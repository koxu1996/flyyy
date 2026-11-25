# local: the single-machine stack. alice, an ordinary lingering user, runs flyyy-local-control, -worker and
# -edge (`nix run .#local-control/.#local-worker/.#local-edge`) with their defaults, as units of her own systemd user
# instance. Nothing of the stack runs as root; the VMs are alice's user units too.
AS_ALICE = "runuser -u alice -- env XDG_RUNTIME_DIR=/run/user/1000"
TOKEN = "/home/alice/.local/state/flyyy/control/control.token"
FLY_LOCAL = f"{AS_ALICE} FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE={TOKEN} flyyy"

for m in [registry, local]:
    m.start()
registry.wait_for_open_port(5000)
DIGESTS.update(json.loads(registry.succeed("cat /etc/flyyy-images.json")))
local.wait_for_unit("user@1000.service")
for role in ["control", "worker", "edge"]:
    local.succeed(f"{AS_ALICE} systemd-run --user --unit=flyyy-{role} flyyy-local-{role}")

ws: list = []
def worker_alive() -> bool:
    global ws
    rc, out = local.execute(f"{FLY_LOCAL} workers --json")
    ws = json.loads(out) if rc == 0 else []
    return any(w["name"] == "worker1" and w["alive"] for w in ws)


check(until(worker_alive, DEPLOY), f"local: worker1 announced through the local gossip ({ws[0]['backhaul'] if ws else '-'})", f"local: workers {ws}")
out = local.succeed(f"{FLY_LOCAL} workers")
check(out.startswith("single-host (dev) mode") and all(w["single_host"] for w in ws), "local: single-host (dev) mode, shown by flyyy workers", f"local: workers {out}")
rc, out = local.execute(f"{FLY_LOCAL} init 2>&1")
check(rc != 0 and "409" in out and "single-host" in out, "local: flyyy init refused in single-host mode (no mesh)", f"local: init {rc} {out}")
rc, out = local.execute(f"{FLY_LOCAL} deploy whoami --image {image('traefik/whoami')} --port 80 --count 2 --timeout {COLD_DEPLOY} 2>&1")
check(rc == 0, "local: deploy whoami x2 converged", f"local: deploy: {rc} {out}")
seen: set = set()
def both() -> bool:
    rc, out = local.execute(f"curl -fs --max-time {REQUEST} http://whoami.flyyy.localhost:8000/")
    if rc == 0:
        seen.add(id_of(out))
    return len(seen) == 2
check(until(both, ACT), f"local: the edge on 127.0.0.1:8000 spreads over both machines ({' '.join(sorted(seen))})", f"local: edge saw {seen}")

units = local.succeed(f"{AS_ALICE} systemctl --user list-units --plain --no-legend 'fc-*' | awk '$4 == \"running\" {{print $1}}'").split()
check(len(units) == 2, f"local: the VMs are alice's user units ({' '.join(units)})", f"local: user units {units}")
procs = local.succeed("ps -eo user:20,comm | awk '$2 ~ /^(corrosion|proxy|cloud-hyperviso)$/' | sort | uniq -c").strip()
users = {l.split()[1] for l in procs.splitlines()}
check(users == {"alice"}, f"local: corrosion, proxy and cloud-hypervisor all run as alice ({'; '.join(l.strip() for l in procs.splitlines())})", f"local: processes {procs}")

local.succeed(f"{FLY_LOCAL} destroy whoami --timeout {DEPLOY}")
check(until(lambda: not local.succeed(f"{AS_ALICE} systemctl --user list-units --all --plain --no-legend 'fc-*'").strip(), PROPAGATE),
      "local: destroy stopped and removed both VM units", "local: VM units left after destroy")
# Leaving a role stops all its processes.
local.succeed(f"{AS_ALICE} systemctl --user stop flyyy-worker")
check(local.execute("pgrep -u alice -f 'proxy (agent|worker) '")[0] != 0, "local: stopping flyyy-worker stopped its agent and backhaul", "local: worker processes left")
