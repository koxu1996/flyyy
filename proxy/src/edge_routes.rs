//! The edge's routes: passing services from its subscription, by vhost (http) or `tcp:<ip>:<port>`, each
//! at its worker's backhaul; plus what it polls (stale workers, ingress addresses) and the instances it
//! ejects for a while after a tunnel to them failed.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tracing::warn;

use hyper::header::HeaderValue;
use serde::{Deserialize, Serialize};

use crate::{
    balance::{self, Loads},
    corro::maybe,
    ingress,
    pool::Pool,
    services::{self, Protocol},
    subscription::{Sub, Table},
    table::{Route, RouteTable, Target, Upstream},
    wg,
};

/// The edge's subscription: passing services as the control plane placed them, at the backhaul it bound
/// for their worker, with their app's IP. Only `service_status` (the passing) is a worker's word, and only
/// for an instance placed on that worker: a row for another worker's machine, or one nobody placed, joins
/// nothing. App, protocol and edge port are placement's, the backhaul is `backhauls'`: none of them a
/// worker's to say. Heartbeats aren't in it (stale workers are polled).
pub(crate) const SUBSCRIPTION: &str = "SELECT s.id, p.worker, p.app, b.backhaul, p.protocol, p.edge_port, coalesce(i.ip, ''), p.soft_limit, p.hard_limit \
     FROM service_status s \
     JOIN placement p ON p.id = s.id AND p.worker = s.worker JOIN backhauls b ON b.worker = p.worker \
     LEFT JOIN ips i ON i.app = p.app AND i.state = 'allocated' WHERE s.state = 'passing'";

/// One row of SUBSCRIPTION. Every field is checked before it's routed (edge_entry), never trusted.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct EdgeRow {
    pub(crate) id: String,
    pub(crate) worker: String,
    pub(crate) app: String,
    pub(crate) backhaul: String,
    #[serde(deserialize_with = "maybe")]
    pub(crate) protocol: Option<Protocol>,
    #[serde(deserialize_with = "maybe")]
    pub(crate) edge_port: Option<u16>,
    pub(crate) ip: String,
    /// The service's concurrency limits per instance (0: none; off-type: none).
    #[serde(deserialize_with = "maybe")]
    pub(crate) soft_limit: Option<u32>,
    #[serde(deserialize_with = "maybe")]
    pub(crate) hard_limit: Option<u32>,
}

/// In `/state` as the row it came as: an array in SUBSCRIPTION's column order, edge_port as text.
impl Serialize for EdgeRow {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let edge_port = self.edge_port.map(|p| p.to_string());
        (&self.id, &self.worker, &self.app, &self.backhaul, self.protocol, edge_port, &self.ip, self.soft_limit, self.hard_limit).serialize(s)
    }
}

impl std::fmt::Display for EdgeRow {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let port = self.edge_port.map_or("-".into(), |p| p.to_string());
        let protocol = self.protocol.map_or("-", Protocol::as_str);
        write!(f, "{} {} {} {} {protocol} {port} {}", self.id, self.worker, self.app, self.backhaul, self.ip)
    }
}

/// What an edge knows of ingress addresses: shared IPs (HTTP by Host), pools (dedicated addresses: tcp
/// only, each on its app's address) and each allocated address's app.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Ingress {
    pub(crate) shared: Vec<Ipv4Addr>,
    pub(crate) pools: Vec<ingress::Prefix>,
    pub(crate) owners: HashMap<Ipv4Addr, String>,
}

impl Ingress {
    pub(crate) fn in_pool(&self, ip: IpAddr) -> bool {
        matches!(ip, IpAddr::V4(v4) if self.pools.iter().any(|p| p.contains(v4)))
    }
}

/// How long an instance whose tunnel failed is tried last.
pub(crate) const EJECT: Duration = Duration::from_secs(10);

/// What an edge is configured with (its flags).
pub(crate) struct EdgeConfig {
    /// Every answer's x-edge header.
    pub(crate) name: HeaderValue,
    /// http services are `<app>.<apps_domain>`.
    pub(crate) apps_domain: String,
    /// `--single-host`: loopback backhauls are routed too.
    pub(crate) single_host: bool,
}

/// An edge's state: its configuration, subscription and routes, its backhaul connections, and what it polls.
pub(crate) struct EdgeState {
    pub(crate) config: EdgeConfig,
    pub(crate) pool: Pool,
    sub: Mutex<Sub<EdgeRow>>,
    pub(crate) routes: RouteTable,
    /// Workers whose heartbeat is stale (polled, so heartbeats don't churn the subscription).
    pub(crate) stale: RwLock<HashSet<String>>,
    /// Instances whose tunnel just failed, until when they're tried last.
    ejected: Mutex<HashMap<String, Instant>>,
    /// Each instance's requests in flight and counters (balance.rs).
    pub(crate) loads: Loads,
    /// Each app service's traffic, 10 s buckets for the last hour (metrics.rs).
    pub(crate) metrics: Arc<crate::metrics::Edge>,
    /// Rows not routed, and why (in /state).
    skipped: Mutex<BTreeMap<String, String>>,
    /// The ingress prefixes and who owns each dedicated address, and that they changed (the shared IPs'
    /// HTTP listeners follow them).
    pub(crate) ingress: RwLock<Ingress>,
    pub(crate) ingress_changed: tokio::sync::Notify,
}

impl EdgeState {
    pub(crate) fn new(config: EdgeConfig, pool: Pool) -> Self {
        EdgeState {
            config,
            pool,
            sub: Mutex::default(),
            routes: RouteTable::default(),
            stale: RwLock::default(),
            ejected: Mutex::default(),
            loads: Loads::default(),
            metrics: Arc::default(),
            skipped: Mutex::default(),
            ingress: RwLock::default(),
            ingress_changed: tokio::sync::Notify::new(),
        }
    }

    /// The order to try a route's instances in (balance::order): by load, fresh workers first, ejected last.
    pub(crate) fn order<'a>(&self, route: &'a Route) -> Vec<&'a Upstream> {
        let (stale, ejected) = (self.stale.read().unwrap(), self.ejected.lock().unwrap());
        balance::order(&route.upstreams, |id| self.loads.in_flight(id), &stale, &ejected, Instant::now(), (balance::random(), balance::random()))
    }

    pub(crate) fn eject(&self, id: &str) {
        self.ejected.lock().unwrap().insert(id.to_owned(), Instant::now() + EJECT);
    }

    pub(crate) fn recovered(&self, id: &str) {
        self.ejected.lock().unwrap().remove(id);
    }

    /// Each instance's counters (in flight, total, rejected, failures, peak) and limits.
    pub(crate) fn load_json(&self) -> serde_json::Value {
        self.loads.json(&self.routes.upstreams())
    }

    /// `/state`: the subscription, the routes, the instances ejected for now, and every instance's load.
    pub(crate) fn state_json(&self) -> String {
        let now = Instant::now();
        let mut v = self.sub.lock().unwrap().json();
        v["routes"] = self.routes.json();
        v["ejected"] = self.ejected.lock().unwrap().iter().filter(|(_, t)| **t > now).map(|(id, _)| id.clone()).collect::<Vec<_>>().into();
        v["load"] = self.load_json();
        v["skipped"] = serde_json::json!(*self.skipped.lock().unwrap());
        v.to_string()
    }
}

/// A row as (route key, upstream), or why it isn't routed.
fn edge_entry(config: &EdgeConfig, row: &EdgeRow) -> Result<(String, Upstream), String> {
    let (key, addr) = edge_upstream(config, &row.worker, &row.app, &row.backhaul, row.protocol, row.edge_port, &row.ip)?;
    let up = Upstream {
        id: row.id.clone(),
        addr: Target::Tcp(addr),
        app: row.app.clone(),
        worker: row.worker.clone(),
        soft_limit: row.soft_limit.unwrap_or(0),
        hard_limit: row.hard_limit.unwrap_or(0),
    };
    Ok((key, up))
}

impl Table for EdgeState {
    type Row = EdgeRow;

    fn sub(&self) -> &Mutex<Sub<EdgeRow>> {
        &self.sub
    }

    fn snapshot(&self, rows: &BTreeMap<u64, EdgeRow>) {
        let mut by_key: BTreeMap<String, Vec<Upstream>> = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        for row in rows.values() {
            match edge_entry(&self.config, row) {
                Ok((key, up)) => by_key.entry(key).or_default().push(up),
                Err(e) => {
                    warn!(instance = %row.id, reason = %e, "skipping instance");
                    skipped.insert(row.id.clone(), e);
                }
            }
        }
        self.routes.replace(by_key);
        *self.skipped.lock().unwrap() = skipped;
    }

    /// Just this row: out of the route of the row it replaced, into the route of the row it is now.
    fn change(&self, old: Option<&EdgeRow>, new: Option<&EdgeRow>, _rows: &BTreeMap<u64, EdgeRow>) {
        if let Some(r) = old {
            self.skipped.lock().unwrap().remove(&r.id);
        }
        if let Some((key, up)) = old.and_then(|r| edge_entry(&self.config, r).ok()) {
            self.routes.edit(&key, |ups| ups.retain(|u| u.id != up.id));
        }
        match new.map(|r| edge_entry(&self.config, r)) {
            Some(Ok((key, up))) => self.routes.edit(&key, |ups| {
                ups.retain(|u| u.id != up.id);
                ups.push(up);
            }),
            Some(Err(e)) => {
                let id = new.map_or("?", |r| r.id.as_str());
                warn!(instance = %id, reason = %e, "skipping instance");
                self.skipped.lock().unwrap().insert(id.to_owned(), e);
            }
            None => {}
        }
    }
}

/// One edge row → (route key, backhaul). The backhaul is the control plane's `backhauls` row, bound to the
/// worker's mesh identity; it's still checked here, as gossip isn't authenticated: the worker name must be a
/// node's name (`wg::node_name_ok`), and the address be on the mesh (or loopback, in single-host mode). The worker validates
/// every CONNECT target too. The route key: an http service's vhost `<app>.<apps-domain>`, a tcp service's
/// `tcp:<app's IP>:<edge port>`.
pub(crate) fn edge_upstream(
    config: &EdgeConfig,
    worker: &str,
    app: &str,
    backhaul: &str,
    protocol: Option<Protocol>,
    edge_port: Option<u16>,
    ip: &str,
) -> Result<(String, SocketAddr), String> {
    if !wg::node_name_ok(worker) {
        return Err(format!("bad worker name {worker:?}: want a node's name"));
    }
    let addr = backhaul.parse().map_err(|e| format!("worker {worker}: bad backhaul {backhaul:?}: {e}"))?;
    if !wg::backhaul_ok(addr, config.single_host) {
        return Err(format!("worker {worker}: backhaul {addr} is not on the mesh"));
    }
    match protocol {
        Some(Protocol::Http) => Ok((format!("{app}.{}", config.apps_domain).to_ascii_lowercase(), addr)),
        Some(Protocol::Tcp) => match (edge_port, ip.parse::<Ipv4Addr>()) {
            (Some(p), Ok(ip)) if p > 0 && !services::RESERVED_EDGE_PORTS.contains(&p) => Ok((format!("tcp:{ip}:{p}"), addr)),
            (_, Err(_)) => Err(format!("app {app}: no dedicated IP ({ip:?})")),
            _ => Err(format!("bad edge port {edge_port:?}")),
        },
        None => Err("bad protocol (not http or tcp)".into()),
    }
}

/// An edge for tests: edge1, the default apps domain, on the mesh (no TLS: never dials).
#[cfg(test)]
pub(crate) fn test_edge() -> EdgeState {
    EdgeState::new(EdgeConfig { name: HeaderValue::from_static("edge1"), apps_domain: "flyyy.localhost".into(), single_host: false }, Pool::new(None))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::table::ids;

    fn row(id: &str, worker: &str, app: &str, backhaul: &str, protocol: &str, edge_port: u16, ip: &str) -> EdgeRow {
        let protocol = serde_json::from_value(serde_json::Value::String(protocol.into())).ok();
        EdgeRow {
            id: id.into(),
            worker: worker.into(),
            app: app.into(),
            backhaul: backhaul.into(),
            protocol,
            edge_port: Some(edge_port),
            ip: ip.into(),
            soft_limit: Some(0),
            hard_limit: Some(0),
        }
    }

    fn edge() -> EdgeState {
        test_edge()
    }

    #[test]
    fn edge_groups_by_app_across_workers_and_skips_bad_worker_rows() {
        let state = edge();
        let mut rows = BTreeMap::from([
            (1, row("w2-a", "worker2", "Web", "10.250.1.2:4280", "http", 0, "10.200.1.9")),
            (2, row("w1-a", "worker1", "web", "10.250.1.1:4280", "http", 0, "")),
            (3, row("w9-a", "worker9", "web", "not-an-addr", "http", 0, "")),
            (4, row("x-a", "Evil!", "web", "10.250.1.9:4280", "http", 0, "")),
            // Off the mesh: a LAN address, and a token's address.
            (5, row("w5-a", "worker5", "web", "10.99.5.2:4280", "http", 0, "")),
            (6, row("w6-a", "worker6", "web", "10.250.255.6:4280", "http", 0, "")),
            // tcp: by (app's IP, edge port); never the edges' own ports, never without an IP or a bad protocol.
            (7, row("w1-d", "worker1", "db", "10.250.1.1:4280", "tcp", 5432, "10.200.1.7")),
            (8, row("w1-e", "worker1", "db", "10.250.1.1:4280", "tcp", 443, "10.200.1.7")),
            (9, row("w1-f", "worker1", "db", "10.250.1.1:4280", "udp", 53, "10.200.1.7")),
            (10, row("w2-g", "worker2", "pg", "10.250.1.2:4280", "tcp", 5432, "10.200.1.8")),
            (11, row("w2-h", "worker2", "pg", "10.250.1.2:4280", "tcp", 5432, "")),
            // An edge_port that isn't a number (gossip from anywhere): that row alone is skipped.
            (12, EdgeRow { edge_port: None, ..row("w2-i", "worker2", "pg", "10.250.1.2:4280", "tcp", 0, "10.200.1.8") }),
        ]);
        state.snapshot(&rows);
        assert_eq!(ids(&state.routes, "web.flyyy.localhost"), ["w1-a 10.250.1.1:4280", "w2-a 10.250.1.2:4280"]);
        // Two apps on :5432, each on its own address: two routes.
        assert_eq!(ids(&state.routes, "tcp:10.200.1.7:5432"), ["w1-d 10.250.1.1:4280"]);
        assert_eq!(ids(&state.routes, "tcp:10.200.1.8:5432"), ["w2-g 10.250.1.2:4280"]);
        assert_eq!(state.routes.keys().len(), 3);
        rows.remove(&1);
        state.snapshot(&rows);
        assert_eq!(ids(&state.routes, "web.flyyy.localhost"), ["w1-a 10.250.1.1:4280"]);
    }

    #[test]
    fn edge_applies_a_change_to_its_routes_only() {
        let state = edge();
        let web = |id: &str, w: &str| row(id, w, "web", &format!("10.250.1.{}:4280", &w[6..]), "http", 0, "");
        let db = row("c", "worker1", "db", "10.250.1.1:4280", "tcp", 5432, "10.200.1.7");
        let none = BTreeMap::new();
        state.snapshot(&BTreeMap::from([(1, web("a", "worker1")), (2, web("b", "worker2")), (3, db.clone())]));
        let db_route = state.routes.route("tcp:10.200.1.7:5432").unwrap().unwrap();
        // b goes, d comes (another worker), a moves to worker3: web changes, db stays the very same Arc.
        state.change(Some(&web("b", "worker2")), None, &none);
        state.change(None, Some(&web("d", "worker2")), &none);
        state.change(Some(&web("a", "worker1")), Some(&web("a", "worker3")), &none);
        assert_eq!(ids(&state.routes, "web.flyyy.localhost"), ["a 10.250.1.3:4280", "d 10.250.1.2:4280"]);
        assert!(Arc::ptr_eq(&db_route, &state.routes.route("tcp:10.200.1.7:5432").unwrap().unwrap()), "untouched routes aren't rebuilt");
        // A bad new row is skipped; its route's last upstream gone removes the route.
        state.change(None, Some(&row("x", "Evil!", "web", "10.250.1.9:4280", "http", 0, "")), &none);
        state.change(Some(&db), None, &none);
        assert!(state.routes.route("tcp:10.200.1.7:5432").unwrap().is_none() && ids(&state.routes, "web.flyyy.localhost").len() == 2);
    }

    #[test]
    fn rows_read_like_the_subscription_writes_them() {
        let r: EdgeRow = serde_json::from_str(r#"["w1-d.db", "worker1", "db", "10.250.1.1:4280", "tcp", 5432, "10.200.1.7", 0, 0]"#).unwrap();
        assert_eq!(r, row("w1-d.db", "worker1", "db", "10.250.1.1:4280", "tcp", 5432, "10.200.1.7"));
        assert_eq!(r.to_string(), "w1-d.db worker1 db 10.250.1.1:4280 tcp 5432 10.200.1.7");
        let bad: EdgeRow = serde_json::from_str(r#"["x", "worker1", "db", "10.250.1.1:4280", "tcp", "soon", "", "x", 2]"#).unwrap();
        assert_eq!((bad.edge_port, bad.soft_limit, bad.hard_limit), (None, None, Some(2)));
        // In /state as before typed rows: an array, edge_port as text (null if absent).
        assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::json!(["w1-d.db", "worker1", "db", "10.250.1.1:4280", "tcp", "5432", "10.200.1.7", 0, 0]));
        assert_eq!(serde_json::to_value(&bad).unwrap()[5], serde_json::Value::Null);
    }
}
