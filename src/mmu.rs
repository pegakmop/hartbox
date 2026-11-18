//! Sv39 virtual memory: software TLB and hardware page-table walker.
//!
//! A two-way table caches 4 KiB translations. Two direct-mapped tables cover
//! 2 MiB and 1 GiB superpages.
//! The walker updates accessed and dirty bits in the page table.

use crate::arch::*;
use crate::bus::Bus;
use crate::hart::{
    ftlb_index, FastTlb, Hart, MicroTlb, Res, Trap, DATA_IDENTITY, DATA_PAGED, DATA_SLOW, DCTX_IDENTITY, DCTX_MPRV,
    DCTX, DCTX_SUM, DCTX_SUPERVISOR, DCTX_USER, FTLB_SETS, VICTIM_SETS,
};

const TLB_SMALL_SETS: usize = 1024;
const FTLB_GROUP: usize = FTLB_SETS / 64;
const _: () = assert!(FTLB_SETS.is_multiple_of(FTLB_GROUP) && FTLB_SETS / FTLB_GROUP <= 64);
/// Entries per direct-mapped table for 2 MiB and 1 GiB superpages. Kernel
/// space is mapped in superpages, so these are hit constantly; a linear scan
/// over them cost a few percent of every boot and every shell command.
const TLB_BIG2M: usize = 64;
const TLB_BIG1G: usize = 8;
/// VPN bits a 2 MiB mapping covers.
const MASK_2M: u64 = (1 << 9) - 1;
const MASK_1G: u64 = (1 << 18) - 1;
const PPN_MASK: u64 = (1 << 44) - 1;
const VPN_MASK: u64 = (1 << 52) - 1;
const TLB_GEN_SHIFT: u32 = 58;
const TLB_GEN_MASK: u64 = u64::MAX << TLB_GEN_SHIFT;
const TLB_TAG_MASK: u64 = TLB_GEN_MASK | VPN_MASK;

/// The significant Sv39 VPN bits identify 512 1 GiB and 262,144 2 MiB
/// regions. Keep exact marks even when the page-walk TLB evicts a large leaf.
/// Each dirty group covers 64 words so an address-space switch clears only
/// the parts of the 2 MiB bitmap that were used.
pub(crate) struct Superpages {
    large: [u64; 8],
    medium: Box<[u64]>,
    dirty: u64,
}

impl Default for Superpages {
    fn default() -> Self {
        Self { large: [0; 8], medium: vec![0; 4096].into_boxed_slice(), dirty: 0 }
    }
}

impl Superpages {
    #[inline(always)]
    fn slot(vpn: u64) -> usize {
        (vpn as usize >> 9) & ((1 << 18) - 1)
    }

    #[inline(always)]
    fn mark(&mut self, vpn: u64, mask: u64) {
        let medium = Self::slot(vpn);
        if mask == MASK_2M {
            self.medium[medium >> 6] |= 1 << (medium & 63);
            self.dirty |= 1 << (medium >> 12);
        } else {
            let large = medium >> 9;
            self.large[large >> 6] |= 1 << (large & 63);
        }
    }

    /// Remove the mark whose entire cached region is about to be invalidated.
    fn take_region(&mut self, vpn: u64) -> u64 {
        let medium = Self::slot(vpn);
        let large = medium >> 9;
        let large_word = &mut self.large[large >> 6];
        let large_bit = 1 << (large & 63);
        if *large_word & large_bit != 0 {
            *large_word &= !large_bit;
            self.medium[large * 8..large * 8 + 8].fill(0);
            return MASK_1G;
        }
        let medium_word = &mut self.medium[medium >> 6];
        let medium_bit = 1 << (medium & 63);
        if *medium_word & medium_bit == 0 {
            return 0;
        }
        *medium_word &= !medium_bit;
        MASK_2M
    }

    fn clear(&mut self) {
        self.large.fill(0);
        let mut groups = std::mem::take(&mut self.dirty);
        while groups != 0 {
            let start = groups.trailing_zeros() as usize * 64;
            groups &= groups - 1;
            self.medium[start..start + 64].fill(0);
        }
    }
}

#[inline(always)]
fn small_index(vpn: u64) -> usize {
    // The flat TLB indexes low VPN bits. A different hash avoids taking a
    // second miss when two pages collide in the flat table.
    ((vpn ^ (vpn >> 11) ^ (vpn >> 22)) as usize) & (TLB_SMALL_SETS - 1)
}

#[inline(always)]
fn victim_index(va: u64) -> usize {
    ((va >> 12) ^ (va >> (12 + FTLB_SETS.trailing_zeros()))) as usize & (VICTIM_SETS - 1)
}

// PTE bit positions.
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_G: u64 = 1 << 5;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

// Internal permission encoding stored in TLB entries.
const P_R: u8 = 1;
const P_W: u8 = 2;
const P_X: u8 = 4;
const P_U: u8 = 8;
const P_G: u8 = 16;
const P_D: u8 = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Fetch,
    Load,
    Store,
}

#[derive(Clone, Copy)]
struct TlbEntry {
    /// 52-bit VPN, six permission bits and six generation bits.
    vpn_perm: u64,
    /// 44-bit PPN followed by the 16-bit ASID.
    ppn_asid: u64,
}

impl TlbEntry {
    const INVALID: Self = Self { vpn_perm: VPN_MASK, ppn_asid: 0 };

    #[inline(always)]
    fn lookup(self, vpn: u64, tag: u64, mask: u64, asid: u16, acc: Access, prv: u8, sum: bool, mxr: bool) -> Option<(u64, u64)> {
        let perm = ((self.vpn_perm >> 52) & 0x3f) as u8;
        if self.vpn_perm & TLB_TAG_MASK == tag & !mask
            && (perm & P_G != 0 || self.ppn_asid >> 44 == asid as u64)
            && perm_ok(perm, acc, prv, sum, mxr)
            && (acc != Access::Store || perm & P_D != 0)
        {
            Some(((self.ppn_asid & PPN_MASK) | (vpn & mask), mask))
        } else {
            None
        }
    }

    fn invalidate(&mut self, asid: Option<u16>) {
        if asid.is_none_or(|a| self.vpn_perm & (u64::from(P_G) << 52) == 0 && self.ppn_asid >> 44 == a as u64) {
            *self = Self::INVALID;
        }
    }
}

pub struct Tlb {
    generation: u64,
    small: [[TlbEntry; 2]; TLB_SMALL_SETS],
    big2m: [TlbEntry; TLB_BIG2M],
    big1g: [TlbEntry; TLB_BIG1G],
}

impl Default for Tlb {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a mapping with permissions `perm` permits `acc` at privilege `prv`.
#[inline(always)]
fn perm_ok(perm: u8, acc: Access, prv: u8, sum: bool, mxr: bool) -> bool {
    let user_page = perm & P_U != 0;
    let priv_ok = if prv == PRV_U {
        user_page
    } else {
        // Supervisor never executes user pages; SUM allows data access to them.
        !user_page || (sum && acc != Access::Fetch)
    };
    let right = match acc {
        Access::Fetch => perm & P_X != 0,
        Access::Load => perm & P_R != 0 || (mxr && perm & P_X != 0),
        Access::Store => perm & P_W != 0,
    };
    priv_ok && right
}

impl Tlb {
    pub fn new() -> Self {
        Tlb {
            generation: 0,
            small: [[TlbEntry::INVALID; 2]; TLB_SMALL_SETS],
            big2m: [TlbEntry::INVALID; TLB_BIG2M],
            big1g: [TlbEntry::INVALID; TLB_BIG1G],
        }
    }

    pub fn flush_all(&mut self) {
        // Match the generation with the VPN in the same tag comparison. A
        // full fence only clears the arrays when a generation would repeat.
        self.generation = self.generation.wrapping_add(1 << TLB_GEN_SHIFT);
        if self.generation == 0 {
            self.small.fill([TlbEntry::INVALID; 2]);
            self.big2m.fill(TlbEntry::INVALID);
            self.big1g.fill(TlbEntry::INVALID);
        }
    }

    /// Look up a translation. Returns the physical page number.
    #[inline(always)]
    pub fn lookup(&self, vpn: u64, asid: u16, acc: Access, prv: u8, sum: bool, mxr: bool) -> Option<u64> {
        self.lookup_mapping(vpn, asid, acc, prv, sum, mxr).map(|(ppn, _)| ppn)
    }

    /// Look up a translation. Returns the physical page number and the mask of
    /// the mapping (0 for a 4 KiB page).
    #[inline(always)]
    pub fn lookup_mapping(&self, vpn: u64, asid: u16, acc: Access, prv: u8, sum: bool, mxr: bool) -> Option<(u64, u64)> {
        let small = &self.small[small_index(vpn)];
        let tag = vpn | self.generation;
        small[0].lookup(vpn, tag, 0, asid, acc, prv, sum, mxr)
            .or_else(|| self.big2m[(vpn as usize >> 9) & (TLB_BIG2M - 1)].lookup(vpn, tag, MASK_2M, asid, acc, prv, sum, mxr))
            .or_else(|| self.big1g[(vpn as usize >> 18) & (TLB_BIG1G - 1)].lookup(vpn, tag, MASK_1G, asid, acc, prv, sum, mxr))
            .or_else(|| small[1].lookup(vpn, tag, 0, asid, acc, prv, sum, mxr))
    }

    fn insert(&mut self, vpn: u64, mask: u64, ppn: u64, asid: u16, perm: u8) {
        let entry = TlbEntry {
            vpn_perm: (vpn & !mask) | (u64::from(perm) << 52) | self.generation,
            ppn_asid: ppn | (u64::from(asid) << 44),
        };
        match mask {
            0 => {
                let ways = &mut self.small[small_index(vpn)];
                if ways[0].vpn_perm & VPN_MASK != vpn || ways[0].ppn_asid >> 44 != u64::from(asid) {
                    ways[1] = ways[0];
                }
                ways[0] = entry;
            }
            MASK_2M => self.big2m[(vpn as usize >> 9) & (TLB_BIG2M - 1)] = entry,
            _ => self.big1g[(vpn as usize >> 18) & (TLB_BIG1G - 1)] = entry,
        }
    }

    pub fn sfence(&mut self, vpn: Option<u64>, asid: Option<u16>) {
        match (vpn, asid) {
            (None, None) => self.flush_all(),
            (Some(vpn), asid) => self.flush_page(vpn, asid),
            (None, asid) => {
                for e in self.small.iter_mut().flatten().chain(&mut self.big2m).chain(&mut self.big1g) {
                    e.invalidate(asid);
                }
            }
        }
    }

    /// The only entries that can map `vpn` are the set and superpage slots it indexes.
    fn flush_page(&mut self, vpn: u64, asid: Option<u16>) {
        for entry in &mut self.small[small_index(vpn)] {
            if entry.vpn_perm & VPN_MASK == vpn {
                entry.invalidate(asid);
            }
        }
        for (entry, mask) in [
            (&mut self.big2m[(vpn as usize >> 9) & (TLB_BIG2M - 1)], MASK_2M),
            (&mut self.big1g[(vpn as usize >> 18) & (TLB_BIG1G - 1)], MASK_1G),
        ] {
            if entry.vpn_perm & VPN_MASK == vpn & !mask {
                entry.invalidate(asid);
            }
        }
    }
}

impl Hart {
    /// Privilege used for a data access, honouring `mstatus.MPRV`.
    #[inline(always)]
    pub(crate) fn data_priv(&self, acc: Access) -> u8 {
        if acc != Access::Fetch && self.mstatus & MSTATUS_MPRV != 0 {
            ((self.mstatus & MSTATUS_MPP) >> 11) as u8
        } else {
            self.prv
        }
    }

    /// Translate a data address (load/store/AMO). Identity-mapped when paging
    /// is off or the effective privilege is M.
    #[inline(always)]
    pub fn translate(&mut self, bus: &mut Bus, va: u64, acc: Access) -> Res<u64> {
        let prv = self.data_priv(acc);
        if !self.satp_sv39 || prv == PRV_M {
            return Ok(va);
        }
        self.translate_tlb(bus, va, acc, prv).map(|(pa, _)| pa)
    }

    /// Remember that the page holding `va`, which translates to `pa`, is plain RAM
    /// that a load (or, if `store`, a store) may reach with no further check, in
    /// the flat data TLB. Anything else stays on the general path.
    pub(crate) fn ftlb_fill(&mut self, bus: &Bus, va: u64, pa: u64, store: bool) {
        // With MPRV set the privilege of an access is not the hart's own.
        if self.data_mode == DATA_SLOW {
            return;
        }
        let off = (pa & !0xfff).wrapping_sub(RAM_BASE) as usize;
        if off.checked_add(4096).is_none_or(|end| end > bus.ram.len()) {
            return;
        }
        // A page that holds decoded code must see every store, to invalidate it.
        if store && bus.page_has_code(off >> 12) {
            return;
        }
        let host = bus.ram.as_ptr() as usize + off;
        let page = va & !0xfff;
        let e = FastTlb { tag: page, addend: (host as u64).wrapping_sub(page) };
        let index = ftlb_index(va);
        let table = self.dctx as usize * 2 + usize::from(store);
        self.ftlb_dirty[table] |= 1 << (index / FTLB_GROUP);
        // SAFETY: the current tables are `FTLB_SETS` entries inside `ftlb_store`.
        unsafe {
            if store {
                let old = self.stlb_cur.add(index).replace(e);
                if old.tag != u64::MAX && old.tag != e.tag {
                    self.store_victim[self.dctx as usize][victim_index(old.tag)] = old;
                }
                let slot = (table / 2) * FTLB_SETS + index;
                self.note_stlb_page(bus, off >> 12, slot);
            } else {
                self.install_fast_load(index, e);
            }
        }
    }

    #[inline(always)]
    unsafe fn install_fast_load(&mut self, index: usize, entry: FastTlb) {
        let old = self.ltlb_cur.add(index).replace(entry);
        if old.tag != u64::MAX && old.tag != entry.tag {
            self.load_victim[self.dctx as usize][victim_index(old.tag)] = old;
        }
    }

    /// Recover a valid RAM mapping on a flat load miss. The caller has checked
    /// that the access fits within one page. Stores still use the precise path
    /// for code protection and reverse lookup.
    #[inline(always)]
    pub(crate) fn recover_load_mapping(&mut self, va: u64) -> Option<usize> {
        if self.data_mode == DATA_SLOW {
            return None;
        }
        let slot = &mut self.load_victim[self.dctx as usize][victim_index(va)];
        let entry = *slot;
        if entry.tag != va & !0xfff {
            return None;
        }
        *slot = FastTlb::EMPTY;
        // Eviction and promotion keep the same primary slot and dirty group.
        unsafe { self.install_fast_load(ftlb_index(va), entry) };
        Some(va.wrapping_add(entry.addend) as usize)
    }

    /// Reuse a displaced store translation. The caller still writes through
    /// the bus, because the physical page may hold decoded code by now.
    #[inline(always)]
    pub(crate) fn recover_store_mapping(&mut self, bus: &Bus, va: u64) -> Option<u64> {
        if self.data_mode == DATA_SLOW {
            return None;
        }
        let slot = &mut self.store_victim[self.dctx as usize][victim_index(va)];
        if slot.tag != va & !0xfff {
            return None;
        }
        let host = va.wrapping_add(slot.addend);
        *slot = FastTlb::EMPTY;
        Some(RAM_BASE + host.wrapping_sub(bus.ram.as_ptr() as u64))
    }

    /// Remember that a flat store TLB entry may map RAM page `ppn`.
    #[inline(always)]
    fn note_stlb_page(&mut self, bus: &Bus, ppn: usize, slot: usize) {
        let w = ppn >> 6;
        if w >= self.stlb_pages.len() {
            self.stlb_pages.resize((bus.ram.len() >> 18) + 1, 0);
        }
        if ppn >= self.stlb_slots.len() {
            self.stlb_slots.resize(bus.ram.len() >> 12, 0);
        }
        debug_assert!(slot + 1 < u16::MAX as usize);
        let bit = 1u64 << (ppn & 63);
        let encoded = (slot + 1) as u16;
        let hint = &mut self.stlb_slots[ppn];
        if self.stlb_pages[w] & bit == 0 {
            *hint = encoded;
        } else if *hint != encoded {
            *hint = u16::MAX;
        }
        self.stlb_pages[w] |= bit;
    }

    /// RAM page `ppn` has just been decoded as code for the first time: stores to it must
    /// take the general path from now on, so remove the store entries that map it.
    ///
    /// Returns whether this hart may have held a store entry for the page.
    fn drop_store_page(&mut self, bus: &Bus, ppn: usize) -> bool {
        let (w, bit) = (ppn >> 6, 1u64 << (ppn & 63));
        match self.stlb_pages.get_mut(w) {
            Some(word) if *word & bit != 0 => *word &= !bit,
            _ => return false,
        }
        let host = (bus.ram.as_ptr() as u64).wrapping_add((ppn as u64) << 12);
        // Newly executable pages usually had one writable mapping. Check its
        // recorded slot instead of scanning every context's whole table. An
        // evicted/reused slot is harmless: validate the physical page before
        // clearing it. Aliases retain the full conservative scan below.
        if let Some(&hint) = self.stlb_slots.get(ppn) {
            if hint != 0 && hint != u16::MAX {
                let slot = hint as usize - 1;
                let table = 2 * (slot / FTLB_SETS) + 1;
                let e = &mut self.ftlb_store[table * FTLB_SETS + slot % FTLB_SETS];
                if e.tag != u64::MAX && e.tag.wrapping_add(e.addend) == host {
                    *e = FastTlb::EMPTY;
                }
                return true;
            }
        }
        for table in (1..2 * DCTX).step_by(2) {
            let mut groups = self.ftlb_dirty[table];
            while groups != 0 {
                let start = table * FTLB_SETS + groups.trailing_zeros() as usize * FTLB_GROUP;
                groups &= groups - 1;
                for e in &mut self.ftlb_store[start..start + FTLB_GROUP] {
                    if e.tag != u64::MAX && e.tag.wrapping_add(e.addend) == host {
                        *e = FastTlb::EMPTY;
                    }
                }
            }
        }
        true
    }

    /// Forget the store entries of every context: a page that stores could reach
    /// freely has just been decoded as code.
    pub(crate) fn flush_store_tlb(&mut self) {
        for table in (1..2 * DCTX).step_by(2) {
            self.clear_fast_table(table);
        }
        self.stlb_pages.fill(0);
    }

    /// Clear only populated regions of sparse tables. A dense table is faster
    /// to clear in one contiguous pass.
    fn clear_fast_table(&mut self, table: usize) {
        let mut groups = std::mem::take(&mut self.ftlb_dirty[table]);
        if groups == 0 {
            return;
        }
        if table % 2 == 0 {
            self.load_victim[table / 2].fill(FastTlb::EMPTY);
        } else {
            self.store_victim[table / 2].fill(FastTlb::EMPTY);
        }
        let at = table * FTLB_SETS;
        let entries = &mut self.ftlb_store[at..at + FTLB_SETS];
        if groups.count_ones() as usize > FTLB_SETS / FTLB_GROUP / 2 {
            entries.fill(FastTlb::EMPTY);
            return;
        }
        while groups != 0 {
            let start = groups.trailing_zeros() as usize * FTLB_GROUP;
            entries[start..start + FTLB_GROUP].fill(FastTlb::EMPTY);
            groups &= groups - 1;
        }
    }

    /// Pick the data context for the current privilege, `SUM`, `MPRV` and paging
    /// mode, and with it the flat TLBs and the summary the slow path uses.
    pub(crate) fn select_dctx(&mut self) {
        let fetch_context = if self.satp_sv39 { self.prv } else { PRV_M };
        self.cache_stamp = (self.cache_stamp & !(3 << 32)) | (u64::from(fetch_context) << 32);
        let (ctx, mode) = if self.mstatus & MSTATUS_MPRV != 0 {
            (DCTX_MPRV, DATA_SLOW)
        } else if !self.satp_sv39 || self.prv == PRV_M {
            (DCTX_IDENTITY, DATA_IDENTITY)
        } else if self.prv == PRV_U {
            (DCTX_USER, DATA_PAGED)
        } else if self.mstatus & MSTATUS_SUM != 0 {
            (DCTX_SUM, DATA_PAGED)
        } else {
            (DCTX_SUPERVISOR, DATA_PAGED)
        };
        self.data_mode = mode;
        self.dctx = ctx;
        // MPRV never fills its tables, so all harts can share the empty entries.
        self.ltlb_cur = if mode == DATA_SLOW {
            crate::hart::EMPTY_FAST_TLB.as_ptr().cast_mut()
        } else {
            unsafe { self.ftlb_store.as_mut_ptr().add(ctx as usize * 2 * FTLB_SETS) }
        };
        // Both layouts have a store table immediately after the load table.
        self.stlb_cur = unsafe { self.ltlb_cur.add(FTLB_SETS) };
    }

    /// Drop the store entries of the pages that were decoded as code for the first time
    /// since this hart last looked.
    #[inline(always)]
    pub(crate) fn sync_code_marks(&mut self, bus: &Bus) {
        if bus.code_marks.load(std::sync::atomic::Ordering::Relaxed) != self.seen_code_marks {
            self.sync_code_marks_slow(bus);
        }
    }

    #[cold]
    #[inline(never)]
    fn sync_code_marks_slow(&mut self, bus: &Bus) {
        use std::sync::atomic::Ordering::Acquire;
        let marks = bus.code_marks.load(Acquire);
        let n = marks.wrapping_sub(self.seen_code_marks) as usize;
        let mt = bus.mt.is_some();
        if n > crate::bus::CODE_LOG {
            if mt {
                self.forget_code_under_store_entries(bus);
            }
            self.flush_store_tlb();
        } else {
            for k in 0..n {
                let at = self.seen_code_marks.wrapping_add(k as u32) as usize % crate::bus::CODE_LOG;
                let e = bus.code_log[at].load(std::sync::atomic::Ordering::Relaxed);
                let (ppn, by) = ((e & 0x03ff_ffff) as usize, (e >> 26) as u64);
                // A page another hart decoded while this one could still store to it freely:
                // those stores did not invalidate anything, so the other hart's blocks of
                // the page may be stale. Make them so; they are rebuilt from the bytes now.
                if self.drop_store_page(bus, ppn) && mt && by != self.id {
                    bus.forget_code(ppn);
                }
            }
            // Marks made while this loop ran may have overwritten log entries it was
            // about to read: then fall back to the page-by-page rule.
            if mt && bus.code_marks.load(Acquire).wrapping_sub(self.seen_code_marks) as usize > crate::bus::CODE_LOG {
                self.forget_code_under_store_entries(bus);
                self.flush_store_tlb();
            }
        }
        self.seen_code_marks = marks;
    }

    /// The mark log overflowed: every page this hart may store to freely that now holds
    /// decoded code is made stale, whoever decoded it.
    fn forget_code_under_store_entries(&mut self, bus: &Bus) {
        for w in 0..self.stlb_pages.len() {
            let mut bits = self.stlb_pages[w];
            while bits != 0 {
                let ppn = (w << 6) | bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if bus.page_has_code(ppn) {
                    bus.forget_code(ppn);
                }
            }
        }
    }

    /// Translation through the TLB, walking the page table on a miss. Returns
    /// the physical address and the mask of the mapping it came from.
    #[inline(always)]
    fn translate_tlb(&mut self, bus: &mut Bus, va: u64, acc: Access, prv: u8) -> Res<(u64, u64)> {
        let vpn = va >> 12;
        let sum = self.mstatus & MSTATUS_SUM != 0;
        let mxr = self.mstatus & MSTATUS_MXR != 0;
        let (pa, mask) = match self.tlb.lookup_mapping(vpn, self.asid, acc, prv, sum, mxr) {
            Some((ppn, mask)) => {
                crate::counters::inc(&crate::counters::TLB_HITS);
                ((ppn << 12) | (va & 0xfff), mask)
            }
            None => {
                crate::counters::inc(&crate::counters::TLB_WALKS);
                self.walk(bus, va, acc, prv, sum, mxr)?
            }
        };
        if mask != 0 {
            self.superpages.mark(vpn, mask);
        }
        Ok((pa, mask))
    }

    /// Translate an instruction fetch address, with a one-page shortcut so
    /// sequential fetches avoid the TLB entirely.
    #[inline(always)]
    pub(crate) fn fetch_pa(&mut self, bus: &mut Bus, pc: u64) -> Res<u64> {
        if !self.satp_sv39 || self.prv == PRV_M {
            return Ok(pc);
        }
        let vpn = pc >> 12;
        let m = self.ifetch[usize::from(self.prv == PRV_S)];
        if vpn & !m.mask == m.tag {
            return Ok(((m.ppn | (vpn & m.mask)) << 12) | (pc & 0xfff));
        }
        self.fetch_pa_slow(bus, pc)
    }

    #[cold]
    #[inline(never)]
    fn fetch_pa_slow(&mut self, bus: &mut Bus, pc: u64) -> Res<u64> {
        let (pa, mask) = self.translate_tlb(bus, pc, Access::Fetch, self.prv)?;
        let vpn = pc >> 12;
        self.ifetch[usize::from(self.prv == PRV_S)] = MicroTlb {
            tag: vpn & !mask,
            mask,
            ppn: (pa >> 12) & !mask,
        };
        Ok(pa)
    }

    /// Hardware page-table walk for Sv39.
    #[cold]
    #[inline(never)]
    fn walk(&mut self, bus: &mut Bus, va: u64, acc: Access, prv: u8, sum: bool, mxr: bool) -> Res<(u64, u64)> {
        let (page_cause, access_cause) = match acc {
            Access::Fetch => (EXC_FETCH_PAGE, EXC_IFETCH_ACCESS),
            Access::Load => (EXC_LOAD_PAGE, EXC_LOAD_ACCESS),
            Access::Store => (EXC_STORE_PAGE, EXC_STORE_ACCESS),
        };
        // Sv39 virtual addresses must be sign-extended from bit 38.
        let top = (va as i64) >> 38;
        if top != 0 && top != -1 {
            return Err(Trap::new(page_cause, va));
        }
        let vpn = va >> 12;
        let root = (self.satp & PPN_MASK) << 12;
        let mut table = root;
        let mut level: u64 = 2;
        let mut global = false;
        loop {
            let idx = (vpn >> (9 * level)) & 0x1ff;
            let pte_pa = table + idx * 8;
            let pte = bus.read::<8>(pte_pa).ok_or(Trap::new(access_cause, va))?;
            let valid = pte & PTE_V != 0;
            let r = pte & PTE_R != 0;
            let w = pte & PTE_W != 0;
            let x = pte & PTE_X != 0;
            // Reserved encodings and bits 63:54 (Svpbmt/Svnapot are not enabled).
            if !valid || (!r && w) || (pte >> 54) != 0 {
                return Err(Trap::new(page_cause, va));
            }
            global |= pte & PTE_G != 0;
            if !r && !x {
                // Pointer to the next level.
                if level == 0 || pte & (PTE_A | PTE_D | PTE_U) != 0 {
                    return Err(Trap::new(page_cause, va));
                }
                table = ((pte >> 10) & PPN_MASK) << 12;
                level -= 1;
                continue;
            }
            // Leaf. Superpages must be naturally aligned.
            let ppn = (pte >> 10) & PPN_MASK;
            let mask = (1u64 << (9 * level)) - 1;
            if ppn & mask != 0 {
                return Err(Trap::new(page_cause, va));
            }
            let mut perm = 0u8;
            if r {
                perm |= P_R;
            }
            if w {
                perm |= P_W;
            }
            if x {
                perm |= P_X;
            }
            if pte & PTE_U != 0 {
                perm |= P_U;
            }
            if global {
                perm |= P_G;
            }
            if !perm_ok(perm, acc, prv, sum, mxr) {
                return Err(Trap::new(page_cause, va));
            }
            // Hardware A/D update.
            let mut new_pte = pte | PTE_A;
            if acc == Access::Store {
                new_pte |= PTE_D;
            }
            if new_pte != pte && !bus.update_pte(pte_pa, pte, new_pte).ok_or(Trap::new(access_cause, va))? {
                // The old translation lost a race with a PTE update. Walk again so
                // a replaced PTE never receives A/D bits for the old mapping.
                table = root;
                level = 2;
                global = false;
                continue;
            }
            if new_pte & PTE_D != 0 {
                perm |= P_D;
            }
            let base = ppn & !mask;
            let mapped_vpn_base = vpn & !mask;
            self.tlb.insert(mapped_vpn_base, mask, base, self.asid, perm);
            let ppn_full = base | (vpn & mask);
            return Ok(((ppn_full << 12) | (va & 0xfff), mask));
        }
    }

    /// `sfence.vma` with the usual `rs1`/`rs2` (x0) wildcard rules.
    ///
    /// A fence for one page does not throw everything away. The hardware TLB drops the
    /// entries that match. The flat data tables drop the slots covered by the
    /// corresponding leaf, including siblings derived from a large mapping.
    /// The block links, which are keyed by virtual pc under the translation epoch, only
    /// need the epoch to move if one of them was made for this page (`link_pages`): the
    /// kernel fences data pages all the time (copy on write, `mprotect`, `munmap`), and
    /// those never carry code.
    pub(crate) fn sfence_vma(&mut self, va: Option<u64>, asid: Option<u16>) {
        crate::counters::inc(&crate::counters::SFENCES);
        // The flat tables and fetch links are only populated for the active
        // address space. SATP writes already discard them on an ASID change.
        if asid.is_some_and(|asid| asid != self.asid) {
            self.tlb.sfence(va.map(|va| va >> 12), asid);
            return;
        }
        let Some(va) = va else {
            self.tlb.sfence(None, asid);
            self.flush_translation_cache();
            return;
        };
        let vpn = va >> 12;
        let canonical = ((va as i64) << 25) >> 25 == va as i64;
        let region_mask = if canonical { self.superpages.take_region(vpn) } else { 0 };
        self.tlb.sfence(Some(vpn), asid);
        crate::counters::inc(&crate::counters::SFENCE_PAGE);
        if region_mask != 0 {
            self.flush_data_region(vpn, region_mask);
            // Block links may have reached a sibling page through the same
            // large leaf, including leaves evicted from the page-walk TLB.
            self.invalidate_block_links();
        } else {
            self.flush_data_page(va);
            if self.page_has_links(vpn) {
                self.invalidate_block_links();
            }
        }
        for fetch in &mut self.ifetch {
            let shared = !(fetch.mask | region_mask);
            if (vpn ^ fetch.tag) & shared == 0 {
                *fetch = MicroTlb::EMPTY;
            }
        }
    }

    /// A large leaf can populate many flat slots, including recently evicted
    /// entries. Clear the entries in that virtual range across all contexts.
    fn flush_data_region(&mut self, vpn: u64, mask: u64) {
        let region = vpn & !mask;
        let first = ftlb_index(region << 12) / FTLB_GROUP;
        let group_count = ((mask + 1) as usize).min(FTLB_SETS) / FTLB_GROUP;
        let group_mask = (u64::MAX >> (64 - group_count)) << first;
        let matches = |entry: &FastTlb| (entry.tag >> 12) & !mask == region;
        for table in 0..2 * DCTX {
            let mut groups = self.ftlb_dirty[table] & group_mask;
            while groups != 0 {
                let start = table * FTLB_SETS + groups.trailing_zeros() as usize * FTLB_GROUP;
                groups &= groups - 1;
                for entry in &mut self.ftlb_store[start..start + FTLB_GROUP] {
                    if matches(entry) {
                        *entry = FastTlb::EMPTY;
                    }
                }
            }
        }
        for entry in self.load_victim.iter_mut().chain(&mut self.store_victim).flatten() {
            if matches(entry) {
                *entry = FastTlb::EMPTY;
            }
        }
    }

    /// Drop the flat data entries for the page holding `va`.
    fn flush_data_page(&mut self, va: u64) {
        let page = va & !0xfff;
        let slot = crate::hart::ftlb_index(va);
        let group = 1 << (slot / FTLB_GROUP);
        for table in 0..2 * DCTX {
            if self.ftlb_dirty[table] & group != 0 {
                let e = &mut self.ftlb_store[table * FTLB_SETS + slot];
                if e.tag == page {
                    *e = FastTlb::EMPTY;
                }
            }
        }
        let victim = victim_index(va);
        for context in 0..DCTX {
            for entry in [&mut self.load_victim[context][victim], &mut self.store_victim[context][victim]] {
                if entry.tag == page {
                    *entry = FastTlb::EMPTY;
                }
            }
        }
    }

    /// Discard cached translations after the satp or ASID changes, or after an
    /// `sfence.vma`. Everything cached is dropped, including block links: the
    /// same virtual pc may map to different code under new page tables.
    pub(crate) fn flush_translation_cache(&mut self) {
        crate::counters::inc(&crate::counters::MTLB_FLUSH);
        for table in 0..2 * DCTX {
            self.clear_fast_table(table);
        }
        self.stlb_pages.fill(0);
        self.select_dctx();
        self.ifetch.fill(MicroTlb::EMPTY);
        self.superpages.clear();
        self.invalidate_block_links();
    }

    fn invalidate_block_links(&mut self) {
        self.cache_stamp = self.cache_stamp.wrapping_add(1 << 34);
        self.link_pages.fill(0);
    }

    /// MXR changes the permission to read executable pages through Sv39.
    /// Walk-TLB hits recheck permissions, and stores and fetches are unaffected.
    pub(crate) fn flush_mxr_loads(&mut self) {
        crate::counters::inc(&crate::counters::MTLB_FLUSH);
        for context in [DCTX_USER, DCTX_SUPERVISOR, DCTX_SUM] {
            self.clear_fast_table(usize::from(context) * 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tlb_store_requires_dirty() {
        let mut t = Tlb::new();
        t.insert(5, 0, 9, 0, P_R | P_W);
        assert!(t.lookup(5, 0, Access::Load, PRV_S, false, false).is_some());
        assert!(t.lookup(5, 0, Access::Store, PRV_S, false, false).is_none());
        t.insert(5, 0, 9, 0, P_R | P_W | P_D);
        assert_eq!(t.lookup(5, 0, Access::Store, PRV_S, false, false), Some(9));
    }

    #[test]
    fn walk_tlb_does_not_resurrect_entries_on_generation_wrap() {
        let mut tlb = Tlb::new();
        for generation in 0..128 {
            for (vpn, mask) in [(7, 0), (3 << 9, MASK_2M), (1 << 18, MASK_1G)] {
                assert_eq!(tlb.lookup(vpn, 3, Access::Load, PRV_S, false, false), None);
                tlb.insert(vpn, mask, 0, 3, P_R);
                assert_eq!(tlb.lookup(vpn, 3, Access::Load, PRV_S, false, false), Some(0));
            }
            if generation == 0 {
                // This entry stays untouched for the rest of the test.
                tlb.insert(11, 0, 9, 3, P_R | P_G);
            } else {
                assert_eq!(tlb.lookup(11, 3, Access::Load, PRV_S, false, false), None);
            }
            tlb.flush_all();
        }
    }

    #[test]
    fn walk_tlb_retains_both_ways_and_fences_one_page() {
        let mut tlb = Tlb::new();
        let a = 0;
        let b = TLB_SMALL_SETS as u64;
        assert_eq!(small_index(a), small_index(b));
        tlb.insert(a, 0, 8, 0, P_R);
        tlb.insert(b, 0, 9, 0, P_R);
        assert_eq!(tlb.lookup(a, 0, Access::Load, PRV_S, false, false), Some(8));
        assert_eq!(tlb.lookup(b, 0, Access::Load, PRV_S, false, false), Some(9));
        tlb.sfence(Some(a), None);
        assert_eq!(tlb.lookup(a, 0, Access::Load, PRV_S, false, false), None);
        assert_eq!(tlb.lookup(b, 0, Access::Load, PRV_S, false, false), Some(9));
    }

    #[test]
    fn displaced_flat_load_survives_a_collision_but_not_a_fence() {
        let mut h = Hart::new(0, false);
        let bus = Bus::new(64 << 10, 1);
        let a = 0x4000_1000;
        let b = a + (FTLB_SETS as u64 * 4096);
        h.ftlb_fill(&bus, a, RAM_BASE + 0x2000, false);
        h.ftlb_fill(&bus, b, RAM_BASE + 0x3000, false);
        assert_eq!(h.recover_load_mapping(a + 6), Some(bus.ram.as_ptr() as usize + 0x2006));
        assert_eq!(h.recover_load_mapping(b), Some(bus.ram.as_ptr() as usize + 0x3000));
        h.sfence_vma(Some(a), None);
        assert_eq!(h.recover_load_mapping(a), None);
        h.flush_translation_cache();
        assert_eq!(h.recover_load_mapping(b), None);
        assert_eq!(unsafe { (*h.ltlb_cur.add(ftlb_index(b))).tag }, u64::MAX);
    }

    #[test]
    fn supervisor_cannot_fetch_user_page() {
        assert!(!perm_ok(P_X | P_U, Access::Fetch, PRV_S, true, false));
        assert!(perm_ok(P_X | P_U, Access::Fetch, PRV_U, false, false));
        assert!(perm_ok(P_R | P_U, Access::Load, PRV_S, true, false));
        assert!(!perm_ok(P_R | P_U, Access::Load, PRV_S, false, false));
    }

    #[test]
    fn cached_fetch_rechecks_privilege() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(64 << 10, 1);
        let root = RAM_BASE + 0x4000;
        let va = 0x1000;
        // 1 GiB user-executable leaf at VA 0.
        bus.write::<8>(root, ((RAM_BASE >> 12) << 10) | PTE_V | PTE_X | PTE_U | PTE_A).unwrap();
        bus.write::<4>(RAM_BASE + va, 0x0010_8093).unwrap(); // addi ra,ra,1
        bus.write::<4>(RAM_BASE + va + 4, 0x0000_006f).unwrap(); // jal zero,0
        hart.csr_write(CSR_SATP, (8 << 60) | (root >> 12)).unwrap();
        hart.set_prv(PRV_U);
        hart.pc = va;
        hart.run(&mut bus, 1);
        assert_eq!(hart.x[1], 1);
        hart.set_prv(PRV_S);
        hart.pc = va;
        hart.run(&mut bus, 1);
        assert_eq!(hart.mcause, EXC_FETCH_PAGE);
        assert_eq!(hart.mtval, va);
        assert_eq!(hart.x[1], 1);
    }

    #[test]
    fn checked_block_links_revalidate_across_aliases_and_page_fences() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(64 << 10, 1);
        let (root, middle, leaves) = (RAM_BASE + 0x8000, RAM_BASE + 0x9000, RAM_BASE + 0xa000);
        let pte = |pa: u64, flags| ((pa >> 12) << 10) | flags;
        bus.write::<8>(root, pte(middle, PTE_V)).unwrap();
        bus.write::<8>(middle, pte(leaves, PTE_V)).unwrap();
        for (vpn, pa) in [(1, RAM_BASE), (2, RAM_BASE + 0x1000), (5, RAM_BASE), (6, RAM_BASE + 0x2000)] {
            bus.write::<8>(leaves + vpn * 8, pte(pa, PTE_V | PTE_X | PTE_A)).unwrap();
        }
        bus.write::<4>(RAM_BASE + 0xffc, 0x0040_006f).unwrap(); // jal x0,+4
        for (pa, value) in [(RAM_BASE + 0x1000, 7), (RAM_BASE + 0x2000, 11)] {
            bus.write::<4>(pa, value << 20 | 0x193).unwrap(); // addi x3,x0,value
            bus.write::<4>(pa + 4, 0x1050_0073).unwrap(); // wfi
        }
        hart.csr_write(CSR_SATP, (8 << 60) | (root >> 12)).unwrap();
        hart.set_prv(PRV_S);
        for (pc, expected) in [(0x1ffc, 7), (0x1ffc, 7), (0x5ffc, 11), (0x5ffc, 11), (0x1ffc, 7)] {
            hart.pc = pc;
            hart.wfi = false;
            assert_eq!(hart.run(&mut bus, 8), 3);
            assert_eq!(hart.x[3], expected);
        }
        bus.write::<8>(leaves + 2 * 8, pte(RAM_BASE + 0x2000, PTE_V | PTE_X | PTE_A)).unwrap();
        hart.sfence_vma(Some(0x2000), None);
        hart.pc = 0x1ffc;
        hart.wfi = false;
        assert_eq!(hart.run(&mut bus, 8), 3);
        assert_eq!(hart.x[3], 11);
    }

    #[test]
    fn store_victim_respects_code_invalidation_and_fences() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(64 << 10, 1);
        let pa = RAM_BASE + 0x3000;
        let alias = pa + FTLB_SETS as u64 * 4096;
        hart.ftlb_fill(&bus, pa, pa, true);
        hart.ftlb_fill(&bus, alias, RAM_BASE + 0x4000, true);
        bus.mark_code(pa, 4, 0);
        hart.sync_code_marks(&bus);
        hart.store_general::<4>(&mut bus, pa, 0x0010_0073).unwrap();
        assert_eq!(bus.code_generation(3), 1);
        hart.ftlb_fill(&bus, alias, RAM_BASE + 0x4000, true);
        hart.sfence_vma(Some(pa), None);
        assert_eq!(hart.recover_store_mapping(&bus, pa), None);
    }

    #[test]
    fn fence_invalidates_fast_superpage_mappings_after_tlb_eviction() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(64 << 10, 1);
        let root = RAM_BASE + 0x4000;
        let leaf = RAM_BASE + 0x5000;
        let va = 0x1000;
        bus.write::<8>(root, ((leaf >> 12) << 10) | PTE_V).unwrap();
        bus.write::<8>(leaf, ((RAM_BASE >> 12) << 10) | PTE_V | PTE_R | PTE_A).unwrap();
        bus.write::<1>(RAM_BASE + va, 7).unwrap();
        hart.csr_write(CSR_SATP, (8 << 60) | (root >> 12)).unwrap();
        hart.set_prv(PRV_S);
        assert_eq!(hart.load::<1>(&mut bus, va).unwrap(), 7);
        let unrelated = (1 << 30) + (1 << 21) + 0x3000;
        hart.ftlb_fill(&bus, unrelated, RAM_BASE + 0x3000, false);
        hart.sfence_vma(Some(unrelated), None);
        // This address shared the former 2 MiB hash with the live leaf.
        assert_eq!(unsafe { (*hart.ltlb_cur.add(ftlb_index(va))).tag }, va);
        hart.ftlb_fill(&bus, unrelated, RAM_BASE + 0x3000, false);
        // Evict this translation from the 2 MiB TLB, leaving its flat entry.
        hart.tlb.insert(TLB_BIG2M as u64 * 512, MASK_2M, RAM_BASE >> 12, 0, P_R);
        bus.write::<8>(leaf, 0).unwrap();
        hart.sfence_vma(Some(va + 4096), Some(1));
        assert_eq!(hart.load::<1>(&mut bus, va).unwrap(), 7);
        hart.sfence_vma(Some(va + 4096), None);
        assert_eq!(hart.load::<1>(&mut bus, va).unwrap_err().cause as u64, EXC_LOAD_PAGE);
        assert_eq!(unsafe { (*hart.ltlb_cur.add(ftlb_index(unrelated))).tag }, unrelated);
    }

    #[test]
    fn fence_clears_all_cached_slots_of_a_gigapage() {
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(64 << 10, 1);
        let root = RAM_BASE + 0x4000;
        let va = 0x1000;
        let alias = va + 4096 * FTLB_SETS as u64;
        let other = (1 << 30) + 0x3000;
        bus.write::<8>(root, ((RAM_BASE >> 12) << 10) | PTE_V | PTE_R | PTE_A).unwrap();
        hart.csr_write(CSR_SATP, (8 << 60) | (root >> 12)).unwrap();
        hart.set_prv(PRV_S);
        assert_eq!(hart.load::<1>(&mut bus, va).unwrap(), 0);
        // Both the primary slot and its eviction victim came from the same
        // region. A fence on a third page must discard them both.
        hart.ftlb_fill(&bus, alias, RAM_BASE + 0x2000, false);
        hart.ftlb_fill(&bus, other, RAM_BASE + 0x3000, false);
        bus.write::<8>(root, 0).unwrap();
        hart.sfence_vma(Some(0x8000), None);
        assert_eq!(hart.load::<1>(&mut bus, va).unwrap_err().cause as u64, EXC_LOAD_PAGE);
        assert_eq!(hart.load::<1>(&mut bus, alias).unwrap_err().cause as u64, EXC_LOAD_PAGE);
        assert_eq!(unsafe { (*hart.ltlb_cur.add(ftlb_index(other))).tag }, other);
    }

    #[test]
    fn store_reverse_hint_invalidates_single_and_aliased_mappings() {
        let mut h = Hart::new(0, false);
        let bus = Bus::new(64 << 10, 1);
        let pa = RAM_BASE + 0x6000;
        let a = 0x4000_1000; let b = a + 4096;
        h.ftlb_fill(&bus, a, pa, true);
        assert_ne!(h.stlb_slots[6], u16::MAX);
        assert!(h.drop_store_page(&bus, 6));
        assert_eq!(unsafe { (*h.stlb_cur.add(ftlb_index(a))).tag }, u64::MAX);
        h.ftlb_fill(&bus, a, pa, true);
        h.ftlb_fill(&bus, b, pa, true);
        assert_eq!(h.stlb_slots[6], u16::MAX);
        assert!(h.drop_store_page(&bus, 6));
        for va in [a, b] {
            assert_eq!(unsafe { (*h.stlb_cur.add(ftlb_index(va))).tag }, u64::MAX);
        }
    }

    #[test]
    fn stale_store_reverse_hints_do_not_invalidate_reused_slots() {
        let mut h = Hart::new(0, false);
        let bus = Bus::new(64 << 10, 1);
        let va = 0x4000_1000;
        h.ftlb_fill(&bus, va, RAM_BASE + 0x6000, true);
        h.ftlb_fill(&bus, va, RAM_BASE + 0x7000, true);
        assert!(h.drop_store_page(&bus, 6));
        assert_eq!(unsafe { (*h.stlb_cur.add(ftlb_index(va))).tag }, va);
        h.flush_store_tlb();
        let alias = va + 4096;
        h.ftlb_fill(&bus, alias, RAM_BASE + 0x6000, true);
        assert_ne!(h.stlb_slots[6], u16::MAX);
        assert!(h.drop_store_page(&bus, 6));
        assert_eq!(unsafe { (*h.stlb_cur.add(ftlb_index(alias))).tag }, u64::MAX);
    }

    #[test]
    fn store_reverse_hints_cover_inactive_privilege_contexts() {
        for aliased in [false, true] {
            let mut h = Hart::new(0, false);
            let bus = Bus::new(64 << 10, 1);
            let va = 0x4000_1000;
            let pa = RAM_BASE + 0x6000;
            h.satp_sv39 = true;
            h.prv = PRV_U;
            h.select_dctx();
            h.ftlb_fill(&bus, va, pa, true);
            let user_entry = unsafe { h.stlb_cur.add(ftlb_index(va)) };
            h.prv = PRV_S;
            h.select_dctx();
            if aliased {
                h.ftlb_fill(&bus, va, pa, true);
                assert_eq!(h.stlb_slots[6], u16::MAX);
            }
            let supervisor_entry = unsafe { h.stlb_cur.add(ftlb_index(va)) };
            h.ftlb_fill(&bus, va + 4096, RAM_BASE + 0x7000, true);
            h.ftlb_fill(&bus, va, pa, false);
            assert!(h.drop_store_page(&bus, 6));
            assert_eq!(unsafe { (*user_entry).tag }, u64::MAX);
            assert_eq!(unsafe { (*supervisor_entry).tag }, u64::MAX);
            assert_eq!(unsafe { (*h.stlb_cur.add(ftlb_index(va + 4096))).tag }, va + 4096);
            assert_eq!(unsafe { (*h.ltlb_cur.add(ftlb_index(va))).tag }, va);
        }
    }
}
