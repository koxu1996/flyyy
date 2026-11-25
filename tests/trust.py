# trust: Corrosion carries facts only, and the edges route only as the control plane placed and bound.
# 0. Every node's Corrosion API wants that node's token, which no other local user can read.
# 1. The agent API takes the control plane's certificate only. 2. Env values never reach gossip.
# 3. A compromised worker (worker1) writing rows for another worker's machines, for machines nobody placed,
# and announcing a fake backhaul changes no routing. 4. With the control plane stopped, machines keep
# running and serving, through an agent restart and a VM's death too.

T = "/var/lib/flyyy-tls"

# 0. Reads and writes without the token or with a wrong one: 401. The token file is root's, readable by
# the corroapi group only (not by flyyy, whose user manager runs the VMs); Corrosion's config, which
# holds it, and its admin socket are in a directory only Corrosion can enter.
QUERY = shlex.quote(json.dumps(["SELECT count(*) FROM workers", []]))
WRITE = shlex.quote(json.dumps([["DELETE FROM workers WHERE name = 'nope'", []]]))
WRONG = "-H 'authorization: Bearer " + "0" * 64 + "'"


def corro_code(m, port: int, path: str, body: str, auth: str) -> str:
    return m.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' {auth} -H 'content-type: application/json' -d {body} http://127.0.0.1:{port}{path}").strip()


for m, port, rundir in [(worker1, 8080, "corrosion"), (control, 8080, "corrosion"), (edges, 8101, "corrosion-edge1")]:
    got = {k: (corro_code(m, port, "/v1/queries", QUERY, a), corro_code(m, port, "/v1/transactions", WRITE, a)) for k, a in [("none", ""), ("wrong", WRONG), ("right", CORRO_AUTH)]}
    check(got == {"none": ("401", "401"), "wrong": ("401", "401"), "right": ("200", "200")},
          f"corrosion api: {m.name}'s reads and writes get 401 without its token or with a wrong one, 200 with it", f"corrosion api: {m.name} {got}")
    mode = m.succeed(f"stat -c '%a %U:%G' {CORRO_TOKEN}").strip()
    others = [u for u in ["nobody", "flyyy"] if m.execute(f"id -u {u}")[0] == 0]
    tries = [f"cat {CORRO_TOKEN}", f"cat /run/{rundir}/config.toml", f"ls /run/{rundir}"]
    leaks = [f"{u}: {t}" for u in others for t in tries if m.execute(f"runuser -u {u} -- {t} >/dev/null 2>&1")[0] == 0]
    check(m.succeed(f"grep -c bearer-token /run/{rundir}/config.toml").strip() == "1", f"corrosion api: {m.name}'s Corrosion runs with the token in its config",
          f"corrosion api: {m.name}'s config has no token")
    check(mode == "640 root:corroapi" and not leaks, f"corrosion api: {m.name}'s token is 640 root:corroapi; {', '.join(others)} can't read it, the config or the admin socket",
          f"corrosion api: {m.name} token {mode}, readable: {leaks}")


def sampler(vhost: str) -> None:
    edges.succeed(f"systemd-run --unit=sampler --setenv=PATH=/run/current-system/sw/bin sh -c 'while :; do curl -s -o /dev/null -w \"%{{http_code}}\\n\" --max-time {SLOW_REQUEST} https://{vhost}/ >> /tmp/samples; done'")


def samples() -> list[str]:
    edges.succeed("systemctl stop sampler")
    return edges.succeed("cat /tmp/samples; rm /tmp/samples").split()


def routes_now() -> list:
    return [edge_state(e)["routes"] for e in [1, 2]]


def served(vhost: str, n: int = 24) -> set[str]:
    """Machine ids answering n requests through the edges; empty if any request failed."""
    got = [via_edge(vhost, 10)[1] for _ in range(n)]
    return set(got) if all(got) else set()


whoami = sorted(mid for _, mid in live("whoami"))
check(until(lambda: served("whoami.flyyy.localhost", 48) == set(whoami), ACT), f"setup: whoami served by its {len(whoami)} machines", "setup: whoami not served by all")

# 1. The agent API: the control plane's certificate only.
rc, out = agent("worker2", "GET", "/v1/machines")
check(rc == 200 and set(on("worker2", "whoami")) <= set(json.loads(out)), "agent API: the control plane lists worker2's machines", f"agent API: {rc} {out[:200]}")
rc, _ = agent("worker2", "GET", "/v1/machines", cert=False)
check(rc == 0, "agent API: no client certificate -> no handshake", f"agent API without a certificate: {rc}")
ip2 = MESH["worker2"]
for m, who in [(worker1, "worker1"), (edges, "an edge")]:
    out = m.execute(f"curl -s --max-time {SLOW_REQUEST} -w '%{{http_code}}' --cacert {T}/ca.pem --cert {T}/cert.pem --key {T}/key.pem https://{ip2}:{AGENT_PORT}/v1/machines")[1]
    check(not out.endswith("200"), f"agent API: {who}'s own mesh certificate is refused", f"agent API: {who} got {out[:200]}")
check(re.search(r"want role control", journal(worker2, "agent")), "agent API: refusals logged (want role control)", "agent API: no refusal logged")

# 2. Env values travel in the agent API only: not in any node's Corrosion, not in the API's answers.
secret = "s3cret-" + control.succeed("od -An -N8 -tx1 /dev/urandom | tr -d ' \\n'")
fly(f"deploy envy --image {image('traefik/whoami')} --port 80 --count 1 --timeout {DEPLOY} --env TOKEN={secret}")
envy = status("envy")["machines"][0]
dbs = {n: m for n, m in [("worker1", worker1), ("worker2", worker2), ("worker3", worker3), ("control", control)]}
leaked = [n for n, m in dbs.items() if m.execute(f"grep -rqa {secret} /var/lib/corrosion")[0] == 0]
leaked += [f"edge{e}" for e in [1, 2] if edges.execute(f"grep -rqa {secret} /var/lib/corrosion-edge{e}")[0] == 0]
check(not leaked, "env: the value is in no node's Corrosion (control plane, workers, edges)", f"env: found in {leaked}")
check(secret not in fly("status envy --json") and status("envy")["env"] == ["TOKEN"], "env: the API shows the name only", "env: the API shows the value")
w = dbs[envy["worker"]]
check(w.execute(f"grep -rqa {secret} {STATE}/desired")[0] == 0, f"env: {envy['worker']} holds it in what the control plane told it (desired/)", "env: not on its worker")
fly(f"destroy envy --timeout {DEPLOY}")

# 3. worker1 compromised: everything it can write to Corrosion, written straight into its agent.
before = routes_now()
m2 = on("worker2", "whoami")[0]
m1 = on("worker1", "whoami")[0]
FORGE = ("INSERT INTO service_status (id, machine, worker, app, service, protocol, addr, edge_port, state) "
         "VALUES (?, ?, 'worker1', ?, 'http', ?, ?, ?, 'passing') ON CONFLICT (id, worker) DO UPDATE SET app = excluded.app, protocol = excluded.protocol, "
         "addr = excluded.addr, edge_port = excluded.edge_port, state = 'passing'")
def w1(stmts: list) -> None:
    worker1.succeed(f"curl -fs {CORRO_AUTH} -H 'content-type: application/json' -d {shlex.quote(json.dumps(stmts))} http://127.0.0.1:8080/v1/transactions")
# Its agent stopped, so nothing of worker1's writes over the forgeries (its machines keep serving).
worker1.succeed("systemctl stop agent")
w1([
    # worker2's machine claimed as worker1's, passing: an instance placed on worker2.
    [FORGE, [f"{m2}.http", m2, "whoami", "http", f"vsock:{m2}:80", 0]],
    # A machine nobody placed, under another app's name and as tcp on that app's own address.
    [FORGE, ["m-ghost.http", "m-ghost", "echo", "tcp", "vsock:m-ghost:80", 5432]],
    # Its own machine relabelled: another app, tcp.
    [FORGE, [f"{m1}.http", m1, "echo", "tcp", f"vsock:{m1}:80", 5432]],
    # A fake backhaul: another worker's address, and one off the mesh.
    ["UPDATE workers SET backhaul = ?, heartbeat_at = ? WHERE name = 'worker1'", [f"{MESH['worker2']}:4280", int(time.time() * 1000)]],
])
time.sleep(8)
w1([["UPDATE workers SET backhaul = '10.66.6.6:4280', heartbeat_at = ? WHERE name = 'worker1'", [int(time.time() * 1000)]]])
time.sleep(8)
bound = q(control, "http://127.0.0.1:8080", "SELECT backhaul FROM backhauls WHERE worker = 'worker1'")
check(bound == [[f"{MESH['worker1']}:4280"]], f"forgery: worker1's backhaul stays bound to its mesh address ({bound[0][0] if bound else '-'})", f"forgery: worker1's backhaul {bound}")
check(re.search(r"agent worker=worker1 announces error=.* not on its mesh address", journal(control, "control")), "forgery: the control plane logs the announced address it ignored", "forgery: not logged")
check(routes_now() == before, "forgery: both edges' routes unchanged (no instance, app, port or backhaul moved)", f"forgery: routes {before} -> {routes_now()}")
check(served("whoami.flyyy.localhost", 48) == set(whoami), "forgery: whoami still served by exactly its machines", "forgery: whoami served differently")
check(code("echo.flyyy.localhost") == "200" and served("echo.flyyy.localhost") == {m for _, m in live("echo")}, "forgery: echo unaffected", "forgery: echo affected")
rc, out = agent("worker2", "GET", "/v1/machines")
check(rc == 200 and f"{m2}" in json.loads(out), "forgery: worker2 still holds its machine", f"forgery: worker2 {rc}")
st = status("whoami")
check(st["converged"] and all(m["state"] == "started" for m in st["machines"]), "forgery: the control plane's view of whoami unchanged (converged)", f"forgery: whoami {st}")
w1([["DELETE FROM service_status WHERE worker = 'worker1' AND id IN (?, 'm-ghost.http')", [f"{m2}.http"]],
    ["UPDATE service_status SET protocol = 'http', app = 'whoami', edge_port = 0 WHERE id = ? AND worker = 'worker1'", [f"{m1}.http"]]])
start(worker1, "agent")

# 4. The control plane stopped: machines keep running and serving; an agent restarts from what it was told,
# and a VM that dies is started again, with nobody to ask.
# Everything the checks need from the control plane's API, read before it stops.
m3 = on("worker3", "whoami")[0]
control.succeed("systemctl stop control")
sampler("whoami.flyyy.localhost")
time.sleep(2)
worker3.succeed("systemctl restart agent")
time.sleep(5)
smp = samples()
check(smp and all(c == "200" for c in smp), f"control plane down: {len(smp)} whoami requests through an agent restart, all 200", f"control plane down: {sorted(set(smp))}")
# A VM that dies is started again from what the agent was told, with nobody to ask.
# shortcut: requests routed to it in the second before its health check fails get a 502 (the edges don't
# retry once the backhaul's CONNECT succeeded); that window isn't new here, any crash has it.
pid = vm_pid(worker3, m3)
worker3.succeed(f"kill -9 {pid}")
again = lambda: vm_pid(worker3, m3) not in ("", "0", pid) and fact(m3)[0] == "started"
check(until(again, BOOT), f"control plane down: {m3}'s VM killed -> started again by worker3's agent", f"control plane down: {m3} -> {fact(m3)}, pid {vm_pid(worker3, m3)}")
check(until(lambda: served("whoami.flyyy.localhost", 48) == set(whoami), ACT), "control plane down: whoami served by all its machines again", "control plane down: not all served")
control.succeed("systemctl start control")
control.wait_until_succeeds(f"{FLY} ls", timeout=ACT)
check(until(lambda: status("whoami")["converged"], BOOT), "control plane back: whoami converged, nothing redone", "control plane back: whoami not converged")
