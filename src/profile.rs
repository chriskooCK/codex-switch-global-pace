use std::fs::{File, OpenOptions};
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use sha2::{Digest as _, Sha256};

use crate::auth::{
    ConditionalWrite, app_home, atomic_write_private, atomic_write_private_if_unchanged,
    codex_auth_path, current_file, profiles_dir, read_auth, require_durable_private_write,
    write_auth,
};
use crate::error::CsError;
use crate::jwt::{StrictAccountBinding, parse_account_info};
use crate::output::{user_print, user_println};

const MAX_ALIAS_LEN: usize = 64;
const PROFILE_CONCURRENCY_RETRY_LIMIT: usize = 8;

pub fn profile_auth_path(alias: &str) -> Result<PathBuf> {
    validate_alias(alias)?;
    let profiles_dir = profiles_dir()?;
    Ok(profile_auth_path_for_validated_alias(&profiles_dir, alias))
}

fn profile_auth_path_for_validated_alias(profiles_dir: &Path, alias: &str) -> PathBuf {
    profile_dir_for_validated_alias(profiles_dir, alias).join("auth.json")
}

fn profile_dir_for_validated_alias(profiles_dir: &Path, alias: &str) -> PathBuf {
    profiles_dir.join(alias)
}

/// A validated profile-registry root reused for one logical batch operation.
///
/// Alias listings are deliberately read afresh on every call so existing
/// concurrency checks keep observing registry changes. Only application-home
/// resolution is shared; every path lookup still validates its alias before
/// joining it beneath this root.
struct ProfileRegistry {
    profiles_dir: PathBuf,
}

struct ProfileAuthSnapshot {
    alias: String,
    path: PathBuf,
    bytes: Vec<u8>,
}

struct ProfileRegistrySnapshot {
    profiles: Vec<ProfileAuthSnapshot>,
}

#[cfg(test)]
thread_local! {
    static TEST_PROFILE_REGISTRY_SNAPSHOT_COUNT: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static TEST_AFTER_PROFILE_POLICY_VALIDATION:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
            std::cell::RefCell::new(None)
        };
}

#[cfg(test)]
fn reset_profile_registry_snapshot_count() {
    TEST_PROFILE_REGISTRY_SNAPSHOT_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn profile_registry_snapshot_count() -> usize {
    TEST_PROFILE_REGISTRY_SNAPSHOT_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn after_next_profile_policy_validation(action: impl FnOnce() + 'static) {
    TEST_AFTER_PROFILE_POLICY_VALIDATION.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(action));
    });
}

#[cfg(test)]
fn run_after_profile_policy_validation_test_hook() {
    TEST_AFTER_PROFILE_POLICY_VALIDATION.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

struct ParsedProfileAuthSnapshot {
    alias: String,
    value: serde_json::Value,
}

struct ParsedProfileRegistrySnapshot {
    profiles: Vec<ParsedProfileAuthSnapshot>,
}

/// One immutable, parsed view of the saved profile registry for batch callers.
/// Resolving the registry root and reading each auth file happens exactly once
/// for the complete batch.
#[derive(Debug)]
pub(crate) struct ProfileAccountSnapshot {
    pub(crate) alias: String,
    pub(crate) path: PathBuf,
    pub(crate) info: crate::jwt::AccountInfo,
}

/// The exact registry generation used by the locked active-profile recheck.
///
/// This owns bytes only. The profile lease and auth transaction used to prove
/// `current` are released before the value reaches a caller. Raw credential
/// snapshots remain private to this module; batch callers can only project the
/// non-current account metadata they need for a later binding revalidation.
pub(crate) struct SyncedProfileRegistry {
    current: String,
    snapshot: ProfileRegistrySnapshot,
}

impl SyncedProfileRegistry {
    pub(crate) fn current(&self) -> &str {
        &self.current
    }

    /// Parse the retained registry bytes only when daemon candidate ranking is
    /// actually needed. The current profile is deliberately skipped: its fresh
    /// post-probe metadata is authoritative for scoring, and parsing it here
    /// would change the error boundary for a snapshot repaired before probing.
    pub(crate) fn into_candidate_accounts(self) -> Result<Vec<ProfileAccountSnapshot>> {
        let Self { current, snapshot } = self;
        snapshot
            .profiles
            .into_iter()
            .filter(|profile| profile.alias != current)
            .map(|profile| {
                let ProfileAuthSnapshot { alias, path, bytes } = profile;
                let value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                Ok(ProfileAccountSnapshot {
                    alias,
                    path,
                    info: crate::auth::account_info_from_auth_value(&value),
                })
            })
            .collect()
    }

    fn into_current(self) -> String {
        self.current
    }
}

impl ProfileRegistry {
    fn open() -> Result<Self> {
        Ok(Self {
            profiles_dir: profiles_dir()?,
        })
    }

    fn auth_path(&self, alias: &str) -> Result<PathBuf> {
        validate_alias(alias)?;
        Ok(profile_auth_path_for_validated_alias(
            &self.profiles_dir,
            alias,
        ))
    }

    fn profile_dir(&self, alias: &str) -> Result<PathBuf> {
        validate_alias(alias)?;
        Ok(profile_dir_for_validated_alias(&self.profiles_dir, alias))
    }

    fn list_profiles(&self) -> Result<Vec<String>> {
        list_profiles_in(&self.profiles_dir)
    }

    fn snapshot(&self) -> Result<ProfileRegistrySnapshot> {
        let aliases = self.list_profiles()?;
        self.snapshot_aliases(&aliases)
    }

    fn snapshot_aliases(&self, aliases: &[String]) -> Result<ProfileRegistrySnapshot> {
        #[cfg(test)]
        TEST_PROFILE_REGISTRY_SNAPSHOT_COUNT.with(|count| count.set(count.get() + 1));
        let mut profiles = Vec::with_capacity(aliases.len());
        for alias in aliases {
            let path = self.auth_path(alias)?;
            let bytes = std::fs::read(&path).with_context(|| {
                format!("reading existing profile '{alias}' at {}", path.display())
            })?;
            profiles.push(ProfileAuthSnapshot {
                alias: alias.clone(),
                path,
                bytes,
            });
        }
        Ok(ProfileRegistrySnapshot { profiles })
    }
}

impl ProfileRegistrySnapshot {
    fn exact_matches(&self, target: &[u8]) -> Vec<String> {
        self.profiles
            .iter()
            .filter(|profile| profile.bytes == target)
            .map(|profile| profile.alias.clone())
            .collect()
    }

    fn parse(&self) -> Result<ParsedProfileRegistrySnapshot> {
        let mut profiles = Vec::with_capacity(self.profiles.len());
        for profile in &self.profiles {
            let value = serde_json::from_slice(&profile.bytes).with_context(|| {
                format!(
                    "loading profile '{}' from {}",
                    profile.alias,
                    profile.path.display()
                )
            })?;
            profiles.push(ParsedProfileAuthSnapshot {
                alias: profile.alias.clone(),
                value,
            });
        }
        Ok(ParsedProfileRegistrySnapshot { profiles })
    }
}

impl ParsedProfileRegistrySnapshot {
    fn equivalent_matches(&self, target: &serde_json::Value) -> Vec<String> {
        self.profiles
            .iter()
            .filter(|profile| auth_values_semantically_equal(&profile.value, target))
            .map(|profile| profile.alias.clone())
            .collect()
    }

    fn identity_matches(&self, identity: &AccountIdentity) -> IdentityMatches {
        collect_identity_matches(
            identity,
            self.profiles
                .iter()
                .map(|profile| (profile.alias.as_str(), &profile.value)),
        )
    }

    fn profile(&self, alias: &str) -> Option<&serde_json::Value> {
        self.profiles
            .iter()
            .find(|profile| profile.alias == alias)
            .map(|profile| &profile.value)
    }
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() {
        anyhow::bail!("alias cannot be empty");
    }
    if alias == "." || alias == ".." {
        anyhow::bail!("alias cannot be '.' or '..'");
    }
    if alias.len() > MAX_ALIAS_LEN {
        anyhow::bail!("alias must be at most {MAX_ALIAS_LEN} characters");
    }
    if !alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        anyhow::bail!("alias may only contain ASCII letters, digits, '_', '-', '.'");
    }
    Ok(())
}

pub fn list_profiles() -> Result<Vec<String>> {
    ProfileRegistry::open()?.list_profiles()
}

pub(crate) fn load_profile_accounts() -> Result<Vec<ProfileAccountSnapshot>> {
    load_profile_accounts_with_validation(false)
}

pub(crate) fn load_profile_accounts_checked() -> Result<Vec<ProfileAccountSnapshot>> {
    load_profile_accounts_with_validation(true)
}

pub(crate) fn load_profile_accounts_checked_with_active()
-> Result<(Vec<ProfileAccountSnapshot>, Option<String>)> {
    let registry = ProfileRegistry::open()?;
    let snapshot = registry.snapshot()?;
    let parsed = snapshot.parse()?;
    let policy = crate::auth::ManagedAuthPolicySnapshot::load()?;
    let accounts = profile_accounts_from_snapshot(&snapshot, &parsed, Some(&policy))?;
    let live_path = policy.codex_auth_path()?;
    let active = active_profile_from_registry_snapshot(&live_path, &snapshot, &parsed)?;
    Ok((accounts, active))
}

fn load_profile_accounts_with_validation(
    validate_managed_policy: bool,
) -> Result<Vec<ProfileAccountSnapshot>> {
    let registry = ProfileRegistry::open()?;
    let snapshot = registry.snapshot()?;
    let parsed = snapshot.parse()?;
    let policy = validate_managed_policy
        .then(crate::auth::ManagedAuthPolicySnapshot::load)
        .transpose()?;
    profile_accounts_from_snapshot(&snapshot, &parsed, policy.as_ref())
}

fn profile_accounts_from_snapshot(
    snapshot: &ProfileRegistrySnapshot,
    parsed: &ParsedProfileRegistrySnapshot,
    managed_policy: Option<&crate::auth::ManagedAuthPolicySnapshot>,
) -> Result<Vec<ProfileAccountSnapshot>> {
    snapshot
        .profiles
        .iter()
        .zip(&parsed.profiles)
        .map(|(raw, parsed)| {
            debug_assert_eq!(raw.alias, parsed.alias);
            let info = crate::auth::account_info_from_auth_value(&parsed.value);
            if let Some(policy) = managed_policy {
                policy
                    .validate_account_info(&info)
                    .with_context(|| format!("validating profile '{}'", parsed.alias))?;
                #[cfg(test)]
                run_after_profile_policy_validation_test_hook();
            }
            Ok(ProfileAccountSnapshot {
                alias: parsed.alias.clone(),
                path: raw.path.clone(),
                info,
            })
        })
        .collect()
}

fn list_profiles_in(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading profiles directory {}", dir.display()));
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let path = entry.path();
        if !entry
            .file_type()
            .with_context(|| format!("reading file type of {}", path.display()))?
            .is_dir()
        {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if validate_alias(&name).is_ok() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentMarkerSnapshot {
    alias: String,
    bytes: Vec<u8>,
}

impl CurrentMarkerSnapshot {
    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }
}

pub(crate) fn read_current_marker_snapshot_checked() -> Result<Option<CurrentMarkerSnapshot>> {
    let path = current_file()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading current profile marker {}", path.display()));
        }
    };
    let value = std::str::from_utf8(&bytes)
        .with_context(|| format!("current profile marker {} is not UTF-8", path.display()))?;
    let alias = value.trim();
    if alias.is_empty() {
        anyhow::bail!("current profile marker {} is empty", path.display());
    }
    validate_alias(alias)
        .with_context(|| format!("validating current profile marker {}", path.display()))?;
    Ok(Some(CurrentMarkerSnapshot {
        alias: alias.to_string(),
        bytes,
    }))
}

pub(crate) fn read_current_checked() -> Result<Option<String>> {
    read_current_marker_snapshot_checked().map(|snapshot| snapshot.map(|snapshot| snapshot.alias))
}

fn path_exists_checked(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("reading metadata for {}", path.display()))
        }
    }
}

pub(crate) fn profile_exists(alias: &str) -> Result<bool> {
    path_exists_checked(&profile_auth_path(alias)?)
}

fn read_existing_auth(path: &Path) -> Result<Option<serde_json::Value>> {
    if path_exists_checked(path)? {
        return read_auth(path).map(Some);
    }
    Ok(None)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    crate::auth::ensure_private_directory(path)
}

fn ensure_profile_parent(path: &Path) -> Result<()> {
    ensure_private_dir(&app_home()?)?;
    ensure_private_dir(&profiles_dir()?)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    Ok(())
}

fn deleted_profiles_dir() -> Result<PathBuf> {
    Ok(app_home()?.join("deleted-profiles"))
}

fn auth_lock_path() -> Result<PathBuf> {
    Ok(app_home()?.join("auth.lock"))
}

fn legacy_launch_lock_path() -> Result<PathBuf> {
    Ok(app_home()?.join("launch.lock"))
}

/// Maximum time to wait for an auth-related lock. A timeout is reported rather
/// than replacing the inode because an OS lock is the only reliable liveness signal.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn lock_live_auth() -> Result<File> {
    let path = auth_lock_path()?;
    acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, "auth")
}

/// Coordinate with older `codex-switch` binaries that may still stage and
/// restore live authentication while sharing `CODEX_SWITCH_HOME`.
fn lock_legacy_launch_compatibility() -> Result<File> {
    let path = legacy_launch_lock_path()?;
    acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, "legacy launch compatibility")
}

struct AuthTransaction {
    _legacy_launch: File,
    _auth: File,
}

fn lock_auth_transaction() -> Result<AuthTransaction> {
    // Preserve the historical lock order so this binary cannot deadlock with
    // an older shared-state process: launch.lock, then auth.lock.
    let started = Instant::now();
    let legacy_launch = lock_legacy_launch_compatibility()?;
    let legacy_lock_ms = started.elapsed().as_millis();
    let auth_started = Instant::now();
    let auth = lock_live_auth()?;
    let auth_lock_ms = auth_started.elapsed().as_millis();
    let transaction = AuthTransaction {
        _legacy_launch: legacy_launch,
        _auth: auth,
    };
    let live_path = codex_auth_path()?;
    let recovery_started = Instant::now();
    crate::auth::recover_interrupted_auth_publication(&live_path).with_context(|| {
        format!(
            "recovering an interrupted live-auth publication at {}",
            live_path.display()
        )
    })?;
    tracing::debug!(
        legacy_lock_ms,
        auth_lock_ms,
        recovery_ms = recovery_started.elapsed().as_millis(),
        total_ms = started.elapsed().as_millis(),
        "live-auth transaction lock acquired"
    );
    Ok(transaction)
}

/// An OS-backed per-profile lease. Keep this value alive for the complete
/// credential operation; dropping it releases the lease.
pub(crate) struct ProfileLease {
    alias: String,
    _file: File,
}

impl ProfileLease {
    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }
}

fn profile_lease_path(alias: &str) -> Result<PathBuf> {
    validate_alias(alias)?;
    Ok(app_home()?.join("locks").join(format!("{alias}.lock")))
}

const PROFILE_LEASE_WAITING: u8 = 0;
const PROFILE_LEASE_ACQUIRED: u8 = 1;
const PROFILE_LEASE_CANCELLED: u8 = 2;

/// Coordinates cancellation only while an async profile lease is still
/// waiting. Once acquisition wins the state transition, cancellation is a
/// no-op and the credential operation must be allowed to finish.
#[derive(Clone, Debug)]
pub(crate) struct ProfileLeaseAcquireControl {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    wake: std::sync::Arc<tokio::sync::Notify>,
}

impl ProfileLeaseAcquireControl {
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(PROFILE_LEASE_WAITING)),
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Cancel a task that has not acquired its profile lease yet. The compare
    /// and exchange is the boundary that prevents shutdown from cancelling a
    /// task after it may have started a credential-mutating request.
    pub(crate) fn cancel_waiting(&self) -> bool {
        let cancelled = self
            .state
            .compare_exchange(
                PROFILE_LEASE_WAITING,
                PROFILE_LEASE_CANCELLED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok();
        if cancelled {
            self.wake.notify_one();
        }
        cancelled
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::Acquire) == PROFILE_LEASE_CANCELLED
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.wake.notified().await;
        }
    }

    fn mark_acquired(&self) -> bool {
        self.state
            .compare_exchange(
                PROFILE_LEASE_WAITING,
                PROFILE_LEASE_ACQUIRED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

pub(crate) fn acquire_profile_lease(alias: &str) -> Result<ProfileLease> {
    let path = profile_lease_path(alias)?;
    // Synchronous UI mutations must never freeze forever behind another
    // process's interactive OAuth session. They fail closed before doing any
    // credential or network work when the established lock timeout expires.
    let file = acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, &format!("profile '{alias}'"))?;
    Ok(ProfileLease {
        alias: alias.to_string(),
        _file: file,
    })
}

/// Async profile-lease acquisition with an explicit pre-acquisition
/// cancellation boundary. It never creates an unabortable blocking worker:
/// contention is polled with `try_lock`, a Tokio timer, and the control's wake
/// signal.
pub(crate) async fn acquire_profile_lease_async_cancellable(
    alias: impl Into<String>,
    control: &ProfileLeaseAcquireControl,
) -> Result<Option<ProfileLease>> {
    let alias = alias.into();
    let path = profile_lease_path(&alias)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let file = open_lock_file(&path)?;
    let label = format!("profile '{alias}'");

    loop {
        if control.is_cancelled() {
            return Ok(None);
        }
        match FileExt::try_lock(&file) {
            Ok(()) => {
                if !control.mark_acquired() {
                    // Shutdown won the only state transition while this poll
                    // was acquiring the OS lock. Dropping the handle releases
                    // it before any credential or network work can begin.
                    return Ok(None);
                }
                write_lock_holder(&file);
                return Ok(Some(ProfileLease { alias, _file: file }));
            }
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                notify_test_lock_attempt(&label);
                tokio::select! {
                    _ = control.cancelled() => return Ok(None),
                    _ = tokio::time::sleep(LOCK_POLL_INTERVAL) => {}
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("acquiring {label} lock {}", path.display()));
            }
        }
    }
}

/// CLI and background callers deliberately wait until the profile is free.
/// The async polling implementation remains cancellable by dropping its future
/// and leaves no detached `spawn_blocking` waiter behind at runtime shutdown.
pub(crate) async fn acquire_profile_lease_async(alias: impl Into<String>) -> Result<ProfileLease> {
    let control = ProfileLeaseAcquireControl::new();
    acquire_profile_lease_async_cancellable(alias, &control)
        .await?
        .ok_or_else(|| anyhow::anyhow!("profile lease acquisition was cancelled unexpectedly"))
}

pub(crate) fn acquire_profile_leases(aliases: &[&str]) -> Result<Vec<ProfileLease>> {
    let mut aliases = aliases
        .iter()
        .map(|alias| {
            validate_alias(alias)?;
            Ok((*alias).to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    aliases.sort();
    aliases.dedup();
    aliases
        .iter()
        .map(|alias| acquire_profile_lease(alias))
        .collect()
}

fn acquire_file_lock(path: &Path, timeout: Duration, label: &str) -> Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let file = open_lock_file(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => {
                write_lock_holder(&file);
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                notify_test_lock_attempt(label);
                if Instant::now() >= deadline {
                    let holder =
                        read_lock_holder(path).unwrap_or_else(|| "unknown holder".to_string());
                    anyhow::bail!(
                        "{label} lock {} remained held for {:.3}s by {holder}; refusing to replace the live lock file",
                        path.display(),
                        timeout.as_secs_f64(),
                    );
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::Error(e)) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LOCK_ATTEMPT_NOTIFIER:
        std::cell::RefCell<Option<(String, std::sync::mpsc::Sender<()>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn notify_on_test_lock_attempt(label: &str, sender: std::sync::mpsc::Sender<()>) {
    TEST_LOCK_ATTEMPT_NOTIFIER.with(|notifier| {
        *notifier.borrow_mut() = Some((label.to_string(), sender));
    });
}

#[cfg(test)]
fn notify_test_lock_attempt(label: &str) {
    TEST_LOCK_ATTEMPT_NOTIFIER.with(|notifier| {
        let should_notify = notifier
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| target == label);
        if should_notify && let Some((_, sender)) = notifier.borrow_mut().take() {
            let _ = sender.send(());
        }
    });
}

/// Open a stable lock inode. Permission/ownership errors are reported rather
/// than recovered by unlinking because another process may still hold it.
fn open_lock_file(path: &Path) -> Result<File> {
    try_open_lock_file(path).with_context(|| {
        format!(
            "opening auth lock {}; check the file and parent directory ownership",
            path.display()
        )
    })
}

fn try_open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Best-effort: write `pid epoch_secs` to the lock file for diagnostics.
/// Failure is non-fatal — the OS-level flock is the source of truth.
fn write_lock_holder(file: &File) {
    use std::io::Seek;
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{pid} {ts}\n");
    let _ = file.set_len(0);
    let mut f = file;
    let _ = f.seek(std::io::SeekFrom::Start(0));
    let _ = f.write_all(line.as_bytes());
}

fn read_lock_holder(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_current(alias: &str) -> Result<()> {
    let path = current_file()?;
    let outcome = atomic_write_private(&path, alias.as_bytes())
        .with_context(|| format!("writing current profile marker {}", path.display()))?;
    require_durable_private_write(&path, "current profile marker", outcome)
}

#[cfg(test)]
thread_local! {
    static TEST_BEFORE_ACTIVATION_LIVE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static TEST_AFTER_EXACT_LIVE_BINDING: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static TEST_AFTER_IMPORT_REGISTRY_SCAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static TEST_AFTER_UPDATE_PROFILE_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
static TEST_FAIL_NEXT_ACTIVATION_MARKER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static TEST_AFTER_PARTIAL_ACTIVATION: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn fail_next_activation_marker_write() {
    TEST_FAIL_NEXT_ACTIVATION_MARKER.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn after_next_partial_activation(action: impl FnOnce() + Send + 'static) {
    *TEST_AFTER_PARTIAL_ACTIVATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(action));
}

#[cfg(test)]
fn run_after_partial_activation_test_hook() {
    let action = TEST_AFTER_PARTIAL_ACTIVATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(action) = action {
        action();
    }
}

#[cfg(test)]
fn before_next_activation_live_publish(action: impl FnOnce() + 'static) {
    TEST_BEFORE_ACTIVATION_LIVE_PUBLISH.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_before_activation_live_publish_test_hook() {
    TEST_BEFORE_ACTIVATION_LIVE_PUBLISH.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(test)]
fn after_next_exact_live_binding(action: impl FnOnce() + 'static) {
    TEST_AFTER_EXACT_LIVE_BINDING.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_exact_live_binding_test_hook() {
    TEST_AFTER_EXACT_LIVE_BINDING.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(test)]
fn after_next_import_registry_scan(action: impl FnOnce() + 'static) {
    TEST_AFTER_IMPORT_REGISTRY_SCAN.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_after_import_registry_scan_test_hook() {
    TEST_AFTER_IMPORT_REGISTRY_SCAN.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(test)]
fn after_next_update_profile_write(action: impl FnOnce() + 'static) {
    TEST_AFTER_UPDATE_PROFILE_WRITE.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_after_update_profile_write_test_hook() {
    TEST_AFTER_UPDATE_PROFILE_WRITE.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

fn write_activation_marker(alias: &str) -> Result<()> {
    #[cfg(test)]
    if TEST_FAIL_NEXT_ACTIVATION_MARKER.swap(false, std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("injected activation marker failure");
    }
    write_current(alias)
}

fn clear_current() -> Result<()> {
    let path = current_file()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("removing current profile marker {}", path.display())),
    }
}

/// A switch can publish the new live credential before its derived marker or
/// publication-recovery housekeeping completes. Callers must not interpret
/// this error as "no switch happened"; `alias` is the credential that this
/// transaction published. A non-cooperating Codex writer may replace it after
/// publication, so callers that display current state must re-read live auth.
#[derive(Debug, thiserror::Error)]
#[error(
    "profile '{alias}' was published to live auth, but activation did not fully complete; live credentials were not rolled back: {source:#}"
)]
pub(crate) struct PartialProfileActivation {
    alias: String,
    published_digest: [u8; 32],
    #[source]
    source: anyhow::Error,
}

impl PartialProfileActivation {
    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }
}

fn partial_profile_activation_error(
    alias: &str,
    published_live: &[u8],
    source: anyhow::Error,
) -> anyhow::Error {
    #[cfg(test)]
    run_after_partial_activation_test_hook();
    anyhow::Error::new(PartialProfileActivation {
        alias: alias.to_string(),
        published_digest: Sha256::digest(published_live).into(),
        source,
    })
}

fn snapshot_optional_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn restore_file_snapshot(path: &Path, snapshot: Option<&[u8]>) -> Result<()> {
    match snapshot {
        Some(contents) => {
            let outcome = atomic_write_private(path, contents)
                .with_context(|| format!("restoring {}", path.display()))?;
            require_durable_private_write(path, "restored private file", outcome)
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("removing newly-created {}", path.display()))
            }
        },
    }
}

/// Commit the two files that define activation while the caller holds the auth
/// transaction and target profile lease.
///
/// Live auth is the source of truth, so it is published first. Once visible it
/// is never rolled back: Codex does not participate in this app's transaction
/// lock, and a rollback could destroy credentials Codex wrote in the meantime.
/// The marker is derived state and a failure to update it is reported as a
/// partial activation that the normal live-auth reconciliation can repair.
fn activation_is_already_exact(
    alias: &str,
    value: &serde_json::Value,
    live_path: &Path,
    expected_live: Option<&[u8]>,
) -> Result<bool> {
    let Some(expected_live) = expected_live else {
        return Ok(false);
    };
    if snapshot_optional_file(live_path)?.as_deref() != Some(expected_live) {
        return Ok(false);
    }
    let Ok(live_value) = serde_json::from_slice::<serde_json::Value>(expected_live) else {
        return Ok(false);
    };
    if &live_value != value {
        return Ok(false);
    }

    // An unreadable or stale marker is an optimization miss: the normal
    // publication path below repairs it and preserves the existing error
    // behavior. Only an exact three-way binding can skip durable writes.
    Ok(read_current_checked().ok().flatten().as_deref() == Some(alias))
}

fn commit_activation_if_live_unchanged(
    alias: &str,
    value: &serde_json::Value,
    expected_live: Option<&[u8]>,
) -> Result<()> {
    validate_alias(alias)?;
    crate::auth::validate_managed_auth_value(value)?;
    let live_path = codex_auth_path()?;
    let published_live = serde_json::to_string_pretty(value)?.into_bytes();
    if activation_is_already_exact(alias, value, &live_path, expected_live)? {
        return Ok(());
    }

    #[cfg(test)]
    run_before_activation_live_publish_test_hook();
    let publication = atomic_write_private_if_unchanged(&live_path, expected_live, &published_live);
    match publication {
        Ok(ConditionalWrite::Written) => write_activation_marker(alias).map_err(|source| {
            partial_profile_activation_error(
                alias,
                &published_live,
                source.context("updating the activation marker failed"),
            )
        }),
        Ok(ConditionalWrite::Changed) => anyhow::bail!(
            "live auth changed before profile '{alias}' could be published; live credentials and the activation marker were not overwritten, retry the switch"
        ),
        Ok(ConditionalWrite::PublishedRecoveryRequired(detail)) => {
            Err(partial_profile_activation_error(
                alias,
                &published_live,
                anyhow::anyhow!(
                    "durable publication recovery is incomplete and the activation marker was not changed: {detail}"
                ),
            ))
        }
        Ok(ConditionalWrite::RestoredRecoveryRequired(detail)) => anyhow::bail!(
            "profile '{alias}' was not activated and the previous/foreign live credential was restored, but private recovery cleanup is incomplete; the activation marker was not changed: {detail}"
        ),
        Ok(ConditionalWrite::AmbiguousRecoveryRequired(detail)) => anyhow::bail!(
            "profile '{alias}' activation reached an ambiguous filesystem state; no recovery artifact was overwritten and the activation marker was not changed: {detail}"
        ),
        Err(error) => Err(error.context(format!(
            "publishing live auth for profile '{alias}' failed; live credentials and the activation marker were not changed"
        ))),
    }
}

/// Activate newly minted credentials after they are durable in the profile.
/// Unlike a normal switch, this must never restore the previous live refresh
/// token: the auth server has already invalidated it. A failure therefore
/// leaves the new credential in the profile (and, when that write succeeded,
/// live) and reports precisely which activation step remains incomplete.
fn commit_fresh_credentials_activation(
    alias: &str,
    value: &serde_json::Value,
    expected_live: Option<&[u8]>,
) -> Result<()> {
    commit_activation_if_live_unchanged(alias, value, expected_live).with_context(|| {
        format!(
            "new credentials are preserved in profile '{alias}', but exact live-auth activation is incomplete; the previous refresh token may already be invalid"
        )
    })
}

fn live_belongs_to_profile_locked(registry: &ProfileRegistry, alias: &str) -> Result<bool> {
    let profile_path = registry.auth_path(alias)?;
    let Some(profile) = read_existing_auth(&profile_path)? else {
        return Ok(false);
    };
    let live_path = codex_auth_path()?;
    let Some(live_snapshot) = snapshot_optional_file(&live_path)? else {
        return Ok(false);
    };
    live_snapshot_belongs_to_auth_value_in_registry(registry, alias, &profile, &live_snapshot)
}

fn live_snapshot_belongs_to_profile_locked(alias: &str, live_snapshot: &[u8]) -> Result<bool> {
    let registry = ProfileRegistry::open()?;
    let profile_path = registry.auth_path(alias)?;
    let Some(profile) = read_existing_auth(&profile_path)? else {
        return Ok(false);
    };
    live_snapshot_belongs_to_auth_value_in_registry(&registry, alias, &profile, live_snapshot)
}

fn live_snapshot_belongs_to_auth_value_locked(
    alias: &str,
    profile: &serde_json::Value,
    live_snapshot: &[u8],
) -> Result<bool> {
    let registry = ProfileRegistry::open()?;
    live_snapshot_belongs_to_auth_value_in_registry(&registry, alias, profile, live_snapshot)
}

fn live_snapshot_belongs_to_auth_value_in_registry(
    registry: &ProfileRegistry,
    alias: &str,
    profile: &serde_json::Value,
    live_snapshot: &[u8],
) -> Result<bool> {
    let live: serde_json::Value = serde_json::from_slice(live_snapshot)
        .context("parsing the exact live-auth snapshot used for ownership validation")?;

    let current = read_current_checked()?;
    // A refresh writes the profile before it decides whether the live copy must
    // follow. The pre-write snapshot supplied by that caller is therefore the
    // strongest proof that the marker still names these live credentials. This
    // also disambiguates legacy duplicate profiles without consulting identity.
    if current.as_deref() == Some(alias) && profile == &live {
        return Ok(true);
    }

    // Prefer an exact stored-credential binding over identity. In particular,
    // two aliases may legitimately survive from older releases with the same
    // account_id/email; refreshing the inactive one must not make it active.
    let exact = find_matching_profiles_for_bytes_in_registry(registry, live_snapshot)?;
    if let Some(bound) = select_exact_profile_binding(&exact, current.as_deref())? {
        return Ok(bound == alias);
    }

    // When Codex itself rotated live auth, no saved file has the new bytes yet.
    // The current marker may still bind that change, but only to the alias it
    // names and only with a complete strict identity match. Identity by itself
    // never chooses an alias.
    if current.as_deref() != Some(alias) {
        return Ok(false);
    }
    let profile_identity = extract_identity(profile);
    let live_identity = extract_identity(&live);
    if let (Some(profile_email), Some(live_email)) = (
        profile_identity.email.as_deref(),
        live_identity.email.as_deref(),
    ) && profile_email != live_email
    {
        return Ok(false);
    }
    if let (Some(profile_id), Some(live_id)) = (
        profile_identity.account_id.as_deref(),
        live_identity.account_id.as_deref(),
    ) && profile_id != live_id
    {
        return Ok(false);
    }
    match (
        profile_identity.account_id.as_deref(),
        profile_identity.email.as_deref(),
        live_identity.account_id.as_deref(),
        live_identity.email.as_deref(),
    ) {
        (Some(profile_id), Some(profile_email), Some(live_id), Some(live_email)) => {
            Ok(profile_id == live_id && profile_email == live_email)
        }
        _ => anyhow::bail!(
            "cannot safely determine whether live auth belongs to profile '{alias}': account_id and email are required when credential files differ"
        ),
    }
}

fn repair_stale_current_marker_locked(stale_alias: &str) -> Result<()> {
    if read_current_checked()?.as_deref() != Some(stale_alias) {
        return Ok(());
    }
    let live_path = codex_auth_path()?;
    if let Some(actual) = find_matching_profile_checked(&live_path)? {
        write_current(&actual)
    } else {
        clear_current()
    }
}

/// A switch decision prepared against one exact observation of live auth.
/// Preparation alone is not overwrite authorization; only the explicit
/// confirmation transition below can create a committable value.
pub(crate) struct PreparedProfileSwitch {
    alias: String,
    target_snapshot: Vec<u8>,
    live_snapshot: Option<Vec<u8>>,
    requires_confirmation: bool,
}

pub(crate) struct ConfirmedProfileSwitch {
    prepared: PreparedProfileSwitch,
}

#[derive(Debug)]
#[must_use = "selection-history warnings must be surfaced to the user or daemon log"]
pub(crate) struct ProfileSwitchOutcome {
    selection_history_warning: Option<anyhow::Error>,
}

impl ProfileSwitchOutcome {
    fn record(alias: &str) -> Self {
        Self {
            selection_history_warning: crate::cache::try_set_last_used(alias).err(),
        }
    }

    pub(crate) fn selection_history_warning(&self) -> Option<&anyhow::Error> {
        self.selection_history_warning.as_ref()
    }
}

#[derive(Debug)]
#[must_use = "a committed mutation's durability warning must be surfaced to the user"]
pub struct ProfileMutationOutcome {
    durability_warning: Option<anyhow::Error>,
}

impl ProfileMutationOutcome {
    fn committed() -> Self {
        Self {
            durability_warning: None,
        }
    }

    fn committed_with_warnings(warnings: Vec<anyhow::Error>) -> Self {
        let durability_warning = match warnings.len() {
            0 => None,
            1 => warnings.into_iter().next(),
            _ => Some(anyhow::anyhow!(
                "multiple durability confirmations failed: {}",
                warnings
                    .into_iter()
                    .map(|warning| format!("{warning:#}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        };
        Self { durability_warning }
    }

    pub fn durability_warning(&self) -> Option<&anyhow::Error> {
        self.durability_warning.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn test_committed() -> Self {
        Self::committed()
    }

    #[cfg(test)]
    pub(crate) fn test_committed_with_durability_warning(
        warning: impl Into<anyhow::Error>,
    ) -> Self {
        Self::committed_with_warnings(vec![warning.into()])
    }
}

/// User authorization for a switch that must precede an irreversible external
/// action. Unlike `PreparedProfileSwitch`, this permits the selected target and
/// its exactly-bound live copy to rotate together during that action, but never
/// permits the account identity, alias binding, or an unrelated live-auth
/// change. It owns the target lease so the alias cannot be renamed, deleted, or
/// rebound before the action ends.
pub(crate) struct AuthorizedProfileSwitch {
    lease: ProfileLease,
    live_snapshot: Option<Vec<u8>>,
    target_identity: AccountIdentity,
}

impl PreparedProfileSwitch {
    pub(crate) fn requires_confirmation(&self) -> bool {
        self.requires_confirmation
    }

    /// Whether live credentials must first be written back to their saved
    /// owner before this switch can be committed without an overwrite prompt.
    /// A live file that is already semantically identical to a saved profile
    /// needs no synchronization pass.
    pub(crate) fn needs_live_sync(&self) -> bool {
        self.live_snapshot.is_some() && self.requires_confirmation
    }
}

impl ConfirmedProfileSwitch {
    pub(crate) fn alias(&self) -> &str {
        &self.prepared.alias
    }
}

impl AuthorizedProfileSwitch {
    pub(crate) fn alias(&self) -> &str {
        self.lease.alias()
    }

    pub(crate) fn lease(&self) -> &ProfileLease {
        &self.lease
    }
}

pub(crate) fn prepare_profile_switch(alias: &str) -> Result<PreparedProfileSwitch> {
    validate_alias(alias)?;
    let lease = acquire_profile_lease(alias)?;
    prepare_profile_switch_with_lease(&lease)
}

/// Prove the common stable state without scanning every saved profile.
///
/// The target profile and the current marker may name different aliases. When
/// they do, only the profile named by the marker can provide this proof. A
/// marker or profile observation failure is deliberately an optimization miss:
/// the caller still runs the existing authoritative registry scan and retains
/// its established error and overwrite-warning behavior.
fn live_exactly_matches_current_profile(
    registry: &ProfileRegistry,
    target_alias: &str,
    target_snapshot: &[u8],
    live_snapshot: &[u8],
) -> Result<bool> {
    let Some(current_alias) = read_current_checked().ok().flatten() else {
        return Ok(false);
    };
    let matches = if current_alias == target_alias {
        target_snapshot == live_snapshot
    } else {
        let Ok(current_path) = registry.auth_path(&current_alias) else {
            return Ok(false);
        };
        std::fs::read(current_path).is_ok_and(|profile| profile == live_snapshot)
    };
    if matches {
        let _: serde_json::Value = serde_json::from_slice(live_snapshot)
            .context("parsing live authentication before matching saved profiles")?;
    }
    Ok(matches)
}

pub(crate) fn prepare_profile_switch_with_lease(
    lease: &ProfileLease,
) -> Result<PreparedProfileSwitch> {
    let alias = lease.alias();
    let registry = ProfileRegistry::open()?;
    let src = registry.auth_path(alias)?;
    let live_path = codex_auth_path()?;
    // Observe live auth only between complete credential transactions. A
    // refresh intentionally writes its profile before bringing the live copy
    // forward; sampling that intermediate state would misclassify a tracked
    // login as destructive and bypass the normal transaction wait.
    let _transaction = lock_auth_transaction()?;
    let target_snapshot = snapshot_optional_file(&src)?
        .ok_or_else(|| anyhow::Error::from(CsError::NotFound(alias.to_string())))?;
    let target: serde_json::Value = serde_json::from_slice(&target_snapshot)
        .with_context(|| format!("parsing saved profile '{alias}' at {}", src.display()))?;
    crate::auth::validate_managed_auth_value(&target)?;
    let live_snapshot = snapshot_optional_file(&live_path)?;
    let requires_confirmation = match live_snapshot.as_deref() {
        Some(contents)
            if live_exactly_matches_current_profile(
                &registry,
                alias,
                &target_snapshot,
                contents,
            )? =>
        {
            false
        }
        Some(contents) => {
            find_equivalent_profiles_for_bytes_in_registry(&registry, contents)?.is_empty()
        }
        None => false,
    };
    Ok(PreparedProfileSwitch {
        alias: alias.to_string(),
        target_snapshot,
        live_snapshot,
        requires_confirmation,
    })
}

pub(crate) fn commit_confirmed_profile_switch(
    confirmed: ConfirmedProfileSwitch,
) -> Result<ProfileSwitchOutcome> {
    let lease = acquire_profile_lease(confirmed.alias())?;
    commit_confirmed_profile_switch_with_lease(confirmed, &lease)
}

pub(crate) fn commit_confirmed_profile_switch_with_lease(
    confirmed: ConfirmedProfileSwitch,
    lease: &ProfileLease,
) -> Result<ProfileSwitchOutcome> {
    let transaction = lock_auth_transaction()?;
    let revalidated = revalidate_confirmed_profile_switch_locked(confirmed, lease)?;
    commit_activation_if_live_unchanged(
        lease.alias(),
        &revalidated.target,
        revalidated.live_snapshot.as_deref(),
    )?;
    drop(transaction);
    Ok(ProfileSwitchOutcome::record(lease.alias()))
}

struct RevalidatedConfirmedProfileSwitch {
    target: serde_json::Value,
    live_snapshot: Option<Vec<u8>>,
}

/// Revalidate a user's exact confirmation after reacquiring the target lease.
/// The caller must hold the auth transaction for the duration of this check.
fn revalidate_confirmed_profile_switch_locked(
    confirmed: ConfirmedProfileSwitch,
    lease: &ProfileLease,
) -> Result<RevalidatedConfirmedProfileSwitch> {
    let PreparedProfileSwitch {
        alias,
        target_snapshot,
        live_snapshot,
        ..
    } = confirmed.prepared;
    if lease.alias() != alias {
        anyhow::bail!(
            "profile switch confirmation belongs to '{alias}', not the leased profile '{}'",
            lease.alias()
        );
    }
    let src = profile_auth_path(lease.alias())?;
    if snapshot_optional_file(&src)?.as_deref() != Some(target_snapshot.as_slice()) {
        anyhow::bail!(
            "profile '{alias}' changed after the switch was prepared; nothing was overwritten, review it and retry the switch"
        );
    }
    let val: serde_json::Value = serde_json::from_slice(&target_snapshot)
        .with_context(|| format!("parsing saved profile '{alias}' at {}", src.display()))?;
    crate::auth::validate_managed_auth_value(&val)?;
    Ok(RevalidatedConfirmedProfileSwitch {
        target: val,
        live_snapshot,
    })
}

fn confirm_prepared_profile_switch_with<F>(
    prepared: PreparedProfileSwitch,
    allow_prompt: bool,
    confirm_overwrite: F,
) -> Result<ConfirmedProfileSwitch>
where
    F: FnOnce() -> Result<bool>,
{
    if prepared.requires_confirmation() {
        if !allow_prompt {
            anyhow::bail!(
                "current auth.json is not tracked; interactive confirmation is required before overwriting it"
            );
        }
        if !confirm_overwrite()? {
            return Err(CsError::Aborted.into());
        }
    }
    Ok(ConfirmedProfileSwitch { prepared })
}

fn prompt_untracked_live_overwrite() -> Result<bool> {
    user_print(
        "Current auth.json does not belong to any saved profile -- switching will overwrite it. Continue? [y/N] ",
    );
    io::stdout().flush()?;
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn prepare_and_confirm_profile_switch_with<F>(
    alias: &str,
    allow_prompt: bool,
    confirm_overwrite: F,
) -> Result<ConfirmedProfileSwitch>
where
    F: FnOnce() -> Result<bool>,
{
    let prepared = prepare_profile_switch(alias)?;
    confirm_prepared_profile_switch_with(prepared, allow_prompt, confirm_overwrite)
}

pub(crate) fn prepare_and_confirm_profile_switch(
    alias: &str,
    allow_prompt: bool,
) -> Result<ConfirmedProfileSwitch> {
    prepare_and_confirm_profile_switch_with(alias, allow_prompt, prompt_untracked_live_overwrite)
}

pub(crate) fn confirm_prepared_profile_switch_without_overwrite(
    prepared: PreparedProfileSwitch,
) -> Result<ConfirmedProfileSwitch> {
    if prepared.requires_confirmation() {
        anyhow::bail!(
            "current auth.json is not tracked; explicit confirmation is required before overwriting it"
        );
    }
    Ok(ConfirmedProfileSwitch { prepared })
}

/// Revalidate an already-confirmed exact live-auth overwrite before an
/// irreversible network action (currently reset-card redemption). Confirmation
/// never owns the target lease while waiting for input; authorization begins
/// only after the caller reacquires it and rejects any intervening target or
/// live-auth change. The final commit accepts token rotation only for the same
/// complete account identity.
pub(crate) fn authorize_confirmed_profile_switch_before_side_effect(
    confirmed: ConfirmedProfileSwitch,
    lease: ProfileLease,
) -> Result<AuthorizedProfileSwitch> {
    let transaction = lock_auth_transaction()?;
    let revalidated = revalidate_confirmed_profile_switch_locked(confirmed, &lease)?;
    let current_live = snapshot_optional_file(&codex_auth_path()?)?;
    if current_live != revalidated.live_snapshot {
        anyhow::bail!(
            "live auth changed after the switch was confirmed for profile '{}'; no irreversible account action was authorized and nothing was overwritten",
            lease.alias()
        );
    }
    let target_identity = extract_identity(&revalidated.target);
    require_complete_account_identity(lease.alias(), &target_identity)?;
    drop(transaction);
    Ok(AuthorizedProfileSwitch {
        lease,
        live_snapshot: revalidated.live_snapshot,
        target_identity,
    })
}

struct RevalidatedAuthorizedProfileSwitch {
    target: serde_json::Value,
    live_snapshot: Option<Vec<u8>>,
}

fn validate_authorized_profile_switch_locked(
    authorized: &AuthorizedProfileSwitch,
) -> Result<RevalidatedAuthorizedProfileSwitch> {
    let alias = authorized.alias();
    let target_path = profile_auth_path(alias)?;
    let target = read_existing_auth(&target_path)?
        .ok_or_else(|| anyhow::Error::from(CsError::NotFound(alias.to_string())))?;
    crate::auth::validate_managed_auth_value(&target)?;
    if !exact_identity_matches(&authorized.target_identity, &extract_identity(&target)) {
        anyhow::bail!(
            "profile '{alias}' changed account identity after the switch was authorized; live credentials and the activation marker were not overwritten"
        );
    }

    let live_path = codex_auth_path()?;
    let current_live = snapshot_optional_file(&live_path)?;
    if current_live != authorized.live_snapshot {
        let Some(current_live_snapshot) = current_live.as_deref() else {
            anyhow::bail!(
                "live auth changed after the switch was authorized; live credentials and the activation marker were not overwritten"
            );
        };
        let current_live_value: serde_json::Value =
            serde_json::from_slice(current_live_snapshot)
                .context("parsing live auth changed after the profile switch was authorized")?;
        crate::auth::validate_managed_auth_value(&current_live_value)?;
        let still_selected = read_current_checked()?.as_deref() == Some(alias);
        let same_generation = current_live_value == target;
        let same_identity = exact_identity_matches(
            &authorized.target_identity,
            &extract_identity(&current_live_value),
        );
        if !still_selected || !same_generation || !same_identity {
            anyhow::bail!(
                "live auth changed after the switch was authorized and no longer exactly matches profile '{alias}'; live credentials and the activation marker were not overwritten"
            );
        }

        // A usage/reset-card preflight can rotate the selected account's
        // credentials while this authorization owns its target lease. Once
        // the locked recheck proves that both durable copies and the marker
        // still name that same strict identity and credential generation, the
        // new live bytes become the only safe CAS basis for this revalidated
        // operation.
    }
    Ok(RevalidatedAuthorizedProfileSwitch {
        target,
        live_snapshot: current_live,
    })
}

pub(crate) fn revalidate_authorized_profile_switch(
    authorized: &AuthorizedProfileSwitch,
) -> Result<()> {
    let _transaction = lock_auth_transaction()?;
    validate_authorized_profile_switch_locked(authorized).map(|_| ())
}

pub(crate) fn commit_authorized_profile_switch(
    authorized: AuthorizedProfileSwitch,
) -> Result<ProfileSwitchOutcome> {
    let transaction = lock_auth_transaction()?;
    let revalidated = validate_authorized_profile_switch_locked(&authorized)?;
    commit_activation_if_live_unchanged(
        authorized.alias(),
        &revalidated.target,
        revalidated.live_snapshot.as_deref(),
    )?;
    drop(transaction);
    Ok(ProfileSwitchOutcome::record(authorized.alias()))
}

/// Activate `target_alias` only while the caller's view of the active profile
/// still matches both the marker and live credentials.
pub(crate) fn switch_profile_if_current(
    expected_alias: &str,
    target_alias: &str,
) -> Result<Option<ProfileSwitchOutcome>> {
    validate_alias(expected_alias)?;
    validate_alias(target_alias)?;
    // Ownership validation reads the expected profile while publication reads
    // the target. Keep both aliases stable across that entire decision so a
    // concurrent rename/delete cannot invalidate either side of the proof.
    let _leases = acquire_profile_leases(&[expected_alias, target_alias])?;
    let transaction = lock_auth_transaction()?;
    if read_current_checked()?.as_deref() != Some(expected_alias) {
        return Ok(None);
    }
    let live_path = codex_auth_path()?;
    let Some(live_snapshot) = snapshot_optional_file(&live_path)? else {
        return Ok(None);
    };
    if !live_snapshot_belongs_to_profile_locked(expected_alias, &live_snapshot)? {
        return Ok(None);
    }
    if expected_alias == target_alias {
        drop(transaction);
        return Ok(Some(ProfileSwitchOutcome::record(target_alias)));
    }
    let target_path = profile_auth_path(target_alias)?;
    let target = match read_existing_auth(&target_path)? {
        Some(value) => value,
        None => return Err(CsError::NotFound(target_alias.to_string()).into()),
    };
    crate::auth::validate_managed_auth_value(&target)?;
    commit_activation_if_live_unchanged(target_alias, &target, Some(&live_snapshot))?;
    drop(transaction);
    Ok(Some(ProfileSwitchOutcome::record(target_alias)))
}

/// Compare-and-swap a refresh rotation while holding the caller's profile
/// lease and the auth transaction. A concurrent re-login supersedes the
/// presented token and must win.
#[derive(Debug)]
pub(crate) enum RefreshTokenUpdate {
    /// The refreshed credential is durable in the profile and, when that
    /// profile was active, in live Codex auth as well.
    Saved,
    /// Another credential replaced the token presented to the auth server.
    /// Refusing to overwrite it preserves the newer writer.
    Superseded { recovery_path: PathBuf },
    /// The refreshed credential is already visible in the profile, but either
    /// its directory durability or the subsequent live-auth activation could
    /// not be confirmed. The caller must stop without spending another
    /// single-use refresh token and surface `cause` as a partial commit.
    SavedWithCommitIncomplete {
        recovery_path: Option<PathBuf>,
        cause: anyhow::Error,
    },
    /// The refreshed credential is durably committed to the profile and, when
    /// applicable, live Codex auth. Only exact cleanup of its now-redundant
    /// recovery stage is incomplete.
    SavedWithCleanupIncomplete {
        recovery_path: Option<PathBuf>,
        cause: anyhow::Error,
    },
    /// The rotated credential passed response validation and remains durably
    /// staged, but a local operational failure stopped profile commit before
    /// the saved credential was replaced.
    RecoveryPreserved { path: PathBuf, cause: anyhow::Error },
    /// The auth server rotated the single-use token, but the returned
    /// credentials failed a local account/policy invariant. They were not
    /// installed into the profile or live auth; the exact response was instead
    /// durably preserved for explicit recovery.
    Quarantined { path: PathBuf, cause: anyhow::Error },
}

/// Exact pre-network authorization for deciding whether a newly-rotated
/// profile credential may also replace live Codex auth. The snapshot is carried
/// across the HTTP request and is the only value accepted by the final CAS.
pub(crate) struct FreshCredentialsActivationAuthorization {
    alias: String,
    expected_live: Option<Vec<u8>>,
    expected_current_marker: Option<String>,
    activate_live: bool,
    expected_identity: AccountIdentity,
    expected_profile: serde_json::Value,
}

enum FreshLiveActivationPlan {
    Skip,
    Activate { expected_live: Option<Vec<u8>> },
    Incomplete(anyhow::Error),
}

fn plan_fresh_live_activation(
    alias: &str,
    authorization: &FreshCredentialsActivationAuthorization,
    pre_refresh_profile: &serde_json::Value,
) -> FreshLiveActivationPlan {
    let current = match read_current_checked() {
        Ok(current) => current,
        Err(cause) => {
            return FreshLiveActivationPlan::Incomplete(cause.context(format!(
                "refreshed profile '{alias}' can be made durable, but its active marker could not be revalidated before live-auth activation"
            )));
        }
    };

    if authorization.activate_live && current == authorization.expected_current_marker {
        return FreshLiveActivationPlan::Activate {
            expected_live: authorization.expected_live.clone(),
        };
    }

    if current.as_deref() != Some(alias) {
        return FreshLiveActivationPlan::Skip;
    }

    let live_path = match codex_auth_path() {
        Ok(path) => path,
        Err(cause) => return FreshLiveActivationPlan::Incomplete(cause),
    };
    let current_live = match snapshot_optional_file(&live_path) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return FreshLiveActivationPlan::Incomplete(anyhow::anyhow!(
                "profile '{alias}' became active while its refresh was in flight, but live auth is absent; the refreshed profile will be saved without recreating live auth"
            ));
        }
        Err(cause) => {
            return FreshLiveActivationPlan::Incomplete(cause.context(format!(
                "profile '{alias}' became active while its refresh was in flight, but its exact live-auth snapshot could not be read"
            )));
        }
    };
    let live: serde_json::Value = match serde_json::from_slice(&current_live) {
        Ok(live) => live,
        Err(cause) => {
            return FreshLiveActivationPlan::Incomplete(anyhow::Error::new(cause).context(
                format!(
                    "profile '{alias}' became active while its refresh was in flight, but live auth is not valid JSON"
                ),
            ));
        }
    };
    if &live != pre_refresh_profile {
        return FreshLiveActivationPlan::Incomplete(anyhow::anyhow!(
            "profile '{alias}' became active while its refresh was in flight, but live auth is not the exact pre-refresh credential; the foreign or newer live credential was not overwritten"
        ));
    }

    FreshLiveActivationPlan::Activate {
        expected_live: Some(current_live),
    }
}

pub(crate) fn authorize_fresh_credentials_activation(
    lease: &ProfileLease,
) -> Result<FreshCredentialsActivationAuthorization> {
    let alias = lease.alias();
    validate_alias(alias)?;
    let _transaction = lock_auth_transaction()?;
    let profile = read_auth(&profile_auth_path(alias)?)?;
    let expected_identity = extract_identity(&profile);
    require_complete_account_identity(alias, &expected_identity).with_context(|| {
        format!("profile '{alias}' cannot safely authorize a single-use token refresh")
    })?;
    let expected_live = snapshot_optional_file(&codex_auth_path()?)?;
    let expected_current_marker = read_current_checked()?;
    let activate_live = match expected_live.as_deref() {
        Some(snapshot) => live_snapshot_belongs_to_auth_value_locked(alias, &profile, snapshot)?,
        None => false,
    };
    Ok(FreshCredentialsActivationAuthorization {
        alias: alias.to_string(),
        expected_live,
        expected_current_marker,
        activate_live,
        expected_identity,
        expected_profile: profile,
    })
}

fn quarantine_refreshed_credentials(
    alias: &str,
    value: &serde_json::Value,
    cause: anyhow::Error,
) -> Result<RefreshTokenUpdate> {
    let stage = stage_refresh_rotation(alias, value).with_context(|| {
        format!(
            "refreshed credentials for profile '{alias}' were rejected, but their recovery copy could not be preserved"
        )
    })?;
    Ok(quarantine_staged_refresh(stage, cause))
}

fn quarantine_staged_refresh(
    stage: RotationRecoveryStage,
    cause: anyhow::Error,
) -> RefreshTokenUpdate {
    RefreshTokenUpdate::Quarantined {
        path: stage.path,
        cause,
    }
}

fn preserve_staged_refresh(
    stage: RotationRecoveryStage,
    cause: anyhow::Error,
) -> RefreshTokenUpdate {
    RefreshTokenUpdate::RecoveryPreserved {
        path: stage.path,
        cause,
    }
}

/// Infallible, timestamp-free copy of the successor material returned by a
/// successful refresh response. This is the first write-ahead value: it must be
/// durable before `apply_tokens` performs clock-dependent local transformation.
fn raw_refresh_rotation_material(
    alias: &str,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> serde_json::Value {
    serde_json::json!({
        "recovery_kind": "validated_token_refresh_response",
        "profile_alias": alias,
        "id_token": id_token,
        "access_token": access_token,
        "refresh_token": refresh_token,
    })
}

fn exact_recovery_path_after_cleanup_failure(
    path: &Path,
    expected: &crate::fs_ops::FileToken,
    cleanup: anyhow::Error,
) -> (Option<PathBuf>, anyhow::Error) {
    match crate::fs_ops::token_if_present(path) {
        Ok(Some(observed)) if &observed == expected => (Some(path.to_path_buf()), cleanup),
        Ok(_) => (None, cleanup),
        Err(observation) => (
            None,
            anyhow::anyhow!(
                "exact recovery-stage cleanup failed ({cleanup:#}) and the path could not be rebound to its original file identity ({observation:#})"
            ),
        ),
    }
}

fn displaced_stage_cleanup_incomplete(
    path: &Path,
    expected: &crate::fs_ops::FileToken,
    cleanup: anyhow::Error,
) -> anyhow::Error {
    match crate::fs_ops::token_if_present(path) {
        Ok(Some(observed)) if &observed == expected => anyhow::anyhow!(
            "cleanup of the exact previous recovery stage is incomplete ({cleanup:#}); that exact stage remains at {}",
            path.display()
        ),
        Ok(Some(_)) => anyhow::anyhow!(
            "cleanup of the exact previous recovery stage is incomplete ({cleanup:#}); {} now belongs to a different file, so no previous-stage ownership is claimed there",
            path.display()
        ),
        Ok(None) => anyhow::anyhow!(
            "cleanup of the exact previous recovery stage is incomplete ({cleanup:#}); {} is no longer present, but removal durability was not confirmed",
            path.display()
        ),
        Err(observation) => anyhow::anyhow!(
            "cleanup of the exact previous recovery stage is incomplete ({cleanup:#}); {} could not be revalidated, so no previous-stage ownership is claimed there ({observation:#})",
            path.display()
        ),
    }
}

/// Preserve an invalid refresh response after the server returned a non-empty
/// successor refresh token. The caller must hold the same profile lease and
/// pre-request activation authorization that guarded the irreversible request.
/// Neither the saved profile nor live auth is modified.
pub(crate) fn quarantine_invalid_refresh_response_leased(
    lease: &ProfileLease,
    authorization: FreshCredentialsActivationAuthorization,
    presented_refresh_token: &str,
    recovery: &serde_json::Value,
    cause: anyhow::Error,
) -> Result<RefreshTokenUpdate> {
    let alias = lease.alias();
    validate_alias(alias)?;
    let cause = if authorization.alias != alias {
        cause.context(format!(
            "refresh-response authorization belongs to '{}', not leased profile '{alias}'",
            authorization.alias
        ))
    } else if refresh_token(&authorization.expected_profile) != Some(presented_refresh_token) {
        cause.context(
            "the pre-request authorization did not contain the refresh token presented to the server",
        )
    } else {
        cause
    };
    quarantine_refreshed_credentials(alias, recovery, cause)
}

struct RefreshCommitHooks<AfterLock, BeforeCleanup> {
    after_lock: AfterLock,
    before_cleanup: BeforeCleanup,
}

fn update_profile_tokens_if_refresh_matches_after_lock<AfterLock, BeforeCleanup>(
    lease: &ProfileLease,
    authorization: FreshCredentialsActivationAuthorization,
    presented_refresh_token: &str,
    id_token: &str,
    access_token: &str,
    new_refresh_token: &str,
    hooks: RefreshCommitHooks<AfterLock, BeforeCleanup>,
) -> Result<RefreshTokenUpdate>
where
    AfterLock: FnOnce(),
    BeforeCleanup: FnOnce(),
{
    let alias = lease.alias();
    validate_alias(alias)?;
    if authorization.alias != alias {
        anyhow::bail!(
            "fresh-credential activation authorization belongs to '{}', not '{alias}'",
            authorization.alias
        );
    }
    crate::auth::validate_complete_oauth_tokens(id_token, access_token, new_refresh_token)?;

    let raw_rotation =
        raw_refresh_rotation_material(alias, id_token, access_token, new_refresh_token);
    let mut stage = stage_refresh_rotation(alias, &raw_rotation).with_context(|| {
        format!(
            "refreshed credentials for profile '{alias}' could not be preserved before local credential transformation"
        )
    })?;
    let mut staged_value = authorization.expected_profile.clone();
    if let Err(cause) =
        crate::auth::apply_tokens(&mut staged_value, id_token, access_token, new_refresh_token)
    {
        return Ok(preserve_staged_refresh(
            stage,
            cause.context(format!(
                "refreshed credentials for profile '{alias}' were preserved, but local credential transformation failed"
            )),
        ));
    }
    if let Err(cause) = stage.persist(&staged_value) {
        return Ok(preserve_staged_refresh(
            stage,
            cause.context(format!(
                "refreshed credentials for profile '{alias}' were preserved, but the complete local credential could not replace their raw recovery material"
            )),
        ));
    }

    let _transaction = match lock_auth_transaction() {
        Ok(transaction) => transaction,
        Err(cause) => {
            return Ok(preserve_staged_refresh(
                stage,
                cause.context(format!(
                    "refreshed credentials for profile '{alias}' were preserved, but the legacy-compatible auth transaction could not be acquired before profile commit"
                )),
            ));
        }
    };
    (hooks.after_lock)();

    if refresh_token(&authorization.expected_profile) != Some(presented_refresh_token) {
        return Ok(quarantine_staged_refresh(
            stage,
            anyhow::anyhow!(
                "the pre-request authorization did not contain the refresh token presented to the server"
            ),
        ));
    }
    if let Err(cause) = ensure_account_identity_matches(
        alias,
        &authorization.expected_identity,
        &extract_identity(&staged_value),
    ) {
        return Ok(quarantine_staged_refresh(
            stage,
            cause.context("the refresh endpoint returned credentials for another account"),
        ));
    }
    if let Err(cause) = crate::auth::validate_managed_auth_value(&staged_value) {
        return Ok(quarantine_staged_refresh(
            stage,
            cause.context("the refreshed credentials violate the managed account policy"),
        ));
    }

    let profile_path = match profile_auth_path(alias) {
        Ok(path) => path,
        Err(cause) => return Ok(preserve_staged_refresh(stage, cause)),
    };
    let profile = match read_auth(&profile_path) {
        Ok(profile) => profile,
        Err(cause) => return Ok(preserve_staged_refresh(stage, cause)),
    };
    if refresh_token(&profile) != Some(presented_refresh_token) {
        return Ok(RefreshTokenUpdate::Superseded {
            recovery_path: stage.path,
        });
    }

    if let Err(cause) = ensure_account_identity_matches(
        alias,
        &authorization.expected_identity,
        &extract_identity(&profile),
    ) {
        return Ok(quarantine_staged_refresh(
            stage,
            cause.context(
                "the profile changed account identity while its token refresh was in flight",
            ),
        ));
    }

    let mut updated = profile.clone();
    if let Err(cause) =
        crate::auth::apply_tokens(&mut updated, id_token, access_token, new_refresh_token)
    {
        return Ok(preserve_staged_refresh(stage, cause));
    }
    if let Err(cause) = ensure_account_identity_matches(
        alias,
        &authorization.expected_identity,
        &extract_identity(&updated),
    ) {
        return Ok(quarantine_staged_refresh(
            stage,
            cause.context("the refresh endpoint returned credentials for another account"),
        ));
    }
    if let Err(cause) = crate::auth::validate_managed_auth_value(&updated) {
        return Ok(quarantine_staged_refresh(
            stage,
            cause.context("the refreshed credentials violate the managed account policy"),
        ));
    }
    let live_activation = plan_fresh_live_activation(alias, &authorization, &profile);
    if updated != staged_value
        && let Err(cause) = stage.persist(&updated)
    {
        return Ok(preserve_staged_refresh(
            stage,
            cause.context("updating the recovery stage with the final profile credential"),
        ));
    }
    let profile_write = match write_auth(&profile_path, &updated) {
        Ok(outcome) => outcome,
        Err(cause) => return Ok(preserve_staged_refresh(stage, cause)),
    };
    if let Err(cause) = require_durable_private_write(
        &profile_path,
        "refreshed profile credentials",
        profile_write,
    ) {
        return Ok(RefreshTokenUpdate::SavedWithCommitIncomplete {
            recovery_path: Some(stage.path),
            cause,
        });
    }

    let activation_error = match live_activation {
        FreshLiveActivationPlan::Skip => None,
        FreshLiveActivationPlan::Activate { expected_live } => {
            commit_fresh_credentials_activation(alias, &updated, expected_live.as_deref()).err()
        }
        FreshLiveActivationPlan::Incomplete(cause) => Some(cause),
    };

    (hooks.before_cleanup)();
    let cleanup_error = crate::auth::remove_bound_path(&stage.path, &stage.token)
        .err()
        .map(|cause| exact_recovery_path_after_cleanup_failure(&stage.path, &stage.token, cause));
    match (activation_error, cleanup_error) {
        (None, None) => Ok(RefreshTokenUpdate::Saved),
        (Some(cause), None) => Ok(RefreshTokenUpdate::SavedWithCommitIncomplete {
            recovery_path: None,
            cause,
        }),
        (None, Some((recovery_path, cause))) => {
            Ok(RefreshTokenUpdate::SavedWithCleanupIncomplete {
                recovery_path,
                cause: cause.context(
                    "the refreshed credential commit completed, but its exact recovery stage could not be removed",
                ),
            })
        }
        (Some(activation), Some((recovery_path, cleanup))) => {
            Ok(RefreshTokenUpdate::SavedWithCommitIncomplete {
                recovery_path,
                cause: anyhow::anyhow!(
                    "live-auth activation failed ({activation:#}) and exact recovery-stage cleanup also failed ({cleanup:#})"
                ),
            })
        }
    }
}

pub(crate) fn update_profile_tokens_if_refresh_matches_leased(
    lease: &ProfileLease,
    authorization: FreshCredentialsActivationAuthorization,
    presented_refresh_token: &str,
    id_token: &str,
    access_token: &str,
    new_refresh_token: &str,
) -> Result<RefreshTokenUpdate> {
    update_profile_tokens_if_refresh_matches_after_lock(
        lease,
        authorization,
        presented_refresh_token,
        id_token,
        access_token,
        new_refresh_token,
        RefreshCommitHooks {
            after_lock: || {},
            before_cleanup: || {},
        },
    )
}

/// Stable ownership proof captured before an interactive re-login begins.
/// Complete identities keep the existing strict binding behavior. A legacy
/// incomplete profile instead retains its exact process-local file revision and
/// known identity components; replacing it requires explicit caller
/// confirmation and a durable archive of the old credential.
#[derive(Debug)]
enum ProfileReauthProof {
    Strict(StrictAccountBinding),
    Incomplete {
        identity: AccountIdentity,
        file_revision: crate::fs_ops::FileRevisionToken,
    },
}

#[derive(Debug)]
pub(crate) struct PreparedProfileReauth {
    alias: String,
    proof: ProfileReauthProof,
}

impl PreparedProfileReauth {
    pub(crate) fn email(&self) -> Option<&str> {
        match &self.proof {
            ProfileReauthProof::Strict(binding) => Some(&binding.email),
            ProfileReauthProof::Incomplete { identity, .. } => identity.email.as_deref(),
        }
    }

    pub(crate) fn requires_recoverable_replacement(&self) -> bool {
        matches!(self.proof, ProfileReauthProof::Incomplete { .. })
    }
}

#[derive(Debug)]
pub(crate) enum ProfileReauthOutcome {
    Reauthorized,
    RecoveredIncomplete { archive_path: PathBuf },
}

impl ProfileReauthOutcome {
    pub(crate) fn archive_path(&self) -> Option<&Path> {
        match self {
            Self::Reauthorized => None,
            Self::RecoveredIncomplete { archive_path } => Some(archive_path),
        }
    }
}

pub(crate) fn prepare_profile_reauth_with_lease(
    lease: &ProfileLease,
) -> Result<PreparedProfileReauth> {
    let alias = lease.alias();
    let profile_path = profile_auth_path(alias)?;
    let existing = read_existing_auth(&profile_path)?
        .ok_or_else(|| anyhow::Error::from(CsError::NotFound(alias.to_string())))?;
    let identity = extract_identity(&existing);
    let proof = match crate::auth::account_info_from_auth_value(&existing).strict_binding() {
        Some(binding) => ProfileReauthProof::Strict(binding),
        None => ProfileReauthProof::Incomplete {
            identity,
            file_revision: crate::fs_ops::revision_token_for_path(&profile_path)
                .with_context(|| format!("binding incomplete profile '{alias}' before re-login"))?,
        },
    };
    Ok(PreparedProfileReauth {
        alias: alias.to_string(),
        proof,
    })
}

fn ensure_known_identity_components_match(
    alias: &str,
    known: &AccountIdentity,
    incoming: &AccountIdentity,
) -> Result<()> {
    require_complete_account_identity(alias, incoming)?;
    if known
        .account_id
        .as_deref()
        .is_some_and(|account_id| incoming.account_id.as_deref() != Some(account_id))
        || known
            .email
            .as_deref()
            .is_some_and(|email| incoming.email.as_deref() != Some(email))
    {
        anyhow::bail!(
            "authenticated account does not match the known identity fields of legacy profile '{alias}'"
        );
    }
    Ok(())
}

fn deleted_profile_archive_path(alias: &str) -> Result<PathBuf> {
    validate_alias(alias)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(deleted_profiles_dir()?.join(format!("{alias}.backup-{timestamp}")))
}

fn archive_profile_credentials_exact(
    alias: &str,
    profile_path: &Path,
    expected: &crate::fs_ops::FileToken,
) -> Result<PathBuf> {
    let deleted_dir = deleted_profiles_dir()?;
    ensure_private_dir(&deleted_dir)?;
    let archive_dir = deleted_profile_archive_path(alias)?;
    std::fs::create_dir(&archive_dir).with_context(|| {
        format!(
            "reserving a recoverable archive for profile '{alias}' at {}",
            archive_dir.display()
        )
    })?;
    ensure_private_dir(&archive_dir)?;
    #[cfg(unix)]
    crate::auth::confirm_namespace_durability(&archive_dir)?;

    let archive_auth = archive_dir.join("auth.json");
    let creation = crate::fs_ops::create_exclusive_copy(profile_path, &archive_auth, expected)
        .with_context(|| {
            format!(
                "archiving the exact previous credentials for profile '{alias}' at {}",
                archive_auth.display()
            )
        })?;
    if matches!(
        creation,
        crate::fs_ops::CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(_)
    ) {
        #[cfg(unix)]
        crate::auth::confirm_namespace_durability(&archive_auth).with_context(|| {
            format!(
                "confirming the previous credential archive for profile '{alias}' at {}",
                archive_auth.display()
            )
        })?;
        #[cfg(windows)]
        anyhow::bail!(
            "Windows exact archive creation at {} returned an unsupported Unix-only durability outcome",
            archive_auth.display()
        );
    }
    #[cfg(windows)]
    crate::auth::harden_windows_private_file(&archive_auth)?;
    Ok(archive_dir)
}

fn replace_strict_profile_auth_and_live(
    lease: &ProfileLease,
    expected_binding: &StrictAccountBinding,
    val: &serde_json::Value,
) -> Result<()> {
    let alias = lease.alias();
    crate::auth::validate_managed_auth_value(val)?;
    let _transaction = lock_auth_transaction().with_context(|| {
        format!(
            "the live-auth transaction could not be acquired before replacing profile '{alias}'"
        )
    })?;
    let profile_path = profile_auth_path(alias)?;
    let existing = read_existing_auth(&profile_path)?
        .ok_or_else(|| anyhow::Error::from(CsError::NotFound(alias.to_string())))?;
    let current_binding = crate::auth::account_info_from_auth_value(&existing)
        .strict_binding()
        .with_context(|| {
            format!(
                "profile '{alias}' no longer contains the complete account identity authorized before re-login"
            )
        })?;
    anyhow::ensure!(
        &current_binding == expected_binding,
        "profile '{alias}' changed account identity while re-login was in progress"
    );
    ensure_same_account_identity(alias, &existing, val)?;
    let live_snapshot = snapshot_optional_file(&codex_auth_path()?)?;
    let was_active = match live_snapshot.as_deref() {
        Some(snapshot) => live_snapshot_belongs_to_auth_value_locked(alias, &existing, snapshot),
        None => Ok(false),
    }
    .with_context(|| {
        format!(
            "determining whether live auth also needs replacement failed before profile '{alias}' was changed"
        )
    })?;
    write_profile_auth_durably(alias, &profile_path, val)?;
    if was_active {
        commit_fresh_credentials_activation(alias, val, live_snapshot.as_deref())?;
    } else if read_current_checked()
        .with_context(|| {
            format!(
                "new credentials are preserved in profile '{alias}', but reading the current marker failed"
            )
        })?
        .as_deref()
        == Some(alias)
    {
        repair_stale_current_marker_locked(alias).with_context(|| {
            format!(
                "new credentials are preserved in profile '{alias}', but repairing its stale current marker failed"
            )
        })?;
    }
    Ok(())
}

/// Commit a re-login after the interactive OAuth wait no longer owns the
/// profile lease. A complete profile must retain its strict identity. An
/// incomplete legacy profile must retain its exact captured file revision,
/// match every known identity component, and be archived before explicit
/// replacement.
pub(crate) fn commit_prepared_profile_reauth_with_lease(
    prepared: PreparedProfileReauth,
    lease: &ProfileLease,
    val: &serde_json::Value,
    allow_recoverable_replacement: bool,
) -> Result<ProfileReauthOutcome> {
    anyhow::ensure!(
        lease.alias() == prepared.alias,
        "re-login for '{}' received profile lease for '{}'",
        prepared.alias,
        lease.alias()
    );
    let alias = lease.alias();
    match prepared.proof {
        ProfileReauthProof::Strict(binding) => {
            replace_strict_profile_auth_and_live(lease, &binding, val)?;
            Ok(ProfileReauthOutcome::Reauthorized)
        }
        ProfileReauthProof::Incomplete {
            identity,
            file_revision,
        } => {
            anyhow::ensure!(
                allow_recoverable_replacement,
                "profile '{alias}' has incomplete account identity; explicit confirmation is required before its previous credentials can be archived and replaced"
            );
            crate::auth::validate_managed_auth_value(val)?;
            let incoming_identity = extract_identity(val);
            ensure_known_identity_components_match(alias, &identity, &incoming_identity)?;

            let _transaction = lock_auth_transaction()?;
            let profile_path = profile_auth_path(alias)?;
            let current_token = file_revision
                .revalidate_path(&profile_path)
                .with_context(|| {
                    format!("revalidating incomplete profile '{alias}' after OAuth")
                })?
                .with_context(|| {
                    format!(
                        "profile '{alias}' changed while re-login was in progress; its previous credentials were not archived or replaced"
                    )
                })?;
            let existing = read_auth(&profile_path)?;
            anyhow::ensure!(
                extract_identity(&existing) == identity,
                "profile '{alias}' changed identity while re-login was in progress; its previous credentials were not archived or replaced"
            );

            let duplicates = scan_profiles_by_identity(&incoming_identity)?
                .exact
                .into_iter()
                .filter(|existing_alias| existing_alias != alias)
                .collect::<Vec<_>>();
            anyhow::ensure!(
                duplicates.is_empty(),
                "authenticated account already belongs to profile(s) {}; legacy profile '{alias}' was not replaced",
                duplicates.join(", ")
            );

            let live_snapshot = snapshot_optional_file(&codex_auth_path()?)?;
            let was_active = match live_snapshot.as_deref() {
                Some(snapshot) => {
                    live_snapshot_belongs_to_auth_value_locked(alias, &existing, snapshot)?
                }
                None => false,
            };
            let archive_path =
                archive_profile_credentials_exact(alias, &profile_path, &current_token)?;
            write_profile_auth_durably(alias, &profile_path, val).with_context(|| {
                format!(
                    "the previous credentials remain recoverable at {}; profile '{alias}' replacement did not complete",
                    archive_path.display()
                )
            })?;
            if was_active {
                commit_fresh_credentials_activation(alias, val, live_snapshot.as_deref())
                    .with_context(|| {
                        format!(
                            "profile '{alias}' was replaced and its previous credentials remain at {}, but live activation did not fully complete",
                            archive_path.display()
                        )
                    })?;
            } else if read_current_checked()
                .with_context(|| {
                    format!(
                        "profile '{alias}' was replaced and its previous credentials remain at {}, but reading the current marker failed",
                        archive_path.display()
                    )
                })?
                .as_deref()
                == Some(alias)
            {
                repair_stale_current_marker_locked(alias).with_context(|| {
                    format!(
                        "profile '{alias}' was replaced and its previous credentials remain at {}, but repairing its stale current marker failed",
                        archive_path.display()
                    )
                })?;
            }
            Ok(ProfileReauthOutcome::RecoveredIncomplete { archive_path })
        }
    }
}

fn find_matching_profiles_for_bytes_checked(target: &[u8]) -> Result<Vec<String>> {
    let registry = ProfileRegistry::open()?;
    find_matching_profiles_for_bytes_in_registry(&registry, target)
}

fn find_matching_profiles_for_bytes_in_registry(
    registry: &ProfileRegistry,
    target: &[u8],
) -> Result<Vec<String>> {
    Ok(registry.snapshot()?.exact_matches(target))
}

fn auth_values_semantically_equal(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    left == right
}

fn find_equivalent_profiles_for_bytes_in_registry(
    registry: &ProfileRegistry,
    target: &[u8],
) -> Result<Vec<String>> {
    let target: serde_json::Value = serde_json::from_slice(target)
        .context("parsing live authentication before matching saved profiles")?;
    Ok(registry.snapshot()?.parse()?.equivalent_matches(&target))
}

fn find_matching_profiles_checked(auth_path: &Path) -> Result<Vec<String>> {
    let target = match std::fs::read(auth_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", auth_path.display()));
        }
    };
    find_matching_profiles_for_bytes_checked(&target)
}

fn select_exact_profile_binding(
    matches: &[String],
    current: Option<&str>,
) -> Result<Option<String>> {
    match matches {
        [] => Ok(None),
        [alias] => Ok(Some(alias.clone())),
        aliases => {
            if let Some(current) = current
                && aliases.iter().any(|alias| alias == current)
            {
                return Ok(Some(current.to_string()));
            }
            anyhow::bail!(
                "live auth exactly matches multiple legacy profiles ({}), and the current marker does not disambiguate them",
                aliases.join(", ")
            )
        }
    }
}

pub(crate) fn find_matching_profile_checked(auth_path: &Path) -> Result<Option<String>> {
    let matches = find_matching_profiles_checked(auth_path)?;
    let current = if matches.len() > 1 {
        read_current_checked()?
    } else {
        None
    };
    select_exact_profile_binding(&matches, current.as_deref())
}

/// Revalidate a partial activation against the current live bytes while the
/// app-owned auth transaction and target profile lease are held. This proves
/// only the snapshot observed by this call: Codex can write live auth again as
/// soon as it returns, so UI callers must not treat the typed error's alias as
/// permanently authoritative.
pub(crate) fn partial_activation_is_currently_bound_checked(
    partial: &PartialProfileActivation,
) -> Result<bool> {
    let lease = acquire_profile_lease(partial.alias())?;
    let _transaction = lock_auth_transaction()?;
    let Some(profile) = read_existing_auth(&profile_auth_path(lease.alias())?)? else {
        return Ok(false);
    };
    let Some(live) = snapshot_optional_file(&codex_auth_path()?)? else {
        return Ok(false);
    };
    let live_digest: [u8; 32] = Sha256::digest(&live).into();
    if live_digest != partial.published_digest {
        return Ok(false);
    }
    let current_profile_publication = serde_json::to_string_pretty(&profile)?.into_bytes();
    let profile_digest: [u8; 32] = Sha256::digest(current_profile_publication).into();
    Ok(profile_digest == partial.published_digest)
}

pub fn find_matching_profile(auth_path: &Path) -> Result<Option<String>> {
    find_matching_profile_checked(auth_path)
}

fn exact_active_profile_from_registry_snapshot(
    registry_snapshot: &ProfileRegistrySnapshot,
    live_bytes: &[u8],
) -> Result<Option<String>> {
    let exact = registry_snapshot.exact_matches(live_bytes);
    if exact.is_empty() {
        return Ok(None);
    }
    let current = if exact.len() > 1 {
        read_current_checked()?
    } else {
        None
    };
    select_exact_profile_binding(&exact, current.as_deref())
}

fn non_exact_active_profile_from_registry_snapshot(
    src: &Path,
    live_bytes: &[u8],
    parsed_registry: &ParsedProfileRegistrySnapshot,
) -> Result<Option<String>> {
    let live: serde_json::Value = serde_json::from_slice(live_bytes)
        .with_context(|| format!("parsing live auth {}", src.display()))?;
    let equivalent = parsed_registry.equivalent_matches(&live);
    if !equivalent.is_empty() {
        let current = if equivalent.len() > 1 {
            read_current_checked()?
        } else {
            None
        };
        return select_exact_profile_binding(&equivalent, current.as_deref());
    }

    // If Codex changed the live token bytes directly, only the current marker
    // may bind those credentials back to an alias. A bare identity lookup is
    // ambiguous in stores created by older releases that allowed duplicates.
    let live_identity = extract_identity(&live);
    if let Some(alias) = read_current_checked()?
        && let Some(profile) = parsed_registry.profile(&alias)
        && exact_identity_matches(&extract_identity(profile), &live_identity)
    {
        return Ok(Some(alias));
    }
    // A stale/missing marker may be repaired only when strict identity resolves
    // to exactly one profile. Legacy duplicates deliberately yield `None`.
    let exact_identity = parsed_registry.identity_matches(&live_identity).exact;
    Ok((exact_identity.len() == 1).then(|| exact_identity[0].clone()))
}

fn active_profile_from_registry_snapshot(
    src: &Path,
    registry_snapshot: &ProfileRegistrySnapshot,
    parsed_registry: &ParsedProfileRegistrySnapshot,
) -> Result<Option<String>> {
    let Some(live_bytes) = snapshot_optional_file(src)? else {
        return Ok(None);
    };
    if let Some(exact) =
        exact_active_profile_from_registry_snapshot(registry_snapshot, &live_bytes)?
    {
        return Ok(Some(exact));
    }
    non_exact_active_profile_from_registry_snapshot(src, &live_bytes, parsed_registry)
}

fn active_profile_from_live_with_registry_snapshot()
-> Result<Option<(String, ProfileRegistrySnapshot)>> {
    let src = codex_auth_path()?;
    let Some(live_bytes) = snapshot_optional_file(&src)? else {
        return Ok(None);
    };
    let registry = ProfileRegistry::open()
        .context("resolving the profile registry for active-profile detection")?;
    let registry_snapshot = registry.snapshot()?;
    if let Some(exact) =
        exact_active_profile_from_registry_snapshot(&registry_snapshot, &live_bytes)?
    {
        return Ok(Some((exact, registry_snapshot)));
    }
    let parsed_registry = registry_snapshot.parse()?;
    Ok(
        non_exact_active_profile_from_registry_snapshot(&src, &live_bytes, &parsed_registry)?
            .map(|alias| (alias, registry_snapshot)),
    )
}

pub fn active_profile_from_live() -> Result<Option<String>> {
    Ok(active_profile_from_live_with_registry_snapshot()?.map(|(alias, _snapshot)| alias))
}

/// Return a cheap, non-authoritative candidate for the overwhelmingly common
/// stable state: the current marker's profile bytes exactly equal live auth.
///
/// Any observation failure is a cache miss, not a recovered result. The caller
/// still runs the complete registry scan while holding the synchronization
/// locks before publishing the marker, preserving full fail-closed validation
/// and exact/equivalent/identity match precedence.
fn exact_current_profile_hint() -> Option<String> {
    let alias = read_current_checked().ok().flatten()?;
    let live = std::fs::read(codex_auth_path().ok()?).ok()?;
    let registry = ProfileRegistry::open().ok()?;
    let profile = std::fs::read(registry.auth_path(&alias).ok()?).ok()?;
    (profile == live).then_some(alias)
}

pub(crate) fn sync_current_from_live_with_registry() -> Result<Option<SyncedProfileRegistry>> {
    for _ in 0..4 {
        let candidate = match exact_current_profile_hint() {
            Some(alias) => Some(alias),
            None => active_profile_from_live()?,
        };
        let Some(alias) = candidate else {
            return Ok(None);
        };
        let lease = acquire_profile_lease(&alias)?;
        let transaction = lock_auth_transaction()?;
        let Some((confirmed_alias, snapshot)) = active_profile_from_live_with_registry_snapshot()?
        else {
            drop(transaction);
            drop(lease);
            continue;
        };
        if confirmed_alias != alias {
            drop(transaction);
            drop(lease);
            continue;
        }
        if read_current_checked()?.as_deref() != Some(alias.as_str()) {
            write_current(&alias)?;
        }
        return Ok(Some(SyncedProfileRegistry {
            current: alias,
            snapshot,
        }));
    }
    anyhow::bail!("live active profile kept changing while synchronizing the current marker")
}

pub fn sync_current_from_live() -> Result<Option<String>> {
    Ok(sync_current_from_live_with_registry()?.map(SyncedProfileRegistry::into_current))
}

fn repair_current_for_exact_live_match(alias: &str) -> Result<()> {
    let _lease = acquire_profile_lease(alias)?;
    let _transaction = lock_auth_transaction()?;
    let live_path = codex_auth_path()?;
    let profile_path = profile_auth_path(alias)?;
    let live = std::fs::read(&live_path)
        .with_context(|| format!("reading live auth {}", live_path.display()))?;
    let profile = std::fs::read(&profile_path)
        .with_context(|| format!("reading profile auth {}", profile_path.display()))?;
    if live == profile && read_current_checked()?.as_deref() != Some(alias) {
        write_current(alias)?;
    }
    Ok(())
}

fn repair_current_for_equivalent_live_match(alias: &str) -> Result<()> {
    let _lease = acquire_profile_lease(alias)?;
    let _transaction = lock_auth_transaction()?;
    let live_path = codex_auth_path()?;
    let profile_path = profile_auth_path(alias)?;
    let live = std::fs::read(&live_path)
        .with_context(|| format!("reading live auth {}", live_path.display()))?;
    let profile = std::fs::read(&profile_path)
        .with_context(|| format!("reading profile auth {}", profile_path.display()))?;
    let live: serde_json::Value = serde_json::from_slice(&live)
        .with_context(|| format!("parsing live auth {}", live_path.display()))?;
    let profile: serde_json::Value = serde_json::from_slice(&profile)
        .with_context(|| format!("parsing profile auth {}", profile_path.display()))?;
    if auth_values_semantically_equal(&live, &profile)
        && read_current_checked()?.as_deref() != Some(alias)
    {
        write_current(alias)?;
    }
    Ok(())
}

// ── Deduplication ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountIdentity {
    pub account_id: Option<String>,
    pub email: Option<String>,
}

pub fn extract_identity(auth: &serde_json::Value) -> AccountIdentity {
    let info = parse_account_info(auth);
    AccountIdentity {
        account_id: info.account_id,
        email: info.email.and_then(|email| {
            let email = email.trim().to_lowercase();
            (!email.is_empty()).then_some(email)
        }),
    }
}

fn ensure_same_account_identity(
    alias: &str,
    existing: &serde_json::Value,
    incoming: &serde_json::Value,
) -> Result<()> {
    let existing = extract_identity(existing);
    let incoming = extract_identity(incoming);
    ensure_account_identity_matches(alias, &existing, &incoming)
}

fn require_complete_account_identity(alias: &str, identity: &AccountIdentity) -> Result<()> {
    if identity.account_id.is_none() || identity.email.is_none() {
        anyhow::bail!(
            "profile '{alias}' must contain both account_id and email for strict account identity validation"
        );
    }
    Ok(())
}

fn ensure_account_identity_matches(
    alias: &str,
    existing: &AccountIdentity,
    incoming: &AccountIdentity,
) -> Result<()> {
    let (Some(existing_account_id), Some(incoming_account_id)) =
        (&existing.account_id, &incoming.account_id)
    else {
        anyhow::bail!(
            "refusing to overwrite profile '{alias}': both existing and incoming credentials must contain matching account_id and email values"
        );
    };
    let email_matches = matches!(
        (&existing.email, &incoming.email),
        (Some(existing), Some(incoming)) if existing == incoming
    );
    if email_matches && existing_account_id == incoming_account_id {
        return Ok(());
    }
    anyhow::bail!("authenticated account does not match profile '{alias}'")
}

/// Find a profile with a strict match: both account_id AND email must be present and equal.
/// Used by `auto_track_current` to avoid silently syncing on incomplete identity matches.
pub fn find_profile_by_identity_exact(identity: &AccountIdentity) -> Result<Option<String>> {
    let registry = ProfileRegistry::open()?;
    find_profile_by_identity_exact_in_registry(&registry, identity)
}

fn find_profile_by_identity_exact_in_registry(
    registry: &ProfileRegistry,
    identity: &AccountIdentity,
) -> Result<Option<String>> {
    let matches = scan_profiles_by_identity_in_registry(registry, identity)?.exact;
    Ok((matches.len() == 1).then(|| matches[0].clone()))
}

fn exact_identity_matches(left: &AccountIdentity, right: &AccountIdentity) -> bool {
    matches!(
        (
            left.account_id.as_deref(),
            left.email.as_deref(),
            right.account_id.as_deref(),
            right.email.as_deref(),
        ),
        (Some(left_id), Some(left_email), Some(right_id), Some(right_email))
            if left_id == right_id && left_email == right_email
    )
}

/// Profiles matching an identity, split by match strength so callers can tell
/// an unambiguous hit from "several workspaces share this email".
#[derive(Default)]
struct IdentityMatches {
    /// account_id AND email both equal. Modern writes prevent duplicates, but
    /// legacy stores can contain several aliases with this same identity.
    exact: Vec<String>,
    /// A shared email or account_id while the other identity component is
    /// missing on at least one side. These are diagnostic conflicts only and
    /// are never eligible credential-write targets.
    incomplete_identity_matches: Vec<String>,
}

fn scan_profiles_by_identity(identity: &AccountIdentity) -> Result<IdentityMatches> {
    let registry = ProfileRegistry::open()?;
    scan_profiles_by_identity_in_registry(&registry, identity)
}

fn scan_profiles_by_identity_in_registry(
    registry: &ProfileRegistry,
    identity: &AccountIdentity,
) -> Result<IdentityMatches> {
    Ok(registry.snapshot()?.parse()?.identity_matches(identity))
}

fn collect_identity_matches<'a>(
    identity: &AccountIdentity,
    profiles: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
) -> IdentityMatches {
    let mut matches = IdentityMatches::default();
    for (alias, value) in profiles {
        let existing = extract_identity(value);
        // Match: account_id AND email both equal (same person, same workspace)
        if exact_identity_matches(identity, &existing) {
            matches.exact.push(alias.to_string());
            continue;
        }

        if incomplete_identity_matches(identity, &existing) {
            matches.incomplete_identity_matches.push(alias.to_string());
        }
    }
    matches
}

fn incomplete_identity_matches(left: &AccountIdentity, right: &AccountIdentity) -> bool {
    let shared_account_id = matches!(
        (left.account_id.as_deref(), right.account_id.as_deref()),
        (Some(left), Some(right)) if left == right
    );
    let shared_email = matches!(
        (left.email.as_deref(), right.email.as_deref()),
        (Some(left), Some(right)) if left == right
    );
    let account_id_is_incomplete = left.account_id.is_none() || right.account_id.is_none();
    let email_is_incomplete = left.email.is_none() || right.email.is_none();
    (shared_email && account_id_is_incomplete) || (shared_account_id && email_is_incomplete)
}

pub fn alias_from_email(email: &str) -> String {
    let base = email.split('@').next().unwrap_or(email);
    let alias = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(MAX_ALIAS_LEN)
        .collect::<String>();
    if alias.is_empty() {
        "account".to_string()
    } else {
        alias
    }
}

// ── Return types ──────────────────────────────────────────

#[derive(Debug)]
pub enum SaveAction {
    Created(String),
    Updated(String),
}

impl SaveAction {
    pub fn alias(&self) -> &str {
        match self {
            SaveAction::Created(alias) | SaveAction::Updated(alias) => alias,
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            SaveAction::Created(_) => "created",
            SaveAction::Updated(_) => "updated",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ImportRecoveryCleanupIncomplete {
    pub(crate) recovery_path: Option<PathBuf>,
    pub(crate) cause: anyhow::Error,
}

#[derive(Debug)]
pub(crate) struct ImportProfileCommitIncomplete {
    pub(crate) recovery_path: Option<PathBuf>,
    pub(crate) cause: anyhow::Error,
}

#[derive(Debug)]
pub(crate) struct ImportSaveOutcome {
    pub(crate) action: SaveAction,
    pub(crate) profile_commit: Option<ImportProfileCommitIncomplete>,
    pub(crate) recovery_cleanup: Option<ImportRecoveryCleanupIncomplete>,
}

enum ImportPromotionOutcome {
    Durable {
        recovery_cleanup: Option<ImportRecoveryCleanupIncomplete>,
    },
    ProfileCommitIncomplete(ImportProfileCommitIncomplete),
}

#[derive(Debug)]
pub(crate) enum ValidatedImportCommit {
    Profile(ImportSaveOutcome),
    RecoveryPreserved {
        recovery_path: Option<PathBuf>,
        cause: anyhow::Error,
    },
}

#[derive(Debug)]
pub(crate) enum RecoveredImportAction {
    Profile(ImportSaveOutcome),
    RecoveryPreserved {
        recovery_path: Option<PathBuf>,
        reason: String,
    },
}

/// Durable copy of a credential after the auth server has consumed its
/// previous single-use refresh token.
///
/// The file lives under the app-owned recovery directory. A successful
/// credential commit removes or promotes this exact file, while a failed or
/// interrupted commit deliberately leaves it in recovery.
#[derive(Debug)]
pub(crate) struct RotationRecoveryStage {
    path: PathBuf,
    token: crate::fs_ops::FileToken,
    _directory_guard: crate::auth::PrivateDirectoryGuard,
}

impl RotationRecoveryStage {
    pub(crate) fn persist(&mut self, val: &serde_json::Value) -> Result<()> {
        let contents = serde_json::to_vec_pretty(val)
            .context("serializing rotated credentials for recovery")?;
        let mut candidate = stage_rotation_candidate(&self.path, &contents)?;
        let previous_path = self.path.clone();
        let previous_token = self.token.clone();

        let observed = crate::fs_ops::token_if_present(&previous_path)?;
        if observed.as_ref() != Some(&previous_token) {
            self.adopt_candidate(&mut candidate);
            anyhow::bail!(
                "rotated credential stage {} changed before exact replacement; the latest usable credentials were preserved at {}",
                previous_path.display(),
                self.path.display()
            );
        }

        #[cfg(unix)]
        let displaced = candidate.path.clone();
        #[cfg(windows)]
        let displaced = rotation_displaced_path(&previous_path)?;
        #[cfg(unix)]
        let boundary = crate::fs_ops::exchange(&candidate.path, &previous_path);
        #[cfg(windows)]
        let boundary =
            crate::fs_ops::replace_with_displaced(&candidate.path, &previous_path, &displaced);

        let stage_after = crate::fs_ops::token_if_present(&previous_path)?;
        let candidate_after = crate::fs_ops::token_if_present(&candidate.path)?;
        let displaced_after = if displaced == candidate.path {
            candidate_after.clone()
        } else {
            crate::fs_ops::token_if_present(&displaced)?
        };
        if stage_after.as_ref() == Some(&candidate.token)
            && displaced_after.as_ref() == Some(&previous_token)
            && (displaced == candidate.path || candidate_after.is_none())
        {
            candidate.cleanup_on_drop = false;
            self.token = candidate.token.clone();
            if let Err(durability) =
                crate::auth::confirm_namespace_boundary(&previous_path, &boundary)
            {
                anyhow::bail!(
                    "latest rotated credentials are visible at {}, but exact replacement durability is unconfirmed ({durability:#}); the displaced previous stage was preserved at {}",
                    previous_path.display(),
                    displaced.display()
                );
            }
            if let Err(cleanup) = crate::auth::remove_bound_path(&displaced, &previous_token) {
                return Err(displaced_stage_cleanup_incomplete(
                    &displaced,
                    &previous_token,
                    cleanup,
                )
                .context(format!(
                    "latest rotated credentials were committed at {}",
                    previous_path.display()
                )));
            }
            return Ok(());
        }

        if stage_after.as_ref() == Some(&previous_token)
            && candidate_after.as_ref() == Some(&candidate.token)
            && (displaced == candidate.path || displaced_after.is_none())
        {
            self.adopt_candidate(&mut candidate);
            return Err(boundary.err().unwrap_or_else(|| {
                anyhow::anyhow!(
                    "rotated credential exchange reported success without replacing the exact stage; the latest credentials were preserved at {}",
                    self.path.display()
                )
            }));
        }

        if candidate_after.as_ref() == Some(&candidate.token) {
            self.adopt_candidate(&mut candidate);
        } else {
            candidate.cleanup_on_drop = false;
        }
        Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!(
                "rotated credential exchange left an unclassified state; every observed file was preserved (stage {}, candidate {}, displaced {})",
                previous_path.display(),
                candidate.path.display(),
                displaced.display()
            )
        }))
    }

    fn adopt_candidate(&mut self, candidate: &mut ExactRotationFile) {
        self.path = candidate.path.clone();
        self.token = candidate.token.clone();
        candidate.cleanup_on_drop = false;
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn contains(&self, val: &serde_json::Value) -> Result<bool> {
        let raw = self.read_exact_bytes()?;
        let staged: serde_json::Value = serde_json::from_slice(&raw).with_context(|| {
            format!(
                "parsing exact rotated credential stage {}",
                self.path.display()
            )
        })?;
        Ok(staged == *val)
    }

    fn read_exact_bytes(&self) -> Result<Vec<u8>> {
        let mut file = crate::fs_ops::open_direct_regular(&self.path)?;
        let before = crate::fs_ops::token_for_file(&mut file)?;
        if before != self.token {
            anyhow::bail!(
                "rotated credential stage {} no longer matches its owned file token",
                self.path.display()
            );
        }
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)?;
        let after = crate::fs_ops::token_for_file(&mut file)?;
        let path_after = crate::fs_ops::token_if_present(&self.path)?;
        if after != self.token
            || path_after.as_ref() != Some(&self.token)
            || !self.token.matches_bytes(&raw)
        {
            anyhow::bail!(
                "rotated credential stage {} changed while it was read",
                self.path.display()
            );
        }
        Ok(raw)
    }
}

struct ExactRotationFile {
    path: PathBuf,
    token: crate::fs_ops::FileToken,
    cleanup_on_drop: bool,
}

impl Drop for ExactRotationFile {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Err(error) = crate::auth::remove_bound_path(&self.path, &self.token)
        {
            tracing::warn!(
                "preserving rotated-credential transaction file {} because exact cleanup failed: {error:#}",
                self.path.display()
            );
        }
    }
}

fn stage_rotation_candidate(stage_path: &Path, contents: &[u8]) -> Result<ExactRotationFile> {
    let parent = stage_path.parent().with_context(|| {
        format!(
            "rotated credential stage has no parent: {}",
            stage_path.display()
        )
    })?;
    ensure_private_dir(parent)?;
    let mut candidate = tempfile::Builder::new()
        .prefix(".rotated-credential-candidate-")
        .suffix(".json")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "creating rotated credential candidate in {}",
                parent.display()
            )
        })?;
    #[cfg(windows)]
    crate::auth::harden_windows_private_file(candidate.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        candidate
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    candidate.write_all(contents)?;
    candidate.as_file().sync_all()?;
    let token = crate::fs_ops::token_for_file(candidate.as_file_mut())?;
    let (_file, path) = candidate
        .keep()
        .map_err(|error| error.error)
        .context("retaining exact rotated credential candidate")?;
    // The server has already consumed the previous refresh token. Make the
    // candidate name durable before the exchange so a crash cannot leave only
    // the old, now-dead stage. Windows relies on the synced file plus its
    // supported namespace primitive; Unix additionally fsyncs the directory.
    #[cfg(unix)]
    crate::auth::confirm_namespace_durability(&path).with_context(|| {
        format!(
            "making rotated credential candidate durable before exchange at {}",
            path.display()
        )
    })?;
    Ok(ExactRotationFile {
        path,
        token,
        cleanup_on_drop: true,
    })
}

#[cfg(test)]
thread_local! {
    static TEST_BEFORE_IMPORT_PROMOTION:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static TEST_BEFORE_IMPORT_RECOVERY_CLEANUP:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn before_next_import_promotion(action: impl FnOnce() + 'static) {
    TEST_BEFORE_IMPORT_PROMOTION.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_before_import_promotion_test_hook() {
    TEST_BEFORE_IMPORT_PROMOTION.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(test)]
fn before_next_import_recovery_cleanup(action: impl FnOnce() + 'static) {
    TEST_BEFORE_IMPORT_RECOVERY_CLEANUP.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_before_import_recovery_cleanup_test_hook() {
    TEST_BEFORE_IMPORT_RECOVERY_CLEANUP.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(windows)]
fn rotation_displaced_path(stage_path: &Path) -> Result<PathBuf> {
    use rand::Rng as _;

    const UNIQUE_DISPLACED_PATH_ATTEMPTS: usize = 16;
    let parent = stage_path.parent().with_context(|| {
        format!(
            "rotated credential stage has no parent: {}",
            stage_path.display()
        )
    })?;
    for _ in 0..UNIQUE_DISPLACED_PATH_ATTEMPTS {
        let mut nonce = [0_u8; 16];
        rand::rng().fill_bytes(&mut nonce);
        let candidate = parent.join(format!(
            ".rotated-credential-displaced-{}",
            hex::encode(nonce)
        ));
        if crate::fs_ops::token_if_present(&candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not reserve a collision-free displaced path beside {}",
        stage_path.display()
    )
}

#[derive(Debug)]
pub struct ImportSuccess {
    pub source: PathBuf,
    pub alias: String,
    pub action: &'static str,
    pub account: crate::jwt::AccountInfo,
    pub usage: crate::usage::UsageInfo,
    pub recovery_path: Option<PathBuf>,
    pub cleanup_warning: Option<String>,
}

#[derive(Debug)]
pub struct ImportFailure {
    pub source: PathBuf,
    pub stage: &'static str,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<ImportSuccess>,
    pub skipped: Vec<ImportFailure>,
}

// ── Startup auth change detection ─────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum AuthChange {
    /// No live Codex credential file exists.
    NoLiveAuth,
    /// Live auth.json belongs to a completely new account.
    NewAccount,
    /// Live auth.json differs from every saved profile but contains neither
    /// email nor account_id, so it cannot be bound to a profile safely.
    UnidentifiedAccount,
    /// Live auth.json matches an existing profile's identity but tokens differ.
    TokensUpdated { alias: String },
    /// The live credential resembles saved profiles but lacks enough identity
    /// information to bind it safely.
    UnresolvedIdentity { aliases: Vec<String> },
    /// No actionable change.
    NoChange,
}

/// Result of reconciling live Codex credentials after the TUI is visible.
/// Existing profiles are updated only through the same guarded read-back path
/// used by the interactive startup flow. A genuinely new account remains an
/// explicit user action and is never saved under a guessed alias.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TuiAuthReconciliation {
    NoLiveAuth,
    NoChange,
    ProfileUpdated {
        alias: String,
        info: crate::jwt::AccountInfo,
    },
    UntrackedAccount,
    UnidentifiedAccount,
    UnresolvedIdentity {
        aliases: Vec<String>,
    },
}

pub(crate) fn reconcile_live_auth_for_tui() -> Result<TuiAuthReconciliation> {
    match detect_auth_change()? {
        AuthChange::NoLiveAuth => Ok(TuiAuthReconciliation::NoLiveAuth),
        AuthChange::NoChange => Ok(TuiAuthReconciliation::NoChange),
        AuthChange::TokensUpdated { alias } => {
            update_profile_from_live(&alias)
                .with_context(|| format!("synchronizing newer live credentials for '{alias}'"))?;
            let profile_path = profile_auth_path(&alias)?;
            let auth = read_auth(&profile_path).with_context(|| {
                format!(
                    "reading synchronized profile '{alias}' at {}",
                    profile_path.display()
                )
            })?;
            Ok(TuiAuthReconciliation::ProfileUpdated {
                alias,
                info: crate::auth::account_info_from_auth_value(&auth),
            })
        }
        AuthChange::NewAccount => Ok(TuiAuthReconciliation::UntrackedAccount),
        AuthChange::UnidentifiedAccount => Ok(TuiAuthReconciliation::UnidentifiedAccount),
        AuthChange::UnresolvedIdentity { aliases } => {
            Ok(TuiAuthReconciliation::UnresolvedIdentity { aliases })
        }
    }
}

fn read_live_auth_snapshot_for_detection(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => Ok(None),
                Ok(_) => Err(read_error).with_context(|| {
                    format!(
                        "reading existing live auth path {} during change detection",
                        path.display()
                    )
                }),
                Err(metadata_error) => Err(metadata_error).with_context(|| {
                    format!(
                        "checking whether live auth path {} is absent during change detection",
                        path.display()
                    )
                }),
            }
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "reading live auth for change detection at {}",
                path.display()
            )
        }),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CurrentExactMatch {
    Match,
    Miss,
    Retry,
}

/// Prove the common no-op state without enumerating the complete registry.
///
/// This result is deliberately only a classification: it never returns an
/// alias or an authorization that a caller could carry into a later write.
/// Missing state and unequal bytes require the authoritative registry path;
/// actual marker or profile observation failures remain errors. Rechecking the
/// raw marker bytes prevents a stale marker observation from being published
/// as a stable result while another process switches profiles.
fn exact_current_profile_match_checked(live_snapshot: &[u8]) -> Result<CurrentExactMatch> {
    let Some(marker) = read_current_marker_snapshot_checked()
        .context("reading the current marker for exact live-auth matching")?
    else {
        return Ok(CurrentExactMatch::Miss);
    };
    let profile_path = profile_auth_path(marker.alias())?;
    let profile_snapshot = match std::fs::read(&profile_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CurrentExactMatch::Miss);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "reading current profile '{}' for exact live-auth matching at {}",
                    marker.alias(),
                    profile_path.display()
                )
            });
        }
    };
    if profile_snapshot != live_snapshot {
        return Ok(CurrentExactMatch::Miss);
    }

    #[cfg(test)]
    run_exact_live_binding_test_hook();

    let current = read_current_marker_snapshot_checked()
        .context("rechecking the current marker after exact live-auth matching")?;
    if current.as_ref() != Some(&marker) {
        return Ok(CurrentExactMatch::Retry);
    }
    Ok(CurrentExactMatch::Match)
}

/// Compare live auth.json against saved profiles.
/// - Exact byte match → NoChange
/// - Identity match (email + account_id) but different content → TokensUpdated
/// - A shared email or account_id with an incomplete counterpart → UnresolvedIdentity
/// - Neither email nor account_id is present → UnidentifiedAccount
/// - No identity match → NewAccount
///
/// A genuinely absent live file is `NoLiveAuth`. Path resolution, reads,
/// parsing, marker inspection, and saved-profile scans are observation
/// failures and are returned to the caller instead of impersonating absence.
pub fn detect_auth_change() -> Result<AuthChange> {
    let auth_path = codex_auth_path().context("resolving the live auth path")?;
    let mut unmatched_live = None;
    for _ in 0..PROFILE_CONCURRENCY_RETRY_LIMIT {
        let Some(live_snapshot) = read_live_auth_snapshot_for_detection(&auth_path)? else {
            return Ok(AuthChange::NoLiveAuth);
        };
        let val: serde_json::Value = serde_json::from_slice(&live_snapshot).with_context(|| {
            format!(
                "parsing live auth for change detection at {}",
                auth_path.display()
            )
        })?;
        match exact_current_profile_match_checked(&live_snapshot)? {
            CurrentExactMatch::Match => return Ok(AuthChange::NoChange),
            CurrentExactMatch::Miss => {
                unmatched_live = Some((live_snapshot, val));
                break;
            }
            CurrentExactMatch::Retry => continue,
        }
    }
    let Some((live_snapshot, val)) = unmatched_live else {
        anyhow::bail!("current profile marker kept changing while checking live authentication");
    };
    let registry = ProfileRegistry::open()
        .context("resolving the profile registry for auth change detection")?;
    let registry_snapshot = registry
        .snapshot()
        .context("comparing live auth bytes with saved profiles")?;

    // Exact file matches are authoritative. If legacy duplicates have the same
    // bytes, only their current marker may disambiguate them; never fall through
    // to identity selection after exact-byte ambiguity.
    match registry_snapshot.exact_matches(&live_snapshot) {
        matches if !matches.is_empty() => {
            let is_legacy_duplicate = matches.len() > 1;
            let current = if is_legacy_duplicate {
                read_current_checked().context(
                    "reading the current profile marker to disambiguate exact live-auth matches",
                )?
            } else {
                None
            };
            let alias = select_exact_profile_binding(&matches, current.as_deref())
                .context("binding exact live auth bytes to a saved profile")?
                .ok_or_else(|| {
                    anyhow::anyhow!("exact live-auth comparison returned no profile binding")
                })?;
            // The common case already has the correct marker. It is a pure
            // observation and must not wait behind the profile, legacy-launch,
            // and live-auth locks that are needed only when a repair is due.
            // Marker read failures still fail closed instead of being treated
            // as a missing marker.
            let marker_is_current = if is_legacy_duplicate {
                false
            } else {
                read_current_checked()
                    .with_context(|| {
                        format!("repairing the current marker for exact live profile '{alias}'")
                    })?
                    .as_deref()
                    == Some(alias.as_str())
            };
            #[cfg(test)]
            run_exact_live_binding_test_hook();
            // With duplicate bytes, the marker was the only evidence for
            // `alias`. It may have legitimately changed while the files were
            // scanned, so it must never be "repaired" from that stale
            // observation. A unique exact match remains safe to repair under
            // the transaction's live-byte recheck.
            if !is_legacy_duplicate && !marker_is_current {
                repair_current_for_exact_live_match(&alias).with_context(|| {
                    format!("repairing the current marker for exact live profile '{alias}'")
                })?;
            }
            return Ok(AuthChange::NoChange);
        }
        _ => {}
    }

    let identity = extract_identity(&val);
    // Parsing is delayed until exact-byte matching has failed. This preserves
    // the exact-match authority of a valid profile even when an unrelated
    // legacy profile is malformed, while ensuring every saved file is read at
    // most once during this detection pass.
    let parsed_registry = registry_snapshot.parse().with_context(|| {
        if identity.email.is_none() && identity.account_id.is_none() {
            "comparing identityless live auth with saved profiles"
        } else {
            "comparing the live auth identity with saved profiles"
        }
    })?;
    if identity.email.is_none() && identity.account_id.is_none() {
        // Formatting differences do not make an otherwise identical
        // credential untracked. Exact bytes were handled above; keep the
        // existing semantic-equivalence contract before reporting that this
        // snapshot cannot be identified.
        let equivalent = parsed_registry.equivalent_matches(&val);
        match equivalent.as_slice() {
            [] => return Ok(AuthChange::UnidentifiedAccount),
            [alias] => {
                let marker_is_current = read_current_checked()
                    .context("reading the current marker for equivalent live auth")?
                    .as_deref()
                    == Some(alias.as_str());
                if !marker_is_current {
                    repair_current_for_equivalent_live_match(alias).with_context(|| {
                        format!(
                            "repairing the current marker for semantically equivalent live profile '{alias}'"
                        )
                    })?;
                }
                return Ok(AuthChange::NoChange);
            }
            aliases => {
                let current = read_current_checked().context(
                    "reading the current marker to disambiguate equivalent live-auth matches",
                )?;
                if current
                    .as_ref()
                    .is_some_and(|current| aliases.iter().any(|alias| alias == current))
                {
                    return Ok(AuthChange::NoChange);
                }
                anyhow::bail!(
                    "live auth semantically matches multiple legacy profiles ({}), and the current marker does not disambiguate them",
                    aliases.join(", ")
                );
            }
        }
    }

    // The read-back path writes live credentials into a profile, so one shared
    // identity component cannot authorize a write when the other is missing.
    let IdentityMatches {
        exact,
        incomplete_identity_matches,
    } = parsed_registry.identity_matches(&identity);
    match exact.as_slice() {
        [alias] => {
            return Ok(AuthChange::TokensUpdated {
                alias: alias.clone(),
            });
        }
        [] => {}
        aliases => {
            if let Some(current) = read_current_checked().context(
                "reading the current profile marker to disambiguate matching account identities",
            )? && aliases.iter().any(|alias| alias == &current)
            {
                return Ok(AuthChange::TokensUpdated { alias: current });
            }
            anyhow::bail!(
                "live credentials have the same exact account identity as multiple legacy profiles ({}), and the current marker does not disambiguate them; refusing to choose an update target",
                aliases.join(", ")
            );
        }
    }
    match incomplete_identity_matches.as_slice() {
        [] => Ok(AuthChange::NewAccount),
        incomplete => Ok(AuthChange::UnresolvedIdentity {
            aliases: incomplete.to_vec(),
        }),
    }
}

/// `last_refresh` as written by `auth::apply_tokens` and `login`: an RFC3339
/// string at the auth.json root. Absent or malformed values yield `None`.
fn parse_last_refresh(val: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = val.get("last_refresh")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

fn refresh_token(val: &serde_json::Value) -> Option<&str> {
    val.get("tokens")?.get("refresh_token")?.as_str()
}

fn same_import_credential(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    if left == right {
        return true;
    }
    matches!(
        (refresh_token(left), refresh_token(right)),
        (Some(left), Some(right)) if !left.is_empty() && left == right
    )
}

/// Registry reservation held across refresh-token-consuming import validation
/// and the final create-only commit. Its fields are deliberately private: only
/// the checked constructor below can establish this transaction boundary.
pub(crate) struct ImportCredentialReservation {
    _leases: Vec<ProfileLease>,
    _transaction: AuthTransaction,
    profiles: ParsedProfileRegistrySnapshot,
}

/// Reject a credential already owned by live Codex auth or a saved profile,
/// then retain every relevant lease and the registry transaction until import
/// validation and commit finish. Identity remains an authenticated
/// post-validation check; this preflight deliberately compares only exact
/// credentials and refresh-token ownership.
pub(crate) fn reserve_import_credential_for_validation(
    incoming: &serde_json::Value,
) -> Result<ImportCredentialReservation> {
    let registry = ProfileRegistry::open()
        .context("resolving the profile registry for import credential reservation")?;
    for _ in 0..PROFILE_CONCURRENCY_RETRY_LIMIT {
        let aliases = registry.list_profiles()?;
        #[cfg(test)]
        run_after_import_registry_scan_test_hook();
        let alias_refs = aliases.iter().map(String::as_str).collect::<Vec<_>>();
        let leases = acquire_profile_leases(&alias_refs)?;
        let transaction = lock_auth_transaction()?;

        // A creator may have committed a new alias after the first directory
        // scan but before this transaction was acquired. Only a stable second
        // scan proves that every committed profile is covered by the leases
        // retained through validation and final import publication.
        if registry.list_profiles()? != aliases {
            drop(transaction);
            drop(leases);
            continue;
        }

        let live_path = codex_auth_path()?;
        if let Some(live) = read_existing_auth(&live_path)?
            && same_import_credential(incoming, &live)
        {
            anyhow::bail!(
                "refusing duplicate import: live Codex auth already owns this credential or refresh_token"
            );
        }

        let profiles = registry.snapshot_aliases(&aliases)?.parse()?;
        for saved in &profiles.profiles {
            if same_import_credential(incoming, &saved.value) {
                anyhow::bail!(
                    "refusing duplicate import: profile '{}' already owns this credential or refresh_token",
                    saved.alias
                );
            }
        }
        return Ok(ImportCredentialReservation {
            _leases: leases,
            _transaction: transaction,
            profiles,
        });
    }

    anyhow::bail!(
        "profile registry kept changing while reserving imported credentials; retry the command"
    )
}

/// How an auth.json's `last_refresh` looks to the rollback guard, phrased for
/// the user: a refusal has to say what each side actually carried, otherwise
/// "cannot be ordered" is unactionable.
fn describe_last_refresh(val: &serde_json::Value) -> String {
    match (
        parse_last_refresh(val),
        val.get("last_refresh").and_then(|v| v.as_str()),
    ) {
        (Some(ts), _) => ts.to_string(),
        (None, Some(raw)) => format!("unparseable last_refresh '{raw}'"),
        (None, None) => "no last_refresh".to_string(),
    }
}

/// The incoming credentials would replace a `refresh_token` that cannot be
/// shown to be the dead one, so writing them risks destroying the account's
/// only working credential.
///
/// Typed rather than a bare message: callers decide whether to surface this to
/// the user, and matching on error text couples them to this wording — a
/// rewording would silently turn the check off instead of failing to compile.
#[derive(Debug)]
pub struct StaleLiveAuth {
    pub alias: String,
    /// The incoming copy's `last_refresh` state, as `describe_last_refresh` renders it.
    pub live: String,
    /// The stored profile's `last_refresh` state, same rendering.
    pub profile: String,
}

impl std::fmt::Display for StaleLiveAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to overwrite profile '{}': the incoming credentials carry a different \
             refresh_token and cannot be shown to be the newer of the two \
             (incoming: {}; profile: {}). Refresh tokens are single-use, so the older copy is \
             already revoked and overwriting would destroy the working one. \
             `codex-switch-global-pace use {}` keeps the profile's credentials and pushes them \
             back into ~/.codex/auth.json. To replace them safely, run \
             `codex-switch-global-pace login {}` and then `codex-switch-global-pace use {}`.",
            self.alias, self.live, self.profile, self.alias, self.alias, self.alias
        )
    }
}

impl std::error::Error for StaleLiveAuth {}

/// Refuse to replace a profile's `refresh_token` with one that cannot be proven newer.
///
/// OpenAI rotates `refresh_token` on every use: of two different tokens for the
/// same account, exactly one is still usable and the other is already dead.
/// Picking wrong is unrecoverable without a full re-login, so the guard demands
/// positive evidence before letting a rotation through.
///
/// `last_refresh` is only weak evidence — it is wall-clock, second-resolution,
/// and moves backwards on NTP corrections — so it is allowed to decide only when
/// both sides carry a parseable stamp and those stamps actually differ. Equal,
/// missing or malformed stamps are a conflict, not a default. The common case
/// never reaches the timestamps at all: an ordinary sync rotates `access_token`
/// while `refresh_token` stays put, and identical tokens cannot revoke anything.
fn ensure_live_not_older(
    alias: &str,
    profile: &serde_json::Value,
    incoming: &serde_json::Value,
) -> Result<()> {
    if refresh_token(profile) == refresh_token(incoming) {
        return Ok(());
    }
    if let (Some(incoming_ts), Some(profile_ts)) =
        (parse_last_refresh(incoming), parse_last_refresh(profile))
        && incoming_ts > profile_ts
    {
        return Ok(());
    }
    Err(StaleLiveAuth {
        alias: alias.to_string(),
        live: describe_last_refresh(incoming),
        profile: describe_last_refresh(profile),
    }
    .into())
}

/// The one door through which credentials reach an existing profile.
///
/// Every entry point that copies an already-minted auth.json into the profile
/// store (`cmd_save`, `update_profile_from_live`, and login replacement) goes
/// through here, so the two invariants cannot be bypassed by adding a caller:
/// the credentials must belong to this profile's account, and they must not
/// roll its single-use `refresh_token` backwards. Imports are create-only and
/// never select an existing profile.
///
/// A profile that does not exist yet has nothing to protect, so this doubles as
/// the create path; callers that require an existing profile check that first.
fn write_profile_credentials(alias: &str, incoming: &serde_json::Value) -> Result<()> {
    validate_alias(alias)?;
    crate::auth::validate_managed_auth_value(incoming)?;
    let dst = profile_auth_path(alias)?;
    if let Some(existing) = read_existing_auth(&dst)? {
        ensure_same_account_identity(alias, &existing, incoming)?;
        ensure_live_not_older(alias, &existing, incoming)?;
    }
    ensure_profile_parent(&dst)?;
    write_profile_auth_durably(alias, &dst, incoming)
}

fn write_profile_auth_durably(alias: &str, path: &Path, value: &serde_json::Value) -> Result<()> {
    let outcome = write_auth(path, value)?;
    require_durable_private_write(path, "profile credentials", outcome).with_context(|| {
        format!(
            "profile '{alias}' now contains the new credentials, but their durable commit is incomplete"
        )
    })
}

/// The profile these credentials provably belong to, if there is exactly one.
///
/// `Err` whenever an email or account_id resembles an existing profile while
/// the other identity component is missing on either side. One component alone
/// never authorizes credential replacement.
fn resolve_identity_target(
    registry: &ProfileRegistry,
    identity: &AccountIdentity,
) -> Result<Option<String>> {
    let IdentityMatches {
        exact,
        mut incomplete_identity_matches,
    } = scan_profiles_by_identity_in_registry(registry, identity)?;
    match exact.as_slice() {
        [alias] => return Ok(Some(alias.clone())),
        [] => {}
        aliases => anyhow::bail!(
            "Cannot safely choose between {} legacy profiles with the same account_id and email ({}). Refusing to overwrite either profile; name the existing alias explicitly.",
            aliases.len(),
            aliases.join(", ")
        ),
    }
    if incomplete_identity_matches.is_empty() {
        return Ok(None);
    }
    incomplete_identity_matches.sort();
    anyhow::bail!(
        "Cannot safely match credentials to {} existing profile(s) ({}) because account_id or email is missing on at least one side. Refusing to overwrite credentials from incomplete identity evidence; run `codex-switch-global-pace login <alias>` so both credentials contain account_id and email.",
        incomplete_identity_matches.len(),
        incomplete_identity_matches.join(", ")
    )
}

/// Where credentials should land when the user named an alias explicitly.
///
/// An existing explicitly named alias is still subject to strict identity
/// validation before any write. For a new alias, only a complete exact identity
/// match may redirect the write to an existing profile.
fn resolve_named_target(
    registry: &ProfileRegistry,
    alias: &str,
    identity: &AccountIdentity,
) -> Result<Option<String>> {
    validate_alias(alias)?;
    if path_exists_checked(&registry.auth_path(alias)?)? {
        return Ok(Some(alias.to_string()));
    }
    resolve_identity_target(registry, identity)
}

#[derive(Debug)]
struct ProfileWritePlan {
    alias: String,
    updated: bool,
    requested_alias_taken: bool,
}

fn plan_profile_write(
    identity: &AccountIdentity,
    hint_alias: Option<&str>,
) -> Result<ProfileWritePlan> {
    let registry = ProfileRegistry::open()?;
    let existing = match hint_alias {
        Some(alias) => resolve_named_target(&registry, alias, identity)?,
        None => resolve_identity_target(&registry, identity)?,
    };
    if let Some(alias) = existing {
        return Ok(ProfileWritePlan {
            alias,
            updated: true,
            requested_alias_taken: false,
        });
    }

    let requested = hint_alias
        .map(str::to_string)
        .or_else(|| identity.email.as_deref().map(alias_from_email))
        .unwrap_or_else(|| "account".to_string());
    validate_alias(&requested)?;
    let alias = if path_exists_checked(&registry.auth_path(&requested)?)? {
        make_unique_alias(&registry, &requested)?
    } else {
        requested.clone()
    };
    validate_alias(&alias)?;
    Ok(ProfileWritePlan {
        requested_alias_taken: alias != requested,
        alias,
        updated: false,
    })
}

/// Copy an exact live-auth observation into an existing profile and update the
/// derived activation marker only while that observation remains current.
/// Live auth is never rewritten by this read-back operation.
pub fn update_profile_from_live(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let lease = acquire_profile_lease(alias)?;
    update_profile_from_live_guarded(&lease, None, LiveProfileSyncMode::PersistAndMark)
}

/// Wait for the active profile's credential work and persist live auth only
/// when it actually differs. An exact match needs no profile or marker write.
pub(crate) fn synchronize_profile_from_live_for_switch_leased(lease: &ProfileLease) -> Result<()> {
    update_profile_from_live_guarded(lease, None, LiveProfileSyncMode::SwitchBoundary)
}

pub(crate) fn update_profile_from_live_if_current_marker(
    alias: &str,
    expected_marker: &CurrentMarkerSnapshot,
) -> Result<()> {
    if expected_marker.alias() != alias {
        anyhow::bail!(
            "post-command synchronization was authorized for marker '{}' instead of profile '{alias}'",
            expected_marker.alias()
        );
    }
    let lease = acquire_profile_lease(alias)?;
    update_profile_from_live_guarded(
        &lease,
        Some(expected_marker),
        LiveProfileSyncMode::PersistAndMark,
    )
}

pub(crate) fn ensure_current_marker_unchanged(expected: &CurrentMarkerSnapshot) -> Result<()> {
    let current = read_current_marker_snapshot_checked()?.ok_or_else(|| {
        anyhow::anyhow!(
            "current profile marker disappeared after profile '{}' was synchronized at startup",
            expected.alias()
        )
    })?;
    if current != *expected {
        anyhow::bail!(
            "current profile marker changed after profile '{}' was synchronized at startup; post-command credentials were not assigned to a guessed alias",
            expected.alias()
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LiveProfileSyncMode {
    PersistAndMark,
    SwitchBoundary,
}

fn update_profile_from_live_guarded(
    lease: &ProfileLease,
    expected_marker: Option<&CurrentMarkerSnapshot>,
    mode: LiveProfileSyncMode,
) -> Result<()> {
    let alias = lease.alias();
    validate_alias(alias)?;
    let _transaction = lock_auth_transaction()?;
    if let Some(expected_marker) = expected_marker {
        ensure_current_marker_unchanged(expected_marker)?;
    }
    let live_path = codex_auth_path()?;
    // This is a read-back into a profile the caller already knows, not a save:
    // a missing profile is its own failure, distinct from the guards below.
    let profile_path = profile_auth_path(alias)?;
    let Some(profile) = read_existing_auth(&profile_path)? else {
        return Err(CsError::NotFound(alias.to_string()).into());
    };
    let mut initial_live = None;
    if mode == LiveProfileSyncMode::SwitchBoundary {
        let live_snapshot = snapshot_optional_file(&live_path)?.ok_or_else(|| {
            anyhow::anyhow!("live auth disappeared while updating profile '{alias}'")
        })?;
        let live: serde_json::Value = serde_json::from_slice(&live_snapshot)
            .with_context(|| format!("parsing live auth {}", live_path.display()))?;
        if auth_values_semantically_equal(&profile, &live) {
            return Ok(());
        }
        initial_live = Some((live_snapshot, live));
    }

    let mut profile_was_updated = false;
    for _ in 0..PROFILE_CONCURRENCY_RETRY_LIMIT {
        if let Some(expected_marker) = expected_marker {
            ensure_current_marker_unchanged(expected_marker)?;
        }
        let (live_snapshot, live) = match initial_live.take() {
            Some(observation) => observation,
            None => {
                let live_snapshot = snapshot_optional_file(&live_path)?.ok_or_else(|| {
                    anyhow::anyhow!("live auth disappeared while updating profile '{alias}'")
                })?;
                let live = serde_json::from_slice(&live_snapshot)
                    .with_context(|| format!("parsing live auth {}", live_path.display()))?;
                (live_snapshot, live)
            }
        };
        if let Err(error) = write_profile_credentials(alias, &live) {
            if profile_was_updated {
                return Err(error).context(format!(
                    "profile '{alias}' contains the last complete live-auth snapshot, but a newer live credential could not be safely synchronized; live auth and the activation marker were not overwritten"
                ));
            }
            return Err(error);
        }
        profile_was_updated = true;

        #[cfg(test)]
        run_after_update_profile_write_test_hook();
        if snapshot_optional_file(&live_path)?.as_deref() != Some(live_snapshot.as_slice()) {
            continue;
        }

        if let Some(expected_marker) = expected_marker {
            // The marker already contains the desired alias. Rewriting it would
            // create a stale path-based overwrite window, so only verify the
            // exact authorized bytes and leave the marker untouched.
            return ensure_current_marker_unchanged(expected_marker).with_context(|| {
                format!(
                    "profile '{alias}' contains the exact current live credentials, but its startup marker authorization is no longer current; live auth and the marker were not overwritten"
                )
            });
        }
        return write_activation_marker(alias).with_context(|| {
            format!(
                "profile '{alias}' contains the exact current live credentials, but updating its activation marker failed; live auth was not overwritten"
            )
        });
    }

    anyhow::bail!(
        "profile '{alias}' contains the last complete live-auth snapshot, but live auth kept changing before its activation marker could be updated; live auth and the activation marker were not overwritten"
    )
}

// ── Auto-track ────────────────────────────────────────────

/// If the live auth.json belongs to an untracked account, auto-save it.
/// Returns true if a new profile was created.
pub fn auto_track_current() -> Result<bool> {
    let src = codex_auth_path()?;
    let mut unmatched_live = None;
    for _ in 0..PROFILE_CONCURRENCY_RETRY_LIMIT {
        let Some(live) = snapshot_optional_file(&src)? else {
            return Ok(false);
        };
        let val: serde_json::Value = serde_json::from_slice(&live)
            .with_context(|| format!("parsing live auth {}", src.display()))?;
        match exact_current_profile_match_checked(&live)? {
            CurrentExactMatch::Match => return Ok(false),
            CurrentExactMatch::Miss => {
                unmatched_live = Some(val);
                break;
            }
            CurrentExactMatch::Retry => continue,
        }
    }
    let Some(val) = unmatched_live else {
        anyhow::bail!("current profile marker kept changing while checking live authentication");
    };
    let identity = extract_identity(&val);

    if find_profile_by_identity_exact(&identity)?.is_some() {
        // Exact match (account_id + email) — safe to sync the current pointer.
        sync_current_from_live()?;
        return Ok(false);
    }
    if let SaveAction::Created(a) = cmd_save(None)? {
        user_println(&format!("Auto-saved current account as profile: {a}"));
        return Ok(true);
    }
    Ok(false)
}

// ── Command implementations ───────────────────────────────

pub fn cmd_save(alias: Option<&str>) -> Result<SaveAction> {
    let src = codex_auth_path()?;
    let preview = read_auth(&src)?;
    let mut plan = plan_profile_write(&extract_identity(&preview), alias)?;

    for _ in 0..PROFILE_CONCURRENCY_RETRY_LIMIT {
        let lease = acquire_profile_lease(&plan.alias)?;
        let _transaction = lock_auth_transaction()?;
        let live_snapshot = snapshot_optional_file(&src)?
            .ok_or_else(|| anyhow::anyhow!("live auth disappeared during profile save"))?;
        let val: serde_json::Value = serde_json::from_slice(&live_snapshot)
            .with_context(|| format!("parsing exact live auth snapshot {}", src.display()))?;
        let confirmed = plan_profile_write(&extract_identity(&val), alias)?;
        if confirmed.alias != lease.alias() {
            plan = confirmed;
            continue;
        }

        write_profile_credentials(&confirmed.alias, &val)?;
        commit_fresh_credentials_activation(&confirmed.alias, &val, Some(&live_snapshot))?;
        let action = if confirmed.updated {
            SaveAction::Updated(confirmed.alias.clone())
        } else {
            SaveAction::Created(confirmed.alias.clone())
        };
        match (&action, alias) {
            (SaveAction::Updated(target), Some(named)) if named != target => {
                user_println(&format!(
                    "Duplicate account detected -- updated existing profile: {target} (not creating {named})"
                ));
            }
            (SaveAction::Updated(target), _) => {
                user_println(&format!("Updated profile: {target}"));
            }
            (SaveAction::Created(target), Some(named)) if confirmed.requested_alias_taken => {
                user_println(&format!(
                    "Saved profile: {target} (alias '{named}' already taken)"
                ));
            }
            (SaveAction::Created(target), _) => {
                user_println(&format!("Saved profile: {target}"));
            }
        }
        return Ok(action);
    }
    anyhow::bail!("profile destination kept changing while saving; retry the command")
}

fn make_unique_alias(registry: &ProfileRegistry, base: &str) -> Result<String> {
    const MAX_RETRIES: u32 = 1000;
    let mut n: u32 = 2;
    loop {
        let suffix = format!("_{n}");
        let prefix_len = MAX_ALIAS_LEN.saturating_sub(suffix.len());
        let prefix = base.chars().take(prefix_len).collect::<String>();
        let candidate = format!("{prefix}{suffix}");
        if !path_exists_checked(&registry.auth_path(&candidate)?)? {
            return Ok(candidate);
        }
        n += 1;
        if n > MAX_RETRIES {
            anyhow::bail!(
                "could not generate a unique alias for '{base}' after {MAX_RETRIES} attempts"
            );
        }
    }
}

pub(crate) fn switch_profile_with_prompt(
    alias: &str,
    allow_prompt: bool,
) -> Result<ProfileSwitchOutcome> {
    let confirmed = prepare_and_confirm_profile_switch(alias, allow_prompt)?;
    commit_confirmed_profile_switch(confirmed)
}

pub fn cmd_use(alias: &str, allow_prompt: bool) -> Result<()> {
    let outcome = switch_profile_with_prompt(alias, allow_prompt)?;
    if let Some(error) = outcome.selection_history_warning() {
        user_println(&crate::safe_text::terminal_text(&format!(
            "Warning: profile '{alias}' was switched successfully, but its selection history could not be recorded: {error:#}"
        )));
    }
    user_println(&format!("Switched to profile: {alias}"));
    Ok(())
}

pub fn switch_profile(alias: &str) -> Result<()> {
    let prepared = prepare_profile_switch(alias)?;
    let confirmed = confirm_prepared_profile_switch_without_overwrite(prepared)?;
    let outcome = commit_confirmed_profile_switch(confirmed)?;
    if let Some(error) = outcome.selection_history_warning() {
        tracing::warn!(
            alias,
            "profile switched but selection history was not recorded: {error:#}"
        );
    }
    Ok(())
}

pub fn cmd_delete(alias: &str) -> Result<ProfileMutationOutcome> {
    validate_alias(alias)?;
    let _lease = acquire_profile_lease(alias)?;
    let _transaction = lock_auth_transaction()?;
    let registry = ProfileRegistry::open()?;
    let dir = registry.profile_dir(alias)?;
    if !path_exists_checked(&dir)? {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    // Reading the credential is intentional even when live auth is missing:
    // a corrupt or unreadable existing profile must never be archived as if it
    // had safely passed the active-account check.
    read_auth(&registry.auth_path(alias)?)?;
    if live_belongs_to_profile_locked(&registry, alias)? {
        return Err(CsError::ActiveProfileDelete(alias.to_string()).into());
    }
    let current_path = current_file()?;
    let current_snapshot = snapshot_optional_file(&current_path)?;
    let stale_marker = read_current_checked()?.as_deref() == Some(alias);
    let deleted_dir = deleted_profiles_dir()?;
    ensure_private_dir(&deleted_dir)?;
    let archived = deleted_profile_archive_path(alias)?;
    // Cache state is reconstructable; clear it before moving the recoverable
    // profile so a cache write failure cannot leave the command half-deleted.
    crate::cache::purge_profile(alias)?;
    if stale_marker {
        repair_stale_current_marker_locked(alias)?;
    }
    let archive = match crate::fs_ops::rename_directory_noreplace_durable(&dir, &archived)
        .with_context(|| {
            format!(
                "archiving profile directory {} to {}",
                dir.display(),
                archived.display()
            )
        }) {
        Ok(outcome) => outcome,
        Err(error) => {
            if stale_marker
                && let Err(rollback) =
                    restore_file_snapshot(&current_path, current_snapshot.as_deref())
            {
                anyhow::bail!(
                    "deleting profile failed ({error:#}) and restoring its current marker failed ({rollback:#})"
                );
            }
            return Err(error);
        }
    };
    match archive {
        crate::fs_ops::DirectoryRenameOutcome::DurablyRenamed => {
            Ok(ProfileMutationOutcome::committed())
        }
        crate::fs_ops::DirectoryRenameOutcome::VisibleDurabilityUnconfirmed { cause } => {
            Ok(ProfileMutationOutcome::committed_with_warnings(vec![
                anyhow::anyhow!(
                    "profile '{alias}' is visibly archived at {}, but the directory rename durability could not be confirmed: {cause:#}",
                    archived.display()
                ),
            ]))
        }
    }
}

pub fn collect_import_files(path: &Path) -> Result<Vec<PathBuf>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CsError::NoAuthFile(path.display().to_string()).into());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading metadata for {}", path.display()));
        }
    };
    if metadata.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "import path is not a regular file or directory: {}",
            path.display()
        );
    }

    let mut files = vec![];
    collect_import_files_recursive(path, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_import_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry
            .file_type()
            .with_context(|| format!("reading file type of {}", path.display()))?
            .is_dir()
        {
            collect_import_files_recursive(&path, files)?;
            continue;
        }
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            files.push(path);
        }
    }
    Ok(())
}

pub fn save_imported_auth_value(
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    validated_account_id: &str,
    suggested_alias: Option<&str>,
) -> Result<SaveAction> {
    let committed = save_imported_auth_value_with_stage(
        val,
        hint_alias,
        validated_account_id,
        suggested_alias,
        None,
    )?;
    let ValidatedImportCommit::Profile(outcome) = committed else {
        unreachable!("an import without rotation material cannot preserve a recovery stage")
    };
    debug_assert!(outcome.profile_commit.is_none());
    debug_assert!(outcome.recovery_cleanup.is_none());
    Ok(outcome.action)
}

pub(crate) fn save_imported_auth_value_with_stage(
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    validated_account_id: &str,
    suggested_alias: Option<&str>,
    stage: Option<RotationRecoveryStage>,
) -> Result<ValidatedImportCommit> {
    let result = (|| {
        let _transaction = lock_auth_transaction()?;
        save_imported_auth_value_with_stage_locked(
            val,
            hint_alias,
            validated_account_id,
            suggested_alias,
            stage.as_ref(),
            None,
        )
    })();
    classify_validated_import_commit(stage.as_ref(), result)
}

pub(crate) fn save_reserved_imported_auth_value_with_stage(
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    validated_account_id: &str,
    suggested_alias: Option<&str>,
    stage: Option<RotationRecoveryStage>,
    reservation: ImportCredentialReservation,
) -> Result<ValidatedImportCommit> {
    let result = save_imported_auth_value_with_stage_locked(
        val,
        hint_alias,
        validated_account_id,
        suggested_alias,
        stage.as_ref(),
        Some(&reservation.profiles),
    );
    // Keep every reserved profile lease and the auth transaction alive through
    // the create-only commit that consumes the retained registry snapshot.
    drop(reservation);
    classify_validated_import_commit(stage.as_ref(), result)
}

fn save_imported_auth_value_with_stage_locked(
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    validated_account_id: &str,
    suggested_alias: Option<&str>,
    stage: Option<&RotationRecoveryStage>,
    reserved_profiles: Option<&ParsedProfileRegistrySnapshot>,
) -> Result<ImportSaveOutcome> {
    let identity = extract_identity(val);
    let account_id = identity
        .account_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("imported auth must contain a non-empty account_id"))?;
    if account_id != validated_account_id {
        anyhow::bail!(
            "imported account_id '{account_id}' does not match Usage API validated account_id \
             '{validated_account_id}'"
        );
    }
    crate::auth::validate_managed_auth_value(val)?;

    // Usage API proves the bearer can access this workspace, but a Team
    // workspace id is shared by multiple users and the JWT is not
    // signature-verified here. It therefore cannot prove ownership of an
    // existing profile. Imports are create-only, but an exact account identity
    // already owned by another alias is not a distinct account and must not be
    // duplicated. Alias/path collisions for different identities still get a
    // unique destination.
    match reserved_profiles {
        Some(profiles) => refuse_duplicate_import_identity_in_snapshot(&identity, profiles)?,
        None => refuse_duplicate_import_identity(&identity)?,
    }
    create_import_profile(val, hint_alias, suggested_alias, stage)
}

fn classify_validated_import_commit(
    stage: Option<&RotationRecoveryStage>,
    result: Result<ImportSaveOutcome>,
) -> Result<ValidatedImportCommit> {
    match (stage, result) {
        (_, Ok(outcome)) => Ok(ValidatedImportCommit::Profile(outcome)),
        (None, Err(cause)) => Err(cause),
        (Some(stage), Err(cause)) => {
            let (recovery_path, cause) = classify_import_recovery_after_failure(stage, cause);
            Ok(ValidatedImportCommit::RecoveryPreserved {
                recovery_path,
                cause,
            })
        }
    }
}

fn refuse_duplicate_import_identity(identity: &AccountIdentity) -> Result<()> {
    let existing = scan_profiles_by_identity(identity)?.exact;
    refuse_duplicate_import_identity_matches(existing)
}

fn refuse_duplicate_import_identity_in_snapshot(
    identity: &AccountIdentity,
    profiles: &ParsedProfileRegistrySnapshot,
) -> Result<()> {
    let existing = profiles.identity_matches(identity).exact;
    refuse_duplicate_import_identity_matches(existing)
}

fn refuse_duplicate_import_identity_matches(existing: Vec<String>) -> Result<()> {
    if let [alias] = existing.as_slice() {
        anyhow::bail!(
            "refusing duplicate import: profile '{alias}' already has the same account_id and email"
        );
    }
    if existing.len() > 1 {
        anyhow::bail!(
            "refusing duplicate import: profile(s) '{}' already have the same account_id and email",
            existing.join(", ")
        );
    }
    Ok(())
}

/// Preserve credentials rotated by the auth server after validation later
/// failed. Without a successful Usage API response they may never overwrite an
/// existing profile; a unique recovery profile is the only safe destination.
pub(crate) fn save_recovered_import_auth_value_with_stage(
    val: serde_json::Value,
    hint_alias: Option<&str>,
    suggested_alias: Option<&str>,
    stage: Option<RotationRecoveryStage>,
) -> Result<RecoveredImportAction> {
    let (current_stage, stale_stage) = match stage {
        Some(stage) => match stage.contains(&val) {
            Ok(true) => (Some(stage), None),
            Ok(false) => (
                None,
                Some((
                    stage.path,
                    stage.token,
                    "the earlier recovery stage contains a superseded refresh token".to_string(),
                )),
            ),
            Err(error) => (
                None,
                Some((
                    stage.path,
                    stage.token,
                    format!("the earlier recovery stage could not be verified: {error:#}"),
                )),
            ),
        },
        None => (None, None),
    };
    // A refresh token has already been consumed server-side. Make the latest
    // value durable before waiting for any global profile-registry lock; that
    // lock may legitimately be held by another interactive operation.
    let current_stage = match current_stage {
        Some(stage) => stage,
        None => stage_import_rotation(&val).with_context(|| {
            let stale_detail = stale_stage
                .as_ref()
                .map(|(_, _, detail)| format!("; {detail}"))
                .unwrap_or_default();
            format!("saving the latest rotated credentials before profile recovery{stale_detail}")
        })?,
    };
    remove_stale_import_stage(stale_stage.as_ref());

    let _transaction = match lock_auth_transaction() {
        Ok(transaction) => transaction,
        Err(error) => {
            let (recovery_path, cause) =
                classify_import_recovery_after_failure(&current_stage, error);
            return Ok(RecoveredImportAction::RecoveryPreserved {
                recovery_path,
                reason: format!(
                    "profile recovery could not acquire the profile-registry transaction: {:#}",
                    cause
                ),
            });
        }
    };
    let account_id = extract_identity(&val)
        .account_id
        .filter(|account_id| !account_id.is_empty());
    let validation = account_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("rotated credentials have no authenticated account_id"))
        .and_then(|_| crate::auth::validate_managed_auth_value(&val));
    let profile_result = validation.and_then(|_| {
        let identity = extract_identity(&val);
        refuse_duplicate_import_identity(&identity)?;
        create_import_profile(&val, hint_alias, suggested_alias, Some(&current_stage))
            .map(RecoveredImportAction::Profile)
    });
    match profile_result {
        Ok(action) => Ok(action),
        Err(error) => {
            let (recovery_path, cause) =
                classify_import_recovery_after_failure(&current_stage, error);
            Ok(RecoveredImportAction::RecoveryPreserved {
                recovery_path,
                reason: format!("{cause:#}"),
            })
        }
    }
}

fn remove_stale_import_stage(stale_stage: Option<&(PathBuf, crate::fs_ops::FileToken, String)>) {
    let Some((path, token, _)) = stale_stage else {
        return;
    };
    if let Err(error) = crate::auth::remove_bound_path(path, token) {
        tracing::warn!(
            "Current rotated import credentials were recovered, but stale recovery file {} could not be removed exactly: {error:#}",
            path.display()
        );
    }
}

pub(crate) fn stage_import_rotation(val: &serde_json::Value) -> Result<RotationRecoveryStage> {
    stage_rotation_recovery(val, "rotated-import-")
}

fn stage_refresh_rotation(alias: &str, val: &serde_json::Value) -> Result<RotationRecoveryStage> {
    validate_alias(alias)?;
    stage_rotation_recovery(val, &format!("rotated-refresh-{alias}-"))
}

fn stage_rotation_recovery(
    val: &serde_json::Value,
    file_prefix: &str,
) -> Result<RotationRecoveryStage> {
    let contents =
        serde_json::to_vec_pretty(val).context("serializing rotated credentials for recovery")?;
    let (path, token, directory_guard) = create_rotation_recovery_file(file_prefix, &contents)?;
    Ok(RotationRecoveryStage {
        path,
        token,
        _directory_guard: directory_guard,
    })
}

fn create_rotation_recovery_file(
    file_prefix: &str,
    contents: &[u8],
) -> Result<(
    PathBuf,
    crate::fs_ops::FileToken,
    crate::auth::PrivateDirectoryGuard,
)> {
    let recovery_dir = crate::auth::app_home()?.join("recovery");
    let directory_guard = crate::auth::acquire_private_directory(&recovery_dir)?;
    let mut reserved = tempfile::Builder::new()
        .prefix(file_prefix)
        .suffix(".json")
        .tempfile_in(&recovery_dir)
        .with_context(|| {
            format!(
                "reserving a unique rotated-credential recovery file in {}",
                recovery_dir.display()
            )
        })?;
    #[cfg(windows)]
    crate::auth::harden_windows_private_file(reserved.path())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        reserved
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    reserved
        .write_all(contents)
        .context("writing rotated credentials to their initial recovery file")?;
    reserved
        .as_file()
        .sync_all()
        .context("synchronizing the initial rotated-credential recovery file")?;
    let token = crate::fs_ops::token_for_file(reserved.as_file_mut())?;
    let (_file, path) = reserved
        .keep()
        .map_err(|error| error.error)
        .context("preserving the written rotated-credential recovery file")?;
    #[cfg(unix)]
    if let Err(durability) = crate::auth::confirm_namespace_durability(&path) {
        match crate::fs_ops::token_if_present(&path) {
            Ok(Some(observed)) if observed == token => anyhow::bail!(
                "the exact rotated credentials are visible and file-synchronized at {}, but creation of that recovery name is not durably confirmed ({durability:#}); the exact file was left in place and the profile commit was not started",
                path.display()
            ),
            Ok(Some(_)) => anyhow::bail!(
                "rotated-credential recovery-name durability is unconfirmed ({durability:#}), and {} was replaced before its original file identity could be reported; the profile commit was not started",
                path.display()
            ),
            Ok(None) => anyhow::bail!(
                "rotated-credential recovery-name durability is unconfirmed ({durability:#}), and the written file at {} is no longer present; the profile commit was not started",
                path.display()
            ),
            Err(observation) => anyhow::bail!(
                "rotated-credential recovery-name durability is unconfirmed ({durability:#}), and the file identity at {} could not be revalidated ({observation:#}); the profile commit was not started",
                path.display()
            ),
        }
    }
    Ok((path, token, directory_guard))
}

fn create_import_profile(
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    suggested_alias: Option<&str>,
    stage: Option<&RotationRecoveryStage>,
) -> Result<ImportSaveOutcome> {
    let registry = ProfileRegistry::open()?;
    let identity = extract_identity(val);
    let alias = hint_alias
        .map(|s| s.to_string())
        .or_else(|| identity.email.as_deref().map(alias_from_email))
        .or_else(|| suggested_alias.map(str::to_string))
        .unwrap_or_else(|| "account".to_string());
    validate_alias(&alias)?;
    let alias = if path_exists_checked(&registry.auth_path(&alias)?)? {
        make_unique_alias(&registry, &alias)?
    } else {
        alias
    };
    validate_alias(&alias)?;

    let (profile_commit, recovery_cleanup) = match stage {
        Some(stage) => match promote_import_stage(&alias, val, stage)? {
            ImportPromotionOutcome::Durable { recovery_cleanup } => (None, recovery_cleanup),
            ImportPromotionOutcome::ProfileCommitIncomplete(incomplete) => (Some(incomplete), None),
        },
        None => {
            write_profile_credentials(&alias, val)?;
            (None, None)
        }
    };
    Ok(ImportSaveOutcome {
        action: SaveAction::Created(alias),
        profile_commit,
        recovery_cleanup,
    })
}

fn classify_import_recovery_after_failure(
    stage: &RotationRecoveryStage,
    cause: anyhow::Error,
) -> (Option<PathBuf>, anyhow::Error) {
    match crate::fs_ops::token_if_present(stage.path()) {
        Ok(Some(observed)) if observed == stage.token => (Some(stage.path().to_path_buf()), cause),
        Ok(_) => (None, cause),
        Err(observation) => (
            None,
            cause.context(format!(
                "the original recovery stage at {} could not be revalidated ({observation:#})",
                stage.path().display()
            )),
        ),
    }
}

fn import_profile_commit_incomplete(
    stage: &RotationRecoveryStage,
    cause: anyhow::Error,
) -> ImportProfileCommitIncomplete {
    let (recovery_path, cause) = classify_import_recovery_after_failure(stage, cause);
    ImportProfileCommitIncomplete {
        recovery_path,
        cause,
    }
}

fn promote_import_stage(
    alias: &str,
    val: &serde_json::Value,
    stage: &RotationRecoveryStage,
) -> Result<ImportPromotionOutcome> {
    validate_alias(alias)?;
    crate::auth::validate_managed_auth_value(val)?;
    let staged: serde_json::Value = serde_json::from_slice(&stage.read_exact_bytes()?)
        .with_context(|| {
            format!(
                "parsing staged rotated credentials {}",
                stage.path().display()
            )
        })?;
    if staged != *val {
        anyhow::bail!(
            "staged rotated credentials {} do not match the validated import",
            stage.path().display()
        );
    }

    let destination = profile_auth_path(alias)?;
    ensure_profile_parent(&destination)?;
    #[cfg(test)]
    run_before_import_promotion_test_hook();
    let creation = crate::fs_ops::create_exclusive_copy(stage.path(), &destination, &stage.token)
        .with_context(|| {
        format!(
            "publishing staged credentials {} to new profile '{}' without replacement",
            stage.path().display(),
            alias
        )
    })?;
    #[cfg(windows)]
    let destination_token = creation.token().clone();
    if matches!(
        creation,
        crate::fs_ops::CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(_)
    ) {
        #[cfg(unix)]
        if let Err(durability) = crate::auth::confirm_namespace_durability(&destination) {
            return Ok(ImportPromotionOutcome::ProfileCommitIncomplete(
                import_profile_commit_incomplete(
                    stage,
                    anyhow::anyhow!(
                        "profile '{alias}' is visible at {}, but its no-clobber publication is not durably confirmed ({durability:#})",
                        destination.display()
                    ),
                ),
            ));
        }
        #[cfg(windows)]
        return Ok(ImportPromotionOutcome::ProfileCommitIncomplete(
            import_profile_commit_incomplete(
                stage,
                anyhow::anyhow!(
                    "Windows no-clobber profile publication at {} returned an unsupported Unix-only durability outcome",
                    destination.display()
                ),
            ),
        ));
    }
    #[cfg(windows)]
    if let Err(hardening) = crate::auth::harden_windows_private_file(&destination) {
        return match crate::auth::remove_bound_path(&destination, &destination_token) {
            Ok(()) => Err(hardening)
                .with_context(|| format!("securing newly-published import profile '{alias}'")),
            Err(cleanup) => Ok(ImportPromotionOutcome::ProfileCommitIncomplete(
                import_profile_commit_incomplete(
                    stage,
                    anyhow::anyhow!(
                        "new import profile '{alias}' could not be secured ({hardening:#}) or exactly removed ({cleanup:#}); its visible state is incomplete"
                    ),
                ),
            )),
        };
    }
    #[cfg(test)]
    run_before_import_recovery_cleanup_test_hook();
    let recovery_cleanup = crate::auth::remove_bound_path(stage.path(), &stage.token)
        .err()
        .map(|cleanup| {
            let (recovery_path, cause) =
                exact_recovery_path_after_cleanup_failure(stage.path(), &stage.token, cleanup);
            ImportRecoveryCleanupIncomplete {
                recovery_path,
                cause: cause.context(format!(
                    "profile '{alias}' was durably published, but exact cleanup of its recovery stage is incomplete"
                )),
            }
        });
    Ok(ImportPromotionOutcome::Durable { recovery_cleanup })
}

pub fn rename_profile(old_alias: &str, new_alias: &str) -> Result<ProfileMutationOutcome> {
    validate_alias(old_alias)?;
    validate_alias(new_alias)?;
    if old_alias == new_alias {
        anyhow::bail!("old and new profile aliases are identical");
    }
    let _leases = acquire_profile_leases(&[old_alias, new_alias])?;
    let _transaction = lock_auth_transaction()?;
    let registry = ProfileRegistry::open()?;
    let old_dir = registry.profile_dir(old_alias)?;
    if !path_exists_checked(&old_dir)? {
        return Err(CsError::NotFound(old_alias.to_string()).into());
    }
    let new_dir = registry.profile_dir(new_alias)?;
    if path_exists_checked(&new_dir)? {
        anyhow::bail!("profile '{new_alias}' already exists");
    }
    read_auth(&registry.auth_path(old_alias)?)?;
    let live_was_old = live_belongs_to_profile_locked(&registry, old_alias)?;
    let current_path = current_file()?;
    let current_snapshot = snapshot_optional_file(&current_path)?;
    let marker_was_old = read_current_checked()?.as_deref() == Some(old_alias);

    let profile_durability_warning = match crate::fs_ops::rename_directory_noreplace_durable(
        &old_dir, &new_dir,
    )
    .with_context(|| {
        format!(
            "renaming profile {} -> {}",
            old_dir.display(),
            new_dir.display()
        )
    })? {
        crate::fs_ops::DirectoryRenameOutcome::DurablyRenamed => None,
        crate::fs_ops::DirectoryRenameOutcome::VisibleDurabilityUnconfirmed { cause } => {
            Some(cause)
        }
    };
    let cache_durability_warning = match crate::cache::rename(old_alias, new_alias) {
        Ok(
            crate::cache::RenameOutcome::Unchanged | crate::cache::RenameOutcome::DurablyRenamed,
        ) => None,
        Ok(crate::cache::RenameOutcome::VisibleDurabilityUnconfirmed { cause }) => Some(cause),
        Err(error) => match crate::fs_ops::rename_directory_noreplace_durable(&new_dir, &old_dir) {
            Ok(crate::fs_ops::DirectoryRenameOutcome::DurablyRenamed) => {
                return Err(error)
                    .context("renaming profile cache state; profile directory rename rolled back");
            }
            Ok(crate::fs_ops::DirectoryRenameOutcome::VisibleDurabilityUnconfirmed { cause }) => {
                anyhow::bail!(
                    "renaming profile cache failed ({error:#}); the profile directory was visibly restored, but rollback durability could not be confirmed ({cause:#})"
                )
            }
            Err(rollback) => anyhow::bail!(
                "renaming profile cache failed ({error:#}) and restoring the profile directory failed ({rollback:#})"
            ),
        },
    };

    let marker_result = if live_was_old {
        write_activation_marker(new_alias)
    } else if marker_was_old {
        repair_stale_current_marker_locked(old_alias)
    } else {
        Ok(())
    };
    if let Err(error) = marker_result {
        let mut rollback_errors = Vec::new();
        let mut rollback_uncertainties = Vec::new();
        match crate::cache::rename(new_alias, old_alias) {
            Ok(
                crate::cache::RenameOutcome::Unchanged
                | crate::cache::RenameOutcome::DurablyRenamed,
            ) => {}
            Ok(crate::cache::RenameOutcome::VisibleDurabilityUnconfirmed { cause }) => {
                rollback_uncertainties.push(format!("cache directory durability: {cause:#}"));
            }
            Err(rollback) => rollback_errors.push(format!("cache: {rollback:#}")),
        }
        match crate::fs_ops::rename_directory_noreplace_durable(&new_dir, &old_dir) {
            Ok(crate::fs_ops::DirectoryRenameOutcome::DurablyRenamed) => {}
            Ok(crate::fs_ops::DirectoryRenameOutcome::VisibleDurabilityUnconfirmed { cause }) => {
                rollback_uncertainties.push(format!("profile directory durability: {cause:#}"));
            }
            Err(rollback) => {
                rollback_errors.push(format!("profile directory: {rollback:#}"));
            }
        }
        if let Err(rollback) = restore_file_snapshot(&current_path, current_snapshot.as_deref()) {
            rollback_errors.push(format!("current marker: {rollback:#}"));
        }
        if rollback_errors.is_empty() && rollback_uncertainties.is_empty() {
            return Err(error)
                .context("updating current marker after profile rename; rename rolled back");
        }
        if rollback_errors.is_empty() {
            anyhow::bail!(
                "updating current marker after profile rename failed ({error:#}); the rename was visibly rolled back, but rollback durability could not be confirmed: {}",
                rollback_uncertainties.join("; ")
            );
        }
        rollback_errors.extend(rollback_uncertainties);
        anyhow::bail!(
            "updating current marker after profile rename failed ({error:#}); rollback also failed: {}",
            rollback_errors.join("; ")
        );
    }
    let mut durability_warnings = Vec::new();
    if let Some(cause) = cache_durability_warning {
        durability_warnings.push(anyhow::anyhow!(
            "profile cache was renamed from '{old_alias}' to '{new_alias}', but directory durability could not be confirmed: {cause:#}"
        ));
    }
    if let Some(cause) = profile_durability_warning {
        durability_warnings.push(anyhow::anyhow!(
            "profile was visibly renamed from '{old_alias}' to '{new_alias}', but directory durability could not be confirmed: {cause:#}"
        ));
    }
    Ok(ProfileMutationOutcome::committed_with_warnings(
        durability_warnings,
    ))
}

pub fn save_auth_value(val: serde_json::Value, hint_alias: Option<&str>) -> Result<SaveAction> {
    let identity = extract_identity(&val);
    if identity.account_id.is_none() || identity.email.is_none() {
        anyhow::bail!(
            "login credentials must contain both a non-empty account_id and email before a profile can be saved"
        );
    }
    crate::auth::validate_managed_auth_value(&val)?;
    let mut plan = plan_profile_write(&identity, hint_alias)?;
    for _ in 0..PROFILE_CONCURRENCY_RETRY_LIMIT {
        let lease = acquire_profile_lease(&plan.alias)?;
        let _transaction = lock_auth_transaction()?;
        let live_snapshot = snapshot_optional_file(&codex_auth_path()?)?;
        let confirmed = plan_profile_write(&identity, hint_alias)?;
        if confirmed.alias != lease.alias() {
            plan = confirmed;
            continue;
        }

        let profile_dst = profile_auth_path(&confirmed.alias)?;
        if confirmed.updated {
            let existing = read_existing_auth(&profile_dst)?.ok_or_else(|| {
                anyhow::anyhow!("profile '{}' disappeared during login", confirmed.alias)
            })?;
            // Re-login intentionally replaces a potentially unstamped token,
            // but never relaxes account identity.
            ensure_same_account_identity(&confirmed.alias, &existing, &val)?;
        }
        ensure_profile_parent(&profile_dst)?;
        write_profile_auth_durably(&confirmed.alias, &profile_dst, &val)?;
        commit_fresh_credentials_activation(&confirmed.alias, &val, live_snapshot.as_deref())?;
        return Ok(if confirmed.updated {
            SaveAction::Updated(confirmed.alias)
        } else {
            SaveAction::Created(confirmed.alias)
        });
    }
    anyhow::bail!("profile destination kept changing during login; retry the command")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::MutexGuard;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::Result;
    use fs4::FileExt;

    use super::{cmd_delete, cmd_save, cmd_use, rename_profile, switch_profile, validate_alias};

    fn write_auth_durable(path: &Path, value: &serde_json::Value) {
        crate::auth::write_auth(path, value)
            .unwrap()
            .assert_durably_published();
    }

    fn current_alias() -> String {
        super::read_current_checked()
            .expect("read current profile marker")
            .expect("current profile marker exists")
    }

    fn recovery_files() -> Vec<PathBuf> {
        let recovery = crate::auth::app_home().unwrap().join("recovery");
        match std::fs::read_dir(recovery) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("reading recovery directory failed: {error}"),
        }
    }

    struct TestEnv {
        _lock: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        old_home: Option<OsString>,
        old_codex_home: Option<OsString>,
        old_app_home: Option<OsString>,
    }

    struct ThreadCleanup<G> {
        blocker: Option<G>,
        workers: Vec<JoinHandle<()>>,
    }

    impl<G> ThreadCleanup<G> {
        fn new(blocker: G) -> Self {
            Self {
                blocker: Some(blocker),
                workers: Vec::new(),
            }
        }

        fn push(&mut self, worker: JoinHandle<()>) {
            self.workers.push(worker);
        }

        fn release_blocker(&mut self) {
            self.blocker.take();
        }

        fn join_all(&mut self) {
            let mut first_panic = None;
            for worker in self.workers.drain(..) {
                if let Err(panic) = worker.join()
                    && first_panic.is_none()
                {
                    first_panic = Some(panic);
                }
            }
            if let Some(panic) = first_panic {
                std::panic::resume_unwind(panic);
            }
        }
    }

    impl<G> Drop for ThreadCleanup<G> {
        fn drop(&mut self) {
            self.blocker.take();
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
    }

    impl TestEnv {
        fn new() -> Self {
            crate::config::init_defaults_for_tests();
            let lock = super::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = crate::fs_ops::create_direct_tempdir().unwrap();
            let codex_home = home.path().join(".codex");
            let app_home = home.path().join(".codex-switch");
            let old_home = std::env::var_os("HOME");
            let old_codex_home = std::env::var_os("CODEX_HOME");
            let old_app_home = std::env::var_os("CODEX_SWITCH_HOME");

            unsafe {
                std::env::set_var("HOME", home.path());
                std::env::set_var("CODEX_HOME", &codex_home);
                std::env::set_var("CODEX_SWITCH_HOME", &app_home);
            }

            Self {
                _lock: lock,
                _home: home,
                old_home,
                old_codex_home,
                old_app_home,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.old_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.old_codex_home {
                    Some(value) => std::env::set_var("CODEX_HOME", value),
                    None => std::env::remove_var("CODEX_HOME"),
                }
                match &self.old_app_home {
                    Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
                }
            }
        }
    }

    fn assert_invalid_alias<T: std::fmt::Debug>(result: Result<T>, expected_message: &str) {
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), expected_message);
    }

    #[test]
    fn validate_alias_accepts_expected_values() {
        assert!(validate_alias("alpha-123_.beta").is_ok());
        assert!(validate_alias(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn validate_alias_rejects_reserved_or_empty_values() {
        assert!(validate_alias("").is_err());
        assert!(validate_alias(".").is_err());
        assert!(validate_alias("..").is_err());
    }

    #[test]
    fn validate_alias_rejects_separators_and_non_ascii() {
        assert!(validate_alias("../escape").is_err());
        assert!(validate_alias("with/slash").is_err());
        assert!(validate_alias("\u{4E2D}\u{6587}").is_err());
        assert!(validate_alias(&"a".repeat(65)).is_err());
    }

    #[test]
    fn profile_commands_reject_invalid_alias_inputs() {
        let _env = TestEnv::new();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                cmd_use(alias, true),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                switch_profile(alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                cmd_delete(alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                rename_profile(alias, "valid-alias"),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(cmd_use("", true), "alias cannot be empty");
        assert_invalid_alias(switch_profile(""), "alias cannot be empty");
        assert_invalid_alias(cmd_delete(""), "alias cannot be empty");
        assert_invalid_alias(rename_profile("", "valid-alias"), "alias cannot be empty");
    }

    #[test]
    fn rename_profile_rejects_invalid_new_alias() {
        let _env = TestEnv::new();
        let old_dir = super::profiles_dir().unwrap().join("valid-alias");
        std::fs::create_dir_all(&old_dir).unwrap();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                rename_profile("valid-alias", alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(rename_profile("valid-alias", ""), "alias cannot be empty");
    }

    #[test]
    fn rename_keeps_profile_and_visible_cache_alias_in_the_same_generation() {
        let _env = TestEnv::new();
        let old_auth = super::profile_auth_path("old").unwrap();
        std::fs::create_dir_all(old_auth.parent().unwrap()).unwrap();
        write_auth_durable(&old_auth, &serde_json::json!({}));

        let cache_path = crate::auth::app_home().unwrap().join("cache.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &cache_path,
            &serde_json::json!({
                "entries": {},
                "last_used": {"old": 7}
            }),
        );
        crate::auth::fail_next_private_durability_confirmation();

        let outcome = rename_profile("old", "new")
            .expect("a visibly-published cache rename must commit with the profile directory");
        assert!(
            outcome.durability_warning().is_some(),
            "the committed cache rename must retain its durability warning"
        );

        let profiles = super::profiles_dir().unwrap();
        assert!(!profiles.join("old").exists());
        assert!(profiles.join("new").exists());
        let cache: serde_json::Value =
            serde_json::from_slice(&std::fs::read(cache_path).unwrap()).unwrap();
        assert_eq!(cache.pointer("/last_used/new"), Some(&serde_json::json!(7)));
        assert_eq!(cache.pointer("/last_used/old"), None);
    }

    #[test]
    fn rename_reports_visible_reverse_cache_when_marker_rollback_is_not_durable() {
        let _env = TestEnv::new();
        let auth = realistic_auth_json("old@example.com", "acct-old", "access", "refresh");
        seed_profile("old", &auth);
        write_auth_durable(&crate::auth::codex_auth_path().unwrap(), &auth);
        super::write_current("old").unwrap();

        let cache_path = crate::auth::app_home().unwrap().join("cache.json");
        write_auth_durable(
            &cache_path,
            &serde_json::json!({
                "entries": {},
                "last_used": {"old": 7}
            }),
        );

        crate::auth::fail_private_durability_confirmation_after(1);
        super::fail_next_activation_marker_write();
        let error = rename_profile("old", "new")
            .expect_err("a marker failure must report uncertain reverse-cache durability");
        let detail = format!("{error:#}");
        assert!(detail.contains("visibly rolled back"), "{detail}");
        assert!(
            detail.contains("rollback durability could not be confirmed"),
            "{detail}"
        );

        let profiles = super::profiles_dir().unwrap();
        assert!(profiles.join("old").exists());
        assert!(!profiles.join("new").exists());
        assert_eq!(
            super::read_current_checked().unwrap().as_deref(),
            Some("old")
        );
        let cache: serde_json::Value =
            serde_json::from_slice(&std::fs::read(cache_path).unwrap()).unwrap();
        assert_eq!(cache.pointer("/last_used/old"), Some(&serde_json::json!(7)));
        assert_eq!(cache.pointer("/last_used/new"), None);
    }

    #[test]
    fn switch_profile_waits_for_auth_lock() {
        let _env = TestEnv::new();

        let live = crate::auth::codex_auth_path().unwrap();
        let current =
            realistic_auth_json("current@example.com", "acct_current", "acc_old", "ref_old");
        seed_profile("current-profile", &current);
        write_auth_durable(&live, &current);

        let next = realistic_auth_json("next@example.com", "acct_next", "acc_new", "ref_new");
        let profile_path = super::profile_auth_path("next-profile").unwrap();
        super::ensure_profile_parent(&profile_path).unwrap();
        write_auth_durable(&profile_path, &next);

        let lock_path = super::auth_lock_path().unwrap();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        FileExt::lock(&lock_file).unwrap();

        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("auth", attempt_tx);
            let _ = done_tx.send(super::switch_profile("next-profile"));
        });
        let mut cleanup = ThreadCleanup::new(lock_file);
        cleanup.push(handle);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not reach auth lock attempt");
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "switch should block while auth lock is held"
        );
        assert_eq!(
            crate::auth::read_auth(&live)
                .unwrap()
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("acc_old")
        );

        cleanup.release_blocker();

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not finish after auth lock release")
            .unwrap();
        cleanup.join_all();
        assert_eq!(
            crate::auth::read_auth(&live)
                .unwrap()
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("acc_new")
        );
        assert_eq!(current_alias(), "next-profile");
    }

    #[test]
    fn prepared_switch_skips_registry_scan_for_the_exact_current_profile() {
        let _env = TestEnv::new();
        let current = realistic_auth_json(
            "current@example.com",
            "acct_current",
            "current",
            "current-ref",
        );
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        seed_profile("current", &current);
        seed_profile("target", &target);
        super::switch_profile("current").unwrap();
        super::reset_profile_registry_snapshot_count();

        let prepared = super::prepare_profile_switch("target").unwrap();

        assert!(!prepared.requires_confirmation());
        assert_eq!(
            super::profile_registry_snapshot_count(),
            0,
            "an exact binding to the distinct current alias must avoid a full registry snapshot"
        );
    }

    #[test]
    fn prepared_switch_does_not_use_target_bytes_for_a_different_current_marker() {
        let _env = TestEnv::new();
        let marker_profile =
            realistic_auth_json("marker@example.com", "acct_marker", "marker", "marker-ref");
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        seed_profile("marker-profile", &marker_profile);
        seed_profile("target", &target);
        write_live(&target);
        super::write_current("marker-profile").unwrap();
        super::reset_profile_registry_snapshot_count();

        let prepared = super::prepare_profile_switch("target").unwrap();

        assert!(!prepared.requires_confirmation());
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "the exact target is not evidence for a marker that names another profile"
        );
    }

    #[test]
    fn prepared_switch_preserves_untracked_live_auth_confirmation() {
        let _env = TestEnv::new();
        let current = realistic_auth_json(
            "current@example.com",
            "acct_current",
            "current",
            "current-ref",
        );
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        let untracked = realistic_auth_json(
            "untracked@example.com",
            "acct_untracked",
            "untracked",
            "untracked-ref",
        );
        seed_profile("current", &current);
        seed_profile("target", &target);
        super::switch_profile("current").unwrap();
        write_live(&untracked);
        super::reset_profile_registry_snapshot_count();

        let prepared = super::prepare_profile_switch("target").unwrap();

        assert!(prepared.requires_confirmation());
        assert_eq!(super::profile_registry_snapshot_count(), 1);
    }

    #[test]
    fn marker_failure_leaves_new_live_active_and_previous_marker_unchanged() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        seed_profile("alice", &alice);
        seed_profile("bob", &bob);
        super::switch_profile("alice").unwrap();

        super::fail_next_activation_marker_write();
        let error =
            super::switch_profile("bob").expect_err("a marker failure must fail the normal switch");
        assert!(format!("{error:#}").contains("injected activation marker failure"));
        assert!(
            format!("{error:#}").contains("live credentials were not rolled back"),
            "{error:#}"
        );
        assert_eq!(current_alias(), "alice");
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/access_token")
                .and_then(|value| value.as_str()),
            Some("b-old")
        );
    }

    #[test]
    fn publication_boundary_preserves_newer_live_auth_and_existing_marker() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        let external =
            realistic_auth_json("new@example.com", "acct_new", "new-access", "new-refresh");
        seed_profile("alice", &alice);
        seed_profile("bob", &bob);
        super::switch_profile("alice").unwrap();

        let external_for_hook = external.clone();
        super::before_next_activation_live_publish(move || {
            write_live(&external_for_hook);
        });
        let error = super::switch_profile("bob")
            .expect_err("a newer live credential must win at the publication boundary");

        assert!(format!("{error:#}").contains("live auth changed"));
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            external
        );
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn prepared_switch_refuses_to_overwrite_live_auth_that_changed_after_authorization() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        let external =
            realistic_auth_json("new@example.com", "acct_new", "new-access", "new-refresh");
        seed_profile("alice", &alice);
        seed_profile("bob", &bob);
        super::switch_profile("alice").unwrap();

        let prepared = super::prepare_profile_switch("bob").unwrap();
        assert!(!prepared.requires_confirmation());
        write_live(&external);

        let confirmed = super::confirm_prepared_profile_switch_without_overwrite(prepared).unwrap();
        let error = super::commit_confirmed_profile_switch(confirmed)
            .expect_err("authorization must be bound to the observed live bytes");
        assert!(error.to_string().contains("live auth changed"), "{error:#}");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            external
        );
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn prepared_switch_refuses_a_target_replaced_after_authorization() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        let replacement = realistic_auth_json("mallory@example.com", "acct_m", "m-new", "m-ref");
        seed_profile("alice", &alice);
        seed_profile("target", &bob);
        super::switch_profile("alice").unwrap();

        let prepared = super::prepare_profile_switch("target").unwrap();
        let target_path = super::profile_auth_path("target").unwrap();
        std::fs::remove_file(&target_path).unwrap();
        write_auth_durable(&target_path, &replacement);

        let confirmed = super::confirm_prepared_profile_switch_without_overwrite(prepared).unwrap();
        let error = super::commit_confirmed_profile_switch(confirmed)
            .expect_err("confirmation must not authorize a replacement target credential");
        assert!(
            error.to_string().contains("profile 'target' changed"),
            "{error:#}"
        );
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            alice
        );
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn confirmation_wait_does_not_hold_the_target_profile_lease() {
        let _env = TestEnv::new();
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        let untracked = realistic_auth_json(
            "untracked@example.com",
            "acct_untracked",
            "untracked",
            "untracked-ref",
        );
        seed_profile("target", &target);
        write_live(&untracked);

        let (prompt_started_tx, prompt_started_rx) = std::sync::mpsc::channel();
        let (release_prompt_tx, release_prompt_rx) = std::sync::mpsc::channel();
        let (confirmed_tx, confirmed_rx) = std::sync::mpsc::channel();
        let confirmation_worker = std::thread::spawn(move || {
            let result = super::prepare_and_confirm_profile_switch_with("target", true, || {
                prompt_started_tx.send(()).unwrap();
                release_prompt_rx.recv().unwrap();
                Ok(true)
            })
            .map(|confirmed| confirmed.alias().to_string());
            let _ = confirmed_tx.send(result);
        });

        prompt_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("confirmation did not reach the injected prompt");
        let (lease_attempt_tx, lease_attempt_rx) = std::sync::mpsc::channel();
        let (lease_result_tx, lease_result_rx) = std::sync::mpsc::channel();
        let lease_worker = std::thread::spawn(move || {
            lease_attempt_tx.send(()).unwrap();
            let result = super::acquire_profile_lease("target").map(drop);
            let _ = lease_result_tx.send(result);
        });
        lease_attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("same-alias lease worker did not start");
        let lease_result = lease_result_rx.recv_timeout(Duration::from_secs(2));

        release_prompt_tx.send(()).unwrap();
        let confirmed_alias = confirmed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("confirmation did not finish after approval")
            .unwrap();
        confirmation_worker.join().unwrap();
        lease_worker.join().unwrap();

        assert_eq!(confirmed_alias, "target");
        assert!(
            matches!(&lease_result, Ok(Ok(()))),
            "same-alias lease was blocked while confirmation waited: {lease_result:?}"
        );
    }

    #[test]
    fn approved_switch_rejects_target_change_before_irreversible_side_effect() {
        let _env = TestEnv::new();
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        let replacement = realistic_auth_json(
            "target@example.com",
            "acct_target",
            "replacement",
            "replacement-ref",
        );
        let untracked = realistic_auth_json(
            "untracked@example.com",
            "acct_untracked",
            "untracked",
            "untracked-ref",
        );
        seed_profile("target", &target);
        write_live(&untracked);

        let confirmed =
            super::prepare_and_confirm_profile_switch_with("target", true, || Ok(true)).unwrap();
        let target_path = super::profile_auth_path("target").unwrap();
        write_auth_durable(&target_path, &replacement);
        let lease = super::acquire_profile_lease("target").unwrap();
        let error =
            match super::authorize_confirmed_profile_switch_before_side_effect(confirmed, lease) {
                Ok(_) => panic!("changed target bytes must not authorize reset-card redemption"),
                Err(error) => error,
            };

        assert!(
            error.to_string().contains("profile 'target' changed"),
            "{error:#}"
        );
        assert_eq!(crate::auth::read_auth(&target_path).unwrap(), replacement);
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            untracked
        );
        assert!(super::read_current_checked().unwrap().is_none());
    }

    #[test]
    fn approved_switch_rejects_live_change_before_irreversible_side_effect() {
        let _env = TestEnv::new();
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        let untracked = realistic_auth_json(
            "untracked@example.com",
            "acct_untracked",
            "untracked",
            "untracked-ref",
        );
        let changed_live = realistic_auth_json(
            "changed@example.com",
            "acct_changed",
            "changed",
            "changed-ref",
        );
        seed_profile("target", &target);
        write_live(&untracked);

        let confirmed =
            super::prepare_and_confirm_profile_switch_with("target", true, || Ok(true)).unwrap();
        write_live(&changed_live);
        let lease = super::acquire_profile_lease("target").unwrap();
        let error =
            match super::authorize_confirmed_profile_switch_before_side_effect(confirmed, lease) {
                Ok(_) => panic!("changed live bytes must not authorize reset-card redemption"),
                Err(error) => error,
            };

        assert!(error.to_string().contains("live auth changed"), "{error:#}");
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("target").unwrap()).unwrap(),
            target
        );
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            changed_live
        );
        assert!(super::read_current_checked().unwrap().is_none());
    }

    #[test]
    fn unchecked_switch_refuses_to_overwrite_untracked_live_auth() {
        let _env = TestEnv::new();
        let target =
            realistic_auth_json("target@example.com", "acct_target", "target", "target-ref");
        let untracked = realistic_auth_json(
            "untracked@example.com",
            "acct_untracked",
            "untracked",
            "untracked-ref",
        );
        seed_profile("target", &target);
        write_live(&untracked);

        let error = super::switch_profile("target")
            .expect_err("non-interactive callers cannot authorize destructive overwrite");
        assert!(
            error.to_string().contains("explicit confirmation"),
            "{error:#}"
        );
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            untracked
        );
    }

    #[test]
    fn rotated_credentials_are_not_rolled_back_when_marker_write_fails() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();

        super::fail_next_activation_marker_write();
        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
        )
        .expect("the profile write itself succeeds");
        let super::RefreshTokenUpdate::SavedWithCommitIncomplete {
            recovery_path,
            cause,
        } = update
        else {
            panic!("the injected marker failure must be reported as a partial activation")
        };
        assert!(
            recovery_path.is_none(),
            "a durable profile no longer needs a duplicate recovery stage"
        );
        let detail = format!("{cause:#}");
        assert!(detail.contains("was published to live auth"), "{detail}");
        assert!(detail.contains("activation marker failed"), "{detail}");

        for path in [
            super::profile_auth_path("alice").unwrap(),
            crate::auth::codex_auth_path().unwrap(),
        ] {
            let value = crate::auth::read_auth(&path).unwrap();
            assert_eq!(
                value
                    .pointer("/tokens/refresh_token")
                    .and_then(|value| value.as_str()),
                Some("a-ref-new"),
                "new refresh token must remain durable at {}",
                path.display()
            );
        }
        assert_eq!(current_alias(), "alice");
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn visible_rotated_credentials_are_not_reported_saved_when_durability_is_unconfirmed() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);

        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        crate::auth::fail_next_private_durability_confirmation();

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
        )
        .expect("publication itself is visible and must use a partial outcome");
        let super::RefreshTokenUpdate::SavedWithCommitIncomplete {
            recovery_path,
            cause,
        } = update
        else {
            panic!("unconfirmed directory durability must never be reported as Saved")
        };
        let detail = format!("{cause:#}");
        assert!(detail.contains("is visible"), "{detail}");
        assert!(detail.contains("durability"), "{detail}");
        assert_eq!(
            profile_refresh_token("alice"),
            "a-ref-new",
            "the visible rotated credential must not be mistaken for an unwritten value"
        );
        let recovery_path = recovery_path
            .expect("unconfirmed profile durability must retain the exact recovery stage");
        assert_eq!(
            crate::auth::read_auth(&recovery_path).unwrap()["tokens"]["refresh_token"],
            "a-ref-new"
        );
    }

    #[test]
    fn cleanup_failure_never_reports_a_replacement_as_the_recovery_stage() {
        let _env = TestEnv::new();
        let recovery_dir = crate::auth::app_home().unwrap().join("recovery");
        std::fs::create_dir_all(&recovery_dir).unwrap();
        let stage_path = recovery_dir.join("owned-stage.json");
        std::fs::write(&stage_path, b"owned-stage").unwrap();
        let stage_token = crate::fs_ops::token_for_path(&stage_path).unwrap();

        std::fs::remove_file(&stage_path).unwrap();
        std::fs::write(&stage_path, b"foreign-replacement").unwrap();
        let cleanup = crate::auth::remove_bound_path(&stage_path, &stage_token)
            .expect_err("exact cleanup must refuse a different file identity");
        let (reported_path, _) =
            super::exact_recovery_path_after_cleanup_failure(&stage_path, &stage_token, cleanup);

        assert!(reported_path.is_none());
        assert_eq!(
            std::fs::read(&stage_path).unwrap(),
            b"foreign-replacement",
            "the replacement must not be removed or described as the rotated credential"
        );
    }

    #[test]
    fn displaced_cleanup_does_not_claim_an_unlinked_stage_was_preserved() {
        let _env = TestEnv::new();
        let recovery_dir = crate::auth::app_home().unwrap().join("recovery");
        std::fs::create_dir_all(&recovery_dir).unwrap();
        let displaced = recovery_dir.join("displaced-stage.json");
        std::fs::write(&displaced, b"previous-stage").unwrap();
        let displaced_token = crate::fs_ops::token_for_path(&displaced).unwrap();
        std::fs::remove_file(&displaced).unwrap();

        let error = super::displaced_stage_cleanup_incomplete(
            &displaced,
            &displaced_token,
            anyhow::anyhow!("parent-directory sync failed"),
        );
        let detail = format!("{error:#}");
        assert!(detail.contains("is no longer present"), "{detail}");
        assert!(
            detail.contains("removal durability was not confirmed"),
            "{detail}"
        );
        assert!(!detail.contains("both were preserved"), "{detail}");
    }

    #[test]
    fn committed_refresh_reports_cleanup_only_failure_without_claiming_a_foreign_path() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let mut foreign_path = None;

        let update = super::update_profile_tokens_if_refresh_matches_after_lock(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
            super::RefreshCommitHooks {
                after_lock: || {},
                before_cleanup: || {
                    let files = recovery_files();
                    assert_eq!(files.len(), 1);
                    std::fs::remove_file(&files[0]).unwrap();
                    std::fs::write(&files[0], b"foreign-replacement").unwrap();
                    foreign_path = Some(files[0].clone());
                },
            },
        )
        .unwrap();

        let super::RefreshTokenUpdate::SavedWithCleanupIncomplete {
            recovery_path,
            cause,
        } = update
        else {
            panic!("only exact recovery cleanup should be incomplete")
        };
        assert!(recovery_path.is_none());
        assert!(format!("{cause:#}").contains("credential commit completed"));
        assert_eq!(profile_refresh_token("alice"), "a-ref-new");
        let foreign_path = foreign_path.unwrap();
        assert_eq!(
            std::fs::read(&foreign_path).unwrap(),
            b"foreign-replacement"
        );
    }

    #[test]
    fn rotated_profile_is_saved_without_overwriting_live_changed_after_authorization() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();

        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let foreign = realistic_auth_json(
            "foreign@example.com",
            "acct_foreign",
            "foreign-access",
            "foreign-refresh",
        );
        write_live(&foreign);

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
        )
        .expect("the rotated credential must remain durable in the profile");
        let super::RefreshTokenUpdate::SavedWithCommitIncomplete { cause, .. } = update else {
            panic!("the changed live snapshot must prevent activation")
        };
        assert!(format!("{cause:#}").contains("live auth changed"));
        assert_eq!(profile_refresh_token("alice"), "a-ref-new");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            foreign,
            "the post-authorization live writer must not be overwritten"
        );
    }

    #[test]
    fn refreshing_an_inactive_legacy_identity_duplicate_does_not_replace_live_auth() {
        let _env = TestEnv::new();
        let active = realistic_auth_json("same@example.com", "acct_same", "active", "active-ref");
        let inactive =
            realistic_auth_json("same@example.com", "acct_same", "inactive", "inactive-ref");
        seed_profile("active", &active);
        seed_profile("inactive", &inactive);
        super::switch_profile("active").unwrap();

        let lease = super::acquire_profile_lease("inactive").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let result = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "inactive-ref",
            &make_jwt("same@example.com", "acct_same"),
            "inactive-new",
            "inactive-ref-new",
        )
        .unwrap();

        assert!(matches!(result, super::RefreshTokenUpdate::Saved));
        assert_eq!(current_alias(), "active");
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("active")
        );
        assert_eq!(profile_refresh_token("inactive"), "inactive-ref-new");
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn refresh_updates_live_when_a_legacy_switch_activates_the_profile_in_flight() {
        let _env = TestEnv::new();
        let active =
            realistic_auth_json("active@example.com", "acct_active", "active", "active-ref");
        let inactive = realistic_auth_json(
            "inactive@example.com",
            "acct_inactive",
            "inactive",
            "inactive-ref",
        );
        seed_profile("active", &active);
        seed_profile("inactive", &inactive);
        super::switch_profile("active").unwrap();

        let lease = super::acquire_profile_lease("inactive").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        write_live(&inactive);
        std::fs::write(super::current_file().unwrap(), b"inactive\n").unwrap();

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "inactive-ref",
            &make_jwt("inactive@example.com", "acct_inactive"),
            "inactive-new",
            "inactive-ref-new",
        )
        .unwrap();

        assert!(matches!(update, super::RefreshTokenUpdate::Saved));
        assert_eq!(profile_refresh_token("inactive"), "inactive-ref-new");
        assert_eq!(current_alias(), "inactive");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap()["tokens"]["refresh_token"],
            "inactive-ref-new"
        );
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn refresh_updates_live_when_its_missing_active_marker_is_repaired_in_flight() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();
        std::fs::remove_file(super::current_file().unwrap()).unwrap();

        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        super::write_current("alice").unwrap();

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
        )
        .unwrap();

        assert!(matches!(update, super::RefreshTokenUpdate::Saved));
        assert_eq!(current_alias(), "alice");
        assert_eq!(profile_refresh_token("alice"), "a-ref-new");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap()["tokens"]["refresh_token"],
            "a-ref-new"
        );
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn refresh_does_not_overwrite_nonexact_live_when_profile_becomes_active_in_flight() {
        let _env = TestEnv::new();
        let active =
            realistic_auth_json("active@example.com", "acct_active", "active", "active-ref");
        let inactive = realistic_auth_json(
            "inactive@example.com",
            "acct_inactive",
            "inactive",
            "inactive-ref",
        );
        let foreign = realistic_auth_json(
            "foreign@example.com",
            "acct_foreign",
            "foreign",
            "foreign-ref",
        );
        seed_profile("active", &active);
        seed_profile("inactive", &inactive);
        super::switch_profile("active").unwrap();

        let lease = super::acquire_profile_lease("inactive").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        write_live(&foreign);
        std::fs::write(super::current_file().unwrap(), b"inactive\n").unwrap();

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "inactive-ref",
            &make_jwt("inactive@example.com", "acct_inactive"),
            "inactive-new",
            "inactive-ref-new",
        )
        .unwrap();

        let super::RefreshTokenUpdate::SavedWithCommitIncomplete {
            recovery_path,
            cause,
        } = update
        else {
            panic!("a foreign live credential must be preserved and reported")
        };
        assert!(recovery_path.is_none());
        assert!(format!("{cause:#}").contains("not the exact pre-refresh credential"));
        assert_eq!(profile_refresh_token("inactive"), "inactive-ref-new");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            foreign
        );
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn refresh_reports_marker_read_failure_after_saving_the_rotated_profile() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();

        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let marker = super::current_file().unwrap();
        std::fs::remove_file(&marker).unwrap();
        std::fs::create_dir(&marker).unwrap();

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
        )
        .unwrap();

        let super::RefreshTokenUpdate::SavedWithCommitIncomplete {
            recovery_path,
            cause,
        } = update
        else {
            panic!("marker read failure must be reported as a partial live activation")
        };
        assert!(recovery_path.is_none());
        assert!(format!("{cause:#}").contains("active marker could not be revalidated"));
        assert_eq!(profile_refresh_token("alice"), "a-ref-new");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            alice
        );
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn current_marker_keeps_active_binding_when_duplicate_has_the_old_bytes() {
        let _env = TestEnv::new();
        let old = realistic_auth_json("same@example.com", "acct_same", "old", "old-ref");
        seed_profile("active", &old);
        seed_profile("duplicate", &old);
        super::switch_profile("active").unwrap();

        let lease = super::acquire_profile_lease("active").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let result = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "old-ref",
            &make_jwt("same@example.com", "acct_same"),
            "new",
            "new-ref",
        )
        .unwrap();

        assert!(matches!(result, super::RefreshTokenUpdate::Saved));
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/refresh_token")
                .and_then(|v| v.as_str()),
            Some("new-ref")
        );
        assert_eq!(current_alias(), "active");
        assert_eq!(profile_refresh_token("duplicate"), "old-ref");
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn refresh_does_not_undo_a_later_switch_between_exact_duplicate_aliases() {
        let _env = TestEnv::new();
        let old = realistic_auth_json("same@example.com", "acct_same", "old", "old-ref");
        seed_profile("refreshing", &old);
        seed_profile("selected", &old);
        super::switch_profile("refreshing").unwrap();

        let lease = super::acquire_profile_lease("refreshing").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        super::switch_profile("selected").unwrap();
        assert_eq!(current_alias(), "selected");

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "old-ref",
            &make_jwt("same@example.com", "acct_same"),
            "new",
            "new-ref",
        )
        .unwrap();

        assert!(matches!(update, super::RefreshTokenUpdate::Saved));
        assert_eq!(current_alias(), "selected");
        assert_eq!(profile_refresh_token("refreshing"), "new-ref");
        assert_eq!(profile_refresh_token("selected"), "old-ref");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            old,
            "the later explicit selection owns live auth even when both old credential files were byte-identical"
        );
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn semantic_live_match_precedes_ambiguous_legacy_identity_matching() {
        let _env = TestEnv::new();
        let matching =
            realistic_auth_json("same@example.com", "acct_same", "current", "current-ref");
        let stale = realistic_auth_json("same@example.com", "acct_same", "stale", "stale-ref");
        seed_profile("matching", &matching);
        seed_profile("stale", &stale);
        let live_path = crate::auth::codex_auth_path().unwrap();
        std::fs::create_dir_all(live_path.parent().unwrap()).unwrap();
        std::fs::write(&live_path, serde_json::to_vec(&matching).unwrap()).unwrap();
        super::write_current("stale").unwrap();

        assert_eq!(
            super::active_profile_from_live().unwrap().as_deref(),
            Some("matching")
        );
    }

    #[test]
    fn ambiguous_exact_live_duplicates_are_reported_instead_of_becoming_no_change() {
        let _env = TestEnv::new();
        let exact = realistic_auth_json("same@example.com", "acct_same", "exact", "exact-ref");
        let stale = realistic_auth_json("same@example.com", "acct_same", "stale", "stale-ref");
        seed_profile("first", &exact);
        seed_profile("second", &exact);
        seed_profile("stale", &stale);
        write_live(&exact);
        super::write_current("stale").unwrap();

        let active_error = super::active_profile_from_live()
            .expect_err("ambiguous exact bindings must remain an explicit error");
        assert!(
            format!("{active_error:#}").contains("current marker does not disambiguate"),
            "{active_error:#}"
        );
        let error = super::detect_auth_change()
            .expect_err("an unrelated marker cannot silently resolve duplicate exact profiles");
        assert!(
            format!("{error:#}").contains("current marker does not disambiguate"),
            "{error:#}"
        );
        assert_eq!(current_alias(), "stale");
    }

    #[test]
    fn active_profile_propagates_an_unreadable_live_auth_model() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, b"{").unwrap();

        let error = super::active_profile_from_live()
            .expect_err("malformed live auth must not become an inactive-account result");

        assert!(
            format!("{error:#}").contains("parsing live auth"),
            "{error:#}"
        );
    }

    #[test]
    fn exact_duplicate_detection_does_not_undo_a_concurrent_marker_switch() {
        let _env = TestEnv::new();
        let exact = realistic_auth_json("same@example.com", "acct_same", "exact", "exact-ref");
        seed_profile("first", &exact);
        seed_profile("second", &exact);
        super::switch_profile("first").unwrap();
        super::after_next_exact_live_binding(|| {
            super::switch_profile("second").unwrap();
        });
        super::reset_profile_registry_snapshot_count();

        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        ));
        assert_eq!(current_alias(), "second");
        assert_eq!(
            super::profile_registry_snapshot_count(),
            0,
            "a raced exact marker must be retried from live state without a registry scan"
        );
        assert_eq!(
            super::active_profile_from_live().unwrap().as_deref(),
            Some("second")
        );
    }

    #[test]
    fn valid_refresh_material_is_staged_before_local_credential_transformation() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();
        let profile_path = super::profile_auth_path("alice").unwrap();
        let live_path = crate::auth::codex_auth_path().unwrap();
        let profile_before = std::fs::read(&profile_path).unwrap();
        let live_before = std::fs::read(&live_path).unwrap();

        let lease = super::acquire_profile_lease("alice").unwrap();
        let mut authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        authorization.expected_profile["tokens"] = serde_json::json!("invalid-local-shape");
        let id_token = make_jwt("alice@example.com", "acct_a");
        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &id_token,
            "a-new",
            "a-ref-new",
        )
        .expect("the returned successor must survive a local transformation failure");

        let super::RefreshTokenUpdate::RecoveryPreserved { path, cause } = update else {
            panic!("local transformation failure must preserve raw rotation material")
        };
        assert!(
            format!("{cause:#}").contains("local credential transformation failed"),
            "{cause:#}"
        );
        let recovery = crate::auth::read_auth(&path).unwrap();
        assert_eq!(
            recovery["recovery_kind"],
            "validated_token_refresh_response"
        );
        assert_eq!(recovery["profile_alias"], "alice");
        assert_eq!(recovery["id_token"], id_token);
        assert_eq!(recovery["access_token"], "a-new");
        assert_eq!(recovery["refresh_token"], "a-ref-new");
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
        assert_eq!(std::fs::read(&live_path).unwrap(), live_before);
    }

    #[test]
    fn rotated_credentials_are_preserved_when_the_auth_transaction_is_unavailable() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();

        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();

        let auth_lock = super::auth_lock_path().unwrap();
        std::fs::remove_file(&auth_lock).unwrap();
        std::fs::create_dir(&auth_lock).unwrap();

        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("alice@example.com", "acct_a"),
            "a-new",
            "a-ref-new",
        )
        .expect("the issued credential must be preserved as a typed recovery outcome");
        let super::RefreshTokenUpdate::RecoveryPreserved { path, cause } = update else {
            panic!("an unavailable auth transaction must preserve the staged credential")
        };
        assert!(
            format!("{cause:#}").contains("auth transaction could not be acquired"),
            "{cause:#}"
        );

        let saved = crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(
            saved
                .pointer("/tokens/refresh_token")
                .and_then(serde_json::Value::as_str),
            Some("a-ref"),
            "the profile must not be overwritten outside the compatibility transaction"
        );
        let recovered = crate::auth::read_auth(&path).unwrap();
        assert_eq!(
            recovered
                .pointer("/tokens/refresh_token")
                .and_then(serde_json::Value::as_str),
            Some("a-ref-new"),
            "the exact issued credential must remain available for recovery"
        );
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/refresh_token")
                .and_then(serde_json::Value::as_str),
            Some("a-ref"),
            "live auth remains unchanged until its transaction can be acquired"
        );
    }

    #[test]
    fn refresh_stages_before_waiting_for_the_auth_transaction_then_commits_exactly() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();
        let authorization = {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::authorize_fresh_credentials_activation(&lease).unwrap()
        };
        let transaction = super::lock_auth_transaction().unwrap();

        let worker = std::thread::spawn(move || {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::update_profile_tokens_if_refresh_matches_leased(
                &lease,
                authorization,
                "a-ref",
                &make_jwt("alice@example.com", "acct_a"),
                "a-new",
                "a-ref-new",
            )
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recovery_files().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "refresh did not stage its consumed response before waiting for the auth transaction"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(profile_refresh_token("alice"), "a-ref");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            alice,
            "profile and live auth must remain old while the compatibility transaction is held"
        );

        drop(transaction);
        let update = worker.join().unwrap().unwrap();
        assert!(matches!(update, super::RefreshTokenUpdate::Saved));
        assert_eq!(profile_refresh_token("alice"), "a-ref-new");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap()["tokens"]["refresh_token"],
            "a-ref-new"
        );
        assert!(recovery_files().is_empty());
    }

    #[test]
    fn conditional_switch_does_not_override_a_newer_manual_switch() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref"),
        );
        seed_profile(
            "bob",
            &realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref"),
        );
        seed_profile(
            "carol",
            &realistic_auth_json("carol@example.com", "acct_c", "c-old", "c-ref"),
        );
        super::switch_profile("alice").unwrap();
        super::switch_profile("bob").unwrap();

        assert!(
            super::switch_profile_if_current("alice", "carol")
                .unwrap()
                .is_none()
        );
        assert_eq!(current_alias(), "bob");
        assert_eq!(
            super::active_profile_from_live().unwrap().as_deref(),
            Some("bob")
        );
    }

    #[test]
    fn conditional_switch_holds_expected_profile_lease_through_ownership_check() {
        let _env = TestEnv::new();
        let expected = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let target = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        seed_profile("expected", &expected);
        seed_profile("target", &target);
        super::switch_profile("expected").unwrap();

        let held_lease = super::acquire_profile_lease("expected").unwrap();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("profile 'expected'", attempt_tx);
            let _ = done_tx.send(super::switch_profile_if_current("expected", "target"));
        });
        let mut cleanup = ThreadCleanup::new(held_lease);
        cleanup.push(worker);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("conditional switch did not wait for the expected profile lease");
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        cleanup.release_blocker();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("conditional switch did not finish after lease release")
                .unwrap()
                .is_some()
        );
        cleanup.join_all();
        assert_eq!(current_alias(), "target");
    }

    #[test]
    fn successful_switch_surfaces_selection_history_write_failure() {
        let _env = TestEnv::new();
        let target = realistic_auth_json("target@example.com", "acct_target", "t-old", "t-ref");
        seed_profile("target", &target);
        let app_home = crate::auth::app_home().unwrap();
        std::fs::create_dir_all(&app_home).unwrap();
        std::fs::write(app_home.join("cache.json"), b"{malformed cache").unwrap();

        let prepared = super::prepare_profile_switch("target").unwrap();
        let confirmed = super::confirm_prepared_profile_switch_without_overwrite(prepared).unwrap();
        let outcome = super::commit_confirmed_profile_switch(confirmed)
            .expect("selection-history failure must not misreport the committed auth switch");

        let warning = outcome
            .selection_history_warning()
            .expect("malformed cache must remain visible to the caller");
        assert!(format!("{warning:#}").contains("parsing cache file"));
        assert_eq!(
            super::active_profile_from_live().unwrap().as_deref(),
            Some("target")
        );
    }

    #[test]
    fn successful_switch_does_not_wait_for_contended_selection_history() {
        let _env = TestEnv::new();
        let target = realistic_auth_json("target@example.com", "acct_target", "t-old", "t-ref");
        seed_profile("target", &target);
        let prepared = super::prepare_profile_switch("target").unwrap();
        let confirmed = super::confirm_prepared_profile_switch_without_overwrite(prepared).unwrap();

        let app_home = crate::auth::app_home().unwrap();
        std::fs::create_dir_all(&app_home).unwrap();
        std::fs::write(
            app_home.join("cache.json"),
            br#"{"entries":{},"last_used":{}}"#,
        )
        .unwrap();
        let cache_lock_path = app_home.join("cache.lock");
        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(cache_lock_path)
            .unwrap();
        FileExt::lock(&cache_lock).unwrap();

        let (switch_tx, switch_rx) = std::sync::mpsc::channel();
        let switch_worker = std::thread::spawn(move || {
            let _ = switch_tx.send(super::commit_confirmed_profile_switch(confirmed));
        });
        let switch = switch_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("committed switch waited for the contended selection-history cache")
            .unwrap();
        let warning = switch
            .selection_history_warning()
            .expect("cache contention must remain visible as a non-fatal history warning");
        assert!(format!("{warning:#}").contains("cache lock"));
        switch_worker.join().unwrap();
        assert_eq!(current_alias(), "target");

        // The auth transaction and target lease end with the committed switch,
        // even though its derived history could not be recorded. Releasing the
        // unrelated cache owner then permits a normal rename without a stale
        // last-used key being recreated under either alias.
        super::lock_auth_transaction()
            .map(drop)
            .expect("committed switch retained the auth transaction");
        drop(cache_lock);
        let _ = super::rename_profile("target", "renamed").unwrap();

        let cache: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app_home.join("cache.json")).unwrap()).unwrap();
        assert!(cache.pointer("/last_used/target").is_none());
        assert!(cache.pointer("/last_used/renamed").is_none());
    }

    #[test]
    fn successful_selection_history_write_releases_auth_but_keeps_target_lease() {
        let _env = TestEnv::new();
        let target = realistic_auth_json("target@example.com", "acct_target", "t-old", "t-ref");
        seed_profile("target", &target);
        let prepared = super::prepare_profile_switch("target").unwrap();
        let confirmed = super::confirm_prepared_profile_switch_without_overwrite(prepared).unwrap();

        let (history_started_tx, history_started_rx) = std::sync::mpsc::channel();
        let (history_continue_tx, history_continue_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let (switch_tx, switch_rx) = std::sync::mpsc::channel();
        let switch_worker = std::thread::spawn(move || {
            crate::cache::before_next_last_used_write(move || {
                history_started_tx.send(()).unwrap();
                let _ = history_continue_rx.recv();
            });
            let _ = switch_tx.send(super::commit_confirmed_profile_switch(confirmed));
        });
        let mut cleanup = ThreadCleanup::new(history_continue_tx);
        cleanup.push(switch_worker);
        history_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not reach the successful selection-history write");

        let (auth_done_tx, auth_done_rx) = std::sync::mpsc::channel();
        cleanup.push(std::thread::spawn(move || {
            let _ = auth_done_tx.send(super::lock_auth_transaction().map(drop));
        }));
        auth_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("selection-history write retained the auth transaction")
            .expect("concurrent auth transaction failed");

        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (rename_tx, rename_rx) = std::sync::mpsc::channel();
        cleanup.push(std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("profile 'target'", attempt_tx);
            let _ = rename_tx.send(super::rename_profile("target", "renamed"));
        }));
        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not contend on the switch-owned target lease");
        assert!(matches!(
            rename_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        cleanup.release_blocker();
        let switch = switch_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not finish after selection-history continuation")
            .unwrap();
        assert!(switch.selection_history_warning().is_none());
        let rename = rename_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not finish after the switch released its target lease")
            .unwrap();
        assert!(rename.durability_warning().is_none());
        cleanup.join_all();

        let app_home = crate::auth::app_home().unwrap();
        let cache: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app_home.join("cache.json")).unwrap()).unwrap();
        assert!(cache.pointer("/last_used/target").is_none());
        assert!(cache.pointer("/last_used/renamed").is_some());
    }

    #[test]
    fn authorized_switch_owns_target_lease_until_release() {
        let _env = TestEnv::new();
        let target = realistic_auth_json("target@example.com", "acct_target", "t-old", "t-ref");
        seed_profile("target", &target);
        let confirmed = super::prepare_and_confirm_profile_switch("target", false).unwrap();
        let lease = super::acquire_profile_lease("target").unwrap();
        let authorized =
            super::authorize_confirmed_profile_switch_before_side_effect(confirmed, lease).unwrap();

        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("profile 'target'", attempt_tx);
            let _ = done_tx.send(super::rename_profile("target", "renamed"));
        });
        let mut cleanup = ThreadCleanup::new(authorized);
        cleanup.push(worker);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not wait for the authorization-owned target lease");
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        cleanup.release_blocker();
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not finish after authorization release")
            .unwrap();
        cleanup.join_all();
        assert!(super::profile_auth_path("renamed").unwrap().exists());
    }

    #[test]
    fn authorized_switch_adopts_selected_rotation_and_skips_exact_activation_writes() {
        let _env = TestEnv::new();
        let target = realistic_auth_json("target@example.com", "acct_target", "t-old", "t-ref");
        seed_profile("target", &target);
        super::switch_profile("target").unwrap();

        let confirmed = super::prepare_and_confirm_profile_switch("target", false).unwrap();
        let lease = super::acquire_profile_lease("target").unwrap();
        let authorized =
            super::authorize_confirmed_profile_switch_before_side_effect(confirmed, lease).unwrap();

        let first_refresh =
            super::authorize_fresh_credentials_activation(authorized.lease()).unwrap();
        let first_update = super::update_profile_tokens_if_refresh_matches_leased(
            authorized.lease(),
            first_refresh,
            "t-ref",
            &make_jwt("target@example.com", "acct_target"),
            "t-mid",
            "t-ref-mid",
        )
        .unwrap();
        assert!(matches!(first_update, super::RefreshTokenUpdate::Saved));
        super::revalidate_authorized_profile_switch(&authorized)
            .expect("the selected target's strict same-account rotation must be adopted");

        // A second rotation after preflight models the post-redemption usage
        // request. The final commit must safely rebase once more rather than
        // comparing against either older live snapshot.
        let second_refresh =
            super::authorize_fresh_credentials_activation(authorized.lease()).unwrap();
        let second_update = super::update_profile_tokens_if_refresh_matches_leased(
            authorized.lease(),
            second_refresh,
            "t-ref-mid",
            &make_jwt("target@example.com", "acct_target"),
            "t-new",
            "t-ref-new",
        )
        .unwrap();
        assert!(matches!(second_update, super::RefreshTokenUpdate::Saved));

        let live_publish_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let live_publish_ran_from_hook = live_publish_ran.clone();
        super::before_next_activation_live_publish(move || {
            live_publish_ran_from_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        super::fail_next_activation_marker_write();

        let outcome = super::commit_authorized_profile_switch(authorized)
            .expect("an exact already-active rotated generation must commit as a no-op");
        assert!(outcome.selection_history_warning().is_none());
        assert!(
            !live_publish_ran.load(std::sync::atomic::Ordering::SeqCst),
            "the exact live credential generation must not be republished"
        );
        let marker_error = super::write_activation_marker("target")
            .expect_err("the marker failure must remain armed when the no-op skipped its write");
        assert!(
            marker_error
                .to_string()
                .contains("injected activation marker failure"),
            "{marker_error:#}"
        );
        super::run_before_activation_live_publish_test_hook();
        assert!(
            live_publish_ran.load(std::sync::atomic::Ordering::SeqCst),
            "the publication hook must remain armed when the no-op skipped its write"
        );
        assert_eq!(profile_refresh_token("target"), "t-ref-new");
        assert_eq!(current_alias(), "target");
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/refresh_token")
                .and_then(serde_json::Value::as_str),
            Some("t-ref-new")
        );
    }

    #[test]
    fn authorized_switch_rejects_same_identity_with_different_credential_bytes() {
        let _env = TestEnv::new();
        let target = realistic_auth_json("same@example.com", "acct_same", "target", "target-ref");
        let unrelated_rotation =
            realistic_auth_json("same@example.com", "acct_same", "other", "other-ref");
        seed_profile("target", &target);
        super::switch_profile("target").unwrap();

        let confirmed = super::prepare_and_confirm_profile_switch("target", false).unwrap();
        let lease = super::acquire_profile_lease("target").unwrap();
        let authorized =
            super::authorize_confirmed_profile_switch_before_side_effect(confirmed, lease).unwrap();
        write_live(&unrelated_rotation);

        let error = super::commit_authorized_profile_switch(authorized)
            .expect_err("strict identity alone must not adopt different credential bytes");
        assert!(
            error
                .to_string()
                .contains("no longer exactly matches profile 'target'"),
            "{error:#}"
        );
        assert_eq!(current_alias(), "target");
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            unrelated_rotation
        );
    }

    #[test]
    fn auth_lock_timeout_preserves_live_lock_inode() {
        let _env = TestEnv::new();
        let lock_path = super::auth_lock_path().unwrap();
        super::ensure_private_dir(lock_path.parent().unwrap()).unwrap();
        let holder = super::open_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();
        super::write_lock_holder(&holder);

        let err =
            super::acquire_file_lock(&lock_path, Duration::from_millis(25), "auth").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("auth lock"), "{message}");
        assert!(
            message.contains(&lock_path.display().to_string()),
            "{message}"
        );

        let reopened = super::open_lock_file(&lock_path).unwrap();
        assert!(matches!(
            FileExt::try_lock(&reopened),
            Err(fs4::TryLockError::WouldBlock)
        ));
        FileExt::unlock(&holder).unwrap();
    }

    #[test]
    fn rename_waits_for_the_profiles_credential_lease() {
        let _env = TestEnv::new();
        seed_profile(
            "old",
            &realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref"),
        );
        let held_lease = super::acquire_profile_lease("old").unwrap();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("profile 'old'", attempt_tx);
            let _ = done_tx.send(super::rename_profile("old", "new"));
        });
        let mut cleanup = ThreadCleanup::new(held_lease);
        cleanup.push(worker);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not wait on the profile lease");
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        cleanup.release_blocker();
        let _ = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("rename did not finish after lease release")
            .unwrap();
        cleanup.join_all();
        assert!(super::profile_auth_path("new").unwrap().exists());
        assert!(!super::profile_auth_path("old").unwrap().exists());
    }

    #[test]
    fn import_credential_reservation_blocks_registry_changes_through_validation() {
        let _env = TestEnv::new();
        let incoming = realistic_auth_json(
            "import@example.com",
            "acct_import",
            "import-access",
            "import-refresh",
        );
        let reservation = super::reserve_import_credential_for_validation(&incoming).unwrap();
        let concurrent = realistic_auth_json(
            "other@example.com",
            "acct_other",
            "other-access",
            "other-refresh",
        );
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("legacy launch compatibility", attempt_tx);
            let _ = done_tx.send(super::save_auth_value(concurrent, Some("other")));
        });
        let mut cleanup = ThreadCleanup::new(reservation);
        cleanup.push(worker);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("concurrent profile commit did not wait on the import reservation");
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        cleanup.release_blocker();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("profile commit did not resume after reservation release")
            .unwrap();
        cleanup.join_all();
        assert!(super::profile_auth_path("other").unwrap().exists());
    }

    #[test]
    fn import_reservation_restarts_when_an_alias_commits_after_its_initial_scan() {
        let _env = TestEnv::new();
        let incoming = realistic_auth_json(
            "import@example.com",
            "acct_import",
            "import-access",
            "import-refresh",
        );
        let late_profile = incoming.clone();
        super::after_next_import_registry_scan(move || seed_profile("late", &late_profile));

        let error = match super::reserve_import_credential_for_validation(&incoming) {
            Err(error) => error,
            Ok(_) => panic!("the stable rescan must include the newly committed alias"),
        };

        assert!(
            format!("{error:#}").contains("profile 'late' already owns this credential"),
            "{error:#}"
        );
    }

    #[test]
    fn reserved_import_commit_reuses_the_reserved_registry_snapshot() {
        let _env = TestEnv::new();
        seed_profile(
            "existing",
            &realistic_auth_json(
                "existing@example.com",
                "acct_existing",
                "existing-access",
                "existing-refresh",
            ),
        );
        let incoming = realistic_auth_json(
            "import@example.com",
            "acct_import",
            "import-access",
            "import-refresh",
        );
        super::reset_profile_registry_snapshot_count();

        let reservation = super::reserve_import_credential_for_validation(&incoming).unwrap();
        assert_eq!(super::profile_registry_snapshot_count(), 1);
        let committed = super::save_reserved_imported_auth_value_with_stage(
            &incoming,
            Some("imported"),
            "acct_import",
            None,
            None,
            reservation,
        )
        .unwrap();
        let super::ValidatedImportCommit::Profile(action) = committed else {
            panic!("an import without rotation material cannot require recovery")
        };

        assert!(
            matches!(action.action, super::SaveAction::Created(ref alias) if alias == "imported")
        );
        assert!(action.profile_commit.is_none());
        assert!(action.recovery_cleanup.is_none());
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "the validated commit must consume the snapshot retained by its reservation"
        );
    }

    #[test]
    fn reserved_import_snapshot_still_rejects_an_existing_exact_identity() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json(
                "alice@example.com",
                "acct_alice",
                "existing-access",
                "existing-refresh",
            ),
        );
        let incoming = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "import-access",
            "import-refresh",
        );
        super::reset_profile_registry_snapshot_count();

        let reservation = super::reserve_import_credential_for_validation(&incoming).unwrap();
        let error = super::save_reserved_imported_auth_value_with_stage(
            &incoming,
            Some("alice"),
            "acct_alice",
            None,
            None,
            reservation,
        )
        .expect_err("the retained identity snapshot must preserve create-only import semantics");

        assert!(error.to_string().contains("profile 'alice'"), "{error:#}");
        assert!(
            error.to_string().contains("same account_id and email"),
            "{error:#}"
        );
        assert_eq!(super::profile_registry_snapshot_count(), 1);
        assert!(!super::profile_auth_path("alice_2").unwrap().exists());
    }

    #[test]
    fn switch_profile_waits_for_legacy_launch_compatibility_lock() {
        let _env = TestEnv::new();
        let next = realistic_auth_json("next@example.com", "acct_next", "acc_new", "ref_new");
        let profile_path = super::profile_auth_path("next-profile").unwrap();
        super::ensure_profile_parent(&profile_path).unwrap();
        write_auth_durable(&profile_path, &next);

        let lease = super::lock_legacy_launch_compatibility().unwrap();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("legacy launch compatibility", attempt_tx);
            let _ = done_tx.send(super::switch_profile("next-profile"));
        });
        let mut cleanup = ThreadCleanup::new(lease);
        cleanup.push(handle);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not reach the legacy compatibility lock");
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "switch must wait while an older launcher can still restore auth.json"
        );

        cleanup.release_blocker();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not finish after the compatibility lock was released")
            .unwrap();
        cleanup.join_all();
    }

    #[test]
    fn refreshed_profile_and_live_auth_update_are_one_transaction() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        let alice_path = super::profile_auth_path("alice").unwrap();
        let bob_path = super::profile_auth_path("bob").unwrap();
        super::ensure_profile_parent(&alice_path).unwrap();
        super::ensure_profile_parent(&bob_path).unwrap();
        write_auth_durable(&alice_path, &alice);
        write_auth_durable(&bob_path, &bob);
        super::switch_profile("alice").unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let updater = std::thread::spawn(move || {
            let lease = super::acquire_profile_lease("alice").unwrap();
            let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
            let result = super::update_profile_tokens_if_refresh_matches_after_lock(
                &lease,
                authorization,
                "a-ref",
                &make_jwt("alice@example.com", "acct_a"),
                "a-new",
                "a-ref-new",
                super::RefreshCommitHooks {
                    after_lock: || {
                        let _ = started_tx.send(());
                        let _ = continue_rx.recv();
                    },
                    before_cleanup: || {},
                },
            );
            let _ = done_tx.send(result);
        });
        let mut cleanup = ThreadCleanup::new(continue_tx);
        cleanup.push(updater);
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (switch_tx, switch_rx) = std::sync::mpsc::channel();
        let switcher = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("legacy launch compatibility", attempt_tx);
            let _ = switch_tx.send(super::switch_profile("bob"));
        });
        cleanup.push(switcher);
        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("profile switch did not wait for the refresh transaction");
        assert!(matches!(
            switch_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        cleanup.release_blocker();
        let update = done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(matches!(update, super::RefreshTokenUpdate::Saved));
        switch_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("profile switch did not finish after refresh transaction")
            .unwrap();
        cleanup.join_all();

        assert_eq!(current_alias(), "bob");
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("b-old")
        );
        let alice_updated = crate::auth::read_auth(&alice_path).unwrap();
        assert_eq!(
            alice_updated
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("a-new")
        );
    }

    #[test]
    fn sync_current_from_live_matches_live_identity() {
        let _env = TestEnv::new();

        let alpha = realistic_auth_json("alpha@example.com", "acct_alpha", "acc_a", "ref_a");
        let alpha_path = super::profile_auth_path("alpha").unwrap();
        super::ensure_profile_parent(&alpha_path).unwrap();
        write_auth_durable(&alpha_path, &alpha);

        let beta = realistic_auth_json("beta@example.com", "acct_beta", "acc_b_old", "ref_b_old");
        let beta_path = super::profile_auth_path("beta").unwrap();
        super::ensure_profile_parent(&beta_path).unwrap();
        write_auth_durable(&beta_path, &beta);

        super::write_current("alpha").unwrap();
        let live = realistic_auth_json("beta@example.com", "acct_beta", "acc_b_new", "ref_b_new");
        write_auth_durable(&crate::auth::codex_auth_path().unwrap(), &live);
        super::reset_profile_registry_snapshot_count();

        assert_eq!(
            super::sync_current_from_live().unwrap().as_deref(),
            Some("beta")
        );
        assert_eq!(current_alias(), "beta");
        assert_eq!(
            super::profile_registry_snapshot_count(),
            2,
            "a stale marker still needs discovery plus locked revalidation"
        );
    }

    #[test]
    fn checked_account_list_and_active_binding_share_one_registry_snapshot() {
        let env = TestEnv::new();
        let alpha = realistic_auth_json("alpha@example.com", "acct_alpha", "acc_a", "ref_a");
        let beta = realistic_auth_json("beta@example.com", "acct_beta", "acc_b", "ref_b");
        seed_profile("alpha", &alpha);
        seed_profile("beta", &beta);
        write_auth_durable(&crate::auth::codex_auth_path().unwrap(), &beta);
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = [\"acct_alpha\", \"acct_beta\"]\n",
        )
        .unwrap();
        super::reset_profile_registry_snapshot_count();
        crate::auth::reset_managed_policy_batch_test_counts();

        let (accounts, active) = super::load_profile_accounts_checked_with_active().unwrap();

        assert_eq!(accounts.len(), 2);
        assert_eq!(active.as_deref(), Some("beta"));
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "list rows and active-profile resolution must reuse one immutable registry read"
        );
        assert_eq!(
            crate::auth::managed_policy_batch_test_counts(),
            (1, 1, 2),
            "the combined list and active binding batch must share one policy snapshot"
        );
    }

    #[test]
    fn checked_account_batch_loads_one_policy_and_parses_each_account_once() {
        let env = TestEnv::new();
        let alpha = realistic_auth_json("alpha@example.com", "acct_alpha", "acc_a", "ref_a");
        let beta = realistic_auth_json("beta@example.com", "acct_beta", "acc_b", "ref_b");
        seed_profile("alpha", &alpha);
        seed_profile("beta", &beta);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = [\"acct_alpha\", \"acct_beta\"]\n",
        )
        .unwrap();
        crate::auth::reset_managed_policy_batch_test_counts();

        let accounts = super::load_profile_accounts_checked().unwrap();

        assert_eq!(accounts.len(), 2);
        assert_eq!(
            crate::auth::managed_policy_batch_test_counts(),
            (1, 1, 2),
            "one batch must load and parse config once and parse each profile JWT once"
        );
    }

    #[test]
    fn unchecked_account_batch_does_not_read_managed_policy() {
        let env = TestEnv::new();
        let alpha = realistic_auth_json("alpha@example.com", "acct_alpha", "acc_a", "ref_a");
        let beta = realistic_auth_json("beta@example.com", "acct_beta", "acc_b", "ref_b");
        seed_profile("alpha", &alpha);
        seed_profile("beta", &beta);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "this is deliberately invalid TOML",
        )
        .unwrap();
        crate::auth::reset_managed_policy_batch_test_counts();

        let accounts = super::load_profile_accounts().unwrap();

        assert_eq!(accounts.len(), 2);
        assert_eq!(
            crate::auth::managed_policy_batch_test_counts(),
            (0, 0, 2),
            "the unchecked TUI projection must not touch managed config"
        );
    }

    #[test]
    fn checked_account_batch_uses_one_config_generation() {
        let env = TestEnv::new();
        let alpha = realistic_auth_json("alpha@example.com", "acct_alpha", "acc_a", "ref_a");
        let beta = realistic_auth_json("beta@example.com", "acct_beta", "acc_b", "ref_b");
        seed_profile("alpha", &alpha);
        seed_profile("beta", &beta);
        let config_path = env._home.path().join(".codex/config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "forced_chatgpt_workspace_id = [\"acct_alpha\", \"acct_beta\"]\n",
        )
        .unwrap();
        super::after_next_profile_policy_validation({
            let config_path = config_path.clone();
            move || {
                std::fs::write(
                    config_path,
                    "forced_chatgpt_workspace_id = \"acct_alpha\"\n",
                )
                .unwrap();
            }
        });

        let accounts = super::load_profile_accounts_checked().unwrap();

        assert_eq!(
            accounts
                .iter()
                .map(|account| account.alias.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let error = super::load_profile_accounts_checked()
            .expect_err("the next batch must observe the newer policy generation");
        let detail = format!("{error:#}");
        assert!(
            detail.contains("acct_beta") && detail.contains("not allowed"),
            "{detail}"
        );
    }

    #[test]
    fn sync_current_exact_marker_uses_only_the_locked_registry_scan() {
        let _env = TestEnv::new();
        let exact = realistic_auth_json("same@example.com", "acct_same", "same", "same-ref");
        seed_profile("first", &exact);
        seed_profile("second", &exact);
        super::switch_profile("second").unwrap();
        super::reset_profile_registry_snapshot_count();

        assert_eq!(
            super::sync_current_from_live().unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(current_alias(), "second");
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "an exact current marker is only a hint; one full locked scan remains authoritative"
        );
    }

    #[test]
    fn synced_registry_projects_candidates_without_another_registry_snapshot() {
        let _env = TestEnv::new();
        let current = realistic_auth_json(
            "current@example.com",
            "acct_current",
            "current",
            "current-ref",
        );
        let candidate = realistic_auth_json(
            "candidate@example.com",
            "acct_candidate",
            "candidate",
            "candidate-ref",
        );
        seed_profile("current", &current);
        seed_profile("candidate", &candidate);
        super::switch_profile("current").unwrap();
        super::reset_profile_registry_snapshot_count();

        let synced = super::sync_current_from_live_with_registry()
            .unwrap()
            .expect("the exact live profile should synchronize");

        assert_eq!(synced.current(), "current");
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "the locked active-profile scan should produce the retained snapshot"
        );
        let accounts = synced.into_candidate_accounts().unwrap();
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "candidate projection must parse retained bytes without reopening the registry"
        );
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].alias, "candidate");
        assert_eq!(
            accounts[0]
                .info
                .strict_binding()
                .expect("the candidate should have a complete identity")
                .account_id,
            "acct_candidate"
        );
    }

    #[test]
    fn synced_registry_defers_malformed_candidate_parsing_until_projection() {
        let _env = TestEnv::new();
        let current = realistic_auth_json(
            "current@example.com",
            "acct_current",
            "current",
            "current-ref",
        );
        seed_profile("current", &current);
        super::switch_profile("current").unwrap();
        let broken_path = super::profile_auth_path("broken").unwrap();
        super::ensure_profile_parent(&broken_path).unwrap();
        std::fs::write(&broken_path, b"{not-json").unwrap();

        let synced = super::sync_current_from_live_with_registry()
            .expect("an unrelated malformed candidate is not parsed below the threshold boundary")
            .expect("the exact current profile should still synchronize");
        let error = synced
            .into_candidate_accounts()
            .expect_err("candidate projection must surface malformed saved auth");

        assert!(format!("{error:#}").contains("broken"), "{error:#}");
    }

    #[test]
    fn sync_current_exact_hint_still_reports_an_incomplete_registry() {
        let _env = TestEnv::new();
        let exact = realistic_auth_json("good@example.com", "acct_good", "same", "same-ref");
        seed_profile("good", &exact);
        super::switch_profile("good").unwrap();
        std::fs::create_dir_all(
            super::profile_auth_path("broken")
                .unwrap()
                .parent()
                .unwrap(),
        )
        .unwrap();
        super::reset_profile_registry_snapshot_count();

        let error = super::sync_current_from_live()
            .expect_err("the hint must not hide an unreadable saved-profile entry");
        let detail = format!("{error:#}");
        assert!(detail.contains("broken"), "{detail}");
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "the authoritative locked scan must still inspect the complete registry"
        );
    }

    #[test]
    fn switch_boundary_noop_does_not_apply_write_policy_to_identical_auth() {
        let env = TestEnv::new();
        let disallowed = realistic_auth_json(
            "blocked@example.com",
            "workspace-blocked",
            "same-access",
            "same-refresh",
        );
        let profile_path = super::profile_auth_path("broken").unwrap();
        super::ensure_profile_parent(&profile_path).unwrap();
        write_auth_durable(&profile_path, &disallowed);
        write_auth_durable(&crate::auth::codex_auth_path().unwrap(), &disallowed);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();
        let lease = super::acquire_profile_lease("broken").unwrap();

        super::synchronize_profile_from_live_for_switch_leased(&lease).unwrap();

        assert_eq!(crate::auth::read_auth(&profile_path).unwrap(), disallowed);
        assert!(!crate::auth::current_file().unwrap().exists());
    }

    #[test]
    fn switch_boundary_still_rejects_disallowed_auth_before_profile_write() {
        let env = TestEnv::new();
        let saved = realistic_auth_json(
            "blocked@example.com",
            "workspace-blocked",
            "old-access",
            "old-refresh",
        );
        let live = realistic_auth_json(
            "blocked@example.com",
            "workspace-blocked",
            "new-access",
            "new-refresh",
        );
        let profile_path = super::profile_auth_path("blocked").unwrap();
        super::ensure_profile_parent(&profile_path).unwrap();
        write_auth_durable(&profile_path, &saved);
        write_auth_durable(&crate::auth::codex_auth_path().unwrap(), &live);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();
        let lease = super::acquire_profile_lease("blocked").unwrap();

        let error = super::synchronize_profile_from_live_for_switch_leased(&lease).unwrap_err();

        assert!(format!("{error:#}").contains("not allowed"), "{error:#}");
        assert_eq!(crate::auth::read_auth(&profile_path).unwrap(), saved);
    }

    // ── detect_auth_change tests ─────────────────────────────

    fn make_jwt(email: &str, account_id: &str) -> String {
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": format!("user_{account_id}"),
                "organizations": [],
            }
        });
        let json = serde_json::to_vec(&claims).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json)
        };
        format!("x.{encoded}.y")
    }

    /// Build a realistic auth.json matching the format produced by `login::build_auth_json`.
    fn realistic_auth_json(
        email: &str,
        account_id: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": make_jwt(email, account_id),
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": "2026-04-07T00:00:00Z"
        })
    }

    fn auth_json_without_identity(access_token: &str, refresh_token: &str) -> serde_json::Value {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        };
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": format!("x.{encoded}.y"),
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": "",
            },
            "last_refresh": "2026-04-07T00:00:00Z"
        })
    }

    fn auth_json_without_email(
        account_id: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> serde_json::Value {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": format!("user_{account_id}"),
                "organizations": [],
            }
        });
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        };
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": format!("x.{encoded}.y"),
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": "2026-04-07T00:00:00Z"
        })
    }

    // ── Basic branch coverage ────────────────────────────────

    #[test]
    fn detect_no_auth_file_returns_no_live_auth() {
        let _env = TestEnv::new();
        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoLiveAuth
        ));
    }

    #[test]
    fn detect_corrupt_auth_file_reports_the_parse_error() {
        let env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let parent = live.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        std::fs::write(&live, "{invalid json!!!").unwrap();
        let error = super::detect_auth_change()
            .expect_err("a present malformed live auth file must not disable synchronization");
        assert!(
            format!("{error:#}").contains("parsing live auth for change detection"),
            "{error:#}"
        );
        drop(env);
    }

    #[test]
    fn detect_live_auth_read_failure_is_not_reported_as_no_change() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        std::fs::create_dir_all(&live).unwrap();

        let error = super::detect_auth_change()
            .expect_err("an unreadable live auth path must stop change detection");
        assert!(
            format!("{error:#}").contains("reading live auth for change detection"),
            "{error:#}"
        );
    }

    #[test]
    fn detect_exact_current_profile_skips_the_registry_snapshot() {
        let _env = TestEnv::new();
        let auth = realistic_auth_json(
            "stable@example.com",
            "acct_stable",
            "stable-access",
            "stable-refresh",
        );
        seed_profile("stable", &auth);
        write_live(&auth);
        super::write_current("stable").unwrap();
        super::reset_profile_registry_snapshot_count();

        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        );
        assert_eq!(
            super::profile_registry_snapshot_count(),
            0,
            "an exact checked current binding must not enumerate unrelated profiles"
        );
    }

    #[test]
    fn exact_current_auto_track_and_account_list_need_one_full_snapshot() {
        let _env = TestEnv::new();
        let auth = auth_json_without_identity("stable-access", "stable-refresh");
        seed_profile("stable", &auth);
        write_live(&auth);
        super::write_current("stable").unwrap();
        super::reset_profile_registry_snapshot_count();

        assert!(!super::auto_track_current().unwrap());
        assert_eq!(
            super::profile_registry_snapshot_count(),
            0,
            "stable auto-track must finish before an identity or registry scan"
        );

        let (accounts, active) = super::load_profile_accounts_checked_with_active().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].alias, "stable");
        assert_eq!(active.as_deref(), Some("stable"));
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "the account batch must remain the only complete registry snapshot"
        );
    }

    #[test]
    fn exact_current_profile_read_errors_do_not_fall_through_to_a_registry_scan() {
        let _env = TestEnv::new();
        let auth = realistic_auth_json(
            "broken@example.com",
            "acct_broken",
            "broken-access",
            "broken-refresh",
        );
        write_live(&auth);
        let profile_path = super::profile_auth_path("broken").unwrap();
        std::fs::create_dir_all(&profile_path).unwrap();
        super::write_current("broken").unwrap();

        super::reset_profile_registry_snapshot_count();
        let detect_error = super::detect_auth_change()
            .expect_err("a current-profile read failure must stop change detection");
        assert!(
            format!("{detect_error:#}").contains("reading current profile 'broken'"),
            "{detect_error:#}"
        );
        assert_eq!(super::profile_registry_snapshot_count(), 0);

        super::reset_profile_registry_snapshot_count();
        let auto_track_error = super::auto_track_current()
            .expect_err("a current-profile read failure must stop auto-track");
        assert!(
            format!("{auto_track_error:#}").contains("reading current profile 'broken'"),
            "{auto_track_error:#}"
        );
        assert_eq!(super::profile_registry_snapshot_count(), 0);
    }

    #[test]
    fn unrelated_disallowed_profile_does_not_block_live_equivalence() {
        let env = TestEnv::new();
        let allowed = realistic_auth_json(
            "allowed@example.com",
            "workspace-allowed",
            "allowed-access",
            "allowed-refresh",
        );
        let blocked = realistic_auth_json(
            "blocked@example.com",
            "workspace-blocked",
            "blocked-access",
            "blocked-refresh",
        );
        seed_profile("allowed", &allowed);
        seed_profile("blocked", &blocked);
        write_live(&allowed);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();

        let prepared = super::prepare_profile_switch("allowed").unwrap();

        assert!(!prepared.requires_confirmation());
    }

    #[test]
    fn detect_profile_scan_parse_failure_is_not_reported_as_no_change() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(
            &live,
            &realistic_auth_json("new@example.com", "acct_new", "acc", "ref"),
        );
        let broken = super::profile_auth_path("broken").unwrap();
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "{not valid json").unwrap();

        let error = super::detect_auth_change()
            .expect_err("a malformed saved profile must stop identity scanning");
        let detail = format!("{error:#}");
        assert!(
            detail.contains("comparing the live auth identity with saved profiles"),
            "{detail}"
        );
        assert!(detail.contains("broken"), "{detail}");
    }

    #[test]
    fn detect_exact_match_remains_authoritative_with_an_unrelated_malformed_profile() {
        let _env = TestEnv::new();
        let live_auth = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "same-access",
            "same-refresh",
        );
        seed_profile("alice", &live_auth);
        write_live(&live_auth);
        let broken = super::profile_auth_path("broken").unwrap();
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "{not valid json").unwrap();

        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        );
    }

    #[test]
    fn detect_equivalent_identityless_duplicates_require_the_current_marker() {
        let _env = TestEnv::new();
        let auth = auth_json_without_identity("same-access", "same-refresh");
        seed_profile("first", &auth);
        seed_profile("second", &auth);
        let live = crate::auth::codex_auth_path().unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, serde_json::to_vec(&auth).unwrap()).unwrap();

        let error = super::detect_auth_change()
            .expect_err("equivalent identityless duplicates need explicit marker evidence");
        assert!(
            format!("{error:#}").contains("current marker does not disambiguate"),
            "{error:#}"
        );

        super::write_current("second").unwrap();
        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        );
        assert_eq!(current_alias(), "second");
    }

    #[test]
    fn detect_exact_match_marker_read_failure_is_not_reported_as_no_change() {
        let _env = TestEnv::new();
        let value = realistic_auth_json("test@example.com", "acct_1", "acc", "ref");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &value);
        super::cmd_save(Some("test-profile")).unwrap();
        let current = crate::auth::current_file().unwrap();
        std::fs::remove_file(&current).unwrap();
        std::fs::create_dir(&current).unwrap();
        super::reset_profile_registry_snapshot_count();

        let error = super::detect_auth_change()
            .expect_err("a marker observation failure must stop exact-match repair");
        assert!(
            format!("{error:#}").contains("current marker for exact live-auth matching"),
            "{error:#}"
        );
        assert_eq!(
            super::profile_registry_snapshot_count(),
            0,
            "a checked marker error must not be downgraded into a full-scan miss"
        );
    }

    #[test]
    fn detect_exact_match_returns_no_change_and_repairs_marker() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("test@example.com", "acct_1", "acc_a", "ref_a");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("test-profile")).unwrap();
        super::write_current("stale-profile").unwrap();
        super::reset_profile_registry_snapshot_count();
        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        ));
        assert_eq!(current_alias(), "test-profile");
        assert_eq!(
            super::profile_registry_snapshot_count(),
            1,
            "a stale marker must still take the authoritative registry path"
        );
    }

    #[test]
    fn detect_exact_match_with_current_marker_does_not_wait_for_profile_lease() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("test@example.com", "acct_1", "acc_a", "ref_a");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("test-profile")).unwrap();
        let held_lease = super::acquire_profile_lease("test-profile").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(super::detect_auth_change()).unwrap();
        });

        let detected = rx.recv_timeout(std::time::Duration::from_secs(1));
        drop(held_lease);
        worker.join().unwrap();

        assert_eq!(
            detected
                .expect("an already-correct marker must not enter the repair lock path")
                .unwrap(),
            super::AuthChange::NoChange
        );
    }

    #[test]
    fn detect_exact_match_without_identity_remains_no_change() {
        let _env = TestEnv::new();
        let auth = auth_json_without_identity("same-access", "same-refresh");
        seed_profile("identityless", &auth);
        write_live(&auth);

        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        );
    }

    #[test]
    fn detect_equivalent_match_without_identity_remains_no_change() {
        let _env = TestEnv::new();
        let auth = auth_json_without_identity("same-access", "same-refresh");
        seed_profile("identityless", &auth);
        let live = crate::auth::codex_auth_path().unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, serde_json::to_vec(&auth).unwrap()).unwrap();
        assert_ne!(
            std::fs::read(&live).unwrap(),
            std::fs::read(super::profile_auth_path("identityless").unwrap()).unwrap()
        );

        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NoChange
        );
        assert_eq!(current_alias(), "identityless");
    }

    #[test]
    fn detect_new_account_when_no_profiles_exist() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("new@example.com", "acct_new", "acc_x", "ref_x");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NewAccount
        ));
    }

    #[test]
    fn detect_new_account_when_different_identity() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_alice", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &alice);
        super::cmd_save(Some("alice")).unwrap();
        // Different person
        let bob = realistic_auth_json("bob@example.com", "acct_bob", "acc_2", "ref_2");
        write_auth_durable(&live, &bob);
        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NewAccount
        ));
    }

    // ── Token update scenarios (real refresh patterns) ───────

    #[test]
    fn detect_tokens_updated_refresh_token_changed() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("user@example.com", "acct_u", "acc_old", "ref_old");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("user-profile")).unwrap();
        // Re-login: new refresh_token
        let updated = realistic_auth_json("user@example.com", "acct_u", "acc_old", "ref_new");
        write_auth_durable(&live, &updated);
        match super::detect_auth_change().unwrap() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "user-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    #[test]
    fn detect_tokens_updated_only_access_token_changed() {
        let _env = TestEnv::new();
        // Simulates token refresh where only access_token rotates (refresh_token reused)
        let val = realistic_auth_json("user@example.com", "acct_u", "acc_old", "ref_same");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("user-profile")).unwrap();
        let updated = realistic_auth_json("user@example.com", "acct_u", "acc_new", "ref_same");
        write_auth_durable(&live, &updated);
        match super::detect_auth_change().unwrap() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "user-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    #[test]
    fn detect_tokens_updated_only_last_refresh_timestamp_changed() {
        let _env = TestEnv::new();
        // Simulates codex CLI updating only the last_refresh timestamp
        let val = realistic_auth_json("user@example.com", "acct_u", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("ts-profile")).unwrap();
        // Same tokens, different timestamp
        let mut updated = realistic_auth_json("user@example.com", "acct_u", "acc_1", "ref_1");
        updated["last_refresh"] = serde_json::json!("2026-04-08T12:00:00Z");
        write_auth_durable(&live, &updated);
        match super::detect_auth_change().unwrap() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "ts-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    // ── Identity matching edge cases ─────────────────────────

    #[test]
    fn detect_tokens_updated_email_case_insensitive() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("User@Example.COM", "acct_u", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("case-profile")).unwrap();
        // Same email different case, new token
        let updated = realistic_auth_json("user@example.com", "acct_u", "acc_2", "ref_2");
        write_auth_durable(&live, &updated);
        match super::detect_auth_change().unwrap() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "case-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    #[test]
    fn detect_auth_change_refuses_single_email_match_without_account_id() {
        let _env = TestEnv::new();
        // Profile saved with account_id
        let val = realistic_auth_json("legacy@example.com", "acct_fb", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        super::cmd_save(Some("fb-profile")).unwrap();
        // Live auth.json has no account_id in JWT claims (email-only match)
        let claims_no_id = serde_json::json!({
            "email": "legacy@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let json_bytes = serde_json::to_vec(&claims_no_id).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json_bytes)
        };
        let jwt_no_id = format!("x.{encoded}.y");
        // account_id is empty string — should be treated as None after fix
        let updated = serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt_no_id,
                "access_token": "acc_new",
                "refresh_token": "ref_new",
                "account_id": "",
            },
            "last_refresh": "2026-04-08T00:00:00Z"
        });
        write_auth_durable(&live, &updated);
        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::UnresolvedIdentity { aliases }
                if aliases == ["fb-profile"]
        ));
    }

    #[test]
    fn detect_auth_change_refuses_account_id_match_when_live_email_is_missing() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json("alice@example.com", "acct_a", "old", "old-refresh"),
        );
        write_live(&auth_json_without_email("acct_a", "new", "new-refresh"));

        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::UnresolvedIdentity { aliases } if aliases == ["alice"]
        ));
    }

    #[test]
    fn detect_auth_change_refuses_account_id_match_when_saved_email_is_missing() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &auth_json_without_email("acct_a", "old", "old-refresh"),
        );
        write_live(&realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "new",
            "new-refresh",
        ));

        assert!(matches!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::UnresolvedIdentity { aliases } if aliases == ["alice"]
        ));
    }

    #[test]
    fn detect_auth_change_keeps_complete_different_emails_as_a_new_account() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json(
                "alice@example.com",
                "shared-workspace",
                "old",
                "old-refresh",
            ),
        );
        write_live(&realistic_auth_json(
            "bob@example.com",
            "shared-workspace",
            "new",
            "new-refresh",
        ));

        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::NewAccount
        );
    }

    // ── update_profile_from_live ─────────────────────────────

    #[test]
    fn checked_current_marker_distinguishes_missing_from_corrupt_empty_state() {
        let _env = TestEnv::new();
        assert!(
            super::read_current_marker_snapshot_checked()
                .unwrap()
                .is_none()
        );

        std::fs::create_dir_all(crate::auth::app_home().unwrap()).unwrap();
        std::fs::write(crate::auth::current_file().unwrap(), "  \n").unwrap();
        let error = super::read_current_marker_snapshot_checked()
            .expect_err("a present empty marker must not become a missing marker fallback");
        assert!(error.to_string().contains("is empty"), "{error:#}");
    }

    #[test]
    fn guarded_post_command_sync_rejects_a_changed_legacy_duplicate_marker() {
        let _env = TestEnv::new();
        let original = stamped_auth_json(
            "same@example.com",
            "acct_same",
            "old",
            "old-refresh",
            Some("2026-07-20T00:00:00Z"),
        );
        seed_profile("startup", &original);
        seed_profile("duplicate", &original);
        super::write_current("startup").unwrap();
        let expected = super::read_current_marker_snapshot_checked()
            .unwrap()
            .unwrap();
        write_live(&stamped_auth_json(
            "same@example.com",
            "acct_same",
            "new",
            "new-refresh",
            Some("2026-07-21T00:00:00Z"),
        ));

        super::write_current("duplicate").unwrap();
        let error = super::update_profile_from_live_if_current_marker("startup", &expected)
            .expect_err("a newer marker must win over stale post-command synchronization");

        assert!(
            error.to_string().contains("current profile marker changed"),
            "{error:#}"
        );
        assert_eq!(profile_refresh_token("startup"), "old-refresh");
        assert_eq!(current_alias(), "duplicate");
    }

    #[test]
    fn update_profile_from_live_syncs_content_and_preserves_others() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();

        // Create two profiles
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        write_auth_durable(&live, &alice);
        super::cmd_save(Some("alice")).unwrap();
        let bob = realistic_auth_json("bob@example.com", "acct_b", "acc_b1", "ref_b1");
        write_auth_durable(&live, &bob);
        super::cmd_save(Some("bob")).unwrap();

        // Update live with new alice tokens. The stamp has to move forward: a
        // rotated refresh_token may only overwrite the stored one when the live
        // copy can prove it is the newer of the two.
        let alice_updated = stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_a2",
            "ref_a2",
            Some("2026-04-09T00:00:00Z"),
        );
        write_auth_durable(&live, &alice_updated);
        super::update_profile_from_live("alice").unwrap();

        // Verify: alice's profile file content matches updated live
        let profile_val =
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(profile_val["tokens"]["access_token"], "acc_a2");
        assert_eq!(profile_val["tokens"]["refresh_token"], "ref_a2");
        assert_eq!(profile_val["OPENAI_API_KEY"], serde_json::Value::Null);

        // Verify: bob's profile was NOT modified
        let bob_val = crate::auth::read_auth(&super::profile_auth_path("bob").unwrap()).unwrap();
        assert_eq!(bob_val["tokens"]["access_token"], "acc_b1");

        // Verify: current marker updated
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn update_profile_from_live_rejects_different_account_identity() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        write_auth_durable(&live, &alice);
        super::cmd_save(Some("alice")).unwrap();

        let bob = realistic_auth_json("bob@example.com", "acct_b", "acc_b1", "ref_b1");
        write_auth_durable(&live, &bob);

        let result = super::update_profile_from_live("alice");
        assert!(result.is_err());
        let saved = crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "acc_a1");
    }

    #[test]
    fn relogin_rejects_different_account_identity() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        write_auth_durable(&live, &alice);
        super::cmd_save(Some("alice")).unwrap();

        let bob = realistic_auth_json("bob@example.com", "acct_b", "acc_b1", "ref_b1");
        let lease = super::acquire_profile_lease("alice").unwrap();
        let prepared = super::prepare_profile_reauth_with_lease(&lease).unwrap();
        let result =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &bob, false);
        assert!(result.is_err());
        let saved = crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "acc_a1");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prepared_relogin_does_not_retain_the_profile_lease() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        seed_profile("alice", &alice);
        let prepared = {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };

        let replacement = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            super::acquire_profile_lease_async("alice"),
        )
        .await
        .expect("interactive re-login preparation must release the alias lease")
        .unwrap();

        assert_eq!(prepared.email(), Some("alice@example.com"));
        drop(replacement);
    }

    #[test]
    fn prepared_relogin_accepts_a_same_identity_rotation_during_oauth() {
        let _env = TestEnv::new();
        let old = realistic_auth_json("alice@example.com", "acct_a", "old-access", "old-refresh");
        seed_profile("alice", &old);
        let prepared = {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let rotated = realistic_auth_json(
            "Alice@example.com",
            "acct_a",
            "rotated-access",
            "rotated-refresh",
        );
        write_auth_durable(&super::profile_auth_path("alice").unwrap(), &rotated);
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("alice").unwrap();
        super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, false).unwrap();

        assert_eq!(profile_access_token("alice"), "oauth-access");
        assert_eq!(profile_refresh_token("alice"), "oauth-refresh");
    }

    #[test]
    fn complete_relogin_updates_exact_live_auth_despite_a_stale_marker() {
        let _env = TestEnv::new();
        let old = realistic_auth_json("alice@example.com", "acct_a", "old-access", "old-refresh");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "bob-access", "bob-refresh");
        seed_profile("alice", &old);
        seed_profile("bob", &bob);
        write_live(&old);
        super::write_current("bob").unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("alice").unwrap();
        super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, false).unwrap();

        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap(),
            oauth
        );
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            oauth
        );
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn strict_relogin_revalidates_the_profile_inside_the_legacy_auth_transaction() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "old-access", "old-refresh");
        seed_profile("alice", &alice);
        let prepared = {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );
        let transaction = super::lock_auth_transaction().unwrap();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("legacy launch compatibility", attempt_tx);
            let lease = super::acquire_profile_lease("alice").unwrap();
            let result =
                super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, false);
            let _ = done_tx.send(result);
        });
        let mut cleanup = ThreadCleanup::new(transaction);
        cleanup.push(worker);
        attempt_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("re-login did not wait for the legacy-compatible auth transaction");

        let rebound = realistic_auth_json("bob@example.com", "acct_b", "bob-access", "bob-refresh");
        let profile_path = super::profile_auth_path("alice").unwrap();
        write_auth_durable(&profile_path, &rebound);
        cleanup.release_blocker();

        let error = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("re-login did not finish after the auth transaction was released")
            .expect_err("a legacy identity replacement must win over stale re-login proof");
        assert!(
            format!("{error:#}").contains("changed account identity"),
            "{error:#}"
        );
        assert_eq!(crate::auth::read_auth(&profile_path).unwrap(), rebound);
        cleanup.join_all();
    }

    #[test]
    fn prepared_relogin_rejects_an_alias_rebound_during_oauth() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "old-access", "old-refresh");
        seed_profile("target", &alice);
        let prepared = {
            let lease = super::acquire_profile_lease("target").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let rebound = realistic_auth_json("bob@example.com", "acct_b", "bob-access", "bob-refresh");
        let target_path = super::profile_auth_path("target").unwrap();
        write_auth_durable(&target_path, &rebound);

        let lease = super::acquire_profile_lease("target").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &rebound, false)
                .expect_err("a rebound alias must not adopt the new owner's OAuth credential");

        assert!(
            format!("{error:#}").contains("changed account identity"),
            "{error:#}"
        );
        assert_eq!(crate::auth::read_auth(&target_path).unwrap(), rebound);
    }

    #[test]
    fn prepared_relogin_does_not_recreate_a_deleted_profile() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "old-access", "old-refresh");
        seed_profile("alice", &alice);
        let prepared = {
            let lease = super::acquire_profile_lease("alice").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let profile_path = super::profile_auth_path("alice").unwrap();
        std::fs::remove_file(&profile_path).unwrap();
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("alice").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, false)
                .expect_err("a deleted profile must stay deleted after OAuth");

        assert!(error.downcast_ref::<crate::error::CsError>().is_some());
        assert!(!profile_path.exists());
    }

    #[test]
    fn prepared_relogin_archives_and_recovers_an_incomplete_legacy_identity() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        let original_bytes = std::fs::read(super::profile_auth_path("legacy").unwrap()).unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            let prepared = super::prepare_profile_reauth_with_lease(&lease).unwrap();
            assert!(prepared.requires_recoverable_replacement());
            prepared
        };
        let oauth = realistic_auth_json(
            "Alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let outcome =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .unwrap();
        let archive_path = outcome
            .archive_path()
            .expect("legacy recovery must report its exact archive");

        assert_eq!(
            crate::auth::read_auth(&archive_path.join("auth.json")).unwrap(),
            incomplete
        );
        assert_eq!(
            std::fs::read(archive_path.join("auth.json")).unwrap(),
            original_bytes,
            "the archive must preserve the exact previous credential bytes"
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            oauth
        );
    }

    #[test]
    fn incomplete_relogin_requires_explicit_recoverable_replacement_confirmation() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, false)
                .expect_err("an incomplete profile must not be replaced without confirmation");

        assert!(format!("{error:#}").contains("explicit confirmation"));
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            incomplete
        );
        assert!(!super::deleted_profiles_dir().unwrap().exists());
    }

    #[test]
    fn incomplete_relogin_updates_exact_active_live_auth_after_archiving() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        write_live(&incomplete);
        super::write_current("legacy").unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let outcome =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .unwrap();

        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            oauth
        );
        assert_eq!(current_alias(), "legacy");
        assert_eq!(
            crate::auth::read_auth(&outcome.archive_path().unwrap().join("auth.json")).unwrap(),
            incomplete
        );
    }

    #[test]
    fn incomplete_relogin_reports_visible_profile_with_unconfirmed_durability_exactly() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        write_live(&incomplete);
        super::write_current("legacy").unwrap();
        let profile_path = super::profile_auth_path("legacy").unwrap();
        let original_bytes = std::fs::read(&profile_path).unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );
        crate::auth::fail_next_private_durability_confirmation();

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .expect_err("visible-but-unconfirmed profile publication must be reported");
        let archives = std::fs::read_dir(super::deleted_profiles_dir().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();

        assert_eq!(archives.len(), 1);
        assert_eq!(
            std::fs::read(archives[0].join("auth.json")).unwrap(),
            original_bytes
        );
        assert_eq!(crate::auth::read_auth(&profile_path).unwrap(), oauth);
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            incomplete,
            "live auth is not attempted until profile durability is confirmed"
        );
        let detail = format!("{error:#}");
        assert!(detail.contains("replacement did not complete"), "{detail}");
        assert!(
            detail.contains("now contains the new credentials"),
            "{detail}"
        );
        assert!(detail.contains("durable commit is incomplete"), "{detail}");
        assert!(
            detail.contains(&archives[0].display().to_string()),
            "{detail}"
        );
        assert!(!detail.contains("could not be replaced"), "{detail}");
    }

    #[test]
    fn incomplete_relogin_reports_live_publish_with_marker_failure_exactly() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        write_live(&incomplete);
        super::write_current("legacy").unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );
        super::fail_next_activation_marker_write();

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .expect_err("a marker failure after live publication must remain visible");
        let archives = std::fs::read_dir(super::deleted_profiles_dir().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();

        assert_eq!(archives.len(), 1);
        assert_eq!(
            crate::auth::read_auth(&archives[0].join("auth.json")).unwrap(),
            incomplete
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            oauth
        );
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            oauth,
            "live publication is never rolled back after the marker step fails"
        );
        let detail = format!("{error:#}");
        assert!(
            detail.contains("live activation did not fully complete"),
            "{detail}"
        );
        assert!(detail.contains("was published to live auth"), "{detail}");
        assert!(
            detail.contains(&archives[0].display().to_string()),
            "{detail}"
        );
        assert!(!detail.contains("could not be synchronized"), "{detail}");
    }

    #[test]
    fn incomplete_relogin_of_an_inactive_profile_preserves_the_active_account() {
        let _env = TestEnv::new();
        let active = realistic_auth_json("bob@example.com", "acct_b", "bob-access", "bob-refresh");
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("active", &active);
        seed_profile("legacy", &incomplete);
        super::switch_profile("active").unwrap();
        let live_before = std::fs::read(crate::auth::codex_auth_path().unwrap()).unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let outcome =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .unwrap();

        assert!(outcome.archive_path().unwrap().join("auth.json").exists());
        assert_eq!(current_alias(), "active");
        assert_eq!(
            std::fs::read(crate::auth::codex_auth_path().unwrap()).unwrap(),
            live_before,
            "recovering an inactive legacy profile must not touch live Codex auth"
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            oauth
        );
    }

    #[test]
    fn incomplete_relogin_archive_failure_leaves_every_credential_unchanged() {
        let _env = TestEnv::new();
        let active = realistic_auth_json("bob@example.com", "acct_b", "bob-access", "bob-refresh");
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("active", &active);
        seed_profile("legacy", &incomplete);
        super::switch_profile("active").unwrap();
        let live_before = std::fs::read(crate::auth::codex_auth_path().unwrap()).unwrap();
        let profile_path = super::profile_auth_path("legacy").unwrap();
        let profile_before = std::fs::read(&profile_path).unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        std::fs::write(super::deleted_profiles_dir().unwrap(), b"blocked").unwrap();
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .expect_err(
                    "replacement must not start unless the previous credential is archived",
                );

        assert!(format!("{error:#}").contains("deleted-profiles"));
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
        assert_eq!(
            std::fs::read(crate::auth::codex_auth_path().unwrap()).unwrap(),
            live_before
        );
        assert_eq!(current_alias(), "active");
    }

    #[test]
    fn incomplete_relogin_marker_failure_reports_the_completed_archive_and_replacement() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        super::write_current("legacy").unwrap();
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let current_path = crate::auth::current_file().unwrap();
        std::fs::remove_file(&current_path).unwrap();
        std::fs::create_dir(&current_path).unwrap();
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .expect_err("a post-commit marker read failure must remain visible");
        let archives = std::fs::read_dir(super::deleted_profiles_dir().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();

        assert_eq!(archives.len(), 1);
        let detail = format!("{error:#}");
        assert!(detail.contains("profile 'legacy' was replaced"), "{detail}");
        assert!(
            detail.contains(&archives[0].display().to_string()),
            "{detail}"
        );
        assert_eq!(
            crate::auth::read_auth(&archives[0].join("auth.json")).unwrap(),
            incomplete
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            oauth
        );
    }

    #[test]
    fn incomplete_relogin_rejects_a_known_identity_mismatch_without_archiving() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let wrong =
            realistic_auth_json("bob@example.com", "acct_b", "oauth-access", "oauth-refresh");

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &wrong, true)
                .expect_err("known legacy identity fields must still match");

        assert!(format!("{error:#}").contains("known identity fields"));
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            incomplete
        );
        assert!(!super::deleted_profiles_dir().unwrap().exists());
    }

    #[test]
    fn incomplete_relogin_rejects_same_bytes_recreated_during_oauth() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let profile_path = super::profile_auth_path("legacy").unwrap();
        std::fs::remove_file(&profile_path).unwrap();
        write_auth_durable(&profile_path, &incomplete);
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .expect_err("same-byte delete/recreate must not satisfy the prepared file token");

        assert!(format!("{error:#}").contains("changed while re-login"));
        assert_eq!(crate::auth::read_auth(&profile_path).unwrap(), incomplete);
        assert!(!super::deleted_profiles_dir().unwrap().exists());
    }

    #[test]
    fn incomplete_relogin_does_not_duplicate_an_existing_complete_identity() {
        let _env = TestEnv::new();
        let incomplete = realistic_auth_json("alice@example.com", "", "old-access", "old-refresh");
        seed_profile("legacy", &incomplete);
        let complete = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "existing-access",
            "existing-refresh",
        );
        seed_profile("existing", &complete);
        let prepared = {
            let lease = super::acquire_profile_lease("legacy").unwrap();
            super::prepare_profile_reauth_with_lease(&lease).unwrap()
        };
        let oauth = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "oauth-access",
            "oauth-refresh",
        );

        let lease = super::acquire_profile_lease("legacy").unwrap();
        let error =
            super::commit_prepared_profile_reauth_with_lease(prepared, &lease, &oauth, true)
                .expect_err("legacy recovery must not duplicate an existing strict identity");

        assert!(format!("{error:#}").contains("already belongs to profile(s) existing"));
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("legacy").unwrap()).unwrap(),
            incomplete
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("existing").unwrap()).unwrap(),
            complete
        );
        assert!(!super::deleted_profiles_dir().unwrap().exists());
    }

    // ── Failure paths ────────────────────────────────────────

    #[test]
    fn update_profile_from_live_fails_when_no_auth_file() {
        let _env = TestEnv::new();
        // No live auth.json exists
        let result = super::update_profile_from_live("nonexistent");
        assert!(result.is_err());
    }

    // ── Rollback protection (one-time-rotation refresh tokens) ──

    /// `realistic_auth_json` with an explicit (or absent) `last_refresh` stamp.
    fn stamped_auth_json(
        email: &str,
        account_id: &str,
        access_token: &str,
        refresh_token: &str,
        last_refresh: Option<&str>,
    ) -> serde_json::Value {
        let mut val = realistic_auth_json(email, account_id, access_token, refresh_token);
        match last_refresh {
            Some(ts) => val["last_refresh"] = serde_json::json!(ts),
            None => {
                val.as_object_mut().unwrap().remove("last_refresh");
            }
        }
        val
    }

    fn seed_profile(alias: &str, val: &serde_json::Value) {
        let path = super::profile_auth_path(alias).unwrap();
        super::ensure_profile_parent(&path).unwrap();
        write_auth_durable(&path, val);
    }

    fn write_live(val: &serde_json::Value) {
        write_auth_durable(&crate::auth::codex_auth_path().unwrap(), val);
    }

    fn profile_refresh_token(alias: &str) -> String {
        crate::auth::read_auth(&super::profile_auth_path(alias).unwrap()).unwrap()["tokens"]
            ["refresh_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn profile_access_token(alias: &str) -> String {
        crate::auth::read_auth(&super::profile_auth_path(alias).unwrap()).unwrap()["tokens"]
            ["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn tui_reconciliation_updates_only_a_strict_newer_identity_match() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "old-access",
                "old-refresh",
                Some("2026-08-25T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "new-access",
            "new-refresh",
            Some("2026-08-26T00:00:00Z"),
        ));

        let super::TuiAuthReconciliation::ProfileUpdated { alias, info } =
            super::reconcile_live_auth_for_tui().unwrap()
        else {
            panic!("newer live credentials must update their strict profile")
        };
        assert_eq!(alias, "alice");
        assert_eq!(info.email.as_deref(), Some("alice@example.com"));
        assert_eq!(info.account_id.as_deref(), Some("acct_a"));
        assert_eq!(profile_access_token("alice"), "new-access");
        assert_eq!(profile_refresh_token("alice"), "new-refresh");
        assert_eq!(
            super::read_current_checked().unwrap().as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn tui_reconciliation_never_auto_saves_an_untracked_account() {
        let _env = TestEnv::new();
        let saved = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "alice-access",
            "alice-refresh",
        );
        seed_profile("alice", &saved);
        write_live(&realistic_auth_json(
            "foreign@example.com",
            "acct_foreign",
            "foreign-access",
            "foreign-refresh",
        ));

        assert_eq!(
            super::reconcile_live_auth_for_tui().unwrap(),
            super::TuiAuthReconciliation::UntrackedAccount
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap(),
            saved
        );
        assert_eq!(super::list_profiles().unwrap(), vec!["alice"]);
    }

    #[test]
    fn tui_reconciliation_reports_unidentified_live_auth() {
        let _env = TestEnv::new();
        let saved = realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "alice-access",
            "alice-refresh",
        );
        seed_profile("alice", &saved);
        write_live(&auth_json_without_identity(
            "unknown-access",
            "unknown-refresh",
        ));

        assert_eq!(
            super::reconcile_live_auth_for_tui().unwrap(),
            super::TuiAuthReconciliation::UnidentifiedAccount
        );
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap(),
            saved
        );
    }

    #[test]
    fn tui_reconciliation_leaves_exact_live_credentials_untouched() {
        let _env = TestEnv::new();
        let auth =
            realistic_auth_json("alice@example.com", "acct_a", "same-access", "same-refresh");
        seed_profile("alice", &auth);
        write_live(&auth);

        assert_eq!(
            super::reconcile_live_auth_for_tui().unwrap(),
            super::TuiAuthReconciliation::NoChange
        );
        assert_eq!(profile_access_token("alice"), "same-access");
    }

    /// The rollback guard is typed so callers can recognise it without matching
    /// on wording; every entry point must reject through that same type.
    fn assert_rollback_refusal(err: &anyhow::Error) -> &super::StaleLiveAuth {
        err.downcast_ref::<super::StaleLiveAuth>()
            .unwrap_or_else(|| panic!("the refusal must stay downcastable, got: {err:#}"))
    }

    #[test]
    fn update_profile_from_live_rejects_live_older_than_profile() {
        let _env = TestEnv::new();
        // The profile already holds a rotated refresh token; live still holds the
        // dead predecessor. Copying live over the profile would destroy the only
        // usable credential for this account.
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_new",
                "ref_new",
                Some("2026-07-28T04:51:15Z"),
            ),
        );
        super::write_current("bob").unwrap();
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        // Typed, so a caller deciding whether to show this to the user does not
        // have to match on the wording.
        let stale = err
            .downcast_ref::<super::StaleLiveAuth>()
            .unwrap_or_else(|| panic!("the refusal must stay downcastable, got: {err:#}"));
        assert_eq!(stale.alias, "alice");
        assert!(
            err.to_string().contains("older"),
            "error must explain the inverted direction, got: {err}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(
            current_alias(),
            "bob",
            "a rejected read-back must not repoint the current profile"
        );
    }

    #[test]
    fn update_profile_from_live_accepts_live_newer_than_profile() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-28T04:51:15Z"),
        ));

        super::update_profile_from_live("alice").unwrap();
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn update_profile_from_live_retries_the_exact_newer_live_snapshot() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_first",
            "ref_first",
            Some("2026-07-21T00:00:00Z"),
        ));
        let newest = stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_newest",
            "ref_newest",
            Some("2026-07-22T00:00:00Z"),
        );
        let newest_for_hook = newest.clone();
        super::after_next_update_profile_write(move || write_live(&newest_for_hook));

        super::update_profile_from_live("alice").unwrap();

        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap(),
            newest
        );
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            newest,
            "read-back synchronization must never rewrite live auth"
        );
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn update_profile_from_live_allows_same_refresh_token_without_any_timestamp() {
        let _env = TestEnv::new();
        // Legacy profile without a stamp, and the refresh token did not rotate:
        // the write cannot revoke anything, so the ordinary sync must not be
        // blocked just because neither side can be ordered in time.
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_same", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_same",
            None,
        ));

        super::update_profile_from_live("alice").unwrap();
        assert_eq!(profile_access_token("alice"), "acc_new");
    }

    #[test]
    fn update_profile_from_live_refuses_rotated_token_when_profile_has_no_timestamp() {
        let _env = TestEnv::new();
        // A legacy profile carries no stamp, so nothing orders it against the
        // live copy. The refresh tokens differ, which means exactly one of them
        // is still valid — guessing would destroy the other.
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_old", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        assert_rollback_refusal(&err);
        assert!(
            err.to_string().contains("no last_refresh"),
            "the message must say the profile carries no stamp, got: {err}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn update_profile_from_live_refuses_rotated_token_when_profile_timestamp_is_unparseable() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("not-a-timestamp"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        assert_rollback_refusal(&err);
        assert!(
            err.to_string().contains("not-a-timestamp"),
            "the message must echo the unusable stamp, got: {err}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn update_profile_from_live_refuses_rotated_token_when_timestamps_are_equal() {
        let _env = TestEnv::new();
        // Equal wall-clock stamps (the field has second resolution) prove
        // nothing about which rotation happened first.
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        assert_rollback_refusal(&err);
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn update_profile_from_live_rejects_unstamped_live_against_stamped_profile() {
        let _env = TestEnv::new();
        // A stamped profile records a known refresh time; an unstamped live file
        // cannot prove it is at least as fresh, so the copy is refused.
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_new",
                "ref_new",
                Some("2026-07-28T04:51:15Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            None,
        ));

        assert!(super::update_profile_from_live("alice").is_err());
        assert_eq!(profile_refresh_token("alice"), "ref_new");
    }

    // ── Read-back identity ambiguity (same email, several workspaces) ──

    fn jwt_without_account_id(email: &str) -> String {
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let json = serde_json::to_vec(&claims).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json)
        };
        format!("x.{encoded}.y")
    }

    fn auth_json_without_account_id(
        email: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt_without_account_id(email),
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": "",
            },
            "last_refresh": "2026-07-20T00:00:00Z"
        })
    }

    #[test]
    fn detect_auth_change_refuses_to_guess_between_same_email_workspaces() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@example.com", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@example.com", "acct_personal", "acc_p", "ref_p"),
        );
        // Live file carries no account_id — the email alone matches both profiles.
        write_live(&auth_json_without_account_id(
            "oai001@example.com",
            "acc_live",
            "ref_live",
        ));

        match super::detect_auth_change().unwrap() {
            super::AuthChange::UnresolvedIdentity { aliases }
                if aliases == ["oai001", "oai001_20x"] => {}
            other => panic!("ambiguous email match must not select a profile, got {other:?}"),
        }
    }

    #[test]
    fn detect_auth_change_picks_workspace_profile_by_account_id() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@example.com", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@example.com", "acct_personal", "acc_p", "ref_p"),
        );
        write_live(&realistic_auth_json(
            "oai001@example.com",
            "acct_personal",
            "acc_p2",
            "ref_p2",
        ));

        match super::detect_auth_change().unwrap() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "oai001_20x"),
            other => panic!("expected TokensUpdated for oai001_20x, got {other:?}"),
        }
    }

    #[test]
    fn detect_auth_change_uses_current_marker_to_disambiguate_legacy_exact_identities() {
        let _env = TestEnv::new();
        seed_profile(
            "first",
            &realistic_auth_json("same@example.com", "acct_same", "first", "first-ref"),
        );
        seed_profile(
            "second",
            &realistic_auth_json("same@example.com", "acct_same", "second", "second-ref"),
        );
        super::write_current("second").unwrap();
        write_live(&stamped_auth_json(
            "same@example.com",
            "acct_same",
            "codex-rotated",
            "codex-rotated-ref",
            Some("2026-04-08T00:00:00Z"),
        ));

        match super::detect_auth_change().unwrap() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "second"),
            other => panic!("current marker must disambiguate legacy identities, got {other:?}"),
        }
    }

    // ── cmd_save identity ambiguity (same email, several workspaces) ──

    #[test]
    fn cmd_save_ambiguous_email_refuses_to_guess_between_profiles() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@example.com", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@example.com", "acct_personal", "acc_p", "ref_p"),
        );
        // Live file carries no account_id — the email alone matches both profiles.
        write_live(&auth_json_without_account_id(
            "oai001@example.com",
            "acc_live",
            "ref_live",
        ));

        let err = cmd_save(None).expect_err("ambiguous email match must not silently save");
        let msg = err.to_string();
        assert!(
            msg.contains("oai001"),
            "message should list candidate: {msg}"
        );
        assert!(
            msg.contains("oai001_20x"),
            "message should list candidate: {msg}"
        );
        assert!(
            msg.contains("codex-switch-global-pace login <alias>"),
            "message should name a command that still exists: {msg}"
        );
        assert!(
            !msg.contains(" save "),
            "removed command leaked into: {msg}"
        );

        // Neither existing profile was silently overwritten.
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
    }

    #[test]
    fn cmd_save_refuses_legacy_duplicate_exact_identities() {
        let _env = TestEnv::new();
        seed_profile(
            "first",
            &stamped_auth_json(
                "same@example.com",
                "acct_same",
                "first-old",
                "first-ref",
                Some("2026-04-07T00:00:00Z"),
            ),
        );
        seed_profile(
            "second",
            &stamped_auth_json(
                "same@example.com",
                "acct_same",
                "second-old",
                "second-ref",
                Some("2026-04-07T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "same@example.com",
            "acct_same",
            "live-new",
            "live-ref",
            Some("2026-04-08T00:00:00Z"),
        ));

        let error = cmd_save(None).expect_err("duplicate exact identities need an explicit alias");
        let message = error.to_string();
        assert!(
            message.contains("first") && message.contains("second"),
            "{message}"
        );
        assert_eq!(profile_refresh_token("first"), "first-ref");
        assert_eq!(profile_refresh_token("second"), "second-ref");
    }

    #[test]
    fn cmd_save_exact_match_updates_the_right_profile() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@example.com", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@example.com", "acct_personal", "acc_p", "ref_p"),
        );
        write_live(&stamped_auth_json(
            "oai001@example.com",
            "acct_personal",
            "acc_p2",
            "ref_p2",
            Some("2026-04-09T00:00:00Z"),
        ));

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "oai001_20x"),
            other => panic!("expected exact match to update oai001_20x, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p2");
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
    }

    #[test]
    fn cmd_save_does_not_replace_a_malformed_existing_profile() {
        let _env = TestEnv::new();
        let path = super::profile_auth_path("corrupt").unwrap();
        super::ensure_profile_parent(&path).unwrap();
        std::fs::write(&path, b"{not-json").unwrap();
        write_live(&realistic_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
        ));

        let error =
            cmd_save(Some("corrupt")).expect_err("an unreadable existing profile must fail closed");
        assert!(format!("{error:#}").contains("parsing"), "{error:#}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{not-json");
    }

    #[test]
    fn cmd_save_refuses_single_email_match_without_account_id() {
        let _env = TestEnv::new();
        seed_profile(
            "legacy",
            &auth_json_without_account_id("legacy@example.com", "acc_old", "ref_old"),
        );
        let mut live = auth_json_without_account_id("legacy@example.com", "acc_new", "ref_new");
        live["last_refresh"] = serde_json::json!("2026-07-25T00:00:00Z");
        write_live(&live);

        let error = cmd_save(None).expect_err(
            "a single email match still must not replace credentials without account_id",
        );
        assert!(error.to_string().contains("account_id"), "{error:#}");
        assert_eq!(profile_refresh_token("legacy"), "ref_old");
    }

    #[test]
    fn cmd_save_refuses_single_account_id_match_without_email() {
        let _env = TestEnv::new();
        seed_profile(
            "legacy",
            &realistic_auth_json("legacy@example.com", "acct_legacy", "acc_old", "ref_old"),
        );
        let mut live = auth_json_without_email("acct_legacy", "acc_new", "ref_new");
        live["last_refresh"] = serde_json::json!("2026-07-25T00:00:00Z");
        write_live(&live);

        let error = cmd_save(None)
            .expect_err("a shared account_id must not replace credentials without email");
        assert!(
            error.to_string().contains("account_id or email"),
            "{error:#}"
        );
        assert_eq!(profile_refresh_token("legacy"), "ref_old");
    }

    // ── An explicit alias fixes destination, not identity proof ──

    /// A named profile must never redirect to its same-email twin, and naming it
    /// must never bypass complete account_id + email validation.
    fn seed_email_twins() {
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@example.com", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@example.com", "acct_personal", "acc_p", "ref_p"),
        );
    }

    #[test]
    fn cmd_save_with_explicit_alias_still_requires_complete_account_identity() {
        let _env = TestEnv::new();
        seed_email_twins();
        // No account_id on the live copy: the email alone matches both twins,
        // and "first candidate wins" would land on `oai001`.
        write_live(&auth_json_without_account_id(
            "oai001@example.com",
            "acc_live",
            "ref_live",
        ));

        let error = cmd_save(Some("oai001_20x"))
            .expect_err("an explicit alias cannot replace account_id validation");
        assert!(error.to_string().contains("account_id"), "{error:#}");
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
        assert_eq!(
            profile_refresh_token("oai001"),
            "ref_t",
            "the twin the user did not name must keep its credentials"
        );
    }

    #[test]
    fn save_imported_auth_value_rejects_email_only_credentials_even_with_explicit_alias() {
        let _env = TestEnv::new();
        seed_email_twins();
        let imported = auth_json_without_account_id("oai001@example.com", "acc_imp", "ref_imp");

        let err =
            super::save_imported_auth_value(&imported, Some("oai001_20x"), "acct_import", None)
                .expect_err("unverified JWT email must not select an existing profile");
        assert!(err.to_string().contains("non-empty account_id"));
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
    }

    #[test]
    fn save_imported_auth_value_does_not_overwrite_an_explicit_alias() {
        let _env = TestEnv::new();
        seed_email_twins();
        let imported =
            realistic_auth_json("oai001@example.com", "acct_attacker", "acc_imp", "ref_imp");
        let action =
            super::save_imported_auth_value(&imported, Some("oai001_20x"), "acct_attacker", None)
                .expect("an explicit alias collision should create a unique profile");
        match action {
            super::SaveAction::Created(alias) => assert_eq!(alias, "oai001_20x_2"),
            other => panic!("import must never update the named profile, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
        assert_eq!(profile_refresh_token("oai001_20x_2"), "ref_imp");
    }

    #[test]
    fn save_imported_auth_value_requires_usage_validated_account_id_match() {
        let _env = TestEnv::new();
        let imported = realistic_auth_json("alice@example.com", "acct_alice", "acc_imp", "ref_imp");
        let err = super::save_imported_auth_value(&imported, None, "acct_other", None)
            .expect_err("unverified JWT identity cannot replace validation evidence");
        assert!(err.to_string().contains("does not match Usage API"));
        assert!(super::list_profiles().unwrap().is_empty());
    }

    #[test]
    fn initial_rotation_recovery_stage_is_the_written_credential_file() {
        let _env = TestEnv::new();
        let rotated = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "access_latest",
            "refresh_latest",
        );

        let stage = super::stage_import_rotation(&rotated).unwrap();
        let files = recovery_files();

        assert_eq!(files, vec![stage.path().to_path_buf()]);
        assert!(stage.contains(&rotated).unwrap());
        let raw = std::fs::read(stage.path()).unwrap();
        assert!(
            !raw.is_empty(),
            "the owned stage must never be an empty reservation"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&raw).unwrap(),
            rotated
        );
    }

    #[test]
    fn validated_import_promotes_the_exact_rotation_stage_without_a_duplicate() {
        let _env = TestEnv::new();
        let imported = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "access_latest",
            "refresh_latest",
        );
        let stage = super::stage_import_rotation(&imported).unwrap();
        let stage_path = stage.path().to_path_buf();

        let committed = super::save_imported_auth_value_with_stage(
            &imported,
            Some("alice"),
            "acct_alice",
            None,
            Some(stage),
        )
        .unwrap();
        let super::ValidatedImportCommit::Profile(action) = committed else {
            panic!("validated stage promotion must create the profile")
        };

        assert!(matches!(action.action, super::SaveAction::Created(ref alias) if alias == "alice"));
        assert!(action.profile_commit.is_none());
        assert!(action.recovery_cleanup.is_none());
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap(),
            imported
        );
        assert!(
            !stage_path.exists(),
            "successful no-clobber publication must remove the exact recovery stage instead of leaving a duplicate"
        );
        let recovery_entries = std::fs::read_dir(crate::auth::app_home().unwrap().join("recovery"))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert!(recovery_entries.is_empty());
    }

    #[test]
    fn committed_import_reports_cleanup_only_failure_without_claiming_a_foreign_path() {
        let _env = TestEnv::new();
        let imported = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "access_latest",
            "refresh_latest",
        );
        let stage = super::stage_import_rotation(&imported).unwrap();
        let stage_path = stage.path().to_path_buf();
        let replaced_path = stage_path.clone();
        super::before_next_import_recovery_cleanup(move || {
            std::fs::remove_file(&replaced_path).unwrap();
            std::fs::write(&replaced_path, b"foreign-replacement").unwrap();
        });

        let committed = super::save_imported_auth_value_with_stage(
            &imported,
            Some("alice"),
            "acct_alice",
            None,
            Some(stage),
        )
        .unwrap();
        let super::ValidatedImportCommit::Profile(outcome) = committed else {
            panic!("cleanup failure happens after the durable profile commit")
        };

        assert!(
            matches!(outcome.action, super::SaveAction::Created(ref alias) if alias == "alice")
        );
        assert!(outcome.profile_commit.is_none());
        let cleanup = outcome
            .recovery_cleanup
            .expect("profile publication succeeded, so only stage cleanup is incomplete");
        assert!(cleanup.recovery_path.is_none());
        assert!(format!("{:#}", cleanup.cause).contains("durably published"));
        assert_eq!(
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap(),
            imported
        );
        assert_eq!(std::fs::read(stage_path).unwrap(), b"foreign-replacement");
    }

    #[test]
    fn rotated_import_promotion_never_overwrites_a_concurrent_profile() {
        let _env = TestEnv::new();
        let imported = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "access_latest",
            "refresh_latest",
        );
        let stage = super::stage_import_rotation(&imported).unwrap();
        let stage_path = stage.path().to_path_buf();
        let destination = super::profile_auth_path("alice").unwrap();
        let writer_destination = destination.clone();
        super::before_next_import_promotion(move || {
            std::fs::write(&writer_destination, b"foreign-profile").unwrap();
        });

        let committed = super::save_imported_auth_value_with_stage(
            &imported,
            Some("alice"),
            "acct_alice",
            None,
            Some(stage),
        )
        .unwrap();
        let super::ValidatedImportCommit::RecoveryPreserved {
            recovery_path,
            cause,
        } = committed
        else {
            panic!("a destination collision must preserve the rotated stage")
        };

        assert!(format!("{cause:#}").contains("without replacement"));
        assert_eq!(recovery_path.as_deref(), Some(stage_path.as_path()));
        assert_eq!(std::fs::read(&destination).unwrap(), b"foreign-profile");
        assert_eq!(crate::auth::read_auth(&stage_path).unwrap(), imported);
    }

    #[test]
    fn rotated_import_persist_preserves_a_replaced_stage_and_adopts_the_latest_copy() {
        let _env = TestEnv::new();
        let earlier =
            realistic_auth_json("alice@example.com", "acct_alice", "access_1", "refresh_1");
        let latest =
            realistic_auth_json("alice@example.com", "acct_alice", "access_2", "refresh_2");
        let mut stage = super::stage_import_rotation(&earlier).unwrap();
        let replaced_path = stage.path().to_path_buf();
        std::fs::write(&replaced_path, b"foreign-stage").unwrap();

        let error = stage.persist(&latest).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("changed before exact replacement")
        );
        assert_eq!(std::fs::read(&replaced_path).unwrap(), b"foreign-stage");
        assert_ne!(stage.path(), replaced_path);
        assert!(stage.contains(&latest).unwrap());
    }

    #[test]
    fn recovered_import_normalizes_the_latest_rotation_before_registry_lock_failure() {
        let _env = TestEnv::new();
        let earlier =
            realistic_auth_json("alice@example.com", "acct_alice", "access_1", "refresh_1");
        let latest =
            realistic_auth_json("alice@example.com", "acct_alice", "access_2", "refresh_2");
        let stale_stage = super::stage_import_rotation(&earlier).unwrap();
        let stale_path = stale_stage.path().to_path_buf();

        // Make the production auth-lock open fail immediately. Recovery must
        // already have written RT2 to a new private stage before it reaches
        // this profile-registry transaction boundary.
        std::fs::create_dir_all(super::auth_lock_path().unwrap()).unwrap();
        let action = super::save_recovered_import_auth_value_with_stage(
            latest.clone(),
            Some("recovered"),
            None,
            Some(stale_stage),
        )
        .expect("a busy registry leaves the latest credentials recoverable");

        let super::RecoveredImportAction::RecoveryPreserved {
            recovery_path,
            reason,
        } = action
        else {
            panic!("registry-lock failure must retain a recovery stage")
        };
        let path = recovery_path.expect("the exact latest stage is still present");
        assert_ne!(path, stale_path, "the RT1 stage must not masquerade as RT2");
        assert_eq!(crate::auth::read_auth(&path).unwrap(), latest);
        assert!(!stale_path.exists(), "the superseded RT1 stage is removed");
        assert!(reason.contains("profile-registry transaction"), "{reason}");
        assert!(super::list_profiles().unwrap().is_empty());
    }

    #[test]
    fn imported_credentials_refuse_an_existing_exact_identity_without_overwriting() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json(
                "alice@example.com",
                "acct_shared_workspace",
                "access_existing",
                "refresh_existing",
            ),
        );
        let imported = realistic_auth_json(
            "alice@example.com",
            "acct_shared_workspace",
            "access_imported",
            "refresh_imported",
        );

        let error = super::save_imported_auth_value(
            &imported,
            None,
            "acct_shared_workspace",
            Some("alice"),
        )
        .expect_err("the same strict identity must not create a second alias");

        assert!(error.to_string().contains("profile 'alice'"), "{error:#}");
        assert!(
            error.to_string().contains("same account_id and email"),
            "{error:#}"
        );
        assert_eq!(profile_refresh_token("alice"), "refresh_existing");
        assert!(!super::profile_auth_path("alice_2").unwrap().exists());
    }

    #[test]
    fn refresh_token_cas_does_not_overwrite_a_concurrent_relogin() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json("alice@example.com", "acct_a", "old_access", "refresh_old"),
        );
        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        write_auth_durable(
            &super::profile_auth_path("alice").unwrap(),
            &realistic_auth_json("alice@example.com", "acct_a", "login_access", "refresh_new"),
        );
        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "refresh_old",
            &make_jwt("alice@example.com", "acct_a"),
            "stale_access",
            "stale_refresh",
        )
        .unwrap();
        let super::RefreshTokenUpdate::Superseded { recovery_path } = update else {
            panic!("a re-login that replaced the presented token must win")
        };
        assert_eq!(profile_refresh_token("alice"), "refresh_new");
        assert_eq!(
            crate::auth::read_auth(&recovery_path).unwrap()["tokens"]["refresh_token"],
            "stale_refresh"
        );

        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "refresh_new",
            &make_jwt("alice@example.com", "acct_a"),
            "fresh_access",
            "fresh_refresh",
        )
        .unwrap();
        assert!(matches!(update, super::RefreshTokenUpdate::Saved));
        assert_eq!(profile_refresh_token("alice"), "fresh_refresh");
    }

    #[test]
    fn refreshed_credentials_for_another_account_are_quarantined_without_rebinding() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();
        let profile_path = super::profile_auth_path("alice").unwrap();
        let live_path = crate::auth::codex_auth_path().unwrap();
        let profile_before = std::fs::read(&profile_path).unwrap();
        let live_before = std::fs::read(&live_path).unwrap();

        let lease = super::acquire_profile_lease("alice").unwrap();
        let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
        let update = super::update_profile_tokens_if_refresh_matches_leased(
            &lease,
            authorization,
            "a-ref",
            &make_jwt("bob@example.com", "acct_b"),
            "b-access",
            "b-refresh",
        )
        .expect("the rejected rotation must be preserved as a typed quarantine outcome");

        let super::RefreshTokenUpdate::Quarantined { path, cause } = update else {
            panic!("cross-account credentials must never be installed")
        };
        assert!(
            format!("{cause:#}").contains("another account"),
            "{cause:#}"
        );
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
        assert_eq!(std::fs::read(&live_path).unwrap(), live_before);
        let recovered = crate::auth::read_auth(&path).unwrap();
        assert_eq!(
            recovered
                .pointer("/tokens/refresh_token")
                .and_then(serde_json::Value::as_str),
            Some("b-refresh")
        );
        assert_eq!(
            super::extract_identity(&recovered).account_id.as_deref(),
            Some("acct_b")
        );
    }

    #[test]
    fn blank_refreshed_tokens_leave_profile_and_live_auth_unchanged() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &alice);
        super::switch_profile("alice").unwrap();
        let profile_path = super::profile_auth_path("alice").unwrap();
        let live_path = crate::auth::codex_auth_path().unwrap();
        let profile_before = std::fs::read(&profile_path).unwrap();
        let live_before = std::fs::read(&live_path).unwrap();
        let valid_id = make_jwt("alice@example.com", "acct_a");
        let lease = super::acquire_profile_lease("alice").unwrap();

        for (id_token, access_token, refresh_token, expected_field) in [
            ("", "a-new", "a-ref-new", "id_token"),
            (valid_id.as_str(), " \t", "a-ref-new", "access_token"),
            (valid_id.as_str(), "a-new", "\n", "refresh_token"),
        ] {
            let authorization = super::authorize_fresh_credentials_activation(&lease).unwrap();
            let error = super::update_profile_tokens_if_refresh_matches_leased(
                &lease,
                authorization,
                "a-ref",
                id_token,
                access_token,
                refresh_token,
            )
            .expect_err("blank refreshed tokens must be rejected before publication");
            assert!(error.to_string().contains(expected_field), "{error:#}");
            assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
            assert_eq!(std::fs::read(&live_path).unwrap(), live_before);
        }
    }

    #[test]
    fn refresh_authorization_rejects_a_blank_email_identity() {
        let _env = TestEnv::new();
        let auth = realistic_auth_json("   ", "acct_a", "a-old", "a-ref");
        seed_profile("alice", &auth);
        let lease = super::acquire_profile_lease("alice").unwrap();

        let error = super::authorize_fresh_credentials_activation(&lease)
            .err()
            .expect("blank email claims cannot establish a strict refresh identity pin");

        assert!(
            format!("{error:#}").contains("both account_id and email"),
            "{error:#}"
        );
    }

    #[test]
    fn switch_rejects_disallowed_managed_workspace_without_changing_live_auth() {
        let env = TestEnv::new();
        seed_profile(
            "blocked",
            &realistic_auth_json(
                "blocked@example.com",
                "workspace-blocked",
                "blocked_access",
                "blocked_refresh",
            ),
        );
        let original = realistic_auth_json(
            "allowed@example.com",
            "workspace-allowed",
            "live_access",
            "live_refresh",
        );
        seed_profile("allowed", &original);
        write_live(&original);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();

        let err = super::switch_profile("blocked").expect_err("managed policy must fail closed");
        assert!(err.to_string().contains("not allowed"));
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            original
        );
    }

    #[test]
    fn save_rejects_disallowed_managed_workspace_before_creating_profile() {
        let env = TestEnv::new();
        let blocked = realistic_auth_json(
            "blocked@example.com",
            "workspace-blocked",
            "blocked_access",
            "blocked_refresh",
        );
        write_live(&blocked);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();

        let err = super::cmd_save(Some("blocked"))
            .expect_err("managed policy must guard new profile creation");
        assert!(err.to_string().contains("not allowed"));
        assert!(!super::profile_auth_path("blocked").unwrap().exists());
    }

    // ── Rollback protection on the save/import entry points ──

    /// A profile holding the rotated token, plus a live copy still holding its
    /// already-revoked predecessor.
    fn seed_profile_ahead_of_live() {
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_new",
                "ref_new",
                Some("2026-07-28T04:51:15Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            Some("2026-07-20T00:00:00Z"),
        ));
    }

    #[test]
    fn cmd_save_refuses_to_roll_a_profile_back_to_a_revoked_token() {
        let _env = TestEnv::new();
        seed_profile_ahead_of_live();

        let named = cmd_save(Some("alice")).expect_err("an explicit alias must not skip the guard");
        assert_eq!(assert_rollback_refusal(&named).alias, "alice");
        assert_eq!(profile_refresh_token("alice"), "ref_new");

        let inferred = cmd_save(None).expect_err("the inferred target must not skip the guard");
        assert_rollback_refusal(&inferred);
        assert_eq!(profile_refresh_token("alice"), "ref_new");
    }

    #[test]
    fn save_imported_auth_value_refuses_a_stale_duplicate_without_overwriting() {
        let _env = TestEnv::new();
        seed_profile_ahead_of_live();
        // A stale auth.json dump on disk is the same hazard as a stale live file.
        let imported = stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            Some("2026-07-20T00:00:00Z"),
        );

        let error = super::save_imported_auth_value(&imported, None, "acct_a", None)
            .expect_err("a stale dump of the same strict identity must be refused");
        assert!(error.to_string().contains("profile 'alice'"), "{error:#}");
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert!(!super::profile_auth_path("alice_2").unwrap().exists());
    }

    #[test]
    fn cmd_save_allows_resave_when_the_refresh_token_did_not_rotate() {
        let _env = TestEnv::new();
        // Neither side is stamped, so nothing can be ordered — but the refresh
        // token is identical, so the write cannot revoke anything.
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_same", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_same",
            None,
        ));

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "alice"),
            other => panic!("an unrotated re-save must still go through, got {other:?}"),
        }
        assert_eq!(profile_access_token("alice"), "acc_new");
    }

    #[test]
    fn cmd_save_refuses_a_rotated_token_when_the_stamps_are_equal() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = cmd_save(None).expect_err("equal stamps cannot order two different tokens");
        assert_rollback_refusal(&err);
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn cmd_save_refuses_a_rotated_token_when_the_profile_has_no_stamp() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_old", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = cmd_save(None).expect_err("an unstamped profile cannot be proven older");
        let msg = err.to_string();
        assert_rollback_refusal(&err);
        assert!(
            msg.contains("no last_refresh"),
            "the message must name the profile's state, got: {msg}"
        );
        assert!(
            msg.contains("codex-switch-global-pace use alice"),
            "the message must offer a way out, got: {msg}"
        );
        assert!(
            msg.contains("codex-switch-global-pace login alice"),
            "the message must offer a valid re-authorization command, got: {msg}"
        );
        assert!(
            !msg.contains("codex-switch-global-pace save"),
            "removed command leaked into: {msg}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn cmd_save_updates_the_profile_when_live_is_provably_newer() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-28T04:51:15Z"),
        ));

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "alice"),
            other => panic!("the normal forward sync must still work, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(current_alias(), "alice");
    }

    #[test]
    fn detect_no_identity_in_jwt_returns_unidentified_account() {
        let _env = TestEnv::new();
        let val = auth_json_without_identity("acc_x", "ref_x");
        let live = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live, &val);
        assert_eq!(
            super::detect_auth_change().unwrap(),
            super::AuthChange::UnidentifiedAccount
        );
    }

    #[test]
    fn login_with_an_explicit_alias_never_writes_to_its_email_twin() {
        let _env = TestEnv::new();
        seed_email_twins();
        // Freshly minted credentials for the personal workspace. Resolving by
        // identity would land on `oai001_20x` and silently replace the working
        // token of a profile the user did not name.
        let minted =
            realistic_auth_json("oai001@example.com", "acct_personal", "acc_new", "ref_new");

        let err = super::save_auth_value(minted, Some("oai001"))
            .expect_err("a named profile holding another workspace must not be reassigned");
        assert!(
            format!("{err:#}").contains("oai001"),
            "the refusal must name the profile that was asked for: {err:#}"
        );
        assert_eq!(
            profile_refresh_token("oai001_20x"),
            "ref_p",
            "the twin the user did not name must keep its credentials"
        );
    }

    #[test]
    fn login_sink_rejects_incomplete_identity_before_profile_resolution() {
        let _env = TestEnv::new();
        seed_email_twins();
        let minted = auth_json_without_account_id("oai001@example.com", "acc_new", "ref_new");

        let err = super::save_auth_value(minted, None)
            .expect_err("an incomplete OAuth result must not reach profile resolution");
        assert!(
            format!("{err:#}").contains("both a non-empty account_id and email"),
            "{err:#}"
        );
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
    }

    #[test]
    fn login_sink_never_creates_an_incomplete_profile() {
        let _env = TestEnv::new();
        let minted = auth_json_without_account_id("alice@example.com", "acc_new", "ref_new");

        let error = super::save_auth_value(minted, Some("alice"))
            .expect_err("the persistence boundary must reject an incomplete OAuth identity");

        assert!(
            format!("{error:#}").contains("both a non-empty account_id and email"),
            "{error:#}"
        );
        assert!(!super::profile_auth_path("alice").unwrap().exists());
        assert!(!crate::auth::codex_auth_path().unwrap().exists());
    }

    #[test]
    fn login_updating_an_existing_profile_also_activates_it_live() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json(
            "alice@example.com",
            "acct_alice",
            "alice_access",
            "alice_refresh",
        );
        let bob = realistic_auth_json(
            "bob@example.com",
            "acct_bob",
            "bob_access_old",
            "bob_refresh_old",
        );
        seed_profile("alice", &alice);
        seed_profile("bob", &bob);
        write_live(&alice);
        let live_path = crate::auth::codex_auth_path().unwrap();
        super::write_current("alice").unwrap();

        let minted = realistic_auth_json(
            "bob@example.com",
            "acct_bob",
            "bob_access_new",
            "bob_refresh_new",
        );
        match super::save_auth_value(minted, None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "bob"),
            other => panic!("existing bob profile should be updated, got {other:?}"),
        }

        assert_eq!(profile_access_token("bob"), "bob_access_new");
        assert_eq!(profile_access_token("alice"), "alice_access");
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("bob_access_new"),
            "the account named by current must also own live auth.json"
        );
        assert_eq!(current_alias(), "bob");
        assert_eq!(
            super::find_matching_profile(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            Some("bob".to_string())
        );
        let live_backup = std::fs::read_dir(live_path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("auth.json.bak.")
            })
            .expect("activating login must back up the previous live credentials");
        let backup = crate::auth::read_auth(&live_backup.path()).unwrap();
        assert_eq!(
            backup
                .pointer("/tokens/access_token")
                .and_then(|value| value.as_str()),
            Some("alice_access")
        );
    }

    #[test]
    fn deleting_then_reusing_an_alias_does_not_inherit_the_old_accounts_cache() {
        let _env = TestEnv::new();
        let alias = "reused";
        seed_profile(
            alias,
            &realistic_auth_json("old@example.com", "acct_old", "old_access", "old_refresh"),
        );
        crate::cache::put(alias, &crate::usage::UsageInfo::default()).unwrap();
        crate::cache::try_set_last_used(alias).unwrap();
        crate::cache::put_auth_failure(
            alias,
            "old_refresh",
            &crate::usage::UsageError {
                summary: "old failure".to_string(),
                detail: "old failure detail".to_string(),
            },
        )
        .unwrap();
        assert!(crate::cache::get(alias).unwrap().is_some());
        assert_ne!(
            crate::cache::last_used_snapshot_checked()
                .unwrap()
                .get(alias)
                .copied()
                .unwrap_or(0),
            0
        );
        assert!(
            crate::cache::get_auth_failure(alias, "old_refresh")
                .unwrap()
                .is_some()
        );

        let _ = cmd_delete(alias).unwrap();
        seed_profile(
            alias,
            &realistic_auth_json("new@example.com", "acct_new", "new_access", "new_refresh"),
        );

        assert!(crate::cache::get(alias).unwrap().is_none());
        assert_eq!(
            crate::cache::last_used_snapshot_checked()
                .unwrap()
                .get(alias)
                .copied()
                .unwrap_or(0),
            0
        );
        assert!(
            crate::cache::get_auth_failure(alias, "old_refresh")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_protection_uses_live_profile_not_a_stale_marker() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref"),
        );
        seed_profile(
            "bob",
            &realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref"),
        );
        super::switch_profile("alice").unwrap();
        super::write_current("bob").unwrap();

        let error = super::cmd_delete("alice")
            .expect_err("live alice credentials must protect alice from deletion");
        assert!(error.to_string().contains("active profile"));
        assert!(super::profile_auth_path("alice").unwrap().exists());
    }

    #[test]
    fn stale_marker_does_not_block_deleting_a_different_live_profile() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref"),
        );
        seed_profile(
            "bob",
            &realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref"),
        );
        super::switch_profile("bob").unwrap();
        super::write_current("alice").unwrap();

        let _ = super::cmd_delete("alice").unwrap();
        assert!(!super::profile_auth_path("alice").unwrap().exists());
        assert_eq!(
            super::active_profile_from_live().unwrap().as_deref(),
            Some("bob")
        );
        assert_eq!(current_alias(), "bob");
    }

    #[test]
    fn login_replaces_an_unstamped_profile_because_the_credentials_are_new() {
        let _env = TestEnv::new();
        // A legacy profile with no last_refresh. The freshness gate would call
        // this unorderable and refuse — but re-login is exactly how a user
        // recovers such a profile, so it must not be blocked here.
        seed_profile(
            "legacy",
            &stamped_auth_json("legacy@example.com", "acct_l", "acc_old", "ref_old", None),
        );
        let minted = realistic_auth_json("legacy@example.com", "acct_l", "acc_fresh", "ref_fresh");

        match super::save_auth_value(minted, Some("legacy")) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "legacy"),
            other => panic!("re-login must be able to replace a legacy profile, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("legacy"), "ref_fresh");
    }
}
