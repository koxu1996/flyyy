# balance: the edges pick by load (the less loaded of two random instances, by requests in flight) and keep
# to each service's hard limit (a request waits about 1 s for a slot, then 503). Through edge1 only, with
# edge2 stopped: the limits and counts are per edge. The test app's ?slow answers after the machine's DELAY
# and says "overlap" if another such request was being served there at the same time.
TESTAPP = image("flyyy/testapp")
TOKEN = control.succeed("cat /etc/control.token").strip()
edges.succeed("systemctl stop edge2")


def burst(app: str, n: int, par: int, timeout: int = 15) -> list[tuple[str, str]]:
    """n requests to the app's ?slow, par at a time, through edge1: (status, body) of each."""
    edges.succeed("rm -rf /tmp/burst && mkdir /tmp/burst")
    edges.succeed(f"seq {n} | xargs -P {par} -I{{}} sh -c 'curl -s --max-time {timeout} -o /tmp/burst/{{}} -w %{{http_code}} "
                  f"https://{app}.flyyy.localhost/cgi-bin/info?slow > /tmp/burst/{{}}.code'")
    out = []
    for i in range(1, n + 1):
        code = edges.succeed(f"cat /tmp/burst/{i}.code").strip()
        body = edges.execute(f"cat /tmp/burst/{i}")[1].strip()
        out.append((code, body))
    return out


def edge1_load() -> dict:
    return edge_state(1)["load"]


def routed(app: str, n: int) -> bool:
    return len(edge_state(1)["routes"].get(f"{app}.flyyy.localhost", [])) == n


def backhauled(on: dict[str, str]) -> bool:
    """Each machine's backhaul takes CONNECT to it: a backhaul's table trails the edges' by a moment, and
    until then edge1 answers 502."""
    return all(bh_code(w, m) == "200" for m, w in on.items())


# 1. A slow instance gets fewer requests than a fast one: 2 s against none, 8 at a time. Least loaded keeps
# their requests in flight even, so each gets requests as fast as it answers them (round robin: half each).
fast = machine_create("worker1", "lb", TESTAPP, 8080, env={"DELAY": "0"})
slow = machine_create("worker2", "lb", TESTAPP, 8080, env={"DELAY": "2"})
check(wait_fact(fast, "started", DEPLOY) is not None and wait_fact(slow, "started", DEPLOY) is not None, "lb: a fast and a slow machine started", "lb: not started")
check(until(lambda: routed("lb", 2), BOOT), "lb: edge1 routes both", f"lb: routes {edge_state(1)['routes']}")
check(until(lambda: backhauled({fast: "worker1", slow: "worker2"}), PROPAGATE), "lb: both backhauls have them", "lb: a backhaul hasn't")
got = burst("lb", 160, 8)
by = {m: sum(1 for c, b in got if c == "200" and b.endswith(m)) for m in [fast, slow]}
check(all(c == "200" for c, _ in got), "lb: 160 requests, all 200", f"lb: codes {sorted({c for c, _ in got})}")
check(by[fast] >= 3 * by[slow] and by[slow] > 0, f"lb: the fast instance got {by[fast]}, the slow one {by[slow]} (by load, not round robin)",
      f"lb: fast {by[fast]}, slow {by[slow]}")
ld = edge1_load()
check(all(ld[f"{m}.http"]["in_flight"] == 0 for m in [fast, slow]) and ld[f"{fast}.http"]["total"] >= by[fast],
      "lb: edge1's /state counts them (in flight back to 0, totals)", f"lb: load {ld}")

# 2. hard_limit 1: never more than one request at an instance; past what fits in the ~1 s queue, 503.
lims = [machine_create(w, "lim", TESTAPP, 8080, env={"DELAY": "0.5"}, hard_limit=1) for w in ["worker1", "worker3"]]
check(all(wait_fact(m, "started", DEPLOY) is not None for m in lims), "lim: two machines with hard_limit 1 started", "lim: not started")
check(until(lambda: routed("lim", 2), BOOT), "lim: edge1 routes both", "lim: not routed")
check(until(lambda: backhauled(dict(zip(lims, ["worker1", "worker3"]))), PROPAGATE), "lim: both backhauls have them", "lim: a backhaul hasn't")
before = {m: edge1_load().get(f"{m}.http", {}).get("rejected", 0) for m in lims}
got = burst("lim", 16, 16)
codes = [c for c, _ in got]
n503 = codes.count("503")
check(set(codes) <= {"200", "503"} and n503 > 0 and codes.count("200") >= 4,
      f"lim: 16 at once on 2 instances of 0.5 s: {codes.count('200')} served, {n503} got 503", f"lim: codes {codes}")
check(not any("overlap" in b for c, b in got if c == "200"), "lim: no instance ever served two at once (checked at the instance)", f"lim: {[b for _, b in got]}")
ld = edge1_load()
check(all(ld[f"{m}.http"]["peak"] == 1 and ld[f"{m}.http"]["hard_limit"] == 1 for m in lims), "lim: edge1's peak in flight per instance is 1", f"lim: {ld}")
rejected = sum(ld[f"{m}.http"]["rejected"] - before[m] for m in lims)
check(rejected == n503, f"lim: edge1 counts the {n503} 503s as rejected", f"lim: rejected {rejected}, 503s {n503}")
check(all(k in ld[f"{lims[0]}.http"] for k in ["in_flight", "total", "rejected", "failures"]), "state: per instance in flight, total, rejected, failures", f"state: {ld}")
for m in [fast, slow, *lims]:
    machine_set(m, "destroyed")

# 3. The limits through the API, flyyy status, and the control plane's view of the load (it asks the edges).
fly(f"deploy web --image {TESTAPP} --service http=http:8080,soft=2,hard=4 --count 2 --timeout {DEPLOY}")
web_svc = status("web")["services"][0]
check((web_svc["soft_limit"], web_svc["hard_limit"]) == (2, 4) and "limits soft 2 hard 4" in fly("status web"), "web: soft 2, hard 4 in the API and flyyy status", f"web: {web_svc}")
check(until(lambda: routed("web", 2), BOOT), "web: edge1 routes both", "web: not routed")
# Served too: a backhaul's table trails the edges' by a moment after a deploy (edge1 then gets 404s).
check(until(lambda: all(c == "200" for c, _ in burst("web", 4, 4)), ACT), "web: both serve through edge1", "web: not served")
LOAD = f"curl -fs -H 'authorization: Bearer {TOKEN}' http://127.0.0.1:4000/v1/apps/web/load"
first = json.loads(control.succeed(LOAD))
check(first["rps"] is None and len(first["machines"]) == 2, "load: the first sample has no rate yet, both machines", f"load: {first}")
burst("web", 40, 4)
d = json.loads(control.succeed(LOAD))
edge1 = next(e for e in d["edges"] if e["name"] == "edge1")
# An instance's total counts every request given to it, its failed tunnels too (a backhaul 404 just after a
# deploy, retried on the other): served = total - failures.
served = (d["total"] - d["failures"]) - (first["total"] - first["failures"])
check(edge1["ok"] and served == 40 and d["rps"] and d["rps"] > 0,
      f"load: the control plane summed edge1's counters: 40 more served ({d['failures'] - first['failures']} failed tunnels retried), {d['rps']} req/s over {d['interval_s']} s",
      f"load: {d}")
check(all(i["soft_limit"] == 2 and i["hard_limit"] == 4 for i in d["instances"]) and sum(m["total"] for m in d["machines"]) == d["total"],
      "load: per instance with its limits, per machine sums to the app's", f"load: {d}")
check(any(not e["ok"] for e in d["edges"] if e["name"] == "edge2"), "load: stopped edge2 is reported as not answering", f"load: edges {d['edges']}")
rc, _ = control.execute("curl -fs http://127.0.0.1:4000/v1/apps/web/load")
check(rc != 0, "load: needs a token", "load: answered without one")
start(edges, "edge2")
