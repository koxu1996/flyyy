# Corrosion: queries, the control plane's transactions, the workers' machine facts and the subscriptions.
# Every node's Corrosion API wants that node's bearer token (root and the corroapi group can read it):
# CORRO_AUTH is the curl header for it, read on the node where the command runs.
CORRO_TOKEN = "/var/lib/flyyy-corroapi/token"
CORRO_AUTH = f"-H \"authorization: Bearer $(cat {CORRO_TOKEN})\""


def q(m, api: str, sql: str, params: list = []) -> list:
    """Rows of a Corrosion query, sorted."""
    body = shlex.quote(json.dumps([sql, params]))
    out = m.succeed(f"curl -fs {CORRO_AUTH} -H 'content-type: application/json' -d {body} {api}/v1/queries")
    return sorted(json.loads(l)["row"][1] for l in out.splitlines() if '"row"' in l)


def tx(stmts: list) -> None:
    """One transaction through the control plane's Corrosion agent (the writer of placement, backhauls and ips)."""
    body = shlex.quote(json.dumps(stmts))
    control.succeed(f"curl -fs {CORRO_AUTH} -H 'content-type: application/json' -d {body} http://127.0.0.1:8080/v1/transactions")


def fact(mid: str) -> tuple[str, str]:
    rows = q(worker1, "http://127.0.0.1:8080", "SELECT state, addr FROM machine_status WHERE id = ?", [mid])
    return (rows[0][0], rows[0][1]) if rows else ("", "")


def wait_fact(mid: str, state: str, timeout: float = BOOT) -> float | None:
    """Seconds until the machine's fact is `state`, or None."""
    t0 = time.time()
    return time.time() - t0 if until(lambda: fact(mid)[0] == state, timeout, 0.1) else None


# What the backhauls and the edges subscribe to (backhaul.rs, edge_routes.rs), edge_port as text as an edge's
# /state has it.
BACKHAUL_ROWS = ("SELECT p.id, p.app, 'vsock:' || p.machine || ':' || p.port FROM placement p "
                 "JOIN service_status s ON s.id = p.id AND s.worker = p.worker WHERE p.worker = ? AND s.state != 'down'")
EDGE_JOIN = ("SELECT s.id, p.worker, p.app, b.backhaul, p.protocol, CAST(p.edge_port AS TEXT), coalesce(i.ip, ''), p.soft_limit, p.hard_limit FROM service_status s "
             "JOIN placement p ON p.id = s.id AND p.worker = s.worker JOIN backhauls b ON b.worker = p.worker "
             "LEFT JOIN ips i ON i.app = p.app AND i.state = 'allocated' WHERE s.state = 'passing'")
