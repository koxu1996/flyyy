# guest: the guest's PID 1 (flyyy-init), checked from inside VMs with the Nix-built test app
# (tests/images.nix): exact argv and env, User and WorkingDir, /dev and /etc, name resolution, zombies,
# and PID 1's signal forwarding.
import base64

# 40+ args the kernel cmdline used to mangle: spaces, quotes, =, a second --, empty, tabs, newlines, unicode, globs.
ARGS = ["a b", "", 'q"uote', "it's", "x=y", "--", "-x", "tab\there", "zażółć", "$HOME", "*", "a;b", "{id}?", "new\nline"] + [f"arg {i}" for i in range(30)]
SHELL_VARS = {"PWD", "SHLVL", "_", "OLDPWD"}  # exported by the test app's own shell


def one(app: str) -> tuple[str, str]:
    w, mid = live(app)[0]
    return w, mid


def info(app: str, what: str) -> str:
    n, mid = one(app)
    out = ""
    def got() -> bool:
        nonlocal out
        rc, out = edges.execute(f"curl -fs --max-time {REQUEST} {bh_proxy(n)} 'http://{mid}.http/cgi-bin/info?{what}'")
        return rc == 0
    until(got, REQUEST)
    return out


def console(app: str) -> str:
    n, mid = one(app)
    return vm_journal(WORKERS[n], mid)


# 3. Exact argv and env (testapp: root, no WorkingDir, Entrypoint `sh /app/serve.sh`).
args = " ".join(shlex.quote(a) for a in ARGS)
fly(f"deploy t-argv --image {image('flyyy/testapp')} --port 8080 --count 1 --timeout {DEPLOY} --env 'FOO=a b=c; --d' --env EMPTY= -- {args}")
n, mid = one("t-argv")
argv = base64.b64decode(info("t-argv", "argv")).decode().split("\0")[:-1]
want = [a.replace("{id}", mid) for a in ARGS]
check(argv == want, f"argv: {len(ARGS)} args (spaces, quotes, =, --, empty, tab, newline, unicode, globs) arrive byte for byte ({{id}} -> {mid})", f"argv: got {argv}, want {want}")
env = dict(e.split("=", 1) for e in base64.b64decode(info("t-argv", "env")).decode().split("\0") if "=" in e)
want_env = {"PATH": "/bin", "IMAGE_VAR": "from the image", "SPACED": "a b  c", "FOO": "a b=c; --d", "EMPTY": "", "HOSTNAME": mid, "HOME": "/root"}
got_env = {k: v for k, v in env.items() if k not in SHELL_VARS}
check(got_env == want_env, f"env: exactly the image Env + FOO/EMPTY + HOSTNAME/HOME ({len(got_env)} vars)", f"env: got {got_env}, want {want_env}")
leak = [k for k in env if re.match(r"^(pci|earlycon|console|reboot|panic|FLYYY_)", k)]
check(not leak, "env: no kernel params leak (pci, earlycon, FLYYY_*)", f"env leaks: {leak}")
check(info("t-argv", "pid1").strip() == "/init", "PID 1 is /init (flyyy-init, the initrd's only file)", f"PID 1: {info('t-argv', 'pid1')}")


def size(app: str) -> tuple[int, int]:
    """(vCPUs, MemTotal in MiB) as the guest sees them."""
    cpus, _, kb, _ = info(app, "size").split()
    return int(cpus), int(kb) // 1024


cpus, mib = size("t-argv")
check(cpus == 1 and 64 < mib <= 128, f"size: by default 1 vCPU, {mib} MiB in the guest", f"size: default machine sees {cpus} vCPUs, {mib} MiB")

# What apps expect of a Linux box: a wall clock, IPv6 on lo, cgroup v2, inotify.
skew = int(info("t-argv", "clock")) - time.time()
check(abs(skew) < 10, f"clock: the guest's wall clock is the host's (skew {skew:+.1f} s)", f"clock: guest is {skew:+.0f} s off the host")
check(info("t-argv", "ipv6").strip() == "ok", "ipv6: an app listens and answers on [::1]", f"ipv6: {info('t-argv', 'ipv6')}")
cg = info("t-argv", "cgroup").split("\n")
check(cg[0] == "0::/" and {"cpuset", "cpu", "memory", "pids"} <= set(cg[1].split()), f"cgroup: v2 at /sys/fs/cgroup, the app in its root ({cg[1]})", f"cgroup: {cg}")
check(info("t-argv", "inotify").strip().isdigit(), "inotify: available (/proc/sys/fs/inotify)", f"inotify: {info('t-argv', 'inotify')}")
net = info("t-argv", "net").strip().split("\n")
check(net[:3] == ["inet 10.0.0.2/32", "routes: 0", "v6: 0"] and "unreachable" in " ".join(net[3:]).lower(),
      "net: eth0 is a dummy with 10.0.0.2/32, no IPv6, no routes (1.1.1.1 unreachable)", f"net: {net}")
vol = info("t-argv", "volume").split()
check(vol[:1] == ["drwxr-xr-x"] and vol[2:4] == ["root", "root"], "volume: the image's /data/vol created (root, 0755)", f"volume: {vol}")
CAP = "0x0100000200040000000000000000000000000000"
xa = info("t-argv", "xattr")
check(f"security.capability={CAP}" in xa.split("# file: /tmp/caps")[0] and 'user.flyyy=0x796573' in xa,
      "xattr: the image's file capability and user.* xattr survive registry -> EROFS -> overlay", f"xattr: image file: {xa}")
check(xa.rstrip().endswith(f"security.capability={CAP}") and "# file: /tmp/caps" in xa,
      "xattr: setcap-style security.capability on the scratch (ext4 under the overlay)", f"xattr: scratch file: {xa}")

# 5. /dev and /etc, and PATH lookup.
dev = info("t-argv", "dev").split("\n")
check(all(l.endswith(" yes") for l in dev if l), "/dev: fd/0, stdout, pts/ptmx exist, /dev/shm writable", f"/dev: {dev}")
check(info("t-argv", "resolve").split() == ["localhost", "ok", mid, "ok"], f"/etc/hosts: localhost and {mid} resolve (and answer)", f"resolution: {info('t-argv', 'resolve')}")
check(until(lambda: "testapp: written to /dev/stdout as 0" in console("t-argv"), REQUEST), "/dev/stdout: a write to it reaches the console", "/dev/stdout: nothing on the console")
check("flyyy-init: started /bin/sh" in console("t-argv"), "PATH: Entrypoint `sh` (no slash) found in the image's PATH", "PATH: no 'started /bin/sh' in the console")

# 6. PID 1: orphans are reaped, SIGTERM reaches the app's group, the app's exit ends the VM.
info("t-argv", "spawn")
time.sleep(1.5)
z = info("t-argv", "zombies").strip()
check(z == "0", "pid 1: 20 abandoned grandchildren reaped (0 zombies)", f"pid 1: {z} zombies")
info("t-argv", "term")
check(until(lambda: re.search(r"flyyy-init: \S+ exited \(143\)", console("t-argv")) is not None, PROPAGATE),
      "pid 1: SIGTERM to the launcher reached the app (exit 143 ended the VM)", "pid 1: no forwarded SIGTERM / exit 143 in the console")

# Size: --cpus/--memory reach Cloud Hypervisor (its arguments, in the launch record) and the guest sees them.
fly(f"deploy t-big --image {image('flyyy/testapp')} --port 8080 --count 1 --cpus 2 --memory 512 --timeout {DEPLOY}")
cpus, mib = size("t-big")
n, mid = one("t-big")
a = json.loads(WORKERS[n].succeed(f"cat {RUN}/{mid}.json"))
mc = {k: a[a.index(k) + 1] for k in ("--cpus", "--memory")}
check(cpus == 2 and 384 < mib <= 512 and mc == {"--cpus": "boot=2", "--memory": "size=512M"},
      f"size: --cpus 2 --memory 512 -> Cloud Hypervisor {mc}, the guest sees {cpus} vCPUs, {mib} MiB", f"size: {mc}, guest {cpus} vCPUs {mib} MiB")
fly("destroy t-big")

# 4. User and WorkingDir from the image (testapp-user: User app, WorkingDir /app).
fly(f"deploy t-user --image {image('flyyy/testapp-user')} --port 8080 --count 1 --timeout {DEPLOY}")
ids = info("t-user", "ids").strip()
check(ids == "uid=1000 gid=1000 groups=1000 1001 cwd=/app", f"user/workdir: {ids}", f"user/workdir: {ids}")
uenv = dict(e.split("=", 1) for e in base64.b64decode(info("t-user", "env")).decode().split("\0") if "=" in e)
check(uenv.get("HOME") == "/home/app", "user/workdir: HOME from the image's passwd (/home/app)", f"user/workdir: HOME {uenv.get('HOME')}")
dev = info("t-user", "dev").split("\n")
check(all(l.endswith(" yes") for l in dev if l) and info("t-user", "resolve").split()[1] == "ok",
      "user/workdir: root-only setup (hosts, /dev/shm, pts) happened before dropping privileges", f"non-root: {dev}")
check(until(lambda: "testapp: written to /dev/stdout as 1000" in console("t-user"), REQUEST), "user/workdir: a non-root app can reopen /dev/stdout", "non-root /dev/stdout: nothing on the console")

for a in ["t-argv", "t-user"]:
    fly(f"destroy {a}")
