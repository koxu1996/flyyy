//! What the control plane publishes in Corrosion, the only writer of each of these tables: the ingress
//! addresses (ip_pools, ips), placement (each instance's app and worker) and each worker's backhaul as bound
//! to its mesh identity. Each is written only where it changed since the last time (all of it after a
//! restart). The edges route only by these: a row a worker writes can't move an app, an instance or a
//! backhaul.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

use corro_api_types::Statement;

use super::{model::Worker, Control};
use crate::{
    corro::query,
    ingress::Lease,
    states::{NodeRole, PoolKind},
};

/// A `placement` row of an instance (its id is the key).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placed {
    machine: String,
    app: String,
    worker: String,
    protocol: &'static str,
    port: u16,
    edge_port: u16,
    /// 0: none.
    soft_limit: u32,
    hard_limit: u32,
}

impl Control {
    /// Where a worker's announced service is, bound to its mesh identity: the mesh address of the approved
    /// worker node of that name, at the announced port. The worker says only the port: an address it
    /// announces that isn't its own changes nothing. Single-host: the announced loopback address.
    pub(crate) fn bound(&self, w: &Worker, announced: &str) -> Result<SocketAddr, String> {
        let addr: SocketAddr = announced.parse().map_err(|e| format!("bad address {announced:?}: {e}"))?;
        if self.single_host {
            return match addr.ip().is_loopback() {
                true => Ok(addr),
                false => Err(format!("{addr}: single-host mode takes loopback only")),
            };
        }
        let ip = self.mesh.node_ip(&w.name, NodeRole::Worker).ok_or("not a worker of the mesh")?;
        if addr.ip() != IpAddr::V4(ip) {
            self.agents.note(&format!("{} announces", w.name), Some(format!("{addr}, not on its mesh address {ip}: using {ip}")));
        }
        Ok(SocketAddr::new(IpAddr::V4(ip), addr.port()))
    }

    /// Its flags into `ip_pools`: each prefix active, any other prefix retired.
    pub(crate) async fn write_pools(&self) -> Result<(), String> {
        let mut want: Vec<(String, PoolKind)> = self.shared.iter().map(|ip| (format!("{ip}/32"), PoolKind::Shared)).collect();
        want.extend(self.pools.iter().map(|p| (p.to_string(), PoolKind::Pool)));
        let have: Vec<(String,)> = query(&self.client, Statement::Simple(crate::ingress::ACTIVE_PREFIXES.into())).await?;
        let mut stmts: Vec<Statement> = have
            .iter()
            .filter(|(p,)| !want.iter().any(|(w, _)| w == p))
            .map(|(p,)| Statement::WithParams("UPDATE ip_pools SET state = 'retired' WHERE prefix = ?".into(), vec![p.as_str().into()]))
            .collect();
        stmts.extend(want.iter().map(|(p, kind)| {
            Statement::WithParams(
                "INSERT INTO ip_pools (prefix, kind, state) VALUES (?, ?, 'active') ON CONFLICT (prefix) DO UPDATE SET kind = excluded.kind, state = 'active'"
                    .into(),
                vec![p.as_str().into(), kind.to_string().into()],
            )
        }));
        if !stmts.is_empty() {
            self.client.execute(&stmts, None).await.map_err(|e| format!("writing ip_pools: {e}"))?;
        }
        tracing::info!(shared = ?want.iter().filter(|w| w.1 == PoolKind::Shared).map(|w| &w.0).collect::<Vec<_>>(), pools = ?self.pools.iter().map(|p| p.to_string()).collect::<Vec<_>>(), "ingress prefixes published");
        Ok(())
    }

    /// The leases into `ips`, those that changed since the last time (all of them after a restart).
    pub(crate) async fn publish_leases(&self) -> Result<(), String> {
        let leases = self.leases();
        let changed: Vec<&Lease> = {
            let published = self.published_leases.lock().unwrap();
            leases.iter().filter(|l| published.get(&l.ip) != Some(*l)).collect()
        };
        if changed.is_empty() {
            return Ok(());
        }
        let stmts: Vec<Statement> = changed
            .iter()
            .map(|l| {
                Statement::WithParams(
                    "INSERT INTO ips (ip, app, state, allocated_at, released_at) VALUES (?, ?, ?, ?, ?) \
                     ON CONFLICT (ip) DO UPDATE SET app = excluded.app, state = excluded.state, allocated_at = excluded.allocated_at, released_at = excluded.released_at"
                        .into(),
                    vec![
                        l.ip.to_string().into(), l.app.as_str().into(), if l.released_at == 0 { "allocated" } else { "released" }.into(),
                        l.allocated_at.into(), l.released_at.into(),
                    ],
                )
            })
            .collect();
        self.client.execute(&stmts, None).await.map_err(|e| format!("writing ips: {e}"))?;
        self.published_leases.lock().unwrap().extend(changed.into_iter().map(|l| (l.ip, l.clone())));
        Ok(())
    }

    /// Every instance of every machine it placed into `placement`.
    pub(crate) async fn publish_placement(&self) -> Result<(), String> {
        let want: HashMap<String, Placed> = self.db.read(|r| {
            r.machines
                .iter()
                .flat_map(|(id, m)| {
                    m.spec.services.iter().map(move |s| {
                        let p = Placed {
                            machine: id.clone(),
                            app: m.spec.app.clone(),
                            worker: m.worker.clone(),
                            protocol: s.protocol.as_str(),
                            port: s.port,
                            edge_port: s.published_port().unwrap_or(0),
                            soft_limit: s.soft_limit.unwrap_or(0),
                            hard_limit: s.hard_limit.unwrap_or(0),
                        };
                        (format!("{id}.{}", s.name), p)
                    })
                })
                .collect()
        });
        let changed: Vec<(&String, &Placed)> = {
            let published = self.published_placement.lock().unwrap();
            want.iter().filter(|(id, p)| published.get(*id) != Some(*p)).collect()
        };
        if changed.is_empty() {
            return Ok(());
        }
        let stmts: Vec<Statement> =
            changed
                .iter()
                .map(|(id, p)| {
                    Statement::WithParams(
                    "INSERT INTO placement (id, machine, app, worker, protocol, port, edge_port, soft_limit, hard_limit) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (id) DO UPDATE SET machine = excluded.machine, app = excluded.app, worker = excluded.worker, \
                     protocol = excluded.protocol, port = excluded.port, edge_port = excluded.edge_port, \
                     soft_limit = excluded.soft_limit, hard_limit = excluded.hard_limit"
                        .into(),
                    vec![
                        id.as_str().into(), p.machine.as_str().into(), p.app.as_str().into(), p.worker.as_str().into(), p.protocol.into(),
                        (p.port as i64).into(), (p.edge_port as i64).into(), (p.soft_limit as i64).into(), (p.hard_limit as i64).into(),
                    ],
                )
                })
                .collect();
        self.client.execute(&stmts, None).await.map_err(|e| format!("writing placement: {e}"))?;
        self.published_placement.lock().unwrap().extend(changed.into_iter().map(|(id, p)| (id.clone(), p.clone())));
        Ok(())
    }

    /// Each worker's announced backhaul into `backhauls`, once bound to its mesh identity; "" while it isn't
    /// (the edges then don't route to it at all).
    pub(crate) async fn publish_backhauls(&self, workers: &[Worker]) -> Result<(), String> {
        let want: HashMap<String, String> = workers
            .iter()
            .map(|w| {
                let b = match self.bound(w, &w.backhaul) {
                    Ok(a) => a.to_string(),
                    Err(e) => {
                        self.agents.note(&format!("{} backhaul", w.name), Some(e));
                        String::new()
                    }
                };
                (w.name.clone(), b)
            })
            .collect();
        let changed: Vec<(&String, &String)> = {
            let published = self.published_backhauls.lock().unwrap();
            want.iter().filter(|(w, b)| published.get(*w) != Some(*b)).collect()
        };
        if changed.is_empty() {
            return Ok(());
        }
        let stmts: Vec<Statement> = changed
            .iter()
            .map(|(w, b)| {
                Statement::WithParams(
                    "INSERT INTO backhauls (worker, backhaul) VALUES (?, ?) ON CONFLICT (worker) DO UPDATE SET backhaul = excluded.backhaul".into(),
                    vec![w.as_str().into(), b.as_str().into()],
                )
            })
            .collect();
        self.client.execute(&stmts, None).await.map_err(|e| format!("writing backhauls: {e}"))?;
        for (w, b) in &changed {
            tracing::info!(worker = %w, backhaul = %if b.is_empty() { "none (not bound to its mesh identity)" } else { b.as_str() }, "backhaul bound");
        }
        self.published_backhauls.lock().unwrap().extend(changed.into_iter().map(|(w, b)| (w.clone(), b.clone())));
        Ok(())
    }
}
