//! Built-in supervisor binary interface (SBI).
//!
//! When no M-mode firmware is supplied, `ecall`s from S-mode are handled
//! here, on the host, instead of trapping into guest firmware. The calls
//! cover what Linux needs: base, timer, IPI, remote fence, hart state
//! management, system reset, debug console and the legacy v0.1 console.

use crate::arch::*;
use crate::bus::{Bus, Exit, Req};
use crate::hart::Hart;

const SBI_SUCCESS: i64 = 0;
const SBI_ERR_NOT_SUPPORTED: i64 = -2;
const SBI_ERR_INVALID_PARAM: i64 = -3;
const SBI_ERR_INVALID_ADDRESS: i64 = -5;
const SBI_ERR_ALREADY_AVAILABLE: i64 = -6;

const EID_LEGACY_MAX: u64 = 0x08;
const EID_BASE: u64 = 0x10;
const EID_TIME: u64 = 0x5449_4d45;
const EID_IPI: u64 = 0x0073_5049;
const EID_RFENCE: u64 = 0x5246_4e43;
const EID_HSM: u64 = 0x0048_534d;
const EID_SRST: u64 = 0x5352_5354;
const EID_DBCN: u64 = 0x4442_434e;

const MAX_DBCN_CHUNK: u64 = 4096;

fn supported(eid: u64) -> bool {
    eid <= EID_LEGACY_MAX || matches!(eid, EID_BASE | EID_TIME | EID_IPI | EID_RFENCE | EID_HSM | EID_SRST | EID_DBCN)
}

impl Hart {
    /// Service an `ecall` from S-mode. Results go back in `a0`/`a1`.
    pub(crate) fn sbi_call(&mut self, bus: &mut Bus) {
        let eid = self.x[17];
        let fid = self.x[16];
        let a0 = self.x[10];
        let a1 = self.x[11];
        let a2 = self.x[12];

        // With one host thread per hart the console, the hart states and the timer
        // registers are shared: hold the machine lock (a remote fence waits for other
        // harts and must not hold it).
        let mut guard = bus.mt.map(|m| m.lock.lock());
        let (err, val): (i64, u64) = match eid {
            // ------------------------------------------------ legacy v0.1
            0x00 => {
                self.set_timer(bus, a0);
                (0, 0)
            }
            0x01 => {
                bus.uart.tx.push(a0 as u8);
                (0, 0)
            }
            0x02 => {
                let c = bus.uart.rx.pop_front().map_or(-1i64, |b| b as i64);
                (c, 0)
            }
            0x03 => {
                self.mip &= !MIP_SSIP;
                (0, 0)
            }
            0x04 => {
                let mask = read_u64(bus, a0);
                self.send_ipi(bus, mask, 0);
                (0, 0)
            }
            0x05 => (0, 0),
            0x06 | 0x07 => {
                drop(guard.take());
                self.remote_fence(bus);
                (0, 0)
            }
            0x08 => {
                bus.exit = Some(Exit::Shutdown(0));
                (0, 0)
            }
            // ------------------------------------------------ v0.2+
            EID_BASE => match fid {
                0 => (SBI_SUCCESS, 2 << 24), // SBI spec 2.0
                1 => (SBI_SUCCESS, 0x7f),    // implementation id: this emulator
                2 => (SBI_SUCCESS, 1),       // implementation version
                3 => (SBI_SUCCESS, supported(a0) as u64),
                4..=6 => (SBI_SUCCESS, 0),   // mvendorid / marchid / mimpid
                _ => (SBI_ERR_NOT_SUPPORTED, 0),
            },
            EID_TIME => match fid {
                0 => {
                    self.set_timer(bus, a0);
                    (SBI_SUCCESS, 0)
                }
                _ => (SBI_ERR_NOT_SUPPORTED, 0),
            },
            EID_IPI => match fid {
                0 => {
                    self.send_ipi(bus, a0, a1);
                    (SBI_SUCCESS, 0)
                }
                _ => (SBI_ERR_NOT_SUPPORTED, 0),
            },
            EID_RFENCE => match fid {
                // remote_fence_i, remote_sfence_vma{,_asid}: every hart's TLB is
                // flushed. Harts never run concurrently here, so this is exact.
                0..=7 => {
                    drop(guard.take());
                    self.remote_fence(bus);
                    (SBI_SUCCESS, 0)
                }
                _ => (SBI_ERR_NOT_SUPPORTED, 0),
            },
            EID_HSM => self.hsm_call(bus, fid, a0, a1, a2),
            EID_SRST => match fid {
                0 => {
                    bus.exit = Some(match a0 {
                        0 => Exit::Shutdown(0),
                        1 | 2 => Exit::Reboot,
                        _ => return self.set_ret(SBI_ERR_NOT_SUPPORTED, 0),
                    });
                    (SBI_SUCCESS, 0)
                }
                _ => (SBI_ERR_NOT_SUPPORTED, 0),
            },
            EID_DBCN => self.dbcn_call(bus, fid, a0, a1, a2),
            _ => (SBI_ERR_NOT_SUPPORTED, 0),
        };

        if eid <= EID_LEGACY_MAX {
            // Legacy calls return a single value in a0.
            self.x[10] = err as u64;
        } else {
            self.set_ret(err, val);
        }
    }

    /// Flush this hart's translations and every other hart's. With one host thread the
    /// others are flushed before they run again; with a thread per hart this waits until
    /// each has done it, because the caller goes on to reuse the pages it unmapped.
    fn remote_fence(&mut self, bus: &mut Bus) {
        // The caller may have just written code the other harts will run.
        self.sync_code_marks(bus);
        self.sfence_vma(None, None);
        match bus.mt {
            Some(mt) => mt.flush_others(self),
            None => bus.reqs.push(Req::FlushTlbAll),
        }
    }

    fn set_ret(&mut self, err: i64, val: u64) {
        self.x[10] = err as u64;
        self.x[11] = val;
    }

    fn set_timer(&mut self, bus: &mut Bus, when: u64) {
        let id = self.id as usize;
        if let Some(cmp) = bus.clint.mtimecmp.get(id) {
            cmp.store(when, std::sync::atomic::Ordering::Relaxed);
        }
        self.mip &= !MIP_STIP;
        self.irq_dirty = true;
        self.break_chain();
    }

    fn send_ipi(&mut self, bus: &mut Bus, mask: u64, base: u64) {
        let harts = bus.clint.msip.len() as u64;
        // base == -1 means "all harts" in the v0.2 calling convention; the
        // legacy call always passes base 0 and a plain mask.
        for hart in 0..harts {
            let selected = if base == u64::MAX {
                true
            } else if hart >= base && hart - base < 64 {
                (mask >> (hart - base)) & 1 != 0
            } else {
                false
            };
            if !selected {
                continue;
            }
            if hart == self.id {
                self.mip |= MIP_SSIP;
                self.irq_dirty = true;
                self.break_chain();
            } else if let Some(mt) = bus.mt {
                mt.ssip(hart as usize);
            } else {
                bus.reqs.push(Req::Ssip { hart: hart as usize });
            }
        }
    }

    fn hsm_call(&mut self, bus: &mut Bus, fid: u64, a0: u64, a1: u64, a2: u64) -> (i64, u64) {
        let nharts = bus.hart_stopped.len() as u64;
        match fid {
            0 => {
                // hart_start(hartid, start_addr, opaque)
                if a0 >= nharts {
                    return (SBI_ERR_INVALID_PARAM, 0);
                }
                let target = a0 as usize;
                if !bus.hart_stopped[target] {
                    return (SBI_ERR_ALREADY_AVAILABLE, 0);
                }
                bus.hart_stopped[target] = false;
                match bus.mt {
                    Some(mt) => mt.start(target, a1, a2),
                    None => bus.reqs.push(Req::Start { hart: target, pc: a1, opaque: a2 }),
                }
                (SBI_SUCCESS, 0)
            }
            1 => {
                // hart_stop: the caller stops itself.
                bus.hart_stopped[self.id as usize] = true;
                self.stopped = true;
                self.irq_dirty = true;
                self.break_chain();
                (SBI_SUCCESS, 0)
            }
            2 => {
                // hart_get_status(hartid): 0 = started, 1 = stopped
                if a0 >= nharts {
                    return (SBI_ERR_INVALID_PARAM, 0);
                }
                (SBI_SUCCESS, bus.hart_stopped[a0 as usize] as u64)
            }
            3 => {
                // hart_suspend: retentive suspend is implemented as WFI.
                self.wfi = true;
                self.irq_dirty = true;
                self.break_chain();
                (SBI_SUCCESS, 0)
            }
            _ => (SBI_ERR_NOT_SUPPORTED, 0),
        }
    }

    fn dbcn_call(&mut self, bus: &mut Bus, fid: u64, a0: u64, a1: u64, a2: u64) -> (i64, u64) {
        let addr = a1 | (a2 << 32);
        match fid {
            0 => {
                // write(num_bytes, base_lo, base_hi)
                let len = a0.min(MAX_DBCN_CHUNK) as usize;
                let mut buf = vec![0u8; len];
                if !bus.read_bytes(addr, &mut buf) {
                    return (SBI_ERR_INVALID_ADDRESS, 0);
                }
                bus.uart.tx.extend_from_slice(&buf);
                (SBI_SUCCESS, len as u64)
            }
            1 => {
                // read(num_bytes, base_lo, base_hi): copy from console input.
                let want = a0.min(MAX_DBCN_CHUNK) as usize;
                let n = want.min(bus.uart.rx.len());
                let data: Vec<u8> = bus.uart.rx.drain(..n).collect();
                if !bus.write_bytes(addr, &data) {
                    return (SBI_ERR_INVALID_ADDRESS, 0);
                }
                (SBI_SUCCESS, n as u64)
            }
            2 => {
                // write_byte(byte)
                bus.uart.tx.push(a0 as u8);
                (SBI_SUCCESS, 0)
            }
            _ => (SBI_ERR_NOT_SUPPORTED, 0),
        }
    }
}

fn read_u64(bus: &Bus, addr: u64) -> u64 {
    let mut b = [0u8; 8];
    if bus.read_bytes(addr, &mut b) {
        u64::from_le_bytes(b)
    } else {
        0
    }
}
