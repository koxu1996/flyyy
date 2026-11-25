# The join app (`nix run .#join`, nix/join.nix) on a NixOS machine without flyyy's module (`plain`): one
# command with a token, which carries the control plane's address; approve; the worker announces itself,
# opens the machine's firewall to the mesh and serves; run again without a token, it resumes. Every part
# runs as bob, nothing as root. The app itself runs as root with --user bob: the build sandbox can't make
# sudo's setuid wrapper, so its sudo path isn't covered here.
PARTS = ["mesh", "corrosion", "agent", "backhaul"]
JOIN = "flyyy-join --user bob"


def units_active() -> bool:
    return all(plain.execute(f"systemctl is-active -q flyyy-{p}")[0] == 0 for p in PARTS)


def flyyy_procs() -> list[tuple[str, str]]:
    """(user, command) of the worker's processes: the units' scripts, flyyy, proxy, Corrosion, Cloud Hypervisor
    (its comm, cut to 15 characters: cloud-hyperviso)."""
    out = plain.succeed("ps -eo user:20=,comm= | grep -E ' (flyyy|proxy|corrosion|cloud-hyperviso)' || true")
    return [(u, c) for u, c in (l.split(None, 1) for l in out.splitlines() if l.strip())]
WORKERS["plain"] = plain


def wk(name: str) -> dict:
    return next((w for w in json.loads(fly("workers --json")) if w["name"] == name), {})


plain.start()
plain.wait_for_unit("multi-user.target")
underlay_up(plain)
check(plain.execute("systemctl cat mesh agent")[0] != 0, "plain: no flyyy module (no mesh or agent unit)", "plain has flyyy's units")

# 1. One command: the token is all the app needs. No --name: the node is named after its host, plain.
tok = fly("token").strip()
check(tok.endswith("@10.99.0.2:51820"), "the join token carries the control plane's endpoint", f"token {tok}")
rc, out = plain.execute(f"flyyy-join {shlex.quote(tok)} 2>&1")
check(rc != 0 and "--user NAME" in out, "join app as root without --user: refused (nothing runs as root)", f"as root: exit {rc}\n{out}")
rc, out = plain.execute(f"{JOIN} {shlex.quote(tok)} 2>&1")
shown('#join, a fresh join', out)
lines = out.splitlines()
check(rc == 0 and lines[-2:] == ["✓ plain joined through 10.99.0.2:51820", "next: approve plain in the dashboard (or flyyy approve plain)"]
      and len(lines) <= 3 and "--nixos" not in out and "/dev/kvm" not in out and "bob" not in out and "services.flyyy" not in out,
      "join app: units started; a short result: joined, approve plain next (no --nixos hint)", f"join app: exit {rc}\n{out}")
rc, out = plain.execute(f"{JOIN} --nixos 2>&1")
check(rc == 0 and "services.flyyy.worker = { enable = true; name = \"plain\"; }" in out and 'controlAddress = "10.99.0.2:51820"' in out and "✓" not in out,
      "join app --nixos: the module snippet for plain (its control plane from the join), and nothing else", f"join --nixos: exit {rc}\n{out}")
check(units_active(), "join app: flyyy-mesh, -corrosion, -agent and -backhaul active", "join app: not all units active")
users = {p: plain.succeed(f"systemctl show -P User flyyy-{p}").strip() for p in PARTS}
caps = {p: plain.succeed(f"systemctl show -P AmbientCapabilities flyyy-{p}").strip() for p in PARTS}
check(set(users.values()) == {"bob"} and caps["mesh"] == "cap_net_admin" and not any(caps[p] for p in PARTS[1:]),
      "join app: every unit is User=bob; only flyyy-mesh has CAP_NET_ADMIN", f"join app: users {users}, ambient caps {caps}")
check(plain.execute("test -e /var/lib/systemd/linger/bob && systemctl is-active -q user@1000")[0] == 0, "join app: bob lingers and its systemd runs (for the VMs)", "join app: bob doesn't linger")
mesh_approve(["plain"])
check(until(lambda: wk("plain").get("alive") is True, BOOT), f"join app: plain alive, backhaul {wk('plain').get('backhaul')}", f"join app: plain {wk('plain')}")
check(plain.execute("iptables -C nixos-fw -i flyyy0 -j nixos-fw-accept")[0] == 0, "join app: NixOS's firewall lets the mesh in", "join app: no firewall rule for flyyy0")

# 2. It serves: one more whoami goes to plain (it has none), and the edges route to it.
fly(f"scale whoami --count 7 --timeout {DEPLOY}")
new = on("plain", "whoami")
check(len(new) == 1, f"scale 6 -> 7 put the new machine on plain ({' '.join(new)})", f"plain has {new}")
check(until(lambda: any(via_edge("whoami.flyyy.localhost")[1] == new[0] for _ in range(14)), PROPAGATE),
      f"the edges route whoami to {new[0]} on plain", f"no request reached {new[0]}")

procs = flyyy_procs()
root = [c for u, c in procs if u == "root"]
names = {c for u, c in procs if u == "bob"}
check(not root and {"flyyy", "proxy", "corrosion", "cloud-hyperviso"} <= {n if n.startswith("cloud-") else n.split("-")[0] for n in names},
      f"no flyyy process runs as root; bob runs {', '.join(sorted(names))}", f"processes: {procs}")
vm = plain.succeed("runuser -u bob -- env XDG_RUNTIME_DIR=/run/user/1000 systemctl --user list-units --plain --no-legend 'fc-*'")
check(new[0] in vm, f"{new[0]}'s VM is a unit of bob's systemd", f"bob's units: {vm}")

# 3. Again with a token: refused, it joined already. Without one: it resumes, with the same address.
rc, out = plain.execute(f"{JOIN} {shlex.quote(fly('token').strip())} 2>&1")
check(rc != 0 and "already joined as plain" in out, "join app with a second token: refused, already joined", f"second token: exit {rc}\n{out}")
ip = MESH["plain"]
plain.succeed("systemctl stop flyyy-worker.slice")
check(until(lambda: wk("plain").get("alive") is False, BOOT), "flyyy-worker.slice stopped: plain dead", f"plain {wk('plain')}")
rc, out = plain.execute(f"{JOIN} 2>&1")
shown('#join, a resume', out)
check(rc == 0 and "flyyy-join: resuming plain" in out and "✓ plain running again (approved before)" in out, "join app without a token: resumes plain",
      f"resume: exit {rc}\n{out}")
check(until(lambda: wk("plain").get("alive") is True, BOOT) and mesh_nodes()["plain"]["ip"] == ip and units_active(),
      f"resume: plain's units active and alive again at {ip}", f"resume: {wk('plain')}")
check(until(lambda: any(via_edge("whoami.flyyy.localhost")[1] == new[0] for _ in range(14)), PROPAGATE),
      f"resume: {new[0]} still served (its VM outlived the unit)", f"resume: no request reached {new[0]}")
