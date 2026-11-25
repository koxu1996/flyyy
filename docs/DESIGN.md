# Design notes

How the pieces work and why, newest last. The spike and stages 2–10 are in docs/history/stages.md, and the prompts that drove them in docs/history/.

# Rootless worker, and `nix run` per role

## Worker
- Nothing on a worker runs as root:
  - **flyyy:** the agent runs as the `flyyy` user. It is lingering (uid 2000, in the `kvm` group), so its
    systemd user instance runs without a login.
  - **VMs:** each VM is a `systemd-run --user` unit of that instance, named `fc-<id>`, with `UMask=0007`.
    VMs outlive an agent crash because they are units, not children of the agent.
  - **Firecracker** needs only `/dev/kvm`, since VMs have no NIC.
  - **Image builds** (crane, mkfs.erofs) run as the agent's own user.
  - **Corrosion** runs with `DynamicUser`.
- Removed: the `firecracker@` template (its `ip_forward` write, `rm` and `chgrp` hooks) and setpriv.
- **The backhaul** is still its own user. Its only extra right is the `flyyy` group, which reaches the vsock
  sockets (0770, in `/run/flyyy` 0750). The agent's state (`/var/lib/flyyy`, flyyy's home) stays 0700.
- **Paths:** `FLYYY_STATE_DIR` (default `/var/lib/flyyy`) and `FLYYY_RUN_DIR` (default `/run/flyyy`, shared
  with the backhaul) replace the fixed `/var/lib/machine-agent` and `/run/fc`. The agent's `systemctl --user`
  finds the user instance through `XDG_RUNTIME_DIR`.
- **Stopping:** a transient unit is unloaded once it exits, so "not loaded" counts as stopped. Before each
  start, a `reset-failed` frees the name from an earlier failed run, whose unit is kept for its `Result`.
- **Tests:** in the test containers, `user@.service` runs without PAM, because its `pam_unix` account check
  needs the setuid `unix_chkpwd`, which the build sandbox can't make. The tests read VM consoles with
  `journalctl _SYSTEMD_USER_UNIT=fc-<id>.service`.

## `nix run .#local-control`, `.#local-worker`, `.#local-edge`
- (Called `.#control`, `.#worker`, `.#edge` until those names went to the control host's rootless apps; see the last section.)
- Each runs Corrosion plus its role as plain processes of the calling user, with no root and no NixOS.
- **Flags:**
  - all roles: `--name`, `--gossip`, `--api` (Corrosion), `--bootstrap`, `--state-dir`;
  - control: `--listen` and `--token-file` (made if missing);
  - worker: `--backhaul` and `--backhaul-state`;
  - edge: `--listen` and `--edge-state`.
- **Defaults** don't collide, so all three run on one machine:

  | role | gossip | Corrosion API | serves |
  |---|---|---|---|
  | control | 127.0.0.1:8787 | 127.0.0.1:8081 | API on :4000 |
  | worker | 127.0.0.1:8788 | 127.0.0.1:8082 | backhaul :4280, state :9080 |
  | edge | 127.0.0.1:8789 | 127.0.0.1:8083 | apps on http://<app>.flyyy.localhost:8000/, state :9100 |

  Each bootstraps from the other two.
- **State** lives in `~/.local/state/flyyy/<name>`. The control plane prints the `FLYYY_API` and
  `FLYYY_TOKEN_FILE` the CLI needs.
- **The worker** needs `/dev/kvm` and a systemd user instance (a login session, or `loginctl enable-linger`).
  Its VMs are the caller's own `fc-*` user units.
- shortcut: a role's processes run in the foreground of one shell, with no supervision or restarts. If one
  exits, the rest are stopped.
- **The `local` suite** checks this end to end. An ordinary lingering user runs the three roles with their
  defaults in one container: whoami ×2 from the test registry, served round robin by the edge on :8000. The
  VMs are that user's units; Corrosion, proxy and Firecracker all run as that user; destroy removes the units.

## Storm fix
- The storm generator still wrote stage-8-era TCP addrs (`10.100.N.200:<port>`). Since stage 9 the backhauls
  skip those rows as invalid, so storms never actually built backhaul routes from them. The order and gap
  audit (which reads the `change` lines) and the final state (real vsock addrs) didn't notice. The rows now
  carry `vsock:<id>:<seq>`.

# WireGuard mesh, run by the control plane

## Shape
- **Interface:** `flyyy0` on `10.250.0.0/16`, UDP 51820, on the kernel module.
  - The control plane is `10.250.0.1`. Approved nodes get `10.250.1.N` in approval order, and join tokens
    use `10.250.255.N`.
  - Shared code: `proxy/src/wg.rs`, which shells out to `ip` and `wg`.
  - Each node keeps a peer for every other node and for the control plane.
- **Privileges:** only `CAP_NET_ADMIN`.
  - The control process owns the control plane's interface (`AmbientCapabilities`, `DynamicUser`).
  - On a node, `flyyy join` runs as the `mesh` user and keeps running: it brings the interface back after a
    reboot and polls the control plane's peer list every 5 s.
- **Mesh state** lives in the control plane's state directory: `mesh.key` (0600) and `mesh.json` (tokens
  by their public half, and nodes). It isn't in Corrosion, which itself runs over the mesh.
- **What moved onto the mesh:**
  - Corrosion gossip binds the node's mesh address. Its config is a template filled at start, and every
    agent bootstraps from the control plane's `10.250.0.1:8787`. `gossip.client_addr` puts its outgoing
    QUIC on the mesh address too: by default that's `[::]:0`, which left 8 UDP sockets per agent on every
    interface. The mesh suite's "nothing on the LAN" check found them.
  - The backhaul listens on, and is announced at, the worker's mesh address.
  - The control API is on `10.250.0.1:4000`, plus `127.0.0.1:4000` for the CLI on the control host.
  - Corrosion APIs and the backhaul's `/state` are `127.0.0.1` only.
  - Edges and the control plane refuse a backhaul that isn't on the mesh (or loopback, for the single-machine
    `nix run` stack). The worker's address comes from its own `workers` row, so it is checked, not trusted.
- **Removed shortcuts:** plaintext QUIC gossip and the unauthenticated backhaul on the LAN. Both still have
  no auth of their own, but now only run inside WireGuard.

## Join
1. **`flyyy init`** creates the control plane's key and interface, and prints the first token. **`flyyy
   token`** prints more. A token is `flyyy-join:<bootstrap private key>:<10.250.255.N>:<control plane
   public key>`, then `@<host>:<port>` when the control plane runs with `--endpoint`
   (`services.flyyy.control.endpoint`): where nodes' WireGuard reaches it, so the token is all a join
   needs. The control plane keeps only the bootstrap public key, as a peer limited to that one address.
   Unused tokens expire after 24 h.
2. **`flyyy join [<host>] --token …`** (the host only for a token without one; given, it wins) comes up
   with the bootstrap key and makes the node's own key locally;
   that private key never leaves the node. It posts the public key to `POST /v1/mesh/join` over the
   tunnel. WireGuard ties a source address to one key, so the source address identifies the token, and
   that request spends it. A second join from the same address gets 403.
3. **While pending,** the node's only peer is the control plane. The control plane's mesh routes accept a
   token's address only for `/v1/mesh/join`, and admin routes still need the bearer token.
4. **`flyyy approve <name>`** assigns the address and adds the node's own key as a peer.
5. **The switch:** the node sets its own key and address. Its first `GET /v1/mesh/peers` from there marks it
   joined, and the control plane drops the bootstrap key. The node then deletes the spent token and its key.
6. **Resuming:** every step can be resumed from `--state-dir`, and a restart after the switch needs no token.

## Tests
- `boot()` runs the real flow for worker1–3 and the edges; workers.py joins worker4 when it starts it.
- Suites reach other nodes only over the mesh (`MESH[name]`). They read Corrosion and `/state` locally on
  each node, and storm's generator now writes through worker1's own agent.
- The `mesh` suite (31 checks) covers:
  - full peer sets and handshakes;
  - nothing on the LAN but UDP 51820;
  - the token accepted once;
  - the pending restrictions;
  - the key switch, with the bootstrap key gone on both sides;
  - node-to-node traffic;
  - resuming without a token after the interface is deleted.
- **The host needs the `wireguard` module loaded** (`boot.kernelModules`), because the sandboxed containers
  can't load it.

## Shortcuts
- One mesh. No key rotation, no removing nodes, no revoking a pending join, and no expiry of approved keys.
- Every node bootstraps Corrosion from the control plane. Membership persists in Corrosion, but a node that
  has never seen the cluster needs the control plane up.
- Endpoints are the ones the control plane observed, unless a node advertises its own (`flyyy join --endpoint`, below). A node behind NAT that advertises nothing is reachable by the others only through whatever mapping its NAT made towards the control plane, which other nodes can't use with most NATs. There's no NAT traversal or relaying.

# TLS at the edge

## How it works
- `proxy edge … --apps-domain D --https ADDR --tls-cert PEM --tls-key PEM` uses an already-issued
  certificate, with no ACME. The vhost is `<app>.<D>` (default `flyyy.localhost`).
- Both listeners use SO_REUSEPORT, so several edges share them.
  - The HTTPS listener serves the apps, over TLS 1.2 and 1.3 with rustls and ring, on HTTP/1.1.
  - Plain HTTP on `<listen>` only redirects: 308 to the same host, path and query on HTTPS. A host
    outside the apps domain gets 404 and no redirect.
- **SIGHUP** (`systemctl reload`, `kill -HUP` for the app) rereads the files; new handshakes get the new
  certificate. A reload that fails is refused and the old certificate stays. A key that doesn't match
  the certificate (rustls `CertifiedKey::from_der`) counts as failing.
- The edge sets `X-Forwarded-Proto` (`https` or `http`) for the app.
- The NixOS edge takes `names.tls = { https; cert; key; }`. The `nix run .#local-edge` app takes the same flags.
- shortcut: one certificate for every vhost (a wildcard), no per-app certificates.

## Tests
- The edges use a self-signed `*.flyyy.localhost`, built at test time and trusted in the edges
  container. They read a copy in `/var/lib/edge-tls` (key 0640, group `edge`), so verify can swap it.
- Every app request in the suites is now HTTPS, verified against the trust store. verify checks:
  - the 308 redirect from both edges, keeping path and query;
  - no redirect outside the apps domain;
  - `X-Forwarded-Proto: https` at the app;
  - a reload serving a new certificate from both edges, so the old CA no longer verifies;
  - a mismatched key refused, with the previous certificate kept.

## On nixvps, with the real `*.flyyy.devpeek.io` certificate
- Ports 80/443 there belong to RKE2's ingress, so the edge ran on 18080/18443, built on nixvps for
  x86_64 (no KVM there, so the edge only). Checked from nixvps through its public address against the
  system CA store; the firewall was left alone.
- **Results:**
  - TLS 1.3, `CN=*.flyyy.devpeek.io`, issued by Let's Encrypt (YE2), valid until 27 Jan 2027, verify
    result 0, for `whoami` and another name under the wildcard;
  - `http://whoami.flyyy.devpeek.io:18080/a/b?c=1` → `308 https://whoami.flyyy.devpeek.io:18443/a/b?c=1`;
  - SIGHUP reloaded the real files, and the edge kept serving.
- With no Corrosion behind that edge, apps answered 503.

# mTLS inside the mesh

## How it works
- **The CA** is the control plane (`proxy/src/pki.rs`, rcgen). Its key (0600) and self-signed certificate
  live in its mesh directory, made at `flyyy init`.
- **Certificates:**
  - SANs: `IP:<mesh address>` (Corrosion verifies a peer by its IP, and so do the edges) and
    `URI:flyyy://<role>/<name>`.
  - EKU serverAuth and clientAuth, ECDSA P-256.
  - Lifetime `FLYYY_CERT_LIFETIME`, default 24 h, at least 60 s.
- **Issuing:**
  - `flyyy join --role worker|edge` (a node can't claim `control`) makes the key and a CSR on the node.
    The key never leaves it.
  - `POST /v1/mesh/cert` is accepted only from the node's own mesh address, which WireGuard ties to its
    key.
  - The certificate's SANs are the control plane's records (address, role it joined as, name). The CSR's
    own subject and extensions are ignored; only its signature-checked public key is used.
- **Renewal:** past half the lifetime, with a new key each time. The control plane renews its own
  certificate (role `control`, `10.250.0.1`) the same way.
- **Files:** `/var/lib/flyyy-tls/{ca,cert,key}.pem`, 0640, group `meshtls` (Corrosion, backhaul, edges).
  The key is written before the certificate, each via a temp file and rename.
- **Corrosion gossip:** TLS with client certificates required (`[gossip.tls]` with `ca_file` and
  `[gossip.tls.client]`), no plaintext. The pinned Corrosion supports that: `WebPkiClientVerifier` on the
  server side, and the client presents its certificate. It reads the files at start only, so a path unit
  restarts it when `cert.pem` changes.
- **The backhaul:** TLS, accepting only client certificates from the mesh CA whose URI SAN says `edge`
  (rustls `WebPkiClientVerifier` plus a role check). Edges verify the worker by its mesh IP and present
  their own certificate. Both reload a renewed certificate on their own (mtime polling). The backhaul
  also refuses to listen anywhere but a mesh node address or loopback, never a wildcard.
- **Unchanged:** the CLI and dashboard keep the bearer token.
- **The single-machine `nix run` stack** has no mesh, so no TLS directory: plain on loopback.
- shortcut: no revocation (short lifetimes instead), and the CA key never rotates.

# Worker capacity and allocation

- **Capacity:** each heartbeat reports the worker's arch (Rust's names: `aarch64`, `x86_64`), its CPUs
  (`available_parallelism`, so affinity and cgroup limits count) and memory in MiB (`MemTotal`), read once
  at start. They are new `workers` columns, and constant values keep heartbeats from churning the edges'
  subscriptions.
- **`GET /v1/workers`** adds `arch`, `cpus`, `memory`, and `allocated_cpus` / `allocated_memory`: the
  sizes of the worker's live machines (desired state not `destroyed`, so stopped ones count) summed.
- **`flyyy workers`** shows ARCH, CPUS and MEMORY (MIB) as allocated/capacity, e.g. `14/32`.
- shortcut: overcommit is allowed and placement still ignores capacity; the numbers are informational.
- **Tests:**
  - the workers suite checks each worker's numbers against its own `nproc` and MemTotal, and that the
    allocation matches its machines;
  - for overcommit, two stopped 8 CPU / 16 GiB machines take worker1 to `20/8` CPUs in both the API and
    the CLI, and destroying them gives the allocation back.

# Image sizes

- **Reporting:** each agent, every 5 s, sizes its finished `images/<hex>.erofs` files (not `.tmp`) and
  writes the `images` table (PK worker, digest; that agent is the only writer). It writes only the rows
  whose size changed, reading the table first, so a restart writes nothing. A file that's gone is written
  once as 0 bytes (no deletes).
- **API:** an app's `image_bytes` is the largest size reported for its image's digest (each worker builds
  its own copy), null before any finished it. A worker's `image_bytes` is its images summed.
- **CLI:** `flyyy status` shows `image size: 11.6 MiB (12148736 bytes)`.
- **Tests (images suite):**
  - table rows against each worker's real file sizes;
  - `image_bytes` for workers and apps, and the size in `flyyy status`;
  - no writes while nothing changes, or after an agent restart;
  - a deleted image reported as 0.

# A faster test boot

Profiled with a timed copy of `boot()`. On its own the boot was about 25 s, with containers at multi-user
in 3 s and the registry push 7–9 s. But "3 workers alive" ranged from 6 s to 92 s. Corrosion's announcer
backs off from 5 s to 2 min after a failed bootstrap, and a node whose Corrosion started before the
control plane's waited for the next try.

- **Ordering:** `boot()` now waits for the control plane's Corrosion before nodes join. They only start
  Corrosion once approved (they need a certificate), so their first announcement finds a live peer.
- **Registry:** built at build time. The test images are converted to skopeo's `dir:` layout (layers
  gzipped) and served read-only straight from the store (`registryRoot`, as compat does). There's no push
  at boot; the digests are in `/etc/flyyy-images.json`.
- **Result:** boot plus seed went from 30–120 s to 13 s.
- **Per-suite costs that remain:** about 12 s of evaluation and 9 s to type-check the driver.

# Services and health checks

- **Shape:** an app has services (`services.rs`). Each has a name, http or tcp, a VM port, for tcp an
  edge port, and a check. http GETs a path and wants 2xx/3xx. With no path, it GETs `/` and wants any answer
  below 500: the app speaks HTTP, since plenty of images have no page at `/`. tcp wants the connection kept
  open. Each check has an interval and a timeout (defaults 5 s and 2 s). `--port N` is one http service named
  `http`, and so is a row without services (older apps, machines without an app).
- **Limits:**
  - at most 16 services per app, and at most one http service (it's the app's vhost);
  - names and ports are unique;
  - edge ports are unique within the app (each app has its own IP, see Ingress IPs), and 80/443 are
    reserved;
  - the API and the agent both validate.
- **Guest:** `FLYYY_PORTS` lists every service port, with one vsock forward each.
- **Checks:** each agent checks its own running machines over vsock (`health.rs`).
  - It writes `service_status` (`<machine>.<service>`: passing, failing, or down) only when a row changes.
  - It reads the table first, so a restart writes nothing.
  - A booting VM is failing, not down; down means the machine stopped running.
  - A service that isn't passing is checked every 1 s rather than every interval, so a new machine is
    routed as soon as it answers.
  - Drain: a machine about to be stopped leaves the targets first, so its services are written down.
    The agent then waits for what it can see: the rows down in its Corrosion, and no CONNECT for the
    machine at its backhaul (`last_connect_ms_ago` on the backhaul's /state) for 500 ms since. That means
    the edges stopped sending it requests. It gives up after 10 s, and logs which way it ended. An idle
    machine goes in about 1 s, a busy one when the edges have moved on.
- **Converged** (what deploy/scale wait for) now also needs every live machine's services passing.
- **Edges:** route only to passing services.
  - http is routed by Host and tcp by `tcp:<edge_port>`. Both edges listen on each edge port
    (SO_REUSEPORT) and open and close those listeners as the routes change.
  - The order is round robin, then instances on stale workers (no heartbeat in `ALIVE_MS`), then
    instances whose tunnel failed in the last 10 s (ejected; a success clears it).
  - Backhauls serve any service that isn't down. CONNECT targets are `<machine>.<service>`.
- **CLI:** `flyyy deploy --service NAME=PROTO:PORT[@EDGE][,path=..][,interval=..][,timeout=..]`
  (repeatable, not with `--port`). `flyyy status` lists the services, with N/M passing.
- **Tests (services suite):**
  - an http and a tcp service on two machines: checks pass, and both protocols are routed;
  - nothing is written in steady state;
  - a failing check leaves the route while the tcp service stays;
  - a stopped backhaul's instance is ejected after one try per edge;
  - scaling down marks services down;
  - deploy refuses two http services, a taken edge port, and `--port` with `--service`;
  - destroy closes the edge port.
- shortcut: an ejection is per edge and lasts 10 s whether or not the instance has recovered (no active
  probing from the edges).
- shortcut: the drain waits for "no new requests", not "no open connections" (those are cut when the VM
  stops), and it holds up that reconcile round.
- shortcut: tcp routing has no PROXY protocol, so the app sees the backhaul's connection, not the client.

# Orgs and users

- **Tables:**
  - `orgs` (name, state active|destroyed);
  - `users` (name, org, role admin|member, `token_sha256`);
  - `apps.org` (`""` for the operator's own apps).
  - Only the control plane API writes them. Corrosion copies every row to every node, so a user's token
    is never stored: it's printed once by `flyyy users create` (`flyyy_` + 64 hex, from /dev/urandom),
    and only its hex SHA-256 is kept.
- **Callers:**
  - The control plane's token is the operator's. It alone uses `/v1/orgs`, `/v1/users`, `/v1/workers`
    and `/v1/mesh`; a user's token gets 403 there.
  - Any other bearer token is hashed and looked up in `users`, and it counts only if the user's org is
    active. No match is a 401.
- **Apps:**
  - A user lists, sees, scales and destroys only its org's apps. Another org's app is a 404 (not 403), as
    if it weren't there, and stays untouched.
  - Names are still global (each is a vhost): a name another org holds is a 409 "taken", while it's active
    and until its machines are gone. That 409 is the one thing a user can learn about another org's app.
  - An edge-port clash names the other app only if the caller can see it.
  - A user deploys into its own org (`--org` with another org is a 400). The operator can pass
    `--org`, and otherwise keeps an app's org (`""` for a new app).
- **Orgs:**
  - `flyyy orgs create` makes an org, or brings a destroyed one back.
  - `flyyy orgs destroy` is refused (409, listing the apps) while the org has active apps. Otherwise it
    marks the org destroyed and blanks its users' hashes (nothing hashes to `""`), so their tokens stop
    working.
  - Rows are updated, never deleted, so user names are never reused.
- **CLI:** `flyyy orgs [--json] | create | destroy`, `flyyy users [--json] | create <user> --org <org>
  [--role]`, `flyyy deploy --org`; `flyyy ls` has an ORG column and `flyyy status` an `org:` line.
- **Tests (orgs suite):**
  - two orgs with one user each;
  - tokens are hashed on a worker's copy, and the token never appears there or in the control plane's log;
  - each user deploys, and each sees only its own app;
  - status, scale and destroy of the other's app are 404s, and that app is untouched;
  - the other's name gives 409, and `--org` for another org gives 400;
  - a user gets 403 on the operator's routes, and an unknown token gets 401;
  - the operator lists both apps with their orgs and scales one;
  - destroying an org is refused while it has an active app, and afterwards its user gets 401.
- shortcut: `role` is recorded and shown, but admin and member have the same rights over their org's apps.
- shortcut: no token revocation or rotation for one user (destroying the org revokes them all), and no
  user-made users: only the operator creates them.
- shortcut: an app deployed into an org while it's being destroyed survives in the destroyed org (only
  the operator reaches it); every token lookup is a Corrosion query (no cache).

# Ingress IPs

- **Pools:** the control plane's `--shared-ip A` and `--ip-pool P` (repeatable; IPv4, pools at most a
  /16, no overlaps) go to `ip_pools` (prefix, kind shared|pool, state). Prefixes the flags no longer name
  are retired.
  - Cluster: `--shared-ip 10.200.0.1 --ip-pool 10.200.1.0/24`.
  - `nix run .#local-control`: `--shared-ip 127.0.0.1 --ip-pool 127.0.1.0/24`, inside lo's 127.0.0.0/8, so the
    single-machine stack needs no route and no privilege.
- **AnyIP:** `proxy anyip` (a unit per edge, DynamicUser, CAP_NET_ADMIN only, so the edge itself gets no
  extra privilege) runs `ip route replace local <prefix> dev lo table local` for every active prefix
  that isn't already local.
- **Allocation, the control plane's only:**
  - On deploy, an app without an address gets a random free one from a pool: not shared, not allocated,
    and not released in the last 24 h. It is written to `ips` (ip, app, state allocated|released,
    allocated_at, released_at) in the same transaction as the app.
  - A mutex covers reading the leases through writing the new one, so two deploys can't get the same
    address.
  - A redeploy or scale keeps it. Destroy releases it in the destroy's own transaction.
  - An exhausted pool is a 503. A tcp service without any pool is a 400.
- **Edges:**
  - **Subscription:** each passing service row also carries its app's allocated address (a LEFT JOIN on
    ips). A tcp route's key is `tcp:<ip>:<edge port>`, and a row without an address is skipped.
  - **Poller:** every 2 s the edge polls the active prefixes and the allocated addresses. Rows outside a
    pool are skipped.
  - **tcp:** one SO_REUSEPORT listener per (app's IP, edge port), only on pool addresses. Binds that fail
    (no AnyIP route yet) are retried every 2 s. A listener serves its own key only, so an app's traffic
    can't arrive through another app's address.
  - **HTTP:** stays on the shared IPs, by Host. The edges also listen on each shared IP, on the ports of
    their own listen addresses (plain HTTP redirects when there is HTTPS). HTTP that reaches a pool
    address anyway (a wildcard listener) is a 421.
- **Edge ports:** a tcp service's edge port defaults to its VM port and need only be unique within the
  app, so two apps can both publish :5432.
- **CLI:** `flyyy ips <app> [--json]` (ADDRESS, TYPE dedicated|shared v4, SERVES). `flyyy status` has an
  `ips:` line, and its tcp services show `<ip>:<port>`. The API's app view has `ips`.
- **Tests (ips suite):**
  - pools in the table, and local on the edges;
  - two apps, both with tcp :5432, get distinct pool addresses, shown by `flyyy ips` and status;
  - each address's :5432 reaches its own app on both edges;
  - nothing answers on another port, an unallocated address, or the shared IP;
  - HTTPS on the shared IP goes by Host, plain HTTP there redirects, and there's no HTTP on an app's own
    address;
  - a redeploy to count 2 keeps the address;
  - destroy releases it and the edges close it;
  - redeploying gets a new address, not the quarantined one, and the other app is unaffected throughout.
- shortcut: IPv4 only; one control plane allocates (no cross-process lock); retired prefixes keep their
  routes until reboot; released rows are never deleted; no DNS (clients use the address).

# Dashboard (`flyyy dashboard`)

- **What:** the web UI (proxy/src/dashboard.html), embedded in the CLI. It serves only on loopback:
  `--listen`, default 127.0.0.1:4040, and anything that isn't loopback is refused before binding.
  - It serves the page at `/`, and `/config.json` as `{"apps_url": ..., "flake_url": ...}` from
    `--apps-url` (default `http://{app}.flyyy.localhost:8000/`; it must be http(s) with `{app}`), which
    the page uses for app links, and `--flake-url` (default this repo's `experiment` branch), the flake
    the join dialog's command runs (below, "Adding a worker without the module").
  - It forwards the page's API calls to the control plane (`FLYYY_API`) with the CLI's token
    (`FLYYY_TOKEN_FILE`), so the token never reaches the browser. A token the browser sends is ignored.
    The token can be the operator's or a user's: the control plane scopes the answers as usual.
- **Forwarded, and nothing else (404 "not forwarded"):** `/v1/apps` (GET) and `/v1/apps/<app>`
  (GET/PUT/PATCH/DELETE, with the name checked), `GET /v1/workers`, and the mesh's list (`GET /v1/mesh`),
  create (`POST /v1/mesh`), tokens (`POST /v1/mesh/tokens`) and approve
  (`POST /v1/mesh/nodes/<node>/approve`). Not orgs, users, the mesh's join/peers/cert, or query strings.
  Bodies are limited to 1 MiB.
- **Browsers as the threat:** any site the user visits can send requests to 127.0.0.1:4040.
  - DNS rebinding: Host must be `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>`, else 421,
    for the page and the API alike.
  - CSRF: every write needs `Origin: http://<that Host>` and a JSON content type, else 403. A
    cross-site page can't send that without a CORS preflight, and nothing here answers one.
  - Answers are `no-store` and `nosniff`, with no referrer. The CSP allows only the page's own inline
    script and style, its data: icon, and requests to its own origin, with no framing.
- **The page:** taken as given, with the tcp services shown on the app's own IP (`<ip>:<port>`) and an
  IPs row (dedicated and shared) on each app's page. It replaces the control plane's `/dashboard` from the
  ingress step, which kept the token in the browser.
- **Tests (dashboard suite):**
  - the page and its headers, and `/config.json` with the default and a custom `--apps-url`;
  - forwarded reads (workers, mesh, apps), with a wrong browser token ignored;
  - forwarded writes: deploy, scale, a join token, approve and create both reaching the control plane,
    and destroy;
  - the token in no answer;
  - three rebinding Hosts get 421 for the page, reads and writes;
  - three CSRF variants get 403 for tokens, destroy and approve, and none of them happened;
  - eight routes that aren't forwarded get 404, and the control plane never saw them;
  - a user's dashboard gets 403 on workers and none of the operator's apps;
  - `--listen 0.0.0.0` is refused.
- **Layout check (`page-overflow`, tests/page/):** headless Chromium with classic scrollbars, against a mock
  API with long values (tests/page/mock.py). At every width from 900 to 1400 px in 10 px steps, Nodes and an
  app's VMs tab must not scroll sideways (`scrollWidth == clientWidth`). It caught the Workers table running
  up to 35 px off the page between 1200 and 1250 px: its tracks had fixed minimums. Now every `.lrow`
  track can shrink to 0 and the cells clip or ellipsize; the breakpoints only drop columns.
- shortcut: apart from that layout check, no browser runs the page in the tests (only `node --check` on its script, by hand); nothing
  authenticates the browser to the forwarder, so anything local that speaks HTTP to it acts with the
  CLI's token, as it could by reading the token file.

# Single-host (dev) mode

**Real deployments are mesh-only.** Every backhaul is on a node's mesh address (10.250.x.y), and the edges
dial nothing else.

- **What it allows:** loopback backhauls, so a whole stack can run on one machine without a mesh. Each
  process accepts them only when it runs with `--single-host`: `proxy control`, `proxy agent`,
  `proxy worker` (the backhaul) and `proxy edge`.
- **Who passes it:** the `nix run .#local-control/.#local-worker/.#local-edge` apps do; the production units (nix/modules
  modules, the test cluster) never do. Without it:
  - the control plane marks a loopback worker invalid;
  - an edge skips its rows ("loopback: single-host mode only");
  - an agent won't announce a loopback backhaul;
  - a backhaul won't listen on one.
- **Refused alongside a mesh:**
  - the control plane exits if its mesh state exists, and `flyyy init` (`POST /v1/mesh`) is a 409 while
    it runs single-host;
  - a node with a mesh certificate directory (`FLYYY_TLS_DIR`) exits.
- **Shown:** `GET /v1/workers` rows carry `single_host`. `flyyy workers` starts with "single-host (dev)
  mode: ...", and the dashboard's Nodes page shows a "single-host (dev) mode" pill, with the way to a mesh
  explained instead of the create-mesh link.
- **Tests:**
  - local: the banner, and `flyyy init` refused;
  - workers: a loopback worker row is skipped by the edges and invalid at the control plane, and the
    banner is absent in the cluster;
  - mesh: the control plane, an edge and a backhaul on the mesh refuse `--single-host`, and a backhaul
    without it refuses a loopback listen address;
  - a unit test for `backhaul_ok`.

# The edge's request path at scale

- **Backhaul pool:**
  - Each edge keeps up to 4 long-lived HTTP/2 connections per worker backhaul (mTLS on the mesh, ALPN
    `h2`). A request, http or tcp, is a `CONNECT <instance>` stream on one of them (round robin), not a TCP
    and TLS handshake of its own.
  - Connections are retired after 5 min, so both ends' renewed certificates are used. HTTP/2 pings (every
    10 s, 5 s timeout) find a worker that vanished without closing.
  - A CONNECT has 5 s. A pooled connection that fails or times out is dropped, and the CONNECT is retried
    once on a new connection before the instance counts as failed.
- **Backhaul server:** serves HTTP/2 (the edges) and HTTP/1.1 (one CONNECT, e.g. the tests' `curl -p`) on
  the same listener (hyper-util's auto builder, ALPN `h2` and `http/1.1`).
- **Runtime:** the edge runs on tokio's multi-threaded runtime (one worker per CPU). The other roles keep
  one thread.
- **Routes:** the table is `HashMap<key, Arc<Route>>`, and a request takes only its route.
  - A subscription change on an edge is applied to the routes of the row it replaced and the row it is now
    (`State::apply`), each rebuilt from its own upstreams with its round-robin position kept. No other
    route is touched (a unit test checks the very same Arc survives).
  - Snapshots still rebuild everything. The backhaul's small table is rebuilt on each change, since a row
    is valid only against the others.
- **Ejection:** a backhaul 404 (the worker doesn't serve that instance yet: its table can trail the edges'
  by a moment) no longer ejects. The next instance is tried and this one keeps its place.
- **Guest:** the vsock shim (socat) listened with its default backlog of 5. Under load the VM's accept
  queue overflowed, the vsock handshake closed, and the edge returned 502 (3.3% of requests at c=64
  before, 6.7% once the edge got faster). It now listens with `backlog=1024`.

## Benchmark (tests/bench.py)

oha in the edges container: HTTPS to edge1 (edge2 stopped), whoami ×2 on two workers (1 vCPU, 128 MiB
each), keep-alive clients, 15 s at c=1 and 20 s at c=64. Run on one machine (Apple M2, 8 CPUs; all
containers and oha share them), each suite run alone.

| run | c=1 req/s | c=1 p99 | c=64 req/s | c=64 p99 | errors |
|---|---|---|---|---|---|
| before, as it was | 279 | 8.75 ms | 4,882 | 28.21 ms | 3.3% 502 at c=64 (guest backlog) |
| before, with the guest backlog fix only | 348 | 7.36 ms | 5,793 | 22.81 ms | none |
| after (pool + HTTP/2, multi-thread edge, per-route changes; backlog fix) | 811 | 4.32 ms | 10,021 | 14.12 ms | none |

Same guest, old edge vs new: 2.3× the requests per second at c=1 and 1.7× at c=64, and p99 down 41% and
38%. The edge-only run without the backlog fix gave 767 req/s, p99 4.42 ms at c=1, and 10,661 req/s, p99
13.74 ms at c=64, but with 6.7% 502s from the guest.

- shortcut: one machine. oha and the containers share its CPUs, so the numbers compare runs, not
  deployments. Per-request HTTP/1 still runs over each CONNECT stream (the app's protocol), and nothing
  pools connections from the backhaul to the VMs.

# The join stall: shared cgroups in the test cluster

- **Symptom:** rarely, one node's `flyyy join` never got an answer from the control plane ("connect timed
  out" every 5 s). It wasn't "1 in 11 boots": across every saved log it happened once in about 200 boots,
  so looping 30 boots (`tests/boots.py`) never hit it.
- **What happened that once:** worker2's `network-addresses-eth1.service` failed at boot with "Failed to
  spawn executor: No such file or directory". The node had no underlay address, so its WireGuard
  handshakes never left. WireGuard doesn't report that, so the join just waited. Nothing was wrong in the
  mesh code.
- **Why the unit failed:** the test driver starts every container with `systemd-nspawn --keep-unit
  --register=no` from the same process, and there's no host systemd in the build sandbox to give each its
  own cgroup. So all containers shared one cgroup tree: worker1's
  `/sys/fs/cgroup/system.slice/agent.service/cgroup.procs` listed three agents.
  - Units named the same in several containers (`network-addresses-eth1`, `agent`, `mesh-cert`,
    `lastlog2-import`, …) shared one cgroup. One container's systemd removing it while another spawned
    into it failed that start.
  - The old logs had about 150 such failures. Mostly harmless (`lastlog2-import` failed at nearly every
    boot), they included a `mesh-cert` restart that left Corrosion on an old certificate, and one
    underlay address.
- **Fix (tests/lib.nix `ownCgroup`):** each container's start script moves itself into
  `<driver cgroup>/<container>` before running the nspawn module's own command. Each container's systemd
  now has its own tree (one agent per `agent.service`).
  - shortcut: that start command is rebuilt from the pinned nixpkgs module, so it must be updated along
    with nixpkgs.
- **For real joins (flyyy join):**
  - After 20 s without reaching the control plane, and every 60 s after, the join says what it can see:
    whether WireGuard ever got a handshake with the control plane, and the route and source address this
    node would use for the endpoint. Examples: "no WireGuard handshake with the control plane yet; no
    route to 10.99.0.2:51820: Network is unreachable", or a route without a source address.
  - The mesh suite reproduces the stall on purpose. worker4 joins with its underlay address removed: the
    hint appears and names the missing route, and the join goes through once the address is back.
  - `boot()` now waits for each node's underlay address before it joins.

# Conditions, not sleeps

These fixed waits became conditions the code or the test can observe:

| was | now |
|---|---|
| the agent's 2 s drain | services down, then no CONNECT at the backhaul for 500 ms (deploy checks every drain ended that way; 1.3–1.6 s) |
| workers: `sleep 3` before the quiet window | both edges' /state have every live machine's row |
| workers: `sleep 3` after the bad rows | both edges' /state have the bad rows (so not routing them is their choice) |
| verify: 11 s for the ejections to lapse | both edges' /state list no ejected instance (`ejected`, new) |
| images: one more 6 s report tick | each agent's own record of what it reported (`reports/images.json` in its state dir) equals its files and the table |

# The Rust refactor

proxy/ in small steps, each on its own commit and each tested on all 14 suites. The wire formats (the schema, the API's JSON, /state) are unchanged.

| step | what |
|---|---|
| randomness | `random.rs` over ring's `SystemRandom`; the operator token is compared with `hmac::verify` (constant time) |
| modules | main.rs, control.rs and agent.rs split by concern: `edge`, `backhaul`, `pool`, `table`, `subscription`, `http`, `vsock`; `control/` and `agent/` |
| typed rows | `corro::query<T>` deserializes rows into structs; no `CAST AS TEXT`, no slice-matching. `corro::maybe` turns an off-type column into None (a subscription row that fails to deserialize would end the stream) |
| subscriptions | one `Table` trait (its row type, snapshot, change) for the edge, the backhaul and the agent |
| enums | `states.rs`: desired, fact, lifecycle, health, role, pool kind, node role, stored as their lowercase text |
| errors | typed where a caller acts on the kind: `TunnelError` (and whether it ejects), `NotReady`, the mesh's `ApiError`, `CmdError` (a command's exit code: a VM stop treats systemctl's 5, unit not loaded, as already stopped). Everything else is only logged or shown, so it stays a string |
| config | single-host, the run and state directories, the TLS directory and the edge's settings are passed in, not read from statics or env deep inside |
| clap | every proxy role and the flyyy CLI. The units and `nix run` apps pass named flags; the directories also come from env (FLYYY_RUN_DIR, FLYYY_STATE_DIR, FLYYY_TLS_DIR, …) |

What did change, on purpose:
- The proxy roles take named flags instead of positional arguments.
- Malformed arguments get clap's messages. The CLI's own checks (`--port` or `--service`, `--cmd` is gone) keep their text, and bad arguments still exit 1: 2 is "timed out".
- `capacity()` fails at start instead of silently falling back.
- flyyy also takes `--api` and `--api-token-file` (FLYYY_API, FLYYY_TOKEN_FILE).

# A new edge's slow first sync

The ips suite sometimes failed "the edges hold both prefixes as local": an edge's Corrosion got the `ip_pools` rows 41 to 68 s after it joined, past the 60 s wait. Three causes, in the order they were found:
- **No endpoint:** the control plane learned a node's endpoint only from its own key's first handshake, after approval. Other nodes had it as a peer with no endpoint for a few seconds, and sends failed with "Destination address required". The control plane now uses the endpoint of the node's bootstrap tunnel until its own key handshakes: the same address and listen port.
- **Backoff:** Corrosion's wait between sync rounds grows to 15 s and never resets. `perf.max_sync_backoff = 2` on the mesh nodes.
- **Peer selection (the one that mattered):** each round syncs with the top 3 candidates, sorted by most missing, least recently synced, then nearest. The sort is stable, the candidates arrive in actor-id order, and rand's `choose_multiple` keeps that order when it takes them all (6 or fewer). A node that has just joined ties on every key, so in small clusters its first rounds always went to its lowest-id peers, reachable or not. When the control plane's id sorted 4th or 5th of 5, the new node didn't sync with the only node that had the rows until the ties broke on their own. That's a catch-up delay of tens of seconds, not a stall: measured 25–36 s unpatched against 11–12 s patched. `nix/patches/corrosion/0002-shuffle-sync-candidate-ties.patch` shuffles before the sort.
  - Before: 2 of 10 ips runs slow (41–49 s). With the shuffle: 20 of 20 within 4.7 s.
  - A first attempt, marking peers with nothing to give as synced, made it worse (6 of 20 failed): the order of the ties was the problem, not the timestamps.
  - shortcut: a local patch on Corrosion main, worth sending upstream.

# Corrosion for service discovery only

Desired state left gossip. The control plane keeps apps, machines (each with its spec, env included), orgs,
users (token hashes) and IP leases in its own store (`<mesh-dir>/store.json`, control/db.rs). It sends
each worker its machines over the agent API (agent/api.rs). Corrosion holds only published facts, each
table with one writer:

| table | writer |
|---|---|
| machine_status, service_status, images, workers (heartbeat, capacity, allocation, announced ports) | each worker, its own rows |
| placement (instance → machine, app, worker, protocol, ports), backhauls (worker → bound backhaul), ip_pools, ips | the control plane |

- **The agent API:** mesh mTLS on each worker's mesh address (port 4281). Only a certificate with the control role (`flyyy://control/...`) is accepted. Calls:
  - `GET /v1/machines` lists id → desired state and spec digest;
  - `PUT /v1/machines/<id>` (a spec), `POST .../start`, `POST .../stop` and `DELETE` are each idempotent;
  - a spec is checked on the way in (400), and a different spec for an existing id is a 409.

  Each round the control plane lists every worker holding its machines and sends only what's missing. A worker that was down catches up on the next round. With `--single-host`, it's plain HTTP on loopback.
- **The agent's own copy:** the agent keeps what it was told in `<state-dir>/desired/<id>.json`. It runs that on restart with the control plane down, and starts a VM that died again with nobody to ask. A destroyed machine keeps its record, so a repeated destroy is a no-op.
- **Env values** travel only in that RPC. The trust suite checks that a deployed value is in no node's Corrosion and that the API shows names only.
- **Routing:** edges route `service_status` rows only as placement says, joining on the instance id and the worker (edge_routes.rs). App, protocol and edge port come from placement, the backhaul from `backhauls`. Backhauls serve only instances placed on their own worker.
- **Bound to mesh identity:** a worker's backhaul and agent API are always its mesh address (the approved worker node of that name). Only the port is the worker's word, so an address it announces that isn't its own changes nothing, after a control-plane restart too.
- **Keyed by writer:** `service_status` and `machine_status` are keyed by `(id, worker)`. A worker's row about another worker's machine is a row of its own, and placement doesn't match it. The control plane counts a check or fact only from the machine's own worker, never by the app written in the row.
- **The trust suite:** worker1, compromised, writes rows for worker2's machine, for a machine nobody placed, and for its own machine relabelled as another app on tcp. It also announces two fake backhauls. Both edges' routes stay identical, whoami and echo are served by exactly their machines, and the control plane's view stays converged. With the control plane stopped, an agent restart and a killed VM change nothing: every request is answered.
- shortcut: Corrosion doesn't authenticate writers, so a worker can still write rows in another worker's name, or write placement. Only signed rows would stop that.
- shortcut: no migration. A cluster from before this needs its Corrosion state wiped: `apps`, `machines`, `orgs` and `users` are gone from the schema, and the status tables' keys changed.
- shortcut: a VM that dies and is started again gets requests in the second before its health check fails: its row still says passing, and its vsock answers before the app listens, so those requests get a 502. The edges don't retry once the backhaul's CONNECT succeeded. Any crash has this window, the control plane up or not.

# Balancing by load

The edges pick instances by load (proxy/src/balance.rs) instead of round robin:
- **In flight:** each edge counts requests in flight per instance (connections, for tcp). A request holds its instance's slot until its response body ends.
- **The pick:** the instances fall into groups, best first: under the hard limit, not ejected, on a fresh worker, under the soft limit. The edge picks the less loaded of two random instances in the best group. The other instances follow as fallbacks if the tunnel fails. So the old order (fresh workers first, ejected instances last) still holds.
- **Limits:** services take optional `soft_limit` and `hard_limit` (`--service web=http:8080,soft=2,hard=4`). The control plane publishes them in `placement`. An instance at its hard limit gets nothing more. When every instance is there, a request waits up to 1 s for a slot, then gets 503; a tcp connection is closed.
- **Counters:** each edge's `/state` has `load`, per instance: in flight, total, rejected (503s from the hard limit), failures (failed tunnels, requests with no answer), peak in flight, and the limits.
- **The control plane's view:** each edge serves the same counters on its mesh address (`--load`, mTLS, the control plane's certificate only) and announces it in the `edges` table. `GET /v1/apps/<name>/load` asks every edge and sums per instance, per machine and for the app. Its req/s and errors/s come from the totals' change since the previous call.
- **Where it shows:** `flyyy status` shows each service's limits. The dashboard's app page polls the load every 2 s: in flight per instance against its limits, req/s and errors.
- **The balance suite:** a 2 s instance next to an instant one gets 28 of 160 requests (8 at a time), against 132 for the fast one. With `hard_limit` 1 and 16 requests at once on two 0.5 s instances, 4 are served and 12 get 503. No instance ever serves two at once: the test app checks this itself with an atomic `mkdir`.
- **Spread, not starved:** least loaded evens out requests in flight, so each instance gets requests as fast as it answers them. A slow instance isn't starved.
- shortcut: the limits and counts are per edge. Two edges can each send an instance up to its hard limit.
- shortcut: the control plane's rates are per app, between whichever two calls came last. Fine for one dashboard polling.

## Benchmark: round robin against by load (tests/bench.py)

The same bench as above: whoami ×2, edge1 only, 15 s at c=1 and 20 s at c=64. It was run alone, once each, on fab1a66 (round robin) and then on the new code, on the same machine.

| run | c=1 req/s | c=1 p99 | c=64 req/s | c=64 p99 | errors |
|---|---|---|---|---|---|
| round robin (fab1a66) | 850 | 4.26 ms | 9,970 | 14.40 ms | none |
| by load (power of two choices) | 736 | 4.44 ms | 10,711 | 9.95 ms | none |

Under load, picking by load does better: at c=64, 7% more requests per second and a p99 31% lower, because fewer requests queue behind a busy instance. At c=1, every instance has nothing in flight, so picking gains nothing and costs a little per request. The 13% drop there is from one run each, so part of it may be noise.

# Per-app metrics, without Prometheus

Each node keeps the metrics for what happens on it, in 10 s buckets for the last hour (proxy/src/metrics.rs). The control plane asks for them when someone looks:
- **Edges:** per app and service, the requests by status class (2xx, 3xx, 4xx, 5xx) and, among the 5xx, the edge's own 502s (no instance answered) and 503s (every instance at its hard limit). Also latency to the response's headers, as a histogram of 48 bins growing by 1.25× (a percentile is its bin's upper bound, so within 25%), and bytes in and out, counted when each body ends. For tcp: connections opened, and the most open at once.
- **Agents:** every 5 s, each machine's CPU (the change in its VM unit's `CPUUsageNSec`, in cores) and memory (`MemoryCurrent`). Where systemd doesn't account a unit's memory, as with the test cluster's user managers, the memory is the Firecracker process' RSS instead.
- **The API:** `GET /v1/apps/<name>/metrics?window=5m|1h` is org-scoped like the rest (another org's app is a 404). The control plane asks every edge in `edges` and each worker the app has machines on, over the mesh with its own certificate (edges: `/metrics` on `--load`; workers: `/v1/metrics` on the agent API). It merges the answers by bucket into per-bucket series and window totals for the app, plus CPU and memory per machine. `missing` names each node that didn't answer, so a gap shows instead of lower numbers.
- **Where it shows:** `flyyy metrics <app> [--window 1h]` prints the totals, each machine's CPU and memory now, and anything missing. The dashboard's app page has four small charts that refresh every 10 s while the page is open: req/s by status, latency p50/p95/p99, and CPU and memory per machine.
- **The metrics suite:** 60 requests spread over both edges (26 and 34) add up to exactly 60 2xx. 10 of the app's own 500s count as 5xx with none of them the edges'. 16 hard-limit 503s count as the edges' own. A VM burning a core reads 100%, the idle one 0%. Stopping edge2 lists it as missing. Another org's token gets 404.
- shortcut: in memory only, so a restart loses that node's hour. Buckets are by wall clock, so the nodes' clocks must agree (NTP).
- shortcut: a tcp connection's bytes count when it closes, in that bucket, and not at all if it ends in an error.

# x86_64 next to aarch64

The flake builds the packages, apps and checks for `aarch64-linux` and `x86_64-linux`: one `forSystem` in flake.nix, mapped over both.
- **The guest kernel:** allnoconfig plus `guest/kernel.config`, which both arches share, and that arch's `guest/kernel-<arch>.config`. The "every option stuck" check runs over both files' lines.
  - arm64 adds 4K pages, the OF serial, AMBA and the PL031 RTC (the wall clock).
  - x86_64 adds `VIRTIO_MMIO_CMDLINE_DEVICES`, since Firecracker passes its devices as `virtio_mmio.device=` arguments. It adds `HYPERVISOR_GUEST`, `PARAVIRT` and `KVM_GUEST` for kvm-clock, which sets the wall clock (no RTC needed). It also adds `SMP` and `X86_MPPARSE`, because allnoconfig on x86 has neither, and without them a 2-vCPU machine would see one CPU.
  - The 8250 serial is the console on both.
  - Firecracker's format for each arch: `Image` on arm64, the ELF `vmlinux` on x86_64 (not stripped). Both are installed as `$out/kernel`, so the agent and the modules don't care which.
- **Corrosion:** its `.cargo/config.toml` links x86_64 with `-fuse-ld=mold`, so mold is in the build there. The aarch64 derivation is unchanged.
- **The tests' pulled images** (whoami, http-echo) have a hash per arch. The workers suite checks the worker's own `uname -m`.
- **Checked on an x86_64 NUC** (16 CPUs, /dev/kvm):
  - the packages and apps build there;
  - `nix run .#local-control/.#local-worker/.#local-edge` in single-host mode runs whoami and busybox, and the worker reports `x86_64`;
  - whoami answers through the edge on :8000;
  - inside the busybox VM, `uname -m` is `x86_64` and `date +%s` matches the host's to the second.
  - shortcut: evaluating x86_64 on the aarch64 host stops at Corrosion's import-from-derivation (its Cargo.lock); it evaluates and builds on an x86_64 machine, or with one as a remote builder.

# A token on every Corrosion API

Every node's Corrosion API requires a bearer token (Corrosion's `[api] authorization`), so no other local process can read or write cluster state through it.
- **The token:** one per node. `corroapi-token` (a root oneshot) makes it on the first boot as `/var/lib/flyyy-corroapi/token`: 32 random bytes in hex, 0640 root:corroapi, in a 0750 root:corroapi directory.
- **Who reads it:** Corrosion and our units (control, agent, backhaul, edge, anyip), through `SupplementaryGroups=corroapi`. No user is in the group, so neither the agent's user manager nor its VMs can read it.
- **Corrosion** appends the token to the config it writes at start, with a shell builtin, so the token is never on a command line. The config and the admin socket sit in a 0700 runtime directory with umask 077. Before this, both were readable by every local user.
- **The roles:** every role takes `--corrosion-token-file` and gets its client from one place, `corro::Api::client`. The file is checked at start: no access for others, at least 32 token68 characters. corro-client and corro-api-types now come from the Corrosion commit we build (git, pinned by rev), because the crates.io release can't send a token (`with_auth_token`).
- **Single-host:** `nix run .#local-control/.#local-worker/.#local-edge` make the token in each role's own 0700 state directory, 0600. The readiness check passes curl its header on stdin.
- **The tests** send the token (`CORRO_AUTH` in tests/prelude/corrosion.py). The trust suite checks it on a worker, the control plane and the edges:
  - reads and writes get 401 without the token or with a wrong one, and 200 with it;
  - the token file is 640 root:corroapi;
  - `nobody`, and on a worker `flyyy`, can't read the token or the config, nor list Corrosion's runtime directory.
- shortcut: Corrosion compares the token with `==`, not in constant time. It's loopback only, behind the file permissions.

# Deploying to real hosts

The flake exports `nixosModules.control`, `.edge` and `.worker`: the same modules the test cluster runs, with options in place of its constants, and flyyy's packages for the host's system.
- **The options** (`services.flyyy`):
  - `controlAddress`: the control plane as the node reaches it, where its WireGuard dials (UDP 51820).
  - `control.{enable, sharedIps, ipPools, operatorTokenFile, openFirewall, endpoint}`. The operator token is made at first boot if missing (0600 root), and the control plane reads it as a systemd credential. `endpoint` (host or host:port) goes into every join token; it defaults to `controlAddress`, when that's set on the control host.
  - `dashboard.flakeUrl`: the flake in the join dialog's command.
  - `edge.{enable, nodeName, instances, listen, https.{listen, cert, key}, appsDomain}`. `instances` is one edge, named after the host, unless set: the tests run two on one host, which share its listen addresses.
  - `worker.{enable, name}`: `workerN`, its mesh node's name too.
- **Every node** trusts `flyyy0` in its firewall: everything between nodes runs inside WireGuard. The control plane opens UDP 51820. The modules no longer turn the firewall off; the test cluster does, in its own network module.
- **Joining:** a module-managed node knows its name, role and the control plane's address, so a join token is all it lacks. Installing one as `/var/lib/flyyy-mesh/join-token` starts the join (a path unit): `printf %s '<token>' | sudo install -m 600 -o mesh -g mesh /dev/stdin /var/lib/flyyy-mesh/join-token`. The node then shows up in `flyyy mesh` and the dashboard, pending your approval. Edges with the module join this way. The dashboard's Add node dialog shows one command, for a worker or an edge without the module (next section, and "+ Add node" at the end).
- **The target setup:**
  - **nixvps** (x86_64, public 203.0.113.10; RKE2 owns :80 and :443): `nix/examples/control-host.nix`. It runs the control plane (mesh hub, CA, API, Corrosion) with UDP 51820 open, and `containers.flyyy-edge`: the edge as its own mesh node, on a private network (192.168.120.1 for the host, .2 for the edge), NATed to the host's uplink, with HTTPS on :8443 forwarded from the host's :8443. The edge's `controlAddress` is the host's side of that network.
    - RKE2's ingress passes `*.flyyy.devpeek.io` :443 through to the host's :8443 (TLS passthrough: ingress-nginx's `ssl-passthrough`, and a Service without a selector whose Endpoints are the host's address, port 8443). The edge holds the wildcard certificate, from the host's `/var/lib/flyyy-edge-tls` (bind-mounted read-only).
  - **nuc** (x86_64, behind home NAT): `nix/examples/worker.nix` as worker1. Its WireGuard dials out, so the control plane reaches it without anything opened. The other nodes (the edge on nixvps, whose requests go to its backhaul) need it advertised: forward a UDP port on the router to its 51820 and set `services.flyyy.worker.endpoint = "<home public IP or name>:<port>"`.
  - **The laptop** (aarch64, behind home NAT): the same example as worker2, plus `flyyy dashboard` on loopback. The CLI and the dashboard reach the API at 10.250.0.1:4000 over the mesh; copy the operator token over from nixvps for `FLYYY_TOKEN_FILE`.
  - **Order:**
    1. Deploy nixvps; `sudo flyyy --api http://127.0.0.1:4000 --api-token-file /var/lib/flyyy-operator/token init` there.
    2. Install a token on the edge container and on each worker, and approve each: `flyyy approve <name>`, or from the dashboard.
- **Checked:** the examples evaluate as whole NixOS systems (`nix eval` of `system.build.toplevel` on aarch64), and every suite passes on these modules.
- **Not checked:** none of this has been deployed to nixvps, nuc or the laptop, so RKE2's passthrough and WireGuard through the home NAT are unchecked too.
- shortcut: the edge's tcp services (each app's own address from the pool) are local to the edge container; only HTTPS through RKE2 reaches the apps from outside.
- **Without the modules:** `nix run .#control`, `.#edge` and `.#dashboard` run the same three roles on one host, the edge and the dashboard without containers (the last section).

# Adding a worker without the module (`nix run .#join`)

The dashboard's Add node dialog (Worker) shows one command, with the token in it:
`nix run 'git+https://github.com/koxu1996/flyyy?ref=main#join' -- --name <next free workerN> '<token>'`.
The name is the next `workerN` after every one the page sees, in the mesh or announced. The flake URL is `services.flyyy.dashboard.flakeUrl` (`--flake-url` on `flyyy dashboard` too). The control plane's address comes from the token, so the control plane needs `endpoint` set, which defaults to its `controlAddress` when that's defined. Edges keep using the module.

- **Nothing runs as root.** Each part of the worker is a system unit of its own in `flyyy-worker.slice` (`flyyy-mesh`, `flyyy-corrosion`, `flyyy-agent`, `flyyy-backhaul`, nix/join.nix), with `User=` set to whoever ran `nix run`. Run as root, it needs `--user NAME` (or sudo's caller) and refuses root itself. Only `flyyy-mesh` gets `AmbientCapabilities=CAP_NET_ADMIN`. All of them have `NoNewPrivileges`, and each restarts on its own.
  - **The VMs** stay `systemd-run --user` units of that user, as the agent makes them everywhere. The app turns on `loginctl enable-linger` for it, so its systemd runs without a login. Without logind (some containers), it writes the linger file and starts `user@<uid>` itself.
  - **`/dev/kvm`:** the user needs it through its group, or it must be 0666. The app checks both, says which it found, and when neither holds, says which is missing and how to fix it.
- **sudo is for the setup only:** the WireGuard check, the units, linger, the firewall rule, the GC root, and the user's own directories: `/var/lib/flyyy-mesh`, `/var/lib/flyyy-tls`, `/var/lib/flyyy`, `/var/lib/corrosion`, `/var/lib/flyyy-corroapi`, `/run/flyyy` and `/run/corrosion`. They're the module's paths, owned by the user, and an earlier root run's are handed over. `nix run` itself runs as the user, since it fetches the flake with the user's GitHub key, which root doesn't have.
- **Checks first, with clear errors:** the architecture (aarch64 or x86_64), `/dev/kvm` and the user's access to it, systemd, and the kernel's WireGuard. `ip link add type wireguard` is tried, which also loads the module.
- **NixOS or not:**
  - On NixOS the units are transient (`systemd-run`): `/etc/systemd/system` belongs to the system, and this works with `mutableUsers = false` and a root wiped at boot. They don't survive a reboot; run the command again to resume.
  - Elsewhere it writes `/etc/systemd/system/flyyy-<part>.service` and enables them, plus a `tmpfiles.d` entry for the `/run` directories, so the worker starts again at boot.
  - The units' script is GC-rooted (`/nix/var/nix/gcroots/flyyy/worker`).
- **Resume:** running it again without a token picks the join up from `/var/lib/flyyy-mesh` (name, control plane, keys). It refuses a token on a machine that already joined, and it refuses another user's join. Before approval it also says how to start over.
- **NixOS's firewall:** the app opens it the way the module's `trustedInterfaces` does (everything on `flyyy0`), plus UDP 51820, for the iptables firewall and for the nftables one. For any other firewall, it says what to open. The rule doesn't survive a firewall reload; running the app again puts it back.
- **At the end** it waits up to a minute for the control plane to answer, then prints a short result: `✓ <name> joined through <control>` and `next: approve <name> in the dashboard`, or `✓ <name> running again (approved before)`. The module snippet that makes the worker permanent, with the steps for handing the state to the module's users, is behind `--nixos`, which prints it and exits.
- shortcut: the parts share one user, rather than the module's user per service, because the module's users can't be added without the module (`mutableUsers = false`). A compromised backhaul, for example, could read the agent's state. The module is the separated setup.
- shortcut: Corrosion reads its certificate only at start, so a renewed one (after half its 24 h lifetime) restarts `flyyy-corrosion`.
- shortcut: the token is on the `nix run` command line, so other local users can see it in `ps` until it's spent, which happens seconds later.
- **The join suite** (tests/join.py): a NixOS container without flyyy's module, with its firewall on and a user bob in the kvm group (`plain`).
  - The token carries the control plane's endpoint, from its `controlAddress`.
  - As root without `--user`, the app refuses. `flyyy-join --user bob --name worker5 '<token>'` starts the four units and prints two lines: joined, and approve next. `--nixos` then prints the snippet, with the control plane from the join. bob reaches `/dev/kvm` because it's 0666 in the container: it's bound from the host, whose kvm group isn't the container's.
  - Every unit is `User=bob`, only `flyyy-mesh` has `CAP_NET_ADMIN`, and bob lingers, with his systemd running.
  - After approval, worker5 is alive and its firewall lets the mesh in. A scale puts a whoami machine there, and the edges route to it.
  - No flyyy process runs as root: the units' scripts, `flyyy`, `proxy`, Corrosion and Firecracker are all bob's, and the VM is a unit of bob's systemd.
  - A second token is refused. With the slice stopped, running it again without a token resumes worker5 at the same mesh address, and its machine still serves.
- **Not checked:** the sudo path (the build sandbox can't make sudo's setuid wrapper, so the suite runs the app as root with `--user`), logind's `enable-linger` (the containers have no logind), `/dev/kvm` through the group, the "neither group nor 0666" message, the non-NixOS units, the nftables firewall, a real `nix run` of the git+ssh flake, and the module snippet's switch-over steps.

# The guest's init: one static binary

The initrd is flyyy-init alone (`/init`, a static musl binary, about 715 KiB). It is PID 1 from the kernel's start to the VM's end, and replaces the busybox shell script and socat.
- **The initramfs stage** (guest/src/boot.rs):
  - devtmpfs on /dev, and the console as fds 0–2 (the initramfs has no /dev/console of its own);
  - the network, with interface ioctls: `lo` up; dummy0 renamed eth0, given 10.0.0.2/32 (address and netmask set while it's down, so no wider route ever appears), with IPv6 off (the sysctl), then up;
  - the hostname;
  - the image (vda, EROFS, read-only) and the scratch drive (vdb, ext4) mounted, then the overlay of both on /newroot;
  - `config.json` read from the scratch drive into memory, then deleted there. It may carry secrets, which the app gets as env only. It's no longer copied into the overlay as `/.flyyy`.
  - /dev and /proc moved into the new root, sysfs and the root cgroup mounted there;
  - the root switch: the new root moved onto / and chrooted (pivot_root doesn't work from an initramfs). It then goes on in the same process: /dev and /etc, the user, then fork and exec the app, reap and forward signals, as before.
- **The vsock relays** (guest/src/relay.rs): a listener per port in FLYYY_PORTS, made before the root switch. Each connection is relayed to 127.0.0.1:<port> on two threads, one per direction. The client's half-close is passed on to the app. The app's end closes both connections fully, as socat did: Firecracker's vsock doesn't pass a guest's half-close to the host, so an answer delimited by its connection closing (no Content-Length, as the test app's CGI answers) never ended at the edge. The first build only passed the half-close on, and balance hung on both arches. A connection the app doesn't answer yet is closed. A connection the guest drops itself (no thread, no descriptor for the relay, a failed accept) is logged on the console, at most once a second, with a count of the ones in between.
- **Gotchas:**
  - The signals PID 1 waits for (SIGCHLD, and the ones it forwards) are blocked before any thread starts, so they all reach the main thread's `sigwait`.
  - The forked child of a process with threads makes only async-signal-safe calls: errors are written with write(2), with errno in decimal.
  - listen(2) is called directly with a backlog of 1024: nix's `Backlog`, and the vsock crate's `bind`, refuse anything over musl's SOMAXCONN of 128.
  - ioctl's request type is an int on musl and an unsigned long on glibc (`as _`).
  - Nothing on a relay thread may panic: it's PID 1, built with panic = "abort".
  - The child resets SIGPIPE to its default: Rust's runtime ignores it, and an ignored signal survives execve, so apps used to start with SIGPIPE ignored.
- **Checks:** `guest-test` (its unit tests under musl, the relay's half-close among them) and `guest-clippy` (for the musl target). init.sh and its shellcheck are gone. PID 1's command line is now `/init` (tests/guest.py).
- shortcut: the initramfs's own mounts (/lower, /scratch) and its one file stay, hidden under the new root; switch_root used to delete the initramfs's files.

# The public dashboard

`proxy dashboard` (and `nixosModules.dashboard`) serves dashboard.html over HTTPS and forwards the page's API calls to the control plane over the mesh, passing on each caller's own `Authorization: Bearer` header. It holds no token itself. `flyyy dashboard` stays as the loopback mode, acting with the CLI's token.
- **Sign-in:** `/config.json` says `"auth": "token"`. The page asks for a token (the operator's, or an org user's from `flyyy users create`), keeps it in the tab's sessionStorage, and sends it on every call. A 401 signs the page out. The page makes no call until the config has said which mode it's in, so a reload doesn't send a token-less request that counts as a failure.
- **Who sees what:** the control plane decides, by the caller's token, as for the CLI. An org user sees and changes only their org's apps; workers and the mesh are the operator's.
- **Hardening:**
  - The route allowlist is today's, plus `GET /v1/apps/<app>/metrics` with only its `?window=` passed on. The loopback dashboard forwarded neither, so its metrics charts couldn't load.
  - The Host must be the public name (`--host`); writes need its https Origin and a JSON body.
  - A strict CSP, the same for both modes: the page's one inline script and one stylesheet by SHA-256, no `unsafe-inline` for either (only for style attributes, `style-src-attr`), requests to its own origin only, no framing, forms or base. HSTS on the public one.
  - Request bodies are at most 64 KiB; a request's head at most 16 KiB, and it must arrive within 10 s.
  - 401s are limited per source address: past 10 in a minute, every request from that address gets 429 until the minute is over. That covers 401s from the control plane and a missing or malformed bearer here.
- **On the mesh:** the dashboard's host is a node of a new role, `dashboard`. The control plane refuses it a mesh certificate, and `flyyy join` doesn't ask for one. So it can reach the control plane's API, but nothing a certificate opens: the backhauls, the agent APIs, the edges' load ports, Corrosion's gossip.
- **On nixvps:** `containers.flyyy-dash` in `nix/examples/control-host.nix`, its own mesh node on its own private network (192.168.121.1 for the host, .2 for the dashboard), NATed. RKE2's ingress passes `flyyy.devpeek.io` :443 through to the host's :9443, forwarded to the container's :8443. Its certificate is the edge's, which must also name `flyyy.devpeek.io`: a `*.flyyy.devpeek.io` wildcard doesn't cover it.
- **The dashboard suite** checks the public one on a container of its own (pubdash) that joins as a dashboard:
  - the page with its CSP and HSTS;
  - 401 with no token, a wrong one, or a non-bearer;
  - alice (red) sees only red-web, and the operator sees every app;
  - workers are the operator's;
  - metrics with `?window=1h` passed on;
  - a write from another Origin gets 403, and alice's from the page gets 202;
  - orgs aren't forwarded;
  - no token in its unit and no certificate on it;
  - after 10 failed sign-ins from one address, 429.
- shortcut: behind RKE2's TCP passthrough every caller has the ingress's address, so the 401 limit is one for all of them there; PROXY protocol would give each its own.

# Advertised WireGuard endpoints

`flyyy join --endpoint host:port` (`services.flyyy.edge.endpoint`, `.worker.endpoint`) lets a node say where the other nodes should reach its WireGuard. The control plane keeps it from the join and hands it out in the other nodes' peer lists instead of the endpoint it learned from the node's packets. Nodes without one keep the learned endpoint, as before.
- **The endpoint:** `<IPv4>:<port>`, `[<IPv6>]:<port>` or `<name>:<port>`, checked by the CLI and again by the control plane. A name is resolved by `wg set` on each node.
- **`/v1/mesh`:** shows `endpoint` (learned), `advertised_endpoint`, and `peer_endpoint` (what the others get). `flyyy mesh` shows the advertised one, marked "(advertised)".
- **The mesh suite:** worker4 advertises 10.99.40.2:51820, a second address of its own. worker1 blackholes worker4's learned address (10.99.4.2). worker1 still gets worker4 at the advertised endpoint and handshakes with it there, and worker4 reaches worker1's backhaul node to node. The other nodes keep their learned endpoints.
- **Behind NAT:** forward a UDP port on the NAT to the node's 51820 and advertise `<public address>:<that port>`. The control plane still reaches the node the usual way, since the node dials it.
- shortcut: one endpoint for every other node. A node behind a NAT advertises an address all of them can reach (its forwarded public port), not a LAN one. Two nodes behind the same NAT then reach each other through it, which needs the router to hairpin.
- shortcut: sent at the first join only. Changing it later means a new join (a new node), for now.
- shortcut: a name is re-applied, and so re-resolved, on every 5 s peer sync, since `wg show` reports the resolved address. That's harmless, and it follows dynamic DNS.
- shortcut: until two nodes first handshake at the advertised endpoint, WireGuard can roam one to the learned address, until the next peer sync (5 s) sets it back.

# The control host without the modules (`nix run .#control`, `.#edge`, `.#dashboard`)

Three apps for the control host, rootless like `#join`, which shares their setup (nix/host.nix; the apps are nix/host-apps.nix). Each part is a unit of its own with `User=` the caller, in the app's slice (`flyyy-control.slice`, `flyyy-edge.slice`, `flyyy-dashboard.slice`). sudo is for the setup only. Each opens NixOS's firewall for what it serves and resumes from `/var/lib/flyyy*` when run again. The single-host dev apps that had these names are now `.#local-control`, `.#local-worker` and `.#local-edge`.
- **What they print**, in one shape for all four (with `#join`): a progress line (`joining as …`, `resuming …`), then `✓ <what's up>`, a few indented facts, `next: <the one step that comes next>`, and on `#edge` `permanent setup: add --nixos` (`#control`, `#dashboard` and `#join` don't print it; `--nixos` still works on all four). Errors and warnings go to stderr, with their log hints.
  - `--nixos` prints the NixOS-module equivalent and the steps to hand the state over, and exits without changing anything. On the control plane's host, `#edge` and `#dashboard` print that the modules run them in a container of their own.
  - `#control` prints two lines and nothing else: `✓ control plane up (endpoint …)` (or `running again`) and `operator token: <token>`, on every run (the token is the caller's own file). `--hide-token` prints its path instead (for screen recordings). The next steps are in the README.
- **`#control -- --endpoint HOST[:PORT]`:**
  - `flyyy-control`, the control plane, with `CAP_NET_ADMIN` only (the mesh hub, the CA, the API on 127.0.0.1:4000).
  - `flyyy-control-corrosion`, its Corrosion.
  - The first start makes the mesh, with no join token: `proxy control --create-mesh` makes it at start when there's none (`Mesh::create`; `flyyy init` is create plus a token, as before). The operator token is in `/var/lib/flyyy-operator/token` (the caller's, 0600), where flyyy looks by default. The dashboard's "+ Add node" adds nodes (`flyyy token` still makes a join token from the CLI).
  - The endpoint is kept in `/var/lib/flyyy-control/endpoint`, so a re-run needs no flags. It refuses a host that joined the mesh as a node, and a control plane that's another user's (the module's).
  - The module equivalent is `nixosModules.control` with `control.endpoint`, plus the steps to hand its state to the module's users.
- **`#edge -- --listen ADDR [--tls-cert PEM --tls-key PEM [--http ADDR]] [--apps-domain D] [--name N] '<token>'`:**
  - `--listen` is HTTPS with a certificate, plain HTTP without. With HTTPS, `--http` is the plain address that redirects (default 127.0.0.1:8000). A port below 1024 gets `CAP_NET_BIND_SERVICE`, and only `flyyy-edge` gets it.
  - **Elsewhere** it joins as a mesh node of role edge, with its own Corrosion: `flyyy-edge-mesh` (`CAP_NET_ADMIN`), `-corrosion`, `flyyy-edge`, `-anyip`. The module equivalent is `nixosModules.edge`.
  - **On the control plane's host** (`/var/lib/flyyy-control` is there) it can't join, since there's one `flyyy0` per host. Instead:
    - it has the control plane's mesh address, 10.250.0.1, where it announces its load counters;
    - it reads the control plane's Corrosion, so it must run as the same user;
    - `flyyy-edge-cert` (`flyyy local-cert`) gets its certificate over loopback for the join token: `POST /v1/mesh/local`;
    - units: `flyyy-edge-cert`, `flyyy-edge`, `flyyy-edge-anyip`.
  - With the modules, an edge never shares that host: it's a container of its own (`containers.flyyy-edge` in nix/examples/control-host.nix), which `--nixos` says.
- **`POST /v1/mesh/local`** (`Access::Local`): on the main listener from loopback only; 404 elsewhere, including the mesh listener.
  - The token authorizes it. An unused one is spent (its bootstrap peer goes) and registers a local node: name, role, and the token's public key, in `mesh.json`'s `local`.
  - After that the same token renews the certificate, under the same name only. The edge keeps it in `/var/lib/flyyy-edge/token` (0600).
  - The certificate is for 10.250.0.1, `flyyy://edge/<name>`. Only role edge: nothing else on that host needs one.
  - `role_at(10.250.0.1)` is edge when there's a local edge, so the control plane reads its load and metrics there (mTLS). `/v1/mesh` lists it as `state: local`, and the dashboard shows "On the control host".
  - The edge's mesh certificate is in `/var/lib/flyyy-edge/tls`, not `/var/lib/flyyy-edge-tls`, where the example keeps the public certificate.
- **`#dashboard -- --host NAME --tls-cert PEM --tls-key PEM [--listen ADDR] [--apps-url URL] ['<token>']`:**
  - On the control plane's host it calls the local API (127.0.0.1:4000), with no mesh and no token; it refuses one. With the modules it's a container there (`containers.flyyy-dash`).
  - Elsewhere it joins with role dashboard (`flyyy-dashboard-mesh`), with no certificate, as the module's does.
  - `--apps-url` defaults to `https://{app}.<host>/`.
- **The setup they share** (host.nix, which `#join` uses too):
  - who runs it (never uid 0);
  - the checks (architecture, systemd, WireGuard, and that the user can read the certificates);
  - the user's directories, secrets, firewall rules (iptables or nftables) and GC root;
  - the units: transient on NixOS, real ones elsewhere, quoted for systemd. An argument with `$` or a newline is refused, since systemd would expand it even from systemd-run;
  - waiting on a unit's log.
  - `#join` also now checks `--endpoint` before anything changes, and refuses a host that's a control plane or joined as another role.
- shortcut: an edge on the control plane's host runs as the control plane's user and reads its Corrosion with its token, so the two aren't separated from each other. The module's container setup is.
- shortcut: the local edge's certificate is for the control plane's address, which Corrosion's gossip would take as the control plane's. They're the same user's anyway, and the edge runs no Corrosion.
- shortcut: one local edge at a time (`role_at` answers for the address, not the name); a second would need another address.
- shortcut: `#control` has no `--shared-ip` or `--ip-pool`, so no tcp services. The module's `sharedIps` and `ipPools` are where those are.
- **The hosts suite** (tests/hosts.py): its own cluster.
  - `ctl` is a NixOS container without flyyy's modules, with its firewall on, where carol runs all three apps.
  - `plain` is the worker host, where bob runs `#join`.
  - `#control` as root without `--user` is refused. As carol it makes the mesh with no join token and no node (`/v1/mesh`), and its output is exactly two lines: up, and the token. `--nixos` prints the module's setup with the endpoint, and the units keep running. The edge's token comes from `flyyy token` and carries 10.99.8.2:51820. Both units are carol's, and only `flyyy-control` has `CAP_NET_ADMIN`. The firewall lets WireGuard and `flyyy0` in, and Corrosion and the API listen on 10.250.0.1.
  - `#edge` with the token on :443 is a local node at 10.250.0.1 in `/v1/mesh`, its certificate for 10.250.0.1 and `flyyy://edge/edge1`. The host has one WireGuard interface and no edge mesh or Corrosion units. Every unit is carol's, and only `flyyy-edge` may bind :443.
  - The spent token under another name is refused, and so is `#edge` again with a new token.
  - `#dashboard` with a token is refused; without one it runs with no mesh unit, and says to sign in with the operator token next. `#edge --nixos` and `#dashboard --nixos` name their containers.
  - A worker joins with `#join` and comes up alive. `/v1/mesh/local` from that mesh node is a 404.
  - whoami answers through edge1, asked from the worker's machine over HTTPS on :443. The control plane's metrics read edge1's counters at 10.250.0.1. The dashboard answers `/v1/workers` from the local API with the caller's token.
  - Every flyyy, proxy and Corrosion process on `ctl` is carol's.
  - With the three slices stopped, `#control` resumes without flags or a new token (printing the same token; with `--hide-token`, only its path), `#edge` resumes edge1 without a token, `#dashboard` comes back, and whoami still answers through edge1.
  - Each app's output is in the suite's log (`out  #control, first start:` and so on).
- **Not checked:** the sudo path (the suite runs the apps as root with `--user`), `--hide-token` on a first start, the non-NixOS units and the nftables firewall, `#edge` and `#dashboard` joining from another host (they reuse `flyyy join`, which the mesh and join suites cover, but not these apps' units), the module snippets' switch-over steps, x86_64, and nixvps.

# The dashboard's Nodes page

- **Pending joins** are amber banners at the top: "<name> asks to join as <role> · from <endpoint> · <age>", with the two-click Approve on the right.
  - The endpoint is the one the other nodes would get: the advertised one, else the one learned from the bootstrap tunnel.
  - The age is `asked_age_s` in `/v1/mesh`: nodes now keep `joined_ms`. Nodes from before have none, so the banner leaves the age out.
- **Two tables** for the rest:
  - **Control plane and edges:** node and role, state (local included), mesh IP, endpoint (marked "(advertised)" when it is), handshake. A dashboard's mesh node is listed here too. (Now a Control plane card, then Edges and Dashboards: see "The Nodes page: a Control plane card".)
  - **Workers:** worker, with arch and machines underneath; state; the vCPU and memory meters; images; mesh IP; heartbeat. A worker is matched to its mesh node by its backhaul's mesh IP, else by name. Without a mesh, the workers alone are listed.
- **Narrower:** below 1200 px the endpoint, images and mesh IP columns go, below 900 px the heartbeat, and below 600 px the meters. State pills stay on one line.
- **Sidebar:** the profile button (who's signed in, with Sign out in its menu) is a full-width row at the bottom, above the collapse toggle. On phones it's just its avatar in the header. See "No workers, and one profile button" below.
- The old single-table `.nrow` CSS is gone.
- **Checked:**
  - screenshots (headless Chromium, a mock of the API) at 1440, 1300, 1100, 820, 800, 560 and 375 px, in token and local mode: nothing runs off the page;
  - the mesh suite checks `asked_age_s` on a pending node.
- **Not checked:** a real pending join in the browser (the mock had two), and the collapsed sidebar.

## + Add node

"+ Add node" replaces "+ Join token". It makes a token, or the mesh first when there's none, and opens one dialog.
- **The dialog:**
  - a Worker / Edge toggle, Worker by default;
  - one command with the token in it, written over several lines with `\` continuations, so it reads easily and pastes as is;
  - a copy button and one line: "One use, expires in 24 h; the node appears here to approve."
  - The token isn't shown on its own any more.
- **Worker:** `nix run '<flake>#join' -- --name workerN '<token>'`, with the next free `workerN`, as before.
- **Edge:** `nix run '<flake>#edge' -- --name … --listen … --apps-domain … --tls-cert … --tls-key … '<token>'`.
  - **Fields:** name, listen (default 0.0.0.0:8443), and the certificate and key paths (default `~/flyyy-tls/cert.pem` and `key.pem`).
  - **Remembered** per browser (localStorage `flyyy.add-edge`, with try/catch). A remembered name that's taken by now gives way to the next free `edgeN`, which is also the placeholder.
  - **The apps domain** comes from the dashboard's apps_url (`https://{app}.<domain>/`).
  - **The command** follows the fields as they change. Values are quoted with the page's `shellQuote`, except that a leading `~/` stays bare so the node's shell expands it. A name that isn't `[a-z0-9-]{1,32}` marks its field red.
- **The empty Nodes page** says "No nodes yet: + Add node". The sidebar badge is unchanged.
- **Checked:**
  - in headless Chromium against the mock API: both modes of the dialog at 1100 and 390 px, and the empty page;
  - the edge command with a certificate path containing a space, run through bash with `printf` in place of `nix`: each value one argument, `~` expanded;
  - the dashboard suite checks the page has every piece of the dialog and no separate token display.
- **Not checked:** a real `nix run` of the edge command.

## The Nodes subtitle, and the load panel's rows

- **The Nodes subtitle** no longer shows the control plane's key. When N > 0 it says "N join commands not used yet (expire in 24 h)"; otherwise it's empty, apart from the single-host pill.
- **The load panel's rows** have their own class, `.ldrow`. Its `.lrow` rule (and `.lrow.head`, `.lrow .mono`) applied to every `.lrow`: it turned the app list's 6-column rows into 5 columns, so the arrow wrapped, and it set the Nodes tables' padding.
  - Now the app list keeps its grid, and the Nodes tables use the app list's row padding.
  - On phones the load panel's cells wrap and its limit chip takes a line of its own; before, its five columns ran off the page.
- **Checked:** in headless Chromium against the mock API, the app list, the load panel (an app page) and both Nodes tables at 1440, 900 and 375 px.

## New app: a template first

- "From a template" leads the New app dialog as the recommended start, with a "Recommended" badge and the accent border, and it has the focus when the dialog opens. "Your own image" stays second and quieter: no fill and a hairline border until hovered, and a lighter title.
- The `nix run` apps' help and errors now say "dashboard: + Add node" where they said "+ Join token".
- **Checked:** in headless Chromium at 1100 and 375 px; the dashboard suite checks the order, the badge and the focus in the page.

# Orgs & users in the dashboard, and who's signed in

- **"Orgs & users"** is a sidebar page for the operator:
  - one row per org, with its users as chips (admins marked), its active apps, and Destroy. Destroy is disabled while the org has apps; destroyed orgs stay listed, dimmed;
  - **+ New org** asks for a name;
  - **+ New user** asks for a name and an org (a select of the active orgs). Then it shows the user's token once, with a copy button and "Shown once; give it to the user." Only the token's hash is kept;
  - **Destroy** uses the apps' typed-name confirmation. The confirm dialog is now shared: an app or an org.
- **The CLI alternative:** each dialog shows the equivalent command under the form, as the deploy dialog shows `flyyy deploy`, with a copy button. It follows the fields as you type: `flyyy orgs create acme`, `flyyy users create alice --org acme`, `flyyy orgs destroy acme`.
  - Copy buttons anywhere on the page now share one handler.
- **`GET /v1/whoami`** (any valid token, `Access::User`) answers `{"operator": true}` or `{"user": …, "org": …}`.
  - The sidebar shows it at the bottom, as the profile button: "operator", or "alice" with "acme" under it.
  - Nodes and Orgs & users are hidden until whoami says operator. For an org user they stay hidden, and their calls (workers, mesh, orgs, users) aren't made, so no 403s appear. A link to either page goes to Apps.
  - On phones the header shows the current page's label, the others as icons, so three pages fit, and the profile button's avatar.
- **The dashboard forwards** `GET /v1/whoami`, `GET`/`POST /v1/orgs`, `DELETE /v1/orgs/<org>`, and `GET`/`POST /v1/users`. Nothing else under those paths is forwarded, and writes still need the page's Origin and JSON. The control plane keeps orgs and users the operator's: an org user gets 403.
- **flyyy's default `--api-token-file`** is now `/var/lib/flyyy-operator/token`, the operator token's file on the control host (#control's, and the module's), instead of `/var/lib/spike/control.token`. With the default `--api` (10.250.0.1:4000, reachable on the control host itself), the commands the dialogs show work as they are there, as the token's owner. #control prints the path and says so.
- **Tests:**
  - **unit:** whoami's answer; the routes table; the forwarding allowlist (orgs and users in, `/v1/orgs/<org>` only for DELETE, no path tricks).
  - **dashboard suite:**
    - the page's pieces (the page, the commands shown, who's signed in, both pages hidden by default);
    - whoami for the CLI's token;
    - + New org and + New user through the dashboard (the token in the answer, then valid as tom of teal), and destroying the org (tom's token stops working);
    - CSRF on org writes;
    - alice's dashboard: whoami says alice of red, and orgs, users and mesh are 403 for her, so her page shows neither page;
    - the public dashboard: orgs 200 for the operator and 403 for alice, whoami by each caller's token.
  - **hosts suite:** carol runs `flyyy orgs create acme` with no flags or environment on the control host.
- **Checked in headless Chromium** against the mock API: the page at 1200 and 375 px, the New user dialog and its token, Destroy, and an org user's sidebar (Apps only, `#nodes` going to Apps).

# App events and machine logs

- **Events** (proxy/src/control/events.rs): the control plane keeps a ring of the newest 200 per app, in memory, newest first. `GET /v1/apps/<name>/events` answers `{app, events: [{at_ms, machine, text}]}`, for whoever sees the app.
  - **From the API:** "deployed <image>, N machines", "scaled to N", "destroyed" (no machine).
  - **From the reconciler:** "placed on workerN" when it creates a machine.
  - **From the facts it already reads every round** (machine_status, service_status, workers): each round's facts are compared with the last round's (`Seen`, `changes`, a pure function with unit tests):
    - "image building" and "image built", from the machine's `pulling` state and its leaving it;
    - "starting", "started", "stopping", "stopped", "failed: <error>", "destroyed";
    - "check <service> passing", or "failing"/"down", from the machine's own worker's row only;
    - "workerN silent (no heartbeat)" and "workerN back (heartbeat)", for each of its live machines, with the same 30 s rule as placement.
  - shortcut: in memory only, so a restart empties the rings, and its first round is a baseline with no events. Fact changes are seen once per round (5 s, or an API write): their time is the round's, and a state that came and went between two rounds isn't seen. A quick image build can go unnoticed, so the suite notes those events instead of requiring them.
- **Logs:** `GET /v1/apps/<name>/machines/<id>/logs`. The id must be the app's (`<app>-<hex>`, else 400), and the machine one the control plane placed (else 404).
  - The control plane asks that machine's worker over the agent API (mTLS, its certificate only): `GET /v1/machines/<id>/logs`, only for a machine the agent holds.
  - The agent reads the VM unit's journal (below): the guest's kernel, its init and the app.
  - **That needs the journal on disk:** journald splits off a journal per user (SplitMode=uid) only when it stores the journal persistently, and lets the user read it through an ACL. Kept in memory (systemd's `auto` without `/var/log/journal`), everything is in the system journal, which the agent's user can't read: the first run failed that way ("insufficient permissions").
    - So the worker module sets `services.journald.settings.Journal.Storage = mkDefault "persistent"`. That's host-wide but overridable; the alternative, the `systemd-journal` group, would let the agent read every log on the host.
    - The agent runs `journalctl --user-unit fc-<id>.service _TRANSPORT=stdout -n 200 -o cat`, not `--user`, which opens the user's own files only. That's the VM's stdout (its serial console), not systemd's lines about the unit.
    - **Tests:** the Nix build sandbox refuses ACLs, so in the test containers flyyy's file is root's (found with a throwaway container test). There the workers get `SplitMode=none` and flyyy the `systemd-journal` group, test-only (tests/lib.nix), so the same command reads the system journal. The ACL path itself runs only on real hosts. The same command, run as an ordinary user, read a user unit's output here, but that user is in `wheel`.
    - The join app can't change a host's journald, so there the agent's error says what's missing (`Storage=persistent`).
  - The answer is `{machine, worker, lines}`.
- **The dashboard:**
  - **Events** on the app page (now its Events tab): time (24 h), machine, text, newest first, polled every 5 s, with failures in red.
  - **Logs** on each machine row opens a dialog with its last 200 lines, scrolled to the end, and Refresh.
  - The dashboard forwards both routes.
  - On phones a machine row drops its state text, since its dot shows the state; an event's text goes under its time.
- **New app → Configure:** the Served as / Health check row fits the dialog at every width. Its columns are `minmax(0, …)` and a label's hint shrinks with an ellipsis, where the health check's long hint used to push the field off the right. The memory sizes wrap to a second line instead of running off. The image field shows its start (the repo) when the dialog focuses it, and after it loses focus.
- **Tests:**
  - **unit:** the event diff (a machine's life, its checks, another worker's row, its worker silent and back, a failure's error), the ring (200, newest first, per app), the routes and their ids, the dashboard's allowlist;
  - **the events suite:**
    - a deploy of the svc image ×2: deployed (the app's), placed, started, check web passing, newest first;
    - a check failing, then passing again;
    - a machine's console: from its worker, at most 200 lines, the guest's init in it; 404 for an unknown machine, 400 for another app's id, 404 for another org's user on both routes;
    - scaled to 1, and the machine that goes;
    - its worker silent (agent stopped), then back;
  - **the dashboard suite** checks the page has the Events panel, the Logs dialog and the Configure fixes.
- **Checked in headless Chromium** against the mock API: Configure at 1100, 560 and 375 px, from your own image and from a template; the app page with events and the logs dialog at 1200 and 375 px.

# The app page, redesigned

- **The header** is unchanged: name, status, URL, Scale, ⋯.
- **A status strip** under it, five cells, live:
  - Machines: started / count;
  - Health: passing / started for the http service (else the first), "GET <path> passing" or "TCP connect passing", red when below started;
  - Traffic: req/s now from `/load`, with "N in flight now";
  - Latency p95, with p50 under it, for the metrics window;
  - Errors: 5xx plus the edges' 502/503 for the window, red when above 0.
  - The layout is five columns on desktop, three below 1100 px and two on phones. The grid's 1 px gaps draw the hairlines, and the last cell spans the row's rest.
- **Tabs** under the strip: Configuration (the default), Traffic, Machines (N), Events. The tab is kept across the 2 s refreshes and resets when another app's page opens. Events (the previous section) were a panel below the tabs; they're the fourth tab now, polled every 5 s whichever tab is open, so it opens on the latest. Below 520 px the tabs' padding shrinks so the four fit in one row.
- **Configuration:**
  - Image: owner and name in bold, size and digest under them;
  - Size: vCPU and memory each, with the total under them;
  - Services: name, protocol, port → host, then the check on the next line, with limits only when set;
  - IPs, only if any;
  - Command, and Env names ("values hidden").
- **Traffic:**
  - the 5 min / 1 hour toggle, then a 2×2 grid of larger charts (one column on phones) with dashed gridlines at the quarters and a time axis ("5 min ago · 2.5 min ago · now");
  - legends give the window's totals, not the last bucket: 2xx/3xx/4xx/5xx counts, p50/p95/p99 from the totals, CPU per machine on average, memory per machine now;
  - one line under the grid: requests, bytes in and out, the edges' own 502/503 only when non-zero, and the nodes that didn't answer.
- **Machines:** one table, replacing the Load panel and the old machine list.
  - Columns: machine (state dot, id; the id opens its logs), worker with its arch (from `/v1/workers`, so the operator's only), state, in flight and req/s (summed over that machine's `/load` instances), CPU now, memory now / limit, Logs.
  - A machine's error is a full-width red line under its row.
  - Below 1100 px in flight and Logs go; phones show machine, worker and state only.
- **Removed:** the Load panel, the Metrics panel's summary line, the stat tiles (Machines, vCPU, Memory, Image), and their CSS (`.lsum`, `.ldrow`, `.mrow`). `/load` and `/metrics` are still polled (2 s and 10 s) for the strip and the tabs.
- **Checked:**
  - in headless Chromium against the mock API (with metrics, workers' arch and a failed machine): each tab at 1440, 900 and 375 px;
  - the dashboard suite checks the strip, the tabs and that the old pieces are gone.
- **Not checked:** the page against a live cluster in a browser.

# No workers, and one profile button

- **No workers, on Nodes:** with the control plane (and edges) but no worker, the Workers section still shows. In it is an empty state: 🖥️, "No workers yet", what workers are for, and "+ Add worker", which opens Add node on Worker. Pending joins stay as banners above. With no mesh at all, the old whole-page empty state stays.
- **No workers, on Apps** (the operator only; an org user's page never fetches workers):
  - no apps and no live worker: "No workers yet", "Apps need a worker to run on." and "+ Add worker", which goes to Nodes and opens Add node;
  - apps but no live worker: an amber line above the list, "No live workers: machines can't start. Add a worker →", doing the same.
- **Empty-state icons:** Apps 🪁, Orgs 🏢, an unknown app 🔍, no workers 🖥️ (on Nodes and Apps), no nodes at all 🛰️, all at 40 px. The #motif drawing is gone.
- **One profile button** instead of the who row and Sign out:
  - a round avatar (the first letter on the accent-soft colour), the name, and the org (or "operator") under it;
  - a click opens a menu above it: "Signed in as alice, org acme" (or "as the operator"), then Sign out in red. `flyyy dashboard` (auth "local") has no Sign out, so the menu shows only who. The item keeps the #sign-out id;
  - Esc (focus back on the button) or a click outside closes it. It's `position: fixed`, placed from the button, so the sidebar's overflow doesn't clip it; a resize re-places it;
  - collapsed sidebar: the avatar alone. Phones: the avatar in the header, with the menu opening below it.
  - shortcut: for the operator the name and the line under it both say "operator", as specified.
- **Checked** in headless Chromium at 1280 and 375 px, light and dark, against the mock: Nodes with the control plane alone, with an edge and a pending worker, and full; Apps with no workers, with workers but no apps, and with apps and a dead worker; Orgs empty; the unknown app; the Events tab; the profile menu expanded, collapsed, for an org user and with auth local. "+ Add worker" from Apps (empty and the amber line) and from Nodes opens Add node on Worker.
- **The dashboard suite** checks the page has these pieces.

# Each node's host on the Nodes page

- **The join request carries `hostname`:** `flyyy join` sends /proc/sys/kernel/hostname, if it passes `wg::hostname_ok`: 1-64 printable ASCII characters, no spaces. Otherwise it sends none.
  - The control plane refuses a bad one with 400, before the token is spent. It keeps a good one in mesh.json, on the node.
  - `GET /v1/mesh` returns it per node. Local nodes (an edge on the control host) and `control` get the control host's own name.
  - Older mesh.json entries have none, and the page shows nothing for them.
  - The `Join` body is `deny_unknown_fields`, so a new `flyyy join` against an older control plane is refused. Both come from the same flake.
- **The Nodes page** shows "on nuc", muted, under the node name in both tables, before the rest of that line ("on nuc, x86_64, 4 machines"; "on nixvps, API and mesh hub"). A pending join's banner reads "worker7 on nixAir asks to join as worker".
- **Tests:**
  - unit tests for the check and for a mesh.json entry without a hostname; a join with a bad one is refused and its token stays unspent;
  - the mesh suite checks worker4's and the control plane's hostnames in `/v1/mesh`;
  - the hosts suite checks worker1's (#join), edge1's and the control plane's;
  - the dashboard suite checks the markup.
- **Checked** in headless Chromium at 1280 and 375 px, light and dark: Nodes full, with the control plane and an edge, and with the control plane alone; no mesh; Apps with no workers.

# New app's Continue, the tab bar, Configure's sizes, the CLI on its own

- **New app's first two steps select, then Continue:** a click on "From a template", "Your own image" or a template card selects it (the accent border). The footer's primary Continue goes on.
  - Continue is disabled on the template step until a card is selected. "From a template" is selected when the dialog opens, so Continue works at once.
  - A double-click, or Enter on a focused card, selects and goes on. Back (‹) keeps the selection, and focus goes to it.
- **The tab bar no longer scrolls:** its buttons' `margin-bottom: -1px` (for the underline over the bar's line) made the content 40 px in a 39 px `overflow-x: auto` box, so it scrolled both ways. The underline is now an inset shadow inside a 40 px button, and the bar has `overflow-y: hidden`.
- **Scrollbar audit:** a mock hook (`?audit`) lists every `auto`/`scroll` box whose content overflows it, and the page's width.
  - Run with classic scrollbars: Chromium `--disable-features=OverlayScrollbar`, and Firefox (Selenium, `widget.gtk.overlay-scrollbars.enabled` false).
  - Checked at 1280, 1100 and 900 px: Apps; the app page's four tabs; Nodes, with pending joins and IPv6 endpoints; Orgs; New app (choice, templates, Configure from a template and from an image); Add node as worker and as edge; Logs; New org; a destroy confirm; and the profile menu, expanded and collapsed.
  - Nothing overflowed, and the page's scrollWidth equals its clientWidth everywhere. The old page shows `div.tabs 40/39` there.
- **Configure's sizes:** the Machines stepper, CPUs, Memory and HTTP/TCP are 40 px tall, like the inputs. Machines and CPUs share a row (Name and Port the one above).
  - Memory is its own full-width row of 7 equal columns. Below 560 px it's a 4-column grid (4 + 3).
- **The CLI on its own:** `packages.flyyy` (a copy of the proxy package's `flyyy` binary; its closure is glibc and gcc-lib) and `apps.flyyy`.
  - Under each `flyyy …` command (deploy, orgs and users, a destroy confirm), once and muted: "No flyyy here? nix profile install '<flake>#flyyy'", from the page's flake URL.
  - #control's result added `cli: nix profile install '<flake>#flyyy'`; since cut down to two lines (up, the token), the README has it.
  - The hosts suite runs `nix run <flake>#flyyy -- orgs` on the control host, as carol (the operator token's owner), and gets the orgs table.
    - The flake there is a stand-in with no inputs whose `apps.flyyy` is the same package (tests/lib.nix), since the real flake's inputs aren't available offline.
    - The container's /nix/store is the host's, read-only and without a database, so no nix-daemon runs. The suite copies the stand-in's closure into a chroot store in carol's home, loads its registration, and runs `nix --store ~/cli run`. That runs in a user namespace, hence as carol: root there couldn't read the token.
    - shortcut: the real flake's `apps.flyyy` is only evaluated and built outside (`nix run .#flyyy -- --help`), not run against a control plane.

# "VMs" in the UI

- **The dashboard calls an app's machines "VMs":**
  - the strip, the "VMs (N)" tab and its table header, the Configure field, the Apps list's column, the Scale panel and its toast, the size line, the worker line ("x86_64, 2 VMs"), the destroy confirm, the per-VM charts, a tooltip and an aria-label;
  - singular and plural: "1 VM", "2 VMs";
  - "machine" stays where it means a host ("paste its command on that machine", "Turn your machines into a cloud").
- **What keeps "machine":** the API, the CLI, the schema, ids, classes and `data-*` attributes, and these notes' internals.
- **The control plane's events:** a deploy reads "deployed <image>, 2 VMs", a scale "scaled to 1 VM". The other event texts never named machines.

# Saying when there's no edge

- **No edge** means the mesh has no edge joined, approved or on the control host. One still asking to join doesn't count. Without one, nothing routes traffic to any app. It's known only with a mesh, so only the operator sees these notes.
- **Nodes:** an amber note in the Edges section (it was under "Control plane and edges"): "No edge: apps aren't reachable from outside. + Add edge", which opens Add node on Edge.
- **Apps list and the app page:** the amber line, as for no live workers: "No edge: apps aren't reachable. Add an edge →". It goes to Nodes and opens Add node on Edge.
- **Traffic tab:** when the metrics asked no edge (`asked.edges == 0`), it says "No edge is serving this app, so there's no traffic to show" instead of the request and latency charts and the totals line. The VMs' CPU and memory charts stay, since they come from the workers.
- **Checked** in headless Chromium at 1280 and 375 px, light and dark, against the mock's `noedge` (edge-fra still pending). The dashboard suite checks the markup.

# A link to the source on GitHub

- **`githubUrl(flake_url)`** maps the page's flake URL to the repo's page: `git+ssh://git@github.com/<owner>/<repo>?ref=<branch>` (also `.git`, `git+https://`, `https://github.com/…` and `github:<owner>/<repo>[/<branch>]`) becomes `https://github.com/<owner>/<repo>/tree/<branch>`. Without a branch it's the repo's page. Anything else (GitLab, `path:`, look-alike hosts) gives no link.
- **Where:**
  - the sidebar's footer, a "Source" row with the GitHub mark above the profile button; collapsed, the mark alone ("Source on GitHub" as its title);
  - phones: the mark in the header, left of the avatar;
  - the sign-in form: "Source on GitHub" at its bottom.
  - All open in a new tab (`rel=noopener`). Nothing is fetched from GitHub.
- **Tests:** dashboard-js checks the mapping (git+ssh with a ref, `.git` and a branch with a slash, `github:` with and without a branch, https, git+https, and six non-GitHub URLs). The dashboard suite checks the markup.

# The Nodes page: a Control plane card

- **Top to bottom:** pending-join banners, a Control plane card, Edges (N), Dashboards (N) (only with a dashboard's mesh node, which isn't an edge or a worker), and Workers (N).
- **The Control plane card** is one row of facts:
  - name and "on <host>";
  - state;
  - mesh IP;
  - the endpoint nodes dial;
  - peers, "3 of 3 connected" (the nodes with a mesh address, not those on the control host, with a handshake in the last 3 minutes; amber when some aren't);
  - the API.
  - Below 1100 px it's three columns under the name, and below 600 px two.
  - `GET /v1/mesh`'s `control` gains `endpoint` (from `--endpoint`, host:51820, as in every join token; null without it) and `api` (10.250.0.1:4000).
- **Edges:** Edge, State, Mesh IP, Endpoint, Handshake. An edge on the control host reads "on the control host", with "–" for endpoint and handshake. With no edge, the no-edge note ("+ Add edge") is there instead of the table.
- **Workers:** unchanged, with its empty state.
- **Tests:**
  - the mesh and hosts suites check `control.endpoint` (10.99.0.2:51820; #control's 10.99.8.2:51820) and `control.api`;
  - the dashboard suite checks the markup.
- **Checked** in headless Chromium at 1280, 1100 and 375 px, light and dark: full, no edge, and the control plane alone.
- **Found, not fixed:** the page containers' ids (`apps`, `nodes`, `orgs`) are also the pages' URL hashes, so opening `#nodes` on a long page scrolls to that element, past the toolbar (+ Add node). HEAD does it too (scrollY 66 in headless Chromium).

# Node names: any name, the hostname by default

- **One rule for every node's name** (worker, edge, a dashboard's mesh node): 1-32 of `[a-z0-9-]`, starting with a letter (`wg::node_name_ok`).
  - It replaces every `workerN` check: the mesh join and local-cert, the edges' route check (edge_routes.rs), the control plane's worker rows (control/model.rs), the agent's and the backhaul's `--worker`, `flyyy join --name`, join.nix, the host apps, the worker module's `strMatching` and the dashboard.
  - Nothing reads a number out of a name. Placement breaks ties by name order.
  - The test prelude keys the worker containers by name: `WORKERS["worker1"]`, `NAMES3`, `bh("worker1", …)`. `wnum` is gone.
- **`--name` is optional** for #join, #edge, #dashboard elsewhere, and `flyyy join`. The default is the hostname: lowercased, anything outside `[a-z0-9-]` a `-`, cut to 32, and a leading digit prefixed with `n` (still 32 at most). This is `wg::default_node_name`, with `default_name` in nix/host.nix and `defaultNodeName` in the page doing the same.
- **A taken name is refused:** a join whose name another key already holds gets 409, "name nuc is taken; pass --name <other>", before the token is spent.
- **Add node:** one optional Name field for Worker and Edge, with the placeholder "the machine's hostname". The command has `--name` only when the field is filled, and the field turns red for a bad or taken name. `nextFree` is gone, and the name is no longer remembered per browser (the edge's other fields still are).
- **The Nodes page** leaves out "on <host>" when the name is the one the hostname gives (no "nuc on nuc").
- **Tests:**
  - unit tests: the rule and the default (in Rust, and in dashboard-js for the page's copy); a taken name refused with the token unspent; `1box`, `Nuc` and `my_host` refused;
  - the join suite joins `plain` without `--name` (its hostname);
  - the workers suite's bad-name row is `9evil`.
- shortcut: with a non-UTF-8 locale, bash's `default_name` turns a multi-byte character into several `-`, where Rust makes one.

# Add node in two steps, edges' listen addresses, and Apps before the cluster is ready

- **Add node, like New app:** first two choices in New app's style, 🖥️ Worker ("Runs your apps' VMs. Any Linux machine with KVM; it can sit behind NAT.") and 🌐 Edge ("Serves your apps' HTTPS to the internet. Needs a public IP and the TLS certificate."). Click selects; double-click, Enter or Continue goes on. Then that kind's form, titled "Add worker" or "Add edge", with Back (‹). The Worker/Edge toggle is gone. The token is still made when the dialog opens. "+ Add edge" (the no-edge notes), "+ Add worker" (Nodes with no worker) and the Apps checklist's buttons open that kind's form directly; "+ Add node" opens the choices.
- **Edge listen defaults to `[::]:443`,** hinted "HTTPS, IPv4 + IPv6": on Linux one `[::]` socket takes IPv4 too, as long as `net.ipv6.bindv6only` is 0, the default (the edge doesn't set `IPV6_V6ONLY` either way). The `#edge` app already grants `CAP_NET_BIND_SERVICE` for a port below 1024. A browser whose saved fields (`flyyy.add-edge`) hold the old default, `0.0.0.0:8443`, gets the new one; its other saved fields stay.
- **Each edge's listen address:** the `edges` row an edge writes at every start (and every 60 s) has a new column, `listen`: its HTTPS address, else its plain HTTP one. A restart with other flags updates it. `/v1/mesh` adds `listen` to each edge node: the addresses of the `edges` rows whose load address is on that node's mesh address. A node can run several edges (the tests' `edges` does), so it's a list.
  - **The Edges table** drops the "edge" tag by the name (the table says Edges; the same for Dashboards) and has a Listen column: `[::]:443 · IPv4 + IPv6` for `[::]`, else the address as it is. Below 1200 px, mesh IP and endpoint go first and Listen stays; below 600 px it goes too.
  - **`flyyy mesh`** has a LISTEN column in the same words, `-` for nodes that aren't edges.
- **Apps, empty and the cluster not ready** (no live worker or no edge; the operator only, since org users get neither workers nor the mesh): one empty state, 🧩 "Two things before your first app", replacing "No workers yet". It lists:
  1. "A worker: Runs the VMs." with ✓ and the live workers' names, or "<name>: waiting for approval →" (a link to Nodes) for a pending one, or "+ Add worker";
  2. "An edge: Serves them to the internet." in the same way.
  - "+ New app" stays disabled under it, with "New apps can start once both are here."
  - With both there, the usual "Nothing running yet". With apps, the two amber notes as before.
- **Tests:** `page-overflow` (tests/page/) runs ui.py after the width sweep, in headless Chromium against the mock API, whose scenarios are now switchable: full, bare, a pending worker, both ready. It checks:
  - the Listen column's text and no role tag;
  - Add node's choices, Continue, Back, double-click, and the edge form's `[::]:443` in the field and the command;
  - a saved `0.0.0.0:8443` giving way to the new default while a saved certificate path stays;
  - the checklist, bare and with a pending worker, its buttons opening the right form, the usual empty state once both are there, and an org user's usual empty state.
  - The sweep still passes from 900 to 1400 px with the Listen column.
  - The mesh suite checks the real thing: the tests' edges node reports `["127.0.0.1:443"]` in `/v1/mesh`, and `flyyy mesh` shows it under LISTEN with `-` for workers.
- **Not checked:** an edge restarted with a different `--listen` updating its row (it's the same `ON CONFLICT … DO UPDATE` as `load_addr`), and a real `[::]:443` edge taking IPv4.

# Cloud Hypervisor instead of Firecracker

Machines are Cloud Hypervisor VMs now (nixpkgs' `cloud-hypervisor`, v54 when switched). Each is still a `systemd-run --user` unit, `fc-<id>` (the name kept, so a worker's running units are still its own after an upgrade), with no NIC and one vsock.
- **The launch** (agent/vm.rs `ch_args`): Cloud Hypervisor takes no config file, so its arguments go on the unit's command line: `--kernel`, `--initramfs`, `--cmdline`, `--cpus boot=N`, `--memory size=NM`, two `--disk`s (the image `readonly=on`, then the scratch, both `image_type=raw,sparse=off`: vda, vdb), `--vsock cid=3,socket=<run>/<id>.vsock`, `--serial tty --console off`. `<run>/<id>.json` is the launch record: exactly those arguments, as a JSON array. Option values are comma-separated, so a path with a comma is refused.
- **Vsock:** Cloud Hypervisor's hybrid vsock is Firecracker's protocol (`CONNECT <port>\n`, `OK <port>\n`), so the backhaul and the readiness check are unchanged. Unlike Firecracker's, it passes a guest's half-close on to the host as EOF; the relay still closes fully when the app does.
- **The socket's mode:** Cloud Hypervisor sets `umask(077)` itself at start, whatever the unit's UMask, so the socket comes out 0700 and the backhaul (another user, in the group) got `Permission denied`. The agent's readiness check now chmods it to 0770 first, before the machine is started, so the backhaul is never sent to a socket it can't open.
- **The VM's end:** Cloud Hypervisor reboots the VM on a guest reset instead of exiting, and a kernel panic (PID 1 exiting, then panic=1) is a reset. So flyyy-init no longer exits: when the app ends, on any failed step, and on a Rust panic (a panic hook), it powers off (`reboot(RB_POWER_OFF)`: ACPI on x86_64, PSCI on arm64), and Cloud Hypervisor exits 0. The app's code is still in the console line.
- **The guest kernel:** devices are virtio-pci, so `PCI`, `PCI_MSI`, `VIRTIO_PCI` replace `VIRTIO_MMIO`.
  - x86_64: `PVH` (Cloud Hypervisor boots vmlinux at its PVH entry), `ACPI` (power-off), the 8250 (ttyS0); `VIRTIO_MMIO_CMDLINE_DEVICES` is gone. kvm-clock, SMP and the MP table as before.
  - arm64: the PL011 serial and its console (ttyAMA0) replace the 8250 on OF; `PCI_HOST_GENERIC` (the ECAM host bridge); the PL031 RTC as before.
  - The agent picks the console by its own arch (`ttyAMA0` or `ttyS0`).
- **Processes:** Cloud Hypervisor's comm is `cloud-hyperviso` (cut to 15 characters); the local and join suites match that.
- shortcut: x86_64 is checked only as far as its kernel config (every option sticks, evaluated on the aarch64 host); a VM wasn't booted on x86_64.
- shortcut: a kernel panic not caused by init still resets, so Cloud Hypervisor reboots the VM; that boot finds no config.json (deleted on the first) and powers off.
