use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{DefaultTerminal, style::Style, text::Line};
use tokio::sync::Semaphore;

use crate::auth;
use crate::cache;
use crate::jwt::AccountInfo;
use crate::login;
use crate::output::{format_local_datetime, format_local_timestamp, reset_credits_count};
use crate::profile::{
    self, cmd_delete, list_profiles, profile_auth_path, rename_profile, sync_current_from_live,
    validate_alias,
};
use crate::safe_text;
use crate::usage::{
    ConsumedResetCredit, GlobalPaceAccountInput, GlobalWeeklySummary, Refresh, ResetCredit,
    UsageError, UsageInfo, calculate_global_weekly_summary, reset_credit_expiry_sort_key,
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

fn retained_usage_by_alias(accounts: Vec<AccountEntry>) -> HashMap<String, UsageStatus> {
    accounts
        .into_iter()
        .map(|account| (account.alias, account.usage))
        .collect()
}

fn refresh_fetches_loaded_usage(refresh: Refresh) -> bool {
    !matches!(refresh, Refresh::Cached)
}

fn refresh_forces_negative_caches(refresh: Refresh) -> bool {
    matches!(refresh, Refresh::Forced)
}

fn refresh_priority(refresh: Refresh) -> u8 {
    match refresh {
        Refresh::Cached => 0,
        Refresh::Unattended => 1,
        Refresh::Forced => 2,
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
    started: Instant,
    slow_reported: bool,
    lease_control: profile::ProfileLeaseAcquireControl,
    handle: tokio::task::JoinHandle<std::result::Result<(), String>>,
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
    loaded_usage: Option<UsageInfo>,
}

#[derive(Debug)]
struct WarmupPreflightTask {
    origin: WarmupPreflightOrigin,
    candidate_count: usize,
    aliases: BTreeSet<String>,
    handle: tokio::task::JoinHandle<Result<Vec<String>>>,
}

fn inspect_warmup_candidates(candidates: Vec<WarmupPreflightCandidate>) -> Result<Vec<String>> {
    let disk_aliases = candidates
        .iter()
        .filter(|candidate| candidate.loaded_usage.is_none())
        .map(|candidate| candidate.alias.clone())
        .collect::<Vec<_>>();
    let mut cached_usage = if disk_aliases.is_empty() {
        HashMap::new()
    } else {
        crate::cache::get_many(&disk_aliases)
            .context("reading cached usage for warmup candidates")?
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
            ready.push(candidate.alias);
        }
    }
    ready.sort();
    Ok(ready)
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

enum ProfileSwitchTaskResult {
    Prepared {
        result: Result<profile::PreparedProfileSwitch>,
        live_sync_attempted: bool,
    },
    LiveSynchronized(Result<()>),
    Committed(Result<profile::ProfileSwitchOutcome>),
}

#[derive(Debug)]
struct AccountTask {
    alias: String,
    kind: AccountTaskKind,
    lease_control: profile::ProfileLeaseAcquireControl,
    handle: tokio::task::JoinHandle<()>,
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
    pub pending_usage_refreshes: HashMap<String, Refresh>,
    pub usage_next_id: u64,
    pub pending_results: tokio::sync::mpsc::Receiver<(String, u64, Result<UsageInfo, UsageError>)>,
    pub result_sender: tokio::sync::mpsc::Sender<(String, u64, Result<UsageInfo, UsageError>)>,
    pub pending_workspace: tokio::sync::mpsc::Receiver<String>,
    pub workspace_sender: tokio::sync::mpsc::Sender<String>,
    pub pending_reset_cards:
        tokio::sync::mpsc::Receiver<(String, Result<ConsumedResetCredit, ResetCardFailure>)>,
    pub reset_card_sender:
        tokio::sync::mpsc::Sender<(String, Result<ConsumedResetCredit, ResetCardFailure>)>,
    pending_profile_switches: tokio::sync::mpsc::Receiver<(String, ProfileSwitchTaskResult)>,
    profile_switch_sender: tokio::sync::mpsc::Sender<(String, ProfileSwitchTaskResult)>,
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
    shutting_down: bool,
    pub confirm: Option<ConfirmAction>,
    pub rename: Option<RenameState>,
    pub usage_limiter: Arc<Semaphore>,
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
    pub pending_models: tokio::sync::mpsc::Receiver<(String, u64, Result<Vec<ModelEntry>, String>)>,
    pub model_sender: tokio::sync::mpsc::Sender<(String, u64, Result<Vec<ModelEntry>, String>)>,
    pub model_requests: HashMap<String, u64>,
    pub model_next_id: u64,
}

impl App {
    pub fn new() -> Self {
        #[cfg(test)]
        crate::config::init_defaults_for_tests();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
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
            usage_next_id: 0,
            pending_results: rx,
            result_sender: tx,
            pending_workspace: workspace_rx,
            workspace_sender: workspace_tx,
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
            shutting_down: false,
            confirm: None,
            rename: None,
            usage_limiter: Arc::new(Semaphore::new(cfg.network.max_concurrent)),
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
        }
    }

    fn track_account_task(
        &mut self,
        alias: String,
        kind: AccountTaskKind,
        lease_control: profile::ProfileLeaseAcquireControl,
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
                handle,
            },
        );
    }

    fn account_operation_in_flight(&self, alias: &str) -> bool {
        self.account_tasks.values().any(|task| task.alias == alias)
            || self.is_warmup_in_flight(alias)
            || self.refreshing_requests.contains_key(alias)
            || self.model_requests.contains_key(alias)
            || self.reset_cards_in_flight.contains(alias)
    }

    fn profile_switch_in_flight(&self) -> bool {
        self.account_tasks.values().any(|task| {
            matches!(
                task.kind,
                AccountTaskKind::SwitchPrepare
                    | AccountTaskKind::SwitchSync
                    | AccountTaskKind::SwitchCommit
            )
        })
    }

    fn interactive_operation_in_flight(&self) -> bool {
        self.confirm.is_some() || self.rename.is_some() || self.profile_switch_in_flight()
    }

    pub fn has_pending_credential_tasks(&self) -> bool {
        !self.account_tasks.is_empty() || !self.warmup_tasks.is_empty()
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
            let cancelled_before_lease = task.lease_control.is_cancelled();
            let joined = task.handle.await;
            if cancelled_before_lease {
                match kind {
                    AccountTaskKind::Usage { request_id } => {
                        let is_current = self
                            .refreshing_requests
                            .get(&alias)
                            .is_some_and(|(active_id, _)| *active_id == request_id);
                        if is_current {
                            self.refreshing_requests.remove(&alias);
                            self.pending_usage_refreshes.remove(&alias);
                        }
                    }
                    AccountTaskKind::Model { request_id } => {
                        let is_current = self
                            .model_requests
                            .get(&alias)
                            .is_some_and(|active_id| *active_id == request_id);
                        if is_current {
                            self.model_requests.remove(&alias);
                        }
                    }
                    AccountTaskKind::ResetCard => {
                        self.reset_cards_in_flight.remove(&alias);
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
                    let is_current = self
                        .refreshing_requests
                        .get(&alias)
                        .is_some_and(|(active_id, _)| *active_id == request_id);
                    if is_current {
                        self.refreshing_requests.remove(&alias);
                        self.pending_usage_refreshes.remove(&alias);
                        if let Some(entry) =
                            self.accounts.iter_mut().find(|entry| entry.alias == alias)
                        {
                            entry.usage = UsageStatus::Error(UsageError {
                                summary: "usage task stopped".to_string(),
                                detail: format!("usage task stopped ({alias}): {detail}"),
                            });
                        }
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
                    }
                }
                AccountTaskKind::ResetCard => {
                    self.reset_cards_in_flight.remove(&alias);
                    let mut unknown = format!(
                        "reset-card consumption may have occurred because its worker stopped ({detail}); verify before retry"
                    );
                    if let Err(cache_error) = cache::invalidate(&alias) {
                        unknown.push_str(&format!(
                            "; usage cache invalidation also failed: {cache_error:#}; do not retry until usage is refreshed and card ownership is verified"
                        ));
                    }
                    if let Some(entry) = self.accounts.iter_mut().find(|entry| entry.alias == alias)
                    {
                        entry.usage = UsageStatus::Error(UsageError {
                            summary: "reset-card outcome unknown".to_string(),
                            detail: unknown.clone(),
                        });
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
        // Only the pre-lease phase is safe to cancel: no credential-bearing
        // request can have left the process yet. Acquisition and shutdown race
        // through one atomic state transition, so tasks that already own their
        // lease remain tracked and are drained to their normal network timeout.
        for task in self.account_tasks.values() {
            task.lease_control.cancel_waiting();
        }
        for task in self.warmup_tasks.values() {
            task.lease_control.cancel_waiting();
        }
        while self.has_pending_credential_tasks() {
            self.poll_results();
            self.poll_reset_card_results();
            self.poll_model_results();
            self.poll_profile_switch_results();
            self.poll_warmup_results().await;
            self.poll_account_tasks().await;
            if self.has_pending_credential_tasks() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        // A sender completes just before its handle, so one final drain applies
        // messages that arrived between the last channel poll and handle join.
        self.poll_results();
        self.poll_reset_card_results();
        self.poll_model_results();
        self.poll_profile_switch_results();
    }

    /// Kick off a model-list fetch for `alias` if the detail panel needs it
    /// and it isn't already loaded or in flight. Idempotent — safe to call
    /// every frame.
    pub fn ensure_models_loaded(&mut self, alias: &str) {
        // Errors are stable session state too. Retrying every render tick can
        // hammer a permanently failing endpoint; `refresh_one` is the explicit
        // transition that invalidates any terminal state and starts a new request.
        if self.model_cache.contains_key(alias) {
            return;
        }
        let path = match profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => return,
        };
        self.model_cache
            .insert(alias.to_string(), ModelStatus::Loading);
        let request_id = self.model_next_id;
        self.model_next_id = self.model_next_id.wrapping_add(1);
        self.model_requests.insert(alias.to_string(), request_id);
        let alias_owned = alias.to_string();
        let tx = self.model_sender.clone();
        let limiter = self.usage_limiter.clone();
        let tracked_alias = alias_owned.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let permit = tokio::select! {
                permit = limiter.acquire() => permit.ok(),
                _ = task_lease_control.cancelled() => None,
            };
            let Some(_permit) = permit else { return };
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
                            request_id,
                            Err(format!(
                                "failed to lock profile for model discovery: {error:#}"
                            )),
                        ))
                        .await;
                    return;
                }
            };
            let result =
                crate::warmup::fetch_models_for_profile_leased(&alias_owned, &path, &lease)
                    .await
                    .map_err(|e| e.to_string());
            let _ = tx.send((alias_owned, request_id, result)).await;
        });
        self.track_account_task(
            tracked_alias,
            AccountTaskKind::Model { request_id },
            lease_control,
            handle,
        );
    }

    /// Fetch the model list for the currently-selected account, if the
    /// detail panel is visible. No-op when nothing is selected.
    pub fn ensure_models_loaded_for_selected(&mut self) {
        if !self.detail_visible {
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

    pub fn poll_model_results(&mut self) {
        let mut refresh_open_account = false;
        while let Ok((alias, request_id, result)) = self.pending_models.try_recv() {
            let is_current_request = self
                .model_requests
                .get(&alias)
                .is_some_and(|active_id| *active_id == request_id);
            if !is_current_request {
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
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
    }

    fn rebuild_open_account_menu(&mut self) {
        let scroll = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { popup, .. }) => popup.scroll,
            _ => return,
        };
        self.open_account_menu();
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
        let auth_expiries = profile_auth_path(&entry.alias)
            .ok()
            .and_then(|path| auth::read_auth(&path).ok())
            .map(|auth| {
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
                expiries
            })
            .unwrap_or_default();
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

    pub fn load_profiles(&mut self) {
        if let Err(error) = self.try_load_profiles() {
            self.set_status_error(format!("Profile reload failed: {error:#}"), 6);
        }
    }

    fn try_load_profiles(&mut self) -> Result<()> {
        let profiles = list_profiles().context("listing saved profiles")?;
        // The live auth file is the source of truth. If it belongs to an
        // untracked account, no saved profile is active; retaining the stale
        // marker here would highlight the account that used to be active.
        let current = sync_current_from_live().context("synchronizing the active profile")?;
        let mut loaded = Vec::with_capacity(profiles.len());
        for alias in profiles {
            let path = profile_auth_path(&alias)
                .with_context(|| format!("resolving profile path for '{alias}'"))?;
            let info = auth::read_account_info_checked(&path)
                .with_context(|| format!("loading profile '{alias}'"))?;
            loaded.push((alias, info));
        }

        // Do not take or mutate the displayed model until every path/read and
        // active-binding check above has succeeded.
        let mut retained_usage = retained_usage_by_alias(std::mem::take(&mut self.accounts));
        self.accounts = loaded
            .into_iter()
            .map(|(alias, info)| AccountEntry {
                usage: retained_usage.remove(&alias).unwrap_or(UsageStatus::Idle),
                is_current: current.as_deref() == Some(alias.as_str()),
                alias,
                info,
            })
            .collect();
        let known_aliases: BTreeSet<&str> = self
            .accounts
            .iter()
            .map(|account| account.alias.as_str())
            .collect();
        self.model_cache
            .retain(|alias, _| known_aliases.contains(alias.as_str()));
        self.model_requests
            .retain(|alias, _| known_aliases.contains(alias.as_str()));
        self.marked
            .retain(|alias| self.accounts.iter().any(|account| &account.alias == alias));
        // A reload can follow credential replacement for an existing alias.
        // Invalidate old generations so their late results cannot bind to the
        // newly loaded profile; the caller starts the replacement refresh.
        self.refreshing_requests.clear();
        self.pending_usage_refreshes.clear();
        self.selected = 0;
        self.view_indices.clear();
        self.update_view();
        if let Some(account_idx) = self.accounts.iter().position(|a| a.is_current)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
        Ok(())
    }

    pub fn load_profiles_preserving_selection(&mut self) {
        let selected_alias = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|entry| entry.alias.clone());

        self.load_profiles();

        if let Some(alias) = selected_alias
            && let Some(account_idx) = self.accounts.iter().position(|a| a.alias == alias)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
    }

    /// Credentials for one or more saved aliases were replaced. Clear both
    /// terminal cache entries and active generations: a late response made
    /// with the previous credentials must not bind to the replacement login.
    fn invalidate_models_after_credential_reload(&mut self) {
        self.model_cache.clear();
        self.model_requests.clear();
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
        if self.shutting_down {
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
            candidates.push(WarmupPreflightCandidate {
                alias,
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
        let handle = tokio::task::spawn_blocking(move || inspect_warmup_candidates(candidates));
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
        aliases: Vec<String>,
    ) {
        let started = aliases.len();
        let skipped = candidate_count.saturating_sub(started);
        for alias in aliases {
            self.spawn_warmup(alias);
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
            Ok(Ok(aliases)) => {
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
                let aliases = aliases
                    .into_iter()
                    .filter(|alias| {
                        self.accounts
                            .iter()
                            .find(|account| account.alias == *alias)
                            .is_some_and(|account| match &account.usage {
                                UsageStatus::Error(_) => false,
                                UsageStatus::Loaded(usage) => {
                                    !crate::usage::usage_has_active_warmup_window(usage, now)
                                }
                                UsageStatus::Idle | UsageStatus::Loading => true,
                            })
                            && !self.is_warmup_in_flight(alias)
                    })
                    .collect();
                self.report_warmup_preflight_success(origin, candidate_count, aliases);
            }
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
        let Some(idx) = self
            .accounts
            .iter()
            .position(|account| account.alias == alias)
        else {
            return;
        };
        self.model_cache.remove(alias);
        self.model_requests.remove(alias);
        self.fetch_usage_for(idx, Refresh::Forced);
        self.ensure_models_loaded(alias);
        self.set_status(format!("Refreshing {alias}"), 3);
    }

    fn spawn_warmup(&mut self, alias: String) {
        // Skip if this alias already has an in-flight warmup task.
        if self.is_warmup_in_flight(&alias) {
            return;
        }
        let task_id = self.warmup_next_id;
        self.warmup_next_id = self.warmup_next_id.wrapping_add(1);
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let limiter = self.usage_limiter.clone();
        let tracked_alias = alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let permit = tokio::select! {
                permit = limiter.acquire() => permit.ok(),
                _ = task_lease_control.cancelled() => None,
            };
            let Some(_permit) = permit else { return Ok(()) };
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
            crate::warmup::warmup_account_leased(&alias, &path, &lease)
                .await
                .map_err(|e| {
                    tracing::error!(alias = %alias, error = %format!("{e:#}"), "warmup failed");
                    format!("{e:#}")
                })
        });
        self.warmup_tasks.insert(
            task_id,
            WarmupTask {
                alias: tracked_alias,
                started: Instant::now(),
                slow_reported: false,
                lease_control,
                handle,
            },
        );
    }

    pub fn poll_update(&mut self) {
        if let Some(rx) = &mut self.update_rx {
            match rx.try_recv() {
                Ok(version) => {
                    self.update_available = Some(version);
                    self.update_rx = None;
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

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_rx = Some(rx);
        let is_dev = crate::update::current_version().contains("-dev");
        tokio::spawn(async move {
            let result = if is_dev {
                crate::update::check_for_dev_update().await
            } else {
                crate::update::check_for_update(false).await
            };
            if let Ok(Some(info)) = result {
                let _ = tx.send(info.latest_version);
            }
        });
    }

    pub async fn poll_warmup_results(&mut self) {
        let mut to_refresh = std::collections::BTreeSet::<String>::new();
        let mut successes = Vec::new();
        let mut failures = Vec::new();

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
            let cancelled_before_lease = task.lease_control.is_cancelled();
            let joined = task.handle.await;
            if cancelled_before_lease {
                continue;
            }
            match joined {
                Ok(Ok(())) => {
                    to_refresh.insert(alias.clone());
                    successes.push(alias);
                }
                Ok(Err(e)) => {
                    failures.push((alias.clone(), format!("Warmup failed ({alias}): {e}")));
                }
                Err(error) => {
                    let detail = crate::task_batch::join_failure_detail(&error);
                    failures.push((
                        alias.clone(),
                        format!("Warmup task stopped ({alias}): {detail}"),
                    ));
                }
            }
        }
        for alias in to_refresh {
            if self.shutting_down {
                continue;
            }
            if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
                // Always force a fresh fetch after warmup while keeping the previous
                // quota visible until the replacement arrives.
                self.fetch_usage_for(idx, Refresh::Forced);
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
            self.set_status(
                format!("Warmed up {} — refreshing usage...", successes.join(", ")),
                4,
            );
        }
    }

    pub fn poll_reset_card_results(&mut self) {
        let mut to_refresh = std::collections::BTreeSet::<String>::new();
        while let Ok((alias, result)) = self.pending_reset_cards.try_recv() {
            self.reset_cards_in_flight.remove(&alias);
            match result {
                Ok(consumed) => {
                    if let Err(err) = cache::invalidate(&alias) {
                        tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
                    }
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
                    to_refresh.insert(alias);
                }
                Err(e) => {
                    if e.invalidate_cache
                        && let Err(err) = cache::invalidate(&alias)
                    {
                        tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
                    }
                    self.set_status_error(e.message, 7);
                }
            }
        }
        for alias in to_refresh {
            if self.shutting_down {
                continue;
            }
            if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
                self.fetch_usage_for(idx, Refresh::Forced);
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

    fn fetch_usage_for(&mut self, idx: usize, refresh: Refresh) {
        let entry = match self.accounts.get(idx) {
            Some(e) => e,
            None => return,
        };
        if self.refreshing_requests.contains_key(&entry.alias) {
            if refresh_fetches_loaded_usage(refresh) {
                self.pending_usage_refreshes
                    .entry(entry.alias.clone())
                    .and_modify(|queued| {
                        if refresh_priority(refresh) > refresh_priority(*queued) {
                            *queued = refresh;
                        }
                    })
                    .or_insert(refresh);
            }
            return;
        }
        let needs_usage =
            refresh_fetches_loaded_usage(refresh) || !matches!(entry.usage, UsageStatus::Loaded(_));
        let force_negative_caches = refresh_forces_negative_caches(refresh);
        let alias = entry.alias.clone();
        let needs_workspace = if force_negative_caches {
            true
        } else if let Some(account_id) = entry.info.account_id.as_deref() {
            match crate::cache::workspace_name_is_known(account_id) {
                Ok(known) => !known,
                Err(error) => {
                    self.set_status_error(
                        format!(
                            "Could not inspect workspace metadata cache for {alias}: {error:#}"
                        ),
                        6,
                    );
                    return;
                }
            }
        } else {
            false
        };
        if !needs_usage && !needs_workspace {
            return;
        }

        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let limiter = self.usage_limiter.clone();

        if needs_usage && !matches!(self.accounts[idx].usage, UsageStatus::Loaded(_)) {
            self.accounts[idx].usage = UsageStatus::Loading;
        }

        let usage_tx = self.result_sender.clone();
        let workspace_tx = self.workspace_sender.clone();
        let request_id = needs_usage.then(|| {
            let request_id = self.usage_next_id;
            self.usage_next_id = self.usage_next_id.wrapping_add(1);
            self.refreshing_requests
                .insert(alias.clone(), (request_id, refresh));
            request_id
        });
        let tracked_alias = alias.clone();
        let tracked_request_id = request_id;
        let lease_control = needs_usage.then(profile::ProfileLeaseAcquireControl::new);
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let permit = match task_lease_control.as_ref() {
                Some(control) => tokio::select! {
                    permit = limiter.acquire() => permit.ok(),
                    _ = control.cancelled() => None,
                },
                None => limiter.acquire().await.ok(),
            };
            let Some(_permit) = permit else { return };
            if needs_usage {
                let control = task_lease_control
                    .as_ref()
                    .expect("usage work has lease control");
                let lease =
                    match profile::acquire_profile_lease_async_cancellable(alias.clone(), control)
                        .await
                    {
                        Ok(Some(lease)) => lease,
                        Ok(None) => return,
                        Err(error) => {
                            let result = Err(UsageError {
                                summary: "profile lock failed".to_string(),
                                detail: format!(
                                    "[{alias}] could not lock profile for usage refresh: {error:#}"
                                ),
                            });
                            let _ = usage_tx
                                .send((alias, request_id.expect("usage request id"), result))
                                .await;
                            return;
                        }
                    };
                let result = crate::usage::fetch_usage_retried_with_existing_lease(
                    &alias, &path, refresh, &lease,
                )
                .await;
                // Usage is independent of best-effort workspace metadata.
                let _ = usage_tx
                    .send((alias.clone(), request_id.expect("usage request id"), result))
                    .await;
                drop(lease);
            }
            if needs_workspace {
                // Read auth after usage because that path may have refreshed the token.
                if let Ok(auth) = crate::auth::read_auth(&path)
                    && let Err(err) =
                        crate::workspace::refresh_for_auth_if_needed(&auth, force_negative_caches)
                            .await
                {
                    tracing::debug!("[{alias}] workspace metadata unavailable: {err}");
                }
                let _ = workspace_tx.send(alias).await;
            }
        });
        if let Some(request_id) = tracked_request_id {
            self.track_account_task(
                tracked_alias,
                AccountTaskKind::Usage { request_id },
                lease_control.expect("tracked usage work has lease control"),
                handle,
            );
        } else {
            // Workspace-only lookups do not touch refresh credentials. They may
            // be cancelled on process exit without stranding account state.
            drop(handle);
        }
    }

    fn refresh_indices(&mut self, target_indices: &[usize], refresh: Refresh) {
        let cached = if matches!(refresh, Refresh::Cached) {
            let mut loaded = Vec::with_capacity(target_indices.len());
            for &i in target_indices {
                let Some(entry) = self.accounts.get(i) else {
                    continue;
                };
                match crate::cache::get(&entry.alias) {
                    Ok(value) => loaded.push((i, value)),
                    Err(error) => {
                        self.set_status_error(
                            format!("Could not read usage cache for {}: {error:#}", entry.alias),
                            6,
                        );
                        return;
                    }
                }
            }
            loaded
        } else {
            Vec::new()
        };

        for &i in target_indices {
            let entry = &mut self.accounts[i];
            if let UsageStatus::Error(_) = &entry.usage {
                entry.usage = UsageStatus::Idle;
            }
        }
        for (i, value) in cached {
            if let Some(cached) = value {
                self.accounts[i].usage = UsageStatus::Loaded(Box::new(cached));
            }
        }
        for &i in target_indices {
            self.fetch_usage_for(i, refresh);
        }
        self.update_view();
    }

    /// Refresh usage for all visible accounts (search-filtered view).
    /// Batch refresh of just the marked accounts is exposed separately
    /// via the Enter > Batch menu so the implicit "marks change scope"
    /// behavior is gone.
    pub fn refresh(&mut self, refresh: Refresh) {
        let target_indices: Vec<usize> = self.view_indices.clone();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn refresh_all(&mut self, refresh: Refresh) {
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn poll_results(&mut self) {
        let mut changed = false;
        let mut workspace_cache_errors = Vec::new();
        let open_account_alias = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { info, .. }) => Some(info.alias.clone()),
            _ => None,
        };
        let mut refresh_open_account = false;
        while let Ok((alias, request_id, result)) = self.pending_results.try_recv() {
            let is_current_request = self
                .refreshing_requests
                .get(&alias)
                .is_some_and(|(active_id, _)| *active_id == request_id);
            if !is_current_request {
                continue;
            }
            self.refreshing_requests.remove(&alias);
            let Some(idx) = self.accounts.iter().position(|entry| entry.alias == alias) else {
                continue;
            };
            self.accounts[idx].usage = match result {
                Ok(u) => UsageStatus::Loaded(Box::new(u)),
                Err(e) => UsageStatus::Error(e),
            };
            if let Err(error) = crate::cache::apply_workspace_name(&mut self.accounts[idx].info) {
                workspace_cache_errors.push((alias.clone(), error));
            }
            refresh_open_account |= open_account_alias.as_deref() == Some(alias.as_str());
            changed = true;
            if let Some(refresh) = self.pending_usage_refreshes.remove(&alias)
                && !self.shutting_down
            {
                self.fetch_usage_for(idx, refresh);
            }
        }
        while let Ok(alias) = self.pending_workspace.try_recv() {
            if let Some(entry) = self.accounts.iter_mut().find(|entry| entry.alias == alias) {
                match crate::cache::apply_workspace_name(&mut entry.info) {
                    Ok(()) => {
                        refresh_open_account |=
                            open_account_alias.as_deref() == Some(alias.as_str());
                        changed = true;
                    }
                    Err(error) => workspace_cache_errors.push((alias, error)),
                }
            }
        }
        if changed {
            self.update_view();
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
        if !workspace_cache_errors.is_empty() {
            workspace_cache_errors.sort_by(|(left, _), (right, _)| left.cmp(right));
            let detail = workspace_cache_errors
                .into_iter()
                .map(|(alias, error)| format!("[{alias}] {error:#}"))
                .collect::<Vec<_>>()
                .join("; ");
            self.set_status_error(format!("Could not apply workspace metadata: {detail}"), 6);
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
        if self.interactive_operation_in_flight() {
            self.set_status(
                "Finish the active confirmation or profile switch before switching again"
                    .to_string(),
                5,
            );
            return;
        }
        if self.account_operation_in_flight(&alias) {
            self.set_status(
                format!("{alias}: wait for the account operation to finish before switching"),
                5,
            );
            return;
        }
        self.start_profile_switch_prepare(alias, false);
    }

    fn start_profile_switch_prepare(&mut self, alias: String, live_sync_attempted: bool) {
        if self.shutting_down {
            return;
        }
        let tx = self.profile_switch_sender.clone();
        let tracked_alias = alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let result = match profile::acquire_profile_lease_async_cancellable(
                alias.clone(),
                &task_lease_control,
            )
            .await
            {
                Ok(Some(lease)) => {
                    match tokio::task::spawn_blocking(move || {
                        profile::prepare_profile_switch_with_lease(&lease)
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(anyhow::anyhow!(
                            "profile switch preparation worker stopped: {}",
                            crate::task_batch::join_failure_detail(&error)
                        )),
                    }
                }
                Ok(None) => return,
                Err(error) => Err(error.context(format!(
                    "acquiring profile lease before preparing switch to '{alias}'"
                ))),
            };
            let _ = tx
                .send((
                    alias,
                    ProfileSwitchTaskResult::Prepared {
                        result,
                        live_sync_attempted,
                    },
                ))
                .await;
        });
        self.track_account_task(
            tracked_alias.clone(),
            AccountTaskKind::SwitchPrepare,
            lease_control,
            handle,
        );
        self.set_status(format!("Preparing switch to {tracked_alias}..."), 60);
    }

    fn start_live_auth_sync_before_switch(&mut self, target_alias: String) {
        if self.shutting_down {
            return;
        }
        let tx = self.profile_switch_sender.clone();
        let tracked_alias = target_alias.clone();
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
        let handle = tokio::spawn(async move {
            let result = async {
                let active_alias = tokio::task::spawn_blocking(profile::active_profile_from_live)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "current-login identification worker stopped: {}",
                            crate::task_batch::join_failure_detail(&error)
                        )
                    })??
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "current Codex login is not saved; switch stopped without overwriting it"
                        )
                    })?;
                let lease = profile::acquire_profile_lease_async_cancellable(
                    active_alias.clone(),
                    &task_lease_control,
                )
                .await
                .with_context(|| {
                    format!("acquiring profile lease before synchronizing '{active_alias}'")
                })?
                .ok_or_else(|| anyhow::anyhow!("live-credential synchronization was cancelled"))?;
                tokio::task::spawn_blocking(move || {
                    profile::update_profile_from_live_leased(&lease)
                })
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "live-credential synchronization worker stopped: {}",
                        crate::task_batch::join_failure_detail(&error)
                    )
                })?
                .with_context(|| {
                    format!(
                        "saving refreshed live credentials to profile '{active_alias}' before switching"
                    )
                })
            }
            .await;
            let _ = tx
                .send((
                    target_alias.clone(),
                    ProfileSwitchTaskResult::LiveSynchronized(result),
                ))
                .await;
        });
        self.track_account_task(
            tracked_alias.clone(),
            AccountTaskKind::SwitchSync,
            lease_control,
            handle,
        );
        self.set_status(
            format!("Saving the current Codex login before switching to {tracked_alias}..."),
            60,
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
            let result = match profile::acquire_profile_lease_async_cancellable(
                alias.clone(),
                &task_lease_control,
            )
            .await
            {
                Ok(Some(lease)) => match tokio::task::spawn_blocking(move || {
                    profile::commit_confirmed_profile_switch_with_lease(confirmed, &lease)
                })
                .await
                {
                    Ok(result) => result,
                    Err(error) => Err(anyhow::anyhow!(
                        "profile switch commit worker stopped: {}",
                        crate::task_batch::join_failure_detail(&error)
                    )),
                },
                Ok(None) => return,
                Err(error) => Err(error.context(format!(
                    "acquiring profile lease before committing switch to '{alias}'"
                ))),
            };
            let _ = tx
                .send((alias, ProfileSwitchTaskResult::Committed(result)))
                .await;
        });
        self.track_account_task(
            tracked_alias.clone(),
            AccountTaskKind::SwitchCommit,
            lease_control,
            handle,
        );
        self.set_status(format!("Switching to {tracked_alias}..."), 60);
    }

    pub fn poll_profile_switch_results(&mut self) {
        while let Ok((alias, result)) = self.pending_profile_switches.try_recv() {
            match result {
                ProfileSwitchTaskResult::Prepared { .. } if self.shutting_down => {}
                ProfileSwitchTaskResult::LiveSynchronized(_) if self.shutting_down => {}
                ProfileSwitchTaskResult::Prepared {
                    result: Ok(prepared),
                    live_sync_attempted: false,
                } if prepared.requires_confirmation() => {
                    self.start_live_auth_sync_before_switch(alias);
                }
                ProfileSwitchTaskResult::Prepared {
                    result: Ok(prepared),
                    live_sync_attempted,
                } => match profile::confirm_prepared_profile_switch_without_overwrite(prepared) {
                    Ok(confirmed) => self.start_profile_switch_commit(confirmed),
                    Err(error) => {
                        let detail = if live_sync_attempted {
                            format!(
                                "live authentication changed again after its saved profile was synchronized: {error:#}"
                            )
                        } else {
                            format!("{error:#}")
                        };
                        self.set_status_error(format!("Switch failed: {detail}"), 5);
                    }
                },
                ProfileSwitchTaskResult::Prepared {
                    result: Err(error), ..
                } => {
                    self.set_status_error(format!("Switch failed: {error:#}"), 5);
                }
                ProfileSwitchTaskResult::LiveSynchronized(Ok(())) => {
                    self.start_profile_switch_prepare(alias, true)
                }
                ProfileSwitchTaskResult::LiveSynchronized(Err(error)) => {
                    self.set_status_error(format!("Switch failed: {error:#}"), 5)
                }
                ProfileSwitchTaskResult::Committed(result) => {
                    self.finish_profile_switch(alias, result);
                }
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
                match crate::profile::active_profile_from_live() {
                    Ok(Some(active)) if active == alias => {
                        self.set_status_error("Cannot delete the active profile".to_string(), 3);
                        return;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.set_status_error(
                            format!("Cannot verify the active profile: {error:#}"),
                            5,
                        );
                        return;
                    }
                }
                let delete_result = cmd_delete(&alias);
                self.reconcile_delete_result(&alias, delete_result);
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
                let mut report = BatchDeleteReport::default();
                let current = match crate::profile::active_profile_from_live() {
                    Ok(current) => current,
                    Err(error) => {
                        self.set_status_error(
                            format!("Cannot verify the active profile: {error:#}"),
                            5,
                        );
                        return;
                    }
                };
                for alias in &aliases {
                    if current.as_deref() == Some(alias.as_str()) {
                        report.failures.push(format!("{alias}: active, skipped"));
                        continue;
                    }
                    report.record(alias, cmd_delete(alias));
                }
                self.marked.clear();
                self.load_profiles();
                self.refresh(Refresh::Forced);
                let msg = report.message();
                if report.failures.is_empty() {
                    self.set_status(msg, 6);
                } else {
                    self.set_status_error(msg, 6);
                }
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
    ) {
        let reload_result = self.try_load_profiles();
        let visibly_deleted =
            reload_result.is_ok() && self.accounts.iter().all(|entry| entry.alias != alias);
        if reload_result.is_ok() {
            self.refresh(Refresh::Forced);
        }
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

    fn reconcile_rename_result(
        &mut self,
        old: &str,
        new: &str,
        rename_result: Result<profile::ProfileMutationOutcome>,
    ) {
        let was_marked = self.marked.contains(old);
        let reload_result = self.try_load_profiles();
        let visibly_renamed = reload_result.is_ok()
            && self.accounts.iter().all(|entry| entry.alias != old)
            && self.accounts.iter().any(|entry| entry.alias == new);
        if reload_result.is_ok() {
            if was_marked && visibly_renamed {
                self.marked.insert(new.to_string());
            }
            if let Some(account_idx) = self.accounts.iter().position(|entry| entry.alias == new)
                && let Some(view_idx) = self
                    .view_indices
                    .iter()
                    .position(|&index| index == account_idx)
            {
                self.selected = view_idx;
            }
            self.refresh(Refresh::Forced);
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
        if !self.reset_cards_in_flight.insert(alias.to_string()) {
            self.set_status(format!("{alias}: reset card use is already in progress"), 4);
            return;
        }
        let path = match profile_auth_path(alias) {
            Ok(p) => p,
            Err(e) => {
                self.reset_cards_in_flight.remove(alias);
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let alias_owned = alias.to_string();
        let tracked_alias = alias_owned.clone();
        let tx = self.reset_card_sender.clone();
        self.set_status(format!("Using reset card for {alias}..."), 6);
        let lease_control = profile::ProfileLeaseAcquireControl::new();
        let task_lease_control = lease_control.clone();
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
                    let _ = tx.send((alias_owned, Err(failure))).await;
                    return;
                }
            };
            let preflight = crate::usage::fetch_usage_retried_with_existing_lease(
                &alias_owned,
                &path,
                Refresh::Forced,
                &lease,
            )
            .await;
            let result = match preflight {
                Ok(preflight) => {
                    match crate::usage::validate_reset_credit_preflight(
                        &alias_owned,
                        &preflight,
                        &credit,
                    ) {
                        Ok(()) => crate::usage::consume_reset_credit_by_id_leased(
                            &alias_owned,
                            &path,
                            credit,
                            &lease,
                        )
                        .await
                        .map_err(|error| {
                            let unknown = error.outcome_unknown_after_request();
                            reset_card_failure_from_outcome(
                                unknown,
                                error.user_facing_unknown_message(&alias_owned),
                                format!("Reset card failed ({alias_owned}): {error}"),
                            )
                        }),
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
            let _ = tx.send((alias_owned, result)).await;
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
                let rename_result = rename_profile(&old, &new);
                self.reconcile_rename_result(&old, &new, rename_result);
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
        self.status_msg = Some(safe_text::bounded_terminal_text(
            &msg,
            STATUS_MESSAGE_MAX_CHARS,
        ));
        self.status_is_error = false;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
    }

    fn set_status_error(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(safe_text::bounded_terminal_text(
            &msg,
            STATUS_MESSAGE_MAX_CHARS,
        ));
        self.status_is_error = true;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
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

        if self.loading_count() > 0
            || !self.warmup_tasks.is_empty()
            || self.warmup_preflight.is_some()
        {
            self.next_auto_refresh = Some(now + Duration::from_secs(5));
            return;
        }

        self.load_profiles_preserving_selection();
        let account_count = self.accounts.len();
        if self.auto_warmup_enabled {
            self.warmup_all(account_count);
        }
        self.refresh_all(Refresh::Unattended);
        self.next_auto_refresh = Some(now + self.auto_refresh_interval);

        if !self.auto_warmup_enabled {
            self.set_status(
                format!("Auto refresh: refreshing {account_count} account(s)"),
                4,
            );
        }
    }

    pub fn tick(&mut self) {
        if let Some(expiry) = self.status_expiry
            && Instant::now() >= expiry
        {
            self.status_msg = None;
            self.status_expiry = None;
        }
    }
}

pub async fn run() -> Result<()> {
    // auth-change detection runs before dispatch(), so auto_track is already handled.

    // Ensure terminal is restored even on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal).await;
    ratatui::restore();
    result
}

/// Preserve an event-loop error while still reaching the same credential-safe
/// shutdown boundary as an explicit `q`. Rendering and terminal input can fail
/// after a refresh request has reached the server, so returning immediately
/// would otherwise drop its task with a rotated token only in memory.
async fn drain_credential_tasks_on_error<T>(app: &mut App, result: Result<T>) -> Result<T> {
    if result.is_err() {
        app.drain_credential_tasks().await;
    }
    result
}

async fn run_app(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::new();
    app.load_profiles();
    app.update_view();

    if !app.accounts.is_empty() {
        app.refresh(Refresh::Cached);
    }
    app.start_update_check();

    loop {
        app.poll_results();
        app.poll_warmup_preflight_result().await;
        app.poll_warmup_results().await;
        app.poll_reset_card_results();
        app.poll_model_results();
        app.poll_account_tasks().await;
        app.poll_profile_switch_results();
        app.poll_update();
        app.tick();
        app.run_due_auto_refresh();
        app.ensure_models_loaded_for_selected();

        let render_now =
            crate::auth::now_unix_secs().context("reading system clock for TUI render")?;
        let draw_result = terminal
            .draw(|f| super::ui::render(f, &mut app, render_now))
            .context("drawing TUI");
        drain_credential_tasks_on_error(&mut app, draw_result).await?;

        let event_ready = drain_credential_tasks_on_error(
            &mut app,
            event::poll(Duration::from_millis(100)).context("polling terminal events"),
        )
        .await?;
        if event_ready
            && let Event::Key(key) = drain_credential_tasks_on_error(
                &mut app,
                event::read().context("reading terminal event"),
            )
            .await?
        {
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
                    if app.has_pending_credential_tasks() {
                        app.set_status(
                            "Finishing active account operations before exit...".to_string(),
                            60,
                        );
                        // Failure to paint the exit notice must not skip the
                        // credential-safety boundary below.
                        let _ =
                            terminal.draw(|frame| super::ui::render(frame, &mut app, render_now));
                        app.drain_credential_tasks().await;
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
        MenuAction::Use(alias) => {
            app.close_menu();
            // Reuse switch_selected logic by selecting the alias first.
            if let Some(account_idx) = app.accounts.iter().position(|a| a.alias == alias)
                && let Some(view_idx) = app.view_indices.iter().position(|&i| i == account_idx)
            {
                app.selected = view_idx;
            }
            app.switch_selected();
        }
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

enum OAuthMode {
    Add,
    Relogin(String),
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

    // Replacement writes preserve the profile copy even when a later live-auth
    // update reports an error. Invalidate generations for every completed OAuth
    // attempt so a result fetched with pre-login credentials can never repopulate
    // the cache after such a partial commit.
    app.invalidate_models_after_credential_reload();
    match result {
        Ok(msg) => {
            app.set_status(msg, 5);
            app.load_profiles_preserving_selection();
            app.refresh(Refresh::Forced);
            // Reset auto-refresh timer so it doesn't fire immediately.
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Err(e) => {
            app.set_status_error(format!("OAuth failed: {e}"), 7);
        }
    }
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
    if failed.is_empty() && !stopped {
        app.set_status(summary, 8);
    } else {
        app.set_status_error(summary, 8);
    }
    // See `perform_oauth`: a failed round may still have preserved new profile
    // credentials before a live-auth update failed.
    app.invalidate_models_after_credential_reload();
    app.load_profiles_preserving_selection();
    app.refresh(Refresh::Forced);
    if app.auto_refresh_enabled {
        app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
    }
}

async fn run_oauth_inner(
    mode: OAuthMode,
    device: bool,
    lease_control: Option<&profile::ProfileLeaseAcquireControl>,
) -> Result<String> {
    match mode {
        OAuthMode::Add => {
            let tokens = if device {
                login::run_device_code_auth().await?
            } else {
                login::run_device_auth().await?
            };
            let (auth_val, info) = login::build_auth_from_tokens(&tokens)?;
            let action = profile::save_auth_value(auth_val.clone(), None)?;
            let alias = action.alias().to_string();
            let verb = action.action(); // "created" / "updated"
            let email_disp = info.email.as_deref().unwrap_or("unknown");
            println!(
                "[ok] Account {verb}: {} ({})",
                safe_text::terminal_text(&alias),
                safe_text::terminal_text(email_disp)
            );
            if let Err(err) = crate::workspace::refresh_for_auth(&auth_val).await {
                tracing::debug!("workspace metadata unavailable after TUI login save: {err}");
            }
            Ok(format!("Account {verb}: {alias}"))
        }
        OAuthMode::Relogin(alias) => {
            // The target alias is known before OAuth begins. Hold its lease
            // across authentication and commit so no usage, model, warmup, or
            // reset operation can race the credential being replaced.
            let lease = match lease_control {
                Some(control) => {
                    match profile::acquire_profile_lease_async_cancellable(alias.clone(), control)
                        .await?
                    {
                        Some(lease) => lease,
                        None => return Err(login::LoginCancelled.into()),
                    }
                }
                None => profile::acquire_profile_lease_async(alias.clone()).await?,
            };
            let tokens = if device {
                login::run_device_code_auth().await?
            } else {
                login::run_device_auth().await?
            };
            let (auth_val, info) = login::build_auth_from_tokens(&tokens)?;
            profile::replace_profile_auth_and_live_if_current_leased(&lease, &auth_val)?;
            drop(lease);
            let email_disp = info.email.as_deref().unwrap_or("unknown");
            println!(
                "[ok] Re-logged in: {} ({})",
                safe_text::terminal_text(&alias),
                safe_text::terminal_text(email_disp)
            );
            if let Err(err) = crate::workspace::refresh_for_auth(&auth_val).await {
                tracing::debug!("workspace metadata unavailable after TUI re-login save: {err}");
            }
            Ok(format!("Re-logged in: {alias}"))
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
        AccountEntry, AccountTaskKind, App, BatchDeleteReport, ConfirmAction, ModelStatus,
        STATUS_MESSAGE_MAX_CHARS, SearchState, SortMode, UsageStatus, WarmupTask,
        batch_relogin_not_attempted, drain_credential_tasks_on_error,
        finish_login_or_stop_after_round, refresh_fetches_loaded_usage,
        refresh_forces_negative_caches, reset_card_failure_from_outcome, retained_usage_by_alias,
    };
    use crate::{
        jwt::{AccountInfo, OrgInfo},
        login, profile,
        usage::{Refresh, ResetCredit, UsageInfo, WindowUsage},
        warmup::ModelEntry,
    };
    use crossterm::event::KeyCode;
    use std::time::Instant;

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
            info: AccountInfo::default(),
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
                info: AccountInfo::default(),
                usage: UsageStatus::Loaded(Box::default()),
                is_current: false,
            },
            AccountEntry {
                alias: "disk-waiting".into(),
                info: AccountInfo::default(),
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
        crate::cache::put(
            "account",
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
        assert!(crate::cache::get("account").unwrap().is_some());

        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
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

        assert!(crate::cache::get("account").unwrap().is_none());
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
        app.view_indices.extend([0, 1]);

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
            .try_send(("account".into(), 4, Ok(vec![ModelEntry::default()])))
            .unwrap();

        app.invalidate_models_after_credential_reload();
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
            info: AccountInfo::default(),
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
                started: Instant::now() - std::time::Duration::from_secs(61),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
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
        let handle = tokio::spawn(async move {
            panic!("warmup panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "account".into(),
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
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
        let success = tokio::spawn(async { Ok(()) });
        let failure = tokio::spawn(async { Err("injected warmup failure".to_string()) });
        app.warmup_tasks.insert(
            20,
            WarmupTask {
                alias: "success".into(),
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                handle: success,
            },
        );
        app.warmup_tasks.insert(
            10,
            WarmupTask {
                alias: "failure".into(),
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
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
        let slow = tokio::spawn(std::future::pending::<std::result::Result<(), String>>());
        let failure = tokio::spawn(async { Err("injected warmup failure".to_string()) });
        app.warmup_tasks.insert(
            2,
            WarmupTask {
                alias: "slow".into(),
                started: Instant::now() - std::time::Duration::from_secs(61),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                handle: slow,
            },
        );
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "failure".into(),
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
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
        let slow = tokio::spawn(std::future::pending::<std::result::Result<(), String>>());
        let success = tokio::spawn(async { Ok(()) });
        app.warmup_tasks.insert(
            2,
            WarmupTask {
                alias: "slow".into(),
                started: Instant::now() - std::time::Duration::from_secs(61),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
                handle: slow,
            },
        );
        app.warmup_tasks.insert(
            1,
            WarmupTask {
                alias: "success".into(),
                started: Instant::now(),
                slow_reported: false,
                lease_control: profile::ProfileLeaseAcquireControl::new(),
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

    #[test]
    fn model_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
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
        app.model_cache
            .insert("account".into(), ModelStatus::Loading);
        app.model_requests.insert("account".into(), 2);

        let model = |slug: &str| ModelEntry {
            slug: slug.into(),
            ..ModelEntry::default()
        };
        app.model_sender
            .try_send(("account".into(), 2, Ok(vec![model("new")])))
            .unwrap();
        app.model_sender
            .try_send(("account".into(), 1, Ok(vec![model("old")])))
            .unwrap();

        app.poll_model_results();

        let Some(ModelStatus::Loaded(models)) = app.model_cache.get("account") else {
            panic!("newest model result should remain loaded");
        };
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "new");
    }

    #[test]
    fn usage_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
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
            .try_send(("account".into(), 1, Ok(UsageInfo::default())))
            .unwrap();
        app.poll_results();
        assert_eq!(app.loading_count(), 0);

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should remain open");
        };
        assert!(info.usage.is_some());
    }

    #[test]
    fn stale_usage_result_is_ignored_after_a_new_request_generation_starts() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (2, Refresh::Forced));

        app.result_sender
            .try_send((
                "account".into(),
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
    fn forced_follow_up_is_queued_when_usage_request_is_already_in_flight() {
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

        app.fetch_usage_for(0, Refresh::Forced);

        assert_eq!(
            app.pending_usage_refreshes.get("account"),
            Some(&Refresh::Forced)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_refresh_keeps_last_loaded_usage_visible_while_request_is_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
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
    fn profile_reload_retains_loaded_usage_by_alias() {
        let retained = retained_usage_by_alias(vec![AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        }]);

        assert!(matches!(
            retained.get("account"),
            Some(UsageStatus::Loaded(_))
        ));
    }

    #[test]
    fn unattended_refresh_refetches_loaded_usage_without_forcing_negative_caches() {
        assert!(refresh_fetches_loaded_usage(Refresh::Unattended));
        assert!(!refresh_forces_negative_caches(Refresh::Unattended));
        assert!(refresh_forces_negative_caches(Refresh::Forced));
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
        app.shutting_down = true;
        app.accounts.push(AccountEntry {
            alias: "old".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.marked.insert("old".into());

        app.reconcile_rename_result(
            "old",
            "new",
            Ok(
                profile::ProfileMutationOutcome::test_committed_with_durability_warning(
                    anyhow::anyhow!("directory durability was not confirmed"),
                ),
            ),
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

        let mut app = App::new();
        app.shutting_down = true;
        app.accounts.push(AccountEntry {
            alias: "deleted".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);

        app.reconcile_delete_result(
            "deleted",
            Ok(
                profile::ProfileMutationOutcome::test_committed_with_durability_warning(
                    anyhow::anyhow!("directory durability was not confirmed"),
                ),
            ),
        );

        assert!(app.accounts.is_empty());
        assert!(!app.status_is_error);
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("durability could not be confirmed"))
        );
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

    #[test]
    fn reset_card_result_always_clears_in_flight_state() {
        let mut app = App::new();
        app.reset_cards_in_flight.insert("account".into());
        app.reset_card_sender
            .try_send((
                "account".into(),
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
