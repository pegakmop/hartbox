//! `rvemu` command-line interface.

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, ValueEnum};

use rvemu::console::{self, TerminalGuard};
use rvemu::ext2;
use rvemu::initramfs::{build_rootfs, InitOptions, RootfsOptions, EXT4, VIRTIO_BLK, VIRTIO_CORE, VIRTIO_NET, VIRTIO_RNG};
use rvemu::machine::DiskConfig;
use rvemu::net::PortForward;
use rvemu::{Exit, Machine, MachineConfig};

/// Quiet boot: the guest's kernel reports only errors. Two initcalls are skipped:
/// `jent_mod_init` self-tests the jitter-entropy RNG, which takes about a second of
/// emulated time (the kernel is seeded from virtio-rng, and `/dev/random` is unchanged),
/// and `init_f2fs_fs` registers a filesystem the guest never mounts.
const QUIET_APPEND: &str = "console=ttyS0 panic=-1 quiet loglevel=2 initcall_blacklist=jent_mod_init,init_f2fs_fs";
/// Verbose boot: the full kernel log.
const VERBOSE_APPEND: &str = "console=ttyS0 earlycon=sbi panic=-1";

/// Fast RISC-V (RV64GC) full-system emulator. Boots Linux, and Alpine Linux
/// from its minirootfs tarball, with virtio disks, networking and an RTC.
/// Source of guest time (see `--clock`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ClockMode {
    Real,
    Virtual,
}

#[derive(Parser, Debug)]
#[command(name = "rvemu", version, about, long_about = None)]
struct Cli {
    /// Linux kernel: gzip-compressed or raw RISC-V Image, or an ELF
    #[arg(short = 'k', long, value_name = "FILE")]
    kernel: Option<PathBuf>,

    /// M-mode firmware such as OpenSBI fw_dynamic. Without it, SBI calls are served by the emulator
    #[arg(long, value_name = "FILE")]
    bios: Option<PathBuf>,

    /// Initial RAM disk in cpio format (plain or gzip-compressed)
    #[arg(short = 'i', long, value_name = "FILE", conflicts_with_all = ["rootfs", "overlay"])]
    initrd: Option<PathBuf>,

    /// Root filesystem tarball (.tar.gz), e.g. Alpine minirootfs. It is packed into an initramfs in memory
    #[arg(long, value_name = "FILE")]
    rootfs: Option<PathBuf>,

    /// Program to run as PID 1 inside the rootfs. Default: a built-in /init that starts a shell
    #[arg(long, value_name = "PATH", requires = "rootfs")]
    init: Option<String>,

    /// Directory added on top of the rootfs (repeatable). Put kernel modules in
    /// lib/rvemu/ to load virtio drivers, and certificates in etc/ssl/certs/
    #[arg(long, value_name = "DIR", requires = "rootfs")]
    overlay: Vec<PathBuf>,

    /// Attach a raw disk image as a virtio-blk device (/dev/vda, /dev/vdb, ...).
    /// Append ",ro" for read-only
    #[arg(long = "disk", value_name = "FILE[,ro]", value_parser = parse_disk)]
    disks: Vec<DiskConfig>,

    /// Root filesystem on a persistent disk image, which is /dev/vda. The image is
    /// created on first use. Default: ~/.rvemu/root.img. Needs --rootfs
    #[arg(long, value_name = "FILE", conflicts_with = "ephemeral")]
    root_disk: Option<PathBuf>,

    /// Size of a root disk image when it is created
    #[arg(long, value_name = "SIZE", default_value = "8G", value_parser = parse_size)]
    root_size: u64,

    /// Keep the root filesystem in memory: nothing is saved between runs
    #[arg(long)]
    ephemeral: bool,

    /// Do not configure IPv6 on the guest network (IPv6 is on by default with --net)
    #[arg(long)]
    no_ipv6: bool,

    /// Attach a virtio network card connected to the host through user-mode NAT.
    /// The guest uses 10.0.2.15, the gateway is 10.0.2.2 and DNS is 10.0.2.3
    #[arg(long)]
    net: bool,

    /// Use an explicit IPv4 or IPv6 DNS resolver for the guest
    #[arg(long, requires = "net")]
    dns_server: Option<std::net::IpAddr>,

    /// Do not attach the virtio entropy source (/dev/hwrng)
    #[arg(long)]
    no_rng: bool,

    /// Forward host port HOST to guest port GUEST (repeatable), e.g. 2222:22.
    /// The guest is reached on 127.0.0.1:HOST; with BIND, such as 0.0.0.0:2222:22,
    /// on that address instead (0.0.0.0 is reachable from the network). Requires --net
    #[arg(long = "forward", value_name = "[BIND:]HOST:GUEST", value_parser = parse_forward)]
    forwards: Vec<PortForward>,

    /// Kernel command line (default: a quiet boot, or the full log with -v)
    #[arg(short = 'a', long, value_name = "ARGS")]
    append: Option<String>,

    /// Guest RAM size, e.g. 256M or 1G
    #[arg(short = 'm', long, value_name = "SIZE", default_value = "512M", value_parser = parse_size)]
    mem: u64,

    /// Number of harts (virtual CPUs); each runs on its own host thread. The default is 1
    /// (the deterministic single-threaded engine); `auto` is the number of fast cores of this
    /// machine, which runs `7z b`, `make -j` and parallel jobs in parallel
    #[arg(short = 's', long, value_name = "N", value_parser = parse_smp)]
    smp: Option<usize>,

    /// With more than one hart, run all of them on one host thread instead of one thread
    /// each (slower, deterministic; guest time then follows `--clock`)
    #[arg(long)]
    single_thread: bool,

    /// Bare-metal ELF or raw binary to run in M-mode, instead of a kernel
    #[arg(long, value_name = "FILE")]
    program: Option<PathBuf>,

    /// Show the full kernel log and emulator status messages
    #[arg(short = 'v', long)]
    verbose: bool,

    /// When all harts are idle, jump straight to the next timer instead of sleeping
    #[arg(long)]
    warp: bool,

    /// Guest clock: `virtual` counts one tick per 16 retired instructions (deterministic,
    /// but a guest benchmark such as 7z b then reads a fixed ~160 MIPS whatever the host
    /// does); `real` follows the host clock, so guest benchmarks report the real speed
    #[arg(long, value_enum, default_value_t = ClockMode::Virtual)]
    clock: ClockMode,

    /// Stop after this many instructions
    #[arg(long, value_name = "N")]
    max_insns: Option<u64>,

    /// Print instruction count and speed on exit
    #[arg(long)]
    stats: bool,

    /// Assume the root disk is already populated and boot straight from it,
    /// leaving the root filesystem out of the initramfs
    #[arg(long)]
    minimal_initramfs: bool,

    /// Include the full initramfs for recovery or benchmark comparisons
    #[arg(long, conflicts_with = "minimal_initramfs")]
    full_initramfs: bool,

    /// Keep the host terminal in cooked mode
    #[arg(long)]
    no_raw: bool,

    /// Write the device tree the machine boots with to FILE and exit
    #[arg(long, value_name = "FILE")]
    dump_dtb: Option<PathBuf>,
}

/// Parse the hart count: a number, or `auto` for the fast cores of this machine.
fn parse_smp(s: &str) -> Result<usize, String> {
    if s.trim().eq_ignore_ascii_case("auto") {
        return Ok(fast_cores());
    }
    match s.trim().parse::<usize>() {
        Ok(n) if (1..=64).contains(&n) => Ok(n),
        _ => Err("the number of harts must be 1 to 64, or `auto`".into()),
    }
}

/// Cores that run the emulator at full speed: the performance cores of an Apple silicon
/// Mac (the efficiency cores are slower, and one hart on each would only wait for the
/// others), else every logical CPU.
fn fast_cores() -> usize {
    #[cfg(target_os = "macos")]
    {
        let mut n: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        // SAFETY: a documented sysctl that fills one int.
        let rc = unsafe {
            libc::sysctlbyname(
                c"hw.perflevel0.logicalcpu".as_ptr(),
                &mut n as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && n > 0 {
            return n as usize;
        }
    }
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(64)
}

/// Parse sizes such as `512M`, `2G`, `64k` or a plain byte count.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1u64 << 20),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1u64 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().map(|v| v * mult).map_err(|e| format!("invalid size '{s}': {e}"))
}

/// Parse `HOST:GUEST` port numbers.
fn parse_forward(s: &str) -> Result<PortForward, String> {
    let bad = || format!("expected [BIND:]HOST:GUEST, got '{s}'");
    let port = |p: &str| p.parse::<u16>().map_err(|e| format!("invalid port '{p}': {e}"));
    let parts: Vec<&str> = s.split(':').collect();
    let (bind, host, guest) = match parts[..] {
        [host, guest] => (Ipv4Addr::LOCALHOST, host, guest),
        [bind, host, guest] => (bind.parse::<Ipv4Addr>().map_err(|e| format!("invalid address '{bind}': {e}"))?, host, guest),
        _ => return Err(bad()),
    };
    Ok(PortForward { bind, host_port: port(host)?, guest_port: port(guest)? })
}

/// Parse `FILE[,ro]`.
fn parse_disk(s: &str) -> Result<DiskConfig, String> {
    match s.strip_suffix(",ro") {
        Some(path) => Ok(DiskConfig { path: PathBuf::from(path), read_only: true }),
        None => Ok(DiskConfig { path: PathBuf::from(s), read_only: false }),
    }
}

/// The virtio kernel modules this run needs, in load order.
fn virtio_modules(cli: &Cli, persistent: bool) -> Vec<&'static str> {
    let mut modules: Vec<&'static str> = VIRTIO_CORE.to_vec();
    if !cli.no_rng {
        modules.extend_from_slice(VIRTIO_RNG);
    }
    if persistent {
        modules.extend_from_slice(VIRTIO_BLK);
        modules.extend_from_slice(EXT4);
    } else if !cli.disks.is_empty() {
        modules.extend_from_slice(VIRTIO_BLK);
    }
    if cli.net {
        modules.extend_from_slice(VIRTIO_NET);
    }
    modules
}

/// The root disk image used when none is given: ~/.rvemu/root.img.
fn default_root_disk() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set; pass --root-disk FILE or --ephemeral")?;
    Ok(PathBuf::from(home).join(".rvemu").join("root.img"))
}

/// The root disk image, created if it does not exist yet.
fn prepare_root_disk(cli: &Cli) -> Result<PathBuf, String> {
    let path = match &cli.root_disk {
        Some(path) => path.clone(),
        None => default_root_disk()?,
    };
    if !path.exists() {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        ext2::format(&path, cli.root_size, "rvemu-root")
            .map_err(|e| format!("cannot create root disk {}: {e}", path.display()))?;
        eprintln!("[rvemu] created root disk {} ({} GiB)", path.display(), cli.root_size >> 30);
    }
    Ok(path)
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Print an emulator status line, only in verbose mode.
macro_rules! status {
    ($cli:expr, $($arg:tt)*) => {
        if $cli.verbose {
            eprintln!("[rvemu] {}", format_args!($($arg)*));
        }
    };
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    // Several harts cost nothing on a one-process guest (boot and a gcc run are as fast as
    // with one) and run `7z b`, `make -j` and parallel jobs on all fast cores.
    let smp = cli.smp.unwrap_or(1);
    if let Some(server) = cli.dns_server {
        rvemu::net::set_dns_override(server);
    }
    if !cli.forwards.is_empty() && !cli.net {
        return Err("--forward needs --net".into());
    }
    if cli.root_disk.is_some() && cli.rootfs.is_none() {
        return Err("--root-disk needs --rootfs".into());
    }
    // The persistent root is set up by the generated /init, so it needs --rootfs
    // and not a custom --init.
    let persistent = cli.rootfs.is_some() && cli.init.is_none() && !cli.ephemeral;
    let setup_start = Instant::now();
    let mut disks = Vec::new();
    let mut minimal_initramfs = false;
    if persistent {
        let path = prepare_root_disk(&cli)?;
        minimal_initramfs = cli.minimal_initramfs || (!cli.full_initramfs
            && !cli.overlay.iter().any(|overlay| overlay.join("init").exists())
            && ext2::has_populated_root(&path));
        status!(cli, "root filesystem on {}", path.display());
        disks.push(DiskConfig { path, read_only: false });
    }
    disks.extend(cli.disks.iter().cloned());
    let t_disk = setup_start.elapsed();
    // The kernel is read and decompressed on another thread while the rootfs is packed.
    let kernel_job = cli.kernel.clone().map(|path| {
        std::thread::spawn(move || read_file(&path).and_then(rvemu::loader::maybe_gunzip))
    });
    let firmware = cli.bios.as_deref().map(read_file).transpose()?;
    let program = cli.program.as_deref().map(read_file).transpose()?;
    let initrd = match (&cli.rootfs, &cli.initrd) {
        (Some(path), _) => {
            let tgz = read_file(path)?;
            // With a prepared root disk the initramfs is minimal, and the
            // network stack is loaded by the session in the background, so the
            // prompt does not wait for it.
            let all = virtio_modules(&cli, persistent);
            let (modules, late_modules) = if minimal_initramfs {
                let (late, early): (Vec<_>, Vec<_>) = all
                    .into_iter()
                    .partition(|m| VIRTIO_NET.contains(m) || VIRTIO_RNG.contains(m));
                (early, late)
            } else {
                (all, Vec::new())
            };
            let opts = RootfsOptions {
                init: cli.init.clone(),
                init_options: InitOptions {
                    modules,
                    late_modules: late_modules.into_iter().collect(),
                    network: cli.net,
                    network_ipv6: cli.net && !cli.no_ipv6,
                    persistent_root: persistent,
                    console_size: None,
                    omit_rootfs: minimal_initramfs,
                },
                overlays: cli.overlay.clone(),
            };
            let cpio = build_rootfs(&tgz, &opts).map_err(|e| format!("cannot read rootfs {}: {e}", path.display()))?;
            Some(cpio)
        }
        (None, Some(path)) => Some(read_file(path)?),
        _ => None,
    };
    let t_rootfs = setup_start.elapsed();
    let kernel = match kernel_job {
        Some(job) => Some(job.join().map_err(|_| "kernel loader thread panicked".to_string())??),
        None => None,
    };

    let t_kernel = setup_start.elapsed();
    let append = cli.append.clone().unwrap_or_else(|| {
        if cli.verbose {
            VERBOSE_APPEND.to_string()
        } else {
            QUIET_APPEND.to_string()
        }
    });
    let config = MachineConfig {
        ram_size: cli.mem,
        harts: smp,
        bootargs: append,
        firmware,
        kernel,
        initrd,
        program,
        warp: cli.warp,
        real_time: cli.clock == ClockMode::Real,
        max_insns: cli.max_insns,
        disks,
        net: cli.net,
        ipv6: !cli.no_ipv6,
        rng: !cli.no_rng,
        forwards: cli.forwards.clone(),
        threads: !cli.single_thread,
    };

    let mut machine = Machine::new(config, Box::new(io::stdout()), None).map_err(|e| e.to_string())?;

    let t_machine = setup_start.elapsed();
    if let Some(path) = &cli.dump_dtb {
        return dump_dtb(&machine, path).map(|_| ExitCode::SUCCESS);
    }

    status!(
        cli,
        "{} MiB RAM, {} hart(s){}, {} virtio device(s){}{}",
        cli.mem >> 20,
        smp,
        if smp > 1 && !cli.single_thread { " on one host thread each" } else { "" },
        cli.disks.len() + usize::from(persistent) + usize::from(cli.net) + usize::from(!cli.no_rng),
        if cli.net { ", user-mode network" } else { "" },
        if cli.bios.is_some() { ", OpenSBI firmware" } else { "" },
    );

    // The machine owns the input channel now; create it once the config is valid.
    machine.attach_input(console::spawn_stdin_reader());
    machine.set_plain_output(!console::stdout_is_tty());

    let guard = if cli.no_raw { None } else { Some(TerminalGuard::enable_raw_mode()) };
    let started = Instant::now();
    let exit = machine.run();
    drop(guard);
    let elapsed = started.elapsed();

    let code = match exit {
        Exit::Shutdown(code) => {
            status!(cli, "guest powered off");
            code as u8
        }
        Exit::Reboot => {
            status!(cli, "guest requested a reboot; exiting");
            0
        }
        Exit::Halt => {
            eprintln!("\nrvemu: all harts stopped; the guest can no longer run");
            1
        }
        Exit::Limit => {
            status!(cli, "instruction limit reached");
            0
        }
        Exit::OutputClosed => {
            status!(cli, "output closed; stopping");
            0
        }
    };

    if cli.stats {
        eprintln!(
            "[rvemu] setup {:.0} ms (root disk {:.0}, rootfs {:.0}, kernel wait {:.0}, machine {:.0})",
            t_machine.as_secs_f64() * 1e3,
            t_disk.as_secs_f64() * 1e3,
            (t_rootfs - t_disk).as_secs_f64() * 1e3,
            (t_kernel - t_rootfs).as_secs_f64() * 1e3,
            (t_machine - t_kernel).as_secs_f64() * 1e3
        );
        let secs = elapsed.as_secs_f64();
        let mips = machine.insns as f64 / secs.max(1e-9) / 1e6;
        eprintln!(
            "[rvemu] {} instructions in {:.3} s ({:.1} MIPS, {} hart(s), {} MiB RAM)",
            machine.insns,
            secs,
            mips,
            smp,
            cli.mem >> 20
        );
        #[cfg(feature = "counters")]
        eprintln!("[rvemu]   STACK_MAX            {}", rvemu::counters::STACK_MAX.load(std::sync::atomic::Ordering::Relaxed));
        #[cfg(feature = "counters")]
        for (name, value) in rvemu::counters::snapshot().0 {
            eprintln!("[rvemu]   {name:<20} {value}");
        }
        #[cfg(feature = "counters")]
        {
            let mut ops: Vec<(u64, u8)> = (0..108u8)
                .map(|i| (rvemu::counters::OP_COUNT[i as usize].load(std::sync::atomic::Ordering::Relaxed), i))
                .filter(|(v, _)| *v != 0)
                .collect();
            ops.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let total: u64 = ops.iter().map(|(v, _)| v).sum();
            let mut bigrams: Vec<(u32, u8, u8)> = Vec::new();
            for a in 0..128u8 {
                for b in 0..128u8 {
                    let v = rvemu::counters::BIGRAM[a as usize][b as usize]
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if v != 0 { bigrams.push((v, a, b)); }
                }
            }
            bigrams.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            eprintln!("[rvemu]   -- top bigrams --");
            for (value, a, b) in bigrams.iter().take(48) {
                // SAFETY: both indices are below the number of Op variants.
                let (oa, ob) = unsafe {
                    (std::mem::transmute::<u8, rvemu::insn::Op>(*a), std::mem::transmute::<u8, rvemu::insn::Op>(*b))
                };
                eprintln!("[rvemu]   {oa:<8?} {ob:<8?} {value:>12}");
            }
            let mut trigrams: Vec<(u32, u8, u8, u8)> = Vec::new();
            for a in 0..128u8 {
                for b in 0..128u8 {
                    for c in 0..128u8 {
                        let v = rvemu::counters::TRIGRAM[a as usize][b as usize][c as usize]
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if v != 0 { trigrams.push((v, a, b, c)); }
                    }
                }
            }
            trigrams.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            eprintln!("[rvemu]   -- top trigrams --");
            for (value, a, b, c) in trigrams.iter().take(32) {
                // SAFETY: all indices are below the number of Op variants.
                let (oa, ob, oc) = unsafe {
                    (std::mem::transmute::<u8, rvemu::insn::Op>(*a),
                     std::mem::transmute::<u8, rvemu::insn::Op>(*b),
                     std::mem::transmute::<u8, rvemu::insn::Op>(*c))
                };
                eprintln!("[rvemu]   {oa:<8?} {ob:<8?} {oc:<8?} {value:>10}");
            }
            let mut quads: Vec<(u32, usize, usize, usize, usize)> = Vec::new();
            for a in 0..32usize {
                for b in 0..32usize {
                    for c in 0..32usize {
                        for d in 0..32usize {
                            let idx = ((a * 32 + b) * 32 + c) * 32 + d;
                            let v = rvemu::counters::QUAD[idx].load(std::sync::atomic::Ordering::Relaxed);
                            if v != 0 { quads.push((v, a, b, c, d)); }
                        }
                    }
                }
            }
            quads.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            let mut runs: Vec<(u64, usize)> = Vec::new();
            for slot in 0..rvemu::counters::RUN_SLOTS {
                let len = rvemu::counters::RUN_LEN[slot].load(std::sync::atomic::Ordering::Relaxed) as usize;
                if len >= 2 {
                    runs.push((rvemu::counters::RUN_COUNT[slot].load(std::sync::atomic::Ordering::Relaxed), slot));
                }
            }
            runs.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            eprintln!("[rvemu]   -- top ALU runs (sampled x64) --");
            for (count, slot) in runs.iter() {
                let len = rvemu::counters::RUN_LEN[*slot].load(std::sync::atomic::Ordering::Relaxed) as usize;
                let k = len / 2;
                let mut line = format!("SPECRUN {} {}", count * 64, k);
                for i in 0..k {
                    let w0 = rvemu::counters::RUN_DATA[*slot][i * 2].load(std::sync::atomic::Ordering::Relaxed);
                    let w1 = rvemu::counters::RUN_DATA[*slot][i * 2 + 1].load(std::sync::atomic::Ordering::Relaxed);
                    let (op, rd, rs1, rs2) = (w0 as u8, (w0 >> 8) as u8, (w0 >> 16) as u8, (w0 >> 24) as u8);
                    let imm = w1 as u32 as i32;
                    let ilen = (w1 >> 32) as u8;
                    let name = unsafe { std::mem::transmute::<u8, rvemu::insn::Op>(op) };
                    line.push_str(&format!(" {name:?}:{rd}:{rs1}:{rs2}:{imm}:{ilen}"));
                }
                eprintln!("{line}");
            }
            for (count, slot) in runs.iter().take(48) {
                let len = rvemu::counters::RUN_LEN[*slot].load(std::sync::atomic::Ordering::Relaxed) as usize;
                let mut line = format!("[rvemu]   {:>10} ", count * 64);
                for i in 0..len / 2 {
                    let w = rvemu::counters::RUN_DATA[*slot][i].load(std::sync::atomic::Ordering::Relaxed);
                    let (op, rd, rs1) = (w as u8, (w >> 8) as u8, (w >> 16) as u8);
                    let (rs2, imm) = ((w >> 24) as u8, (w >> 32) as u32 as i32);
                    let opname = unsafe { std::mem::transmute::<u8, rvemu::insn::Op>(op) };
                    line.push_str(&format!(" {opname:?} x{rd},{rs1},{rs2}/{imm};"));
                }
                eprintln!("{line}");
            }
            eprintln!("[rvemu]   -- top unfused bigrams --");
            let mut singles: Vec<(u32, u8, u8)> = Vec::new();
            for a in 0..128u8 {
                for b in 0..128u8 {
                    let v = rvemu::counters::SINGLE_BIGRAM[a as usize][b as usize]
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if v != 0 { singles.push((v, a, b)); }
                }
            }
            singles.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            eprintln!("[rvemu]   -- top unfused bigrams --");
            for (value, a, b) in singles.iter().take(24) {
                let (oa, ob) = unsafe {
                    (std::mem::transmute::<u8, rvemu::insn::Op>(*a), std::mem::transmute::<u8, rvemu::insn::Op>(*b))
                };
                eprintln!("[rvemu]   {oa:<8?} {ob:<8?} {value:>12}");
            }
            let mut gen: Vec<(u64, u8)> = (0..108u8)
                .map(|i| (rvemu::counters::GENERIC_OP[i as usize].load(std::sync::atomic::Ordering::Relaxed), i))
                .filter(|(v, _)| *v != 0)
                .collect();
            gen.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let mut used: Vec<(u64, usize)> = (0..1200)
                .map(|i| (rvemu::counters::SPEC_USE[i].load(std::sync::atomic::Ordering::Relaxed), i))
                .filter(|(v, _)| *v != 0)
                .collect();
            used.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            for (v, i) in used.iter().take(10) {
                eprintln!("[rvemu]   spec {i} used {v}");
            }
            let mut csrs: Vec<(u64, usize)> = (0..4096 * 4)
                .map(|i| (rvemu::counters::CSR_USE[i].load(std::sync::atomic::Ordering::Relaxed), i))
                .filter(|(v, _)| *v != 0)
                .collect();
            csrs.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            eprintln!("[rvemu]   -- top csrs (addr kind: 0 read, 1 rw, 2 rs, 3 rc) --");
            for (v, i) in csrs.iter().take(16) {
                eprintln!("[rvemu]   csr {:#05x} kind {} {v}", i / 4, i % 4);
            }
            eprintln!(
                "[rvemu]   build phases ns: decode {} mark {} plan {} steps {} alloc {}",
                rvemu::counters::PHASE_NS[0].load(std::sync::atomic::Ordering::Relaxed),
                rvemu::counters::PHASE_NS[1].load(std::sync::atomic::Ordering::Relaxed),
                rvemu::counters::PHASE_NS[2].load(std::sync::atomic::Ordering::Relaxed),
                rvemu::counters::PHASE_NS[3].load(std::sync::atomic::Ordering::Relaxed),
                rvemu::counters::PHASE_NS[4].load(std::sync::atomic::Ordering::Relaxed)
            );
            eprintln!("[rvemu]   -- top general-path ops --");
            for (v, i) in gen.iter().take(12) {
                let op = unsafe { std::mem::transmute::<u8, rvemu::insn::Op>(*i) };
                eprintln!("[rvemu]   {op:<10?} {v}");
            }
            eprintln!("[rvemu]   -- top quads --");
            for (value, a, b, c, d) in quads.iter().take(32) {
                let n = |i: usize| format!("{:?}", rvemu::counters::QUAD_CLASS_OPS[i]);
                eprintln!("[rvemu]   {:<8} {:<8} {:<8} {:<8} {value:>10}", n(*a), n(*b), n(*c), n(*d));
            }
            eprintln!("[rvemu]   -- op frequency (total {total}) --");
            for (value, i) in ops.iter().take(24) {
                // SAFETY: the index is below the number of Op variants.
                let op: rvemu::insn::Op = unsafe { std::mem::transmute::<u8, rvemu::insn::Op>(*i) };
                eprintln!("[rvemu]   {op:<10?} {value:>14} {:5.2}%", *value as f64 * 100.0 / total as f64);
            }
        }
    }
    // The process is about to end: freeing hundreds of megabytes of guest RAM and tens of
    // thousands of decoded blocks one by one only delays the exit (about 10 ms of a boot).
    std::mem::forget(machine);
    Ok(ExitCode::from(code))
}

/// Write the DTB that hart 0 was handed at boot.
fn dump_dtb(machine: &Machine, path: &Path) -> Result<(), String> {
    let addr = machine.harts[0].x[11];
    let mut header = [0u8; 8];
    if !machine.bus.read_bytes(addr, &mut header) {
        return Err("device tree is not in RAM".into());
    }
    let total = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    let mut blob = vec![0u8; total];
    machine.bus.read_bytes(addr, &mut blob);
    fs::write(path, &blob).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    eprintln!("[rvemu] wrote {} bytes of device tree to {}", total, path.display());
    Ok(())
}

/// Ask the scheduler to keep the emulator thread on the fast cores. On Apple silicon a
/// thread at the default quality of service is moved between performance and efficiency
/// cores whenever the machine is busy, and every move cools the branch predictors and the
/// caches the interpreter depends on; this thread does the guest's work, so it gets the
/// highest class.
fn prefer_fast_cores() {
    #[cfg(target_os = "macos")]
    // SAFETY: a plain call that changes the calling thread's scheduling class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

fn main() -> ExitCode {
    // Make sure a panic never leaves the host terminal in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        console::restore_terminal();
        default_hook(info);
    }));

    let cli = Cli::parse();
    // The block engine runs chains of blocks through nested calls, so the
    // emulator gets a stack bigger than a default thread's.
    let result = std::thread::Builder::new()
        .name("rvemu".into())
        .stack_size(64 << 20)
        .spawn(move || {
            prefer_fast_cores();
            run(cli)
        })
        .expect("failed to spawn the emulator thread")
        .join()
        .expect("emulator thread panicked");
    match result {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("rvemu: {msg}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_with_and_without_a_bind_address() {
        assert_eq!(parse_forward("2222:22"), Ok(PortForward::local(2222, 22)));
        assert_eq!(
            parse_forward("0.0.0.0:2222:22"),
            Ok(PortForward { bind: Ipv4Addr::UNSPECIFIED, host_port: 2222, guest_port: 22 })
        );
        assert!(parse_forward("2222").is_err());
        assert!(parse_forward("localhost:1:2").is_err());
        assert!(parse_forward("1:2:3:4").is_err());
        assert!(parse_forward("70000:22").is_err());
    }
}
