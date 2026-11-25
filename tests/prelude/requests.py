# Requests through the backhauls and the edges, from the edges container, and the edges' /state.

# The backhaul is mTLS on the mesh and takes edge certificates only: requests to it go from the edges
# container, with its mesh certificate, as the edges' own do (the worker verified by its mesh address).
TLS_DIR = "/var/lib/flyyy-tls"
BH_TLS = f"--proxy-cacert {TLS_DIR}/ca.pem --proxy-cert {TLS_DIR}/cert.pem --proxy-key {TLS_DIR}/key.pem"


def bh_proxy(worker: str) -> str:
    return f"-p -x https://{MESH[worker]}:4280 {BH_TLS}"


def svc(target: str) -> str:
    """A backhaul CONNECT target is a service, <machine>.<service>: a bare machine id means its http
    service (what --port and unmanaged machines have). Anything with a dot or colon goes as it is."""
    return target if "." in target or ":" in target else f"{target}.http"


def bh(worker: str, target: str, path: str = "/") -> str:
    """One request to a machine through a worker's backhaul (CONNECT <target>), from the edges container."""
    return edges.execute(f"curl -fs --max-time {REQUEST} {bh_proxy(worker)} http://{svc(target)}{path}")[1]


def bh_code(worker: str, target: str) -> str:
    return edges.execute(f"curl -s -o /dev/null --max-time {REQUEST} -w '%{{http_connect}}' {bh_proxy(worker)} http://{svc(target)}/")[1]


def code(vhost: str, timeout: int = REQUEST, extra: str = "") -> str:
    return edges.execute(f"curl -s -o /dev/null -w '%{{http_code}}' --max-time {timeout} {extra} https://{vhost}/")[1]


def via_edge(vhost: str, timeout: int = REQUEST) -> tuple[str, str]:
    """("edgeN", machine id) of one request to 127.0.0.1:80."""
    r = edges.execute(f"curl -fsi --max-time {timeout} https://{vhost}/ | tr -d '\\r'")[1]
    e = re.search(r"(?im)^x-edge: (\S+)", r)
    return (e.group(1) if e else "", id_of(r.split("\n\n", 1)[-1]))


def edge_state(e: int) -> dict:
    """Edge e's /state: its subscription rows, routes, and the instances it has ejected for now."""
    return json.loads(edges.succeed(f"curl -fs http://127.0.0.1:910{e}/state"))


def edges_have(ids: set[str]) -> bool:
    """Both edges' subscriptions hold rows with these instance ids (whether they route them or not)."""
    return all(ids <= {r[0] for r in edge_state(e)["rows"]} for e in [1, 2])
