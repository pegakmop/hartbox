//! Static fusion for integer operations, memory accesses and branches.

use super::{
    alu, extend, finish_direct_to, finish_jalr_to, host_read, host_write, is_alu, is_imm,
    pc_of, pre2, pre3, pre4, produces, ras_push_at, retired, taken, tlb_entry, tlb_load,
    tlb_store, Chain, STEPS,
};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart, FTLB_SETS};
use crate::insn::{DInsn, Op, Step};

/// Run the instruction `d` with opcode `OP` if it completes on a fast path: an
/// ALU result, or a load or store that needs no trap and no MMIO. Returns false
/// and changes nothing otherwise, and the caller then runs the normal step.
/// A register it writes is never `x0` (`build_block` keeps such instructions out of
/// fused units, see `fuses`), so the write needs no test.
#[inline(always)]
unsafe fn try_fast_v<const OP: u8>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    d: *const DInsn,
) -> Option<u64> {
    try_fast_input::<OP, false, true>(chain, tlb, h, d, 0)
}

/// `FORWARD` is selected when the block is built, so forwarding a dependency
/// inside a fused unit adds no register-number comparisons to execution.
#[inline(always)]
unsafe fn try_fast_input<const OP: u8, const FORWARD: bool, const WRITE: bool>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    d: *const DInsn,
    value: u64,
) -> Option<u64> {
    let di = &*d;
    let is = |op: Op| OP == op as u8;
    if is_alu(OP) {
        let x = if FORWARD { value } else { h.rx_u(di.rs1) };
        let y = if is_imm(OP) {
            di.imm as u64
        } else {
            h.rx_u(di.rs2)
        };
        let r = alu::<OP>(x, y);
        if WRITE {
            h.set_x(di.rd, r);
        }
        return Some(r);
    }
    if is(Op::Lui) {
        if WRITE {
            h.set_x(di.rd, di.imm as u64);
        }
        return Some(di.imm as u64);
    }
    if is(Op::Auipc) {
        let r = pc_of(chain, h, d).wrapping_add(di.imm as u64);
        if WRITE {
            h.set_x(di.rd, r);
        }
        return Some(r);
    }
    if is(Op::Ld)
        || is(Op::Lw)
        || is(Op::Lwu)
        || is(Op::Lh)
        || is(Op::Lhu)
        || is(Op::Lb)
        || is(Op::Lbu)
    {
        let (w, signed) = if is(Op::Ld) {
            (8, true)
        } else if is(Op::Lw) {
            (4, true)
        } else if is(Op::Lwu) {
            (4, false)
        } else if is(Op::Lh) {
            (2, true)
        } else if is(Op::Lhu) {
            (2, false)
        } else if is(Op::Lb) {
            (1, true)
        } else {
            (1, false)
        };
        let va = (if FORWARD { value } else { h.rx_u(di.rs1) }).wrapping_add(di.imm as u64);
        let raw = match w {
            8 => tlb_load::<8>(tlb, h, va),
            4 => tlb_load::<4>(tlb, h, va),
            2 => tlb_load::<2>(tlb, h, va),
            _ => tlb_load::<1>(tlb, h, va),
        };
        let v = raw?;
        let v = if signed {
            match w {
                8 => v,
                4 => extend::<4, true>(v),
                2 => extend::<2, true>(v),
                _ => extend::<1, true>(v),
            }
        } else {
            v
        };
        if WRITE {
            h.set_x(di.rd, v);
        }
        return Some(v);
    }
    if is(Op::Sd) || is(Op::Sw) || is(Op::Sh) || is(Op::Sb) {
        let w = if is(Op::Sd) {
            8
        } else if is(Op::Sw) {
            4
        } else if is(Op::Sh) {
            2
        } else {
            1
        };
        let va = (if FORWARD { value } else { h.rx_u(di.rs1) }).wrapping_add(di.imm as u64);
        let v = h.rx_u(di.rs2);
        let ok = match w {
            8 => tlb_store::<8>(tlb, h, va, v),
            4 => tlb_store::<4>(tlb, h, va, v),
            2 => tlb_store::<2>(tlb, h, va, v),
            _ => tlb_store::<1>(tlb, h, va, v),
        };
        return if ok { Some(0) } else { None };
    }
    None
}

/// Two instructions, `A` then `B`, in one dispatch. Each half runs its fast
/// path; if one cannot complete, the normal step of that half runs instead.
/// Fused pairs read their operands from the register file and pass no result on.
#[inline(never)]
unsafe fn step_pair<const A: u8, const B: u8, const FORWARD: bool>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre2(d);
    crate::counters::inc(&crate::counters::PAIRS);
    crate::counters::inc_op(A);
    crate::counters::inc_op(B);
    if !FORWARD && memory_width(A) != 0 && memory_width(B) != 0 {
        if let Some((a, b)) = independent_memory::<A, B>(tlb, h, d) {
            return nx(chain, tlb, h, bus, d.add(2), b, a);
        }
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    }
    let Some(a) = try_fast_v::<A>(chain, tlb, h, d) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let d2 = d.add(1);
    if let Some(b) = try_fast_input::<B, FORWARD, true>(chain, tlb, h, d2, a) {
        return nx(chain, tlb, h, bus, d2.add(1), b, a);
    }
    STEPS[B as usize](chain, tlb, h, bus, d2, a, fa)
}

const fn memory_width(op: u8) -> usize {
    match op {
        op if op == Op::Lb as u8 || op == Op::Lbu as u8 || op == Op::Sb as u8 => 1,
        op if op == Op::Lh as u8 || op == Op::Lhu as u8 || op == Op::Sh as u8 => 2,
        op if op == Op::Lw as u8 || op == Op::Lwu as u8 || op == Op::Sw as u8 => 4,
        op if op == Op::Ld as u8 || op == Op::Sd as u8 => 8,
        _ => 0,
    }
}

const fn is_store(op: u8) -> bool {
    op >= Op::Sb as u8 && op <= Op::Sd as u8
}

/// Probe both addresses before accessing either. Independence lets the host
/// overlap the table loads and use one guard. Guest accesses still run in
/// order.
#[inline(always)]
unsafe fn independent_memory<const A: u8, const B: u8>(
    tlb: *mut FastTlb, h: &mut Hart, d: *const DInsn,
) -> Option<(u64, u64)> {
    let a = &*d;
    let b = &*d.add(1);
    let va = h.rx_u(a.rs1).wrapping_add_signed(i64::from(a.imm));
    let vb = h.rx_u(b.rs1).wrapping_add_signed(i64::from(b.imm));
    let ea = tlb_entry(tlb.add(FTLB_SETS * usize::from(is_store(A))), va);
    let eb = tlb_entry(tlb.add(FTLB_SETS * usize::from(is_store(B))), vb);
    let miss_a = ea.tag ^ (va & (!0xfff | (memory_width(A) as u64 - 1)));
    let miss_b = eb.tag ^ (vb & (!0xfff | (memory_width(B) as u64 - 1)));
    if miss_a | miss_b != 0 {
        return None;
    }
    let first = memory_at::<A>(h, a, va.wrapping_add(ea.addend) as usize);
    let last = memory_at::<B>(h, b, vb.wrapping_add(eb.addend) as usize);
    Some((first, last))
}

#[inline(always)]
unsafe fn memory_at<const OP: u8>(h: &mut Hart, d: &DInsn, host: usize) -> u64 {
    let width = memory_width(OP);
    if is_store(OP) {
        let value = h.rx_u(d.rs2);
        match width {
            1 => host_write::<1>(host, value),
            2 => host_write::<2>(host, value),
            4 => host_write::<4>(host, value),
            _ => host_write::<8>(host, value),
        }
        return 0;
    }
    let value = match width {
        1 => extend::<1, true>(host_read::<1>(host)),
        2 => extend::<2, true>(host_read::<2>(host)),
        4 => extend::<4, true>(host_read::<4>(host)),
        _ => host_read::<8>(host),
    };
    let value = match OP {
        op if op == Op::Lbu as u8 => value as u8 as u64,
        op if op == Op::Lhu as u8 => value as u16 as u64,
        op if op == Op::Lwu as u8 => value as u32 as u64,
        _ => value,
    };
    h.set_x(d.rd, value);
    value
}

/// The second operation consumes and overwrites the first operation's result.
/// Commit the intermediate value only if the second operation takes a slow path.
#[inline(never)]
unsafe fn step_overwrite<const A: u8, const B: u8>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let next = pre2(d);
    let Some(a) = try_fast_input::<A, false, false>(chain, tlb, h, d, 0) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let d2 = d.add(1);
    if let Some(b) = try_fast_input::<B, true, true>(chain, tlb, h, d2, a) {
        return next(chain, tlb, h, bus, d.add(2), b, a);
    }
    h.set_x((*d).rd, a);
    STEPS[B as usize](chain, tlb, h, bus, d2, a, fa)
}

/// Three instructions in one dispatch. Both adjacent dependencies can use
/// the preceding result directly. A miss continues at the precise instruction.
#[inline(never)]
unsafe fn step_triple<const A: u8, const B: u8, const C: u8, const AB: bool, const BC: bool>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre3(d);
    crate::counters::inc(&crate::counters::PAIRS);
    let Some(a) = try_fast_v::<A>(chain, tlb, h, d) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let d2 = d.add(1);
    let Some(b) = try_fast_input::<B, AB, true>(chain, tlb, h, d2, a) else {
        return STEPS[B as usize](chain, tlb, h, bus, d2, a, fa);
    };
    let d3 = d2.add(1);
    if let Some(c) = try_fast_input::<C, BC, true>(chain, tlb, h, d3, b) {
        return nx(chain, tlb, h, bus, d3.add(1), c, b);
    }
    STEPS[C as usize](chain, tlb, h, bus, d3, b, a)
}

/// Four instructions, A B C D, in one dispatch. Each half runs its fast path;
/// a half that cannot complete falls back to its normal step.
#[inline(never)]
unsafe fn step_quad<const A: u8, const B: u8, const C: u8, const D: u8>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre4(d);
    crate::counters::inc(&crate::counters::PAIRS);
    let Some(a) = try_fast_v::<A>(chain, tlb, h, d) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let d2 = d.add(1);
    let Some(b) = try_fast_v::<B>(chain, tlb, h, d2) else {
        return STEPS[B as usize](chain, tlb, h, bus, d2, a, fa);
    };
    let d3 = d2.add(1);
    let Some(c) = try_fast_v::<C>(chain, tlb, h, d3) else {
        return STEPS[C as usize](chain, tlb, h, bus, d3, b, a);
    };
    let d4 = d3.add(1);
    if let Some(value) = try_fast_v::<D>(chain, tlb, h, d4) {
        return nx(chain, tlb, h, bus, d4.add(1), value, c);
    }
    STEPS[D as usize](chain, tlb, h, bus, d4, c, b)
}

/// An instruction `A` that completes on a fast path, then the conditional branch
/// `OP` that follows it, in one dispatch. The branch may sit inside the block (its
/// fall-through continues after it) or end it; either way only the taken edge leaves.
/// If `A` cannot finish on its fast path its ordinary step runs, and the branch then
/// runs as its own unit.
#[inline(never)]
unsafe fn step_xbr<const A: u8, const OP: u8, const XF: u8>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let nx = pre2(d);
    crate::counters::inc(&crate::counters::PAIRS);
    let Some(r) = try_fast_v::<A>(chain, tlb, h, d) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let d2 = d.add(1);
    let di = &*d2;
    // `XF` says which branch operands are the result of `A`: those come from `r` instead
    // of being read back from the register file `A` has just written.
    let a = if XF & 1 != 0 { r } else { h.rx_u(di.rs1) };
    let b = if XF & 4 != 0 {
        0
    } else if XF & 2 != 0 {
        r
    } else {
        h.rx_u(di.rs2)
    };
    if taken::<OP>(a, b) {
        return finish_direct_to(
            chain,
            tlb,
            h,
            bus,
            d2,
            retired(h, d2) + 1,
            chain.pc(h).wrapping_add(di.imm as i64 as u64),
            fa,
            fb,
        );
    }
    nx(chain, tlb, h, bus, d2.add(1), r, r)
}

/// A fast instruction followed by a jump. The old generic pair handed the
/// jump to a separate handler; complete it here, forwarding the target base
/// directly when the first instruction produced it.
#[inline(never)]
unsafe fn step_xjump<const A: u8, const INDIRECT: bool, const FORWARD: bool>(
    chain: Chain,
    tlb: *mut crate::hart::FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let Some(value) = try_fast_v::<A>(chain, tlb, h, d) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let j = d.add(1);
    let di = &*j;
    if INDIRECT {
        let base = if FORWARD { value } else { h.rx_u(di.rs1) };
        let target = base.wrapping_add(di.imm as i64 as u64) & !1;
        h.wx_u(di.rd, pc_of(chain, h, j).wrapping_add(di.len as u64));
        finish_jalr_to(chain, tlb, h, bus, j, target)
    } else {
        h.wx_u(di.rd, pc_of(chain, h, j).wrapping_add(di.len as u64));
        if di.rd == 1 || di.rd == 5 {
            ras_push_at(chain, h, j);
        }
        finish_direct_to(
            chain,
            tlb,
            h,
            bus,
            j,
            di.index as u64 + 1,
            chain.pc(h).wrapping_add(di.imm as i64 as u64),
            fa,
            fb,
        )
    }
}

pub(super) fn jump_step(a: Op, b: Op, forwards: bool) -> Option<Step> {
    if !matches!(b, Op::Jal | Op::Jalr) {
        return None;
    }
    macro_rules! select {
        ($($op:ident),* $(,)?) => {
            match a {
                $(Op::$op => Some(if b == Op::Jal {
                    step_xjump::<{ Op::$op as u8 }, false, false> as Step
                } else if forwards {
                    step_xjump::<{ Op::$op as u8 }, true, true> as Step
                } else {
                    step_xjump::<{ Op::$op as u8 }, true, false> as Step
                }),)*
                _ => None,
            }
        };
    }
    select!(
        Add, Sub, And, Or, Xor, Sll, Srl, Sra, Slt, Sltu, Mul, Addw, Subw, Sllw, Srlw, Sraw, Mulw,
        Addi, Andi, Ori, Xori, Slli, Srli, Srai, Addiw, Slti, Sltiu, Slliw, Srliw, Sraiw, Lui,
        Auipc, Ld, Lw, Lwu, Lh, Lhu, Lb, Lbu, Sd, Sw, Sh, Sb
    )
}

/// The fused step for an instruction `a` followed by the conditional branch `b`: any
/// instruction with a fast path (ALU, `lui`, `auipc`, loads, stores) paired with any of
/// the six branches. It is a rule about the shape of the code, not a list of sequences
/// that were seen running.
pub(super) fn xbr_step(a: Op, b: Op, xf: u8) -> Option<Step> {
    macro_rules! with_xf {
        ($a:ident, $b:ident) => {
            match xf {
                0 => Some(step_xbr::<{ Op::$a as u8 }, { Op::$b as u8 }, 0> as Step),
                1 => Some(step_xbr::<{ Op::$a as u8 }, { Op::$b as u8 }, 1> as Step),
                2 => Some(step_xbr::<{ Op::$a as u8 }, { Op::$b as u8 }, 2> as Step),
                4 => Some(step_xbr::<{ Op::$a as u8 }, { Op::$b as u8 }, 4> as Step),
                5 => Some(step_xbr::<{ Op::$a as u8 }, { Op::$b as u8 }, 5> as Step),
                _ => Some(step_xbr::<{ Op::$a as u8 }, { Op::$b as u8 }, 3> as Step),
            }
        };
    }
    macro_rules! with_branch {
        ($a:ident) => {
            match b {
                Op::Beq => with_xf!($a, Beq),
                Op::Bne => with_xf!($a, Bne),
                Op::Blt => with_xf!($a, Blt),
                Op::Bge => with_xf!($a, Bge),
                Op::Bltu => with_xf!($a, Bltu),
                Op::Bgeu => with_xf!($a, Bgeu),
                _ => None,
            }
        };
    }
    macro_rules! xbr {
        ($($a:ident),* $(,)?) => {
            match a {
                $(Op::$a => with_branch!($a),)*
                _ => None,
            }
        };
    }
    xbr!(
        Add, Sub, And, Or, Xor, Sll, Srl, Sra, Slt, Sltu, Mul, Addw, Subw, Sllw, Srlw, Sraw, Mulw,
        Addi, Andi, Ori, Xori, Slli, Srli, Srai, Addiw, Slti, Sltiu, Slliw, Srliw, Sraiw, Lui,
        Auipc, Ld, Lw, Lwu, Lh, Lhu, Lb, Lbu, Sd, Sw, Sh, Sb,
    )
}

/// The fused quad-step for four opcodes, if one is provided.
pub(super) fn quad_step(a: Op, b: Op, c: Op, d: Op) -> Option<Step> {
    macro_rules! quad {
        ($($w:ident, $x:ident, $y:ident, $z:ident);* $(;)?) => {
            match (a, b, c, d) {
                $((Op::$w, Op::$x, Op::$y, Op::$z) => Some(step_quad::<{ Op::$w as u8 }, { Op::$x as u8 }, { Op::$y as u8 }, { Op::$z as u8 }> as Step),)*
                _ => None,
            }
        };
    }
    quad!(
        // Mined 4-grams from Alpine/apk/busybox/Python execution.
        Xor, Srliw, Slliw, Addw;
        Addw, Srliw, Slliw, Addw;
        Srliw, Slliw, Addw, Srliw;
        Srliw, Slliw, Addw, Xor;
        Add, Add, Add, Add;
        Addw, Slliw, Addw, Xor;
        Add, Add, Add, Jal;
        Sd, Sd, Sd, Sd;
        Lw, Srliw, Addi, Addw;
        Xor, Addw, Lw, Srliw;
    )
}

/// The fused triple-step for three opcodes, if one is provided.
pub(super) fn first_forward(a: &DInsn, b: &DInsn) -> bool {
    a.rd != 0 && produces(a.op) && (b.rs1 == a.rd || commute_for_forwarding(a, b))
}

/// An interchangeable right operand can use the same statically selected
/// forwarding path as a left operand. The planner swaps the fields only for
/// an installed fusion.
pub(super) fn commute_for_forwarding(a: &DInsn, b: &DInsn) -> bool {
    matches!(b.op, Op::Add | Op::Addw | Op::Mul | Op::Mulw | Op::And | Op::Or | Op::Xor)
        && a.rd != 0 && a.rd == b.rs2 && a.rd != b.rs1 && produces(a.op)
}

#[inline(always)]
fn pair_variant<const A: u8, const B: u8>(forward: bool) -> Step {
    if forward {
        step_pair::<A, B, true>
    } else {
        step_pair::<A, B, false>
    }
}

pub(super) fn forward_store(a: &DInsn, b: &DInsn) -> Option<Step> {
    if a.rd == 0 || a.rd != b.rs2 || !is_store(b.op as u8) {
        return None;
    }
    let forward_base = a.rd == b.rs1;
    macro_rules! select {
        ($($op:ident),* $(,)?) => {
            match a.op {
                $(Op::$op => {
                    let choose = |op| [
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sb as u8 }, 1, false> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sb as u8 }, 1, true> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sh as u8 }, 2, false> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sh as u8 }, 2, true> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sw as u8 }, 4, false> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sw as u8 }, 4, true> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sd as u8 }, 8, false> as Step,
                        step_forward_store::<{ Op::$op as u8 }, { Op::Sd as u8 }, 8, true> as Step,
                    ][op];
                    Some(choose((b.op as usize - Op::Sb as usize) * 2 + usize::from(forward_base)))
                },)*
                _ => None,
            }
        };
    }
    select!(Add, Sub, And, Or, Xor, Sll, Srl, Sra, Slt, Sltu, Mul, Addw, Subw, Sllw, Srlw, Sraw, Mulw,
        Addi, Andi, Ori, Xori, Slli, Srli, Srai, Addiw, Slti, Sltiu, Slliw, Srliw, Sraiw, Lui, Auipc,
        Ld, Lw, Lwu, Lh, Lhu, Lb, Lbu)
}

#[inline(never)]
unsafe fn step_forward_store<const A: u8, const B: u8, const N: usize, const BASE: bool>(
    chain: Chain, tlb: *mut FastTlb, h: &mut Hart, bus: &mut Bus,
    d: *const DInsn, fa: u64, fb: u64,
) -> u64 {
    let next = pre2(d);
    let Some(value) = try_fast_v::<A>(chain, tlb, h, d) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let store = &*d.add(1);
    let base = if BASE { value } else { h.rx_u(store.rs1) };
    let va = base.wrapping_add_signed(i64::from(store.imm));
    if tlb_store::<N>(tlb, h, va, value) {
        return next(chain, tlb, h, bus, d.add(2), value, value);
    }
    STEPS[B as usize](chain, tlb, h, bus, d.add(1), value, fa)
}

#[inline(always)]
fn triple_variant<const A: u8, const B: u8, const C: u8>(ab: bool, bc: bool) -> Step {
    match (ab, bc) {
        (false, false) => step_triple::<A, B, C, false, false>,
        (true, false) => step_triple::<A, B, C, true, false>,
        (false, true) => step_triple::<A, B, C, false, true>,
        (true, true) => step_triple::<A, B, C, true, true>,
    }
}

pub(super) fn triple_step(a: &DInsn, b: &DInsn, c: &DInsn) -> Option<Step> {
    let ab = first_forward(a, b);
    let bc = first_forward(b, c);
    if ab && bc && a.rd == b.rd && b.rd == c.rd
        && (b.rs2 != a.rd || commute_for_forwarding(a, b))
        && (c.rs2 != b.rd || commute_for_forwarding(b, c))
    {
        let index = |insn: &DInsn| usize::from(OVERWRITE_TRIPLE_INDEX[insn.op as usize]);
        if let Some(step) = OVERWRITE_TRIPLES.get(index(a)).and_then(|row| row.get(index(b))).and_then(|row| row.get(index(c))) {
            return Some(*step);
        }
    }
    let (a, b, c) = (a.op, b.op, c.op);
    macro_rules! triple {
        ($($x:ident, $y:ident, $z:ident);* $(;)?) => {
            match (a, b, c) {
                $((Op::$x, Op::$y, Op::$z) => Some(triple_variant::<{ Op::$x as u8 }, { Op::$y as u8 }, { Op::$z as u8 }>(ab, bc)),)*
                _ => None,
            }
        };
    }
    triple!(
        // Mined from Alpine/apk/busybox/Python execution; each is a hot
        // three-instruction sequence, usually a pair plus one more.
        Slliw, Addw, Xor;
        Xor, Srliw, Slliw;
        Add, Add, Add;
        Addw, Srliw, Slliw;
        Srliw, Slliw, Addw;
        Ld, Ld, Ld;
        Sd, Sd, Sd;
        And, And, Xor;
        Xor, Addw, Addw;
        Xor, Xor, And;
        Xori, And, And;
        Addw, Slliw, Addw;
        And, Xor, Addw;
        Ld, Ld, Addi;
        Andi, Slli, Add;
        Slliw, Srliw, Addw;
        Slliw, Addw, Slliw;
        Add, Add, Bne;
        Addi, Sd, Sd;
        Srliw, Addw, Slliw;
        Xor, Xori, And;
        Addw, Addw, Add;
        Addi, Addw, Addw;
        Lw, Srliw, Addi;
        Addi, Addw, Lwu;
        Lwu, Addi, Addw;
        Addw, Lw, Srliw;
        Addw, Addw, Srliw;
        Jal, Addi, Sd;
        Addi, Jalr, Ld;
    )
    .or_else(|| generic_triple(a, b, c, usize::from(ab) | (usize::from(bc) << 1)))
}

/// Two results will be overwritten in this unit. Keep them in host
/// registers and materialize only the value visible at a possible fault.
#[inline(never)]
unsafe fn overwrite_triple<const A: u8, const B: u8, const C: u8>(
    chain: Chain, tlb: *mut FastTlb, h: &mut Hart, bus: &mut Bus,
    d: *const DInsn, fa: u64, fb: u64,
) -> u64 {
    let next = pre3(d);
    let Some(a) = try_fast_input::<A, false, false>(chain, tlb, h, d, 0) else {
        return STEPS[A as usize](chain, tlb, h, bus, d, fa, fb);
    };
    let second = d.add(1);
    let Some(b) = try_fast_input::<B, true, false>(chain, tlb, h, second, a) else {
        h.set_x((*d).rd, a);
        return STEPS[B as usize](chain, tlb, h, bus, second, a, fa);
    };
    let third = d.add(2);
    if let Some(c) = try_fast_input::<C, true, true>(chain, tlb, h, third, b) {
        return next(chain, tlb, h, bus, d.add(3), c, b);
    }
    h.set_x((*second).rd, b);
    STEPS[C as usize](chain, tlb, h, bus, third, b, a)
}

// Dense tables share one index per supported opcode. Each cell contains the
// dependency variants, so a decode miss selects a step with a data load
// instead of walking nested opcode switches.
const fn fusion_indices(ops: &[Op]) -> [u8; 128] {
    let mut index = [u8::MAX; 128];
    let mut i = 0;
    while i < ops.len() {
        index[ops[i] as usize] = i as u8;
        i += 1;
    }
    index
}

macro_rules! triple_steps {
    ($ops:tt) => { triple_steps!(@outer $ops; $ops) };
    (@outer [$($a:ident),*]; $ops:tt) => { [$(triple_steps!(@middle $a; $ops; $ops)),*] };
    (@middle $a:ident; [$($b:ident),*]; $ops:tt) => { [$(triple_steps!(@inner $a, $b; $ops)),*] };
    (@inner $a:ident, $b:ident; [$($c:ident),*]) => {
        [$([
            step_triple::<{ Op::$a as u8 }, { Op::$b as u8 }, { Op::$c as u8 }, false, false> as Step,
            step_triple::<{ Op::$a as u8 }, { Op::$b as u8 }, { Op::$c as u8 }, true, false> as Step,
            step_triple::<{ Op::$a as u8 }, { Op::$b as u8 }, { Op::$c as u8 }, false, true> as Step,
            step_triple::<{ Op::$a as u8 }, { Op::$b as u8 }, { Op::$c as u8 }, true, true> as Step,
        ]),*]
    };
}

macro_rules! define_triples {
    ([$($op:ident),*]) => {
        const TRIPLE_OPS: &[Op] = &[$(Op::$op),*];
        const TRIPLE_INDEX: [u8; 128] = fusion_indices(TRIPLE_OPS);
        const TRIPLE_N: usize = TRIPLE_OPS.len();
        static TRIPLE_STEPS: [[[[Step; 4]; TRIPLE_N]; TRIPLE_N]; TRIPLE_N] =
            triple_steps!([$($op),*]);
    };
}
define_triples!([Addi, Ld, Add, Sd, Lw, Slli, Andi, Lui, Addiw, Lbu, Sw, Addw, Auipc, Xor, Lhu]);

macro_rules! overwrite_triples {
    (@a [$($a:ident),*]; $ops:tt) => { [$(overwrite_triples!(@b $a; $ops; $ops)),*] };
    (@b $a:ident; [$($b:ident),*]; $ops:tt) => { [$(overwrite_triples!(@c $a, $b; $ops)),*] };
    (@c $a:ident, $b:ident; [$($c:ident),*]) => {
        [$(overwrite_triple::<{ Op::$a as u8 }, { Op::$b as u8 }, { Op::$c as u8 }> as Step),*]
    };
    ([$($op:ident),*]) => {
        const OVERWRITE_TRIPLE_OPS: &[Op] = &[$(Op::$op),*];
        const OVERWRITE_TRIPLE_INDEX: [u8; 128] = fusion_indices(OVERWRITE_TRIPLE_OPS);
        const OVERWRITE_TRIPLE_N: usize = OVERWRITE_TRIPLE_OPS.len();
        static OVERWRITE_TRIPLES: [[[Step; OVERWRITE_TRIPLE_N]; OVERWRITE_TRIPLE_N]; OVERWRITE_TRIPLE_N] =
            overwrite_triples!(@a [$($op),*]; [$($op),*]);
    };
}
overwrite_triples!([Addi, Ld, Add, Lw, Slli, Andi, Addiw, Lbu, Addw, Xor, Lhu]);

fn generic_triple(a: Op, b: Op, c: Op, dependency: usize) -> Option<Step> {
    let index = |op: Op| usize::from(TRIPLE_INDEX[op as usize]);
    Some(TRIPLE_STEPS.get(index(a))?.get(index(b))?.get(index(c))?[dependency])
}

/// The fused step for the pair of opcodes, if one is provided.
pub(super) fn pair_step(a: Op, b: Op, forward: bool) -> Option<Step> {
    macro_rules! pair {
        ($($x:ident, $y:ident);* $(;)?) => {
            match (a, b) {
                $((Op::$x, Op::$y) => Some(pair_variant::<{ Op::$x as u8 }, { Op::$y as u8 }>(forward)),)*
                _ => None,
            }
        };
    }
    pair!(
        Ld, Ld;
        Sd, Sd;
        Slliw, Addw;
        Addw, Addw;
        Srliw, Slliw;
        Addw, Xor;
        Addi, Sd;
        Add, Add;
        Ld, Addi;
        Xor, Srliw;
        Addi, Addi;
        Xor, Xor;
        Addw, Srliw;
        Xor, Addw;
        Slli, Add;
        Ld, Xor;
        Ld, Add;
        Sd, Addi;
        Addi, Bltu;
        Sd, Ld;
        Srli, Slli;
        Add, Ld;
        Xor, Ld;
        Slli, Srli;
        Lw, Addi;
        Add, Sd;
        Addi, Jalr;
        Ld, Sd;
        Sw, Addi;
        Ld, Beq;
        Srliw, Srliw;
        Sd, Add;
        And, And;
        Add, Bne;
        Addi, Add;
        Addi, Bne;
        Addi, Ld;
        Lw, Lw;
        Xor, And;
        Andi, Addi;
        Srliw, Addw;
        Addi, Addw;
        Auipc, Ld;
        Addi, Sw;
        Add, Srli;
        Addw, Ld;
        Ld, Jal;
        Sd, Lbu;
        Sd, Auipc;
        Lbu, Jalr;
        Lbu, Bne;
        Auipc, Lbu;
        Slliw, Slliw;
        Srli, Srli;
        Ld, Jalr;
        And, Xor;
        Add, Jal;
        Sub, Slli;
        // Mined from Alpine boot plus apk/busybox/Python execution.
        Lw, Srliw;
        Srliw, Addi;
        Slliw, Srliw;
        Xori, And;
        Andi, Slli;
        Xor, Lw;
        Lbu, Lbu;
        Add, Addi;
        Auipc, Addi;
        Sw, Sw;
        Add, Beq;
    )
    .or_else(|| generic_pair(a, b, forward))
}

macro_rules! pair_steps {
    ([$($a:ident),*]; $ops:tt) => { [$(pair_steps!(@row $a; $ops)),*] };
    (@row $a:ident; [$($b:ident),*]) => {
        [$([
            step_pair::<{ Op::$a as u8 }, { Op::$b as u8 }, false> as Step,
            step_pair::<{ Op::$a as u8 }, { Op::$b as u8 }, true> as Step,
        ]),*]
    };
}

macro_rules! define_pairs {
    ([$($op:ident),*]) => {
        const PAIR_OPS: &[Op] = &[$(Op::$op),*];
        const PAIR_INDEX: [u8; 128] = fusion_indices(PAIR_OPS);
        const PAIR_SECOND_INDEX: [u8; 128] = fusion_indices(&[$(Op::$op,)* Op::Jal, Op::Jalr]);
        const PAIR_N: usize = PAIR_OPS.len();
        static PAIR_STEPS: [[[Step; 2]; PAIR_N + 2]; PAIR_N] =
            pair_steps!([$($op),*]; [$($op,)* Jal, Jalr]);
        define_overwrites!([$($op),*]; [
            Add, Sub, And, Or, Xor, Addw, Subw,
            Addi, Andi, Ori, Xori, Slli, Srli, Srai, Addiw, Slti, Sltiu, Slliw, Srliw, Sraiw,
            Ld, Lw, Lwu, Lh, Lhu, Lb, Lbu
        ]);
    };
}
macro_rules! overwrite_steps {
    ([$($a:ident),*]; $ops:tt) => { [$(overwrite_steps!(@row $a; $ops)),*] };
    (@row $a:ident; [$($b:ident),*]) => {
        [$(step_overwrite::<{ Op::$a as u8 }, { Op::$b as u8 }> as Step),*]
    };
}

macro_rules! define_overwrites {
    ($a:tt; [$($b:ident),*]) => {
        const OVERWRITE_OPS: &[Op] = &[$(Op::$b),*];
        const OVERWRITE_INDEX: [u8; 128] = fusion_indices(OVERWRITE_OPS);
        static OVERWRITE_STEPS: [[Step; OVERWRITE_OPS.len()]; PAIR_N] =
            overwrite_steps!($a; [$($b),*]);
    };
}

define_pairs!([
    Add, Sub, And, Or, Xor, Sll, Srl, Sra, Slt, Sltu, Mul, Addw, Subw, Sllw, Srlw, Sraw, Mulw,
    Addi, Andi, Ori, Xori, Slli, Srli, Srai, Addiw, Slti, Sltiu, Slliw, Srliw, Sraiw, Lui, Auipc,
    Ld, Lw, Lwu, Lh, Lhu, Lb, Lbu, Sd, Sw, Sh, Sb
]);

pub(super) fn overwrite_pair(a: &DInsn, b: &DInsn) -> Option<Step> {
    if a.rd != b.rd || a.rd == 0 || a.rd != b.rs1 || b.rs2 == a.rd {
        return None;
    }
    let a = usize::from(PAIR_INDEX[a.op as usize]);
    let b = usize::from(OVERWRITE_INDEX[b.op as usize]);
    Some(*OVERWRITE_STEPS.get(a)?.get(b)?)
}

fn generic_pair(a: Op, b: Op, forward: bool) -> Option<Step> {
    let a = usize::from(PAIR_INDEX[a as usize]);
    let b = usize::from(PAIR_SECOND_INDEX[b as usize]);
    Some(PAIR_STEPS.get(a)?.get(b)?[usize::from(forward)])
}
