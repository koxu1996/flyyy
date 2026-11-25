# `nix run <flake>#control`, `#edge` and `#dashboard`: the control host's roles without flyyy's NixOS
# modules, rootless as #join is (host.nix): each part a system unit with User= the caller (as root: --user,
# or sudo's caller; never root) in a slice of its own, sudo for the setup only, NixOS's firewall opened, a
# re-run resumes from /var/lib/flyyy*.
#   #control   the control plane (CAP_NET_ADMIN, the mesh hub) and its Corrosion; the mesh made at the first
#              start (`proxy control --create-mesh`), with no join token: the dashboard's + Add node makes
#              them (or `flyyy token`). Prints the operator token's path.
#   #edge      a mesh node of role edge, with its own Corrosion. On the control plane's host (one flyyy0 per
#              host) it doesn't join: it has the control plane's mesh address, its certificate from the
#              control plane over loopback for the join token (`flyyy local-cert`), and reads the control
#              plane's Corrosion.
#   #dashboard the public dashboard. On the control plane's host it calls the local API: no mesh, no token.
#              Elsewhere it joins as a mesh node of role dashboard.
# Each prints a progress line, then what's up in a few lines (✓, the facts, `next:`); --nixos prints its
# NixOS-module setup (and the steps to hand its state over) instead, and exits.
args@{ pkgs, lib, proxy, meshIp, meshTls, corroApiToken, flakeUrl, ... }:
let
  host = import ./host.nix args;
  operatorToken = "/var/lib/flyyy-operator/token";
  # The edge's own state: on the control plane's host its join token (its credential) and name, and its
  # mesh certificate (not the control plane's, in meshTls; not /var/lib/flyyy-edge-tls either, where
  # nix/examples/control-host.nix keeps the edge's public certificate).
  edgeState = "/var/lib/flyyy-edge";
  edgeTls = "${edgeState}/tls";
  # One part, as its unit runs it (as the user): `flyyy-host <part> [args]`.
  run = pkgs.writeShellApplication {
    name = "flyyy-host";
    runtimeInputs = [ proxy pkgs.wireguard-tools pkgs.iproute2 pkgs.coreutils ];
    text = ''
      part=$1
      shift
      case $part in
        control)
          exec proxy control --listen 127.0.0.1:4000 --corrosion 127.0.0.1:8080 --corrosion-token-file ${corroApiToken} --token-file ${operatorToken} \
            --mesh-dir /var/lib/flyyy-control --tls-dir ${meshTls} --endpoint "$1" --create-mesh
          ;;
        corrosion-control)
          ${host.corrosionPart "control"}
          ;;
        corrosion-edge)
          ${host.corrosionPart "edge"}
          ;;
        # `mesh <role> <name> [endpoint]`: this node's side of the mesh, as #join's.
        mesh)
          token=()
          [ -s /var/lib/flyyy-mesh/join-token ] && token=(--token-file /var/lib/flyyy-mesh/join-token)
          ep=()
          [ -n "''${3:-}" ] && ep=(--endpoint "$3")
          exec flyyy join "''${token[@]}" "''${ep[@]}" --name "$2" --role "$1" --state-dir /var/lib/flyyy-mesh --tls-dir ${meshTls}
          ;;
        local-cert)
          exec flyyy local-cert --token-file ${edgeState}/token --name "$1" --tls-dir ${edgeTls}
          ;;
        # `edge <tls dir> <proxy edge flags>`: once it has its certificate, its load counters on its mesh
        # address (the control plane's, on its host).
        edge)
          tls=$1
          shift
          until [ -s "$tls/cert.pem" ]; do sleep 1; done
          ip=$(${meshIp})
          exec proxy edge --state 127.0.0.1:9101 --corrosion 127.0.0.1:8080 --corrosion-token-file ${corroApiToken} --tls-dir "$tls" --load "$ip:9201" "$@"
          ;;
        anyip) exec proxy anyip --corrosion 127.0.0.1:8080 --corrosion-token-file ${corroApiToken} ;;
        dashboard) exec proxy dashboard "$@" ;;
        *) echo "flyyy-host: unknown part $part" >&2; exit 2 ;;
      esac
    '';
  };
  cli = "FLYYY_API=http://127.0.0.1:4000 FLYYY_TOKEN_FILE=${operatorToken} ${proxy}/bin/flyyy";
  # The flags every app takes, then its own: `parse "<its flags, |-separated>" "$@"` sets each (dashes as
  # underscores) and $token, the one argument. A flag with a ! takes no value: it sets flag_<name>=1.
  parse = ''
    parse() {
      local keys=$1
      shift
      token="" flag_nixos=0
      while [ $# -gt 0 ]; do
        case $1 in
          -h|--help) usage; exit 0 ;;
          --*)
            local key=''${1#--}
            key=''${key//-/_}
            if [[ "|nixos!|$keys|" == *"|$key!|"* ]]; then
              printf -v "flag_$key" 1
              shift
              continue
            fi
            [ $# -ge 2 ] || { usage >&2; exit 2; }
            [[ "|user|$keys|" == *"|$key|"* ]] || { echo "unknown flag $1" >&2; usage >&2; exit 2; }
            printf -v "$key" '%s' "$2"
            shift 2 ;;
          *) [ -z "$token" ] || { usage >&2; exit 2; }; token=$1; shift ;;
        esac
      done
    }
    # host:port's port, and whether the host is loopback.
    port_of() { echo "''${1##*:}"; }
    loopback() { [[ $1 == 127.* || $1 == \[::1\]:* || $1 == localhost:* ]]; }
    addr_ok() { [[ $1 =~ ^([0-9.]+|\[[0-9A-Fa-f:]+\]):[0-9]{1,5}$ ]] || die "$2 $1: want <IPv4>:<port> or [<IPv6>]:<port>"; }
    # A first join's token, which must carry the control plane's address: that address.
    token_control() {
      [[ $1 =~ ^flyyy-join:[^@]+@(.+)$ ]] ||
        die "that token carries no control plane address: make one on a control plane with an endpoint (#control --endpoint, or the module's control.endpoint)"
      echo "''${BASH_REMATCH[1]}"
    }
    # A certificate or key file, as an absolute path the user can read.
    pem() {
      [ -n "$2" ] || die "$1: needed"
      local f
      f=$(realpath -e "$2") || die "$1 $2: no such file"
      check_readable "$f"
      echo "$f"
    }
    # CAP_NET_BIND_SERVICE for a port below 1024.
    bind_props() {
      local a
      for a in "$@"; do
        [ "$(port_of "$a")" -lt 1024 ] && { props+=(AmbientCapabilities=CAP_NET_BIND_SERVICE CapabilityBoundingSet=CAP_NET_BIND_SERVICE); return; }
      done
      return 0
    }
    # The mesh state of a node that joined (#edge or #dashboard elsewhere): a resume, or a first join.
    # Sets $name, $control, $approved and $progress; refuses another role's join or another user's.
    # shellcheck disable=SC2034  # set for the caller (#control has none)
    mesh_state() {
      local role=$1 state joined owner
      state=$(as_root cat /var/lib/flyyy-mesh/state.json 2> /dev/null || true)
      if [ -n "$state" ]; then
        joined=$(jq -r .name <<< "$state")
        [ "$(jq -r .role <<< "$state")" = "$role" ] || die "this machine joined the mesh as $joined, role $(jq -r .role <<< "$state"), not as a $role"
        owner=$(stat -c %U /var/lib/flyyy-mesh)
        [ "$owner" = "$user" ] || die "$joined joined as $owner: run it as $owner"
        [ -z "$token" ] || die "this machine already joined as $joined (/var/lib/flyyy-mesh/state.json): run it again without a token to resume"
        [ -z "$name" ] || [ "$name" = "$joined" ] || die "this machine joined as $joined, not $name"
        name=$joined control=$(jq -r .control <<< "$state") approved=$(jq -r '.ip // empty' <<< "$state")
        progress="resuming $name"
      else
        [ -n "$token" ] || die "a first join needs a token (dashboard: + Add node; CLI: flyyy token)"
        [ -n "$name" ] || name=$(default_name)
        name_ok "$name"
        control=$(token_control "$token")
        approved=
        progress="joining as $name ($role) through $control"
      fi
    }
    # Up to a minute for the control plane to answer the join (a warning when it doesn't).
    joined() {
      wait_log "$1" "waiting for .flyyy approve.|on the mesh" 60 ||
        echo "$app: no answer from the control plane at $control yet; its log: sudo journalctl -u $1 -f" >&2
    }
    approve_next() { echo "next: approve $name in the dashboard (or flyyy approve $name)"; }
  '';
  app = name: usage: text: pkgs.writeShellApplication {
    name = "flyyy-${name}";
    inherit (host) runtimeInputs;
    text = ''
      app=flyyy-${name} slice=flyyy-${name} user=
      ${host.prelude}
      ${parse}
      usage() {
        cat <<EOF
      ${usage}
        --user      run as root: the user it runs as (default \$SUDO_USER); nothing of it runs as root
        --nixos     print the NixOS-module setup that makes it permanent (and how to hand its state over), and exit
      EOF
      }
      ${text}
    '';
  };
in
{
  controlHostApp = app "control" ''
    usage: flyyy-control [--endpoint HOST[:PORT]] [--hide-token] [--user USER] [--nixos]
      The control plane, run by you: flyyy-control (the API, the mesh hub and its CA; CAP_NET_ADMIN) and
      flyyy-control-corrosion (flyyy-control.slice). The first start creates the mesh; nodes are added with
      the dashboard's + Add node (or a token from flyyy token).
      --endpoint  where the nodes' WireGuard reaches this host (port 51820 unless given); every join token
                  carries it. A first start needs it; later ones keep it unless given again.
      --hide-token  print the operator token's path, not the token'' ''
    endpoint="" flag_hide_token=0
    parse "endpoint|hide_token!" "$@"
    [ -z "$token" ] || { usage >&2; exit 2; }
    who "$user"
    check_host
    check_wireguard
    role=$(joined_as)
    [ -z "$role" ] || die "this machine is a mesh node ($role, /var/lib/flyyy-mesh): one flyyy0 per host, so no control plane here"
    if [ -d /var/lib/flyyy-control ]; then
      owner=$(stat -c %U /var/lib/flyyy-control)
      [ "$owner" = "$user" ] || die "the control plane here is $owner's (/var/lib/flyyy-control: the module's, or another user's run)"
    fi
    endpoint=''${endpoint:-$(as_root cat /var/lib/flyyy-control/endpoint 2> /dev/null || true)}
    [ -n "$endpoint" ] || die "a first start needs --endpoint HOST: where the nodes' WireGuard reaches this host"
    [[ $endpoint =~ ^([A-Za-z0-9.-]+|\[[0-9A-Fa-f:]+\])(:[0-9]{1,5})?$ ]] || die "--endpoint $endpoint: want <host>[:<port>], an IPv4 address, [<IPv6>] or a name"
    if [ "$flag_nixos" = 1 ]; then
      cat <<EOF
    To make the control plane permanent with flyyy's NixOS module (it keeps the mesh, its CA and the desired state):

      # flake.nix: inputs.flyyy.url = "${flakeUrl}";
      imports = [ inputs.flyyy.nixosModules.control ];
      services.flyyy.control = { enable = true; endpoint = "$endpoint"; };

    Then: sudo systemctl stop flyyy-control.slice, nixos-rebuild switch, and hand the state to the module's users:
    sudo chown -R control:control /var/lib/flyyy-control; sudo chown -R control:meshtls ${meshTls};
    sudo chown -R root:root /var/lib/flyyy-operator; sudo rm -rf /var/lib/corrosion /var/lib/flyyy-corroapi;
    sudo systemctl restart corroapi-token operator-token control corrosion
    EOF
      exit 0
    fi

    # Whether this start makes the mesh (asked before the units run: the control plane makes it at start).
    made=no
    as_root test -e /var/lib/flyyy-control/mesh.key || made=yes
    units_clear flyyy-control flyyy-control-corrosion
    own_dirs "0700 /var/lib/flyyy-control" "0750 ${meshTls}" "0700 /var/lib/flyyy-operator" ${lib.escapeShellArgs host.corrosionDirs}
    secret ${corroApiToken}
    secret ${operatorToken}
    put /var/lib/flyyy-control/endpoint "$endpoint"
    firewall "iface flyyy0" "udp 51820"
    gcroot control ${run}
    run_dirs "0700 /run/corrosion"
    props=(AmbientCapabilities=CAP_NET_ADMIN CapabilityBoundingSet=CAP_NET_ADMIN)
    unit flyyy-control "flyyy control plane (flyyy-control)" ${run}/bin/flyyy-host control "$endpoint"
    unit flyyy-control-corrosion "flyyy control plane: corrosion (flyyy-control)" ${run}/bin/flyyy-host corrosion-control
    units_up

    # The control plane makes the mesh at its first start: its API, once it answers with one.
    out=
    for _ in $(seq 60); do
      out=$(${cli} mesh --json 2>&1) && break
      sleep 1
    done
    if ! jq -e .control <<< "$out" > /dev/null 2>&1; then
      as_root journalctl -u flyyy-control --since "@$since" -o cat --no-pager | tail -20 >&2
      die "the control plane's API on 127.0.0.1:4000 didn't answer with its mesh (above: its log): $out"
    fi
    # Two lines: it's up, and the operator token (what signs in to the dashboard and the CLI). The token is
    # the caller's own file, so it's printed on every run; --hide-token prints its path instead.
    if [ $made = yes ]; then up="up"; else up="running again"; fi
    echo "✓ control plane $up (endpoint $endpoint)"
    if [ "$flag_hide_token" = 0 ]; then
      echo "operator token: $(as_root cat ${operatorToken})"
    else
      echo "operator token: in ${operatorToken}"
    fi
  '';

  edgeHostApp = app "edge" ''
    usage: flyyy-edge --listen ADDR [--tls-cert PEM --tls-key PEM [--http ADDR]] [--apps-domain D] [--name NAME]
                      [--endpoint HOST:PORT] [--user USER] [--nixos] [TOKEN]
      An edge, run by you (flyyy-edge.slice): apps served by Host <app>.<apps-domain>. On the control plane's
      host it uses the control plane's mesh address and Corrosion (flyyy-edge-cert, flyyy-edge, flyyy-edge-anyip);
      elsewhere it joins the mesh with its own (flyyy-edge-mesh, -corrosion, flyyy-edge, flyyy-edge-anyip).
      TOKEN         a join token (dashboard: + Add node; CLI: flyyy token): a first start needs one; without,
                    it resumes
      --listen      where apps are served: HTTPS with --tls-cert and --tls-key (already issued; reload:
                    systemctl kill -s HUP flyyy-edge), else plain HTTP
      --http        with HTTPS, the plain HTTP address, which only redirects (default 127.0.0.1:8000)
      --apps-domain the apps' domain (default flyyy.localhost)
      --name        its name, unique in the cluster: a-z, 0-9 and -, starting with a letter (default: the hostname)
      --endpoint    joining behind NAT: the WireGuard endpoint the other nodes should use for this one'' ''
    listen="" http="" tls_cert="" tls_key="" apps_domain=flyyy.localhost name="" endpoint=""
    parse "listen|http|tls_cert|tls_key|apps_domain|name|endpoint" "$@"
    who "$user"
    check_host
    [ -n "$listen" ] || die "--listen ADDR: where apps are served"
    addr_ok "$listen" --listen
    [[ $apps_domain =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ ]] || die "--apps-domain $apps_domain: want lowercase DNS labels"
    serve=(--apps-domain "$apps_domain")
    if [ -n "$tls_cert$tls_key" ]; then
      tls_cert=$(pem --tls-cert "$tls_cert")
      tls_key=$(pem --tls-key "$tls_key")
      http=''${http:-127.0.0.1:8000}
      addr_ok "$http" --http
      serve+=(--listen "$http" --https "$listen" --tls-cert "$tls_cert" --tls-key "$tls_key")
      ports=("$listen" "$http")
    else
      [ -z "$http" ] || die "--http is for HTTPS (--tls-cert, --tls-key): without, --listen is plain HTTP"
      serve+=(--listen "$listen")
      ports=("$listen")
    fi
    open=()
    for a in "''${ports[@]}"; do loopback "$a" || open+=("tcp $(port_of "$a")"); done

    # Where it is and who it is, before anything changes.
    here=no
    if control_host; then
      here=yes
      owner=$(stat -c %U /var/lib/flyyy-control)
      [ "$owner" = "$user" ] || die "the control plane here is $owner's: an edge on its host reads its Corrosion, so it runs as $owner"
      [ -z "$endpoint" ] || die "--endpoint: on the control plane's host the edge has no WireGuard of its own"
      if as_root test -s ${edgeState}/name; then
        was=$(as_root cat ${edgeState}/name)
        [ -z "$token" ] || die "this host's edge is $was already: run it again without a token to resume (to start over: sudo systemctl stop flyyy-edge.slice; sudo rm -rf ${edgeState})"
        [ -z "$name" ] || [ "$name" = "$was" ] || die "this host's edge is $was, not $name"
        name=$was
        progress="resuming $name, on the control plane's host"
      else
        [ -n "$token" ] || die "a first start needs a join token (dashboard: + Add node; CLI: flyyy token)"
        [[ $token =~ ^flyyy-join: ]] || die "that isn't a join token"
        [ -n "$name" ] || name=$(default_name)
        name_ok "$name"
        progress="$name on the control plane's host (its mesh address and Corrosion)"
      fi
    else
      check_wireguard
      mesh_state edge
    fi
    if [ "$flag_nixos" = 1 ]; then
      if [ $here = yes ]; then
        cat <<EOF
    With flyyy's NixOS modules an edge doesn't share the control plane's host: it runs in a NixOS container of
    its own there, a mesh node (nix/examples/control-host.nix in ${flakeUrl}: containers.flyyy-edge).
    EOF
      else
        https=
        [ -z "$tls_cert" ] || https=" https = { listen = \"$listen\"; cert = \"$tls_cert\"; key = \"$tls_key\"; };"
        cat <<EOF
    To make $name permanent with flyyy's NixOS module (it keeps this node's mesh identity):

      # flake.nix: inputs.flyyy.url = "${flakeUrl}";
      imports = [ inputs.flyyy.nixosModules.edge ];
      services.flyyy.controlAddress = "$control";
      services.flyyy.edge = { enable = true; nodeName = "$name"; listen = "''${http:-$listen}";$https appsDomain = "$apps_domain"; };

    Then: sudo systemctl stop flyyy-edge.slice, nixos-rebuild switch, and hand this node's state to the module's
    users: sudo rm -rf /var/lib/corrosion /var/lib/flyyy-corroapi; sudo chown -R mesh:mesh /var/lib/flyyy-mesh;
    sudo chown -R mesh:meshtls ${meshTls}; sudo systemctl restart corroapi-token mesh
    EOF
      fi
      exit 0
    fi
    echo "$app: $progress"

    units_clear flyyy-edge flyyy-edge-cert flyyy-edge-mesh flyyy-edge-corrosion flyyy-edge-anyip
    gcroot edge ${run}
    if [ $here = yes ]; then
      own_dirs "0700 ${edgeState}" "0700 ${edgeTls}"
      # The token stays: spent at the first certificate, it's what renews it.
      [ -z "$token" ] || { put ${edgeState}/token "$token"; put ${edgeState}/name "$name"; }
      firewall "''${open[@]}"
      unit flyyy-edge-cert "flyyy edge $name: its certificate (flyyy-edge)" ${run}/bin/flyyy-host local-cert "$name"
      bind_props "''${ports[@]}"
      unit flyyy-edge "flyyy edge $name (flyyy-edge)" ${run}/bin/flyyy-host edge ${edgeTls} --name "$name" "''${serve[@]}"
    else
      own_dirs ${lib.escapeShellArgs (host.meshDirs ++ host.corrosionDirs)}
      secret ${corroApiToken}
      [ -z "$token" ] || put /var/lib/flyyy-mesh/join-token "$token"
      firewall "iface flyyy0" "udp 51820" "''${open[@]}"
      run_dirs "0700 /run/corrosion"
      props=(AmbientCapabilities=CAP_NET_ADMIN CapabilityBoundingSet=CAP_NET_ADMIN)
      unit flyyy-edge-mesh "flyyy edge $name: mesh (flyyy-edge)" ${run}/bin/flyyy-host mesh edge "$name" "$endpoint"
      unit flyyy-edge-corrosion "flyyy edge $name: corrosion (flyyy-edge)" ${run}/bin/flyyy-host corrosion-edge
      bind_props "''${ports[@]}"
      unit flyyy-edge "flyyy edge $name (flyyy-edge)" ${run}/bin/flyyy-host edge ${meshTls} --name "$name" "''${serve[@]}"
    fi
    props=(AmbientCapabilities=CAP_NET_ADMIN CapabilityBoundingSet=CAP_NET_ADMIN)
    unit flyyy-edge-anyip "flyyy edge $name: anyip (flyyy-edge)" ${run}/bin/flyyy-host anyip
    units_up

    if [ $here = yes ]; then
      # Its certificate, for the token (a refused one stops flyyy-edge-cert, and its log says why).
      for i in $(seq 60); do
        as_root test -s ${edgeTls}/cert.pem && break
        as_root systemctl is-active -q flyyy-edge-cert || [ "$i" -lt 5 ] ||
          { as_root journalctl -u flyyy-edge-cert --since "@$since" -o cat --no-pager | tail -20 >&2; die "flyyy-edge-cert stopped (above: its log)"; }
        sleep 1
      done
      as_root test -s ${edgeTls}/cert.pem || echo "flyyy-edge: no certificate yet; its log: sudo journalctl -u flyyy-edge-cert -f" >&2
      echo "✓ $name serves apps on $listen"
      echo "  apps at <app>.$apps_domain"
      echo "next: deploy one (dashboard: + New app)"
    else
      joined flyyy-edge-mesh
      if [ -n "$approved" ]; then echo "✓ $name running again (approved before), serves apps on $listen"; else echo "✓ $name joined through $control, serves apps on $listen once approved"; fi
      echo "  apps at <app>.$apps_domain"
      if [ -n "$approved" ]; then echo "next: deploy one (dashboard: + New app)"; else approve_next; fi
    fi
    echo "permanent setup: add --nixos"
  '';

  dashboardHostApp = app "dashboard" ''
    usage: flyyy-dashboard --host NAME --tls-cert PEM --tls-key PEM [--listen ADDR] [--apps-url URL] [--name NAME]
                           [--user USER] [--nixos] [TOKEN]
      The public dashboard, run by you (flyyy-dashboard.slice): the page over HTTPS, its calls to the control
      plane with each caller's own token. On the control plane's host it calls the local API (no mesh, no
      token); elsewhere it joins the mesh as a node of role dashboard (flyyy-dashboard-mesh).
      TOKEN       elsewhere, a join token: a first start needs one; without, it resumes
      --host      its public name: the only Host it answers
      --tls-cert  the certificate chain for it, and --tls-key its key (reload: systemctl kill -s HUP flyyy-dashboard)
      --listen    where it serves HTTPS (default 0.0.0.0:8443)
      --apps-url  where its app links go, {app} the app's name (default https://{app}.<host>/)
      --name      elsewhere: its mesh node's name (default: the hostname)'' ''
    host="" tls_cert="" tls_key="" listen=0.0.0.0:8443 apps_url="" name=""
    parse "host|tls_cert|tls_key|listen|apps_url|name" "$@"
    who "$user"
    check_host
    [[ $host =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ ]] || die "--host '$host': want its public name, lowercase DNS labels"
    addr_ok "$listen" --listen
    tls_cert=$(pem --tls-cert "$tls_cert")
    tls_key=$(pem --tls-key "$tls_key")
    [ -n "$apps_url" ] || apps_url="https://{app}.$host/"
    serve=(--listen "$listen" --host "$host" --tls-cert "$tls_cert" --tls-key "$tls_key" --apps-url "$apps_url")
    open=()
    loopback "$listen" || open=("tcp $(port_of "$listen")")

    if control_host; then
      [ -z "$token" ] || die "on the control plane's host the dashboard calls the local API: it needs no join token (that one stays unused)"
      [ -z "$name" ] || die "--name: on the control plane's host the dashboard isn't a mesh node"
      if [ "$flag_nixos" = 1 ]; then
        cat <<EOF
    With flyyy's NixOS modules the dashboard doesn't share the control plane's host: it runs in a NixOS container
    of its own there, a mesh node (nix/examples/control-host.nix in ${flakeUrl}: containers.flyyy-dash).
    EOF
        exit 0
      fi
      echo "flyyy-dashboard: on the control plane's host (its API on 127.0.0.1:4000)"
      units_clear flyyy-dashboard flyyy-dashboard-mesh
      firewall "''${open[@]}"
      gcroot dashboard ${run}
      bind_props "$listen"
      unit flyyy-dashboard "flyyy dashboard https://$host/ (flyyy-dashboard)" ${run}/bin/flyyy-host dashboard "''${serve[@]}" --control 127.0.0.1:4000
      units_up
      wait_log flyyy-dashboard "public dashboard" 30 || echo "flyyy-dashboard: not serving yet; its log: sudo journalctl -u flyyy-dashboard -f" >&2
      echo "✓ dashboard on https://$host/ (listening on $listen)"
      echo "next: sign in with the operator token, then + Add node"
    else
      check_wireguard
      mesh_state dashboard
      if [ "$flag_nixos" = 1 ]; then
        cat <<EOF
    To make $name permanent with flyyy's NixOS module (it keeps this node's mesh identity):

      # flake.nix: inputs.flyyy.url = "${flakeUrl}";
      imports = [ inputs.flyyy.nixosModules.dashboard ];
      services.flyyy.controlAddress = "$control";
      services.flyyy.dashboard = { enable = true; nodeName = "$name"; host = "$host"; listen = "$listen";
        tlsCert = "$tls_cert"; tlsKey = "$tls_key"; appsUrl = "$apps_url"; };

    Then: sudo systemctl stop flyyy-dashboard.slice, nixos-rebuild switch, and hand this node's state to the
    module's: sudo chown -R mesh:mesh /var/lib/flyyy-mesh; sudo systemctl restart mesh
    (the certificate and key must be readable by the module's flyyy-dashboard user).
    EOF
        exit 0
      fi
      echo "$app: $progress"
      units_clear flyyy-dashboard flyyy-dashboard-mesh
      own_dirs ${lib.escapeShellArgs host.meshDirs}
      [ -z "$token" ] || put /var/lib/flyyy-mesh/join-token "$token"
      # The mesh, as the module's trustedInterfaces does.
      firewall "iface flyyy0" "''${open[@]}"
      gcroot dashboard ${run}
      props=(AmbientCapabilities=CAP_NET_ADMIN CapabilityBoundingSet=CAP_NET_ADMIN)
      unit flyyy-dashboard-mesh "flyyy dashboard $name: mesh (flyyy-dashboard)" ${run}/bin/flyyy-host mesh dashboard "$name"
      bind_props "$listen"
      unit flyyy-dashboard "flyyy dashboard https://$host/ (flyyy-dashboard)" ${run}/bin/flyyy-host dashboard "''${serve[@]}"
      units_up
      joined flyyy-dashboard-mesh
      if [ -n "$approved" ]; then
        echo "✓ dashboard on https://$host/ (listening on $listen), $name running again (approved before)"
        echo "next: sign in with the operator token, then + Add node"
      else
        echo "✓ dashboard on https://$host/ (listening on $listen), $name joined through $control"
        approve_next
      fi
    fi
  '';
}
