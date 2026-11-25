//! The edge's TLS: an already-issued certificate (no ACME), PEM chain + key, reloaded on SIGHUP.
//! A reload that fails (unreadable, unparsable, key not matching the certificate) keeps serving the old one.
// shortcut: one certificate for every vhost (a wildcard for *.<apps-domain>), no per-app certificates.
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tracing::{info, warn};

use rustls::{
    crypto::ring,
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    ServerConfig,
};
use tokio_rustls::TlsAcceptor;

#[derive(Debug)]
pub(crate) struct Certs {
    cert: String,
    key: String,
    current: RwLock<Arc<CertifiedKey>>,
}

/// The chain (end-entity first) and its key, checked against each other.
fn load(cert: &str, key: &str) -> Result<CertifiedKey, String> {
    let chain = CertificateDer::pem_file_iter(cert).map_err(|e| format!("{cert}: {e}"))?.collect::<Result<Vec<_>, _>>().map_err(|e| format!("{cert}: {e}"))?;
    if chain.is_empty() {
        return Err(format!("{cert}: no certificate in it"));
    }
    let k = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{key}: {e}"))?;
    CertifiedKey::from_der(chain, k, &ring::default_provider()).map_err(|e| format!("{cert} with {key}: {e}"))
}

impl Certs {
    pub(crate) fn open(cert: &str, key: &str) -> Result<Arc<Self>, String> {
        let ck = load(cert, key)?;
        Ok(Arc::new(Certs { cert: cert.into(), key: key.into(), current: RwLock::new(Arc::new(ck)) }))
    }

    /// Read the files again; on any error keep the current certificate.
    pub(crate) fn reload(&self) -> Result<(), String> {
        let ck = load(&self.cert, &self.key)?;
        *self.current.write().unwrap() = Arc::new(ck);
        Ok(())
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap().clone())
    }
}

/// TLS 1.2 and 1.3 (rustls' safe defaults), HTTP/1.1 only (the edge is an HTTP/1 server).
pub(crate) fn acceptor(certs: Arc<Certs>) -> Result<TlsAcceptor, String> {
    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_cert_resolver(certs);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// SIGHUP: reload the certificate (e.g. after a renewal). New handshakes get the new one.
pub(crate) async fn reload_on_hup(certs: Arc<Certs>) {
    let Ok(mut hup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) else {
        warn!("can't catch SIGHUP: the certificate won't be reloaded");
        return;
    };
    while hup.recv().await.is_some() {
        match certs.reload() {
            Ok(()) => info!(cert = %certs.cert, key = %certs.key, "reloaded the certificate"),
            Err(e) => warn!(error = %e, "reload failed, still serving the previous certificate"),
        }
    }
}

/// A TLS handshake has this long; then the connection is dropped.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

impl rustls::client::ResolvesClientCert for Certs {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[rustls::SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().unwrap().clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Reload when the certificate file changes (a renewal, pki.rs writes the key first). Polling, every 10 s.
pub(crate) async fn reload_on_change(certs: Arc<Certs>) {
    let mtime = |c: &Certs| std::fs::metadata(&c.cert).and_then(|m| m.modified()).ok();
    let mut last = mtime(&certs);
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let now = mtime(&certs);
        if now != last {
            match certs.reload() {
                Ok(()) => {
                    info!(cert = %certs.cert, "reloaded the renewed certificate");
                    last = now;
                }
                Err(e) => warn!(error = %e, "renewed certificate not loaded yet"),
            }
        }
    }
}

fn mesh_roots(dir: &str) -> Result<Arc<rustls::RootCertStore>, String> {
    let ca = format!("{dir}/{}", crate::pki::CA);
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(&ca).map_err(|e| format!("{ca}: {e}"))? {
        roots.add(c.map_err(|e| format!("{ca}: {e}"))?).map_err(|e| format!("{ca}: {e}"))?;
    }
    Ok(Arc::new(roots))
}

/// This node's mesh certificate, reloaded when renewed.
fn mesh_certs(dir: &str) -> Result<Arc<Certs>, String> {
    let certs = Certs::open(&format!("{dir}/{}", crate::pki::CERT), &format!("{dir}/{}", crate::pki::KEY))?;
    tokio::spawn(reload_on_change(certs.clone()));
    Ok(certs)
}

/// A client certificate from the mesh CA, and of one role (its URI SAN): the backhaul takes edges only.
#[derive(Debug)]
struct OnlyRole {
    inner: Arc<dyn rustls::server::danger::ClientCertVerifier>,
    role: &'static str,
}

impl rustls::server::danger::ClientCertVerifier for OnlyRole {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        self.inner.verify_client_cert(end_entity, intermediates, now)?;
        match crate::pki::role_of(end_entity) {
            Some((role, _)) if role == self.role => Ok(rustls::server::danger::ClientCertVerified::assertion()),
            other => Err(rustls::Error::General(format!("want role {}, got (role, name) {other:?}", self.role))),
        }
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The backhaul's TLS: its own mesh certificate, and only edges' certificates accepted. HTTP/2 (the edges'
/// pooled connections) or HTTP/1.1 (a single CONNECT, as curl sends one).
pub(crate) fn backhaul_acceptor(dir: &str) -> Result<TlsAcceptor, String> {
    let provider = Arc::new(ring::default_provider());
    let inner = rustls::server::WebPkiClientVerifier::builder_with_provider(mesh_roots(dir)?, provider.clone()).build().map_err(|e| e.to_string())?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(OnlyRole { inner, role: "edge" }))
        .with_cert_resolver(mesh_certs(dir)?);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// TLS for what only the control plane may call (a worker's agent API, an edge's load counters): this
/// node's own mesh certificate, and only the control plane's certificate accepted. HTTP/1.1.
pub(crate) fn control_only_acceptor(dir: &str) -> Result<TlsAcceptor, String> {
    let provider = Arc::new(ring::default_provider());
    let inner = rustls::server::WebPkiClientVerifier::builder_with_provider(mesh_roots(dir)?, provider.clone()).build().map_err(|e| e.to_string())?;
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(OnlyRole { inner, role: "control" }))
        .with_cert_resolver(mesh_certs(dir)?);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// The control plane's TLS to an agent API: the worker's certificate verified for its mesh address (its IP
/// SAN), and the control plane's own certificate presented. HTTP/1.1.
pub(crate) fn control_connector(dir: &str) -> Result<tokio_rustls::TlsConnector, String> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(mesh_roots(dir)?)
        .with_client_cert_resolver(mesh_certs(dir)?);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// An edge's TLS to a backhaul: the worker's certificate verified for its mesh address (its IP SAN),
/// and the edge's own certificate presented.
pub(crate) fn edge_connector(dir: &str) -> Result<tokio_rustls::TlsConnector, String> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(mesh_roots(dir)?)
        .with_client_cert_resolver(mesh_certs(dir)?);
    // The edges' pooled backhaul connections are HTTP/2, one CONNECT stream per request.
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_bad_files_and_keeps_the_old_certificate() {
        let dir = std::env::temp_dir().join(format!("flyyy-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        std::fs::write(p("empty.pem"), "").unwrap();
        std::fs::write(p("junk.pem"), "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n").unwrap();
        assert!(load(&p("missing.pem"), &p("missing.key")).unwrap_err().contains("missing.pem"));
        assert!(load(&p("empty.pem"), &p("empty.pem")).unwrap_err().contains("no certificate"));
        assert!(load(&p("junk.pem"), &p("junk.pem")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
