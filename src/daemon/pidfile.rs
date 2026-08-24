use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};

static PIDFILE_HANDLE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

#[derive(Debug, Deserialize, Serialize)]
struct PidIdentity {
    version: u8,
    pid: u32,
    executable: PathBuf,
    #[serde(default)]
    generation: String,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Deserialize, Serialize)]
struct ShutdownRequest {
    version: u8,
    pid: u32,
    generation: String,
}

pub fn pidfile_path() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("daemon.pid"))
}

fn pidfile_lock_path_for(path: &Path) -> PathBuf {
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    PathBuf::from(lock_name)
}

fn open_pidfile_lock_at(path: &Path) -> Result<File> {
    let lock_path = pidfile_lock_path_for(path);
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| anyhow::anyhow!("opening daemon PID lock {}: {err}", lock_path.display()))
}

fn acquire_pidfile_lock_at(path: &Path) -> Result<File> {
    let lock_file = open_pidfile_lock_at(path)?;
    match FileExt::try_lock(&lock_file) {
        Ok(()) => Ok(lock_file),
        Err(TryLockError::WouldBlock) => anyhow::bail!(
            "daemon PID lock is already held at {}; another daemon is running",
            pidfile_lock_path_for(path).display()
        ),
        Err(TryLockError::Error(err)) => anyhow::bail!(
            "locking daemon PID authority {}: {err}",
            pidfile_lock_path_for(path).display()
        ),
    }
}

/// One-version migration probe for daemons that still lock `daemon.pid`
/// itself. New daemons never take this lock; they own `daemon.pid.lock`.
fn legacy_pidfile_lock_is_held_checked(path: &Path) -> Result<bool> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(anyhow::anyhow!(
                "opening legacy daemon PID lock {}: {err}",
                path.display()
            ));
        }
    };
    match FileExt::try_lock(&file) {
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => Err(anyhow::anyhow!(
            "checking legacy daemon PID lock {}: {err}",
            path.display()
        )),
        Ok(()) => {
            FileExt::unlock(&file).map_err(|err| {
                anyhow::anyhow!("unlocking legacy daemon PID file {}: {err}", path.display())
            })?;
            Ok(false)
        }
    }
}

fn shutdown_request_path() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("daemon.shutdown"))
}

/// Atomically create a PID file using O_CREAT|O_EXCL semantics.
/// Fails if the file already exists (prevents TOCTOU race).
pub fn write_pidfile_exclusive() -> Result<()> {
    let path = pidfile_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_file = acquire_pidfile_lock_at(&path)?;
    let identity = PidIdentity {
        version: 2,
        pid: std::process::id(),
        executable: std::env::current_exe()?,
        generation: daemon_generation(),
    };
    let encoded = serde_json::to_vec(&identity)?;
    // create_new(true) → O_CREAT | O_EXCL: atomic, fails if file exists.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!(
                    "PID file already exists at {}; another daemon may be running",
                    path.display()
                )
            } else {
                anyhow::anyhow!("Failed to create PID file {}: {e}", path.display())
            }
        })?;
    use std::io::Write;
    if let Err(error) = file
        .write_all(&encoded)
        .map_err(|e| anyhow::anyhow!("Failed to write PID to {}: {e}", path.display()))
        .and_then(|()| {
            file.sync_data()
                .map_err(|e| anyhow::anyhow!("Failed to sync PID file {}: {e}", path.display()))
        })
    {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    // A fresh PID file has exclusive ownership, so any request left by a prior
    // daemon cannot target this generation.
    let _ = std::fs::remove_file(shutdown_request_path()?);

    let handle = PIDFILE_HANDLE.get_or_init(|| Mutex::new(None));
    let mut guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(lock_file);
    Ok(())
}

pub fn read_pidfile() -> Option<u32> {
    let path = pidfile_path().ok()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    read_pid_from_raw(&raw)
}

/// Return the running daemon PID using the PID file's held lock as the
/// authoritative liveness signal. Unlike the status-oriented reader, this
/// never folds malformed content, permissions, or lock errors into "stopped".
pub(crate) fn running_pid_checked() -> Result<Option<u32>> {
    let path = pidfile_path()?;
    running_pid_checked_at(&path)
}

fn running_pid_checked_at(path: &Path) -> Result<Option<u32>> {
    let initial_raw = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(anyhow::anyhow!(
                "reading daemon PID file {}: {err}",
                path.display()
            ));
        }
    };
    let lock_file = open_pidfile_lock_at(path)?;
    match FileExt::try_lock(&lock_file) {
        Err(TryLockError::WouldBlock) => {
            // Re-read after observing the held authority lock so a stale
            // identity sampled before a new generation started is never used.
            let raw = std::fs::read_to_string(path).map_err(|err| {
                anyhow::anyhow!(
                    "daemon PID lock is held but its identity {} cannot be read: {err}",
                    path.display()
                )
            })?;
            let identity = parse_pid_identity(&raw).ok_or_else(|| {
                anyhow::anyhow!(
                    "daemon PID lock is held but its identity is malformed: {}",
                    path.display()
                )
            })?;
            Ok(Some(identity.pid))
        }
        Err(TryLockError::Error(err)) => Err(anyhow::anyhow!(
            "checking daemon PID lock {}: {err}",
            pidfile_lock_path_for(path).display()
        )),
        Ok(()) => {
            let legacy_running_pid = if let Some(raw) = initial_raw {
                let identity = parse_pid_identity(&raw);
                if identity.is_none() && read_pid_from_raw(&raw).is_none() {
                    anyhow::bail!("daemon PID file is malformed: {}", path.display());
                }
                let legacy_lock_held = legacy_pidfile_lock_is_held_checked(path)?;
                match (identity, legacy_lock_held) {
                    (Some(identity), true) => Some(identity.pid),
                    (None, true) => anyhow::bail!(
                        "daemon PID file {} is locked but has no trusted process identity",
                        path.display()
                    ),
                    (_, false) => None,
                }
            } else {
                None
            };
            FileExt::unlock(&lock_file).map_err(|err| {
                anyhow::anyhow!(
                    "unlocking daemon PID lock {}: {err}",
                    pidfile_lock_path_for(path).display()
                )
            })?;
            Ok(legacy_running_pid)
        }
    }
}

fn read_pid_from_raw(raw: &str) -> Option<u32> {
    parse_pid_identity(raw)
        .map(|identity| identity.pid)
        .or_else(|| raw.trim().parse::<u32>().ok().filter(|pid| *pid > 0))
}

fn parse_pid_identity(raw: &str) -> Option<PidIdentity> {
    let identity: PidIdentity = serde_json::from_str(raw).ok()?;
    let supported_version = match identity.version {
        1 => identity.generation.is_empty(),
        2 => !identity.generation.is_empty(),
        _ => false,
    };
    (supported_version && identity.pid > 0 && !identity.executable.as_os_str().is_empty())
        .then_some(identity)
}

fn daemon_generation() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(target_os = "windows")]
pub fn request_shutdown(pid: u32) -> Result<()> {
    let path = pidfile_path()?;
    let raw = std::fs::read_to_string(&path)?;
    let identity = parse_pid_identity(&raw)
        .filter(|identity| identity.pid == pid)
        .ok_or_else(|| {
            anyhow::anyhow!("Refusing to stop PID {pid}: daemon process identity is stale")
        })?;
    let request = ShutdownRequest {
        version: 1,
        pid,
        generation: identity.generation,
    };
    let request_path = shutdown_request_path()?;
    crate::auth::atomic_write_private(&request_path, &serde_json::to_vec(&request)?)?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn shutdown_requested() -> bool {
    let Ok(pidfile_path) = pidfile_path() else {
        return false;
    };
    let Ok(identity_raw) = std::fs::read_to_string(pidfile_path) else {
        return false;
    };
    let Some(identity) = parse_pid_identity(&identity_raw) else {
        return false;
    };
    let Ok(request_path) = shutdown_request_path() else {
        return false;
    };
    let Ok(request_raw) = std::fs::read_to_string(request_path) else {
        return false;
    };
    let Ok(request) = serde_json::from_str::<ShutdownRequest>(&request_raw) else {
        return false;
    };
    shutdown_request_matches(&identity, &request)
}

#[cfg(any(target_os = "windows", test))]
fn shutdown_request_matches(identity: &PidIdentity, request: &ShutdownRequest) -> bool {
    request.version == 1 && request.pid == identity.pid && request.generation == identity.generation
}

fn release_pidfile_handle() {
    let Some(handle) = PIDFILE_HANDLE.get() else {
        return;
    };
    let mut guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.take() {
        let _ = FileExt::unlock(&file);
    }
}

pub fn cleanup_pidfile() -> Result<()> {
    release_pidfile_handle();
    let path = pidfile_path()?;
    cleanup_pidfile_at(&path)
}

fn cleanup_pidfile_at(path: &Path) -> Result<()> {
    let lock_file = open_pidfile_lock_at(path)?;
    match FileExt::try_lock(&lock_file) {
        Err(TryLockError::WouldBlock) => anyhow::bail!(
            "Refusing to remove PID file {}: locked by a running daemon",
            path.display()
        ),
        Err(TryLockError::Error(e)) => return Err(e.into()),
        Ok(()) => {}
    }
    if legacy_pidfile_lock_is_held_checked(path)? {
        anyhow::bail!(
            "Refusing to remove PID file {}: locked by a running legacy daemon",
            path.display()
        );
    }
    let remove_result = match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    };
    FileExt::unlock(&lock_file).map_err(|err| {
        anyhow::anyhow!(
            "unlocking daemon PID lock {}: {err}",
            pidfile_lock_path_for(path).display()
        )
    })?;
    remove_result
}

/// RAII guard that cleans up the PID file on drop (including panics).
pub struct PidGuard;

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = cleanup_pidfile();
    }
}

/// Send SIGTERM to a process.
#[cfg(not(target_os = "windows"))]
pub fn send_sigterm(pid: u32) -> Result<()> {
    if running_pid_checked()? != Some(pid) {
        anyhow::bail!("Refusing to stop PID {pid}: daemon process identity is stale");
    }
    #[cfg(unix)]
    {
        // SAFETY: sending SIGTERM to a known PID.
        let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("Failed to send SIGTERM to PID {pid}: {err}");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        anyhow::bail!("Stopping daemon is not supported on this platform");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PidIdentity, ShutdownRequest, acquire_pidfile_lock_at, cleanup_pidfile_at,
        parse_pid_identity, pidfile_lock_path_for, read_pid_from_raw, running_pid_checked_at,
        shutdown_request_matches,
    };
    use fs4::FileExt;
    use std::path::PathBuf;

    #[test]
    fn legacy_pidfile_is_not_trusted() {
        assert!(parse_pid_identity("4242").is_none());
        assert_eq!(read_pid_from_raw("4242"), Some(4242));
    }

    #[test]
    fn version_one_pid_identity_remains_trusted_during_upgrade() {
        let raw = r#"{"version":1,"pid":4242,"executable":"/tmp/codex-switch"}"#;
        let identity = parse_pid_identity(raw).expect("v1 daemon pidfile remains readable");
        assert_eq!(identity.pid, 4242);
        assert!(identity.generation.is_empty());
    }

    #[test]
    fn shutdown_request_must_match_pid_and_generation() {
        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from("codex-switch"),
            generation: "old-generation".to_string(),
        };
        let matching = ShutdownRequest {
            version: 1,
            pid: 4242,
            generation: "old-generation".to_string(),
        };
        assert!(shutdown_request_matches(&identity, &matching));
        assert!(!shutdown_request_matches(
            &identity,
            &ShutdownRequest {
                generation: "new-generation".to_string(),
                ..matching
            }
        ));
    }

    #[test]
    fn checked_running_pid_uses_the_lock_without_a_process_list_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let identity = PidIdentity {
            version: 2,
            // This PID need not exist: the held generation lock, not tasklist,
            // is the transaction authority under test.
            pid: u32::MAX,
            executable: PathBuf::from("codex-switch"),
            generation: "locked-generation".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
        let lock_path = pidfile_lock_path_for(&path);
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        FileExt::lock(&file).unwrap();

        assert_eq!(running_pid_checked_at(&path).unwrap(), Some(u32::MAX));

        FileExt::unlock(&file).unwrap();
        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
    }

    #[test]
    fn checked_running_pid_never_folds_a_malformed_locked_file_into_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        std::fs::write(&path, b"{not-valid-json").unwrap();
        let lock_path = pidfile_lock_path_for(&path);
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        FileExt::lock(&file).unwrap();

        let error =
            running_pid_checked_at(&path).expect_err("a malformed PID identity must fail closed");
        assert!(error.to_string().contains("malformed"), "{error:#}");

        FileExt::unlock(&file).unwrap();
    }

    #[test]
    fn checked_running_pid_recognizes_an_unlocked_legacy_numeric_file_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        std::fs::write(&path, b"4242").unwrap();

        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn checked_running_pid_never_trusts_a_locked_legacy_numeric_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        std::fs::write(&path, b"4242").unwrap();
        let legacy_owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock(&legacy_owner).unwrap();

        let error = running_pid_checked_at(&path)
            .expect_err("a numeric PID alone must never authorize process control");
        assert!(
            error.to_string().contains("no trusted process identity"),
            "{error:#}"
        );

        FileExt::unlock(&legacy_owner).unwrap();
    }

    #[test]
    fn held_lock_without_identity_is_transient_error_then_becomes_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let owner = acquire_pidfile_lock_at(&path).unwrap();

        let missing = running_pid_checked_at(&path)
            .expect_err("a held authority lock without its identity must fail closed");
        assert!(
            missing.to_string().contains("cannot be read"),
            "{missing:#}"
        );

        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from("codex-switch"),
            generation: "ready-generation".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
        assert_eq!(running_pid_checked_at(&path).unwrap(), Some(4242));

        FileExt::unlock(&owner).unwrap();
        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn prior_release_same_file_lock_remains_a_live_migration_authority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let identity = PidIdentity {
            version: 1,
            pid: 4242,
            executable: PathBuf::from("codex-switch"),
            generation: String::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
        let legacy_owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock(&legacy_owner).unwrap();

        assert_eq!(running_pid_checked_at(&path).unwrap(), Some(4242));
        assert!(cleanup_pidfile_at(&path).is_err());

        FileExt::unlock(&legacy_owner).unwrap();
        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    fn prior_release_windows_same_file_lock_fails_closed_without_tasklist_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let identity = PidIdentity {
            version: 1,
            pid: 4242,
            executable: PathBuf::from("codex-switch.exe"),
            generation: String::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
        let legacy_owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        FileExt::lock(&legacy_owner).unwrap();

        let error = running_pid_checked_at(&path)
            .expect_err("Windows cannot safely read a legacy same-file-locked identity");
        assert!(
            error.to_string().contains("reading daemon PID file"),
            "{error:#}"
        );

        FileExt::unlock(&legacy_owner).unwrap();
        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
    }

    #[test]
    fn concurrent_start_is_rejected_without_waiting_to_become_a_second_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from("codex-switch"),
            generation: "first-generation".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
        let owner = acquire_pidfile_lock_at(&path).unwrap();

        let contender_path = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            tx.send(acquire_pidfile_lock_at(&contender_path).map(|_| ()))
                .unwrap();
        });
        let contender_result = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("a concurrent startup must fail immediately instead of waiting");

        let err = cleanup_pidfile_at(&path).unwrap_err();

        assert!(path.exists());
        assert!(err.to_string().contains("locked by a running daemon"));
        assert!(
            contender_result
                .unwrap_err()
                .to_string()
                .contains("another daemon is running")
        );
        FileExt::unlock(&owner).unwrap();
        contender.join().unwrap();
    }
}
