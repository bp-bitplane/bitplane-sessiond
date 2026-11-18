---
status: accepted
date: 2026-11-18
---

# fs and git watches are scoped per connection, over shared OS watchers

Sessions, agents, planes and jobs are broadcast to every connection. What a client watches on disk is not: a cc window shows one plane's folders and repos, and another client shows others. So the fs and git modules keep **one watch scope per module per connection**.

**`*.watch { paths[] }` replaces the scope.** The call names every path the connection wants from now on; `[]` stops. There are no subscribe and unsubscribe calls to pair up, and a client that lost track of what it asked for just sends its whole set again. A connection that closes drops its scopes.

**Only newly added paths get initial state.** A path already in the scope gets nothing on a repeated watch, and a new one gets one `fs.changed { path }` or `git.changed { path, status }` right after the reply, so a watch alone is enough to render. Events after that are changes only; a client that starts using a path reads it itself (`fs.list`, `git.status`).

**`gone` ends a watch.** A path that cannot be watched (missing, not a folder, not a repo) gets `{ path, gone: true }` at once. A watched path that vanishes later gets `gone` once and leaves the scope; the daemon never re-arms it, even if the path comes back. Watching it again is the client's call.

**Depth differs by module.** `fs.watch` is non-recursive: a path is one folder, and a change in a subfolder is not its change, as in cc's Files tree where each expanded folder is watched on its own. `git.watch` covers the repo's worktree and its git dir (and, for a linked worktree, the common dir) recursively, because a status changes on an edit anywhere in the worktree and on a commit, checkout or stage that only touches the git dir. Changes inside ignored build folders, lock files and object storage are skipped.

**OS watchers are shared and ref-counted.** Each module holds one watcher per canonical path, however many connections and spellings name it, and drops it when the last scope lets go. Events are routed to every connection whose scope holds the path, under the spelling that connection used.

**git is read through the CLI.** The git module runs `git` (`status --porcelain=v2 --branch -z`, `show`, `check-ignore`) instead of linking a git library, for the reasons of cc ADR 0006, which moves here: the CLI agrees with what the user and the agents run in the same worktrees. Without `git` on the login shell's `PATH` the module is unavailable (ADR 0023), and `fs.list` reports nothing ignored but `.git`.

## Considered Options

- **Subscriptions** (`watch { path }` / `unwatch { path }` per path). Rejected: a client has to mirror the daemon's set, and a missed unwatch after a reconnect or a crash leaks a watcher. Replacing the scope is idempotent.
- **Broadcasting fs and git events to every connection.** Rejected: every client would pay for every other client's folders, and a busy worktree in one plane would flood windows showing another.
- **Re-arming a path that comes back.** Rejected: the daemon would have to poll for every vanished path forever, and a recreated folder or reinitialised repo is a new thing the client should look at deliberately.
- **Sending initial state for every path on every watch.** Rejected: cc re-sends its whole scope whenever a folder is expanded, so each expansion would re-read every open folder and repo.

## Consequences

- A client merging several views (cc's windows) sends the union of their paths and routes events itself.
- A repo whose status reads keep failing is reported as `git.changed` with an `error`, not as `gone`; only a missing folder or a path that is no longer a repo is gone.
- Spec: [M1 — bitplane-sessiond locally, with bitplane-cc moved onto it](https://github.com/bp-bitplane/bitplane-protocol/issues/8); decision: [Scoped watches: one scope or subscriptions](https://github.com/bp-bitplane/bitplane-protocol/issues/5).
