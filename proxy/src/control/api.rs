//! The HTTP API's entry: each request parsed into a route, checked against who may call it (routes.rs),
//! then handed to its resource's handler: apps.rs, orgs.rs, mesh_routes.rs, workers.rs.

use std::{
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Instant,
};

use http_body_util::{BodyExt, Limited};
use hyper::{body::Incoming, header, Request, Response, StatusCode};

use super::{
    auth::Caller,
    error, json,
    routes::{authorize, Route, Who},
    Control, MAX_BODY,
};
use crate::{api_error::ApiError, wg, Body};

pub(crate) async fn body<T: serde::de::DeserializeOwned>(req: Request<Incoming>) -> Result<T, ApiError> {
    let bytes = Limited::new(req.into_body(), MAX_BODY).collect().await.map_err(|e| error(StatusCode::BAD_REQUEST, format!("body: {e}")))?.to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| error(StatusCode::BAD_REQUEST, format!("bad JSON: {e}")))
}

pub(crate) async fn api(req: Request<Incoming>, control: Arc<Control>, peer: SocketAddr, mesh_listener: bool) -> Result<Response<Body>, Infallible> {
    let t0 = Instant::now();
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let from = match peer.ip() {
        IpAddr::V4(ip) if mesh_listener && wg::in_mesh(ip) => Some(ip),
        _ => None,
    };
    let loopback = !mesh_listener && peer.ip().is_loopback();
    let resp = control.handle(req, from, loopback).await.unwrap_or_else(|e| e.response());
    tracing::info!(%method, %path, status = resp.status().as_u16(), %peer, ms = t0.elapsed().as_millis() as u64, "request");
    Ok(resp)
}

/// `GET /v1/whoami`: who this token is, so a page can show it and offer only what it may do.
fn whoami(c: &Caller) -> serde_json::Value {
    match c {
        Caller::Operator => serde_json::json!({ "operator": true }),
        Caller::User { name, org } => serde_json::json!({ "user": name, "org": org }),
    }
}

impl Control {
    /// `from`: the caller's mesh address, only on the mesh listener. There WireGuard ties a source address
    /// to one key, so it identifies a token (10.250.255.x) or a node; anywhere else it means nothing.
    /// `loopback`: from this host, on the main listener.
    pub(crate) async fn handle(&self, req: Request<Incoming>, from: Option<Ipv4Addr>, loopback: bool) -> Result<Response<Body>, ApiError> {
        let route = Route::parse(req.method(), req.uri().path(), req.uri().query())?;
        let token = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        let who = authorize(route.access(), from, loopback, token.and_then(|t| self.caller(t)))?;
        match (route, who) {
            (r @ (Route::MeshJoin | Route::MeshJoinStatus | Route::MeshPeers | Route::MeshCert), Who::Source(ip)) => self.mesh_source_route(r, req, ip).await,
            (Route::MeshLocal, Who::Local) => self.mesh_local_route(req).await,
            (r @ (Route::MeshList | Route::MeshInit | Route::MeshToken | Route::MeshApprove { .. }), Who::Caller(_)) => self.mesh_route(r).await,
            (Route::Workers, Who::Caller(_)) => self.workers_route().await,
            (Route::Whoami, Who::Caller(c)) => Ok(json(StatusCode::OK, whoami(&c))),
            (r @ (Route::OrgsList | Route::OrgCreate | Route::OrgDestroy { .. }), Who::Caller(_)) => self.orgs_route(r, req).await,
            (r @ (Route::UsersList | Route::UserCreate), Who::Caller(_)) => self.users_route(r, req).await,
            (r, Who::Caller(caller)) => self.apps_route(r, req, caller).await,
            (r, Who::Source(ip)) => unreachable!("{r:?} isn't a mesh source route, yet {ip} was let in"),
            (r, Who::Local) => unreachable!("{r:?} isn't the local route, yet loopback was let in"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whoami_says_operator_or_user_and_org() {
        assert_eq!(whoami(&Caller::Operator), serde_json::json!({ "operator": true }));
        assert_eq!(whoami(&Caller::User { name: "alice".into(), org: "red".into() }), serde_json::json!({ "user": "alice", "org": "red" }));
    }
}
