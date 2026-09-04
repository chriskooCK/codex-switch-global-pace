use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::jwt::PlanKind;

mod api;
mod global_pace;
mod parse;
mod reset_credits;
mod scoring;

#[allow(unused_imports)]
pub(crate) use api::{
    FirstNetworkPermit, NetworkPermitBudget, PreparedCoreUsageRequest, PreparedFullUsageRequest,
    UsageObservation, UsageTaskCancellation, apply_account_routing_headers, do_refresh_token,
    do_refresh_token_with_network,
    execute_prepared_core_usage_cancellable_with_existing_lease_and_client,
    execute_prepared_core_usage_with_existing_lease_and_client,
    execute_prepared_full_usage_with_existing_lease_and_client,
    fetch_usage_observation_force_with_existing_lease_and_client,
    fetch_usage_retried_with_existing_lease_and_client, first_network_permit,
    network_wait_cancelled_error, network_wait_was_cancelled, persist_refresh_resolution,
    prepare_core_usage_unattended_with_existing_lease, prepare_core_usage_with_existing_lease,
    prepare_full_usage_with_existing_lease,
    probe_core_usage_unattended_with_existing_lease_and_client,
};
pub use api::{
    fetch_usage_retried, fetch_usage_retried_force, fetch_usage_retried_unattended,
    refresh_expiring_tokens, refresh_expiring_tokens_with_client,
    refresh_expiring_tokens_within_with_client, validate_import_auth,
    validate_import_auth_with_client,
};
// Re-exported for the lib target's public API (used by integration tests via
// `codex_switch::usage::X`); the binary target doesn't call these through this
// path itself, so they'd otherwise look unused there.
#[allow(unused_imports)]
pub use api::fetch_usage_with_refresh;
#[allow(unused_imports)]
pub use api::refresh_expiring_tokens_within;
#[allow(unused_imports)]
pub use global_pace::{
    AccountWeeklyPace, GlobalPaceAccountInput, GlobalPaceWeighting, GlobalWeeklySummary,
    calculate_account_weekly_pace, calculate_effective_capacity, calculate_global_weekly_summary,
};
#[allow(unused_imports)]
pub use parse::parse_usage;
#[allow(unused_imports)]
pub(crate) use reset_credits::{
    ConsumeResetCreditError, PreparedResetCreditConsumeRequest, PreparedResetCreditEnrichment,
    consume_reset_credit_by_id_leased_with_client, enrich_reset_credits_for_auth_with_client,
    execute_prepared_reset_credit_consume_with_existing_lease_and_client,
    execute_prepared_reset_credit_enrichment_with_existing_lease_and_client,
    prepare_reset_credit_consume_with_existing_lease,
    prepare_reset_credit_enrichment_with_existing_lease, reset_credit_expiry_sort_key,
    reset_credit_expiry_timestamp, validate_reset_credit_preflight,
};
pub use reset_credits::{consume_reset_credit_by_id, earliest_reset_credit};
#[allow(unused_imports)]
pub use scoring::visible_pace_percent_at;
pub(crate) use scoring::{
    QuotaPaceState, UsageAvailability, normalized_plan_kind, normalized_quota_usage,
    quota_pace_state, usage_availability, visible_pace_marker,
};
pub use scoring::{
    is_available, is_candidate_eligible, pace_percent_at, pick_switch_target, score_candidates,
    usage_has_active_warmup_window,
};
#[allow(unused_imports)]
pub use scoring::{score_unified, warmup_window_active};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WindowUsage {
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub window_minutes: Option<i64>,
}

/// Resolve a quota window's duration without inventing metadata. The slot
/// default is authoritative only when the API omitted the duration entirely;
/// explicitly invalid metadata makes the window unusable for time-based
/// decisions.
pub(crate) fn quota_window_duration_secs(window: &WindowUsage, default_secs: i64) -> Option<i64> {
    match window.window_minutes {
        Some(minutes) => minutes.checked_mul(60).filter(|seconds| *seconds > 0),
        None => (default_secs > 0).then_some(default_secs),
    }
}

/// Validate the quota fields automatic selection needs from one window.
/// Display-only clamping is deliberately not used here: an out-of-range value
/// cannot prove that a required window is selectable.
pub(crate) fn validated_quota_window(
    window: &WindowUsage,
    default_secs: i64,
) -> Option<(f64, i64)> {
    let used_percent = window
        .used_percent
        .filter(|used| used.is_finite() && (0.0..=100.0).contains(used))?;
    let duration_secs = quota_window_duration_secs(window, default_secs)?;
    Some((used_percent, duration_secs))
}

/// Resolve the label and duration from window metadata, using the caller's
/// explicit slot definition only when the API omitted that metadata.
pub(crate) fn quota_window_spec(
    window: &WindowUsage,
    default_label: &str,
    default_secs: i64,
) -> (String, Option<i64>) {
    let duration_secs = quota_window_duration_secs(window, default_secs);
    match window.window_minutes {
        Some(minutes) if minutes > 0 && minutes % 1_440 == 0 => {
            (format!("{}d", minutes / 1_440), duration_secs)
        }
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => {
            (format!("{}h", minutes / 60), duration_secs)
        }
        Some(minutes) => (format!("{minutes}m"), duration_secs),
        None => (default_label.to_string(), duration_secs),
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SpendControlLimit {
    pub source: Option<String>,
    pub limit: Option<String>,
    pub used: Option<String>,
    pub remaining: Option<String>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

/// One entry from the `additional_rate_limits` array in the usage API response.
/// Represents a metered feature (e.g. `codex_other`) with its own independent windows.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AdditionalRateLimit {
    pub limit_name: Option<String>,
    pub metered_feature: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary: Option<WindowUsage>,
    pub secondary: Option<WindowUsage>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    pub granted_at: Option<String>,
    pub expires_at: Option<String>,
}

/// A schema problem retained by the public issue-preserving parser. Network
/// fetches use the checked parser and reject these responses outright;
/// retaining the typed issue keeps direct library callers from receiving a
/// silent fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum UsageParseIssue {
    InvalidPrimaryWindow { detail: String },
    InvalidSecondaryWindow { detail: String },
    InvalidRateLimitReachedType { raw: String, detail: String },
    InvalidAdditionalRateLimits { detail: String },
    InvalidCodeReviewRateLimit { detail: String },
    InvalidPlanType { detail: String },
    InvalidRateLimit { detail: String },
    InvalidCredits { detail: String },
    InvalidSpendControl { detail: String },
}

impl std::fmt::Display for UsageParseIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPrimaryWindow { detail } => {
                write!(formatter, "primary_window: {detail}")
            }
            Self::InvalidSecondaryWindow { detail } => {
                write!(formatter, "secondary_window: {detail}")
            }
            Self::InvalidRateLimitReachedType { detail, .. } => {
                write!(formatter, "rate_limit_reached_type: {detail}")
            }
            Self::InvalidAdditionalRateLimits { detail } => {
                write!(formatter, "additional_rate_limits: {detail}")
            }
            Self::InvalidCodeReviewRateLimit { detail } => {
                write!(formatter, "code_review_rate_limit: {detail}")
            }
            Self::InvalidPlanType { detail } => write!(formatter, "plan_type: {detail}"),
            Self::InvalidRateLimit { detail } => write!(formatter, "rate_limit: {detail}"),
            Self::InvalidCredits { detail } => write!(formatter, "credits: {detail}"),
            Self::InvalidSpendControl { detail } => {
                write!(formatter, "spend_control: {detail}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConsumedResetCredit {
    pub credit: ResetCredit,
    pub code: Option<String>,
    pub windows_reset: Option<u64>,
    pub redeemed_at: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UsageInfo {
    /// Opaque revision of the exact disk-cache entry that supplied or stored
    /// this value. Deferred metadata uses it to avoid updating a newer quota
    /// snapshot.
    pub(crate) cache_revision: Option<String>,
    pub fetched_at: Option<i64>,
    pub primary: Option<WindowUsage>,   // 5h window
    pub secondary: Option<WindowUsage>, // 7d window
    pub credits_balance: Option<f64>,
    pub unlimited_credits: Option<bool>,
    /// plan_type from usage API response (authoritative; overrides JWT claims when present)
    pub plan_type: Option<String>,
    pub reset_credits_available_count: Option<u64>,
    pub reset_credits: Vec<ResetCredit>,
    pub reset_credits_error: Option<String>,
    /// Explicit account/workspace-level restriction reported by the API.
    pub account_limited: bool,
    /// Whether spend control itself is explicitly reached. This is distinct
    /// from ordinary weekly quota exhaustion, although both make
    /// `account_limited` true for existing availability/scoring behavior.
    pub spend_control_reached: bool,
    /// Backend-classified limit reason, preserved for detailed diagnostics.
    pub rate_limit_reached_type: Option<String>,
    /// Effective workspace/user spend-control limit, when supplied by the backend.
    pub individual_limit: Option<Box<SpendControlLimit>>,
    /// Per-feature rate limits from `additional_rate_limits[]` (e.g. codex_other).
    pub additional_limits: Vec<AdditionalRateLimit>,
    /// Explicit schema problems retained by [`parse_usage`]. Checked network
    /// parsing rejects the same issues before a `UsageInfo` is returned.
    pub parse_issues: Vec<UsageParseIssue>,
}

/// A non-window account restriction that a Codex rate-limit reset card cannot
/// repair. Keep this distinct from the broad `account_limited` compatibility
/// flag: the usage API also sets that flag for an ordinary exhausted window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplicitAccountBlocker {
    SpendControlReached,
    WorkspaceOwnerCreditsDepleted,
    WorkspaceMemberCreditsDepleted,
    WorkspaceOwnerUsageLimitReached,
    WorkspaceMemberUsageLimitReached,
    UnrecognizedRateLimitReason(String),
    MalformedUsageResponse(String),
}

impl ExplicitAccountBlocker {
    pub fn wire_reason(&self) -> &str {
        match self {
            Self::SpendControlReached => "spend_control_reached",
            Self::WorkspaceOwnerCreditsDepleted => "workspace_owner_credits_depleted",
            Self::WorkspaceMemberCreditsDepleted => "workspace_member_credits_depleted",
            Self::WorkspaceOwnerUsageLimitReached => "workspace_owner_usage_limit_reached",
            Self::WorkspaceMemberUsageLimitReached => "workspace_member_usage_limit_reached",
            Self::UnrecognizedRateLimitReason(reason) => reason,
            Self::MalformedUsageResponse(issue) => issue,
        }
    }
}

impl std::fmt::Display for ExplicitAccountBlocker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.wire_reason())
    }
}

/// Classify only account/workspace restrictions that remain in force even if
/// the ordinary quota windows are reset.
pub fn explicit_account_blocker(usage: &UsageInfo) -> Option<ExplicitAccountBlocker> {
    if usage.spend_control_reached {
        return Some(ExplicitAccountBlocker::SpendControlReached);
    }
    let reason_blocker = match usage.rate_limit_reached_type.as_deref() {
        Some("workspace_owner_credits_depleted") => {
            Some(ExplicitAccountBlocker::WorkspaceOwnerCreditsDepleted)
        }
        Some("workspace_member_credits_depleted") => {
            Some(ExplicitAccountBlocker::WorkspaceMemberCreditsDepleted)
        }
        Some("workspace_owner_usage_limit_reached") => {
            Some(ExplicitAccountBlocker::WorkspaceOwnerUsageLimitReached)
        }
        Some("workspace_member_usage_limit_reached") => {
            Some(ExplicitAccountBlocker::WorkspaceMemberUsageLimitReached)
        }
        Some("rate_limit_reached") | None => None,
        Some(reason) => Some(ExplicitAccountBlocker::UnrecognizedRateLimitReason(
            reason.to_string(),
        )),
    };
    if reason_blocker.is_some() {
        return reason_blocker;
    }
    usage
        .parse_issues
        .first()
        .map(|issue| ExplicitAccountBlocker::MalformedUsageResponse(issue.to_string()))
}

/// A broad `limit_reached` verdict that is not one of the explicit
/// account/workspace restrictions above. The API normally pairs this with an
/// exhausted quota window, but partial or slightly inconsistent snapshots must
/// still fail closed until the reported windows have reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinaryAccountLimit {
    UntilReset(i64),
    ResetUnknown,
}

impl OrdinaryAccountLimit {
    pub const fn is_active(self, now: i64) -> bool {
        match self {
            Self::UntilReset(resets_at) => resets_at > now,
            Self::ResetUnknown => true,
        }
    }
}

/// Classify the broad account-limited verdict separately from explicit
/// blockers. A quota reset can repair this state, so its reset timestamp is
/// retained instead of erasing the underlying usage windows.
pub fn ordinary_account_limit(usage: &UsageInfo) -> Option<OrdinaryAccountLimit> {
    if !usage.account_limited || explicit_account_blocker(usage).is_some() {
        return None;
    }

    let windows = [usage.primary.as_ref(), usage.secondary.as_ref()];
    let present = windows.into_iter().flatten().collect::<Vec<_>>();
    if present.is_empty() {
        return Some(OrdinaryAccountLimit::ResetUnknown);
    }

    let exhausted = present
        .iter()
        .copied()
        .filter(|window| window.used_percent.is_some_and(|used| used >= 100.0))
        .collect::<Vec<_>>();
    let relevant = if exhausted.is_empty() {
        present
    } else {
        exhausted
    };
    if relevant.iter().any(|window| window.resets_at.is_none()) {
        return Some(OrdinaryAccountLimit::ResetUnknown);
    }

    // When the broad verdict and percentages disagree, the response does not
    // identify which window caused it. Waiting for the latest reported reset
    // is conservative and a normal refresh will clear the stale verdict sooner.
    let resets_at = relevant
        .into_iter()
        .filter_map(|window| window.resets_at)
        .max();
    let Some(resets_at) = resets_at else {
        return Some(OrdinaryAccountLimit::ResetUnknown);
    };
    Some(OrdinaryAccountLimit::UntilReset(resets_at))
}

/// One assembled display row for an additional-limit pool. Pure data,
/// derived from `AdditionalRateLimit` so CLI and TUI renderers can share
/// the same assembly logic instead of each re-deriving `unavailable`.
#[derive(Debug, Clone)]
pub struct PoolRow {
    pub limit_name: String,
    /// True when the API reports the pool as exhausted or disallowed.
    pub unavailable: bool,
    pub primary: Option<WindowUsage>,
    pub secondary: Option<WindowUsage>,
}

/// Assemble display rows from the raw `additional_limits` array. Returns an
/// empty vec when there are no additional pools (the common case today).
pub fn additional_pool_rows(limits: &[AdditionalRateLimit]) -> Vec<PoolRow> {
    limits
        .iter()
        .map(|l| PoolRow {
            limit_name: l.limit_name.clone().unwrap_or_else(|| "pool".to_string()),
            unavailable: l.limit_reached == Some(true) || l.allowed == Some(false),
            primary: l.primary.clone(),
            secondary: l.secondary.clone(),
        })
        .collect()
}

/// One validated quota window used by automatic selection.
#[derive(Debug, Clone)]
pub struct CandidateWindow {
    pub(crate) used_percent: f64,
    pub(crate) resets_at: Option<i64>,
    pub(crate) duration_secs: i64,
}

impl CandidateWindow {
    fn from_usage(window: &WindowUsage, default_secs: i64, now: i64) -> Option<Self> {
        let (used_percent, duration_secs) = validated_quota_window(window, default_secs)?;
        if let Some(resets_at) = window.resets_at
            && resets_at > now
            && resets_at.checked_sub(now)? > duration_secs
        {
            return None;
        }
        Some(Self {
            used_percent,
            resets_at: window.resets_at,
            duration_secs,
        })
    }

    pub fn effective_used(&self, now: i64) -> f64 {
        if self.resets_at.is_some_and(|timestamp| timestamp <= now) {
            0.0
        } else {
            self.used_percent
        }
    }
}

/// All normalized data needed to score an account. Pure data, no I/O.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub alias: String,
    pub primary: Option<CandidateWindow>,
    pub weekly: Option<CandidateWindow>,
    pub explicit_account_blocker: Option<ExplicitAccountBlocker>,
    pub ordinary_account_limit: Option<OrdinaryAccountLimit>,
    pub plan_kind: PlanKind,
    pub last_used: i64,
    pub now: i64,
    // Pool-level signals (set by caller after building all candidates)
    pub pool_size: usize,
    pub pool_exhausted: usize,
    pub team_priority: bool,
}

impl Candidate {
    /// Build from UsageInfo + metadata. `now` should be shared across all candidates.
    pub fn from_usage(
        alias: String,
        u: &UsageInfo,
        plan_kind: PlanKind,
        last_used: i64,
        now: i64,
    ) -> Self {
        Self {
            alias,
            primary: u
                .primary
                .as_ref()
                .and_then(|window| CandidateWindow::from_usage(window, WINDOW_5H_SECS, now)),
            weekly: u
                .secondary
                .as_ref()
                .and_then(|window| CandidateWindow::from_usage(window, WINDOW_7D_SECS, now)),
            explicit_account_blocker: explicit_account_blocker(u),
            ordinary_account_limit: ordinary_account_limit(u),
            plan_kind,
            last_used,
            now,
            pool_size: 1,
            pool_exhausted: 0,
            team_priority: false,
        }
    }

    /// Whether this candidate has the validated weekly window required for
    /// quota-aware selection. A primary window is optional scoring evidence.
    pub fn has_required_quota_data(&self) -> bool {
        main_weekly_quota_available(self.weekly.as_ref())
    }

    pub fn is_team(&self) -> bool {
        matches!(self.plan_kind, PlanKind::Team)
    }

    pub fn is_free(&self) -> bool {
        matches!(self.plan_kind, PlanKind::Free)
    }

    /// Reset-aware effective primary usage: 0.0 if the window has reset.
    pub fn effective_used_5h(&self) -> Option<f64> {
        self.primary
            .as_ref()
            .map(|window| window.effective_used(self.now))
    }

    /// Reset-aware effective weekly usage: 0.0 if the window has reset.
    pub fn effective_used_7d(&self) -> Option<f64> {
        self.weekly
            .as_ref()
            .map(|window| window.effective_used(self.now))
    }

    pub fn account_limit_active(&self) -> bool {
        self.explicit_account_blocker.is_some()
            || self
                .ordinary_account_limit
                .is_some_and(|limit| limit.is_active(self.now))
    }
}

/// A validated main weekly window is the quota-data contract shared by
/// automatic selection, status, and global weekly pace. The API may omit the
/// short window for any plan, so plan names and primary-window presence are not
/// evidence that an otherwise valid weekly quota is unavailable.
pub(crate) const fn main_weekly_quota_available<T>(weekly: Option<&T>) -> bool {
    weekly.is_some()
}

/// Window durations in seconds (used for pace calculation).
pub const WINDOW_5H_SECS: i64 = 5 * 3600;
pub const WINDOW_7D_SECS: i64 = 7 * 86400;

/// Free plan accounts become ineligible below this 5h remaining%.
pub const FREE_FLOOR_PCT: f64 = 35.0;

/// Minimum elapsed time before a quota window proves that warmup truly stuck.
pub const MIN_WARMUP_ELAPSED_SECS: i64 = 5 * 60;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(1);
pub(super) const ACCESS_TOKEN_REFRESH_MARGIN_SECS: i64 = 60;

/// How much of the cache a usage fetch may skip.
///
/// One boolean used to cover two unrelated requests: wanting numbers that are
/// not stale, and wanting a verdict the auth server has already given to be
/// asked again. Only a person can mean the second — an unattended timer that
/// re-presents a spent credential every polling interval learns nothing and
/// pays for the rejection every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Serve a fresh cache entry as-is. Everyday reads.
    Cached,
    /// Ignore the usage TTL, but honour a recorded auth verdict. What a timer
    /// with nobody watching wants.
    Unattended,
    /// Ignore both. Reserved for a person explicitly asking again, and the only
    /// way back from a verdict recorded in error.
    Forced,
}

impl Refresh {
    pub(super) fn skips_usage_cache(self) -> bool {
        !matches!(self, Refresh::Cached)
    }

    pub(super) fn may_re_present_a_rejected_credential(self) -> bool {
        matches!(self, Refresh::Forced)
    }
}

#[derive(Debug, Clone)]
pub struct RefreshedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// A refresh the auth server rejected outright (bad/consumed credential).
///
/// OpenAI rotates `refresh_token` on every use and answers replays with
/// `refresh_token_reused`, so retrying such a failure can never succeed — it
/// only burns round trips. Carried as a typed error so retry loops can
/// recognise it via `anyhow::Error::downcast_ref`.
#[derive(Debug, Clone)]
pub struct TerminalAuthError {
    /// Server-provided error code (or `http_<status>` when the body had none).
    pub code: String,
    /// Server-provided human-readable message, when present.
    pub message: Option<String>,
}

impl TerminalAuthError {
    /// Short, actionable line for list/TUI status columns.
    pub fn summary(&self) -> String {
        format!("re-login required ({})", self.code)
    }
}

impl std::fmt::Display for TerminalAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "token refresh rejected, sign in again — {}", self.code)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TerminalAuthError {}

/// A refresh request whose server-side token-rotation result cannot be proven.
/// Replaying the presented single-use token could consume or reject it again,
/// so retry loops must stop immediately.
#[derive(Debug)]
pub(crate) struct RefreshOutcomeUnknown {
    cause: anyhow::Error,
}

impl RefreshOutcomeUnknown {
    pub(crate) fn new(cause: anyhow::Error) -> Self {
        Self { cause }
    }
}

impl std::fmt::Display for RefreshOutcomeUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "token refresh outcome is unknown after the request may have reached the server; do not retry this credential before inspecting the profile",
        )
    }
}

impl std::error::Error for RefreshOutcomeUnknown {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

/// Outcome of validating an auth.json on the `import` path.
///
/// Validation invokes its required persistence callback before making any
/// follow-up usage request. `refreshed` is still populated **even when
/// `result` is an error**, allowing the import command to distinguish a spent
/// source credential and report where its durable recovery copy was preserved.
pub struct ImportValidation {
    pub refreshed: Option<RefreshedTokens>,
    /// Account id that the Usage API accepted for these credentials.
    pub validated_account_id: Option<String>,
    pub result: anyhow::Result<UsageInfo>,
}

/// Structured error for usage fetch failures.
#[derive(Debug, Clone)]
pub struct UsageError {
    /// Short summary for user-facing display (e.g. "HTTP 401 Unauthorized")
    pub summary: String,
    /// Full detail for debug/log (e.g. "Usage API failed (HTTP 401), token refresh also failed: ...")
    pub detail: String,
}

impl UsageError {
    /// A refresh was deliberately not started because the exact live-auth
    /// state needed for a safe post-response compare-and-swap could not be
    /// captured first. No server-side token rotation has happened yet.
    pub fn refresh_authorization_failed(alias: &str, cause: &anyhow::Error) -> Self {
        Self {
            summary: "token refresh not started".to_string(),
            detail: format!(
                "[{alias}] token refresh was not started because the exact live-auth state could \
                 not be authorized for a safe conditional update: {cause:#}. No credential was \
                 sent to the refresh endpoint."
            ),
        }
    }

    /// The auth server issued rotated credentials but they could not be written
    /// to disk.
    ///
    /// This is *not* a rejected refresh: the new tokens are valid, they simply
    /// never reached the profile, while the previous `refresh_token` is already
    /// dead server-side. Continuing would leave the user with an account that
    /// silently stops working at the next start, so the wording has to point at
    /// the local write failure and carry the underlying IO/permission cause.
    pub fn token_persist_failed(alias: &str, cause: &anyhow::Error) -> Self {
        Self {
            summary: "refreshed token not saved".to_string(),
            detail: format!(
                "[{alias}] token refresh succeeded but the rotated credentials could not be saved: \
                 {cause:#}. The auth server has already invalidated the previous refresh token, so \
                 this profile may need to sign in again once the write problem is fixed."
            ),
        }
    }

    /// The refreshed profile bytes are visible, but their full local commit
    /// (directory durability and, when active, live-auth synchronization) did
    /// not complete. Retrying the refresh would spend another single-use token.
    pub fn credential_commit_incomplete(
        alias: &str,
        recovery_path: Option<&std::path::Path>,
        cause: &anyhow::Error,
    ) -> Self {
        let recovery = recovery_path.map_or_else(String::new, |path| {
            format!(
                " The exact rotated credentials remain privately preserved at {}.",
                path.display()
            )
        });
        Self {
            summary: "credential commit incomplete".to_string(),
            detail: format!(
                "[{alias}] refreshed credentials are visible in the profile, but their durable \
                 commit or live Codex auth synchronization could not be confirmed safely: \
                 {cause:#}.{recovery} The refresh was stopped without spending another token. Fix the \
                 reported local path problem, then inspect the profile before retrying."
            ),
        }
    }

    /// The rotated credential commit is complete; only deletion of its
    /// redundant write-ahead recovery stage could not be proven exact.
    pub fn recovery_cleanup_incomplete(
        alias: &str,
        recovery_path: Option<&std::path::Path>,
        cause: &anyhow::Error,
    ) -> Self {
        let recovery = recovery_path.map_or_else(
            || {
                " The original recovery file could not be rebound to its exact file identity, so no path is claimed for it."
                    .to_string()
            },
            |path| {
                format!(
                    " The exact redundant recovery file is still present at {}.",
                    path.display()
                )
            },
        );
        Self {
            summary: "recovery cleanup incomplete".to_string(),
            detail: format!(
                "[{alias}] refreshed credentials were durably committed to the profile and any required live Codex auth update completed, but exact cleanup of the write-ahead recovery stage failed: {cause:#}.{recovery} No additional token was spent. Resolve the reported local path problem before deleting any recovery file manually."
            ),
        }
    }

    /// A rotation completed server-side, but installing its response would
    /// have rebound the profile to a different account or violated managed
    /// account policy. The response is retained as a private recovery file
    /// rather than overwriting either saved or live credentials.
    pub fn refreshed_credentials_quarantined(
        alias: &str,
        path: &std::path::Path,
        cause: &anyhow::Error,
    ) -> Self {
        Self {
            summary: "refreshed credentials quarantined".to_string(),
            detail: format!(
                "[{alias}] token refresh succeeded, but its credentials were not installed: \
                 {cause:#}. The exact rotated credentials were preserved privately at {}. \
                 The existing profile and live Codex auth were left unchanged; inspect the \
                 account mismatch before recovering or retrying.",
                path.display()
            ),
        }
    }

    /// A validated rotation is durable in the private recovery area, but a
    /// local operational failure prevented any profile replacement.
    pub fn rotated_credentials_recovery_preserved(
        alias: &str,
        path: &std::path::Path,
        cause: &anyhow::Error,
    ) -> Self {
        Self {
            summary: "rotated credentials preserved for recovery".to_string(),
            detail: format!(
                "[{alias}] token refresh succeeded and the exact rotated credentials were preserved privately at {}, but the profile commit was stopped before replacement: {cause:#}. The existing profile and live Codex auth were left unchanged; fix the reported local problem and inspect the named recovery file before retrying.",
                path.display()
            ),
        }
    }

    /// The endpoint returned a non-empty successor refresh token together with
    /// an invalid token set, and even the private recovery copy could not be
    /// committed durably. The old profile remains visible but its presented
    /// refresh token may already be dead server-side.
    pub fn invalid_refresh_recovery_failed(alias: &str, cause: &anyhow::Error) -> Self {
        Self {
            summary: "rotated credential recovery failed".to_string(),
            detail: format!(
                "[{alias}] token refresh issued a non-empty successor refresh token, but the response was invalid and its private recovery copy could not be committed: {cause:#}. The existing profile and live Codex auth were left unchanged, but the previous refresh token may already be invalid; do not retry it and sign in again if the recovery artifact cannot be located."
            ),
        }
    }

    pub fn refresh_outcome_unknown(alias: &str, cause: &anyhow::Error) -> Self {
        Self {
            summary: "token refresh outcome unknown".to_string(),
            detail: format!(
                "[{alias}] {cause:#}. The refresh request may have reached the server, so whether its single-use token was consumed or rotated cannot be confirmed. The existing profile and live Codex auth were left unchanged; do not retry this credential before inspecting it, and sign in again if necessary."
            ),
        }
    }

    /// A different writer replaced the profile credential while this refresh
    /// was in flight, so the compare-and-swap deliberately preserved it.
    pub fn token_update_superseded(alias: &str, recovery_path: &std::path::Path) -> Self {
        Self {
            summary: "refreshed token superseded".to_string(),
            detail: format!(
                "[{alias}] the saved credential changed while token refresh was in progress. \
                 The refreshed response was not installed because overwriting the newer \
                 credential would be unsafe. The exact rotated response was preserved privately \
                 at {}; this request was stopped.",
                recovery_path.display()
            ),
        }
    }
}

/// One profile whose rotated credentials could not be committed completely
/// during an opportunistic refresh.
///
/// Opportunistic refresh is a batch, and the daemon runs it on a timer, so a
/// single failure must neither abort the remaining profiles nor disappear into
/// a log line: it is collected and handed back for the caller to surface.
#[derive(Debug, Clone)]
pub struct TokenPersistFailure {
    pub alias: String,
    pub error: UsageError,
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

#[cfg(test)]
mod usage_error_tests {
    use super::UsageError;

    #[test]
    fn cleanup_only_failure_reports_committed_credentials_and_only_an_exact_path() {
        let path = std::path::Path::new("recovery/owned.json");
        let error = UsageError::recovery_cleanup_incomplete(
            "alice",
            Some(path),
            &anyhow::anyhow!("exact removal refused"),
        );
        assert_eq!(error.summary, "recovery cleanup incomplete");
        assert!(error.detail.contains("durably committed"));
        assert!(error.detail.contains("recovery/owned.json"));
        assert!(
            !error
                .detail
                .contains("commit or live Codex auth synchronization could not")
        );

        let unbound = UsageError::recovery_cleanup_incomplete(
            "alice",
            None,
            &anyhow::anyhow!("path identity changed"),
        );
        assert!(unbound.detail.contains("no path is claimed"));
        assert!(!unbound.detail.contains("recovery/owned.json"));
    }
}

/// One scored candidate. Pure data, no I/O.
pub struct ScoredCandidate {
    pub candidate: Candidate,
    pub usage: UsageInfo,
    pub score: f64,
}

#[cfg(test)]
mod pool_row_tests {
    use super::*;

    #[test]
    fn empty_additional_limits_yields_no_rows() {
        assert!(additional_pool_rows(&[]).is_empty());
    }

    #[test]
    fn pool_with_both_windows_produces_one_row() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_other".to_string()),
            allowed: Some(true),
            limit_reached: Some(false),
            primary: Some(WindowUsage {
                used_percent: Some(42.0),
                resets_at: Some(1000),
                window_minutes: Some(300),
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(2000),
                window_minutes: Some(10_080),
            }),
        }];

        let rows = additional_pool_rows(&limits);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].limit_name, "GPT-5.3-Codex-Spark");
        assert!(!rows[0].unavailable);
        assert_eq!(rows[0].primary.as_ref().unwrap().used_percent, Some(42.0));
        assert_eq!(rows[0].secondary.as_ref().unwrap().used_percent, Some(10.0));
    }

    #[test]
    fn quota_window_spec_prefers_metadata_over_the_slot_default() {
        let weekly = WindowUsage {
            window_minutes: Some(10_080),
            ..Default::default()
        };
        let omitted = WindowUsage::default();

        assert_eq!(
            quota_window_spec(&weekly, "5h", WINDOW_5H_SECS),
            ("7d".to_string(), Some(WINDOW_7D_SECS))
        );
        assert_eq!(
            quota_window_spec(&omitted, "5h", WINDOW_5H_SECS),
            ("5h".to_string(), Some(WINDOW_5H_SECS))
        );
    }

    #[test]
    fn quota_window_spec_preserves_invalid_explicit_duration_as_unavailable() {
        let invalid = WindowUsage {
            window_minutes: Some(0),
            ..Default::default()
        };

        assert_eq!(
            quota_window_spec(&invalid, "5h", WINDOW_5H_SECS),
            ("0m".to_string(), None)
        );
    }

    #[test]
    fn limit_reached_pool_is_marked_unavailable() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("exhausted-pool".to_string()),
            limit_reached: Some(true),
            allowed: Some(true),
            ..Default::default()
        }];

        let rows = additional_pool_rows(&limits);
        assert!(rows[0].unavailable);
    }

    #[test]
    fn disallowed_pool_is_marked_unavailable() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("disallowed-pool".to_string()),
            allowed: Some(false),
            limit_reached: Some(false),
            ..Default::default()
        }];

        let rows = additional_pool_rows(&limits);
        assert!(rows[0].unavailable);
    }
}
