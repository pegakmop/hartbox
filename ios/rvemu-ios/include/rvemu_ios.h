// C ABI around rvemu for the iOS app. See rvemu-ios/src/lib.rs.
#ifndef RVEMU_IOS_H
#define RVEMU_IOS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// Boot a machine from an in-memory kernel and rootfs tarball. `overlay_dir`
/// is merged into the initramfs (kernel modules under lib/rvemu) and
/// `root_disk` is a persistent ext4 image, created on first use; both may be
/// NULL. Returns 0 on success, or a negative error code.
int32_t rvemu_ios_start(const uint8_t *kernel, size_t kernel_len,
                        const uint8_t *rootfs, size_t rootfs_len,
                        const char *overlay_dir, const char *root_disk,
                        int32_t omit_rootfs, uint32_t ram_mb);

/// Drain up to `max` console bytes into `buf`. Returns the number written.
size_t rvemu_ios_read(uint8_t *buf, size_t max);

/// Feed console input to the guest.
void rvemu_ios_write(const uint8_t *buf, size_t len);

/// Ask the machine to stop at the next quantum boundary.
void rvemu_ios_stop(void);

/// Whether a machine is running.
int32_t rvemu_ios_running(void);

/// Exit code of the last machine (0 while it runs or after a clean poweroff).
int32_t rvemu_ios_exit_code(void);

/// Instructions retired so far, across all harts.
uint64_t rvemu_ios_insns(void);

/** Nanoseconds the guest spent idle, to subtract from wall time when
 *  reporting instructions per second. */
uint64_t rvemu_ios_idle_ns(void);

/// Listen on every network interface at `host_port` and forward connections to
/// `guest_port` in the guest (ssh uses 2222 -> 22). Replaces an earlier call.
/// Returns 0, or -1 if the port cannot be bound.
int32_t rvemu_ios_expose(uint16_t host_port, uint16_t guest_port);

/// Stop listening on the port opened by `rvemu_ios_expose`.
void rvemu_ios_unexpose(void);

/// Copy the last error message into `buf` and return its length.
size_t rvemu_ios_last_error(uint8_t *buf, size_t max);

#ifdef __cplusplus
}
#endif

#endif
