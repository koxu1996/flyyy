//! Ingress addresses, Fly-style. The control plane's flags name shared IPs (HTTP, routed by Host) and
//! pools; it writes them to `ip_pools` and alone allocates from them: on deploy each app gets a random
//! dedicated address from a pool (an `ips` row, kept across redeploys), released on destroy and not reused
//! for `QUARANTINE_MS`. The edges accept every prefix (AnyIP: `ip route add local <prefix> dev lo`, done by
//! `proxy anyip`) and route by (destination IP, port): an app's tcp services listen on its own address.
// shortcut: IPv4 only; one control plane allocates (a mutex there, no cross-process lock).
use std::{net::Ipv4Addr, time::Duration};
use tracing::{info, warn};

use corro_api_types::Statement;

use crate::corro::query;

/// A released address isn't handed out again for this long (clients and DNS may still point at it).
pub(crate) const QUARANTINE_MS: i64 = 24 * 3600 * 1000;

/// A pool can't be bigger than a /16 (allocation looks at every address in it).
const MIN_POOL_LEN: u8 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Prefix {
    pub(crate) net: Ipv4Addr,
    pub(crate) len: u8,
}

impl Prefix {
    pub(crate) fn parse(s: &str) -> Result<Prefix, String> {
        let (ip, len) = s.split_once('/').unwrap_or((s, "32"));
        let net: Ipv4Addr = ip.parse().map_err(|e| format!("{s:?}: {e}"))?;
        let len: u8 = len.parse().ok().filter(|l| *l <= 32).ok_or_else(|| format!("{s:?}: want a.b.c.d/0-32"))?;
        let p = Prefix { net, len };
        if u32::from(net) & !p.mask() != 0 {
            return Err(format!("{s:?}: host bits set (want {}/{len})", Ipv4Addr::from(u32::from(net) & p.mask())));
        }
        Ok(p)
    }

    fn mask(self) -> u32 {
        if self.len == 0 {
            0
        } else {
            u32::MAX << (32 - self.len)
        }
    }

    pub(crate) fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.net)
    }

    /// The addresses an app may get: all but the network and broadcast ones (a /31 or /32 has no such).
    fn hosts(self) -> impl Iterator<Item = Ipv4Addr> {
        let (first, last) = (u32::from(self.net), u32::from(self.net) | !self.mask());
        let (first, last) = if self.len <= 30 { (first + 1, last - 1) } else { (first, last) };
        (first..=last).map(Ipv4Addr::from)
    }
}

impl std::fmt::Display for Prefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.net, self.len)
    }
}

/// An `--ip-pool` value: a prefix of at most a /16 (allocation looks at every address in it).
pub(crate) fn pool(s: &str) -> Result<Prefix, String> {
    let p = Prefix::parse(s)?;
    if p.len < MIN_POOL_LEN {
        return Err(format!("{p}: at most a /{MIN_POOL_LEN}"));
    }
    Ok(p)
}

/// The pools together: none overlaps another.
pub(crate) fn pools_ok(pools: &[Prefix]) -> Result<(), String> {
    for (i, p) in pools.iter().enumerate() {
        if pools[..i].iter().any(|o| o.contains(p.net) || p.contains(o.net)) {
            return Err(format!("--ip-pool {p} overlaps another pool"));
        }
    }
    Ok(())
}

/// An app's dedicated address (an `ips` row, as the control plane publishes it): the app it is (or was)
/// for, and when it was allocated and released (0: allocated).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Lease {
    pub(crate) ip: Ipv4Addr,
    pub(crate) app: String,
    #[serde(default)]
    pub(crate) allocated_at: i64,
    pub(crate) released_at: i64,
}

/// A free address for a new app: in a pool, not shared, not allocated, not released in the last
/// QUARANTINE_MS. `pick` chooses among them (a random index), so addresses aren't guessable in order.
pub(crate) fn free(pools: &[Prefix], shared: &[Ipv4Addr], leases: &[Lease], now: i64, pick: impl FnOnce(usize) -> usize) -> Option<Ipv4Addr> {
    let busy = |ip: &Ipv4Addr| shared.contains(ip) || leases.iter().any(|l| &l.ip == ip && (l.released_at == 0 || now - l.released_at < QUARANTINE_MS));
    let candidates: Vec<Ipv4Addr> = pools.iter().flat_map(|p| p.hosts()).filter(|ip| !busy(ip)).collect();
    (!candidates.is_empty()).then(|| candidates[pick(candidates.len()) % candidates.len()])
}

/// The active `ip_pools` prefixes (anyip makes them local; the control plane compares its flags with them).
pub(crate) const ACTIVE_PREFIXES: &str = "SELECT prefix FROM ip_pools WHERE state = 'active'";

/// `proxy anyip`'s flags.
#[derive(clap::Args, Debug)]
pub struct AnyipArgs {
    /// The Corrosion agent's API (and token) to read ip_pools from.
    #[command(flatten)]
    corrosion: crate::corro::ApiArgs,
}

/// `proxy anyip`: make every active `ip_pools` prefix local on this host (AnyIP), so the edges can bind
/// and answer any address in it. Needs CAP_NET_ADMIN, which is why it isn't the edge.
/// A prefix already local (127.0.1.0/24 is inside lo's 127.0.0.0/8) is left alone: no privilege needed.
// shortcut: a retired prefix's route stays until reboot.
pub async fn anyip(args: AnyipArgs) {
    let api = args.corrosion.open();
    let client = api.client();
    let stmt = Statement::Simple(ACTIVE_PREFIXES.into());
    info!(corrosion = %api.addr, "anyip: making ip_pools prefixes local");
    loop {
        // Prefixes as text: one that doesn't parse is skipped (and logged), not the others.
        match query::<(String,)>(&client, stmt.clone()).await {
            Ok(rows) => {
                for (p,) in rows {
                    let Ok(p) = Prefix::parse(&p).map_err(|e| warn!(error = %e, "anyip: skipping a prefix")) else { continue };
                    if let Err(e) = make_local(p).await {
                        warn!(prefix = %p, error = %e, "anyip");
                    }
                }
            }
            Err(e) => warn!(error = %e, "anyip: reading ip_pools"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn make_local(p: Prefix) -> Result<(), String> {
    let probe = p.hosts().next().unwrap_or(p.net).to_string();
    let out = tokio::process::Command::new("ip").args(["-o", "route", "get", &probe]).output().await.map_err(|e| format!("ip route get: {e}"))?;
    if String::from_utf8_lossy(&out.stdout).starts_with("local ") {
        return Ok(());
    }
    let out = tokio::process::Command::new("ip")
        .args(["route", "replace", "local", &p.to_string(), "dev", "lo", "table", "local"])
        .output()
        .await
        .map_err(|e| format!("ip route: {e}"))?;
    if !out.status.success() {
        return Err(format!("ip route replace local {p} dev lo: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    info!(prefix = %p, "anyip: local now");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_flags_and_allocation() {
        let p = Prefix::parse("10.200.1.0/29").unwrap();
        assert!(p.contains("10.200.1.7".parse().unwrap()) && !p.contains("10.200.1.8".parse().unwrap()));
        assert_eq!(p.hosts().map(|i| i.to_string()).collect::<Vec<_>>(), ["10.200.1.1", "10.200.1.2", "10.200.1.3", "10.200.1.4", "10.200.1.5", "10.200.1.6"]);
        assert_eq!(Prefix::parse("10.200.0.1").unwrap().hosts().count(), 1);
        for bad in ["10.200.1.1/24", "10.200.1.0/33", "nope/24", "::1/128"] {
            assert!(Prefix::parse(bad).is_err(), "{bad}");
        }
        let f = |s: &str| -> Result<Vec<Prefix>, String> {
            let pools = s.split_whitespace().map(pool).collect::<Result<Vec<_>, _>>()?;
            pools_ok(&pools).map(|()| pools)
        };
        assert_eq!(f("10.200.1.0/24").unwrap(), [Prefix::parse("10.200.1.0/24").unwrap()]);
        for bad in ["10.0.0.0/8", "10.200.1.0/24 10.200.1.128/25", "x", "10.200.1.1/24"] {
            assert!(f(bad).is_err(), "{bad}");
        }

        // A /29 is 6 hosts; .1 is shared, .2 allocated, .3 released an hour ago (quarantined), .4 released
        // 25 h ago (free again): free are .4 .5 .6, and `pick` chooses among them.
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let now = 100 * QUARANTINE_MS;
        let lease = |s: &str, released_at| Lease { ip: ip(s), app: "a".into(), allocated_at: 0, released_at };
        let leases = [lease("10.200.1.2", 0), lease("10.200.1.3", now - 3_600_000), lease("10.200.1.4", now - QUARANTINE_MS - 3_600_000)];
        let shared = [ip("10.200.1.1")];
        let all: Vec<_> = (0..3).map(|i| free(&[p], &shared, &leases, now, |_| i).unwrap().to_string()).collect();
        assert_eq!(all, ["10.200.1.4", "10.200.1.5", "10.200.1.6"]);
        let full: Vec<Lease> = ["10.200.1.4", "10.200.1.5", "10.200.1.6"].iter().map(|s| lease(s, 0)).chain(leases.clone()).collect();
        assert_eq!(free(&[p], &shared, &full, now, |_| 0), None, "exhausted: the quarantined .3 stays out");
    }
}
