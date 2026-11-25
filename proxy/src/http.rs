//! HTTP serving helpers shared by the proxies and the control plane: plain and TLS listeners (HTTP/1),
//! SO_REUSEPORT, empty answers, and the stream type a backhaul connection is.

use std::{convert::Infallible, net::SocketAddr};
use tracing::warn;

use http_body_util::{BodyExt, Empty, Full};
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpSocket},
};

use crate::{tls, Body};

pub(crate) fn empty(code: StatusCode) -> Response<Body> {
    let mut r = Response::new(Empty::new().map_err(|n| match n {}).boxed());
    *r.status_mut() = code;
    r
}

/// `GET /state` (any path, in fact) on a proxy's debug listener: whatever `state` says now.
pub(crate) async fn serve_state(listener: TcpListener, state: impl Fn() -> String + Clone + Send + Sync + 'static) {
    serve(listener, move |_req, _peer, _local| {
        let body = state();
        async move { Ok(Response::new(Full::new(Bytes::from(body)).map_err(|n| match n {}).boxed())) }
    })
    .await
}

/// `f(request, peer, local)`: local is the address the connection came in on (the edges route on it).
pub(crate) async fn serve<F, Fut>(listener: TcpListener, f: F)
where
    F: Fn(Request<Incoming>, SocketAddr, SocketAddr) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<Response<Body>, Infallible>> + Send + 'static,
{
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let Ok(local) = stream.local_addr() else { continue };
        let f = f.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| f(req, peer, local));
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).with_upgrades().await;
        });
    }
}

/// `serve` behind TLS: the handshake first (with a deadline), then the same HTTP/1 service.
pub(crate) async fn serve_tls<F, Fut>(listener: TcpListener, tls: tokio_rustls::TlsAcceptor, f: F)
where
    F: Fn(Request<Incoming>, SocketAddr, SocketAddr) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<Response<Body>, Infallible>> + Send + 'static,
{
    loop {
        let Ok((stream, peer)) = listener.accept().await else { continue };
        let Ok(local) = stream.local_addr() else { continue };
        let (f, tls) = (f.clone(), tls.clone());
        tokio::spawn(async move {
            let stream = match tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, tls.accept(stream)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return warn!(%peer, error = %e, "tls handshake failed"),
                Err(_) => return warn!(%peer, "tls handshake timed out"),
            };
            let svc = service_fn(move |req| f(req, peer, local));
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).with_upgrades().await;
        });
    }
}

/// SO_REUSEPORT listener: every edge binds the same address and the kernel spreads connections.
// shortcut: SO_REUSEPORT on one host stands in for anycast across edge locations.
pub(crate) fn reuseport(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_reuseport(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// A backhaul connection, plain or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
