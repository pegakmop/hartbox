//! Platform-Level Interrupt Controller (SiFive-compatible register layout).
//!
//! Context `2*h` is machine mode and `2*h + 1` is supervisor mode for hart
//! `h`. Sources are level-triggered: a claimed source is re-pended on
//! completion if its line is still asserted.

pub const NUM_SOURCES: usize = 64;

pub struct Plic {
    priority: [u32; NUM_SOURCES],
    /// Pending bits, two 32-bit words in one u64.
    pending: u64,
    /// Lines currently asserted by devices.
    level: u64,
    /// Claimed-but-not-completed sources.
    claimed: u64,
    enable: Vec<u64>,
    threshold: Vec<u32>,
}

const PENDING_OFF: u64 = 0x1000;
const ENABLE_OFF: u64 = 0x2000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_OFF: u64 = 0x20_0000;
const CONTEXT_STRIDE: u64 = 0x1000;

impl Plic {
    pub fn new(harts: usize) -> Self {
        let contexts = harts * 2;
        Plic {
            priority: [0; NUM_SOURCES],
            pending: 0,
            level: 0,
            claimed: 0,
            enable: vec![0; contexts],
            threshold: vec![0; contexts],
        }
    }

    /// Drive interrupt source `src` high or low.
    #[inline]
    pub fn set_level(&mut self, src: u32, asserted: bool) {
        let bit = 1u64 << src;
        if asserted {
            self.level |= bit;
        } else {
            self.level &= !bit;
        }
    }

    /// Re-evaluate pending bits from the device lines.
    fn refresh_pending(&mut self) {
        self.pending |= self.level & !self.claimed;
    }

    /// Whether context `ctx` has a deliverable interrupt.
    pub fn context_pending(&mut self, ctx: usize) -> bool {
        self.refresh_pending();
        self.best(ctx).is_some()
    }

    fn best(&self, ctx: usize) -> Option<(u32, u32)> {
        let mut best: Option<(u32, u32)> = None;
        let mut bits = self.pending & self.enable[ctx];
        while bits != 0 {
            let src = bits.trailing_zeros();
            bits &= bits - 1;
            let prio = self.priority[src as usize];
            if prio > self.threshold[ctx] && best.is_none_or(|(_, p)| prio > p) {
                best = Some((src, prio));
            }
        }
        best
    }

    fn claim(&mut self, ctx: usize) -> u32 {
        self.refresh_pending();
        match self.best(ctx) {
            Some((src, _)) => {
                self.pending &= !(1u64 << src);
                self.claimed |= 1u64 << src;
                src
            }
            None => 0,
        }
    }

    fn complete(&mut self, src: u32) {
        if (src as usize) < NUM_SOURCES {
            self.claimed &= !(1u64 << src);
        }
    }

    pub fn read(&mut self, off: u64, size: usize) -> Option<u64> {
        if size != 4 {
            return None;
        }
        let v = if off < PENDING_OFF {
            let src = (off / 4) as usize;
            if src == 0 || src >= NUM_SOURCES {
                0
            } else {
                self.priority[src] as u64
            }
        } else if (PENDING_OFF..PENDING_OFF + 8).contains(&off) {
            let word = (off - PENDING_OFF) / 4;
            self.refresh_pending();
            (self.pending >> (32 * word)) & 0xffff_ffff
        } else if (ENABLE_OFF..CONTEXT_OFF).contains(&off) {
            let ctx = ((off - ENABLE_OFF) / ENABLE_STRIDE) as usize;
            let word = ((off - ENABLE_OFF) % ENABLE_STRIDE) / 4;
            match self.enable.get(ctx) {
                Some(e) if word < 2 => (e >> (32 * word)) & 0xffff_ffff,
                _ => 0,
            }
        } else if off >= CONTEXT_OFF {
            let ctx = ((off - CONTEXT_OFF) / CONTEXT_STRIDE) as usize;
            let reg = (off - CONTEXT_OFF) % CONTEXT_STRIDE;
            if ctx >= self.threshold.len() {
                return Some(0);
            }
            match reg {
                0 => self.threshold[ctx] as u64,
                4 => self.claim(ctx) as u64,
                _ => 0,
            }
        } else {
            0
        };
        Some(v)
    }

    pub fn write(&mut self, off: u64, size: usize, val: u64) -> Option<()> {
        if size != 4 {
            return None;
        }
        let val = val as u32;
        if off < PENDING_OFF {
            let src = (off / 4) as usize;
            if src != 0 && src < NUM_SOURCES {
                self.priority[src] = val & 7;
            }
        } else if (ENABLE_OFF..CONTEXT_OFF).contains(&off) {
            let ctx = ((off - ENABLE_OFF) / ENABLE_STRIDE) as usize;
            let word = ((off - ENABLE_OFF) % ENABLE_STRIDE) / 4;
            if let Some(e) = self.enable.get_mut(ctx) {
                if word < 2 {
                    let mask = 0xffff_ffffu64 << (32 * word);
                    *e = (*e & !mask) | ((val as u64) << (32 * word));
                    // source 0 is hard-wired to zero
                    *e &= !1;
                }
            }
        } else if off >= CONTEXT_OFF {
            let ctx = ((off - CONTEXT_OFF) / CONTEXT_STRIDE) as usize;
            let reg = (off - CONTEXT_OFF) % CONTEXT_STRIDE;
            if let Some(t) = self.threshold.get_mut(ctx) {
                match reg {
                    0 => *t = val & 7,
                    4 => self.complete(val),
                    _ => {}
                }
            }
        }
        Some(())
    }
}
