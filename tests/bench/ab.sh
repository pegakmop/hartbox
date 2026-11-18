#!/bin/bash
# Interleaved comparison of emulator builds on one workload.
#
#   tests/bench/ab.sh WORKLOAD ROUNDS BIN...      (WORKLOAD is a file in tests/bench/workloads/)
#
# Prints, per binary, guest instructions per kilocycle of host CPU for each round (best
# first), the host instructions per guest instruction, and the wall-clock MIPS. Compare builds
# by the first number: it does not depend on the clock frequency, on which core the scheduler
# picked, or on idle time, so a 1 % difference is real. Wall-clock MIPS on a shared laptop swings
# by a factor of two with the load; use it only to describe a result, never to decide one.
# Removing host instructions that are never in the way (a branch that is never taken, a test
# that is always true) does not change the first number: the interpreter waits on mispredicted
# branches and instruction delivery, not on instruction count.
here=$(cd "$(dirname "$0")" && pwd)
W=$1; R=$2; shift 2
L=$(cat "$here/workloads/$W.sh")
T=$(mktemp)
for i in $(seq "$R"); do
  for b in "$@"; do
    out=$(TIMEL=/usr/bin/time\ -l timeout 600 "$here/run.sh" "$b" "$L" 2>&1)
    if echo "$out" | grep -a -q ": not found"; then
      echo "warning: a guest command was not found; the workload tools are missing from the root image" >&2
    fi
    g=$(echo "$out" | grep -a "instructions in" | sed -n 's/.*\] \([0-9]*\) instructions.*/\1/p')
    c=$(echo "$out" | grep -a "cycles elapsed" | awk '{print $1}')
    hi=$(echo "$out" | grep -a "instructions retired" | awk '{print $1}')
    m=$(echo "$out" | grep -a "instructions in" | sed -n 's/.*(\([0-9.]*\) MIPS.*/\1/p')
    echo "$(basename "$b") $g $c $hi $m" >> "$T"
  done
done
awk '{k=$1; ipc=$2/$3*1000; v[k]=v[k] " " sprintf("%.2f",ipc); if(ipc>best[k])best[k]=ipc; hs[k]+=$4/$2; n[k]++; mm[k]+=$5}
     END {for (k in v) printf "%-14s best %.1f guest insns/kcycle  runs:%s  host/guest %.2f  wall MIPS %.0f\n", k, best[k], v[k], hs[k]/n[k], mm[k]/n[k]}' "$T" | sort
rm -f "$T"
