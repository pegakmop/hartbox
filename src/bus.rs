//! Physical address space: RAM, memory-mapped devices, and the bookkeeping
//! that keeps decoded code (basic blocks, see `block.rs`) coherent with RAM.
//!
//! Each RAM page has a bitmap of the halfwords that decoded blocks were built
//! from, and a generation counter. A store that touches a marked halfword bumps
//! the generation, which makes every block of that page stale. Self-modifying
//! code and DMA are therefore correct without extra work in the common case.

use std::sync::atomic::{fence, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering::*};

use crate::arch::*;
use crate::devices::clint::Clint;
use crate::devices::rtc::GoldfishRtc;
use crate::devices::virtio::{VirtioDevice, VirtioMmio};
use crate::devices::plic::Plic;
use crate::devices::uart::Uart;

/// One bit per halfword of a 4 KiB page. Atomic because, with one host thread per hart,
/// several harts mark and test the bits at once.
type CodeBits = [AtomicU64; 32];

/// Whether any halfword in `lo..=hi` (indices within one page) is marked.
#[inline(always)]
fn overlaps(bits: &CodeBits, lo: usize, hi: usize) -> bool {
    (lo >> 6..=hi >> 6).any(|w| bits[w].load(Relaxed) & halfword_mask(w, lo, hi) != 0)
}

#[inline(always)]
fn halfword_mask(word: usize, lo: usize, hi: usize) -> u64 {
    let start = word << 6;
    let first = lo.saturating_sub(start);
    let last = (hi - start).min(63);
    (!0u64 << first) & (!0u64 >> (63 - last))
}

/// Halfwords per 4 KiB page.
const HALFWORDS_PER_PAGE: usize = 2048;

/// Why the machine stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Shutdown(u32),
    Reboot,
    /// Every hart is stopped; nothing can run again.
    Halt,
    /// The configured instruction limit was reached.
    Limit,
    /// Host standard output was closed, so there is nowhere to show output.
    OutputClosed,
}

/// Work that one hart asks the machine to perform between quanta, because it
/// affects other harts.
#[derive(Clone, Copy, Debug)]
pub enum Req {
    /// Hart start via SBI HSM.
    Start { hart: usize, pc: u64, opaque: u64 },
    /// Supervisor software interrupt to a hart (SBI IPI, HLE mode).
    Ssip { hart: usize },
    /// Flush the TLB of every hart (remote `sfence.vma`).
    FlushTlbAll,
}

/// Access to guest RAM by physical address, as seen by DMA-capable devices.
/// Writes go through the same path as CPU stores, so decoded code is kept coherent.
pub trait GuestMem {
    fn mem_read(&self, pa: u64, buf: &mut [u8]) -> bool;
    fn mem_write(&mut self, pa: u64, data: &[u8]) -> bool;
}

impl GuestMem for Bus {
    fn mem_read(&self, pa: u64, buf: &mut [u8]) -> bool {
        self.read_bytes(pa, buf)
    }

    fn mem_write(&mut self, pa: u64, data: &[u8]) -> bool {
        self.write_bytes(pa, data)
    }
}

/// Entries in `Bus::code_log` (a power of two is not required).
pub const CODE_LOG: usize = 256;

pub struct Bus {
    pub ram: Box<[u8]>,
    /// Per RAM page: the halfwords covered by decoded blocks. Allocated on the first mark
    /// and never freed (a hart may be reading it), only cleared.
    code_bits: Vec<AtomicPtr<CodeBits>>,
    /// Per RAM page: 1 while the page holds decoded code (some bit is set).
    pub code_flag: Vec<AtomicU8>,
    /// Per RAM page: incremented whenever code in the page is overwritten.
    pub page_gen: Vec<AtomicU32>,
    /// Bumped whenever any decoded code is invalidated. Block links record it
    /// so a transition needs one global compare instead of a per-page lookup.
    pub code_epoch: AtomicU32,
    /// Bumped whenever a page of RAM is decoded as code for the first time, so a
    /// hart can drop the store translations it holds for that page.
    pub code_marks: AtomicU32,
    /// The pages behind the last `CODE_LOG` bumps of `code_marks` (entry `n % CODE_LOG`
    /// is the page of bump `n` in the low 26 bits and the marking hart above), so a hart
    /// drops only the store translations of those pages instead of all of them.
    pub code_log: [AtomicU32; CODE_LOG],
    /// Serialises marking and forgetting decoded code (multi-threaded harts only).
    code_lock: crate::mt::SpinLock,
    /// Multi-threaded harts: the shared control block. `None` while one host thread runs
    /// every hart.
    pub mt: Option<&'static crate::mt::Mt>,
    pub clint: Clint,
    pub plic: Plic,
    pub uart: Uart,
    pub rtc: GoldfishRtc,
    /// virtio-mmio slots in attachment order.
    pub virtio: Vec<Option<Box<VirtioMmio>>>,
    /// Current machine timer value, refreshed by the machine every tick.
    pub mtime: u64,
    pub reqs: Vec<Req>,
    pub exit: Option<Exit>,
    /// SBI HSM state of each hart: true while stopped.
    pub hart_stopped: Vec<bool>,
}

impl Drop for Bus {
    fn drop(&mut self) {
        for slot in &mut self.code_bits {
            let p = *slot.get_mut();
            if !p.is_null() {
                // SAFETY: only `bits_of` allocates these boxes. No hart can
                // access the bus while it is being dropped.
                unsafe { drop(Box::from_raw(p)) };
            }
        }
    }
}

impl Bus {
    pub fn new(ram_size: usize, harts: usize) -> Self {
        assert!(ram_size.is_multiple_of(4096) && ram_size > 0, "RAM size must be a non-zero multiple of 4 KiB");
        Bus {
            ram: vec![0u8; ram_size].into_boxed_slice(),
            code_bits: (0..ram_size / 4096).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
            code_flag: (0..ram_size / 4096).map(|_| AtomicU8::new(0)).collect(),
            page_gen: (0..ram_size / 4096).map(|_| AtomicU32::new(0)).collect(),
            code_epoch: AtomicU32::new(0),
            code_marks: AtomicU32::new(0),
            code_log: [const { AtomicU32::new(0) }; CODE_LOG],
            code_lock: crate::mt::SpinLock::new(),
            mt: None,
            clint: Clint::new(harts),
            plic: Plic::new(harts),
            uart: Uart::new(),
            rtc: GoldfishRtc::default(),
            virtio: Vec::new(),
            mtime: 0,
            reqs: Vec::new(),
            exit: None,
            hart_stopped: vec![false; harts],
        }
    }

    #[inline(always)]
    pub fn ram_offset(pa: u64) -> usize {
        pa.wrapping_sub(RAM_BASE) as usize
    }

    /// Whether RAM page `ppn` holds decoded code.
    #[inline(always)]
    pub fn page_has_code(&self, ppn: usize) -> bool {
        self.code_flag[ppn].load(Relaxed) != 0
    }

    /// The code bitmap of page `ppn`, allocated on first use. Call with `code_lock` held.
    fn bits_of(&self, ppn: usize) -> &CodeBits {
        let mut p = self.code_bits[ppn].load(Acquire);
        if p.is_null() {
            p = Box::into_raw(Box::new([const { AtomicU64::new(0) }; 32]));
            self.code_bits[ppn].store(p, Release);
        }
        // SAFETY: the box is never freed.
        unsafe { &*p }
    }

    /// Record that the bytes `[pa, pa+len)` were decoded into a block by hart `hart`.
    ///
    /// SMP decoders read the bytes again after publishing the marks. A concurrent store
    /// either sees the marks or its changed bytes are found by that second read.
    pub fn mark_code(&self, pa: u64, len: u64, hart: u64) {
        if len == 0 {
            return;
        }
        let _g = self.mt.map(|_| self.code_lock.lock());
        let mut start = Self::ram_offset(pa) >> 1;
        let end = (Self::ram_offset(pa) + len as usize).div_ceil(2);
        while start < end {
            let (ppn, lo) = (start / HALFWORDS_PER_PAGE, start % HALFWORDS_PER_PAGE);
            let page_end = ((ppn + 1) * HALFWORDS_PER_PAGE).min(end);
            let hi = (page_end - 1) % HALFWORDS_PER_PAGE;
            let bits = self.bits_of(ppn);
            for word in lo >> 6..=hi >> 6 {
                let mask = halfword_mask(word, lo, hi);
                let before = bits[word].load(Relaxed);
                let after = before | mask;
                if before != after {
                    // Other bitmap writers hold the same lock in SMP mode.
                    bits[word].store(after, Relaxed);
                }
            }
            if self.code_flag[ppn].load(Relaxed) == 0 {
                self.code_flag[ppn].store(1, Relaxed);
                let n = self.code_marks.load(Relaxed);
                self.code_log[n as usize % CODE_LOG].store(ppn as u32 | (hart as u32) << 26, Relaxed);
                self.code_marks.store(n.wrapping_add(1), Release);
            }
            start = page_end;
        }
        if self.mt.is_some() {
            fence(SeqCst);
        }
    }

    /// Read the generation after any in-progress invalidation has published its epoch.
    /// The block builder uses this before reclaiming the previous generation's blocks.
    pub(crate) fn code_generation(&self, ppn: usize) -> u32 {
        let _g = self.mt.map(|_| self.code_lock.lock());
        self.page_gen[ppn].load(Relaxed)
    }

    /// Note a write to `[off, off+len)`: if it touches decoded code, the
    /// affected pages' blocks become stale. Call it after the bytes are written.
    #[inline(always)]
    fn invalidate_code(&self, off: usize, len: usize) {
        let (ppn, in_page) = (off >> 12, off & 0xfff);
        if in_page + len > 4096 {
            return self.invalidate_code_slow(off, len);
        }
        if self.mt.is_some() {
            // Store, then look at the marks; the marking hart publishes the marks and
            // then reads the bytes: one of the two sees the other.
            fence(SeqCst);
        }
        if self.page_has_code(ppn) {
            self.forget_if_overlapping(ppn, in_page >> 1, (in_page + len - 1) >> 1);
        }
    }

    #[cold]
    #[inline(never)]
    fn forget_if_overlapping(&self, ppn: usize, lo: usize, hi: usize) {
        let _g = self.mt.map(|_| self.code_lock.lock());
        let p = self.code_bits[ppn].load(Acquire);
        // SAFETY: a non-null bitmap is never freed.
        if let Some(bits) = unsafe { p.as_ref() } {
            if self.page_has_code(ppn) && overlaps(bits, lo, hi) {
                self.forget_locked(ppn);
            }
        }
    }

    /// A write touched decoded code in page `ppn`: bump its generation so its
    /// blocks are rebuilt, and drop the bitmap until they are decoded again.
    fn forget_locked(&self, ppn: usize) {
        crate::counters::inc(&crate::counters::FORGET_CODE);
        // No decoded code is left in the page, so it is a plain data page again: stores
        // may use the flat store TLB for it (see `Hart::ftlb_fill`). Decoding code in it
        // again marks it anew and bumps `code_marks`.
        self.code_flag[ppn].store(0, Relaxed);
        // SAFETY: a non-null bitmap is never freed.
        if let Some(bits) = unsafe { self.code_bits[ppn].load(Acquire).as_ref() } {
            for w in bits {
                w.store(0, Relaxed);
            }
        }
        self.page_gen[ppn].store(self.page_gen[ppn].load(Relaxed).wrapping_add(1), Release);
        self.code_epoch.store(self.code_epoch.load(Relaxed).wrapping_add(1), Release);
    }

    /// Make every block decoded from page `ppn` stale (used by a hart that finds it held
    /// store translations for a page another hart has since decoded as code).
    #[cold]
    #[inline(never)]
    pub(crate) fn forget_code(&self, ppn: usize) {
        let _g = self.mt.map(|_| self.code_lock.lock());
        if self.page_has_code(ppn) {
            self.forget_locked(ppn);
        }
    }

    /// A write that crosses a page boundary (only from bulk copies).
    #[cold]
    #[inline(never)]
    fn invalidate_code_slow(&self, off: usize, len: usize) {
        if self.mt.is_some() {
            fence(SeqCst);
        }
        let end = off + len - 1;
        for ppn in (off >> 12)..=(end >> 12) {
            if !self.page_has_code(ppn) {
                continue;
            }
            let lo = if ppn == off >> 12 { (off & 0xfff) >> 1 } else { 0 };
            let hi = if ppn == end >> 12 { (end & 0xfff) >> 1 } else { HALFWORDS_PER_PAGE - 1 };
            self.forget_if_overlapping(ppn, lo, hi);
        }
    }

    /// Read `N` bytes (1, 2, 4 or 8) from physical address `pa`, little-endian.
    #[inline(always)]
    pub fn read<const N: usize>(&mut self, pa: u64) -> Option<u64> {
        let off = Self::ram_offset(pa);
        if off <= self.ram.len() - N {
            // SAFETY: `off + N` is inside the RAM allocation. One access of the natural
            // width keeps an aligned word untorn when other harts write it.
            return Some(unsafe { crate::block::host_read::<N>(self.ram.as_ptr() as usize + off) });
        }
        self.mmio_read(pa, N)
    }

    /// Write the low `N` bytes of `v` to physical address `pa`.
    #[inline(always)]
    pub fn write<const N: usize>(&mut self, pa: u64, v: u64) -> Option<()> {
        let off = Self::ram_offset(pa);
        if off <= self.ram.len() - N {
            // SAFETY: as in `read`.
            unsafe { crate::block::host_write::<N>(self.ram.as_ptr() as usize + off, v) };
            self.invalidate_code(off, N);
            return Some(());
        }
        self.mmio_write(pa, N, v)
    }

    /// Host address of the `N` bytes at RAM physical address `pa`, if they are plain RAM.
    #[inline(always)]
    pub fn ram_ptr<const N: usize>(&self, pa: u64) -> Option<usize> {
        let off = Self::ram_offset(pa);
        (off <= self.ram.len() - N).then(|| self.ram.as_ptr() as usize + off)
    }

    /// Update accessed/dirty bits only if the PTE is still the one the walker read.
    /// Page tables must be in RAM for an atomic hardware update.
    pub(crate) fn update_pte(&mut self, pa: u64, old: u64, new: u64) -> Option<bool> {
        let p = self.ram_ptr::<8>(pa)?;
        if p & 7 != 0 {
            return self.cas::<8>(pa, old, new);
        }
        // SAFETY: aligned and within RAM. AcqRel keeps the update before the
        // translated access on the calling hart.
        let changed = unsafe { AtomicU64::from_ptr(p as *mut u64) }
            .compare_exchange(old.to_le(), new.to_le(), AcqRel, Relaxed)
            .is_ok();
        if changed {
            self.invalidate_code(Self::ram_offset(pa), 8);
        }
        Some(changed)
    }

    /// Atomic read-modify-write of `N` (4 or 8) bytes in RAM or, failing that, a plain one:
    /// `imm` is the AMO's funct5. Returns the old value, sign-extended from 32 bits for
    /// `N == 4`.
    pub fn amo<const N: usize>(&mut self, pa: u64, imm: i32, src: u64) -> Option<u64> {
        match self.ram_ptr::<N>(pa) {
            // SAFETY: the caller checked alignment; the address is inside RAM.
            Some(p) if p & (N - 1) == 0 => {
                let old = unsafe { crate::block::amo_host(p, N == 4, imm, src) };
                self.invalidate_code(Self::ram_offset(pa), N);
                Some(old)
            }
            _ => {
                let old = self.read::<N>(pa)?;
                let old = if N == 4 { old as u32 as i32 as i64 as u64 } else { old };
                let s = if N == 4 { src as u32 as i32 as i64 as u64 } else { src };
                self.write::<N>(pa, crate::block::amo_value(imm, old, s))?;
                Some(old)
            }
        }
    }

    /// Store-conditional: write `new` if the `N` bytes at `pa` still hold `expect`.
    pub fn cas<const N: usize>(&mut self, pa: u64, expect: u64, new: u64) -> Option<bool> {
        match self.ram_ptr::<N>(pa) {
            // SAFETY: the caller checked alignment; the address is inside RAM.
            Some(p) if p & (N - 1) == 0 => {
                let ok = unsafe { crate::block::cas_host(p, N == 4, expect, new) };
                if ok {
                    self.invalidate_code(Self::ram_offset(pa), N);
                }
                Some(ok)
            }
            _ => {
                if self.read::<N>(pa)? != expect {
                    return Some(false);
                }
                self.write::<N>(pa, new)?;
                Some(true)
            }
        }
    }

    /// Copy bytes out of RAM. Returns `false` if any byte is outside RAM.
    pub fn read_bytes(&self, pa: u64, buf: &mut [u8]) -> bool {
        let off = Self::ram_offset(pa);
        match self.ram.get(off..off.wrapping_add(buf.len())) {
            Some(src) if off <= self.ram.len() => {
                buf.copy_from_slice(src);
                true
            }
            _ => false,
        }
    }

    /// Copy bytes into RAM (used by loaders and device DMA).
    pub fn write_bytes(&mut self, pa: u64, data: &[u8]) -> bool {
        let off = Self::ram_offset(pa);
        if off > self.ram.len() || data.len() > self.ram.len() - off {
            return false;
        }
        if !data.is_empty() {
            self.ram[off..off + data.len()].copy_from_slice(data);
            self.invalidate_code(off, data.len());
        }
        true
    }

    /// Attach a virtio device and return its slot.
    pub fn attach_virtio(&mut self, dev: Box<dyn VirtioDevice>) -> usize {
        let slot = self.virtio.len();
        assert!(slot < VIRTIO_SLOTS, "too many virtio devices");
        let irq = VIRTIO_IRQ_BASE + slot as u32;
        self.virtio.push(Some(Box::new(VirtioMmio::new(dev, irq))));
        slot
    }

    /// The device in `slot`, if it has type `T`.
    pub fn device_mut<T: VirtioDevice>(&mut self, slot: usize) -> Option<&mut T> {
        self.virtio.get_mut(slot)?.as_mut()?.dev.as_any().downcast_mut::<T>()
    }

    /// Let a device do background work, such as delivering queued frames.
    pub fn virtio_poll(&mut self, slot: usize) {
        if let Some(mut mmio) = self.virtio.get_mut(slot).and_then(Option::take) {
            mmio.poll(self);
            self.virtio[slot] = Some(mmio);
        }
    }

    fn virtio_slot(pa: u64) -> Option<usize> {
        (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_SLOTS as u64 * VIRTIO_STRIDE)
            .contains(&pa)
            .then(|| ((pa - VIRTIO_BASE) / VIRTIO_STRIDE) as usize)
    }

    /// Drive the PLIC lines of the RTC and every virtio device from their state.
    pub fn update_device_irqs(&mut self) {
        self.plic.set_level(RTC_IRQ, self.rtc.irq_asserted());
        for mmio in self.virtio.iter().flatten() {
            self.plic.set_level(mmio.irq, mmio.irq_asserted());
        }
    }

    /// Device registers are shared by all harts; with several host threads every access
    /// holds the machine lock and then re-evaluates the interrupt lines it may have moved.
    #[cold]
    fn mmio_read(&mut self, pa: u64, size: usize) -> Option<u64> {
        let Some(mt) = self.mt else { return self.mmio_read_inner(pa, size) };
        let _g = mt.lock.lock();
        let r = self.mmio_read_inner(pa, size);
        self.refresh_irq_lines(mt);
        r
    }

    #[cold]
    fn mmio_write(&mut self, pa: u64, size: usize, val: u64) -> Option<()> {
        let Some(mt) = self.mt else { return self.mmio_write_inner(pa, size, val) };
        let _g = mt.lock.lock();
        let r = self.mmio_write_inner(pa, size, val);
        self.refresh_irq_lines(mt);
        r
    }

    /// Recompute the device interrupt lines and tell the harts whose inputs changed.
    /// The machine lock must be held.
    pub fn refresh_irq_lines(&mut self, mt: &'static crate::mt::Mt) {
        self.uart_irq_update();
        self.update_device_irqs();
        mt.update_lines(self);
    }

    fn mmio_read_inner(&mut self, pa: u64, size: usize) -> Option<u64> {
        crate::counters::inc(&crate::counters::MMIO_READS);
        if (RTC_BASE..RTC_BASE + RTC_SIZE).contains(&pa) {
            return Some(self.rtc.read(pa - RTC_BASE));
        }
        if let Some(slot) = Self::virtio_slot(pa) {
            let off = (pa - VIRTIO_BASE) % VIRTIO_STRIDE;
            return Some(self.virtio.get(slot).and_then(|d| d.as_ref()).map_or(0, |m| m.read(off, size)));
        }
        if (CLINT_BASE..CLINT_BASE + CLINT_SIZE).contains(&pa) {
            let now = self.mt.map_or(self.mtime, |m| m.now_ticks());
            return self.clint.read(pa - CLINT_BASE, size, now);
        }
        if (PLIC_BASE..PLIC_BASE + PLIC_SIZE).contains(&pa) {
            return self.plic.read(pa - PLIC_BASE, size);
        }
        if (UART_BASE..UART_BASE + UART_SIZE).contains(&pa) {
            return self.uart.read(pa - UART_BASE, size);
        }
        if (SIFIVE_TEST_BASE..SIFIVE_TEST_BASE + SIFIVE_TEST_SIZE).contains(&pa) {
            return Some(0);
        }
        None
    }

    fn mmio_write_inner(&mut self, pa: u64, size: usize, val: u64) -> Option<()> {
        crate::counters::inc(&crate::counters::MMIO_WRITES);
        if (RTC_BASE..RTC_BASE + RTC_SIZE).contains(&pa) {
            self.rtc.write(pa - RTC_BASE, val);
            return Some(());
        }
        if let Some(slot) = Self::virtio_slot(pa) {
            let off = (pa - VIRTIO_BASE) % VIRTIO_STRIDE;
            if let Some(mut mmio) = self.virtio.get_mut(slot).and_then(Option::take) {
                mmio.write(off, size, val, self);
                self.virtio[slot] = Some(mmio);
            }
            return Some(());
        }
        if (CLINT_BASE..CLINT_BASE + CLINT_SIZE).contains(&pa) {
            return self.clint.write(pa - CLINT_BASE, size, val);
        }
        if (PLIC_BASE..PLIC_BASE + PLIC_SIZE).contains(&pa) {
            return self.plic.write(pa - PLIC_BASE, size, val);
        }
        if (UART_BASE..UART_BASE + UART_SIZE).contains(&pa) {
            return self.uart.write(pa - UART_BASE, size, val);
        }
        if (SIFIVE_TEST_BASE..SIFIVE_TEST_BASE + SIFIVE_TEST_SIZE).contains(&pa) {
            // Test finisher: lets bare-metal programs power the machine off.
            match val as u32 & 0xffff {
                0x5555 => self.exit = Some(Exit::Shutdown(0)),
                0x3333 => self.exit = Some(Exit::Shutdown(val as u32 >> 16)),
                0x7777 => self.exit = Some(Exit::Reboot),
                _ => {}
            }
            return Some(());
        }
        None
    }
}
