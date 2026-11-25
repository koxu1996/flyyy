# The CLI on the control plane, and what it says about apps. Machines: worker1-4, control, edges, registry.
# Between nodes only the WireGuard mesh: Corrosion APIs and /state are queried on their own node.
import shlex
# The worker containers by node name (nothing reads a number out of a name); NAMES3, the three every suite starts.
WORKERS = {"worker1": worker1, "worker2": worker2, "worker3": worker3, "worker4": worker4}
NAMES3 = ["worker1", "worker2", "worker3"]
FLY = "FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE=/etc/control.token flyyy"
# Mesh node name -> its address on the mesh (boot() and mesh_join() fill it).
MESH: dict[str, str] = {}


def fly(args: str) -> str:
    return control.succeed(f"{FLY} {args}")


def fly_rc(args: str) -> tuple[int, str]:
    return control.execute(f"{FLY} {args} 2>&1")


def status(app: str) -> dict:
    return json.loads(fly(f"status {app} --json"))


def live(app: str) -> list[tuple[str, str]]:
    return [(m["worker"], m["id"]) for m in status(app)["machines"] if m["desired"] != "destroyed"]


def on(worker: str, app: str) -> list[str]:
    return sorted(mid for w, mid in live(app) if w == worker)

