---
status: accepted
date: 2026-10-23
---

# Built-in modules behind one in-process `Module` trait

The daemon is one process split into **modules**, each owning exactly one namespace of protocol methods and events: root (`hello`, `daemon.shutdown`, `job.*`), sessions (`session.*`), agent.claude (`agent.claude.*`), planes (`plane.*`), fs (`fs.*`) and git (`git.*`). All are compiled in.

Every module implements one `Module` trait:

- `namespace()`;
- `probe(host) -> Availability`, run **once, at startup**;
- method handlers, which the root router dispatches to by namespace prefix;
- an events sink provided by the root module, which fans events out to connections.

A module declares its dependencies, and the root resolves them at startup into narrow **typed handles**. A module whose dependency is unavailable is unavailable too. Change notifications between modules travel on the root's **internal bus**, never as direct calls. Each module persists its own state under `<state>/<namespace>/`, inside the instance's state directory (ADR 0022).

Availability is decided at startup and never re-probed: no `bp`, no `git` or no `claude` makes its module unavailable. `hello` lists every module in `capabilities` with a `reason` that names the fix ("bp not found on PATH; install it and restart the daemon"), and a call into an unavailable module returns `MODULE_UNAVAILABLE`.

No module depends on planes. sessions, fs and git work on paths, and a session's `plane` is an opaque id that sessions stores and returns. The only dependency between modules is agent.claude → sessions. Other modules contribute **job kinds** to the root's jobs, such as planes' `plane.create`.

## Considered Options

- **One flat method table with no modules.** Rejected: a missing `bp` or `claude` would have no clean failure, and the hosts where a tool is absent (containers, M2 servers) are exactly where the protocol must still work.
- **Out-of-process modules now** (an executable speaking JSON-RPC over stdio that registers a namespace). Deferred: the trait mirrors that contract, so moving a module out later is mechanical, but no M1 module needs its own process.
- **Re-probing availability while running.** Rejected: a launchd agent's `PATH` is fixed at start, so a re-probe would often miss the fix anyway. Restarting the daemon is the remedy, and offering it is a client concern.
- **Modules calling each other directly.** Rejected: the bus keeps each module's state behind its own serialisation, and the only dependency stays explicit.

## Consequences

- A host without `bp`, `git` or `claude` still serves every other module.
- agent.codex arrives as another built-in module; a shared `agent.*` module is split out of agent.claude only then.
- Each module serialises its own state mutations; requests to different modules run concurrently and their replies are not ordered (ADR 0024).
- Decision: [How sessiond is split into modules](https://github.com/bp-bitplane/bitplane-sessiond/issues/4).
