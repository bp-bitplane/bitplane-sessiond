---
status: accepted
date: 2026-11-09
---

# The agent link follows `SessionStart` and is never cleared by `SessionEnd`

A session's **agent link** (agent session + cwd) is persisted state (ADR 0027) and is what restore resumes. It has its own rules, separate from live agent state (ADR 0030), because the events that end live state also fire when nobody meant to leave the conversation.

**`SessionEnd` never touches the link.** Claude Code runs `SessionEnd` whenever its PTY hangs up, whatever the cause: a daemon stopping, a quit, a reboot (cc#43 saw it on every quit path). Its reasons (`clear`, `resume`, `logout`, `prompt_input_exit`, `other`) do not tell those apart from the user typing `/exit`. `SessionEnd` only ends the live agent.

**The link changes only on a new `SessionStart`** of the outermost claude in the session's PTY. So `/clear`, `/branch`, an in-session `/resume` and `--fork-session` move the link to the new agent session (and the old Agent gets `agent.claude.removed`). `compact` reports the same id, so it changes nothing. A nested `claude -p` never moves it: it gets no hooks (ADR 0029) and is not the outermost claude.

**The link is cleared only when the user left claude while the session lives on**: claude exits and the shell regains the PTY foreground. `session.close` clears it too. An agent moved to Claude Code's background supervisor with `/bg` has left the PTY, so it counts as exited and its link is dropped.

**Shutdown** persists links before it stops hook ingestion and only then hangs up the PTYs (ADR 0027), so shells ending on the way out never clear a link. A crash or reboot cannot clear one either: the hooks have no daemon to reach, and the persisted file is what the next instance restores.

## Considered Options

- **Clearing the link on `SessionEnd`, guarded by a quit marker or a "client still alive" check** (cc's `REMOVE_LINK`). Rejected: that is the bug in [Agent sessions aren't resumed after quitting from the Dock or app menu](https://github.com/bp-bitplane/bitplane-cc/issues/43). Every quit path that skipped the marker lost every link, and any guard that depends on who is still alive races the shutdown.
- **Clearing on `SessionEnd` reasons other than `other`.** Rejected: `prompt_input_exit` and `other` both cover a user leaving, and `clear` / `resume` are followed by a `SessionStart` that moves the link anyway.
- **Taking the link from the registry.** Rejected: the registry reports `sessionId` only when set, says nothing once the process is gone, and would tie restore to a poll.
- **Keeping the link after `/exit`.** Rejected: a user who quit claude and kept the shell would see it come back on the next restore.

## Consequences

- If claude crashes and the shell regains the foreground, the link is cleared exactly as for `/exit`; the user resumes that conversation by hand.
- Whether `/bg` keeps the agent session id or fires `SessionEnd` is not documented by Claude Code; the rule above does not depend on either.
- The regression lives in the agent tests against a real daemon: shutdown, SIGTERM and SIGKILL with claude running keep the link, and `/exit` clears it.
- Decision: [Agent adapter interface and wiring](https://github.com/bp-bitplane/bitplane-sessiond/issues/5), items 3, 8 and 9.
