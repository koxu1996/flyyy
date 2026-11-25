//! The mesh's PKI. The control plane runs a CA and issues every node (and itself) a short-lived
//! certificate from the node's CSR, so the node's key never leaves it. SANs: IP:<mesh address> (what
//! Corrosion and the edges verify a peer by) and URI:flyyy://<role>/<name>. Usable for both server and
//! client auth. A node renews over the mesh once half the lifetime has passed. Used by the control plane
//! (control.rs, mesh.rs), the backhaul and edges (tls.rs) and `flyyy join` (bin/flyyy.rs includes this file).
// shortcut: no revocation (short lifetimes instead), one CA key that never rotates.
// Both binaries include this file and each uses only part of it.
use std::{net::Ipv4Addr, time::Duration};

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use time::OffsetDateTime;

/// What a node is, in its certificate. Nodes ask for worker or edge at join; only the control plane is control.
pub const ROLES: [&str; 3] = ["control", "worker", "edge"];
/// The files in a node's TLS directory.
pub const CA: &str = "ca.pem";
pub const CERT: &str = "cert.pem";
pub const KEY: &str = "key.pem";

pub fn uri(role: &str, name: &str) -> String {
    format!("flyyy://{role}/{name}")
}

/// (role, name) from a certificate's URI SAN, if it has one of ours.
pub fn role_of(der: &[u8]) -> Option<(String, String)> {
    use x509_parser::extensions::GeneralName;
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let san = cert.subject_alternative_name().ok()??;
    san.value.general_names.iter().find_map(|g| match g {
        GeneralName::URI(u) => {
            let (role, name) = u.strip_prefix("flyyy://")?.split_once('/')?;
            ROLES.contains(&role).then(|| (role.to_owned(), name.to_owned()))
        }
        _ => None,
    })
}

/// The CA: its key (0600) and self-signed certificate in `dir`, made on first use.
pub struct Ca {
    issuer: Issuer<'static, KeyPair>,
    pem: String,
}

impl Ca {
    pub fn open_or_create(dir: &str) -> Result<Self, String> {
        let (key_file, cert_file) = (format!("{dir}/ca.key"), format!("{dir}/ca.pem"));
        if !std::path::Path::new(&key_file).exists() {
            let key = KeyPair::generate().map_err(|e| format!("CA key: {e}"))?;
            let mut p = CertificateParams::default();
            p.distinguished_name = DistinguishedName::new();
            p.distinguished_name.push(DnType::CommonName, "flyyy mesh CA");
            p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
            let now = OffsetDateTime::now_utc();
            (p.not_before, p.not_after) = (now - time::Duration::minutes(1), now + time::Duration::days(3650));
            let cert = p.self_signed(&key).map_err(|e| format!("CA certificate: {e}"))?;
            crate::wg::write_private(&key_file, &key.serialize_pem())?;
            std::fs::write(&cert_file, cert.pem()).map_err(|e| format!("{cert_file}: {e}"))?;
        }
        let key = KeyPair::from_pem(&std::fs::read_to_string(&key_file).map_err(|e| format!("{key_file}: {e}"))?).map_err(|e| format!("{key_file}: {e}"))?;
        let pem = std::fs::read_to_string(&cert_file).map_err(|e| format!("{cert_file}: {e}"))?;
        let issuer = Issuer::from_ca_cert_pem(&pem, key).map_err(|e| format!("{cert_file}: {e}"))?;
        Ok(Ca { issuer, pem })
    }

    pub fn pem(&self) -> &str {
        &self.pem
    }

    /// A certificate for this CSR's key. Everything in it is the control plane's: the CSR's own subject
    /// and extensions are ignored, only its (signature-checked) public key is used.
    pub fn issue(&self, csr_pem: &str, ip: Ipv4Addr, role: &str, name: &str, lifetime: Duration) -> Result<String, String> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).map_err(|e| format!("bad CSR: {e}"))?;
        let mut p = CertificateParams::default();
        p.distinguished_name = DistinguishedName::new();
        p.distinguished_name.push(DnType::CommonName, name);
        p.subject_alt_names = vec![SanType::IpAddress(ip.into()), SanType::URI(uri(role, name).try_into().map_err(|e| format!("uri: {e}"))?)];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
        p.use_authority_key_identifier_extension = true;
        let now = OffsetDateTime::now_utc();
        // A minute back: a node's clock a little ahead of ours.
        (p.not_before, p.not_after) = (now - time::Duration::minutes(1), now + lifetime);
        let cert = CertificateSigningRequestParams { params: p, public_key: csr.public_key }.signed_by(&self.issuer).map_err(|e| format!("sign: {e}"))?;
        Ok(cert.pem())
    }
}

/// A node's new key and a CSR for it (the key stays here; only the CSR goes to the control plane).
pub fn new_key_and_csr(name: &str) -> Result<(String, String), String> {
    let key = KeyPair::generate().map_err(|e| format!("key: {e}"))?;
    let mut p = CertificateParams::default();
    p.distinguished_name = DistinguishedName::new();
    p.distinguished_name.push(DnType::CommonName, name);
    let csr = p.serialize_request(&key).map_err(|e| format!("CSR: {e}"))?.pem().map_err(|e| format!("CSR: {e}"))?;
    Ok((key.serialize_pem(), csr))
}

/// Due for renewal: missing, unreadable, or past half its lifetime.
pub fn needs_renewal(cert_file: &str) -> bool {
    use x509_parser::pem::parse_x509_pem;
    let Ok(pem) = std::fs::read(cert_file) else { return true };
    let Ok((_, pem)) = parse_x509_pem(&pem) else { return true };
    let Ok(cert) = pem.parse_x509() else { return true };
    let (from, to) = (cert.validity().not_before.timestamp(), cert.validity().not_after.timestamp());
    OffsetDateTime::now_utc().unix_timestamp() >= from + (to - from) / 2
}

/// Write the CA, key and certificate into a node's TLS directory: each to a temporary file then renamed,
/// the key before the certificate (a reader that sees the new certificate finds its key). The files are
/// 0640 and take the directory's group: the services that use them read them through that group.
pub fn write_tls(dir: &str, ca: &str, key: &str, cert: &str) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let gid = std::fs::metadata(dir).map_err(|e| format!("{dir}: {e}"))?.gid();
    for (name, data) in [(CA, ca), (KEY, key), (CERT, cert)] {
        let (path, tmp) = (format!("{dir}/{name}"), format!("{dir}/.{name}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, data).map_err(|e| format!("{tmp}: {e}"))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640)).map_err(|e| format!("{tmp}: {e}"))?;
        std::os::unix::fs::chown(&tmp, None, Some(gid)).map_err(|e| format!("chown {tmp}: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_from_a_csr_with_our_sans_only() {
        let dir = std::env::temp_dir().join(format!("flyyy-pki-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = Ca::open_or_create(dir.to_str().unwrap()).unwrap();
        let (_key, csr) = new_key_and_csr("worker1").unwrap();
        let pem = ca.issue(&csr, Ipv4Addr::new(10, 250, 1, 1), "worker", "worker1", Duration::from_secs(3600)).unwrap();
        let der = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap().1.contents;
        assert_eq!(role_of(&der), Some(("worker".into(), "worker1".into())));
        let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
        let ips: Vec<_> = cert
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .filter_map(|g| if let x509_parser::extensions::GeneralName::IPAddress(b) = g { Some(b.to_vec()) } else { None })
            .collect();
        assert_eq!(ips, [vec![10, 250, 1, 1]]);
        assert_eq!(cert.issuer().to_string(), "CN=flyyy mesh CA");
        assert!(Ca::open_or_create(dir.to_str().unwrap()).unwrap().pem() == ca.pem());
        assert!(ca.issue("not a csr", Ipv4Addr::new(10, 250, 1, 1), "worker", "w", Duration::from_secs(60)).is_err());
        // Fresh: not due; a missing file is.
        let f = dir.join("c.pem");
        std::fs::write(&f, &pem).unwrap();
        assert!(!needs_renewal(f.to_str().unwrap()) && needs_renewal(dir.join("nope.pem").to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
