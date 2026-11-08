# bitplane-sessiond

Long-lived per-host daemon that owns PTYs and keeps shells and agents alive across disconnects, with multi-client attach, scrollback and agent-session resume.

## Agent skills

### Issue tracker

GitHub issues across the `bp-bitplane` repos, collected on org Project #1; cross-cutting work goes in `bitplane-protocol`. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`. `GLOSSARY.md` is a synced copy of the stack glossary in `bitplane-protocol`; edit it there. ADRs share one numbering across the stack's repos.

## Building against a local protocol checkout

`bitplane-protocol` is a git dependency pinned by `rev`. To build against the plane's sibling checkout instead, create the gitignored `.cargo/config.toml`:

```toml
[patch."https://github.com/bp-bitplane/bitplane-protocol"]
bitplane-protocol = { path = "../bitplane-protocol/crates/bitplane-protocol" }
```

The patch rewrites `Cargo.lock`; never commit that version. Push the protocol change, bump `rev`, and let CI's `--locked` build check the lock.
