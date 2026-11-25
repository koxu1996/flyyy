/ponytail full

## Goal
Stage 7 of the spike (stages 1–6: `PROMPT.md`, `PROMPT-stage2.md`…`PROMPT-stage6.md`, all summarised in `NOTES.md`).
The set of workers is still fixed at build time. `flake.nix` has `workers = [ 1 2 3 ]`, and from that it generates
the edges' `worker=backhaul,...` map and the control plane's `worker1,worker2,worker3` placement list as command-line
arguments. Adding a worker means rebuilding and restarting the edges and the control plane.

Make **workers announce themselves through Corrosion** instead:
- Each worker's machine agent writes its own row in a new `workers` table: name, backhaul address, and a heartbeat.
- The **edges** take backhaul addresses from that table, and the **control plane** takes the workers it may place
  machines on from it (only live ones, by heartbeat).
- Neither gets a worker list as an argument any more. A new worker joins at runtime, and nothing else is rebuilt or restarted.

## What stays static (say so in the code)
- **Container definitions:** a worker is still an nspawn container defined in Nix (worker N gets 10.99.N.x). "Joining"
  means starting a container that wasn't in the original set, not creating machines out of thin air.
- **Gossip bootstrap seeds:** agents still bootstrap from a fixed list of seed peers (today the 3 workers' gossip
  addresses, like DNS seeds). Seeds are only for finding the cluster; membership and routing data come from Corrosion.
  `# shortcut: static gossip seeds`.
- **Test harness loops** over the 3 base workers in `verify.sh`/`storm.sh` may stay as they are. The new worker's
  checks must not need any static list.

## Data model
```sql
-- One row per worker, written only by that worker's machine agent.
CREATE TABLE workers (
  name TEXT NOT NULL PRIMARY KEY,         -- workerN, as in machines.worker
  backhaul TEXT NOT NULL DEFAULT "",      -- ip:port of its backhaul (CONNECT <machine-id>)
  started_at INTEGER NOT NULL DEFAULT 0,  -- unix ms when this agent process started
  heartbeat_at INTEGER NOT NULL DEFAULT 0 -- unix ms, refreshed every 5 s
);
```
- Same rules as before: one writer per row (the worker's own agent), no `DELETE`. A worker that goes away keeps its
  row with an old heartbeat. `# shortcut: no deregistration or GC of dead workers`.
- **Liveness is judged by the reader:** alive = `heartbeat_at` newer than 30 s by the reader's clock.
  `// shortcut: one host, one clock; across hosts this needs skew handling or a heartbeat counter`.
- Heartbeats are UPDATEs of the same row every 5 s per worker: cheap, single writer, and not the delete + re-insert
  pattern behind #596.

## Worker side (machine agent)
- `proxy agent` gets `--backhaul <ip:port>` (its own worker's backhaul, from the flake's per-worker config). It upserts
  its row on start (`started_at`), then updates `heartbeat_at` every 5 s, independent of the reconcile loop and its pulls.
  A failed heartbeat write is logged and retried on the next tick.
- The heartbeat comes from the machine agent, not from the backhaul process, because "alive" here means "can run
  new machines". So a worker whose agent is down but whose backhaul is up still serves its running machines, and
  just gets no new ones. Write that down in the code.

## Edge side
- No backhaul map argument: `proxy edge <listen> <state-addr> <corrosion-api> <name>`.
- Its subscription now also needs each machine's backhaul. Preferred: one joined subscription,
  `SELECT s.id, s.worker, s.app, w.backhaul FROM machine_status s JOIN workers w ON w.name = s.worker WHERE s.state = 'started'`,
  so `publish` keeps one row type (`[id, worker, app, backhaul]`) and the stage 3 in-order/no-gap loop is unchanged.
  Fallback (risk 1): a second subscription on `workers` feeding the same `publish`.
- Validate every backhaul (parses as `ip:port`; the row's name matches `workerN`) and skip and log bad rows, as the
  edge already does for unknown workers.
- Edges don't filter by heartbeat: per-request fallback (stage 4) already covers a dead backhaul, and the stage 5
  "stale facts" check relies on edges still trying.
- `// shortcut: anything that can write Corrosion can point an edge's backhaul anywhere (no auth on gossip); the
  worker still validates every CONNECT target`.

## Control plane side
- No worker list argument: `proxy control <listen> <corrosion-api> <token-file>`.
- Each reconcile round reads `workers` along with `apps`/`machines`. Placement (creates) uses only valid and alive
  workers. Machines already on a dead worker are still counted, and are **not** rescheduled (out of scope). With no
  live worker, creates wait and the reason is logged.
- New `GET /v1/workers` and `flyyy workers [--json]`: name, backhaul, alive, heartbeat age, uptime, live machines.
- `decide()` stays pure: it gets the live worker list. Extend its test with a dead worker that gets no new machines.

## Flake / scripts
- Remove `backhauls` and the control plane's worker list. `workers = [ 1 2 3 ]` stays as the set `up.sh` starts.
- Add a package `worker4` (the same `worker` function, N = 4) that `up.sh` does **not** start, for the join test.
  `down.sh` destroys it if present, and its leftovers check covers it.
- New `workers.sh` for the criteria below. `verify.sh`, `deploy.sh`, `lifecycle.sh` and `storm.sh` keep passing.

## Success criteria (all must pass)
1. **No static worker lists:** neither the edge's nor the control plane's `ExecStart` contains a worker name or a backhaul
   address. Show it with a `grep`, or by reading the unit files of the running containers.
2. **Announce:** after `up.sh`, `flyyy workers` shows worker1–3 alive with their backhauls. A stopped and restarted
   agent bumps `started_at`, and the heartbeat age stays < 6 s while it runs.
3. **Join at runtime:** start `worker4` (`extra-container create <worker4> --start`, plus its `/var/lib/spike/worker4`).
   Within ~10 s it shows alive. `flyyy scale whoami --count 8` places both new machines on worker4 (it's the emptiest),
   and `whoami.flyyy.localhost` round robins over all 8 through both edges. The edge and control plane PIDs are
   unchanged throughout (no restart), and backhaul validation still holds (another worker's id → 404 at worker4).
4. **Liveness:** stop worker4's agent. Within 40 s `flyyy workers` shows it dead, and a scale-up places nothing on it.
   Its running machines keep serving through its backhaul. Start the agent again → alive, and placement uses it again.
5. **Leave:** scale back to 6 (the worker4 machines go first, since that worker has the most whoami machines, or
   explain why not), then stop the worker4 container. It's dead in `flyyy workers`, its row stays, and edges have no
   routes to it. Re-start it → alive again with the same name.
6. **Bad rows:** a `workers` row with an invalid backhaul, or a bad name, written through the control plane's agent by the
   test, is skipped by both edges (logged) and never placed on by the control plane.
7. **Heartbeats don't churn routing:** over 60 s with no machine changes, each edge's subscription log has 0 change
   events (heartbeat_at isn't a selected column). Report the Corrosion write rate the heartbeats add.
8. `verify.sh --failover`, `deploy.sh`, `lifecycle.sh` and `storm.sh plain churn edge-restart agent-kill` pass, and
   `down.sh` leaves the host clean (worker4 included).

## Risk points: check these first, in this order, and stop early if one is a blocker
1. **A joined subscription in Corrosion** (`machine_status ⋈ workers`): does it work, does a `workers` row arriving
   after its machines' facts make them appear, and do heartbeat UPDATEs (unselected column) stay silent?
   If any of these fail, use the two-subscription fallback and say which part failed.
2. **Starting one more worker container while the rest run:** extra-container with a separate package, its transit
   link, Corrosion gossip joining via the seeds, and `/var/lib/spike/worker4`.
3. **Heartbeat independence:** a long pull (stage 6: one reconcile round blocks on it) must not stop the heartbeat.

## Constraints
- Mark every shortcut as `# shortcut:` / `// shortcut:`. One small unit test for non-trivial logic (`decide()` with
  dead workers; the edge's row validation if it grows).
- No new dependencies expected (tokio intervals and the existing Corrosion client cover it).
- Validation and error handling aren't shortcuts: bad `workers` rows are skipped and logged, never trusted.

## Out of scope
- Rescheduling machines from dead workers, draining or decommissioning a worker, and capacity-aware placement.
- Workers off this host, and dynamic gossip seeds.
- Edge-side health from heartbeats.

## Deliverables
- Flake (`workers` schema, agent `--backhaul`, no worker lists in the edge/control units, `worker4` package), proxy
  changes (agent heartbeat, edge join/validation, control placement from the table, `/v1/workers`, `flyyy workers`),
  `workers.sh`, and `down.sh`.
- A "Stage 7" section in `NOTES.md` (≤ ½ page):
  - verdict;
  - which subscription shape worked;
  - numbers: join → alive → first machine placed, agent stop → seen dead, and the heartbeat write rate;
  - the top 3 problems for taking this further.
