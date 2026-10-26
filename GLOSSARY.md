# bitplane-sessiond

The long-lived per-user process on a host that keeps sessions alive and answers every host-side call. Session, agent session and agent link are used exactly as `bitplane-cc/GLOSSARY.md` defines them; host and plane as `bitplane-cli/CONTEXT.md` does.

## Language

**Daemon**:
The one long-lived `bitplane-sessiond` process per user on a host; it owns that host's sessions and jobs.
_Avoid_: Host agent, server, sessiond (when meaning the running process)

**Module**:
A part of the daemon that owns one namespace of protocol methods and events, such as `session` or `agent.claude`; a host may lack one, and the client is told at hello. The root module owns the methods with no namespace.
_Avoid_: Plugin, service, component

**Session root**:
The directory a session's shell starts in; for a plane's session, its plane root.
_Avoid_: Cwd, plane path, working directory

**Attachment**:
One client's live view of a session; a session may have several at once.
_Avoid_: Connection, subscription, client (when meaning the view)

**Job**:
A long-running operation on a host, with phases, a log and cancel, that outlives the client that started it; creating a plane is one.
_Avoid_: Create (as a noun), task, operation
