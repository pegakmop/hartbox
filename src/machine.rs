//! The whole machine: harts, bus, devices, boot layout and the run loop.
//!
//! By default the harts are multiplexed on one host thread in fixed-size quanta. Because
//! they never run concurrently, memory is trivially sequentially consistent
//! and remote TLB shootdowns can be applied at quantum boundaries before the
//! target hart runs again. With `MachineConfig::threads` (the command line's default for
//! `--smp N` with N > 1) every hart runs on its own host thread instead, and guest time
//! follows the host clock; see `mt.rs` for what that changes.
//!
//! Time is derived from the retired-instruction count (one `time` tick per
//! `INSNS_PER_TICK` instructions), which is deterministic but pins the guest at
//! a fixed virtual speed whatever the host does. With `real_time` it follows the
//! host's monotonic clock instead (as in QEMU's TCG), so a guest benchmark such
//! as `7z b` reports the speed the interpreter really has. When every hart is idle in `wfi`, the machine either sleeps on the host
//! until the next timer or input event, or (with `warp`) advances time straight
//! to the next deadline.

use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

/// Nanoseconds the machine has spent sleeping because the guest was idle. The
/// embedder subtracts it from wall time: a guest waiting on a host file read
/// should not look like a slow interpreter.
pub static IDLE_NS_HINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

use std::fs::OpenOptions;
use std::path::PathBuf;

use crate::arch::*;
use crate::devices::blk::VirtioBlk;
use crate::devices::net::VirtioNet;
use crate::devices::rng::VirtioRng;
use crate::devices::rtc::wall_ns;
use crate::net::{Gateway, PortForward, GUEST_MAC};
use crate::bus::{Bus, Exit, Req};
use crate::devices::uart::Uart;
use crate::fdt::{build_machine_dtb, MachineDesc};
use crate::hart::Hart;
use crate::loader::{load_at, load_firmware, load_kernel};

/// Total instructions retired by all harts, refreshed every quantum. Embedders
/// (the iOS app) read this to show live throughput.
pub static INSNS_HINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set by an embedder to stop the machine at the next quantum boundary.
pub static STOP_HINT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Instructions per scheduling quantum. A quantum is about 0.65 ms of
/// emulated time (16 instructions per `time` tick at 10 MHz). Interrupts are
/// delivered at quantum boundaries, so this stays well below the guest's own
/// timer period, while a larger quantum keeps the per-quantum overhead
/// (device updates, host clock reads, chain unwinding) small.
const QUANTUM: u64 = 65536;
/// Longest host sleep while idle, so console input and the stop flag stay responsive.
const MAX_IDLE_SLEEP: Duration = Duration::from_millis(20);
const OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(5);
const NET_POLL_INTERVAL: Duration = Duration::from_micros(500);
const NET_IDLE_SLEEP: Duration = Duration::from_millis(1);
const PAGE: u64 = 4096;
/// Free RAM kept above the device tree for firmware fixups.
const DTB_SLACK: u64 = 64 << 10;
/// SBI firmware (OpenSBI `fw_dynamic`) info block magic: "OSBI".
const FW_DYNAMIC_MAGIC: u64 = 0x4942_534f;
const FW_DYNAMIC_VERSION: u64 = 2;

fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

fn align_down(v: u64, a: u64) -> u64 {
    v & !(a - 1)
}

/// A raw disk image exposed to the guest as a virtio-blk device.
#[derive(Clone, Debug)]
pub struct DiskConfig {
    pub path: PathBuf,
    pub read_only: bool,
}

/// Everything needed to construct a machine.
#[derive(Default)]
pub struct MachineConfig {
    pub ram_size: u64,
    pub harts: usize,
    pub bootargs: String,
    /// M-mode firmware (e.g. OpenSBI `fw_dynamic`). When absent, the built-in SBI is used.
    pub firmware: Option<Vec<u8>>,
    pub kernel: Option<Vec<u8>>,
    pub initrd: Option<Vec<u8>>,
    /// Bare-metal ELF or binary started directly in M-mode (no kernel).
    pub program: Option<Vec<u8>>,
    /// Skip host sleeps when every hart is idle.
    pub warp: bool,
    /// Drive guest time from the host's monotonic clock instead of the retired
    /// instruction count.
    pub real_time: bool,
    pub max_insns: Option<u64>,
    /// Disk images, attached in order as virtio-blk devices.
    pub disks: Vec<DiskConfig>,
    /// Attach a virtio-net NIC connected to the user-mode gateway.
    pub net: bool,
    /// Give the gateway IPv6 (fd00:2::/64) as well as IPv4 (requires `net`).
    pub ipv6: bool,
    /// Attach a virtio-rng entropy source.
    pub rng: bool,
    /// Host ports forwarded into the guest (requires `net`).
    pub forwards: Vec<PortForward>,
    /// Run each hart on its own host thread (needs more than one hart; guest time then
    /// follows the host clock). See `mt.rs`.
    pub threads: bool,
}

pub struct Machine {
    pub harts: Vec<Hart>,
    pub bus: Bus,
    /// Total instructions retired by all harts, as of the last tick.
    pub insns: u64,
    /// Virtual time spent idle (host sleeps, or warped), in timebase ticks.
    idle_ticks: u64,
    warp: bool,
    /// Guest time follows the host clock (see the module comment).
    real_time: bool,
    /// Host instant at which guest time was zero (set by the first `run`).
    clock_origin: Option<Instant>,
    /// Ticks jumped over by `warp` while in real-time mode.
    warp_ticks: u64,
    max_insns: Option<u64>,
    input: Option<Receiver<Vec<u8>>>,
    out: Box<dyn Write>,
    started: Instant,
    last_flush: Instant,
    gateway: Option<Gateway>,
    net_slot: Option<usize>,
    last_net_poll: Instant,
    /// Strip terminal-only sequences from guest output (set when not a terminal).
    plain_output: bool,
    /// One host thread per hart.
    threads: bool,
}

impl Machine {
    pub fn new(cfg: MachineConfig, out: Box<dyn Write>, input: Option<Receiver<Vec<u8>>>) -> Result<Self, String> {
        if cfg.harts == 0 || cfg.harts > 64 {
            return Err("number of harts must be between 1 and 64".into());
        }
        if cfg.ram_size < 16 << 20 || !cfg.ram_size.is_multiple_of(PAGE) {
            return Err("RAM size must be at least 16 MiB and a multiple of 4 KiB".into());
        }
        if cfg.program.is_some() && cfg.kernel.is_some() {
            return Err("a bare-metal program and a kernel cannot be loaded together".into());
        }
        if cfg.firmware.is_none() && cfg.kernel.is_none() && cfg.program.is_none() {
            return Err("nothing to boot: give a kernel (-k), firmware (--bios) or a program".into());
        }

        let harts_n = cfg.harts;
        let hle = cfg.firmware.is_none();
        let ram_end = RAM_BASE + cfg.ram_size;
        let mut bus = Bus::new(cfg.ram_size as usize, harts_n);

        // virtio devices, in slot order: disks, network, entropy.
        let mut virtio_count = 0;
        for disk in &cfg.disks {
            let file = OpenOptions::new()
                .read(true)
                .write(!disk.read_only)
                .open(&disk.path)
                .map_err(|e| format!("cannot open disk {}: {e}", disk.path.display()))?;
            let blk = VirtioBlk::new(file, disk.read_only).map_err(|e| e.to_string())?;
            bus.attach_virtio(Box::new(blk));
            virtio_count += 1;
        }
        let net_slot = cfg.net.then(|| {
            virtio_count += 1;
            bus.attach_virtio(Box::new(VirtioNet::new(GUEST_MAC)))
        });
        if cfg.rng {
            bus.attach_virtio(Box::new(VirtioRng::new()));
            virtio_count += 1;
        }

        // Firmware (or the bare-metal program) occupies the start of RAM.
        let mut fw_end = RAM_BASE;
        let has_fw = cfg.firmware.is_some();
        if let Some(fw) = cfg.firmware {
            let l = load_firmware(&mut bus, fw)?;
            fw_end = align_up(l.end, PAGE);
        }
        let mut entry_prog = None;
        if let Some(p) = cfg.program {
            let l = load_firmware(&mut bus, p)?;
            entry_prog = Some(l.entry);
            fw_end = align_up(l.end, PAGE).max(fw_end);
        }

        let kernel = match cfg.kernel {
            Some(k) => Some(load_kernel(&mut bus, k)?),
            None => None,
        };
        let kernel_end = align_up(kernel.map_or(fw_end, |k| k.end.max(fw_end)), PAGE);

        // Layout from the top of RAM down: DTB, [firmware info], initrd.
        let fw_reserved = has_fw.then_some((RAM_BASE, fw_end - RAM_BASE));
        let desc = |initrd| MachineDesc {
            harts: harts_n,
            ram_size: cfg.ram_size,
            bootargs: &cfg.bootargs,
            initrd,
            firmware: fw_reserved,
            virtio_devices: virtio_count,
        };
        let dtb_len = build_machine_dtb(&desc(cfg.initrd.as_ref().map(|_| (0, 0)))).len() as u64;
        // Leave slack above the tree: firmware (OpenSBI) grows the device tree
        // in place when it adds reserved-memory and fixup nodes.
        let dtb_addr = align_down(ram_end - dtb_len - DTB_SLACK, PAGE);
        let fw_info_addr = dtb_addr - PAGE;
        let initrd = match cfg.initrd {
            Some(rd) => {
                let top = if has_fw { fw_info_addr } else { dtb_addr };
                let start = align_down(top - rd.len() as u64, PAGE);
                if start < kernel_end {
                    return Err(format!(
                        "initramfs ({} MiB) does not fit in {} MiB of RAM; use -m to add memory",
                        rd.len() >> 20,
                        cfg.ram_size >> 20
                    ));
                }
                load_at(&mut bus, start, &rd)?;
                Some((start, start + rd.len() as u64))
            }
            None => None,
        };
        let dtb = build_machine_dtb(&desc(initrd));
        debug_assert_eq!(dtb.len() as u64, dtb_len);
        load_at(&mut bus, dtb_addr, &dtb)?;

        let fw_next = kernel.map(|k| k.entry).or(entry_prog).unwrap_or(0);
        if has_fw {
            let words = [FW_DYNAMIC_MAGIC, FW_DYNAMIC_VERSION, fw_next, 1 /* next mode: S */, 0, 0];
            let mut blk = Vec::with_capacity(48);
            for w in words {
                blk.extend_from_slice(&w.to_le_bytes());
            }
            load_at(&mut bus, fw_info_addr, &blk)?;
        }

        // Harts and their boot state.
        let mut harts: Vec<Hart> = (0..harts_n as u64).map(|i| Hart::new(i, hle)).collect();
        for h in harts.iter_mut() {
            h.mstatus = MSTATUS_FS_INITIAL;
            if hle {
                // No firmware to delegate for us: do it here.
                h.medeleg = 0xB1FF;
                h.mideleg = MIP_S_MASK;
                h.mcounteren = 0x7;
                h.scounteren = 0x7;
            }
        }
        if has_fw {
            for (i, h) in harts.iter_mut().enumerate() {
                h.pc = RAM_BASE;
                h.set_prv(PRV_M);
                h.x[10] = i as u64;
                h.x[11] = dtb_addr;
                h.x[12] = fw_info_addr;
            }
        } else if let Some(entry) = entry_prog {
            let h = &mut harts[0];
            h.pc = entry;
            h.set_prv(PRV_M);
            h.x[10] = 0;
            h.x[11] = dtb_addr;
        } else if let Some(k) = kernel {
            let h = &mut harts[0];
            h.pc = k.entry;
            h.set_prv(PRV_S);
            h.x[10] = 0;
            h.x[11] = dtb_addr;
        }
        // Secondary harts start stopped in the built-in SBI case; the kernel
        // starts them through SBI HSM.
        if hle {
            for (i, flag) in bus.hart_stopped.iter_mut().enumerate() {
                if i > 0 {
                    *flag = true;
                    harts[i].stopped = true;
                    harts[i].break_chain();
                }
            }
        }

        let gateway = match cfg.net {
            true => Some(Gateway::new(&cfg.forwards, cfg.ipv6).map_err(|e| format!("cannot set up networking: {e}"))?),
            false => None,
        };
        let now = Instant::now();
        Ok(Machine {
            harts,
            bus,
            insns: 0,
            idle_ticks: 0,
            warp: cfg.warp,
            real_time: cfg.real_time,
            clock_origin: None,
            warp_ticks: 0,
            max_insns: cfg.max_insns,
            input,
            out,
            started: now,
            last_flush: now,
            gateway,
            net_slot,
            last_net_poll: now,
            plain_output: false,
            threads: cfg.threads && harts_n > 1,
        })
    }

    /// Run until the guest powers off, resets, or the instruction limit is hit.
    pub fn run(&mut self) -> Exit {
        if self.threads {
            return self.run_threads();
        }
        loop {
            if STOP_HINT.swap(false, std::sync::atomic::Ordering::Relaxed) {
                self.flush_output(true);
                return Exit::Halt;
            }
            for i in 0..self.harts.len() {
                if self.harts[i].stopped {
                    continue;
                }
                self.harts[i].run(&mut self.bus, QUANTUM);
            }
            self.process_requests();
            self.tick();

            if let Some(exit) = self.bus.exit.take() {
                self.flush_output(true);
                return exit;
            }
            if self.max_insns.is_some_and(|m| self.insns >= m) {
                self.flush_output(true);
                return Exit::Limit;
            }
            if self.all_stopped() {
                self.flush_output(true);
                return Exit::Halt;
            }
            if !self.any_runnable() {
                self.idle();
            }
        }
    }

    /// Run every hart on its own host thread; this thread serves the devices until the
    /// guest stops. Guest time follows the host clock.
    fn run_threads(&mut self) -> Exit {
        use crate::mt::{hart_main, HartPtrs, Mt};
        use std::sync::atomic::Ordering::{Relaxed, Release};
        let n = self.harts.len();
        let base = if self.real_time { self.real_ticks() } else { self.bus.mtime };
        let mt: &'static Mt = Box::leak(Box::new(Mt::new(n, base)));
        // Blocks decoded so far may hold `fence` as a no-op.
        for (i, h) in self.harts.iter_mut().enumerate() {
            h.reset_blocks();
            h.mt_ctl = &mt.ctl[i];
            mt.ctl[i].progress.store(h.instret, Relaxed);
            mt.ctl[i].hart.store(h as *mut Hart, Release);
        }
        self.bus.mt = Some(mt);
        let bus: *mut Bus = &mut self.bus;
        let ptrs: Vec<*mut Hart> = self.harts.iter_mut().map(|h| h as *mut Hart).collect();
        let exit = std::thread::scope(|scope| {
            for (i, hart) in ptrs.into_iter().enumerate() {
                let p = HartPtrs { hart, bus };
                std::thread::Builder::new()
                    .name(format!("hart{i}"))
                    .stack_size(64 << 20)
                    .spawn_scoped(scope, move || hart_main(mt, i, p))
                    .expect("failed to spawn a hart thread");
            }
            let exit = self.serve_threads(mt);
            mt.stop_all();
            exit
        });
        self.bus.mt = None;
        for h in self.harts.iter_mut() {
            h.mt_ctl = std::ptr::null();
            h.reset_blocks();
        }
        self.insns = self.harts.iter().map(|h| h.instret).sum();
        INSNS_HINT.store(self.insns, Relaxed);
        self.bus.mtime = mt.now_ticks();
        self.flush_output_mt(true);
        exit
    }

    /// The machine thread of a multi-threaded run: input, console output, timers of the
    /// devices, networking, stop conditions.
    fn serve_threads(&mut self, mt: &'static crate::mt::Mt) -> Exit {
        use std::sync::atomic::Ordering::Relaxed;
        loop {
            if STOP_HINT.swap(false, Relaxed) {
                return Exit::Halt;
            }
            let (exit, all_stopped) = {
                let _g = mt.lock.lock();
                self.pump_input();
                self.bus.rtc.tick(wall_ns());
                self.service_network();
                self.bus.refresh_irq_lines(mt);
                (self.bus.exit.take(), self.bus.hart_stopped.iter().all(|&stopped| stopped))
            };
            self.insns = mt.ctl.iter().map(|c| c.progress.load(Relaxed)).sum();
            INSNS_HINT.store(self.insns, Relaxed);
            self.bus.mtime = mt.now_ticks();
            self.flush_output_mt(false);
            if let Some(exit) = exit {
                return exit;
            }
            if self.max_insns.is_some_and(|m| self.insns >= m) {
                return Exit::Limit;
            }
            if all_stopped {
                return Exit::Halt;
            }
            let wait = if self.gateway.as_ref().is_some_and(Gateway::is_busy) { NET_POLL_INTERVAL } else { Duration::from_millis(2) };
            match &self.input {
                Some(rx) => match rx.recv_timeout(wait) {
                    Ok(chunk) => {
                        let _g = mt.lock.lock();
                        self.bus.uart.push_input(&chunk);
                        self.bus.refresh_irq_lines(mt);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => self.input = None,
                },
                None => std::thread::sleep(wait),
            }
        }
    }

    /// `flush_output` for a run where harts append to the console buffer concurrently: take
    /// the buffer under the lock, write it without.
    fn flush_output_mt(&mut self, force: bool) {
        let Some(mt) = self.bus.mt else {
            return self.flush_output(force);
        };
        let pending = {
            let _g = mt.lock.lock();
            // The machine thread wakes every few milliseconds anyway, so flush at every wake-up:
            // an idle guest's echo and prompt then show up without a delay.
            let due = force || self.bus.uart.tx.len() >= 4096 || self.last_flush.elapsed() >= Duration::from_millis(1);
            if self.bus.uart.tx.is_empty() || !due {
                return;
            }
            std::mem::take(&mut self.bus.uart.tx)
        };
        let written = if self.plain_output {
            self.out.write_all(&crate::console::plain_text(&pending))
        } else {
            self.out.write_all(&pending)
        }
        .and_then(|_| self.out.flush());
        if let Err(e) = written {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                let _g = mt.lock.lock();
                self.bus.exit = Some(Exit::OutputClosed);
            }
        }
        self.last_flush = Instant::now();
    }

    fn all_stopped(&self) -> bool {
        self.harts.iter().all(|h| h.stopped)
    }

    fn any_runnable(&self) -> bool {
        self.harts.iter().any(|h| !h.stopped && (!h.wfi || h.mip & h.mie != 0))
    }

    /// Apply requests that harts made of the machine (SBI HSM, IPI, TLB
    /// shootdown). Harts never run concurrently, so applying these between
    /// quanta is indistinguishable from applying them immediately.
    fn process_requests(&mut self) {
        if self.bus.reqs.is_empty() {
            return;
        }
        for req in std::mem::take(&mut self.bus.reqs) {
            match req {
                Req::Start { hart, pc, opaque } => {
                    let h = &mut self.harts[hart];
                    h.stopped = false;
                    h.wfi = false;
                    h.pc = pc;
                    h.set_prv(PRV_S);
                    h.x[10] = hart as u64;
                    h.x[11] = opaque;
                    h.reservation = None;
                    h.flush_translation_cache();
                    h.irq_dirty = true;
                    h.break_chain();
                }
                Req::Ssip { hart } => {
                    let h = &mut self.harts[hart];
                    h.mip |= MIP_SSIP;
                    h.irq_dirty = true;
                    h.break_chain();
                }
                Req::FlushTlbAll => {
                    for h in self.harts.iter_mut() {
                        h.tlb.flush_all();
                        h.flush_translation_cache();
                    }
                }
            }
        }
    }

    /// Refresh device state and interrupt lines. Called once per quantum.
    fn tick(&mut self) {
        self.insns = self.harts.iter().map(|h| h.instret).sum();
        INSNS_HINT.store(self.insns, std::sync::atomic::Ordering::Relaxed);
        IDLE_NS_HINT.store(self.idle_ticks.saturating_mul(1_000_000_000 / crate::arch::TIMEBASE_HZ), std::sync::atomic::Ordering::Relaxed);
        let now = if self.real_time { self.real_ticks() } else { self.insns / INSNS_PER_TICK + self.idle_ticks };
        self.bus.mtime = now;

        self.pump_input();
        self.bus.uart_irq_update();
        self.bus.rtc.tick(wall_ns());
        self.service_network();
        self.bus.update_device_irqs();

        let hle = self.harts.first().is_some_and(|h| h.hle);
        for (i, h) in self.harts.iter_mut().enumerate() {
            let expired = now >= self.bus.clint.mtimecmp[i].load(std::sync::atomic::Ordering::Relaxed);
            if hle {
                // No M-mode firmware: the timer is delivered directly as a
                // supervisor timer interrupt (STIP), which set_timer clears.
                if expired {
                    h.set_hw_pending(MIP_STIP, true);
                }
            } else {
                h.set_hw_pending(MIP_MTIP, expired);
            }
            h.set_hw_pending(MIP_MSIP, self.bus.clint.msip[i].load(std::sync::atomic::Ordering::Relaxed) != 0);
            let ext_m = self.bus.plic.context_pending(2 * i);
            let ext_s = self.bus.plic.context_pending(2 * i + 1);
            h.set_hw_pending(MIP_MEIP, ext_m);
            h.set_hw_pending(MIP_SEIP, ext_s);
        }
        self.flush_output(false);
    }

    /// Guest time in timebase ticks from the host's monotonic clock, never going
    /// backwards. Read once per quantum (about 0.1 ms of host time), which is
    /// also how often `mtime` and the timer comparison are refreshed.
    fn real_ticks(&mut self) -> u64 {
        let origin = *self.clock_origin.get_or_insert_with(Instant::now);
        let ns = origin.elapsed().as_nanos();
        let ticks = (ns * TIMEBASE_HZ as u128 / 1_000_000_000) as u64 + self.warp_ticks;
        ticks.max(self.bus.mtime)
    }

    /// Move frames between the virtio-net device and the gateway. The gateway
    /// is polled right away when the guest sent something, and otherwise at
    /// most every `NET_POLL_INTERVAL`.
    fn service_network(&mut self) {
        let (Some(gw), Some(slot)) = (self.gateway.as_mut(), self.net_slot) else {
            return;
        };
        let mut sent = false;
        if let Some(net) = self.bus.device_mut::<VirtioNet>(slot) {
            for frame in net.take_tx() {
                gw.from_guest(&frame);
                sent = true;
            }
        }
        if sent || self.last_net_poll.elapsed() >= NET_POLL_INTERVAL {
            gw.poll();
            self.last_net_poll = Instant::now();
            if let Some(net) = self.bus.device_mut::<VirtioNet>(slot) {
                for frame in gw.drain_to_guest() {
                    net.push_rx(frame);
                }
            }
            self.bus.virtio_poll(slot);
        }
    }

    fn pump_input(&mut self) {
        let mut closed = false;
        if let Some(rx) = &self.input {
            loop {
                match rx.try_recv() {
                    Ok(chunk) => self.bus.uart.push_input(&chunk),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
        }
        if closed {
            self.input = None;
        }
    }

    /// Nothing can run until an interrupt arrives: sleep until the next timer
    /// deadline (or input), or warp straight to it.
    fn idle(&mut self) {
        self.flush_output(true);
        let now = self.bus.mtime;
        let next_deadline = self
            .bus
            .clint
            .mtimecmp
            .iter()
            .zip(self.harts.iter())
            .filter(|(_, h)| !h.stopped)
            .map(|(c, _)| c.load(std::sync::atomic::Ordering::Relaxed))
            .filter(|c| *c > now)
            .min();

        if self.warp {
            if let Some(deadline) = next_deadline {
                self.idle_ticks += deadline - now;
                if self.real_time {
                    self.warp_ticks += deadline - now;
                }
                self.bus.mtime = deadline;
                return;
            }
        }

        let wait = match next_deadline {
            Some(deadline) => {
                let ns = (deadline - now).saturating_mul(1_000_000_000 / TIMEBASE_HZ);
                Duration::from_nanos(ns).min(MAX_IDLE_SLEEP)
            }
            None => MAX_IDLE_SLEEP,
        };
        // Open connections need prompt polling of host sockets.
        let wait = if self.gateway.as_ref().is_some_and(Gateway::is_busy) { wait.min(NET_IDLE_SLEEP) } else { wait };
        let before = Instant::now();
        match &self.input {
            Some(rx) => match rx.recv_timeout(wait) {
                Ok(chunk) => self.bus.uart.push_input(&chunk),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.input = None,
            },
            None => std::thread::sleep(wait),
        }
        let slept = before.elapsed().as_nanos() as u64;
        self.idle_ticks += slept / (1_000_000_000 / TIMEBASE_HZ);
    }

    /// Write guest output as plain text (for logs and pipes).
    pub fn set_plain_output(&mut self, plain: bool) {
        self.plain_output = plain;
    }

    fn flush_output(&mut self, force: bool) {
        if self.bus.uart.tx.is_empty() {
            return;
        }
        if force || self.bus.uart.tx.len() >= 4096 || self.last_flush.elapsed() >= OUTPUT_FLUSH_INTERVAL {
            let written = if self.plain_output {
                self.out.write_all(&crate::console::plain_text(&self.bus.uart.tx))
            } else {
                self.out.write_all(&self.bus.uart.tx)
            }
            .and_then(|_| self.out.flush());
            if let Err(e) = written {
                // Nobody is listening any more (e.g. `rvemu | head`): stop the guest.
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    self.bus.exit = Some(Exit::OutputClosed);
                }
            }
            self.bus.uart.tx.clear();
            self.last_flush = Instant::now();
        }
    }

    /// Wall-clock time since the machine was created.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Connect host console input (bytes arrive on the channel).
    pub fn attach_input(&mut self, rx: Receiver<Vec<u8>>) {
        self.input = Some(rx);
    }

    /// Access the console device (for embedding and tests).
    pub fn uart(&mut self) -> &mut Uart {
        &mut self.bus.uart
    }
}

impl Bus {
    /// Recompute the UART interrupt line into the PLIC.
    pub fn uart_irq_update(&mut self) {
        let asserted = self.uart.irq_asserted();
        self.plic.set_level(UART_IRQ, asserted);
    }
}

/// Initial `mstatus.FS` value: FP state present but clean.
const MSTATUS_FS_INITIAL: u64 = 1 << 13;
