# events: an app's events (GET /v1/apps/<name>/events, the control plane's ring per app, newest first) as a
# deploy, a check, a scale and a silent worker make them, and a machine's console (GET /v1/apps/<name>/
# machines/<id>/logs, its worker's journal of the VM's unit, through the agent API).
SVC = image("flyyy/svc")
API = "http://127.0.0.1:4000"


def get(path: str, token_file: str = "/etc/control.token") -> tuple[str, dict]:
    rc, out = control.execute(f"curl -s -w '\\n%{{http_code}}' -H \"authorization: Bearer $(cat {token_file})\" {API}{path}")
    body, _, code = out.rpartition("\n")
    try:
        return code, json.loads(body)
    except ValueError:
        return code, {"raw": body}


def evs() -> list[dict]:
    return get("/v1/apps/ev/events")[1].get("events", [])


def has(text: str, machine: str = "") -> bool:
    return any(text in e["text"] and (not machine or e["machine"] == machine) for e in evs())


fly(f"deploy ev --image {SVC} --count 2 --timeout {DEPLOY} --service web=http:8080,path=/healthz,interval=1,timeout=1")
where = dict((mid, w) for w, mid in live("ev"))
ms = sorted(where)

# 1. A deploy's events: deployed (the app's), each machine placed on its worker, started, its check passing.
check(until(lambda: has(", 2 VMs") and has("deployed") and all(has(f"placed on {where[m]}", m) and has("started", m) and has("check web passing", m) for m in ms), PROPAGATE),
      f"events: deployed (2 VMs); {', '.join(ms)} placed, started, check web passing", f"events: {evs()}")
e = evs()
check([x["at_ms"] for x in e] == sorted((x["at_ms"] for x in e), reverse=True) and next(x for x in e if "deployed" in x["text"])["machine"] == "",
      f"events: newest first ({len(e)}); the deploy is the app's own (no machine)", f"events: {e}")
built = [m for m in ms if has("image built", m) or has("image building", m)]
# A build shorter than a round (5 s) isn't seen: the unit test covers the texts.
note(f"events: image building/built seen for {built or 'none'} (rounds every 5 s may miss a quick build)")

# 2. A check failing, then passing again (set through its worker's backhaul, as in the services suite).
a, b = ms
na = where[a]
bh(na, f"{a}.web", "/cgi-bin/fail")
check(until(lambda: has("check web failing", a), PROPAGATE), f"events: {a}'s check web failing", f"events: {evs()[:5]}")
bh(na, f"{a}.web", "/cgi-bin/pass")
check(until(lambda: evs()[0]["text"] == "check web passing" and evs()[0]["machine"] == a, PROPAGATE), f"events: {a}'s check web passing again, newest",
      f"events: {evs()[:5]}")

# 3. A machine's console from its worker: the guest's init and the app, as the VM's unit logged them.
st, logs = get(f"/v1/apps/ev/machines/{a}/logs")
lines = logs.get("lines") or []
check(st == "200" and logs.get("worker") == where[a] and 0 < len(lines) <= 200 and any("flyyy-init" in l for l in lines),
      f"logs: {a}'s console from {where[a]}, {len(lines)} lines (at most 200), the guest's init in it", f"logs: {st} {str(logs)[:400]}")
codes = [get(p)[0] for p in ["/v1/apps/ev/machines/ev-ffffff/logs", f"/v1/apps/ev/machines/{a}x/logs", "/v1/apps/ev/machines/other-0a1b2c/logs"]]
check(codes == ["404", "400", "400"], "logs: an unknown machine 404; an id that isn't the app's 400", f"logs: {codes}")
# Another org's user sees neither its events nor its logs.
fly("orgs create red")
control.succeed(f"{FLY} users create alice --org red > /etc/alice.token")
codes = [get(p, "/etc/alice.token")[0] for p in ["/v1/apps/ev/events", f"/v1/apps/ev/machines/{a}/logs"]]
check(codes == ["404", "404"], "access: another org's user gets 404 for its events and logs", f"access: {codes}")

# 4. Scaled down: the app's event, and the machine that goes.
fly(f"scale ev --count 1 --timeout {DEPLOY}")
gone = next(m for m in ms if m not in [mid for _, mid in live("ev")])
check(until(lambda: has("scaled to 1 VM") and (has("destroyed", gone) or has("stopped", gone)), PROPAGATE), f"events: scaled to 1 VM; {gone} stopped or destroyed",
      f"events: {evs()[:6]}")

# 5. Its worker goes silent (its agent stopped: no heartbeat), then comes back.
kept = next(m for m in ms if m != gone)
n = where[kept]
WORKERS[n].succeed("systemctl stop agent")
check(until(lambda: has(f"{n} silent", kept), BOOT), f"events: {n} silent (no heartbeat), for {kept}", f"events: {evs()[:5]}")
start(WORKERS[n], "agent")
check(until(lambda: has(f"{n} back", kept), BOOT), f"events: {n} back", f"events: {evs()[:5]}")
