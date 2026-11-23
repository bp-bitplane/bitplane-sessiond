---
status: accepted
date: 2026-10-23
amended: 2026-11-23
---

# Built-in modules behind one in-process `Module` trait

The daemon is one process split into **modules**, each owning exactly one namespace of protocol methods and events: root (`hello`, `daemon.shutdown`, `job.*`), sessions (`session.*`), agent (`agent.*`, every agent kind inside it), planes (`plane.*`), fs (`fs.*`) and git (`git.*`). All are compiled in.

Every module implements one `Module` trait:

- `namespace()`;
- `probe(host) -> Availability`, run **once, at startup**;
- method handlers, which the root router dispatches to by namespace prefix;
- an events sink provided by the root module, which fans events out to connections.

A module declares its dependencies, and the root resolves them at startup into narrow **typed handles**. A module whose dependency is unavailable is unavailable too. Change notifications between modules travel on the root's **internal bus**, never as direct calls; a typed handle is called directly only for what a module must answer before another can act, such as the agent questions below. Each module persists its own state under `<state>/<namespace>/`, inside the instance's state directory (ADR 0022).

Availability is decided at startup and never re-probed: no `bp` or no `git` makes its module unavailable. Agent kinds are the exception below. `hello` lists every module in `capabilities` with a `reason` that names the fix ("bp not found on PATH; install it and restart the daemon"), and a call into an unavailable module returns `MODULE_UNAVAILABLE`.

No module depends on planes. sessions, fs and git work on paths, and a session's `plane` is an opaque id that sessions stores and returns. The only dependency between modules is agent → sessions. Other modules contribute **job kinds** to the root's jobs, such as planes' `plane.create`.

**The agent module.** Agent messages are kind-generic (bitplane-protocol#22, as amended by [Claude terminals](https://github.com/bp-bitplane/bitplane-sessiond/issues/50)), so there is one `agent` namespace with the kinds inside it, each kind one adapter (ADR 0029), not a module per agent. The module is always available, and availability is per kind: `agent.list` reports each kind, re-resolved on the captured `PATH` on every list and launch, and `agent.kind.changed` broadcasts a change, so a missing `claude` makes the `claude` kind unavailable and its agent terminals `spawnFailed`, not the module (ADR 0029). Sessions call the agent module through a typed handle, `Launchers`, for what it must know when it spawns an agent terminal: the argv and env to launch, the agent link, the cwd of a known agent session, and which restored terminals come back exited (ADR 0027). A bus message could not answer those before the spawn. Change notifications still travel on the bus: the PTY child and foreground, a closed session, a changed link.

## Considered Options

- **One flat method table with no modules.** Rejected: a missing `bp` or `claude` would have no clean failure, and the hosts where a tool is absent (containers, M2 servers) are exactly where the protocol must still work.
- **Out-of-process modules now** (an executable speaking JSON-RPC over stdio that registers a namespace). Deferred: the trait mirrors that contract, so moving a module out later is mechanical, but no M1 module needs its own process.
- **Re-probing availability while running.** Rejected: a launchd agent's `PATH` is fixed at start, so a re-probe would often miss the fix anyway. Restarting the daemon is the remedy, and offering it is a client concern. Agent kinds are re-resolved instead: looking up one binary is cheap, and Claude Code installed into a directory already on the captured `PATH` shows up without a restart.
- **Modules calling each other directly.** Rejected for change notifications: the bus keeps each module's state behind its own serialisation, and the only dependency stays explicit. Spawn-time questions from sessions to the agent module go through `Launchers`, which is that dependency's typed handle.
- **One module per agent kind** (`agent.claude`, then `agent.codex` with a shared `agent.*` split out). Rejected in the amendment: the protocol's agent messages are kind-generic, and a kind differs only in its adapter.

## Consequences

- A host without `bp` or `git` still serves every other module; one without `claude` serves the agent module with the `claude` kind unavailable.
- Each module serialises its own state mutations; requests to different modules run concurrently and their replies are not ordered (ADR 0024).
- Decision: [How sessiond is split into modules](https://github.com/bp-bitplane/bitplane-sessiond/issues/4), amended by [Claude terminals](https://github.com/bp-bitplane/bitplane-sessiond/issues/50).
