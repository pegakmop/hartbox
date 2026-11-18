# Benchmarking the emulator

`run.sh` boots Alpine on a root image, types a workload, powers off and prints the emulator's
`--stats` line. `ab.sh WORKLOAD ROUNDS BIN...` runs the same workload on several builds,
interleaved, and compares them by **guest instructions per kilocycle of host CPU**
(`/usr/bin/time -l` reports `cycles elapsed`). Workloads are in `workloads/` (`boot`, `gcc`,
`sqlite`, `lzma`, `py`, `gz`, `mix`); they need python3, gcc, sqlite and 7zip in the root
image: `apk add python3 build-base sqlite 7zip`.
A missing tool is easy to miss: without 7zip the `lzma` workload is only the boot (about 468 M
instructions), because the guest prints `7z: not found` and the harness shows only the stats
line. `ab.sh` now prints a warning in that case. To avoid changing your own disk, make a
throwaway image (`ROOT_IMG=/some/path/fresh.img tests/bench/run.sh BIN 'apk add 7zip' --net
--root-size 2G`) and point ROOT_IMG at it.

    cp target/release/rvemu /tmp/rvemu_a      # freeze each build before comparing
    ROOT_IMG=/path/to/root.img tests/bench/ab.sh gcc 3 /tmp/rvemu_a /tmp/rvemu_b

`compare.py` validates completion markers and keeps per-run logs and JSON.
When CPU cycle counters are unavailable, it compares millions of guest
instructions per host user CPU second and still records wall MIPS. Set
`TIMEL=` to select that metric explicitly. `apk` uses the configured live
mirrors. `apk-local` uses cached, signed package indexes from a previously
prepared root image. `disk` writes, syncs and reads back a temporary file
using aligned direct I/O. It needs Python on the disposable disk.

    TIMEL= RVEMU_NO_SPEC=1 python3 tests/bench/compare.py --rounds 5 \
        --root /tmp/rvemu-bench-root.img --output /tmp/rvemu-bench-results \
        --workloads boot sqlite mix lzma apk-local shell fp2 gcc disk \
        -- /tmp/rvemu_a /tmp/rvemu_b

The freestanding `tests/isa/bulk_bench.S` benchmark isolates the cooperative
interpreter's RAM loops. Build it with the same RISC-V Clang and LLD used for
`tests/isa/run_diff.sh`. Add `-DCOPY` for copy throughput. The default build
measures fill throughput.

    clang --target=riscv64-unknown-elf -march=rv64gc -mabi=lp64d -nostdlib \
        -fuse-ld=ld.lld -Wl,-T,tests/isa/link.ld tests/isa/bulk_bench.S -o /tmp/bulk-fill.elf
    target/release/rvemu --program /tmp/bulk-fill.elf -m 128M --smp 1 --stats --no-raw

Use `-DWORD_FILL` for a repeated 64-bit value with different bytes. Build
`tests/isa/stream_bench.S` with the same flags to measure search throughput,
or add `-DCOMPARE` for two-stream comparison. The stream fixture verifies
the guest's final values and positions before printing `STREAM_OK`.

Three rules learned the hard way:

* **Never sleep before the guest's first command.** The commands are queued in a fifo before
  the guest starts. A harness that waits one second and then types makes every run that boots
  faster than a second end at about one second: a 469 M instruction boot that really takes
  0.44 s (1060 MIPS) looked like 460 MIPS for several rounds.
* **Compare by cycles, not by wall-clock MIPS.** The test machine is a fanless laptop that
  runs other jobs, and the MIPS of the same binary swing by a factor of two with the load.
  Instructions per cycle do not, and are repeatable to about 1 %. The first run of a freshly
  written binary is delayed several seconds by Gatekeeper, so it starts into an idle machine and
  looks much faster; discard it.
* **Check that the binaries differ, and use 5 rounds.** A build that fails leaves the previous
  `target/release/rvemu` in place, and copying it under a new name compares a binary with itself:
  run `md5 -r` on the frozen binaries and build with a script that stops at the first `error`.
  The machine is shared, so identical binaries differ by 1 to 1.5 % in best-of-three, and a
  noisy minute moves a single run by 15 %: take the median of 5 or more interleaved rounds, or
  the median of the per-round ratios, and do not believe a gain of 2 % or less from fewer.
  (In zsh, which the agent's shell is, an unquoted `$v` is not split into words: `for v in "a b";
  do script $v; done` passes one argument.)
* **Fewer host instructions is not faster.** The interpreter waits on mispredicted branches
  and instruction delivery (the CPU Counters instrument shows about 19 % of the issue slots
  useful on gcc, 40 % on LZMA), so removing a test that is never in the way changes nothing. A change
  has to remove a dispatch, a mispredict or a slow path to show up.

Multi-hart runs (`run.sh BIN CMDS --smp N`): each hart is a host thread, so guest instructions per
kilocycle of the whole process is the sum over threads and says little about one thread's speed.
Compare engines with the default single hart as above. To measure how a workload scales, use the
wall-clock MIPS of the `--stats` line and the work done (instructions) together, and keep `N` at the
number of fast cores (`--smp auto`: 4 on an M5 MacBook Air): the efficiency cores run a thread at
about half speed and the guest's barriers wait for them. Guest time follows the host clock in this
mode, so a guest benchmark such as `7z b` reports real speed and `timeout N` is real seconds. A
workload with parallelism of its own is needed (`7z b`, parallel gcc, four CPython processes): a
one-process workload does not get faster with more harts.

Profiling without root: `sample PID 4 1 -file out.txt` (all threads; the interpreter is made of
tail calls, so every handler is a leaf) and `xctrace record --template 'CPU Counters' --attach PID
--time-limit 3s` (stall breakdown per 10 ms; the `MetricAggregationForProcess` table has it).

The default is one hart; `run.sh` adds `--smp 1` unless you pass
`--smp` yourself or set `SMP=N` (`SMP=auto` for the default). Engine A/B comparisons need the single
hart; `workloads/par.sh` (four parallel compiles and four CPython loops) is the multi-hart workload.
