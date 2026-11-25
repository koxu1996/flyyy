//! Background image builds for the machine agent: one per pinned digest, each a small state machine
//! (resolving → building → checking → ready, or failed) persisted after every step in
//! `<dir>/<digest>.json`, so a restarted agent resumes instead of starting over. The result,
//! `<dir>/<digest>.erofs`, is a read-only EROFS image shared by every machine of that digest here.
//! The reconcile loop never waits on a build: it asks `ensure`, and a finished build wakes it.
use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{info, warn};

use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::Notify};

use crate::util::now_ms;

const BUILD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Step {
    Resolving,
    Building,
    Checking,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    /// The ref this digest was first asked for, `<repo>@sha256:<digest>` (validated by the caller).
    image: String,
    step: Step,
    /// The image's run config: set once resolved.
    config: Option<ImageConfig>,
    error: String,
    attempts: u32,
    retry_at_ms: i64,
}

/// Where a build (re)starts from a persisted record. Each step is skipped when its output exists:
/// resolving when the config is already in the record, building when the image file exists.
/// None: nothing to do (ready, or failed and not due for a retry).
fn next(rec: &Record, image_exists: bool, now_ms: i64) -> Option<Step> {
    match rec.step {
        Step::Ready if image_exists => None,
        Step::Failed if now_ms < rec.retry_at_ms => None,
        _ if rec.config.is_none() => Some(Step::Resolving),
        _ if !image_exists => Some(Step::Building),
        _ => Some(Step::Checking),
    }
}

/// Retry delay after `attempts` failures: 1, 2, 4 … 60 s.
fn retry_delay(attempts: u32) -> Duration {
    Duration::from_secs(1u64 << attempts.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

/// The parts of an OCI image config a machine's launch needs (Docker's semantics, see agent.rs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImageConfig {
    pub env: Vec<String>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub workdir: String,
    pub user: String,
    /// The image's `Volumes` (mount points). Default for records written before they were kept.
    #[serde(default)]
    pub volumes: Vec<String>,
}

/// What the reconcile loop sees.
pub(crate) enum Status {
    Ready { file: String, config: ImageConfig },
    Pending,
    Failed(String),
}

#[derive(Clone)]
pub(crate) struct Images {
    dir: String,
    /// The reconcile loop's wake-up: notified when a build ends either way.
    changed: Arc<Notify>,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    records: HashMap<String, Record>,
    /// Digests with a build task running: at most one per digest.
    running: HashSet<String>,
}

fn digest_of(image: &str) -> &str {
    image.split_once("@sha256:").map_or("", |(_, d)| d)
}

/// containerd/OCI platform name for this host's architecture.
fn platform() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        a => a,
    };
    format!("linux/{arch}")
}

/// Last non-empty stderr line: crane and mkfs.erofs put the reason there.
fn reason(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim().to_owned()
}

impl Images {
    /// Load persisted records, drop half-written files (never trusted), resume interrupted builds.
    pub(crate) fn open(dir: &str, changed: Arc<Notify>) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
        let mut records = HashMap::new();
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
            if name.ends_with(".tmp") {
                info!(file = %name, "image: removing a leftover build");
                std::fs::remove_file(&path).map_err(|e| format!("rm {}: {e}", path.display()))?;
            } else if let Some(digest) = name.strip_suffix(".json") {
                match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Record>(&b).map_err(|e| e.to_string())) {
                    Ok(rec) => {
                        records.insert(digest.to_owned(), rec);
                    }
                    Err(e) => warn!(path = %path.display(), error = %e, "image: ignoring a record"),
                }
            }
        }
        let images = Images { dir: dir.to_owned(), changed, inner: Arc::new(Mutex::new(Inner { records, running: HashSet::new() })) };
        let resume: Vec<String> = {
            let inner = images.inner.lock().unwrap();
            inner.records.iter().filter(|(_, r)| !matches!(r.step, Step::Ready | Step::Failed)).map(|(d, _)| d.clone()).collect()
        };
        for digest in resume {
            info!(%digest, "image: resuming an interrupted build");
            images.spawn(&digest);
        }
        Ok(images)
    }

    fn file(&self, digest: &str) -> String {
        format!("{}/{digest}.erofs", self.dir)
    }

    fn persist(&self, digest: &str, rec: &Record) -> Result<(), String> {
        let path = format!("{}/{digest}.json", self.dir);
        let tmp = format!("{path}.tmp");
        let json = serde_json::to_vec(rec).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| format!("write {path}: {e}"))
    }

    /// The image's status, starting (or retrying) its build when needed. Never blocks.
    pub(crate) fn ensure(&self, image: &str) -> Status {
        let digest = digest_of(image).to_owned();
        let exists = std::path::Path::new(&self.file(&digest)).exists();
        let mut inner = self.inner.lock().unwrap();
        if inner.running.contains(&digest) {
            return Status::Pending;
        }
        let rec = inner.records.entry(digest.clone()).or_insert_with(|| Record {
            image: image.to_owned(),
            step: Step::Resolving,
            config: None,
            error: String::new(),
            attempts: 0,
            retry_at_ms: 0,
        });
        match (rec.step, next(rec, exists, now_ms())) {
            (Step::Ready, None) => Status::Ready { file: self.file(&digest), config: rec.config.clone().unwrap_or_default() },
            (_, None) => Status::Failed(rec.error.clone()),
            (_, Some(_)) => {
                drop(inner);
                self.spawn(&digest);
                Status::Pending
            }
        }
    }

    fn spawn(&self, digest: &str) {
        if !self.inner.lock().unwrap().running.insert(digest.to_owned()) {
            return;
        }
        let (images, digest) = (self.clone(), digest.to_owned());
        tokio::spawn(async move {
            images.build(&digest).await;
            images.inner.lock().unwrap().running.remove(&digest);
            images.changed.notify_one();
        });
    }

    /// Run the remaining steps, persisting after each one. A failure records the error and a retry time.
    async fn build(&self, digest: &str) {
        let t0 = Instant::now();
        loop {
            let mut rec = match self.inner.lock().unwrap().records.get(digest) {
                Some(r) => r.clone(),
                None => return,
            };
            let exists = std::path::Path::new(&self.file(digest)).exists();
            let Some(step) = next(&rec, exists, now_ms()) else { return };
            let ts = Instant::now();
            rec.step = step;
            let _ = self.persist(digest, &rec);
            let result = match step {
                Step::Resolving => self.resolve(&rec.image).await.map(|config| {
                    rec.config = Some(config);
                }),
                // The export's output is only a .tmp, so checking it follows in the same run; on a
                // resume, Checking is only reached when the final file already exists.
                Step::Building => match self.export(&rec.image, digest).await {
                    Ok(()) => {
                        info!(%digest, step = %"Building", ms = ts.elapsed().as_millis() as u64, "image step done");
                        rec.step = Step::Checking;
                        let _ = self.persist(digest, &rec);
                        self.check(digest).await
                    }
                    Err(e) => Err(e),
                },
                Step::Checking => self.check(digest).await,
                Step::Ready | Step::Failed => unreachable!("next() never returns these"),
            };
            match result {
                Ok(()) => {
                    if step != Step::Building {
                        info!(%digest, step = ?step, ms = ts.elapsed().as_millis() as u64, "image step done");
                    }
                    if matches!(step, Step::Building | Step::Checking) {
                        rec.step = Step::Ready;
                        rec.error.clear();
                        rec.attempts = 0;
                        let size = std::fs::metadata(self.file(digest)).map_or(0, |m| m.len());
                        info!(%digest, bytes = size, ms = t0.elapsed().as_millis() as u64, "image ready");
                    }
                }
                Err(e) => {
                    rec.attempts += 1;
                    let delay = retry_delay(rec.attempts);
                    rec.error = format!("{:?}: {e}", rec.step);
                    rec.step = Step::Failed;
                    rec.retry_at_ms = now_ms() + delay.as_millis() as i64;
                    warn!(%digest, retry_in_s = delay.as_secs(), error = %rec.error, "image failed");
                }
            }
            if let Err(e) = self.persist(digest, &rec) {
                warn!(%digest, error = %e, "image: saving its record");
            }
            let done = matches!(rec.step, Step::Ready | Step::Failed);
            self.inner.lock().unwrap().records.insert(digest.to_owned(), rec);
            if done {
                return;
            }
        }
    }

    /// A build tool (crane, mkfs.erofs) in the images directory, as the agent's own (unprivileged)
    /// user: building an image needs no privileges at all, no mounts, no block devices.
    fn tool(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("HOME", &self.dir).current_dir(&self.dir).kill_on_drop(true);
        c
    }

    /// The image config: Env, Entrypoint, Cmd, WorkingDir, User and Volumes, kept as lists (no joining:
    /// the launcher gets them exactly, through config.json).
    async fn resolve(&self, image: &str) -> Result<ImageConfig, String> {
        let out = self.tool("crane").args(["config", "--platform", &platform(), image]).output();
        let out = tokio::time::timeout(BUILD_TIMEOUT, out).await.map_err(|_| "crane config timed out".to_owned())?.map_err(|e| format!("crane: {e}"))?;
        if !out.status.success() {
            return Err(format!("crane config: {}", reason(&out.stderr)));
        }
        let config: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("image config: {e}"))?;
        let strings =
            |v: &serde_json::Value| -> Vec<String> { v.as_array().map_or(vec![], |a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()) };
        let c = &config["config"];
        let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_owned();
        let volumes = c["Volumes"].as_object().map_or(vec![], |o| o.keys().cloned().collect());
        Ok(ImageConfig {
            env: strings(&c["Env"]),
            entrypoint: strings(&c["Entrypoint"]),
            cmd: strings(&c["Cmd"]),
            workdir: text(&c["WorkingDir"]),
            user: text(&c["User"]),
            volumes,
        })
    }

    /// `crane export | mkfs.erofs --tar=f` into `<digest>.erofs.tmp`. Both exit statuses count, so a
    /// truncated export never turns into an image.
    // shortcut: uncompressed EROFS (no -z); try lz4 when disk matters more than CPU.
    async fn export(&self, image: &str, digest: &str) -> Result<(), String> {
        let tmp = format!("{}.tmp", self.file(digest));
        let mut crane = self
            .tool("crane")
            .args(["export", "--platform", &platform(), image, "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("crane: {e}"))?;
        let tar: Stdio = crane.stdout.take().expect("piped").try_into().map_err(|e| format!("pipe: {e}"))?;
        let mkfs = self
            .tool("mkfs.erofs")
            .args(["--tar=f", &tmp])
            .stdin(tar)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("mkfs.erofs: {e}"))?;
        let both = async { tokio::join!(crane.wait_with_output(), mkfs.wait_with_output()) };
        let (c, m) = tokio::time::timeout(BUILD_TIMEOUT, both).await.map_err(|_| format!("export timed out after {}s", BUILD_TIMEOUT.as_secs()))?;
        let (c, m) = (c.map_err(|e| format!("crane: {e}"))?, m.map_err(|e| format!("mkfs.erofs: {e}"))?);
        let failed = if !c.status.success() {
            Some(format!("crane export: {}", reason(&c.stderr)))
        } else if !m.status.success() {
            Some(format!("mkfs.erofs: {}", reason(&m.stderr)))
        } else {
            None
        };
        match failed {
            Some(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
            None => Ok(()),
        }
    }

    /// fsck the freshly built image, make it read-only, and only then give it its final name.
    async fn check(&self, digest: &str) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, file) = (format!("{}.tmp", self.file(digest)), self.file(digest));
        if std::path::Path::new(&file).exists() {
            return Ok(());
        }
        let out = Command::new("fsck.erofs").arg(&tmp).output().await.map_err(|e| format!("fsck.erofs: {e}"))?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("fsck.erofs: {}", reason(&out.stderr)));
        }
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444)).map_err(|e| format!("chmod {tmp}: {e}"))?;
        std::fs::rename(&tmp, &file).map_err(|e| format!("rename {tmp}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(step: Step, resolved: bool, retry_at_ms: i64) -> Record {
        Record { image: String::new(), step, config: resolved.then(ImageConfig::default), error: String::new(), attempts: 1, retry_at_ms }
    }

    #[test]
    fn next_step_resumes_from_persisted_state_and_skips_finished_work() {
        // Fresh, or cut off while resolving: resolve.
        assert_eq!(next(&rec(Step::Resolving, false, 0), false, 0), Some(Step::Resolving));
        // Cut off mid-export (its .tmp was removed on start): config is kept, export again.
        assert_eq!(next(&rec(Step::Building, true, 0), false, 0), Some(Step::Building));
        // The image exists (renamed) but the record didn't say ready yet: only check.
        assert_eq!(next(&rec(Step::Checking, true, 0), true, 0), Some(Step::Checking));
        // Ready with its file: nothing. Ready but the file is gone: rebuild it.
        assert_eq!(next(&rec(Step::Ready, true, 0), true, 0), None);
        assert_eq!(next(&rec(Step::Ready, true, 0), false, 0), Some(Step::Building));
        // Failed: nothing until the retry time, then from wherever it got to.
        assert_eq!(next(&rec(Step::Failed, false, 1_000), false, 999), None);
        assert_eq!(next(&rec(Step::Failed, false, 1_000), false, 1_000), Some(Step::Resolving));
        assert_eq!(next(&rec(Step::Failed, true, 1_000), false, 2_000), Some(Step::Building));
        assert_eq!((retry_delay(1), retry_delay(3), retry_delay(20)), (Duration::from_secs(1), Duration::from_secs(4), Duration::from_secs(60)));
    }
}
