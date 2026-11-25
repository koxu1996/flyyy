# Each worker's agent API (agent/api.rs): mTLS on its mesh address, the control plane's certificate only.
AGENT_PORT = 4281
CONTROL_TLS = "/var/lib/flyyy-tls"
# machine_create's machines: id -> worker.
PUT_ON: dict[str, str] = {}


def agent(worker: str, method: str, path: str, body=None, cert: bool = True) -> tuple[int, str]:
    """A call to a worker's agent API from the control plane's container, with its certificate (or none):
    (HTTP status, body). Status 0: no answer (a refused handshake, say)."""
    ip = MESH.get(worker) or mesh_nodes()[worker]["ip"]
    creds = f"--cert {CONTROL_TLS}/cert.pem --key {CONTROL_TLS}/key.pem" if cert else ""
    data = f"-d {shlex.quote(json.dumps(body))}" if body is not None else ""
    out = control.execute(f"curl -s --max-time {SLOW_REQUEST} -w '\\n%{{http_code}}' --cacert {CONTROL_TLS}/ca.pem {creds} -X {method} {data} https://{ip}:{AGENT_PORT}{path}")[1]
    text, _, code = out.rpartition("\n")
    return int(code or 0), text


def machine_create(worker: str, app: str, img: str, port: int, cmd: list = [], mid: str = "", cpus: int = 1, memory: int = 128, start: bool = True,
                   env: dict = {}, soft_limit: int = 0, hard_limit: int = 0) -> str:
    """A machine the control plane doesn't manage, put straight on a worker through its agent API (one http
    service, with these limits; 0: none)."""
    mid = mid or "m-" + control.succeed("od -An -N4 -tx1 /dev/urandom | tr -d ' \\n'")
    cmd = [a.replace("{id}", mid) for a in cmd]
    service = {"name": "http", "protocol": "http", "port": port} | ({"soft_limit": soft_limit} if soft_limit else {}) | ({"hard_limit": hard_limit} if hard_limit else {})
    spec = {"id": mid, "app": app, "image": img, "cmd": cmd, "env": env, "port": port, "cpus": cpus, "memory": memory, "services": [service]}
    code, out = agent(worker, "PUT", f"/v1/machines/{mid}", spec)
    assert code == 200, f"machine_create {mid} on {worker}: {code} {out}"
    PUT_ON[mid] = worker
    place(mid, app, worker, port, soft_limit, hard_limit)
    if start:
        machine_set(mid, "started")
    return mid


def place(mid: str, app: str, worker: str, port: int, soft_limit: int = 0, hard_limit: int = 0) -> None:
    """Its placement row, as the control plane writes it for its own: the edges route only placed instances."""
    tx([["INSERT INTO placement (id, machine, app, worker, protocol, port, edge_port, soft_limit, hard_limit) VALUES (?, ?, ?, ?, 'http', ?, 0, ?, ?)",
         [f"{mid}.http", mid, app, worker, port, soft_limit, hard_limit]]])


def machine_set(mid: str, state: str) -> None:
    """Start, stop or destroy one of those."""
    method, path = {"started": ("POST", f"/v1/machines/{mid}/start"), "stopped": ("POST", f"/v1/machines/{mid}/stop"),
                    "destroyed": ("DELETE", f"/v1/machines/{mid}")}[state]
    code, out = agent(PUT_ON[mid], method, path)
    assert code == 200, f"machine_set {mid} {state}: {code} {out}"
