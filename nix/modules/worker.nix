# A worker: its Corrosion agent, the machine agent and the backhaul. It joins the mesh as
# services.flyyy.worker.name (outbound: behind NAT is fine) and runs machines as Cloud Hypervisor VMs (/dev/kvm).
{ config, lib, pkgs, ... }:
let
  cfg = config.services.flyyy;
  fl = import ./lib.nix { inherit pkgs lib; inherit (cfg) packages; };
  inherit (fl) backhaulPort agentPort flyyyUid meshIp corrosionService corroApiUnits corroApiFlag meshTls waitTls;
  inherit (cfg.packages) proxy kernel initrd;
  n = cfg.worker.name;
in
{
  imports = [ ./common.nix ];

  options.services.flyyy.worker = {
    enable = lib.mkEnableOption "a flyyy worker: Corrosion, the machine agent and the backhaul, on the mesh";
    name = lib.mkOption {
      type = lib.types.strMatching "[a-z][a-z0-9-]{0,31}";
      example = "nuc";
      description = "This worker's name, unique in the cluster: its mesh node's and its `workers` row's (1-32 of a-z, 0-9 and -, starting with a letter).";
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
  };

  config = lib.mkIf cfg.worker.enable (lib.mkMerge [
    (fl.meshNode config.systemd.package cfg.controlAddress n "worker" cfg.worker.endpoint [ "corrosion.service" ])
    {
      services.flyyy.enable = true;
      # Nothing here runs as root. The agent is the flyyy user, lingering so its systemd user instance
      # runs without a login: each VM is a `systemd-run --user` unit of it (fc-<id>), with UMask 0007.
      # Cloud Hypervisor needs only /dev/kvm. The backhaul is its own user, in the flyyy group: that is its
      # only way to the VMs' vsock sockets (0770, in /run/flyyy 0750); the agent's state stays 0700.
      users.users.flyyy = {
        isSystemUser = true;
        uid = flyyyUid;
        group = "flyyy";
        # meshtls: the agent API serves with this node's mesh certificate.
        extraGroups = [ "kvm" "meshtls" ];
        linger = true;
        home = "/var/lib/flyyy";
        createHome = true;
        homeMode = "700";
      };
      users.groups.flyyy.gid = flyyyUid;
      users.users.backhaul = { isSystemUser = true; group = "backhaul"; extraGroups = [ "flyyy" "meshtls" ]; };
      users.groups.backhaul = { };
      systemd.tmpfiles.rules = [ "d /run/flyyy 0750 flyyy flyyy -" ];
      # For operators (and the tests' hand-made orphan VM).
      environment.systemPackages = [ pkgs.cloud-hypervisor ];
      # The VMs' consoles are the flyyy user's units' journal: on disk, journald splits off a journal per
      # user (SplitMode=uid), which the agent reads as flyyy for a machine's logs (GET /v1/machines/<id>/logs);
      # kept in memory, all of it is the system journal, which flyyy can't read.
      services.journald.settings.Journal.Storage = lib.mkDefault "persistent";
      systemd.services = {
        # No forwarding: VMs have no network; the backhaul reaches them over vsock. A network namespace
        # inherits the host's ip_forward, and boot.kernel.sysctl doesn't apply in containers
        # (systemd-sysctl skips them), so set it at boot.
        no-forwarding = {
          wantedBy = [ "multi-user.target" ];
          before = [ "agent.service" ];
          serviceConfig = { Type = "oneshot"; RemainAfterExit = true; ExecStart = "${pkgs.procps}/bin/sysctl -w net.ipv4.ip_forward=0"; };
        };
        corrosion = corrosionService n 8787 "127.0.0.1:8080";
        # The machine agent: what the control plane sends over its API (mTLS, the control plane's
        # certificate only) → fc-<id> user units, facts → `machine_status`. It keeps what it was told, so
        # it runs on with the control plane down. VMs are units of flyyy's systemd, not children of the
        # agent, so they outlive an agent crash.
        agent = {
          wantedBy = [ "multi-user.target" ];
          wants = [ "user@${toString flyyyUid}.service" ];
          after = [ "user@${toString flyyyUid}.service" ] ++ corroApiUnits;
          requires = corroApiUnits;
          path = [ pkgs.crane pkgs.erofs-utils pkgs.e2fsprogs pkgs.coreutils pkgs.cloud-hypervisor config.systemd.package ];
          # Its systemd-run --user units need the user manager's runtime directory.
          environment.XDG_RUNTIME_DIR = "/run/user/${toString flyyyUid}";
          serviceConfig = {
            # It announces the backhaul and its API on this worker's mesh address.
            ExecStart = pkgs.writeShellScript "agent" ''
              ${waitTls}
              ip=$(${meshIp})
              exec ${proxy}/bin/proxy agent --corrosion 127.0.0.1:8080 ${corroApiFlag} --worker ${n} --kernel ${kernel}/kernel --initrd ${initrd} \
                --backhaul "$ip:${toString backhaulPort}" --api "$ip:${toString agentPort}" --tls-dir ${meshTls} --state-dir /var/lib/flyyy --run-dir /run/flyyy
            '';
            SyslogIdentifier = "agent";
            User = "flyyy";
            Group = "flyyy";
            # The token for this unit only: the VMs (flyyy's user manager) don't get it.
            SupplementaryGroups = [ "corroapi" ];
            UMask = "0077";
            Restart = "always";
            RestartSec = 1;
          };
        };
        # The backhaul: CONNECT <instance-id> from the edges → this worker's VM, over its vsock socket.
        # Unprivileged; its only extra right is the flyyy group on /run/flyyy.
        proxy = {
          wantedBy = [ "multi-user.target" ];
          after = corroApiUnits;
          requires = corroApiUnits;
          serviceConfig = {
            # Only on the mesh: an edge reaches it through WireGuard, with mTLS (edge certificates only).
            ExecStart = pkgs.writeShellScript "backhaul" ''
              ${waitTls}
              exec ${proxy}/bin/proxy worker --listen "$(${meshIp}):${toString backhaulPort}" --state 127.0.0.1:9080 --corrosion 127.0.0.1:8080 ${corroApiFlag} --worker ${n} \
                --run-dir /run/flyyy --tls-dir ${meshTls}
            '';
            SyslogIdentifier = "backhaul";
            User = "backhaul";
            Group = "backhaul";
            SupplementaryGroups = [ "corroapi" ];
            Restart = "always";
          };
        };
      };
    }
  ]);
}
