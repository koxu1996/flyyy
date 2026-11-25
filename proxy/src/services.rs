//! An app's services: what of a machine is reachable, how, and how it's checked. Each has a name, http or
//! tcp, the port in the VM, for tcp the edge port it's published on (on the app's own dedicated IP, so it
//! need only be unique within the app; it defaults to the VM port), and a health check. Workers run the
//! checks against their own VMs (agent.rs) and write `service_status`; edges route only to passing ones,
//! http by Host (`<app>.<apps-domain>`) on the shared IPs, tcp on (the app's IP, the edge port). `--port N`
//! is one http service named http.
// shortcut: at most one http service per app (its vhost is the app's; a second would need a second name
// under the apps domain, which a one-label wildcard certificate doesn't cover).
// Every binary includes this file and each uses only part of it.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Tcp,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Tcp => "tcp",
        }
    }
}

/// The health check: http GETs `path` and wants 2xx/3xx, or without a path GETs / and wants any answer
/// below 500 (the app speaks HTTP; plenty of images have no page at /). tcp wants the app to accept and keep
/// the connection open. Every `interval` seconds, each attempt given `timeout` seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: u32,
    #[serde(default = "default_timeout")]
    pub timeout: u32,
}

fn default_interval() -> u32 {
    5
}

fn default_timeout() -> u32 {
    2
}

impl Default for Check {
    fn default() -> Self {
        Check { path: None, interval: default_interval(), timeout: default_timeout() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub protocol: Protocol,
    /// In the VM.
    pub port: u16,
    /// tcp only: where the edges publish it, on the app's dedicated IP; unset is the VM port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_port: Option<u16>,
    #[serde(default)]
    pub check: Check,
    /// Concurrency per instance, per edge (requests in flight; connections, for tcp): the edges prefer
    /// instances under the soft limit, and never send one more than the hard limit (a request waits about
    /// 1 s for a slot, then gets 503; a tcp connection is closed). Unset: no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_limit: Option<u32>,
}

pub const MAX_SERVICES: usize = 16;

/// The most a limit can be (and what keeps it within placement's INTEGER).
pub const MAX_LIMIT: u32 = 100_000;

impl Service {
    /// tcp: the port the edges publish it on (edge_port, else its own); http: none (it's by Host).
    pub fn published_port(&self) -> Option<u16> {
        match self.protocol {
            Protocol::Tcp => Some(self.edge_port.unwrap_or(self.port)),
            Protocol::Http => None,
        }
    }
}

/// The `--port N` shortcut, and what a machine row without services means: one http service named http.
pub fn http_on(port: u16) -> Vec<Service> {
    vec![Service { name: "http".into(), protocol: Protocol::Http, port, edge_port: None, check: Check::default(), soft_limit: None, hard_limit: None }]
}

/// Edge ports an app's tcp service can't take: the edges' own (HTTP, HTTPS).
pub const RESERVED_EDGE_PORTS: [u16; 2] = [80, 443];

fn name_ok(s: &str) -> bool {
    (1..=20).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && !s.ends_with('-')
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One app's services. Edge ports are per IP, and each app has its own, so this is all there is to check.
pub fn services_ok(services: &[Service]) -> Result<(), String> {
    if services.is_empty() || services.len() > MAX_SERVICES {
        return Err(format!("services: {} of them, want 1-{MAX_SERVICES}", services.len()));
    }
    for (i, s) in services.iter().enumerate() {
        let n = &s.name;
        if !name_ok(n) {
            return Err(format!("service {n:?}: want a name [a-z][a-z0-9-]{{0,19}}"));
        }
        if services[..i].iter().any(|o| &o.name == n) {
            return Err(format!("service {n}: name used twice"));
        }
        if s.port == 0 || services[..i].iter().any(|o| o.port == s.port) {
            return Err(format!("service {n}: port {}: want 1-65535, one service per port", s.port));
        }
        match (s.protocol, s.published_port()) {
            (Protocol::Http, _) if s.edge_port.is_some() => return Err(format!("service {n}: http is routed by Host; no edge_port")),
            (Protocol::Tcp, Some(p)) if p == 0 || RESERVED_EDGE_PORTS.contains(&p) => {
                return Err(format!("service {n}: edge port {p}: want 1-65535, not {RESERVED_EDGE_PORTS:?} (set edge_port)"))
            }
            (Protocol::Tcp, Some(p)) if services[..i].iter().any(|o| o.published_port() == Some(p)) => {
                return Err(format!("service {n}: edge port {p} used twice"))
            }
            _ => {}
        }
        let c = &s.check;
        if let Some(p) = &c.path {
            if !p.starts_with('/') || p.len() > 256 || !p.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(format!("service {n}: check path {p:?}: want /..., printable, no spaces, at most 256"));
            }
        }
        if !(1..=300).contains(&c.interval) || c.timeout == 0 || c.timeout > c.interval {
            return Err(format!("service {n}: check interval {} s / timeout {} s: want 1-300, timeout 1..=interval", c.interval, c.timeout));
        }
        for (what, l) in [("soft", s.soft_limit), ("hard", s.hard_limit)] {
            if l.is_some_and(|l| !(1..=MAX_LIMIT).contains(&l)) {
                return Err(format!("service {n}: {what} limit {}: want 1-{MAX_LIMIT}", l.unwrap_or(0)));
            }
        }
        if let (Some(soft), Some(hard)) = (s.soft_limit, s.hard_limit) {
            if soft > hard {
                return Err(format!("service {n}: soft limit {soft} over the hard limit {hard}"));
            }
        }
    }
    if services.iter().filter(|s| s.protocol == Protocol::Http).count() > 1 {
        return Err("services: at most one http service (it's the app's vhost)".into());
    }
    Ok(())
}

/// `--service NAME=PROTO:PORT[@EDGE_PORT][,path=/p][,interval=S][,timeout=S][,soft=N][,hard=N]`, as the CLI
/// takes it (a tcp service without @EDGE_PORT is published on its own port; soft and hard are the limits).
pub fn parse_flag(s: &str) -> Result<Service, String> {
    let bad = || format!("--service {s:?}: want NAME=http:PORT or NAME=tcp:PORT[@EDGE_PORT], then ,path=/p ,interval=S ,timeout=S ,soft=N ,hard=N");
    let (head, opts) = s.split_once(',').map_or((s, ""), |(h, o)| (h, o));
    let (name, spec) = head.split_once('=').ok_or_else(bad)?;
    let (proto, ports) = spec.split_once(':').ok_or_else(bad)?;
    let protocol = match proto {
        "http" => Protocol::Http,
        "tcp" => Protocol::Tcp,
        _ => return Err(bad()),
    };
    let (port, edge_port) = match ports.split_once('@') {
        Some((p, e)) => (p, Some(e.parse::<u16>().map_err(|_| bad())?)),
        None => (ports, None),
    };
    let (mut check, mut soft_limit, mut hard_limit) = (Check::default(), None, None);
    for o in opts.split(',').filter(|o| !o.is_empty()) {
        match o.split_once('=').ok_or_else(bad)? {
            ("path", v) => check.path = Some(v.into()),
            ("interval", v) => check.interval = v.parse().map_err(|_| bad())?,
            ("timeout", v) => check.timeout = v.parse().map_err(|_| bad())?,
            ("soft", v) => soft_limit = Some(v.parse().map_err(|_| bad())?),
            ("hard", v) => hard_limit = Some(v.parse().map_err(|_| bad())?),
            _ => return Err(bad()),
        }
    }
    Ok(Service { name: name.into(), protocol, port: port.parse().map_err(|_| bad())?, edge_port, check, soft_limit, hard_limit })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_limits() {
        let web = parse_flag("web=http:8080,path=/healthz,interval=3,timeout=1").unwrap();
        assert_eq!(
            (web.protocol, web.port, web.edge_port, web.check.path.as_deref(), web.check.interval, web.check.timeout),
            (Protocol::Http, 8080, None, Some("/healthz"), 3, 1)
        );
        let lim = parse_flag("web=http:80,soft=4,hard=8").unwrap();
        assert_eq!((lim.soft_limit, lim.hard_limit), (Some(4), Some(8)));
        assert!(services_ok(&[lim]).is_ok());
        let db = parse_flag("db=tcp:5432@15432").unwrap();
        assert_eq!((db.protocol, db.port, db.edge_port, db.check.clone()), (Protocol::Tcp, 5432, Some(15432), Check::default()));
        assert!(services_ok(&[web.clone(), db.clone()]).is_ok() && services_ok(&http_on(80)).is_ok());
        let pg = parse_flag("pg=tcp:5432").unwrap();
        assert_eq!((pg.edge_port, pg.published_port(), web.published_port()), (None, Some(5432), None));
        assert!(services_ok(&[pg, web.clone()]).is_ok());
        for bad in ["web", "web=udp:1", "web=http:x", "db=tcp:1@x", "web=http:80,bogus=1"] {
            assert!(parse_flag(bad).is_err(), "{bad}");
        }
        let svc = |f: &str| parse_flag(f).unwrap();
        for bad in [
            vec![],
            vec![svc("Web=http:80")],
            vec![svc("a=http:80"), svc("b=http:81")],         // two http
            vec![svc("a=http:80"), svc("a=tcp:81@9000")],     // name twice
            vec![svc("a=tcp:80@9000"), svc("b=tcp:80@9001")], // port twice
            vec![svc("a=tcp:80@9000"), svc("b=tcp:81@9000")], // edge port twice
            vec![svc("a=tcp:9000"), svc("b=tcp:81@9000")],    // ... once by default
            vec![svc("a=tcp:443")],                           // defaults to the edges' own
            vec![svc("a=http:80@9000")],                      // http with edge port
            vec![svc("a=tcp:80@443")],                        // the edges' own
            vec![svc("a=http:80,path=nope")],
            vec![svc("a=http:80,interval=2,timeout=3")],
            vec![svc("a=http:80,soft=9,hard=8")], // soft over hard
            vec![svc("a=http:80,hard=0")],
        ] {
            assert!(services_ok(&bad).is_err(), "{bad:?}");
        }
    }
}
