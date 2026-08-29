use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::jwt::StrictAccountBinding;
use crate::usage::{ResetCredit, UsageError, UsageInfo, UsageParseIssue};

/// How long an authoritative workspace lookup is trusted.
///
/// Both a name and a confirmed absence can change without us: a workspace may
/// be renamed, and a personal account may join one. Applying one lifetime to
/// both answers keeps either change at most a day away — `--force` shows it
/// immediately.
const WORKSPACE_RESOLUTION_TTL: u64 = 24 * 60 * 60;

static CACHE_LOCK: Mutex<()> = Mutex::new(());
const CACHE_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CACHE_LOCK_PENDING: u8 = 0;
const CACHE_LOCK_CONTENDED: u8 = 1;
const CACHE_LOCK_ACQUIRED: u8 = 2;
const CACHE_LOCK_CANCELLED: u8 = 3;
const CACHE_LOCK_TIMED_OUT: u8 = 4;

#[cfg(test)]
thread_local! {
    static TEST_BEFORE_LAST_USED_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_next_last_used_write(action: impl FnOnce() + 'static) {
    TEST_BEFORE_LAST_USED_WRITE.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_before_last_used_write_test_hook() {
    TEST_BEFORE_LAST_USED_WRITE.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

/// Cancellation boundary for derived-cache work that is still waiting for the
/// cache lock. Once acquisition wins, cancellation is a no-op and the small
/// read or durable mutation is allowed to finish.
#[derive(Clone, Debug)]
pub(crate) struct CacheLockAcquireControl {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    wake: std::sync::Arc<tokio::sync::Notify>,
}

impl CacheLockAcquireControl {
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(CACHE_LOCK_PENDING)),
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn cancel_waiting(&self) -> bool {
        let cancelled = self
            .state
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |state| match state {
                    CACHE_LOCK_PENDING | CACHE_LOCK_CONTENDED => Some(CACHE_LOCK_CANCELLED),
                    _ => None,
                },
            )
            .is_ok();
        if cancelled {
            self.wake.notify_one();
        }
        cancelled
    }

    /// Cancel only after the worker has observed actual OS-lock contention.
    /// This narrower boundary lets opportunistic startup cache reads keep an
    /// uncontended hit even when another authoritative startup prerequisite
    /// happens to win the scheduler race.
    pub(crate) fn cancel_contended(&self) -> bool {
        let cancelled = self
            .state
            .compare_exchange(
                CACHE_LOCK_CONTENDED,
                CACHE_LOCK_CANCELLED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok();
        if cancelled {
            self.wake.notify_one();
        }
        cancelled
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::Acquire) == CACHE_LOCK_CANCELLED
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
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |state| match state {
                    CACHE_LOCK_PENDING | CACHE_LOCK_CONTENDED => Some(CACHE_LOCK_ACQUIRED),
                    _ => None,
                },
            )
            .is_ok()
    }

    fn mark_contended(&self) -> bool {
        match self.state.compare_exchange(
            CACHE_LOCK_PENDING,
            CACHE_LOCK_CONTENDED,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) | Err(CACHE_LOCK_CONTENDED) => true,
            Err(CACHE_LOCK_ACQUIRED | CACHE_LOCK_CANCELLED | CACHE_LOCK_TIMED_OUT) => false,
            Err(state) => unreachable!("unknown cache-lock acquisition state {state}"),
        }
    }

    fn mark_timed_out(&self) -> bool {
        self.state
            .compare_exchange(
                CACHE_LOCK_CONTENDED,
                CACHE_LOCK_TIMED_OUT,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    /// Stable owner of this usage observation.
    ///
    /// Older cache files do not carry these fields. They remain readable by
    /// the compatibility APIs, but identity-bound callers deliberately treat
    /// either missing component as a miss rather than assigning legacy data to
    /// whichever account currently owns the alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    ts: u64,
    primary_used: Option<f64>,
    primary_reset: Option<i64>,
    #[serde(default)]
    primary_window_minutes: Option<i64>,
    secondary_used: Option<f64>,
    secondary_reset: Option<i64>,
    #[serde(default)]
    secondary_window_minutes: Option<i64>,
    #[serde(default)]
    credits_balance: Option<f64>,
    #[serde(default)]
    unlimited_credits: Option<bool>,
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    reset_credits_available_count: Option<u64>,
    #[serde(default)]
    reset_credits: Vec<ResetCredit>,
    #[serde(default)]
    reset_credits_error: Option<String>,
    /// Whether the reset-card fields came from an authoritative reset-card
    /// lookup. Older cache generations predate quota-only auto-select entries,
    /// so a missing marker means complete.
    #[serde(
        default = "reset_metadata_complete_by_default",
        skip_serializing_if = "bool_is_true"
    )]
    reset_metadata_complete: bool,
    #[serde(default)]
    account_limited: bool,
    #[serde(default)]
    spend_control_reached: bool,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
    #[serde(default)]
    individual_limit: Option<Box<crate::usage::SpendControlLimit>>,
    #[serde(default)]
    additional_limits: Vec<crate::usage::AdditionalRateLimit>,
    #[serde(default)]
    parse_issues: Vec<UsageParseIssue>,
}

const fn reset_metadata_complete_by_default() -> bool {
    true
}

fn bool_is_true(value: &bool) -> bool {
    *value
}

/// A refusal the auth server will repeat for as long as the profile keeps the
/// credential it refused.
///
/// Unlike [`CacheEntry`] this carries no TTL. It is not a stale-data trade-off:
/// the server named a specific credential as spent, and that verdict can only
/// be undone by replacing the credential — which `credential` detects on its
/// own. Expiring the record on a timer would buy nothing but a periodic round
/// trip whose answer is already known.
#[derive(Serialize, Deserialize)]
struct AuthFailureEntry {
    ts: u64,
    /// Hex SHA-256 of the rejected `refresh_token`. The token itself is never
    /// stored: this file is not the credential store, and a fingerprint answers
    /// the only question asked of it — is this still the same credential?
    credential: String,
    summary: String,
    detail: String,
}

/// One authoritative workspace lookup. The presence of this entry means the
/// lookup was resolved; `name: None` is the server's equally authoritative
/// answer that the account currently has no workspace name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceCacheEntry {
    ts: u64,
    name: Option<String>,
}

/// Fresh workspace metadata from a cache snapshot.
///
/// `Unresolved` covers both a never-requested account and an expired answer.
/// Keeping it distinct from `Absent` prevents personal plans from being looked
/// up on every invocation while still allowing both positive and negative
/// answers to expire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceState {
    Unresolved,
    Named(String),
    Absent,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CacheSnapshot {
    pub(crate) usage: HashMap<String, UsageInfo>,
    pub(crate) workspaces: HashMap<String, WorkspaceState>,
    /// Remaining freshness for every resolved workspace value at snapshot
    /// creation time. Unresolved values have no entry here.
    pub(crate) workspace_fresh_for: HashMap<String, Duration>,
}

/// Opaque identity of the exact raw alias entry observed by an automatic-
/// selection cache snapshot. This includes stale and differently-bound
/// entries, which are ordinary fresh-cache misses but still matter to the
/// later compare-and-swap publication boundary.
#[derive(Debug, Clone)]
pub(crate) struct UsageCacheBaseline {
    alias: String,
    serialized_entry: Option<Vec<u8>>,
    /// Alias-scoped mutation identity, retained even while the entry is
    /// absent. This closes absent -> present -> absent ABA races without
    /// invalidating unrelated profiles.
    mutation: Option<String>,
}

/// One alias result from an automatic-selection cache snapshot. `usage` is
/// present for any fresh entry owned by the expected account, including a
/// quota-only auto-select generation. `reset_metadata_complete` keeps that
/// narrower entry from masquerading as complete usage, while `baseline`
/// identifies the raw generation (or exact absence) seen under the same lock.
pub(crate) struct AutoSelectUsageCacheLookup {
    usage: Option<UsageInfo>,
    reset_metadata_complete: bool,
    baseline: UsageCacheBaseline,
}

impl AutoSelectUsageCacheLookup {
    pub(crate) fn into_parts(self) -> (Option<UsageInfo>, UsageCacheBaseline) {
        (self.usage, self.baseline)
    }

    pub(crate) fn reset_metadata_complete(&self) -> bool {
        self.reset_metadata_complete
    }

    #[cfg(test)]
    pub(crate) fn absent_for_test(alias: impl Into<String>) -> Self {
        let alias = alias.into();
        Self {
            usage: None,
            reset_metadata_complete: false,
            baseline: UsageCacheBaseline {
                alias,
                serialized_entry: None,
                mutation: None,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn fresh_for_test(alias: impl Into<String>, usage: UsageInfo) -> Self {
        let alias = alias.into();
        Self {
            usage: Some(usage),
            reset_metadata_complete: true,
            baseline: UsageCacheBaseline {
                alias,
                serialized_entry: None,
                mutation: None,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn quota_only_for_test(alias: impl Into<String>, usage: UsageInfo) -> Self {
        let alias = alias.into();
        Self {
            usage: Some(usage),
            reset_metadata_complete: false,
            baseline: UsageCacheBaseline {
                alias,
                serialized_entry: None,
                mutation: None,
            },
        }
    }
}

pub(crate) struct AutoSelectUsageCacheSnapshot {
    lookups: HashMap<String, AutoSelectUsageCacheLookup>,
}

impl AutoSelectUsageCacheSnapshot {
    pub(crate) fn has_fresh_usage(&self, alias: &str) -> bool {
        self.lookups
            .get(alias)
            .is_some_and(|lookup| lookup.usage.is_some())
    }

    pub(crate) fn take(&mut self, alias: &str) -> Result<AutoSelectUsageCacheLookup> {
        self.lookups
            .remove(alias)
            .with_context(|| format!("automatic-selection cache snapshot has no alias '{alias}'"))
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RankingCacheSnapshot {
    pub(crate) last_used: HashMap<String, i64>,
    pub(crate) workspaces: HashMap<String, WorkspaceState>,
}

enum UsageSnapshotRequest {
    #[cfg(test)]
    Unbound(Vec<String>),
    Bound(HashMap<String, StrictAccountBinding>),
}

enum SnapshotTimestamp {
    AtLockAcquisition,
    #[cfg(test)]
    Fixed(u64),
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    entries: HashMap<String, CacheEntry>,
    /// Alias-scoped mutation identities. Entries deliberately remain after a
    /// deletion so an in-flight writer cannot confuse a later absence with the
    /// absence it originally observed.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    usage_mutations: HashMap<String, String>,
    /// Tracks the last time each profile was selected by `use` (unix seconds).
    #[serde(default)]
    last_used: HashMap<String, i64>,
    /// Timestamped workspace lookup results keyed by stable ChatGPT account id.
    #[serde(default)]
    workspaces: HashMap<String, WorkspaceCacheEntry>,
    /// Legacy positive workspace cache. These values had no timestamp, so they
    /// cannot safely be promoted to fresh entries. They are accepted during
    /// deserialization and retired by [`migrate_legacy_workspaces`].
    #[serde(default, skip_serializing)]
    workspace_names: HashMap<String, String>,
    /// Legacy negative workspace cache. Unlike legacy positive entries, these
    /// already carried the timestamp required by the unified representation.
    #[serde(default, skip_serializing)]
    workspace_names_absent: HashMap<String, u64>,
    /// Profiles whose credential the auth server has permanently refused.
    #[serde(default)]
    auth_failures: HashMap<String, AuthFailureEntry>,
}

fn cache_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.json"))
}

fn cache_lock_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.lock"))
}

fn open_cache_lock_file(path: &std::path::Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache directory {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting permissions on {}", parent.display()))?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening cache lock {}", path.display()))
}

#[cfg(test)]
fn with_cache_file_lock_at<T>(
    path: &std::path::Path,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _file = acquire_cache_file_lock_at(path, timeout)?;
    operation()
}

fn acquire_cache_file_lock_at(path: &std::path::Path, timeout: Duration) -> Result<std::fs::File> {
    let file = open_cache_lock_file(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(CACHE_LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "cache lock {} remained held for {:.3}s; refusing to replace the live lock file",
                    path.display(),
                    timeout.as_secs_f64()
                );
            }
            Err(TryLockError::Error(err)) => {
                return Err(anyhow::Error::from(err))
                    .with_context(|| format!("locking cache file {}", path.display()));
            }
        }
    }
    Ok(file)
}

fn with_cache_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    with_cache_lock_at(&cache_lock_path()?, CACHE_LOCK_WAIT_TIMEOUT, operation)
}

fn with_cache_lock_at<T>(
    lock_path: &std::path::Path,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    // Take the path-specific OS lock before the process-wide serialization
    // guard. No thread can then hold the process guard while waiting on an
    // external cache owner, which keeps unrelated in-process cache paths and
    // latency-sensitive best-effort writes from inheriting that wait.
    let _file_lock = acquire_cache_file_lock_at(lock_path, timeout)?;
    let _process_lock = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("cache process lock poisoned"))?;
    operation()
}

/// Run a best-effort derived-cache mutation without extending a completed
/// user-visible operation behind unrelated cache work.
///
/// The path-specific OS lock is attempted exactly once. Once it is owned, the
/// process guard cannot be held by another operation on this cache path because
/// every production cache flow uses the same OS-first lock order. Callers must
/// surface failure as a warning; this is only appropriate for metadata whose
/// publication is explicitly non-authoritative.
fn try_with_cache_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let path = cache_lock_path()?;
    let file = open_cache_lock_file(&path)?;
    match FileExt::try_lock(&file) {
        Ok(()) => {
            let _process_lock = CACHE_LOCK
                .lock()
                .map_err(|_| anyhow::anyhow!("cache process lock poisoned"))?;
            operation()
        }
        Err(TryLockError::WouldBlock) => {
            anyhow::bail!("cache lock {} is busy", path.display())
        }
        Err(TryLockError::Error(error)) => Err(anyhow::Error::from(error))
            .with_context(|| format!("locking cache file {}", path.display())),
    }
}

fn with_cache_lock_cancellable_at<T>(
    lock_path: &std::path::Path,
    timeout: Duration,
    control: &CacheLockAcquireControl,
    operation: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    let deadline = Instant::now() + timeout;
    let file = open_cache_lock_file(lock_path)?;
    loop {
        if control.is_cancelled() {
            return Ok(None);
        }
        match FileExt::try_lock(&file) {
            Ok(()) => {
                if !control.mark_acquired() {
                    return Ok(None);
                }
                let _process_lock = CACHE_LOCK
                    .lock()
                    .map_err(|_| anyhow::anyhow!("cache process lock poisoned"))?;
                return operation().map(Some);
            }
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                if !control.mark_contended() {
                    return Ok(None);
                }
                std::thread::sleep(CACHE_LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => {
                if !control.mark_contended() || !control.mark_timed_out() {
                    return Ok(None);
                }
                anyhow::bail!(
                    "cache lock {} remained held for {:.3}s; refusing to replace the live lock file",
                    lock_path.display(),
                    timeout.as_secs_f64()
                );
            }
            Err(TryLockError::Error(error)) => {
                return Err(anyhow::Error::from(error))
                    .with_context(|| format!("locking cache file {}", lock_path.display()));
            }
        }
    }
}

fn timestamp_is_fresh(now: u64, recorded_at: u64, ttl: u64) -> bool {
    now.checked_sub(recorded_at).is_some_and(|age| age <= ttl)
}

fn ttl() -> Result<u64> {
    Ok(crate::config::try_get()?.cache.ttl)
}

fn load_cache_checked() -> Result<CacheFile> {
    let path = cache_path()?;
    load_cache_checked_at(&path)
}

fn load_cache_checked_at(path: &std::path::Path) -> Result<CacheFile> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let mut cache: CacheFile = serde_json::from_str(&contents)
                .with_context(|| format!("parsing cache file {}", path.display()))?;
            migrate_legacy_workspaces(&mut cache);
            Ok(cache)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CacheFile::default()),
        Err(error) => Err(error).with_context(|| format!("reading cache file {}", path.display())),
    }
}

fn migrate_legacy_workspaces(cache: &mut CacheFile) {
    // A legacy absence has all the data required by the new representation, so
    // migrate it exactly. A legacy name has no observation timestamp; treating
    // it as fresh for an invented period would preserve stale team names. Drop
    // that derived value and let the normal unresolved path revalidate it.
    for (account_id, ts) in cache.workspace_names_absent.drain() {
        cache
            .workspaces
            .entry(account_id)
            .or_insert(WorkspaceCacheEntry { ts, name: None });
    }
    cache.workspace_names.clear();
}

fn load_last_used_checked_at(path: &std::path::Path) -> Result<HashMap<String, i64>> {
    Ok(load_cache_checked_at(path)?.last_used)
}

fn save_cache(cache: &CacheFile) -> Result<()> {
    let path = cache_path()?;
    save_cache_at(&path, cache)
}

fn save_cache_at(path: &std::path::Path, cache: &CacheFile) -> Result<()> {
    let outcome = publish_cache_at(path, cache)?;
    auth::require_durable_private_write(path, "usage cache", outcome)
}

fn publish_cache_at(
    path: &std::path::Path,
    cache: &CacheFile,
) -> Result<auth::PrivateWriteOutcome> {
    let json = serde_json::to_string(cache).context("serializing cache")?;
    auth::atomic_write_private(path, json.as_bytes())
        .with_context(|| format!("writing cache file {}", path.display()))
}

fn to_entry_with_binding(
    u: &UsageInfo,
    recorded_at: u64,
    binding: Option<&StrictAccountBinding>,
) -> CacheEntry {
    CacheEntry {
        revision: u.cache_revision.clone(),
        account_id: binding.map(|binding| binding.account_id.clone()),
        email: binding.map(|binding| binding.email.clone()),
        ts: recorded_at,
        primary_used: u.primary.as_ref().and_then(|w| w.used_percent),
        primary_reset: u.primary.as_ref().and_then(|w| w.resets_at),
        primary_window_minutes: u.primary.as_ref().and_then(|w| w.window_minutes),
        secondary_used: u.secondary.as_ref().and_then(|w| w.used_percent),
        secondary_reset: u.secondary.as_ref().and_then(|w| w.resets_at),
        secondary_window_minutes: u.secondary.as_ref().and_then(|w| w.window_minutes),
        credits_balance: u.credits_balance,
        unlimited_credits: u.unlimited_credits,
        plan_type: u.plan_type.clone(),
        reset_credits_available_count: u.reset_credits_available_count,
        reset_credits: u.reset_credits.clone(),
        reset_credits_error: u.reset_credits_error.clone(),
        reset_metadata_complete: true,
        account_limited: u.account_limited,
        spend_control_reached: u.spend_control_reached,
        rate_limit_reached_type: u.rate_limit_reached_type.clone(),
        individual_limit: u.individual_limit.clone(),
        additional_limits: u.additional_limits.clone(),
        parse_issues: u.parse_issues.clone(),
    }
}

#[cfg(test)]
fn to_entry(u: &UsageInfo, recorded_at: u64) -> CacheEntry {
    to_entry_with_binding(u, recorded_at, None)
}

fn from_entry(e: &CacheEntry) -> Option<UsageInfo> {
    use crate::usage::WindowUsage;
    let primary = if e.primary_used.is_some() || e.primary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.primary_used,
            resets_at: e.primary_reset,
            window_minutes: e.primary_window_minutes,
        })
    } else {
        None
    };
    let secondary = if e.secondary_used.is_some() || e.secondary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.secondary_used,
            resets_at: e.secondary_reset,
            window_minutes: e.secondary_window_minutes,
        })
    } else {
        None
    };
    Some(UsageInfo {
        cache_revision: e.revision.clone(),
        fetched_at: Some(i64::try_from(e.ts).ok()?),
        primary,
        secondary,
        credits_balance: e.credits_balance,
        unlimited_credits: e.unlimited_credits,
        plan_type: e.plan_type.clone(),
        reset_credits_available_count: e.reset_credits_available_count,
        reset_credits: e.reset_credits.clone(),
        reset_credits_error: e.reset_credits_error.clone(),
        account_limited: e.account_limited,
        spend_control_reached: e.spend_control_reached,
        rate_limit_reached_type: e.rate_limit_reached_type.clone(),
        individual_limit: e.individual_limit.clone(),
        additional_limits: e.additional_limits.clone(),
        parse_issues: e.parse_issues.clone(),
    })
}

/// Get cached usage for an alias if within TTL.
#[cfg(test)]
pub fn get(alias: &str) -> Result<Option<UsageInfo>> {
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        let ttl = ttl()?;
        let now =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        Ok(fresh_usage(&cache, alias, now, ttl))
    })
}

/// Get cached usage only when the entry belongs to the expected account.
///
/// Alias ownership is mutable, so a timestamp alone is not sufficient for
/// production reads. Legacy entries and entries written for another strict
/// identity are misses; callers can then obtain an authoritative result.
pub(crate) fn get_bound(alias: &str, binding: &StrictAccountBinding) -> Result<Option<UsageInfo>> {
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        let ttl = ttl()?;
        let now =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        Ok(fresh_usage_bound(&cache, alias, binding, now, ttl))
    })
}

fn fresh_usage(cache: &CacheFile, alias: &str, now: u64, ttl: u64) -> Option<UsageInfo> {
    let entry = cache.entries.get(alias)?;
    if !entry.reset_metadata_complete {
        return None;
    }
    timestamp_is_fresh(now, entry.ts, ttl)
        .then(|| from_entry(entry))
        .flatten()
}

fn fresh_usage_bound(
    cache: &CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
    now: u64,
    ttl: u64,
) -> Option<UsageInfo> {
    let entry = cache.entries.get(alias)?;
    if entry.account_id.as_deref() != Some(binding.account_id.as_str())
        || entry.email.as_deref() != Some(binding.email.as_str())
        || !entry.reset_metadata_complete
    {
        return None;
    }
    timestamp_is_fresh(now, entry.ts, ttl)
        .then(|| from_entry(entry))
        .flatten()
}

/// Read one consistent identity-bound fresh-usage snapshot for a batch.
pub(crate) fn get_many_bound(
    bindings: &HashMap<String, StrictAccountBinding>,
) -> Result<HashMap<String, UsageInfo>> {
    Ok(get_snapshot_bound(bindings, &[])?.usage)
}

/// Read the automatic-selection freshness result and the exact raw cache
/// generation for every alias from one immutable cache-file snapshot.
///
/// A stale entry, a legacy/unbound entry, an entry owned by another account,
/// and a truly absent entry are all cache misses. A fresh quota-only entry is
/// intentionally a hit only here; general readers require complete reset-card
/// metadata. Every baseline includes the alias tombstone so a later completed
/// core probe can publish only if that exact state is still present.
pub(crate) fn get_auto_select_usage_snapshot(
    bindings: &HashMap<String, StrictAccountBinding>,
) -> Result<AutoSelectUsageCacheSnapshot> {
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        let usage_ttl = ttl()?;
        let now =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        auto_select_usage_snapshot_from_cache(&cache, bindings, now, usage_ttl)
    })
}

fn auto_select_usage_snapshot_from_cache(
    cache: &CacheFile,
    bindings: &HashMap<String, StrictAccountBinding>,
    now: u64,
    usage_ttl: u64,
) -> Result<AutoSelectUsageCacheSnapshot> {
    let mut lookups = HashMap::with_capacity(bindings.len());
    for (alias, binding) in bindings {
        let entry = cache.entries.get(alias);
        let belongs_to_expected_account = entry.is_some_and(|entry| {
            entry.account_id.as_deref() == Some(binding.account_id.as_str())
                && entry.email.as_deref() == Some(binding.email.as_str())
        });
        let usage = entry
            .filter(|entry| {
                belongs_to_expected_account && timestamp_is_fresh(now, entry.ts, usage_ttl)
            })
            .and_then(from_entry);
        let reset_metadata_complete =
            usage.is_some() && entry.is_some_and(|entry| entry.reset_metadata_complete);
        lookups.insert(
            alias.clone(),
            AutoSelectUsageCacheLookup {
                usage,
                reset_metadata_complete,
                baseline: usage_cache_baseline(cache, alias)?,
            },
        );
    }
    Ok(AutoSelectUsageCacheSnapshot { lookups })
}

fn usage_cache_baseline(cache: &CacheFile, alias: &str) -> Result<UsageCacheBaseline> {
    let serialized_entry = cache
        .entries
        .get(alias)
        .map(serde_json::to_vec)
        .transpose()
        .with_context(|| format!("serializing usage-cache baseline for profile '{alias}'"))?;
    Ok(UsageCacheBaseline {
        alias: alias.to_string(),
        serialized_entry,
        mutation: cache.usage_mutations.get(alias).cloned(),
    })
}

/// Read usage and workspace metadata from one immutable cache-file snapshot.
///
/// Every requested account id is present in `workspaces`, including unresolved
/// ones. This lets startup and list rendering answer all cache questions after
/// one lock acquisition and one file read instead of reopening the same file
/// once per account.
pub(crate) fn get_snapshot(aliases: &[String], account_ids: &[String]) -> Result<CacheSnapshot> {
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        let usage_ttl = ttl()?;
        let now =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        Ok(snapshot_from_cache(
            &cache,
            aliases,
            account_ids,
            now,
            usage_ttl,
        ))
    })
}

/// Read usage and workspace metadata together while verifying every usage
/// entry against the account that currently owns its alias.
pub(crate) fn get_snapshot_bound(
    bindings: &HashMap<String, StrictAccountBinding>,
    account_ids: &[String],
) -> Result<CacheSnapshot> {
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        let usage_ttl = ttl()?;
        let now =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        Ok(snapshot_from_cache_bound(
            &cache,
            bindings,
            account_ids,
            now,
            usage_ttl,
        ))
    })
}

/// Cancellable identity-bound snapshot read for latency-sensitive UI startup.
///
/// Cancellation is honored only while the worker is waiting for the in-process
/// or cross-process cache lock. Once the lock is acquired, the snapshot read
/// completes normally, so shutdown can drain the tracked task without relying
/// on aborting an already-running blocking worker.
pub(crate) async fn get_snapshot_bound_async_cancellable(
    bindings: &HashMap<String, StrictAccountBinding>,
    account_ids: &[String],
    control: &CacheLockAcquireControl,
) -> Result<Option<CacheSnapshot>> {
    let cache_path = cache_path()?;
    let lock_path = cache_lock_path()?;
    let usage_ttl = ttl()?;
    get_snapshot_bound_async_cancellable_at(
        cache_path,
        lock_path,
        bindings.clone(),
        account_ids.to_vec(),
        usage_ttl,
        control,
    )
    .await
}

#[cfg(test)]
async fn get_snapshot_async_cancellable_at(
    cache_path: PathBuf,
    lock_path: PathBuf,
    aliases: Vec<String>,
    account_ids: Vec<String>,
    now: u64,
    usage_ttl: u64,
    control: &CacheLockAcquireControl,
) -> Result<Option<CacheSnapshot>> {
    get_requested_snapshot_async_cancellable_at(
        cache_path,
        lock_path,
        UsageSnapshotRequest::Unbound(aliases),
        account_ids,
        SnapshotTimestamp::Fixed(now),
        usage_ttl,
        control,
    )
    .await
}

async fn get_snapshot_bound_async_cancellable_at(
    cache_path: PathBuf,
    lock_path: PathBuf,
    bindings: HashMap<String, StrictAccountBinding>,
    account_ids: Vec<String>,
    usage_ttl: u64,
    control: &CacheLockAcquireControl,
) -> Result<Option<CacheSnapshot>> {
    get_requested_snapshot_async_cancellable_at(
        cache_path,
        lock_path,
        UsageSnapshotRequest::Bound(bindings),
        account_ids,
        SnapshotTimestamp::AtLockAcquisition,
        usage_ttl,
        control,
    )
    .await
}

async fn get_requested_snapshot_async_cancellable_at(
    cache_path: PathBuf,
    lock_path: PathBuf,
    request: UsageSnapshotRequest,
    account_ids: Vec<String>,
    timestamp: SnapshotTimestamp,
    usage_ttl: u64,
    control: &CacheLockAcquireControl,
) -> Result<Option<CacheSnapshot>> {
    let worker_control = control.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        with_cache_lock_cancellable_at(&lock_path, CACHE_LOCK_WAIT_TIMEOUT, &worker_control, || {
            let cache = load_cache_checked_at(&cache_path)?;
            let now = match timestamp {
                SnapshotTimestamp::AtLockAcquisition => u64::try_from(auth::now_unix_secs()?)
                    .context("converting usage-cache timestamp")?,
                #[cfg(test)]
                SnapshotTimestamp::Fixed(now) => now,
            };
            Ok(match &request {
                #[cfg(test)]
                UsageSnapshotRequest::Unbound(aliases) => {
                    snapshot_from_cache(&cache, aliases, &account_ids, now, usage_ttl)
                }
                UsageSnapshotRequest::Bound(bindings) => {
                    snapshot_from_cache_bound(&cache, bindings, &account_ids, now, usage_ttl)
                }
            })
        })
    });
    let joined = tokio::select! {
        joined = &mut worker => joined,
        _ = control.cancelled() => worker.await,
    };
    joined.context("cache snapshot read worker failed")?
}

async fn mutate_cache_async_cancellable<T, F>(
    control: &CacheLockAcquireControl,
    mutation: F,
) -> Result<Option<T>>
where
    T: Send + 'static,
    F: FnOnce(&mut CacheFile) -> Result<(T, bool)> + Send + 'static,
{
    let cache_path = cache_path()?;
    let lock_path = cache_lock_path()?;
    let worker_control = control.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        with_cache_lock_cancellable_at(&lock_path, CACHE_LOCK_WAIT_TIMEOUT, &worker_control, || {
            let mut cache = load_cache_checked_at(&cache_path)?;
            let (outcome, changed) = mutation(&mut cache)?;
            if changed {
                save_cache_at(&cache_path, &cache)?;
            }
            Ok(outcome)
        })
    });
    let joined = tokio::select! {
        joined = &mut worker => joined,
        _ = control.cancelled() => worker.await,
    };
    joined.context("cache mutation worker failed")?
}

fn snapshot_from_cache(
    cache: &CacheFile,
    aliases: &[String],
    account_ids: &[String],
    now: u64,
    usage_ttl: u64,
) -> CacheSnapshot {
    let usage = aliases
        .iter()
        .filter_map(|alias| {
            fresh_usage(cache, alias, now, usage_ttl).map(|usage| (alias.clone(), usage))
        })
        .collect();
    let (workspaces, workspace_fresh_for) = workspace_snapshot(cache, account_ids, now);
    CacheSnapshot {
        usage,
        workspaces,
        workspace_fresh_for,
    }
}

fn snapshot_from_cache_bound(
    cache: &CacheFile,
    bindings: &HashMap<String, StrictAccountBinding>,
    account_ids: &[String],
    now: u64,
    usage_ttl: u64,
) -> CacheSnapshot {
    let usage = bindings
        .iter()
        .filter_map(|(alias, binding)| {
            fresh_usage_bound(cache, alias, binding, now, usage_ttl)
                .map(|usage| (alias.clone(), usage))
        })
        .collect();
    let (workspaces, workspace_fresh_for) = workspace_snapshot(cache, account_ids, now);
    CacheSnapshot {
        usage,
        workspaces,
        workspace_fresh_for,
    }
}

fn workspace_snapshot(
    cache: &CacheFile,
    account_ids: &[String],
    now: u64,
) -> (HashMap<String, WorkspaceState>, HashMap<String, Duration>) {
    let mut states = HashMap::with_capacity(account_ids.len());
    let mut fresh_until = HashMap::with_capacity(account_ids.len());
    for account_id in account_ids {
        let state = workspace_state(cache, account_id, now);
        if !matches!(state, WorkspaceState::Unresolved)
            && let Some(entry) = cache.workspaces.get(account_id)
        {
            fresh_until.insert(
                account_id.clone(),
                Duration::from_secs(
                    entry
                        .ts
                        .saturating_add(WORKSPACE_RESOLUTION_TTL)
                        .saturating_sub(now),
                ),
            );
        }
        states.insert(account_id.clone(), state);
    }
    (states, fresh_until)
}

pub(crate) const fn workspace_resolution_ttl() -> Duration {
    Duration::from_secs(WORKSPACE_RESOLUTION_TTL)
}

/// Store usage result in cache.
#[cfg(test)]
pub fn put(alias: &str, usage: &UsageInfo) -> Result<()> {
    put_with_binding(alias, usage, None)
}

/// Store a usage result together with the strict account identity observed
/// while holding the profile's credential lease.
fn new_usage_cache_revision() -> String {
    use rand::Rng;

    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn mark_usage_mutation(cache: &mut CacheFile, alias: &str) {
    cache
        .usage_mutations
        .insert(alias.to_string(), new_usage_cache_revision());
}

/// Store a bound usage value and return the exact revision written. The
/// revision lets a later, deferred metadata request prove that the quota entry
/// it enriches has not been replaced in the meantime.
pub(crate) fn put_bound_versioned(
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
) -> Result<UsageInfo> {
    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        let versioned = replace_bound_usage(&mut cache, alias, binding, usage)?;
        save_cache(&cache)?;
        Ok(versioned)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoreProbeResetMetadata {
    /// Publish only the quota observation. Preserve reset-card metadata only
    /// when the exact baseline already belongs to the same account; otherwise
    /// record that a reset-card lookup is still required.
    PreserveExisting,
    /// The probe has been followed by an authoritative reset-card lookup.
    Complete,
}

#[derive(Debug, Clone)]
pub(crate) struct CoreProbeCacheUpdate {
    pub(crate) alias: String,
    pub(crate) binding: StrictAccountBinding,
    pub(crate) baseline: UsageCacheBaseline,
    pub(crate) usage: UsageInfo,
    pub(crate) reset_metadata: CoreProbeResetMetadata,
}

#[derive(Debug)]
pub(crate) struct CoreProbeCacheOutcome {
    pub(crate) alias: String,
    pub(crate) usage: UsageInfo,
    pub(crate) baseline: UsageCacheBaseline,
    pub(crate) reset_metadata_complete: bool,
}

/// Publish a completed auto-select probe only against the exact alias
/// generation observed before its request began.
///
/// Freshness is deliberately absent from this decision. The raw entry and the
/// persisted alias mutation must both match. A newer same-account generation
/// is returned unchanged so the caller can re-score authoritative data; a
/// rebound or an invalidated absence fails instead of being republished. The
/// complete batch uses one lock, one cache read, and at most one durable write.
fn complete_core_probes_bound(
    updates: &[CoreProbeCacheUpdate],
) -> Result<Vec<CoreProbeCacheOutcome>> {
    let mut aliases = std::collections::HashSet::with_capacity(updates.len());
    for update in updates {
        if update.baseline.alias != update.alias {
            anyhow::bail!(
                "core-probe cache baseline for '{}' cannot complete profile '{}'",
                update.baseline.alias,
                update.alias
            );
        }
        if !aliases.insert(update.alias.as_str()) {
            anyhow::bail!(
                "automatic-selection cache batch contains duplicate profile '{}'",
                update.alias
            );
        }
    }

    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        let recorded_at =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        let mut changed = false;
        let mut outcomes = Vec::with_capacity(updates.len());
        for update in updates {
            let (usage, item_changed) = complete_core_probe_bound_at(
                &mut cache,
                &update.alias,
                &update.binding,
                &update.baseline,
                &update.usage,
                update.reset_metadata,
                recorded_at,
            )?;
            changed |= item_changed;
            let reset_metadata_complete = cache
                .entries
                .get(&update.alias)
                .with_context(|| {
                    format!(
                        "completed core probe for '{}' has no cache entry",
                        update.alias
                    )
                })?
                .reset_metadata_complete;
            outcomes.push(CoreProbeCacheOutcome {
                alias: update.alias.clone(),
                usage,
                baseline: usage_cache_baseline(&cache, &update.alias)?,
                reset_metadata_complete,
            });
        }
        if changed {
            save_cache(&cache)?;
        }
        Ok(outcomes)
    })
}

fn complete_core_probe_bound_at(
    cache: &mut CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
    baseline: &UsageCacheBaseline,
    completed_probe: &UsageInfo,
    reset_metadata: CoreProbeResetMetadata,
    recorded_at: u64,
) -> Result<(UsageInfo, bool)> {
    if baseline.alias != alias {
        anyhow::bail!(
            "core-probe cache baseline for '{}' cannot complete profile '{alias}'",
            baseline.alias
        );
    }
    let current_serialized = cache
        .entries
        .get(alias)
        .map(serde_json::to_vec)
        .transpose()
        .with_context(|| format!("serializing current usage-cache generation for '{alias}'"))?;
    let current_mutation = cache.usage_mutations.get(alias).cloned();

    if current_serialized == baseline.serialized_entry && current_mutation == baseline.mutation {
        let versioned = replace_core_probe_usage_at(
            cache,
            alias,
            binding,
            completed_probe,
            reset_metadata,
            recorded_at,
        );
        return Ok((versioned, true));
    }

    let current_belongs_to_expected_account = cache.entries.get(alias).is_some_and(|entry| {
        entry.account_id.as_deref() == Some(binding.account_id.as_str())
            && entry.email.as_deref() == Some(binding.email.as_str())
    });
    if current_belongs_to_expected_account {
        let current = cache
            .entries
            .get(alias)
            .and_then(from_entry)
            .with_context(|| {
                format!("intervening usage-cache generation for '{alias}' has an invalid timestamp")
            })?;
        return Ok((current, false));
    }

    anyhow::bail!(
        "usage-cache generation for '{alias}' was invalidated or rebound while its core probe was being completed"
    )
}

fn replace_core_probe_usage_at(
    cache: &mut CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
    reset_metadata: CoreProbeResetMetadata,
    recorded_at: u64,
) -> UsageInfo {
    let preserved = cache.entries.get(alias).and_then(|entry| {
        (entry.account_id.as_deref() == Some(binding.account_id.as_str())
            && entry.email.as_deref() == Some(binding.email.as_str()))
        .then(|| {
            (
                entry.reset_credits_available_count,
                entry.reset_credits.clone(),
                entry.reset_credits_error.clone(),
                entry.reset_metadata_complete,
            )
        })
    });
    let mut versioned = usage.clone();
    let reset_metadata_complete = match reset_metadata {
        CoreProbeResetMetadata::Complete => true,
        CoreProbeResetMetadata::PreserveExisting => {
            let (available_count, credits, error, complete) =
                preserved.unwrap_or((None, Vec::new(), None, false));
            versioned.reset_credits_available_count = available_count;
            versioned.reset_credits = credits;
            versioned.reset_credits_error = error;
            complete
        }
    };
    versioned.cache_revision = Some(new_usage_cache_revision());
    let mut entry = to_entry_with_binding(&versioned, recorded_at, Some(binding));
    entry.reset_metadata_complete = reset_metadata_complete;
    cache.entries.insert(alias.to_string(), entry);
    mark_usage_mutation(cache, alias);
    versioned
}

fn replace_bound_usage(
    cache: &mut CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
) -> Result<UsageInfo> {
    let recorded_at =
        u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
    Ok(replace_bound_usage_at(
        cache,
        alias,
        binding,
        usage,
        recorded_at,
    ))
}

fn replace_bound_usage_at(
    cache: &mut CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
    recorded_at: u64,
) -> UsageInfo {
    let mut versioned = usage.clone();
    versioned.cache_revision = Some(new_usage_cache_revision());
    cache.entries.insert(
        alias.to_string(),
        to_entry_with_binding(&versioned, recorded_at, Some(binding)),
    );
    mark_usage_mutation(cache, alias);
    versioned
}

fn merge_reset_credit_enrichment(
    cache: &mut CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
) -> bool {
    let Some(expected_revision) = usage.cache_revision.as_deref() else {
        return false;
    };
    let Some(entry) = cache.entries.get_mut(alias) else {
        return false;
    };
    if entry.account_id.as_deref() != Some(binding.account_id.as_str())
        || entry.email.as_deref() != Some(binding.email.as_str())
        || entry.revision.as_deref() != Some(expected_revision)
    {
        return false;
    }
    entry.reset_credits_available_count = usage.reset_credits_available_count;
    entry.reset_credits = usage.reset_credits.clone();
    entry.reset_credits_error = usage.reset_credits_error.clone();
    entry.reset_metadata_complete = true;
    mark_usage_mutation(cache, alias);
    true
}

#[cfg(test)]
fn put_with_binding(
    alias: &str,
    usage: &UsageInfo,
    binding: Option<&StrictAccountBinding>,
) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        let recorded_at =
            u64::try_from(auth::now_unix_secs()?).context("converting usage-cache timestamp")?;
        cache.entries.insert(
            alias.to_string(),
            to_entry_with_binding(usage, recorded_at, binding),
        );
        mark_usage_mutation(&mut cache, alias);
        save_cache(&cache)
    })
}

fn credential_fingerprint(refresh_token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(refresh_token.as_bytes()))
}

/// Upper bound on server-supplied text kept in the cache file.
const STORED_TEXT_MAX: usize = 512;

/// Make server-supplied text safe to keep and to re-render.
///
/// The wording in a verdict comes from the auth server. It used to be shown
/// once and dropped; recording it means it is written to disk and printed
/// again on every later listing, so escape sequences would be replayed into
/// the terminal each time and an oversized `error_description` would sit in
/// the cache file forever. Control characters go, length is bounded, and
/// everything readable — including non-ASCII — is preserved.
fn sanitize_for_storage(text: &str) -> String {
    crate::safe_text::bounded_terminal_text(text, STORED_TEXT_MAX)
}

fn record_auth_failure(
    cache: &mut CacheFile,
    alias: &str,
    refresh_token: &str,
    summary: &str,
    detail: &str,
    recorded_at: u64,
) {
    cache.auth_failures.insert(
        alias.to_string(),
        AuthFailureEntry {
            ts: recorded_at,
            credential: credential_fingerprint(refresh_token),
            summary: sanitize_for_storage(summary),
            detail: sanitize_for_storage(detail),
        },
    );
}

/// Forget fetch state keyed by `alias` while preserving selection history.
/// The alias mutation advances even when state was already absent so an
/// in-flight exact-absence writer cannot publish across this boundary.
/// Returns whether any visible fetch record was removed.
fn drop_fetch_state(cache: &mut CacheFile, alias: &str) -> bool {
    let dropped_usage = cache.entries.remove(alias).is_some();
    let dropped_failure = cache.auth_failures.remove(alias).is_some();
    mark_usage_mutation(cache, alias);
    dropped_usage || dropped_failure
}

fn drop_fetch_state_bound(
    cache: &mut CacheFile,
    alias: &str,
    binding: &StrictAccountBinding,
) -> bool {
    if cache.entries.get(alias).is_some_and(|entry| {
        entry.account_id.as_deref() != Some(binding.account_id.as_str())
            || entry.email.as_deref() != Some(binding.email.as_str())
    }) {
        return false;
    }
    drop_fetch_state(cache, alias);
    true
}

/// Forget every cache record owned by a profile alias. Returns whether anything
/// was removed.
fn drop_profile_state(cache: &mut CacheFile, alias: &str) -> bool {
    let dropped_fetch_state = drop_fetch_state(cache, alias);
    let dropped_last_used = cache.last_used.remove(alias).is_some();
    dropped_fetch_state || dropped_last_used
}

fn auth_failure_for<'a>(
    cache: &'a CacheFile,
    alias: &str,
    refresh_token: &str,
) -> Option<&'a AuthFailureEntry> {
    let entry = cache.auth_failures.get(alias)?;
    (entry.credential == credential_fingerprint(refresh_token)).then_some(entry)
}

/// Replace every record keyed by `new` with the complete cache generation
/// currently owned by `old`. Returns whether anything changed.
///
/// The three movable alias-record maps are one logical generation even though
/// any one may be absent. Clearing the destination first prevents a rename
/// from combining source usage with an older destination auth verdict or
/// selection timestamp. Mutation tombstones stay attached to their namespace
/// and are advanced rather than moved.
fn migrate_alias(cache: &mut CacheFile, old: &str, new: &str) -> bool {
    if old == new {
        return false;
    }

    // Each map may hold `old` without the others — a profile can have been used
    // but never fetched, or refused before it was ever selected.
    drop_profile_state(cache, new);
    if let Some(entry) = cache.entries.remove(old) {
        cache.entries.insert(new.to_string(), entry);
    }
    if let Some(ts) = cache.last_used.remove(old) {
        cache.last_used.insert(new.to_string(), ts);
    }
    if let Some(failure) = cache.auth_failures.remove(old) {
        cache.auth_failures.insert(new.to_string(), failure);
    }
    // Namespace history belongs to the alias, not the entry being renamed.
    // Retain tombstones for both names and start a new destination generation.
    mark_usage_mutation(cache, old);
    mark_usage_mutation(cache, new);
    true
}

/// The auth server's standing refusal for `alias`, if it still concerns the
/// credential the profile currently holds.
pub fn get_auth_failure(alias: &str, refresh_token: &str) -> Result<Option<UsageError>> {
    with_cache_lock(|| {
        Ok(
            auth_failure_for(&load_cache_checked()?, alias, refresh_token)
                .map(auth_failure_to_usage_error),
        )
    })
}

fn auth_failure_to_usage_error(entry: &AuthFailureEntry) -> UsageError {
    UsageError {
        summary: entry.summary.clone(),
        detail: entry.detail.clone(),
    }
}

/// Read standing auth refusals for a complete candidate batch from one cache
/// snapshot. A contended or malformed cache fails the batch once instead of
/// multiplying the same lock timeout by the number of profiles.
pub(crate) fn get_auth_failures(
    credentials: &HashMap<String, String>,
) -> Result<HashMap<String, UsageError>> {
    if credentials.is_empty() {
        return Ok(HashMap::new());
    }
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        Ok(auth_failures_for(&cache, credentials))
    })
}

fn auth_failures_for(
    cache: &CacheFile,
    credentials: &HashMap<String, String>,
) -> HashMap<String, UsageError> {
    credentials
        .iter()
        .filter_map(|(alias, refresh_token)| {
            auth_failure_for(cache, alias, refresh_token)
                .map(|entry| (alias.clone(), auth_failure_to_usage_error(entry)))
        })
        .collect()
}

/// Remember that the auth server refused `refresh_token` for good.
pub fn put_auth_failure(alias: &str, refresh_token: &str, error: &UsageError) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        let recorded_at = u64::try_from(auth::now_unix_secs()?)
            .context("converting auth-failure cache timestamp")?;
        record_auth_failure(
            &mut cache,
            alias,
            refresh_token,
            &error.summary,
            &error.detail,
            recorded_at,
        );
        save_cache(&cache)
    })
}

pub fn set_workspace_name(account_id: &str, name: Option<&str>) -> Result<()> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(());
    }
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        let recorded_at = u64::try_from(auth::now_unix_secs()?)
            .context("converting workspace-cache timestamp")?;
        let changed = update_workspace_name(&mut cache, account_id, name, recorded_at);
        if changed {
            save_cache(&cache)?;
        }
        Ok(())
    })
}

pub(crate) async fn set_workspace_state_async_cancellable(
    account_id: &str,
    state: &WorkspaceState,
    control: &CacheLockAcquireControl,
) -> Result<Option<bool>> {
    let account_id = account_id.trim().to_string();
    if account_id.is_empty() || matches!(state, WorkspaceState::Unresolved) {
        return Ok(Some(false));
    }
    let name = match state {
        WorkspaceState::Named(name) => {
            let name = name.trim();
            (!name.is_empty()).then(|| name.to_string())
        }
        WorkspaceState::Absent => None,
        WorkspaceState::Unresolved => unreachable!("unresolved state returned above"),
    };
    mutate_cache_async_cancellable(control, move |cache| {
        let recorded_at = u64::try_from(auth::now_unix_secs()?)
            .context("converting workspace-cache timestamp")?;
        let changed = update_workspace_name(cache, &account_id, name.as_deref(), recorded_at);
        Ok((changed, changed))
    })
    .await
}

fn update_workspace_name(
    cache: &mut CacheFile,
    account_id: &str,
    name: Option<&str>,
    recorded_at: u64,
) -> bool {
    let entry = WorkspaceCacheEntry {
        ts: recorded_at,
        name: name.map(str::to_string),
    };
    if cache.workspaces.get(account_id) == Some(&entry) {
        return false;
    }
    cache.workspaces.insert(account_id.to_string(), entry);
    true
}

fn workspace_state(cache: &CacheFile, account_id: &str, now: u64) -> WorkspaceState {
    let Some(entry) = cache
        .workspaces
        .get(account_id)
        .filter(|entry| timestamp_is_fresh(now, entry.ts, WORKSPACE_RESOLUTION_TTL))
    else {
        return WorkspaceState::Unresolved;
    };
    match &entry.name {
        Some(name) => WorkspaceState::Named(name.clone()),
        None => WorkspaceState::Absent,
    }
}

pub fn apply_workspace_name(info: &mut crate::jwt::AccountInfo) -> Result<()> {
    let Some(account_id) = info.account_id.as_deref() else {
        return Ok(());
    };
    let account_ids = [account_id.to_string()];
    let snapshot = get_snapshot(&[], &account_ids)?;
    let state = snapshot
        .workspaces
        .get(account_id)
        .expect("requested workspace state must be present in the cache snapshot");
    apply_workspace_state(info, state);
    Ok(())
}

pub(crate) fn apply_workspace_state(info: &mut crate::jwt::AccountInfo, state: &WorkspaceState) {
    match state {
        WorkspaceState::Unresolved => {}
        WorkspaceState::Named(name) => info.workspace_name = Some(name.clone()),
        WorkspaceState::Absent => info.workspace_name = None,
    }
}

/// Remove cached usage for an alias while preserving last-used metadata.
///
/// Also drops any standing auth refusal: callers use this to force the next
/// read back onto the network, and leaving the refusal behind would answer
/// that read from cache anyway.
pub fn invalidate(alias: &str) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        drop_fetch_state(&mut cache, alias);
        save_cache(&cache).context("writing usage cache invalidation")?;
        Ok(())
    })
}

pub(crate) async fn invalidate_bound_async(
    alias: &str,
    binding: &StrictAccountBinding,
) -> Result<bool> {
    let alias = alias.to_string();
    let binding = binding.clone();
    tokio::task::spawn_blocking(move || {
        with_cache_lock(|| {
            let mut cache = load_cache_checked()?;
            if !drop_fetch_state_bound(&mut cache, &alias, &binding) {
                return Ok(false);
            }
            save_cache(&cache).context("writing identity-bound usage cache invalidation")?;
            Ok(true)
        })
    })
    .await
    .context("identity-bound usage-cache invalidation worker failed")?
}

/// Remove all cache state owned by a profile that is being deleted.
///
/// Unlike [`invalidate`], profile deletion must also discard last-used history:
/// the same alias may later be assigned to a different account.
pub fn purge_profile(alias: &str) -> Result<()> {
    with_cache_lock(|| {
        // Deleting a profile must not treat an unreadable or malformed cache
        // as empty: doing so could archive the profile while its old alias
        // state remains on disk and later leaks into a replacement profile.
        let mut cache = load_cache_checked()?;
        drop_profile_state(&mut cache, alias);
        save_cache(&cache).context("writing deleted-profile cache cleanup")?;
        Ok(())
    })
}

/// Async wrapper around [`get`]: runs the blocking lock + file read on a
/// dedicated blocking thread so it never stalls a tokio worker. Use this on
/// the high-concurrency usage-fetch path (up to `network.max_concurrent`
/// tasks) instead of calling [`get`] directly inside an async task.
#[cfg(test)]
pub async fn get_async(alias: &str) -> Result<Option<UsageInfo>> {
    let alias = alias.to_string();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        if TEST_PANIC_NEXT_CACHE_READ_WORKER.swap(false, std::sync::atomic::Ordering::SeqCst) {
            panic!("injected usage-cache worker panic");
        }
        get(&alias)
    })
    .await
    .context("usage-cache read worker failed")?
}

/// Async wrapper around [`get_bound`].
pub(crate) async fn get_bound_async(
    alias: &str,
    binding: &StrictAccountBinding,
) -> Result<Option<UsageInfo>> {
    let alias = alias.to_string();
    let binding = binding.clone();
    tokio::task::spawn_blocking(move || get_bound(&alias, &binding))
        .await
        .context("usage-cache read worker failed")?
}

#[cfg(test)]
static TEST_PANIC_NEXT_CACHE_READ_WORKER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) async fn put_bound_versioned_async(
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
) -> Result<UsageInfo> {
    let alias = alias.to_string();
    let binding = binding.clone();
    let usage = usage.clone();
    tokio::task::spawn_blocking(move || put_bound_versioned(&alias, &binding, &usage))
        .await
        .context("usage-cache write worker failed")?
}

pub(crate) async fn complete_core_probes_bound_async(
    updates: Vec<CoreProbeCacheUpdate>,
) -> Result<Vec<CoreProbeCacheOutcome>> {
    tokio::task::spawn_blocking(move || complete_core_probes_bound(&updates))
        .await
        .context("core-probe cache completion worker failed")?
}

/// Store a bound usage generation without making a latency-sensitive caller
/// wait after its follow-up has been superseded. Cancellation is honored only
/// while waiting for the cache lock; once acquired, the durable write finishes.
pub(crate) async fn put_bound_versioned_async_cancellable(
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
    control: &CacheLockAcquireControl,
) -> Result<Option<UsageInfo>> {
    let alias = alias.to_string();
    let binding = binding.clone();
    let usage = usage.clone();
    mutate_cache_async_cancellable(control, move |cache| {
        let versioned = replace_bound_usage(cache, &alias, &binding, &usage)?;
        Ok((versioned, true))
    })
    .await
}

pub(crate) async fn merge_reset_credit_enrichment_bound_async_cancellable(
    alias: &str,
    binding: &StrictAccountBinding,
    usage: &UsageInfo,
    control: &CacheLockAcquireControl,
) -> Result<Option<bool>> {
    let alias = alias.to_string();
    let binding = binding.clone();
    let usage = usage.clone();
    mutate_cache_async_cancellable(control, move |cache| {
        let changed = merge_reset_credit_enrichment(cache, &alias, &binding, &usage);
        Ok((changed, changed))
    })
    .await
}

/// Async wrapper around [`get_auth_failure`]; see [`get_async`] for rationale.
pub async fn get_auth_failure_async(
    alias: &str,
    refresh_token: &str,
) -> Result<Option<UsageError>> {
    let alias = alias.to_string();
    let refresh_token = refresh_token.to_string();
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        if TEST_PANIC_NEXT_AUTH_FAILURE_CACHE_READ_WORKER
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("injected auth-failure cache worker panic");
        }
        get_auth_failure(&alias, &refresh_token)
    })
    .await
    .context("auth-failure cache read worker failed")?
}

#[cfg(test)]
static TEST_PANIC_NEXT_AUTH_FAILURE_CACHE_READ_WORKER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Async wrapper around [`put_auth_failure`]; see [`get_async`] for rationale.
pub async fn put_auth_failure_async(
    alias: &str,
    refresh_token: &str,
    error: &UsageError,
) -> Result<()> {
    let alias = alias.to_string();
    let refresh_token = refresh_token.to_string();
    let error = error.clone();
    tokio::task::spawn_blocking(move || put_auth_failure(&alias, &refresh_token, &error))
        .await
        .context("auth-failure cache write worker failed")?
}

/// Read a consistent snapshot of profile-selection history for automatic
/// ranking. Malformed or unreadable cache state is an error: choosing an
/// account with invented `0` timestamps would change the automatic-selection
/// decision.
pub(crate) fn last_used_snapshot_checked() -> Result<HashMap<String, i64>> {
    with_cache_lock(|| load_last_used_checked_at(&cache_path()?))
}

/// Read selection history and workspace labels for automatic ranking from one
/// immutable cache generation.
pub(crate) fn ranking_snapshot_checked(account_ids: &[String]) -> Result<RankingCacheSnapshot> {
    with_cache_lock(|| {
        let cache = load_cache_checked()?;
        let now = u64::try_from(auth::now_unix_secs()?)
            .context("converting workspace-cache timestamp")?;
        let (workspaces, _) = workspace_snapshot(&cache, account_ids, now);
        Ok(RankingCacheSnapshot {
            last_used: cache.last_used,
            workspaces,
        })
    })
}

/// Record successful-selection metadata only when both cache locks are
/// immediately available. The profile switch itself is already durable before
/// this derived history is attempted, so contention is returned to the caller
/// as a warning instead of keeping the switch visibly in progress.
pub(crate) fn try_set_last_used(alias: &str) -> Result<()> {
    try_with_cache_lock(|| {
        #[cfg(test)]
        run_before_last_used_write_test_hook();
        let mut cache = load_cache_checked()?;
        let now = crate::auth::now_unix_secs()?;
        cache.last_used.insert(alias.to_string(), now);
        save_cache(&cache).context("writing last_used cache")
    })
}

#[derive(Debug)]
#[must_use = "a visibly-published cache rename must not be rolled back as though it failed"]
pub(crate) enum RenameOutcome {
    Unchanged,
    DurablyRenamed,
    VisibleDurabilityUnconfirmed { cause: anyhow::Error },
}

pub(crate) fn rename(old: &str, new: &str) -> Result<RenameOutcome> {
    with_cache_lock(|| {
        let mut cache = load_cache_checked()?;
        if !migrate_alias(&mut cache, old, new) {
            return Ok(RenameOutcome::Unchanged);
        }
        match publish_cache_at(&cache_path()?, &cache)? {
            auth::PrivateWriteOutcome::DurablyPublished => Ok(RenameOutcome::DurablyRenamed),
            auth::PrivateWriteOutcome::VisibleDurabilityUnconfirmed { cause } => {
                Ok(RenameOutcome::VisibleDurabilityUnconfirmed { cause })
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs4::FileExt;
    use serde_json::json;
    use std::time::Duration;

    const TEST_NOW: u64 = 1_800_000_000;

    fn strict_binding(account_id: &str, email: &str) -> StrictAccountBinding {
        StrictAccountBinding {
            account_id: account_id.to_string(),
            email: email.to_string(),
        }
    }

    #[test]
    fn test_cache_entry_deserialize_without_credits() {
        let entry: CacheEntry = serde_json::from_value(json!({
            "ts": 123,
            "primary_used": 25.0,
            "primary_reset": 456,
            "secondary_used": 75.0,
            "secondary_reset": 789
        }))
        .unwrap();

        assert_eq!(entry.account_id, None);
        assert_eq!(entry.email, None);
        assert_eq!(entry.credits_balance, None);
        assert_eq!(entry.unlimited_credits, None);
        assert_eq!(entry.reset_credits_available_count, None);
        assert!(entry.reset_credits.is_empty());
        assert_eq!(entry.reset_credits_error, None);
        assert!(
            entry.reset_metadata_complete,
            "cache generations written before quota-only entries were always complete"
        );
        assert!(!entry.account_limited);
        assert!(!entry.spend_control_reached);
        assert!(entry.parse_issues.is_empty());

        let usage = from_entry(&entry).expect("test timestamp must fit the usage model");
        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
        assert_eq!(usage.reset_credits_available_count, None);
        assert!(usage.reset_credits.is_empty());
        assert!(!usage.account_limited);
        assert!(!usage.spend_control_reached);
    }

    #[test]
    fn existing_cache_files_default_to_no_alias_mutations() {
        let cache: CacheFile = serde_json::from_value(json!({ "entries": {} })).unwrap();
        assert!(cache.usage_mutations.is_empty());
    }

    #[test]
    fn test_cache_round_trip_preserves_limit_details() {
        let usage = UsageInfo {
            account_limited: true,
            spend_control_reached: true,
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            additional_limits: vec![crate::usage::AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                allowed: Some(true),
                limit_reached: Some(false),
                primary: None,
                secondary: None,
            }],
            parse_issues: vec![UsageParseIssue::InvalidAdditionalRateLimits {
                detail: "item 0 is malformed".to_string(),
            }],
            ..Default::default()
        };

        let entry = to_entry(&usage, TEST_NOW);
        assert!(entry.account_limited);
        assert!(entry.spend_control_reached);
        let restored = from_entry(&entry).expect("test timestamp must fit the usage model");
        assert!(restored.account_limited);
        assert!(restored.spend_control_reached);
        assert_eq!(
            restored.rate_limit_reached_type.as_deref(),
            Some("rate_limit_reached")
        );
        assert_eq!(restored.additional_limits.len(), 1);
        assert_eq!(restored.parse_issues, usage.parse_issues);
        assert_eq!(
            restored.additional_limits[0].metered_feature.as_deref(),
            Some("codex_bengalfox")
        );
    }

    #[test]
    fn future_dated_cache_records_are_never_fresh() {
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "future".to_string(),
            to_entry(&UsageInfo::default(), TEST_NOW + 1),
        );
        cache.workspaces.insert(
            "future".to_string(),
            WorkspaceCacheEntry {
                ts: TEST_NOW + 1,
                name: None,
            },
        );
        cache.entries.insert(
            "unrepresentable".to_string(),
            to_entry(&UsageInfo::default(), u64::MAX),
        );

        assert!(fresh_usage(&cache, "future", TEST_NOW, u64::MAX).is_none());
        assert!(fresh_usage(&cache, "unrepresentable", u64::MAX, 0).is_none());
        assert_eq!(
            workspace_state(&cache, "future", TEST_NOW),
            WorkspaceState::Unresolved
        );
    }

    #[test]
    fn identity_bound_usage_accepts_a_matching_owner() {
        let binding = strict_binding("acct-1", "alice@example.com");
        let usage = UsageInfo {
            plan_type: Some("plus".to_string()),
            ..Default::default()
        };
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "alice".to_string(),
            to_entry_with_binding(&usage, TEST_NOW, Some(&binding)),
        );

        let restored = fresh_usage_bound(&cache, "alice", &binding, TEST_NOW, 60)
            .expect("the same strict account identity should own its cached usage");

        assert_eq!(restored.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn identity_bound_usage_rejects_either_mismatched_owner_component() {
        let stored_binding = strict_binding("acct-1", "alice@example.com");
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "reused".to_string(),
            to_entry_with_binding(&UsageInfo::default(), TEST_NOW, Some(&stored_binding)),
        );

        assert!(
            fresh_usage_bound(
                &cache,
                "reused",
                &strict_binding("acct-2", "alice@example.com"),
                TEST_NOW,
                60,
            )
            .is_none()
        );
        assert!(
            fresh_usage_bound(
                &cache,
                "reused",
                &strict_binding("acct-1", "other@example.com"),
                TEST_NOW,
                60,
            )
            .is_none()
        );
    }

    #[test]
    fn auto_select_snapshot_keeps_raw_miss_generations_distinct() {
        let expected = strict_binding("acct-expected", "expected@example.com");
        let other = strict_binding("acct-other", "other@example.com");
        let bindings = HashMap::from([
            ("fresh".to_string(), expected.clone()),
            ("stale".to_string(), expected.clone()),
            ("rebound".to_string(), expected.clone()),
            ("absent".to_string(), expected.clone()),
        ]);
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "fresh".to_string(),
            to_entry_with_binding(&UsageInfo::default(), 100, Some(&expected)),
        );
        cache.entries.insert(
            "stale".to_string(),
            to_entry_with_binding(&UsageInfo::default(), 1, Some(&expected)),
        );
        cache.entries.insert(
            "rebound".to_string(),
            to_entry_with_binding(&UsageInfo::default(), 100, Some(&other)),
        );

        let mut snapshot =
            auto_select_usage_snapshot_from_cache(&cache, &bindings, 100, 1).unwrap();
        assert!(snapshot.has_fresh_usage("fresh"));
        assert!(!snapshot.has_fresh_usage("stale"));
        assert!(!snapshot.has_fresh_usage("rebound"));
        assert!(!snapshot.has_fresh_usage("absent"));

        let stale = snapshot.take("stale").unwrap().into_parts().1;
        let rebound = snapshot.take("rebound").unwrap().into_parts().1;
        let absent = snapshot.take("absent").unwrap().into_parts().1;
        assert!(stale.serialized_entry.is_some());
        assert!(rebound.serialized_entry.is_some());
        assert_ne!(stale.serialized_entry, rebound.serialized_entry);
        assert!(absent.serialized_entry.is_none());
    }

    #[test]
    fn core_probe_replaces_the_exact_stale_baseline_without_consulting_ttl() {
        let binding = strict_binding("acct-exact", "exact@example.com");
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "exact".to_string(),
            to_entry_with_binding(
                &UsageInfo {
                    cache_revision: Some("stale-baseline".to_string()),
                    secondary: Some(crate::usage::WindowUsage {
                        used_percent: Some(99.0),
                        ..Default::default()
                    }),
                    reset_credits_available_count: Some(1),
                    reset_credits: vec![ResetCredit {
                        id: "preserved-card".to_string(),
                        granted_at: None,
                        expires_at: None,
                    }],
                    ..Default::default()
                },
                10,
                Some(&binding),
            ),
        );
        let baseline = usage_cache_baseline(&cache, "exact").unwrap();
        assert!(
            fresh_usage_bound(&cache, "exact", &binding, 1_000, 1).is_none(),
            "the baseline is intentionally stale under a very low TTL"
        );

        let (completed, changed) = complete_core_probe_bound_at(
            &mut cache,
            "exact",
            &binding,
            &baseline,
            &UsageInfo {
                secondary: Some(crate::usage::WindowUsage {
                    used_percent: Some(42.0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CoreProbeResetMetadata::PreserveExisting,
            50_000,
        )
        .unwrap();

        assert!(changed);
        assert_eq!(completed.fetched_at, None);
        assert_ne!(completed.cache_revision.as_deref(), Some("stale-baseline"));
        assert_eq!(completed.reset_credits[0].id, "preserved-card");
        let stored = from_entry(cache.entries.get("exact").unwrap()).unwrap();
        assert_eq!(stored.fetched_at, Some(50_000));
        assert_eq!(
            stored
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(42.0)
        );
        assert!(cache.entries["exact"].reset_metadata_complete);
    }

    #[test]
    fn quota_only_probe_is_reusable_only_by_automatic_selection() {
        let binding = strict_binding("acct-quota", "quota@example.com");
        let mut cache = CacheFile::default();
        let baseline = usage_cache_baseline(&cache, "quota").unwrap();
        let probe = UsageInfo {
            secondary: Some(crate::usage::WindowUsage {
                used_percent: Some(10.0),
                ..Default::default()
            }),
            ..Default::default()
        };

        let (_, changed) = complete_core_probe_bound_at(
            &mut cache,
            "quota",
            &binding,
            &baseline,
            &probe,
            CoreProbeResetMetadata::PreserveExisting,
            TEST_NOW,
        )
        .unwrap();

        assert!(changed);
        assert!(
            fresh_usage_bound(&cache, "quota", &binding, TEST_NOW, 60).is_none(),
            "normal cache readers must not mistake quota-only data for complete usage"
        );
        let mut snapshot = auto_select_usage_snapshot_from_cache(
            &cache,
            &HashMap::from([("quota".to_string(), binding.clone())]),
            TEST_NOW,
            60,
        )
        .unwrap();
        let lookup = snapshot.take("quota").unwrap();
        assert!(lookup.usage.is_some());
        assert!(!lookup.reset_metadata_complete());

        let quota_baseline = usage_cache_baseline(&cache, "quota").unwrap();
        let mut enriched = probe;
        enriched.reset_credits_available_count = Some(1);
        enriched.reset_credits = vec![ResetCredit {
            id: "quota-card".to_string(),
            granted_at: None,
            expires_at: None,
        }];
        let (_, changed) = complete_core_probe_bound_at(
            &mut cache,
            "quota",
            &binding,
            &quota_baseline,
            &enriched,
            CoreProbeResetMetadata::Complete,
            TEST_NOW + 1,
        )
        .unwrap();
        assert!(changed);
        let completed = fresh_usage_bound(&cache, "quota", &binding, TEST_NOW + 1, 60)
            .expect("authoritative reset metadata must promote the quota-only entry");
        assert_eq!(completed.reset_credits[0].id, "quota-card");
    }

    #[test]
    fn persisted_absence_mutation_rejects_create_invalidate_absence_aba() {
        let binding = strict_binding("acct-aba", "aba@example.com");
        let mut cache = CacheFile::default();
        let original_absence = usage_cache_baseline(&cache, "aba").unwrap();
        replace_bound_usage_at(&mut cache, "aba", &binding, &UsageInfo::default(), TEST_NOW);
        drop_fetch_state(&mut cache, "aba");
        assert!(!cache.entries.contains_key("aba"));
        assert!(cache.usage_mutations.contains_key("aba"));
        let mut cache: CacheFile =
            serde_json::from_slice(&serde_json::to_vec(&cache).unwrap()).unwrap();

        let error = complete_core_probe_bound_at(
            &mut cache,
            "aba",
            &binding,
            &original_absence,
            &UsageInfo::default(),
            CoreProbeResetMetadata::PreserveExisting,
            TEST_NOW + 1,
        )
        .expect_err("a later absence must not equal the originally observed absence");

        assert!(format!("{error:#}").contains("invalidated or rebound"));
        assert!(!cache.entries.contains_key("aba"));
    }

    #[test]
    fn core_probe_preserves_an_intervening_generation_even_when_it_is_stale() {
        let binding = strict_binding("acct-race", "race@example.com");
        let mut cache = CacheFile::default();
        let baseline = usage_cache_baseline(&cache, "race").unwrap();
        cache.entries.insert(
            "race".to_string(),
            to_entry_with_binding(
                &UsageInfo {
                    cache_revision: Some("intervening-generation".to_string()),
                    secondary: Some(crate::usage::WindowUsage {
                        used_percent: Some(12.0),
                        ..Default::default()
                    }),
                    reset_credits_available_count: Some(1),
                    reset_credits: vec![ResetCredit {
                        id: "intervening-card".to_string(),
                        granted_at: None,
                        expires_at: None,
                    }],
                    ..Default::default()
                },
                1,
                Some(&binding),
            ),
        );
        assert!(
            fresh_usage_bound(&cache, "race", &binding, 10_000, 1).is_none(),
            "the intervening generation is intentionally older than the TTL"
        );

        let (completed, changed) = complete_core_probe_bound_at(
            &mut cache,
            "race",
            &binding,
            &baseline,
            &UsageInfo {
                secondary: Some(crate::usage::WindowUsage {
                    used_percent: Some(88.0),
                    ..Default::default()
                }),
                reset_credits_available_count: Some(1),
                reset_credits: vec![ResetCredit {
                    id: "fresh-card".to_string(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..Default::default()
            },
            CoreProbeResetMetadata::Complete,
            u64::MAX - 1,
        )
        .unwrap();

        assert!(!changed);
        assert_eq!(completed.fetched_at, Some(1));
        assert_eq!(
            completed.cache_revision.as_deref(),
            Some("intervening-generation")
        );
        assert_eq!(
            completed
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.0)
        );
        assert_eq!(completed.reset_credits[0].id, "intervening-card");
        let stored = cache.entries.get("race").unwrap();
        assert_eq!(stored.ts, 1, "the intervening timestamp must remain exact");
        assert_eq!(stored.revision.as_deref(), Some("intervening-generation"));
    }

    #[test]
    fn core_probe_does_not_merge_into_an_intervening_other_account() {
        let expected = strict_binding("acct-expected", "expected@example.com");
        let other = strict_binding("acct-other", "other@example.com");
        let mut cache = CacheFile::default();
        let baseline = usage_cache_baseline(&cache, "rebound").unwrap();
        cache.entries.insert(
            "rebound".to_string(),
            to_entry_with_binding(
                &UsageInfo {
                    cache_revision: Some("other-generation".to_string()),
                    reset_credits: vec![ResetCredit {
                        id: "other-card".to_string(),
                        granted_at: None,
                        expires_at: None,
                    }],
                    ..Default::default()
                },
                7,
                Some(&other),
            ),
        );
        let probe = UsageInfo {
            reset_credits: vec![ResetCredit {
                id: "expected-card".to_string(),
                granted_at: None,
                expires_at: None,
            }],
            ..Default::default()
        };

        let error = complete_core_probe_bound_at(
            &mut cache,
            "rebound",
            &expected,
            &baseline,
            &probe,
            CoreProbeResetMetadata::Complete,
            99,
        )
        .expect_err("an intervening rebind must fail the completion");

        assert!(format!("{error:#}").contains("invalidated or rebound"));
        let stored = from_entry(cache.entries.get("rebound").unwrap()).unwrap();
        assert_eq!(stored.cache_revision.as_deref(), Some("other-generation"));
        assert_eq!(stored.reset_credits[0].id, "other-card");
    }

    #[test]
    fn deferred_reset_metadata_only_merges_into_the_exact_cache_revision() {
        let binding = strict_binding("acct-1", "alice@example.com");
        let old_core = UsageInfo {
            cache_revision: Some("revision-old".to_string()),
            plan_type: Some("plus".to_string()),
            ..UsageInfo::default()
        };
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "alice".to_string(),
            to_entry_with_binding(&old_core, TEST_NOW, Some(&binding)),
        );
        let old_enriched = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![ResetCredit {
                id: "old-card".to_string(),
                ..ResetCredit::default()
            }],
            ..old_core.clone()
        };

        assert!(merge_reset_credit_enrichment(
            &mut cache,
            "alice",
            &binding,
            &old_enriched,
        ));
        assert_eq!(cache.entries["alice"].reset_credits[0].id, "old-card");

        let new_core = UsageInfo {
            cache_revision: Some("revision-new".to_string()),
            plan_type: Some("pro".to_string()),
            ..UsageInfo::default()
        };
        cache.entries.insert(
            "alice".to_string(),
            to_entry_with_binding(&new_core, TEST_NOW + 1, Some(&binding)),
        );

        assert!(!merge_reset_credit_enrichment(
            &mut cache,
            "alice",
            &binding,
            &old_enriched,
        ));
        assert_eq!(cache.entries["alice"].plan_type.as_deref(), Some("pro"));
        assert!(cache.entries["alice"].reset_credits.is_empty());

        let replacement_binding = strict_binding("acct-2", "replacement@example.com");
        let replacement_core = UsageInfo {
            cache_revision: old_core.cache_revision.clone(),
            plan_type: Some("team".to_string()),
            ..UsageInfo::default()
        };
        cache.entries.insert(
            "alice".to_string(),
            to_entry_with_binding(&replacement_core, TEST_NOW + 2, Some(&replacement_binding)),
        );

        assert!(!merge_reset_credit_enrichment(
            &mut cache,
            "alice",
            &binding,
            &old_enriched,
        ));
        assert_eq!(cache.entries["alice"].plan_type.as_deref(), Some("team"));
        assert!(cache.entries["alice"].reset_credits.is_empty());
    }

    #[test]
    fn legacy_unbound_usage_is_an_identity_bound_cache_miss() {
        let legacy_entry: CacheEntry = serde_json::from_value(json!({
            "ts": TEST_NOW,
            "primary_used": 25.0,
            "primary_reset": 456,
            "secondary_used": 75.0,
            "secondary_reset": 789
        }))
        .unwrap();
        let mut cache = CacheFile::default();
        cache.entries.insert("legacy".to_string(), legacy_entry);

        assert!(
            fresh_usage_bound(
                &cache,
                "legacy",
                &strict_binding("acct-1", "alice@example.com"),
                TEST_NOW,
                60,
            )
            .is_none(),
            "missing legacy ownership must not be assigned to the alias's current account"
        );
        assert!(
            fresh_usage(&cache, "legacy", TEST_NOW, 60).is_some(),
            "the compatibility reader remains intentionally unchanged"
        );
    }

    #[test]
    fn a_recorded_verdict_is_readable_while_the_credential_is_unchanged() {
        let mut cache = CacheFile::default();
        record_auth_failure(
            &mut cache,
            "dead",
            "refresh_old",
            "re-login required (refresh_token_reused)",
            "detail",
            TEST_NOW,
        );

        let found = auth_failure_for(&cache, "dead", "refresh_old")
            .expect("the same credential must still be considered rejected");
        assert_eq!(found.summary, "re-login required (refresh_token_reused)");
    }

    #[test]
    fn a_recorded_verdict_does_not_apply_to_a_replacement_credential() {
        // Signing in again is the only cure for a terminal verdict, and it is
        // visible here as a different refresh token. Keying the record on the
        // alias instead would survive the re-login and keep a working account
        // marked dead.
        let mut cache = CacheFile::default();
        record_auth_failure(
            &mut cache,
            "dead",
            "refresh_old",
            "summary",
            "detail",
            TEST_NOW,
        );

        assert!(
            auth_failure_for(&cache, "dead", "refresh_new").is_none(),
            "a verdict about a spent credential says nothing about its replacement"
        );
    }

    #[test]
    fn renaming_a_profile_carries_its_recorded_verdict() {
        let mut cache = CacheFile::default();
        record_auth_failure(
            &mut cache,
            "old",
            "refresh_old",
            "summary",
            "detail",
            TEST_NOW,
        );

        migrate_alias(&mut cache, "old", "new");

        assert!(auth_failure_for(&cache, "old", "refresh_old").is_none());
        assert!(
            auth_failure_for(&cache, "new", "refresh_old").is_some(),
            "a rename must not resurrect network calls for a credential already known dead"
        );
    }

    #[test]
    fn alias_migration_replaces_every_destination_record_with_one_source_generation() {
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "old".to_string(),
            to_entry(
                &UsageInfo {
                    plan_type: Some("source".to_string()),
                    ..UsageInfo::default()
                },
                TEST_NOW,
            ),
        );
        cache.last_used.insert("old".to_string(), 111);

        cache.entries.insert(
            "new".to_string(),
            to_entry(
                &UsageInfo {
                    plan_type: Some("destination".to_string()),
                    ..UsageInfo::default()
                },
                TEST_NOW - 1,
            ),
        );
        cache.last_used.insert("new".to_string(), 222);
        record_auth_failure(
            &mut cache,
            "new",
            "destination-refresh",
            "stale destination verdict",
            "stale destination detail",
            TEST_NOW - 1,
        );

        assert!(migrate_alias(&mut cache, "old", "new"));

        assert!(!cache.entries.contains_key("old"));
        assert!(!cache.last_used.contains_key("old"));
        assert_eq!(cache.entries["new"].plan_type.as_deref(), Some("source"));
        assert_eq!(cache.last_used.get("new"), Some(&111));
        assert!(
            !cache.auth_failures.contains_key("new"),
            "a destination-only auth verdict must not survive into the source generation"
        );
        assert!(cache.usage_mutations.contains_key("old"));
        assert!(cache.usage_mutations.contains_key("new"));
    }

    #[test]
    fn a_batch_snapshot_answers_usage_and_workspace_questions_together() {
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "alice".to_string(),
            to_entry(&UsageInfo::default(), TEST_NOW),
        );
        update_workspace_name(&mut cache, "named", Some("Platform"), TEST_NOW);
        update_workspace_name(&mut cache, "personal", None, TEST_NOW);
        update_workspace_name(
            &mut cache,
            "expired",
            Some("Old Platform"),
            TEST_NOW - WORKSPACE_RESOLUTION_TTL - 1,
        );

        let snapshot = snapshot_from_cache(
            &cache,
            &["alice".to_string(), "missing".to_string()],
            &[
                "named".to_string(),
                "personal".to_string(),
                "expired".to_string(),
            ],
            TEST_NOW,
            60,
        );

        assert!(snapshot.usage.contains_key("alice"));
        assert!(!snapshot.usage.contains_key("missing"));
        assert_eq!(
            snapshot.workspaces.get("named"),
            Some(&WorkspaceState::Named("Platform".to_string()))
        );
        assert_eq!(
            snapshot.workspaces.get("personal"),
            Some(&WorkspaceState::Absent)
        );
        assert_eq!(
            snapshot.workspaces.get("expired"),
            Some(&WorkspaceState::Unresolved)
        );
        assert_eq!(
            snapshot.workspace_fresh_for.get("named"),
            Some(&Duration::from_secs(WORKSPACE_RESOLUTION_TTL))
        );
        assert_eq!(
            snapshot.workspace_fresh_for.get("personal"),
            Some(&Duration::from_secs(WORKSPACE_RESOLUTION_TTL))
        );
        assert!(!snapshot.workspace_fresh_for.contains_key("expired"));
    }

    #[test]
    fn an_identity_bound_snapshot_filters_reused_and_legacy_aliases() {
        let expected = strict_binding("acct-expected", "expected@example.com");
        let other = strict_binding("acct-other", "other@example.com");
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "matching".to_string(),
            to_entry_with_binding(&UsageInfo::default(), TEST_NOW, Some(&expected)),
        );
        cache.entries.insert(
            "reused".to_string(),
            to_entry_with_binding(&UsageInfo::default(), TEST_NOW, Some(&other)),
        );
        cache.entries.insert(
            "legacy".to_string(),
            to_entry(&UsageInfo::default(), TEST_NOW),
        );
        let bindings = HashMap::from([
            ("matching".to_string(), expected.clone()),
            ("reused".to_string(), expected.clone()),
            ("legacy".to_string(), expected),
        ]);

        let snapshot = snapshot_from_cache_bound(&cache, &bindings, &[], TEST_NOW, 60);

        assert!(snapshot.usage.contains_key("matching"));
        assert!(!snapshot.usage.contains_key("reused"));
        assert!(!snapshot.usage.contains_key("legacy"));
    }

    #[test]
    fn a_stored_verdict_keeps_no_control_characters_and_stays_bounded() {
        // `summary`/`detail` are server-controlled. They used to be shown once
        // and discarded; now they are persisted and re-rendered to the terminal
        // on every later listing, which makes escape sequences and unbounded
        // length worth stripping at the point they become durable.
        let mut cache = CacheFile::default();
        let hostile = format!("\u{1b}[31mred\u{1b}[0m\r\n{}", "x".repeat(4096));
        record_auth_failure(
            &mut cache,
            "dead",
            "refresh_old",
            &hostile,
            &hostile,
            TEST_NOW,
        );

        let stored = auth_failure_for(&cache, "dead", "refresh_old").unwrap();
        for field in [&stored.summary, &stored.detail] {
            assert!(
                !field.chars().any(char::is_control),
                "control characters must not survive into the cache: {field:?}"
            );
            assert!(
                field.chars().count() <= STORED_TEXT_MAX,
                "stored text must be bounded, got {} chars",
                field.chars().count()
            );
        }
        assert!(
            stored.summary.contains("red"),
            "the readable part must survive: {:?}",
            stored.summary
        );
    }

    #[test]
    fn auth_failure_batch_matches_each_alias_to_its_exact_credential() {
        let mut cache = CacheFile::default();
        record_auth_failure(
            &mut cache,
            "rejected",
            "refresh-rejected",
            "rejected",
            "credential rejected",
            TEST_NOW,
        );
        record_auth_failure(
            &mut cache,
            "rotated",
            "refresh-old",
            "old",
            "old credential rejected",
            TEST_NOW,
        );
        let credentials = HashMap::from([
            ("rejected".to_string(), "refresh-rejected".to_string()),
            ("rotated".to_string(), "refresh-new".to_string()),
            ("healthy".to_string(), "refresh-healthy".to_string()),
        ]);

        let failures = auth_failures_for(&cache, &credentials);

        assert_eq!(failures.len(), 1);
        assert_eq!(failures["rejected"].summary, "rejected");
        assert!(!failures.contains_key("rotated"));
        assert!(!failures.contains_key("healthy"));
    }

    #[test]
    fn empty_auth_failure_batch_needs_no_cache_snapshot() {
        assert!(get_auth_failures(&HashMap::new()).unwrap().is_empty());
    }

    #[test]
    fn invalidating_an_alias_also_drops_its_recorded_verdict() {
        // `invalidate` is what makes the forced re-fetch after a reset-card
        // consume actually reach the network.
        let mut cache = CacheFile::default();
        record_auth_failure(
            &mut cache,
            "dead",
            "refresh_old",
            "summary",
            "detail",
            TEST_NOW,
        );

        assert!(drop_fetch_state(&mut cache, "dead"));

        assert!(auth_failure_for(&cache, "dead", "refresh_old").is_none());
    }

    #[test]
    fn identity_bound_invalidation_never_drops_a_reused_alias_owner() {
        let expected = strict_binding("acct_expected", "expected@example.com");
        let replacement = strict_binding("acct_replacement", "replacement@example.com");
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "account".into(),
            to_entry_with_binding(&UsageInfo::default(), TEST_NOW, Some(&replacement)),
        );
        record_auth_failure(
            &mut cache,
            "account",
            "replacement-refresh",
            "summary",
            "detail",
            TEST_NOW,
        );

        assert!(!drop_fetch_state_bound(&mut cache, "account", &expected));
        assert!(cache.entries.contains_key("account"));
        assert!(auth_failure_for(&cache, "account", "replacement-refresh").is_some());

        assert!(drop_fetch_state_bound(&mut cache, "account", &replacement));
        assert!(!cache.entries.contains_key("account"));
        assert!(auth_failure_for(&cache, "account", "replacement-refresh").is_none());
    }

    #[test]
    fn purging_a_profile_drops_usage_selection_history_and_auth_failure() {
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "reused".to_string(),
            to_entry(&UsageInfo::default(), TEST_NOW),
        );
        cache.last_used.insert("reused".to_string(), 123);
        record_auth_failure(
            &mut cache,
            "reused",
            "refresh_old",
            "summary",
            "detail",
            TEST_NOW,
        );

        assert!(drop_profile_state(&mut cache, "reused"));
        assert!(!cache.entries.contains_key("reused"));
        assert!(!cache.last_used.contains_key("reused"));
        assert!(auth_failure_for(&cache, "reused", "refresh_old").is_none());
    }

    #[test]
    fn cache_mutation_load_rejects_malformed_state() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "not-json").unwrap();

        let error = match load_cache_checked_at(&path) {
            Err(error) => error,
            Ok(_) => {
                panic!("cache mutation must stop rather than treating corrupt state as empty")
            }
        };

        assert!(format!("{error:#}").contains("parsing cache file"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not-json");
    }

    #[test]
    fn automatic_ranking_history_rejects_malformed_cache_state() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "not-json").unwrap();

        let error = load_last_used_checked_at(&path)
            .expect_err("automatic ranking must not invent zero timestamps for corrupt state");

        assert!(format!("{error:#}").contains("parsing cache file"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not-json");
    }

    #[test]
    fn cache_mutation_load_treats_only_a_missing_file_as_empty() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("missing-cache.json");

        let cache = load_cache_checked_at(&path).unwrap();

        assert!(cache.entries.is_empty());
        assert!(cache.last_used.is_empty());
        assert!(cache.auth_failures.is_empty());
    }

    #[tokio::test]
    async fn async_cache_read_propagates_a_blocking_worker_join_failure() {
        TEST_PANIC_NEXT_CACHE_READ_WORKER.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = get_async("worker-panic")
            .await
            .expect_err("a blocking worker panic must not become a cache miss");

        assert!(
            format!("{error:#}").contains("usage-cache read worker failed"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn async_auth_failure_read_propagates_a_blocking_worker_join_failure() {
        TEST_PANIC_NEXT_AUTH_FAILURE_CACHE_READ_WORKER
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let error = get_auth_failure_async("worker-panic", "refresh")
            .await
            .expect_err("a blocking worker panic must not authorize another token request");

        assert!(
            format!("{error:#}").contains("auth-failure cache read worker failed"),
            "{error:#}"
        );
    }

    #[test]
    fn a_confirmed_absence_is_remembered_so_the_account_is_not_looked_up_again() {
        // Personal plans have no workspace name. The server saying so is an
        // answer; not storing it made every invocation ask the same question.
        let mut cache = CacheFile::default();
        assert_eq!(
            workspace_state(&cache, "acct-personal", TEST_NOW),
            WorkspaceState::Unresolved
        );

        assert!(update_workspace_name(
            &mut cache,
            "acct-personal",
            None,
            TEST_NOW,
        ));

        assert_eq!(
            workspace_state(&cache, "acct-personal", TEST_NOW),
            WorkspaceState::Absent
        );
    }

    #[test]
    fn a_changed_workspace_name_supersedes_the_previous_resolution() {
        let mut cache = CacheFile::default();
        update_workspace_name(&mut cache, "acct", Some("Old Name"), TEST_NOW);

        assert!(update_workspace_name(
            &mut cache,
            "acct",
            Some("Night City"),
            TEST_NOW + 1,
        ));

        assert_eq!(
            workspace_state(&cache, "acct", TEST_NOW + 1),
            WorkspaceState::Named("Night City".to_string())
        );
    }

    #[test]
    fn named_and_absent_workspace_resolutions_expire_consistently() {
        let mut cache = CacheFile::default();
        let expired_at = TEST_NOW - WORKSPACE_RESOLUTION_TTL - 1;
        update_workspace_name(&mut cache, "named", Some("Old Name"), expired_at);
        update_workspace_name(&mut cache, "absent", None, expired_at);

        assert_eq!(
            workspace_state(&cache, "named", TEST_NOW),
            WorkspaceState::Unresolved
        );
        assert_eq!(
            workspace_state(&cache, "absent", TEST_NOW),
            WorkspaceState::Unresolved
        );
    }

    #[test]
    fn authoritative_absence_clears_a_stale_jwt_workspace_name() {
        let mut cache = CacheFile::default();
        update_workspace_name(&mut cache, "acct-team", Some("Old Team"), TEST_NOW);
        update_workspace_name(&mut cache, "acct-team", None, TEST_NOW + 1);
        let mut info = crate::jwt::AccountInfo {
            account_id: Some("acct-team".to_string()),
            workspace_name: Some("Old Team".to_string()),
            ..Default::default()
        };

        let state = workspace_state(&cache, "acct-team", TEST_NOW + 1);
        apply_workspace_state(&mut info, &state);

        assert_eq!(state, WorkspaceState::Absent);
        assert_eq!(info.workspace_name, None);
    }

    #[test]
    fn legacy_workspace_cache_migrates_without_inventing_freshness() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "entries": {},
                "last_used": {},
                "workspace_names": { "named": "Legacy Team" },
                "workspace_names_absent": { "personal": TEST_NOW },
                "auth_failures": {}
            }))
            .unwrap(),
        )
        .unwrap();

        let cache = load_cache_checked_at(&path).unwrap();

        assert_eq!(
            workspace_state(&cache, "named", TEST_NOW),
            WorkspaceState::Unresolved,
            "legacy names have no timestamp and must be revalidated"
        );
        assert_eq!(
            workspace_state(&cache, "personal", TEST_NOW),
            WorkspaceState::Absent,
            "legacy absence timestamps can migrate exactly"
        );
        let serialized = serde_json::to_value(cache).unwrap();
        assert!(serialized.get("workspace_names").is_none());
        assert!(serialized.get("workspace_names_absent").is_none());
        assert_eq!(
            serialized.pointer("/workspaces/personal/ts"),
            Some(&json!(TEST_NOW))
        );
    }

    #[test]
    fn cache_mutation_waits_for_cross_process_lock() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_path = lock_path.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(with_cache_file_lock_at(
                    &worker_path,
                    Duration::from_secs(1),
                    || Ok(()),
                ))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "cache mutation must wait for an independently-held OS lock"
        );

        drop(holder);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn external_cache_wait_does_not_hold_process_serialization() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let blocked_path = dir.path().join("blocked.lock");
        let unrelated_path = dir.path().join("unrelated.lock");
        let holder = open_cache_lock_file(&blocked_path).unwrap();
        FileExt::lock(&holder).unwrap();

        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let blocked_worker = std::thread::spawn(move || {
            waiting_tx.send(()).unwrap();
            with_cache_lock_at(&blocked_path, Duration::from_secs(5), || Ok(()))
        });
        waiting_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let (unrelated_tx, unrelated_rx) = std::sync::mpsc::channel();
        let unrelated_worker = std::thread::spawn(move || {
            unrelated_tx
                .send(with_cache_lock_at(
                    &unrelated_path,
                    Duration::from_secs(1),
                    || Ok(7),
                ))
                .unwrap();
        });
        let unrelated = unrelated_rx.recv_timeout(Duration::from_secs(1));

        drop(holder);
        blocked_worker.join().unwrap().unwrap();
        unrelated_worker.join().unwrap();
        assert_eq!(
            unrelated
                .expect("an external lock wait blocked an unrelated in-process cache path")
                .unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn cancellable_snapshot_does_not_wait_for_a_held_cache_lock() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();

        let control = CacheLockAcquireControl::new();
        let worker_control = control.clone();
        let task = tokio::spawn(async move {
            get_snapshot_async_cancellable_at(
                cache_path,
                lock_path,
                Vec::new(),
                Vec::new(),
                TEST_NOW,
                60,
                &worker_control,
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(control.cancel_waiting());

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled cache waiter must settle promptly")
            .unwrap()
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn contended_cache_cancellation_and_timeout_have_one_winner() {
        let cancelled = CacheLockAcquireControl::new();
        assert!(cancelled.mark_contended());
        assert!(cancelled.cancel_contended());
        assert!(!cancelled.mark_timed_out());

        let timed_out = CacheLockAcquireControl::new();
        assert!(timed_out.mark_contended());
        assert!(timed_out.mark_timed_out());
        assert!(!timed_out.cancel_contended());
        assert!(!timed_out.cancel_waiting());
    }

    #[tokio::test]
    async fn cancellable_snapshot_measures_freshness_after_lock_wait() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let lock_path = dir.path().join("cache.lock");
        let binding = strict_binding("acct", "account@example.com");
        let initial_second = u64::try_from(auth::now_unix_secs().unwrap()).unwrap();
        while u64::try_from(auth::now_unix_secs().unwrap()).unwrap() == initial_second {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let recorded_at = u64::try_from(auth::now_unix_secs().unwrap()).unwrap();
        let mut cache = CacheFile::default();
        cache.entries.insert(
            "account".into(),
            to_entry_with_binding(&UsageInfo::default(), recorded_at, Some(&binding)),
        );
        save_cache_at(&cache_path, &cache).unwrap();
        let holder = open_cache_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();

        let control = CacheLockAcquireControl::new();
        let worker_control = control.clone();
        let bindings = [("account".to_string(), binding)].into_iter().collect();
        let task = tokio::spawn(async move {
            get_snapshot_bound_async_cancellable_at(
                cache_path,
                lock_path,
                bindings,
                Vec::new(),
                0,
                &worker_control,
            )
            .await
        });
        // Give the blocking reader time to reach the held OS lock, then cross
        // the entry's zero-TTL second while it is still waiting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        while u64::try_from(auth::now_unix_secs().unwrap()).unwrap() <= recorded_at {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(holder);

        let snapshot = task.await.unwrap().unwrap().unwrap();
        assert!(!snapshot.usage.contains_key("account"));
    }

    #[test]
    fn cancellation_loses_after_the_cache_lock_is_acquired() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let control = CacheLockAcquireControl::new();
        let worker_control = control.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();

        let worker = std::thread::spawn(move || {
            with_cache_lock_cancellable_at(
                &lock_path,
                Duration::from_secs(5),
                &worker_control,
                || {
                    acquired_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();
                    Ok(7)
                },
            )
        });
        acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!control.cancel_waiting());
        finish_tx.send(()).unwrap();

        assert_eq!(worker.join().unwrap().unwrap(), Some(7));
    }

    #[test]
    fn cache_atomic_write_replaces_existing_file() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut first = CacheFile::default();
        first.last_used.insert("alice".into(), 1);
        save_cache_at(&path, &first).unwrap();

        let mut second = CacheFile::default();
        second.last_used.insert("bob".into(), 2);
        save_cache_at(&path, &second).unwrap();

        let saved: CacheFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.last_used.get("bob"), Some(&2));
        assert!(!saved.last_used.contains_key("alice"));
    }

    #[test]
    fn cache_lock_timeout_preserves_live_lock_file() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        std::fs::write(&lock_path, "holder-marker").unwrap();
        FileExt::lock(&holder).unwrap();

        let err =
            with_cache_file_lock_at(&lock_path, Duration::from_millis(25), || Ok(())).unwrap_err();
        assert!(err.to_string().contains("cache lock"));
        let reopened = open_cache_lock_file(&lock_path).unwrap();
        assert!(matches!(
            FileExt::try_lock(&reopened),
            Err(fs4::TryLockError::WouldBlock)
        ));
        FileExt::unlock(&holder).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            "holder-marker"
        );
    }
}
