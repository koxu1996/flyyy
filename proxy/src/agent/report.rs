//! What a worker announces: its heartbeat (with its capacity) and its finished images' sizes.

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tracing::{info, warn};

use corro_api_types::Statement;
use corro_client::CorrosionApiClient;

use super::desired::Store;
use crate::{
    corro::query,
    states::Desired,
    util::{now_ms, write_atomic},
};

/// This worker's `images` rows (what it reported); the one parameter is the worker's name.
pub(crate) const REPORTED: &str = "SELECT digest, bytes FROM images WHERE worker = ?";

/// What a worker announces in its `workers` row besides the heartbeat.
pub(crate) struct Announce {
    pub(crate) worker: String,
    /// Where its backhaul and agent API listen. The control plane binds them to the worker's mesh identity
    /// before anyone uses them.
    pub(crate) backhaul: SocketAddr,
    pub(crate) agent_api: SocketAddr,
    /// (arch, CPUs, memory MiB).
    pub(crate) capacity: (&'static str, i64, i64),
}

/// The machines this worker holds that aren't destroyed: (count, CPUs, memory MiB), the control plane's
/// placement or not.
pub(crate) fn allocation(store: &Store) -> (i64, i64, i64) {
    store.all().values().filter(|r| r.desired != Desired::Destroyed).fold((0, 0, 0), |(n, c, m), r| (n + 1, c + r.spec.cpus as i64, m + r.spec.memory as i64))
}

/// This worker's capacity: its architecture, the CPUs it may use (affinity and cgroup limits counted) and
/// its memory in MiB (MemTotal). Read once: they don't change while it runs. An error rather than a 0 the
/// control plane would show as a worker with no memory.
pub(crate) fn capacity() -> Result<(&'static str, i64, i64), String> {
    let cpus = std::thread::available_parallelism().map_err(|e| format!("CPUs: {e}"))?.get() as i64;
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
    let kib =
        meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?.trim().parse::<i64>().ok()).ok_or("/proc/meminfo: no MemTotal")?;
    Ok((std::env::consts::ARCH, cpus, kib / 1024))
}

/// This worker's finished images (`images/<hex>.erofs`, not `.tmp`): sha256:<hex> -> bytes.
pub(crate) fn image_sizes(dir: &str) -> HashMap<String, i64> {
    let Ok(entries) = std::fs::read_dir(dir) else { return HashMap::new() };
    entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            let hex = name.strip_suffix(".erofs")?;
            (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| (format!("sha256:{hex}"), e.metadata().map_or(0, |m| m.len() as i64)))
        })
        .collect()
}

/// The image sizes to write: those that differ from what's in the table, and 0 for an image that's gone.
pub(crate) fn changed_images(last: &HashMap<String, i64>, now: &HashMap<String, i64>) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = now.iter().filter(|(d, b)| last.get(*d) != Some(b)).map(|(d, b)| (d.clone(), *b)).collect();
    out.extend(last.iter().filter(|(d, b)| **b != 0 && !now.contains_key(*d)).map(|(d, _)| (d.clone(), 0)));
    out.sort();
    out
}

/// Report this worker's finished images in the `images` table (one row per digest), every 5 s, writing only
/// the sizes that changed. What's there already is read first, so a restart rewrites nothing.
pub(crate) async fn report_images(client: CorrosionApiClient, worker: String, state_dir: String) {
    let dir = format!("{state_dir}/images");
    let stmt = Statement::WithParams(REPORTED.into(), vec![worker.clone().into()]);
    let mut last: HashMap<String, i64> = loop {
        match query::<(String, i64)>(&client, stmt.clone()).await {
            Ok(rows) => break rows.into_iter().collect(),
            Err(e) => warn!(error = %e, "images: reading the table"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    // What the table has for this worker, as far as this agent knows: once it equals the image files,
    // there's nothing left to write (the images suite waits for that instead of a report tick).
    // Its own directory: the state dir's *.json are machines (read_locals), images/*.json image records.
    let reported = format!("{state_dir}/reports/images.json");
    if let Err(e) = std::fs::create_dir_all(format!("{state_dir}/reports")) {
        warn!(error = %e, "images: reports directory");
    }
    let note = |last: &HashMap<String, i64>| {
        let sorted: BTreeMap<&String, &i64> = last.iter().collect();
        if let Err(e) = write_atomic(&reported, &serde_json::to_vec(&sorted).expect("serialisable")) {
            warn!(error = %e, "images: noting what was reported");
        }
    };
    note(&last);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let changed = changed_images(&last, &image_sizes(&dir));
        if changed.is_empty() {
            continue;
        }
        let stmts: Vec<Statement> = changed
            .iter()
            .map(|(d, b)| {
                Statement::WithParams(
                    "INSERT INTO images (worker, digest, bytes) VALUES (?, ?, ?) ON CONFLICT (worker, digest) DO UPDATE SET bytes = excluded.bytes".into(),
                    vec![worker.as_str().into(), d.as_str().into(), (*b).into()],
                )
            })
            .collect();
        // A failed write is retried on the next tick (`last` unchanged).
        match client.execute(&stmts, None).await {
            Ok(_) => {
                info!(images = %changed.iter().map(|(d, b)| format!("{} {b}", &d[7..19])).collect::<Vec<_>>().join(", "), "images reported");
                last.extend(changed);
                note(&last);
            }
            Err(e) => warn!(error = %e, "images: writing"),
        }
    }
}

/// Announce this worker: upsert its `workers` row every 5 s, in its own task so that a long pull in
/// the reconcile loop can't stall it. The heartbeat comes from the machine agent, not the backhaul,
/// because "alive" means "can run new machines": a worker whose agent is down still serves its
/// running machines through its backhaul, it just gets no new ones. Same values are no-op writes
/// for cr-sqlite, so each tick only changes `heartbeat_at`.
pub(crate) async fn heartbeat(client: CorrosionApiClient, announce: Announce, store: Arc<Store>) {
    let Announce { worker, backhaul, agent_api, capacity: (arch, cpus, memory) } = announce;
    let started_at = now_ms();
    info!(%worker, %arch, cpus, memory_mib = memory, "capacity");
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let (machines, allocated_cpus, allocated_memory) = allocation(&store);
        let stmt = Statement::WithParams(
            "INSERT INTO workers (name, backhaul, agent_api, started_at, heartbeat_at, arch, cpus, memory, machines, allocated_cpus, allocated_memory) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (name) DO UPDATE SET backhaul = excluded.backhaul, agent_api = excluded.agent_api, started_at = excluded.started_at, \
             heartbeat_at = excluded.heartbeat_at, arch = excluded.arch, cpus = excluded.cpus, memory = excluded.memory, machines = excluded.machines, \
             allocated_cpus = excluded.allocated_cpus, allocated_memory = excluded.allocated_memory"
                .into(),
            vec![
                worker.as_str().into(),
                backhaul.to_string().into(),
                agent_api.to_string().into(),
                started_at.into(),
                now_ms().into(),
                arch.into(),
                cpus.into(),
                memory.into(),
                machines.into(),
                allocated_cpus.into(),
                allocated_memory.into(),
            ],
        );
        // A failed write is retried on the next tick.
        if let Err(e) = client.execute(&[stmt], None).await {
            warn!(error = %e, "heartbeat");
        }
    }
}
