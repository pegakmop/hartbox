//! Page-bounded comparisons of two independent RAM streams.

use super::{
    extend, finish_direct_to, host_read, iterations_to_bound, pc_of, tlb_entry, Chain, STEPS,
};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart};
use crate::insn::{DInsn, Op, Step};

const ABSENT: usize = 7;

#[derive(Clone, Copy, Default)]
struct Input {
    value_reg: u8,
    cursor_reg: u8,
    cursor: u64,
    host: usize,
}

pub(super) fn plan(ops: &[DInsn]) -> Option<(Step, usize)> {
    if !(5..=7).contains(&ops.len()) {
        return None;
    }
    let last = ops.len() - 1;
    let mut loads = ops
        .iter()
        .enumerate()
        .filter(|(_, d)| super::super::is_load(d.op));
    let (ai, a) = loads.next()?;
    let (bi, b) = loads.next()?;
    if loads.next().is_some()
        || a.op != b.op
        || a.rs1 == 0
        || b.rs1 == 0
        || a.rd == 0
        || b.rd == 0
        || a.rd == b.rd
        || a.rs1 == b.rs1
        || [a.rd, b.rd].contains(&a.rs1)
        || [a.rd, b.rd].contains(&b.rs1)
    {
        return None;
    }
    let (width, signed) = match a.op {
        Op::Lb => (1, true),
        Op::Lbu => (1, false),
        Op::Lh => (2, true),
        Op::Lhu => (2, false),
        Op::Lw => (4, true),
        Op::Lwu => (4, false),
        _ => (8, false),
    };
    let mut updates = [ABSENT; 2];
    for (i, d) in ops.iter().enumerate().filter(|(_, d)| d.op == Op::Addi) {
        if d.rd != d.rs1 || d.imm != width {
            return None;
        }
        let slot = if d.rd == a.rs1 {
            0
        } else if d.rd == b.rs1 {
            1
        } else {
            return None;
        };
        if updates[slot] != ABSENT {
            return None;
        }
        updates[slot] = i;
    }
    if updates.contains(&ABSENT) {
        return None;
    }
    let mut pair = false;
    let mut needle = ABSENT;
    let mut bound = ABSENT;
    let mut needle_reg = 0;
    let mut bound_reg = 0;
    let mut bound_lane = 0;
    let live = [a.rd, b.rd, a.rs1, b.rs1];
    for (i, d) in ops.iter().enumerate() {
        if [ai, bi, updates[0], updates[1]].contains(&i) {
            continue;
        }
        let args = [d.rs1, d.rs2];
        let eq = if i == last { Op::Beq } else { Op::Bne };
        let ne = if i == last { Op::Bne } else { Op::Beq };
        if !pair && d.op == eq && ai < i && bi < i && args.contains(&a.rd) && args.contains(&b.rd) {
            pair = true;
            continue;
        }
        let invariant = if live.contains(&d.rs1) && !live.contains(&d.rs2) {
            Some((d.rs1, d.rs2))
        } else if live.contains(&d.rs2) && !live.contains(&d.rs1) {
            Some((d.rs2, d.rs1))
        } else {
            None
        };
        let (subject, value) = invariant?;
        if needle == ABSENT
            && d.op == ne
            && ((subject == a.rd && ai < i) || (subject == b.rd && bi < i))
        {
            needle = i;
            needle_reg = value;
            continue;
        }
        let lane = if subject == a.rs1 {
            0
        } else if subject == b.rs1 {
            1
        } else {
            return None;
        };
        let lt = if i == last { Op::Bltu } else { Op::Bgeu };
        if bound != ABSENT
            || updates[lane] >= i
            || !(d.op == ne || (d.op == lt && d.rs1 == subject))
        {
            return None;
        }
        bound = i;
        bound_reg = value;
        bound_lane = lane;
    }
    if !pair || last == ai || last == bi || updates.contains(&last) {
        return None;
    }
    // Three-bit instruction slots, two update-order flags, then the invariant
    // register numbers and the lane whose cursor the bound checks.
    let data = last
        | ai << 3
        | bi << 6
        | needle << 9
        | bound << 12
        | usize::from(updates[0] < ai) << 15
        | usize::from(updates[1] < bi) << 16
        | usize::from(needle_reg) << 17
        | usize::from(bound_reg) << 22
        | bound_lane << 27;
    let step: Step = match (width, signed) {
        (1, true) => step::<1, true>,
        (1, false) => step::<1, false>,
        (2, true) => step::<2, true>,
        (2, false) => step::<2, false>,
        (4, true) => step::<4, true>,
        (4, false) => step::<4, false>,
        _ => step::<8, false>,
    };
    Some((step, data))
}

#[inline(never)]
unsafe fn step<const W: usize, const SIGNED: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    debug_assert!(bus.mt.is_none());
    let data = (*d).link.get();
    let last = data & 7;
    let branch = d.add(last);
    let k = u64::from((*branch).index) + 1;
    let left = chain.left(h);
    let len = last as u64 + 1;
    let mut count = 1 + left.saturating_sub(k).div_ceil(len);
    let mut streams = [Input::default(); 2];
    for (lane, stream) in streams.iter_mut().enumerate() {
        let load = &*d.add((data >> (3 + lane * 3)) & 7);
        let cursor = h.rx_u(load.rs1);
        let before = (data >> (15 + lane)) & 1;
        let va = cursor.wrapping_add_signed(i64::from(load.imm) + (before * W) as i64);
        let entry = tlb_entry(tlb, va);
        if entry.tag != va & (!0xfff | (W as u64 - 1)) {
            return STEPS[(*d).op as usize](chain, tlb, h, bus, d, fa, fb);
        }
        count = count
            .min((4096 - (va & 0xfff)) / W as u64)
            .min((u64::MAX - cursor) / W as u64);
        *stream = Input {
            value_reg: load.rd,
            cursor_reg: load.rs1,
            cursor,
            host: va.wrapping_add(entry.addend) as usize,
        };
    }
    let bound = (data >> 12) & 7;
    if bound != ABSENT {
        let cursor = streams[(data >> 27) & 1].cursor;
        let limit = h.rx_u(((data >> 22) & 31) as u8);
        let op = if matches!((*d.add(bound)).op, Op::Bltu | Op::Bgeu) {
            Op::Bltu
        } else {
            Op::Bne
        };
        count = count.min(iterations_to_bound(op, cursor, limit, W as u64) - 1);
    }
    if count < 2 {
        return STEPS[(*d).op as usize](chain, tlb, h, bus, d, fa, fb);
    }
    let needle = (((data >> 9) & 7) != ABSENT)
        .then(|| h.rx_u(((data >> 17) & 31) as u8))
        .and_then(|value| {
            let raw = value & (u64::MAX >> (64 - W * 8));
            (extend::<W, SIGNED>(raw) == value).then_some(raw)
        });
    // Leave the first unequal lane or stopping value to the original body,
    // which can exit before either cursor update.
    let count = super::scan::first_stop::<W, true>(
        streams[0].host,
        streams[1].host,
        needle,
        count as usize,
    ) as u64;
    if count < 2 {
        return STEPS[(*d).op as usize](chain, tlb, h, bus, d, fa, fb);
    }
    for input in streams {
        let value = extend::<W, SIGNED>(host_read::<W>(input.host + (count as usize - 1) * W));
        h.set_x(input.value_reg, value);
        h.set_x(input.cursor_reg, input.cursor + count * W as u64);
    }
    let state = Chain::enter(h, chain.pc(h), left - (count - 1) * len);
    finish_direct_to(state, tlb, h, bus, branch, k, pc_of(state, h, d), 0, 0)
}
