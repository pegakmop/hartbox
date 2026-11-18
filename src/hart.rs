//! Hart (hardware thread) state, the fetch/execute loop, and trap delivery.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::arch::*;
use crate::bus::Bus;
use crate::block::{BlockArena, BlockPage, PendingLink};
use crate::mmu::{Superpages, Tlb};

/// A synchronous exception or an interrupt, with its trap value.
#[derive(Clone, Copy, Debug)]
///
/// The cause is an exception code (0..=15) stored as a byte, which keeps
/// `Result<_, Trap>` small enough to be returned in registers.
pub struct Trap {
    pub cause: u8,
    pub tval: u64,
}

impl Trap {
    #[inline(always)]
    pub fn new(cause: u64, tval: u64) -> Self {
        Trap { cause: cause as u8, tval }
    }
}

pub type Res<T> = Result<T, Trap>;

// The layout is fixed: the fields the block engine touches on every
// instruction come first, so they are all within reach of one immediate offset.
#[repr(C)]
pub struct Hart {
    pub x: [u64; 32],
    /// Raw FP register contents (NaN-boxed for single precision).
    pub f: [u64; 32],
    pub pc: u64,
    /// Instructions executed within the current chain, materialized for CSR
    /// access. Outside cold execution, x86-64 uses it for the remaining budget.
    pub(crate) chained: u64,
    pub(crate) chain_budget: u64,
    pub(crate) chain_limit: AtomicU64,
    /// Return-address stack: where recent calls will come back to, and the block
    /// engine's cached successor for each (see `block::finish_ret`). It is only a
    /// prediction, checked against the real target at every return.
    pub(crate) ras: [RasEntry; RAS_SIZE],
    pub(crate) ras_sp: usize,
    /// The flat data TLBs for the current context (privilege, `SUM`, `MPRV` and
    /// paging decide which; see `select_dctx`): one entry per 4 KiB page of RAM that
    /// a plain access can reach, indexed by the low bits of the page number. A hit
    /// is a tag compare and an add (see `FastTlb`).
    pub(crate) ltlb_cur: *mut FastTlb,
    pub(crate) stlb_cur: *mut FastTlb,
    /// Direct-mapped cache of the blocks this hart entered recently, keyed by pc and
    /// link stamp (see `block::BEntry`). `finish` probes it when a block's own link
    /// slots miss, so an indirect jump with many targets stays in the chain.
    pub(crate) bcache_ptr: *mut crate::block::BEntry,
    /// `Bus::code_marks` as of the last time this hart looked (see `sync_code_marks`).
    pub(crate) seen_code_marks: u32,
    /// One bit per RAM page that a flat store TLB entry may map (set when an entry is
    /// filled, cleared when the page is dropped): `drop_store_page` skips pages with no bit.
    pub(crate) stlb_pages: Vec<u64>,
    /// Decode buffer of `build_block`, kept so a block build does not allocate.
    pub(crate) build_ops: Vec<crate::insn::DInsn>,
    pub(crate) decode_cache: crate::insn::DecodeCache,
    /// Current data context.
    pub(crate) dctx: u8,
    /// Virtual pc the current block was entered at, for targets that keep it in the hart.
    pub(crate) block_pc: u64,
    pub(crate) pending_link: Option<PendingLink>,
    #[cfg(feature = "counters")]
    pub(crate) in_block: bool,
    /// The translation epoch and fetch privilege, with the code epoch at
    /// the start of the active chain in the low 32 bits.
    pub(crate) cache_stamp: u64,
    /// How data accesses are translated, as a summary for the block engine:
    /// `DATA_IDENTITY`, `DATA_PAGED` (use the micro-TLB) or `DATA_SLOW`.
    pub data_mode: u8,
    pub prv: u8,
    /// Halted by SBI `hart_stop`; the machine will not schedule it.
    pub stopped: bool,
    pub wfi: bool,
    /// Running under the built-in SBI (no M-mode firmware present).
    pub hle: bool,
    /// Set whenever something that can change interrupt delivery happens.
    pub irq_dirty: bool,
    pub id: u64,
    /// Host scheduler accounting. Guest counter writes never change it.
    pub instret: u64,
    pub(crate) instret_bias: u64,
    pub(crate) cycle_bias: u64,
    /// Backing store of every context's flat TLBs: `DCTX` contexts, each a load
    /// table and then a store table of `FTLB_SETS` entries.
    pub(crate) ftlb_store: Box<[FastTlb]>,
    /// Populated groups in each load and store table.
    pub(crate) ftlb_dirty: [u64; 2 * DCTX],
    pub(crate) load_victim: [[FastTlb; VICTIM_SETS]; DCTX],
    pub(crate) store_victim: [[FastTlb; VICTIM_SETS]; DCTX],
    /// Backing store of `bcache_ptr`.
    pub(crate) bcache: Box<[crate::block::BEntry]>,

    pub mstatus: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mie: u64,
    pub mip: u64,
    pub mtvec: u64,
    pub mcounteren: u64,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub stvec: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub scounteren: u64,
    pub satp: u64,
    pub satp_sv39: bool,
    pub asid: u16,
    pub menvcfg: u64,
    pub senvcfg: u64,
    pub mcountinhibit: u64,
    pub pmpcfg: [u64; 2],
    pub pmpaddr: [u64; 16],
    pub fflags: u64,
    pub frm: u64,

    pub reservation: Option<u64>,
    /// What `lr` read (zero-extended): with several host threads `sc` is a compare-and-swap
    /// against it.
    pub reservation_val: u64,
    /// This hart's control block when harts run on separate host threads, else null.
    pub(crate) mt_ctl: *const crate::mt::HartCtl,
    pub tlb: Tlb,
    /// Instruction-fetch shortcuts for U and S mode. Permission is checked
    /// before filling either slot, and M-mode fetches bypass the MMU.
    pub(crate) ifetch: [MicroTlb; 2],
    /// Decoded basic blocks, per RAM page (see `block.rs`).
    pub(crate) blocks: Vec<Option<Box<BlockPage>>>,
    pub(crate) block_arena: BlockArena,
    /// One bit (hashed from the virtual page number) for every virtual page that a
    /// pc-keyed cache entry (a block link, a block-cache entry, a return-stack entry)
    /// was made for since translations were last discarded. An `sfence.vma` for a page
    /// whose bit is clear has no such entry to kill (see `Hart::sfence_vma`).
    pub(crate) link_pages: [u64; LINK_PAGE_WORDS],
    /// Large leaves that may have populated flat translations or block links.
    pub(crate) superpages: Superpages,
    /// Reverse hint for a RAM page's store translation: store-only slot + 1, or
    /// u16::MAX for aliases. Valid only while its stlb_pages bit is set.
    pub(crate) stlb_slots: Vec<u16>,
}

/// Words in `Hart::link_pages`: 64 Ki bits.
pub(crate) const LINK_PAGE_WORDS: usize = 1024;

/// Entries in the return-address stack (a power of two).
pub(crate) const RAS_SIZE: usize = 16;

/// One predicted return: the continuation pc after a call, the link stamp it was
/// pushed under, the call's terminator, whose `link` caches the continuation block, and a
/// copy of that link taken when the call was made (0 if the block was not known yet), so a
/// return finds the block in the entry itself instead of through the terminator.
#[derive(Clone, Copy)]
pub(crate) struct RasEntry {
    pub(crate) pc: u64,
    pub(crate) stamp: u64,
    pub(crate) term: *const crate::insn::DInsn,
    pub(crate) next: usize,
}

impl RasEntry {
    pub(crate) const EMPTY: RasEntry = RasEntry { pc: u64::MAX, stamp: u64::MAX, term: std::ptr::null(), next: 0 };
}

/// Entries in each flat data TLB (a power of two).
pub const FTLB_SETS: usize = 8192;
/// Slots for recently displaced flat translations, per table and data context.
pub(crate) const VICTIM_SETS: usize = 16;
/// Data contexts, each with its own flat TLBs, so a change of privilege only
/// switches tables instead of dropping what was cached: user, supervisor,
/// supervisor with `SUM`, and identity (M-mode or paging off). `MPRV` uses
/// a shared empty table and never installs a fast mapping.
pub const DCTX: usize = 4;
// Store reverse hints encode a table slot plus one in a u16; MAX marks aliases.
const _: () = assert!(FTLB_SETS.is_power_of_two() && DCTX * FTLB_SETS < u16::MAX as usize);
pub const DCTX_USER: u8 = 0;
pub const DCTX_SUPERVISOR: u8 = 1;
pub const DCTX_SUM: u8 = 2;
pub const DCTX_IDENTITY: u8 = 3;
pub const DCTX_MPRV: u8 = 4;

/// One entry of a flat data TLB. `tag` is the virtual address of a 4 KiB page, and
/// `addend` is what turns a virtual address in that page into a host address:
/// `host = va + addend` (wrapping). A plain access of `N` bytes hits when
/// `va & (!0xfff | (N - 1)) == tag`, so a misaligned access never hits and takes
/// the general path, and nothing else needs to be checked: an entry exists only
/// for a page of RAM that the current privilege may access in this way (for the
/// store table, that holds no decoded code). `tag` is all ones when empty.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct FastTlb {
    pub tag: u64,
    pub addend: u64,
}

pub(crate) static EMPTY_FAST_TLB: [FastTlb; 2 * FTLB_SETS] = [FastTlb::EMPTY; 2 * FTLB_SETS];

impl FastTlb {
    pub const EMPTY: FastTlb = FastTlb { tag: u64::MAX, addend: 0 };
}

/// Slot of the page holding `va` in a flat data TLB.
#[inline(always)]
pub fn ftlb_index(va: u64) -> usize {
    ((va >> 12) as usize) & (FTLB_SETS - 1)
}

/// `Hart::data_mode`: data addresses are identical to physical ones.
pub const DATA_IDENTITY: u8 = 0;
/// `Hart::data_mode`: data addresses are translated through the micro-TLB.
pub const DATA_PAGED: u8 = 1;
/// `Hart::data_mode`: MPRV is set, so data accesses take the general path.
pub const DATA_SLOW: u8 = 2;

/// One-entry translation cache for a single access class. It holds one
/// mapping, which may be a superpage: a virtual page matches when its bits
/// above `mask` equal `tag`, and its physical page is `ppn | (vpn & mask)`.
#[derive(Clone, Copy)]
pub struct MicroTlb {
    pub tag: u64,
    pub mask: u64,
    pub ppn: u64,
}

impl MicroTlb {
    pub const EMPTY: MicroTlb = MicroTlb { tag: u64::MAX, mask: 0, ppn: 0 };
}

impl Hart {
    pub fn new(id: u64, hle: bool) -> Self {
        let mut h = Hart {
            id,
            x: [0; 32],
            f: [0; 32],
            pc: 0,
            prv: PRV_M,
            stopped: false,
            wfi: false,
            hle,
            instret: 0,
            instret_bias: 0,
            cycle_bias: 0,
            irq_dirty: false,
            mstatus: 0,
            medeleg: 0,
            mideleg: 0,
            mie: 0,
            mip: 0,
            mtvec: 0,
            mcounteren: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            stvec: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            scounteren: 0,
            satp: 0,
            satp_sv39: false,
            asid: 0,
            menvcfg: 0,
            senvcfg: 0,
            mcountinhibit: 0,
            pmpcfg: [0; 2],
            pmpaddr: [0; 16],
            fflags: 0,
            frm: 0,
            reservation: None,
            reservation_val: 0,
            mt_ctl: std::ptr::null(),
            tlb: Tlb::new(),
            ifetch: [MicroTlb::EMPTY; 2],
            cache_stamp: 0,
            data_mode: DATA_IDENTITY,
            pending_link: None,
            #[cfg(feature = "counters")]
            in_block: false,
            block_pc: 0,
            chained: 0,
            chain_budget: 0,
            chain_limit: AtomicU64::new(0),
            blocks: Vec::new(),
            block_arena: BlockArena::default(),
            ras: [RasEntry::EMPTY; RAS_SIZE],
            ras_sp: 0,
            link_pages: [0; LINK_PAGE_WORDS],
            superpages: Superpages::default(),
            ltlb_cur: std::ptr::null_mut(),
            stlb_cur: std::ptr::null_mut(),
            bcache_ptr: std::ptr::null_mut(),
            seen_code_marks: 0,
            stlb_pages: Vec::new(),
            stlb_slots: Vec::new(),
            build_ops: Vec::with_capacity(65),
            decode_cache: crate::insn::DecodeCache::default(),
            dctx: 0,
            ftlb_store: vec![FastTlb::EMPTY; DCTX * 2 * FTLB_SETS].into_boxed_slice(),
            ftlb_dirty: [0; 2 * DCTX],
            load_victim: [[FastTlb::EMPTY; VICTIM_SETS]; DCTX],
            store_victim: [[FastTlb::EMPTY; VICTIM_SETS]; DCTX],
            bcache: vec![crate::block::BEntry::EMPTY; crate::block::BCACHE_SIZE].into_boxed_slice(),
        };
        h.bcache_ptr = h.bcache.as_mut_ptr();
        h.select_dctx();
        h
    }

    /// Change the privilege level and pick the data context that goes with it.
    #[inline(always)]
    pub(crate) fn set_prv(&mut self, prv: u8) {
        self.prv = prv;
        self.select_dctx();
    }



    /// Run until the budget is consumed or the hart idles or stops.
    /// Returns scheduler progress, including faulting instruction attempts.
    pub fn run(&mut self, bus: &mut Bus, budget: u64) -> u64 {
        let base = 0u8;
        crate::counters::stack_base(&base as *const u8 as usize);
        let start = self.instret;
        let mut left = budget;
        // The native floating-point steps leave their exception flags in the host's `FPSR`
        // (see `fastfp.rs`): start from a clean one, and take them into `fflags` at the end.
        crate::fastfp::clear_native_flags();
        while left != 0 {
            // Another hart asked for something (an interrupt, a TLB flush, a stop): hand
            // control back to the hart's thread, which serves it.
            if !self.mt_ctl.is_null() {
                // SAFETY: `mt_ctl` points into the machine's control block, which outlives the run.
                if unsafe { (*self.mt_ctl).kick.load(std::sync::atomic::Ordering::Relaxed) } != 0 {
                    break;
                }
            }
            self.sync_code_marks(bus);
            if self.irq_dirty || self.wfi || self.stopped {
                if self.irq_dirty {
                    self.irq_dirty = false;
                    if let Some(cause) = self.pending_interrupt() {
                        self.take_trap(cause, 0);
                    }
                }
                if self.wfi {
                    if self.mip & self.mie == 0 {
                        break;
                    }
                    self.wfi = false;
                }
                if self.stopped {
                    break;
                }
            }
            match self.next_block(bus) {
                Ok(Some(block)) => {
                    // SAFETY: `next_block` returns a block that is allocated now.
                    let used = unsafe { self.exec_block(bus, block, left) };
                    left = left.saturating_sub(used);
                }
                Ok(None) => {
                    self.step_uncached(bus);
                    left -= 1;
                }
                Err(t) => {
                    self.take_uncached_fault(t);
                    left -= 1;
                }
            }
        }
        self.fflags |= crate::fastfp::native_flags();
        crate::fastfp::clear_native_flags();
        self.chained = 0;
        self.instret.wrapping_sub(start)
    }

    /// Charge a fault against the scheduler without incrementing guest minstret.
    pub(crate) fn take_uncached_fault(&mut self, trap: Trap) {
        self.instret = self.instret.wrapping_add(1);
        self.instret_bias = self.instret_bias.wrapping_sub(1);
        self.take_trap(trap.cause as u64, trap.tval);
    }

    /// Highest-priority interrupt that is both pending and deliverable now.
    fn pending_interrupt(&self) -> Option<u64> {
        let pend = self.mip & self.mie;
        if pend == 0 {
            return None;
        }
        let m_enabled = self.prv < PRV_M || self.mstatus & MSTATUS_MIE != 0;
        let s_enabled = self.prv < PRV_S || (self.prv == PRV_S && self.mstatus & MSTATUS_SIE != 0);
        let m_pend = pend & !self.mideleg;
        let s_pend = pend & self.mideleg;
        if m_enabled && m_pend != 0 {
            for &c in &INT_PRIORITY {
                if m_pend & (1 << c) != 0 {
                    return Some(IRQ_FLAG | c);
                }
            }
        }
        if s_enabled && s_pend != 0 {
            for &c in &INT_PRIORITY {
                if s_pend & (1 << c) != 0 {
                    return Some(IRQ_FLAG | c);
                }
            }
        }
        None
    }

    /// Enter a trap handler. `cause` carries `IRQ_FLAG` for interrupts.
    pub fn take_trap(&mut self, cause: u64, tval: u64) {
        crate::counters::inc(&crate::counters::TRAPS);
        let is_int = cause & IRQ_FLAG != 0;
        let code = cause & !IRQ_FLAG;
        let deleg = if is_int { self.mideleg } else { self.medeleg };
        let to_s = self.prv <= PRV_S && (deleg >> code) & 1 != 0;
        let pc = self.pc;
        if to_s {
            self.sepc = pc;
            self.scause = cause;
            self.stval = tval;
            let mut ms = self.mstatus & !(MSTATUS_SPIE | MSTATUS_SPP | MSTATUS_SIE);
            if self.mstatus & MSTATUS_SIE != 0 {
                ms |= MSTATUS_SPIE;
            }
            if self.prv == PRV_S {
                ms |= MSTATUS_SPP;
            }
            self.mstatus = ms;
            self.prv = PRV_S;
            let base = self.stvec & !3;
            self.pc = if is_int && self.stvec & 3 == 1 { base + 4 * code } else { base };
        } else {
            self.mepc = pc;
            self.mcause = cause;
            self.mtval = tval;
            let mut ms = self.mstatus & !(MSTATUS_MPIE | MSTATUS_MPP | MSTATUS_MIE);
            if self.mstatus & MSTATUS_MIE != 0 {
                ms |= MSTATUS_MPIE;
            }
            ms |= (self.prv as u64) << 11;
            self.mstatus = ms;
            self.prv = PRV_M;
            let base = self.mtvec & !3;
            self.pc = if is_int && self.mtvec & 3 == 1 { base + 4 * code } else { base };
        }
        self.reservation = None;
        // A trap changes the privilege and the status bits, not the mappings: only
        // the context changes.
        self.select_dctx();
    }

    /// Make the current slow instruction return to the run loop after changing
    /// interrupt delivery, stopping or entering WFI.
    #[inline(always)]
    pub(crate) fn break_chain(&mut self) {
        self.chain_limit.store(0, Relaxed);
    }

    /// Set the machine-level pending bits that the machine derives from
    /// hardware state (timer, software, external interrupt lines).
    pub fn set_hw_pending(&mut self, bits: u64, asserted: bool) {
        if asserted {
            self.mip |= bits;
        } else {
            self.mip &= !bits;
        }
        self.irq_dirty = true;
        self.break_chain();
    }

    /// Current effective privilege for `mstatus`-dependent reads.
    #[inline(always)]
    pub fn privilege(&self) -> u8 {
        self.prv
    }

    /// Return from a trap handler (`mret`/`sret`).
    pub(crate) fn xret(&mut self, to_m: bool) {
        if to_m {
            let new_prv = ((self.mstatus & MSTATUS_MPP) >> 11) as u8;
            let mut ms = self.mstatus & !(MSTATUS_MIE | MSTATUS_MPP | MSTATUS_MPIE | MSTATUS_MPRV);
            if self.mstatus & MSTATUS_MPIE != 0 {
                ms |= MSTATUS_MIE;
            }
            ms |= MSTATUS_MPIE;
            // MPRV survives only when returning to M-mode.
            if new_prv == PRV_M {
                ms |= self.mstatus & MSTATUS_MPRV;
            }
            self.mstatus = ms;
            self.prv = new_prv;
            self.pc = self.mepc;
        } else {
            let new_prv = if self.mstatus & MSTATUS_SPP != 0 { PRV_S } else { PRV_U };
            let mut ms = self.mstatus & !(MSTATUS_SIE | MSTATUS_SPP | MSTATUS_SPIE | MSTATUS_MPRV);
            if self.mstatus & MSTATUS_SPIE != 0 {
                ms |= MSTATUS_SIE;
            }
            ms |= MSTATUS_SPIE;
            self.mstatus = ms;
            self.prv = new_prv;
            self.pc = self.sepc;
        }
        self.select_dctx();
        self.irq_dirty = true;
        self.break_chain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_return_clears_mprv() {
        let mut hart = Hart::new(0, false);
        hart.mstatus = MSTATUS_MPRV | MSTATUS_SPP;
        hart.xret(false);
        assert_eq!(hart.prv, PRV_S);
        assert_eq!(hart.mstatus & MSTATUS_MPRV, 0);
    }

    #[test]
    fn fetch_faults_consume_budget_without_retiring() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(4096, 1);
        assert_eq!(hart.run(&mut bus, 8), 8);
        assert_eq!(hart.csr_read(&bus, CSR_MINSTRET).unwrap(), 0);
        assert_eq!(hart.mcause, EXC_IFETCH_ACCESS);
    }

    #[test]
    fn split_access_fault_reports_the_unmapped_byte() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(4096, 1);
        let end = RAM_BASE + 4096;
        let load = hart.load::<8>(&mut bus, end - 2).unwrap_err();
        let store = hart.store::<8>(&mut bus, end - 2, 0).unwrap_err();
        assert_eq!((u64::from(load.cause), load.tval), (EXC_LOAD_ACCESS, end));
        assert_eq!((u64::from(store.cause), store.tval), (EXC_STORE_ACCESS, end));
    }
}
