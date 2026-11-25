//! The control plane API's client, the CLI's and the dashboard's: one request per connection, plain HTTP
//! (on the mesh, inside WireGuard), with the caller's bearer token.

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, header, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpStream;

pub struct Client {
    /// host:port of the API.
    pub addr: String,
    /// None for the mesh's own routes (join, peers), which know the caller by its tunnel address.
    pub token: Option<String>,
}

impl Client {
    /// One request on a fresh connection; the response status and JSON body.
    pub async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<(StatusCode, Value), String> {
        tokio::time::timeout(Duration::from_secs(10), self.call_once(method, path, body))
            .await
            .map_err(|_| format!("no answer from the control plane at {} within 10 s", self.addr))?
    }

    async fn call_once(&self, method: Method, path: &str, body: Option<Value>) -> Result<(StatusCode, Value), String> {
        let (status, bytes) = self.raw_once(method, path, Bytes::from(body.map(|b| b.to_string()).unwrap_or_default()), None).await?;
        let json = serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        Ok((status, json))
    }

    /// One request with this body as it is (the dashboard forwards the page's), and the answer as it is.
    pub async fn raw(&self, method: Method, path: &str, body: Bytes) -> Result<(StatusCode, Bytes), String> {
        self.raw_as(method, path, body, None).await
    }

    /// `raw`, with this Authorization (the public dashboard's caller's own) instead of this client's token.
    pub async fn raw_as(&self, method: Method, path: &str, body: Bytes, authorization: Option<header::HeaderValue>) -> Result<(StatusCode, Bytes), String> {
        tokio::time::timeout(Duration::from_secs(10), self.raw_once(method, path, body, authorization))
            .await
            .map_err(|_| format!("no answer from the control plane at {} within 10 s", self.addr))?
    }

    async fn raw_once(&self, method: Method, path: &str, body: Bytes, authorization: Option<header::HeaderValue>) -> Result<(StatusCode, Bytes), String> {
        let connect = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&self.addr));
        let stream = connect
            .await
            .map_err(|_| format!("cannot reach the control plane at {}: connect timed out", self.addr))?
            .map_err(|e| format!("cannot reach the control plane at {}: {e}", self.addr))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.map_err(|e| format!("handshake: {e}"))?;
        tokio::spawn(conn);
        let mut req = Request::builder().method(method).uri(path).header(header::HOST, &self.addr);
        match (authorization, &self.token) {
            (Some(a), _) => req = req.header(header::AUTHORIZATION, a),
            (None, Some(t)) => req = req.header(header::AUTHORIZATION, format!("Bearer {t}")),
            (None, None) => {}
        }
        let req = req.header(header::CONTENT_TYPE, "application/json").body(Full::new(body)).map_err(|e| format!("request: {e}"))?;
        let resp = sender.send_request(req).await.map_err(|e| format!("request to {}: {e}", self.addr))?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await.map_err(|e| format!("response: {e}"))?.to_bytes();
        Ok((status, bytes))
    }

    /// A 2xx body, or the API's error message.
    pub async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let (status, json) = self.call(method, path, body).await?;
        if status.is_success() {
            Ok(json)
        } else {
            Err(format!("{}: {}", status.as_u16(), json["error"].as_str().map_or_else(|| json.to_string(), str::to_owned)))
        }
    }
}
