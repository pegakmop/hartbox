# Interpreter performance changes, 2026-10-21

The selected engine is the measured `portable-v6` variant. It uses ordinary,
statically compiled Rust handlers and requires no JIT or executable RAM. The
same engine is built into the CLI and the iOS static library.

## Changes

- Coalesce 2–8 adjacent integer loads or stores with a common, unchanged base
  and compatible displacements. A single TLB guard validates alignment and
  the complete span within one page. A miss executes the original precise
  instruction path, including MMIO, faults and writes to decoded code.
- Carry the last two architectural instruction results out of generic fused
  units. Choose dependent operand variants while decoding, avoiding register
  reads and runtime dependency comparisons. Generated runs retain opaque
  outputs, so consumers use the architectural register file there.
- Fuse fast integer/memory operations with jumps, and recognize AUIPC/JALR
  calls while preserving both register writes and existing return hints.
- Allocate decoded block entry slots in 64-slot chunks instead of allocating
  all 2,048 slots for every code page.
- Record a physical page's store-TLB slot to accelerate code protection.
  Validate stale hints; writable aliases use the conservative table scan.
- Make FP test fallback counts local to each test thread, removing a
  pre-existing race in the differential test instrumentation.

New selection rules depend on ISA operations and operands. No new application
profiles or generated application-specific templates were added. The final
address-prefix and FP-rounding experiments were discarded after comparison.

## Measurements

CPU gain is the median of per-round ratios of guest instructions per host CPU
cycle against a frozen original binary. Binary order alternates between rounds.
Wall MIPS are separate medians from the same batch, and vary with host load.

| Workload | Rounds | CPU gain | Original → selected wall MIPS |
| --- | ---: | ---: | ---: |
| Linux boot | 5 | +2.4% | 1,217 → 1,252 |
| SQLite | 5 | +5.4% | 1,116 → 1,200 |
| Userspace mix | 5 | +2.2% | 1,193 → 1,250 |
| Python | 7 | +3.7% | 1,223 → 1,247 |
| Python/awk FP mix | 5 | +7.9% | 1,032 → 1,195 |
| Shell/file/compression mix | 3 | +4.5% | 1,329 → 1,384 |
| APK update/search/info | 5 | +5.9% | 1,046 → 1,140 |
| Guest GCC | 5 | +1.9% | 826 → 840 |
| LZMA | 5 | +3.5% | 1,421 → 1,535 |

These are measurements on the shared Apple M5 Mac, not an iPhone. They do not
establish a universal speedup. Both binaries received identical interactive
thread QoS during measurement through a benchmark-only precompiled constructor
calling `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0)`.

Host load became unstable in the final Python batch, producing implausibly
large gains for one prototype; the table uses the earlier seven-round Python
batch and retains both batches in the evidence. An earlier shell batch overlapped
compilation and showed a slowdown. The three-round rerun above had compilation
stopped and validated all compression hashes. Median shell maximum RSS also
fell from about 248 MiB to 235 MiB. Treat sub-percent differences as noise.

Per-run measurements, binary hashes and batch names are in
[performance-results-2026-10-21.json](performance-results-2026-10-21.json).
Original logs and the before-change source snapshot are retained locally in
`/tmp/rvemu-overhaul-20261021/`. Benchmarks used throwaway root-image copies.

## Validation and artifacts

- 54 release library tests pass, including 2,000 randomized decoded-program
  comparisons, memory groups on hits/faults/boundaries, jump fusion, register
  dependencies, and store-TLB aliases across privilege contexts.
- The 32,233-line ISA transcript matches the original byte for byte. The
  original already differed from QEMU on 34 lines; this work preserves that
  baseline rather than claiming full QEMU agreement.
- All five four-hart guest checks pass: CPU count, atomic/lock/message/litmus
  torture, cross-modifying code after `fence.i`, parallel answers and completion.
- The iOS release build succeeds from `ios/rvemu-ios` with target
  `aarch64-apple-ios`. Its output is
  `ios/rvemu-ios/target/aarch64-apple-ios/release/librvemu_ios.a`, the archive
  referenced by the Xcode project. CLI output is `target/release/rvemu`.

To reproduce comparisons with frozen binaries and a disposable prepared image:

```sh
python3 tests/bench/compare.py --root /path/to/disposable-root.img \
  --output /tmp/new-comparison --rounds 5 \
  --workloads boot sqlite mix py fp2 shell apk gcc lzma -- \
  /path/to/original /path/to/selected
```

The runner saves every guest log and rejects incomplete runs and missing tools.
Prepare the image with Python, GCC, SQLite and 7zip first; APK needs networking.

## Research used

Static handler grouping follows the interpreter technique described in the
[Vmgen superinstruction documentation](https://www.complang.tuwien.ac.at/anton/vmgen/html-docs/Superinstructions.html).
The safety review of direct links and code protection used QEMU's discussion of
[page-local chaining and invalidation](https://www.qemu.org/docs/master/devel/tcg.html)
and [per-CPU TLBs and concurrent invalidation](https://www.qemu.org/docs/master/devel/multi-thread-tcg.html).
Those cache and coherence principles were applied to the existing interpreter.
