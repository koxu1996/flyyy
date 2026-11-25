//! The reconciler: one level-triggered round over every app, woken by API writes or every 5 s. It decides
//! machines into the store, publishes the leases, and brings every worker's agent in line (agents.rs).

use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tracing::{info, warn};

use super::{
    db::MachineRec,
    events::Seen,
    model::{live_workers, valid_worker, Machine},
    placement::{decide, Op},
    Control,
};
use crate::{
    agent::spec::Spec,
    random,
    states::{Desired, Lifecycle},
    util::now_ms,
};

impl Control {
    /// One round over every app; all its machine changes in one write.
    pub(crate) async fn reconcile(&self) -> Result<(), String> {
        if !self.pools_written.load(std::sync::atomic::Ordering::Relaxed) {
            self.write_pools().await?;
            self.pools_written.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let apps = self.apps(None);
        let mut machines = self.machines().await?;
        let rows = self.workers().await?;
        // What changed since the last round, for the apps' events.
        let checks = self.checks().await?;
        self.events.observe(Seen::of(&machines, &checks, &rows, now_ms()), now_ms());
        for w in &rows {
            if let Err(e) = valid_worker(w, self.single_host) {
                warn!(error = %e, "ignoring workers row");
            }
        }
        // New machines only where the edges and the control plane can reach them: its backhaul and agent API
        // bound to its mesh identity.
        let workers: Vec<String> = live_workers(&rows, now_ms(), self.single_host)
            .into_iter()
            .filter(|n| rows.iter().any(|w| &w.name == n && self.bound(w, &w.backhaul).is_ok() && self.bound(w, &w.agent_api).is_ok()))
            .collect();
        let mut created: BTreeMap<String, MachineRec> = BTreeMap::new();
        let mut destroyed = Vec::new();
        for app in &apps {
            let live = machines.iter().filter(|m| m.managed && m.app == app.name && m.live()).count();
            if workers.is_empty() && app.state == Lifecycle::Active && live < app.count as usize {
                warn!(app = %app.name, missing = app.count as usize - live, "wants more machines, but no worker is alive");
            }
            for op in decide(app, &machines, &workers) {
                match op {
                    Op::Create { worker } => {
                        let id = format!("{}-{}", app.name, random::hex(3)?);
                        let spec = Spec {
                            id: id.clone(),
                            app: app.name.clone(),
                            image: app.image.clone(),
                            cmd: app.cmd.iter().map(|a| a.replace("{id}", &id)).collect(),
                            env: app.env.iter().map(|(k, v)| (k.clone(), v.replace("{id}", &id))).collect(),
                            port: app.port,
                            cpus: app.cpus,
                            memory: app.memory,
                            services: app.services.clone(),
                        };
                        info!(app = %app.name, machine = %id, %worker, "create");
                        self.events.add(&app.name, &id, format!("placed on {worker}"), now_ms());
                        // Count it for the next app's placement in this same round.
                        machines.push(Machine {
                            id: id.clone(),
                            worker: worker.clone(),
                            app: app.name.clone(),
                            managed: true,
                            desired: Desired::Started,
                            state: None,
                            addr: String::new(),
                            error: String::new(),
                            cpus: app.cpus,
                            memory: app.memory,
                        });
                        created.insert(id, MachineRec { worker, spec, desired: Desired::Started });
                    }
                    Op::Destroy { id } => {
                        info!(app = %app.name, machine = %id, why = %match app.state { Lifecycle::Active => "scale down", Lifecycle::Destroyed => "app destroyed" }, "destroy");
                        if let Some(m) = machines.iter_mut().find(|m| m.id == id) {
                            m.desired = Desired::Destroyed;
                        }
                        destroyed.push(id);
                    }
                }
            }
        }
        if !created.is_empty() || !destroyed.is_empty() {
            self.db
                .write(|r| {
                    r.machines.extend(created);
                    for id in &destroyed {
                        if let Some(m) = r.machines.get_mut(id) {
                            m.desired = Desired::Destroyed;
                        }
                    }
                    Ok::<_, String>(())
                })
                .map_err(|e| format!("writing machines: {e}"))??;
        }
        self.publish_leases().await?;
        self.publish_backhauls(&rows).await?;
        self.publish_placement().await?;
        self.sync_agents(&rows).await;
        Ok(())
    }

    /// Wake on every API write, else every 5 s. The machine agents report facts on their own; the tick
    /// catches up workers that were unreachable (or restarted) and the rest after a restart of this one.
    pub(crate) async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.reconcile().await {
                warn!(error = %e, "reconcile");
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }
}
