# storm.sh: change storms through worker1's Corrosion agent, checking that every proxy (3 backhauls, 2
# edges) applied every change of its subscription in order with no gap, also across faults, and ends with
# its agent's table. Then churn: 20 create -> started -> destroy cycles with no leaks. Probes are written
# straight into Corrosion, as their writers would: service_status rows (<id>.http, passing) once, and the
# storm itself in placement (what the subscriptions take addresses from); no machine agent touches them.
# Cases: plain proxy-restart edge-restart agent-restart agent-kill churn. (storm.sh's 5-minute `lag` case
# isn't ported.)
UPSERT = ("INSERT INTO placement (id, machine, app, worker, protocol, port, edge_port) VALUES (?, ?, 'probe', ?, 'http', ?, 0) "
          "ON CONFLICT (id) DO UPDATE SET machine = excluded.machine, port = excluded.port")
PROBES = [f"{n}-p{k}" for n in NAMES3 for k in range(5)]
tx([["INSERT INTO service_status (id, machine, worker, app, service, protocol, addr, state) VALUES (?, ?, ?, 'probe', 'http', 'http', '', 'passing')",
     [f"{p}.http", p, p.split("-")[0]]] for p in PROBES])

# The storm generator: sequential single-statement transactions, ids <worker>-p{0..4} round robin,
# every third block of 15 a DELETE of its placement; each upsert's port is a fresh sequence number, so the
# order a proxy applied them in is visible in its journal. The addrs are valid (vsock:<id>:<port>), so the
# backhauls route every row rather than skipping it; nothing answers on them.
worker1.succeed("""cat > /tmp/storm.sh <<'EOF'
seq_no=$1
token=$(cat /var/lib/flyyy-corroapi/token)
for i in $(seq 0 999); do
  n=$((i % 3 + 1)); id="worker$n-p$(((i / 3) % 5))"; seq_no=$((seq_no + 1))
  if [ $(((i / 15) % 3)) = 2 ]; then
    body=$(jq -nc --arg id "$id.http" '[["DELETE FROM placement WHERE id = ?", [$id]]]')
  else
    body=$(jq -nc --arg id "$id" --arg w "worker$n" --argjson p "$((10000 + seq_no))" --arg sql "$UPSERT" '[[$sql, [($id + ".http"), $id, $w, $p]]]')
  fi
  curl -fs -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$body" http://127.0.0.1:8080/v1/transactions >/dev/null || echo "storm: tx $i failed" >&2
done
EOF""")
seq_no = 0


def storm_in_background() -> None:
    global seq_no
    worker1.succeed(f"systemd-run --unit=storm --setenv=PATH=/run/current-system/sw/bin --setenv=UPSERT={shlex.quote(UPSERT)} bash /tmp/storm.sh {seq_no}")
    seq_no += 1000


def storm_wait() -> None:
    until(lambda: worker1.execute("systemctl is-active -q storm")[0] != 0, COLD_DEPLOY, 1)
    failed = worker1.succeed("journalctl -u storm -o cat --no-pager | grep -c 'tx .* failed' || true").strip()
    check(failed == "0", "storm: 1000 transactions written", f"storm: {failed} transactions failed")
    worker1.succeed("systemctl reset-failed storm || true")


def settle() -> None:
    """Known final state: probe.flyyy.localhost = each worker's two whoami machines (placed at their vsock port)."""
    stmts = []
    for n in NAMES3:
        ids = on(n, "whoami")
        for k in [0, 1]:
            stmts.append([UPSERT, [f"{n}-p{k}.http", ids[k], n, 80]])
        stmts.append(["DELETE FROM placement WHERE worker = ? AND id IN (?, ?, ?)", [n, f"{n}-p2.http", f"{n}-p3.http", f"{n}-p4.http"]])
    for s in stmts:
        worker1.succeed(f"curl -fs {CORRO_AUTH} -H 'content-type: application/json' -d {shlex.quote(json.dumps([s]))} http://127.0.0.1:8080/v1/transactions")


# What the backhauls and edges subscribe to (backhaul.rs, edge_routes.rs).
DB_ROWS = BACKHAUL_ROWS
JOIN = EDGE_JOIN


def converged_all() -> bool:
    for n in NAMES3:
        src = q(worker1, "http://127.0.0.1:8080", DB_ROWS, [n])
        mine = q(WORKERS[n], "http://127.0.0.1:8080", DB_ROWS, [n])
        px = sorted(json.loads(WORKERS[n].succeed("curl -fs http://127.0.0.1:9080/state"))["rows"])
        if not (src and src == mine == px):
            return False
    all_rows = q(worker1, "http://127.0.0.1:8080", JOIN)
    for e in [1, 2]:
        px = sorted(json.loads(edges.succeed(f"curl -fs http://127.0.0.1:910{e}/state"))["rows"])
        if not (all_rows and all_rows == q(edges, f"http://127.0.0.1:810{e}", JOIN) == px):
            return False
    return True


def converge() -> None:
    t0 = time.time()
    check(until(converged_all, DEPLOY, 1), f"converged in {time.time() - t0:.0f} s: all proxies == all agents", f"no convergence after {DEPLOY} s")


def audit(m, unit: str, since: float) -> None:
    """Change ids contiguous within each snapshot; probe upserts in the writer's order (rising ports)."""
    changes = snaps = resyncs = gaps = reorders = 0
    last = -1
    prev: dict[str, int] = {}
    for l in journal(m, unit, since).splitlines():
        if l.startswith("snapshot "):
            snaps += 1
            c = re.search(r"change_id=Some\((\d+)\)", l)
            last = int(c.group(1)) if c else -1
        elif c := re.match(r"change change_id=(\d+) kind=(\w+) rowid=\d+ row=(.*)$", l):
            changes += 1
            cid, kind, row = int(c.group(1)), c.group(2), c.group(3).split()
            if last >= 0 and cid != last + 1:
                gaps += 1
                print(f"  gap in {m.name} {unit}: {last} -> {cid}")
            last = cid
            # A backhaul's probe row: `<id> probe vsock:<id>:<port>`.
            if kind != "Delete" and len(row) == 3 and row[1] == "probe":
                port = int(row[2].rsplit(":", 1)[1]) if ":" in row[2] else 0
                key = row[0]
                if port >= 10000:
                    if port <= prev.get(key, 0):
                        reorders += 1
                    prev[key] = port
        elif l.startswith("resync "):
            resyncs += 1
    summary = f"changes={changes} snapshots={snaps} resyncs={resyncs} gaps={gaps} reorders={reorders}"
    check(gaps == 0 and reorders == 0, f"audit {m.name} {unit}: {summary}", f"audit {m.name} {unit}: {summary}")


def run_case(name: str, fault) -> None:
    print(f"== {name}")
    since = time.time()
    storm_in_background()
    time.sleep(4)
    fault()
    storm_wait()
    settle()
    converge()
    for n in NAMES3:
        audit(WORKERS[n], "proxy", since)
    for e in [1, 2]:
        audit(edges, f"edge{e}", since)


run_case("plain", lambda: None)
run_case("proxy-restart", lambda: worker2.succeed("systemctl restart proxy"))
run_case("edge-restart", lambda: edges.succeed("systemctl restart edge1"))
run_case("agent-restart", lambda: worker3.succeed("systemctl restart corrosion"))
run_case("agent-kill", lambda: kill9(worker2, "corrosion"))

# probe.flyyy.localhost routes like the final state says: through the edges to each worker's whoami machines.
want = sorted(mid for n in NAMES3 for mid in on(n, "whoami"))
# 96 requests: the edges pick among idle instances at random, so 24 could miss one of six (7%; 96: 1e-7).
got = sorted({id_of(edges.succeed(f"curl -fs --max-time {REQUEST} https://probe.flyyy.localhost/")) for _ in range(96)})
check(got == want, f"probe via edges -> {' '.join(got)}", f"probe via edges -> {got}, want {want}")

# Churn: create -> started -> destroy, 20 times round robin over the workers, while sampling whoami
# through the edges (it always has its 6 machines, so every sample must be 200); then nothing leaks.
print("== churn")
WHOAMI = image("traefik/whoami")
edges.succeed(f"systemd-run --unit=sampler --setenv=PATH=/run/current-system/sw/bin sh -c 'while :; do curl -s -o /dev/null -w \"%{{http_code}}\\n\" --max-time {SLOW_REQUEST} https://whoami.flyyy.localhost/ >> /tmp/samples; done'")
creates, destroys = [], []
for i in range(20):
    mid = machine_create(NAMES3[i % 3], "whoami", WHOAMI, 80)
    t = wait_fact(mid, "started", BOOT)
    creates.append(t) if t is not None else bad(f"churn: {mid} never started")
    machine_set(mid, "destroyed")
    t = wait_fact(mid, "destroyed", BOOT)
    destroys.append(t) if t is not None else bad(f"churn: {mid} never destroyed")
edges.succeed("systemctl stop sampler")
smp = edges.succeed("cat /tmp/samples").split()
check(smp and all(c == "200" for c in smp), f"churn: {len(smp)} whoami samples during churn, all 200", f"churn: samples {sorted(set(smp))}")
stats = lambda xs: f"avg {sum(xs) / len(xs) * 1000:.0f} ms, max {max(xs) * 1000:.0f}" if xs else "none"
check(len(creates) == 20, f"churn: create -> started x20: {stats(creates)}", "churn: creates missing")
check(len(destroys) == 20, f"churn: destroy -> destroyed x20: {stats(destroys)}", "churn: destroys missing")
leaks = []
for n in NAMES3:
    keep = set(on(n, "whoami") + on(n, "echo"))
    files = WORKERS[n].succeed(f"ls {RUN} {STATE}; {AS_FLYYY} systemctl --user list-units --all --plain --no-legend 'fc-*' | awk '{{print $1}}'").split()
    for f in files:
        base = re.sub(r"^fc-|\.(service|json|vsock|scratch)$", "", f)
        # desired/: what the control plane told the agent, destroyed machines' records included (not a leak).
        if f.endswith(":") or f in ("images", "reports", "desired") or base in keep:
            continue
        leaks.append(f"{n}:{f}")
check(not leaks, "churn: no leaked units, configs, vsock sockets, state files or scratch drives", f"churn leaks: {leaks}")
