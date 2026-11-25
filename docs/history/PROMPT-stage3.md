/ponytail full

## Goal
Stage 3 of the spike (stage 1: `PROMPT.md`, stage 2: `PROMPT-stage2.md`, both summarised in `NOTES.md`).
Replace the proxy's static routes file with **Corrosion** (https://github.com/superfly/corrosion),
the distributed SQLite database with gossip and CRDTs. Each worker runs a Corrosion agent, and its proxy
builds its route table from a Corrosion **subscription**. The point of this stage is to prove that the
proxy sees **every change, in order, with no gap**, including across disconnects, restarts and lag,
and that it never serves a half-built route table.

## What "in order, no gap" means here (the contract to implement and test)
Corrosion's server only gives you the building blocks. Its Rust SDK (`corro-client`) enforces part
of the contract, and the proxy adds a thin loop around it for the rest.
Facts from the docs and source (re-check them against the pinned version, they may have changed):
- `POST /v1/subscriptions` (body: `[sql, params]`) returns header `corro-query-id` and an NDJSON stream:
  `columns`, then `row` × N (`[rowid, [values]]`), then `eoq` (with `change_id`), then
  `change` (`[type, rowid, values, change_id]`, type `insert|update|delete`; values = all columns,
  even on delete). Any `error` event is fatal for that stream.
- `change_id` is per subscription and increases by exactly 1. The reference client
  (`crates/corro-client/src/sub.rs`) treats anything other than `last + 1` as `MissedChange`:
  the local view is inconsistent.
- `GET /v1/subscriptions/:id?from=<change_id>` resumes. The server keeps only about the last 500 changes
  per subscription (pruned every 300 s). An older `from` gets an error ("already deleted older
  changes"), not a resend.
- `GET /v1/subscriptions/:id` with no `from` resends the full current rows + `eoq` + live changes.
- Subscriptions are restored after an agent restart only if it shut down cleanly. After a crash the id
  is unknown, so you have to `POST` again.
- Clients that fall too far behind get an error and their stream is closed.
- Rows are keyed by the subscription's `rowid`, not the table primary key. Map routes by that rowid.
- `/v1/updates/:table` has no snapshot and no change ids. **Do not use it**; it can't give "no gap".

What the SDK already does (`SubscriptionStream`, `subscribe_typed` / `subscription_typed`). Don't re-implement it:
- NDJSON parsing into typed events (`Columns`, `Row`, `EndOfQuery { change_id }`, `Change`, `Error`)
  with rows deserialised into a `#[derive(Deserialize)]` struct.
- The `change_id == last + 1` check. Anything else ends the stream with `MissedChange`.
- Reconnect after an **I/O error** with `GET /:id?from=last` (10 attempts, 1 s apart), then
  `MaxRetryAttempts`. If the first snapshot never reached `eoq`, it gives `UnfinishedQuery`.

What the proxy adds on top, as one small outer loop:
1. **Snapshot**: `subscribe_typed(stmt, false, None)`. Collect `Row`s into a *staging* table until
   `EndOfQuery`, then atomically swap it in, so no partial table is ever visible.
2. **Live**: apply each `Change` to the live table (the SDK already guarantees the order).
3. **Resync on anything terminal**: `MissedChange`, `MaxRetryAttempts`, `UnfinishedQuery`,
   `UnexpectedStatusCode` (pruned `from`, unknown id after a dirty restart), an `Error` event, **or a
   clean end of the stream**. The SDK does *not* reconnect on a clean end; its doc comment claims
   it does, but the code doesn't. Then go back to step 1 with a fresh `subscribe_typed`. Retry with **`backon`**
   (`ExponentialBuilder` with jitter and a max delay, retrying until it succeeds). Don't write a
   hand-rolled sleep loop. Reset the backoff once a snapshot reaches `EndOfQuery`. The old table
   keeps serving until the new snapshot swaps in.
- Before the very first snapshot completes, the proxy answers **503**, not 404. "No data yet" must
  never look like "no route".
- On proxy restart, start from step 1. Persisting `last` without persisting the table can't
  give "no gap"; it's not needed.
- Applying one change event is atomic. A multi-row Corrosion transaction arrives as several events,
  so their intermediate states can be visible. Note that in NOTES.md and don't solve it.
- Ordering is **per proxy, relative to its local agent**. Corrosion does not promise the same order
  on every agent, only convergence to the same final state. Observe and report whether agents saw the
  single writer's changes in the same order, but don't make it a pass criterion.

## Target topology (changes from stage 2 in bold)
host
├── containerd + devmapper (unchanged)
└── 3 × nspawn workers
    ├── **corrosion agent: gossip `10.99.N.2:8787` (plaintext QUIC), API `0.0.0.0:8080`,
    │   bootstrap = the other two workers (worker↔worker traffic is routed through the host)**
    ├── proxy.service: **routes from its local agent's subscription** (same RR + connect fallback
    │   as stage 2), **plus a debug endpoint `0.0.0.0:9080/state`**
    └── Firecracker VMs (unchanged: whoami a/b on all, echo c/d on worker1+2)

## Data
- Schema (a Nix-generated `.sql` in `db.schema_paths`), CRDT rules: non-null PK, non-null columns
  need defaults, no unique indexes:
  `CREATE TABLE instances (id TEXT NOT NULL PRIMARY KEY, worker TEXT NOT NULL DEFAULT '',
   app TEXT NOT NULL DEFAULT '', addr TEXT NOT NULL DEFAULT '');` (id = `worker1-a`, addr = `ip:port`).
- Proxy N subscribes to `SELECT id, app, addr FROM instances WHERE worker = ?` with param `workerN`.
  Vhost = `<app>.flyyy.localhost`, as in stage 2.
- `up.sh` writes all instances from the existing `plan` once, through **worker1's** API
  (`POST /v1/transactions`). worker2/3 proxies only get them through gossip. The static routes file
  is gone.
- `/state` returns JSON: `{subscription_id, last_change_id, snapshots, resyncs, routes}` (routes
  sorted). `verify.sh` compares it against `POST /v1/queries` on the same agent.

## Building Corrosion
- **Trap:** `nixpkgs#corrosion` is corrosion-rs (the CMake/Rust tool), not superfly's.
- Prefer the official `v1.0.0` `corrosion-aarch64-unknown-linux-gnu.tar.gz` release (`fetchurl` +
  `autoPatchelfHook`). Fall back to the repo's own `flake.nix`. Building from source is the last
  resort (it pins Rust 1.89).
- Pin the docs/source you rely on, the agent and `corro-client`, to the same release. Re-check the facts above against it, and
  report any differences in NOTES.md.

## Proxy changes
- Same crate. New dependencies: **`corro-client` `0.2.0-alpha.0` from crates.io** (published with
  the v1.0.0 release, so no git dependency and no `outputHashes`), `corro-api-types` (same version)
  for `Statement`/`ChangeType`, `serde` (derive for the row struct), `serde_json` (`/state`) and `backon` 1.x
  (resync backoff, `tokio-sleep` feature, default features off).
  The proxy's own HTTP serving stays on hyper.
- This pulls in reqwest and rusqlite with bundled SQLite. Expect a longer build, and record the
  binary size and RSS against stage 2 in NOTES.md.
- One log line per subscription event applied (`change_id type rowid values`), and one per
  snapshot/resync/reconnect with its reason, so the journal is an audit trail.
- Routes live behind a lock or `Arc` swap; the request path never waits on the subscription.

## Success criteria (all must pass)
1. Stage 2 checks still pass unchanged (direct VMs, RR per app, 404 where not placed, failover).
2. Each proxy's `/state.routes` equals its agent's query result, and `last_change_id` is set.
3. **Change storm**: from the host, 1000 sequential single-statement transactions against worker1
   (insert/update/delete rows of a `probe` app on all 3 workers, ending in a known final state).
   Afterwards each proxy's `/state` matches the final DB, its journal shows contiguous change ids
   with no resync, and `probe.flyyy.localhost` routes as the final state says.
4. **Fault injection during a storm.** Each case must end with `/state` == DB and the
   reconnect/resync path logged:
   a. `systemctl restart proxy` on one worker → new snapshot.
   b. Clean restart of one worker's corrosion agent → reconnect with `from` (or resync, logged with reason).
   c. `kill -9` of one worker's corrosion agent → the id is gone → resync via new `POST`.
   d. `SIGSTOP` a proxy, write > 500 changes, wait past one prune (≤ 300 s), `SIGCONT` →
      resume refused or lagging-client error → resync. If prune can't be forced, wait for it and
      say so.
5. The proxy answers 503 before its first snapshot (e.g. start it with the agent stopped).
6. `up.sh` / `down.sh` unchanged for the user, and the host is clean after down.

## Risk points: check these first, in this order, and stop early if one is a blocker
1. The Corrosion binary runs on aarch64 NixOS inside nspawn (glibc, cr-sqlite extension loading).
2. 3 agents form a cluster through the host (`10.99.N.2` ↔ `10.99.M.2`); a write on worker1 shows up
   in a query on worker3.
3. `corro-client` builds in the flake (reqwest's TLS backend: avoid needing OpenSSL if a
   default-features switch allows it) and talks to the v1.0.0 agent.
4. Subscription behaviour on the pinned version: the `eoq.change_id` of a fresh subscription,
   contiguous ids, `from` resume, the error on pruned `from`, and restore after clean vs. dirty restart.

## Constraints
- Keep the one-table flake approach. Corrosion configs, schema and bootstrap lists are generated,
  with no per-worker copy-paste.
- Fault tests go in a separate `storm.sh` (uses sudo) so `verify.sh` stays fast.
- Mark every shortcut as `# shortcut:` / `// shortcut:`, as before (plaintext gossip, the 500/300 s
  assumptions, the single writer, and so on).

## Out of scope
TLS / gossip encryption, multiple writers or conflict handling, VM-lifecycle-driven registration
(VMs don't write their own rows yet), cross-worker routing, health checks, persistence across
`down.sh`, Corrosion on the host, the PG wire protocol, templates.

## Deliverables
- Flake changes (corrosion package + per-worker agent unit), proxy changes, `storm.sh`, updated `verify.sh`.
- A "Stage 3" section in `NOTES.md` (≤ ¾ page):
  - verdict;
  - the subscription facts as observed on the pinned version (vs. the list above);
  - whether agents saw the same order;
  - change propagation latency (worker1 write → worker3 proxy applied, rough);
  - agent RSS;
  - the top 3 problems for taking this further.
