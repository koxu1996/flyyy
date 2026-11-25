//! `/v1/mesh`: a node's side by its source address on the mesh listener (join, its approval, its peers,
//! its certificate), an edge's on the control plane's own host (by its join token), and the operator's (create the mesh, list it, join tokens, approve a node).

use std::net::{Ipv4Addr, SocketAddr};

use corro_api_types::Statement;
use tracing::warn;

use hyper::{body::Incoming, Request, Response, StatusCode};

use super::{answer, api::body, error, routes::Route, Control, Csr, Join, LocalCert};
use crate::{api_error::ApiError, corro::query, Body};

impl Control {
    /// A node's mesh routes: `from` is its address (routes::authorize checked its kind).
    pub(crate) async fn mesh_source_route(&self, route: Route, req: Request<Incoming>, from: Ipv4Addr) -> Result<Response<Body>, ApiError> {
        match route {
            Route::MeshJoin => {
                let j: Join = body(req).await?;
                answer(self.mesh.join(from, &j.name, &j.key, &j.role, j.endpoint.as_deref(), j.hostname.as_deref()).await)
            }
            Route::MeshJoinStatus => answer(self.mesh.status(from)),
            Route::MeshPeers => answer(self.mesh.peers(from).await),
            Route::MeshCert => {
                let c: Csr = body(req).await?;
                answer(self.mesh.cert(from, &c.csr))
            }
            r => unreachable!("not a mesh source route: {r:?}"),
        }
    }

    /// `POST /v1/mesh/local`, from this host: an edge's certificate here, for its join token.
    pub(crate) async fn mesh_local_route(&self, req: Request<Incoming>) -> Result<Response<Body>, ApiError> {
        let l: LocalCert = body(req).await?;
        answer(self.mesh.local_cert(&l.token, &l.name, &l.role, &l.csr).await)
    }

    /// The operator's mesh routes.
    pub(crate) async fn mesh_route(&self, route: Route) -> Result<Response<Body>, ApiError> {
        match route {
            Route::MeshInit if self.single_host => {
                Err(error(StatusCode::CONFLICT, "single-host (dev) mode: no mesh; restart the control plane without --single-host first"))
            }
            Route::MeshInit => answer(self.mesh.init().await),
            Route::MeshList => {
                let mut list = self.mesh.list().await;
                if let Ok(v) = &mut list {
                    self.add_edge_listens(v).await;
                }
                answer(list)
            }
            Route::MeshToken => answer(self.mesh.token().await),
            Route::MeshApprove { node } => answer(self.mesh.approve(&node).await),
            r => unreachable!("not an operator mesh route: {r:?}"),
        }
    }

    /// Each edge node's `listen`: where its edges serve apps, as they announced it (`edges` rows whose load
    /// address is on the node's mesh address: one node can run several edges, as the tests' does).
    async fn add_edge_listens(&self, list: &mut serde_json::Value) {
        let rows: Vec<(String, String)> = match query(&self.client, Statement::Simple(super::load::EDGE_LISTENS.into())).await {
            Ok(r) => r,
            Err(e) => return warn!(error = %e, "mesh list: the edges' listen addresses"),
        };
        for n in list["nodes"].as_array_mut().into_iter().flatten().filter(|n| n["role"] == "edge") {
            let ip = n["ip"].as_str().and_then(|s| s.parse::<Ipv4Addr>().ok());
            let mut listen: Vec<&str> =
                rows.iter().filter(|(load, _)| load.parse::<SocketAddr>().ok().map(|a| a.ip()) == ip.map(Into::into)).map(|(_, l)| l.as_str()).collect();
            listen.dedup();
            n["listen"] = listen.into();
        }
    }
}
