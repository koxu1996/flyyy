# Shared by every test (tests/lib.nix prepends it): result bookkeeping, polling, the registry's images.
import json, re, time

fails: list[str] = []
# The suite's start, for its wall time (done() prints it); SUITE is its name (tests/lib.nix sets it).
T0 = time.time()


def ok(m: str) -> None:
    print("ok   " + m)


def bad(m: str) -> None:
    print("FAIL " + m)
    fails.append(m)


def note(m: str) -> None:
    print("note " + m)


def shown(label: str, out: str) -> None:
    """A command's output in the log, as its user saw it (the host apps' suites)."""
    print(f"out  {label}:\n" + "\n".join("     | " + l for l in out.rstrip("\n").splitlines()))


def check(cond: object, good: str, failed: str) -> None:
    ok(good) if cond else bad(failed)


def done() -> None:
    print(f"wall {SUITE} {time.time() - T0:.0f} s")
    assert not fails, f"{len(fails)} checks failed: {fails}"


# How long a suite waits, by what for (seconds); generous, as the build sandbox shares a few CPUs between
# every container. A wait for something else names its own.
# One request, or a quick check that should hold at once.
REQUEST = 5
# A slow request: through a restart or a deploy, or a dashboard call that waits on the control plane.
SLOW_REQUEST = 15
# A change propagating: through Corrosion to the edges and backhauls, a check's state, a peer or a route.
PROPAGATE = 15
# An agent or the control plane acting on it: a machine stopped or destroyed, checks passing, a restart.
ACT = 30
# A machine or a node coming up: a VM booted, a node joined the mesh, a worker alive again.
BOOT = 60
# A deploy, scale or destroy with the images already pulled (the CLI's --timeout, and converged).
DEPLOY = 180
# A deploy that pulls to cold workers (the seed apps, single-host mode), and the storm run.
COLD_DEPLOY = 300

REGISTRY = "10.99.0.3:5000"
DIGESTS: dict[str, str] = {}
ID = re.compile(r"\b[a-z][a-z0-9]*-[0-9a-f]{6}\b")


def until(f, timeout: float = 10, step: float = 0.25) -> bool:
    end = time.time() + timeout
    while True:
        if f():
            return True
        if time.time() > end:
            return False
        time.sleep(step)


def image(name: str) -> str:
    return f"{REGISTRY}/{name}@{DIGESTS[name]}"


def id_of(body: str) -> str:
    m = ID.search(body)
    return m.group(0) if m else ""
