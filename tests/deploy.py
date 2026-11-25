# deploy.sh: the control plane and CLI app by app: deploy (cold and warm image), ls/status, scale (spread,
# scale to 0), the no-op and 409 re-deploys, destroy and re-deploy, kill -9 of the control plane mid
# scale-up, rejected input, and an image that can't be pulled.
import math

COLD = image("flyyy/cold1")
WHOAMI = image("traefik/whoami")
TOKEN = control.succeed("cat /etc/control.token").strip()


def stored(jq: str):
    """A jq expression over the control plane's store (its desired state, control/db.rs)."""
    return json.loads(control.succeed(f"jq -c {shlex.quote(jq)} /var/lib/flyyy-control/store.json"))


def converged_in(args: str) -> str:
    rc, out = fly_rc(args)
    m = re.findall(r"converged in [0-9.]+ s", out)
    return m[-1] if rc == 0 and m else ""


def spread(app: str, count: int) -> str:
    ws = [w for w, _ in live(app)]
    per = {w: ws.count(w) for w in sorted(set(ws))}
    good = len(ws) == count and max(per.values(), default=0) <= math.ceil(count / 3)
    return " ".join(f"{w}:{n}" for w, n in per.items()) if good else ""


def started_others() -> int:
    return sum(a["started"] for a in json.loads(fly("ls --json")) if a["state"] == "active" and a["name"] not in ("blue", "red"))


def answers(vhost: str) -> bool:
    return until(lambda: code(vhost) == "200", REQUEST)


def sampler(vhost: str) -> None:
    edges.succeed(f"systemd-run --unit=sampler --setenv=PATH=/run/current-system/sw/bin sh -c 'while :; do curl -s -o /dev/null -w \"%{{http_code}}\\n\" --max-time {SLOW_REQUEST} https://{vhost}/ >> /tmp/samples; done'")


def samples() -> list[str]:
    edges.succeed("systemctl stop sampler")
    out = edges.succeed("cat /tmp/samples; rm /tmp/samples").split()
    return out


others0 = started_others()

# 2. Deploy: a cold image (built from the registry), one per worker, spread through the edges.
t = converged_in(f"deploy blue --image {COLD} --port 80 --count 3")
check(t, f"deploy blue (cold image, 3): {t}", "deploy blue (cold) did not converge")
s = spread("blue", 3)
check(s, f"deploy: one per worker ({s})", f"deploy: spread {live('blue')}")
# (cold1 is a static page without a machine id; each edge's spread is covered by the verify suite.)
check(answers("blue.flyyy.localhost"), "deploy: blue answers through the edges", "deploy: blue doesn't answer")

# 3. ls / status, text and JSON.
ls = fly("ls")
check(re.search(r"(?m)^blue +- +10\.99\.0\.3:5000/flyyy/cold1@[0-9a-f]{12} +80 +3/3 +active$", ls), "ls: blue 3/3 active", f"ls: {ls}")
st = status("blue")
check(st["converged"] and sum(m["state"] == "started" for m in st["machines"]) == 3, "status --json: 3 machines started, converged", f"status --json: {st}")
check(len(re.findall(r"(?m)^blue-[0-9a-f]{6} +worker[123] +started +started +vsock:blue-[0-9a-f]{6}:80 ", fly("status blue"))) == 3, "status: 3 machine rows", "status: rows")

# 4. Scale 3 -> 5 -> 2 (sampling through the edges while scaling down), 0, back to 2; no-op and 409 re-deploys.
t = converged_in("scale blue --count 5")
s = spread("blue", 5)
check(t and s, f"scale 3 -> 5: {t}, spread {s}", f"scale 3 -> 5: {live('blue')}")
sampler("blue.flyyy.localhost")
t_down = time.time()
t = converged_in("scale blue --count 2")
s = spread("blue", 2)
check(t and s, f"scale 5 -> 2: {t}, spread {s}", f"scale 5 -> 2: {live('blue')}")
time.sleep(2)
smp = samples()
check(smp and all(c == "200" for c in smp), f"scale down: {len(smp)} samples via the edges, all 200", f"scale down: {sorted(set(smp))}")
# Each stopped machine drained on what the agent saw: its services down, then no more requests from the edges.
drained = [l for n in NAMES3 for l in journal(WORKERS[n], "agent", t_down).splitlines() if l.startswith("drained ")]
took = [int(m) for l in drained for m in re.findall(r"after (\d+) ms", l)]
check(len(drained) >= 1 and all("services down and no requests" in l for l in drained) and len(took) == len(drained),
      f"drain: {len(drained)} drains ended on services down + edges quiet, in {min(took, default=0)}-{max(took, default=0)} ms", f"drain: {drained}")
t = converged_in("scale blue --count 0")
time.sleep(1.5)
check(t and code("blue.flyyy.localhost") == "404", f"scale to 0: {t}, vhost 404", "scale to 0")
t = converged_in("scale blue --count 2")
check(t and answers("blue.flyyy.localhost"), f"scale 0 -> 2: {t}, vhost 200 again", "scale 0 -> 2")
n0 = stored('[.machines[] | select(.spec.app == "blue")] | length')
t = converged_in(f"deploy blue --image {COLD} --port 80 --count 2")
check(t and stored('[.machines[] | select(.spec.app == "blue")] | length') == n0, f"deploy, same spec: no-op ({t})", "deploy, same spec: not a no-op")
rc, out = fly_rc(f"deploy blue --image {WHOAMI} --port 80 --count 2")
check(rc == 1 and "409" in out, f"deploy, different image: refused ({out.strip()})", f"deploy, different image: {rc} {out}")
rc, out = fly_rc(f"deploy blue --image {COLD} --port 80 --count 2 --memory 256")
check(rc == 1 and "409" in out, "deploy, different memory: refused (409)", f"deploy, different memory: {rc} {out}")

# 5. Destroy, then the same name again: fresh ids, warm image.
old = {m["id"] for m in status("blue")["machines"]}
t = converged_in("destroy blue")
check(t, f"destroy blue: {t}", "destroy blue did not converge")
time.sleep(1.5)
check(code("blue.flyyy.localhost") == "404", "destroy: vhost 404", "destroy: vhost not 404")
check(re.search(r"(?m)^blue .* destroyed$", fly("ls")) and not status("blue")["machines"], "destroy: ls shows it destroyed, no machines left", "destroy: ls")
t = converged_in(f"deploy blue --image {COLD} --port 80 --count 3")
new = {m["id"] for m in status("blue")["machines"]}
check(t and not old & new, f"re-deploy blue (warm image, 3): {t}, fresh ids", "re-deploy reused ids or didn't converge")

# 6. Stateless: CLI with the control plane down; kill -9 mid scale-up; the data plane serves throughout.
sampler("whoami.flyyy.localhost")
control.succeed("systemctl stop control")
rc, out = fly_rc("ls")
check(rc == 1 and "cannot reach the control plane" in out, f"control plane down: CLI exits 1 ({out.strip()})", f"control plane down: {rc} {out}")
control.succeed("systemctl start control")
control.wait_until_succeeds(f"{FLY} ls", timeout=ACT)
n0 = stored('[.machines[] | select(.spec.app == "blue")] | length')
fly("scale blue --count 10 --detach")
time.sleep(0.5)
t0 = time.time()
kill9(control, "control")
control.wait_until_succeeds(f"{FLY} ls", timeout=ACT)
ok(f"kill -9 mid scale-up: API back in {int((time.time() - t0) * 1000)} ms")
t = converged_in("scale blue --count 10")
n = stored('[.machines[] | select(.spec.app == "blue")] | length')
check(t and len(live("blue")) == 10 and n == n0 + 7, f"kill -9: scale 3 -> 10 finished ({t}), 7 machines created, no duplicates", f"kill -9: live {len(live('blue'))}, rows {n0} -> {n}")
smp = samples()
check(smp and all(c == "200" for c in smp), f"data plane: {len(smp)} whoami samples while the control plane was down/killed, all 200", f"data plane: {sorted(set(smp))}")
converged_in("scale blue --count 2")

# 7. Rejected input: 400/401/404, nothing written.
apps0, m0 = stored("[(.apps | length), ([.apps[].count] | add)]"), stored(".machines | length")
img = WHOAMI
cases = [
    ("PUT", "/v1/apps/Blue", {"image": img, "port": 80, "count": 1}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": "traefik/whoami:latest", "port": 80, "count": 1}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 0, "count": 1}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1000}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "bogus": {}}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "cmd": "a string, not an argv"}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "env": {"1A": "x"}}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "env": {"A": "a\u0000b"}}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "cmd": [str(i) for i in range(65)]}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "cpus": 0}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "cpus": 9}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "memory": 64}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "memory": 99999}, TOKEN, "400"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1, "memory": "1G"}, TOKEN, "400"),
    ("PATCH", "/v1/apps/blue", {"count": -1}, TOKEN, "400"),
    ("PATCH", "/v1/apps/nope", {"count": 1}, TOKEN, "404"),
    ("GET", "/v1/apps", None, "", "401"),
    ("GET", "/v1/apps", None, "wrong-token-wrong-token-wrong-token", "401"),
    ("PUT", "/v1/apps/x", {"image": img, "port": 80, "count": 1}, "wrong-token-wrong-token-wrong-token", "401"),
]
for method, path, body, tok, want_code in cases:
    data = f"-d {shlex.quote(json.dumps(body))}" if body is not None else ""
    got = control.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' -X {method} -H 'authorization: Bearer {tok}' -H 'content-type: application/json' {data} http://127.0.0.1:4000{path}")
    check(got == want_code, f"reject: {method} {path} -> {got}", f"reject: {method} {path} {body} -> {got}, want {want_code}")
time.sleep(1)
check((stored("[(.apps | length), ([.apps[].count] | add)]"), stored(".machines | length")) == (apps0, m0),
      "reject: nothing written", "reject: something was written")

# 8. An image that can't be pulled: machines fail with the pull error, others unaffected, destroy + good deploy recovers.
# A deadline the deploy can't meet: it times out (exit 2) with both machines failed.
DOOMED = 20
rc, out = fly_rc(f"deploy red --image {REGISTRY}/flyyy/nope@sha256:{'0' * 64} --port 80 --count 2 --timeout {DOOMED}")
failed = [m for m in status("red")["machines"] if m["state"] == "failed"]
check(rc == 2 and len(failed) == 2, f"bad image: deploy timed out (exit 2), both machines failed: {failed[0]['error'][:120] if failed else ''}", f"bad image: exit {rc}, {status('red')}")
check(started_others() == others0, f"bad image: other apps still have {others0} started", f"bad image: other apps {started_others()}, was {others0}")
converged_in("destroy red")
t = converged_in(f"deploy red --image {WHOAMI} --port 80 --count 2")
check(t and answers("red.flyyy.localhost"), f"bad image: destroy + deploy with a good image recovers ({t})", "bad image: no recovery")
converged_in("destroy red")
converged_in("destroy blue")
