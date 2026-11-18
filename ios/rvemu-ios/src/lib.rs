//! C ABI around `rvemu` for the iOS app.
//!
//! The Swift side loads the kernel and root filesystem into memory, calls
//! `rvemu_ios_start`, and then polls `rvemu_ios_read` for console output while
//! feeding keystrokes back with `rvemu_ios_write`. The machine runs on its own
//! thread so the UI never blocks.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use std::ffi::CStr;
use std::path::PathBuf;

use std::net::Ipv4Addr;

use rvemu::initramfs::{build_rootfs, InitOptions, RootfsOptions, EXT4, VIRTIO_BLK, VIRTIO_CORE, VIRTIO_NET, VIRTIO_RNG};
use rvemu::machine::{DiskConfig, MachineConfig, IDLE_NS_HINT, INSNS_HINT};
use rvemu::Machine;

/// Kernel command line: quiet, and skip two initcalls the guest never needs.
/// `jent_mod_init` self-tests the jitter entropy RNG (about a second of
/// emulated time), and the guest mounts no f2fs filesystem.
const BOOTARGS: &str = "console=ttyS0 panic=-1 quiet loglevel=2 initcall_blacklist=jent_mod_init,init_f2fs_fs";

/// Cap on console bytes held for the UI, so a chatty guest cannot grow the
/// process without bound. Oldest bytes are dropped first.
const MAX_PENDING_OUTPUT: usize = 1 << 20;

/// Console size the app's terminal emulates; the guest tty is set to match so
/// its line wrapping agrees with the screen.
const CONSOLE_COLS: u16 = 64;
const CONSOLE_ROWS: u16 = 30;

/// Size of the root disk image created on first boot. The guest keeps its
/// files there, so they survive a reboot.
const ROOT_DISK_SIZE: u64 = 4 << 30;

struct Shared {
    out: Mutex<Vec<u8>>,
    input: Mutex<Option<Sender<Vec<u8>>>>,
    running: AtomicBool,
    exit_code: AtomicU64,
    error: Mutex<Option<String>>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

fn shared() -> &'static Shared {
    SHARED.get_or_init(|| Shared {
        out: Mutex::new(Vec::new()),
        input: Mutex::new(None),
        running: AtomicBool::new(false),
        exit_code: AtomicU64::new(0),
        error: Mutex::new(None),
    })
}

/// Destination for guest console output: a buffer the UI drains.
struct OutSink;

impl Write for OutSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let s = shared();
        if let Ok(mut q) = s.out.lock() {
            q.extend_from_slice(buf);
            if q.len() > MAX_PENDING_OUTPUT {
                let extra = q.len() - MAX_PENDING_OUTPUT;
                q.drain(..extra);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn set_error(msg: String) {
    if let Ok(mut e) = shared().error.lock() {
        *e = Some(msg);
    }
}

/// Let other devices reach the guest: listen on every network interface at
/// `host_port` and forward connections to `guest_port` in the guest (the app
/// uses 2222 -> 22 for ssh). Replaces an earlier call; may be called before or
/// while a machine runs. Returns 0 on success, -1 if the port cannot be bound.
#[no_mangle]
pub extern "C" fn rvemu_ios_expose(host_port: u16, guest_port: u16) -> i32 {
    match rvemu::net::expose(Ipv4Addr::UNSPECIFIED, host_port, guest_port) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Stop listening on the port opened by `rvemu_ios_expose`.
#[no_mangle]
pub extern "C" fn rvemu_ios_unexpose() {
    rvemu::net::unexpose();
}

/// Boot a machine. `kernel` is a gzip-compressed or raw RISC-V Image, `rootfs`
/// is an Alpine minirootfs `.tar.gz`; both are copied. `overlay` is a directory
/// merged into the initramfs, with kernel modules under `lib/rvemu`, or NULL.
/// The root filesystem is unpacked into a memory initramfs, so nothing touches
/// the device storage.
///
/// The guest gets virtio-net and virtio-rng and the gateway addressing used by
/// the `--net` command line option, so the shell can resolve and fetch.
///
/// Returns 0 on success, -1 on bad arguments, -2 if a machine is already
/// running, -3 if the assets could not be prepared.
///
/// # Safety
/// `kernel`/`rootfs` must point to `kernel_len`/`rootfs_len` readable bytes, and
/// `overlay` must be NULL or a NUL-terminated path.
#[no_mangle]
pub unsafe extern "C" fn rvemu_ios_start(
    kernel: *const u8,
    kernel_len: usize,
    rootfs: *const u8,
    rootfs_len: usize,
    overlay: *const std::os::raw::c_char,
    disk: *const std::os::raw::c_char,
    omit_rootfs: i32,
    ram_mb: u32,
) -> i32 {
    if kernel.is_null() || rootfs.is_null() || kernel_len == 0 || rootfs_len == 0 {
        return -1;
    }
    let s = shared();
    if s.running.load(Ordering::Acquire) {
        return -2;
    }
    if let Ok(mut e) = s.error.lock() {
        *e = None;
    }
    if let Ok(mut q) = s.out.lock() {
        q.clear();
    }

    let kernel = std::slice::from_raw_parts(kernel, kernel_len).to_vec();
    let rootfs = std::slice::from_raw_parts(rootfs, rootfs_len).to_vec();
    let overlays: Vec<PathBuf> = if overlay.is_null() {
        Vec::new()
    } else {
        match CStr::from_ptr(overlay).to_str() {
            Ok(path) if !path.is_empty() => vec![PathBuf::from(path)],
            Ok(_) => Vec::new(),
            Err(_) => return -1,
        }
    };
    // A NUL-terminated path for the guest's root filesystem image. It is
    // created and formatted on first boot, then reused.
    let disk: Option<PathBuf> = if disk.is_null() {
        None
    } else {
        match CStr::from_ptr(disk).to_str() {
            Ok(path) if !path.is_empty() => Some(PathBuf::from(path)),
            Ok(_) => None,
            Err(_) => return -1,
        }
    };

    let (tx, rx) = channel::<Vec<u8>>();
    *s.input.lock().unwrap() = Some(tx.clone());
    s.exit_code.store(0, Ordering::Release);
    INSNS_HINT.store(0, Ordering::Relaxed);
    IDLE_NS_HINT.store(0, Ordering::Relaxed);

    let result = std::thread::Builder::new()
        .name("rvemu".into())
        // The block engine chains through nested calls; this is its stack.
        .stack_size(32 << 20)
        .spawn(move || {
            // iOS: run on the performance cores rather than defaulting to
            // whatever the system picks for a background thread.
            #[cfg(target_os = "ios")]
            {
                extern "C" {
                    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
                }
                const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
                // SAFETY: sets this thread's own QoS class.
                unsafe {
                    pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
                }
            }
            let kernel = match rvemu::loader::maybe_gunzip(kernel) {
                Ok(k) => k,
                Err(e) => {
                    set_error(format!("kernel: {e}"));
                    s.exit_code.store(3, Ordering::Release);
                    s.running.store(false, Ordering::Release);
                    return;
                }
            };
            let mut modules = VIRTIO_CORE.to_vec();
            if disk.is_some() {
                modules.extend_from_slice(VIRTIO_BLK);
                modules.extend_from_slice(EXT4);
            }
            // Network and entropy come up in the session's background, so the
            // prompt does not wait for them.
            let mut late_modules = VIRTIO_NET.to_vec();
            late_modules.extend_from_slice(VIRTIO_RNG);
            // An already-populated root disk does not need the root filesystem
            // in the initramfs: leaving it out skips the kernel's unpack of a
            // whole rootfs and boots straight from the disk.
            let omit_rootfs = omit_rootfs != 0 && disk.as_deref().is_some_and(rvemu::ext2::has_populated_root);
            let opts = RootfsOptions {
                init: None,
                init_options: InitOptions {
                    modules: modules.clone(),
                    late_modules: late_modules.clone(),
                    persistent_root: disk.is_some(),
                    network: true,
                    network_ipv6: true,
                    console_size: Some((CONSOLE_COLS, CONSOLE_ROWS)),
                    omit_rootfs,
                },
                overlays: overlays.clone(),
            };
            // Packing the initramfs (gunzip, tar walk, cpio write) costs a few
            // tenths of a second of host time. Keep it beside the root disk so
            // only the first boot after an asset change pays for it.
            let cache_path = disk
                .as_ref()
                .and_then(|d| d.parent())
                .map(|dir| {
                    let key = initramfs_key(&rootfs, &overlays, &modules)
                        ^ initramfs_key(&rootfs, &[], &late_modules) ^ (omit_rootfs as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                    dir.join(format!("initramfs-{key:016x}.cpio"))
                });
            let initrd = match cache_path.as_ref().and_then(|p| std::fs::read(p).ok()) {
                Some(cached) => cached,
                None => match build_rootfs(&rootfs, &opts) {
                    Ok(cpio) => {
                        if let Some(path) = &cache_path {
                            prune_initramfs_cache(path);
                            let _ = std::fs::write(path, &cpio);
                        }
                        cpio
                    }
                    Err(e) => {
                        set_error(format!("rootfs: {e}"));
                        s.exit_code.store(3, Ordering::Release);
                        s.running.store(false, Ordering::Release);
                        return;
                    }
                },
            };
            let mut disks = Vec::new();
            if let Some(path) = &disk {
                if !path.exists() {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if let Err(e) = rvemu::ext2::format(path, ROOT_DISK_SIZE, "rvemu-root") {
                        set_error(format!("root disk: {e}"));
                        s.exit_code.store(3, Ordering::Release);
                        s.running.store(false, Ordering::Release);
                        return;
                    }
                }
                disks.push(DiskConfig { path: path.clone(), read_only: false });
            }
            let cfg = MachineConfig {
                ram_size: (ram_mb as u64) << 20,
                harts: 1,
                bootargs: BOOTARGS.to_string(),
                firmware: None,
                kernel: Some(kernel),
                initrd: Some(initrd),
                program: None,
                // Never warp: guest timeouts (DNS, ping, TCP) must elapse in
                // real time or they fire before the host has replied.
                warp: false,
                real_time: false,
                max_insns: None,
                disks,
                net: true,
                ipv6: true,
                rng: true,
                forwards: Vec::new(),
                // One hart, as before: `harts` above and this flag are the place to try
                // one host thread per hart on the phone.
                threads: false,
            };
            let mut machine = match Machine::new(cfg, Box::new(OutSink), Some(rx)) {
                Ok(m) => m,
                Err(e) => {
                    set_error(e);
                    s.exit_code.store(3, Ordering::Release);
                    s.running.store(false, Ordering::Release);
                    return;
                }
            };
            let exit = machine.run();
            let code = match exit {
                rvemu::Exit::Shutdown(c) => c as u64,
                rvemu::Exit::Limit | rvemu::Exit::OutputClosed | rvemu::Exit::Reboot => 0,
                rvemu::Exit::Halt => 1,
            };
            s.exit_code.store(code, Ordering::Release);
            s.running.store(false, Ordering::Release);
        });

    match result {
        Ok(_) => {
            s.running.store(true, Ordering::Release);
            0
        }
        Err(e) => {
            set_error(format!("thread: {e}"));
            -3
        }
    }
}

/// Drain up to `max` console bytes into `buf`. Returns the number written.
///
/// # Safety
/// `buf` must point to `max` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn rvemu_ios_read(buf: *mut u8, max: usize) -> usize {
    if buf.is_null() || max == 0 {
        return 0;
    }
    let s = shared();
    let Ok(mut q) = s.out.lock() else { return 0 };
    let n = q.len().min(max);
    if n != 0 {
        std::ptr::copy_nonoverlapping(q.as_ptr(), buf, n);
        q.drain(..n);
    }
    n
}

/// Feed console input to the guest.
///
/// # Safety
/// `buf` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn rvemu_ios_write(buf: *const u8, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(buf, len).to_vec();
    let s = shared();
    if let Ok(guard) = s.input.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(bytes);
        }
    }
}

/// Ask the machine to stop at the next quantum boundary.
#[no_mangle]
pub extern "C" fn rvemu_ios_stop() {
    rvemu::machine::STOP_HINT.store(true, Ordering::Relaxed);
}

/// Whether a machine is running.
#[no_mangle]
pub extern "C" fn rvemu_ios_running() -> i32 {
    shared().running.load(Ordering::Acquire) as i32
}

/// Exit code of the last machine (0 while it runs).
#[no_mangle]
pub extern "C" fn rvemu_ios_exit_code() -> i32 {
    shared().exit_code.load(Ordering::Acquire) as i32
}

/// Instructions retired so far, across all harts.
#[no_mangle]
pub extern "C" fn rvemu_ios_insns() -> u64 {
    INSNS_HINT.load(Ordering::Relaxed)
}

/// Nanoseconds the guest spent idle (waiting on a host read, a timer or input).
/// Subtract it from wall time when computing instructions per second, otherwise
/// a guest that is blocked on storage looks like a slow interpreter.
#[no_mangle]
pub extern "C" fn rvemu_ios_idle_ns() -> u64 {
    IDLE_NS_HINT.load(Ordering::Relaxed)
}

/// Copy the last error message into `buf` (NUL-terminated when it fits) and
/// return its length in bytes.
///
/// # Safety
/// `buf` must point to `max` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn rvemu_ios_last_error(buf: *mut u8, max: usize) -> usize {
    if buf.is_null() || max == 0 {
        return 0;
    }
    let s = shared();
    let Ok(e) = s.error.lock() else { return 0 };
    let Some(msg) = e.as_ref() else { return 0 };
    let n = msg.len().min(max.saturating_sub(1));
    std::ptr::copy_nonoverlapping(msg.as_ptr(), buf, n);
    *buf.add(n) = 0;
    n
}

/// Bumped when the generated `/init` scripts change, so a cache built by an
/// older build is not reused.
const INIT_SCRIPT_VERSION: u64 = 6;

/// FNV-1a over everything that changes what the packed initramfs contains.
fn initramfs_key(rootfs: &[u8], overlays: &[PathBuf], modules: &[&str]) -> u64 {
    fn mix(h: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x100000001b3);
        }
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    mix(&mut h, &INIT_SCRIPT_VERSION.to_le_bytes());
    mix(&mut h, rvemu::initramfs::RSSH.as_bytes());
    mix(&mut h, rvemu::initramfs::SESSION.as_bytes());
    mix(&mut h, rootfs);
    for m in modules {
        mix(&mut h, m.as_bytes());
        mix(&mut h, &[0]);
    }
    for dir in overlays {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("lib/rvemu"))
            .map(|it| it.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        for path in files {
            if let Some(name) = path.file_name() {
                mix(&mut h, name.to_string_lossy().as_bytes());
            }
            if let Ok(data) = std::fs::read(&path) {
                mix(&mut h, &data);
            }
        }
    }
    h
}

/// Drop older initramfs caches so they cannot pile up.
fn prune_initramfs_cache(keep: &std::path::Path) {
    let Some(dir) = keep.parent() else { return };
    let Some(keep_name) = keep.file_name() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name != keep_name && name.to_string_lossy().starts_with("initramfs-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
