//! `proxy agent`: the machine agent.
//! Takes this worker's desired state from the control plane, over the agent API (api.rs; kept in
//! desired.rs, so it runs on with the control plane down) and, on every change and every resync
//! tick, diffs it against what is actually here (local state files + `fc-<id>` units),
//! acts, and reports `machine_status` facts back through the local Corrosion agent. Images are
//! built in the background (`images.rs`); each machine boots its image read-only plus a fresh
//! scratch drive, stacked with overlayfs by the guest init. VMs have no network: one vsock device,
//! whose port the guest init forwards to the app on 127.0.0.1.
//! The agent runs as an ordinary user: each VM is a transient unit of that user's systemd
//! (`systemd-run --user`), which supervises it; the agent only decides, so a crashed agent leaves
//! VMs running. Nothing here needs root: Cloud Hypervisor needs only /dev/kvm, and VMs have no NIC.
//! It also announces the worker: its own `workers` row (backhaul and agent API addresses, capacity and
//! allocation, a heartbeat every 5 s).
pub(crate) mod api;
pub(crate) mod desired;
mod guest;
mod plan;
pub(crate) mod reconcile;
pub(crate) mod report;
pub(crate) mod spec;
mod sys;
#[cfg(test)]
mod tests;
mod usage;
mod vm;

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use backon::{ExponentialBackoff, ExponentialBuilder};
use corro_client::CorrosionApiClient;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::NodeArgs;
use crate::{
    agent::{
        desired::Store,
        guest::GuestConfig,
        plan::Report,
        report::{capacity, heartbeat, report_images, Announce},
        spec::Spec,
    },
    images::Images,
};

/// The local state file: everything needed to start, stop and clean up the machine.
#[derive(Serialize, Deserialize)]
pub(crate) struct Local {
    pub(crate) spec: Spec,
    pub(crate) port: u16,
    /// What the guest launcher runs, written onto every boot's fresh scratch drive.
    pub(crate) config: GuestConfig,
    /// The read-only image file this machine boots.
    pub(crate) image_file: String,
}

pub(crate) struct Failure {
    pub(crate) error: String,
    pub(crate) retry_at: Instant,
    pub(crate) delays: ExponentialBackoff,
}

/// The agent's directories: `state`, one JSON record and scratch drive per machine and the read-only
/// images (`images/`, images.rs); `run`, each VM's launch record and vsock socket (shared with the
/// backhaul).
pub(crate) struct Dirs {
    pub(crate) state: String,
    pub(crate) run: String,
}

pub(crate) struct Agent {
    pub(crate) dirs: Dirs,
    /// Its backhaul's /state (the drain reads it).
    pub(crate) backhaul_state: SocketAddr,
    pub(crate) worker: String,
    pub(crate) kernel: String,
    pub(crate) initrd: String,
    pub(crate) client: CorrosionApiClient,
    /// What the control plane told it.
    pub(crate) store: Arc<Store>,
    pub(crate) images: Images,
    pub(crate) backoff: ExponentialBuilder,
    pub(crate) failures: HashMap<String, Failure>,
    /// Started by us and not yet ready, with when: if the unit goes away first, or the port still
    /// doesn't accept after READY_TIMEOUT, that start failed.
    pub(crate) starting: HashMap<String, Instant>,
    /// Running and accepted a TCP connection on its port.
    pub(crate) ready: HashSet<String>,
    /// Some machine waits on an image: tick fast, so image retries happen when they're due.
    pub(crate) pulling: bool,
    /// Last facts written, so only changes are sent. Empty after a restart, so all are rewritten.
    pub(crate) reported: HashMap<String, (String, Report)>,
    /// The running machines' services, for the health checks (health.rs).
    pub(crate) targets: crate::health::Targets,
}

/// `proxy agent`'s flags.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Its Corrosion agent's API and token.
    #[command(flatten)]
    corrosion: crate::corro::ApiArgs,
    /// This worker's name (a node's name: `wg::node_name_ok`).
    #[arg(long, value_parser = crate::wg::node_name)]
    worker: String,
    /// The guests' kernel and initrd.
    #[arg(long)]
    kernel: String,
    #[arg(long)]
    initrd: String,
    /// This worker's backhaul, as announced to the edges (its mesh address, or loopback with --single-host).
    #[arg(long)]
    backhaul: SocketAddr,
    /// The agent API, where the control plane sends this worker's machines (this worker's mesh address,
    /// or loopback with --single-host).
    #[arg(long)]
    api: SocketAddr,
    /// The backhaul's /state: where a drain sees the edges' requests to a machine stop.
    #[arg(long, env = "FLYYY_BACKHAUL_STATE", default_value = "127.0.0.1:9080")]
    backhaul_state: SocketAddr,
    /// One JSON record and scratch drive per machine, and the images.
    #[arg(long, env = "FLYYY_STATE_DIR", default_value = "/var/lib/flyyy")]
    state_dir: String,
    /// Each VM's launch record (its Cloud Hypervisor arguments) and vsock socket (shared with the backhaul).
    #[arg(long, env = "FLYYY_RUN_DIR", default_value = "/run/flyyy")]
    run_dir: String,
    #[command(flatten)]
    node: NodeArgs,
}

pub async fn main(args: Args) {
    let single_host = args.node.single_host("agent");
    let (api, worker, kernel, initrd, backhaul) = (args.corrosion.open(), args.worker, args.kernel, args.initrd, args.backhaul);
    // It announces this to the edges, which accept a loopback one only in single-host mode: say so here.
    assert!(crate::wg::backhaul_ok(backhaul, single_host), "backhaul {backhaul}: want this worker's mesh address (10.250.x.y), or loopback with --single-host");
    assert!(crate::wg::backhaul_ok(args.api, single_host), "--api {}: want this worker's mesh address (10.250.x.y), or loopback with --single-host", args.api);
    let dirs = Dirs { state: args.state_dir, run: args.run_dir };
    std::fs::create_dir_all(&dirs.state).expect("state dir");
    let changed = Arc::new(Notify::new());
    let images = Images::open(&format!("{}/images", dirs.state), changed.clone()).expect("images dir");
    let store = Arc::new(Store::open(&format!("{}/desired", dirs.state), changed.clone()).unwrap_or_else(|e| panic!("desired state: {e}")));
    let machines: Arc<crate::metrics::Machines> = Arc::default();
    tokio::spawn(usage::run(store.clone(), machines.clone()));
    tokio::spawn(api::run(args.api, args.node.tls_dir.clone(), single_host, store.clone(), machines));
    let capacity = capacity().unwrap_or_else(|e| panic!("this worker's capacity: {e}"));
    let announce = Announce { worker: worker.clone(), backhaul, agent_api: args.api, capacity };
    tokio::spawn(heartbeat(api.client(), announce, store.clone()));
    tokio::spawn(report_images(api.client(), worker.clone(), dirs.state.clone()));
    let targets: crate::health::Targets = Default::default();
    tokio::spawn(crate::health::run(api.client(), worker.clone(), dirs.run.clone(), targets.clone()));
    tracing::info!(%worker, %backhaul, corrosion = %api.addr, "agent started");
    let agent = Agent {
        dirs,
        backhaul_state: args.backhaul_state,
        worker: worker.clone(),
        kernel,
        initrd,
        client: api.client(),
        store,
        images,
        backoff: ExponentialBuilder::new().with_min_delay(Duration::from_secs(1)).with_max_delay(Duration::from_secs(60)).without_max_times(),
        failures: HashMap::new(),
        starting: HashMap::new(),
        ready: HashSet::new(),
        pulling: false,
        reported: HashMap::new(),
        targets,
    };
    agent.run(changed).await;
}
