#!/bin/sh
# End-to-end smoke test: boots Alpine on rvemu with a persistent root disk, a
# virtio data disk, the entropy device, networking and the RTC, runs guest
# commands, and checks their output. A second boot on the same root disk checks
# that files survive a reboot.
#
# Needs the Alpine assets prepared by examples/boot-alpine.sh (run it once first).
# Usage: tests/e2e/smoke.sh [rootfs.tar.gz]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${WORK:-$here/../../examples/work}
rvemu=${RVEMU:-$here/../../target/release/rvemu}
rootfs=${1:-$work/alpine-minirootfs-3.24.2-riscv64.tar.gz}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

# A 16 MiB data disk whose first sector starts with a marker. The root disk is
# a new image in the temporary directory, so the run never touches ~/.rvemu.
printf 'rvemu-smoke-marker' > "$out/disk.img"
dd if=/dev/zero bs=1048576 count=16 2>/dev/null | tail -c +1 >> "$out/disk.img"

{
    sleep 2
    echo 'dd if=/dev/vdb bs=18 count=1 2>/dev/null; echo'
    echo 'dd if=/dev/hwrng bs=8 count=1 2>/dev/null | wc -c'
    echo 'date +%Y'
    echo 'mount | grep " on / type " | sed "s/ (.*//"'
    echo 'ip -6 addr show dev eth0 scope global | grep -o "fd00:2::15/64"'
    echo 'ping -6 -c 1 -W 3 fd00:2::2 | grep -q "1 packets received" && echo PING6_OK'
    echo 'nslookup example.com fd00:2::3 2>/dev/null | grep -c "Address: 2606:"'
    echo 'echo persisted > /root/persist-check; sync'
    echo 'wget -q -O- http://example.com | grep -c "Example Domain"'
    echo 'wget -q -O /dev/null https://example.com && echo HTTPS_OK'
    echo 'n=0; for i in 1 2 3 4 5; do wget -q -O /dev/null http://example.com && n=$((n+1)); done; echo LOOP_$n'
    echo 'ping -c 2 -W 3 1.1.1.1 | grep -q "2 packets received" && echo PING_OK'
    echo 'exit'
    sleep 60
} | timeout 240 "$rvemu" --overlay "$work/overlay" --rootfs "$rootfs" -k "$work/vmlinuz-lts" \
    --net --root-disk "$out/root.img" --root-size 1G --disk "$out/disk.img" -m 512M --no-raw \
    > "$out/log.txt" 2>&1 || true

{
    sleep 2
    echo 'cat /root/persist-check'
    echo 'exit'
    sleep 30
} | timeout 120 "$rvemu" --overlay "$work/overlay" --rootfs "$rootfs" -k "$work/vmlinuz-lts" \
    --net --root-disk "$out/root.img" -m 512M --no-raw > "$out/log2.txt" 2>&1 || true

fail=0
check() {
    if grep -q "$1" "$2"; then
        echo "ok    $3"
    else
        echo "FAIL  $3 (expected '$1')"
        fail=1
    fi
}
check '^rvemu-smoke-marker$' "$out/log.txt" 'virtio data disk: first sector reads back'
check '^8$' "$out/log.txt" 'entropy device: 8 random bytes'
check '^20[0-9][0-9]$' "$out/log.txt" 'RTC: a plausible current year'
check 'on / type ext4' "$out/log.txt" 'persistent root: / is an ext4 disk'
check 'fd00:2::15/64' "$out/log.txt" 'IPv6: the guest has its global address'
check 'PING6_OK' "$out/log.txt" 'IPv6: the gateway answers ping6'
check '^[1-9][0-9]*$' "$out/log.txt" 'IPv6 DNS: example.com resolves to IPv6 addresses through the gateway'
check '^1$' "$out/log.txt" 'HTTP: example.com page fetched through the gateway'
check 'HTTPS_OK' "$out/log.txt" 'HTTPS: TLS fetch with the CA bundle'
check 'LOOP_5' "$out/log.txt" 'five sequential downloads complete'
check 'PING_OK' "$out/log.txt" 'ICMP: echo to an external address through the gateway'
check '^persisted$' "$out/log2.txt" 'persistence: a file written in one boot is there after a reboot'
check 'guest powered off\|reboot: Power down' "$out/log.txt" 'guest powers off cleanly'
exit $fail
