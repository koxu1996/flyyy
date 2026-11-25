# What every flyyy host has, whatever its role (control.nix, edge.nix and worker.nix import this): the
# options they share, the node's Corrosion API token, the CLI, and the mesh interface trusted by the
# firewall (everything between nodes runs inside WireGuard).
{ config, lib, pkgs, ... }:
let
  cfg = config.services.flyyy;
  fl = import ./lib.nix { inherit pkgs lib; inherit (cfg) packages; };
in
{
  options.services.flyyy = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      internal = true;
      description = "Set by the role modules: this host runs a flyyy role.";
    };
    packages = lib.mkOption {
      type = lib.types.attrsOf lib.types.package;
      description = "flyyy's packages for this host's system: proxy, corrosion, kernel, initrd. The flake's nixosModules set them.";
    };
    controlAddress = lib.mkOption {
      type = lib.types.str;
      example = "203.0.113.10";
      description = ''
        The control plane's address as this node reaches it: where the node's WireGuard dials the mesh hub
        (`flyyy join <this>`, UDP ${toString fl.wgPort}). Its public address, or a private one the node can
        reach (an edge container on the control plane's host: the host's side of the container network).
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    fl.corroApiModule
    {
      networking.firewall.trustedInterfaces = [ "flyyy0" ];
      environment.systemPackages = [ cfg.packages.proxy pkgs.wireguard-tools ];
    }
  ]);
}
