//! A machine's VM: its local record, scratch drive and Cloud Hypervisor unit (a `systemd-run --user` unit).

use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};

use tokio::io::AsyncReadExt;

use super::{
    guest::guest_config,
    spec::Spec,
    sys::{remove, run, systemctl, unit, NOT_LOADED},
    Agent, Local,
};
use crate::{
    images::ImageConfig,
    util::write_atomic,
    vsock::{self, dial_vsock},
};

/// Each running machine's scratch drive, made fresh on every start.
// shortcut: fixed 1 GiB scratch, no per-app size; nothing persists across a stop (volumes are out of scope).
pub(crate) const SCRATCH_SIZE: &str = "1G";

/// How long a started machine's app gets to accept its first connection.
pub(crate) const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The app accepted a connection: the vsock handshake succeeded (the guest shim took it) and the shim
/// didn't hang up within 200 ms (it does at once when its TCP connect to the app fails). Data or a
/// still-open connection both count as accepted.
/// First the socket is opened to the agent's group (0770), for the backhaul: Cloud Hypervisor sets
/// umask 077 itself, whatever the unit's UMask, so it makes the socket 0700. Before the machine is
/// started, so the backhaul is never sent to a socket it can't open; again on every check until then
/// (and after an agent restart), which is harmless.
// shortcut: readiness = vsock handshake + the app accepted a TCP connection; no app-level health check.
pub(crate) async fn ready(socket: &str, port: u16) -> Result<(), String> {
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o770)).map_err(|e| format!("chmod {socket}: {e}"))?;
    }
    let mut s = dial_vsock(socket, port).await?;
    let mut b = [0u8; 1];
    match tokio::time::timeout(Duration::from_millis(200), s.read(&mut b)).await {
        Err(_) | Ok(Ok(1..)) => Ok(()),
        Ok(Ok(0)) => Err("the guest closed the connection (nothing listening on the port?)".into()),
        Ok(Err(e)) => Err(e.to_string()),
    }
}

/// Cloud Hypervisor's arguments for a machine's VM. Every device is virtio-pci: the image (read-only) and
/// the scratch, in that order (vda, vdb), and one vsock. No NIC, and no virtio console: the serial (8250
/// on x86_64, PL011 on arm64) is the console, on stdout (so in the unit's journal).
/// Only what the initrd itself needs goes on the kernel cmdline (hostname, the vsock shims' ports); the
/// app's argv and env are in config.json, on the scratch.
fn ch_args(kernel: &str, initrd: &str, local: &Local, scratch: &str, vsock: &str, ports: &str) -> Result<Vec<String>, String> {
    // Cloud Hypervisor's option values are comma-separated key=value lists: a path can't hold a comma.
    for path in [kernel, initrd, &local.image_file, scratch, vsock] {
        if path.contains(',') {
            return Err(format!("{path}: Cloud Hypervisor can't take a path with a comma"));
        }
    }
    let console = if cfg!(target_arch = "aarch64") { "ttyAMA0" } else { "ttyS0" };
    let id = &local.spec.id;
    Ok([
        "--kernel",
        kernel,
        "--initramfs",
        initrd,
        "--cmdline",
        &format!("console={console} reboot=k panic=1 FLYYY_MACHINE_ID={id} FLYYY_PORTS={ports}"),
        "--cpus",
        &format!("boot={}", local.spec.cpus),
        "--memory",
        &format!("size={}M", local.spec.memory),
        // sparse=off: the raw backend can't punch holes anyway, and says so on every start otherwise.
        "--disk",
        &format!("path={},readonly=on,image_type=raw,sparse=off", local.image_file),
        &format!("path={scratch},image_type=raw,sparse=off"),
        // The CID is per VM (each has its own socket), so every guest is 3.
        "--vsock",
        &format!("cid=3,socket={vsock}"),
        "--serial",
        "tty",
        "--console",
        "off",
    ]
    .map(String::from)
    .to_vec())
}

impl Agent {
    pub(crate) fn read_locals(&self) -> Result<BTreeMap<String, Local>, String> {
        let mut out = BTreeMap::new();
        let dir = &self.dirs.state;
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().is_some_and(|e| e == "json") {
                let local: Local = std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                out.insert(local.spec.id.clone(), local);
            }
        }
        Ok(out)
    }

    /// Ids of `fc-<id>` units that are active, activating or deactivating.
    pub(crate) async fn running(&self) -> Result<HashSet<String>, String> {
        let out = systemctl(&["list-units", "--all", "--plain", "--no-legend", "--full", "fc-*"]).await?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let id = f.first()?.strip_prefix("fc-")?.strip_suffix(".service")?;
                matches!(f.get(2), Some(&("active" | "activating" | "deactivating"))).then(|| id.to_owned())
            })
            .collect())
    }

    /// Persist the local state. Only called once the image is ready; nothing to allocate (no network).
    pub(crate) fn create(&self, spec: &Spec, image_file: &str, image: &ImageConfig) -> Result<Local, String> {
        let local = Local { spec: spec.clone(), port: spec.port, config: guest_config(spec, image)?, image_file: image_file.into() };
        let json = serde_json::to_vec(&local).map_err(|e| e.to_string())?;
        write_atomic(&format!("{}/{}.json", self.dirs.state, spec.id), &json)?;
        Ok(local)
    }

    /// Make a fresh sparse scratch drive, write the VM's launch record (tmpfs, so on every start) and
    /// start the unit. Disk 1 (vda) is the shared image (read-only), disk 2 (vdb) the scratch; the guest
    /// init stacks them with overlayfs.
    pub(crate) async fn start(&mut self, local: &Local) -> Result<(), String> {
        let id = &local.spec.id;
        let (dir, run_dir) = (&self.dirs.state, &self.dirs.run);
        if !std::path::Path::new(&local.image_file).exists() {
            return Err(format!("image file {} is gone", local.image_file));
        }
        let scratch = format!("{dir}/{id}.scratch");
        remove(&scratch)?;
        run("truncate", &["-s", SCRATCH_SIZE, &scratch]).await?;
        // The launcher's config goes onto the fresh scratch, as /flyyy/config.json (outside the
        // overlay's upper/): mke2fs -d fills the new filesystem from a directory.
        let seed = format!("{dir}/{id}.seed");
        let _ = std::fs::remove_dir_all(&seed);
        std::fs::create_dir_all(format!("{seed}/flyyy")).map_err(|e| format!("{seed}: {e}"))?;
        let config = serde_json::to_vec(&local.config).map_err(|e| e.to_string())?;
        std::fs::write(format!("{seed}/flyyy/config.json"), config).map_err(|e| format!("{seed}: {e}"))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(format!("{seed}/flyyy/config.json"), std::fs::Permissions::from_mode(0o600));
        }
        // No journal: the scratch is thrown away at every stop, so a journal would only add writes.
        let made = run("mke2fs", &["-q", "-F", "-t", "ext4", "-O", "^has_journal", "-d", &seed, &scratch]).await;
        let _ = std::fs::remove_dir_all(&seed);
        made?;
        // Cloud Hypervisor creates the vsock socket and wants the path free.
        let vsock = format!("{run_dir}/{id}.vsock");
        remove(&vsock)?;
        let ports: Vec<String> = local.spec.services().iter().map(|s| s.port.to_string()).collect();
        let args = ch_args(&self.kernel, &self.initrd, local, &scratch, &vsock, &ports.join(","))?;
        std::fs::create_dir_all(run_dir).map_err(|e| format!("{run_dir}: {e}"))?;
        // The launch record: exactly the unit's arguments, for whoever wants to see how a VM was started
        // (Cloud Hypervisor takes no config file; the unit gets them on its command line).
        let record = serde_json::to_vec(&args).map_err(|e| e.to_string())?;
        write_atomic(&format!("{run_dir}/{id}.json"), &record)?;
        // A failed earlier run keeps its unit loaded (for its Result); the name must be free.
        let _ = systemctl(&["reset-failed", &unit(id)]).await;
        // UMask 0007 for what the unit makes; Cloud Hypervisor's own umask (077) wins for the vsock
        // socket, which `ready` opens to the group (0770) for the backhaul.
        // shortcut: stop = SIGTERM, no guest shutdown, so ext4 replays its journal on next boot;
        // add a vsock/serial "poweroff" path if VMs keep state.
        let unit_args = [
            "--user",
            "--unit",
            &unit(id),
            "--description",
            &format!("flyyy VM {id}"),
            "-p",
            "UMask=0007",
            // Its CPU and memory, for the app's metrics (usage.rs).
            "-p",
            "CPUAccounting=yes",
            "-p",
            "MemoryAccounting=yes",
            "--",
            "cloud-hypervisor",
        ];
        let argv: Vec<&str> = unit_args.iter().copied().chain(args.iter().map(String::as_str)).collect();
        run("systemd-run", &argv).await?;
        self.starting.insert(id.clone(), Instant::now());
        Ok(())
    }

    /// Stop the unit, then drop its scratch drive: nothing a machine writes survives a stop.
    pub(crate) async fn stop(&mut self, id: &str) -> Result<(), String> {
        self.starting.remove(id);
        self.ready.remove(id);
        // A transient unit is gone once it exits: not loaded means already stopped.
        match systemctl(&["stop", &unit(id)]).await {
            Err(e) if e.code != Some(NOT_LOADED) => return Err(e.into()),
            _ => {}
        }
        remove(&vsock::socket(&self.dirs.run, id))?;
        remove(&format!("{}/{id}.scratch", self.dirs.state))
    }

    /// Stop, then remove everything the machine had here. Each step tolerates "already gone",
    /// so a half-done destroy (agent killed mid-way) finishes on the next round.
    pub(crate) async fn destroy(&mut self, id: &str) -> Result<(), String> {
        self.stop(id).await?;
        let _ = systemctl(&["reset-failed", &unit(id)]).await;
        remove(&format!("{}/{id}.json", self.dirs.run))?;
        remove(&format!("{}/{id}.json", self.dirs.state))
    }
}
