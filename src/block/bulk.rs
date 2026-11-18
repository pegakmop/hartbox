//! Page-bounded fast paths for regular RAM copy, fill and scan loops.

use super::{extend, finish_direct_to, go_tlb, host_read, host_write, pc_of, tlb_entry, Chain, STEPS};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart, FTLB_SETS};
use crate::insn::{DInsn, Op, Step};

mod compare;
mod reduce;
mod scan;

#[derive(Clone, Copy)]
pub(super) struct Plan {
    pub(super) len: usize,
    pub(super) data: usize,
    pub(super) step: Step,
}

/// Match actual loads and stores, including interleaved pointer updates. Each
/// copied lane must come from its corresponding load. Temporaries can be
/// reused, as long as each store still sees the correct value.
pub(super) fn plan(ops: &[DInsn]) -> Option<Plan> {
    if let Some(step) = scan::plan(ops) {
        return Some(Plan { len: ops.len(), data: 0, step });
    }
    if let Some((step, data)) = compare::plan(ops) {
        return Some(Plan { len: ops.len(), data, step });
    }
    if let Some(plan) = reduce::plan(ops) {
        return Some(plan);
    }
    let (branch, body) = ops.split_last()?;
    let first = body.first()?;
    debug_assert_eq!(branch.off.wrapping_add(branch.imm), first.off);
    let (width, store) = match first.op {
        Op::Lb | Op::Lbu | Op::Sb => (1, Op::Sb),
        Op::Lh | Op::Lhu | Op::Sh => (2, Op::Sh),
        Op::Lw | Op::Lwu | Op::Sw => (4, Op::Sw),
        Op::Ld | Op::Sd => (8, Op::Sd),
        _ => return None,
    };
    let copy = first.op != store;
    let src = if copy { first.rs1 } else { 0 };
    let dst = body.iter().find(|d| d.op == store)?.rs1;
    if dst == 0 || (copy && (src == 0 || src == dst)) {
        return None;
    }
    let is_cursor = |r: u8| r == dst || (copy && r == src);
    let mut operands = [branch.rs1, branch.rs2];
    let search = copy && branch.op == Op::Bne && operands.contains(&first.rd);
    let is_subject = |r| if search { r == first.rd } else { is_cursor(r) };
    if !is_subject(operands[0]) {
        operands.swap(0, 1);
    }
    let [subject, bound] = operands;
    if !is_subject(subject) || is_subject(bound) || (search && is_cursor(bound)) {
        return None;
    }

    let mut origin = [usize::MAX; 32];
    let mut source = Stream::default();
    let mut dest = Stream::default();
    for d in body {
        if copy && d.op == first.op {
            if d.rs1 != src || d.rd == 0 || is_cursor(d.rd) || d.rd == bound {
                return None;
            }
            origin[d.rd as usize] = source.lanes;
            source.access(d.imm, width)?;
        } else if d.op == store {
            if d.rs1 != dst
                || (copy && origin[d.rs2 as usize] != dest.lanes)
                || (!copy && (d.rs2 != first.rs2 || d.rs2 == dst))
            {
                return None;
            }
            dest.access(d.imm, width)?;
        } else if d.op == Op::Addi && d.rd == d.rs1 && d.imm != 0 {
            if copy && d.rd == src && source.advance == 0 {
                source.advance = d.imm;
            } else if d.rd == dst && dest.advance == 0 {
                dest.advance = d.imm;
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    let backward = dest.advance < 0;
    if !dest.valid(width)
        || (copy && (source.lanes != dest.lanes || source.advance != dest.advance || !source.valid(width)))
        || (search && (backward || source.lanes != 1))
        || !(branch.op == Op::Bne || (branch.op == Op::Bltu && (branch.rs1 == subject) != backward))
    {
        return None;
    }
    let step = bulk_step(first.op, backward, search);
    let len = ops.len();
    // The memory instruction's link field is otherwise unused. Its layout
    // is [length, lanes, destination register, padding, two i16 offsets].
    let data = len
        | (dest.lanes << 8)
        | (usize::from(dst) << 16)
        | (usize::from(source.start as u16) << 24)
        | (usize::from(dest.start as u16) << 40);
    Some(Plan { len, data, step })
}

fn bulk_step(op: Op, backward: bool, search: bool) -> Step {
    macro_rules! copy {
        ($w:literal, $signed:literal) => {
            if search {
                step_bulk::<$w, true, $signed, false, true> as Step
            } else if backward {
                step_bulk::<$w, true, $signed, true, false> as Step
            } else {
                step_bulk::<$w, true, $signed, false, false> as Step
            }
        };
    }
    macro_rules! fill {
        ($w:literal) => {
            if backward {
                step_bulk::<$w, false, false, true, false> as Step
            } else {
                step_bulk::<$w, false, false, false, false> as Step
            }
        };
    }
    match op {
        Op::Lb => copy!(1, true),
        Op::Lbu => copy!(1, false),
        Op::Lh => copy!(2, true),
        Op::Lhu => copy!(2, false),
        Op::Lw => copy!(4, true),
        Op::Lwu => copy!(4, false),
        Op::Ld => copy!(8, false),
        Op::Sb => fill!(1),
        Op::Sh => fill!(2),
        Op::Sw => fill!(4),
        Op::Sd => fill!(8),
        _ => unreachable!(),
    }
}

#[derive(Default)]
struct Stream {
    start: i16,
    advance: i32,
    lanes: usize,
    direction: i32,
}

impl Stream {
    fn access(&mut self, displacement: i32, width: usize) -> Option<()> {
        let off = displacement.checked_add(self.advance)?;
        if self.lanes == 0 {
            self.start = i16::try_from(off).ok()?;
        } else {
            if self.lanes == 1 {
                self.direction = (off - i32::from(self.start)).signum();
            }
            if i64::from(off) != i64::from(self.start) + (self.lanes * width) as i64 * i64::from(self.direction)
                || self.direction == 0
            {
                return None;
            }
        }
        self.lanes += 1;
        Some(())
    }

    fn valid(&self, width: usize) -> bool {
        self.lanes != 0 && (self.lanes * width) == self.advance.unsigned_abs() as usize
            && (self.lanes == 1 || self.direction == self.advance.signum())
    }
}

/// How many complete iterations are permitted by the branch, before applying
/// the page and scheduler limits. The guest tests the branch after each step.
fn iterations_to_bound(op: Op, ptr: u64, bound: u64, stride: u64) -> u64 {
    if op == Op::Bltu {
        bound.saturating_sub(ptr).div_ceil(stride).max(1)
    } else {
        let distance = bound.wrapping_sub(ptr);
        if distance != 0 && distance.is_multiple_of(stride) {
            distance / stride
        } else {
            u64::MAX
        }
    }
}

#[inline(never)]
unsafe fn step_bulk<const W: usize, const COPY: bool, const SIGNED: bool, const BACKWARD: bool, const SEARCH: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    // Bulk steps are only selected in cooperative execution. This keeps
    // another hart from observing host loads and stores grouped by memcpy.
    debug_assert!(bus.mt.is_none());
    let first = &*d;
    let data = first.link.get();
    let len = data as u8 as usize;
    let stride = ((data >> 8) as u8 as usize) * W;
    let dst_reg = (data >> 16) as u8;
    let dst = h.rx_u(dst_reg);
    let dst_va = dst.wrapping_add((data >> 40) as i16 as i64 as u64);
    let write = tlb_entry(tlb.add(FTLB_SETS), dst_va);
    let tag_mask = !0xfff | (W as u64 - 1);
    if write.tag != dst_va & tag_mask {
        return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
    }

    let src = if COPY { h.rx_u(first.rs1) } else { 0 };
    let src_va = src.wrapping_add((data >> 24) as i16 as i64 as u64);
    let mut src_host = 0;
    let room_at = |va: u64| {
        let off = va & 0xfff;
        if BACKWARD { off + W as u64 } else { 4096 - off }
    };
    let mut room = room_at(dst_va);
    let value = h.rx_u(first.rs2);
    if COPY {
        let read = tlb_entry(tlb, src_va);
        if read.tag != src_va & tag_mask {
            return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
        }
        src_host = src_va.wrapping_add(read.addend) as usize;
        room = room.min(room_at(src_va));
    }

    let branch = &*d.add(len - 1);
    let k = u64::from(branch.index) + 1;
    let is_subject = |r| {
        if SEARCH { r == first.rd } else { r == dst_reg || (COPY && r == first.rs1) }
    };
    let (subject, bound_reg) = if is_subject(branch.rs1) {
        (branch.rs1, branch.rs2)
    } else {
        (branch.rs2, branch.rs1)
    };
    let left = chain.left(h);
    let bound = h.rx_u(bound_reg);
    // The scalar interpreter completes the current block even at a small
    // budget. A subsequent iteration is allowed only if the branch can chain.
    let budget = 1 + left.saturating_sub(k).div_ceil(len as u64);
    let mut count = (room / stride as u64).min(budget);
    if count == 0 {
        return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
    }
    if SEARCH {
        let raw = bound & (u64::MAX >> (64 - W * 8));
        if extend::<W, SIGNED>(raw) == bound {
            count = count.min(scan::first_stop::<W, false>(src_host, 0, Some(raw), count as usize) as u64 + 1);
        }
    } else {
        let cursor = h.rx_u(subject);
        let (start, stop) = if BACKWARD { (bound, cursor) } else { (cursor, bound) };
        count = count.min(iterations_to_bound(branch.op, start, stop, stride as u64));
    }
    let bytes = count * stride as u64;
    let advance = if BACKWARD { -(bytes as i64) } else { bytes as i64 };
    let dst_host = dst_va.wrapping_add(write.addend) as usize;
    let Some(dst_end) = dst.checked_add_signed(advance) else {
        return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let low = |host| if BACKWARD { host - (bytes as usize - W) } else { host };

    if COPY {
        let Some(src_end) = src.checked_add_signed(advance) else {
            return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
        };
        let distance = dst_host.abs_diff(src_host);
        let overwrites_future = distance != 0 && distance < bytes as usize
            && (dst_host < src_host) == BACKWARD;
        if overwrites_future {
            return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
        }
        // Read the final iteration's temporaries before the host copy because
        // it can overwrite source bytes through a virtual alias.
        let mut offset = bytes as usize - stride;
        for index in 0..len - 1 {
            let insn = &*d.add(index);
            if insn.op == first.op {
                let at = if BACKWARD { src_host - offset } else { src_host + offset };
                h.set_x(insn.rd, extend::<W, SIGNED>(host_read::<W>(at)));
                offset += W;
            }
        }
        if distance >= bytes as usize {
            std::ptr::copy_nonoverlapping(low(src_host) as *const u8, low(dst_host) as *mut u8, bytes as usize);
        } else if dst_host != src_host {
            std::ptr::copy(low(src_host) as *const u8, low(dst_host) as *mut u8, bytes as usize);
        }
        h.set_x(first.rs1, src_end);
    } else {
        fill::<W>(low(dst_host), value, bytes as usize);
    }
    h.set_x(dst_reg, dst_end);

    let remaining = left - (count - 1) * len as u64;
    let state = Chain::enter(h, chain.pc(h), remaining);
    let target = h.rx_u(subject);
    let taken = if branch.op == Op::Bltu {
        if BACKWARD { target > bound } else { target < bound }
    } else {
        target != bound
    };
    let last = h.rx_u((*d.add(len - 2)).rd);
    if taken {
        finish_direct_to(
            state,
            tlb,
            h,
            bus,
            d.add(len - 1),
            k,
            pc_of(state, h, d),
            last,
            last,
        )
    } else {
        go_tlb(state, tlb, h, bus, d.add(len), last, last)
    }
}

#[inline(always)]
unsafe fn fill<const W: usize>(host: usize, value: u64, bytes: usize) {
    let byte = value as u8;
    let width_mask = u64::MAX >> (64 - W * 8);
    if (value ^ (u64::from(byte) * 0x0101_0101_0101_0101)) & width_mask == 0 {
        std::ptr::write_bytes(host as *mut u8, byte, bytes);
        return;
    }

    let pattern = match W {
        2 => u64::from(value as u16) * 0x0001_0001_0001_0001,
        4 => u64::from(value as u32) * 0x0000_0001_0000_0001,
        _ => value,
    };
    let wide = bytes & !7;
    for at in (0..wide).step_by(8) {
        host_write::<8>(host + at, pattern);
    }
    for at in (wide..bytes).step_by(W) {
        host_write::<W>(host + at, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::RAM_BASE;

    fn addi(rd: u32, rs: u32, imm: u32) -> u32 {
        (imm << 20) | (rs << 15) | (rd << 7) | 0x13
    }

    fn load(rd: u32, base: u32, off: u32) -> u32 {
        (off << 20) | (base << 15) | (2 << 12) | (rd << 7) | 3
    }

    fn store(base: u32, rs: u32, off: u32) -> u32 {
        ((off >> 5) << 25) | (rs << 20) | (base << 15) | (2 << 12) | ((off & 31) << 7) | 0x23
    }

    fn branch(left: u32, right: u32, cond: u32, off: i32) -> u32 {
        let bits = off as u32;
        ((bits >> 12 & 1) << 31)
            | ((bits >> 5 & 63) << 25)
            | (right << 20)
            | (left << 15)
            | (cond << 12)
            | ((bits >> 1 & 15) << 8)
            | ((bits >> 11 & 1) << 7)
            | 0x63
    }

    #[test]
    fn bulk_paths_match_scalar_registers_memory_and_retirement() {
        let src = RAM_BASE + 0x3ff8;
        let dst = RAM_BASE + 0x5ffc;
        let lanes = 24;
        for (copy, first_dst, value, cond) in [
            (true, dst, 0, 6),
            (true, src + 4, 0, 1),
            (true, src - 4, 0, 1),
            (false, dst, 0xa5a5_a5a5, 6),
            (false, dst, 0x1234_5678, 1),
        ] {
            let mut code = if copy {
                // Reuse the loaded register, and advance the source before the
                // second store. The last word is sign-extended in the guest.
                vec![
                    load(9, 5, 0),
                    store(6, 9, 0),
                    load(9, 5, 4),
                    addi(5, 5, 8),
                    store(6, 9, 4),
                ]
            } else {
                vec![store(6, 9, 0), store(6, 9, 4)]
            };
            code.push(addi(6, 6, 8));
            let cursor = if copy { 5 } else { 6 };
            code.push(branch(cursor, 8, cond, -(code.len() as i32) * 4));
            code.push(addi(12, 9, 7));
            let done = RAM_BASE + (code.len() as u64) * 4 + 4096;
            code.push(0x0000_106f); // jal x0, +4096
            for budget in [1, 7, 16, 4096] {
                let (mut fast, mut scalar) = (Hart::new(0, false), Hart::new(0, false));
                let (mut bus, mut reference) = (Bus::new(64 << 10, 1), Bus::new(64 << 10, 1));
                for (hart, memory) in [(&mut fast, &mut bus), (&mut scalar, &mut reference)] {
                    hart.pc = RAM_BASE;
                    hart.x[5] = src;
                    hart.x[6] = first_dst;
                    hart.x[8] = (if copy { src } else { first_dst }) + lanes * 4;
                    hart.x[9] = value;
                    for (i, &word) in code.iter().enumerate() {
                        memory
                            .write::<4>(RAM_BASE + i as u64 * 4, word as u64)
                            .unwrap();
                    }
                    for i in 0..lanes {
                        memory.write::<4>(src + 4 * i, 0x8123_4567 + i).unwrap();
                    }
                    for i in [src, src + 4096] {
                        hart.ftlb_fill(memory, i, i, false);
                    }
                    for i in [first_dst, (first_dst & !0xfff) + 4096] {
                        hart.ftlb_fill(memory, i, i, true);
                    }
                }
                for batch in 0..=lanes {
                    let block = fast.next_block(&mut bus).unwrap().unwrap();
                    if batch == 0 {
                        let ops = unsafe { super::super::Block::ops_ptr(block) };
                        let step = unsafe { (*ops).step.unwrap() };
                        let expected = if copy {
                            step_bulk::<4, true, true, false, false> as Step
                        } else {
                            step_bulk::<4, false, false, false, false> as Step
                        };
                        assert!(std::ptr::fn_addr_eq(step, expected), "bulk plan missing");
                    }
                    let retired = unsafe { fast.exec_block(&mut bus, block, budget) };
                    for _ in 0..retired {
                        scalar.step_uncached(&mut reference);
                    }
                    assert_eq!(
                        (fast.pc, fast.instret, fast.x),
                        (scalar.pc, scalar.instret, scalar.x)
                    );
                    assert_eq!(bus.ram, reference.ram);
                    if fast.pc == done {
                        break;
                    }
                }
                assert_eq!(fast.pc, done);
            }
        }
    }
}
