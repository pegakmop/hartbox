# Interpreter performance continuation, 2026-10-22

The selected variant is `tlb4k-v1`. It extends the previously installed `portable-v6` engine with general interpreter state and cache improvements. All handlers are statically compiled Rust, shared by the CLI and iOS app. No JIT, executable-memory allocation, new application profiles or new application-specific templates were added.

## Engine changes

- Carry the active load-TLB pointer through threaded handlers and linked block exits in an argument register. The store table immediately follows it. This removes the repeated load of the active table pointer from the hart on fast integer and FP memory accesses. Refresh it at entry and after generic execution that may change privilege or translation state; keep the existing chain breaks on those transitions.
- On ARM64, carry the count retired before the current block through handlers in another argument register. Materialize it in the hart before cold execution, CSR access, traps and chain exits. Propagate the updated count through direct links and cached returns. Keep the live chain-limit check, so interrupts and code invalidation still break execution promptly. Other targets retain six scalar handler arguments and the existing count field.
- Increase each flat data TLB from 2,048 to 4,096 entries. This doubles its direct-mapped virtual coverage from 8 to 16 MiB without adding a guard or dispatch. Backing storage increases from 320 to 640 KiB per hart. Permission contexts, page tags, code protection and invalidation remain in force. A compile-time bound protects the 16-bit reverse-slot encoding.
- Update the existing generated-handler interface and its generator mechanically. After removing the context arguments and unused cold-fallback arguments, the generated source matches the starting templates exactly. No templates were mined or added.
- Make the GCC workload emit success only after compilation succeeds and its executable returns one of its two expected status values.

## Measurements

All numbers below use the final single batch: seven rounds, three frozen binaries, nine workloads, 189 validated guest runs. Binary order alternates between rounds. The reference is the build already installed before this continuation, rather than the older pre-overhaul engine. The runner validates completion markers and missing commands and retains every run. No compilation ran during measurement.

The selected variant has a **3.5% equal-workload geometric-mean CPU gain** over that reference. The smaller-TLB count-register variant measured 2.3% by the same calculation.

| Workload | Paired CPU gain | Host instructions per guest instruction reduced | Reference → selected wall MIPS |
| --- | ---: | ---: | ---: |
| Linux boot | +1.7% | 1.7% | 1,219 → 1,218 |
| SQLite | +1.9% | 2.0% | 1,193 → 1,228 |
| Userspace mix | +2.9% | 2.2% | 1,334 → 1,356 |
| APK update/search/info | +5.2% | 3.0% | 1,084 → 1,129 |
| Python | +2.0% | 2.3% | 1,456 → 971 |
| Python/awk FP | +3.3% | 2.2% | 1,355 → 1,391 |
| Guest GCC | +3.7% | 2.9% | 718 → 781 |
| Shell/file/compression mix | +5.8% | 3.3% | 1,090 → 1,346 |
| LZMA | +5.2% | 4.3% | 998 → 1,001 |

These are measurements on the shared Apple M5 Mac, not an iPhone. Both binaries receive the same interactive thread QoS. Host load and scheduling produced substantial outliers, including Python and compression runs; the raw data retains them. Paired ratios and separate absolute medians can therefore disagree. Treat small differences as estimates rather than a universal speed guarantee. APK includes real network updates, and wall MIPS also reflects waits.

The separate seven-round Python confirmation against the final rebuilt CLI measured +2.3% CPU efficiency. Its wall-MIPS medians were 1,551 → 1,434. These data support the CPU-efficiency improvement while leaving wall-time gains uncertain on this shared host. Both batches are retained.

Earlier experiments and all final per-run values, binary hashes and batch names are retained in [the machine-readable evidence](performance-results-2026-10-22.json). Compact 24-byte decoded instructions were rejected after repeated compression regressions and increased host instruction work. The selected representation remains 32 bytes. An earlier TLB-only state-passing variant was also compared before selecting the combined state and capacity changes.

Original logs, frozen binaries and source snapshots remain in `/tmp/rvemu-overhaul-20261022/`. Benchmarks used a disposable root-image clone, and did not modify the phone’s root image. The earlier overhaul and its separate reference measurements remain in [the October 21 report](engine-performance-2026-10-21.md).

## Validation and artifacts

Final checks passed:

- 56 release library tests, run with one test thread.
- 32,233 ISA transcript lines match the reference byte for byte, preserving its 34 existing differences from QEMU.
- All five four-hart checks, including atomics and cross-modifying code after `fence.i`.
- All 13 networking/storage smoke checks, including HTTPS and persistence across reboot.
- ARM64 counter-feature checking and x86-64 counter-feature/test type checking.
- CLI and signed iOS Release builds; the app was installed and launched on the connected iPhone 17e and its running process was verified.

Artifact and source hashes are recorded in the evidence file. The new count test compares cold and warmed calls, cached returns, memory operations, counter reads and writes against uncached instruction execution, across budgets of 1, 3, 17, 64 and 512. The translation-context test checks load and store mappings across user, supervisor, supervisor-with-SUM and identity contexts.

The CLI artifact is `target/release/rvemu`. The iOS archive is `ios/rvemu-ios/target/aarch64-apple-ios/release/librvemu_ios.a`, built by the Xcode Release target with the existing A14-compatible settings. The app uses the existing bundle identity and signing configuration.

## Research applied

The field-to-parameter approach follows the [handler-state discussion in the Graal documentation](https://github.com/oracle/graal/blob/master/truffle/docs/OneCompilationPerBytecodeHandler.md). Here it is implemented with ordinary static Rust functions. The register budget was checked against [Arm’s AAPCS64](https://github.com/ARM-software/abi-aa/blob/main/aapcs64/aapcs64.rst) and [LLVM’s tail-call constraints](https://llvm.org/docs/LangRef.html); disassembly confirms tail jumps on the ARM64 fast paths. Research on [instruction-pointer dependency chains](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.ECOOP.2024.14) also informed the review of dispatch and decoded-layout experiments.

To reproduce the final comparison with prepared, disposable guest storage:

```sh
python3 tests/bench/compare.py --root /path/to/disposable-root.img \
  --output /tmp/new-comparison --rounds 7 \
  --workloads boot sqlite mix apk py fp2 gcc shell lzma -- \
  /path/to/reference /path/to/candidate
```
