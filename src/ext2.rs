//! Creates an empty ext2 file system, used as the persistent root disk.
//!
//! The image uses 4 KiB blocks, 8192 inodes per block group and 128-byte
//! inodes, and has no journal, so the guest mounts it with `mount -t ext4`.
//! Each block group keeps a copy of the superblock and of the group descriptor
//! table, as `mke2fs` does without sparse superblocks. Only the root directory
//! and `lost+found` exist at first; the guest fills in everything else.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: u64 = 4096;
const BLOCKS_PER_GROUP: u64 = 32768;
const INODES_PER_GROUP: u32 = 8192;
const INODE_SIZE: u64 = 128;
const INODE_TABLE_BLOCKS: u64 = INODES_PER_GROUP as u64 * INODE_SIZE / BLOCK_SIZE;
/// Blocks at the start of a group: superblock, descriptors, two bitmaps, inode table.
const META_BLOCKS: u64 = 4 + INODE_TABLE_BLOCKS;
const FIRST_INO: u32 = 11;
const ROOT_INO: u32 = 2;
const LOST_FOUND_INO: u32 = 11;
/// Group descriptors are 32 bytes and must fit in one block.
const MAX_GROUPS: u64 = BLOCK_SIZE / 32;
const MIN_SIZE: u64 = 8 << 20;

const EXT2_MAGIC: u16 = 0xEF53;
const FEATURE_FILETYPE: u32 = 0x0002;
const FILE_TYPE_DIR: u8 = 2;
const MODE_DIR_755: u16 = 0o40755;
const MODE_DIR_700: u16 = 0o40700;

/// Read the completion marker on images made by this formatter. Unknown
/// layouts, incomplete provisioning and read errors use the full initramfs.
pub fn has_populated_root(path: &Path) -> bool {
    probe_root(path).unwrap_or(false)
}

fn probe_root(path: &Path) -> Option<bool> {
    let file = File::open(path).ok()?;
    let mut sb = [0; 1024];
    file.read_exact_at(&mut sb, 1024).ok()?;
    let blocks = u64::from(le32(&sb, 4));
    if le16(&sb, 56) != EXT2_MAGIC || le16(&sb, 58) & 2 != 0
        || le32(&sb, 20) != 0 || le32(&sb, 24) != 2
        || le32(&sb, 32) != BLOCKS_PER_GROUP as u32 || le32(&sb, 40) != INODES_PER_GROUP
        || le16(&sb, 88) != INODE_SIZE as u16 || le32(&sb, 92) != 0
        || le32(&sb, 96) != FEATURE_FILETYPE || le32(&sb, 100) != 0
        || blocks * BLOCK_SIZE > file.metadata().ok()?.len()
    {
        return None;
    }
    let disk = RootProbe { file, blocks, inodes: le32(&sb, 0) };
    let root = disk.inode(ROOT_INO)?;
    let etc = disk.inode(disk.child(&root, b"etc")?)?;
    let marker = disk.inode(disk.child(&etc, b"rvemu-root")?)?;
    let bin = disk.inode(disk.child(&root, b"bin")?)?;
    let shell = disk.inode(disk.child(&bin, b"busybox")?)?;
    let mut magic = [0; 4];
    if le16(&shell, 0) & 0o170100 != 0o100100 || le32(&shell, 4) < magic.len() as u32 {
        return None;
    }
    disk.file.read_exact_at(&mut magic, disk.block(le32(&shell, 40))?).ok()?;
    let mut text = *b"rvemu-root-1\n";
    if le16(&marker, 0) & 0o170000 != 0o100000 || le32(&marker, 4) != text.len() as u32 {
        return None;
    }
    disk.file.read_exact_at(&mut text, disk.block(le32(&marker, 40))?).ok()?;
    Some(magic == *b"\x7fELF" && text == *b"rvemu-root-1\n")
}

struct RootProbe {
    file: File,
    blocks: u64,
    inodes: u32,
}

impl RootProbe {
    fn block(&self, id: u32) -> Option<u64> {
        (id != 0 && u64::from(id) < self.blocks).then_some(u64::from(id) * BLOCK_SIZE)
    }

    fn inode(&self, id: u32) -> Option<[u8; INODE_SIZE as usize]> {
        let index = id.checked_sub(1)?;
        if id > self.inodes {
            return None;
        }
        let group = index / INODES_PER_GROUP;
        let mut pointer = [0; 4];
        self.file.read_exact_at(&mut pointer, BLOCK_SIZE + u64::from(group) * 32 + 8).ok()?;
        let start = self.block(u32::from_le_bytes(pointer))?;
        let offset = start + u64::from(index % INODES_PER_GROUP) * INODE_SIZE;
        let mut node = [0; INODE_SIZE as usize];
        self.file.read_exact_at(&mut node, offset).ok()?;
        (le16(&node, 26) != 0 && le32(&node, 32) & 0x80000 == 0).then_some(node)
    }

    fn child(&self, dir: &[u8], name: &[u8]) -> Option<u32> {
        if le16(dir, 0) & 0o170000 != 0o040000 {
            return None;
        }
        let mut block = [0; BLOCK_SIZE as usize];
        let count = (u64::from(le32(dir, 4)) / BLOCK_SIZE).min(12) as usize;
        for i in 0..count {
            let Some(offset) = self.block(le32(dir, 40 + i * 4)) else { continue };
            self.file.read_exact_at(&mut block, offset).ok()?;
            let mut at = 0;
            while at + 8 <= block.len() {
                let size = usize::from(le16(&block, at + 4));
                let count = usize::from(block[at + 6]);
                if size < 8 || size & 3 != 0 || at + size > block.len() || count > size - 8 {
                    return None;
                }
                let id = le32(&block, at);
                if id != 0 && &block[at + 8..at + 8 + count] == name {
                    return Some(id);
                }
                at += size;
            }
        }
        None
    }
}

fn le16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(buf[at..at + 2].try_into().unwrap())
}

fn le32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

/// Create a new file system of `size` bytes at `path`. The file must not exist.
pub fn format(path: &Path, size: u64, label: &str) -> io::Result<()> {
    if size < MIN_SIZE || !size.is_multiple_of(BLOCK_SIZE) {
        return Err(invalid("size must be a multiple of 4 KiB and at least 8 MiB"));
    }
    if label.len() > 16 {
        return Err(invalid("volume label is longer than 16 bytes"));
    }
    let mut blocks = size / BLOCK_SIZE;
    // A last group too small to hold its own metadata is left out.
    let rem = blocks % BLOCKS_PER_GROUP;
    if rem != 0 && rem < META_BLOCKS + 2 {
        blocks -= rem;
    }
    let groups = blocks.div_ceil(BLOCKS_PER_GROUP);
    if groups > MAX_GROUPS {
        return Err(invalid("at most 16 GiB is supported"));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);

    let mut descs = Vec::with_capacity(groups as usize);
    let mut free_blocks = 0u64;
    for g in 0..groups {
        let start = g * BLOCKS_PER_GROUP;
        let in_group = (blocks - start).min(BLOCKS_PER_GROUP);
        let (used_blocks, used_inodes, used_dirs) = if g == 0 {
            (META_BLOCKS + 2, FIRST_INO, 2u16)
        } else {
            (META_BLOCKS, 0, 0)
        };
        let fb = in_group - used_blocks;
        let fi = INODES_PER_GROUP - used_inodes;
        free_blocks += fb;
        descs.push(Descriptor {
            block_bitmap: (start + 2) as u32,
            inode_bitmap: (start + 3) as u32,
            inode_table: (start + 4) as u32,
            free_blocks: fb as u16,
            free_inodes: fi as u16,
            used_dirs,
        });
    }

    let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
    file.set_len(size)?;

    let gdt = descriptor_block(&descs);
    let total_inodes = groups as u32 * INODES_PER_GROUP;
    let sb = |group: u16| {
        superblock(&SuperblockFields {
            blocks,
            inodes: total_inodes,
            free_blocks,
            // Inodes 1 to FIRST_INO - 1 are reserved and FIRST_INO is lost+found.
            free_inodes: total_inodes - FIRST_INO,
            group,
            now,
            label,
        })
    };

    for g in 0..groups {
        let start = g * BLOCKS_PER_GROUP;
        let in_group = (blocks - start).min(BLOCKS_PER_GROUP);
        let superblock_bytes = sb(g as u16);
        if g == 0 {
            file.seek(SeekFrom::Start(1024))?;
        } else {
            file.seek(SeekFrom::Start(start * BLOCK_SIZE))?;
        }
        file.write_all(&superblock_bytes)?;

        file.seek(SeekFrom::Start((start + 1) * BLOCK_SIZE))?;
        file.write_all(&gdt)?;

        let used_blocks = META_BLOCKS + if g == 0 { 2 } else { 0 };
        let used_inodes = if g == 0 { FIRST_INO } else { 0 };
        let bb = bitmap(&[(0, used_blocks as usize), (in_group as usize, BLOCKS_PER_GROUP as usize)]);
        file.seek(SeekFrom::Start((start + 2) * BLOCK_SIZE))?;
        file.write_all(&bb)?;

        let ib = bitmap(&[(0, used_inodes as usize), (INODES_PER_GROUP as usize, BLOCKS_PER_GROUP as usize)]);
        file.seek(SeekFrom::Start((start + 3) * BLOCK_SIZE))?;
        file.write_all(&ib)?;
    }

    // Group 0: the first inode-table block holds the root and lost+found inodes,
    // and the next two blocks hold their directory contents.
    let root_block = META_BLOCKS;
    let lost_found_block = root_block + 1;
    let mut table = vec![0u8; BLOCK_SIZE as usize];
    put_inode(&mut table, ROOT_INO, MODE_DIR_755, 3, root_block as u32, now);
    put_inode(&mut table, LOST_FOUND_INO, MODE_DIR_700, 2, lost_found_block as u32, now);
    file.seek(SeekFrom::Start(4 * BLOCK_SIZE))?;
    file.write_all(&table)?;

    let mut root_dir = vec![0u8; BLOCK_SIZE as usize];
    put_dirent(&mut root_dir, 0, ROOT_INO, 12, ".", FILE_TYPE_DIR);
    put_dirent(&mut root_dir, 12, ROOT_INO, 12, "..", FILE_TYPE_DIR);
    put_dirent(&mut root_dir, 24, LOST_FOUND_INO, (BLOCK_SIZE - 24) as u16, "lost+found", FILE_TYPE_DIR);
    file.seek(SeekFrom::Start(root_block * BLOCK_SIZE))?;
    file.write_all(&root_dir)?;

    let mut lost_found = vec![0u8; BLOCK_SIZE as usize];
    put_dirent(&mut lost_found, 0, LOST_FOUND_INO, 12, ".", FILE_TYPE_DIR);
    put_dirent(&mut lost_found, 12, ROOT_INO, (BLOCK_SIZE - 12) as u16, "..", FILE_TYPE_DIR);
    file.seek(SeekFrom::Start(lost_found_block * BLOCK_SIZE))?;
    file.write_all(&lost_found)?;

    file.sync_all()
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

struct Descriptor {
    block_bitmap: u32,
    inode_bitmap: u32,
    inode_table: u32,
    free_blocks: u16,
    free_inodes: u16,
    used_dirs: u16,
}

fn descriptor_block(descs: &[Descriptor]) -> Vec<u8> {
    let mut out = vec![0u8; BLOCK_SIZE as usize];
    for (i, d) in descs.iter().enumerate() {
        let o = i * 32;
        put32(&mut out, o, d.block_bitmap);
        put32(&mut out, o + 4, d.inode_bitmap);
        put32(&mut out, o + 8, d.inode_table);
        put16(&mut out, o + 12, d.free_blocks);
        put16(&mut out, o + 14, d.free_inodes);
        put16(&mut out, o + 16, d.used_dirs);
    }
    out
}

struct SuperblockFields<'a> {
    blocks: u64,
    inodes: u32,
    free_blocks: u64,
    free_inodes: u32,
    group: u16,
    now: u32,
    label: &'a str,
}

fn superblock(f: &SuperblockFields) -> Vec<u8> {
    let mut sb = vec![0u8; 1024];
    put32(&mut sb, 0, f.inodes);
    put32(&mut sb, 4, f.blocks as u32);
    put32(&mut sb, 8, 0);
    put32(&mut sb, 12, f.free_blocks as u32);
    put32(&mut sb, 16, f.free_inodes);
    put32(&mut sb, 20, 0);
    put32(&mut sb, 24, 2);
    put32(&mut sb, 28, 2);
    put32(&mut sb, 32, BLOCKS_PER_GROUP as u32);
    put32(&mut sb, 36, BLOCKS_PER_GROUP as u32);
    put32(&mut sb, 40, INODES_PER_GROUP);
    put32(&mut sb, 44, 0);
    put32(&mut sb, 48, f.now);
    put16(&mut sb, 52, 0);
    put16(&mut sb, 54, 0xFFFF);
    put16(&mut sb, 56, EXT2_MAGIC);
    put16(&mut sb, 58, 1);
    put16(&mut sb, 60, 1);
    put16(&mut sb, 62, 0);
    put32(&mut sb, 64, f.now);
    put32(&mut sb, 68, 0);
    put32(&mut sb, 72, 0);
    put32(&mut sb, 76, 1);
    put16(&mut sb, 80, 0);
    put16(&mut sb, 82, 0);
    put32(&mut sb, 84, FIRST_INO);
    put16(&mut sb, 88, INODE_SIZE as u16);
    put16(&mut sb, 90, f.group);
    put32(&mut sb, 92, 0);
    put32(&mut sb, 96, FEATURE_FILETYPE);
    put32(&mut sb, 100, 0);
    let label = f.label.as_bytes();
    sb[120..120 + label.len()].copy_from_slice(label);
    sb
}

/// A block or inode bitmap with the given `[start, end)` bit ranges set.
fn bitmap(ranges: &[(usize, usize)]) -> Vec<u8> {
    let mut out = vec![0u8; BLOCK_SIZE as usize];
    for &(start, end) in ranges {
        for i in start..end {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

fn put_inode(table: &mut [u8], ino: u32, mode: u16, links: u16, block0: u32, now: u32) {
    let o = (ino as usize - 1) * INODE_SIZE as usize;
    let i = &mut table[o..o + INODE_SIZE as usize];
    put16(i, 0, mode);
    put32(i, 4, BLOCK_SIZE as u32);
    put32(i, 8, now);
    put32(i, 12, now);
    put32(i, 16, now);
    put16(i, 26, links);
    put32(i, 28, (BLOCK_SIZE / 512) as u32);
    put32(i, 40, block0);
}

fn put_dirent(block: &mut [u8], at: usize, ino: u32, rec_len: u16, name: &str, file_type: u8) {
    put32(block, at, ino);
    put16(block, at + 4, rec_len);
    block[at + 6] = name.len() as u8;
    block[at + 7] = file_type;
    block[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path, at: u64, len: usize) -> Vec<u8> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).unwrap();
        buf
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }

    #[test]
    fn new_image_is_consistent() {
        let path = std::env::temp_dir().join(format!("rvemu-ext2-test-{}.img", std::process::id()));
        let size = 3 * BLOCKS_PER_GROUP * BLOCK_SIZE;
        format(&path, size, "test-root").unwrap();

        assert!(!has_populated_root(&path));
        let sb = read(&path, 1024, 1024);
        assert_eq!(u16_at(&sb, 56), EXT2_MAGIC);
        let blocks = u32_at(&sb, 4) as u64;
        assert_eq!(blocks, size / BLOCK_SIZE);
        let groups = blocks.div_ceil(BLOCKS_PER_GROUP) as usize;
        assert_eq!(u32_at(&sb, 40), INODES_PER_GROUP);
        assert_eq!(u32_at(&sb, 0), groups as u32 * INODES_PER_GROUP);
        assert_eq!(&sb[120..120 + 9], b"test-root");

        let gdt = read(&path, BLOCK_SIZE, BLOCK_SIZE as usize);
        let mut free_blocks = 0u64;
        for g in 0..groups {
            let d = g * 32;
            let start = g as u64 * BLOCKS_PER_GROUP;
            assert_eq!(u32_at(&gdt, d) as u64, start + 2);
            assert_eq!(u32_at(&gdt, d + 8) as u64, start + 4);
            free_blocks += u16_at(&gdt, d + 12) as u64;
            // Every group has its own copy of the superblock.
            let copy = read(&path, start * BLOCK_SIZE, 1024);
            if g > 0 {
                assert_eq!(u16_at(&copy, 56), EXT2_MAGIC);
                assert_eq!(u16_at(&copy, 90), g as u16);
            }
        }
        assert_eq!(free_blocks, u32_at(&sb, 12) as u64);

        // Root and lost+found are directories with the right entries.
        let root_inode = read(&path, 4 * BLOCK_SIZE + (ROOT_INO as u64 - 1) * INODE_SIZE, 128);
        assert_eq!(u16_at(&root_inode, 0), MODE_DIR_755);
        assert_eq!(u16_at(&root_inode, 26), 3);
        assert_eq!(u32_at(&root_inode, 40) as u64, META_BLOCKS);
        let dir = read(&path, META_BLOCKS * BLOCK_SIZE, 64);
        assert_eq!(&dir[8..9], b".");
        assert_eq!(&dir[20..22], b"..");
        assert_eq!(u32_at(&dir, 24), LOST_FOUND_INO);
        assert_eq!(&dir[32..42], b"lost+found");

        // Bitmaps: metadata, root and lost+found are in use; the first free block is next.
        let bb = read(&path, 2 * BLOCK_SIZE, BLOCK_SIZE as usize);
        let used = |bits: &[u8], i: usize| bits[i / 8] & (1 << (i % 8)) != 0;
        assert!(used(&bb, META_BLOCKS as usize + 1));
        assert!(!used(&bb, META_BLOCKS as usize + 2));
        let ib = read(&path, 3 * BLOCK_SIZE, BLOCK_SIZE as usize);
        assert!(used(&ib, 10) && !used(&ib, 11));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn refuses_to_overwrite() {
        let path = std::env::temp_dir().join(format!("rvemu-ext2-exists-{}.img", std::process::id()));
        std::fs::write(&path, b"keep").unwrap();
        assert!(format(&path, 16 << 20, "x").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_file(&path).unwrap();
    }
}
