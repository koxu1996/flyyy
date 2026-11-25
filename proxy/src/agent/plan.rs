//! What to do per machine (create, start, stop, destroy) from desired and actual state, and the fact
//! reported for it.

use std::collections::{BTreeMap, HashSet};

use super::Local;
use crate::states::{Desired, Fact};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Actual {
    /// A local state file exists: IP and tap are allocated, the image is ready.
    pub(crate) local: bool,
    /// `fc-<id>` is active (or activating/deactivating).
    pub(crate) running: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Create,
    Start,
    Stop,
    Destroy,
}

/// The reconcile diff. Ids in backoff skip
/// Create/Start but may still be stopped or destroyed; ids whose image isn't ready (`pulling`)
/// skip Create (and so their Start). Anything here but not desired is an orphan.
pub(crate) fn plan(
    desired: &BTreeMap<String, Desired>,
    actual: &BTreeMap<String, Actual>,
    backoff: &HashSet<String>,
    pulling: &HashSet<String>,
) -> Vec<(String, Action)> {
    let mut out = Vec::new();
    for (id, want) in desired {
        let a = actual.get(id).copied().unwrap_or_default();
        let wait = backoff.contains(id) || (pulling.contains(id) && !a.local);
        let actions: &[Action] = match want {
            Desired::Started if !a.local => &[Action::Create, Action::Start],
            Desired::Started if !a.running => &[Action::Start],
            Desired::Stopped if !a.local => &[Action::Create],
            Desired::Stopped if a.running => &[Action::Stop],
            Desired::Destroyed if a.local || a.running => &[Action::Destroy],
            _ => &[],
        };
        for &act in actions {
            if !(wait && matches!(act, Action::Create | Action::Start)) {
                out.push((id.clone(), act));
            }
        }
    }
    for (id, a) in actual {
        if !desired.contains_key(id) && (a.local || a.running) {
            out.push((id.clone(), Action::Destroy));
        }
    }
    out
}

/// What the agent reports of a machine (a `machine_status` row): its state, where it answers, and why it
/// failed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Report {
    pub(crate) state: Fact,
    pub(crate) addr: String,
    pub(crate) error: String,
}

impl Report {
    pub(crate) fn failed(error: impl Into<String>) -> Self {
        Report { state: Fact::Failed, addr: String::new(), error: error.into() }
    }
}

pub(crate) fn fact(want: Desired, local: Option<&Local>, running: bool, ready: bool, error: Option<&str>) -> Report {
    let addr = local.map_or(String::new(), |l| format!("vsock:{}:{}", l.spec.id, l.port));
    let state = match (local.is_some(), running, ready) {
        (false, false, _) if want == Desired::Destroyed => Fact::Destroyed,
        (_, true, true) => Fact::Started,
        (_, true, false) => Fact::Starting,
        _ if error.is_some() => Fact::Failed,
        (false, false, _) => Fact::Creating,
        (true, false, _) if want == Desired::Started => Fact::Starting,
        (true, false, _) => Fact::Stopped,
    };
    let addr = if state == Fact::Destroyed { String::new() } else { addr };
    Report { state, addr, error: error.unwrap_or_default().into() }
}
