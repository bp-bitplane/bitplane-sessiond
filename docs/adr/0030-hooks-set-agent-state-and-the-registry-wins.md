---
status: accepted
date: 2026-11-09
---

# Hooks set agent state at once, the `claude agents --json` registry wins, and no hooks means `tracking: poll`

Each tracked agent has a live `state` (`idle | busy | waiting{reason} | resuming | exited`), an optional `lastError` and a `tracking` mode (`hooks | poll`). Two sources feed it: the per-session hooks (ADR 0029), which are immediate but miss some edges, and Claude Code's own registry, `claude agents --json`, which is authoritative but polled.

**Hooks set state immediately.** `UserPromptSubmit` → busy, and it clears `lastError`. `PermissionRequest` and a `Notification` of type `permission_prompt` → waiting (permission); `elicitation_dialog` and `agent_needs_input` → waiting (input). `Stop` → idle. `StopFailure` fires instead of `Stop` when a turn ends on an API error → idle with `lastError` from its `error` and `last_assistant_message`. `SessionEnd` → exited; its `reason` is only a hint.

**The registry wins.** `claude agents --json` is documented, needs no TTY and lists every Claude Code on the host as an array: `pid` and `status` (`busy | waiting | idle`) while the process is alive, `waitingFor` while waiting (`permission prompt`, `input needed`, `sandbox request`, `worker request`, `dialog open`; mapped to permission, input and other), `sessionId` and `name` when set, `kind` interactive or background. Entries are matched to sessions by pid through the PTY process tree; the rest are ignored. It is polled every ~3 s while any agent is busy or waiting, and continuously while any agent has `tracking: poll`. When it disagrees with the hook-derived state, the registry is right: no hook is documented for a turn the user interrupts with Esc, so the next poll is what turns busy into idle.

**Exit** is the first of `SessionEnd`, the PTY child exiting, or the claude pid vanishing from the process tree and the registry.

**Degraded mode.** If a session's foreground process is `claude` but no `SessionStart` arrived within ~2 s, the hooks did not load (ADR 0029 lists how). The agent is then listed from the registry alone with `tracking: poll`: state lags up to ~3 s, there is no `lastError`, `waiting.reason` is coarser and `/clear` shows only as a changed id. cc shows an error above that session's terminal naming the likely cause.

**Resuming.** A resumed agent is `resuming` from the moment `claude --resume` is typed until its `SessionStart`. If the shell regains the foreground first, the agent goes `exited`, never staying `resuming`. There is no transcript pre-check: the transcript format is internal to Claude Code.

## Considered Options

- **Hooks only.** Rejected: with no hook for an Esc interrupt the agent would stay busy, and a lost hook would never be corrected.
- **The registry only.** Rejected: up to 3 s of lag on every edge, no `StopFailure` error, no `SessionStart` source, and polling all the time even when every agent is idle.
- **Reading `~/.claude/sessions/*.json` or the transcript.** Rejected: both are internal formats; the registry is the documented interface over the same data.
- **Treating sessions without hooks as untracked.** Rejected: an rc file rebuilding `PATH` would make agents silently disappear from the UI instead of showing coarsely, with a reason.

## Consequences

- A hook edge can be reversed by the next poll when the registry has not caught up yet; the ~3 s poll keeps that window short, and the hook usually agrees.
- No cost is shown in v1: the only live source would replace the user's status line.
- Decision: [Agent adapter interface and wiring](https://github.com/bp-bitplane/bitplane-sessiond/issues/5), items 4–7.
