# An edge host: one mesh node (role edge) with one edge or more (services.flyyy.edge.instances), each its
# Corrosion agent, the edge proxy and anyip. Several edges on one host share its listen addresses
# (SO_REUSEPORT); the test cluster runs two. Ports are per edge, from its index i: 880i gossip, 810i
# Corrosion API, 910i /state, 920i its load counters and metrics (on the mesh, for the control plane).
{ config, lib, pkgs, ... }:
let
  cfg = config.services.flyyy;
  e = cfg.edge;
  fl = import ./lib.nix { inherit pkgs lib; inherit (cfg) packages; };
  inherit (fl) meshIp corrosionServiceIn corroApiUnits corroApiFlag meshTls waitTls;
  inherit (cfg.packages) proxy;
  flags = " --apps-domain ${e.appsDomain}" + lib.optionalString (e.https != null) " --https ${e.https.listen} --tls-cert ${e.https.cert} --tls-key ${e.https.key}";
  units = name: i: let is = toString i; in {
    "corrosion-${name}" = corrosionServiceIn "corrosion-${name}" name (8800 + i) "127.0.0.1:810${is}";
    ${name} = {
      wantedBy = [ "multi-user.target" ];
      after = corroApiUnits;
      requires = corroApiUnits;
      serviceConfig = {
        # Its load counters on the mesh, for the control plane (mTLS, its certificate only).
        ExecStart = pkgs.writeShellScript name ''
          ${waitTls}
          exec ${proxy}/bin/proxy edge --listen ${e.listen} --state 127.0.0.1:910${is} --corrosion 127.0.0.1:810${is} ${corroApiFlag} --name ${name}${flags} \
            --tls-dir ${meshTls} --load "$(${meshIp}):920${is}"
        '';
        SyslogIdentifier = name;
        # Rereads the certificate and key.
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        User = "edge";
        Group = "edge";
        SupplementaryGroups = [ "corroapi" ];
        Restart = "always";
        AmbientCapabilities = "CAP_NET_BIND_SERVICE";
      };
    };
    # Makes the ingress pools local here (AnyIP), so the edge can bind any of their addresses.
    "${name}-anyip" = {
      wantedBy = [ "multi-user.target" ];
      after = corroApiUnits;
      requires = corroApiUnits;
      path = [ pkgs.iproute2 ];
      serviceConfig = {
        ExecStart = "${proxy}/bin/proxy anyip --corrosion 127.0.0.1:810${is} ${corroApiFlag}";
        DynamicUser = true;
        SupplementaryGroups = [ "corroapi" ];
        AmbientCapabilities = "CAP_NET_ADMIN";
        CapabilityBoundingSet = "CAP_NET_ADMIN";
        Restart = "always";
        RestartSec = 1;
      };
    };
  };
in
{
  imports = [ ./common.nix ];

  options.services.flyyy.edge = {
    enable = lib.mkEnableOption "a flyyy edge host: apps served by Host and their tcp services, on the mesh";
    nodeName = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "This host's mesh node name.";
    };
    endpoint = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "home.example.com:51821";
      description = ''
        The WireGuard endpoint the other nodes should use for this one (host:port), sent at its first join;
        null: the address the control plane sees its packets from. Behind NAT, a UDP port forwarded to this
        node's 51820, so nodes other than the control plane can reach it too.
      '';
    };
    instances = lib.mkOption {
      type = lib.types.attrsOf (lib.types.submodule {
        options.index = lib.mkOption { type = lib.types.ints.between 1 9; description = "Picks its ports (880i, 810i, 910i, 920i)."; };
      });
      default = { ${config.networking.hostName} = { index = 1; }; };
      defaultText = lib.literalExpression "{ \${config.networking.hostName} = { index = 1; }; }";
      description = "The edges on this host, by name (unique in the cluster: the `edges` table's key, the x-edge header); each its own Corrosion agent.";
    };
    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:80";
      example = "0.0.0.0:8080";
      description = "Where apps are served over plain HTTP by Host; with `https`, it only redirects there.";
    };
    https = lib.mkOption {
      default = null;
      description = "HTTPS with an already-issued certificate (`systemctl reload <edge>` rereads it).";
      type = lib.types.nullOr (lib.types.submodule {
        options = {
          listen = lib.mkOption { type = lib.types.str; example = "0.0.0.0:8443"; description = "The HTTPS address."; };
          cert = lib.mkOption { type = lib.types.str; description = "The certificate chain (PEM), readable by the edge user."; };
          key = lib.mkOption { type = lib.types.str; description = "Its key (PEM), readable by the edge user (group edge, 0640)."; };
        };
      });
    };
    appsDomain = lib.mkOption {
      type = lib.types.str;
      default = "flyyy.localhost";
      example = "flyyy.devpeek.io";
      description = "The apps' domain: `<app>.<this>` is an app's vhost.";
    };
  };

  config = lib.mkIf e.enable (lib.mkMerge [
    (fl.meshNode config.systemd.package cfg.controlAddress e.nodeName "edge" e.endpoint (map (n: "corrosion-${n}.service") (lib.attrNames e.instances)))
    {
      services.flyyy.enable = true;
      # meshtls: its mesh certificate, for mTLS to the workers' backhauls.
      users.users.edge = { isSystemUser = true; uid = 2080; group = "edge"; extraGroups = [ "meshtls" ]; };
      users.groups.edge.gid = 2080;
      systemd.services = lib.mkMerge (lib.mapAttrsToList (name: v: units name v.index) e.instances);
    }
  ]);
}
