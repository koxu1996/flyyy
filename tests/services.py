# services: an app's services (an http one and a tcp one), health-checked by the workers against their own
# VMs (service_status, written only when a result changes) and routed by the edges to passing ones only:
# http by Host, tcp on its edge port on the app's own IP (both edges listen, SO_REUSEPORT). A failing check takes an instance out
# of that service's route; an instance whose tunnel fails is ejected for a while; a destroyed machine's
# services go down. (Stale workers last: proxy's order() unit test.)
SVC = image("flyyy/svc")
SPEC = "--service web=http:8080,path=/healthz,interval=1,timeout=1 --service echo=tcp:7000@17000,interval=1,timeout=1"
fly(f"deploy svc --image {SVC} --count 2 --timeout {DEPLOY} {SPEC}")
where = dict((mid, w) for w, mid in live("svc"))
ms = sorted(where)
IP = next(i["ip"] for i in status("svc")["ips"] if i["type"] == "dedicated")


def states() -> dict[str, str]:
    return dict(q(control, "http://127.0.0.1:8080", "SELECT id, state FROM service_status WHERE app = 'svc'"))


def ids_via_edges(n: int = 16) -> set[str]:
    return {edges.execute(f"curl -fs --max-time {REQUEST} https://svc.flyyy.localhost/cgi-bin/id")[1].strip() for _ in range(n)} - {""}


def echo(msg: str) -> str:
    """What comes back through the app's IP:17000, after the machine id the svc image says first."""
    cmd = f"exec 3<>/dev/tcp/{IP}/17000 && read -t 3 id <&3 && printf '%s\\n' {shlex.quote(msg)} >&3 && read -t 3 l <&3 && echo $l"
    return edges.execute(f"bash -c {shlex.quote(cmd)}")[1].strip()


# 1. Checked and reported: both services of both machines pass; the API and flyyy status say so.
check(until(lambda: all(states().get(f"{m}.{s}") == "passing" for m in ms for s in ["web", "echo"]), ACT),
      f"checks: web and echo pass on both machines ({' '.join(ms)})", f"checks: {states()}")
st = status("svc")
check([(s["name"], s["passing"]) for s in st["services"]] == [("web", 2), ("echo", 2)], "status: web 2 and echo 2 passing", f"status: {st['services']}")
out = fly("status svc")
check("service web: http port 8080" in out and f"service echo: tcp port 7000, {IP}:17000" in out and "2/2 passing" in out,
      "flyyy status lists both services and their checks", f"flyyy status: {out}")

# 2. Routed: http by Host over both instances, tcp on the edge port; both edges listen on it.
check(until(lambda: ids_via_edges() == set(ms), PROPAGATE), "http: https://svc.flyyy.localhost/ reaches both instances", f"http: {ids_via_edges()}")
check(until(lambda: echo("hello svc") == "hello svc", PROPAGATE), f"tcp: {IP}:17000 (the app's own IP) echoes through an instance", f"tcp: echo -> {echo('x')!r}")
n = edges.succeed(f"ss -Htln 'src {IP} and sport = :17000' | grep -c . || true").strip()
check(n == "2", f"tcp: both edges listen on {IP}:17000 (SO_REUSEPORT)", f"tcp: {n} listeners on {IP}:17000")

# 3. Written only on change: nothing while the results stay the same.
since = time.time()
time.sleep(6)
writes = sum(len(re.findall(r"(?m)^health instance=svc-", journal(WORKERS[n], "agent", since))) for n in NAMES3)
check(writes == 0, "health: no writes while every result stays the same", f"health: {writes} writes in 6 s of steady checks")

# 4. A failing check: a's web fails (set through its worker's backhaul, which serves failing services
# too) -> out of the http route; its echo still passes and still takes connections.
a, b = ms
na = where[a]
bh(na, f"{a}.web", "/cgi-bin/fail")
check(until(lambda: states().get(f"{a}.web") == "failing", PROPAGATE), f"check: {a}'s web failing once /healthz is gone", f"check: {states()}")
check(until(lambda: ids_via_edges() == {b}, PROPAGATE), f"http: only {b} while {a} fails its check", f"http: {ids_via_edges()}")
check(states().get(f"{a}.echo") == "passing" and status("svc")["services"][0]["passing"] == 1, f"check: {a}'s echo still passes; web 1/2", f"check: {states()} {status('svc')['services']}")
bh(na, f"{a}.web", "/cgi-bin/pass")
check(until(lambda: ids_via_edges() == set(ms), PROPAGATE), f"http: {a} back in the route once it passes again", f"http: {ids_via_edges()}")

# 5. A tunnel that fails gets its instance ejected: with a's backhaul stopped, every request still gets an
# answer, and a is tried (and fails) about once per edge, not on every request.
since = time.time()
WORKERS[na].succeed("systemctl stop proxy")
got = [edges.execute(f"curl -fs --max-time {REQUEST} https://svc.flyyy.localhost/cgi-bin/id")[1].strip() for _ in range(12)]
tries = sum(journal(edges, f"edge{e}", since).count(f"{a}.web(") for e in [1, 2])
check(got == [b] * 12 and 1 <= tries <= 2, f"eject: 12/12 answered by {b} with {a}'s backhaul down; {a} was tried {tries}x (ejected after)",
      f"eject: answers {got}, {a} tried {tries}x")
start(WORKERS[na], "proxy")

# 6. A destroyed machine's services go down (once), and out of every route.
fly(f"scale svc --count 1 --timeout {DEPLOY}")
gone = next(m for m in ms if m not in dict((mid, w) for w, mid in live("svc")))
check(until(lambda: states().get(f"{gone}.web") == "down" and states().get(f"{gone}.echo") == "down", PROPAGATE), f"down: {gone}'s services down after scale 2 -> 1", f"down: {states()}")

# 7. What deploy refuses: two http services, the edges' own ports, --port with --service.
rc, out = fly_rc(f"deploy bad --image {SVC} --count 1 --service a=http:8080 --service b=http:8081")
check(rc != 0 and "400" in out and "at most one http" in out, "deploy: two http services -> 400", f"deploy two http: {rc} {out}")
rc, out = fly_rc(f"deploy clash --image {SVC} --count 1 --service x=tcp:443")
check(rc != 0 and "400" in out and "443" in out, "deploy: a tcp service on the edges' own 443 -> 400", f"deploy clash: {rc} {out}")
rc, out = fly_rc(f"deploy both --image {SVC} --count 1 --port 8080 --service a=http:8080")
check(rc != 0 and "not both" in out, "deploy: --port and --service together refused", f"deploy both: {rc} {out}")

# 8. Destroyed: no service routes, and the edges close the tcp listener.
fly(f"destroy svc --timeout {DEPLOY}")
check(until(lambda: edges.succeed(f"ss -Htln 'src {IP} and sport = :17000' | grep -c . || true").strip() == "0", PROPAGATE), f"destroy: the edges closed {IP}:17000", "destroy: 17000 still open")
