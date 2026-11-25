/ponytail full

## Goal
Stage 6 of the spike (stages 1–5: `PROMPT.md`, `PROMPT-stage2.md`…`PROMPT-stage5.md`, all summarised in `NOTES.md`).
Stage 5 made machines dynamic, but you still drive them one row at a time through `lib.sh`, writing to worker1's
Corrosion agent. Add a **control plane** and a **CLI** that work in terms of *apps*:
```
flyyy deploy blue --image traefik/whoami@sha256:… --port 80 --count 3
flyyy ls
flyyy status blue
flyyy scale blue --count 2
flyyy destroy blue
```
- The control plane is a **stateless** Rust process with **its own Corrosion agent**. All of its state lives in
  Corrosion, so it can be `kill -9`ed at any point and picks up where it left off.
- It exposes a small HTTP/JSON API, and the CLI is only a client of that API: no Corrosion access and no logic of its own.
- It turns an app's desired state into `machines` rows (count and placement). The stage 5 machine agents keep
  doing the rest, unchanged.

## Target topology (changes from stage 5 in bold)
host
├── containerd + devmapper (unchanged); **images are pulled on demand, so the static catalogue goes away (see risk 1)**
├── **`flyyy` CLI → `http://10.99.0.2:4000`**
├── **nspawn `control` (privateNetwork, 10.99.0.1 ↔ 10.99.0.2)**
│   ├── **corrosion agent: joins the cluster (6 agents in total)**
│   └── **control.service: API on 10.99.0.2:4000, plus the app reconciler; the only writer of `apps` and of managed `machines` rows**
├── 2 × nspawn edges (unchanged: `<app>.flyyy.localhost` comes from `machine_status.app` already)
└── 3 × nspawn workers (machine agent: **pulls the image by digest if it's missing**; otherwise unchanged)

## Data model
```sql
-- Desired app state. Written only by the control plane API.
CREATE TABLE apps (
  name TEXT NOT NULL PRIMARY KEY,    -- [a-z][a-z0-9-]{0,19}: a DNS label, and the prefix of its machine ids
  image TEXT NOT NULL DEFAULT "",    -- <repo>@sha256:<64 hex>; digest required, no tags
  port INTEGER NOT NULL DEFAULT 0,
  cmd TEXT NOT NULL DEFAULT "",
  count INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL DEFAULT ""     -- active | destroyed
);
```
- `machines` gains `port INTEGER` (from the app, no longer the catalogue) and `managed INTEGER`. Machines with
  `managed = 0` are **unmanaged**: the `lib.sh`/`lifecycle.sh` ones. The control plane ignores them, so stage 5's
  tests keep working.
- The stage 5 rules still hold: one writer per row, destroy is a state and not a `DELETE`, ids are never reused
  (`<app>-<6 hex>`). A destroyed and re-deployed app reuses its `apps` row, but its machine ids are fresh. Write that down as a known exception, and check it against #596's pattern: an `UPDATE` of an
  existing row is fine; a delete followed by a re-insert of the same key is what to avoid.
- `// shortcut: destroyed apps and machines are kept forever (no GC)`.

## Control plane (`proxy control <listen> <corrosion-api> <worker,...> <token-file>`)
- Same crate, a fourth mode, reusing `follow`/`session`. It follows `apps`, `machines` and `machine_status`. Every
  change, and a ~10 s tick, wakes **one level-triggered app reconciler**. It reads only Corrosion tables, never memory
  from earlier rounds, and writes `machines` through its own agent, so it always sees its own writes.
- Per app, with `live` = its non-destroyed managed machines:
  - **create** while `live` < count. Placement: the worker with the fewest live machines, ties by name.
    `// shortcut: static worker list, no capacity or health; spread only`.
  - **destroy extras** while `live` > count: not-yet-started first, then from the most loaded worker.
  - **app destroyed** → every machine destroyed.
  - Write the per-app decision as a **pure function** (app, its machines, their facts, workers → creates/destroys).
    That's the one small unit test: scale up/down spread, extras chosen, destroy.
- **No in-place updates yet.** A deploy to an active app with the same image/port/cmd just sets the count.
  A different image/port/cmd → 409, "destroy it first". `// shortcut: no rolling or blue/green deploys`.
- **API** (JSON, hyper http1 server as already used):
  - `PUT /v1/apps/<name>` `{image, port, count, cmd?}` = deploy (409 if active with a different spec);
  - `PATCH /v1/apps/<name>` `{count}` = scale;
  - `DELETE /v1/apps/<name>`;
  - `GET /v1/apps`;
  - `GET /v1/apps/<name>`: spec, desired vs started, `converged`, and each machine (id, worker,
    desired state, fact state, addr, error).
  - Writes return 202 once the `apps` row is written: they don't wait for VMs. Unknown app → 404, bad input → 400 with
    a reason, nothing written.
- **Validation and auth aren't shortcuts.** Validate every field strictly on the server: the app name, digest-pinned
  image refs (`^[a-z0-9._/-]+@sha256:[0-9a-f]{64}$`), port 1–65535, count 0–20, cmd as in stage 5 (`{id}` → the machine id).
  Every request needs `Authorization: Bearer <token>`. `up.sh` generates the token (readable by the invoking user),
  and the control plane compares it in constant time. `// shortcut: one static token over plaintext HTTP on the transit net`.
- Log one line per request (method, path, status, ms) and one per reconciler action (app, create/destroy, id, why).

## CLI (`flyyy`)
- A second binary in the crate (`src/bin/flyyy.rs`), using the hyper client already in the tree.
  Arguments are parsed by hand for these 5 commands; add a parser crate only if that gets unwieldy, and then by the repo's
  Rust dependency rule. `FLYYY_API` (default `http://10.99.0.2:4000`) and `FLYYY_TOKEN_FILE` (default the file `up.sh`
  writes) configure it.
- `deploy`/`scale`/`destroy` wait by default until `converged`, showing progress as machines change state; `--detach`
  returns at once. Exit codes: 0 converged, 1 API error, 2 timed out (`--timeout`, default 120 s).
- `ls`: name, image (short digest), state, started/desired. `status <app>`: spec plus the machine table. `--json` on both.

## Machine agent changes
- `image` is now a digest-pinned ref. If the image isn't in containerd yet, the agent pulls it (`ctr images pull
  --platform linux/arm64 --snapshotter devmapper <ref>` over its socket), then reads the image config and computes the
  chain id the way up.sh does now. Validate the ref before passing it as argv; pull failures are machine failures
  (`failed`, error, backoff).
- The port comes from the row, not the catalogue. Everything else is unchanged.

## up.sh / scripts
- `up.sh`: workers, edges, `control`, then **seed via the CLI**: `flyyy deploy whoami --image traefik/whoami@sha256:c4717a8d1f0134a7444e24f881160e033991f23027c6c5a9a3f8fd22e70d1d44 --port 80 --count 6`
  and `flyyy deploy echo --image hashicorp/http-echo@sha256:fcb75f691c8b0414d670ae570240cbf95502cc18a9ba57e982ecac589760a186 --port 5678 --count 4 --cmd '-text={id}'`
  (the flake's current digests). `plan` goes away; scripts get their expected instances from `flyyy status --json`.
- `verify.sh`, `lifecycle.sh` and `storm.sh` keep passing after adapting to that. In `storm.sh`, `churn` stays on unmanaged machines.
- A new `deploy.sh` covers the criteria below.

## Success criteria (all must pass)
1. `up.sh` comes up with both apps via the CLI, and `verify.sh --failover` passes.
2. **Deploy:** `flyyy deploy blue … --count 3` exits 0 with 3 machines started, one per worker.
   `blue.flyyy.localhost` round robins over the 3. Measure deploy → converged, with a cold image (pull) and a warm one.
3. **ls / status** show the right counts, states and per-machine facts, both as text and as `--json`.
4. **Scale:** 3 → 5 → 2. Spread stays even (no worker has more than ⌈count/3⌉), and extras are destroyed.
   Scale to 0 → vhost 404, and scaling up again works.
   Scaling down while sampling `blue.flyyy.localhost` gives no non-200; report the edge fallbacks (the drain
   problem, `TODO.md`; don't fix it here). Re-deploying the same spec is a no-op, and a different image → 409.
5. **Destroy:** every machine is destroyed, the vhost 404s, and `ls` shows it destroyed.
   Re-deploying the same name works and gets fresh ids.
6. **Stateless:** `kill -9` the control plane in the middle of a scale-up (for example 2 → 10). The data plane keeps
   serving throughout, and after the restart it finishes with no duplicate or missing machines. While it's down, the CLI
   fails with a clear error (exit 1).
7. **Rejected input:** bad app name, a tag-only image, port 0, count 1000, unknown fields, no or wrong token → 400/401.
   Nothing is written (check `apps`/`machines`).
8. **Bad image:** deploying an app with an image digest that doesn't exist → its machines `failed` with the pull error,
   `status` shows why, other apps are unaffected, and destroy + deploy with a good image recovers.
9. `lifecycle.sh`, `storm.sh plain churn` and `storm.sh agent-kill` still pass, and `down.sh` leaves the host clean
   (also: no control container, and no token file left over).

## Risk points: check these first, in this order, and stop early if one is a blocker
1. **Pulling by digest through host containerd, from inside a worker:** the arm64 manifest out of the index, unpacked
   into devmapper, plus the config and chain id. If the host can't reach the registry (or rate-limits), fall back to
   the stage 5 catalogue keyed by digest ref (pre-imported by `up.sh`) and say so. Nothing else changes.
2. **The `control` container joins gossip** (a 6th agent on its own transit link). Its writes reach the machine
   agents, and the host reaches its API.
3. **Reconciler idempotence:** two quick scales, or a scale straight after a deploy, must not double-create. Check
   that its own writes are visible in its next round (they go through its local agent).
4. Adding columns to `machines` and a new `apps` table is a fresh-DB schema change: confirm the stage 5 subscriptions
   still work with the added columns.

## Constraints
- Keep the one-table flake approach: `control` is one more small entry next to `workers`/`edges`.
  Mark every shortcut as `# shortcut:` / `// shortcut:`.
- No new crates for the server or the client (hyper is there). New dependencies only if unavoidable, by the repo's Rust rule.
- `verify.sh` stays fast. App-level checks (criteria 2–8) go in `deploy.sh` (sudo only where it inspects containers
  or kills processes, with the tools on PATH as in stage 5).

## Out of scope
- An HA control plane (several instances need leader election or per-app leases), multi-region, and capacity-aware
  scheduling.
- Updating a running app: rolling or blue/green deploys, rollback, canaries. Also health checks, env/secrets, volumes,
  image GC.
- TLS, users, and per-app permissions.
- Drain before stop (`TODO.md`): measure it, don't fix it.

## Deliverables
- Flake (`control` container, schema, token in `up.sh`), the control mode and the `flyyy` binary, machine agent pull,
  `up.sh`/`down.sh`, `deploy.sh`, and adapted `verify.sh`/`lifecycle.sh`/`storm.sh`.
- A "Stage 6" section in `NOTES.md` (≤ ¾ page):
  - verdict;
  - how pulls worked out;
  - numbers: deploy → converged (cold and warm), scale up/down and edge fallbacks, recovery after
    `kill -9` of the control plane, and control plane RSS;
  - the top 3 problems for taking this further.
