---
status: accepted
date: 2026-11-08
---

# libghostty-vt is the terminal-state engine, with a daemon-side snapshot fix-up

Every session's output is always fed into its own terminal emulator, whether or not any attachment exists, so an attachment can start from the terminal as it is now instead of a replay. The emulator is **`libghostty-vt`** (the crate 0.2.2, building ghostty at a pinned commit from source vendored through `GHOSTTY_SOURCE_DIR`), behind a `Spool`-style trait of our own (the shape of shpool's `SessionSpool`). It is the same Ghostty core cc renders with (cc ADR 0001), so the daemon and the webview parse every byte identically.

**An attach is one SNAPSHOT, then OUTPUT** from the next offset, taken under one lock (ADR 0024). The snapshot is the formatter's VT output with a fix-up the daemon applies, because the formatter alone does not restore the screen:

1. Re-emit the cursor CUP last; the formatter emits it before DECSTBM and the tabstop rebuild, which move it.
2. Pad trailing blank rows with LFs under a full-screen region, then restore the region; the formatter drops them, which leaves the viewport too low when there is scrollback.
3. Append OSC 2 with the title.
4. Append DECSCUSR with the cursor shape from `CURSOR_VISUAL_STYLE` (`block_hollow` → block, blink from `CURSOR_BLINKING`).
5. Turn `palette` off, so no OSC 4 overrides the client's colour theme.

**Scrollback is a byte budget.** `max_scrollback` maps to ghostty's PageList `max_size` despite its name; about 1 MiB holds 1k rows at 120 columns. The setting is exposed in bytes, never as a line count.

**The primary-screen gap is accepted.** The formatter only covers the active screen, and the C API cannot select the other one. An attachment that opens while an alt-screen app runs (Claude Code always is one) gets that app's screen exactly, but not the shell output behind it, and sees a blank primary screen once the app exits. There is no shadow terminal and no patched ghostty; the formatter gaps go upstream (protocol#7).

## Considered Options

- **`vt100` 0.17** (pure Rust, the fallback named before the spike). Rejected: its snapshot has no scrollback, lands on the primary screen and drops focus events, kitty keyboard flags and the title, and it uses about four times the memory (39.6 vs 10.0 MiB per session at 10k rows).
- **Replaying a raw output ring buffer.** Rejected, as cc ADR 0004 already did: a replay that starts mid escape sequence or after an alt-screen switch garbles the screen.
- **A shadow primary-screen terminal, or patching ghostty's formatter.** Rejected: a second emulator per session for a gap only late attachments see, or a fork to carry; the fix belongs upstream as a screen selector.
- **No terminal state, redraw on `SIGWINCH`** (abduco, dtach). Rejected: a reattach would lose all scrollback and anything a program does not redraw.

## Consequences

- The build needs Zig 0.15.2 exactly, a pre-26.4 macOS SDK for its linker, and glibc ≥ 2.27 on the gnu targets (`docs/build.md`).
- The ghostty C API is pre-1.0, so bumping the pinned commit means re-running the snapshot fidelity tests over the recorded fixtures.
- Modes are emitted before content; with DECAWM off or ORIGIN/IRM on, a soft-wrapped row could replay skewed. No recording hits it, and it stays untested.
- Research: [What the session daemon must do](https://github.com/bp-bitplane/bitplane-sessiond/issues/1); spike: [libghostty-vt as the terminal-state engine](https://github.com/bp-bitplane/bitplane-sessiond/issues/6).
