#!/bin/sh
# Boot Alpine Linux (riscv64) on rvemu, with networking and virtio devices.
#
# On first use this fetches the Alpine riscv64 kernel, the initramfs that
# carries the virtio drivers, and the CA certificate bundle from the Alpine
# mirror, and builds an overlay directory from them. Later runs reuse it.
#
#   examples/boot-alpine.sh                           # network + IPv6, persistent root, quiet boot
#   examples/boot-alpine.sh --disk disk.img -v        # add a data disk (/dev/vdb), verbose boot
#   examples/boot-alpine.sh --ephemeral               # keep the root in memory instead
#   ROOTFS=/path/to/rootfs.tar.gz examples/boot-alpine.sh --smp 4
#
# The persistent root is ~/.rvemu/root.img, created on the first run. Use
# --root-disk FILE to choose another image. Extra arguments are passed through to rvemu.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=${WORK:-$here/work}
release=${ALPINE_RELEASE:-v3.24}
version=${ALPINE_VERSION:-3.24.2}
kver=${KERNEL_VERSION:-6.18.52-0-lts}
mirror=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
rvemu=${RVEMU:-$here/../target/release/rvemu}
rootfs=${ROOTFS:-$work/alpine-minirootfs-$version-riscv64.tar.gz}
bundle=$work/alpine-uboot-$version-riscv64.tar.gz
overlay=$work/overlay
mkdir -p "$work"

if [ ! -f "$rootfs" ]; then
    curl -fL -o "$rootfs" "$mirror/$release/releases/riscv64/alpine-minirootfs-$version-riscv64.tar.gz"
fi

if [ ! -f "$work/vmlinuz-lts" ] || [ ! -d "$overlay/lib/rvemu" ]; then
    [ -f "$bundle" ] || curl -fL -o "$bundle" "$mirror/$release/releases/riscv64/alpine-uboot-$version-riscv64.tar.gz"
    rm -rf "$work/extract" && mkdir -p "$work/extract" "$overlay/lib/rvemu" "$overlay/etc/ssl/certs"
    # Kernel, and the initramfs whose module tree holds the virtio drivers.
    tar -xzf "$bundle" -C "$work/extract" ./boot/vmlinuz-lts ./boot/initramfs-lts ./apks/riscv64
    mv "$work/extract/boot/vmlinuz-lts" "$work/vmlinuz-lts"
    (cd "$work/extract" && gzip -dc boot/initramfs-lts | cpio -idm 2>/dev/null)
    for m in virtio_ring virtio virtio_mmio virtio-rng virtio_blk failover net_failover virtio_net \
             crc16 mbcache jbd2 ext4 fat vfat nls_utf8 nls_cp437 nls_ucs2_utils; do
        find "$work/extract/usr/lib/modules/$kver" -name "$m.ko" -exec cp {} "$overlay/lib/rvemu/" \;
    done
    # Helper: `rvemu-mount` loads the filesystem drivers and mounts the disk.
    mkdir -p "$overlay/usr/local/bin"
    cat > "$overlay/usr/local/bin/rvemu-mount" <<'MOUNT'
#!/bin/sh
# Mount the first virtio disk (/dev/vda, vfat or ext4) at /mnt/disk.
for m in nls_ucs2_utils nls_utf8 nls_cp437 fat vfat crc16 mbcache jbd2 ext4; do
    [ -f "/lib/rvemu/$m.ko" ] && insmod "/lib/rvemu/$m.ko" >/dev/null 2>&1
done
mkdir -p /mnt/disk && mount /dev/vda /mnt/disk
MOUNT
    chmod 755 "$overlay/usr/local/bin/rvemu-mount"
    # CA certificates, so `wget https://...` works.
    apk=$(ls "$work"/extract/apks/riscv64/ca-certificates-bundle-*.apk 2>/dev/null | head -1)
    if [ -n "$apk" ]; then
        tar -xzf "$apk" -C "$work/extract" ./etc/ssl/certs/ca-certificates.crt 2>/dev/null || true
        cp "$work/extract/etc/ssl/certs/ca-certificates.crt" "$overlay/etc/ssl/certs/" 2>/dev/null || true
    fi
    rm -rf "$work/extract"
fi

exec "$rvemu" -k "$work/vmlinuz-lts" --rootfs "$rootfs" --overlay "$overlay" --net -m 512M "$@"
