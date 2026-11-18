//! virtio-mmio transport (version 2) and split virtqueues.
//!
//! The transport owns the register file and the queue state; device models
//! implement [`VirtioDevice`] and process descriptor chains through a
//! [`GuestMem`] view of guest RAM.

use std::any::Any;

use crate::bus::GuestMem;

/// "virt" in little-endian.
const MAGIC: u32 = 0x7472_6976;
const VENDOR: u32 = 0x554d_4551; // "QEMU"
/// Negotiated for every modern (non-legacy) device.
pub const F_VERSION_1: u64 = 1 << 32;
pub const MAX_QUEUE: u16 = 256;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;

/// One descriptor from a descriptor table.
#[derive(Clone, Copy, Debug)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
}

impl Desc {
    pub fn writable(&self) -> bool {
        self.flags & DESC_F_WRITE != 0
    }
}

/// A descriptor chain taken from the available ring.
pub struct Chain {
    pub head: u16,
    pub descs: Vec<Desc>,
}

impl Chain {
    /// Total length of the device-readable (driver-to-device) part.
    pub fn readable_len(&self) -> usize {
        self.descs.iter().filter(|d| !d.writable()).map(|d| d.len as usize).sum()
    }

    /// Total length of the device-writable (device-to-driver) part.
    pub fn writable_len(&self) -> usize {
        self.descs.iter().filter(|d| d.writable()).map(|d| d.len as usize).sum()
    }

    /// Gather every readable descriptor into one buffer.
    pub fn read_all(&self, mem: &dyn GuestMem) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.readable_len());
        for d in self.descs.iter().filter(|d| !d.writable()) {
            let start = out.len();
            out.resize(start + d.len as usize, 0);
            if !mem.mem_read(d.addr, &mut out[start..]) {
                out.truncate(start);
                break;
            }
        }
        out
    }

    /// Scatter `data` across the writable descriptors. Returns bytes written.
    pub fn write_all(&self, mem: &mut dyn GuestMem, data: &[u8]) -> usize {
        let mut done = 0;
        for d in self.descs.iter().filter(|d| d.writable()) {
            if done == data.len() {
                break;
            }
            let n = (d.len as usize).min(data.len() - done);
            if !mem.mem_write(d.addr, &data[done..done + n]) {
                break;
            }
            done += n;
        }
        done
    }
}

fn read_u16(mem: &dyn GuestMem, addr: u64) -> Option<u16> {
    let mut b = [0u8; 2];
    mem.mem_read(addr, &mut b).then(|| u16::from_le_bytes(b))
}

/// State of one split virtqueue.
#[derive(Clone, Copy, Default, Debug)]
pub struct Queue {
    pub num: u16,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub last_avail: u16,
}

impl Queue {
    /// Take the next chain the driver made available, if any.
    pub fn pop(&mut self, mem: &dyn GuestMem) -> Option<Chain> {
        if !self.ready || self.num == 0 {
            return None;
        }
        let avail_idx = read_u16(mem, self.avail + 2)?;
        if avail_idx == self.last_avail {
            return None;
        }
        let slot = (self.last_avail % self.num) as u64;
        let head = read_u16(mem, self.avail + 4 + 2 * slot)?;
        self.last_avail = self.last_avail.wrapping_add(1);

        let mut descs = Vec::new();
        let mut i = head;
        for _ in 0..self.num {
            let mut raw = [0u8; 16];
            if i >= self.num || !mem.mem_read(self.desc + 16 * i as u64, &mut raw) {
                // A malformed chain is completed empty rather than wedging the queue.
                return Some(Chain { head, descs: Vec::new() });
            }
            let d = Desc {
                addr: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
                len: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
                flags: u16::from_le_bytes(raw[12..14].try_into().unwrap()),
            };
            let next = u16::from_le_bytes(raw[14..16].try_into().unwrap());
            if d.flags & DESC_F_INDIRECT != 0 {
                // INDIRECT is not negotiated, so the driver must not use it.
                return Some(Chain { head, descs: Vec::new() });
            }
            descs.push(d);
            if d.flags & DESC_F_NEXT == 0 {
                return Some(Chain { head, descs });
            }
            i = next;
        }
        Some(Chain { head, descs: Vec::new() })
    }

    /// Report a completed chain to the driver.
    pub fn push_used(&mut self, mem: &mut dyn GuestMem, head: u16, len: u32) {
        let idx = read_u16(mem, self.used + 2).unwrap_or(0);
        let slot = (idx % self.num.max(1)) as u64;
        let mut e = [0u8; 8];
        e[0..4].copy_from_slice(&(head as u32).to_le_bytes());
        e[4..8].copy_from_slice(&len.to_le_bytes());
        mem.mem_write(self.used + 4 + 8 * slot, &e);
        mem.mem_write(self.used + 2, &idx.wrapping_add(1).to_le_bytes());
    }

    /// Whether the driver has made more chains available.
    pub fn has_avail(&self, mem: &dyn GuestMem) -> bool {
        self.ready && read_u16(mem, self.avail + 2).is_some_and(|i| i != self.last_avail)
    }
}

/// Behaviour of a concrete virtio device.
pub trait VirtioDevice: Any {
    fn device_id(&self) -> u32;
    /// Device feature bits (64-bit). `F_VERSION_1` is added by the transport.
    fn features(&self) -> u64;
    fn queue_count(&self) -> usize;
    /// Device-specific configuration space, read at offset `off` from 0x100.
    fn config_read(&self, _off: u64, _size: usize) -> u64 {
        0
    }
    fn config_write(&mut self, _off: u64, _size: usize, _val: u64) {}
    /// The driver notified queue `q`. Returns true to raise an interrupt.
    fn notify(&mut self, q: usize, queues: &mut [Queue], mem: &mut dyn GuestMem) -> bool;
    /// Periodic work that does not depend on a driver notification.
    fn poll(&mut self, _queues: &mut [Queue], _mem: &mut dyn GuestMem) -> bool {
        false
    }
    fn as_any(&mut self) -> &mut dyn Any;
}

/// A virtio-mmio device: transport registers plus the device model.
pub struct VirtioMmio {
    pub dev: Box<dyn VirtioDevice>,
    pub irq: u32,
    queues: Vec<Queue>,
    device_features_sel: u32,
    queue_sel: u32,
    status: u32,
    int_status: u32,
}

impl VirtioMmio {
    pub fn new(dev: Box<dyn VirtioDevice>, irq: u32) -> Self {
        let queues = vec![Queue::default(); dev.queue_count()];
        VirtioMmio { dev, irq, queues, device_features_sel: 0, queue_sel: 0, status: 0, int_status: 0 }
    }

    pub fn irq_asserted(&self) -> bool {
        self.int_status != 0
    }

    fn features(&self) -> u64 {
        self.dev.features() | F_VERSION_1
    }

    pub fn read(&self, off: u64, size: usize) -> u64 {
        if off >= 0x100 {
            return self.dev.config_read(off - 0x100, size);
        }
        let v = match off {
            0x000 => MAGIC,
            0x004 => 2,
            0x008 => self.dev.device_id(),
            0x00c => VENDOR,
            0x010 => (self.features() >> (32 * self.device_features_sel)) as u32,
            0x034 => {
                if (self.queue_sel as usize) < self.queues.len() {
                    MAX_QUEUE as u32
                } else {
                    0
                }
            }
            0x044 => self.queues.get(self.queue_sel as usize).is_some_and(|q| q.ready) as u32,
            0x060 => self.int_status,
            0x070 => self.status,
            0x0fc => 0, // config generation
            _ => 0,
        };
        v as u64
    }

    /// Register write. `mem` is guest RAM, needed when a notify drives the device.
    pub fn write(&mut self, off: u64, size: usize, val: u64, mem: &mut dyn GuestMem) {
        if off >= 0x100 {
            self.dev.config_write(off - 0x100, size, val);
            return;
        }
        let v = val as u32;
        let qi = self.queue_sel as usize;
        match off {
            0x014 => self.device_features_sel = v,
            0x020 | 0x024 => {}
            0x030 => self.queue_sel = v,
            0x038 => {
                if let Some(q) = self.queues.get_mut(qi) {
                    q.num = (v as u16).min(MAX_QUEUE);
                }
            }
            0x044 => {
                if let Some(q) = self.queues.get_mut(qi) {
                    q.ready = v & 1 != 0;
                }
            }
            0x050 => {
                let q = v as usize;
                if q < self.queues.len() && self.dev.notify(q, &mut self.queues, mem) {
                    self.int_status |= 1;
                }
            }
            0x064 => self.int_status &= !v,
            0x070 => {
                if v == 0 {
                    self.reset();
                } else {
                    self.status = v;
                }
            }
            0x080 => set_lo(self.queues.get_mut(qi).map(|q| &mut q.desc), v),
            0x084 => set_hi(self.queues.get_mut(qi).map(|q| &mut q.desc), v),
            0x090 => set_lo(self.queues.get_mut(qi).map(|q| &mut q.avail), v),
            0x094 => set_hi(self.queues.get_mut(qi).map(|q| &mut q.avail), v),
            0x0a0 => set_lo(self.queues.get_mut(qi).map(|q| &mut q.used), v),
            0x0a4 => set_hi(self.queues.get_mut(qi).map(|q| &mut q.used), v),
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.status = 0;
        self.int_status = 0;
        self.queue_sel = 0;
        self.device_features_sel = 0;
        for q in self.queues.iter_mut() {
            *q = Queue::default();
        }
    }

    /// Run background device work (for example, delivering network frames).
    pub fn poll(&mut self, mem: &mut dyn GuestMem) {
        if self.dev.poll(&mut self.queues, mem) {
            self.int_status |= 1;
        }
    }
}

fn set_lo(field: Option<&mut u64>, v: u32) {
    if let Some(f) = field {
        *f = (*f & 0xffff_ffff_0000_0000) | v as u64;
    }
}

fn set_hi(field: Option<&mut u64>, v: u32) {
    if let Some(f) = field {
        *f = (*f & 0xffff_ffff) | ((v as u64) << 32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guest memory starting at physical address 0.
    struct Mem(Vec<u8>);

    impl GuestMem for Mem {
        fn mem_read(&self, pa: u64, buf: &mut [u8]) -> bool {
            let s = pa as usize;
            match self.0.get(s..s + buf.len()) {
                Some(src) => {
                    buf.copy_from_slice(src);
                    true
                }
                None => false,
            }
        }

        fn mem_write(&mut self, pa: u64, data: &[u8]) -> bool {
            let s = pa as usize;
            match self.0.get_mut(s..s + data.len()) {
                Some(dst) => {
                    dst.copy_from_slice(data);
                    true
                }
                None => false,
            }
        }
    }

    fn put_desc(m: &mut Mem, table: u64, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let at = table + 16 * i as u64;
        m.mem_write(at, &addr.to_le_bytes());
        m.mem_write(at + 8, &len.to_le_bytes());
        m.mem_write(at + 12, &flags.to_le_bytes());
        m.mem_write(at + 14, &next.to_le_bytes());
    }

    #[test]
    fn queue_pops_chains_and_reports_used() {
        let mut m = Mem(vec![0; 0x4000]);
        let (desc, avail, used) = (0x1000u64, 0x2000u64, 0x3000u64);
        // A two-descriptor chain: readable data at 0x800, writable buffer at 0x900.
        put_desc(&mut m, desc, 0, 0x800, 16, DESC_F_NEXT, 1);
        put_desc(&mut m, desc, 1, 0x900, 32, DESC_F_WRITE, 0);
        m.mem_write(avail + 2, &1u16.to_le_bytes()); // driver published one chain
        m.mem_write(avail + 4, &0u16.to_le_bytes()); // ring[0] = head 0

        let mut q = Queue { num: 4, ready: true, desc, avail, used, last_avail: 0 };
        assert!(q.has_avail(&m));
        let chain = q.pop(&m).expect("one chain");
        assert_eq!(chain.head, 0);
        assert_eq!(chain.readable_len(), 16);
        assert_eq!(chain.writable_len(), 32);
        assert!(q.pop(&m).is_none(), "no more chains");

        q.push_used(&mut m, chain.head, 20);
        let mut idx = [0u8; 2];
        m.mem_read(used + 2, &mut idx);
        assert_eq!(u16::from_le_bytes(idx), 1);
        let mut elem = [0u8; 8];
        m.mem_read(used + 4, &mut elem);
        assert_eq!(u32::from_le_bytes(elem[4..8].try_into().unwrap()), 20);
    }

    #[test]
    fn chain_io_moves_bytes_through_guest_buffers() {
        let mut m = Mem(vec![0; 0x2000]);
        m.mem_write(0x800, b"abcdef");
        let chain = Chain {
            head: 0,
            descs: vec![
                Desc { addr: 0x800, len: 6, flags: 0 },
                Desc { addr: 0x900, len: 4, flags: DESC_F_WRITE },
            ],
        };
        assert_eq!(chain.read_all(&m), b"abcdef");
        assert_eq!(chain.write_all(&mut m, b"XYZWQ"), 4);
        let mut out = [0u8; 4];
        m.mem_read(0x900, &mut out);
        assert_eq!(&out, b"XYZW");
    }
}
