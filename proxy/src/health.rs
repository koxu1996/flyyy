//! The agent's health checks: each service of each of this worker's running machines (services.rs), against
//! the VM over vsock, every `check.interval` seconds. Results go to `service_status` (one row per machine
//! and service, `<machine>.<service>`), written only when a row changes: passing, failing (also while it boots), or down
//! once its machine isn't running any more. The table is read first, so a restart rewrites nothing.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{info, warn};

use corro_api_types::Statement;
use corro_client::CorrosionApiClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    corro::query,
    services::{Protocol, Service},
    states::Health,
    util::now_ms,
    vsock::{dial_vsock, socket},
};

/// What to check: this worker's running machines (id -> (app, services)). None until the agent's first
/// reconcile, so nothing is marked down before it knows what runs.
pub(crate) type Targets = Arc<Mutex<Option<BTreeMap<String, (String, Vec<Service>)>>>>;

/// This worker's `service_status` rows, read back at start; the one parameter is the worker's name.
pub(crate) const STORED: &str = "SELECT id, app, protocol, addr, edge_port, state FROM service_status WHERE worker = ?";

/// A `service_status` row of this worker, as read back at start.
#[derive(serde::Deserialize)]
struct Stored {
    id: String,
    app: String,
    protocol: Protocol,
    addr: String,
    edge_port: u16,
    state: Health,
}

/// One `service_status` row's content (what's compared to decide on a write).
#[derive(Debug, Clone, PartialEq)]
struct Row {
    app: String,
    protocol: Protocol,
    addr: String,
    edge_port: u16,
    state: Health,
}

/// http: GET the path, want 2xx/3xx (no path: GET /, want any status below 500). tcp: the app accepts and keeps the connection (the guest's vsock
/// shim connects to it; if nothing listens there, the shim closes at once).
async fn check(run_dir: &str, machine: &str, app: &str, s: &Service) -> Result<(), String> {
    let attempt = async {
        let mut conn = dial_vsock(&socket(run_dir, machine), s.port).await?;
        match s.protocol {
            Protocol::Http => {
                let req = format!("GET {} HTTP/1.0\r\nHost: {app}\r\nUser-Agent: flyyy-check\r\n\r\n", s.check.path.as_deref().unwrap_or("/"));
                conn.write_all(req.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
                let mut buf = Vec::new();
                while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 512 {
                    let mut chunk = [0u8; 256];
                    match conn.read(&mut chunk).await {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(e) => return Err(format!("read: {e}")),
                    }
                }
                let line = String::from_utf8_lossy(&buf);
                let code: u16 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|c| c.parse().ok())
                    .ok_or_else(|| format!("no HTTP status: {:?}", line.lines().next().unwrap_or("")))?;
                let ok = if s.check.path.is_some() { (200..400).contains(&code) } else { code < 500 };
                if ok {
                    Ok(())
                } else {
                    Err(format!("HTTP {code}"))
                }
            }
            Protocol::Tcp => match tokio::time::timeout(Duration::from_millis(300), conn.read(&mut [0u8; 1])).await {
                Err(_) | Ok(Ok(1..)) => Ok(()),
                Ok(Ok(0)) => Err("closed: nothing listening on the port?".into()),
                Ok(Err(e)) => Err(format!("read: {e}")),
            },
        }
    };
    tokio::time::timeout(Duration::from_secs(s.check.timeout.into()), attempt).await.map_err(|_| format!("timed out after {} s", s.check.timeout))?
}

/// The writes due: rows that changed, and down for services that aren't targets any more (`live`; one
/// still waiting for its first result keeps its row).
fn changes(last: &HashMap<String, Row>, now: &HashMap<String, Row>, live: &HashSet<String>) -> Vec<(String, Row)> {
    let mut out: Vec<(String, Row)> = now.iter().filter(|(id, r)| last.get(*id) != Some(r)).map(|(id, r)| (id.clone(), r.clone())).collect();
    out.extend(
        last.iter().filter(|(id, r)| r.state != Health::Down && !live.contains(*id)).map(|(id, r)| (id.clone(), Row { state: Health::Down, ..r.clone() })),
    );
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub(crate) async fn run(client: CorrosionApiClient, worker: String, run_dir: String, targets: Targets) {
    let stmt = Statement::WithParams(STORED.into(), vec![worker.clone().into()]);
    let mut last: HashMap<String, Row> = loop {
        match query::<Stored>(&client, stmt.clone()).await {
            Ok(rows) => {
                break rows
                    .into_iter()
                    .map(|s| (s.id, Row { app: s.app, protocol: s.protocol, addr: s.addr, edge_port: s.edge_port, state: s.state }))
                    .collect();
            }
            Err(e) => warn!(error = %e, "reading service_status"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    // Each service's latest result (state, error), and when it's next due.
    let results: Arc<Mutex<HashMap<String, (Health, String)>>> = Arc::default();
    let mut due: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let Some(targets) = targets.lock().unwrap().clone() else { continue };
        let mut now: HashMap<String, Row> = HashMap::new();
        for (machine, (app, services)) in &targets {
            for s in services {
                let id = format!("{machine}.{}", s.name);
                if due.get(&id).is_none_or(|t| Instant::now() >= *t) {
                    // Not passing (booting, or failing): again in 1 s, so it's routed as soon as it passes.
                    let passing = results.lock().unwrap().get(&id).is_some_and(|(st, _)| *st == Health::Passing);
                    let every = if passing { s.check.interval } else { 1 };
                    due.insert(id.clone(), Instant::now() + Duration::from_secs(every.into()));
                    let (machine, app, s, id, results, run_dir) = (machine.clone(), app.clone(), s.clone(), id.clone(), results.clone(), run_dir.clone());
                    tokio::spawn(async move {
                        let r = check(&run_dir, &machine, &app, &s).await;
                        let v = match r {
                            Ok(()) => (Health::Passing, String::new()),
                            Err(e) => (Health::Failing, e),
                        };
                        results.lock().unwrap().insert(id, v);
                    });
                }
                // Not written until its first check is in.
                if let Some((state, _)) = results.lock().unwrap().get(&id) {
                    let addr = format!("vsock:{machine}:{}", s.port);
                    now.insert(id, Row { app: app.clone(), protocol: s.protocol, addr, edge_port: s.published_port().unwrap_or(0), state: *state });
                }
            }
        }
        // Services gone from the targets: forget their results and schedule (down is written below).
        let live: HashSet<String> = targets.iter().flat_map(|(m, (_, ss))| ss.iter().map(move |s| format!("{m}.{}", s.name))).collect();
        results.lock().unwrap().retain(|id, _| live.contains(id));
        due.retain(|id, _| live.contains(id));
        let changed = changes(&last, &now, &live);
        if changed.is_empty() {
            continue;
        }
        let errors = results.lock().unwrap().clone();
        let stmts: Vec<Statement> = changed
            .iter()
            .map(|(id, r)| {
                let (machine, service) = id.split_once('.').expect("<machine>.<service>");
                let error = errors.get(id).map(|(_, e)| e.clone()).unwrap_or_default();
                Statement::WithParams(
                    "INSERT INTO service_status (id, machine, worker, app, service, protocol, addr, edge_port, state, error, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (id, worker) DO UPDATE SET app = excluded.app, protocol = excluded.protocol, addr = excluded.addr, edge_port = excluded.edge_port, \
                     state = excluded.state, error = excluded.error, updated_at = excluded.updated_at"
                        .into(),
                    vec![
                        id.as_str().into(), machine.into(), worker.as_str().into(), r.app.as_str().into(), service.into(), r.protocol.as_str().into(),
                        r.addr.as_str().into(), (r.edge_port as i64).into(), r.state.to_string().into(), error.into(), now_ms().into(),
                    ],
                )
            })
            .collect();
        // A failed write is retried on the next tick (`last` unchanged).
        match client.execute(&stmts, None).await {
            Ok(_) => {
                for (id, r) in changed {
                    info!(instance = %id, state = %r.state, error = %errors.get(&id).map(|(_, e)| e.as_str()).unwrap_or(""), "health");
                    last.insert(id, r);
                }
            }
            Err(e) => warn!(error = %e, "writing service_status"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_only_changes_and_downs_what_is_gone() {
        let row = |state: &str| Row { app: "a".into(), protocol: Protocol::Http, addr: "vsock:m:80".into(), edge_port: 0, state: state.parse().unwrap() };
        let last = HashMap::from([("m.http".to_string(), row("passing")), ("n.http".to_string(), row("passing")), ("o.http".to_string(), row("down"))]);
        let live = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        // m unchanged; n's machine gone -> down; o already down -> nothing; p new.
        let now = HashMap::from([("m.http".to_string(), row("passing")), ("p.http".to_string(), row("failing"))]);
        let got: Vec<(String, Health)> = changes(&last, &now, &live(&["m.http", "p.http"])).into_iter().map(|(id, r)| (id, r.state)).collect();
        assert_eq!(got, [("n.http".to_string(), Health::Down), ("p.http".to_string(), Health::Failing)]);
        assert!(changes(&last, &HashMap::from([("m.http".to_string(), row("passing")), ("n.http".to_string(), row("passing"))]), &live(&["m.http", "n.http"]))
            .is_empty());
        // After a restart: n still a target, its first result not in yet -> its row stays.
        assert!(changes(&last, &HashMap::from([("m.http".to_string(), row("passing"))]), &live(&["m.http", "n.http"])).is_empty());
    }
}
