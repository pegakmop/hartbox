# Repository index

Hartbox and the `rvemu` CLI share a Rust library that emulates an RV64GC
machine. For installation and everyday use, start with the [README](README.md).

## Code map

| Area | Start here |
| --- | --- |
| iOS app | [Swift UI and terminal](ios/RvemuApp/), [Xcode project](ios/rvemu.xcodeproj/), and [Rust bridge](ios/rvemu-ios/src/lib.rs) |
| CLI and machine | [Arguments and boot setup](src/main.rs) and [machine scheduling](src/machine.rs) |
| Boot and storage | [Initramfs and guest init](src/initramfs.rs), [root disk format and warm-boot probe](src/ext2.rs), and [virtio block device](src/devices/blk.rs) |
| RISC-V interpreter | [Harts](src/hart.rs), [decoded blocks and links](src/block.rs), [threaded dispatch](src/block/dispatch.rs), [instruction decoder](src/insn.rs), and [scalar execution](src/exec.rs) |
| Hot paths | [Instruction fusion](src/block/fusion.rs), [arithmetic and addressing idioms](src/block/idioms.rs), [paired RAM accesses](src/block/linear_memory.rs), [bulk RAM operations](src/block/bulk.rs), [integer reductions](src/block/bulk/reduce.rs), [native floating point](src/fastfp.rs), and [generated templates](src/spec_index.rs) |
| Memory and threading | [RAM and device bus](src/bus.rs), [Sv39 and TLBs](src/mmu.rs), and [parallel harts](src/mt.rs) |
| Firmware and devices | [Built-in SBI](src/sbi.rs), [device tree](src/fdt.rs), and [emulated devices](src/devices/) |
| Networking and SSH | [IPv4/IPv6 gateway](src/net/mod.rs), [DNS proxy](src/net/dns.rs), [iOS SSH integration](ios/RvemuApp/Rssh.swift), and [rssh script](tools/rssh) |

Both front ends create a machine that loads the kernel, initramfs, and devices.
The scheduler runs each hart through decoded blocks cached by physical address.
Static handlers fuse decoded operations and batch eligible RAM loops within a
page and the scheduler budget. ARM64 handlers can bring the next handler and
its operands into registers in one load. Loads and stores through one base
register can share a checked RAM translation. Ordinary instruction handlers
preserve precise traps and device accesses when a fast-path check fails. The
one-hart iOS app uses the cooperative scheduler and the same statically
compiled engine.

## Tests

Run the Rust suite with `cargo test --release`. Build the release CLI and run
[the Alpine launcher](examples/boot-alpine.sh) once to prepare the assets for
the guest tests.

- [Boot and storage smoke test](tests/e2e/smoke.sh) checks networking and file
  persistence across reboots on a temporary disk.
- [Curl test](tests/e2e/curl.sh) checks IPv4 fallback, IPv6 loopback, concurrent
  connections, and a streamed upload. Set `ROOT_IMG` to a disposable guest image
  with curl installed.
- [Four-hart test](tests/e2e/threads.sh) checks atomics, parallel guest programs,
  and instruction-cache coherence after `fence.i`.
- [ISA differential test](tests/isa/run_diff.sh) builds a freestanding RV64
  program with RISC-V Clang and LLD and compares the output with QEMU when
  available.
- [RAM loop benchmarks](tests/isa/bulk_bench.S) verify forward and backward
  copying, sentinel copying, filling, and integer reduction while measuring
  throughput for identical guest instruction counts.

## Benchmarks and history

The [benchmark guide](tests/bench/README.md) shows how to compare two release
builds with a disposable Alpine disk. The [November 18 boot and networking
results](docs/performance-results-2026-11-18-network.json) include the individual
boot, Python, LZMA, shell, and package runs. See the [November interpreter
results](docs/performance-results-2026-11-18.json) and [earlier engineering
notes](https://github.com/u6420771547-hash/hartbox/blob/v1.0.0/README.md#performance)
for historical measurements and experiments.
