---
status: accepted
date: 2026-10-18
---

# One daemon instance per protocol version per host, with a locked pidfile and a peer-uid check

A host runs one **daemon instance** of `bitplane-sessiond serve` per protocol version (ADR 0020). Each instance owns its own socket, pidfile and state directory, all keyed by its version, at the paths ADR 0020 names. During an update two instances run side by side: the old one drains and exits, and the new one adopts its state.

**The pidfile's `flock` is the instance's claim.** An instance takes an exclusive lock on its pidfile before binding the socket and holds it for its lifetime. A starter that cannot take the lock exits, so two racing starters never both bind. A free lock is the only proof that an instance is gone: only then may a starter remove a stale socket, and only then may another version's instance adopt that version's `sessions.json`.

**Every connection is checked.** The socket sits in a `0700` directory, and on accept the daemon compares the peer's uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS) with its own and closes the connection on a mismatch, before reading a byte. Every connection that passes is fully trusted, as ADR 0020 makes the host the isolation boundary.

**Start.** When `LISTEN_FDS` is set the daemon takes its socket from systemd or launchd. Otherwise a client starts it on demand: dial the version's socket; on no answer remove the stale socket, run `serve` detached (`setsid`, stdio to the log file) and poll with backoff for about 5 s. cc does this with the daemon it ships as a Tauri sidecar. In M1 the systemd and launchd units are documented templates, not an installer.

## Considered Options

- **One daemon serving every protocol version.** Rejected: it would need hot upgrade with fd handover to replace itself, which M1 does not build. Separate instances make an update an ordinary handover (`daemon.shutdown { restore: true }`, then adoption).
- **No pidfile; let the failing `bind` decide.** Rejected: a starter that finds a stale socket has to unlink it first, and two starters that both unlink and bind both succeed on different inodes.
- **The `0700` directory alone, without a peer check.** Rejected: socket activation and a misconfigured directory can both widen access, and the check costs one syscall per connection.

## Consequences

- An instance never touches another version's socket or state while that version's lock is held.
- On PID 1 in a container the daemon has to reap every child and handle `SIGTERM` itself; starting it there without systemd stays in the fog (ADR 0020).
- Research: [What the session daemon must do, and what to build on](https://github.com/bp-bitplane/bitplane-sessiond/issues/1); host and paths: ADR 0020 in `bitplane-protocol`.
