# What the rootless `nix run` apps share (join.nix: #join; host-apps.nix: #control, #edge, #dashboard):
# shell functions for the setup they do with sudo, and the Corrosion part they run. Each app runs as the
# user who ran it (as root: --user, or sudo's caller; never root); its parts are system units with User=
# that user, in a slice of its own; sudo is for the setup only.
{ pkgs, lib, meshTls, corrosionStart, ... }:
{
  # Bash, with $app set to the app's name. After `who`: $user, $uid, $group, and as_root (sudo when not root).
  prelude = ''
    die() { echo "$app: $*" >&2; exit 1; }
    as_root() { "''${sudo[@]}" "$@"; }
    # Who the parts run as: the caller, or with root, --user ($1) or sudo's caller. Never root.
    who() {
      sudo=()
      if [ "$(id -u)" = 0 ]; then
        user=''${1:-''${SUDO_USER:-}}
        [ -n "$user" ] || die "run as root: say which user it runs as, --user NAME (nothing of it runs as root)"
      else
        [ -z "$1" ] || [ "$1" = "$(id -un)" ] || die "--user is for running it as root; as $(id -un), it runs as $(id -un)"
        user=$(id -un)
        command -v sudo > /dev/null || die "no sudo: the setup (units, firewall, directories) needs it"
        sudo=(sudo)
      fi
      uid=$(id -u "$user") || die "no user $user"
      [ "$uid" != 0 ] || die "nothing of it runs as root ($user is uid 0): --user NAME"
      group=$(id -gn "$user")
    }
    # The architecture and systemd; $hostbin: the running systemd's own bin directory.
    check_host() {
      case $(uname -m) in
        aarch64|x86_64) ;;
        *) die "unsupported architecture $(uname -m): flyyy builds for aarch64-linux and x86_64-linux" ;;
      esac
      [ -d /run/systemd/system ] || die "systemd isn't running here: the parts are systemd units"
      local systemctl
      systemctl=$(command -v systemctl) || die "no systemctl in PATH"
      hostbin=$(dirname "$(readlink -f "$systemctl")")
      [ -x "$hostbin/systemd-run" ] || die "no systemd-run next to $systemctl"
    }
    # The kernel's WireGuard: creating an interface loads the module if there is one.
    check_wireguard() {
      as_root ip link del flyyy-wgcheck 2> /dev/null || true
      local err
      err=$(as_root ip link add flyyy-wgcheck type wireguard 2>&1) ||
        die "this kernel has no WireGuard (CONFIG_WIREGUARD; on NixOS it's a module, normally there): ip link add type wireguard: $err"
      as_root ip link del flyyy-wgcheck
    }
    # A file the user must be able to read (a certificate, its key).
    check_readable() {
      as_root setpriv --reuid="$uid" --regid="$(id -g "$user")" --init-groups test -r "$1" ||
        die "$user can't read $1 (it runs as $user)"
    }
    # The user's directories, "mode path" each (an earlier run's, or root's, handed over).
    own_dirs() {
      local d
      for d in "$@"; do
        as_root install -d -m "''${d%% *}" -o "$user" -g "$group" "''${d#* }"
        as_root chown -R "$user:$group" "''${d#* }"
      done
    }
    # A random secret in $1 (0600, the user's), unless there is one.
    secret() {
      [ -s "$1" ] && return 0
      od -An -N32 -tx1 /dev/urandom | tr -d ' \n' | as_root install -m 600 -o "$user" -g "$group" /dev/stdin "$1"
    }
    # $2 into $1 (0600, the user's). printf is a builtin: a token isn't on sudo's command line.
    put() { printf %s "$2" | as_root install -m 600 -o "$user" -g "$group" /dev/stdin "$1"; }
    # The /run directories again after a reboot (systemd-tmpfiles), off NixOS: "mode path" each.
    run_dirs() {
      [ -e /etc/NIXOS ] && return 0
      local d
      for d in "$@"; do printf 'd %s %s %s %s -\n' "''${d#* }" "''${d%% *}" "$user" "$group"; done |
        as_root install -D -m 644 /dev/stdin "/etc/tmpfiles.d/$slice.conf"
    }
    # The host's firewall, if it's NixOS's: let in each of "iface NAME", "udp PORT", "tcp PORT". Other
    # firewalls are the operator's. A firewall reload drops these; running the app again puts them back.
    firewall() {
      [ $# -gt 0 ] || return 0
      local ipt=${pkgs.iptables}/bin/iptables nft=${pkgs.nftables}/bin/nft r m=() rule have
      if as_root "$ipt" -n -L nixos-fw > /dev/null 2>&1; then
        for r in "$@"; do
          case $r in iface\ *) m=(-i "''${r#* }") ;; *) m=(-p "''${r%% *}" --dport "''${r#* }") ;; esac
          as_root "$ipt" -C nixos-fw "''${m[@]}" -j nixos-fw-accept 2> /dev/null || as_root "$ipt" -I nixos-fw 1 "''${m[@]}" -j nixos-fw-accept
        done
      elif have=$(as_root "$nft" list chain inet nixos-fw input-allow 2> /dev/null); then
        for r in "$@"; do
          # As nft lists it.
          case $r in iface\ *) rule="iifname \"''${r#* }\" accept" ;; *) rule="''${r%% *} dport ''${r#* } accept" ;; esac
          # shellcheck disable=SC2086
          grep -qF "$rule" <<< "$have" || as_root "$nft" insert rule inet nixos-fw input-allow ''${rule//\"/}
        done
      else
        echo "$app: if this machine has a firewall, let in: $*" >&2
      fi
    }
    # The units' script survives a garbage collection while it's in use.
    gcroot() {
      as_root mkdir -p /nix/var/nix/gcroots/flyyy
      as_root ln -sfn "$2" "/nix/var/nix/gcroots/flyyy/$1"
    }
    nixos=no
    [ -e /etc/NIXOS ] && nixos=yes
    started=() props=() deps=()
    # Stop and remove units of an earlier run: those it may have made, whichever it makes now.
    units_clear() {
      local n
      for n in "$@"; do
        as_root systemctl stop "$n" 2> /dev/null || true
        as_root systemctl reset-failed "$n" 2> /dev/null || true
        if [ -e "/etc/systemd/system/$n.service" ]; then
          as_root systemctl disable "$n" 2> /dev/null || true
          as_root rm -f "/etc/systemd/system/$n.service"
        fi
      done
    }
    # `unit NAME DESCRIPTION COMMAND...`: a part, as $user in $slice, restarted whenever it stops; $props
    # (service settings, e.g. capabilities) and $deps (unit settings) for this one only. On NixOS a
    # transient unit (/etc/systemd/system is the system's own; gone at a reboot: run the app again),
    # elsewhere a real one, enabled by units_up.
    # systemd's quoting for a unit file's ExecStart: each word in double quotes, % and $ doubled.
    sd_quote() {
      local a
      for a in "$@"; do a=''${a//\\/\\\\}; a=''${a//\"/\\\"}; a=''${a//%/%%}; printf '"%s" ' "''${a//\$/\$\$}"; done
    }
    unit() {
      local name=$1 desc=$2 x p=()
      shift 2
      # systemd expands $VAR in a command even from systemd-run: refused rather than quoted.
      for x in "$@"; do [[ $x != *[$'\n$']* ]] || die "$name: an argument with \$ or a newline: $x"; done
      local service=(User="$user" Group="$group" Restart=always RestartSec=2 UMask=0077 NoNewPrivileges=yes "''${props[@]}")
      if [ $nixos = yes ]; then
        for x in "''${deps[@]}" "''${service[@]}"; do p+=(-p "$x"); done
        as_root systemd-run --quiet --unit "$name" --slice "$slice" --description "$desc" "''${p[@]}" "$@" > /dev/null
      else
        printf '%s\n' "[Unit]" "Description=$desc" "''${deps[@]}" "Wants=network-online.target" "After=network-online.target" \
          "[Service]" "ExecStart=$(sd_quote "$@")" "Slice=$slice.slice" "''${service[@]}" "[Install]" "WantedBy=multi-user.target" |
          as_root install -D -m 644 /dev/stdin "/etc/systemd/system/$name.service"
      fi
      started+=("$name")
      props=() deps=()
    }
    units_up() {
      [ $nixos = yes ] && return 0
      as_root systemctl daemon-reload
      as_root systemctl enable --quiet --now "''${started[@]}"
    }
    # Up to $3 s for unit $1's log (since $since) to match $2; 1 when it didn't, and dies when it stopped.
    since=$(date +%s)
    wait_log() {
      local i
      for i in $(seq "$3"); do
        as_root journalctl -u "$1" --since "@$since" -o cat --no-pager 2> /dev/null | grep -Eq "$2" && return 0
        as_root systemctl is-active -q "$1" || [ "$i" -lt 5 ] ||
          { as_root journalctl -u "$1" --since "@$since" -o cat --no-pager | tail -20 >&2; die "$1 stopped (above: its log)"; }
        sleep 1
      done
      return 1
    }
    # Whether this is the control plane's host: the #control app's (or the module's) state.
    control_host() { [ -d /var/lib/flyyy-control ]; }
    # A node's name: 1-32 of a-z, 0-9 and -, starting with a letter (wg::node_name_ok). Its default, from the
    # hostname: lowercased, anything else a -, cut to 32, a leading digit prefixed with n (wg::default_node_name).
    name_ok() { [[ $1 =~ ^[a-z][a-z0-9-]{0,31}$ ]] || die "--name $1: want 1-32 of a-z, 0-9 and -, starting with a letter (unique in the cluster)"; }
    default_name() {
      local h
      h=$(< /proc/sys/kernel/hostname)
      h=''${h,,} h=''${h//[^a-z0-9-]/-} h=''${h:0:32}
      [[ $h =~ ^[0-9] ]] && h=n''${h:0:31}
      echo "$h"
    }
    # The role this host joined the mesh as, if it did.
    joined_as() { as_root cat /var/lib/flyyy-mesh/state.json 2> /dev/null | jq -r '.role // empty' || true; }
  '';
  # The Corrosion part, as its unit runs it: it reads its certificate at start only (the module restarts
  # it on renewal), so a renewed one ends this, and systemd starts it again. `name`: its config's (control,
  # or a node's role); gossip on 8787, the API on 127.0.0.1:8080, state in /var/lib/corrosion.
  corrosionPart = name: ''
    until [ -s ${meshTls}/cert.pem ]; do sleep 1; done
    first=$(stat -c %Y ${meshTls}/cert.pem)
    ${corrosionStart "corrosion" name 8787 "127.0.0.1:8080"} &
    pid=$!
    while kill -0 "$pid" 2> /dev/null; do
      sleep 10
      [ "$(stat -c %Y ${meshTls}/cert.pem)" = "$first" ] || { echo "mesh certificate renewed, restarting"; kill "$pid"; wait "$pid" || true; exit 1; }
    done
    wait "$pid"
  '';
  # The directories every mesh node's parts use, "mode path".
  meshDirs = [ "0700 /var/lib/flyyy-mesh" "0750 ${meshTls}" ];
  corrosionDirs = [ "0700 /var/lib/corrosion" "0700 /var/lib/flyyy-corroapi" "0700 /run/corrosion" ];
  runtimeInputs = [ pkgs.coreutils pkgs.iproute2 pkgs.jq pkgs.gnugrep pkgs.util-linux ];
}
