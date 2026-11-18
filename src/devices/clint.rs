//! Core-Local Interruptor: machine timer (`mtime`/`mtimecmp`) and
//! machine software interrupts (`msip`), one slot per hart.
//!
//! `mtime` itself lives in the bus and is derived from the instruction
//! count; this device only holds the per-hart compare and software-interrupt
//! registers. The machine turns them into `mip` bits at each scheduling tick.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// The registers are atomics because, with one host thread per hart, a hart reads its own
/// compare value without taking the machine lock.
pub struct Clint {
    pub msip: Vec<AtomicU32>,
    pub mtimecmp: Vec<AtomicU64>,
}

const MSIP_OFF: u64 = 0x0000;
const MTIMECMP_OFF: u64 = 0x4000;
const MTIME_OFF: u64 = 0xbff8;

impl Clint {
    pub fn new(harts: usize) -> Self {
        Clint {
            msip: (0..harts).map(|_| AtomicU32::new(0)).collect(),
            mtimecmp: (0..harts).map(|_| AtomicU64::new(u64::MAX)).collect(),
        }
    }

    pub fn read(&self, off: u64, size: usize, mtime: u64) -> Option<u64> {
        let h = |base: u64, stride: u64| -> Option<usize> {
            let idx = (off.checked_sub(base)? / stride) as usize;
            (idx < self.msip.len()).then_some(idx)
        };
        if off < MSIP_OFF + 4 * self.msip.len() as u64 {
            return Some(self.msip[h(MSIP_OFF, 4)?].load(Relaxed) as u64);
        }
        if off >= MTIMECMP_OFF && off < MTIMECMP_OFF + 8 * self.mtimecmp.len() as u64 {
            let idx = h(MTIMECMP_OFF, 8)?;
            let v = self.mtimecmp[idx].load(Relaxed);
            return Some(match size {
                4 if (off - MTIMECMP_OFF).is_multiple_of(8) => v & 0xffff_ffff,
                4 => v >> 32,
                _ => v,
            });
        }
        if off == MTIME_OFF {
            return Some(mtime);
        }
        if off == MTIME_OFF + 4 {
            return Some(mtime >> 32);
        }
        None
    }

    pub fn write(&mut self, off: u64, size: usize, val: u64) -> Option<()> {
        if off < MSIP_OFF + 4 * self.msip.len() as u64 {
            let idx = ((off - MSIP_OFF) / 4) as usize;
            self.msip[idx].store((val & 1) as u32, Relaxed);
            return Some(());
        }
        if off >= MTIMECMP_OFF && off < MTIMECMP_OFF + 8 * self.mtimecmp.len() as u64 {
            let idx = ((off - MTIMECMP_OFF) / 8) as usize;
            let cur = self.mtimecmp[idx].load(Relaxed);
            let rel = (off - MTIMECMP_OFF) % 8;
            self.mtimecmp[idx].store(
                match (size, rel) {
                    (8, 0) => val,
                    (4, 0) => (cur & !0xffff_ffff) | (val & 0xffff_ffff),
                    (4, 4) => (cur & 0xffff_ffff) | (val << 32),
                    _ => return None,
                },
                Relaxed,
            );
            return Some(());
        }
        // `mtime` is read-only from the guest's point of view in this model.
        if off == MTIME_OFF || off == MTIME_OFF + 4 {
            return Some(());
        }
        None
    }
}
