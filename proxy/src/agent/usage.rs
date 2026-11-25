//! Every machine's CPU and memory, sampled from its VM's unit (systemd's accounting of the Cloud
//! Hypervisor process) every SAMPLE; metrics.rs keeps the buckets, the agent API serves them. Where systemd doesn't
//! account the unit's memory (the memory controller not delegated to the agent's user manager), its main
//! process' resident memory instead: Cloud Hypervisor's own, the guest's touched memory included.

use std::{collections::HashMap, sync::Arc, time::Duration};
use tracing::warn;

use super::{
    desired::Store,
    sys::{systemctl, unit},
};
use crate::{metrics::Machines, states::Desired, util::now_ms};

pub(crate) const SAMPLE: Duration = Duration::from_secs(5);

pub(crate) async fn run(store: Arc<Store>, machines: Arc<Machines>) {
    loop {
        tokio::time::sleep(SAMPLE).await;
        let ids: Vec<(String, String)> = store.all().into_iter().filter(|(_, r)| r.desired != Desired::Destroyed).map(|(id, r)| (id, r.spec.app)).collect();
        if ids.is_empty() {
            continue;
        }
        let units: Vec<String> = ids.iter().map(|(id, _)| unit(id)).collect();
        let mut args = vec!["show", "-p", "Id,CPUUsageNSec,MemoryCurrent,MainPID"];
        args.extend(units.iter().map(String::as_str));
        match systemctl(&args).await {
            Ok(out) => {
                let at = now_ms();
                let by_unit = parse(&out);
                for (id, app) in &ids {
                    let Unit { cpu, memory, pid } = by_unit.get(&unit(id)).copied().unwrap_or_default();
                    let memory = memory.or_else(|| pid.filter(|p| *p > 0).and_then(rss));
                    machines.sample(id, app, cpu, memory, at);
                }
            }
            Err(e) => warn!(error = %e, "usage: reading the VMs' units"),
        }
    }
}

/// A unit's CPUUsageNSec, MemoryCurrent and MainPID; None where systemd has none ("[not set]", or u64::MAX
/// from older versions: a unit not running).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Unit {
    cpu: Option<u64>,
    memory: Option<u64>,
    pid: Option<u64>,
}

/// `systemctl show`'s blocks (one per unit, blank-line separated), by unit.
fn parse(out: &str) -> HashMap<String, Unit> {
    out.split("\n\n")
        .filter_map(|block| {
            let props: HashMap<&str, &str> = block.lines().filter_map(|l| l.split_once('=')).collect();
            let n = |k: &str| props.get(k).and_then(|v| v.parse::<u64>().ok()).filter(|v| *v != u64::MAX);
            Some((props.get("Id")?.to_string(), Unit { cpu: n("CPUUsageNSec"), memory: n("MemoryCurrent"), pid: n("MainPID") }))
        })
        .collect()
}

/// A process' resident memory in bytes (VmRSS in /proc/<pid>/status).
fn rss(pid: u64) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_systemctl_show() {
        use super::Unit;
        let out = "Id=fc-a.service\nCPUUsageNSec=1500\nMemoryCurrent=4096\nMainPID=42\n\nMemoryCurrent=[not set]\nId=fc-b.service\nCPUUsageNSec=[not set]\nMainPID=0\n\n\
                   Id=fc-c.service\nCPUUsageNSec=18446744073709551615\nMemoryCurrent=7\n";
        let p = super::parse(out);
        assert_eq!(p["fc-a.service"], Unit { cpu: Some(1500), memory: Some(4096), pid: Some(42) });
        assert_eq!(p["fc-b.service"], Unit { cpu: None, memory: None, pid: Some(0) });
        assert_eq!(p["fc-c.service"], Unit { cpu: None, memory: Some(7), pid: None });
        // This process' own: some memory.
        assert!(super::rss(std::process::id() as u64).is_some_and(|b| b > 0));
    }
}
