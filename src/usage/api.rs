use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth::{self, CLIENT_ID, format_reqwest_error};

use super::parse::parse_usage_checked;
use super::reset_credits::{ResetCreditRequestAuth, enrich_reset_credits};
use super::{
    ImportValidation, MAX_RETRIES, RETRY_DELAY, Refresh, RefreshOutcomeUnknown, RefreshedTokens,
    TerminalAuthError, TokenPersistFailure, UsageError, UsageInfo,
};

#[derive(Debug, thiserror::Error)]
#[error("network limiter closed")]
pub(super) struct NetworkLimiterClosed;

pub(crate) type FirstNetworkPermit = Pin<
    Box<dyn Future<Output = Result<Option<tokio::sync::OwnedSemaphorePermit>>> + Send + 'static>,
>;

/// Build the ordinary, non-cancellable admission wait used by CLI and daemon
/// callers. TUI callers wrap the same wait in their safe cancellation race.
pub(crate) fn first_network_permit(limiter: Arc<tokio::sync::Semaphore>) -> FirstNetworkPermit {
    Box::pin(async move {
        limiter
            .acquire_owned()
            .await
            .map(Some)
            .map_err(|_| NetworkLimiterClosed.into())
    })
}

#[derive(Debug, thiserror::Error)]
#[error("network wait cancelled before the first request")]
pub(crate) struct NetworkWaitCancelled;

pub(crate) fn network_wait_was_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NetworkWaitCancelled>().is_some()
}

pub(crate) fn network_wait_cancelled_error() -> anyhow::Error {
    NetworkWaitCancelled.into()
}

/// Consume one caller-defined admission wait at the first actual HTTP
/// exchange, then reacquire shared network capacity for later exchanges.
///
/// The deferred first wait means local authorization and request construction
/// do not occupy capacity, while the permit obtained by that one admission is
/// used directly by the request instead of being returned and reacquired. Each
/// exchange retains its permit through the response-body read and releases it
/// before credential persistence, retry backoff, or local cache work.
pub(crate) struct NetworkPermitBudget {
    first: Option<FirstNetworkPermit>,
    limiter: Option<Arc<tokio::sync::Semaphore>>,
    unlimited: bool,
    first_wait_cancelled: bool,
}

impl NetworkPermitBudget {
    pub(crate) fn new(first: FirstNetworkPermit) -> Self {
        Self {
            first: Some(first),
            limiter: None,
            unlimited: false,
            first_wait_cancelled: false,
        }
    }

    pub(super) fn unlimited() -> Self {
        Self {
            first: None,
            limiter: None,
            unlimited: true,
            first_wait_cancelled: false,
        }
    }

    pub(crate) fn first_wait_was_cancelled(&self) -> bool {
        self.first_wait_cancelled
    }

    pub(crate) async fn acquire(&mut self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
        if self.unlimited {
            return Ok(None);
        }
        if self.first_wait_cancelled {
            return Err(NetworkWaitCancelled.into());
        }
        if let Some(first) = self.first.take() {
            let Some(permit) = first.await? else {
                self.first_wait_cancelled = true;
                return Err(NetworkWaitCancelled.into());
            };
            self.limiter = Some(Arc::clone(permit.semaphore()));
            return Ok(Some(permit));
        }

        self.limiter
            .as_ref()
            .context("network budget was used before its first permit")?
            .clone()
            .acquire_owned()
            .await
            .map(Some)
            .map_err(|_| NetworkLimiterClosed.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetCreditEnrichment {
    Inline,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageCacheWrite {
    Store,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsageObservationPolicy {
    reset_credit_enrichment: ResetCreditEnrichment,
    cache_write: UsageCacheWrite,
}

const COMPLETE_USAGE_POLICY: UsageObservationPolicy = UsageObservationPolicy {
    reset_credit_enrichment: ResetCreditEnrichment::Inline,
    cache_write: UsageCacheWrite::Store,
};

const CORE_PROBE_POLICY: UsageObservationPolicy = UsageObservationPolicy {
    reset_credit_enrichment: ResetCreditEnrichment::Deferred,
    cache_write: UsageCacheWrite::Skip,
};

#[derive(Debug)]
pub(crate) struct UsageObservation {
    pub(crate) usage: UsageInfo,
    pub(crate) binding: crate::jwt::StrictAccountBinding,
}

/// A metadata-complete usage refresh prepared entirely under a profile lease.
/// The shared budget's first admission is not polled until execution reaches
/// an actual HTTP exchange.
pub(crate) struct PreparedFullUsageRequest {
    inner: PreparedUsageRequest,
}

/// Credential and routing state prepared under a profile lease before a
/// caller enters the scarce network budget.
pub(crate) struct PreparedCoreUsageRequest {
    state: PreparedCoreUsageState,
}

enum PreparedCoreUsageState {
    Cached(UsageInfo),
    Network(PreparedUsageRequest),
}

impl PreparedCoreUsageRequest {
    pub(crate) fn cached_usage(&self) -> Option<&UsageInfo> {
        match &self.state {
            PreparedCoreUsageState::Cached(usage) => Some(usage),
            PreparedCoreUsageState::Network(_) => None,
        }
    }
}

struct PreparedUsageRequest {
    alias: String,
    profile_path: std::path::PathBuf,
    cache_binding: crate::jwt::StrictAccountBinding,
    account_id: Option<String>,
    is_fedramp: bool,
    id_token: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    endpoints: auth::ServiceEndpoints,
}

/// A successful HTTP response whose JSON does not satisfy the usage schema.
/// Repeating the same read cannot turn that payload into trustworthy quota
/// data, so the outer network retry loop must surface it immediately.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
struct InvalidUsageResponse {
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageRequestFailureKind {
    Transport,
    DeterministicRequest,
    Http(reqwest::StatusCode),
}

/// A usage/refresh request failure whose retryability is determined by what
/// actually happened on the wire, not by parsing its display text. The outer
/// loop may retry transport failures and explicitly transient HTTP responses;
/// every other status is a deterministic result for the presented bearer.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
struct UsageRequestFailure {
    kind: UsageRequestFailureKind,
    detail: String,
}

impl UsageRequestFailure {
    fn request(context: &str, error: &reqwest::Error) -> anyhow::Error {
        let kind = if error.is_builder()
            || error.is_redirect()
            || error.is_status()
            || error.is_decode()
        {
            UsageRequestFailureKind::DeterministicRequest
        } else {
            UsageRequestFailureKind::Transport
        };
        Self {
            kind,
            detail: format_reqwest_error(context, error).to_string(),
        }
        .into()
    }

    fn transport(context: &str, error: &reqwest::Error) -> anyhow::Error {
        Self {
            kind: UsageRequestFailureKind::Transport,
            detail: format_reqwest_error(context, error).to_string(),
        }
        .into()
    }

    fn http(status: reqwest::StatusCode, detail: String) -> anyhow::Error {
        Self {
            kind: UsageRequestFailureKind::Http(status),
            detail,
        }
        .into()
    }

    fn is_retryable(&self) -> bool {
        match self.kind {
            UsageRequestFailureKind::Transport => true,
            UsageRequestFailureKind::DeterministicRequest => false,
            UsageRequestFailureKind::Http(status) => {
                matches!(
                    status,
                    reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
                ) || status.is_server_error()
            }
        }
    }
}

fn usage_response_json_error(context: &str, error: &reqwest::Error) -> anyhow::Error {
    if error.is_decode() {
        InvalidUsageResponse {
            detail: format!("{context}: {error}"),
        }
        .into()
    } else {
        UsageRequestFailure::transport(context, error)
    }
}

fn parse_checked_usage_response(body: &Value) -> Result<UsageInfo> {
    parse_usage_checked(body).map_err(|error| {
        InvalidUsageResponse {
            detail: format!("{error:#}"),
        }
        .into()
    })
}

pub(crate) fn apply_account_routing_headers(
    mut builder: reqwest::RequestBuilder,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> reqwest::RequestBuilder {
    if let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) {
        builder = builder.header("ChatGPT-Account-ID", account_id);
    }
    if is_fedramp {
        builder = builder.header("X-OpenAI-Fedramp", "true");
    }
    builder
}

/// The auth server reports failures in two shapes: the OAuth 2.0 standard
/// `{"error": "invalid_grant", "error_description": "..."}` and OpenAI's
/// `{"error": {"code": ..., "message": ..., "type": ...}}`. Accept both, or the
/// whole response fails to deserialize and the actionable server message is
/// replaced by a serde type error.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RefreshError {
    Code(String),
    Detail {
        code: Option<String>,
        message: Option<String>,
        #[serde(rename = "type")]
        kind: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<RefreshError>,
    error_description: Option<String>,
}

#[derive(Debug)]
pub(crate) enum RefreshTokenResolution {
    Validated(RefreshedTokens),
    RotatedButInvalid {
        recovery: Value,
        cause: anyhow::Error,
    },
}

impl RefreshResponse {
    /// Normalize both wire shapes to `(code, message)`.
    fn error_parts(&self) -> Option<(String, Option<String>)> {
        match self.error.as_ref()? {
            RefreshError::Code(code) => nonempty_error_code(code)
                .map(|code| (code.to_string(), self.error_description.clone())),
            RefreshError::Detail {
                code,
                message,
                kind,
            } => code
                .as_deref()
                .and_then(nonempty_error_code)
                .or_else(|| kind.as_deref().and_then(nonempty_error_code))
                .map(|code| {
                    (
                        code.to_string(),
                        message.clone().or_else(|| self.error_description.clone()),
                    )
                }),
        }
    }
}

fn nonempty_error_code(code: &str) -> Option<&str> {
    let code = code.trim();
    (!code.is_empty()).then_some(code)
}

fn refresh_outcome_unknown(cause: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(RefreshOutcomeUnknown::new(cause))
}

/// Auth-server verdicts no retry can change, independent of HTTP status.
const TERMINAL_AUTH_CODES: &[&str] = &[
    "refresh_token_reused",
    "refresh_token_invalidated",
    "invalid_grant",
    "invalid_client",
    "unauthorized_client",
    "access_denied",
];

/// The subset of [`TERMINAL_AUTH_CODES`] that may outlive the invocation.
///
/// Both are OpenAI-specific and say one unambiguous thing: *this* credential is
/// gone, and only signing in again produces another. Everything else in
/// `TERMINAL_AUTH_CODES` is standard OAuth wording that assorted servers and
/// intermediaries also emit for transient conditions — `invalid_grant` for
/// clock skew, `access_denied` from a gateway — and a bare 4xx can as easily be
/// a proxy, a WAF, or a captive portal in front of the real endpoint.
///
/// Guessing wrong in this direction is expensive: a recorded verdict survives
/// until the next sign-in, so a transient cause would leave a working account
/// showing "re-login required" with nothing to suggest that `--force` clears
/// it. Guessing wrong the other way costs one round trip. So only these two are
/// remembered; every code in `TERMINAL_AUTH_CODES` still stops the retry loop
/// within the call it happened in.
const MEMORABLE_AUTH_CODES: &[&str] = &["refresh_token_reused", "refresh_token_invalidated"];

fn is_memorable_auth_verdict(code: &str) -> bool {
    MEMORABLE_AUTH_CODES.contains(&code)
}

/// A 4xx from the token endpoint means the credential itself was rejected, so
/// replaying it only re-triggers reuse detection. 429/408 are load/timing
/// signals and stay retryable.
fn is_terminal_auth_failure(code: &str, status: reqwest::StatusCode) -> bool {
    if matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::REQUEST_TIMEOUT
    ) {
        return false;
    }
    TERMINAL_AUTH_CODES.contains(&code) || status.is_client_error()
}

/// Record a verdict against the credential that earned it.
///
/// Keyed by the token rather than the alias so that signing in again clears it
/// without every credential-writing path having to remember to.
pub(super) async fn remember_terminal_verdict(
    alias: &str,
    code: &str,
    refresh_token: Option<&str>,
    error: &UsageError,
) {
    if !is_memorable_auth_verdict(code) {
        return;
    }
    let Some(refresh_token) = refresh_token else {
        return;
    };
    if let Err(cache_error) =
        crate::cache::put_auth_failure_async(alias, refresh_token, error).await
    {
        warn!("[{alias}] could not record the terminal auth verdict in cache: {cache_error:#}");
    }
}

/// Return a standing auth-server verdict for this exact credential.
///
/// Cache-read failures are terminal for an unattended refresh decision: when
/// we cannot prove that a single-use token is still eligible, submitting it
/// would risk replaying a credential the server already rejected.
pub(super) async fn cached_terminal_auth_verdict(
    alias: &str,
    refresh_token: &str,
) -> std::result::Result<Option<UsageError>, UsageError> {
    crate::cache::get_auth_failure_async(alias, refresh_token)
        .await
        .map_err(|error| UsageError {
            summary: "auth cache unreadable".to_string(),
            detail: format!(
                "[{alias}] could not safely decide whether this credential was already rejected: {error:#}"
            ),
        })
}

fn format_refresh_error(code: &str, message: Option<&str>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

fn access_token_needs_refresh(access_token: &str, margin_secs: i64) -> Result<bool> {
    Ok(crate::jwt::is_token_expiring(access_token, margin_secs)? == Some(true))
}

/// Extract a short summary from an error message for user-facing display.
/// Looks for "HTTP <status>" patterns; falls back to first line truncated.
pub(super) fn extract_error_summary(err: &str) -> String {
    // Look for "HTTP 4xx ..." or "HTTP 5xx ..." pattern
    if let Some(pos) = err.find("HTTP ") {
        let rest = &err[pos..];
        // Take until comma, closing paren, or end
        let end = rest.find([',', ')']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    // Fallback: first line, truncated
    let first_line = err.lines().next().unwrap_or(err);
    let mut chars = first_line.chars();
    let preview: String = chars.by_ref().take(60).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        first_line.to_string()
    }
}

/// High-level: fetch usage with retry, token refresh, and disk cache.
pub async fn fetch_usage_retried(
    alias: &str,
    profile_path: &Path,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, Refresh::Cached).await
}

/// Bypass the usage TTL for current numbers, but leave a recorded auth verdict
/// standing. For callers running on a timer with nobody watching.
pub async fn fetch_usage_retried_unattended(
    alias: &str,
    profile_path: &Path,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, Refresh::Unattended).await
}

/// Bypass every cache, including a recorded auth verdict. Only for a person
/// explicitly asking again — see [`Refresh::Forced`].
pub async fn fetch_usage_retried_force(
    alias: &str,
    profile_path: &Path,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, Refresh::Forced).await
}

/// Perform one explicit usage lookup and return the strict identity derived
/// from the same auth snapshot that supplied its request credentials. A
/// confirmation flow can carry that binding across its consent gap without a
/// separate preflight auth read.
pub(crate) async fn fetch_usage_observation_force_with_existing_lease_and_client(
    alias: &str,
    profile_path: &Path,
    lease: &crate::profile::ProfileLease,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
    client: &reqwest::Client,
) -> std::result::Result<UsageObservation, UsageError> {
    ensure_usage_configuration()?;
    fetch_usage_observation_with_lease_and_client(
        alias,
        profile_path,
        Refresh::Forced,
        lease,
        expected_binding,
        client,
        COMPLETE_USAGE_POLICY,
    )
    .await
}

pub(crate) async fn fetch_usage_retried_with_existing_lease_and_client(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
    expected_binding: &crate::jwt::StrictAccountBinding,
    client: &reqwest::Client,
) -> std::result::Result<UsageInfo, UsageError> {
    ensure_usage_configuration()?;
    if let Some(cached) =
        usage_cache_hit(alias, profile_path, refresh, Some(expected_binding)).await?
    {
        return Ok(cached);
    }
    fetch_usage_observation_with_lease_and_client(
        alias,
        profile_path,
        refresh,
        lease,
        Some(expected_binding),
        client,
        COMPLETE_USAGE_POLICY,
    )
    .await
    .map(|observation| observation.usage)
}

/// Probe current quota for an unattended decision without reading or writing
/// the usage cache and without contacting the dedicated reset-credit endpoint.
///
/// This is deliberately cache-neutral: a daemon decision must not replace a
/// metadata-complete cache entry with its quota-only response. Token rotation
/// and persistence still happen under the caller-owned profile lease.
pub(crate) async fn probe_core_usage_unattended_with_existing_lease_and_client(
    alias: &str,
    profile_path: &Path,
    lease: &crate::profile::ProfileLease,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
    client: &reqwest::Client,
) -> std::result::Result<UsageInfo, UsageError> {
    ensure_usage_configuration()?;
    fetch_usage_observation_with_lease_and_client(
        alias,
        profile_path,
        Refresh::Unattended,
        lease,
        expected_binding,
        client,
        CORE_PROBE_POLICY,
    )
    .await
    .map(|observation| observation.usage)
}

/// Prepare the cache-neutral automatic-selection quota request without holding
/// a network permit. The caller must keep `lease` alive and pass the same lease
/// to [`execute_prepared_core_usage_with_existing_lease_and_client`].
pub(crate) async fn prepare_core_usage_unattended_with_existing_lease(
    alias: &str,
    profile_path: &Path,
    lease: &crate::profile::ProfileLease,
    expected_binding: &crate::jwt::StrictAccountBinding,
) -> std::result::Result<PreparedCoreUsageRequest, UsageError> {
    ensure_usage_configuration()?;
    prepare_usage_request(
        alias,
        profile_path,
        Refresh::Unattended,
        lease,
        Some(expected_binding),
    )
    .await
    .map(|inner| PreparedCoreUsageRequest {
        state: PreparedCoreUsageState::Network(inner),
    })
}

/// Resolve cache and credential state under the caller-owned profile lease,
/// before a TUI worker enters the scarce network budget. A cache hit is carried
/// as a ready result so it never acquires a permit at all.
pub(crate) async fn prepare_core_usage_with_existing_lease(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
    expected_binding: &crate::jwt::StrictAccountBinding,
) -> std::result::Result<PreparedCoreUsageRequest, UsageError> {
    ensure_usage_configuration()?;
    if let Some(cached) =
        usage_cache_hit(alias, profile_path, refresh, Some(expected_binding)).await?
    {
        return Ok(PreparedCoreUsageRequest {
            state: PreparedCoreUsageState::Cached(cached),
        });
    }
    prepare_usage_request(alias, profile_path, refresh, lease, Some(expected_binding))
        .await
        .map(|inner| PreparedCoreUsageRequest {
            state: PreparedCoreUsageState::Network(inner),
        })
}

/// Execute a request prepared by
/// [`prepare_core_usage_unattended_with_existing_lease`]. The shared budget
/// releases each request permit before terminal-verdict or cache work begins.
pub(crate) async fn execute_prepared_core_usage_with_existing_lease_and_client(
    prepared: PreparedCoreUsageRequest,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<UsageInfo, UsageError> {
    let inner = match prepared.state {
        PreparedCoreUsageState::Cached(usage) => return Ok(usage),
        PreparedCoreUsageState::Network(inner) => inner,
    };
    execute_prepared_usage_request(
        inner,
        lease,
        client,
        ResetCreditEnrichment::Deferred,
        UsageCacheWrite::Skip,
        network,
    )
    .await
    .map(|observation| observation.usage)
}

/// Prepare a metadata-complete usage refresh without consuming network
/// capacity. Unlike the core-only probe, execution enriches reset-card data
/// and publishes the resulting identity-bound cache entry.
pub(crate) async fn prepare_full_usage_with_existing_lease(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
) -> std::result::Result<PreparedFullUsageRequest, UsageError> {
    ensure_usage_configuration()?;
    prepare_usage_request(alias, profile_path, refresh, lease, expected_binding)
        .await
        .map(|inner| PreparedFullUsageRequest { inner })
}

/// Execute a prepared metadata-complete refresh. The shared budget releases
/// each request permit before independent disk-cache publication begins.
pub(crate) async fn execute_prepared_full_usage_with_existing_lease_and_client(
    prepared: PreparedFullUsageRequest,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<UsageObservation, UsageError> {
    execute_prepared_usage_request(
        prepared.inner,
        lease,
        client,
        ResetCreditEnrichment::Inline,
        UsageCacheWrite::Store,
        network,
    )
    .await
}

/// Write credentials the auth server just rotated back to the profile.
///
/// The previous `refresh_token` is dead the moment these were issued, so a
/// failed write leaves only an in-memory copy of the sole credential the server
/// still accepts. Losing it bricks the account, which makes this a reportable
/// failure rather than something to warn about and walk past.
fn persist_refreshed_tokens(
    lease: &crate::profile::ProfileLease,
    authorization: crate::profile::FreshCredentialsActivationAuthorization,
    presented_refresh_token: &str,
    new_tokens: &RefreshedTokens,
) -> std::result::Result<(), UsageError> {
    let alias = lease.alias();
    let update = crate::profile::update_profile_tokens_if_refresh_matches_leased(
        lease,
        authorization,
        presented_refresh_token,
        &new_tokens.id_token,
        &new_tokens.access_token,
        &new_tokens.refresh_token,
    )
    .map_err(|err| UsageError::token_persist_failed(alias, &err))?;
    match update {
        crate::profile::RefreshTokenUpdate::Saved => Ok(()),
        crate::profile::RefreshTokenUpdate::Superseded => {
            Err(UsageError::token_update_superseded(alias))
        }
        crate::profile::RefreshTokenUpdate::SavedWithActivationIncomplete { cause } => {
            Err(UsageError::live_activation_incomplete(alias, &cause))
        }
        crate::profile::RefreshTokenUpdate::Quarantined { path, cause } => Err(
            UsageError::refreshed_credentials_quarantined(alias, &path, &cause),
        ),
    }
}

/// Complete the local side of a refresh-token rotation before any follow-up
/// network request. Invalid responses with a non-empty successor are never
/// installed; their raw token fields are durably quarantined instead.
pub(crate) fn persist_refresh_resolution(
    lease: &crate::profile::ProfileLease,
    authorization: crate::profile::FreshCredentialsActivationAuthorization,
    presented_refresh_token: &str,
    resolution: RefreshTokenResolution,
) -> std::result::Result<RefreshedTokens, UsageError> {
    let alias = lease.alias();
    match resolution {
        RefreshTokenResolution::Validated(tokens) => {
            persist_refreshed_tokens(lease, authorization, presented_refresh_token, &tokens)?;
            Ok(tokens)
        }
        RefreshTokenResolution::RotatedButInvalid { recovery, cause } => {
            let update = crate::profile::quarantine_invalid_refresh_response_leased(
                lease,
                authorization,
                presented_refresh_token,
                &recovery,
                cause,
            )
            .map_err(|error| UsageError::invalid_refresh_recovery_failed(alias, &error))?;
            match update {
                crate::profile::RefreshTokenUpdate::Quarantined { path, cause } => Err(
                    UsageError::refreshed_credentials_quarantined(alias, &path, &cause),
                ),
                crate::profile::RefreshTokenUpdate::Saved
                | crate::profile::RefreshTokenUpdate::Superseded
                | crate::profile::RefreshTokenUpdate::SavedWithActivationIncomplete { .. } => {
                    Err(UsageError::invalid_refresh_recovery_failed(
                        alias,
                        &anyhow::anyhow!(
                            "invalid refresh response did not produce a quarantine outcome"
                        ),
                    ))
                }
            }
        }
    }
}

fn persist_unbound_refresh_resolution<F>(
    alias: &str,
    presented_refresh_token: &str,
    resolution: RefreshTokenResolution,
    persist_rotation: &mut F,
) -> Result<RefreshedTokens>
where
    F: FnMut(&str, RefreshedTokens) -> Result<()>,
{
    match resolution {
        RefreshTokenResolution::Validated(tokens) => {
            persist_rotation(presented_refresh_token, tokens.clone())?;
            Ok(tokens)
        }
        RefreshTokenResolution::RotatedButInvalid { recovery, cause } => {
            match crate::profile::stage_import_rotation(&recovery) {
                Ok(stage) => Err(cause.context(format!(
                    "[{alias}] token refresh returned an invalid token set; its non-empty successor refresh token was preserved privately at {} and was not installed",
                    stage.path().display()
                ))),
                Err(recovery_error) => anyhow::bail!(
                    "[{alias}] token refresh returned an invalid token set ({cause:#}) after issuing a non-empty successor refresh token, and its private recovery copy failed ({recovery_error:#}); the previous refresh token may already be invalid"
                ),
            }
        }
    }
}

fn resolve_refreshed_tokens(
    response: RefreshResponse,
    status: reqwest::StatusCode,
    current_id_token: Option<&str>,
    current_refresh_token: &str,
) -> Result<RefreshTokenResolution> {
    if let Some((code, message)) = response.error_parts() {
        if is_terminal_auth_failure(&code, status) {
            return Err(TerminalAuthError { code, message }.into());
        }
        return Err(UsageRequestFailure::http(
            status,
            format!(
                "token refresh failed: {}",
                format_refresh_error(&code, message.as_deref())
            ),
        ));
    }

    // Only a structured OAuth error proves that the server rejected the
    // rotation. A proxy error, empty body, or otherwise unrecognized non-2xx
    // response can arrive after the endpoint consumed the single-use token.
    if !status.is_success() {
        return Err(refresh_outcome_unknown(anyhow::anyhow!(
            "token refresh returned HTTP {status} without a recognizable OAuth error"
        )));
    }

    let returned_nonempty_refresh = response
        .refresh_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty());
    let recovery = serde_json::json!({
        "id_token": response.id_token.clone(),
        "access_token": response.access_token.clone(),
        "refresh_token": response.refresh_token.clone(),
        "recovery_kind": "invalid_token_refresh_response"
    });
    let resolved = (|| -> Result<RefreshedTokens> {
        let id_token = response
            .id_token
            .or_else(|| current_id_token.map(str::to_string))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "token refresh response omitted id_token and no existing id_token is available"
                )
            })?;
        // OAuth success responses must explicitly issue an access token. Using
        // the existing value here would turn an empty HTTP 2xx body into a
        // false success even though the server may already have rotated the
        // refresh token.
        let access_token = response.access_token.ok_or_else(|| {
            anyhow::anyhow!("token refresh success response omitted access_token")
        })?;
        let refresh_token = response
            .refresh_token
            .unwrap_or_else(|| current_refresh_token.to_string());
        auth::validate_complete_oauth_tokens(&id_token, &access_token, &refresh_token)
            .context("invalid token refresh response")?;

        Ok(RefreshedTokens {
            id_token,
            access_token,
            refresh_token,
        })
    })();

    match resolved {
        Ok(tokens) => Ok(RefreshTokenResolution::Validated(tokens)),
        Err(cause) if returned_nonempty_refresh => {
            Ok(RefreshTokenResolution::RotatedButInvalid { recovery, cause })
        }
        Err(cause) => Err(refresh_outcome_unknown(
            cause.context("invalid token refresh success response"),
        )),
    }
}

/// Credentials re-read from a profile after a refresh was rejected.
struct ReloadedCredentials {
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
}

/// Re-read `profile_path` after the auth server rejected a refresh outright.
///
/// The daemon timer and the CLI (`list`, `best`) refresh the same profile from
/// separate processes, so both can read the same `refresh_token` and present
/// it. The server rotates it for exactly one of them and answers the other
/// `refresh_token_reused` — a verdict about that *token*, not about the
/// account, whose live credentials the winner has meanwhile written to disk.
///
/// Returns the stored credentials only when their `refresh_token` differs from
/// `presented`. An unchanged profile means nobody rotated anything, so the
/// rejection is the real thing and the caller must keep reporting it.
fn reload_rotated_credentials(
    profile_path: &Path,
    presented: Option<&str>,
) -> Option<ReloadedCredentials> {
    let val = auth::read_auth(profile_path).ok()?;
    let (access_token, refresh_token) = auth::extract_tokens(&val);
    let refresh_token = refresh_token?;
    if Some(refresh_token.as_str()) == presented {
        return None;
    }
    Some(ReloadedCredentials {
        id_token: auth::extract_id_token(&val),
        access_token: access_token?,
        refresh_token,
    })
}

async fn fetch_usage_retried_inner(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
) -> std::result::Result<UsageInfo, UsageError> {
    ensure_usage_configuration()?;
    let lease = acquire_usage_profile_lease(alias).await?;
    if let Some(cached) = usage_cache_hit(alias, profile_path, refresh, None).await? {
        return Ok(cached);
    }

    let client = build_usage_http_client(alias)?;
    fetch_usage_retried_with_lease_and_client(
        alias,
        profile_path,
        refresh,
        &lease,
        &client,
        ResetCreditEnrichment::Inline,
        UsageCacheWrite::Store,
    )
    .await
}

fn build_usage_http_client(alias: &str) -> std::result::Result<reqwest::Client, UsageError> {
    auth::build_http_client().map_err(|error| UsageError {
        summary: "HTTP client unavailable".to_string(),
        detail: format!("[{alias}] could not build HTTP client: {error:#}"),
    })
}

async fn acquire_usage_profile_lease(
    alias: &str,
) -> std::result::Result<crate::profile::ProfileLease, UsageError> {
    crate::profile::acquire_profile_lease_async(alias.to_string())
        .await
        .map_err(|error| UsageError {
            summary: "profile lock failed".to_string(),
            detail: format!("[{alias}] could not lock profile for usage refresh: {error:#}"),
        })
}

fn ensure_usage_configuration() -> std::result::Result<(), UsageError> {
    crate::config::try_get()
        .map(|_| ())
        .map_err(|error| UsageError {
            summary: "configuration unavailable".to_string(),
            detail: format!("usage request cannot start: {error:#}"),
        })
}

async fn usage_cache_hit(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
) -> std::result::Result<Option<UsageInfo>, UsageError> {
    if refresh.skips_usage_cache() {
        debug!("{alias}: {refresh:?} refresh, bypassing the usage cache");
        return Ok(None);
    }
    let binding = auth::read_auth_async(profile_path)
        .await
        .map(|value| crate::auth::account_info_from_auth_value(&value))
        .map_err(|error| UsageError {
            summary: "auth file unreadable".to_string(),
            detail: format!(
                "[{alias}] could not verify identity before reading usage cache: {error:#}"
            ),
        })?
        .strict_binding()
        .ok_or_else(|| UsageError {
            summary: "account identity incomplete".to_string(),
            detail: format!(
                "[{alias}] usage cache requires a verified account id and email identity"
            ),
        })?;
    if expected_binding.is_some_and(|expected| expected != &binding) {
        return Err(UsageError {
            summary: "profile identity changed".to_string(),
            detail: format!(
                "[{alias}] profile identity changed while the usage request was queued"
            ),
        });
    }
    match crate::cache::get_bound_async(alias, &binding)
        .await
        .map_err(|error| UsageError {
            summary: "usage cache unreadable".to_string(),
            detail: format!("[{alias}] failed to read usage cache: {error:#}"),
        })? {
        Some(cached) => {
            debug!("{alias}: cache hit");
            Ok(Some(cached))
        }
        None => {
            debug!("{alias}: cache miss, fetching from API");
            Ok(None)
        }
    }
}

async fn fetch_usage_retried_with_lease_and_client(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
    reset_credit_enrichment: ResetCreditEnrichment,
    cache_write: UsageCacheWrite,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_observation_with_lease_and_client(
        alias,
        profile_path,
        refresh,
        lease,
        None,
        client,
        UsageObservationPolicy {
            reset_credit_enrichment,
            cache_write,
        },
    )
    .await
    .map(|observation| observation.usage)
}

async fn fetch_usage_observation_with_lease_and_client(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
    client: &reqwest::Client,
    policy: UsageObservationPolicy,
) -> std::result::Result<UsageObservation, UsageError> {
    let prepared =
        prepare_usage_request(alias, profile_path, refresh, lease, expected_binding).await?;
    let mut network = NetworkPermitBudget::unlimited();
    execute_prepared_usage_request(
        prepared,
        lease,
        client,
        policy.reset_credit_enrichment,
        policy.cache_write,
        &mut network,
    )
    .await
}

async fn prepare_usage_request(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
) -> std::result::Result<PreparedUsageRequest, UsageError> {
    if lease.alias() != alias {
        return Err(UsageError {
            summary: "profile lock mismatch".to_string(),
            detail: format!(
                "usage request for '{alias}' received lease for '{}'",
                lease.alias()
            ),
        });
    }

    let val = auth::read_auth_async(profile_path).await.map_err(|e| {
        let detail = format!("failed to read auth file {}: {e}", profile_path.display());
        UsageError {
            summary: "auth file unreadable".into(),
            detail,
        }
    })?;
    let account_info = crate::jwt::parse_account_info(&val);
    let cache_binding = account_info.strict_binding().ok_or_else(|| UsageError {
        summary: "account identity incomplete".to_string(),
        detail: format!("[{alias}] usage refresh requires a verified account id and email"),
    })?;
    if expected_binding.is_some_and(|expected| expected != &cache_binding) {
        return Err(UsageError {
            summary: "profile identity changed".to_string(),
            detail: format!(
                "[{alias}] profile identity changed while the usage request was queued"
            ),
        });
    }
    let account_id = account_info.account_id;
    let is_fedramp = account_info.is_fedramp;
    let id_token = auth::extract_id_token(&val);
    let (access_token, refresh_token) = auth::extract_tokens(&val);

    // A verdict the auth server already named stands until the credential is
    // replaced, so re-presenting it buys nothing but the round trip. Only an
    // explicit user force skips this — see [`Refresh`].
    if !refresh.may_re_present_a_rejected_credential()
        && let Some(rt) = refresh_token.as_deref()
        && let Some(known) = cached_terminal_auth_verdict(alias, rt).await?
    {
        debug!("{alias}: credential already rejected by the auth server, not retrying");
        return Err(known);
    }

    let access_token = match access_token {
        Some(t) => t,
        None => {
            return Err(UsageError {
                summary: "no access_token".into(),
                detail: "no access_token in auth file".into(),
            });
        }
    };

    let endpoints = auth::service_endpoints().map_err(|error| UsageError {
        summary: "service endpoint policy invalid".to_string(),
        detail: format!("[{alias}] could not resolve service endpoints: {error:#}"),
    })?;
    Ok(PreparedUsageRequest {
        alias: alias.to_string(),
        profile_path: profile_path.to_path_buf(),
        cache_binding,
        account_id,
        is_fedramp,
        id_token,
        access_token,
        refresh_token,
        endpoints,
    })
}

async fn execute_prepared_usage_request(
    prepared: PreparedUsageRequest,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
    reset_credit_enrichment: ResetCreditEnrichment,
    cache_write: UsageCacheWrite,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<UsageObservation, UsageError> {
    let PreparedUsageRequest {
        alias,
        profile_path,
        cache_binding,
        account_id,
        is_fedramp,
        mut id_token,
        access_token,
        mut refresh_token,
        endpoints,
    } = prepared;
    if lease.alias() != alias {
        return Err(UsageError {
            summary: "profile lock mismatch".to_string(),
            detail: format!(
                "prepared usage request for '{alias}' received lease for '{}'",
                lease.alias()
            ),
        });
    }
    let alias = alias.as_str();
    let profile_path = profile_path.as_path();
    let mut at = access_token;
    let mut last_err = String::new();
    let mut last_summary = String::new();
    // A rejected refresh may just mean a concurrent refresh of the same profile
    // won the rotation, so one such rejection buys a single extra round in which
    // the winner's stored token is tried. Granted at most once: two peers each
    // re-arming on the other's write would otherwise keep this loop alive
    // without either ever reporting a result.
    let mut recovery_round_used = false;
    // Carries the server's error code alongside the error so the verdict can be
    // recorded if the recovery round confirms it.
    let mut pending_terminal: Option<(UsageError, String)> = None;
    let mut max_attempts = MAX_RETRIES;
    let mut attempt = 0;
    while attempt < max_attempts {
        if attempt > 0 {
            debug!("[{alias}] retry attempt {}/{max_attempts}", attempt + 1);
            tokio::time::sleep(RETRY_DELAY).await;
        }

        // Deliberately *after* the delay. The winner writes the rotated token
        // only once the server has issued it, which is already when our replay
        // starts being refused — reading the profile the instant the rejection
        // arrives can still find the old token and mislabel a healthy account.
        if let Some((terminal, code)) = pending_terminal.take() {
            let Some(stored) = reload_rotated_credentials(profile_path, refresh_token.as_deref())
            else {
                // Nothing else rotated the credential, so the rejection was
                // about the token this profile still holds — final.
                remember_terminal_verdict(alias, &code, refresh_token.as_deref(), &terminal).await;
                return Err(terminal);
            };
            info!(
                "[{alias}] refresh was rejected but the profile now holds a different token; \
                 a concurrent refresh won the rotation, retrying with the stored credentials"
            );
            at = stored.access_token;
            id_token = stored.id_token;
            refresh_token = Some(stored.refresh_token);
        }

        let mut rotated_tokens = None;
        let mut authorization_failure = None;
        let mut persist_failure = None;
        let result = {
            let mut authorize_rotation = || {
                crate::profile::authorize_fresh_credentials_activation(lease).map_err(|error| {
                    let error = UsageError::refresh_authorization_failed(alias, &error);
                    let detail = error.detail.clone();
                    authorization_failure = Some(error);
                    anyhow::anyhow!(detail)
                })
            };
            let mut persist_before_follow_up =
                |activation_authorization: crate::profile::FreshCredentialsActivationAuthorization,
                 presented: &str,
                 resolution: RefreshTokenResolution|
                 -> Result<RefreshedTokens> {
                    match persist_refresh_resolution(
                        lease,
                        activation_authorization,
                        presented,
                        resolution,
                    ) {
                        Ok(tokens) => {
                            rotated_tokens = Some(tokens.clone());
                            Ok(tokens)
                        }
                        Err(error) => {
                            let detail = error.detail.clone();
                            persist_failure = Some(error);
                            Err(anyhow::anyhow!(detail))
                        }
                    }
                };
            fetch_usage_with_refresh_transactional(
                &endpoints,
                client,
                alias,
                &at,
                id_token.as_deref(),
                refresh_token.as_deref(),
                account_id.as_deref(),
                is_fedramp,
                &mut authorize_rotation,
                &mut persist_before_follow_up,
                reset_credit_enrichment,
                network,
            )
            .await
        };

        // Authorization runs immediately before the refresh endpoint. A local
        // failure therefore spends no token and must not be retried as a
        // network failure.
        if let Some(error) = authorization_failure {
            return Err(error);
        }
        // Persistence is invoked synchronously from the successful refresh
        // branch, before that branch can send its follow-up usage GET. A failed
        // write is terminal for this call: retrying would spend another
        // single-use token that we still cannot keep.
        if let Some(error) = persist_failure {
            return Err(error);
        }
        if let Some(new_tokens) = rotated_tokens {
            at = new_tokens.access_token;
            id_token = Some(new_tokens.id_token);
            refresh_token = Some(new_tokens.refresh_token);
        }

        match result {
            Ok(usage) => {
                if cache_write == UsageCacheWrite::Skip {
                    return Ok(UsageObservation {
                        usage,
                        binding: cache_binding,
                    });
                }
                let usage = match crate::cache::put_bound_versioned_async(
                    alias,
                    &cache_binding,
                    &usage,
                )
                .await
                {
                    Ok(versioned) => versioned,
                    Err(error) => {
                        warn!(
                            "[{alias}] usage succeeded, but caching the result failed: {error:#}"
                        );
                        usage
                    }
                };
                return Ok(UsageObservation {
                    usage,
                    binding: cache_binding,
                });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if e.downcast_ref::<InvalidUsageResponse>().is_some() {
                    warn!("[{alias}] Usage API response rejected without retry: {msg}");
                    return Err(UsageError {
                        summary: extract_error_summary(&msg),
                        detail: msg,
                    });
                }
                warn!(
                    "[{alias}] attempt {}/{max_attempts} failed: {msg}",
                    attempt + 1
                );
                if let Some(terminal) = e.downcast_ref::<TerminalAuthError>() {
                    let error = UsageError {
                        summary: terminal.summary(),
                        detail: msg,
                    };
                    let code = terminal.code.clone();
                    if recovery_round_used {
                        remember_terminal_verdict(alias, &code, refresh_token.as_deref(), &error)
                            .await;
                        return Err(error);
                    }
                    recovery_round_used = true;
                    // Add the round rather than spend one of the existing ones,
                    // so a rejection arriving on the final attempt is still
                    // checked against the profile before the account is failed.
                    max_attempts += 1;
                    pending_terminal = Some((error, code));
                    attempt += 1;
                    continue;
                }
                if e.downcast_ref::<RefreshOutcomeUnknown>().is_some() {
                    return Err(UsageError::refresh_outcome_unknown(alias, &e));
                }
                if e.downcast_ref::<NetworkLimiterClosed>().is_some() {
                    return Err(UsageError {
                        summary: "usage limiter closed".to_string(),
                        detail: format!("[{alias}] usage retry could not reserve network capacity"),
                    });
                }
                let Some(request_failure) = e.downcast_ref::<UsageRequestFailure>() else {
                    warn!("[{alias}] usage request failed without retry: {msg}");
                    return Err(UsageError {
                        summary: extract_error_summary(&msg),
                        detail: msg,
                    });
                };
                if !request_failure.is_retryable() {
                    warn!("[{alias}] Usage API response rejected without retry: {msg}");
                    return Err(UsageError {
                        summary: extract_error_summary(&msg),
                        detail: msg,
                    });
                }
                last_summary = extract_error_summary(&msg);
                last_err = msg;
            }
        }
        attempt += 1;
    }
    Err(UsageError {
        summary: last_summary,
        detail: last_err,
    })
}

/// Fetch usage, automatically refreshing on expiry or a 401/403 response.
///
/// `persist_rotation` runs synchronously immediately after a refresh response
/// is decoded and before any follow-up usage request. It must durably save the
/// rotated credential: returning `Ok(())` without doing so can lose the only
/// refresh token the auth server still accepts if this future is later
/// cancelled. A persistence error stops the request before another token can be
/// spent.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_usage_with_refresh<F>(
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
    persist_rotation: &mut F,
) -> Result<UsageInfo>
where
    F: FnMut(&str, RefreshedTokens) -> Result<()>,
{
    let endpoints = auth::service_endpoints()?;
    let client = auth::build_http_client()?;
    fetch_usage_with_refresh_at(
        &endpoints,
        &client,
        alias,
        access_token,
        id_token,
        refresh_token,
        account_id,
        is_fedramp,
        persist_rotation,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fetch_usage_with_refresh_at<F>(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
    persist_rotation: &mut F,
) -> Result<UsageInfo>
where
    F: FnMut(&str, RefreshedTokens) -> Result<()>,
{
    let mut network = NetworkPermitBudget::unlimited();
    let mut authorize_rotation = || Ok(());
    let mut persist_authorized = |(): (), presented: &str, resolution: RefreshTokenResolution| {
        persist_unbound_refresh_resolution(alias, presented, resolution, persist_rotation)
    };
    fetch_usage_with_refresh_transactional(
        endpoints,
        client,
        alias,
        access_token,
        id_token,
        refresh_token,
        account_id,
        is_fedramp,
        &mut authorize_rotation,
        &mut persist_authorized,
        ResetCreditEnrichment::Inline,
        &mut network,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_usage_fetch(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    alias: &str,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    reset_credit_enrichment: ResetCreditEnrichment,
    network: &mut NetworkPermitBudget,
    mut usage: UsageInfo,
) -> UsageInfo {
    if reset_credit_enrichment == ResetCreditEnrichment::Inline {
        enrich_reset_credits(
            endpoints,
            alias,
            client,
            ResetCreditRequestAuth::new(access_token, account_id, is_fedramp),
            &mut usage,
            network,
        )
        .await;
    }
    usage
}

struct BufferedUsageResponse {
    status: reqwest::StatusCode,
    body: Option<Value>,
}

#[allow(clippy::too_many_arguments)]
async fn send_usage_request(
    client: &reqwest::Client,
    usage_url: &str,
    bearer: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    request_context: &'static str,
    body_context: &'static str,
    network: &mut NetworkPermitBudget,
) -> Result<BufferedUsageResponse> {
    let request = apply_account_routing_headers(
        client
            .get(usage_url)
            .header("Authorization", format!("Bearer {bearer}")),
        account_id,
        is_fedramp,
    );
    let (status, body) = {
        let _permit = network.acquire().await?;
        let response = request
            .send()
            .await
            .map_err(|error| UsageRequestFailure::request(request_context, &error))?;
        let status = response.status();
        let body = if status.is_success() {
            Some(response.json().await.map_err(|error| {
                usage_response_json_error(&format!("{body_context} (HTTP {status})"), &error)
            })?)
        } else {
            None
        };
        (status, body)
    };
    Ok(BufferedUsageResponse { status, body })
}

/// Internal variant that obtains a commit authorization before waiting for
/// each refresh request's network slot and carries that exact value to the
/// persistence step. Ordinary usage GETs therefore remain independent of
/// live-auth filesystem state, while no single-use refresh token can be spent
/// without a prepared conditional publication boundary and no scarce network
/// slot is occupied by the authorization or durable write.
#[allow(clippy::too_many_arguments)]
async fn fetch_usage_with_refresh_transactional<A, F, T>(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
    authorize_rotation: &mut A,
    persist_rotation: &mut F,
    reset_credit_enrichment: ResetCreditEnrichment,
    network: &mut NetworkPermitBudget,
) -> Result<UsageInfo>
where
    A: FnMut() -> Result<T>,
    F: FnMut(T, &str, RefreshTokenResolution) -> Result<RefreshedTokens>,
{
    let usage_url = endpoints.usage()?;
    let mut rejected_refresh: Option<anyhow::Error> = None;
    let mut retryable_proactive_refresh: Option<anyhow::Error> = None;

    // Usage authorization depends on the access token. An expired ID token is
    // not a reason to serialize a healthy read through the credential-write
    // boundary; it is replaced when the access token itself needs rotation.
    if let Some(rt) = refresh_token
        && access_token_needs_refresh(access_token, 60)?
    {
        info!("[{alias}] token expiring soon, proactively refreshing");

        let authorization = authorize_rotation()?;
        match do_refresh_token_with_network(endpoints, alias, client, id_token, rt, network).await {
            Ok(resolution) => {
                let new_tokens = persist_rotation(authorization, rt, resolution)?;
                let bearer = new_tokens.access_token.clone();

                let response = send_usage_request(
                    client,
                    usage_url,
                    &bearer,
                    account_id,
                    is_fedramp,
                    "Usage API request failed",
                    "failed to parse usage response",
                    network,
                )
                .await?;

                let status = response.status;
                debug!("[{alias}] Usage API (after proactive refresh): HTTP {status}");
                if status.is_success() {
                    let body = response
                        .body
                        .expect("successful usage response must carry its buffered body");
                    debug!(
                        "[{alias}] Usage API raw body (proactive): {}",
                        crate::auth::redact_sensitive_log_body(&body)
                    );
                    let usage = parse_checked_usage_response(&body)?;
                    return Ok(finish_usage_fetch(
                        endpoints,
                        client,
                        alias,
                        &bearer,
                        account_id,
                        is_fedramp,
                        reset_credit_enrichment,
                        network,
                        usage,
                    )
                    .await);
                }
                return Err(UsageRequestFailure::http(
                    status,
                    format!("Usage API failed (HTTP {status}) after proactive token refresh"),
                ));
            }
            Err(e) => {
                if e.downcast_ref::<RefreshOutcomeUnknown>().is_some() {
                    return Err(e);
                }
                if e.downcast_ref::<TerminalAuthError>().is_some() {
                    info!("[{alias}] proactive token refresh rejected permanently: {e:#}");
                    rejected_refresh = Some(e);
                } else if e
                    .downcast_ref::<UsageRequestFailure>()
                    .is_some_and(UsageRequestFailure::is_retryable)
                {
                    info!(
                        "[{alias}] proactive token refresh failed transiently, trying with existing token before a delayed retry: {e:#}"
                    );
                    retryable_proactive_refresh = Some(e);
                } else {
                    info!(
                        "[{alias}] proactive token refresh failed, trying with existing token: {e:#}"
                    );
                }
            }
        }
    }

    let response = send_usage_request(
        client,
        usage_url,
        access_token,
        account_id,
        is_fedramp,
        "Usage API request failed",
        "failed to parse usage response",
        network,
    )
    .await?;

    let status = response.status;
    debug!("[{alias}] Usage API: HTTP {status}");
    if status.is_success() {
        let body = response
            .body
            .expect("successful usage response must carry its buffered body");
        debug!(
            "[{alias}] Usage API raw body: {}",
            crate::auth::redact_sensitive_log_body(&body)
        );
        let usage = parse_checked_usage_response(&body)?;
        return Ok(finish_usage_fetch(
            endpoints,
            client,
            alias,
            access_token,
            account_id,
            is_fedramp,
            reset_credit_enrichment,
            network,
            usage,
        )
        .await);
    }

    // The auth server already rejected this refresh token moments ago; asking
    // again can only re-trigger reuse detection and add a round trip.
    if let Some(e) = rejected_refresh {
        return Err(e.context(format!("Usage API failed (HTTP {status})")));
    }

    // A proactive refresh that received an explicitly transient response did
    // not rotate the credential. If the old bearer is also rejected, return
    // that typed failure to the outer loop so its retry delay is observed;
    // immediately entering the reactive branch would submit the same refresh
    // token twice back-to-back (especially harmful after HTTP 429).
    if (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
        && let Some(error) = retryable_proactive_refresh
    {
        return Err(error.context(format!(
            "Usage API failed (HTTP {status}) after a transient proactive token refresh failure"
        )));
    }

    // If 401/403 and we have a refresh_token, try to refresh
    if let Some(rt) = refresh_token
        && (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
    {
        info!("[{alias}] got HTTP {status}, attempting token refresh");

        let authorization = authorize_rotation()?;
        match do_refresh_token_with_network(endpoints, alias, client, id_token, rt, network).await {
            Ok(resolution) => {
                let new_tokens = persist_rotation(authorization, rt, resolution)?;
                let bearer = new_tokens.access_token.clone();

                let response = send_usage_request(
                    client,
                    usage_url,
                    &bearer,
                    account_id,
                    is_fedramp,
                    "Usage API retry request failed",
                    "failed to parse usage response after refresh",
                    network,
                )
                .await?;

                let status2 = response.status;
                debug!("[{alias}] Usage API (after token refresh): HTTP {status2}");
                if status2.is_success() {
                    let body = response
                        .body
                        .expect("successful usage response must carry its buffered body");
                    let usage = parse_checked_usage_response(&body)?;
                    return Ok(finish_usage_fetch(
                        endpoints,
                        client,
                        alias,
                        &bearer,
                        account_id,
                        is_fedramp,
                        reset_credit_enrichment,
                        network,
                        usage,
                    )
                    .await);
                }
                return Err(UsageRequestFailure::http(
                    status2,
                    format!("Usage API still failed (HTTP {status2}) after token refresh"),
                ));
            }
            Err(e) => {
                info!("[{alias}] token refresh failed: {e:#}");
                // `.context` (not `bail!`) so the typed terminal-auth error
                // stays downcastable by the retry loop.
                return Err(e.context(format!(
                    "Usage API failed (HTTP {status}), token refresh also failed"
                )));
            }
        }
    }

    Err(UsageRequestFailure::http(
        status,
        format!("Usage API failed (HTTP {status}), no refresh_token available"),
    ))
}

/// Validate an auth.json being imported, refreshing its credentials if needed.
///
/// `persist_rotation` must durably write the updated auth value. It runs for
/// every successful refresh before any follow-up usage request is sent; an
/// error stops validation immediately rather than leaving a new single-use
/// token only in memory. See [`ImportValidation`].
pub async fn validate_import_auth<F>(
    val: &mut serde_json::Value,
    persist_rotation: F,
) -> ImportValidation
where
    F: FnMut(&serde_json::Value) -> Result<()>,
{
    let client = match auth::build_http_client() {
        Ok(client) => client,
        Err(error) => {
            return ImportValidation {
                refreshed: None,
                validated_account_id: None,
                result: Err(error.context("building import-validation HTTP client")),
            };
        }
    };
    validate_import_auth_with_client(val, persist_rotation, &client).await
}

/// As [`validate_import_auth`], reusing a caller-owned HTTP client for token,
/// usage, reset-credit, and workspace requests.
pub async fn validate_import_auth_with_client<F>(
    val: &mut serde_json::Value,
    mut persist_rotation: F,
    client: &reqwest::Client,
) -> ImportValidation
where
    F: FnMut(&serde_json::Value) -> Result<()>,
{
    let mut refreshed = None;
    let mut validated_account_id = None;
    let result = match auth::service_endpoints() {
        Ok(endpoints) => validate_import_auth_capturing_refresh(
            &endpoints,
            client,
            val,
            &mut refreshed,
            &mut persist_rotation,
        )
        .await
        .map(|(usage, account_id)| {
            validated_account_id = Some(account_id);
            usage
        }),
        Err(error) => Err(error),
    };
    ImportValidation {
        refreshed,
        validated_account_id,
        result,
    }
}

/// Record a rotation, write it into the auth value being validated, and persist
/// that value before the caller can make another network request.
///
/// `refreshed` is assigned *before* the fallible write so that a failure to
/// update the value still leaves the caller holding the live credentials.
fn adopt_refreshed_tokens<F>(
    val: &mut serde_json::Value,
    tokens: RefreshedTokens,
    refreshed: &mut Option<RefreshedTokens>,
    persist_rotation: &mut F,
) -> Result<()>
where
    F: FnMut(&serde_json::Value) -> Result<()>,
{
    let tokens = refreshed.insert(tokens);
    auth::apply_tokens(
        val,
        &tokens.id_token,
        &tokens.access_token,
        &tokens.refresh_token,
    )?;
    persist_rotation(val).context("persisting rotated import credentials")
}

/// Inner body of [`validate_import_auth`]. Every rotation reaches both
/// `refreshed` and durable storage before a follow-up usage request.
async fn validate_import_auth_capturing_refresh<F>(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    val: &mut serde_json::Value,
    refreshed: &mut Option<RefreshedTokens>,
    persist_rotation: &mut F,
) -> Result<(UsageInfo, String)>
where
    F: FnMut(&serde_json::Value) -> Result<()>,
{
    let (access_token, refresh_token) = auth::extract_tokens(val);
    let id_token = auth::extract_id_token(val);
    let account_info = crate::jwt::parse_account_info(val);
    let account_id = account_info.account_id;
    let is_fedramp = account_info.is_fedramp;

    let alias = "import";
    match (access_token, refresh_token) {
        (Some(at), rt) => {
            let validated_account_id = account_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("imported auth must contain an account_id"))?;
            let usage = {
                let mut persist_before_follow_up =
                    |_: &str, tokens: RefreshedTokens| -> Result<()> {
                        adopt_refreshed_tokens(val, tokens, refreshed, persist_rotation)
                    };
                fetch_usage_with_refresh_at(
                    endpoints,
                    client,
                    alias,
                    &at,
                    id_token.as_deref(),
                    rt.as_deref(),
                    Some(&validated_account_id),
                    is_fedramp,
                    &mut persist_before_follow_up,
                )
                .await?
            };
            Ok((usage, validated_account_id))
        }
        (None, Some(rt)) => {
            let first_resolution =
                do_refresh_token(endpoints, alias, client, id_token.as_deref(), &rt).await?;
            let mut defer_persistence = |_: &str, _: RefreshedTokens| Ok(());
            let first = persist_unbound_refresh_resolution(
                alias,
                &rt,
                first_resolution,
                &mut defer_persistence,
            )?;
            let (access_token, id_token, refresh_token) = (
                first.access_token.clone(),
                first.id_token.clone(),
                first.refresh_token.clone(),
            );
            adopt_refreshed_tokens(val, first, refreshed, persist_rotation)?;

            let validated_account_id = crate::jwt::parse_account_info(val)
                .account_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("refreshed auth must contain an account_id"))?;
            let usage = {
                let mut persist_before_follow_up =
                    |_: &str, tokens: RefreshedTokens| -> Result<()> {
                        adopt_refreshed_tokens(val, tokens, refreshed, persist_rotation)
                    };
                fetch_usage_with_refresh_at(
                    endpoints,
                    client,
                    alias,
                    &access_token,
                    Some(&id_token),
                    Some(&refresh_token),
                    Some(&validated_account_id),
                    is_fedramp,
                    &mut persist_before_follow_up,
                )
                .await?
            };
            Ok((usage, validated_account_id))
        }
        (None, None) => anyhow::bail!("auth.json missing access_token and refresh_token"),
    }
}

/// Build the token refresh request. Codex sends a JSON body
/// ({client_id, grant_type, refresh_token}) — keep the same shape so the
/// auth server sees requests identical to the real client's.
pub(crate) fn build_refresh_request(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> reqwest::RequestBuilder {
    client.post(token_url).json(&serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    }))
}

pub(crate) async fn do_refresh_token(
    endpoints: &auth::ServiceEndpoints,
    alias: &str,
    client: &reqwest::Client,
    current_id_token: Option<&str>,
    refresh_token: &str,
) -> Result<RefreshTokenResolution> {
    let mut network = NetworkPermitBudget::unlimited();
    do_refresh_token_with_network(
        endpoints,
        alias,
        client,
        current_id_token,
        refresh_token,
        &mut network,
    )
    .await
}

pub(crate) async fn do_refresh_token_with_network(
    endpoints: &auth::ServiceEndpoints,
    alias: &str,
    client: &reqwest::Client,
    current_id_token: Option<&str>,
    refresh_token: &str,
    network: &mut NetworkPermitBudget,
) -> Result<RefreshTokenResolution> {
    let token_url = endpoints.token()?;
    debug!("[{alias}] sending token refresh request to {token_url}");

    let request = build_refresh_request(client, token_url, refresh_token);
    let (status, body_text) = {
        let _permit = network.acquire().await?;
        let resp = request.send().await.map_err(|error| {
            let detail = format_reqwest_error("token refresh request failed", &error).to_string();
            refresh_outcome_unknown(anyhow::Error::new(error).context(format!(
                "token refresh request transport failed after submission began: {detail}"
            )))
        })?;

        let status = resp.status();

        // Read raw body before returning the permit: a truncated response can
        // leave a single-use refresh-token outcome unknowable.
        let body_text = resp.text().await.map_err(|error| {
            refresh_outcome_unknown(anyhow::Error::new(error).context(format!(
                "failed to read token refresh response body (HTTP {status})"
            )))
        })?;
        (status, body_text)
    };
    debug!("[{alias}] token refresh response: HTTP {status}");

    let r: RefreshResponse = serde_json::from_str(&body_text).map_err(|error| {
        // A token refresh body may contain access/refresh/id tokens; redact them
        // before logging so `--debug` output is safe to share in bug reports.
        let redacted = serde_json::from_str::<Value>(&body_text)
            .map(|v| crate::auth::redact_sensitive_log_body(&v))
            .unwrap_or_else(|_| format!("<non-JSON body, {} bytes>", body_text.len()));
        debug!("[{alias}] token refresh parse failure, raw body: {redacted}");
        refresh_outcome_unknown(anyhow::Error::new(error).context(format!(
            "failed to parse token refresh response (HTTP {status})"
        )))
    })?;

    let resolution = resolve_refreshed_tokens(r, status, current_id_token, refresh_token)
        .with_context(|| format!("[{alias}] token refresh HTTP {status}"))?;
    match &resolution {
        RefreshTokenResolution::Validated(_) => info!("[{alias}] token refresh succeeded"),
        RefreshTokenResolution::RotatedButInvalid { .. } => warn!(
            "[{alias}] token refresh returned a successor refresh token in an invalid response; preserving it for recovery"
        ),
    }
    Ok(resolution)
}

/// Max number of tokens to refresh opportunistically per CLI invocation.
const OPPORTUNISTIC_REFRESH_LIMIT: usize = 3;
/// Refresh tokens expiring within this many seconds.
const OPPORTUNISTIC_REFRESH_MARGIN: i64 = 1800; // 30 minutes
/// How many rotations may be in flight at once. Each in-flight request holds a
/// credential that only exists in its own response, so this also bounds how
/// much can be lost if the process dies mid-batch.
const OPPORTUNISTIC_REFRESH_CONCURRENCY: usize = 2;
/// Wall-clock budget for *starting* opportunistic refreshes. It never cancels
/// one — see [`refresh_expiring_tokens_within`].
const OPPORTUNISTIC_START_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

fn opportunistic_worker_join_error(alias: &str, error: &tokio::task::JoinError) -> UsageError {
    let outcome = crate::task_batch::join_failure_outcome(error);
    UsageError {
        summary: "token refresh outcome unknown".to_string(),
        detail: format!(
            "[{alias}] the opportunistic token-refresh worker {outcome} before its result could be \
             collected. It may already have sent the single-use refresh token to the auth server, \
             so credential rotation and local persistence cannot be confirmed. Inspect this \
             profile before retrying and sign in again if its credential no longer works."
        ),
    }
}

fn record_opportunistic_worker_result(
    joined: std::result::Result<(tokio::task::Id, Option<UsageError>), tokio::task::JoinError>,
    task_aliases: &mut HashMap<tokio::task::Id, String>,
    failures: &mut Vec<TokenPersistFailure>,
) {
    let task_id = match &joined {
        Ok((task_id, _)) => *task_id,
        Err(error) => error.id(),
    };
    let alias = task_aliases
        .remove(&task_id)
        .expect("every opportunistic refresh task is registered before it can be joined");
    let error = match joined {
        Ok((_, error)) => error,
        Err(error) => Some(opportunistic_worker_join_error(&alias, &error)),
    };
    if let Some(error) = error {
        failures.push(TokenPersistFailure { alias, error });
    }
}

fn profile_still_holds_refresh_token(profile_path: &Path, presented: &str) -> bool {
    auth::read_auth(profile_path)
        .ok()
        .and_then(|value| auth::extract_tokens(&value).1)
        .as_deref()
        == Some(presented)
}

fn opportunistic_start_budget_remaining(deadline: tokio::time::Instant) -> bool {
    tokio::time::Instant::now() < deadline
}

fn opportunistic_refresh_deadline(budget: std::time::Duration) -> Result<tokio::time::Instant> {
    tokio::time::Instant::now()
        .checked_add(budget)
        .context("opportunistic refresh budget exceeds the runtime timer range")
}

#[cfg(test)]
type BeforeOpportunisticRequestHook = Box<dyn FnOnce(tokio::time::Instant) + Send>;

#[cfg(test)]
fn before_opportunistic_request_hook()
-> &'static std::sync::Mutex<Option<BeforeOpportunisticRequestHook>> {
    static HOOK: std::sync::OnceLock<std::sync::Mutex<Option<BeforeOpportunisticRequestHook>>> =
        std::sync::OnceLock::new();
    HOOK.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn set_before_opportunistic_request_hook(hook: impl FnOnce(tokio::time::Instant) + Send + 'static) {
    *before_opportunistic_request_hook().lock().unwrap() = Some(Box::new(hook));
}

#[cfg(test)]
fn run_before_opportunistic_request_hook(deadline: tokio::time::Instant) {
    let hook = before_opportunistic_request_hook().lock().unwrap().take();
    if let Some(hook) = hook {
        hook(deadline);
    }
}

/// Opportunistically refresh tokens that are about to expire.
///
/// Refresh *failures* are logged, not propagated. A memorable terminal
/// rejection is cached against the presented credential so the next background
/// pass does not replay it. Failures to **save** a rotated token are returned
/// instead: the old credential is already dead server-side, so a lost write
/// silently bricks that profile and the caller has to tell someone. A worker
/// panic or cancellation is returned through the same channel because its
/// server-side rotation and persistence outcome cannot be reconstructed.
pub async fn refresh_expiring_tokens() -> Result<Vec<TokenPersistFailure>> {
    refresh_expiring_tokens_within(OPPORTUNISTIC_START_BUDGET).await
}

/// As [`refresh_expiring_tokens`], reusing a caller-owned HTTP client across
/// every profile selected for the opportunistic batch.
pub async fn refresh_expiring_tokens_with_client(
    client: &reqwest::Client,
) -> Result<Vec<TokenPersistFailure>> {
    refresh_expiring_tokens_within_with_client(OPPORTUNISTIC_START_BUDGET, client).await
}

/// As [`refresh_expiring_tokens`], with an explicit start budget.
///
/// `budget` bounds how long this may wait for a profile lease and **open** new
/// rotations; it is never a deadline for the ones already open. `refresh_token` is single-use: as soon as
/// a request reaches the auth server the presented token is dead and its
/// replacement exists only in that one response. Abandoning the request — which
/// is what a `timeout` around the join loop does, since `JoinSet::drop` aborts
/// every unfinished task — would therefore leave the profile holding a
/// credential nothing will ever accept again. So every started refresh is
/// awaited to completion, and the budget only decides whether the *next*
/// candidate is contacted at all. A candidate that is never contacted loses
/// nothing: it keeps its working token for the next invocation.
///
/// Residual window we cannot close: the HTTP client in `auth::build_http_client`
/// carries its own total timeout, and if that fires the server may already have
/// rotated the credential while we never read the answer. Nothing on this side
/// can prevent that — the loss is decided by whether the request reached the
/// server, not by how long we wait. Shortening either timeout only *widens* the
/// window (more rotations cut off mid-flight), so neither is tuned for latency.
///
/// Worst-case wall clock for a synchronous caller (`list`, `best`) is therefore
/// HTTP client construction + `budget` + one HTTP client timeout. Client
/// construction is deliberately outside the start budget; a refresh started
/// just before the budget expired may still hang for the client's full timeout.
pub async fn refresh_expiring_tokens_within(
    budget: std::time::Duration,
) -> Result<Vec<TokenPersistFailure>> {
    let candidates = opportunistic_refresh_candidates()?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let endpoints = auth::service_endpoints()?;
    // Build before starting the budget: client construction can synchronously
    // initialize TLS state, but the budget is only for opening rotations.
    let client = auth::build_http_client().context("building opportunistic refresh client")?;
    run_opportunistic_refresh_batch(candidates, budget, endpoints, &client).await
}

/// As [`refresh_expiring_tokens_within`], reusing a caller-owned HTTP client.
pub async fn refresh_expiring_tokens_within_with_client(
    budget: std::time::Duration,
    client: &reqwest::Client,
) -> Result<Vec<TokenPersistFailure>> {
    let candidates = opportunistic_refresh_candidates()?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let endpoints = auth::service_endpoints()?;
    run_opportunistic_refresh_batch(candidates, budget, endpoints, client).await
}

type OpportunisticRefreshCandidate = (String, std::path::PathBuf, String, i64);

fn opportunistic_token_expiry(access_token: &str, id_token: Option<&str>) -> Option<i64> {
    [
        crate::jwt::token_expires_at(access_token),
        id_token.and_then(crate::jwt::token_expires_at),
    ]
    .into_iter()
    .flatten()
    .min()
}

fn opportunistic_refresh_candidates() -> Result<Vec<OpportunisticRefreshCandidate>> {
    let profiles = crate::profile::list_profiles().context("listing profiles for token refresh")?;

    let now = auth::now_unix_secs()?;

    // Collect current tokens for profiles expiring soon.
    let mut candidates = Vec::new();
    for alias in &profiles {
        let path = match crate::profile::profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let val = match auth::read_auth(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (access_token, refresh_token) = auth::extract_tokens(&val);
        let id_token = auth::extract_id_token(&val);
        let Some(at) = access_token else { continue };
        let Some(rt) = refresh_token else { continue };
        let expiry = opportunistic_token_expiry(&at, id_token.as_deref());
        let Some(exp) = expiry else {
            continue;
        };
        let Some(remaining) = exp.checked_sub(now) else {
            continue;
        };
        if remaining < OPPORTUNISTIC_REFRESH_MARGIN {
            candidates.push((alias.clone(), path, rt, exp));
        }
    }

    // Expiry alone says nothing about whether the credential can still be
    // rotated. Read every standing refusal from one immutable cache snapshot;
    // otherwise one contended lock can impose the full timeout per alias.
    let credentials = candidates
        .iter()
        .map(|(alias, _, refresh_token, _)| (alias.clone(), refresh_token.clone()))
        .collect::<HashMap<_, _>>();
    let rejected = crate::cache::get_auth_failures(&credentials)
        .context("reading auth-failure cache for token refresh candidates")?;
    candidates.retain(|(alias, _, _, _)| {
        let rejected = rejected.contains_key(alias);
        if rejected {
            debug!("[{alias}] skipping opportunistic refresh: credential already rejected");
        }
        !rejected
    });

    // Sort by expiration: soonest first
    candidates.sort_by_key(|c| c.3);
    candidates.truncate(OPPORTUNISTIC_REFRESH_LIMIT);

    let count = candidates.len();
    debug!(
        "opportunistic refresh: {count} token(s) expiring within {}s",
        OPPORTUNISTIC_REFRESH_MARGIN
    );

    Ok(candidates)
}

async fn run_opportunistic_refresh_batch(
    candidates: Vec<OpportunisticRefreshCandidate>,
    budget: std::time::Duration,
    endpoints: auth::ServiceEndpoints,
    client: &reqwest::Client,
) -> Result<Vec<TokenPersistFailure>> {
    // Start refreshes while the budget lasts, then wait for every started one:
    // an in-flight rotation is not cancellable without losing the credential.
    let deadline = opportunistic_refresh_deadline(budget)?;
    let mut queued = candidates.into_iter();
    let mut tasks: tokio::task::JoinSet<Option<UsageError>> = tokio::task::JoinSet::new();
    let mut task_aliases = HashMap::new();
    let mut failures = Vec::new();

    loop {
        while tasks.len() < OPPORTUNISTIC_REFRESH_CONCURRENCY
            && tokio::time::Instant::now() < deadline
        {
            let Some((alias, path, rt, _discovered_expiry)) = queued.next() else {
                break;
            };
            let tracked_alias = alias.clone();
            let client = client.clone();
            let endpoints = endpoints.clone();
            let task = tasks.spawn(async move {
                let lease_control = crate::profile::ProfileLeaseAcquireControl::new();
                let lease = match tokio::time::timeout_at(
                    deadline,
                    crate::profile::acquire_profile_lease_async_cancellable(
                        alias.clone(),
                        &lease_control,
                    ),
                )
                .await
                {
                    Ok(Ok(Some(lease))) => lease,
                    Ok(Ok(None)) => return None,
                    Ok(Err(error)) => {
                        debug!("[{alias}] opportunistic refresh could not lock profile: {error:#}");
                        return None;
                    }
                    Err(_) => {
                        lease_control.cancel_waiting();
                        debug!("[{alias}] opportunistic refresh skipped: lease wait spent budget");
                        return None;
                    }
                };
                // Acquiring the lease can race the timer at the exact boundary.
                // Recheck while holding it so an expired batch cannot start a
                // fresh HTTP rotation.
                if tokio::time::Instant::now() >= deadline {
                    debug!("[{alias}] opportunistic refresh skipped: budget expired after lease");
                    return None;
                }
                // Candidate discovery is intentionally lock-free. Re-read after
                // acquiring the lease so no credential observed before the
                // lease can be sent to the auth server after another operation
                // rotated it.
                let value = match auth::read_auth(&path) {
                    Ok(value) => value,
                    Err(error) => {
                        debug!("[{alias}] opportunistic refresh could not reload auth: {error:#}");
                        return None;
                    }
                };
                let (current_access_token, current_refresh_token) = auth::extract_tokens(&value);
                let current_access_token = current_access_token?;
                let current_refresh_token = current_refresh_token?;
                if current_refresh_token != rt {
                    debug!(
                        "[{alias}] opportunistic refresh skipped: credential changed before lease"
                    );
                    return None;
                }
                let id_token = auth::extract_id_token(&value);
                let Some(current_expiry) =
                    opportunistic_token_expiry(&current_access_token, id_token.as_deref())
                else {
                    debug!(
                        "[{alias}] opportunistic refresh skipped: current tokens have no expiry"
                    );
                    return None;
                };
                let remaining = match auth::now_unix_secs()
                    .and_then(|now| {
                        current_expiry.checked_sub(now)
                            .context("token expiration distance exceeds the signed time range")
                    }) {
                    Ok(remaining) => remaining,
                    Err(error) => {
                        return Some(UsageError {
                            summary: "system clock unavailable".to_string(),
                            detail: format!(
                                "[{alias}] cannot safely time token refresh: {error:#}"
                            ),
                        });
                    }
                };
                if remaining >= OPPORTUNISTIC_REFRESH_MARGIN {
                    debug!(
                        "[{alias}] opportunistic refresh skipped: current tokens expire in {remaining}s"
                    );
                    return None;
                }
                debug!("[{alias}] token expires in {remaining}s, refreshing");

                // File parsing and task scheduling can spend the remainder of
                // the budget after the lease-level check. This is the final
                // boundary before an irreversible refresh-token rotation.
                #[cfg(test)]
                run_before_opportunistic_request_hook(deadline);
                let activation_authorization =
                    match crate::profile::authorize_fresh_credentials_activation(&lease) {
                        Ok(authorization) => authorization,
                        Err(error) => {
                            return Some(UsageError::refresh_authorization_failed(&alias, &error));
                        }
                    };
                if !opportunistic_start_budget_remaining(deadline) {
                    debug!(
                        "[{alias}] opportunistic refresh skipped: budget expired before request"
                    );
                    return None;
                }
                match do_refresh_token(
                    &endpoints,
                    &alias,
                    &client,
                    id_token.as_deref(),
                    &rt,
                )
                .await
                {
                    Ok(resolution) => match persist_refresh_resolution(
                        &lease,
                        activation_authorization,
                        &rt,
                        resolution,
                    ) {
                        Ok(_) => {
                            info!("[{alias}] opportunistic token refresh succeeded");
                            None
                        }
                        // Report rather than abort: the remaining profiles still
                        // deserve their refresh, and this one is only recoverable
                        // once a human hears about it.
                        Err(error) => Some(error),
                    },
                    Err(e) => {
                        let detail = format!("{e:#}");
                        if e.downcast_ref::<RefreshOutcomeUnknown>().is_some() {
                            return Some(UsageError::refresh_outcome_unknown(&alias, &e));
                        }
                        if let Some(terminal) = e.downcast_ref::<TerminalAuthError>() {
                            let error = UsageError {
                                summary: terminal.summary(),
                                detail: detail.clone(),
                            };
                            if profile_still_holds_refresh_token(&path, &rt) {
                                remember_terminal_verdict(
                                    &alias,
                                    &terminal.code,
                                    Some(&rt),
                                    &error,
                                )
                                .await;
                            } else {
                                debug!(
                                    "[{alias}] not caching terminal verdict for a superseded credential"
                                );
                            }
                        }
                        debug!("[{alias}] opportunistic token refresh failed: {detail}");
                        None
                    }
                }
            });
            let previous = task_aliases.insert(task.id(), tracked_alias);
            debug_assert!(previous.is_none(), "JoinSet task IDs must be unique");
        }

        // No timeout here on purpose: this awaits requests the auth server has
        // already been told about.
        let Some(joined) = tasks.join_next_with_id().await else {
            break;
        };
        record_opportunistic_worker_result(joined, &mut task_aliases, &mut failures);
    }

    debug_assert!(task_aliases.is_empty());

    Ok(failures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn excessive_opportunistic_refresh_budget_is_rejected() {
        let error = opportunistic_refresh_deadline(std::time::Duration::MAX)
            .expect_err("an unrepresentable timer budget must not panic or be accepted");
        assert!(
            error
                .to_string()
                .contains("budget exceeds the runtime timer range"),
            "{error:#}"
        );
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: String) -> Self {
            let previous = std::env::var(key).ok();
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

    fn write_auth_durable(path: &Path, value: &serde_json::Value) {
        crate::auth::write_auth(path, value)
            .unwrap()
            .assert_durably_published();
    }

    fn jwt_with_exp(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({"exp": exp}).to_string());
        format!("header.{payload}.signature")
    }

    fn jwt_with_exp_and_identity(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "exp": exp,
                "email": "budget-test@example.com",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-budget-test"
                }
            })
            .to_string(),
        );
        format!("header.{payload}.signature")
    }

    fn valid_usage_body() -> Value {
        json!({
            "plan_type": "pro",
            "rate_limit": null,
            "credits": null,
            "spend_control": null,
            "additional_rate_limits": null,
            "rate_limit_reached_type": null
        })
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn deterministic_usage_400_stops_after_one_request() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/usage",
            get(move || {
                let calls = Arc::clone(&server_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::BAD_REQUEST
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));

        let now = auth::now_unix_secs().unwrap();
        let alias = "usage_400";
        let profile_path = crate::profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400)
                }
            }),
        );
        let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
            .await
            .unwrap();
        let error = probe_core_usage_unattended_with_existing_lease_and_client(
            alias,
            &profile_path,
            &lease,
            None,
            &reqwest::Client::new(),
        )
        .await
        .expect_err("a deterministic 400 must fail without an outer retry");
        server.abort();

        assert!(error.detail.contains("HTTP 400 Bad Request"), "{error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn transient_usage_http_statuses_are_retried() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());
        let now = auth::now_unix_secs().unwrap();

        for (case, status) in [
            ("timeout", StatusCode::REQUEST_TIMEOUT),
            ("rate_limit", StatusCode::TOO_MANY_REQUESTS),
            ("server_error", StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let server_calls = Arc::clone(&calls);
            let app = Router::new().route(
                "/usage",
                get(move || {
                    let calls = Arc::clone(&server_calls);
                    async move {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            status.into_response()
                        } else {
                            axum::Json(valid_usage_body()).into_response()
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));

            let alias = format!("usage_transient_{case}");
            let profile_path = crate::profile::profile_auth_path(&alias).unwrap();
            std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
            write_auth_durable(
                &profile_path,
                &json!({
                    "tokens": {
                        "id_token": jwt_with_exp_and_identity(now + 86_400),
                        "access_token": jwt_with_exp(now + 86_400)
                    }
                }),
            );
            let lease = crate::profile::acquire_profile_lease_async(alias.clone())
                .await
                .unwrap();
            let usage = probe_core_usage_unattended_with_existing_lease_and_client(
                &alias,
                &profile_path,
                &lease,
                None,
                &reqwest::Client::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{case} should retry: {error:?}"));
            server.abort();

            assert_eq!(usage.plan_type.as_deref(), Some("pro"), "{case}");
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{case}");
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn retry_backoff_releases_the_network_permit() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let (first_attempt_tx, mut first_attempt_rx) = tokio::sync::mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/usage",
            get(move || {
                let calls = Arc::clone(&server_calls);
                let first_attempt_tx = first_attempt_tx.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        first_attempt_tx.send(()).unwrap();
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        axum::Json(valid_usage_body()).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));

        let alias = "retry_permit_boundary";
        let now = auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400)
                }
            }),
        );
        let binding = auth::account_info_from_auth_value(&auth::read_auth(&profile_path).unwrap())
            .strict_binding()
            .unwrap();
        let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
            .await
            .unwrap();
        let prepared = prepare_core_usage_with_existing_lease(
            alias,
            &profile_path,
            Refresh::Forced,
            &lease,
            &binding,
        )
        .await
        .unwrap();
        let limiter = Arc::new(tokio::sync::Semaphore::new(1));
        let client = reqwest::Client::new();
        let mut network = NetworkPermitBudget::new(first_network_permit(limiter.clone()));
        let refresh = tokio::spawn(async move {
            execute_prepared_core_usage_with_existing_lease_and_client(
                prepared,
                &lease,
                &client,
                &mut network,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), first_attempt_rx.recv())
            .await
            .expect("first usage attempt did not reach the server")
            .expect("first-attempt channel closed");
        let recovered = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            limiter.clone().acquire_owned(),
        )
        .await
        .expect("retry backoff retained the only network permit")
        .unwrap();
        tokio::time::sleep(RETRY_DELAY + std::time::Duration::from_millis(100)).await;
        assert!(
            !refresh.is_finished(),
            "the retry must reserve fresh capacity before its second request"
        );
        drop(recovered);

        refresh.await.unwrap().unwrap();
        server.abort();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn first_admission_cancellation_is_consumed_once() {
        let polls = Arc::new(AtomicUsize::new(0));
        let first_polls = Arc::clone(&polls);
        let first: FirstNetworkPermit = Box::pin(async move {
            first_polls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        });
        let mut network = NetworkPermitBudget::new(first);

        let first_error = network.acquire().await.unwrap_err();
        assert!(network_wait_was_cancelled(&first_error));
        assert!(network.first_wait_was_cancelled());
        assert_eq!(polls.load(Ordering::SeqCst), 1);

        let second_error = network
            .acquire()
            .await
            .expect_err("a cancelled first admission must not be polled again");
        assert!(network_wait_was_cancelled(&second_error));
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_authorization_and_persistence_release_network_capacity() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        crate::config::init_defaults_for_tests();

        let (token_request_tx, mut token_request_rx) = tokio::sync::mpsc::unbounded_channel();
        let (usage_request_tx, mut usage_request_rx) = tokio::sync::mpsc::unbounded_channel();
        let now = auth::now_unix_secs().unwrap();
        let id_token = jwt_with_exp_and_identity(now + 86_400);
        let refreshed_id_token = id_token.clone();
        let refreshed_access_token = jwt_with_exp(now + 86_400);
        let token_response_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let server_token_response_gate = Arc::clone(&token_response_gate);
        let app = Router::new()
            .route(
                "/token",
                post(move || {
                    let requested = token_request_tx.clone();
                    let id_token = refreshed_id_token.clone();
                    let access_token = refreshed_access_token.clone();
                    let response_gate = Arc::clone(&server_token_response_gate);
                    async move {
                        requested.send(()).unwrap();
                        response_gate.acquire().await.unwrap().forget();
                        axum::Json(json!({
                            "id_token": id_token,
                            "access_token": access_token,
                            "refresh_token": "rotated-refresh"
                        }))
                    }
                }),
            )
            .route(
                "/usage",
                get(move || {
                    let requested = usage_request_tx.clone();
                    async move {
                        requested.send(()).unwrap();
                        axum::Json(valid_usage_body())
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

        let (authorization_started_tx, authorization_started_rx) = tokio::sync::oneshot::channel();
        let (authorization_release_tx, authorization_release_rx) = std::sync::mpsc::channel();
        let (persistence_started_tx, persistence_started_rx) = tokio::sync::oneshot::channel();
        let (persistence_release_tx, persistence_release_rx) = std::sync::mpsc::channel();
        let limiter = Arc::new(tokio::sync::Semaphore::new(1));
        let (first_wait_started_tx, mut first_wait_started_rx) = tokio::sync::oneshot::channel();
        let first_wait_limiter = limiter.clone();
        let first_permit: FirstNetworkPermit = Box::pin(async move {
            first_wait_started_tx.send(()).unwrap();
            first_wait_limiter
                .acquire_owned()
                .await
                .map(Some)
                .map_err(|_| NetworkLimiterClosed.into())
        });
        let endpoints = auth::service_endpoints().unwrap();
        let expired_access_token = jwt_with_exp(now - 1);
        let refresh_id_token = id_token.clone();
        let refresh = tokio::spawn(async move {
            let mut network = NetworkPermitBudget::new(first_permit);
            let mut authorization_started_tx = Some(authorization_started_tx);
            let mut authorize_rotation = move || {
                authorization_started_tx
                    .take()
                    .expect("authorization runs once")
                    .send(())
                    .unwrap();
                authorization_release_rx.recv().unwrap();
                Ok(())
            };
            let mut persistence_started_tx = Some(persistence_started_tx);
            let mut persist_rotation =
                move |(): (), _: &str, resolution: RefreshTokenResolution| {
                    persistence_started_tx
                        .take()
                        .expect("persistence runs once")
                        .send(())
                        .unwrap();
                    persistence_release_rx.recv().unwrap();
                    match resolution {
                        RefreshTokenResolution::Validated(tokens) => Ok(tokens),
                        RefreshTokenResolution::RotatedButInvalid { .. } => {
                            panic!("the fixture must return valid refreshed credentials")
                        }
                    }
                };
            fetch_usage_with_refresh_transactional(
                &endpoints,
                &reqwest::Client::new(),
                "permit-boundary",
                &expired_access_token,
                Some(&refresh_id_token),
                Some("initial-refresh"),
                None,
                false,
                &mut authorize_rotation,
                &mut persist_rotation,
                ResetCreditEnrichment::Deferred,
                &mut network,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), authorization_started_rx)
            .await
            .expect("refresh authorization did not start")
            .unwrap();
        assert!(matches!(
            first_wait_started_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let authorization_capacity = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            limiter.clone().acquire_owned(),
        )
        .await
        .expect("blocked authorization retained the only network permit")
        .unwrap();
        authorization_release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), first_wait_started_rx)
            .await
            .expect("first network admission was not polled after authorization")
            .unwrap();
        let overtaker = tokio::spawn(limiter.clone().acquire_owned());
        drop(authorization_capacity);
        tokio::time::timeout(std::time::Duration::from_secs(1), token_request_rx.recv())
            .await
            .expect("the first admission permit was returned and reacquired behind another waiter")
            .expect("token-request channel closed");
        assert!(
            !overtaker.is_finished(),
            "another waiter acquired capacity while the first HTTP exchange was reading its body"
        );
        overtaker.abort();
        let _ = overtaker.await;
        token_response_gate.add_permits(1);

        tokio::time::timeout(std::time::Duration::from_secs(1), persistence_started_rx)
            .await
            .expect("credential persistence did not start")
            .unwrap();
        let persistence_capacity = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            limiter.clone().acquire_owned(),
        )
        .await
        .expect("blocked persistence retained the only network permit")
        .unwrap();
        persistence_release_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                usage_request_rx.recv(),
            )
            .await
            .is_err(),
            "the follow-up usage request started without reacquiring network capacity"
        );
        drop(persistence_capacity);
        tokio::time::timeout(std::time::Duration::from_secs(1), usage_request_rx.recv())
            .await
            .expect("usage request did not start after capacity was returned")
            .expect("usage-request channel closed");

        let usage = refresh.await.unwrap().unwrap();
        server.abort();
        assert_eq!(usage.plan_type.as_deref(), Some("pro"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn transient_proactive_refresh_is_not_replayed_before_outer_retry() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        crate::config::init_defaults_for_tests();

        let now = auth::now_unix_secs().unwrap();
        let access_token = jwt_with_exp(now + 30);
        let id_token = jwt_with_exp_and_identity(now + 86_400);

        for (case, transient_status) in [
            ("timeout", StatusCode::REQUEST_TIMEOUT),
            ("rate_limit", StatusCode::TOO_MANY_REQUESTS),
            ("server_error", StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let usage_calls = Arc::new(AtomicUsize::new(0));
            let token_calls = Arc::new(AtomicUsize::new(0));
            let usage_server_calls = Arc::clone(&usage_calls);
            let token_server_calls = Arc::clone(&token_calls);
            let app = Router::new()
                .route(
                    "/usage",
                    get(move || {
                        let calls = Arc::clone(&usage_server_calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            StatusCode::UNAUTHORIZED
                        }
                    }),
                )
                .route(
                    "/token",
                    post(move || {
                        let calls = Arc::clone(&token_server_calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            (
                                transient_status,
                                axum::Json(json!({"error": "temporarily_unavailable"})),
                            )
                                .into_response()
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
            let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

            let endpoints = auth::service_endpoints().unwrap();
            let mut network = NetworkPermitBudget::unlimited();
            let mut authorize_rotation = || Ok(());
            let mut persist_rotation =
                |(): (), _: &str, _: RefreshTokenResolution| -> Result<RefreshedTokens> {
                    panic!("a failed refresh must never reach persistence")
                };
            let error = fetch_usage_with_refresh_transactional(
                &endpoints,
                &reqwest::Client::new(),
                case,
                &access_token,
                Some(&id_token),
                Some("same-refresh"),
                None,
                false,
                &mut authorize_rotation,
                &mut persist_rotation,
                ResetCreditEnrichment::Deferred,
                &mut network,
            )
            .await
            .expect_err("the typed transient refresh failure must reach the outer retry loop");
            server.abort();

            let request_failure = error
                .downcast_ref::<UsageRequestFailure>()
                .unwrap_or_else(|| panic!("{case}: typed request failure was lost: {error:#}"));
            assert!(request_failure.is_retryable(), "{case}: {error:#}");
            assert_eq!(usage_calls.load(Ordering::SeqCst), 1, "{case}");
            assert_eq!(
                token_calls.load(Ordering::SeqCst),
                1,
                "{case}: the same refresh token was replayed before the outer retry delay"
            );
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn supplied_client_is_shared_by_usage_and_reset_credit_requests() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        crate::config::init_defaults_for_tests();

        let marker_hits = Arc::new(AtomicUsize::new(0));
        let usage_hits = Arc::clone(&marker_hits);
        let credits_hits = Arc::clone(&marker_hits);
        let app = Router::new()
            .route(
                "/usage",
                get(move |headers: axum::http::HeaderMap| {
                    let hits = Arc::clone(&usage_hits);
                    async move {
                        if headers.get("x-client-marker").and_then(|v| v.to_str().ok())
                            == Some("shared")
                        {
                            hits.fetch_add(1, Ordering::SeqCst);
                        }
                        axum::Json(json!({
                            "plan_type": "pro",
                            "rate_limit": null,
                            "credits": null,
                            "spend_control": null,
                            "additional_rate_limits": null,
                            "rate_limit_reached_type": null
                        }))
                    }
                }),
            )
            .route(
                "/credits",
                get(move |headers: axum::http::HeaderMap| {
                    let hits = Arc::clone(&credits_hits);
                    async move {
                        if headers.get("x-client-marker").and_then(|v| v.to_str().ok())
                            == Some("shared")
                        {
                            hits.fetch_add(1, Ordering::SeqCst);
                        }
                        axum::Json(json!({
                            "available_count": 1,
                            "credits": [{"id": "credit-1", "status": "available"}]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-client-marker", "shared".parse().unwrap());
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .unwrap();
        let mut persist = |_: &str, _: RefreshedTokens| Ok(());
        let usage = fetch_usage_with_refresh_at(
            &auth::service_endpoints().unwrap(),
            &client,
            "alice",
            "access-token",
            None,
            None,
            Some("account-1"),
            false,
            &mut persist,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(marker_hits.load(Ordering::SeqCst), 2);
        assert_eq!(usage.reset_credits_available_count, Some(1));
        assert_eq!(usage.reset_credits.len(), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn full_usage_cache_publication_does_not_hold_the_network_permit() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let (credits_finished_tx, mut credits_finished_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route("/usage", get(|| async { axum::Json(valid_usage_body()) }))
            .route(
                "/credits",
                get(move || {
                    let finished = credits_finished_tx.clone();
                    async move {
                        let response = axum::Json(json!({
                            "available_count": 1,
                            "credits": [{"id": "credit-1", "status": "available"}]
                        }));
                        finished.send(()).unwrap();
                        response
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let alias = "cache_boundary";
        let now = crate::auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400)
                }
            }),
        );
        let binding = crate::auth::account_info_from_auth_value(
            &crate::auth::read_auth(&profile_path).unwrap(),
        )
        .strict_binding()
        .unwrap();
        let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
            .await
            .unwrap();
        let prepared = prepare_full_usage_with_existing_lease(
            alias,
            &profile_path,
            Refresh::Forced,
            &lease,
            Some(&binding),
        )
        .await
        .unwrap();

        let cache_lock_holder = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock_holder).unwrap();

        let limiter = Arc::new(tokio::sync::Semaphore::new(1));
        let client = reqwest::Client::new();
        let mut network = NetworkPermitBudget::new(first_network_permit(limiter.clone()));
        let refresh = tokio::spawn(async move {
            execute_prepared_full_usage_with_existing_lease_and_client(
                prepared,
                &lease,
                &client,
                &mut network,
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            credits_finished_rx.recv(),
        )
        .await
        .expect("full usage refresh did not finish its network phase")
        .expect("reset-credit completion channel closed");

        let recovered_permit = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            limiter.clone().acquire_owned(),
        )
        .await
        .expect("cache publication retained the only network permit")
        .unwrap();
        assert!(
            !refresh.is_finished(),
            "the cache lock should still be holding the refresh after its permit was released"
        );
        drop(recovered_permit);
        fs4::FileExt::unlock(&cache_lock_holder).unwrap();

        refresh.await.unwrap().unwrap();
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn quota_first_api_returns_before_blocked_reset_credit_enrichment() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let (credits_started_tx, mut credits_started_rx) = tokio::sync::mpsc::unbounded_channel();
        let credits_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let handler_gate = Arc::clone(&credits_gate);
        let app = Router::new()
            .route(
                "/usage",
                get(|| async {
                    axum::Json(json!({
                        "plan_type": "pro",
                        "rate_limit": {
                            "primary_window": {
                                "used_percent": 12.0,
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
                get(move || {
                    let started = credits_started_tx.clone();
                    let gate = Arc::clone(&handler_gate);
                    async move {
                        started.send(()).unwrap();
                        let permit = gate.acquire_owned().await.unwrap();
                        permit.forget();
                        axum::Json(json!({
                            "available_count": 1,
                            "credits": [{"id": "credit-1", "status": "available"}]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let now = crate::auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path("quota_first").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400)
                }
            }),
        );
        let lease = crate::profile::acquire_profile_lease_async("quota_first".to_string())
            .await
            .unwrap();
        let binding = auth::account_info_from_auth_value(&auth::read_auth(&profile_path).unwrap())
            .strict_binding()
            .unwrap();
        let client = reqwest::Client::new();

        let prepared = prepare_core_usage_with_existing_lease(
            "quota_first",
            &profile_path,
            Refresh::Forced,
            &lease,
            &binding,
        )
        .await
        .unwrap();
        let mut network = NetworkPermitBudget::new(first_network_permit(std::sync::Arc::new(
            tokio::sync::Semaphore::new(1),
        )));
        let core_usage = execute_prepared_core_usage_with_existing_lease_and_client(
            prepared,
            &lease,
            &client,
            &mut network,
        )
        .await
        .unwrap();

        assert_eq!(
            credits_started_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty),
            "the quota-first call must not contact the reset-credit endpoint"
        );
        assert_eq!(
            core_usage
                .primary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.0)
        );
        assert_eq!(core_usage.cache_revision, None);
        assert!(
            crate::cache::get_bound("quota_first", &binding)
                .unwrap()
                .is_none(),
            "the TUI core helper must return before publishing a cache generation"
        );

        let auth_value = auth::read_auth(&profile_path).unwrap();
        let enrichment_client = client.clone();
        let enrichment = tokio::spawn(async move {
            let mut usage = core_usage;
            crate::usage::enrich_reset_credits_for_auth_with_client(
                "quota_first",
                &auth_value,
                &mut usage,
                &enrichment_client,
            )
            .await;
            usage
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), credits_started_rx.recv())
            .await
            .expect("reset-credit enrichment did not reach the mock endpoint")
            .expect("reset-credit start channel closed unexpectedly");
        assert!(
            !enrichment.is_finished(),
            "enrichment should still be waiting on the blocked credits response"
        );
        credits_gate.add_permits(1);
        let enriched = enrichment.await.unwrap();
        server.abort();

        assert_eq!(enriched.reset_credits_error, None);
        assert_eq!(enriched.reset_credits_available_count, Some(1));
        assert_eq!(enriched.reset_credits.len(), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn unattended_core_probe_bypasses_and_preserves_metadata_complete_cache() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let reset_credit_calls = Arc::new(AtomicUsize::new(0));
        let reset_credit_server_calls = Arc::clone(&reset_credit_calls);
        let app = Router::new()
            .route(
                "/usage",
                get(|| async {
                    axum::Json(json!({
                        "plan_type": "pro",
                        "rate_limit": {
                            "primary_window": {
                                "used_percent": 12.0,
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
                get(move || {
                    let calls = Arc::clone(&reset_credit_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({"available_count": 0, "credits": []}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let now = crate::auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path("daemon_probe").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        let auth_value = json!({
            "tokens": {
                "id_token": jwt_with_exp_and_identity(now + 86_400),
                "access_token": jwt_with_exp(now + 86_400)
            }
        });
        write_auth_durable(&profile_path, &auth_value);
        let binding = auth::account_info_from_auth_value(&auth_value)
            .strict_binding()
            .unwrap();
        let cached = crate::cache::put_bound_versioned(
            "daemon_probe",
            &binding,
            &UsageInfo {
                primary: Some(crate::usage::WindowUsage {
                    used_percent: Some(77.0),
                    ..crate::usage::WindowUsage::default()
                }),
                reset_credits_available_count: Some(3),
                ..UsageInfo::default()
            },
        )
        .unwrap();
        let cached_revision = cached.cache_revision.clone();

        let lease = crate::profile::acquire_profile_lease_async("daemon_probe".to_string())
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let probed = probe_core_usage_unattended_with_existing_lease_and_client(
            "daemon_probe",
            &profile_path,
            &lease,
            Some(&binding),
            &client,
        )
        .await
        .unwrap();
        drop(lease);

        assert_eq!(
            probed
                .primary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.0),
            "the decision probe must bypass the cached quota"
        );
        assert_eq!(reset_credit_calls.load(Ordering::SeqCst), 0);
        let still_cached = crate::cache::get_bound("daemon_probe", &binding)
            .unwrap()
            .unwrap();
        assert_eq!(
            still_cached
                .primary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(77.0)
        );
        assert_eq!(still_cached.reset_credits_available_count, Some(3));
        assert_eq!(still_cached.cache_revision, cached_revision);
        server.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn expected_binding_mismatch_is_rejected_before_network_io() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let usage_calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&usage_calls);
        let app = Router::new().route(
            "/usage",
            get(move || {
                let calls = Arc::clone(&server_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "plan_type": "pro",
                        "rate_limit": null,
                        "credits": null,
                        "spend_control": null,
                        "additional_rate_limits": null,
                        "rate_limit_reached_type": null
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));

        let now = crate::auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path("rebound").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400)
                }
            }),
        );
        let expected = crate::jwt::StrictAccountBinding {
            account_id: "acct-previous-owner".to_string(),
            email: "previous@example.com".to_string(),
        };
        let lease = crate::profile::acquire_profile_lease_async("rebound".to_string())
            .await
            .unwrap();

        let error = probe_core_usage_unattended_with_existing_lease_and_client(
            "rebound",
            &profile_path,
            &lease,
            Some(&expected),
            &reqwest::Client::new(),
        )
        .await
        .expect_err("a rebound alias must be rejected before its usage request");
        server.abort();

        assert_eq!(error.summary, "profile identity changed");
        assert_eq!(usage_calls.load(Ordering::SeqCst), 0);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn rejected_refreshed_bearer_does_not_rotate_a_second_time() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        for (case, rejected_status) in [
            ("unauthorized", StatusCode::UNAUTHORIZED),
            ("forbidden", StatusCode::FORBIDDEN),
        ] {
            let usage_calls = Arc::new(AtomicUsize::new(0));
            let token_calls = Arc::new(AtomicUsize::new(0));
            let usage_server_calls = Arc::clone(&usage_calls);
            let token_server_calls = Arc::clone(&token_calls);
            let now = auth::now_unix_secs().unwrap();
            let refreshed_id = jwt_with_exp_and_identity(now + 172_800);
            let refreshed_access = jwt_with_exp(now + 172_800);
            let app = Router::new()
                .route(
                    "/usage",
                    get(move || {
                        let calls = Arc::clone(&usage_server_calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            rejected_status
                        }
                    }),
                )
                .route(
                    "/token",
                    post(move || {
                        let calls = Arc::clone(&token_server_calls);
                        let id_token = refreshed_id.clone();
                        let access_token = refreshed_access.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            axum::Json(json!({
                                "id_token": id_token,
                                "access_token": access_token,
                                "refresh_token": "new-refresh"
                            }))
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
            let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

            let alias = format!("rejected_refreshed_{case}");
            let profile_path = crate::profile::profile_auth_path(&alias).unwrap();
            std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
            let initial_auth = json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400),
                    "refresh_token": "old-refresh"
                }
            });
            write_auth_durable(&profile_path, &initial_auth);
            write_auth_durable(&crate::auth::codex_auth_path().unwrap(), &initial_auth);

            let error = fetch_usage_retried_force(&alias, &profile_path)
                .await
                .expect_err("a rejected newly refreshed bearer must be terminal");
            server.abort();

            assert!(
                error.detail.contains(&format!("HTTP {rejected_status}")),
                "{case}: {error:?}"
            );
            assert_eq!(usage_calls.load(Ordering::SeqCst), 2, "{case}");
            assert_eq!(token_calls.load(Ordering::SeqCst), 1, "{case}");
        }
    }

    async fn run_ambiguous_refresh_response(
        status: StatusCode,
        body: Value,
    ) -> (UsageError, usize, usize) {
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let usage_calls = Arc::new(AtomicUsize::new(0));
        let token_calls = Arc::new(AtomicUsize::new(0));
        let usage_server_calls = Arc::clone(&usage_calls);
        let token_server_calls = Arc::clone(&token_calls);
        let app = Router::new()
            .route(
                "/usage",
                get(move || {
                    let calls = Arc::clone(&usage_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::UNAUTHORIZED
                    }
                }),
            )
            .route(
                "/token",
                post(move || {
                    let calls = Arc::clone(&token_server_calls);
                    let body = body.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        (status, axum::Json(body))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

        let now = crate::auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path("alice").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400),
                    "refresh_token": "old-refresh"
                }
            }),
        );

        let error = fetch_usage_retried_force("alice", &profile_path)
            .await
            .expect_err("an ambiguous refresh response must stop the request");
        server.abort();
        (
            error,
            usage_calls.load(Ordering::SeqCst),
            token_calls.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn terminal_verdict_guard_rejects_a_superseded_refresh_token() {
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = home.path().join("auth.json");
        write_auth_durable(
            &path,
            &json!({
                "tokens": {
                    "id_token": "id",
                    "access_token": "access",
                    "refresh_token": "refresh_new"
                }
            }),
        );

        assert!(!profile_still_holds_refresh_token(&path, "refresh_old"));
        assert!(profile_still_holds_refresh_token(&path, "refresh_new"));
    }

    #[test]
    fn expired_id_token_does_not_refresh_a_valid_access_token() {
        let now = crate::auth::now_unix_secs().unwrap();
        let access = jwt_with_exp(now + 86_400);

        assert!(!access_token_needs_refresh(&access, 60).unwrap());
    }

    #[test]
    fn expiring_access_token_still_triggers_proactive_refresh() {
        let now = crate::auth::now_unix_secs().unwrap();
        let access = jwt_with_exp(now + 30);

        assert!(access_token_needs_refresh(&access, 60).unwrap());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn opportunistic_refresh_rechecks_current_expiry_after_candidate_discovery() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let now = auth::now_unix_secs().unwrap();
        let response_id = jwt_with_exp_and_identity(now + 172_800);
        let response_access = jwt_with_exp(now + 172_800);
        let app = Router::new().route(
            "/token",
            post(move || {
                let calls = Arc::clone(&server_calls);
                let id_token = response_id.clone();
                let access_token = response_access.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "id_token": id_token,
                        "access_token": access_token,
                        "refresh_token": "rotated-refresh"
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

        let alias = "expiry_rechecked";
        let profile_path = crate::profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 30),
                    "access_token": jwt_with_exp(now + 30),
                    "refresh_token": "same-refresh"
                }
            }),
        );

        let candidates = opportunistic_refresh_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, alias);

        let fresh_id = jwt_with_exp_and_identity(now + 86_400);
        let fresh_access = jwt_with_exp(now + 86_400);
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": fresh_id,
                    "access_token": fresh_access,
                    "refresh_token": "same-refresh"
                }
            }),
        );

        let failures = run_opportunistic_refresh_batch(
            candidates,
            std::time::Duration::from_secs(1),
            auth::service_endpoints().unwrap(),
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.abort();

        assert!(failures.is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "fresh access/id tokens must cancel the stale candidate before rotation"
        );
        let stored = auth::read_auth(&profile_path).unwrap();
        assert_eq!(
            stored
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("same-refresh")
        );
    }

    #[test]
    fn final_opportunistic_request_boundary_rejects_an_expired_deadline() {
        let now = tokio::time::Instant::now();
        assert!(!opportunistic_start_budget_remaining(now));
        assert!(opportunistic_start_budget_remaining(
            now + std::time::Duration::from_secs(60)
        ));
    }

    #[tokio::test]
    async fn opportunistic_refresh_surfaces_worker_panic_for_the_exact_alias() {
        let mut tasks: tokio::task::JoinSet<Option<UsageError>> = tokio::task::JoinSet::new();
        let task = tasks.spawn(async {
            panic!("secret-refresh-token-must-not-be-rendered");
        });
        let mut task_aliases = HashMap::from([(task.id(), "alice".to_string())]);
        let mut failures = Vec::new();

        let joined = tasks.join_next_with_id().await.unwrap();
        record_opportunistic_worker_result(joined, &mut task_aliases, &mut failures);

        assert!(task_aliases.is_empty());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].alias, "alice");
        assert_eq!(failures[0].error.summary, "token refresh outcome unknown");
        assert!(failures[0].error.detail.contains("panicked"));
        assert!(
            failures[0]
                .error
                .detail
                .contains("single-use refresh token")
        );
        assert!(!failures[0].error.detail.contains("secret-refresh-token"));
    }

    #[tokio::test]
    async fn opportunistic_refresh_surfaces_worker_cancellation_for_the_exact_alias() {
        let mut tasks: tokio::task::JoinSet<Option<UsageError>> = tokio::task::JoinSet::new();
        let task = tasks.spawn(std::future::pending());
        let mut task_aliases = HashMap::from([(task.id(), "bob".to_string())]);
        task.abort();
        let mut failures = Vec::new();

        let joined = tasks.join_next_with_id().await.unwrap();
        record_opportunistic_worker_result(joined, &mut task_aliases, &mut failures);

        assert!(task_aliases.is_empty());
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].alias, "bob");
        assert_eq!(failures[0].error.summary, "token refresh outcome unknown");
        assert!(failures[0].error.detail.contains("was cancelled"));
    }

    #[tokio::test]
    async fn opportunistic_refresh_keeps_successful_none_outcome_failure_free() {
        let mut tasks: tokio::task::JoinSet<Option<UsageError>> = tokio::task::JoinSet::new();
        let task = tasks.spawn(async { None });
        let mut task_aliases = HashMap::from([(task.id(), "carol".to_string())]);
        let mut failures = Vec::new();

        let joined = tasks.join_next_with_id().await.unwrap();
        record_opportunistic_worker_result(joined, &mut task_aliases, &mut failures);

        assert!(task_aliases.is_empty());
        assert!(failures.is_empty());
    }

    // Endpoint variables and auth paths are both process-global. Always acquire
    // URL_ENV_LOCK before TEST_ENV_LOCK so mixed URL/home fixtures cannot race
    // and tests needing both locks have one deadlock-free ordering.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn auth_read_finishing_after_the_budget_does_not_open_a_rotation() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let app = Router::new().route(
            "/token",
            post(move || {
                let calls = Arc::clone(&server_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "id_token": "new-id",
                        "access_token": "new-access",
                        "refresh_token": "new-refresh"
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set(
            "CODEX_HOME",
            home.path().join("codex").display().to_string(),
        );
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));
        let profile_path = home.path().join("profiles/late/auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        let expiring_id = jwt_with_exp_and_identity(crate::auth::now_unix_secs().unwrap() - 60);
        let expiring_access = jwt_with_exp(crate::auth::now_unix_secs().unwrap() - 60);
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": expiring_id,
                    "access_token": expiring_access,
                    "refresh_token": "old-refresh"
                }
            }),
        );
        set_before_opportunistic_request_hook(|deadline| {
            while tokio::time::Instant::now() < deadline {
                std::hint::spin_loop();
            }
        });

        let failures = refresh_expiring_tokens_within(std::time::Duration::from_millis(20))
            .await
            .unwrap();
        server.abort();

        assert!(failures.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let stored = crate::auth::read_auth(&profile_path).unwrap();
        assert_eq!(
            stored
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("old-refresh")
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn invalid_rotated_response_is_quarantined_without_retry_or_publication() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let usage_calls = Arc::new(AtomicUsize::new(0));
        let token_calls = Arc::new(AtomicUsize::new(0));
        let usage_server_calls = Arc::clone(&usage_calls);
        let token_server_calls = Arc::clone(&token_calls);
        let app = Router::new()
            .route(
                "/usage",
                get(move || {
                    let calls = Arc::clone(&usage_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::UNAUTHORIZED
                    }
                }),
            )
            .route(
                "/token",
                post(move || {
                    let calls = Arc::clone(&token_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "id_token": "",
                            "access_token": "new-access",
                            "refresh_token": "new-refresh"
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

        let now = crate::auth::now_unix_secs().unwrap();
        let profile = json!({
            "tokens": {
                "id_token": jwt_with_exp_and_identity(now + 86_400),
                "access_token": jwt_with_exp(now + 86_400),
                "refresh_token": "old-refresh"
            }
        });
        let profile_path = crate::profile::profile_auth_path("alice").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(&profile_path, &profile);
        let live_path = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live_path, &profile);
        let profile_before = std::fs::read(&profile_path).unwrap();
        let live_before = std::fs::read(&live_path).unwrap();

        let error = fetch_usage_retried_force("alice", &profile_path)
            .await
            .expect_err("an invalid rotated response must not be installed");
        server.abort();

        assert_eq!(error.summary, "refreshed credentials quarantined");
        assert_eq!(usage_calls.load(Ordering::SeqCst), 1);
        assert_eq!(token_calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
        assert_eq!(std::fs::read(&live_path).unwrap(), live_before);
        let recovery_dir = home.path().join("recovery");
        let recovery_files = std::fs::read_dir(&recovery_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(recovery_files.len(), 1);
        let recovery: Value =
            serde_json::from_slice(&std::fs::read(&recovery_files[0]).unwrap()).unwrap();
        assert_eq!(
            recovery.get("refresh_token").and_then(Value::as_str),
            Some("new-refresh")
        );
        assert_eq!(
            recovery.get("recovery_kind").and_then(Value::as_str),
            Some("invalid_token_refresh_response")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blank_successor_refresh_token_stops_after_one_irreversible_request() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        crate::config::init_defaults_for_tests();
        let usage_calls = Arc::new(AtomicUsize::new(0));
        let token_calls = Arc::new(AtomicUsize::new(0));
        let usage_server_calls = Arc::clone(&usage_calls);
        let token_server_calls = Arc::clone(&token_calls);
        let app = Router::new()
            .route(
                "/usage",
                get(move || {
                    let calls = Arc::clone(&usage_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::UNAUTHORIZED
                    }
                }),
            )
            .route(
                "/token",
                post(move || {
                    let calls = Arc::clone(&token_server_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({
                            "id_token": "new-id",
                            "access_token": "new-access",
                            "refresh_token": ""
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{address}/token"));

        let mut authorize = || Ok(());
        let mut persist = |(): (), _: &str, _: RefreshTokenResolution| -> Result<RefreshedTokens> {
            panic!("an unusable successor must never reach persistence");
        };
        let endpoints = auth::service_endpoints().unwrap();
        let client = auth::build_http_client().unwrap();
        let mut network = NetworkPermitBudget::unlimited();
        let error = fetch_usage_with_refresh_transactional(
            &endpoints,
            &client,
            "alice",
            "old-access",
            Some("old-id"),
            Some("old-refresh"),
            None,
            false,
            &mut authorize,
            &mut persist,
            ResetCreditEnrichment::Inline,
            &mut network,
        )
        .await
        .expect_err("a blank successor makes the refresh outcome unknown");
        server.abort();

        assert!(
            error.downcast_ref::<RefreshOutcomeUnknown>().is_some(),
            "unexpected error: {error:#}"
        );
        assert_eq!(usage_calls.load(Ordering::SeqCst), 1);
        assert_eq!(token_calls.load(Ordering::SeqCst), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn ambiguous_http_responses_never_replay_the_single_use_refresh_token() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        for (status, body, case) in [
            (StatusCode::OK, json!({}), "empty success"),
            (
                StatusCode::BAD_GATEWAY,
                json!({"message": "upstream unavailable"}),
                "unstructured non-success",
            ),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": {}}),
                "unrecognizable error object",
            ),
        ] {
            let (error, usage_calls, token_calls) =
                run_ambiguous_refresh_response(status, body).await;
            assert_eq!(error.summary, "token refresh outcome unknown", "{case}");
            assert!(error.detail.contains("do not retry"), "{case}: {error:?}");
            assert_eq!(usage_calls, 1, "{case}");
            assert_eq!(token_calls, 1, "{case}");
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn transport_loss_after_refresh_submission_stops_after_one_request() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());

        let usage_calls = Arc::new(AtomicUsize::new(0));
        let usage_server_calls = Arc::clone(&usage_calls);
        let usage_app = Router::new().route(
            "/usage",
            get(move || {
                let calls = Arc::clone(&usage_server_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::UNAUTHORIZED
                }
            }),
        );
        let usage_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let usage_address = usage_listener.local_addr().unwrap();
        let usage_server =
            tokio::spawn(async move { axum::serve(usage_listener, usage_app).await.unwrap() });

        let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_address = token_listener.local_addr().unwrap();
        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_server_calls = Arc::clone(&token_calls);
        let token_server = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;

            loop {
                let Ok((mut stream, _)) = token_listener.accept().await else {
                    return;
                };
                token_server_calls.fetch_add(1, Ordering::SeqCst);
                let mut request = vec![0_u8; 4096];
                let _ = stream.read(&mut request).await;
                // Drop the connection without an HTTP response after observing
                // the submitted POST. The client cannot know whether a token
                // endpoint behind this connection already rotated the token.
            }
        });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{usage_address}/usage"));
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", format!("http://{token_address}/token"));

        let now = crate::auth::now_unix_secs().unwrap();
        let profile_path = crate::profile::profile_auth_path("alice").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(
            &profile_path,
            &json!({
                "tokens": {
                    "id_token": jwt_with_exp_and_identity(now + 86_400),
                    "access_token": jwt_with_exp(now + 86_400),
                    "refresh_token": "old-refresh"
                }
            }),
        );

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fetch_usage_retried_force("alice", &profile_path),
        )
        .await
        .expect("transport-loss fixture must complete")
        .expect_err("transport loss must make refresh outcome unknown");
        usage_server.abort();
        token_server.abort();

        assert_eq!(error.summary, "token refresh outcome unknown");
        assert!(error.detail.contains("do not retry"), "{error:?}");
        assert_eq!(usage_calls.load(Ordering::SeqCst), 1);
        assert_eq!(token_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_refresh_request_uses_json_body_like_codex() {
        let request = build_refresh_request(
            &reqwest::Client::new(),
            "https://auth.openai.com/oauth/token",
            "refresh-token-value",
        )
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "client_id": crate::auth::CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "refresh-token-value",
            })
        );
    }

    #[test]
    fn test_account_routing_headers_include_workspace_and_fedramp() {
        let request = apply_account_routing_headers(
            reqwest::Client::new().get("https://example.invalid/usage"),
            Some("workspace-123"),
            true,
        )
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("workspace-123")
        );
        assert_eq!(
            request
                .headers()
                .get("X-OpenAI-Fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn test_refresh_without_id_token_preserves_existing_id_token() {
        let RefreshTokenResolution::Validated(refreshed) = resolve_refreshed_tokens(
            RefreshResponse {
                id_token: None,
                access_token: Some("new-access".to_string()),
                refresh_token: None,
                error: None,
                error_description: None,
            },
            reqwest::StatusCode::OK,
            Some("existing-id"),
            "existing-refresh",
        )
        .unwrap() else {
            panic!("a valid refresh response must resolve to validated tokens");
        };

        assert_eq!(refreshed.id_token, "existing-id");
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "existing-refresh");
    }

    #[test]
    fn rotated_refresh_response_quarantines_explicitly_empty_identity_or_access_token() {
        for field in ["id_token", "access_token"] {
            let mut response = RefreshResponse {
                id_token: Some("new-id".to_string()),
                access_token: Some("new-access".to_string()),
                refresh_token: Some("new-refresh".to_string()),
                error: None,
                error_description: None,
            };
            match field {
                "id_token" => response.id_token = Some(" \t".to_string()),
                "access_token" => response.access_token = Some(String::new()),
                "refresh_token" => response.refresh_token = Some("\n".to_string()),
                _ => unreachable!(),
            }

            let resolution = resolve_refreshed_tokens(
                response,
                reqwest::StatusCode::OK,
                Some("existing-id"),
                "existing-refresh",
            )
            .expect("a returned successor must be preserved for recovery");
            let RefreshTokenResolution::RotatedButInvalid { recovery, cause } = resolution else {
                panic!("an invalid rotated response must not be accepted");
            };
            assert_eq!(
                recovery.get("refresh_token").and_then(Value::as_str),
                Some("new-refresh")
            );
            assert!(format!("{cause:#}").contains(field), "{field}: {cause:#}");
        }
    }

    #[test]
    fn explicitly_blank_refresh_token_is_outcome_unknown() {
        let error = resolve_refreshed_tokens(
            RefreshResponse {
                id_token: Some("new-id".to_string()),
                access_token: Some("new-access".to_string()),
                refresh_token: Some("\n".to_string()),
                error: None,
                error_description: None,
            },
            reqwest::StatusCode::OK,
            Some("existing-id"),
            "existing-refresh",
        )
        .expect_err("a blank returned refresh token makes rotation outcome unknown");

        assert!(error.downcast_ref::<RefreshOutcomeUnknown>().is_some());
        assert!(format!("{error:#}").contains("refresh_token"));
    }

    #[test]
    fn invalid_rotation_recovery_failure_is_terminal_and_leaves_auth_unchanged() {
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());
        let now = crate::auth::now_unix_secs().unwrap();
        let profile = json!({
            "tokens": {
                "id_token": jwt_with_exp_and_identity(now + 86_400),
                "access_token": jwt_with_exp(now + 86_400),
                "refresh_token": "old-refresh"
            }
        });
        let profile_path = crate::profile::profile_auth_path("alice").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        write_auth_durable(&profile_path, &profile);
        let live_path = crate::auth::codex_auth_path().unwrap();
        write_auth_durable(&live_path, &profile);
        let profile_before = std::fs::read(&profile_path).unwrap();
        let live_before = std::fs::read(&live_path).unwrap();
        std::fs::write(home.path().join("recovery"), b"blocks recovery directory").unwrap();

        let lease = crate::profile::acquire_profile_lease("alice").unwrap();
        let authorization = crate::profile::authorize_fresh_credentials_activation(&lease).unwrap();
        let error = persist_refresh_resolution(
            &lease,
            authorization,
            "old-refresh",
            RefreshTokenResolution::RotatedButInvalid {
                recovery: json!({
                    "id_token": "",
                    "access_token": "new-access",
                    "refresh_token": "new-refresh"
                }),
                cause: anyhow::anyhow!("invalid id_token"),
            },
        )
        .expect_err("failure to preserve the successor must be terminal");

        assert_eq!(error.summary, "rotated credential recovery failed");
        assert!(error.detail.contains("do not retry"));
        assert_eq!(std::fs::read(&profile_path).unwrap(), profile_before);
        assert_eq!(std::fs::read(&live_path).unwrap(), live_before);
    }

    #[test]
    fn refresh_success_requires_an_explicit_access_token() {
        let error = resolve_refreshed_tokens(
            RefreshResponse {
                id_token: None,
                access_token: None,
                refresh_token: None,
                error: None,
                error_description: None,
            },
            reqwest::StatusCode::OK,
            Some("existing-id"),
            "existing-refresh",
        )
        .expect_err("a success response must not reuse the stored access token");

        assert!(error.downcast_ref::<RefreshOutcomeUnknown>().is_some());
        assert!(format!("{error:#}").contains("access_token"), "{error:#}");
    }
}
