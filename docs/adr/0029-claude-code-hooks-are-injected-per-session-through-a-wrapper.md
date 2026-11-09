---
status: accepted
date: 2026-11-09
---

# Claude Code hooks are injected per session through a `claude` wrapper

The `agent.claude` module (ADR 0023) tracks the Claude Code agents running inside daemon sessions, and only those, without touching the user's Claude Code configuration. Every Claude Code fact below was checked against `claude` 2.1.318 and the official docs.

**The adapter interface.** The module talks to Claude Code only through one internal trait, so a second agent is a second adapter, not a fork of the module:

```text
trait AgentAdapter {
  kind() -> "claude"
  instrument(session) -> { env, path_prepend }      // wrapper dir, BITPLANE_SESSION_ID, hook token
  ingest(hook_payload) -> Option<AgentEvent>
  probe() -> Vec<LiveAgent { pid, agentSession?, cwd, status, waitingFor? }>
  resume_command(link) -> String                    // typed into the shell
}
AgentEvent = Started{id, cwd, source} | Busy | Waiting{reason} | Idle{error?} | Ended{reason}
```

`LiveAgent.agentSession` is optional because `claude agents --json` reports `sessionId` only when set. The module joins events to sessions through the sessions module's PTY foreground process group and process tree, its only dependency. Tests never reach the trait; they drive a real daemon with a fake `claude` on `PATH`.

**Wiring.** `bitplane-sessiond` is multi-call. A `claude` symlink to it lives in a daemon-owned dir in the instance's state dir (ADR 0022), and `instrument` prepends that dir to each session's `PATH`. Invoked as `claude`, the binary finds the next `claude` on `PATH` after its own dir and `exec`s it with `--settings <per-session hooks file>`. `--settings` takes one value, a file path or inline JSON, so a `--settings` the user passed (either form) is merged into the generated file rather than added as a second flag. The generated settings sit above the user, project and local files, and Claude Code merges hook lists across those levels, so the user's own hooks keep running. With `CLAUDECODE=1` in the environment (Claude Code sets it for everything it runs) the wrapper `exec`s the real `claude` unchanged. Nothing is installed globally: `~/.claude/settings.json` is never written.

**Hook transport.** Every hook is a `command` hook running `bitplane-sessiond hook`, which forwards its stdin over the daemon's unix socket on a dedicated **hook channel**, not the client protocol (ADR 0024). It carries `BITPLANE_SESSION_ID` and a per-session token that `instrument` puts in the session's env, so no process outside a session can speak for it. The forwarder is fire-and-forget with a short timeout and always exits 0: it never stalls a turn, and stays well inside the 1.5 s budget Claude Code gives all `SessionEnd` hooks together.

**Identity comes from `SessionStart`.** The agent session id is the `session_id` of each `SessionStart` (sources `startup`, `resume`, `clear`, `compact`, `fork`); `--session-id` is not used.

## Considered Options

- **A global hook install guarded by an env var** (cc's `BITPLANE_CC` hooks in `~/.claude/settings.json`). Rejected: it edits a file the user owns, runs in every Claude Code on the host, and needs a migration whenever the hook changes.
- **A shell function or alias for `claude`.** Rejected: shell-specific, and invisible to anything that `exec`s `claude` without an interactive shell.
- **A separate `CLAUDE_CONFIG_DIR` per session.** Rejected: it relocates the user's whole Claude Code state (credentials, history, plugins), not just the hooks.
- **`http` hooks.** Rejected: the daemon would need a TCP listener, which a host may forbid, and an `allowedHttpHookUrls` allowlist at any settings level silently drops them.
- **Choosing the id up front with `--session-id`.** Rejected: `/clear`, `/branch`, in-session `/resume` and `--fork-session` change the id anyway, and only `SessionStart` reports the new one.
- **Hooks over the client protocol.** Rejected: a hook would need a `hello` per event, and session-scoped tokens have no place in the client protocol.

## Consequences

- A shell whose rc files rebuild `PATH` bypasses the wrapper, as does `--bare` or `disableAllHooks` / `allowManagedHooksOnly` in the user's or the organization's settings. Then no hooks arrive, and tracking falls back to the registry (ADR 0030).
- A nested `claude -p` run by an agent sees `CLAUDECODE=1`, gets no hooks and can never take the session's agent link (ADR 0031).
- Resume goes through the wrapper (`claude --resume <id>`, typed into the shell), so the hooks are re-injected without a special path.
- Decision: [Agent adapter interface and wiring](https://github.com/bp-bitplane/bitplane-sessiond/issues/5), items 1–3 and 10.
