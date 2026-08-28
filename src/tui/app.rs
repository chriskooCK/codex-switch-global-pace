use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{DefaultTerminal, style::Style, text::Line};
use tokio::sync::Semaphore;

use crate::auth;
use crate::cache;
use crate::jwt::{AccountInfo, StrictAccountBinding};
use crate::login;
use crate::output::{format_local_datetime, format_local_timestamp, reset_credits_count};
use crate::profile::{
    self, TuiAuthReconciliation, cmd_delete, profile_auth_path, read_current_checked,
    rename_profile, sync_current_from_live, validate_alias,
};
use crate::safe_text;
use crate::usage::{
    ConsumeResetCreditError, ConsumedResetCredit, GlobalPaceAccountInput, GlobalWeeklySummary,
    Refresh, ResetCredit, UsageError, UsageInfo, calculate_global_weekly_summary,
    reset_credit_expiry_sort_key,
};
use crate::warmup::ModelEntry;

const STATUS_MESSAGE_MAX_CHARS: usize = 1024;

#[derive(Debug, Clone)]
pub struct AccountEntry {
    pub alias: String,
    pub info: AccountInfo,
    pub usage: UsageStatus,
    pub is_current: bool,
}

#[derive(Debug, Clone)]
pub enum UsageStatus {
    Idle,
    Loading,
    Loaded(Box<UsageInfo>),
    Error(UsageError),
}

#[derive(Debug, Clone, Copy)]
enum CachedUsageApplication {
    RequestedRefresh,
    Startup,
}

#[derive(Debug, Clone)]
struct WorkspaceMemoryResolution {
    state: cache::WorkspaceState,
    fresh_until: Instant,
}

impl WorkspaceMemoryResolution {
    fn is_fresh(&self, now: Instant) -> bool {
        now < self.fresh_until && !matches!(self.state, cache::WorkspaceState::Unresolved)
    }
}

fn retained_usage_by_identity(
    accounts: Vec<AccountEntry>,
) -> HashMap<String, (Option<StrictAccountBinding>, UsageStatus)> {
    accounts
        .into_iter()
        .map(|account| {
            (
                account.alias,
                (strict_account_identity(&account.info), account.usage),
            )
        })
        .collect()
}

fn strict_account_identity(info: &AccountInfo) -> Option<StrictAccountBinding> {
    info.strict_binding()
}

fn read_profile_auth_expiries(
    alias: &str,
    expected: &StrictAccountBinding,
) -> Result<Vec<super::menu::AuthExpiry>> {
    let path = profile_auth_path(alias)?;
    let auth = auth::read_auth(&path)
        .with_context(|| format!("reading profile auth for account details: {alias}"))?;
    let actual = auth::account_info_from_auth_value(&auth)
        .strict_binding()
        .context("profile auth is missing a complete account id and email identity")?;
    anyhow::ensure!(
        actual == *expected,
        "profile identity changed while account details were loading"
    );

    let mut expiries = Vec::new();
    if let Some(token) = auth::extract_id_token(&auth) {
        expiries.push(super::menu::AuthExpiry {
            name: "ID token".to_string(),
            expires_at: crate::jwt::token_expires_at(&token),
        });
    }
    if let Some(token) = auth
        .pointer("/tokens/access_token")
        .and_then(serde_json::Value::as_str)
    {
        expiries.push(super::menu::AuthExpiry {
            name: "Access token".to_string(),
            expires_at: crate::jwt::token_expires_at(token),
        });
    }
    Ok(expiries)
}

fn refresh_fetches_loaded_usage(refresh: Refresh) -> bool {
    !matches!(refresh, Refresh::Cached)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceRefresh {
    Skip,
    IfStale,
    Forced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AccountRefreshPlan {
    usage: Option<Refresh>,
    workspace: WorkspaceRefresh,
}

impl AccountRefreshPlan {
    fn usage_and_workspace(usage: Refresh) -> Self {
        let workspace = match usage {
            Refresh::Cached | Refresh::Unattended => WorkspaceRefresh::IfStale,
            Refresh::Forced => WorkspaceRefresh::Forced,
        };
        Self {
            usage: Some(usage),
            workspace,
        }
    }

    fn usage_only(usage: Refresh) -> Self {
        Self {
            usage: Some(usage),
            workspace: WorkspaceRefresh::Skip,
        }
    }

    fn workspace_only(workspace: WorkspaceRefresh) -> Self {
        Self {
            usage: None,
            workspace,
        }
    }

    fn resume_cancelled_usage(usage: Refresh) -> Self {
        let usage = match usage {
            // A cancelled cache-miss request must not reopen the alias cache;
            // resume at the network/auth-verdict stage it had already reached.
            Refresh::Cached => Refresh::Unattended,
            Refresh::Unattended => Refresh::Unattended,
            Refresh::Forced => Refresh::Forced,
        };
        Self::usage_only(usage)
    }

    fn merged_with(self, other: Self) -> Self {
        Self {
            usage: merge_usage_refresh(self.usage, other.usage),
            workspace: merge_workspace_refresh(self.workspace, other.workspace),
        }
    }

    fn needs_follow_up(self) -> bool {
        self.usage.is_some_and(refresh_fetches_loaded_usage)
            || !matches!(self.workspace, WorkspaceRefresh::Skip)
    }
}

fn merge_usage_refresh(left: Option<Refresh>, right: Option<Refresh>) -> Option<Refresh> {
    match (left, right) {
        (None, refresh) | (refresh, None) => refresh,
        (Some(Refresh::Forced), _) | (_, Some(Refresh::Forced)) => Some(Refresh::Forced),
        (Some(Refresh::Unattended), _) | (_, Some(Refresh::Unattended)) => {
            Some(Refresh::Unattended)
        }
        (Some(Refresh::Cached), Some(Refresh::Cached)) => Some(Refresh::Cached),
    }
}

async fn publish_usage_lease_release(
    sender: &tokio::sync::mpsc::Sender<(String, u64)>,
    alias: &str,
    request_id: u64,
) {
    let _ = sender.send((alias.to_string(), request_id)).await;
}

fn merge_workspace_refresh(left: WorkspaceRefresh, right: WorkspaceRefresh) -> WorkspaceRefresh {
    match (left, right) {
        (WorkspaceRefresh::Forced, _) | (_, WorkspaceRefresh::Forced) => WorkspaceRefresh::Forced,
        (WorkspaceRefresh::IfStale, _) | (_, WorkspaceRefresh::IfStale) => {
            WorkspaceRefresh::IfStale
        }
        (WorkspaceRefresh::Skip, WorkspaceRefresh::Skip) => WorkspaceRefresh::Skip,
    }
}

#[derive(Debug)]
pub struct ResetCardFailure {
    message: String,
    invalidate_cache: bool,
}

fn map_reset_card_failure(message: String, invalidate_cache: bool) -> ResetCardFailure {
    ResetCardFailure {
        message,
        invalidate_cache,
    }
}

/// The actual `outcome_unknown_after_request` -> `invalidate_cache` routing decision,
/// isolated from `ConsumeResetCreditError` so it can be unit-tested directly instead of
/// only through a literal struct construction (a reset card is a non-renewable resource:
/// routing an unknown outcome to "definite failure" would let the UI offer to burn a
/// second card after the first attempt may have already consumed one).
fn reset_card_failure_from_outcome(
    unknown: bool,
    unknown_message: String,
    definite_message: String,
) -> ResetCardFailure {
    if unknown {
        map_reset_card_failure(unknown_message, true)
    } else {
        map_reset_card_failure(definite_message, false)
    }
}

fn reset_card_failure_from_consume_error(
    alias: &str,
    error: ConsumeResetCreditError,
) -> ResetCardFailure {
    let unknown = error.outcome_unknown_after_request();
    reset_card_failure_from_outcome(
        unknown,
        error.user_facing_unknown_message(alias),
        format!("Reset card failed ({alias}): {error}"),
    )
}

#[derive(Debug, Default)]
struct BatchDeleteReport {
    committed: usize,
    durability_warnings: Vec<String>,
    failures: Vec<String>,
}

impl BatchDeleteReport {
    fn record(&mut self, alias: &str, result: Result<profile::ProfileMutationOutcome>) {
        match result {
            Ok(outcome) => {
                self.committed += 1;
                if let Some(warning) = outcome.durability_warning() {
                    self.durability_warnings
                        .push(format!("{alias}: {warning:#}"));
                }
            }
            Err(error) => self.failures.push(format!("{alias}: {error:#}")),
        }
    }

    fn message(&self) -> String {
        let mut message = format!("Deleted {} account(s) (recoverable)", self.committed);
        if !self.durability_warnings.is_empty() {
            message.push_str(&format!(
                "; durability unconfirmed for {}: {}",
                self.durability_warnings.len(),
                self.durability_warnings.join("; ")
            ));
        }
        if !self.failures.is_empty() {
            message.push_str(&format!("; {} failed", self.failures.len()));
        }
        message
    }
}

#[derive(Debug, Clone)]
pub enum ModelStatus {
    Loading,
    Loaded(Vec<ModelEntry>),
    Error(String),
}

type ModelTaskResult = (
    String,
    StrictAccountBinding,
    u64,
    Result<Vec<ModelEntry>, String>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Quota,
    Status,
}

impl SortMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Quota => "quota",
            SortMode::Status => "status",
        }
    }
}

pub enum ConfirmAction {
    Delete(String),
    BatchDelete(Vec<String>),
    ConsumeResetCard {
        alias: String,
        credit: ResetCredit,
        expires_at: String,
    },
}

pub struct RenameState {
    pub old_alias: String,
    pub input: String,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub query: String,
    pub cursor: usize,
}

#[derive(Debug)]
pub struct WarmupTask {
    alias: String,
    binding: StrictAccountBinding,
    origin: WarmupOrigin,
    started: Instant,
    slow_reported: bool,
    lease_control: profile::ProfileLeaseAcquireControl,
    network_wait: SafeTaskCancellation,
    model_discovery: SafeTaskCancellation,
    handle: tokio::task::JoinHandle<std::result::Result<(), String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarmupOrigin {
    Manual,
    Automatic,
}

#[derive(Debug, Clone)]
enum WarmupPreflightOrigin {
    Single { alias: String },
    Marked,
    Automatic { refreshing_accounts: usize },
}

#[derive(Debug)]
struct WarmupPreflightCandidate {
    alias: String,
    binding: StrictAccountBinding,
    loaded_usage: Option<UsageInfo>,
}

#[derive(Debug)]
struct WarmupReadyCandidate {
    alias: String,
    binding: StrictAccountBinding,
    cached_usage: Option<UsageInfo>,
}

#[derive(Debug)]
struct WarmupPreflightTask {
    origin: WarmupPreflightOrigin,
    candidate_count: usize,
    aliases: BTreeSet<String>,
    control: cache::CacheLockAcquireControl,
    handle: tokio::task::JoinHandle<Result<Option<Vec<WarmupReadyCandidate>>>>,
}

async fn inspect_warmup_candidates(
    candidates: Vec<WarmupPreflightCandidate>,
    control: cache::CacheLockAcquireControl,
) -> Result<Option<Vec<WarmupReadyCandidate>>> {
    let disk_bindings = candidates
        .iter()
        .filter(|candidate| candidate.loaded_usage.is_none())
        .map(|candidate| (candidate.alias.clone(), candidate.binding.clone()))
        .collect::<HashMap<_, _>>();
    let mut cached_usage = if disk_bindings.is_empty() {
        HashMap::new()
    } else {
        let Some(snapshot) =
            crate::cache::get_snapshot_bound_async_cancellable(&disk_bindings, &[], &control)
                .await
                .context("reading cached usage for warmup candidates")?
        else {
            return Ok(None);
        };
        snapshot.usage
    };
    let now = crate::auth::now_unix_secs().context("reading system clock for warmup preflight")?;
    let mut ready = Vec::new();
    for candidate in candidates {
        let usage = match candidate.loaded_usage {
            Some(usage) => Some(usage),
            None => cached_usage.remove(&candidate.alias),
        };
        let has_active_window = usage
            .as_ref()
            .is_some_and(|usage| crate::usage::usage_has_active_warmup_window(usage, now));
        if !has_active_window {
            ready.push(WarmupReadyCandidate {
                alias: candidate.alias,
                binding: candidate.binding,
                cached_usage: usage,
            });
        }
    }
    ready.sort_by(|left, right| left.alias.cmp(&right.alias));
    Ok(Some(ready))
}

#[derive(Debug, Clone, Copy)]
enum AccountTaskKind {
    Usage { request_id: u64 },
    Model { request_id: u64 },
    ResetCard,
    SwitchPrepare,
    SwitchSync,
    SwitchCommit,
}

impl AccountTaskKind {
    fn is_profile_switch(self) -> bool {
        matches!(
            self,
            Self::SwitchPrepare | Self::SwitchSync | Self::SwitchCommit
        )
    }

    fn profile_switch_progress(self, alias: &str) -> Option<String> {
        match self {
            Self::SwitchPrepare => Some(format!("Preparing switch to {alias}...")),
            Self::SwitchSync => Some(format!(
                "Synchronizing the current Codex login before switching to {alias}..."
            )),
            Self::SwitchCommit => Some(format!("Switching to {alias}...")),
            Self::Usage { .. } | Self::Model { .. } | Self::ResetCard => None,
        }
    }
}

#[derive(Debug)]
struct SafeTaskCancellationState {
    state: std::sync::atomic::AtomicU8,
    completed: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

const SAFE_TASK_WAITING: u8 = 0;
const SAFE_TASK_CANCELLED: u8 = 1;
const SAFE_TASK_COMMITTED: u8 = 2;

/// One cancellation boundary explicitly owned by a worker. `begin_work` and
/// `request` compete through a single state transition, so cancellation cannot
/// cross from a safe wait into token refresh or another credential mutation.
#[derive(Clone, Debug)]
pub(super) struct SafeTaskCancellation {
    state: Arc<SafeTaskCancellationState>,
}

impl SafeTaskCancellation {
    fn new() -> Self {
        Self {
            state: Arc::new(SafeTaskCancellationState {
                state: std::sync::atomic::AtomicU8::new(SAFE_TASK_WAITING),
                completed: std::sync::atomic::AtomicBool::new(false),
                wake: tokio::sync::Notify::new(),
            }),
        }
    }

    pub(super) fn request(&self) -> bool {
        let cancelled = self
            .state
            .state
            .compare_exchange(
                SAFE_TASK_WAITING,
                SAFE_TASK_CANCELLED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok();
        if cancelled {
            self.state.wake.notify_one();
        }
        cancelled
    }

    fn begin_work(&self) -> bool {
        self.state
            .state
            .compare_exchange(
                SAFE_TASK_WAITING,
                SAFE_TASK_COMMITTED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    async fn cancelled(&self) {
        loop {
            if self.state.state.load(std::sync::atomic::Ordering::Acquire) == SAFE_TASK_CANCELLED {
                return;
            }
            self.state.wake.notified().await;
        }
    }

    fn mark_completed(&self) {
        debug_assert!(
            self.state.state.load(std::sync::atomic::Ordering::Acquire) == SAFE_TASK_CANCELLED
        );
        self.state
            .completed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn completed(&self) -> bool {
        self.state
            .completed
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

fn cancellable_first_network_permit(
    limiter: Arc<tokio::sync::Semaphore>,
    cancellation: SafeTaskCancellation,
) -> crate::usage::FirstNetworkPermit {
    let first_wait = crate::usage::first_network_permit(limiter);
    Box::pin(async move {
        tokio::select! {
            permit = first_wait => match permit {
                Ok(Some(permit)) if cancellation.begin_work() => Ok(Some(permit)),
                Ok(Some(_)) | Ok(None) => {
                    cancellation.mark_completed();
                    Ok(None)
                }
                Err(error) => Err(error),
            },
            _ = cancellation.cancelled() => {
                cancellation.mark_completed();
                Ok(None)
            },
        }
    })
}

#[derive(Clone, Debug)]
pub(super) enum CredentialTaskCancellation {
    Safe(SafeTaskCancellation),
    Usage(crate::usage::UsageTaskCancellation),
}

impl CredentialTaskCancellation {
    pub(super) fn request(&self) -> bool {
        match self {
            Self::Safe(control) => control.request(),
            Self::Usage(control) => control.request(),
        }
    }
}

#[derive(Debug)]
struct AccountTask {
    alias: String,
    kind: AccountTaskKind,
    lease_control: profile::ProfileLeaseAcquireControl,
    followup_controls: Vec<cache::CacheLockAcquireControl>,
    network_wait: Option<SafeTaskCancellation>,
    read_only_work: Option<SafeTaskCancellation>,
    usage_work: Option<crate::usage::UsageTaskCancellation>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Default)]
struct AccountTaskControls {
    followup_controls: Vec<cache::CacheLockAcquireControl>,
    network_wait: Option<SafeTaskCancellation>,
    read_only_work: Option<SafeTaskCancellation>,
    usage_work: Option<crate::usage::UsageTaskCancellation>,
}

#[derive(Debug)]
struct StartupCacheTask {
    control: cache::CacheLockAcquireControl,
    handle: tokio::task::JoinHandle<Result<Option<cache::CacheSnapshot>>>,
}

#[derive(Debug)]
struct StartupProfileTask {
    handle: tokio::task::JoinHandle<Result<ProfileReloadSnapshot>>,
}

#[derive(Debug)]
struct StartupHttpClientTask {
    handle: tokio::task::JoinHandle<Result<reqwest::Client>>,
}

#[derive(Debug)]
struct StartupSelfUpdateCleanupTask {
    handle:
        tokio::task::JoinHandle<std::result::Result<bool, crate::update::PendingSelfUpdateCleanup>>,
}

struct StartupFileLogInitTask {
    handle: tokio::task::JoinHandle<Result<()>>,
}

#[derive(Debug)]
struct AuthExpiryTask {
    alias: String,
    binding: StrictAccountBinding,
    request_id: u64,
    handle: tokio::task::JoinHandle<Result<Vec<super::menu::AuthExpiry>>>,
}

#[derive(Debug)]
struct WorkspaceCacheWriteTask {
    account_id: String,
    generation: u64,
    control: cache::CacheLockAcquireControl,
    handle: tokio::task::JoinHandle<Result<Option<bool>>>,
}

#[derive(Debug)]
struct WorkspaceLookupTask {
    account_id: String,
    generation: u64,
    handle: tokio::task::JoinHandle<()>,
}

/// Capture one identity-checked workspace credential snapshot, then release the
/// profile boundary before the independent network-capacity wait begins.
async fn prepare_workspace_lookup_auth(
    alias: &str,
    path: &std::path::Path,
    binding: &StrictAccountBinding,
) -> Result<serde_json::Value> {
    let lease = profile::acquire_profile_lease_async(alias.to_string())
        .await
        .context("acquiring profile lease for workspace lookup")?;
    let auth = crate::auth::read_auth_async(path)
        .await
        .with_context(|| format!("reading profile auth {}", path.display()))?;
    let actual = crate::auth::account_info_from_auth_value(&auth)
        .strict_binding()
        .context("profile auth is missing a complete account id and email identity")?;
    anyhow::ensure!(
        &actual == binding,
        "profile identity changed before workspace lookup"
    );
    drop(lease);
    Ok(auth)
}

#[derive(Debug)]
struct UsageCacheInvalidationTask {
    alias: String,
    binding: StrictAccountBinding,
    refresh_after: Option<AccountRefreshPlan>,
    warning_on_failure: Option<String>,
    handle: tokio::task::JoinHandle<Result<bool>>,
}

#[derive(Debug, Clone)]
enum ProfileMutationKind {
    Delete { alias: String },
    BatchDelete { aliases: Vec<String> },
    Rename { old: String, new: String },
}

#[derive(Debug)]
struct ProfileReloadSnapshot {
    current: Option<String>,
    accounts: Vec<(String, AccountInfo)>,
}

#[derive(Debug)]
struct ProfileMutationCompletion<T> {
    result: T,
    reload: Result<ProfileReloadSnapshot>,
}

#[derive(Debug)]
enum ProfileMutationOutput {
    Delete(ProfileMutationCompletion<Result<profile::ProfileMutationOutcome>>),
    BatchDelete(ProfileMutationCompletion<BatchDeleteReport>),
    Rename(ProfileMutationCompletion<Result<profile::ProfileMutationOutcome>>),
}

#[derive(Debug)]
struct ProfileMutationTask {
    kind: ProfileMutationKind,
    handle: tokio::task::JoinHandle<ProfileMutationOutput>,
}

fn load_profile_reload_snapshot(reconcile_live: bool) -> Result<ProfileReloadSnapshot> {
    let current = if reconcile_live {
        sync_current_from_live().context("synchronizing the active profile after profile change")?
    } else {
        read_current_checked().context("reading the active profile marker after profile change")?
    };
    let accounts = profile::load_profile_accounts()
        .context("loading profiles after profile change")?
        .into_iter()
        .map(|account| (account.alias, account.info))
        .collect();
    Ok(ProfileReloadSnapshot { current, accounts })
}

const WARMUP_SLOW_NOTICE: Duration = Duration::from_secs(60);

pub struct App {
    pub accounts: Vec<AccountEntry>,
    pub selected: usize,
    pub search: Option<SearchState>,
    pub search_active: bool,
    pub sort_mode: SortMode,
    pub view_indices: Vec<usize>,
    pub marked: BTreeSet<String>,
    pub status_msg: Option<String>,
    pub status_is_error: bool,
    pub status_expiry: Option<Instant>,
    pub refreshing_requests: HashMap<String, (u64, Refresh)>,
    pending_usage_refreshes: HashMap<String, AccountRefreshPlan>,
    /// Refresh work that could not start while a profile switch owned the
    /// credential boundary. Usage and workspace intent are retained separately.
    deferred_post_switch_usage_refreshes: BTreeMap<String, AccountRefreshPlan>,
    pub usage_next_id: u64,
    pub pending_results: tokio::sync::mpsc::Receiver<(
        String,
        StrictAccountBinding,
        u64,
        Result<UsageInfo, UsageError>,
    )>,
    pub result_sender: tokio::sync::mpsc::Sender<(
        String,
        StrictAccountBinding,
        u64,
        Result<UsageInfo, UsageError>,
    )>,
    pending_usage_enrichment:
        tokio::sync::mpsc::Receiver<(String, StrictAccountBinding, u64, UsageInfo)>,
    usage_enrichment_sender:
        tokio::sync::mpsc::Sender<(String, StrictAccountBinding, u64, UsageInfo)>,
    pending_usage_lease_releases: tokio::sync::mpsc::Receiver<(String, u64)>,
    usage_lease_release_sender: tokio::sync::mpsc::Sender<(String, u64)>,
    usage_generations: HashMap<String, u64>,
    usage_lease_release_generations: HashMap<String, u64>,
    usage_metadata_requests: HashMap<String, u64>,
    pub pending_workspace: tokio::sync::mpsc::Receiver<(
        String,
        StrictAccountBinding,
        u64,
        Result<cache::WorkspaceState, String>,
    )>,
    pub workspace_sender: tokio::sync::mpsc::Sender<(
        String,
        StrictAccountBinding,
        u64,
        Result<cache::WorkspaceState, String>,
    )>,
    workspace_states: HashMap<String, WorkspaceMemoryResolution>,
    /// Latest workspace lookup still in flight for each stable account id.
    /// A forced request replaces the generation; late completions can then be
    /// discarded without touching either memory or disk cache state.
    workspace_requests: HashMap<String, u64>,
    workspace_next_id: u64,
    workspace_lookup_tasks: HashMap<u64, WorkspaceLookupTask>,
    /// Derived workspace-cache writes must not hold up result application on
    /// the event loop. They remain tracked so shutdown can cancel only work
    /// that has not acquired the cache lock yet and drain anything already
    /// committed to finishing.
    workspace_cache_writes: HashMap<u64, WorkspaceCacheWriteTask>,
    workspace_cache_latest: HashMap<String, u64>,
    workspace_cache_write_next_id: u64,
    workspace_next_expiry: Option<Instant>,
    usage_cache_invalidation_tasks: HashMap<u64, UsageCacheInvalidationTask>,
    usage_cache_invalidation_next_id: u64,
    pub pending_reset_cards: tokio::sync::mpsc::Receiver<(
        String,
        StrictAccountBinding,
        Result<ConsumedResetCredit, ResetCardFailure>,
    )>,
    pub reset_card_sender: tokio::sync::mpsc::Sender<(
        String,
        StrictAccountBinding,
        Result<ConsumedResetCredit, ResetCardFailure>,
    )>,
    pending_profile_switches: tokio::sync::mpsc::Receiver<(String, super::switch::TaskResult)>,
    profile_switch_sender: tokio::sync::mpsc::Sender<(String, super::switch::TaskResult)>,
    pub reset_cards_in_flight: BTreeSet<String>,
    /// Tracks each warmup until its task has observably completed. The slow-task
    /// notice never cancels work because a refresh-token rotation or submitted
    /// quota request must be allowed to reach its persistence boundary.
    pub warmup_tasks: HashMap<u64, WarmupTask>,
    pub warmup_next_id: u64,
    /// One all-or-nothing warmup eligibility pass. Disk cache reads run on the
    /// blocking pool, and no warmup credential task starts until this handle
    /// has returned a decision for the complete candidate set.
    warmup_preflight: Option<WarmupPreflightTask>,
    /// Join handles for account-scoped network work whose cancellation could
    /// strand a rotated credential or leave a reset-card outcome unknown.
    account_tasks: HashMap<u64, AccountTask>,
    account_task_next_id: u64,
    /// Guarded live-credential reconciliation begins only after the first TUI
    /// frame. It is tracked to completion because it may write a newer live
    /// credential into its strictly matched saved profile.
    startup_auth_reconciliation: Option<tokio::task::JoinHandle<Result<TuiAuthReconciliation>>>,
    /// The first registry snapshot is loaded only after the terminal has
    /// painted. A slow synced filesystem can then delay account rows without
    /// delaying the application window itself or blocking input processing.
    startup_profile_task: Option<StartupProfileTask>,
    startup_cache_task: Option<StartupCacheTask>,
    startup_http_client_task: Option<StartupHttpClientTask>,
    startup_self_update_cleanup_task: Option<StartupSelfUpdateCleanupTask>,
    startup_file_log_init_task: Option<StartupFileLogInitTask>,
    /// Retained after post-frame arming so a late first-write initialization
    /// failure can be surfaced once through the normal TUI warning path.
    /// This observer is not startup work and never delays shutdown.
    file_log_writer: Option<crate::logging::FileLogWriter>,
    pending_startup_maintenance_warnings: Vec<String>,
    startup_exit_warnings: Arc<std::sync::Mutex<Vec<String>>>,
    profile_mutation_task: Option<ProfileMutationTask>,
    startup_auth_state: StartupAuthState,
    shutting_down: bool,
    pub confirm: Option<ConfirmAction>,
    pub rename: Option<RenameState>,
    pub usage_limiter: Arc<Semaphore>,
    http_client: Option<reqwest::Client>,
    pub update_available: Option<String>,
    pub update_rx: Option<tokio::sync::oneshot::Receiver<String>>,
    pub auto_refresh_enabled: bool,
    pub auto_refresh_interval: Duration,
    pub next_auto_refresh: Option<Instant>,
    pub auto_warmup_enabled: bool,
    pub detail_visible: bool,
    pub help_popup: Option<super::popup::PopupState>,
    pub menu: Option<super::menu::MenuState>,
    /// Session-level per-alias model list cache (no TTL). Populated lazily
    /// for the selected account or when its account details are opened.
    pub model_cache: HashMap<String, ModelStatus>,
    pub pending_models: tokio::sync::mpsc::Receiver<ModelTaskResult>,
    pub model_sender: tokio::sync::mpsc::Sender<ModelTaskResult>,
    pub model_requests: HashMap<String, u64>,
    pub model_next_id: u64,
    auth_expiry_tasks: HashMap<u64, AuthExpiryTask>,
    auth_expiry_latest: HashMap<String, u64>,
    auth_expiry_next_id: u64,
    /// Monotonic generation of state that can change the rendered frame.
    /// Ordinary background-task presence is intentionally excluded because it
    /// has no spinner. Profile-switch phases are user-visible status content and
    /// explicitly advance this revision when their tracked task changes.
    render_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAuthState {
    Ready,
    Reconciling,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupAuthPoll {
    Pending,
    Ready,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialOperationBlocker {
    StartupNetworkInitialization,
    StartupFailure,
    Transition(CredentialTransitionBlocker),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialTransitionBlocker {
    StartupReconciliation,
    ProfileSwitch,
    ProfileMutation,
}

impl App {
    #[cfg(test)]
    pub fn new() -> Self {
        crate::config::init_defaults_for_tests();
        Self::with_http_client(Some(
            crate::auth::build_http_client().expect("test HTTP client must be configurable"),
        ))
    }

    #[cfg(test)]
    fn with_http_client(http_client: Option<reqwest::Client>) -> Self {
        Self::with_http_client_and_warning_sink(
            http_client,
            Arc::new(std::sync::Mutex::new(Vec::new())),
        )
    }

    fn with_http_client_and_warning_sink(
        http_client: Option<reqwest::Client>,
        startup_exit_warnings: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let (usage_enrichment_tx, usage_enrichment_rx) = tokio::sync::mpsc::channel(128);
        let (usage_lease_release_tx, usage_lease_release_rx) = tokio::sync::mpsc::channel(128);
        let (workspace_tx, workspace_rx) = tokio::sync::mpsc::channel(128);
        let (reset_card_tx, reset_card_rx) = tokio::sync::mpsc::channel(16);
        let (profile_switch_tx, profile_switch_rx) = tokio::sync::mpsc::channel(16);
        let (model_tx, model_rx) = tokio::sync::mpsc::channel(32);
        let cfg = crate::config::get();
        App {
            accounts: vec![],
            selected: 0,
            search: None,
            search_active: false,
            sort_mode: SortMode::Name,
            view_indices: vec![],
            marked: BTreeSet::new(),
            status_msg: None,
            status_is_error: false,
            status_expiry: None,
            refreshing_requests: HashMap::new(),
            pending_usage_refreshes: HashMap::new(),
            deferred_post_switch_usage_refreshes: BTreeMap::new(),
            usage_next_id: 0,
            pending_results: rx,
            result_sender: tx,
            pending_usage_enrichment: usage_enrichment_rx,
            usage_enrichment_sender: usage_enrichment_tx,
            pending_usage_lease_releases: usage_lease_release_rx,
            usage_lease_release_sender: usage_lease_release_tx,
            usage_generations: HashMap::new(),
            usage_lease_release_generations: HashMap::new(),
            usage_metadata_requests: HashMap::new(),
            pending_workspace: workspace_rx,
            workspace_sender: workspace_tx,
            workspace_states: HashMap::new(),
            workspace_requests: HashMap::new(),
            workspace_next_id: 0,
            workspace_lookup_tasks: HashMap::new(),
            workspace_cache_writes: HashMap::new(),
            workspace_cache_latest: HashMap::new(),
            workspace_cache_write_next_id: 0,
            workspace_next_expiry: None,
            usage_cache_invalidation_tasks: HashMap::new(),
            usage_cache_invalidation_next_id: 0,
            pending_reset_cards: reset_card_rx,
            reset_card_sender: reset_card_tx,
            pending_profile_switches: profile_switch_rx,
            profile_switch_sender: profile_switch_tx,
            reset_cards_in_flight: BTreeSet::new(),
            warmup_tasks: HashMap::new(),
            warmup_next_id: 0,
            warmup_preflight: None,
            account_tasks: HashMap::new(),
            account_task_next_id: 0,
            startup_auth_reconciliation: None,
            startup_profile_task: None,
            startup_cache_task: None,
            startup_http_client_task: None,
            startup_self_update_cleanup_task: None,
            startup_file_log_init_task: None,
            file_log_writer: None,
            pending_startup_maintenance_warnings: Vec::new(),
            startup_exit_warnings,
            profile_mutation_task: None,
            startup_auth_state: StartupAuthState::Ready,
            shutting_down: false,
            confirm: None,
            rename: None,
            usage_limiter: Arc::new(Semaphore::new(cfg.network.max_concurrent)),
            http_client,
            update_available: None,
            update_rx: None,
            auto_refresh_enabled: false,
            auto_refresh_interval: Duration::from_secs(cfg.tui.auto_refresh_interval_secs),
            next_auto_refresh: None,
            auto_warmup_enabled: false,
            detail_visible: true,
            help_popup: None,
            menu: None,
            model_cache: HashMap::new(),
            pending_models: model_rx,
            model_sender: model_tx,
            model_requests: HashMap::new(),
            model_next_id: 0,
            auth_expiry_tasks: HashMap::new(),
            auth_expiry_latest: HashMap::new(),
            auth_expiry_next_id: 0,
            render_revision: 0,
        }
    }

    fn track_account_task(
        &mut self,
        alias: String,
        kind: AccountTaskKind,
        lease_control: profile::ProfileLeaseAcquireControl,
        handle: tokio::task::JoinHandle<()>,
    ) {
        self.track_account_task_with_controls(
            alias,
            kind,
            lease_control,
            AccountTaskControls::default(),
            handle,
        );
    }

    fn track_account_task_with_controls(
        &mut self,
        alias: String,
        kind: AccountTaskKind,
        lease_control: profile::ProfileLeaseAcquireControl,
        controls: AccountTaskControls,
        handle: tokio::task::JoinHandle<()>,
    ) {
        let task_id = self.account_task_next_id;
        self.account_task_next_id = self.account_task_next_id.wrapping_add(1);
        self.account_tasks.insert(
            task_id,
            AccountTask {
                alias,
                kind,
                lease_control,
                followup_controls: controls.followup_controls,
                network_wait: controls.network_wait,
                read_only_work: controls.read_only_work,
                usage_work: controls.usage_work,
                handle,
            },
        );
        if kind.is_profile_switch() {
            self.mark_render_changed();
        }
    }

    fn account_operation_in_flight(&self, alias: &str) -> bool {
        self.account_tasks.values().any(|task| task.alias == alias)
            || self.is_warmup_in_flight(alias)
            || self.refreshing_requests.contains_key(alias)
            || self.model_requests.contains_key(alias)
            || self.reset_cards_in_flight.contains(alias)
    }

    fn reset_card_in_flight(&self, alias: &str) -> bool {
        self.account_tasks
            .values()
            .any(|task| task.alias == alias && matches!(task.kind, AccountTaskKind::ResetCard))
    }

    fn cancel_waiting_background_credential_work_for(&self, alias: &str) -> Option<Refresh> {
        let active_usage = self.refreshing_requests.get(alias).copied();
        let mut cancelled_active_usage = false;
        for task in self.account_tasks.values().filter(|task| {
            task.alias == alias
                && matches!(
                    task.kind,
                    AccountTaskKind::Usage { .. } | AccountTaskKind::Model { .. }
                )
        }) {
            let cancelled_before_lease = task.lease_control.cancel_waiting();
            let cancelled_during_network_wait = task
                .network_wait
                .as_ref()
                .is_some_and(SafeTaskCancellation::request);
            let cancelled_during_usage_read = task
                .usage_work
                .as_ref()
                .is_some_and(crate::usage::UsageTaskCancellation::request);
            for control in &task.followup_controls {
                control.cancel_waiting();
            }
            if let Some(control) = &task.read_only_work {
                let _ = control.request();
            }
            if (cancelled_before_lease
                || cancelled_during_network_wait
                || cancelled_during_usage_read)
                && let AccountTaskKind::Usage { request_id } = task.kind
                && active_usage.is_some_and(|(active_id, _)| active_id == request_id)
            {
                cancelled_active_usage = true;
            }
        }
        for task in self
            .warmup_tasks
            .values()
            .filter(|task| task.alias == alias)
        {
            task.lease_control.cancel_waiting();
            let _ = task.network_wait.request();
            let _ = task.model_discovery.request();
        }
        cancelled_active_usage.then(|| active_usage.expect("cancelled active usage exists").1)
    }

    fn cancel_usage_followups_for(&self, alias: &str) {
        for task in self.account_tasks.values().filter(|task| {
            task.alias == alias && matches!(task.kind, AccountTaskKind::Usage { .. })
        }) {
            for control in &task.followup_controls {
                control.cancel_waiting();
            }
        }
    }

    fn defer_post_switch_usage_refresh(&mut self, alias: String, plan: AccountRefreshPlan) {
        self.deferred_post_switch_usage_refreshes
            .entry(alias)
            .and_modify(|queued| {
                *queued = (*queued).merged_with(plan);
            })
            .or_insert(plan);
    }

    fn background_credential_cancellations(
        &self,
    ) -> Vec<(
        String,
        profile::ProfileLeaseAcquireControl,
        Vec<CredentialTaskCancellation>,
    )> {
        let mut controls: Vec<_> = self
            .account_tasks
            .values()
            .filter(|task| {
                matches!(
                    task.kind,
                    AccountTaskKind::Usage { .. } | AccountTaskKind::Model { .. }
                )
            })
            .map(|task| {
                let mut cancellations: Vec<CredentialTaskCancellation> = task
                    .network_wait
                    .iter()
                    .chain(task.read_only_work.iter())
                    .cloned()
                    .map(CredentialTaskCancellation::Safe)
                    .collect();
                cancellations.extend(
                    task.usage_work
                        .iter()
                        .cloned()
                        .map(CredentialTaskCancellation::Usage),
                );
                (
                    task.alias.clone(),
                    task.lease_control.clone(),
                    cancellations,
                )
            })
            .collect();
        controls.extend(self.warmup_tasks.values().map(|task| {
            (
                task.alias.clone(),
                task.lease_control.clone(),
                vec![
                    CredentialTaskCancellation::Safe(task.network_wait.clone()),
                    CredentialTaskCancellation::Safe(task.model_discovery.clone()),
                ],
            )
        }));
        controls
    }

    fn profile_switch_in_flight(&self) -> bool {
        self.account_tasks
            .values()
            .any(|task| task.kind.is_profile_switch())
    }

    /// Return the newest tracked profile-switch phase for the status bar.
    ///
    /// A completed phase can coexist briefly with the next phase between its
    /// result-channel send and JoinHandle cleanup. The task id is monotonic, so
    /// the newest id is the authoritative phase without duplicating switch
    /// state in a separate UI flag.
    pub(super) fn profile_switch_progress(&self) -> Option<String> {
        let (_, task) = self
            .account_tasks
            .iter()
            .filter_map(|(&task_id, task)| task.kind.is_profile_switch().then_some((task_id, task)))
            .max_by_key(|(task_id, _)| *task_id)?;
        task.kind.profile_switch_progress(&task.alias)
    }

    #[cfg(test)]
    pub(super) fn track_pending_profile_switch_for_render_test(&mut self, alias: &str) {
        let handle = tokio::spawn(std::future::pending());
        self.track_account_task(
            alias.to_string(),
            AccountTaskKind::SwitchPrepare,
            profile::ProfileLeaseAcquireControl::new(),
            handle,
        );
    }

    fn profile_mutation_in_flight(&self) -> bool {
        self.profile_mutation_task.is_some()
    }

    fn interactive_operation_in_flight(&self) -> bool {
        self.confirm.is_some()
            || self.rename.is_some()
            || self.profile_switch_in_flight()
            || self.profile_mutation_in_flight()
    }

    fn credential_transition_blocker(&self) -> Option<CredentialTransitionBlocker> {
        match self.startup_auth_state {
            StartupAuthState::Reconciling => {
                return Some(CredentialTransitionBlocker::StartupReconciliation);
            }
            StartupAuthState::Ready | StartupAuthState::Blocked => {}
        }
        if self.profile_switch_in_flight() {
            return Some(CredentialTransitionBlocker::ProfileSwitch);
        }
        self.profile_mutation_in_flight()
            .then_some(CredentialTransitionBlocker::ProfileMutation)
    }

    fn credential_operation_blocker(&self) -> Option<CredentialOperationBlocker> {
        if self.startup_auth_state == StartupAuthState::Blocked {
            return Some(CredentialOperationBlocker::StartupFailure);
        }
        if self.http_client.is_none() {
            return Some(CredentialOperationBlocker::StartupNetworkInitialization);
        }
        self.credential_transition_blocker()
            .map(CredentialOperationBlocker::Transition)
    }

    fn credential_operations_ready(&self) -> bool {
        self.credential_operation_blocker().is_none()
    }

    fn request_client(&mut self) -> Option<reqwest::Client> {
        match self.http_client.clone() {
            Some(client) => Some(client),
            None => {
                self.set_status_error("HTTP client is not initialized".to_string(), 5);
                None
            }
        }
    }

    fn reject_new_credential_operation(&mut self) -> bool {
        let Some(blocker) = self.credential_operation_blocker() else {
            return false;
        };
        let (message, is_error) = match blocker {
            CredentialOperationBlocker::StartupNetworkInitialization => (
                "Preparing the shared network client before starting account operations",
                false,
            ),
            CredentialOperationBlocker::StartupFailure => (
                "Account operations are blocked because live credential synchronization failed",
                true,
            ),
            CredentialOperationBlocker::Transition(
                CredentialTransitionBlocker::StartupReconciliation,
            ) => (
                "Finishing live credential synchronization before starting an account operation",
                false,
            ),
            CredentialOperationBlocker::Transition(CredentialTransitionBlocker::ProfileSwitch) => (
                "Finish the active profile switch before starting another account operation",
                false,
            ),
            CredentialOperationBlocker::Transition(
                CredentialTransitionBlocker::ProfileMutation,
            ) => (
                "Finish the active profile change before starting another account operation",
                false,
            ),
        };
        if is_error {
            self.set_status_error(message.to_string(), 5);
        } else {
            self.set_status(message.to_string(), 5);
        }
        true
    }

    fn reject_credential_recovery_during_transition(&mut self) -> bool {
        let Some(blocker) = self.credential_transition_blocker() else {
            return false;
        };
        let message = match blocker {
            CredentialTransitionBlocker::StartupReconciliation => {
                "Finishing live credential synchronization before changing accounts"
            }
            CredentialTransitionBlocker::ProfileSwitch => {
                "Finish the active profile switch before changing accounts again"
            }
            CredentialTransitionBlocker::ProfileMutation => {
                "Finish the active profile change before changing accounts"
            }
        };
        self.set_status(message.to_string(), 5);
        true
    }

    pub fn has_pending_credential_tasks(&self) -> bool {
        self.startup_auth_reconciliation.is_some()
            || self.startup_profile_task.is_some()
            || self.startup_cache_task.is_some()
            || self.startup_http_client_task.is_some()
            || self.profile_mutation_task.is_some()
            || !self.account_tasks.is_empty()
            || !self.warmup_tasks.is_empty()
            || self.warmup_preflight.is_some()
            || !self.workspace_cache_writes.is_empty()
            || !self.usage_cache_invalidation_tasks.is_empty()
    }

    fn render_revision(&self) -> u64 {
        self.render_revision
    }

    fn mark_render_changed(&mut self) {
        self.render_revision = self.render_revision.wrapping_add(1);
    }

    fn start_startup_cache_read(
        &mut self,
        bindings: HashMap<String, StrictAccountBinding>,
        account_ids: Vec<String>,
    ) {
        if self.startup_cache_task.is_some() || bindings.is_empty() {
            return;
        }
        let control = cache::CacheLockAcquireControl::new();
        let worker_control = control.clone();
        let handle = tokio::spawn(async move {
            cache::get_snapshot_bound_async_cancellable(&bindings, &account_ids, &worker_control)
                .await
        });
        self.startup_cache_task = Some(StartupCacheTask { control, handle });
    }

    fn start_startup_profile_load(&mut self) {
        if self.startup_profile_task.is_some() {
            return;
        }
        self.startup_auth_state = StartupAuthState::Reconciling;
        self.set_status("Loading saved accounts...".to_string(), 60);
        let handle = tokio::task::spawn_blocking(|| load_profile_reload_snapshot(false));
        self.startup_profile_task = Some(StartupProfileTask { handle });
    }

    fn start_startup_http_client(&mut self) {
        if self.startup_http_client_task.is_some() || self.http_client.is_some() {
            return;
        }
        let handle = tokio::task::spawn_blocking(crate::auth::build_http_client);
        self.startup_http_client_task = Some(StartupHttpClientTask { handle });
    }

    fn start_post_draw_startup_maintenance(
        &mut self,
        file_log_writer: crate::logging::FileLogWriter,
    ) {
        debug_assert!(self.startup_self_update_cleanup_task.is_none());
        debug_assert!(self.startup_file_log_init_task.is_none());
        debug_assert!(self.file_log_writer.is_none());
        self.startup_self_update_cleanup_task = Some(StartupSelfUpdateCleanupTask {
            handle: tokio::task::spawn_blocking(
                crate::update::recover_pending_self_update_cleanup_on_startup,
            ),
        });
        self.file_log_writer = Some(file_log_writer.clone());
        self.startup_file_log_init_task = Some(StartupFileLogInitTask {
            handle: tokio::task::spawn_blocking(move || {
                file_log_writer.finish_deferred_initialization()
            }),
        });
    }

    fn has_pending_startup_maintenance(&self) -> bool {
        self.startup_self_update_cleanup_task.is_some()
            || self.startup_file_log_init_task.is_some()
            || !self.pending_startup_maintenance_warnings.is_empty()
    }

    async fn finish_startup_self_update_cleanup(&mut self, wait: bool) -> Option<String> {
        if !wait
            && !self
                .startup_self_update_cleanup_task
                .as_ref()
                .is_some_and(|task| task.handle.is_finished())
        {
            return None;
        }
        let task = self.startup_self_update_cleanup_task.take()?;
        match task.handle.await {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => Some(crate::app::format_pending_self_update_cleanup_warning(
                &error,
            )),
            Err(error) => {
                let error = anyhow::anyhow!(
                    "startup cleanup task failed: {}",
                    crate::task_batch::join_failure_detail(&error)
                );
                Some(crate::app::format_pending_self_update_cleanup_warning(
                    &error,
                ))
            }
        }
    }

    async fn finish_startup_file_log_init(&mut self, wait: bool) -> Option<String> {
        let task_ready = self
            .startup_file_log_init_task
            .as_ref()
            .is_some_and(|task| task.handle.is_finished());
        if (wait || task_ready)
            && let Some(task) = self.startup_file_log_init_task.take()
        {
            let failure = match task.handle.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(error) => Some(format!(
                    "initialization task failed: {}",
                    crate::task_batch::join_failure_detail(&error)
                )),
            };
            if let Some(failure) = failure
                && let Some(writer) = self.file_log_writer.as_ref()
            {
                writer.disable_initialization_after_task_failure(failure);
            }
        }

        let writer = self.file_log_writer.as_ref()?;
        let error = match writer.take_initialization_error() {
            Ok(Some(error)) => error,
            Ok(None) => return None,
            Err(error) => format!("could not inspect file-log initialization: {error:#}"),
        };
        self.file_log_writer = None;
        Some(format!("Warning: file logging is unavailable: {error}"))
    }

    fn queue_startup_maintenance_warnings(&mut self, warnings: Vec<String>) {
        if warnings.is_empty() {
            return;
        }
        let warning =
            safe_text::bounded_terminal_text(&warnings.join("; "), STATUS_MESSAGE_MAX_CHARS);
        self.pending_startup_maintenance_warnings.push(warning);
    }

    /// Startup profile/auth/cache progress owns the ordinary status slot while
    /// it is changing. Publish maintenance warnings at the settled boundary so
    /// they cannot be replaced by the next transient startup message.
    fn present_startup_maintenance_warnings(&mut self) {
        if self.pending_startup_maintenance_warnings.is_empty() {
            return;
        }
        let warnings = std::mem::take(&mut self.pending_startup_maintenance_warnings);
        self.set_status_error(warnings.join("; "), 8);
    }

    async fn poll_startup_maintenance(&mut self) {
        let mut warnings = Vec::new();
        if let Some(warning) = self.finish_startup_self_update_cleanup(false).await {
            warnings.push(warning);
        }
        if let Some(warning) = self.finish_startup_file_log_init(false).await {
            warnings.push(warning);
        }
        self.queue_startup_maintenance_warnings(warnings);
    }

    async fn drain_startup_maintenance(&mut self) {
        let mut warnings = Vec::new();
        if let Some(warning) = self.finish_startup_self_update_cleanup(true).await {
            warnings.push(warning);
        }
        if let Some(warning) = self.finish_startup_file_log_init(true).await {
            warnings.push(warning);
        }
        self.queue_startup_maintenance_warnings(warnings);
        if !self.pending_startup_maintenance_warnings.is_empty() {
            let warnings = std::mem::take(&mut self.pending_startup_maintenance_warnings);
            let warning =
                safe_text::bounded_terminal_text(&warnings.join("; "), STATUS_MESSAGE_MAX_CHARS);
            self.startup_exit_warnings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(warning.clone());
            self.set_status_error(warning, 8);
        }
    }

    async fn poll_startup_http_client(&mut self) -> Option<Result<()>> {
        if !self
            .startup_http_client_task
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            return None;
        }
        let task = self
            .startup_http_client_task
            .take()
            .expect("finished startup HTTP-client task must remain tracked");
        Some(match task.handle.await {
            Ok(Ok(client)) => {
                self.http_client = Some(client);
                Ok(())
            }
            Ok(Err(error)) => Err(error).context("building the shared TUI HTTP client"),
            Err(error) => Err(anyhow::anyhow!(
                "startup HTTP-client task failed: {}",
                crate::task_batch::join_failure_detail(&error)
            )),
        })
    }

    async fn poll_startup_profile_result(&mut self) -> Option<Result<ProfileReloadSnapshot>> {
        if !self
            .startup_profile_task
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            return None;
        }
        let task = self
            .startup_profile_task
            .take()
            .expect("finished startup profile task must remain tracked");
        Some(match task.handle.await {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!(
                "startup profile task failed: {}",
                crate::task_batch::join_failure_detail(&error)
            )),
        })
    }

    async fn poll_startup_cache_result(&mut self) -> Option<Result<Option<cache::CacheSnapshot>>> {
        if !self
            .startup_cache_task
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            return None;
        }
        let task = self
            .startup_cache_task
            .take()
            .expect("finished startup cache task must remain tracked");
        Some(match task.handle.await {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!(
                "startup cache task failed: {}",
                crate::task_batch::join_failure_detail(&error)
            )),
        })
    }

    fn start_startup_auth_reconciliation(&mut self) {
        if self.startup_auth_reconciliation.is_some() {
            return;
        }
        self.startup_auth_state = StartupAuthState::Reconciling;
        self.set_status("Synchronizing live Codex credentials...".to_string(), 60);
        self.startup_auth_reconciliation = Some(tokio::task::spawn_blocking(
            profile::reconcile_live_auth_for_tui,
        ));
    }

    fn apply_auth_reconciliation(
        &mut self,
        reconciliation: TuiAuthReconciliation,
        invalidated_aliases: &BTreeSet<String>,
    ) -> Result<String> {
        match reconciliation {
            TuiAuthReconciliation::NoChange
                if invalidated_aliases.iter().any(|alias| {
                    self.accounts
                        .iter()
                        .all(|account| account.alias != alias.as_str())
                }) =>
            {
                self.try_load_profiles_from_marker_preserving_selection(invalidated_aliases)?;
                Ok("Accounts ready".to_string())
            }
            TuiAuthReconciliation::NoChange if invalidated_aliases.is_empty() => {
                let current =
                    read_current_checked().context("reading the active profile marker")?;
                self.apply_displayed_current(current.as_deref());
                Ok("Accounts ready".to_string())
            }
            TuiAuthReconciliation::NoChange => {
                let current =
                    read_current_checked().context("reading the active profile marker")?;
                let loaded = self
                    .accounts
                    .iter()
                    .map(|account| (account.alias.clone(), account.info.clone()))
                    .collect();
                self.apply_profile_snapshot_preserving_selection(
                    current,
                    loaded,
                    invalidated_aliases,
                );
                Ok("Accounts ready".to_string())
            }
            TuiAuthReconciliation::ProfileUpdated { alias, info } => {
                let mut invalidated_aliases = invalidated_aliases.clone();
                invalidated_aliases.insert(alias.clone());
                self.invalidate_models_after_credential_reload(&invalidated_aliases);
                if invalidated_aliases.iter().any(|invalidated| {
                    self.accounts
                        .iter()
                        .all(|account| account.alias != invalidated.as_str())
                }) {
                    self.try_load_profiles_from_marker_preserving_selection(&invalidated_aliases)?;
                } else {
                    let loaded = self
                        .accounts
                        .iter()
                        .map(|account| {
                            let account_info = if account.alias == alias {
                                info.clone()
                            } else {
                                account.info.clone()
                            };
                            (account.alias.clone(), account_info)
                        })
                        .collect();
                    self.apply_profile_snapshot_preserving_selection(
                        Some(alias.clone()),
                        loaded,
                        &invalidated_aliases,
                    );
                }
                Ok(format!("Updated live credentials for {alias}"))
            }
            TuiAuthReconciliation::NoLiveAuth => {
                self.apply_displayed_current(None);
                Ok("No live Codex account is signed in".to_string())
            }
            TuiAuthReconciliation::UntrackedAccount => {
                self.apply_displayed_current(None);
                Ok("The live Codex account is not saved; press a to add it".to_string())
            }
            TuiAuthReconciliation::UnidentifiedAccount => {
                self.apply_displayed_current(None);
                Ok("The live Codex account identity could not be verified".to_string())
            }
            TuiAuthReconciliation::UnresolvedIdentity { aliases } => {
                self.apply_displayed_current(None);
                Ok(format!(
                    "Live Codex account identity is incomplete; matching profiles: {}",
                    aliases.join(", ")
                ))
            }
        }
    }

    async fn poll_startup_auth_reconciliation(&mut self) -> StartupAuthPoll {
        let Some(task) = self.startup_auth_reconciliation.as_ref() else {
            return match self.startup_auth_state {
                StartupAuthState::Ready => StartupAuthPoll::Ready,
                StartupAuthState::Reconciling => StartupAuthPoll::Pending,
                StartupAuthState::Blocked => StartupAuthPoll::Blocked,
            };
        };
        if !task.is_finished() {
            return StartupAuthPoll::Pending;
        }
        let task = self
            .startup_auth_reconciliation
            .take()
            .expect("finished startup reconciliation must remain tracked");
        let result = match task.await {
            Ok(Ok(reconciliation)) => {
                self.apply_auth_reconciliation(reconciliation, &BTreeSet::new())
            }
            Ok(Err(error)) => Err(error.context("synchronizing live credentials")),
            Err(error) => Err(anyhow::anyhow!(
                "live credential synchronization task failed: {}",
                crate::task_batch::join_failure_detail(&error)
            )),
        };
        match result {
            Ok(message) => {
                self.startup_auth_state = StartupAuthState::Ready;
                self.set_status(message, 8);
                StartupAuthPoll::Ready
            }
            Err(error) => {
                self.startup_auth_state = StartupAuthState::Blocked;
                self.set_status_error(
                    format!("Live credential synchronization failed: {error:#}"),
                    8,
                );
                StartupAuthPoll::Blocked
            }
        }
    }

    /// Observe completed account tasks so panics cannot leave their state
    /// permanently in flight. Successful tasks deliver their typed result over
    /// the existing result channels before their JoinHandle becomes finished.
    pub async fn poll_account_tasks(&mut self) {
        let mut finished: Vec<u64> = self
            .account_tasks
            .iter()
            .filter_map(|(task_id, task)| task.handle.is_finished().then_some(*task_id))
            .collect();
        finished.sort_unstable();
        let mut failures = Vec::new();

        for task_id in finished {
            let Some(task) = self.account_tasks.remove(&task_id) else {
                continue;
            };
            let alias = task.alias;
            let kind = task.kind;
            if kind.is_profile_switch() {
                self.mark_render_changed();
            }
            let cancelled_before_lease = task.lease_control.is_cancelled();
            let cancelled_at_safe_boundary = task
                .network_wait
                .as_ref()
                .is_some_and(SafeTaskCancellation::completed)
                || task
                    .read_only_work
                    .as_ref()
                    .is_some_and(SafeTaskCancellation::completed)
                || task
                    .usage_work
                    .as_ref()
                    .is_some_and(crate::usage::UsageTaskCancellation::cancellation_completed);
            let joined = task.handle.await;
            if (cancelled_before_lease || cancelled_at_safe_boundary) && joined.is_ok() {
                match kind {
                    AccountTaskKind::Usage { request_id } => {
                        // Task completion proves the lease is gone even when a
                        // newer UI event already consumed or replaced the core
                        // result. The helper records only the latest generation.
                        self.record_usage_lease_release(&alias, request_id);
                        let is_current = self
                            .refreshing_requests
                            .get(&alias)
                            .is_some_and(|(active_id, _)| *active_id == request_id);
                        if is_current {
                            // A successfully joined cancellation at either safe
                            // boundary has no required worker result. Task
                            // completion proves that no profile lease remains.
                            let resume_after_switch = self.profile_switch_in_flight();
                            let cancelled_refresh = self
                                .refreshing_requests
                                .remove(&alias)
                                .map(|(_, refresh)| refresh);
                            self.mark_render_changed();
                            if self
                                .usage_metadata_requests
                                .get(&alias)
                                .is_some_and(|active_id| *active_id == request_id)
                            {
                                self.usage_metadata_requests.remove(&alias);
                            }
                            self.pending_usage_refreshes.remove(&alias);
                            if resume_after_switch && let Some(refresh) = cancelled_refresh {
                                self.defer_post_switch_usage_refresh(
                                    alias.clone(),
                                    AccountRefreshPlan::resume_cancelled_usage(refresh),
                                );
                            }
                            if let Some(entry) =
                                self.accounts.iter_mut().find(|entry| entry.alias == alias)
                                && matches!(entry.usage, UsageStatus::Loading)
                            {
                                entry.usage = UsageStatus::Idle;
                                self.mark_render_changed();
                                self.update_view();
                            }
                        }
                    }
                    AccountTaskKind::Model { request_id } => {
                        let is_current = self
                            .model_requests
                            .get(&alias)
                            .is_some_and(|active_id| *active_id == request_id);
                        if is_current {
                            self.model_requests.remove(&alias);
                            if matches!(self.model_cache.get(&alias), Some(ModelStatus::Loading)) {
                                self.model_cache.remove(&alias);
                                self.mark_render_changed();
                            }
                        }
                    }
                    AccountTaskKind::ResetCard => {
                        if self.reset_cards_in_flight.remove(&alias) {
                            self.mark_render_changed();
                        }
                    }
                    AccountTaskKind::SwitchPrepare
                    | AccountTaskKind::SwitchSync
                    | AccountTaskKind::SwitchCommit => {}
                }
                continue;
            }
            let Err(error) = joined else { continue };
            let detail = crate::task_batch::join_failure_detail(&error);
            match kind {
                AccountTaskKind::Usage { request_id } => {
                    // Unwinding has dropped any acquired lease. Record that
                    // boundary independently of whether the core result was
                    // already removed from `refreshing_requests`.
                    self.record_usage_lease_release(&alias, request_id);
                    let is_current = self
                        .refreshing_requests
                        .get(&alias)
                        .is_some_and(|(active_id, _)| *active_id == request_id);
                    if is_current {
                        // The JoinHandle has finished, so unwinding has dropped
                        // any acquired lease even though no boundary message was
                        // available from the stopped worker.
                        self.refreshing_requests.remove(&alias);
                        if self
                            .usage_metadata_requests
                            .get(&alias)
                            .is_some_and(|active_id| *active_id == request_id)
                        {
                            self.usage_metadata_requests.remove(&alias);
                        }
                        self.pending_usage_refreshes.remove(&alias);
                        if let Some(entry) =
                            self.accounts.iter_mut().find(|entry| entry.alias == alias)
                        {
                            entry.usage = UsageStatus::Error(UsageError {
                                summary: "usage task stopped".to_string(),
                                detail: format!("usage task stopped ({alias}): {detail}"),
                            });
                        }
                        self.mark_render_changed();
                        self.update_view();
                    }
                }
                AccountTaskKind::Model { request_id } => {
                    let is_current = self
                        .model_requests
                        .get(&alias)
                        .is_some_and(|active_id| *active_id == request_id);
                    if is_current {
                        self.model_requests.remove(&alias);
                        self.model_cache.insert(
                            alias.clone(),
                            ModelStatus::Error(format!("model task stopped: {detail}")),
                        );
                        self.mark_render_changed();
                    }
                }
                AccountTaskKind::ResetCard => {
                    let mut unknown = format!(
                        "reset-card consumption may have occurred because its worker stopped ({detail}); verify before retry"
                    );
                    let binding = self
                        .accounts
                        .iter()
                        .find(|entry| entry.alias == alias)
                        .and_then(|entry| strict_account_identity(&entry.info));
                    if let Some(binding) = binding {
                        self.start_usage_cache_invalidation(
                            alias.clone(),
                            binding,
                            None,
                            Some(unknown.clone()),
                        );
                    } else {
                        self.reset_cards_in_flight.remove(&alias);
                        unknown.push_str(
                            "; the account identity is no longer available, so its usage cache could not be safely invalidated; do not retry until usage is refreshed and card ownership is verified",
                        );
                    }
                    if let Some(entry) = self.accounts.iter_mut().find(|entry| entry.alias == alias)
                    {
                        entry.usage = UsageStatus::Error(UsageError {
                            summary: "reset-card outcome unknown".to_string(),
                            detail: unknown.clone(),
                        });
                        self.mark_render_changed();
                        self.update_view();
                    }
                    failures.push((alias, unknown));
                    continue;
                }
                AccountTaskKind::SwitchPrepare | AccountTaskKind::SwitchSync => {}
                AccountTaskKind::SwitchCommit => {
                    let switch_error = anyhow::anyhow!(
                        "profile switch task stopped before reporting its outcome: {detail}"
                    );
                    if let Err(reconcile_error) =
                        self.reconcile_displayed_current_after_switch_error(&switch_error)
                    {
                        for account in &mut self.accounts {
                            account.is_current = false;
                        }
                        failures.push((
                            alias.clone(),
                            format!(
                                "{detail}; active account could not be verified: {reconcile_error:#}"
                            ),
                        ));
                        continue;
                    }
                }
            }
            failures.push((alias, detail));
        }

        if !failures.is_empty() {
            failures.sort_by(|(a_alias, a_detail), (b_alias, b_detail)| {
                a_alias.cmp(b_alias).then_with(|| a_detail.cmp(b_detail))
            });
            let detail = failures
                .iter()
                .map(|(alias, detail)| format!("[{alias}] {detail}"))
                .collect::<Vec<_>>()
                .join("; ");
            let subject = if failures.len() == 1 {
                "Account task stopped"
            } else {
                "Account tasks stopped"
            };
            self.set_status_error(format!("{subject}: {detail}"), 6);
        }
    }

    /// Finish every account operation already started before allowing the Tokio
    /// runtime to shut down. No additional timeout is layered here: each HTTP
    /// operation keeps the configured client timeout, and abandoning a refresh
    /// request after the server received it can lose its single-use token.
    pub async fn drain_credential_tasks(&mut self) {
        self.shutting_down = true;
        // Pre-lease work and read-only usage network phases can stop. A usage
        // task that crossed its token-refresh boundary refuses cancellation so
        // the response and durable persistence still drain normally; model
        // discovery retains its separate first-request and later-GET bounds.
        for task in self.account_tasks.values() {
            task.lease_control.cancel_waiting();
            for control in &task.followup_controls {
                control.cancel_waiting();
            }
            if let Some(control) = &task.network_wait {
                let _ = control.request();
            }
            if let Some(control) = &task.read_only_work {
                let _ = control.request();
            }
            if let Some(control) = &task.usage_work {
                let _ = control.request();
            }
        }
        for task in self.warmup_tasks.values() {
            task.lease_control.cancel_waiting();
            let _ = task.network_wait.request();
            let _ = task.model_discovery.request();
        }
        if let Some(task) = &self.startup_cache_task {
            task.control.cancel_waiting();
        }
        if let Some(task) = &self.warmup_preflight {
            task.control.cancel_waiting();
        }
        for task in self.workspace_cache_writes.values() {
            task.control.cancel_waiting();
        }
        while self.has_pending_credential_tasks() {
            let _ = self.poll_startup_http_client().await;
            let _ = self.poll_startup_profile_result().await;
            self.poll_startup_auth_reconciliation().await;
            let _ = self.poll_startup_cache_result().await;
            self.poll_profile_mutation().await;
            self.poll_results();
            self.poll_usage_lease_releases();
            self.poll_workspace_cache_writes().await;
            self.poll_reset_card_results();
            self.poll_model_results();
            self.poll_profile_switch_results();
            self.poll_warmup_preflight_result().await;
            self.poll_warmup_results().await;
            self.poll_account_tasks().await;
            self.poll_usage_cache_invalidations().await;
            if self.has_pending_credential_tasks() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // A sender completes just before its handle, so one final drain applies
        // messages that arrived between the last channel poll and handle join.
        let _ = self.poll_startup_http_client().await;
        self.poll_results();
        self.poll_usage_lease_releases();
        self.poll_workspace_cache_writes().await;
        self.poll_reset_card_results();
        self.poll_model_results();
        self.poll_profile_switch_results();
        self.poll_usage_cache_invalidations().await;
    }

    /// Kick off a model-list fetch for `alias` if the detail panel needs it
    /// and it isn't already loaded or in flight. Idempotent — safe to call
    /// every frame.
    pub fn ensure_models_loaded(&mut self, alias: &str) {
        if !self.credential_operations_ready() {
            return;
        }
        // Errors are stable session state too. Retrying every render tick can
        // hammer a permanently failing endpoint; `refresh_one` invalidates a
        // terminal state and the background scheduler retries after quota work.
        if self.model_cache.contains_key(alias) {
            return;
        }
        let Some(expected_binding) = self
            .accounts
            .iter()
            .find(|account| account.alias == alias)
            .and_then(|account| strict_account_identity(&account.info))
        else {
            self.model_cache.insert(
                alias.to_string(),
                ModelStatus::Error("account identity is incomplete".to_string()),
            );
            self.mark_render_changed();
            return;
        };
        let path = match profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => return,
        };
        let Some(http_client) = self.request_client() else {
            return;
        };
        self.model_cache
            .insert(alias.to_string(), ModelStatus::Loading);
        self.mark_render_changed();
        let request_id = self.model_next_id;
        self.model_next_id = self.model_next_id.wrapping_add(1);
        self.model_requests.insert(alias.to_string(), request_id);
        let alias_owned = alias.to_string();
        let tx = self.model_sender.clone();
        let limiter = self.usage_limiter.clone();
        let tracked_alias = alias_owned.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let network_wait = SafeTaskCancellation::new();
        let task_network_wait = network_wait.clone();
        let read_only_work = SafeTaskCancellation::new();
        let task_read_only_work = read_only_work.clone();
        let task_binding = expected_binding.clone();
        let handle = tokio::spawn(async move {
            let lease = match profile::acquire_profile_lease_async_cancellable(
                alias_owned.clone(),
                &task_lease_control,
            )
            .await
            {
                Ok(Some(lease)) => lease,
                Ok(None) => return,
                Err(error) => {
                    let _ = tx
                        .send((
                            alias_owned,
                            task_binding,
                            request_id,
                            Err(format!(
                                "failed to lock profile for model discovery: {error:#}"
                            )),
                        ))
                        .await;
                    return;
                }
            };
            let first_permit = cancellable_first_network_permit(limiter, task_network_wait.clone());
            let prepared = match crate::warmup::prepare_models_for_profile_leased_with_client(
                &alias_owned,
                &path,
                lease,
                &task_binding,
                &http_client,
                first_permit,
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(error) if crate::warmup::network_wait_was_cancelled(&error) => return,
                Err(error) => {
                    let _ = tx
                        .send((
                            alias_owned,
                            task_binding,
                            request_id,
                            Err(error.to_string()),
                        ))
                        .await;
                    return;
                }
            };
            let result = tokio::select! {
                _ = task_read_only_work.cancelled() => {
                    task_read_only_work.mark_completed();
                    return;
                }
                result = crate::warmup::fetch_prepared_models_with_client(
                    prepared,
                    &http_client,
                ) => result,
            };
            let result = match result {
                Err(error) if crate::warmup::network_wait_was_cancelled(&error) => return,
                result => result.map_err(|error| error.to_string()),
            };
            let _ = tx
                .send((alias_owned, task_binding, request_id, result))
                .await;
        });
        self.track_account_task_with_controls(
            tracked_alias,
            AccountTaskKind::Model { request_id },
            lease_control,
            AccountTaskControls {
                network_wait: Some(network_wait),
                read_only_work: Some(read_only_work),
                ..AccountTaskControls::default()
            },
            handle,
        );
    }

    /// Fetch the model list for the currently-selected account, if the
    /// detail panel is visible. No-op when nothing is selected.
    pub fn ensure_models_loaded_for_selected(&mut self) {
        if !self.detail_visible || self.profile_switch_in_flight() {
            return;
        }
        if let Some(alias) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|e| e.alias.clone())
        {
            self.ensure_models_loaded(&alias);
        }
    }

    fn latest_core_usage_released_for(&self, alias: &str) -> bool {
        !self.refreshing_requests.contains_key(alias)
            && self.usage_generations.get(alias).is_none_or(|generation| {
                self.usage_lease_release_generations.get(alias) == Some(generation)
            })
    }

    fn selected_core_usage_is_settled(&self) -> bool {
        self.selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .is_some_and(|account| {
                matches!(
                    account.usage,
                    UsageStatus::Loaded(_) | UsageStatus::Error(_)
                ) && self.latest_core_usage_released_for(&account.alias)
            })
    }

    pub fn poll_model_results(&mut self) {
        let mut refresh_open_account = false;
        let mut changed = false;
        while let Ok((alias, binding, request_id, result)) = self.pending_models.try_recv() {
            let is_current_request = self
                .model_requests
                .get(&alias)
                .is_some_and(|active_id| *active_id == request_id);
            let identity_matches = self
                .accounts
                .iter()
                .find(|account| account.alias == alias)
                .and_then(|account| strict_account_identity(&account.info))
                .is_some_and(|current| current == binding);
            if !is_current_request {
                continue;
            }
            if !identity_matches {
                self.model_requests.remove(&alias);
                if matches!(self.model_cache.get(&alias), Some(ModelStatus::Loading)) {
                    self.model_cache.remove(&alias);
                    changed = true;
                }
                continue;
            }
            self.model_requests.remove(&alias);
            refresh_open_account |= matches!(
                self.menu.as_ref(),
                Some(super::menu::MenuState::Account { info, .. }) if info.alias == alias
            );
            self.model_cache.insert(
                alias,
                match result {
                    Ok(models) => ModelStatus::Loaded(models),
                    Err(e) => ModelStatus::Error(e),
                },
            );
            changed = true;
        }
        if changed {
            self.mark_render_changed();
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
    }

    fn start_auth_expiry_load(&mut self, alias: String, binding: StrictAccountBinding) {
        if let Some(previous) = self.auth_expiry_latest.get(&alias).copied()
            && let Some(task) = self.auth_expiry_tasks.get(&previous)
        {
            task.handle.abort();
        }
        let request_id = self.auth_expiry_next_id;
        self.auth_expiry_next_id = self.auth_expiry_next_id.wrapping_add(1);
        let worker_alias = alias.clone();
        let worker_binding = binding.clone();
        let handle = tokio::task::spawn_blocking(move || {
            read_profile_auth_expiries(&worker_alias, &worker_binding)
        });
        self.auth_expiry_latest.insert(alias.clone(), request_id);
        self.auth_expiry_tasks.insert(
            request_id,
            AuthExpiryTask {
                alias,
                binding,
                request_id,
                handle,
            },
        );
    }

    async fn poll_auth_expiry_tasks(&mut self) {
        let finished = self
            .auth_expiry_tasks
            .iter()
            .filter_map(|(request_id, task)| task.handle.is_finished().then_some(*request_id))
            .collect::<Vec<_>>();
        for request_id in finished {
            let Some(task) = self.auth_expiry_tasks.remove(&request_id) else {
                continue;
            };
            let is_latest = self
                .auth_expiry_latest
                .get(&task.alias)
                .is_some_and(|latest| *latest == task.request_id);
            if !is_latest {
                continue;
            }
            self.auth_expiry_latest.remove(&task.alias);
            let result = match task.handle.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!(
                    "account-detail auth task failed: {}",
                    crate::task_batch::join_failure_detail(&error)
                )),
            };
            let account_matches = self
                .accounts
                .iter()
                .find(|account| account.alias == task.alias)
                .and_then(|account| strict_account_identity(&account.info))
                .as_ref()
                == Some(&task.binding);
            let menu_matches = matches!(
                self.menu.as_ref(),
                Some(super::menu::MenuState::Account { info, .. })
                    if info.alias == task.alias
                        && info.account_id.as_deref() == Some(task.binding.account_id.as_str())
                        && info.email.as_deref().is_some_and(|email| {
                            email.trim().eq_ignore_ascii_case(&task.binding.email)
                        })
            );
            if !account_matches || !menu_matches {
                continue;
            }
            match result {
                Ok(expiries) => {
                    if let Some(super::menu::MenuState::Account { info, .. }) = self.menu.as_mut() {
                        let changed = info.auth_expiries.len() != expiries.len()
                            || info
                                .auth_expiries
                                .iter()
                                .zip(&expiries)
                                .any(|(left, right)| {
                                    left.name != right.name || left.expires_at != right.expires_at
                                });
                        info.auth_expiries = expiries;
                        if changed {
                            self.mark_render_changed();
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        "[{}] account-detail token expiry unavailable: {error:#}",
                        task.alias
                    );
                }
            }
        }
    }

    fn rebuild_open_account_menu(&mut self) {
        let (scroll, menu_alias, menu_account_id, menu_email, auth_expiries) =
            match self.menu.as_ref() {
                Some(super::menu::MenuState::Account { popup, info, .. }) => (
                    popup.scroll,
                    info.alias.clone(),
                    info.account_id.clone(),
                    info.email.clone(),
                    info.auth_expiries.clone(),
                ),
                _ => return,
            };
        let same_account = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .is_some_and(|account| {
                account.alias == menu_alias
                    && account.info.account_id == menu_account_id
                    && match (account.info.email.as_deref(), menu_email.as_deref()) {
                        (Some(current), Some(open)) => current.eq_ignore_ascii_case(open),
                        (None, None) => true,
                        _ => false,
                    }
            });
        if same_account {
            self.open_account_menu_with_auth_expiries(auth_expiries);
        } else {
            self.open_account_menu();
        }
        if let Some(super::menu::MenuState::Account { popup, .. }) = self.menu.as_mut() {
            popup.scroll = scroll;
        }
    }

    pub fn open_help(&mut self) {
        self.help_popup = Some(super::popup::PopupState::new());
    }

    pub fn close_help(&mut self) {
        self.help_popup = None;
    }

    pub fn open_account_menu(&mut self) {
        let Some(account_idx) = self.selected_account_idx() else {
            return;
        };
        let alias = self.accounts[account_idx].alias.clone();
        if let Some(binding) = strict_account_identity(&self.accounts[account_idx].info) {
            self.start_auth_expiry_load(alias, binding);
        }
        self.open_account_menu_with_auth_expiries(Vec::new());
    }

    fn open_account_menu_with_auth_expiries(
        &mut self,
        auth_expiries: Vec<super::menu::AuthExpiry>,
    ) {
        let Some(account_idx) = self.selected_account_idx() else {
            return;
        };
        let alias = self.accounts[account_idx].alias.clone();
        self.ensure_models_loaded(&alias);
        let entry = &self.accounts[account_idx];
        let loaded_usage = match &entry.usage {
            UsageStatus::Loaded(u) => Some(u.as_ref()),
            _ => None,
        };
        let plan = loaded_usage
            .and_then(|u| u.plan_type.as_deref())
            .or(entry.info.plan_type.as_deref());
        let reset_cards = loaded_usage.and_then(reset_credits_count);
        let reset_card_expiries = loaded_usage
            .map(|u| {
                let mut credits: Vec<_> = u.reset_credits.iter().collect();
                credits.sort_by_key(|credit| reset_credit_expiry_sort_key(credit));
                credits
                    .into_iter()
                    .map(|credit| {
                        let granted = credit
                            .granted_at
                            .as_deref()
                            .map(format_local_datetime)
                            .unwrap_or_else(|| "grant date unavailable".to_string());
                        let expires = credit
                            .expires_at
                            .as_deref()
                            .map(format_local_datetime)
                            .unwrap_or_else(|| "no expiry date".to_string());
                        format!("expires {expires} · granted {granted}")
                    })
                    .collect()
            })
            .unwrap_or_default();
        let can_consume_reset_card = loaded_usage
            .and_then(|u| crate::usage::earliest_reset_credit(&u.reset_credits))
            .is_some()
            && loaded_usage
                .is_some_and(|usage| crate::usage::explicit_account_blocker(usage).is_none())
            && loaded_usage.is_some_and(|usage| usage.reset_credits_error.is_none())
            && !self.reset_cards_in_flight.contains(&entry.alias);
        let usage_meta: Vec<String> = loaded_usage
            .map(|usage| {
                let mut items = Vec::new();
                if usage.account_limited || usage.rate_limit_reached_type.is_some() {
                    let reason = usage
                        .rate_limit_reached_type
                        .as_deref()
                        .map(|value| format!(" · {}", value.replace(['_', '-'], " ")))
                        .unwrap_or_default();
                    items.push(format!("  Status  limited{reason}"));
                }
                if usage.unlimited_credits == Some(true) {
                    items.push("  credits unlimited".to_string());
                } else if let Some(balance) = usage.credits_balance {
                    items.push(format!("  credits ${balance:.2}"));
                }
                if usage.reset_credits_error.is_some() {
                    items.push("  Reset-card details are temporarily unavailable".to_string());
                }
                if let Some(limit) = &usage.individual_limit {
                    let mut parts = vec!["  Monthly API".to_string()];
                    if let Some(value) = &limit.limit {
                        parts.push(format!("{value} total"));
                    }
                    if let Some(value) = &limit.used {
                        parts.push(format!("{value} used"));
                    }
                    if let Some(value) = &limit.remaining {
                        parts.push(format!("{value} remaining"));
                    }
                    if let Some(value) = limit.remaining_percent {
                        parts.push(format!("{value:.0}% left"));
                    }
                    if let Some(value) = limit.resets_at {
                        parts.push(format!("resets {}", format_local_timestamp(value)));
                    }
                    if parts.len() > 1 {
                        items.push(parts.join(" · "));
                    }
                }
                items
            })
            .unwrap_or_default()
            .into_iter()
            .collect();
        let models: Vec<String> = match self.model_cache.get(&entry.alias) {
            Some(ModelStatus::Loaded(models)) => crate::warmup::sorted_models_for_display(models)
                .into_iter()
                .map(|model| {
                    let label = match &model.display_name {
                        Some(name) => name.clone(),
                        None => model.slug.clone(),
                    };
                    let default = model
                        .default_reasoning_effort
                        .as_deref()
                        .unwrap_or("not reported");
                    let allowed = if model.supported_reasoning_efforts.is_empty() {
                        "not reported".to_string()
                    } else {
                        model.supported_reasoning_efforts.join(", ")
                    };
                    format!("  {label} · default {default} · allowed {allowed}")
                })
                .collect(),
            Some(ModelStatus::Error(error)) => vec![format!("  error: {error}")],
            _ => vec!["  loading...".to_string()],
        };
        self.menu = Some(super::menu::MenuState::account(
            super::menu::AccountMenuInfo {
                alias: entry.alias.clone(),
                email: entry.info.email.clone(),
                account_id: entry.info.account_id.clone(),
                user_id: entry.info.user_id.clone(),
                workspace_name: entry.info.workspace_name.clone(),
                is_fedramp: entry.info.is_fedramp,
                plan_label: entry.info.plan_label_with(plan),
                plan_type: plan.map(str::to_string),
                is_current: entry.is_current,
                organizations: entry
                    .info
                    .organizations
                    .iter()
                    .filter(|organization| !organization.title.is_empty())
                    .map(|organization| {
                        let role = organization
                            .role
                            .split(['_', '-'])
                            .filter(|part| !part.is_empty())
                            .map(|part| {
                                let mut chars = part.chars();
                                chars
                                    .next()
                                    .map(|first| {
                                        first.to_uppercase().collect::<String>() + chars.as_str()
                                    })
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!(
                            "{} · {}{}",
                            organization.title,
                            if role.is_empty() { "Member" } else { &role },
                            if organization.is_default {
                                " · default workspace"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect(),
                auth_expiries,
                usage: loaded_usage.cloned().map(Box::new),
                usage_meta,
                models,
                reset_cards,
                reset_card_expiries,
                can_consume_reset_card,
            },
        ));
    }

    pub fn open_batch_menu(&mut self) {
        let count = self.marked.len();
        if count == 0 {
            return;
        }
        self.menu = Some(super::menu::MenuState::batch(count));
    }

    pub fn open_batch_relogin_flow(&mut self) {
        let count = self.marked.len();
        if count == 0 {
            return;
        }
        self.menu = Some(super::menu::MenuState::batch_relogin_flow(count));
    }

    pub fn open_add_menu(&mut self) {
        self.menu = Some(super::menu::MenuState::add());
    }

    pub fn open_relogin_flow_menu(&mut self, alias: String, email: Option<String>) {
        self.menu = Some(super::menu::MenuState::relogin_flow(alias, email));
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Warmup just one alias.
    pub fn warmup_one(&mut self, alias: &str) {
        if self.reject_new_credential_operation() {
            return;
        }
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.alias == alias)
            .map(|(i, _)| i)
            .collect();
        self.start_warmup_preflight(
            target_indices,
            WarmupPreflightOrigin::Single {
                alias: alias.to_string(),
            },
        );
    }

    pub fn request_consume_reset_card(&mut self, alias: &str) {
        if self.reject_new_credential_operation() {
            return;
        }
        if self.interactive_operation_in_flight() {
            self.set_status(
                "Finish the active confirmation or profile switch before using a reset card"
                    .to_string(),
                5,
            );
            return;
        }
        if self.reset_cards_in_flight.contains(alias) {
            self.set_status(format!("{alias}: reset card use is already in progress"), 4);
            return;
        }
        if self.usage_metadata_requests.contains_key(alias) {
            self.set_status(
                format!("{alias}: finishing reset-card verification; try again shortly"),
                4,
            );
            return;
        }
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        let UsageStatus::Loaded(u) = &entry.usage else {
            self.set_status(format!("{alias}: refresh usage before using reset card"), 4);
            return;
        };
        if let Some(blocker) = crate::usage::explicit_account_blocker(u) {
            self.set_status(
                format!(
                    "{alias}: reset card cannot clear the account/workspace restriction ({blocker})"
                ),
                6,
            );
            return;
        }
        if let Some(error) = u.reset_credits_error.as_deref() {
            self.set_status_error(
                format!("{alias}: reset-card details could not be verified ({error})"),
                6,
            );
            return;
        }
        let Some(credit) = crate::usage::earliest_reset_credit(&u.reset_credits) else {
            self.set_status(format!("{alias}: no available reset cards"), 4);
            return;
        };
        self.confirm = Some(ConfirmAction::ConsumeResetCard {
            alias: alias.to_string(),
            credit: credit.clone(),
            expires_at: credit
                .expires_at
                .as_deref()
                .map(format_local_datetime)
                .unwrap_or_else(|| "no expiry".to_string()),
        });
    }

    /// Request delete confirmation for a specific alias (called from menu).
    pub fn request_delete_alias(&mut self, alias: &str) {
        if self.interactive_operation_in_flight() {
            self.set_status(
                "Finish the active confirmation or profile switch before deleting".to_string(),
                5,
            );
            return;
        }
        if self.reject_new_credential_operation() {
            return;
        }
        if self.account_operation_in_flight(alias) {
            self.set_status(
                format!("{alias}: wait for the account operation to finish before deleting"),
                5,
            );
            return;
        }
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        if entry.is_current {
            self.set_status_error("Cannot delete the active profile".to_string(), 3);
            return;
        }
        self.confirm = Some(ConfirmAction::Delete(entry.alias.clone()));
    }

    /// Begin rename for a specific alias (called from menu).
    pub fn start_rename_alias(&mut self, alias: &str) {
        if self.interactive_operation_in_flight() {
            self.set_status(
                "Finish the active confirmation, rename, or profile switch before renaming"
                    .to_string(),
                5,
            );
            return;
        }
        if self.reject_new_credential_operation() {
            return;
        }
        if self.account_operation_in_flight(alias) {
            self.set_status(
                format!("{alias}: wait for the account operation to finish before renaming"),
                5,
            );
            return;
        }
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        let old = entry.alias.clone();
        let len = grapheme_count(&old);
        self.rename = Some(RenameState {
            old_alias: old.clone(),
            input: old,
            cursor: len,
        });
    }

    #[cfg(test)]
    pub fn load_profiles(&mut self) {
        if let Err(error) = self.try_load_profiles() {
            self.set_status_error(format!("Profile reload failed: {error:#}"), 6);
        }
    }

    #[cfg(test)]
    fn try_load_profiles(&mut self) -> Result<()> {
        // The live auth file is the source of truth. If it belongs to an
        // untracked account, no saved profile is active; retaining the stale
        // marker here would highlight the account that used to be active.
        let current = sync_current_from_live().context("synchronizing the active profile")?;
        self.try_load_profiles_with_current(current)
    }

    #[cfg(test)]
    fn load_profiles_from_marker(&mut self) {
        if let Err(error) = self.try_load_profiles_from_marker() {
            self.set_status_error(format!("Profile reload failed: {error:#}"), 6);
        }
    }

    #[cfg(test)]
    fn try_load_profiles_from_marker(&mut self) -> Result<()> {
        let current = read_current_checked().context("reading the active profile marker")?;
        self.try_load_profiles_with_current(current)
    }

    fn apply_displayed_current(&mut self, current: Option<&str>) {
        let selected_alias = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|entry| entry.alias.clone());
        let mut changed = false;
        for account in &mut self.accounts {
            let is_current = current == Some(account.alias.as_str());
            changed |= account.is_current != is_current;
            account.is_current = is_current;
        }
        if changed {
            self.mark_render_changed();
        }
        self.update_view();
        if let Some(alias) = selected_alias
            && let Some(account_idx) = self.accounts.iter().position(|entry| entry.alias == alias)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
        self.rebuild_open_account_menu();
    }

    #[cfg(test)]
    fn try_load_profiles_with_current(&mut self, current: Option<String>) -> Result<()> {
        self.try_load_profiles_with_current_invalidating(current, &BTreeSet::new())
    }

    #[cfg(test)]
    fn try_load_profiles_with_current_invalidating(
        &mut self,
        current: Option<String>,
        invalidated_aliases: &BTreeSet<String>,
    ) -> Result<()> {
        let loaded = profile::load_profile_accounts()
            .context("loading saved profiles")?
            .into_iter()
            .map(|account| (account.alias, account.info))
            .collect::<Vec<_>>();

        self.apply_loaded_profiles(current, loaded, invalidated_aliases);
        Ok(())
    }

    fn apply_loaded_profiles(
        &mut self,
        current: Option<String>,
        loaded: Vec<(String, AccountInfo)>,
        invalidated_aliases: &BTreeSet<String>,
    ) {
        // Do not take or mutate the displayed model until every path/read and
        // active-binding check above has succeeded.
        let mut retained_usage = retained_usage_by_identity(std::mem::take(&mut self.accounts));
        let mut unchanged_bindings = BTreeSet::new();
        self.accounts = loaded
            .into_iter()
            .map(|(alias, info)| {
                let identity = strict_account_identity(&info);
                let usage = match retained_usage.remove(&alias) {
                    Some((Some(previous), usage))
                        if identity.as_ref() == Some(&previous)
                            && !invalidated_aliases.contains(&alias) =>
                    {
                        unchanged_bindings.insert(alias.clone());
                        usage
                    }
                    _ => UsageStatus::Idle,
                };
                AccountEntry {
                    usage,
                    is_current: current.as_deref() == Some(alias.as_str()),
                    alias,
                    info,
                }
            })
            .collect();
        let known_account_ids = self
            .accounts
            .iter()
            .filter_map(|account| account.info.account_id.as_deref())
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let workspace_now = Instant::now();
        self.workspace_states.retain(|account_id, resolution| {
            known_account_ids.contains(account_id) && resolution.is_fresh(workspace_now)
        });
        self.recompute_workspace_expiry_deadline();
        for account in &mut self.accounts {
            if let Some(account_id) = account.info.account_id.as_deref()
                && let Some(resolution) = self.workspace_states.get(account_id)
            {
                cache::apply_workspace_state(&mut account.info, &resolution.state);
            }
        }
        self.model_cache
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.model_requests
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.marked
            .retain(|alias| unchanged_bindings.contains(alias));
        // Preserve work for accounts whose complete identity and credential
        // generation did not change. Reloading one alias must not strand an
        // unrelated row in Loading or discard its already-paid network result.
        self.refreshing_requests
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.pending_usage_refreshes
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.usage_generations
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.usage_lease_release_generations
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.usage_metadata_requests
            .retain(|alias, _| unchanged_bindings.contains(alias));
        self.workspace_requests
            .retain(|account_id, _| known_account_ids.contains(account_id));
        for account in &mut self.accounts {
            if matches!(account.usage, UsageStatus::Loading)
                && !self.refreshing_requests.contains_key(&account.alias)
            {
                account.usage = UsageStatus::Idle;
            }
        }
        self.selected = 0;
        self.view_indices.clear();
        self.update_view();
        if let Some(account_idx) = self.accounts.iter().position(|a| a.is_current)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
        self.mark_render_changed();
    }

    fn try_load_profiles_from_marker_preserving_selection(
        &mut self,
        invalidated_aliases: &BTreeSet<String>,
    ) -> Result<()> {
        let current = read_current_checked().context("reading the active profile marker")?;
        let loaded = profile::load_profile_accounts()
            .context("loading saved profiles")?
            .into_iter()
            .map(|account| (account.alias, account.info))
            .collect();
        self.apply_profile_snapshot_preserving_selection(current, loaded, invalidated_aliases);
        Ok(())
    }

    fn apply_profile_snapshot_preserving_selection(
        &mut self,
        current: Option<String>,
        loaded: Vec<(String, AccountInfo)>,
        invalidated_aliases: &BTreeSet<String>,
    ) {
        let selected_alias = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|entry| entry.alias.clone());

        self.apply_loaded_profiles(current, loaded, invalidated_aliases);

        if let Some(alias) = selected_alias
            && let Some(account_idx) = self.accounts.iter().position(|a| a.alias == alias)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
    }

    async fn complete_credential_recovery_reload(
        &mut self,
        invalidated_aliases: &BTreeSet<String>,
    ) -> Result<()> {
        let reconciliation = tokio::task::spawn_blocking(profile::reconcile_live_auth_for_tui)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "live credential reconciliation task failed: {}",
                    crate::task_batch::join_failure_detail(&error)
                )
            })??;
        self.apply_auth_reconciliation(reconciliation, invalidated_aliases)?;
        self.startup_auth_state = StartupAuthState::Ready;
        Ok(())
    }

    async fn finish_oauth_attempt(
        &mut self,
        result: Result<String>,
        invalidated_aliases: &BTreeSet<String>,
    ) {
        // An OAuth round can persist replacement credentials before a later
        // live-auth update reports an error. Always discard pre-login model
        // generations and reconcile the displayed profiles before deciding
        // whether ordinary credential work is safe again.
        self.invalidate_models_after_credential_reload(invalidated_aliases);
        let reload_result = self
            .complete_credential_recovery_reload(invalidated_aliases)
            .await;
        let reload_succeeded = match (result, reload_result) {
            (Ok(message), Ok(())) => {
                self.set_status(message, 5);
                true
            }
            (Err(error), Ok(())) => {
                self.set_status_error(format!("OAuth failed: {error}"), 7);
                true
            }
            (Ok(message), Err(reload_error)) => {
                self.startup_auth_state = StartupAuthState::Blocked;
                self.set_status_error(
                    format!("{message}; profile reload failed: {reload_error:#}"),
                    8,
                );
                false
            }
            (Err(error), Err(reload_error)) => {
                self.startup_auth_state = StartupAuthState::Blocked;
                self.set_status_error(
                    format!("OAuth failed: {error}; profile reload failed: {reload_error:#}"),
                    8,
                );
                false
            }
        };
        if reload_succeeded {
            self.refresh(Refresh::Forced);
            if self.auto_refresh_enabled {
                self.next_auto_refresh = Some(Instant::now() + self.auto_refresh_interval);
            }
        }
    }

    /// Credentials for one or more saved aliases were replaced. Clear both
    /// terminal cache entries and active generations: a late response made
    /// with the previous credentials must not bind to the replacement login.
    fn invalidate_models_after_credential_reload(
        &mut self,
        invalidated_aliases: &BTreeSet<String>,
    ) {
        self.model_cache
            .retain(|alias, _| !invalidated_aliases.contains(alias));
        self.model_requests
            .retain(|alias, _| !invalidated_aliases.contains(alias));
    }

    /// Recompute `view_indices` based on the current search query.
    pub fn update_view(&mut self) {
        let selected_account_idx = self.selected_account_idx();

        self.view_indices = match &self.search {
            None => (0..self.accounts.len()).collect(),
            Some(s) if s.query.is_empty() => (0..self.accounts.len()).collect(),
            Some(s) => {
                let q = s.query.to_lowercase();
                self.accounts
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        entry.alias.to_lowercase().contains(&q)
                            || entry
                                .info
                                .email
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&q)
                            || entry
                                .info
                                .plan_type
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&q)
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        };

        match self.sort_mode {
            SortMode::Name => {}
            SortMode::Quota => {
                let quotas: Vec<Option<f64>> = (0..self.accounts.len())
                    .map(|idx| self.quota_used_percent(idx))
                    .collect();
                self.view_indices
                    .sort_by(|&a, &b| match (quotas[a], quotas[b]) {
                        (Some(left), Some(right)) => left.total_cmp(&right),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    });
            }
            SortMode::Status => {
                let statuses: Vec<u8> = (0..self.accounts.len())
                    .map(|idx| self.status_order(idx))
                    .collect();
                self.view_indices
                    .sort_by(|&a, &b| statuses[a].cmp(&statuses[b]));
            }
        }

        if let Some(account_idx) = selected_account_idx
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
            return;
        }

        if self.view_indices.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.view_indices.len() {
            self.selected = self.view_indices.len() - 1;
        }
    }

    /// Get the selected index in `accounts`.
    pub fn selected_account_idx(&self) -> Option<usize> {
        self.view_indices.get(self.selected).copied()
    }

    pub fn loading_count(&self) -> usize {
        self.refreshing_requests.len()
    }

    /// Derive the dashboard aggregate from every registered account's current
    /// in-memory usage. Search filtering affects only `view_indices`, never this
    /// pool. Disk-cache TTL decides whether a new read may reuse a cache entry;
    /// it does not expire a successful snapshot already owned by this TUI. The
    /// pure global calculation still rejects missing, invalid, or elapsed weekly
    /// windows, and this calculation performs no I/O or additional polling.
    pub fn global_weekly_summary(&self, now: i64) -> GlobalWeeklySummary {
        let inputs: Vec<GlobalPaceAccountInput> = self
            .accounts
            .iter()
            .map(|entry| match &entry.usage {
                UsageStatus::Loaded(usage) => {
                    GlobalPaceAccountInput::from_usage(entry.alias.clone(), usage)
                }
                UsageStatus::Idle | UsageStatus::Loading | UsageStatus::Error(_) => {
                    GlobalPaceAccountInput::unavailable(entry.alias.clone())
                }
            })
            .collect();
        calculate_global_weekly_summary(&inputs, now)
    }

    pub fn is_refreshing(&self, alias: &str) -> bool {
        self.refreshing_requests.contains_key(alias)
    }

    pub fn cycle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Name => SortMode::Quota,
            SortMode::Quota => SortMode::Status,
            SortMode::Status => SortMode::Name,
        };
        self.update_view();
    }

    pub fn toggle_mark(&mut self) {
        if let Some(idx) = self.selected_account_idx() {
            let alias = self.accounts[idx].alias.clone();
            if !self.marked.remove(&alias) {
                self.marked.insert(alias);
            }
        }

        if self.selected + 1 < self.view_indices.len() {
            self.selected += 1;
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    fn is_warmup_in_flight(&self, alias: &str) -> bool {
        self.warmup_tasks.values().any(|task| task.alias == alias)
            || self
                .warmup_preflight
                .as_ref()
                .is_some_and(|task| task.aliases.contains(alias))
    }

    fn start_warmup_preflight(
        &mut self,
        target_indices: Vec<usize>,
        origin: WarmupPreflightOrigin,
    ) {
        if self.shutting_down || !self.credential_operations_ready() {
            return;
        }
        if self.warmup_preflight.is_some() {
            self.set_status(
                "Warmup eligibility inspection is already in progress".to_string(),
                4,
            );
            return;
        }

        let candidate_count = target_indices.len();
        let mut candidates = Vec::new();
        for &idx in &target_indices {
            let Some(account) = self.accounts.get(idx) else {
                continue;
            };
            let alias = account.alias.clone();
            let loaded_usage = match &account.usage {
                UsageStatus::Loaded(usage) => Some(usage.as_ref().clone()),
                UsageStatus::Error(_) => continue,
                UsageStatus::Idle | UsageStatus::Loading => None,
            };
            if self.is_warmup_in_flight(&alias) {
                continue;
            }
            let Some(binding) = strict_account_identity(&account.info) else {
                continue;
            };
            candidates.push(WarmupPreflightCandidate {
                alias,
                binding,
                loaded_usage,
            });
        }

        if candidates.is_empty() {
            self.report_warmup_preflight_success(origin, candidate_count, Vec::new());
            return;
        }

        let aliases = candidates
            .iter()
            .map(|candidate| candidate.alias.clone())
            .collect();
        let control = cache::CacheLockAcquireControl::new();
        let worker_control = control.clone();
        let handle =
            tokio::spawn(
                async move { inspect_warmup_candidates(candidates, worker_control).await },
            );
        match &origin {
            WarmupPreflightOrigin::Single { alias } => {
                self.set_status(format!("Inspecting warmup state for {alias}..."), 6);
            }
            WarmupPreflightOrigin::Marked => {
                self.set_status(
                    format!("Inspecting warmup state for {candidate_count} marked account(s)..."),
                    6,
                );
            }
            WarmupPreflightOrigin::Automatic {
                refreshing_accounts,
            } => {
                self.set_status(
                    format!(
                        "Auto refresh: refreshing {refreshing_accounts} account(s), inspecting warmup eligibility"
                    ),
                    6,
                );
            }
        }
        self.warmup_preflight = Some(WarmupPreflightTask {
            origin,
            candidate_count,
            aliases,
            control,
            handle,
        });
    }

    fn warmup_all(&mut self, refreshing_accounts: usize) {
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        self.start_warmup_preflight(
            target_indices,
            WarmupPreflightOrigin::Automatic {
                refreshing_accounts,
            },
        );
    }

    fn report_warmup_preflight_success(
        &mut self,
        origin: WarmupPreflightOrigin,
        candidate_count: usize,
        candidates: Vec<WarmupReadyCandidate>,
    ) {
        let warmup_origin = if matches!(origin, WarmupPreflightOrigin::Automatic { .. }) {
            WarmupOrigin::Automatic
        } else {
            WarmupOrigin::Manual
        };
        let mut started_aliases = BTreeSet::new();
        for candidate in candidates {
            let alias = candidate.alias.clone();
            if self.spawn_preflighted_warmup(candidate, warmup_origin) {
                started_aliases.insert(alias);
            }
        }
        let started = started_aliases.len();
        let skipped = candidate_count.saturating_sub(started);
        if warmup_origin == WarmupOrigin::Automatic {
            let immediate_refreshes = self
                .accounts
                .iter()
                .enumerate()
                .filter_map(|(idx, account)| {
                    (!started_aliases.contains(&account.alias)).then_some(idx)
                })
                .collect::<Vec<_>>();
            for idx in immediate_refreshes {
                self.fetch_usage_for(
                    idx,
                    AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
                );
            }
        }

        match origin {
            WarmupPreflightOrigin::Single { alias } => {
                if started == 0 {
                    if candidate_count == 0 {
                        self.set_status(format!("{alias}: nothing to warm up"), 4);
                    } else {
                        self.set_status(format!("{alias}: already active or in flight"), 4);
                    }
                } else {
                    self.set_status(format!("Warming up {alias}..."), 6);
                }
            }
            WarmupPreflightOrigin::Marked => {
                if started == 0 {
                    self.set_status(
                        format!("All {candidate_count} marked already active or skipped"),
                        4,
                    );
                } else {
                    let mut message = format!("Warming up {started} marked account(s)");
                    if skipped > 0 {
                        message.push_str(&format!(" ({skipped} skipped)"));
                    }
                    self.set_status(message, 6);
                }
            }
            WarmupPreflightOrigin::Automatic {
                refreshing_accounts,
            } => {
                let mut message =
                    format!("Auto refresh: refreshing {refreshing_accounts} account(s)");
                if started > 0 {
                    message.push_str(&format!(", warming {started}"));
                }
                self.set_status(message, 4);
            }
        }
    }

    fn report_warmup_preflight_failure(&mut self, origin: WarmupPreflightOrigin, detail: String) {
        let automatic = matches!(origin, WarmupPreflightOrigin::Automatic { .. });
        let message = match origin {
            WarmupPreflightOrigin::Single { alias } => {
                format!("Could not inspect usage state before warming up {alias}: {detail}")
            }
            WarmupPreflightOrigin::Marked => {
                format!("Could not inspect usage state before marked warmup: {detail}")
            }
            WarmupPreflightOrigin::Automatic {
                refreshing_accounts,
            } => format!(
                "Auto refresh: refreshing {refreshing_accounts} account(s); automatic warmup could not inspect cached usage: {detail}"
            ),
        };
        if automatic {
            let indices = (0..self.accounts.len()).collect::<Vec<_>>();
            for idx in indices {
                self.fetch_usage_for(
                    idx,
                    AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
                );
            }
        }
        self.set_status_error(message, 6);
    }

    pub async fn poll_warmup_preflight_result(&mut self) {
        if !self
            .warmup_preflight
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            return;
        }
        if !self.shutting_down && self.profile_switch_in_flight() {
            return;
        }
        let Some(task) = self.warmup_preflight.take() else {
            return;
        };
        let origin = task.origin;
        let candidate_count = task.candidate_count;
        let joined = task.handle.await;
        if self.shutting_down {
            return;
        }

        match joined {
            Ok(Ok(Some(candidates))) => {
                // Account state may have refreshed while the blocking cache read
                // was in flight. Recheck every returned alias in memory before
                // starting any task, then publish the whole batch together.
                let now = match crate::auth::now_unix_secs() {
                    Ok(now) => now,
                    Err(error) => {
                        self.report_warmup_preflight_failure(
                            origin,
                            format!("reading system clock for warmup recheck: {error:#}"),
                        );
                        return;
                    }
                };
                let candidates = candidates
                    .into_iter()
                    .filter_map(|mut candidate| {
                        let account = self
                            .accounts
                            .iter()
                            .find(|account| account.alias == candidate.alias)?;
                        if strict_account_identity(&account.info).as_ref()
                            != Some(&candidate.binding)
                            || self.is_warmup_in_flight(&candidate.alias)
                        {
                            return None;
                        }
                        match &account.usage {
                            UsageStatus::Error(_) => None,
                            UsageStatus::Loaded(usage)
                                if crate::usage::usage_has_active_warmup_window(usage, now) =>
                            {
                                None
                            }
                            UsageStatus::Loaded(usage) => {
                                candidate.cached_usage = Some(usage.as_ref().clone());
                                Some(candidate)
                            }
                            UsageStatus::Idle | UsageStatus::Loading => Some(candidate),
                        }
                    })
                    .collect();
                self.report_warmup_preflight_success(origin, candidate_count, candidates);
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                self.report_warmup_preflight_failure(origin, format!("{error:#}"));
            }
            Err(error) => {
                self.report_warmup_preflight_failure(
                    origin,
                    crate::task_batch::join_failure_detail(&error),
                );
            }
        }
    }

    pub fn refresh_one(&mut self, alias: &str) {
        if self.reject_new_credential_operation() {
            return;
        }
        let Some(idx) = self
            .accounts
            .iter()
            .position(|account| account.alias == alias)
        else {
            return;
        };
        if !self.model_requests.contains_key(alias) {
            self.model_cache.remove(alias);
        }
        self.fetch_usage_for(
            idx,
            AccountRefreshPlan::usage_and_workspace(Refresh::Forced),
        );
        self.set_status(format!("Refreshing {alias}"), 3);
    }

    fn spawn_preflighted_warmup(
        &mut self,
        candidate: WarmupReadyCandidate,
        origin: WarmupOrigin,
    ) -> bool {
        let WarmupReadyCandidate {
            alias,
            binding: expected_binding,
            cached_usage,
        } = candidate;
        if !self.credential_operations_ready() {
            return false;
        }
        // Skip if this alias already has an in-flight warmup task.
        if self.is_warmup_in_flight(&alias) {
            return false;
        }
        let identity_is_current = self
            .accounts
            .iter()
            .find(|account| account.alias == alias)
            .and_then(|account| strict_account_identity(&account.info))
            .as_ref()
            == Some(&expected_binding);
        if !identity_is_current {
            self.set_status_error(
                format!("Cannot warm up {alias}: account identity changed after preflight"),
                5,
            );
            return false;
        }
        let task_id = self.warmup_next_id;
        self.warmup_next_id = self.warmup_next_id.wrapping_add(1);
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return false;
            }
        };
        let limiter = self.usage_limiter.clone();
        let Some(http_client) = self.request_client() else {
            return false;
        };
        let tracked_alias = alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let network_wait = SafeTaskCancellation::new();
        let task_network_wait = network_wait.clone();
        let model_discovery = SafeTaskCancellation::new();
        let task_model_discovery_wait = model_discovery.clone();
        let task_model_discovery_commit = model_discovery.clone();
        let task_binding = expected_binding.clone();
        let handle = tokio::spawn(async move {
            let lease = match profile::acquire_profile_lease_async_cancellable(
                alias.clone(),
                &task_lease_control,
            )
            .await
            {
                Ok(Some(lease)) => lease,
                Ok(None) => return Ok(()),
                Err(error) => return Err(format!("failed to lock profile for warmup: {error:#}")),
            };
            let first_permit = cancellable_first_network_permit(limiter, task_network_wait.clone());
            let controls = crate::warmup::WarmupExecutionControls::cancellable(
                first_permit,
                async move {
                    task_model_discovery_wait.cancelled().await;
                    task_model_discovery_wait.mark_completed();
                },
                move || {
                    let committed = task_model_discovery_commit.begin_work();
                    if !committed {
                        task_model_discovery_commit.mark_completed();
                    }
                    committed
                },
            );
            let result =
                crate::warmup::warmup_account_leased_with_client_after_usage_preflight_with_controls(
                    &alias,
                    &path,
                    lease,
                    &http_client,
                    &task_binding,
                    cached_usage,
                    controls,
                )
                .await;
            match result {
                Ok(lease) => {
                    drop(lease);
                    Ok(())
                }
                Err(error) if crate::warmup::warmup_wait_was_cancelled(&error) => Ok(()),
                Err(error) => {
                    tracing::error!(alias = %alias, error = %format!("{error:#}"), "warmup failed");
                    Err(format!("{error:#}"))
                }
            }
        });
        self.warmup_tasks.insert(
            task_id,
            WarmupTask {
                alias: tracked_alias,
                binding: expected_binding,
                origin,
                started: Instant::now(),
                slow_reported: false,
                lease_control,
                network_wait,
                model_discovery,
                handle,
            },
        );
        true
    }

    pub fn poll_update(&mut self) {
        if let Some(rx) = &mut self.update_rx {
            match rx.try_recv() {
                Ok(version) => {
                    self.update_available = Some(version);
                    self.update_rx = None;
                    self.mark_render_changed();
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending (no update or check failed)
                    self.update_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Still waiting, keep polling
                }
            }
        }
    }

    pub fn start_update_check(&mut self) {
        if self.update_rx.is_some() || self.update_available.is_some() {
            return;
        }
        // Startup owns the single configured client. If it was not built,
        // update discovery must not create a separate pool or CA state.
        let Some(client) = self.http_client.clone() else {
            return;
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_rx = Some(rx);
        let is_dev = crate::update::current_version().contains("-dev");
        tokio::spawn(async move {
            let result = if is_dev {
                crate::update::check_for_dev_update_with_client(&client).await
            } else {
                crate::update::check_for_update_with_client(false, &client).await
            };
            if let Ok(Some(info)) = result {
                let _ = tx.send(info.latest_version);
            }
        });
    }

    pub async fn poll_warmup_results(&mut self) {
        let mut to_refresh = BTreeMap::<String, AccountRefreshPlan>::new();
        let mut successes = Vec::new();
        let mut failures = Vec::new();
        let mut refresh_deferred = false;

        let mut finished_task_ids: Vec<u64> = self
            .warmup_tasks
            .iter()
            .filter_map(|(task_id, task)| task.handle.is_finished().then_some(*task_id))
            .collect();
        finished_task_ids.sort_unstable();

        for task_id in finished_task_ids {
            let Some(task) = self.warmup_tasks.remove(&task_id) else {
                continue;
            };
            let alias = task.alias;
            let origin = task.origin;
            let identity_matches = self
                .accounts
                .iter()
                .find(|account| account.alias == alias)
                .and_then(|account| strict_account_identity(&account.info))
                .is_some_and(|current| current == task.binding);
            let cancelled_safely = task.lease_control.is_cancelled()
                || task.network_wait.completed()
                || task.model_discovery.completed();
            let joined = task.handle.await;
            if cancelled_safely && matches!(&joined, Ok(Ok(()))) {
                continue;
            }
            if !identity_matches {
                continue;
            }
            match joined {
                Ok(Ok(())) => {
                    to_refresh.insert(
                        alias.clone(),
                        AccountRefreshPlan::usage_only(Refresh::Forced),
                    );
                    successes.push(alias);
                }
                Ok(Err(e)) => {
                    if origin == WarmupOrigin::Automatic {
                        to_refresh.insert(
                            alias.clone(),
                            AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
                        );
                    }
                    failures.push((alias.clone(), format!("Warmup failed ({alias}): {e}")));
                }
                Err(error) => {
                    if origin == WarmupOrigin::Automatic {
                        to_refresh.insert(
                            alias.clone(),
                            AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
                        );
                    }
                    let detail = crate::task_batch::join_failure_detail(&error);
                    failures.push((
                        alias.clone(),
                        format!("Warmup task stopped ({alias}): {detail}"),
                    ));
                }
            }
        }
        for (alias, plan) in to_refresh {
            if self.shutting_down {
                continue;
            }
            if self.profile_switch_in_flight() {
                self.defer_post_switch_usage_refresh(alias, plan);
                refresh_deferred = true;
                continue;
            }
            if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
                self.fetch_usage_for(idx, plan);
            }
        }

        successes.sort();
        failures.sort_by(|(a, _), (b, _)| a.cmp(b));
        let now = Instant::now();
        let mut slow_aliases = Vec::new();
        let mut newly_slow_task_ids = Vec::new();
        for (task_id, task) in &self.warmup_tasks {
            if now.duration_since(task.started) >= WARMUP_SLOW_NOTICE {
                slow_aliases.push(task.alias.clone());
                if !task.slow_reported {
                    newly_slow_task_ids.push(*task_id);
                }
            }
        }
        slow_aliases.sort();
        for task_id in &newly_slow_task_ids {
            if let Some(task) = self.warmup_tasks.get_mut(task_id) {
                task.slow_reported = true;
            }
        }

        if !failures.is_empty() {
            let message = if failures.len() == 1 {
                failures.pop().expect("one failure was checked above").1
            } else {
                format!(
                    "Warmup failures: {}",
                    failures
                        .into_iter()
                        .map(|(_, message)| message)
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            };
            self.set_status_error(message, 6);
        } else if !slow_aliases.is_empty()
            && (!newly_slow_task_ids.is_empty() || !successes.is_empty())
        {
            self.set_status(
                format!(
                    "Warmup still running after 60s: {}",
                    slow_aliases.join(", ")
                ),
                6,
            );
        } else if !successes.is_empty() {
            let action = if refresh_deferred {
                "usage refresh queued until the profile switch finishes"
            } else {
                "refreshing usage..."
            };
            self.set_status(format!("Warmed up {} — {action}", successes.join(", ")), 4);
        }
    }

    fn start_usage_cache_invalidation(
        &mut self,
        alias: String,
        binding: StrictAccountBinding,
        refresh_after: Option<AccountRefreshPlan>,
        warning_on_failure: Option<String>,
    ) {
        let task_id = self.usage_cache_invalidation_next_id;
        self.usage_cache_invalidation_next_id =
            self.usage_cache_invalidation_next_id.wrapping_add(1);
        let worker_alias = alias.clone();
        let worker_binding = binding.clone();
        let handle = tokio::spawn(async move {
            cache::invalidate_bound_async(&worker_alias, &worker_binding).await
        });
        self.usage_cache_invalidation_tasks.insert(
            task_id,
            UsageCacheInvalidationTask {
                alias,
                binding,
                refresh_after,
                warning_on_failure,
                handle,
            },
        );
    }

    async fn poll_usage_cache_invalidations(&mut self) {
        let finished = self
            .usage_cache_invalidation_tasks
            .iter()
            .filter_map(|(task_id, task)| task.handle.is_finished().then_some(*task_id))
            .collect::<Vec<_>>();
        let mut rebuild_menu = false;
        for task_id in finished {
            let Some(task) = self.usage_cache_invalidation_tasks.remove(&task_id) else {
                continue;
            };
            let failure = match task.handle.await {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(format!("{error:#}")),
                Err(error) => Some(crate::task_batch::join_failure_detail(&error)),
            };
            if self.reset_cards_in_flight.remove(&task.alias) {
                self.mark_render_changed();
            }
            rebuild_menu = true;
            if let Some(detail) = failure {
                tracing::warn!("[{}] usage cache invalidation failed: {detail}", task.alias);
                let message = task.warning_on_failure.unwrap_or_else(|| {
                    format!(
                        "Reset-card use for {} completed, but its cached usage could not be cleared",
                        task.alias
                    )
                });
                self.set_status_error(
                    format!(
                        "{message}; usage cache invalidation failed: {detail}; do not retry until usage is refreshed and card ownership is verified"
                    ),
                    8,
                );
            }
            if self.shutting_down {
                continue;
            }
            if let Some(plan) = task.refresh_after
                && let Some(idx) = self.accounts.iter().position(|account| {
                    account.alias == task.alias
                        && strict_account_identity(&account.info).as_ref() == Some(&task.binding)
                })
            {
                self.fetch_usage_for(idx, plan);
            }
        }
        if rebuild_menu && matches!(self.menu, Some(super::menu::MenuState::Account { .. })) {
            self.rebuild_open_account_menu();
        }
    }

    pub fn poll_reset_card_results(&mut self) {
        while let Ok((alias, binding, result)) = self.pending_reset_cards.try_recv() {
            let identity_matches = self
                .accounts
                .iter()
                .find(|account| account.alias == alias)
                .and_then(|account| strict_account_identity(&account.info))
                .is_some_and(|current| current == binding);
            if !identity_matches {
                if self.reset_cards_in_flight.remove(&alias) {
                    self.mark_render_changed();
                }
                continue;
            }
            match result {
                Ok(consumed) => {
                    self.set_status(
                        format!(
                            "Used reset card for {alias} (was expiring {})",
                            consumed
                                .credit
                                .expires_at
                                .as_deref()
                                .map(format_local_datetime)
                                .unwrap_or_else(|| "no expiry".to_string())
                        ),
                        6,
                    );
                    self.start_usage_cache_invalidation(
                        alias,
                        binding,
                        Some(AccountRefreshPlan::usage_only(Refresh::Forced)),
                        None,
                    );
                }
                Err(e) => {
                    let message = e.message;
                    self.set_status_error(message.clone(), 7);
                    if e.invalidate_cache {
                        self.start_usage_cache_invalidation(alias, binding, None, Some(message));
                    } else if self.reset_cards_in_flight.remove(&alias) {
                        self.mark_render_changed();
                    }
                }
            }
        }
    }

    fn quota_used_percent(&self, idx: usize) -> Option<f64> {
        let entry = &self.accounts[idx];
        let UsageStatus::Loaded(usage) = &entry.usage else {
            return None;
        };
        if crate::usage::usage_availability(usage, &entry.info)
            == crate::usage::UsageAvailability::Unavailable
        {
            return None;
        }
        [
            (usage.primary.as_ref(), crate::usage::WINDOW_5H_SECS),
            (usage.secondary.as_ref(), crate::usage::WINDOW_7D_SECS),
        ]
        .into_iter()
        .find_map(|(window, default_duration_secs)| {
            crate::usage::validated_quota_window(window?, default_duration_secs)
                .map(|(used_percent, _)| used_percent)
        })
    }

    fn status_order(&self, idx: usize) -> u8 {
        match &self.accounts[idx].usage {
            UsageStatus::Error(_) => 0,
            UsageStatus::Loaded(u) if !crate::usage::is_available(u, &self.accounts[idx].info) => 1,
            UsageStatus::Loaded(_) => 2,
            UsageStatus::Loading => 3,
            UsageStatus::Idle => 4,
        }
    }

    fn workspace_request_would_start(
        &self,
        binding: &StrictAccountBinding,
        refresh: WorkspaceRefresh,
    ) -> bool {
        match refresh {
            WorkspaceRefresh::Skip => false,
            WorkspaceRefresh::IfStale => !self.workspace_requests.contains_key(&binding.account_id),
            WorkspaceRefresh::Forced => true,
        }
    }

    fn begin_workspace_request(
        &mut self,
        binding: &StrictAccountBinding,
        refresh: WorkspaceRefresh,
    ) -> Option<u64> {
        if !self.workspace_request_would_start(binding, refresh) {
            return None;
        }
        if let Some(previous_generation) = self.workspace_requests.get(&binding.account_id).copied()
            && let Some(task) = self.workspace_lookup_tasks.get(&previous_generation)
        {
            task.handle.abort();
        }
        self.cancel_waiting_workspace_cache_write(&binding.account_id);
        let generation = self.workspace_next_id;
        self.workspace_next_id = self.workspace_next_id.wrapping_add(1);
        self.workspace_requests
            .insert(binding.account_id.clone(), generation);
        Some(generation)
    }

    fn cancel_waiting_workspace_cache_write(&self, account_id: &str) {
        let Some(task_id) = self.workspace_cache_latest.get(account_id) else {
            return;
        };
        if let Some(task) = self.workspace_cache_writes.get(task_id) {
            task.control.cancel_waiting();
        }
    }

    fn start_workspace_cache_write(
        &mut self,
        account_id: String,
        state: cache::WorkspaceState,
        generation: u64,
    ) {
        if self.shutting_down {
            return;
        }
        self.cancel_waiting_workspace_cache_write(&account_id);
        let task_id = self.workspace_cache_write_next_id;
        self.workspace_cache_write_next_id = self.workspace_cache_write_next_id.wrapping_add(1);
        let control = cache::CacheLockAcquireControl::new();
        let worker_control = control.clone();
        let worker_account_id = account_id.clone();
        let handle = tokio::spawn(async move {
            cache::set_workspace_state_async_cancellable(
                &worker_account_id,
                &state,
                &worker_control,
            )
            .await
        });
        self.workspace_cache_latest
            .insert(account_id.clone(), task_id);
        self.workspace_cache_writes.insert(
            task_id,
            WorkspaceCacheWriteTask {
                account_id,
                generation,
                control,
                handle,
            },
        );
    }

    async fn poll_workspace_cache_writes(&mut self) {
        let finished = self
            .workspace_cache_writes
            .iter()
            .filter_map(|(task_id, task)| task.handle.is_finished().then_some(*task_id))
            .collect::<Vec<_>>();
        for task_id in finished {
            let Some(task) = self.workspace_cache_writes.remove(&task_id) else {
                continue;
            };
            if self.workspace_cache_latest.get(&task.account_id) == Some(&task_id) {
                self.workspace_cache_latest.remove(&task.account_id);
            }
            match task.handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(
                    "[{}] workspace cache generation {} failed: {error:#}",
                    task.account_id,
                    task.generation
                ),
                Err(error) => tracing::warn!(
                    "[{}] workspace cache generation {} worker failed: {error}",
                    task.account_id,
                    task.generation
                ),
            }
        }
    }

    fn spawn_workspace_lookup(
        &mut self,
        alias: String,
        path: std::path::PathBuf,
        binding: StrictAccountBinding,
        generation: u64,
        client: reqwest::Client,
    ) {
        let sender = self.workspace_sender.clone();
        let limiter = self.usage_limiter.clone();
        let account_id = binding.account_id.clone();
        let handle = tokio::spawn(async move {
            let result = async {
                let auth = prepare_workspace_lookup_auth(&alias, &path, &binding).await?;
                let _permit = limiter
                    .acquire()
                    .await
                    .map_err(|_| anyhow::anyhow!("usage limiter closed"))?;
                crate::workspace::lookup_state_for_auth_with_client(&auth, &client).await
            }
            .await
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            let _ = sender.send((alias, binding, generation, result)).await;
        });
        self.workspace_lookup_tasks.insert(
            generation,
            WorkspaceLookupTask {
                account_id,
                generation,
                handle,
            },
        );
    }

    async fn poll_workspace_lookup_tasks(&mut self) {
        let finished = self
            .workspace_lookup_tasks
            .iter()
            .filter_map(|(generation, task)| task.handle.is_finished().then_some(*generation))
            .collect::<Vec<_>>();
        for generation in finished {
            let Some(task) = self.workspace_lookup_tasks.remove(&generation) else {
                continue;
            };
            if let Err(error) = task.handle.await {
                let is_latest = self
                    .workspace_requests
                    .get(&task.account_id)
                    .is_some_and(|active| *active == task.generation);
                if is_latest {
                    self.workspace_requests.remove(&task.account_id);
                    tracing::warn!(
                        "[{}] workspace lookup generation {} stopped: {}",
                        task.account_id,
                        task.generation,
                        crate::task_batch::join_failure_detail(&error)
                    );
                }
            }
        }
    }

    fn start_workspace_refresh_for(&mut self, idx: usize, refresh: WorkspaceRefresh) {
        if !self.credential_operations_ready() || matches!(refresh, WorkspaceRefresh::Skip) {
            return;
        }
        let Some(entry) = self.accounts.get(idx) else {
            return;
        };
        let alias = entry.alias.clone();
        if matches!(refresh, WorkspaceRefresh::IfStale)
            && entry.info.account_id.as_deref().is_some_and(|account_id| {
                self.workspace_states
                    .get(account_id)
                    .is_some_and(|resolution| resolution.is_fresh(Instant::now()))
            })
        {
            return;
        }
        let Some(binding) = strict_account_identity(&entry.info) else {
            return;
        };
        if !self.workspace_request_would_start(&binding, refresh) {
            return;
        }
        let path = match profile_auth_path(&alias) {
            Ok(path) => path,
            Err(error) => {
                self.set_status_error(format!("Path error for {alias}: {error}"), 5);
                return;
            }
        };
        let Some(client) = self.request_client() else {
            return;
        };
        let Some(generation) = self.begin_workspace_request(&binding, refresh) else {
            return;
        };
        self.spawn_workspace_lookup(alias, path, binding, generation, client);
    }

    fn record_usage_lease_release(&mut self, alias: &str, request_id: u64) {
        if self.usage_generations.get(alias) == Some(&request_id) {
            self.usage_lease_release_generations
                .insert(alias.to_string(), request_id);
        }
    }

    /// Resume work merged behind a usage request only after that generation's
    /// profile lease has observably been released. The quota result is sent
    /// earlier so it can be painted promptly, but a workspace lookup must not
    /// read credentials while refresh persistence still owns the alias.
    fn poll_usage_lease_releases(&mut self) {
        while let Ok((alias, request_id)) = self.pending_usage_lease_releases.try_recv() {
            self.record_usage_lease_release(&alias, request_id);
        }

        if self.shutting_down || !self.credential_operations_ready() {
            return;
        }
        let ready = self
            .pending_usage_refreshes
            .keys()
            .filter(|alias| {
                !self.refreshing_requests.contains_key(*alias)
                    && self.usage_generations.get(*alias)
                        == self.usage_lease_release_generations.get(*alias)
            })
            .cloned()
            .collect::<Vec<_>>();
        for alias in ready {
            let Some(plan) = self.pending_usage_refreshes.remove(&alias) else {
                continue;
            };
            let Some(idx) = self
                .accounts
                .iter()
                .position(|account| account.alias == alias)
            else {
                continue;
            };
            self.fetch_usage_for(idx, plan);
        }
    }

    fn fetch_usage_for(&mut self, idx: usize, plan: AccountRefreshPlan) {
        if !self.credential_operations_ready() {
            return;
        }
        let entry = match self.accounts.get(idx) {
            Some(e) => e,
            None => return,
        };
        if self.refreshing_requests.contains_key(&entry.alias) {
            if plan.needs_follow_up() {
                self.pending_usage_refreshes
                    .entry(entry.alias.clone())
                    .and_modify(|queued| {
                        *queued = (*queued).merged_with(plan);
                    })
                    .or_insert(plan);
            }
            return;
        }
        let needs_usage = plan.usage.is_some_and(|refresh| {
            refresh_fetches_loaded_usage(refresh) || !matches!(entry.usage, UsageStatus::Loaded(_))
        });
        let alias = entry.alias.clone();
        let wants_workspace = match plan.workspace {
            WorkspaceRefresh::Skip => false,
            WorkspaceRefresh::IfStale => {
                entry.info.account_id.as_deref().is_some_and(|account_id| {
                    !self
                        .workspace_states
                        .get(account_id)
                        .is_some_and(|resolution| resolution.is_fresh(Instant::now()))
                })
            }
            WorkspaceRefresh::Forced => true,
        };
        if !needs_usage && !wants_workspace {
            return;
        }
        let Some(expected_binding) = strict_account_identity(&entry.info) else {
            if needs_usage {
                self.accounts[idx].usage = UsageStatus::Error(UsageError {
                    summary: "account identity incomplete".to_string(),
                    detail: format!(
                        "[{alias}] usage refresh requires a verified account id and email"
                    ),
                });
                self.mark_render_changed();
                self.update_view();
            }
            return;
        };
        let workspace_would_start = wants_workspace
            && self.workspace_request_would_start(&expected_binding, plan.workspace);
        if !needs_usage && !workspace_would_start {
            return;
        }

        if !needs_usage {
            self.start_workspace_refresh_for(idx, plan.workspace);
            return;
        }
        let refresh = plan
            .usage
            .expect("usage work must carry an explicit refresh policy");

        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let limiter = self.usage_limiter.clone();
        let Some(http_client) = self.request_client() else {
            return;
        };

        if needs_usage && !matches!(self.accounts[idx].usage, UsageStatus::Loaded(_)) {
            self.accounts[idx].usage = UsageStatus::Loading;
        }

        if workspace_would_start {
            let workspace_follow_up = AccountRefreshPlan::workspace_only(plan.workspace);
            self.pending_usage_refreshes
                .entry(alias.clone())
                .and_modify(|queued| {
                    *queued = (*queued).merged_with(workspace_follow_up);
                })
                .or_insert(workspace_follow_up);
        }

        // A previous generation may still be enriching reset metadata after
        // its quota was already displayed. That work is reconstructable and
        // must not delay or overwrite an explicit newer generation.
        self.cancel_usage_followups_for(&alias);

        let usage_tx = self.result_sender.clone();
        let usage_enrichment_tx = self.usage_enrichment_sender.clone();
        let usage_lease_release_tx = self.usage_lease_release_sender.clone();
        let request_id = {
            let request_id = self.usage_next_id;
            self.usage_next_id = self.usage_next_id.wrapping_add(1);
            self.refreshing_requests
                .insert(alias.clone(), (request_id, refresh));
            self.usage_generations.insert(alias.clone(), request_id);
            self.usage_lease_release_generations.remove(&alias);
            self.usage_metadata_requests
                .insert(alias.clone(), request_id);
            request_id
        };
        self.mark_render_changed();
        let tracked_alias = alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let usage_work = crate::usage::UsageTaskCancellation::new();
        let task_usage_work = usage_work.clone();
        let base_cache_control = cache::CacheLockAcquireControl::new();
        let task_base_cache_control = base_cache_control.clone();
        let enrichment_control = cache::CacheLockAcquireControl::new();
        let task_enrichment_control = enrichment_control.clone();
        let task_binding = expected_binding.clone();
        let handle = tokio::spawn(async move {
            let lease = match profile::acquire_profile_lease_async_cancellable(
                alias.clone(),
                &task_lease_control,
            )
            .await
            {
                Ok(Some(lease)) => lease,
                Ok(None) => {
                    if task_usage_work.is_cancelled() {
                        task_usage_work.mark_cancellation_completed();
                    }
                    return;
                }
                Err(error) => {
                    if !task_usage_work.finish() {
                        task_usage_work.mark_cancellation_completed();
                        return;
                    }
                    let result = Err(UsageError {
                        summary: "profile lock failed".to_string(),
                        detail: format!(
                            "[{alias}] could not lock profile for usage refresh: {error:#}"
                        ),
                    });
                    let _ = usage_tx
                        .send((alias.clone(), task_binding.clone(), request_id, result))
                        .await;
                    publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;
                    return;
                }
            };
            // The profile boundary and every local credential/cache decision
            // precede the shared network slot. A locked alias therefore cannot
            // occupy scarce request capacity while another alias is runnable.
            let prepared = crate::usage::prepare_core_usage_with_existing_lease(
                &alias,
                &path,
                refresh,
                &lease,
                &task_binding,
            )
            .await;
            let result = match prepared {
                Ok(prepared) => match prepared.cached_usage().cloned() {
                    Some(usage) => Ok(usage),
                    None => {
                        let first_permit = crate::usage::first_network_permit(limiter.clone());
                        let mut network = crate::usage::NetworkPermitBudget::new(first_permit);
                        crate::usage::execute_prepared_core_usage_cancellable_with_existing_lease_and_client(
                            prepared,
                            &lease,
                            &http_client,
                            &mut network,
                            &task_usage_work,
                        )
                        .await
                    }
                },
                Err(error) => Err(error),
            };
            if !task_usage_work.finish() {
                task_usage_work.mark_cancellation_completed();
                drop(lease);
                publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;
                return;
            }
            let core_usage = result.as_ref().ok().cloned();
            // The core helper does not return until any rotated credential has
            // crossed its persistence boundary, so the quota result and the
            // scarce network permit can be released independently of the
            // reset-card metadata read below.
            let _ = usage_tx
                .send((alias.clone(), task_binding.clone(), request_id, result))
                .await;

            // Keep the profile lease until the post-refresh auth snapshot has
            // been read and identity-checked. This preserves one coherent
            // credential view without delaying quota display or another
            // account's network request.
            let Some(mut usage) = core_usage else {
                drop(lease);
                publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;
                return;
            };
            let auth = match crate::auth::read_auth_async(&path)
                .await
                .map_err(|error| format!("refreshed auth could not be read: {error:#}"))
                .and_then(|auth| {
                    (crate::auth::account_info_from_auth_value(&auth).strict_binding()
                        == Some(task_binding.clone()))
                    .then_some(auth)
                    .ok_or_else(|| "profile identity changed after the quota request".to_string())
                }) {
                Ok(auth) => auth,
                Err(error) => {
                    drop(lease);
                    publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;
                    tracing::debug!("[{alias}] {error}");
                    usage.reset_credits_error = Some("refreshed auth unavailable".to_string());
                    let _ = usage_enrichment_tx
                        .send((alias, task_binding, request_id, usage))
                        .await;
                    return;
                }
            };

            // The quota is already visible and the network permit is already
            // free. Keep the profile lease through this post-refresh,
            // identity-bound durable write so an alias cannot be rebound
            // between verification and cache publication.
            usage = match cache::put_bound_versioned_async_cancellable(
                &alias,
                &task_binding,
                &usage,
                &task_base_cache_control,
            )
            .await
            {
                Ok(Some(versioned)) => versioned,
                Ok(None) => {
                    drop(lease);
                    publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;
                    return;
                }
                Err(error) => {
                    drop(lease);
                    publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;
                    tracing::warn!(
                        "[{alias}] quota is available, but caching the base usage failed; reset-credit enrichment skipped: {error:#}"
                    );
                    let _ = usage_enrichment_tx
                        .send((alias, task_binding, request_id, usage))
                        .await;
                    return;
                }
            };
            drop(lease);
            publish_usage_lease_release(&usage_lease_release_tx, &alias, request_id).await;

            let reset_permit = tokio::select! {
                permit = limiter.acquire() => permit.ok(),
                _ = task_enrichment_control.cancelled() => None,
            };
            let Some(_reset_permit) = reset_permit else {
                return;
            };
            tokio::select! {
                _ = task_enrichment_control.cancelled() => return,
                _ = crate::usage::enrich_reset_credits_for_auth_with_client(
                    &alias,
                    &auth,
                    &mut usage,
                    &http_client,
                ) => {}
            }
            drop(_reset_permit);
            match cache::merge_reset_credit_enrichment_bound_async_cancellable(
                &alias,
                &task_binding,
                &usage,
                &task_enrichment_control,
            )
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!("[{alias}] reset-credit cache enrichment failed: {error:#}");
                }
            }
            let _ = usage_enrichment_tx
                .send((alias, task_binding, request_id, usage))
                .await;
        });
        self.track_account_task_with_controls(
            tracked_alias,
            AccountTaskKind::Usage { request_id },
            lease_control,
            AccountTaskControls {
                followup_controls: vec![base_cache_control, enrichment_control],
                network_wait: None,
                read_only_work: None,
                usage_work: Some(usage_work),
            },
            handle,
        );
    }

    fn refresh_indices(&mut self, target_indices: &[usize], refresh: Refresh) {
        if matches!(refresh, Refresh::Cached) {
            let bindings = target_indices
                .iter()
                .filter_map(|&i| {
                    self.accounts.get(i).and_then(|entry| {
                        strict_account_identity(&entry.info)
                            .map(|binding| (entry.alias.clone(), binding))
                    })
                })
                .collect::<HashMap<_, _>>();
            if !bindings.is_empty() {
                let cached = match crate::cache::get_many_bound(&bindings) {
                    Ok(cached) => cached,
                    Err(error) => {
                        self.set_status_error(format!("Could not read usage cache: {error:#}"), 6);
                        return;
                    }
                };
                self.apply_cached_usage(cached, CachedUsageApplication::RequestedRefresh, None);
            }
        }

        self.start_refresh_indices(target_indices, refresh);
    }

    fn apply_cached_usage(
        &mut self,
        mut cached: HashMap<String, UsageInfo>,
        application: CachedUsageApplication,
        startup_identities: Option<&HashMap<String, StrictAccountBinding>>,
    ) {
        let mut changed = false;
        for entry in &mut self.accounts {
            let identity_matches = match application {
                CachedUsageApplication::RequestedRefresh => true,
                CachedUsageApplication::Startup => strict_account_identity(&entry.info)
                    .and_then(|identity| {
                        startup_identities
                            .and_then(|identities| identities.get(&entry.alias))
                            .map(|expected| expected == &identity)
                    })
                    .unwrap_or(false),
            };
            let can_apply = matches!(entry.usage, UsageStatus::Idle | UsageStatus::Loading)
                || matches!(application, CachedUsageApplication::RequestedRefresh)
                    && matches!(entry.usage, UsageStatus::Error(_));
            if identity_matches
                && can_apply
                && let Some(usage) = cached.remove(&entry.alias)
            {
                entry.usage = UsageStatus::Loaded(Box::new(usage));
                changed = true;
            }
        }
        if changed {
            self.mark_render_changed();
            self.update_view();
        }
    }

    fn apply_startup_cache_snapshot(
        &mut self,
        snapshot: cache::CacheSnapshot,
        startup_identities: &HashMap<String, StrictAccountBinding>,
    ) {
        let cache::CacheSnapshot {
            usage,
            workspaces,
            workspace_fresh_for,
        } = snapshot;
        let snapshot_applied_at = Instant::now();
        let mut workspace_changed = false;
        for entry in &mut self.accounts {
            let Some(identity) = strict_account_identity(&entry.info) else {
                continue;
            };
            if startup_identities.get(&entry.alias) != Some(&identity) {
                continue;
            }
            if let Some(state) = workspaces.get(&identity.account_id)
                && let Some(fresh_for) = workspace_fresh_for.get(&identity.account_id)
                && !fresh_for.is_zero()
            {
                self.workspace_states.insert(
                    identity.account_id.clone(),
                    WorkspaceMemoryResolution {
                        state: state.clone(),
                        fresh_until: snapshot_applied_at + *fresh_for,
                    },
                );
                let previous_workspace = entry.info.workspace_name.clone();
                cache::apply_workspace_state(&mut entry.info, state);
                workspace_changed |= entry.info.workspace_name != previous_workspace;
            }
        }
        self.recompute_workspace_expiry_deadline();
        self.apply_cached_usage(
            usage,
            CachedUsageApplication::Startup,
            Some(startup_identities),
        );
        if workspace_changed {
            self.mark_render_changed();
        }
        self.update_view();
        if matches!(self.menu, Some(super::menu::MenuState::Account { .. })) {
            self.rebuild_open_account_menu();
        }
    }

    fn start_refresh_indices(&mut self, target_indices: &[usize], refresh: Refresh) {
        for &i in target_indices {
            let Some(entry) = self.accounts.get_mut(i) else {
                continue;
            };
            if let UsageStatus::Error(_) = &entry.usage {
                entry.usage = UsageStatus::Idle;
            }
        }
        for &i in target_indices {
            self.fetch_usage_for(i, AccountRefreshPlan::usage_and_workspace(refresh));
        }
        self.update_view();
    }

    /// Keep display order immutable while scheduling the row that matters most
    /// to the user first. At ordinary startup the selected and active rows are
    /// the same; retaining both priorities also makes a selection made while
    /// profile loading was in flight deterministic.
    fn startup_request_order(&self, target_indices: &[usize]) -> Vec<usize> {
        let selected = self.selected_account_idx();
        let current = target_indices.iter().copied().find(|&idx| {
            self.accounts
                .get(idx)
                .is_some_and(|account| account.is_current)
        });
        let mut ordered = Vec::with_capacity(target_indices.len());
        let targets = target_indices.iter().copied().collect::<BTreeSet<_>>();
        let mut included = BTreeSet::new();
        for priority in [selected, current].into_iter().flatten() {
            if targets.contains(&priority) && included.insert(priority) {
                ordered.push(priority);
            }
        }
        for &idx in target_indices {
            if included.insert(idx) {
                ordered.push(idx);
            }
        }
        ordered
    }

    /// Continue startup after one identity-checked cache snapshot. Register
    /// every core quota request first, then attach workspace intent through the
    /// ordinary alias-local follow-up path. A ready alias can therefore resolve
    /// its workspace without waiting for an unrelated slow account, while its
    /// own credential persistence and lease release remain mandatory.
    ///
    /// A missing value is already a proven cache miss. Sending that account
    /// through `Refresh::Cached` would read the alias cache again and could
    /// reattach a value that the startup identity check deliberately rejected.
    fn start_startup_refresh_indices(&mut self, target_indices: &[usize]) -> Vec<String> {
        let ordered = self.startup_request_order(target_indices);
        let requests = ordered
            .into_iter()
            .filter_map(|idx| {
                self.accounts.get(idx).map(|account| {
                    let refresh = if matches!(account.usage, UsageStatus::Loaded(_)) {
                        Refresh::Cached
                    } else {
                        Refresh::Unattended
                    };
                    (idx, account.alias.clone(), refresh)
                })
            })
            .collect::<Vec<_>>();

        // Spawn all quota work before any auxiliary lookup can enter the shared
        // network budget. The second pass either starts workspace immediately
        // for a cache hit or merges it behind that alias's active generation.
        for &(idx, _, refresh) in &requests {
            self.fetch_usage_for(idx, AccountRefreshPlan::usage_only(refresh));
        }
        for &(idx, _, refresh) in &requests {
            let workspace = AccountRefreshPlan::usage_and_workspace(refresh).workspace;
            self.fetch_usage_for(idx, AccountRefreshPlan::workspace_only(workspace));
        }
        let aliases = requests.into_iter().map(|(_, alias, _)| alias).collect();
        self.update_view();
        aliases
    }

    fn startup_core_refreshes_settled(&self, aliases: &[String]) -> bool {
        aliases
            .iter()
            .all(|alias| self.latest_core_usage_released_for(alias))
    }

    /// Refresh usage for all visible accounts (search-filtered view).
    /// Batch refresh of just the marked accounts is exposed separately
    /// via the Enter > Batch menu so the implicit "marks change scope"
    /// behavior is gone.
    pub fn refresh(&mut self, refresh: Refresh) {
        if self.reject_new_credential_operation() {
            return;
        }
        let target_indices: Vec<usize> = self.view_indices.clone();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn refresh_all(&mut self, refresh: Refresh) {
        if self.reject_new_credential_operation() {
            return;
        }
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn poll_results(&mut self) {
        let mut changed = false;
        let open_account_alias = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { info, .. }) => Some(info.alias.clone()),
            _ => None,
        };
        let mut refresh_open_account = false;
        while let Ok((alias, binding, request_id, result)) = self.pending_results.try_recv() {
            let is_current_request = self
                .refreshing_requests
                .get(&alias)
                .is_some_and(|(active_id, _)| *active_id == request_id);
            if !is_current_request {
                continue;
            }
            self.refreshing_requests.remove(&alias);
            changed = true;
            let Some(idx) = self.accounts.iter().position(|entry| entry.alias == alias) else {
                if self
                    .usage_metadata_requests
                    .get(&alias)
                    .is_some_and(|active_id| *active_id == request_id)
                {
                    self.usage_metadata_requests.remove(&alias);
                }
                continue;
            };
            if strict_account_identity(&self.accounts[idx].info).as_ref() != Some(&binding) {
                self.usage_metadata_requests.remove(&alias);
                self.pending_usage_refreshes.remove(&alias);
                if matches!(self.accounts[idx].usage, UsageStatus::Loading) {
                    self.accounts[idx].usage = UsageStatus::Idle;
                    changed = true;
                }
                continue;
            }
            let core_succeeded = result.is_ok();
            self.accounts[idx].usage = match result {
                Ok(u) => UsageStatus::Loaded(Box::new(u)),
                Err(e) => UsageStatus::Error(e),
            };
            refresh_open_account |= open_account_alias.as_deref() == Some(alias.as_str());
            changed = true;
            if !core_succeeded {
                self.usage_metadata_requests.remove(&alias);
            }
        }
        while let Ok((alias, binding, request_id, usage)) = self.pending_usage_enrichment.try_recv()
        {
            let is_latest_generation = self
                .usage_generations
                .get(&alias)
                .is_some_and(|active_id| *active_id == request_id);
            let is_current_metadata = self
                .usage_metadata_requests
                .get(&alias)
                .is_some_and(|active_id| *active_id == request_id);
            if is_current_metadata {
                self.usage_metadata_requests.remove(&alias);
            }
            let Some(entry) = self.accounts.iter_mut().find(|entry| entry.alias == alias) else {
                continue;
            };
            if !is_latest_generation || !is_current_metadata {
                continue;
            }
            if strict_account_identity(&entry.info).as_ref() != Some(&binding) {
                self.pending_usage_refreshes.remove(&alias);
                continue;
            }
            if matches!(entry.usage, UsageStatus::Loaded(_)) {
                entry.usage = UsageStatus::Loaded(Box::new(usage));
                refresh_open_account |= open_account_alias.as_deref() == Some(alias.as_str());
                changed = true;
            }
        }
        while let Ok((alias, binding, generation, result)) = self.pending_workspace.try_recv() {
            let is_latest_generation = self
                .workspace_requests
                .get(&binding.account_id)
                .is_some_and(|active_generation| *active_generation == generation);
            if !is_latest_generation {
                continue;
            }
            self.workspace_requests.remove(&binding.account_id);
            let owner_is_current = self.accounts.iter().any(|entry| {
                entry.alias == alias
                    && strict_account_identity(&entry.info).as_ref() == Some(&binding)
            });
            if !owner_is_current {
                continue;
            }
            let state = match result {
                Ok(state) => state,
                Err(error) => {
                    tracing::debug!("[{alias}] workspace metadata unavailable: {error}");
                    continue;
                }
            };
            if matches!(state, cache::WorkspaceState::Unresolved) {
                continue;
            }
            self.start_workspace_cache_write(binding.account_id.clone(), state.clone(), generation);
            self.workspace_states.insert(
                binding.account_id.clone(),
                WorkspaceMemoryResolution {
                    state: state.clone(),
                    fresh_until: Instant::now() + cache::workspace_resolution_ttl(),
                },
            );
            self.recompute_workspace_expiry_deadline();
            // Workspace resolution is keyed by account_id in both the disk
            // cache and memory. Apply one verified response to every complete
            // row for that account so duplicate aliases stay consistent.
            for entry in &mut self.accounts {
                if strict_account_identity(&entry.info)
                    .is_some_and(|identity| identity.account_id == binding.account_id)
                {
                    let previous_workspace = entry.info.workspace_name.clone();
                    crate::cache::apply_workspace_state(&mut entry.info, &state);
                    if entry.info.workspace_name != previous_workspace {
                        refresh_open_account |=
                            open_account_alias.as_deref() == Some(entry.alias.as_str());
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.mark_render_changed();
            self.update_view();
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
    }

    pub fn switch_selected(&mut self) {
        let Some(entry) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
        else {
            return;
        };
        let alias = entry.alias.clone();
        let is_current = entry.is_current;
        if self.interactive_operation_in_flight() {
            self.set_status(
                "Finish the active confirmation or profile switch before switching again"
                    .to_string(),
                5,
            );
            return;
        }
        if self.reject_credential_recovery_during_transition() {
            return;
        }
        if is_current {
            self.set_status(format!("Already using {alias}"), 3);
            return;
        }
        if self.reset_card_in_flight(&alias) {
            self.set_status(
                format!("{alias}: finish the reset-card operation before switching"),
                5,
            );
            return;
        }
        if let Some(refresh) = self.pending_usage_refreshes.get(&alias).copied() {
            self.defer_post_switch_usage_refresh(alias.clone(), refresh);
        }
        if let Some(refresh) = self.cancel_waiting_background_credential_work_for(&alias) {
            self.defer_post_switch_usage_refresh(
                alias.clone(),
                AccountRefreshPlan::resume_cancelled_usage(refresh),
            );
        }
        self.start_profile_switch_prepare(alias, super::switch::PreparePass::Initial);
    }

    fn start_profile_switch_prepare(&mut self, alias: String, pass: super::switch::PreparePass) {
        if self.shutting_down {
            return;
        }
        let tx = self.profile_switch_sender.clone();
        let tracked_alias = alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            if let Some(result) = super::switch::prepare(alias, pass, task_lease_control).await {
                let _ = tx.send(result).await;
            }
        });
        self.track_account_task(
            tracked_alias.clone(),
            AccountTaskKind::SwitchPrepare,
            lease_control,
            handle,
        );
    }

    fn start_live_auth_sync_before_switch(&mut self, target_alias: String) {
        if self.shutting_down {
            return;
        }
        let tx = self.profile_switch_sender.clone();
        let tracked_alias = target_alias.clone();
        let background_cancellations = self.background_credential_cancellations();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let result = super::switch::synchronize_live(
                target_alias,
                task_lease_control,
                background_cancellations,
            )
            .await;
            let _ = tx.send(result).await;
        });
        self.track_account_task(
            tracked_alias.clone(),
            AccountTaskKind::SwitchSync,
            lease_control,
            handle,
        );
    }

    fn start_profile_switch_commit(&mut self, confirmed: profile::ConfirmedProfileSwitch) {
        let alias = confirmed.alias().to_string();
        if self.shutting_down {
            return;
        }
        let tx = self.profile_switch_sender.clone();
        let tracked_alias = alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            if let Some(result) = super::switch::commit(confirmed, task_lease_control).await {
                let _ = tx.send(result).await;
            }
        });
        self.track_account_task(
            tracked_alias.clone(),
            AccountTaskKind::SwitchCommit,
            lease_control,
            handle,
        );
    }

    pub fn poll_profile_switch_results(&mut self) {
        while let Ok((alias, result)) = self.pending_profile_switches.try_recv() {
            result.record_timing(&alias);
            match result {
                super::switch::TaskResult::Prepared { .. } if self.shutting_down => {}
                super::switch::TaskResult::LiveSynchronized { .. } if self.shutting_down => {}
                super::switch::TaskResult::Prepared {
                    result: Ok(prepared),
                    pass: super::switch::PreparePass::Initial,
                    ..
                } if prepared.needs_live_sync() => {
                    self.start_live_auth_sync_before_switch(alias);
                }
                super::switch::TaskResult::Prepared {
                    result: Ok(prepared),
                    pass,
                    ..
                } => match profile::confirm_prepared_profile_switch_without_overwrite(prepared) {
                    Ok(confirmed) => self.start_profile_switch_commit(confirmed),
                    Err(error) => {
                        let detail = if pass == super::switch::PreparePass::AfterLiveSync {
                            format!(
                                "live authentication changed again after its saved profile was synchronized: {error:#}"
                            )
                        } else {
                            format!("{error:#}")
                        };
                        self.set_status_error(format!("Switch failed: {detail}"), 5);
                    }
                },
                super::switch::TaskResult::Prepared {
                    result: Err(error), ..
                } => {
                    self.set_status_error(format!("Switch failed: {error:#}"), 5);
                }
                super::switch::TaskResult::LiveSynchronized { result: Ok(()), .. } => self
                    .start_profile_switch_prepare(alias, super::switch::PreparePass::AfterLiveSync),
                super::switch::TaskResult::LiveSynchronized {
                    result: Err(error), ..
                } => self.set_status_error(format!("Switch failed: {error:#}"), 5),
                super::switch::TaskResult::Committed { result, .. } => {
                    self.finish_profile_switch(alias, result);
                }
            }
        }
        self.resume_deferred_post_switch_usage_refreshes();
    }

    fn resume_deferred_post_switch_usage_refreshes(&mut self) {
        if self.shutting_down || self.profile_switch_in_flight() {
            return;
        }
        let requests = std::mem::take(&mut self.deferred_post_switch_usage_refreshes);
        for (alias, refresh) in requests {
            if let Some(idx) = self
                .accounts
                .iter()
                .position(|account| account.alias == alias)
            {
                self.fetch_usage_for(idx, refresh);
            }
        }
    }

    fn finish_profile_switch(
        &mut self,
        alias: String,
        result: Result<profile::ProfileSwitchOutcome>,
    ) {
        match result {
            Ok(outcome) => {
                self.startup_auth_state = StartupAuthState::Ready;
                for account in &mut self.accounts {
                    account.is_current = account.alias == alias;
                }
                if let Some(warning) = outcome.selection_history_warning() {
                    self.set_status(
                        format!(
                            "Switched to {alias}; warning: selection history was not updated: {warning:#}"
                        ),
                        8,
                    );
                } else {
                    self.set_status(format!("Switched to {alias}"), 3);
                }
            }
            Err(error) => {
                let reconciliation = self.reconcile_displayed_current_after_switch_error(&error);
                let mut message = format!("Switch failed: {error:#}");
                if let Err(reconcile_error) = reconciliation {
                    for account in &mut self.accounts {
                        account.is_current = false;
                    }
                    message.push_str(&format!(
                        "; active account could not be verified: {reconcile_error:#}"
                    ));
                }
                self.set_status_error(message, 5);
            }
        }
    }

    fn reconcile_displayed_current_after_switch_error(
        &mut self,
        error: &anyhow::Error,
    ) -> Result<Option<String>> {
        let live_path = auth::codex_auth_path()?;
        let active = if let Some(partial) =
            error.downcast_ref::<profile::PartialProfileActivation>()
            && profile::partial_activation_is_currently_bound_checked(partial)?
        {
            Some(partial.alias().to_string())
        } else {
            profile::find_matching_profile_checked(&live_path)?
        };
        for account in &mut self.accounts {
            account.is_current = active.as_deref() == Some(account.alias.as_str());
        }
        Ok(active)
    }

    fn start_profile_mutation(&mut self, kind: ProfileMutationKind) {
        if self.profile_mutation_task.is_some() || self.shutting_down {
            return;
        }
        let worker_kind = kind.clone();
        let handle = tokio::task::spawn_blocking(move || match worker_kind {
            ProfileMutationKind::Delete { alias } => {
                let result = cmd_delete(&alias);
                let reload = load_profile_reload_snapshot(result.is_err());
                ProfileMutationOutput::Delete(ProfileMutationCompletion { result, reload })
            }
            ProfileMutationKind::BatchDelete { aliases } => {
                let mut report = BatchDeleteReport::default();
                for alias in aliases {
                    report.record(&alias, cmd_delete(&alias));
                }
                let reload = load_profile_reload_snapshot(!report.failures.is_empty());
                ProfileMutationOutput::BatchDelete(ProfileMutationCompletion {
                    result: report,
                    reload,
                })
            }
            ProfileMutationKind::Rename { old, new } => {
                let result = rename_profile(&old, &new);
                let reload = load_profile_reload_snapshot(result.is_err());
                ProfileMutationOutput::Rename(ProfileMutationCompletion { result, reload })
            }
        });
        let status = match &kind {
            ProfileMutationKind::Delete { alias } => format!("Deleting {alias}..."),
            ProfileMutationKind::BatchDelete { aliases } => {
                format!("Deleting {} account(s)...", aliases.len())
            }
            ProfileMutationKind::Rename { old, new } => format!("Renaming {old} -> {new}..."),
        };
        self.profile_mutation_task = Some(ProfileMutationTask { kind, handle });
        self.set_status(status, 60);
    }

    async fn poll_profile_mutation(&mut self) {
        if !self
            .profile_mutation_task
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            return;
        }
        let task = self
            .profile_mutation_task
            .take()
            .expect("finished profile mutation task must remain tracked");
        let output = match task.handle.await {
            Ok(output) => output,
            Err(error) => {
                let detail = crate::task_batch::join_failure_detail(&error);
                let reload_error = || {
                    Err(anyhow::anyhow!(
                        "profile state could not be reloaded because its change task stopped: {detail}"
                    ))
                };
                match task.kind.clone() {
                    ProfileMutationKind::Delete { .. } => {
                        ProfileMutationOutput::Delete(ProfileMutationCompletion {
                            result: Err(anyhow::anyhow!("profile change task stopped: {detail}")),
                            reload: reload_error(),
                        })
                    }
                    ProfileMutationKind::BatchDelete { .. } => {
                        let mut report = BatchDeleteReport::default();
                        report
                            .failures
                            .push(format!("profile change task stopped: {detail}"));
                        ProfileMutationOutput::BatchDelete(ProfileMutationCompletion {
                            result: report,
                            reload: reload_error(),
                        })
                    }
                    ProfileMutationKind::Rename { .. } => {
                        ProfileMutationOutput::Rename(ProfileMutationCompletion {
                            result: Err(anyhow::anyhow!("profile change task stopped: {detail}")),
                            reload: reload_error(),
                        })
                    }
                }
            }
        };
        match (task.kind, output) {
            (ProfileMutationKind::Delete { alias }, ProfileMutationOutput::Delete(completion)) => {
                self.reconcile_delete_result(&alias, completion.result, completion.reload)
            }
            (
                ProfileMutationKind::BatchDelete { .. },
                ProfileMutationOutput::BatchDelete(completion),
            ) => self.reconcile_batch_delete_report(completion.result, completion.reload),
            (
                ProfileMutationKind::Rename { old, new },
                ProfileMutationOutput::Rename(completion),
            ) => self.reconcile_rename_result(&old, &new, completion.result, completion.reload),
            _ => unreachable!("profile mutation result must match its tracked kind"),
        }
    }

    pub fn confirm_action(&mut self) {
        let action = match self.confirm.take() {
            Some(a) => a,
            None => return,
        };
        match action {
            ConfirmAction::Delete(alias) => {
                if self.account_operation_in_flight(&alias) {
                    self.set_status(
                        format!(
                            "{alias}: wait for the account operation to finish before deleting"
                        ),
                        5,
                    );
                    return;
                }
                self.start_profile_mutation(ProfileMutationKind::Delete { alias });
            }
            ConfirmAction::BatchDelete(aliases) => {
                if let Some(alias) = aliases
                    .iter()
                    .find(|alias| self.account_operation_in_flight(alias))
                {
                    self.set_status(
                        format!(
                            "{alias}: wait for the account operation to finish before deleting"
                        ),
                        5,
                    );
                    return;
                }
                self.start_profile_mutation(ProfileMutationKind::BatchDelete { aliases });
            }
            ConfirmAction::ConsumeResetCard { alias, credit, .. } => {
                self.consume_reset_card(&alias, credit);
            }
        }
    }

    fn reconcile_delete_result(
        &mut self,
        alias: &str,
        delete_result: Result<profile::ProfileMutationOutcome>,
        reload_result: Result<ProfileReloadSnapshot>,
    ) {
        let reload_result = reload_result.map(|snapshot| {
            self.apply_loaded_profiles(snapshot.current, snapshot.accounts, &BTreeSet::new());
        });
        let visibly_deleted =
            reload_result.is_ok() && self.accounts.iter().all(|entry| entry.alias != alias);
        match (delete_result, reload_result) {
            (Ok(outcome), Ok(())) if visibly_deleted => {
                if let Some(warning) = outcome.durability_warning() {
                    self.set_status(
                        format!(
                            "Deleted {alias} (recoverable), but durability could not be confirmed: {warning:#}"
                        ),
                        8,
                    );
                } else {
                    self.set_status(format!("Deleted {alias} (recoverable)"), 3);
                }
            }
            (Ok(_), Ok(())) => self.set_status_error(
                format!(
                    "Delete reported a committed archive, but the reloaded profile list still contains {alias}"
                ),
                6,
            ),
            (Ok(outcome), Err(reload)) => self.set_status_error(
                format!(
                    "Deleted {alias}, but the profile list could not be reloaded: {reload:#}{}",
                    outcome
                        .durability_warning()
                        .map(|warning| format!("; durability warning: {warning:#}"))
                        .unwrap_or_default()
                ),
                6,
            ),
            (Err(error), Ok(())) => {
                self.set_status_error(format!("Delete failed: {error:#}"), 5)
            }
            (Err(error), Err(reload)) => self.set_status_error(
                format!(
                    "Delete failed ({error:#}); the profile list also could not be reloaded: {reload:#}"
                ),
                7,
            ),
        }
    }

    fn reconcile_batch_delete_report(
        &mut self,
        report: BatchDeleteReport,
        reload_result: Result<ProfileReloadSnapshot>,
    ) {
        self.marked.clear();
        let reload_result = reload_result.map(|snapshot| {
            self.apply_loaded_profiles(snapshot.current, snapshot.accounts, &BTreeSet::new());
        });
        let mut message = report.message();
        if let Err(error) = &reload_result {
            message.push_str(&format!("; profile list reload failed: {error:#}"));
        }
        if report.failures.is_empty() && reload_result.is_ok() {
            self.set_status(message, 6);
        } else {
            self.set_status_error(message, 6);
        }
    }

    fn migrate_renamed_alias_state(&mut self, old: &str, new: &str) {
        if let Some(account) = self
            .accounts
            .iter_mut()
            .find(|account| account.alias == old)
        {
            if let Some(binding) = strict_account_identity(&account.info) {
                self.workspace_requests.remove(&binding.account_id);
            }
            account.alias = new.to_string();
        }
        if self.marked.remove(old) {
            self.marked.insert(new.to_string());
        }
        if let Some(model) = self.model_cache.remove(old) {
            self.model_cache.insert(new.to_string(), model);
        }
    }

    fn reconcile_rename_result(
        &mut self,
        old: &str,
        new: &str,
        rename_result: Result<profile::ProfileMutationOutcome>,
        reload_result: Result<ProfileReloadSnapshot>,
    ) {
        if rename_result.is_ok() {
            self.migrate_renamed_alias_state(old, new);
        }
        let reload_result = reload_result.map(|snapshot| {
            self.apply_loaded_profiles(snapshot.current, snapshot.accounts, &BTreeSet::new());
        });
        let visibly_renamed = reload_result.is_ok()
            && self.accounts.iter().all(|entry| entry.alias != old)
            && self.accounts.iter().any(|entry| entry.alias == new);
        if reload_result.is_ok()
            && let Some(account_idx) = self.accounts.iter().position(|entry| entry.alias == new)
            && let Some(view_idx) = self
                .view_indices
                .iter()
                .position(|&index| index == account_idx)
        {
            self.selected = view_idx;
        }
        match (rename_result, reload_result) {
            (Ok(outcome), Ok(())) if visibly_renamed => {
                if let Some(warning) = outcome.durability_warning() {
                    self.set_status(
                        format!(
                            "Renamed {old} -> {new}, but durability could not be confirmed: {warning:#}"
                        ),
                        8,
                    );
                } else {
                    self.set_status(format!("Renamed {old} -> {new}"), 3);
                }
            }
            (Ok(_), Ok(())) => self.set_status_error(
                format!(
                    "Rename reported success, but the reloaded profile list does not contain {new}"
                ),
                6,
            ),
            (Ok(outcome), Err(reload)) => self.set_status_error(
                format!(
                    "Renamed {old} -> {new}, but the profile list could not be reloaded: {reload:#}{}",
                    outcome
                        .durability_warning()
                        .map(|warning| format!("; durability warning: {warning:#}"))
                        .unwrap_or_default()
                ),
                6,
            ),
            (Err(error), Ok(())) => {
                self.set_status_error(format!("Rename failed: {error:#}"), 5)
            }
            (Err(error), Err(reload)) => self.set_status_error(
                format!(
                    "Rename failed ({error:#}); the profile list also could not be reloaded: {reload:#}"
                ),
                7,
            ),
        }
    }

    fn consume_reset_card(&mut self, alias: &str, credit: ResetCredit) {
        if self.reject_new_credential_operation() {
            return;
        }
        if !self.reset_cards_in_flight.insert(alias.to_string()) {
            self.set_status(format!("{alias}: reset card use is already in progress"), 4);
            return;
        }
        let Some(expected_binding) = self
            .accounts
            .iter()
            .find(|account| account.alias == alias)
            .and_then(|account| strict_account_identity(&account.info))
        else {
            self.reset_cards_in_flight.remove(alias);
            self.set_status_error(
                format!("Cannot use a reset card for {alias}: account identity is incomplete"),
                5,
            );
            return;
        };
        let path = match profile_auth_path(alias) {
            Ok(p) => p,
            Err(e) => {
                self.reset_cards_in_flight.remove(alias);
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let Some(http_client) = self.request_client() else {
            self.reset_cards_in_flight.remove(alias);
            return;
        };
        let limiter = self.usage_limiter.clone();
        let alias_owned = alias.to_string();
        let tracked_alias = alias_owned.clone();
        let tx = self.reset_card_sender.clone();
        self.set_status(format!("Using reset card for {alias}..."), 6);
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let task_binding = expected_binding;
        let handle = tokio::spawn(async move {
            let lease = match profile::acquire_profile_lease_async_cancellable(
                alias_owned.clone(),
                &task_lease_control,
            )
            .await
            {
                Ok(Some(lease)) => lease,
                Ok(None) => return,
                Err(error) => {
                    let failure = reset_card_failure_from_outcome(
                        false,
                        String::new(),
                        format!(
                            "Reset card failed ({alias_owned}): profile lock failed: {error:#}"
                        ),
                    );
                    let _ = tx.send((alias_owned, task_binding, Err(failure))).await;
                    return;
                }
            };
            // Resolve credentials, identity, endpoint policy, and any standing
            // auth verdict before reserving the shared network slot. Execution
            // releases that slot before its independent cache publication.
            let mut network =
                crate::usage::NetworkPermitBudget::new(crate::usage::first_network_permit(limiter));
            let preflight = match crate::usage::prepare_full_usage_with_existing_lease(
                &alias_owned,
                &path,
                Refresh::Forced,
                &lease,
                Some(&task_binding),
            )
            .await
            {
                Ok(prepared) => {
                    crate::usage::execute_prepared_full_usage_with_existing_lease_and_client(
                        prepared,
                        &lease,
                        &http_client,
                        &mut network,
                    )
                    .await
                    .map(|observation| observation.usage)
                }
                Err(error) => Err(error),
            };
            let result = match preflight {
                Ok(preflight) => {
                    match crate::usage::validate_reset_credit_preflight(
                        &alias_owned,
                        &preflight,
                        &credit,
                    ) {
                        Ok(()) => {
                            match crate::usage::prepare_reset_credit_consume_with_existing_lease(
                                &alias_owned,
                                &path,
                                credit,
                                &lease,
                            )
                                .await
                            {
                                Ok(prepared) => {
                                    crate::usage::execute_prepared_reset_credit_consume_with_existing_lease_and_client(
                                        prepared,
                                        &lease,
                                        &http_client,
                                        &mut network,
                                    )
                                    .await
                                    .map_err(|error| {
                                        reset_card_failure_from_consume_error(
                                            &alias_owned,
                                            error,
                                        )
                                    })
                                }
                                Err(error) => Err(reset_card_failure_from_consume_error(
                                    &alias_owned,
                                    error,
                                )),
                            }
                        }
                        Err(error) => Err(map_reset_card_failure(
                            format!("Reset card blocked ({alias_owned}): {error:#}"),
                            false,
                        )),
                    }
                }
                Err(error) => Err(map_reset_card_failure(
                    format!(
                        "Reset card preflight failed ({alias_owned}): {}; no reset card was requested",
                        error.detail
                    ),
                    false,
                )),
            };
            let _ = tx.send((alias_owned, task_binding, result)).await;
        });
        self.track_account_task(
            tracked_alias,
            AccountTaskKind::ResetCard,
            lease_control,
            handle,
        );
    }

    pub fn request_batch_delete(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        if self.interactive_operation_in_flight() {
            self.set_status(
                "Finish the active confirmation or profile switch before deleting".to_string(),
                5,
            );
            return;
        }
        if self.reject_new_credential_operation() {
            return;
        }
        if let Some(alias) = self
            .marked
            .iter()
            .find(|alias| self.account_operation_in_flight(alias))
            .cloned()
        {
            self.set_status(
                format!("{alias}: wait for the account operation to finish before deleting"),
                5,
            );
            return;
        }
        let aliases: Vec<String> = self.marked.iter().cloned().collect();
        self.confirm = Some(ConfirmAction::BatchDelete(aliases));
    }

    /// Refresh all marked accounts (force).
    pub fn refresh_marked(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        if self.reject_new_credential_operation() {
            return;
        }
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| self.marked.contains(&a.alias))
            .map(|(i, _)| i)
            .collect();
        let count = target_indices.len();
        self.refresh_indices(&target_indices, Refresh::Forced);
        self.set_status(format!("Refreshing {count} marked account(s)..."), 3);
    }

    /// Warmup all marked accounts (skipping already-active / in-flight / errored).
    pub fn warmup_marked(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        if self.reject_new_credential_operation() {
            return;
        }
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| self.marked.contains(&a.alias))
            .map(|(i, _)| i)
            .collect();
        self.start_warmup_preflight(target_indices, WarmupPreflightOrigin::Marked);
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn handle_rename_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc => {
                self.rename = None;
                return false;
            }
            KeyCode::Enter => {
                let Some(state) = self.rename.as_ref() else {
                    return false;
                };
                let old = state.old_alias.clone();
                let new = state.input.trim().to_string();
                if new.is_empty() || new == old {
                    self.rename = None;
                    return false;
                }
                if let Err(err) = validate_alias(&new) {
                    self.rename = None;
                    self.set_status_error(format!("Invalid alias: {err}"), 3);
                    return false;
                }
                // Auto refresh may have started while the editor was open.
                // Keep the user's input intact and let them retry once the
                // operation completes rather than blocking the event loop on
                // the profile lease.
                if let Some(alias) = [&old, &new]
                    .into_iter()
                    .find(|alias| self.account_operation_in_flight(alias))
                {
                    self.set_status(
                        format!(
                            "{alias}: wait for the account operation to finish before renaming"
                        ),
                        5,
                    );
                    return true;
                }
                self.rename = None;
                self.start_profile_mutation(ProfileMutationKind::Rename { old, new });
                return false;
            }
            _ => {
                let Some(state) = self.rename.as_mut() else {
                    return false;
                };
                edit_grapheme_input(&mut state.input, &mut state.cursor, code);
            }
        }
        true
    }

    pub fn handle_search_key(&mut self, code: KeyCode) -> bool {
        let mut clear_search = false;
        let mut accept_search = false;

        {
            let state = match &mut self.search {
                Some(s) => s,
                None => return false,
            };

            match code {
                KeyCode::Esc => {
                    clear_search = true;
                }
                KeyCode::Enter => {
                    accept_search = true;
                }
                _ => {
                    edit_grapheme_input(&mut state.query, &mut state.cursor, code);
                }
            }
        }

        if clear_search {
            self.search = None;
            self.search_active = false;
            self.update_view();
            return false;
        }

        if accept_search {
            self.search_active = false;
            if self
                .search
                .as_ref()
                .is_some_and(|state| state.query.is_empty())
            {
                self.search = None;
            }
            self.update_view();
            return false;
        }

        self.update_view();
        true
    }

    fn set_status(&mut self, msg: String, secs: u64) {
        let msg = safe_text::bounded_terminal_text(&msg, STATUS_MESSAGE_MAX_CHARS);
        let changed = self.status_msg.as_deref() != Some(msg.as_str()) || self.status_is_error;
        self.status_msg = Some(msg);
        self.status_is_error = false;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
        if changed {
            self.mark_render_changed();
        }
    }

    fn set_status_error(&mut self, msg: String, secs: u64) {
        let msg = safe_text::bounded_terminal_text(&msg, STATUS_MESSAGE_MAX_CHARS);
        let changed = self.status_msg.as_deref() != Some(msg.as_str()) || !self.status_is_error;
        self.status_msg = Some(msg);
        self.status_is_error = true;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
        if changed {
            self.mark_render_changed();
        }
    }

    pub fn auto_refresh_interval_secs(&self) -> u64 {
        self.auto_refresh_interval.as_secs()
    }

    pub fn auto_refresh_remaining_secs(&self) -> Option<u64> {
        if !self.auto_refresh_enabled {
            return None;
        }
        Some(
            self.next_auto_refresh
                .map(|next| next.saturating_duration_since(Instant::now()).as_secs())
                .unwrap_or(0),
        )
    }

    pub fn toggle_auto_refresh(&mut self) {
        self.auto_refresh_enabled = !self.auto_refresh_enabled;
        if self.auto_refresh_enabled {
            self.next_auto_refresh = Some(Instant::now());
            self.set_status(
                format!(
                    "Auto refresh on (every {}s)",
                    self.auto_refresh_interval_secs()
                ),
                4,
            );
        } else {
            self.next_auto_refresh = None;
            self.set_status("Auto refresh off".to_string(), 3);
        }
    }

    pub fn toggle_detail_panel(&mut self) {
        self.detail_visible = !self.detail_visible;
        if self.detail_visible {
            // This is an explicit request from the user, unlike the automatic
            // detail prefetch that waits for all quota rows to settle.
            self.ensure_models_loaded_for_selected();
            self.set_status("Account details shown".to_string(), 3);
        } else {
            self.set_status("Account details hidden".to_string(), 3);
        }
    }

    /// Toggle auto-warmup. Auto-warmup piggybacks on the auto-refresh tick: every
    /// refresh cycle it calls `warmup_all`, which warms the short window when
    /// present, or the weekly window for a weekly-only response.
    /// Enabling auto-warmup turns on auto-refresh if it is off — without refresh,
    /// the warmup pass has no fresh usage data to decide eligibility.
    pub fn toggle_auto_warmup(&mut self) {
        self.auto_warmup_enabled = !self.auto_warmup_enabled;
        if self.auto_warmup_enabled {
            let mut msg = "Auto warmup on".to_string();
            if !self.auto_refresh_enabled {
                self.auto_refresh_enabled = true;
                self.next_auto_refresh = Some(Instant::now());
                msg.push_str(&format!(
                    " (also enabled auto-refresh every {}s)",
                    self.auto_refresh_interval_secs()
                ));
            }
            self.set_status(msg, 4);
        } else {
            self.set_status("Auto warmup off".to_string(), 3);
        }
    }

    pub fn run_due_auto_refresh(&mut self) {
        if !self.auto_refresh_enabled {
            return;
        }

        let now = Instant::now();
        if self.next_auto_refresh.is_some_and(|next| now < next) {
            return;
        }

        if !self.credential_operations_ready()
            || self.loading_count() > 0
            || !self.warmup_tasks.is_empty()
            || self.warmup_preflight.is_some()
        {
            self.next_auto_refresh = Some(now + Duration::from_secs(5));
            return;
        }

        let account_count = self.accounts.len();
        if self.auto_warmup_enabled {
            self.warmup_all(account_count);
        } else {
            self.refresh_all(Refresh::Unattended);
        }
        self.next_auto_refresh = Some(now + self.auto_refresh_interval);

        if !self.auto_warmup_enabled {
            self.set_status(
                format!("Auto refresh: refreshing {account_count} account(s)"),
                4,
            );
        }
    }

    fn recompute_workspace_expiry_deadline(&mut self) {
        self.workspace_next_expiry = self
            .workspace_states
            .values()
            .map(|resolution| resolution.fresh_until)
            .min();
    }

    fn expire_workspace_resolutions(&mut self, now: Instant) {
        if self.workspace_next_expiry.is_none_or(|expiry| now < expiry) {
            return;
        }
        let expired_account_ids = self
            .workspace_states
            .iter()
            .filter_map(|(account_id, resolution)| {
                (!resolution.is_fresh(now)).then_some(account_id.clone())
            })
            .collect::<BTreeSet<_>>();
        self.workspace_states
            .retain(|_, resolution| resolution.is_fresh(now));
        self.recompute_workspace_expiry_deadline();
        if expired_account_ids.is_empty() {
            return;
        }
        for account in &mut self.accounts {
            if account
                .info
                .account_id
                .as_ref()
                .is_some_and(|account_id| expired_account_ids.contains(account_id))
            {
                account.info.workspace_name = None;
            }
        }
        self.mark_render_changed();
        self.update_view();
        if matches!(self.menu, Some(super::menu::MenuState::Account { .. })) {
            self.rebuild_open_account_menu();
        }
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        self.expire_workspace_resolutions(now);
        if let Some(expiry) = self.status_expiry
            && now >= expiry
        {
            self.status_msg = None;
            self.status_expiry = None;
            self.mark_render_changed();
        }
    }
}

pub async fn run(file_log_writer: crate::logging::FileLogWriter) -> Result<()> {
    // Ensure terminal is restored even on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let startup_exit_warnings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let result = run_app(
        &mut terminal,
        file_log_writer,
        Arc::clone(&startup_exit_warnings),
    )
    .await;
    ratatui::restore();
    let warnings = std::mem::take(
        &mut *startup_exit_warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for warning in warnings {
        eprintln!("{}", crate::color::warn(&warning));
    }
    result
}

/// Preserve an event-loop error while still reaching the same shutdown
/// boundaries as an explicit `q`. Rendering and terminal input can fail after
/// a refresh request has reached the server or while post-draw maintenance is
/// running, so returning immediately could lose either credential persistence
/// or exact cleanup completion.
async fn drain_credential_tasks_on_error<T>(app: &mut App, result: Result<T>) -> Result<T> {
    if result.is_err() {
        app.drain_credential_tasks().await;
        app.drain_startup_maintenance().await;
    }
    result
}

enum StartupUsagePhase {
    WaitingForProfiles,
    ReadingCache {
        identities: HashMap<String, StrictAccountBinding>,
    },
    Ready {
        cache_error: Option<String>,
    },
    RefreshingCore {
        ordered_aliases: Vec<String>,
    },
    Settled,
}

fn advance_startup_ready_phase(app: &mut App, phase: &mut StartupUsagePhase) -> bool {
    let cache_error = match std::mem::replace(phase, StartupUsagePhase::Settled) {
        StartupUsagePhase::Ready { cache_error } => cache_error,
        previous => {
            *phase = previous;
            return false;
        }
    };
    if let Some(error) = cache_error {
        app.set_status_error(error, 6);
    }
    if !app.accounts.is_empty() {
        let mut target_indices = app.view_indices.clone();
        let mut included = target_indices.iter().copied().collect::<BTreeSet<_>>();
        for idx in 0..app.accounts.len() {
            if included.insert(idx) {
                target_indices.push(idx);
            }
        }
        let ordered_aliases = app.start_startup_refresh_indices(&target_indices);
        *phase = StartupUsagePhase::RefreshingCore { ordered_aliases };
    }
    true
}

fn advance_startup_usage_phase(app: &mut App, phase: &mut StartupUsagePhase) -> bool {
    let StartupUsagePhase::RefreshingCore { ordered_aliases } = phase else {
        return false;
    };
    app.poll_usage_lease_releases();
    if !app.startup_core_refreshes_settled(ordered_aliases) {
        return false;
    }
    *phase = StartupUsagePhase::Settled;
    true
}

fn redraw_after_poll(
    explicitly_requested: bool,
    revision_before: u64,
    revision_after: u64,
    last_render_second: i64,
    render_second: i64,
) -> bool {
    explicitly_requested || revision_before != revision_after || last_render_second != render_second
}

async fn run_app(
    terminal: &mut DefaultTerminal,
    file_log_writer: crate::logging::FileLogWriter,
    startup_exit_warnings: Arc<std::sync::Mutex<Vec<String>>>,
) -> Result<()> {
    let mut app = App::with_http_client_and_warning_sink(None, startup_exit_warnings);
    let initial_render_now = crate::auth::now_unix_secs().context("reading startup clock")?;
    // The registry can live on a synced filesystem and contains one auth file
    // per account. Start that immutable snapshot on the blocking pool, paint
    // immediately, and only then let its result drive reconciliation/cache IO.
    app.start_startup_profile_load();
    let initial_draw = terminal
        .draw(|frame| super::ui::render(frame, &mut app, initial_render_now))
        .context("drawing initial TUI frame");
    app.start_post_draw_startup_maintenance(file_log_writer);
    drain_credential_tasks_on_error(&mut app, initial_draw).await?;
    let mut last_render_second = initial_render_now;
    let mut redraw_requested = false;

    // Client construction may synchronously read and parse a custom CA bundle.
    // Track it on the blocking pool so profile/cache startup and input handling
    // continue independently, then reuse the resulting pool for every request.
    app.start_startup_http_client();
    let mut startup_auth_settled = false;
    let mut startup_usage_phase = StartupUsagePhase::WaitingForProfiles;

    loop {
        let render_revision_before_poll = app.render_revision();
        if let Some(result) = app.poll_startup_http_client().await {
            drain_credential_tasks_on_error(&mut app, result).await?;
            app.start_update_check();
        }
        if matches!(startup_usage_phase, StartupUsagePhase::WaitingForProfiles)
            && let Some(result) = app.poll_startup_profile_result().await
        {
            match result {
                Ok(snapshot) => {
                    app.apply_loaded_profiles(
                        snapshot.current,
                        snapshot.accounts,
                        &BTreeSet::new(),
                    );
                    app.start_startup_auth_reconciliation();
                    let identities = app
                        .accounts
                        .iter()
                        .filter_map(|entry| {
                            strict_account_identity(&entry.info)
                                .map(|identity| (entry.alias.clone(), identity))
                        })
                        .collect::<HashMap<_, _>>();
                    if identities.is_empty() {
                        startup_usage_phase = StartupUsagePhase::Ready { cache_error: None };
                    } else {
                        let mut account_ids = identities
                            .values()
                            .map(|identity| identity.account_id.clone())
                            .collect::<Vec<_>>();
                        account_ids.sort();
                        account_ids.dedup();
                        app.start_startup_cache_read(identities.clone(), account_ids);
                        startup_usage_phase = StartupUsagePhase::ReadingCache { identities };
                    }
                }
                Err(error) => {
                    app.startup_auth_state = StartupAuthState::Blocked;
                    app.set_status_error(
                        format!("Saved accounts could not be loaded: {error:#}"),
                        8,
                    );
                    startup_auth_settled = true;
                    startup_usage_phase = StartupUsagePhase::Settled;
                }
            }
        }

        if matches!(startup_usage_phase, StartupUsagePhase::ReadingCache { .. })
            && let Some(result) = app.poll_startup_cache_result().await
        {
            let StartupUsagePhase::ReadingCache { identities } = std::mem::replace(
                &mut startup_usage_phase,
                StartupUsagePhase::WaitingForProfiles,
            ) else {
                unreachable!("startup cache result requires the reading phase")
            };
            let cache_error = match result {
                Ok(Some(snapshot)) => {
                    if !app.accounts.is_empty() {
                        app.apply_startup_cache_snapshot(snapshot, &identities);
                    }
                    None
                }
                Ok(None) => Some(
                    "Startup usage cache read was cancelled before acquiring its lock".to_string(),
                ),
                Err(error) => Some(format!("Could not read usage cache: {error:#}")),
            };
            startup_usage_phase = StartupUsagePhase::Ready { cache_error };
        }

        if !startup_auth_settled && app.startup_auth_reconciliation.is_some() {
            match app.poll_startup_auth_reconciliation().await {
                StartupAuthPoll::Pending => {}
                StartupAuthPoll::Ready => {
                    startup_auth_settled = true;
                }
                StartupAuthPoll::Blocked => startup_auth_settled = true,
            }
        }
        if startup_auth_settled && matches!(startup_usage_phase, StartupUsagePhase::Ready { .. }) {
            if app.startup_auth_state == StartupAuthState::Blocked {
                startup_usage_phase = StartupUsagePhase::Settled;
            } else if app.credential_operations_ready() {
                advance_startup_ready_phase(&mut app, &mut startup_usage_phase);
            }
        }
        app.poll_results();
        app.poll_usage_lease_releases();
        app.poll_workspace_lookup_tasks().await;
        app.poll_workspace_cache_writes().await;
        app.poll_warmup_preflight_result().await;
        app.poll_warmup_results().await;
        app.poll_reset_card_results();
        app.poll_model_results();
        app.poll_auth_expiry_tasks().await;
        app.poll_account_tasks().await;
        app.poll_usage_cache_invalidations().await;
        app.poll_profile_switch_results();
        app.poll_profile_mutation().await;
        app.poll_update();
        app.poll_startup_maintenance().await;
        app.tick();
        advance_startup_usage_phase(&mut app, &mut startup_usage_phase);
        if app.startup_auth_state == StartupAuthState::Ready {
            if matches!(startup_usage_phase, StartupUsagePhase::Settled) {
                app.run_due_auto_refresh();
            }
            if matches!(
                startup_usage_phase,
                StartupUsagePhase::RefreshingCore { .. } | StartupUsagePhase::Settled
            ) && app.selected_core_usage_is_settled()
            {
                app.ensure_models_loaded_for_selected();
            }
        }
        if startup_auth_settled && matches!(startup_usage_phase, StartupUsagePhase::Settled) {
            app.present_startup_maintenance_warnings();
        }

        let render_now = drain_credential_tasks_on_error(
            &mut app,
            crate::auth::now_unix_secs().context("reading system clock for TUI render"),
        )
        .await?;
        if redraw_after_poll(
            redraw_requested,
            render_revision_before_poll,
            app.render_revision(),
            last_render_second,
            render_now,
        ) {
            let draw_result = terminal
                .draw(|f| super::ui::render(f, &mut app, render_now))
                .context("drawing TUI");
            drain_credential_tasks_on_error(&mut app, draw_result).await?;
            last_render_second = render_now;
            redraw_requested = false;
        }

        let event_ready = drain_credential_tasks_on_error(
            &mut app,
            event::poll(Duration::from_millis(100)).context("polling terminal events"),
        )
        .await?;
        if event_ready {
            let terminal_event = drain_credential_tasks_on_error(
                &mut app,
                event::read().context("reading terminal event"),
            )
            .await?;
            redraw_requested = true;
            let Event::Key(key) = terminal_event else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // Search and rename inputs need raw case-sensitive keystrokes.
            if app.rename.is_some() {
                app.handle_rename_key(key.code);
                continue;
            }
            if app.search_active {
                app.handle_search_key(key.code);
                continue;
            }

            // Capital 'W' is a distinct global binding (toggle auto-warmup),
            // separate from menu 'w' (per-account warmup). Detect it before
            // case normalization so it survives the lowercase dispatch below.
            // Only meaningful in the main view (no popup/menu/confirm overlay).
            if matches!(key.code, KeyCode::Char('W'))
                && app.help_popup.is_none()
                && app.menu.is_none()
                && app.confirm.is_none()
            {
                app.toggle_auto_warmup();
                continue;
            }

            // Normalize letter case for top-level dispatch:
            // any uppercase letter is treated as its lowercase equivalent.
            let code = match key.code {
                KeyCode::Char(c) if c.is_ascii_uppercase() => KeyCode::Char(c.to_ascii_lowercase()),
                other => other,
            };

            // Help popup: any key (esc/q/h preferred) closes it; arrows scroll.
            if app.help_popup.is_some() {
                handle_help_key(&mut app, code);
                continue;
            }

            // Active menu intercepts everything.
            if app.menu.is_some() {
                handle_menu_key(&mut app, terminal, code).await;
                continue;
            }

            if app.confirm.is_some() {
                match code {
                    KeyCode::Char('y') => app.confirm_action(),
                    _ => app.cancel_confirm(),
                }
                continue;
            }

            match code {
                KeyCode::Char('q') => {
                    app.shutting_down = true;
                    let pending_credentials = app.has_pending_credential_tasks();
                    let pending_maintenance = app.has_pending_startup_maintenance();
                    if pending_credentials || pending_maintenance {
                        let message = match (pending_credentials, pending_maintenance) {
                            (true, true) => {
                                "Finishing active account operations and startup maintenance before exit..."
                            }
                            (true, false) => "Finishing active account operations before exit...",
                            (false, true) => "Finishing startup maintenance before exit...",
                            (false, false) => unreachable!("pending exit work was already checked"),
                        };
                        app.set_status(message.to_string(), 60);
                        // Failure to paint the exit notice must not skip the
                        // credential-safety boundary below.
                        let _ =
                            terminal.draw(|frame| super::ui::render(frame, &mut app, render_now));
                        app.drain_credential_tasks().await;
                        app.drain_startup_maintenance().await;
                    }
                    break;
                }
                KeyCode::Esc => {
                    if app.search.is_some() {
                        app.search = None;
                        app.update_view();
                    } else if !app.marked.is_empty() {
                        app.clear_marks();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') if app.selected + 1 < app.view_indices.len() => {
                    app.selected += 1;
                }
                KeyCode::Up | KeyCode::Char('k') if app.selected > 0 => {
                    app.selected -= 1;
                }
                KeyCode::Enter => {
                    if app.marked.is_empty() {
                        app.open_account_menu();
                    } else {
                        app.open_batch_menu();
                    }
                }
                KeyCode::Char('a') => app.open_add_menu(),
                KeyCode::Char('r') => app.refresh(Refresh::Forced),
                KeyCode::Char('t') => app.toggle_auto_refresh(),
                KeyCode::Char('i') => app.toggle_detail_panel(),
                KeyCode::Char('s') => app.cycle_sort(),
                KeyCode::Char('h') => app.open_help(),
                KeyCode::Char(' ') => app.toggle_mark(),
                KeyCode::Char('/') => {
                    if let Some(search) = &mut app.search {
                        search.cursor = grapheme_count(&search.query);
                    } else {
                        app.search = Some(SearchState {
                            query: String::new(),
                            cursor: 0,
                        });
                        app.update_view();
                    }
                    app.search_active = true;
                }
                _ => {}
            }
        }
    }

    Ok(())
}

async fn handle_menu_key(app: &mut App, terminal: &mut DefaultTerminal, code: KeyCode) {
    let Some(menu) = app.menu.as_mut() else {
        return;
    };
    let action = menu.handle_key(code);
    use super::menu::MenuAction;
    match action {
        MenuAction::Noop => {}
        MenuAction::Close => app.close_menu(),
        MenuAction::Use(alias) => dispatch_menu_use(app, &alias),
        MenuAction::ReloginRequest(alias, email) => {
            app.open_relogin_flow_menu(alias, email);
        }
        MenuAction::Relogin { alias, device } => {
            app.close_menu();
            perform_oauth(terminal, app, OAuthMode::Relogin(alias), device).await;
        }
        MenuAction::Add { device } => {
            app.close_menu();
            perform_oauth(terminal, app, OAuthMode::Add, device).await;
        }
        MenuAction::RefreshOne(alias) => {
            app.close_menu();
            app.refresh_one(&alias);
        }
        MenuAction::Rename(alias) => {
            app.close_menu();
            app.start_rename_alias(&alias);
        }
        MenuAction::WarmupOne(alias) => {
            app.close_menu();
            app.warmup_one(&alias);
        }
        MenuAction::ConsumeResetCard(alias) => {
            app.close_menu();
            app.request_consume_reset_card(&alias);
        }
        MenuAction::DeleteRequest(alias) => {
            app.close_menu();
            app.request_delete_alias(&alias);
        }
        MenuAction::BatchRefresh => {
            app.close_menu();
            app.refresh_marked();
        }
        MenuAction::BatchWarmup => {
            app.close_menu();
            app.warmup_marked();
        }
        MenuAction::BatchReloginRequest => {
            app.open_batch_relogin_flow();
        }
        MenuAction::BatchRelogin { device } => {
            app.close_menu();
            perform_batch_relogin(terminal, app, device).await;
        }
        MenuAction::BatchDeleteRequest => {
            app.close_menu();
            app.request_batch_delete();
        }
    }
}

fn dispatch_menu_use(app: &mut App, alias: &str) {
    app.close_menu();
    let Some(account_idx) = app
        .accounts
        .iter()
        .position(|account| account.alias == alias)
    else {
        app.set_status_error(format!("Account '{alias}' is no longer available"), 5);
        return;
    };
    let Some(view_idx) = app
        .view_indices
        .iter()
        .position(|&index| index == account_idx)
    else {
        app.set_status_error(
            format!("Account '{alias}' is no longer in the current view"),
            5,
        );
        return;
    };
    app.selected = view_idx;
    app.switch_selected();
}

enum OAuthMode {
    Add,
    Relogin(String),
}

struct OAuthSave {
    alias: String,
    message: String,
}

fn reset_plain_terminal_view() {
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
    );
    let _ = std::io::Write::flush(&mut stdout);
}

fn suspend_tui_for_plain_output() {
    ratatui::restore();
    reset_plain_terminal_view();
}

fn resume_tui_after_plain_output(terminal: &mut DefaultTerminal) {
    reset_plain_terminal_view();
    *terminal = ratatui::init();
    let _ = terminal.clear();
}

/// Suspend the TUI, run OAuth (browser PKCE or device code), persist the
/// resulting auth.json to the appropriate profile, then restore the TUI.
///
/// Always restores the terminal even on error so the caller can keep running.
async fn perform_oauth(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mode: OAuthMode,
    device: bool,
) {
    if app.reject_credential_recovery_during_transition() {
        app.close_menu();
        return;
    }
    // Tear down TUI: restore cooked mode + clear screen so the OAuth output
    // (browser prompts, device user_code, polling progress) is visible.
    suspend_tui_for_plain_output();
    // TUI starts with MessageMode::Silent; switch to Stdout so login.rs
    // user_println calls (device code URL, user_code) are actually shown.
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    let mode_name = match &mode {
        OAuthMode::Add => "Add new account".to_string(),
        OAuthMode::Relogin(alias) => {
            format!("Re-login: {}", safe_text::terminal_text(alias))
        }
    };
    println!("\n=== {mode_name} ===");
    if device {
        println!("Flow: device code\n");
    } else {
        println!("Flow: browser (PKCE)\n");
    }

    let attempted_alias = match &mode {
        OAuthMode::Add => None,
        OAuthMode::Relogin(alias) => Some(alias.clone()),
    };
    let result = run_oauth_inner(mode, device, None).await;

    // Flush stdout so any buffered output (e.g. device code URL) appears
    // before TUI repaints, particularly important on Windows.
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if result.is_ok() {
        println!("\nReturning to TUI...");
    } else {
        if let Err(ref e) = result {
            eprintln!("\nError: {}", safe_text::terminal_text(&e.to_string()));
        }
        println!("\nReturning to TUI...");
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    // Restore silent mode before reinitializing TUI.
    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    let mut invalidated_aliases = attempted_alias.into_iter().collect::<BTreeSet<_>>();
    if let Ok(saved) = &result {
        invalidated_aliases.insert(saved.alias.clone());
    }
    app.finish_oauth_attempt(result.map(|saved| saved.message), &invalidated_aliases)
        .await;
}

/// Sequentially re-login every marked alias. The TUI is suspended for the
/// duration; OAuth output goes to the cooked terminal so the user sees
/// browser prompts / device codes / progress.
///
/// Ctrl+C requests that the batch stop after the current round reaches its
/// credential-commit boundary. A round still waiting for OAuth may also return
/// [`login::LoginCancelled`] itself, before it has credentials to persist.
fn batch_relogin_not_attempted(
    total: usize,
    ok: usize,
    failed: usize,
    cancelled_accounts: usize,
) -> usize {
    total.saturating_sub(ok + failed + cancelled_accounts)
}

/// Record a stop request without dropping credential work that may already
/// have reached the server. A round still waiting for its alias lease is
/// cancelled at that safe boundary; once lease acquisition wins, the round
/// reaches its commit boundary before the batch stops.
async fn finish_login_or_stop_after_round<T, LoginFuture, StopFuture>(
    login_future: LoginFuture,
    stop_future: StopFuture,
    lease_control: &profile::ProfileLeaseAcquireControl,
) -> (Result<T>, bool, Option<anyhow::Error>)
where
    LoginFuture: std::future::Future<Output = Result<T>>,
    StopFuture: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::pin!(login_future);
    tokio::pin!(stop_future);
    tokio::select! {
        biased;
        signal = &mut stop_future => {
            let signal_error = signal
                .context("listening for Ctrl+C during batch re-login")
                .err();
            lease_control.cancel_waiting();
            let result = login_future.await;
            (result, signal_error.is_none(), signal_error)
        }
        result = &mut login_future => (result, false, None),
    }
}

async fn perform_batch_relogin(terminal: &mut DefaultTerminal, app: &mut App, device: bool) {
    if app.reject_credential_recovery_during_transition() {
        app.close_menu();
        return;
    }
    let aliases: Vec<String> = app.marked.iter().cloned().collect();
    if aliases.is_empty() {
        return;
    }

    suspend_tui_for_plain_output();
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    let total = aliases.len();
    println!("\n=== Batch re-login: {total} account(s) ===");
    if device {
        println!("Flow: device code\n");
    } else {
        println!("Flow: browser (PKCE)\n");
    }

    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut cancelled_accounts = 0usize;
    let mut stop_requested = false;
    let mut stop_listener_error = None;
    let stop_signal = tokio::signal::ctrl_c();
    tokio::pin!(stop_signal);

    for (i, alias) in aliases.iter().enumerate() {
        println!(
            "\n--- [{}/{}] {} ---",
            i + 1,
            total,
            safe_text::terminal_text(alias)
        );
        let mode = OAuthMode::Relogin(alias.clone());
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let (result, stop_after_round, listener_error) = finish_login_or_stop_after_round(
            run_oauth_inner(mode, device, Some(&lease_control)),
            stop_signal.as_mut(),
            &lease_control,
        )
        .await;
        match result {
            Ok(_) => ok += 1,
            Err(e) if login::is_login_cancelled(&e) => {
                eprintln!("[cancelled] Batch re-login stopped by user");
                cancelled_accounts = 1;
            }
            Err(e) => {
                eprintln!(
                    "[err] {}: {}",
                    safe_text::terminal_text(alias),
                    safe_text::terminal_text(&e.to_string())
                );
                failed.push((alias.clone(), e.to_string()));
            }
        }
        if let Some(error) = listener_error {
            eprintln!(
                "[err] Ctrl+C listener failed: {}",
                safe_text::terminal_text(&error.to_string())
            );
            stop_listener_error = Some(error);
        }
        if cancelled_accounts > 0 || stop_after_round || stop_listener_error.is_some() {
            stop_requested = stop_after_round;
            break;
        }
    }

    let _ = std::io::Write::flush(&mut std::io::stdout());
    let stopped = stop_requested || cancelled_accounts > 0 || stop_listener_error.is_some();
    if stopped {
        let not_attempted =
            batch_relogin_not_attempted(total, ok, failed.len(), cancelled_accounts);
        println!(
            "\n=== Batch stopped: {ok} ok, {} failed, {cancelled_accounts} cancelled, {not_attempted} not attempted ===",
            failed.len(),
        );
    } else {
        println!("\n=== Batch complete: {ok} ok, {} failed ===", failed.len());
    }
    if !failed.is_empty() {
        for (a, e) in &failed {
            println!(
                "  - {}: {}",
                safe_text::terminal_text(a),
                safe_text::terminal_text(e)
            );
        }
    }
    println!("\nReturning to TUI...");
    tokio::time::sleep(Duration::from_millis(1200)).await;

    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    app.marked.clear();
    let summary = if let Some(error) = &stop_listener_error {
        format!("Batch re-login stopped after Ctrl+C listener failed: {error}")
    } else if stopped {
        let not_attempted =
            batch_relogin_not_attempted(total, ok, failed.len(), cancelled_accounts);
        format!(
            "Batch re-login stopped: {ok} ok, {} failed, {cancelled_accounts} cancelled, {not_attempted} not attempted",
            failed.len()
        )
    } else if failed.is_empty() {
        format!("Batch re-login: {ok} ok")
    } else {
        format!("Batch re-login: {ok} ok, {} failed", failed.len())
    };
    // See `perform_oauth`: a failed round may still have preserved new profile
    // credentials before a live-auth update failed.
    let invalidated_aliases = aliases.iter().cloned().collect::<BTreeSet<_>>();
    app.invalidate_models_after_credential_reload(&invalidated_aliases);
    match app
        .complete_credential_recovery_reload(&invalidated_aliases)
        .await
    {
        Ok(()) => {
            if failed.is_empty() && !stopped {
                app.set_status(summary, 8);
            } else {
                app.set_status_error(summary, 8);
            }
            app.refresh(Refresh::Forced);
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Err(error) => {
            app.startup_auth_state = StartupAuthState::Blocked;
            app.set_status_error(format!("{summary}; profile reload failed: {error:#}"), 8);
        }
    }
}

async fn run_oauth_inner(
    mode: OAuthMode,
    device: bool,
    lease_control: Option<&profile::ProfileLeaseAcquireControl>,
) -> Result<OAuthSave> {
    match mode {
        OAuthMode::Add => {
            let tokens = if device {
                login::run_device_code_auth().await?
            } else {
                login::run_device_auth().await?
            };
            let (auth_val, info) = login::build_auth_from_tokens(&tokens)?;
            let action = profile::save_auth_value(auth_val, None)?;
            let alias = action.alias().to_string();
            let verb = action.action(); // "created" / "updated"
            let email_disp = info.email.as_deref().unwrap_or("unknown");
            println!(
                "[ok] Account {verb}: {} ({})",
                safe_text::terminal_text(&alias),
                safe_text::terminal_text(email_disp)
            );
            Ok(OAuthSave {
                message: format!("Account {verb}: {alias}"),
                alias,
            })
        }
        OAuthMode::Relogin(alias) => {
            // Capture the target identity under a short lease, then release it
            // while the user completes OAuth. Once this first acquisition
            // succeeds, batch cancellation drains the round through commit;
            // the second acquisition is therefore intentionally non-cancellable.
            let prepared = {
                let lease = match lease_control {
                    Some(control) => {
                        match profile::acquire_profile_lease_async_cancellable(
                            alias.clone(),
                            control,
                        )
                        .await?
                        {
                            Some(lease) => lease,
                            None => return Err(login::LoginCancelled.into()),
                        }
                    }
                    None => profile::acquire_profile_lease_async(alias.clone()).await?,
                };
                profile::prepare_profile_reauth_with_lease(&lease)?
            };
            let tokens = if device {
                login::run_device_code_auth().await?
            } else {
                login::run_device_auth().await?
            };
            let (auth_val, info) = login::build_auth_from_tokens(&tokens)?;
            let lease = profile::acquire_profile_lease_async(alias.clone()).await?;
            profile::commit_prepared_profile_reauth_with_lease(prepared, &lease, &auth_val)?;
            drop(lease);
            let email_disp = info.email.as_deref().unwrap_or("unknown");
            println!(
                "[ok] Re-logged in: {} ({})",
                safe_text::terminal_text(&alias),
                safe_text::terminal_text(email_disp)
            );
            Ok(OAuthSave {
                message: format!("Re-logged in: {alias}"),
                alias,
            })
        }
    }
}

fn handle_help_key(app: &mut App, code: KeyCode) {
    let Some(state) = app.help_popup.as_mut() else {
        return;
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => app.close_help(),
        KeyCode::Down | KeyCode::Char('j') => state.scroll_down(u16::MAX),
        KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
        KeyCode::PageDown => state.page_down(5, u16::MAX),
        KeyCode::PageUp => state.page_up(5),
        KeyCode::Home => state.reset(),
        _ => app.close_help(),
    }
}

/// Byte boundaries for the same Unicode grapheme clusters ratatui renders.
/// Cursor state is an index into this vector, not a scalar-value offset, so
/// combining marks and emoji ZWJ sequences are never split by editing.
fn grapheme_boundaries(input: &str) -> Vec<usize> {
    let line = Line::from(input);
    let mut boundaries = Vec::with_capacity(input.chars().count() + 1);
    boundaries.push(0);
    for grapheme in line.styled_graphemes(Style::default()) {
        let end = boundaries.last().copied().unwrap_or(0) + grapheme.symbol.len();
        boundaries.push(end);
    }
    debug_assert_eq!(boundaries.last().copied(), Some(input.len()));
    boundaries
}

pub(super) fn grapheme_count(input: &str) -> usize {
    grapheme_boundaries(input).len().saturating_sub(1)
}

pub(super) fn grapheme_to_byte(input: &str, cursor: usize) -> usize {
    let boundaries = grapheme_boundaries(input);
    boundaries.get(cursor).copied().unwrap_or(input.len())
}

fn grapheme_cursor_after_byte(input: &str, byte: usize) -> usize {
    let target = byte.min(input.len());
    grapheme_boundaries(input)
        .into_iter()
        .position(|boundary| boundary >= target)
        .unwrap_or_else(|| grapheme_count(input))
}

fn edit_grapheme_input(input: &mut String, cursor: &mut usize, code: KeyCode) {
    *cursor = (*cursor).min(grapheme_count(input));
    match code {
        KeyCode::Backspace if *cursor > 0 => {
            let start = grapheme_to_byte(input, *cursor - 1);
            let end = grapheme_to_byte(input, *cursor);
            input.replace_range(start..end, "");
            *cursor -= 1;
        }
        KeyCode::Delete if *cursor < grapheme_count(input) => {
            let start = grapheme_to_byte(input, *cursor);
            let end = grapheme_to_byte(input, *cursor + 1);
            input.replace_range(start..end, "");
        }
        KeyCode::Left if *cursor > 0 => *cursor -= 1,
        KeyCode::Right if *cursor < grapheme_count(input) => *cursor += 1,
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = grapheme_count(input),
        KeyCode::Char(character) if !character.is_control() => {
            let byte = grapheme_to_byte(input, *cursor);
            input.insert(byte, character);
            *cursor = grapheme_cursor_after_byte(input, byte + character.len_utf8());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountEntry, AccountRefreshPlan, AccountTaskControls, AccountTaskKind, App,
        BatchDeleteReport, CachedUsageApplication, ConfirmAction, ModelStatus,
        STATUS_MESSAGE_MAX_CHARS, SafeTaskCancellation, SearchState, SortMode,
        StartupFileLogInitTask, StartupSelfUpdateCleanupTask, StartupUsagePhase, UsageStatus,
        WarmupOrigin, WarmupReadyCandidate, WarmupTask, WorkspaceRefresh,
        advance_startup_ready_phase, advance_startup_usage_phase, batch_relogin_not_attempted,
        dispatch_menu_use, drain_credential_tasks_on_error, finish_login_or_stop_after_round,
        prepare_workspace_lookup_auth, redraw_after_poll, refresh_fetches_loaded_usage,
        reset_card_failure_from_outcome, retained_usage_by_identity, strict_account_identity,
    };
    use crate::{
        jwt::{AccountInfo, OrgInfo, StrictAccountBinding},
        login, profile,
        usage::{Refresh, ResetCredit, UsageInfo, WindowUsage},
        warmup::ModelEntry,
    };
    use crossterm::event::KeyCode;
    use std::{sync::Arc, time::Instant};

    fn test_binding() -> StrictAccountBinding {
        StrictAccountBinding {
            account_id: "acct_test".to_string(),
            email: "test@example.com".to_string(),
        }
    }

    fn test_account_info() -> AccountInfo {
        test_account_info_for("test@example.com", "acct_test")
    }

    fn test_account_info_for(email: &str, account_id: &str) -> AccountInfo {
        AccountInfo {
            account_id: Some(account_id.to_string()),
            email: Some(email.to_string()),
            ..AccountInfo::default()
        }
    }

    fn add_test_account(app: &mut App, alias: &str) {
        app.accounts.push(AccountEntry {
            alias: alias.to_string(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn set_text(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn write_auth_durable(path: &std::path::Path, value: &serde_json::Value) {
        crate::auth::write_auth(path, value)
            .unwrap()
            .assert_durably_published();
    }

    fn managed_auth(
        email: &str,
        account_id: &str,
        access: &str,
        refresh: &str,
    ) -> serde_json::Value {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let token = format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        serde_json::json!({
            "tokens": {
                "id_token": token,
                "access_token": access,
                "refresh_token": refresh,
                "account_id": account_id,
            }
        })
    }

    async fn finish_warmup_preflight(app: &mut App) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                app.poll_warmup_preflight_result().await;
                if app.warmup_preflight.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("warmup preflight must finish");
    }

    async fn finish_workspace_cache_writes(app: &mut App) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !app.workspace_cache_writes.is_empty() {
                app.poll_workspace_cache_writes().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace cache writes must finish");
    }

    async fn settle_profile_switch(app: &mut App) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                app.poll_account_tasks().await;
                app.poll_profile_switch_results();
                let switch_task_pending = app.account_tasks.values().any(|task| {
                    matches!(
                        task.kind,
                        AccountTaskKind::SwitchPrepare
                            | AccountTaskKind::SwitchSync
                            | AccountTaskKind::SwitchCommit
                    )
                });
                if !switch_task_pending {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("profile switch task must settle");
    }

    #[test]
    fn selecting_the_current_account_starts_no_switch_task() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "current".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: true,
        });
        app.view_indices.push(0);

        app.switch_selected();

        assert!(app.account_tasks.is_empty());
        assert_eq!(app.status_msg.as_deref(), Some("Already using current"));
    }

    #[tokio::test]
    async fn profile_switch_progress_uses_the_newest_tracked_switch_phase() {
        let mut app = App::new();
        app.track_account_task(
            "account".into(),
            AccountTaskKind::SwitchPrepare,
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(std::future::pending()),
        );
        app.track_account_task(
            "account".into(),
            AccountTaskKind::SwitchSync,
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(std::future::pending()),
        );
        app.track_account_task(
            "other".into(),
            AccountTaskKind::Usage { request_id: 7 },
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(std::future::pending()),
        );

        assert_eq!(
            app.profile_switch_progress().as_deref(),
            Some("Synchronizing the current Codex login before switching to account...")
        );

        app.track_account_task(
            "account".into(),
            AccountTaskKind::SwitchCommit,
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(std::future::pending()),
        );
        assert_eq!(
            app.profile_switch_progress().as_deref(),
            Some("Switching to account...")
        );
    }

    #[test]
    fn safe_task_cancellation_and_work_start_share_one_exact_boundary() {
        let cancelled = SafeTaskCancellation::new();
        assert!(cancelled.request());
        assert!(!cancelled.begin_work());
        cancelled.mark_completed();
        assert!(cancelled.completed());

        let committed = SafeTaskCancellation::new();
        assert!(committed.begin_work());
        assert!(!committed.request());
        assert!(!committed.completed());
    }

    #[tokio::test]
    async fn cancellable_first_network_permit_does_not_treat_limiter_shutdown_as_cancellation() {
        let limiter = Arc::new(tokio::sync::Semaphore::new(0));
        limiter.close();
        let cancellation = SafeTaskCancellation::new();

        let error = super::cancellable_first_network_permit(limiter, cancellation.clone())
            .await
            .expect_err("a closed limiter must remain an operational error");

        assert!(!crate::usage::network_wait_was_cancelled(&error));
        assert!(!cancellation.completed());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn account_menu_enter_then_u_dispatches_the_complete_switch_without_confirmation() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();

        for (alias, email, account_id, access) in [
            (
                "current",
                "current@example.com",
                "acct_current",
                "refreshed-current",
            ),
            (
                "target",
                "target@example.com",
                "acct_target",
                "target-access",
            ),
            (
                "unrelated",
                "unrelated@example.com",
                "acct_unrelated",
                "unrelated-access",
            ),
        ] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(
                &path,
                &managed_auth(email, account_id, access, &format!("{alias}-refresh")),
            );
        }
        let current_auth = managed_auth(
            "current@example.com",
            "acct_current",
            "refreshed-current",
            "current-refresh",
        );
        std::fs::write(
            codex_home.join("auth.json"),
            serde_json::to_vec(&current_auth).unwrap(),
        )
        .unwrap();

        let mut app = App::new();
        for (alias, is_current) in [("current", true), ("target", false), ("unrelated", false)] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: test_account_info_for(
                    &format!("{alias}@example.com"),
                    &format!("acct_{alias}"),
                ),
                usage: UsageStatus::Idle,
                is_current,
            });
        }
        app.view_indices.extend([0, 1, 2]);
        app.selected = 1;
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        for (idx, alias) in [(0, "current"), (1, "target"), (2, "unrelated")] {
            app.fetch_usage_for(
                idx,
                AccountRefreshPlan::usage_and_workspace(Refresh::Cached),
            );
            app.ensure_models_loaded(alias);
            app.spawn_preflighted_warmup(
                super::WarmupReadyCandidate {
                    alias: alias.into(),
                    binding: StrictAccountBinding {
                        account_id: format!("acct_{alias}"),
                        email: format!("{alias}@example.com"),
                    },
                    cached_usage: None,
                },
                WarmupOrigin::Manual,
            );
        }
        assert!(app.refreshing_requests.contains_key("current"));
        assert!(app.refreshing_requests.contains_key("target"));
        assert!(app.refreshing_requests.contains_key("unrelated"));
        assert!(app.model_requests.contains_key("current"));
        assert!(app.model_requests.contains_key("target"));
        assert!(app.model_requests.contains_key("unrelated"));
        assert!(app.is_warmup_in_flight("current"));
        assert!(app.is_warmup_in_flight("target"));
        assert!(app.is_warmup_in_flight("unrelated"));

        app.startup_auth_state = super::StartupAuthState::Blocked;
        app.open_account_menu();
        let action = app
            .menu
            .as_mut()
            .expect("Enter should open the account menu")
            .handle_key(KeyCode::Char('u'));
        let super::super::menu::MenuAction::Use(alias) = action else {
            panic!("u should dispatch the account-menu Use action");
        };
        dispatch_menu_use(&mut app, &alias);
        assert!(app.profile_switch_in_flight());
        settle_profile_switch(&mut app).await;
        assert_eq!(
            app.refreshing_requests
                .get("target")
                .map(|(_, refresh)| *refresh),
            Some(Refresh::Unattended),
            "a cancelled startup cache miss must resume without reopening the alias cache"
        );
        let _ = app.cancel_waiting_background_credential_work_for("target");
        // The equivalent-live fast path no longer enters the synchronization
        // pass that used to cancel the previously current account's unrelated
        // reads. Cancel them explicitly for this bounded test cleanup.
        let _ = app.cancel_waiting_background_credential_work_for("current");

        let settled = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while app
                .account_tasks
                .values()
                .any(|task| task.alias != "unrelated")
                || app
                    .warmup_tasks
                    .values()
                    .any(|task| task.alias != "unrelated")
            {
                app.poll_warmup_results().await;
                app.poll_account_tasks().await;
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "cancelled background reads must settle; account tasks: {:?}; warmup tasks: {:?}",
            app.account_tasks
                .values()
                .map(|task| (
                    task.alias.clone(),
                    task.kind,
                    task.lease_control.is_cancelled(),
                    task.handle.is_finished()
                ))
                .collect::<Vec<_>>(),
            app.warmup_tasks
                .values()
                .map(|task| (
                    task.alias.clone(),
                    task.lease_control.is_cancelled(),
                    task.handle.is_finished()
                ))
                .collect::<Vec<_>>()
        );

        assert!(app.menu.is_none());
        assert!(app.confirm.is_none());
        assert!(!app.accounts[0].is_current);
        assert!(app.accounts[1].is_current);
        assert_eq!(app.startup_auth_state, super::StartupAuthState::Ready);
        assert!(matches!(app.accounts[0].usage, UsageStatus::Idle));
        assert!(matches!(app.accounts[1].usage, UsageStatus::Idle));
        assert!(!app.model_cache.contains_key("current"));
        assert!(!app.model_cache.contains_key("target"));
        assert!(app.refreshing_requests.contains_key("unrelated"));
        assert!(app.model_requests.contains_key("unrelated"));
        assert!(app.is_warmup_in_flight("unrelated"));
        assert_eq!(
            crate::auth::read_auth(&codex_home.join("auth.json")).unwrap(),
            managed_auth(
                "target@example.com",
                "acct_target",
                "target-access",
                "target-refresh",
            )
        );
        assert_eq!(
            crate::auth::read_auth(&home.path().join("profiles/current/auth.json")).unwrap(),
            managed_auth(
                "current@example.com",
                "acct_current",
                "refreshed-current",
                "current-refresh",
            )
        );
        app.drain_credential_tasks().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn alias_cancellation_stops_warmup_model_discovery_without_success_refresh() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        let network_wait = SafeTaskCancellation::new();
        assert!(
            network_wait.begin_work(),
            "the test warmup has already crossed its first network boundary"
        );
        let model_discovery = SafeTaskCancellation::new();
        let task_model_discovery = model_discovery.clone();
        let handle = tokio::spawn(async move {
            task_model_discovery.cancelled().await;
            task_model_discovery.mark_completed();
            Ok(())
        });
        app.warmup_tasks.insert(
            0,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: network_wait.clone(),
                model_discovery: model_discovery.clone(),
                handle,
            },
        );

        let _ = app.cancel_waiting_background_credential_work_for("account");
        assert!(
            !network_wait.request(),
            "alias cancellation must not cross the committed first-request boundary"
        );
        assert!(
            !model_discovery.request(),
            "alias cancellation must request the separate model-discovery boundary"
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.warmup_tasks[&0].handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled model discovery must settle");
        assert!(model_discovery.completed());

        app.poll_warmup_results().await;

        assert!(app.warmup_tasks.is_empty());
        assert!(!app.refreshing_requests.contains_key("account"));
        assert!(app.status_msg.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_drain_cancels_warmup_model_discovery_after_network_admission() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);

        let network_wait = SafeTaskCancellation::new();
        assert!(network_wait.begin_work());
        let model_discovery = SafeTaskCancellation::new();
        let task_model_discovery = model_discovery.clone();
        let handle = tokio::spawn(async move {
            task_model_discovery.cancelled().await;
            task_model_discovery.mark_completed();
            Ok(())
        });
        app.warmup_tasks.insert(
            0,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait,
                model_discovery: model_discovery.clone(),
                handle,
            },
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.drain_credential_tasks(),
        )
        .await
        .expect("shutdown must cancel and drain lease-free model discovery");

        assert!(model_discovery.completed());
        assert!(app.warmup_tasks.is_empty());
        assert!(!app.refreshing_requests.contains_key("account"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_switch_rejects_new_background_credential_work() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: true,
        });
        app.view_indices.push(0);

        let switch_handle = tokio::spawn(async {});
        app.track_account_task(
            "account".into(),
            AccountTaskKind::SwitchPrepare,
            profile::ProfileLeaseAcquireControl::new(),
            switch_handle,
        );
        assert!(app.profile_switch_in_flight());

        app.refresh(Refresh::Forced);
        app.ensure_models_loaded("account");
        app.warmup_one("account");
        assert!(app.refreshing_requests.is_empty());
        assert!(app.model_requests.is_empty());
        assert!(app.model_cache.is_empty());
        assert!(app.warmup_preflight.is_none());
        assert!(app.warmup_tasks.is_empty());

        app.auto_refresh_enabled = true;
        app.next_auto_refresh = Some(Instant::now());
        let deferred_after = Instant::now();
        app.run_due_auto_refresh();
        assert!(
            app.next_auto_refresh
                .is_some_and(|next| next > deferred_after)
        );

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while app.profile_switch_in_flight() {
                tokio::task::yield_now().await;
                app.poll_account_tasks().await;
            }
        })
        .await
        .expect("mock profile switch task must settle");
        assert!(!app.profile_switch_in_flight());
    }

    #[test]
    fn startup_reconciliation_defers_every_account_request_path() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.startup_auth_state = super::StartupAuthState::Reconciling;

        app.ensure_models_loaded("account");
        app.refresh_all(Refresh::Forced);
        app.warmup_one("account");
        app.request_delete_alias("account");
        app.start_rename_alias("account");
        app.marked.insert("account".into());
        app.request_batch_delete();
        assert!(app.model_requests.is_empty());
        assert!(app.refreshing_requests.is_empty());
        assert!(app.warmup_preflight.is_none());
        assert!(app.warmup_tasks.is_empty());
        assert!(app.confirm.is_none());
        assert!(app.rename.is_none());

        app.auto_refresh_enabled = true;
        app.next_auto_refresh = Some(Instant::now());
        let deferred_after = Instant::now();
        app.run_due_auto_refresh();
        assert!(
            app.next_auto_refresh
                .is_some_and(|next| next > deferred_after)
        );
        assert!(app.account_tasks.is_empty());
    }

    #[tokio::test]
    async fn startup_reconciliation_failure_blocks_account_requests() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: true,
        });
        app.view_indices.push(0);
        app.startup_auth_state = super::StartupAuthState::Reconciling;
        app.startup_auth_reconciliation = Some(tokio::spawn(async {
            anyhow::bail!("deliberate reconciliation failure")
        }));
        while !app
            .startup_auth_reconciliation
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            app.poll_startup_auth_reconciliation().await,
            super::StartupAuthPoll::Blocked
        );
        assert_eq!(app.startup_auth_state, super::StartupAuthState::Blocked);
        assert_eq!(app.credential_transition_blocker(), None);
        app.refresh_all(Refresh::Forced);
        app.ensure_models_loaded("account");
        assert!(app.status_is_error);
        assert!(app.account_tasks.is_empty());
        assert!(app.refreshing_requests.is_empty());
        assert!(app.model_requests.is_empty());
    }

    #[test]
    fn auto_refresh_uses_the_loaded_account_snapshot_without_registry_io() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(profile_path, b"not-json").unwrap();

        let mut app = App::new();
        app.auto_refresh_enabled = true;
        app.next_auto_refresh = Some(Instant::now());
        let deferred_after = Instant::now();

        app.run_due_auto_refresh();

        assert!(!app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message == "Auto refresh: refreshing 0 account(s)")
        );
        assert!(app.account_tasks.is_empty());
        assert!(app.refreshing_requests.is_empty());
        assert!(app.warmup_tasks.is_empty());
        assert!(
            app.next_auto_refresh
                .is_some_and(|next| next > deferred_after)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn automatic_warmup_preflight_partitions_accounts_into_one_network_path_each() {
        let mut app = App::new();
        for (alias, account_id) in [("warm", "acct_warm"), ("refresh", "acct_refresh")] {
            app.accounts.push(AccountEntry {
                alias: alias.to_string(),
                info: test_account_info_for(&format!("{alias}@example.com"), account_id),
                usage: UsageStatus::Idle,
                is_current: false,
            });
            app.workspace_states.insert(
                account_id.to_string(),
                super::WorkspaceMemoryResolution {
                    state: crate::cache::WorkspaceState::Absent,
                    fresh_until: Instant::now() + std::time::Duration::from_secs(60),
                },
            );
        }
        app.view_indices.extend([0, 1]);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        app.report_warmup_preflight_success(
            super::WarmupPreflightOrigin::Automatic {
                refreshing_accounts: 2,
            },
            2,
            vec![super::WarmupReadyCandidate {
                alias: "warm".into(),
                binding: StrictAccountBinding {
                    account_id: "acct_warm".into(),
                    email: "warm@example.com".into(),
                },
                cached_usage: Some(UsageInfo::default()),
            }],
        );

        assert!(app.is_warmup_in_flight("warm"));
        assert_eq!(
            app.warmup_tasks.values().next().map(|task| task.origin),
            Some(WarmupOrigin::Automatic)
        );
        assert!(!app.refreshing_requests.contains_key("warm"));
        assert_eq!(
            app.refreshing_requests
                .get("refresh")
                .map(|(_, refresh)| *refresh),
            Some(Refresh::Unattended)
        );
        assert!(app.workspace_requests.is_empty());

        app.drain_credential_tasks().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_automatic_warmup_preserves_the_single_unattended_refresh() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        app.workspace_states.insert(
            "acct_test".into(),
            super::WorkspaceMemoryResolution {
                state: crate::cache::WorkspaceState::Absent,
                fresh_until: Instant::now() + std::time::Duration::from_secs(60),
            },
        );
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.warmup_tasks.insert(
            0,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Automatic,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: tokio::spawn(async { Err("injected failure".to_string()) }),
            },
        );
        while !app.warmup_tasks[&0].handle.is_finished() {
            tokio::task::yield_now().await;
        }

        app.poll_warmup_results().await;

        assert_eq!(
            app.refreshing_requests
                .get("account")
                .map(|(_, refresh)| *refresh),
            Some(Refresh::Unattended)
        );
        assert!(app.workspace_requests.is_empty());
        assert!(app.status_is_error);
        app.drain_credential_tasks().await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn failed_oauth_attempt_reloads_a_partially_committed_profile() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let auth = managed_auth(
            "account@example.com",
            "acct_account",
            "replacement-access",
            "replacement-refresh",
        );
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(&profile_path, &auth);
        write_auth_durable(&codex_home.join("auth.json"), &auth);

        let mut app = App::new();
        app.startup_auth_state = super::StartupAuthState::Blocked;
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        app.finish_oauth_attempt(
            Err(anyhow::anyhow!(
                "live-auth update failed after profile commit"
            )),
            &["account".to_string()].into_iter().collect(),
        )
        .await;

        assert_eq!(app.startup_auth_state, super::StartupAuthState::Ready);
        assert!(app.status_is_error);
        assert_eq!(app.accounts.len(), 1);
        assert!(app.accounts[0].is_current);
        assert!(app.refreshing_requests.contains_key("account"));
        app.drain_credential_tasks().await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn missing_live_auth_clears_the_marker_only_startup_selection() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth("account@example.com", "acct_account", "access", "refresh"),
        );
        std::fs::write(home.path().join("current"), "account").unwrap();

        let mut app = App::new();
        app.load_profiles_from_marker();
        assert!(app.accounts[0].is_current);
        app.start_startup_auth_reconciliation();
        let outcome = loop {
            let outcome = app.poll_startup_auth_reconciliation().await;
            if outcome != super::StartupAuthPoll::Pending {
                break outcome;
            }
            tokio::task::yield_now().await;
        };

        assert_eq!(outcome, super::StartupAuthPoll::Ready);
        assert!(!app.accounts[0].is_current);
        assert_eq!(app.startup_auth_state, super::StartupAuthState::Ready);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn completed_warmup_preflight_waits_for_profile_switch_to_finish() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth("account@example.com", "acct_account", "access", "refresh"),
        );
        let _held_lease = profile::acquire_profile_lease("account").unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info_for("account@example.com", "acct_account"),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.warmup_preflight = Some(super::WarmupPreflightTask {
            origin: super::WarmupPreflightOrigin::Single {
                alias: "account".into(),
            },
            candidate_count: 1,
            aliases: ["account".to_string()].into_iter().collect(),
            control: crate::cache::CacheLockAcquireControl::new(),
            handle: tokio::spawn(async {
                Ok(Some(vec![super::WarmupReadyCandidate {
                    alias: "account".to_string(),
                    binding: StrictAccountBinding {
                        account_id: "acct_account".to_string(),
                        email: "account@example.com".to_string(),
                    },
                    cached_usage: None,
                }]))
            }),
        });
        let (release_switch, wait_for_release) = tokio::sync::oneshot::channel();
        app.track_account_task(
            "target".into(),
            AccountTaskKind::SwitchPrepare,
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(async move {
                let _ = wait_for_release.await;
            }),
        );
        while !app
            .warmup_preflight
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            tokio::task::yield_now().await;
        }

        app.poll_warmup_preflight_result().await;

        assert!(app.warmup_preflight.is_some());
        assert!(app.warmup_tasks.is_empty());

        release_switch.send(()).unwrap();
        while app.profile_switch_in_flight() {
            tokio::task::yield_now().await;
            app.poll_account_tasks().await;
        }
        app.poll_warmup_preflight_result().await;

        assert!(app.warmup_preflight.is_none());
        assert_eq!(app.warmup_tasks.len(), 1);
        app.drain_credential_tasks().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_warmup_refreshes_usage_without_workspace_lookup() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.warmup_tasks.insert(
            0,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: tokio::spawn(async { Ok(()) }),
            },
        );
        while !app.warmup_tasks[&0].handle.is_finished() {
            tokio::task::yield_now().await;
        }

        app.poll_warmup_results().await;

        assert_eq!(
            app.refreshing_requests
                .get("account")
                .map(|(_, refresh)| *refresh),
            Some(Refresh::Forced)
        );
        assert!(app.workspace_requests.is_empty());
        app.drain_credential_tasks().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_warmup_defers_usage_refresh_until_profile_switch_finishes() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "unrelated".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.warmup_tasks.insert(
            0,
            WarmupTask {
                alias: "unrelated".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: tokio::spawn(async { Ok(()) }),
            },
        );
        let (release_switch, wait_for_release) = tokio::sync::oneshot::channel();
        app.track_account_task(
            "target".into(),
            AccountTaskKind::SwitchPrepare,
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(async move {
                let _ = wait_for_release.await;
            }),
        );
        while !app.warmup_tasks[&0].handle.is_finished() {
            tokio::task::yield_now().await;
        }

        app.poll_warmup_results().await;

        assert_eq!(
            app.deferred_post_switch_usage_refreshes.get("unrelated"),
            Some(&AccountRefreshPlan::usage_only(Refresh::Forced))
        );
        assert!(!app.refreshing_requests.contains_key("unrelated"));
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("queued until the profile switch finishes"))
        );

        release_switch.send(()).unwrap();
        while app.profile_switch_in_flight() {
            tokio::task::yield_now().await;
            app.poll_account_tasks().await;
        }
        app.poll_profile_switch_results();

        assert!(app.deferred_post_switch_usage_refreshes.is_empty());
        assert!(app.refreshing_requests.contains_key("unrelated"));
        assert!(app.workspace_requests.is_empty());
        app.drain_credential_tasks().await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn switch_cancellations_resume_target_and_active_usage_once() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        for alias in ["target", "current"] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(
                &path,
                &managed_auth(
                    "test@example.com",
                    "acct_test",
                    &format!("{alias}-access"),
                    &format!("{alias}-refresh"),
                ),
            );
        }

        let mut app = App::new();
        add_test_account(&mut app, "target");
        add_test_account(&mut app, "current");
        app.view_indices.extend([0, 1]);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        for (request_id, alias, refresh) in [
            (7, "target", Refresh::Forced),
            (8, "current", Refresh::Unattended),
        ] {
            app.refreshing_requests
                .insert(alias.to_string(), (request_id, refresh));
            app.usage_generations.insert(alias.to_string(), request_id);
            app.accounts
                .iter_mut()
                .find(|account| account.alias == alias)
                .unwrap()
                .usage = UsageStatus::Loading;

            let usage_work = crate::usage::UsageTaskCancellation::new();
            assert!(usage_work.request());
            usage_work.mark_cancellation_completed();
            app.track_account_task_with_controls(
                alias.to_string(),
                AccountTaskKind::Usage { request_id },
                profile::ProfileLeaseAcquireControl::new(),
                AccountTaskControls {
                    usage_work: Some(usage_work),
                    ..AccountTaskControls::default()
                },
                tokio::spawn(async {}),
            );
        }

        // The target was already queued by `switch_selected`. Observing its
        // worker cancellation must merge with that intent, not schedule a
        // second post-switch generation.
        app.defer_post_switch_usage_refresh(
            "target".into(),
            AccountRefreshPlan::resume_cancelled_usage(Refresh::Forced),
        );
        let (release_switch, wait_for_release) = tokio::sync::oneshot::channel();
        app.track_account_task(
            "target".into(),
            AccountTaskKind::SwitchSync,
            profile::ProfileLeaseAcquireControl::new(),
            tokio::spawn(async move {
                let _ = wait_for_release.await;
            }),
        );

        while app
            .account_tasks
            .values()
            .filter(|task| matches!(task.kind, AccountTaskKind::Usage { .. }))
            .any(|task| !task.handle.is_finished())
        {
            tokio::task::yield_now().await;
        }
        app.poll_account_tasks().await;

        assert!(app.refreshing_requests.is_empty());
        assert_eq!(app.deferred_post_switch_usage_refreshes.len(), 2);
        assert_eq!(
            app.deferred_post_switch_usage_refreshes.get("target"),
            Some(&AccountRefreshPlan::usage_only(Refresh::Forced))
        );
        assert_eq!(
            app.deferred_post_switch_usage_refreshes.get("current"),
            Some(&AccountRefreshPlan::usage_only(Refresh::Unattended))
        );

        let generation_before_resume = app.usage_next_id;
        release_switch.send(()).unwrap();
        while app.profile_switch_in_flight() {
            tokio::task::yield_now().await;
            app.poll_account_tasks().await;
        }
        app.poll_profile_switch_results();

        assert!(app.deferred_post_switch_usage_refreshes.is_empty());
        assert_eq!(app.usage_next_id, generation_before_resume.wrapping_add(2));
        assert_eq!(app.refreshing_requests.len(), 2);
        assert!(app.refreshing_requests.contains_key("target"));
        assert!(app.refreshing_requests.contains_key("current"));
        app.drain_credential_tasks().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_account_task_panic_is_reported() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (7, Refresh::Cached));
        app.usage_generations.insert("account".into(), 7);
        app.pending_usage_refreshes.insert(
            "account".into(),
            AccountRefreshPlan::workspace_only(WorkspaceRefresh::IfStale),
        );

        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            task_control.cancelled().await;
            panic!("cancelled usage task panic");
        });
        app.track_account_task(
            "account".into(),
            AccountTaskKind::Usage { request_id: 7 },
            lease_control.clone(),
            handle,
        );
        assert!(lease_control.cancel_waiting());

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.account_tasks.is_empty() {
                tokio::task::yield_now().await;
                app.poll_account_tasks().await;
            }
        })
        .await
        .expect("cancelled account task must settle");

        assert!(matches!(app.accounts[0].usage, UsageStatus::Error(_)));
        assert_eq!(app.usage_lease_release_generations.get("account"), Some(&7));
        assert!(!app.pending_usage_refreshes.contains_key("account"));
        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("panicked"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn usage_task_panic_after_core_delivery_records_the_latest_lease_release() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.accounts[0].usage = UsageStatus::Loaded(Box::default());
        app.usage_generations.insert("account".into(), 7);

        let handle = tokio::spawn(async {
            panic!("usage task panicked after delivering its core result");
        });
        app.track_account_task(
            "account".into(),
            AccountTaskKind::Usage { request_id: 7 },
            profile::ProfileLeaseAcquireControl::new(),
            handle,
        );
        while !app
            .account_tasks
            .values()
            .all(|task| task.handle.is_finished())
        {
            tokio::task::yield_now().await;
        }

        app.poll_account_tasks().await;

        assert_eq!(app.usage_lease_release_generations.get("account"), Some(&7));
        assert!(app.startup_core_refreshes_settled(&["account".to_string()]));
        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn safely_cancelled_usage_task_records_release_after_active_marker_is_gone() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.usage_generations.insert("account".into(), 9);

        let network_wait = SafeTaskCancellation::new();
        let worker_network_wait = network_wait.clone();
        let handle = tokio::spawn(async move {
            worker_network_wait.cancelled().await;
            worker_network_wait.mark_completed();
        });
        app.track_account_task_with_controls(
            "account".into(),
            AccountTaskKind::Usage { request_id: 9 },
            profile::ProfileLeaseAcquireControl::new(),
            AccountTaskControls {
                network_wait: Some(network_wait.clone()),
                ..AccountTaskControls::default()
            },
            handle,
        );
        assert!(network_wait.request());
        while !app
            .account_tasks
            .values()
            .all(|task| task.handle.is_finished())
        {
            tokio::task::yield_now().await;
        }

        app.poll_account_tasks().await;

        assert_eq!(app.usage_lease_release_generations.get("account"), Some(&9));
        assert!(app.startup_core_refreshes_settled(&["account".to_string()]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_warmup_task_panic_is_reported() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_control = lease_control.clone();
        let handle: tokio::task::JoinHandle<std::result::Result<(), String>> =
            tokio::spawn(async move {
                task_control.cancelled().await;
                panic!("cancelled warmup task panic");
            });
        app.warmup_tasks.insert(
            0,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: lease_control.clone(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle,
            },
        );
        assert!(lease_control.cancel_waiting());

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.warmup_tasks.is_empty() {
                tokio::task::yield_now().await;
                app.poll_warmup_results().await;
            }
        })
        .await
        .expect("cancelled warmup task must settle");

        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("panicked"))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_account_menu_alias_never_switches_the_selected_fallback_row() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "remaining".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        dispatch_menu_use(&mut app, "removed");

        assert!(app.account_tasks.is_empty());
        assert_eq!(app.selected, 0);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("no longer available"))
        );
        assert!(app.status_is_error);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn refresh_one_reuses_an_in_flight_model_generation() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth("account@example.com", "acct_account", "access", "refresh"),
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info_for("account@example.com", "acct_account"),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.ensure_models_loaded("account");
        let initial_request_id = app.model_requests["account"];

        app.refresh_one("account");

        assert_eq!(app.model_requests["account"], initial_request_id);
        assert_eq!(
            app.account_tasks
                .values()
                .filter(|task| matches!(task.kind, AccountTaskKind::Model { .. }))
                .count(),
            1
        );
        app.drain_credential_tasks().await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn refresh_one_defers_new_model_discovery_until_quota_settles() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth("account@example.com", "acct_account", "access", "refresh"),
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info_for("account@example.com", "acct_account"),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        app.refresh_one("account");

        assert!(!app.model_cache.contains_key("account"));
        assert!(app.model_requests.is_empty());
        assert!(
            app.account_tasks
                .values()
                .all(|task| !matches!(task.kind, AccountTaskKind::Model { .. }))
        );
        app.drain_credential_tasks().await;
    }

    #[test]
    fn background_model_discovery_waits_only_for_the_selected_core_boundary() {
        let mut app = App::new();
        app.accounts = vec![
            AccountEntry {
                alias: "selected".into(),
                info: test_account_info(),
                usage: UsageStatus::Loaded(Box::default()),
                is_current: true,
            },
            AccountEntry {
                alias: "other".into(),
                info: test_account_info_for("other@example.com", "acct_other"),
                usage: UsageStatus::Loading,
                is_current: false,
            },
        ];
        app.view_indices.extend([0, 1]);
        app.usage_generations.insert("selected".into(), 7);
        app.refreshing_requests
            .insert("other".into(), (11, Refresh::Unattended));

        assert!(!app.selected_core_usage_is_settled());
        app.record_usage_lease_release("selected", 7);
        assert!(app.selected_core_usage_is_settled());

        app.accounts[0].usage = UsageStatus::Loading;
        assert!(!app.selected_core_usage_is_settled());
        app.accounts[0].usage = UsageStatus::Error(super::UsageError {
            summary: "unavailable".into(),
            detail: "unavailable".into(),
        });
        assert!(app.selected_core_usage_is_settled());

        app.refreshing_requests
            .insert("selected".into(), (7, Refresh::Unattended));
        assert!(!app.selected_core_usage_is_settled());
        app.refreshing_requests.remove("selected");
        assert!(app.selected_core_usage_is_settled());
        assert!(app.refreshing_requests.contains_key("other"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn explicit_detail_request_can_start_models_while_other_quota_is_pending() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/selected/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth("test@example.com", "acct_test", "access", "refresh"),
        );

        let mut app = App::new();
        app.detail_visible = false;
        app.accounts = vec![
            AccountEntry {
                alias: "selected".into(),
                info: test_account_info(),
                usage: UsageStatus::Loaded(Box::default()),
                is_current: true,
            },
            AccountEntry {
                alias: "other".into(),
                info: test_account_info_for("other@example.com", "acct_other"),
                usage: UsageStatus::Loading,
                is_current: false,
            },
        ];
        app.view_indices.extend([0, 1]);
        app.refreshing_requests
            .insert("other".into(), (11, Refresh::Unattended));
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));

        app.toggle_detail_panel();

        assert!(app.detail_visible);
        assert!(app.model_requests.contains_key("selected"));
        assert!(app.account_tasks.values().any(|task| {
            matches!(task.kind, AccountTaskKind::Model { .. }) && task.alias == "selected"
        }));
        app.drain_credential_tasks().await;
    }

    #[test]
    fn update_check_does_not_build_an_independent_client() {
        crate::config::init_defaults_for_tests();
        let mut app = App::with_http_client(None);

        app.start_update_check();

        assert!(app.update_rx.is_none());
        assert!(app.update_available.is_none());
    }

    #[test]
    fn stopped_batch_distinguishes_cancelled_from_completed_accounts() {
        assert_eq!(batch_relogin_not_attempted(3, 1, 0, 1), 1);
        assert_eq!(batch_relogin_not_attempted(3, 1, 0, 0), 2);
        assert_eq!(batch_relogin_not_attempted(3, 1, 1, 0), 1);
    }

    #[test]
    fn global_weekly_summary_uses_all_accounts_not_only_filtered_view() {
        let now = 1_000_000;
        let weekly = |used_percent, elapsed_percent: i64| UsageInfo {
            fetched_at: Some(now),
            secondary: Some(WindowUsage {
                used_percent: Some(used_percent),
                resets_at: Some(
                    now + crate::usage::WINDOW_7D_SECS
                        - crate::usage::WINDOW_7D_SECS * elapsed_percent / 100,
                ),
                window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
            }),
            ..UsageInfo::default()
        };
        let mut app = App::new();
        app.accounts = vec![
            AccountEntry {
                alias: "visible".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(weekly(50.0, 50))),
                is_current: true,
            },
            AccountEntry {
                alias: "filtered-out".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(weekly(100.0, 95))),
                is_current: false,
            },
            AccountEntry {
                alias: "unavailable".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Idle,
                is_current: false,
            },
        ];
        app.view_indices = vec![0];

        let summary = app.global_weekly_summary(now);

        assert_eq!(summary.included_accounts, 2);
        assert_eq!(summary.excluded_accounts, 1);
        assert!((summary.effective_capacity - 195.0).abs() < 1e-9);
        assert_eq!(summary.next_reset_alias.as_deref(), Some("filtered-out"));
    }

    #[test]
    fn global_weekly_summary_keeps_loaded_snapshots_until_the_weekly_window_expires() {
        let now = 1_000_000;
        let mut app = App::new();
        let cache_ttl = i64::try_from(crate::config::get().cache.ttl)
            .expect("default cache TTL fits the timestamp model");
        let weekly = |fetched_at, resets_at| UsageInfo {
            fetched_at: Some(fetched_at),
            secondary: Some(WindowUsage {
                used_percent: Some(50.0),
                resets_at: Some(resets_at),
                window_minutes: Some(crate::usage::WINDOW_7D_SECS / 60),
            }),
            ..UsageInfo::default()
        };
        app.accounts = vec![
            AccountEntry {
                alias: "loaded-before-cache-ttl".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(weekly(
                    now - cache_ttl - 1,
                    now + crate::usage::WINDOW_7D_SECS / 2,
                ))),
                is_current: false,
            },
            AccountEntry {
                alias: "elapsed-weekly-window".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(weekly(now, now))),
                is_current: false,
            },
        ];

        let summary = app.global_weekly_summary(now);

        assert_eq!(summary.included_accounts, 1);
        assert_eq!(summary.excluded_accounts, 1);
        assert_eq!(
            summary.next_reset_alias.as_deref(),
            Some("loaded-before-cache-ttl")
        );
    }

    #[test]
    fn status_sort_groups_limited_before_available_independent_of_plan() {
        let weekly_only = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(20.0),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };
        let complete = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(20.0),
                ..WindowUsage::default()
            }),
            ..weekly_only.clone()
        };
        let plus = || AccountInfo {
            plan_type: Some("plus".to_string()),
            ..AccountInfo::default()
        };
        let mut app = App::new();
        app.accounts = vec![
            AccountEntry {
                alias: "free-complete".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(weekly_only.clone())),
                is_current: false,
            },
            AccountEntry {
                alias: "plus-limited".into(),
                info: plus(),
                usage: UsageStatus::Loaded(Box::new(UsageInfo {
                    account_limited: true,
                    ..weekly_only
                })),
                is_current: false,
            },
            AccountEntry {
                alias: "plus-complete".into(),
                info: plus(),
                usage: UsageStatus::Loaded(Box::new(complete)),
                is_current: false,
            },
        ];
        app.sort_mode = SortMode::Status;

        app.update_view();

        let aliases = app
            .view_indices
            .iter()
            .map(|&index| app.accounts[index].alias.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            aliases,
            vec!["plus-limited", "free-complete", "plus-complete"]
        );
    }

    #[test]
    fn quota_sort_uses_valid_short_then_weekly_without_plan_shape_assumptions() {
        let weekly = |used_percent| WindowUsage {
            used_percent: Some(used_percent),
            ..WindowUsage::default()
        };
        let plus = || AccountInfo {
            plan_type: Some("plus".to_string()),
            ..AccountInfo::default()
        };
        let mut app = App::new();
        app.accounts = vec![
            AccountEntry {
                alias: "weekly-missing".into(),
                info: plus(),
                usage: UsageStatus::Loaded(Box::new(UsageInfo {
                    primary: Some(weekly(5.0)),
                    ..UsageInfo::default()
                })),
                is_current: false,
            },
            AccountEntry {
                alias: "short-preferred".into(),
                info: plus(),
                usage: UsageStatus::Loaded(Box::new(UsageInfo {
                    primary: Some(weekly(30.0)),
                    secondary: Some(weekly(1.0)),
                    ..UsageInfo::default()
                })),
                is_current: false,
            },
            AccountEntry {
                alias: "weekly-only-pro".into(),
                info: plus(),
                usage: UsageStatus::Loaded(Box::new(UsageInfo {
                    plan_type: Some("pro".to_string()),
                    secondary: Some(weekly(20.0)),
                    ..UsageInfo::default()
                })),
                is_current: false,
            },
            AccountEntry {
                alias: "weekly-only-unknown".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::new(UsageInfo {
                    plan_type: Some("future_plan".to_string()),
                    secondary: Some(weekly(10.0)),
                    ..UsageInfo::default()
                })),
                is_current: false,
            },
            AccountEntry {
                alias: "weekly-after-invalid-short".into(),
                info: plus(),
                usage: UsageStatus::Loaded(Box::new(UsageInfo {
                    plan_type: Some("pro".to_string()),
                    primary: Some(weekly(101.0)),
                    secondary: Some(weekly(15.0)),
                    ..UsageInfo::default()
                })),
                is_current: false,
            },
        ];
        app.sort_mode = SortMode::Quota;

        app.update_view();

        let aliases = app
            .view_indices
            .iter()
            .map(|&index| app.accounts[index].alias.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            aliases,
            vec![
                "weekly-only-unknown",
                "weekly-after-invalid-short",
                "weekly-only-pro",
                "short-preferred",
                "weekly-missing",
            ]
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn warmup_cache_failure_remains_visible_and_starts_no_task() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        std::fs::write(home.path().join("cache.json"), b"{not valid json")
            .expect("write malformed usage cache");

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "broken-cache".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });

        app.warmup_one("broken-cache");

        assert!(app.warmup_preflight.is_some());
        assert!(app.warmup_tasks.is_empty());
        finish_warmup_preflight(&mut app).await;

        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("Could not inspect usage state")),
            "unexpected warmup status: {:?}",
            app.status_msg
        );
        assert!(app.warmup_tasks.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn blocked_cache_preflight_keeps_the_ui_path_nonblocking_and_starts_no_partial_batch() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let cache_lock_path = home.path().join("cache.lock");
        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&cache_lock_path)
            .unwrap();
        fs4::FileExt::lock(&cache_lock).unwrap();

        let mut app = App::new();
        app.accounts = vec![
            AccountEntry {
                alias: "loaded-ready".into(),
                info: test_account_info(),
                usage: UsageStatus::Loaded(Box::default()),
                is_current: false,
            },
            AccountEntry {
                alias: "disk-waiting".into(),
                info: test_account_info(),
                usage: UsageStatus::Idle,
                is_current: false,
            },
        ];
        app.marked = ["loaded-ready".to_string(), "disk-waiting".to_string()]
            .into_iter()
            .collect();

        app.warmup_marked();

        assert!(app.warmup_preflight.is_some());
        assert!(app.warmup_tasks.is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        app.poll_warmup_preflight_result().await;
        assert!(app.warmup_preflight.is_some());
        assert!(app.warmup_tasks.is_empty());

        fs4::FileExt::unlock(&cache_lock).unwrap();
        finish_warmup_preflight(&mut app).await;

        assert!(app.warmup_preflight.is_none());
        assert_eq!(app.warmup_tasks.len(), 2);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.drain_credential_tasks(),
        )
        .await
        .expect("spawned warmups must drain");
    }

    #[tokio::test]
    async fn simultaneous_stop_is_recorded_after_completed_batch_round() {
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let (result, stop_requested, signal_error) = finish_login_or_stop_after_round(
            async { Ok("saved") },
            async { Ok(()) },
            &lease_control,
        )
        .await;

        assert_eq!(result.unwrap(), "saved");
        assert!(stop_requested);
        assert!(signal_error.is_none());
    }

    #[tokio::test]
    async fn stop_request_waits_for_the_current_batch_round_to_finish() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let login = async move {
            release_rx.await.expect("test releases the login round");
            anyhow::Ok("saved")
        };
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let completion = finish_login_or_stop_after_round(login, async { Ok(()) }, &lease_control);
        tokio::pin!(completion);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut completion)
                .await
                .is_err(),
            "a stop request must not drop the current login round"
        );
        release_tx.send(()).expect("login receiver remains alive");
        let (result, stop_requested, signal_error) = completion.await;

        assert_eq!(result.unwrap(), "saved");
        assert!(stop_requested);
        assert!(signal_error.is_none());
    }

    #[tokio::test]
    async fn shutdown_drain_waits_for_started_account_work() {
        let mut app = App::new();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_by_task = completed.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        app.track_account_task(
            "account".into(),
            AccountTaskKind::ResetCard,
            profile::ProfileLeaseAcquireControl::new(),
            handle,
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.drain_credential_tasks(),
        )
        .await
        .expect("drain must finish once the tracked task finishes");

        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(app.account_tasks.is_empty());
        assert!(app.shutting_down);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn account_task_panics_are_redacted_and_reported_deterministically() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let mut app = App::new();
        for (alias, secret) in [
            ("z-account", "SECRET_Z_ACCOUNT_TOKEN"),
            ("a-account", "SECRET_A_ACCOUNT_TOKEN"),
        ] {
            app.reset_cards_in_flight.insert(alias.to_string());
            let handle = tokio::spawn(async move { panic!("{secret}") });
            app.track_account_task(
                alias.to_string(),
                AccountTaskKind::ResetCard,
                profile::ProfileLeaseAcquireControl::new(),
                handle,
            );
        }

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while app
                .account_tasks
                .values()
                .any(|task| !task.handle.is_finished())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicking account tasks must finish");
        app.poll_account_tasks().await;

        let status = app
            .status_msg
            .as_deref()
            .expect("join failures are visible");
        assert!(status.contains("Account tasks stopped"), "{status}");
        assert!(status.contains("worker panicked"), "{status}");
        assert!(!status.contains("SECRET_A_ACCOUNT_TOKEN"), "{status}");
        assert!(!status.contains("SECRET_Z_ACCOUNT_TOKEN"), "{status}");
        assert!(
            status.find("a-account").unwrap() < status.find("z-account").unwrap(),
            "{status}"
        );
        assert!(app.account_tasks.is_empty());
        assert!(app.reset_cards_in_flight.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reset_card_task_panic_after_lease_invalidates_cache_and_warns_before_retry() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        crate::cache::put_bound_versioned(
            "account",
            &test_binding(),
            &UsageInfo {
                fetched_at: Some(
                    crate::auth::now_unix_secs()
                        .expect("test clock must be a supported Unix timestamp"),
                ),
                reset_credits: vec![ResetCredit {
                    id: "possibly-consumed".into(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..UsageInfo::default()
            },
        )
        .unwrap();
        assert!(
            crate::cache::get_bound("account", &test_binding())
                .unwrap()
                .is_some()
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.view_indices.push(0);
        app.reset_cards_in_flight.insert("account".into());
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let _lease = profile::acquire_profile_lease_async_cancellable(
                "account".to_string(),
                &task_control,
            )
            .await
            .unwrap()
            .expect("test task acquires the lease before the simulated post-request panic");
            panic!("panic after reset-card POST");
        });
        app.track_account_task(
            "account".into(),
            AccountTaskKind::ResetCard,
            lease_control,
            handle,
        );

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while app
                .account_tasks
                .values()
                .any(|task| !task.handle.is_finished())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("simulated reset-card worker must stop");
        app.poll_account_tasks().await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.usage_cache_invalidation_tasks.is_empty() {
                tokio::task::yield_now().await;
                app.poll_usage_cache_invalidations().await;
            }
        })
        .await
        .expect("identity-bound cache invalidation must finish");

        assert!(
            crate::cache::get_bound("account", &test_binding())
                .unwrap()
                .is_none()
        );
        assert!(!app.reset_cards_in_flight.contains("account"));
        assert!(matches!(&app.accounts[0].usage, UsageStatus::Error(_)));
        assert!(app.status_is_error);
        let status = app.status_msg.as_deref().unwrap();
        assert!(status.contains("consumption may have occurred"), "{status}");
        assert!(status.contains("verify before retry"), "{status}");
        assert!(!status.contains("panic after reset-card POST"), "{status}");
    }

    #[tokio::test]
    async fn event_loop_error_drains_started_account_work_and_preserves_the_error() {
        let mut app = App::new();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_by_task = completed.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        app.track_account_task(
            "account".into(),
            AccountTaskKind::ResetCard,
            profile::ProfileLeaseAcquireControl::new(),
            handle,
        );

        let error = drain_credential_tasks_on_error::<()>(
            &mut app,
            Err(anyhow::anyhow!("terminal read failed")),
        )
        .await
        .expect_err("the original event-loop error must be returned");

        assert_eq!(error.to_string(), "terminal read failed");
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(app.account_tasks.is_empty());
        assert!(app.shutting_down);
    }

    #[tokio::test]
    async fn event_loop_error_drains_started_post_draw_cleanup_and_preserves_the_error() {
        let mut app = App::new();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_by_task = completed.clone();
        app.startup_self_update_cleanup_task = Some(StartupSelfUpdateCleanupTask {
            handle: tokio::task::spawn_blocking(move || {
                release_rx.recv().expect("release cleanup fixture");
                completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(false)
            }),
        });
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            release_tx.send(()).expect("release cleanup task");
        });

        let error = drain_credential_tasks_on_error::<()>(
            &mut app,
            Err(anyhow::anyhow!("terminal draw failed")),
        )
        .await
        .expect_err("the original event-loop error must be returned");

        assert_eq!(error.to_string(), "terminal draw failed");
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(app.startup_self_update_cleanup_task.is_none());
    }

    #[tokio::test]
    async fn failed_post_draw_file_log_initialization_is_surfaced_safely() {
        let mut app = App::new();
        let hostile = format!(
            "log path rejected\u{1b}]52;c;clipboard\u{7}\n{}",
            "x".repeat(STATUS_MESSAGE_MAX_CHARS + 100)
        );
        app.file_log_writer = Some(crate::logging::deferred_file_log_writer());
        app.startup_file_log_init_task = Some(StartupFileLogInitTask {
            handle: tokio::task::spawn_blocking(move || Err(anyhow::anyhow!(hostile))),
        });
        while !app
            .startup_file_log_init_task
            .as_ref()
            .is_some_and(|task| task.handle.is_finished())
        {
            tokio::task::yield_now().await;
        }

        app.poll_startup_maintenance().await;
        app.set_status("Synchronizing live Codex credentials...".to_string(), 60);
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Synchronizing live Codex credentials...")
        );
        app.present_startup_maintenance_warnings();

        let status = app.status_msg.as_deref().expect("file-log warning");
        assert!(status.starts_with("Warning: file logging is unavailable:"));
        assert!(status.chars().all(|character| !character.is_control()));
        assert!(status.chars().count() <= STATUS_MESSAGE_MAX_CHARS);
        assert!(app.status_is_error);
        assert!(app.startup_file_log_init_task.is_none());
        assert!(app.file_log_writer.is_none());
    }

    #[tokio::test]
    async fn late_first_write_file_log_initialization_failure_is_surfaced_once() {
        use std::io::Write as _;
        use tracing_subscriber::fmt::MakeWriter as _;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let occupied = root.path().join("occupied");
        std::fs::write(&occupied, b"not a directory").unwrap();
        let writer = crate::logging::FileLogWriter::deferred_for_directory(occupied);
        writer
            .finish_deferred_initialization()
            .expect("a record-free post-frame transition must stay lazy");

        let mut app = App::new();
        app.file_log_writer = Some(writer.clone());
        writer
            .make_writer()
            .write_all(b"late enabled record\n")
            .expect_err("the unusable log directory must reject its first write");

        app.poll_startup_maintenance().await;
        app.present_startup_maintenance_warnings();
        let first_warning = app.status_msg.clone().expect("late file-log warning");
        assert!(first_warning.starts_with("Warning: file logging is unavailable:"));
        assert!(app.status_is_error);
        assert!(app.file_log_writer.is_none());

        app.poll_startup_maintenance().await;
        assert!(app.pending_startup_maintenance_warnings.is_empty());
        assert_eq!(app.status_msg.as_deref(), Some(first_warning.as_str()));
        assert_eq!(writer.take_initialization_error().unwrap(), None);
    }

    #[tokio::test]
    async fn event_loop_error_preserves_a_queued_maintenance_warning_after_restore() {
        let mut app = App::new();
        let warning_sink = Arc::clone(&app.startup_exit_warnings);
        app.queue_startup_maintenance_warnings(vec![
            "Warning: file logging is unavailable: rejected path".to_string(),
        ]);

        drain_credential_tasks_on_error::<()>(
            &mut app,
            Err(anyhow::anyhow!("terminal event failed")),
        )
        .await
        .expect_err("the original event-loop error must be returned");

        assert!(app.pending_startup_maintenance_warnings.is_empty());
        let warnings = warning_sink
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            "Warning: file logging is unavailable: rejected path"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_cancels_a_task_contended_before_profile_lease_acquisition() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let held_lease = crate::profile::acquire_profile_lease("account").unwrap();

        let mut app = App::new();
        app.reset_cards_in_flight.insert("account".into());
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_control = lease_control.clone();
        let (attempted_tx, attempted_rx) = tokio::sync::oneshot::channel();
        let acquired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let acquired_by_task = acquired.clone();
        let handle = tokio::spawn(async move {
            attempted_tx.send(()).unwrap();
            if crate::profile::acquire_profile_lease_async_cancellable("account", &task_control)
                .await
                .unwrap()
                .is_some()
            {
                acquired_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        attempted_rx.await.expect("lease attempt started");
        app.track_account_task(
            "account".into(),
            AccountTaskKind::ResetCard,
            lease_control,
            handle,
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.drain_credential_tasks(),
        )
        .await
        .expect("shutdown must cancel a task that is only waiting for its lease");

        assert!(!acquired.load(std::sync::atomic::Ordering::SeqCst));
        assert!(app.account_tasks.is_empty());
        assert!(app.reset_cards_in_flight.is_empty());
        drop(held_lease);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn profile_switch_is_tracked_and_shutdown_cancels_a_prelease_prepare() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "saved-access",
                "saved-refresh",
            ),
        );
        let live_path = codex_home.join("auth.json");
        write_auth_durable(
            &live_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "saved-access",
                "saved-refresh",
            ),
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        let held_lease = crate::profile::acquire_profile_lease("account").unwrap();
        app.switch_selected();
        assert!(
            app.account_tasks
                .values()
                .any(|task| matches!(task.kind, AccountTaskKind::SwitchPrepare))
        );
        tokio::task::yield_now().await;

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.drain_credential_tasks(),
        )
        .await
        .expect("shutdown must cancel a switch prepare still waiting for its profile lease");

        assert!(app.account_tasks.is_empty());
        assert_eq!(
            crate::auth::read_auth(&live_path).unwrap(),
            managed_auth(
                "account@example.com",
                "acct_account",
                "saved-access",
                "saved-refresh",
            )
        );
        assert!(!app.accounts[0].is_current);
        drop(held_lease);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn profile_switch_gate_rejects_a_second_alias_while_prepare_is_in_flight() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();
        for alias in ["first", "second"] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(
                &path,
                &managed_auth(
                    &format!("{alias}@example.com"),
                    &format!("acct_{alias}"),
                    &format!("{alias}-access"),
                    &format!("{alias}-refresh"),
                ),
            );
        }

        let mut app = App::new();
        for alias in ["first", "second"] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Idle,
                is_current: false,
            });
        }
        app.accounts.push(AccountEntry {
            alias: "incomplete".into(),
            info: AccountInfo {
                account_id: Some("acct_test".into()),
                email: None,
                ..AccountInfo::default()
            },
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.extend([0, 1, 2]);

        app.switch_selected();
        app.selected = 1;
        app.switch_selected();

        let switch_tasks = app
            .account_tasks
            .values()
            .filter(|task| {
                matches!(
                    task.kind,
                    AccountTaskKind::SwitchPrepare
                        | AccountTaskKind::SwitchSync
                        | AccountTaskKind::SwitchCommit
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(switch_tasks.len(), 1);
        assert_eq!(switch_tasks[0].alias, "first");
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("before switching again"))
        );

        app.drain_credential_tasks().await;
        assert!(app.confirm.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn profile_switch_never_replaces_an_existing_confirmation() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();
        for alias in ["switch-target", "delete-target"] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(
                &path,
                &managed_auth(
                    &format!("{alias}@example.com"),
                    &format!("acct_{alias}"),
                    &format!("{alias}-access"),
                    &format!("{alias}-refresh"),
                ),
            );
        }
        write_auth_durable(
            &codex_home.join("auth.json"),
            &serde_json::json!({
                "tokens": {
                    "access_token": "untracked-access",
                    "refresh_token": "untracked-refresh"
                }
            }),
        );

        let mut app = App::new();
        for alias in ["switch-target", "delete-target"] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Idle,
                is_current: false,
            });
        }
        app.view_indices.extend([0, 1]);

        app.request_delete_alias("delete-target");
        app.switch_selected();
        assert!(matches!(
            app.confirm,
            Some(ConfirmAction::Delete(ref alias)) if alias == "delete-target"
        ));
        assert!(!app.profile_switch_in_flight());

        app.confirm = None;
        app.switch_selected();
        assert!(app.profile_switch_in_flight());
        app.confirm = Some(ConfirmAction::Delete("delete-target".into()));
        settle_profile_switch(&mut app).await;

        assert!(matches!(
            app.confirm,
            Some(ConfirmAction::Delete(ref alias)) if alias == "delete-target"
        ));
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("not saved"))
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn profile_switch_prepare_keeps_event_loop_responsive_during_auth_lock_contention() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "saved-access",
                "saved-refresh",
            ),
        );

        let auth_lock = crate::profile::lock_live_auth().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            drop(auth_lock);
        });
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        app.switch_selected();
        let responsive = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        })
        .await;
        assert!(
            responsive.is_ok(),
            "auth-lock contention must stay on the blocking pool"
        );

        release.join().unwrap();
        settle_profile_switch(&mut app).await;
        assert!(app.accounts[0].is_current);
        assert!(!app.status_is_error);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn profile_switch_commit_keeps_event_loop_responsive_during_cache_lock_contention() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "saved-access",
                "saved-refresh",
            ),
        );
        write_auth_durable(
            &codex_home.join("auth.json"),
            &managed_auth(
                "account@example.com",
                "acct_account",
                "saved-access",
                "saved-refresh",
            ),
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            drop(cache_lock);
        });

        app.switch_selected();
        let responsive = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        })
        .await;
        assert!(
            responsive.is_ok(),
            "cache-lock contention must stay on the blocking pool"
        );

        release.join().unwrap();
        settle_profile_switch(&mut app).await;
        assert!(app.accounts[0].is_current);
        assert!(!app.status_is_error);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_drains_a_task_after_profile_lease_acquisition() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());

        let mut app = App::new();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_control = lease_control.clone();
        let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_by_task = completed.clone();
        let handle = tokio::spawn(async move {
            let _lease =
                crate::profile::acquire_profile_lease_async_cancellable("account", &task_control)
                    .await
                    .unwrap()
                    .expect("lease must be acquired before shutdown");
            acquired_tx.send(()).unwrap();
            finish_rx.await.expect("test releases credential work");
            completed_by_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        app.track_account_task(
            "account".into(),
            AccountTaskKind::ResetCard,
            lease_control,
            handle,
        );
        acquired_rx.await.expect("task acquired its profile lease");

        {
            let drain = app.drain_credential_tasks();
            tokio::pin!(drain);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut drain)
                    .await
                    .is_err(),
                "shutdown must not cancel work after lease acquisition"
            );
            finish_tx.send(()).expect("credential task remains alive");
            tokio::time::timeout(std::time::Duration::from_secs(1), drain)
                .await
                .expect("drain finishes after the credential task reaches completion");
        }

        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(app.account_tasks.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn batch_stop_cancels_a_round_contended_before_profile_lease_acquisition() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let held_lease = crate::profile::acquire_profile_lease("account").unwrap();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let login_control = lease_control.clone();
        let (attempted_tx, mut attempted_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let login = async move {
            attempted_tx.send(()).unwrap();
            match crate::profile::acquire_profile_lease_async_cancellable("account", &login_control)
                .await?
            {
                Some(_lease) => anyhow::Ok("saved"),
                None => Err(login::LoginCancelled.into()),
            }
        };
        let stop = async move {
            stop_rx.await.map_err(std::io::Error::other)?;
            Ok(())
        };
        let completion = finish_login_or_stop_after_round(login, stop, &lease_control);
        tokio::pin!(completion);
        tokio::select! {
            attempted = &mut attempted_rx => attempted.expect("lease attempt started"),
            result = &mut completion => panic!("contended round ended before stop: {result:?}"),
        }

        stop_tx.send(()).expect("stop listener remains alive");
        let (result, stop_requested, signal_error) =
            tokio::time::timeout(std::time::Duration::from_secs(1), completion)
                .await
                .expect("pre-lease batch round must stop promptly");

        assert!(login::is_login_cancelled(&result.unwrap_err()));
        assert!(stop_requested);
        assert!(signal_error.is_none());
        drop(held_lease);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn batch_stop_drains_a_round_after_profile_lease_acquisition() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let login_control = lease_control.clone();
        let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let login = async move {
            let _lease =
                crate::profile::acquire_profile_lease_async_cancellable("account", &login_control)
                    .await?
                    .expect("round acquires its lease before stop");
            acquired_tx.send(()).unwrap();
            finish_rx.await.expect("test releases the login round");
            anyhow::Ok("saved")
        };
        let stop = async move {
            stop_rx.await.map_err(std::io::Error::other)?;
            Ok(())
        };
        let completion = finish_login_or_stop_after_round(login, stop, &lease_control);
        tokio::pin!(completion);
        tokio::select! {
            acquired = acquired_rx => acquired.expect("round acquired its lease"),
            result = &mut completion => panic!("round ended before stop: {result:?}"),
        }

        stop_tx.send(()).expect("stop listener remains alive");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut completion)
                .await
                .is_err(),
            "a post-lease stop request must await the current round"
        );
        finish_tx.send(()).expect("login round remains alive");
        let (result, stop_requested, signal_error) = completion.await;

        assert_eq!(result.unwrap(), "saved");
        assert!(stop_requested);
        assert!(signal_error.is_none());
    }

    #[tokio::test]
    async fn switch_rename_and_delete_do_not_block_on_in_flight_account_work() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        let handle = tokio::spawn(std::future::pending());
        app.track_account_task(
            "account".into(),
            AccountTaskKind::ResetCard,
            profile::ProfileLeaseAcquireControl::new(),
            handle,
        );

        app.start_rename_alias("account");
        assert!(app.rename.is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("before renaming"))
        );

        app.status_msg = None;
        app.request_delete_alias("account");
        assert!(app.confirm.is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("before deleting"))
        );

        app.status_msg = None;
        app.switch_selected();
        assert_eq!(app.account_tasks.len(), 1, "no switch task may be queued");
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("before switching"))
        );

        let task = app.account_tasks.remove(&0).expect("tracked task");
        task.handle.abort();
        let _ = task.handle.await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn delete_confirmation_keeps_the_ui_free_while_the_profile_lease_is_held() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth("account@example.com", "acct_account", "access", "refresh"),
        );
        let held_lease = profile::acquire_profile_lease("account").unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo {
                account_id: Some("acct_account".into()),
                email: Some("account@example.com".into()),
                ..AccountInfo::default()
            },
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.view_indices.push(0);
        app.confirm = Some(ConfirmAction::Delete("account".into()));

        app.confirm_action();

        assert!(app.profile_mutation_in_flight());
        assert!(
            profile_path.exists(),
            "the held lease still owns the profile"
        );
        drop(held_lease);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while app.profile_mutation_in_flight() {
                app.poll_profile_mutation().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("delete task must finish after the lease is released");

        assert!(app.accounts.is_empty());
        assert!(!profile_path.exists());
        assert!(app.account_tasks.is_empty());
        assert!(app.refreshing_requests.is_empty());
    }

    #[tokio::test]
    async fn rename_does_not_wait_on_an_in_flight_destination_alias() {
        let mut app = App::new();
        for alias in ["old", "existing"] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Idle,
                is_current: false,
            });
        }
        app.view_indices.extend([0, 1]);
        let handle = tokio::spawn(std::future::pending());
        app.track_account_task(
            "existing".into(),
            AccountTaskKind::ResetCard,
            profile::ProfileLeaseAcquireControl::new(),
            handle,
        );
        app.start_rename_alias("old");
        let rename = app.rename.as_mut().expect("rename editor opens");
        rename.input = "existing".into();
        rename.cursor = super::grapheme_count(&rename.input);

        assert!(app.handle_rename_key(KeyCode::Enter));
        assert!(app.rename.is_some(), "input is retained for a later retry");
        assert!(app.status_msg.as_deref().is_some_and(
            |message| message.starts_with("existing:") && message.contains("before renaming")
        ));

        let task = app.account_tasks.remove(&0).expect("tracked task");
        task.handle.abort();
        let _ = task.handle.await;
    }

    #[test]
    fn search_edits_whole_unicode_graphemes() {
        let mut app = App::new();
        app.search = Some(SearchState {
            query: "👩‍💻a".to_string(),
            cursor: 2,
        });
        app.search_active = true;

        app.handle_search_key(KeyCode::Left);
        app.handle_search_key(KeyCode::Backspace);

        let state = app.search.as_ref().expect("search remains active");
        assert_eq!(state.query, "a");
        assert_eq!(state.cursor, 0);

        app.handle_search_key(KeyCode::Home);
        app.handle_search_key(KeyCode::Char('e'));
        app.handle_search_key(KeyCode::Char('\u{301}'));
        let state = app.search.as_ref().expect("search remains active");
        assert_eq!(state.query, "e\u{301}a");
        assert_eq!(
            state.cursor, 1,
            "combining mark joins the preceding grapheme"
        );

        app.handle_search_key(KeyCode::Backspace);
        let state = app.search.as_ref().expect("search remains active");
        assert_eq!(state.query, "a");
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn search_input_ignores_terminal_control_characters() {
        let mut app = App::new();
        app.search = Some(SearchState {
            query: "safe".to_string(),
            cursor: 4,
        });
        app.search_active = true;

        app.handle_search_key(KeyCode::Char('\u{1b}'));
        app.handle_search_key(KeyCode::Char('\n'));

        let state = app.search.as_ref().expect("search remains active");
        assert_eq!(state.query, "safe");
        assert_eq!(state.cursor, 4);
    }

    #[test]
    fn model_results_from_before_credential_reload_are_ignored() {
        let mut app = App::new();
        app.model_cache
            .insert("account".into(), ModelStatus::Loading);
        app.model_requests.insert("account".into(), 4);
        app.model_sender
            .try_send((
                "account".into(),
                test_binding(),
                4,
                Ok(vec![ModelEntry::default()]),
            ))
            .unwrap();

        app.invalidate_models_after_credential_reload(
            &["account".to_string()].into_iter().collect(),
        );
        app.poll_model_results();

        assert!(!app.model_cache.contains_key("account"));
        assert!(!app.model_requests.contains_key("account"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn untracked_live_auth_does_not_leave_stale_profile_active() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/tracked/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "tracked@example.com",
                "acct_tracked",
                "tracked",
                "tracked-refresh",
            ),
        );
        write_auth_durable(
            &codex_home.join("auth.json"),
            &serde_json::json!({
                "tokens": { "access_token": "untracked", "refresh_token": "untracked-refresh" }
            }),
        );
        std::fs::write(home.path().join("current"), "tracked").unwrap();

        let mut app = App::new();
        app.load_profiles();

        assert_eq!(app.accounts.len(), 1);
        assert!(!app.accounts[0].is_current);

        app.switch_selected();
        settle_profile_switch(&mut app).await;

        assert!(app.status_is_error);
        assert!(app.confirm.is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("not saved"))
        );
        assert_eq!(
            crate::auth::read_auth(&codex_home.join("auth.json"))
                .unwrap()
                .pointer("/tokens/access_token")
                .and_then(serde_json::Value::as_str),
            Some("untracked")
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn refreshed_tracked_live_auth_is_saved_before_switch_without_confirmation() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();

        let mut stored_current = managed_auth(
            "current@example.com",
            "acct_current",
            "stored-access",
            "stored-refresh",
        );
        stored_current["last_refresh"] = serde_json::Value::String("2026-08-26T10:00:00Z".into());
        let mut refreshed_live = managed_auth(
            "current@example.com",
            "acct_current",
            "refreshed-access",
            "refreshed-refresh",
        );
        refreshed_live["last_refresh"] = serde_json::Value::String("2026-08-26T10:05:00Z".into());
        let target = managed_auth(
            "target@example.com",
            "acct_target",
            "target-access",
            "target-refresh",
        );

        for (alias, auth) in [("current", &stored_current), ("target", &target)] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(&path, auth);
        }
        write_auth_durable(&codex_home.join("auth.json"), &refreshed_live);
        std::fs::write(home.path().join("current"), "current").unwrap();

        let mut app = App::new();
        app.load_profiles();
        let target_idx = app
            .accounts
            .iter()
            .position(|account| account.alias == "target")
            .unwrap();
        app.selected = app
            .view_indices
            .iter()
            .position(|index| *index == target_idx)
            .unwrap();

        app.switch_selected();
        settle_profile_switch(&mut app).await;

        assert!(app.confirm.is_none());
        assert!(
            !app.status_is_error,
            "unexpected status: {:?}",
            app.status_msg
        );
        assert_eq!(
            crate::auth::read_auth(&home.path().join("profiles/current/auth.json")).unwrap(),
            refreshed_live
        );
        assert_eq!(
            crate::auth::read_auth(&codex_home.join("auth.json")).unwrap(),
            target
        );
        assert!(
            app.accounts
                .iter()
                .find(|account| account.alias == "target")
                .is_some_and(|account| account.is_current)
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn partial_switch_publication_updates_the_tui_to_the_actual_live_account() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();

        let alice = managed_auth(
            "alice@example.com",
            "acct_alice",
            "alice-access",
            "alice-refresh",
        );
        let bob = managed_auth("bob@example.com", "acct_bob", "bob-access", "bob-refresh");
        for (alias, auth) in [("alice", &alice), ("bob", &bob)] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(&path, auth);
        }
        write_auth_durable(&codex_home.join("auth.json"), &alice);
        std::fs::write(home.path().join("current"), "alice").unwrap();

        let mut app = App::new();
        app.load_profiles();
        let bob_account = app
            .accounts
            .iter()
            .position(|account| account.alias == "bob")
            .unwrap();
        app.selected = app
            .view_indices
            .iter()
            .position(|index| *index == bob_account)
            .unwrap();

        crate::profile::fail_next_activation_marker_write();
        app.switch_selected();
        settle_profile_switch(&mut app).await;

        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("was published to live auth")),
            "{:?}",
            app.status_msg
        );
        assert!(
            app.accounts
                .iter()
                .find(|account| account.alias == "bob")
                .is_some_and(|account| account.is_current)
        );
        assert!(
            app.accounts
                .iter()
                .find(|account| account.alias == "alice")
                .is_some_and(|account| !account.is_current)
        );
        assert_eq!(
            crate::auth::read_auth(&codex_home.join("auth.json")).unwrap(),
            bob
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("current")).unwrap(),
            "alice"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn partial_switch_rechecks_live_auth_before_updating_the_tui() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();

        let alice = managed_auth(
            "alice@example.com",
            "acct_alice",
            "alice-access",
            "alice-refresh",
        );
        let bob = managed_auth("bob@example.com", "acct_bob", "bob-access", "bob-refresh");
        for (alias, auth) in [("alice", &alice), ("bob", &bob)] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(&path, auth);
        }
        let live_path = codex_home.join("auth.json");
        write_auth_durable(&live_path, &alice);
        std::fs::write(home.path().join("current"), "alice").unwrap();

        let mut app = App::new();
        app.load_profiles();
        let bob_account = app
            .accounts
            .iter()
            .position(|account| account.alias == "bob")
            .unwrap();
        app.selected = app
            .view_indices
            .iter()
            .position(|index| *index == bob_account)
            .unwrap();

        crate::profile::fail_next_activation_marker_write();
        let replacement_path = live_path.clone();
        let replacement = alice.clone();
        crate::profile::after_next_partial_activation(move || {
            write_auth_durable(&replacement_path, &replacement);
        });
        app.switch_selected();
        settle_profile_switch(&mut app).await;

        assert!(app.status_is_error);
        assert_eq!(crate::auth::read_auth(&live_path).unwrap(), alice);
        assert!(
            app.accounts
                .iter()
                .find(|account| account.alias == "alice")
                .is_some_and(|account| account.is_current)
        );
        assert!(
            app.accounts
                .iter()
                .find(|account| account.alias == "bob")
                .is_some_and(|account| !account.is_current)
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn partial_switch_reconciliation_failure_clears_every_active_highlight() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();

        let alice = managed_auth(
            "alice@example.com",
            "acct_alice",
            "alice-access",
            "alice-refresh",
        );
        let bob = managed_auth("bob@example.com", "acct_bob", "bob-access", "bob-refresh");
        for (alias, auth) in [("alice", &alice), ("bob", &bob)] {
            let path = home.path().join(format!("profiles/{alias}/auth.json"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_auth_durable(&path, auth);
        }
        let live_path = codex_home.join("auth.json");
        write_auth_durable(&live_path, &alice);
        std::fs::write(home.path().join("current"), "alice").unwrap();

        let mut app = App::new();
        app.load_profiles();
        let bob_account = app
            .accounts
            .iter()
            .position(|account| account.alias == "bob")
            .unwrap();
        app.selected = app
            .view_indices
            .iter()
            .position(|index| *index == bob_account)
            .unwrap();

        crate::profile::fail_next_activation_marker_write();
        let unreadable_live = live_path.clone();
        crate::profile::after_next_partial_activation(move || {
            std::fs::remove_file(&unreadable_live).unwrap();
            std::fs::create_dir(&unreadable_live).unwrap();
        });
        app.switch_selected();
        settle_profile_switch(&mut app).await;

        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("active account could not be verified")),
            "{:?}",
            app.status_msg
        );
        assert!(
            app.accounts.iter().all(|account| !account.is_current),
            "an unverifiable live binding must not leave a stale active highlight"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn successful_switch_surfaces_selection_history_failure_as_a_warning() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let saved = managed_auth(
            "account@example.com",
            "acct_account",
            "saved-access",
            "saved-refresh",
        );
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_home).unwrap();
        write_auth_durable(&profile_path, &saved);
        std::fs::write(home.path().join("cache.json"), b"{invalid cache").unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        app.switch_selected();
        settle_profile_switch(&mut app).await;

        assert!(app.accounts[0].is_current);
        assert!(!app.status_is_error, "the activation itself succeeded");
        assert!(
            app.status_msg.as_deref().is_some_and(|message| {
                message.contains("Switched to account")
                    && message.contains("selection history was not updated")
            }),
            "unexpected switch warning: {:?}",
            app.status_msg
        );
        assert_eq!(
            crate::auth::read_auth(&codex_home.join("auth.json")).unwrap(),
            saved
        );
    }

    #[test]
    fn failed_profile_reload_preserves_the_previous_model_and_reports_the_error() {
        let _lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::create_dir_all(home.path().join("profiles/broken/auth.json")).unwrap();

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "retained".to_string(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);

        app.load_profiles();

        assert_eq!(app.accounts.len(), 1);
        assert_eq!(app.accounts[0].alias, "retained");
        assert!(app.accounts[0].is_current);
        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("Profile reload failed")),
            "{:?}",
            app.status_msg
        );
    }

    #[tokio::test]
    async fn slow_warmup_stays_tracked_until_it_really_finishes() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            finish_rx
                .await
                .map_err(|error| format!("finish signal dropped: {error}"))?;
            Ok(())
        });
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now() - std::time::Duration::from_secs(61),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle,
            },
        );

        app.poll_warmup_results().await;
        assert!(app.is_warmup_in_flight("account"));
        assert!(app.warmup_tasks[&1].slow_reported);
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Warmup still running after 60s: account")
        );

        app.status_msg = None;
        app.poll_warmup_results().await;
        assert!(
            app.status_msg.is_none(),
            "the slow notice is emitted only once"
        );
        assert!(app.is_warmup_in_flight("account"));

        finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.warmup_tasks[&1].handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("warmup task should finish after its release signal");
        app.poll_warmup_results().await;

        assert!(app.warmup_tasks.is_empty());
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Warmed up account — refreshing usage...")
        );
    }

    #[tokio::test]
    async fn panicked_warmup_clears_tracking_after_join_error_is_observed() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        let handle = tokio::spawn(async move {
            panic!("warmup panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "account".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle,
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.warmup_tasks[&1].handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("panicked warmup task should finish");

        app.poll_warmup_results().await;

        assert!(app.warmup_tasks.is_empty());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.starts_with("Warmup task stopped (account):"))
        );
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| !message.contains("warmup panic"))
        );
    }

    #[tokio::test]
    async fn mixed_warmup_results_keep_the_deterministic_failure_status() {
        let mut app = App::new();
        add_test_account(&mut app, "success");
        add_test_account(&mut app, "failure");
        let success = tokio::spawn(async { Ok(()) });
        let failure = tokio::spawn(async { Err("injected warmup failure".to_string()) });
        app.warmup_tasks.insert(
            20,
            WarmupTask {
                alias: "success".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: success,
            },
        );
        app.warmup_tasks.insert(
            10,
            WarmupTask {
                alias: "failure".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: failure,
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while app
                .warmup_tasks
                .values()
                .any(|task| !task.handle.is_finished())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("warmup fixtures should finish");

        app.poll_warmup_results().await;

        assert!(app.warmup_tasks.is_empty());
        assert!(app.status_is_error);
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Warmup failed (failure): injected warmup failure")
        );
    }

    #[tokio::test]
    async fn new_slow_notice_does_not_replace_a_warmup_failure_on_the_next_poll() {
        let mut app = App::new();
        add_test_account(&mut app, "slow");
        add_test_account(&mut app, "failure");
        let slow = tokio::spawn(std::future::pending::<std::result::Result<(), String>>());
        let failure = tokio::spawn(async { Err("injected warmup failure".to_string()) });
        app.warmup_tasks.insert(
            2,
            WarmupTask {
                alias: "slow".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now() - std::time::Duration::from_secs(61),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: slow,
            },
        );
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "failure".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: failure,
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.warmup_tasks[&1].handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed warmup fixture should finish");

        app.poll_warmup_results().await;
        assert!(app.status_is_error);
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Warmup failed (failure): injected warmup failure")
        );
        assert!(app.warmup_tasks[&2].slow_reported);

        app.poll_warmup_results().await;
        assert!(app.status_is_error);
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Warmup failed (failure): injected warmup failure")
        );

        let slow = app.warmup_tasks.remove(&2).unwrap().handle;
        slow.abort();
        let _ = slow.await;
    }

    #[tokio::test]
    async fn ongoing_slow_warmup_status_outranks_a_completed_success() {
        let mut app = App::new();
        add_test_account(&mut app, "slow");
        add_test_account(&mut app, "success");
        let slow = tokio::spawn(std::future::pending::<std::result::Result<(), String>>());
        let success = tokio::spawn(async { Ok(()) });
        app.warmup_tasks.insert(
            2,
            WarmupTask {
                alias: "slow".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now() - std::time::Duration::from_secs(61),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: slow,
            },
        );
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "success".into(),
                binding: test_binding(),
                origin: WarmupOrigin::Manual,
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                network_wait: SafeTaskCancellation::new(),
                model_discovery: SafeTaskCancellation::new(),
                handle: success,
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.warmup_tasks[&1].handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successful warmup fixture should finish");

        app.poll_warmup_results().await;

        assert_eq!(
            app.status_msg.as_deref(),
            Some("Warmup still running after 60s: slow")
        );
        assert!(app.warmup_tasks[&2].slow_reported);
        let slow = app.warmup_tasks.remove(&2).unwrap().handle;
        slow.abort();
        let _ = slow.await;
    }

    #[tokio::test]
    async fn model_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loading);
        app.model_requests.insert("account".into(), 7);
        app.open_account_menu();

        app.model_sender
            .try_send((
                "account".into(),
                test_binding(),
                7,
                Ok(vec![ModelEntry {
                    slug: "official-slug".into(),
                    display_name: Some("Official Name".into()),
                    description: Some("Official description".into()),
                    visibility: Some("list".into()),
                    supported_in_api: Some(true),
                    context_window: Some(372_000),
                    default_reasoning_effort: Some("medium".into()),
                    supported_reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
                    ..ModelEntry::default()
                }]),
            ))
            .unwrap();
        app.poll_model_results();

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should remain open");
        };
        assert!(info.models.iter().any(|line| {
            line.trim() == "Official Name · default medium · allowed low, medium, high"
        }));
        assert!(!info.models.iter().any(|line| {
            line.contains("official-slug")
                || line.contains("visibility=")
                || line.contains("context=")
        }));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_cancels_a_model_request_only_after_credential_preparation() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "access-token",
                "refresh-token",
            ),
        );

        let (models_started_tx, mut models_started_rx) = tokio::sync::mpsc::unbounded_channel();
        let models_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let handler_gate = std::sync::Arc::clone(&models_gate);
        let server = axum::Router::new().route(
            "/codex/models",
            axum::routing::get(move || {
                let started = models_started_tx.clone();
                let gate = std::sync::Arc::clone(&handler_gate);
                async move {
                    started.send(()).unwrap();
                    let _permit = gate.acquire_owned().await.unwrap();
                    axum::Json(serde_json::json!({
                        "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let _models_url =
            EnvVarGuard::set_text("CS_MODELS_URL", &format!("http://{address}/codex/models"));

        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        app.ensure_models_loaded("account");

        tokio::time::timeout(std::time::Duration::from_secs(1), models_started_rx.recv())
            .await
            .expect("model preparation must reach the read-only endpoint")
            .expect("model endpoint arrival channel closed unexpectedly");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.drain_credential_tasks(),
        )
        .await
        .expect("shutdown must cancel a blocked read-only model request");

        assert!(app.account_tasks.is_empty());
        assert!(!app.model_requests.contains_key("account"));
        assert!(!matches!(
            app.model_cache.get("account"),
            Some(ModelStatus::Loading)
        ));
        assert_eq!(app.usage_limiter.available_permits(), 1);

        models_gate.add_permits(1);
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn account_menu_loads_auth_expiry_without_blocking_the_open_path() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "access-token",
                "refresh-token",
            ),
        );

        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));

        app.open_account_menu();
        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu.as_ref() else {
            panic!("account detail should open before auth metadata finishes");
        };
        assert!(info.auth_expiries.is_empty());
        assert_eq!(app.auth_expiry_tasks.len(), 1);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.auth_expiry_tasks.is_empty() {
                tokio::task::yield_now().await;
                app.poll_auth_expiry_tasks().await;
            }
        })
        .await
        .expect("account-detail auth read must finish");

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu.as_ref() else {
            panic!("account detail should remain open");
        };
        assert_eq!(
            info.auth_expiries
                .iter()
                .map(|expiry| expiry.name.as_str())
                .collect::<Vec<_>>(),
            ["ID token", "Access token"]
        );
    }

    #[test]
    fn model_error_is_stable_until_explicit_refresh() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Error("denied".into()));

        app.tick();
        app.ensure_models_loaded_for_selected();

        assert!(matches!(
            app.model_cache.get("account"),
            Some(ModelStatus::Error(error)) if error == "denied"
        ));
        assert!(!app.model_requests.contains_key("account"));
    }

    #[test]
    fn stale_model_result_cannot_overwrite_newer_generation() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.model_cache
            .insert("account".into(), ModelStatus::Loading);
        app.model_requests.insert("account".into(), 2);

        let model = |slug: &str| ModelEntry {
            slug: slug.into(),
            ..ModelEntry::default()
        };
        app.model_sender
            .try_send(("account".into(), test_binding(), 2, Ok(vec![model("new")])))
            .unwrap();
        app.model_sender
            .try_send(("account".into(), test_binding(), 1, Ok(vec![model("old")])))
            .unwrap();

        app.poll_model_results();

        let Some(ModelStatus::Loaded(models)) = app.model_cache.get("account") else {
            panic!("newest model result should remain loaded");
        };
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "new");
    }

    #[test]
    fn model_result_for_a_previous_alias_owner_is_discarded() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.model_cache
            .insert("account".into(), ModelStatus::Loading);
        app.model_requests.insert("account".into(), 3);
        let previous_owner = StrictAccountBinding {
            account_id: "acct_previous".to_string(),
            email: "previous@example.com".to_string(),
        };
        app.model_sender
            .try_send((
                "account".into(),
                previous_owner,
                3,
                Ok(vec![ModelEntry::default()]),
            ))
            .unwrap();

        app.poll_model_results();

        assert!(!app.model_requests.contains_key("account"));
        assert!(!app.model_cache.contains_key("account"));
    }

    #[tokio::test]
    async fn usage_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));
        app.refreshing_requests
            .insert("account".into(), (1, Refresh::Cached));
        app.open_account_menu();

        app.result_sender
            .try_send((
                "account".into(),
                test_binding(),
                1,
                Ok(UsageInfo::default()),
            ))
            .unwrap();
        app.poll_results();
        assert_eq!(app.loading_count(), 0);

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should remain open");
        };
        assert!(info.usage.is_some());
    }

    #[test]
    fn quota_result_is_visible_before_deferred_reset_metadata_finishes() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (5, Refresh::Forced));
        app.usage_generations.insert("account".into(), 5);
        app.usage_metadata_requests.insert("account".into(), 5);
        let core = UsageInfo {
            plan_type: Some("plus".to_string()),
            ..UsageInfo::default()
        };
        app.result_sender
            .try_send(("account".into(), test_binding(), 5, Ok(core.clone())))
            .unwrap();

        app.poll_results();

        assert!(!app.refreshing_requests.contains_key("account"));
        assert_eq!(
            match &app.accounts[0].usage {
                UsageStatus::Loaded(usage) => usage.plan_type.as_deref(),
                _ => None,
            },
            Some("plus")
        );
        assert_eq!(app.usage_metadata_requests.get("account"), Some(&5));

        let enriched = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![ResetCredit {
                id: "credit".to_string(),
                ..ResetCredit::default()
            }],
            ..core
        };
        app.usage_enrichment_sender
            .try_send(("account".into(), test_binding(), 5, enriched))
            .unwrap();
        app.poll_results();

        assert!(!app.usage_metadata_requests.contains_key("account"));
        assert!(matches!(
            &app.accounts[0].usage,
            UsageStatus::Loaded(usage) if usage.reset_credits[0].id == "credit"
        ));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_base_cache_write_releases_quota_and_network_before_enrichment() {
        use std::sync::Arc;

        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "access-token",
                "refresh-token",
            ),
        );

        let (credits_started_tx, mut credits_started_rx) = tokio::sync::mpsc::unbounded_channel();
        let credits_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let handler_gate = Arc::clone(&credits_gate);
        let server = axum::Router::new()
            .route(
                "/usage",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "plan_type": "plus",
                        "rate_limit": {
                            "primary_window": {
                                "used_percent": 23.0,
                                "reset_at": 4_102_444_800_i64,
                                "limit_window_seconds": 18_000
                            }
                        },
                        "credits": null,
                        "spend_control": null,
                        "additional_rate_limits": null,
                        "rate_limit_reached_type": null
                    }))
                }),
            )
            .route(
                "/credits",
                axum::routing::get(move || {
                    let started = credits_started_tx.clone();
                    let gate = Arc::clone(&handler_gate);
                    async move {
                        started.send(()).unwrap();
                        let permit = gate.acquire_owned().await.unwrap();
                        permit.forget();
                        axum::Json(serde_json::json!({
                            "available_count": 1,
                            "credits": [{"id": "credit-1", "status": "available"}]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let _usage_url = EnvVarGuard::set_text("CS_USAGE_URL", &format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set_text("CS_RESET_CREDITS_URL", &format!("http://{address}/credits"));

        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock).unwrap();

        let mut app = App::new();
        app.usage_limiter = Arc::new(tokio::sync::Semaphore::new(1));
        app.accounts.push(AccountEntry {
            alias: "account".to_string(),
            info: test_account_info_for("account@example.com", "acct_account"),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.fetch_usage_for(0, AccountRefreshPlan::usage_only(Refresh::Forced));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                app.poll_results();
                if matches!(app.accounts[0].usage, UsageStatus::Loaded(_)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("quota row must settle while the cache lock is held");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(app.usage_limiter.available_permits(), 1);
        assert!(app.account_tasks.values().any(|task| {
            matches!(task.kind, AccountTaskKind::Usage { .. }) && !task.handle.is_finished()
        }));
        assert_eq!(
            credits_started_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty),
            "reset-credit enrichment must not start before the base cache generation is durable"
        );

        fs4::FileExt::unlock(&cache_lock).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), credits_started_rx.recv())
            .await
            .expect("reset-credit enrichment must start after the base cache write")
            .expect("reset-credit start channel closed unexpectedly");
        let binding = test_account_info_for("account@example.com", "acct_account")
            .strict_binding()
            .unwrap();
        let base_cached = crate::cache::get_bound("account", &binding)
            .unwrap()
            .expect("base usage must be cached before reset-credit enrichment");
        let base_revision = base_cached
            .cache_revision
            .clone()
            .expect("base usage cache must carry a revision");
        assert!(base_cached.reset_credits.is_empty());

        credits_gate.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                app.poll_account_tasks().await;
                app.poll_results();
                if app.account_tasks.is_empty() && app.usage_metadata_requests.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reset-credit enrichment must settle");

        let enriched_cached = crate::cache::get_bound("account", &binding)
            .unwrap()
            .expect("enriched usage must remain cached");
        assert_eq!(
            enriched_cached.cache_revision.as_deref(),
            Some(base_revision.as_str())
        );
        assert_eq!(enriched_cached.reset_credits[0].id, "credit-1");
        assert!(matches!(
            &app.accounts[0].usage,
            UsageStatus::Loaded(usage) if usage.reset_credits[0].id == "credit-1"
        ));
        server.abort();
    }

    #[test]
    fn stale_usage_result_is_ignored_after_a_new_request_generation_starts() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (2, Refresh::Forced));

        app.result_sender
            .try_send((
                "account".into(),
                test_binding(),
                1,
                Err(crate::usage::UsageError {
                    summary: "old request".into(),
                    detail: "must be ignored".into(),
                }),
            ))
            .unwrap();
        app.poll_results();

        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert_eq!(app.loading_count(), 1);
    }

    #[test]
    fn usage_result_for_a_previous_alias_owner_is_discarded() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (9, Refresh::Forced));
        app.usage_metadata_requests.insert("account".into(), 9);
        app.result_sender
            .try_send((
                "account".into(),
                StrictAccountBinding {
                    account_id: "acct_previous".to_string(),
                    email: "previous@example.com".to_string(),
                },
                9,
                Ok(UsageInfo::default()),
            ))
            .unwrap();

        app.poll_results();

        assert!(matches!(app.accounts[0].usage, UsageStatus::Idle));
        assert!(!app.refreshing_requests.contains_key("account"));
        assert!(!app.usage_metadata_requests.contains_key("account"));
    }

    #[test]
    fn deferred_reset_metadata_for_a_previous_alias_owner_is_discarded() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                plan_type: Some("current".to_string()),
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_generations.insert("account".into(), 9);
        app.usage_metadata_requests.insert("account".into(), 9);
        app.usage_enrichment_sender
            .try_send((
                "account".into(),
                StrictAccountBinding {
                    account_id: "acct_previous".to_string(),
                    email: "previous@example.com".to_string(),
                },
                9,
                UsageInfo {
                    plan_type: Some("previous".to_string()),
                    reset_credits_available_count: Some(1),
                    ..UsageInfo::default()
                },
            ))
            .unwrap();

        app.poll_results();

        let UsageStatus::Loaded(usage) = &app.accounts[0].usage else {
            panic!("the current owner's quota must stay loaded")
        };
        assert_eq!(usage.plan_type.as_deref(), Some("current"));
        assert_eq!(usage.reset_credits_available_count, None);
        assert!(!app.usage_metadata_requests.contains_key("account"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn workspace_result_applies_to_every_alias_for_the_same_account_id() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let mut app = App::new();
        for alias in ["first", "second"] {
            app.accounts.push(AccountEntry {
                alias: alias.to_string(),
                info: test_account_info(),
                usage: UsageStatus::Idle,
                is_current: false,
            });
        }
        app.accounts.push(AccountEntry {
            alias: "incomplete".to_string(),
            info: AccountInfo {
                account_id: Some("acct_test".to_string()),
                email: None,
                ..AccountInfo::default()
            },
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.extend([0, 1, 2]);
        let generation = app
            .begin_workspace_request(&test_binding(), WorkspaceRefresh::IfStale)
            .unwrap();
        app.workspace_sender
            .try_send((
                "first".to_string(),
                test_binding(),
                generation,
                Ok(crate::cache::WorkspaceState::Named(
                    "Shared workspace".to_string(),
                )),
            ))
            .unwrap();

        app.poll_results();

        assert!(
            app.accounts[..2]
                .iter()
                .all(|entry| entry.info.workspace_name.as_deref() == Some("Shared workspace"))
        );
        assert!(app.accounts[2].info.workspace_name.is_none());
        assert!(!app.workspace_requests.contains_key("acct_test"));
        finish_workspace_cache_writes(&mut app).await;
    }

    #[test]
    fn non_forced_workspace_request_is_deduplicated_by_account_id() {
        let mut app = App::new();
        let binding = test_binding();

        let first = app.begin_workspace_request(&binding, WorkspaceRefresh::IfStale);
        let duplicate = app.begin_workspace_request(&binding, WorkspaceRefresh::IfStale);

        assert!(first.is_some());
        assert!(duplicate.is_none());
        assert_eq!(app.workspace_requests.get("acct_test"), first.as_ref());
    }

    #[test]
    fn latest_workspace_failure_completes_the_in_flight_generation() {
        let mut app = App::new();
        add_test_account(&mut app, "account");
        let binding = test_binding();
        let generation = app
            .begin_workspace_request(&binding, WorkspaceRefresh::IfStale)
            .unwrap();
        app.workspace_sender
            .try_send((
                "account".into(),
                binding,
                generation,
                Err("request failed".into()),
            ))
            .unwrap();

        app.poll_results();

        assert!(!app.workspace_requests.contains_key("acct_test"));
        assert!(app.accounts[0].info.workspace_name.is_none());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn startup_workspace_waits_for_its_own_core_but_not_an_unrelated_alias() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "pre-refresh-access",
                "refresh-token",
            ),
        );

        let (authorization_tx, mut authorization_rx) = tokio::sync::mpsc::unbounded_channel();
        let service = axum::Router::new().route(
            "/accounts",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let authorization_tx = authorization_tx.clone();
                async move {
                    authorization_tx
                        .send(
                            headers
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_string),
                        )
                        .unwrap();
                    axum::Json(serde_json::json!({
                        "accounts": [{
                            "id": "acct_test",
                            "name": "Workspace",
                            "structure": "workspace"
                        }],
                        "account_ordering": ["acct_test"]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        let _accounts_url = EnvVarGuard::set_text(
            "CS_ACCOUNTS_CHECK_URL",
            &format!("http://{address}/accounts"),
        );

        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.accounts.push(AccountEntry {
            alias: "slow".into(),
            info: test_account_info_for("slow@example.com", "acct_slow"),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.extend([0, 1]);
        app.refreshing_requests
            .insert("account".into(), (7, Refresh::Unattended));
        app.usage_generations.insert("account".into(), 7);
        app.pending_usage_refreshes.insert(
            "account".into(),
            AccountRefreshPlan::workspace_only(WorkspaceRefresh::IfStale),
        );
        app.refreshing_requests
            .insert("slow".into(), (8, Refresh::Unattended));
        app.usage_generations.insert("slow".into(), 8);
        let mut phase = StartupUsagePhase::RefreshingCore {
            ordered_aliases: vec!["account".into(), "slow".into()],
        };

        assert!(!advance_startup_usage_phase(&mut app, &mut phase));
        assert!(app.workspace_lookup_tasks.is_empty());
        assert_eq!(
            authorization_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty),
        );

        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "post-refresh-access",
                "refresh-token",
            ),
        );
        app.refreshing_requests.remove("account");
        app.accounts[0].usage = UsageStatus::Loaded(Box::default());

        assert!(!advance_startup_usage_phase(&mut app, &mut phase));
        assert!(app.workspace_lookup_tasks.is_empty());

        app.record_usage_lease_release("account", 7);
        assert!(!advance_startup_usage_phase(&mut app, &mut phase));
        assert!(matches!(phase, StartupUsagePhase::RefreshingCore { .. }));
        assert!(app.refreshing_requests.contains_key("slow"));
        let authorization =
            tokio::time::timeout(std::time::Duration::from_secs(2), authorization_rx.recv())
                .await
                .expect("workspace request must start after its alias releases the core lease")
                .expect("workspace authorization channel closed unexpectedly");
        assert_eq!(authorization.as_deref(), Some("Bearer post-refresh-access"));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                app.poll_workspace_lookup_tasks().await;
                app.poll_results();
                if app.workspace_lookup_tasks.is_empty() && app.workspace_requests.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace task must settle");
        finish_workspace_cache_writes(&mut app).await;

        app.refreshing_requests.remove("slow");
        app.accounts[1].usage = UsageStatus::Error(super::UsageError {
            summary: "unavailable".into(),
            detail: "unavailable".into(),
        });
        app.record_usage_lease_release("slow", 8);
        assert!(advance_startup_usage_phase(&mut app, &mut phase));
        assert!(matches!(phase, StartupUsagePhase::Settled));
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn combined_refresh_starts_workspace_with_the_rotated_bearer() {
        use axum::response::IntoResponse;

        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "pre-refresh-access",
                "single-use-refresh",
            ),
        );
        let refreshed_id_token = managed_auth("test@example.com", "acct_test", "unused", "unused")
            .pointer("/tokens/id_token")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_string();

        let (usage_authorization_tx, mut usage_authorization_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let (workspace_authorization_tx, mut workspace_authorization_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let service = axum::Router::new()
            .route(
                "/usage",
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let usage_authorization_tx = usage_authorization_tx.clone();
                    async move {
                        let authorization = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                        usage_authorization_tx.send(authorization.clone()).unwrap();
                        if authorization.as_deref() == Some("Bearer pre-refresh-access") {
                            return (
                                axum::http::StatusCode::UNAUTHORIZED,
                                axum::Json(serde_json::json!({"error": "expired"})),
                            )
                                .into_response();
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "plan_type": "plus",
                                "rate_limit": null,
                                "credits": null,
                                "spend_control": null,
                                "additional_rate_limits": null,
                                "rate_limit_reached_type": null
                            })),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post(move || {
                    let id_token = refreshed_id_token.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "id_token": id_token,
                            "access_token": "post-refresh-access",
                            "refresh_token": "rotated-refresh"
                        }))
                    }
                }),
            )
            .route(
                "/credits",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "available_count": 0,
                        "credits": []
                    }))
                }),
            )
            .route(
                "/accounts",
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let workspace_authorization_tx = workspace_authorization_tx.clone();
                    async move {
                        workspace_authorization_tx
                            .send(
                                headers
                                    .get(axum::http::header::AUTHORIZATION)
                                    .and_then(|value| value.to_str().ok())
                                    .map(str::to_string),
                            )
                            .unwrap();
                        axum::Json(serde_json::json!({
                            "accounts": [{
                                "id": "acct_test",
                                "name": "Workspace",
                                "structure": "workspace"
                            }],
                            "account_ordering": ["acct_test"]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        let _usage_url = EnvVarGuard::set_text("CS_USAGE_URL", &format!("http://{address}/usage"));
        let _token_url = EnvVarGuard::set_text("CS_TOKEN_URL", &format!("http://{address}/token"));
        let _credits_url =
            EnvVarGuard::set_text("CS_RESET_CREDITS_URL", &format!("http://{address}/credits"));
        let _accounts_url = EnvVarGuard::set_text(
            "CS_ACCOUNTS_CHECK_URL",
            &format!("http://{address}/accounts"),
        );

        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        app.fetch_usage_for(0, AccountRefreshPlan::usage_and_workspace(Refresh::Forced));
        assert!(
            app.workspace_lookup_tasks.is_empty() && app.workspace_requests.is_empty(),
            "combined refresh must preserve workspace as a post-usage plan"
        );

        let workspace_authorization =
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    app.poll_results();
                    app.poll_usage_lease_releases();
                    app.poll_workspace_lookup_tasks().await;
                    app.poll_account_tasks().await;
                    if let Ok(authorization) = workspace_authorization_rx.try_recv() {
                        break authorization;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("combined refresh must reach its deferred workspace lookup");

        assert_eq!(
            workspace_authorization.as_deref(),
            Some("Bearer post-refresh-access")
        );
        assert_eq!(
            usage_authorization_rx.try_recv().unwrap().as_deref(),
            Some("Bearer pre-refresh-access")
        );
        assert_eq!(
            usage_authorization_rx.try_recv().unwrap().as_deref(),
            Some("Bearer post-refresh-access")
        );
        assert!(usage_authorization_rx.try_recv().is_err());
        let stored = crate::auth::read_auth(&profile_path).unwrap();
        assert_eq!(
            stored
                .pointer("/tokens/access_token")
                .and_then(serde_json::Value::as_str),
            Some("post-refresh-access")
        );

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                app.poll_results();
                app.poll_usage_lease_releases();
                app.poll_workspace_lookup_tasks().await;
                app.poll_account_tasks().await;
                if app.account_tasks.is_empty()
                    && app.workspace_lookup_tasks.is_empty()
                    && app.workspace_requests.is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("usage enrichment and workspace lookup must settle");
        finish_workspace_cache_writes(&mut app).await;
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn workspace_lookup_does_not_reserve_a_network_slot_while_waiting_for_profile() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "access-token",
                "refresh-token",
            ),
        );

        let client = reqwest::Client::new();
        let held_lease = profile::acquire_profile_lease("account").unwrap();
        let mut app = App::new();
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        app.spawn_workspace_lookup("account".into(), profile_path, test_binding(), 1, client);

        tokio::task::yield_now().await;
        assert_eq!(
            app.usage_limiter.available_permits(),
            1,
            "profile contention must be resolved before reserving scarce network capacity"
        );

        app.workspace_lookup_tasks[&1].handle.abort();
        drop(held_lease);
        while !app.workspace_lookup_tasks[&1].handle.is_finished() {
            tokio::task::yield_now().await;
        }
        app.poll_workspace_lookup_tasks().await;
        assert!(app.workspace_lookup_tasks.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn workspace_network_wait_releases_the_profile_lease_for_switching() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "access-token",
                "refresh-token",
            ),
        );

        let auth = prepare_workspace_lookup_auth("account", &profile_path, &test_binding())
            .await
            .unwrap();
        let limiter = Arc::new(tokio::sync::Semaphore::new(0));
        let blocked_limiter = Arc::clone(&limiter);
        let network_wait = tokio::spawn(async move {
            let _auth = auth;
            let _permit = blocked_limiter.acquire().await.unwrap();
        });
        tokio::task::yield_now().await;
        assert!(!network_wait.is_finished());

        let switch_lease = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            profile::acquire_profile_lease_async("account"),
        )
        .await
        .expect("workspace permit wait must not delay the switch profile boundary")
        .unwrap();
        drop(switch_lease);

        network_wait.abort();
        let _ = network_wait.await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn usage_waiting_for_profile_does_not_reserve_a_network_slot() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/locked/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "locked@example.com",
                "acct_locked",
                "access-token",
                "refresh-token",
            ),
        );

        let held_lease = profile::acquire_profile_lease("locked").unwrap();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        profile::notify_on_test_lock_attempt("profile 'locked'", attempt_tx);

        let mut app = App::new();
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        app.accounts.push(AccountEntry {
            alias: "locked".into(),
            info: test_account_info_for("locked@example.com", "acct_locked"),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.fetch_usage_for(0, AccountRefreshPlan::usage_only(Refresh::Forced));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if attempt_rx.try_recv().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("usage worker must reach the contended profile boundary");
        assert_eq!(
            app.usage_limiter.available_permits(),
            1,
            "a locked alias must not occupy the only network slot"
        );

        let lease_control = app
            .account_tasks
            .values()
            .find(|task| matches!(task.kind, AccountTaskKind::Usage { .. }))
            .expect("usage task must remain tracked while waiting")
            .lease_control
            .clone();
        assert!(lease_control.cancel_waiting());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !app.account_tasks.is_empty() {
                app.poll_account_tasks().await;
                app.poll_results();
                app.poll_usage_lease_releases();
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pre-lease usage cancellation must settle");
        drop(held_lease);

        assert_eq!(app.usage_limiter.available_permits(), 1);
        assert!(!app.refreshing_requests.contains_key("locked"));
        assert_eq!(
            app.usage_lease_release_generations.get("locked"),
            app.usage_generations.get("locked")
        );
        assert!(matches!(app.accounts[0].usage, UsageStatus::Idle));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn model_and_warmup_lease_waits_do_not_reserve_a_network_slot() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let alias = "locked-detail";
        let binding = StrictAccountBinding {
            account_id: "acct_locked_detail".into(),
            email: "locked-detail@example.com".into(),
        };
        let profile_path = home.path().join("profiles/locked-detail/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                &binding.email,
                &binding.account_id,
                "access-token",
                "refresh-token",
            ),
        );
        let held_lease = profile::acquire_profile_lease(alias).unwrap();

        let mut app = App::new();
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        app.accounts.push(AccountEntry {
            alias: alias.into(),
            info: test_account_info_for(&binding.email, &binding.account_id),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        let (model_attempt_tx, model_attempt_rx) = std::sync::mpsc::channel();
        profile::notify_on_test_lock_attempt("profile 'locked-detail'", model_attempt_tx);
        app.ensure_models_loaded(alias);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if model_attempt_rx.try_recv().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("model worker must reach the contended profile boundary");
        assert_eq!(app.usage_limiter.available_permits(), 1);

        let (warmup_attempt_tx, warmup_attempt_rx) = std::sync::mpsc::channel();
        profile::notify_on_test_lock_attempt("profile 'locked-detail'", warmup_attempt_tx);
        assert!(app.spawn_preflighted_warmup(
            WarmupReadyCandidate {
                alias: alias.into(),
                binding,
                cached_usage: Some(UsageInfo::default()),
            },
            WarmupOrigin::Manual,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if warmup_attempt_rx.try_recv().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("warmup worker must reach the contended profile boundary");
        assert_eq!(
            app.usage_limiter.available_permits(),
            1,
            "model and warmup lease waiters must leave the only network slot available"
        );

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.drain_credential_tasks(),
        )
        .await
        .expect("pre-lease model and warmup cancellation must settle");
        drop(held_lease);
        assert_eq!(app.usage_limiter.available_permits(), 1);
    }

    #[tokio::test]
    async fn stopped_workspace_lookup_releases_the_latest_deduplication_generation() {
        let mut app = App::new();
        app.workspace_requests.insert("acct_test".into(), 7);
        app.workspace_lookup_tasks.insert(
            7,
            super::WorkspaceLookupTask {
                account_id: "acct_test".into(),
                generation: 7,
                handle: tokio::spawn(async { panic!("injected workspace lookup panic") }),
            },
        );
        while !app.workspace_lookup_tasks[&7].handle.is_finished() {
            tokio::task::yield_now().await;
        }

        app.poll_workspace_lookup_tasks().await;

        assert!(app.workspace_lookup_tasks.is_empty());
        assert!(!app.workspace_requests.contains_key("acct_test"));
        assert!(
            app.begin_workspace_request(&test_binding(), WorkspaceRefresh::IfStale)
                .is_some()
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn late_workspace_generation_cannot_overwrite_the_latest_memory_or_disk_value() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        let binding = test_binding();
        let slow_generation = app
            .begin_workspace_request(&binding, WorkspaceRefresh::Forced)
            .unwrap();
        let fast_generation = app
            .begin_workspace_request(&binding, WorkspaceRefresh::Forced)
            .unwrap();

        app.workspace_sender
            .try_send((
                "account".into(),
                binding.clone(),
                fast_generation,
                Ok(crate::cache::WorkspaceState::Named("Latest".into())),
            ))
            .unwrap();
        app.poll_results();
        app.workspace_sender
            .try_send((
                "account".into(),
                binding,
                slow_generation,
                Ok(crate::cache::WorkspaceState::Named("Stale".into())),
            ))
            .unwrap();
        app.poll_results();

        assert_eq!(
            app.accounts[0].info.workspace_name.as_deref(),
            Some("Latest")
        );
        finish_workspace_cache_writes(&mut app).await;
        let snapshot = crate::cache::get_snapshot(&[], &["acct_test".to_string()]).unwrap();
        assert_eq!(
            snapshot.workspaces.get("acct_test"),
            Some(&crate::cache::WorkspaceState::Named("Latest".into()))
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn workspace_cache_contention_never_blocks_result_application_and_only_latest_writes() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock).unwrap();

        let mut app = App::new();
        add_test_account(&mut app, "account");
        let binding = test_binding();
        let first_generation = app
            .begin_workspace_request(&binding, WorkspaceRefresh::Forced)
            .unwrap();
        app.workspace_sender
            .try_send((
                "account".into(),
                binding.clone(),
                first_generation,
                Ok(crate::cache::WorkspaceState::Named("First".into())),
            ))
            .unwrap();

        let started = Instant::now();
        app.poll_results();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "workspace result application must not wait for the cache lock"
        );
        assert_eq!(
            app.accounts[0].info.workspace_name.as_deref(),
            Some("First")
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let latest_generation = app
            .begin_workspace_request(&binding, WorkspaceRefresh::Forced)
            .unwrap();
        app.workspace_sender
            .try_send((
                "account".into(),
                binding,
                latest_generation,
                Ok(crate::cache::WorkspaceState::Named("Latest".into())),
            ))
            .unwrap();
        app.poll_results();
        assert_eq!(
            app.accounts[0].info.workspace_name.as_deref(),
            Some("Latest")
        );

        fs4::FileExt::unlock(&cache_lock).unwrap();
        finish_workspace_cache_writes(&mut app).await;
        let snapshot = crate::cache::get_snapshot(&[], &["acct_test".to_string()]).unwrap();
        assert_eq!(
            snapshot.workspaces.get("acct_test"),
            Some(&crate::cache::WorkspaceState::Named("Latest".into()))
        );
        assert!(app.workspace_cache_latest.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_cancels_a_workspace_cache_write_still_waiting_for_the_lock() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock).unwrap();

        let mut app = App::new();
        add_test_account(&mut app, "account");
        let binding = test_binding();
        let generation = app
            .begin_workspace_request(&binding, WorkspaceRefresh::Forced)
            .unwrap();
        app.workspace_sender
            .try_send((
                "account".into(),
                binding,
                generation,
                Ok(crate::cache::WorkspaceState::Named("Workspace".into())),
            ))
            .unwrap();
        app.poll_results();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            app.drain_credential_tasks(),
        )
        .await
        .expect("a derived cache write that has not acquired its lock must be cancellable");
        assert!(app.workspace_cache_writes.is_empty());
        assert!(app.workspace_cache_latest.is_empty());

        fs4::FileExt::unlock(&cache_lock).unwrap();
    }

    #[test]
    fn workspace_result_for_a_replaced_alias_owner_is_not_applied_or_cached() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info_for("replacement@example.com", "acct_replacement"),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        let previous = test_binding();
        let generation = app
            .begin_workspace_request(&previous, WorkspaceRefresh::IfStale)
            .unwrap();
        app.workspace_sender
            .try_send((
                "account".into(),
                previous,
                generation,
                Ok(crate::cache::WorkspaceState::Named("Previous".into())),
            ))
            .unwrap();

        app.poll_results();

        assert!(app.accounts[0].info.workspace_name.is_none());
        assert!(!app.workspace_requests.contains_key("acct_test"));
        let snapshot = crate::cache::get_snapshot(&[], &["acct_test".to_string()]).unwrap();
        assert_eq!(
            snapshot.workspaces.get("acct_test"),
            Some(&crate::cache::WorkspaceState::Unresolved)
        );
    }

    #[test]
    fn queued_follow_ups_merge_usage_and_workspace_intent_independently() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (1, Refresh::Cached));

        app.fetch_usage_for(0, AccountRefreshPlan::usage_only(Refresh::Forced));
        app.fetch_usage_for(
            0,
            AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
        );

        assert_eq!(
            app.pending_usage_refreshes.get("account"),
            Some(&AccountRefreshPlan {
                usage: Some(Refresh::Forced),
                workspace: WorkspaceRefresh::IfStale,
            })
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn forced_core_refresh_supersedes_deferred_metadata_without_waiting_for_it() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.usage_generations.insert("account".into(), 5);
        app.usage_metadata_requests.insert("account".into(), 5);
        app.usage_next_id = 6;

        app.fetch_usage_for(0, AccountRefreshPlan::usage_and_workspace(Refresh::Forced));

        assert_eq!(
            app.refreshing_requests.get("account").map(|(id, _)| *id),
            Some(6)
        );
        assert_eq!(app.usage_generations.get("account"), Some(&6));
        assert_eq!(app.usage_metadata_requests.get("account"), Some(&6));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_refresh_keeps_last_loaded_usage_visible_while_request_is_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);

        app.refresh_indices(&[0], Refresh::Forced);

        assert!(
            matches!(app.accounts[0].usage, UsageStatus::Loaded(_)),
            "force refresh must retain the last value until its replacement arrives"
        );
        assert_eq!(app.loading_count(), 1);
    }

    #[test]
    fn profile_reload_retains_loaded_usage_with_its_strict_identity() {
        let retained = retained_usage_by_identity(vec![AccountEntry {
            alias: "account".into(),
            info: AccountInfo {
                account_id: Some("acct_account".into()),
                email: Some("account@example.com".into()),
                ..AccountInfo::default()
            },
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        }]);

        assert!(matches!(
            retained.get("account"),
            Some((Some(_), UsageStatus::Loaded(_)))
        ));
    }

    #[test]
    fn startup_request_order_prioritizes_selection_then_current_without_reordering_rows() {
        let mut app = App::new();
        for (alias, is_current) in [
            ("first", false),
            ("current", true),
            ("middle", false),
            ("selected", false),
        ] {
            app.accounts.push(AccountEntry {
                alias: alias.into(),
                info: test_account_info_for(
                    &format!("{alias}@example.com"),
                    &format!("acct_{alias}"),
                ),
                usage: UsageStatus::Idle,
                is_current,
            });
        }
        app.view_indices = vec![2, 1, 3, 0];
        app.selected = 2;
        let displayed = app.view_indices.clone();

        let ordered = app.startup_request_order(&displayed);

        assert_eq!(ordered, vec![3, 1, 2, 0]);
        assert_eq!(app.view_indices, displayed);
    }

    #[test]
    fn startup_ready_transition_is_a_no_op_for_another_phase() {
        let mut app = App::new();
        let mut phase = StartupUsagePhase::RefreshingCore {
            ordered_aliases: vec!["account".into()],
        };

        assert!(!advance_startup_ready_phase(&mut app, &mut phase));
        assert!(matches!(
            phase,
            StartupUsagePhase::RefreshingCore { ref ordered_aliases }
                if ordered_aliases == &["account".to_string()]
        ));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn startup_cache_error_is_reported_without_skipping_core_dispatch() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "test@example.com",
                "acct_test",
                "access-token",
                "refresh-token",
            ),
        );

        let mut app = App::new();
        add_test_account(&mut app, "account");
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let mut phase = StartupUsagePhase::Ready {
            cache_error: Some("Could not read usage cache: injected failure".into()),
        };

        assert!(advance_startup_ready_phase(&mut app, &mut phase));

        assert!(app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("injected failure"))
        );
        assert!(matches!(
            phase,
            StartupUsagePhase::RefreshingCore { ref ordered_aliases }
                if ordered_aliases == &["account".to_string()]
        ));
        assert!(app.refreshing_requests.contains_key("account"));
        assert_eq!(
            app.pending_usage_refreshes.get("account"),
            Some(&AccountRefreshPlan::workspace_only(
                WorkspaceRefresh::IfStale
            ))
        );
        app.drain_credential_tasks().await;
    }

    #[test]
    fn unchanged_pending_poll_does_not_request_a_redraw() {
        let mut app = App::new();
        let (_sender, receiver) = tokio::sync::oneshot::channel();
        app.update_rx = Some(receiver);
        let before = app.render_revision();

        app.poll_update();

        assert!(app.update_rx.is_some());
        assert!(!redraw_after_poll(
            false,
            before,
            app.render_revision(),
            42,
            42,
        ));
        assert!(redraw_after_poll(
            false,
            before,
            app.render_revision(),
            42,
            43,
        ));
    }

    #[test]
    fn startup_cache_only_fills_accounts_without_a_settled_result() {
        let account = |alias: &str, usage| AccountEntry {
            alias: alias.into(),
            info: AccountInfo {
                account_id: Some(format!("acct_{alias}")),
                email: Some(format!("{alias}@example.com")),
                ..AccountInfo::default()
            },
            usage,
            is_current: false,
        };
        let usage = |fetched_at| UsageInfo {
            fetched_at: Some(fetched_at),
            ..UsageInfo::default()
        };
        let mut app = App::new();
        app.accounts = vec![
            account("idle", UsageStatus::Idle),
            account("loading", UsageStatus::Loading),
            account("loaded", UsageStatus::Loaded(Box::new(usage(30)))),
            account(
                "error",
                UsageStatus::Error(crate::usage::UsageError {
                    summary: "new failure".into(),
                    detail: "keep the settled network result".into(),
                }),
            ),
        ];
        let cached = ["idle", "loading", "loaded", "error"]
            .into_iter()
            .map(|alias| (alias.to_string(), usage(10)))
            .collect();
        let identities = app
            .accounts
            .iter()
            .map(|account| {
                (
                    account.alias.clone(),
                    strict_account_identity(&account.info).unwrap(),
                )
            })
            .collect();

        app.apply_cached_usage(cached, CachedUsageApplication::Startup, Some(&identities));

        assert!(matches!(
            &app.accounts[0].usage,
            UsageStatus::Loaded(usage) if usage.fetched_at == Some(10)
        ));
        assert!(matches!(
            &app.accounts[1].usage,
            UsageStatus::Loaded(usage) if usage.fetched_at == Some(10)
        ));
        assert!(matches!(
            &app.accounts[2].usage,
            UsageStatus::Loaded(usage) if usage.fetched_at == Some(30)
        ));
        assert!(matches!(&app.accounts[3].usage, UsageStatus::Error(_)));
    }

    #[test]
    fn startup_snapshot_does_not_cross_a_reused_alias_identity() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo {
                account_id: Some("acct_replacement".into()),
                email: Some("replacement@example.com".into()),
                ..AccountInfo::default()
            },
            usage: UsageStatus::Idle,
            is_current: false,
        });
        let startup_identities = [(
            "account".to_string(),
            crate::jwt::StrictAccountBinding {
                account_id: "acct_original".into(),
                email: "original@example.com".into(),
            },
        )]
        .into_iter()
        .collect();
        let snapshot = crate::cache::CacheSnapshot {
            usage: [("account".to_string(), UsageInfo::default())]
                .into_iter()
                .collect(),
            workspaces: [(
                "acct_original".to_string(),
                crate::cache::WorkspaceState::Named("Original workspace".into()),
            )]
            .into_iter()
            .collect(),
            workspace_fresh_for: [(
                "acct_original".to_string(),
                std::time::Duration::from_secs(60),
            )]
            .into_iter()
            .collect(),
        };

        app.apply_startup_cache_snapshot(snapshot, &startup_identities);

        assert!(matches!(app.accounts[0].usage, UsageStatus::Idle));
        assert!(app.accounts[0].info.workspace_name.is_none());
        assert!(app.workspace_states.is_empty());
    }

    #[test]
    fn expired_workspace_resolution_is_removed_without_per_frame_scanning() {
        let mut app = App::new();
        let mut info = test_account_info();
        info.workspace_name = Some("Old workspace".to_string());
        app.accounts.push(AccountEntry {
            alias: "account".to_string(),
            info,
            usage: UsageStatus::Idle,
            is_current: false,
        });
        let expired_at = Instant::now() - std::time::Duration::from_millis(1);
        app.workspace_states.insert(
            "acct_test".to_string(),
            super::WorkspaceMemoryResolution {
                state: crate::cache::WorkspaceState::Named("Old workspace".to_string()),
                fresh_until: expired_at,
            },
        );
        app.workspace_next_expiry = Some(expired_at);

        app.tick();

        assert!(app.workspace_states.is_empty());
        assert!(app.workspace_next_expiry.is_none());
        assert!(app.accounts[0].info.workspace_name.is_none());
    }

    #[test]
    fn explicit_cached_refresh_can_restore_an_account_from_error() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Error(crate::usage::UsageError {
                summary: "old failure".into(),
                detail: "replace from the requested cache read".into(),
            }),
            is_current: false,
        });
        app.apply_cached_usage(
            [("account".into(), UsageInfo::default())]
                .into_iter()
                .collect(),
            CachedUsageApplication::RequestedRefresh,
            None,
        );

        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
    }

    #[test]
    fn account_refresh_plan_preserves_existing_general_workspace_policy() {
        let unattended = AccountRefreshPlan::usage_and_workspace(Refresh::Unattended);
        let forced = AccountRefreshPlan::usage_and_workspace(Refresh::Forced);

        assert!(refresh_fetches_loaded_usage(Refresh::Unattended));
        assert_eq!(unattended.workspace, WorkspaceRefresh::IfStale);
        assert_eq!(forced.workspace, WorkspaceRefresh::Forced);
        assert_eq!(
            AccountRefreshPlan::usage_only(Refresh::Forced).workspace,
            WorkspaceRefresh::Skip
        );
        assert_eq!(
            AccountRefreshPlan::resume_cancelled_usage(Refresh::Cached),
            AccountRefreshPlan::usage_only(Refresh::Unattended)
        );
    }

    #[test]
    fn account_refresh_plan_merges_usage_and_workspace_intent_independently() {
        let usage_only_forced = AccountRefreshPlan::usage_only(Refresh::Forced);
        let unattended = AccountRefreshPlan::usage_and_workspace(Refresh::Unattended);

        assert_eq!(
            usage_only_forced.merged_with(unattended),
            AccountRefreshPlan {
                usage: Some(Refresh::Forced),
                workspace: WorkspaceRefresh::IfStale,
            }
        );
        assert_eq!(
            usage_only_forced
                .merged_with(AccountRefreshPlan::usage_and_workspace(Refresh::Forced,)),
            AccountRefreshPlan::usage_and_workspace(Refresh::Forced)
        );
    }

    #[test]
    fn deferred_refreshes_merge_usage_and_workspace_intent_independently() {
        let mut app = App::new();
        app.defer_post_switch_usage_refresh(
            "account".into(),
            AccountRefreshPlan::usage_only(Refresh::Forced),
        );
        app.defer_post_switch_usage_refresh(
            "account".into(),
            AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
        );

        assert_eq!(
            app.deferred_post_switch_usage_refreshes.get("account"),
            Some(&AccountRefreshPlan {
                usage: Some(Refresh::Forced),
                workspace: WorkspaceRefresh::IfStale,
            })
        );
    }

    #[test]
    fn account_detail_formats_workspaces_and_reset_cards_without_raw_ids() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo {
                organizations: vec![OrgInfo {
                    id: "org-secret-looking-id".into(),
                    title: "Night City".into(),
                    role: "owner".into(),
                    is_default: true,
                }],
                ..Default::default()
            },
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits_available_count: Some(1),
                reset_credits: vec![ResetCredit {
                    id: "credit-secret-looking-id".into(),
                    granted_at: Some("2026-07-01T08:00:00Z".into()),
                    expires_at: Some("2026-07-20T08:00:00Z".into()),
                }],
                ..Default::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));

        app.open_account_menu();

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should open");
        };
        assert_eq!(
            info.organizations,
            vec!["Night City · Owner · default workspace"]
        );
        assert!(info.reset_card_expiries[0].contains("expires 2026-07-20"));
        assert!(!info.reset_card_expiries[0].contains("credit-secret-looking-id"));
        assert!(!info.organizations[0].contains("org-secret-looking-id"));
    }

    #[test]
    fn reset_card_confirmation_pins_identity_and_pending_use_disables_a_second_consent() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits: vec![ResetCredit {
                    id: "confirmed-credit".into(),
                    granted_at: None,
                    expires_at: Some("2026-08-30T00:00:00Z".into()),
                }],
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);

        app.request_consume_reset_card("account");
        let Some(ConfirmAction::ConsumeResetCard { credit, .. }) = app.confirm.as_ref() else {
            panic!("reset-card confirmation should open");
        };
        assert_eq!(credit.id, "confirmed-credit");

        app.cancel_confirm();
        app.reset_cards_in_flight.insert("account".into());
        app.request_consume_reset_card("account");
        assert!(app.confirm.is_none());

        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));
        app.open_account_menu();
        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should open");
        };
        assert!(!info.can_consume_reset_card);
    }

    #[test]
    fn hard_blocker_disables_reset_card_menu_and_confirmation() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "blocked".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits: vec![ResetCredit {
                    id: "cannot-help".into(),
                    granted_at: None,
                    expires_at: None,
                }],
                account_limited: true,
                spend_control_reached: true,
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);

        app.request_consume_reset_card("blocked");

        assert!(app.confirm.is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("spend_control_reached"))
        );
        app.model_cache
            .insert("blocked".into(), ModelStatus::Loaded(Vec::new()));
        app.open_account_menu();
        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should open");
        };
        assert!(!info.can_consume_reset_card);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_card_forced_preflight_blocker_sends_no_consume_request() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Keep the crate-wide order URL_ENV_LOCK -> TEST_ENV_LOCK. This test
        // redirects usage endpoints as well as the process-global profile home.
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "access-token",
                "refresh-token",
            ),
        );
        let confirmed = ResetCredit {
            id: "confirmed-credit".into(),
            granted_at: Some("2026-08-01T00:00:00Z".into()),
            expires_at: Some("2026-09-01T00:00:00Z".into()),
        };
        let usage_body = serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "limit_reached": true,
                "primary_window": {
                    "used_percent": 100.0,
                    "reset_at": 2_000_000_000_i64,
                    "limit_window_seconds": 18_000
                },
                "secondary_window": {
                    "used_percent": 50.0,
                    "reset_at": 2_000_600_000_i64,
                    "limit_window_seconds": 604_800
                }
            },
            "rate_limit_reached_type": {
                "type": "workspace_owner_credits_depleted"
            }
        });
        let credit_body = serde_json::json!({
            "available_count": 1,
            "credits": [{
                "id": "confirmed-credit",
                "reset_type": "codex_rate_limits",
                "status": "available",
                "granted_at": "2026-08-01T00:00:00Z",
                "expires_at": "2026-09-01T00:00:00Z"
            }]
        });
        let consume_requests = Arc::new(AtomicUsize::new(0));
        let consume_requests_for_route = Arc::clone(&consume_requests);
        let server = axum::Router::new()
            .route(
                "/usage",
                axum::routing::get(move || {
                    let body = usage_body.clone();
                    async move { axum::Json(body) }
                }),
            )
            .route(
                "/credits",
                axum::routing::get(move || {
                    let body = credit_body.clone();
                    async move { axum::Json(body) }
                }),
            )
            .route(
                "/consume",
                axum::routing::post(move || {
                    let requests = Arc::clone(&consume_requests_for_route);
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({"code": "reset"}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let _usage_url = EnvVarGuard::set_text("CS_USAGE_URL", &format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set_text("CS_RESET_CREDITS_URL", &format!("http://{address}/credits"));
        let _consume_url = EnvVarGuard::set_text(
            "CS_RESET_CREDITS_CONSUME_URL",
            &format!("http://{address}/consume"),
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".to_string(),
            info: test_account_info_for("account@example.com", "acct_account"),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.consume_reset_card("account", confirmed);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                app.poll_account_tasks().await;
                app.poll_reset_card_results();
                if app.account_tasks.is_empty() && app.reset_cards_in_flight.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reset-card preflight must settle");

        assert_eq!(consume_requests.load(Ordering::SeqCst), 0);
        assert!(app.status_is_error);
        assert!(
            app.status_msg.as_deref().is_some_and(|message| {
                message.contains("workspace_owner_credits_depleted")
                    && message.contains("no reset card was requested")
            }),
            "unexpected status: {:?}",
            app.status_msg
        );
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_card_http_phases_obey_the_shared_network_limit() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_path = home.path().join("profiles/account/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &managed_auth(
                "account@example.com",
                "acct_account",
                "access-token",
                "refresh-token",
            ),
        );
        let confirmed = ResetCredit {
            id: "confirmed-credit".into(),
            granted_at: Some("2026-08-01T00:00:00Z".into()),
            expires_at: Some("2026-09-01T00:00:00Z".into()),
        };

        let (usage_started_tx, mut usage_started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (credits_started_tx, mut credits_started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (consume_started_tx, mut consume_started_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = axum::Router::new()
            .route(
                "/usage",
                axum::routing::get(move || {
                    let started = usage_started_tx.clone();
                    async move {
                        started.send(()).unwrap();
                        axum::Json(serde_json::json!({
                            "plan_type": "plus",
                            "rate_limit": {
                                "limit_reached": true,
                                "primary_window": {
                                    "used_percent": 100.0,
                                    "reset_at": 2_000_000_000_i64,
                                    "limit_window_seconds": 18_000
                                },
                                "secondary_window": {
                                    "used_percent": 50.0,
                                    "reset_at": 2_000_600_000_i64,
                                    "limit_window_seconds": 604_800
                                }
                            },
                            "rate_limit_reached_type": null,
                            "credits": null,
                            "spend_control": null,
                            "additional_rate_limits": null
                        }))
                    }
                }),
            )
            .route(
                "/credits",
                axum::routing::get(move || {
                    let started = credits_started_tx.clone();
                    async move {
                        started.send(()).unwrap();
                        axum::Json(serde_json::json!({
                            "available_count": 1,
                            "credits": [{
                                "id": "confirmed-credit",
                                "reset_type": "codex_rate_limits",
                                "status": "available",
                                "granted_at": "2026-08-01T00:00:00Z",
                                "expires_at": "2026-09-01T00:00:00Z"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/consume",
                axum::routing::post(move || {
                    let started = consume_started_tx.clone();
                    async move {
                        started.send(()).unwrap();
                        axum::Json(serde_json::json!({
                            "code": "reset",
                            "windows_reset": 2
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let _usage_url = EnvVarGuard::set_text("CS_USAGE_URL", &format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set_text("CS_RESET_CREDITS_URL", &format!("http://{address}/credits"));
        let _consume_url = EnvVarGuard::set_text(
            "CS_RESET_CREDITS_CONSUME_URL",
            &format!("http://{address}/consume"),
        );

        let cache_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock).unwrap();

        let mut app = App::new();
        app.usage_limiter = Arc::new(tokio::sync::Semaphore::new(1));
        app.accounts.push(AccountEntry {
            alias: "account".to_string(),
            info: test_account_info_for("account@example.com", "acct_account"),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        let preflight_gate = app.usage_limiter.clone().acquire_owned().await.unwrap();
        app.consume_reset_card("account", confirmed);

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(150),
                usage_started_rx.recv(),
            )
            .await
            .is_err(),
            "forced quota preflight bypassed the occupied network slot"
        );
        drop(preflight_gate);
        tokio::time::timeout(std::time::Duration::from_secs(2), usage_started_rx.recv())
            .await
            .expect("quota preflight did not start after capacity was released")
            .expect("quota preflight channel closed");
        tokio::time::timeout(std::time::Duration::from_secs(2), credits_started_rx.recv())
            .await
            .expect("reset-credit preflight did not start")
            .expect("reset-credit preflight channel closed");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while app.usage_limiter.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("preflight retained its network slot while waiting to publish cache state");

        let consume_gate = app.usage_limiter.clone().acquire_owned().await.unwrap();
        fs4::FileExt::unlock(&cache_lock).unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(150),
                consume_started_rx.recv(),
            )
            .await
            .is_err(),
            "reset-card POST bypassed the occupied network slot"
        );
        drop(consume_gate);
        tokio::time::timeout(std::time::Duration::from_secs(2), consume_started_rx.recv())
            .await
            .expect("reset-card POST did not start after capacity was released")
            .expect("reset-card consume channel closed");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while app
                .account_tasks
                .values()
                .any(|task| !task.handle.is_finished())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reset-card worker did not settle");

        assert_eq!(app.usage_limiter.available_permits(), 1);
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn visible_rename_error_reloads_committed_alias_and_reports_warning() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();
        let new_path = home.path().join("profiles/new/auth.json");
        std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &new_path,
            &managed_auth("account@example.com", "acct_account", "access", "refresh"),
        );

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "old".into(),
            info: AccountInfo {
                account_id: Some("acct_account".into()),
                email: Some("account@example.com".into()),
                ..AccountInfo::default()
            },
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.view_indices.push(0);
        app.marked.insert("old".into());
        app.model_cache
            .insert("old".into(), ModelStatus::Loaded(Vec::new()));

        app.reconcile_rename_result(
            "old",
            "new",
            Ok(
                profile::ProfileMutationOutcome::test_committed_with_durability_warning(
                    anyhow::anyhow!("directory durability was not confirmed"),
                ),
            ),
            super::load_profile_reload_snapshot(false),
        );

        assert_eq!(
            app.accounts
                .iter()
                .map(|entry| entry.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["new"]
        );
        assert!(app.marked.contains("new"));
        assert!(!app.marked.contains("old"));
        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert!(matches!(
            app.model_cache.get("new"),
            Some(ModelStatus::Loaded(_))
        ));
        assert!(!app.model_cache.contains_key("old"));
        assert!(app.account_tasks.is_empty());
        assert!(app.refreshing_requests.is_empty());
        assert!(!app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("durability could not be confirmed"))
        );
        app.drain_credential_tasks().await;
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn visible_delete_error_removes_stale_row_and_reports_warning() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        std::fs::create_dir_all(&codex_home).unwrap();
        let remaining_path = home.path().join("profiles/remaining/auth.json");
        std::fs::create_dir_all(remaining_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &remaining_path,
            &managed_auth(
                "remaining@example.com",
                "acct_remaining",
                "access",
                "refresh",
            ),
        );

        let mut app = App::new();
        app.accounts.extend([
            AccountEntry {
                alias: "deleted".into(),
                info: AccountInfo::default(),
                usage: UsageStatus::Idle,
                is_current: false,
            },
            AccountEntry {
                alias: "remaining".into(),
                info: AccountInfo {
                    account_id: Some("acct_remaining".into()),
                    email: Some("remaining@example.com".into()),
                    ..AccountInfo::default()
                },
                usage: UsageStatus::Loaded(Box::default()),
                is_current: false,
            },
        ]);
        app.view_indices.extend([0, 1]);

        app.reconcile_delete_result(
            "deleted",
            Ok(
                profile::ProfileMutationOutcome::test_committed_with_durability_warning(
                    anyhow::anyhow!("directory durability was not confirmed"),
                ),
            ),
            super::load_profile_reload_snapshot(false),
        );

        assert_eq!(app.accounts.len(), 1);
        assert_eq!(app.accounts[0].alias, "remaining");
        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert!(app.account_tasks.is_empty());
        assert!(app.refreshing_requests.is_empty());
        assert!(!app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("durability could not be confirmed"))
        );
    }

    #[test]
    fn registry_reload_preserves_an_unrelated_in_flight_usage_generation() {
        let mut app = App::new();
        let removed_info = test_account_info_for("removed@example.com", "acct_removed");
        let retained_info = test_account_info_for("retained@example.com", "acct_retained");
        app.accounts.extend([
            AccountEntry {
                alias: "removed".into(),
                info: removed_info,
                usage: UsageStatus::Idle,
                is_current: false,
            },
            AccountEntry {
                alias: "retained".into(),
                info: retained_info.clone(),
                usage: UsageStatus::Loading,
                is_current: false,
            },
        ]);
        app.view_indices.extend([0, 1]);
        app.refreshing_requests
            .insert("retained".into(), (17, Refresh::Forced));
        app.usage_generations.insert("retained".into(), 17);
        app.usage_metadata_requests.insert("retained".into(), 17);
        app.pending_usage_refreshes.insert(
            "retained".into(),
            AccountRefreshPlan::usage_and_workspace(Refresh::Unattended),
        );

        app.apply_loaded_profiles(
            None,
            vec![("retained".into(), retained_info)],
            &std::collections::BTreeSet::new(),
        );

        assert!(matches!(app.accounts[0].usage, UsageStatus::Loading));
        assert_eq!(
            app.refreshing_requests.get("retained"),
            Some(&(17, Refresh::Forced))
        );
        assert_eq!(app.usage_generations.get("retained"), Some(&17));
        assert_eq!(app.usage_metadata_requests.get("retained"), Some(&17));
        assert_eq!(
            app.pending_usage_refreshes.get("retained"),
            Some(&AccountRefreshPlan::usage_and_workspace(
                Refresh::Unattended
            ))
        );
        app.pending_usage_refreshes.remove("retained");

        app.result_sender
            .try_send((
                "retained".into(),
                StrictAccountBinding {
                    account_id: "acct_retained".into(),
                    email: "retained@example.com".into(),
                },
                17,
                Ok(UsageInfo::default()),
            ))
            .unwrap();
        app.poll_results();

        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert!(!app.refreshing_requests.contains_key("retained"));
    }

    #[test]
    fn mixed_batch_delete_counts_committed_warning_and_true_failure_truthfully() {
        let mut report = BatchDeleteReport::default();
        report.record(
            "durable",
            Ok(profile::ProfileMutationOutcome::test_committed()),
        );
        report.record(
            "warning",
            Ok(
                profile::ProfileMutationOutcome::test_committed_with_durability_warning(
                    anyhow::anyhow!("directory sync failed"),
                ),
            ),
        );
        report.record("failed", Err(anyhow::anyhow!("permission denied")));

        assert_eq!(report.committed, 2);
        assert_eq!(report.durability_warnings.len(), 1);
        assert_eq!(report.failures.len(), 1);
        let message = report.message();
        assert!(message.contains("Deleted 2 account(s)"), "{message}");
        assert!(
            message.contains("durability unconfirmed for 1"),
            "{message}"
        );
        assert!(message.contains("warning"), "{message}");
        assert!(message.contains("1 failed"), "{message}");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn completed_reset_card_refreshes_usage_without_workspace_lookup() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.reset_cards_in_flight.insert("account".into());
        app.reset_card_sender
            .try_send((
                "account".into(),
                test_binding(),
                Ok(crate::usage::ConsumedResetCredit {
                    credit: ResetCredit::default(),
                    code: None,
                    windows_reset: None,
                    redeemed_at: None,
                }),
            ))
            .unwrap();

        app.poll_reset_card_results();
        assert!(app.reset_cards_in_flight.contains("account"));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !app.usage_cache_invalidation_tasks.is_empty() {
                tokio::task::yield_now().await;
                app.poll_usage_cache_invalidations().await;
            }
        })
        .await
        .expect("reset-card cache invalidation must finish");

        assert_eq!(
            app.refreshing_requests
                .get("account")
                .map(|(_, refresh)| *refresh),
            Some(Refresh::Forced)
        );
        assert!(app.workspace_requests.is_empty());
        app.drain_credential_tasks().await;
    }

    #[tokio::test]
    async fn completed_reset_card_surfaces_cache_invalidation_failure_before_refresh() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: test_account_info(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_limiter = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        app.reset_cards_in_flight.insert("account".into());
        app.usage_cache_invalidation_tasks.insert(
            7,
            super::UsageCacheInvalidationTask {
                alias: "account".into(),
                binding: test_binding(),
                refresh_after: Some(AccountRefreshPlan::usage_only(Refresh::Forced)),
                warning_on_failure: None,
                handle: tokio::spawn(async {
                    Err(anyhow::anyhow!("injected cache invalidation failure"))
                }),
            },
        );

        while !app.usage_cache_invalidation_tasks[&7].handle.is_finished() {
            tokio::task::yield_now().await;
        }
        app.poll_usage_cache_invalidations().await;

        assert!(!app.reset_cards_in_flight.contains("account"));
        assert!(app.status_is_error);
        let status = app.status_msg.as_deref().unwrap_or_default();
        assert!(
            status.contains("cached usage could not be cleared"),
            "{status}"
        );
        assert!(
            status.contains("injected cache invalidation failure"),
            "{status}"
        );
        assert_eq!(
            app.refreshing_requests
                .get("account")
                .map(|(_, refresh)| *refresh),
            Some(Refresh::Forced)
        );
        app.drain_credential_tasks().await;
    }

    #[test]
    fn reset_card_result_always_clears_in_flight_state() {
        let mut app = App::new();
        app.reset_cards_in_flight.insert("account".into());
        app.reset_card_sender
            .try_send((
                "account".into(),
                test_binding(),
                Err(super::map_reset_card_failure("failed".into(), false)),
            ))
            .unwrap();

        app.poll_reset_card_results();

        assert!(!app.reset_cards_in_flight.contains("account"));
    }

    #[test]
    fn unknown_reset_card_outcome_invalidates_cache_and_uses_safe_message() {
        let failure = reset_card_failure_from_outcome(
            true,
            "account: reset-card consumption may have occurred; verify before retry".to_string(),
            "Reset card failed (account): HTTP 400".to_string(),
        );

        // Unknown outcome must invalidate the cache: the card may have been consumed,
        // so a stale "still available" cache entry could let the UI burn a second one.
        assert!(failure.invalidate_cache);
        assert!(failure.message.contains("account"));
        assert!(failure.message.contains("consumption may have occurred"));
        assert!(failure.message.contains("verify before retry"));
        // Must route to the safe message, never the raw definite-failure text.
        assert!(!failure.message.contains("HTTP 400"));
    }

    #[test]
    fn definite_reset_card_outcome_keeps_accurate_error_without_invalidation() {
        let failure = reset_card_failure_from_outcome(
            false,
            "account: reset-card consumption may have occurred; verify before retry".to_string(),
            "Reset card failed (account): HTTP 400".to_string(),
        );

        // Definite (unconsumed) outcome must NOT invalidate the cache, and must surface
        // the accurate error rather than the unknown-outcome safe message.
        assert!(!failure.invalidate_cache);
        assert_eq!(failure.message, "Reset card failed (account): HTTP 400");
    }

    #[test]
    fn status_storage_removes_terminal_controls_and_bounds_untrusted_errors() {
        let mut app = App::new();
        let hostile = format!(
            "server\u{1b}]52;clipboard\u{7}\n{}",
            "한".repeat(STATUS_MESSAGE_MAX_CHARS + 50)
        );

        app.set_status_error(hostile, 5);

        let stored = app.status_msg.as_deref().expect("stored status");
        assert!(stored.chars().all(|character| !character.is_control()));
        assert_eq!(stored.chars().count(), STATUS_MESSAGE_MAX_CHARS);
        assert!(app.status_is_error);
    }
}
