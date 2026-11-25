# `nix run <flake>#join -- [--name NAME] '<token>'`: this machine joins the mesh as a worker without
# flyyy's NixOS module: on a NixOS host that doesn't have it (even mutableUsers = false, root wiped at
# boot) or on any Linux with Nix and systemd. The dashboard's join dialog shows the command.
# Nothing runs as root. Each part of the worker (mesh, Corrosion, agent, backhaul) is a system unit,
# flyyy-<part> in flyyy-worker.slice, with User= the user who ran it (or --user, when that's root); only the
# mesh gets CAP_NET_ADMIN. The VMs are `systemd-run --user` units of that user (lingering), as the agent
# makes them everywhere. sudo is for the setup only: the units, linger, the firewall rule, and the
# user's own /var/lib/flyyy* and /run/flyyy.
# The setup it shares with the other `nix run` apps (host-apps.nix) is host.nix's.
args@{ pkgs, lib, proxy, kernel, initrd, meshIp, waitTls, meshTls, corroApiToken, backhaulPort, agentPort, flakeUrl, ... }:
let
  host = import ./host.nix args;
  mesh = "/var/lib/flyyy-mesh";
  parts = "mesh corrosion agent backhaul";
  # The directories the user owns: mode, path.
  dirs = host.meshDirs ++ host.corrosionDirs ++ [ "0700 /var/lib/flyyy" "0750 /run/flyyy" ];
  # One part of the worker, as its unit runs it (as the user): `flyyy-worker <part> <name> <host's systemd
  # bin dir> [endpoint]`. The agent's systemctl and systemd-run must be the running systemd's. Corrosion,
  # the agent and the backhaul wait for the mesh certificate, which the mesh part gets once approved.
  run = pkgs.writeShellApplication {
    name = "flyyy-worker";
    runtimeInputs = [ proxy pkgs.wireguard-tools pkgs.iproute2 pkgs.coreutils pkgs.crane pkgs.erofs-utils pkgs.e2fsprogs pkgs.cloud-hypervisor ];
    text = ''
      part=$1 name=$2
      PATH=$PATH:$3
      endpoint=''${4:-}
      case $part in
        mesh)
          token=()
          [ -s ${mesh}/join-token ] && token=(--token-file ${mesh}/join-token)
          ep=()
          [ -n "$endpoint" ] && ep=(--endpoint "$endpoint")
          exec flyyy join "''${token[@]}" "''${ep[@]}" --name "$name" --role worker --state-dir ${mesh} --tls-dir ${meshTls}
          ;;
        corrosion)
          # Its config is a worker's, whatever the name.
          ${host.corrosionPart "worker"}
          ;;
        agent)
          ${waitTls}
          ip=$(${meshIp})
          XDG_RUNTIME_DIR=/run/user/$(id -u)
          export XDG_RUNTIME_DIR
          exec proxy agent --corrosion 127.0.0.1:8080 --corrosion-token-file ${corroApiToken} --worker "$name" --kernel ${kernel}/kernel --initrd ${initrd} \
            --backhaul "$ip:${toString backhaulPort}" --api "$ip:${toString agentPort}" --tls-dir ${meshTls} --state-dir /var/lib/flyyy --run-dir /run/flyyy
          ;;
        backhaul)
          ${waitTls}
          exec proxy worker --listen "$(${meshIp}):${toString backhaulPort}" --state 127.0.0.1:9080 --corrosion 127.0.0.1:8080 --corrosion-token-file ${corroApiToken} \
            --worker "$name" --run-dir /run/flyyy --tls-dir ${meshTls}
          ;;
        *) echo "flyyy-worker: unknown part $part" >&2; exit 2 ;;
      esac
    '';
  };
  # The NixOS module snippet that makes the worker permanent, printed by --nixos ($name, $control and $user
  # are the join app's).
  snippet = ''
    To make $name permanent with flyyy's NixOS module (it keeps this node's mesh identity):

      # flake.nix: inputs.flyyy.url = "${flakeUrl}";
      imports = [ inputs.flyyy.nixosModules.worker ];
      services.flyyy.controlAddress = "$control";
      services.flyyy.worker = { enable = true; name = "$name"; };

    Then: sudo systemctl stop flyyy-worker.slice, nixos-rebuild switch, and hand this node's state to the
    module's users: sudo systemctl --user -M $user@ stop 'fc-*'; sudo rm -rf /var/lib/corrosion /var/lib/flyyy-corroapi;
    sudo chown -R mesh:mesh ${mesh}; sudo chown -R mesh:meshtls ${meshTls}; sudo chown -R flyyy:flyyy /var/lib/flyyy;
    sudo systemctl restart corroapi-token mesh corrosion agent proxy
  '';
in
pkgs.writeShellApplication {
  name = "flyyy-join";
  inherit (host) runtimeInputs;
  text = ''
    app=flyyy-join slice=flyyy-worker
    ${host.prelude}
    usage() {
      cat <<EOF
    usage: flyyy-join [--name NAME] [--endpoint HOST:PORT] [--user USER] [--nixos] [TOKEN]
      Joins this machine to a flyyy mesh as a worker, run by you: the units flyyy-mesh, -corrosion, -agent and
      -backhaul (flyyy-worker.slice) run as your user, and so do the VMs.
      TOKEN       a join token (dashboard: + Add node; CLI: flyyy token). A first join needs it;
                  without one, it resumes this machine's join from ${mesh}.
      --name      this worker's name, unique in the cluster: a-z, 0-9 and -, starting with a letter
                  (default: the hostname, lowercased, anything else a -)
      --endpoint  for a first join behind NAT: the WireGuard endpoint the other nodes should use for this one
      --user      run as root: the user the worker runs as (default \$SUDO_USER); nothing runs as root
      --nixos     print the NixOS-module setup that makes it permanent (and how to hand its state over), and exit
    EOF
    }
    name="" endpoint="" token="" user="" flag_nixos=0
    while [ $# -gt 0 ]; do
      case $1 in
        -h|--help) usage; exit 0 ;;
        --nixos) flag_nixos=1; shift ;;
        --name|--endpoint|--user) [ $# -ge 2 ] || { usage >&2; exit 2; }; printf -v "''${1#--}" '%s' "$2"; shift 2 ;;
        -*) echo "unknown flag $1" >&2; usage >&2; exit 2 ;;
        *) [ -z "$token" ] || { usage >&2; exit 2; }; token=$1; shift ;;
      esac
    done
    who "$user"
    # flyyy join checks it too, but only once the units are there.
    [ -z "$endpoint" ] || [[ $endpoint =~ ^([A-Za-z0-9.-]+|\[[0-9A-Fa-f:]+\]):[0-9]{1,5}$ ]] ||
      die "--endpoint $endpoint: want <IPv4>:<port>, [<IPv6>]:<port> or <name>:<port>"

    ! control_host || die "this is the control plane's host (/var/lib/flyyy-control): one flyyy0 per host, so no worker joins here"

    # A first join, or a resume of the one this machine started.
    state=$(as_root cat ${mesh}/state.json 2> /dev/null || true)
    if [ -n "$state" ]; then
      joined=$(jq -r .name <<< "$state") role=$(jq -r .role <<< "$state")
      [ "$role" = worker ] || die "this machine joined the mesh as $joined, role $role, not as a worker"
      owner=$(stat -c %U ${mesh})
      # root: an earlier join app's, which ran everything as root.
      [ "$owner" = "$user" ] || [ "$owner" = root ] || die "$joined joined as $owner: run it as $owner"
      if [ -n "$token" ]; then
        [ -n "$(jq -r '.ip // empty' <<< "$state")" ] || echo "flyyy-join: to start over with a new token: sudo systemctl stop flyyy-worker.slice; sudo rm -rf ${mesh}" >&2
        die "this machine already joined as $joined (${mesh}/state.json): run it again without a token to resume"
      fi
      [ -z "$name" ] || [ "$name" = "$joined" ] || die "this machine joined as $joined, not $name"
      [ -z "$endpoint" ] || die "--endpoint is for a first join; $joined keeps the one it joined with"
      name=$joined
      control=$(jq -r .control <<< "$state")
      approved=$(jq -r '.ip // empty' <<< "$state")
      progress="resuming $name"
    else
      [ -n "$token" ] || die "a first join needs a token (dashboard: + Add node): flyyy-join '<token>'"
      [ -n "$name" ] || name=$(default_name)
      name_ok "$name"
      [[ $token =~ ^flyyy-join:[^@]+@(.+)$ ]] ||
        die "that token carries no control plane address: make one on a control plane with services.flyyy.control.endpoint (or controlAddress) set"
      control=''${BASH_REMATCH[1]}
      approved=
      progress="joining as $name through $control"
    fi
    if [ "$flag_nixos" = 1 ]; then
      cat <<EOF
    ${snippet}
    EOF
      exit 0
    fi

    # What this machine needs, before anything changes.
    check_host
    [ -c /dev/kvm ] || die "no /dev/kvm: this machine can't run Cloud Hypervisor VMs (KVM off in firmware, or a VM without nested virtualisation)"
    # The VMs run as $user: /dev/kvm through its group, or open to everyone.
    kmode=$(stat -c %a /dev/kvm) kgroup=$(stat -c %G /dev/kvm)
    [ $((8#$kmode & 6)) = 6 ] || id -nG "$user" | tr ' ' '\n' | grep -qx "$kgroup" ||
      die "$user can't use /dev/kvm ($kmode, group $kgroup): it isn't 0666, and $user isn't in $kgroup. Add $user to $kgroup (NixOS: users.users.$user.extraGroups = [ \"$kgroup\" ]; elsewhere: sudo usermod -aG $kgroup $user) or make /dev/kvm 0666"
    check_wireguard
    echo "flyyy-join: $progress"

    # The setup, with sudo. The units go first: an earlier run's (this one's, or the old root unit).
    # shellcheck disable=SC2046
    units_clear $(printf 'flyyy-%s ' ${parts}) flyyy-worker
    # The user's directories (an earlier root run's handed over too).
    own_dirs ${lib.escapeShellArgs dirs}
    secret ${corroApiToken}
    # Where the mesh part's join looks (removed once spent).
    [ -z "$token" ] || put ${mesh}/join-token "$token"
    # The user's systemd instance, without a login: the VMs are its units.
    # Without logind (some containers), what it would do: the linger file, and the user's systemd started.
    if ! as_root loginctl enable-linger "$user" 2> /dev/null; then
      as_root install -D -m 644 /dev/null "/var/lib/systemd/linger/$user"
      as_root systemctl start "user@$uid.service"
    fi
    for _ in $(seq 30); do [ -S "/run/user/$uid/systemd/private" ] && break; sleep 1; done
    [ -S "/run/user/$uid/systemd/private" ] || die "$user's systemd instance (user@$uid) didn't start after loginctl enable-linger"
    # The mesh (as the module's trustedInterfaces does) and WireGuard.
    firewall "iface flyyy0" "udp 51820"
    gcroot worker ${run}

    # The units: one per part, as $user, restarted whenever they stop.
    run_dirs "0750 /run/flyyy" "0700 /run/corrosion"
    for p in ${parts}; do
      case $p in
        mesh) props=(AmbientCapabilities=CAP_NET_ADMIN CapabilityBoundingSet=CAP_NET_ADMIN) ;;
        agent) deps=("Wants=user@$uid.service" "After=user@$uid.service") ;;
      esac
      unit "flyyy-$p" "flyyy worker $name: $p (flyyy-join)" ${run}/bin/flyyy-worker "$p" "$name" "$hostbin" "$endpoint"
    done
    units_up

    # Up to a minute for the control plane to answer: the join's log says when it did.
    wait_log flyyy-mesh "waiting for .flyyy approve.|on the mesh" 60 ||
      echo "flyyy-join: no answer from the control plane at $control yet; its log: sudo journalctl -u flyyy-mesh -f" >&2
    if [ -n "$approved" ]; then
      echo "✓ $name running again (approved before)"
    elif [ -n "$state" ]; then
      echo "✓ $name running again"
      echo "next: approve $name in the dashboard (or flyyy approve $name)"
    else
      echo "✓ $name joined through $control"
      echo "next: approve $name in the dashboard (or flyyy approve $name)"
    fi
  '';
}
