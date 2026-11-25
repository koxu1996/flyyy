//! The per-app decision of the reconciler (which machines to create or destroy) and when an app is converged.

use std::collections::BTreeMap;

use super::model::{App, Machine};
use crate::{
    control::model::Check,
    states::{Fact, Lifecycle},
};

#[derive(Debug, PartialEq)]
pub(crate) enum Op {
    Create { worker: String },
    Destroy { id: String },
}

/// The per-app decision. `machines` is every machine (any app, managed or not), so placement can
/// balance load; only this app's managed machines are created or destroyed. `workers` are the live
/// ones: new machines only go there, but machines on dead workers still count (no rescheduling).
/// Create: the worker with the fewest of this app's machines, then the fewest machines overall, then by name.
/// Destroy (scale down): not-yet-started first, then the worker with most of this app's, then most overall.
// shortcut: placement is spread only, no capacity; dead workers' machines aren't rescheduled.
pub(crate) fn decide(app: &App, machines: &[Machine], workers: &[String]) -> Vec<Op> {
    let mine: Vec<&Machine> = machines.iter().filter(|m| m.managed && m.app == app.name && m.live()).collect();
    if app.state != Lifecycle::Active {
        return mine.iter().map(|m| Op::Destroy { id: m.id.clone() }).collect();
    }
    let mut load: BTreeMap<&str, (usize, usize)> = workers.iter().map(|w| (w.as_str(), (0, 0))).collect();
    for m in machines.iter().filter(|m| m.live()) {
        if let Some(l) = load.get_mut(m.worker.as_str()) {
            l.1 += 1;
            if m.managed && m.app == app.name {
                l.0 += 1;
            }
        }
    }
    let want = app.count as usize;
    let mut ops = Vec::new();
    if mine.len() < want {
        for _ in mine.len()..want {
            let Some((&w, l)) = load.iter_mut().min_by_key(|(w, l)| (**l, **w)) else { break };
            l.0 += 1;
            l.1 += 1;
            ops.push(Op::Create { worker: w.to_owned() });
        }
    } else {
        let mut left = mine;
        for _ in want..left.len() {
            let key = |m: &&Machine| {
                let l = load.get(m.worker.as_str()).copied().unwrap_or_default();
                (m.state != Some(Fact::Started), l, m.id.clone())
            };
            let Some(i) = (0..left.len()).max_by_key(|&i| key(&left[i])) else { break };
            let m = left.swap_remove(i);
            if let Some(l) = load.get_mut(m.worker.as_str()) {
                l.0 -= 1;
                l.1 -= 1;
            }
            ops.push(Op::Destroy { id: m.id.clone() });
        }
    }
    ops
}

/// Converged: an active app has exactly `count` started machines whose services all pass their checks
/// (so it's routed), and every destroyed one is gone; a destroyed app has none left.
pub(crate) fn converged(app: &App, mine: &[&Machine], checks: &[Check]) -> bool {
    let gone = mine.iter().filter(|m| !m.live()).all(|m| m.state == Some(Fact::Destroyed));
    let live: Vec<_> = mine.iter().filter(|m| m.live()).collect();
    let passing = |m: &Machine| app.services.iter().all(|s| checks.iter().any(|c| c.passing(m, &s.name)));
    match app.state {
        Lifecycle::Active => gone && live.len() == app.count as usize && live.iter().all(|m| m.state == Some(Fact::Started) && passing(m)),
        Lifecycle::Destroyed => gone && live.is_empty(),
    }
}
