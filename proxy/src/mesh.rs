//! The control plane's side of the WireGuard mesh: its own interface (10.250.0.1), join tokens, nodes and
//! the peers every node gets. State is a JSON file in the control plane's state directory, next to its
//! private key; only the control plane serves it, so it isn't in Corrosion (which runs over the mesh).
//!
//! Join: `flyyy token` makes a bootstrap key pair, keeps the public half as a peer limited to one
//! 10.250.255.x address and prints the private half in the token. `flyyy join` comes up with it and
//! posts its own public key from that address: WireGuard only lets that key use that source address, so
//! the address identifies the token, and the token is spent. `flyyy approve <name>` gives the node an
//! address and adds its own key; its first peers request from there drops the bootstrap key.
//! The control plane is also the mesh's CA (pki.rs): a node asks for its certificate with a CSR from its
//! own mesh address, and gets one for that address, the role it joined as and its name.
//! An edge on the control plane's own host can't join (one flyyy0 per host): it gets its certificate for the
//! control plane's address over loopback instead, with a join token, which it keeps to renew it
//! (`local_cert`, `flyyy local-cert`).
// shortcut: one mesh, no key rotation, no removal of nodes, no revocation of pending joins.
use std::{net::Ipv4Addr, sync::Mutex, time::Duration};
use tracing::{info, warn};

use hyper::StatusCode;
use serde::{Deserialize, Serialize};

use crate::{
    api_error::ApiError,
    pki::{self, Ca},
    states::NodeRole,
    util::now_ms,
    wg::{self, Peer, Token, CONTROL_IP},
};

/// An unused token stops working after this long.
const TOKEN_TTL_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRow {
    /// The bootstrap key's public half.
    key: String,
    ip: Ipv4Addr,
    created_ms: i64,
    used: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NodeState {
    /// Joined with a token, waiting for `flyyy approve`.
    Pending,
    /// Has its address and its own key is a peer; still reachable by its bootstrap key until it switches.
    Approved,
    /// Switched to its own key; the bootstrap key is gone.
    Joined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    name: String,
    /// Its own public key, made on the node.
    key: String,
    boot_key: String,
    boot_ip: Ipv4Addr,
    ip: Option<Ipv4Addr>,
    state: NodeState,
    /// What it joined as, shown to whoever approves it; in its certificate. None in a state file from
    /// before roles (shown and certified as "", as then).
    #[serde(default)]
    role: Option<NodeRole>,
    /// Where the control plane last saw it (WireGuard's roaming endpoint), for the other nodes.
    #[serde(default)]
    endpoint: String,
    /// The endpoint it advertised at its join (`flyyy join --endpoint`): given to the other nodes instead
    /// of the learned one, which behind a NAT is the NAT's outside address and port as the control plane
    /// sees them, often unreachable from other nodes.
    #[serde(default)]
    advertised: Option<String>,
    /// When it joined (asked to be approved); 0 in a state file from before this.
    #[serde(default)]
    joined_ms: i64,
    /// Its host's name, as it reported it at its join (`wg::hostname_ok`); None in a state file from
    /// before this, or from a node that sent none.
    #[serde(default)]
    hostname: Option<String>,
}

impl Node {
    /// Where the other nodes are told to reach it.
    fn peer_endpoint(&self) -> String {
        self.advertised.clone().unwrap_or_else(|| self.endpoint.clone())
    }
}

/// A node on the control plane's own host (`flyyy local-cert`): no WireGuard of its own, so it has the
/// control plane's address. What it was set up with is its credential: the join token, whose public half
/// is kept here.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Local {
    name: String,
    role: NodeRole,
    key: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    tokens: Vec<TokenRow>,
    nodes: Vec<Node>,
    #[serde(default)]
    local: Vec<Local>,
}

impl State {
    fn name_taken(&self, name: &str) -> bool {
        self.nodes.iter().any(|n| n.name == name) || self.local.iter().any(|l| l.name == name)
    }
}

/// A mesh route's answer: its JSON, or the status and message to refuse with.
pub(crate) type Answer = Result<serde_json::Value, ApiError>;

pub(crate) struct Mesh {
    dir: String,
    /// None until `flyyy init`.
    state: Mutex<Option<(State, String)>>,
    /// The CA, with the mesh.
    ca: Mutex<Option<std::sync::Arc<Ca>>>,
    /// Where the control plane's own certificate goes (its --tls-dir, read by its Corrosion agent).
    tls_dir: Option<String>,
    /// How long a certificate is valid; nodes renew after half of it.
    cert_lifetime: Duration,
    /// Where nodes' WireGuard reaches this one (host:port), put in every join token; None: the joining
    /// node is told (`flyyy join <host>`).
    endpoint: Option<String>,
}

fn err(status: StatusCode, msg: impl std::fmt::Display) -> ApiError {
    ApiError::new(status, msg)
}

impl Mesh {
    fn key_file(&self) -> String {
        format!("{}/mesh.key", self.dir)
    }

    fn state_file(&self) -> String {
        format!("{}/mesh.json", self.dir)
    }

    /// Load an existing mesh and bring the interface back up (it doesn't survive a reboot).
    pub(crate) async fn open(dir: &str, tls_dir: Option<String>, cert_lifetime: Duration, endpoint: Option<String>) -> Result<Self, String> {
        let mesh = Mesh { dir: dir.into(), state: Mutex::new(None), ca: Mutex::new(None), tls_dir, cert_lifetime, endpoint };
        let Ok(private) = std::fs::read_to_string(mesh.key_file()) else { return Ok(mesh) };
        *mesh.ca.lock().unwrap() = Some(std::sync::Arc::new(Ca::open_or_create(dir)?));
        let state: State = serde_json::from_slice(&std::fs::read(mesh.state_file()).map_err(|e| format!("{}: {e}", mesh.state_file()))?)
            .map_err(|e| format!("{}: {e}", mesh.state_file()))?;
        let public = wg::pubkey(private.trim()).await?;
        wg::up(&mesh.key_file(), CONTROL_IP).await?;
        *mesh.state.lock().unwrap() = Some((state, public));
        mesh.reconcile().await?;
        Ok(mesh)
    }

    pub(crate) fn exists(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    /// The role of the approved node at a mesh address, if any: what binds an edge's announced address.
    pub(crate) fn role_at(&self, ip: Ipv4Addr) -> Option<NodeRole> {
        let guard = self.state.lock().unwrap();
        let (state, _) = guard.as_ref()?;
        let local = || state.local.first().filter(|_| ip == CONTROL_IP).map(|l| l.role);
        state.nodes.iter().find(|n| n.ip == Some(ip)).and_then(|n| n.role).or_else(local)
    }

    /// A node's mesh address, if a node of that name and role has one (approved or joined): what binds a
    /// worker's announced addresses to its mesh identity.
    pub(crate) fn node_ip(&self, name: &str, role: NodeRole) -> Option<Ipv4Addr> {
        let guard = self.state.lock().unwrap();
        let (state, _) = guard.as_ref()?;
        state.nodes.iter().find(|n| n.name == name && n.role == Some(role)).and_then(|n| n.ip)
    }

    fn save(&self, state: &State) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
        crate::util::write_atomic(&self.state_file(), &json)
    }

    /// `flyyy init`: the mesh, then the first token.
    pub(crate) async fn init(&self) -> Answer {
        if self.exists() {
            return Err(err(StatusCode::CONFLICT, "the mesh exists already; `flyyy token` makes another join token"));
        }
        self.create().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        self.token().await
    }

    /// The control plane's key and interface, and the CA: the mesh, without a token (`proxy control
    /// --create-mesh` at start, when there's none yet; `init` for `flyyy init`).
    pub(crate) async fn create(&self) -> Result<(), String> {
        let private = wg::genkey().await?;
        wg::write_private(&self.key_file(), &private)?;
        let public = wg::pubkey(&private).await?;
        let state = State::default();
        self.save(&state)?;
        wg::up(&self.key_file(), CONTROL_IP).await?;
        *self.ca.lock().unwrap() = Some(std::sync::Arc::new(Ca::open_or_create(&self.dir)?));
        *self.state.lock().unwrap() = Some((state, public.clone()));
        info!(ip = %CONTROL_IP, key = %public, "mesh created");
        Ok(())
    }

    /// `flyyy token`: a new bootstrap key pair; only its public half is kept.
    pub(crate) async fn token(&self) -> Answer {
        let private = wg::genkey().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let public = wg::pubkey(&private).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let token = {
            let mut guard = self.state.lock().unwrap();
            let Some((state, control_pub)) = guard.as_mut() else { return Err(err(StatusCode::CONFLICT, "no mesh yet: run `flyyy init`")) };
            let now = now_ms();
            state.tokens.retain(|t| t.used || now - t.created_ms < TOKEN_TTL_MS);
            let taken = |ip: Ipv4Addr| state.tokens.iter().any(|t| t.ip == ip) || state.nodes.iter().any(|n| n.boot_ip == ip && n.state != NodeState::Joined);
            let Some(ip) = (1..=254).map(|i| Ipv4Addr::new(10, 250, 255, i)).find(|&ip| !taken(ip)) else {
                return Err(err(StatusCode::SERVICE_UNAVAILABLE, "254 join tokens or joins are outstanding"));
            };
            state.tokens.push(TokenRow { key: public, ip, created_ms: now, used: false });
            self.save(state).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            Token { key: private, ip, control_pub: control_pub.clone(), control: self.endpoint.clone() }
        };
        self.reconcile().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        info!(ip = %token.ip, "mesh: join token");
        Ok(serde_json::json!({ "token": token.encode(), "ip": token.ip }))
    }

    /// `flyyy join`, from a token's address: spends the token, registers the node's own key.
    pub(crate) async fn join(&self, from: Ipv4Addr, name: &str, key: &str, role: &str, endpoint: Option<&str>, hostname: Option<&str>) -> Answer {
        if !wg::node_name_ok(name) {
            return Err(err(StatusCode::BAD_REQUEST, format!("bad node name {name:?}: want 1-32 of a-z, 0-9 and -, starting with a letter")));
        }
        // A node can't join as the control plane.
        let role: NodeRole = role.parse().map_err(|()| err(StatusCode::BAD_REQUEST, format!("bad role {role:?}: want worker, edge or dashboard")))?;
        if let Some(e) = endpoint.filter(|e| !wg::endpoint_ok(e)) {
            return Err(err(StatusCode::BAD_REQUEST, format!("bad endpoint {e:?}: want <IPv4>:<port>, [<IPv6>]:<port> or <name>:<port>")));
        }
        if let Some(h) = hostname.filter(|h| !wg::hostname_ok(h)) {
            return Err(err(StatusCode::BAD_REQUEST, format!("bad hostname {h:?}: want 1-64 printable characters, no spaces")));
        }
        if !wg::key_ok(key) {
            return Err(err(StatusCode::BAD_REQUEST, "bad key: want a WireGuard public key (base64, 44 characters)"));
        }
        {
            let mut guard = self.state.lock().unwrap();
            let Some((state, _)) = guard.as_mut() else { return Err(err(StatusCode::CONFLICT, "no mesh")) };
            let now = now_ms();
            let Some(i) = state.tokens.iter().position(|t| t.ip == from && !t.used && now - t.created_ms < TOKEN_TTL_MS) else {
                return Err(err(StatusCode::FORBIDDEN, format!("no unused join token for {from}")));
            };
            // Another key holds it (the token isn't spent): the node picks another.
            if state.name_taken(name) {
                return Err(err(StatusCode::CONFLICT, format!("name {name} is taken; pass --name <other>")));
            }
            if state.nodes.iter().any(|n| n.key == key) || state.tokens.iter().any(|t| t.key == key) {
                return Err(err(StatusCode::BAD_REQUEST, "that key is in use: make the node's own key on the node"));
            }
            state.tokens[i].used = true;
            let boot_key = state.tokens[i].key.clone();
            state.nodes.push(Node {
                name: name.into(),
                key: key.into(),
                boot_key,
                boot_ip: from,
                ip: None,
                state: NodeState::Pending,
                role: Some(role),
                endpoint: String::new(),
                advertised: endpoint.map(str::to_owned),
                joined_ms: now,
                hostname: hostname.map(str::to_owned),
            });
            self.save(state).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        }
        info!(node = %name, %from, %role, advertised = %endpoint.unwrap_or("-"), hostname = %hostname.unwrap_or("-"), "mesh: joined, waiting for `flyyy approve`");
        Ok(serde_json::json!({ "name": name, "state": "pending" }))
    }

    /// What a joining node polls, from its token's address: pending, or approved with its address.
    pub(crate) fn status(&self, from: Ipv4Addr) -> Answer {
        let guard = self.state.lock().unwrap();
        let Some((state, _)) = guard.as_ref() else { return Err(err(StatusCode::CONFLICT, "no mesh")) };
        let n = state
            .nodes
            .iter()
            .find(|n| n.boot_ip == from && n.state != NodeState::Joined)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no join from {from}")))?;
        Ok(serde_json::json!({ "name": n.name, "state": n.state, "ip": n.ip }))
    }

    /// `flyyy approve <name>`: the next free address, and its own key becomes a peer.
    pub(crate) async fn approve(&self, name: &str) -> Answer {
        let ip = {
            let mut guard = self.state.lock().unwrap();
            let Some((state, _)) = guard.as_mut() else { return Err(err(StatusCode::CONFLICT, "no mesh")) };
            let used: Vec<Ipv4Addr> = state.nodes.iter().filter_map(|n| n.ip).collect();
            let n = state.nodes.iter_mut().find(|n| n.name == name).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no node {name}")))?;
            if n.state != NodeState::Pending {
                return Err(err(StatusCode::CONFLICT, format!("{name} is {:?} already", n.state)));
            }
            let free = (0u32..254 * 254).map(|i| Ipv4Addr::new(10, 250, 1 + (i / 254) as u8, 1 + (i % 254) as u8)).find(|ip| !used.contains(ip));
            let ip = free.ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "no free mesh address"))?;
            n.ip = Some(ip);
            n.state = NodeState::Approved;
            self.save(state).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            ip
        };
        self.reconcile().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        info!(node = %name, %ip, "mesh: approved");
        Ok(serde_json::json!({ "name": name, "ip": ip }))
    }

    /// A node's peers, asked from its own address (so with its own key): every other node, and the
    /// control plane. The first such request means it switched, and its bootstrap key is dropped.
    pub(crate) async fn peers(&self, from: Ipv4Addr) -> Answer {
        let (peers, switched) = {
            let mut guard = self.state.lock().unwrap();
            let Some((state, control_pub)) = guard.as_mut() else { return Err(err(StatusCode::CONFLICT, "no mesh")) };
            let me = state.nodes.iter_mut().find(|n| n.ip == Some(from)).ok_or_else(|| err(StatusCode::FORBIDDEN, format!("{from} is not a node")))?;
            let switched = me.state == NodeState::Approved;
            if switched {
                me.state = NodeState::Joined;
            }
            let mut peers = vec![Peer { key: control_pub.clone(), ip: CONTROL_IP, endpoint: String::new() }];
            peers.extend(
                state.nodes.iter().filter(|n| n.ip != Some(from)).filter_map(|n| n.ip.map(|ip| Peer { key: n.key.clone(), ip, endpoint: n.peer_endpoint() })),
            );
            if switched {
                self.save(state).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            }
            (peers, switched)
        };
        if switched {
            info!(%from, "mesh: switched to its own key");
            self.reconcile().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        }
        Ok(serde_json::json!({ "peers": peers }))
    }

    /// A node's certificate, asked from its own mesh address (so with its own WireGuard key): for that
    /// address, its role and name, whatever the CSR says. Its key stays on the node: only the CSR comes here.
    pub(crate) fn cert(&self, from: Ipv4Addr, csr: &str) -> Answer {
        let (role, name) = {
            let guard = self.state.lock().unwrap();
            let Some((state, _)) = guard.as_ref() else { return Err(err(StatusCode::CONFLICT, "no mesh")) };
            let n = state
                .nodes
                .iter()
                .find(|n| n.ip == Some(from) && n.state != NodeState::Pending)
                .ok_or_else(|| err(StatusCode::FORBIDDEN, format!("{from} is not a node")))?;
            (n.role, n.name.clone())
        };
        // Nothing a dashboard runs uses one, and with one it could reach what the CA's certificates open
        // (Corrosion's gossip takes any of them).
        if role == Some(NodeRole::Dashboard) {
            return Err(err(StatusCode::FORBIDDEN, format!("{name} is a dashboard: no certificate")));
        }
        let ca = self.ca.lock().unwrap().clone().ok_or_else(|| err(StatusCode::CONFLICT, "no CA"))?;
        let role = role.map_or(String::new(), |r| r.to_string());
        let cert = ca.issue(csr, from, &role, &name, self.cert_lifetime).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        info!(node = %name, %role, %from, valid_s = self.cert_lifetime.as_secs(), "mesh: certificate issued");
        Ok(serde_json::json!({ "cert": cert, "ca": ca.pem() }))
    }

    /// A certificate for a node on this host (role edge only: nothing else here needs one), for the
    /// control plane's address, asked over loopback. Its join token authorizes it: unused, the token is
    /// spent and the node registered; afterwards the same token renews it, under the same name and role.
    pub(crate) async fn local_cert(&self, token: &str, name: &str, role: &str, csr: &str) -> Answer {
        let token = Token::decode(token).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        if !wg::node_name_ok(name) {
            return Err(err(StatusCode::BAD_REQUEST, format!("bad node name {name:?}: want 1-32 of a-z, 0-9 and -, starting with a letter")));
        }
        if role.parse::<NodeRole>() != Ok(NodeRole::Edge) {
            return Err(err(StatusCode::BAD_REQUEST, format!("role {role:?}: only an edge gets a certificate on the control plane's host")));
        }
        let key = wg::pubkey(&token.key).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let spent = {
            let mut guard = self.state.lock().unwrap();
            let Some((state, _)) = guard.as_mut() else { return Err(err(StatusCode::CONFLICT, "no mesh")) };
            match state.local.iter().find(|l| l.key == key) {
                Some(l) if l.name == name => false,
                Some(l) => return Err(err(StatusCode::FORBIDDEN, format!("that token set up {} here, not {name}", l.name))),
                None => {
                    let now = now_ms();
                    let Some(i) = state.tokens.iter().position(|t| t.key == key && !t.used && now - t.created_ms < TOKEN_TTL_MS) else {
                        return Err(err(StatusCode::FORBIDDEN, "no unused join token of that key (spent, expired, or not this mesh's)"));
                    };
                    if state.name_taken(name) {
                        return Err(err(StatusCode::CONFLICT, format!("name {name} is taken; pass --name <other>")));
                    }
                    state.tokens[i].used = true;
                    state.local.push(Local { name: name.into(), role: NodeRole::Edge, key });
                    self.save(state).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
                    true
                }
            }
        };
        if spent {
            info!(node = %name, "mesh: a local edge, on the control plane's host");
            // The token's bootstrap peer goes.
            self.reconcile().await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        }
        let ca = self.ca.lock().unwrap().clone().ok_or_else(|| err(StatusCode::CONFLICT, "no CA"))?;
        let cert = ca.issue(csr, CONTROL_IP, "edge", name, self.cert_lifetime).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        info!(node = %name, role = "edge", from = %CONTROL_IP, valid_s = self.cert_lifetime.as_secs(), "mesh: certificate issued (local)");
        Ok(serde_json::json!({ "cert": cert, "ca": ca.pem() }))
    }

    /// The control plane's own certificate (role control, 10.250.0.1), for its Corrosion agent: made and
    /// renewed like a node's, from a key made here.
    fn own_cert(&self) -> Result<(), String> {
        let Some(dir) = &self.tls_dir else { return Ok(()) };
        let Some(ca) = self.ca.lock().unwrap().clone() else { return Ok(()) };
        if !pki::needs_renewal(&format!("{dir}/{}", pki::CERT)) {
            return Ok(());
        }
        let (key, csr) = pki::new_key_and_csr("control")?;
        let cert = ca.issue(&csr, CONTROL_IP, "control", "control", self.cert_lifetime)?;
        pki::write_tls(dir, ca.pem(), &key, &cert)?;
        info!(%dir, "mesh: renewed the control plane's own certificate");
        Ok(())
    }

    /// The nodes, for `flyyy mesh`, with the last handshake the control plane saw; the control plane's own
    /// address, key, host, the endpoint nodes dial (in every join token; null without --endpoint) and API.
    pub(crate) async fn list(&self) -> Answer {
        let live = wg::live().await.unwrap_or_default();
        let now = now_ms() / 1000;
        let guard = self.state.lock().unwrap();
        let Some((state, control_pub)) = guard.as_ref() else { return Err(err(StatusCode::CONFLICT, "no mesh yet: run `flyyy init`")) };
        let mut nodes: Vec<_> = state
            .nodes
            .iter()
            .map(|n| {
                let hs = live.iter().find(|l| l.key == n.key).map(|l| l.handshake).filter(|&h| h > 0);
                serde_json::json!({ "name": n.name, "role": n.role.map_or(String::new(), |r| r.to_string()), "ip": n.ip, "state": n.state, "endpoint": n.endpoint, "advertised_endpoint": n.advertised, "peer_endpoint": n.peer_endpoint(), "handshake_age_s": hs.map(|h| now - h as i64), "asked_age_s": (n.joined_ms > 0).then(|| now - n.joined_ms / 1000), "hostname": n.hostname })
            })
            .collect();
        // On this host: the control plane's address and hostname, no WireGuard of their own.
        let own = wg::hostname();
        nodes.extend(state.local.iter().map(|l| serde_json::json!({ "name": l.name, "role": l.role.to_string(), "ip": CONTROL_IP, "state": "local", "endpoint": "", "advertised_endpoint": null, "peer_endpoint": "", "handshake_age_s": null, "asked_age_s": null, "hostname": own })));
        let tokens = state.tokens.iter().filter(|t| !t.used).count();
        Ok(
            serde_json::json!({ "control": { "ip": CONTROL_IP, "key": control_pub, "hostname": own, "endpoint": self.endpoint, "api": format!("{CONTROL_IP}:{}", wg::CONTROL_API_PORT) }, "nodes": nodes, "unused_tokens": tokens }),
        )
    }

    /// The interface's peers from the state, and the endpoints the kernel learned back into it.
    pub(crate) async fn reconcile(&self) -> Result<(), String> {
        if let Err(e) = self.own_cert() {
            warn!(error = %e, "mesh: own certificate");
        }
        let live = wg::live().await?;
        let want = {
            let mut guard = self.state.lock().unwrap();
            let Some((state, _)) = guard.as_mut() else { return Ok(()) };
            let now = now_ms();
            if learn_endpoints(&mut state.nodes, &live) {
                self.save(state)?;
            }
            want(state, now)
        };
        wg::sync(&want).await
    }

    pub(crate) async fn run(self: std::sync::Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = self.reconcile().await {
                warn!(error = %e, "mesh");
            }
        }
    }
}

/// Each node's endpoint as WireGuard last saw it, by its own key; whether any changed. Until its own key
/// has handshaken, where its bootstrap tunnel came from: the same address and listen port (wg::up), so
/// the other nodes can reach it from its approval on, not only once it has switched keys and reached us.
fn learn_endpoints(nodes: &mut [Node], live: &[wg::Live]) -> bool {
    let mut changed = false;
    for n in nodes {
        let seen = |key: &str| live.iter().find(|l| l.key == key && !l.endpoint.is_empty());
        let Some(l) = seen(&n.key).or_else(|| n.endpoint.is_empty().then(|| seen(&n.boot_key)).flatten()) else { continue };
        if l.endpoint != n.endpoint {
            n.endpoint = l.endpoint.clone();
            changed = true;
        }
    }
    changed
}

/// The control plane's peers: unused, unexpired tokens and nodes not yet switched (by their bootstrap
/// key, on its address only), and approved nodes (by their own key).
fn want(state: &State, now: i64) -> Vec<Peer> {
    let boot = |key: &str, ip| Peer { key: key.into(), ip, endpoint: String::new() };
    let mut peers: Vec<Peer> = state.tokens.iter().filter(|t| !t.used && now - t.created_ms < TOKEN_TTL_MS).map(|t| boot(&t.key, t.ip)).collect();
    for n in &state.nodes {
        if n.state != NodeState::Joined {
            peers.push(boot(&n.boot_key, n.boot_ip));
        }
        if let Some(ip) = n.ip {
            peers.push(Peer { key: n.key.clone(), ip, endpoint: String::new() });
        }
    }
    peers.sort();
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_from_the_own_key_else_the_bootstrap_tunnel() {
        let k = |c: char| format!("{}=", c.to_string().repeat(43));
        let live = |key: char, endpoint: &str| wg::Live { key: k(key), allowed: String::new(), endpoint: endpoint.into(), handshake: 1 };
        let node = |key: char, boot: char| Node {
            name: "n".into(),
            key: k(key),
            boot_key: k(boot),
            boot_ip: "10.250.255.1".parse().unwrap(),
            ip: None,
            state: NodeState::Pending,
            role: Some(NodeRole::Worker),
            endpoint: String::new(),
            advertised: None,
            joined_ms: 0,
            hostname: None,
        };
        // Only the bootstrap tunnel has handshaken: its endpoint, until the own key's.
        let mut nodes = vec![node('a', 'b')];
        // The other nodes get the learned endpoint, unless the node advertised one at its join.
        nodes[0].endpoint = "198.51.100.9:40312".into();
        assert_eq!(nodes[0].peer_endpoint(), "198.51.100.9:40312");
        nodes[0].advertised = Some("home.example.com:51821".into());
        assert_eq!(nodes[0].peer_endpoint(), "home.example.com:51821");
        // Learning goes on underneath (shown in /v1/mesh), without changing what the others get.
        nodes[0].endpoint.clear();
        assert!(learn_endpoints(&mut nodes, &[live('b', "10.99.3.2:51820")]) && nodes[0].peer_endpoint() == "home.example.com:51821");
        nodes[0].advertised = None;
        nodes[0].endpoint.clear();
        assert!(learn_endpoints(&mut nodes, &[live('b', "10.99.3.2:51820")]));
        assert_eq!(nodes[0].endpoint, "10.99.3.2:51820");
        assert!(!learn_endpoints(&mut nodes, &[live('b', "10.99.3.2:51820")]));
        // The own key's wins, and the old bootstrap one doesn't take over again.
        assert!(learn_endpoints(&mut nodes, &[live('a', "192.0.2.7:51820"), live('b', "10.99.3.2:51820")]));
        assert_eq!(nodes[0].endpoint, "192.0.2.7:51820");
        assert!(!learn_endpoints(&mut nodes, &[live('b', "10.99.3.2:51820")]));
        // Never seen: nothing.
        let mut fresh = vec![node('c', 'd')];
        assert!(!learn_endpoints(&mut fresh, &[live('d', "")]) && fresh[0].endpoint.is_empty());
    }

    /// A local edge has the control plane's address: role_at says edge there (what binds its announced load
    /// address), its name is taken, and it's no WireGuard peer.
    #[test]
    fn a_local_edge_is_at_the_control_planes_address() {
        let mesh = |local: Vec<Local>| Mesh {
            dir: String::new(),
            state: Mutex::new(Some((State { local, ..State::default() }, String::new()))),
            ca: Mutex::new(None),
            tls_dir: None,
            cert_lifetime: Duration::from_secs(60),
            endpoint: None,
        };
        let none = mesh(vec![]);
        assert_eq!(none.role_at(CONTROL_IP), None);
        let one = mesh(vec![Local { name: "edge0".into(), role: NodeRole::Edge, key: format!("{}=", "a".repeat(43)) }]);
        assert_eq!(one.role_at(CONTROL_IP), Some(NodeRole::Edge));
        assert_eq!(one.role_at("10.250.1.1".parse().unwrap()), None);
        let guard = one.state.lock().unwrap();
        let (state, _) = guard.as_ref().unwrap();
        assert!(state.name_taken("edge0") && !state.name_taken("edge1"));
        assert!(want(state, 0).is_empty());
    }

    /// A node's hostname: a mesh.json from before it loads without one; a join with a bad one is refused
    /// before its token is spent.
    #[test]
    fn hostnames_are_optional_and_checked() {
        let old = r#"{"tokens":[],"nodes":[{"name":"worker1","key":"k","boot_key":"b","boot_ip":"10.250.255.1","ip":"10.250.1.1","state":"joined"}]}"#;
        let state: State = serde_json::from_str(old).unwrap();
        assert_eq!(state.nodes[0].hostname, None);
        let k = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=".to_owned();
        let from: Ipv4Addr = "10.250.255.1".parse().unwrap();
        let tokens = vec![TokenRow { key: format!("{}=", "b".repeat(43)), ip: from, created_ms: now_ms(), used: false }];
        let mesh = Mesh {
            dir: String::new(),
            state: Mutex::new(Some((State { tokens, ..State::default() }, String::new()))),
            ca: Mutex::new(None),
            tls_dir: None,
            cert_lifetime: Duration::from_secs(60),
            endpoint: None,
        };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let e = rt.block_on(mesh.join(from, "worker1", &k, "worker", None, Some("two words"))).unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert!(!mesh.state.lock().unwrap().as_ref().unwrap().0.tokens[0].used, "the token isn't spent");
        // Any node name, not workerN; one another key holds is refused, the token still unspent.
        for bad in ["1box", "Nuc", "my_host"] {
            assert_eq!(rt.block_on(mesh.join(from, bad, &k, "worker", None, None)).unwrap_err().status, StatusCode::BAD_REQUEST, "{bad}");
        }
        mesh.state.lock().unwrap().as_mut().unwrap().0.local.push(Local { name: "nuc".into(), role: NodeRole::Edge, key: format!("{}=", "c".repeat(43)) });
        let e = rt.block_on(mesh.join(from, "nuc", &k, "worker", None, None)).unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (StatusCode::CONFLICT, "name nuc is taken; pass --name <other>"));
        assert!(!mesh.state.lock().unwrap().as_ref().unwrap().0.tokens[0].used, "the token isn't spent");
    }

    #[test]
    fn peers_follow_tokens_and_join_states() {
        let k = |c: char| format!("{}=", c.to_string().repeat(43));
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let node = |name: &str, key: char, boot: char, b: &str, addr: Option<&str>, state| Node {
            name: name.into(),
            key: k(key),
            boot_key: k(boot),
            boot_ip: ip(b),
            ip: addr.map(ip),
            state,
            role: Some(NodeRole::Worker),
            endpoint: String::new(),
            advertised: None,
            joined_ms: 0,
            hostname: None,
        };
        let state = State {
            local: vec![],
            tokens: vec![
                TokenRow { key: k('a'), ip: ip("10.250.255.1"), created_ms: 1000, used: false },
                TokenRow { key: k('b'), ip: ip("10.250.255.2"), created_ms: 0, used: false },
                TokenRow { key: k('c'), ip: ip("10.250.255.3"), created_ms: 1000, used: true },
            ],
            nodes: vec![
                node("pending", 'p', 'c', "10.250.255.3", None, NodeState::Pending),
                node("approved", 'q', 'd', "10.250.255.4", Some("10.250.1.1"), NodeState::Approved),
                node("joined", 'r', 'e', "10.250.255.5", Some("10.250.1.2"), NodeState::Joined),
            ],
        };
        // At TTL + 500 ms: token b expired, a not; c is used (its node is pending, so its key stays).
        let got: Vec<(String, String)> = want(&state, TOKEN_TTL_MS + 500).into_iter().map(|p| (p.key[..1].to_owned(), p.ip.to_string())).collect();
        let mut expect = vec![("a", "10.250.255.1"), ("c", "10.250.255.3"), ("d", "10.250.255.4"), ("q", "10.250.1.1"), ("r", "10.250.1.2")]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect::<Vec<_>>();
        expect.sort();
        let mut got_sorted = got.clone();
        got_sorted.sort();
        assert_eq!(got_sorted, expect, "the joined node's bootstrap key (e) and the expired token (b) are gone");
    }
}
