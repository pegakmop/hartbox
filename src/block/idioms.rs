//! Statically compiled handlers for algebraic relationships in decoded operands.

use super::{finish_jalr_to, pc_of, pre2, pre3, spec_load, spec_store, sx32, Chain, STEPS};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart};
use crate::insn::{DInsn, Op, Step};

pub(super) fn triple(a: &DInsn, b: &DInsn, c: &DInsn) -> Option<Step> {
    if a.op == Op::Auipc && c.op == Op::Jalr {
        return load_jump(a, b, c);
    }
    rotate(a, b, c)
}

/// Fold a computed base and a following displacement. A failed probe runs
/// the original base instruction first, making its result visible on a trap.
pub(super) fn address_pair(a: &DInsn, b: &DInsn) -> Option<Step> {
    if !matches!(a.op, Op::Addi | Op::Auipc) || a.rd == 0 || b.rs1 != a.rd {
        return None;
    }
    let pc_relative = a.op == Op::Auipc;
    let save = a.rd != b.rd;
    macro_rules! load {
        ($w:literal, $signed:literal) => {
            match (pc_relative, save) {
                (false, false) => load_at::<$w, $signed, false, false> as Step,
                (false, true) => load_at::<$w, $signed, false, true> as Step,
                (true, false) => load_at::<$w, $signed, true, false> as Step,
                (true, true) => load_at::<$w, $signed, true, true> as Step,
            }
        };
    }
    macro_rules! store {
        ($w:literal) => {
            if pc_relative {
                store_at::<$w, true> as Step
            } else {
                store_at::<$w, false> as Step
            }
        };
    }
    let step = match b.op {
        Op::Lb if b.rd != 0 => load!(1, true),
        Op::Lbu if b.rd != 0 => load!(1, false),
        Op::Lh if b.rd != 0 => load!(2, true),
        Op::Lhu if b.rd != 0 => load!(2, false),
        Op::Lw if b.rd != 0 => load!(4, true),
        Op::Lwu if b.rd != 0 => load!(4, false),
        Op::Ld if b.rd != 0 => load!(8, false),
        Op::Sb => store!(1),
        Op::Sh => store!(2),
        Op::Sw => store!(4),
        Op::Sd => store!(8),
        _ => return None,
    };
    let delta =
        i64::from(a.imm) + i64::from(b.imm) + if pc_relative { i64::from(a.off) } else { 0 };
    a.link.set(delta as usize);
    Some(step)
}

#[inline(always)]
fn address_origin<const PC: bool>(chain: Chain, h: &Hart, d: &DInsn) -> u64 {
    if PC {
        chain.pc(h)
    } else {
        h.rx_u(d.rs1)
    }
}

#[inline(always)]
fn intermediate_base<const PC: bool>(origin: u64, d: &DInsn) -> u64 {
    origin.wrapping_add_signed(i64::from(d.imm) + if PC { i64::from(d.off) } else { 0 })
}

#[inline(never)]
unsafe fn load_at<const W: usize, const SIGNED: bool, const PC: bool, const SAVE: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let next = pre2(d);
    let a = &*d;
    let origin = address_origin::<PC>(chain, h, a);
    let va = origin.wrapping_add(a.link.get() as u64);
    let Some(value) = spec_load::<W, SIGNED>(tlb, h, bus, va) else {
        return STEPS[a.op as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let base = if SAVE {
        let base = intermediate_base::<PC>(origin, a);
        h.set_x(a.rd, base);
        base
    } else {
        0
    };
    h.set_x((*d.add(1)).rd, value);
    next(chain, tlb, h, bus, d.add(2), value, base)
}

#[inline(never)]
unsafe fn store_at<const W: usize, const PC: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let next = pre2(d);
    let (a, b) = (&*d, &*d.add(1));
    let origin = address_origin::<PC>(chain, h, a);
    let base = intermediate_base::<PC>(origin, a);
    let va = origin.wrapping_add(a.link.get() as u64);
    let value = if a.rd == b.rs2 { base } else { h.rx_u(b.rs2) };
    if !spec_store::<W>(tlb, h, bus, va, value) {
        return STEPS[a.op as usize](chain, tlb, h, bus, d, fa, fb);
    }
    h.set_x(a.rd, base);
    next(chain, tlb, h, bus, d.add(2), base, base)
}

/// A PC-relative RAM load feeds an indirect jump. Translation and the load
/// still happen on every execution, so a slot can change between calls.
fn load_jump(a: &DInsn, b: &DInsn, c: &DInsn) -> Option<Step> {
    if a.rd == 0 || b.rd == 0 || b.rs1 != a.rd || c.rs1 != b.rd {
        return None;
    }
    let keep_base = a.rd != b.rd && a.rd != c.rd;
    let keep_load = b.rd != c.rd;
    macro_rules! select {
        ($w:literal, $signed:literal) => {
            match (keep_base, keep_load) {
                (false, false) => step_load_jump::<$w, $signed, false, false> as Step,
                (false, true) => step_load_jump::<$w, $signed, false, true> as Step,
                (true, false) => step_load_jump::<$w, $signed, true, false> as Step,
                (true, true) => step_load_jump::<$w, $signed, true, true> as Step,
            }
        };
    }
    let step = match b.op {
        Op::Ld => select!(8, false),
        Op::Lw => select!(4, true),
        Op::Lwu => select!(4, false),
        _ => return None,
    };
    let delta = i64::from(a.off) + i64::from(a.imm) + i64::from(b.imm);
    a.link.set(delta as usize);
    Some(step)
}

#[inline(never)]
unsafe fn step_load_jump<
    const W: usize,
    const SIGNED: bool,
    const KEEP_BASE: bool,
    const KEEP_LOAD: bool,
>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let a = &*d;
    let va = chain.pc(h).wrapping_add(a.link.get() as u64);
    let Some(value) = spec_load::<W, SIGNED>(tlb, h, bus, va) else {
        return STEPS[Op::Auipc as usize](chain, tlb, h, bus, d, fa, fb);
    };
    if KEEP_BASE {
        h.set_x(
            a.rd,
            pc_of(chain, h, d).wrapping_add_signed(i64::from(a.imm)),
        );
    }
    if KEEP_LOAD {
        h.set_x((*d.add(1)).rd, value);
    }
    let jump = d.add(2);
    let c = &*jump;
    let target = value.wrapping_add_signed(i64::from(c.imm)) & !1;
    h.wx_u(c.rd, pc_of(chain, h, jump).wrapping_add(u64::from(c.len)));
    finish_jalr_to(chain, tlb, h, bus, jump, target)
}

/// Two complementary shifts of one register produce disjoint bit ranges.
/// OR, XOR, and wrapping addition all combine those ranges into a rotate.
fn rotate(a: &DInsn, b: &DInsn, c: &DInsn) -> Option<Step> {
    let (left, word) = match (a.op, b.op) {
        (Op::Slli, Op::Srli) => (true, false),
        (Op::Srli, Op::Slli) => (false, false),
        (Op::Slliw, Op::Srliw) => (true, true),
        (Op::Srliw, Op::Slliw) => (false, true),
        _ => return None,
    };
    if a.rs1 != b.rs1
        || a.rd == a.rs1
        || a.rd == b.rd
        || a.rd == 0
        || b.rd == 0
        || c.rd == 0
        || a.imm + b.imm != if word { 32 } else { 64 }
        || !((c.rs1 == a.rd && c.rs2 == b.rd) || (c.rs1 == b.rd && c.rs2 == a.rd))
        || !matches!(
            (word, c.op),
            (true, Op::Addw) | (false, Op::Add | Op::Or | Op::Xor)
        )
        || (c.rd != a.rd && c.rd != b.rd)
    {
        return None;
    }
    let keep_first = c.rd == b.rd;
    macro_rules! select {
        ($left:literal, $word:literal) => {
            if keep_first {
                step_rotate::<$left, $word, true> as Step
            } else {
                step_rotate::<$left, $word, false> as Step
            }
        };
    }
    Some(match (left, word) {
        (true, true) => select!(true, true),
        (true, false) => select!(true, false),
        (false, true) => select!(false, true),
        (false, false) => select!(false, false),
    })
}

#[inline(never)]
unsafe fn step_rotate<const LEFT: bool, const WORD: bool, const KEEP_FIRST: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    _fa: u64,
    _fb: u64,
) -> u64 {
    let next = pre3(d);
    let a = &*d;
    let keep = &*d.add(usize::from(!KEEP_FIRST));
    let x = h.rx_u(a.rs1);
    let amount = a.imm as u32;
    let shift = keep.imm as u32;
    let (value, saved) = if WORD {
        let x = x as u32;
        let value = if LEFT {
            x.rotate_left(amount)
        } else {
            x.rotate_right(amount)
        };
        let saved = if LEFT == KEEP_FIRST {
            x << shift
        } else {
            x >> shift
        };
        (sx32(value), sx32(saved))
    } else {
        let value = if LEFT {
            x.rotate_left(amount)
        } else {
            x.rotate_right(amount)
        };
        let saved = if LEFT == KEEP_FIRST {
            x << shift
        } else {
            x >> shift
        };
        (value, saved)
    };
    h.set_x(keep.rd, saved);
    h.set_x((*d.add(2)).rd, value);
    // If the second shift was overwritten, no consumer can ask for its value.
    next(chain, tlb, h, bus, d.add(3), value, saved)
}
