/ponytail full

## Goal
Stage 10 of the spike (stages 1–9: `PROMPT.md`, `PROMPT-stage2.md`…`PROMPT-stage9.md`, all summarised in `NOTES.md`).
`compat/REPORT.md` tried 61 real images and only 22 run. Most failures come from how we start the app: the image's
Env/Entrypoint/Cmd are flattened onto the kernel command line, and busybox `switch_root` `execv`s whatever arrives.

Replace that with **a small static guest launcher and a config file**, Fly's `init` in miniature. It covers report
causes C1, C2, C4, C5, C6, C8, C9 and C12 (the cause codes are in `compat/REPORT.md` §2):
- **C1:** argv[0] is resolved in the image's `PATH`;
- **C2, C4, C9:** env and argv carried exactly, so spaces, `=`, a second `--` and long lists all survive;
- **C8:** the image's `WorkingDir` and `User` honoured;
- **C5:** a usable `/dev` (`fd`, `std*`, `pts`, `shm`);
- **C6:** `/etc/hosts`, `hostname` and `resolv.conf` in place;
- **C12:** a `--env` path from the CLI through the API to the guest;
- **PID 1 duties:** reaping zombies and forwarding signals.

The kernel cmdline carries only what the initrd itself needs.

## Where things are today (read these first)
- `proxy/src/images.rs` `resolve()`: Env + `--` + Entrypoint, and Cmd, joined with spaces. `agent.rs` `create()`/`start()`
  build `boot_args` from them.
- `flake.nix` `init`: busybox; `lo`, hostname, the socat vsock shim, the EROFS + scratch overlay, `switch_root`.
- `control.rs`: `cmd` is one string limited by `cmd_ok` (no quotes, ≤ 512 chars) because of the cmdline. `flyyy deploy --cmd`.
- `compat/`: `images.txt`, `compat.sh` (the driver), `results.txt`, `REPORT.md` (the baseline: 22/61).
  `compat/` is local and gitignored: it is the baseline to compare against, not part of the repo.

## The config (agent → guest)
- The agent writes `<id>.flyyy/config.json` and puts it **on the scratch drive at create time**: `mke2fs -d <dir>`
  populates the fresh scratch, under `/flyyy/`, outside the overlay `upper/`. The scratch is still made fresh on every
  start, so the config always matches the current spec.
- Schema (version it):
  ```json
  { "version": 1, "argv": ["docker-entrypoint.sh", "postgres"],
    "env": ["PATH=/usr/local/sbin:…", "PGDATA=/var/lib/postgresql/data", "POSTGRES_PASSWORD=…"],
    "workdir": "/", "user": "", "hostname": "pg-1a2b3c", "machine_id": "pg-1a2b3c" }
  ```
- **Built by the agent** from the image config (stored by the stage 8 image state machine, which now also keeps
  `WorkingDir` and `User`) and the machine spec, with Docker's semantics:
  - **argv** = Entrypoint + (the machine's cmd if set, else the image Cmd). `--cmd` still replaces Cmd only.
  - **env** = the image Env, overridden by the app's env, plus `HOSTNAME`, plus `HOME` from the user's passwd entry
    (or `/root`) if unset.
  - **Defaults:** `PATH` is Docker's default when the image sets none; `workdir` is `/` when empty.
  - Keep the `{id}` substitution in cmd (and in env values).
- **Kernel cmdline after this:** only `console=ttyS0 reboot=k panic=1 FLYYY_MACHINE_ID=<id> FLYYY_PORT=<port>`, which
  the initrd needs for the shim and the hostname. **No app env or args.**

## The launcher (`flyyy-init`)
- A new small Rust crate (`guest/`), **static** (musl, aarch64), with no runtime dependencies. `serde_json` is OK, by the
  repo's Rust dependency rule; no tokio. It's built by Nix (`pkgsStatic.rustPlatform`, or a musl cross build) and
  copied into the initrd.
- **Hand-off:** the busybox init keeps doing what it does now (mounts, overlay, shim). Before `switch_root` it copies
  the launcher and `/scratch/flyyy/config.json` to `/newroot/.flyyy/` (they land in the overlay upper layer, so per
  boot) and runs `exec switch_root /newroot /.flyyy/flyyy-init`.
  If `/scratch/flyyy/config.json` is missing or unreadable, init fails loudly (as today: print, exit, panic).
- **What it does, in order:**
  1. Read and validate the config (version, sizes, no NUL bytes).
  2. Fix `/dev`:
     - `fd` → `/proc/self/fd`;
     - `stdin|stdout|stderr` → `fd/0|1|2`;
     - `mount devpts /dev/pts` (+ `ptmx`);
     - tmpfs on `/dev/shm` (needs `CONFIG_TMPFS`, below).
  3. Write `/etc/hosts` (`127.0.0.1 localhost <hostname>`), `/etc/hostname`, and an `/etc/resolv.conf`. There's no
     DNS yet, so a resolver on `127.0.0.1` fails fast instead of the Go-style fallback to `[::1]:53`.
     All of this lands in the overlay, as root, before privileges are dropped.
  4. Resolve `User`, Docker style: `user`, `user:group`, `uid`, `uid:gid`, looked up in the image's
     `/etc/passwd`/`/etc/group`, plus supplementary groups (initgroups).
     - A numeric uid with no entry is allowed.
     - An unknown name → fail loudly.
  5. `chdir(workdir)`. If it's missing, create it, as Docker does.
  6. Resolve argv[0]: if it has no `/`, search the config env's `PATH`.
  7. `fork`. The child calls `setgroups`/`setgid`/`setuid`, then `execve(path, argv, env)`. **The environment is exactly
     the config env**, so kernel params like `pci=off` and `earlycon=` no longer leak into the app.
  8. The launcher stays PID 1:
     - reaps every child;
     - forwards SIGTERM/SIGINT/SIGHUP/SIGQUIT/SIGUSR1/SIGUSR2 to the app's process group;
     - exits with the app's status when the app exits. PID 1 exiting panics the kernel, so Firecracker exits and the
       agent sees it, as today.
- Errors go to the console with a `flyyy-init:` prefix: every exec, user and config failure names exactly what failed.

## Kernel
- Add `CONFIG_TMPFS=y` (for `/dev/shm`) and `CONFIG_SYSVIPC=y`. Postgres, a must-pass below, needs SysV shm even with
  POSIX DSM. Keep the "missing option → fail the build" check.
- Nothing else here. The other kernel options from the report (C10, C11, C15, C16) are a later stage.

## Env and argv end to end (C12, C9)
- **API:** `PUT /v1/apps/<name>` gets
  - `env: {"K": "V", …}`: names `[A-Za-z_][A-Za-z0-9_]*`, values any UTF-8 without NUL, ≤ 64 vars, ≤ 32 KiB total;
  - `cmd: ["arg", …]`: ≤ 64 args, each ≤ 4 KiB, no NUL.
  Both are part of the spec, so a change → 409, as for image/port today. `cmd_ok`'s quote and length rule goes away,
  because it only existed for the cmdline.
- **Tables:** `apps` and `machines` gain `env` (JSON object text), and `cmd` becomes JSON array text. These are fresh-DB
  schema changes, as before.
- **CLI:** `flyyy deploy <app> … [--env K=V]… [-- <argv>…]`. Everything after `--` is the cmd argv, already split by
  the user's shell, so `-- nginx -g 'daemon off;'` just works. Drop the string `--cmd`, or keep it as a deprecated
  alias that becomes a one-element argv. `flyyy status` shows env names only, never values.
- `// shortcut: env values (secrets included) are stored in Corrosion in plaintext and readable by every agent;
  real secrets need encryption to the machine's worker`.
- **Unmanaged machines** (`lib.sh machine_create`) gain the same two columns (empty env, the argv from the old cmd
  string), so `lifecycle.sh` and `storm.sh` keep working.
- `compat/compat.sh` and `images.txt`: the cmd field becomes shell words (split with `eval set --`, quoted where needed)
  and a new env field (`K=V;K=V`). Keep the format simple.

## Success criteria (all must pass)
1. **Compat re-run** (`compat.sh --all`, plus the old workaround jobs where still relevant):
   - none of the images whose first blocker was C1, C2, C4, C5, C8 or C9 still fails on that cause;
   - write `compat/REPORT-stage10.md`: the new scoreboard against the baseline 22/61, and each still-failing image's new
     first blocker (with console evidence);
   - images that now hit memory, readiness, cgroups or IPv6 are expected; list them, don't fix them here.
2. **Must work (real protocol probe):**
   - redis (PING), valkey (`tini --`), memcached (`version`);
   - node (`node -e` HTTP server), php (`php -S 0.0.0.0:8080`), deno (`tini -- … eval`);
   - dotnet aspnetapp (WorkingDir);
   - postgres with `--env POSTGRES_PASSWORD=…` (psql `select 1`).
   Report the rest of the report's "likely unlocked" list (haproxy, keydb, meilisearch, vault, consul, couchdb, envoy,
   rabbitmq, uptime-kuma, ghost, umami, adminer, clickhouse, mariadb with env) as they come out.
3. **Exact argv and env:** a small test app (e.g. `node -e` or busybox `sh` + `httpd` CGI) returns its argv and env over HTTP.
   - argv with spaces, quotes, `=`, a `--` and 40+ elements arrives byte for byte;
   - env = image Env + app env + `HOSTNAME`/`HOME`, and **no** `pci=off`, `earlycon` or `FLYYY_PORT` leak.
4. **User and WorkingDir:**
   - an image with `User` set (a distroless `nonroot` one, or `USER 1000` in the config) runs as that uid/gid
     (the test app shows `id`);
   - the working directory is the image's;
   - root-only steps (`/etc/hosts`, mounts) still happened.
5. **/dev and /etc:**
   - `/dev/fd/0`, `/dev/stdout`, `/dev/pts` and `/dev/shm` exist and work (shown from the test app);
   - `localhost` and the hostname resolve to `127.0.0.1` with getent or Go;
   - nginx's `/dev/stdout` log path now reaches the console.
6. **PID 1:**
   - a test app that spawns and abandons 20 short-lived children leaves no zombies (it reports `/proc/*/stat` states);
   - SIGTERM to the launcher reaches the app's process group (show it however is simplest, for example a unit test of
     the forwarding plus one in-VM check);
   - the app's exit status is the VM's end (the agent's "exited before its port answered" still fires for an app that exits).
7. **Validation:** bad env names, NUL bytes, oversized env or argv, and too many args → 400, nothing written. A corrupt
   or missing `config.json` → the VM fails with a `flyyy-init:` or `init:` message and the machine fact says
   `exited before its port answered`.
8. `verify.sh --failover`, `deploy.sh`, `lifecycle.sh`, `images.sh`, `storm.sh plain churn` and `workers.sh` pass,
   whoami and echo are deployed by `up.sh` as before, and `down.sh` leaves the host clean.

## Risk points: check these first, in this order, and stop early if one is a blocker
1. **A static aarch64 musl Rust binary from Nix** (`pkgsStatic.rustPlatform.buildRustPackage`) that runs in the guest
   with no `/lib`, from both the initrd and `/newroot/.flyyy`.
2. **The PID 1 hand-off:** `switch_root` into `/.flyyy/flyyy-init` from the overlay upper layer, with `/dev`, `/proc` and
   `/sys` already moved. The socat shim started earlier keeps running, and the launcher must not kill or reap it by
   mistake: it isn't the launcher's child, but check.
3. **`mke2fs -d`** putting `flyyy/config.json` on a fresh 1 GiB scratch (it worked for whole trees in stage 8's
   fallback), and how long that takes vs the current `truncate` + `mke2fs`.
4. **Users:** resolving them from the image's `/etc/passwd` with no NSS (musl, a static binary). Images with no
   `/etc/passwd` and a numeric `User`; images where the user's home or workdir doesn't exist.

## Constraints
- Mark every shortcut as `# shortcut:` / `// shortcut:`. Keep the one-table flake approach.
- New dependencies only by the repo's Rust rule (latest exact version, `default-features = false`, explicit
  `features`, a comment). The guest crate should need at most `serde`/`serde_json` (and maybe `libc`).
- Validation and error handling aren't shortcuts: the config is validated on both sides, and the launcher never
  continues past a failed step (mount, write, setuid, chdir, exec).
- Small unit tests for the non-trivial logic:
  - the agent's config merge (Entrypoint/Cmd/cmd, env override, defaults, `{id}`);
  - the launcher's user spec parsing against a sample passwd/group, and its PATH lookup;
  - API env/argv validation.

## Out of scope
- Per-app memory/CPU (C7), readiness grace and the readiness false positive (C3; report §3), and other kernel options
  (cgroups, IPv6, RTC, inotify).
- Egress and DNS, health checks, graceful guest shutdown over vsock, secrets encryption, image GC, `flyyy ls` GC.
- C17, the unexplained segfaults (nginx, typesense): re-test and report only.

## Deliverables
- `guest/` crate, flake (static build, initrd hand-off, kernel options), agent (config build + scratch population),
  images.rs (WorkingDir/User kept), control plane and CLI (env, argv), schema, adapted `lib.sh`/`compat/`, and
  `compat/REPORT-stage10.md`.
- A "Stage 10" section in `NOTES.md` (≤ ½ page):
  - verdict;
  - compat score before/after;
  - launcher size;
  - boot time impact (create → started for whoami vs stage 9);
  - the top 3 remaining blockers by images affected.
