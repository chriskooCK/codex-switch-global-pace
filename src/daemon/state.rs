/// Daemon state snapshot (`~/.codex-switch/daemon-state.json`).
///
/// The daemon has no control socket; this file is its observability surface.
/// The loop overwrites it atomically after every event, `daemon status` (and
/// anything else) reads it. Writes are best-effort — a failing snapshot must
/// never take the daemon down.
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub last_poll_at: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    /// Unix seconds until which polling is suspended after failures.
    pub backoff_until: Option<i64>,
    pub last_switch: Option<SwitchRecord>,
    pub pending_switch: Option<PendingSwitch>,
    pub last_cache_refresh_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchRecord {
    pub from: String,
    pub to: String,
    pub at: i64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSwitch {
    pub to: String,
    pub since: i64,
}

pub fn state_path() -> anyhow::Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("daemon-state.json"))
}

/// Best-effort atomic write. Snapshot failure must not stop the daemon, but it
/// must remain visible in unattended logs.
pub fn write(state: &mut DaemonState) {
    state.updated_at = crate::auth::now_unix_secs();
    if let Err(error) = write_snapshot(state) {
        tracing::warn!("daemon state snapshot write failed: {error:#}");
    }
}

fn write_snapshot(state: &DaemonState) -> anyhow::Result<()> {
    let path = state_path().context("resolving daemon state snapshot path")?;
    write_at(&path, state)
}

fn write_at(path: &Path, state: &DaemonState) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serializing daemon state snapshot")?;
    let outcome = crate::auth::atomic_write_private(path, &bytes)
        .with_context(|| format!("writing daemon state snapshot {}", path.display()))?;
    crate::auth::require_durable_private_write(path, "daemon state snapshot", outcome)
        .with_context(|| format!("confirming daemon state snapshot {}", path.display()))
}

pub fn read() -> anyhow::Result<Option<DaemonState>> {
    read_at(&state_path()?)
}

fn read_at(path: &Path) -> anyhow::Result<Option<DaemonState>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading daemon state snapshot {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("parsing daemon state snapshot {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrips_through_disk() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon-state.json");

        let state = DaemonState {
            pid: 4242,
            started_at: 100,
            updated_at: 200,
            last_poll_at: Some(190),
            last_error: Some("boom".to_string()),
            consecutive_failures: 2,
            backoff_until: Some(400),
            last_switch: Some(SwitchRecord {
                from: "alice".to_string(),
                to: "bob".to_string(),
                at: 150,
                score: 87.5,
            }),
            pending_switch: Some(PendingSwitch {
                to: "carol".to_string(),
                since: 195,
            }),
            last_cache_refresh_at: Some(180),
        };

        write_at(&path, &state).expect("write snapshot");
        let loaded = read_at(&path)
            .expect("snapshot read should succeed")
            .expect("snapshot should be present");

        assert_eq!(loaded.pid, 4242);
        assert_eq!(loaded.consecutive_failures, 2);
        assert_eq!(loaded.last_switch.as_ref().unwrap().to, "bob");
        assert_eq!(loaded.pending_switch.as_ref().unwrap().to, "carol");
        assert_eq!(loaded.backoff_until, Some(400));
    }

    #[test]
    fn missing_snapshot_returns_none() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon-state.json");
        assert!(read_at(&path).unwrap().is_none());
    }

    #[test]
    fn malformed_snapshot_is_reported_instead_of_treated_as_missing() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon-state.json");
        std::fs::write(&path, b"not json").unwrap();

        let error = read_at(&path).expect_err("malformed state must remain observable");

        assert!(
            format!("{error:#}").contains("parsing daemon state snapshot"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains(&path.display().to_string()));
    }

    #[test]
    fn snapshot_io_error_is_reported_instead_of_treated_as_missing() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();

        let error = read_at(dir.path()).expect_err("a directory is not a readable state file");

        assert!(
            format!("{error:#}").contains("reading daemon state snapshot"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains(&dir.path().display().to_string()));
    }

    #[test]
    fn snapshot_write_error_is_returned_to_the_best_effort_logging_boundary() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let destination_is_a_directory = dir.path().join("daemon-state.json");
        std::fs::create_dir(&destination_is_a_directory).unwrap();

        let error = write_at(&destination_is_a_directory, &DaemonState::default())
            .expect_err("a directory cannot be replaced by the state snapshot file");

        assert!(
            format!("{error:#}").contains("writing daemon state snapshot"),
            "{error:#}"
        );
        assert!(
            format!("{error:#}").contains(&destination_is_a_directory.display().to_string()),
            "{error:#}"
        );
    }
}
