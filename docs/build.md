# Building

The daemon ships for the six bootstrap targets, so M2's bootstrap can install it on any host it reaches:

| Target | Linking | Runs on |
|---|---|---|
| `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | dynamic, glibc ≥ 2.27 | glibc distros from Ubuntu 18.04 and Debian 10 on |
| `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` | fully static | any Linux, Alpine included |
| `x86_64-apple-darwin`, `aarch64-apple-darwin` | system | macOS |

`scripts/build-targets.sh` builds them into `target/dist/` and runs `scripts/check-binary.sh` on each. With no arguments it builds all six on macOS and the four Linux targets elsewhere; it also takes target names.

## libghostty-vt

Every target links libghostty-vt (ADR 0025), which `libghostty-vt-sys` builds from ghostty's source with Zig. `scripts/build-env.sh <command>` runs a command with what that build needs, and `build-targets.sh` and CI run every cargo command through it:

- `GHOSTTY_SOURCE_DIR` pointing at `vendor/ghostty` (gitignored), which `scripts/vendor-ghostty.sh` fetches at the commit the crate pins. Without it the crate's build script clones ghostty itself.
- On macOS, `scripts/macos-sdk/` first on `PATH` (see below).

Locally, `scripts/build-env.sh cargo test` works, or put the variable in the gitignored `.cargo/config.toml` (macOS still needs the shim on `PATH`):

```toml
[env]
GHOSTTY_SOURCE_DIR = { value = "vendor/ghostty", relative = true }
```

Dev and test builds compile ghostty optimized (`[profile.dev.package.libghostty-vt-sys]`): its Debug build verifies page integrity on every scroll, which stalls a busy session.

## Toolchain

- Rust from `rust-toolchain.toml`; the script adds the targets and `llvm-tools` itself.
- Zig exactly 0.15.2, for the Linux targets. The script refuses any other version. Download it from [ziglang.org](https://ziglang.org/download/#release-0.15.2) and put it on `PATH`, or point `CARGO_ZIGBUILD_ZIG_PATH` at it.
- `cargo install cargo-zigbuild --locked`.

Linux targets are cross-built with `cargo zigbuild`, on macOS as well as on Linux. CI builds all six on a macOS runner.

## Guarantees

- **glibc floor.** The gnu targets build against glibc 2.27 (`--target <triple>.2.27`), and `check-binary.sh` fails when a binary references any `GLIBC_` symbol version newer than 2.27.
- **Static musl.** `check-binary.sh` fails when a musl binary has a program interpreter or a `NEEDED` entry.
- **It runs.** `scripts/smoke.sh <target>` runs `bitplane-sessiond dev hello`, which auto-starts `serve` and must answer `hello`. Linux binaries run in Docker (`alpine` for musl, `debian:10` for gnu), Darwin ones directly. CI smokes the four Linux binaries on x86_64 and arm64 runners and the aarch64 Darwin one; `x86_64-apple-darwin` is only checked for its architecture.

## macOS SDK ≥ 26.4

Zig 0.15.2's linker breaks against the macOS 26.4 and later SDKs (undefined `_waitpid`, `_sigaction`… while linking ghostty's build runner; [ziglang/zig#31658](https://codeberg.org/ziglang/zig/issues/31658)), and Zig ignores `SDKROOT`: it asks `xcrun --sdk macosx --show-sdk-path`. `scripts/macos-sdk/xcrun` is a shim that answers that with `BITPLANE_MACOS_SDK`, or else the newest installed SDK older than 26.4 (Command Line Tools or any Xcode), and passes every other call to `/usr/bin/xcrun`. The build fails with a clear message when no such SDK is installed: install Command Line Tools 26.3 or older (they ship the 15.x SDKs as well), or point `BITPLANE_MACOS_SDK` at one. Drop the shim once a Zig release with the fix is pinned.
