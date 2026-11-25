/ponytail full

## Goal
Stage 5 of the spike (stages 1–4: `PROMPT.md`, `PROMPT-stage2.md`…`PROMPT-stage4.md`, all summarised in `NOTES.md`).
Until now the VMs were fixed at build time: Nix generated a `firecracker@<vm>` unit and `/etc/fc/<vm>.json` per VM,
`up.sh` prepared their snapshots and registered them in `instances`. Make machines **dynamic**:
- **Desired state** lives in Corrosion, in a `machines` table: which machine, which app/image, on which worker,
  started or stopped or destroyed.
- Each worker runs a **machine agent** (below: "the agent", not to be confused with the Corrosion agent). It watches
  its own worker's rows in `machines` and starts, stops and tears down VMs until reality matches them.
- The agent reports **facts** back into Corrosion, in a `machine_status` table: the current state, the address, and
  the last error. The edges and backhauls route from these facts, not from the desired state.

## Target topology (changes from stage 4 in bold)
host
├── containerd + devmapper (unchanged), **images imported by `up.sh` into a static catalogue**
├── 2 × nspawn edges (unchanged; **their subscription reads `machine_status`**)
└── 3 × nspawn workers
    ├── corrosion agent (unchanged)
    ├── **agent.service: subscribes to `machines WHERE worker = ?`, reconciles, writes `machine_status`**
    ├── proxy.service → backhaul (unchanged; **its allowlist is this worker's `machine_status` rows that are started**)
    └── Firecracker VMs: **`firecracker@<id>` instances created at runtime by the agent; no per-VM Nix config**

## Data model
```sql
-- Desired state. Written only by the operator (lib.sh), always through worker1's Corrosion agent.
CREATE TABLE machines (
  id TEXT NOT NULL PRIMARY KEY,      -- [a-z0-9-]{1,32}: m-<8 hex> from lib.sh, worker<n>-<vm> from up.sh; never reused
  worker TEXT NOT NULL DEFAULT "",   -- placement is chosen by the operator
  app TEXT NOT NULL DEFAULT "",
  image TEXT NOT NULL DEFAULT "",    -- a catalogue key, e.g. "whoami"
  cmd TEXT NOT NULL DEFAULT "",      -- optional Cmd override, as in today's plan
  state TEXT NOT NULL DEFAULT ""     -- started | stopped | destroyed
);
-- Facts. Each row is written only by the agent on that machine's worker.
CREATE TABLE machine_status (
  id TEXT NOT NULL PRIMARY KEY,
  worker TEXT NOT NULL DEFAULT "",
  app TEXT NOT NULL DEFAULT "",      -- copied from machines, so the edge subscription stays single-table
  state TEXT NOT NULL DEFAULT "",    -- creating | started | stopped | failed | destroyed
  addr TEXT NOT NULL DEFAULT "",     -- ip:port, set while it has an IP
  error TEXT NOT NULL DEFAULT "",
  updated_at INTEGER NOT NULL DEFAULT 0
);
```
- `instances` goes away. Edge: `SELECT id, worker, app FROM machine_status WHERE state = 'started'`.
  Backhaul: `SELECT id, addr FROM machine_status WHERE worker = ? AND state = 'started'`. The rest of the stage 3/4
  subscription loop stays as it is.
- **Every row has exactly one writer**, and destroy is a state, not a `DELETE`. Machine ids are never reused.
  This deliberately avoids the delete + re-insert of the same primary key that triggered the stage 3 Corrosion
  divergence, and the multi-writer cases (#530/#531).
  `// shortcut: destroyed rows are kept forever; GC them once the Corrosion bug is understood`.
- `// shortcut: the operator picks the worker; no scheduler or capacity checks`.
- `// shortcut: the spec is immutable except for state; replace = create a new machine and destroy the old one`.

## The machine agent
- Same crate, a third mode, `proxy agent <corrosion api> workerN ...`, so `session`/`follow`/`publish` are reused.
  New dependencies follow the repo's Rust rule (latest exact version, `default-features = false`, explicit `features`,
  a comment). Most likely, only tokio `process`/`fs` features are needed.
- **Level-triggered reconcile:** on every subscription change, and also every ~10 s, compute
  `desired (its machines rows) × actual (local state)` → actions, and apply them idempotently. Write the diff as a
  **pure function**: that's the one small unit test, covering create, start, stop, destroy, orphan and failed-with-backoff.
- **Actual state is local, never read back from Corrosion:** `/var/lib/machine-agent/<id>.json` (spec, IP, snapshot key)
  plus `systemctl is-active firecracker@<id>`. `machine_status` is only a report. On agent restart, rebuild everything
  from local state, then rewrite the facts.
- Reuse the platform rather than libraries: shell out to `ctr`, `systemctl` and `ip` (from `tokio::process`).
  `systemd` stays the VM supervisor; the agent only decides.
  `// shortcut: shells out to ctr/systemctl; use the containerd gRPC API and sd-bus when it matters`.
- Lifecycle per machine:
  - **create** (desired started/stopped, no local state): allocate an IP, prepare the snapshot, mknod the disk,
    write the Firecracker config and args, persist the local state;
  - **start:** `systemctl start firecracker@<id>`; report `started` only once `addr` accepts TCP (with a timeout);
  - **stop:** `systemctl stop` and report `stopped`. Keep the snapshot and IP, so a restart gets the same disk;
  - **destroy:** stop, then remove the snapshot, disk node, tap, config and local state, and report `destroyed`;
  - **orphans:** local state or `firecracker@*` units with no matching desired row → destroy;
  - **failure** (bad image, boot failure, unit failed): report `failed` with the error, retry with backoff (backon is
    already a dependency). No tight loops.
- Only write facts when they change (plus `updated_at`), as one transaction to the worker's own Corrosion agent.
- Log one line per action: id, from → to, ms, error.

## Worker changes
- Generic `firecracker@` template only. `fc-pre` reads `/run/fc/%i.json` (written by the agent) instead of `/etc/fc`.
  `vmsOf`, `fcConfig` and the per-VM drop-ins go away.
- IPs: the agent allocates `10.100.N.10–250` from its own local state; the MAC is derived from the IP. Tap names have a
  15-character limit (IFNAMSIZ), so use a short derived name, for example `tap-<last octet>`, and not the machine id.
- Images: `up.sh` still imports the image tarballs into containerd, and writes a **catalogue**
  (`/var/lib/spike/images/<key>.json`: chain id, Env/Entrypoint/Cmd args), using the jq code it uses today, bind-mounted
  read-only into the workers. `// shortcut: static, pre-imported image catalogue; no registry pulls`.
- Snapshots: the agent prepares `workerN-<id>` from the catalogue chain id over the containerd socket (bind-mounted
  in), and puts the block node under `/spike` as `up.sh` does now. It runs as root in the worker.
  `// shortcut: workers share one containerd namespace and a root socket; isolate per worker beyond the spike`.

## Operator side
- `lib.sh` gets `machine_create <worker> <app> [cmd]` (prints the new id), `machine_start|stop|destroy <id>`,
  `machine_ls` (desired and facts side by side) and `machine_wait <id> <state> [timeout]` (polls facts).
  All writes go through worker1's agent.
- `up.sh` keeps the same interface. Instead of preparing snapshots and inserting `instances`, it creates one
  `machines` row per line of the old `plan`, so the stage 4 `verify.sh` topology still comes up the same way.
  Keep the `worker<n>-<vm>` instance names as their ids here: the id never changes, so it isn't reused.
  The plan becomes seed data only.

## Success criteria (all must pass)
1. After `up.sh`, `verify.sh --failover` passes (the instance list now comes from facts). It needs adapting
   where it starts and stops `firecracker@` units directly: those checks now use desired state.
2. **Create:** `machine_create worker3 echo` → facts `started` with an addr in ≤ N s (measure it). The edge round robin
   for `echo.flyyy.localhost` includes it, and its backhaul accepts it.
3. **Stop / start:** stop → facts `stopped`, the edges and backhaul stop routing to it, and the unit is inactive while
   the snapshot is kept. Start → `started` again, with the **same addr and the same snapshot** (a file written into the
   rootfs before the stop survives; check it from the host via the dm device while it's stopped, or skip and say so).
4. **Destroy:** facts `destroyed`; no unit, tap, snapshot, disk node, `/run/fc` config or local state remains.
   Its id 404s at the backhaul.
5. **Agent crash:** `kill -9` the agent while VMs run → the VMs keep running (same Firecracker PID). After restart
   the agent reconciles without restarting them, and the facts are unchanged.
6. **Orphan cleanup:** a `firecracker@` unit/state for an id not in `machines` (create one by hand on a worker) is torn
   down within one resync.
7. **Failure:** a machine with an unknown image key → facts `failed` with a readable error, retries spaced out by
   backoff (check the log), and other machines unaffected.
8. **Churn:** 20 create → started → destroy cycles across workers (`storm.sh churn`). Every machine reaches each
   state, the edges never route to a non-`started` machine (sample through the edge during the churn: no 502 for an
   app that always has ≥ 1 started machine), and nothing leaks (units, taps, dm devices, snapshots, local state files).
   `storm.sh plain` still passes, with 0 gaps on all proxies.
9. After `down.sh`, the host is clean, as before (also: no leftover snapshots or dm devices).

## Risk points: check these first, in this order, and stop early if one is a blocker
1. **Snapshot + mknod from inside a worker:** `ctr snapshots prepare` over the bind-mounted socket, then `mknod`
   of the dm device into `/spike`. `block-device-mapper` is already allowed, but check the container keeps `CAP_MKNOD`.
   Fallback if it doesn't work: a tiny host-side helper unit that the agent asks over a unix socket, marked as a shortcut.
2. **Runtime `firecracker@<id>` units** with no per-instance drop-in or `wantedBy`. Starting them works from the agent.
   After a worker container restart nothing autostarts them, which is fine: the agent's reconcile starts them.
3. **Subscriptions whose WHERE flips** (`state = 'started'` → `stopped`): the edge and backhaul must see a delete-type
   change. Check this on the real API before relying on it.
4. **Agent ↔ local Corrosion writes:** facts written on worker2 show up on both edges, and worker1's operator writes
   reach worker2/3's agent subscription.

## Constraints
- Keep the one-table flake approach. Mark every shortcut as `# shortcut:` / `// shortcut:`.
- Validation and error handling aren't shortcuts:
  - the agent validates every desired row (known worker, catalogue key, state enum, id format) before acting;
  - a row that fails validation gets `failed` and an error, and never a shell command built from it;
  - pass arguments as argv, never through a shell string.
- `verify.sh` stays fast. Lifecycle checks (criteria 2–7) can go in it or in a new `lifecycle.sh`, whichever stays
  simpler. Churn goes in `storm.sh`.

## Out of scope
- A scheduler, placement or capacity checks, and moving machines between workers.
- Volumes, in-place spec updates, resource limits, health checks beyond the TCP readiness probe.
- Registry pulls, image GC, an API server or auth in front of `machines`.
- Fixing the Corrosion divergence bug (reported as superfly/corrosion#596).

## Deliverables
- Flake changes (generic template unit, agent unit, catalogue bind mount, schema), the agent mode in the proxy crate,
  `up.sh`/`down.sh`, `lib.sh` machine commands, updated `verify.sh`/`storm.sh` (+ `lifecycle.sh` if used).
- A "Stage 5" section in `NOTES.md` (≤ ¾ page):
  - verdict;
  - how snapshot and unit management from inside nspawn worked out;
  - numbers: create → routable, stop → unroutable at the edge, destroy → clean, agent restart → reconciled,
    and agent RSS;
  - the top 3 problems for taking this further.
