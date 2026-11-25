//! The agent's loop: desired state from the control plane (desired.rs), the VMs here, a plan, its actions
//! (draining machines first), and the facts reported back.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{info, warn};

use backon::BackoffBuilder;
use corro_api_types::Statement;
use tokio::{io::AsyncReadExt, sync::Notify};

use super::{
    plan::{fact, plan, Action, Actual, Report},
    sys::{now_secs, systemctl, unit},
    vm::{ready, READY_TIMEOUT},
    Agent, Failure,
};
use crate::agent::SocketAddr;
use crate::{
    corro::query,
    images::Status,
    states::{Desired, Fact},
    vsock,
};

/// Drain: a stopping machine stays up until its services are written down and the edges have stopped
/// sending it requests: no CONNECT for it at this worker's backhaul (its /state) for DRAIN_QUIET, counted
/// from when the rows went down (so an idle machine waits that long for the gossip, not more). At most
/// DRAIN_MAX.
// shortcut: "no new requests", not "no open connections": those are cut when the VM stops.
pub(crate) const DRAIN_QUIET: Duration = Duration::from_millis(500);

/// The draining machines' services that aren't down yet; `{marks}` is a `?` per machine.
pub(crate) const UP: &str = "SELECT id FROM service_status WHERE machine IN ({marks}) AND state != 'down'";

pub(crate) const DRAIN_MAX: Duration = Duration::from_secs(10);

/// The backhaul's /state (the agent's --backhaul-state): how long ago each instance it
/// serves got its last CONNECT.
pub(crate) async fn backhaul_quiet_ms(addr: SocketAddr) -> Result<HashMap<String, u64>, String> {
    let body = tokio::time::timeout(Duration::from_secs(2), async {
        let mut s = tokio::net::TcpStream::connect(&addr).await.map_err(|e| format!("{addr}: {e}"))?;
        tokio::io::AsyncWriteExt::write_all(&mut s, format!("GET /state HTTP/1.0\r\nHost: {addr}\r\n\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(buf)
    })
    .await
    .map_err(|_| format!("{addr}: timed out"))??;
    let text = String::from_utf8_lossy(&body);
    let json = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("{addr}/state: {e}"))?;
    Ok(v["last_connect_ms_ago"].as_object().into_iter().flatten().filter_map(|(k, ms)| Some((k.clone(), ms.as_u64()?))).collect())
}

impl Agent {
    pub(crate) fn fail(&mut self, id: &str, error: String) {
        let backoff = &self.backoff;
        let f = self.failures.entry(id.to_owned()).or_insert_with(|| Failure { error: String::new(), retry_at: Instant::now(), delays: backoff.build() });
        let delay = f.delays.next().expect("unbounded backoff");
        f.error = error;
        f.retry_at = Instant::now() + delay;
        warn!(machine = %id, retry_in = ?delay, error = %f.error, "machine failed");
    }

    /// Write the facts that changed since the last report, in one transaction.
    pub(crate) async fn report(&mut self, facts: Vec<(String, String, Report)>) {
        let changed: Vec<_> = facts.into_iter().filter(|(id, app, f)| self.reported.get(id) != Some(&(app.clone(), f.clone()))).collect();
        if changed.is_empty() {
            return;
        }
        let stmts: Vec<Statement> = changed
            .iter()
            .map(|(id, app, Report { state, addr, error })| {
                Statement::WithParams(
                    "INSERT INTO machine_status (id, worker, app, state, addr, error, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (id, worker) DO UPDATE SET app = excluded.app, state = excluded.state, \
                     addr = excluded.addr, error = excluded.error, updated_at = excluded.updated_at"
                        .into(),
                    vec![
                        id.as_str().into(),
                        self.worker.as_str().into(),
                        app.as_str().into(),
                        state.to_string().into(),
                        addr.as_str().into(),
                        error.as_str().into(),
                        now_secs().into(),
                    ],
                )
            })
            .collect();
        match self.client.execute(&stmts, None).await {
            Ok(_) => {
                for (id, app, f) in changed {
                    info!(machine = %id, state = %f.state, addr = %f.addr, error = %f.error, "fact");
                    self.reported.insert(id, (app, f));
                }
            }
            // Not marked as reported, so the next round retries.
            Err(e) => warn!(error = %e, "writing facts"),
        }
    }

    /// Wait until these machines' services are down in service_status and their backhaul has had no CONNECT
    /// for them for DRAIN_QUIET since; at most DRAIN_MAX. Says what ended the wait.
    pub(crate) async fn drain(&self, machines: &[&String]) -> String {
        let t0 = Instant::now();
        let marks = vec!["?"; machines.len()].join(", ");
        let stmt = Statement::WithParams(UP.replace("{marks}", &marks), machines.iter().map(|m| m.as_str().into()).collect());
        let mut down_at: Option<Instant> = None;
        loop {
            if t0.elapsed() >= DRAIN_MAX {
                return format!("gave up after {} s (still up in service_status or still getting requests)", DRAIN_MAX.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            if down_at.is_none() {
                match query::<(String,)>(&self.client, stmt.clone()).await {
                    Ok(rows) if rows.is_empty() => down_at = Some(Instant::now()),
                    Ok(_) => continue,
                    Err(e) => return format!("can't read service_status ({e}); not waiting"),
                }
            }
            let since_down = down_at.expect("set above").elapsed();
            let quiet = match backhaul_quiet_ms(self.backhaul_state).await {
                Ok(ago) => ago
                    .iter()
                    .filter(|(id, _)| machines.iter().any(|m| id.split_once('.').is_some_and(|(mid, _)| mid == m.as_str())))
                    .all(|(_, ms)| Duration::from_millis(*ms) >= DRAIN_QUIET),
                Err(e) => return format!("services down, but the backhaul's state is unreadable ({e})"),
            };
            if quiet && since_down >= DRAIN_QUIET {
                return format!("services down and no requests for {} ms, after {} ms", DRAIN_QUIET.as_millis(), t0.elapsed().as_millis());
            }
        }
    }

    pub(crate) async fn reconcile(&mut self) -> Result<(), String> {
        let records = self.store.all();
        let desired: BTreeMap<String, Desired> = records.iter().map(|(id, r)| (id.clone(), r.desired)).collect();
        let apps: HashMap<String, String> = records.iter().map(|(id, r)| (id.clone(), r.spec.app.clone())).collect();
        let specs: HashMap<String, super::spec::Spec> = records.into_iter().map(|(id, r)| (id, r.spec)).collect();
        let mut locals = self.read_locals()?;
        let running = self.running().await?;

        // Images of machines still to be created: ready, building (`pulling`), or failed.
        let mut images = HashMap::new();
        let mut pulling = HashSet::new();
        let mut image_errors = HashMap::new();
        for (id, spec) in &specs {
            if desired[id] == Desired::Destroyed || locals.contains_key(id) {
                continue;
            }
            match self.images.ensure(&spec.image) {
                Status::Ready { file, config } => {
                    images.insert(id.clone(), (file, config));
                }
                Status::Pending => {
                    pulling.insert(id.clone());
                }
                Status::Failed(e) => {
                    pulling.insert(id.clone());
                    image_errors.insert(id.clone(), format!("image: {e}"));
                }
            }
        }

        // A unit we started that is gone before it became ready: that start failed.
        for id in self.starting.keys().cloned().collect::<Vec<_>>() {
            if !running.contains(&id) {
                self.starting.remove(&id);
                let result = systemctl(&["show", "-P", "Result", &unit(&id)]).await.unwrap_or_default();
                self.fail(&id, format!("{} exited before its port answered (Result={})", unit(&id), result.trim()));
            }
        }

        let ids: HashSet<&String> = locals.keys().chain(running.iter()).collect();
        let actual = ids.into_iter().map(|id| (id.clone(), Actual { local: locals.contains_key(id), running: running.contains(id) })).collect();
        let now = Instant::now();
        let backoff = self.failures.iter().filter(|(_, f)| f.retry_at > now).map(|(id, _)| id.clone()).collect();
        self.pulling = !pulling.is_empty();
        let mut failed_now = HashSet::new();
        let actions = plan(&desired, &actual, &backoff, &pulling);
        // Drain: machines about to stop leave the health targets (their services are written down, so the
        // edges stop routing to them); they go once that's done and the edges' requests have stopped.
        let draining: Vec<&String> =
            actions.iter().filter(|(id, act)| matches!(act, Action::Stop | Action::Destroy) && running.contains(id)).map(|(id, _)| id).collect();
        let routed = {
            let mut t = self.targets.lock().unwrap();
            draining.iter().filter(|id| t.as_mut().is_some_and(|t| t.remove(**id).is_some())).count()
        };
        if routed > 0 {
            let names = draining.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
            info!(machines = %names, "draining");
            let why = self.drain(&draining).await;
            info!(machines = %names, why = %why, "drained");
        }
        for (id, act) in actions {
            // A failed Create means no Start this round (and the Create error stays the one reported).
            if failed_now.contains(&id) {
                continue;
            }
            let t0 = Instant::now();
            // Edges stop routing to it first (the fallback covers the gossip delay).
            if matches!(act, Action::Stop | Action::Destroy) && desired.contains_key(&id) {
                let addr = locals.get(&id).map_or(String::new(), |l| format!("vsock:{}:{}", l.spec.id, l.port));
                self.report(vec![(id.clone(), apps[&id].clone(), Report { state: Fact::Stopping, addr, error: String::new() })]).await;
            }
            let r = match act {
                Action::Create => {
                    let (file, config) = &images[&id];
                    self.create(&specs[&id], file, config).map(|l| {
                        locals.insert(id.clone(), l);
                    })
                }
                Action::Start => match locals.remove(&id) {
                    Some(l) => {
                        let r = self.start(&l).await;
                        locals.insert(id.clone(), l);
                        r
                    }
                    None => Err("no local state".into()),
                },
                Action::Stop => self.stop(&id).await,
                Action::Destroy => {
                    locals.remove(&id);
                    self.destroy(&id).await
                }
            };
            match r {
                Ok(()) => {
                    info!(machine = %id, action = ?act, ms = t0.elapsed().as_millis() as u64, "done");
                    if matches!(act, Action::Stop | Action::Destroy) {
                        self.failures.remove(&id);
                    }
                }
                Err(e) => {
                    self.fail(&id, format!("{act:?}: {e}"));
                    failed_now.insert(id);
                }
            }
        }

        // Readiness: a running machine is started once its app accepts a connection through vsock.
        let running = self.running().await?;
        self.ready.retain(|id| running.contains(id));
        let mut not_ready = Vec::new();
        for (id, l) in &locals {
            if running.contains(id) && !self.ready.contains(id) {
                match ready(&vsock::socket(&self.dirs.run, id), l.port).await {
                    Ok(()) => {
                        info!(machine = %id, at = %format!("vsock:{id}:{}", l.port), "ready");
                        self.ready.insert(id.clone());
                        self.starting.remove(id);
                        self.failures.remove(id);
                    }
                    Err(e) if self.starting.get(id).is_some_and(|t| t.elapsed() > READY_TIMEOUT) => not_ready.push((id.clone(), e)),
                    Err(_) => {}
                }
            }
        }
        // Never ready: stop it and fail it, so it's retried with backoff (and the fact says why).
        for (id, e) in not_ready {
            let _ = self.stop(&id).await;
            self.fail(&id, format!("port {} not accepting after {}s: {e}", locals[&id].port, READY_TIMEOUT.as_secs()));
        }

        let facts = desired
            .iter()
            .map(|(id, want)| {
                let f = match (image_errors.get(id), pulling.contains(id) && !locals.contains_key(id)) {
                    (Some(e), _) => Report::failed(e.clone()),
                    (None, true) => Report { state: Fact::Pulling, addr: String::new(), error: String::new() },
                    _ => fact(*want, locals.get(id), running.contains(id), self.ready.contains(id), self.failures.get(id).map(|f| f.error.as_str())),
                };
                (id.clone(), apps[id].clone(), f)
            })
            .collect();
        self.report(facts).await;
        // Health checks for what runs now: one still booting fails its checks (not down), so a restarted
        // agent or VM doesn't mark it gone.
        let targets = locals.iter().filter(|(id, _)| running.contains(*id)).map(|(id, l)| (id.clone(), (l.spec.app.clone(), l.spec.services()))).collect();
        *self.targets.lock().unwrap() = Some(targets);
        Ok(())
    }

    /// Wake on every desired-state change, else every 10 s (0.5 s while something is in flight). The first
    /// round is at once: the desired state is on disk, so there's no snapshot to wait for.
    pub(crate) async fn run(mut self, changed: Arc<Notify>) {
        let mut first = true;
        loop {
            let busy = !self.starting.is_empty() || !self.failures.is_empty() || self.pulling;
            if !std::mem::take(&mut first) {
                tokio::select! {
                    _ = changed.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(if busy { 500 } else { 10_000 })) => {}
                }
            }
            if let Err(e) = self.reconcile().await {
                warn!(error = %e, "reconcile");
            }
        }
    }
}
