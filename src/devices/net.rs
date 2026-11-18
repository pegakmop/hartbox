//! virtio-net: an Ethernet NIC whose frames are exchanged with the network
//! gateway (see `crate::net`). Queue 0 receives, queue 1 transmits.

use std::any::Any;
use std::collections::VecDeque;

use super::virtio::{Queue, VirtioDevice};
use crate::bus::GuestMem;

const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
/// Size of `virtio_net_hdr` when VIRTIO_F_VERSION_1 is negotiated.
const HDR_LEN: usize = 12;
const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;
/// Frames waiting for the guest; beyond this the gateway's frames are dropped.
const RX_LIMIT: usize = 1024;

pub struct VirtioNet {
    pub mac: [u8; 6],
    /// Frames from the gateway, waiting for guest receive buffers.
    rx_in: VecDeque<Vec<u8>>,
    /// Frames the guest transmitted, taken by the gateway each tick.
    tx_out: Vec<Vec<u8>>,
    pub rx_dropped: u64,
}

impl VirtioNet {
    pub fn new(mac: [u8; 6]) -> Self {
        VirtioNet { mac, rx_in: VecDeque::new(), tx_out: Vec::new(), rx_dropped: 0 }
    }

    /// Queue a frame for delivery to the guest.
    pub fn push_rx(&mut self, frame: Vec<u8>) {
        if self.rx_in.len() < RX_LIMIT {
            self.rx_in.push_back(frame);
        } else {
            self.rx_dropped += 1;
        }
    }

    /// Frames the guest has transmitted since the last call.
    pub fn take_tx(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.tx_out)
    }

    /// Hand queued frames to the guest. Returns true if any were delivered.
    fn deliver(&mut self, queues: &mut [Queue], mem: &mut dyn GuestMem) -> bool {
        let mut any = false;
        while !self.rx_in.is_empty() {
            if !queues[RX_QUEUE].has_avail(mem) {
                break;
            }
            let frame = self.rx_in.pop_front().expect("queue is not empty");
            let Some(chain) = queues[RX_QUEUE].pop(mem) else { break };
            let mut data = vec![0u8; HDR_LEN + frame.len()];
            // num_buffers = 1 (little-endian u16 at offset 10)
            data[10] = 1;
            data[HDR_LEN..].copy_from_slice(&frame);
            let written = if chain.writable_len() >= data.len() { chain.write_all(mem, &data) } else { 0 };
            queues[RX_QUEUE].push_used(mem, chain.head, written as u32);
            any = true;
        }
        any
    }
}

impl VirtioDevice for VirtioNet {
    fn device_id(&self) -> u32 {
        1
    }

    fn features(&self) -> u64 {
        F_MAC | F_STATUS
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn config_read(&self, off: u64, size: usize) -> u64 {
        // struct virtio_net_config: mac[6], status (u16; bit 0 = link up)
        let mut bytes = [0u8; 8];
        bytes[..6].copy_from_slice(&self.mac);
        bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
        let mut v = 0u64;
        for i in 0..size.min(8) {
            if let Some(b) = bytes.get(off as usize + i) {
                v |= (*b as u64) << (8 * i);
            }
        }
        v
    }

    fn notify(&mut self, q: usize, queues: &mut [Queue], mem: &mut dyn GuestMem) -> bool {
        match q {
            TX_QUEUE => {
                while let Some(chain) = queues[TX_QUEUE].pop(mem) {
                    let raw = chain.read_all(mem);
                    if raw.len() > HDR_LEN {
                        self.tx_out.push(raw[HDR_LEN..].to_vec());
                    }
                    queues[TX_QUEUE].push_used(mem, chain.head, 0);
                }
                true
            }
            RX_QUEUE => self.deliver(queues, mem),
            _ => false,
        }
    }

    fn poll(&mut self, queues: &mut [Queue], mem: &mut dyn GuestMem) -> bool {
        self.deliver(queues, mem)
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}
