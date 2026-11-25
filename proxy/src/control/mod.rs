//! `proxy control`: the control plane.
//! An HTTP/JSON API over its own store of desired state (db.rs: apps, machines, orgs, users, IP leases),
//! and one level-triggered reconciler that turns each app's desired state (count) into machines and
//! sends each worker its own over the agent API (agents.rs). Corrosion carries only facts: what the
//! workers report (machine_status, service_status, images, their `workers` rows with a heartbeat; only
//! live ones get new machines), and what this publishes (the ingress addresses). The store is written
//! before anything else sees a change, so a kill -9 at any point just resumes.
//! It also runs the WireGuard mesh (mesh.rs): the admin API on `--listen` and, once the mesh exists, on
//! 10.250.0.1:4000 too, where the mesh's own routes (join, peers) know callers by their tunnel address.
mod agents;
mod api;
mod apps;
mod auth;
mod db;
mod events;
pub(crate) mod load;
mod mesh_routes;
mod metrics;
pub(crate) mod model;
mod orgs;
mod placement;
mod publish;
mod reconcile;
mod routes;
pub(crate) mod store;
#[cfg(test)]
mod tests;
mod workers;

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use corro_client::CorrosionApiClient;
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, header, Response, StatusCode};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::Notify};

use crate::{
    api_error::ApiError,
    control::{agents::Agents, api::api, auth::OperatorToken, db::Db},
    ingress::{self, Lease, Prefix},
    mesh::{Answer, Mesh},
    serve, wg, Body,
};

pub(crate) const MAX_COUNT: u32 = 20;

/// A worker is alive while its heartbeat (every 5 s) is newer than this, by our clock.
// shortcut: one host, one clock; across hosts this needs skew handling or a heartbeat counter.
pub(crate) const ALIVE_MS: i64 = 30_000;

pub(crate) const MAX_BODY: usize = 16 * 1024;

pub(crate) struct Control {
    /// Its local Corrosion agent: the facts, and what it publishes.
    client: CorrosionApiClient,
    /// The desired state.
    db: Db,
    /// Its client of the workers' agent APIs.
    agents: Agents,
    /// What it last wrote to `ips`, `placement` and `backhauls` (empty after a start, so all are written once).
    published_leases: Mutex<HashMap<Ipv4Addr, Lease>>,
    published_placement: Mutex<HashMap<String, publish::Placed>>,
    published_backhauls: Mutex<HashMap<String, String>>,
    /// Each app's last load sample (load.rs), for its rates.
    load_samples: Mutex<HashMap<String, load::Sample>>,
    /// Each app's recent events (events.rs).
    events: events::Events,
    token: OperatorToken,
    wake: Notify,
    mesh: Arc<Mesh>,
    /// Its `--shared-ip`s and `--ip-pool`s (ingress.rs), written to `ip_pools` once Corrosion takes them.
    shared: Vec<Ipv4Addr>,
    pools: Vec<Prefix>,
    pools_written: std::sync::atomic::AtomicBool,
    /// `--single-host` (dev): loopback backhauls count as valid workers, and there's no mesh.
    single_host: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Join {
    name: String,
    key: String,
    role: String,
    /// The WireGuard endpoint the node advertises (behind NAT: its forwarded address), given to the other
    /// nodes instead of the one the control plane learns from its packets.
    #[serde(default)]
    endpoint: Option<String>,
    /// The node's host's name, for the dashboard (`wg::hostname_ok`).
    #[serde(default)]
    hostname: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Csr {
    csr: String,
}

/// `POST /v1/mesh/local`: an edge on this host, by its join token (`flyyy local-cert`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalCert {
    token: String,
    name: String,
    role: String,
    csr: String,
}

/// A mesh route's answer as the API's.
pub(crate) fn answer(a: Answer) -> Result<Response<Body>, ApiError> {
    a.map(|v| json(StatusCode::OK, v))
}

pub(crate) fn json(code: StatusCode, v: serde_json::Value) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(v.to_string())).map_err(|n| match n {}).boxed());
    *r.status_mut() = code;
    r.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().expect("static"));
    r
}

/// A request refused with this status and message (answered at the top, api::api).
pub(crate) fn error(code: StatusCode, msg: impl std::fmt::Display) -> ApiError {
    ApiError::new(code, msg)
}

/// `proxy control`'s flags.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// The API (and the dashboard's forwarder's target), on this host.
    #[arg(long)]
    listen: SocketAddr,
    /// Its Corrosion agent's API and token.
    #[command(flatten)]
    corrosion: crate::corro::ApiArgs,
    /// The operator's bearer token (at least 32 characters).
    #[arg(long)]
    token_file: String,
    /// The control plane's state: the desired state (store.json), and the WireGuard mesh's keys, tokens,
    /// nodes and CA.
    #[arg(long)]
    mesh_dir: String,
    /// Where HTTP is served by Host (repeatable).
    #[arg(long)]
    shared_ip: Vec<Ipv4Addr>,
    /// Where each app's own address comes from (repeatable; at most a /16 each, no overlaps).
    #[arg(long, value_parser = ingress::pool)]
    ip_pool: Vec<Prefix>,
    /// How long a node's mesh certificate is valid, in seconds; nodes renew after half of it.
    #[arg(long, env = "FLYYY_CERT_LIFETIME", default_value_t = 86_400, value_parser = clap::value_parser!(u64).range(60..))]
    cert_lifetime: u64,
    /// Where nodes reach this host's WireGuard: host or host:port (default port 51820). Every join token
    /// carries it, so a token is all `flyyy join` needs.
    #[arg(long, value_parser = control_endpoint)]
    endpoint: Option<String>,
    /// Make the mesh at start when there's none yet, without a join token (the #control app; `flyyy
    /// init` makes it and a token).
    #[arg(long, conflicts_with = "single_host")]
    create_mesh: bool,
    #[command(flatten)]
    node: crate::NodeArgs,
}

fn control_endpoint(s: &str) -> Result<String, String> {
    let e = if crate::wg::endpoint_ok(s) { s.to_owned() } else { format!("{s}:{}", crate::wg::PORT) };
    match crate::wg::endpoint_ok(&e) {
        true => Ok(e),
        false => Err("want <host>[:<port>]: an IPv4 address, [<IPv6>] or a name".into()),
    }
}

pub async fn main(args: Args) {
    let single_host = args.node.single_host;
    let (listen, corro, token_file, mesh_dir) = (args.listen, args.corrosion.open(), &args.token_file, &args.mesh_dir);
    ingress::pools_ok(&args.ip_pool).unwrap_or_else(|e| panic!("proxy control: {e}"));
    let (shared, pools) = (args.shared_ip, args.ip_pool);
    let token = std::fs::read_to_string(token_file).unwrap_or_else(|e| panic!("token file {token_file}: {e}")).trim().to_owned();
    assert!(token.len() >= 32, "token in {token_file} is too short (want >= 32 chars)");
    let mesh = Mesh::open(mesh_dir, args.node.tls_dir.clone(), Duration::from_secs(args.cert_lifetime), args.endpoint)
        .await
        .unwrap_or_else(|e| panic!("mesh in {mesh_dir}: {e}"));
    if args.create_mesh && !mesh.exists() {
        mesh.create().await.unwrap_or_else(|e| panic!("creating the mesh in {mesh_dir}: {e}"));
    }
    let db = Db::open(&format!("{mesh_dir}/store.json")).unwrap_or_else(|e| panic!("desired state: {e}"));
    let control = Arc::new(Control {
        client: corro.client(),
        db,
        agents: Agents::new(args.node.tls_dir.clone()),
        published_leases: Mutex::default(),
        published_placement: Mutex::default(),
        published_backhauls: Mutex::default(),
        load_samples: Mutex::default(),
        events: events::Events::default(),
        token: OperatorToken::new(&token).unwrap_or_else(|e| panic!("operator token: {e}")),
        wake: Notify::new(),
        mesh: Arc::new(mesh),
        shared,
        pools,
        pools_written: Default::default(),
        single_host,
    });
    // Single-host (dev): never alongside a mesh. Real deployments are mesh-only, and a loopback row there
    // would be a worker nobody can reach.
    if single_host {
        assert!(!control.mesh.exists(), "proxy control: refusing --single-host: a mesh exists in {mesh_dir} (single-host is for one machine without one)");
        tracing::info!("single-host (dev) mode: loopback backhauls accepted, no mesh");
    }
    tokio::spawn(control.clone().run());
    tokio::spawn(control.mesh.clone().run());
    tokio::spawn(mesh_listener(control.clone()));
    let listener = TcpListener::bind(listen).await.expect("bind");
    tracing::info!(%listen, corrosion = %corro.addr, "control plane started");
    // shortcut: one static bearer token, over plaintext HTTP (inside WireGuard on the mesh).
    serve(listener, move |req, peer, _| api(req, control.clone(), peer, false)).await;
}

/// The API on the control plane's mesh address, once there is one (`flyyy init` makes it).
pub(crate) async fn mesh_listener(control: Arc<Control>) {
    let addr = SocketAddr::from((wg::CONTROL_IP, wg::CONTROL_API_PORT));
    let listener = loop {
        if control.mesh.exists() {
            match TcpListener::bind(addr).await {
                Ok(l) => break l,
                Err(e) => tracing::warn!(%addr, error = %e, "mesh listener"),
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    tracing::info!(%addr, "control plane on the mesh");
    serve(listener, move |req, peer, _| api(req, control.clone(), peer, true)).await;
}
