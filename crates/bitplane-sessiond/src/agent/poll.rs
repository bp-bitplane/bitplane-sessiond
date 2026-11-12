//! The registry poll (ADR 0030): while any agent works, or runs without
//! hooks, the agent's own registry is read and wins over the hooks; while
//! any agent is live at all, it is read now and then to see `/bg`.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Instant;

use bitplane_protocol::agent::{Agent, AgentState, Tracking};
use bitplane_protocol::session::SessionId;
use tokio::sync::Notify;

use super::{LiveAgent, Processes, Shared, State, process};

/// Probes whenever [`Shared::next_probe`] says so, until the module is gone.
pub(super) async fn run(shared: Weak<Shared>, wake: Arc<Notify>) {
    loop {
        let Some(due) = shared.upgrade().map(|shared| shared.next_probe()) else {
            return;
        };
        match due {
            Some(due) => {
                tokio::select! {
                    () = tokio::time::sleep_until(due.into()) => {}
                    () = wake.notified() => continue,
                }
            }
            None => {
                wake.notified().await;
                continue;
            }
        }
        let Some((adapter, env, processes)) = shared.upgrade().map(|shared| {
            let mut state = shared.state.lock().unwrap();
            state.last_probe = Some(Instant::now());
            (
                shared.adapter.clone(),
                shared.env.clone(),
                state.processes.clone(),
            )
        }) else {
            return;
        };
        let snapshot = Instant::now();
        let probed = tokio::task::spawn_blocking(move || {
            adapter
                .probe(&env)
                .map(|live| (join(&live, &processes), live))
        })
        .await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match probed {
            Ok(Some((joined, live))) => shared.apply(&live, &joined, snapshot),
            Ok(None) => tracing::debug!("couldn't read the agent registry"),
            Err(error) => tracing::warn!(%error, "the agent registry probe failed"),
        }
    }
}

/// Each session's interactive registry entries, by index into the probe.
type Joined = HashMap<SessionId, Vec<usize>>;

impl Shared {
    /// When the registry is next read: one poll interval after the last
    /// read while any agent is busy, waiting or tracked by poll, one live
    /// interval after it while any agent is live, and when a candidate
    /// falls due; `None` with no live agent and no candidate.
    fn next_probe(&self) -> Option<Instant> {
        let state = self.state.lock().unwrap();
        let after = |interval| {
            state
                .last_probe
                .map_or_else(Instant::now, |last| last + interval)
        };
        let live = state.agents.iter().filter(|tracked| {
            tracked.agent.session.is_some() && tracked.agent.state != AgentState::Exited
        });
        let working = live.clone().any(|tracked| {
            matches!(
                tracked.agent.state,
                AgentState::Busy | AgentState::Waiting { .. }
            ) || tracked.agent.tracking == Tracking::Poll
        });
        let polled = if working {
            Some(after(self.timing.poll))
        } else if live.clone().next().is_some() {
            Some(after(self.timing.live))
        } else {
            None
        };
        let candidate = state
            .candidates
            .values()
            .map(|candidate| candidate.due.max(after(self.timing.poll)))
            .min();
        match (polled, candidate) {
            (Some(polled), Some(candidate)) => Some(polled.min(candidate)),
            (polled, candidate) => polled.or(candidate),
        }
    }

    /// Applies a registry read that started at `snapshot`. A hook handled
    /// since is newer than the read, so it keeps its session's state.
    fn apply(&self, live: &[LiveAgent], joined: &Joined, snapshot: Instant) {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        let none = Vec::new();
        let mut restarted = Vec::new();
        let mut backgrounded = Vec::new();
        for tracked in state.agents.iter_mut() {
            let Some(session) = tracked.agent.session.clone() else {
                continue;
            };
            if tracked.agent.state == AgentState::Exited {
                continue;
            }
            let id = tracked.agent.agent_session.clone();
            let entries: Vec<&LiveAgent> = joined
                .get(&session)
                .unwrap_or(&none)
                .iter()
                .map(|&index| &live[index])
                .collect();
            if live
                .iter()
                .any(|entry| entry.background && entry.agent_session.as_ref() == Some(&id))
            {
                self.change(tracked, |agent| agent.state = AgentState::Exited);
                backgrounded.push((session, id));
                continue;
            }
            let child = state.processes.get(&session).and_then(|p| p.shell);
            let vanished = tracked
                .pid
                .is_some_and(|pid| !child.is_some_and(|child| process::descends_from(pid, child)));
            if vanished {
                self.change(tracked, |agent| agent.state = AgentState::Exited);
                continue;
            }
            if state.candidates.contains_key(&session) {
                continue;
            }
            let entry = entries
                .iter()
                .find(|entry| entry.agent_session.as_ref() == Some(&id))
                .or_else(|| {
                    let pid = tracked.pid?;
                    entries.iter().find(|entry| entry.pid == pid)
                });
            let Some(entry) = entry else {
                continue;
            };
            tracked.pid = Some(entry.pid);
            if tracked.agent.tracking == Tracking::Poll
                && entry
                    .agent_session
                    .as_ref()
                    .is_some_and(|other| *other != id)
            {
                restarted.push((session, (*entry).clone()));
                continue;
            }
            let fresh_hook = state
                .hooked
                .get(&session)
                .is_some_and(|hooked| *hooked >= snapshot);
            self.change(tracked, |agent| {
                if entry.name.is_some() {
                    agent.name.clone_from(&entry.name);
                }
                if let Some(registry) = entry.state.clone().filter(|_| !fresh_hook) {
                    agent.state = registry;
                }
            });
        }
        for (session, entry) in restarted {
            self.adopt(state, session, &entry);
        }
        // `/bg` took the agent out of the terminal; it is not resumed there (ADR 0031).
        for (session, id) in backgrounded {
            let linked = state.links.get(&session).cloned().flatten();
            if linked.is_some_and(|link| link.agent_session == id) {
                self.link(state, &session, None);
            }
        }
        self.check_candidates(state, live, joined, snapshot);
    }

    /// Lists the registry's agent in each due candidate session with
    /// `tracking: poll`, a resumed one included. The candidate stays until
    /// then: the session's PTY child is the agent, and its exit drops it.
    fn check_candidates(
        &self,
        state: &mut State,
        live: &[LiveAgent],
        joined: &Joined,
        snapshot: Instant,
    ) {
        let due: Vec<SessionId> = state
            .candidates
            .iter()
            .filter(|(_, candidate)| candidate.due <= snapshot)
            .map(|(session, _)| session.clone())
            .collect();
        for session in due {
            if state
                .hooked
                .get(&session)
                .is_some_and(|hooked| *hooked >= snapshot)
            {
                continue;
            }
            let processes = state.processes.get(&session).cloned().unwrap_or_default();
            let entries: Vec<&LiveAgent> = joined
                .get(&session)
                .into_iter()
                .flatten()
                .map(|&index| &live[index])
                .collect();
            let entry = entries
                .iter()
                .find(|entry| Some(entry.pid) == processes.group)
                .or(entries.first());
            if let Some(entry) = entry.filter(|entry| entry.agent_session.is_some()) {
                let entry = (*entry).clone();
                self.adopt(state, session, &entry);
            }
        }
    }

    /// `entry` becomes its session's agent, tracked by the registry alone.
    fn adopt(&self, state: &mut State, session: SessionId, entry: &LiveAgent) {
        let Some(agent_session) = entry.agent_session.clone() else {
            return;
        };
        state.candidates.remove(&session);
        let agent = Agent {
            kind: self.adapter.kind().into(),
            agent_session,
            session: Some(session),
            cwd: entry.cwd.clone(),
            name: entry.name.clone(),
            state: entry.state.clone().unwrap_or(AgentState::Idle),
            last_error: None,
            tracking: Tracking::Poll,
        };
        self.start_agent(state, agent, Some(entry.pid));
    }
}

/// Interactive entries to the agent terminal whose PTY child they run under.
fn join(live: &[LiveAgent], processes: &HashMap<SessionId, Processes>) -> Joined {
    let mut joined = Joined::new();
    for (index, entry) in live.iter().enumerate() {
        if entry.background {
            continue;
        }
        let session = processes.iter().find(|(_, processes)| {
            processes
                .shell
                .is_some_and(|shell| process::descends_from(entry.pid, shell))
        });
        if let Some((session, _)) = session {
            joined.entry(session.clone()).or_default().push(index);
        }
    }
    joined
}
