//! The worker side of the backhaul: CONNECT <instance> from the edges, piped to that VM over vsock.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{info, warn};

use corro_api_types::Statement;
use hyper::{body::Incoming, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};

use crate::{
    http::{empty, serve_state},
    subscription::{follow, Sub, Table},
    table::{NotReady, RouteTable, Target, Upstream},
    tls,
    vsock::{dial_vsock, parse_vsock, socket},
    wg, Body, NodeArgs,
};

/// A backhaul's subscription: the instances the control plane placed on its worker whose services aren't
/// down (failing ones too: the edges decide where to route), each at its machine's vsock port. The one
/// parameter is the worker's name. The address comes from placement, not from a worker's row.
pub(crate) const SUBSCRIPTION: &str = "SELECT p.id, p.app, 'vsock:' || p.machine || ':' || p.port FROM placement p \
     JOIN service_status s ON s.id = p.id AND s.worker = p.worker WHERE p.worker = ? AND s.state != 'down'";

/// One row of SUBSCRIPTION: an instance (`<machine>.<service>`) and its `vsock:<machine>:<port>`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct BackhaulRow {
    pub(crate) id: String,
    pub(crate) app: String,
    pub(crate) addr: String,
}

/// In `/state` as the row it came as: an array in SUBSCRIPTION's column order.
impl Serialize for BackhaulRow {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (&self.id, &self.app, &self.addr).serialize(s)
    }
}

impl std::fmt::Display for BackhaulRow {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} {} {}", self.id, self.app, self.addr)
    }
}

/// A backhaul's state: the instances it may dial, and each one's last CONNECT (the agent's drain waits for
/// it to go quiet).
pub(crate) struct BackhaulState {
    /// The worker's runtime directory, with each VM's vsock socket.
    run_dir: String,
    sub: Mutex<Sub<BackhaulRow>>,
    pub(crate) routes: RouteTable,
    last_connect: Mutex<HashMap<String, Instant>>,
    /// Rows not served, and why (in /state).
    skipped: Mutex<BTreeMap<String, String>>,
}

impl BackhaulState {
    pub(crate) fn new(run_dir: String) -> Self {
        BackhaulState { run_dir, sub: Mutex::default(), routes: RouteTable::default(), last_connect: Mutex::default(), skipped: Mutex::default() }
    }

    /// `/state`: the subscription, the routes, and how long ago each instance got its last CONNECT.
    pub(crate) fn state_json(&self) -> String {
        let now = Instant::now();
        let mut v = self.sub.lock().unwrap().json();
        v["routes"] = self.routes.json();
        v["skipped"] = serde_json::json!(*self.skipped.lock().unwrap());
        v["last_connect_ms_ago"] = serde_json::json!(self
            .last_connect
            .lock()
            .unwrap()
            .iter()
            .map(|(id, t)| (id.clone(), now.duration_since(*t).as_millis() as u64))
            .collect::<BTreeMap<_, _>>());
        v.to_string()
    }
}

impl Table for BackhaulState {
    type Row = BackhaulRow;

    fn sub(&self) -> &Mutex<Sub<BackhaulRow>> {
        &self.sub
    }

    /// An addr may name another of this worker's machines (the storm probe rows do), but never one that
    /// isn't in this worker's own table.
    fn snapshot(&self, rows: &BTreeMap<u64, BackhaulRow>) {
        let own: HashSet<&str> = rows.values().map(|r| r.id.split_once('.').map_or(r.id.as_str(), |(m, _)| m)).collect();
        let mut by_key: BTreeMap<String, Vec<Upstream>> = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        for row in rows.values() {
            let entry = parse_vsock(&row.addr).and_then(|(machine, port)| match own.contains(machine.as_str()) {
                true => Ok(Upstream {
                    id: row.id.clone(),
                    addr: Target::Vsock { socket: socket(&self.run_dir, &machine), port },
                    app: String::new(),
                    worker: String::new(),
                    soft_limit: 0,
                    hard_limit: 0,
                }),
                false => Err(format!("addr {:?}: {machine} isn't a machine of this worker", row.addr)),
            });
            match entry {
                Ok(up) => by_key.entry(row.id.clone()).or_default().push(up),
                Err(e) => {
                    warn!(instance = %row.id, reason = %e, "skipping instance");
                    skipped.insert(row.id.clone(), e);
                }
            }
        }
        self.routes.replace(by_key);
        *self.skipped.lock().unwrap() = skipped;
    }

    /// The whole (small) table again: a row is valid only against the others (its machine must be one of
    /// this worker's).
    fn change(&self, _old: Option<&BackhaulRow>, _new: Option<&BackhaulRow>, rows: &BTreeMap<u64, BackhaulRow>) {
        self.snapshot(rows);
    }
}

/// `proxy worker`'s flags.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// This worker's mesh address and port (or loopback with --single-host): never a wildcard, so the
    /// backhaul can't be reached off the mesh.
    #[arg(long)]
    listen: SocketAddr,
    /// Its /state.
    #[arg(long)]
    state: SocketAddr,
    /// Its Corrosion agent's API and token.
    #[command(flatten)]
    corrosion: crate::corro::ApiArgs,
    /// This worker's name (a node's name: `wg::node_name_ok`): its services are the ones served.
    #[arg(long, value_parser = crate::wg::node_name)]
    worker: String,
    /// The agent's runtime directory, with each VM's vsock socket.
    #[arg(long, env = "FLYYY_RUN_DIR", default_value = "/run/flyyy")]
    run_dir: String,
    #[command(flatten)]
    node: NodeArgs,
}

/// `proxy worker`: the backhaul of a worker, following its services and taking CONNECTs from the edges.
pub async fn run(args: Args) {
    let single_host = args.node.single_host("backhaul");
    let state = Arc::new(BackhaulState::new(args.run_dir));
    let stmt = Statement::WithParams(SUBSCRIPTION.into(), vec![args.worker.clone().into()]);
    let api = args.corrosion.open();
    tokio::spawn(follow(api.clone(), stmt, state.clone()));
    let s = state.clone();
    let debug = TcpListener::bind(args.state).await.expect("bind state");
    tokio::spawn(serve_state(debug, move || s.state_json()));
    let addr = args.listen;
    info!(worker = %args.worker, %addr, state = %args.state, corrosion = %api.addr, "backhaul started");
    assert!(
        wg::backhaul_ok(addr, single_host),
        "backhaul listen {addr}: want this worker's mesh address (10.250.x.y), or loopback with --single-host; not {}",
        addr.ip()
    );
    let listener = TcpListener::bind(addr).await.expect("bind");
    // On the mesh: mTLS, and only edges' certificates (the backhaul takes CONNECTs from edges only).
    let acceptor = args.node.tls_dir.map(|dir| {
        info!(%dir, "backhaul: mTLS, edge certificates only");
        tls::backhaul_acceptor(&dir).unwrap_or_else(|e| panic!("mesh tls: {e}"))
    });
    serve_backhaul(listener, acceptor, state).await;
}

/// Worker side of the backhaul. Only instances in this worker's own table are dialled, so an
/// edge can't make the worker connect anywhere else; unknown ids and raw addresses get 404.
pub(crate) async fn backhaul(req: Request<Incoming>, state: Arc<BackhaulState>, peer: SocketAddr) -> Result<Response<Body>, Infallible> {
    let t0 = Instant::now();
    let target = req.uri().authority().map(|a| a.host().to_owned());
    if let (true, Some(id)) = (req.method() == Method::CONNECT, target.as_deref()) {
        let mut last = state.last_connect.lock().unwrap();
        // Only recent ones matter (a drain looks at the last second or so); keep the map small.
        if last.len() > 4096 {
            last.retain(|_, t| t.elapsed() < Duration::from_secs(600));
        }
        last.insert(id.to_owned(), Instant::now());
    }
    let route = target.as_deref().map(|id| state.routes.route(id));
    let (resp, vm) = match (req.method() == Method::CONNECT, route, target.as_deref()) {
        (false, ..) => (empty(StatusCode::METHOD_NOT_ALLOWED), None),
        (_, Some(Err(NotReady)), _) => (empty(StatusCode::SERVICE_UNAVAILABLE), None),
        (_, _, None) | (_, None, _) => (empty(StatusCode::BAD_REQUEST), None),
        (_, Some(Ok(route)), Some(id)) => match route.as_ref().and_then(|r| r.upstreams.first()) {
            None => (empty(StatusCode::NOT_FOUND), None),
            Some(up) => match &up.addr {
                Target::Vsock { socket, port } => match dial_vsock(socket, *port).await {
                    Ok(stream) => {
                        tokio::spawn(pipe(req, stream));
                        (empty(StatusCode::OK), Some(up.addr.clone()))
                    }
                    Err(e) => {
                        warn!(instance = %id, error = %e, "CONNECT");
                        (empty(StatusCode::BAD_GATEWAY), Some(up.addr.clone()))
                    }
                },
                Target::Tcp(_) => (empty(StatusCode::BAD_GATEWAY), Some(up.addr.clone())),
            },
        },
    };
    let vm = vm.map_or("-".into(), |a| a.to_string());
    info!(target = %target.as_deref().unwrap_or("-"), status = resp.status().as_u16(), %vm, %peer, ms = t0.elapsed().as_millis() as u64, "CONNECT");
    Ok(resp)
}

/// After the 200 goes out, pipe the upgraded backhaul stream and the VM connection until either closes.
pub(crate) async fn pipe<S: AsyncRead + AsyncWrite + Unpin>(req: Request<Incoming>, mut vm: S) {
    match hyper::upgrade::on(req).await {
        Ok(up) => {
            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(up), &mut vm).await;
        }
        Err(e) => warn!(error = %e, "upgrade"),
    }
}

/// The backhaul's listener: HTTP/2 (the edges' pooled connections, many CONNECT streams each) or HTTP/1.1
/// (one CONNECT, e.g. curl), behind mTLS on the mesh.
pub(crate) async fn serve_backhaul(listener: TcpListener, tls: Option<tokio_rustls::TlsAcceptor>, state: Arc<BackhaulState>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let _ = stream.set_nodelay(true);
        let (tls, state) = (tls.clone(), state.clone());
        tokio::spawn(async move {
            let svc = service_fn(move |req| backhaul(req, state.clone(), peer));
            let http = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let _ = match tls {
                Some(tls) => match tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, tls.accept(stream)).await {
                    Ok(Ok(s)) => http.serve_connection_with_upgrades(TokioIo::new(s), svc).await,
                    Ok(Err(e)) => return warn!(%peer, error = %e, "tls handshake failed"),
                    Err(_) => return warn!(%peer, "tls handshake timed out"),
                },
                None => http.serve_connection_with_upgrades(TokioIo::new(stream), svc).await,
            };
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::ids;

    #[test]
    fn worker_keys_by_instance_id_and_only_dials_its_own_machines_over_vsock() {
        let state = BackhaulState::new("/run/flyyy".into());
        let row = |id: &str, app: &str, addr: &str| BackhaulRow { id: id.into(), app: app.into(), addr: addr.into() };
        state.snapshot(&BTreeMap::from([
            (1, row("w1-a.http", "web", "vsock:w1-a:80")),
            (2, row("w1-a.db", "web", "vsock:w1-a:5432")),
            (3, row("w1-p0.http", "probe", "vsock:w1-a:80")),
            (4, row("w1-x.http", "web", "10.100.1.10:80")),
            (5, row("w1-y.http", "web", "vsock:../../etc:80")),
            (6, row("w1-z.http", "web", "vsock:w2-a:80")),
            (7, row("w1-q.http", "web", "vsock:w1-a:0")),
        ]));
        assert_eq!(ids(&state.routes, "w1-a.http"), ["w1-a.http /run/flyyy/w1-a.vsock:80"]);
        assert_eq!(ids(&state.routes, "w1-a.db"), ["w1-a.db /run/flyyy/w1-a.vsock:5432"]);
        assert_eq!(ids(&state.routes, "w1-p0.http"), ["w1-p0.http /run/flyyy/w1-a.vsock:80"]);
        assert!(["w1-x.http", "w1-y.http", "w1-z.http", "w1-q.http"].iter().all(|k| state.routes.route(k).unwrap().is_none()));
    }
}
