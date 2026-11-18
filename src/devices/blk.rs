//! virtio-blk: a block device backed by a host file.

use std::any::Any;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use super::virtio::{Chain, Desc, Queue, VirtioDevice};
use crate::bus::GuestMem;

const F_RO: u64 = 1 << 5;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const SECTOR: u64 = 512;
const DEVICE_ID: &[u8] = b"rvemu-virtio-blk";

const PAGE: usize = 4096;
const CACHE_WAYS: usize = 4;
const CACHE_SETS: usize = 2048;
/// Bypass the cache for streaming reads, so they cannot evict hot metadata.
const STREAM_BYTES: usize = 64 << 10;

pub struct VirtioBlk {
    file: File,
    sectors: u64,
    read_only: bool,
    cache: DiskCache,
    transfer: Vec<u8>,
    pub reads: u64,
    pub writes: u64,
}

struct Page {
    number: u64,
    used: u64,
    data: Box<[u8; PAGE]>,
}

/// Four pages compete for each set. Both hits and eviction examine at most
/// four entries, even after streaming workloads have filled the cache.
struct DiskCache {
    sets: Box<[[Option<Page>; CACHE_WAYS]]>,
    age: u64,
}

impl DiskCache {
    fn new() -> Self {
        Self {
            sets: std::iter::repeat_with(|| std::array::from_fn(|_| None))
                .take(CACHE_SETS)
                .collect(),
            age: 0,
        }
    }

    fn set(&mut self, number: u64) -> &mut [Option<Page>; CACHE_WAYS] {
        let index = (number ^ (number >> CACHE_SETS.trailing_zeros())) as usize & (CACHE_SETS - 1);
        &mut self.sets[index]
    }

    fn page(&mut self, file: &File, capacity: u64, number: u64) -> io::Result<&[u8; PAGE]> {
        self.age += 1;
        let age = self.age;
        let set = self.set(number);
        let slot = set
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.number == number));
        let index = if let Some(index) = slot {
            index
        } else {
            let (index, _) = set
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| p.as_ref().map(|p| p.used))
                .unwrap();
            let mut data = set[index]
                .take()
                .map_or_else(|| Box::new([0; PAGE]), |p| p.data);
            let start = number * PAGE as u64;
            let len = (capacity - start).min(PAGE as u64) as usize;
            file.read_exact_at(&mut data[..len], start)?;
            set[index] = Some(Page {
                number,
                used: age,
                data,
            });
            index
        };
        let page = set[index].as_mut().unwrap();
        page.used = age;
        Ok(&page.data)
    }

    fn read(
        &mut self,
        file: &File,
        capacity: u64,
        mut offset: u64,
        mut out: &mut [u8],
    ) -> io::Result<()> {
        if out.len() >= STREAM_BYTES {
            return file.read_exact_at(out, offset);
        }
        while !out.is_empty() {
            let within = offset as usize % PAGE;
            let len = (PAGE - within).min(out.len());
            let page = self.page(file, capacity, offset / PAGE as u64)?;
            out[..len].copy_from_slice(&page[within..within + len]);
            out = &mut out[len..];
            offset += len as u64;
        }
        Ok(())
    }

    /// Writes reach the host file before acknowledgement. Cache hits reflect
    /// the completed write, and a partial host error invalidates affected pages.
    fn write(&mut self, file: &File, mut offset: u64, mut data: &[u8]) -> io::Result<()> {
        let result = file.write_all_at(data, offset);
        while !data.is_empty() {
            let number = offset / PAGE as u64;
            let within = offset as usize % PAGE;
            let len = (PAGE - within).min(data.len());
            if let Some(slot) = self
                .set(number)
                .iter_mut()
                .find(|p| p.as_ref().is_some_and(|p| p.number == number))
            {
                if result.is_ok() {
                    slot.as_mut().unwrap().data[within..within + len].copy_from_slice(&data[..len]);
                } else {
                    *slot = None;
                }
            }
            data = &data[len..];
            offset += len as u64;
        }
        result
    }
}

impl VirtioBlk {
    pub fn new(file: File, read_only: bool) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            sectors: len / SECTOR,
            read_only,
            cache: DiskCache::new(),
            transfer: Vec::new(),
            reads: 0,
            writes: 0,
        })
    }

    /// Make the guest's writes reach the drive. Every write is already in the host's file
    /// cache (writes go through), so a killed emulator loses nothing; this is for the guest's
    /// `flush`. Rust's `sync_data` is `fcntl(F_FULLFSYNC)` on macOS, which also empties the
    /// drive's own cache and takes milliseconds: a boot spent a tenth of its time in it, with
    /// the guest waiting. `fsync` costs a fraction of that and protects against everything
    /// but a host power failure; `RVEMU_FULL_SYNC=1` restores the full flush.
    fn sync(&self) -> std::io::Result<()> {
        #[cfg(target_vendor = "apple")]
        {
            use std::os::fd::AsRawFd;
            static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if !*FULL.get_or_init(|| std::env::var_os("RVEMU_FULL_SYNC").is_some_and(|v| v != "0"))
            {
                // SAFETY: a plain call on a descriptor this struct owns.
                return if unsafe { libc::fsync(self.file.as_raw_fd()) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                };
            }
        }
        self.file.sync_data()
    }

    /// Serve one request. The header is the first readable descriptor, the
    /// status byte the last writable one, and the data sits in between.
    fn request(&mut self, chain: &Chain, mem: &mut dyn GuestMem) -> u32 {
        let (header, rest) = chain.descs.split_first().unwrap();
        let (status, descs) = rest.split_last().unwrap();
        if !status.writable() || status.len == 0 {
            return 0;
        }
        let mut hdr = [0u8; 16];
        let (code, written) =
            if header.writable() || header.len < 16 || !mem.mem_read(header.addr, &mut hdr) {
                (S_IOERR, 0)
            } else {
                let kind = u32::from_le_bytes(hdr[..4].try_into().unwrap());
                let sector = u64::from_le_bytes(hdr[8..].try_into().unwrap());
                self.transfer(kind, sector, descs, mem)
            };
        written + u32::from(mem.mem_write(status.addr, &[code]))
    }

    fn transfer(
        &mut self,
        kind: u32,
        sector: u64,
        descs: &[Desc],
        mem: &mut dyn GuestMem,
    ) -> (u8, u32) {
        match kind {
            T_FLUSH => return (if self.sync().is_ok() { S_OK } else { S_IOERR }, 0),
            T_GET_ID => {
                let mut id = [0u8; 20];
                id[..DEVICE_ID.len()].copy_from_slice(DEVICE_ID);
                let written = scatter(mem, descs, &id);
                return (
                    if written == id.len() { S_OK } else { S_IOERR },
                    written as u32,
                );
            }
            T_IN | T_OUT => {}
            _ => return (S_UNSUPP, 0),
        }
        let Some(offset) = sector.checked_mul(SECTOR) else {
            return (S_IOERR, 0);
        };
        let length: usize = descs.iter().map(|d| d.len as usize).sum();
        let capacity = self.sectors * SECTOR;
        if length >= u32::MAX as usize
            || offset > capacity
            || length as u64 > capacity - offset
            || (kind == T_OUT && self.read_only)
            || descs.iter().any(|d| d.writable() != (kind == T_IN))
        {
            return (S_IOERR, 0);
        }
        if self
            .transfer
            .try_reserve(length.saturating_sub(self.transfer.len()))
            .is_err()
        {
            return (S_IOERR, 0);
        }
        self.transfer.resize(length, 0);
        let buf = &mut self.transfer;
        if kind == T_IN {
            if self.cache.read(&self.file, capacity, offset, buf).is_err() {
                return (S_IOERR, 0);
            }
            let written = scatter(mem, descs, buf);
            if written != length {
                return (S_IOERR, written as u32);
            }
            self.reads += 1;
            (S_OK, written as u32)
        } else {
            let mut rest = &mut buf[..];
            for d in descs {
                let (part, next) = rest.split_at_mut(d.len as usize);
                if !mem.mem_read(d.addr, part) {
                    return (S_IOERR, 0);
                }
                rest = next;
            }
            if self.cache.write(&self.file, offset, buf).is_err() {
                return (S_IOERR, 0);
            }
            self.writes += 1;
            (S_OK, 0)
        }
    }
}

fn scatter(mem: &mut dyn GuestMem, descs: &[Desc], data: &[u8]) -> usize {
    let mut done = 0;
    for d in descs.iter().filter(|d| d.writable()) {
        let len = (d.len as usize).min(data.len() - done);
        if len == 0 {
            continue;
        }
        if !mem.mem_write(d.addr, &data[done..done + len]) {
            break;
        }
        done += len;
    }
    done
}

impl Drop for VirtioBlk {
    fn drop(&mut self) {
        if !self.read_only {
            let _ = self.sync();
        }
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 {
        2
    }

    fn features(&self) -> u64 {
        let mut f = F_FLUSH;
        if self.read_only {
            f |= F_RO;
        }
        f
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn config_read(&self, off: u64, size: usize) -> u64 {
        // struct virtio_blk_config starts with `capacity` (in 512-byte sectors).
        let bytes = self.sectors.to_le_bytes();
        let mut v = 0u64;
        for i in 0..size.min(8) {
            if let Some(b) = bytes.get(off as usize + i) {
                v |= (*b as u64) << (8 * i);
            }
        }
        v
    }

    fn notify(&mut self, _q: usize, queues: &mut [Queue], mem: &mut dyn GuestMem) -> bool {
        let mut any = false;
        while let Some(chain) = queues[0].pop(mem) {
            let len = if chain.descs.len() < 2 {
                0
            } else {
                self.request(&chain, mem)
            };
            queues[0].push_used(mem, chain.head, len);
            any = true;
        }
        any
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    #[test]
    fn write_through_refreshes_cached_pages_across_an_unaligned_boundary() {
        let name = format!(
            "rvemu-block-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        );
        let path = std::env::temp_dir().join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let capacity = (PAGE * 3) as u64;
        file.set_len(capacity).unwrap();

        let mut cache = DiskCache::new();
        let mut read = vec![0u8; PAGE * 2];
        cache.read(&file, capacity, 0, &mut read).unwrap();
        let offset = (PAGE - 512) as u64;
        let data = vec![0x55; PAGE + 512];
        cache.write(&file, offset, &data).unwrap();
        let mut cached = vec![0; data.len()];
        cache.read(&file, capacity, offset, &mut cached).unwrap();
        assert_eq!(cached, data);
        let mut backing = vec![0; data.len()];
        file.read_exact_at(&mut backing, offset).unwrap();
        assert_eq!(backing, data);
    }
}
