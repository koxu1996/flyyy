//! A route table: route key -> upstreams, each route behind its own Arc, so a request takes only its
//! route and a change replaces only the routes it touches. The edge's and the backhaul's (edge_routes.rs,
//! backhaul.rs) are built from their subscriptions' rows.

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{Arc, RwLock},
};

#[derive(Clone)]
pub(crate) struct Upstream {
    pub(crate) id: String,
    pub(crate) addr: Target,
    /// Edge: its app, whose metrics its requests count in (metrics.rs).
    pub(crate) app: String,
    /// Edge: the worker it's on, to try instances on stale workers last.
    pub(crate) worker: String,
    /// Edge: its service's concurrency limits (balance.rs; 0: none).
    pub(crate) soft_limit: u32,
    pub(crate) hard_limit: u32,
}

/// Where an upstream is. Edge: a worker's backhaul. Worker: a VM, by its vsock socket and guest port.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    Tcp(SocketAddr),
    Vsock { socket: String, port: u16 },
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Target::Tcp(a) => write!(f, "{a}"),
            Target::Vsock { socket, port } => write!(f, "{socket}:{port}"),
        }
    }
}

/// A route's instances (the edge picks among them by load, balance.rs).
pub(crate) struct Route {
    pub(crate) upstreams: Vec<Upstream>,
}

pub(crate) type Routes = HashMap<String, Arc<Route>>;

/// No routes yet: the first snapshot hasn't come.
#[derive(Debug)]
pub(crate) struct NotReady;

/// The routes, `None` until the first snapshot (requests then get 503, not 404), and a wake-up for what
/// follows them (the edge's tcp listeners).
#[derive(Default)]
pub(crate) struct RouteTable {
    routes: RwLock<Option<Routes>>,
    pub(crate) changed: tokio::sync::Notify,
}

impl RouteTable {
    /// Look up a route: NotReady while there's no table yet (a 503, where a missing route is a 404).
    pub(crate) fn route(&self, key: &str) -> Result<Option<Arc<Route>>, NotReady> {
        self.routes.read().unwrap().as_ref().map(|r| r.get(key).cloned()).ok_or(NotReady)
    }

    pub(crate) fn keys(&self) -> Vec<String> {
        self.routes.read().unwrap().as_ref().map(|r| r.keys().cloned().collect()).unwrap_or_default()
    }

    /// The whole table anew (a snapshot).
    pub(crate) fn replace(&self, by_key: BTreeMap<String, Vec<Upstream>>) {
        let mut guard = self.routes.write().unwrap();
        let routes = by_key
            .into_iter()
            .map(|(key, mut upstreams)| {
                upstreams.sort_by(|a, b| a.id.cmp(&b.id));
                (key, Arc::new(Route { upstreams }))
            })
            .collect();
        *guard = Some(routes);
        drop(guard);
        self.changed.notify_one();
    }

    /// One route rebuilt from its own upstreams (`f` changes them); gone once it has none. No other route
    /// is touched.
    pub(crate) fn edit(&self, key: &str, f: impl FnOnce(&mut Vec<Upstream>)) {
        let mut guard = self.routes.write().unwrap();
        let routes = guard.get_or_insert_with(HashMap::new);
        let mut ups = routes.get(key).map_or_else(Vec::new, |r| r.upstreams.clone());
        f(&mut ups);
        if ups.is_empty() {
            routes.remove(key);
        } else {
            ups.sort_by(|a, b| a.id.cmp(&b.id));
            routes.insert(key.to_owned(), Arc::new(Route { upstreams: ups }));
        }
        drop(guard);
        self.changed.notify_one();
    }

    /// Every routed upstream, by instance id (the limits for /state's load).
    pub(crate) fn upstreams(&self) -> HashMap<String, Upstream> {
        self.routes
            .read()
            .unwrap()
            .as_ref()
            .map(|r| r.values().flat_map(|route| route.upstreams.iter().map(|u| (u.id.clone(), u.clone()))).collect())
            .unwrap_or_default()
    }

    /// Its part of `/state`: each key's upstreams as "id addr".
    pub(crate) fn json(&self) -> serde_json::Value {
        let routes = self.routes.read().unwrap().as_ref().map(|r| {
            r.iter()
                .map(|(k, route)| (k.clone(), route.upstreams.iter().map(|u| format!("{} {}", u.id, u.addr)).collect::<Vec<_>>()))
                .collect::<BTreeMap<_, _>>()
        });
        serde_json::json!(routes)
    }
}

#[cfg(test)]
pub(crate) fn ids(table: &RouteTable, key: &str) -> Vec<String> {
    table.route(key).unwrap().unwrap().upstreams.iter().map(|u| format!("{} {}", u.id, u.addr)).collect()
}
