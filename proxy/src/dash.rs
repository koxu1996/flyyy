//! The web dashboard (dashboard.html, embedded) and its forwarder. The page's API calls come here, same
//! origin, and only what it needs goes on to the control plane: /v1/apps[/<app>], GET /v1/apps/<app>/load
//! and /metrics (its ?window only), GET /v1/workers, and the mesh's list, create, tokens and approve.
//! Two modes:
//! - `flyyy dashboard`: on loopback, acting with the CLI's token (added here; it never reaches the browser).
//!   The threat is any site the user visits, which can make requests to 127.0.0.1:4040: DNS rebinding (a page
//!   on evil.example whose name now resolves to 127.0.0.1) is stopped by the Host check (this listener's own
//!   loopback name and port), CSRF (a cross-site write, such as approving the attacker's node) by wanting
//!   this listener's Origin and a JSON body on every write, which a cross-site request can't send without a
//!   CORS preflight, and nothing here answers one.
//! - `proxy dashboard`: public, over HTTPS, holding no token. The page signs in with the user's own token
//!   (the operator's or an org user's, kept in the tab's sessionStorage), and every call carries it in
//!   Authorization, passed on as it is to the control plane over the mesh, which decides what it may see.
//!   The Host must be the public name; writes need its https Origin and JSON, as above. 401s are limited per
//!   source address (guessing tokens), and requests and their bodies are bounded.
//!
//! Both send a strict CSP: the page's inline script and style by hash, requests to this origin only, no
//! framing, no forms, no base.
// shortcut (loopback): no auth between the browser and this process; anything local that speaks HTTP to it
// acts with the CLI's token (as it could by reading the token file).
use std::{
    collections::HashMap,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Bytes,
    header::{self, HeaderValue},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;

use crate::client::Client;

const PAGE: &str = include_str!("dashboard.html");
/// The "New app" sheet's ready-made apps (the page fetches them): pinned images, sizes and env that work.
// shortcut: built in; a file of the operator's own once people want theirs.
const TEMPLATES: &str = include_str!("templates.json");
/// The biggest request body forwarded (a deploy is a few KiB; the control plane takes 16 KiB).
const MAX_BODY: usize = 64 * 1024;
/// The most a request's head may take (hyper's read buffer), and how long it may take to arrive.
const MAX_HEAD: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// 401s per source address: at most this many in WINDOW, then 429 until the window is over.
const FAILS: u32 = 10;
const WINDOW: Duration = Duration::from_secs(60);
/// A bearer token as the control plane makes them (64 hex, behind flyyy_ for users): bounded here.
const MAX_TOKEN: usize = 256;

/// Allowed Host headers: this listener's loopback names, with its port.
pub(crate) fn host_ok(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")].iter().any(|ok| h.eq_ignore_ascii_case(ok)))
}

/// The public name, as browsers send it (with :443 or without a port).
pub(crate) fn public_host_ok(host: Option<&str>, name: &str) -> bool {
    host.is_some_and(|h| h.eq_ignore_ascii_case(name) || h.eq_ignore_ascii_case(&format!("{name}:443")))
}

/// What the page may reach on the control plane: (method, path, query) after this listener; the query
/// it forwards, if any (only /metrics' window).
pub(crate) fn forwarded(method: &Method, path: &str, query: Option<&str>) -> Option<String> {
    let app_ok = |n: &str| !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let node_ok = |n: &str| !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let plain = |ok: bool| ok.then(|| path.to_owned());
    match (method, path) {
        (&Method::GET, "/v1/apps" | "/v1/workers" | "/v1/mesh" | "/v1/whoami" | "/v1/orgs" | "/v1/users") => plain(true),
        (&Method::POST, "/v1/orgs" | "/v1/users") => plain(true),
        (&Method::DELETE, p) if p.starts_with("/v1/orgs/") => plain(p.strip_prefix("/v1/orgs/").is_some_and(app_ok)),
        (&Method::POST, "/v1/mesh" | "/v1/mesh/tokens") => plain(true),
        (&Method::GET | &Method::PUT | &Method::PATCH | &Method::DELETE, p) if p.strip_prefix("/v1/apps/").is_some_and(app_ok) => plain(true),
        (&Method::GET, p) if p.strip_prefix("/v1/apps/").and_then(|r| r.strip_suffix("/load")).is_some_and(app_ok) => plain(true),
        (&Method::GET, p) if p.strip_prefix("/v1/apps/").and_then(|r| r.strip_suffix("/events")).is_some_and(app_ok) => plain(true),
        // A machine's logs: the control plane checks the id is the app's.
        (&Method::GET, p)
            if p.strip_prefix("/v1/apps/")
                .and_then(|r| r.strip_suffix("/logs"))
                .and_then(|r| r.split_once("/machines/"))
                .is_some_and(|(a, id)| app_ok(a) && node_ok(id)) =>
        {
            plain(true)
        }
        (&Method::GET, p) if p.strip_prefix("/v1/apps/").and_then(|r| r.strip_suffix("/metrics")).is_some_and(app_ok) => {
            match query.unwrap_or("").split('&').find_map(|kv| kv.strip_prefix("window=")) {
                None => Some(path.to_owned()),
                Some(w @ ("5m" | "1h")) => Some(format!("{path}?window={w}")),
                Some(_) => None,
            }
        }
        (&Method::POST, p) => plain(p.strip_prefix("/v1/mesh/nodes/").and_then(|r| r.strip_suffix("/approve")).is_some_and(node_ok)),
        _ => None,
    }
}

/// A write must come from the page itself: Origin is this listener (`origin`), and the body JSON.
pub(crate) fn write_ok(method: &Method, origin: Option<&str>, own: &str, content_type: Option<&str>) -> bool {
    if matches!(*method, Method::GET | Method::HEAD) {
        return true;
    }
    let json = content_type.is_some_and(|c| c.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json")));
    origin.is_some_and(|o| o.eq_ignore_ascii_case(own)) && json
}

/// `Authorization: Bearer <token>` as the page sends it: a token of token68 characters, bounded. Passed
/// on as it is; anything else is a 401 here.
pub(crate) fn bearer(value: Option<&str>) -> Option<&str> {
    let t = value?.strip_prefix("Bearer ")?;
    (!t.is_empty() && t.len() <= MAX_TOKEN && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))).then_some(value?)
}

/// 401s by source address, in fixed windows.
#[derive(Default)]
pub(crate) struct Limiter {
    by_ip: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Limiter {
    pub(crate) fn limited(&self, ip: IpAddr, now: Instant) -> bool {
        self.by_ip.lock().unwrap().get(&ip).is_some_and(|(start, n)| now.duration_since(*start) < WINDOW && *n >= FAILS)
    }

    pub(crate) fn failed(&self, ip: IpAddr, now: Instant) {
        let mut by_ip = self.by_ip.lock().unwrap();
        // Bounded: old windows go first.
        if by_ip.len() > 10_000 {
            by_ip.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
        }
        let e = by_ip.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= WINDOW {
            *e = (now, 0);
        }
        e.1 += 1;
    }
}

/// `'sha256-<base64>'` of a CSP source: the page's inline script and style are allowed by these alone.
fn csp_hash(s: &str) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let d = ring::digest::digest(&ring::digest::SHA256, s.as_bytes());
    let mut out = String::new();
    for c in d.as_ref().chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    format!("'sha256-{out}'")
}

/// The text between `open` and `close` in the page: its one inline script, its one inline style.
fn inline(open: &str, close: &str) -> &'static str {
    let start = PAGE.find(open).expect("the page has it") + open.len();
    &PAGE[start..start + PAGE[start..].find(close).expect("closed")]
}

/// The CSP: the page's script and stylesheet by hash, style attributes (presentation only, no script) inline,
/// and the IBM Plex fonts from Google Fonts (a stylesheet and its font files; without them the page falls back
/// to system fonts).
// shortcut: each visit fetches the fonts from Google; self-hosting them would keep the dashboard to itself.
fn csp() -> &'static str {
    static CSP: OnceLock<String> = OnceLock::new();
    CSP.get_or_init(|| {
        let (script, style) = (csp_hash(inline("<script>", "</script>")), csp_hash(inline("<style>", "</style>")));
        format!(
            "default-src 'none'; script-src {script}; style-src-elem {style} https://fonts.googleapis.com; style-src-attr 'unsafe-inline'; \
             font-src https://fonts.gstatic.com; img-src data:; connect-src 'self'; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        )
    })
}

fn answer(code: StatusCode, content_type: &str, body: impl Into<Bytes>, public: bool) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = code;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, content_type.parse().expect("static"));
    h.insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().expect("static"));
    h.insert(header::REFERRER_POLICY, "no-referrer".parse().expect("static"));
    h.insert(header::CONTENT_SECURITY_POLICY, csp().parse().expect("ASCII"));
    if public {
        h.insert(header::STRICT_TRANSPORT_SECURITY, "max-age=31536000".parse().expect("static"));
    }
    r
}

fn refuse(code: StatusCode, msg: &str, public: bool) -> Response<Full<Bytes>> {
    answer(code, "application/json", serde_json::json!({ "error": msg }).to_string(), public)
}

/// Which dashboard this is.
#[derive(Clone)]
enum Mode {
    /// `flyyy dashboard`: the CLI's client, with its token; this listener's port.
    Local { client: Arc<Client>, port: u16 },
    /// `proxy dashboard`: the control plane's client, no token (each request's own); the public name.
    Public { client: Arc<Client>, host: String, limiter: Arc<Limiter> },
}

async fn handle(req: Request<hyper::body::Incoming>, mode: Mode, peer: IpAddr, config: Arc<String>) -> Result<Response<Full<Bytes>>, Infallible> {
    let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), req.uri().query().map(str::to_owned));
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_owned);
    let public = matches!(mode, Mode::Public { .. });
    let resp = 'r: {
        let origin = match &mode {
            Mode::Local { port, .. } if !host_ok(host.as_deref(), *port) => {
                break 'r refuse(
                    StatusCode::MISDIRECTED_REQUEST,
                    &format!("Host {:?}: want 127.0.0.1:{port} or localhost:{port}", host.as_deref().unwrap_or("")),
                    public,
                )
            }
            Mode::Public { host: name, .. } if !public_host_ok(host.as_deref(), name) => {
                break 'r refuse(StatusCode::MISDIRECTED_REQUEST, &format!("Host {:?}: want {name}", host.as_deref().unwrap_or("")), public)
            }
            Mode::Local { .. } => format!("http://{}", host.as_deref().unwrap_or_default()),
            Mode::Public { host: name, .. } => format!("https://{name}"),
        };
        match (&method, path.as_str()) {
            (&Method::GET, "/") => break 'r answer(StatusCode::OK, "text/html; charset=utf-8", PAGE, public),
            (&Method::GET, "/templates.json") => break 'r answer(StatusCode::OK, "application/json", TEMPLATES, public),
            (&Method::GET, "/config.json") => break 'r answer(StatusCode::OK, "application/json", config.to_string(), public),
            _ => {}
        }
        let Some(target) = forwarded(&method, &path, query.as_deref()) else {
            break 'r refuse(StatusCode::NOT_FOUND, "not forwarded to the control plane", public);
        };
        let header_of = |h: header::HeaderName| req.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if !write_ok(&method, header_of(header::ORIGIN).as_deref(), &origin, header_of(header::CONTENT_TYPE).as_deref()) {
            break 'r refuse(StatusCode::FORBIDDEN, "writes come from the dashboard page only (Origin, JSON)", public);
        }
        let now = Instant::now();
        let (client, auth) = match &mode {
            Mode::Local { client, .. } => (client.clone(), None),
            Mode::Public { client, limiter, .. } => {
                if limiter.limited(peer, now) {
                    break 'r refuse(StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins from here; wait a minute", public);
                }
                match bearer(header_of(header::AUTHORIZATION).as_deref()).and_then(|v| HeaderValue::from_str(v).ok()) {
                    Some(v) => (client.clone(), Some(v)),
                    None => {
                        limiter.failed(peer, now);
                        break 'r refuse(StatusCode::UNAUTHORIZED, "sign in: missing or malformed bearer token", public);
                    }
                }
            }
        };
        let body = match Limited::new(req.into_body(), MAX_BODY).collect().await {
            Ok(b) => b.to_bytes(),
            Err(e) => break 'r refuse(StatusCode::PAYLOAD_TOO_LARGE, &format!("body: {e} (at most {MAX_BODY} bytes)"), public),
        };
        match client.raw_as(method.clone(), &target, body, auth).await {
            Ok((code, bytes)) => {
                if let (StatusCode::UNAUTHORIZED, Mode::Public { limiter, .. }) = (code, &mode) {
                    limiter.failed(peer, now);
                }
                answer(code, "application/json", bytes, public)
            }
            Err(e) => refuse(StatusCode::BAD_GATEWAY, &e, public),
        }
    };
    tracing::info!(%method, %path, status = resp.status().as_u16(), %peer, "dashboard request");
    Ok(resp)
}

fn apps_url_ok(apps_url: &str) -> Result<(), String> {
    match (apps_url.starts_with("http://") || apps_url.starts_with("https://")) && apps_url.contains("{app}") {
        true => Ok(()),
        false => Err(format!("--apps-url {apps_url:?}: want an http(s) URL with {{app}} in it, e.g. https://{{app}}.flyyy.example.com/")),
    }
}

/// Where the join dialog's `nix run '<this>#join'` gets flyyy from, unless --flake-url says otherwise
/// (nix/modules/lib.nix has the same default, for the module's option).
pub const FLAKE_URL: &str = "git+https://github.com/koxu1996/flyyy?ref=main";

/// The page's /config.json: where app links go, how it signs in, and the flake the join command runs.
fn config(apps_url: &str, flake_url: &str, auth: &str) -> Result<Arc<String>, String> {
    apps_url_ok(apps_url)?;
    // It goes inside single quotes in a shell command the operator copies.
    if flake_url.is_empty() || flake_url.contains(['\'', '#']) || flake_url.chars().any(char::is_whitespace) {
        return Err(format!("--flake-url {flake_url:?}: want a flake URL without quotes, spaces or a #fragment"));
    }
    Ok(Arc::new(serde_json::json!({ "apps_url": apps_url, "flake_url": flake_url, "auth": auth }).to_string()))
}

/// One connection's HTTP/1: its head bounded in size and time, the request's handled by `handle`.
async fn serve_conn<S>(io: S, mode: Mode, peer: IpAddr, config: Arc<String>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(move |req| handle(req, mode.clone(), peer, config.clone()));
    let _ =
        http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(HEAD_TIMEOUT).max_buf_size(MAX_HEAD).serve_connection(TokioIo::new(io), svc).await;
}

/// `flyyy dashboard [--listen 127.0.0.1:4040] [--apps-url URL] [--flake-url URL]`: serve until killed.
pub async fn run(client: Client, addr: SocketAddr, apps_url: &str, flake_url: &str) -> Result<(), String> {
    if !addr.ip().is_loopback() {
        return Err(format!("--listen {addr}: want a loopback address (127.0.0.1 or ::1): the dashboard acts with your token"));
    }
    let config = config(apps_url, flake_url, "local")?;
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("listen on {addr}: {e}"))?;
    let client = Arc::new(client);
    let shown = if addr.ip().is_ipv6() { format!("[::1]:{}", addr.port()) } else { format!("127.0.0.1:{}", addr.port()) };
    tracing::info!(url = %format!("http://{shown}/"), control_plane = %client.addr, %apps_url, "dashboard");
    let mode = Mode::Local { client, port: addr.port() };
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        tokio::spawn(serve_conn(stream, mode.clone(), peer.ip(), config.clone()));
    }
}

/// `proxy dashboard`'s flags.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Where it serves HTTPS.
    #[arg(long)]
    listen: SocketAddr,
    /// Its public name: the only Host it answers, and its writes' Origin (https://<this>).
    #[arg(long, value_parser = public_name)]
    host: String,
    /// The certificate chain (PEM) for it, and its key; SIGHUP rereads them.
    #[arg(long)]
    tls_cert: String,
    #[arg(long)]
    tls_key: String,
    /// The control plane's API, on the mesh (no token here: each request carries its caller's).
    #[arg(long, default_value = "10.250.0.1:4000")]
    control: SocketAddr,
    /// Where the app links go, {app} the app's name.
    #[arg(long)]
    apps_url: String,
    /// The flake the join dialog's command runs (`nix run '<this>#join'`).
    #[arg(long, default_value = FLAKE_URL)]
    flake_url: String,
}

fn public_name(s: &str) -> Result<String, String> {
    match crate::edge::domain_ok(s) {
        true => Ok(s.to_owned()),
        false => Err("want a DNS name, e.g. flyyy.example.com".into()),
    }
}

/// `proxy dashboard`: the public dashboard, over HTTPS, forwarding to the control plane on the mesh.
// shortcut: behind a TCP passthrough (RKE2's ingress) every caller has the proxy's address, so the 401
// limit is one for all of them there; PROXY protocol would give each its own.
pub async fn public(args: Args) {
    let config = config(&args.apps_url, &args.flake_url, "token").unwrap_or_else(|e| panic!("{e}"));
    let certs = crate::tls::Certs::open(&args.tls_cert, &args.tls_key).unwrap_or_else(|e| panic!("tls: {e}"));
    let acceptor = crate::tls::acceptor(certs.clone()).unwrap_or_else(|e| panic!("tls: {e}"));
    tokio::spawn(crate::tls::reload_on_hup(certs));
    let listener = TcpListener::bind(args.listen).await.unwrap_or_else(|e| panic!("listen on {}: {e}", args.listen));
    let client = Arc::new(Client { addr: args.control.to_string(), token: None });
    tracing::info!(url = %format!("https://{}/", args.host), listen = %args.listen, control_plane = %args.control, "public dashboard");
    let mode = Mode::Public { client, host: args.host, limiter: Arc::default() };
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let (acceptor, mode, config) = (acceptor.clone(), mode.clone(), config.clone());
        tokio::spawn(async move {
            match tokio::time::timeout(crate::tls::HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => serve_conn(tls, mode, peer.ip(), config).await,
                Ok(Err(e)) => tracing::warn!(%peer, error = %e, "dashboard: tls handshake failed"),
                Err(_) => tracing::warn!(%peer, "dashboard: tls handshake timed out"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_pinned_and_complete() {
        let groups: Vec<serde_json::Value> = serde_json::from_str(TEMPLATES).unwrap();
        let items: Vec<&serde_json::Value> = groups.iter().flat_map(|g| g["items"].as_array().unwrap()).collect();
        assert!(groups.iter().all(|g| g["id"].is_string() && g["name"].is_string()) && !items.is_empty());
        for t in &items {
            let image = t["image"].as_str().unwrap_or("");
            assert!(t["name"].is_string() && t["port"].as_u64().is_some_and(|p| p > 0) && crate::agent::spec::image_ok(image), "{t}");
        }
    }

    #[test]
    fn hosts_routes_and_writes() {
        assert!(host_ok(Some("127.0.0.1:4040"), 4040) && host_ok(Some("localhost:4040"), 4040) && host_ok(Some("[::1]:4040"), 4040));
        // Rebinding: another name, or no port, or another port.
        for bad in [Some("evil.example:4040"), Some("127.0.0.1"), Some("127.0.0.1:4041"), Some("localhost.evil.example:4040"), None] {
            assert!(!host_ok(bad, 4040), "{bad:?}");
        }
        assert!(public_host_ok(Some("flyyy.example.com"), "flyyy.example.com") && public_host_ok(Some("FLYYY.example.com:443"), "flyyy.example.com"));
        assert!(
            !public_host_ok(Some("evil.example"), "flyyy.example.com")
                && !public_host_ok(Some("flyyy.example.com:8443"), "flyyy.example.com")
                && !public_host_ok(None, "x")
        );
        for (m, p) in [
            (Method::GET, "/v1/apps"),
            (Method::GET, "/v1/apps/blue"),
            (Method::PUT, "/v1/apps/blue"),
            (Method::PATCH, "/v1/apps/blue"),
            (Method::DELETE, "/v1/apps/blue"),
            (Method::GET, "/v1/workers"),
            (Method::GET, "/v1/mesh"),
            (Method::POST, "/v1/mesh"),
            (Method::POST, "/v1/mesh/tokens"),
            (Method::POST, "/v1/mesh/nodes/worker4/approve"),
            (Method::GET, "/v1/apps/blue/load"),
            (Method::GET, "/v1/apps/blue/metrics"),
            (Method::GET, "/v1/apps/blue/events"),
            (Method::GET, "/v1/apps/blue/machines/blue-0a1b2c/logs"),
            (Method::GET, "/v1/whoami"),
            (Method::GET, "/v1/orgs"),
            (Method::POST, "/v1/orgs"),
            (Method::DELETE, "/v1/orgs/acme"),
            (Method::GET, "/v1/users"),
            (Method::POST, "/v1/users"),
        ] {
            assert_eq!(forwarded(&m, p, None).as_deref(), Some(p), "{m} {p}");
        }
        // Only /metrics' window goes on; other queries are dropped; a bad window isn't forwarded.
        assert_eq!(forwarded(&Method::GET, "/v1/apps/blue/metrics", Some("window=1h&x=y")).as_deref(), Some("/v1/apps/blue/metrics?window=1h"));
        assert_eq!(forwarded(&Method::GET, "/v1/apps/blue", Some("a=b")).as_deref(), Some("/v1/apps/blue"));
        assert_eq!(forwarded(&Method::GET, "/v1/apps/blue/metrics", Some("window=1h%26x")), None);
        for (m, p) in [
            (Method::POST, "/v1/workers"),
            (Method::GET, "/v1/orgs/acme"),
            (Method::POST, "/v1/apps/blue/events"),
            (Method::GET, "/v1/apps/blue/machines/../logs"),
            (Method::GET, "/v1/apps/blue/machines/a/b/logs"),
            (Method::GET, "/v1/apps/blue/machines/blue-1"),
            (Method::DELETE, "/v1/orgs"),
            (Method::DELETE, "/v1/orgs/"),
            (Method::DELETE, "/v1/orgs/../apps"),
            (Method::DELETE, "/v1/users"),
            (Method::POST, "/v1/whoami"),
            (Method::GET, "/v1/dashboard"),
            (Method::DELETE, "/v1/apps"),
            (Method::GET, "/v1/apps/blue/x"),
            (Method::GET, "/v1/apps/../orgs"),
            (Method::GET, "/v1/apps/"),
            (Method::POST, "/v1/mesh/nodes/../approve"),
            (Method::POST, "/v1/mesh/nodes/a/b/approve"),
            (Method::GET, "/v1/mesh/tokens"),
            (Method::POST, "/v1/mesh/join"),
            (Method::GET, "/v1/mesh/peers"),
            (Method::PUT, "/v1/apps/blue/load"),
            (Method::GET, "/v1/apps/../load"),
            (Method::POST, "/v1/apps/blue/metrics"),
        ] {
            assert_eq!(forwarded(&m, p, None), None, "{m} {p}");
        }
        let o = "http://127.0.0.1:4040";
        assert!(write_ok(&Method::GET, None, o, None));
        assert!(write_ok(&Method::POST, Some("http://127.0.0.1:4040"), o, Some("application/json")));
        assert!(write_ok(&Method::PUT, Some("http://127.0.0.1:4040"), o, Some("application/json; charset=utf-8")));
        // CSRF: no Origin, another origin, a form's content type; http where https is wanted.
        assert!(!write_ok(&Method::POST, None, o, Some("application/json")));
        assert!(!write_ok(&Method::POST, Some("http://evil.example"), o, Some("application/json")));
        assert!(!write_ok(&Method::DELETE, Some("http://127.0.0.1:4040"), o, Some("text/plain")));
        assert!(!write_ok(&Method::POST, Some("http://127.0.0.1:4040"), o, None));
        assert!(!write_ok(&Method::POST, Some("http://flyyy.example.com"), "https://flyyy.example.com", Some("application/json")));
    }

    #[test]
    fn bearer_tokens_and_the_401_limit() {
        let t = format!("Bearer flyyy_{}", "a".repeat(64));
        assert_eq!(bearer(Some(&t)), Some(t.as_str()));
        for bad in [None, Some("Bearer "), Some("bearer x"), Some("Basic eA=="), Some("Bearer a b"), Some("Bearer a\"b")] {
            assert_eq!(bearer(bad), None, "{bad:?}");
        }
        assert_eq!(bearer(Some(&format!("Bearer {}", "a".repeat(MAX_TOKEN + 1)))), None);
        let l = Limiter::default();
        let (ip, other, t0) = ("192.0.2.1".parse().unwrap(), "192.0.2.2".parse().unwrap(), Instant::now());
        for _ in 0..FAILS {
            assert!(!l.limited(ip, t0));
            l.failed(ip, t0);
        }
        assert!(l.limited(ip, t0) && !l.limited(other, t0));
        assert!(!l.limited(ip, t0 + WINDOW), "a new window");
    }

    #[test]
    fn the_csp_allows_the_page_by_hash_only() {
        let c = csp();
        assert!(!c.contains("script-src 'unsafe-inline'") && c.contains("script-src 'sha256-") && c.contains("style-src-elem 'sha256-"));
        assert!(c.contains("default-src 'none'") && c.contains("connect-src 'self'") && c.contains("frame-ancestors 'none'"));
        assert!(c.contains("style-src-elem 'sha256-") && c.contains(" https://fonts.googleapis.com;") && c.contains("font-src https://fonts.gstatic.com;"));
        // The hash encoder against a known value: SHA-256 of "" is 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=.
        assert_eq!(csp_hash(""), "'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='");
        // The page has no inline event handlers (the CSP would block them).
        assert!(!PAGE.contains(" onclick=\"") && !PAGE.contains(" onload=\""));
    }

    /// Every API call of the page goes through its one request helper, which adds the signed-in token:
    /// a bare fetch of an API path from anywhere else is a 401 on the public dashboard, and counts as a
    /// failed sign-in (getMesh's did, until the address got 429 for everything).
    #[test]
    fn the_page_calls_the_api_through_one_helper() {
        // Each call up to its first argument's end.
        let fetches: Vec<&str> = PAGE.match_indices("fetch(").map(|(i, _)| PAGE[i..].split([',', ')']).next().unwrap_or("")).collect();
        assert_eq!(fetches, ["fetch(path", "fetch(\"/templates.json\"", "fetch(\"/config.json\""], "{fetches:?}");
    }
}
