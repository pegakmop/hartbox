//! Batch sequential loads through one live RAM translation.

use crate::block::invoke;

use super::{
    extend, finish_direct_to, go_tlb, host_read, iterations_to_bound, pc_of, tlb_entry, Chain,
    STEPS,
};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart};
use crate::insn::{DInsn, Op, Step};

pub(super) fn plan(ops: &[DInsn]) -> Option<Step> {
    let [first, second, branch] = ops else {
        return None;
    };
    let leading = first.op == Op::Addi;
    let (load, advance) = if leading {
        (second, first)
    } else {
        (first, second)
    };
    let width = match load.op {
        Op::Lb | Op::Lbu => 1,
        Op::Lh | Op::Lhu => 2,
        Op::Lw | Op::Lwu => 4,
        Op::Ld => 8,
        _ => return None,
    };
    let cursor = load.rs1;
    if cursor == 0
        || load.rd == cursor
        || advance.op != Op::Addi
        || advance.rs1 != cursor
        || advance.rd != cursor
        || advance.imm != width
    {
        return None;
    }
    let compare = |reg: u8| match branch.op {
        Op::Bne if branch.rs1 == reg => Some(branch.rs2),
        Op::Bne if branch.rs2 == reg => Some(branch.rs1),
        Op::Bltu if branch.rs1 == reg => Some(branch.rs2),
        _ => None,
    };
    let search = load.rd != 0
        && branch.op == Op::Bne
        && compare(load.rd).is_some_and(|r| r != cursor && r != load.rd);
    if !search && !compare(cursor).is_some_and(|r| r != cursor && r != load.rd) {
        return None;
    }
    macro_rules! select {
        ($w:literal, $signed:literal) => {
            match (leading, search) {
                (true, true) => step::<$w, $signed, true, true> as Step,
                (true, false) => step::<$w, $signed, true, false> as Step,
                (false, true) => step::<$w, $signed, false, true> as Step,
                (false, false) => step::<$w, $signed, false, false> as Step,
            }
        };
    }
    Some(match load.op {
        Op::Lb => select!(1, true),
        Op::Lbu => select!(1, false),
        Op::Lh => select!(2, true),
        Op::Lhu => select!(2, false),
        Op::Lw => select!(4, true),
        Op::Lwu => select!(4, false),
        _ => select!(8, false),
    })
}

#[inline(never)]
unsafe fn step<const W: usize, const SIGNED: bool, const LEADING: bool, const SEARCH: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    debug_assert!(bus.mt.is_none());
    let load = &*d.add(usize::from(LEADING));
    let branch = &*d.add(2);
    let cursor = h.rx_u(load.rs1);
    let va = cursor.wrapping_add_signed(i64::from(load.imm) + if LEADING { W as i64 } else { 0 });
    let entry = tlb_entry(tlb, va);
    if entry.tag != va & (!0xfff | (W as u64 - 1)) {
        return invoke(STEPS[(*d).op as usize], chain, tlb, h, bus, d, fa, fb);
    }
    let subject = if SEARCH { load.rd } else { load.rs1 };
    let compare = h.rx_u(if branch.rs1 == subject {
        branch.rs2
    } else {
        branch.rs1
    });
    let k = u64::from(branch.index) + 1;
    let left = chain.left(h);
    let budget = 1 + left.saturating_sub(k).div_ceil(3);
    let capacity = (4096 - (va & 0xfff)) / W as u64;
    let mut count = capacity.min(budget);
    let host = va.wrapping_add(entry.addend) as usize;
    if SEARCH {
        let raw = compare & (u64::MAX >> (64 - W * 8));
        if extend::<W, SIGNED>(raw) == compare {
            count =
                (first_stop::<W, false>(host, 0, Some(raw), count as usize) as u64 + 1).min(count);
        }
    } else {
        count = count.min(iterations_to_bound(branch.op, cursor, compare, W as u64));
    }
    let Some(end) = cursor.checked_add(count * W as u64) else {
        return invoke(STEPS[(*d).op as usize], chain, tlb, h, bus, d, fa, fb);
    };
    let value = extend::<W, SIGNED>(host_read::<W>(host + (count as usize - 1) * W));
    h.wx_u(load.rd, value);
    h.set_x(load.rs1, end);
    let remaining = left - (count - 1) * 3;
    let state = Chain::enter(h, chain.pc(h), remaining);
    let last = if LEADING { value } else { end };
    let result = if SEARCH { value } else { end };
    let taken = if branch.op == Op::Bltu {
        result < compare
    } else {
        result != compare
    };
    if taken {
        return finish_direct_to(
            state,
            tlb,
            h,
            bus,
            d.add(2),
            k,
            pc_of(state, h, d),
            last,
            last,
        );
    }
    go_tlb(state, tlb, h, bus, d.add(3), last, last)
}

/// Index of the first needle match or unequal lane, or `count` if neither
/// stops the scan. The caller checked the page bounds of every input.
#[inline(always)]
pub(super) unsafe fn first_stop<const W: usize, const PAIR: bool>(
    host: usize,
    other: usize,
    needle: Option<u64>,
    count: usize,
) -> usize {
    let mut at = 0;
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    {
        use std::arch::aarch64::*;
        let lanes = 16 / W;
        while count - at >= lanes {
            let a = vld1q_u8((host + at * W) as *const u8);
            let mut mask = if PAIR {
                let b = vld1q_u8((other + at * W) as *const u8);
                vmvnq_u8(vceqq_u8(a, b))
            } else {
                vdupq_n_u8(0)
            };
            if let Some(needle) = needle {
                let matches = match W {
                    1 => vceqq_u8(a, vdupq_n_u8(needle as u8)),
                    2 => vreinterpretq_u8_u16(vceqq_u16(
                        vreinterpretq_u16_u8(a),
                        vdupq_n_u16(needle as u16),
                    )),
                    4 => vreinterpretq_u8_u32(vceqq_u32(
                        vreinterpretq_u32_u8(a),
                        vdupq_n_u32(needle as u32),
                    )),
                    _ => vreinterpretq_u8_u64(vceqq_u64(
                        vreinterpretq_u64_u8(a),
                        vdupq_n_u64(needle),
                    )),
                };
                mask = vorrq_u8(mask, matches);
            }
            let mask = vreinterpretq_u64_u8(mask);
            let low = vgetq_lane_u64::<0>(mask);
            let high = vgetq_lane_u64::<1>(mask);
            if low | high != 0 {
                let bit = if low != 0 {
                    low.trailing_zeros()
                } else {
                    64 + high.trailing_zeros()
                };
                return at + bit as usize / (8 * W);
            }
            at += lanes;
        }
    }
    while at < count {
        let value = host_read::<W>(host + at * W);
        if needle == Some(value) || (PAIR && value != host_read::<W>(other + at * W)) {
            return at;
        }
        at += 1;
    }
    count
}
