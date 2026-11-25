//! The initramfs stage: flyyy-init is the initrd's only file (/init), PID 1 from the kernel's start. It
//! mounts /dev and /proc, sets up the network (no NIC: `lo` and a dummy eth0), mounts the root (an overlay
//! of the read-only EROFS image, vda, and a fresh ext4 scratch, vdb), takes its config from the scratch
//! drive into memory, and switches to the new root by moving it onto / and chrooting (pivot_root doesn't
//! work from an initramfs). Then main.rs goes on in the same process. Any failed step powers the VM off
//! (main.rs `die`), so Cloud Hypervisor exits instead of the VM hanging or rebooting.

use std::{fs, io, os::fd::AsRawFd};

use nix::{
    mount::{mount, MsFlags},
    unistd::{chdir, chroot, sethostname},
};

/// Where the machine agent writes the launch config, on the scratch drive (outside the overlay).
const CONFIG: &str = "/scratch/flyyy/config.json";
const MAX_CONFIG_BYTES: u64 = 1 << 20;
/// eth0's address: a private /32 with no routes. Apps that want a non-loopback address to advertise get
/// one; nothing sent to it leaves the VM (there is no NIC).
// shortcut: the same address in every VM; the app's port is still only reachable over vsock.
const ETH0: [u8; 4] = [10, 0, 0, 2];

fn mnt(
    src: &str,
    target: &str,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|e| format!("mkdir {target}: {e}"))?;
    mount(Some(src), target, Some(fstype), flags, data)
        .map_err(|e| format!("mount {fstype} on {target}: {e}"))
}

fn moved(from: &str, to: &str) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("mkdir {to}: {e}"))?;
    mount(Some(from), to, None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .map_err(|e| format!("move {from} to {to}: {e}"))
}

/// /dev (devtmpfs) and the console as fds 0-2: the initramfs has no /dev/console of its own, so the
/// kernel started this with none.
pub fn console() -> Result<(), String> {
    mnt("dev", "/dev", "devtmpfs", MsFlags::empty(), None)?;
    let c = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/console")
        .map_err(|e| format!("/dev/console: {e}"))?;
    for fd in 0..3 {
        // SAFETY: valid descriptors; plain syscall.
        if unsafe { libc::dup2(c.as_raw_fd(), fd) } < 0 {
            return Err(format!("dup2 console: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// An interface request for `name`.
fn ifreq(name: &str) -> libc::ifreq {
    // SAFETY: ifreq is plain data; all zeroes is a valid value.
    let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, s) in r.ifr_name.iter_mut().zip(name.bytes()) {
        *d = s as libc::c_char;
    }
    r
}

fn sockaddr_in(ip: [u8; 4]) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(ip),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in fits in sockaddr (both 16 bytes), the layout SIOCSIFADDR expects.
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

fn ioctl(
    sock: &std::os::fd::OwnedFd,
    what: &str,
    request: libc::c_ulong,
    r: &mut libc::ifreq,
) -> Result<(), String> {
    // SAFETY: a socket and a valid ifreq for the request. The request's type is an int on musl and an
    // unsigned long on glibc: `as _` fits either.
    if unsafe { libc::ioctl(sock.as_raw_fd(), request as _, r as *mut libc::ifreq) } != 0 {
        return Err(format!("{what}: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// `lo` up; the kernel's dummy0 renamed eth0, with 10.0.0.2/32 (address and netmask set while it's down,
/// so no wider route ever appears) and IPv6 off (no link-local address or route on it), then up.
pub fn network() -> Result<(), String> {
    // SAFETY: plain syscall; the descriptor is owned right away.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(format!("socket: {}", io::Error::last_os_error()));
    }
    // SAFETY: a new descriptor nothing else owns.
    let sock = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    let up = |name: &str| -> Result<(), String> {
        let mut r = ifreq(name);
        r.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        ioctl(&sock, &format!("{name} up"), libc::SIOCSIFFLAGS, &mut r)
    };
    up("lo")?;
    let mut r = ifreq("dummy0");
    for (d, s) in unsafe { r.ifr_ifru.ifru_newname.iter_mut() }.zip(b"eth0") {
        *d = *s as libc::c_char;
    }
    ioctl(&sock, "rename dummy0 to eth0", libc::SIOCSIFNAME, &mut r)?;
    let mut r = ifreq("eth0");
    r.ifr_ifru.ifru_addr = sockaddr_in(ETH0);
    ioctl(&sock, "eth0 address", libc::SIOCSIFADDR, &mut r)?;
    let mut r = ifreq("eth0");
    r.ifr_ifru.ifru_netmask = sockaddr_in([255; 4]);
    ioctl(&sock, "eth0 netmask", libc::SIOCSIFNETMASK, &mut r)?;
    fs::write("/proc/sys/net/ipv6/conf/eth0/disable_ipv6", "1")
        .map_err(|e| format!("eth0 IPv6 off: {e}"))?;
    up("eth0")
}

pub fn hostname(name: &str) -> Result<(), String> {
    sethostname(name).map_err(|e| format!("hostname {name}: {e}"))
}

/// The root: the image read-only, the scratch drive, the overlay of both on /newroot. Returns the launch
/// config, read from the scratch drive into memory and deleted there (it may carry secrets: the app
/// gets them as env, never as a file).
pub fn root() -> Result<Vec<u8>, String> {
    mnt("/dev/vda", "/lower", "erofs", MsFlags::MS_RDONLY, None)?;
    mnt("/dev/vdb", "/scratch", "ext4", MsFlags::empty(), None)?;
    for d in ["/scratch/upper", "/scratch/work"] {
        fs::create_dir_all(d).map_err(|e| format!("mkdir {d}: {e}"))?;
    }
    let meta = fs::metadata(CONFIG).map_err(|e| format!("{CONFIG} (on the scratch drive): {e}"))?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "{CONFIG}: {} bytes, more than {MAX_CONFIG_BYTES}",
            meta.len()
        ));
    }
    let raw = fs::read(CONFIG).map_err(|e| format!("{CONFIG}: {e}"))?;
    fs::remove_file(CONFIG).map_err(|e| format!("rm {CONFIG}: {e}"))?;
    // On disk before the overlay goes up, so the layout is visible from outside (and survives a crash).
    nix::unistd::sync();
    mnt(
        "overlay",
        "/newroot",
        "overlay",
        MsFlags::empty(),
        Some("lowerdir=/lower,upperdir=/scratch/upper,workdir=/scratch/work"),
    )?;
    Ok(raw)
}

/// /dev and /proc moved into the new root, /sys and the root cgroup (the VM is the app's whole machine:
/// writable, apps read their limits from it) mounted there, then the new root moved onto / and chrooted.
pub fn switch() -> Result<(), String> {
    moved("/dev", "/newroot/dev")?;
    moved("/proc", "/newroot/proc")?;
    mnt("sys", "/newroot/sys", "sysfs", MsFlags::empty(), None)?;
    mnt(
        "cgroup2",
        "/newroot/sys/fs/cgroup",
        "cgroup2",
        MsFlags::empty(),
        None,
    )?;
    chdir("/newroot").map_err(|e| format!("chdir /newroot: {e}"))?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .map_err(|e| format!("move the new root onto /: {e}"))?;
    chroot(".").map_err(|e| format!("chroot: {e}"))?;
    chdir("/").map_err(|e| format!("chdir /: {e}"))
}

/// /proc, for the IPv6 sysctl (moved into the new root later).
pub fn proc() -> Result<(), String> {
    mnt("proc", "/proc", "proc", MsFlags::empty(), None)
}
