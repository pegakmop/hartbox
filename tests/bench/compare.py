#!/usr/bin/env python3
"""Interleave frozen emulator builds and retain validated, per-run evidence.

Use a throwaway copy of a prepared root image: guest workloads write files.
Use CPU cycles when available and guest instructions per user CPU second
otherwise. Record wall speed separately.
TIMEL can override the timer, identically for every build.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import resource
import signal
import statistics
import subprocess


MARKERS = {
    "boot": "BOOT_ONLY", "sqlite": "SQLITE_DONE", "mix": "MIX_DONE",
    "py": "PY_DONE", "fp2": "FP2_DONE", "gcc": "GCC_DONE",
    "lzma": "LZMA_DONE", "shell": "SHELL_DONE", "apk": "APK_DONE",
    "apk-local": "APK_LOCAL_DONE", "disk": "DISK_DONE",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path, help="throwaway prepared root image")
    parser.add_argument("--output", required=True, type=Path, help="new directory for logs and results")
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--timeout", type=float, default=90, help="seconds per guest run")
    parser.add_argument("--workloads", nargs="+", choices=MARKERS, required=True)
    parser.add_argument("binaries", nargs="+", type=Path, help="frozen binaries, baseline first; separate with --")
    args = parser.parse_args()
    if args.rounds < 1 or args.timeout <= 0:
        parser.error("rounds and timeout must be positive")
    binaries = [path.resolve(strict=True) for path in args.binaries]
    root = args.root.resolve(strict=True)
    manifest = {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in binaries}
    if len(manifest) != len(binaries) or len(set(manifest.values())) != len(binaries):
        parser.error("each binary must have a distinct path and contents")
    args.output.mkdir(parents=True, exist_ok=False)
    here = Path(__file__).resolve().parent
    env = os.environ.copy()
    env["ROOT_IMG"] = str(root)
    env.setdefault("TIMEL", "/usr/bin/time -l" if platform.system() == "Darwin" else "/usr/bin/time -v")
    records = []
    evidence = {"manifest": manifest, "root": str(root), "timer": env["TIMEL"], "records": records}
    result_path = args.output / "results.json"
    for workload in args.workloads:
        command = (here / "workloads" / f"{workload}.sh").read_text()
        for round_index in range(args.rounds):
            order = range(len(binaries)) if round_index % 2 == 0 else reversed(range(len(binaries)))
            for index in order:
                binary = binaries[index]
                argv = [str(here / "run.sh"), str(binary), command]
                if workload == "apk":
                    argv.append("--net")
                start_cpu = resource.getrusage(resource.RUSAGE_CHILDREN).ru_utime
                with subprocess.Popen(argv, env=env, stdout=subprocess.PIPE,
                                      stderr=subprocess.STDOUT, start_new_session=True) as proc:
                    try:
                        raw, _ = proc.communicate(timeout=args.timeout)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        raw, _ = proc.communicate()
                        (args.output / f"{workload}-{round_index}-{index}.log").write_bytes(raw)
                        raise RuntimeError(f"{workload}: {binary} exceeded {args.timeout}s")
                cpu_seconds = resource.getrusage(resource.RUSAGE_CHILDREN).ru_utime - start_cpu
                output = raw.decode(errors="replace").replace("\r", "")
                log = f"{workload}-{round_index}-{index}.log"
                (args.output / log).write_text(output)
                stats = re.search(r"\[rvemu\] (\d+) instructions in ([\d.]+) s \(([\d.]+) MIPS", output)
                cycles = re.search(r"(\d+)\s+cycles elapsed", output)
                retired = re.search(r"(\d+)\s+instructions retired", output)
                marker = re.search(r"^" + MARKERS[workload] + r"(?: .*)?$", output, re.MULTILINE)
                if proc.returncode or not stats or not marker or ": not found" in output or cpu_seconds <= 0:
                    raise RuntimeError(f"{workload}: {binary} failed; see {args.output / log}")
                guest, seconds, mips = map(float, stats.groups())
                count = int(cycles[1]) if cycles else None
                records.append({"workload": workload, "round": round_index,
                                "binary": str(binary), "guest": int(guest), "seconds": seconds,
                                "mips": mips, "cycles": count,
                                "user_cpu_seconds": cpu_seconds,
                                "cpu_mips": guest / cpu_seconds / 1e6,
                                "guest_per_kcycle": guest / count * 1000 if count else None,
                                "host_instructions": int(retired[1]) if retired else None, "log": log})
                result_path.write_text(json.dumps(evidence, indent=2) + "\n")
        rows = [row for row in records if row["workload"] == workload]
        metric = "guest_per_kcycle" if all(row["cycles"] for row in rows) else "cpu_mips"
        for binary in binaries:
            measured = [row for row in rows if row["binary"] == str(binary)]
            ratios = []
            for row in measured:
                baseline = next(item for item in rows if item["binary"] == str(binaries[0])
                                and item["round"] == row["round"])
                ratios.append(row[metric] / baseline[metric])
            middle = statistics.median(row[metric] for row in measured)
            gain = (statistics.median(ratios) - 1) * 100
            print(f"{workload:8} {binary.name:20} {metric} {middle:.2f}, paired gain {gain:+.2f}%", flush=True)


if __name__ == "__main__":
    main()
