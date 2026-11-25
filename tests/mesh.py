# mesh: the WireGuard mesh run by the control plane. boot() already made it (flyyy init, a token per node,
# flyyy join, flyyy approve); this checks the result, then walks worker4 through a join step by step: the
# token is spent once, a pending node reaches only the control plane's join routes, approval switches it to
# its own key (made on the node, never sent anywhere) and drops the bootstrap key, and a restart resumes
# without a token.
NODES = ["worker1", "worker2", "worker3", "edges"]
NODE_M = {"worker1": worker1, "worker2": worker2, "worker3": worker3, "edges": edges}


def wg_peers(m) -> dict[str, str]:
    """public key -> allowed-ips on m's flyyy0."""
    out = m.succeed("wg show flyyy0 allowed-ips").strip()
    return {l.split("\t")[0]: l.split("\t")[1] for l in out.splitlines() if "\t" in l}


def own_key(m) -> str:
    return m.succeed("wg pubkey < /var/lib/flyyy-mesh/node.key").strip()


ctl_key = json.loads(fly("mesh --json"))["control"]["key"]

# 1. Every node joined, with an address, its own key, and a full mesh of peers.
nodes = mesh_nodes()
check(all(nodes[n]["state"] == "joined" and nodes[n]["ip"].startswith("10.250.1.") for n in NODES),
      "mesh: " + ", ".join(f"{n} {nodes[n]['ip']}" for n in NODES) + " joined", f"mesh: {nodes}")
keys = {n: own_key(NODE_M[n]) for n in NODES}
ctl = wg_peers(control)
check(all(ctl.get(keys[n]) == f"{nodes[n]['ip']}/32" for n in NODES), "mesh: the control plane has each node's own key, for its address only", f"mesh: control peers {ctl}")
check(not any(a.startswith("10.250.255.") for a in ctl.values()), "mesh: no bootstrap key left on the control plane (all tokens spent)", f"mesh: control peers {ctl}")
for n in NODES:
    want = {ctl_key: "10.250.0.1/32"} | {keys[o]: f"{nodes[o]['ip']}/32" for o in NODES if o != n}
    got = {}
    def full() -> bool:
        global got
        got = wg_peers(NODE_M[n])
        return got == want
    check(until(full, PROPAGATE), f"mesh: {n}'s peers are the control plane and the other {len(NODES) - 1} nodes", f"mesh: {n} peers {got}, want {want}")
# Every node polls the control plane over the mesh, so each has a handshake with it.
for n in NODES:
    hs = dict(l.split("\t") for l in NODE_M[n].succeed("wg show flyyy0 latest-handshakes").strip().splitlines())
    check(int(hs.get(ctl_key, "0")) > 0, f"mesh: {n} has handshaken with the control plane", f"mesh: {n} handshakes {hs}")

# 2. Nothing between nodes on the LAN: TCP listeners only on loopback or the mesh, UDP only WireGuard. The
# edges also serve the ingress addresses (10.200.0.0/16 here: the shared IP and the pool), for clients.
for n in NODES:
    m = NODE_M[n]
    tcp = m.succeed("ss -Htln | awk '{print $4}'").split()
    udp = m.succeed("ss -Huln | awk '{print $4}'").split()
    ok_tcp = ("127.", "[::1]", "10.250.") + (("10.200.",) if n == "edges" else ())
    off = [a for a in tcp if not a.startswith(ok_tcp)] + [a for a in udp if not (a.startswith(("127.", "10.250.")) or a.endswith(":51820"))]
    check(not off, f"mesh: {n} listens only on loopback and the mesh (and WireGuard's UDP 51820)", f"mesh: {n} listens on {off}")

# 3. A join step by step: worker4 with a fresh token. First without its underlay address (as the one
# join that ever stalled: its address unit had failed): the join says why it gets no answer, and goes on
# once the address is back.
worker4.start()
worker4.wait_for_unit("multi-user.target")
underlay_up(worker4)
under = worker4.succeed("ip -4 -o addr show dev eth1 | awk '$4 ~ /^10\\.99\\./ {print $4}'").strip()
worker4.succeed(f"ip addr del {under} dev eth1")
token = fly("token").strip()
boot_ip = token.split(":")[2]
mesh_start(worker4, token)
hint = lambda: "still no answer from the control plane" in journal(worker4, "mesh")
check(until(hint, BOOT), "join: without a network, it says why after 20 s", f"join: no hint in {journal(worker4, 'mesh')[-600:]}")
line = next(l for l in journal(worker4, "mesh").splitlines() if "still no answer" in l)
check("no WireGuard handshake" in line and "no route to 10.99.0.2:51820" in line, f"join: the hint names the cause: {line.split(': ', 1)[1][:160]}...", f"join: hint {line}")
worker4.succeed(f"ip addr add {under} dev eth1")
check(until(lambda: mesh_nodes().get("worker4", {}).get("state") == "pending", ACT, 0.5), f"join: worker4 pending, from {boot_ip}", f"join: {mesh_nodes().get('worker4')}")
asked = mesh_nodes()["worker4"].get("asked_age_s")
check(isinstance(asked, int) and 0 <= asked <= ACT + 5, f"join: /v1/mesh says when worker4 asked ({asked} s ago), for the dashboard's banner", f"join: asked_age_s {asked}")
# Its host's name, from its join (the dashboard's "on <host>"); the control plane's own, under "control".
host4, ctl_host = worker4.succeed("cat /proc/sys/kernel/hostname").strip(), control.succeed("cat /proc/sys/kernel/hostname").strip()
mesh_json = json.loads(fly("mesh --json"))
check(mesh_nodes()["worker4"].get("hostname") == host4 and mesh_json["control"].get("hostname") == ctl_host
      and mesh_json["control"].get("endpoint") == "10.99.0.2:51820" and mesh_json["control"].get("api") == "10.250.0.1:4000",
      f"join: /v1/mesh has worker4's hostname ({host4}), the control plane's ({ctl_host}), its endpoint and API", f"join: hostnames {mesh_nodes()['worker4'].get('hostname')}, {mesh_json['control']}")
check(worker4.execute("test -e /var/lib/flyyy-mesh/join-token")[0] == 0, "join: the token file stays until approved (resumable)", "join: token file gone early")
api = "http://10.250.0.1:4000"
# The token is spent: a second join from its address is refused.
k2 = worker4.succeed("wg genkey | wg pubkey").strip()
rc = worker4.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' --max-time {REQUEST} -X POST -d '{{\"name\": \"again\", \"key\": \"{k2}\", \"role\": \"worker\"}}' {api}/v1/mesh/join")
check(rc == "403", "join: the token is accepted once (a second join from its address -> 403)", f"join: second join -> {rc}")
# Pending: only the join routes; no peers, no admin API without the bearer token, no other node.
for path, want in [("/v1/mesh/peers", "403"), ("/v1/apps", "401"), ("/v1/mesh", "401")]:
    rc = worker4.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' --max-time {REQUEST} {api}{path}")
    check(rc == want, f"pending: GET {path} -> {want}", f"pending: GET {path} -> {rc}, want {want}")
def tcp_open(m, addr: str, port: int) -> bool:
    return m.execute(f"timeout {REQUEST} bash -c 'exec 3<>/dev/tcp/{addr}/{port}'")[0] == 0


check(not tcp_open(worker4, MESH["worker1"], 4280), "pending: worker1's backhaul unreachable (no peer for it)", "pending: reached worker1's backhaul")
check(set(wg_peers(worker4)) == {ctl_key}, "pending: worker4's only peer is the control plane", f"pending: peers {wg_peers(worker4)}")

# worker4 advertised 10.99.40.2:51820 at its join (tests/default.nix): a second address of its own. The
# address the control plane learns from its packets (10.99.4.2) is made unreachable from worker1, so
# worker1 and worker4 can handshake only if worker1 was given the advertised one.
ADVERTISED, LEARNED = "10.99.40.2:51820", "10.99.4.2"
worker4.succeed("ip addr add 10.99.40.2/16 dev eth1")
worker1.succeed(f"ip route add blackhole {LEARNED}/32")
check(worker1.execute(f"ping -c 1 -W 1 {LEARNED}")[0] != 0 and worker1.succeed("ping -c 1 -W 2 10.99.40.2") != "",
      f"advertise: worker1 can't reach worker4's learned address ({LEARNED}), only the advertised one", "advertise: blackhole not in place")
fly("approve worker4")
check(until(lambda: mesh_nodes()["worker4"]["state"] == "joined", ACT, 0.5), "approve: worker4 switched to its own key", f"approve: {mesh_nodes()['worker4']}")
w4 = mesh_nodes()["worker4"]
ip4 = worker4.succeed("ip -4 -o addr show dev flyyy0 | awk '{print $4}'").strip()
check(ip4 == f"{w4['ip']}/16", f"approve: worker4's flyyy0 is {ip4} (the bootstrap address is gone)", f"approve: flyyy0 {ip4}, want {w4['ip']}/16")
k4 = own_key(worker4)
ctl = wg_peers(control)
check(ctl.get(k4) == f"{w4['ip']}/32" and f"{boot_ip}/32" not in ctl.values(), "approve: the control plane has worker4's own key, its bootstrap key is gone",
      f"approve: control peers {ctl}")
gone = worker4.execute("test -e /var/lib/flyyy-mesh/join-token || test -e /var/lib/flyyy-mesh/boot.key")[0] != 0
check(gone, "approve: the spent token and its key are deleted on worker4", "approve: token or boot key still there")
check(until(lambda: wg_peers(worker1).get(k4) == f"{w4['ip']}/32", PROPAGATE), "approve: worker1 has worker4 as a peer (full mesh)", f"approve: worker1 peers {wg_peers(worker1)}")
check(until(lambda: tcp_open(worker4, MESH["worker1"], 4280), PROPAGATE), "approve: worker4 reaches worker1's backhaul over the mesh, node to node", "approve: no TCP to worker1's backhaul")
w4 = mesh_nodes()["worker4"]
check(w4["advertised_endpoint"] == ADVERTISED and w4["peer_endpoint"] == ADVERTISED and w4["endpoint"].startswith(f"{LEARNED}:"),
      f"advertise: /v1/mesh has worker4's advertised endpoint ({ADVERTISED}) for the others, and the learned one ({w4['endpoint']})", f"advertise: {w4}")


def wg_of(m, what: str) -> dict[str, str]:
    return dict(l.split("\t") for l in m.succeed(f"wg show flyyy0 {what}").strip().splitlines())


# Until their first handshake, worker4's initiations come from the learned address and WireGuard roams
# worker1 there, until worker1's next peer sync (5 s) sets the advertised one again; once they've
# handshaken there, worker4 answers and sends from the advertised address, and it stays.
at_advertised = lambda: wg_of(worker1, "endpoints").get(k4) == ADVERTISED and int(wg_of(worker1, "latest-handshakes").get(k4, "0")) > 0
check(until(at_advertised, PROPAGATE), f"advertise: worker1 has worker4 at {ADVERTISED} and has handshaken with it there",
      f"advertise: worker1 endpoint {wg_of(worker1, 'endpoints').get(k4)}, handshake {wg_of(worker1, 'latest-handshakes').get(k4)}")
check(f"{ADVERTISED} (advertised)" in fly("mesh"), "advertise: flyyy mesh shows it, marked advertised", f"advertise: {fly('mesh')}")
# Nodes without one keep the learned address.
check(all(not n.get("advertised_endpoint") and n["peer_endpoint"] == n["endpoint"] for name, n in mesh_nodes().items() if name != "worker4"),
      "advertise: the other nodes keep their learned endpoints", f"advertise: {mesh_nodes()}")
worker1.succeed(f"ip route del blackhole {LEARNED}/32")

# 4. Restart resumes from state, without a token: also after the interface is gone (as after a reboot).
worker4.succeed("ip link del flyyy0")
worker4.succeed("systemctl restart mesh")
check(until(lambda: worker4.execute(f"ip -4 -o addr show dev flyyy0 | grep -q 'inet {w4['ip']}/16'")[0] == 0, PROPAGATE),
      "restart: worker4 came back as itself without a token", "restart: flyyy0 not back")
check(until(lambda: set(wg_peers(worker4)) == {ctl_key} | set(keys.values()), PROPAGATE), "restart: and got its peers again", f"restart: peers {wg_peers(worker4)}")

# 5. mTLS inside the mesh: the control plane's CA issued each node a certificate from its CSR (the key
# made on the node), for its mesh address, role and name; short-lived (120 s in this suite) and renewed.
CA = f"{TLS_DIR}/ca.pem"


def cert_info(m) -> str:
    return m.succeed(f"openssl x509 -in {TLS_DIR}/cert.pem -noout -issuer -serial -ext subjectAltName -dates 2>&1")


ROLE = {"worker1": "worker", "worker2": "worker", "worker3": "worker", "edges": "edge", "worker4": "worker"}
for n, m in list(NODE_M.items()) + [("worker4", worker4)]:
    until(lambda: m.execute(f"test -s {TLS_DIR}/cert.pem")[0] == 0, ACT)
    info = cert_info(m)
    ip = mesh_nodes()[n]["ip"]
    ok_san = f"IP Address:{ip}" in info and f"URI:flyyy://{ROLE[n]}/{n}" in info and "issuer=CN=flyyy mesh CA" in info
    check(ok_san, f"mtls: {n}'s certificate is for {ip} and flyyy://{ROLE[n]}/{n}, from the mesh CA", f"mtls: {n}: {info}")
    perms = m.succeed(f"stat -c '%a %G' {TLS_DIR}/key.pem").strip()
    check(perms == "640 meshtls", f"mtls: {n}'s key is 0640, group meshtls", f"mtls: {n} key {perms}")
info = cert_info(control)
check("IP Address:10.250.0.1" in info and "URI:flyyy://control/control" in info, "mtls: the control plane has its own certificate (control, 10.250.0.1)", f"mtls: control: {info}")
# The CA never saw a node's key: worker1's key isn't anywhere on the control plane.
# Its third line: pure key material (the second starts with the PKCS#8 header every P-256 key shares).
# /var/lib is where the control plane keeps state; bounded, so a stuck read fails instead of hanging.
k1 = worker1.succeed(f"sed -n 3p {TLS_DIR}/key.pem").strip()
check(control.execute(f"timeout {BOOT} grep -rqF -D skip -- {shlex.quote(k1)} /var/lib")[0] == 1, "mtls: worker1's private key never left worker1", "mtls: worker1's key found on the control plane")
# Corrosion gossips over mTLS, and still converges.
# (Read when it's there: with 120 s certificates Corrosion restarts every minute, recreating its runtime dir.)
cfg = ""
def read_cfg() -> bool:
    global cfg
    rc, cfg = worker1.execute("cat /run/corrosion/config.toml")
    return rc == 0
until(read_cfg, PROPAGATE)
check("[gossip.tls]" in cfg and "plaintext" not in cfg and "[gossip.tls.client]" in cfg, "mtls: Corrosion gossip is TLS with client certificates, not plaintext", f"mtls: corrosion config {cfg}")
# The backhaul takes edges' certificates only: an edge's works, a worker's and none are refused.
check(until(lambda: bh_code("worker1", "nope") == "404", PROPAGATE), "mtls: an edge certificate is accepted by worker1's backhaul (CONNECT nope -> 404)", f"mtls: edge -> {bh_code("worker1", 'nope')}")
w4cert = f"--proxy-cacert {CA} --proxy-cert {TLS_DIR}/cert.pem --proxy-key {TLS_DIR}/key.pem"
rc = worker4.execute(f"curl -s -o /dev/null --max-time {REQUEST} -p -x https://{MESH['worker1']}:4280 {w4cert} http://nope/")[0]
check(rc != 0, f"mtls: a worker's certificate is refused by worker1's backhaul (curl exit {rc})", "mtls: a worker certificate was accepted")
rc = edges.execute(f"curl -s -o /dev/null --max-time {REQUEST} -p -x https://{MESH['worker1']}:4280 --proxy-cacert {CA} http://nope/")[0]
check(rc != 0, f"mtls: no client certificate is refused too (curl exit {rc})", "mtls: no certificate was accepted")
check('want role edge, got (role, name) Some(("worker", "worker4"))' in journal(worker1, "proxy"), "mtls: the backhaul logs why it refused worker4", "mtls: no refusal logged")

# Renewal: past half its lifetime a node gets a new certificate (new key, new serial); Corrosion is
# restarted for it, the edges and backhauls reload it, and everything still works.
def serial(m) -> str:
    s = re.search(r"serial=(\S+)", cert_info(m))
    return s.group(1) if s else ""



s1, e1, c1 = serial(worker1), serial(edges), serial(control)
pid = worker1.succeed("systemctl show -P MainPID corrosion").strip()
check(until(lambda: serial(worker1) != s1 and serial(edges) != e1 and serial(control) != c1, DEPLOY, 2),
      "mtls: worker1, the edges and the control plane renewed their certificates", f"mtls: serials {s1} {e1} {c1} -> {serial(worker1)} {serial(edges)} {serial(control)}")
check(until(lambda: worker1.succeed("systemctl show -P MainPID corrosion").strip() not in ("0", pid), ACT), "mtls: worker1's Corrosion restarted for its new certificate", "mtls: corrosion not restarted")
check(until(lambda: "reloaded the renewed" in journal(edges, "edge1"), ACT), "mtls: edge1 reloaded its renewed certificate", "mtls: edge1 didn't reload")
tx([["INSERT INTO machine_status (id, worker, app, state, addr) VALUES ('renew-probe', 'worker9', 'probe', 'stopped', '')", []]])
def probe_state() -> str:
    """The probe row's state on worker1, or "" while its Corrosion is restarting (every minute here)."""
    try:
        return fact("renew-probe")[0]
    except Exception:
        return ""


check(until(lambda: probe_state() == "stopped", BOOT), "mtls: gossip still converges after the renewal (control -> worker1)", "mtls: gossip broken after renewal")
check(until(lambda: bh_code("worker1", "nope") == "404", ACT), "mtls: the backhaul still takes the edges' renewed certificate", f"mtls: edge -> {bh_code("worker1", 'nope')}")


# Single-host (dev) mode is refused on a mesh: by the control plane (its mesh exists) and by a node with mesh
# certificates. Run as one-off processes of the same binary, next to the real ones.
proxy_of = lambda m, unit: f"$(readlink -f /proc/$(systemctl show -P MainPID {unit})/exe)"
rc, out = control.execute(f"timeout {ACT} {proxy_of(control, 'control')} control --listen 127.0.0.1:4999 --corrosion 127.0.0.1:8080 --corrosion-token-file {CORRO_TOKEN} --token-file /etc/control.token --mesh-dir /var/lib/flyyy-control --single-host 2>&1")
check(rc != 0 and "refusing --single-host: a mesh exists" in out, "single-host: the control plane refuses it once a mesh exists", f"single-host: control {rc} {out[-300:]}")
rc, out = edges.execute(f"timeout {ACT} {proxy_of(edges, 'edge1')} edge --listen 127.0.0.1:4998 --state 127.0.0.1:4997 --corrosion 127.0.0.1:8101 --corrosion-token-file {CORRO_TOKEN} --name edge9 --tls-dir /var/lib/flyyy-tls --single-host 2>&1")
check(rc != 0 and "refusing --single-host: this node is on a mesh" in out, "single-host: an edge on the mesh refuses it", f"single-host: edge {rc} {out[-300:]}")
rc, out = worker1.execute(f"timeout {ACT} {proxy_of(worker1, 'proxy')} worker --listen 127.0.0.1:4996 --state 127.0.0.1:4995 --corrosion 127.0.0.1:8080 --corrosion-token-file {CORRO_TOKEN} --worker worker1 --tls-dir /var/lib/flyyy-tls --single-host 2>&1")
check(rc != 0 and "refusing --single-host: this node is on a mesh" in out, "single-host: a worker's backhaul on the mesh refuses it", f"single-host: backhaul {rc} {out[-300:]}")
rc, out = worker1.execute(f"timeout {ACT} {proxy_of(worker1, 'proxy')} worker --listen 127.0.0.1:4996 --state 127.0.0.1:4995 --corrosion 127.0.0.1:8080 --corrosion-token-file {CORRO_TOKEN} --worker worker1 2>&1")
check(rc != 0 and "or loopback with --single-host" in out, "single-host: without it, a loopback backhaul listen address is refused", f"single-host: loopback listen {rc} {out[-300:]}")


# An edge node's listen address, as its edges announce it in their `edges` rows: both of the node's edges
# serve HTTPS on 127.0.0.1:443, so one address; `flyyy mesh` shows it under LISTEN.
listen_of = lambda: mesh_nodes().get("edges", {}).get("listen")
check(until(lambda: listen_of() == ["127.0.0.1:443"], PROPAGATE), "listen: /v1/mesh says the edges node serves on 127.0.0.1:443", f"listen: {listen_of()}")
table = fly("mesh")
head = next((l for l in table.splitlines() if l.startswith("NAME")), "")
row = next((l for l in table.splitlines() if l.startswith("edges ")), "")
check("LISTEN" in head and "127.0.0.1:443" in row and all("-" in r.split()[-3:] for r in table.splitlines() if r.startswith("worker")),
      "listen: flyyy mesh has a LISTEN column, the edges' address there and - for workers", f"listen: flyyy mesh\n{table}")
