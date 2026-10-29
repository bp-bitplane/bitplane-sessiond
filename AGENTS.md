# bitplane-sessiond

Long-lived per-host daemon that owns PTYs and keeps shells and agents alive across disconnects, with multi-client attach, scrollback and agent-session resume.

## Agent skills

### Issue tracker

GitHub issues across the `bp-bitplane` repos, collected on org Project #1; cross-cutting work goes in `bitplane-protocol`. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context: `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`. `GLOSSARY.md` is a synced copy of the stack glossary in `bitplane-protocol`; edit it there. ADRs share one numbering across the stack's repos.
