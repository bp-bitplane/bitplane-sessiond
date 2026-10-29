# bitplane

The shared vocabulary of the bitplane stack: a set of small tools (`bp`, `bitplane-sessiond`, `bitplane-gateway`, `bitplane-cc`) that create planes and run agent sessions in them. This file is the source; every other repo carries a synced copy and edits it here.

## Stack and access

**Host**:
One isolated setup of the stack: one OS account in one OS environment (a machine, a VM or a container). Planes, projects and sessions live on exactly one host, and a host has exactly one planes directory, one projects directory and one daemon.
_Avoid_: Machine (when meaning the host), server, box

**OS account**:
The operating-system account a host runs as; two OS accounts on one machine are two hosts.
_Avoid_: User, OS user

**Machine**:
The physical or virtual computer hosts run on; a plain word, not a domain concept, and hosts are never grouped by it.
_Avoid_: Host (when meaning the computer)

**User**:
A human using any of the bitplane tools; holds one or more identities.
_Avoid_: Person, OS user (that is the OS account), account

**Identity**:
A named principal the gateway knows, such as `alice`, `team-a` or `ci-bot`, that the access list grants hosts to; bots have identities but no user.
_Avoid_: User (when meaning the principal), principal, account

**Access list**:
The gateway's mapping of identities to the hosts they may reach; the only boundary between identities, since everyone on a host is fully trusted there.

**Gateway**:
The service that keeps the host registry, reaches hosts, bootstraps them and routes clients to them.

**Personal mode**:
A gateway serving one user and their own hosts, with the single implicit identity `local`.

**Shared mode**:
A gateway serving many users of one organisation, who sign in through an auth plugin.

## Sessions

**Daemon**:
The long-lived `bitplane-sessiond` of a host; it owns that host's sessions and jobs, and there is exactly one per host.
_Avoid_: Host agent, server, per-user daemon, sessiond (when meaning the running process)

**Daemon instance**:
One running `bitplane-sessiond` process of one protocol version; after an update the old instance drains while the new one takes over.
_Avoid_: Daemon (when meaning one version's process), versioned daemon

**Module**:
A part of the daemon that owns one namespace of protocol methods and events, such as `session` or `agent.claude`; a host may lack one, and the client is told at hello. The root module owns the methods with no namespace.
_Avoid_: Plugin, service, component

**Session**:
One shell process with its own terminal, belonging to exactly one plane.
_Avoid_: Terminal, shell, tab

**Session root**:
The directory a session's shell starts in; for a plane's session, its plane root.
_Avoid_: Cwd, plane path, working directory

**Attachment**:
One client's live view of a session; a session may have several at once.
_Avoid_: Connection, subscription, client (when meaning the view)

**Job**:
A long-running operation on a host, with phases, a log and cancel, that outlives the client that started it; creating a plane is one.
_Avoid_: Create (as a noun), task, operation

**Agent session**:
One conversation of a coding agent such as Claude Code, run inside a session and resumable by its own id.
_Avoid_: Session (when meaning the agent's conversation), chat

**Agent link**:
The record of which agent session a session last ran; restoring the session resumes that agent session.
_Avoid_: Agent binding, resume token

## Planes

**Plane**:
A named set of `(project, branch)` worktrees sharing one lifecycle, on a single host, in one plane root; at most one worktree per project, and the plane itself has no branch.
_Avoid_: Worktree group, workspace, task, group, topic

**Plane root**:
`<planes-dir>/<plane-id>/`, the one directory holding a single plane's worktrees; its name is the plane id, so renaming a plane is moving it.
_Avoid_: Plane directory, plane path, plane folder

**Planes directory**:
The directory on a host under which plane roots are created, defaulting to `~/planes`; exactly one per host.

**Plane id**:
The name of a plane root, unique per host and mutable by design; generated ids start with the reserved `bp-` prefix.

**Plane file**:
`plane.toml` at the plane root: the membership list of the plane, with exactly `version`, `id` and `[members]`.

**Member**:
One worktree in a plane, together with what it is a worktree of: a project or an ad-hoc repo.
_Avoid_: Worktree (on its own), project, repo (when meaning a plane's member)

**Ad-hoc member**:
A member whose worktree comes from a repo that is not a project; it has no name and no scripts.
_Avoid_: Unregistered member, bare member, non-adopted project

**Plane layout**:
Where a member's worktree sits inside the plane root: a path derived once, at create time, from the project's source.

## Projects

**Project**:
Exactly one repo plus its scripts and settings; the thing a plane can contain.
_Avoid_: Source repo (when meaning the project), repository, registry entry

**Project source**:
A project's git URL or local path; which of the two it is decides whether the project is owned or adopted.

**Owned project**:
A project whose source repo bitplane built and owns: always a URL, always a bare repo inside the project directory.
_Avoid_: URL project, URL-sourced project, remote project, cloned project

**Adopted project**:
A project whose source repo is the user's own checkout, pointed at in place; bitplane never moves, converts or deletes branches in it.
_Avoid_: Local project, local-path project

**Add**:
Registering an owned project from a URL by building a bare source repo for it.

**Adopt**:
Registering an existing local checkout as an adopted project, in place.

**Project name**:
A `/`-separated path of lowercase segments, unique per host and renameable, such as `acme/platform/codestyle`; one name never nests inside another.

**`@` sigil**:
How a project is referenced, as in `@acme/codestyle`; syntax only, never part of the name.

**Projects directory**:
The directory on a host holding one project directory per project; it is the registry of projects.
_Avoid_: Registry, store

**Project directory**:
`<projects-dir>/<project-name>/`, holding `project.toml` and, for an owned project, its source repo.

**Source repo**:
The local git repo a member's worktree is derived from: the bare repo of an owned project, or the user's checkout for an adopted one. Internal term.
_Avoid_: Bare mirror, mirror

**Plane branch**:
A branch in a source repo's `refs/heads/*`; for an owned project these are exactly the branches members were created on, past and present.

**Base branch**:
The commit a new plane branch is cut from, resolved per project and never stored.
_Avoid_: Default branch, main branch

**Occupied branch**:
A branch checked out in any worktree of a source repo, which a plane therefore cannot use.

**Script**:
A named command declared on a project, bound to zero or more script points and runnable on demand with `bp run`.
_Avoid_: Hook, action, task, command

**Script point**:
A moment in a worktree's lifecycle at which bound scripts fire: `post_worktree_create` or `pre_worktree_remove`.

## State and contract

**State**:
Everything bitplane knows, which is exactly what is on disk in the planes and projects directories.
_Avoid_: Store, database, index, cache

**Sentinel**:
The file a lock is taken on, never the file the lock protects.

**Incomplete marker**:
`.bitplane/incomplete` in a plane root, present exactly while a plane has been claimed but never finished being created.

**Abort window**:
The span of a create between claiming the plane and the last worktree landing, in which everything can be discarded losing nothing.

**Repair**:
Reconnecting a plane's worktrees to their source repos after a directory moved.

**Reap**:
Destroying a plane judged to be garbage, never over uncommitted or unpushed work.

**Per-source-repo lock**:
The mutex serialising every git command that writes to one source repo.

**`bp`**:
The command the bitplane CLI installs on `PATH`; `bitplane` is the crate, repo and package name.
_Avoid_: bitplane (when meaning the command)

**Engine**:
The request/response interface every mutation passes through; the machine contract the CLI renders.

**Error envelope**:
The five fields every failure is reported as (`error`, `code`, `message`, `problems`, `remedy`); a structure with a human and a JSON rendering.
_Avoid_: The JSON error, the error JSON

**Waiver**:
Per-reason, per-invocation consent to lose a specific piece of work the user is looking at.

**Refusal**:
An operation stopped because no informed consent is possible; it cannot be waived.

**Unpushed**:
A branch tip not contained in any `refs/remotes/origin/*`.
_Avoid_: Unmerged, ahead of upstream

**Prunable**:
Git's word for a worktree whose directory is gone; never said of a branch.

## Command center

**Tab**:
One entry in a plane's tab bar in cc: a session tab or a file tab.
_Avoid_: Session (when meaning the UI element)

**Session tab**:
The UI handle of one session; there is always exactly one session tab per session.
_Avoid_: Terminal tab

**File tab**:
A tab showing one file of a plane, as a diff or its plain contents; it exists only in the UI, is never restored, and opens as the plane's single preview until pinned.
_Avoid_: Editor, diff tab, file view

**Gone plane**:
A plane that disappeared or was renamed while it still has open sessions; its sessions keep running.
_Avoid_: Dead plane, orphan

**Draft plane**:
A folder in the planes directory that has no plane file yet; cc shows it and opens sessions in it like a plane, until `bp create --here` turns it into one.
_Avoid_: Empty plane, pending plane

**Busy session**:
A session whose terminal is running something other than its idle shell, such as Claude Code, an editor or a build; closing it asks first.
_Avoid_: Active session, running session

**Exited session**:
A session whose shell ended with an error or never started; its tab stays open showing why, and can be restarted.
_Avoid_: Dead session, crashed tab

**Restore**:
Reopening, on launch, the tabs that were open when cc last stopped, each with a fresh shell in its plane and its agent session resumed.
_Avoid_: Reattach (a reload reattaches to running sessions), persist

**Sidebar**:
The resizable plane list on the left edge of cc, where planes and their tabs are picked.
_Avoid_: Plane list, navigator

**Tool bar**:
The vertical strip of icons on the right edge of cc; it switches the tool panel between tools and holds plane actions.
_Avoid_: Sidebar (that is the plane list on the left), activity bar

**Tool**:
One view of the active plane that the tool bar can show in the tool panel, such as Source control or Files.
_Avoid_: Panel, view, extension

**Tool panel**:
The resizable area next to the tool bar that shows the active tool; collapsed when no tool is active.
_Avoid_: Right sidebar, drawer
