# bitplane-sessiond

Long-lived per-host daemon that owns PTYs and keeps shells and agents alive across disconnects, with multi-client attach, scrollback and agent-session resume.

Built for six targets (Linux gnu and musl, macOS; x86_64 and aarch64): see [docs/build.md](docs/build.md).
