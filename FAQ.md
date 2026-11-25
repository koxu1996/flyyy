# flyyy FAQ

Questions asked about flyyy, with answers checked against the code. Each answer stands on its own; the
README, [NOTES.md](NOTES.md) and [docs/DESIGN.md](docs/DESIGN.md) have the full picture.

## Guest VM and kernel

### Why are there separate `guest/kernel-x86_64.config` and `guest/kernel-arm64.config` files?

The guest kernel starts from `allnoconfig`, so every option is off. Three files are added on top
(`nix/packages.nix`):

- `guest/kernel.config` is shared by both arches. It holds what the VM and apps need everywhere:
  virtio-mmio, virtio-blk and vsock, EROFS, overlayfs and ext4, the 8250 serial console, IPv6, inotify,
  cgroup v2, a dummy NIC, and xattrs and ACLs.
- `kernel-<arch>.config` (the one for the build host's arch) adds what each arch needs to boot under
  Firecracker.

The build fails if any line of either file doesn't end up in `.config`. Without that check, `allnoconfig`
would silently drop options whose dependencies aren't met.

The two per-arch files don't mirror each other, and they aren't supposed to. Each one lists only what
that arch's `allnoconfig` leaves out and the guest still needs. Options land in one file and not the other
for three reasons, checked against the Kconfig of Linux 6.18:

1. **The option exists only on that arch.** `X86_MPPARSE`, `HYPERVISOR_GUEST` and `KVM_GUEST` are x86
   options; `ARM64_4K_PAGES` is arm64's. Each file can only hold its own arch's options.
2. **One arch already forces the option on.** The other arch has to ask for it.
   - **SMP:** on arm64 it is `def_bool y`, always on. On x86 it is a question that `allnoconfig`
     answers no, so the x86 file sets `SMP` (and `X86_MPPARSE`, to read the MP table Firecracker writes to
     find the vCPUs).
   - **`ARM_AMBA`:** a hidden option that `config ARM64` itself selects. It is listed in the arm64 file only
     because `RTC_DRV_PL031` depends on it, so the "every option stuck" check states the dependency
     outright. `ARM64_4K_PAGES` is already arm64's default page size; it is listed to pin it.
3. **Firecracker gives each arch different hardware and describes it in a different way.**

| Need | x86_64 under Firecracker | arm64 under Firecracker |
|---|---|---|
| Finding the virtio devices (disk, vsock) | No device tree: the devices come as `virtio_mmio.device=` kernel arguments, so it needs `VIRTIO_MMIO_CMDLINE_DEVICES` | Listed in the device tree (FDT); arm64 always selects `OF`, so it needs nothing extra |
| Finding the 8250 console | Fixed legacy I/O ports, found with no help | Listed in the device tree, so it needs `SERIAL_OF_PLATFORM` (which depends on `OF`, off on x86) |
| Wall clock | No emulated RTC chip; the time comes from KVM's paravirt clock, kvm-clock. It needs `KVM_GUEST`, which sits behind `PARAVIRT`, which sits behind the `HYPERVISOR_GUEST` menu (all off in `allnoconfig`) | An emulated PL031 RTC on the AMBA bus, so it needs `RTC_DRV_PL031` (+ `ARM_AMBA`). arm64 has no kvm-clock equivalent, and its arch timer (always selected) keeps time but doesn't know the date |
| Several vCPUs | `SMP` + `X86_MPPARSE` (see above) | Always on |

Without the clock options, guests start at 1970 on either arch. Commit `bc61901` added the PL031 clock on
arm64 and took the compat run from 40 working images to 51. Both arches share `RTC_CLASS` and
`RTC_HCTOSYS` (from `kernel.config`), which copy an RTC's time into the system clock at boot. That only
does something on arm64; on x86_64 there is no RTC, and kvm-clock sets the time itself.

A 4K-page guest works on a 16K-page host such as the M2: a guest's page size is independent of the host's,
which the first spike (`416a729`) confirmed.

**The kernel image format also differs by arch.** Firecracker boots `Image` on arm64 and the ELF
`vmlinux` (unstripped) on x86_64. Both are installed as `$out/kernel`, so nothing else has to care which
arch it is. The x86_64 fragment came with x86_64 support in commit `5289960`.

### What more guest kernel options could we enable?

Not enabled yet (as of 2026-11-17), and worth adding to the shared `guest/kernel.config`. All three are
yes/no questions in Kconfig, and `allnoconfig` answers no, so they are off on both arches today. None of
them is needed by the 62 compat images (`docs/APP_COVERAGE.md`: 57 supported).

- **`SECCOMP` (which also brings in `SECCOMP_FILTER`):** today a guest app can't install a seccomp filter.
  Apps that sandbox themselves (headless Chromium, OpenSSH's privilege separation, some language runtimes)
  either get an error or run unsandboxed. It is the cheapest piece of the "hardening" item in
  [NOTES.md](NOTES.md): it doesn't isolate VMs from each other (Firecracker and KVM do that), but it lets
  apps protect themselves inside their VM.
- **`HIGH_RES_TIMERS`:** without it, guest timers fire only on the kernel tick, every 4 ms at the default
  250 Hz. A 1 ms `nanosleep` or `epoll_wait` timeout takes up to 4 ms, which adds latency to timer-heavy
  apps (databases, Go and Node runtimes).
- **`NO_HZ_IDLE`** (tickless idle): without it, every vCPU of every idle VM still wakes 250 times a second,
  and each wake-up costs the host CPU time. That grows with VM density on a worker.

To add one: put the line in `guest/kernel.config`. The kernel build's "every option stuck" check
(`nix/packages.nix`) fails if a dependency is missing.

Not worth adding: a virtio RNG (`HW_RANDOM_VIRTIO`). Firecracker isn't given an entropy device
(`proxy/src/agent/vm.rs`), and modern kernels' jitter entropy keeps `getrandom` from stalling at boot.

Some options look missing but are on already: syscalls behind `EXPERT` (`TIMERFD`, `EVENTFD`, `SIGNALFD`,
`AIO`, `IO_URING`, `FHANDLE`, `POSIX_TIMERS`, `SHMEM`) default to y when `EXPERT` is off, as it is in
`allnoconfig`.

### How does flyyy-init (the guest's PID 1) work, and how is its code laid out?

`flyyy-init` is the guest's whole initrd: one static musl binary installed as `/init` (about 715 KiB),
built from the `guest/` crate (`nix/packages.nix`). It is PID 1 from the kernel's start to the VM's end.
It does the job Docker plus a mini-init would do for a container: it builds the root filesystem, sets up
the process exactly as the image asks, relays the app's ports over vsock, reaps zombies and forwards
signals. It replaced a busybox shell script plus socat (stage 10). With the old script, 22 of 61 images
ran; flyyy-init raised that to 30, and later kernel and guest fixes brought it to 57 of 62.

**Its inputs come by two routes:**
- **The kernel command line**, written by the worker's agent (`proxy/src/agent/vm.rs`):
  `console=ttyS0 reboot=k panic=1 FLYYY_MACHINE_ID=<id> FLYYY_PORTS=<p1,p2>`. The kernel passes unknown
  `NAME=value` arguments to PID 1 as environment variables. Only what init needs before the disks are
  mounted goes this way.
- **`/flyyy/config.json` on the scratch drive (vdb).** The agent makes a fresh ext4 scratch image for
  every boot, with `mke2fs -d` from a seed directory (no journal, because the scratch is thrown away).
  The config holds the image's `argv`, `env`, `workdir`, `user`, `hostname`, `machine_id` and `volumes`.
  It is parsed with `deny_unknown_fields` and checked (`parse_config`). The app's argv and env never go
  through the kernel command line, so secrets in the env never show up in `/proc/cmdline`.

**The code: three files, about 1000 lines**

| File | What's in it |
|---|---|
| `guest/src/main.rs` | `main()`, the sequence below. `Config` and its checks; `/dev` and `/etc` setup; `resolve_user` (Docker's `User` forms: `user`, `user:group`, `uid`, `uid:gid`, checked against the image's passwd and group); `find_exec` (PATH lookup like `execvp`); exit-code mapping; the fork/exec and the PID 1 loop. Unit tests at the bottom. |
| `guest/src/boot.rs` | The initramfs stage, done with plain syscalls and ioctls (no `ip` or `mount` binaries): console, `/proc`, network, hostname, the root overlay, and the switch to the new root. |
| `guest/src/relay.rs` | The vsock port relays: parsing `FLYYY_PORTS`, the listeners, and the two-way copy with the half-close rules. Unit tests. |

Dependencies: `serde` and `serde_json` (the config), `libc` (raw fork, exec, signals, ioctls and
`listen`), `nix` (mounts, chroot, hostname, sockets) and `vsock`. The release build uses `opt-level = "s"`,
LTO, `panic = "abort"` and is stripped.

**The boot sequence (`main()`)**
1. `boot::console`: mounts devtmpfs on `/dev` and opens the console as fds 0–2, because the initramfs has
   no `/dev/console` of its own.
2. `block_signals`: blocks SIGCHLD and the signals it forwards (TERM, INT, HUP, QUIT, USR1, USR2) before
   any thread starts. That way they all reach the main thread's `sigwait`.
3. `boot::proc` and `boot::network`: brings `lo` up. The kernel's dummy NIC `dummy0` is renamed `eth0` and
   given `10.0.0.2/32`, with IPv6 off on it. It goes nowhere, but some apps (Cassandra, Alertmanager) need
   a non-loopback address to advertise. The VM has no real NIC.
4. `boot::hostname`: sets the hostname from `FLYYY_MACHINE_ID`.
5. **The relays start before the root exists:** one vsock listener per port, each on its own thread. They
   need no filesystem.
6. `boot::root`: mounts vda (the image as read-only EROFS) on `/lower` and vdb (the ext4 scratch) on
   `/scratch`. It reads `config.json` into memory (at most a fixed size), deletes it from disk, syncs,
   and mounts an overlay (`lower=/lower`, `upper=/scratch/upper`) on `/newroot`.
7. `boot::switch`: moves `/dev` and `/proc` into the new root and mounts sysfs and cgroup2 there. Then it
   moves the new root onto `/` and chroots into it. `pivot_root` doesn't work from an initramfs, so the
   old initramfs files stay hidden underneath instead of being deleted.
8. `setup_dev`: creates `/dev/fd` and the `stdin`/`stdout`/`stderr` links, a private devpts instance with
   its own `ptmx`, and a 64 MiB `/dev/shm` (Docker's default size).
   `setup_etc`: writes `/etc/hosts`, `/etc/hostname` and `/etc/resolv.conf` the way Docker does.
   `resolv.conf` points at 127.0.0.1, so DNS fails fast instead of timing out, because the VM has no
   network. It also creates any missing volume directories, after rejecting paths with `..`.
9. Prepares the process the way Docker would:
   - resolves the user and supplementary groups;
   - sets `HOME` if the image didn't (`/root` for root, `/` otherwise);
   - uses the image's `PATH`, or Docker's default;
   - gives `/dev/console` to the app's uid, so a non-root app can reopen `/dev/stdout` (an nginx-style log
     config);
   - creates the working directory if it's missing and changes into it;
   - finds the program on `PATH`.
10. `fork`. The child calls `setsid`, resets SIGPIPE to its default and clears its signal mask, then calls
    `setgroups`, `setgid` and `setuid`, then `execve`. A failure exits with 126 or 127.
11. **The PID 1 loop:** `sigwait`.
    - A forwarded signal is sent with `kill(-pid, sig)` to the app's whole process group.
    - On SIGCHLD it reaps everything with `waitpid(-1, WNOHANG)`, since orphans are reparented to PID 1.
    - When the app itself exits, init exits with the app's code. PID 1 exiting panics the kernel, `panic=1`
      and `reboot=k` turn that into a reboot, and Firecracker exits on a guest reboot. So the app ending
      ends the VM, and the worker's agent sees it.
    - The app's code is reported shell-style (`exit_code`): an exit status as it is, and a death by
      signal as 128 + the signal number (139 for SIGSEGV, 137 for SIGKILL, often the OOM killer). Init
      prints `flyyy-init: <program> exited (<code>)` on the console, so it ends up in the app's logs. The
      worker never receives the number: it only sees the Firecracker unit's `Result=` (`reconcile.rs`).
      So the log line is where to find why an app died.

**Failing fast:** any error before the exec calls `die()`, which prints `flyyy-init: <msg>` and exits
PID 1. The VM ends that way instead of hanging, and the message lands in the VM's journal on the worker.

**How the relay works (`relay.rs`)**
- The worker's backhaul connects to the VM's vsock on the app's port. For each connection, init connects
  to `127.0.0.1:<port>` inside the guest and copies bytes on two threads, one per direction.
- If nothing listens yet (the app is still starting), the vsock connection is closed right away, as
  socat did.
- **Half-close rules:** the client's half-close is passed on to the app, so the app can still answer
  after its read side ends. When the app closes, both connections are closed fully, because Firecracker's
  vsock doesn't pass a guest's half-close to the host. With only the half-close passed on, a reply that
  ends by closing the connection (no `Content-Length`) never finished at the edge. That bug hung the
  balance test on both arches.

**Gotchas written into the code**
- **The forked child:** the parent has threads (the relays), so the child may only make
  async-signal-safe calls. Its errors are written with `write(2)`, never `eprintln!`.
- **SIGPIPE:** Rust's runtime ignores SIGPIPE, and an ignored signal survives `execve`. So the child
  resets it, or every app would start with SIGPIPE ignored.
- **Nothing on a relay thread may panic:** with `panic = "abort"` in PID 1, a panic ends the VM.
- **Threads come from `thread::Builder`, not `thread::spawn`:**
  - `thread::spawn` panics if the OS can't create a thread, which would kill PID 1 and the VM.
    `Builder::spawn` returns a `Result`, so under load a connection that gets no thread is closed and
    `relay::serve` sleeps 10 ms, then goes on.
- **Dropped connections are logged, at most once a second:** connections the guest itself drops (no
  thread, no descriptor for the relay's second half, a failed `accept`) print
  `flyyy-init: vsock :<port>: <what>: <error>` on the console, which ends up in the VM's journal on the
  worker. Messages in between are only counted, and the count is reported with the next message
  (`RateLimit` in `relay.rs`). The console is a slow serial line, so a flood of messages would stall the
  loop that's failing. The write uses `writeln!` with its error ignored, not `eprintln!`, because
  `eprintln!` panics if stderr fails. Before 2026-11-17 these drops were silent: the worker only saw
  the connection close and the edge returned 502.
  - **Not logged:** a connection to an app port nothing listens on yet. That happens on every boot until
    the app starts listening, and the worker's health check already reports an app that never does.
  - `Builder` also sets the stack size. Each connection gets two threads, one per direction, and each
    reserves 256 KiB (`STACK`), not Rust's default 2 MiB. They only call `io::copy`, and the VM may
    have as little as 128 MiB.
- **`listen(2)` is called directly with a backlog of 1024,** because nix and the vsock crate refuse
  anything over musl's `SOMAXCONN` of 128.
- **ioctl's request type differs:** it is an `int` on musl and an `unsigned long` on glibc, hence
  `as _`.

**Tests:** `guest-test` (the unit tests under musl, including the relay's half-close) and `guest-clippy`.
`tests/guest.py` checks it in a real VM, where PID 1's command line is `/init`. The full write-up is in
`docs/DESIGN.md`, under "The guest's init: one static binary".

### Why does flyyy-init open a vsock listener per app port, not one per VM?

**What the code does:** `main.rs` loops over `FLYYY_PORTS`, and for each declared app port it opens a
dedicated vsock listener on the same port number (`relay::listen(port)`), served by its own accept
thread (`relay::serve`). An app with ports 80 and 5432 gets two listeners, vsock:80 and vsock:5432.

Two levels make it look odd at first:
- **Per VM there is one vsock device:** one Firecracker vsock, one Unix socket on the host,
  `<run_dir>/<machine>.vsock`.
- **Per app port there is one guest listener,** all on that one device. A vsock port works like a TCP
  port on that device.

In flyyy one VM runs one instance of one app, so "per VM" and "per app" are the same thing.

**Addressing:** a vsock address is a pair (CID, port), like (IP address, TCP port), but between a VM and
its host only, with no network involved. The CID would normally pick the VM, but with Firecracker the host
side picks the VM by the Unix socket path instead. So every flyyy guest has CID 3
(`"vsock": { "guest_cid": 3, "uds_path": ... }` in `proxy/src/agent/vm.rs`), and the routing address
`vsock:<machine>:<port>` means "that VM's socket file, then that port".

The vsock port number is the address of the service, and Firecracker provides that addressing for free:

- **How the worker reaches a service** (`proxy/src/vsock.rs`, `dial_vsock`): the backhaul connects to the
  VM's Unix socket, writes `CONNECT <port>\n`, and Firecracker replies `OK <n>\n` once a guest listener on
  that vsock port accepts. After that, the socket is a plain byte stream to the app. Firecracker does the
  demultiplexing by port.
- **The routing data names the same thing:** a placement row's address is `vsock:<machine>:<port>`
  (`backhaul.rs`'s `SUBSCRIPTION`), and the worker's health checks dial the same address (`health.rs`).
  The guest vsock port equals the app's port, and the relay forwards it to `127.0.0.1:<same port>`. No
  mapping table is needed anywhere.

**One listener per VM would mean inventing our own header:** a single vsock port would need a small
protocol in front of every stream to say which app port the connection is for. The guest would have to
parse it, check it and version it. That is reimplementing what `CONNECT <port>` already does, plus more
code in PID 1 that must never panic.

**Per-port listeners also act as an allowlist.** Init listens only on the ports in `FLYYY_PORTS`, which
are the app's declared services. A `CONNECT` to any other port fails the handshake ("nothing on port
N?"). So something the app binds on localhost only (an admin or debug port) can't be reached from the
worker's backhaul.

**The cost is small:** one accept thread per declared port (usually one or two), with the listeners
created before the root switch. Each connection then gets its own two copy threads, the same as it would
with a single listener.

### Could the guest use one vsock listener and pick the port from the data, instead of a listener thread per port?

Yes, it could work, but it's the weaker of the alternatives. "A listener thread per port" combines two
choices that can be changed separately: *how the port is chosen*, and *how many threads serve the
listeners*.

**One listener, port named in the data:** the worker would `CONNECT <one fixed port>` and then send a
header such as `PORT 5432\n`. Init would read it, check it against the allowlist and connect to
`127.0.0.1:5432`.
- **What changes:** a small protocol of our own, added on both sides. All three places that dial a VM
  would send it: `backhaul.rs`, `health.rs` and `agent/vm.rs`'s readiness check. The guest parses it in
  PID 1, which must never panic. The allowlist moves from "which ports have listeners" into code.
- **What it gains:** almost nothing. Firecracker's `CONNECT <port>` already picks the port, and a vsock
  socket can't listen on "every port" anyway (binding to `VMADDR_PORT_ANY` just picks one free port).
  Even with a single listener, init still has to know `FLYYY_PORTS` to check the header. It would save
  one idle accept thread per extra port, and that is all.

**Same addressing, fewer threads (the better option if threads ever matter):**
- **One accept loop over all the listeners:** a single thread `poll`s or `epoll`s the per-port listening
  sockets, so there's one thread no matter how many ports. No protocol change; only `relay::serve`
  changes.
- **Event-driven copying:** today each *connection* takes two threads (256 KiB stacks, `relay.rs`), and
  that, not the accept threads, is what grows under load. One epoll loop doing the copies (optionally
  with `splice` through a pipe) would remove the per-connection threads. The half-close rules stay the
  same. The price is more complex code in PID 1.
- **An async runtime (tokio) in the guest:** it would also work, but it makes the 715 KiB static
  binary bigger and puts a lot of machinery into PID 1.

**Not alternatives:** the app can't listen on vsock itself, because images speak TCP. Giving the VM a
real NIC would remove the relay, but it reverses the decision that VMs have no network card at all.

**Status (2026-11-17):** per-port listeners and two threads per connection. That is fine for one or two
ports and the compat and load tests. The epoll relay is the improvement to make if guest thread counts
under many concurrent connections become a problem.

### Why does the relay connect to `127.0.0.1:<port>`, the same port number it listens on over vsock?

**Why TCP:** apps in images speak TCP and listen the way they would under Docker, for example
`0.0.0.0:80`. They know nothing about vsock. The relay is the bridge between the two: the worker reaches
the VM over vsock, and the app is reached over TCP inside the guest.

**Why 127.0.0.1:** loopback is the only network path inside the VM. There is no NIC; `eth0` is a dummy
device that goes nowhere. An app bound to `0.0.0.0` (or to `[::]`, which also accepts IPv4 by default)
accepts on loopback. So does an app bound to `127.0.0.1` only. That works in flyyy but not with Docker's
published ports, where a localhost-only app can't be reached. Two cases fail:
- an app bound only to `::1`, or to `[::]` with `IPV6_V6ONLY` set, because the relay dials IPv4 only.
  This is a known gap as of 2026-11-17. A realistic trigger is Node 17+ with `listen(port, "localhost")`:
  the guest's `/etc/hosts` maps `localhost` to both `127.0.0.1` and `::1`, so Node usually binds `::1`.
  The connection is closed without a log line, and the health check fails. The proposed fix, not
  implemented: in `relay::serve`, dial `127.0.0.1:<port>` and fall back to `[::1]:<port>` if refused.
  That costs one extra loopback attempt, only when the first is refused. The guest kernel has IPv6 and
  `lo` has `::1`; only `eth0` has IPv6 turned off. Public IPv6 is a separate matter: HTTP apps are
  reachable over IPv6, since the edges listen on `[::]:443`, but TCP services' dedicated app IPs are
  IPv4 only (README).
- an app bound only to eth0's `10.0.0.2`, which is rare.

**Why the same port number:** a service's `port` is defined as the port *in the VM*
(`proxy/src/services.rs`, `Service.port`), which is Docker's container port. The vsock port is the same
number, so nothing needs to map between them anywhere: the placement address `vsock:<machine>:<port>`,
`FLYYY_PORTS`, the health checks and the relay all use one number. The port the outside world sees is a
separate setting:
- HTTP services are published by Host on the edges' shared 443;
- TCP services use `edge_port`, on the app's own IP. When `edge_port` is unset it defaults to the VM port.

**How it looks end to end:** client → edge (public port) → worker backhaul → `CONNECT <port>` on the VM's
vsock socket → flyyy-init's listener on vsock `<port>` → `127.0.0.1:<port>` → the app.

### Why is `relay()` generic over a `Duplex` trait?

So the relay's tricky part, the half-close and full-close rules, can be unit-tested without vsock.

- **In production** it relays a `VsockStream` (the worker's connection) to a `TcpStream` (the app):
  `relay(v, tcp)` in `relay::serve`.
- **In the tests** both ends are `TcpStream` pairs on loopback, standing in for vsock
  (`relays_both_ways_and_passes_half_closes_on`, `the_apps_end_closes_both_sides`). The Nix build sandbox
  where `guest-test` runs has no vsock device, so a test can't create a real `VsockStream`. These tests
  guard the bug that once hung the balance suite on both arches: an answer without `Content-Length`
  never finishing at the edge.

**What the trait provides:** `relay` needs four things from each stream.
- `Read` and `Write`, for `io::copy`.
- `split()`: a second handle to the same socket, so each direction can run on its own thread. It wraps
  the `try_clone` method.
- `shut(how)`: half-close or full close. It wraps the `shutdown` method, and ignores its error, because
  a failed shutdown on a dying connection isn't worth reporting.

`try_clone` and `shutdown` are ordinary methods that each type defines for itself, not part of a shared
std trait. So `Duplex` names them once, with an `impl` for each type. The bounds `Send + 'static` let a
stream move into the second thread, and `Sized` lets `split` return `Self`.

**Cost:** none at run time. Generics are compiled into a separate copy of the function for each pair of
types actually used (`(VsockStream, TcpStream)` in the binary, `(TcpStream, TcpStream)` only in tests),
so there is no dynamic dispatch. `Box<dyn ...>` would have worked too, at the price of an allocation and
virtual calls.

### Where is a VM's own config (`config.json`), and how does it get into the guest?

It moves through four stages:

1. **Built by the worker's agent:** `guest_config()` in `proxy/src/agent/guest.rs` builds a `GuestConfig`
   with Docker's rules:
   - `argv` is the image's Entrypoint plus the machine's `cmd`, or else the image's Cmd;
   - `env` is the image's Env, overridden by the machine's env, plus `HOSTNAME`. If neither sets `PATH`,
     Docker's default (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`) is added, as
     Docker does for images without one, such as `FROM scratch` images. This was checked against moby's
     source on 2026-11-17: `CreateDaemonEnvironment` in `daemon/container/container.go` starts the
     env with `PATH=` plus `oci.DefaultPathEnv` (the same string, in `daemon/pkg/oci/defaults.go`) and
     `HOSTNAME`, then applies the image's and the user's env on top, replacing keys that match. flyyy-init has the same constant
     (`guest/src/main.rs`) as a fallback for its own program lookup only. With the current agent that
     fallback never runs, but the two copies must stay the same;
   - `workdir`, `user` and `volumes` come from the image;
   - `hostname` and `machine_id` are the machine id.

   The config is stored in the machine's state file, `/var/lib/flyyy/<id>.json` (the `Local` struct; the
   directory is `0700`). So the env, secrets included, lives on the worker in that file.
2. **Written onto every boot's fresh scratch drive** (`proxy/src/agent/vm.rs`):
   - The agent writes it to `<state>/<id>.seed/flyyy/config.json` (mode `0600`).
   - It runs `mke2fs -t ext4 -O ^has_journal -d <seed>` to make `<state>/<id>.scratch` (1 GiB, sparse)
     with the file inside, then deletes the seed directory.
   - Firecracker attaches that image as the VM's second drive, `vdb`.
3. **Read by flyyy-init in the guest:**
   - `vdb` is mounted at `/scratch`, so the file is at **`/scratch/flyyy/config.json`** (`CONFIG` in
     `guest/src/boot.rs`, limit 1 MiB).
   - Init reads it into memory, **deletes it**, and runs `sync` before mounting the overlay.
   - It is outside the overlay's `upper/` directory, so it never appears in the app's root filesystem.
4. **Given to the app only as its process setup:** argv, env, working directory and user. The app never
   sees the file, so secrets passed as env aren't left in a file the app or an attacker in the VM could
   read. Nothing of the app's config goes on the kernel command line, which only carries
   `FLYYY_MACHINE_ID` and `FLYYY_PORTS`.

**Why a disk file and not the kernel command line:** the command line is limited in size, has awkward
quoting, and is visible in `/proc/cmdline`. An early version passed Entrypoint, Cmd and Env that way
and it was "full of hacks" ([NOTES.md](NOTES.md), stage 10).

**Limits on the config, layer by layer:**
- **The agent** (`proxy/src/agent/spec.rs`) limits what the user sets: `cmd` to 64 args of at most
  4 KiB each, and `env` to 64 vars and 32 KiB. The image's volumes are capped at 64. The image's
  Entrypoint, Cmd and Env come from the registry, and the agent doesn't limit them.
- **flyyy-init doesn't trust the config** and checks it again (`parse_config`): `version` must be 1;
  argv must be non-empty; argv and env are each at most 1024 items (`MAX_ITEMS`, a generous sanity cap
  over image plus user); there are no NUL bytes; env entries are `NAME=value`; the hostname is valid;
  `workdir` is absolute; the file is at most 1 MiB. A failure exits PID 1 with `config: ...`, which ends
  the VM with a clear message.
- **The kernel's** `execve` limit (`E2BIG`, about 2 MiB of args plus env) is the hard ceiling behind
  all of these.

**Lifetime:** the scratch image is created again at every start and removed at stop. The state file
`<id>.json` lives as long as the machine is placed on that worker.

**Stale comment:** `GuestConfig`'s doc comment still says the guest reads `/.flyyy/config.json`. That
path is from before the change that stopped copying the config into the overlay (`docs/DESIGN.md`, "The
guest's init"); the real path is `/scratch/flyyy/config.json` during boot.

### The VM has `eth0` with 10.0.0.2. Isn't it supposed to have no network?

It has no network. `eth0` is not a NIC: it is the kernel's **dummy** interface (`CONFIG_DUMMY=y` in
`guest/kernel.config`). It accepts an address and drops every packet sent through it, with no wire
behind it.

**No real NIC exists:**
- The Firecracker config the agent writes (`proxy/src/agent/vm.rs`) has no `network-interfaces`: no tap
  device, no virtio-net.
- The guest kernel has no virtio-net driver.
- The only ways in and out of the VM are vsock (the app's ports, health checks) and the block devices.

**Why it exists:** some apps refuse to start, or misbehave, with loopback as their only address. They
look for a non-loopback interface to advertise to peers or bind to; Cassandra and Alertmanager were the
cases in the compat run. The dummy gives them one, and it goes nowhere.

**How it's set up** (`boot::network` in `guest/src/boot.rs`, with ioctls at boot):
- `dummy0` is renamed `eth0`, the name apps and scripts expect;
- it gets `10.0.0.2` with a `/32` netmask. Both are set while the interface is down, so no wider subnet
  route ever appears;
- IPv6 is turned off on it, so it has no link-local address or route;
- then it's brought up.

**What an app sees:** traffic to `10.0.0.2` itself works; the kernel delivers it locally, like loopback.
There is no default route, so a connection to any other address fails at once with "Network is
unreachable" instead of timing out. DNS fails fast too, because `/etc/resolv.conf` points at 127.0.0.1.
Every VM uses the same 10.0.0.2. The address never leaves the VM, so it can't clash.

Not to be confused with the **WireGuard mesh** (`flyyy0`, `10.250.0.0/16`). That one links the hosts
(control plane, workers, edges), never the VMs.

### Can apps reach the outside world (the internet, DNS, other apps)?

No. As of 2026-11-17, VMs have **no outbound connectivity**: no internet, no DNS, and no app-to-app
traffic. Traffic only comes *in*: edge → worker backhaul → vsock → the app. This was a deliberate choice
in stage 9: a VM with no network has less to configure and less to attack ([NOTES.md](NOTES.md)). The
result inside the guest:
- a connection to anything other than loopback or the dummy `10.0.0.2` fails with "Network is
  unreachable";
- name lookups fail fast, because `/etc/resolv.conf` points at 127.0.0.1, where nothing listens.

**What it rules out today:**
- apps that call external APIs (payments, OAuth, webhooks out, sending email);
- apps that pull data or updates at runtime;
- an app that uses a database run as another flyyy app. `docs/APP_COVERAGE.md` skips umami for this
  reason: "needs an external database (DATABASE_URL); apps can't reach each other yet".

Self-contained servers work: web servers, and databases or caches that clients reach from outside
through the edges. That covers most of the compat run.

**This gap isn't in NOTES.md's "road to production" list,** but a real platform needs it.

**Possible designs (proposals only, nothing implemented):**
1. **Egress over vsock**, keeping the no-NIC design. A guest can open a vsock connection to the host
   (CID 2, port P); Firecracker forwards it to the Unix socket `<uds_path>_P` on the worker. flyyy-init
   would run a small HTTP/HTTPS (CONNECT) or SOCKS proxy on loopback and tunnel it out that way. A
   worker-side egress proxy would then apply policy: allowlists, per-org limits, logging.
   - DNS: either the proxy resolves names (HTTP CONNECT and SOCKS5 carry host names), or a small DNS
     forwarder on 127.0.0.1:53 sends queries out the same way.
   - Catch: apps must use `HTTP_PROXY`/`HTTPS_PROXY`, which many do but not all. Raw TCP to arbitrary
     hosts doesn't work without more machinery.
2. **A real NIC** (tap + virtio-net) with NAT and nftables on the worker, like most VM platforms. Every
   app works unchanged. But it brings back per-VM addressing, firewalling and guest network setup, the
   complexity stage 9 removed. Isolation between VMs would then depend on the firewall rules.
3. **App-to-app over the mesh** (like Fly.io's private 6PN network): private addresses per org, reachable
   only by that org's apps. It needs either option 2's NIC, or vsock forwarding like option 1, with
   names from Corrosion.

Option 1 fits the current design best: the guest stays network-less, and every outbound connection goes
through one place on the worker that applies policy.

**Expected future need (the maintainer's view, 2026-11-17):** flyyy will probably need a 6PN-like private
network. Option 3 is the likely direction for app-to-app traffic, such as a web app using a database
deployed as another flyyy app. Fly.io's 6PN gives:
- every VM a private IPv6 address in its organization's network;
- isolation between organizations;
- `<app>.internal` DNS names;
- a way for developers to join from their laptop over WireGuard.

flyyy already has parts of this: the host WireGuard mesh, organizations, and Corrosion for service
names. The open design question is how traffic gets into and out of a VM: real NICs (option 2), or
per-org connections forwarded over vsock by the worker (as in option 1).

### Why EROFS for the image, and not squashfs?

**What the repo records** (stage 8: [NOTES.md](NOTES.md), `docs/history/PROMPT-stage8.md`): EROFS was
chosen to replace containerd devmapper snapshots, which were fixed-size ext4 disks. The comparison was
against ext4, not squashfs. Its reasons:
- **Built straight from a tar stream:**
  `crane export <ref> - | mkfs.erofs --tar=f <digest>.erofs`. There is no unpacked copy on disk, no
  mount, no block device and no root. It runs as an unprivileged user, and the owners, modes, symlinks,
  xattrs and the sticky bit come from the tar headers.
- **It sizes itself from the content,** so there's no disk size to guess and no resize step. It is also
  smaller: 11.6 MiB for whoami and 8.3 MiB for echo, against 31 MiB each as ext4 sized from the tar.
- **One file per image digest per worker,** cached as `<state>/images/<digest>.erofs` (mode 0444), checked
  with `fsck.erofs` and shared read-only by every VM of that image. Firecracker attaches it as `vda`.
- **Uncompressed for now:** no `-z`, a recorded shortcut ("try lz4 when disk matters more than CPU").

**Why EROFS over squashfs in general** (not recorded in the repo; general knowledge of the two
filesystems):
- **Built for random reads:** EROFS is block-aligned, and uncompressed it is read in place with no
  decompression step. squashfs is always compressed, in blocks, so a small random read can decompress a
  whole block. Container workloads (many small files, random access at startup) favor EROFS.
- **It is the container-image filesystem in Linux:** composefs, containerd's EROFS snapshotter,
  Nydus and Android use it, and its tar import (`--tar=f`) is part of mainline `erofs-utils`. squashfs can
  be built from tar too (`sqfstar`, or `tar2sqfs` from squashfs-tools-ng), so tar import alone wasn't the
  deciding point.
- **Room to grow:** lz4 or zstd compression, deduplication, and sharing pages between VMs or images are
  available in EROFS without changing the layout.

squashfs would have worked. Nothing in flyyy depends on EROFS-only features today, only on a read-only
filesystem with xattrs that overlayfs accepts as its lower layer. The kernel options are `CONFIG_EROFS_FS`
plus `_XATTR`, `_POSIX_ACL` and `_SECURITY`, needed for file capabilities in images (commit `18008d6`).

### Which signals does flyyy-init forward to the app, and why only those?

It forwards six: **TERM, INT, HUP, QUIT, USR1, USR2** (`FORWARDED` in `guest/src/main.rs`). PID 1 blocks
them, together with SIGCHLD, before any thread starts. It waits for them with `sigwait` and sends each one
to the app's whole process group (`kill(-pid, sig)`). SIGCHLD is handled by init itself: it reaps zombies.

**Why those six:** they are the signals people send on purpose to control a server: stop
(TERM, INT, QUIT), reload (HUP), and app-defined actions (USR1, USR2, for example log reopening or
debug dumps). The repo doesn't record the choice. The other signals fall into these groups:
- **Can't be caught at all:** SIGKILL, SIGSTOP. Nobody can forward them.
- **Generated for a specific process or thread by the kernel:** SIGSEGV, SIGBUS, SIGFPE, SIGILL,
  SIGPIPE, SIGALRM and so on. Forwarding one sent to PID 1 would be wrong.
- **Terminal and job control:** SIGWINCH, SIGTSTP, SIGTTIN, SIGTTOU, SIGCONT. The console is a serial
  log, not an interactive terminal.

This is narrower than tini or dumb-init, which forward every catchable signal except the synchronous
ones. Not forwarded here, though an app could use them: SIGWINCH, SIGPWR, SIGURG and the real-time
signals. One of these reaches init's default behavior, which for PID 1 means it is ignored.

**In practice the forwarding is dormant today:**
- **Nothing outside the VM can signal the guest.** The agent stops a machine with `systemctl stop` on
  its transient unit, which sends SIGTERM to the *Firecracker process on the worker* and ends the VM
  from outside. The app gets no signal and has no chance to shut down cleanly (`proxy/src/agent/vm.rs`,
  "shortcut: stop = SIGTERM, no guest shutdown"; [NOTES.md](NOTES.md), connection draining).
- So signals only reach PID 1 from inside the VM, for example the app or its helper processes running
  `kill -HUP 1`.

The forwarding is ready for graceful shutdown when it comes. A future stop path would get a request
into the guest, for example a control message over vsock or Firecracker's `SendCtrlAltDel`, which is
x86 only; the kernel turns Ctrl-Alt-Del into SIGINT for PID 1. Init would forward SIGTERM or SIGINT to
the app, wait out a grace period, and only then kill the VM.

### Do apps have volumes or any persistent storage?

No. As of 2026-11-17, **nothing an app writes survives a stop or restart**. `proxy/src/agent/vm.rs` says
so: "shortcut: fixed 1 GiB scratch, no per-app size; nothing persists across a stop (volumes are out of
scope)".

**What `cfg.volumes` actually is:** the image's own `VOLUME` declarations, from the `Volumes` key of the
image config (`proxy/src/images.rs`). Postgres, for example, declares `/var/lib/postgresql/data`.
- Under Docker, each declared path gets an anonymous volume mounted there.
- In flyyy, the agent passes the paths on (at most 64, checked by `volume_ok`), and flyyy-init only
  **creates the directories** (root, 0755) if they're missing. Init checks the paths again itself.
- Those directories are ordinary directories in the overlay, so their contents live on that boot's
  scratch drive.

**Where an app's writes go:** the overlay's upper layer, `/scratch/upper` on the scratch drive (`vdb`).
That is a 1 GiB sparse ext4 image (`SCRATCH_SIZE`), made fresh at every start and deleted at every
stop. The image itself (`vda`, EROFS) is read-only. So:
- a database deployed on flyyy (Postgres, Redis, MongoDB in the compat run) works, but starts empty
  after every restart;
- 1 GiB is the app's total writable space, `/tmp` included unless the app mounts its own tmpfs;
- a user can't attach storage: there is no volume setting in the machine spec.

**Persistent volumes would need:**
- a per-machine (or per-volume) disk image kept across stops, attached as another Firecracker drive and
  mounted at the declared path;
- placement that keeps a machine on the worker holding its data, or replication or snapshots to move it;
- a clean guest shutdown, so ext4 isn't cut off mid-write. Today stop is a SIGTERM to Firecracker, so
  the code notes "ext4 replays its journal on next boot; add a vsock/serial poweroff path if VMs keep
  state".
