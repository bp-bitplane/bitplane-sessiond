---
status: accepted
date: 2026-11-09
---

# Hooks set agent state at once, the `claude agents --json` registry wins, and no hooks means `tracking: poll`

Each tracked agent has a live `state` (`idle | busy | waiting{reason} | resuming | exited`), an optional `lastError` and a `tracking` mode (`hooks | poll`). Two sources feed it: the per-session hooks of a Claude terminal (ADR 0029), which are immediate but miss some edges, and Claude Code's own registry, `claude agents --json`, which is authoritative but polled.

**Hooks set state immediately.** `UserPromptSubmit` → busy, and it clears `lastError`. `PermissionRequest` and a `Notification` of type `permission_prompt` → waiting (permission); `elicitation_dialog` and `agent_needs_input` → waiting (input). `Stop` → idle. `StopFailure` fires instead of `Stop` when a turn ends on an API error → idle with `lastError` from its `error` and `last_assistant_message`. `SessionEnd` → exited; its `reason` is only a hint. No hook maps to waiting (other); that comes from the registry.

**The registry wins.** `claude agents --json` is documented, needs no TTY and lists every Claude Code on the host as an array: `pid` and `status` (`busy | waiting | idle`) while the process is alive, `waitingFor` while waiting (`permission prompt`, `input needed`, `sandbox request`, `worker request`, `dialog open`; mapped to permission, input and other), `sessionId` and `name` when set, `kind` interactive or background. Entries are matched to Claude terminals by pid through the PTY process tree; the rest are ignored. When it disagrees with the hook-derived state, the registry is right: no hook is documented for a turn the user interrupts with Esc, so the next poll is what turns busy into idle.

**Polling.** Every ~3 s while any agent is busy, waiting or has `tracking: poll`. Every ~30 s while any agent is live, even when all of them are idle and tracked by hooks: that is what catches an agent session moved to Claude Code's background supervisor with `/bg`, which no hook reports. With no live agent nothing is polled.

**Exit** is the first of `SessionEnd`, the PTY child (`claude` itself) exiting, or the claude pid vanishing from the process tree. An agent session that `/bg` moved out of the PTY counts as exited within ~30 s.

**Degraded mode.** If no `SessionStart` arrives within ~2 s of a launch, fresh or resumed, the hooks did not load: `disableAllHooks` or `allowManagedHooksOnly` in the user's or the organization's settings (ADR 0029). The agent is then listed from the registry alone with `tracking: poll`: state lags up to ~3 s, there is no `lastError`, `waiting.reason` is coarser and `/clear` shows only as a changed id. cc shows an error above that session's terminal naming those two settings.

**Resuming.** A resumed agent is `resuming` from the launch of `claude --resume` until its `SessionStart`, or until the ~2 s degraded-mode switch, so it never stays `resuming`. A resume that fails ends with `claude` exiting, and the session is `exited` like any other exit; there is no separate timer. There is no transcript pre-check: the transcript format is internal to Claude Code.

## Considered Options

- **Hooks only.** Rejected: with no hook for an Esc interrupt or `/bg` the agent would stay busy or live, and a lost hook would never be corrected.
- **The registry only.** Rejected: up to 3 s of lag on every edge, no `StopFailure` error, no `SessionStart` source, and polling every ~3 s all the time.
- **Reading `~/.claude/sessions/*.json` or the transcript.** Rejected: both are internal formats; the registry is the documented interface over the same data.
- **Treating Claude terminals without hooks as untracked.** Rejected: a setting that blocks hooks would make agents silently disappear from the UI instead of showing coarsely, with a reason.
- **A timer that ends a resume with no `SessionStart` as failed.** Rejected: with `claude` as the PTY child a failed resume ends in its own exit, and a timer would misjudge a slow start or blocked hooks.

## Consequences

- A hook edge can be reversed by the next poll when the registry has not caught up yet; the ~3 s poll keeps that window short, and the hook usually agrees.
- An idle host with live agents still runs `claude agents --json` every ~30 s.
- No cost is shown in v1: the only live source would replace the user's status line.
- Decision: [Agent adapter interface and wiring](https://github.com/bp-bitplane/bitplane-sessiond/issues/5), items 4–7, reworked by [Claude terminals](https://github.com/bp-bitplane/bitplane-sessiond/issues/50).
