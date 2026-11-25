//! The edge: HTTP(S) by Host on its listen addresses and the shared IPs, tcp services on each app's own
//! address, and the pollers for stale workers and ingress addresses.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tracing::{info, warn};

use corro_api_types::Statement;
use corro_client::CorrosionApiClient;
use http_body_util::BodyExt;
use hyper::{
    body::Incoming,
    header::{self, HeaderValue},
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use crate::NodeArgs;
use crate::{
    balance::{self, Held},
    control,
    corro::{maybe, query},
    edge_routes::{self, EdgeConfig, EdgeState, Ingress},
    http::{empty, reuseport, serve, serve_state, serve_tls},
    ingress,
    metrics::{self, Counted, Dir},
    pool::Pool,
    states::PoolKind,
    subscription::follow,
    table::{NotReady, Route},
    tls,
    util::now_ms,
    Body,
};

/// Dot-separated DNS labels: [a-z0-9-], none empty or starting/ending with `-`.
pub(crate) fn domain_ok(d: &str) -> bool {
    d.len() <= 200
        && d.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// What an edge polls: the active ingress prefixes, the allocated addresses, and the workers' heartbeats.
pub(crate) const POOLS: &str = "SELECT prefix, kind FROM ip_pools WHERE state = 'active'";
pub(crate) const ALLOCATED: &str = "SELECT ip, app FROM ips WHERE state = 'allocated'";
pub(crate) const HEARTBEATS: &str = "SELECT name, heartbeat_at FROM workers";

/// `proxy edge`'s flags.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Where apps are served by Host (plain HTTP; with --https it only redirects there).
    #[arg(long)]
    listen: SocketAddr,
    /// Its /state.
    #[arg(long)]
    state: SocketAddr,
    /// Its Corrosion agent's API and token.
    #[command(flatten)]
    corrosion: crate::corro::ApiArgs,
    /// Its name, in every answer's x-edge header.
    #[arg(long)]
    name: HeaderValue,
    /// The apps' domain: `<app>.<this>` is an app's vhost.
    #[arg(long, default_value = "flyyy.localhost", value_parser = apps_domain)]
    apps_domain: String,
    /// HTTPS here, with --tls-cert and --tls-key (an already-issued certificate; SIGHUP rereads them).
    #[arg(long, requires_all = ["tls_cert", "tls_key"])]
    https: Option<SocketAddr>,
    /// The certificate chain (PEM), for --https.
    #[arg(long, requires = "https")]
    tls_cert: Option<String>,
    /// Its key (PEM), for --https.
    #[arg(long, requires = "https")]
    tls_key: Option<String>,
    /// Its load counters and metrics for the control plane (`GET /load`, `GET /metrics`), announced in the
    /// `edges` table: on this node's mesh address, mTLS with the control plane's certificate only (with
    /// --single-host, plain on loopback). Unset: the control plane doesn't see this edge's load or metrics.
    #[arg(long)]
    load: Option<SocketAddr>,
    #[command(flatten)]
    node: NodeArgs,
}

fn apps_domain(d: &str) -> Result<String, String> {
    match domain_ok(d) {
        true => Ok(d.to_owned()),
        false => Err("want lowercase DNS labels, e.g. flyyy.example.com".into()),
    }
}

/// `proxy edge`: follow the passing services, serve HTTP(S) by Host on --listen (and the shared IPs), tcp
/// services on each app's own address.
pub async fn run(args: Args) {
    let single_host = args.node.single_host("edge");
    let (listen, state_listen, api) = (args.listen, args.state, args.corrosion.open());
    let https = args.https.zip(args.tls_cert).zip(args.tls_key).map(|((a, c), k)| (a, c, k));
    let tls = args.node.tls_dir.as_deref().map(|d| tls::edge_connector(d).unwrap_or_else(|e| panic!("mesh tls: {e}")));
    let config = EdgeConfig { name: args.name, apps_domain: args.apps_domain, single_host };
    let state = Arc::new(EdgeState::new(config, Pool::new(tls)));
    tokio::spawn(follow(api.clone(), Statement::Simple(edge_routes::SUBSCRIPTION.into()), state.clone()));
    let s = state.clone();
    let debug = TcpListener::bind(state_listen).await.expect("bind state");
    tokio::spawn(serve_state(debug, move || s.state_json()));
    info!(edge = %state.config.name.to_str().unwrap_or("?"), %listen, state = %state_listen, corrosion = %api.addr, "edge started");
    let listen_addr = listen;
    tokio::spawn(tcp_listeners(state.clone()));
    tokio::spawn(poll_stale(api.client(), state.clone()));
    tokio::spawn(poll_ingress(api.client(), state.clone()));
    tokio::spawn(state.metrics.clone().tick());
    if let Some(addr) = args.load {
        // The control plane's address for an edge on its host (`flyyy local-cert`).
        let here = addr.ip() == std::net::IpAddr::V4(crate::wg::CONTROL_IP);
        assert!(crate::wg::backhaul_ok(addr, single_host) || here, "--load {addr}: want this node's mesh address (10.250.x.y), or loopback with --single-host");
        assert!(args.node.tls_dir.is_some() || single_host, "--load {addr}: needs --tls-dir (mTLS) off single-host mode");
        tokio::spawn(serve_load(addr, args.node.tls_dir.clone(), state.clone()));
        // Where it serves apps: its HTTPS address, else its plain HTTP one.
        let serves = https.as_ref().map_or(listen_addr, |(a, _, _)| *a);
        tokio::spawn(announce(api.client(), state.config.name.to_str().unwrap_or("?").to_owned(), addr, serves));
    }
    let listener = reuseport(listen_addr).expect("bind with SO_REUSEPORT");
    let Some((addr, cert, key)) = https else {
        tokio::spawn(shared_listeners(state.clone(), listen_addr, None));
        return serve(listener, move |req, peer, local| edge(req, state.clone(), peer, local, "http")).await;
    };
    let certs = tls::Certs::open(&cert, &key).unwrap_or_else(|e| panic!("tls: {e}"));
    let acceptor = tls::acceptor(certs.clone()).unwrap_or_else(|e| panic!("tls: {e}"));
    tokio::spawn(tls::reload_on_hup(certs));
    let tls_listener = reuseport(addr).expect("bind HTTPS with SO_REUSEPORT");
    info!(https = %addr, domain = %state.config.apps_domain, %cert, %listen, "HTTPS (plain HTTP redirects to it)");
    tokio::spawn(shared_listeners(state.clone(), listen_addr, Some((addr, acceptor.clone()))));
    let s = state.clone();
    tokio::spawn(serve(listener, move |req, _peer, local| redirect(req, addr.port(), s.clone(), local)));
    serve_tls(tls_listener, acceptor, move |req, peer, local| edge(req, state.clone(), peer, local, "https")).await;
}

/// HTTP is for the shared IPs: one arriving on an ingress pool's address (an app's dedicated IP, or one
/// nobody has) is misdirected (421), whatever its Host: that address carries only its app's tcp services.
pub(crate) fn misdirected(state: &EdgeState, local: SocketAddr) -> bool {
    state.ingress.read().unwrap().in_pool(local.ip())
}

/// Plain HTTP when the edge has TLS: a permanent redirect to the same host and path on HTTPS.
pub(crate) async fn redirect(req: Request<Incoming>, https_port: u16, state: Arc<EdgeState>, local: SocketAddr) -> Result<Response<Body>, Infallible> {
    if misdirected(&state, local) {
        info!(method = %req.method(), path = %req.uri().path(), status = 421, local = %local.ip(), "request (HTTP here is for the shared IPs)");
        return Ok(empty(StatusCode::MISDIRECTED_REQUEST));
    }
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(|h| h.rsplit_once(':').map_or(h, |(n, _)| n).to_ascii_lowercase());
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str()).to_owned();
    let resp = match host.filter(|h| domain_ok(h)) {
        // A Host we'd also serve over TLS; anything else (a bad header, an IP) gets no redirect.
        Some(h) if h.ends_with(&format!(".{}", state.config.apps_domain)) => {
            let port = if https_port == 443 { String::new() } else { format!(":{https_port}") };
            let mut r = empty(StatusCode::PERMANENT_REDIRECT);
            r.headers_mut().insert(header::LOCATION, format!("https://{h}{port}{path}").parse().expect("checked host and a URI path"));
            r
        }
        Some(_) => empty(StatusCode::NOT_FOUND),
        None => empty(StatusCode::BAD_REQUEST),
    };
    let mut resp = resp;
    resp.headers_mut().insert("x-edge", state.config.name.clone());
    info!(method = %req.method(), host = %req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("-"), %path, status = resp.status().as_u16(), "redirect to https");
    Ok(resp)
}

pub(crate) async fn edge(
    req: Request<Incoming>,
    state: Arc<EdgeState>,
    peer: SocketAddr,
    local: SocketAddr,
    proto: &'static str,
) -> Result<Response<Body>, Infallible> {
    let t0 = Instant::now();
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(|h| h.rsplit_once(':').map_or(h, |(name, _port)| name).to_ascii_lowercase());
    let (mut resp, note) = match host.as_deref() {
        _ if misdirected(&state, local) => (empty(StatusCode::MISDIRECTED_REQUEST), format!("(HTTP on {} is for the shared IPs)", local.ip())),
        None => (empty(StatusCode::BAD_REQUEST), "-".into()),
        Some(h) => match state.routes.route(h) {
            Err(NotReady) => (empty(StatusCode::SERVICE_UNAVAILABLE), "-".into()),
            Ok(None) => (empty(StatusCode::NOT_FOUND), "-".into()),
            // A routed request counts in its app service's metrics: its status at the headers, its bytes
            // as its bodies end.
            Ok(Some(route)) => match route_series(&state, &route) {
                Some(series) => {
                    let req = req.map(|b| Counted::new(b, series.clone(), Dir::In));
                    let (resp, note, ours) = forward(req, &route, &state, peer, proto).await;
                    series.request(resp.status().as_u16(), ours, t0.elapsed());
                    (resp.map(|b| Counted::new(b, series, Dir::Out).boxed()), note)
                }
                None => (empty(StatusCode::BAD_GATEWAY), "- no instances".into()),
            },
        },
    };
    resp.headers_mut().insert("x-edge", state.config.name.clone());
    info!(%method, host = %host.as_deref().unwrap_or("-"), %path, status = resp.status().as_u16(), ms = t0.elapsed().as_millis() as u64, upstream = %note, "request");
    Ok(resp)
}

/// A route's app service, whose metrics its requests and connections count in (a route is one service's).
fn route_series(state: &EdgeState, route: &Route) -> Option<Arc<metrics::Series>> {
    let up = route.upstreams.first()?;
    Some(state.metrics.series(&up.app, up.id.split_once('.').map_or("", |(_, svc)| svc)))
}

/// For the control plane, on its own address: `GET /load`, this edge's name and every instance's counters
/// (balance.rs); `GET /metrics?app=<name>&n=<buckets>`, the app's traffic here (metrics.rs).
async fn serve_load(addr: SocketAddr, tls_dir: Option<String>, state: Arc<EdgeState>) {
    let listener = TcpListener::bind(addr).await.unwrap_or_else(|e| panic!("load on {addr}: {e}"));
    let f = move |req: Request<Incoming>, _peer: SocketAddr, _local: SocketAddr| {
        let state = state.clone();
        async move {
            let json = |code: StatusCode, v: serde_json::Value| {
                let mut r = Response::new(http_body_util::Full::new(hyper::body::Bytes::from(v.to_string())).map_err(|n| match n {}).boxed());
                *r.status_mut() = code;
                r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
                r
            };
            let edge = state.config.name.to_str().unwrap_or("").to_owned();
            let resp = match (req.method(), req.uri().path()) {
                (&hyper::Method::GET, "/load") => json(StatusCode::OK, serde_json::json!({ "edge": edge, "load": state.load_json() })),
                (&hyper::Method::GET, "/metrics") => match metrics::query(req.uri().query()) {
                    Ok((app, n)) => {
                        let mut v = state.metrics.json(&app, n);
                        v["edge"] = edge.into();
                        json(StatusCode::OK, v)
                    }
                    Err(e) => json(StatusCode::BAD_REQUEST, serde_json::json!({ "error": e })),
                },
                _ => empty(StatusCode::NOT_FOUND),
            };
            Ok::<_, Infallible>(resp)
        }
    };
    match tls_dir {
        Some(dir) => {
            let acceptor = tls::control_only_acceptor(&dir).unwrap_or_else(|e| panic!("load TLS from {dir}: {e}"));
            info!(%addr, "load counters (mTLS, the control plane's certificate only)");
            serve_tls(listener, acceptor, f).await
        }
        None => {
            info!(%addr, "load counters (single-host: plain HTTP on loopback)");
            serve(listener, f).await
        }
    }
}

/// This edge's `edges` row, where its load counters are and where it serves apps: written at start (so a
/// restart with other flags updates it) and every 60 s (a write that failed is retried then).
async fn announce(client: CorrosionApiClient, name: String, load: SocketAddr, listen: SocketAddr) {
    let started_at = crate::util::now_ms();
    loop {
        let stmt = Statement::WithParams(
            "INSERT INTO edges (name, load_addr, started_at, listen) VALUES (?, ?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET load_addr = excluded.load_addr, started_at = excluded.started_at, listen = excluded.listen"
                .into(),
            vec![name.as_str().into(), load.to_string().into(), started_at.into(), listen.to_string().into()],
        );
        if let Err(e) = client.execute(&[stmt], None).await {
            warn!(error = %e, "writing the edges row");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// The instance by load (`EdgeState::order`, balance.rs), then on a failed tunnel the next ones (no request
/// bytes were sent yet); a failed one is ejected for a while. An instance at its hard limit is passed
/// over; when every one is, the request waits up to balance::QUEUE for a slot, then gets 503. The slot is
/// held until the response body ends. Returns the response, a log note (the instance used and the ones
/// that failed), and whether the edge made the response (its own 502 or 503) rather than the instance.
pub(crate) async fn forward<B>(mut req: Request<B>, route: &Route, state: &EdgeState, peer: SocketAddr, proto: &'static str) -> (Response<Body>, String, bool)
where
    B: hyper::body::Body<Data = hyper::body::Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let xff = match req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {}", peer.ip()),
        None => peer.ip().to_string(),
    };
    req.headers_mut().insert("x-forwarded-for", xff.parse().expect("ip is a valid header value"));
    // Ours, not the client's: whether the client reached this edge over TLS.
    req.headers_mut().insert("x-forwarded-proto", HeaderValue::from_static(proto));
    let mut failed = Vec::new();
    let deadline = tokio::time::Instant::now() + balance::QUEUE;
    let freed = state.loads.freed();
    loop {
        // Before looking: a slot given back while we look still wakes us.
        let wake = freed.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let order = state.order(route);
        let mut full = true;
        for up in &order {
            let Some(slot) = state.loads.acquire(up) else { continue };
            full = false;
            let io = match state.pool.tunnel(up).await {
                Ok(io) => io,
                Err(e) => {
                    slot.load.failures.fetch_add(1, Ordering::Relaxed);
                    if e.ejects() {
                        state.eject(&up.id);
                    }
                    failed.push(format!("{}({e})", up.id));
                    continue;
                }
            };
            state.recovered(&up.id);
            let note = format!("{}@{} failed=[{}]", up.id, up.addr, failed.join(" "));
            let Ok((mut sender, conn)) = hyper::client::conn::http1::handshake(io).await else {
                slot.load.failures.fetch_add(1, Ordering::Relaxed);
                return (empty(StatusCode::BAD_GATEWAY), note, true);
            };
            tokio::spawn(conn);
            return match sender.send_request(req).await {
                Ok(resp) => (resp.map(|body| Held { body, slot: Some(slot) }.boxed()), note, false),
                Err(_) => {
                    slot.load.failures.fetch_add(1, Ordering::Relaxed);
                    (empty(StatusCode::BAD_GATEWAY), note, true)
                }
            };
        }
        if !full || order.is_empty() {
            return (empty(StatusCode::BAD_GATEWAY), format!("- failed=[{}]", failed.join(" ")), true);
        }
        // Every instance at its hard limit: wait for a slot, up to the deadline.
        if tokio::time::timeout_at(deadline, wake).await.is_err() {
            state.loads.get(&order[0].id).rejected.fetch_add(1, Ordering::Relaxed);
            return (empty(StatusCode::SERVICE_UNAVAILABLE), format!("- every instance at its hard limit for {} ms", balance::QUEUE.as_millis()), true);
        }
    }
}

/// An edge's tcp services: one SO_REUSEPORT listener per (app's IP, edge port) in the routes, on addresses
/// in an ingress pool only, opened and closed as the routes change. A bind that fails (the AnyIP route not
/// there yet) is retried every 2 s. Each listener serves its own key only: an app's traffic can't arrive
/// through another app's address.
pub(crate) async fn tcp_listeners(state: Arc<EdgeState>) {
    let mut open: HashMap<SocketAddr, tokio::task::JoinHandle<()>> = HashMap::new();
    loop {
        let keys = state.routes.keys();
        let want: std::collections::HashSet<SocketAddr> = {
            let ing = state.ingress.read().unwrap();
            keys.iter().filter_map(|k| k.strip_prefix("tcp:")?.parse::<SocketAddr>().ok()).filter(|a| ing.in_pool(a.ip())).collect()
        };
        open.retain(|addr, task| {
            let keep = want.contains(addr);
            if !keep {
                task.abort();
                info!(%addr, "tcp: closed");
            }
            keep
        });
        let mut failed = false;
        for addr in want {
            if open.contains_key(&addr) {
                continue;
            }
            match reuseport(addr) {
                Ok(l) => {
                    info!(%addr, "tcp: listening");
                    let s = state.clone();
                    open.insert(
                        addr,
                        tokio::spawn(async move {
                            loop {
                                let Ok((conn, peer)) = l.accept().await else { continue };
                                tokio::spawn(tcp_conn(conn, addr, peer, s.clone()));
                            }
                        }),
                    );
                }
                Err(e) => {
                    warn!(%addr, error = %e, "tcp: can't listen");
                    failed = true;
                }
            }
        }
        tokio::select! {
            _ = state.routes.changed.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)), if failed => {}
        }
    }
}

/// One tcp connection on `addr` (an app's IP and edge port): tunnel to an instance by load (as `forward`,
/// ejecting failed ones), then pipe bytes, holding its slot for as long as the connection lasts. When every
/// instance is at its hard limit for balance::QUEUE, the connection is closed.
pub(crate) async fn tcp_conn(mut client: TcpStream, addr: SocketAddr, peer: SocketAddr, state: Arc<EdgeState>) {
    let t0 = Instant::now();
    let Ok(Some(route)) = state.routes.route(&format!("tcp:{addr}")) else { return };
    let series = route_series(&state, &route);
    let _open = series.as_ref().map(|s| s.opened());
    let mut failed = Vec::new();
    let deadline = tokio::time::Instant::now() + balance::QUEUE;
    let freed = state.loads.freed();
    loop {
        let wake = freed.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let order = state.order(&route);
        let mut full = true;
        for up in &order {
            let Some(slot) = state.loads.acquire(up) else { continue };
            full = false;
            match state.pool.tunnel(up).await {
                Ok(io) => {
                    state.recovered(&up.id);
                    let r = tokio::io::copy_bidirectional(&mut client, &mut TokioIo::new(io)).await;
                    drop(slot);
                    // shortcut: a connection's bytes count when it closes (in that bucket), and not at all
                    // when it ends in an error.
                    if let (Some(s), Ok((to_app, to_client))) = (&series, &r) {
                        s.bytes(*to_app, *to_client);
                    }
                    info!(%addr, instance = %up.id, %peer, failed = %failed.join(" "), bytes = ?r, ms = t0.elapsed().as_millis() as u64, "TCP");
                    return;
                }
                Err(e) => {
                    slot.load.failures.fetch_add(1, Ordering::Relaxed);
                    if e.ejects() {
                        state.eject(&up.id);
                    }
                    failed.push(format!("{}({e})", up.id));
                }
            }
        }
        if !full || order.is_empty() {
            warn!(%addr, %peer, failed = %failed.join(" "), "TCP: no instance");
            return;
        }
        if tokio::time::timeout_at(deadline, wake).await.is_err() {
            state.loads.get(&order[0].id).rejected.fetch_add(1, Ordering::Relaxed);
            warn!(%addr, %peer, waited_ms = balance::QUEUE.as_millis() as u64, "TCP: closed, every instance at its hard limit");
            return;
        }
    }
}

/// HTTP (and HTTPS) on each shared IP as well, on the ports of the edge's own listen addresses: plain
/// HTTP redirects when there is HTTPS, as on those. Not when the edge already listens on that IP or on a
/// wildcard. A bind that fails (no AnyIP route yet) is retried every 2 s.
pub(crate) async fn shared_listeners(state: Arc<EdgeState>, listen: SocketAddr, https: Option<(SocketAddr, tokio_rustls::TlsAcceptor)>) {
    let mut open: HashMap<SocketAddr, tokio::task::JoinHandle<()>> = HashMap::new();
    let own = |ip: std::net::IpAddr| {
        listen.ip().is_unspecified() || ip == listen.ip() || https.as_ref().is_some_and(|(a, _)| a.ip().is_unspecified() || a.ip() == ip)
    };
    loop {
        let shared: Vec<std::net::IpAddr> = state.ingress.read().unwrap().shared.iter().map(|ip| (*ip).into()).filter(|ip| !own(*ip)).collect();
        let mut want: Vec<(SocketAddr, bool)> = shared.iter().map(|ip| (SocketAddr::new(*ip, listen.port()), false)).collect();
        if let Some((a, _)) = &https {
            want.extend(shared.iter().map(|ip| (SocketAddr::new(*ip, a.port()), true)));
        }
        open.retain(|addr, task| {
            let keep = want.iter().any(|(w, _)| w == addr);
            if !keep {
                task.abort();
                info!(%addr, "shared listener closed");
            }
            keep
        });
        let mut failed = false;
        for (addr, tls) in want {
            if open.contains_key(&addr) {
                continue;
            }
            let l = match reuseport(addr) {
                Ok(l) => l,
                Err(e) => {
                    warn!(%addr, error = %e, "can't listen on a shared IP");
                    failed = true;
                    continue;
                }
            };
            let s = state.clone();
            let task = match (&https, tls) {
                (Some((_, acceptor)), true) => tokio::spawn(serve_tls(l, acceptor.clone(), move |req, peer, local| edge(req, s.clone(), peer, local, "https"))),
                (Some((a, _)), false) => {
                    let port = a.port();
                    tokio::spawn(serve(l, move |req, _peer, local| redirect(req, port, s.clone(), local)))
                }
                (None, _) => tokio::spawn(serve(l, move |req, peer, local| edge(req, s.clone(), peer, local, "http"))),
            };
            info!(%addr, protocol = %if tls { "HTTPS" } else { "HTTP" }, "listening on a shared IP");
            open.insert(addr, task);
        }
        tokio::select! {
            _ = state.ingress_changed.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)), if failed => {}
        }
    }
}

/// An edge's ingress view, polled every 2 s (it changes only on deploy and destroy): active prefixes from
/// ip_pools, allocated addresses from ips. Rows are checked, never trusted: a bad one is skipped.
pub(crate) async fn poll_ingress(client: CorrosionApiClient, state: Arc<EdgeState>) {
    let pools = Statement::Simple(POOLS.into());
    let ips = Statement::Simple(ALLOCATED.into());
    loop {
        // Text columns, each row parsed on its own: a bad one is skipped, not the others.
        match (query::<PoolRow>(&client, pools.clone()).await, query::<(String, String)>(&client, ips.clone()).await) {
            (Ok(p), Ok(i)) => {
                let mut ing = Ingress::default();
                for PoolRow(prefix, kind) in p {
                    match (ingress::Prefix::parse(&prefix), kind) {
                        (Ok(pre), Some(PoolKind::Shared)) if pre.len == 32 => ing.shared.push(pre.net),
                        (Ok(pre), Some(PoolKind::Pool)) => ing.pools.push(pre),
                        _ => warn!(prefix = ?prefix, kind = ?kind, "ingress: skipping an ip_pools row"),
                    }
                }
                for (ip, app) in i {
                    match ip.parse::<std::net::Ipv4Addr>() {
                        Ok(addr) if ing.in_pool(addr.into()) => {
                            ing.owners.insert(addr, app);
                        }
                        _ => warn!(ip = ?ip, app = ?app, "ingress: skipping an ips row"),
                    }
                }
                let changed = *state.ingress.read().unwrap() != ing;
                if changed {
                    info!(shared = ?ing.shared, pools = ?ing.pools.iter().map(|p| p.to_string()).collect::<Vec<_>>(), allocated = ing.owners.len(), "ingress");
                    *state.ingress.write().unwrap() = ing;
                    state.routes.changed.notify_one();
                    state.ingress_changed.notify_one();
                }
            }
            (Err(e), _) | (_, Err(e)) => warn!(error = %e, "ingress: reading"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// An `ip_pools` row as gossip has it: its prefix as text, its kind (None if it isn't one of ours).
#[derive(serde::Deserialize)]
struct PoolRow(String, #[serde(deserialize_with = "maybe")] Option<PoolKind>);

/// A `workers` row's name and heartbeat, as gossip has it (an off-type heartbeat: None, that row skipped).
#[derive(serde::Deserialize)]
struct WorkerSeen(String, #[serde(deserialize_with = "maybe")] Option<i64>);

/// An edge's view of which workers are stale (heartbeat older than the control plane's liveness window),
/// polled every 5 s: selecting heartbeat_at in the subscription would change its rows on every heartbeat.
pub(crate) async fn poll_stale(client: CorrosionApiClient, state: Arc<EdgeState>) {
    let stmt = Statement::Simple(HEARTBEATS.into());
    loop {
        match query::<WorkerSeen>(&client, stmt.clone()).await {
            Ok(rows) => {
                let now = now_ms();
                let stale = rows.into_iter().filter_map(|WorkerSeen(name, hb)| (now - hb? > control::ALIVE_MS).then_some(name));
                *state.stale.write().unwrap() = stale.collect();
            }
            Err(e) => warn!(error = %e, "stale workers: reading"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge_routes::test_edge;

    #[test]
    fn edge_flags_and_apps_domains() {
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            args: Args,
        }
        let base = "edge --listen 127.0.0.1:80 --state 127.0.0.1:9101 --corrosion 127.0.0.1:8101 --corrosion-token-file t --name edge1";
        let f = |extra: &str| <T as clap::Parser>::try_parse_from(format!("{base} {extra}").split_whitespace()).map(|t| t.args);
        let o = f("").unwrap();
        assert!(o.apps_domain == "flyyy.localhost" && o.https.is_none() && !o.node.single_host);
        let o = f("--apps-domain flyyy.devpeek.io --https 0.0.0.0:443 --tls-cert c.pem --tls-key k.pem").unwrap();
        assert_eq!(
            (o.apps_domain.as_str(), o.https, o.tls_cert.as_deref(), o.tls_key.as_deref()),
            ("flyyy.devpeek.io", Some("0.0.0.0:443".parse().unwrap()), Some("c.pem"), Some("k.pem"))
        );
        for bad in [
            "--https 0.0.0.0:443",
            "--tls-cert c --tls-key k",
            "--apps-domain Flyyy.io",
            "--apps-domain a..b",
            "--apps-domain -a.b",
            "--https nope --tls-cert c --tls-key k",
            "--bogus x",
            "--apps-domain",
            "--apps-domain a --apps-domain b",
        ] {
            assert!(f(bad).is_err(), "{bad}");
        }
        assert!(domain_ok("flyyy.localhost") && !domain_ok("flyyy.localhost.") && !domain_ok("a/b") && !domain_ok(""));
    }

    #[test]
    fn http_only_off_the_pools() {
        let state = test_edge();
        *state.ingress.write().unwrap() = Ingress {
            shared: vec!["10.200.0.1".parse().unwrap()],
            pools: vec![ingress::Prefix::parse("10.200.1.0/24").unwrap()],
            owners: HashMap::from([("10.200.1.7".parse().unwrap(), "db".into())]),
        };
        let at = |s: &str| SocketAddr::new(s.parse().unwrap(), 443);
        // An app's own address, one nobody has: tcp only. Shared IPs and anything else (127.0.0.1): by Host.
        assert!(misdirected(&state, at("10.200.1.7")) && misdirected(&state, at("10.200.1.200")));
        assert!(!misdirected(&state, at("10.200.0.1")) && !misdirected(&state, at("127.0.0.1")));
    }
}
