//! `GET /v1/apps/<name>/load`: the app's load as the edges see it. The control plane asks every edge in the
//! `edges` table, at the address it announced once that's an edge node's own mesh address (mTLS, its own
//! certificate, as with the agent API), and sums each instance's counters over the edges: per instance, per
//! machine and for the app. Rates (req/s, errors/s) are from the totals' change since the last call for
//! this app.
// shortcut: the rates are per control plane and per app, between whoever's last two calls; good enough
// for a dashboard polling every few seconds.

use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, SocketAddr},
    time::Instant,
};

use corro_api_types::Statement;
use hyper::{Method, StatusCode};
use serde::Deserialize;

use super::{model::App, Control};
use crate::{corro::query, states::NodeRole};

/// The edges and where their load counters are.
pub(crate) const EDGES: &str = "SELECT name, load_addr FROM edges ORDER BY name";
/// Where each edge serves apps, by its load address (on its node's mesh address).
pub(crate) const EDGE_LISTENS: &str = "SELECT load_addr, listen FROM edges WHERE listen != '' ORDER BY listen";

/// One instance's counters on one edge (balance.rs's Load::json).
#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(crate) struct Counts {
    #[serde(default)]
    in_flight: u64,
    #[serde(default)]
    total: u64,
    #[serde(default)]
    rejected: u64,
    #[serde(default)]
    failures: u64,
}

impl Counts {
    fn add(&mut self, o: &Counts) {
        self.in_flight += o.in_flight;
        self.total += o.total;
        self.rejected += o.rejected;
        self.failures += o.failures;
    }

    fn errors(&self) -> u64 {
        self.rejected + self.failures
    }
}

/// The last sample of an app's totals, for its rates.
pub(crate) struct Sample {
    at: Instant,
    /// Instance id -> (total, errors).
    totals: HashMap<String, (u64, u64)>,
}

#[derive(Deserialize)]
struct EdgeLoad {
    load: HashMap<String, Counts>,
}

impl Control {
    /// An edge's announced load address, once bound: on the mesh address of an approved edge node.
    /// Single-host: loopback only.
    pub(super) fn edge_addr(&self, announced: &str) -> Result<SocketAddr, String> {
        let addr: SocketAddr = announced.parse().map_err(|e| format!("bad address {announced:?}: {e}"))?;
        if self.single_host {
            return if addr.ip().is_loopback() { Ok(addr) } else { Err(format!("{addr}: single-host mode takes loopback only")) };
        }
        match addr.ip() {
            IpAddr::V4(ip) if self.mesh.role_at(ip) == Some(NodeRole::Edge) => Ok(addr),
            _ => Err(format!("{addr} isn't an edge node's mesh address")),
        }
    }

    /// Every edge's counters: (edge name, its instances' counts, or why it didn't answer).
    async fn edge_loads(&self) -> Result<Vec<(String, Result<HashMap<String, Counts>, String>)>, String> {
        let edges: Vec<(String, String)> = query(&self.client, Statement::Simple(EDGES.into())).await?;
        let asks = edges.into_iter().map(|(name, announced)| async move {
            let r = async {
                let addr = self.edge_addr(&announced)?;
                match self.agents.call(addr, Method::GET, "/load", None).await? {
                    (StatusCode::OK, b) => serde_json::from_slice::<EdgeLoad>(&b).map(|l| l.load).map_err(|e| format!("bad answer: {e}")),
                    (code, _) => Err(format!("{}", code.as_u16())),
                }
            };
            (name, r.await)
        });
        Ok(futures::future::join_all(asks).await)
    }

    /// The app's load, as the API answers it.
    pub(crate) async fn app_load(&self, app: &App) -> Result<serde_json::Value, String> {
        let machines: Vec<_> = self.machines().await?.into_iter().filter(|m| m.app == app.name && m.live()).collect();
        let edges = self.edge_loads().await?;
        let mut instances: BTreeMap<String, Counts> = BTreeMap::new();
        for m in &machines {
            for s in &app.services {
                instances.insert(format!("{}.{}", m.id, s.name), Counts::default());
            }
        }
        for (_, load) in &edges {
            for (id, c) in load.iter().flatten() {
                if let Some(sum) = instances.get_mut(id) {
                    sum.add(c);
                }
            }
        }
        let now = Instant::now();
        let prev = self
            .load_samples
            .lock()
            .unwrap()
            .insert(app.name.clone(), Sample { at: now, totals: instances.iter().map(|(id, c)| (id.clone(), (c.total, c.errors()))).collect() });
        let dt = prev.as_ref().map(|p| now.duration_since(p.at).as_secs_f64()).filter(|dt| *dt > 0.0);
        // Per second since the last sample: None without one (or for an instance that's new since).
        let rate = |id: &str, (total, errors): (u64, u64)| -> Option<(f64, f64)> {
            let (p, dt) = (prev.as_ref()?.totals.get(id)?, dt?);
            Some((total.saturating_sub(p.0) as f64 / dt, errors.saturating_sub(p.1) as f64 / dt))
        };
        let round = |v: f64| (v * 10.0).round() / 10.0;
        let view = |c: &Counts, r: Option<(f64, f64)>| {
            serde_json::json!({
                "in_flight": c.in_flight, "total": c.total, "rejected": c.rejected, "failures": c.failures,
                "rps": r.map(|r| round(r.0)), "errors_per_s": r.map(|r| round(r.1)),
            })
        };
        let mut app_sum = Counts::default();
        let mut app_rate: Option<(f64, f64)> = dt.map(|_| (0.0, 0.0));
        let mut out_instances = Vec::new();
        let mut out_machines = Vec::new();
        for m in &machines {
            let mut sum = Counts::default();
            let mut m_rate: Option<(f64, f64)> = dt.map(|_| (0.0, 0.0));
            for s in &app.services {
                let id = format!("{}.{}", m.id, s.name);
                let c = instances[&id];
                let r = rate(&id, (c.total, c.errors()));
                sum.add(&c);
                m_rate = m_rate.zip(r).map(|(a, b)| (a.0 + b.0, a.1 + b.1));
                let mut v = view(&c, r);
                v["id"] = id.into();
                v["machine"] = m.id.clone().into();
                v["service"] = s.name.clone().into();
                v["soft_limit"] = s.soft_limit.into();
                v["hard_limit"] = s.hard_limit.into();
                out_instances.push(v);
            }
            app_sum.add(&sum);
            app_rate = app_rate.zip(m_rate).map(|(a, b)| (a.0 + b.0, a.1 + b.1));
            let mut v = view(&sum, m_rate);
            v["id"] = m.id.clone().into();
            v["worker"] = m.worker.clone().into();
            out_machines.push(v);
        }
        let mut v = view(&app_sum, app_rate);
        v["app"] = app.name.clone().into();
        v["interval_s"] = dt.map(round).into();
        v["machines"] = out_machines.into();
        v["instances"] = out_instances.into();
        v["edges"] = edges
            .iter()
            .map(|(name, r)| match r {
                Ok(_) => serde_json::json!({ "name": name, "ok": true }),
                Err(e) => serde_json::json!({ "name": name, "ok": false, "error": e }),
            })
            .collect::<Vec<_>>()
            .into();
        Ok(v)
    }
}
