# ips: Fly-style ingress addresses. The control plane's flags (nix/modules/control.nix: --shared-ip 10.200.0.1, --ip-pool
# 10.200.1.0/24) go to ip_pools; the edges make them local (AnyIP, `proxy anyip`). Each app gets a random
# dedicated address from the pool (ips), kept across redeploys and released on destroy, then not reused for
# 24 h. HTTP stays on the shared IP, by Host; tcp services are on (the app's own IP, edge port), so two apps
# both publish :5432, and neither's traffic can arrive on the other's address.
import ipaddress
SVC = image("flyyy/svc")
SHARED, POOL = "10.200.0.1", ipaddress.ip_network("10.200.1.0/24")
SPEC = "--service db=tcp:5432,interval=1,timeout=1 --service web=http:8080,path=/healthz,interval=1,timeout=1 --env TCP_PORT=5432"


def ip_of(app: str) -> str:
    return next((i["ip"] for i in status(app)["ips"] if i["type"] == "dedicated"), "")


def banner(ip: str, port: int = 5432) -> str:
    """The first line through ip:port from the edges: the svc image says its machine id first."""
    cmd = f"exec 3<>/dev/tcp/{ip}/{port} && read -t 3 l <&3 && echo $l"
    return edges.execute(f"timeout {REQUEST} bash -c {shlex.quote(cmd)} 2>/dev/null")[1].strip()


def listeners(ip: str, port: int = 5432) -> int:
    return int(edges.succeed(f"ss -Htln 'src {ip} and sport = :{port}' | grep -c . || true").strip())


# 1. The prefixes: in ip_pools, and local on the edges.
pools = q(control, "http://127.0.0.1:8080", "SELECT prefix, kind, state FROM ip_pools")
check(pools == [[f"{SHARED}/32", "shared", "active"], [str(POOL), "pool", "active"]], f"pools: {SHARED} shared, {POOL} a pool", f"pools: {pools}")
# execute, not succeed: until the route is there, `ip route get` fails (no route), which is "not yet".
local = lambda: all(edges.execute(f"ip -o route get {a}")[1].startswith("local ") for a in [SHARED, "10.200.1.77"])
# 60 s: the edges join the mesh last, and their Corrosion gets the ip_pools rows a little after that.
check(until(local, BOOT), "anyip: the edges hold both prefixes as local (ip route ... dev lo)", f"anyip: {edges.succeed('ip route show table local')}")

# 2. Two apps, both with tcp :5432 (the edge port defaults to the service's): each gets its own address.
for app in ["pg-a", "pg-b"]:
    fly(f"deploy {app} --image {SVC} --count 1 --timeout {DEPLOY} {SPEC}")
a_ip, b_ip = ip_of("pg-a"), ip_of("pg-b")
a_id, b_id = live("pg-a")[0][1], live("pg-b")[0][1]
check(a_ip != b_ip and all(ipaddress.ip_address(i) in POOL for i in [a_ip, b_ip]) and SHARED not in (a_ip, b_ip),
      f"alloc: pg-a got {a_ip}, pg-b {b_ip}: distinct, from the pool", f"alloc: {a_ip} {b_ip}")
rows = q(control, "http://127.0.0.1:8080", "SELECT ip, app, state FROM ips")
check(rows == sorted([[a_ip, "pg-a", "allocated"], [b_ip, "pg-b", "allocated"]]), "alloc: ips rows record both", f"alloc: {rows}")
out = fly("ips pg-a")
check(re.search(rf"(?m)^{re.escape(a_ip)} +dedicated v4 +tcp db :5432$", out) and re.search(rf"(?m)^{re.escape(SHARED)} +shared v4 +http\(s\) pg-a\.", out),
      "cli: flyyy ips pg-a lists its dedicated address (db :5432) and the shared one", f"cli: {out}")
out = fly("status pg-a")
check(f"ips: {a_ip} dedicated, {SHARED} shared" in out and f"service db: tcp port 5432, {a_ip}:5432;" in out, "cli: flyyy status shows the addresses and db on its own IP",
      f"cli: {out}")

# 3. Routed by (destination IP, port): each address's :5432 is its own app, on both edges.
check(until(lambda: banner(a_ip) == a_id and banner(b_ip) == b_id, ACT), f"tcp: {a_ip}:5432 -> {a_id}, {b_ip}:5432 -> {b_id}",
      f"tcp: {a_ip} -> {banner(a_ip)!r}, {b_ip} -> {banner(b_ip)!r}")
# A bind before the AnyIP route was there is retried every 2 s.
check(until(lambda: listeners(a_ip) == 2 and listeners(b_ip) == 2, ACT), "tcp: both edges listen on each app's :5432", f"tcp: {listeners(a_ip)} / {listeners(b_ip)} listeners")
seen = {banner(ip) for ip in [a_ip, b_ip] * 6}
check(seen == {a_id, b_id}, "tcp: 12 connections, every one to the app whose address it hit", f"tcp: {seen}")
# Not anywhere else: another port on an app's address, a pool address nobody has, the shared IP.
spare = str(next(h for h in POOL.hosts() if str(h) not in (a_ip, b_ip)))
check(banner(a_ip, 5433) == "" and banner(spare) == "" and banner(SHARED) == "", f"tcp: nothing on {a_ip}:5433, on unallocated {spare}:5432 or on {SHARED}:5432",
      "tcp: answered off its own address")

# 4. HTTP stays on the shared IP, by Host (and 127.0.0.1, as before); not on the apps' own addresses.
http_id = lambda app, ip: edges.execute(f"curl -fs --max-time {REQUEST} --resolve {app}.flyyy.localhost:443:{ip} https://{app}.flyyy.localhost/cgi-bin/id")[1].strip()
check(until(lambda: http_id("pg-a", SHARED) == a_id and http_id("pg-b", SHARED) == b_id, PROPAGATE), f"http: https on {SHARED} routes pg-a and pg-b by Host",
      f"http: {http_id('pg-a', SHARED)!r} {http_id('pg-b', SHARED)!r}")
r = edges.succeed(f"curl -si --max-time {REQUEST} http://{SHARED}/ -H 'Host: pg-a.flyyy.localhost' | head -1").strip()
check(" 308" in r, f"http: plain HTTP on {SHARED} redirects to HTTPS", f"http: {r}")
check(http_id("pg-a", a_ip) == "" and http_id("pg-b", a_ip) == "", f"http: no HTTP(S) on pg-a's own {a_ip}, for it or for pg-b", "http: served on a dedicated IP")

# 5. Kept across redeploys and scaling.
fly(f"deploy pg-a --image {SVC} --count 2 --timeout {DEPLOY} {SPEC}")
check(ip_of("pg-a") == a_ip, f"keep: a redeploy (count 1 -> 2) keeps {a_ip}", f"keep: now {ip_of('pg-a')}")
a_ids = {m for _, m in live("pg-a")}
check(until(lambda: {banner(a_ip) for _ in range(8)} == a_ids, ACT), f"keep: {a_ip}:5432 reaches both pg-a machines", f"keep: {[banner(a_ip) for _ in range(4)]}")

# 6. Destroy releases it (the edges stop listening); it isn't reused: a new deploy gets another one.
fly(f"destroy pg-a --timeout {DEPLOY}")
rel = q(control, "http://127.0.0.1:8080", "SELECT state, CAST(released_at > 0 AS TEXT) FROM ips WHERE ip = ?", [a_ip])
check(rel == [["released", "1"]] and ip_of("pg-a") == "", f"release: {a_ip} released with pg-a", f"release: {rel}")
check(until(lambda: listeners(a_ip) == 0, PROPAGATE), f"release: the edges closed {a_ip}:5432", f"release: {listeners(a_ip)} listeners left")
fly(f"deploy pg-a --image {SVC} --count 1 --timeout {DEPLOY} {SPEC}")
a2 = ip_of("pg-a")
check(a2 not in ("", a_ip, b_ip), f"quarantine: pg-a redeployed gets {a2}, not the released {a_ip}", f"quarantine: got {a2}")
check(until(lambda: banner(a2) == live("pg-a")[0][1], ACT) and banner(a_ip) == "", f"quarantine: {a2}:5432 serves pg-a; {a_ip}:5432 nothing", "quarantine: wrong routing")
check(banner(b_ip) == b_id and ip_of("pg-b") == b_ip, f"others: pg-b kept {b_ip} and kept serving throughout", "others: pg-b disturbed")
