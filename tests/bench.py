# bench: requests per second and latency through one edge to an app on two workers (oha, in the edges
# container, HTTPS to edge1 with edge2 stopped). Not pass/fail on speed: it checks that every request
# succeeded and prints a BENCH line per run for docs/DESIGN.md. Numbers are only comparable between runs on one
# machine (containers share its CPUs with oha).
fly(f"deploy bench --image {image('traefik/whoami')} --port 80 --count 2 --timeout {DEPLOY}")
ws = sorted({w for w, _ in live("bench")})
check(len(ws) == 2, f"bench: whoami x2 on {' and '.join(ws)}", f"bench: workers {ws}")
edges.succeed("systemctl stop edge2")
check(until(lambda: {via_edge("bench.flyyy.localhost")[0] for _ in range(6)} == {"edge1"}, PROPAGATE), "bench: only edge1 answers", "bench: edge2 still answering")
check(until(lambda: len({via_edge("bench.flyyy.localhost")[1] for _ in range(8)}) == 2, PROPAGATE), "bench: edge1 reaches both machines", "bench: one machine only")
URL = "https://bench.flyyy.localhost/"


def oha(c: int, secs: int) -> dict:
    # --insecure: oha keeps its own trust store, not the container's (TLS verification isn't what this measures).
    out = edges.succeed(f"oha -z {secs}s -c {c} --no-tui --insecure --output-format json {URL}")
    return json.loads(out)


oha(8, 3)  # warm-up: connections, caches
for c, secs in [(1, 15), (64, 20)]:
    r = oha(c, secs)
    s, p = r["summary"], r["latencyPercentiles"]
    codes = r.get("statusCodeDistribution", {})
    ms = lambda v: f"{(v or 0) * 1000:.2f}"
    line = f"BENCH c={c} {secs}s: {s['requestsPerSec']:.0f} req/s, p50 {ms(p['p50'])} ms, p99 {ms(p['p99'])} ms, success {s['successRate']:.4f}, codes {codes}"
    print(line)
    check(s["successRate"] == 1 and set(codes) == {"200"}, line, f"bench: failures: {line} {r.get('errorDistribution')}")
start(edges, "edge2")
