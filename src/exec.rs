//! Instruction execution for the integer, M, A and system instruction sets.
//! Floating point lives in `fpu.rs`.

use crate::arch::*;
use crate::bus::Bus;
use crate::hart::{Hart, Res, Trap};
use crate::insn::{DInsn, Op};
use crate::mmu::Access;

#[inline(always)]
fn sext32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

impl Hart {
    #[inline(always)]
    pub(crate) fn rx(&self, r: u8) -> u64 {
        self.x[(r & 31) as usize]
    }

    /// `rx` for register numbers already known to be below 32, which the block
    /// engine checks when it builds a block.
    #[inline(always)]
    pub(crate) fn rx_u(&self, r: u8) -> u64 {
        debug_assert!(r < 32);
        // SAFETY: `r` is below 32 (see above), the length of `x`.
        unsafe { *self.x.get_unchecked(r as usize) }
    }

    /// Read register `R`, which the caller knows at compile time. The address
    /// needs no index load at all, which is what generated superinstructions
    /// use for every operand.
    #[inline(always)]
    pub(crate) fn rx_c<const R: u8>(&self) -> u64 {
        debug_assert!(R < 32);
        if R == 0 {
            return 0;
        }
        // SAFETY: `R` is below 32, the length of `x`.
        unsafe { *self.x.get_unchecked(R as usize) }
    }

    /// Write register `R`, known at compile time and known non-zero.
    #[inline(always)]
    pub(crate) fn set_x_c<const R: u8>(&mut self, v: u64) {
        debug_assert!(R != 0 && R < 32);
        // SAFETY: `R` is in 1..32, the length of `x`.
        unsafe { *self.x.get_unchecked_mut(R as usize) = v };
    }

    /// Write a register that is known to be nonzero, with no test. The block
    /// engine uses this only for instructions whose destination is not `x0`.
    #[inline(always)]
    pub(crate) fn set_x(&mut self, r: u8, v: u64) {
        debug_assert!(r != 0 && r < 32);
        // SAFETY: `r` is in 1..32, the length of `x`.
        unsafe { *self.x.get_unchecked_mut(r as usize) = v };
    }

    /// `wx` for register numbers already known to be below 32.
    #[inline(always)]
    pub(crate) fn wx_u(&mut self, r: u8, v: u64) {
        debug_assert!(r < 32);
        if r != 0 {
            // SAFETY: `r` is below 32 (see above), the length of `x`.
            unsafe { *self.x.get_unchecked_mut(r as usize) = v };
        }
    }

    #[inline(always)]
    pub(crate) fn wx(&mut self, r: u8, v: u64) {
        if r != 0 {
            self.x[(r & 31) as usize] = v;
        }
    }

    /// Fault cause for a failed data access at `va` of width `n`.
    #[cold]
    fn data_fault(va: u64, n: usize, access: Access) -> Trap {
        let misaligned = va & (n as u64 - 1) != 0;
        let cause = match (access, misaligned) {
            (Access::Load, true) => EXC_LOAD_MISALIGN,
            (Access::Load, false) => EXC_LOAD_ACCESS,
            (_, true) => EXC_STORE_MISALIGN,
            (_, false) => EXC_STORE_ACCESS,
        };
        Trap::new(cause, va)
    }

    /// Load `N` bytes from virtual address `va`, zero-extended.
    #[inline(always)]
    pub(crate) fn load<const N: usize>(&mut self, bus: &mut Bus, va: u64) -> Res<u64> {
        // SAFETY: the current table has `FTLB_SETS` entries, and the index is below that.
        let e = unsafe { &*self.ltlb_cur.add(crate::hart::ftlb_index(va)) };
        if e.tag == va & (!0xfffu64 | (N as u64 - 1)) {
            // SAFETY: the entry maps this page to host RAM, and an aligned access
            // of `N` bytes stays inside the page.
            return Ok(unsafe { crate::block::host_read::<N>(va.wrapping_add(e.addend) as usize) });
        }
        self.load_general::<N>(bus, va)
    }

    /// A load that is not in the flat TLB: translate, read, and remember the page.
    #[inline(never)]
    pub(crate) fn load_general<const N: usize>(&mut self, bus: &mut Bus, va: u64) -> Res<u64> {
        if (va & 0xfff) + N as u64 <= 4096 {
            if let Some(host) = self.recover_load_mapping(va) {
                // SAFETY: the cached translation maps the whole page to RAM.
                return Ok(unsafe { crate::block::host_read::<N>(host) });
            }
            let pa = self.translate(bus, va, Access::Load)?;
            match bus.read::<N>(pa) {
                Some(v) => {
                    self.ftlb_fill(bus, va, pa, false);
                    Ok(v)
                }
                None => Err(Self::data_fault(va, N, Access::Load)),
            }
        } else {
            self.load_split::<N>(bus, va)
        }
    }

    /// Accesses that cross a page boundary are performed byte by byte so that
    /// each byte is translated through its own page.
    #[cold]
    fn load_split<const N: usize>(&mut self, bus: &mut Bus, va: u64) -> Res<u64> {
        let mut v = 0u64;
        for i in 0..N as u64 {
            let byte = va.wrapping_add(i);
            let pa = self.translate(bus, byte, Access::Load)?;
            let b = bus.read::<1>(pa).ok_or(Trap::new(EXC_LOAD_ACCESS, byte))?;
            v |= b << (8 * i);
        }
        Ok(v)
    }

    /// Store the low `N` bytes of `val` to virtual address `va`.
    #[inline(always)]
    pub(crate) fn store<const N: usize>(&mut self, bus: &mut Bus, va: u64, val: u64) -> Res<()> {
        // SAFETY: as for `load`.
        let e = unsafe { &*self.stlb_cur.add(crate::hart::ftlb_index(va)) };
        if e.tag == va & (!0xfffu64 | (N as u64 - 1)) {
            // SAFETY: as for `load`; the store table only has pages without code.
            unsafe { crate::block::host_write::<N>(va.wrapping_add(e.addend) as usize, val) };
            return Ok(());
        }
        self.store_general::<N>(bus, va, val)
    }

    /// A store that is not in the flat TLB: translate, write, and remember the page.
    #[inline(never)]
    pub(crate) fn store_general<const N: usize>(&mut self, bus: &mut Bus, va: u64, val: u64) -> Res<()> {
        if (va & 0xfff) + N as u64 <= 4096 {
            let pa = match self.recover_store_mapping(bus, va) {
                Some(pa) => pa,
                None => self.translate(bus, va, Access::Store)?,
            };
            match bus.write::<N>(pa, val) {
                Some(()) => {
                    self.ftlb_fill(bus, va, pa, true);
                    // Writable Sv39 leaves are readable. Fill an empty load
                    // slot without displacing a live mapping on store streams.
                    let slot = unsafe { &*self.ltlb_cur.add(crate::hart::ftlb_index(va)) };
                    if slot.tag == u64::MAX {
                        self.ftlb_fill(bus, va, pa, false);
                    }
                    Ok(())
                }
                None => Err(Self::data_fault(va, N, Access::Store)),
            }
        } else {
            self.store_split::<N>(bus, va, val)
        }
    }

    #[cold]
    fn store_split<const N: usize>(&mut self, bus: &mut Bus, va: u64, val: u64) -> Res<()> {
        for i in 0..N as u64 {
            let byte = va.wrapping_add(i);
            let pa = self.translate(bus, byte, Access::Store)?;
            bus.write::<1>(pa, val >> (8 * i)).ok_or(Trap::new(EXC_STORE_ACCESS, byte))?;
        }
        Ok(())
    }

    /// Raw encoding of the instruction at `pc`, used as `mtval` for CSR
    /// instructions that trap as illegal.
    #[cold]
    fn raw_insn_at(&mut self, bus: &mut Bus, pc: u64) -> u64 {
        let lo = match self.translate(bus, pc, Access::Fetch).ok().and_then(|pa| bus.read::<2>(pa)) {
            Some(v) => v,
            None => return 0,
        };
        if lo & 3 != 3 {
            return lo;
        }
        match self.translate(bus, pc.wrapping_add(2), Access::Fetch).ok().and_then(|pa| bus.read::<2>(pa)) {
            Some(hi) => (hi << 16) | lo,
            None => lo,
        }
    }

    /// Execute one decoded instruction that is at `pc`.
    ///
    /// The hart's `pc` is not updated for sequential instructions: the caller
    /// has already set it to the fall-through address. Control transfers
    /// overwrite it with their target, and traps leave it to the caller.
    #[inline(never)]
    pub(crate) fn exec(&mut self, bus: &mut Bus, d: &DInsn, pc: u64) -> Res<()> {
        let next = pc.wrapping_add(d.len as u64);
        let (rd, rs1, rs2) = (d.rd, d.rs1, d.rs2);
        let imm = d.imm as i64;
        let a = self.rx(rs1);
        let b = self.rx(rs2);

        match d.op {
            Op::Lui => self.wx(rd, imm as u64),
            Op::Auipc => self.wx(rd, pc.wrapping_add(imm as u64)),
            Op::Jal => {
                self.wx(rd, next);
                self.pc = pc.wrapping_add(imm as u64);
            }
            Op::Jalr => {
                let target = a.wrapping_add(imm as u64) & !1;
                self.wx(rd, next);
                self.pc = target;
            }
            Op::Beq => {
                if a == b {
                    self.pc = pc.wrapping_add(imm as u64);
                }
            }
            Op::Bne => {
                if a != b {
                    self.pc = pc.wrapping_add(imm as u64);
                }
            }
            Op::Blt => {
                if (a as i64) < (b as i64) {
                    self.pc = pc.wrapping_add(imm as u64);
                }
            }
            Op::Bge => {
                if (a as i64) >= (b as i64) {
                    self.pc = pc.wrapping_add(imm as u64);
                }
            }
            Op::Bltu => {
                if a < b {
                    self.pc = pc.wrapping_add(imm as u64);
                }
            }
            Op::Bgeu => {
                if a >= b {
                    self.pc = pc.wrapping_add(imm as u64);
                }
            }

            // ---------------------------------------------- loads
            Op::Lb => {
                let v = self.load::<1>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v as i8 as i64 as u64);
            }
            Op::Lh => {
                let v = self.load::<2>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v as i16 as i64 as u64);
            }
            Op::Lw => {
                let v = self.load::<4>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v as i32 as i64 as u64);
            }
            Op::Ld => {
                let v = self.load::<8>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v);
            }
            Op::Lbu => {
                let v = self.load::<1>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v);
            }
            Op::Lhu => {
                let v = self.load::<2>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v);
            }
            Op::Lwu => {
                let v = self.load::<4>(bus, a.wrapping_add(imm as u64))?;
                self.wx(rd, v);
            }

            // ---------------------------------------------- stores
            Op::Sb => self.store::<1>(bus, a.wrapping_add(imm as u64), b)?,
            Op::Sh => self.store::<2>(bus, a.wrapping_add(imm as u64), b)?,
            Op::Sw => self.store::<4>(bus, a.wrapping_add(imm as u64), b)?,
            Op::Sd => self.store::<8>(bus, a.wrapping_add(imm as u64), b)?,

            // ---------------------------------------------- OP-IMM
            Op::Addi => self.wx(rd, a.wrapping_add(imm as u64)),
            Op::Slti => self.wx(rd, ((a as i64) < imm) as u64),
            Op::Sltiu => self.wx(rd, (a < imm as u64) as u64),
            Op::Xori => self.wx(rd, a ^ imm as u64),
            Op::Ori => self.wx(rd, a | imm as u64),
            Op::Andi => self.wx(rd, a & imm as u64),
            Op::Slli => self.wx(rd, a << (imm as u32 & 63)),
            Op::Srli => self.wx(rd, a >> (imm as u32 & 63)),
            Op::Srai => self.wx(rd, ((a as i64) >> (imm as u32 & 63)) as u64),
            Op::Addiw => self.wx(rd, sext32((a as u32).wrapping_add(imm as u32))),
            Op::Slliw => self.wx(rd, sext32((a as u32) << (imm as u32 & 31))),
            Op::Srliw => self.wx(rd, sext32((a as u32) >> (imm as u32 & 31))),
            Op::Sraiw => self.wx(rd, ((a as u32 as i32) >> (imm as u32 & 31)) as i64 as u64),

            // ---------------------------------------------- OP
            Op::Add => self.wx(rd, a.wrapping_add(b)),
            Op::Sub => self.wx(rd, a.wrapping_sub(b)),
            Op::Sll => self.wx(rd, a << (b & 63)),
            Op::Slt => self.wx(rd, ((a as i64) < (b as i64)) as u64),
            Op::Sltu => self.wx(rd, (a < b) as u64),
            Op::Xor => self.wx(rd, a ^ b),
            Op::Srl => self.wx(rd, a >> (b & 63)),
            Op::Sra => self.wx(rd, ((a as i64) >> (b & 63)) as u64),
            Op::Or => self.wx(rd, a | b),
            Op::And => self.wx(rd, a & b),
            Op::Addw => self.wx(rd, sext32((a as u32).wrapping_add(b as u32))),
            Op::Subw => self.wx(rd, sext32((a as u32).wrapping_sub(b as u32))),
            Op::Sllw => self.wx(rd, sext32((a as u32) << (b & 31))),
            Op::Srlw => self.wx(rd, sext32((a as u32) >> (b & 31))),
            Op::Sraw => self.wx(rd, ((a as u32 as i32) >> (b & 31)) as i64 as u64),

            // ---------------------------------------------- M extension
            Op::Mul => self.wx(rd, a.wrapping_mul(b)),
            Op::Mulh => self.wx(rd, (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64),
            Op::Mulhsu => self.wx(rd, (((a as i64 as i128) * (b as i128)) >> 64) as u64),
            Op::Mulhu => self.wx(rd, (((a as u128) * (b as u128)) >> 64) as u64),
            Op::Div => {
                let (x, y) = (a as i64, b as i64);
                let r = if y == 0 {
                    u64::MAX
                } else if x == i64::MIN && y == -1 {
                    x as u64
                } else {
                    (x / y) as u64
                };
                self.wx(rd, r);
            }
            Op::Divu => self.wx(rd, a.checked_div(b).unwrap_or(u64::MAX)),
            Op::Rem => {
                let (x, y) = (a as i64, b as i64);
                let r = if y == 0 {
                    a
                } else if x == i64::MIN && y == -1 {
                    0
                } else {
                    (x % y) as u64
                };
                self.wx(rd, r);
            }
            Op::Remu => self.wx(rd, if b == 0 { a } else { a % b }),
            Op::Mulw => self.wx(rd, sext32((a as u32).wrapping_mul(b as u32))),
            Op::Divw => {
                let (x, y) = (a as i32, b as i32);
                let r = if y == 0 {
                    -1i32
                } else if x == i32::MIN && y == -1 {
                    x
                } else {
                    x / y
                };
                self.wx(rd, r as i64 as u64);
            }
            Op::Divuw => {
                let (x, y) = (a as u32, b as u32);
                self.wx(rd, sext32(x.checked_div(y).unwrap_or(u32::MAX)));
            }
            Op::Remw => {
                let (x, y) = (a as i32, b as i32);
                let r = if y == 0 {
                    x
                } else if x == i32::MIN && y == -1 {
                    0
                } else {
                    x % y
                };
                self.wx(rd, r as i64 as u64);
            }
            Op::Remuw => {
                let (x, y) = (a as u32, b as u32);
                self.wx(rd, sext32(if y == 0 { x } else { x % y }));
            }

            // ---------------------------------------------- misc-mem / system
            Op::Fence | Op::FenceI => {}
            Op::FenceMt => std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst),
            Op::FenceIMt => {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                self.sync_code_marks(bus);
            }
            Op::Ecall => {
                if self.hle && self.prv == PRV_S {
                    self.sbi_call(bus);
                } else {
                    let cause = match self.prv {
                        PRV_U => EXC_ECALL_U,
                        PRV_S => EXC_ECALL_S,
                        _ => EXC_ECALL_M,
                    };
                    return Err(Trap::new(cause, 0));
                }
            }
            Op::Ebreak => return Err(Trap::new(EXC_BREAKPOINT, pc)),
            Op::Mret => {
                if self.prv < PRV_M {
                    return Err(self.illegal_here(bus, pc));
                }
                self.xret(true);
                return Ok(());
            }
            Op::Sret => {
                if self.prv < PRV_S || (self.prv == PRV_S && self.mstatus & MSTATUS_TSR != 0) {
                    return Err(self.illegal_here(bus, pc));
                }
                self.xret(false);
                return Ok(());
            }
            Op::Wfi => {
                if self.prv == PRV_U || (self.prv == PRV_S && self.mstatus & MSTATUS_TW != 0) {
                    return Err(self.illegal_here(bus, pc));
                }
                self.wfi = true;
                self.irq_dirty = true;
                self.break_chain();
            }
            Op::SfenceVma => {
                if self.prv == PRV_U || (self.prv == PRV_S && self.mstatus & MSTATUS_TVM != 0) {
                    return Err(self.illegal_here(bus, pc));
                }
                let va = if rs1 == 0 { None } else { Some(a) };
                let asid = if rs2 == 0 { None } else { Some(b as u16) };
                self.sfence_vma(va, asid);
            }
            Op::Csr => {
                let addr = imm as u16;
                let op = d.f & 3;
                // The immediate forms take their operand from the rs1 field.
                let src = if d.f >= 4 { rs1 as u64 } else { a };
                let write = op == 1 || rs1 != 0;
                let read = !(op == 1 && rd == 0);
                crate::counters::inc_csr(addr, if !write { 0 } else { op as usize });
                if self.csr_check(addr, write).is_err() {
                    return Err(self.illegal_here(bus, pc));
                }
                let old = if read {
                    match self.csr_read(bus, addr) {
                        Ok(v) => v,
                        Err(_) => return Err(self.illegal_here(bus, pc)),
                    }
                } else {
                    0
                };
                if write {
                    let new = match op {
                        1 => src,
                        2 => old | src,
                        _ => old & !src,
                    };
                    if self.csr_write(addr, new).is_err() {
                        return Err(self.illegal_here(bus, pc));
                    }
                }
                self.wx(rd, old);
            }

            // ---------------------------------------------- A extension
            Op::Lr => {
                let w = d.f == 2;
                let n = if w { 4 } else { 8 };
                if a & (n - 1) != 0 {
                    return Err(Trap::new(EXC_LOAD_MISALIGN, a));
                }
                let pa = self.translate(bus, a, Access::Load)?;
                let raw = if w {
                    bus.read::<4>(pa).ok_or(Self::data_fault(a, 4, Access::Load))?
                } else {
                    bus.read::<8>(pa).ok_or(Self::data_fault(a, 8, Access::Load))?
                };
                self.reservation = Some(pa);
                self.reservation_val = raw;
                self.wx(rd, if w { raw as u32 as i32 as i64 as u64 } else { raw });
            }
            Op::Sc => {
                let w = d.f == 2;
                let n = if w { 4 } else { 8 };
                if a & (n - 1) != 0 {
                    return Err(Trap::new(EXC_STORE_MISALIGN, a));
                }
                let pa = self.translate(bus, a, Access::Store)?;
                let mut success = self.reservation == Some(pa);
                self.reservation = None;
                if success {
                    if bus.mt.is_some() {
                        success = if w {
                            bus.cas::<4>(pa, self.reservation_val, b)
                        } else {
                            bus.cas::<8>(pa, self.reservation_val, b)
                        }
                        .ok_or(Self::data_fault(a, if w { 4 } else { 8 }, Access::Store))?;
                    } else if w {
                        bus.write::<4>(pa, b).ok_or(Self::data_fault(a, 4, Access::Store))?;
                    } else {
                        bus.write::<8>(pa, b).ok_or(Self::data_fault(a, 8, Access::Store))?;
                    }
                }
                self.wx(rd, if success { 0 } else { 1 });
            }
            Op::Amo => {
                let w = d.f == 2;
                let n = if w { 4 } else { 8 };
                if a & (n - 1) != 0 {
                    return Err(Trap::new(EXC_STORE_MISALIGN, a));
                }
                let pa = self.translate(bus, a, Access::Store)?;
                let src = if w { b as u32 as i32 as i64 as u64 } else { b };
                let old = if bus.mt.is_some() {
                    if w {
                        bus.amo::<4>(pa, imm as i32, src)
                    } else {
                        bus.amo::<8>(pa, imm as i32, src)
                    }
                    .ok_or(Self::data_fault(a, if w { 4 } else { 8 }, Access::Store))?
                } else {
                    let old = if w {
                        bus.read::<4>(pa).ok_or(Self::data_fault(a, 4, Access::Store))? as u32 as i32 as i64 as u64
                    } else {
                        bus.read::<8>(pa).ok_or(Self::data_fault(a, 8, Access::Store))?
                    };
                    let new = crate::block::amo_value(imm as i32, old, src);
                    if w {
                        bus.write::<4>(pa, new).ok_or(Self::data_fault(a, 4, Access::Store))?;
                    } else {
                        bus.write::<8>(pa, new).ok_or(Self::data_fault(a, 8, Access::Store))?;
                    }
                    old
                };
                self.reservation = None;
                self.wx(rd, old);
            }

            // ---------------------------------------------- F / D
            Op::Illegal => return Err(Trap::new(EXC_ILLEGAL, d.imm as u32 as u64)),
            Op::Empty => return Err(Trap::new(EXC_ILLEGAL, 0)),
            _ => self.exec_fp(bus, d, pc)?,
        }

        Ok(())
    }

    /// Illegal-instruction trap for the instruction at `pc`.
    #[cold]
    fn illegal_here(&mut self, bus: &mut Bus, pc: u64) -> Trap {
        Trap::new(EXC_ILLEGAL, self.raw_insn_at(bus, pc))
    }
}
