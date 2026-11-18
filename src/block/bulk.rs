//! Page-bounded fast paths for regular RAM copy and fill loops.

use super::{extend, finish_direct_to, go_tlb, host_read, host_write, pc_of, tlb_entry, Chain, STEPS};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart, FTLB_SETS};
use crate::insn::{DInsn, Op, Step};

mod compare;
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
    let (branch, body) = ops.split_last()?;
    let first = body.first()?;
    debug_assert_eq!(branch.off.wrapping_add(branch.imm), first.off);
    let (width, store, step) = match first.op {
        Op::Lb => (1, Op::Sb, step_bulk::<1, true, true> as Step),
        Op::Lbu => (1, Op::Sb, step_bulk::<1, true, false> as Step),
        Op::Lh => (2, Op::Sh, step_bulk::<2, true, true> as Step),
        Op::Lhu => (2, Op::Sh, step_bulk::<2, true, false> as Step),
        Op::Lw => (4, Op::Sw, step_bulk::<4, true, true> as Step),
        Op::Lwu => (4, Op::Sw, step_bulk::<4, true, false> as Step),
        Op::Ld => (8, Op::Sd, step_bulk::<8, true, false> as Step),
        Op::Sb => (1, Op::Sb, step_bulk::<1, false, false> as Step),
        Op::Sh => (2, Op::Sh, step_bulk::<2, false, false> as Step),
        Op::Sw => (4, Op::Sw, step_bulk::<4, false, false> as Step),
        Op::Sd => (8, Op::Sd, step_bulk::<8, false, false> as Step),
        _ => return None,
    };
    let copy = first.op != store;
    let src = if copy { first.rs1 } else { 0 };
    let dst = body.iter().find(|d| d.op == store)?.rs1;
    if dst == 0 || (copy && (src == 0 || src == dst)) {
        return None;
    }
    let is_cursor = |r: u8| r == dst || (copy && r == src);
    let bound = if is_cursor(branch.rs1) && matches!(branch.op, Op::Bne | Op::Bltu) {
        branch.rs2
    } else if branch.op == Op::Bne && is_cursor(branch.rs2) {
        branch.rs1
    } else {
        return None;
    };
    if is_cursor(bound) {
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
        } else if d.op == Op::Addi && d.rd == d.rs1 && d.imm > 0 {
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
    let stride = dest.lanes * width;
    if stride != dest.advance as usize
        || (copy && (source.lanes != dest.lanes || source.advance != dest.advance))
    {
        return None;
    }
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

#[derive(Default)]
struct Stream {
    start: i16,
    advance: i32,
    lanes: usize,
}

impl Stream {
    fn access(&mut self, displacement: i32, width: usize) -> Option<()> {
        let off = displacement.checked_add(self.advance)?;
        if self.lanes == 0 {
            self.start = i16::try_from(off).ok()?;
        } else if i64::from(off) != i64::from(self.start) + (self.lanes * width) as i64 {
            return None;
        }
        self.lanes += 1;
        Some(())
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
unsafe fn step_bulk<const W: usize, const COPY: bool, const SIGNED: bool>(
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
    let mut room = 4096 - (dst_va & 0xfff);
    let value = h.rx_u(first.rs2);
    if COPY {
        let read = tlb_entry(tlb, src_va);
        if read.tag != src_va & tag_mask {
            return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
        }
        src_host = src_va.wrapping_add(read.addend) as usize;
        room = room.min(4096 - (src_va & 0xfff));
    }

    let branch = &*d.add(len - 1);
    let k = u64::from(branch.index) + 1;
    let branch_ptr = |r| r == dst_reg || (COPY && r == first.rs1);
    let (cursor_reg, bound_reg) = if branch_ptr(branch.rs1) {
        (branch.rs1, branch.rs2)
    } else {
        (branch.rs2, branch.rs1)
    };
    let left = chain.left(h);
    let bound = h.rx_u(bound_reg);
    let cursor = h.rx_u(cursor_reg);
    // The scalar interpreter completes the current block even at a small
    // budget. A subsequent iteration is allowed only if the branch can chain.
    let budget = 1 + left.saturating_sub(k).div_ceil(len as u64);
    let count = (room / stride as u64).min(budget).min(iterations_to_bound(
        branch.op,
        cursor,
        bound,
        stride as u64,
    ));
    let bytes = count * stride as u64;
    let dst_host = dst_va.wrapping_add(write.addend) as usize;
    if count == 0
        || dst.checked_add(bytes).is_none()
        || (COPY && (src.checked_add(bytes).is_none()
            || (dst_host > src_host && dst_host - src_host < bytes as usize)))
    {
        return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
    }

    if COPY {
        // Ascending guest loads still see the original data if the destination
        // is below the source. Read the final temporaries before the host copy
        // because it can overwrite source bytes through a virtual alias.
        let mut at = src_host + (bytes as usize - stride);
        for offset in 0..len - 1 {
            let insn = &*d.add(offset);
            if insn.op == first.op {
                h.set_x(insn.rd, extend::<W, SIGNED>(host_read::<W>(at)));
                at += W;
            }
        }
        if dst_host.abs_diff(src_host) >= bytes as usize {
            std::ptr::copy_nonoverlapping(src_host as *const u8, dst_host as *mut u8, bytes as usize);
        } else if dst_host != src_host {
            std::ptr::copy(src_host as *const u8, dst_host as *mut u8, bytes as usize);
        }
        h.set_x(first.rs1, src + bytes);
    } else {
        fill::<W>(dst_host, value, bytes as usize);
    }
    h.set_x(dst_reg, dst + bytes);

    let remaining = left - (count - 1) * len as u64;
    let state = Chain::enter(h, chain.pc(h), remaining);
    let target = h.rx_u(cursor_reg);
    let taken = if branch.op == Op::Bltu {
        target < bound
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
                            step_bulk::<4, true, true> as Step
                        } else {
                            step_bulk::<4, false, false> as Step
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
