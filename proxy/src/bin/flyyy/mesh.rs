//! The WireGuard mesh: init, token, approve and the node list (the control plane's side, the operator's),
//! and `flyyy join` (a node's side).

use std::{
    net::Ipv4Addr,
    process::ExitCode,
    time::{Duration, Instant},
};
use tracing::{info, warn};

use hyper::{Method, StatusCode};
use proxy::{
    client::Client,
    pki,
    states::{self, NodeRole},
    wg,
};

use crate::{
    out::{s, show, table},
    Json,
};

#[derive(clap::Args, Debug)]
#[command(group = clap::ArgGroup::new("join_token").args(["token", "token_file"]))]
pub(crate) struct Join {
    /// The control plane's host[:port], for a first join with a token that doesn't carry it (later ones
    /// resume from --state-dir).
    control: Option<String>,
    #[arg(long)]
    token: Option<String>,
    /// Removed once approved.
    #[arg(long)]
    token_file: Option<String>,
    /// For a first join.
    #[arg(long, value_enum)]
    role: Option<NodeRole>,
    /// A node's name: 1-32 of a-z, 0-9 and -, starting with a letter. Default: the hostname, lowercased,
    /// anything else a -, cut to 32, a leading digit prefixed with n.
    #[arg(long, value_parser = wg::node_name)]
    name: Option<String>,
    /// For a first join: the WireGuard endpoint the other nodes should use for this one (host:port),
    /// instead of the address the control plane sees: behind NAT, the port forwarded to this node's 51820.
    #[arg(long, value_parser = endpoint)]
    endpoint: Option<String>,
    #[arg(long, env = "FLYYY_MESH_DIR", default_value = "/var/lib/flyyy-mesh")]
    state_dir: String,
    /// Where its mesh certificate goes.
    #[arg(long, env = "FLYYY_TLS_DIR", default_value = "/var/lib/flyyy-tls")]
    tls_dir: String,
}

#[derive(clap::Args, Debug)]
pub(crate) struct LocalCert {
    /// The join token it's set up with: spent at the first certificate, then what renews it, so it stays.
    #[arg(long)]
    token_file: String,
    #[arg(long)]
    name: String,
    /// Only edge: nothing else on the control plane's host needs a certificate.
    #[arg(long, value_enum, default_value = "edge")]
    role: NodeRole,
    /// Where its certificate goes (not the control plane's own --tls-dir).
    #[arg(long)]
    tls_dir: String,
    /// The control plane's API on this host (loopback).
    #[arg(long, default_value = "127.0.0.1:4000")]
    control: String,
}

/// `flyyy local-cert`: a certificate now, then again past half its lifetime. A refusal (a spent or wrong
/// token, a bad name) ends it, as join's does.
pub(crate) async fn local_cert(args: &LocalCert) -> Result<ExitCode, String> {
    let f = &args.token_file;
    let token = std::fs::read_to_string(f).map_err(|e| format!("token file {f}: {e}"))?.trim().to_owned();
    let client = Client { addr: args.control.clone(), token: None };
    let (dir, role) = (&args.tls_dir, states::text(args.role));
    let cert_file = format!("{dir}/{}", pki::CERT);
    loop {
        if pki::needs_renewal(&cert_file) {
            let (key, csr) = pki::new_key_and_csr(&args.name)?;
            let body = serde_json::json!({ "token": token, "name": args.name, "role": role, "csr": csr });
            match client.call(Method::POST, "/v1/mesh/local", Some(body)).await {
                Ok((code, v)) if code.is_success() => {
                    let (cert, ca) = (v["cert"].as_str().unwrap_or_default(), v["ca"].as_str().unwrap_or_default());
                    pki::write_tls(dir, ca, &key, cert)?;
                    info!(%dir, node = %args.name, "local-cert: new mesh certificate");
                }
                // 409 is no mesh yet (the control plane just started), or a name taken: said, and retried.
                Ok((code, v)) if code.is_client_error() && code != StatusCode::CONFLICT => {
                    return Err(format!("certificate refused: {}: {}", code.as_u16(), v["error"].as_str().unwrap_or_default()));
                }
                Ok((code, v)) => warn!(status = code.as_u16(), error = %v["error"].as_str().unwrap_or_default(), "local-cert"),
                // The control plane isn't up yet, or has no mesh yet.
                Err(e) => warn!(error = %e, "local-cert"),
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn endpoint(s: &str) -> Result<String, String> {
    match wg::endpoint_ok(s) {
        true => Ok(s.to_owned()),
        false => Err("want <IPv4>:<port>, [<IPv6>]:<port> or <name>:<port>".into()),
    }
}

/// `flyyy init` (the mesh, then a token) or `flyyy token` (another one).
pub(crate) async fn token(client: &Client, init: bool) -> Result<ExitCode, String> {
    let t = client.ok(Method::POST, if init { "/v1/mesh" } else { "/v1/mesh/tokens" }, None).await?;
    if init {
        eprintln!("flyyy: mesh created; control plane at {} on it", wg::CONTROL_IP);
    }
    eprintln!("flyyy: one-use join token for {} (secret until used; expires in 24 h):", s(&t["ip"]));
    println!("{}", s(&t["token"]));
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn approve(client: &Client, node: &str) -> Result<ExitCode, String> {
    let v = client.ok(Method::POST, &format!("/v1/mesh/nodes/{node}/approve"), None).await?;
    println!("approve {node}: {}", s(&v["ip"]));
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn list(client: &Client, json: &Json) -> Result<ExitCode, String> {
    let m = client.ok(Method::GET, "/v1/mesh", None).await?;
    show(json, &m, &|m| {
        println!("control plane {} key {} ({} unused join tokens)", s(&m["control"]["ip"]), s(&m["control"]["key"]), m["unused_tokens"]);
        let mut rows = vec![["NAME", "ROLE", "IP", "STATE", "ENDPOINT", "LISTEN", "HANDSHAKE"].map(String::from).to_vec()];
        for n in m["nodes"].as_array().into_iter().flatten() {
            let hs = n["handshake_age_s"].as_i64().map_or("never".into(), |a| format!("{a}s ago"));
            // The endpoint the other nodes get: the advertised one if any (marked), else the learned one.
            let ep = match n["advertised_endpoint"].as_str() {
                Some(a) => format!("{a} (advertised)"),
                None => s(&n["endpoint"]),
            };
            // An edge's: where it serves apps, as it announced it (the dashboard says it the same way).
            let listen: Vec<String> = n["listen"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).map(listen_text).collect();
            let listen = if listen.is_empty() { "-".into() } else { listen.join(", ") };
            rows.push(vec![s(&n["name"]), s(&n["role"]), s(&n["ip"]), s(&n["state"]), ep, listen, hs]);
        }
        table(rows);
    });
    Ok(ExitCode::SUCCESS)
}

/// An edge's listen address as the operator reads it: `[::]` is IPv4 and IPv6 on one socket (Linux's default,
/// net.ipv6.bindv6only = 0).
fn listen_text(l: &str) -> String {
    match l.starts_with("[::]:") {
        true => format!("{l} · IPv4 + IPv6"),
        false => l.to_owned(),
    }
}

/// How long a join goes without reaching the control plane before it explains why it might not.
const STALL_HINT_AFTER: Duration = Duration::from_secs(20);

/// Why the bootstrap tunnel may not answer: whether WireGuard ever got a handshake from the control plane,
/// and the route this node would send its packets to the endpoint by. No route, or no source address on
/// it, means this node's own network isn't up (a container whose address unit failed once looked just like
/// a join that hangs); a route and still no handshake means UDP to the endpoint doesn't arrive or come back.
async fn stall_hint(control_pub: &str, endpoint: &str) -> String {
    let handshake = match wg::live().await {
        Ok(peers) => match peers.iter().find(|p| p.key == control_pub) {
            Some(p) if p.handshake > 0 => "a WireGuard handshake did happen (so the tunnel is up; the API isn't answering)".to_owned(),
            Some(_) => "no WireGuard handshake with the control plane yet".to_owned(),
            None => "the control plane isn't a peer of this interface".to_owned(),
        },
        Err(e) => format!("can't read WireGuard's state: {e}"),
    };
    let route = match tokio::net::lookup_host(endpoint).await.map(|mut a| a.next()) {
        Ok(Some(addr)) => {
            let out = tokio::process::Command::new("ip").args(["-o", "route", "get", &addr.ip().to_string()]).output().await;
            match out {
                Ok(o) if o.status.success() => {
                    let r = String::from_utf8_lossy(&o.stdout).split_whitespace().collect::<Vec<_>>().join(" ");
                    if r.contains(" src ") {
                        format!("packets to {addr} leave by: {r}")
                    } else {
                        format!("packets to {addr} have no source address: {r}")
                    }
                }
                Ok(o) => format!("no route to {addr}: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => format!("ip route get {}: {e}", addr.ip()),
            }
        }
        Ok(None) | Err(_) => format!("can't resolve the control plane's endpoint {endpoint}"),
    };
    format!(
        "still no answer from the control plane: {handshake}; {route}. Check this node's network and that UDP {endpoint} reaches the control plane and back."
    )
}

/// A node's mesh state (`<state-dir>/state.json`), next to its keys: `node.key` (its own, made here) and,
/// until it switches, `boot.key` (the token's).
#[derive(serde::Serialize, serde::Deserialize)]
struct NodeState {
    name: String,
    /// worker or edge: in its certificate.
    #[serde(default)]
    role: String,
    /// The control plane's underlay endpoint, host:port.
    control: String,
    control_pub: String,
    boot_ip: Ipv4Addr,
    /// Set once approved.
    ip: Option<Ipv4Addr>,
}

/// `flyyy join`: come up with the token's key, send this node's own key, wait for `flyyy approve`, switch
/// to the own key, then keep the peers in sync with the control plane. Resumable at every step.
pub(crate) async fn join(args: &Join) -> Result<ExitCode, String> {
    let dir = &args.state_dir;
    std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    let (state_file, boot_key, node_key) = (format!("{dir}/state.json"), format!("{dir}/boot.key"), format!("{dir}/node.key"));
    let save = |st: &NodeState| -> Result<(), String> {
        let tmp = format!("{state_file}.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(st).expect("serialisable")).map_err(|e| format!("{tmp}: {e}"))?;
        std::fs::rename(&tmp, &state_file).map_err(|e| format!("{state_file}: {e}"))
    };
    let mut st: NodeState = match std::fs::read(&state_file) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{state_file}: {e}"))?,
        Err(_) => {
            // clap refuses both.
            let raw = match (&args.token, &args.token_file) {
                (Some(t), _) => t.clone(),
                (None, Some(f)) => std::fs::read_to_string(f).map_err(|e| format!("token file {f}: {e}"))?,
                (None, None) => return Err("want one of --token or --token-file (a first join needs a token)".into()),
            };
            let token = wg::Token::decode(&raw)?;
            // The one given here wins: the token's is how the control plane thinks nodes reach it.
            let host = args
                .control
                .as_ref()
                .or(token.control.as_ref())
                .ok_or("want the control plane's host[:port]: this token doesn't carry it (the control plane has no --endpoint)")?;
            let name = match &args.name {
                Some(n) => n.clone(),
                None => wg::default_node_name(&std::fs::read_to_string("/proc/sys/kernel/hostname").map_err(|e| format!("hostname: {e}"))?),
            };
            if !wg::node_name_ok(&name) {
                return Err(format!("this host's name makes {name:?}, not a node's name: pass --name"));
            }
            wg::write_private(&boot_key, &token.key)?;
            wg::write_private(&node_key, &wg::genkey().await?)?;
            let role = states::text(args.role.ok_or("want --role worker, edge or dashboard")?);
            let control = if host.contains(':') { host.clone() } else { format!("{host}:{}", wg::PORT) };
            let st = NodeState { name, role, control, control_pub: token.control_pub, boot_ip: token.ip, ip: None };
            save(&st)?;
            st
        }
    };
    let control = wg::Peer { key: st.control_pub.clone(), ip: wg::CONTROL_IP, endpoint: st.control.clone() };
    let client = Client { addr: format!("{}:{}", wg::CONTROL_IP, wg::CONTROL_API_PORT), token: None };
    if st.ip.is_none() {
        wg::up(&boot_key, st.boot_ip).await?;
        wg::sync(std::slice::from_ref(&control)).await?;
        info!(ip = %st.boot_ip, control = %st.control, "join: bootstrap tunnel up");
        let own = wg::pubkey(std::fs::read_to_string(&node_key).map_err(|e| format!("{node_key}: {e}"))?.trim()).await?;
        let mut said = false;
        let (t0, mut next_hint) = (Instant::now(), STALL_HINT_AFTER);
        let ip = loop {
            match client.call(Method::GET, "/v1/mesh/join", None).await {
                Ok((code, v)) if code.is_success() && v["state"] == "approved" => {
                    break v["ip"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| format!("approved without an address: {v}"))?;
                }
                Ok((code, _)) if code.is_success() => {
                    if !said {
                        info!(node = %st.name, "join: waiting for `flyyy approve` on the control plane");
                        said = true;
                    }
                }
                // Not registered yet: send this node's own public key; that spends the token.
                Ok((StatusCode::NOT_FOUND, _)) => {
                    let (code, v) = client
                        .call(
                            Method::POST,
                            "/v1/mesh/join",
                            Some(serde_json::json!({ "name": st.name, "key": own, "role": st.role, "endpoint": args.endpoint, "hostname": wg::hostname() })),
                        )
                        .await?;
                    if !code.is_success() {
                        return Err(format!("join refused: {}: {}", code.as_u16(), v["error"].as_str().unwrap_or_default()));
                    }
                    info!(node = %st.name, key = %own, "join: sent this node's own key");
                }
                Ok((code, v)) => warn!(status = code.as_u16(), error = %v["error"].as_str().unwrap_or_default(), "join"),
                Err(e) => {
                    warn!(error = %e, "join");
                    // WireGuard says nothing when packets can't leave: say what this node can see.
                    if t0.elapsed() >= next_hint {
                        warn!(hint = %stall_hint(&st.control_pub, &st.control).await, "join: no answer yet");
                        next_hint += Duration::from_secs(60);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        st.ip = Some(ip);
        save(&st)?;
        // The token is spent: nothing needs it (or its key) any more.
        let _ = std::fs::remove_file(&boot_key);
        if let Some(f) = &args.token_file {
            let _ = std::fs::remove_file(f);
        }
        info!(%ip, "join: approved; switching to this node's own key");
    }
    let ip = st.ip.expect("set above");
    wg::up(&node_key, ip).await?;
    wg::sync(std::slice::from_ref(&control)).await?;
    info!(node = %st.name, %ip, "join: on the mesh; keeping its peers in sync");
    let tls_dir = &args.tls_dir;
    let cert_file = format!("{tls_dir}/{}", pki::CERT);
    // Its mesh certificate: a new key here, a CSR to the CA over the mesh; again past half its lifetime.
    let renew = || async {
        let (key, csr) = pki::new_key_and_csr(&st.name)?;
        let (code, v) = client.call(Method::POST, "/v1/mesh/cert", Some(serde_json::json!({ "csr": csr }))).await?;
        if !code.is_success() {
            return Err(format!("{}: {}", code.as_u16(), v["error"].as_str().unwrap_or_default()));
        }
        let (cert, ca) = (v["cert"].as_str().unwrap_or_default(), v["ca"].as_str().unwrap_or_default());
        pki::write_tls(tls_dir, ca, &key, cert)?;
        info!(%tls_dir, "join: new mesh certificate");
        Ok::<(), String>(())
    };
    // The control plane's view, every 5 s. It knows the others' endpoints; ours to it is from state.
    let mut last: Vec<wg::Peer> = Vec::new();
    // A dashboard gets no certificate (it reaches the control plane's API only).
    let wants_cert = st.role != states::text(NodeRole::Dashboard);
    loop {
        if wants_cert && pki::needs_renewal(&cert_file) {
            if let Err(e) = renew().await {
                warn!(error = %e, "join: certificate");
            }
        }
        match client.call(Method::GET, "/v1/mesh/peers", None).await {
            Ok((code, v)) if code.is_success() => match serde_json::from_value::<Vec<wg::Peer>>(v["peers"].clone()) {
                Ok(peers) => {
                    let mut want: Vec<wg::Peer> = peers.into_iter().filter(|p| p.key != control.key && p.ip != ip).collect();
                    want.push(control.clone());
                    want.sort();
                    match wg::sync(&want).await {
                        Ok(()) if want != last => {
                            info!(peers = %want.iter().map(|p| format!("{} {}", p.ip, p.endpoint)).collect::<Vec<_>>().join(", "), "join: peers");
                            last = want;
                        }
                        Ok(()) => {}
                        Err(e) => warn!(error = %e, "join: peers"),
                    }
                }
                Err(e) => warn!(error = %e, "join: bad peers"),
            },
            Ok((code, v)) => warn!(status = code.as_u16(), error = %v["error"].as_str().unwrap_or_default(), "join: peers"),
            Err(e) => warn!(error = %e, "join: peers"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
