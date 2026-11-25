# The cluster every test runs: the real worker/edge/control modules from nix/modules (the flake's
# nixosModules, here with the test cluster's addresses as their options) as nspawn
# containers (no QEMU, no sudo), on one VLAN with up.sh's 10.99.x addresses as the underlay (boot() then
# joins them into the WireGuard mesh, which carries everything between them), plus a local registry
# serving tests/images.nix read-only from the store. `mkTest { name, script }` adds a suite's Python to a shared prelude
# (helpers, boot, seed apps). worker4 is defined but only started by suites that start it.
{ pkgs, lib, roles, proxy, flyyy, roleApps, joinApp, hostApps }:
let
  token = lib.fixedWidthString 64 "0" "flyyy-test-token";
  # The edges' certificate: a self-signed *.flyyy.localhost (its own CA), trusted in the edges container.
  # Test-only: its key is in the store. The edges read a copy in /var/lib/edge-tls, so tests can swap it.
  testCert = pkgs.runCommand "flyyy-test-cert" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 -subj "/CN=*.flyyy.localhost" \
      -addext "subjectAltName=DNS:*.flyyy.localhost" -keyout $out/key.pem -out $out/cert.pem 2>/dev/null
  '';
  # The public dashboard's certificate (tests/dashboard.py), dash.flyyy.localhost, self-signed. Test-only.
  dashCert = pkgs.runCommand "flyyy-dash-cert" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 -subj "/CN=dash.flyyy.localhost" \
      -addext "subjectAltName=DNS:dash.flyyy.localhost" -keyout $out/key.pem -out $out/cert.pem 2>/dev/null
  '';
  # The control plane's underlay address: where every node's WireGuard reaches it.
  controlAddress = "10.99.0.2";
  images = import ./images.nix { inherit pkgs lib; };
  # One VLAN, a /16: every container reaches the others directly at its up.sh address.
  net = addr: {
    virtualisation.vlans = [ 1 ];
    networking.interfaces.eth1.ipv4.addresses = [{ address = addr; prefixLength = 16; }];
    networking.firewall.enable = false;
    environment.systemPackages = [ pkgs.curl pkgs.jq pkgs.e2fsprogs pkgs.erofs-utils pkgs.iproute2 pkgs.openssl ];
    # resolvconf can't run in the build sandbox (setfacl), and Corrosion builds a DNS resolver even for
    # IP:port bootstrap entries: without any resolv.conf it skips the bootstrap and never forms a cluster.
    networking.resolvconf.enable = lib.mkForce false;
    environment.etc."resolv.conf".text = "nameserver 127.0.0.1\n";
    system.stateVersion = "26.05";
  };
  # Each container in a cgroup of its own. The driver starts every container (systemd-nspawn --keep-unit,
  # no host systemd in the build sandbox) in its own cgroup, so they all shared one tree: a unit named the
  # same in two containers (network-addresses-eth1, agent, mesh-cert, ...) shared one cgroup, and one
  # container's systemd removing it while another spawned into it failed that start ("Failed to spawn
  # executor: No such file or directory"). Once that left worker2 without its address, and its mesh join
  # waited forever. The start script below is the nspawn module's own (nixos/modules/virtualisation/
  # nspawn-container), moved into <its cgroup>/<container> first.
  # shortcut: rebuilds that module's start command; update it with the pinned nixpkgs.
  ownCgroup = { config, modulesPath, ... }: let
    cfg = config.virtualisation;
    run-nspawn = pkgs.callPackage "${modulesPath}/virtualisation/nspawn-container/run-nspawn" { };
    options = lib.cli.toCommandLineShellGNU { } {
      container-name = config.system.name;
      root-dir = cfg.rootDir;
      interfaces-json = builtins.toJSON (lib.attrValues cfg.allInterfaces);
      init = "${config.system.build.toplevel}/init";
      cmdline-json = builtins.toJSON cfg.cmdline;
    };
  in {
    system.build.nspawn = lib.mkForce (pkgs.writers.writeDashBin "run-${config.system.name}-nspawn" ''
      cg="/sys/fs/cgroup$(${pkgs.gnused}/bin/sed -n 's/^0:://p' /proc/self/cgroup)/${config.system.name}"
      ${pkgs.coreutils}/bin/mkdir -p "$cg" && echo $$ > "$cg/cgroup.procs" || { echo "own cgroup: can't move into $cg" >&2; exit 1; }
      exec ${lib.getExe run-nspawn} ${options} ${lib.escapeShellArgs cfg.systemd-nspawn.options} "$@"
    '');
  };
  # A lingering user's systemd instance (user@<uid>) without PAM: its pam_unix account check needs the
  # setuid unix_chkpwd wrapper, which the build sandbox can't make. PAM is also what would set its
  # XDG_RUNTIME_DIR.
  userManager = {
    systemd.services."user@".serviceConfig = { PAMName = ""; Environment = "XDG_RUNTIME_DIR=/run/user/%i"; };
  };
  worker = n: {
    imports = [ roles.worker (net "10.99.${toString n}.2") userManager ];
    # A machine's logs (the agent reads its VMs' journal as flyyy): on a host journald gives flyyy its own
    # journal file through an ACL, but the build sandbox refuses ACLs (journald's setxattr). Here everything
    # stays in the system journal, which flyyy reads through the group. Test-only: the module doesn't.
    services.journald.settings.Journal.SplitMode = "none";
    users.users.flyyy.extraGroups = [ "systemd-journal" ];
    services.flyyy = { inherit controlAddress; worker = { enable = true; name = "worker${toString n}"; }; };
    virtualisation.systemd-nspawn.options = [ "--bind=/dev/kvm" ];
  };
  asDir = name: tar: pkgs.runCommand "image-${lib.replaceStrings [ "/" ] [ "-" ] name}" { nativeBuildInputs = [ pkgs.skopeo ]; } ''
    skopeo --insecure-policy --tmpdir "$TMPDIR" copy -q --dest-compress docker-archive:${tar} dir:$out
  '';
  imagesRoot = registryRoot (lib.mapAttrs asDir images);
  # A read-only registry on `port` serving `root`.
  registryConfig = root: port: pkgs.writeText "registry-${toString port}.json" (builtins.toJSON {
    version = "0.1";
    http.addr = "0.0.0.0:${toString port}";
    log.level = "warn";
    storage = {
      filesystem.rootdirectory = "${root}";
      maintenance = { uploadpurging.enabled = false; readonly.enabled = true; };
    };
  });
  containers = {
    worker1 = worker 1;
    worker2 = worker 2;
    worker3 = worker 3;
    worker4 = worker 4;
    control = {
      imports = [ roles.control (net controlAddress) ];
      # Ingress: HTTP on the shared IP, each app's own address from the pool; the edges make both local.
      services.flyyy.control = { enable = true; sharedIps = [ "10.200.0.1" ]; ipPools = [ "10.200.1.0/24" ]; operatorTokenFile = "/etc/control.token"; };
      # Its join tokens carry this (control.endpoint's default).
      services.flyyy.controlAddress = controlAddress;
      environment.etc."control.token".text = token;
    };
    # Both edges in one network namespace, as in up.sh's host netns: they share 127.0.0.1:80 (and :443).
    edges = {
      imports = [ roles.edge (net "10.99.1.1") ];
      services.flyyy.controlAddress = controlAddress;
      services.flyyy.edge = {
        enable = true;
        nodeName = "edges";
        instances = { edge1.index = 1; edge2.index = 2; };
        https = { listen = "127.0.0.1:443"; cert = "/var/lib/edge-tls/cert.pem"; key = "/var/lib/edge-tls/key.pem"; };
      };
      security.pki.certificateFiles = [ "${testCert}/cert.pem" ];
      systemd.tmpfiles.rules = [
        "d /var/lib/edge-tls 0750 root edge -"
        "C /var/lib/edge-tls/cert.pem 0644 root root - ${testCert}/cert.pem"
        "C /var/lib/edge-tls/key.pem 0640 root edge - ${testCert}/key.pem"
        # C copies the store file's 0444; the key is the edge user's only.
        "z /var/lib/edge-tls/cert.pem 0644 root root -"
        "z /var/lib/edge-tls/key.pem 0640 root edge -"
      ];
      environment.systemPackages = [ pkgs.openssl ];
    };
    # The test images, served read-only straight from the store (registryRoot): each converted to skopeo's
    # dir: layout at build time (layers gzipped, as a push would), so nothing is pushed at boot. The digests
    # are in /etc/flyyy-images.json (repo -> sha256:...).
    registry = {
      imports = [ (net "10.99.0.3") ];
      environment.etc."flyyy-images.json".source = "${imagesRoot}/refs.json";
      systemd.services.registry = {
        wantedBy = [ "multi-user.target" ];
        environment.OTEL_TRACES_EXPORTER = "none";  # it exports to localhost:4318 by default
        serviceConfig = { ExecStart = "${lib.getExe pkgs.distribution} serve ${registryConfig imagesRoot 5000}"; DynamicUser = true; };
      };
    };
  };
  # The public dashboard (`proxy dashboard`), a mesh node of its own: only the dashboard suite has it.
  pubdash = {
    imports = [ roles.dashboard (net "10.99.6.2") ];
    services.flyyy.controlAddress = controlAddress;
    services.flyyy.dashboard = {
      enable = true;
      host = "dash.flyyy.localhost";
      tlsCert = "/var/lib/dash-tls/cert.pem";
      tlsKey = "/var/lib/dash-tls/key.pem";
      appsUrl = "https://{app}.flyyy.localhost/";
    };
    systemd.tmpfiles.rules = [
      "d /var/lib/dash-tls 0750 root flyyy-dashboard -"
      "C /var/lib/dash-tls/cert.pem 0644 root root - ${dashCert}/cert.pem"
      "C /var/lib/dash-tls/key.pem 0640 root flyyy-dashboard - ${dashCert}/key.pem"
      "z /var/lib/dash-tls/cert.pem 0644 root root -"
      "z /var/lib/dash-tls/key.pem 0640 root flyyy-dashboard -"
    ];
  };
  # A NixOS machine without flyyy's module, its firewall on, for the join app (tests/join.py): only that
  # suite has it.
  plain = {
    imports = [ (net "10.99.7.2") userManager ];
    virtualisation.systemd-nspawn.options = [ "--bind=/dev/kvm" ];
    networking.firewall.enable = lib.mkForce true;
    # Who the worker runs as: /dev/kvm through the kvm group.
    users.users.bob = { isNormalUser = true; uid = 1000; extraGroups = [ "kvm" ]; };
    environment.systemPackages = [ joinApp pkgs.iptables pkgs.procps ];
  };
  # The control host without flyyy's modules (tests/hosts.py), its firewall on: carol runs #control, #edge and
  # #dashboard there. The edge's and the dashboard's certificates are carol's (0600). Only that suite has it.
  cliFlake = pkgs.writeTextDir "flake.nix" ''
    { outputs = _: { apps.${pkgs.stdenv.hostPlatform.system}.flyyy = { type = "app"; program = "${lib.getExe flyyy}"; }; }; }
  '';
  ctl = {
    imports = [ (net "10.99.8.2") ];
    networking.firewall.enable = lib.mkForce true;
    users.users.carol = { isNormalUser = true; uid = 1000; };
    environment.systemPackages = hostApps ++ [ proxy pkgs.iptables pkgs.procps ];
    # `nix run <flake>#flyyy`, offline: a flake with no inputs whose apps.flyyy is the package (the real
    # flake's is the same; evaluating it here would need its inputs). The container's store is the host's,
    # read-only, with no database: hosts.py copies this closure (its registration file) into a chroot store.
    nix.settings.experimental-features = [ "nix-command" "flakes" ];
    environment.etc."flyyy-flake".source = cliFlake;
    environment.etc."flyyy-flake-reg".source = "${pkgs.closureInfo { rootPaths = [ cliFlake ]; }}/registration";
    systemd.tmpfiles.rules = lib.concatMap (d: [
      "d /var/lib/${d.dir} 0700 carol users -"
      "C /var/lib/${d.dir}/cert.pem 0644 carol users - ${d.cert}/cert.pem"
      "C /var/lib/${d.dir}/key.pem 0600 carol users - ${d.cert}/key.pem"
      "z /var/lib/${d.dir}/cert.pem 0644 carol users -"
      "z /var/lib/${d.dir}/key.pem 0600 carol users -"
    ]) [ { dir = "edge-tls"; cert = testCert; } { dir = "dash-tls"; cert = dashCert; } ];
  };
  # One machine, one ordinary user (lingering, not root): `nix run .#control`, `.#worker` and `.#edge`
  # with their defaults (tests/local.py). Only that test has it: every container in a test is another
  # NixOS evaluation, and evaluating all the checks at once must fit in memory.
  local = {
    imports = [ (net "10.99.5.2") userManager ];
    virtualisation.systemd-nspawn.options = [ "--bind=/dev/kvm" ];
    users.users.alice = { isNormalUser = true; uid = 1000; linger = true; extraGroups = [ "kvm" ]; };
    environment.systemPackages = roleApps ++ [ proxy ];
  };
  # A registry root (distribution's filesystem layout) for images = { "<repo>" = <skopeo dir: copy>; },
  # served read-only straight from the store: every blob is a symlink, nothing is copied (the registry
  # container above copies, fine for small images, not for gigabytes). refs.json: repo -> manifest digest.
  registryRoot = images: pkgs.runCommand "registry-root" { nativeBuildInputs = [ pkgs.jq ]; } (''
    v2=$out/docker/registry/v2
    # Images share layers (same digest, same content): the first copy serves them all.
    blob() { mkdir -p $v2/blobs/sha256/''${1:0:2}/$1 && ln -sfn $2 $v2/blobs/sha256/''${1:0:2}/$1/data; }
    link() { mkdir -p $1 && printf sha256:%s $2 > $1/link; }
    echo '{}' > refs.json
  '' + lib.concatStrings (lib.mapAttrsToList (repo: dir: ''
    r=$v2/repositories/${repo}
    m=$(sha256sum < ${dir}/manifest.json | cut -d' ' -f1)
    blob $m ${dir}/manifest.json
    link $r/_manifests/revisions/sha256/$m $m
    for f in ${dir}/*; do
      h=''${f##*/}
      [ ''${#h} = 64 ] || continue
      blob $h $f
      link $r/_layers/sha256/$h $h
    done
    jq --arg r ${repo} --arg d sha256:$m '.[$r] = $d' refs.json > refs.new && mv refs.new refs.json
  '') images) + ''
    mv refs.json $out/refs.json
  '');
  # base.py for every test; prelude/ (the cluster's helpers, by concern) only for tests with the cluster.
  base = builtins.readFile ./base.py;
  prelude = lib.concatMapStringsSep "\n\n" (f: builtins.readFile ./prelude/${f}.py) [ "cli" "requests" "corrosion" "units" "agent" "mesh" "boot" ];
in
{
  inherit containers local pubdash plain ctl registryRoot testCert dashCert;
  # extra: container name -> module added to it; timeout: the test's global timeout in seconds;
  # cluster = false: the script starts what it needs (no boot(), no seed apps); nodes: the containers.
  mkTest = { name, script, seed ? true, cluster ? true, nodes ? containers, extra ? { }, timeout ? 3600 }: pkgs.testers.runNixOSTest {
    name = "flyyy-${name}";
    containers = lib.mapAttrs (n: c: { imports = [ c ownCgroup ] ++ lib.optional (extra ? ${n}) extra.${n}; }) nodes;
    globalTimeout = timeout;
    testScript = "SUITE = \"${name}\"\n" + base + lib.optionalString cluster ("\n\n" + prelude + ''

      boot()
      ${lib.optionalString seed "seed()"}
    '') + script + ''

      done()
    '';
  };
}
