//! Paired accesses for decoded runs with consecutive word or doubleword addresses.

use super::{
    extend, host_read, host_write, pre2, pre3, pre4, pre5, pre6, pre7, pre8, tlb_entry, Chain,
    STEPS,
};
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart, FTLB_SETS};
use crate::insn::{DInsn, Step};

pub(super) fn plan(ops: &[DInsn], width: usize, signed: bool, store: bool) -> Option<Step> {
    let stride = ops[1].imm - ops[0].imm;
    if stride.unsigned_abs() as usize != width
        || !ops
            .windows(2)
            .all(|pair| pair[1].imm - pair[0].imm == stride)
    {
        return None;
    }
    let len = ops.len();
    macro_rules! select {
        ($width:literal, $signed:literal, $store:literal, $reverse:literal) => {
            match len {
                2 => step::<2, $width, $signed, $store, $reverse> as Step,
                3 => step::<3, $width, $signed, $store, $reverse> as Step,
                4 => step::<4, $width, $signed, $store, $reverse> as Step,
                5 => step::<5, $width, $signed, $store, $reverse> as Step,
                6 => step::<6, $width, $signed, $store, $reverse> as Step,
                7 => step::<7, $width, $signed, $store, $reverse> as Step,
                _ => step::<8, $width, $signed, $store, $reverse> as Step,
            }
        };
        ($width:literal, $signed:literal, $store:literal) => {
            if stride < 0 {
                select!($width, $signed, $store, true)
            } else {
                select!($width, $signed, $store, false)
            }
        };
    }
    Some(match (width, signed, store) {
        (4, false, false) => select!(4, false, false),
        (4, true, false) => select!(4, true, false),
        (4, _, true) => select!(4, false, true),
        (8, _, false) => select!(8, false, false),
        (8, _, true) => select!(8, false, true),
        _ => unreachable!(),
    })
}

#[inline(never)]
unsafe fn step<
    const N: usize,
    const W: usize,
    const SIGNED: bool,
    const STORE: bool,
    const REVERSE: bool,
>(
    chain: Chain,
    tlb: *mut FastTlb,
    h: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    fa: u64,
    fb: u64,
) -> u64 {
    let next = match N {
        2 => pre2(d),
        3 => pre3(d),
        4 => pre4(d),
        5 => pre5(d),
        6 => pre6(d),
        7 => pre7(d),
        _ => pre8(d),
    };
    let first = &*d;
    let va = h
        .rx_u(first.rs1)
        .wrapping_add_signed(i64::from(first.link.get() as i32));
    let table = if STORE { tlb.add(FTLB_SETS) } else { tlb };
    let entry = tlb_entry(table, va);
    let delta = va.wrapping_sub(entry.tag).rotate_right(W.trailing_zeros());
    if entry.tag == u64::MAX || delta > (4096 / W - N) as u64 {
        return STEPS[first.op as usize](chain, tlb, h, bus, d, fa, fb);
    }

    // One checked RAM translation covers the whole run. The planner selects
    // this handler only when the guest is running on one host thread.
    let base = va.wrapping_add(entry.addend) as usize;
    let mut previous = 0;
    let mut last = 0;
    for i in (0..N - 1).step_by(2) {
        let host = base + W * if REVERSE { N - i - 2 } else { i };
        let a = &*d.add(i);
        let b = &*d.add(i + 1);
        if STORE {
            let (a, b) = (h.rx_u(a.rs2), h.rx_u(b.rs2));
            let (lo, hi) = if REVERSE { (b, a) } else { (a, b) };
            store_pair::<W>(host, lo, hi);
        } else {
            let (lo, hi) = load_pair::<W, SIGNED>(host);
            (previous, last) = if REVERSE { (hi, lo) } else { (lo, hi) };
            h.set_x(a.rd, previous);
            h.set_x(b.rd, last);
        }
    }
    if N % 2 != 0 {
        let host = base + if REVERSE { 0 } else { (N - 1) * W };
        let tail = &*d.add(N - 1);
        if STORE {
            host_write::<W>(host, h.rx_u(tail.rs2));
        } else {
            previous = last;
            last = extend::<W, SIGNED>(host_read::<W>(host));
            h.set_x(tail.rd, last);
        }
    }
    next(chain, tlb, h, bus, d.add(N), last, previous)
}

#[inline(always)]
unsafe fn load_pair<const W: usize, const SIGNED: bool>(host: usize) -> (u64, u64) {
    #[cfg(target_arch = "aarch64")]
    {
        let lo: u64;
        let hi: u64;
        macro_rules! load {
            ($instruction:literal, $lane:literal) => {
                core::arch::asm!(concat!($instruction, " {lo", $lane, "}, {hi", $lane, "}, [{host}]"),
                    lo = lateout(reg) lo, hi = lateout(reg) hi, host = in(reg) host,
                    options(readonly, nostack, preserves_flags),
                )
            };
        }
        match (W, SIGNED) {
            (8, _) => load!("ldp", ""),
            (4, false) => load!("ldp", ":w"),
            _ => load!("ldpsw", ""),
        }
        (lo, hi)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        (
            extend::<W, SIGNED>(host_read::<W>(host)),
            extend::<W, SIGNED>(host_read::<W>(host + W)),
        )
    }
}

#[inline(always)]
unsafe fn store_pair<const W: usize>(host: usize, lo: u64, hi: u64) {
    #[cfg(target_arch = "aarch64")]
    {
        macro_rules! store {
            ($lane:literal) => {
                core::arch::asm!(concat!("stp {lo", $lane, "}, {hi", $lane, "}, [{host}]"),
                    lo = in(reg) lo, hi = in(reg) hi, host = in(reg) host,
                    options(nostack, preserves_flags),
                )
            };
        }
        if W == 8 {
            store!("");
        } else {
            store!(":w");
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        host_write::<W>(host, lo);
        host_write::<W>(host + W, hi);
    }
}
