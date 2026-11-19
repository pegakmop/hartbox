//! Basic-block execution.
//!
//! A block is a straight run of decoded instructions. It starts at some pc and
//! ends at the first control transfer or system instruction, at a size limit,
//! or at a 4 KiB page boundary. Each block is decoded once and then reused, so
//! the hot loop pays for one translation and one cache lookup per block rather
//! than per instruction.
//!
//! Coherence: decoding a block marks the halfwords it came from in
//! `Bus::code_bits`. A store that touches a marked halfword bumps the page's
//! generation (`Bus::page_gen`), which invalidates the page's blocks in every
//! hart. Stores to data that only shares a page with code invalidate nothing.

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::Cell;
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::ptr;

use crate::arch::*;
use crate::bus::Bus;
use crate::hart::{ftlb_index, Hart, Res, Trap, RAS_SIZE};
use crate::insn::{decode16, decode32, DInsn, DecodeCache, Op, Step};
use crate::mmu::Access;

mod bulk;
mod dispatch;
mod fusion;
mod idioms;
mod linear_memory;
mod writeback;
#[cfg(test)]
mod overhaul_tests;
pub(crate) use dispatch::{invoke, pre1, pre2, pre3, pre4, pre5, pre6, pre7, pre8};
use dispatch::{operands, PRELOAD_OPERANDS};
use fusion::{commute_for_forwarding, first_forward, forward_store, jump_step, memory_with_gap, overwrite_pair, pair_step, quad_step, triple_step, xbr_step};

/// Longest block, in instructions.
const MAX_BLOCK: usize = 64;
/// Bound the interval between kick checks without loading an atomic at
/// every linked branch. Cooperative execution uses the full run budget.
const SMP_CHAIN_BUDGET: u64 = 2048;
/// Halfword slots in one 4 KiB page.
const SLOTS: usize = 2048;
const PAGE_CHUNK: usize = 64;
type StartChunk = [Option<ptr::NonNull<Block>>; PAGE_CHUNK];

// Link slots and decoded instructions are aligned, leaving this tag bit free.
const LINK_CHECKED: usize = 1;

/// Entries in the per-hart block cache (a power of two).
pub(crate) const BCACHE_SIZE: usize = 1024;

/// One entry of the per-hart block cache: the block entered at virtual pc `pc`
/// under link stamp `stamp` (translation epoch and code epoch). An entry whose
/// stamp is not the current one is never used, so a block that was freed after a
/// code write cannot be reached through it: the write bumps the code epoch first.
#[derive(Clone, Copy)]
#[repr(C, align(32))]
pub(crate) struct BEntry {
    pub(crate) pc: u64,
    pub(crate) stamp: u64,
    pub(crate) block: *const Block,
}

impl BEntry {
    pub(crate) const EMPTY: BEntry = BEntry { pc: u64::MAX, stamp: u64::MAX, block: ptr::null() };
}

#[inline(always)]
fn bcache_index(pc: u64) -> usize {
    (((pc >> 1) ^ (pc >> 11)) as usize) & (BCACHE_SIZE - 1)
}

/// A decoded straight-line run of instructions.
///
/// Blocks are keyed by physical address, so one block can be reached through
/// several virtual aliases of the same page. It therefore records no pc: the
/// executing hart supplies the virtual pc.
///
/// A block is one allocation: this 64-byte header, then its instructions. A link
/// that names a block therefore also names its first instruction (`ops_ptr`), so
/// entering a block takes no load to find its code, and the header and the first
/// instructions are neighbours in memory.
#[repr(C, align(64))]
pub struct Block {
    /// The two successors this block led to most recently.
    links: [LinkSlot; 2],
    /// Physical address of the first instruction.
    pa: u64,
    /// Instructions that follow the header, including the terminator.
    n: usize,
}

const _: () = assert!(std::mem::size_of::<Block>() == 64);

/// Per-hart storage keeps consecutively decoded blocks close together and
/// releases the working set when the hart is dropped. Block pointers never move.
pub(crate) struct BlockArena {
    chunks: Vec<ptr::NonNull<u8>>,
    used: usize,
    free: [Vec<ptr::NonNull<Block>>; MAX_BLOCK + 2],
}

const ARENA_LAYOUT: Layout = match Layout::from_size_align(4 << 20, 64) {
    Ok(layout) => layout,
    Err(_) => panic!("invalid block arena layout"),
};

impl Default for BlockArena {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            used: ARENA_LAYOUT.size(),
            free: std::array::from_fn(|_| Vec::new()),
        }
    }
}

impl BlockArena {
    fn insert(&mut self, ops: &[DInsn], pa: u64) -> ptr::NonNull<Block> {
        let n = ops.len();
        assert!(n <= MAX_BLOCK + 1);
        let size = (std::mem::size_of::<Block>() + n * std::mem::size_of::<DInsn>()).next_multiple_of(64);
        let p = if let Some(p) = self.free[n].pop() {
            p
        } else {
            if ARENA_LAYOUT.size() - self.used < size {
                let chunk = ptr::NonNull::new(unsafe { alloc(ARENA_LAYOUT) })
                    .unwrap_or_else(|| handle_alloc_error(ARENA_LAYOUT));
                self.chunks.push(chunk);
                self.used = 0;
            }
            let chunk = self.chunks.last().unwrap();
            // SAFETY: this entire block fits within the new or current chunk.
            let p = unsafe { chunk.add(self.used).cast::<Block>() };
            self.used += size;
            p
        };
        // SAFETY: the header and instructions fit in this aligned, exclusive slot.
        // Neither the header nor DInsn owns allocations or requires destruction.
        unsafe {
            ptr::write(p.as_ptr(), Block { links: [LinkSlot::empty(), LinkSlot::empty()], pa, n });
            ptr::copy_nonoverlapping(ops.as_ptr(), p.as_ptr().add(1).cast::<DInsn>(), n);
        }
        p
    }

    fn recycle(&mut self, block: ptr::NonNull<Block>) {
        let n = unsafe { block.as_ref().n };
        self.free[n].push(block);
    }
}

impl Drop for BlockArena {
    fn drop(&mut self) {
        for chunk in self.chunks.drain(..) {
            unsafe { dealloc(chunk.as_ptr(), ARENA_LAYOUT) };
        }
    }
}

/// A chain miss to fill after the run loop resolves its destination.
#[derive(Clone, Copy)]
pub(crate) struct PendingLink {
    source: *const Block,
    edge: Edge,
    target_pc: u64,
    stamp: u64,
}

#[derive(Clone, Copy)]
struct Edge(*const DInsn);

/// A link from a block to one of its successors. It holds while the hart keeps
/// the translations it had when the link was made and no decoded code has been
/// invalidated since. Only then is `target` still allocated.
#[repr(C)]
struct LinkSlot {
    /// Virtual pc of the successor.
    pc: Cell<u64>,
    /// `(translation epoch, code epoch)` of the hart when the link was made.
    stamp: Cell<u64>,
    /// The first threaded instruction, immediately after the successor header.
    entry: Cell<*const DInsn>,
}

impl LinkSlot {
    fn empty() -> LinkSlot {
        LinkSlot {
            pc: Cell::new(u64::MAX),
            stamp: Cell::new(u64::MAX),
            entry: Cell::new(ptr::null()),
        }
    }

    #[inline(always)]
    fn get(&self, pc: u64, stamp: u64) -> Option<*const DInsn> {
        (self.pc.get() == pc && self.stamp.get() == stamp).then(|| self.entry.get())
    }
}

impl Block {
    /// The block's first instruction.
    #[inline(always)]
    unsafe fn ops_ptr(block: *const Block) -> *const DInsn {
        // The instructions follow the header in the same allocation.
        block.add(1).cast()
    }

    /// The successor at `pc`, if this block linked to it and the link still holds.
    #[inline(always)]
    fn follow(&self, pc: u64, stamp: u64) -> Option<(&LinkSlot, *const DInsn)> {
        self.links.iter().find_map(|slot| slot.get(pc, stamp).map(|target| (slot, target)))
    }

    /// Remember the successor's first instruction at `pc`, most recent first.
    fn link(&self, pc: u64, stamp: u64, entry: *const DInsn) -> &LinkSlot {
        let [first, second] = &self.links;
        if first.pc.get() != pc {
            second.pc.set(first.pc.get());
            second.stamp.set(first.stamp.get());
            second.entry.set(first.entry.get());
        }
        first.pc.set(pc);
        first.stamp.set(stamp);
        first.entry.set(entry);
        first
    }
}

/// The blocks of one RAM page, indexed by the halfword they start at.
pub struct BlockPage {
    gen: u32,
    starts: [Option<Box<StartChunk>>; SLOTS / PAGE_CHUNK],
}

impl BlockPage {
    fn new(gen: u32) -> Self {
        BlockPage { gen, starts: std::array::from_fn(|_| None) }
    }

    #[inline(always)]
    fn get(&self, slot: usize) -> Option<ptr::NonNull<Block>> {
        self.starts[slot / PAGE_CHUNK].as_ref()?[slot % PAGE_CHUNK]
    }

    fn insert(&mut self, slot: usize, block: ptr::NonNull<Block>) -> Option<ptr::NonNull<Block>> {
        let chunk = self.starts[slot / PAGE_CHUNK]
            .get_or_insert_with(|| Box::new(std::array::from_fn(|_| None)));
        chunk[slot % PAGE_CHUNK].replace(block)
    }

    fn recycle(&mut self, arena: &mut BlockArena) {
        for chunk in self.starts.iter_mut().filter_map(Option::take) {
            for block in chunk.iter().flatten() {
                arena.recycle(*block);
            }
        }
    }
}

impl Hart {
    /// Discard decoded code when switching between cooperative and threaded harts.
    pub(crate) fn reset_blocks(&mut self) {
        self.bcache.fill(BEntry::EMPTY);
        self.ras.fill(crate::hart::RasEntry::EMPTY);
        self.ras_sp = 0;
        self.pending_link = None;
        self.blocks.clear();
        self.block_arena = BlockArena::default();
        self.flush_translation_cache();
    }

    /// Resume at the current pc, filling any chain miss that brought us here.
    #[inline(always)]
    pub(crate) fn next_block(&mut self, bus: &mut Bus) -> Res<Option<*const Block>> {
        // Snapshot before lookup: a block found just before a concurrent code write
        // must never be published with the newer epoch.
        let epoch = bus.code_epoch.load(Acquire);
        let stamp = (self.cache_stamp & !u64::from(u32::MAX)) | u64::from(epoch);
        self.cache_stamp = stamp;
        let pending = self.pending_link.take().filter(|p| p.stamp == stamp && p.target_pc == self.pc);
        // A new quantum, trap or chain break often resumes at a recently entered block.
        if let Some(next) = self.bcache_lookup(self.pc, stamp) {
            if let Some(p) = pending {
                unsafe { p.fill(next) };
            }
            return Ok(Some(next));
        }
        let next = self.block_at(bus)?;
        if let Some(to) = next {
            if bus.code_epoch.load(Acquire) == epoch {
                if let Some(p) = pending {
                    // SAFETY: a rebuild may reclaim the source, but only after the
                    // invalidation epoch is visible. Keep no reference across lookup.
                    unsafe { p.fill(to) };
                }
                self.bcache_fill(stamp, to);
            } else {
                // A rebuild may have recycled blocks from the sampled epoch.
                // Execute one uncached instruction before starting a fresh chain.
                return Ok(None);
            }
        }
        Ok(next)
    }

    #[inline(always)]
    fn bcache_lookup(&self, pc: u64, stamp: u64) -> Option<*const Block> {
        // SAFETY: the index is masked to the table size.
        let e = unsafe { &*self.bcache_ptr.add(bcache_index(pc)) };
        (e.pc == pc && e.stamp == stamp).then_some(e.block)
    }

    /// Remember that the block `to` starts at the current pc under `stamp`.
    #[inline(always)]
    pub(crate) fn bcache_fill(&mut self, stamp: u64, to: *const Block) {
        let pc = self.pc;
        self.note_link_page(pc);
        // SAFETY: the index is masked to the table size.
        unsafe { *self.bcache_ptr.add(bcache_index(pc)) = BEntry { pc, stamp, block: to } };
    }

    /// Record that a pc-keyed cache entry now exists for the virtual page of `pc`.
    #[inline(always)]
    pub(crate) fn note_link_page(&mut self, pc: u64) {
        let bit = ((pc >> 12) as usize) & (crate::hart::LINK_PAGE_WORDS * 64 - 1);
        // SAFETY: the index is masked to the table size.
        unsafe { *self.link_pages.get_unchecked_mut(bit >> 6) |= 1u64 << (bit & 63) };
    }

    /// Whether a pc-keyed cache entry may exist for the virtual page number `vpn`.
    pub(crate) fn page_has_links(&self, vpn: u64) -> bool {
        let bit = (vpn as usize) & (crate::hart::LINK_PAGE_WORDS * 64 - 1);
        self.link_pages[bit >> 6] & (1u64 << (bit & 63)) != 0
    }

    /// The block starting at the current pc, decoding it if needed.
    /// `Ok(None)` means the instruction straddles a page boundary and has to be
    /// executed through [`Hart::step_uncached`].
    #[inline(always)]
    pub(crate) fn block_at(&mut self, bus: &mut Bus) -> Res<Option<*const Block>> {
        let pc = self.pc;
        if pc & 1 != 0 {
            return Err(Trap::new(EXC_IFETCH_MISALIGN, pc));
        }
        let pa = self.fetch_pa(bus, pc)?;
        let off = Bus::ram_offset(pa);
        if off >= bus.ram.len() {
            return Err(Trap::new(EXC_IFETCH_ACCESS, pc));
        }
        let ppn = off >> 12;
        let slot = (off & 0xfff) >> 1;
        if let Some(Some(page)) = self.blocks.get(ppn) {
            if page.gen == bus.page_gen[ppn].load(Relaxed) {
                if let Some(block) = page.get(slot) {
                    return Ok(Some(block.as_ptr()));
                }
            }
        }
        self.build_block(bus, pa)
    }

    /// Decode the block starting at the physical address `pa` and cache it.
    #[cold]
    #[inline(never)]
    fn build_block(&mut self, bus: &mut Bus, pa: u64) -> Res<Option<*const Block>> {
        #[cfg(feature = "counters")]
        let t0 = std::time::Instant::now();
        let r = self.build_block_inner(bus, pa);
        #[cfg(feature = "counters")]
        crate::counters::add(&crate::counters::BUILD_NS, t0.elapsed().as_nanos() as u64);
        r
    }

    #[inline(never)]
    fn build_block_inner(&mut self, bus: &mut Bus, pa: u64) -> Res<Option<*const Block>> {
        crate::counters::inc(&crate::counters::BLOCKS_BUILT);
        // The page generation is read before the first byte is: a write that bumps it
        // later makes the block stale even though it is stamped when it is finished.
        #[cfg(feature = "counters")]
        let mut tp = std::time::Instant::now();
        #[cfg(feature = "counters")]
        macro_rules! phase {
            ($i:expr) => {{
                let n = tp.elapsed().as_nanos() as u64;
                crate::counters::PHASE_NS[$i].fetch_add(n, Relaxed);
                tp = std::time::Instant::now();
            }};
        }
        #[cfg(not(feature = "counters"))]
        macro_rules! phase {
            ($i:expr) => {};
        }
        // The decode buffer lives in the hart, so a build allocates nothing.
        let mut ops = std::mem::take(&mut self.build_ops);
        let gen = if bus.mt.is_some() {
            decode_page_block::<true>(bus, pa, self.id, &mut ops, &mut self.decode_cache)
        } else {
            decode_page_block::<false>(bus, pa, self.id, &mut ops, &mut self.decode_cache)
        };
        phase!(0);
        let Some(gen) = gen else {
            self.build_ops = ops;
            return Ok(None);
        };
        // A newly decoded page must not stay writable through the flat store TLB.
        self.sync_code_marks(bus);
        let last = ops.last().unwrap();
        ops.push(DInsn {
            off: last.off + i32::from(last.len),
            index: ops.len() as u8,
            ..DInsn::EMPTY
        });
        phase!(1);
        // The block engine indexes the register file without a bounds check.
        debug_assert!(ops.iter().all(|d| d.rd < 32 && d.rs1 < 32 && d.rs2 < 32));
        let n = ops.len();
        // A conditional branch with instructions after it falls through in the
        // block; only its taken edge exits.
        let mut flow_a = [false; MAX_BLOCK + 1];
        for i in 0..n {
            flow_a[i] = is_cond_branch(ops[i].op) && i + 1 < n - 1;
        }
        let flow = &flow_a[..n];
        // A backward branch can describe a regular RAM loop. Remember its
        // first closing branch and inspect the body only if fusion reaches it.
        let cooperative = bus.mt.is_none();
        let mut loop_ends = [0u8; MAX_BLOCK + 1];
        if cooperative {
            for (end, d) in ops[..n - 1].iter().enumerate() {
                if matches!(d.op, Op::Beq | Op::Bltu | Op::Bne) && d.imm < 0 {
                    let target = d.off + d.imm;
                    if let Ok(start) = ops[..end].binary_search_by_key(&target, |op| op.off) {
                        if loop_ends[start] == 0 {
                            loop_ends[start] = (end + 1) as u8;
                        }
                    }
                }
            }
        }
        // Plan fused units from left to right. The terminator is a separate
        // step, and a fusion can include a branch only if it handles the exit.
        // The step of every unit that starts at an index, when it is a fused one.
        let mut plan: [Option<Step>; MAX_BLOCK + 1] = [None; MAX_BLOCK + 1];
        let mut opaque_a = [false; MAX_BLOCK + 1];
        let opaque = &mut opaque_a[..n];
        let mut i = 0;
        // End of the last generated run: the fusion pass still plans inside a
        // run, so a run that bails on a memory instruction hands its tail to
        // the fused units it would have used anyway.
        let mut spec_until = 0usize;
        #[cfg(feature = "counters")]
        let skip: u32 = {
            static SKIP: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
            *SKIP.get_or_init(|| std::env::var("RVEMU_SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(0))
        };
        #[cfg(not(feature = "counters"))]
        let skip: u32 = 0;
        while i + 2 < n {
            let end = usize::from(loop_ends[i]);
            if let Some(bulk) = ops.get(i..end).and_then(bulk::plan) {
                ops[i].link.set(bulk.data);
                plan[i] = Some(bulk.step);
                i += bulk.len;
                continue;
            }
            // A generated superinstruction for this exact operand tuple wins
            // over the generic pairs, triples and quads: it covers more
            // instructions with fewer dispatches and touches the register file
            // only where the sequence's own values can be seen from outside.
            // SAFETY: `i` indexes `ops`, whose last entry is the terminator;
            // the matcher stops there.
            if i >= spec_until && skip & 1 == 0 {
                if let Some((len, step)) = unsafe { crate::spec::lookup(ops.as_ptr().add(i), n - i) } {
                    if i + len < n {
                        plan[i] = Some(step);
                        for k in 0..len {
                            opaque[i + k] = true;
                        }
                        spec_until = i + len;
                        i += 1;
                        continue;
                    }
                }
            }
            if i >= spec_until {
                normalize_integer(&mut ops[i]);
            }
            if let Some((len, step)) = writeback::plan(&ops[i..n - 1]) {
                plan[i] = Some(step);
                i += len;
                continue;
            }
            // Adjacent accesses through one unchanged base register can share
            // translation and bounds validation. This is selected from the ISA
            // operands alone; no program identities or profiles are involved.
            let memory = memory_run(&ops[i..n - 1], cooperative);
            if let Some((len, step)) = memory.filter(|(len, _)| *len >= 4) {
                plan[i] = Some(step);
                i += len;
                continue;
            }
            if skip & 32 == 0 && !flow[i] && !flow[i + 1] {
                if let Some(idiom) = special_pair(&ops[i], &ops[i + 1]) {
                    match idiom {
                        Idiom::Const(v) => {
                            ops[i].imm = v;
                            plan[i] = Some(step_const2 as Step);
                        }
                        Idiom::PcConst(v) => {
                            ops[i].imm = v;
                            plan[i] = Some(step_pcconst2 as Step);
                        }
                        Idiom::Zext32 => plan[i] = Some(step_zext2 as Step),
                        Idiom::PcRelJump => plan[i] = Some(pcrel_jump(&ops[i], &ops[i + 1])),
                    }
                    i += 2;
                    continue;
                }
            }
            if !flow[i] && fuses(&ops[i]) {
                let a = &ops[i];
                let b = &ops[i + 1];
                let forwards = produces(a.op) && a.rd != 0 && b.rs1 == a.rd;
                if let Some(step) = jump_step(a.op, b.op, forwards) {
                    plan[i] = Some(step);
                    i += 2;
                    continue;
                }
            }
            if let Some(step) = idioms::integer_result_pair(&ops[i], &ops[i + 1]) {
                plan[i] = Some(step);
                i += 2;
                continue;
            }
            if skip & 2 == 0 && i + 4 < n && !flow[i] && !flow[i + 1] && !flow[i + 2] && !flow[i + 3]
                && fuses(&ops[i]) && fuses(&ops[i + 1]) && fuses(&ops[i + 2]) && fuses(&ops[i + 3])
            {
                if let Some(step) = quad_step(ops[i].op, ops[i + 1].op, ops[i + 2].op, ops[i + 3].op) {
                    plan[i] = Some(step);
                    i += 4;
                    continue;
                }
            }
            if let Some((len, step)) = memory.filter(|(len, _)| *len == 3) {
                plan[i] = Some(step);
                i += len;
                continue;
            }
            if i + 3 < n {
                if let Some(step) = idioms::triple(&ops[i], &ops[i + 1], &ops[i + 2]) {
                    plan[i] = Some(step);
                    i += 3;
                    continue;
                }
                if let Some(step) = memory_with_gap(&ops[i], &ops[i + 1], &ops[i + 2]) {
                    plan[i] = Some(step);
                    i += 3;
                    continue;
                }
            }
            if skip & 4 == 0 && i + 3 < n && !flow[i] && !flow[i + 1] && !flow[i + 2]
                && fuses(&ops[i]) && fuses(&ops[i + 1]) && fuses(&ops[i + 2])
            {
                if let Some(step) = triple_step(&ops[i], &ops[i + 1], &ops[i + 2]) {
                    // Choose both swaps before changing any instruction fields.
                    let bc = commute_for_forwarding(&ops[i + 1], &ops[i + 2]);
                    commute_operands(&mut ops, i);
                    if bc {
                        let c = &mut ops[i + 2];
                        std::mem::swap(&mut c.rs1, &mut c.rs2);
                    }
                    plan[i] = Some(step);
                    i += 3;
                    continue;
                }
            }
            // A two-instruction address fold should not split a longer unit.
            if let Some(step) = idioms::address_pair(&ops[i], &ops[i + 1]) {
                plan[i] = Some(step);
                i += 2;
                continue;
            }
            if skip & 8 == 0 && !flow[i] && fuses(&ops[i]) {
                // Which operands of the branch are what `ops[i]` writes.
                let (a, br) = (&ops[i], &ops[i + 1]);
                let xf = if writes_register(a.op) && a.rd != 0 {
                    (br.rs1 == a.rd) as u8 | ((br.rs2 == a.rd) as u8) << 1
                } else {
                    0
                };
                if let Some(step) = xbr_step(a.op, br.op, xf | if br.rs2 == 0 { 4 } else { 0 }) {
                    plan[i] = Some(step);
                    i += 2;
                    continue;
                }
            }
            if let Some((len, step)) = memory.filter(|(len, _)| *len == 2) {
                plan[i] = Some(step);
                i += len;
                continue;
            }
            if let Some(step) = mixed_memory_pair(&ops[i], &ops[i + 1]) {
                plan[i] = Some(step);
                i += 2;
                continue;
            }
            if skip & 16 == 0 && !flow[i] && !flow[i + 1] && fuses(&ops[i]) && fuses(&ops[i + 1]) {
                let (a, b) = (&ops[i], &ops[i + 1]);
                if let Some(step) = forward_store(a, b).or_else(|| pair_step(a.op, b.op, first_forward(a, b))) {
                    commute_operands(&mut ops, i);
                    let step = overwrite_pair(&ops[i], &ops[i + 1]).unwrap_or(step);
                    plan[i] = Some(step);
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        if i == n - 2 && i >= spec_until {
            normalize_integer(&mut ops[i]);
        }
        phase!(2);
        for i in 0..n {
            ops[i].step = Some(match plan[i] {
                Some(step) => step,
                None => step_of(&ops, opaque, flow, i),
            });
        }
        fold_targets(&mut ops);
        phase!(3);
        let off = Bus::ram_offset(pa);
        let (ppn, slot) = (off >> 12, (off & 0xfff) >> 1);
        if self.blocks.len() <= ppn {
            self.blocks.resize_with(ppn + 1, || None);
        }
        let page = self.blocks[ppn].get_or_insert_with(|| Box::new(BlockPage::new(gen)));
        if page.gen != gen {
            // Blocks from before the last write to this page are stale.
            page.recycle(&mut self.block_arena);
            page.gen = gen;
        }
        let block = self.block_arena.insert(&ops, pa);
        if let Some(old) = page.insert(slot, block) {
            self.block_arena.recycle(old);
        }
        self.build_ops = ops;
        phase!(4);
        #[cfg(feature = "counters")]
        let _ = tp;
        Ok(Some(block.as_ptr()))
    }

    /// Assign the steps a block build gives an unfused instruction sequence. Only
    /// for tests, which run such a sequence through one generated step and
    /// through the ordinary steps, and compare the state both leave behind.
    #[cfg(test)]
    pub(crate) fn steps_for_test(ops: &mut [DInsn]) {
        let n = ops.len();
        // Every instruction of a generated run is marked fused in a real block, so
        // none of them takes an operand forwarded from another: a run that hands
        // an instruction back to its ordinary step relies on that.
        let fused = vec![true; n];
        // A conditional branch with instructions after it falls through, as in
        // a real block.
        let flow: Vec<bool> = (0..n).map(|i| is_cond_branch(ops[i].op) && i + 1 < n - 1).collect();
        for i in 0..n {
            let step = step_of(ops, &fused, &flow, i);
            ops[i].step = Some(step);
            ops[i].index = i as u8;
        }
    }

    /// Execute a block from the current pc, through the blocks it chains to, until a
    /// return to the run loop or a trap. `left` is the budget. Returns the number of
    /// instructions the run accounted for.
    #[inline(always)]
    pub(crate) unsafe fn exec_block(&mut self, bus: &mut Bus, block: *const Block, left: u64) -> u64 {
        let pc = self.pc;
        let base = Block::ops_ptr(block);
        let budget = if self.mt_ctl.is_null() {
            self.chain_limit.store(left, Relaxed);
            left
        } else {
            // Synchronize with a kick that cleared the previous budget before this
            // quantum could publish its new one.
            let budget = left.min(SMP_CHAIN_BUDGET);
            self.chain_limit.swap(budget, std::sync::atomic::Ordering::AcqRel);
            if unsafe { (*self.mt_ctl).kick.load(Acquire) } != 0 {
                return 0;
            }
            budget
        };
        self.chain_budget = budget;
        let chain = Chain::enter(self, pc, budget);
        // SAFETY: blocks end with a terminator entry, and every step moves
        // forward through the block's own instructions.
        #[cfg(feature = "counters")]
        { self.in_block = true; }
        let n = unsafe { go_tlb(chain, self.ltlb_cur, self, bus, base, 0, 0) };
        #[cfg(feature = "counters")]
        { self.in_block = false; }
        self.instret = self.instret.wrapping_add(n);
        n
    }

    /// Decode and run one instruction without caching it.
    #[cold]
    #[inline(never)]
    pub(crate) fn step_uncached(&mut self, bus: &mut Bus) {
        self.chained = 0;
        match self.decode_at(bus) {
            Ok(d) => {
                let pc = self.pc;
                self.pc = pc.wrapping_add(d.len as u64);
                match self.exec(bus, &d, pc) {
                    Ok(()) => self.instret = self.instret.wrapping_add(1),
                    Err(t) => {
                        self.pc = pc;
                        self.take_uncached_fault(t);
                    }
                }
            }
            Err(t) => self.take_uncached_fault(t),
        }
    }

    /// Decode the instruction at the current pc, translating both halves when
    /// it straddles a page boundary.
    fn decode_at(&mut self, bus: &mut Bus) -> Res<DInsn> {
        let pc = self.pc;
        if pc & 1 != 0 {
            return Err(Trap::new(EXC_IFETCH_MISALIGN, pc));
        }
        let pa = self.fetch_pa(bus, pc)?;
        if Bus::ram_offset(pa) >= bus.ram.len() {
            return Err(Trap::new(EXC_IFETCH_ACCESS, pc));
        }
        let lo = bus.read::<2>(pa).ok_or(Trap::new(EXC_IFETCH_ACCESS, pc))? as u16;
        if lo & 3 != 3 {
            return Ok(decode16(lo));
        }
        let high_pc = pc.wrapping_add(2);
        let hi_pa = if pc & 0xfff == 0xffe { self.translate(bus, high_pc, Access::Fetch)? } else { pa + 2 };
        let hi = bus.ram_ptr::<2>(hi_pa).ok_or(Trap::new(EXC_IFETCH_ACCESS, high_pc))?;
        let word = (unsafe { host_read::<2>(hi) } as u32) << 16 | lo as u32;
        Ok(decode_word(word, bus.mt.is_some()))
    }
}

fn commute_operands(ops: &mut [DInsn], i: usize) {
    if commute_for_forwarding(&ops[i], &ops[i + 1]) {
        let insn = &mut ops[i + 1];
        std::mem::swap(&mut insn.rs1, &mut insn.rs2);
    }
}

/// Decode from a translated, aligned RAM address. A RAM page is complete, and
/// the page limit is checked before either halfword load, so no byte needs to
/// go through the MMIO dispatcher or repeat the RAM bounds check.
fn decode_page_block<const THREADED: bool>(
    bus: &Bus, pa: u64, hart: u64, ops: &mut Vec<DInsn>, cache: &mut DecodeCache,
) -> Option<u32> {
    let off = Bus::ram_offset(pa);
    let ppn = off >> 12;
    let host = bus.ram.as_ptr() as usize + off;
    let page_left = 4096 - (off & 0xfff);
    let mut raw = [0u16; MAX_BLOCK * 2];
    // Concurrent modification can force retries. Fall back to one uncached
    // instruction if the page stays busy, so a writer cannot stall the hart.
    for _ in 0..4 {
        let gen = bus.code_generation(ppn);
        let mut at = 0;
        ops.clear();
        while at < page_left && ops.len() < MAX_BLOCK {
            let fits_word = at + 4 <= page_left;
            let word = if fits_word {
                unsafe { host_read::<4>(host + at) as u32 }
            } else {
                unsafe { host_read::<2>(host + at) as u32 }
            };
            let lo = word as u16;
            if lo & 3 == 3 {
                if !fits_word {
                    // The high half needs a separate instruction translation.
                    break;
                }
                if THREADED {
                    raw[at / 2 + 1] = (word >> 16) as u16;
                }
            }
            let mut d = cache.decode(word);
            if THREADED {
                d.op = threaded_fence(d.op);
                raw[at / 2] = lo;
            }
            #[cfg(feature = "counters")]
            {
                thread_local! { static SEEN: std::cell::RefCell<std::collections::HashSet<u64>> = Default::default(); }
                crate::counters::inc(&crate::counters::OPS_BUILT);
                if SEEN.with(|s| s.borrow_mut().insert(pa + at as u64)) {
                    crate::counters::inc(&crate::counters::OPS_UNIQUE);
                }
            }
            let end = d.op.ends_block()
                && !is_cond_branch(d.op)
                && !(d.op == Op::Csr && crate::csr::trap_state_register(d.imm as u16));
            d.off = at as i32;
            d.index = ops.len() as u8;
            at += d.len as usize;
            ops.push(d);
            if end {
                break;
            }
        }
        if ops.is_empty() {
            return None;
        }
        bus.mark_code(pa, at as u64, hart);
        if !THREADED || (raw[..at / 2].iter().enumerate().all(|(i, &word)| {
            // `mark_code` published the marks before this validation pass.
            unsafe { host_read::<2>(host + 2 * i) == word as u64 }
        }) && bus.page_gen[ppn].load(Acquire) == gen) {
            return Some(gen);
        }
    }
    None
}

#[inline(always)]
fn decode_word(word: u32, mt: bool) -> DInsn {
    let mut d = decode32(word);
    if mt {
        d.op = threaded_fence(d.op);
    }
    d
}

#[inline(always)]
fn threaded_fence(op: Op) -> Op {
    match op {
        Op::Fence => Op::FenceMt,
        Op::FenceI => Op::FenceIMt,
        op => op,
    }
}

/// Give every `jal` and conditional branch the offset of its target from the start of
/// the block in `imm` (the offset of the instruction plus its own displacement), so an
/// exit computes its target pc with one add. Run once the block's instruction offsets are
/// known and its generated runs have been matched (the matcher wants the displacements).
pub(crate) fn fold_targets(ops: &mut [DInsn]) {
    for d in ops.iter_mut() {
        if matches!(d.op, Op::Jal) || is_cond_branch(d.op) {
            d.imm = d.off.wrapping_add(d.imm);
        }
    }
}

/// The displacement a `jal` or branch had before `fold_targets`.
#[cfg(feature = "counters")]
fn unfolded_imm(d: &DInsn) -> i32 {
    if matches!(d.op, Op::Jal) || is_cond_branch(d.op) {
        d.imm.wrapping_sub(d.off)
    } else {
        d.imm
    }
}


// ------------------------------------------------------------ threaded steps
//
// A step runs one unit of a block and tail-calls the next one. Besides the hart,
// the bus, the instruction and the pc, it takes `fa` and `fb`: the preceding results, or a result and preloaded operands. A unit that writes a register also
// stores its result in the register file, so the register file is always
// current. For each operand, `build_block` decides once whether it is read from
// the register file or taken from `fa` or `fb`, so no run-time test is needed.

/// Operand source: the register file.
const MEM: u8 = 0;
/// Operand source: the result of the unit just before.
const FA: u8 = 1;
/// Operand source: the result of the unit two before.
const FB: u8 = 2;
/// Operand source: the architectural zero register.
const ZERO: u8 = 3;

/// Integer ALU operations with two register operands.
const ALU_RR: [Op; 17] = [
    Op::Add,
    Op::Sub,
    Op::And,
    Op::Or,
    Op::Xor,
    Op::Sll,
    Op::Srl,
    Op::Sra,
    Op::Slt,
    Op::Sltu,
    Op::Mul,
    Op::Addw,
    Op::Subw,
    Op::Sllw,
    Op::Srlw,
    Op::Sraw,
    Op::Mulw,
];

/// Integer ALU operations with a register and an immediate operand.
const ALU_IMM: [Op; 13] = [
    Op::Addi,
    Op::Andi,
    Op::Ori,
    Op::Xori,
    Op::Slli,
    Op::Srli,
    Op::Srai,
    Op::Addiw,
    Op::Slti,
    Op::Sltiu,
    Op::Slliw,
    Op::Srliw,
    Op::Sraiw,
];

const fn in_list(op: u8, list: &[Op]) -> bool {
    let mut i = 0;
    while i < list.len() {
        if list[i] as u8 == op {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether an instruction can be part of a generated run: it has no side
/// effect beyond its destination register and no branch.
pub(crate) const fn fusible(op: Op) -> bool {
    is_alu(op as u8) || matches!(op, Op::Lui | Op::Auipc | Op::Fence | Op::FenceI)
}

/// Whether the profiler can continue sampling after the instruction.
#[cfg(feature = "counters")]
const fn runnable(op: Op) -> bool {
    spec_candidate(op) && !matches!(op, Op::Jal | Op::Jalr)
}

/// Whether a conditional branch may end (or sit inside) a generated run.
pub(crate) const fn spec_branch(op: Op) -> bool {
    matches!(op, Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu)
}

/// Whether an instruction may appear inside a generated run.
pub(crate) const fn spec_candidate(op: Op) -> bool {
    fusible(op)
        || spec_branch(op)
        || matches!(op, Op::Jal | Op::Jalr)
        || matches!(
            op,
            Op::Ld | Op::Lw | Op::Lwu | Op::Lh | Op::Lhu | Op::Lb | Op::Lbu
                | Op::Sd | Op::Sw | Op::Sh | Op::Sb
        )
}

/// The comparison of the conditional branch `op` on `a` and `b`.
pub(crate) fn spec_taken(op: Op, a: u64, b: u64) -> bool {
    match op {
        Op::Beq => a == b,
        Op::Bne => a != b,
        Op::Blt => (a as i64) < (b as i64),
        Op::Bge => (a as i64) >= (b as i64),
        Op::Bltu => a < b,
        _ => a >= b,
    }
}

/// Whether the instruction writes a register (a store does not).
pub(crate) const fn writes_register(op: Op) -> bool {
    !matches!(op, Op::Sd | Op::Sw | Op::Sh | Op::Sb)
}

/// Load `W` bytes from `va` on the fast path, or `None` when the instruction
/// has to go through the ordinary step. Used by generated runs.
#[inline(always)]
pub(crate) unsafe fn spec_load<const W: usize, const SIGNED: bool>(tlb: *mut crate::hart::FastTlb, h: &Hart, _bus: &Bus, va: u64) -> Option<u64> {
    let raw = tlb_load::<W>(tlb, h, va)?;
    Some(if SIGNED { extend::<W, true>(raw) } else { raw })
}

/// Store `W` bytes on the fast path, or `false` when the ordinary step has to
/// run (the page holds decoded code, or the access is not plain RAM).
#[inline(always)]
pub(crate) unsafe fn spec_store<const W: usize>(tlb: *mut crate::hart::FastTlb, h: &Hart, _bus: &mut Bus, va: u64, v: u64) -> bool {
    tlb_store::<W>(tlb, h, va, v)
}

/// Read `N` bytes at `va` through the flat load TLB: a tag compare, an add and
/// the read. `None` means the access is not in the table (or is misaligned).
#[inline(always)]
pub(crate) unsafe fn tlb_load<const N: usize>(tlb: *mut crate::hart::FastTlb, _h: &Hart, va: u64) -> Option<u64> {
    let e = tlb_entry(tlb, va);
    if e.tag == va & (!0xfffu64 | (N as u64 - 1)) {
        Some(host_read::<N>(va.wrapping_add(e.addend) as usize))
    } else {
        None
    }
}

/// Write `N` bytes at `va` through the flat store TLB, which only holds pages
/// without decoded code. `false` means the access is not in the table.
#[inline(always)]
pub(crate) unsafe fn tlb_store<const N: usize>(tlb: *mut crate::hart::FastTlb, _h: &Hart, va: u64, v: u64) -> bool {
    let e = tlb_entry(tlb.add(crate::hart::FTLB_SETS), va);
    if e.tag == va & (!0xfffu64 | (N as u64 - 1)) {
        host_write::<N>(va.wrapping_add(e.addend) as usize, v);
        true
    } else {
        false
    }
}

/// Read the tag and address addend together. On ARM64, a pair load lets the
/// RAM access use the addend without issuing another load after the tag check.
#[inline(always)]
unsafe fn tlb_entry(tlb: *mut crate::hart::FastTlb, va: u64) -> crate::hart::FastTlb {
    let entry = tlb.add(ftlb_index(va));
    #[cfg(target_arch = "aarch64")]
    {
        let tag: u64;
        let addend: u64;
        core::arch::asm!(
            "ldp {tag}, {addend}, [{entry}]",
            entry = in(reg) entry,
            tag = lateout(reg) tag,
            addend = lateout(reg) addend,
            options(readonly, nostack, preserves_flags),
        );
        crate::hart::FastTlb { tag, addend }
    }
    #[cfg(not(target_arch = "aarch64"))]
    { *entry }
}

/// Hand a generated run back to the ordinary step for instruction `k` of it,
/// at index `k`. Used when a memory instruction cannot take the fast path.
pub(crate) unsafe fn spec_step_at(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    k: usize,
) -> u64 {
    let at = d.add(k);
    invoke(STEPS[(*at).op as usize], chain, tlb, h, bus, at, 0, 0)
}

const fn is_alu(op: u8) -> bool {
    in_list(op, &ALU_RR) || in_list(op, &ALU_IMM)
}

/// Whether `op` is an ALU operation whose second operand is the immediate.
const fn is_imm(op: u8) -> bool {
    in_list(op, &ALU_IMM)
}

/// A pure instruction only writes its destination register, with no other effect and
/// no possible trap. Such an instruction with destination `x0` can be a no-op.
const fn pure_register_op(op: u8) -> bool {
    is_alu(op) || op == Op::Lui as u8 || op == Op::Auipc as u8
}

/// Whether `op` passes its register result on to the unit after it: a single
/// instruction that writes its destination with the value it computed.
fn produces(op: Op) -> bool {
    is_alu(op as u8) || matches!(op, Op::Lui | Op::Auipc | Op::Ld | Op::Lw | Op::Lwu | Op::Lh | Op::Lhu | Op::Lb | Op::Lbu)
}

/// Canonicalize identities that hold for every register value. This runs after
/// the existing template lookup, before selecting generic fused handlers.
fn normalize_integer(d: &mut DInsn) {
    if d.rs1 == 0 && is_imm(d.op as u8) {
        d.imm = match d.op {
            Op::Addi | Op::Addiw | Op::Ori | Op::Xori => d.imm,
            Op::Slti => i32::from(d.imm > 0),
            Op::Sltiu => i32::from(d.imm != 0),
            _ => 0,
        };
        d.op = Op::Lui;
    } else if d.rs1 == d.rs2 && matches!(d.op, Op::Sub | Op::Subw | Op::Xor | Op::Slt | Op::Sltu)
        || d.rs2 == 0 && matches!(d.op, Op::And | Op::Mul | Op::Mulw)
        || d.op == Op::Andi && d.imm == 0
    {
        d.op = Op::Lui;
        d.imm = 0;
    } else if d.rs1 == 0 && matches!(d.op, Op::Add | Op::Or | Op::Xor) {
        d.op = Op::Addi;
        d.rs1 = d.rs2;
        d.imm = 0;
    } else if d.rs2 == 0 && matches!(d.op, Op::Add | Op::Sub | Op::Or | Op::Xor | Op::Sll | Op::Srl | Op::Sra) {
        d.op = Op::Addi;
        d.imm = 0;
    } else if d.rs2 == 0 && matches!(d.op, Op::Addw | Op::Subw | Op::Sllw | Op::Srlw | Op::Sraw) {
        d.op = Op::Addiw;
        d.imm = 0;
    }
}

/// Step for each opcode, reading every operand from the register file.
pub(crate) const STEPS: [Step; 128] = steps();

const fn steps() -> [Step; 128] {
    let mut t: [Step; 128] = [step_generic; 128];
    t[Op::Empty as usize] = step_end;
    // Fences order memory operations; this machine is single-threaded and in
    // order, so a fence is a no-op that only has to advance.
    t[Op::Fence as usize] = step_nop;
    t[Op::FenceI as usize] = step_nop;
    t[Op::FenceMt as usize] = step_fence_mt;
    t[Op::FenceIMt as usize] = step_fence_i_mt;
    t[Op::Lui as usize] = step_lui;
    t[Op::Auipc as usize] = step_auipc;
    t[Op::Jal as usize] = step_jal;
    t[Op::Add as usize] = step_alu::<{ Op::Add as u8 }, MEM, MEM>;
    t[Op::Sub as usize] = step_alu::<{ Op::Sub as u8 }, MEM, MEM>;
    t[Op::And as usize] = step_alu::<{ Op::And as u8 }, MEM, MEM>;
    t[Op::Or as usize] = step_alu::<{ Op::Or as u8 }, MEM, MEM>;
    t[Op::Xor as usize] = step_alu::<{ Op::Xor as u8 }, MEM, MEM>;
    t[Op::Sll as usize] = step_alu::<{ Op::Sll as u8 }, MEM, MEM>;
    t[Op::Srl as usize] = step_alu::<{ Op::Srl as u8 }, MEM, MEM>;
    t[Op::Sra as usize] = step_alu::<{ Op::Sra as u8 }, MEM, MEM>;
    t[Op::Slt as usize] = step_alu::<{ Op::Slt as u8 }, MEM, MEM>;
    t[Op::Sltu as usize] = step_alu::<{ Op::Sltu as u8 }, MEM, MEM>;
    t[Op::Mul as usize] = step_alu::<{ Op::Mul as u8 }, MEM, MEM>;
    t[Op::Addw as usize] = step_alu::<{ Op::Addw as u8 }, MEM, MEM>;
    t[Op::Subw as usize] = step_alu::<{ Op::Subw as u8 }, MEM, MEM>;
    t[Op::Sllw as usize] = step_alu::<{ Op::Sllw as u8 }, MEM, MEM>;
    t[Op::Srlw as usize] = step_alu::<{ Op::Srlw as u8 }, MEM, MEM>;
    t[Op::Sraw as usize] = step_alu::<{ Op::Sraw as u8 }, MEM, MEM>;
    t[Op::Mulw as usize] = step_alu::<{ Op::Mulw as u8 }, MEM, MEM>;
    t[Op::Addi as usize] = step_alui::<{ Op::Addi as u8 }, MEM>;
    t[Op::Andi as usize] = step_alui::<{ Op::Andi as u8 }, MEM>;
    t[Op::Ori as usize] = step_alui::<{ Op::Ori as u8 }, MEM>;
    t[Op::Xori as usize] = step_alui::<{ Op::Xori as u8 }, MEM>;
    t[Op::Slli as usize] = step_alui::<{ Op::Slli as u8 }, MEM>;
    t[Op::Srli as usize] = step_alui::<{ Op::Srli as u8 }, MEM>;
    t[Op::Srai as usize] = step_alui::<{ Op::Srai as u8 }, MEM>;
    t[Op::Addiw as usize] = step_alui::<{ Op::Addiw as u8 }, MEM>;
    t[Op::Slti as usize] = step_alui::<{ Op::Slti as u8 }, MEM>;
    t[Op::Sltiu as usize] = step_alui::<{ Op::Sltiu as u8 }, MEM>;
    t[Op::Slliw as usize] = step_alui::<{ Op::Slliw as u8 }, MEM>;
    t[Op::Srliw as usize] = step_alui::<{ Op::Srliw as u8 }, MEM>;
    t[Op::Sraiw as usize] = step_alui::<{ Op::Sraiw as u8 }, MEM>;
    t[Op::Ld as usize] = step_load::<8, false, MEM>;
    t[Op::Lw as usize] = step_load::<4, true, MEM>;
    t[Op::Lwu as usize] = step_load::<4, false, MEM>;
    t[Op::Lh as usize] = step_load::<2, true, MEM>;
    t[Op::Lhu as usize] = step_load::<2, false, MEM>;
    t[Op::Lb as usize] = step_load::<1, true, MEM>;
    t[Op::Lbu as usize] = step_load::<1, false, MEM>;
    t[Op::Sd as usize] = step_store::<8, MEM, MEM>;
    t[Op::Sw as usize] = step_store::<4, MEM, MEM>;
    t[Op::Sh as usize] = step_store::<2, MEM, MEM>;
    t[Op::Sb as usize] = step_store::<1, MEM, MEM>;
    t[Op::Beq as usize] = step_br::<{ Op::Beq as u8 }, MEM, MEM>;
    t[Op::Bne as usize] = step_br::<{ Op::Bne as u8 }, MEM, MEM>;
    t[Op::Blt as usize] = step_br::<{ Op::Blt as u8 }, MEM, MEM>;
    t[Op::Bge as usize] = step_br::<{ Op::Bge as u8 }, MEM, MEM>;
    t[Op::Bltu as usize] = step_br::<{ Op::Bltu as u8 }, MEM, MEM>;
    t[Op::Bgeu as usize] = step_br::<{ Op::Bgeu as u8 }, MEM, MEM>;
    t[Op::Jalr as usize] = step_jalr::<MEM>;
    t[Op::Mulh as usize] = step_muldiv::<{ Op::Mulh as u8 }>;
    t[Op::Mulhsu as usize] = step_muldiv::<{ Op::Mulhsu as u8 }>;
    t[Op::Mulhu as usize] = step_muldiv::<{ Op::Mulhu as u8 }>;
    t[Op::Div as usize] = step_muldiv::<{ Op::Div as u8 }>;
    t[Op::Divu as usize] = step_muldiv::<{ Op::Divu as u8 }>;
    t[Op::Rem as usize] = step_muldiv::<{ Op::Rem as u8 }>;
    t[Op::Remu as usize] = step_muldiv::<{ Op::Remu as u8 }>;
    t[Op::Divw as usize] = step_muldiv::<{ Op::Divw as u8 }>;
    t[Op::Divuw as usize] = step_muldiv::<{ Op::Divuw as u8 }>;
    t[Op::Remw as usize] = step_muldiv::<{ Op::Remw as u8 }>;
    t[Op::Remuw as usize] = step_muldiv::<{ Op::Remuw as u8 }>;
    t[Op::Amo as usize] = step_amo;
    t[Op::Lr as usize] = step_lr;
    t[Op::Sc as usize] = step_sc;
    t
}

/// State shared by all steps of a chain. The two ARM64 fields are scalar
/// arguments, keeping the PC and remaining budget in registers across handler tail calls.
/// x86-64 already uses all six integer argument registers, so it uses the
/// hart fields instead.
#[derive(Clone, Copy)]
pub struct Chain {
    #[cfg(target_arch = "aarch64")]
    pc: u64,
    #[cfg(target_arch = "aarch64")]
    left: u64,
}

impl Chain {
    #[inline(always)]
    fn enter(_h: &mut Hart, _pc: u64, _left: u64) -> Self {
        #[cfg(target_arch = "aarch64")]
        { Self { pc: _pc, left: _left } }
        #[cfg(not(target_arch = "aarch64"))]
        {
            _h.block_pc = _pc;
            _h.chained = _left;
            Self {}
        }
    }

    #[cfg(test)]
    pub(crate) fn from_hart(_h: &Hart) -> Self {
        #[cfg(target_arch = "aarch64")]
        { Self { pc: _h.block_pc, left: _h.chain_budget } }
        #[cfg(not(target_arch = "aarch64"))]
        { Self {} }
    }

    #[inline(always)]
    fn pc(self, _h: &Hart) -> u64 {
        #[cfg(target_arch = "aarch64")]
        { self.pc }
        #[cfg(not(target_arch = "aarch64"))]
        { _h.block_pc }
    }

    #[inline(always)]
    fn left(self, _h: &Hart) -> u64 {
        #[cfg(target_arch = "aarch64")]
        { self.left }
        #[cfg(not(target_arch = "aarch64"))]
        { _h.chained }
    }

    #[inline(always)]
    fn executed(self, h: &Hart, in_block: u64) -> u64 {
        h.chain_budget - self.left(h) + in_block
    }
}

#[inline(always)]
pub(crate) unsafe fn pc_of(chain: Chain, h: &Hart, d: *const DInsn) -> u64 {
    chain.pc(h).wrapping_add((*d).off as i64 as u64)
}

#[inline(always)]
unsafe fn retired(_h: &Hart, d: *const DInsn) -> u64 {
    (*d).index as u64
}

/// Use this only on an installed block; test-only instruction arrays have no header.
#[inline(always)]
unsafe fn block_of(d: *const DInsn) -> *const Block {
    d.sub((*d).index as usize).cast::<Block>().sub(1)
}

impl PendingLink {
    /// The caller validated both the source and the resolved destination's stamp.
    unsafe fn fill(self, target: *const Block) {
        let entry = Block::ops_ptr(target);
        let slot = (*self.source).link(self.target_pc, self.stamp, entry);
        self.edge.cache(slot, entry);
    }
}

impl Edge {
    const INDIRECT: Self = Self(ptr::null());

    #[inline(always)]
    fn same_page(d: *const DInsn) -> Self {
        Self(d)
    }

    #[inline(always)]
    fn cross_page(d: *const DInsn) -> Self {
        Self(d.map_addr(|address| address | LINK_CHECKED))
    }

    /// A same-page edge needs only the successor. A cross-page edge points to
    /// the stamped slot and checks its PC as well, since the slot can be reused
    /// or the source block can run under a different virtual alias.
    #[inline(always)]
    unsafe fn cache(self, slot: &LinkSlot, entry: *const DInsn) {
        if !self.0.is_null() {
            let checked = self.0.addr() & LINK_CHECKED != 0;
            let d = self.0.map_addr(|address| address & !LINK_CHECKED);
            let link = if checked {
                slot as *const LinkSlot as usize | LINK_CHECKED
            } else {
                entry as usize
            };
            (*d).link.set(link);
        }
    }
}

/// Leave through an instruction that has already set `h.pc` to a dynamic target.
#[inline(always)]
unsafe fn finish(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, k: u64) -> u64 {
    let left = chain.left(h);
    crate::counters::inc(&crate::counters::EXITS);
    if k < left {
        return follow_or_return(tlb, h, bus, left - k, d, Edge::INDIRECT);
    }
    chain.executed(h, k)
}

/// A call's terminator is never executed. Its link slot caches the continuation
/// when it lies in the same page as the call, independent of virtual aliases.
#[inline(always)]
pub(crate) unsafe fn ras_push_at(chain: Chain, h: &mut Hart, d: *const DInsn) {
    let ret = pc_of(chain, h, d).wrapping_add((*d).len as u64);
    ras_push_return(h, d.add(1), ret);
}

#[inline(always)]
unsafe fn ras_push_return(h: &mut Hart, term: *const DInsn, ret: u64) {
    let sp = h.ras_sp.wrapping_add(1);
    h.ras_sp = sp;
    let e = h.ras.get_unchecked_mut(sp & (RAS_SIZE - 1));
    let same_page = ret & 0xfff != 0;
    e.pc = if same_page { ret } else { u64::MAX };
    e.stamp = h.cache_stamp;
    e.term = term;
    e.next = if same_page { (*term).link.get() } else { 0 };
    // A cached continuation is on the call page, which was already recorded
    // when the chain entered it.
}

#[inline(always)]
pub(crate) unsafe fn finish_jalr(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn) -> u64 {
    finish_jalr_to(chain, tlb, h, bus, d, h.pc)
}

#[inline(always)]
unsafe fn finish_jalr_to(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, target: u64) -> u64 {
    let di = &*d;
    if di.rd == 0 {
        if (di.rs1 == 1 || di.rs1 == 5) && di.imm == 0 {
            return finish_ret(chain, tlb, h, bus, d, target);
        }
    } else if di.rd == 1 || di.rd == 5 {
        ras_push_at(chain, h, d);
    }
    finish_indirect_to(chain, tlb, h, bus, d, target)
}

/// Most indirect branches hit their most recent target. Keep this check
/// in the branch handler, with the guest PC still in a host register.
#[inline(always)]
unsafe fn finish_indirect_to(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, target: u64) -> u64 {
    let k = retired(h, d) + 1;
    let left = chain.left(h);
    crate::counters::inc(&crate::counters::EXITS);
    if k < left {
        let remaining = left - k;
        if let Some(next) = (*block_of(d)).links[0].get(target, h.cache_stamp) {
            crate::counters::inc(&crate::counters::LINK_HIT);
            crate::counters::inc(&crate::counters::BLOCKS_CHAINED);
            let state = Chain::enter(h, target, remaining);
            return go_tlb(state, tlb, h, bus, next, 0, 0);
        }
        h.pc = target;
        return follow_or_return(tlb, h, bus, remaining, d, Edge::INDIRECT);
    }
    h.pc = target;
    chain.executed(h, k)
}

/// Predict only the block pointer. The actual guest target and fetch stamp must
/// match the return-stack entry before its cached continuation can be used.
#[inline(always)]
unsafe fn finish_ret(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, target: u64) -> u64 {
    let k = retired(h, d) + 1;
    let left = chain.left(h);
    crate::counters::inc(&crate::counters::EXITS);
    let sp = h.ras_sp;
    h.ras_sp = sp.wrapping_sub(1);
    if k < left {
        let remaining = left - k;
        let e = *h.ras.get_unchecked(sp & (RAS_SIZE - 1));
        // The chain's blocks remain allocated until control returns to the run
        // loop. FENCE.I detects a newer code epoch before continuing.
        if e.pc == target && e.stamp == h.cache_stamp {
            if e.next != 0 {
                crate::counters::inc(&crate::counters::LINK_HIT);
                crate::counters::inc(&crate::counters::BLOCKS_CHAINED);
                let next = Chain::enter(h, target, remaining);
                return go_tlb(next, tlb, h, bus, e.next as *const DInsn, 0, 0);
            }
            h.pc = target;
            return follow_or_return(tlb, h, bus, remaining, d, Edge::same_page(e.term));
        }
        h.pc = target;
        return follow_or_return(tlb, h, bus, remaining, d, Edge::INDIRECT);
    }
    h.pc = target;
    chain.executed(h, k)
}

/// Wrapper for generated handlers that already materialized the destination PC.
#[inline(always)]
pub(crate) unsafe fn finish_direct(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, k: u64) -> u64 {
    finish_direct_to(chain, tlb, h, bus, d, k, h.pc, 0, 0)
}

/// Fixed-target exit. A same-page link shares the source block's physical
/// generation and fetch permission. Cross-page links must check the stamp.
#[inline(always)]
pub(crate) unsafe fn finish_direct_to(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    k: u64,
    target: u64,
    fa: u64,
    fb: u64,
) -> u64 {
    let left = chain.left(h);
    crate::counters::inc(&crate::counters::EXITS);
    #[cfg(feature = "counters")]
    if h.in_block {
        classify_inblock(chain, h, d, target);
    }
    if k < left {
        let remaining = left - k;
        let link = (*d).link.get();
        let next = if link & LINK_CHECKED == 0 {
            link as *const DInsn
        } else {
            let slot = &*((link & !LINK_CHECKED) as *const LinkSlot);
            slot.get(target, h.cache_stamp).unwrap_or(ptr::null())
        };
        if !next.is_null() {
            crate::counters::inc(&crate::counters::LINK_HIT);
            crate::counters::inc(&crate::counters::BLOCKS_CHAINED);
            let state = Chain::enter(h, target, remaining);
            return go_tlb(state, tlb, h, bus, next, fa, fb);
        }
        h.pc = target;
        let same_page = (target ^ chain.pc(h)) >> 12 == 0;
        let edge = if same_page { Edge::same_page(d) } else { Edge::cross_page(d) };
        return follow_or_return(tlb, h, bus, remaining, d, edge);
    }
    h.pc = target;
    chain.executed(h, k)
}

/// Counters only: is the target of this taken exit an instruction of the same block?
#[cfg(feature = "counters")]
#[cold]
#[inline(never)]
unsafe fn classify_inblock(chain: Chain, h: &Hart, d: *const DInsn, target: u64) {
    let blk = block_of(d);
    let ops = Block::ops_ptr(blk);
    let base = chain.pc(h);
    let toff = target.wrapping_sub(base) as i64;
    let boff = (*d).off as i64;
    for i in 0..(*blk).n {
        if (*ops.add(i)).off as i64 == toff && target >= base {
            if i == 0 {
                crate::counters::inc(&crate::counters::INB_SELF);
            } else if toff > boff {
                crate::counters::inc(&crate::counters::INB_FWD);
                crate::counters::add(&crate::counters::INB_FWD_SKIP, (i as u64) - (*d).index as u64 - 1);
            } else {
                crate::counters::inc(&crate::counters::INB_BACK);
            }
            return;
        }
    }
    crate::counters::inc(&crate::counters::INB_OUT);
}

/// Resolve a warmed successor. Only a miss needs to remember the source block
/// for the run loop, so linked exits never store an active-block pointer.
#[inline(never)]
unsafe fn follow_or_return(tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, remaining: u64, d: *const DInsn, edge: Edge) -> u64 {
    if edge.0.is_null() {
        crate::counters::inc(&crate::counters::FOLLOW_INDIRECT);
    } else {
        crate::counters::inc(&crate::counters::FOLLOW_DIRECT);
    }
    // Within a chain all blocks from this epoch stay allocated. A synchronized
    // code change ends the chain, where next_block will sample the new epoch.
    let stamp = h.cache_stamp;
    let cur = &*block_of(d);
    let pc = h.pc;
    let pending = PendingLink { source: cur, edge, target_pc: pc, stamp };
    let next = if let Some((slot, next)) = cur.follow(pc, stamp) {
        crate::counters::inc(&crate::counters::LINK_HIT);
        edge.cache(slot, next);
        next
    } else if let Some(next) = h.bcache_lookup(pc, stamp) {
        crate::counters::inc(&crate::counters::BCACHE_HIT);
        pending.fill(next);
        Block::ops_ptr(next)
    } else {
        match revalidate(h, bus, cur, stamp) {
            Some(next) => {
                pending.fill(next);
                Block::ops_ptr(next)
            }
            None => {
                crate::counters::inc(&crate::counters::LINK_MISS);
                h.pending_link = Some(pending);
                return h.chain_budget - remaining;
            }
        }
    };
    crate::counters::inc(&crate::counters::BLOCKS_CHAINED);
    let next_chain = Chain::enter(h, pc, remaining);
    go_tlb(next_chain, tlb, h, bus, next, 0, 0)
}

/// A cached target from the same code epoch still exists. Translate once and
/// check its physical address to reuse it after a translation-context change.
#[inline(never)]
unsafe fn revalidate(h: &mut Hart, bus: &mut Bus, cur: &Block, stamp: u64) -> Option<*const Block> {
    let pc = h.pc;
    let code = stamp as u32;
    let mut cand: *const Block = ptr::null();
    for slot in &cur.links {
        if slot.pc.get() == pc && slot.stamp.get() as u32 == code {
            // Every linked entry starts immediately after its owning header.
            let entry = slot.entry.get();
            if !entry.is_null() {
                cand = entry.cast::<Block>().sub(1);
            }
            break;
        }
    }
    if cand.is_null() {
        let e = &*h.bcache_ptr.add(bcache_index(pc));
        if e.pc == pc && e.stamp as u32 == code {
            cand = e.block;
        }
    }
    if cand.is_null() {
        return None;
    }
    // A fault is left to the run loop, which takes the trap.
    let pa = h.fetch_pa(bus, pc).ok()?;
    if pa != (*cand).pa {
        return None;
    }
    crate::counters::inc(&crate::counters::BCACHE_HIT);
    h.bcache_fill(stamp, cand);
    Some(cand)
}

#[cfg(test)]
unsafe fn go(h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    go_tlb(Chain::from_hart(h), h.ltlb_cur, h, bus, d, fa, fb)
}

/// Run the unit at `d`, preserving the active TLB and (on ARM64) the count at
/// this block's entry. The native dispatch supplies the current operands in
/// the argument that otherwise holds the second forwarded result.
#[inline(always)]
pub(crate) unsafe fn go_tlb(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    crate::counters::inc_op((*d).op as u8);
    #[cfg(feature = "counters")]
    unsafe {
        crate::counters::sample_tick(|| sample_run_at(d));
    }
    #[cfg(feature = "counters")]
    if (*d).op != Op::Empty && std::ptr::fn_addr_eq((*d).step.unwrap_unchecked(), STEPS[(*d).op as usize]) {
        crate::counters::inc(&crate::counters::SINGLE_UNITS);
        // A unit that is not part of a fused group: what follows it is the
        // best candidate for the next fusion.
        crate::counters::inc_single((*d).op as u8, (*d.add(1)).op as u8);
    }
    if (*d).op != Op::Empty {
        let next = (*d.add(1)).op;
        crate::counters::inc_bigram((*d).op as u8, next as u8);
        // `d + 2` is in the block whenever `d + 1` is not the terminator.
        if next != Op::Empty {
            let third = (*d.add(2)).op;
            crate::counters::inc_trigram((*d).op as u8, next as u8, third as u8);
            if third != Op::Empty {
                crate::counters::inc_quad((*d).op as u8, next as u8, third as u8, (*d.add(3)).op as u8);
            }
        }
    }
    // SAFETY: every instruction of a built block has its step set, and the
    // terminator is never run past.
    invoke((*d).step.unwrap_unchecked(), chain, tlb, h, bus, d, fa, fb)
}

/// Record the run starting at `d`, for template mining. Cold and never inlined:
/// it carries the run buffer, which must not be part of the hot path's frame.
#[cfg(feature = "counters")]
#[cold]
#[inline(never)]
unsafe fn sample_run_at(d: *const DInsn) {
    let di0 = &*d;
    // A run starts at the block's first instruction, after anything a run cannot
    // contain, or right after a conditional branch (which ends the run before
    // it, and whose fall-through starts the next).
    let natural = di0.index == 0 || !runnable((*d.sub(1)).op) || spec_branch((*d.sub(1)).op);
    if !natural || !runnable(di0.op) || (writes_register(di0.op) && di0.rd == 0) {
        return;
    }
    let mut words = [0u64; crate::counters::RUN_MAX * 2];
    let mut n = 0usize;
    let mut branches = 0usize;
    while n < crate::counters::RUN_MAX {
        let di = &*d.add(n);
        if !runnable(di.op) || (writes_register(di.op) && di.rd == 0) {
            break;
        }
        if spec_branch(di.op) {
            // A run may hold a second branch: its taken edge simply leaves the
            // block, and the run goes on when it is not taken. It stops at the
            // second branch and keeps it, since a run that ends on a branch also
            // removes the dispatch that would follow it.
            words[n * 2] = crate::counters::run_word0(di.op as u8, di.rd, di.rs1, di.rs2);
            words[n * 2 + 1] = crate::counters::run_word1(unfolded_imm(di), di.len);
            n += 1;
            branches += 1;
            if branches == 2 {
                break;
            }
            continue;
        }
        words[n * 2] = crate::counters::run_word0(di.op as u8, di.rd, di.rs1, di.rs2);
        words[n * 2 + 1] = crate::counters::run_word1(unfolded_imm(di), di.len);
        n += 1;
    }
    // The block's own last instruction (a branch, or the jump that ends it) is
    // part of the run when nothing follows it: a run that ends the block saves
    // the terminator's dispatch too.
    let tail = &*d.add(n);
    if (*d.add(n + 1)).op == Op::Empty && (spec_branch(tail.op) || matches!(tail.op, Op::Jal | Op::Jalr)) {
        words[n * 2] = crate::counters::run_word0(tail.op as u8, tail.rd, tail.rs1, tail.rs2);
        words[n * 2 + 1] = crate::counters::run_word1(unfolded_imm(tail), tail.len);
        n += 1;
    }
    if n >= 3 {
        crate::counters::sample_run(&words[..n * 2]);
    }
}

/// Run the unit after `d`, which is at `pc`. `res` is what `d` wrote to its
/// destination (0 if it wrote none), and `fa` is the value `d` itself received
/// as its `fa`.
#[inline(always)]
unsafe fn advance(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, res: u64, fa: u64) -> u64 {
    go_tlb(chain, tlb, h, bus, d.add(1), res, fa)
}

/// A unit that writes no register, or one whose result is not forwarded: `fa`
/// passes through as the result two units back.
#[inline(never)]
unsafe fn step_nop(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, _fb: u64) -> u64 {
    let nx = pre1(d);
    nx.run(chain, tlb, h, bus, fa, fa)
}

/// `fence` with several host threads: a full barrier, so the order the guest asked for is
/// the order other harts see.
#[inline(never)]
unsafe fn step_fence_mt(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, _fb: u64) -> u64 {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    let nx = pre1(d);
    nx.run(chain, tlb, h, bus, fa, fa)
}

/// `fence.i` with several host threads: a barrier, then take in the pages other harts have
/// decoded since this hart last looked, so stores this hart made through a stale flat
/// store entry cannot leave their decoded blocks valid.
#[inline(never)]
unsafe fn step_fence_i_mt(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, _fb: u64) -> u64 {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    h.sync_code_marks(bus);
    if chain_interrupted(h, bus) {
        return stop_after(chain, h, d);
    }
    let nx = pre1(d);
    nx.run(chain, tlb, h, bus, fa, fa)
}

/// Block terminator: the fall-through pc is `pc`.
unsafe fn step_end(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let n = retired(h, d);
    finish_direct_to(chain, tlb, h, bus, d, n, pc_of(chain, h, d), fa, fb)
}

/// A fault consumes execution budget, but does not retire architecturally.
#[cold]
#[inline(never)]
unsafe fn trap_out(chain: Chain, h: &mut Hart, d: *const DInsn, t: Trap) -> u64 {
    let n = retired(h, d);
    h.pc = pc_of(chain, h, d);
    h.take_trap(t.cause as u64, t.tval);
    h.instret_bias = h.instret_bias.wrapping_sub(1);
    chain.executed(h, n + 1)
}

/// Check after slow operations that can affect code, IRQs or the execution mode.
/// Synchronous changes return immediately. A remote kick is also checked here
/// and whenever the bounded SMP chain returns to the run loop.
#[inline(always)]
fn chain_interrupted(h: &Hart, bus: &Bus) -> bool {
    (h.chain_budget != 0 && h.chain_limit.load(Relaxed) == 0)
        || bus.code_epoch.load(Relaxed) != h.cache_stamp as u32
}

#[inline(always)]
unsafe fn stop_after(chain: Chain, h: &mut Hart, d: *const DInsn) -> u64 {
    h.pc = pc_of(chain, h, d).wrapping_add(u64::from((*d).len));
    chain.executed(h, retired(h, d) + 1)
}

/// Any instruction without a dedicated step, through the interpreter. It may
/// be the last one, and then it has set the pc itself.
#[inline(never)]
pub(crate) unsafe fn step_generic(chain: Chain, _tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, _fb: u64) -> u64 {
    crate::counters::inc_generic((*d).op as u8);
    let di = &*d;
    let pc = pc_of(chain, h, d);
    h.pc = pc.wrapping_add(di.len as u64);
    // Whether this instruction ends the block is read before running it, so
    // that both continuations below are plain tail calls. That matters: this
    // step is the one slow path every block can end on, and a non-tail call
    // here would nest a frame per block and grow the chain stack.
    let last = (*d.add(1)).op == Op::Empty;
    let start = h.chained;
    h.chained = chain.executed(h, retired(h, d));
    let result = h.exec(bus, di, pc);
    h.chained = start;
    if let Err(t) = result {
        return trap_out(chain, h, d, t);
    }
    if chain_interrupted(h, bus) {
        return chain.executed(h, retired(h, d) + 1);
    }
    if last {
        finish(chain, h.ltlb_cur, h, bus, d, retired(h, d) + 1)
    } else {
        advance(chain, h.ltlb_cur, h, bus, d, fa, fa)
    }
}

/// The value an AMO stores, from the old value and the register operand (`funct5` is the
/// operation, as in `Hart::exec`).
#[inline(always)]
pub(crate) fn amo_value(funct5: i32, old: u64, src: u64) -> u64 {
    match funct5 {
        0x00 => old.wrapping_add(src),
        0x01 => src,
        0x04 => old ^ src,
        0x0c => old & src,
        0x08 => old | src,
        0x10 => {
            if (old as i64) < (src as i64) {
                old
            } else {
                src
            }
        }
        0x14 => {
            if (old as i64) > (src as i64) {
                old
            } else {
                src
            }
        }
        0x18 => old.min(src),
        _ => old.max(src),
    }
}

/// The AMO `funct5` as one host atomic operation on the aligned word or doubleword at host
/// address `p`; returns the old value, sign-extended from 32 bits for a word.
///
/// # Safety
/// `p` must be aligned for the width and point at memory that stays allocated.
pub(crate) unsafe fn amo_host(p: usize, word: bool, funct5: i32, b: u64) -> u64 {
    use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering::SeqCst};
    if word {
        let (a, s, v) = (AtomicU32::from_ptr(p as *mut u32), AtomicI32::from_ptr(p as *mut i32), b as u32);
        let old = match funct5 {
            0x00 => a.fetch_add(v, SeqCst),
            0x01 => a.swap(v, SeqCst),
            0x04 => a.fetch_xor(v, SeqCst),
            0x0c => a.fetch_and(v, SeqCst),
            0x08 => a.fetch_or(v, SeqCst),
            0x10 => s.fetch_min(v as i32, SeqCst) as u32,
            0x14 => s.fetch_max(v as i32, SeqCst) as u32,
            0x18 => a.fetch_min(v, SeqCst),
            _ => a.fetch_max(v, SeqCst),
        };
        old as i32 as i64 as u64
    } else {
        let (a, s) = (AtomicU64::from_ptr(p as *mut u64), AtomicI64::from_ptr(p as *mut i64));
        match funct5 {
            0x00 => a.fetch_add(b, SeqCst),
            0x01 => a.swap(b, SeqCst),
            0x04 => a.fetch_xor(b, SeqCst),
            0x0c => a.fetch_and(b, SeqCst),
            0x08 => a.fetch_or(b, SeqCst),
            0x10 => s.fetch_min(b as i64, SeqCst) as u64,
            0x14 => s.fetch_max(b as i64, SeqCst) as u64,
            0x18 => a.fetch_min(b, SeqCst),
            _ => a.fetch_max(b, SeqCst),
        }
    }
}

/// Compare-and-swap of the aligned word or doubleword at host address `p`, for `sc`.
///
/// # Safety
/// As for `amo_host`.
pub(crate) unsafe fn cas_host(p: usize, word: bool, expect: u64, new: u64) -> bool {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    if word {
        AtomicU32::from_ptr(p as *mut u32).compare_exchange(expect as u32, new as u32, SeqCst, SeqCst).is_ok()
    } else {
        AtomicU64::from_ptr(p as *mut u64).compare_exchange(expect, new, SeqCst, SeqCst).is_ok()
    }
}

/// An atomic read-modify-write on a page the flat store table maps (plain RAM without
/// decoded code). With one host thread a plain read and write is atomic; with one thread
/// per hart it is a host atomic. Anything else, a misaligned address included, takes the
/// interpreter.
#[inline(never)]
unsafe fn step_amo(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let va = h.rx_u(di.rs1);
    let word = di.f == 2;
    let n: u64 = if word { 4 } else { 8 };
    let e = &*tlb.add(crate::hart::FTLB_SETS).add(ftlb_index(va));
    if e.tag == va & (!0xfffu64 | (n - 1)) {
        let p = va.wrapping_add(e.addend) as usize;
        let b = h.rx_u(di.rs2);
        let old = if bus.mt.is_some() {
            amo_host(p, word, di.imm, if word { b as u32 as i32 as i64 as u64 } else { b })
        } else if word {
            let old = host_read::<4>(p) as u32 as i32 as i64 as u64;
            host_write::<4>(p, amo_value(di.imm, old, b as u32 as i32 as i64 as u64));
            old
        } else {
            let old = host_read::<8>(p);
            host_write::<8>(p, amo_value(di.imm, old, b));
            old
        };
        h.reservation = None;
        h.wx_u(di.rd, old);
        return advance(chain, tlb, h, bus, d, fa, fa);
    }
    step_generic(chain, tlb, h, bus, d, fa, fb)
}

/// Load-reserved on a page the flat load table maps.
#[inline(never)]
unsafe fn step_lr(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let va = h.rx_u(di.rs1);
    let word = di.f == 2;
    let n: u64 = if word { 4 } else { 8 };
    let e = &*tlb.add(ftlb_index(va));
    if e.tag == va & (!0xfffu64 | (n - 1)) {
        let p = va.wrapping_add(e.addend) as usize;
        let raw = if word { host_read::<4>(p) } else { host_read::<8>(p) };
        let v = if word { raw as u32 as i32 as i64 as u64 } else { raw };
        h.reservation = Some((p - bus.ram.as_ptr() as usize) as u64 + RAM_BASE);
        h.reservation_val = raw;
        h.wx_u(di.rd, v);
        return advance(chain, tlb, h, bus, d, fa, fa);
    }
    step_generic(chain, tlb, h, bus, d, fa, fb)
}

/// Store-conditional on a page the flat store table maps.
#[inline(never)]
unsafe fn step_sc(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let va = h.rx_u(di.rs1);
    let word = di.f == 2;
    let n: u64 = if word { 4 } else { 8 };
    let e = &*tlb.add(crate::hart::FTLB_SETS).add(ftlb_index(va));
    if e.tag == va & (!0xfffu64 | (n - 1)) {
        let p = va.wrapping_add(e.addend) as usize;
        let pa = (p - bus.ram.as_ptr() as usize) as u64 + RAM_BASE;
        let mut ok = h.reservation == Some(pa);
        h.reservation = None;
        if ok {
            let b = h.rx_u(di.rs2);
            if bus.mt.is_some() {
                // Another hart may have written the word since `lr`: succeed only if it
                // still holds what `lr` read.
                ok = cas_host(p, word, h.reservation_val, b);
            } else if word {
                host_write::<4>(p, b);
            } else {
                host_write::<8>(p, b);
            }
        }
        h.wx_u(di.rd, if ok { 0 } else { 1 });
        return advance(chain, tlb, h, bus, d, fa, fa);
    }
    step_generic(chain, tlb, h, bus, d, fa, fb)
}

#[inline(always)]
fn sx32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

/// The value of operand `r` of a unit, from the source `S`.
#[inline(always)]
fn operand<const S: u8>(h: &Hart, r: u8, fa: u64, fb: u64) -> u64 {
    match S {
        FA => {
            crate::counters::inc(&crate::counters::FWD_FA);
            fa
        }
        FB => {
            crate::counters::inc(&crate::counters::FWD_FB);
            fb
        }
        ZERO => 0,
        _ => h.rx_u(r),
    }
}

/// The result of the integer ALU operation `OP` on `a` and `b`. For an immediate
/// form, `b` is the immediate; each such form computes what its register form does.
#[inline(always)]
pub(crate) fn alu<const OP: u8>(a: u64, b: u64) -> u64 {
    use Op::*;
    let is = |op: Op| OP == op as u8;
    if is(Add) || is(Addi) {
        a.wrapping_add(b)
    } else if is(Sub) {
        a.wrapping_sub(b)
    } else if is(And) || is(Andi) {
        a & b
    } else if is(Or) || is(Ori) {
        a | b
    } else if is(Xor) || is(Xori) {
        a ^ b
    } else if is(Sll) || is(Slli) {
        a << (b & 63)
    } else if is(Srl) || is(Srli) {
        a >> (b & 63)
    } else if is(Sra) || is(Srai) {
        ((a as i64) >> (b & 63)) as u64
    } else if is(Slt) || is(Slti) {
        ((a as i64) < (b as i64)) as u64
    } else if is(Sltu) || is(Sltiu) {
        (a < b) as u64
    } else if is(Mul) {
        a.wrapping_mul(b)
    } else if is(Addw) || is(Addiw) {
        sx32((a as u32).wrapping_add(b as u32))
    } else if is(Subw) {
        sx32((a as u32).wrapping_sub(b as u32))
    } else if is(Sllw) || is(Slliw) {
        sx32((a as u32) << (b & 31))
    } else if is(Srlw) || is(Srliw) {
        sx32((a as u32) >> (b & 31))
    } else if is(Sraw) || is(Sraiw) {
        ((a as u32 as i32) >> (b & 31)) as i64 as u64
    } else {
        // Mulw, the only remaining ALU operation.
        sx32((a as u32).wrapping_mul(b as u32))
    }
}

/// The M extension's high multiplies, divisions and remainders (the low multiplies are
/// ALU operations), with the architecture's results for a zero divisor and for overflow.
#[inline(always)]
fn muldiv<const OP: u8>(a: u64, b: u64) -> u64 {
    use Op::*;
    let is = |op: Op| OP == op as u8;
    if is(Mulh) {
        (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64
    } else if is(Mulhsu) {
        (((a as i64 as i128) * (b as i128)) >> 64) as u64
    } else if is(Mulhu) {
        (((a as u128) * (b as u128)) >> 64) as u64
    } else {
        div_rem::<OP>(a, b)
    }
}

/// ARM64 produces the RISC-V quotient on signed overflow and zero on a
/// zero divisor. The latter already gives the correct remainder via a - q*b.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn div_rem<const OP: u8>(a: u64, b: u64) -> u64 {
    use Op::*;
    let is = |op: Op| OP == op as u8;
    let word = is(Divw) || is(Divuw) || is(Remw) || is(Remuw);
    let signed = is(Div) || is(Rem) || is(Divw) || is(Remw);
    let quotient: u64;
    macro_rules! divide {
        ($instruction:literal, $lane:literal) => {
            core::arch::asm!(concat!($instruction, " {q", $lane, "}, {a", $lane, "}, {b", $lane, "}"),
                q = lateout(reg) quotient, a = in(reg) a, b = in(reg) b,
                options(pure, nomem, nostack, preserves_flags),
            )
        };
    }
    // SAFETY: neither divide instruction traps, including on a zero divisor.
    unsafe {
        match (signed, word) {
            (true, false) => divide!("sdiv", ""),
            (false, false) => divide!("udiv", ""),
            (true, true) => divide!("sdiv", ":w"),
            (false, true) => divide!("udiv", ":w"),
        }
    }
    let zero = if word { b as u32 == 0 } else { b == 0 };
    let value = if is(Rem) || is(Remu) || is(Remw) || is(Remuw) {
        a.wrapping_sub(quotient.wrapping_mul(b))
    } else if zero {
        u64::MAX
    } else {
        quotient
    };
    if word { sx32(value as u32) } else { value }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn div_rem<const OP: u8>(a: u64, b: u64) -> u64 {
    use Op::*;
    let is = |op: Op| OP == op as u8;
    if is(Div) {
        let (x, y) = (a as i64, b as i64);
        if y == 0 {
            u64::MAX
        } else if x == i64::MIN && y == -1 {
            x as u64
        } else {
            (x / y) as u64
        }
    } else if is(Divu) {
        a.checked_div(b).unwrap_or(u64::MAX)
    } else if is(Rem) {
        let (x, y) = (a as i64, b as i64);
        if y == 0 {
            a
        } else if x == i64::MIN && y == -1 {
            0
        } else {
            (x % y) as u64
        }
    } else if is(Remu) {
        if b == 0 {
            a
        } else {
            a % b
        }
    } else if is(Divw) {
        let (x, y) = (a as i32, b as i32);
        (if y == 0 {
            -1i32
        } else if x == i32::MIN && y == -1 {
            x
        } else {
            x / y
        }) as i64 as u64
    } else if is(Divuw) {
        sx32((a as u32).checked_div(b as u32).unwrap_or(u32::MAX))
    } else if is(Remw) {
        let (x, y) = (a as i32, b as i32);
        (if y == 0 {
            x
        } else if x == i32::MIN && y == -1 {
            0
        } else {
            x % y
        }) as i64 as u64
    } else {
        // Remuw.
        let (x, y) = (a as u32, b as u32);
        sx32(if y == 0 { x } else { x % y })
    }
}

/// `mulh`, `div`, `rem` and the like: a leaf step, so they no longer pay for the
/// interpreter's call and its frame.
#[inline(never)]
unsafe fn step_muldiv<const OP: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let nx = pre1(d);
    let (rd, rs1, rs2, _) = operands(d, fb);
    let r = muldiv::<OP>(h.rx_u(rs1), h.rx_u(rs2));
    h.wx_u(rd, r);
    nx.run(chain, tlb, h, bus, fa, fa)
}

/// Whether the branch `OP` is taken on `a` and `b`.
#[inline(always)]
fn taken<const OP: u8>(a: u64, b: u64) -> bool {
    use Op::*;
    let is = |op: Op| OP == op as u8;
    if is(Beq) {
        a == b
    } else if is(Bne) {
        a != b
    } else if is(Blt) {
        (a as i64) < (b as i64)
    } else if is(Bge) {
        (a as i64) >= (b as i64)
    } else if is(Bltu) {
        a < b
    } else {
        a >= b
    }
}

/// Read `N` bytes at a host address. The caller has checked that the access is
/// inside a page of RAM, so this cannot go out of bounds.
#[inline(always)]
pub(crate) unsafe fn host_read<const N: usize>(p: usize) -> u64 {
    let p = p as *const u8;
    match N {
        1 => *p as u64,
        2 => (p as *const u16).read_unaligned() as u64,
        4 => (p as *const u32).read_unaligned() as u64,
        _ => (p as *const u64).read_unaligned(),
    }
}

/// Write the low `N` bytes of `v` at a host address (see `host_read`).
#[inline(always)]
pub(crate) unsafe fn host_write<const N: usize>(p: usize, v: u64) {
    let p = p as *mut u8;
    match N {
        1 => *p = v as u8,
        2 => (p as *mut u16).write_unaligned(v as u16),
        4 => (p as *mut u32).write_unaligned(v as u32),
        _ => (p as *mut u64).write_unaligned(v),
    }
}

/// Sign- or zero-extend a loaded value of `N` bytes.
#[inline(always)]
fn extend<const N: usize, const SIGNED: bool>(v: u64) -> u64 {
    if !SIGNED {
        return v;
    }
    match N {
        1 => v as i8 as i64 as u64,
        2 => v as i16 as i64 as u64,
        4 => v as i32 as i64 as u64,
        _ => v,
    }
}

/// A load that the fast path could not complete. It is a tail call, so the
/// fast step itself needs no stack frame.
#[cold]
#[inline(never)]
unsafe fn load_slow<const N: usize, const SIGNED: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    va: u64,
    fa: u64,
) -> u64 {
    crate::counters::inc(&crate::counters::SLOW_LOAD);
    // A misaligned access that stays inside a page the flat table maps (the table only
    // hits aligned ones): read it in place instead of translating again.
    let e = &*tlb.add(ftlb_index(va));
    if e.tag == va & !0xfffu64 && (va & 0xfff) + N as u64 <= 4096 {
        let r = extend::<N, SIGNED>(host_read::<N>(va.wrapping_add(e.addend) as usize));
        h.wx_u((*d).rd, r);
        return advance(chain, tlb, h, bus, d, r, fa);
    }
    match h.load_general::<N>(bus, va) {
        Ok(v) => {
            let r = extend::<N, SIGNED>(v);
            h.wx_u((*d).rd, r);
            advance(chain, tlb, h, bus, d, r, fa)
        }
        Err(t) => trap_out(chain, h, d, t),
    }
}

/// A store that the fast path could not complete.
#[cold]
#[inline(never)]
unsafe fn store_slow<const N: usize>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    va: u64,
    fa: u64,
) -> u64 {
    crate::counters::inc(&crate::counters::SLOW_STORE);
    let v = h.rx_u((*d).rs2);
    // As for loads; the store table only maps pages without decoded code, so nothing
    // needs invalidating.
    let e = &*tlb.add(crate::hart::FTLB_SETS).add(ftlb_index(va));
    if e.tag == va & !0xfffu64 && (va & 0xfff) + N as u64 <= 4096 {
        host_write::<N>(va.wrapping_add(e.addend) as usize, v);
        return advance(chain, tlb, h, bus, d, fa, fa);
    }
    match h.store_general::<N>(bus, va, v) {
        Ok(()) => {
            if chain_interrupted(h, bus) {
                return stop_after(chain, h, d);
            }
            advance(chain, tlb, h, bus, d, fa, fa)
        }
        Err(t) => trap_out(chain, h, d, t),
    }
}

/// An integer ALU operation on two registers. `OP` is its opcode. The destination
/// is never `x0` here, since `build_block` makes those units no-ops.
#[inline(never)]
unsafe fn step_alu<const OP: u8, const SA: u8, const SB: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre1(d);
    let (rd, rs1, rs2, _) = operands(d, fb);
    let x = operand::<SA>(h, rs1, fa, fb);
    let y = operand::<SB>(h, rs2, fa, fb);
    let r = alu::<OP>(x, y);
    h.set_x(rd, r);
    nx.run(chain, tlb, h, bus, r, fa)
}

/// An integer ALU operation on a register and the immediate. Its destination is
/// never `x0`, as for `step_alu`.
#[inline(never)]
unsafe fn step_alui<const OP: u8, const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre1(d);
    let (rd, rs1, _, imm) = operands(d, fb);
    let x = operand::<SA>(h, rs1, fa, fb);
    let r = alu::<OP>(x, imm as u64);
    h.set_x(rd, r);
    nx.run(chain, tlb, h, bus, r, fa)
}

#[inline(never)]
unsafe fn step_move<const WORD: bool, const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let nx = pre1(d);
    let (rd, rs1, _, _) = operands(d, fb);
    let value = operand::<SA>(h, rs1, fa, fb);
    let value = if WORD { sx32(value as u32) } else { value };
    h.set_x(rd, value);
    nx.run(chain, tlb, h, bus, value, fa)
}

#[inline(never)]
unsafe fn step_lui(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let nx = pre1(d);
    let (rd, _, _, imm) = operands(d, fb);
    let v = imm as u64;
    h.set_x(rd, v);
    nx.run(chain, tlb, h, bus, v, fa)
}

#[inline(never)]
unsafe fn step_auipc(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, _fb: u64) -> u64 {
    let nx = pre1(d);
    let di = &*d;
    let v = pc_of(chain, h, d).wrapping_add(di.imm as u64);
    h.set_x(di.rd, v);
    nx.run(chain, tlb, h, bus, v, fa)
}

/// A load of `N` bytes from `rs1 + imm`. Its destination is never `x0`: `step_of` gives
/// such a load to the interpreter.
#[inline(never)]
unsafe fn step_load<const N: usize, const SIGNED: bool, const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre1(d);
    let (rd, rs1, _, imm) = operands(d, fb);
    let va = operand::<SA>(h, rs1, fa, fb).wrapping_add(imm as u64);
    if let Some(v) = tlb_load::<N>(tlb, h, va) {
        crate::counters::inc(&crate::counters::FAST_LOAD_HIT);
        let r = extend::<N, SIGNED>(v);
        h.set_x(rd, r);
        return nx.run(chain, tlb, h, bus, r, fa);
    }
    load_slow::<N, SIGNED>(chain, tlb, h, bus, d, va, fa)
}

/// A store of `N` bytes of `rs2` to `rs1 + imm`.
#[inline(never)]
unsafe fn step_store<const N: usize, const SA: u8, const SB: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre1(d);
    let (_, rs1, rs2, imm) = operands(d, fb);
    let va = operand::<SA>(h, rs1, fa, fb).wrapping_add(imm as u64);
    let v = operand::<SB>(h, rs2, fa, fb);
    if tlb_store::<N>(tlb, h, va, v) {
        crate::counters::inc(&crate::counters::FAST_STORE_HIT);
        return nx.run(chain, tlb, h, bus, fa, fa);
    }
    store_slow::<N>(chain, tlb, h, bus, d, va, fa)
}

/// A conditional branch that ends its block. Like `step_br_flow`, only the taken edge
/// leaves here; the fall-through continues into the block's terminator, which
/// exits with a link of its own (an exit instruction carries one target).
#[inline(never)]
unsafe fn step_br<const OP: u8, const SA: u8, const SB: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let di = &*d;
    let a = operand::<SA>(h, di.rs1, fa, fb);
    let b = operand::<SB>(h, di.rs2, fa, fb);
    if taken::<OP>(a, b) {
        finish_direct_to(chain, tlb, h, bus, d, retired(h, d) + 1, chain.pc(h).wrapping_add(di.imm as i64 as u64), fa, fb)
    } else {
        // The branch ends the block, so the terminator follows it: leave through the
        // terminator's exit here instead of dispatching to it.
        let t = d.add(1);
        finish_direct_to(chain, tlb, h, bus, t, retired(h, t), pc_of(chain, h, t), fa, fa)
    }
}

/// A conditional branch whose fall-through continues inside the block. Only the
/// taken edge leaves it.
#[inline(never)]
unsafe fn step_br_flow<const OP: u8, const SA: u8, const SB: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre1(d);
    let di = &*d;
    let a = operand::<SA>(h, di.rs1, fa, fb);
    let b = operand::<SB>(h, di.rs2, fa, fb);
    if taken::<OP>(a, b) {
        finish_direct_to(chain, tlb, h, bus, d, retired(h, d) + 1, chain.pc(h).wrapping_add(di.imm as i64 as u64), fa, fb)
    } else {
        nx.run(chain, tlb, h, bus, fa, fa)
    }
}

#[inline(never)]
unsafe fn step_jal(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let pc = pc_of(chain, h, d);
    h.wx_u(di.rd, pc.wrapping_add(di.len as u64));
    if di.rd == 1 || di.rd == 5 {
        ras_push_at(chain, h, d);
    }
    finish_direct_to(chain, tlb, h, bus, d, retired(h, d) + 1, chain.pc(h).wrapping_add(di.imm as i64 as u64), fa, fb)
}

/// `jal` with a known non-zero destination: writes the return address without
/// testing the register number.
#[inline(never)]
unsafe fn step_jal_set(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let pc = pc_of(chain, h, d);
    h.set_x(di.rd, pc.wrapping_add(di.len as u64));
    if di.rd == 1 || di.rd == 5 {
        ras_push_at(chain, h, d);
    }
    finish_direct_to(chain, tlb, h, bus, d, retired(h, d) + 1, chain.pc(h).wrapping_add(di.imm as i64 as u64), fa, fb)
}

/// `jal x0, target`: a plain jump, no return address.
#[inline(never)]
unsafe fn step_jal_bare(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    finish_direct_to(chain, tlb, h, bus, d, retired(h, d) + 1, chain.pc(h).wrapping_add(di.imm as i64 as u64), fa, fb)
}

/// `jalr` with a known non-zero destination.
#[inline(never)]
unsafe fn step_jalr_set<const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let target = operand::<SA>(h, di.rs1, fa, fb).wrapping_add(di.imm as u64) & !1;
    h.set_x(di.rd, pc_of(chain, h, d).wrapping_add(di.len as u64));
    finish_jalr_to(chain, tlb, h, bus, d, target)
}

/// A return through one of the ISA's two link registers.
#[inline(never)]
unsafe fn step_ret<const R: u8, const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let target = operand::<SA>(h, R, fa, fb) & !1;
    finish_ret(chain, tlb, h, bus, d, target)
}

/// `jalr x0, rs, off` without a return hint.
#[inline(never)]
unsafe fn step_jalr_bare<const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let target = operand::<SA>(h, di.rs1, fa, fb).wrapping_add(di.imm as u64) & !1;
    finish_indirect_to(chain, tlb, h, bus, d, target)
}

#[inline(never)]
unsafe fn step_jalr<const SA: u8>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
    let di = &*d;
    let target = operand::<SA>(h, di.rs1, fa, fb).wrapping_add(di.imm as u64) & !1;
    h.wx_u(di.rd, pc_of(chain, h, d).wrapping_add(di.len as u64));
    finish_jalr_to(chain, tlb, h, bus, d, target)
}

/// The variant of a step for the sources of its operands, if there is one.
macro_rules! pick1 {
    ($f:ident, [$($pre:expr),*], $sa:expr) => {
        match $sa {
            MEM => Some($f::<$({ $pre },)* MEM> as Step),
            FA => Some($f::<$({ $pre },)* FA> as Step),
            FB => Some($f::<$({ $pre },)* FB> as Step),
            ZERO => Some($f::<$({ $pre },)* ZERO> as Step),
            _ => None,
        }
    };
}

/// As `pick1`, for a step with two register operands.
macro_rules! pick2 {
    ($f:ident, [$($pre:expr),*], $sa:expr, $sb:expr) => {
        match ($sa, $sb) {
            (MEM, MEM) => Some($f::<$({ $pre },)* MEM, MEM> as Step),
            (MEM, FA) => Some($f::<$({ $pre },)* MEM, FA> as Step),
            (MEM, FB) => Some($f::<$({ $pre },)* MEM, FB> as Step),
            (FA, MEM) => Some($f::<$({ $pre },)* FA, MEM> as Step),
            (FA, FA) => Some($f::<$({ $pre },)* FA, FA> as Step),
            (FA, FB) => Some($f::<$({ $pre },)* FA, FB> as Step),
            (FB, MEM) => Some($f::<$({ $pre },)* FB, MEM> as Step),
            (FB, FA) => Some($f::<$({ $pre },)* FB, FA> as Step),
            (FB, FB) => Some($f::<$({ $pre },)* FB, FB> as Step),
            (MEM, ZERO) => Some($f::<$({ $pre },)* MEM, ZERO> as Step),
            (FA, ZERO) => Some($f::<$({ $pre },)* FA, ZERO> as Step),
            (FB, ZERO) => Some($f::<$({ $pre },)* FB, ZERO> as Step),
            (ZERO, MEM) => Some($f::<$({ $pre },)* ZERO, MEM> as Step),
            (ZERO, FA) => Some($f::<$({ $pre },)* ZERO, FA> as Step),
            (ZERO, FB) => Some($f::<$({ $pre },)* ZERO, FB> as Step),
            (ZERO, ZERO) => Some($f::<$({ $pre },)* ZERO, ZERO> as Step),
            _ => None,
        }
    };
}

/// The step for `op` when its register operands come from the sources `sa` and
/// `sb`. `None` means the `STEPS` entry applies (no operand to forward, or no
/// variant for this combination).
fn variant(op: Op, sa: u8, sb: u8) -> Option<Step> {
    match op {
        Op::Add => pick2!(step_alu, [Op::Add as u8], sa, sb),
        Op::Sub => pick2!(step_alu, [Op::Sub as u8], sa, sb),
        Op::And => pick2!(step_alu, [Op::And as u8], sa, sb),
        Op::Or => pick2!(step_alu, [Op::Or as u8], sa, sb),
        Op::Xor => pick2!(step_alu, [Op::Xor as u8], sa, sb),
        Op::Sll => pick2!(step_alu, [Op::Sll as u8], sa, sb),
        Op::Srl => pick2!(step_alu, [Op::Srl as u8], sa, sb),
        Op::Sra => pick2!(step_alu, [Op::Sra as u8], sa, sb),
        Op::Slt => pick2!(step_alu, [Op::Slt as u8], sa, sb),
        Op::Sltu => pick2!(step_alu, [Op::Sltu as u8], sa, sb),
        Op::Mul => pick2!(step_alu, [Op::Mul as u8], sa, sb),
        Op::Addw => pick2!(step_alu, [Op::Addw as u8], sa, sb),
        Op::Subw => pick2!(step_alu, [Op::Subw as u8], sa, sb),
        Op::Sllw => pick2!(step_alu, [Op::Sllw as u8], sa, sb),
        Op::Srlw => pick2!(step_alu, [Op::Srlw as u8], sa, sb),
        Op::Sraw => pick2!(step_alu, [Op::Sraw as u8], sa, sb),
        Op::Mulw => pick2!(step_alu, [Op::Mulw as u8], sa, sb),
        Op::Addi => pick1!(step_alui, [Op::Addi as u8], sa),
        Op::Andi => pick1!(step_alui, [Op::Andi as u8], sa),
        Op::Ori => pick1!(step_alui, [Op::Ori as u8], sa),
        Op::Xori => pick1!(step_alui, [Op::Xori as u8], sa),
        Op::Slli => pick1!(step_alui, [Op::Slli as u8], sa),
        Op::Srli => pick1!(step_alui, [Op::Srli as u8], sa),
        Op::Srai => pick1!(step_alui, [Op::Srai as u8], sa),
        Op::Addiw => pick1!(step_alui, [Op::Addiw as u8], sa),
        Op::Slti => pick1!(step_alui, [Op::Slti as u8], sa),
        Op::Sltiu => pick1!(step_alui, [Op::Sltiu as u8], sa),
        Op::Slliw => pick1!(step_alui, [Op::Slliw as u8], sa),
        Op::Srliw => pick1!(step_alui, [Op::Srliw as u8], sa),
        Op::Sraiw => pick1!(step_alui, [Op::Sraiw as u8], sa),
        Op::Ld => pick1!(step_load, [8, false], sa),
        Op::Lw => pick1!(step_load, [4, true], sa),
        Op::Lwu => pick1!(step_load, [4, false], sa),
        Op::Lh => pick1!(step_load, [2, true], sa),
        Op::Lhu => pick1!(step_load, [2, false], sa),
        Op::Lb => pick1!(step_load, [1, true], sa),
        Op::Lbu => pick1!(step_load, [1, false], sa),
        Op::Sd => pick2!(step_store, [8], sa, sb),
        Op::Sw => pick2!(step_store, [4], sa, sb),
        Op::Sh => pick2!(step_store, [2], sa, sb),
        Op::Sb => pick2!(step_store, [1], sa, sb),
        Op::Beq => pick2!(step_br, [Op::Beq as u8], sa, sb),
        Op::Bne => pick2!(step_br, [Op::Bne as u8], sa, sb),
        Op::Blt => pick2!(step_br, [Op::Blt as u8], sa, sb),
        Op::Bge => pick2!(step_br, [Op::Bge as u8], sa, sb),
        Op::Bltu => pick2!(step_br, [Op::Bltu as u8], sa, sb),
        Op::Bgeu => pick2!(step_br, [Op::Bgeu as u8], sa, sb),
        Op::Jalr => pick1!(step_jalr, [], sa),
        _ => None,
    }
}

/// The step for an in-block conditional branch, mirroring `variant`.
fn variant_flow(op: Op, sa: u8, sb: u8) -> Option<Step> {
    match op {
        Op::Beq => pick2!(step_br_flow, [Op::Beq as u8], sa, sb),
        Op::Bne => pick2!(step_br_flow, [Op::Bne as u8], sa, sb),
        Op::Blt => pick2!(step_br_flow, [Op::Blt as u8], sa, sb),
        Op::Bge => pick2!(step_br_flow, [Op::Bge as u8], sa, sb),
        Op::Bltu => pick2!(step_br_flow, [Op::Bltu as u8], sa, sb),
        Op::Bgeu => pick2!(step_br_flow, [Op::Bgeu as u8], sa, sb),
        _ => None,
    }
}

/// Where operand `r` of the unit at `i` comes from. A source is forwarded only when
/// the unit just before (or two before) is the last to write `r`, and that instruction passes its result on (see `produces`). Generated runs
/// have an opaque output contract; their operands always use the register file.
fn source(ops: &[DInsn], opaque: &[bool], i: usize, r: u8) -> u8 {
    if r == 0 {
        return ZERO;
    }
    let depth = if PRELOAD_OPERANDS { 1 } else { 2 };
    for (back, src) in [(1, FA), (2, FB)].into_iter().take(depth) {
        if i >= back && ops[i - back].rd == r {
            let j = i - back;
            return if !opaque[j] && produces(ops[j].op) { src } else { MEM };
        }
    }
    MEM
}

/// The step for the unit at `i` of a block. `opaque[j]` marks instructions of
/// generated runs, whose outgoing forwarding registers are unspecified.
fn step_of(ops: &[DInsn], opaque: &[bool], flow: &[bool], i: usize) -> Step {
    let d = &ops[i];
    if d.rd == 0 && pure_register_op(d.op as u8) {
        return step_nop as Step;
    }
    if d.rd == 0 && is_load(d.op) {
        return step_generic as Step;
    }
    // Floating point runs on the host's FPU when the operands allow it (see `fastfp.rs`).
    if matches!(
        d.op,
        Op::Flw | Op::Fld | Op::Fsw | Op::Fsd | Op::Fmadd | Op::Fmsub | Op::Fnmsub | Op::Fnmadd | Op::Fp
    ) {
        return crate::fastfp::fast_step(d).unwrap_or(step_generic as Step);
    }
    let sa = source(ops, opaque, i, d.rs1);
    let sb = source(ops, opaque, i, d.rs2);
    if flow[i] {
        if let Some(step) = variant_flow(d.op, sa, sb) {
            return step;
        }
    }
    // Jumps: pick the step for a zero or non-zero destination at build time.
    match d.op {
        Op::Addi if d.imm == 0 => return pick1!(step_move, [false], sa).unwrap(),
        Op::Addiw if d.imm == 0 => return pick1!(step_move, [true], sa).unwrap(),
        Op::Jal if d.rd == 0 => return step_jal_bare as Step,
        Op::Jal if d.rd != 0 => return step_jal_set as Step,
        Op::Jalr if d.rd == 0 => {
            let step = match (d.rs1, d.imm) {
                (1, 0) => pick1!(step_ret, [1], sa),
                (5, 0) => pick1!(step_ret, [5], sa),
                _ => pick1!(step_jalr_bare, [], sa),
            };
            if let Some(s) = step {
                return s;
            }
        }
        Op::Jalr if d.rd != 0 => {
            if let Some(s) = pick1!(step_jalr_set, [], sa) {
                return s;
            }
        }
        _ => {}
    }
    variant(d.op, sa, sb).unwrap_or(STEPS[d.op as usize])
}

/// What a fused idiom pair computes (see `special_pair`).
#[derive(Clone, Copy)]
enum Idiom {
    /// `lui`+`addi`/`addiw`: a constant that fits a signed 32-bit immediate.
    Const(i32),
    /// `auipc`+`addi`: a pc-relative constant (the `auipc`'s pc plus this).
    PcConst(i32),
    /// `slli rd, rs, 32` + `srli rd, rd, 32`: `rd = rs` zero-extended from 32 bits.
    Zext32,
    /// AUIPC supplies the target base of the following JALR.
    PcRelJump,
}

/// Two adjacent instructions that together are a well-known idiom and can be done in
/// one unit that computes the final value directly instead of going through the
/// intermediate. The pair must write the same register, so nothing is lost.
fn special_pair(a: &DInsn, b: &DInsn) -> Option<Idiom> {
    if a.op == Op::Auipc && b.op == Op::Jalr && a.rd != 0 && b.rs1 == a.rd
        && !(b.rd == 0 && b.imm == 0 && matches!(b.rs1, 1 | 5))
    {
        return Some(Idiom::PcRelJump);
    }
    if a.rd == 0 || b.rd != a.rd || b.rs1 != a.rd {
        return None;
    }
    let (hi, lo) = (a.imm as i64, b.imm as i64);
    match (a.op, b.op) {
        (Op::Lui, Op::Addi) => i32::try_from(hi + lo).ok().map(Idiom::Const),
        (Op::Lui, Op::Addiw) => Some(Idiom::Const((hi + lo) as i32)),
        (Op::Auipc, Op::Addi) => i32::try_from(hi + lo).ok().map(Idiom::PcConst),
        (Op::Slli, Op::Srli) if hi == 32 && lo == 32 => Some(Idiom::Zext32),
        _ => None,
    }
}

const RETURN_OFFSET_BITS: u32 = 9;
const RETURN_OFFSET_MASK: u64 = (1 << RETURN_OFFSET_BITS) - 1;
const _: () = assert!(MAX_BLOCK * 4 <= RETURN_OFFSET_MASK as usize);

/// Fold both addresses relative to the start of the block. The block PC is
/// even, so clearing the target's low bit can also happen before execution.
fn pcrel_jump(a: &DInsn, b: &DInsn) -> Step {
    let delta = (i64::from(a.off) + i64::from(a.imm) + i64::from(b.imm)) & !1;
    let return_offset = (b.off + i32::from(b.len)) as usize;
    a.link.set((delta as usize) << RETURN_OFFSET_BITS | return_offset);
    match (b.rd, a.rd == b.rd) {
        (0, _) => step_pcrel_jump::<true, false, false>,
        (1 | 5, true) => step_pcrel_jump::<false, true, true>,
        (1 | 5, false) => step_pcrel_jump::<true, true, true>,
        (_, true) => step_pcrel_jump::<false, true, false>,
        _ => step_pcrel_jump::<true, true, false>,
    }
}

#[inline(never)]
unsafe fn step_pcrel_jump<const SAVE_BASE: bool, const LINK: bool, const CALL: bool>(
    chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus,
    d: *const DInsn, fa: u64, fb: u64,
) -> u64 {
    let a = &*d;
    let jump = d.add(1);
    let b = &*jump;
    let pc = chain.pc(h);
    let metadata = a.link.get() as u64;
    if SAVE_BASE {
        h.set_x(a.rd, pc_of(chain, h, d).wrapping_add(a.imm as i64 as u64));
    }
    if LINK {
        let ret = pc.wrapping_add(metadata & RETURN_OFFSET_MASK);
        h.set_x(b.rd, ret);
        if CALL {
            ras_push_return(h, jump.add(1), ret);
        }
    }
    let target = pc.wrapping_add(((metadata as i64) >> RETURN_OFFSET_BITS) as u64);
    finish_direct_to(chain, tlb, h, bus, jump, u64::from(b.index) + 1, target, fa, fb)
}

/// A fused `lui`+`addi` (see `special_pair`): `rd` gets the constant in `imm`.
#[inline(never)]
unsafe fn step_const2(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, _fa: u64, _fb: u64) -> u64 {
    let nx = pre2(d);
    let di = &*d;
    h.set_x(di.rd, di.imm as u64);
    nx.run(chain, tlb, h, bus, di.imm as u64, 0)
}

/// A fused `auipc`+`addi`: `rd` gets the pc of the `auipc` plus the constant in `imm`.
#[inline(never)]
unsafe fn step_pcconst2(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, _fa: u64, _fb: u64) -> u64 {
    let nx = pre2(d);
    let di = &*d;
    let v = pc_of(chain, h, d).wrapping_add(di.imm as u64);
    h.set_x(di.rd, v);
    nx.run(chain, tlb, h, bus, v, 0)
}

/// A fused `slli rd, rs, 32` + `srli rd, rd, 32`.
#[inline(never)]
unsafe fn step_zext2(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, _fa: u64, _fb: u64) -> u64 {
    let nx = pre2(d);
    let di = &*d;
    let v = h.rx_u(di.rs1) & 0xffff_ffff;
    h.set_x(di.rd, v);
    nx.run(chain, tlb, h, bus, v, 0)
}

/// Whether `op` is a load.
const fn is_load(op: Op) -> bool {
    matches!(op, Op::Ld | Op::Lw | Op::Lwu | Op::Lh | Op::Lhu | Op::Lb | Op::Lbu)
}

/// Whether the instruction may be a member of a fused unit: one that writes `x0` (a no-op,
/// or a load the interpreter runs) is not, so `try_fast` can write its register untested.
fn fuses(d: &DInsn) -> bool {
    d.rd != 0 || !(is_alu(d.op as u8) || is_load(d.op) || matches!(d.op, Op::Lui | Op::Auipc))
}

/// Adjacent loads or stores with the same base and width. Loads must not
/// overwrite their address base before the run has finished. x0 loads retain
/// their ordinary precise-fault path.
fn memory_run(ops: &[DInsn], cooperative: bool) -> Option<(usize, Step)> {
    let a = ops.first()?;
    let (width, signed, store) = match a.op {
        Op::Ld => (8, false, false), Op::Lw => (4, true, false),
        Op::Lwu => (4, false, false), Op::Lh => (2, true, false),
        Op::Lhu => (2, false, false), Op::Lb => (1, true, false),
        Op::Lbu => (1, false, false), Op::Sd => (8, false, true),
        Op::Sw => (4, false, true), Op::Sh => (2, false, true),
        Op::Sb => (1, false, true), _ => return None,
    };
    let (mut lo, mut hi) = (a.imm, a.imm);
    let mut n = 0;
    for d in ops.iter().take(8) {
        if d.op != a.op || d.rs1 != a.rs1
            || (!store && d.rd == 0)
            || d.imm.wrapping_sub(a.imm) & (width as i32 - 1) != 0
        { break; }
        let (next_lo, next_hi) = (lo.min(d.imm), hi.max(d.imm));
        // Limit the window so accesses that usually span different pages do not
        // acquire an extra failed fast-path probe.
        if next_hi as i64 - next_lo as i64 + width as i64 > 256 { break; }
        lo = next_lo; hi = next_hi; n += 1;
        if !store && d.rd == a.rs1 {
            break;
        }
    }
    if n < 2 { return None; }
    macro_rules! pick_n {
        ($w:expr, $s:expr, $st:expr) => {
            match n {
                2 => step_memory_run::<2, $w, $s, $st> as Step,
                3 => step_memory_run::<3, $w, $s, $st> as Step,
                4 => step_memory_run::<4, $w, $s, $st> as Step,
                5 => step_memory_run::<5, $w, $s, $st> as Step,
                6 => step_memory_run::<6, $w, $s, $st> as Step,
                7 => step_memory_run::<7, $w, $s, $st> as Step,
                _ => step_memory_run::<8, $w, $s, $st> as Step,
            }
        };
    }
    let linear = if cooperative && width >= 4 {
        linear_memory::plan(&ops[..n], width, signed, store)
    } else {
        None
    };
    let step = linear.unwrap_or_else(|| match (width, signed, store) {
        (8, _, false) => pick_n!(8, false, false),
        (4, true, false) => pick_n!(4, true, false),
        (4, false, false) => pick_n!(4, false, false),
        (2, true, false) => pick_n!(2, true, false),
        (2, false, false) => pick_n!(2, false, false),
        (1, true, false) => pick_n!(1, true, false),
        (1, false, false) => pick_n!(1, false, false),
        (8, _, true) => pick_n!(8, false, true),
        (4, _, true) => pick_n!(4, false, true),
        (2, _, true) => pick_n!(2, false, true),
        _ => pick_n!(1, false, true),
    });
    // Memory instructions never use the direct-exit link. Keep the lowest
    // displacement and the precomputed page-offset bound there instead.
    let limit = (4096 - width - (hi as i64 - lo as i64) as usize) / width;
    a.link.set((limit << 32) | lo as u32 as usize);
    Some((n, step))
}

#[inline(never)]
unsafe fn step_memory_run<const N: usize, const W: usize, const SIGNED: bool, const STORE: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
    h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64,
) -> u64 {
    let nx = match N {
        2 => pre2(d), 3 => pre3(d), 4 => pre4(d), 5 => pre5(d),
        6 => pre6(d), 7 => pre7(d), _ => pre8(d),
    };
    let metadata = (*d).link.get();
    let base = h.rx_u((*d).rs1);
    let va = base.wrapping_add(metadata as u32 as i32 as i64 as u64);
    let table = if STORE { tlb.add(crate::hart::FTLB_SETS) } else { tlb };
    let e = tlb_entry(table, va);
    // Rotate the alignment bits to the top: one unsigned comparison proves
    // alignment, a matching page, and room for the entire run in that page.
    let delta = va.wrapping_sub(e.tag).rotate_right(W.trailing_zeros());
    if e.tag != u64::MAX && delta <= (metadata >> 32) as u64 {
        let host_base = base.wrapping_add(e.addend);
        let mut last = 0;
        let mut previous = 0;
        for k in 0..N {
            let di = &*d.add(k);
            let host = host_base.wrapping_add(di.imm as i64 as u64) as usize;
            if STORE {
                host_write::<W>(host, h.rx_u(di.rs2));
            } else {
                let value = extend::<W, SIGNED>(host_read::<W>(host));
                h.set_x(di.rd, value);
                previous = last;
                last = value;
            }
        }
        return nx.run(chain, tlb, h, bus, last, previous);
    }
    // Nothing has executed: hand the first access to the normal step so faults,
    // MMIO, page boundaries and decoded-code writes keep their precise order.
    invoke(STEPS[(*d).op as usize], chain, tlb, h, bus, d, fa, fb)
}

/// Two accesses of differing kinds can share one translation when they have
/// the same base. Guard the complete span around the wider access, which also
/// proves alignment of the narrower one.
fn mixed_memory_pair(a: &DInsn, b: &DInsn) -> Option<Step> {
    if a.op == b.op || a.rs1 != b.rs1 {
        return None;
    }
    let kind = |op: Op, first: Op| (op as usize).wrapping_sub(first as usize);
    let load_a = is_load(a.op);
    let load_b = is_load(b.op);
    if (load_a && (a.rd == 0 || a.rd == a.rs1)) || (load_b && b.rd == 0) {
        return None;
    }
    let ka = kind(a.op, if load_a { Op::Lb } else { Op::Sb });
    let kb = kind(b.op, if load_b { Op::Lb } else { Op::Sb });
    let step = match (load_a, load_b) {
        (true, true) => MIXED_LOADS[ka][kb],
        (true, false) => *LOAD_STORE[ka].get(kb)?,
        (false, true) => STORE_LOAD.get(ka)?[kb],
        (false, false) => *MIXED_STORES.get(ka)?.get(kb)?,
    };
    let (wa, wb) = (1i64 << (ka & 3), 1i64 << (kb & 3));
    let (oa, ob) = (i64::from(a.imm), i64::from(b.imm));
    let width = wa.max(wb);
    if (oa - ob) & (wa.min(wb) - 1) != 0 {
        return None;
    }
    let (lo, hi) = (oa.min(ob), (oa + wa).max(ob + wb));
    if hi - lo > 256 {
        return None;
    }
    let anchor = if wa >= wb { oa } else { ob };
    let min = (anchor - lo + width - 1) / width;
    let max = (4096 - (hi - anchor)) / width;
    a.link.set(anchor as u32 as usize | (min as usize) << 32 | ((max - min) as usize) << 48);
    Some(step)
}

macro_rules! mixed_steps {
    ($store_a:literal, $store_b:literal; [$($a:literal),*]; $kinds:tt) => {
        [$(mixed_steps!(@row $store_a, $store_b; $a; $kinds)),*]
    };
    (@row $store_a:literal, $store_b:literal; $a:literal; [$($b:literal),*]) => {
        [$(step_mixed_memory::<$a, $b, $store_a, $store_b> as Step),*]
    };
}
const MIXED_LOADS: [[Step; 7]; 7] = mixed_steps!(false, false; [0, 1, 2, 3, 4, 5, 6]; [0, 1, 2, 3, 4, 5, 6]);
const LOAD_STORE: [[Step; 4]; 7] = mixed_steps!(false, true; [0, 1, 2, 3, 4, 5, 6]; [0, 1, 2, 3]);
const STORE_LOAD: [[Step; 7]; 4] = mixed_steps!(true, false; [0, 1, 2, 3]; [0, 1, 2, 3, 4, 5, 6]);
const MIXED_STORES: [[Step; 4]; 4] = mixed_steps!(true, true; [0, 1, 2, 3]; [0, 1, 2, 3]);

#[inline(always)]
unsafe fn access_mixed<const KIND: u8, const STORE: bool>(h: &mut Hart, d: &DInsn, base: u64) -> u64 {
    let host = base.wrapping_add(d.imm as i64 as u64) as usize;
    if STORE {
        let value = h.rx_u(d.rs2);
        match KIND {
            0 => host_write::<1>(host, value),
            1 => host_write::<2>(host, value),
            2 => host_write::<4>(host, value),
            _ => host_write::<8>(host, value),
        }
        return 0;
    }
    let value = match KIND {
        0 => extend::<1, true>(host_read::<1>(host)),
        1 => extend::<2, true>(host_read::<2>(host)),
        2 => extend::<4, true>(host_read::<4>(host)),
        3 => host_read::<8>(host),
        4 => host_read::<1>(host),
        5 => host_read::<2>(host),
        _ => host_read::<4>(host),
    };
    h.set_x(d.rd, value);
    value
}

#[inline(never)]
unsafe fn step_mixed_memory<const A: u8, const B: u8, const STORE_A: bool, const STORE_B: bool>(
    chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus,
    d: *const DInsn, fa: u64, fb: u64,
) -> u64 {
    let next = pre2(d);
    let metadata = (*d).link.get();
    let base = h.rx_u((*d).rs1);
    let va = base.wrapping_add(metadata as u32 as i32 as i64 as u64);
    // The store TLB excludes decoded code, and a writable Sv39 leaf is
    // always readable. One store mapping safely covers both access kinds.
    let table = if STORE_A || STORE_B { tlb.add(crate::hart::FTLB_SETS) } else { tlb };
    let entry = tlb_entry(table, va);
    let shift = u32::from((A & 3).max(B & 3));
    let delta = va.wrapping_sub(entry.tag).rotate_right(shift);
    let min = (metadata >> 32) as u16;
    let range = (metadata >> 48) as u64;
    if entry.tag != u64::MAX && delta.wrapping_sub(u64::from(min)) <= range {
        let host = base.wrapping_add(entry.addend);
        let first = access_mixed::<A, STORE_A>(h, &*d, host);
        let last = access_mixed::<B, STORE_B>(h, &*d.add(1), host);
        return next.run(chain, tlb, h, bus, last, first);
    }
    invoke(STEPS[(*d).op as usize], chain, tlb, h, bus, d, fa, fb)
}

#[cfg(test)]
mod memory_run_tests {
    use super::*;

    #[test]
    fn normalized_integer_identities_preserve_registers() {
        let mut original = Hart::new(0, false);
        let mut normalized = Hart::new(0, false);
        let mut bus = Bus::new(4096, 1);
        let values = [0, 1, u64::MAX, 0x8000_0000, 0x89ab_cdef_1234_5678];
        for &op in ALU_RR.iter().chain(ALU_IMM.iter()) {
            for rs1 in 0..3 {
                for rs2 in 0..3 {
                    for imm in [-2048, -1, 0, 1, 31, 32, 63] {
                        for &value in &values {
                            let d = DInsn { op, rd: 1, rs1, rs2, imm, len: 4, ..DInsn::EMPTY };
                            let mut n = d.clone();
                            normalize_integer(&mut n);
                            original.x = [0; 32];
                            original.x[1] = value;
                            original.x[2] = value.rotate_left(17);
                            normalized.x = original.x;
                            original.exec(&mut bus, &d, 0x1000).unwrap();
                            normalized.exec(&mut bus, &n, 0x1000).unwrap();
                            assert_eq!(normalized.x, original.x, "{d:?} -> {n:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn chained_calls_and_csr_counts_match_uncached_execution() {
        // Repeated calls warm both direct links and the return-address cache.
        // The final addi and CSR share a block. A guest write below the count
        // already executed must also leave scheduler progress monotonic.
        for write_counter in [false, true] {
            for budget in [1, 3, 17, 64, 512] {
                let mut words: Vec<u32> = vec![
                    0x01400513, // addi a0,zero,20
                    0x00000593, // addi a1,zero,0
                    0x038000ef, // jal ra,+56 (function at byte 64)
                    0xfff50513, // addi a0,a0,-1
                    0xfe051ce3, // bne a0,zero,-8
                    0x00000013, // nop
                    0xb0202673, // csrrs a2,minstret,zero
                ];
                if write_counter {
                    words.push(0xb02 << 20 | 16 << 15 | 1 << 12 | 15 << 7 | 0x73);
                }
                words.extend([0x00060693, 0x10500073]); // mv a3,a2; wfi
                words.resize(16, 0x00000013);
                words.extend([
                    0x00758593, // addi a1,a1,7
                    0x00ba3023, // sd a1,0(s4)
                    0x000a3703, // ld a4,0(s4)
                    0x00008067, // ret
                ]);
                let mut h = Hart::new(0, false);
                let mut r = Hart::new(0, false);
                let mut bus = Bus::new(64 << 10, 1);
                let mut rb = Bus::new(64 << 10, 1);
                let start = RAM_BASE + 0x100;
                for (i, &word) in words.iter().enumerate() {
                    for b in [&mut bus, &mut rb] {
                        b.write::<4>(start + i as u64 * 4, word as u64).unwrap();
                    }
                }
                for warm in [false, true] {
                    for hart in [&mut h, &mut r] {
                        hart.pc = start;
                        hart.x = [0; 32];
                        hart.x[20] = RAM_BASE + 0x8000;
                        hart.x[16] = 3;
                        hart.instret = 0;
                        hart.chained = 0;
                        hart.instret_bias = 0;
                        hart.wfi = false;
                    }
                    for _ in 0..256 {
                        if h.wfi { break; }
                        h.run(&mut bus, budget);
                    }
                    for _ in 0..256 {
                        if r.wfi { break; }
                        r.step_uncached(&mut rb);
                    }
                    assert!(h.wfi && r.wfi, "budget={budget} warm={warm}");
                    assert_eq!(h.x, r.x, "budget={budget} warm={warm}");
                    assert_eq!((h.pc, h.instret), (r.pc, r.instret));
                    assert_eq!((h.x[11], h.x[12], h.x[13], h.x[14]), (140, 143, 143, 140));
                    assert_eq!(h.instret, if write_counter { 147 } else { 146 });
                    if write_counter {
                        assert_eq!(h.x[15], 144);
                        assert_eq!(h.csr_read(&bus, CSR_MINSTRET).unwrap(), 5);
                        assert_eq!(h.csr_read(&bus, CSR_MCYCLE).unwrap(), 147);
                    }
                    assert_eq!(bus.read::<8>(RAM_BASE + 0x8000).unwrap(), 140);
                }
            }
        }
    }

    #[test]
    fn threaded_memory_refreshes_the_context_at_each_entry() {
        let mut h = Hart::new(0, false);
        let mut bus = Bus::new(64 << 10, 1);
        let va = 0x4000_1000;
        let contexts = [(PRV_U, 0), (PRV_S, 0), (PRV_S, MSTATUS_SUM), (PRV_M, 0)];
        h.satp_sv39 = true;
        for (k, &(prv, status)) in contexts.iter().enumerate() {
            h.prv = prv;
            h.mstatus = status;
            h.select_dctx();
            let pa = RAM_BASE + 0x6000 + k as u64 * 4096;
            bus.write::<8>(pa, 0x1234_5678_0000_0000 + k as u64).unwrap();
            h.ftlb_fill(&bus, va, pa, false);
            h.ftlb_fill(&bus, va, pa, true);
        }
        let mut load = DInsn::EMPTY;
        load.op = Op::Ld; load.rd = 1; load.rs1 = 20; load.len = 4;
        let mut store = DInsn::EMPTY;
        store.op = Op::Sd; store.rs1 = 20; store.rs2 = 21;
        store.imm = 8; store.len = 4; store.off = 4;
        let mut end = DInsn::EMPTY; end.off = 8;
        let mut ops = [load, store, end];
        Hart::steps_for_test(&mut ops);
        for k in [3, 0, 2, 1, 0, 3, 1, 2] {
            let (prv, status) = contexts[k];
            h.prv = prv;
            h.mstatus = status;
            h.select_dctx();
            h.pc = 0x1000; h.block_pc = h.pc; h.break_chain();
            h.x[20] = va; h.x[21] = 0xfedc_ba98_0000_0000 + k as u64;
            let n = unsafe { go(&mut h, &mut bus, ops.as_ptr(), 0, 0) };
            assert_eq!((h.x[1], n), (0x1234_5678_0000_0000 + k as u64, 2));
            let pa = RAM_BASE + 0x6000 + k as u64 * 4096;
            assert_eq!(bus.read::<8>(pa + 8).unwrap(), h.x[21]);
        }
    }

    #[test]
    fn fused_blocks_match_uncached_execution_across_unit_boundaries() {
        let mut rng = 0x15e7_8a92_b314_604du64;
        let mut next = || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
        for case in 0..2000 {
            let mut words = Vec::new();
            for _ in 0..48 {
                let rd = 1 + (next() as u32 % 15);
                let a = next() as u32 % 16;
                let b = next() as u32 % 16;
                let imm = next() as u32 & 0xfff;
                let word = match next() % 12 {
                    0 => imm << 20 | a << 15 | rd << 7 | 0x13, // addi
                    1 => b << 20 | a << 15 | rd << 7 | 0x33, // add
                    2 => b << 20 | a << 15 | 4 << 12 | rd << 7 | 0x33, // xor
                    3 => (imm & 31) << 20 | a << 15 | 1 << 12 | rd << 7 | 0x1b, // slliw
                    4 => (imm & 0x1f8) << 20 | 20 << 15 | 3 << 12 | rd << 7 | 3, // ld
                    5 => { let off = imm & 0x1f8;
                        (off >> 5) << 25 | b << 20 | 20 << 15 | 3 << 12 | (off & 31) << 7 | 0x23 },
                    6 => 0x00001463 | b << 20 | a << 15, // bne +8
                    7 => 0x00000463 | b << 20 | a << 15, // beq +8
                    8 => (next() as u32 & 0xfffff000) | rd << 7 | 0x37,
                    9 => 0x02000033 | b << 20 | a << 15 | rd << 7, // mul
                    10 => 0x008000ef, // jal ra,+8
                    _ => b << 20 | a << 15 | rd << 7 | 0x3b, // addw
                };
                words.push(word);
            }
            words.push(0x73); // ecall ends the generated test program
            let mut h = Hart::new(0, false); let mut r = Hart::new(0, false);
            let mut bus = Bus::new(64 << 10, 1); let mut rb = Bus::new(64 << 10, 1);
            for hart in [&mut h, &mut r] { hart.pc = RAM_BASE + 0x100; }
            for (k, word) in words.iter().enumerate() {
                bus.write::<4>(h.pc + k as u64 * 4, *word as u64).unwrap();
                rb.write::<4>(r.pc + k as u64 * 4, *word as u64).unwrap();
            }
            for reg in 1..32 { h.x[reg] = next(); }
            h.x[20] = if case % 7 == 0 { 7 } else { RAM_BASE + 0x8000 };
            r.x = h.x;
            for _ in 0..32 {
                let block = h.block_at(&mut bus).unwrap().unwrap();
                let n = unsafe { h.exec_block(&mut bus, block, 0) };
                for _ in 0..n {
                    let d = r.decode_at(&mut rb).unwrap();
                    let pc = r.pc;
                    r.pc = pc.wrapping_add(d.len as u64);
                    if let Err(t) = r.exec(&mut rb, &d, pc) {
                        r.pc = pc; r.take_trap(t.cause as u64, t.tval);
                    }
                }
                assert_eq!(h.x, r.x, "registers in program {case}, pc={:#x}", h.pc);
                assert_eq!((h.pc, h.mepc, h.mcause, h.mtval, h.mstatus),
                    (r.pc, r.mepc, r.mcause, r.mtval, r.mstatus), "program {case}");
                assert_eq!(bus.ram, rb.ram, "memory in program {case}");
                if h.pc < RAM_BASE { break; }
            }
        }
    }

    #[test]
    fn coalesced_memory_matches_precise_steps_on_hits_and_faults() {
        let kinds = [(Op::Ld, 8), (Op::Lw, 4), (Op::Lwu, 4),
            (Op::Lh, 2), (Op::Lhu, 2), (Op::Lb, 1), (Op::Lbu, 1),
            (Op::Sd, 8), (Op::Sw, 4), (Op::Sh, 2), (Op::Sb, 1)];
        for (op, width) in kinds {
            for n in 2..=8 {
                for pattern in 0..3 {
                for base in [0, 7, 4090, RAM_BASE + 0x8000, RAM_BASE + 0x8001,
                    RAM_BASE + 0x8ff8, RAM_BASE + 0xfff8] {
                    for cached in [false, true] {
                        let mut ops: Vec<DInsn> = (0..n).map(|k| {
                            let mut d = DInsn::EMPTY;
                            d.op = op; d.rd = k as u8 + 1; d.rs1 = 20; d.rs2 = k as u8 + 1;
                            let slot = match pattern { 0 => k, 1 => n - 1 - k, _ => (k * 3) % n };
                            d.imm = (slot * width) as i32; d.len = 4; d.off = (k * 4) as i32;
                            d
                        }).collect();
                        let (len, step) = memory_run(&ops, true).unwrap();
                        assert_eq!(len, n);
                        let mut end = DInsn::EMPTY; end.off = n as i32 * 4;
                        ops.push(end);
                        Hart::steps_for_test(&mut ops);
                        let mut a = Hart::new(0, false); let mut b = Hart::new(0, false);
                        let mut ba = Bus::new(64 << 10, 1); let mut bb = Bus::new(64 << 10, 1);
                        for bus in [&mut ba, &mut bb] { bus.ram.fill(0x91); }
                        for (h, bus) in [(&mut a, &ba), (&mut b, &bb)] {
                            h.pc = 0x1000; h.block_pc = 0x1000; h.break_chain();
                            for r in 1..32 { h.x[r] = 0x8123_4567_89ab_cdefu64.wrapping_mul(r as u64); }
                            h.x[20] = base;
                            if cached && base >= RAM_BASE && base < RAM_BASE + (64 << 10) {
                                h.ftlb_fill(bus, base, base, false);
                                h.ftlb_fill(bus, base, base, true);
                            }
                        }
                        let na = unsafe { invoke(step, Chain::from_hart(&a), a.ltlb_cur, &mut a, &mut ba, ops.as_ptr(), 0, 0) };
                        let nb = unsafe { invoke(ops[0].step.unwrap(), Chain::from_hart(&b), b.ltlb_cur, &mut b, &mut bb, ops.as_ptr(), 0, 0) };
                        assert_eq!(a.x, b.x, "{op:?} n={n} base={base:#x} cached={cached}");
                        assert_eq!((a.pc, a.mepc, a.mcause, a.mtval, na), (b.pc, b.mepc, b.mcause, b.mtval, nb));
                        assert_eq!(ba.ram, bb.ram, "{op:?} n={n} base={base:#x}");
                    }
                }
                }
            }
        }
    }

    #[test]
    fn coalescing_rejects_address_base_clobbers_and_zero_loads() {
        for rd in [0, 20] {
            let mut a = DInsn::EMPTY;
            a.op = Op::Ld; a.rs1 = 20; a.rd = rd;
            let mut b = a.clone(); b.imm = 8;
            assert!(memory_run(&[a, b], true).is_none());
        }
    }

    #[test]
    fn mixed_memory_pair_matches_precise_path_across_page_edges() {
        let mut ops = [
            DInsn { op: Op::Lb, rs1: 4, rd: 5, imm: -3, off: 0, len: 4, ..DInsn::EMPTY },
            DInsn { op: Op::Ld, rs1: 4, rd: 4, off: 4, len: 4, ..DInsn::EMPTY },
            DInsn { off: 8, ..DInsn::EMPTY },
        ];
        Hart::steps_for_test(&mut ops);
        let pair = mixed_memory_pair(&ops[0], &ops[1]).unwrap();
        for base in [RAM_BASE + 8, RAM_BASE + 4096, RAM_BASE + 8192] {
            let mut bus = Bus::new(8192, 1);
            bus.ram.fill(0x91);
            for step in [pair, ops[0].step.unwrap()] {
                let mut hart = Hart::new(0, false);
                hart.block_pc = RAM_BASE;
                hart.x[4] = base;
                hart.ftlb_fill(&bus, base - 3, base - 3, false);
                hart.ftlb_fill(&bus, base, base, false);
                let n = unsafe { invoke(step, Chain::from_hart(&hart), hart.ltlb_cur, &mut hart, &mut bus, ops.as_ptr(), 0, 0) };
                assert_eq!(n, 2);
                assert_eq!(hart.x[5], (-111i64) as u64);
                if base == RAM_BASE + 8192 {
                    assert_eq!((hart.x[4], hart.mcause, hart.mtval), (base, EXC_LOAD_ACCESS, base));
                } else {
                    assert_eq!(hart.x[4], 0x9191_9191_9191_9191);
                    assert_eq!(hart.pc, RAM_BASE + 8);
                }
            }
        }
    }

    #[test]
    fn overwritten_intermediate_is_visible_to_a_fault_handler() {
        let mut ops = [
            DInsn { op: Op::Addi, rs1: 3, rd: 4, imm: 8, len: 4, ..DInsn::EMPTY },
            DInsn { op: Op::Ld, rs1: 4, rd: 4, off: 4, len: 4, ..DInsn::EMPTY },
            DInsn { off: 8, ..DInsn::EMPTY },
        ];
        Hart::steps_for_test(&mut ops);
        let step = overwrite_pair(&ops[0], &ops[1]).unwrap();
        let mut hart = Hart::new(0, false);
        let mut bus = Bus::new(4096, 1);
        let end = RAM_BASE + 4096;
        hart.block_pc = RAM_BASE;
        hart.x[3] = end - 8;
        let n = unsafe { invoke(step, Chain::from_hart(&hart), hart.ltlb_cur, &mut hart, &mut bus, ops.as_ptr(), 0, 0) };
        assert_eq!((n, hart.x[4], hart.mepc, hart.mcause, hart.mtval), (2, end, RAM_BASE + 4, EXC_LOAD_ACCESS, end));
    }

    #[test]
    fn fused_jumps_match_instruction_steps_including_slow_memory() {
        let kinds = [Op::Add, Op::Sub, Op::And, Op::Or, Op::Xor, Op::Sll, Op::Srl, Op::Sra,
            Op::Slt, Op::Sltu, Op::Mul, Op::Addw, Op::Subw, Op::Sllw, Op::Srlw, Op::Sraw,
            Op::Mulw, Op::Addi, Op::Andi, Op::Ori, Op::Xori, Op::Slli, Op::Srli, Op::Srai,
            Op::Addiw, Op::Slti, Op::Sltiu, Op::Slliw, Op::Srliw, Op::Sraiw,
            Op::Lui, Op::Auipc, Op::Ld, Op::Lw, Op::Lwu, Op::Lh, Op::Lhu, Op::Lb, Op::Lbu,
            Op::Sd, Op::Sw, Op::Sh, Op::Sb];
        for op in kinds {
            for jump in [Op::Jal, Op::Jalr] {
                for rd in [0, 1, 5, 10] {
                    for forwards in [false, true] {
                        for address in [0, RAM_BASE + 0x8000, RAM_BASE + 0x8001] {
                            for cached in [false, true] {
                                let mut a = DInsn::EMPTY;
                                a.op = op; a.rd = 1; a.rs1 = 20; a.rs2 = 3; a.len = 4;
                                a.imm = if is_load(op) || matches!(op, Op::Sd | Op::Sw | Op::Sh | Op::Sb) { 0 } else { 7 };
                                let mut j = DInsn::EMPTY;
                                j.op = jump; j.rd = rd; j.rs1 = if forwards { 1 } else { 3 };
                                j.imm = 0; j.off = 4; j.len = 4;
                                let mut end = DInsn::EMPTY; end.off = 8;
                                let mut ops = [a, j, end];
                                Hart::steps_for_test(&mut ops); fold_targets(&mut ops);
                                let forwarding = forwards && produces(op);
                                let step = jump_step(op, jump, forwarding).unwrap();
                                let mut h = Hart::new(0, false); let mut r = Hart::new(0, false);
                                let mut bus = Bus::new(64 << 10, 1); let mut rb = Bus::new(64 << 10, 1);
                                for (hart, mem) in [(&mut h, &mut bus), (&mut r, &mut rb)] {
                                    mem.ram.fill(0x91);
                                    hart.pc = 0x1000; hart.block_pc = 0x1000; hart.break_chain();
                                    hart.x[20] = address; hart.x[3] = 0x8000_1003;
                                    if cached && address >= RAM_BASE {
                                        hart.ftlb_fill(mem, address, address, false);
                                        hart.ftlb_fill(mem, address, address, true);
                                    }
                                }
                                let n = unsafe { invoke(step, Chain::from_hart(&h), h.ltlb_cur, &mut h, &mut bus, ops.as_ptr(), 0, 0) };
                                let nr = unsafe { invoke(ops[0].step.unwrap(), Chain::from_hart(&r), r.ltlb_cur, &mut r, &mut rb, ops.as_ptr(), 0, 0) };
                                assert_eq!(h.x, r.x, "{op:?} {jump:?} rd={rd} forward={forwarding}");
                                assert_eq!((h.pc, h.mepc, h.mcause, h.mtval, h.ras_sp, n),
                                    (r.pc, r.mepc, r.mcause, r.mtval, r.ras_sp, nr));
                                assert_eq!(bus.ram, rb.ram);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn pcrel_calls_preserve_both_destinations_and_return_hints() {
        for base_reg in [1, 5, 10, 31] {
            for dest in [0, 1, 5, 10, 31] {
                for hi in [i32::MIN, -4096, 0, 4096, 0x7ffff000] {
                    for low in [-2048, -1, 0, 1, 2047] {
                        let mut a = DInsn::EMPTY;
                        a.op = Op::Auipc; a.rd = base_reg; a.imm = hi; a.len = 4;
                        let mut b = DInsn::EMPTY;
                        b.op = Op::Jalr; b.rd = dest; b.rs1 = base_reg;
                        b.imm = low; b.len = 4; b.off = 4;
                        if dest == 0 && low == 0 && matches!(base_reg, 1 | 5) {
                            assert!(special_pair(&a, &b).is_none());
                            continue;
                        }
                        assert!(matches!(special_pair(&a, &b), Some(Idiom::PcRelJump)));
                        let mut end = DInsn::EMPTY; end.off = 8;
                        let mut ops = [a, b, end]; Hart::steps_for_test(&mut ops);
                        let mut h = Hart::new(0, false); let mut r = Hart::new(0, false);
                        let mut bus = Bus::new(4096, 1); let mut rb = Bus::new(4096, 1);
                        for hart in [&mut h, &mut r] {
                            hart.pc = 0xffff_ffff_8000_0ffc;
                            hart.block_pc = hart.pc; hart.break_chain();
                        }
                        let step = pcrel_jump(&ops[0], &ops[1]);
                        let n = unsafe { invoke(step, Chain::from_hart(&h), h.ltlb_cur, &mut h, &mut bus, ops.as_ptr(), 0, 0) };
                        let nr = unsafe { invoke(ops[0].step.unwrap(), Chain::from_hart(&r), r.ltlb_cur, &mut r, &mut rb, ops.as_ptr(), 0, 0) };
                        assert_eq!(h.x, r.x);
                        assert_eq!((h.pc, h.ras_sp, n), (r.pc, r.ras_sp, nr));
                    }
                }
            }
        }
    }
}

/// Whether `op` is a conditional branch, which a block can fall through.
const fn is_cond_branch(op: Op) -> bool {
    matches!(op, Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu)
}
