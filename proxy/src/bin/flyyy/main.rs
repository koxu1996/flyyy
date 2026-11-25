//! `flyyy`: the CLI. A thin client of the control plane API; no Corrosion access, no logic of its own.
//! `flyyy --help` lists the commands, `flyyy <command> --help` each one's flags.
//! `join` runs on a node, with CAP_NET_ADMIN only, and keeps running: it keeps the node's mesh peers in sync
//! and its mesh certificate renewed (the key made here, a CSR to the control plane's CA).
//! deploy/scale/destroy wait until converged (--detach: don't; --timeout <s>, default 120).
//! Exit codes: 0 ok/converged, 1 error (bad arguments too), 2 timed out.
mod apps;
mod cluster;
mod mesh;
mod out;

use std::{net::SocketAddr, process::ExitCode};

use clap::{Parser, Subcommand};
use proxy::{client::Client, dash};

#[derive(Parser, Debug)]
#[command(name = "flyyy", about = "The CLI of the control plane's API.")]
struct Cli {
    /// The control plane's API (default: the control plane on the mesh).
    #[arg(long, env = "FLYYY_API", default_value = "http://10.250.0.1:4000", value_parser = api_addr)]
    api: String,
    /// The operator's token, or a user's (`users create` prints it).
    /// Default: the operator token's file on the control host (#control's, and the module's).
    #[arg(long, env = "FLYYY_TOKEN_FILE", default_value = "/var/lib/flyyy-operator/token")]
    api_token_file: String,
    #[command(subcommand)]
    command: Command,
}

/// `http://host:port` → `host:port`.
fn api_addr(api: &str) -> Result<String, String> {
    Ok(api.strip_prefix("http://").ok_or_else(|| format!("{api:?}: want http://host:port"))?.trim_end_matches('/').to_owned())
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create or update an app.
    Deploy(apps::Deploy),
    /// Change an app's machine count.
    Scale {
        app: String,
        #[arg(long)]
        count: u64,
        #[command(flatten)]
        wait: Wait,
    },
    /// Destroy an app and its machines.
    Destroy {
        app: String,
        #[command(flatten)]
        wait: Wait,
    },
    /// The apps.
    Ls(Json),
    /// An app and its machines.
    Status {
        app: String,
        #[command(flatten)]
        json: Json,
    },
    /// An app's metrics over a window: requests by status, latency, bytes, and its machines' CPU and memory.
    Metrics {
        app: String,
        /// 5m or 1h.
        #[arg(long, default_value = "5m", value_parser = ["5m", "1h"])]
        window: String,
        #[command(flatten)]
        json: Json,
    },
    /// An app's addresses: its own (its tcp services) and the shared ones (HTTP, by Host).
    Ips {
        app: String,
        #[command(flatten)]
        json: Json,
    },
    /// The workers.
    Workers(Json),
    /// The orgs (operator only, as are `users` and the mesh commands).
    Orgs {
        #[command(subcommand)]
        verb: Option<cluster::OrgVerb>,
        #[command(flatten)]
        json: Json,
    },
    /// The users.
    Users {
        #[command(subcommand)]
        verb: Option<cluster::UserVerb>,
        #[command(flatten)]
        json: Json,
    },
    /// The web dashboard on loopback; it forwards the page's API calls with this CLI's token, which never
    /// reaches the browser.
    Dashboard {
        #[arg(long, default_value = "127.0.0.1:4040")]
        listen: SocketAddr,
        /// Where the app links go, {app} the app's name.
        #[arg(long, default_value = "http://{app}.flyyy.localhost:8000/")]
        apps_url: String,
        /// The flake the join dialog's command runs (`nix run '<this>#join'`).
        #[arg(long, default_value = dash::FLAKE_URL)]
        flake_url: String,
    },
    /// Create the WireGuard mesh, print a one-use join token.
    Init,
    /// Print another join token.
    Token,
    /// Let a joined node in: it gets an address and switches to its own key.
    Approve { node: String },
    /// The mesh's nodes.
    Mesh(Json),
    /// On a node, with CAP_NET_ADMIN: join the mesh, then keep its peers and certificate up to date.
    Join(mesh::Join),
    /// On the control plane's own host, for an edge there (it can't join: one flyyy0 per host): get its
    /// certificate for the control plane's address with a join token, then keep it renewed.
    LocalCert(mesh::LocalCert),
}

#[derive(clap::Args, Debug)]
pub(crate) struct Wait {
    /// Don't wait until converged.
    #[arg(long)]
    pub(crate) detach: bool,
    /// How long to wait, in seconds; then exit 2.
    #[arg(long, default_value_t = 120)]
    pub(crate) timeout: u64,
}

#[derive(clap::Args, Debug)]
pub(crate) struct Json {
    /// The API's JSON instead of a table.
    #[arg(long)]
    pub(crate) json: bool,
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    let command = match cli.command {
        Command::Join(args) => return mesh::join(&args).await,
        Command::LocalCert(args) => return mesh::local_cert(&args).await,
        c => c,
    };
    let token = std::fs::read_to_string(&cli.api_token_file).map_err(|e| format!("token file {}: {e}", cli.api_token_file))?.trim().to_owned();
    let client = Client { addr: cli.api, token: Some(token) };
    match command {
        Command::Deploy(d) => apps::deploy(&client, d).await,
        Command::Scale { app, count, wait } => apps::scale(&client, &app, count, &wait).await,
        Command::Destroy { app, wait } => apps::destroy(&client, &app, &wait).await,
        Command::Ls(json) => apps::ls(&client, &json).await,
        Command::Status { app, json } => apps::status(&client, &app, &json).await,
        Command::Ips { app, json } => apps::ips(&client, &app, &json).await,
        Command::Metrics { app, window, json } => apps::metrics(&client, &app, &window, &json).await,
        Command::Workers(json) => cluster::workers(&client, &json).await,
        Command::Orgs { verb, json } => cluster::orgs(&client, verb, &json).await,
        Command::Users { verb, json } => cluster::users(&client, verb, &json).await,
        Command::Dashboard { listen, apps_url, flake_url } => dash::run(client, listen, &apps_url, &flake_url).await.map(|()| ExitCode::SUCCESS),
        Command::Init => mesh::token(&client, true).await,
        Command::Token => mesh::token(&client, false).await,
        Command::Approve { node } => mesh::approve(&client, &node).await,
        Command::Mesh(json) => mesh::list(&client, &json).await,
        Command::Join(_) | Command::LocalCert(_) => unreachable!("handled above"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // clap's own exit code for bad arguments is 2, which here means timed out.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
        }
    };
    // Logs (join and the dashboard's) on stderr; what a command prints for the user goes to stdout.
    proxy::init_logging();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flyyy: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use proxy::wg;

    #[test]
    fn cli() {
        Cli::command().debug_assert();
        // The default --api is the control plane on the mesh (a literal, as clap wants it).
        let cmd = Cli::command();
        let api = cmd.get_arguments().find(|a| a.get_id() == "api").unwrap();
        assert_eq!(api_addr(api.get_default_values()[0].to_str().unwrap()).unwrap(), format!("{}:{}", wg::CONTROL_IP, wg::CONTROL_API_PORT));
    }
}
