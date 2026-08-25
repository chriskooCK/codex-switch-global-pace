use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth::{self, CLIENT_ID, format_reqwest_error};

use super::parse::parse_usage_checked;
use super::reset_credits::enrich_reset_credits;
use super::{
    ImportValidation, MAX_RETRIES, RETRY_DELAY, Refresh, RefreshOutcomeUnknown, RefreshedTokens,
    TerminalAuthError, TokenPersistFailure, UsageError, UsageInfo,
};

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
async fn remember_terminal_verdict(
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

fn format_refresh_error(code: &str, message: Option<&str>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

fn usage_url() -> String {
    std::env::var("CS_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_string())
}

fn token_needs_refresh(access_token: &str, id_token: Option<&str>, margin_secs: i64) -> bool {
    crate::jwt::is_token_expiring(access_token, margin_secs).unwrap_or(false)
        || id_token
            .is_some_and(|token| crate::jwt::is_token_expiring(token, margin_secs).unwrap_or(false))
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

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

/// Usage discovery performed as one stage of a larger credential operation
/// that already owns this profile's lease (currently warmup). Acquiring again
/// would deadlock because the OS lease is deliberately non-reentrant.
pub(crate) async fn fetch_usage_retried_unattended_leased(
    alias: &str,
    profile_path: &Path,
    lease: &crate::profile::ProfileLease,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_with_lease(alias, profile_path, Refresh::Unattended, lease).await
}

/// TUI usage discovery after its cancellable lease-acquisition phase has
/// completed. Cache semantics remain identical to the ordinary entry points;
/// the caller owns the lease so shutdown can distinguish pre-network waiting
/// from credential work that must be drained.
pub(crate) async fn fetch_usage_retried_with_existing_lease(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
) -> std::result::Result<UsageInfo, UsageError> {
    ensure_usage_configuration()?;
    if let Some(cached) = usage_cache_hit(alias, refresh).await? {
        return Ok(cached);
    }
    fetch_usage_retried_with_lease(alias, profile_path, refresh, lease).await
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
        anyhow::bail!(
            "token refresh failed: {}",
            format_refresh_error(&code, message.as_deref())
        );
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
    if let Some(cached) = usage_cache_hit(alias, refresh).await? {
        return Ok(cached);
    }

    let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
        .await
        .map_err(|error| UsageError {
            summary: "profile lock failed".to_string(),
            detail: format!("[{alias}] could not lock profile for usage refresh: {error:#}"),
        })?;
    fetch_usage_retried_with_lease(alias, profile_path, refresh, &lease).await
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
    refresh: Refresh,
) -> std::result::Result<Option<UsageInfo>, UsageError> {
    if refresh.skips_usage_cache() {
        debug!("{alias}: {refresh:?} refresh, bypassing the usage cache");
        return Ok(None);
    }
    match crate::cache::get_async(alias)
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

async fn fetch_usage_retried_with_lease(
    alias: &str,
    profile_path: &Path,
    refresh: Refresh,
    lease: &crate::profile::ProfileLease,
) -> std::result::Result<UsageInfo, UsageError> {
    if lease.alias() != alias {
        return Err(UsageError {
            summary: "profile lock mismatch".to_string(),
            detail: format!(
                "usage request for '{alias}' received lease for '{}'",
                lease.alias()
            ),
        });
    }

    let val = auth::read_auth(profile_path).map_err(|e| {
        let detail = format!("failed to read auth file {}: {e}", profile_path.display());
        UsageError {
            summary: "auth file unreadable".into(),
            detail,
        }
    })?;
    let account_info = crate::jwt::parse_account_info(&val);
    let account_id = account_info.account_id;
    let is_fedramp = account_info.is_fedramp;
    let mut id_token = auth::extract_id_token(&val);
    let (access_token, refresh_token) = auth::extract_tokens(&val);
    let mut refresh_token = refresh_token;

    // A verdict the auth server already named stands until the credential is
    // replaced, so re-presenting it buys nothing but the round trip. Only an
    // explicit user force skips this — see [`Refresh`].
    if !refresh.may_re_present_a_rejected_credential()
        && let Some(rt) = refresh_token.as_deref()
    {
        match crate::cache::get_auth_failure_async(alias, rt).await {
            Ok(Some(known)) => {
                debug!("{alias}: credential already rejected by the auth server, not retrying");
                return Err(known);
            }
            Ok(None) => {}
            Err(error) => {
                return Err(UsageError {
                    summary: "auth cache unreadable".to_string(),
                    detail: format!(
                        "[{alias}] could not safely decide whether this credential was already rejected: {error:#}"
                    ),
                });
            }
        }
    }

    let mut at = match access_token {
        Some(t) => t,
        None => {
            return Err(UsageError {
                summary: "no access_token".into(),
                detail: "no access_token in auth file".into(),
            });
        }
    };

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
                alias,
                &at,
                id_token.as_deref(),
                refresh_token.as_deref(),
                account_id.as_deref(),
                is_fedramp,
                &mut authorize_rotation,
                &mut persist_before_follow_up,
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
                if let Err(error) = crate::cache::put_async(alias, &usage).await {
                    warn!("[{alias}] usage succeeded, but caching the result failed: {error:#}");
                }
                return Ok(usage);
            }
            Err(e) => {
                let msg = format!("{e:#}");
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
    let mut authorize_rotation = || Ok(());
    let mut persist_authorized = |(): (), presented: &str, resolution: RefreshTokenResolution| {
        persist_unbound_refresh_resolution(alias, presented, resolution, persist_rotation)
    };
    fetch_usage_with_refresh_transactional(
        alias,
        access_token,
        id_token,
        refresh_token,
        account_id,
        is_fedramp,
        &mut authorize_rotation,
        &mut persist_authorized,
    )
    .await
}

/// Internal variant that obtains a commit authorization immediately before
/// each refresh request and carries that exact value to the persistence step.
/// Ordinary usage GETs therefore remain independent of live-auth filesystem
/// state, while no single-use refresh token can be spent without a prepared
/// conditional publication boundary.
#[allow(clippy::too_many_arguments)]
async fn fetch_usage_with_refresh_transactional<A, F, T>(
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
    authorize_rotation: &mut A,
    persist_rotation: &mut F,
) -> Result<UsageInfo>
where
    A: FnMut() -> Result<T>,
    F: FnMut(T, &str, RefreshTokenResolution) -> Result<RefreshedTokens>,
{
    let client = auth::build_http_client()?;
    let usage_url = usage_url();
    let mut rejected_refresh: Option<anyhow::Error> = None;

    // Refresh when either JWT is near expiry so account identity metadata does
    // not remain stale while the access token is still usable.
    if let Some(rt) = refresh_token
        && token_needs_refresh(access_token, id_token, 60)
    {
        info!("[{alias}] token expiring soon, proactively refreshing");

        let authorization = authorize_rotation()?;
        match do_refresh_token(alias, &client, id_token, rt).await {
            Ok(resolution) => {
                let new_tokens = persist_rotation(authorization, rt, resolution)?;
                let bearer = new_tokens.access_token.clone();

                let resp = apply_account_routing_headers(
                    client
                        .get(&usage_url)
                        .header("Authorization", format!("Bearer {bearer}")),
                    account_id,
                    is_fedramp,
                )
                .send()
                .await
                .map_err(|e| format_reqwest_error("Usage API request failed", &e))?;

                let status = resp.status();
                debug!("[{alias}] Usage API (after proactive refresh): HTTP {status}");
                if status.is_success() {
                    let body: Value = resp.json().await.map_err(|e| {
                        anyhow::anyhow!("failed to parse usage response (HTTP {status}): {e}")
                    })?;
                    debug!(
                        "[{alias}] Usage API raw body (proactive): {}",
                        crate::auth::redact_sensitive_log_body(&body)
                    );
                    let mut usage = parse_usage_checked(&body)?;
                    enrich_reset_credits(
                        alias, &client, &bearer, account_id, is_fedramp, &mut usage,
                    )
                    .await;
                    return Ok(usage);
                }
                anyhow::bail!("Usage API failed (HTTP {status}) after proactive token refresh");
            }
            Err(e) => {
                if e.downcast_ref::<RefreshOutcomeUnknown>().is_some() {
                    return Err(e);
                }
                if e.downcast_ref::<TerminalAuthError>().is_some() {
                    info!("[{alias}] proactive token refresh rejected permanently: {e:#}");
                    rejected_refresh = Some(e);
                } else {
                    info!(
                        "[{alias}] proactive token refresh failed, trying with existing token: {e:#}"
                    );
                }
            }
        }
    }

    let resp = apply_account_routing_headers(
        client
            .get(&usage_url)
            .header("Authorization", format!("Bearer {access_token}")),
        account_id,
        is_fedramp,
    )
    .send()
    .await
    .map_err(|e| format_reqwest_error("Usage API request failed", &e))?;

    let status = resp.status();
    debug!("[{alias}] Usage API: HTTP {status}");
    if status.is_success() {
        let body: Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse usage response (HTTP {status}): {e}"))?;
        debug!(
            "[{alias}] Usage API raw body: {}",
            crate::auth::redact_sensitive_log_body(&body)
        );
        let mut usage = parse_usage_checked(&body)?;
        enrich_reset_credits(
            alias,
            &client,
            access_token,
            account_id,
            is_fedramp,
            &mut usage,
        )
        .await;
        return Ok(usage);
    }

    // The auth server already rejected this refresh token moments ago; asking
    // again can only re-trigger reuse detection and add a round trip.
    if let Some(e) = rejected_refresh {
        return Err(e.context(format!("Usage API failed (HTTP {status})")));
    }

    // If 401/403 and we have a refresh_token, try to refresh
    if let Some(rt) = refresh_token
        && (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
    {
        info!("[{alias}] got HTTP {status}, attempting token refresh");

        let authorization = authorize_rotation()?;
        match do_refresh_token(alias, &client, id_token, rt).await {
            Ok(resolution) => {
                let new_tokens = persist_rotation(authorization, rt, resolution)?;
                let bearer = new_tokens.access_token.clone();

                let resp2 = apply_account_routing_headers(
                    client
                        .get(&usage_url)
                        .header("Authorization", format!("Bearer {bearer}")),
                    account_id,
                    is_fedramp,
                )
                .send()
                .await
                .map_err(|e| format_reqwest_error("Usage API retry request failed", &e))?;

                let status2 = resp2.status();
                debug!("[{alias}] Usage API (after token refresh): HTTP {status2}");
                if status2.is_success() {
                    let body: Value = resp2.json().await.map_err(|e| {
                        anyhow::anyhow!(
                            "failed to parse usage response after refresh (HTTP {status2}): {e}"
                        )
                    })?;
                    let mut usage = parse_usage_checked(&body)?;
                    enrich_reset_credits(
                        alias, &client, &bearer, account_id, is_fedramp, &mut usage,
                    )
                    .await;
                    return Ok(usage);
                }
                anyhow::bail!("Usage API still failed (HTTP {status2}) after token refresh");
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

    anyhow::bail!("Usage API failed (HTTP {status}), no refresh_token available");
}

/// Validate an auth.json being imported, refreshing its credentials if needed.
///
/// `persist_rotation` must durably write the updated auth value. It runs for
/// every successful refresh before any follow-up usage request is sent; an
/// error stops validation immediately rather than leaving a new single-use
/// token only in memory. See [`ImportValidation`].
pub async fn validate_import_auth<F>(
    val: &mut serde_json::Value,
    mut persist_rotation: F,
) -> ImportValidation
where
    F: FnMut(&serde_json::Value) -> Result<()>,
{
    let mut refreshed = None;
    let mut validated_account_id = None;
    let result = validate_import_auth_capturing_refresh(val, &mut refreshed, &mut persist_rotation)
        .await
        .map(|(usage, account_id)| {
            validated_account_id = Some(account_id);
            usage
        });
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
                fetch_usage_with_refresh(
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
            if let Err(err) = crate::workspace::refresh_for_auth(val).await {
                debug!("workspace metadata unavailable while importing: {err}");
            }
            Ok((usage, validated_account_id))
        }
        (None, Some(rt)) => {
            let client = auth::build_http_client()?;
            let first_resolution =
                do_refresh_token(alias, &client, id_token.as_deref(), &rt).await?;
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
                fetch_usage_with_refresh(
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
            if let Err(err) = crate::workspace::refresh_for_auth(val).await {
                debug!("workspace metadata unavailable while importing: {err}");
            }
            Ok((usage, validated_account_id))
        }
        (None, None) => anyhow::bail!("auth.json missing access_token and refresh_token"),
    }
}

/// Build the token refresh request. Codex 0.144.1 sends a JSON body
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
    alias: &str,
    client: &reqwest::Client,
    current_id_token: Option<&str>,
    refresh_token: &str,
) -> Result<RefreshTokenResolution> {
    let token_url = auth::token_url();
    debug!("[{alias}] sending token refresh request to {token_url}");

    let resp = build_refresh_request(client, &token_url, refresh_token)
        .send()
        .await
        .map_err(|error| {
            let detail = format_reqwest_error("token refresh request failed", &error).to_string();
            refresh_outcome_unknown(anyhow::Error::new(error).context(format!(
                "token refresh request transport failed after submission began: {detail}"
            )))
        })?;

    let status = resp.status();
    debug!("[{alias}] token refresh response: HTTP {status}");

    // Read raw body first so we can log it on parse failure
    let body_text = resp.text().await.map_err(|error| {
        refresh_outcome_unknown(anyhow::Error::new(error).context(format!(
            "failed to read token refresh response body (HTTP {status})"
        )))
    })?;

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
pub async fn refresh_expiring_tokens() -> Vec<TokenPersistFailure> {
    refresh_expiring_tokens_within(OPPORTUNISTIC_START_BUDGET).await
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
) -> Vec<TokenPersistFailure> {
    let profiles = match crate::profile::list_profiles() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let now = auth::now_unix_secs();

    // Collect current tokens for profiles expiring soon.
    let mut candidates: Vec<(String, std::path::PathBuf, String, i64)> = Vec::new();
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
        // Expiry alone says nothing about whether the credential can still be
        // rotated. Without this, every dead profile is refreshed again here —
        // after `list` has already printed its final screen, so the user waits
        // on a request whose answer is known and not even displayed.
        match crate::cache::get_auth_failure(alias, &rt) {
            Ok(Some(_)) => {
                debug!("[{alias}] skipping opportunistic refresh: credential already rejected");
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                warn!(
                    "[{alias}] skipping opportunistic refresh because the auth-failure cache could not be read: {error:#}"
                );
                continue;
            }
        }
        let expiry = [
            crate::jwt::token_expires_at(&at),
            id_token.as_deref().and_then(crate::jwt::token_expires_at),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(exp) = expiry else {
            continue;
        };
        let remaining = exp - now;
        if remaining < OPPORTUNISTIC_REFRESH_MARGIN {
            candidates.push((alias.clone(), path, rt, exp));
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    // Sort by expiration: soonest first
    candidates.sort_by_key(|c| c.3);
    candidates.truncate(OPPORTUNISTIC_REFRESH_LIMIT);

    let count = candidates.len();
    debug!(
        "opportunistic refresh: {count} token(s) expiring within {}s",
        OPPORTUNISTIC_REFRESH_MARGIN
    );

    // Build before starting the budget: client construction can synchronously
    // initialize TLS state, but the budget is only for opening rotations.
    let client = match auth::build_http_client() {
        Ok(client) => client,
        Err(error) => {
            warn!(
                stage = "client_build_failed",
                "opportunistic token refresh unavailable: {error:#}"
            );
            return Vec::new();
        }
    };

    // Start refreshes while the budget lasts, then wait for every started one:
    // an in-flight rotation is not cancellable without losing the credential.
    let deadline = tokio::time::Instant::now() + budget;
    let mut queued = candidates.into_iter();
    let mut tasks: tokio::task::JoinSet<Option<UsageError>> = tokio::task::JoinSet::new();
    let mut task_aliases = HashMap::new();
    let mut failures = Vec::new();

    loop {
        while tasks.len() < OPPORTUNISTIC_REFRESH_CONCURRENCY
            && tokio::time::Instant::now() < deadline
        {
            let Some((alias, path, rt, exp)) = queued.next() else {
                break;
            };
            let tracked_alias = alias.clone();
            let client = client.clone();
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
                let (_, current_refresh_token) = auth::extract_tokens(&value);
                let current_refresh_token = current_refresh_token?;
                if current_refresh_token != rt {
                    debug!(
                        "[{alias}] opportunistic refresh skipped: credential changed before lease"
                    );
                    return None;
                }
                let id_token = auth::extract_id_token(&value);
                let remaining = exp - auth::now_unix_secs();
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

    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    async fn run_ambiguous_refresh_response(
        status: StatusCode,
        body: Value,
    ) -> (UsageError, usize, usize) {
        let home = tempfile::tempdir().unwrap();
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

        let now = crate::auth::now_unix_secs();
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
        let home = tempfile::tempdir().unwrap();
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
    fn expired_id_token_triggers_refresh_before_access_token_expires() {
        let now = crate::auth::now_unix_secs();
        let access = jwt_with_exp(now + 86_400);
        let id = jwt_with_exp(now - 60);

        assert!(token_needs_refresh(&access, Some(&id), 60));
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

    // Process-global auth paths are serialized by TEST_ENV_LOCK. This test uses
    // a current-thread runtime, and no awaited task tries to reacquire it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn auth_read_finishing_after_the_budget_does_not_open_a_rotation() {
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
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
        let expiring_id = jwt_with_exp_and_identity(crate::auth::now_unix_secs() - 60);
        let expiring_access = jwt_with_exp(crate::auth::now_unix_secs() - 60);
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

        let failures = refresh_expiring_tokens_within(std::time::Duration::from_millis(20)).await;
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

        let home = tempfile::tempdir().unwrap();
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

        let now = crate::auth::now_unix_secs();
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
        let error = fetch_usage_with_refresh_transactional(
            "alice",
            "old-access",
            Some("old-id"),
            Some("old-refresh"),
            None,
            false,
            &mut authorize,
            &mut persist,
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

        let home = tempfile::tempdir().unwrap();
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

        let now = crate::auth::now_unix_secs();
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
        let home = tempfile::tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path().display().to_string());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", codex_home.display().to_string());
        let now = crate::auth::now_unix_secs();
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
