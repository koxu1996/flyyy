# The WireGuard mesh: join, approve, and what to dump when a join stalls.
def mesh_nodes() -> dict[str, dict]:
    return {n["name"]: n for n in json.loads(fly("mesh --json"))["nodes"]}


def mesh_start(m, token: str) -> None:
    """A node's first join: the token where its mesh service (`flyyy join`) looks, then start it."""
    m.succeed(f"printf %s {shlex.quote(token)} | install -o mesh -g mesh -m 600 /dev/stdin /var/lib/flyyy-mesh/join-token")
    m.succeed("systemctl start mesh")


def stall_dump(name: str) -> str:
    """Both ends of a join that never got through: WireGuard on the control plane and on the node, the
    node's route to the control plane, and its join log."""
    m = {"edges": edges, **WORKERS}[name]
    cmds = {
        control: ["wg show flyyy0 dump", "journalctl -u control --no-pager | grep -E 'mesh|wg' | tail -40"],
        m: ["wg show all dump", "ip -4 addr", "ip route get 10.250.0.1", "journalctl -u mesh --no-pager | tail -40"],
    }
    return "\n".join(f"--- {mm.name}: {c}\n{mm.execute(c)[1]}" for mm, cs in cmds.items() for c in cs)


def mesh_approve(names: list[str]) -> None:
    """Approve each (in this order, so the addresses are 10.250.1.1 up in it) once it has joined, then wait
    until all of them switched to their own keys."""
    for name in names:
        assert until(lambda: mesh_nodes().get(name, {}).get("state") == "pending", BOOT, 0.5), f"mesh: {name} never joined\n{stall_dump(name)}"
        fly(f"approve {name}")
    assert until(lambda: all(mesh_nodes()[n]["state"] == "joined" for n in names), BOOT, 0.5), f"mesh: {mesh_nodes()}"
    MESH.update({n: v["ip"] for n, v in mesh_nodes().items()})


def mesh_join(m, name: str) -> None:
    """One more node into the mesh (worker4): a new token, join, approve."""
    mesh_start(m, fly("token").strip())
    mesh_approve([name])


def underlay_up(m) -> None:
    """The node's underlay address is there before it joins. A node without one joins forever without a
    word from WireGuard (flyyy join now says so after 20 s): it once happened here, when the containers
    still shared one cgroup tree and worker2's address unit failed to start (tests/lib.nix ownCgroup)."""
    m.wait_until_succeeds("ip -4 -o addr show dev eth1 | grep -q 'inet 10\\.99\\.'", timeout=ACT)
