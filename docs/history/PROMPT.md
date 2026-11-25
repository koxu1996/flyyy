/ponytail full

## Goal
A throwaway feasibility spike: can systemd-nspawn containers each host Firecracker
microVMs whose root disk is a containerd devmapper snapshot of a Docker image
(the fly.io "Docker without Docker" approach, no container runtime in the guest)?
I want a yes/no answer plus the smallest working config that proves it.
This is not a production design.

## Host
- NixOS on Apple M2 (aarch64-linux), flakes, `extra-container` available.
- Firecracker on the host itself is expected to work. Assume that, but verify it
  in step 1 below.
- Everything is aarch64: image variant, kernel, firecracker binary.
- Host kernel uses 16K pages.
- You may run `sudo` yourself (each call needs my fingerprint, so batch the
  privileged steps into a few calls where you can).

## Target topology
host
├── containerd + devmapper snapshotter (one thin pool, loopback-backed)
│   └── whoami image unpacked once → 6 thin snapshots, one per VM
└── 3 × systemd-nspawn workers (worker1..3), `containers.*` via `extra-container`
    └── 2 × Firecracker VMs each → 6 VMs total
        - root disk: that VM's /dev/mapper snapshot device, passed into the worker
        - kernel: aarch64 uncompressed `Image`, built by Nix
        - initrd: static busybox + a `sh` init script, built by Nix. It mounts
          /proc, /sys, /dev and the root disk, does `switch_root`, and execs the
          image's Entrypoint + Cmd
        - size: 1 vCPU, 128 MiB

## IP plan
- workerN gets `10.100.N.0/24`: worker side of the host veth `10.100.N.1`,
  VMs `10.100.N.10` and `10.100.N.11`.
- The host routes each `10.100.N.0/24` via its worker's veth.

## Image → root disk pipeline
1. Nix fetches the image pinned by digest, arm64 variant (`dockerTools.pullImage`)
   → `ctr image import --snapshotter devmapper` (no registry access at runtime).
2. For each VM: `ctr snapshots --snapshotter devmapper prepare <vm> <chainID>`
   → resolve its /dev/mapper device.
3. Read Entrypoint/Cmd/Env from the image config on the host and pass them to the
   init (kernel cmdline after `--` becomes init's argv; good enough here).
4. The rootfs is never modified to inject the init. The init lives in the initrd.

## Success criteria (all must pass)
1. One command (or a short script) sets up the pool, imports the image, creates
   snapshots, and `extra-container create --start` brings up 3 workers and 6 VMs.
2. From the host, `curl` to each of the 6 whoami endpoints returns a response
   showing a distinct hostname/IP.
3. A single `verify.sh` checks 1–2 and exits non-zero on any failure.
4. Teardown (`extra-container destroy`, snapshot removal, pool removal) leaves the
   host clean.

## Constraints
- One `flake.nix` plus as few files as possible. Generate the 3 workers and 6 VMs
  from a loop or function, and run the VMs with one systemd template unit (`firecracker@.service`).
- containerd via `virtualisation.containerd` with devmapper configured in
  `settings`. Pool creation is a oneshot unit ordered before containerd.
- **Never enable PCI**: no `--enable-pci`, virtio-mmio devices only. The guest
  kernel doesn't need PCI support.
- No microvm.nix, no podman/docker in the guest, no NixOS in the guest.
- Static IPs via the kernel `ip=` cmdline, and the hostname too. No network code in the init.
- Mark every shortcut as `# shortcut: <limit>, <when to upgrade>` (e.g. the
  loopback thin pool, Entrypoint passed via cmdline).

## Known risk points: check these first, in this order, and stop early if one is a blocker
1. Firecracker boots one VM **on the host** using a devmapper snapshot device as
   the root disk, plus the Nix-built kernel and initrd (proves the whole guest side
   with no nspawn).
2. The same VM boots **inside one nspawn worker**: `/dev/kvm` plus the dm block
   device passed in (`allowedDevices` by major `block-device-mapper`, since minors
   change; `bindMounts` for the node).
3. `/dev/net/tun` and CAP_NET_ADMIN inside nspawn, so it can create tap interfaces.
4. aarch64 guest kernel: virtio-mmio, virtio-blk, ext4, IP_PNP built in (not as
   modules). The host has 16K pages: start with a 4K-page guest (Firecracker's
   reference config) and switch to 16K only if the guest fails to boot.
5. devmapper snapshotter on NixOS/aarch64: thin pool setup, base image size big
   enough, fs_type ext4.
6. Routing from host → nspawn → tap → VM.

## Out of scope
Orchestration, load balancing, TLS, secrets, persistence, registries/pull at
runtime, monitoring, CI, jailer, x86_64, hardening beyond what's needed to boot.

## Deliverables
- Working code in this repo.
- `NOTES.md` (≤ 1 page): verdict (feasible / feasible-with-caveats / blocked),
  every privilege or device the nspawn workers needed, boot time and memory
  per VM, snapshot creation time, and the top 3 problems for taking this further
  (containerd placement, signals/zombie reaping, pool sizing).
