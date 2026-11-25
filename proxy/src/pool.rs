//! The edge's way to the backhauls: a pool of long-lived HTTP/2 connections per worker (mTLS on the mesh),
//! each request a CONNECT stream on one of them.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::warn;

use http_body_util::Empty;
use hyper::{body::Bytes, upgrade::Upgraded, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::{
    http::Io,
    table::{Target, Upstream},
    tls,
};

// shortcut: fixed 1 s connect timeout.
pub(crate) async fn dial(addr: SocketAddr) -> Result<TcpStream, String> {
    match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(addr)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(format!("connect: {e}")),
        Err(_) => Err("connect timeout".into()),
    }
}

/// The edge's backhaul connections: up to POOL_CONNS long-lived HTTP/2 connections per worker backhaul
/// (mTLS on the mesh), each carrying many CONNECT streams at once, so a request costs a stream, not a TCP and
/// TLS handshake. A connection is retired after POOL_MAX_AGE, so renewed certificates (both ends') are used.
// shortcut: no per-connection stream limits of our own (hyper's defaults), no idle reaping beyond the age.
pub(crate) struct Pooled {
    sender: hyper::client::conn::http2::SendRequest<Empty<Bytes>>,
    born: Instant,
}

pub(crate) const POOL_CONNS: usize = 4;

pub(crate) const POOL_MAX_AGE: Duration = Duration::from_secs(300);

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a tunnel to an instance failed; its Display is what the edge logs (`failed=[id(reason)]`).
#[derive(Debug)]
pub(crate) enum TunnelError {
    /// The upstream isn't a backhaul address (a worker's own route can't be an edge's).
    NotBackhaul(Target),
    /// Before the backhaul answered: dial, TLS, HTTP/2 or the CONNECT itself.
    Unreachable(String),
    /// The backhaul answered the CONNECT with this.
    Refused(StatusCode),
    Upgrade(String),
}

impl std::fmt::Display for TunnelError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            TunnelError::NotBackhaul(t) => write!(f, "{t} is not a backhaul"),
            TunnelError::Unreachable(e) => f.write_str(e),
            TunnelError::Refused(s) => write!(f, "backhaul {}", s.as_u16()),
            TunnelError::Upgrade(e) => write!(f, "upgrade: {e}"),
        }
    }
}

impl TunnelError {
    /// Whether it ejects its instance: everything does but a backhaul 404. That one means the worker
    /// doesn't serve the instance (yet: its table can trail the edges' by a moment): the next instance is
    /// tried, but this one keeps its place in the round robin.
    pub(crate) fn ejects(&self) -> bool {
        !matches!(self, TunnelError::Refused(StatusCode::NOT_FOUND))
    }
}

/// An edge's pool: its connections per backhaul (each with its round-robin position), and its mTLS to the
/// backhauls (tls.rs) when the node is on a mesh.
pub(crate) struct Pool {
    tls: Option<tokio_rustls::TlsConnector>,
    conns: Mutex<HashMap<SocketAddr, (Vec<Pooled>, usize)>>,
}

impl Pool {
    pub(crate) fn new(tls: Option<tokio_rustls::TlsConnector>) -> Self {
        Pool { tls, conns: Mutex::default() }
    }

    /// A live pooled connection to `addr`, round robin, once the pool there is full; None: dial another.
    fn pooled(&self, addr: SocketAddr) -> Option<hyper::client::conn::http2::SendRequest<Empty<Bytes>>> {
        let mut pool = self.conns.lock().unwrap();
        let (conns, next) = pool.entry(addr).or_default();
        conns.retain(|c| !c.sender.is_closed() && c.born.elapsed() < POOL_MAX_AGE);
        if conns.len() < POOL_CONNS {
            return None;
        }
        *next = next.wrapping_add(1);
        Some(conns[*next % conns.len()].sender.clone())
    }

    /// A new HTTP/2 connection to a backhaul (TCP, then mTLS on the mesh), added to the pool.
    async fn connect(&self, addr: SocketAddr) -> Result<hyper::client::conn::http2::SendRequest<Empty<Bytes>>, String> {
        let stream = dial(addr).await?;
        let _ = stream.set_nodelay(true);
        // On the mesh: mTLS, the worker verified by its mesh address, this edge by its own certificate.
        let io: Box<dyn Io> = match &self.tls {
            Some(c) => {
                let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
                let tls = tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, c.connect(name, stream)).await.map_err(|_| "tls: handshake timed out".to_owned())?;
                Box::new(tls.map_err(|e| format!("tls: {e}"))?)
            }
            None => Box::new(stream),
        };
        // Pings find a connection to a worker that vanished without closing it.
        let (sender, conn) = hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .timer(hyper_util::rt::TokioTimer::new())
            .keep_alive_interval(Duration::from_secs(10))
            .keep_alive_timeout(Duration::from_secs(5))
            .handshake(TokioIo::new(io))
            .await
            .map_err(|e| format!("h2 handshake: {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                warn!(backhaul = %addr, error = %e, "pooled connection ended");
            }
        });
        let mut pool = self.conns.lock().unwrap();
        let (conns, _) = pool.entry(addr).or_default();
        if conns.len() < POOL_CONNS {
            conns.push(Pooled { sender: sender.clone(), born: Instant::now() });
        }
        Ok(sender)
    }

    /// Edge → worker: `CONNECT <instance-id>` on a pooled HTTP/2 connection to its backhaul; the upgraded stream
    /// reaches the VM. A pooled connection that fails is dropped and the CONNECT retried once on a new one (a
    /// dead connection says nothing about the instance).
    pub(crate) async fn tunnel(&self, up: &Upstream) -> Result<Upgraded, TunnelError> {
        let Target::Tcp(addr) = up.addr else { return Err(TunnelError::NotBackhaul(up.addr.clone())) };
        for attempt in 0..2 {
            let (mut sender, from_pool) = match self.pooled(addr) {
                Some(s) if attempt == 0 => (s, true),
                _ => (self.connect(addr).await.map_err(TunnelError::Unreachable)?, false),
            };
            let req = Request::builder()
                .method(Method::CONNECT)
                .uri(up.id.as_str())
                .body(Empty::<Bytes>::new())
                .map_err(|e| TunnelError::Unreachable(format!("bad id: {e}")))?;
            // Bounded like the dial (1 s) and handshake: a stuck connection fails over instead of hanging.
            let sent = tokio::time::timeout(CONNECT_TIMEOUT, async {
                sender.ready().await?;
                sender.send_request(req).await
            });
            let resp = match sent.await {
                Ok(Ok(r)) => r,
                Err(_) if from_pool => {
                    warn!(backhaul = %addr, "pooled connection timed out, retrying on a new one");
                    continue;
                }
                Err(_) => return Err(TunnelError::Unreachable("CONNECT: timed out".into())),
                Ok(Err(e)) if from_pool => {
                    warn!(backhaul = %addr, error = %e, "pooled connection failed, retrying on a new one");
                    continue;
                }
                Ok(Err(e)) => return Err(TunnelError::Unreachable(format!("CONNECT: {e}"))),
            };
            if resp.status() != StatusCode::OK {
                return Err(TunnelError::Refused(resp.status()));
            }
            return hyper::upgrade::on(resp).await.map_err(|e| TunnelError::Upgrade(e.to_string()));
        }
        Err(TunnelError::Unreachable("CONNECT: no connection".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_backhaul_404_spares_its_instance() {
        assert!(!TunnelError::Refused(StatusCode::NOT_FOUND).ejects());
        assert!(TunnelError::Refused(StatusCode::BAD_GATEWAY).ejects() && TunnelError::Unreachable("connect: refused".into()).ejects());
        assert_eq!(TunnelError::Refused(StatusCode::BAD_GATEWAY).to_string(), "backhaul 502", "what the edges log, as before");
    }
}
