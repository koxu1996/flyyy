/ponytail full

## Goal
Stage 8 of the spike (stages 1–7: `PROMPT.md`, `PROMPT-stage2.md`…`PROMPT-stage7.md`, all summarised in `NOTES.md`).
VM disks still come from host infrastructure that every worker shares:
- a containerd started by `up.sh`;
- a devmapper thin pool on loop files (`modprobe dm-thin-pool`, `losetup`, `dmsetup`);
- a root socket bind-mounted into each worker;
- `mknod` of dm devices, which needed the `block-device-mapper rwm` device rule.

Replace all of it with **files that each worker builds itself**:
- **Image → read-only EROFS, once per image per worker, in the background:** `crane export <ref> - | mkfs.erofs --tar=f <file>`,
  streamed. EROFS sizes itself from the content, so there's no size to choose, no tar copy on disk and no resize step.
  No mounts, no block devices and no root. The file is cached and shared read-only by every machine of that image on the worker.
  (Checked before building: both images build this way, keep owners, modes, symlinks and the sticky bit, and come out
  at 11.6 MiB for whoami and 8.3 MiB for echo, against 31 MiB each as ext4 sized from the tar.)
- **Per machine:** that image as a **read-only drive**, plus a **fresh sparse scratch drive on every boot**.
  Nothing written survives a stop: persistent data is what volumes are for, and they're out of scope.
- **In the guest:** the initrd stacks them with **overlayfs** (lower = image, upper/work = scratch) and `switch_root`s
  into the merged root.

The aim is that `up.sh`/`down.sh` and the worker containers get simpler: no containerd, thin pool, loop/dm devices,
host modules or device rules.

## Target (changes from stage 7 in bold)
host
├── **nothing for disks: no containerd, no thin pool, no loop or dm devices**
├── `control`, 2 edges (unchanged)
└── workers 1–3 (+4)
    ├── corrosion agent, backhaul (unchanged)
    ├── machine agent: **background image builds, one per digest, cached as `<state>/images/<digest>.erofs`
    │   (read-only); a sparse `<state>/<id>.scratch` per running machine, recreated on every start**
    └── Firecracker VMs: **drive 1 = the image (`is_read_only`), drive 2 = scratch; the initrd mounts an overlay**

## Image builds: a background state machine per digest (machine agent)
Modelled on a per-image FSM with idempotent "already done?" steps and persisted state, so a crash resumes instead
of starting over. The reconcile loop never blocks on an image.
- **Key:** the pinned digest from the ref (`<repo>@sha256:<d>`, validated as in stage 6). It's known without network
  access, and the same digest always means the same bytes. Two refs with the same digest share one build.
- **States** (persisted in `<state>/images/<digest>.json` after every transition, written atomically):
  `resolving → building → checking → ready`, or `failed` with an error and a `retry_at`.
  1. **resolving:** `crane config --platform linux/arm64 <ref>` → Env/Entrypoint/Cmd (stored in the JSON). This
     replaces the stage 6 `ctr content get` path.
  2. **building:** `crane export --platform linux/arm64 <ref> - | mkfs.erofs --tar=f <digest>.erofs.tmp`. crane
     flattens the layers and applies whiteouts. Both exit statuses count: a failed export must not leave an image built
     from a truncated stream.
  3. **checking:** `fsck.erofs`, then `chmod 0444` and rename to `<digest>.erofs`.
  4. **ready.**
  Each step first checks whether its output already exists, so it's idempotent. Resolving is skipped when its result is
  already in the JSON, and building is skipped when the `.erofs` exists.
  `// shortcut: uncompressed EROFS (no -z); try lz4 when disk matters more than CPU`.
- **Concurrency:** one build task per digest at a time (an in-memory map from digest to task). Every machine needing
  that digest waits on the same build, and different digests build in parallel. The reconcile loop only reads the state.
- **Restart:** on agent start, every non-`ready`, non-`failed` JSON resumes from its persisted state. Leftover `*.tmp`
  files are removed first, since they're never trusted. A build that was cut off starts its export again: there's no
  partial download to resume.
- **Unprivileged:** run the crane and mkfs.erofs steps as an unprivileged user (`setpriv --reuid/--regid --clear-groups`)
  in an images directory that user owns. The agent only reads the results.
- **Failure:** `failed` with crane's or mkfs.erofs's error and backoff (backon, as for machines). A retry starts from
  `resolving`, reusing any finished outputs.
- **Machines waiting on an image:** `plan()` gives them no Create until their image is `ready`. Their fact is
  `pulling` while it builds, or `failed` with the image's error. Extend `plan()`'s unit test with the image states.
- `// shortcut: each worker fetches and builds every image itself (no shared cache, no layer dedupe, no image GC)`.

## Machines
- **Start** (every boot): `truncate -s 1G <id>.scratch && mke2fs -q -t ext4 <id>.scratch` (sparse, so it only uses what
  the guest writes), then the Firecracker config:
  - drive `rootfs`: the EROFS image file, `is_read_only: true`;
  - drive `scratch`: read-write;
  - neither is the root device: the initrd mounts both.
- **Stop:** delete the scratch file after the unit stops. **Destroy:** the same, plus the config and local state.
  The image stays in the cache.
- The IP and tap stay allocated across stop/start, as before.
- `// shortcut: fixed 1 GiB scratch, no per-app size; nothing persists across a stop (volumes are out of scope)`.
- The agent no longer touches containerd, `/sys/block`, `mknod` or `/spike`. Both files live in its state directory.
  Remove the `/spike` and containerd-socket bind mounts and the `block-device-mapper` rule from the worker containers.

## Guest
- Kernel: add `CONFIG_EROFS_FS=y` and `CONFIG_OVERLAY_FS=y` (keep the "missing option → fail the build" check).
- Initrd init:
  1. mount `/dev/vda` as `erofs` (read-only) at `/lower`;
  2. mount `/dev/vdb` at `/scratch` and create `upper` and `work` there;
  3. mount `overlay` at `/newroot` (`lowerdir=/lower,upperdir=/scratch/upper,workdir=/scratch/work`);
  4. move `/dev`, `/proc`, `/sys` as before, then `switch_root`.
  If any step fails, print what failed to the console and power off (panic), rather than hang.

## up.sh / down.sh / scripts
- `up.sh`: the API token, containers and seed apps only. No thin pool, containerd or images.
- `down.sh`: destroy containers and remove state, plus a leftovers check that also asserts there are no `spike` dm or
  loop devices and no `spike-containerd` unit.
- The `tools` package loses containerd/lvm2.
- `lifecycle.sh`: the stop/start check becomes "scratch is fresh":
  - while stopped, no scratch file exists;
  - after start, a new one exists and its `upper/` holds only what the guest wrote since that boot (`debugfs ls`);
  - the IP is the same.
  Destroy checks the scratch file is gone and the image is still cached. The other checks just drop snapshots and dm devices.
- `storm.sh churn` leak check: scratch files, configs, units, taps and local state, but no snapshots or dm devices any more.

## Success criteria (all must pass)
1. **Host is plain:** while the stack is up, there's no containerd process, no `spike-containerd` unit, and no `spike`
   dm or loop devices. `up.sh`/`down.sh` have no `dmsetup`/`losetup`/`modprobe`/`ctr`. The worker containers have no
   device rule or containerd socket.
2. **Everything still works:** `verify.sh --failover`, `deploy.sh`, `lifecycle.sh`, `storm.sh plain churn agent-kill`
   and `workers.sh` pass, with the adaptations above.
3. **Background builds:**
   - deploying a cold image with `--count 3` gives one build per worker, with the machines `pulling` meanwhile;
   - during that build, other machines on the same worker (another app, a stop/start) carry on: the reconcile loop
     isn't blocked;
   - a second app with the same digest, deployed during the build, waits on the same build, not a new one (the log shows one export per worker);
   - later machines reuse the cached image with no crane run.
4. **Image file:**
   - log the build time and EROFS size per image;
   - mode 0444, opened read-only by Firecracker;
   - its checksum is unchanged after several machines have booted, run and been destroyed.
5. **Image fidelity:** whoami and echo behave as before. Spot-check that the image keeps file modes, owners and
   symlinks: compare a few entries of `crane export | tar tv` with `dump.erofs --path=…`.
6. **Fresh scratch:** after a stop, there's no scratch file. After a start, there's a new one with `upper/` and `work/`
   (`debugfs ls`), and nothing from before the stop.
7. **Failures and restarts:**
   - an unknown digest → the image `failed` with crane's error, the waiting machines `failed`, and retries back off;
   - `kill -9` of the agent mid-build → on restart, the `.tmp` files are removed and the build resumes from its
     persisted state (resolving isn't repeated), then reaches `ready`;
   - a failing `crane export` mid-stream never gives a `ready` image.
8. `down.sh` leaves the host clean, with nothing left from the old disk stack either.

## Risk points: check these first, in this order, and stop early if one is a blocker
1. **Registry access from inside a worker.** containerd pulled from the host network; now each worker fetches through
   its private veth. It needs DNS (is the container's resolv.conf usable?) and NAT out.
   - If it doesn't work: add a masquerade rule for `10.99.0.0/16` in a **table of our own** (`nft add table ip spike …`)
     in `up.sh`, and delete that table in `down.sh`, so the host config stays untouched.
   - If that's blocked too: run the export step on the host as the invoking (non-root) user, into each worker's images
     directory. That's still no containerd/devmapper. Say which one you used.
2. **EROFS in the guest:** the kernel options build, the initrd mounts EROFS, and overlayfs accepts it as a lower.
   Fallback: ext4 from the tar (`mke2fs -b 4096 -O ^has_journal -d <tar>`, sized from tar bytes + entries × 4 KiB
   + 16 MiB); checked locally, it works too. Say if you used it.
3. **One image file opened read-only by many Firecracker VMs at once** (`is_read_only: true`).
4. **Background tasks in the agent:** builds are `tokio::spawn`ed child-process pipelines. Their state is shared with
   the reconcile loop only through the persisted JSON and a small in-memory map. A finished build must wake the
   reconcile loop (the existing `Notify`), so waiting machines start without waiting for the 10 s tick.

## Constraints
- Mark every shortcut as `# shortcut:` / `// shortcut:`. Keep the one-table flake approach.
- New dependencies: none in Rust, as `crane`, `erofs-utils`, `e2fsprogs` (scratch) and `util-linux` are tools on the agent's `path`.
- Validation and error handling aren't shortcuts: refs are validated before argv, every build step is atomic
  (`.tmp` + rename), and a failed export or fsck fails with a readable error.
- One small unit test for non-trivial logic: `plan()` with images not ready, and the image FSM's next step from a
  persisted state.

## Out of scope
- Volumes and anything else that persists across a stop. Also image GC, sharing the cache across workers,
  lazy/streaming images and registry auth.
- Moving Firecracker out of nspawn, and jailer.

## Deliverables
- Flake (kernel option, initrd init, worker containers without the disk-stack bits, the agent's tool path, slimmer
  `tools`), agent changes (image FSM, scratch per boot, two drives), `up.sh`/`down.sh`, and adapted
  `lifecycle.sh`/`storm.sh`/`verify.sh`.
- A "Stage 8" section in `NOTES.md` (≤ ½ page):
  - verdict;
  - which risk-1 and risk-2 paths were used;
  - numbers: cold image build time per step, and EROFS size per image; create → started (warm) vs
    stage 7; scratch bytes actually used per machine; how much shorter `up.sh`/`down.sh` got;
  - the top 3 problems for taking this further.
