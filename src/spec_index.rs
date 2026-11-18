//! The filter in front of the generated superinstruction table.
//!
//! A block build asks `spec::lookup` at every instruction whether a template starts there.
//! The templates are keyed by exact register tuples, but those tuples are common (`addi sp,
//! sp, -16`, `ld ra, 8(sp)`): a filter on the first instruction alone passed 52 % of the
//! positions of a boot, and each pass cost a binary search that chases a pointer per step
//! into the template table. Every template has at least three instructions, so the filter
//! here is on the (operation, registers) of the first three; 1 to 2 % of the positions pass
//! (the real matches are 4 % of the passes of the old filter), and the cold path of a block
//! build is a third shorter. It is built from `SPECS` on first use, so regenerating the
//! templates needs no change here.

use crate::insn::DInsn;
use crate::spec::{pack0, SPECS};
use std::sync::OnceLock;

/// Filter size in bits (a power of two).
const BITS: u32 = 17;

pub(crate) struct Index {
    /// `RVEMU_NO_SPEC=1`: run without any generated step, for differential testing against the
    /// ordinary interpreter.
    pub disabled: bool,
    filter: Box<[u64]>,
}

#[inline(always)]
fn hash3(a: u64, b: u64, c: u64) -> usize {
    let h = a.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F).rotate_left(21)
        ^ c.wrapping_mul(0x1656_67B1_9E37_79F9).rotate_left(43);
    (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - BITS)) as usize
}

fn build() -> Index {
    let mut filter = vec![0u64; 1 << (BITS - 6)];
    for s in SPECS.iter() {
        let w = s.words;
        if w.len() >= 6 {
            let h = hash3(w[0], w[2], w[4]);
            filter[h >> 6] |= 1u64 << (h & 63);
        }
    }
    Index {
        disabled: std::env::var("RVEMU_NO_SPEC").is_ok_and(|v| v != "0"),
        filter: filter.into_boxed_slice(),
    }
}

#[inline(always)]
pub(crate) fn index() -> &'static Index {
    static IX: OnceLock<Index> = OnceLock::new();
    IX.get_or_init(build)
}

impl Index {
    /// Whether a template may start at `d`. The caller has checked that three instructions
    /// are readable there.
    #[inline(always)]
    pub(crate) unsafe fn may_match(&self, d: *const DInsn) -> bool {
        let h = hash3(pack0(&*d), pack0(&*d.add(1)), pack0(&*d.add(2)));
        self.filter[h >> 6] >> (h & 63) & 1 != 0
    }
}
