# Interpreter performance, 2026-11-16

The shared Rust interpreter batches regular RAM copy and fill loops in
cooperative execution. It identifies the loop from the decoded instructions,
checks both translations where needed, and limits each batch by page boundaries,
the branch and the remaining scheduler budget. The final register state and
retired instruction count match scalar execution. This is the execution mode
used by the iOS app. All handlers are statically compiled.

Integer triples forward both adjacent results when the decoded registers
permit it. Each flat data table has 8,192 entries, with 512 KiB of
additional backing memory per hart across all contexts. Store reverse hints retain
their existing size. The macOS CLI text segment grew by 912 KiB. The virtio
block cache uses four-way sets, reuses its transfer buffer and updates
the backing file before acknowledging writes.

## Alpine workload comparison

Five interleaved pairs per workload ran on one hart on an Apple-silicon Mac,
using a disposable copy of an Alpine disk. Both binaries used `RVEMU_NO_SPEC=1`.
CPU MIPS is millions of guest instructions per host user CPU second. Gains
are medians of the paired ratios, which can differ from the ratio of the
separate medians.

| Workload | CPU MIPS before → after | Paired CPU gain | Wall MIPS before → after |
| --- | ---: | ---: | ---: |
| Alpine boot | 985 → 1,005 | +2.26% | 1,089 → 1,113 |
| SQLite | 1,056 → 1,059 | +2.93% | 1,116 → 1,125 |
| Python | 1,351 → 1,418 | +5.02% | 1,375 → 1,443 |
| Userspace mix | 1,115 → 1,132 | +0.25% | 1,151 → 1,169 |
| LZMA | 1,246 → 1,230 | +2.65% | 1,263 → 1,248 |
| APK update/search with local indexes | 1,080 → 1,118 | +2.12% | 1,089 → 1,129 |
| Shell, files and compression | 1,103 → 1,131 | +2.47% | 1,105 → 1,134 |
| Floating point | 1,294 → 1,353 | +3.98% | 1,299 → 1,358 |
| Guest GCC | 745 → 744 | -1.01% | 746 → 745 |
| Direct disk I/O | 721 → 771 | +6.72% | 720 → 769 |

The equal-workload geometric-mean CPU gain was **2.72%** across 100
validated runs. The GCC result was 1.01% lower in this batch. Separate
wall-time medians also varied under host scheduling and I/O.

## Default settings and live APK

Five pairs ran `apk update`, search and installed-package listing against
Alpine's live repositories. CPU throughput improved **2.44%**, with a
median elapsed time of **4.06 → 3.99 seconds**. Every run validated a
package index and printed the completion marker.

Three further pairs with default template settings measured **+1.02%**
for boot, **+2.33%** for Python and **+0.86%** for shell. The default
shell's median wall time was 5.81 → 5.89 seconds. Three GCC pairs
with default settings measured **+1.16%**.

## Dedicated RAM loop benchmarks

Seven pairs used the freestanding `tests/isa/bulk_bench.S` fixture with
the same guest instruction counts in both builds.

| Operation | Guest instructions | Wall MIPS before → after | Speedup |
| --- | ---: | ---: | ---: |
| Copy | 478,184,752 | 2,304 → 13,678 | 5.94× |
| Fill | 251,674,548 | 2,429 → 10,035 | 4.13× |

## Validation

- The ARM64 iOS static library and macOS release CLI built successfully.
  The x86-64 macOS library passed type checking. The throughput
  measurements were taken on Apple-silicon macOS.
- All 71 library tests passed. The new cases compare batched loops with
  scalar execution across page boundaries, overlapping ranges and different
  instruction budgets, and verify the disk cache after a cross-page write.
- The 32,233-line ISA transcript matched the starting binary with either
  template setting. The starting build's 34 QEMU differences were unchanged.
- All five four-hart checks and all 13 queued-input networking and storage
  smoke checks passed, including cross-hart code invalidation and disk
  persistence across reboot.

The [per-run evidence](performance-results-2026-11-16.json) includes
binary hashes, instruction counts, CPU seconds and wall MIPS. The
individual logs and frozen CLI binaries are in
`/tmp/rvemu-interpreter-lC4rre/`.

Sources for interpreter dispatch and loop operands include
[Wasm3's interpreter design](https://github.com/wasm3/wasm3/blob/main/docs/Interpreter.md),
[Ertl and Paysan on interpreter instruction-pointer updates](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.ECOOP.2024.14),
and the Linux RISC-V [copy](https://github.com/torvalds/linux/blob/master/arch/riscv/lib/memcpy.S)
and [fill](https://github.com/torvalds/linux/blob/master/arch/riscv/lib/memset.S) routines.
