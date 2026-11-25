# Every test suite as a flake check: nix build .#checks.<system>.<suite> (aarch64-linux or x86_64-linux; no sudo).
args@{ pkgs, lib, ... }:
let
  t = import ./lib.nix args;
  suite = name: t.mkTest { inherit name; script = builtins.readFile (./. + "/${name}.py"); };
in
{
  verify = suite "verify";
  deploy = suite "deploy";
  lifecycle = suite "lifecycle";
  images = suite "images";
  storm = suite "storm";
  workers = suite "workers";
  guest = suite "guest";
  services = suite "services";
  orgs = t.mkTest { name = "orgs"; seed = false; script = builtins.readFile ./orgs.py; };
  ips = t.mkTest { name = "ips"; seed = false; script = builtins.readFile ./ips.py; };
  # Both dashboards: `flyyy dashboard` on loopback, and the public one (its own mesh node, pubdash).
  dashboard = t.mkTest { name = "dashboard"; seed = false; nodes = t.containers // { inherit (t) pubdash; }; script = builtins.readFile ./dashboard.py; };
  # Forged rows and a fake backhaul from a compromised worker change no routing; machines run on with the
  # control plane stopped.
  trust = suite "trust";
  # Load balancing: by requests in flight, and each service's hard limit (through one edge).
  balance = suite "balance";
  # Per-app metrics: the edges' traffic and the agents' CPU and memory, merged by the control plane.
  metrics = t.mkTest { name = "metrics"; seed = false; script = builtins.readFile ./metrics.py; };
  # Not a correctness check: requests per second and latency through one edge (docs/DESIGN.md has the numbers).
  # Not a check either: boot() alone, to loop (nix build --rebuild) when chasing a rare boot or join failure.
  boots = t.mkTest { name = "boots"; seed = false; script = builtins.readFile ./boots.py; };
  bench = t.mkTest { name = "bench"; seed = false; script = builtins.readFile ./bench.py; extra.edges.environment.systemPackages = [ pkgs.oha ]; };
  # An app's events (the control plane's ring) and its machines' logs (their workers' journals).
  events = t.mkTest { name = "events"; seed = false; script = builtins.readFile ./events.py; };
  # 120 s certificates, so the suite sees them renewed.
  mesh = t.mkTest {
    name = "mesh";
    seed = false;
    script = builtins.readFile ./mesh.py;
    extra.control.systemd.services.control.environment.FLYYY_CERT_LIFETIME = "120";
    # worker4 advertises a WireGuard endpoint of its own (a second address the suite gives it).
    extra.worker4.services.flyyy.worker.endpoint = "10.99.40.2:51820";
  };
  # Not a NixOS test: the dashboard page in headless Chromium, no horizontal overflow from 900 to 1400 px.
  page-overflow = import ./page { inherit pkgs lib; };
  # The join app (nix run .#join) on a machine without flyyy's module.
  join = t.mkTest { name = "join"; nodes = t.containers // { inherit (t) plain; }; script = builtins.readFile ./join.py; };
  # The control host's apps (nix run .#control, .#edge, .#dashboard) on one machine without the modules, a
  # worker by the join app, and an app through that edge. Its own cluster: none of the module-run one.
  hosts = t.mkTest {
    name = "hosts";
    cluster = false;
    nodes = { inherit (t.containers) registry; inherit (t) ctl plain; };
    script = builtins.readFile ./hosts.py;
    # Requests from the worker's machine: it trusts the edge's and the dashboard's certificates.
    extra.plain.security.pki.certificateFiles = [ "${t.testCert}/cert.pem" "${t.dashCert}/cert.pem" ];
  };
  # The single-machine stack of `nix run .#local-control/.#local-worker/.#local-edge`, as an ordinary user.
  local = t.mkTest { name = "local"; cluster = false; nodes = { inherit (t.containers) registry; inherit (t) local; }; script = builtins.readFile ./local.py; };
  # Not a test: the dev stack. `nix run .#stack` (needs sudo) runs its interactive driver.
  stack = suite "stack";
}
