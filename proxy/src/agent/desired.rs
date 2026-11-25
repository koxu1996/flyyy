//! What the control plane told this agent: one record per machine (its spec and desired state), kept in
//! `<state-dir>/desired/<id>.json` so the agent runs it after a restart, with the control plane down too.
//! Written only through the agent API (agent/api.rs); a destroyed machine keeps its record, so a repeated
//! destroy is a no-op and the fact can say destroyed.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::spec::{name_ok, spec_ok, Spec};
use crate::{states::Desired, util::write_atomic};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) spec: Spec,
    pub(crate) desired: Desired,
}

/// Why a request was refused: the API's status follows the kind.
#[derive(Debug, PartialEq)]
pub(crate) enum Refused {
    /// The spec or id isn't valid.
    Bad(String),
    /// No machine of that id here.
    Missing,
    /// It exists with another spec (specs are immutable: a new one is a new machine), or it's destroyed.
    Conflict(String),
}

pub(crate) struct Store {
    dir: String,
    records: Mutex<BTreeMap<String, Record>>,
    /// The agent's loop, woken on every change.
    changed: Arc<Notify>,
}

impl Store {
    /// Every record in `dir`; one that doesn't parse is an error (it would be a machine the agent forgot).
    pub(crate) fn open(dir: &str, changed: Arc<Notify>) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        let mut records = BTreeMap::new();
        for e in std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))? {
            let path = e.map_err(|e| format!("{dir}: {e}"))?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let r: Record = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
            records.insert(r.spec.id.clone(), r);
        }
        Ok(Store { dir: dir.into(), records: Mutex::new(records), changed })
    }

    pub(crate) fn all(&self) -> BTreeMap<String, Record> {
        self.records.lock().unwrap().clone()
    }

    fn save(&self, r: &Record) -> Result<(), String> {
        write_atomic(&format!("{}/{}.json", self.dir, r.spec.id), &serde_json::to_vec(r).expect("serialisable"))
    }

    /// Put a machine's spec: a new one starts out stopped; the same spec again changes nothing (true: it was
    /// new or changed).
    pub(crate) fn put(&self, id: &str, spec: Spec) -> Result<bool, Refused> {
        if spec.id != id {
            return Err(Refused::Bad(format!("spec id {:?} isn't {id:?}", spec.id)));
        }
        spec_ok(&spec).map_err(Refused::Bad)?;
        let mut records = self.records.lock().unwrap();
        match records.get(id) {
            Some(r) if r.spec == spec => Ok(false),
            Some(r) if r.desired == Desired::Destroyed => Err(Refused::Conflict(format!("{id} is destroyed"))),
            Some(_) => Err(Refused::Conflict(format!("{id} exists with another spec (a new spec is a new machine)"))),
            None => {
                let r = Record { spec, desired: Desired::Stopped };
                self.save(&r).map_err(Refused::Bad)?;
                records.insert(id.into(), r);
                self.changed.notify_one();
                Ok(true)
            }
        }
    }

    /// Start, stop or destroy a machine; the state it's in already changes nothing. Destroyed is final.
    pub(crate) fn set(&self, id: &str, want: Desired) -> Result<bool, Refused> {
        if !name_ok(id) {
            return Err(Refused::Bad(format!("bad id {id:?}")));
        }
        let mut records = self.records.lock().unwrap();
        let Some(r) = records.get_mut(id) else { return Err(Refused::Missing) };
        if r.desired == want {
            return Ok(false);
        }
        if r.desired == Desired::Destroyed {
            return Err(Refused::Conflict(format!("{id} is destroyed")));
        }
        let mut next = r.clone();
        next.desired = want;
        self.save(&next).map_err(Refused::Bad)?;
        *r = next;
        self.changed.notify_one();
        Ok(true)
    }
}
