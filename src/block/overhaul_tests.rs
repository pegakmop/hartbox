use super::*;

const WFI: u32 = 0x10500073;
const MEMORY_SIZE: u64 = 0x8000;
const TRAP: u64 = RAM_BASE + 0x400;

fn i_type(op: u32, f3: u32, rd: u32, rs1: u32, imm: i32) -> u32 {
    ((imm as u32) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | op
}

fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
    i_type(0x13, 0, rd, rs1, imm)
}

fn sd(base: u32, source: u32, off: u32) -> u32 {
    (off >> 5) << 25 | source << 20 | base << 15 | 3 << 12 | (off & 31) << 7 | 0x23
}

fn branch(f3: u32, a: u32, b: u32, offset: i32) -> u32 {
    let offset = offset as u32;
    (offset >> 12 & 1) << 31
        | (offset >> 5 & 63) << 25
        | b << 20
        | a << 15
        | f3 << 12
        | (offset >> 1 & 15) << 8
        | (offset >> 11 & 1) << 7
        | 0x63
}

/// Check each scheduler exit, including exact pointer/register state at a
/// page miss and the architectural PC of a fault.
fn run_case(
    code: &[u32],
    init: impl Fn(&mut Hart, &mut Bus),
    budget: u64,
    expected_step: Step,
) -> (Hart, Bus) {
    let mut fast = Hart::new(0, false);
    let mut scalar = Hart::new(0, false);
    let mut bus = Bus::new(MEMORY_SIZE as usize, 1);
    let mut reference = Bus::new(MEMORY_SIZE as usize, 1);
    for (hart, memory) in [(&mut fast, &mut bus), (&mut scalar, &mut reference)] {
        hart.pc = RAM_BASE;
        hart.mtvec = TRAP;
        memory.write::<4>(TRAP, WFI as u64).unwrap();
        for (i, &word) in code.iter().enumerate() {
            memory
                .write::<4>(RAM_BASE + i as u64 * 4, word as u64)
                .unwrap();
        }
        init(hart, memory);
    }
    let block = fast.next_block(&mut bus).unwrap().unwrap();
    let step = unsafe { (*Block::ops_ptr(block)).step.unwrap() };
    assert!(std::ptr::fn_addr_eq(step, expected_step));
    for _ in 0..256 {
        if fast.wfi {
            break;
        }
        let n = fast.run(&mut bus, budget);
        assert!(n > 0);
        for _ in 0..n {
            scalar.step_uncached(&mut reference);
        }
        assert_eq!(fast.x, scalar.x);
        assert_eq!(
            (fast.pc, fast.instret, fast.instret_bias),
            (scalar.pc, scalar.instret, scalar.instret_bias)
        );
        assert_eq!(
            (fast.mcause, fast.mepc, fast.mtval),
            (scalar.mcause, scalar.mepc, scalar.mtval)
        );
    }
    assert!(fast.wfi && scalar.wfi);
    assert_eq!(bus.ram, reference.ram);
    (fast, bus)
}

fn decode_sequence(code: &[u32]) -> Vec<DInsn> {
    code.iter()
        .enumerate()
        .map(|(i, &word)| {
            let mut d = decode32(word);
            d.off = (i * 4) as i32;
            d.index = i as u8;
            d
        })
        .collect()
}

#[test]
fn fused_ram_window_preserves_order_and_second_access_faults() {
    let mut code = [i_type(3, 3, 6, 5, 0), addi(5, 5, 8), sd(5, 6, 0), WFI];
    for reverse in [false, true] {
        if reverse {
            code[..3].copy_from_slice(&[sd(5, 6, 0), addi(5, 5, 8), i_type(3, 3, 6, 5, 0)]);
        }
        let ops = decode_sequence(&code);
        let step = memory_with_gap(&ops[0], &ops[1], &ops[2]).unwrap();
        for offset in [0x3008, 0x3ff8, MEMORY_SIZE - 8] {
            for cached in [false, true] {
                run_case(&code, |hart, bus| {
                    let base = RAM_BASE + offset;
                    hart.x[5] = base;
                    hart.x[6] = u64::MAX - 42;
                    bus.write::<8>(base, 0x0102_0304_0506_0708).unwrap();
                    if cached {
                        for address in [base, base + 8] {
                            hart.ftlb_fill(bus, address, address, false);
                            hart.ftlb_fill(bus, address, address, true);
                        }
                    }
                }, 128, step);
            }
        }
    }
}

#[test]
fn backward_streams_keep_precise_overlap_and_page_state() {
    let load = |off| i_type(3, 2, 9, 5, off);
    let store = |off: i32| {
        let off = off as u32;
        (off >> 5) << 25 | 9 << 20 | 6 << 15 | 2 << 12 | (off & 31) << 7 | 0x23
    };
    for compare in [branch(1, 5, 8, -24), branch(6, 8, 5, -24)] {
        let code = [load(0), store(0), load(-4), store(-4), addi(5, 5, -8), addi(6, 6, -8), compare, WFI];
        let ops = decode_sequence(&code[..7]);
        let step = bulk::plan(&ops).unwrap().step;
        let source = RAM_BASE + 0x4008;
        for dest in [RAM_BASE + 0x600c, source + 4, source - 4, source] {
            for budget in [1, 18, 65536] {
                run_case(&code, |hart, bus| {
                    hart.x[5] = source;
                    hart.x[6] = dest;
                    hart.x[8] = source - 8 * 16;
                    for i in 0..32 {
                        bus.write::<4>(source - i * 4, 0x8765_4321 + i).unwrap();
                    }
                    for (address, store) in [(source, false), (dest, true)] {
                        for va in [address, address - 4096] {
                            hart.ftlb_fill(bus, va, va, store);
                        }
                    }
                }, budget, step);
            }
        }
    }
}

#[test]
fn sentinel_copy_stores_the_marker_and_retains_precise_faults() {
    let code = [i_type(3, 0, 9, 5, 0), 9 << 20 | 6 << 15 | 0x23, addi(5, 5, 1),
        addi(6, 6, 1), branch(1, 8, 9, -16), WFI];
    let ops = decode_sequence(&code[..5]);
    let step = bulk::plan(&ops).unwrap().step;
    for (offset, marker) in [(0x3ff9, Some(0)), (0x3ff9, Some(23)), (MEMORY_SIZE - 7, None)] {
        for budget in [1, 20, 65536] {
            let (hart, bus) = run_case(&code, |hart, bus| {
                let source = RAM_BASE + offset;
                let dest = RAM_BASE + 0x5ffb;
                hart.x[5] = source;
                hart.x[6] = dest;
                hart.x[8] = (-119i64) as u64;
                for i in 0..marker.map_or(7, |pos| pos + 1) {
                    let value = if marker == Some(i) { 0x89 } else { 0xa0 + i };
                    bus.write::<1>(source + i, value).unwrap();
                }
                for (address, store) in [(source, false), (dest, true)] {
                    for va in [address, address + 4096] {
                        hart.ftlb_fill(bus, va, va, store);
                    }
                }
            }, budget, step);
            if let Some(marker) = marker {
                assert_eq!(hart.x[9], (-119i64) as u64);
                assert_eq!(bus.ram[0x5ffb + marker as usize], 0x89);
            } else {
                assert_eq!(hart.mcause, EXC_LOAD_ACCESS);
            }
        }
    }
}

#[test]
fn ram_reductions_match_precise_steps_with_word_overflow_and_page_faults() {
    for (op, f3) in [(0x3bu32, 0u32), (0x33, 0), (0x33, 4), (0x33, 6), (0x33, 7)] {
        let reduce = |a: u32, b: u32| b << 20 | a << 15 | f3 << 12 | 9 << 7 | op;
        let code = [i_type(3, 2, 6, 5, 0), addi(5, 5, 8), i_type(3, 2, 7, 5, -4),
            reduce(9, 7), reduce(6, 9), branch(6, 5, 8, -20), WFI];
        let ops = decode_sequence(&code[..6]);
        let step = bulk::plan(&ops).unwrap().step;
        for (offset, valid_words) in [(0x3ff8, 40), (MEMORY_SIZE - 8, 2)] {
            for budget in [1, 19, 65536] {
                run_case(&code, |hart, bus| {
                    let base = RAM_BASE + offset;
                    hart.x[5] = base;
                    hart.x[8] = base + 160;
                    hart.x[9] = 0xabcdef00_7ffffffe;
                    for i in 0..valid_words {
                        bus.write::<4>(base + i * 4, (0x7fffffed + i * 123) & 0xffffffff).unwrap();
                    }
                    for va in [base, base + 4096] {
                        hart.ftlb_fill(bus, va, va, false);
                    }
                }, budget, step);
            }
        }
    }
}

#[test]
fn sequential_scans_match_at_vector_lanes_pages_and_faults() {
    for leading in [false, true] {
        for (width, f3, needle) in [
            (1, 0, -119i64 as u64),
            (2, 5, 0x89),
            (4, 2, 0x89),
            (8, 3, 0x89),
        ] {
            let load = i_type(3, f3, 6, 5, if leading { -width } else { 0 });
            let add = addi(5, 5, width);
            for bounded in [false, true] {
                let cmp = if bounded {
                    branch(6, 5, 7, -8)
                } else {
                    branch(1, 7, 6, -8)
                };
                let mut code = if leading {
                    vec![add, load, cmp]
                } else {
                    vec![load, add, cmp]
                };
                let ops = decode_sequence(&code);
                let step = bulk::plan(&ops).unwrap().step;
                code.extend([addi(8, 5, 1), addi(9, 6, 2), WFI]);
                for (offset, marker) in [(0x3ff0, Some(0)), (0x3ff0, Some(17)), (0x7ff0, None)] {
                    if bounded && marker.is_none() {
                        continue;
                    }
                    for budget in [1, 65, 65536] {
                        let (h, _) = run_case(
                            &code,
                            |h, bus| {
                                let base = RAM_BASE + offset;
                                let w = width as u64;
                                h.x[5] = base;
                                h.x[7] = if bounded {
                                    base + (marker.unwrap() + 1) * w
                                } else {
                                    needle
                                };
                                if let Some(i) = marker {
                                    bus.write::<1>(base + i * w, 0x89).unwrap();
                                }
                                h.ftlb_fill(bus, base, base, false);
                            },
                            budget,
                            step,
                        );
                        if marker.is_none() {
                            assert_eq!(h.mtval, RAM_BASE + MEMORY_SIZE);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn stream_comparisons_preserve_the_precise_exit_and_pointer_state() {
    let load_a = i_type(3, 4, 7, 5, 0);
    let load_b = i_type(3, 4, 8, 6, -1);
    let code = [
        load_a,
        addi(6, 6, 1),
        load_b,
        branch(1, 7, 8, 12),
        addi(5, 5, 1),
        branch(1, 7, 0, -20),
        WFI,
    ];
    let step = bulk::plan(&decode_sequence(&code[..6])).unwrap().step;
    for budget in [1, 31, 65536] {
        for mismatch in [Some(0), Some(19), None] {
            run_case(
                &code,
                |h, bus| {
                    let a = RAM_BASE + 0x3ff0;
                    let b = RAM_BASE + 0x6002;
                    h.x[5] = a;
                    h.x[6] = b;
                    for j in 0..51 {
                        bus.write::<1>(a + j, j + 1).unwrap();
                        bus.write::<1>(b + j, if Some(j) == mismatch { 77 } else { j + 1 })
                            .unwrap();
                    }
                    for start in [a, a + 4096, b] {
                        h.ftlb_fill(bus, start, start, false);
                    }
                },
                budget,
                step,
            );
        }
    }
}

#[test]
fn grouped_address_updates_and_independent_memory_keep_fault_order() {
    let ld = |rd, base, offset| i_type(3, 3, rd, base, offset);
    let leading = [addi(5, 5, -16), sd(5, 5, 0), sd(5, 7, 8), WFI];
    let trailing = [ld(6, 5, 0), ld(7, 5, 8), addi(5, 5, 16), addi(8, 7, 1), WFI];
    let independent = [ld(7, 5, 0), sd(6, 8, 0), WFI];
    let forwarded = [ld(7, 5, 0), sd(6, 7, 0), WFI];
    for (code, step) in [
        (
            leading.as_slice(),
            writeback::plan(&decode_sequence(&leading)).unwrap().1,
        ),
        (
            trailing.as_slice(),
            writeback::plan(&decode_sequence(&trailing)).unwrap().1,
        ),
        (independent.as_slice(), pair_step(Op::Ld, Op::Sd, false).unwrap()),
        (forwarded.as_slice(), {
            let ops = decode_sequence(&forwarded);
            forward_store(&ops[0], &ops[1]).unwrap()
        }),
    ] {
        for (base, dest, cached) in [
            (0x4000, 0x4008, true),
            (0x4000, 0x4000, false),
            (0x7ff8, 0x8000, true),
            (0x8008, 0x3000, true),
        ] {
            run_case(
                code,
                |h, bus| {
                    h.x[5] = RAM_BASE + base;
                    h.x[6] = RAM_BASE + dest;
                    h.x[7] = u64::MAX;
                    h.x[8] = 0x1289_362c_aa7f_2c5d;
                    for addr in [h.x[5] - 16, h.x[5], h.x[6]] {
                        if addr < RAM_BASE + MEMORY_SIZE {
                            bus.write::<8>(addr, 0x9134_5678_1234_5678).unwrap();
                            if cached {
                                h.ftlb_fill(bus, addr, addr, false);
                                h.ftlb_fill(bus, addr, addr, true);
                            }
                        }
                    }
                },
                128,
                step,
            );
        }
    }
}

#[test]
fn arithmetic_and_pcrel_fusions_preserve_intermediate_values() {
    for rd in [6, 7] {
        let code = [
            i_type(0x1b, 5, 6, 5, 13),
            i_type(0x1b, 1, 7, 5, 19),
            7 << 20 | 6 << 15 | rd << 7 | 0x3b,
            addi(8, 6, 1),
            addi(9, 7, 1),
            WFI,
        ];
        let ops = decode_sequence(&code);
        let step = idioms::triple(&ops[0], &ops[1], &ops[2]).unwrap();
        run_case(&code, |h, _| h.x[5] = 0x9999_8888_8123_4567, 128, step);
    }
    for (slot, cached) in [(0x3000, true), (0x3000, false), (0x8000, false)] {
        for (load_reg, link) in [(5, 0), (6, 5), (6, 6), (6, 1)] {
            let code = [
                slot as u32 | 5 << 7 | 0x17,
                i_type(3, 3, load_reg, 5, 0),
                i_type(0x67, 0, link, load_reg, 0),
                WFI,
            ];
            let ops = decode_sequence(&code);
            let step = idioms::triple(&ops[0], &ops[1], &ops[2]).unwrap();
            run_case(
                &code,
                |h, bus| {
                    let va = RAM_BASE + slot;
                    if va < RAM_BASE + MEMORY_SIZE {
                        bus.write::<8>(va, RAM_BASE + 13).unwrap();
                        if cached {
                            h.ftlb_fill(bus, va, va, false);
                        }
                    }
                },
                128,
                step,
            );
        }
    }
}

#[test]
fn folded_addresses_keep_aliasing_and_traps_precise() {
    for load in [true, false] {
        for in_bounds in [true, false] {
            for cached in [true, false] {
                let access = if load {
                    i_type(3, 3, 6, 6, 16)
                } else {
                    sd(6, 6, 16)
                };
                let code = [addi(6, 5, -8), access, i_type(0x13, 6, 7, 6, 1), WFI];
                let ops = decode_sequence(&code);
                let step = idioms::address_pair(&ops[0], &ops[1]).unwrap();
                run_case(
                    &code,
                    |h, bus| {
                        let addr = RAM_BASE + if in_bounds { 0x4000 } else { MEMORY_SIZE };
                        h.x[5] = addr - 8;
                        if in_bounds {
                            bus.write::<8>(addr, 0x91f2_1234_5678_9abc).unwrap();
                            if cached {
                                h.ftlb_fill(bus, addr, addr, !load);
                            }
                        }
                    },
                    128,
                    step,
                );
            }
        }
    }
}

#[test]
fn forwarded_right_operand_and_overwrites_keep_precise_load_faults() {
    let add = 5 << 20 | 7 << 15 | 5 << 7 | 0x33;
    let load = i_type(3, 3, 5, 5, 0);
    for middle in [add, load] {
        let code = [addi(5, 6, 16), middle, load, WFI];
        let ops = decode_sequence(&code);
        let step = triple_step(&ops[0], &ops[1], &ops[2]).unwrap();
        for cached in [true, false] {
            for off in [0x4000, MEMORY_SIZE] {
                run_case(
                    &code,
                    |h, bus| {
                        let va = RAM_BASE + off;
                        h.x[6] = va - 16;
                        h.x[7] = 0;
                        if va < RAM_BASE + MEMORY_SIZE {
                            bus.write::<8>(va, RAM_BASE + 0x5000).unwrap();
                            bus.write::<8>(RAM_BASE + 0x5000, 0x8765_4321_abcdef00)
                                .unwrap();
                            if cached {
                                h.ftlb_fill(bus, va, va, false);
                                h.ftlb_fill(bus, RAM_BASE + 0x5000, RAM_BASE + 0x5000, false);
                            }
                        }
                    },
                    128,
                    step,
                );
            }
        }
    }
}
