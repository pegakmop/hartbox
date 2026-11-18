#!/bin/bash
# Profile-guided build of the emulator: an instrumented build, a training run over the
# workloads in tests/bench/workloads, the merged profile, and an optimized build.
#
#   tools/pgo.sh              -> target/pgo/rvemu
#
# It was worth +1 to 2 % on every workload measured (boot, gcc, sqlite, lzma, python,
# gzip), so it is not part of the default build: the profile is only valid for the compiler
# that produced it, and a checked-in profile would break `cargo build` on other toolchains.
#
# Trains on one hart, then on four harts (MT_WORKLOADS). Needs `rustup component add llvm-tools`
# and a root image that has the tools the workloads use (ROOT_IMG, see tests/bench/run.sh;
# WORKLOADS="boot gz" trains on fewer).
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
out=${PGO_DIR:-$here/target/pgo}
tool=$(find "$(rustc --print sysroot)" -name llvm-profdata | head -1)
[ -x "$tool" ] || { echo "llvm-profdata not found: rustup component add llvm-tools" >&2; exit 1; }
# The flags replace those of .cargo/config.toml, so repeat them. They go in
# CARGO_ENCODED_RUSTFLAGS (separated by 0x1f) rather than RUSTFLAGS, which splits at spaces:
# a path such as "/Volumes/HP P900/..." would break the profile options.
sep=$(printf '\037')
flags="-C${sep}llvm-args=-align-all-functions=6"
[ "$(uname -sm)" = "Darwin arm64" ] && flags="$flags${sep}-C${sep}target-cpu=apple-m1"
rm -rf "$out/raw"; mkdir -p "$out/raw"
cd "$here"
CARGO_ENCODED_RUSTFLAGS="$flags${sep}-C${sep}profile-generate=$out/raw" CARGO_TARGET_DIR="$out/gen" cargo build --release
# One hart (the engine comparisons) ...
for w in ${WORKLOADS:-boot gz sqlite py lzma gcc}; do
  echo "training: $w"
  "$here/tests/bench/run.sh" "$out/gen/release/rvemu" "$(cat "$here/tests/bench/workloads/$w.sh")" >/dev/null 2>&1 || true
done
# ... and the default, one host thread per hart: the multi-hart paths (atomics, fences, the
# idle loop) would otherwise count as never run and be optimized for size.
for w in ${MT_WORKLOADS:-boot par}; do
  echo "training (4 harts): $w"
  SMP=4 "$here/tests/bench/run.sh" "$out/gen/release/rvemu" "$(cat "$here/tests/bench/workloads/$w.sh")" >/dev/null 2>&1 || true
done
"$tool" merge -o "$out/rvemu.profdata" "$out"/raw/*.profraw
CARGO_ENCODED_RUSTFLAGS="$flags${sep}-C${sep}profile-use=$out/rvemu.profdata" CARGO_TARGET_DIR="$out/use" cargo build --release
cp "$out/use/release/rvemu" "$out/rvemu"
echo "built $out/rvemu"
