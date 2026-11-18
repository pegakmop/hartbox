#!/bin/sh
# Differential ISA test.
#
# Builds test.c for rv64gc (bare metal), runs it on rvemu and, when
# qemu-system-riscv64 is installed, on QEMU's `virt` machine, then compares
# the two transcripts. The program covers integer and word operations,
# division edge cases, M-mode traps, atomics, and single/double FP with all
# five rounding modes and fflags.
#
# Known, deliberate differences with QEMU (the transcript lines differ):
#   * round-to-nearest-away ties at the underflow boundary: IEEE 754 gives the
#     smallest subnormal, QEMU gives zero.
#   * `misa` reports the H extension in QEMU's default CPU, not here.
#   * NX flags of fused multiply-add when the product underflows.
#
# Usage: tests/isa/run_diff.sh            (needs clang with RISC-V support and ld.lld)
#   CLANG=... LD=... RVEMU=... OUT=... can override the tools.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${OUT:-$here/out}
CLANG=${CLANG:-clang}
LD=${LD:-ld.lld}
RVEMU=${RVEMU:-$here/../../target/release/rvemu}
mkdir -p "$out"

"$CLANG" --target=riscv64-unknown-elf -march=rv64gc -mabi=lp64d -mcmodel=medany \
    -O2 -ffreestanding -fno-builtin -fno-stack-protector -nostdlib \
    -fuse-ld="$LD" -Wl,-T,"$here/link.ld" "$here/test.c" -o "$out/test.elf"

"$RVEMU" --program "$out/test.elf" -m 128M --no-raw > "$out/rvemu.txt"
echo "rvemu: $(wc -l < "$out/rvemu.txt") lines"

if command -v qemu-system-riscv64 >/dev/null 2>&1; then
    qemu-system-riscv64 -machine virt -nographic -m 128M -bios "$out/test.elf" \
        -serial file:"$out/qemu.txt" >/dev/null 2>&1 || true
    if cmp -s "$out/qemu.txt" "$out/rvemu.txt"; then
        echo "identical to QEMU"
    else
        echo "differs from QEMU in $(diff "$out/qemu.txt" "$out/rvemu.txt" | grep -c '^<') lines (see $out)"
    fi
fi
