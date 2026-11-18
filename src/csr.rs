//! Control and status registers: access checks, WARL write masks and the
//! `sstatus`/`sie`/`sip` views of their M-mode counterparts.

use crate::arch::*;
use crate::bus::Bus;
use crate::hart::{Hart, Res, Trap};

/// Bits visible through `sstatus` on read.
const SSTATUS_READ: u64 = SSTATUS_WMASK | MSTATUS_SD | (2u64 << 32);
/// Supervisor-delegable exception causes (ecall-from-S and ecall-from-M excluded).
const MEDELEG_MASK: u64 = 0xB1FF;
/// Supervisor-delegable interrupt causes.
const MIDELEG_MASK: u64 = MIP_S_MASK;
const SATP_PPN_MASK: u64 = (1 << 44) - 1;

#[inline(always)]
fn illegal() -> Trap {
    Trap::new(EXC_ILLEGAL, 0)
}

impl Hart {
    /// Full `mstatus` value as software observes it.
    pub(crate) fn mstatus_read(&self) -> u64 {
        let mut v = (self.mstatus & MSTATUS_WMASK) | MSTATUS_XL_FIXED;
        if self.mstatus & MSTATUS_FS == MSTATUS_FS {
            v |= MSTATUS_SD;
        }
        v
    }

    /// Permission and existence checks shared by all CSR instructions.
    #[inline]
    pub(crate) fn csr_check(&self, addr: u16, write: bool) -> Res<()> {
        if self.prv < ((addr >> 8) & 3) as u8 {
            return Err(illegal());
        }
        if write && (addr >> 10) & 3 == 3 {
            return Err(illegal());
        }
        match addr {
            CSR_FFLAGS | CSR_FRM | CSR_FCSR if self.mstatus & MSTATUS_FS == 0 => Err(illegal()),
            CSR_SATP if self.prv == PRV_S && self.mstatus & MSTATUS_TVM != 0 => Err(illegal()),
            _ => Ok(()),
        }
    }

    /// Whether unprivileged/supervisor code may read counter `idx` (0 = cycle,
    /// 1 = time, 2 = instret).
    #[inline]
    fn counter_visible(&self, idx: u32) -> bool {
        if self.prv == PRV_M {
            return true;
        }
        if (self.mcounteren >> idx) & 1 == 0 {
            return false;
        }
        self.prv == PRV_S || (self.scounteren >> idx) & 1 != 0
    }

    pub(crate) fn csr_read(&self, bus: &Bus, addr: u16) -> Res<u64> {
        crate::counters::inc(&crate::counters::CSR_READS);
        let v = match addr {
            CSR_FFLAGS => self.fflags | crate::fastfp::native_flags(),
            CSR_FRM => self.frm,
            CSR_FCSR => (self.frm << 5) | self.fflags | crate::fastfp::native_flags(),
            CSR_CYCLE | CSR_TIME | CSR_INSTRET => {
                let idx = (addr - CSR_CYCLE) as u32;
                if !self.counter_visible(idx) {
                    return Err(illegal());
                }
                match idx {
                    // Chain-local progress has not been banked into `instret`
                    // yet; count it so reads stay monotonic.
                    0 => self.counter_at(self.cycle_bias),
                    2 => self.counter_at(self.instret_bias),
                    _ => bus.mt.map_or(bus.mtime, |m| m.now_ticks()),
                }
            }
            0xC03..=0xC1F => {
                if !self.counter_visible(((addr - 0xC00) as u32).min(31)) {
                    return Err(illegal());
                }
                0
            }
            CSR_SSTATUS => self.mstatus_read() & SSTATUS_READ,
            CSR_SIE => self.mie & self.mideleg & MIP_S_MASK,
            CSR_STVEC => self.stvec,
            CSR_SCOUNTEREN => self.scounteren,
            CSR_SENVCFG => self.senvcfg,
            CSR_SSCRATCH => self.sscratch,
            CSR_SEPC => self.sepc,
            CSR_SCAUSE => self.scause,
            CSR_STVAL => self.stval,
            CSR_SIP => self.mip & self.mideleg & MIP_S_MASK,
            CSR_SATP => self.satp,
            CSR_MSTATUS => self.mstatus_read(),
            CSR_MISA => MISA_VALUE,
            CSR_MEDELEG => self.medeleg,
            CSR_MIDELEG => self.mideleg,
            CSR_MIE => self.mie,
            CSR_MTVEC => self.mtvec,
            CSR_MCOUNTEREN => self.mcounteren,
            CSR_MENVCFG => self.menvcfg,
            CSR_MCOUNTINHIBIT => self.mcountinhibit,
            CSR_MSCRATCH => self.mscratch,
            CSR_MEPC => self.mepc,
            CSR_MCAUSE => self.mcause,
            CSR_MTVAL => self.mtval,
            CSR_MIP => self.mip,
            CSR_PMPCFG0 => self.pmpcfg[0],
            CSR_PMPCFG2 => self.pmpcfg[1],
            CSR_PMPADDR0..=CSR_PMPADDR15 => self.pmpaddr[(addr - CSR_PMPADDR0) as usize],
            CSR_MCYCLE => self.counter_at(self.cycle_bias),
            CSR_MINSTRET => self.counter_at(self.instret_bias),
            0xB03..=0xB1F | 0xB23..=0xB3F | 0x323..=0x33F => 0,
            CSR_MVENDORID | CSR_MARCHID | CSR_MIMPID => 0,
            CSR_MHARTID => self.id,
            _ => return Err(illegal()),
        };
        Ok(v)
    }

    pub(crate) fn csr_write(&mut self, addr: u16, val: u64) -> Res<()> {
        crate::counters::inc(&crate::counters::CSR_WRITES);
        match addr {
            CSR_FFLAGS => {
                self.fflags = val & 0x1f;
                crate::fastfp::clear_native_flags();
            }
            CSR_FRM => self.frm = val & 7,
            CSR_FCSR => {
                self.fflags = val & 0x1f;
                self.frm = (val >> 5) & 7;
                crate::fastfp::clear_native_flags();
            }
            CSR_SSTATUS => {
                let old = self.mstatus;
                self.mstatus = (self.mstatus & !SSTATUS_WMASK) | (val & SSTATUS_WMASK);
                self.translation_state_changed(old, self.mstatus);
            }
            CSR_SIE => {
                let m = self.mideleg & MIP_S_MASK;
                self.mie = (self.mie & !m) | (val & m);
            }
            CSR_STVEC => self.stvec = val & !2,
            CSR_SCOUNTEREN => self.scounteren = val & 0xffff_ffff,
            CSR_SENVCFG => self.senvcfg = val,
            CSR_SSCRATCH => self.sscratch = val,
            CSR_SEPC => self.sepc = val & !1,
            CSR_SCAUSE => self.scause = val,
            CSR_STVAL => self.stval = val,
            CSR_SIP => {
                // Only the supervisor software interrupt is writable from S-mode.
                let m = self.mideleg & MIP_SSIP;
                self.mip = (self.mip & !m) | (val & m);
            }
            CSR_SATP => self.write_satp(val),
            CSR_MSTATUS => {
                let mut v = val & MSTATUS_WMASK;
                // MPP = 2 is reserved (no hypervisor mode): WARL to U.
                if (v & MSTATUS_MPP) == (2 << 11) {
                    v &= !MSTATUS_MPP;
                }
                let old = self.mstatus;
                self.mstatus = v;
                self.translation_state_changed(old, v);
            }
            CSR_MISA => {}
            CSR_MEDELEG => self.medeleg = val & MEDELEG_MASK,
            CSR_MIDELEG => self.mideleg = val & MIDELEG_MASK,
            CSR_MIE => self.mie = val & MIE_MASK,
            CSR_MTVEC => self.mtvec = val & !2,
            CSR_MCOUNTEREN => self.mcounteren = val & 0xffff_ffff,
            CSR_MENVCFG => self.menvcfg = val,
            CSR_MCOUNTINHIBIT => self.mcountinhibit = val & 0xffff_ffff,
            CSR_MSCRATCH => self.mscratch = val,
            CSR_MEPC => self.mepc = val & !1,
            CSR_MCAUSE => self.mcause = val,
            CSR_MTVAL => self.mtval = val,
            CSR_MIP => {
                let m = MIP_SSIP | MIP_STIP | MIP_SEIP;
                self.mip = (self.mip & !m) | (val & m);
            }
            CSR_PMPCFG0 => self.pmpcfg[0] = val,
            CSR_PMPCFG2 => self.pmpcfg[1] = val,
            CSR_PMPADDR0..=CSR_PMPADDR15 => self.pmpaddr[(addr - CSR_PMPADDR0) as usize] = val,
            // The explicit value takes precedence over the increment from the
            // CSR instruction itself. Scheduler progress is independent of it.
            CSR_MCYCLE => self.cycle_bias = val.wrapping_sub(self.counter_at(1)),
            CSR_MINSTRET => self.instret_bias = val.wrapping_sub(self.counter_at(1)),
            0xB03..=0xB1F | 0xB23..=0xB3F | 0x323..=0x33F => {}
            _ => return Err(illegal()),
        }
        self.irq_dirty = true;
        self.break_chain();
        Ok(())
    }

    fn write_satp(&mut self, val: u64) {
        let mode = val >> 60;
        // Only bare and Sv39 are implemented; other modes are WARL-ignored.
        if (mode == 0 || mode == 8) && self.satp != val {
            self.satp = (mode << 60) | (((val >> 44) & 0xffff) << 44) | (val & SATP_PPN_MASK);
            self.satp_sv39 = mode == 8;
            self.asid = ((val >> 44) & 0xffff) as u16;
            crate::counters::inc(&crate::counters::SATP_WRITES);
            self.tlb.flush_all();
            self.flush_translation_cache();
        }
    }

    #[inline(always)]
    fn counter_at(&self, bias: u64) -> u64 {
        self.instret.wrapping_add(self.chained).wrapping_add(bias)
    }
}

impl Hart {
    /// React to a write of `mstatus`. `SUM` and `MPRV` only change which cached
    /// translations apply (each data context has its own), so the cached ones stay;
    /// a change of `MXR` invalidates cached paged loads.
    #[inline(always)]
    fn translation_state_changed(&mut self, old: u64, new: u64) {
        let diff = old ^ new;
        if diff & MSTATUS_MXR != 0 {
            self.flush_mxr_loads();
        }
        if diff & (MSTATUS_SUM | MSTATUS_MPRV) != 0 {
            self.select_dctx();
        }
    }
}
