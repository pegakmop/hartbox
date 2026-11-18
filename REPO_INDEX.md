# Repository index

Full-system RV64GC emulator. The Rust library is shared by the CLI and the iOS C ABI.

| Area | Entry points and responsibilities |
| --- | --- |
| CLI and boot | `src/main.rs`: CLI, assets, root disk, configuration; `src/machine.rs`: machine construction, scheduling, device service, idle |
| Execution engine | `src/hart.rs` holds architectural state and runs each hart. `src/block.rs` builds traces, owns the per-hart arena and resolves exits. `src/block/fusion.rs` handles integer, memory and branch fusion. `src/block/idioms.rs` folds decoded address and rotate instructions. `src/block/writeback.rs` groups memory accesses with an address-register update. `src/block/bulk.rs` and `src/block/bulk/` batch regular RAM loops. |
| ISA | `src/insn.rs` decodes compressed and 32-bit instructions and caches their ISA operands. `src/exec.rs` handles precise integer, memory and system execution. `src/fpu.rs` handles software F/D semantics and `src/fastfp.rs` contains native FP handlers. |
| Privileged architecture | `src/arch.rs`: constants; `src/csr.rs`: CSRs; `src/mmu.rs`: Sv39 walker, permissions, translation caches; `src/sbi.rs`: built-in SBI |
| Memory and concurrency | `src/bus.rs`: RAM/MMIO and code coherence; `src/mt.rs`: host thread per hart, interrupts, atomic coordination |
| Existing static specialisations | `src/spec.rs`: generated exact-instruction superinstructions; `src/spec_index.rs`: lookup filter; `tools/gen_spec.py`, `tools/spec_to_runs.py`: generators; `src/counters.rs`: optional profiling |
| Devices | `src/devices/`: UART, interrupt/timer controllers, RTC, virtio transport, block, network and entropy devices. `src/devices/blk.rs` owns the write-through host file cache. |
| Network | `src/net/mod.rs`: IPv4/IPv6 gateway, TCP handshakes and UDP/ICMP proxying. `src/net/dns.rs` bridges guest UDP and TCP DNS to the Apple resolver. `src/net/host.rs` dials host TCP sockets, including DNS64 addresses on Apple platforms. |
| Filesystems and loading | `src/ext2.rs`: root image formatter and read-only warm-boot probe. `src/initramfs.rs`: cpio and guest init. `src/loader.rs`: ELF/Image/raw loading. `src/fdt.rs`: device tree. |
| iOS | `ios/rvemu-ios/src/lib.rs`: C ABI; `ios/RvemuApp/`: Swift UI, terminal, networking integration, resources |
| Unit/differential tests | Inline Rust tests, `src/block/overhaul_tests.rs` for scalar/threaded equivalence, `src/spec_test.rs` for templates, and `src/fastfp_test.rs` for floating point |
| Guest tests | `tests/isa/` covers freestanding ISA/QEMU transcripts and RAM throughput. `tests/e2e/curl.sh` exercises address fallback, IPv6, concurrent connections and a streamed upload. `tests/e2e/` also checks persistent storage and four-hart code coherence. `tests/bench/compare.py` interleaves Alpine workloads and verifies their completion markers. |
| Build and launch | `Cargo.toml`, `.cargo/config.toml`, `examples/boot-alpine.sh`, `tools/pgo.sh` |

## Execution and memory paths

`main` hands control to `Machine`, which schedules `Hart::run`. The hart
checks its virtual block cache, falls back to a physical page's decoded
block, then decodes from RAM if needed. A 32 KiB per-hart cache reuses ISA
operands by instruction encoding. Block offsets, forwarding and links are
assigned for each trace. Fixed same-page exits link directly to the first
decoded instruction of the successor. Fixed cross-page exits cache a
link slot and validate its virtual target and fetch stamp. Indirect jumps
check the last target before using the general resolver. A miss records the
edge for the run loop to fill after translation.

ARM64 passes the block PC and remaining budget in registers. x86-64 keeps
both in the hart so the six remaining arguments fit in integer registers.
Threaded harts poll between chains capped at 2,048 instructions, with up to
one decoded block of budget overshoot. Slow instructions that change control
state or observe a code write return immediately. Faulting attempts advance
scheduler progress while guest `minstret` counts retired instructions.

Dense opcode tables select fusion handlers. Triples can pass both adjacent
results directly to the next operation. Commutative instructions can take a
forwarded right operand, and a store can take its value directly from the
preceding result. Pairs and triples can skip overwritten register writes and
restore the visible intermediate on a slow path. Compatible mixed-width memory accesses share
translation and bounds checks. PC-relative jumps precompute both target and
return offsets during block construction. These rules use decoded ISA
operands for every guest.

Independent adjacent memory operations probe both data TLB entries before
the first access and commit in guest order. Groups based on an updated
address register share one translation. Address pairs can fold two
displacements, and a PC-relative load followed by an indirect jump reads
the target from the live mapping on every execution. The rotate handler
keeps the shift result that remains visible in the guest register file.

The cooperative interpreter batches regular RAM copies, repeated-value
fills, sequential reads and two-stream comparisons. ARM64 uses Advanced
SIMD for searches and comparisons. Operands and branch dependencies choose
these handlers. Each batch checks its live translations and stays within a
guest page for each stream. A possible fault, MMIO access or unsafe copy
overlap uses the ordinary instruction handlers. Batches materialize guest
registers and account for every guest instruction within the scheduler
budget. The CLI and iOS use the same statically compiled interpreter.
[RISC-V load and store semantics](https://docs.riscv.org/reference/isa/v20240411/unpriv/rv64.html),
[Arm Advanced SIMD intrinsics](https://arm-software.github.io/acle/neon_intrinsics/advsimd.html),
[Arm memory-comparison implementation](https://github.com/ARM-software/optimized-routines/blob/master/string/aarch64/memcmp.S).

Threaded memory steps carry the active flat load TLB pointer. The store
table follows it. Each flat table has 8,192 entries, with 1 MiB of
backing storage across the four load/store contexts per hart. Store
reverse hints encode only store slots and scan populated groups for
aliased pages. ARM64 fetches each tag and address addend together.
Loads and stores can recover displaced translations. Recovered stores go
through the bus so writes to decoded code still invalidate it. The two-way
4 KiB and superpage walk caches use generation tags for full fences.
An ordinary page fence clears matching fast entries and fetch shortcuts.
A fence covering a cached superpage also clears its derived entries.
The walker retries if another hart replaces a PTE before its accessed/dirty
update. [QEMU software TLB design](https://github.com/qemu/qemu/blob/master/include/hw/core/cpu.h),
[RISC-V supervisor architecture](https://github.com/riscv/riscv-isa-manual/blob/main/src/priv/supervisor.adoc).

The decoder marks source halfwords by bitmap word. Overlapping CPU writes
or DMA bump the page generation and shared code epoch. Fetch privilege is
part of the virtual block cache stamp. Threaded `fence.i` stops a chain that
observes a newer code epoch. Each hart reclaims its decoded arena on drop.
The same-page linking invariant also appears in [QEMU's translation internals](https://www.qemu.org/docs/master/devel/tcg.html).

Virtio block I/O reuses its transfer buffer and keeps a four-way
set-associative read cache. Writes update the backing file before
acknowledgement and refresh resident pages. Streaming reads bypass the
cache.

Earlier performance work and comparisons are recorded in the [October 22
report](docs/engine-performance-2026-10-22.md).

## November 18 networking and boot update

The previous gateway sent a guest SYN-ACK before dialing the host socket.
The reproduced HTTPS request selected an unreachable IPv6 destination and
then reset during TLS. The gateway now waits for the host connection and
reports a failed dial during the guest handshake. The same request fell back
to IPv4 and validated the server certificate. Local tests also verified IPv6
loopback, four parallel connections, and a checksum-checked 2 MiB upload.
Live DNS over UDP and TCP, the explicit resolver override and Alpine
`apk update` completed on disposable guest disks.

The CLI uses its smaller boot archive after a read-only check finds the
provisioning marker and an executable BusyBox on a supported persistent
root. `--full-initramfs` keeps the boot path comparable between builds. An
8 MiB test disk filled during provisioning and booted to a recovery shell
from memory. First boot on a writable disk and persistence across reboots
also passed.

Five paired runs with the default settings on an Apple M5 measured the
following release results. CPU MIPS counts millions of guest instructions
per host user CPU second. Elapsed time covers guest execution after host
preparation.

| Workload | Median elapsed time before → after | Paired CPU MIPS gain |
| --- | ---: | ---: |
| Persistent boot | 0.379 → 0.336 s | +3.78% |
| Python | 1.392 → 1.363 s | +0.50% |
| LZMA | 1.538 → 1.511 s | +0.64% |

Five more pairs with matching full initramfs and generated templates
disabled measured −0.41% for boot, +1.40% for Python, and +1.29% for LZMA.
Three pairs with default settings measured +0.43% CPU MIPS on shell and
file tools and +1.95% on cached package operations. Live `apk update` and
package queries measured −0.23% CPU MIPS, with median elapsed time falling
from 3.604 to 3.484 seconds.

Python printed the same checksum and compression result on both builds.
LZMA decompression matched the input byte for byte. The release passed
78 unit tests, five four-hart checks, and 13 storage and network smoke
checks. Both template settings reproduced the 32,233-line ISA transcript.
The unsigned ARM64 iOS app built successfully. The per-run archive also
includes comparable full-initramfs timings and the shell and package runs.
[Per-run measurements](docs/performance-results-2026-11-18-network.json).

## November 17–18 validation

Five interleaved pairs per workload ran on one hart with `RVEMU_NO_SPEC=1`
on both binaries. CPU MIPS counts millions of guest instructions per host
user CPU second. The geometric-mean paired gain across ten workloads was
**1.28%**.

| Alpine workload | CPU MIPS before → after | Paired gain |
| --- | ---: | ---: |
| Shell, file tools and compression | 1,281 → 1,291 | +0.41% |
| Cached package index update and search | 1,143 → 1,195 | +3.33% |
| Boot | 920 → 903 | −1.28% |
| SQLite | 936 → 983 | +2.65% |
| Python | 1,152 → 1,171 | −0.52% |
| Mixed utilities | 941 → 965 | +2.63% |
| LZMA | 1,124 → 1,113 | −0.87% |
| Floating point | 1,270 → 1,284 | +0.22% |
| Guest GCC | 722 → 720 | +1.75% |
| Direct disk I/O | 708 → 742 | +4.70% |

Paired gains use the median of the run-by-run ratios. The separate MIPS
medians can move differently. Four more pairs with the default template
settings measured **+2.68%** for shell and file tools, with median wall time
of 6.052 → 5.913 seconds.

Five interleaved pairs with the default template settings ran `apk update`,
search and installed-package listing against Alpine's public repositories.
All ten runs completed. The median paired CPU throughput gain was 2.12%,
and median wall time was 3.552 → 3.478 seconds on Apple-silicon macOS.

The ARM64 iOS static library built successfully. All 76 release unit tests,
five four-hart checks and 13 network and storage checks passed. Both template
settings produced the same 32,233-line ISA transcript as the starting CLI.
The starting CLI's 34 differences from QEMU were unchanged.

In freestanding tests with identical guest instruction counts, the median
paired wall-throughput gains were **63.2×** for a sequential search,
**80.8×** for two-stream comparison and **9.6×** for repeated 64-bit values
with different bytes. The existing copy and repeated-byte fill paths both
measured 1.02×. [Per-run measurements](docs/performance-results-2026-11-18.json).

## November 16 validation

The final interpreter gained 2.72% geometric-mean CPU MIPS across ten
Alpine workloads. Dedicated RAM copies ran 5.94× as fast and fills
ran 4.13× as fast. The [November 16 benchmark report](docs/engine-performance-2026-11-16.md)
records the workload comparisons and correctness checks.

## November 11 validation

Three interleaved pairs per workload compared the release CLI with a frozen
copy of the starting binary on ARM64 macOS. Both builds ran on one hart
with `RVEMU_NO_SPEC=1`. Throughput is millions of guest instructions per
host user CPU second. Gains are medians of the paired ratios.

| Workload | Starting → updated CPU MIPS | Paired gain |
| --- | ---: | ---: |
| Alpine boot | 1,018 → 1,027 | +0.8% |
| SQLite | 1,076 → 1,091 | +1.5% |
| Python | 1,335 → 1,358 | +1.5% |
| Python and awk floating point | 1,425 → 1,505 | +5.3% |
| Shell, files and gzip | 1,240 → 1,271 | +2.4% |
| Guest GCC | 846 → 854 | +0.9% |

The equal-workload geometric-mean gain was **2.1%**. The mixed workload
was excluded because the disposable root image lacked 7-Zip. The mixed
and LZMA scripts now require their file and compression commands to
succeed before reporting completion.

With default template settings, three paired rounds measured **+2.0%**
for boot, **+6.4%** for Python and **+3.8%** for GCC.

The CLI and ARM64 iOS static library built successfully. ARM64 and x86-64
counter builds passed type checking. The 32,233-line ISA transcript matched
the starting build with either template setting. All five four-hart
checks passed, including code modification after `fence.i`. The library
suite passed 67 tests. Two tests that bind localhost sockets were skipped
in this workspace. The ARM64 CLI text section grew by 3.7%, and the
encoding cache adds 32 KiB per hart.

Research references for dispatch and architectural checks include
[efficient threaded interpreters](https://jilp.org/vol5/v5paper12.pdf),
[interpreter instruction-pointer dependencies](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.ECOOP.2024.14),
and the [RISC-V machine-level ISA](https://docs.riscv.org/reference/isa/priv/machine.html).

## November 10 validation

Three interleaved rounds against the starting CLI on Apple silicon gave a
3.3% geometric-mean gain in guest instructions per user CPU second across
Alpine boot, SQLite, Python, floating point, shell tools and GCC. Individual
gains were 2.0–5.2%. A synthetic load loop alternating between pages 16 MiB
apart gained 37.4%, including guest startup. Boot, Python and GCC also gained
2.3–4.4% in a separate two-round run with `RVEMU_NO_SPEC=1` on both builds.

The CLI and ARM64 iOS release library built successfully. The x86-64 tests
and counters configuration passed type checking. The 32,233-line guest ISA
transcript matched the starting build byte for byte, and all five four-hart
checks passed, including code modification after `fence.i`. The library suite
passed 59 tests with two socket-binding tests skipped because localhost binds
are restricted in this workspace.

Useful searches:

```sh
rg -n 'fn (run|next_block|decode_page_block|exec_block|finish_direct_to|follow_or_return)' src
rg -n 'fusion_indices|overwrite_pair|mixed_memory_pair|pcrel_jump' src/block.rs src/block
rg -n 'independent_memory|address_pair|load_jump|rotate|writeback::plan' src/block.rs src/block
rg -n 'ftlb_fill|recover_(load|store)_mapping|clear_fast_table|sfence_vma|sync_code_marks' src
rg -n 'mark_code|forget_code|page_gen|code_epoch' src
rg -n 'step_bulk|iterations_to_bound|first_stop|loop_ends|DiskCache' src
```

The checkout has no `.git` directory. The comparisons use a frozen copy of
the starting release binary and a disposable guest disk.
