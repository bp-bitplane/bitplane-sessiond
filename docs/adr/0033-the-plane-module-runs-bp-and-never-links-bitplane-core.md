---
status: accepted
date: 2026-11-19
---

# The plane module runs `bp` as a child process and never links `bitplane-core`

Every plane operation the daemon performs, from `plane.list` to `plane.create`, is a `bp` run. The plane module never links `bitplane-core`: the crate is unpublished and its internals are unstable, while `bp`'s wire types, exit codes and error envelope are the contract bitplane maintains. This is cc ADR 0002, which moves here: cc no longer runs `bp` at all.

**A `Request` on stdin, not a command line.** The module spawns `bp --stdin --json` and writes one `Request` as JSON to its stdin (bitplane-cli ADR-0010). `bp` answers exactly as on the argv path: the `Response` on stdout, the `ErrorEnvelope` as the last line of stderr, its usual exit codes. The daemon has no argv renderer, so anything `bp` can be asked on a command line it can be asked here. `plane.create` adds `--progress-fd 3` (ADR 0034). `bp` runs with the daemon's login-shell environment captured at startup, like sessions.

**`plane.dispatch` passes `bp`'s wire through.** `dispatch { request } → { response }` carries the `Request` and the `Response` as opaque JSON (ADR 0021): the daemon neither validates nor reshapes them, and a client gets every `bp` command without the protocol re-declaring it. Where the module itself has to understand an answer (`bp list` for the sidebar model, `create`'s result, progress events) it decodes only the fields it uses and ignores the rest.

**`BP_REFUSED` means `bp` ran and said no.** Exit `0` or `3` (success with a finding) with a `Response` on stdout is the reply. Any other exit with an `ErrorEnvelope` on stderr is `BP_REFUSED`, its `detail` the envelope verbatim, whatever its `error` tag: a usage error, a lock timeout and a refusal alike. Everything else is "bp could not be run", a JSON-RPC internal error whose message names the cause and the tail of stderr: `bp` failed to spawn, died on a signal, or exited without an answer the module could parse. A client shows the first as `bp`'s own message and remedy, and the second as a broken install.

**Availability is probed once.** At startup the module runs `bp --version --json`; its `version` and `contract` go into `hello` as the plane module's `info`. No `bp` on the captured `PATH`, or a probe that fails, makes the module unavailable with a reason naming the fix (ADR 0023). A `bp` that disappears later is not re-probed; each call then fails as "bp could not be run".

**No queue in the daemon.** Calls run concurrently, each its own `bp`, and none of them delays sessions or typing. `bp`'s own locks serialise what must be serialised, and a lock that cannot be taken in time comes back as `BP_REFUSED` with `lock_timeout`. A dispatch whose connection closes still runs to its end; only its reply is dropped, since killing `bp` mid-mutation would leave work it only unwinds on SIGINT.

## Considered Options

- **Linking `bitplane-core`.** Rejected: it ties the daemon's build to an unpublished crate with no stability promise, and the daemon would have to track every internal change instead of one contract number.
- **Rendering argv from a `Request`.** Rejected: a second renderer of the wire, kept in step with clap by hand, is the drift ADR-0010 removed.
- **Typed `bp` requests and responses in `bitplane-protocol`.** Rejected: two copies of the engine's types drift, as cc's hand-copied ones did (ADR 0021).
- **One error for every `bp` failure.** Rejected: a client must tell "bp refused, here is why and what to do" from "bp is broken", and only the first carries an envelope to render.
- **Re-probing `bp` when a call fails.** Rejected, as for every module (ADR 0023).

## Consequences

- The fake `bp` in the protocol tests is a script that reads stdin and answers on stdout, stderr and its exit status, the same contract the real one keeps.
- A newer `bp` with fields the daemon does not know still works through `plane.dispatch` and the decoded paths; its `contract` in `hello` tells a client what it is talking to.
- Every plane call costs one process; the sidebar model is re-read on planes-dir changes and `plane.refresh`, not per client.
- Spec: [M1 — bitplane-sessiond locally, with bitplane-cc moved onto it](https://github.com/bp-bitplane/bitplane-protocol/issues/8); `bp`'s side: bitplane-cli ADR-0010, *Progress is a third channel*.
