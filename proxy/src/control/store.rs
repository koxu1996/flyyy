//! The control plane's reads: desired state from its own store (db.rs: apps, machines, orgs, users,
//! leases), facts from Corrosion (machine_status, service_status, images, workers).

use std::collections::HashMap;

use corro_api_types::Statement;

use crate::{
    control::{
        auth::{token_hash, Caller},
        model::{App, Check, Image, Machine, Org, User, Worker},
        Control,
    },
    corro::query,
    ingress::Lease,
    states::{Fact, Lifecycle},
};

/// The facts the control plane reads: checks, images, workers, machine states.
pub(crate) const CHECKS: &str = "SELECT machine, worker, service, state FROM service_status";
pub(crate) const IMAGES: &str = "SELECT worker, digest, bytes FROM images";
pub(crate) const WORKERS: &str =
    "SELECT name, backhaul, agent_api, started_at, heartbeat_at, arch, cpus, memory, machines, allocated_cpus, allocated_memory FROM workers ORDER BY name";
pub(crate) const FACTS: &str = "SELECT id, worker, state, addr, error FROM machine_status";

impl Control {
    /// Every app, or the one of that name.
    pub(crate) fn apps(&self, name: Option<&str>) -> Vec<App> {
        self.db.read(|r| r.apps.values().filter(|a| name.is_none_or(|n| a.name == n)).cloned().collect())
    }

    /// The caller behind a bearer token: the operator's own, or a user's (looked up by its hash) whose org
    /// is active. None: no such token.
    // The lookup compares hashes, not tokens, so its timing says nothing usable about a token.
    pub(crate) fn caller(&self, token: &str) -> Option<Caller> {
        if self.token.matches(token) {
            return Some(Caller::Operator);
        }
        let hash = token_hash(token);
        self.db.read(|r| {
            r.users
                .iter()
                .find(|(_, u)| u.token_sha256 == hash && r.orgs.get(&u.org).is_some_and(|o| o.state == Lifecycle::Active))
                .map(|(name, u)| Caller::User { name: name.clone(), org: u.org.clone() })
        })
    }

    pub(crate) fn orgs(&self) -> Vec<Org> {
        self.db.read(|r| r.orgs.iter().map(|(name, o)| Org { name: name.clone(), state: o.state }).collect())
    }

    /// Every user; never the token hashes.
    pub(crate) fn users(&self) -> Vec<User> {
        self.db.read(|r| r.users.iter().map(|(name, u)| User { name: name.clone(), org: u.org.clone(), role: u.role }).collect())
    }

    /// Every lease, allocated and released.
    pub(crate) fn leases(&self) -> Vec<Lease> {
        self.db.read(|r| r.leases.values().cloned().collect())
    }

    /// Every health check. Matched to a machine by its id and worker (Check::passing), never by the app a
    /// worker wrote in it.
    pub(crate) async fn checks(&self) -> Result<Vec<Check>, String> {
        query(&self.client, Statement::Simple(CHECKS.into())).await
    }

    pub(crate) async fn images(&self) -> Result<Vec<Image>, String> {
        query(&self.client, Statement::Simple(IMAGES.into())).await
    }

    pub(crate) async fn workers(&self) -> Result<Vec<Worker>, String> {
        let stmt = Statement::Simple(WORKERS.into());
        query(&self.client, stmt).await
    }

    /// The machines it placed, each with its `machine_status` fact: only one its own worker wrote counts.
    pub(crate) async fn machines(&self) -> Result<Vec<Machine>, String> {
        #[derive(serde::Deserialize)]
        struct Status {
            id: String,
            worker: String,
            #[serde(deserialize_with = "crate::corro::maybe")]
            state: Option<Fact>,
            addr: String,
            error: String,
        }
        let facts: Vec<Status> = query(&self.client, Statement::Simple(FACTS.into())).await?;
        let facts: HashMap<(String, String), Status> = facts.into_iter().map(|f| ((f.id.clone(), f.worker.clone()), f)).collect();
        Ok(self.db.read(|r| {
            r.machines
                .iter()
                .map(|(id, m)| {
                    let f = facts.get(&(id.clone(), m.worker.clone()));
                    Machine {
                        id: id.clone(),
                        worker: m.worker.clone(),
                        app: m.spec.app.clone(),
                        managed: true,
                        desired: m.desired,
                        state: f.and_then(|f| f.state),
                        addr: f.map_or(String::new(), |f| f.addr.clone()),
                        error: f.map_or(String::new(), |f| f.error.clone()),
                        cpus: m.spec.cpus,
                        memory: m.spec.memory,
                    }
                })
                .collect()
        }))
    }
}
