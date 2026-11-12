---
status: accepted
date: 2026-11-09
---

# Claude terminals launch `claude` with per-session `--settings`

The `agent` module (ADR 0023), through its `claude` kind, tracks Claude Code agents in **Claude terminals**, and only there, without touching the user's Claude Code configuration. Every Claude Code fact below was checked against `claude` 2.1.318 and the official docs.

**A Claude terminal runs `claude` as the PTY child, with no shell.** `session.open { …, agent?: { kind, resume? } }` opens one; without `agent` the session is a regular terminal running the login shell, and gets no agent env, no token and no `PATH` change. `Session.agent?: { kind }` tells the two apart. The PTY child exiting is `claude` exiting, so the session becomes `exited { code }`. A `claude` the user starts by hand in a regular terminal is not tracked: no state, no link, no restore.

**Launch.** The daemon runs `claude --settings <per-session hooks file> [--resume <id>]` in the link's cwd, falling back to the session root, with the captured login-shell env (ADR 0022) plus `BITPLANE_SESSION_ID` and the session's hook token. The binary is resolved on that env's `PATH` on every launch and every `agent.list`, so installing or removing `claude` shows up without a daemon restart. Every launch failure, an unknown kind or a missing binary, is `spawnFailed { error }`: the session is still created and persisted, and `session.restart` launches it once the cause is fixed. M1 passes no other arguments; users switch model and mode with in-session commands. `--settings` sits above the user, project and local settings files, and Claude Code merges hook lists across those levels, so the user's own hooks keep running. Nothing is installed globally: `~/.claude/settings.json` is never written.

**The adapter interface.** The module talks to Claude Code only through one internal trait, so a second agent kind is a second adapter, not a fork of the module:

```text
trait AgentAdapter {
  kind() -> "claude"
  resolve(env) -> Result<binary, error>              // on the captured PATH
  launch(session, link?) -> { argv, env }            // --settings, --resume, BITPLANE_SESSION_ID, token
  ingest(hook_payload) -> Option<AgentEvent>
  probe() -> Vec<LiveAgent { pid, agentSession?, cwd, status, waitingFor? }>
}
AgentEvent = Started{id, cwd, source} | Busy | Waiting{reason} | Idle{error?} | Ended{reason}
```

`LiveAgent.agentSession` is optional because `claude agents --json` reports `sessionId` only when set. Registry entries are joined to sessions by pid through the PTY process tree, whose root is now `claude` itself. Tests never reach the trait; they drive a real daemon that launches a fake `claude`, given by absolute path, which asserts its argv.

**Hook channel.** Every hook is a `command` hook running `bitplane-sessiond hook`, which forwards its stdin over a dedicated **hook channel**, `hook.sock` in the instance's runtime dir, not the client protocol (ADR 0024). The socket is owner-only (0600) in an owner-only (0700) dir: the same trust model as the client socket, everything running as the user is trusted. Unlike the client socket there is no peer-uid check, because macOS cannot report the uid of a hook process that has already exited, and the file modes already keep other users out. The forwarder carries `BITPLANE_SESSION_ID` and the per-session token; the token routes an event to its session and keeps a stray or stale hook from landing on the wrong one, it does not authenticate the user. A hook with a wrong token or session id is dropped. The forwarder is fire-and-forget with a short timeout and always exits 0: it never stalls a turn, and stays well inside the 1.5 s budget Claude Code gives all `SessionEnd` hooks together.

**Identity comes from `SessionStart`.** The agent session id is the `session_id` of each `SessionStart` (sources `startup`, `resume`, `clear`, `compact`, `fork`); `--session-id` is not used.

## Considered Options

- **A `claude` wrapper on each session's `PATH`** (a multi-call `bitplane-sessiond` symlinked as `claude` in a daemon-owned dir, which `exec`ed the next `claude` with a generated `--settings`, skipped under `CLAUDECODE=1`). Rejected after it shipped on the branch: on macOS `/etc/zprofile` runs `path_helper`, which puts the system paths, `/opt/homebrew/bin` among them, ahead of everything already on `PATH`, so a login shell found the real `claude` first and the wrapper was bypassed on an ordinary Mac. It also had to merge a user's own `--settings` (the flag takes one value) and tracked a `claude` typed into any session, nested or not.
- **Typing `claude` into a login shell in the session.** Rejected: exit and resume depend on who holds the PTY foreground, a resume typed into the shell races the rc files, and an exited agent leaves a shell that looks like a live session.
- **A global hook install guarded by an env var** (cc's `BITPLANE_CC` hooks in `~/.claude/settings.json`). Rejected: it edits a file the user owns, runs in every Claude Code on the host, and needs a migration whenever the hook changes.
- **A shell function or alias for `claude`.** Rejected: shell-specific, and invisible to anything that `exec`s `claude` without an interactive shell.
- **A separate `CLAUDE_CONFIG_DIR` per session.** Rejected: it relocates the user's whole Claude Code state (credentials, history, plugins), not just the hooks.
- **`http` hooks.** Rejected: the daemon would need a TCP listener, which a host may forbid, and an `allowedHttpHookUrls` allowlist at any settings level silently drops them.
- **Choosing the id up front with `--session-id`.** Rejected: `/clear`, `/branch`, in-session `/resume` and `--fork-session` change the id anyway, and only `SessionStart` reports the new one.
- **Hooks over the client protocol.** Rejected: a hook would need a `hello` per event, and session-scoped tokens have no place in the client protocol.

## Consequences

- `disableAllHooks` or `allowManagedHooksOnly` in the user's or the organization's settings keeps the hooks from loading. Then no `SessionStart` arrives and tracking falls back to the registry (ADR 0030).
- A nested `claude -p` run by an agent inherits the env but not the `--settings` flag, so it gets no hooks and can never take the session's agent link (ADR 0031).
- Resume is the same launch with `--resume <id>`, so the hooks are injected again without a special path.
- Decision: [Agent adapter interface and wiring](https://github.com/bp-bitplane/bitplane-sessiond/issues/5), items 1–3 and 10, reworked by [Claude terminals](https://github.com/bp-bitplane/bitplane-sessiond/issues/50).
