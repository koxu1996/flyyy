# Stage log

The spike and stages 2–10 as they were built, moved here from the design notes. Later work is in docs/DESIGN.md.

# Spike notes: Firecracker in systemd-nspawn, rootfs from containerd devmapper

**Verdict: feasible, with caveats.** On nixAir (M2, aarch64, 16K-page host kernel), 3 nspawn
workers each run 2 Firecracker VMs whose root disk is a containerd devmapper thin snapshot of
`traefik/whoami`. `verify.sh` gets 6 distinct hostnames from the host, and `down.sh` leaves the host clean.

```
sudo ./up.sh      # pool → containerd → import → 6 snapshots → 3 workers  (~20 s warm)
./verify.sh       # PASS / FAILED (add --failover for the stage 2 VM stop/start checks; uses sudo)
sudo ./down.sh    # teardown + leftover check ("host clean")
```

## What the nspawn workers needed
- `/dev/kvm`: `allowedDevices` rw + `bindMounts`.
- `/dev/net/tun`: `enableTun = true`.
- Root disks: `allowedDevices` on major `block-device-mapper` (minors change), plus up.sh
  `mknod`s a stable `/spike/<vm>.disk` node into a bind-mounted host dir. No `/dev/mapper`
  or udev inside the worker.
- CAP_NET_ADMIN for taps/bridge: already granted by `privateNetwork = true`; no extra capabilities.
- Not needed: privileged mode, PCI, userns tweaks, extra seccomp changes.

## Numbers (rough, single run)
| | |
|---|---|
| guest kernel build (allnoconfig + 28 options, 6.18) | a few minutes, 7.4 MiB `Image` |
| 6 snapshot prepares (`ctr snapshots prepare` + mknod) | ~0.9 s total |
| `systemctl start firecracker@a` → HTTP 200 | ~1.1 s (incl. nspawn exec overhead) |
| Firecracker RSS per VM (128 MiB guest) | ~34 MB |

## Deviations from PROMPT.md
- containerd and the thin pool run as a **transient `systemd-run` unit / script steps** instead
  of `virtualisation.containerd` + oneshot, so the host config stays untouched and teardown is trivial.
- extra-container's upstream flake lags this nixpkgs, so the flake calls the host
  extra-container's `eval-config.nix` directly (what `buildContainers` does).
- The IP plan gained a transit link: host `10.99.N.1` ↔ worker `10.99.N.2`. VMs are on worker
  bridge `10.100.N.1/24`, and the host routes `10.100.N.0/24` via the worker.

## Findings
- **4K guest on 16K host works** with no tweaks. The guest kernel has no PCI and no modules.
- containerd 2.x `ctr image import` needs `transfer.v1.local.unpack_config` for devmapper.
- NixOS template units: per-instance drop-ins reset `PATH`, so the template's `path =` is ignored.
- `extra-container destroy` leaves `/var/lib/nixos-containers/<name>/var/empty` (immutable).

## Top 3 problems before going further
1. **containerd placement / disk handoff.** Host containerd + mknod into workers works but couples
   the host to every VM's lifecycle. Per-worker containerd means solving dm inside nspawn (no udev, `/dev/mapper/control`).
2. **Init is a toy.** Entrypoint/Env go through the kernel cmdline (no spaces, ≤32 items, no
   WorkingDir/User). The app is PID 1 with no signal forwarding or zombie reaping. Stops are hard
   kills (journal replay on next boot). There is no RTC, so the guest clock is 1970.
3. **Storage sizing.** A loopback thin pool and a fixed 128 MB base size per snapshot. Real use needs
   a real block device, pool monitoring (thin pools fail badly when full), and per-image sizing.

---

# Stage 2: per-worker vhost proxy + second app

**Verdict: works.** Each worker runs `proxy` (Rust, tokio + hyper) on :80, routing
`<app>.flyyy.localhost` round robin over that worker's VMs of that app. There are 10 VMs: whoami a/b
on all 3 workers, and `hashicorp/http-echo` c/d on worker1 and worker2. `verify.sh --failover` passes:
- all 10 VMs answer directly;
- each worker's proxy alternates between its VMs of the right app;
- `echo` on worker3 and unknown vhosts → 404, and a missing Host → 400;
- with one VM down all requests go to the other; with both down → 502; round robin resumes after restart.

One table drives everything: `apps` (image, port, optional per-VM Cmd) + `vmsOf n` (placement).
It generates the Firecracker configs, the units, the proxy routes and `plan`, the per-VM list
that up.sh/verify.sh read.

## Numbers (rough, single run)
| | |
|---|---|
| proxy RSS | ~2.6 MB per worker |
| 10 VMs, Firecracker RSS | ~310 MB total, ~31 MB each |
| 10 snapshot prepares | ~1.7 s |
| `up.sh` warm (2 images, 10 VMs) | ~33 s |
| latency, 1000 sequential reqs (`oha -c 1`) | direct 0.11 ms avg → via proxy 0.14 ms avg (p99 0.41 → 0.26 ms) |

## Findings
- The stage 1 init and pipeline worked for http-echo unchanged (10 layers, chain ID, non-80 port,
  Cmd override). But the image says `User: 65532` and `WorkingDir: /home/nonroot`, which the init
  ignores, so the app runs as root in `/`.
- Connect-failure fallback needs a connect timeout: a stopped VM's IP gets no ARP reply, so
  without one connect hangs ~3 s. 1 s is the current shortcut, so a request that lands on a dead VM costs 1 s.
- Extra crates beyond tokio + hyper: `hyper-util` (only the `TokioIo` adapter) and `http-body-util`
  (one boxed body type for proxied and error responses). The proxy dials upstreams itself instead
  of using hyper-util's pooled client, so it can fall back on connect errors before the request body is consumed.

## Top 3 problems before going further
1. **No upstream connection reuse.** Every request dials a fresh TCP connection to the VM. That's fine at
   this latency, but under load it means connection churn and TIME_WAIT buildup. Pooling and retries
   need body-replay rules.
2. **No health checks.** A dead VM costs a 1 s timeout per request that hits it; there is no ejection.
   Placement-driven routing (or health state from the worker's own systemd units) is the next step.
3. **Image config fidelity.** User, WorkingDir and args with spaces are still not honoured. The cmdline
   approach should move to a config drive / vsock handoff before more images are tried.

---

# Stage 3: routes from Corrosion subscriptions

**Verdict: the proxy side works; Corrosion's own replication did not survive `kill -9`.**
Each worker runs a Corrosion agent (gossip over the host, plaintext QUIC) and its proxy builds
routes from `SELECT id, app, addr FROM instances WHERE worker = ?` via `corro-client`, with a
`backon` resync loop around it. `up.sh` writes the 10 instances once through worker1; worker2/3 get
them by gossip. `verify.sh --failover` passes. `sudo ./storm.sh` (one sudo for the whole run, ~9 min):
plain, proxy-restart, clean agent restart and lag all end with proxy == its agent == worker1's agent,
**0 gaps and 0 reorders** in every proxy's journal. 503 before the first snapshot: confirmed.

## Subscription facts on v1.0.0 (vs PROMPT-stage3.md)
- Fresh subscription: `eoq.change_id` = 0 (also for an empty result). `from` is exclusive.
- **Changes are diffs of the query result, not writes.** Repeated writes to a row coalesce:
  1000 transactions → ~80 change events per proxy. Distinct rows are not coalesced (2000 inserts +
  2000 deletes → exactly 4000 contiguous events).
- Every `POST` makes a new subscription id, even for identical SQL, so the proxy first re-attaches with
  `GET /:id` and only POSTs if that fails.
- Clean agent restart: the subscription is restored, but the open stream ends cleanly (the SDK does
  not reconnect on that, as expected) → resync. `kill -9`: "could not restore subscription" → new POST.
  A kill mid-stream surfaced as a truncated-JSON parse error, not an I/O error, so the SDK's own
  `from` reconnect never ran; the outer loop handled it.
- **Lag:** a SIGSTOPped proxy with 4000 pending changes over 5 min (past one prune) was never
  dropped; the agent kept the stream open and the proxy caught up in order. No lagging-client error
  or pruned-`from` error could be triggered, so that resync path is only covered by the generic loop.
  `MissedChange` was never observed.

## Ordering, latency, memory
- Each proxy applied the single writer's upserts in write order (rising ports, 0 reorders). The proxies
  subscribe to disjoint rows, so cross-agent order is only checked indirectly.
- worker1 write → worker3 proxy applied: ~0.1–0.6 s (5 samples).
- Agent RSS 32–55 MB. Proxy RSS ~4.8 MB (stage 2: 2.6 MB), binary 7.0 MB (reqwest + bundled SQLite).

## Deviations
- Corrosion is **built from source**: the release binary's jemalloc aborts on 16K pages
  ("Unsupported system page size"), fixed with `JEMALLOC_SYS_WITH_LG_PAGE=16`. It is pinned to
  **main HEAD `190b842`** (2026-10-27), not v1.0.0, since main has many replication fixes and no release yet.
- Everything else is on its latest version: nixpkgs nixos-unstable `31ecbaca`, both images at their latest
  digests, all crates at their latest releases (`corro-client` 0.2.0-alpha.0 is still the newest).

## Top 3 problems before going further
1. **Permanent agent divergence after `kill -9`, also on main HEAD.** It's intermittent: 3 permanent
   splits in 22 `kill -9` storms (v1.0.0 2, main 1, plus 1 main run not checked past 90 s), seen on
   both nixpkgs pins. Usually the restarted agent catches up in 2–45 s (~30 s of that is
   rediscovering peers: "No existing cluster member state to load!").
   When it fails, the restarted agent keeps the pre-delete `addr` of a deleted and re-inserted row
   (col_version 0, an old db_version) under the new causal length (cl 133). It has the revival marker
   and `worker`/`app`, but neither later `addr` write. It has no bookkeeping gaps and no buffered
   changes, so sync never re-sends them; restarts don't help, and only a new write to the row heals it.
   The revival marker is restamped with the version of the lost `addr` change, so that change was
   processed but its value wasn't written: this looks like a merge bug, not lost gossip. Evidence and
   the upstream draft are in the session scratchpad. Meanwhile it would need a periodic agent-vs-peer checksum.
   **Update (root cause and fix; reported as [superfly/corrosion#596](https://github.com/superfly/corrosion/issues/596),
   repro: [koxu1996/corrosion-dup-seq-divergence](https://github.com/koxu1996/corrosion-dup-seq-divergence)):** it is not caused by the crash, only by being behind.
   cr-sqlite's resurrect path stamps the `-1` marker with the incoming change's `(db_version, seq)`, so a relay
   node's `crsql_changes` can hold two rows with the same seq. Corrosion's `ChunkedChanges` stops at the first row
   with `seq == last_seq`, so the relay serves only the marker, and the restarted node never gets the `addr` change.
   The deterministic repro diverges 22 of 23 runs. The flake now applies `patches/0001-chunked-changes-drain-dup-seq.patch`
   (drain rows with the same seq; `cid` added to the buffered-changes key). With it, 14 of 14 `agent-kill` storms
   converged here, in 26–40 s (the ~30 s is still peer rediscovery). Before the fix about 1 storm in 7 split, so a
   ~1 in 9 chance remains that this was luck; the deterministic repro is the stronger evidence (3 of 3 fixed).
2. **Not an event log.** Coalescing and multi-row transactions arriving as separate events mean the proxy
   sees a correct, ordered sequence of *states*, not every write, and it can briefly see
   intermediate states. That's fine for routes, but not for anything that needs every write.
3. **Security and ownership.** Plaintext gossip, the API on 0.0.0.0:8080 without auth, a single writer,
   and VMs don't register themselves: lifecycle-driven writes need a decision on who owns which row.

---

# Stage 4: anycast-style edges + backhaul

**Verdict: works.** Two edge containers (`edge1`, `edge2`) share the host network namespace and both
listen on `127.0.0.1:80` with `SO_REUSEPORT`. Each routes `<app>.flyyy.localhost` round robin over all of
the app's VMs across workers, from its own Corrosion agent (5 agents in the cluster now). It reaches a VM only
via `CONNECT <instance-id>` to that worker's backhaul (`10.99.N.2:4280`). The backhaul is the old worker proxy,
which now only dials instances in its own subscription and pipes bytes. Host routes to the VM subnets and worker
forwarding are gone. `verify.sh --failover` and `storm.sh plain proxy-restart edge-restart` pass
(0 gaps on all 5 proxies, `probe` routed through the edges).

From the host, a plain `curl http://whoami.flyyy.localhost/` now works, since curl resolves `*.localhost` to 127.0.0.1.

## What SO_REUSEPORT across two nspawn containers needed
- `privateNetwork = false`, plus `networking.firewall.enable = false` in the edge containers: they share the host netns,
  so their firewall unit would rewrite the host's.
- **The same UID in both edges:** Linux only joins sockets to one reuseport group if they have the same effective UID.
  So the edges run as a fixed `edge` user (uid 2080) instead of `DynamicUser`, and `PRIVATE_USERS=no` keeps it the
  host UID. `TcpSocket::set_reuseport` from tokio means no new crate.
- The edge agents gossip on worker1's host-side transit address (`10.99.1.1:880e`), so `up.sh` starts the edges after
  the workers (a second `extra-container create`).

## Numbers (rough, single run)
| | |
|---|---|
| kernel spread over 1000 new connections | edge1 513 / edge2 487 |
| latency, 1000 sequential reqs (`oha -c 1`) | 0.50 ms avg, p50 0.41, p99 2.0 (stage 2: 0.14 via proxy, 0.11 direct) |
| edge proxy RSS / backhaul RSS | ~5.0 MB each |
| Corrosion agent RSS (workers and edges) | 41–46 MB |

## Findings
- The extra ~0.35 ms is one more proxy hop plus one new backhaul TCP connection and a CONNECT round trip per
  request. Keepalive to the edge makes no difference (0.50 vs 0.50 ms avg), because the backhaul is per request anyway.
- Validation works: unknown ids, other workers' ids and raw `ip:port` targets all get 404 and are never dialled,
  and non-CONNECT requests get 405. Edges don't see VM addresses at all (their query has no `addr`).
- Failover is per request: a stopped VM or backhaul is skipped and the next instance is tried. With all whoami VMs
  down, a 502 takes ~6 s (6 × the 1 s VM connect timeout at the backhauls). With one edge stopped, the kernel sends
  everything to the other.
- No new dependencies: only the tokio `io-util` feature (`copy_bidirectional`) and hyper's built-in CONNECT upgrades.

## Top 3 problems before going further
1. **Backhaul cost per request.** A fresh TCP connection + CONNECT per request is cheap on one host but costs
   2 RTTs edge↔worker in reality. It needs a pooled or multiplexed backhaul (HTTP/2 or persistent tunnels over WireGuard), which
   then needs per-stream target validation and flow control.
2. **No health state.** Edges learn nothing from failures: every request re-tries dead instances, and they pay the
   1 s timeout at the backhaul each time. The backhaul's 404/502 could feed edge-side ejection, and VM lifecycle should
   write the rows (still `up.sh`).
3. **The anycast stand-in is thin.** SO_REUSEPORT on one host has no locality, and stopping an edge resets the
   connections still queued on its socket (not measured). Edges also depend on worker1's link for gossip
   and on a static backhaul map.

# Stage 5: dynamic machines, desired state → machine agent → facts

**Verdict: works.** VMs are no longer defined in Nix. Desired state is a Corrosion table, `machines` (written by
`lib.sh machine_*` through worker1). A machine agent on each worker (`proxy agent`, a third mode of the same crate)
follows its own worker's rows and makes them real: snapshot, disk node, IP and tap, then a runtime `firecracker@<id>`
unit. It reports facts into `machine_status` (state, addr, error). The edges and backhauls now route from these facts
(`state = 'started'`). `up.sh` just seeds the 10 old plan machines as rows. `verify.sh --failover`, `lifecycle.sh`
(create/stop/start/destroy, `kill -9` of the agent, orphan cleanup, failures) and `storm.sh plain churn` all pass.

## Snapshots and units from inside nspawn
- The containerd socket is bind-mounted into each worker, and the agent shells out to `ctr` (prepare/mounts/rm). The
  container has no `/dev/mapper`, so the device number comes from `/sys/block/dm-*/dm/name`.
- `mknod` of the disk node failed with EPERM until the device rule became `block-device-mapper rwm`: creating a node
  needs the cgroup `m` permission as well as `rw`. That was the only blocker.
- One generic `firecracker@` template. The agent writes `/run/fc/<id>.json` and its `fc-pre`/`fc-post` read the tap
  name from it. Taps are `tap-<last octet>`, because of the 15-character limit on interface names. systemd supervises
  the VMs, so an agent crash leaves them running (the PIDs were unchanged after `kill -9`).

## Numbers (rough, single runs)
| | |
|---|---|
| create → started (facts) | 1.8 s avg over 20 (p50 2.0, max 2.4) |
| create → answering through the edges | 2.3 s |
| stop → out of both edges | 1.2 s |
| destroy → `destroyed`, nothing left | 0.3 s avg over 20 (max 0.5) |
| agent `kill -9` → restarted, resynced, no actions | ~1.2 s, VMs untouched |
| orphan unit → torn down | ≤ 10 s (one resync tick) |
| 10 seed machines after the insert in `up.sh` | 3 s |
| agent RSS | 5.4 MB |

## Findings
- **Single writer per row, and no deletes**, keep us clear of the stage 3 divergence pattern. Destroyed machines keep
  their rows, and ids are never reused. Storm probes still delete and re-insert, but in `machine_status` only.
- Level-triggered reconcile plus a pure diff (`plan()`, unit-tested) made the orphan, restart and half-done destroy cases
  fall out of the design. The agent never acts before its first snapshot, so an empty table can't wipe a worker.
- Facts lag desired state by the gossip delay. During churn, 162 edge requests (of ~6.8k) hit a machine that was
  already stopping and fell back to the next one. All were 200, but only because of the stage 4 fallback.
- Failures are per machine: an unknown image gives `failed` with the error and retries at 1/2/4/8 s… (backon). An
  invalid row (for example quotes in cmd) gives `failed` and nothing is run.

## Top 3 problems before going further
1. **No drain on stop.** The agent reports `stopping` and stops the VM straight away, so edges briefly route to a dead VM
   (the 162 fallbacks). It should wait until the edges have dropped the machine, or for a fixed grace period, and long
   requests need real connection draining.
2. **Readiness is one TCP connect.** `started` means the port accepted a connection once. Nothing notices a VM that
   hangs later, and there are no health checks, so the edges keep routing to it.
3. **Operator side is thin.** Placement is by hand, the spec can't change, destroyed rows are never collected, and any
   client of worker1's Corrosion API can write `machines`: there's no API or auth in front of the desired state.

# Stage 6: control plane + `flyyy` CLI

**Verdict: works.** A `control` container (its own Corrosion agent, the 6th in the cluster) runs `proxy control`: an
HTTP/JSON API on `10.99.0.2:4000` (bearer token) and a level-triggered reconciler that turns `apps` rows
(image, port, cmd, count) into managed `machines` rows. `flyyy` is a thin client of that API: `deploy`, `scale`,
`destroy` (waiting until converged), `ls` and `status`. `up.sh` now deploys whoami and echo with it, and the plan
and image tarballs are gone: the machine agents pull digest-pinned images through the host's containerd.
`deploy.sh`, `lifecycle.sh`, `verify.sh --failover` and `storm.sh plain churn agent-kill` all pass.

## Pulls
- `ctr images pull --platform linux/arm64 --snapshotter devmapper docker.io/<repo>@sha256:…` over the bind-mounted
  socket worked first time. The agent then reads index → arm64 manifest → config with `ctr content get`, and computes
  the chain id with `sha256sum` (no hashing crate). Already-present images skip the registry entirely.
- A digest that doesn't exist fails fast ("not found") as a normal machine failure: `failed`, the error, backoff.

## Numbers (rough, single runs)
| | |
|---|---|
| deploy 3, cold image (pull) → converged | 6.6 s (6 machines at `up.sh`: 7.6 s) |
| deploy 3, warm image → converged | 2.5 s |
| scale 3 → 5 / 5 → 2 / → 0 / 0 → 2 | 2.3 s / 0.5 s / 0.3 s / 2.0 s |
| control plane `kill -9` → API back | 1.8 s; a scale-up 3 → 10 finished with exactly 7 machines created |
| data plane while the control plane was down/killed | 432 samples, all 200 |
| edge fallbacks while scaling 5 → 2 | 1 of 235 requests (drain, `TODO.md`) |
| control plane RSS | 5.5 MB (its Corrosion agent ~45 MB) |

## Findings
- **Stateless was cheap:** every request and every reconcile round reads Corrosion through the control plane's own
  agent, so its own writes are visible in the next round and nothing is cached. No subscriptions were needed: the
  only writer of `apps` is the API, which wakes the reconciler, plus a 5 s tick for restarts.
- **One writer per `machines` row, for real now:** managed rows by the reconciler, and `lib.sh`'s unmanaged ones
  (`managed = 0`, which the control plane ignores) also through the control plane's agent.
- Placement is per app first (fewest of this app's machines), then by overall load, so every app spreads evenly.
  Scale down removes not-yet-started machines first.
- Re-deploying a destroyed app updates its `apps` row in place (no delete + re-insert), with fresh machine ids. A
  different spec on an active app gets 409: no in-place updates yet.
- Validation is server-side (app names, digest-only images, port, count ≤ 20, cmd, unknown fields), and rejected
  requests write nothing. The token is compared in constant time and reaches the `DynamicUser` service via `LoadCredential`.

## Top 3 problems before going further
1. **No updates of a running app.** A new image means destroy + deploy (downtime). Rolling/blue-green needs a release
   concept on machines plus health-gated cutover, and drain (`TODO.md`) first, or every rollout drops into fallbacks.
2. **Single control plane, static worker list.** Stateless makes a second instance possible, but two reconcilers would
   double-create without per-app leases. Placement knows nothing about worker capacity or health.
3. **Pull path is serial and unauthenticated.** A pull blocks that agent's whole reconcile round. There are no
   registry credentials, rate-limit handling or image GC, and every worker pulls through one shared containerd namespace.

# Stage 7: workers announce themselves

**Verdict: works.** Each machine agent upserts its worker's row in a new `workers` table (name, backhaul, `started_at`,
`heartbeat_at`) every 5 s, from its own task. The edges get backhaul addresses from a joined subscription
(`machine_status ⋈ workers`), and the control plane places new machines only on workers whose heartbeat is < 30 s old.
Neither unit has a worker list any more. A `worker4` container started at runtime announced itself, got the next
machines, and was routed by both edges without restarting them or the control plane. `verify.sh --failover`, `deploy.sh`,
`lifecycle.sh`, `storm.sh plain churn edge-restart agent-kill` and the new `workers.sh` pass.

## Subscription shape
- **The joined subscription worked** (risk 1): one row type `[id, worker, app, backhaul]` and the stage 3 loop unchanged.
  A machine whose worker row is missing isn't routed, and appears when the row arrives. Heartbeats change only
  `heartbeat_at`, which isn't selected: 0 subscription changes on both edges over 60 s.
- Facts → routable at the edges takes ~0.4 s after `converged` for a new app (gossip + the join), so tests that check
  routing right after a deploy have to wait for it.

## Numbers (rough, single runs)
| | |
|---|---|
| worker4 `create --start` → alive | 13 s (container boot + Corrosion join) |
| alive → first machines placed and started (scale 6 → 8) | 1.8 s |
| agent stopped → seen dead | 27 s (30 s window minus the age of the last heartbeat) |
| container restarted → alive again | 6.6 s |
| heartbeat writes | 0.6 transactions/s for 3 workers (1 per 5 s each) |

## Findings
- "Alive" comes from the machine agent, not the backhaul: with worker4's agent stopped, its machines kept serving through
  its backhaul, and only new placements avoided it.
- Bad rows (an unparsable backhaul, a name that isn't `workerN`) are skipped and logged by the edges, and shown as invalid
  by `flyyy workers`; the control plane never places on them. But a *valid-looking* row with a fresh heartbeat would get
  machines: anything that can write Corrosion is trusted (no gossip auth).
- **No drain on leave:** scale-down ties go to the busiest worker overall, so worker4 (no echo machines) kept 2 whoami
  machines when its container stopped. Their facts stayed `started`, so the edges fell back past them (all 200). They
  came back when worker4 rejoined.
- `up.sh` now refuses to run over an existing stack. A test run here once hit a stack started outside the run from a
  half-edited tree, and the old `set -e` failure was hidden behind `| tail`.

## Top 3 problems before going further
1. **Stale facts outlive their worker.** A dead worker's machines stay `started` forever; edges keep paying fallbacks
   and placement counts them. They need to be marked by the control plane (or the edges) from the heartbeat, plus drain
   and rescheduling.
2. **Trust in gossip.** Workers rows and facts are taken at face value: one bad writer can attract machines or redirect
   edge traffic. Needs authenticated gossip (Corrosion TLS + client certs) or signed rows.
3. **Clock-based liveness.** Fine on one host; across hosts it needs skew tolerance or a counter-based heartbeat,
   and 30 s means up to half a minute of placing onto a dead worker.

# Stage 8: worker-built EROFS images instead of containerd + devmapper

**Verdict: works, and the host is plain.** No containerd, thin pool, loop or dm devices, host modules or device rules
any more. Each machine agent builds a read-only **EROFS** image per pinned digest in the background
(`crane export - | mkfs.erofs --tar=f`, as `nobody`), cached and shared by all machines of that digest on the worker.
Every boot gets a fresh sparse ext4 scratch file, and the guest initrd stacks them with overlayfs. `verify.sh --failover`,
`deploy.sh`, `lifecycle.sh`, the new `images.sh`, `storm.sh plain churn agent-kill` and `workers.sh` pass.

## Paths taken
- **Risk 1 (registry from a worker):** the host resolver is Tailscale's `100.100.100.100`, which containers can't reach,
  so workers get public resolvers. `up.sh` adds a masquerade rule for `10.99.0.0/16` in an nft table of its own
  (`ip spike`), which `down.sh` deletes. No host-side fallback was needed.
- **Risk 2 (image format):** **EROFS instead of ext4.** `mkfs.erofs --tar=f` reads the stream from stdin and sizes the
  image from its content. That removed both the size formula and the tar copy on disk, and kept owners, modes,
  symlinks and the sticky bit. The ext4-from-tar path also worked locally (31 MiB per image vs 8–12 MiB) and is the
  documented fallback.
- **Builds** are a per-digest state machine (resolving → building → checking → ready, or failed with backoff),
  persisted after every step. The reconcile loop only reads it (machines show `pulling`), and a finished build wakes it.

## Numbers (rough, single runs)
| | |
|---|---|
| cold build per worker (whoami v1.10.3) | 2.8 s, 6.4 MB EROFS; `up.sh` seeds: whoami 9.9 s / echo 5.6 s to converged |
| image sizes | whoami 12.1 MB, echo 8.7 MB, whoami v1.10.x 6.4 MB |
| warm create → started (churn, 20) | 1.6 s avg, as in stage 6 (pulls were already cached then) |
| fresh scratch on disk | 0.6 MiB of metadata in a 1 GiB sparse file (plus what the guest writes) |
| `up.sh` / `down.sh` | 53 → 46 lines / 21 → 16 lines, with no root-only disk steps left |

## Findings
- **Not blocking:** while the cold build ran, the same worker stopped an echo machine 2.75 s before the image was ready.
  Two apps on one digest gave exactly one build per worker.
- **Crash safety:** killing `crane export` mid-stream gave a failed build, a retry 1 s later, and exactly one image.
  A `kill -9` of the agent mid-export left a `.tmp`, which was removed on restart. The build resumed without fetching
  the config again.
- The images are mode 0444, Firecracker opens them `O_RDONLY`, and the checksum was unchanged after 9 machines.
- Reading a *running* guest's scratch from outside needed a journal-less scratch and a `sync` in the initrd.
  The scratch is ephemeral, so the journal bought nothing anyway.

## Top 3 problems before going further
1. **Every worker fetches every image itself:** N× registry traffic and Docker Hub rate limits, no layer dedupe, and no
   image GC (the cache only grows). A per-host or regional cache, or lazy fetching, would fix the cost.
2. **Nothing persists:** apps that need state need volumes. Those need allocation, attach/detach on a worker, and a
   placement constraint that keeps the machine where its volume is.
3. **The transit net now has outbound NAT,** so guests can't reach the internet (no route from 10.100), but workers
   can. That's broader than "pull images", and the hard-coded public resolvers are a shortcut.

# Stage 9: vsock instead of VM networking

**Verdict: works, and VMs have no network at all.** No bridge, taps, IP/MAC allocation, `ip=`, `/dev/net/tun` or
guest NIC driver. Each VM gets one Firecracker vsock device (`/run/fc/<id>.vsock` on the worker). The backhaul does
Firecracker's `CONNECT <port>` handshake on it and pipes bytes. A static `socat` in the initrd forwards vsock `<port>`
to the app on `127.0.0.1` (`lo` only), so apps are unchanged. `verify.sh --failover`, `deploy.sh`, `lifecycle.sh`,
`images.sh`, `storm.sh plain churn agent-kill` and `workers.sh` pass.

## Choices
- **Shim:** `pkgsStatic.socat` 1.8 (built `WITH_VSOCK`), started by init *before* any mount moves `/dev` (the first
  try redirected to `/dev/null` after it had moved, and every guest panicked). It survives `switch_root`, and the
  app is still PID 1. The machine id and port reach init (and the app) as `FLYYY_MACHINE_ID`/`FLYYY_PORT` kernel
  params. init sets the hostname from the id, which `ip=` used to do.
- **Socket permissions:** the backhaul runs as a fixed `backhaul` user. `/run/fc` is `2750 root:backhaul` (tmpfiles)
  and `firecracker@` has `UMask=0007`, so sockets come out `0770 root:backhaul` with no chmod race after start. The
  backhaul has no other privileges.
- **Facts:** `addr = vsock:<id>:<port>`. The backhaul parses it strictly (the id becomes a path) and only accepts ids of
  its own worker's machines. A `../` path and another worker's machine were skipped, logged and never dialled.
- **Readiness:** handshake `OK` plus the guest not closing within 200 ms (socat closes at once if the app isn't
  listening), with a 30 s limit. whoami on the wrong port and an echo that exits both end `failed` with a reason,
  never `started`.

## Numbers (rough, single runs)
| | |
|---|---|
| latency, edge → backhaul → vsock → VM (`oha -c 1`, 1000) | 0.83 ms avg, p50 0.76, p99 2.7 (stage 4 TCP: 0.50 / 0.41 / 2.0) |
| create → started (churn, 20) | 1.36 s avg (stage 8: 1.6 s) |
| code | +221 / −95 lines (proxy + flake): the network setup went, the vsock dial, `addr` validation, readiness and tests came |

## Findings
- Isolation is structural now: no shared L2 between VMs, no NIC, and a per-VM vsock socket. Before, VMs on one worker
  could reach each other over `br0`.
- The extra ~0.3 ms per request is probably the Unix-socket handshake through Firecracker's vsock muxer, plus socat
  forking a process and making a TCP connect per connection in the guest (not profiled).
- Shim memory inside the guest wasn't measured (no in-guest shell).

## Top 3 problems before going further
1. **socat per connection:** a fork and a TCP hop per request. A long-lived in-guest proxy with connection reuse (or
   apps listening on vsock directly) would cut both latency and guest memory.
2. **One port per machine:** multi-port apps, or anything else that needs to reach the guest (exec, logs, health),
   would need a small guest agent multiplexing over vsock, which is roughly what Fly's init does.
3. **No guest egress at all:** apps that call out (DNS, APIs) need an explicit, filtered outbound path, for example
   a vsock-to-proxy on the worker, rather than a NIC again.

# Stage 10: static guest launcher + config file (interim)

**Verdict so far: works; the compat retest is unfinished.** Apps no longer go through the kernel cmdline. The agent
builds a `config.json` (argv = Entrypoint + cmd/Cmd, env = image Env + app env + HOSTNAME, WorkingDir, User) with
Docker's semantics, and puts it on each boot's fresh scratch (`mke2fs -d`). The initrd hands PID 1 to `flyyy-init`
(`guest/`, static musl, 627 KB):
- it fixes `/dev` (fd, std*, pts, a 64 MiB shm tmpfs) and writes `/etc/hosts`, `hostname` and `resolv.conf`;
- it resolves `User` against the image's passwd/group, changes to `WorkingDir` and looks up argv[0] in `PATH`;
- it gives the console to the app's user, so a non-root app can reopen `/dev/stdout`;
- it runs the app with exactly the configured env, and stays PID 1 to reap and forward signals.

The API and CLI take `env` and an argv (`flyyy deploy … --env K=V -- <argv…>`, which replaces `--cmd`). The kernel
gains TMPFS and SYSVIPC.

## Verified
- `guest.sh`:
  - 44 args with spaces, quotes, `=`, `--`, empty strings, tabs, newlines and unicode arrive byte for byte;
  - env is exactly the image Env + app env + HOSTNAME/HOME, with no `pci=off`, `earlycon` or `FLYYY_*` leaking;
  - PID 1 is the launcher;
  - `/dev`, `/etc/hosts` and `/dev/stdout` work, including as non-root;
  - umami runs as nextjs (uid 1001) in `/app`;
  - 20 abandoned grandchildren are reaped, SIGTERM is forwarded, and the app's exit ends the VM.
- `deploy.sh` (with the new env/argv rejections) and `storm.sh plain churn` pass with the final launcher.
- `verify.sh --failover`, `lifecycle.sh`, `images.sh` and `workers.sh` passed on the launcher build just before the
  `/dev/console` change.
- `lifecycle.sh`'s stop check now waits for the backhaul's table (a 404 race seen once), but that change hasn't been
  re-run yet.

## Compat retest (partial, needs a rerun)
- **30/61 work** (baseline 22/61, of which only 12 without a workaround): 29 with no workaround, plus postgres with
  `--env POSTGRES_PASSWORD`.
- No image fails on C1/C2/C4/C5/C8/C9 any more: every one gets past the launcher.
- redis, valkey and keydb reach "Ready to accept connections" but were marked BROKEN, probably because the probe
  (`nix run nixpkgs#redis` fetching redis-cli inside its 10 s timeout) timed out.
- New first blockers, from the consoles:
  - **memory** (128 MiB): clickhouse, rabbitmq and cockroach OOM-killed; nextcloud's opcache shm;
  - **readiness** (30 s): mongo, kafka, cassandra, grafana, gitea, keycloak, n8n;
  - **cgroups**: redpanda, and mariadb (`/proc/self/cgroup`);
  - **IPv6**: sws, adminer; valkey and keydb warn but work on IPv4;
  - **clock at 1970**: ghost, loki;
  - **image `VOLUME` dirs not created**: uptime-kuma, and cockroach's data dir (a new finding: Docker creates them);
  - **file capabilities**: vault (`setcap: Not supported`);
  - **ENOSYS, probably inotify**: dotnet, jaeger;
  - **config that Docker would also need**: haproxy (config file), opensearch (admin password env), postgres
    without env;
  - **C17 segfaults**: nginx (now past its entrypoint) and typesense;
  - **unclear**: mysql with env (`data directory … unusable`), umami (a node crash).
