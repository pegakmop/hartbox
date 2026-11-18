//! Loading of kernels, firmware and bare-metal programs into guest RAM.
//!
//! Supported formats: gzip-compressed or raw RISC-V `Image` (the format the
//! Linux build system produces), ELF64 RISC-V executables, and raw binaries.

use std::io::Read;

use crate::arch::*;
use crate::bus::Bus;

/// Where the kernel's `Image` expects to be placed when `text_offset` is 0.
const DEFAULT_KERNEL_OFFSET: u64 = 0x20_0000;
const IMAGE_MAGIC: &[u8; 4] = b"RSC\x05";

/// Result of loading a kernel or program image.
#[derive(Clone, Copy, Debug)]
pub struct Loaded {
    pub entry: u64,
    /// One past the highest guest-physical byte the image occupies.
    pub end: u64,
}

/// Transparently strip a gzip wrapper.
pub fn maybe_gunzip(data: Vec<u8>) -> Result<Vec<u8>, String> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut out = Vec::with_capacity(data.len() * 4);
        flate2::read::GzDecoder::new(&data[..])
            .read_to_end(&mut out)
            .map_err(|e| format!("gzip decompression failed: {e}"))?;
        Ok(out)
    } else {
        Ok(data)
    }
}

fn check_range(bus: &Bus, addr: u64, len: usize) -> Result<(), String> {
    let off = Bus::ram_offset(addr);
    if addr < RAM_BASE || off > bus.ram.len() || len > bus.ram.len() - off {
        return Err(format!("image at {addr:#x} ({len} bytes) does not fit in RAM"));
    }
    Ok(())
}

/// Copy `data` to guest-physical `addr`, which must lie in RAM.
pub fn load_at(bus: &mut Bus, addr: u64, data: &[u8]) -> Result<(), String> {
    check_range(bus, addr, data.len())?;
    bus.write_bytes(addr, data);
    Ok(())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

/// Load an ELF64 little-endian RISC-V executable at its physical addresses.
pub fn load_elf(bus: &mut Bus, data: &[u8]) -> Result<Loaded, String> {
    if data.len() < 64 || &data[0..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return Err("not an ELF64 little-endian file".into());
    }
    if le16(data, 18) != 243 {
        return Err("ELF is not a RISC-V (EM_RISCV) binary".into());
    }
    let entry = le64(data, 24);
    let phoff = le64(data, 32) as usize;
    let phentsize = le16(data, 54) as usize;
    let phnum = le16(data, 56) as usize;
    let mut end = 0u64;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if ph + 56 > data.len() {
            return Err("truncated ELF program header".into());
        }
        if le32(data, ph) != 1 {
            continue; // not PT_LOAD
        }
        let offset = le64(data, ph + 8) as usize;
        let paddr = le64(data, ph + 24);
        let filesz = le64(data, ph + 32) as usize;
        let memsz = le64(data, ph + 40) as usize;
        if offset + filesz > data.len() {
            return Err("truncated ELF segment".into());
        }
        check_range(bus, paddr, memsz)?;
        bus.write_bytes(paddr, &data[offset..offset + filesz]);
        // Zero-fill the BSS part of the segment.
        if memsz > filesz {
            let zeros = vec![0u8; memsz - filesz];
            bus.write_bytes(paddr + filesz as u64, &zeros);
        }
        end = end.max(paddr + memsz as u64);
    }
    Ok(Loaded { entry, end })
}

/// Load a Linux RISC-V `Image` (header per Documentation/riscv/boot-image-header).
pub fn load_riscv_image(bus: &mut Bus, data: &[u8]) -> Result<Loaded, String> {
    if data.len() < 64 || &data[56..60] != IMAGE_MAGIC {
        return Err("not a RISC-V Image".into());
    }
    let text_offset = le64(data, 8);
    let image_size = le64(data, 16).max(data.len() as u64);
    let base = RAM_BASE + if text_offset == 0 { DEFAULT_KERNEL_OFFSET } else { text_offset };
    load_at(bus, base, data)?;
    Ok(Loaded { entry: base, end: base + image_size })
}

/// Load a kernel. Accepts gzip, ELF or raw `Image` input.
pub fn load_kernel(bus: &mut Bus, data: Vec<u8>) -> Result<Loaded, String> {
    let data = maybe_gunzip(data)?;
    if data.len() >= 4 && &data[0..4] == b"\x7fELF" {
        load_elf(bus, &data)
    } else if data.len() >= 64 && &data[56..60] == IMAGE_MAGIC {
        load_riscv_image(bus, &data)
    } else {
        // Unknown raw kernel: assume it was linked for the default offset.
        let base = RAM_BASE + DEFAULT_KERNEL_OFFSET;
        load_at(bus, base, &data)?;
        Ok(Loaded { entry: base, end: base + data.len() as u64 })
    }
}

/// Load firmware (raw binary or ELF) at the start of RAM. Returns its entry
/// point and end address.
pub fn load_firmware(bus: &mut Bus, data: Vec<u8>) -> Result<Loaded, String> {
    let data = maybe_gunzip(data)?;
    if data.len() >= 4 && &data[0..4] == b"\x7fELF" {
        let loaded = load_elf(bus, &data)?;
        Ok(loaded)
    } else {
        load_at(bus, RAM_BASE, &data)?;
        Ok(Loaded { entry: RAM_BASE, end: RAM_BASE + data.len() as u64 })
    }
}
