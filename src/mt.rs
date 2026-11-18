//! One host thread per hart.
//!
//! With `--smp N` the machine can run each hart on its own host thread instead of
//! multiplexing them on one. Guest RAM is shared as it is; what had to be made safe for
//! concurrent harts:
//!
//! * decoded-code coherence: the per-page code bitmaps, generations and epochs are atomics
//!   (see `bus.rs`), a store checks them after it has written, and a hart that decodes a
//!   page publishes its marks before it reads the bytes;
//! * `amo*`, `lr`/`sc` and the page-table accessed/dirty update are host atomics, and `fence`
//!   is a real barrier (`Op::FenceMt`);
//! * devices are not thread-safe: every MMIO access, SBI call and the machine thread's
//!   periodic service hold `Mt::lock`. Interrupt inputs are recomputed under the lock
//!   whenever device state may have changed and handed to each hart as bits in
//!   `HartCtl::lines`; a hart reads its timer compare value without the lock;
//! * work that one hart asks of another (interrupt, TLB flush, hart start, stop) is posted in
//!   the target's `HartCtl`. The target polls after each bounded chain and
//!   after slow operations that can affect control state. A kick also wakes a
//!   sleeping hart. A remote fence waits for the acknowledgement.
//!
//! Guest time follows the host's monotonic clock in this mode.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::arch::*;
use crate::bus::Bus;
use crate::hart::Hart;

/// A test-and-set lock. The critical sections are a few device register accesses.
pub struct SpinLock(AtomicBool);

pub struct SpinGuard<'a>(&'a SpinLock);

impl SpinLock {
    pub const fn new() -> Self {
        SpinLock(AtomicBool::new(false))
    }

    #[inline]
    pub fn lock(&self) -> SpinGuard<'_> {
        let mut spins = 0u32;
        while self.0.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
            while self.0.load(Relaxed) {
                spins += 1;
                if spins < 256 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
        SpinGuard(self)
    }
}

impl Default for SpinLock {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SpinGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0 .0.store(false, Release);
    }
}

/// Bits of `HartCtl::kick`: what the hart's thread has to do before it runs guest code again.
pub const K_IRQ: u32 = 1;
pub const K_FLUSH: u32 = 2;
pub const K_START: u32 = 4;
pub const K_STOP: u32 = 8;

/// What the other threads can ask of one hart, and how to wake it.
pub struct HartCtl {
    pub kick: AtomicU32,
    /// Device-driven interrupt inputs: bit 0 `msip`, bit 1 external (M), bit 2 external (S).
    pub lines: AtomicU32,
    /// A supervisor software interrupt is pending (SBI IPI).
    pub ssip: AtomicBool,
    /// Flush requests made and acknowledged (a request is served between two guest chains).
    pub flush_req: AtomicU64,
    pub flush_ack: AtomicU64,
    pub start_pc: AtomicU64,
    pub start_opaque: AtomicU64,
    /// The hart's thread is asleep (or the hart is stopped): it serves its pending requests
    /// before it runs guest code again, so a requester need not wait for it.
    pub parked: AtomicBool,
    pub hart: AtomicPtr<Hart>,
    pub progress: AtomicU64,
    pub thread: OnceLock<std::thread::Thread>,
}

impl HartCtl {
    fn new() -> Self {
        HartCtl {
            kick: AtomicU32::new(0),
            lines: AtomicU32::new(0),
            ssip: AtomicBool::new(false),
            flush_req: AtomicU64::new(0),
            flush_ack: AtomicU64::new(0),
            start_pc: AtomicU64::new(0),
            start_opaque: AtomicU64::new(0),
            parked: AtomicBool::new(false),
            hart: AtomicPtr::new(std::ptr::null_mut()),
            progress: AtomicU64::new(0),
            thread: OnceLock::new(),
        }
    }
}

/// Instructions a hart runs between two looks at the machine (interrupt inputs, timer).
pub const QUANTUM: u64 = 65536;
/// Longest a hart sleeps in `wfi` without looking again.
const MAX_PARK: Duration = Duration::from_millis(20);
/// A hart that has just gone idle polls for this long before it sleeps, if its last idle
/// spell was short: waking a parked host thread takes about 6 us, which is a third of a
/// pipe round trip between two processes on different harts.
const POLL: Duration = Duration::from_micros(40);
/// Idle spells longer than this switch polling off until a short one is seen, so an idle
/// guest (timer ticks) still sleeps.
const SHORT_IDLE: Duration = Duration::from_micros(300);

pub struct Mt {
    /// Protects every `Bus` field except RAM and the decoded-code state.
    pub lock: SpinLock,
    pub stop: AtomicBool,
    origin: Instant,
    /// Guest time at `origin`, in timebase ticks.
    base_ticks: u64,
    pub ctl: Vec<HartCtl>,
}

impl Mt {
    pub fn new(harts: usize, base_ticks: u64) -> Self {
        Mt {
            lock: SpinLock::new(),
            stop: AtomicBool::new(false),
            origin: Instant::now(),
            base_ticks,
            ctl: (0..harts).map(|_| HartCtl::new()).collect(),
        }
    }

    /// Guest time in timebase ticks, from the host's monotonic clock.
    #[inline]
    pub fn now_ticks(&self) -> u64 {
        self.base_ticks + self.origin.elapsed().as_nanos() as u64 / (1_000_000_000 / TIMEBASE_HZ)
    }

    /// Ask hart `i` to look at its requests at the next poll and wake it if sleeping.
    pub fn kick(&self, i: usize, bits: u32) {
        let c = &self.ctl[i];
        c.kick.fetch_or(bits, SeqCst);
        let h = c.hart.load(Acquire);
        if !h.is_null() {
            // SAFETY: the hart outlives the run. Slow handlers observe the cleared
            // budget immediately, and linked blocks poll at the end of a bounded chain.
            unsafe { (*h).chain_limit.store(0, Release) };
        }
        if let Some(t) = c.thread.get() {
            t.unpark();
        }
    }

    /// Post a supervisor software interrupt for hart `i`.
    pub fn ssip(&self, i: usize) {
        self.ctl[i].ssip.store(true, SeqCst);
        self.kick(i, K_IRQ);
    }

    /// Start stopped hart `i` at `pc` (SBI `hart_start`).
    pub fn start(&self, i: usize, pc: u64, opaque: u64) {
        self.ctl[i].start_pc.store(pc, Relaxed);
        self.ctl[i].start_opaque.store(opaque, Relaxed);
        self.kick(i, K_START);
    }

    /// Recompute every hart's device-driven interrupt inputs and kick the ones that changed.
    /// The lock must be held.
    pub fn update_lines(&self, bus: &mut Bus) {
        for i in 0..self.ctl.len() {
            let msip = bus.clint.msip[i].load(Relaxed) != 0;
            let meip = bus.plic.context_pending(2 * i);
            let seip = bus.plic.context_pending(2 * i + 1);
            let new = msip as u32 | (meip as u32) << 1 | (seip as u32) << 2;
            if self.ctl[i].lines.swap(new, AcqRel) != new {
                self.kick(i, K_IRQ);
            }
        }
    }

    /// Flush the translations of every hart but `me` and wait until they have done it.
    pub fn flush_others(&self, me: &mut Hart) {
        let mut waits = [(usize::MAX, 0u64); 64];
        let mut n = 0;
        for j in 0..self.ctl.len() {
            if j as u64 == me.id {
                continue;
            }
            let seq = self.ctl[j].flush_req.fetch_add(1, AcqRel) + 1;
            self.kick(j, K_FLUSH);
            waits[n] = (j, seq);
            n += 1;
        }
        for &(j, seq) in &waits[..n] {
            let c = &self.ctl[j];
            let mut spins = 0u32;
            while c.flush_ack.load(Acquire) < seq && !c.parked.load(SeqCst) && !self.stop.load(Relaxed) {
                // Two harts fencing each other must both keep serving requests.
                me.serve_flush(self);
                spins += 1;
                if spins < 2000 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }

    /// Sleep until woken, `timeout` passes or a request arrives.
    fn park(&self, i: usize, timeout: Duration) {
        let c = &self.ctl[i];
        c.parked.store(true, SeqCst);
        if c.kick.load(SeqCst) == 0 && !self.stop.load(SeqCst) {
            std::thread::park_timeout(timeout);
        }
        c.parked.store(false, SeqCst);
    }

    /// Ask every hart thread to finish.
    pub fn stop_all(&self) {
        self.stop.store(true, SeqCst);
        for i in 0..self.ctl.len() {
            self.kick(i, K_STOP);
        }
    }
}

impl Hart {
    /// Serve a pending TLB flush request.
    pub(crate) fn serve_flush(&mut self, mt: &Mt) {
        let c = &mt.ctl[self.id as usize];
        let req = c.flush_req.load(Acquire);
        if c.flush_ack.load(Relaxed) != req {
            self.tlb.flush_all();
            self.flush_translation_cache();
            c.flush_ack.store(req, Release);
        }
    }

    /// Serve what other threads asked of this hart and take in the machine's state: timer,
    /// interrupt inputs, software interrupts, a start request.
    fn mt_service(&mut self, bus: &mut Bus, mt: &'static Mt) {
        let i = self.id as usize;
        let c = &mt.ctl[i];
        let k = c.kick.swap(0, AcqRel);
        self.serve_flush(mt);
        if k & K_START != 0 {
            self.stopped = false;
            self.wfi = false;
            self.pc = c.start_pc.load(Relaxed);
            self.set_prv(PRV_S);
            self.x[10] = self.id;
            self.x[11] = c.start_opaque.load(Relaxed);
            self.reservation = None;
            self.flush_translation_cache();
            self.irq_dirty = true;
        }
        if c.ssip.swap(false, AcqRel) {
            self.mip |= MIP_SSIP;
            self.irq_dirty = true;
        }
        let expired = mt.now_ticks() >= bus.clint.mtimecmp[i].load(Relaxed);
        if self.hle {
            // No M-mode firmware: the timer is a supervisor timer interrupt (STIP),
            // which `set_timer` clears.
            if expired {
                self.set_hw_pending(MIP_STIP, true);
            }
        } else {
            self.set_hw_pending(MIP_MTIP, expired);
        }
        let lines = c.lines.load(Acquire);
        self.set_hw_pending(MIP_MSIP, lines & 1 != 0);
        self.set_hw_pending(MIP_MEIP, lines & 2 != 0);
        self.set_hw_pending(MIP_SEIP, lines & 4 != 0);
    }
}

/// Raw pointers handed to a hart thread.
pub(crate) struct HartPtrs {
    pub hart: *mut Hart,
    pub bus: *mut Bus,
}
// SAFETY: the pointers stay valid until the threads are joined; see the module comment for
// what is shared between them.
unsafe impl Send for HartPtrs {}

/// The body of one hart's thread.
pub(crate) fn hart_main(mt: &'static Mt, i: usize, p: HartPtrs) {
    // SAFETY: the hart and the bus outlive the thread; the bus is shared by design.
    let (h, bus) = unsafe { (&mut *p.hart, &mut *p.bus) };
    #[cfg(target_os = "macos")]
    // SAFETY: a plain call that changes the calling thread's scheduling class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
    let _ = mt.ctl[i].thread.set(std::thread::current());
    // When this hart's current idle spell began, and whether the last one was short enough
    // to be worth polling through.
    let mut idle_since: Option<Instant> = None;
    let mut poll = true;
    loop {
        if mt.stop.load(Acquire) {
            break;
        }
        h.mt_service(bus, mt);
        if h.stopped {
            mt.park(i, MAX_PARK);
            continue;
        }
        if h.wfi && h.mip & h.mie == 0 {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if poll && since.elapsed() < POLL {
                std::hint::spin_loop();
                continue;
            }
            // Nothing to do until an interrupt: sleep to the next timer deadline.
            let now = mt.now_ticks();
            let cmp = bus.clint.mtimecmp[i].load(Relaxed);
            let wait = if cmp > now {
                Duration::from_nanos((cmp - now).saturating_mul(1_000_000_000 / TIMEBASE_HZ)).min(MAX_PARK)
            } else {
                MAX_PARK
            };
            mt.park(i, wait);
            continue;
        }
        if let Some(since) = idle_since.take() {
            poll = since.elapsed() < SHORT_IDLE;
        }
        h.run(bus, QUANTUM);
        mt.ctl[i].progress.store(h.instret, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bus the way a multi-threaded run sees it: shared by raw pointer, `mt` set.
    fn threaded_bus() -> Bus {
        let mut bus = Bus::new(16 << 20, 2);
        bus.mt = Some(Box::leak(Box::new(Mt::new(2, 0))));
        bus
    }

    #[test]
    fn atomic_adds_from_several_threads_do_not_lose_updates() {
        let mut bus = threaded_bus();
        let pa = RAM_BASE + 0x1000;
        let p = &mut bus as *mut Bus as usize;
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(move || {
                    // SAFETY: the bus outlives the scope; RAM is shared on purpose.
                    let bus = unsafe { &mut *(p as *mut Bus) };
                    for _ in 0..50_000 {
                        bus.amo::<8>(pa, 0x00, 1).unwrap();
                        bus.amo::<4>(pa + 8, 0x00, 3).unwrap();
                    }
                });
            }
        });
        assert_eq!(bus.read::<8>(pa), Some(200_000));
        assert_eq!(bus.read::<4>(pa + 8), Some(600_000));
    }

    #[test]
    fn compare_and_swap_succeeds_for_exactly_one_thread() {
        let mut bus = threaded_bus();
        let pa = RAM_BASE + 0x2000;
        let p = &mut bus as *mut Bus as usize;
        let wins: usize = std::thread::scope(|s| {
            let hs: Vec<_> = (0..4)
                .map(|i| {
                    s.spawn(move || {
                        // SAFETY: as above.
                        let bus = unsafe { &mut *(p as *mut Bus) };
                        usize::from(bus.cas::<8>(pa, 0, 100 + i).unwrap())
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(wins, 1);
        assert!(matches!(bus.read::<8>(pa), Some(100..=103)));
    }

    #[test]
    fn a_store_into_decoded_code_makes_the_page_stale_and_one_beside_it_does_not() {
        let mut bus = threaded_bus();
        let pa = RAM_BASE + 0x3000;
        let ppn = 0x3;
        bus.mark_code(pa + 16, 8, 0);
        assert!(bus.page_has_code(ppn));
        let (gen, epoch) = (bus.page_gen[ppn].load(Relaxed), bus.code_epoch.load(Relaxed));
        bus.write::<4>(pa + 64, 1).unwrap();
        assert_eq!(bus.page_gen[ppn].load(Relaxed), gen, "a store beside the code leaves it alone");
        bus.write::<4>(pa + 20, 1).unwrap();
        assert_eq!(bus.page_gen[ppn].load(Relaxed), gen.wrapping_add(1));
        assert_eq!(bus.code_epoch.load(Relaxed), epoch.wrapping_add(1));
        assert!(!bus.page_has_code(ppn), "the page is a data page again until it is decoded anew");
    }

    #[test]
    fn marking_logs_the_page_and_the_hart_that_decoded_it() {
        let bus = threaded_bus();
        let before = bus.code_marks.load(Relaxed);
        bus.mark_code(RAM_BASE + 0x5000, 4, 1);
        bus.mark_code(RAM_BASE + 0x5008, 4, 0); // same page: not logged again
        assert_eq!(bus.code_marks.load(Relaxed), before.wrapping_add(1));
        assert_eq!(bus.code_log[before as usize % crate::bus::CODE_LOG].load(Relaxed), 5 | 1 << 26);
    }
}
