---
status: accepted
date: 2026-11-09
---

# The agent link follows `SessionStart` and is never cleared by `SessionEnd`

A Claude terminal's **agent link** (agent session + cwd) is persisted state (ADR 0027) and is what resume, `session.restart` and restore launch with `--resume` (ADR 0029). It has its own rules, separate from live agent state (ADR 0030), because the events that end live state also fire when nobody meant to leave the conversation.

**`SessionEnd` never touches the link.** Claude Code runs `SessionEnd` whenever its PTY hangs up, whatever the cause: a daemon stopping, a quit, a reboot (cc#43 saw it on every quit path). Its reasons (`clear`, `resume`, `logout`, `prompt_input_exit`, `other`) do not tell those apart from the user typing `/exit`. `SessionEnd` only ends the live agent.

**The link changes only on a new `SessionStart`** of the `claude` that is the session's PTY child. So `/clear`, `/branch`, an in-session `/resume` and `--fork-session` move the link to the new agent session (and the old Agent gets `agent.removed`). `compact` reports the same id, so it changes nothing. A nested `claude -p` never moves it: it is launched without the session's `--settings`, so it gets no hooks (ADR 0029).

**Only `session.close` clears the link.** `claude` exiting, for `/exit`, a crash or `/bg`, leaves the session `exited` with its link kept, so the exited tab can resume the conversation and `session.restart` relaunches it with `--resume`. Restore brings an exited Claude terminal back exited (ADR 0027), so keeping the link never undoes an `/exit`.

**Shutdown** persists links before it stops hook ingestion and only then hangs up the PTYs (ADR 0027). A crash or reboot cannot change a link either: the hooks have no daemon to reach, and the persisted file is what the next instance restores.

## Considered Options

- **Clearing the link on `SessionEnd`, guarded by a quit marker or a "client still alive" check** (cc's `REMOVE_LINK`). Rejected: that is the bug in [Agent sessions aren't resumed after quitting from the Dock or app menu](https://github.com/bp-bitplane/bitplane-cc/issues/43). Every quit path that skipped the marker lost every link, and any guard that depends on who is still alive races the shutdown.
- **Clearing on `SessionEnd` reasons other than `other`.** Rejected: `prompt_input_exit` and `other` both cover a user leaving, and `clear` / `resume` are followed by a `SessionStart` that moves the link anyway.
- **Clearing the link when `claude` exits.** Rejected: the exited tab is where the user resumes from, a crash looks the same as `/exit`, and a daemon hanging up the PTY ends `claude` too. The earlier reason to clear it, a restore relaunching a conversation the user had left, is gone now that an exited Claude terminal is restored exited.
- **Taking the link from the registry.** Rejected: the registry reports `sessionId` only when set, says nothing once the process is gone, and would tie restore to a poll.

## Consequences

- A link outlives its conversation's process: resuming it while that agent session is still live is refused with `AGENT_LIVE`.
- Whether `/bg` keeps the agent session id or fires `SessionEnd` is not documented by Claude Code; the rule above does not depend on either.
- The regression lives in the agent tests against a real daemon: `daemon.shutdown`, SIGTERM and SIGKILL with `claude` running keep the link and relaunch it on restore, and `/exit` followed by a restore brings the session back exited with its link.
- Decision: [Agent adapter interface and wiring](https://github.com/bp-bitplane/bitplane-sessiond/issues/5), items 3, 8 and 9, reworked by [Claude terminals](https://github.com/bp-bitplane/bitplane-sessiond/issues/50).
