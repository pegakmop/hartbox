# Hartbox

**Alpine Linux on iPhone.** Hartbox boots a full RISC-V Linux system locally
with a terminal, persistent storage, and IPv4 and IPv6 networking. Install
Alpine packages with `apk` or connect from your Mac with `rssh`. The Rust
interpreter runs without JIT.

[Download the latest release](https://github.com/u6420771547-hash/hartbox/releases/latest) · [Repository index](REPO_INDEX.md)

## Run on iPhone

The release includes an unsigned ARM64 IPA. It needs signing for your device
before installation. You can also build and run Hartbox from Xcode on a Mac.

```sh
git clone https://github.com/u6420771547-hash/hartbox.git
cd hartbox
rustup target add aarch64-apple-ios
open ios/rvemu.xcodeproj
```

Select the **Hartbox** scheme and choose your team under **Signing & Capabilities**.
If Xcode reports a bundle identifier conflict, enter an identifier unique to
your team. Select your connected iPhone and run the app. Enable Developer Mode
if Xcode prompts you. [Apple’s device setup guide](https://developer.apple.com/documentation/xcode/building-and-running-an-app)

Once Alpine starts, try installing a package from the terminal.

```sh
apk update
apk add python3
python3 --version
```

Files and installed packages survive guest reboots. Use `exit` from the shell
to shut down the guest cleanly.

## Connect from your Mac

Download and extract the [macOS rssh archive](https://github.com/u6420771547-hash/hartbox/releases/latest).
You can also run [`tools/rssh`](tools/rssh) from this checkout.

1. Connect the iPhone and Mac to the same network and keep Hartbox open.
2. Run `rssh start` in the Alpine shell. It starts OpenSSH and announces the guest.
3. Run `./rssh list` on your Mac from the extracted archive. Connect with
   `./rssh alpine-1a2b`, using the name listed for your guest. Confirm the
   fingerprint shown in Alpine and enter its pairing code when prompted.

## Run the emulator from a shell

The same Rust engine is available as the `rvemu` CLI. Build and boot it from
the checkout with Rust 1.87 or later and `curl`, `gzip`, and `cpio` installed.

```sh
cargo build --release
examples/boot-alpine.sh
```

The script downloads the Alpine boot files on first use and keeps the root
disk at `~/.rvemu/root.img`. Use `--root-disk FILE` to choose a separate image,
`--ephemeral` for a temporary guest, or `--stats` to report emulator speed on
exit. Run `target/release/rvemu --help` for the full list of options.

For the code layout, tests, and benchmark results, see the [repository index](REPO_INDEX.md).
