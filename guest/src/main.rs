//! flyyy-init: the guest's whole initrd (/init), PID 1 from the kernel's start to the VM's end. First the
//! initramfs stage (boot.rs): /dev, the network, the root overlay, the config from the scratch drive, the
//! switch to the new root; and a vsock relay per app port, on threads (relay.rs). Then, in the new root,
//! it prepares `/dev` and `/etc` and runs the image's argv with exactly the configured env, working
//! directory and user, as Docker would. It stays PID 1: it reaps every child, forwards signals to the
//! app's process group, and when the app ends it powers the VM off (`end`), which ends Cloud Hypervisor.
//! Not by exiting: PID 1 exiting panics the kernel, panic=1 resets the VM, and Cloud Hypervisor reboots
//! on a reset instead of exiting.
mod boot;
mod relay;

use std::{
    ffi::CString,
    fmt::Display,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    thread,
};

use nix::mount::MsFlags;
use serde::Deserialize;

const MAX_ITEMS: usize = 1024;
/// Docker's default when the image sets no PATH.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const FORWARDED: [libc::c_int; 6] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    argv: Vec<String>,
    /// `NAME=value`, the app's whole environment.
    env: Vec<String>,
    workdir: String,
    /// Docker's `User`: "", `user`, `user:group`, `uid`, `uid:gid`.
    user: String,
    hostname: String,
    machine_id: String,
    /// The image's volume mount points: created (root, 0755) when missing, as Docker does. They're plain
    /// directories of this boot's overlay, so on the scratch drive.
    #[serde(default)]
    volumes: Vec<String>,
}

fn die(msg: impl Display) -> ! {
    eprintln!("flyyy-init: {msg}");
    end(1)
}

/// The VM's end: power off (ACPI on x86_64, PSCI on arm64), which makes Cloud Hypervisor exit. Nothing to
/// sync: the scratch is thrown away. `code` only matters if the power-off fails, as PID 1's exit status
/// (the kernel then panics, as before).
fn end(code: i32) -> ! {
    // SAFETY: reboot(2) with a valid command; it returns only on failure.
    unsafe { libc::reboot(libc::RB_POWER_OFF) };
    eprintln!("flyyy-init: power off: {}", std::io::Error::last_os_error());
    std::process::exit(code)
}

fn parse_config(raw: &[u8]) -> Result<Config, String> {
    let c: Config = serde_json::from_slice(raw).map_err(|e| format!("config: {e}"))?;
    if c.version != 1 {
        return Err(format!("config: version {}, want 1", c.version));
    }
    if c.argv.is_empty() || c.argv[0].is_empty() {
        return Err("config: empty argv".into());
    }
    if c.argv.len() > MAX_ITEMS || c.env.len() > MAX_ITEMS {
        return Err(format!("config: more than {MAX_ITEMS} args or env vars"));
    }
    let nul = c
        .argv
        .iter()
        .chain(&c.env)
        .chain([&c.workdir, &c.user, &c.hostname, &c.machine_id])
        .any(|s| s.contains('\0'));
    if nul {
        return Err("config: NUL byte in a string".into());
    }
    if let Some(e) = c
        .env
        .iter()
        .find(|e| e.split_once('=').is_none_or(|(k, _)| k.is_empty()))
    {
        return Err(format!("config: env entry {e:?} is not NAME=value"));
    }
    if c.hostname.is_empty()
        || !c
            .hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(format!("config: bad hostname {:?}", c.hostname));
    }
    if !c.workdir.is_empty() && !c.workdir.starts_with('/') {
        return Err(format!("config: workdir {:?} is not absolute", c.workdir));
    }
    Ok(c)
}

fn cstr(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("NUL byte in {s:?}"))
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn mount(src: &str, target: &str, fstype: &str, flags: MsFlags, data: &str) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|e| format!("mkdir {target}: {e}"))?;
    nix::mount::mount(Some(src), target, Some(fstype), flags, Some(data))
        .map_err(|e| format!("mount {fstype} on {target}: {e}"))
}

/// A symlink at `link` unless something is already there (an image may bring its own).
fn link(target: &str, link: &str) -> Result<(), String> {
    if fs::symlink_metadata(link).is_ok() {
        return Ok(());
    }
    symlink(target, link).map_err(|e| format!("ln -s {target} {link}: {e}"))
}

/// What Docker gives every container and devtmpfs doesn't: fd links, a pts instance, /dev/shm.
fn setup_dev() -> Result<(), String> {
    link("/proc/self/fd", "/dev/fd")?;
    for (i, name) in ["stdin", "stdout", "stderr"].iter().enumerate() {
        link(&format!("fd/{i}"), &format!("/dev/{name}"))?;
    }
    mount(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620",
    )?;
    // The pts instance's ptmx, not devtmpfs's (which belongs to no instance).
    let _ = fs::remove_file("/dev/ptmx");
    link("pts/ptmx", "/dev/ptmx")?;
    // shortcut: fixed 64 MiB /dev/shm (Docker's default), not counted against anything.
    mount(
        "shm",
        "/dev/shm",
        "tmpfs",
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        "mode=1777,size=64m",
    )
}

/// `/etc/hosts`, `/etc/hostname` and `/etc/resolv.conf`, as Docker injects them. They replace whatever
/// the image has (a symlink into a missing `/run/...` included) and land in this boot's overlay.
fn setup_etc(hostname: &str) -> Result<(), String> {
    fs::create_dir_all("/etc").map_err(|e| format!("mkdir /etc: {e}"))?;
    let files = [
        (
            "/etc/hosts",
            format!("127.0.0.1\tlocalhost {hostname}\n::1\tlocalhost ip6-localhost ip6-loopback\n"),
        ),
        ("/etc/hostname", format!("{hostname}\n")),
        // No network, so no DNS: a resolver on loopback fails fast (connection refused) instead of
        // timing out, and keeps Go from falling back to [::1]:53.
        (
            "/etc/resolv.conf",
            "# flyyy: this VM has no network and no DNS\nnameserver 127.0.0.1\n".into(),
        ),
    ];
    for (path, content) in files {
        let _ = fs::remove_file(path);
        fs::write(path, content).map_err(|e| format!("write {path}: {e}"))?;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Ids {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
    home: Option<String>,
}

/// Docker's `User` against the image's passwd/group. A numeric uid needs no passwd entry (gid 0 then);
/// a name must exist. Supplementary groups: the primary gid plus every group listing the user.
fn resolve_user(spec: &str, passwd: &str, group: &str) -> Result<Ids, String> {
    let spec = if spec.is_empty() { "0" } else { spec };
    let (u, g) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    // name, uid, gid, home
    let users: Vec<(&str, u32, u32, &str)> = passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((
                *f.first()?,
                f.get(2)?.parse().ok()?,
                f.get(3)?.parse().ok()?,
                *f.get(5)?,
            ))
        })
        .collect();
    // name, gid, members
    let groups: Vec<(&str, u32, Vec<&str>)> = group
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((
                *f.first()?,
                f.get(2)?.parse().ok()?,
                f.get(3)
                    .map_or(vec![], |m| m.split(',').filter(|s| !s.is_empty()).collect()),
            ))
        })
        .collect();
    let entry = match u.parse::<u32>() {
        Ok(uid) => users
            .iter()
            .find(|e| e.1 == uid)
            .copied()
            .map(Ok)
            .unwrap_or(Err(uid)),
        Err(_) => Ok(*users
            .iter()
            .find(|e| e.0 == u)
            .ok_or_else(|| format!("user {u:?} not in the image's /etc/passwd"))?),
    };
    let (uid, name, primary, home) = match entry {
        Ok((name, uid, gid, home)) => (uid, Some(name), gid, Some(home.to_owned())),
        Err(uid) => (uid, None, 0, None),
    };
    let gid = match g {
        None | Some("") => primary,
        Some(g) => match g.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => groups
                .iter()
                .find(|e| e.0 == g)
                .map(|e| e.1)
                .ok_or_else(|| format!("group {g:?} not in the image's /etc/group"))?,
        },
    };
    let mut supplementary = vec![gid];
    if let Some(name) = name {
        for (_, ggid, members) in &groups {
            if members.contains(&name) && !supplementary.contains(ggid) {
                supplementary.push(*ggid);
            }
        }
    }
    Ok(Ids {
        uid,
        gid,
        groups: supplementary,
        home,
    })
}

/// argv[0] as execvp would find it: as is when it has a `/`, else the first executable in PATH.
fn find_exec(arg0: &str, path: &str, is_exec: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if arg0.contains('/') {
        return Some(PathBuf::from(arg0));
    }
    path.split(':')
        .map(|d| Path::new(if d.is_empty() { "." } else { d }).join(arg0))
        .find(|p| is_exec(p))
}

fn is_exec(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The VM's end: the app's exit code, or 128 + its signal, as a shell would report it.
fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

fn env_value<'a>(env: &'a [String], name: &str) -> Option<&'a str> {
    env.iter()
        .find_map(|e| e.strip_prefix(name)?.strip_prefix('='))
}

/// The signals PID 1 waits for (sigwait): SIGCHLD and the ones it forwards. Blocked before any thread
/// starts, so every thread has them blocked and they all come to the waiting main thread.
fn block_signals() -> libc::sigset_t {
    // SAFETY: plain libc calls on a set this function owns.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        for s in FORWARDED {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

/// In the forked child (of a process with threads) only async-signal-safe calls: no allocation, no
/// locks, so no eprintln. `what`, then errno in decimal, with write(2); then _exit.
fn child_fail(what: &[u8], code: i32) -> ! {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let mut digits = [0u8; 12];
    let mut i = digits.len();
    let mut n = errno.unsigned_abs();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    // SAFETY: write(2) and _exit on valid buffers: async-signal-safe.
    unsafe {
        libc::write(2, what.as_ptr().cast(), what.len());
        libc::write(2, b" (errno ".as_ptr().cast(), 8);
        libc::write(2, digits[i..].as_ptr().cast(), digits.len() - i);
        libc::write(2, b")\n".as_ptr().cast(), 2);
        libc::_exit(code)
    }
}

fn main() {
    boot::console().unwrap_or_else(|e| die(e));
    // A panic anywhere (relay threads too) ends the VM like a failed step, not with a kernel panic.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        end(101)
    }));
    let set = block_signals();
    let ports =
        relay::ports(&std::env::var("FLYYY_PORTS").unwrap_or_default()).unwrap_or_else(|e| die(e));
    boot::proc().unwrap_or_else(|e| die(e));
    boot::network().unwrap_or_else(|e| die(e));
    boot::hostname(&std::env::var("FLYYY_MACHINE_ID").unwrap_or_else(|_| "flyyy".into()))
        .unwrap_or_else(|e| die(e));
    // The relays: bound before anything else can fail late, serving from the old root on (they need
    // nothing from any filesystem).
    for port in ports {
        let listener = relay::listen(port).unwrap_or_else(|e| die(e));
        thread::Builder::new()
            .name(format!("vsock:{port}"))
            .spawn(move || relay::serve(listener, port))
            .unwrap_or_else(|e| die(format!("relay :{port}: {e}")));
    }
    let raw = boot::root().unwrap_or_else(|e| die(e));
    let cfg = parse_config(&raw).unwrap_or_else(|e| die(e));
    drop(raw);
    boot::switch().unwrap_or_else(|e| die(e));
    setup_dev().unwrap_or_else(|e| die(e));
    setup_etc(&cfg.hostname).unwrap_or_else(|e| die(e));
    for v in &cfg.volumes {
        // Checked by the agent too; this keeps the launcher from trusting its config.
        if !v.starts_with('/') || v.split('/').any(|c| c == "." || c == "..") {
            die(format!(
                "volume {v:?}: want an absolute path without . or .."
            ));
        }
        fs::create_dir_all(v).unwrap_or_else(|e| die(format!("mkdir volume {v}: {e}")));
    }

    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = fs::read_to_string("/etc/group").unwrap_or_default();
    let ids = resolve_user(&cfg.user, &passwd, &group).unwrap_or_else(|e| die(e));
    let mut env = cfg.env.clone();
    if env_value(&env, "HOME").is_none() {
        let home = ids.home.clone().unwrap_or_else(|| {
            if ids.uid == 0 {
                "/root".into()
            } else {
                "/".into()
            }
        });
        env.push(format!("HOME={home}"));
    }
    let path = env_value(&env, "PATH").unwrap_or(DEFAULT_PATH).to_owned();

    // The app's stdout/stderr are the console. Docker gives a pipe there, which a non-root app can
    // reopen by path (`/dev/stdout`, as nginx-style log configs do); a root-owned 0600 tty it can't,
    // so the app owns the console. The kernel's own console output is unaffected.
    // shortcut: the console is a tty shared with the kernel log, not a per-app log pipe.
    let console = cstr("/dev/console").unwrap_or_else(|e| die(e));
    // SAFETY: a valid path; plain syscall.
    if unsafe { libc::chown(console.as_ptr(), ids.uid, ids.gid) } != 0 {
        die(format!(
            "chown /dev/console to {}:{}: {}",
            ids.uid,
            ids.gid,
            errno()
        ));
    }

    // Docker creates a missing WorkingDir.
    let workdir = if cfg.workdir.is_empty() {
        "/"
    } else {
        &cfg.workdir
    };
    fs::create_dir_all(workdir).unwrap_or_else(|e| die(format!("mkdir {workdir}: {e}")));
    std::env::set_current_dir(workdir).unwrap_or_else(|e| die(format!("chdir {workdir}: {e}")));

    let exe = find_exec(&cfg.argv[0], &path, is_exec)
        .unwrap_or_else(|| die(format!("{}: not found in PATH {path}", cfg.argv[0])));
    let exe_c = cstr(&exe.to_string_lossy()).unwrap_or_else(|e| die(e));
    let argv_c: Vec<CString> = cfg
        .argv
        .iter()
        .map(|a| cstr(a))
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| die(e));
    let env_c: Vec<CString> = env
        .iter()
        .map(|a| cstr(a))
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| die(e));
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
    env_p.push(std::ptr::null());

    let groups: Vec<libc::gid_t> = ids.groups.iter().map(|&g| g as libc::gid_t).collect();
    let exec_failed = format!("flyyy-init: exec {}", exe.display()).into_bytes();
    // SAFETY: libc calls; every pointer outlives its use. The child (of a process with threads) makes
    // only async-signal-safe calls, on what was prepared above.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            die(format!("fork: {}", errno()));
        }
        if pid == 0 {
            // The app: its own session and process group (signals are forwarded to the group),
            // default signal mask, then drop to its user.
            // SIGPIPE back to its default: Rust's runtime ignores it, and an ignored signal survives execve.
            libc::setsid();
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                || libc::setgid(ids.gid) != 0
                || libc::setuid(ids.uid) != 0
            {
                child_fail(b"flyyy-init: switching to the app's uid and gid", 126);
            }
            libc::execve(exe_c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
            child_fail(&exec_failed, 127);
        }
        eprintln!(
            "flyyy-init: started {} (pid {pid}) as uid {} gid {} in {workdir}",
            exe.display(),
            ids.uid,
            ids.gid
        );

        // PID 1: reap everything (orphans get reparented here), forward signals to the app's group,
        // and end with the app.
        loop {
            let mut sig: libc::c_int = 0;
            if libc::sigwait(&set, &mut sig) != 0 {
                continue;
            }
            if sig != libc::SIGCHLD {
                libc::kill(-pid, sig);
                continue;
            }
            loop {
                let mut status: libc::c_int = 0;
                let reaped = libc::waitpid(-1, &mut status, libc::WNOHANG);
                if reaped <= 0 {
                    break;
                }
                if reaped == pid {
                    let code = exit_code(status);
                    eprintln!("flyyy-init: {} exited ({code})", exe.display());
                    end(code);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000::/home/node:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\nbroken line\n";
    const GROUP: &str = "root:x:0:\nnode:x:1000:\ndocker:x:999:node,other\nnogroup:x:65534:\n";

    #[test]
    fn user_specs_resolve_like_docker() {
        let r = |s| resolve_user(s, PASSWD, GROUP);
        assert_eq!(
            r(""),
            Ok(Ids {
                uid: 0,
                gid: 0,
                groups: vec![0],
                home: Some("/root".into())
            })
        );
        assert_eq!(
            r("node"),
            Ok(Ids {
                uid: 1000,
                gid: 1000,
                groups: vec![1000, 999],
                home: Some("/home/node".into())
            })
        );
        assert_eq!(r("node:nogroup").unwrap().gid, 65534);
        assert_eq!(
            r("1000:5").unwrap(),
            Ids {
                uid: 1000,
                gid: 5,
                groups: vec![5, 999],
                home: Some("/home/node".into())
            }
        );
        // A numeric uid without a passwd entry is fine (gid 0, no home); an unknown name isn't.
        assert_eq!(
            r("4242"),
            Ok(Ids {
                uid: 4242,
                gid: 0,
                groups: vec![0],
                home: None
            })
        );
        assert!(r("ghost")
            .unwrap_err()
            .contains("not in the image's /etc/passwd"));
        assert!(r("node:ghosts").unwrap_err().contains("/etc/group"));
        // No passwd at all (scratch images): numeric only.
        assert_eq!(resolve_user("65532:65532", "", "").unwrap().uid, 65532);
    }

    #[test]
    fn exec_lookup_follows_path_like_execvp() {
        let have = |p: &Path| {
            p == Path::new("/usr/local/bin/docker-entrypoint.sh") || p == Path::new("/bin/sh")
        };
        assert_eq!(
            find_exec(
                "docker-entrypoint.sh",
                "/usr/sbin:/usr/local/bin:/bin",
                have
            ),
            Some("/usr/local/bin/docker-entrypoint.sh".into())
        );
        assert_eq!(find_exec("sh", DEFAULT_PATH, have), Some("/bin/sh".into()));
        assert_eq!(find_exec("./app", "/bin", have), Some("./app".into()));
        assert_eq!(find_exec("nope", DEFAULT_PATH, have), None);
    }

    #[test]
    fn config_is_validated() {
        let ok = r#"{"version":1,"argv":["sh","-c","echo a b; --"],"env":["A=1 2","B="],"workdir":"/app","user":"node","hostname":"m-1","machine_id":"m-1"}"#;
        assert!(parse_config(ok.as_bytes()).is_ok());
        for bad in [
            r#"{"version":2,"argv":["x"],"env":[],"workdir":"","user":"","hostname":"h","machine_id":"h"}"#,
            r#"{"version":1,"argv":[],"env":[],"workdir":"","user":"","hostname":"h","machine_id":"h"}"#,
            r#"{"version":1,"argv":["x"],"env":["NOEQUALS"],"workdir":"","user":"","hostname":"h","machine_id":"h"}"#,
            r#"{"version":1,"argv":["x"],"env":["=v"],"workdir":"","user":"","hostname":"h","machine_id":"h"}"#,
            r#"{"version":1,"argv":["x\u0000"],"env":[],"workdir":"","user":"","hostname":"h","machine_id":"h"}"#,
            r#"{"version":1,"argv":["x"],"env":[],"workdir":"rel","user":"","hostname":"h","machine_id":"h"}"#,
            r#"{"version":1,"argv":["x"],"env":[],"workdir":"","user":"","hostname":"h h","machine_id":"h"}"#,
            r#"{"version":1,"argv":["x"],"env":[],"workdir":"","user":"","hostname":"h","machine_id":"h","extra":1}"#,
        ] {
            assert!(parse_config(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn exit_codes_and_env_lookup() {
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGKILL), 128 + libc::SIGKILL);
        let env = vec!["PATH=/x".to_string(), "PATHX=no".into(), "HOME=/h".into()];
        assert_eq!(
            (
                env_value(&env, "PATH"),
                env_value(&env, "HOME"),
                env_value(&env, "USER")
            ),
            (Some("/x"), Some("/h"), None)
        );
    }
}
