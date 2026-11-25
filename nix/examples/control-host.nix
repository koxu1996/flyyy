# The control-plane host (nixvps: x86_64, public 203.0.113.10; RKE2 owns :80 and :443): the control plane
# (mesh hub, CA, API, Corrosion) with WireGuard on UDP 51820, and the edge and the public dashboard each in
# a NixOS container as its own mesh node. RKE2's ingress passes *.flyyy.devpeek.io :443 through (TLS
# passthrough) to the edge's HTTPS port, forwarded from the host's :8443, and flyyy.devpeek.io to the
# dashboard's, forwarded from the host's :9443. Import it with the flake: `(import ./nix/examples/control-host.nix
# flyyy)`, where `flyyy` is this flake.
flyyy:
{ lib, ... }:
let
  # The container network: the host's side and the edge's, NATed to the host's uplink.
  hostAddress = "192.168.120.1";
  edgeAddress = "192.168.120.2";
  # The edge's HTTPS port, in the container and forwarded on the host (what RKE2 passes TLS through to).
  httpsPort = 8443;
  # The dashboard's container network, and its HTTPS port on the host (8443 in the container).
  dashHost = "192.168.121.1";
  dashAddress = "192.168.121.2";
  dashPort = 9443;
in
{
  imports = [ flyyy.nixosModules.control ];

  services.flyyy.control = {
    enable = true;
    # No shared IPs: the edge serves HTTPS by Host on its own listen address, which RKE2 reaches. Each
    # app's own address (its tcp services) is from this pool, local on the edge only.
    # shortcut: tcp services aren't reachable from outside here; only HTTPS through RKE2 is.
    ipPools = [ "10.200.1.0/24" ];
  };

  # The edge's WireGuard runs in its container; the module is the host kernel's.
  boot.kernelModules = [ "wireguard" ];
  networking.nat = {
    enable = true;
    internalInterfaces = [ "ve-flyyy-edge" "ve-flyyy-dash" ];
    # The host's uplink.
    externalInterface = lib.mkDefault "eth0";
  };

  containers.flyyy-edge = {
    autoStart = true;
    privateNetwork = true;
    inherit hostAddress;
    localAddress = edgeAddress;
    forwardPorts = [{ protocol = "tcp"; hostPort = httpsPort; containerPort = httpsPort; }];
    # The apps domain's wildcard certificate, from the host: cert.pem and key.pem (the key 0640 root:2080,
    # the edge user's uid). `nixos-container run flyyy-edge -- systemctl reload flyyy-edge` rereads it.
    bindMounts."/var/lib/edge-tls" = { hostPath = "/var/lib/flyyy-edge-tls"; isReadOnly = true; };
    config = {
      imports = [ flyyy.nixosModules.edge ];
      system.stateVersion = "26.05";
      services.flyyy = {
        # The control plane as the container reaches it: the host's side of the container network.
        controlAddress = hostAddress;
        edge = {
          enable = true;
          # Plain HTTP only redirects to HTTPS; nothing outside reaches it.
          listen = "0.0.0.0:8080";
          https = { listen = "0.0.0.0:${toString httpsPort}"; cert = "/var/lib/edge-tls/cert.pem"; key = "/var/lib/edge-tls/key.pem"; };
          appsDomain = "flyyy.devpeek.io";
        };
      };
    };
  };

  # The public dashboard, https://flyyy.devpeek.io: the page, and its calls to the control plane over the
  # mesh with each caller's own token (it holds none, and gets no mesh certificate).
  containers.flyyy-dash = {
    autoStart = true;
    privateNetwork = true;
    hostAddress = dashHost;
    localAddress = dashAddress;
    forwardPorts = [{ protocol = "tcp"; hostPort = dashPort; containerPort = 8443; }];
    # The same certificate as the edge's, with flyyy.devpeek.io in it too (a wildcard doesn't cover the
    # name it's under): cert.pem, and key.pem 0640 root:2090 (the dashboard user's uid).
    bindMounts."/var/lib/dash-tls" = { hostPath = "/var/lib/flyyy-dash-tls"; isReadOnly = true; };
    config = {
      imports = [ flyyy.nixosModules.dashboard ];
      system.stateVersion = "26.05";
      services.flyyy = {
        controlAddress = dashHost;
        dashboard = {
          enable = true;
          host = "flyyy.devpeek.io";
          tlsCert = "/var/lib/dash-tls/cert.pem";
          tlsKey = "/var/lib/dash-tls/key.pem";
          appsUrl = "https://{app}.flyyy.devpeek.io/";
        };
      };
    };
  };
}
