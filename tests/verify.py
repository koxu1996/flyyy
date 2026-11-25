# verify.sh, including --failover: backhauls, each edge spreading load, no VM networking, backhaul
# validation, proxy tables == their agents, then failover of a machine, a backhaul, all VMs behind
# stale facts, and an edge.

# 2. Every machine through its own worker's backhaul.
for app in ["whoami", "echo"]:
    for w, mid in live(app):
        got = ""
        def answers() -> bool:
            global got
            got = id_of(bh(w, mid))
            return got == mid
        until(answers, REQUEST)
        check(got == mid, f"backhaul {w} CONNECT {mid} -> {got}", f"backhaul {w} CONNECT {mid} -> '{got}'")


# 3. Each edge spreads each app over all its machines; both edges answer. One request at a time, every
# instance has none in flight, so the less loaded of two random ones is a random one (balance.rs).
def rr(app: str) -> None:
    want = sorted(mid for _, mid in live(app))
    k = len(want)
    out = edges.succeed(f"for i in $(seq {20 * k}); do curl -fsi --max-time {REQUEST} https://{app}.flyyy.localhost/ | tr -d '\\r'; echo ===; done")
    seqs: dict[str, list[str]] = {}
    for resp in out.split("===")[:-1]:
        e = re.search(r"(?im)^x-edge: (\S+)", resp)
        seqs.setdefault(e.group(1) if e else "?", []).append(id_of(resp.split("\n\n", 1)[-1]))
    for e in ["edge1", "edge2"]:
        s = seqs.get(e, [])
        spread = len(s) >= 2 * k and sorted(set(s)) == want
        check(spread, f"spread {app} via {e}: {len(s)} requests over all its {k} machines", f"spread {app} via {e}: {s}, want all of {want}")


rr("whoami")
rr("echo")
check(code("nope.flyyy.localhost") == "404", "edge nope.flyyy.localhost -> 404", "edge nope -> not 404")
check(code("127.0.0.1", extra="-k -H 'Host:'") == "400", "edge without Host -> 400", "edge without Host -> not 400")

# 3b. TLS ends at the edges: a self-signed *.flyyy.localhost, trusted in the edges container, so every
# request above was HTTPS with the certificate verified. Plain HTTP only redirects, from both edges
# (they share :80 and :443, SO_REUSEPORT).
redirects = []
for _ in range(16):
    r = edges.succeed(f"curl -si --max-time {REQUEST} 'http://whoami.flyyy.localhost/a/b?c=1&d=2' | tr -d '\\r'")
    st = r.split("\n", 1)[0].split(" ")[1] if r.startswith("HTTP/") else ""
    loc = re.search(r"(?im)^location: (\S+)", r)
    e = re.search(r"(?im)^x-edge: (\S+)", r)
    redirects.append((st, loc.group(1) if loc else "", e.group(1) if e else ""))
check({(st, loc) for st, loc, _ in redirects} == {("308", "https://whoami.flyyy.localhost/a/b?c=1&d=2")} and {e for *_, e in redirects} == {"edge1", "edge2"},
      "tls: plain HTTP -> 308 to the same host, path and query on HTTPS, from both edges", f"tls: redirects {set(redirects)}")
c = edges.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' --max-time {REQUEST} -H 'Host: example.com' http://127.0.0.1/")
check(c == "404", "tls: plain HTTP for a host outside the apps domain -> 404, no redirect", f"tls: example.com over HTTP -> {c}")
hdrs = edges.succeed("curl -fs https://whoami.flyyy.localhost/")
check(re.search(r"(?im)^X-Forwarded-Proto: https\s*$", hdrs) is not None, "tls: the app sees X-Forwarded-Proto: https", f"tls: headers at the app: {hdrs}")
v = edges.succeed("curl -s -o /dev/null -w '%{ssl_verify_result} %{http_version}' https://whoami.flyyy.localhost/")
check(v == "0 1.1", "tls: certificate verified against the trust store, HTTP/1.1", f"tls: verify result / version {v}")

# SIGHUP (systemctl reload) rereads the certificate: a new one is served by both edges; one whose key
# doesn't match is refused and the previous stays. Then the original, trusted one comes back.
edges.succeed("""set -e
  cp /var/lib/edge-tls/cert.pem /tmp/orig-cert.pem; cp /var/lib/edge-tls/key.pem /tmp/orig-key.pem
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 -subj '/CN=*.flyyy.localhost (2)' \
    -addext 'subjectAltName=DNS:*.flyyy.localhost' -keyout /tmp/new-key.pem -out /tmp/new-cert.pem 2>/dev/null
  cat /tmp/new-cert.pem > /var/lib/edge-tls/cert.pem; cat /tmp/new-key.pem > /var/lib/edge-tls/key.pem
  systemctl reload edge1 edge2""")
time.sleep(1)
def served(ca: str) -> set[str]:
    out = edges.succeed(f"for i in $(seq 12); do curl -s -o /dev/null -w '%{{http_code}}\\n' --cacert {ca} https://whoami.flyyy.localhost/ -D - | tr -d '\\r' | grep -i '^x-edge' || echo fail; done")
    return set(out.split())
check(served("/tmp/new-cert.pem") == {"x-edge:", "edge1", "edge2"}, "tls: after a reload both edges serve the new certificate", f"tls: with the new CA: {served('/tmp/new-cert.pem')}")
rc = edges.execute(f"curl -s -o /dev/null --max-time {REQUEST} https://whoami.flyyy.localhost/")[0]
check(rc == 60, "tls: the old certificate is gone (the old CA no longer verifies it)", f"tls: old CA -> curl exit {rc}")
edges.succeed("cat /tmp/orig-key.pem > /var/lib/edge-tls/key.pem; systemctl reload edge1 edge2")
time.sleep(1)
check(served("/tmp/new-cert.pem") == {"x-edge:", "edge1", "edge2"} and "reload failed" in journal(edges, "edge1"),
      "tls: a certificate whose key doesn't match is refused; the previous one stays", "tls: a mismatched reload took effect")
edges.succeed("cat /tmp/orig-cert.pem > /var/lib/edge-tls/cert.pem; systemctl reload edge1 edge2")
check(until(lambda: edges.execute("curl -fs -o /dev/null https://whoami.flyyy.localhost/")[0] == 0, REQUEST), "tls: the original certificate is back", "tls: original certificate not back")

# 4. VMs have no network: whoami lists only loopback and the dummy eth0's 10.0.0.2 (no NIC behind it),
# and no worker has bridges, taps or VM NICs.
ips = set(re.findall(r"(?m)^IP: (\S+)", edges.succeed("curl -fs https://whoami.flyyy.localhost/")))
check(ips and ips <= {"127.0.0.1", "::1", "10.0.0.2"}, f"guest has only loopback and the dummy ({' '.join(sorted(ips))})", f"guest IPs: {ips}")
for n in NAMES3:
    wm = WORKERS[n]
    links = wm.succeed("ip -o link show | grep -oE ' (br0|tap-[0-9]+)[:@]' || true").strip()
    nics = wm.succeed(f"grep -lF '\"--net\"' {RUN}/*.json 2>/dev/null || true").strip()
    # (No /dev/net/tun check: the host's Nix config passes /dev/net into every sandbox.)
    check(not links and not nics, f"{n}: no br0/taps, no --net in any VM's arguments", f"{n}: links {links}, configs {nics}")

# 5. Backhaul validation: only this worker's own machines are dialled, even if placement were wrong. Two bad
# placements written by this test (as if by the control plane), each with a passing fact: a path-traversal
# machine, and another worker's machine placed on worker1.
other = on("worker2", "whoami")[0]
EVIL = "INSERT INTO service_status (id, machine, worker, app, service, protocol, addr, state) VALUES (?, ?, 'worker1', 'evil', 'http', 'http', '', 'passing')"
EVIL_PLACED = "INSERT INTO placement (id, machine, app, worker, protocol, port, edge_port) VALUES (?, ?, 'evil', 'worker1', 'http', 80, 0)"
tx([[EVIL, ["worker1-evil1.http", "worker1-evil1"]], [EVIL, ["worker1-evil2.http", "worker1-evil2"]],
    [EVIL_PLACED, ["worker1-evil1.http", "../../etc"]], [EVIL_PLACED, ["worker1-evil2.http", other]]])
time.sleep(2)
for t in ["nope", other, "10.100.1.10:80", "worker1-evil1", "worker1-evil2"]:
    c = bh_code("worker1", t)
    check(c == "404", f"backhaul worker1 CONNECT {t} -> 404", f"backhaul worker1 CONNECT {t} -> {c}")
c = edges.execute(f"curl -s -o /dev/null -w '%{{http_code}}' --max-time {REQUEST} --cacert {TLS_DIR}/ca.pem --cert {TLS_DIR}/cert.pem --key {TLS_DIR}/key.pem https://{MESH['worker1']}:4280/")[1]
check(c == "405", "backhaul worker1 GET -> 405", f"backhaul worker1 GET -> {c}")
# The CONNECT log line: its target, status and the VM socket it dialled ("-": none).
dials = [l for l in journal(worker1, "proxy").splitlines() if re.match(rf"^CONNECT target=(nope|{other}|worker1-evil[12])\.http |^CONNECT target=10\.100\.1\.10 ", l)]
check(len(dials) >= 5 and all(" status=404 vm=- " in d for d in dials), "backhaul: refused targets were never dialled", f"backhaul refused targets: {dials}")
skipped = json.loads(worker1.succeed("curl -fs http://127.0.0.1:9080/state"))["skipped"]
check({"worker1-evil1.http", "worker1-evil2.http"} <= set(skipped), f"backhaul: bad vsock addrs skipped, in its /state: {skipped}", f"backhaul: skipped {skipped}")
for n in NAMES3:
    f = WORKERS[n].succeed("cat /proc/sys/net/ipv4/ip_forward").strip()
    check(f == "0", f"{n} ip_forward = 0", f"{n} ip_forward = {f}")

# 6. Every proxy's table equals what its own Corrosion agent returns.
for n in NAMES3:
    db = q(WORKERS[n], "http://127.0.0.1:8080", BACKHAUL_ROWS, [n])
    px = sorted(json.loads(WORKERS[n].succeed("curl -fs http://127.0.0.1:9080/state"))["rows"])
    check(db and db == px, f"state {n}: backhaul == agent ({len(db)} rows)", f"state {n}: backhaul {px} != agent {db}")
JOIN = EDGE_JOIN
all_rows = q(worker1, "http://127.0.0.1:8080", JOIN)
for e in [1, 2]:
    px = sorted(json.loads(edges.succeed(f"curl -fs http://127.0.0.1:910{e}/state"))["rows"])
    db = q(edges, f"http://127.0.0.1:810{e}", JOIN)
    check(db and db == px == all_rows, f"state edge{e}: edge == its agent == worker1's agent ({len(db)} rows)", f"state edge{e}: edge {px}, agent {db}, worker1 {all_rows}")
tx([["DELETE FROM service_status WHERE id IN ('worker1-evil1.http', 'worker1-evil2.http')", []],
    ["DELETE FROM placement WHERE id IN ('worker1-evil1.http', 'worker1-evil2.http')", []]])


# 7. Failover.
def only_from(vhost: str, count: int, keep, what: str) -> None:
    """`count` requests, all 200 and all from (edge, machine) pairs that `keep` accepts."""
    got = [via_edge(vhost, 10) for _ in range(count)]
    good = all(e and m and keep(e, m) for e, m in got)
    check(good, f"{what} ({' '.join(sorted({m for _, m in got}))})", f"{what}: {got}")


# The control plane is stopped meanwhile: it would start its machine again (it sends every worker its
# desired state each round). The agent API takes the stop and start without it.
m1 = on("worker1", "whoami")[0]
PUT_ON[m1] = "worker1"
control.succeed("systemctl stop control")
machine_set(m1, "stopped")
wait_fact(m1, "stopped", ACT)
time.sleep(1)
only_from("whoami.flyyy.localhost", 24, lambda e, m: m != m1, f"failover: {m1} stopped (desired state) -> all 200, skipped")
machine_set(m1, "started")
wait_fact(m1, "started", BOOT)
control.succeed("systemctl start control")
time.sleep(1)

w1 = set(on("worker1", "whoami"))
worker1.succeed("systemctl stop proxy")
only_from("whoami.flyyy.localhost", 24, lambda e, m: m not in w1, "failover: worker1 backhaul stopped -> all 200 from other workers")
start(worker1, "proxy")

# Machine agents stopped, then the VMs: the facts still say started, so the edges try every one.
# Starting the agents again restarts the VMs (desired state is still started).
for n in NAMES3:
    WORKERS[n].succeed("systemctl stop agent")
    rc, out = uctl(WORKERS[n], "stop " + " ".join(f"fc-{m}.service" for m in on(n, "whoami")))
    assert rc == 0, out
c = code("whoami.flyyy.localhost", 20)
check(c == "502", "failover: all whoami VMs gone behind stale facts -> 502", f"failover: all whoami VMs gone behind stale facts -> {c}, want 502")
for n in NAMES3:
    start(WORKERS[n], "agent")
back = all(until(lambda w=w, m=m: id_of(bh(w, m)) == m, 30) for w, m in live("whoami"))
check(back, "failover: agents restarted the stopped VMs", "failover: VMs not back after the agents restarted")
# Their checks pass again, and the edges' ejections (every instance, by the 502 above) are over.
passing = lambda: {s for (s,) in q(control, "http://127.0.0.1:8080", "SELECT state FROM service_status WHERE app = 'whoami'")} == {"passing"}
check(until(passing, ACT), "failover: the restarted VMs' checks pass again", "failover: whoami checks not all passing")
check(until(lambda: not edge_state(1)["ejected"] and not edge_state(2)["ejected"], PROPAGATE), "failover: the edges ejected nobody any more",
      f"failover: still ejected {edge_state(1)['ejected']} {edge_state(2)['ejected']}")

edges.succeed("systemctl stop edge1")
only_from("whoami.flyyy.localhost", 20, lambda e, m: e == "edge2", "failover: edge1 stopped -> all 200 from edge2")
start(edges, "edge1")
edges.wait_until_succeeds("curl -fs http://127.0.0.1:9101/state | jq -e '.snapshots > 0'", timeout=ACT)
rr("whoami")
