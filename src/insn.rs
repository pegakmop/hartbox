//! Instruction representation and decoders.
//!
//! Every instruction (32-bit or compressed) is decoded once into a compact
//! [`DInsn`] — 32 bytes, with the immediate already sign-extended and the
//! compressed forms already mapped onto their base-ISA equivalents. Decoded
//! instructions are cached per physical page by the block engine, so
//! the interpreter's hot loop never touches raw encodings.

use std::cell::Cell;

use crate::block::Chain;
use crate::bus::Bus;
use crate::hart::Hart;

/// Operation kind. `Empty` (zero) marks a slot in the decode cache that has
/// not been filled yet, so it must stay the zero value.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Op {
    #[default]
    Empty = 0,
    Illegal,
    // RV64I
    Lui,
    Auipc,
    Jal,
    Jalr,
    Beq,
    Bne,
    Blt,
    Bge,
    Bltu,
    Bgeu,
    Lb,
    Lh,
    Lw,
    Ld,
    Lbu,
    Lhu,
    Lwu,
    Sb,
    Sh,
    Sw,
    Sd,
    Addi,
    Slti,
    Sltiu,
    Xori,
    Ori,
    Andi,
    Slli,
    Srli,
    Srai,
    Addiw,
    Slliw,
    Srliw,
    Sraiw,
    Add,
    Sub,
    Sll,
    Slt,
    Sltu,
    Xor,
    Srl,
    Sra,
    Or,
    And,
    Addw,
    Subw,
    Sllw,
    Srlw,
    Sraw,
    // M
    Mul,
    Mulh,
    Mulhsu,
    Mulhu,
    Div,
    Divu,
    Rem,
    Remu,
    Mulw,
    Divw,
    Divuw,
    Remw,
    Remuw,
    // Misc-mem, system
    Fence,
    FenceI,
    Ecall,
    Ebreak,
    Mret,
    Sret,
    Wfi,
    SfenceVma,
    Csr,
    // A
    Lr,
    Sc,
    Amo,
    // F / D
    Flw,
    Fld,
    Fsw,
    Fsd,
    Fmadd,
    Fmsub,
    Fnmsub,
    Fnmadd,
    Fp,
    /// `fence` decoded while harts run on separate host threads: a real barrier.
    FenceMt,
    /// `fence.i` under the same conditions: a barrier that also looks at the code the
    /// other harts decoded since (see `Hart::sync_code_marks`).
    FenceIMt,
}

/// A predecoded instruction.
///
/// Field use by opcode:
/// * `rd`/`rs1`/`rs2`/`rs3` — register numbers (always < 32).
/// * `f` — funct3, or the rounding-mode field for FP ops.
/// * `len` — 4 or 2 bytes.
/// * `imm` — sign-extended immediate; CSR address for `Csr`; funct5 for
///   `Amo`; `funct5 | fmt << 5` for `Fp`; the raw encoding for `Illegal`.
#[derive(Clone, Debug, Default)]
#[repr(C)]
pub struct DInsn {
    // These fields and the handler pointer form the first 16 bytes.
    pub rd: u8,
    pub rs1: u8,
    pub rs2: u8,
    pub rs3: u8,
    pub imm: i32,
    /// The block-engine step that runs this instruction. Set when the block is
    /// built (see `block.rs`); decoding leaves it empty.
    pub step: Option<Step>,
    pub op: Op,
    pub f: u8,
    pub len: u8,
    /// Index of this instruction in its block, filled in when the block is
    /// built. It lets a block exit count retired instructions with one load
    /// instead of dividing a pointer difference by the instruction size.
    pub index: u8,
    /// Offset of this instruction's pc from the start of its block (the terminator
    /// holds the offset of the fall-through pc). Steps add it to the current
    /// block's virtual PC, carried in an argument on ARM64.
    pub off: i32,
    /// Cache for a fixed exit: zero while unresolved, a direct pointer for a
    /// same-page successor, or a tagged pointer to a checked link slot.
    /// Fused memory operations and PC-relative jumps also store metadata here.
    pub link: Cell<usize>,
}

/// Runs one threaded unit with the active load TLB, hart, bus, instruction,
/// and a forwarded result. The last argument holds either another result or
/// the preloaded operands. `Chain` carries the block PC and budget in registers
/// on ARM64 and uses hart fields on other targets to fit six integer arguments.
/// Returns the instruction count accounted for by the complete chain.
pub type Step = unsafe fn(Chain, *mut crate::hart::FastTlb, &mut Hart, &mut Bus, *const DInsn, u64, u64) -> u64;

impl DInsn {
    pub const EMPTY: DInsn = DInsn {
        op: Op::Empty,
        rd: 0,
        rs1: 0,
        rs2: 0,
        rs3: 0,
        f: 0,
        len: 0,
        index: 0,
        imm: 0,
        off: 0,
        step: None,
        link: Cell::new(0),
    };

    #[inline(always)]
    fn new(op: Op, rd: u8, rs1: u8, rs2: u8, f: u8, imm: i64) -> DInsn {
        DInsn { op, rd, rs1, rs2, rs3: 0, f, len: 4, index: 0, imm: imm as i32, off: 0, step: None, link: Cell::new(0) }
    }

    #[inline(always)]
    fn illegal(raw: u32) -> DInsn {
        DInsn { op: Op::Illegal, rd: 0, rs1: 0, rs2: 0, rs3: 0, f: 0, len: 4, index: 0, imm: raw as i32, off: 0, step: None, link: Cell::new(0) }
    }

    #[inline(always)]
    const fn c(op: Op, rd: u8, rs1: u8, rs2: u8, imm: i64) -> DInsn {
        DInsn { op, rd, rs1, rs2, rs3: 0, f: 0, len: 2, index: 0, imm: imm as i32, off: 0, step: None, link: Cell::new(0) }
    }

    #[inline(always)]
    const fn c_illegal(raw: u16) -> DInsn {
        DInsn { op: Op::Illegal, rd: 0, rs1: 0, rs2: 0, rs3: 0, f: 0, len: 2, index: 0, imm: raw as i32, off: 0, step: None, link: Cell::new(0) }
    }
}

/// Cache only ISA decoding. Virtual addresses, source offsets, forwarding,
/// and block links are assigned independently when a block is built.
pub(crate) struct DecodeCache(Box<[DecodeEntry]>);

const DECODE_SETS: usize = 2048;

// RV64C has a small, fixed encoding space. The shared read-only table avoids
// hashing compressed instructions and leaves the per-hart cache to RV64 words.
#[derive(Clone, Copy)]
struct Compressed {
    imm: i32,
    op: Op,
    rd: u8,
    rs1: u8,
    rs2: u8,
}

const _: () = assert!(std::mem::size_of::<Compressed>() == 8);

static COMPRESSED: [Compressed; 1 << 16] = {
    let empty = Compressed { imm: 0, op: Op::Illegal, rd: 0, rs1: 0, rs2: 0 };
    let mut table = [empty; 1 << 16];
    let mut i = 0;
    while i < table.len() {
        if i & 3 != 3 {
            let d = decode16(i as u16);
            table[i] = Compressed { imm: d.imm, op: d.op, rd: d.rd, rs1: d.rs1, rs2: d.rs2 };
        }
        i += 1;
    }
    table
};

#[derive(Clone, Copy)]
struct DecodeEntry {
    word: u32,
    imm: i32,
    op: Op,
    rd: u8,
    rs1: u8,
    rs2: u8,
    rs3: u8,
    f: u8,
    len: u8,
}

const _: () = assert!(std::mem::size_of::<DecodeEntry>() == 16);

impl Default for DecodeCache {
    fn default() -> Self {
        // A 32-bit instruction ends in 11, so zero can't match a decoded key.
        let empty = DecodeEntry {
            word: 0, imm: 0, op: Op::Empty, rd: 0, rs1: 0,
            rs2: 0, rs3: 0, f: 0, len: 0,
        };
        Self(vec![empty; DECODE_SETS].into_boxed_slice())
    }
}

impl DecodeCache {
    #[inline(always)]
    pub(crate) fn decode(&mut self, word: u32) -> DInsn {
        if word & 3 != 3 {
            let d = COMPRESSED[word as u16 as usize];
            return DInsn::c(d.op, d.rd, d.rs1, d.rs2, i64::from(d.imm));
        }
        let index = ((word ^ (word >> 11) ^ (word >> 22)) as usize) & (DECODE_SETS - 1);
        let entry = &mut self.0[index];
        if entry.word == word {
            return DInsn {
                op: entry.op, rd: entry.rd, rs1: entry.rs1, rs2: entry.rs2, rs3: entry.rs3,
                f: entry.f, len: entry.len, imm: entry.imm, ..DInsn::EMPTY
            };
        }
        let d = decode32(word);
        *entry = DecodeEntry {
            word, imm: d.imm, op: d.op, rd: d.rd, rs1: d.rs1,
            rs2: d.rs2, rs3: d.rs3, f: d.f, len: d.len,
        };
        d
    }
}

impl Op {
    /// Whether this instruction ends a basic block: control transfers, system
    /// instructions (which can change interrupt state or read the counters),
    /// fences, and illegal encodings.
    pub fn ends_block(self) -> bool {
        matches!(
            self,
            Op::Jal
                | Op::Jalr
                | Op::Beq
                | Op::Bne
                | Op::Blt
                | Op::Bge
                | Op::Bltu
                | Op::Bgeu
                | Op::Ecall
                | Op::Ebreak
                | Op::Mret
                | Op::Sret
                | Op::Wfi
                | Op::SfenceVma
                | Op::Csr
                | Op::FenceI
                | Op::FenceIMt
                | Op::Illegal
        )
    }
}

#[inline(always)]
const fn sext(x: u32, bits: u32) -> i64 {
    (((x << (32 - bits)) as i32) >> (32 - bits)) as i64
}

/// Decode a 32-bit (uncompressed) instruction.
pub fn decode32(w: u32) -> DInsn {
    let opcode = w & 0x7f;
    let rd = ((w >> 7) & 0x1f) as u8;
    let f3 = ((w >> 12) & 7) as u8;
    let rs1 = ((w >> 15) & 0x1f) as u8;
    let rs2 = ((w >> 20) & 0x1f) as u8;
    let f7 = (w >> 25) & 0x7f;
    let sw = w as i32;
    let i_imm = (sw >> 20) as i64;
    let s_imm = (((sw >> 25) << 5) | ((w >> 7) & 0x1f) as i32) as i64;
    let b_imm = (((sw >> 31) << 12)
        | (((w >> 7) & 1) << 11) as i32
        | (((w >> 25) & 0x3f) << 5) as i32
        | (((w >> 8) & 0xf) << 1) as i32) as i64;
    let u_imm = (w & 0xffff_f000) as i32 as i64;
    let j_imm = ((sw >> 31) << 20
        | (((w >> 12) & 0xff) << 12) as i32
        | (((w >> 20) & 1) << 11) as i32
        | (((w >> 21) & 0x3ff) << 1) as i32) as i64;

    match opcode {
        0x37 => DInsn::new(Op::Lui, rd, 0, 0, 0, u_imm),
        0x17 => DInsn::new(Op::Auipc, rd, 0, 0, 0, u_imm),
        0x6f => DInsn::new(Op::Jal, rd, 0, 0, 0, j_imm),
        0x67 if f3 == 0 => DInsn::new(Op::Jalr, rd, rs1, 0, 0, i_imm),
        0x63 => {
            let op = match f3 {
                0 => Op::Beq,
                1 => Op::Bne,
                4 => Op::Blt,
                5 => Op::Bge,
                6 => Op::Bltu,
                7 => Op::Bgeu,
                _ => return DInsn::illegal(w),
            };
            DInsn::new(op, 0, rs1, rs2, f3, b_imm)
        }
        0x03 => {
            let op = match f3 {
                0 => Op::Lb,
                1 => Op::Lh,
                2 => Op::Lw,
                3 => Op::Ld,
                4 => Op::Lbu,
                5 => Op::Lhu,
                6 => Op::Lwu,
                _ => return DInsn::illegal(w),
            };
            DInsn::new(op, rd, rs1, 0, f3, i_imm)
        }
        0x23 => {
            let op = match f3 {
                0 => Op::Sb,
                1 => Op::Sh,
                2 => Op::Sw,
                3 => Op::Sd,
                _ => return DInsn::illegal(w),
            };
            DInsn::new(op, 0, rs1, rs2, f3, s_imm)
        }
        0x13 => {
            let shamt = ((w >> 20) & 0x3f) as i64;
            match f3 {
                0 => DInsn::new(Op::Addi, rd, rs1, 0, 0, i_imm),
                1 if (w >> 26) == 0 => DInsn::new(Op::Slli, rd, rs1, 0, 1, shamt),
                2 => DInsn::new(Op::Slti, rd, rs1, 0, 2, i_imm),
                3 => DInsn::new(Op::Sltiu, rd, rs1, 0, 3, i_imm),
                4 => DInsn::new(Op::Xori, rd, rs1, 0, 4, i_imm),
                5 if (w >> 26) == 0 => DInsn::new(Op::Srli, rd, rs1, 0, 5, shamt),
                5 if (w >> 26) == 0x10 => DInsn::new(Op::Srai, rd, rs1, 0, 5, shamt),
                6 => DInsn::new(Op::Ori, rd, rs1, 0, 6, i_imm),
                7 => DInsn::new(Op::Andi, rd, rs1, 0, 7, i_imm),
                _ => DInsn::illegal(w),
            }
        }
        0x1b => {
            let shamt = ((w >> 20) & 0x1f) as i64;
            match (f3, f7) {
                (0, _) => DInsn::new(Op::Addiw, rd, rs1, 0, 0, i_imm),
                (1, 0) => DInsn::new(Op::Slliw, rd, rs1, 0, 1, shamt),
                (5, 0) => DInsn::new(Op::Srliw, rd, rs1, 0, 5, shamt),
                (5, 0x20) => DInsn::new(Op::Sraiw, rd, rs1, 0, 5, shamt),
                _ => DInsn::illegal(w),
            }
        }
        0x33 => {
            let op = match (f7, f3) {
                (0x00, 0) => Op::Add,
                (0x20, 0) => Op::Sub,
                (0x00, 1) => Op::Sll,
                (0x00, 2) => Op::Slt,
                (0x00, 3) => Op::Sltu,
                (0x00, 4) => Op::Xor,
                (0x00, 5) => Op::Srl,
                (0x20, 5) => Op::Sra,
                (0x00, 6) => Op::Or,
                (0x00, 7) => Op::And,
                (0x01, 0) => Op::Mul,
                (0x01, 1) => Op::Mulh,
                (0x01, 2) => Op::Mulhsu,
                (0x01, 3) => Op::Mulhu,
                (0x01, 4) => Op::Div,
                (0x01, 5) => Op::Divu,
                (0x01, 6) => Op::Rem,
                (0x01, 7) => Op::Remu,
                _ => return DInsn::illegal(w),
            };
            DInsn::new(op, rd, rs1, rs2, f3, 0)
        }
        0x3b => {
            let op = match (f7, f3) {
                (0x00, 0) => Op::Addw,
                (0x20, 0) => Op::Subw,
                (0x00, 1) => Op::Sllw,
                (0x00, 5) => Op::Srlw,
                (0x20, 5) => Op::Sraw,
                (0x01, 0) => Op::Mulw,
                (0x01, 4) => Op::Divw,
                (0x01, 5) => Op::Divuw,
                (0x01, 6) => Op::Remw,
                (0x01, 7) => Op::Remuw,
                _ => return DInsn::illegal(w),
            };
            DInsn::new(op, rd, rs1, rs2, f3, 0)
        }
        0x0f => match f3 {
            0 => DInsn::new(Op::Fence, 0, 0, 0, 0, 0),
            1 => DInsn::new(Op::FenceI, 0, 0, 0, 1, 0),
            _ => DInsn::illegal(w),
        },
        0x73 => {
            if f3 == 0 {
                match w {
                    0x0000_0073 => DInsn::new(Op::Ecall, 0, 0, 0, 0, 0),
                    0x0010_0073 => DInsn::new(Op::Ebreak, 0, 0, 0, 0, 0),
                    0x1020_0073 => DInsn::new(Op::Sret, 0, 0, 0, 0, 0),
                    0x3020_0073 => DInsn::new(Op::Mret, 0, 0, 0, 0, 0),
                    0x1050_0073 => DInsn::new(Op::Wfi, 0, 0, 0, 0, 0),
                    _ if f7 == 0x09 && rd == 0 => DInsn::new(Op::SfenceVma, 0, rs1, rs2, 0, 0),
                    _ => DInsn::illegal(w),
                }
            } else if f3 == 4 {
                DInsn::illegal(w)
            } else {
                // imm = CSR address; rs1 doubles as the 5-bit zimm for the *I forms
                DInsn::new(Op::Csr, rd, rs1, 0, f3, (w >> 20) as i64)
            }
        }
        0x2f => {
            if f3 != 2 && f3 != 3 {
                return DInsn::illegal(w);
            }
            let funct5 = w >> 27;
            match funct5 {
                0x02 if rs2 == 0 => DInsn::new(Op::Lr, rd, rs1, 0, f3, 0),
                0x03 => DInsn::new(Op::Sc, rd, rs1, rs2, f3, 0),
                0x00 | 0x01 | 0x04 | 0x08 | 0x0c | 0x10 | 0x14 | 0x18 | 0x1c => {
                    DInsn::new(Op::Amo, rd, rs1, rs2, f3, funct5 as i64)
                }
                _ => DInsn::illegal(w),
            }
        }
        0x07 => match f3 {
            2 => DInsn::new(Op::Flw, rd, rs1, 0, 2, i_imm),
            3 => DInsn::new(Op::Fld, rd, rs1, 0, 3, i_imm),
            _ => DInsn::illegal(w),
        },
        0x27 => match f3 {
            2 => DInsn::new(Op::Fsw, 0, rs1, rs2, 2, s_imm),
            3 => DInsn::new(Op::Fsd, 0, rs1, rs2, 3, s_imm),
            _ => DInsn::illegal(w),
        },
        0x43 | 0x47 | 0x4b | 0x4f => {
            let fmt = (w >> 25) & 3;
            if fmt > 1 {
                return DInsn::illegal(w);
            }
            let op = match opcode {
                0x43 => Op::Fmadd,
                0x47 => Op::Fmsub,
                0x4b => Op::Fnmsub,
                _ => Op::Fnmadd,
            };
            DInsn { op, rd, rs1, rs2, rs3: (w >> 27) as u8 & 31, f: f3, len: 4, index: 0, imm: fmt as i32, off: 0, step: None, link: Cell::new(0) }
        }
        0x53 => {
            let fmt = f7 & 3;
            let funct5 = f7 >> 2;
            if fmt > 1 {
                return DInsn::illegal(w);
            }
            let valid = matches!(funct5, 0x00..=0x05 | 0x08 | 0x0b | 0x14 | 0x18 | 0x1a | 0x1c | 0x1e);
            if !valid {
                return DInsn::illegal(w);
            }
            DInsn::new(Op::Fp, rd, rs1, rs2, f3, (funct5 | (fmt << 5)) as i64)
        }
        _ => DInsn::illegal(w),
    }
}

/// Decode a 16-bit compressed (RVC) instruction into its base equivalent.
pub const fn decode16(h: u16) -> DInsn {
    let h = h as u32;
    let q = h & 3;
    let f3 = (h >> 13) & 7;
    // Register fields of the 3-bit compressed set map onto x8..x15.
    let rp = (((h >> 2) & 7) + 8) as u8; // rd'/rs2'
    let rp1 = (((h >> 7) & 7) + 8) as u8; // rs1'/rd' (CL/CS/CB/CA)
    let rfull = ((h >> 7) & 0x1f) as u8;
    let rs2full = ((h >> 2) & 0x1f) as u8;

    match (q, f3) {
        // ------------------------------------------------ quadrant 0
        (0, 0) => {
            let nzuimm = ((h >> 11) & 3) << 4 | ((h >> 7) & 0xf) << 6 | ((h >> 6) & 1) << 2 | ((h >> 5) & 1) << 3;
            if nzuimm == 0 {
                return DInsn::c_illegal(h as u16);
            }
            DInsn::c(Op::Addi, rp, 2, 0, nzuimm as i64)
        }
        (0, 1) => {
            let uimm = ((h >> 10) & 7) << 3 | ((h >> 5) & 3) << 6;
            DInsn::c(Op::Fld, rp, rp1, 0, uimm as i64)
        }
        (0, 2) => {
            let uimm = ((h >> 10) & 7) << 3 | ((h >> 6) & 1) << 2 | ((h >> 5) & 1) << 6;
            DInsn::c(Op::Lw, rp, rp1, 0, uimm as i64)
        }
        (0, 3) => {
            let uimm = ((h >> 10) & 7) << 3 | ((h >> 5) & 3) << 6;
            DInsn::c(Op::Ld, rp, rp1, 0, uimm as i64)
        }
        (0, 5) => {
            let uimm = ((h >> 10) & 7) << 3 | ((h >> 5) & 3) << 6;
            DInsn::c(Op::Fsd, 0, rp1, rp, uimm as i64)
        }
        (0, 6) => {
            let uimm = ((h >> 10) & 7) << 3 | ((h >> 6) & 1) << 2 | ((h >> 5) & 1) << 6;
            DInsn::c(Op::Sw, 0, rp1, rp, uimm as i64)
        }
        (0, 7) => {
            let uimm = ((h >> 10) & 7) << 3 | ((h >> 5) & 3) << 6;
            DInsn::c(Op::Sd, 0, rp1, rp, uimm as i64)
        }
        // ------------------------------------------------ quadrant 1
        (1, 0) => {
            let imm = sext((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
            DInsn::c(Op::Addi, rfull, rfull, 0, imm)
        }
        (1, 1) => {
            if rfull == 0 {
                return DInsn::c_illegal(h as u16);
            }
            let imm = sext((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
            DInsn::c(Op::Addiw, rfull, rfull, 0, imm)
        }
        (1, 2) => {
            let imm = sext((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
            DInsn::c(Op::Addi, rfull, 0, 0, imm)
        }
        (1, 3) => {
            if rfull == 2 {
                let nz = (((h >> 12) & 1) << 9)
                    | (((h >> 3) & 3) << 7)
                    | (((h >> 5) & 1) << 6)
                    | (((h >> 2) & 1) << 5)
                    | (((h >> 6) & 1) << 4);
                if nz == 0 {
                    return DInsn::c_illegal(h as u16);
                }
                DInsn::c(Op::Addi, 2, 2, 0, sext(nz, 10))
            } else {
                let nz = (((h >> 12) & 1) << 17) | (((h >> 2) & 0x1f) << 12);
                if nz == 0 || rfull == 0 {
                    return DInsn::c_illegal(h as u16);
                }
                DInsn::c(Op::Lui, rfull, 0, 0, sext(nz, 18))
            }
        }
        (1, 4) => {
            let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f);
            match (h >> 10) & 3 {
                0 => DInsn::c(Op::Srli, rp1, rp1, 0, shamt as i64),
                1 => DInsn::c(Op::Srai, rp1, rp1, 0, shamt as i64),
                2 => {
                    let imm = sext((((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f), 6);
                    DInsn::c(Op::Andi, rp1, rp1, 0, imm)
                }
                _ => {
                    let sub = (h >> 5) & 3;
                    if (h >> 12) & 1 == 0 {
                        let op = match sub {
                            0 => Op::Sub,
                            1 => Op::Xor,
                            2 => Op::Or,
                            _ => Op::And,
                        };
                        DInsn::c(op, rp1, rp1, rp, 0)
                    } else {
                        match sub {
                            0 => DInsn::c(Op::Subw, rp1, rp1, rp, 0),
                            1 => DInsn::c(Op::Addw, rp1, rp1, rp, 0),
                            _ => DInsn::c_illegal(h as u16),
                        }
                    }
                }
            }
        }
        (1, 5) => {
            // C.J: imm[11|4|9:8|10|6|7|3:1|5]
            let raw = ((h >> 12) & 1) << 11
                | ((h >> 11) & 1) << 4
                | ((h >> 9) & 3) << 8
                | ((h >> 8) & 1) << 10
                | ((h >> 7) & 1) << 6
                | ((h >> 6) & 1) << 7
                | ((h >> 3) & 7) << 1
                | ((h >> 2) & 1) << 5;
            DInsn::c(Op::Jal, 0, 0, 0, sext(raw, 12))
        }
        (1, 6) | (1, 7) => {
            // C.BEQZ / C.BNEZ: imm[8|4:3] rs1' imm[7:6|2:1|5]
            let raw = ((h >> 12) & 1) << 8
                | ((h >> 10) & 3) << 3
                | ((h >> 5) & 3) << 6
                | ((h >> 3) & 3) << 1
                | ((h >> 2) & 1) << 5;
            let op = if f3 == 6 { Op::Beq } else { Op::Bne };
            DInsn::c(op, 0, rp1, 0, sext(raw, 9))
        }
        // ------------------------------------------------ quadrant 2
        (2, 0) => {
            let shamt = (((h >> 12) & 1) << 5) | ((h >> 2) & 0x1f);
            DInsn::c(Op::Slli, rfull, rfull, 0, shamt as i64)
        }
        (2, 1) => {
            let uimm = (((h >> 12) & 1) << 5) | (((h >> 5) & 3) << 3) | (((h >> 2) & 7) << 6);
            DInsn::c(Op::Fld, rfull, 2, 0, uimm as i64)
        }
        (2, 2) => {
            if rfull == 0 {
                return DInsn::c_illegal(h as u16);
            }
            let uimm = (((h >> 12) & 1) << 5) | (((h >> 4) & 7) << 2) | (((h >> 2) & 3) << 6);
            DInsn::c(Op::Lw, rfull, 2, 0, uimm as i64)
        }
        (2, 3) => {
            if rfull == 0 {
                return DInsn::c_illegal(h as u16);
            }
            let uimm = (((h >> 12) & 1) << 5) | (((h >> 5) & 3) << 3) | (((h >> 2) & 7) << 6);
            DInsn::c(Op::Ld, rfull, 2, 0, uimm as i64)
        }
        (2, 4) => {
            if (h >> 12) & 1 == 0 {
                if rs2full == 0 {
                    if rfull == 0 {
                        return DInsn::c_illegal(h as u16);
                    }
                    // C.JR
                    DInsn::c(Op::Jalr, 0, rfull, 0, 0)
                } else {
                    // C.MV
                    DInsn::c(Op::Add, rfull, 0, rs2full, 0)
                }
            } else if rs2full == 0 {
                if rfull == 0 {
                    // C.EBREAK
                    DInsn::c(Op::Ebreak, 0, 0, 0, 0)
                } else {
                    // C.JALR
                    DInsn::c(Op::Jalr, 1, rfull, 0, 0)
                }
            } else {
                // C.ADD
                DInsn::c(Op::Add, rfull, rfull, rs2full, 0)
            }
        }
        (2, 5) => {
            let uimm = (((h >> 10) & 7) << 3) | (((h >> 7) & 7) << 6);
            DInsn::c(Op::Fsd, 0, 2, rs2full, uimm as i64)
        }
        (2, 6) => {
            let uimm = (((h >> 9) & 0xf) << 2) | (((h >> 7) & 3) << 6);
            DInsn::c(Op::Sw, 0, 2, rs2full, uimm as i64)
        }
        (2, 7) => {
            let uimm = (((h >> 10) & 7) << 3) | (((h >> 7) & 7) << 6);
            DInsn::c(Op::Sd, 0, 2, rs2full, uimm as i64)
        }
        _ => DInsn::c_illegal(h as u16),
    }
}

/// Decode the instruction starting with halfword `lo`. `hi` is only consulted
/// when `lo` begins a 32-bit encoding.
#[inline]
pub fn decode_parts(lo: u16, hi: impl FnOnce() -> Option<u16>) -> Option<DInsn> {
    if lo & 3 != 3 {
        Some(decode16(lo))
    } else {
        let hi = hi()?;
        Some(decode32((hi as u32) << 16 | lo as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_compressed_decode_ignores_the_next_halfword() {
        let mut cache = DecodeCache::default();
        for halfword in 0..=u16::MAX {
            if halfword & 3 == 3 {
                continue;
            }
            let d = decode16(halfword);
            let cached = cache.decode(0xfedc_0000 | u32::from(halfword));
            assert_eq!(
                (d.op, d.rd, d.rs1, d.rs2, d.rs3, d.f, d.imm, d.len),
                (cached.op, cached.rd, cached.rs1, cached.rs2, cached.rs3, cached.f, cached.imm, cached.len),
            );
        }
    }

    #[test]
    fn decodes_addi() {
        // addi a0, a1, -1  => imm=0xfff rs1=11 f3=0 rd=10 opcode=0x13
        let w = (0xfffu32 << 20) | (11 << 15) | (10 << 7) | 0x13;
        let d = decode32(w);
        assert_eq!(d.op, Op::Addi);
        assert_eq!((d.rd, d.rs1, d.imm), (10, 11, -1));
    }

    #[test]
    fn decodes_branch_offset() {
        // beq x1, x2, -8
        let imm: i32 = -8;
        let u = imm as u32;
        let w = (((u >> 12) & 1) << 31)
            | (((u >> 5) & 0x3f) << 25)
            | (2 << 20)
            | (1 << 15)
            | (((u >> 1) & 0xf) << 8)
            | (((u >> 11) & 1) << 7)
            | 0x63;
        let d = decode32(w);
        assert_eq!(d.op, Op::Beq);
        assert_eq!(d.imm, -8);
    }

    #[test]
    fn decodes_jal_offset() {
        // jal x1, -2048 (fits J range)
        let imm: i32 = -2048;
        let u = imm as u32;
        let w = (((u >> 20) & 1) << 31)
            | (((u >> 1) & 0x3ff) << 21)
            | (((u >> 11) & 1) << 20)
            | (((u >> 12) & 0xff) << 12)
            | (1 << 7)
            | 0x6f;
        let d = decode32(w);
        assert_eq!(d.op, Op::Jal);
        assert_eq!(d.imm, -2048);
    }

    #[test]
    fn decodes_c_addi() {
        // c.addi a0, -3 : funct3=000 imm[5]=1 rd=10 imm[4:0]=11101 op=01
        let h: u16 = (1 << 12) | (10 << 7) | (0b11101 << 2) | 0b01;
        let d = decode16(h);
        assert_eq!(d.op, Op::Addi);
        assert_eq!((d.rd, d.rs1, d.imm, d.len), (10, 10, -3, 2));
    }

    #[test]
    fn decodes_c_j_negative() {
        // c.j -2 : offset -2 => imm[11..1]=all ones except bit0.
        // Encode: funct3=101, bits per CJ format for -2.
        let imm: i32 = -2;
        let u = imm as u32;
        let h: u16 = ((0b101u32 << 13)
            | (((u >> 11) & 1) << 12)
            | (((u >> 4) & 1) << 11)
            | (((u >> 8) & 3) << 9)
            | (((u >> 10) & 1) << 8)
            | (((u >> 6) & 1) << 7)
            | (((u >> 7) & 1) << 6)
            | (((u >> 1) & 7) << 3)
            | (((u >> 5) & 1) << 2)
            | 0b01) as u16;
        let d = decode16(h);
        assert_eq!(d.op, Op::Jal);
        assert_eq!(d.imm, -2);
        assert_eq!(d.len, 2);
    }

    #[test]
    fn unknown_is_illegal() {
        assert_eq!(decode32(0).op, Op::Illegal);
        assert_eq!(decode16(0).op, Op::Illegal);
    }
}
