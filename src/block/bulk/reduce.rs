//! Integer reductions of known consecutive RAM loads.

use crate::block::invoke;

use super::{
    extend, finish_direct_to, go_tlb, host_read, iterations_to_bound, pc_of, tlb_entry, Chain,
    Plan, Stream, STEPS,
};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart};
use crate::insn::{DInsn, Op, Step};

pub(super) fn plan(ops: &[DInsn]) -> Option<Plan> {
    let (branch, body) = ops.split_last()?;
    let first = body.first()?;
    let base = first.rs1;
    if body.len() < 3 || base == 0 {
        return None;
    }
    let width = match first.op {
        Op::Lb | Op::Lbu => 1,
        Op::Lh | Op::Lhu => 2,
        Op::Lw | Op::Lwu => 4,
        Op::Ld => 8,
        _ => return None,
    };
    let bound = match branch.op {
        Op::Bne | Op::Bltu if branch.rs1 == base => branch.rs2,
        Op::Bne if branch.rs2 == base => branch.rs1,
        _ => return None,
    };
    if bound == base {
        return None;
    }

    let mut stream = Stream::default();
    let mut loaded = 0u32;
    let mut pending = 0u32;
    let mut reduction = Op::Empty;
    let mut accumulator = 0;
    for d in body {
        if d.op == first.op {
            let bit = 1 << d.rd;
            if d.rs1 != base
                || d.rd == 0
                || [base, bound, accumulator].contains(&d.rd)
                || pending & bit != 0
            {
                return None;
            }
            stream.access(d.imm, width)?;
            loaded |= bit;
            pending |= bit;
        } else if d.op == Op::Addi && d.rs1 == base && d.rd == base && d.imm > 0 {
            if stream.advance != 0 {
                return None;
            }
            stream.advance = d.imm;
        } else {
            if reduction == Op::Empty {
                if !matches!(d.op, Op::Add | Op::Addw | Op::And | Op::Or | Op::Xor)
                    || [0, base, bound].contains(&d.rd)
                    || loaded & (1 << d.rd) != 0
                {
                    return None;
                }
                accumulator = d.rd;
                reduction = d.op;
            }
            if d.op != reduction || d.rd != accumulator {
                return None;
            }
            let other = if d.rs1 == accumulator {
                d.rs2
            } else if d.rs2 == accumulator {
                d.rs1
            } else {
                return None;
            };
            let bit = 1 << other;
            if pending & bit == 0 {
                return None;
            }
            pending &= !bit;
        }
    }
    if pending != 0 || !stream.valid(width) {
        return None;
    }
    let step = match first.op {
        Op::Lb => select::<1, true>(reduction),
        Op::Lbu => select::<1, false>(reduction),
        Op::Lh => select::<2, true>(reduction),
        Op::Lhu => select::<2, false>(reduction),
        Op::Lw => select::<4, true>(reduction),
        Op::Lwu => select::<4, false>(reduction),
        _ => select::<8, false>(reduction),
    };
    let data = ops.len()
        | stream.lanes << 8
        | usize::from(accumulator) << 16
        | usize::from(stream.start as u16) << 24;
    Some(Plan {
        len: ops.len(),
        data,
        step,
    })
}

fn select<const W: usize, const SIGNED: bool>(op: Op) -> Step {
    match op {
        Op::Add => step::<W, SIGNED, { Op::Add as u8 }>,
        Op::Addw => step::<W, SIGNED, { Op::Addw as u8 }>,
        Op::And => step::<W, SIGNED, { Op::And as u8 }>,
        Op::Or => step::<W, SIGNED, { Op::Or as u8 }>,
        Op::Xor => step::<W, SIGNED, { Op::Xor as u8 }>,
        _ => unreachable!(),
    }
}

#[inline(never)]
unsafe fn step<const W: usize, const SIGNED: bool, const OP: u8>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    debug_assert!(bus.mt.is_none());
    let first = &*d;
    let data = first.link.get();
    let len = data as u8 as usize;
    let stride = ((data >> 8) as u8 as usize) * W;
    let accumulator = (data >> 16) as u8;
    let cursor = h.rx_u(first.rs1);
    let va = cursor.wrapping_add_signed(i64::from((data >> 24) as i16));
    let entry = tlb_entry(tlb, va);
    if entry.tag != va & (!0xfff | (W as u64 - 1)) {
        return invoke(STEPS[first.op as usize], chain, tlb, h, bus, d, fa, fb);
    }
    let branch = &*d.add(len - 1);
    let after = u64::from(branch.index) + 1;
    let left = chain.left(h);
    let bound = h.rx_u(if branch.rs1 == first.rs1 {
        branch.rs2
    } else {
        branch.rs1
    });
    let count = ((4096 - (va & 0xfff)) / stride as u64)
        .min(1 + left.saturating_sub(after).div_ceil(len as u64))
        .min(iterations_to_bound(branch.op, cursor, bound, stride as u64));
    let bytes = count * stride as u64;
    if count == 0 || cursor.checked_add(bytes).is_none() {
        return invoke(STEPS[first.op as usize], chain, tlb, h, bus, d, fa, fb);
    }
    let host = va.wrapping_add(entry.addend) as usize;
    let mut result = h.rx_u(accumulator);
    for at in (0..bytes as usize).step_by(W) {
        let value = extend::<W, SIGNED>(host_read::<W>(host + at));
        result = match OP {
            op if op == Op::And as u8 => result & value,
            op if op == Op::Or as u8 => result | value,
            op if op == Op::Xor as u8 => result ^ value,
            _ => result.wrapping_add(value),
        };
    }
    if OP == Op::Addw as u8 {
        result = crate::block::sx32(result as u32);
    }
    h.set_x(accumulator, result);
    let mut last_host = host + bytes as usize - stride;
    for i in 0..len - 1 {
        let load = &*d.add(i);
        if load.op == first.op {
            h.set_x(load.rd, extend::<W, SIGNED>(host_read::<W>(last_host)));
            last_host += W;
        }
    }
    let end = cursor + bytes;
    h.set_x(first.rs1, end);
    let state = Chain::enter(h, chain.pc(h), left - (count - 1) * len as u64);
    let last = h.rx_u((*d.add(len - 2)).rd);
    let taken = if branch.op == Op::Bltu {
        end < bound
    } else {
        end != bound
    };
    if taken {
        return finish_direct_to(
            state,
            tlb,
            h,
            bus,
            d.add(len - 1),
            after,
            pc_of(state, h, d),
            last,
            last,
        );
    }
    go_tlb(state, tlb, h, bus, d.add(len), last, last)
}
