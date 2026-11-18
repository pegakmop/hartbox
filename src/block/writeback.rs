//! Memory runs with a single update to the address register.

use super::{host_read, host_write, pre3, pre4, pre5, pre6, pre7, pre8, tlb_entry, Chain, STEPS};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart, FTLB_SETS};
use crate::insn::{DInsn, Op, Step};

/// Group consecutive accesses and an increment on either end. The increment
/// and all accesses must be based on the same guest register. On a slow path,
/// the ordinary handlers still see each instruction in order.
pub(super) fn plan(ops: &[DInsn]) -> Option<(usize, Step)> {
    let [first, second, _, ..] = ops else {
        return None;
    };
    let leading = first.op == Op::Addi;
    let access = if leading { second } else { first };
    let (width, store) = match access.op {
        Op::Ld => (8, false),
        Op::Sd => (8, true),
        Op::Lw => (4, false),
        Op::Sw => (4, true),
        _ => return None,
    };
    let base = access.rs1;
    if base == 0 {
        return None;
    }
    let updates_base = |d: &DInsn| d.op == Op::Addi && d.rd == base && d.rs1 == base && d.imm != 0;
    if leading && !updates_base(first) {
        return None;
    }
    let accesses = ops[usize::from(leading)..].iter().take(7).take_while(|d| {
        d.op == access.op && d.rs1 == base && (store || (d.rd != 0 && d.rd != base))
    });
    let mut min = access.imm;
    let mut max = min;
    let mut count = 0;
    for d in accesses {
        if (d.imm ^ access.imm) & (width - 1) != 0 {
            break;
        }
        min = min.min(d.imm);
        max = max.max(d.imm);
        count += 1;
    }
    if count < 2
        || max - min + width > 256
        || (!leading && !ops.get(count).is_some_and(updates_base))
    {
        return None;
    }
    let len = count + 1;
    let bound = (4096 - width - (max - min)) / width;
    first.link.set((bound as usize) << 32 | min as u32 as usize);
    macro_rules! select {
        ($w:literal, $store:literal, $leading:literal) => {
            match len {
                3 => step::<3, $w, $store, $leading> as Step,
                4 => step::<4, $w, $store, $leading> as Step,
                5 => step::<5, $w, $store, $leading> as Step,
                6 => step::<6, $w, $store, $leading> as Step,
                7 => step::<7, $w, $store, $leading> as Step,
                _ => step::<8, $w, $store, $leading> as Step,
            }
        };
    }
    let step = match (width, store, leading) {
        (4, false, false) => select!(4, false, false),
        (4, false, true) => select!(4, false, true),
        (4, true, false) => select!(4, true, false),
        (4, true, true) => select!(4, true, true),
        (8, false, false) => select!(8, false, false),
        (8, false, true) => select!(8, false, true),
        (8, true, false) => select!(8, true, false),
        _ => select!(8, true, true),
    };
    Some((len, step))
}

#[inline(never)]
unsafe fn step<const N: usize, const W: usize, const STORE: bool, const LEADING: bool>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let next = match N {
        3 => pre3(d),
        4 => pre4(d),
        5 => pre5(d),
        6 => pre6(d),
        7 => pre7(d),
        _ => pre8(d),
    };
    let update = &*d.add(if LEADING { 0 } else { N - 1 });
    let old = h.rx_u(update.rs1);
    let adjusted = old.wrapping_add_signed(i64::from(update.imm));
    let base = if LEADING { adjusted } else { old };
    let metadata = (*d).link.get();
    let va = base.wrapping_add_signed(i64::from(metadata as u32 as i32));
    let table = if STORE { tlb.add(FTLB_SETS) } else { tlb };
    let entry = tlb_entry(table, va);
    let delta = va.wrapping_sub(entry.tag).rotate_right(W.trailing_zeros());
    if entry.tag == u64::MAX || delta > (metadata >> 32) as u64 {
        return STEPS[(*d).op as usize](chain, tlb, h, bus, d, fa, fb);
    }

    if LEADING {
        h.set_x(update.rd, adjusted);
    }
    let host_base = base.wrapping_add(entry.addend);
    let mut last = 0;
    let mut previous = 0;
    let accesses = d.add(usize::from(LEADING));
    for i in 0..N - 1 {
        let access = &*accesses.add(i);
        let host = host_base.wrapping_add_signed(i64::from(access.imm)) as usize;
        if STORE {
            host_write::<W>(host, h.rx_u(access.rs2));
        } else {
            let raw = host_read::<W>(host);
            let value = if W == 4 {
                raw as i32 as i64 as u64
            } else {
                raw
            };
            h.set_x(access.rd, value);
            previous = last;
            last = value;
        }
    }
    if !LEADING {
        h.set_x(update.rd, adjusted);
        return next(chain, tlb, h, bus, d.add(N), adjusted, last);
    }
    next(chain, tlb, h, bus, d.add(N), last, previous)
}
