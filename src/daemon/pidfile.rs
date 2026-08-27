use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use rand::Rng as _;
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

#[derive(Debug, Deserialize, Serialize)]
struct ShutdownRequest {
    version: u8,
    pid: u32,
    generation: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DaemonGeneration {
    pub(crate) pid: u32,
    pub(crate) generation: String,
}

impl DaemonGeneration {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
}

/// Immutable receiver for the exact daemon generation published by this
/// process. Polling reads only the mutable request file; the PID identity is
/// already known and never reopened on the steady-state path.
#[derive(Debug)]
pub(crate) struct ShutdownRequestMonitor {
    request_path: PathBuf,
    target: DaemonGeneration,
}

impl ShutdownRequestMonitor {
    pub(crate) fn is_requested(&self) -> bool {
        shutdown_requested_at(&self.request_path, &self.target)
    }

    #[cfg(test)]
    fn at(request_path: PathBuf, target: DaemonGeneration) -> Self {
        Self {
            request_path,
            target,
        }
    }
}

enum ShutdownRequestDurability {
    Durable,
    VisibleDurabilityUnconfirmed(anyhow::Error),
}

#[must_use = "shutdown-request durability must be classified after the daemon stop result"]
pub(crate) struct ShutdownRequestOutcome {
    target: DaemonGeneration,
    durability: ShutdownRequestDurability,
}

impl ShutdownRequestOutcome {
    pub(crate) fn target(&self) -> &DaemonGeneration {
        &self.target
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn target_is_running(&self) -> Result<bool> {
        Ok(running_generation_checked()?.as_ref() == Some(&self.target))
    }

    pub(crate) fn require_durable(self) -> Result<()> {
        match self.durability {
            ShutdownRequestDurability::Durable => Ok(()),
            ShutdownRequestDurability::VisibleDurabilityUnconfirmed(error) => Err(error),
        }
    }
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
    match try_acquire_pidfile_lock_at(path)? {
        Some(lock_file) => Ok(lock_file),
        None => anyhow::bail!(
            "daemon PID lock is already held at {}; another daemon is running",
            pidfile_lock_path_for(path).display()
        ),
    }
}

fn try_acquire_pidfile_lock_at(path: &Path) -> Result<Option<File>> {
    let lock_file = open_pidfile_lock_at(path)?;
    match FileExt::try_lock(&lock_file) {
        Ok(()) => Ok(Some(lock_file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => anyhow::bail!(
            "locking daemon PID authority {}: {err}",
            pidfile_lock_path_for(path).display()
        ),
    }
}

pub(crate) enum DaemonAbsenceAcquireFor<Lease> {
    Acquired(Lease),
    Contended,
}

pub(crate) type DaemonAbsenceAcquire = DaemonAbsenceAcquireFor<DaemonAbsenceLease>;

pub(crate) enum ContendingDaemonIdentity {
    Pending,
    Published(DaemonGeneration),
}

/// Exclusive proof that no cooperating daemon owns, or can acquire, the PID
/// authority. Self-update keeps this guard while the public executable is
/// replaced. A direct foreground start uses the same lock in
/// `write_pidfile_exclusive`, so it fails instead of entering the update
/// window.
#[derive(Debug)]
pub(crate) struct DaemonAbsenceLease {
    path: PathBuf,
    lock_file: File,
}

impl DaemonAbsenceLease {
    pub(crate) fn verify(&self) -> Result<()> {
        if crate::fs_ops::token_if_present(&self.path)?.is_some() {
            anyhow::bail!(
                "daemon PID identity appeared while its absence lease was held: {}",
                self.path.display()
            );
        }
        if legacy_pidfile_lock_is_held_checked(&self.path)? {
            anyhow::bail!(
                "a legacy daemon acquired the PID identity while its absence lease was held: {}",
                self.path.display()
            );
        }
        Ok(())
    }
}

impl Drop for DaemonAbsenceLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

pub(crate) fn try_acquire_daemon_absence_lease(
    expected_stale_generation: Option<&DaemonGeneration>,
) -> Result<DaemonAbsenceAcquire> {
    let path = pidfile_path()?;
    try_acquire_daemon_absence_lease_at(&path, expected_stale_generation)
}

pub(crate) fn contending_daemon_identity(
    expected_stale_generation: Option<&DaemonGeneration>,
) -> Result<ContendingDaemonIdentity> {
    let path = pidfile_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ContendingDaemonIdentity::Pending);
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "reading contending daemon PID identity {}: {error}",
                path.display()
            ));
        }
    };
    Ok(classify_contending_daemon_identity(
        &raw,
        expected_stale_generation,
    ))
}

fn classify_contending_daemon_identity(
    raw: &str,
    expected_stale_generation: Option<&DaemonGeneration>,
) -> ContendingDaemonIdentity {
    let Some(identity) = parse_pid_identity(raw) else {
        return ContendingDaemonIdentity::Pending;
    };
    let observed = DaemonGeneration {
        pid: identity.pid,
        generation: identity.generation,
    };
    if expected_stale_generation == Some(&observed) {
        ContendingDaemonIdentity::Pending
    } else {
        ContendingDaemonIdentity::Published(observed)
    }
}

#[cfg(test)]
fn acquire_daemon_absence_lease_at(
    path: &Path,
    expected_stale_generation: Option<&DaemonGeneration>,
) -> Result<DaemonAbsenceLease> {
    match try_acquire_daemon_absence_lease_at(path, expected_stale_generation)? {
        DaemonAbsenceAcquire::Acquired(lease) => Ok(lease),
        DaemonAbsenceAcquire::Contended => anyhow::bail!(
            "acquiring the daemon PID absence lease for self-update: daemon PID lock is already held at {}; another daemon is running",
            pidfile_lock_path_for(path).display()
        ),
    }
}

fn try_acquire_daemon_absence_lease_at(
    path: &Path,
    expected_stale_generation: Option<&DaemonGeneration>,
) -> Result<DaemonAbsenceAcquire> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating daemon state directory for absence lease {}",
                parent.display()
            )
        })?;
    }
    let Some(lock_file) = try_acquire_pidfile_lock_at(path)? else {
        return Ok(DaemonAbsenceAcquire::Contended);
    };
    if legacy_pidfile_lock_is_held_checked(path)? {
        anyhow::bail!(
            "a legacy daemon still owns the PID identity at {}; refusing to enter the self-update replacement window",
            path.display()
        );
    }

    if let Some(stale_token) = crate::fs_ops::token_if_present(path)? {
        let raw = std::fs::read(path)
            .with_context(|| format!("reading stale daemon PID identity {}", path.display()))?;
        if !stale_token.matches_bytes(&raw) {
            anyhow::bail!(
                "daemon PID identity changed while acquiring its absence lease: {}",
                path.display()
            );
        }
        let stale_raw = std::str::from_utf8(&raw)
            .map_err(|_| anyhow::anyhow!("daemon PID file is malformed: {}", path.display()))?;
        if let Some(expected) = expected_stale_generation {
            let identity = parse_pid_identity(stale_raw).ok_or_else(|| {
                anyhow::anyhow!(
                    "daemon PID identity at {} has no exact generation token; refusing to remove it as stale generation {expected:?}",
                    path.display()
                )
            })?;
            let observed = DaemonGeneration {
                pid: identity.pid,
                generation: identity.generation,
            };
            if &observed != expected {
                anyhow::bail!(
                    "daemon generation changed from {expected:?} to {observed:?} before the self-update absence lease was acquired"
                );
            }
        } else if read_pid_from_raw(stale_raw).is_none() {
            anyhow::bail!("daemon PID file is malformed: {}", path.display());
        }
        crate::fs_ops::remove_exact(path, &stale_token).with_context(|| {
            format!(
                "removing the exactly revalidated stale daemon PID identity {}",
                path.display()
            )
        })?;
    }

    let lease = DaemonAbsenceLease {
        path: path.to_path_buf(),
        lock_file,
    };
    lease.verify()?;
    Ok(DaemonAbsenceAcquire::Acquired(lease))
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
pub(crate) fn write_pidfile_exclusive() -> Result<ShutdownRequestMonitor> {
    let path = pidfile_path()?;
    let identity = PidIdentity {
        version: 2,
        pid: std::process::id(),
        executable: std::env::current_exe()?,
        generation: daemon_generation(),
    };
    let request_path = shutdown_request_path()?;
    let lock_file = publish_pid_identity_at(&path, Some(&request_path), &identity)?;

    let handle = PIDFILE_HANDLE.get_or_init(|| Mutex::new(None));
    let mut guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(lock_file);
    Ok(ShutdownRequestMonitor {
        request_path,
        target: DaemonGeneration {
            pid: identity.pid,
            generation: identity.generation,
        },
    })
}

fn publish_pid_identity_at(
    path: &Path,
    request_path: Option<&Path>,
    identity: &PidIdentity,
) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating daemon PID directory {}", parent.display()))?;
    }
    let lock_file = acquire_pidfile_lock_at(path)?;
    if let Some(request_path) = request_path {
        cleanup_stale_shutdown_request_before_publication(request_path, identity)?;
    }

    let encoded = serde_json::to_vec(identity)?;
    // create_new(true) → O_CREAT | O_EXCL: atomic, fails if a stale or foreign
    // identity still occupies the public PID path.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!(
                    "PID file already exists at {}; another daemon may be running",
                    path.display()
                )
            } else {
                anyhow::anyhow!("Failed to create PID file {}: {error}", path.display())
            }
        })?;
    use std::io::Write as _;
    let publication = file
        .write_all(&encoded)
        .map_err(|error| anyhow::anyhow!("Failed to write PID to {}: {error}", path.display()))
        .and_then(|()| {
            file.sync_data().map_err(|error| {
                anyhow::anyhow!("Failed to sync PID file {}: {error}", path.display())
            })
        });
    if let Err(publication_error) = publication {
        return match cleanup_failed_pid_publication(path, file) {
            Ok(()) => Err(publication_error.context(
                "PID identity publication failed; its exact partial artifact was removed",
            )),
            Err(cleanup_error) => Err(publication_error.context(format!(
                "PID identity publication failed and exact partial-artifact cleanup was incomplete: {cleanup_error:#}"
            ))),
        };
    }
    Ok(lock_file)
}

fn cleanup_failed_pid_publication(path: &Path, mut file: File) -> Result<()> {
    let token = crate::fs_ops::token_for_file(&mut file)
        .context("binding a partially published PID identity for exact cleanup")?;
    drop(file);
    match crate::fs_ops::remove_exact(path, &token)? {
        crate::fs_ops::RemoveExactOutcome::Removed => Ok(()),
        crate::fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed => {
            anyhow::bail!(
                "partial PID identity was removed, but parent-directory durability is unconfirmed: {}",
                path.display()
            )
        }
    }
}

fn cleanup_stale_shutdown_request_before_publication(
    request_path: &Path,
    identity: &PidIdentity,
) -> Result<()> {
    let Some(token) = crate::fs_ops::token_if_present(request_path)? else {
        return Ok(());
    };
    let raw = std::fs::read(request_path)
        .with_context(|| format!("reading stale shutdown request {}", request_path.display()))?;
    if !token.matches_bytes(&raw) {
        anyhow::bail!(
            "shutdown request changed while it was classified before PID publication: {}",
            request_path.display()
        );
    }
    let request: ShutdownRequest = serde_json::from_slice(&raw).with_context(|| {
        format!(
            "shutdown request is malformed and was preserved before PID publication: {}",
            request_path.display()
        )
    })?;
    if shutdown_request_matches(identity, &request) {
        return Ok(());
    }
    match crate::fs_ops::remove_exact(request_path, &token)? {
        crate::fs_ops::RemoveExactOutcome::Removed => Ok(()),
        crate::fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed => {
            anyhow::bail!(
                "stale shutdown request was removed before PID publication, but parent-directory durability is unconfirmed: {}",
                request_path.display()
            )
        }
    }
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
    Ok(running_pid_identity_checked()?.map(|identity| identity.pid))
}

pub(crate) fn running_identity_checked() -> Result<Option<(u32, PathBuf)>> {
    let path = pidfile_path()?;
    Ok(running_pid_identity_checked_at(&path)?.map(|identity| (identity.pid, identity.executable)))
}

pub(crate) fn running_generation_checked() -> Result<Option<DaemonGeneration>> {
    Ok(
        running_pid_identity_checked()?.map(|identity| DaemonGeneration {
            pid: identity.pid,
            generation: identity.generation,
        }),
    )
}

fn running_pid_identity_checked() -> Result<Option<PidIdentity>> {
    let path = pidfile_path()?;
    running_pid_identity_checked_at(&path)
}

#[cfg(test)]
fn running_pid_checked_at(path: &Path) -> Result<Option<u32>> {
    Ok(running_pid_identity_checked_at(path)?.map(|identity| identity.pid))
}

fn running_pid_identity_checked_at(path: &Path) -> Result<Option<PidIdentity>> {
    let initial_raw = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let Some(parent) = path.parent() else {
                return Ok(None);
            };
            match std::fs::symlink_metadata(parent) {
                Ok(metadata) if metadata.file_type().is_dir() => None,
                Ok(_) => {
                    anyhow::bail!("daemon PID parent is not a directory: {}", parent.display())
                }
                Err(parent_err) if parent_err.kind() == std::io::ErrorKind::NotFound => {
                    // With no state directory, neither the PID identity nor its
                    // authority lock can exist. This is an authoritative
                    // stopped snapshot and avoids creating state during a
                    // read-only installer probe.
                    return Ok(None);
                }
                Err(parent_err) => {
                    return Err(anyhow::anyhow!(
                        "inspecting daemon PID parent {}: {parent_err}",
                        parent.display()
                    ));
                }
            }
        }
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
            Ok(Some(identity))
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
                    (Some(identity), true) => Some(identity),
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
    let mut nonce = [0_u8; 16];
    rand::rng().fill_bytes(&mut nonce);
    hex::encode(nonce)
}

pub fn request_shutdown(expected: &DaemonGeneration) -> Result<ShutdownRequestOutcome> {
    let identity = running_pid_identity_checked()?
        .filter(|identity| {
            identity.pid == expected.pid && identity.generation == expected.generation
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Refusing to stop PID {}: daemon generation changed before shutdown request publication",
                expected.pid
            )
        })?;
    let pid = expected.pid;
    if identity.generation.is_empty() {
        anyhow::bail!(
            "Refusing to stop PID {pid}: daemon generation predates the exact shutdown-request protocol"
        );
    }
    let request = ShutdownRequest {
        version: 1,
        pid,
        generation: identity.generation.clone(),
    };
    let request_path = shutdown_request_path()?;
    let outcome = crate::auth::atomic_write_private(&request_path, &serde_json::to_vec(&request)?)?;
    let durability = match outcome {
        crate::auth::PrivateWriteOutcome::DurablyPublished => ShutdownRequestDurability::Durable,
        crate::auth::PrivateWriteOutcome::VisibleDurabilityUnconfirmed { cause } => {
            ShutdownRequestDurability::VisibleDurabilityUnconfirmed(cause.context(format!(
                "daemon shutdown request is visible at {}, but its durability is unconfirmed",
                request_path.display()
            )))
        }
    };
    Ok(ShutdownRequestOutcome {
        target: DaemonGeneration {
            pid,
            generation: expected.generation.clone(),
        },
        durability,
    })
}

fn shutdown_requested_at(request_path: &Path, target: &DaemonGeneration) -> bool {
    let Ok(request_raw) = std::fs::read(request_path) else {
        return false;
    };
    let Ok(request) = serde_json::from_slice::<ShutdownRequest>(&request_raw) else {
        return false;
    };
    request.version == 1 && request.pid == target.pid && request.generation == target.generation
}

fn shutdown_request_matches(identity: &PidIdentity, request: &ShutdownRequest) -> bool {
    request.version == 1 && request.pid == identity.pid && request.generation == identity.generation
}

fn release_pidfile_handle() -> Result<()> {
    let Some(handle) = PIDFILE_HANDLE.get() else {
        return Ok(());
    };
    let mut guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.take() {
        FileExt::unlock(&file).context("unlocking the daemon PID authority")?;
    }
    Ok(())
}

pub fn cleanup_pidfile() -> Result<()> {
    release_pidfile_handle()?;
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
    let remove_result = (|| -> Result<()> {
        let Some(token) = crate::fs_ops::token_if_present(path)? else {
            return Ok(());
        };
        match crate::fs_ops::remove_exact(path, &token)? {
            crate::fs_ops::RemoveExactOutcome::Removed => Ok(()),
            crate::fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed => {
                anyhow::bail!(
                    "daemon PID identity was removed, but parent-directory durability is unconfirmed: {}",
                    path.display()
                )
            }
        }
    })();
    let unlock_result = FileExt::unlock(&lock_file).map_err(|err| {
        anyhow::anyhow!(
            "unlocking daemon PID lock {}: {err}",
            pidfile_lock_path_for(path).display()
        )
    });
    match (remove_result, unlock_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(remove_error), Ok(())) => Err(remove_error),
        (Ok(()), Err(unlock_error)) => Err(unlock_error),
        (Err(remove_error), Err(unlock_error)) => Err(remove_error.context(format!(
            "PID cleanup failed and releasing its authority lock also failed: {unlock_error:#}"
        ))),
    }
}

/// RAII guard that cleans up the PID file on drop (including panics).
pub struct PidGuard {
    armed: bool,
}

impl PidGuard {
    pub fn new() -> Self {
        Self { armed: true }
    }

    pub fn cleanup(mut self) -> Result<()> {
        // Explicit cleanup owns the one material attempt. Do not make `Drop`
        // silently retry a failed namespace mutation.
        self.armed = false;
        cleanup_pidfile()
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = cleanup_pidfile()
        {
            eprintln!("Error: daemon PID cleanup during unwind failed: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::publish_pid_identity_at;
    use super::{
        ContendingDaemonIdentity, DaemonAbsenceAcquire, DaemonGeneration, PidIdentity,
        ShutdownRequest, ShutdownRequestMonitor, acquire_daemon_absence_lease_at,
        acquire_pidfile_lock_at, classify_contending_daemon_identity, cleanup_pidfile_at,
        parse_pid_identity, pidfile_lock_path_for, read_pid_from_raw, running_pid_checked_at,
        shutdown_request_matches, try_acquire_daemon_absence_lease_at,
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
    fn shutdown_monitor_uses_its_published_generation_without_a_pidfile_reread() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let request_path = dir.path().join("daemon.shutdown");
        let monitor = ShutdownRequestMonitor::at(
            request_path.clone(),
            DaemonGeneration {
                pid: 4242,
                generation: "published-generation".to_string(),
            },
        );

        std::fs::write(
            &request_path,
            serde_json::to_vec(&ShutdownRequest {
                version: 1,
                pid: 4242,
                generation: "published-generation".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(monitor.is_requested());

        std::fs::write(
            &request_path,
            serde_json::to_vec(&ShutdownRequest {
                version: 1,
                pid: 4242,
                generation: "stale-generation".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(!monitor.is_requested());

        std::fs::remove_file(&request_path).unwrap();
        assert!(!monitor.is_requested());
    }

    #[cfg(windows)]
    #[test]
    fn shutdown_request_published_after_pid_identity_is_never_deleted_by_startup() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let pid_path = dir.path().join("daemon.pid");
        let request_path = dir.path().join("daemon.shutdown");
        let stale = ShutdownRequest {
            version: 1,
            pid: 7,
            generation: "stale-generation".to_string(),
        };
        std::fs::write(&request_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from(r"C:\bin\codex-switch.exe"),
            generation: "new-generation".to_string(),
        };
        let writer_pid_path = pid_path.clone();
        let writer_request_path = request_path.clone();
        let writer_identity = PidIdentity {
            version: identity.version,
            pid: identity.pid,
            executable: identity.executable.clone(),
            generation: identity.generation.clone(),
        };
        let writer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if std::fs::read(&writer_pid_path)
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<PidIdentity>(&raw).ok())
                    .is_some_and(|published| {
                        published.pid == writer_identity.pid
                            && published.generation == writer_identity.generation
                    })
                {
                    let request = ShutdownRequest {
                        version: 1,
                        pid: writer_identity.pid,
                        generation: writer_identity.generation,
                    };
                    std::fs::write(&writer_request_path, serde_json::to_vec(&request).unwrap())
                        .unwrap();
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "PID publication was not observed"
                );
                std::thread::yield_now();
            }
        });

        let lock = publish_pid_identity_at(&pid_path, Some(&request_path), &identity).unwrap();
        writer.join().unwrap();
        let request: ShutdownRequest =
            serde_json::from_slice(&std::fs::read(&request_path).unwrap()).unwrap();
        assert!(shutdown_request_matches(&identity, &request));

        FileExt::unlock(&lock).unwrap();
        cleanup_pidfile_at(&pid_path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn malformed_shutdown_residue_is_preserved_before_pid_publication() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let pid_path = dir.path().join("daemon.pid");
        let request_path = dir.path().join("daemon.shutdown");
        std::fs::write(&request_path, b"not a shutdown request").unwrap();
        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from(r"C:\bin\codex-switch.exe"),
            generation: "new-generation".to_string(),
        };

        let error = publish_pid_identity_at(&pid_path, Some(&request_path), &identity)
            .expect_err("malformed foreign residue must fail closed");
        assert!(error.to_string().contains("malformed"), "{error:#}");
        assert_eq!(
            std::fs::read(&request_path).unwrap(),
            b"not a shutdown request"
        );
        assert!(!pid_path.exists());
    }

    #[test]
    fn checked_running_pid_uses_the_lock_without_a_process_list_probe() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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
    fn checked_running_pid_treats_a_missing_state_directory_as_stopped_without_creating_it() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let state_dir = dir.path().join("missing-state");
        let path = state_dir.join("daemon.pid");

        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
        assert!(!state_dir.exists());
    }

    #[test]
    fn checked_running_pid_never_folds_a_malformed_locked_file_into_stopped() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        std::fs::write(&path, b"4242").unwrap();

        assert_eq!(running_pid_checked_at(&path).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn checked_running_pid_never_trusts_a_locked_legacy_numeric_file() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
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

    #[test]
    fn self_update_absence_lease_blocks_foreground_start_until_commit_boundary() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from("codex-switch"),
            generation: "stopped-generation".to_string(),
        };
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();

        let expected = DaemonGeneration {
            pid: 4242,
            generation: "stopped-generation".to_string(),
        };
        let absence = acquire_daemon_absence_lease_at(&path, Some(&expected)).unwrap();
        assert!(!path.exists());
        absence.verify().unwrap();

        let contender_path = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            tx.send(acquire_pidfile_lock_at(&contender_path).map(|_| ()))
                .unwrap();
        });
        let error = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("foreground start must fail immediately while update owns absence")
            .expect_err("foreground start entered the self-update replacement window");
        assert!(error.to_string().contains("another daemon is running"));
        contender.join().unwrap();

        // The updater releases this guard only after commit/rollback outcome.
        drop(absence);
        let foreground = acquire_pidfile_lock_at(&path).unwrap();
        FileExt::unlock(&foreground).unwrap();
    }

    #[test]
    fn fresh_install_absence_lease_blocks_first_foreground_generation() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let absence = acquire_daemon_absence_lease_at(&path, None).unwrap();
        absence.verify().unwrap();

        let contender_path = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let contender = std::thread::spawn(move || {
            tx.send(acquire_pidfile_lock_at(&contender_path).map(|_| ()))
                .unwrap();
        });
        let error = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("fresh foreground start must fail immediately during publication")
            .expect_err("fresh foreground daemon entered the installer rollback window");
        assert!(error.to_string().contains("another daemon is running"));
        contender.join().unwrap();

        drop(absence);
        let foreground = acquire_pidfile_lock_at(&path).unwrap();
        FileExt::unlock(&foreground).unwrap();
    }

    #[test]
    fn contender_exit_before_pid_publication_becomes_exact_absence() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let contender = acquire_pidfile_lock_at(&path).unwrap();
        assert!(matches!(
            try_acquire_daemon_absence_lease_at(&path, None).unwrap(),
            DaemonAbsenceAcquire::Contended
        ));

        FileExt::unlock(&contender).unwrap();
        let absence = match try_acquire_daemon_absence_lease_at(&path, None).unwrap() {
            DaemonAbsenceAcquire::Acquired(lease) => lease,
            DaemonAbsenceAcquire::Contended => {
                panic!("released pre-publication contender still blocked absence")
            }
        };
        absence.verify().unwrap();
    }

    #[test]
    fn self_update_absence_lease_preserves_a_different_stale_generation() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let identity = PidIdentity {
            version: 2,
            pid: 4242,
            executable: PathBuf::from("codex-switch"),
            generation: "unexpected-generation".to_string(),
        };
        let encoded = serde_json::to_vec(&identity).unwrap();
        std::fs::write(&path, &encoded).unwrap();

        let expected = DaemonGeneration {
            pid: 4242,
            generation: "expected-generation".to_string(),
        };
        let error = acquire_daemon_absence_lease_at(&path, Some(&expected))
            .expect_err("a changed daemon generation must fail closed");
        assert!(
            error.to_string().contains("generation changed"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), encoded);

        let lock = acquire_pidfile_lock_at(&path).unwrap();
        FileExt::unlock(&lock).unwrap();
    }

    #[test]
    fn same_pid_with_a_different_generation_is_a_published_contender() {
        let expected = DaemonGeneration {
            pid: 4242,
            generation: "stopped-generation".to_string(),
        };
        let successor = PidIdentity {
            version: 2,
            pid: expected.pid,
            executable: PathBuf::from("codex-switch"),
            generation: "successor-generation".to_string(),
        };
        let raw = serde_json::to_string(&successor).unwrap();

        match classify_contending_daemon_identity(&raw, Some(&expected)) {
            ContendingDaemonIdentity::Published(observed) => {
                assert_eq!(observed.pid, expected.pid);
                assert_eq!(observed.generation, successor.generation);
            }
            ContendingDaemonIdentity::Pending => {
                panic!("PID reuse with a different generation must not remain pending")
            }
        }
    }
}
