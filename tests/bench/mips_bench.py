#!/usr/bin/env python3
"""Guest-side throughput test for rvemu: one minute of real time by default.

Run it inside Alpine, after `apk add python3`:

    python3 /root/mips_bench.py              # 60 s, three phases of 20 s
    python3 /root/mips_bench.py --seconds 9  # three phases of 3 s

The time is split over three phases:

  1. Python integer loop: CPython bytecode doing 64-bit xorshift mixing. This is
     what most Python code spends its time on.
  2. SHA-256 over 1 MiB: compiled C inside CPython. Mostly 32-bit ALU and shifts.
  3. zlib level 6: compiled C with branches and table lookups.

Timing uses the emulated RTC, not the guest clock. The guest clock (`time.time`,
`time.perf_counter`) advances with retired instructions, so it runs at a fixed
virtual speed and says nothing about the host. The RTC reads the host wall clock.
Each phase starts on one RTC second boundary and ends on a later one, so its real
length is exact to the host clock.

The guest cannot count the instructions it retires. For MIPS, run the same command
on the host with `--stats`: the emulator prints the instruction count and MIPS when
the guest powers off. Boot takes about a second, so that figure is close to the
benchmark's own rate. Use a persistent root (the default) so Python stays installed.
"""

import argparse
import hashlib
import platform
import sys
import time
import zlib

RTC_SINCE_EPOCH = "/sys/class/rtc/rtc0/since_epoch"
MASK = (1 << 64) - 1
# Integer operations in one pass of the Python loop below, and passes per step.
OPS_PER_PASS = 10
PASS_SIZE = 1000


def rtc_seconds():
    """Host wall-clock seconds since the epoch, read from the emulated RTC."""
    with open(RTC_SINCE_EPOCH) as f:
        return int(f.read())


def run_for(seconds, step):
    """Call step() until the host clock has moved `seconds` whole seconds.

    The start is the first RTC second after the call, and the end is the RTC
    second `seconds` later, so the real time is `seconds`, with an error of one
    step() at the end. Returns the number of step() calls.
    """
    start = rtc_seconds()
    while rtc_seconds() == start:
        pass
    start = rtc_seconds()
    calls = 0
    while rtc_seconds() < start + seconds:
        step()
        calls += 1
    return calls


def python_loop(seconds):
    """CPython bytecode: 64-bit xorshift mixing. Returns integer ops per second."""
    state = {"x": 0x9E3779B97F4A7C15, "acc": 0}

    def step():
        a, b = state["x"], state["acc"]
        for _ in range(PASS_SIZE):
            a ^= (a << 13) & MASK
            a ^= a >> 7
            a ^= (a << 17) & MASK
            b = (b + a) & MASK
        state["x"], state["acc"] = a, b

    calls = run_for(seconds, step)
    return calls * PASS_SIZE * OPS_PER_PASS / seconds


def sha256_rate(seconds, buf):
    """SHA-256 of `buf` again and again. Returns MB/s."""
    calls = run_for(seconds, lambda: hashlib.sha256(buf).digest())
    return calls * len(buf) / seconds / 1e6


def zlib_rate(seconds, buf):
    """zlib level-6 compression of `buf` again and again. Returns MB/s."""
    calls = run_for(seconds, lambda: zlib.compress(buf, 6))
    return calls * len(buf) / seconds / 1e6


def main():
    parser = argparse.ArgumentParser(description="rvemu guest throughput test.")
    parser.add_argument(
        "--seconds",
        type=int,
        default=60,
        help="total real time, split evenly over the three phases [default: 60]",
    )
    args = parser.parse_args()
    per_phase = max(1, args.seconds // 3)

    try:
        rtc_seconds()
    except OSError:
        sys.exit(f"{RTC_SINCE_EPOCH} is missing: the RTC is needed to time real seconds")

    # Inputs are built before the timers start.
    sha_buf = bytes(range(256)) * 4096  # 1 MiB
    text = b"".join(b"record %08d value %d\n" % (i, i * i % 9973) for i in range(10000))

    print(f"python {platform.python_version()} on {platform.machine()}")
    print(f"three phases of {per_phase} s each, host wall-clock time\n")

    ops = python_loop(per_phase)
    print(f"  python integer loop : {ops / 1e6:8.2f} M ops/s")
    mbs = sha256_rate(per_phase, sha_buf)
    print(f"  sha256, 1 MiB       : {mbs:8.2f} MB/s")
    mbs = zlib_rate(per_phase, text)
    print(f"  zlib level 6        : {mbs:8.2f} MB/s")
    print(f"\ntotal {3 * per_phase} s")


if __name__ == "__main__":
    main()
