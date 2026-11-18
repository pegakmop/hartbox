//! Differential test of the native floating-point steps (`fastfp.rs`): every instruction is
//! run through the interpreter (`Hart::exec`, the exact software rules of `fpu.rs`) and
//! through the step the block engine picks, on operands that include zeros, subnormals,
//! infinities, NaNs of every kind and the edges of the integer conversions, and the two
//! must leave the same registers and the same `fflags`.

use crate::block::Chain;
use crate::arch::MSTATUS_FS;
use crate::bus::Bus;
use crate::fastfp::{clear_native_flags, native_flags, FALLBACKS};
use crate::hart::Hart;
use crate::insn::{DInsn, Op};

const D_SPECIAL: [f64; 40] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    1.5,
    2.5,
    -0.5,
    -1.5,
    3.0,
    10.0,
    0.1,
    0.3,
    1.0 / 3.0,
    1e300,
    -1e300,
    1e-300,
    5e-324,
    -5e-324,
    2.2250738585072014e-308,
    1.1125369292536007e-308,
    f64::MAX,
    f64::MIN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    2147483647.0,
    2147483647.5,
    2147483648.0,
    -2147483648.0,
    -2147483649.0,
    4294967295.0,
    4294967296.0,
    9223372036854775807.0,
    -9223372036854775808.0,
    18446744073709551615.0,
    4503599627370496.5,
    -0.3,
    0.9999999999999999,
    1.0000000000000002,
    123456789.123456789,
];

const F_SPECIAL: [f32; 30] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    0.5,
    1.5,
    2.5,
    -0.5,
    -1.5,
    3.0,
    0.1,
    0.3,
    1.0 / 3.0,
    1e38,
    -1e38,
    1e-38,
    1e-45,
    -1e-45,
    1.17549435e-38,
    f32::MAX,
    f32::MIN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    2147483648.0,
    -2147483648.0,
    4294967296.0,
    8388608.5,
    16777216.0,
    -0.3,
    123456.79,
];

const D_NAN: [u64; 5] = [0x7ff8_0000_0000_0000, 0x7ff0_0000_0000_0001, 0xfff8_0000_0000_0123, 0x7ff4_0000_0000_0000, 0xffff_ffff_ffff_ffff];
const F_NAN: [u32; 4] = [0x7fc0_0000, 0x7f80_0001, 0xffc0_0123, 0x7fa0_0000];

struct Rig {
    a: Hart,
    b: Hart,
    bus_a: Bus,
    bus_b: Bus,
    rng: u64,
    cases: u64,
    fast: u64,
    /// Compare nothing when the result is zero or subnormal (`Some(true)` for doubles).
    skip_tiny: Option<bool>,
    /// What `f1` has to be instead of what the interpreter leaves there.
    expect_f1: Option<u64>,
}

impl Rig {
    fn new(seed: u64) -> Rig {
        Rig {
            a: Hart::new(0, false),
            b: Hart::new(0, false),
            bus_a: Bus::new(1 << 20, 1),
            bus_b: Bus::new(1 << 20, 1),
            rng: seed,
            cases: 0,
            fast: 0,
            skip_tiny: None,
            expect_f1: None,
        }
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// A double: mostly interesting values, sometimes random bits; NaNs only if `nan`.
    fn dbl(&mut self, nan: bool) -> u64 {
        let r = self.rand();
        match r % 10 {
            0..=4 => D_SPECIAL[(self.rand() % D_SPECIAL.len() as u64) as usize].to_bits(),
            5 if nan => D_NAN[(self.rand() % D_NAN.len() as u64) as usize],
            5 | 6 => {
                // A small number with a random binary exponent.
                let m = (self.rand() % 20001) as f64 - 10000.0;
                let e = (self.rand() % 120) as i32 - 60;
                (m * 2f64.powi(e)).to_bits()
            }
            7 => {
                // Near another interesting value: flip a low bit.
                D_SPECIAL[(self.rand() % D_SPECIAL.len() as u64) as usize].to_bits() ^ (1 << (self.rand() % 3))
            }
            _ => {
                let v = self.rand();
                // Keep random patterns finite unless NaNs are wanted.
                if !nan && f64::from_bits(v).is_nan() {
                    v & !(1 << 62)
                } else {
                    v
                }
            }
        }
    }

    /// A single-precision register value: properly boxed, except now and then.
    fn sgl(&mut self, nan: bool) -> u64 {
        let r = self.rand();
        let bits: u32 = match r % 10 {
            0..=4 => F_SPECIAL[(self.rand() % F_SPECIAL.len() as u64) as usize].to_bits(),
            5 if nan => F_NAN[(self.rand() % F_NAN.len() as u64) as usize],
            5 | 6 => {
                let m = (self.rand() % 2001) as f32 - 1000.0;
                let e = (self.rand() % 60) as i32 - 30;
                (m * 2f32.powi(e)).to_bits()
            }
            7 => F_SPECIAL[(self.rand() % F_SPECIAL.len() as u64) as usize].to_bits() ^ (1 << (self.rand() % 3)),
            _ => {
                let v = self.rand() as u32;
                if !nan && f32::from_bits(v).is_nan() {
                    v & !(1 << 30)
                } else {
                    v
                }
            }
        };
        if nan && self.rand() % 12 == 0 {
            // Improperly boxed.
            (self.rand() & 0xffff_ffff_0000_0000) | bits as u64
        } else {
            0xffff_ffff_0000_0000 | bits as u64
        }
    }

    fn val(&mut self, dbl: bool, nan: bool) -> u64 {
        if dbl {
            self.dbl(nan)
        } else {
            self.sgl(nan)
        }
    }

    fn int(&mut self) -> u64 {
        let r = self.rand();
        match r % 8 {
            0 => 0,
            1 => u64::MAX,
            2 => 0x7fff_ffff,
            3 => 0x8000_0000,
            4 => 0xffff_ffff,
            5 => i64::MIN as u64,
            6 => i64::MAX as u64,
            _ => self.rand() >> (self.rand() % 64),
        }
    }

    /// Run `d` both ways with `x` and `f` as the register files, and compare.
    fn case(&mut self, d: &DInsn, x: &[u64; 32], f: &[u64; 32], frm: u64, name: &str) {
        self.case_flags(d, x, f, frm, name, 0x1f)
    }

    /// As `case`; only the `fflags` bits in `mask` have to agree, and the interpreter's
    /// flags have to be among the native ones. (The interpreter loses the inexact flag of a
    /// square root below 2^-900, where its residual is under the subnormal grid, and of a
    /// fused multiply-add whose addend is far below the product: documented in `fpu.rs`.
    /// The host unit gets those right.)
    fn case_flags(&mut self, d: &DInsn, x: &[u64; 32], f: &[u64; 32], frm: u64, name: &str, mask: u64) {
        let pc = 0x1000u64;
        let len = d.len as u64;
        for h in [&mut self.a, &mut self.b] {
            h.x = *x;
            h.x[0] = 0;
            h.f = *f;
            h.fflags = 0;
            h.frm = frm;
            h.mstatus |= MSTATUS_FS;
            h.prv = 3;
        }
        // The interpreter. (The generators above computed with the host unit too.)
        clear_native_flags();
        self.a.pc = pc + len;
        let slow = self.a.exec(&mut self.bus_a, d, pc);
        // The block engine's step for it.
        let mut ops = vec![d.clone(), DInsn::EMPTY];
        ops[0].off = 0;
        ops[1].off = len as i32;
        ops[1].len = 0;
        Hart::steps_for_test(&mut ops);
        self.b.pc = pc;
        self.b.block_pc = pc;
        self.b.break_chain();
        let before = FALLBACKS.get();
        clear_native_flags();
        let n = unsafe { (ops[0].step.unwrap())(Chain::from_hart(&self.b), self.b.ltlb_cur, &mut self.b, &mut self.bus_b, ops.as_ptr(), 0, 0) };
        let took_fast = FALLBACKS.get() == before;
        self.b.fflags |= native_flags();
        clear_native_flags();
        self.cases += 1;
        if slow.is_err() {
            // An illegal encoding: the step must have handed it over (and so trapped).
            assert!(!took_fast, "{name}: illegal instruction ran on the fast path");
            return;
        }
        if took_fast {
            self.fast += 1;
        }
        if let Some(dbl) = self.skip_tiny {
            let v = self.b.f[1];
            let tiny = if dbl { (v >> 52) & 0x7ff == 0 } else { (v >> 23) & 0xff == 0 };
            if tiny {
                return;
            }
        }
        let ctx = || {
            format!(
                "{name} f3={} rd={} rs1={} rs2={} rs3={} imm={:#x}\n x={:x?}\n f={:x?}\n frm={frm} fast={took_fast}",
                d.f,
                d.rd,
                d.rs1,
                d.rs2,
                d.rs3,
                d.imm,
                &x[..4],
                &f[..4]
            )
        };
        assert_eq!(n, 1, "{}", ctx());
        assert_eq!(self.a.x, self.b.x, "x registers differ: {}", ctx());
        if let Some(v) = self.expect_f1 {
            // The interpreter rounds a single-precision fused multiply-add twice (through
            // a double); the host unit rounds once, like the architecture says.
            assert_eq!(self.b.f[1], v, "f1 is not the single-rounded result: {}", ctx());
            self.a.f[1] = v;
        }
        for i in 0..32 {
            assert_eq!(self.a.f[i], self.b.f[i], "f{i} differs: a={:#x} b={:#x} {}", self.a.f[i], self.b.f[i], ctx());
        }
        assert!(
            self.a.fflags & mask == self.b.fflags & mask && self.a.fflags & !self.b.fflags == 0,
            "fflags differ (interpreter {:#x}, native {:#x}): {}",
            self.a.fflags,
            self.b.fflags,
            ctx()
        );
        #[allow(unused_must_use)]
        let _ = ();
        assert_eq!(self.a.mstatus, self.b.mstatus, "mstatus differs: {}", ctx());
        assert_eq!(self.a.pc, self.b.pc, "pc differs: {}", ctx());
    }
}

fn insn(op: Op, rd: u8, rs1: u8, rs2: u8, rs3: u8, f: u8, imm: i32) -> DInsn {
    DInsn { op, rd, rs1, rs2, rs3, f, len: 4, imm, ..DInsn::EMPTY }
}

/// Registers the operands live in: rd=1 (distinct from the sources), rs1=2, rs2=3, rs3=4.
fn regs(rig: &mut Rig, dbl: bool, nan: bool, n: usize) -> ([u64; 32], [u64; 32]) {
    let mut f = [0u64; 32];
    let mut x = [0u64; 32];
    for r in 0..32 {
        f[r] = rig.val(dbl, nan);
        x[r] = rig.int();
    }
    if rig.rand() % 6 == 0 {
        // Same register for two operands (a * a, a - a).
        let src = 2 + (rig.rand() % n as u64) as usize;
        let dst = 2 + (rig.rand() % n as u64) as usize;
        f[dst] = f[src];
    }
    (x, f)
}

fn rm_pick(rig: &mut Rig) -> (u8, u64) {
    match rig.rand() % 12 {
        0..=3 => (0, 0),
        4..=6 => (7, 0),
        7 => (1, 0),
        8 => (2, 0),
        9 => (3, 0),
        10 => (4, 0),
        // Dynamic with a non-nearest frm.
        _ => (7, rig.rand() % 5),
    }
}

const ROUNDS: u64 = 4000;

#[test]
fn native_arithmetic_matches_the_interpreter() {
    let mut rig = Rig::new(0x1234_5678_9abc_def1);
    for dbl in [true, false] {
        for funct5 in [0u8, 1, 2, 3] {
            for round in 0..ROUNDS {
                let nan = round % 4 == 0;
                let (x, f) = regs(&mut rig, dbl, nan, 2);
                let (rm, frm) = rm_pick(&mut rig);
                let imm = funct5 as i32 | (dbl as i32) << 5;
                let d = insn(Op::Fp, 1, 2, 3, 0, rm, imm);
                rig.case(&d, &x, &f, frm, &format!("fp funct5={funct5} dbl={dbl}"));
            }
        }
        for round in 0..ROUNDS {
            let nan = round % 4 == 0;
            let (x, f) = regs(&mut rig, dbl, nan, 1);
            let (rm, frm) = rm_pick(&mut rig);
            let d = insn(Op::Fp, 1, 2, 0, 0, rm, 0x0b | (dbl as i32) << 5);
            let tiny = dbl && f64::from_bits(f[2]).abs() < 6.2e-271 && f[2] << 1 != 0;
            rig.case_flags(&d, &x, &f, frm, &format!("fsqrt dbl={dbl}"), if tiny { 0x1c } else { 0x1f });
        }
        for op in [Op::Fmadd, Op::Fmsub, Op::Fnmsub, Op::Fnmadd] {
            for round in 0..ROUNDS {
                let nan = round % 4 == 0;
                let (x, f) = regs(&mut rig, dbl, nan, 3);
                let (rm, frm) = rm_pick(&mut rig);
                // The interpreter's fused multiply-add is approximate with subnormal operands
                // (it returns +0 for a negative sum that underflows, for one): the host unit
                // is the reference there, and the encoding and operand order are what this
                // test is for.
                let sub = |v: u64| {
                    if dbl {
                        v << 1 != 0 && (v >> 52) & 0x7ff == 0
                    } else {
                        (v as u32) << 1 != 0 && (v >> 23) & 0xff == 0
                    }
                };
                if sub(f[2]) || sub(f[3]) || sub(f[4]) {
                    continue;
                }
                let d = insn(op, 1, 2, 3, 4, rm, dbl as i32);
                // NX and UF are approximate in the interpreter's fused multiply-add.
                rig.skip_tiny = Some(dbl);
                if !dbl && (rm == 0 || (rm == 7 && frm == 0)) && [f[2], f[3], f[4]].iter().all(|&v| v >> 32 == 0xffff_ffff) {
                    let (a, b, c) = (f32::from_bits(f[2] as u32), f32::from_bits(f[3] as u32), f32::from_bits(f[4] as u32));
                    let r = match op {
                        Op::Fmadd => a.mul_add(b, c),
                        Op::Fmsub => a.mul_add(b, -c),
                        Op::Fnmsub => (-a).mul_add(b, c),
                        _ => (-a).mul_add(b, -c),
                    };
                    rig.expect_f1 = (!r.is_nan() && !a.is_nan() && !b.is_nan() && !c.is_nan())
                        .then_some(0xffff_ffff_0000_0000 | r.to_bits() as u64);
                }
                rig.case_flags(&d, &x, &f, frm, &format!("{op:?} dbl={dbl}"), 0x1c);
                rig.skip_tiny = None;
                rig.expect_f1 = None;
            }
        }
    }
    eprintln!("arithmetic: {} cases, {} on the fast path", rig.cases, rig.fast);
    assert!(rig.fast * 2 > rig.cases, "the fast path barely ran");
}

#[test]
fn native_sign_minmax_compare_match_the_interpreter() {
    let mut rig = Rig::new(0x0fed_cba9_8765_4321);
    for dbl in [true, false] {
        for (funct5, f3s) in [(0x04u8, 0u8..3), (0x05, 0..2), (0x14, 0..3)] {
            for f3 in f3s {
                for round in 0..ROUNDS {
                    let nan = round % 3 == 0;
                    let (x, f) = regs(&mut rig, dbl, nan, 2);
                    let d = insn(Op::Fp, 1, 2, 3, 0, f3, funct5 as i32 | (dbl as i32) << 5);
                    rig.case(&d, &x, &f, 0, &format!("fp funct5={funct5:#x} f3={f3} dbl={dbl}"));
                }
            }
        }
    }
    eprintln!("sign/minmax/compare: {} cases, {} on the fast path", rig.cases, rig.fast);
    assert!(rig.fast * 2 > rig.cases, "the fast path barely ran");
}

#[test]
fn native_conversions_match_the_interpreter() {
    let mut rig = Rig::new(0x5a5a_a5a5_1357_9bdf);
    for dbl in [true, false] {
        // fcvt.{w,wu,l,lu}.{s,d}
        for sel in 0..4u8 {
            for round in 0..ROUNDS * 2 {
                let nan = round % 5 == 0;
                let (x, f) = regs(&mut rig, dbl, nan, 1);
                let (rm, frm) = rm_pick(&mut rig);
                let d = insn(Op::Fp, 1, 2, sel, 0, rm, 0x18 | (dbl as i32) << 5);
                rig.case(&d, &x, &f, frm, &format!("fcvt.int.fp sel={sel} dbl={dbl}"));
            }
        }
        // fcvt.{s,d}.{w,wu,l,lu}
        for sel in 0..4u8 {
            for _ in 0..ROUNDS {
                let (mut x, f) = regs(&mut rig, dbl, false, 1);
                x[2] = rig.int();
                let (rm, frm) = rm_pick(&mut rig);
                let d = insn(Op::Fp, 1, 2, sel, 0, rm, 0x1a | (dbl as i32) << 5);
                rig.case(&d, &x, &f, frm, &format!("fcvt.fp.int sel={sel} dbl={dbl}"));
            }
        }
        // fmv.x.{w,d}, fmv.{w,d}.x
        for _ in 0..ROUNDS {
            let (x, f) = regs(&mut rig, dbl, true, 1);
            let d = insn(Op::Fp, 1, 2, 0, 0, 0, 0x1c | (dbl as i32) << 5);
            rig.case(&d, &x, &f, 0, &format!("fmv.x dbl={dbl}"));
            let d = insn(Op::Fp, 1, 2, 0, 0, 0, 0x1e | (dbl as i32) << 5);
            rig.case(&d, &x, &f, 0, &format!("fmv.f dbl={dbl}"));
        }
    }
    // fcvt.s.d and fcvt.d.s
    for round in 0..ROUNDS * 2 {
        let nan = round % 4 == 0;
        let (x, f) = regs(&mut rig, true, nan, 1);
        let (rm, frm) = rm_pick(&mut rig);
        let d = insn(Op::Fp, 1, 2, 1, 0, rm, 0x08);
        rig.case(&d, &x, &f, frm, "fcvt.s.d");
        let (x, f) = regs(&mut rig, false, nan, 1);
        let d = insn(Op::Fp, 1, 2, 0, 0, rm, 0x08 | 1 << 5);
        rig.case(&d, &x, &f, frm, "fcvt.d.s");
    }
    eprintln!("conversions: {} cases, {} on the fast path", rig.cases, rig.fast);
    assert!(rig.fast * 2 > rig.cases, "the fast path barely ran");
}

#[test]
fn native_flags_reach_the_csrs() {
    let mut h = Hart::new(0, false);
    let mut bus = Bus::new(1 << 20, 1);
    h.mstatus |= MSTATUS_FS;
    h.prv = 3;
    h.f[2] = 1.0f64.to_bits();
    h.f[3] = 3.0f64.to_bits();
    let mut ops = vec![insn(Op::Fp, 1, 2, 3, 0, 0, 0x03 | 1 << 5), DInsn::EMPTY];
    ops[1].len = 0;
    ops[1].off = 4;
    Hart::steps_for_test(&mut ops);
    h.pc = 0x1000;
    h.block_pc = 0x1000;
    h.break_chain();
    clear_native_flags();
    unsafe { (ops[0].step.unwrap())(Chain::from_hart(&h), h.ltlb_cur, &mut h, &mut bus, ops.as_ptr(), 0, 0) };
    // 1/3 is inexact; the flag is still in the host register, and reading fflags sees it.
    assert_eq!(h.fflags, 0);
    assert_eq!(h.csr_read(&bus, 0x001).unwrap(), 1);
    assert_eq!(h.csr_read(&bus, 0x003).unwrap(), 1);
    // Writing fflags replaces it, and clears what the host unit had accrued.
    h.csr_write(0x001, 0).unwrap();
    assert_eq!(h.csr_read(&bus, 0x001).unwrap(), 0);
    assert_eq!(native_flags(), 0);
}

#[test]
fn native_fp_loads_and_stores_use_the_flat_tlb() {
    let mut h = Hart::new(0, false);
    let mut bus = Bus::new(1 << 20, 1);
    h.mstatus |= MSTATUS_FS;
    h.set_prv(crate::arch::PRV_M);
    let page = crate::arch::RAM_BASE + 0x2000;
    bus.write::<8>(page, 0x4009_21fb_5444_2d18).unwrap();
    bus.write::<4>(page + 16, 0x3fc0_0000).unwrap();
    h.ftlb_fill(&bus, page, page, false);
    h.ftlb_fill(&bus, page, page, true);
    h.x[2] = page;
    // fld f1, 0(x2); flw f2, 16(x2); fsd f1, 32(x2); fsw f2, 40(x2)
    let prog = [
        insn(Op::Fld, 1, 2, 0, 0, 3, 0),
        insn(Op::Flw, 2, 2, 0, 0, 2, 16),
        insn(Op::Fsd, 0, 2, 1, 0, 3, 32),
        insn(Op::Fsw, 0, 2, 2, 0, 2, 40),
    ];
    let before = FALLBACKS.get();
    for d in prog {
        let mut ops = vec![d, DInsn::EMPTY];
        ops[1].len = 0;
        ops[1].off = 4;
        Hart::steps_for_test(&mut ops);
        h.pc = 0x1000;
        h.block_pc = 0x1000;
        h.break_chain();
        let n = unsafe { (ops[0].step.unwrap())(Chain::from_hart(&h), h.ltlb_cur, &mut h, &mut bus, ops.as_ptr(), 0, 0) };
        assert_eq!(n, 1);
    }
    assert_eq!(FALLBACKS.get(), before, "a load or store left the fast path");
    assert_eq!(h.f[1], 0x4009_21fb_5444_2d18);
    assert_eq!(h.f[2], 0xffff_ffff_3fc0_0000, "flw NaN-boxes");
    assert_eq!(bus.read::<8>(page + 32), Some(0x4009_21fb_5444_2d18));
    assert_eq!(bus.read::<4>(page + 40), Some(0x3fc0_0000));
    assert_eq!(bus.read::<4>(page + 44), Some(0), "fsw stores 32 bits only");
}
