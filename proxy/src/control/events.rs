//! Each app's recent events, newest first (`GET /v1/apps/<name>/events`): what the API did (deployed,
//! scaled, destroyed), what the reconciler decided (a machine placed on a worker), and what changed in the
//! facts it reads every round (a machine's image and state, its checks, its worker's heartbeat), found by
//! comparing a round's facts with the last round's (`changes`).
// shortcut: in memory only, a ring of RING per app: gone at a restart, and the first round after one is a
// baseline (no events for what it finds). Fact changes are seen once per round (every 5 s, or on an API
// write): their time is the round's, and a state that came and went between two rounds isn't seen.

use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

use serde::Serialize;

use super::{
    model::{Check, Machine, Worker},
    ALIVE_MS,
};
use crate::states::{Fact, Health};

/// Events kept per app.
pub(crate) const RING: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Event {
    pub(crate) at_ms: i64,
    /// The machine it's about; "" for the app itself.
    pub(crate) machine: String,
    pub(crate) text: String,
}

/// What a round saw, for the next one to compare with.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Seen {
    /// Machine id -> its app, worker, fact and error.
    machines: HashMap<String, (String, String, Option<Fact>, String)>,
    /// (machine, service) -> its check's state, from the machine's own worker's row.
    checks: HashMap<(String, String), Option<Health>>,
    /// Worker -> whether its heartbeat is fresh.
    alive: HashMap<String, bool>,
}

impl Seen {
    pub(crate) fn of(machines: &[Machine], checks: &[Check], workers: &[Worker], now: i64) -> Seen {
        Seen {
            machines: machines.iter().map(|m| (m.id.clone(), (m.app.clone(), m.worker.clone(), m.state, m.error.clone()))).collect(),
            checks: checks
                .iter()
                .filter(|c| machines.iter().any(|m| m.id == c.machine && m.worker == c.worker))
                .map(|c| ((c.machine.clone(), c.service.clone()), c.state))
                .collect(),
            alive: workers.iter().map(|w| (w.name.clone(), now - w.heartbeat_at < ALIVE_MS)).collect(),
        }
    }
}

/// What a machine's new fact says, after `before`.
fn fact_text(before: Option<Fact>, now: Fact, error: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Its image was being built, and the machine moved on: built (unless it failed there).
    if before == Some(Fact::Pulling) && !matches!(now, Fact::Pulling | Fact::Failed) {
        out.push("image built".to_owned());
    }
    match now {
        Fact::Pulling => out.push("image building".to_owned()),
        Fact::Creating => {}
        Fact::Starting => out.push("starting".to_owned()),
        Fact::Started => out.push("started".to_owned()),
        Fact::Stopping => out.push("stopping".to_owned()),
        Fact::Stopped => out.push("stopped".to_owned()),
        Fact::Failed if error.is_empty() => out.push("failed".to_owned()),
        Fact::Failed => out.push(format!("failed: {error}")),
        Fact::Destroyed => out.push("destroyed".to_owned()),
    }
    out
}

/// The events between two rounds: (app, machine, text). Nothing for the first round (no `prev`).
pub(crate) fn changes(prev: Option<&Seen>, now: &Seen) -> Vec<(String, String, String)> {
    let Some(prev) = prev else { return vec![] };
    let mut out = Vec::new();
    let mut ids: Vec<&String> = now.machines.keys().collect();
    ids.sort();
    for id in &ids {
        let (app, _, fact, error) = &now.machines[*id];
        let before = prev.machines.get(*id).and_then(|(_, _, f, _)| *f);
        if let Some(f) = fact.filter(|f| Some(*f) != before) {
            out.extend(fact_text(before, f, error).into_iter().map(|t| (app.clone(), (*id).clone(), t)));
        }
    }
    let mut keys: Vec<&(String, String)> = now.checks.keys().collect();
    keys.sort();
    for key in keys {
        let (state, before) = (now.checks[key], prev.checks.get(key).copied().flatten());
        let Some(app) = now.machines.get(&key.0).map(|(a, ..)| a) else { continue };
        let text = match state {
            Some(Health::Passing) if before != Some(Health::Passing) => format!("check {} passing", key.1),
            Some(s @ (Health::Failing | Health::Down)) if before.is_none_or(|b| b == Health::Passing) => format!("check {} {s}", key.1),
            _ => continue,
        };
        out.push((app.clone(), key.0.clone(), text));
    }
    // A worker going silent or coming back: for each app with a live machine there.
    let mut workers: Vec<(&String, &bool)> = now.alive.iter().collect();
    workers.sort();
    for (w, &alive) in workers {
        if prev.alive.get(w).is_none_or(|&was| was == alive) {
            continue;
        }
        let mut on: Vec<(&String, &String)> =
            now.machines.iter().filter(|(_, (_, worker, f, _))| worker == w && *f != Some(Fact::Destroyed)).map(|(id, (app, ..))| (app, id)).collect();
        on.sort();
        let text = if alive { format!("{w} back (heartbeat)") } else { format!("{w} silent (no heartbeat)") };
        out.extend(on.into_iter().map(|(app, id)| (app.clone(), id.clone(), text.clone())));
    }
    out
}

#[derive(Default)]
pub(crate) struct Events {
    by_app: Mutex<HashMap<String, VecDeque<Event>>>,
    last: Mutex<Option<Seen>>,
}

impl Events {
    pub(crate) fn add(&self, app: &str, machine: &str, text: impl Into<String>, at_ms: i64) {
        let mut by_app = self.by_app.lock().unwrap();
        let ring = by_app.entry(app.to_owned()).or_default();
        ring.push_front(Event { at_ms, machine: machine.to_owned(), text: text.into() });
        ring.truncate(RING);
    }

    /// Newest first.
    pub(crate) fn list(&self, app: &str) -> Vec<Event> {
        self.by_app.lock().unwrap().get(app).map(|r| r.iter().cloned().collect()).unwrap_or_default()
    }

    /// A round's facts: the events since the last round's.
    pub(crate) fn observe(&self, seen: Seen, now: i64) {
        let found = {
            let mut last = self.last.lock().unwrap();
            let found = changes(last.as_ref(), &seen);
            *last = Some(seen);
            found
        };
        for (app, machine, text) in found {
            self.add(&app, &machine, text, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::states::Desired;

    fn m(id: &str, worker: &str, state: Option<Fact>, error: &str) -> Machine {
        Machine {
            id: id.into(),
            worker: worker.into(),
            app: "web".into(),
            managed: true,
            desired: Desired::Started,
            state,
            addr: String::new(),
            error: error.into(),
            cpus: 1,
            memory: 128,
        }
    }

    fn w(name: &str, heartbeat_at: i64) -> Worker {
        Worker {
            name: name.into(),
            backhaul: String::new(),
            agent_api: String::new(),
            started_at: 0,
            heartbeat_at,
            arch: String::new(),
            cpus: 0,
            memory: 0,
            machines: 0,
            allocated_cpus: 0,
            allocated_memory: 0,
        }
    }

    fn c(machine: &str, worker: &str, state: Health) -> Check {
        Check { machine: machine.into(), worker: worker.into(), service: "http".into(), state: Some(state) }
    }

    fn texts(v: Vec<(String, String, String)>) -> Vec<String> {
        v.into_iter().map(|(_, m, t)| format!("{m}: {t}")).collect()
    }

    #[test]
    fn a_machines_life_its_checks_and_its_worker() {
        let now = 100_000;
        let workers = [w("worker1", now)];
        let seen = |ms: &[Machine], cs: &[Check], ws: &[Worker]| Seen::of(ms, cs, ws, now);
        // The first round is a baseline.
        let s0 = seen(&[m("web-1", "worker1", None, "")], &[], &workers);
        assert!(changes(None, &s0).is_empty());
        let s1 = seen(&[m("web-1", "worker1", Some(Fact::Pulling), "")], &[], &workers);
        assert_eq!(texts(changes(Some(&s0), &s1)), ["web-1: image building"]);
        // Built and started between two rounds: both said.
        let s2 = seen(&[m("web-1", "worker1", Some(Fact::Started), "")], &[c("web-1", "worker1", Health::Passing)], &workers);
        assert_eq!(texts(changes(Some(&s1), &s2)), ["web-1: image built", "web-1: started", "web-1: check http passing"]);
        // Nothing new: nothing said.
        assert!(changes(Some(&s2), &s2).is_empty());
        // The check fails, then passes again.
        let s3 = seen(&[m("web-1", "worker1", Some(Fact::Started), "")], &[c("web-1", "worker1", Health::Failing)], &workers);
        assert_eq!(texts(changes(Some(&s2), &s3)), ["web-1: check http failing"]);
        assert_eq!(texts(changes(Some(&s3), &s2)), ["web-1: check http passing"]);
        // Another worker's row for this machine doesn't count.
        let s4 = seen(&[m("web-1", "worker1", Some(Fact::Started), "")], &[c("web-1", "worker2", Health::Failing)], &workers);
        assert!(changes(Some(&s2), &s4).is_empty());
        // Its worker goes silent, then comes back.
        let silent = seen(&[m("web-1", "worker1", Some(Fact::Started), "")], &[c("web-1", "worker1", Health::Passing)], &[w("worker1", now - ALIVE_MS)]);
        assert_eq!(texts(changes(Some(&s2), &silent)), ["web-1: worker1 silent (no heartbeat)"]);
        assert_eq!(texts(changes(Some(&silent), &s2)), ["web-1: worker1 back (heartbeat)"]);
        // A failure says why; a stop, a destroy.
        let failed = seen(&[m("web-1", "worker1", Some(Fact::Failed), "image: 404")], &[], &workers);
        assert_eq!(texts(changes(Some(&s1), &failed)), ["web-1: failed: image: 404"]);
        let stopped = seen(&[m("web-1", "worker1", Some(Fact::Stopped), "")], &[], &workers);
        assert_eq!(texts(changes(Some(&s2), &stopped)), ["web-1: stopped"]);
    }

    #[test]
    fn a_ring_per_app_newest_first() {
        let e = Events::default();
        for i in 0..RING + 5 {
            e.add("web", "", format!("e{i}"), i as i64);
        }
        e.add("other", "m", "x", 0);
        let list = e.list("web");
        assert_eq!(list.len(), RING);
        assert_eq!(list[0].text, format!("e{}", RING + 4));
        assert_eq!(list[RING - 1].text, "e5");
        assert_eq!(e.list("other").len(), 1);
        assert!(e.list("none").is_empty());
    }
}
