//! Helpers around processes, files and clocks.

use tokio::process::Command;

use crate::util::now_ms;

/// A command that didn't start, or exited non-zero: its exit code, for callers that act on it.
#[derive(Debug)]
pub(crate) struct CmdError {
    /// None if it didn't start or was killed by a signal.
    pub(crate) code: Option<i32>,
    message: String,
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<CmdError> for String {
    fn from(e: CmdError) -> String {
        e.message
    }
}

/// systemctl's exit code (LSB "not installed") for a unit that isn't loaded.
pub(crate) const NOT_LOADED: i32 = 5;

pub(crate) async fn run(cmd: &str, args: &[&str]) -> Result<String, CmdError> {
    let out = Command::new(cmd).args(args).output().await.map_err(|e| CmdError { code: None, message: format!("{cmd}: {e}") })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(CmdError { code: out.status.code(), message: format!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()) })
    }
}

// shortcut: shells out to systemctl, systemd-run and mke2fs; use sd-bus when it matters.
pub(crate) fn unit(id: &str) -> String {
    format!("fc-{id}.service")
}

/// The agent's own user's systemd (XDG_RUNTIME_DIR must point at its runtime dir).
pub(crate) async fn systemctl(args: &[&str]) -> Result<String, CmdError> {
    run("systemctl", &[&["--user"], args].concat()).await
}

/// A machine's console: the last `n` lines its unit wrote to stdout (Cloud Hypervisor's serial console: the
/// guest's kernel, its init and the app), from the journals the agent's user may read: its own (journald
/// splits one off per user when the journal is on disk, readable through an ACL), or the system's.
/// --user-unit, not --user: that one opens the user's own files only.
pub(crate) async fn console(id: &str, n: usize) -> Result<Vec<String>, CmdError> {
    let args = ["--user-unit", &unit(id), "_TRANSPORT=stdout", "-n", &n.to_string(), "-o", "cat", "--no-pager"];
    let out = run("journalctl", &args).await.map_err(|mut e| {
        // journald splits off a user's journal only when it keeps it on disk.
        if e.message.contains("insufficient permissions") {
            e.message.push_str(" (this worker's journal isn't on disk, so it has no journal of the agent's user: Storage=persistent in journald.conf, as flyyy's worker module sets)");
        }
        e
    })?;
    Ok(out.lines().map(str::to_owned).collect())
}

pub(crate) fn remove(path: &str) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("rm {path}: {e}")),
        _ => Ok(()),
    }
}

pub(crate) fn now_secs() -> i64 {
    now_ms() / 1000
}
