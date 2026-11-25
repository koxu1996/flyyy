//! The control plane's side of the agent API (agent/api.rs): every round, each worker holding machines it
//! placed is listed, and gets what it lacks: a spec put, a start, a stop or a destroy. Level-triggered, so a
//! worker that was down or restarted catches up on the next round. Over mesh mTLS with the control plane's
//! own certificate, to the address the worker announced only once that is its mesh address (publish.rs).

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Mutex,
    time::Duration,
};

use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, header, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

use super::{db::MachineRec, model::Worker, Control};
use crate::{
    agent::{api::Listing, spec::digest},
    states::Desired,
    tls,
};

/// One call has this long, connect included: a dead worker costs a round this much, no more.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Agents {
    /// The control plane's mesh certificate; None: single-host, plain HTTP on loopback.
    tls_dir: Option<String>,
    /// Made on first use: the certificate exists only once the mesh does (`flyyy init`).
    connector: Mutex<Option<tokio_rustls::TlsConnector>>,
    /// Each worker's last sync error, so a dead worker is logged once, not every round.
    errors: Mutex<HashMap<String, String>>,
}

async fn send<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: S, req: Request<Full<Bytes>>) -> Result<(StatusCode, Bytes), String> {
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(|e| format!("handshake: {e}"))?;
    tokio::spawn(conn);
    let resp = sender.send_request(req).await.map_err(|e| format!("request: {e}"))?;
    let status = resp.status();
    let body = resp.into_body().collect().await.map_err(|e| format!("response: {e}"))?.to_bytes();
    Ok((status, body))
}

impl Agents {
    pub(crate) fn new(tls_dir: Option<String>) -> Self {
        Agents { tls_dir, connector: Mutex::default(), errors: Mutex::default() }
    }

    fn connector(&self, dir: &str) -> Result<tokio_rustls::TlsConnector, String> {
        let mut c = self.connector.lock().unwrap();
        if c.is_none() {
            *c = Some(tls::control_connector(dir)?);
        }
        Ok(c.clone().expect("set above"))
    }

    /// One request to an agent API; its status and body.
    pub(crate) async fn call(&self, addr: SocketAddr, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<(StatusCode, Bytes), String> {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, addr.to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| format!("request: {e}"))?;
        let call = async {
            let tcp = TcpStream::connect(addr).await.map_err(|e| format!("connect: {e}"))?;
            match &self.tls_dir {
                Some(dir) => {
                    let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
                    let tls = self.connector(dir)?.connect(name, tcp).await.map_err(|e| format!("tls: {e}"))?;
                    send(tls, req).await
                }
                None => send(tcp, req).await,
            }
        };
        tokio::time::timeout(CALL_TIMEOUT, call).await.map_err(|_| format!("no answer in {} s", CALL_TIMEOUT.as_secs()))?
    }

    /// Log a worker's error when it changes (None: it's gone).
    pub(crate) fn note(&self, worker: &str, error: Option<String>) {
        let mut errors = self.errors.lock().unwrap();
        match error {
            Some(e) if errors.get(worker) != Some(&e) => {
                tracing::warn!(%worker, error = %e, "agent");
                errors.insert(worker.into(), e);
            }
            Some(_) => {}
            None => {
                if errors.remove(worker).is_some() {
                    tracing::info!(%worker, "agent in sync again");
                }
            }
        }
    }
}

impl Control {
    /// One round: every worker holding machines of ours, in parallel.
    pub(crate) async fn sync_agents(&self, workers: &[Worker]) {
        let mut by_worker: BTreeMap<String, Vec<(String, MachineRec)>> = BTreeMap::new();
        for (id, m) in self.db.read(|r| r.machines.clone()) {
            by_worker.entry(m.worker.clone()).or_default().push((id, m));
        }
        let rounds = by_worker.into_iter().map(|(worker, want)| async move {
            let r = match workers.iter().find(|w| w.name == worker) {
                Some(w) => match self.bound(w, &w.agent_api) {
                    Ok(addr) => self.sync_worker(addr, want).await,
                    Err(e) => Err(e),
                },
                None => Err("no workers row".into()),
            };
            self.agents.note(&worker, r.err());
        });
        futures::future::join_all(rounds).await;
    }

    /// What the worker lacks of `want`, sent: specs first, then the desired state.
    async fn sync_worker(&self, addr: SocketAddr, want: Vec<(String, MachineRec)>) -> Result<(), String> {
        let call = |method: Method, path: String, body: Option<Vec<u8>>| async move {
            match self.agents.call(addr, method.clone(), &path, body).await? {
                (StatusCode::OK, b) => Ok(b),
                (code, b) => Err(format!("{method} {path}: {}: {}", code.as_u16(), String::from_utf8_lossy(&b))),
            }
        };
        let listing: Listing = serde_json::from_slice(&call(Method::GET, "/v1/machines".into(), None).await?).map_err(|e| format!("listing: {e}"))?;
        let mut errors = Vec::new();
        for (id, m) in want {
            let held = listing.get(&id);
            let step = async {
                match held {
                    // Never got there: nothing to destroy.
                    None if m.desired == Desired::Destroyed => return Ok(()),
                    None => {
                        call(Method::PUT, format!("/v1/machines/{id}"), Some(serde_json::to_vec(&m.spec).expect("serialisable"))).await?;
                    }
                    Some(h) if h.digest != digest(&m.spec) && h.desired != Desired::Destroyed => {
                        return Err(format!("{id}: the worker holds another spec"));
                    }
                    Some(_) => {}
                }
                match (held.map_or(Desired::Stopped, |h| h.desired), m.desired) {
                    (now, want) if now == want => Ok(()),
                    (_, Desired::Started) => call(Method::POST, format!("/v1/machines/{id}/start"), None).await.map(drop),
                    (_, Desired::Stopped) => call(Method::POST, format!("/v1/machines/{id}/stop"), None).await.map(drop),
                    (_, Desired::Destroyed) => call(Method::DELETE, format!("/v1/machines/{id}"), None).await.map(drop),
                }
            };
            if let Err(e) = step.await {
                errors.push(e);
            }
        }
        match errors.is_empty() {
            true => Ok(()),
            false => Err(errors.join("; ")),
        }
    }
}
