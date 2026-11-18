#!/bin/bash
# Boot Alpine, type the given commands, power off, and print the emulator's `--stats` line.
#
#   tests/bench/run.sh BIN "guest commands" [extra rvemu arguments...]
#
# BIN is the emulator binary (copy target/release/rvemu first when comparing builds).
# ROOT_IMG is the persistent root image to boot (default ~/.rvemu/root.img); the workloads
# in workloads/ need python3, gcc, sqlite and 7zip installed in it (apk add python3 build-base
# sqlite 7zip). WORK is the directory the example script prepared (kernel, rootfs, overlay).
#
# The commands are queued in a fifo before the guest starts. Do not "sleep a while, then
# type": a guest that boots faster than the sleep sits idle, the idle time is part of the
# run, and the reported MIPS drop to instructions / sleep (this is how a 1000 MIPS boot once
# looked like 460 MIPS).
BIN=$1; CMDS=$2; shift 2
here=$(cd "$(dirname "$0")" && pwd)
W=${WORK:-$here/../../examples/work}
ROOT=${ROOT_IMG:-$HOME/.rvemu/root.img}
F=$(mktemp -u "${TMPDIR:-/tmp}/rvemu-bench-fifo.XXXXXX"); mkfifo "$F"
( printf '%s\nexit\n' "$CMDS"; sleep 600 ) > "$F" 2>/dev/null &
WP=$!
# The default is one host thread per fast core; the engine comparisons need the single
# hart (SMP=N or a --smp argument of your own overrides it).
case " $* " in *" --smp "*|*" -s "*) SMPARG= ;; *) SMPARG="--smp ${SMP:-1}" ;; esac
${TIMEL:-} "$BIN" $SMPARG -k "$W/vmlinuz-lts" --rootfs "$W/alpine-minirootfs-3.24.2-riscv64.tar.gz" \
  --overlay "$W/overlay" -m 512M --root-disk "$ROOT" --stats "$@" 2>&1 < "$F"
status=$?
pkill -P $WP 2>/dev/null; kill $WP 2>/dev/null; rm -f "$F"
exit "$status"
