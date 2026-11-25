# The cluster suites' start: boot() then seed().
def boot() -> None:
    """The cluster on its underlay, then the mesh: `flyyy init` on the control plane, a token per node,
    `flyyy join` on each, `flyyy approve` each. Everything between nodes then runs on the mesh."""
    for m in [worker1, worker2, worker3, control, edges, registry]:
        m.start()
    registry.wait_for_open_port(5000)
    DIGESTS.update(json.loads(registry.succeed("cat /etc/flyyy-images.json")))
    for m in [worker1, worker2, worker3, control, edges]:
        m.wait_for_unit("multi-user.target")
        underlay_up(m)
    control.wait_until_succeeds("curl -s -o /dev/null http://127.0.0.1:4000/", timeout=BOOT)
    nodes = [(worker1, "worker1"), (worker2, "worker2"), (worker3, "worker3"), (edges, "edges")]
    tokens = [fly("init").strip()] + [fly("token").strip() for _ in nodes[1:]]
    # The control plane's Corrosion first: the nodes bootstrap from it, and Corrosion's announcer backs
    # off (5 s up to 2 min) after a failed bootstrap, so a node that comes up before it can wait minutes.
    control.wait_until_succeeds(f"{FLY} ls", timeout=DEPLOY)
    for (m, _), t in zip(nodes, tokens):
        mesh_start(m, t)
    mesh_approve([name for _, name in nodes])
    # The workers announce themselves once their Corrosion is on the mesh: seed only when all three are
    # live, or placement spreads the seed apps over fewer of them.
    control.wait_until_succeeds(f"{FLY} workers --json | jq -e '[.[] | select(.alive)] | length == 3'", timeout=DEPLOY)


def seed() -> None:
    """The apps up.sh deployed: whoami x6, echo x4."""
    fly(f"deploy whoami --image {image('traefik/whoami')} --port 80 --count 6 --timeout {COLD_DEPLOY}")
    fly(f"deploy echo --image {image('hashicorp/http-echo')} --port 5678 --count 4 --timeout {COLD_DEPLOY} -- '-text={{id}}'")
    edges.wait_until_succeeds("curl -fs https://whoami.flyyy.localhost/ && curl -fs https://echo.flyyy.localhost/", timeout=ACT)
    ok("seeded whoami x6 and echo x4 from the local registry")
