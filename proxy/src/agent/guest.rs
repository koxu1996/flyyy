//! The guest's config.json: what the VM's init runs, from the image's config and the spec (Docker semantics).

use serde::{Deserialize, Serialize};

use super::spec::{Spec, MAX_ARG_BYTES};
use crate::images::ImageConfig;

/// The guest launcher's config (guest/src/main.rs reads it as `/.flyyy/config.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct GuestConfig {
    pub(crate) version: u32,
    pub(crate) argv: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) workdir: String,
    pub(crate) user: String,
    pub(crate) hostname: String,
    pub(crate) machine_id: String,
    /// Directories to create if missing, as Docker creates a volume's mount point. Default for state
    /// files written before they existed.
    #[serde(default)]
    pub(crate) volumes: Vec<String>,
}

/// Limits for an image's volumes: they come from the image, so they're checked like any other input.
pub(crate) const MAX_VOLUMES: usize = 64;

pub(crate) fn volume_ok(v: &str) -> bool {
    v.starts_with('/') && v.len() <= MAX_ARG_BYTES && !v.contains('\0') && !v.split('/').any(|c| c == "." || c == "..")
}

/// Docker's default PATH, when the image sets none.
pub(crate) const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What the app runs, with Docker's semantics: argv = Entrypoint + (the machine's cmd, or else the
/// image Cmd); env = the image Env overridden by the machine's env, plus HOSTNAME and a default PATH.
/// User and WorkingDir come from the image (HOME is set by the launcher, which reads the image's passwd).
/// Each of the image's Volumes becomes a plain directory on this boot's scratch (no persistence).
pub(crate) fn guest_config(spec: &Spec, image: &ImageConfig) -> Result<GuestConfig, String> {
    let mut argv = image.entrypoint.clone();
    argv.extend(if spec.cmd.is_empty() { image.cmd.clone() } else { spec.cmd.clone() });
    if argv.is_empty() {
        return Err("nothing to run: the image has no Entrypoint or Cmd and the machine no cmd".into());
    }
    let key = |e: &str| e.split_once('=').map_or(e.to_owned(), |(k, _)| k.to_owned());
    let set: Vec<(String, String)> = spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).chain([("HOSTNAME".to_owned(), spec.id.clone())]).collect();
    let mut env: Vec<String> = image.env.iter().filter(|e| !set.iter().any(|(k, _)| *k == key(e))).cloned().collect();
    env.extend(set.iter().map(|(k, v)| format!("{k}={v}")));
    if !env.iter().any(|e| key(e) == "PATH") {
        env.insert(0, format!("PATH={DEFAULT_PATH}"));
    }
    let workdir = if image.workdir.is_empty() { "/".to_owned() } else { image.workdir.clone() };
    if image.volumes.len() > MAX_VOLUMES {
        return Err(format!("image has {} volumes, want at most {MAX_VOLUMES}", image.volumes.len()));
    }
    if let Some(v) = image.volumes.iter().find(|v| !volume_ok(v)) {
        return Err(format!("image volume {v:?}: want an absolute path without . or .."));
    }
    Ok(GuestConfig {
        version: 1,
        argv,
        env,
        workdir,
        user: image.user.clone(),
        hostname: spec.id.clone(),
        machine_id: spec.id.clone(),
        volumes: image.volumes.clone(),
    })
}
