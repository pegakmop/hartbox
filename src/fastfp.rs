//! Native floating point for the block engine.
//!
//! Every F and D instruction used to run through `step_generic`: a non-tail call into the
//! interpreter's big `match`, then the software rounding code of `fpu.rs` (a residual per
//! operation to find the inexact flag and to honour directed rounding). That made a guest
//! floating-point instruction about ten times slower than an integer one.
//!
//! The steps here run the common cases on the host's FPU, with the same dispatch as the
//! integer steps:
//!
//! * the rounding mode is round-to-nearest-even (the instruction's own `rm`, or `dyn` with
//!   `frm` = RNE), which is what the host unit does; conversions to integer use the host
//!   instruction for each of the five RISC-V modes;
//! * `mstatus.FS` is Dirty for instructions that write the FP state (so nothing has to be
//!   updated), or not Off for the ones that only read it;
//! * no operand or result is a NaN. A NaN needs the RISC-V rules (canonical NaN results,
//!   `fmin` and `fmax` semantics, the invalid flag), and a single-precision operand has to be
//!   properly NaN-boxed. Anything unusual is handed to `step_generic`, which runs the whole
//!   instruction again in the interpreter; nothing was written before the decision.
//!
//! The exception flags are the host's own: ARM's IOC/DZC/OFC/UFC/IXC accrue in `FPSR` and
//! have the same meaning as RISC-V's NV/DZ/OF/UF/NX for these operations, so a step does not
//! touch them at all. `native_flags` turns `FPSR` into `fflags` bits; `Hart::run` folds them
//! into `Hart::fflags` (and clears `FPSR`) when a slice ends, and the CSR code does the same
//! when the guest reads or writes `fflags`/`fcsr`.

#[cfg(target_arch = "aarch64")]
pub(crate) use native::*;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) use portable::*;

#[cfg(not(target_arch = "aarch64"))]
mod portable {
    use crate::insn::{DInsn, Step};

    #[inline(always)]
    pub(crate) fn native_flags() -> u64 {
        0
    }

    #[inline(always)]
    pub(crate) fn clear_native_flags() {}

    pub(crate) fn fast_step(_d: &DInsn) -> Option<Step> {
        None
    }
}

#[cfg(target_arch = "aarch64")]
mod native {
    use crate::arch::MSTATUS_FS;
    use crate::block::{pre1, step_generic, tlb_load, tlb_store, Chain};
    use crate::bus::Bus;
    use crate::hart::Hart;
    use crate::insn::{DInsn, Op, Step};

    /// The host unit's accrued exception flags as RISC-V `fflags` bits (NV 16, DZ 8, OF 4,
    /// UF 2, NX 1; ARM has them in the opposite order in bits 0 to 4 of `FPSR`).
    #[inline(always)]
    pub(crate) fn native_flags() -> u64 {
        let v: u64;
        // SAFETY: reads a register of the running thread.
        unsafe { core::arch::asm!("mrs {0}, fpsr", out(reg) v, options(nomem, nostack, preserves_flags)) };
        ((v & 1) << 4) | ((v & 2) << 2) | (v & 4) | ((v & 8) >> 2) | ((v >> 4) & 1)
    }

    #[inline(always)]
    pub(crate) fn clear_native_flags() {
        // SAFETY: clears the accrued flags of the running thread; nothing else reads them.
        unsafe { core::arch::asm!("msr fpsr, xzr", options(nomem, nostack)) };
    }

    #[cfg(test)]
    thread_local! {
        pub(crate) static FALLBACKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Hand the instruction to the interpreter, which runs all of it.
    #[inline(always)]
    unsafe fn slow(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        #[cfg(test)]
        FALLBACKS.set(FALLBACKS.get() + 1);
        step_generic(chain, tlb, h, bus, d, fa, fb)
    }

    /// `mstatus.FS` is Dirty: the FP state can be written without marking it.
    #[inline(always)]
    fn fs_dirty(h: &Hart) -> bool {
        h.mstatus & MSTATUS_FS == MSTATUS_FS
    }

    /// `mstatus.FS` is not Off: the FP state can be read.
    #[inline(always)]
    fn fs_on(h: &Hart) -> bool {
        h.mstatus & MSTATUS_FS != 0
    }

    /// The instruction rounds to nearest, ties to even: its `rm` field says so, or says
    /// `dyn` and `frm` does.
    #[inline(always)]
    fn rne(h: &Hart, rmf: u8) -> bool {
        rmf == 0 || (rmf == 7 && h.frm == 0)
    }

    #[inline(always)]
    fn fr(h: &Hart, r: u8) -> u64 {
        h.f[(r & 31) as usize]
    }

    #[inline(always)]
    fn set_fr(h: &mut Hart, r: u8, v: u64) {
        h.f[(r & 31) as usize] = v;
    }

    /// A single-precision register value is properly NaN-boxed.
    #[inline(always)]
    fn boxed(v: u64) -> bool {
        v >> 32 == 0xffff_ffff
    }

    #[inline(always)]
    fn s_of(v: u64) -> f32 {
        f32::from_bits(v as u32)
    }

    #[inline(always)]
    fn box_s(bits: u32) -> u64 {
        0xffff_ffff_0000_0000 | bits as u64
    }

    /// `v` as `fsgnj` and the like read it: improperly boxed means the canonical NaN.
    #[inline(always)]
    fn unbox_s(v: u64) -> u32 {
        if boxed(v) {
            v as u32
        } else {
            0x7fc0_0000
        }
    }

    // ------------------------------------------------------------ host instructions
    //
    // Every operation that can raise a flag is an `asm!` block, not a Rust operator. The
    // compiler treats `a + b` as pure and may compute it before the test that decides
    // whether the step takes the fast path at all (an improperly boxed operand is any
    // bit pattern, a signalling NaN among them), and the flag it raised would stay in
    // `FPSR` for a result nobody asked for. An `asm!` without `pure` is never speculated.

    macro_rules! asm_bin {
        ($name:ident, $ins:literal, $t:ty, $m:literal) => {
            #[inline(always)]
            fn $name(a: $t, b: $t) -> $t {
                let r: $t;
                // SAFETY: a register-to-register operation.
                unsafe {
                    core::arch::asm!(concat!($ins, " {0:", $m, "}, {1:", $m, "}, {2:", $m, "}"),
                        out(vreg) r, in(vreg) a, in(vreg) b, options(nomem, nostack));
                }
                r
            }
        };
    }
    asm_bin!(fadd_d, "fadd", f64, "d");
    asm_bin!(fsub_d, "fsub", f64, "d");
    asm_bin!(fmul_d, "fmul", f64, "d");
    asm_bin!(fdiv_d, "fdiv", f64, "d");
    asm_bin!(fadd_s, "fadd", f32, "s");
    asm_bin!(fsub_s, "fsub", f32, "s");
    asm_bin!(fmul_s, "fmul", f32, "s");
    asm_bin!(fdiv_s, "fdiv", f32, "s");

    macro_rules! asm_un {
        ($name:ident, $ins:literal, $from:ty, $fm:literal, $to:ty, $tm:literal) => {
            #[inline(always)]
            fn $name(a: $from) -> $to {
                let r: $to;
                // SAFETY: a register-to-register operation.
                unsafe {
                    core::arch::asm!(concat!($ins, " {0:", $tm, "}, {1:", $fm, "}"),
                        out(vreg) r, in(vreg) a, options(nomem, nostack));
                }
                r
            }
        };
    }
    asm_un!(fsqrt_d, "fsqrt", f64, "d", f64, "d");
    asm_un!(fsqrt_s, "fsqrt", f32, "s", f32, "s");
    asm_un!(cvt_d_of_s, "fcvt", f32, "s", f64, "d");
    asm_un!(cvt_s_of_d, "fcvt", f64, "d", f32, "s");

    macro_rules! asm_fma {
        ($name:ident, $ins:literal, $t:ty, $m:literal) => {
            #[inline(always)]
            fn $name(a: $t, b: $t, c: $t) -> $t {
                let r: $t;
                // SAFETY: a register-to-register operation.
                unsafe {
                    core::arch::asm!(concat!($ins, " {0:", $m, "}, {1:", $m, "}, {2:", $m, "}, {3:", $m, "}"),
                        out(vreg) r, in(vreg) a, in(vreg) b, in(vreg) c, options(nomem, nostack));
                }
                r
            }
        };
    }
    // The host names them by the sign of the addend first: RISC-V fmsub (a*b - c) is
    // `fnmsub`, RISC-V fnmsub (-(a*b) + c) is `fmsub`.
    asm_fma!(fmadd_d, "fmadd", f64, "d");
    asm_fma!(fmsub_d, "fnmsub", f64, "d");
    asm_fma!(fnmsub_d, "fmsub", f64, "d");
    asm_fma!(fnmadd_d, "fnmadd", f64, "d");
    asm_fma!(fmadd_s, "fmadd", f32, "s");
    asm_fma!(fmsub_s, "fnmsub", f32, "s");
    asm_fma!(fnmsub_s, "fmsub", f32, "s");
    asm_fma!(fnmadd_s, "fnmadd", f32, "s");

    macro_rules! asm_i2f {
        ($name:ident, $ins:literal, $tm:literal, $im:literal, $t:ty) => {
            #[inline(always)]
            fn $name(x: u64) -> $t {
                let r: $t;
                // SAFETY: a register-to-register operation.
                unsafe {
                    core::arch::asm!(concat!($ins, " {0:", $tm, "}, {1:", $im, "}"),
                        out(vreg) r, in(reg) x, options(nomem, nostack));
                }
                r
            }
        };
    }
    asm_i2f!(scvtf_dl, "scvtf", "d", "x", f64);
    asm_i2f!(scvtf_dw, "scvtf", "d", "w", f64);
    asm_i2f!(ucvtf_dl, "ucvtf", "d", "x", f64);
    asm_i2f!(ucvtf_dw, "ucvtf", "d", "w", f64);
    asm_i2f!(scvtf_sl, "scvtf", "s", "x", f32);
    asm_i2f!(scvtf_sw, "scvtf", "s", "w", f32);
    asm_i2f!(ucvtf_sl, "ucvtf", "s", "x", f32);
    asm_i2f!(ucvtf_sw, "ucvtf", "s", "w", f32);

    // ------------------------------------------------------------ loads and stores

    /// `flw` / `fld`.
    #[inline(never)]
    pub(crate) unsafe fn step_fload<const N: usize>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) {
            let va = h.rx_u(di.rs1).wrapping_add(di.imm as u64);
            if let Some(v) = tlb_load::<N>(tlb, h, va) {
                set_fr(h, di.rd, if N == 4 { box_s(v as u32) } else { v });
                return nx(chain, tlb, h, bus, d.add(1), fa, fa);
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fsw` / `fsd`.
    #[inline(never)]
    pub(crate) unsafe fn step_fstore<const N: usize>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_on(h) {
            let va = h.rx_u(di.rs1).wrapping_add(di.imm as u64);
            let v = fr(h, di.rs2);
            if tlb_store::<N>(tlb, h, va, if N == 4 { v as u32 as u64 } else { v }) {
                return nx(chain, tlb, h, bus, d.add(1), fa, fa);
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    // ------------------------------------------------------------ arithmetic

    #[inline(always)]
    fn bin_d<const OP: u8>(a: f64, b: f64) -> f64 {
        match OP {
            0 => fadd_d(a, b),
            1 => fsub_d(a, b),
            2 => fmul_d(a, b),
            _ => fdiv_d(a, b),
        }
    }

    #[inline(always)]
    fn bin_s<const OP: u8>(a: f32, b: f32) -> f32 {
        match OP {
            0 => fadd_s(a, b),
            1 => fsub_s(a, b),
            2 => fmul_s(a, b),
            _ => fdiv_s(a, b),
        }
    }

    /// `fadd`, `fsub`, `fmul`, `fdiv` (`OP` is `funct5`: 0 to 3).
    #[inline(never)]
    pub(crate) unsafe fn step_fbin<const OP: u8, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) && rne(h, di.f) {
            let (a, b) = (fr(h, di.rs1), fr(h, di.rs2));
            if DBL {
                let r = bin_d::<OP>(f64::from_bits(a), f64::from_bits(b));
                if !r.is_nan() {
                    set_fr(h, di.rd, r.to_bits());
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            } else if boxed(a) && boxed(b) {
                let r = bin_s::<OP>(s_of(a), s_of(b));
                if !r.is_nan() {
                    set_fr(h, di.rd, box_s(r.to_bits()));
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fsqrt`.
    #[inline(never)]
    pub(crate) unsafe fn step_fsqrt<const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) && rne(h, di.f) {
            let a = fr(h, di.rs1);
            if DBL {
                let r = fsqrt_d(f64::from_bits(a));
                if !r.is_nan() {
                    set_fr(h, di.rd, r.to_bits());
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            } else if boxed(a) {
                let r = fsqrt_s(s_of(a));
                if !r.is_nan() {
                    set_fr(h, di.rd, box_s(r.to_bits()));
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fmadd` (0), `fmsub` (1), `fnmsub` (2), `fnmadd` (3): one rounding, on the host's
    /// fused multiply-add.
    #[inline(never)]
    pub(crate) unsafe fn step_fma<const KIND: u8, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) && rne(h, di.f) {
            let (a, b, c) = (fr(h, di.rs1), fr(h, di.rs2), fr(h, di.rs3));
            if DBL {
                let (x, y, z) = (f64::from_bits(a), f64::from_bits(b), f64::from_bits(c));
                let r = match KIND {
                    0 => fmadd_d(x, y, z),
                    1 => fmsub_d(x, y, z),
                    2 => fnmsub_d(x, y, z),
                    _ => fnmadd_d(x, y, z),
                };
                if !r.is_nan() {
                    set_fr(h, di.rd, r.to_bits());
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            } else if boxed(a) && boxed(b) && boxed(c) {
                let (x, y, z) = (s_of(a), s_of(b), s_of(c));
                let r = match KIND {
                    0 => fmadd_s(x, y, z),
                    1 => fmsub_s(x, y, z),
                    2 => fnmsub_s(x, y, z),
                    _ => fnmadd_s(x, y, z),
                };
                if !r.is_nan() {
                    set_fr(h, di.rd, box_s(r.to_bits()));
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    // ------------------------------------------------------------ sign, min/max, compare

    /// `fsgnj` (0), `fsgnjn` (1), `fsgnjx` (2): bit operations, no rounding.
    #[inline(never)]
    pub(crate) unsafe fn step_fsgnj<const KIND: u8, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) {
            let (a, b) = (fr(h, di.rs1), fr(h, di.rs2));
            let r = if DBL {
                const S: u64 = 1 << 63;
                let sign = match KIND {
                    0 => b & S,
                    1 => !b & S,
                    _ => (a ^ b) & S,
                };
                (a & !S) | sign
            } else {
                const S: u32 = 1 << 31;
                let (a, b) = (unbox_s(a), unbox_s(b));
                let sign = match KIND {
                    0 => b & S,
                    1 => !b & S,
                    _ => (a ^ b) & S,
                };
                box_s((a & !S) | sign)
            };
            set_fr(h, di.rd, r);
            return nx(chain, tlb, h, bus, d.add(1), fa, fa);
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fmin` (0) and `fmax` (1) when neither operand is a NaN.
    #[inline(never)]
    pub(crate) unsafe fn step_fminmax<const MAX: bool, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) {
            let (a, b) = (fr(h, di.rs1), fr(h, di.rs2));
            if DBL {
                let (x, y) = (f64::from_bits(a), f64::from_bits(b));
                if !x.is_nan() && !y.is_nan() {
                    // Equal operands are equal bits, except that -0 and +0 compare equal:
                    // min takes the negative one (the sign bits OR), max the positive one.
                    let r = if x < y {
                        if MAX {
                            b
                        } else {
                            a
                        }
                    } else if y < x {
                        if MAX {
                            a
                        } else {
                            b
                        }
                    } else if MAX {
                        a & b
                    } else {
                        a | b
                    };
                    set_fr(h, di.rd, r);
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            } else if boxed(a) && boxed(b) {
                let (x, y) = (cvt_d_of_s(s_of(a)), cvt_d_of_s(s_of(b)));
                if !x.is_nan() && !y.is_nan() {
                    let (a, b) = (a as u32, b as u32);
                    let r = if x < y {
                        if MAX {
                            b
                        } else {
                            a
                        }
                    } else if y < x {
                        if MAX {
                            a
                        } else {
                            b
                        }
                    } else if MAX {
                        a & b
                    } else {
                        a | b
                    };
                    set_fr(h, di.rd, box_s(r));
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fle` (0), `flt` (1), `feq` (2) when neither operand is a NaN.
    #[inline(never)]
    pub(crate) unsafe fn step_fcmp<const KIND: u8, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_on(h) {
            let (a, b) = (fr(h, di.rs1), fr(h, di.rs2));
            let (x, y, ok) = if DBL {
                (f64::from_bits(a), f64::from_bits(b), true)
            } else if boxed(a) && boxed(b) {
                (cvt_d_of_s(s_of(a)), cvt_d_of_s(s_of(b)), true)
            } else {
                (0.0, 0.0, false)
            };
            if ok && !x.is_nan() && !y.is_nan() {
                let r = match KIND {
                    0 => x <= y,
                    1 => x < y,
                    _ => x == y,
                };
                h.wx_u(di.rd, r as u64);
                return nx(chain, tlb, h, bus, d.add(1), fa, fa);
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    // ------------------------------------------------------------ conversions

    macro_rules! cvt_asm {
        ($name:ident, $ins:literal) => {
            /// The host conversion `$ins` (saturating, flags in `FPSR`).
            #[inline(always)]
            fn $name<const LONG: bool>(x: f64) -> u64 {
                let r: u64;
                // SAFETY: a register-to-register conversion.
                unsafe {
                    if LONG {
                        core::arch::asm!(concat!($ins, " {0}, {1:d}"), out(reg) r, in(vreg) x, options(nomem, nostack));
                    } else {
                        core::arch::asm!(concat!($ins, " {0:w}, {1:d}"), out(reg) r, in(vreg) x, options(nomem, nostack));
                    }
                }
                r
            }
        };
    }
    cvt_asm!(fcvtns, "fcvtns");
    cvt_asm!(fcvtnu, "fcvtnu");
    cvt_asm!(fcvtzs, "fcvtzs");
    cvt_asm!(fcvtzu, "fcvtzu");
    cvt_asm!(fcvtms, "fcvtms");
    cvt_asm!(fcvtmu, "fcvtmu");
    cvt_asm!(fcvtps, "fcvtps");
    cvt_asm!(fcvtpu, "fcvtpu");
    cvt_asm!(fcvtas, "fcvtas");
    cvt_asm!(fcvtau, "fcvtau");

    /// `fcvt.w.*`, `fcvt.wu.*`, `fcvt.l.*`, `fcvt.lu.*` for a source that is not a NaN, in the
    /// instruction's rounding mode. The host conversions saturate as RISC-V's do.
    #[inline(never)]
    pub(crate) unsafe fn step_fcvt_f2i<const SIGNED: bool, const LONG: bool, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        let rm = if di.f == 7 { h.frm as u8 } else { di.f };
        if fs_on(h) && rm <= 4 {
            let a = fr(h, di.rs1);
            let (x, ok) = if DBL {
                (f64::from_bits(a), true)
            } else if boxed(a) {
                (cvt_d_of_s(s_of(a)), true)
            } else {
                (0.0, false)
            };
            if ok && !x.is_nan() {
                let r = if SIGNED {
                    match rm {
                        0 => fcvtns::<LONG>(x),
                        1 => fcvtzs::<LONG>(x),
                        2 => fcvtms::<LONG>(x),
                        3 => fcvtps::<LONG>(x),
                        _ => fcvtas::<LONG>(x),
                    }
                } else {
                    match rm {
                        0 => fcvtnu::<LONG>(x),
                        1 => fcvtzu::<LONG>(x),
                        2 => fcvtmu::<LONG>(x),
                        3 => fcvtpu::<LONG>(x),
                        _ => fcvtau::<LONG>(x),
                    }
                };
                let r = if LONG { r } else { r as u32 as i32 as i64 as u64 };
                h.wx_u(di.rd, r);
                return nx(chain, tlb, h, bus, d.add(1), fa, fa);
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fcvt.s.w`, `fcvt.d.l` and the other integer to FP conversions, rounding to nearest.
    #[inline(never)]
    pub(crate) unsafe fn step_fcvt_i2f<const SIGNED: bool, const LONG: bool, const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, 
        h: &mut Hart,
        bus: &mut Bus,
        d: *const DInsn,
        fa: u64,
        fb: u64,
    ) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) && rne(h, di.f) {
            let x = h.rx_u(di.rs1);
            let r = match (SIGNED, LONG, DBL) {
                (true, true, true) => scvtf_dl(x).to_bits(),
                (true, false, true) => scvtf_dw(x).to_bits(),
                (false, true, true) => ucvtf_dl(x).to_bits(),
                (false, false, true) => ucvtf_dw(x).to_bits(),
                (true, true, false) => box_s(scvtf_sl(x).to_bits()),
                (true, false, false) => box_s(scvtf_sw(x).to_bits()),
                (false, true, false) => box_s(ucvtf_sl(x).to_bits()),
                (false, false, false) => box_s(ucvtf_sw(x).to_bits()),
            };
            set_fr(h, di.rd, r);
            return nx(chain, tlb, h, bus, d.add(1), fa, fa);
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fcvt.s.d`.
    #[inline(never)]
    pub(crate) unsafe fn step_fcvt_s_d(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) && rne(h, di.f) {
            let x = f64::from_bits(fr(h, di.rs1));
            if !x.is_nan() {
                set_fr(h, di.rd, box_s(cvt_s_of_d(x).to_bits()));
                return nx(chain, tlb, h, bus, d.add(1), fa, fa);
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fcvt.d.s`.
    #[inline(never)]
    pub(crate) unsafe fn step_fcvt_d_s(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) && rne(h, di.f) {
            let a = fr(h, di.rs1);
            if boxed(a) {
                let x = cvt_d_of_s(s_of(a));
                if !x.is_nan() {
                    set_fr(h, di.rd, x.to_bits());
                    return nx(chain, tlb, h, bus, d.add(1), fa, fa);
                }
            }
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fmv.x.w` / `fmv.x.d`.
    #[inline(never)]
    pub(crate) unsafe fn step_fmv_x<const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_on(h) {
            let a = fr(h, di.rs1);
            h.wx_u(di.rd, if DBL { a } else { unbox_s(a) as i32 as i64 as u64 });
            return nx(chain, tlb, h, bus, d.add(1), fa, fa);
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    /// `fmv.w.x` / `fmv.d.x`.
    #[inline(never)]
    pub(crate) unsafe fn step_fmv_f<const DBL: bool>(chain: Chain, tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, fa: u64, fb: u64) -> u64 {
        let nx = pre1(d);
        let di = &*d;
        if fs_dirty(h) {
            let x = h.rx_u(di.rs1);
            set_fr(h, di.rd, if DBL { x } else { box_s(x as u32) });
            return nx(chain, tlb, h, bus, d.add(1), fa, fa);
        }
        slow(chain, tlb, h, bus, d, fa, fb)
    }

    // ------------------------------------------------------------ selection

    macro_rules! both {
        ($f:ident, [$($c:expr),*], $dbl:expr) => {
            if $dbl { Some($f::<$($c,)* true> as Step) } else { Some($f::<$($c,)* false> as Step) }
        };
    }

    /// The native step for a floating-point instruction, if it has one. Whatever the step
    /// cannot do at run time it hands to `step_generic`, so this only has to rule out the
    /// encodings the interpreter treats as illegal or as another format.
    pub(crate) fn fast_step(d: &DInsn) -> Option<Step> {
        match d.op {
            Op::Flw => Some(step_fload::<4> as Step),
            Op::Fld => Some(step_fload::<8> as Step),
            Op::Fsw => Some(step_fstore::<4> as Step),
            Op::Fsd => Some(step_fstore::<8> as Step),
            Op::Fmadd | Op::Fmsub | Op::Fnmsub | Op::Fnmadd => {
                if d.imm != 0 && d.imm != 1 {
                    return None;
                }
                let dbl = d.imm == 1;
                match d.op {
                    Op::Fmadd => both!(step_fma, [0], dbl),
                    Op::Fmsub => both!(step_fma, [1], dbl),
                    Op::Fnmsub => both!(step_fma, [2], dbl),
                    _ => both!(step_fma, [3], dbl),
                }
            }
            Op::Fp => {
                let funct5 = (d.imm & 31) as u8;
                let fmt = (d.imm >> 5) & 3;
                if fmt > 1 {
                    return None;
                }
                let dbl = fmt == 1;
                let (f3, rs2) = (d.f, d.rs2);
                match funct5 {
                    0x00 => both!(step_fbin, [0], dbl),
                    0x01 => both!(step_fbin, [1], dbl),
                    0x02 => both!(step_fbin, [2], dbl),
                    0x03 => both!(step_fbin, [3], dbl),
                    0x0b if rs2 == 0 => both!(step_fsqrt, [], dbl),
                    0x04 if f3 == 0 => both!(step_fsgnj, [0], dbl),
                    0x04 if f3 == 1 => both!(step_fsgnj, [1], dbl),
                    0x04 if f3 == 2 => both!(step_fsgnj, [2], dbl),
                    0x05 if f3 == 0 => both!(step_fminmax, [false], dbl),
                    0x05 if f3 == 1 => both!(step_fminmax, [true], dbl),
                    0x08 if !dbl && rs2 == 1 => Some(step_fcvt_s_d as Step),
                    0x08 if dbl && rs2 == 0 => Some(step_fcvt_d_s as Step),
                    0x14 if f3 == 0 => both!(step_fcmp, [0], dbl),
                    0x14 if f3 == 1 => both!(step_fcmp, [1], dbl),
                    0x14 if f3 == 2 => both!(step_fcmp, [2], dbl),
                    0x18 => match (rs2, dbl) {
                        (0, true) => Some(step_fcvt_f2i::<true, false, true> as Step),
                        (0, false) => Some(step_fcvt_f2i::<true, false, false> as Step),
                        (1, true) => Some(step_fcvt_f2i::<false, false, true> as Step),
                        (1, false) => Some(step_fcvt_f2i::<false, false, false> as Step),
                        (2, true) => Some(step_fcvt_f2i::<true, true, true> as Step),
                        (2, false) => Some(step_fcvt_f2i::<true, true, false> as Step),
                        (3, true) => Some(step_fcvt_f2i::<false, true, true> as Step),
                        (3, false) => Some(step_fcvt_f2i::<false, true, false> as Step),
                        _ => None,
                    },
                    0x1a => match (rs2, dbl) {
                        (0, true) => Some(step_fcvt_i2f::<true, false, true> as Step),
                        (0, false) => Some(step_fcvt_i2f::<true, false, false> as Step),
                        (1, true) => Some(step_fcvt_i2f::<false, false, true> as Step),
                        (1, false) => Some(step_fcvt_i2f::<false, false, false> as Step),
                        (2, true) => Some(step_fcvt_i2f::<true, true, true> as Step),
                        (2, false) => Some(step_fcvt_i2f::<true, true, false> as Step),
                        (3, true) => Some(step_fcvt_i2f::<false, true, true> as Step),
                        (3, false) => Some(step_fcvt_i2f::<false, true, false> as Step),
                        _ => None,
                    },
                    0x1c if f3 == 0 && rs2 == 0 => both!(step_fmv_x, [], dbl),
                    0x1e if f3 == 0 && rs2 == 0 => both!(step_fmv_f, [], dbl),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}
