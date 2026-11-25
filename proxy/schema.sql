-- The Corrosion schema, read by every node's Corrosion agent (nix/modules/mesh.nix) and by the Rust code (corro.rs).
-- Corrosion: one agent per worker, per edge and in the control plane, gossiping on the mesh. Everyone
-- bootstraps from the control plane's agent; membership itself is in Corrosion.
-- Service discovery only: published facts, never desired state. The control plane keeps apps,
-- machines (env included), orgs, users and IP leases in its own store and sends each worker its
-- machines over the agent API; nothing here is a secret.
-- Rules: non-null PK, non-null columns need defaults, no unique indexes.
-- Every table has one writer, destroy is a state rather than a DELETE, and ids are never reused:
-- this keeps away from the delete + re-insert pattern behind the stage 3 divergence.
-- shortcut: destroyed rows are kept forever; GC them once the Corrosion bug is understood.
-- shortcut: no migrations: a cluster from before this schema needs its Corrosion state wiped.

-- Ingress addresses (proxy/src/ingress.rs), written only by the control plane: the prefixes from its
-- flags (shared IPs as /32s, pools), and each app's dedicated address from a pool.
CREATE TABLE ip_pools (
  prefix TEXT NOT NULL PRIMARY KEY,
  -- shared (HTTP, by Host) or pool (dedicated addresses).
  kind TEXT NOT NULL DEFAULT "pool",
  -- active, or retired once the flags no longer name it.
  state TEXT NOT NULL DEFAULT "active"
);
CREATE TABLE ips (
  ip TEXT NOT NULL PRIMARY KEY,
  app TEXT NOT NULL DEFAULT "",
  -- allocated, or released (by destroy; not reused for 24 h).
  state TEXT NOT NULL DEFAULT "allocated",
  allocated_at INTEGER NOT NULL DEFAULT 0,
  released_at INTEGER NOT NULL DEFAULT 0
);
-- Placement, written only by the control plane: each instance (<machine>.<service>) of a machine it
-- placed, with its app and worker and the service's protocol, VM port, edge port (tcp: where it's
-- published; 0 for http) and concurrency limits (soft, hard; 0: none). The edges route an instance
-- only as placed here, and a backhaul serves only its own worker's.
CREATE TABLE placement (
  id TEXT NOT NULL PRIMARY KEY,
  machine TEXT NOT NULL DEFAULT "",
  app TEXT NOT NULL DEFAULT "",
  worker TEXT NOT NULL DEFAULT "",
  protocol TEXT NOT NULL DEFAULT "",
  port INTEGER NOT NULL DEFAULT 0,
  edge_port INTEGER NOT NULL DEFAULT 0,
  soft_limit INTEGER NOT NULL DEFAULT 0,
  hard_limit INTEGER NOT NULL DEFAULT 0
);
-- Each worker's backhaul as the control plane bound it, written only by the control plane: the
-- worker's own mesh address, at the port it announced ("" for a worker that isn't one of the mesh).
CREATE TABLE backhauls (
  worker TEXT NOT NULL PRIMARY KEY,
  backhaul TEXT NOT NULL DEFAULT ""
);
-- One row per edge, written only by that edge: where its load counters are served on the mesh (the
-- control plane asks there, mTLS, and only on an edge node's own mesh address), and where it serves apps
-- (its HTTPS address, else its plain HTTP one), for the operator.
CREATE TABLE edges (
  name TEXT NOT NULL PRIMARY KEY,
  load_addr TEXT NOT NULL DEFAULT "",
  started_at INTEGER NOT NULL DEFAULT 0,
  listen TEXT NOT NULL DEFAULT ""
);
-- One row per worker, written only by that worker's machine agent: where its backhaul and agent API
-- listen (the control plane binds them to its mesh identity before using them), a heartbeat, and its
-- capacity and allocation. Readers judge liveness by heartbeat age. No deletes: a gone worker keeps
-- its row.
-- shortcut: no deregistration or GC of dead workers.
CREATE TABLE workers (
  name TEXT NOT NULL PRIMARY KEY,
  backhaul TEXT NOT NULL DEFAULT "",
  agent_api TEXT NOT NULL DEFAULT "",
  started_at INTEGER NOT NULL DEFAULT 0,
  heartbeat_at INTEGER NOT NULL DEFAULT 0,
  -- Its capacity: architecture (Rust's names: aarch64, x86_64), CPUs, memory (MiB).
  arch TEXT NOT NULL DEFAULT "",
  cpus INTEGER NOT NULL DEFAULT 0,
  memory INTEGER NOT NULL DEFAULT 0,
  -- What it holds that isn't destroyed: how many machines, their CPUs and memory summed.
  machines INTEGER NOT NULL DEFAULT 0,
  allocated_cpus INTEGER NOT NULL DEFAULT 0,
  allocated_memory INTEGER NOT NULL DEFAULT 0
);
-- Each worker's finished images (EROFS files), written only by that worker's agent and only when a
-- size changes; an image file that's gone is 0 bytes (no deletes).
CREATE TABLE images (
  worker TEXT NOT NULL,
  digest TEXT NOT NULL,
  bytes INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (worker, digest)
);
-- Health checks, one row per machine and service (id <machine>.<service>), written only by that
-- machine's worker and only when it changes. state: passing, failing, or down (the machine isn't
-- started). The edges route to passing ones; the backhaul serves any that isn't down. Keyed by
-- (id, worker), as machine_status: a row a worker writes about another worker's machine is a row of
-- its own, which placement then doesn't match; it can't overwrite the real one.
-- shortcut: Corrosion doesn't authenticate writers, so a worker could still write rows in another
-- worker's name; only signed rows would stop that.
CREATE TABLE service_status (
  id TEXT NOT NULL,
  machine TEXT NOT NULL DEFAULT "",
  worker TEXT NOT NULL DEFAULT "",
  app TEXT NOT NULL DEFAULT "",
  service TEXT NOT NULL DEFAULT "",
  protocol TEXT NOT NULL DEFAULT "",
  addr TEXT NOT NULL DEFAULT "",
  edge_port INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL DEFAULT "",
  error TEXT NOT NULL DEFAULT "",
  updated_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id, worker)
);
-- Facts, each row written only by the machine agent on that machine's worker.
CREATE TABLE machine_status (
  id TEXT NOT NULL,
  worker TEXT NOT NULL DEFAULT "",
  app TEXT NOT NULL DEFAULT "",
  state TEXT NOT NULL DEFAULT "",
  addr TEXT NOT NULL DEFAULT "",
  error TEXT NOT NULL DEFAULT "",
  updated_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id, worker)
);
