# The control plane: its Corrosion agent and `proxy control`. Its own Corrosion agent gossips on
# 10.250.0.1 once `flyyy init` made the mesh; the API is on 127.0.0.1:4000 and on the mesh; the mesh
# itself (WireGuard on UDP 51820, opened here) needs CAP_NET_ADMIN. It's the mesh's CA too: its own
# certificate goes in meshTls for its Corrosion agent, so it is a fixed user (in meshtls) rather than a
# DynamicUser.
{ config, options, lib, pkgs, ... }:
let
  cfg = config.services.flyyy;
  c = cfg.control;
  fl = import ./lib.nix { inherit pkgs lib; inherit (cfg) packages; };
  inherit (fl) corrosionService corroApiUnits corroApiFlag meshTls restartOnRenewal wgPort;
  inherit (cfg.packages) proxy;
  ingress = lib.concatMapStrings (ip: " --shared-ip ${ip}") c.sharedIps + lib.concatMapStrings (p: " --ip-pool ${p}") c.ipPools;
in
{
  imports = [ ./common.nix ];

  options.services.flyyy.control = {
    enable = lib.mkEnableOption "the flyyy control plane: the API, the desired state, the mesh hub and its CA, and Corrosion";
    sharedIps = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "10.200.0.1" ];
      description = "Where the edges serve HTTP(S) by Host, besides their own listen addresses (each made local on every edge, AnyIP).";
    };
    ipPools = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "10.200.1.0/24" ];
      description = "Where each app's own address comes from (for its tcp services; made local on every edge). None: no tcp services.";
    };
    operatorTokenFile = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/flyyy-operator/token";
      description = ''
        The operator's bearer token (at least 32 characters), root's. Made at first boot if missing (0600 root),
        so `sudo flyyy --api-token-file <this> ...` works on this host; copy it to where the CLI or the
        dashboard runs. The control plane reads it as a systemd credential.
      '';
    };
    endpoint = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      # A control host that says how nodes reach it needs nothing more.
      default = if options.services.flyyy.controlAddress.isDefined then cfg.controlAddress else null;
      defaultText = lib.literalExpression "config.services.flyyy.controlAddress if defined, else null";
      example = "mesh.example.com";
      description = ''
        Where nodes' WireGuard reaches this host: host or host:port (default port ${toString wgPort}). Every join
        token then carries it, so a token is all a new worker needs (`nix run <flake>#join -- '<token>'`, the
        dashboard's join command). Null: tokens without it; a node is told
        (services.flyyy.controlAddress, or `flyyy join <host>`).
      '';
    };
    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Open the mesh's WireGuard port (UDP ${toString wgPort}): how every node reaches the hub.";
    };
  };

  config = lib.mkIf c.enable (lib.mkMerge [
    (restartOnRenewal config.systemd.package [ "corrosion.service" ])
    {
      services.flyyy.enable = true;
      networking.firewall.allowedUDPPorts = lib.mkIf c.openFirewall [ wgPort ];
      users.users.control = { isSystemUser = true; group = "control"; extraGroups = [ "meshtls" ]; };
      users.groups.control = { };
      users.groups.meshtls = { };
      systemd.tmpfiles.rules = [ "d ${meshTls} 0750 control meshtls -" ];
      systemd.services = {
        corrosion = corrosionService "control" 8787 "127.0.0.1:8080";
        operator-token = {
          wantedBy = [ "multi-user.target" ];
          path = [ pkgs.coreutils ];
          serviceConfig = { Type = "oneshot"; RemainAfterExit = true; UMask = "0077"; };
          script = ''
            f=${lib.escapeShellArg c.operatorTokenFile}
            [ -s "$f" ] && exit 0
            mkdir -p "$(dirname "$f")"
            tmp=$(mktemp "$f.XXXXXX")
            od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > "$tmp" && mv "$tmp" "$f"
          '';
        };
        control = {
          wantedBy = [ "multi-user.target" ];
          after = corroApiUnits ++ [ "operator-token.service" ];
          requires = corroApiUnits ++ [ "operator-token.service" ];
          path = [ pkgs.wireguard-tools pkgs.iproute2 ];
          serviceConfig = {
            # %d: the credentials directory; LoadCredential lets it read the root-owned token.
            # Ingress (ingress.rs): HTTP on the shared IPs, each app's own address from the pools. The
            # edges make both local (AnyIP).
            ExecStart = "${proxy}/bin/proxy control --listen 127.0.0.1:4000 --corrosion 127.0.0.1:8080 ${corroApiFlag} --token-file %d/token --mesh-dir /var/lib/flyyy-control --tls-dir ${meshTls}${ingress}${lib.optionalString (c.endpoint != null) " --endpoint ${lib.escapeShellArg c.endpoint}"}";
            LoadCredential = "token:${c.operatorTokenFile}";
            User = "control";
            Group = "control";
            SupplementaryGroups = [ "corroapi" ];
            StateDirectory = "flyyy-control";
            StateDirectoryMode = "0700";
            AmbientCapabilities = "CAP_NET_ADMIN";
            CapabilityBoundingSet = "CAP_NET_ADMIN";
            Restart = "always";
            RestartSec = 1;
          };
        };
      };
    }
  ]);
}
