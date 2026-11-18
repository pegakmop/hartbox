#!/bin/sh
# Harts on separate host threads: boots Alpine with 4 harts (one host thread each) and checks
#   * the guest sees 4 CPUs,
#   * atomics, lr/sc, a spinlock, acquire/release message passing and a store-buffering litmus
#     come out exact (threads/torture.c),
#   * cross-modifying code is seen by the other hart after fence.i (threads/smc.c),
#   * four parallel CPU-bound processes finish with the right answer.
# The guest compiles the C files, so it needs gcc and python3: the script makes a throwaway root image and
# installs build-base and python3 over the network. Set ROOT_IMG to a root image that already has them to skip that.
# Usage: tests/e2e/threads.sh [rootfs.tar.gz]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${WORK:-$here/../../examples/work}
rvemu=${RVEMU:-$here/../../target/release/rvemu}
rootfs=${1:-$work/alpine-minirootfs-3.24.2-riscv64.tar.gz}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
root=${ROOT_IMG:-$out/root.img}
# Sources travel as gzip+base64 text: the console input buffer holds 4096 bytes.
enc() { gzip -9 < "$1" | base64 | tr -d '\n'; }

{
    echo 'echo N_HARTS=$(nproc)'
    [ -n "${ROOT_IMG:-}" ] || echo 'apk add build-base python3 >/dev/null 2>&1; echo APK_DONE'
    echo "echo $(enc "$here/threads/torture.c") | base64 -d | gunzip > /tmp/t.c"
    echo "echo $(enc "$here/threads/smc.c") | base64 -d | gunzip > /tmp/s.c"
    echo 'gcc -O1 -pthread -o /tmp/t /tmp/t.c && /tmp/t'
    echo 'gcc -O1 -pthread -o /tmp/s /tmp/s.c && /tmp/s && /tmp/s'
    echo 'for k in 1 2 3 4; do python3 -c "
x=1
for i in range(300000): x=(x*1103515245+12345)&0xffffffff
print(\"par\",x)" & done; wait; echo PAR_DONE'
    echo 'exit'
} | timeout 600 "$rvemu" --overlay "$work/overlay" --rootfs "$rootfs" -k "$work/vmlinuz-lts" \
    --net --root-disk "$root" --root-size 2G -m 512M --smp 4 --no-raw > "$out/log.txt" 2>&1 || true

fail=0
check() {
    if grep -q "$1" "$out/log.txt"; then echo "ok    $2"; else echo "FAIL  $2 (expected '$1')"; fail=1; fi
}
check '^N_HARTS=4$' 'the guest has 4 CPUs'
check '^TORTURE_OK$' 'atomics, lr/sc, spinlock, message passing and the SB litmus are exact'
check '^smc bad 0 (want 0) SMC_OK$' 'cross-modified code is seen after fence.i'
[ "$(grep -c '^par ' "$out/log.txt")" = 4 ] && echo "ok    four parallel processes computed the same answer" || { echo "FAIL  parallel processes"; fail=1; }
check '^PAR_DONE$' 'the parallel run finished'
[ "$fail" = 0 ] || { echo "--- tail of the guest log:"; tail -30 "$out/log.txt"; }
exit "$fail"
