# lifecycle.sh: unmanaged machines (below the control plane) next to the seeded apps: create, stop/start
# (same addr, fresh scratch), orphan cleanup, destroy (nothing left, image still cached), agent kill -9
# (VMs untouched), failures (an image that can't be pulled, an invalid row), honest readiness.
W3 = worker3
ECHO = image("hashicorp/http-echo")
WHOAMI = image("traefik/whoami")


def owned(m, mid: str) -> list[str]:
    """What a machine has on a worker: unit, config, vsock socket, state file, scratch drive."""
    out = []
    if uctl(m, f"is-active -q fc-{mid}.service")[0] == 0:
        out.append("unit")
    out += m.succeed(f"ls {RUN}/{mid}.json {RUN}/{mid}.vsock {STATE}/{mid}.json {STATE}/{mid}.scratch 2>/dev/null || true").split()
    return out


def routes(e: int) -> list[str]:
    st = json.loads(edges.succeed(f"curl -fs http://127.0.0.1:910{e}/state"))
    # Route ids are <machine>.<service>: the machines.
    return [u.split(" ")[0].split(".")[0] for us in (st.get("routes") or {}).values() for u in us]


def in_edges(mid: str) -> bool:
    return all(mid in routes(e) for e in [1, 2])


def scratch_ls(mid: str) -> list[str]:
    out = W3.succeed(f"debugfs -R 'ls /' {STATE}/{mid}.scratch 2>/dev/null || true")
    return sorted(set(out.split()) & {"upper", "work", "lost+found"})


def seen(mid: str) -> bool:
    return any(edges.execute(f"curl -fs --max-time {REQUEST} https://echo.flyyy.localhost/")[1].strip() == mid for _ in range(12))


images0 = W3.succeed(f"ls {STATE}/images/ | grep -c '\\.erofs$'").strip()
started0 = sum(a["started"] for a in json.loads(fly("ls --json")) if a["state"] == "active")

# 2. Create: started, then routable through the edges and its backhaul.
t0 = time.time()
mid = machine_create("worker3", "echo", ECHO, 5678, ["-text={id}"])
t = wait_fact(mid, "started", BOOT)
check(t is not None, f"create {mid} on worker3 -> started in {t:.1f} s" if t else "", f"create {mid} -> {fact(mid)}")
addr = fact(mid)[1]
check(until(lambda: seen(mid), ACT), f"create: {mid} ({addr}) answers via the edges, {time.time() - t0:.1f} s after the insert", f"create: {mid} never answered via the edges")
check(bh_code("worker3", mid) == "200", f"create: backhaul worker3 CONNECT {mid} -> 200", f"create: backhaul -> {bh_code("worker3", mid)}")

# 3. Stop / start: same addr, but a fresh scratch drive every boot.
ino0 = W3.succeed(f"stat -c %i {STATE}/{mid}.scratch").strip()
check(scratch_ls(mid) == ["lost+found", "upper", "work"], f"overlay: running {mid}'s scratch has upper/ and work/", f"overlay: scratch has {scratch_ls(mid)}")
t0 = time.time()
machine_set(mid, "stopped")
check(wait_fact(mid, "stopped", ACT) is not None and until(lambda: all(mid not in routes(e) for e in [1, 2]), PROPAGATE),
      f"stop: {mid} stopped and out of both edges in {time.time() - t0:.1f} s", f"stop: {fact(mid)}")
check(until(lambda: bh_code("worker3", mid) == "404", REQUEST), f"stop: backhaul CONNECT {mid} -> 404", f"stop: backhaul -> {bh_code("worker3", mid)}")
check(uctl(W3, f"is-active -q fc-{mid}.service")[0] != 0, "stop: unit inactive", "stop: unit still active")
check(W3.execute(f"test -e {STATE}/{mid}.scratch")[0] != 0, "stop: scratch drive removed", "stop: scratch still there")
machine_set(mid, "started")
t = wait_fact(mid, "started", BOOT)
check(t is not None and fact(mid)[1] == addr, f"start: {mid} started again, same addr {addr}", f"start: {fact(mid)}, want {addr}")
check(until(lambda: seen(mid), ACT), f"start: {mid} answers via the edges again", "start: not routed again")
ino1 = W3.succeed(f"stat -c %i {STATE}/{mid}.scratch").strip()
check(ino1 != ino0 and scratch_ls(mid) == ["lost+found", "upper", "work"], f"start: a new scratch drive (inode {ino0} -> {ino1}) with a fresh upper/", f"start: inode {ino0} -> {ino1}, {scratch_ls(mid)}")
machine_set(mid, "stopped")
wait_fact(mid, "stopped", ACT)

# 6. Orphan: an fc- unit nobody asked for, booted by hand (as flyyy, like the agent) from the stopped
# machine's config, with a scratch drive (and launcher config) of its own.
W3.succeed(f"{AS_FLYYY} bash -c " + shlex.quote(f"""set -e
  seed=$(mktemp -d); mkdir -p $seed/flyyy
  jq '.config | .hostname = "zz" | .machine_id = "zz"' {STATE}/{mid}.json > $seed/flyyy/config.json
  truncate -s 64M {STATE}/zz.scratch && mke2fs -q -F -t ext4 -d $seed {STATE}/zz.scratch; rm -rf $seed
  jq --arg s {STATE}/zz.scratch 'map(sub("socket=.*"; "socket={RUN}/zz.vsock") | sub("^path=[^,]*[.]scratch,"; "path=\\($s),")
    | sub("FLYYY_MACHINE_ID=[^ ]*"; "FLYYY_MACHINE_ID=zz"))' {RUN}/{mid}.json > {RUN}/zz.json
  jq -r '.[]' {RUN}/zz.json | xargs -d '\\n' systemd-run --user --unit fc-zz.service -p UMask=0007 -- cloud-hypervisor"""))
t0 = time.time()
check(until(lambda: not owned(W3, "zz"), ACT), f"orphan: fc-zz torn down by the agent in {time.time() - t0:.1f} s", f"orphan: zz still has {owned(W3, 'zz')}")

# 4. Destroy: nothing left on the worker, facts say destroyed, backhaul refuses it; the image stays cached.
machine_set(mid, "destroyed")
t = wait_fact(mid, "destroyed", ACT)
check(t is not None, f"destroy: {mid} destroyed in {t:.1f} s" if t else "", f"destroy: {fact(mid)}")
check(not owned(W3, mid), "destroy: no unit, config, vsock socket, state file or scratch drive left", f"destroy: left {owned(W3, mid)}")
check(W3.succeed(f"ls {STATE}/images/ | grep -c '\\.erofs$'").strip() == images0, f"destroy: worker3's {images0} cached images untouched", "destroy: images changed")
check(bh_code("worker3", mid) == "404", f"destroy: backhaul CONNECT {mid} -> 404", f"destroy: backhaul -> {bh_code("worker3", mid)}")

# 5. Agent kill -9: VMs keep running, the restarted agent changes nothing.
def pids() -> dict[str, str]:
    return {m: vm_pid(W3, m) for m in on("worker3", "whoami")}


def facts3() -> list:
    return q(worker1, "http://127.0.0.1:8080", "SELECT id, state, addr FROM machine_status WHERE worker = 'worker3' ORDER BY id")


p0, f0, since = pids(), facts3(), time.time()
kill9(W3, "agent")
t0 = time.time()
# Its desired state is on disk (desired/), so it reconciles at once, with nothing to wait for.
check(until(lambda: re.search(r"(?m)^agent started worker=worker3", journal(W3, "agent", since)) is not None, ACT), f"agent kill: restarted in {time.time() - t0:.1f} s", "agent kill: agent never came back")
time.sleep(11)  # one resync tick too, so reconciles have certainly run
check(pids() == p0, f"agent kill: same Cloud Hypervisor PIDs ({p0})", f"agent kill: PIDs {p0} -> {pids()}")
check(facts3() == f0, "agent kill: facts unchanged", f"agent kill: facts {f0} -> {facts3()}")
after = journal(W3, "agent", since).split("agent started ")[-1]
acts = re.findall(r"(?m)^done machine=\S+ action=(?:Create|Start|Stop|Destroy) ", after)
check(not acts, "agent kill: no actions after the restart", f"agent kill: actions after restart: {acts}")

# 7. Failures: an image that can't be pulled backs off; an invalid spec is refused; others are unaffected.
since = time.time()
bogus = "1" * 64
nope = machine_create("worker2", "nope", f"{REGISTRY}/flyyy/nope@sha256:{bogus}", 80)
check(wait_fact(nope, "failed", ACT) is not None, f"failure: {nope} -> failed", f"failure: {nope} -> {fact(nope)}")
time.sleep(8)
delays = re.findall(rf"(?m)^image failed digest={bogus} retry_in_s=(\d+) ", journal(worker2, "agent", since))
check(3 <= len(delays) <= 5, f"failure: retries back off: {' '.join(delays)}", f"failure: retry delays {delays}")
# The agent API refuses a spec that isn't valid (a cmd that isn't an argv, 64 CPUs): 400, nothing kept or created.
good = {"id": "m-evil", "app": "echo", "image": ECHO, "cmd": [], "env": {}, "port": 5678, "services": [{"name": "http", "protocol": "http", "port": 5678}]}
for mid, broken, why in [("m-evil", {"cmd": "not json"}, "cmd"), ("m-huge", {"cpus": 64}, "cpus")]:
    rc, out = agent("worker2", "PUT", f"/v1/machines/{mid}", {**good, "id": mid, **broken})
    held = json.loads(agent("worker2", "GET", "/v1/machines")[1])
    check(rc == 400 and mid not in held and not owned(worker2, mid), f"failure: {why} {broken[why]!r} -> 400 from the agent API, nothing kept", f"failure: {mid}: {rc} {out}, held {mid in held}")
n = sum(a["started"] for a in json.loads(fly("ls --json")) if a["state"] == "active")
check(n == started0, f"failure: all {n} app machines still started", f"failure: {n} started, was {started0}")
machine_set(nope, "destroyed")
check(wait_fact(nope, "destroyed", ACT) is not None, f"failure: {nope} destroyed", f"failure: {nope} -> {fact(nope)}")

# 4b. Readiness is honest: an app that never listens on the machine's port, or exits, is never `started`.
wrong = machine_create("worker2", "whoami", WHOAMI, 81)
gone = machine_create("worker2", "echo", ECHO, 5678, ["-version"])
check(wait_fact(gone, "failed", ACT) is not None, "readiness: echo -version (exits) -> failed", f"readiness: {gone} -> {fact(gone)}")
seen_started = False
for _ in range(90):
    st = fact(wrong)[0]
    seen_started = seen_started or st == "started"
    if st == "failed":
        break
    time.sleep(0.5)
check(not seen_started and fact(wrong)[0] == "failed", "readiness: whoami on port 81 (it listens on 80) never started -> failed", f"readiness: {wrong} -> {fact(wrong)}, started once: {seen_started}")
for m in [wrong, gone]:
    machine_set(m, "destroyed")
    wait_fact(m, "destroyed", ACT)
