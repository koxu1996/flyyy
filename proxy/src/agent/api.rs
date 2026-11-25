//! The agent API: how the control plane tells a worker what to run. Over mesh mTLS, and only the control
//! plane's certificate is accepted (tls::agent_acceptor); with --single-host, plain HTTP on loopback.
//!   GET    /v1/machines             every machine here: id -> its desired state and spec digest
//!   PUT    /v1/machines/<id>        its spec (JSON, spec.rs); a new machine starts out stopped
//!   POST   /v1/machines/<id>/start
//!   POST   /v1/machines/<id>/stop
//!   DELETE /v1/machines/<id>        destroy it
//!   GET    /v1/machines/<id>/logs   the last LOG_LINES lines of its console (its unit's journal)
//!   GET    /v1/metrics?app=<name>&n=<buckets>   the app's machines' CPU and memory here (metrics.rs)
//! All idempotent: the same request again changes nothing. Env values travel only here, never in gossip.

use std::{collections::BTreeMap, convert::Infallible, net::SocketAddr, sync::Arc};
use tracing::info;

use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::{Bytes, Incoming},
    header, Method, Request, Response, StatusCode,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use super::{
    desired::{Refused, Store},
    spec::{digest, Spec},
    sys,
};
use crate::{
    http::{serve, serve_tls},
    metrics::{self, Machines},
    states::Desired,
    tls, Body,
};

/// A machine's console, as `GET /v1/machines/<id>/logs` gives it: its last lines.
pub(crate) const LOG_LINES: usize = 200;

/// A spec's JSON is at most this big (cmd and env have their own, smaller limits).
const MAX_BODY: usize = 128 * 1024;

/// One machine in the listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Held {
    pub(crate) desired: Desired,
    /// spec::digest of its spec.
    pub(crate) digest: String,
}

pub(crate) type Listing = BTreeMap<String, Held>;

fn json(code: StatusCode, v: serde_json::Value) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(v.to_string())).map_err(|n| match n {}).boxed());
    *r.status_mut() = code;
    r.headers_mut().insert(header::CONTENT_TYPE, "application/json".parse().expect("static"));
    r
}

fn refused(r: Refused) -> Response<Body> {
    let (code, msg) = match r {
        Refused::Bad(e) => (StatusCode::BAD_REQUEST, e),
        Refused::Missing => (StatusCode::NOT_FOUND, "no such machine".into()),
        Refused::Conflict(e) => (StatusCode::CONFLICT, e),
    };
    json(code, serde_json::json!({ "error": msg }))
}

fn done(changed: bool) -> Response<Body> {
    json(StatusCode::OK, serde_json::json!({ "changed": changed }))
}

async fn handle(req: Request<Incoming>, store: Arc<Store>, machines: Arc<Machines>) -> Response<Body> {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    if (&method, path.as_str()) == (&Method::GET, "/v1/metrics") {
        return match metrics::query(req.uri().query()) {
            Ok((app, n)) => json(StatusCode::OK, machines.json(&app, n)),
            Err(e) => refused(Refused::Bad(e)),
        };
    }
    let Some(rest) = path.strip_prefix("/v1/machines") else { return refused(Refused::Bad("no such route".into())) };
    match (method, rest.strip_prefix('/')) {
        (Method::GET, None) => {
            let listing: Listing = store.all().into_iter().map(|(id, r)| (id, Held { desired: r.desired, digest: digest(&r.spec) })).collect();
            json(StatusCode::OK, serde_json::to_value(listing).expect("serialisable"))
        }
        (Method::PUT, Some(id)) if !id.contains('/') => {
            let bytes = match Limited::new(req.into_body(), MAX_BODY).collect().await {
                Ok(b) => b.to_bytes(),
                Err(e) => return refused(Refused::Bad(format!("body: {e}"))),
            };
            let spec: Spec = match serde_json::from_slice(&bytes) {
                Ok(s) => s,
                Err(e) => return refused(Refused::Bad(format!("bad spec: {e}"))),
            };
            store.put(id, spec).map_or_else(refused, done)
        }
        // Only a machine held here: the id is then one the control plane gave (its spec checked it).
        (Method::GET, Some(r)) if r.ends_with("/logs") => {
            let id = r.trim_end_matches("/logs");
            if id.contains('/') || !store.all().contains_key(id) {
                return refused(Refused::Missing);
            }
            match sys::console(id, LOG_LINES).await {
                Ok(lines) => json(StatusCode::OK, serde_json::json!({ "lines": lines })),
                Err(e) => json(StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({ "error": e.to_string() })),
            }
        }
        (Method::POST, Some(r)) => match r.split_once('/') {
            Some((id, "start")) => store.set(id, Desired::Started).map_or_else(refused, done),
            Some((id, "stop")) => store.set(id, Desired::Stopped).map_or_else(refused, done),
            _ => refused(Refused::Bad("no such route".into())),
        },
        (Method::DELETE, Some(id)) if !id.contains('/') => store.set(id, Desired::Destroyed).map_or_else(refused, done),
        _ => refused(Refused::Bad("no such route".into())),
    }
}

/// Serve the API on `addr`: mTLS with this node's mesh certificate (`tls_dir`), or plain on loopback in
/// single-host mode.
pub(crate) async fn run(addr: SocketAddr, tls_dir: Option<String>, single_host: bool, store: Arc<Store>, machines: Arc<Machines>) {
    let listener = TcpListener::bind(addr).await.unwrap_or_else(|e| panic!("agent API on {addr}: {e}"));
    let f = move |req: Request<Incoming>, peer: SocketAddr, _local: SocketAddr| {
        let (store, machines) = (store.clone(), machines.clone());
        async move {
            let (method, path) = (req.method().clone(), req.uri().path().to_owned());
            let resp = handle(req, store, machines).await;
            info!(%method, %path, status = resp.status().as_u16(), %peer, "api");
            Ok::<_, Infallible>(resp)
        }
    };
    match tls_dir {
        Some(dir) => {
            let acceptor = tls::control_only_acceptor(&dir).unwrap_or_else(|e| panic!("agent API TLS from {dir}: {e}"));
            info!(%addr, "agent API (mTLS, the control plane's certificate only)");
            serve_tls(listener, acceptor, f).await
        }
        None => {
            assert!(single_host && addr.ip().is_loopback(), "agent API {addr}: without --tls-dir only on loopback with --single-host");
            info!(%addr, "agent API (single-host: plain HTTP on loopback)");
            serve(listener, f).await
        }
    }
}
