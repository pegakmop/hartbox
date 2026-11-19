//! Differential test for the generated superinstructions: for each template,
//! run its instruction sequence through the specialised step and through the
//! ordinary per-instruction steps, and check both leave the same state behind.
//! The templates are keyed by exact operand tuples, so no ordinary test program
//! reaches them by accident.

use crate::block::Chain;
use crate::block::STEPS;
use crate::bus::Bus;
use crate::hart::Hart;
use crate::insn::{DInsn, Op, Step};
use crate::spec::{lookup, Spec, SPECS};

/// Rebuild the instruction stream a template was generated from, and check the
/// matcher finds it again.
fn template_ops(spec: &Spec) -> Vec<DInsn> {
    let n = spec.words.len() / 2;
    let mut ops = Vec::with_capacity(n + 1);
    for i in 0..n {
        let (w0, w1) = (spec.words[i * 2], spec.words[i * 2 + 1]);
        ops.push(DInsn {
            op: unsafe { std::mem::transmute::<u8, Op>(w0 as u8) },
            rd: (w0 >> 8) as u8,
            rs1: (w0 >> 16) as u8,
            rs2: (w0 >> 24) as u8,
            rs3: 0,
            f: 0,
            len: ((w1 >> 32) & 0xff) as u8,
            index: i as u8,
            imm: (w1 as u32) as i32,
            off: 0,
            step: None,
            link: std::cell::Cell::new(0),
        });
    }
    ops.push(DInsn::EMPTY);
    let mut off = 0i32;
    for d in ops.iter_mut() {
        d.off = off;
        off += d.len as i32;
    }
    Hart::steps_for_test(&mut ops);
    // A longer template may be shadowed by a shorter one with the same prefix;
    // either is correct, and the specialised step hands the rest of the
    // sequence back to the ordinary steps.
    let (len, _step) = unsafe { lookup(ops.as_ptr(), ops.len()) }.expect("lookup must find a template");
    assert!(len <= n, "matched more instructions than the template has");
    crate::block::fold_targets(&mut ops);
    ops
}

fn run(ops: &[DInsn], step: Step, in_ram: bool) -> (Hart, u64) {
    let mut hart = Hart::new(0, false);
    let mut bus = Bus::new(1 << 20, 1);
    for r in 1..32usize {
        // Either addresses that hit RAM (so a run completes and its tail runs)
        // or wild ones (so the slow-path hand-off is exercised too).
        hart.x[r] = if in_ram {
            0x8000_0000u64 + (r as u64 * 512)
        } else {
            0x1111_0000_0000_0000u64
                .wrapping_mul(r as u64 + 3)
                .wrapping_add(0x7f3b_2a19_5c4d_6e8f)
        };
    }
    hart.x[0] = 0;
    hart.pc = 0x1000;
    hart.block_pc = 0x1000;
    // No chaining: `finish` hands the count back at the terminator.
    hart.break_chain();
    let retired = unsafe { crate::block::invoke(step, Chain::from_hart(&hart), hart.ltlb_cur, &mut hart, &mut bus, ops.as_ptr(), 0, 0) };
    (hart, retired)
}

#[test]
fn generated_superinstructions_match_the_generic_path() {
    for (t, spec) in SPECS.iter().enumerate() {
        let ops = template_ops(spec);
        for in_ram in [true, false] {
        let (specialised, n_spec) = run(&ops, spec.step, in_ram);
        let (generic, n_gen) = run(&ops, STEPS[ops[0].op as usize], in_ram);
        for r in 0..32usize {
            if specialised.x[r] != generic.x[r] {
                for i in 0..ops.len() - 1 {
                    let d = &ops[i];
                    eprintln!(
                        "  {i}: {:?} x{} = x{} op x{} imm={} len={}",
                        d.op, d.rd, d.rs1, d.rs2, d.imm, d.len
                    );
                }
                panic!(
                    "template {t}: x{r} differs ({:#x} vs {:#x})",
                    specialised.x[r], generic.x[r]
                );
            }
        }
        assert_eq!(specialised.pc, generic.pc, "template {t}: pc differs (in_ram={in_ram})");
        assert_eq!(n_spec, n_gen, "template {t}: retired count differs");
        }
    }
}

/// The M extension's high multiplies, divisions and remainders have leaf steps of their
/// own; they must agree with the interpreter on every edge (zero divisor, overflow,
/// sign extension of the word forms).
#[test]
fn muldiv_steps_match_the_interpreter() {
    let ops = [
        Op::Mulh,
        Op::Mulhsu,
        Op::Mulhu,
        Op::Div,
        Op::Divu,
        Op::Rem,
        Op::Remu,
        Op::Divw,
        Op::Divuw,
        Op::Remw,
        Op::Remuw,
    ];
    let vals: [u64; 16] = [
        0,
        1,
        2,
        3,
        u64::MAX,
        u64::MAX - 1,
        i64::MIN as u64,
        i64::MAX as u64,
        0x8000_0000,
        0xffff_ffff,
        0x7fff_ffff,
        0xffff_ffff_8000_0000,
        0x1_0000_0000,
        0xdead_beef_1234_5678,
        0x0123_4567_89ab_cdef,
        10,
    ];
    let mut bus = Bus::new(1 << 20, 1);
    for op in ops {
        for &a in &vals {
            for &b in &vals {
                for rd in [5u8, 0] {
                    let d = DInsn { op, rd, rs1: 6, rs2: 7, len: 4, ..DInsn::EMPTY };
                    let mut slow = Hart::new(0, false);
                    slow.x[6] = a;
                    slow.x[7] = b;
                    slow.pc = 0x1004;
                    slow.exec(&mut bus, &d, 0x1000).unwrap();
                    let mut ops_v = vec![d.clone(), DInsn::EMPTY];
                    ops_v[1].off = 4;
                    ops_v[1].len = 0;
                    Hart::steps_for_test(&mut ops_v);
                    let mut fast = Hart::new(0, false);
                    fast.x[6] = a;
                    fast.x[7] = b;
                    fast.pc = 0x1000;
                    fast.block_pc = 0x1000;
                    fast.break_chain();
                    let n = unsafe { crate::block::invoke(ops_v[0].step.unwrap(), Chain::from_hart(&fast), fast.ltlb_cur, &mut fast, &mut bus, ops_v.as_ptr(), 0, 0) };
                    assert_eq!(n, 1);
                    assert_eq!(slow.x, fast.x, "{op:?} {a:#x} {b:#x} rd={rd}");
                }
            }
        }
    }
}
