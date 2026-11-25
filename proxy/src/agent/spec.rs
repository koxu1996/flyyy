//! A machine's spec (what the control plane puts through the agent API), and the limits and checks every spec passes:
//! cmd, env, size, names, image references.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// shortcut: the spec is immutable except for state; replace = create a new machine and destroy the old one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Spec {
    pub(crate) id: String,
    pub(crate) app: String,
    pub(crate) image: String,
    /// Replaces the image's Cmd when non-empty (Entrypoint is kept), as `docker run <image> <args>`.
    pub(crate) cmd: Vec<String>,
    /// Overrides the image's Env.
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) port: u16,
    /// vCPUs and memory (MiB). Defaults for state files written before they existed.
    #[serde(default = "default_cpus")]
    pub(crate) cpus: u32,
    #[serde(default = "default_memory")]
    pub(crate) memory: u32,
    /// Its services (services.rs), never empty: a row without them is one http service on `port`.
    /// Empty only in state files written before services existed (see `Spec::services`).
    #[serde(default)]
    pub(crate) services: Vec<crate::services::Service>,
}

impl Spec {
    pub(crate) fn services(&self) -> Vec<crate::services::Service> {
        if self.services.is_empty() {
            crate::services::http_on(self.port)
        } else {
            self.services.clone()
        }
    }
}

/// Limits for a machine's cmd and env. They travel in config.json (not the kernel cmdline), so any
/// bytes but NUL are fine; the limits only keep rows and configs small.
pub(crate) const MAX_ARGS: usize = 64;

pub(crate) const MAX_ARG_BYTES: usize = 4096;

pub(crate) const MAX_ENV: usize = 64;

pub(crate) const MAX_ENV_BYTES: usize = 32 * 1024;

pub(crate) fn cmd_ok(cmd: &[String]) -> Result<(), String> {
    if cmd.len() > MAX_ARGS {
        return Err(format!("cmd: {} args, want at most {MAX_ARGS}", cmd.len()));
    }
    match cmd.iter().find(|a| a.len() > MAX_ARG_BYTES || a.contains('\0')) {
        Some(a) => Err(format!("cmd: an arg is over {MAX_ARG_BYTES} bytes or has a NUL byte ({} bytes)", a.len())),
        None => Ok(()),
    }
}

pub(crate) fn env_ok(env: &BTreeMap<String, String>) -> Result<(), String> {
    if env.len() > MAX_ENV {
        return Err(format!("env: {} vars, want at most {MAX_ENV}", env.len()));
    }
    let name_ok = |k: &str| k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if let Some(k) = env.keys().find(|k| !name_ok(k)) {
        return Err(format!("env: bad name {k:?}: want [A-Za-z_][A-Za-z0-9_]*"));
    }
    if let Some(k) = env.iter().find(|(_, v)| v.contains('\0')).map(|(k, _)| k) {
        return Err(format!("env: {k} has a NUL byte"));
    }
    let total: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
    if total > MAX_ENV_BYTES {
        return Err(format!("env: {total} bytes, want at most {MAX_ENV_BYTES}"));
    }
    Ok(())
}

/// A machine's size. The defaults are the schema's column defaults.
// shortcut: these are sanity bounds, not capacity: placement ignores size, so a worker can be overcommitted.
pub(crate) const DEFAULT_CPUS: u32 = 1;

pub(crate) const DEFAULT_MEMORY: u32 = 128;

pub(crate) const MAX_CPUS: u32 = 8;

pub(crate) const MIN_MEMORY: u32 = 128;

pub(crate) const MAX_MEMORY: u32 = 16 * 1024;

pub(crate) fn default_cpus() -> u32 {
    DEFAULT_CPUS
}

pub(crate) fn default_memory() -> u32 {
    DEFAULT_MEMORY
}

pub(crate) fn size_ok(cpus: u32, memory: u32) -> Result<(), String> {
    if !(1..=MAX_CPUS).contains(&cpus) {
        return Err(format!("cpus {cpus}: want 1-{MAX_CPUS}"));
    }
    if !(MIN_MEMORY..=MAX_MEMORY).contains(&memory) {
        return Err(format!("memory {memory} MiB: want {MIN_MEMORY}-{MAX_MEMORY}"));
    }
    Ok(())
}

pub(crate) fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A digest-pinned image ref, `[<host>:<port>/]<repo>@sha256:<64 hex>`: no tags, so a machine always gets
/// the same bytes. It ends up as a `crane` argument and a registry path, so the repo is limited to
/// `[a-z0-9._/-]`; a registry port (a private registry such as the tests' `10.99.0.3:5000`) is the only `:`.
pub(crate) fn image_ok(s: &str) -> bool {
    let Some((repo, digest)) = s.split_once("@sha256:") else { return false };
    let repo = match repo.split_once('/') {
        Some((host, rest)) if host.contains(':') => {
            let Some((name, port)) = host.split_once(':') else { return false };
            let host_ok = !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b));
            let port_ok = (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit());
            if !(host_ok && port_ok) {
                return false;
            }
            rest
        }
        _ => repo,
    };
    let repo_ok = !repo.is_empty()
        && repo.len() <= 200
        && repo.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_/".contains(&b))
        && !repo.starts_with(['/', '.', '-'])
        && !repo.ends_with('/')
        && !repo.contains("..")
        && !repo.contains("//");
    repo_ok && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Check a spec before anything acts on it (the agent API refuses one that fails). The id ends up in paths
/// and unit names, and cmd and env in the guest's config, so all are strictly limited.
pub(crate) fn spec_ok(spec: &Spec) -> Result<(), String> {
    let Spec { id, app, image, cmd, env, port, cpus, memory, services } = spec;
    if !name_ok(id) {
        return Err(format!("bad id {id:?}: want [a-z0-9-]{{1,32}}"));
    }
    if !name_ok(app) {
        return Err(format!("bad app {app:?}: want [a-z0-9-]{{1,32}}"));
    }
    if !image_ok(image) {
        return Err(format!("bad image {image:?}: want <repo>@sha256:<64 hex>"));
    }
    if *port == 0 {
        return Err("bad port 0: want 1-65535".into());
    }
    cmd_ok(cmd)?;
    env_ok(env)?;
    size_ok(*cpus, *memory)?;
    if services.is_empty() {
        return Err("no services: want at least one".into());
    }
    crate::services::services_ok(services)
}

/// A spec's digest: what the control plane compares to know a worker holds the spec it sent (env included,
/// so it never needs the values back).
pub(crate) fn digest(spec: &Spec) -> String {
    ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(spec).expect("serialisable")).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
