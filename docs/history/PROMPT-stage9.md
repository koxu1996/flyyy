/ponytail full

## Goal
Stage 9 of the spike (stages 1–8: `PROMPT.md`, `PROMPT-stage2.md`…`PROMPT-stage8.md`, all summarised in `NOTES.md`).
Every Firecracker VM still gets full networking: a tap on a per-worker bridge (`br0`, `10.100.N.0/24`), an IP and MAC
allocated by the machine agent, and `ip=` on the kernel command line. Nothing needs it. The only thing that ever talks
to a VM is its own worker's backhaul, and VMs have no egress anyway.

Replace it with **vsock**:
- **Firecracker's virtio-vsock device**, backed by a Unix socket per VM on the worker (`/run/fc/<id>.vsock`).
- **The backhaul** connects to that socket, does Firecracker's `CONNECT <port>` handshake, and pipes bytes as before.
- **In the guest**, a tiny shim forwards vsock port `<port>` to the app on `127.0.0.1:<port>` (loopback only).

Apps stay unchanged: they still listen on TCP.

The aim is less code and less attack surface:
- no bridge, taps, IP/MAC allocation or `ip=`;
- no L2 between VMs on the same worker (today they can reach each other over `br0`);
- no NIC in the guest at all.

## Target (changes from stage 8 in bold)
host / control / edges: unchanged
worker N
├── corrosion agent, machine agent, backhaul
│   └── **backhaul: `CONNECT <id>` → dial `/run/fc/<id>.vsock`, send `CONNECT <port>\n`, expect `OK …\n`, then pipe**
└── Firecracker VMs: **no network interface; a vsock device (`guest_cid` 3, `uds_path /run/fc/<id>.vsock`)**
    └── guest: **`lo` only; the init starts the shim, vsock `<port>` → `127.0.0.1:<port>`, then `switch_root`s to the app**

## Facts / routing data
- `machine_status.addr` becomes **`vsock:<id>:<port>`**. It still names everything the backhaul needs, so a row can
  point at another machine's VM, as the storm probe rows do (`worker1-p0` → a whoami machine).
- The backhaul allowlist is unchanged in spirit: only ids in its own subscription are dialled.
  - It parses `addr` strictly: the `vsock:` prefix, a machine id in the stage 5 id format (it becomes a path under
    `/run/fc`, so no `/` or `..`), and a port of 1–65535.
  - Bad rows are skipped and logged. Raw `ip:port` targets → 404, as before.
- The edges don't read `addr`: unchanged.

## Guest
- **Kernel:**
  - add `CONFIG_VSOCKETS=y` and `CONFIG_VIRTIO_VSOCKETS=y`;
  - remove `CONFIG_VIRTIO_NET` and `CONFIG_IP_PNP`;
  - keep `CONFIG_NET`/`CONFIG_INET`/`CONFIG_UNIX`, for loopback TCP.
  Keep the "missing option → fail the build" check.
- **Kernel command line:**
  - no `ip=`;
  - the agent adds `FLYYY_MACHINE_ID=<id>` and `FLYYY_PORT=<port>`. Like Fly's `FLY_MACHINE_ID`, they also reach the
    app's environment, since unknown `key=value` params become init's env.
- **Init (initrd):**
  1. `ip link set lo up`, then `hostname "$FLYYY_MACHINE_ID"`. whoami's `Hostname:` line, which the tests read, used to
     come from `ip=`.
  2. Start the shim in the background **before** `switch_root`. It keeps running from the old root, and its binary
     stays mapped after the initramfs is freed.
  3. Mount the overlay as in stage 8, then `exec switch_root`, so the app is still PID 1.
  4. Missing `FLYYY_PORT`, or the shim failing to start → print why and exit, so the kernel panics and Firecracker
     exits (as in stage 8).
- **Shim:** static `socat` from `pkgsStatic.socat` (1.8 has VSOCK), `VSOCK-LISTEN:$FLYYY_PORT,fork,reuseaddr TCP:127.0.0.1:$FLYYY_PORT`.
  If that's a blocker (risk 1), use a ~50-line static Rust binary (musl) doing the same, and say so.
  `// shortcut: one forwarded port per machine (the app's)`.

## Machine agent
- Remove the IP/octet allocation, tap and MAC, and `ip=`. `Local` loses `ip` and `tap`.
- **Firecracker config:**
  - no `network-interfaces`;
  - `"vsock": { "guest_cid": 3, "uds_path": "/run/fc/<id>.vsock" }`. The CID is per VM, so 3 for all.
  - Firecracker wants the socket path not to exist, so remove it before start.
- **Readiness (`started`):** connect to the UDS, `CONNECT <port>`, read `OK`, then make sure the guest side doesn't
  close within ~200 ms. An `OK` alone only proves the shim accepted the connection; an immediate close means its TCP
  connect to the app failed.
  `// shortcut: readiness = vsock handshake + the app accepted a TCP connection; no app-level health check`.
- **Stop/destroy** remove the socket file too (also `ExecStopPost`).
- Facts: `addr = vsock:<id>:<port>`.

## Worker container / units
- `fc-pre` loses the bridge and tap. `ip_forward` = 0 stays (nothing to forward now anyway).
- `fc-post` removes `/run/fc/%i.vsock` instead of deleting a tap.
- Drop `enableTun` and `/dev/net/tun` from the worker container.
- **Socket permissions:** the backhaul runs as `DynamicUser` and can't connect to a root-owned socket. Give it a fixed
  `backhaul` user and group (or keep `DynamicUser` with a static `Group=`). Firecracker's socket must come out
  group-connectable for that group: the `firecracker@` unit's `UMask=0007` plus `/run/fc` owned by `root:backhaul`
  with setgid, or a `chgrp`/`chmod` in an `ExecStartPost` that waits for the socket. Pick one and say why.
  The backhaul gets no other privileges.

## Backhaul (proxy `worker` mode)
- Routes become id → (uds path, port), from `addr` as above. `dial()` → `UnixStream::connect`, write `CONNECT <port>\n`,
  read one line (bounded: at most 64 bytes, 1 s timeout), and require it to start with `OK `. Then
  `copy_bidirectional`, as now. Handshake failure or timeout → 502, logged as now.
- `// shortcut: one vsock connection per request, as with the TCP dial before`.
- Keep the stage 4 log line, with the vsock path in place of the VM address.

## Scripts
- **`verify.sh`:**
  - "no route to 10.100.0.0/16" / "direct `10.100.1.10` fails" become "no `br0` or `tap-*` on any worker, and no
    `network-interfaces` in any `/run/fc/*.json`";
  - backhaul validation also covers a bad `addr` row (`vsock:../x:80`, which must never be dialled);
  - the rest is unchanged.
- **`lifecycle.sh`:**
  - drop the tap checks;
  - "same addr after start" becomes "same `vsock:<id>:<port>`";
  - destroy also checks the `.vsock` file is gone;
  - the orphan test edits `vsock.uds_path` instead of the tap and `ip=`.
- **`storm.sh churn`:** the leak check counts `.vsock` files (= running machines) instead of taps. The probe rows still
  point at whoami machines, through their `vsock:` addrs.
- `lib.sh`, `deploy.sh`, `images.sh` and `workers.sh` only need adapting where they read `addr` or taps.

## Success criteria (all must pass)
1. **No VM networking:**
   - no `br0` or `tap-*` interface on any worker, and no `/dev/net/tun` in the worker containers;
   - Firecracker configs have no `network-interfaces`;
   - inside a guest, only `lo`. Show it indirectly: the guest has no NIC driver, and whoami's output lists only
     `127.0.0.1`/`::1` IPs.
2. **Everything still works:** `verify.sh --failover`, `deploy.sh`, `lifecycle.sh`, `images.sh`, `storm.sh plain churn agent-kill`
   and `workers.sh` pass, with the adaptations above.
3. **Isolation:** two machines on the same worker can't reach each other: they have no shared network. Say how you
   showed it; at least argue it from 1, plus the vsock CID being per VM.
4. **Readiness is honest:** a machine whose app never listens (for example echo with a cmd that makes it exit, or the
   wrong port) never becomes `started`, and its fact says why.
5. **Backhaul safety:** besides stage 4's checks, a `machine_status` row with `addr = vsock:../../etc:80`, or
   `vsock:<other worker's id>:80`, written by the test, is skipped (logged) and never dialled.
6. **Numbers:** latency through edge → backhaul → vsock → VM (`oha -c 1`, 1000 requests) vs stage 4's 0.50 ms avg;
   create → started; shim RSS in the guest if measurable (or say not).
7. `down.sh` leaves the host clean.

## Risk points: check these first, in this order, and stop early if one is a blocker
1. **The shim in the initrd:**
   - `pkgsStatic.socat` builds for aarch64 with VSOCK support;
   - it keeps running across `switch_root`;
   - it reaches the app on `127.0.0.1` with only `lo` up.
   Fallback: a static Rust shim.
2. **Firecracker vsock inside nspawn:** it's userspace (no `/dev/vhost-vsock` needed), but check that the UDS shows
   up, that the host-initiated `CONNECT <port>` works, and how long the handshake takes.
3. **Socket permissions** between Firecracker (root) and the backhaul (unprivileged), without giving the backhaul more rights.
4. **Readiness** with the 200 ms "didn't close" rule: it must not flap for slow-starting apps. Measure the
   create → started difference.

## Constraints
- Mark every shortcut as `# shortcut:` / `// shortcut:`. Keep the one-table flake approach.
- New dependencies: none expected (tokio has `UnixStream`; socat is a Nix package).
- Validation and error handling aren't shortcuts:
  - the backhaul validates `addr` before building a path from it;
  - the handshake read is bounded in size and time;
  - the guest init fails loudly.
- One small unit test for non-trivial logic: `vsock:` addr parsing (good, traversal, bad port, other forms) and the
  handshake reply parsing.

## Out of scope
- Guest egress (it had none in practice; now it has no NIC), and multiple ports per machine.
- Firecracker jailer, and running Firecracker unprivileged.
- Volumes, image GC (stage 8's open problems).

## Deliverables
- Flake (kernel options, initrd with the shim and the new init, worker units without bridge/tap/tun, the backhaul
  user/group and socket permissions), agent and backhaul changes, and the adapted scripts.
- A "Stage 9" section in `NOTES.md` (≤ ½ page):
  - verdict;
  - the shim and permissions choices;
  - numbers (latency vs stage 4, create → started, lines of code removed vs added);
  - the top 3 problems for taking this further.
