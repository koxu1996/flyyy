# Images the tests push into their local registry (the build sandbox has no network).
# whoami and echo are the pinned Docker Hub images up.sh used; the rest are built here, small and
# fully known: a busybox test app that reports what the guest launcher gave it, distinct "cold" images,
# and two bulk images big enough that a build can be caught half-way.
{ pkgs, lib }:
let
  # This system's image of a multi-arch one (arm64 or amd64), by its hash for that arch.
  arch = pkgs.go.GOARCH;
  pull = { hashes, ... }@args: pkgs.dockerTools.pullImage (removeAttrs args [ "hashes" ] // { os = "linux"; inherit arch; hash = hashes.${arch}; });
  bb = pkgs.pkgsStatic.busybox;
  applets = "sh httpd sleep cat id pwd printf env grep ls wget hostname kill tr seq test echo mkdir rmdir base64 date ip rm tcpsvd";
  root = name: extra: pkgs.runCommand "${name}-root" { } ''
    mkdir -p $out/bin $out/etc $out/app/cgi-bin
    cp ${bb}/bin/busybox $out/bin/busybox
    cp ${pkgs.pkgsStatic.attr.bin}/bin/getfattr ${pkgs.pkgsStatic.attr.bin}/bin/setfattr $out/bin/
    for a in ${applets}; do ln -s busybox $out/bin/$a; done
    printf 'root:x:0:0:root:/root:/bin/sh\napp:x:1000:1000:app:/home/app:/bin/sh\n' > $out/etc/passwd
    printf 'root:x:0:\napp:x:1000:\nextra:x:1001:app\n' > $out/etc/group
    ${extra}
  '';
  # The test app: records its exact argv/env/ids/cwd at start (a shell keeps "$@" exact), then serves
  # them (and live checks of /dev, /etc and /proc) from a CGI on :8080.
  testappRoot = root "testapp" ''
    cat > $out/app/serve.sh <<'EOF'
    printf '%s\0' "$@" > /tmp/argv
    env -0 > /tmp/env 2>/dev/null || env > /tmp/env
    id -u > /tmp/uid; id -g > /tmp/gid; id -G > /tmp/groups; pwd > /tmp/cwd
    # ?slow answers after $DELAY seconds (the balance suite's slow and fast instances).
    echo "''${DELAY:-0}" > /tmp/delay
    echo "testapp: up as $(id -u) in $(pwd)"
    # Reopening /dev/stdout by path (as log configs do) must reach the console, also as non-root.
    echo "testapp: written to /dev/stdout as $(id -u)" > /dev/stdout
    exec httpd -f -p 8080 -h /app
    EOF
    cat > $out/app/cgi-bin/info <<'EOF'
    #!/bin/sh
    # ?fail: a 500 from the app itself (the metrics suite): a whole status line, which httpd passes on as it is.
    if [ "$QUERY_STRING" = fail ]; then printf 'HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\n\r\nfailed\n'; exit 0; fi
    echo "Content-Type: text/plain"; echo
    case "$QUERY_STRING" in
      argv) base64 -w0 /tmp/argv ;;
      env) base64 -w0 /tmp/env ;;
      ids) echo "uid=$(cat /tmp/uid) gid=$(cat /tmp/gid) groups=$(cat /tmp/groups) cwd=$(cat /tmp/cwd)" ;;
      dev)
        for p in /dev/fd/0 /dev/stdout /dev/pts/ptmx; do test -e $p && echo "$p yes" || echo "$p no"; done
        (echo y > /dev/shm/t && cat /dev/shm/t) >/dev/null 2>&1 && echo "/dev/shm yes" || echo "/dev/shm no" ;;
      resolve)
        for h in localhost $(hostname); do wget -q -T 2 -O /dev/null "http://$h:8080/cgi-bin/info?pid1" && echo "$h ok" || echo "$h fail"; done ;;
      spawn) for i in $(seq 20); do (sleep 0.3 &); done; echo spawned ;;
      zombies) n=0; for s in /proc/[0-9]*/stat; do st=$(cat $s 2>/dev/null) || continue; st=''${st##*) }; [ "''${st%% *}" = Z ] && n=$((n + 1)); done; echo $n ;;
      pid1) tr '\0' ' ' < /proc/1/cmdline ;;
      size) echo "$(grep -c ^processor /proc/cpuinfo) $(grep MemTotal /proc/meminfo)" ;;
      clock) date +%s ;;
      # Answers after the machine's delay, with its hostname; "overlap" when another such request was
      # already being served here (mkdir is atomic): what the hard-limit check counts at the instance.
      slow) if mkdir /tmp/busy 2>/dev/null; then sleep "$(cat /tmp/delay)"; rmdir /tmp/busy; echo "ok $(hostname)"
        else sleep "$(cat /tmp/delay)"; echo "overlap $(hostname)"; fi ;;
      ipv6) httpd -f -p '[::1]:8081' -h /app & p=$!; sleep 0.3
        wget -q -T 2 -O /dev/null 'http://[::1]:8081/serve.sh' && echo ok || echo fail; kill $p ;;
      cgroup) cat /proc/self/cgroup; cat /sys/fs/cgroup/cgroup.controllers ;;
      inotify) cat /proc/sys/fs/inotify/max_user_instances ;;
      net) ip -4 -o addr show dev eth0 | grep -o 'inet [0-9./]*'; echo "routes: $(ip route | grep -c .)"; echo "v6: $(ip -6 -o addr show dev eth0 | grep -c .)"
        wget -q -T 2 -O /dev/null http://1.1.1.1/ 2>&1 ;;
      volume) ls -ld /data/vol 2>&1 ;;
      xattr) getfattr --absolute-names -d -m - -e hex /app/caps 2>&1
        echo x > /tmp/caps && setfattr -n security.capability -v 0x0100000200040000000000000000000000000000 /tmp/caps 2>&1 &&
          getfattr --absolute-names -n security.capability -e hex /tmp/caps 2>&1 ;;
      term) echo bye; kill -TERM 1 ;;
      # The metrics suite: a CPU kept busy for 30 s in the background.
      burn) (end=$(($(date +%s) + 30)); while [ "$(date +%s)" -lt "$end" ]; do :; done) >/dev/null 2>&1 &
        echo burning ;;
    esac
    EOF
    chmod +x $out/app/serve.sh $out/app/cgi-bin/info
  '';
  testapp = { name, user ? "", workdir ? "" }: pkgs.dockerTools.buildImage {
    name = "flyyy/${name}";
    tag = "test";
    copyToRoot = testappRoot;
    # Writable /tmp for a non-root app (the store would make it read-only).
    extraCommands = "mkdir -p tmp home/app && chmod 1777 tmp && chmod 0777 home/app";
    config = {
      # `sh` without a slash: the launcher must find it in the image's PATH.
      Entrypoint = [ "sh" "/app/serve.sh" ];
      Env = [ "PATH=/bin" "IMAGE_VAR=from the image" "SPACED=a b  c" ];
      # Not in the image's filesystem: the launcher creates it.
      Volumes."/data/vol" = { };
      User = user;
      WorkingDir = workdir;
    };
  };
  # A plain static server with distinct content: distinct digests for cold builds.
  cold = n: pkgs.dockerTools.buildImage {
    name = "flyyy/cold${toString n}";
    tag = "test";
    copyToRoot = root "cold${toString n}" "echo cold${toString n} > $out/app/index.html";
    config.Entrypoint = [ "/bin/httpd" "-f" "-p" "80" "-h" "/app" ];
  };
  # Two services (tests/services.py): http on 8080 (/healthz, which /cgi-bin/fail and /cgi-bin/pass toggle;
  # /cgi-bin/id says which machine) and a tcp echo on $TCP_PORT, default 7000 (tcpsvd: the machine's id on a
  # first line, then cat).
  svc = pkgs.dockerTools.buildImage {
    name = "flyyy/svc";
    tag = "test";
    copyToRoot = root "svc" ''
      mkdir -p $out/www/cgi-bin
      cat > $out/app/serve.sh <<'EOF'
      echo ok > /www/healthz
      tcpsvd 0.0.0.0 "''${TCP_PORT:-7000}" sh -c 'hostname; exec cat' &
      exec httpd -f -p 8080 -h /www
      EOF
      printf '#!/bin/sh\necho "Content-Type: text/plain"; echo; rm -f /www/healthz; echo failing\n' > $out/www/cgi-bin/fail
      printf '#!/bin/sh\necho "Content-Type: text/plain"; echo; echo ok > /www/healthz; echo passing\n' > $out/www/cgi-bin/pass
      printf '#!/bin/sh\necho "Content-Type: text/plain"; echo; hostname\n' > $out/www/cgi-bin/id
      chmod +x $out/www/cgi-bin/*
    '';
    config.Entrypoint = [ "/bin/sh" "/app/serve.sh" ];
  };
  # ~200 MB of incompressible data: its export takes long enough to kill a build half-way.
  bulk = n: pkgs.dockerTools.buildImage {
    name = "flyyy/bulk${toString n}";
    tag = "test";
    copyToRoot = root "bulk${toString n}" ''
      echo bulk${toString n} > $out/app/index.html
      head -c 200M /dev/urandom > $out/app/blob
    '';
    config.Entrypoint = [ "/bin/httpd" "-f" "-p" "80" "-h" "/app" ];
  };
in
{
  # registry path → image tarball
  "traefik/whoami" = pull {
    imageName = "traefik/whoami";
    imageDigest = "sha256:c4717a8d1f0134a7444e24f881160e033991f23027c6c5a9a3f8fd22e70d1d44";
    hashes.arm64 = "sha256-S99E/g69pCcg/QpemmrjpAJJnDv84+tgx0b3DiyoIW0=";
    hashes.amd64 = "sha256-kKNg4v28160F3kOoJJTAmrHU9H4eIjaj0gl+L/9CDj0=";
  };
  "hashicorp/http-echo" = pull {
    imageName = "hashicorp/http-echo";
    imageDigest = "sha256:fcb75f691c8b0414d670ae570240cbf95502cc18a9ba57e982ecac589760a186";
    hashes.arm64 = "sha256-pqWruKiBLstA73d2cnfDm5KqMU4evSWCmVPm9n6Q54k=";
    hashes.amd64 = "sha256-hgVSU8/NXqWtZ/YYOKIFz7DHoH0592qxAi2ljwCSgOM=";
  };
  # Plus /app/caps with xattrs (tests/xattr-layer.py).
  "flyyy/testapp" = pkgs.runCommand "testapp-xattr.tar" { nativeBuildInputs = [ pkgs.python3 ]; }
    "python3 ${./xattr-layer.py} ${testapp { name = "testapp"; }} $out";
  "flyyy/testapp-user" = testapp { name = "testapp-user"; user = "app"; workdir = "/app"; };
  "flyyy/svc" = svc;
  "flyyy/cold1" = cold 1;
  "flyyy/cold2" = cold 2;
  "flyyy/cold3" = cold 3;
  "flyyy/bulk1" = bulk 1;
  "flyyy/bulk2" = bulk 2;
  "flyyy/bulk3" = bulk 3;
}
