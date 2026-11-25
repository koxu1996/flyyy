//! Small helpers used across the roles: wall-clock time and atomic file writes.
use std::time::SystemTime;

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Write `path` whole or not at all: a temporary file next to it, then a rename.
pub(crate) fn write_atomic(path: &str, data: &[u8]) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| format!("write {path}: {e}"))
}
