//! The control plane's desired state, in its own store rather than in gossip: apps, machines (each with
//! its spec, env included), orgs, users (token hashes) and IP leases. One JSON file, read at start and
//! rewritten atomically on every change, so a kill -9 at any point loses at most the change in flight.
// shortcut: one file rewritten whole, fine for a few thousand records; SQLite when it grows.

use std::{collections::BTreeMap, net::Ipv4Addr, sync::Mutex};

use serde::{Deserialize, Serialize};

use super::model::App;
use crate::{
    agent::spec::Spec,
    ingress::Lease,
    states::{Desired, Lifecycle, Role},
    util::write_atomic,
};

/// A machine the control plane placed: where, its spec, and what it should be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MachineRec {
    pub(crate) worker: String,
    pub(crate) spec: Spec,
    pub(crate) desired: Desired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OrgRec {
    pub(crate) state: Lifecycle,
    pub(crate) created_at: i64,
}

/// A user. The token itself is never kept, only its SHA-256 ("" once its org is destroyed: no token
/// hashes to that).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UserRec {
    pub(crate) org: String,
    pub(crate) role: Role,
    pub(crate) token_sha256: String,
    pub(crate) created_at: i64,
}

/// Everything in the store, keyed by name, id or address.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Records {
    pub(crate) apps: BTreeMap<String, App>,
    pub(crate) machines: BTreeMap<String, MachineRec>,
    pub(crate) orgs: BTreeMap<String, OrgRec>,
    pub(crate) users: BTreeMap<String, UserRec>,
    pub(crate) leases: BTreeMap<Ipv4Addr, Lease>,
}

pub(crate) struct Db {
    path: String,
    records: Mutex<Records>,
}

impl Db {
    /// The store at `path`, or an empty one if there's no file yet.
    pub(crate) fn open(path: &str) -> Result<Self, String> {
        let records = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{path}: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Records::default(),
            Err(e) => return Err(format!("{path}: {e}")),
        };
        Ok(Db { path: path.into(), records: Mutex::new(records) })
    }

    pub(crate) fn read<T>(&self, f: impl FnOnce(&Records) -> T) -> T {
        f(&self.records.lock().unwrap())
    }

    /// Change the records and write them out, all or nothing: `f` works on a copy, which replaces them
    /// only once it's on disk. `f`'s own error (the inner one) leaves everything as it was; the outer one
    /// is the write's.
    pub(crate) fn write<T, E>(&self, f: impl FnOnce(&mut Records) -> Result<T, E>) -> Result<Result<T, E>, String> {
        let mut records = self.records.lock().unwrap();
        let mut next = records.clone();
        let out = match f(&mut next) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        write_atomic(&self.path, &serde_json::to_vec(&next).expect("serialisable"))?;
        *records = next;
        Ok(Ok(out))
    }
}
