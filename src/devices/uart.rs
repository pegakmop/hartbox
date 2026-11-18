//! NS16550A-compatible UART (`reg-shift` 0, byte-wide registers).
//!
//! Transmit is instantaneous: bytes are appended to `tx`, which the machine
//! drains to the host. Receive bytes are queued in `rx` by the machine from
//! the host console. The THRE interrupt is latched the way a real 16550 does
//! it, so the Linux 8250 driver sees exactly the sequence it expects.

use std::collections::VecDeque;

const RX_CAPACITY: usize = 4096;

const IER_RDI: u8 = 1 << 0;
const IER_THRI: u8 = 1 << 1;
const LCR_DLAB: u8 = 1 << 7;
const LSR_DR: u8 = 1 << 0;
const LSR_THRE: u8 = 1 << 5;
const LSR_TEMT: u8 = 1 << 6;
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_RDA: u8 = 0x04;

pub struct Uart {
    pub rx: VecDeque<u8>,
    pub tx: Vec<u8>,
    /// Host input that arrived before the guest enabled receive interrupts.
    /// Serial drivers flush their FIFO while opening the port, so input held
    /// until then is not lost.
    held: VecDeque<u8>,
    rx_open: bool,
    ier: u8,
    iir_fcr: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    thre_latch: bool,
}

impl Default for Uart {
    fn default() -> Self {
        Self::new()
    }
}

impl Uart {
    pub fn new() -> Self {
        Uart {
            rx: VecDeque::new(),
            tx: Vec::with_capacity(4096),
            held: VecDeque::new(),
            rx_open: false,
            ier: 0,
            iir_fcr: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            // A freshly reset transmitter is empty and signals THRE.
            thre_latch: false,
        }
    }

    /// Queue host input. Excess input is dropped, as a real FIFO would.
    pub fn push_input(&mut self, bytes: &[u8]) {
        if !self.rx_open {
            for &b in bytes {
                if self.held.len() < RX_CAPACITY {
                    self.held.push_back(b);
                }
            }
            return;
        }
        for &b in bytes {
            if self.rx.len() < RX_CAPACITY {
                self.rx.push_back(b);
            }
        }
    }

    /// Whether the UART currently asserts its interrupt line.
    pub fn irq_asserted(&self) -> bool {
        (self.ier & IER_RDI != 0 && !self.rx.is_empty()) || (self.ier & IER_THRI != 0 && self.thre_latch)
    }

    fn iir(&mut self) -> u8 {
        if self.ier & IER_RDI != 0 && !self.rx.is_empty() {
            IIR_RDA | 0xc0
        } else if self.ier & IER_THRI != 0 && self.thre_latch {
            // Reading IIR acknowledges the THRE interrupt.
            self.thre_latch = false;
            IIR_THRE | 0xc0
        } else {
            IIR_NONE | 0xc0
        }
    }

    pub fn read(&mut self, off: u64, size: usize) -> Option<u64> {
        if size != 1 {
            return None;
        }
        let dlab = self.lcr & LCR_DLAB != 0;
        let v = match off {
            0 if dlab => self.dll,
            0 => self.rx.pop_front().unwrap_or(0),
            1 if dlab => self.dlm,
            1 => self.ier,
            2 => self.iir(),
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                let mut lsr = LSR_THRE | LSR_TEMT;
                if !self.rx.is_empty() {
                    lsr |= LSR_DR;
                }
                lsr
            }
            6 => 0xb0, // CTS | DSR | DCD asserted
            7 => self.scr,
            _ => 0,
        };
        Some(v as u64)
    }

    pub fn write(&mut self, off: u64, size: usize, val: u64) -> Option<()> {
        if size != 1 {
            return None;
        }
        let v = val as u8;
        let dlab = self.lcr & LCR_DLAB != 0;
        match off {
            0 if dlab => self.dll = v,
            0 => {
                self.tx.push(v);
                // Transmit completes immediately; the THRE interrupt is
                // re-armed for the next write.
                self.thre_latch = true;
            }
            1 if dlab => self.dlm = v,
            1 => {
                if v & IER_RDI != 0 && !self.rx_open {
                    // The driver has finished opening the port: release held input.
                    self.rx_open = true;
                    let held = std::mem::take(&mut self.held);
                    self.push_input(&held.into_iter().collect::<Vec<u8>>());
                }
                let was = self.ier & IER_THRI;
                self.ier = v & 0x0f;
                if self.ier & IER_THRI != 0 && was == 0 {
                    self.thre_latch = true;
                }
            }
            2 => self.iir_fcr = v,
            3 => self.lcr = v,
            4 => self.mcr = v,
            7 => self.scr = v,
            _ => {}
        }
        Some(())
    }
}
