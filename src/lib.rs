//! rvemu: a fast, full-system RISC-V (RV64GC) emulator.
//!
//! * `hart` / `exec` / `fpu` — the interpreter core (RV64IMAFDC, Zicsr, Zifencei)
//! * `csr` / `mmu` — privileged architecture: M/S/U modes, traps, Sv39 paging
//! * `bus` / `devices` — RAM, CLINT, PLIC, 16550 UART, SiFive test finisher
//! * `sbi` — built-in SBI for booting kernels without firmware
//! * `machine` — harts, scheduling, boot layout and the run loop
//! * `fdt`, `loader`, `initramfs` — device tree, image loading, rootfs packing

pub mod arch;
pub mod block;
pub mod bus;
pub mod console;
pub mod counters;
pub mod csr;
pub mod devices;
pub mod exec;
pub mod ext2;
pub mod fastfp;
pub mod fdt;
pub mod fpu;
pub mod hart;
pub mod initramfs;
pub mod insn;
pub mod loader;
pub mod machine;
pub mod net;
pub mod mmu;
pub mod mt;
pub mod sbi;
pub mod spec;
pub mod spec_index;
#[cfg(test)]
mod spec_test;
#[cfg(all(test, target_arch = "aarch64"))]
mod fastfp_test;

pub use bus::Exit;
pub use machine::{Machine, MachineConfig};
