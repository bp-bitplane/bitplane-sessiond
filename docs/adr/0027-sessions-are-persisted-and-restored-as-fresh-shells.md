---
status: accepted
date: 2026-11-08
amended: 2026-11-12
---

# Sessions are persisted per daemon instance and restored as fresh shells

The daemon keeps sessions running while clients come and go; when the daemon itself stops (reboot, crash, SIGTERM, update handover) the processes are gone. The next daemon instance brings every session back as a **fresh process** with the same id, in its session root, with its plane, name, env and size. A regular terminal comes back as a fresh shell. A running Claude terminal is relaunched as `claude --resume` from its agent link (ADR 0029). This is cc ADR 0005's restore, moved into the daemon.

**What is persisted.** Per session: id, `seq`, root, plane, user name, the env given to `session.open`, the agent kind, the cwd an agent terminal was opened with, the agent link (agent session + cwd), the last size and, for a Claude terminal, whether it had exited. Not the OSC title or the scrollback: a restored session is a new process, and showing old output as history is a later feature.

**Where and when.** Each daemon instance writes its own `sessions.json` in its own version's state dir (ADRs 0020, 0022), carrying a schema `version`: a newer daemon reads older schemas, an older daemon ignores a newer file. Writes are atomic (tmp + rename) and debounced 500 ms after any change to a persisted field, so a crash loses at most half a second. Shutdown flushes synchronously in the order persist links → stop hook ingestion → hang up PTYs, so hanging up the PTYs never changes a link (ADR 0031).

**Which stops restore.** Crash, reboot, SIGTERM and `daemon.shutdown { restore: true }` all restore. Only `daemon.shutdown { restore: false }` clears the file, after SIGHUPing every session.

**Adoption.** On start a daemon restores its own file, then adopts every other version's file whose pidfile lock is free (ADR 0022): those sessions are appended after its highest `seq`, keeping their relative order, and the adopted file is deleted. One rule covers an update handover, a reboot in the middle of one and a crash of the old version.

**Startup order.** Every restored session is registered before the socket is bound, so the first `session.list` a client gets is complete; processes spawn concurrently. A session whose root is gone is listed `exited` with `rootGone: true`, gets no process and keeps its link; `session.restart` spawns once the root is back. Exited and spawn-failed regular terminals come back running.

**Claude terminals.** One that was running is relaunched with `--resume <id>` from its link, and its agent starts as `resuming`; without a link it gets a fresh `claude`. One that had exited comes back `exited` with its link kept and no process, so an `/exit` is never undone; `session.restart` resumes it. One that cannot be launched (unknown kind, `claude` missing from the captured `PATH`) comes back `spawnFailed { error }` with its link kept, and `session.restart` launches it once the cause is fixed. Either way its agent is listed `exited` with the link's agent session and cwd, as it was before the restart.

**No session generation.** A restored PTY starts again at offset 0, but a daemon restart drops every connection and every new attachment starts with a SNAPSHOT, so an old ack never meets a new offset. Clients tell a transport blip from a new daemon by `hello.daemon.instance` (ADR 0024): on a new instance they call `session.list` and diff by id. No `session.removed` or re-add is sent for restored sessions.

## Considered Options

- **Keeping processes alive across a daemon restart** (hot upgrade with fd handover). Rejected for M1, as in ADR 0022: a reboot loses them anyway, and restore handles every case through one path.
- **A session generation or epoch on `Session` and on frames.** Rejected: the dropped connection already separates old offsets from new ones, and `instance` answers "are these fresh shells?" once per connection instead of per session.
- **One shared `sessions.json` for every version.** Rejected: two instances run side by side during an update, and an older daemon must never rewrite a newer schema.
- **Relaunching every Claude terminal, exited ones too.** Rejected: a daemon restart would undo the user's `/exit`, now that exiting keeps the link (ADR 0031).
- **Dropping sessions whose root is gone.** Rejected: the user would not see what disappeared, and a temporarily unmounted folder would lose its session for good.
- **Persisting the scrollback.** Deferred: the restored shell is a new process, so old output could only be shown as inert history.

## Consequences

- An agent turn in flight, running programs and the scrollback are lost on a daemon restart, exactly as they were on a cc restart before; a cc restart no longer loses anything.
- The agent link is persisted state, separate from live agent state; when it changes and clears is the agent adapter's decision (ADR 0031).
- Decision: [How restored sessions come back after a daemon restart](https://github.com/bp-bitplane/bitplane-sessiond/issues/7), amended by [Claude terminals](https://github.com/bp-bitplane/bitplane-sessiond/issues/50).
