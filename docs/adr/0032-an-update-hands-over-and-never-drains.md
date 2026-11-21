---
status: accepted
date: 2026-11-19
---

# An update hands over, never drains, and the new instance stops the old one

When a host gets a new daemon version, the old instance does not drain. The new instance, before binding its socket, runs `<old version>/bitplane-sessiond shutdown --restore` against every other-version instance whose pidfile is locked, waits for each to exit, and adopts its `sessions.json`. Sessions come back as fresh shells with their agents resumed (ADR 0027). This amends the "old one drains and exits" lines of ADR 0020 and ADR 0022.

A host connection reaches exactly one daemon connection, relayed byte for byte, and the gateway accepts only its own daemon version. A draining instance would therefore keep running sessions that no client can see until it exits. The rule sits in sessiond so local and remote hosts behave the same, and so nothing needs to speak an old protocol: the old binary stops itself.

## Considered Options

- **Drain** (the old instance runs until its last session ends). Rejected: its sessions are invisible to every client in the meantime.
- **The gateway or cc runs the handover** (`daemon.shutdown { restore: true }` over the protocol). Rejected: the caller has to speak the old version, and every caller repeats the logic.

## Consequences

- An update restarts every session on the host. Through the gateway this happens automatically on connect; any confirmation is the client's to ask before it starts the new instance.
- Decided in [How the gateway reaches a host's daemon over system SSH](https://github.com/bp-bitplane/bitplane-gateway/issues/5).
