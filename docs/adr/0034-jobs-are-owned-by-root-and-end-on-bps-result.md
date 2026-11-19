---
status: accepted
date: 2026-11-19
---

# Jobs are owned by the root module and end on `bp`'s result, never on the disk

A job is a long-running operation that outlives the client that started it, with a phase, a log and cancel. `plane.create` is the only kind in M1.

**Root owns jobs, modules contribute kinds.** The root module holds the one job table: ids, `startedBy` (the `hello` client identity, for display), status, the log buffers, persistence and every `job.*` method. A module contributes a **job kind**: it starts the work, reports phase changes, log lines and the final status to root, and knows how to cancel it. The plane module contributes `plane.create`, which `plane.create { args } → { job }` starts. A client handles every job the same way whatever its kind, and a second kind adds no methods.

**State is broadcast, logs are scoped.** `job.changed` and `job.removed` go to every connection, like sessions and planes. `job.log` goes only to connections whose scope holds the job: `job.watch { jobs[] }` replaces the scope, as fs and git watches do (ADR 0032). Each job's log is kept in memory, capped at about 1 MiB / 10k lines, keeping the tail. A newly watched job gets its backlog first, with `dropped` counting evicted lines, then live lines; a finished job sends its backlog only. An unknown or dismissed job gets `job.removed` at once.

**Any client may cancel or dismiss.** Cancel interrupts the work (SIGINT to `bp`); dismiss removes a finished job for everyone. About the last 50 finished jobs are kept, and older ones are auto-dismissed.

**The outcome is `bp`'s answer.** `plane.create` spawns `bp` with a `PlaneCreateRequest` on stdin and `--progress-fd 3` (ADR 0033, bitplane-cli ADR-0010). `phase` is the furthest step any member has started, from the progress events. The log is `bp`'s stderr verbatim. The final status comes from `bp`'s exit status and result alone: success, `script_failed` and `CreateAborted { rollback }` map to succeeded, scriptFailed, nothingCreated and partial. A cancelled create ends the same way, with whatever `bp` answers after unwinding on SIGINT. The daemon never inspects the plane on disk to decide how a job went.

**The table survives a restart; the log does not.** Root persists the job table in its state dir (ADR 0023) and restores it before the socket is bound; like `sessions.json`, other versions' tables are adopted once their pidfile lock is free (ADR 0027). A job that was running when its daemon stopped comes back `failed (daemon restarted)`, and its backlog is one note line saying the log was lost: its `bp` died with the daemon, so the plane may be partial.

## Considered Options

- **Jobs inside the plane module.** Rejected: the next kind would bring its own list, cancel and log methods, and clients would track each separately.
- **Reading the outcome from the disk** (the plane file, the incomplete latch, `bp status`). Rejected: the planes-dir watcher sees a plane appear before its worktrees land, another `bp` or the user may be changing it at the same time, and judging a plane from its files is `bp`'s semantics re-implemented in the daemon. `bp`'s result is the one answer, as ADR-0010 keeps progress from becoming a second one.
- **Broadcasting `job.log`.** Rejected: every client would pay for every log, including `pnpm`'s output from every script of every create.
- **Persisting the log.** Rejected: a restored job has no process behind it, the log is already capped in memory, and the scripts' own log files under the plane survive anyway.
- **Only the starting client may cancel or dismiss.** Rejected: `startedBy` is display only, the starting client may be gone for good, and M4 shared mode adds per-user authorization at this same point.

## Consequences

- A create keeps running when cc quits, and its outcome is waiting in `job.list` after relaunch.
- A daemon restart mid-create leaves a plane that `bp`'s own repair and destroy have to deal with; the job only says that it was interrupted.
- A client that wants a running job's log after reconnecting sends `job.watch` again and gets the capped tail.
- Spec: [M1 — bitplane-sessiond locally, with bitplane-cc moved onto it](https://github.com/bp-bitplane/bitplane-protocol/issues/8); decision: [Jobs: who hears the log, who may cancel](https://github.com/bp-bitplane/bitplane-protocol/issues/6).
