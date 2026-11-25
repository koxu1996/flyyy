# The public dashboard: `proxy dashboard` serves the page over HTTPS and forwards its API calls to the
# control plane over the mesh, each with its caller's own token (it holds none). Its host is a mesh node
# of role dashboard: on the mesh for the control plane's API only, with no mesh certificate (so nothing a
# certificate opens: not the backhauls, the agent APIs or Corrosion's gossip). No Corrosion here either.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.flyyy;
  d = cfg.dashboard;
  fl = import ./lib.nix { inherit pkgs lib; inherit (cfg) packages; };
  inherit (cfg.packages) proxy;
  port = lib.toInt (lib.last (lib.splitString ":" d.listen));
in
{
  imports = [ ./common.nix ];

  options.services.flyyy.dashboard = {
    enable = lib.mkEnableOption "the public flyyy dashboard: the page over HTTPS, its calls to the control plane with each caller's own token";
    nodeName = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "This host's mesh node name.";
    };
    host = lib.mkOption {
      type = lib.types.str;
      example = "flyyy.devpeek.io";
      description = "Its public name: the only Host it answers, and the Origin its writes must come from (https://<this>).";
    };
    listen = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:8443";
      description = "Where it serves HTTPS.";
    };
    tlsCert = lib.mkOption { type = lib.types.str; description = "The certificate chain (PEM) for `host`, readable by the dashboard user; `systemctl reload flyyy-dashboard` rereads it."; };
    tlsKey = lib.mkOption { type = lib.types.str; description = "Its key (PEM), readable by the dashboard user (group flyyy-dashboard, 0640)."; };
    appsUrl = lib.mkOption {
      type = lib.types.str;
      example = "https://{app}.flyyy.devpeek.io/";
      description = "Where the page's app links go, {app} the app's name.";
    };
    flakeUrl = lib.mkOption {
      type = lib.types.str;
      default = fl.flakeUrl;
      description = "The flake the join dialog's command runs: nix run '<this>#join' -- [--name NAME] '<token>'.";
    };
  };

  config = lib.mkIf d.enable (lib.mkMerge [
    (fl.meshNode config.systemd.package cfg.controlAddress d.nodeName "dashboard" null [ ])
    {
      services.flyyy.enable = true;
      users.users.flyyy-dashboard = { isSystemUser = true; uid = 2090; group = "flyyy-dashboard"; };
      users.groups.flyyy-dashboard.gid = 2090;
      systemd.services.flyyy-dashboard = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${proxy}/bin/proxy dashboard --listen ${d.listen} --host ${d.host} --tls-cert ${d.tlsCert} --tls-key ${d.tlsKey} --apps-url ${lib.escapeShellArg d.appsUrl} --flake-url ${lib.escapeShellArg d.flakeUrl}";
          ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
          User = "flyyy-dashboard";
          Group = "flyyy-dashboard";
          AmbientCapabilities = lib.optional (port < 1024) "CAP_NET_BIND_SERVICE";
          CapabilityBoundingSet = lib.optional (port < 1024) "CAP_NET_BIND_SERVICE";
          NoNewPrivileges = true;
          Restart = "always";
          RestartSec = 1;
        };
      };
    }
  ]);
}
