//! The control plane's records (apps; machines with their facts) and the facts it reads from Corrosion
//! (workers, images, checks), and what's computed from them without I/O: service passing counts, image
//! sizes, live workers.

use std::{collections::BTreeMap, net::SocketAddr};

use serde::{Deserialize, Serialize};

use super::ALIVE_MS;
use crate::{
    agent::spec::name_ok,
    corro::maybe,
    services::Service,
    states::{Desired, Fact, Health, Lifecycle, Role},
    wg,
};

/// An app, as the control plane keeps it (db.rs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct App {
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) port: u16,
    /// Replaces the image's Cmd when non-empty; `{id}` becomes each machine's id.
    pub(crate) cmd: Vec<String>,
    /// Overrides the image's Env; `{id}` in values becomes each machine's id. Kept here and sent to the
    /// machine's worker only (agent API), never gossiped; the API shows names only.
    // shortcut: kept in plaintext in the control plane's store, and on the worker in the machine's record.
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) count: u32,
    pub(crate) state: Lifecycle,
    /// Each machine's vCPUs and memory (MiB).
    pub(crate) cpus: u32,
    pub(crate) memory: u32,
    /// Never empty.
    pub(crate) services: Vec<Service>,
    /// The owning org; "" for the operator's own.
    pub(crate) org: String,
}

/// A machine the control plane placed, with its worker's `machine_status` fact.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Machine {
    pub(crate) id: String,
    pub(crate) worker: String,
    #[serde(skip_serializing)]
    pub(crate) app: String,
    /// The reconciler's own (all the control plane's machines are; the tests' unmanaged ones live on the
    /// workers only).
    #[serde(skip_serializing)]
    pub(crate) managed: bool,
    pub(crate) desired: Desired,
    /// What the machine agent reported: None before its first report (and in the API's JSON, "").
    #[serde(serialize_with = "fact_or_empty")]
    pub(crate) state: Option<Fact>,
    pub(crate) addr: String,
    pub(crate) error: String,
    /// Its size, for its worker's allocation.
    #[serde(skip_serializing)]
    pub(crate) cpus: u32,
    #[serde(skip_serializing)]
    pub(crate) memory: u32,
}

/// An org, for the API.
#[derive(Debug)]
pub(crate) struct Org {
    pub(crate) name: String,
    pub(crate) state: Lifecycle,
}

/// A user, for the API: never its token hash.
#[derive(Debug)]
pub(crate) struct User {
    pub(crate) name: String,
    pub(crate) org: String,
    pub(crate) role: Role,
}

/// A `service_status` row's health: a machine's service and its state. The agents write these: a state
/// that isn't one of Health's is None (so not passing).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Check {
    pub(crate) machine: String,
    /// Who wrote it: only a machine's own worker's row counts for it.
    pub(crate) worker: String,
    pub(crate) service: String,
    #[serde(deserialize_with = "maybe")]
    pub(crate) state: Option<Health>,
}

impl Check {
    pub(crate) fn passing(&self, m: &Machine, service: &str) -> bool {
        self.machine == m.id && self.worker == m.worker && self.service == service && self.state == Some(Health::Passing)
    }
}

/// A machine's fact for the API: "" before its first report.
fn fact_or_empty<S: serde::Serializer>(fact: &Option<Fact>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&fact.map_or(String::new(), |f| f.to_string()))
}

impl Machine {
    pub(crate) fn live(&self) -> bool {
        self.desired != Desired::Destroyed
    }
}

/// A `workers` row, as announced by that worker's machine agent.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Worker {
    pub(crate) name: String,
    pub(crate) backhaul: String,
    /// Where its agent API listens: used only once bound to its mesh identity (Control::agent_addr).
    pub(crate) agent_api: String,
    pub(crate) started_at: i64,
    pub(crate) heartbeat_at: i64,
    /// Its capacity, as it announced it: "" and 0s before it reported them.
    pub(crate) arch: String,
    pub(crate) cpus: i64,
    pub(crate) memory: i64,
    /// What it holds that isn't destroyed (the control plane's machines or not): how many, and their
    /// CPUs and memory summed. Overcommit allowed: placement ignores capacity.
    pub(crate) machines: i64,
    pub(crate) allocated_cpus: i64,
    pub(crate) allocated_memory: i64,
}

/// An `images` row: a worker's finished image of a digest (`sha256:<hex>`) and its size.
#[derive(Deserialize)]
pub(crate) struct Image {
    pub(crate) worker: String,
    pub(crate) digest: String,
    pub(crate) bytes: u64,
}

/// An app's services as the API shows them: each with how many of its live machines pass the check.
pub(crate) fn services_view(app: &App, mine: &[&Machine], checks: &[Check]) -> Vec<serde_json::Value> {
    app.services
        .iter()
        .map(|s| {
            let passing = mine.iter().filter(|m| m.live() && checks.iter().any(|c| c.passing(m, &s.name))).count();
            let mut v = serde_json::to_value(s).expect("serialisable");
            v["passing"] = passing.into();
            v
        })
        .collect()
}

/// An app's image size: the largest any worker reported for its digest (each builds its own copy), None
/// before one has finished it.
pub(crate) fn app_image_bytes(app: &App, images: &[Image]) -> Option<u64> {
    let digest = app.image.rsplit_once('@').map(|(_, d)| d)?;
    images.iter().filter(|i| i.digest == digest && i.bytes > 0).map(|i| i.bytes).max()
}

/// A worker's images, summed: the disk its image cache takes.
pub(crate) fn worker_image_bytes(worker: &str, images: &[Image]) -> u64 {
    images.iter().filter(|i| i.worker == worker).map(|i| i.bytes).sum()
}

/// Workers that may get new machines: a valid row (a node's name, an ip:port backhaul) with a fresh heartbeat.
/// Rows are announced by the workers themselves, so they are checked, never trusted.
pub(crate) fn live_workers(rows: &[Worker], now: i64, single_host: bool) -> Vec<String> {
    rows.iter().filter(|w| valid_worker(w, single_host).is_ok() && now - w.heartbeat_at < ALIVE_MS).map(|w| w.name.clone()).collect()
}

pub(crate) fn valid_worker(w: &Worker, single_host: bool) -> Result<(), String> {
    if !wg::node_name_ok(&w.name) {
        return Err(format!("bad worker name {:?}", w.name));
    }
    let addr: SocketAddr = w.backhaul.parse().map_err(|e| format!("worker {}: bad backhaul {:?}: {e}", w.name, w.backhaul))?;
    if !wg::backhaul_ok(addr, single_host) {
        return Err(format!(
            "worker {}: backhaul {addr} is not on the mesh{}",
            w.name,
            if addr.ip().is_loopback() { " (loopback: single-host mode only)" } else { "" }
        ));
    }
    Ok(())
}

/// `[a-z][a-z0-9-]{0,19}`, not ending in `-`: a DNS label, and short enough for `<app>-<6 hex>` ids.
pub(crate) fn app_name_ok(s: &str) -> bool {
    s.len() <= 20 && s.starts_with(|c: char| c.is_ascii_lowercase()) && !s.ends_with('-') && name_ok(s)
}
