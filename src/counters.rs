//! Optional execution counters (cargo feature `counters`), for profiling.
//! Without the feature every `inc` is an empty inline function.
//! Every counter is a relaxed atomic increment and lives off the hot path,
//! except where noted. Print with `--stats`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        $(pub static $name: AtomicU64 = AtomicU64::new(0);)*
    };
}

counters!(
    // Instruction/step accounting.
    FAST_LOAD_HIT,
    FAST_STORE_HIT,
    SLOW_LOAD,
    SLOW_STORE,
    LOAD_CROSS_PAGE,
    LOAD_WRONG_MODE,
    LOAD_MTLB_MISS,
    LOAD_RAM_MISS,
    STORE_CROSS_PAGE,
    STORE_WRONG_MODE,
    STORE_MTLB_MISS,
    STORE_RAM_MISS,
    TRAPS,
    CSR_READS,
    CSR_WRITES,
    MMIO_READS,
    MMIO_WRITES,
    BLOCKS_BUILT,
    OPS_BUILT,
    OPS_UNIQUE,
    BUILD_NS,
    BLOCKS_CHAINED,
    EXITS,
    SELF_LOOP,
    LINK_HIT,
    FOLLOW_INDIRECT,
    FOLLOW_DIRECT,
    LINK_MISS,
    BCACHE_HIT,
    PAIRS,
    TLB_WALKS,
    TLB_HITS,
    SFENCES,
    SFENCE_PAGE,
    SATP_WRITES,
    SPEC_UNITS,
    SPEC_INSNS,
    UDP_DEFERRED,
    UDP_RETRIED,
    ICMP_RETRIED,
    NET_POLLS,
    SINGLE_UNITS,
    FWD_FA,
    FWD_FB,
    FORGET_CODE,
    MTLB_FLUSH,
    INB_FWD,
    INB_BACK,
    INB_SELF,
    INB_OUT,
    INB_FWD_SKIP,
);

/// Sampled runs of consecutive ALU instructions, keyed by their exact operand
/// tuples. Used to generate specialised superinstructions.
pub const RUN_SLOTS: usize = 1 << 15;
pub const RUN_MAX: usize = 64;
pub static RUN_LEN: [AtomicU32; RUN_SLOTS] = [const { AtomicU32::new(0) }; RUN_SLOTS];
pub static RUN_COUNT: [AtomicU64; RUN_SLOTS] = [const { AtomicU64::new(0) }; RUN_SLOTS];
pub static RUN_DATA: [[AtomicU64; RUN_MAX * 2]; RUN_SLOTS] =
    [const { [const { AtomicU64::new(0) }; RUN_MAX * 2] }; RUN_SLOTS];
#[cfg(feature = "counters")]
static RUN_SAMPLE: AtomicU64 = AtomicU64::new(0);

/// Record the run `insns` (packed) if this dispatch is sampled. Sampling keeps
/// the counter traffic to a fraction of the instructions run.
#[inline(always)]
pub fn sample_run(insns: &[u64]) {
    #[cfg(feature = "counters")]
    {
        // `sample_tick` has already decided to sample this dispatch.
        if insns.is_empty() {
            return;
        }
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for w in insns {
            h ^= w;
            h = h.wrapping_mul(0x100000001b3);
        }
        for probe in 0..8u64 {
            let slot = (h.wrapping_add(probe.wrapping_mul(0x9E37_79B9_7F4A_7C15)) as usize) & (RUN_SLOTS - 1);
            let len = RUN_LEN[slot].load(Relaxed);
            if len == 0 {
                // Claim the slot, then fill it.
                if RUN_LEN[slot]
                    .compare_exchange(0, insns.len() as u32, Relaxed, Relaxed)
                    .is_ok()
                {
                    for (i, w) in insns.iter().enumerate().take(RUN_MAX * 2) {
                        RUN_DATA[slot][i].store(*w, Relaxed);
                    }
                    RUN_COUNT[slot].fetch_add(1, Relaxed);
                }
                return;
            }
            if len as usize == insns.len() {
                let same = insns
                    .iter()
                    .enumerate()
                    .all(|(i, w)| RUN_DATA[slot][i].load(Relaxed) == *w);
                if same {
                    RUN_COUNT[slot].fetch_add(1, Relaxed);
                    return;
                }
            }
        }
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = insns;
    }
}

/// Pack one instruction of a run: `op`, its register fields, its immediate and
/// its length.
#[inline(always)]
pub const fn run_word0(op: u8, rd: u8, rs1: u8, rs2: u8) -> u64 {
    (op as u64) | ((rd as u64) << 8) | ((rs1 as u64) << 16) | ((rs2 as u64) << 24)
}

#[inline(always)]
pub const fn run_word1(imm: i32, len: u8) -> u64 {
    (imm as u32) as u64 | ((len as u64) << 32)
}

/// Run `f` for every 8th dispatch, for template mining.
#[inline(always)]
pub fn sample_tick(f: impl FnOnce()) {
    #[cfg(feature = "counters")]
    {
        if RUN_SAMPLE.fetch_add(1, Relaxed) & 15 == 0 {
            f();
        }
    }
    #[cfg(not(feature = "counters"))]
    let _ = f;
}

/// Record which opcode went through the general interpreter.
#[inline(always)]
pub fn inc_generic(op: u8) {
    #[cfg(feature = "counters")]
    GENERIC_OP[(op & 127) as usize].fetch_add(1, Relaxed);
    #[cfg(not(feature = "counters"))]
    let _ = op;
}

/// Add `n` to a counter in one go.
#[inline(always)]
pub fn add(c: &AtomicU64, n: u64) {
    #[cfg(feature = "counters")]
    c.fetch_add(n, Relaxed);
    #[cfg(not(feature = "counters"))]
    let _ = (c, n);
}

/// Deepest chain stack usage seen, in bytes below the base recorded by `stack_base`.
pub static STACK_MAX: AtomicU64 = AtomicU64::new(0);
static STACK_BASE: AtomicU64 = AtomicU64::new(0);

/// Record the stack address at the start of a run.
#[inline(always)]
pub fn stack_base(p: usize) {
    STACK_BASE.store(p as u64, Relaxed);
}

/// Track the deepest stack address reached; the address of a local in the
/// block engine approximates the current chain depth.
#[inline(always)]
pub fn stack_probe(p: usize) {
    #[cfg(feature = "counters")]
    {
        let base = STACK_BASE.load(Relaxed);
        if base != 0 {
            let depth = base.wrapping_sub(p as u64) as u64;
            if depth > STACK_MAX.load(Relaxed) {
                STACK_MAX.store(depth, Relaxed);
            }
        }
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = p;
    }
}

/// Dynamic execution count per opcode (`Op as u8`), counted at dispatch.
pub static SPEC_USE: [AtomicU64; 4096] = [const { AtomicU64::new(0) }; 4096];
/// CSR instructions by address and kind (0 read only, 1 `csrrw`, 2 `csrrs` with a write, 3 `csrrc`
/// with a write), counted in `Hart::exec`.
pub static CSR_USE: [AtomicU64; 4096 * 4] = [const { AtomicU64::new(0) }; 4096 * 4];
#[inline(always)]
pub fn inc_csr(addr: u16, kind: usize) {
    #[cfg(feature = "counters")]
    CSR_USE[(addr as usize & 4095) * 4 + kind].fetch_add(1, Relaxed);
    #[cfg(not(feature = "counters"))]
    let _ = (addr, kind);
}
/// Nanoseconds in each phase of `build_block_inner` (decode, marking, fusion planning, step
/// assignment, allocation), counters builds only.
pub static PHASE_NS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
pub static GENERIC_OP: [AtomicU64; 128] = [const { AtomicU64::new(0) }; 128];
pub static OP_COUNT: [AtomicU64; 128] = [const { AtomicU64::new(0) }; 128];

#[inline(always)]
pub fn inc_op(op: u8) {
    #[cfg(feature = "counters")]
    {
        OP_COUNT[(op & 127) as usize].fetch_add(1, Relaxed);
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = op;
    }
}

/// Opcodes whose unit ran as a plain (unfused) step, and what followed them.
pub static SINGLE_OP: [AtomicU64; 128] = [const { AtomicU64::new(0) }; 128];
pub static SINGLE_BIGRAM: [[AtomicU32; 128]; 128] = [const { [const { AtomicU32::new(0) }; 128] }; 128];

#[inline(always)]
pub fn inc_single(a: u8, b: u8) {
    #[cfg(feature = "counters")]
    {
        SINGLE_OP[(a & 127) as usize].fetch_add(1, Relaxed);
        SINGLE_BIGRAM[(a & 127) as usize][(b & 127) as usize].fetch_add(1, Relaxed);
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = (a, b);
    }
}

/// Dynamic bigram counts: `BIGRAM[a][b]` is how often op `a` was followed by
/// op `b` at step granularity. Used to mine fused pairs.
pub static BIGRAM: [[AtomicU32; 128]; 128] = [const { [const { AtomicU32::new(0) }; 128] }; 128];

/// Classes for 4-gram counting: the thirty most frequent opcodes get their own
/// class so the table stays small (32^4 entries).
const QUAD_CLASSES: usize = 32;

#[cfg(feature = "counters")]
const fn quad_class(op: crate::insn::Op) -> usize {
    use crate::insn::Op::*;
    match op {
        Add => 0, Addi => 1, Ld => 2, Addw => 3, Srliw => 4, Sd => 5, Xor => 6, Slliw => 7,
        Slli => 8, Lw => 9, Bne => 10, Sw => 11, Srli => 12, Beq => 13, And => 14, Jalr => 15,
        Auipc => 16, Lbu => 17, Jal => 18, Or => 19, Addiw => 20, Sub => 21, Andi => 22,
        Lwu => 23, Sltu => 24, Sltiu => 25, Bltu => 26, Sllw => 27, Srlw => 28, Mul => 29,
        Sh => 30,
        _ => 31,
    }
}

/// A representative opcode per class, for printing mining results.
#[cfg(feature = "counters")]
pub const QUAD_CLASS_OPS: [crate::insn::Op; QUAD_CLASSES] = {
    use crate::insn::Op::*;
    [Add, Addi, Ld, Addw, Srliw, Sd, Xor, Slliw, Slli, Lw, Bne, Sw, Srli, Beq, And, Jalr,
     Auipc, Lbu, Jal, Or, Addiw, Sub, Andi, Lwu, Sltu, Sltiu, Bltu, Sllw, Srlw, Mul, Sh, Illegal]
};

/// Dynamic 4-gram counts over the classes above.
pub static QUAD: [AtomicU32; QUAD_CLASSES * QUAD_CLASSES * QUAD_CLASSES * QUAD_CLASSES] =
    [const { AtomicU32::new(0) }; QUAD_CLASSES * QUAD_CLASSES * QUAD_CLASSES * QUAD_CLASSES];

#[inline(always)]
pub fn inc_quad(a: u8, b: u8, c: u8, d: u8) {
    #[cfg(feature = "counters")]
    {
        let ia = quad_class(unsafe { std::mem::transmute::<u8, crate::insn::Op>(a & 127) });
        let ib = quad_class(unsafe { std::mem::transmute::<u8, crate::insn::Op>(b & 127) });
        let ic = quad_class(unsafe { std::mem::transmute::<u8, crate::insn::Op>(c & 127) });
        let id = quad_class(unsafe { std::mem::transmute::<u8, crate::insn::Op>(d & 127) });
        let idx = ((ia * QUAD_CLASSES + ib) * QUAD_CLASSES + ic) * QUAD_CLASSES + id;
        QUAD[idx].fetch_add(1, Relaxed);
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = (a, b, c, d);
    }
}

/// Dynamic trigram counts, for mining three-instruction fusions.
pub static TRIGRAM: [[[AtomicU32; 128]; 128]; 128] =
    [const { [const { [const { AtomicU32::new(0) }; 128] }; 128] }; 128];

#[inline(always)]
pub fn inc_trigram(a: u8, b: u8, c: u8) {
    #[cfg(feature = "counters")]
    {
        TRIGRAM[(a & 127) as usize][(b & 127) as usize][(c & 127) as usize].fetch_add(1, Relaxed);
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = (a, b, c);
    }
}

#[inline(always)]
pub fn inc_bigram(a: u8, b: u8) {
    #[cfg(feature = "counters")]
    {
        BIGRAM[(a & 127) as usize][(b & 127) as usize].fetch_add(1, Relaxed);
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = (a, b);
    }
}

#[inline(always)]
pub fn inc(c: &AtomicU64) {
    #[cfg(feature = "counters")]
    {
        c.fetch_add(1, Relaxed);
    }
    #[cfg(not(feature = "counters"))]
    {
        let _ = c;
    }
}

pub struct Snapshot(pub Vec<(&'static str, u64)>);

pub fn snapshot() -> Snapshot {
    let mut v = Vec::new();
    macro_rules! snap {
        ($($name:ident),* $(,)?) => { $(v.push((stringify!($name), $name.load(Relaxed)));)* };
    }
    snap!(
        EXITS,
        SPEC_UNITS,
        SPEC_INSNS,
        UDP_DEFERRED,
        UDP_RETRIED,
        ICMP_RETRIED,
        SINGLE_UNITS,
        FWD_FA,
        FWD_FB,
        FORGET_CODE,
        MTLB_FLUSH,
        INB_FWD,
        INB_BACK,
        INB_SELF,
        INB_OUT,
        INB_FWD_SKIP,
        SELF_LOOP,
        LINK_HIT,
        FOLLOW_INDIRECT,
        FOLLOW_DIRECT,
            LINK_MISS,
        FAST_LOAD_HIT,
        FAST_STORE_HIT,
        SLOW_LOAD,
        SLOW_STORE,
        LOAD_CROSS_PAGE,
        LOAD_WRONG_MODE,
        LOAD_MTLB_MISS,
        LOAD_RAM_MISS,
        STORE_CROSS_PAGE,
        STORE_WRONG_MODE,
        STORE_MTLB_MISS,
        STORE_RAM_MISS,
        TRAPS,
        CSR_READS,
        CSR_WRITES,
        MMIO_READS,
        MMIO_WRITES,
        BLOCKS_BUILT,
        OPS_BUILT,
        OPS_UNIQUE,
        BUILD_NS,
        BLOCKS_CHAINED,
        PAIRS,
        TLB_WALKS,
        TLB_HITS,
        SFENCES,
        SFENCE_PAGE,
        SATP_WRITES,
    SPEC_UNITS,
    SPEC_INSNS,
    UDP_DEFERRED,
    UDP_RETRIED,
    ICMP_RETRIED,
    NET_POLLS,
    SINGLE_UNITS,
    FWD_FA,
    FWD_FB,
    );
    Snapshot(v)
}
