//! `proxy <role>`: one binary for every node role (the library, src/lib.rs, has them).
use proxy::{agent, backhaul, control, dash, edge, ingress};

/// `proxy <role>`: one binary for every node role.
#[derive(clap::Parser)]
#[command(name = "proxy", about = "flyyy's node roles")]
enum Role {
    /// HTTP(S) by Host, and tcp services on each app's own IP, to the workers' backhauls.
    Edge(edge::Args),
    /// A worker's backhaul: CONNECTs from the edges, to this worker's VMs.
    Worker(backhaul::Args),
    /// The machine agent: this worker's VMs, as the control plane tells it over the agent API.
    Agent(agent::Args),
    /// The control plane: the API, the reconciler and the WireGuard mesh.
    Control(control::Args),
    /// Make the ingress prefixes (ip_pools) local on this host (needs CAP_NET_ADMIN).
    Anyip(ingress::AnyipArgs),
    /// The public dashboard: the page over HTTPS, its API calls to the control plane over the mesh, with
    /// each caller's own token.
    Dashboard(dash::Args),
}

/// The edge, on every request's path, gets a multi-threaded runtime (one worker per CPU); the other roles do
/// little at once and keep one thread.
fn main() {
    let role = <Role as clap::Parser>::parse();
    proxy::init_logging();
    let mut rt = match role {
        Role::Edge(_) => tokio::runtime::Builder::new_multi_thread(),
        _ => tokio::runtime::Builder::new_current_thread(),
    };
    rt.enable_all().build().expect("tokio runtime").block_on(async {
        match role {
            Role::Edge(a) => edge::run(a).await,
            Role::Worker(a) => backhaul::run(a).await,
            Role::Agent(a) => agent::main(a).await,
            Role::Control(a) => control::main(a).await,
            Role::Anyip(a) => ingress::anyip(a).await,
            Role::Dashboard(a) => dash::public(a).await,
        }
    });
}
