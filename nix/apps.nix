# `nix run .#local-control/.#local-worker/.#local-edge`: each role on one machine, as an ordinary user (single-host mode).
{ pkgs, lib, schema, corrosion, proxy, kernel, initrd, ... }:
let
  # `nix run .#local-control`, `.#local-worker`, `.#local-edge`: Corrosion plus that role as plain processes of the calling
  # user, no root, no NixOS. Every address is a flag; the defaults don't collide, so all three run on one
  # machine (and gossip with each other). State in ~/.local/state/flyyy/<name>; leaving stops them all.
  # The worker's VMs are units of the caller's systemd user instance (it needs one: a login session, or
  # `loginctl enable-linger`) and it needs /dev/kvm.
  # shortcut: processes in the foreground of one shell, no supervision or restarts (the NixOS modules
  # above have both).
  # keys: this role's own flags (as variable names), on top of the shared ones.
  roleApp = role: { defaults, keys, flags, extraInputs ? [ ], start }: pkgs.writeShellApplication {
    name = "flyyy-local-${role}";
    runtimeInputs = [ corrosion proxy pkgs.curl pkgs.coreutils ] ++ extraInputs;
    text = ''
      ${defaults}
      bootstrap=127.0.0.1:8787,127.0.0.1:8788,127.0.0.1:8789
      state=
      usage() {
        cat <<EOF
      usage: flyyy-local-${role} [--name NAME] [--gossip ADDR] [--api ADDR] [--bootstrap ADDR,...] [--state-dir DIR]${flags}
        --gossip     this node's Corrosion gossip address (default $gossip)
        --api        its Corrosion API address (default $api)
        --bootstrap  gossip addresses to join, comma-separated (default $bootstrap; its own is skipped)
        --state-dir  default ''${XDG_STATE_HOME:-~/.local/state}/flyyy/<name>
      EOF
      }
      while [ $# -gt 0 ]; do
        case $1 in
          -h|--help) usage; exit 0 ;;
          --*) [ $# -ge 2 ] || { usage >&2; exit 2; }
            key=''${1#--}; key=''${key//-/_}
            case $key in name|gossip|api|bootstrap|state_dir|${keys}) ;;
              *) echo "unknown flag $1" >&2; usage >&2; exit 2 ;;
            esac
            [ "$key" = state_dir ] && key=state
            printf -v "$key" '%s' "$2"; shift 2 ;;
          *) echo "unexpected argument $1" >&2; usage >&2; exit 2 ;;
        esac
      done
      state=''${state:-''${XDG_STATE_HOME:-$HOME/.local/state}/flyyy/$name}
      mkdir -p "$state" && chmod 700 "$state"
      seeds=
      IFS=, read -ra list <<< "$bootstrap"
      for b in "''${list[@]}"; do [ "$b" = "$gossip" ] || seeds+="\"$b\", "; done
      # The Corrosion API's bearer token, the caller's own (0600 in the 0700 state directory): every role
      # here sends it, so other local users can't read or write the stack's state.
      corro_token_file="$state/corrosion.token"
      if [ ! -s "$corro_token_file" ]; then
        (umask 077; od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > "$corro_token_file")
      fi
      corro_token=$(<"$corro_token_file")
      (umask 077; cat > "$state/corrosion.toml") <<EOF
      [db]
      path = "$state/state.db"
      schema_paths = ["${schema}"]
      [gossip]
      addr = "$gossip"
      bootstrap = [''${seeds%, }]
      plaintext = true
      [api]
      addr = "$api"
      authorization.bearer-token = "$corro_token"
      [admin]
      path = "$state/admin.sock"
      EOF

      pids=()
      trap 'kill "''${pids[@]}" 2>/dev/null || true; wait' EXIT
      trap 'exit 130' INT TERM
      corrosion -c "$state/corrosion.toml" agent > "$state/corrosion.log" 2>&1 &
      pids+=($!)
      # The header from stdin (a builtin's output), so the token is never on a command line.
      until printf 'header = "authorization: Bearer %s"\n' "$corro_token" | curl -K - -fs -o /dev/null -H 'content-type: application/json' -d '"SELECT 1"' "http://$api/v1/queries"; do
        kill -0 "''${pids[0]}" 2>/dev/null || { echo "corrosion exited, see $state/corrosion.log" >&2; exit 1; }
        sleep 0.2
      done
      echo "flyyy-local-${role} $name: corrosion on $api (gossip $gossip, log $state/corrosion.log), state in $state"
      ${start}
      # Any process exiting stops the rest.
      wait -n
    '';
  };
  controlApp = roleApp "control" {
    defaults = "name=control gossip=127.0.0.1:8787 api=127.0.0.1:8081 listen=127.0.0.1:4000 token_file='' shared_ip=127.0.0.1 ip_pool=127.0.1.0/24";
    keys = "listen|token_file|shared_ip|ip_pool";
    flags = " [--listen ADDR] [--token-file FILE] [--shared-ip IP] [--ip-pool PREFIX]\n  --listen     the control plane API (default $listen)\n  --token-file its bearer token, made if missing (default <state-dir>/control.token)\n  --shared-ip  where HTTP is served, by Host (default $shared_ip)\n  --ip-pool    each app's own address comes from here (default $ip_pool: inside lo's 127.0.0.0/8, so no AnyIP route is needed)";
    start = ''
      token_file=''${token_file:-$state/control.token}
      if [ ! -e "$token_file" ]; then
        (umask 077; od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > "$token_file")
      fi
      # No mesh unless `flyyy init` (which needs CAP_NET_ADMIN): one machine needs none.
      mkdir -p "$state/mesh"
      # --single-host: everything here talks over loopback (the production units never pass it).
      proxy control --listen "$listen" --corrosion "$api" --corrosion-token-file "$corro_token_file" --token-file "$token_file" --mesh-dir "$state/mesh" --single-host --shared-ip "$shared_ip" --ip-pool "$ip_pool" &
      pids+=($!)
      echo "flyyy-local-control: API on $listen; for the CLI: export FLYYY_API=http://$listen FLYYY_TOKEN_FILE=$token_file"
    '';
  };
  workerApp = roleApp "worker" {
    defaults = "name=worker1 gossip=127.0.0.1:8788 api=127.0.0.1:8082 backhaul=127.0.0.1:4280 backhaul_state=127.0.0.1:9080 agent_api=127.0.0.1:4281";
    keys = "backhaul|backhaul_state|agent_api";
    flags = " [--backhaul ADDR] [--backhaul-state ADDR] [--agent-api ADDR]\n  --backhaul       the backhaul the edges dial, announced in the workers table (default $backhaul)\n  --backhaul-state its /state (default $backhaul_state)\n  --agent-api      where the control plane sends this worker its machines (default $agent_api)";
    # systemctl and systemd-run are the host's: they must match its running systemd.
    extraInputs = [ pkgs.crane pkgs.erofs-utils pkgs.e2fsprogs pkgs.cloud-hypervisor ];
    start = ''
      [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "flyyy-local-worker: no read/write access to /dev/kvm" >&2; exit 1; }
      systemctl --user show-environment > /dev/null ||
        { echo "flyyy-local-worker: no systemd user instance (log in, or loginctl enable-linger $USER)" >&2; exit 1; }
      state_dir="$state/agent" run_dir="''${XDG_RUNTIME_DIR:?}/flyyy/$name"
      mkdir -p "$state_dir" "$run_dir"
      # --backhaul-state: where the agent's drain sees the edges' requests to a machine stop.
      proxy agent --corrosion "$api" --corrosion-token-file "$corro_token_file" --worker "$name" --kernel ${kernel}/kernel --initrd ${initrd} --backhaul "$backhaul" --api "$agent_api" \
        --backhaul-state "$backhaul_state" --state-dir "$state_dir" --run-dir "$run_dir" --single-host &
      pids+=($!)
      proxy worker --listen "$backhaul" --state "$backhaul_state" --corrosion "$api" --corrosion-token-file "$corro_token_file" --worker "$name" --run-dir "$run_dir" --single-host &
      pids+=($!)
      echo "flyyy-local-worker: backhaul on $backhaul; VMs are your user units: systemctl --user list-units 'fc-*'"
    '';
  };
  edgeApp = roleApp "edge" {
    defaults = "name=edge1 gossip=127.0.0.1:8789 api=127.0.0.1:8083 listen=127.0.0.1:8000 edge_state=127.0.0.1:9100 load=127.0.0.1:9200 apps_domain=flyyy.localhost https='' tls_cert='' tls_key=''";
    keys = "listen|edge_state|load|apps_domain|https|tls_cert|tls_key";
    flags = " [--listen ADDR] [--edge-state ADDR] [--load ADDR] [--apps-domain D] [--https ADDR --tls-cert PEM --tls-key PEM]\n  --listen      where apps are served, by Host <app>.<apps-domain> (default $listen); with --https it only redirects\n  --edge-state  its /state (default $edge_state)\n  --load        its load counters, for the control plane (default $load)\n  --apps-domain the apps' domain (default $apps_domain)\n  --https       HTTPS here with this (already-issued) certificate chain and key; kill -HUP reloads them";
    start = ''
      tls=()
      if [ -n "$https$tls_cert$tls_key" ]; then tls=(--https "$https" --tls-cert "$tls_cert" --tls-key "$tls_key"); fi
      proxy edge --listen "$listen" --state "$edge_state" --corrosion "$api" --corrosion-token-file "$corro_token_file" --name "$name" --apps-domain "$apps_domain" --load "$load" "''${tls[@]}" --single-host &
      pids+=($!)
      if [ -n "$https" ]; then
        echo "flyyy-local-edge: apps on https://<app>.$apps_domain:''${https##*:}/ (http://...:''${listen##*:} redirects); reload the certificate: kill -HUP $!"
      else
        echo "flyyy-local-edge: apps on http://<app>.$apps_domain:''${listen##*:}/"
      fi
    '';
  };
in
{ inherit controlApp workerApp edgeApp; }
