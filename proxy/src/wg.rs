//! The WireGuard mesh's shared parts: addresses, join tokens and the kernel interface. Used by the control
//! plane (control.rs) and by `flyyy join` (bin/flyyy.rs includes this file). Both need only CAP_NET_ADMIN.
// shortcut: shells out to `ip` and `wg` (wireguard-tools); use netlink when it matters.
// Both binaries include this file and each uses only part of it.
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// The mesh interface on every node.
pub const IFACE: &str = "flyyy0";
/// UDP port of every node's interface.
pub const PORT: u16 = 51820;
/// The mesh: 10.250.0.0/16. The control plane is .0.1, approved nodes 10.250.1.1 up, join tokens 10.250.255.x.
pub const PREFIX: u8 = 16;
pub const CONTROL_IP: Ipv4Addr = Ipv4Addr::new(10, 250, 0, 1);
/// The control API on the mesh (join, peers, and the admin API with its bearer token).
pub const CONTROL_API_PORT: u16 = 4000;

pub fn in_mesh(ip: Ipv4Addr) -> bool {
    ip.octets()[..2] == [10, 250]
}

/// A join token's address: the only one its bootstrap key may use.
pub fn is_bootstrap(ip: Ipv4Addr) -> bool {
    in_mesh(ip) && ip.octets()[2] == 255
}

/// A node's address: neither the control plane's nor a token's.
pub fn is_node(ip: Ipv4Addr) -> bool {
    in_mesh(ip) && !is_bootstrap(ip) && ip != CONTROL_IP && ip.octets()[2] != 0
}

/// Where an edge may dial a worker's backhaul: a mesh node's address; loopback only in single-host mode
/// (`--single-host`, dev: everything on one machine, no mesh). The address comes from gossip (the control
/// plane's `backhauls` row), so it's checked, not trusted.
pub fn backhaul_ok(addr: std::net::SocketAddr, single_host: bool) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => is_node(ip) || (single_host && ip.is_loopback()),
        std::net::IpAddr::V6(ip) => single_host && ip.is_loopback(),
    }
}

/// A WireGuard key: 32 bytes, base64 (44 characters, the last one `=`).
pub fn key_ok(k: &str) -> bool {
    k.len() == 44 && k.ends_with('=') && k[..43].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        // 32 bytes leave 2 unused bits in the last base64 digit.
        && "AEIMQUYcgkosw048".contains(&k[42..43])
}

/// A one-use join token: `flyyy-join:<bootstrap private key>:<bootstrap address>:<control plane public key>`,
/// then `@<host>:<port>`, where the node's WireGuard reaches the control plane, if it was started with
/// `--endpoint`. It holds a private key, so it is a secret until used; the control plane only keeps the
/// public half.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub key: String,
    pub ip: Ipv4Addr,
    pub control_pub: String,
    /// The control plane's endpoint (host:port), when it knows it: then the token is all a join needs.
    pub control: Option<String>,
}

impl Token {
    pub fn encode(&self) -> String {
        let at = self.control.as_ref().map_or(String::new(), |c| format!("@{c}"));
        format!("flyyy-join:{}:{}:{}{at}", self.key, self.ip, self.control_pub)
    }

    pub fn decode(s: &str) -> Result<Self, String> {
        let bad = || "bad join token: want flyyy-join:<key>:<address>:<key>[@<host>:<port>] (from `flyyy init` or `flyyy token`)".to_owned();
        // Keys are base64 and addresses dotted, so the first @ starts the endpoint.
        let (s, control) = match s.trim().split_once('@') {
            Some((s, c)) if endpoint_ok(c) => (s, Some(c.to_owned())),
            Some(_) => return Err(bad()),
            None => (s.trim(), None),
        };
        let [tag, key, ip, control_pub] = s.split(':').collect::<Vec<_>>()[..] else { return Err(bad()) };
        let ip: Ipv4Addr = ip.parse().map_err(|_| bad())?;
        if tag != "flyyy-join" || !key_ok(key) || !key_ok(control_pub) || !is_bootstrap(ip) {
            return Err(bad());
        }
        Ok(Token { key: key.into(), ip, control_pub: control_pub.into(), control })
    }
}

/// A peer as the control plane hands it out, and as `wg set` takes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Peer {
    pub key: String,
    pub ip: Ipv4Addr,
    /// `ip:port` as the control plane saw it; empty until it did (the peer then reaches us first).
    #[serde(default)]
    pub endpoint: String,
}

/// What `wg show <iface> dump` says about one peer.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub key: String,
    pub allowed: String,
    pub endpoint: String,
    /// Unix seconds, 0 = never.
    pub handshake: u64,
}

async fn run(cmd: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut c = Command::new(cmd);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = c.spawn().map_err(|e| format!("{cmd}: {e}"))?;
    if let Some(input) = stdin {
        let mut i = child.stdin.take().expect("piped");
        i.write_all(input.as_bytes()).await.map_err(|e| format!("{cmd}: {e}"))?;
    }
    let out = child.wait_with_output().await.map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// A private key file: 0600, never overwritten.
pub fn write_private(path: &str, key: &str) -> Result<(), String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| format!("{path}: {e}"))?;
    f.write_all(key.as_bytes()).map_err(|e| format!("{path}: {e}"))
}

/// A new private key. It never leaves the node that made it.
pub async fn genkey() -> Result<String, String> {
    run("wg", &["genkey"], None).await
}

pub async fn pubkey(private: &str) -> Result<String, String> {
    run("wg", &["pubkey"], Some(private)).await
}

/// Bring `IFACE` up with this private key (a file, 0600) and address, listening on PORT. Idempotent: an
/// existing interface is reconfigured, so a key or address switch is this same call.
pub async fn up(key_file: &str, ip: Ipv4Addr) -> Result<(), String> {
    if run("ip", &["link", "show", IFACE], None).await.is_err() {
        run("ip", &["link", "add", IFACE, "type", "wireguard"], None).await?;
    }
    run("wg", &["set", IFACE, "private-key", key_file, "listen-port", &PORT.to_string()], None).await?;
    let cidr = format!("{ip}/{PREFIX}");
    // Only this address: a switch from the bootstrap address drops the old one.
    let current = run("ip", &["-4", "-o", "addr", "show", "dev", IFACE], None).await?;
    for old in current.lines().filter_map(|l| l.split_whitespace().skip_while(|w| *w != "inet").nth(1)) {
        if old != cidr {
            run("ip", &["addr", "del", old, "dev", IFACE], None).await?;
        }
    }
    if !current.contains(&format!("inet {cidr} ")) {
        run("ip", &["addr", "add", &cidr, "dev", IFACE], None).await?;
    }
    run("ip", &["link", "set", IFACE, "up"], None).await?;
    Ok(())
}

pub async fn live() -> Result<Vec<Live>, String> {
    Ok(parse_dump(&run("wg", &["show", IFACE, "dump"], None).await?))
}

/// `wg show dump`: the interface's line, then one tab-separated line per peer: key, psk, endpoint,
/// allowed-ips, latest handshake, rx, tx, keepalive.
pub fn parse_dump(dump: &str) -> Vec<Live> {
    dump.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let none = |s: &str| if s == "(none)" { String::new() } else { s.to_owned() };
            (f.len() >= 5).then(|| Live { key: f[0].into(), endpoint: none(f[2]), allowed: none(f[3]), handshake: f[4].parse().unwrap_or(0) })
        })
        .collect()
}

/// An endpoint a node may advertise: `<IPv4>:<port>`, `[<IPv6>]:<port>` or `<DNS name>:<port>` (`wg set`
/// resolves a name), port 1-65535.
pub fn endpoint_ok(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else { return false };
    let port_ok = port.parse::<u16>().is_ok_and(|p| p > 0);
    let host_ok = host.parse::<Ipv4Addr>().is_ok()
        || host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok())
        || (crate::edge::domain_ok(host) && host.parse::<Ipv4Addr>().is_err() && !host.bytes().all(|b| b.is_ascii_digit() || b == b'.'));
    port_ok && host_ok && s.len() <= 255
}

/// A node's name (a worker, an edge, a dashboard's mesh node): 1-32 of `[a-z0-9-]`, starting with a letter.
/// Its mesh node's, its certificate's and its rows'; nothing reads a number out of it.
pub fn node_name_ok(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// clap's check for a node's name (`--name`, `--worker`).
pub fn node_name(s: &str) -> Result<String, String> {
    if node_name_ok(s) {
        Ok(s.to_owned())
    } else {
        Err(format!("{s:?}: want 1-32 of a-z, 0-9 and -, starting with a letter"))
    }
}

/// A node's default name, from its hostname: lowercased, anything outside `[a-z0-9-]` a `-`, cut to 32, a
/// leading digit prefixed with `n` (still 32 at most). The join apps' `default_name` does the same.
pub fn default_node_name(hostname: &str) -> String {
    let h: String =
        hostname.trim().to_ascii_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' { c } else { '-' }).take(32).collect();
    if h.starts_with(|c: char| c.is_ascii_digit()) {
        format!("n{}", &h[..h.len().min(31)])
    } else {
        h
    }
}

/// A host's name as a node reports it at its join, shown on the dashboard's Nodes page: 1-64 printable
/// ASCII characters, no spaces (the kernel's limit is 64).
pub fn hostname_ok(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic())
}

/// This host's name (/proc/sys/kernel/hostname), if it passes `hostname_ok`.
pub fn hostname() -> Option<String> {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let h = h.trim();
    hostname_ok(h).then(|| h.to_owned())
}

/// Make the interface's peers exactly `want`: remove the others, add or update these. A peer's endpoint
/// is only set when known, so one the kernel learned from the peer itself isn't overwritten with nothing.
pub async fn sync(want: &[Peer]) -> Result<(), String> {
    let have = live().await?;
    for l in have.iter().filter(|l| !want.iter().any(|p| p.key == l.key)) {
        run("wg", &["set", IFACE, "peer", &l.key, "remove"], None).await?;
    }
    for p in want {
        let allowed = format!("{}/32", p.ip);
        let same = have.iter().any(|l| l.key == p.key && l.allowed == allowed && (p.endpoint.is_empty() || l.endpoint == p.endpoint));
        if same {
            continue;
        }
        let mut args = vec!["set", IFACE, "peer", &p.key, "allowed-ips", &allowed, "persistent-keepalive", "25"];
        if !p.endpoint.is_empty() {
            args.extend(["endpoint", &p.endpoint]);
        }
        run("wg", &args, None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_endpoints() {
        for ok in ["203.0.113.7:51820", "[2001:db8::1]:51820", "home.example.com:51820", "nuc:4500"] {
            assert!(endpoint_ok(ok), "{ok}");
        }
        for bad in ["", "203.0.113.7", "203.0.113.7:0", "203.0.113.7:70000", "1.2.3:51820", "2001:db8::1:51820", "a b:1", "-x.example:1", ":51820", "host:port"]
        {
            assert!(!endpoint_ok(bad), "{bad}");
        }
    }

    #[test]
    fn node_names_and_their_default() {
        for ok in ["nuc", "worker1", "edge-fra", "a", &"a".repeat(32), "box-2"] {
            assert!(node_name_ok(ok), "{ok}");
        }
        for bad in ["", "1box", "-x", "Nuc", "my_host", "a.b", &"a".repeat(33)] {
            assert!(!node_name_ok(bad), "{bad:?}");
        }
        assert_eq!(default_node_name("Nuc\n"), "nuc");
        assert_eq!(default_node_name("My_Host.local"), "my-host-local");
        assert_eq!(default_node_name("1box"), "n1box");
        assert_eq!(default_node_name(&"x".repeat(40)), "x".repeat(32));
        assert_eq!(default_node_name(&format!("9{}", "y".repeat(40))), format!("n9{}", "y".repeat(30)));
        assert!(node_name_ok(&default_node_name("nixAir")) && node_name_ok(&default_node_name("1box")));
    }

    #[test]
    fn hostnames() {
        for ok in ["nuc", "nixAir", "web-1.example.com", &"a".repeat(64)] {
            assert!(hostname_ok(ok), "{ok}");
        }
        for bad in ["", "two words", "tab\there", "new\nline", "naïve", &"a".repeat(65)] {
            assert!(!hostname_ok(bad), "{bad:?}");
        }
    }

    const K: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";

    #[test]
    fn loopback_backhauls_only_in_single_host_mode() {
        let a = |s: &str| s.parse::<std::net::SocketAddr>().unwrap();
        assert!(backhaul_ok(a("10.250.1.1:4280"), false) && !backhaul_ok(a("10.99.1.2:4280"), false));
        assert!(!backhaul_ok(a("127.0.0.1:4280"), false) && !backhaul_ok(a("[::1]:4280"), false), "loopback without --single-host");
        assert!(backhaul_ok(a("127.0.0.1:4280"), true) && backhaul_ok(a("[::1]:4280"), true) && backhaul_ok(a("10.250.1.1:4280"), true));
        assert!(!backhaul_ok(a("10.99.1.2:4280"), true) && !backhaul_ok(a("0.0.0.0:4280"), true), "still never off the mesh");
    }

    #[test]
    fn tokens_keys_and_addresses() {
        let t = Token { key: K.into(), ip: Ipv4Addr::new(10, 250, 255, 7), control_pub: K.into(), control: None };
        assert_eq!(Token::decode(&t.encode()), Ok(t.clone()));
        for c in ["203.0.113.7:51820", "[2001:db8::1]:51820", "mesh.example.com:51820"] {
            let t = Token { control: Some(c.into()), ..t.clone() };
            assert_eq!(Token::decode(&t.encode()), Ok(t.clone()), "{c}");
        }
        for bad in [
            "flyyy-join:x:10.250.255.7:x".to_owned(),
            format!("flyyy-join:{K}:10.250.1.7:{K}"), // not a bootstrap address
            format!("other:{K}:10.250.255.7:{K}"),
            format!("flyyy-join:{K}:10.250.255.7"),
            format!("flyyy-join:{K}:10.250.255.7:{K}@"),
            format!("flyyy-join:{K}:10.250.255.7:{K}@mesh.example.com"), // no port
        ] {
            assert!(Token::decode(&bad).is_err(), "{bad}");
        }
        assert!(key_ok(K) && !key_ok(&K.replace('=', "A")) && !key_ok("yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmB="));
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert!(is_node(ip("10.250.1.1")) && !is_node(CONTROL_IP) && !is_node(ip("10.250.255.1")) && !is_node(ip("10.99.1.1")));
    }

    #[test]
    fn dump_parsing() {
        let d = format!("priv\tpub\t51820\toff\n{K}\t(none)\t10.99.1.2:51820\t10.250.1.1/32\t1700000000\t1\t2\t25\n{K}\t(none)\t(none)\t(none)\t0\t0\t0\toff");
        let l = parse_dump(&d);
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].endpoint.as_str(), l[0].allowed.as_str(), l[0].handshake), ("10.99.1.2:51820", "10.250.1.1/32", 1700000000));
        assert_eq!((l[1].endpoint.as_str(), l[1].allowed.as_str()), ("", ""));
    }
}
