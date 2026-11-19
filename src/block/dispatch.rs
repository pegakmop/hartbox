//! Prepare the next threaded dispatch while the current handler is running.

use super::Chain;
use crate::bus::Bus;
use crate::hart::{FastTlb, Hart};
use crate::insn::{DInsn, Step};

pub(super) const PRELOAD_OPERANDS: bool = cfg!(all(
    target_arch = "aarch64",
    target_endian = "little",
    not(feature = "counters")
));

pub(crate) struct Dispatch {
    step: Step,
    instruction: *const DInsn,
    operands: u64,
}

impl Dispatch {
    #[inline(always)]
    pub(crate) unsafe fn run(
        self,
        chain: Chain,
        tlb: *mut FastTlb,
        hart: &mut Hart,
        bus: &mut Bus,
        preceding: u64,
        second_preceding: u64,
    ) -> u64 {
        let last = if PRELOAD_OPERANDS {
            self.operands
        } else {
            second_preceding
        };
        (self.step)(chain, tlb, hart, bus, self.instruction, preceding, last)
    }
}

/// A fallback into a scalar step or the first dispatch from a linked edge.
#[inline(always)]
pub(crate) unsafe fn invoke(
    step: Step,
    chain: Chain,
    tlb: *mut FastTlb,
    hart: &mut Hart,
    bus: &mut Bus,
    d: *const DInsn,
    preceding: u64,
    second_preceding: u64,
) -> u64 {
    let operands = if PRELOAD_OPERANDS {
        d.cast::<u64>().read()
    } else {
        0
    };
    Dispatch {
        step,
        instruction: d,
        operands,
    }
    .run(chain, tlb, hart, bus, preceding, second_preceding)
}

/// The native dispatch carries these fields in the last argument register.
/// Other targets and counter builds use that register for a second result.
#[inline(always)]
pub(super) unsafe fn operands(d: *const DInsn, packed: u64) -> (u8, u8, u8, i32) {
    if PRELOAD_OPERANDS {
        (
            packed as u8,
            (packed >> 8) as u8,
            (packed >> 16) as u8,
            (packed >> 32) as i32,
        )
    } else {
        let d = &*d;
        (d.rd, d.rs1, d.rs2, d.imm)
    }
}

/// Load the next handler at the top of each step to overlap the branch target
/// load with the work in the current instruction. On ARM64, the same pair
/// load advances the instruction pointer and brings in the operands of the
/// next handler before that handler begins.
macro_rules! pre_fn {
    ($name:ident, $count:literal) => {
        #[inline(always)]
        pub(crate) unsafe fn $name(d: *const DInsn) -> Dispatch {
            #[cfg(all(target_arch = "aarch64", target_endian = "little", not(feature = "counters")))]
            {
                use std::mem::{offset_of, size_of};
                const _: () = {
                    assert!(offset_of!(DInsn, rd) == 0);
                    assert!(offset_of!(DInsn, rs1) == 1);
                    assert!(offset_of!(DInsn, rs2) == 2);
                    assert!(offset_of!(DInsn, imm) == 4);
                    assert!(offset_of!(DInsn, step) == 8);
                    assert!(size_of::<DInsn>() == 32 && std::mem::align_of::<DInsn>() >= 8);
                };
                let mut instruction = d;
                let operands: u64;
                let step: usize;
                core::arch::asm!(
                    "ldp {operands}, {step}, [{instruction}, #{offset}]!",
                    instruction = inout(reg) instruction,
                    operands = out(reg) operands,
                    step = out(reg) step,
                    offset = const $count * size_of::<DInsn>(),
                    options(readonly, nostack, preserves_flags),
                );
                Dispatch { step: std::mem::transmute::<usize, Step>(step), instruction, operands }
            }
            #[cfg(not(all(target_arch = "aarch64", target_endian = "little", not(feature = "counters"))))]
            {
                let instruction = d.add($count);
                #[cfg(feature = "counters")]
                let step = super::go_tlb as Step;
                #[cfg(not(feature = "counters"))]
                let step = (*instruction).step.unwrap_unchecked();
                Dispatch { step, instruction, operands: 0 }
            }
        }
    };
}

pre_fn!(pre1, 1);
pre_fn!(pre2, 2);
pre_fn!(pre3, 3);
pre_fn!(pre4, 4);
pre_fn!(pre5, 5);
pre_fn!(pre6, 6);
pre_fn!(pre7, 7);
pre_fn!(pre8, 8);
