# A mock control-plane API for the dashboard page, with long, worst-case values. argv: page path, port.
# GET /__scenario/<name> switches what it serves: full (the default: apps, ten workers, two edges), bare
# (no app, no node), pending (no app, a worker waiting for approval, an edge), ready (no app, a worker and
# an edge).
import json, re, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
PAGE = open(sys.argv[1], "rb").read()
def worker(n, alive=True, age=3_000_000):
    return {"name": f"worker{n}", "backhaul": f"10.250.1.{100+n}:4280", "alive": alive, "valid": True, "heartbeat_age_ms": age if not alive else 2000,
            "uptime_ms": 9e7, "single_host": False, "machines": 12, "arch": "aarch64", "cpus": 128, "memory": 1_048_576,
            "allocated_cpus": 140, "allocated_memory": 1_200_000, "image_bytes": 123_456_789_012}
WORKERS = [worker(i) for i in range(1, 9)] + [worker(9, alive=False), worker(10, alive=False, age=7_000_000)]
MESH = {"control": {"ip": "10.250.0.1", "key": "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=", "api": "10.250.0.1:4000", "hostname": "control-plane-host.example.internal"},
        "unused_tokens": 3,
        "nodes": [{"name": f"worker{i}", "role": "worker", "ip": f"10.250.1.{100+i}", "state": "joined", "endpoint": f"[2001:db8:1234:5678::{i}]:51820",
                   "advertised_endpoint": None, "peer_endpoint": f"[2001:db8:1234:5678::{i}]:51820", "handshake_age_s": 3500, "hostname": f"very-long-hostname-of-worker-{i}.home.example.net"} for i in range(1, 11)]
        + [{"name": f"edge{i}", "role": "edge", "ip": f"10.250.1.{200+i}", "state": "joined", "endpoint": f"edge-{i}.some-very-long-domain.example.com:51820",
            "advertised_endpoint": f"edge-{i}.some-very-long-domain.example.com:51820", "peer_endpoint": "x", "handshake_age_s": 12, "hostname": f"edge-host-{i}.example.com",
            "listen": [listen]} for i, listen in ((1, "[::]:443"), (2, "0.0.0.0:8443"))]}
SVC = [{"name": "http", "protocol": "http", "port": 8080, "edge_port": 443, "passing": 11, "soft_limit": 100, "hard_limit": 200},
       {"name": "tcp9000", "protocol": "tcp", "port": 9000, "edge_port": 9000, "passing": 12}]
def machines():
    return [{"id": f"averyveryverylongappname-{i:06x}", "worker": f"worker{i % 10 + 1}", "desired": "started", "state": "started" if i % 4 else "failed",
             "addr": "vsock:1:2", "error": "" if i % 4 else "the VM exited: cloud-hypervisor: an error message long enough to wrap onto more lines than one, maybe two"} for i in range(12)]
APP = {"name": "averyveryverylongappname", "image": "registry.example.com/some/org/a-really-long-image-name@sha256:" + "a" * 64, "port": 8080, "cmd": [], "env": ["SECRET"],
       "count": 12, "cpus": 4, "memory": 2048, "image_bytes": 987_654_321, "org": "", "services": SVC,
       "ips": [{"ip": "10.200.0.1", "type": "shared"}, {"ip": "10.200.1.17", "type": "dedicated"}], "state": "active", "started": 9, "converged": True}
TOKEN = {"token": "flyyy-join:yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=:10.250.255.1:yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=@203.0.113.7:51820", "ip": "10.250.255.1"}
def node(name, role, state, i):
    return {"name": name, "role": role, "ip": None if state == "pending" else f"10.250.1.{i}", "state": state, "endpoint": "203.0.113.9:51820", "advertised_endpoint": None,
            "peer_endpoint": "", "handshake_age_s": 5, "hostname": name, "listen": ["[::]:443"] if role == "edge" and state != "pending" else []}
def mesh_of(*nodes):
    return {"control": MESH["control"], "unused_tokens": 0, "nodes": list(nodes)}
SCENARIOS = {
    "bare": {"/v1/workers": [], "/v1/mesh": mesh_of(), "/v1/apps": []},
    "pending": {"/v1/workers": [], "/v1/mesh": mesh_of(node("nuc", "worker", "pending", 1), node("edgevps1", "edge", "joined", 2)), "/v1/apps": []},
    "ready": {"/v1/workers": [worker(1) | {"name": "nuc"}], "/v1/mesh": mesh_of(node("nuc", "worker", "joined", 1), node("edgevps1", "edge", "joined", 2)), "/v1/apps": []},
}
ROUTES = {"/config.json": {"apps_url": "https://{app}.flyyy.localhost/", "flake_url": "git+https://github.com/koxu1996/flyyy?ref=main", "auth": "local"},
          "/templates.json": [], "/v1/workers": WORKERS, "/v1/mesh": MESH, "/v1/whoami": {"operator": True},
          "/v1/orgs": [], "/v1/users": [], "/v1/apps": [APP], f"/v1/apps/{APP['name']}": APP | {"machines": machines()}}
FULL = dict(ROUTES)
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        p = self.path.split("?")[0]
        if p.startswith("/__scenario/"):
            ROUTES.clear(); ROUTES.update(FULL); ROUTES.update(SCENARIOS.get(p.rsplit("/", 1)[1], {}))
            return self.send(200, "application/json", b"{}")
        if p == "/":
            return self.send(200, "text/html; charset=utf-8", PAGE)
        if p in ROUTES:
            return self.send(200, "application/json", json.dumps(ROUTES[p]).encode())
        self.send(404, "application/json", b'{"error":"not mocked"}')
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        if self.path in ("/v1/mesh/tokens", "/v1/mesh"):
            return self.send(200, "application/json", json.dumps(TOKEN).encode())
        self.send(404, "application/json", b'{"error":"not mocked"}')
    def send(self, code, ctype, body):
        self.send_response(code); self.send_header("content-type", ctype); self.send_header("content-length", str(len(body))); self.end_headers(); self.wfile.write(body)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
