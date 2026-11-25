# hosts: the control host's apps on one machine without flyyy's modules (`ctl`, its firewall on): carol runs
# #control, #edge and #dashboard (nix/host-apps.nix). The edge doesn't join (one flyyy0 per host): it has the
# control plane's mesh address and a certificate for its join token, and reads the control plane's Corrosion;
# the dashboard calls the local API. Then a worker joins with #join (`plain`, as bob), and an app answers
# through that edge, asked from the worker's machine. Nothing runs as root. The apps run as root with --user:
# the build sandbox can't make sudo's setuid wrapper, so their sudo path isn't covered here.
import shlex
OP_TOKEN = "/var/lib/flyyy-operator/token"
FLY = f"FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE={OP_TOKEN} flyyy"
EDGE_TLS = "--tls-cert /var/lib/edge-tls/cert.pem --tls-key /var/lib/edge-tls/key.pem"
DASH = "--listen 0.0.0.0:9443 --host dash.flyyy.localhost --tls-cert /var/lib/dash-tls/cert.pem --tls-key /var/lib/dash-tls/key.pem"
CONTROL_UNITS = ["flyyy-control", "flyyy-control-corrosion"]
EDGE_UNITS = ["flyyy-edge-cert", "flyyy-edge", "flyyy-edge-anyip"]


def fly(args: str) -> str:
    return ctl.succeed(f"{FLY} {args}")


def active(m, units: list[str]) -> bool:
    return all(m.execute(f"systemctl is-active -q {u}")[0] == 0 for u in units)


def prop(m, unit: str, p: str) -> str:
    return m.succeed(f"systemctl show -P {p} {unit}").strip()


def nodes() -> dict[str, dict]:
    return {n["name"]: n for n in json.loads(fly("mesh --json"))["nodes"]}


def worker_alive() -> bool:
    return any(w["name"] == "worker1" and w["alive"] for w in json.loads(fly("workers --json")))


def via_edge() -> tuple[str, str]:
    """(x-edge, the machine that answered) of a request to whoami through ctl's edge, from plain."""
    rc, out = plain.execute(f"curl -sS -D - --max-time {REQUEST} --resolve whoami.flyyy.localhost:443:10.99.8.2 https://whoami.flyyy.localhost/")
    edge = next((l.split(":", 1)[1].strip() for l in out.splitlines() if l.lower().startswith("x-edge:")), "")
    return (edge, id_of(out)) if rc == 0 else ("", "")


for m in [registry, ctl, plain]:
    m.start()
registry.wait_for_open_port(5000)
DIGESTS.update(json.loads(registry.succeed("cat /etc/flyyy-images.json")))
for m in [ctl, plain]:
    m.wait_for_unit("multi-user.target")
    m.wait_until_succeeds("ip -4 -o addr show dev eth1 | grep -q 'inet 10\\.99\\.'", timeout=ACT)
check(ctl.execute("systemctl cat control mesh corrosion")[0] != 0, "ctl: no flyyy module (no control, mesh or corrosion unit)", "ctl has flyyy's units")

# 1. #control: the control plane and its Corrosion, the mesh made at the first start.
rc, out = ctl.execute("flyyy-control --endpoint 10.99.8.2 2>&1")
check(rc != 0 and "--user NAME" in out, "#control as root without --user: refused (nothing runs as root)", f"as root: exit {rc}\n{out}")
rc, out = ctl.execute("flyyy-control --user carol --endpoint 10.99.8.2 2>&1")
shown('#control, first start', out)
op_token = ctl.succeed(f"cat {OP_TOKEN}").strip()
lines = out.splitlines()
check(rc == 0 and lines == ["✓ control plane up (endpoint 10.99.8.2)", f"operator token: {op_token}"],
      "#control: two lines, up and the operator token", f"#control: exit {rc}\n{out}")
rc, out = ctl.execute("flyyy-control --user carol --nixos 2>&1")
check(rc == 0 and "nixosModules.control" in out and 'endpoint = "10.99.8.2"' in out and "✓" not in out and active(ctl, CONTROL_UNITS),
      "#control --nixos: the module's setup (the endpoint kept), and nothing else done", f"#control --nixos: exit {rc}\n{out}")
# flyyy's defaults are this host's: the operator token's file and 10.250.0.1:4000, so carol needs no flags.
rc, out = ctl.execute("runuser -u carol -- flyyy orgs create acme 2>&1")
listed = ctl.execute("runuser -u carol -- flyyy orgs --json")[1] if rc == 0 else "[]"
check(rc == 0 and any(o["name"] == "acme" for o in json.loads(listed)),
      "flyyy as carol with its defaults (the operator token's file, 10.250.0.1:4000): orgs create acme works as shown", f"flyyy defaults: {rc} {out} {listed}")
mesh0 = json.loads(fly("mesh --json"))
check(mesh0["unused_tokens"] == 0 and mesh0["nodes"] == [], "#control: the mesh has no join token and no node yet", f"#control: mesh {mesh0}")
# The edge's token, as the dashboard's + Add node (or flyyy token) makes it: it carries the endpoint.
tok = fly("token").strip()
check(tok.startswith("flyyy-join:") and tok.endswith("@10.99.8.2:51820"), "flyyy token: a join token carrying 10.99.8.2:51820", f"token {tok}")
check(active(ctl, CONTROL_UNITS) and {prop(ctl, u, "User") for u in CONTROL_UNITS} == {"carol"}
      and prop(ctl, "flyyy-control", "AmbientCapabilities") == "cap_net_admin" and not prop(ctl, "flyyy-control-corrosion", "AmbientCapabilities"),
      "#control: flyyy-control and -corrosion run as carol; only flyyy-control has CAP_NET_ADMIN", f"#control: units {[(u, prop(ctl, u, 'User'), prop(ctl, u, 'AmbientCapabilities')) for u in CONTROL_UNITS]}")
check(ctl.execute("iptables -C nixos-fw -p udp --dport 51820 -j nixos-fw-accept && iptables -C nixos-fw -i flyyy0 -j nixos-fw-accept")[0] == 0,
      "#control: NixOS's firewall lets WireGuard and the mesh in", "#control: no firewall rule")
check(until(lambda: ctl.execute("ss -Hln | grep -q 10.250.0.1:8787 && ss -Htln | grep -q 10.250.0.1:4000")[0] == 0, ACT),
      "#control: its Corrosion (gossip) and API on the mesh address",
      "#control: nothing on 10.250.0.1")
# The CLI as #control's output says to get it, from the flake (here a stand-in flake with the same apps.flyyy:
# tests/lib.nix), with its defaults: the mesh API and the operator token's file.
# As carol, the operator token's owner. The container's /nix/store is the host's, read-only and without a
# database (no nix-daemon), so nix runs on a chroot store in her home holding the stand-in flake's closure
# (nix run enters it in a user and mount namespace).
cli = "su carol -s /bin/sh -c " + shlex.quote(
    "mkdir -p ~/cli/nix/store && for p in $(grep '^/nix/store/' /etc/flyyy-flake-reg | sort -u); do cp -a $p ~/cli/nix/store/; done"
    " && nix-store --store ~/cli --load-db < /etc/flyyy-flake-reg"
    " && nix --store ~/cli run \"path:$(readlink -f /etc/flyyy-flake)#flyyy\" -- orgs 2>&1")
rc, out = ctl.execute(cli)
shown("nix run <flake>#flyyy -- orgs", out)
check(rc == 0 and "error" not in out.lower(), "nix run <flake>#flyyy -- orgs on the control host: the CLI alone, with its defaults", f"flyyy via nix run: exit {rc}\n{out}")

# 2. #edge on the control host: no join; the control plane's address, a certificate for the token, its Corrosion.
rc, out = ctl.execute(f"flyyy-edge --user carol --name edge1 --listen 0.0.0.0:443 {EDGE_TLS} {shlex.quote(tok)} 2>&1")
shown('#edge, first start (on the control host)', out)
check(rc == 0 and "✓ edge1 serves apps on 0.0.0.0:443" in out and out.rstrip().endswith("permanent setup: add --nixos") and len(out.splitlines()) <= 5
      and "containers" not in out and "carol" not in out and "no certificate" not in out,
      "#edge on the control host: up, a short result (no certificate warning: it has one)", f"#edge: exit {rc}\n{out}")
n = nodes().get("edge1", {})
check(n.get("state") == "local" and n.get("ip") == "10.250.0.1" and n.get("role") == "edge", "#edge: a local edge node at the control plane's address, in /v1/mesh",
      f"#edge: mesh {nodes()}")
san = ctl.succeed("openssl x509 -noout -ext subjectAltName -in /var/lib/flyyy-edge/tls/cert.pem")
check("10.250.0.1" in san and "flyyy://edge/edge1" in san, "#edge: its certificate is for 10.250.0.1, role edge, edge1", f"#edge: SANs {san}")
check(ctl.succeed("ip -o link show type wireguard | wc -l").strip() == "1" and ctl.execute("systemctl cat flyyy-edge-mesh flyyy-edge-corrosion")[0] != 0,
      "#edge: one WireGuard interface on the host, and no mesh or Corrosion of the edge's own", "#edge: a second interface or its own mesh unit")
check(active(ctl, EDGE_UNITS) and {prop(ctl, u, "User") for u in EDGE_UNITS} == {"carol"} and prop(ctl, "flyyy-edge", "AmbientCapabilities") == "cap_net_bind_service",
      "#edge: flyyy-edge-cert, flyyy-edge and -anyip run as carol; flyyy-edge may bind :443", f"#edge: {[(u, prop(ctl, u, 'User'), prop(ctl, u, 'AmbientCapabilities')) for u in EDGE_UNITS]}")
rc, out = ctl.execute("runuser -u carol -- flyyy local-cert --token-file /var/lib/flyyy-edge/token --name edge9 --tls-dir /tmp 2>&1")
check(rc != 0 and "that token set up edge1" in out, "the token is edge1's now: another name with it is refused", f"local-cert edge9: exit {rc}\n{out}")
rc, out = ctl.execute(f"flyyy-edge --user carol --listen 0.0.0.0:443 {EDGE_TLS} {shlex.quote(fly('token').strip())} 2>&1")
check(rc != 0 and "edge is edge1 already" in out and active(ctl, EDGE_UNITS), "#edge again with a token: refused, edge1 is this host's edge (still running)", f"#edge again: exit {rc}\n{out}")

# 3. #dashboard on the control host: the local API, no mesh, no token.
rc, out = ctl.execute(f"flyyy-dashboard --user carol {DASH} {shlex.quote(tok)} 2>&1")
check(rc != 0 and "needs no join token" in out, "#dashboard on the control host with a token: refused, it needs none", f"#dashboard with a token: exit {rc}\n{out}")
rc, out = ctl.execute(f"flyyy-dashboard --user carol {DASH} 2>&1")
shown('#dashboard (on the control host)', out)
check(rc == 0 and "✓ dashboard on https://dash.flyyy.localhost/ (listening on 0.0.0.0:9443)" in out and "next: sign in with the operator token, then + Add node" in out
      and len(out.splitlines()) <= 3 and "--nixos" not in out and "containers" not in out and ctl.execute("systemctl cat flyyy-dashboard-mesh")[0] != 0,
      "#dashboard on the control host: up, no mesh unit; a short result, sign in next (no --nixos hint)", f"#dashboard: exit {rc}\n{out}")
rc, out = ctl.execute(f"flyyy-dashboard --user carol {DASH} --nixos 2>&1")
rc2, out2 = ctl.execute(f"flyyy-edge --user carol --listen 0.0.0.0:443 {EDGE_TLS} --nixos 2>&1")
check(rc == 0 and "containers.flyyy-dash" in out and rc2 == 0 and "containers.flyyy-edge" in out2 and active(ctl, EDGE_UNITS + ["flyyy-dashboard"]),
      "#dashboard and #edge --nixos on the control host: with the modules, each is a container of its own; still running",
      f"--nixos: {rc} {out}\n{rc2} {out2}")

# 4. A worker joins with #join.
rc, out = plain.execute(f"flyyy-join --user bob --name worker1 {shlex.quote(fly('token').strip())} 2>&1")
shown('#join, a fresh join', out)
check(rc == 0 and "✓ worker1 joined through 10.99.8.2" in out and "next: approve worker1 in the dashboard (or flyyy approve worker1)" in out,
      "#join: worker1 joined through 10.99.8.2, approve it next", f"#join: exit {rc}\n{out}")
check(until(lambda: nodes().get("worker1", {}).get("state") == "pending", BOOT), "#join: worker1 waits for approval", f"#join: mesh {nodes()}")
# Its host's name in /v1/mesh (the dashboard's "on <host>"); the local edge1 and the control plane: the control host's.
plain_host, ctl_host = plain.succeed("cat /proc/sys/kernel/hostname").strip(), ctl.succeed("cat /proc/sys/kernel/hostname").strip()
control_row = json.loads(fly("mesh --json"))["control"]
check(nodes()["worker1"].get("hostname") == plain_host and nodes()["edge1"].get("hostname") == ctl_host and control_row.get("hostname") == ctl_host,
      f"#join: /v1/mesh says worker1 is on {plain_host}, edge1 and the control plane on {ctl_host}", f"#join: hostnames {nodes()}, {control_row}")
# The control plane's card on the dashboard's Nodes page: the endpoint nodes dial (#control's --endpoint) and its API.
check(control_row.get("endpoint") == "10.99.8.2:51820" and control_row.get("api") == "10.250.0.1:4000",
      "/v1/mesh's control: the endpoint nodes dial (10.99.8.2:51820) and the API (10.250.0.1:4000)", f"/v1/mesh control: {control_row}")
fly("approve worker1")
check(until(worker_alive, BOOT), "#join: worker1 approved and alive (its Corrosion gossips with the control plane's)", f"#join: workers {fly('workers')}")
code = plain.succeed("curl -s -o /dev/null -w '%{http_code}' -X POST -d '{}' http://10.250.0.1:4000/v1/mesh/local").strip()
check(code == "404", "/v1/mesh/local from a mesh node: 404 (loopback on the control host only)", f"/v1/mesh/local from worker1: {code}")

# 5. An app, through the edge on the control host, from the worker's machine (its firewall opened :443).
fly(f"deploy whoami --image {image('traefik/whoami')} --port 80 --count 1 --timeout {COLD_DEPLOY}")
mid = [m["id"] for m in json.loads(fly("status whoami --json"))["machines"]]
check(until(lambda: via_edge() == ("edge1", mid[0]), PROPAGATE), f"whoami answers through edge1 on the control host ({mid[0]} on worker1)", f"through the edge: {via_edge()}")
mt = json.loads(fly("metrics whoami --json"))
check(mt["asked"]["edges"] == 1 and not mt["missing"] and mt["totals"]["requests"] >= 1,
      f"metrics: the control plane read edge1's counters at 10.250.0.1 (mTLS), {mt['totals']['requests']} requests", f"metrics: {mt['asked']} missing {mt['missing']}")
op = ctl.succeed(f"cat {OP_TOKEN}").strip()
# Its Host is the public name alone (as behind a forward from :443).
ws = plain.succeed(f"curl -sS --resolve dash.flyyy.localhost:9443:10.99.8.2 -H 'Host: dash.flyyy.localhost' -H 'authorization: Bearer {op}' https://dash.flyyy.localhost:9443/v1/workers")
check([w["name"] for w in json.loads(ws)] == ["worker1"], "the dashboard answers from the control host's local API, with the caller's token", f"dashboard: {ws}")

procs = ctl.succeed("ps -eo user:20=,comm= | grep -E ' (flyyy|proxy|corrosion)' || true").split("\n")
users = {l.split()[0] for l in procs if l.strip()}
check(users == {"carol"}, f"ctl: every flyyy, proxy and Corrosion process is carol's ({len([l for l in procs if l.strip()])})", f"ctl: processes {procs}")

# 6. Again, after the slices stop: each resumes from /var/lib/flyyy*, and the app still answers.
ctl.succeed("systemctl stop flyyy-dashboard.slice flyyy-edge.slice flyyy-control.slice")
rc, out = ctl.execute("flyyy-control --user carol 2>&1")
shown('#control, again', out)
check(rc == 0 and out.splitlines() == ["✓ control plane running again (endpoint 10.99.8.2)", f"operator token: {op_token}"],
      "#control again, without --endpoint: resumed (the endpoint kept), the same token", f"#control again: exit {rc}\n{out}")
rc, out = ctl.execute("flyyy-control --user carol --hide-token 2>&1")
check(rc == 0 and out.splitlines()[-1] == f"operator token: in {OP_TOKEN}" and op_token not in out,
      "#control --hide-token: the token's path, not the token", f"#control --hide-token: exit {rc}\n{out}")
rc, out = ctl.execute(f"flyyy-edge --user carol --listen 0.0.0.0:443 {EDGE_TLS} 2>&1")
shown('#edge, again', out)
check(rc == 0 and "resuming edge1" in out and "✓ edge1 serves apps on 0.0.0.0:443" in out, "#edge again, without a token: resumes edge1", f"#edge again: exit {rc}\n{out}")
rc, out = ctl.execute(f"flyyy-dashboard --user carol {DASH} 2>&1")
shown('#dashboard, again', out)
check(rc == 0, "#dashboard again", f"#dashboard again: exit {rc}\n{out}")
check(until(lambda: via_edge() == ("edge1", mid[0]), BOOT), f"after the restarts: whoami still answers through edge1 ({mid[0]})", f"after the restarts: {via_edge()}")
