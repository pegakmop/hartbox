# Hartbox

Hartbox boots Alpine Linux on iPhone and iPad and gives you a local terminal.
Its Rust emulator, `rvemu`, also runs as a command-line program. It runs RV64GC
(RV64IMAFDC with Zicsr and Zifencei), implements the privileged architecture
(M/S/U modes, traps, Sv39 virtual memory), and boots **Linux 6.18** and
**Alpine Linux 3.24 from its `minirootfs` tarball** to an interactive shell.

It has no JIT. It is an interpreter that caches decoded basic blocks, a software TLB,
and an idle-aware scheduler. It has a persistent root disk, virtio disks, a virtio
network card with IPv4 and IPv6 access through the host, an entropy source, and a
real-time clock.

```
$ examples/boot-alpine.sh
Welcome to Alpine Linux 3.24.2 (riscv64)

(none):~# wget -qO- http://example.com | head -c 60
<!doctype html><html lang=en><head><meta charset=utf-8><link
(none):~# exit
```

The [repository index](REPO_INDEX.md) maps the current interpreter. Earlier
measurements are in the [October 22 performance notes](docs/engine-performance-2026-10-22.md).

## Build

The repository's `.cargo/config.toml` aligns every function to 64 bytes and targets
`apple-m1` on Apple silicon Macs (`ios/rvemu-ios/.cargo/config.toml` does the alignment
for the iPhone build). Plain `cargo build --release` picks both up.

Requires a recent stable Rust toolchain (1.87 or newer).

```sh
cargo build --release          # binary: target/release/rvemu
cargo test --release           # unit tests
tools/pgo.sh                   # optional: profile-guided build, +1 to 2 %, target/pgo/rvemu
```

### iPhone and iPad

Build the Hartbox scheme in Xcode with the Rust iOS target installed.
Select your Apple Developer team to archive and export a signed app.

```sh
rustup target add aarch64-apple-ios
open ios/rvemu.xcodeproj
```

## Boot Alpine

```sh
examples/boot-alpine.sh                       # network, entropy, RTC, persistent root
examples/boot-alpine.sh --disk disk.img       # plus a data disk (/dev/vdb)
examples/boot-alpine.sh --ephemeral           # root in memory: nothing is kept
examples/boot-alpine.sh --smp auto            # a host thread per fast core: 7z b, make -j run in parallel
examples/boot-alpine.sh --smp 4 --bios fw_dynamic.bin
examples/boot-alpine.sh -v                    # show the full kernel log
```

The root filesystem is kept on a disk image, `~/.rvemu/root.img`. rvemu creates it on
the first run (8 GiB, sparse, so it uses only the space it fills), copies the
rootfs onto it, and boots from it. Files, packages and edits survive reboots. Use
`--root-disk FILE` for another image, `--root-size SIZE` for the size of a new one,
or `--ephemeral` to keep the root in memory as before.

The first run downloads the Alpine kernel, the module initramfs, the minirootfs,
and the CA certificates from the Alpine mirror, and builds an **overlay** directory
(`examples/work/overlay`) from them. Later runs reuse it. The script takes the
rootfs from `ROOTFS=...` if you have one already.

Without the script:

```sh
rvemu -k vmlinuz-lts --rootfs alpine-minirootfs-3.24.2-riscv64.tar.gz \
      --overlay work/overlay --net --disk disk.img -m 512M
```

To reset the root filesystem, delete its image. The next run creates and fills a new one.

The CLI checks its persistent disk for a completed root before booting. A prepared
disk uses a smaller initramfs and starts directly from the installed root. Use
`--full-initramfs` to force the recovery path or keep boot work comparable in a benchmark.

### What the guest gets

| Feature | How | Inside the guest |
|---|---|---|
| Console | NS16550A UART | `/dev/ttyS0`, the shell |
| Root filesystem | on by default, see above | `/dev/vda` (ext4, persistent); `ext4` on `/` |
| Disks | `--disk FILE[,ro]` (repeatable) | `/dev/vdb`, `/dev/vdc`, ... after the root disk (virtio-blk); `/dev/vda` with `--ephemeral` |
| Networking | `--net` | `eth0`: 10.0.2.15/24 and fd00:2::15/64; gateway 10.0.2.2 and fd00:2::2; DNS 10.0.2.3 and fd00:2::3 |
| Entropy | on by default (`--no-rng` to drop it) | `/dev/hwrng` (virtio-rng) |
| Real-time clock | always on | `/dev/rtc0`, `date` and `hwclock` report the host's time |
| Inbound ports | `--forward [BIND:]HOST:GUEST` (repeatable, needs `--net`) | the guest's port is at `127.0.0.1:HOST` on the host (`BIND` such as `0.0.0.0` makes it reachable from the network) |

The virtio drivers are kernel modules. The example overlay supplies them under
`/lib/rvemu/`, and the generated `/init` loads only the ones the run needs: the
core, plus `virtio_net` with `--net`, `virtio_blk` with `--disk`, and `virtio-rng`
unless `--no-rng`. Loading a module costs boot time under emulation, so the
filesystem drivers are not loaded at boot.

### Networking

`--net` gives the guest internet access through the host, with no host
configuration and no privileges:

* **TCP** uses smoltcp on the guest side. The gateway acknowledges guest connections
  after the host connection opens, enabling `curl` and other clients to fall back
  to another address when a route fails. The guest receive window also applies
  backpressure during large uploads.
* **DNS** uses 10.0.2.3 and fd00:2::3 over UDP and TCP. On iOS and macOS, the
  gateway uses the Apple system resolver, with the configured `resolv.conf`
  server available as a fallback. `--dns-server ADDR` provides a direct IPv4 or
  IPv6 resolver for the CLI. [Apple resolver API](https://github.com/apple-oss-distributions/libresolv/blob/main/dns.h),
  [DNS over TCP](https://www.rfc-editor.org/rfc/rfc7766).
* **UDP** is proxied per flow.
* **ICMP echo** is proxied through unprivileged host ICMP sockets, so `ping`
  reaches the internet. Pinging 10.0.2.2 answers from the gateway itself.
* **ARP** is answered by the gateway for every address except the guest's own.
* **IPv6** (on by default; `--no-ipv6` turns it off) uses the ULA prefix fd00:2::/64.
  The gateway answers neighbor discovery for every address except the guest's own,
  so the DNS server resolves on-link. TCP, UDP, and ICMPv6 echo work over IPv6. On
  Apple DNS64/NAT64 networks, outbound TCP can also use a synthesized IPv6 address
  when a guest connects to an IPv4-only host. [Apple IPv6 guidance](https://developer.apple.com/library/archive/documentation/NetworkingInternetWeb/Conceptual/NetworkingOverview/UnderstandingandPreparingfortheIPv6Transition/UnderstandingandPreparingfortheIPv6Transition.html).
* 10.0.2.2 and fd00:2::2 are the host's loopback, so a guest can reach host services there.

Inbound connections use port forwarding. For example, with a service on guest port 8080:

```sh
rvemu ... --forward 2222:8080        # host: curl http://127.0.0.1:2222/
```

Limitations of the network: port forwarding is IPv4 only, there is no DHCP (the
addresses are configured by `/init`), and UDP flows expire after 60 s idle, ICMP
after 30 s. Outbound IPv6 needs a global IPv6 address on the host. Outbound ICMPv6
echo depends on the host's own support for unprivileged ICMPv6 sockets.

### rssh: ssh between the Mac, the emulator and the phone

`tools/rssh` is one POSIX shell script that runs inside the guest (it is installed as
`/usr/local/bin/rssh` on every boot) and on macOS. It does not implement a protocol of its own:
the encrypted connection is OpenSSH (an ed25519 key per machine, key-only login). `rssh` does the
parts around it: run the server, announce it, find devices, pair once, log in.

On macOS, run `tools/rssh` from this checkout or extract the release archive
and run `./rssh`. The script uses the OpenSSH and Bonjour tools supplied by macOS.

```sh
# in Alpine (the emulator on the Mac, or the iPhone app)
rssh start                  # installs openssh once, starts sshd, prints a fingerprint and a pair code
# on the Mac
rssh list                   # NAME  ADDRESS  STATE for every device it finds
rssh alpine-1a2b            # first time: asks for the pair code, then logs in; afterwards just logs in
rssh alpine-1a2b uname -a   # or run one command
```

| Command | Where | What it does |
|---|---|---|
| `rssh start [--pair]` | Alpine | install openssh if needed, write `/etc/rssh/sshd_config`, start sshd, start it again at every boot, announce the name. The first start (or `--pair`) opens a 5-minute pairing window with a random 8-character code |
| `rssh start` | macOS | announce this Mac as `_rssh._tcp` on the network so the guest can find it (Remote Login must be on; the script says how) |
| `rssh stop`, `rssh status` | both | stop and show |
| `rssh list` | both | the Mac looks at Bonjour (`_rssh._tcp`) and at loopback ports 2222-2229 (an emulator started with `--forward 0.0.0.0:2222:22`); the guest asks the phone app, which browses Bonjour for it |
| `rssh pair NAME` | both | one-time: shows the device's host-key fingerprint (compare it with the one `rssh start` printed), asks for the code, installs this machine's key, and closes the pairing window |
| `rssh NAME [cmd]`, `rssh USER@NAME` | both | log in. The last working address is tried first (0.2 s); if it fails the devices are searched again |
| `rssh set background on\|off\|status` | iPhone app | keep the app running with the screen off (below) |
| `rssh forget NAME`, `rssh key`, `rssh trust 'KEY'` | | drop a paired device after it was reset; print this machine's public key; add a key to a device by hand |

Alpine to the Mac works the same way in reverse (`rssh start` on the Mac, then `rssh casey@my-mac` in
Alpine); inside the iPhone app the Mac is found over Wi-Fi, inside the emulator on the Mac it is `rssh
USER@host` (10.0.2.2).

Security: the server listens only after `rssh start`; login is by key (password login exists only during
the pairing window, with a one-time code, and the root password is locked again afterwards or at the next boot);
host keys are remembered per device name, so a changed key stops the connection instead of being
accepted. Nothing is stored in the repository.

**How the phone is reachable.** `rssh start` in the iPhone app sends `expose on NAME` to the app's control
port (the phone's 127.0.0.1:2223, which the guest sees as 10.0.2.2:2223). The app then listens on port 2222
on every interface (`rvemu_ios_expose`, which forwards to the guest's port 22 through the same gateway code as
`--forward`) and announces `NAME._rssh._tcp` over Bonjour. `rssh stop` closes both. The app does this only when the
guest asks. On the Mac emulator there is no control port: start it with `--forward 0.0.0.0:2222:22` (or
`127.0.0.1` to keep it on the Mac) and `rssh list` finds it on loopback.

**Background mode.** iOS suspends an app soon after the screen locks, which also stops sshd. `rssh set background
on` asks the app to run the lowest-power location mode (3 km accuracy, no GPS), which iOS allows to keep an app
running; iOS shows its blue location indicator while it is on, asks once for location permission ("While Using"),
and the setting is remembered. It costs some battery, and iOS can still end the app under memory pressure; Wi-Fi
power saving can add latency to the first connection after the screen has been off for a long time. Reaching the phone
from outside your own network needs a VPN (Tailscale or similar); carriers do not accept inbound connections.

Cost: none on the emulation path. The helper is copied to the root disk only when its text changed (a `[ -e ]` check
on a hash marker, no process, so boot is unchanged at about 448M instructions); the extra work happens only when you
run `rssh`. Files: `tools/rssh`, `src/net/mod.rs` (`PortForward.bind`, `expose`/`unexpose`), `src/initramfs.rs`
(`RSSH`), `ios/RvemuApp/Rssh.swift`, `ios/rvemu-ios/src/lib.rs`.

### Storage

```sh
dd if=/dev/zero of=disk.img bs=1M count=64      # a raw disk
rvemu ... --disk disk.img                        # /dev/vdb with the root disk, writable
rvemu ... --disk data.img,ro                     # read-only
rvemu ... --root-disk my-root.img --root-size 2G # another root image, 2 GiB when created
```

The root disk is `/dev/vda`, and `--disk` images follow it. With `--ephemeral` there is
no root disk, so the first `--disk` is `/dev/vda`.

Disks are raw images, read and written in place. Writes go straight to the host
file. The root image is formatted by rvemu itself (ext2 layout, mounted as ext4
with no journal), so the host needs no filesystem tools. To mount a filesystem, the example overlay includes `rvemu-mount`, which
loads the filesystem drivers and mounts `/dev/vda` at `/mnt/disk`:

```sh
rvemu-mount && echo hello > /mnt/disk/hello.txt && umount /mnt/disk
```

It supports vfat and ext4. The drivers and their character-set modules are part
of the overlay; without the overlay, only raw access works.

### Overlays

`--overlay DIR` adds the contents of a host directory on top of the rootfs
(repeatable). A file replaces a rootfs file with the same path, and an overlay
`init` replaces the generated one. The example overlay holds the virtio modules
(`lib/rvemu/*.ko`), `etc/ssl/certs/ca-certificates.crt`, and `usr/local/bin/rvemu-mount`.

### Logs

The default boot is quiet: the kernel logs only errors (`quiet loglevel=2`), and
skips two initcalls for speed (see Performance), and the guest's output is plain text. When stdout is not a terminal, CRLF becomes LF
and the terminal-query escape the shell sends is dropped, so a log file reads
cleanly. `-v` restores the full kernel log and shows emulator status lines.
`--stats` prints the instruction count and speed when the machine stops.

## Machine model

The memory map follows QEMU's `virt` machine where a standard exists, so distribution
kernels bind to the devices without changes:

| Device | Address | Interrupt |
|---|---|---|
| RAM | `0x8000_0000` | `-m` |
| CLINT | `0x0200_0000` | timer and software (IPI) |
| PLIC | `0x0c00_0000` | 63 sources, two contexts per hart |
| NS16550A UART | `0x1000_0000` | PLIC 10 |
| SiFive test finisher | `0x0010_0000` | power off and reboot |
| Goldfish RTC | `0x0010_1000` | PLIC 11 |
| virtio-mmio slots 0..7 | `0x1000_1000 + 0x1000 * slot` | PLIC 1 + slot |

Devices are attached in order: disks, then the network card, then the entropy
source. The device tree is generated for each run, with the initramfs location,
the CPU ISA, the memory layout, and one node for each device.

The guest gets one hart by default; with `--smp N` (N > 1, or `auto` for one per fast core: 4 on an M5
MacBook Air) every hart runs on its own host thread (see "One host thread per hart" below); `--single-thread` multiplexes them on one thread in quanta instead, where
memory is sequentially consistent and remote TLB shootdowns apply between quanta. When every
hart is in `wfi`, the emulator sleeps until the next timer or input event, so an idle guest
uses about 0.1 % of a host core. In the single-thread mode guest time advances with the
instruction count while running and with real time while idle; with one thread per hart it
follows the host clock (the `--clock` and `--warp` options do not apply).

Firmware is optional: without `--bios`, SBI calls are served by the emulator
itself (base, timer, IPI, remote fence, HSM for multi-hart boot, system reset,
debug console, and the legacy console). With `--bios fw_dynamic.bin`, real OpenSBI
runs in M-mode.

## Command line

| Option | Meaning |
|---|---|
| `-k, --kernel FILE` | Kernel: gzip-compressed or raw RISC-V `Image`, or ELF |
| `--bios FILE` | M-mode firmware (OpenSBI `fw_dynamic`/`fw_jump`) |
| `-i, --initrd FILE` | Initial RAM disk (cpio, plain or gzip); excludes `--rootfs` |
| `--rootfs FILE` | Root filesystem tarball, packed into an initramfs in memory |
| `--init PATH` | Program to run as PID 1 from the rootfs (default: a built-in `/init`) |
| `--overlay DIR` | Directory added on top of the rootfs (repeatable) |
| `--root-disk FILE` | Root filesystem image, `/dev/vda` (default `~/.rvemu/root.img`; needs `--rootfs`) |
| `--root-size SIZE` | Size of a root image when it is created (default `8G`) |
| `--ephemeral` | Keep the root filesystem in memory: no root disk, nothing is saved |
| `--full-initramfs` | Force the full initramfs even when the persistent root is already populated |
| `--disk FILE[,ro]` | Raw disk image as a virtio-blk device (repeatable) |
| `--net` | virtio network card with user-mode networking |
| `--dns-server ADDR` | Forward guest DNS to this IPv4 or IPv6 resolver (needs `--net`) |
| `--no-ipv6` | Do not configure IPv6 in the guest (IPv6 is on with `--net`) |
| `--forward [BIND:]HOST:GUEST` | Forward a host port into the guest (needs `--net`); `BIND` defaults to 127.0.0.1, `0.0.0.0` listens on every interface |
| `--no-rng` | Do not attach the entropy source |
| `-a, --append ARGS` | Kernel command line (default: quiet, or the full log with `-v`) |
| `-m, --mem SIZE` | RAM, for example `256M` or `1G` (default `512M`) |
| `-s, --smp N` | Number of harts (default 1, the deterministic engine); each runs on its own host thread. `auto` is the number of fast cores of this machine (4 on an M5 MacBook Air) |
| `--single-thread` | With `--smp N`, N > 1: run all harts on one host thread (slower, deterministic) |
| `-v, --verbose` | Full kernel log and emulator status messages |
| `--program FILE` | Bare-metal ELF or binary to run in M-mode, without a kernel |
| `--warp` | When every hart is idle, jump to the next timer instead of sleeping |
| `--clock MODE` | Guest clock: `virtual` (default; one `time` tick per 16 retired instructions, deterministic) or `real` (follows the host clock, so guest benchmarks such as `7z b` report the speed the emulator really has) |
| `--max-insns N` | Stop after N instructions |
| `--stats` | Print instruction count and speed on exit |
| `--no-raw` | Keep the host terminal in cooked mode |
| `--dump-dtb FILE` | Write the device tree the machine boots with, then exit |

Ctrl-A x quits the emulator, as in QEMU. Ctrl-C and the other keys go to the guest.

## Architecture

| Module | Role |
|---|---|
| `insn.rs` | Decoder: 32-bit and compressed encodings to a 32-byte `DInsn` |
| `exec.rs` | Integer, M, A and system instructions |
| `fpu.rs` | F and D extensions, with IEEE 754 rounding modes and flags |
| `hart.rs` | Hart state, fetch/run loop, trap entry and return, interrupts |
| `csr.rs` | CSRs: privilege checks, WARL masks, `sstatus`/`sie`/`sip` views |
| `mmu.rs` | Sv39 page walks and A/D updates, generation-tagged walk TLB, flat data TLBs and fetch shortcuts |
| `fastfp.rs` | Native floating point for the block engine: F/D steps on the host FPU, flags kept in the host's `FPSR` |
| `block.rs`, `block/fusion.rs` | Decoded traces, threaded handlers, same-page links, checked cross-page and indirect links, return prediction, instruction fusion and memory fast paths |
| `bus.rs` | Physical address space, RAM, MMIO dispatch, code-write tracking, `GuestMem` for DMA |
| `devices/virtio.rs` | virtio-mmio transport and split virtqueues |
| `devices/blk.rs`, `rng.rs`, `net.rs` | virtio-blk, virtio-rng and virtio-net device models |
| `devices/rtc.rs` | Goldfish RTC |
| `devices/clint.rs`, `plic.rs`, `uart.rs` | CLINT, PLIC, 16550 UART |
| `net/` | User-mode gateway: smoltcp TCP, UDP, ICMP and ICMPv6 proxies, ARP, IPv6 neighbor discovery, DNS, port forwards |
| `ext2.rs` | Formats the persistent root image: an ext2 layout with a superblock and descriptor copy in every group |
| `sbi.rs` | Built-in SBI |
| `machine.rs` | Harts, boot layout, scheduling, device wiring, network service, idle |
| `fdt.rs` | Flattened device tree writer |
| `loader.rs` | gzip, RISC-V `Image`, ELF64 and raw image loading |
| `initramfs.rs` | Rootfs tarball, overlays and the generated `/init` as a newc cpio |
| `console.rs` | Raw terminal mode, the stdin reader, and plain-text output |

## Performance

Measured on an Apple M2, release build (`lto = "fat"`, `codegen-units = 1`,
`panic = "abort"`), no JIT. Times are wall-clock for one run.

| Workload | Instructions | Speed | Time |
|---|---|---|---|
| Compute loop (`tests/isa/bench.c`) | 718 M | about 1190 MIPS | 0.60 s |
| Alpine boot to shell and power-off, `--ephemeral`, quiet boot | 319 M | about 575 MIPS | 0.55 s |
| Alpine boot, persistent root, quiet boot | 491 M | about 590 MIPS | 0.83 s |
| Alpine boot, persistent root with `--minimal-initramfs` | 507 M | about 675 MIPS | 0.75 s |
| Alpine userspace mix (boot, `find`, `tar`, `grep`, `sha256sum`), `--ephemeral` | 813 M | about 1090 MIPS | 0.75 s |
| CPython mix (xorshift loop, `sha256`, `zlib`), persistent root | 2.39 G | about 1030 MIPS | 2.3 s |
| Bare-metal compute loop, 4 MiB through `sha256sum` in the guest | 802 M | about 830 MIPS | not re-measured |

The persistent boot carries about 79 M instructions (0.06 s) of network stack
init that the boot without a disk does not: the modules now load while the root
disk is mounted and the link is configured before the shell starts, so the first
command after the prompt can already resolve names. The userspace mix is a
measure of the *interactive* mix, so it includes that boot.

The superinstructions are what took the userspace mix past 1000 MIPS. Runs now
include loads and stores, which is where most of the remaining time was: a run
that cannot take a memory instruction's fast path hands that instruction to its
ordinary step and the rest keeps its planned fusion. Regenerate with

    cargo build --release --features counters
    ./target/release/rvemu ... --stats | grep SPECRUN > runs.txt
    python3 tools/spec_to_runs.py src/spec.rs > old_runs.txt      # keep the templates already there
    python3 tools/gen_spec.py old_runs.txt:runs.txt src/spec.rs 2400

### What that speed means in practice

Measured in the guest with the RTC (host wall clock), so these are real seconds
on an M2; the app on a phone is within roughly a third of these numbers.

| Guest task | Measured |
|---|---|
| SHA-256, BusyBox `sha256sum` | 64 MB in 4 s (16 MB/s) |
| SHA-256, OpenSSL from Python | 60 MiB in 3 s (20 MB/s) |
| `gzip -6` compression | 8 MB of text in ~1 s |
| `grep` over a file | 8 MB in ~1 s |
| `tar -cf /dev/null /usr` | under a second |
| Python startup (`python3 -c pass`) | under a second |
| Python integer loop (10 64-bit ops per iteration) | 4 M iterations in 44 s |
| `apk add figlet` (download, TLS, unpack) | 5 s |
| `apk add sqlite` | 3 s |
| `apk add build-base` (GCC, binutils, headers) | 35 s |
| `gcc -O2` a small C file | under a second |
| 200 k `sqrt()` calls in compiled C | under a second |

So shell work, BusyBox tooling, package management, editing, small C programs
and glue Python are all comfortable. Number crunching in Python is roughly
20-40x slower than native, and building a large project takes minutes, not
seconds. Anything needing a JIT (Node, Java, .NET) does not run.

Latest round (Apple M5, interleaved runs of frozen binaries, gcc compiling 150 small
functions, 5.6 G instructions; run-to-run noise is about 5 %):

| Workload | Before | After |
|---|---|---|
| gcc | about 470 MIPS | about 710-745 |
| sqlite, steady state (200 000 rows, indexes, a join) | about 855 | about 950 |
| sqlite, boot included (58 % of that run is the boot) | about 550 | about 595 |
| LZMA compress and decompress | about 1200 | about 1350 |
| Alpine boot | about 460 | about 460 |

The boot did not move: it is dominated by hash-like ALU code and the initramfs unpack,
not by calls, fences or cold decoded code.

Times are medians of interleaved runs on a shared machine, so expect about 5 % noise.
MIPS is instructions divided by the emulator's own run time, which excludes the host
setup (about 55 ms).

### Round of 21 October: one host thread per hart

Four rounds of tuning the single-threaded interpreter ended at +2 to +3 % each, and the notes of
the last one named the only large lever left that is not a JIT: run the harts of a `--smp N`
guest on N host threads. That is what this round did. **`--smp N` (N > 1) now gives every hart
its own host thread**; `--smp auto` is the number of fast cores (4 on an M5 MacBook Air), and
`--single-thread` keeps the old behaviour (all harts multiplexed on one thread). The default was
still one hart at that point, on the grounds that four harts cost the boot 0.46 s to 0.59 s; that
figure did not reproduce, and part two below measures it again.

Measured on the M5 MacBook Air (4 performance + 6 efficiency cores), real work in the guest:

| Guest workload | 1 hart | 4 harts | Gain |
|---|---|---|---|
| `7z b` to completion, guest's own total rating | 1917 / 1908 MIPS | 5030 / 4380 / 5312 / 4909 MIPS | 2.3 to 2.8 x |
| `7z b` to completion, emulator MIPS over the run | 1803 / 1795 | 4872 / 4254 / 5122 / 4795 | 2.4 to 2.9 x |
| four CPU-bound CPython processes (15.5 G instructions) | 1349 MIPS, 11.9 s | 4361 MIPS, 3.6 s (5.0 to 5.7 GIPS, 2.8 to 3.1 s with the polling below) | 3.2 to 4.2 x |
| four parallel gcc compiles + 120 forks + four CPython (soak) | | 2.0 to 3.5 GIPS | |

(The `7z b` columns are separate runs on a shared, fanless machine: the later a run comes in a
sequence of flat-out runs, the lower it is, down to 4.2 GIPS for the fifth in a row. `7z b` itself scales at 306 to 399 % CPU on four threads, so the gain is bounded by
the guest's own parallelism.) Eight or ten harts are slower than four (4.0 and 4.5 GIPS on
`7z b`): the efficiency cores run each thread at about half the speed and the guest's barriers wait
for them, hence `auto`. The one-hart interpreter is unchanged: boot 237.5 to 236.4, gcc 194.2 to
194.1, sqlite 244.3 to 245.4 guest instructions per kilocycle, base build against this one,
interleaved (noise is about 1.5 %). An idle 4-hart guest uses about 1 % of a core more than an idle
one-hart guest.

**What had to change for harts to run at the same time** (`src/mt.rs`, `src/bus.rs`):

* *Decoded-code coherence.* Each hart keeps its own decoded blocks, so a write by one hart has to
  make the others' blocks stale. The per-page code bitmaps, generations, epoch and mark log are
  atomics. A store writes and then looks at the bitmap (a full fence between); a hart that decodes
  a page marks it and then reads the bytes again to check nothing changed in between, and reads the
  page generation before the first byte. A hart that holds flat store-TLB entries for a page that
  another hart has since decoded makes that page's blocks stale when it next looks (the mark log
  records which hart decoded), so a store through an entry that was valid a moment earlier cannot
  leave a stale block behind. `fence.i` and the SBI `remote_fence_i` look at the log at once.
* *Memory ordering.* `amo*` are host atomics, `sc` is a compare-and-swap against the value `lr`
  read, the page walker sets accessed/dirty bits with one atomic OR, and `fence` decodes to a
  real barrier. All of this only happens while threads are on (`Bus::mt`); the one-hart path
  runs the same code as before.
* *Devices* are not thread-safe and are not made so: every MMIO access, SBI call and the machine
  thread's periodic service hold one lock. Interrupt inputs (software, external) are recomputed after
  every register access and handed to each hart as bits; a hart reads its own timer compare value
  without the lock, and guest time is the host's monotonic clock in this mode.
* *Requests between harts* (IPI, TLB flush, hart start, stop) are posted in the target's control
  block and the target is kicked: its `chain_limit` is zeroed so the chain it runs ends at the next
  block, and its thread is unparked if it sleeps in `wfi`. A remote fence waits for the targets'
  acknowledgement before the SBI call returns (the caller reuses the pages it unmapped).

**Waking a sleeping hart.** A hart that goes idle polls for 40 microseconds before it sleeps on a
host thread, when its previous idle spell was shorter than 300 microseconds (halt polling, as in
KVM). Waking a parked macOS thread costs about 6 microseconds, a third of a round trip of the
guest's own pipe write and read between two processes on different harts: that ping-pong took
30 to 35 microseconds a round trip on 2 or 4 harts before and takes 15 to 19 now (one hart: 18).
An idle guest sleeps as before (a long idle spell switches polling off); an idle four-hart guest
uses about 2 % of a core for the polling of the first idle moments.

**Tests.** Four unit tests (atomic adds from four threads, a compare-and-swap won by exactly one,
store-into-code invalidation, the mark log). `tests/e2e/threads.sh` boots 4 harts and runs
`threads/torture.c` (atomic add, CAS built on `lr`/`sc`, an `amoswap` spinlock, 32-bit atomics,
acquire/release message passing over 200 000 rounds, a store-buffering litmus with full fences:
all counts exact, the forbidden outcome never) and `threads/smc.c` (one thread rewrites a
function in an RWX page and runs `fence.i`, a thread on another hart must always see the newest
version: 4000 rewrites). A soak of 24 boots at 2, 3, 4 and 8 harts, each running four parallel gcc
compiles, 120 forks and four CPython processes, passed 24 of 24; `tests/e2e/smoke.sh` passes 13 of
13 with `--smp 4` (network, disks, persistence across a reboot); OpenSBI with four harts boots.
Profiling a four-hart compile run shows no time in flush handling, lock waits or parking: the
host threads spend it all executing guest code, so what limits the speed-up is the guest's own
parallelism and the cores' clocks when four of them run flat out.

**Not done.** The iPhone app still runs one hart (the A19 has two fast cores; it needs a hart count
in the FFI and the Swift side). Remote `sfence.vma` ignores the hart mask and the address range
and flushes every other hart completely (the compile profile shows no cost for it). The `fence`
variants all map to one full barrier.

### Round of 21 October, part three: floating point was a cliff, and it is gone

The request was the same again (overhaul the engine for MIPS with general techniques, search the
web). The single-thread integer engine had nothing left that cleared the noise (see part two), so
this round looked for places where the engine is not at its own speed at all instead of for another
2 %. It found one: **every floating-point instruction, `fld`/`fsd` included, ran through
`step_generic`**, the interpreter's non-tail call and then the software rounding code of `fpu.rs`
(a residual per operation, to find the inexact flag and to honour directed rounding). A guest
floating-point instruction cost about ten times what an integer one does. The census of the
standard benchmarks never showed it (their FP share is 0.004 % to 0.1 %), which is why five rounds
missed it; a compiled matrix multiply, `sqrtf`/division and `sin`/`cos` loop (`tests/bench/workloads/fp.sh`)
shows it at once.

| `fp.sh` (256x256 double matrix product, 800 000 single `sqrtf`+divide, 400 000 `sin`+`cos`+casts) | before | after |
|---|---|---|
| host cycles of the whole run (boot, compile, program), `--smp 1` | 8.6 to 8.8 G | 5.80 to 5.83 G |
| wall-clock MIPS of the whole run | 620 to 644 | 958 to 976 |
| the program alone (about 240 M guest instructions), guest insns per kilocycle | 61 | 217 |

That is 3.5x on floating-point code at the same output (`FP_DONE` prints identical digits). The
integer benchmarks do not move (best of 4 interleaved rounds, guest insns per kilocycle, noise
about 1.5 %): boot 240.0 to 242.2, CPython 386.7 to 386.8, sqlite 241.2 to 245.0, gcc 193.3 to 194.8.

How it works (`src/fastfp.rs`):

* The steps run the common case on the host's FPU with the same tail-call dispatch as the integer
  steps: `fld`/`flw`/`fsd`/`fsw` through the flat TLBs, `fadd`/`fsub`/`fmul`/`fdiv`/`fsqrt`, the four
  fused multiply-adds on the host's `fmadd` family (one rounding, as the architecture says),
  `fsgnj*`, `fmin`/`fmax`, `feq`/`flt`/`fle`, `fcvt` in all directions (to integer in each of the
  five rounding modes, with the host's saturating converts) and `fmv`.
* A step does the native thing only when it is certain to give the architectural answer:
  rounding is nearest-even (the instruction's `rm`, or `dyn` with `frm` = RNE), `mstatus.FS` is
  Dirty (Initial or Clean takes the old path once, which marks it), single-precision operands are
  properly NaN-boxed, and neither operands nor result are NaNs (a NaN needs the RISC-V rules:
  canonical NaN results, `fmin`/`fmax` semantics, which operands raise the invalid flag).
  Anything else goes to `step_generic` before anything is written, so the interpreter runs the
  whole instruction as before.
* **The exception flags are the host's own.** ARM's IOC/DZC/OFC/UFC/IXC in `FPSR` mean what
  NV/DZ/OF/UF/NX mean for these operations, so a step does not touch flags at all: no per-operation
  residual, no register read. `Hart::run` clears `FPSR` when a slice starts and folds it into
  `Hart::fflags` when it ends; `csrr`/`csrw` of `fflags`/`fcsr` read and clear it too. `FPSR` is
  per thread, so one thread per hart needs nothing more, and the multiplexed `--single-thread`
  mode is covered by the fold at the end of each slice.
* Two traps found on the way, both now handled: the compiler computes `a + b` *before* the test
  that decides whether the step takes the native path (the operation is pure for it), so a
  garbage single-precision operand that happened to be a signalling NaN raised a flag for an
  instruction that then ran in the interpreter. Every operation that can raise a flag is therefore
  an `asm!` block (never speculated). And the interpreter's own software rounding computes with the
  host FPU too, which fills `FPSR` with flags nobody asked for: `exec_fp` takes the guest's flags
  in before it runs and discards its own after.
* `mulh`, `mulhsu`, `mulhu`, `div`, `divu`, `rem`, `remu` and the word forms are leaf steps now
  too (they were interpreter calls; 0.1 % of the instructions of gcc, more in big-number code).

Checks: four differential tests (`src/fastfp_test.rs`) run about 260 000 random and edge-case
instructions (zeros, subnormals, infinities, every NaN kind, improperly boxed singles, the integer
edges of the conversions, all rounding-mode fields, `dyn` with each `frm`) through the interpreter
and through the step the block builder picks and compare every register and `fflags`; `spec_test.rs`
does the same for the M-extension steps; `tests/isa/run_diff.sh` (single and double FP with all
five rounding modes, compared with QEMU) went from 36 differing lines to 34, because the two
`fmadd` lines the interpreter got wrong (inexact flag when the product underflows) are now QEMU's.

What the differential test found in the old interpreter (still the path for the four directed
rounding modes, not changed): a single-precision `fmadd` is rounded twice (through a double), so
it is off by one unit in rare cases; `fsqrt.d` of an input below 2^-900 loses its inexact flag; a
fused multiply-add whose exact result is a tiny negative that rounds to zero returns +0 where
round-to-nearest gives -0. The native path is right in all three. One difference it has the other
way round: ARM detects tininess before rounding and RISC-V after it, so a result within a quarter
ulp below the smallest normal number can raise `UF` natively where RISC-V does not. Nothing in the
suite or in the QEMU transcript reaches it.

A scan for other cliffs (twelve small guest kernels, each measured by its own host cycles:
division, high multiply, atomics, `getpid`, page faults, `memcpy`, unaligned loads, pipe
ping-pong, `fork`, recursion, `qsort`, file I/O) found none: user code runs at 270 to 770 guest
instructions per kilocycle, kernel-heavy kernels at 130 to 165, which is what kernel code does
here. Unaligned 8-byte loads cost about 50 cycles each (the slow path); nothing common does them.

**Everything of this round against the build it started from** (interleaved, `--smp 1`, guest
instructions per kilocycle of host CPU, best of 4; wall-clock MIPS in brackets, which also
contain the disk-flush change and the load of the machine):

| Workload | Start of the round | Now |
|---|---|---|
| boot to prompt | 241.8 (1117) | 247.2 (1198), +2.2 % |
| sqlite (58 % of it is the boot) | 245.8 (968) | 250.7 (1031), +2.0 % |
| CPython mix | 398.2 (1596) | 399.7 (1588), +0.4 % |
| gcc | 192.1 (594) | 193.1 (655), +0.5 % |
| floating point (`fp.sh`) | 156.8 (467) | 241.5 (740), +54 % |

The integer steady state (gcc, CPython, `7z b`) did not move and was not expected to: the gains of
this round are floating point, the cold paths of a boot, and the disk flush.

**Cold-start costs, found by sampling a boot** (`sample PID -wait` on six boots; the interpreter
loop was 55 to 65 % of the emulator thread, the rest was not the loop):

* *Disk flushes.* 10 % of a boot's wall time was the emulator thread blocked in
  `fcntl(F_FULLFSYNC)`: Rust's `sync_data` on macOS, called for every `flush` the guest sends to
  the virtio disk (an ext4 mount and every journal commit send them). The flush now calls `fsync`,
  which hands the data to the drive without emptying the drive's own cache. Every guest write is
  already in the host's file cache before the guest is told it is done, so killing the emulator
  loses nothing; what `fsync` gives up against `F_FULLFSYNC` is protection from a host power
  failure. `RVEMU_FULL_SYNC=1` brings the full flush back. Boot to prompt: 0.416 to 0.443 s before,
  0.409 to 0.416 s after (1130 to 1160 MIPS to 1163 to 1180). A guest that commits to a database
  on the disk saves about 10 ms per commit.
* *Block building.* Decoding is a fifth of a boot's emulator time (82 000 blocks, 564 000
  instructions; 15 % of the wall time by the phase timers of the counters build), and half of
  that was one function: the lookup of the mined superinstruction templates, asked at every
  instruction. Its first-word filter let 52 % of the positions through (the register tuples of
  `addi sp, sp, -16` and the like are the starts of many templates), and every pass was a binary
  search that follows a pointer per step into the template table; only 4 % of the passes found
  a template. The filter is now on the first three instructions (every template has at least
  three) and is built from the table when the first block is built (`src/spec_index.rs`; 1 to 2 %
  of positions pass). The templates and what they match are unchanged. Boot: 246.0 to 251.0 guest
  instructions per kilocycle (best of 8 interleaved; medians +2.4 %). `tools/gen_spec.py` emits
  the new lookup, so regenerating the table keeps it.
* *Slow exits.* 18 % of the exits of a boot (7.3 M of 39 M) go through `follow_or_return`: 3.8 M
  taken branches and calls to another page, whose block cannot be linked into the exit (a
  cross-page target depends on the virtual alias) and are found through the block's two link
  slots every time, and 3.5 M indirect jumps. It is 10 % of the boot's emulator time at about 25
  cycles per call. Not changed: a cheaper probe saves a fraction of that.
* *Tried, no gain, removed:* making CSR writes that cannot raise an interrupt (`sstatus` with
  only `SIE`, `SUM` or `SPP` changing, and `sscratch`/`sepc`/`scause`/`stval`) skip the return to
  the run loop. They are two thirds of the CSR instructions (the kernel's interrupt enable and
  disable), and the change moved nothing: boot 242.4 against 242.1, gcc 193.4 against 192.7.

**Four harts are still not safe to make the default.** The stress run asked for in part two (three
4-hart guests at once and six busy loops, load average above 20) produced one failure in the first
90 boots: two `Segmentation fault`s in `/init` and `can't create /lib/modules//modules.dep`, with
the boot otherwise completing, so the guest executed something wrong. That is very likely what
stopped the sqlite run of part two. The one-hart engine is deterministic and does not have it.
The race is not found; `--smp auto` stays opt-in.

### Round of 21 October, part two: threads by default, and a search that found no single-thread lever

This round was again a request for a general engine overhaul (no profile training, web research
first). Two things came out of it.

**`--smp auto` was the default for a few hours, and is not any more.** One 4-hart run of the sqlite
benchmark stopped after 158 M of its 840 M instructions while the host load average was 12; 60
clean repeats did not reproduce it and the stress repeat was not run, so the cause is unknown and
the default is one hart again (`--smp auto` is still there and passed every test below). The earlier note that four harts cost the boot 0.46 s to 0.59 s did not
reproduce when measured again in interleaved runs: boot is 0.42 s with one hart (475 M
instructions) and 0.40 to 0.41 s with four (519 M; the secondary harts' own start-up runs in
parallel), a sequential one-process workload (the gcc benchmark, 5.6 G instructions) takes 6.5 to
6.9 s instead of 7.1 to 7.3 s with the same host cycles, and an idle four-hart guest costs about
0.5 % of a core. So the threads cost nothing to a guest that has nothing parallel to do, and they
are what `7z b`, `make -j` and several processes need:

| Guest workload | `--smp 1` | default (4 harts) |
|---|---|---|
| four parallel gcc compiles + four CPython loops (`tests/bench/workloads/par.sh`, 11.2 G instructions) | 13.9 s, 828 MIPS | 4.1 to 4.9 s, 2.3 to 2.7 GIPS (12 runs) |
| boot to prompt | 0.42 s | 0.40 to 0.41 s |
| gcc benchmark (one process chain) | 7.1 to 7.3 s | 6.5 to 6.9 s |

The engine benchmarks pin `--smp 1` (`tests/bench/run.sh` adds it unless you pass `--smp` or set
`SMP=N`), because one hart is what makes two builds comparable by cycles. `tools/pgo.sh`
trains on four harts as well, so that the multi-hart paths are not counted as never run.

**The single-thread engine is at the end of what a no-JIT interpreter of this shape can do, and
the evidence is now broader.** Measured this round (guest instructions per kilocycle, 4 to 5
interleaved rounds, noise about 1.5 %):

* *Translation is not a cost.* The counters build on the gcc benchmark: 897 K blocks built, 228 ms
  of 22.9 s (1 %) in `build_block`. A thread that decodes ahead could gain at most that.
* *Host page faults are not a cost.* Boot: 0.02 s system time of 0.57 s (3.5 %).
* *`opt-level = 2` instead of 3:* sqlite +0.5 %, boot +0.6 %, gcc within noise.
* *A newer compiler and a newer core target:* nightly rustc 1.99 (newer LLVM) and
  `-C target-cpu=apple-m4` against stable 1.97.1 with apple-m1: gcc 191.2 and 191.4 against 192.0,
  sqlite 244.1 and 242.0 against 244.5, boot 240.7 and 241.0 against 239.6, CPython 394.5 and 390.6
  against 393.6. Nothing.
* *The hottest handler is already minimal.* The 8-byte signed `ld` step is 19 host instructions
  (index loads, a register read, the micro-TLB tag compare, the data load, the register write,
  the pointer advance and the dispatch `br`); the TLB check is one dependent chain.

What the literature says agrees with that. The fastest interpreters measured recently are the
same shape as this one: Matt Keeter's Uxn tail-call interpreter in nightly Rust (`become`,
`extern "rust-preserve-none"`, state in arguments) beat his hand-written ARM64 assembly by 10 to 15 %
on an M1 and his `match`-loop VM by 2x; this engine already dispatches by tail call from every
handler, and its hot handlers are leaves, so `preserve_none` (which only removes register saves in
handlers that make a non-tail call) and `become` (which only guarantees what LLVM does here) would
change little and tie the crate to nightly. Rohou et al. show that the indirect predictors of
current cores handle threaded dispatch well, and Ertl's replies that replication still pays on
large workloads match the 4-way replication measured here (-4 %). Context and subroutine
threading (Berndl et al.: about 20 % over direct threading on 2005 hardware) work by emitting
call instructions, which is a code generator. Reverse-engineering notes on Apple's cores say a
taken branch is predicted every cycle while the branch target buffer hits; the cost per dispatch
here is the mispredicts of data-dependent guest branches and the instruction delivery after
them (top-down on gcc: 34 % delivery, 24 % discarded, 23 % back end, 20 % useful).

Not built, with the estimate that kept them out: guest `call`/`ret` as host `bl`/`ret` so the
core's return stack predicts guest returns (gcc runs 121 M `jalr`; at most 6 % if every one
mispredicted, realistically 1 to 2 %, and it needs host-stack unwinding on context switches and
`longjmp`); `DInsn` from 32 to 24 bytes (about 2 %, a rewrite of the link and step fields); both
touch the code every guest instruction goes through, which is now the default, threaded path.

**Verified with the new default:** `tests/e2e/smoke.sh` 13 of 13 and `tests/e2e/threads.sh` 5 of 5
without any `--smp`, 40 unit tests, 12 of 12 runs of the parallel soak, and an interactive run
through a pty (raw mode: prompt in 0.5 s, typing, Ctrl-C, four parallel guest loops, `exit`
returns 0), three times.

**Found while testing, not changed:** at the persistent session's prompt `poweroff`, `reboot` and
`halt` do nothing, with one hart or four. PID 1 is the session shell script, and BusyBox
signals PID 1 for them. `poweroff -f` and `exit` work. The line under "Known limitations" that
recommends `poweroff` is therefore wrong until the session handles those signals.

### Round of 20 October: loading the dispatch target earlier

Apple M5, frozen binaries run interleaved (`tests/bench`). The CPU's top-down breakdown for
gcc is 34 % instruction delivery, 24 % discarded work (mispredictions), 23 % waiting for
operands and 20 % useful, and the run makes an estimated 2.9 G dispatches for 5.6 G guest
instructions, so the discarded share suggests that about one dispatch in eight mispredicts. A
mispredicted indirect branch is found out when its target has been loaded, so the length of the
load chain in front of every dispatch is part of what each misprediction costs. This round
shortened two of those chains and removed one store-to-load forward. Nothing else it tried
moved the cycle count.

**What was kept** (all general; no template or workload knowledge):

* **The return stack remembers the continuation block.** A call pushes the continuation pc, the
  stamp and the call's terminator; the terminator's `link` holds the continuation block, so a
  return loaded the entry, then the terminator, then its link, then the block's first step. The
  entry now holds a copy of that link taken when the call is made (`RasEntry::next`), which
  removes two dependent loads from every return. gcc +1.5 to +2 %, sqlite +1.5 %.
* **Handlers load the next step first.** Every handler ends in an indirect branch to
  `(*d.add(k)).step`, and the compiler schedules that load a few instructions before the
  branch. `pre1` to `pre5` (src/block.rs) issue it with an `asm!` `ldr` as the first thing the
  handler does (single ALU, load, store, in-block branch, `lui`, `auipc`, fused pairs, triples,
  quads and op+branch units). The handlers have the same number of instructions as before; the
  load just starts a few cycles earlier, so a mispredicted dispatch is found out sooner.
* **A fused op+branch unit passes the op's result to the branch.** `step_xbr` ran the op, stored
  its result in the register file and read it back for the branch condition, a store-to-load
  forward in front of the branch that decides whether the next dispatch was predicted
  correctly. `build_block` now sees whether the branch's operands are the op's destination and
  picks one of four variants (`XF`), so the branch takes the value straight from the op.
  An earlier version of this idea in fused pairs compared register numbers at run time and was
  slower; chosen at build time it costs no instruction: 29 of 32 interleaved rounds were faster
  than the build without it (median +0.7 %, CPython +1.5 %, sqlite +1.0 %). The 258 op+branch
  handlers became 1032.
* In a `--features counters` build the hoisted dispatches go through `go`, which does the
  counting, so template mining still sees every unit.

Measured against the build at the start of the round, in a quiet spell of the machine: guest
instructions per kilocycle, median of the per-round ratios over 6 to 8 interleaved rounds (the
same comparison in a busy spell gave +1 to +5 % for the first two changes, and identical
binaries differ by 1.5 % in best-of-three, so read each figure as plus or minus 1 %):

| Workload | Change | MIPS now |
|---|---|---|
| Alpine boot (469 M instructions) | +3.0 % | about 1160 |
| LZMA compress and decompress of a 1.9 MB binary (`7z a`, `7z x`) | +1.6 % | about 1890 |
| sqlite, 30 000 rows, boot included | +2.9 % | about 1130 |
| CPython (xorshift, sha256, zlib) | +2.4 % | about 1610 |
| gcc, 150 small functions | +4.7 % | about 650 |

About +3 % on average (a second comparison against the same baseline gave boot +3.2, sqlite
+2.9, CPython +2.7, gcc +5.8) and positive on every workload, from changes that are not tied
to any program. LZMA gains least: its time is in the generated runs and the data-dependent
range-coder branches, which these changes do not touch. The ISA transcript still differs from QEMU in the same 36 known lines, 34 unit tests and
the 13 smoke checks pass, and a 4-hart boot works.

**Profile-guided build.** `tools/pgo.sh` (instrumented build, training on the workloads in
`tests/bench`, optimized build) gives another +1.7 to +2.2 % on top of this round's build in the
same quiet spell (boot +2.0, LZMA +2.1, sqlite +1.7, CPython +1.7, gcc +2.2), so +3.4 to +8.6 %
over the start of the round, with wall-clock MIPS of about 1180 (boot), 1930 (LZMA), 1150
(sqlite), 1790 (CPython) and 750 (gcc). It is still not the default build, because a profile
only loads in the compiler that wrote it; run the script to get `target/pgo/rvemu` (it is there
now). The script
now passes its flags in `CARGO_ENCODED_RUSTFLAGS`, so a checkout in a path with spaces works.

**Measured and dropped** (cycle metric; `+` is faster):

| Idea | Result |
|---|---|
| Prefetch the successor block's later lines when a link is followed | 0 on gcc, lzma, sqlite |
| 32-byte function alignment instead of 64 | about -1 % on gcc and lzma |
| A taken forward branch to an instruction of its own block jumps there directly instead of leaving the block | gcc and sqlite -1.8 %, host instructions per guest instruction 13.81 to 14.16: a jump target ends fusion, and the units lost cost more than the cheaper exit saves |
| 32-byte block header, link slots allocated on first use (blocks are two instructions long on average when built, so the 64-byte header was most of their memory) | -2 to -5 %, 0.2 to 0.4 host instructions per guest instruction more |
| The continuation's first step copied into the return stack entry as well | -2 to 0 % |
| The same load-first treatment for the 1514 generated runs that end in the next unit | 0 to -2 % |
| The last result of a generic fused pair, triple or quad passed on to the next unit as `fa`, the same idea as op+branch | not measured: the first version hung the guest and the second ended the boot after 65 M instructions (a bug in how generated runs and generic groups share a block, not found in the time left); reverted. The idea is sound and worth a careful second try |
| Branch, `jal` and terminator handlers load their exit link first | +0.4 to +1.7 % in four comparisons, -0.2 to +1.5 % in the last (python -5 %): no net effect, costs 0.05 host instructions per guest instruction |

Also measured, not changed: block decoding is only 1.33 times redundant (1.8 M instructions
decoded for 1.3 M distinct ones in a gcc run, rebuilds included), a block averages 2 instructions
when it is built, and 16 % of gcc's block exits are forward branches that land in their own block.

**Three measurement mistakes this round, all worth knowing.** A build that fails leaves the
previous `target/release/rvemu` in place, and copying it under a new name made several
"comparisons" between identical binaries: they showed wins of 0.7 to 1.4 % and, for one idea,
a verdict that was not one. `md5 -r` on the frozen binaries exposes it. The second was a loop in
zsh (the agent's shell) that passed `"a b"` as one argument, so four variants were built with
none of their patches. The third: the default root image (`~/.rvemu/root.img`) has no 7zip, so the
`lzma` workload ran only the boot for most of the round (468 M instructions, `7z: not found`
discarded with the rest of the guest output); the real LZMA run was measured on a throwaway image
(`apk add 7zip`). `tests/bench/ab.sh` now warns when a guest command is not found, and
`tests/bench/README.md` has the rules that follow from all three and from the noise of this
machine. A timed `7z b` is not a usable comparison either: the guest's `timeout` counts virtual
time, so it ends after a fixed 4.8 G instructions, most of them the CPU-frequency loop.

**Where this leaves the engine.** The single-threaded interpreter is at the limit of its design
without a JIT: gcc spends 34 % of its issue slots waiting for instructions and 24 % on discarded
work, and the remaining ideas are each worth 1 to 2 %. The one large lever that is not a JIT,
running the harts of a `--smp N` guest on N host threads, was built in the round of 21 October
(above); a one-hart guest does not gain from it.

### Round of 19 October: a measurement correction and a small gain

Apple M5, frozen binaries run interleaved, scripts in `tests/bench` (see its README).

**A correction.** The harness used until now typed the first guest command one second after
the emulator started. A boot that takes less than a second sat idle until then, so the
"Alpine boot about 460 MIPS" row above, and the boot share of the sqlite row, were
instructions divided by one second. The boot (469 M instructions, persistent root, quiet) takes
0.44 s: about 1060 MIPS before this round, about 1110 after. `tests/bench/run.sh` queues the
commands before the guest starts, and `tests/bench/ab.sh` compares builds by guest instructions
per kilocycle of host CPU, which does not depend on the clock, the core type or the load
(the laptop is fanless and shared: the wall-clock MIPS of one binary swing by a factor of two).
The older boot and mix figures in this section were taken with that harness or a similar
one; read them as lower bounds.

**The gain.** Guest instructions per kilocycle, best of three interleaved runs, before the
round and now, with the MIPS the final build showed on the (busy) machine:

| Workload | Before | After | Change | MIPS now |
|---|---|---|---|---|
| Alpine boot (469 M instructions) | 229.0 | 235.4 | +2.8 % | about 1110 |
| sqlite, 30 000 rows, boot included | 235.7 | 239.5 | +1.6 % | about 1090 |
| gzip and bzip2 of a 1.9 MB binary | 316.9 | 322.7 | +1.8 % | about 1420 |
| CPython (xorshift, sha256, zlib) | 386.9 | 392.3 | +1.4 % | about 1725 |
| gcc, 150 small functions | 184.6 | 185.0 | +0.2 % (mean of three: +1.5 %) | about 800 |
| LZMA compress and decompress | 410.3 | 421.0 | +2.6 % | about 1855 |
| `7z b`, first 24 G instructions (mean of five) | 474 | 479 | +1.0 % (wall MIPS +2.4 to +3.5 %) | about 2100 |

That is about 2 % in all, from general changes only. What was kept:

* **Store translations.** A page whose decoded code was overwritten stayed marked as code
  forever, so every later store to it took the slow path: 706 000 of the 956 000 slow stores
  of a boot, the init and exec pages the kernel reuses as data. The mark now goes away with
  the code. A page decoded as code for the first time drops only its own entries from the
  flat store tables (a log of the last 256 such pages and a bitmap of the pages that have
  entries) instead of emptying them. A misaligned load or store inside a mapped page finishes
  in place instead of translating again.
* **Atomics.** `amo*`, `lr` and `sc` have their own steps that work on host memory through the
  flat tables and hand anything unusual to the interpreter (boot: 0.8 M of them).
* **Block building** (8 % of a boot, 0.5 % of gcc): the decode buffer lives in the hart, every
  fusion decision is made once and its step kept, and the template matcher's first-word filter
  has 65 536 bits instead of 4 096 (about a fifth of the first words it saw passed and went on to a binary search over 2 400 templates).
* A block-ending branch that is not taken leaves through the terminator's exit without
  dispatching to it; fused units write their registers without the `x0` test (an instruction
  that writes `x0` is no longer fused); the machine is not torn down at exit (10 ms of a
  boot); the emulator thread asks for the highest quality of service (no measurable
  difference; kept because the scheduler was seen moving the thread between efficiency and
  fast cores under load).

**Why there is not more.** The CPU's own counters (Instruments, CPU Counters) say where the
issue slots go. On gcc 19 % do useful work, 18 % wait for operands, 35 % wait for
instructions and 29 % are mispredicted work thrown away; on LZMA 40 % are useful, 24 % wait for
operands, 22 % are discarded and 14 % wait for instructions. The sampled time follows: in
LZMA 35 % is the conditional-branch handlers (the range decoder's branches are data dependent
and would mispredict natively too) and 39 % the generated runs; in gcc 28 % is generic
triples, 18 % pairs, 17 % branches, 12 % op+branch units, 11 % generated runs. Removing host
instructions that are never in the way (the `x0` test: 3 % fewer host instructions) did not
change the cycle count; only removing dispatches or slow paths does, and fusion is saturated
(the 15 opcodes of the generic triples are 65-74 % of the executed instructions of these workloads; widening the set to 18 gave nothing).

Tried this round and dropped, with the measured change in guest instructions per kilocycle:
a software branch predictor (each guest branch switches its own handler between a
usually-taken and a usually-not-taken version): gcc -3.5 %, boot -1 %, LZMA -11 %, because the
hardware's history does better and the changing target makes the dispatch into the branch
harder to predict; forwarding the results of fused-unit members in host registers instead of
re-reading the register file: -0.4 % to -1.3 %; a hot-first function order file (to test
instruction-cache and iTLB layout): 0 %; a lean CSR step and ending the chain only for CSRs
that can change interrupt delivery: -0.3 % to +0.5 %; a 64-entry return-address stack: 0 %;
generic triples over 18 opcodes instead of 15: 0 %. Measured and not built: a taken branch
whose target is another instruction of the same block is 23 % of gcc's exits (an estimated 3 %
if done), and short forward branches over one to three ALU instructions are only 1.4 % of gcc's
branches, so if-conversion would not pay. Profile-guided optimization gives +1 to 2 % on every
workload (`tools/pgo.sh`, target/pgo/rvemu); it is not the default because a profile only
loads in the compiler version that wrote it.

Boot time, start of the last round to now: ephemeral boot 1.03 s → 0.74 s (−28 %),
persistent boot 1.43 s → 1.08 s (−24 %). Most of that is less guest work: the boot
executes 29 % fewer instructions (451 M → 319 M). The emulator's MIPS on a boot stays about
the same, because the removed work ran at a similar rate. The Alpine userspace mix gained
13 % in MIPS (495 → 560), and the compute loop 3 % (about 935 → 960).

The speed comes from:

* a basic-block cache. A straight run of instructions is decoded once into a block of
  up to 64 instructions. A block ends at the first unconditional jump, system or CSR
  instruction, or at a 4 KiB page boundary. Blocks are keyed by physical address, so
  one decode serves every virtual alias of a page.
* superblocks. A conditional branch does not end a block: the fall-through path keeps
  decoding inside it, and only the taken edge leaves. A branch in the middle of a block
  runs its fall-through inline, so the common not-taken case costs no dispatch, no link
  check and no page lookup; the taken case exits exactly like a block-ending branch.
  Fusion never straddles such an edge, and in-block branches have their own step
  variants.
* block chaining. Each block remembers its two most recent successors, with the
  successor's virtual pc and an identity stamp for the translation and code state.
  When a block ends and its link to the next pc holds, the block runs that successor
  directly, as a tail call, without returning to the run loop. The stamp is a single
  compare: links no longer touch the per-page generation vector, and any invalidation
  anywhere bumps one global code epoch. The chain stops and returns when an
  interrupt is pending, the hart waits or stops, the source page was rewritten, or the
  quantum's instruction budget is used up. About 80 % of block transitions chain this
  way (36.5 M of 44.5 M during a boot).
* threaded steps. Each common instruction (integer ALU, loads and stores, branches,
  jumps) has its own small function that runs the next one with a tail call, so
  every instruction has its own indirect branch, and the predictor learns each one
  separately. Rare instructions go through the interpreter.
* operand forwarding. Each step also receives the results of the two instructions
  before it. When an operand was written by one of those (and that instruction is a
  single one, not part of a fused pair), `build_block` picks at build time a variant of
  the step that reads the operand from the argument instead of the register file, so
  no run-time test is needed. Every result is still written to the register file, so
  any reader that is not forwarded sees the right value.
* fused pairs and triples. 69 common adjacent pairs and 30 triples, mined by counting
  dynamic opcode bigrams and trigrams during an Alpine workload (for example `ld`+`ld`,
  `sd`+`sd`, `slliw`+`addw`+`xor`, `xor`+`srliw`+`slliw`), run in one dispatch. Each half
  runs only its fast path, and if a half cannot finish without a trap or MMIO, that half
  runs its ordinary step instead, so behavior is unchanged. Triples are tried before
  pairs, and both are barred from straddling a branch's fall-through edge.
* memory fast paths. Loads and stores that need no page walk, trap or MMIO are
  handled inside the step: the translation comes from a micro-TLB that can hold a
  superpage, or from identity mode, and the RAM access is done directly, with the bounds
  already checked once (the second, redundant checks in the RAM slice and the
  code-bitmap lookup are skipped with `get_unchecked`).
  The data micro-TLBs have 32 direct-mapped entries each for loads and for stores.
  On a boot, slow-path accesses fell from 23 M to 3.6 M and the hit rate rose from 76 %
  to 96 %. Only the rest take a separate function, so the fast step has no stack frame.
  Set-associative micro-TLBs were tried and are slower on a boot: the extra tag
  compares cost more than the misses they save.
* boot work. The default quiet command line skips two kernel initcalls: the
  jitter-entropy self-test (`jent_mod_init`), which takes about a second of emulated
  time and is the largest single cost of a boot, and the f2fs filesystem registration
  (`init_f2fs_fs`), which this system never uses. The kernel is still seeded from
  virtio-rng (`crng init` happens early in the boot), and `/dev/random` and
  `/dev/urandom` work. The jitter-entropy crypto RNG is not registered. The verbose boot
  (`-v`) keeps every initcall. Pass `-a` to change the command line.
  The host also reads and decompresses the kernel on a second thread while it packs the
  rootfs, which cut the host setup from about 73 ms to 56 ms.
* a self loop (a branch back to the block's own entry) chains without touching
  the link slots at all: the block is running, so nothing needs validating.
* cycle and instruction counters are banked once per chain instead of at every
  block exit; a guest read adds the chain-local count so what software sees
  stays exact and monotonic.
* the interrupt, `wfi` and halt flags are folded into the chain budget: the
  code that raises one zeroes the budget, so a block exit tests one load
  instead of four state fields. A store to a page carrying decoded code takes
  the general path, and a write that really touches decoded code still bumps
  the page generation.
* four-instruction fusion ("quads") for the hottest mined 4-grams, on top of
  the pairs and triples, and `jal`/`jalr` steps specialised at build time for a
  zero or non-zero destination (no register-number test per call).
* a generated run may include the last instruction of its block, which is
  usually a call, a return or a loop branch: the run then ends the block and the
  terminator's dispatch disappears with it. This is what CPython runs on most.
* the data micro-TLB has 256 sets and the translation TLB 1024 entries: a
  Python heap touches far more pages than the 32-set table the shell mix was
  tuned on.
* a micro-TLB entry for a plain 4 KiB mapping also carries the host address of
  that page, so a load inside it is tag check, one add and a read: no bounds
  arithmetic, no page-table walk, no bus dispatch.
* templates are re-mined against a build that already uses them, so each round
  adds runs for the work still outside them: half the mix and a third of the
  CPython mix now run inside generated code.
* a register copy is a plain store, and later instructions in the run read the
  source value directly.
* the successor block's instructions are prefetched when a chain is taken.
* a generated run forwards a value it stored to a later load of the same
  address, and a value it loaded to a later load of it: the store or load that
  proved the page is mapped, so the second access cannot fault.
* a return-address stack. A call (`jal` or `jalr` that links in `ra`/`t0`) pushes the
  address it will return to; the call's terminator, which a call never runs, keeps the
  block that starts there. A return that agrees with the top of the stack under the
  current stamp chains straight into that block, so a function called from many places
  no longer thrashes the two link slots of its `ret`. It is only a prediction: every
  return is checked against the real target, so a longjmp or a context switch just
  misses. gcc +9 %.
* `sfence.vma` with an address no longer throws everything away. It drops the hardware
  TLB entry, the one slot of the flat data tables the page can occupy, and the fetch
  shortcut; the translation epoch (which kills every block link) only moves when a
  pc-keyed entry was made for that page (a bitmap, `Hart::link_pages`) or a superpage
  covered it. The kernel fences data pages all the time (copy on write, `mprotect`,
  `munmap`): gcc does about 95 000 of them and 99.5 % are on data pages. gcc +7 %.
* decoded blocks are one allocation each (a 64-byte header followed by the instructions,
  so a link names a block's first instruction with no load) taken from a bump arena in
  the order the blocks are built. Code that ran together sits together in host memory,
  and the decoded working set of gcc (hundreds of MB) takes far fewer host pages and
  cache lines. gcc +13 % for the single allocation and +7 % for the arena; making the
  decoded instruction twice as big cost 8 %, which is how it was found.
* fusion by shape. Any instruction with a fast path (ALU, `lui`, `auipc`, load, store)
  followed by a conditional branch is one unit; any two adjacent fast-path instructions
  are one unit (the mined pairs are only consulted first); and any three out of the
  fifteen most frequent are one unit. These are rules about the code, not lists of
  sequences seen running, so they cover programs nobody mined. gcc +5 %, +8 % and +4 %.
* exits are cheaper: the target of a `jal` or branch is folded into the instruction when
  the block is built, the hart's pc is only written on the slow path, and units pass
  their forwarding registers on instead of clearing them.
* every function starts on a 64-byte cache line (`.cargo/config.toml`), which is worth
  about 5 % on gcc and 3 % on sqlite for code made of thousands of tiny handlers.
* generated superinstructions: `src/spec.rs` holds 2400 functions for
  the exact instruction runs the guest spends its time in, with every register
  and immediate fixed at compile time, values that a later instruction in the
  run reads forwarded through locals, and one dispatch for the whole run. They
  are mined from a profiling run (`tools/gen_spec.py`), verified against the
  ordinary steps by a differential test, and consulted when a block is built;
  anything they do not match uses the pairs, triples and quads as before.
* traps keep block links.
* the initramfs loads the network modules in the background and joins them
  before the link is configured, so the shell only starts once `eth0` is up:
  the first `apk update` after the prompt no longer reports a name-resolution
  failure, at the cost of about 60 ms of boot. A trap or `xret` changes the privilege and the status
  bits but not the mappings, so it only drops the micro-TLBs (which hold no
  permission information) and leaves the links and the hardware TLB alone. Only
  a `satp`/ASID change or an `sfence.vma` invalidates links. Traps are constant
  in user-space work — this alone was worth about 7 % of the userspace mix.
* superpage translations are direct-mapped. The hardware TLB keeps 4 KiB
  mappings in a 512-entry table and 2 MiB / 1 GiB mappings in small
  direct-mapped tables indexed by the page they cover; kernel space is mapped
  in superpages, so a linear scan there cost a few percent of every command.
* a block exit counts retired instructions from a byte in the instruction
  itself (filled in when the block is built) instead of dividing a pointer
  difference by the instruction size at every exit.
* the network stack is loaded by the login session in the background, after the
  prompt, so an interactive shell does not wait for `virtio_net` and its
  dependencies (about 36 M instructions, 8 % of a boot). The disk's own modules
  — the virtio transport, `virtio_blk` and `ext4` — still load first, since the
  root filesystem cannot be mounted without them.
* one `modprobe -a` process inserts all kernel modules instead of forking BusyBox per
  module (about 1 % of a boot), and the app caches the packed initramfs so only the
  first boot after an asset change pays for gunzip, tar and cpio.
* an initramfs without the root filesystem (`--minimal-initramfs`, used by the app once
  the persistent root disk is populated): it carries only BusyBox, the C library, the
  generated `/init` and the module overlay, and the guest mounts the disk and
  `switch_root`s into it. The kernel then skips unpacking a whole rootfs: 12 % fewer
  boot instructions and 20 % less wall time. The app tracks whether the disk is
  populated with a marker file and falls back to the full initramfs (and a reboot) if
  a minimal boot produces no output.
* the pc is written only by control transfers and once per block, not per instruction,
  and the cached translations are dropped only when a bit that affects translation
  changes (`SUM`, `MXR`, `MPRV`, `satp`, the privilege), not on every interrupt toggle.
* code-write tracking. Decoding marks the halfwords it read in a per-page bitmap. A
  store that touches a marked halfword bumps the page's generation and the global code
  epoch, which makes that page's blocks and every block link stale. Interrupts are
  checked at block boundaries, so one can be delayed by up to 64 instructions.

* direct exit links. A taken branch, a `jal` or the end of a block whose target is in the same
  4 KiB page stores its successor block in the exit instruction itself (`DInsn::link`). The
  successor of a pc-relative target in the same page is fixed by the page alone, so following
  it needs no pc compare and no epoch stamp: about 22 host instructions per exit instead of
  about 38. Code writes end the running chain (`end_chain_on_code_write`), so a link is never
  followed after its page changed; a conditional branch exits only on the taken edge and its
  fall-through is the block terminator's own link.
* a per-hart block cache (1024 entries, keyed by pc and link stamp) behind the two links of each
  block, and re-validation after a translation flush: a link whose code epoch still matches only
  has to translate its pc once and compare the physical address, instead of going back to the
  run loop for a full lookup. Returns, indirect calls and the first jumps after every
  `sfence.vma` stay inside the chain. The slow part of an exit is a frameless tail call.
* no pc in the chain of steps. A step gets `(hart, bus, insn, fa, fb)`; each instruction knows its
  offset in the block, and the few steps that need a pc (control transfers, `auipc`, traps)
  compute `block_pc + off`. `DInsn.imm` is 32 bits to make room, so an instruction is 32 bytes.
* idioms folded when the block is built: `lui`+`addi`/`addiw` and `auipc`+`addi` become one
  unit that stores the final constant, and `slli 32`+`srli 32` becomes a zero-extension.
* a 2048-entry flat data TLB per context (was 512): a 16 MiB LZMA dictionary no longer
  thrashes it (+6 % on LZMA, nothing measurable elsewhere).
* the template table is plain integer literals, one `static` per template. A single table of
  `Op::X as u64 | n << 8` expressions had made rustc spend minutes in one constant: the whole
  release build takes about 20 s instead of more than 5 minutes.

Things measured and rejected: following direct jumps inside a block (under 1 %), four copies of
the in-block branch step so that the host predictors see fewer guest branches per copy (0 %),
keeping the hardware TLB across `satp` writes that change the ASID (0 %), fusing a fast instruction with the conditional
branch its fall-through reaches (the extra step variants cost as much as the
dispatch they save), sixteen further mined pairs (same), and dropping
two-instructions-back operand forwarding to halve the ALU step variants (still
a net loss; the forwarding pays for its code). Target-specific code generation (`target-cpu=native`),
profile-guided optimization (about 3 %), outlining the block loop (about 10 % slower),
forwarding a fused pair's first result to its second half (about 2 % slower, the extra
operand checks cost more than the stall), fusing a conditional branch with the
instruction its fall-through reaches (about 3 % slower; the extra step variants cost
more than the dispatch they save), and a larger hart-level TLB (no gain; the data
micro-TLB is different, see above). Kernel boot command-line tweaks
(`mitigations=off`, `random.trust_cpu=on`, `nokaslr`, extra `initcall_blacklist`
entries) made no measurable difference. The scheduling quantum is 65536 instructions;
smaller values lose 5-10 % to per-quantum overhead, larger ones delay interrupts.
Boot-time experiments that did not help: 8-byte alignment of the cpio file data for the
kernel's copy (no change in unpack time), `cryptomgr.notests` (the long silent stretch
of the boot is not the crypto self-tests), `--warp` (the boot never idles, so there is
no wait to skip), and smaller guest RAM (128 MiB saves about 2 % of the boot).

What still limits the interpreter: operands from earlier blocks (about half of all
register reads) and operands from a fused pair still go through the register file.
The front end is the main stall: a profile shows about 40 % of cycles lost to
instruction delivery, mostly the indirect branches of the threaded steps. The next
gains need fewer dispatches per guest instruction, such as longer traces across
linked blocks, or the register cache carried across blocks.

What still limits the boot: unpacking the initramfs is now the largest guest cost. The
kernel copies the rootfs (515 entries, 7 MB) and the overlay into tmpfs, about 0.44 s of
emulated time. A persistent root that is already populated does not need that copy, so
leaving the rootfs out of the initramfs in that case would save about 14 % of a persistent
boot. It needs a reliable marker that the disk was populated, and a fallback for when it
is not, so it is not done yet.

## Testing

* `cargo test --release`: 46 unit tests (one, `net::tests::exposed_port_reaches_the_guest_until_closed`,
  has timing assumptions and failed on the first run of a freshly linked test binary, probably while
  macOS scanned it; it passed on every rerun, with this round's sources and the earlier ones) covering the decoders, TLB and permission
  rules, rounding, CSRs, the FDT, the cpio writer, the init script (which must pass
  `sh -n`), the protocol helpers (checksums, Ethernet, ARP, IPv4, IPv6, UDP, ICMPv6,
  neighbor discovery), the ICMP sockets, the ext2 formatter, virtqueues, the RTC, and the atomics
  and decoded-code invalidation that harts on separate threads rely on.
* `tests/e2e/smoke.sh`: boots Alpine with a persistent root disk, a data disk, the
  network with IPv6, and entropy, runs commands in the guest, and checks the output:
  the root filesystem, IPv6 address, ping to the gateway, IPv6 DNS, disk reads, the
  entropy device, the RTC, HTTP and HTTPS fetches, five sequential downloads, a ping
  to an internet address, a file surviving a reboot, and a clean power-off. Run
  `examples/boot-alpine.sh` once first. It uses a temporary root image, so your
  `~/.rvemu` is not touched.
* `tests/e2e/threads.sh`: boots 4 harts on 4 host threads and runs concurrency tests in the guest
  (atomics, `lr`/`sc`, a spinlock, message passing, a store-buffering litmus, cross-modifying code,
  parallel processes). It installs gcc in a throwaway image over the network.
* `tests/isa/run_diff.sh`: builds a freestanding program covering integer and word
  operations, division corner cases, M-mode traps, atomics, and single and double FP
  with all five rounding modes and `fflags`. The output is compared byte for byte
  with QEMU's `virt` machine. It differs in 34 lines, all of the kinds listed below.
* `tests/bench`: the benchmark scripts and workloads (`run.sh`, `ab.sh`); see its README for how
  to compare builds without being fooled by idle time or by the load on the machine.
* Boots checked by hand: Alpine with the built-in SBI and with OpenSBI, on one and
  four harts; networking under OpenSBI; the interactive terminal (prompt, echo,
  Ctrl-C, Ctrl-A x).

## Known limitations

* **Networking is user-mode only.** No inbound UDP, no IPv6 port forwarding, and no
  DHCP server (the addresses are set by `/init`). Connections are bounded by 64 KiB
  socket buffers. Outbound IPv6 needs the host to have a global IPv6 address.
* **The root image has no repair tools.** rvemu formats it with the ext2 layout and
  the guest mounts it as ext4. There is no `fsck` or resize in the guest yet, so
  power off with `exit` at the shell prompt (it syncs first; plain `poweroff` is not handled, see the 21 October notes). A crash in the middle of a write
  can leave the image needing a manual check.
* **The FAT and ext4 drivers are loaded on demand**, which costs emulated time at
  boot: the ext4 modules add about 0.4 s to a persistent boot. Requests complete
  synchronously against the host file, and the guest's flush (`sync`) makes the host
  write them to disk.
* **Entropy and time come from the host** (`/dev/urandom` and the wall clock).
* **PMP** registers are stored but not enforced. Sv39 is the only translation mode.
  Svnapot, Svpbmt, Sstc, the hypervisor extension, vector, and debug triggers are not implemented.
* **Guest time** advances with the instruction count (with one thread per hart, `--smp N`, it
  follows the host clock). A long busy one-hart guest sees its clock run at the emulated rate,
  not in real time, unless `--clock real` is given.
* **Harts on separate threads** are not deterministic (two runs interleave differently), and the
  guest's memory model is the host's: RVWMO maps onto ARM's weak ordering with real barriers for
  `fence` and sequentially consistent atomics for every `amo`.
* **FP differences from QEMU** (`tests/isa/run_diff.sh` lists them):
  * Round-to-nearest-away ties at the underflow boundary: this emulator follows
    IEEE 754 and returns the smallest subnormal, where QEMU returns zero.
  * The inexact flag of a fused multiply-add is approximate when the product underflows and the
    rounding mode is not nearest-even (nearest-even runs on the host unit and is exact).
  * `misa` reports the H extension in QEMU's default CPU, not here.
* **Cooperative scheduling.** With `--single-thread`, one host thread runs the guest
  harts in turn. Parallel guest workloads use multiple host threads with `--smp N`.
* **Kernel messages at power-off.** The kernel prints `reboot: Power down` itself,
  even in quiet mode.
* **Typed-ahead input** sent before the guest opens its console is echoed by the
  guest's line discipline before the banner appears. It is delivered once the
  serial driver is ready, so it is not lost.
