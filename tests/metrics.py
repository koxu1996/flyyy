# metrics: an app's metrics without Prometheus. Each edge counts its traffic per app and service in 10 s
# buckets (requests by status class, with its own 502s and 503s apart, latency, bytes); each agent samples
# its machines' CPU and memory from their units. GET /v1/apps/<name>/metrics merges them from every edge and
# the app's workers, scoped to the caller's org, and names any node that didn't answer.
TESTAPP = image("flyyy/testapp")
API = "http://127.0.0.1:4000"
HOST = "web.flyyy.localhost"

fly("orgs create red")
fly("orgs create blue")
for user, org in [("alice", "red"), ("bob", "blue")]:
    token = fly(f"users create {user} --org {org} --role admin").strip()
    control.succeed(f"printf %s {shlex.quote(token)} > /etc/{user}.token && chmod 600 /etc/{user}.token")


def get(path: str, token: str = "/etc/alice.token") -> tuple[int, str]:
    """(HTTP status, body) of a GET to the control plane's API with a token file's token."""
    out = control.succeed(f"curl -s --max-time {SLOW_REQUEST} -w '\\n%{{http_code}}' -H \"authorization: Bearer $(cat {token})\" {API}{path}")
    body, _, code = out.rpartition("\n")
    return int(code or 0), body


def metrics(window: str = "1h") -> dict:
    code, body = get(f"/v1/apps/web/metrics?window={window}")
    assert code == 200, f"metrics: {code} {body}"
    return json.loads(body)


def as_user(user: str, args: str) -> tuple[int, str]:
    return control.execute(f"FLYYY_API={API} FLYYY_TOKEN_FILE=/etc/{user}.token flyyy {args} 2>&1")


def hits(query: str, n: int, par: int = 1) -> list[tuple[str, str]]:
    """n requests to the app's /cgi-bin/info?<query> through the edges (SO_REUSEPORT spreads the
    connections over edge1 and edge2), par at a time: (status, the x-edge that answered) of each."""
    edges.succeed("rm -rf /tmp/hits && mkdir /tmp/hits")
    edges.succeed(f"seq {n} | xargs -P {par} -I{{}} sh -c 'curl -s -o /dev/null --max-time {SLOW_REQUEST} -w %{{http_code}},%header{{x-edge}} "
                  f"\"https://{HOST}/cgi-bin/info?{query}\" > /tmp/hits/{{}}'")
    out = []
    for i in range(1, n + 1):
        st, _, edge = edges.succeed(f"cat /tmp/hits/{i}").strip().partition(",")
        out.append((st, edge))
    return out


def delta(before: dict, after: dict, *keys: str) -> tuple:
    return tuple(after["totals"][k] - before["totals"][k] for k in keys)


# 1. Alice's app on two machines, a hard limit of 1 per instance and edge, ?slow taking 0.5 s.
rc, out = as_user("alice", f"deploy web --image {TESTAPP} --service http=http:8080,hard=1 --env DELAY=0.5 --count 2 --timeout {DEPLOY}")
assert rc == 0, out
ms = [m for _, m in live("web")]
check(until(lambda: all(c == "200" for c, _ in hits("ids", 4)), ACT), "setup: web serves through the edges", "setup: web doesn't serve")

# 2. Requests through both edges add up exactly, wherever they landed.
before = metrics()
got = hits("ids", 60)
by_edge = {e: sum(1 for _, x in got if x == e) for e in ["edge1", "edge2"]}
after = metrics()
check(all(c == "200" for c, _ in got) and all(by_edge.values()), f"sum: 60 requests, {by_edge['edge1']} through edge1 and {by_edge['edge2']} through edge2",
      f"sum: {sorted(set(got))}")
check(delta(before, after, "requests", "2xx", "4xx", "5xx") == (60, 60, 0, 0), "sum: the merged metrics count exactly 60 2xx",
      f"sum: {delta(before, after, 'requests', '2xx', '4xx', '5xx')}")
check(after["asked"]["edges"] == 2 and after["missing"] == [] and after["totals"]["p50_ms"] and delta(before, after, "bytes_out")[0] > 0,
      f"sum: both edges answered; p50 {after['totals']['p50_ms']} ms, p99 {after['totals']['p99_ms']} ms, bytes out counted",
      f"sum: {after['asked']} missing {after['missing']} totals {after['totals']}")
last = [sum(after["requests"][k][-2:]) for k in ["2xx", "5xx"]]
check(len(after["t"]) == 360 and len(after["requests"]["2xx"]) == 360 and last[0] > 0, "series: 360 buckets of 10 s for 1h, the last ones with the requests",
      f"series: {len(after['t'])} buckets, last {last}")

# 3. The app's own 500s and 404s, by class; not the edge's.
before = metrics()
codes = [c for c, _ in hits("fail", 10)]
codes += [edges.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' https://{HOST}/nope").strip() for _ in range(5)]
after = metrics()
check(codes == ["500"] * 10 + ["404"] * 5 and delta(before, after, "5xx", "4xx", "edge_502", "edge_503") == (10, 5, 0, 0),
      "5xx: the app's 10 500s are 5xx and its 5 404s 4xx, none of them the edges' own", f"5xx: {codes}, {delta(before, after, '5xx', '4xx', 'edge_502', 'edge_503')}")

# 4. The edges' own 503s (every instance at its hard limit past the queue) are 5xx and counted as theirs.
before = metrics()
got = hits("slow", 24, 24)
n503, n200 = [sum(1 for c, _ in got if c == k) for k in ["503", "200"]]
after = metrics()
check(n503 > 0 and n503 + n200 == 24, f"503: 24 at once on 2 instances of 0.5 s with hard limit 1: {n200} served, {n503} got 503", f"503: {sorted(c for c, _ in got)}")
check(delta(before, after, "edge_503", "5xx", "2xx") == (n503, n503, n200), f"503: counted where they happen, as the edges' {n503} 503s",
      f"503: {delta(before, after, 'edge_503', '5xx', '2xx')}, want {n503}, {n503}, {n200}")

# 5. A busy VM shows its CPU (sampled from its unit by its agent); the other one doesn't.
busy, idle = sorted(ms)
w_busy = next(w for w, m in live("web") if m == busy)
check("burning" in bh(w_busy, busy, "/cgi-bin/info?burn"), f"cpu: {busy} burns a core for 30 s", "cpu: no burn")


def cpu_now(mid: str) -> float:
    return next((m["cpu_now"] or 0.0 for m in metrics("5m")["machines"] if m["id"] == mid), 0.0)


check(until(lambda: cpu_now(busy) >= 0.5, ACT), f"cpu: {busy} at {cpu_now(busy):.0%} of a core", f"cpu: {busy} at {cpu_now(busy):.0%}")
check(cpu_now(idle) < 0.3, f"cpu: {idle}, idle, at {cpu_now(idle):.0%}", f"cpu: idle {idle} at {cpu_now(idle):.0%}")
mm = metrics("5m")
mem = {m["id"]: m["memory_now"] for m in mm["machines"]}
check(set(mem) == set(ms) and all(v and v > 1 << 20 for v in mem.values()), f"memory: both machines report their memory ({mem})", f"memory: {mem}")
check(len(mm["t"]) == 30, "series: 30 buckets for 5m", f"series: {len(mm['t'])} buckets")
rc, out = as_user("alice", "metrics web")
check(rc == 0 and re.search(r"(?m)^requests +\d+ ", out) is not None and busy in out and "%" in out, "cli: flyyy metrics prints the totals and each machine's CPU",
      f"cli: {rc} {out}")

# 6. A node that doesn't answer is named, not silently left out.
edges.succeed("systemctl stop edge2")
m = metrics()
check(m["asked"]["edges"] == 2 and [(g["kind"], g["name"]) for g in m["missing"]] == [("edge", "edge2")], "missing: edge2 stopped is listed as missing",
      f"missing: {m['asked']} {m['missing']}")
rc, out = as_user("alice", "metrics web")
check("missing: edge edge2 didn't answer" in out, "missing: flyyy metrics says so", f"missing: {out}")
start(edges, "edge2")

# 7. Org-scoped like the rest of the API: another org's token gets 404, a bad window 400.
st, body = get("/v1/apps/web/metrics", "/etc/bob.token")
check(st == 404, "orgs: bob (blue) asking for red's web's metrics gets 404", f"orgs: bob -> {st} {body}")
rc, out = as_user("bob", "metrics web")
check(rc == 1 and "404" in out, "orgs: and so does flyyy metrics with his token", f"orgs: {rc} {out}")
check(get("/v1/apps/web/metrics", "/etc/control.token")[0] == 200, "orgs: the operator's token sees it", "orgs: operator refused")
st, body = get("/v1/apps/web/metrics?window=2m")
check(st == 400 and "5m or 1h" in body, "window: 2m is refused (5m or 1h)", f"window: {st} {body}")
