# workers.sh: workers announce themselves (`workers` table, heartbeat). No static worker lists in the
# edge/control units, heartbeats don't churn routing, rows that arrive late or are bad, worker4 joining at
# runtime, agent down -> dead (no new machines, old ones still serve), and the container leaving and
# rejoining.


def wk(name: str) -> dict:
    return next((w for w in json.loads(fly("workers --json")) if w["name"] == name), {})


def alive(name: str) -> bool:
    return wk(name).get("alive") is True


def routes(e: int) -> list[str]:
    st = json.loads(edges.succeed(f"curl -fs http://127.0.0.1:910{e}/state"))
    # Route ids are <machine>.<service>: the machines.
    return [u.split(" ")[0].split(".")[0] for us in (st.get("routes") or {}).values() for u in us]


def pids() -> str:
    return " ".join(f"{u}={m.succeed(f'systemctl show -P MainPID {u}').strip()}" for m, u in [(edges, "edge1"), (edges, "edge2"), (control, "control")])


# 1. No static worker lists in the edge and control plane units.
for m, u in [(edges, "edge1"), (edges, "edge2"), (control, "control")]:
    cmd = re.search(r"argv\[\]=([^;]*)", m.succeed(f"systemctl show -P ExecStart {u}"))
    argv = cmd.group(1) if cmd else ""
    # A wrapper script (it waits for the mesh certificate): its text is the command line.
    prog = argv.split()[0] if argv else ""
    if prog and m.execute(f"head -c2 {prog} | grep -q '#!'")[0] == 0:
        argv = m.succeed(f"cat {prog}")
    check(argv and not re.search(r"worker|4280", argv), f"{u}: ExecStart has no worker list", f"{u}: ExecStart {argv}")

# 2. Announce: worker1-3 alive with their backhauls; an agent restart bumps started_at.
for n in NAMES3:
    r = wk(n)
    check(r.get("alive") and r.get("backhaul") == f"{MESH[n]}:4280" and r.get("heartbeat_age_ms", 99999) < 6000,
          f"{n} announced: backhaul {r.get('backhaul')}, heartbeat {r.get('heartbeat_age_ms')} ms ago", f"{n}: {r}")
up0 = wk("worker2")["uptime_ms"]
worker2.succeed("systemctl restart agent")
time.sleep(2)
up1 = wk("worker2")["uptime_ms"]
check(up1 < 5000 and up1 < up0, f"agent restart on worker2: uptime {up0} -> {up1} ms (started_at bumped)", f"agent restart: uptime {up0} -> {up1}")

# Capacity in the heartbeat (arch, CPUs, memory MiB) and allocation: the live machines' sizes summed.
for n in NAMES3:
    r = wk(n)
    arch = WORKERS[n].succeed("uname -m").strip()
    cpus = int(WORKERS[n].succeed("nproc").strip())
    mem = int(WORKERS[n].succeed("awk '/^MemTotal:/ {print int($2 / 1024)}' /proc/meminfo").strip())
    check((r.get("arch"), r.get("cpus"), r.get("memory")) == (arch, cpus, mem), f"{n} reports {arch}, {cpus} CPUs, {mem} MiB",
          f"{n} capacity {r.get('arch')} {r.get('cpus')} {r.get('memory')}, want {arch} {cpus} {mem}")
    # The seed apps' machines are the default size, 1 CPU and 128 MiB.
    check((r["allocated_cpus"], r["allocated_memory"]) == (r["machines"], 128 * r["machines"]),
          f"{n} allocated {r['allocated_cpus']} CPUs, {r['allocated_memory']} MiB for its {r['machines']} machines", f"{n}: {r}")
# Overcommit is allowed: two stopped 8 CPU / 16 GiB machines (no VMs) take worker1 past its capacity. The
# worker reports its allocation with its heartbeat (every 5 s).
r0 = wk("worker1")
for mid in ["m-big1", "m-big2"]:
    machine_create("worker1", "big", image("traefik/whoami"), 80, mid=mid, cpus=8, memory=16384, start=False)
over = lambda r: r["allocated_cpus"] == r0["allocated_cpus"] + 16 and r["allocated_memory"] == r0["allocated_memory"] + 32768 and r["allocated_cpus"] > r["cpus"] and r["allocated_memory"] > r["memory"]
until(lambda: over(wk("worker1")), PROPAGATE)
r = wk("worker1")
check(over(r), f"overcommit: worker1 allocated {r['allocated_cpus']}/{r['cpus']} CPUs, {r['allocated_memory']}/{r['memory']} MiB", f"overcommit: {r0} -> {r}")
line = next((l for l in fly("workers").splitlines() if l.startswith("worker1 ")), "")
want = [f"{r['allocated_cpus']}/{r['cpus']}", f"{r['allocated_memory']}/{r['memory']}", r["arch"]]
check(all(w in line.split() for w in want), f"flyyy workers shows {' '.join(want)} for worker1", f"flyyy workers: {line!r}, want {want}")
for mid in ["m-big1", "m-big2"]:
    machine_set(mid, "destroyed")
back = lambda: (wk("worker1")["allocated_cpus"], wk("worker1")["allocated_memory"]) == (r0["allocated_cpus"], r0["allocated_memory"])
check(until(back, PROPAGATE), "overcommit: destroying them gives the allocation back", f"overcommit: {r0} -> {wk('worker1')}")

# 7. Heartbeats don't churn routing: 60 s with no machine changes, 0 subscription changes on the edges.
# First every live machine's check passes (the restarted agent's, too), so nothing is still settling.
settled = lambda: {f"{m}.http" for a in ["whoami", "echo"] for _, m in live(a)} <= {i for (i,) in q(control, "http://127.0.0.1:8080", "SELECT id FROM service_status WHERE state = 'passing'")}
check(until(settled, ACT), "health: every live machine passing before the quiet window", "health: not all live machines passing")
# ...and the edges have it (by gossip, a little after the control plane).
check(until(lambda: edges_have({f"{m}.http" for a in ["whoami", "echo"] for _, m in live(a)}), ACT), "health: both edges route every live machine", "health: edges behind")
since = time.time()
# The subscription's last change id, from each edge's /state: the same after 60 s of heartbeats.
before = {e: edge_state(e)["last_change_id"] for e in [1, 2]}
time.sleep(60)
for e in [1, 2]:
    after = edge_state(e)["last_change_id"]
    check(after == before[e], f"edge{e}: 0 subscription changes in 60 s of heartbeats", f"edge{e}: change id {before[e]} -> {after} in 60 s of heartbeats")

# A worker row arriving after its machine's fact; bad rows. Written by this test (fake workers 7-9 only).
SVC = "INSERT INTO service_status (id, machine, worker, app, service, protocol, addr, state) VALUES (?, ?, ?, ?, 'http', 'http', ?, 'passing')"
PLACED = "INSERT INTO placement (id, machine, app, worker, protocol, port, edge_port) VALUES (?, ?, ?, ?, 'http', 80, 0)"
def backhaul_of(worker: str) -> list:
    return q(control, "http://127.0.0.1:8080", "SELECT backhaul FROM backhauls WHERE worker = ?", [worker])
# A worker the mesh doesn't know: its rows say it's worker9 with a mesh backhaul, even placed; the control
# plane binds it no backhaul, so the edges never route it.
tx([[SVC, ["worker9-late.http", "worker9-late", "worker9", "late", "vsock:worker9-late:80"]], [PLACED, ["worker9-late.http", "worker9-late", "late", "worker9"]]])
now = int(time.time() * 1000)
tx([["INSERT INTO workers (name, backhaul, started_at, heartbeat_at) VALUES ('worker9', '10.250.9.9:4280', ?, ?) ON CONFLICT (name) DO NOTHING", [now, now]]])
check(until(lambda: backhaul_of("worker9") == [[""]], PROPAGATE), "late row: the control plane binds worker9 no backhaul (not a worker of the mesh)", f"late row: worker9's backhaul {backhaul_of('worker9')}")
time.sleep(2)
check(all("worker9-late" not in routes(e) for e in [1, 2]), "late row: so the edges don't route it", "late row: routed")
# worker7's backhaul parses but is off the mesh (a LAN address), worker6's is loopback (fine only in
# single-host mode, which no node here runs in), worker8's doesn't parse, 9evil isn't a node name (a digit first): none is
# bound.
tx([["INSERT INTO workers (name, backhaul, started_at, heartbeat_at) VALUES ('worker8', 'nope', ?, ?), ('9evil', '10.250.7.7:4280', ?, ?), ('worker7', '10.99.7.2:4280', ?, ?), "
     "('worker6', '127.0.0.1:4280', ?, ?) ON CONFLICT (name) DO UPDATE SET heartbeat_at = excluded.heartbeat_at", [now] * 8]]
   + [[SVC, [f"{w}-bad.http", f"{w}-bad", w, "bad", f"vsock:{w}-bad:80"]] for w in ["worker8", "9evil", "worker7", "worker6"]]
   + [[PLACED, [f"{w}-bad.http", f"{w}-bad", "bad", w]] for w in ["worker8", "9evil", "worker7", "worker6"]])
check(until(lambda: all(backhaul_of(w) == [[""]] for w in ["worker8", "9evil", "worker7", "worker6"]), PROPAGATE),
      "bad rows: the control plane binds no backhaul for an invalid one, one off the mesh, a loopback one or a bad worker name", "bad rows: a backhaul was bound")
# Even a backhauls row as if from the control plane: the edges check it themselves (loopback, off the mesh).
tx([["INSERT INTO backhauls (worker, backhaul) VALUES ('worker6', '127.0.0.1:4280') ON CONFLICT (worker) DO UPDATE SET backhaul = excluded.backhaul", []]])
check(until(lambda: edges_have({"worker6-bad.http"}), ACT), "bad rows: both edges have worker6's (with a planted loopback backhaul)", "bad rows: edges never got it")
check(all(m not in routes(e) for e in [1, 2] for m in ["worker8-bad", "9evil-bad", "worker7-bad", "worker6-bad"]), "bad rows: none routed", f"bad rows routed: {routes(1)}")
# The planted binding can trail the row itself: until it comes, edge1 skips the row for the empty one.
skipped_why = lambda: edge_state(1)["skipped"].get("worker6-bad.http", "")
check(until(lambda: "not on the mesh" in skipped_why(), PROPAGATE), f"bad rows: edge1 skips the planted loopback backhaul, in its /state: {skipped_why()}",
      f"bad rows: edge1 skipped {edge_state(1)['skipped']}")
check(all([wk(w).get(k) for k in ["valid", "alive"]] == [False, False] for w in ["worker8", "9evil", "worker7", "worker6"]),
      "bad rows: control plane shows them invalid, not alive", f"bad rows: {wk('worker8')} {wk('9evil')} {wk('worker7')} {wk('worker6')}")
check(not any(w["single_host"] for w in json.loads(fly("workers --json"))) and "single-host" not in fly("workers"),
      "single-host: off in the cluster (flyyy workers says nothing of it)", "single-host: shown in the cluster")

# 3. Join: worker4 starts, announces itself, gets the new machines, and is routed with no restarts.
p0 = pids()
t0 = time.time()
worker4.start()
# Into the mesh first (a token, `flyyy join`, approve): its Corrosion and backhaul run on it.
mesh_join(worker4, "worker4")
check(until(lambda: alive("worker4"), BOOT), f"join: worker4 alive {time.time() - t0:.1f} s after start (mesh join included)", "join: worker4 not alive")
fly(f"scale whoami --count 8 --timeout {DEPLOY}")
new = on("worker4", "whoami")
check(len(new) == 2, f"join: scale 6 -> 8 put both new machines on worker4 ({' '.join(new)})", f"join: worker4 has {new}")
want = sorted(mid for _, mid in live("whoami"))
def seen_all() -> bool:
    got = [via_edge("whoami.flyyy.localhost") for _ in range(48)]
    return sorted({m for _, m in got}) == want and len({e for e, _ in got}) == 2
check(until(seen_all, PROPAGATE), "join: both edges spread whoami over all 8", "join: edges don't cover all 8")
check(pids() == p0, f"join: edges and control plane not restarted ({p0})", f"join: PIDs {p0} -> {pids()}")
other = on("worker1", "whoami")[0]
check(bh_code("worker4", other) == "404", f"join: worker4 backhaul refuses worker1's {other} (404)", f"join: worker4 CONNECT {other} -> {bh_code("worker4", other)}")

# 4. Liveness: worker4's agent stops -> dead -> no new machines there; its machines still serve.
t0 = time.time()
worker4.succeed("systemctl stop agent")
check(until(lambda: wk("worker4").get("alive") is False, BOOT), f"liveness: worker4 dead {time.time() - t0:.1f} s after its agent stopped", "liveness: worker4 still alive")
fly(f"scale whoami --count 10 --timeout {DEPLOY}")
check(len(on("worker4", "whoami")) == 2, "liveness: scale 8 -> 10 placed nothing on dead worker4", f"liveness: worker4 has {on('worker4', 'whoami')}")
id4 = on("worker4", "whoami")[0]
check(id_of(bh("worker4", id4)) == id4, f"liveness: {id4} still serves through worker4's backhaul with its agent down", "liveness: worker4's machine doesn't serve")
start(worker4, "agent")
check(until(lambda: alive("worker4"), PROPAGATE), "liveness: worker4 alive again after its agent started", "liveness: worker4 not alive again")
fly(f"scale whoami --count 12 --timeout {DEPLOY}")
check(len(on("worker4", "whoami")) >= 3, f"liveness: scale 10 -> 12 uses worker4 again ({len(on('worker4', 'whoami'))} there)", f"liveness: worker4 has {on('worker4', 'whoami')}")

# 5. Leave and rejoin. Scale-down ties go to the busiest worker overall, and worker4 runs no echo, so it
# keeps some whoami machines (scale-down doesn't move machines between workers).
fly(f"scale whoami --count 6 --timeout {DEPLOY}")
left4 = on("worker4", "whoami")
note(f"leave: after scale 12 -> 6, worker4 keeps {len(left4)} whoami machines: {' '.join(left4)}")
t0 = time.time()
worker4.shutdown()
check(until(lambda: wk("worker4").get("alive") is False, BOOT) and wk("worker4"), f"leave: worker4 stopped -> dead {time.time() - t0:.1f} s later, its row stays", f"leave: {wk('worker4')}")
codes = [code("whoami.flyyy.localhost", 10) for _ in range(24)]
check(all(c == "200" for c in codes), "leave: all 24 whoami requests 200 (edges fall back past worker4's stale machines)", f"leave: {codes}")
t0 = time.time()
worker4.start()
check(until(lambda: alive("worker4"), BOOT), f"rejoin: worker4 alive again {time.time() - t0:.1f} s after start, same name", "rejoin: worker4 not alive")
check(all(wait_fact(m, "started", BOOT) is not None for m in left4), f"rejoin: its machines started again ({' '.join(left4)})", "rejoin: machines not started again")
tx([["DELETE FROM service_status WHERE id IN ('worker9-late.http', 'worker8-bad.http', '9evil-bad.http', 'worker7-bad.http', 'worker6-bad.http')", []],
    ["DELETE FROM placement WHERE id IN ('worker9-late.http', 'worker8-bad.http', '9evil-bad.http', 'worker7-bad.http', 'worker6-bad.http')", []]])
