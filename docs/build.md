# Building

The daemon ships for the six bootstrap targets, so M2's bootstrap can install it on any host it reaches:

| Target | Linking | Runs on |
|---|---|---|
| `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` | dynamic, glibc ≥ 2.27 | glibc distros from Ubuntu 18.04 and Debian 10 on |
| `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` | fully static | any Linux, Alpine included |
| `x86_64-apple-darwin`, `aarch64-apple-darwin` | system | macOS |

`scripts/build-targets.sh` builds them into `target/dist/` and runs `scripts/check-binary.sh` on each. With no arguments it builds all six on macOS and the four Linux targets elsewhere; it also takes target names.

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

Zig 0.15.2's linker breaks against the macOS 26.4 and later SDKs. Nothing Zig builds targets Darwin yet: the Darwin binaries are linked by Apple's toolchain alone. Once libghostty-vt is linked, its Darwin build has to point Zig at a pre-26.4 SDK (an `xcrun` shim or `SDKROOT` to a pinned 26.3 Command Line Tools), unless a Zig fix has landed by then.
