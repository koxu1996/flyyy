# images.sh: VM disks are files the workers build themselves. No containerd anywhere; images build in the
# background (one per digest per worker, not blocking the reconcile loop) into read-only EROFS files that
# keep the image's metadata; a cut-off export never becomes an image; a killed agent resumes its build.
# The bulk images (~200 MB each, tests/images.nix) are slow enough to be caught mid-build.
IMAGES = f"{STATE}/images"
BG, CUT, RESUME = image("flyyy/bulk1"), image("flyyy/bulk2"), image("flyyy/bulk3")


def hexd(ref: str) -> str:
    return ref.split("@sha256:")[1]


def converged(app: str) -> bool:
    return status(app)["converged"]


def workers_of(app: str) -> list[str]:
    return sorted({w for w, _ in live(app)})


# 1. No containerd or device-mapper on the workers.
for n in NAMES3:
    m = WORKERS[n]
    check(m.execute("pgrep -x containerd")[0] != 0 and m.execute("test -e /run/containerd.sock")[0] != 0,
          f"{n}: no containerd, no containerd socket", f"{n}: containerd present")

# 3. Background builds: two apps with the same cold digest share one build per worker, the machines are
# `pulling` meanwhile, and a stop on the same worker isn't held up by it (a machine of the test's own,
# warm image: the control plane would start one of its own again).
echo1 = machine_create("worker1", "probe", image("hashicorp/http-echo"), 5678, ["-text={id}"])
wait_fact(echo1, "started", BOOT)
since = time.time()
fly(f"deploy bg --image {BG} --port 80 --count 3 --detach")
fly(f"deploy bg2 --image {BG} --port 80 --count 3 --detach")
machine_set(echo1, "stopped")
check(until(lambda: any(m["state"] == "pulling" for m in status("bg")["machines"]), PROPAGATE, 0.1), "background: bg machines reported pulling", "background: never saw pulling")
check(until(lambda: converged("bg") and converged("bg2"), DEPLOY), "background: bg and bg2 (same digest) converged", f"background: {status('bg')} {status('bg2')}")
wait_fact(echo1, "stopped", ACT)
for w in sorted(set(workers_of("bg") + workers_of("bg2"))):
    j = journal(WORKERS[w], "agent", since)
    builds = len(re.findall(rf"(?m)^image step done digest={hexd(BG)} step=Building ", j))
    check(builds == 1, f"dedupe: {w} built it once for its bg and bg2 machines", f"dedupe: {w} built it {builds} times")
    for l in re.findall(rf"(?m)^image ready digest={hexd(BG)} .*$", j):
        note(f"{w}: {l}")
# Not blocked: on worker1, the echo machine's stop began (its drain) before the image was ready.
j = worker1.succeed(f"journalctl -u agent --since @{int(since)} -o short-unix --no-pager")
stop_at = next((float(l.split()[0]) for l in j.splitlines() if re.search(rf"draining machines=.*{echo1}", l)), None)
ready_at = next((float(l.split()[0]) for l in j.splitlines() if f"image ready digest={hexd(BG)}" in l), None)
if stop_at and ready_at:
    check(stop_at < ready_at, f"not blocked: worker1 began stopping {echo1} while building ({(ready_at - stop_at) * 1000:.0f} ms before ready)", "not blocked: the stop waited for the build")
else:
    note("not blocked: worker1 has no build of bg or no stop in its log (placement); inconclusive")
machine_set(echo1, "destroyed")
wait_fact(echo1, "destroyed", ACT)
# Warm reuse: a scale-up builds nothing.
since = time.time()
fly("scale bg --count 6")
for n in NAMES3:
    j = journal(WORKERS[n], "agent", since)
    check(not re.search(rf"(?m)^image step done digest={hexd(BG)} step=(Resolving|Building) ", j), f"warm: {n} reused the cached image", f"warm: {n} rebuilt it")

# 4. The image file: read-only on disk and as Cloud Hypervisor opens it, unchanged by the machines using it.
f = f"{IMAGES}/{hexd(BG)}.erofs"
check(worker1.succeed(f"stat -c %a {f}").strip() == "444", f"image: {hexd(BG)[:12]}.erofs is mode 0444, {worker1.succeed(f'stat -c %s {f}').strip()} bytes", "image: mode not 0444")
sum0 = worker1.succeed(f"sha256sum {f}").split()[0]
m = next(m["id"] for m in status("bg")["machines"] if m["worker"] == "worker1" and m["state"] == "started")
pid = vm_pid(worker1, m)
flags = worker1.succeed(f"for fd in /proc/{pid}/fd/*; do case $(readlink $fd) in *.erofs) awk '/^flags/ {{ print $2 }}' /proc/{pid}/fdinfo/${{fd##*/}};; esac; done | head -1").strip()
check(flags and int(flags, 8) & 3 == 0, f"image: fc-{m} opened it O_RDONLY (flags {flags})", f"image: fd flags '{flags}'")
fly("destroy bg")
fly("destroy bg2")
check(worker1.succeed(f"sha256sum {f}").split()[0] == sum0, "image: checksum unchanged after 9 machines booted, ran and were destroyed", "image: checksum changed")

# 5. Fidelity: owners, modes and symlinks survive (echo's distroless image: a nonroot home, symlinks).
w_echo = live("echo")[0][0]
e = f"{IMAGES}/{hexd(image('hashicorp/http-echo'))}.erofs"
def d(p: str) -> str:
    return " ".join(re.findall(r"Uid: [0-9]+ +Gid: [0-9]+ +Access: [0-7]+|symlink|regular file|directory", WORKERS[w_echo].succeed(f"dump.erofs --path={p} {e}")))
check(d("/home/nonroot").startswith("directory Uid: 65532   Gid: 65532  Access: 0700"), "fidelity: /home/nonroot 65532:65532 0700", f"fidelity: /home/nonroot {d('/home/nonroot')}")
check(d("/usr/share/zoneinfo/Africa/Asmera").startswith("symlink"), "fidelity: zoneinfo symlinks kept", "fidelity: symlink lost")
check(d("/etc/nsswitch.conf").startswith("regular file Uid: 0   Gid: 0  Access: 0644"), "fidelity: /etc/nsswitch.conf root 0644", f"fidelity: {d('/etc/nsswitch.conf')}")

# 7a. An export cut off mid-stream never becomes an image: kill crane during the build, it retries and recovers.
since = time.time()
fly(f"deploy cut --image {CUT} --port 80 --count 1 --detach")
until(lambda: bool(workers_of("cut")), ACT)
wm = WORKERS[workers_of("cut")[0]]
rc = wm.execute("for i in $(seq 300); do pkill -f '[c]rane export' && exit 0; sleep 0.05; done; exit 1")[0]
check(rc == 0, "cut: killed crane export mid-build", "cut: never saw crane export")
check(until(lambda: converged("cut"), DEPLOY), "cut: retried and converged", f"cut: {status('cut')}")
j = journal(wm, "agent", since)
check(re.search(rf"(?m)^image failed digest={hexd(CUT)} retry_in_s=1 error=Building: (crane export|mkfs.erofs)", j), "cut: the cut-off build failed and was retried", "cut: no failed build in the log")
check(len(re.findall(rf"(?m)^image ready digest={hexd(CUT)} ", j)) == 1, "cut: only the retry produced an image", "cut: more than one ready")

# 7b. kill -9 the agent mid-build: on restart the .tmp is dropped and the build resumes (no new resolve).
since = time.time()
fly(f"deploy resume --image {RESUME} --port 80 --count 1 --detach")
until(lambda: bool(workers_of("resume")), ACT)
wm = WORKERS[workers_of("resume")[0]]
rc = wm.execute("for i in $(seq 300); do pgrep -f '[c]rane export' >/dev/null && { kill -9 $(systemctl show -P MainPID agent); exit 0; }; sleep 0.05; done; exit 1")[0]
check(rc == 0, "resume: kill -9 agent during the export", "resume: never saw crane export")
check(until(lambda: converged("resume"), DEPLOY), "resume: converged after the agent restart", f"resume: {status('resume')}")
j = journal(wm, "agent", since)
after = j.split("agent started ")[-1]
if f"image: removing a leftover build file={hexd(RESUME)}.erofs.tmp" in j:
    ok("resume: leftover .tmp removed on start")
else:
    note("resume: no .tmp left (killed before mkfs.erofs created it)")
check(f"image: resuming an interrupted build digest={hexd(RESUME)}" in j, "resume: picked up the interrupted build from its persisted state", "resume: no resume in the log")
check(not re.search(rf"(?m)^image step done digest={hexd(RESUME)} step=Resolving ", after), "resume: no new resolve (config kept in the record)", "resume: resolved again")
for a in ["cut", "resume"]:
    fly(f"destroy {a}")

# 8. Each worker reports its finished images' sizes (the images table, one row per digest), and only when one
# changes. Apps and workers show image_bytes; flyyy status the app's size.
def files(n: str) -> dict[str, int]:
    out = WORKERS[n].succeed(f"cd {IMAGES} && for f in *.erofs; do [ -e \"$f\" ] && stat -c '%s %n' \"$f\"; done; true")
    return {f"sha256:{name.removesuffix('.erofs')}": int(size) for size, name in (l.split() for l in out.splitlines())}


def rows(n: str) -> dict[str, int]:
    got = q(WORKERS[n], "http://127.0.0.1:8080", "SELECT digest, CAST(bytes AS TEXT) FROM images WHERE worker = ?", [n])
    return {d: int(b) for d, b in got if int(b) > 0}


for n in NAMES3:
    have = {}
    def same() -> bool:
        global have
        have = files(n)
        return rows(n) == have
    check(until(same, PROPAGATE), f"images: {n} reported its {len(have)} images' sizes ({sum(have.values())} bytes)", f"images: {n} table {rows(n)} != files {have}")
    w = next(x for x in json.loads(fly("workers --json")) if x["name"] == n)
    check(w["image_bytes"] == sum(files(n).values()), f"images: {n} image_bytes = {w['image_bytes']}", f"images: {n} image_bytes {w['image_bytes']} != {sum(files(n).values())}")
for app in ["whoami", "echo"]:
    st = status(app)
    digest = st["image"].rsplit("@", 1)[1]
    want = max(files(n).get(digest, 0) for n in NAMES3)
    check(st["image_bytes"] == want, f"images: {app} image_bytes = {want}", f"images: {app} image_bytes {st['image_bytes']}, want {want}")
    check(f"({want} bytes)" in fly(f"status {app}"), f"images: flyyy status {app} shows its size", f"images: flyyy status {app}: {fly(f'status {app}')}")
# Only changes are written: nothing while no image changes, and nothing after an agent restart (it reads the
# table first). Builds started above can still be finishing: wait until none is, and only count the workers
# whose image files stayed the same.
until(lambda: all(WORKERS[n].execute(f"ls {IMAGES}/*.tmp")[0] != 0 for n in NAMES3), DEPLOY)
# ...and reported: each agent's own record of the table equals its files, so it has nothing left to write.
def reported(n: str) -> dict[str, int]:
    rc, out = WORKERS[n].execute(f"cat {STATE}/reports/images.json")
    return {d: b for d, b in json.loads(out).items() if b > 0} if rc == 0 else {}


check(until(lambda: all(reported(n) == files(n) == rows(n) for n in NAMES3), ACT), "images: every agent has reported all its files", "images: reports pending")
before = {n: files(n) for n in NAMES3}
since = time.time()
time.sleep(20)
worker2.succeed("systemctl restart agent")
time.sleep(20)
steady = [n for n in NAMES3 if files(n) == before[n]]
reports = {n: len(re.findall(r"(?m)^images reported ", journal(WORKERS[n], "agent", since))) for n in steady}
check("worker2" in steady and all(c == 0 for c in reports.values()), f"images: no writes while nothing changed (workers {steady}), none after worker2's agent restarted",
      f"images: reports {reports} (unchanged workers {steady})")
# An image file that's gone is reported as 0 (rows aren't deleted), once.
gone = next(iter(files("worker3")))
worker3.succeed(f"rm {IMAGES}/{gone.removeprefix('sha256:')}.erofs")
def zero() -> bool:
    got = q(worker3, "http://127.0.0.1:8080", "SELECT CAST(bytes AS TEXT) FROM images WHERE worker = 'worker3' AND digest = ?", [gone])
    return got == [["0"]]


check(until(zero, PROPAGATE), f"images: worker3's deleted {gone[:19]} reported as 0 bytes", f"images: {gone} still {rows("worker3").get(gone)}")
