//! virtio-rng: entropy from the host.

use std::any::Any;
use std::fs::File;
use std::io::Read;

use super::virtio::{Queue, VirtioDevice};
use crate::bus::GuestMem;

pub struct VirtioRng {
    source: Option<File>,
}

impl VirtioRng {
    pub fn new() -> Self {
        VirtioRng { source: File::open("/dev/urandom").ok() }
    }
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioDevice for VirtioRng {
    fn device_id(&self) -> u32 {
        4
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn notify(&mut self, _q: usize, queues: &mut [Queue], mem: &mut dyn GuestMem) -> bool {
        let mut any = false;
        while let Some(chain) = queues[0].pop(mem) {
            let len = chain.writable_len();
            let mut buf = vec![0u8; len];
            let filled = match self.source.as_mut() {
                Some(f) => f.read_exact(&mut buf).is_ok(),
                None => false,
            };
            let written = if filled { chain.write_all(mem, &buf) } else { 0 };
            queues[0].push_used(mem, chain.head, written as u32);
            any = true;
        }
        any
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}
