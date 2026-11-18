//! F and D extensions.
//!
//! Arithmetic is done with the host's IEEE-754 double (and single, via
//! double, which is innocuous for +, -, *, / and sqrt). Non-nearest rounding
//! modes are honoured by computing the rounding residual exactly (TwoSum or
//! FMA) and stepping the result one ulp when the mode requires it. `fmadd`
//! directed rounding is approximated from a split product; see
//! `fused_residual`.

use crate::arch::*;
use crate::bus::Bus;
use crate::hart::{Hart, Res, Trap};
use crate::insn::{DInsn, Op};

const NX: u64 = 1 << 0;
const UF: u64 = 1 << 1;
const OF: u64 = 1 << 2;
const DZ: u64 = 1 << 3;
const NV: u64 = 1 << 4;

const RNE: u8 = 0;
const RTZ: u8 = 1;
const RDN: u8 = 2;
const RUP: u8 = 3;
const RMM: u8 = 4;

const CANON32: u32 = 0x7fc0_0000;
const CANON64: u64 = 0x7ff8_0000_0000_0000;

#[inline(always)]
fn box32(bits: u32) -> u64 {
    0xffff_ffff_0000_0000 | bits as u64
}

/// Value of a single-precision register. Improperly boxed values read as the
/// canonical NaN, per the spec.
#[inline(always)]
fn unbox32(v: u64) -> u32 {
    if v >> 32 == 0xffff_ffff {
        v as u32
    } else {
        CANON32
    }
}

#[inline(always)]
fn is_snan(bits: u64, dbl: bool) -> bool {
    if dbl {
        f64::from_bits(bits).is_nan() && bits & (1 << 51) == 0
    } else {
        let b = bits as u32;
        f32::from_bits(b).is_nan() && b & (1 << 22) == 0
    }
}

#[inline(always)]
fn sgn(x: f64) -> i8 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

/// Resolve the instruction's rounding-mode field against `frm`.
#[inline(always)]
fn resolve_rm(field: u8, frm: u64) -> Option<u8> {
    let rm = if field == 7 { frm as u8 } else { field };
    (rm <= RMM).then_some(rm)
}

/// Exact residual of a rounded result: `e` is the sign of (exact - rounded)
/// and `v` approximates its value. The value is only used to decide ties for
/// round-to-nearest-away; `e` alone decides inexactness.
#[derive(Clone, Copy)]
struct Resid {
    e: i8,
    v: f64,
    /// Scale for the tie test: the residual is a tie when |v| == m * gap / 2.
    m: f64,
}

impl Resid {
    const EXACT: Resid = Resid { e: 0, v: 0.0, m: 1.0 };

    #[inline(always)]
    fn from_value(v: f64) -> Resid {
        Resid { e: sgn(v), v, m: 1.0 }
    }
}

/// Step `rn` toward the exact value as required by `rm`.
#[inline(always)]
fn round_f64(rn: f64, res: Resid, rm: u8) -> f64 {
    let e = res.e;
    if e == 0 || !rn.is_finite() {
        return rn;
    }
    match rm {
        RTZ if rn > 0.0 && e < 0 => rn.next_down(),
        RTZ if rn < 0.0 && e > 0 => rn.next_up(),
        RDN if e < 0 => rn.next_down(),
        RUP if e > 0 => rn.next_up(),
        RMM => {
            // Exactly halfway between rn and its neighbour on the exact side:
            // round away from zero, i.e. to the neighbour of larger magnitude.
            let gap = if e > 0 { rn.next_up() - rn } else { rn - rn.next_down() };
            if res.v.abs() == res.m * gap / 2.0 && (rn == 0.0 || (rn > 0.0) == (e > 0)) {
                if e > 0 {
                    rn.next_up()
                } else {
                    rn.next_down()
                }
            } else {
                rn
            }
        }
        _ => rn,
    }
}

#[inline(always)]
fn round_f32(rn: f32, res: Resid, rm: u8) -> f32 {
    let e = res.e;
    if e == 0 || !rn.is_finite() {
        return rn;
    }
    match rm {
        RTZ if rn > 0.0 && e < 0 => rn.next_down(),
        RTZ if rn < 0.0 && e > 0 => rn.next_up(),
        RDN if e < 0 => rn.next_down(),
        RUP if e > 0 => rn.next_up(),
        RMM => {
            let gap = if e > 0 { rn.next_up() - rn } else { rn - rn.next_down() };
            if res.v.abs() == res.m * gap as f64 / 2.0 && (rn == 0.0 || (rn > 0.0) == (e > 0)) {
                if e > 0 {
                    rn.next_up()
                } else {
                    rn.next_down()
                }
            } else {
                rn
            }
        }
        _ => rn,
    }
}

/// Overflow result for a directed rounding mode: largest finite or infinity.
#[inline(always)]
fn overflow_is_max(neg: bool, rm: u8) -> bool {
    match rm {
        RTZ => true,
        RDN => !neg,
        RUP => neg,
        _ => false,
    }
}

/// Round a double result to double precision and compute its flags.
fn finish64(v: f64, res: Resid, rm: u8) -> (f64, u64) {
    if v.is_infinite() {
        let neg = v < 0.0;
        let r = if overflow_is_max(neg, rm) {
            if neg {
                -f64::MAX
            } else {
                f64::MAX
            }
        } else {
            v
        };
        return (r, OF | NX);
    }
    let r = round_f64(v, res, rm);
    let mut fl = if res.e != 0 { NX } else { 0 };
    if res.e != 0 && r.abs() < f64::MIN_POSITIVE {
        fl |= UF;
    }
    (r, fl)
}

/// Round a double-precision intermediate `r64` (with residual `res64`) to
/// single precision.
fn to_f32(r64: f64, res64: Resid, rm: u8) -> (f32, u64) {
    if !r64.is_finite() {
        return (r64 as f32, 0);
    }
    let rn = r64 as f32;
    if rn.is_infinite() {
        let neg = r64 < 0.0;
        let r = if overflow_is_max(neg, rm) {
            if neg {
                -f32::MAX
            } else {
                f32::MAX
            }
        } else {
            rn
        };
        return (r, OF | NX);
    }
    // Exact minus rn: the double-to-single step is exact, so the residual of
    // the double result plus that step gives the direction.
    let d = r64 - rn as f64;
    let res = if d == 0.0 {
        res64
    } else if res64.e == 0 {
        Resid { e: sgn(d), v: d, m: 1.0 }
    } else {
        Resid { e: sgn(d), v: res64.v + d, m: res64.m }
    };
    let r = round_f32(rn, res, rm);
    let mut fl = if res.e != 0 { NX } else { 0 };
    if res.e != 0 && r.abs() < f32::MIN_POSITIVE {
        fl |= UF;
    }
    (r, fl)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bin {
    Add,
    Sub,
    Mul,
    Div,
}

/// 2^600: scales tiny residuals into the normal range, where an FMA can
/// represent them exactly.
const RESIDUAL_SCALE: f64 = 4.149_515_568_880_993e180;

/// Product-to-addend ratio below which a fused product only sets the rounding
/// direction (it is under 1/256 of an ulp of the addend).
const TINY_RATIO: f64 = 8.673_617_379_884_035e-19; // 2^-60

/// Below this magnitude, a residual may fall under the subnormal grid and be
/// lost, so the residual is computed in the scaled domain instead.
const TINY: f64 = 6.223_015_277_861_142e-271; // 2^-900

/// Residual of a product `p` (rounded) against the exact `a*b`.
fn residual_mul(a: f64, b: f64, p: f64) -> Resid {
    if p == 0.0 {
        // Underflow to zero: the exact product is non-zero unless an input is.
        return if a == 0.0 || b == 0.0 {
            Resid::EXACT
        } else {
            Resid { e: sgn(a) * sgn(b), v: 0.0, m: 1.0 }
        };
    }
    if p.abs() >= TINY {
        let r = a.mul_add(b, -p);
        return Resid { e: sgn(r), v: r, m: 1.0 };
    }
    // Tiny product: compute the residual scaled up, where it is representable.
    let r = (a * RESIDUAL_SCALE).mul_add(b, -(p * RESIDUAL_SCALE));
    Resid { e: sgn(r), v: r, m: RESIDUAL_SCALE }
}

/// Residual of a quotient `q` (rounded) against the exact `a/b`.
fn residual_div(a: f64, b: f64, q: f64) -> Resid {
    if a == 0.0 {
        return Resid::EXACT;
    }
    let sb = sgn(b);
    if q == 0.0 {
        return Resid { e: sgn(a) * sb, v: 0.0, m: 1.0 };
    }
    // The residual of a/b is (a - q*b)/b. Keep the numerator residual `r` and
    // the scale |b|*scale so that a tie (|r| == |b|*scale*gap/2) is exact.
    // Small dividends need the scaled residual (see TINY).
    let (r, scale) = if a.abs() >= TINY {
        ((-q).mul_add(b, a), 1.0)
    } else {
        ((-(q * RESIDUAL_SCALE)).mul_add(b, a * RESIDUAL_SCALE), RESIDUAL_SCALE)
    };
    Resid { e: sgn(r) * sb, v: r, m: b.abs() * scale }
}

/// Binary operation in double precision, with the exact residual of the
/// rounded result.
fn bin64(op: Bin, a: f64, b: f64, rm: u8) -> (f64, Resid) {
    match op {
        Bin::Add | Bin::Sub => {
            let b = if op == Bin::Sub { -b } else { b };
            let s = a + b;
            if !s.is_finite() {
                return (s, Resid::EXACT);
            }
            // Knuth's TwoSum: exact error of the rounded sum.
            let bb = s - a;
            let err = (a - (s - bb)) + (b - bb);
            // An exact zero from operands of opposite sign is +0, or -0 when
            // rounding toward negative infinity.
            let s = if s == 0.0 && a.is_sign_negative() != b.is_sign_negative() {
                if rm == RDN {
                    -0.0
                } else {
                    0.0
                }
            } else {
                s
            };
            (s, Resid::from_value(err))
        }
        Bin::Mul => {
            let p = a * b;
            if !p.is_finite() {
                return (p, Resid::EXACT);
            }
            (p, residual_mul(a, b, p))
        }
        Bin::Div => {
            let q = a / b;
            if !q.is_finite() || b == 0.0 || b.is_infinite() {
                return (q, Resid::EXACT);
            }
            (q, residual_div(a, b, q))
        }
    }
}

/// Round-to-integral for float -> integer conversions.
fn round_integral(x: f64, rm: u8) -> f64 {
    match rm {
        RNE => x.round_ties_even(),
        RTZ => x.trunc(),
        RDN => x.floor(),
        RUP => x.ceil(),
        _ => x.round(),
    }
}

fn canon(dbl: bool) -> u64 {
    if dbl {
        CANON64
    } else {
        box32(CANON32)
    }
}

/// Final packaging of an arithmetic result. `finite_in` says whether all
/// inputs were finite, so an infinite result means overflow.
fn package(r: f64, res: Resid, rm: u8, dbl: bool, finite_in: bool) -> (u64, u64) {
    if r.is_nan() {
        return (canon(dbl), 0);
    }
    if r.is_infinite() && !finite_in {
        return (if dbl { r.to_bits() } else { box32((r as f32).to_bits()) }, 0);
    }
    if dbl {
        let (v, fl) = finish64(r, res, rm);
        (v.to_bits(), fl)
    } else {
        let (v, fl) = to_f32(r, res, rm);
        (box32(v.to_bits()), fl)
    }
}

/// Residual of the fused result `r` = RNE(a*b + c), approximately: the sign
/// is exact in the cases that matter for directed rounding.
fn fused_residual(a: f64, b: f64, c: f64, r: f64) -> Resid {
    let p = a * b;
    let ep = a.mul_add(b, -p); // exact tail of the product
    let s = p + c;
    if !s.is_finite() {
        return Resid::EXACT;
    }
    let bb = s - p;
    let e1 = (p - (s - bb)) + (c - bb); // exact tail of p + c
    // exact a*b + c = s + e1 + ep, and r is its rounding, so the residual is
    // (s - r) + e1 + ep. Sum the three terms with error-free steps.
    let t = s - r;
    let u = t + e1;
    let ub = u - t;
    let u_err = (t - (u - ub)) + (e1 - ub);
    let sum = u + (u_err + ep);
    Resid { e: sgn(sum), v: sum, m: 1.0 }
}

/// Round an integer to a float of the requested format, honouring `rm`.
fn int_to_float(v: i128, dbl: bool, rm: u8) -> (u64, u64) {
    if dbl {
        let rn = v as f64;
        let res = Resid::from_value((v - rn as i128) as f64);
        let fl = if res.e != 0 { NX } else { 0 };
        (round_f64(rn, res, rm).to_bits(), fl)
    } else {
        let rn = v as f32;
        let res = Resid::from_value((v - rn as i128) as f64);
        let fl = if res.e != 0 { NX } else { 0 };
        (box32(round_f32(rn, res, rm).to_bits()), fl)
    }
}

/// Float -> integer conversion with saturation. Returns the (unextended)
/// result bits and flags.
fn float_to_int(x: f64, rm: u8, signed: bool, long: bool) -> (u64, u64) {
    if x.is_nan() {
        let v = match (signed, long) {
            (true, true) => i64::MAX as u64,
            (true, false) => i32::MAX as u32 as u64,
            (false, true) => u64::MAX,
            (false, false) => u32::MAX as u64,
        };
        return (v, NV);
    }
    let r = round_integral(x, rm);
    let inexact = if r != x { NX } else { 0 };
    let (lo, hi): (f64, f64) = match (signed, long) {
        (true, true) => (-9.223_372_036_854_776e18, 9.223_372_036_854_776e18),
        (true, false) => (i32::MIN as f64, i32::MAX as f64 + 1.0),
        (false, true) => (0.0, 1.844_674_407_370_955_2e19),
        (false, false) => (0.0, u32::MAX as f64 + 1.0),
    };
    if r < lo {
        // Negative values that round to zero are fine for unsigned conversions.
        let v = match (signed, long) {
            (true, true) => i64::MIN as u64,
            (true, false) => i32::MIN as u32 as u64,
            _ => 0,
        };
        return (v, NV);
    }
    if r >= hi {
        let v = match (signed, long) {
            (true, true) => i64::MAX as u64,
            (true, false) => i32::MAX as u32 as u64,
            (false, true) => u64::MAX,
            (false, false) => u32::MAX as u64,
        };
        return (v, NV);
    }
    let v = if signed {
        if long {
            r as i64 as u64
        } else {
            r as i32 as u32 as u64
        }
    } else if long {
        r as u64
    } else {
        r as u32 as u64
    };
    (v, inexact)
}

/// Classification bit for `fclass`.
fn fclass(bits: u64, dbl: bool) -> u64 {
    let (sign, exp, mant, emax, mbits) = if dbl {
        ((bits >> 63) & 1, (bits >> 52) & 0x7ff, bits & ((1 << 52) - 1), 0x7ff, 52)
    } else {
        ((bits >> 31) & 1, (bits >> 23) & 0xff, bits & 0x7f_ffff, 0xff, 23)
    };
    let qbit = 1u64 << (mbits - 1);
    if exp == emax {
        if mant == 0 {
            if sign != 0 {
                1 << 0
            } else {
                1 << 7
            }
        } else if mant & qbit == 0 {
            1 << 8
        } else {
            1 << 9
        }
    } else if exp == 0 {
        match (mant == 0, sign != 0) {
            (true, true) => 1 << 3,
            (true, false) => 1 << 4,
            (false, true) => 1 << 2,
            (false, false) => 1 << 5,
        }
    } else if sign != 0 {
        1 << 1
    } else {
        1 << 6
    }
}

impl Hart {
    #[inline(always)]
    fn fp_enabled(&self) -> bool {
        self.mstatus & MSTATUS_FS != 0
    }

    #[inline(always)]
    fn set_fs_dirty(&mut self) {
        self.mstatus |= MSTATUS_FS;
    }

    #[inline(always)]
    fn accrue(&mut self, flags: u64) {
        self.fflags |= flags;
    }

    /// Read an FP operand as a double, for either precision.
    #[inline(always)]
    fn read_fp(&self, r: u8, dbl: bool) -> u64 {
        let raw = self.f[(r & 31) as usize];
        if dbl {
            raw
        } else {
            unbox32(raw) as u64
        }
    }

    /// Run a floating-point instruction in the interpreter. The software rounding code
    /// below computes with the host's FPU too, which raises the host's own exception flags
    /// (`FPSR`), where the native steps of `fastfp.rs` keep the guest's: take what the guest
    /// had accrued into `fflags` first, and drop what this computation raised after.
    pub(crate) fn exec_fp(&mut self, bus: &mut Bus, d: &DInsn, pc: u64) -> Res<()> {
        self.fflags |= crate::fastfp::native_flags();
        crate::fastfp::clear_native_flags();
        let r = self.exec_fp_inner(bus, d, pc);
        crate::fastfp::clear_native_flags();
        r
    }

    fn exec_fp_inner(&mut self, bus: &mut Bus, d: &DInsn, pc: u64) -> Res<()> {
        if !self.fp_enabled() {
            return Err(Trap::new(EXC_ILLEGAL, self.fp_raw_bits(bus, pc)));
        }
        let (rd, rs1, rs2, rs3) = (d.rd as usize & 31, d.rs1, d.rs2, d.rs3);
        let a_addr = self.rx(rs1);
        match d.op {
            Op::Flw => {
                let v = self.load::<4>(bus, a_addr.wrapping_add(d.imm as u64))?;
                self.f[rd] = box32(v as u32);
                self.set_fs_dirty();
            }
            Op::Fld => {
                let v = self.load::<8>(bus, a_addr.wrapping_add(d.imm as u64))?;
                self.f[rd] = v;
                self.set_fs_dirty();
            }
            Op::Fsw => {
                let v = self.f[rs2 as usize & 31] as u32 as u64;
                self.store::<4>(bus, a_addr.wrapping_add(d.imm as u64), v)?;
            }
            Op::Fsd => {
                let v = self.f[rs2 as usize & 31];
                self.store::<8>(bus, a_addr.wrapping_add(d.imm as u64), v)?;
            }
            Op::Fmadd | Op::Fmsub | Op::Fnmsub | Op::Fnmadd => {
                let rm = resolve_rm(d.f, self.frm).ok_or(Trap::new(EXC_ILLEGAL, self.fp_raw_bits(bus, pc)))?;
                let dbl = d.imm == 1;
                let a = self.read_fp(rs1, dbl);
                let b = self.read_fp(rs2, dbl);
                let c = self.read_fp(rs3, dbl);
                // FMSUB / FNMSUB negate the addend; FNMADD / FNMSUB negate the product.
                let neg_prod = matches!(d.op, Op::Fnmsub | Op::Fnmadd);
                let neg_add = matches!(d.op, Op::Fmsub | Op::Fnmadd);
                let (bits, fl) = self.fma(a, b, c, dbl, rm, neg_prod, neg_add);
                self.f[rd] = bits;
                self.accrue(fl);
                self.set_fs_dirty();
            }
            Op::Fp => self.exec_fp_op(bus, d, pc)?,
            _ => return Err(Trap::new(EXC_ILLEGAL, d.imm as u32 as u64)),
        }
        Ok(())
    }

    /// Raw bits of the current instruction, for `mtval` on illegal FP ops.
    #[cold]
    fn fp_raw_bits(&mut self, bus: &mut Bus, pc: u64) -> u64 {
        self.translate(bus, pc, crate::mmu::Access::Fetch).ok().and_then(|pa| bus.read::<4>(pa)).unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    fn fma(&self, a: u64, b: u64, c: u64, dbl: bool, rm: u8, neg_prod: bool, neg_add: bool) -> (u64, u64) {
        let fa = if dbl { f64::from_bits(a) } else { f32::from_bits(a as u32) as f64 };
        let fb = if dbl { f64::from_bits(b) } else { f32::from_bits(b as u32) as f64 };
        let fc = if dbl { f64::from_bits(c) } else { f32::from_bits(c as u32) as f64 };
        let nan_in = fa.is_nan() || fb.is_nan() || fc.is_nan();
        let snan = is_snan(a, dbl) || is_snan(b, dbl) || is_snan(c, dbl);
        let fa = if neg_prod { -fa } else { fa };
        let fc = if neg_add { -fc } else { fc };
        let finite_in = fa.is_finite() && fb.is_finite() && fc.is_finite();
        let fl = if snan { NV } else { 0 };
        // inf * 0 is invalid even when the addend is a NaN.
        if fa.is_infinite() && fb == 0.0 || fa == 0.0 && fb.is_infinite() {
            return (canon(dbl), fl | NV);
        }
        if nan_in {
            return (canon(dbl), fl);
        }
        let r = fa.mul_add(fb, fc);
        if r.is_nan() {
            return (canon(dbl), fl | NV);
        }
        // Single precision: the product of two f32 values is exact in f64, so
        // the only roundings are the f64 fused step and the final f32 step.
        let p = fa * fb;
        let res = if !r.is_finite() {
            Resid::EXACT
        } else if fc == 0.0 {
            // Pure product: its own residual decides.
            residual_mul(fa, fb, r)
        } else if p.abs() < fc.abs() * TINY_RATIO {
            // The product is far below the addend's ulp: the result is the
            // addend, and the exact product's sign gives the direction.
            Resid { e: sgn(fa) * sgn(fb), v: 0.0, m: 1.0 }
        } else {
            fused_residual(fa, fb, fc, r)
        };
        // An exact zero sum has the sign rule of IEEE 754: -0 only when rounding
        // toward negative infinity and the product and addend differ in sign.
        let r = if r == 0.0 && res.e == 0 && (fa.is_sign_negative() != fb.is_sign_negative()) != fc.is_sign_negative() {
            if rm == RDN {
                -0.0
            } else {
                0.0
            }
        } else {
            r
        };
        let (bits, f2) = package(r, res, rm, dbl, finite_in);
        (bits, fl | f2)
    }

    fn exec_fp_op(&mut self, bus: &mut Bus, d: &DInsn, pc: u64) -> Res<()> {
        let funct5 = (d.imm & 31) as u8;
        let dbl = (d.imm >> 5) & 1 == 1;
        let f3 = d.f;
        let (rd, rs1, rs2) = (d.rd, d.rs1, d.rs2);
        let illegal = |h: &mut Hart, bus: &mut Bus| Trap::new(EXC_ILLEGAL, h.fp_raw_bits(bus, pc));

        match funct5 {
            // -------------------------------------------- arithmetic
            0x00..=0x03 => {
                let rm = resolve_rm(f3, self.frm).ok_or_else(|| illegal(self, bus))?;
                let op = match funct5 {
                    0x00 => Bin::Add,
                    0x01 => Bin::Sub,
                    0x02 => Bin::Mul,
                    _ => Bin::Div,
                };
                let ra = self.read_fp(rs1, dbl);
                let rb = self.read_fp(rs2, dbl);
                let (bits, fl) = self.arith2(op, ra, rb, dbl, rm);
                self.f[rd as usize] = bits;
                self.accrue(fl);
                self.set_fs_dirty();
            }
            0x0b => {
                if rs2 != 0 {
                    return Err(illegal(self, bus));
                }
                let rm = resolve_rm(f3, self.frm).ok_or_else(|| illegal(self, bus))?;
                let ra = self.read_fp(rs1, dbl);
                let (bits, fl) = self.sqrt(ra, dbl, rm);
                self.f[rd as usize] = bits;
                self.accrue(fl);
                self.set_fs_dirty();
            }
            // -------------------------------------------- sign injection
            0x04 => {
                if f3 > 2 {
                    return Err(illegal(self, bus));
                }
                let a = self.f[rs1 as usize];
                let b = self.f[rs2 as usize];
                let bits = if dbl {
                    let sign = match f3 {
                        0 => b & (1 << 63),
                        1 => !b & (1 << 63),
                        _ => (a ^ b) & (1 << 63),
                    };
                    (a & !(1 << 63)) | sign
                } else {
                    let a32 = unbox32(a);
                    let b32 = unbox32(b);
                    let sign = match f3 {
                        0 => b32 & (1 << 31),
                        1 => !b32 & (1 << 31),
                        _ => (a32 ^ b32) & (1 << 31),
                    };
                    box32((a32 & !(1 << 31)) | sign)
                };
                self.f[rd as usize] = bits;
                self.set_fs_dirty();
            }
            // -------------------------------------------- min / max
            0x05 => {
                if f3 > 1 {
                    return Err(illegal(self, bus));
                }
                let ra = self.read_fp(rs1, dbl);
                let rb = self.read_fp(rs2, dbl);
                let (bits, fl) = minmax(ra, rb, dbl, f3 == 1);
                self.f[rd as usize] = bits;
                self.accrue(fl);
                self.set_fs_dirty();
            }
            // -------------------------------------------- FP <-> FP conversion
            0x08 => {
                let rm = resolve_rm(f3, self.frm).ok_or_else(|| illegal(self, bus))?;
                let (bits, fl) = if !dbl && rs2 == 1 {
                    // FCVT.S.D
                    let x = f64::from_bits(self.f[rs1 as usize]);
                    if is_snan(self.f[rs1 as usize], true) {
                        (box32(CANON32), NV)
                    } else if x.is_nan() {
                        (box32(CANON32), 0)
                    } else if x.is_infinite() {
                        (box32((x as f32).to_bits()), 0)
                    } else {
                        let (v, fl) = to_f32(x, Resid::EXACT, rm);
                        (box32(v.to_bits()), fl)
                    }
                } else if dbl && rs2 == 0 {
                    // FCVT.D.S
                    let raw = self.f[rs1 as usize];
                    let x = unbox32(raw);
                    let fl = if is_snan(x as u64, false) { NV } else { 0 };
                    let v = f32::from_bits(x) as f64;
                    let bits = if v.is_nan() { CANON64 } else { v.to_bits() };
                    (bits, fl)
                } else {
                    return Err(illegal(self, bus));
                };
                self.f[rd as usize] = bits;
                self.accrue(fl);
                self.set_fs_dirty();
            }
            // -------------------------------------------- comparisons
            0x14 => {
                let ra = self.read_fp(rs1, dbl);
                let rb = self.read_fp(rs2, dbl);
                let (x, y) = if dbl {
                    (f64::from_bits(ra), f64::from_bits(rb))
                } else {
                    (f32::from_bits(ra as u32) as f64, f32::from_bits(rb as u32) as f64)
                };
                let any_nan = x.is_nan() || y.is_nan();
                let (res, fl) = match f3 {
                    2 => (x == y, if is_snan(ra, dbl) || is_snan(rb, dbl) { NV } else { 0 }),
                    1 => (x < y, if any_nan { NV } else { 0 }),
                    0 => (x <= y, if any_nan { NV } else { 0 }),
                    _ => return Err(illegal(self, bus)),
                };
                self.wx(rd, res as u64);
                self.accrue(fl);
            }
            // -------------------------------------------- FP -> integer
            0x18 => {
                let rm = resolve_rm(f3, self.frm).ok_or_else(|| illegal(self, bus))?;
                let ra = self.read_fp(rs1, dbl);
                let x = if dbl {
                    f64::from_bits(ra)
                } else {
                    f32::from_bits(ra as u32) as f64
                };
                let (signed, long) = match rs2 {
                    0 => (true, false),
                    1 => (false, false),
                    2 => (true, true),
                    3 => (false, true),
                    _ => return Err(illegal(self, bus)),
                };
                let (v, fl) = float_to_int(x, rm, signed, long);
                let v = if long { v } else { v as u32 as i32 as i64 as u64 };
                self.wx(rd, v);
                self.accrue(fl);
            }
            // -------------------------------------------- integer -> FP
            0x1a => {
                let rm = resolve_rm(f3, self.frm).ok_or_else(|| illegal(self, bus))?;
                let x = self.rx(rs1);
                let v: i128 = match rs2 {
                    0 => x as u32 as i32 as i128,
                    1 => x as u32 as i128,
                    2 => x as i64 as i128,
                    3 => x as i128,
                    _ => return Err(illegal(self, bus)),
                };
                let (bits, fl) = int_to_float(v, dbl, rm);
                self.f[rd as usize] = bits;
                self.accrue(fl);
                self.set_fs_dirty();
            }
            // -------------------------------------------- moves and classify
            0x1c => {
                if rs2 != 0 {
                    return Err(illegal(self, bus));
                }
                match f3 {
                    0 => {
                        // FMV.X.W / FMV.X.D
                        let raw = self.f[rs1 as usize];
                        let v = if dbl { raw } else { unbox32(raw) as i32 as i64 as u64 };
                        self.wx(rd, v);
                    }
                    1 => {
                        let bits = self.read_fp(rs1, dbl);
                        self.wx(rd, fclass(bits, dbl));
                    }
                    _ => return Err(illegal(self, bus)),
                }
            }
            0x1e => {
                if rs2 != 0 || f3 != 0 {
                    return Err(illegal(self, bus));
                }
                let x = self.rx(rs1);
                self.f[rd as usize] = if dbl { x } else { box32(x as u32) };
                self.set_fs_dirty();
            }
            _ => return Err(illegal(self, bus)),
        }
        Ok(())
    }

    fn arith2(&self, op: Bin, ra: u64, rb: u64, dbl: bool, rm: u8) -> (u64, u64) {
        let snan = is_snan(ra, dbl) || is_snan(rb, dbl);
        let (x, y) = if dbl {
            (f64::from_bits(ra), f64::from_bits(rb))
        } else {
            (f32::from_bits(ra as u32) as f64, f32::from_bits(rb as u32) as f64)
        };
        let mut fl = if snan { NV } else { 0 };
        if x.is_nan() || y.is_nan() {
            return (canon(dbl), fl);
        }
        // Invalid: inf-inf, 0*inf, 0/0, inf/inf.
        let both_inf = x.is_infinite() && y.is_infinite();
        let invalid = match op {
            Bin::Add => both_inf && x.signum() != y.signum(),
            Bin::Sub => both_inf && x.signum() == y.signum(),
            Bin::Mul => (x.is_infinite() && y == 0.0) || (x == 0.0 && y.is_infinite()),
            Bin::Div => (x == 0.0 && y == 0.0) || both_inf,
        };
        if invalid {
            return (canon(dbl), fl | NV);
        }
        if op == Bin::Div && y == 0.0 && x.is_finite() && x != 0.0 {
            fl |= DZ;
        }
        // Division by zero yields infinity without overflow.
        let finite_in = x.is_finite() && y.is_finite() && !(op == Bin::Div && y == 0.0);
        // Single precision is computed in double, then rounded.
        let (r, res) = bin64(op, x, y, rm);
        let (bits, f2) = package(r, res, rm, dbl, finite_in);
        (bits, fl | f2)
    }

    fn sqrt(&self, ra: u64, dbl: bool, rm: u8) -> (u64, u64) {
        let snan = is_snan(ra, dbl);
        let x = if dbl {
            f64::from_bits(ra)
        } else {
            f32::from_bits(ra as u32) as f64
        };
        if x.is_nan() {
            return (canon(dbl), if snan { NV } else { 0 });
        }
        if x < 0.0 {
            return (canon(dbl), NV);
        }
        let s = x.sqrt();
        let res = if s.is_finite() {
            let r = (-s).mul_add(s, x);
            Resid { e: sgn(r), v: r, m: 2.0 * s }
        } else {
            Resid::EXACT
        };
        package(s, res, rm, dbl, x.is_finite())
    }
}

fn minmax(ra: u64, rb: u64, dbl: bool, max: bool) -> (u64, u64) {
    let snan = is_snan(ra, dbl) || is_snan(rb, dbl);
    let fl = if snan { NV } else { 0 };
    let (x, y) = if dbl {
        (f64::from_bits(ra), f64::from_bits(rb))
    } else {
        (f32::from_bits(ra as u32) as f64, f32::from_bits(rb as u32) as f64)
    };
    let pick_raw = match (x.is_nan(), y.is_nan()) {
        (true, true) => return (canon(dbl), fl),
        (true, false) => rb,
        (false, true) => ra,
        _ => {
            if x == 0.0 && y == 0.0 {
                // -0 is less than +0, so min prefers -0 and max prefers +0.
                let x_neg = x.is_sign_negative();
                let take_x = if max { !x_neg } else { x_neg };
                if take_x {
                    ra
                } else {
                    rb
                }
            } else {
                let take_x = if max { x > y } else { x < y };
                if take_x {
                    ra
                } else {
                    rb
                }
            }
        }
    };
    let bits = if dbl { pick_raw } else { box32(pick_raw as u32) };
    (bits, fl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directed_rounding_steps_correctly() {
        // 1 + 2^-60 is inexact; RUP must step above 1.0.
        let (s, res) = bin64(Bin::Add, 1.0, 2f64.powi(-60), RNE);
        assert_eq!(s, 1.0);
        assert_eq!(res.e, 1);
        assert_eq!(round_f64(s, res, RUP), 1.0f64.next_up());
        assert_eq!(round_f64(s, res, RNE), 1.0);
        assert_eq!(round_f64(s, res, RTZ), 1.0);
    }

    #[test]
    fn round_to_nearest_away_breaks_ties_outward() {
        // 1 + 2^-53 is exactly halfway between 1 and 1 + 2^-52.
        let (s, res) = bin64(Bin::Add, 1.0, 2f64.powi(-53), RMM);
        assert_eq!(round_f64(s, res, RMM), 1.0f64.next_up());
        assert_eq!(round_f64(s, res, RNE), 1.0);
    }

    #[test]
    fn fclass_categories() {
        assert_eq!(fclass(f64::NEG_INFINITY.to_bits(), true), 1 << 0);
        assert_eq!(fclass((-0.0f64).to_bits(), true), 1 << 3);
        assert_eq!(fclass(1.0f64.to_bits(), true), 1 << 6);
        assert_eq!(fclass(CANON64, true), 1 << 9);
    }

    #[test]
    fn float_to_int_saturates() {
        let (v, fl) = float_to_int(1e30, RTZ, true, false);
        assert_eq!(v as u32 as i32, i32::MAX);
        assert_eq!(fl, NV);
        let (v, fl) = float_to_int(2.5, RNE, true, true);
        assert_eq!((v, fl), (2, NX));
    }
}
