---
status: accepted
date: 2026-11-08
---

# Any attachment may type, the latest active one owns the size, and the PTY never waits on a client

A session can have several attachments, from one or several clients, and they are equal.

**Input is not arbitrated.** Any attachment may type. Each INPUT frame is written to the PTY whole, so a paste is never interleaved with another attachment's input, and nothing more is promised: no lock, no turn-taking. An attachment opened `readOnly` gets `READ_ONLY` on `session.attachment.error` when it types, and typing into an exited session gets `SESSION_EXITED` there too.

**The latest active attachment owns the size.** An attachment becomes active by sending input or a resize; opening it is not activity, so a client that only looks never changes the size. The PTY takes the size of the attachment that was active last. An `ignoreSize` attachment never sets it.

**Flow control is per attachment, and the PTY never waits on it.** Each attachment has its own cumulative byte-ack window (`session.attachment.ack`): 128 KiB window, 32 KiB low water, the numbers of cc ADR 0003. The PTY reader feeds the emulator (ADR 0025) and hands bytes to every attachment's queue without blocking. An attachment whose queue passes 256 KiB is marked behind: its queue is dropped, and once it acks it gets a fresh SNAPSHOT at the current offset instead of the bytes it missed. The other attachments and the shell are unaffected.

## Considered Options

- **One writer at a time** (an input lock or a "driver" attachment). Rejected: none of tmux, zellij, shpool or abduco arbitrates, moving between clients would need a hand-off, and whole-frame writes already keep pastes intact.
- **Smallest attachment wins** (tmux's `smallest`). Rejected: a forgotten small window would shrink the terminal everywhere. Latest active is tmux's default `latest`.
- **The latest attachment to open owns the size.** Rejected: a read-mostly client reattaching in the background would steal the size from the one being typed in.
- **Backpressure from the slowest attachment to the PTY.** Rejected: one stalled view (a sleeping laptop, a slow link in M2) would freeze the shell and every other client. Dropping the queue is safe because the emulator keeps the whole state, and a snapshot is cheaper than replaying a backlog.

## Consequences

- A snapshot after a drop can be up to a whole scrollback budget; an attachment that keeps falling behind pays that each time.
- Two people typing at once interleave at frame boundaries; that is the documented behaviour, not a bug.
- Research: [What the session daemon must do](https://github.com/bp-bitplane/bitplane-sessiond/issues/1), §3.
