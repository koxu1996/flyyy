//! flyyy's library: everything the node roles (`proxy`, src/main.rs) and the CLI (`flyyy`, src/bin/flyyy/)
//! are made of. Public: the roles' entry points, the CLI's API client and dashboard, and the mesh and wire
//! types both share; the rest is the roles' own.
//!
//! The roles (`proxy <role> --help` has each one's flags). Corrosion carries facts only: the edges and
//! backhauls follow subscriptions of it, the agent and the control plane report and publish into it.
//! - `proxy edge`: `<app>.<apps-domain>` → by load over all instances of the app (balance.rs), across
//!   workers, each request a CONNECT stream through its worker's backhaul (pool.rs). Only instances as the
//!   control plane placed them, at the backhaul it bound to the worker's mesh identity (edge_routes.rs);
//!   tcp services on each app's own IP. With --https, TLS ends here (an already-issued certificate,
//!   reloaded on SIGHUP, tls.rs) and plain HTTP on --listen only redirects to it. Both listen with
//!   SO_REUSEPORT, so several edges share them.
//! - `proxy worker`: the backhaul. Accepts `CONNECT <instance-id>` only for instances placed on this
//!   worker, connects to the VM over vsock (Cloud Hypervisor's Unix socket + `CONNECT <port>`) and pipes bytes.
//!   VMs have no network at all.
//! - `proxy agent`: the machine agent (agent/), told what to run by the control plane over its API.
//! - `proxy control`: the control plane (control/).
//! - `proxy anyip`: makes the ingress prefixes local on an edge host (ingress.rs).
pub mod agent;
pub mod backhaul;
pub mod client;
pub mod control;
pub mod dash;
pub mod edge;
pub mod ingress;
pub mod pki;
pub mod services;
pub mod states;
pub mod wg;

mod api_error;
mod balance;
mod corro;
mod edge_routes;
mod health;
mod http;
mod images;
mod mesh;
mod metrics;
mod pool;
mod random;
mod subscription;
mod table;
mod tls;
mod util;
mod vsock;

use http_body_util::combinators::BoxBody;
use hyper::body::Bytes;

pub(crate) use crate::http::serve;

/// Logs: `<LEVEL> <message> <fields>` lines on stderr, for journald (which adds the time). FLYYY_LOG sets
/// the level: error, warn, info (the default), debug or trace.
pub fn init_logging() {
    let level = std::env::var("FLYYY_LOG").ok().and_then(|l| l.parse().ok()).unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    tracing_subscriber::fmt().with_max_level(level).without_time().with_target(false).with_ansi(false).with_writer(std::io::stderr).init();
}

/// The body every handler answers with.
pub type Body = BoxBody<Bytes, hyper::Error>;

/// What every node role takes about the mesh.
#[derive(clap::Args, Debug)]
pub struct NodeArgs {
    /// Loopback backhauls accepted: one machine, no mesh (dev only; refused on a node of a mesh).
    #[arg(long)]
    pub single_host: bool,
    /// This node's mesh certificate directory (pki.rs); unset: no mesh, so no mTLS.
    #[arg(long, env = "FLYYY_TLS_DIR", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    pub tls_dir: Option<String>,
}

impl NodeArgs {
    /// Whether single-host mode is on. Refused on a node of a mesh (one with a mesh certificate directory):
    /// real deployments are mesh-only.
    pub fn single_host(&self, role: &str) -> bool {
        if self.single_host {
            if let Some(dir) = &self.tls_dir {
                panic!("proxy {role}: refusing --single-host: this node is on a mesh (certificates in {dir})");
            }
            tracing::info!(role = %role, "single-host (dev) mode: loopback backhauls accepted, no mesh");
        }
        self.single_host
    }
}
