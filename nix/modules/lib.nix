# Helpers the role modules (control.nix, edge.nix, worker.nix) share: the mesh's addresses and ports,
# Corrosion (a service per agent), the API token, the mesh certificate, and `meshNode`, a node's side of the
# mesh (`flyyy join`). Plain functions of the packages; the options are in the modules.
# `packages` is one argument (not merged in), so a module can pass its option's value without forcing it.
{ pkgs, lib, packages }:
let
  inherit (packages) corrosion proxy;
in
rec {
  # Nothing lists the workers: they announce themselves in the `workers` table (backhaul + heartbeat), so
  # another one (worker4 in the tests) can join at runtime. Edges share 127.0.0.1:80 (SO_REUSEPORT).
  backhaulPort = 4280;
  # Each worker's agent API, on its mesh address: the control plane sends it its machines (mTLS).
  agentPort = 4281;
  # Everything between nodes runs on the WireGuard mesh (10.250.0.0/16, proxy/src/wg.rs): Corrosion gossip,
  # the backhaul, the control API (10.250.0.1:4000). Corrosion APIs and /state are 127.0.0.1 only. The
  # control plane's own address, where nodes' WireGuard reaches it, is services.flyyy.controlAddress.
  meshControl = "10.250.0.1";
  # The control plane's WireGuard port (UDP), opened on its host.
  wgPort = 51820;
  # Corrosion's schema: one agent per worker, per edge and in the control plane, gossiping on the mesh
  # (proxy/schema.sql says what each table is and who writes it; the Rust code checks its queries
  # against it, corro.rs).
  schema = ../../proxy/schema.sql;
  # Where the dashboard's join command (and the join app's module snippet) get flyyy from; proxy/src/dash.rs
  # has the same default.
  flakeUrl = "git+https://github.com/koxu1996/flyyy?ref=main";
  # The worker's unprivileged user (fixed, so its runtime dir /run/user/<uid> is known here).
  flyyyUid = 2000;
  # This node's mesh address, once it has one (not a join token's 10.250.255.x): what its services bind.
  meshIp = pkgs.writeShellScript "mesh-ip" ''
    while :; do
      ip=$(${pkgs.iproute2}/bin/ip -4 -o addr show dev flyyy0 2>/dev/null | ${pkgs.gawk}/bin/awk '{ split($4, a, "/"); if (a[1] !~ /^10\.250\.255\./) print a[1] }')
      [ -n "$ip" ] && { echo "$ip"; exit 0; }
      sleep 1
    done
  '';
  # The Corrosion API's bearer token, one per node (Corrosion's [api] authorization): made by root on the
  # node's first boot (corroapi-token), 0640 root:corroapi. Corrosion and our services read it through
  # SupplementaryGroups=corroapi, so no other local process can read or write cluster state through the
  # API: not other users, not the VMs (the agent's user manager isn't in the group).
  corroApiToken = "/var/lib/flyyy-corroapi/token";
  corroApiUnits = [ "corroapi-token.service" ];
  corroApiModule = {
    users.groups.corroapi = { };
    systemd.tmpfiles.rules = [ "d /var/lib/flyyy-corroapi 0750 root corroapi -" ];
    systemd.services.corroapi-token = {
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-tmpfiles-setup.service" ];
      path = [ pkgs.coreutils ];
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; UMask = "0077"; };
      script = ''
        [ -s ${corroApiToken} ] && exit 0
        tmp=$(mktemp ${corroApiToken}.XXXXXX)
        od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > "$tmp"
        chgrp corroapi "$tmp" && chmod 0640 "$tmp" && mv "$tmp" ${corroApiToken}
      '';
    };
  };
  # The flag every role passes for it.
  corroApiFlag = "--corrosion-token-file ${corroApiToken}";
  # `dir`: the state/runtime directory name, `corrosion` unless two agents share a container (tests).
  # Gossip on this node's mesh address and `port`: known only once it has joined, so the config is a
  # template (@IP@) written at start, into /run/<dir> (the service's RuntimeDirectory; the join app's
  # worker makes it itself).
  # Gossip is mTLS with the node's mesh certificate (pki.rs; Corrosion verifies a peer by its IP SAN)
  # and client certificates required, inside WireGuard. The API wants the node's token (appended at start
  # by a shell builtin, so it's never on a command line); the config and the admin socket are the
  # service's own (0700 runtime directory).
  corrosionStart = dir: name: port: api: let
    tlsFile = f: "${meshTls}/${f}";
    template = (pkgs.formats.toml { }).generate "corrosion-${name}.toml" {
      db = { path = "/var/lib/${dir}/state.db"; schema_paths = [ "${schema}" ]; };
      gossip = {
        addr = "@IP@:${toString port}";
        # Outgoing QUIC from the mesh address too (by default it binds [::]:0, every interface).
        client_addr = "@IP@:0";
        bootstrap = lib.optional (name != "control") "${meshControl}:8787";
        tls = {
          cert_file = tlsFile "cert.pem";
          key_file = tlsFile "key.pem";
          ca_file = tlsFile "ca.pem";
          client = { cert_file = tlsFile "cert.pem"; key_file = tlsFile "key.pem"; };
        };
      };
      api.addr = api;
      admin.path = "/run/${dir}/admin.sock";
      # Corrosion waits longer after every sync round (1, 2, 4, 8, then 15 s) and never resets, even
      # after a round that moved nothing. A node that just joined can spend its first rounds on peers
      # it can't reach yet (their WireGuard endpoint still unknown) or that have nothing it lacks,
      # and so took up to a minute to get rows written before it joined (an edge's ip_pools).
      # The control plane hands out a node's endpoint from its bootstrap tunnel on (mesh.rs), but the
      # others still learn a new node only on their 5 s peer poll.
      # shortcut: retry every 2 s at most, instead of pushing peer changes to the nodes.
      perf.max_sync_backoff = 2;
    };
  in pkgs.writeShellScript "corrosion-${name}" ''
    ip=$(${meshIp})
    ${waitTls}
    token=$(<${corroApiToken})
    {
      ${pkgs.gnused}/bin/sed "s/@IP@/$ip/" ${template}
      printf '\n[api.authorization]\nbearer-token = "%s"\n' "$token"
    } > /run/${dir}/config.toml
    exec ${corrosion}/bin/corrosion -c /run/${dir}/config.toml agent
  '';
  corrosionServiceIn = dir: name: port: api: {
    wantedBy = [ "multi-user.target" ];
    after = corroApiUnits;
    requires = corroApiUnits;
    serviceConfig = {
      ExecStart = corrosionStart dir name port api;
      # Not the wrapper script's store name.
      SyslogIdentifier = "corrosion";
      # Needs no privileges: unprivileged ports, its own state and runtime directories; its mesh
      # certificate through the meshtls group, its API token through corroapi.
      DynamicUser = true;
      SupplementaryGroups = [ "meshtls" "corroapi" ];
      StateDirectory = dir;
      RuntimeDirectory = dir;
      RuntimeDirectoryMode = "0700";
      UMask = "0077";
      Restart = "always";
      RestartSec = 1;
    };
  };
  corrosionService = corrosionServiceIn "corrosion";
  # A node's mesh certificate, key and CA (pki.rs): 0750, group meshtls, whose members read them.
  meshTls = "/var/lib/flyyy-tls";
  # Services that need it wait for the first certificate (a node gets it right after its approval).
  waitTls = "until [ -s ${meshTls}/cert.pem ]; do sleep 1; done";
  # Corrosion reads its certificate at start only: restart these when it's renewed (path unit). Not on
  # the first one: they're waiting for it (waitTls), and a Corrosion stopped while it starts can hang.
  # `systemd`: the host's systemd package (config.systemd.package).
  restartOnRenewal = systemd: units: {
    systemd.paths.mesh-cert = { wantedBy = [ "multi-user.target" ]; pathConfig.PathChanged = "${meshTls}/cert.pem"; };
    systemd.services.mesh-cert = {
      serviceConfig = { Type = "oneshot"; StateDirectory = "mesh-cert"; };
      script = ''
        if [ ! -e "$STATE_DIRECTORY/had-one" ]; then touch "$STATE_DIRECTORY/had-one"; exit 0; fi
        ${lib.optionalString (units != [ ]) "${systemd}/bin/systemctl try-restart ${lib.concatStringsSep " " units}"}
      '';
    };
  };
  # A node's side of the mesh: `flyyy join <controlAddr>`, with only CAP_NET_ADMIN. The first start needs
  # a token in /var/lib/flyyy-mesh/join-token (from `flyyy token`, or the dashboard; deleted once spent):
  # a path unit starts the join as soon as one is put there, so `install` of a token is all a new node
  # needs. Later starts resume from state. It also keeps the node's mesh certificate (role: worker or
  # edge) in meshTls, renewing it over the mesh.
  joinToken = "/var/lib/flyyy-mesh/join-token";
  # `endpoint`: the WireGuard endpoint it advertises at its join (null: the one the control plane learns).
  meshNode = systemd: controlAddr: name: role: endpoint: corrosionUnits: lib.recursiveUpdate (restartOnRenewal systemd corrosionUnits) {
    users.users.mesh = { isSystemUser = true; group = "mesh"; extraGroups = [ "meshtls" ]; };
    users.groups.mesh = { };
    users.groups.meshtls = { };
    environment.systemPackages = [ pkgs.wireguard-tools ];
    systemd.tmpfiles.rules = [ "d /var/lib/flyyy-mesh 0700 mesh mesh -" "d ${meshTls} 0750 mesh meshtls -" ];
    systemd.services.mesh = {
      wantedBy = [ "multi-user.target" ];
      unitConfig.ConditionPathExists = [ "|${joinToken}" "|/var/lib/flyyy-mesh/state.json" ];
      path = [ pkgs.wireguard-tools pkgs.iproute2 ];
      serviceConfig = {
        ExecStart = "${proxy}/bin/flyyy join ${controlAddr} --token-file ${joinToken} --name ${name} --role ${role} --state-dir /var/lib/flyyy-mesh --tls-dir ${meshTls}${lib.optionalString (endpoint != null) " --endpoint ${endpoint}"}";
        User = "mesh";
        Group = "mesh";
        AmbientCapabilities = "CAP_NET_ADMIN";
        CapabilityBoundingSet = "CAP_NET_ADMIN";
        Restart = "always";
        RestartSec = 2;
      };
    };
    # A token put there starts the join: nothing else to run on the node.
    systemd.paths.mesh-join = { wantedBy = [ "multi-user.target" ]; pathConfig = { PathExists = joinToken; Unit = "mesh.service"; }; };
  };
}
