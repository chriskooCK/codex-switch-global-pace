use std::path::Path;

use anyhow::{Context, Result};
use rand::Rng;
use serde_json::Value;
use tracing::debug;

use crate::auth::{self, format_reqwest_error};

use super::api::{
    NetworkLimiterClosed, NetworkPermitBudget, apply_account_routing_headers, extract_error_summary,
};
use super::parse::parse_optional_u64;
use super::{
    ACCESS_TOKEN_REFRESH_MARGIN_SECS, ConsumedResetCredit, MAX_RETRIES, RETRY_DELAY,
    RefreshOutcomeUnknown, RefreshedTokens, ResetCredit, TerminalAuthError, UsageError, UsageInfo,
};

#[derive(Debug, thiserror::Error)]
#[error("reset credits request failed (HTTP {status})")]
struct ResetCreditAccessRejected {
    status: reqwest::StatusCode,
}

/// Reset-card request state prepared under a profile lease before entering the
/// automatic-selection network budget.
pub(crate) struct PreparedResetCreditEnrichment {
    alias: String,
    endpoints: auth::ServiceEndpoints,
    account_id: Option<String>,
    is_fedramp: bool,
    id_token: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    known_refresh_verdict: Option<UsageError>,
    needs_proactive_rotation: bool,
}

/// One exact reset-card redemption prepared under the caller's profile lease.
/// Auth reads, request validation, and endpoint policy resolution all happen
/// before execution polls the shared network budget.
pub(crate) struct PreparedResetCreditConsumeRequest {
    alias: String,
    endpoints: auth::ServiceEndpoints,
    access_token: String,
    account_id: Option<String>,
    is_fedramp: bool,
    credit: ResetCredit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsumeFailureKind {
    DefinitelyNotConsumed,
    OutcomeUnknownAfterRequest,
}

#[derive(Debug)]
pub struct ConsumeResetCreditError {
    kind: ConsumeFailureKind,
    source: anyhow::Error,
}

impl ConsumeResetCreditError {
    fn not_consumed(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: ConsumeFailureKind::DefinitelyNotConsumed,
            source: source.into(),
        }
    }

    fn outcome_unknown(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: ConsumeFailureKind::OutcomeUnknownAfterRequest,
            source: source.into(),
        }
    }

    pub fn definitely_not_consumed(&self) -> bool {
        self.kind == ConsumeFailureKind::DefinitelyNotConsumed
    }

    pub fn outcome_unknown_after_request(&self) -> bool {
        self.kind == ConsumeFailureKind::OutcomeUnknownAfterRequest
    }

    pub fn user_facing_unknown_message(&self, alias: &str) -> String {
        debug_assert!(self.outcome_unknown_after_request());
        format!("{alias}: reset-card consumption may have occurred; verify before retry")
    }
}

impl std::fmt::Display for ConsumeResetCreditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ConsumeResetCreditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

pub(super) struct ResetCreditRequestAuth<'a> {
    access_token: &'a str,
    account_id: Option<&'a str>,
    is_fedramp: bool,
}

impl<'a> ResetCreditRequestAuth<'a> {
    pub(super) fn new(
        access_token: &'a str,
        account_id: Option<&'a str>,
        is_fedramp: bool,
    ) -> Self {
        Self {
            access_token,
            account_id,
            is_fedramp,
        }
    }
}

pub(super) async fn enrich_reset_credits(
    endpoints: &auth::ServiceEndpoints,
    alias: &str,
    client: &reqwest::Client,
    request_auth: ResetCreditRequestAuth<'_>,
    usage: &mut UsageInfo,
    network: &mut NetworkPermitBudget,
) {
    match enrich_reset_credits_checked(
        endpoints,
        client,
        request_auth.access_token,
        request_auth.account_id,
        request_auth.is_fedramp,
        usage,
        network,
    )
    .await
    {
        Ok(()) => {
            usage.reset_credits_error = None;
        }
        Err(err) => {
            let msg = err.to_string();
            debug!("[{alias}] reset credits fetch failed: {msg}");
            usage.reset_credits_error = Some(extract_error_summary(&msg));
        }
    }
}

async fn enrich_reset_credits_checked(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    usage: &mut UsageInfo,
    network: &mut NetworkPermitBudget,
) -> Result<()> {
    let summary = fetch_reset_credits(
        endpoints,
        client,
        access_token,
        account_id,
        is_fedramp,
        network,
    )
    .await?;
    merge_reset_credits(usage, summary);
    usage.reset_credits_error = None;
    Ok(())
}

/// Complete the dedicated reset-credit lookup after a quota-first usage fetch.
/// Failures remain non-fatal and are recorded on `usage`, matching inline
/// enrichment semantics.
pub(crate) async fn enrich_reset_credits_for_auth_with_client(
    alias: &str,
    auth_value: &Value,
    usage: &mut UsageInfo,
    client: &reqwest::Client,
) {
    let mut network = NetworkPermitBudget::unlimited();
    let endpoints = match auth::service_endpoints() {
        Ok(endpoints) => endpoints,
        Err(error) => {
            let message = format!("reset credits endpoint policy unavailable: {error:#}");
            debug!("[{alias}] {message}");
            usage.reset_credits_error = Some(extract_error_summary(&message));
            return;
        }
    };
    let (access_token, _) = auth::extract_tokens(auth_value);
    let Some(access_token) = access_token.filter(|token| !token.trim().is_empty()) else {
        let message = "auth.json missing access_token for reset credits";
        debug!("[{alias}] {message}");
        usage.reset_credits_error = Some(message.to_string());
        return;
    };
    let account_info = crate::jwt::parse_account_info(auth_value);
    enrich_reset_credits(
        &endpoints,
        alias,
        client,
        ResetCreditRequestAuth::new(
            &access_token,
            account_info.account_id.as_deref(),
            account_info.is_fedramp,
        ),
        usage,
        &mut network,
    )
    .await;
}

fn reset_credit_usage_error(alias: &str, error: anyhow::Error) -> UsageError {
    if error.downcast_ref::<NetworkLimiterClosed>().is_some() {
        return UsageError {
            summary: "usage limiter closed".to_string(),
            detail: format!("[{alias}] reset-card request could not reserve network capacity"),
        };
    }
    if let Some(terminal) = error.downcast_ref::<TerminalAuthError>() {
        return UsageError {
            summary: terminal.summary(),
            detail: format!("[{alias}] {error:#}"),
        };
    }
    if error.downcast_ref::<RefreshOutcomeUnknown>().is_some() {
        return UsageError::refresh_outcome_unknown(alias, &error);
    }
    let detail = format!("[{alias}] {error:#}");
    UsageError {
        summary: extract_error_summary(&detail),
        detail,
    }
}

async fn rotate_reset_credit_credentials(
    alias: &str,
    lease: &crate::profile::ProfileLease,
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    id_token: Option<&str>,
    refresh_token: &str,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<RefreshedTokens, UsageError> {
    let authorization = crate::profile::authorize_fresh_credentials_activation(lease)
        .map_err(|error| UsageError::refresh_authorization_failed(alias, &error))?;
    let resolution = match super::api::do_refresh_token_with_network(
        endpoints,
        alias,
        client,
        id_token,
        refresh_token,
        network,
    )
    .await
    {
        Ok(resolution) => resolution,
        Err(error) => {
            let terminal_code = error
                .downcast_ref::<TerminalAuthError>()
                .map(|terminal| terminal.code.clone());
            let usage_error = reset_credit_usage_error(alias, error);
            if let Some(code) = terminal_code {
                super::api::remember_terminal_verdict(
                    alias,
                    &code,
                    Some(refresh_token),
                    &usage_error,
                )
                .await;
            }
            return Err(usage_error);
        }
    };
    super::api::persist_refresh_resolution(lease, authorization, refresh_token, resolution)
}

pub(crate) async fn prepare_reset_credit_enrichment_with_existing_lease(
    alias: &str,
    profile_path: &Path,
    lease: &crate::profile::ProfileLease,
    expected_binding: &crate::jwt::StrictAccountBinding,
) -> std::result::Result<PreparedResetCreditEnrichment, UsageError> {
    if lease.alias() != alias {
        return Err(UsageError {
            summary: "profile lock mismatch".to_string(),
            detail: format!(
                "reset-card request for '{alias}' received lease for '{}'",
                lease.alias()
            ),
        });
    }

    let auth_value = auth::read_auth_async(profile_path)
        .await
        .map_err(|error| UsageError {
            summary: "auth file unreadable".to_string(),
            detail: format!(
                "[{alias}] could not read profile auth for reset-card details: {error:#}"
            ),
        })?;
    let account_info = auth::account_info_from_auth_value(&auth_value);
    let actual_binding = account_info.strict_binding().ok_or_else(|| UsageError {
        summary: "account identity incomplete".to_string(),
        detail: format!(
            "[{alias}] reset-card details require a verified account id and email identity"
        ),
    })?;
    if &actual_binding != expected_binding {
        return Err(UsageError {
            summary: "profile identity changed".to_string(),
            detail: format!(
                "[{alias}] profile changed account identity while the reset-card request was queued"
            ),
        });
    }

    let id_token = auth::extract_id_token(&auth_value);
    let (access_token, refresh_token) = auth::extract_tokens(&auth_value);
    let access_token = access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| UsageError {
            summary: "no access_token".to_string(),
            detail: format!("[{alias}] profile auth has no access_token for reset-card details"),
        })?;
    let refresh_token = refresh_token.filter(|token| !token.trim().is_empty());
    let endpoints = auth::service_endpoints().map_err(|error| UsageError {
        summary: "service endpoint policy invalid".to_string(),
        detail: format!("[{alias}] could not resolve reset-card endpoints: {error:#}"),
    })?;

    let needs_proactive_rotation = crate::jwt::is_token_expiring(
        &access_token,
        ACCESS_TOKEN_REFRESH_MARGIN_SECS,
    )
    .map_err(|error| UsageError {
        summary: "access token unreadable".to_string(),
        detail: format!(
            "[{alias}] could not inspect access-token expiry for reset-card details: {error:#}"
        ),
    })? == Some(true);
    let known_refresh_verdict = match refresh_token.as_deref() {
        Some(refresh_token) => {
            super::api::cached_terminal_auth_verdict(alias, refresh_token).await?
        }
        None => None,
    };
    if needs_proactive_rotation {
        refresh_token.as_deref().ok_or_else(|| UsageError {
            summary: "no refresh_token".to_string(),
            detail: format!(
                "[{alias}] access token is expiring and profile auth has no refresh_token for reset-card details"
            ),
        })?;
        if let Some(known) = known_refresh_verdict.as_ref() {
            debug!("[{alias}] reset-card refresh skipped: credential already rejected");
            return Err(known.clone());
        }
    }
    Ok(PreparedResetCreditEnrichment {
        alias: alias.to_string(),
        endpoints,
        account_id: account_info.account_id,
        is_fedramp: account_info.is_fedramp,
        id_token,
        access_token,
        refresh_token,
        known_refresh_verdict,
        needs_proactive_rotation,
    })
}

pub(crate) async fn execute_prepared_reset_credit_enrichment_with_existing_lease_and_client(
    prepared: PreparedResetCreditEnrichment,
    lease: &crate::profile::ProfileLease,
    usage: &mut UsageInfo,
    client: &reqwest::Client,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<(), UsageError> {
    let PreparedResetCreditEnrichment {
        alias,
        endpoints,
        account_id,
        is_fedramp,
        id_token,
        mut access_token,
        refresh_token,
        known_refresh_verdict,
        needs_proactive_rotation,
    } = prepared;
    if lease.alias() != alias {
        return Err(UsageError {
            summary: "profile lock mismatch".to_string(),
            detail: format!(
                "prepared reset-card request for '{alias}' received lease for '{}'",
                lease.alias()
            ),
        });
    }
    let alias = alias.as_str();
    let mut rotation_used = false;
    if needs_proactive_rotation {
        let Some(presented) = refresh_token.as_deref() else {
            return Err(UsageError {
                summary: "no refresh_token".to_string(),
                detail: format!(
                    "[{alias}] prepared reset-card rotation has no refresh_token; no credential was sent"
                ),
            });
        };
        let rotated = rotate_reset_credit_credentials(
            alias,
            lease,
            &endpoints,
            client,
            id_token.as_deref(),
            presented,
            network,
        )
        .await?;
        access_token = rotated.access_token;
        rotation_used = true;
    }

    let first = enrich_reset_credits_checked(
        &endpoints,
        client,
        &access_token,
        account_id.as_deref(),
        is_fedramp,
        usage,
        network,
    )
    .await;
    let Err(first_error) = first else {
        return Ok(());
    };
    if first_error
        .downcast_ref::<ResetCreditAccessRejected>()
        .is_none()
        || rotation_used
    {
        return Err(reset_credit_usage_error(alias, first_error));
    }

    if let Some(known) = known_refresh_verdict {
        debug!(
            "[{alias}] reset-card refresh skipped after HTTP rejection: credential already rejected"
        );
        return Err(known);
    }
    let presented = refresh_token.as_deref().ok_or_else(|| UsageError {
        summary: "no refresh_token".to_string(),
        detail: format!(
            "[{alias}] reset-card endpoint rejected the access token and profile auth has no refresh_token"
        ),
    })?;
    let rotated = rotate_reset_credit_credentials(
        alias,
        lease,
        &endpoints,
        client,
        id_token.as_deref(),
        presented,
        network,
    )
    .await?;
    enrich_reset_credits_checked(
        &endpoints,
        client,
        &rotated.access_token,
        account_id.as_deref(),
        is_fedramp,
        usage,
        network,
    )
    .await
    .map_err(|error| reset_credit_usage_error(alias, error))
}

async fn fetch_reset_credits(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    network: &mut NetworkPermitBudget,
) -> Result<ResetCreditsSummary> {
    fetch_reset_credits_at_url_with_network(
        client,
        access_token,
        account_id,
        is_fedramp,
        endpoints.reset_credits()?,
        network,
    )
    .await
}

#[cfg(test)]
async fn fetch_reset_credits_at_url(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    url: &str,
) -> Result<ResetCreditsSummary> {
    let mut network = NetworkPermitBudget::unlimited();
    fetch_reset_credits_at_url_with_network(
        client,
        access_token,
        account_id,
        is_fedramp,
        url,
        &mut network,
    )
    .await
}

async fn fetch_reset_credits_at_url_with_network(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    url: &str,
    network: &mut NetworkPermitBudget,
) -> Result<ResetCreditsSummary> {
    let req = client
        .get(url)
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .header("OpenAI-Beta", "codex-1")
        .header("Originator", "Codex Desktop");
    let req = apply_account_routing_headers(req, account_id, is_fedramp);

    let (status, body) = {
        let _permit = network.acquire().await?;
        let resp = req
            .send()
            .await
            .map_err(|e| format_reqwest_error("reset credits request failed", &e))?;
        let status = resp.status();
        let body = if status.is_success() {
            Some(
                resp.json()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to parse reset credits response: {e}"))?,
            )
        } else {
            None
        };
        (status, body)
    };
    if !status.is_success() {
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(ResetCreditAccessRejected { status }.into());
        }
        anyhow::bail!("reset credits request failed (HTTP {status})");
    }

    let body: Value = body.expect("successful reset-credit response must carry its buffered body");
    parse_reset_credits_details(&body)
        .context("reset credits response does not match the expected details shape")
}

pub(crate) fn reset_credit_expiry_timestamp(
    credit: &ResetCredit,
) -> std::result::Result<Option<i64>, chrono::ParseError> {
    credit
        .expires_at
        .as_deref()
        .map(|value| chrono::DateTime::parse_from_rfc3339(value).map(|dt| dt.timestamp()))
        .transpose()
}

pub(crate) fn reset_credit_expiry_sort_key(credit: &ResetCredit) -> i64 {
    reset_credit_expiry_timestamp(credit)
        .ok()
        .flatten()
        .unwrap_or(i64::MAX)
}

pub fn earliest_reset_credit(credits: &[ResetCredit]) -> Option<&ResetCredit> {
    credits
        .iter()
        .min_by_key(|credit| reset_credit_expiry_sort_key(credit))
}

fn load_consume_context(
    alias: &str,
    profile_path: &Path,
) -> std::result::Result<(String, Option<String>, bool), ConsumeResetCreditError> {
    let val = auth::read_auth(profile_path).map_err(ConsumeResetCreditError::not_consumed)?;
    let (access_token, _) = auth::extract_tokens(&val);
    let access_token = access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: auth.json missing access_token"))
        .map_err(ConsumeResetCreditError::not_consumed)?;
    let account_info = crate::jwt::parse_account_info(&val);
    Ok((
        access_token,
        account_info.account_id,
        account_info.is_fedramp,
    ))
}

/// Consume the exact reset credit to which the caller obtained user consent.
///
/// This deliberately does not fetch the server's current list and select a new
/// earliest card. A confirmation UI can therefore never turn consent for one
/// card into consumption of another card when its cached list changes between
/// confirmation and submission.
pub async fn consume_reset_credit_by_id(
    alias: &str,
    profile_path: &Path,
    credit: ResetCredit,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    crate::profile::validate_alias(alias).map_err(ConsumeResetCreditError::not_consumed)?;
    if credit.id.trim().is_empty() {
        return Err(ConsumeResetCreditError::not_consumed(anyhow::anyhow!(
            "{alias}: reset card is missing its id"
        )));
    }

    // Client construction does not use credentials, so keep it outside the
    // profile lease. The lease still covers the complete credential/network
    // lifetime and makes rename/delete wait for a known redemption outcome.
    let client = auth::build_http_client().map_err(ConsumeResetCreditError::not_consumed)?;
    let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
        .await
        .map_err(ConsumeResetCreditError::not_consumed)?;
    consume_reset_credit_by_id_leased_with_client(alias, profile_path, credit, &lease, &client)
        .await
}

pub(crate) async fn consume_reset_credit_by_id_leased_with_client(
    alias: &str,
    profile_path: &Path,
    credit: ResetCredit,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    let prepared =
        prepare_reset_credit_consume_with_existing_lease(alias, profile_path, credit, lease)
            .await?;
    let mut network = NetworkPermitBudget::unlimited();
    execute_prepared_reset_credit_consume_request(prepared, lease, client, &mut network).await
}

pub(crate) async fn prepare_reset_credit_consume_with_existing_lease(
    alias: &str,
    profile_path: &Path,
    credit: ResetCredit,
    lease: &crate::profile::ProfileLease,
) -> std::result::Result<PreparedResetCreditConsumeRequest, ConsumeResetCreditError> {
    validate_consume_request(alias, &credit, lease)?;
    let (access_token, account_id, is_fedramp) = load_consume_context(alias, profile_path)?;
    let endpoints = auth::service_endpoints().map_err(ConsumeResetCreditError::not_consumed)?;

    Ok(PreparedResetCreditConsumeRequest {
        alias: alias.to_string(),
        endpoints,
        access_token,
        account_id,
        is_fedramp,
        credit,
    })
}

pub(crate) async fn execute_prepared_reset_credit_consume_with_existing_lease_and_client(
    prepared: PreparedResetCreditConsumeRequest,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    execute_prepared_reset_credit_consume_request(prepared, lease, client, network).await
}

async fn execute_prepared_reset_credit_consume_request(
    prepared: PreparedResetCreditConsumeRequest,
    lease: &crate::profile::ProfileLease,
    client: &reqwest::Client,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    let PreparedResetCreditConsumeRequest {
        alias,
        endpoints,
        access_token,
        account_id,
        is_fedramp,
        credit,
    } = prepared;
    if lease.alias() != alias {
        return Err(ConsumeResetCreditError::not_consumed(anyhow::anyhow!(
            "prepared reset-card use for '{alias}' received profile lease for '{}'",
            lease.alias()
        )));
    }

    send_reset_credit_consume(
        &endpoints,
        client,
        &access_token,
        account_id.as_deref(),
        is_fedramp,
        credit,
        network,
    )
    .await
}

fn validate_consume_request(
    alias: &str,
    credit: &ResetCredit,
    lease: &crate::profile::ProfileLease,
) -> std::result::Result<(), ConsumeResetCreditError> {
    if credit.id.trim().is_empty() {
        return Err(ConsumeResetCreditError::not_consumed(anyhow::anyhow!(
            "{alias}: reset card is missing its id"
        )));
    }
    if lease.alias() != alias {
        return Err(ConsumeResetCreditError::not_consumed(anyhow::anyhow!(
            "reset-card use for '{alias}' received profile lease for '{}'",
            lease.alias()
        )));
    }
    Ok(())
}

async fn send_reset_credit_consume(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    credit: ResetCredit,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    consume_reset_credit_at_url_with_network(
        client,
        access_token,
        account_id,
        is_fedramp,
        credit,
        endpoints
            .reset_credits_consume()
            .map_err(ConsumeResetCreditError::not_consumed)?,
        network,
    )
    .await
}

#[cfg(test)]
async fn consume_reset_credit_at_url(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    credit: ResetCredit,
    url: &str,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    let mut network = NetworkPermitBudget::unlimited();
    consume_reset_credit_at_url_with_network(
        client,
        access_token,
        account_id,
        is_fedramp,
        credit,
        url,
        &mut network,
    )
    .await
}

#[cfg(test)]
async fn consume_reset_credit_at_url_with_first_permit(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    credit: ResetCredit,
    url: &str,
    first_permit: super::api::FirstNetworkPermit,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    let mut network = NetworkPermitBudget::new(first_permit);
    consume_reset_credit_at_url_with_network(
        client,
        access_token,
        account_id,
        is_fedramp,
        credit,
        url,
        &mut network,
    )
    .await
}

async fn consume_reset_credit_at_url_with_network(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    credit: ResetCredit,
    url: &str,
    network: &mut NetworkPermitBudget,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    // Generate once per user action. Any retry after an ambiguous transport/server
    // failure must identify the same logical redemption to the backend.
    let request_id = redeem_request_id();
    let mut outcome_may_have_changed = false;
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(RETRY_DELAY).await;
        }
        let req = client
            .post(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .header("OpenAI-Beta", "codex-1")
            .header("Originator", "Codex Desktop")
            .json(&serde_json::json!({
                    "credit_id": &credit.id,
                    "redeem_request_id": &request_id,
            }));
        let req = apply_account_routing_headers(req, account_id, is_fedramp);

        let network_permit = network.acquire().await.map_err(|error| {
            if outcome_may_have_changed {
                ConsumeResetCreditError::outcome_unknown(error)
            } else {
                ConsumeResetCreditError::not_consumed(error)
            }
        })?;
        let exchange = {
            let _permit = network_permit;
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let body = if status.is_success() {
                        Some(resp.json::<Value>().await)
                    } else {
                        None
                    };
                    Ok((status, body))
                }
                Err(error) => Err(error),
            }
        };
        let (status, body) = match exchange {
            Ok(exchange) => exchange,
            Err(error) if attempt + 1 < MAX_RETRIES => {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} failed before response: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    format_reqwest_error("request failed", &error)
                );
                continue;
            }
            Err(error) => {
                return Err(ConsumeResetCreditError::outcome_unknown(
                    format_reqwest_error("reset credit consume request failed", &error),
                ));
            }
        };
        if !status.is_success() {
            if (status.is_server_error() || status.as_u16() == 429) && attempt + 1 < MAX_RETRIES {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} returned HTTP {status}",
                    attempt + 1,
                    MAX_RETRIES
                );
                continue;
            }
            let error = anyhow::anyhow!("reset credit consume request failed (HTTP {status})");
            if status.is_client_error() && status.as_u16() != 429 && !outcome_may_have_changed {
                return Err(ConsumeResetCreditError::not_consumed(error));
            }
            return Err(ConsumeResetCreditError::outcome_unknown(error));
        }

        match body.expect("successful consume response must carry its buffered body") {
            Ok(body) => {
                return parse_consumed_reset_credit(&body, credit)
                    .map_err(ConsumeResetCreditError::outcome_unknown);
            }
            Err(error) if attempt + 1 < MAX_RETRIES => {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} returned invalid JSON: {error}",
                    attempt + 1,
                    MAX_RETRIES
                );
            }
            Err(error) => {
                return Err(ConsumeResetCreditError::outcome_unknown(anyhow::anyhow!(
                    "failed to parse reset credit consume response: {error}"
                )));
            }
        }
    }

    unreachable!("reset credit retry loop always returns on its final attempt")
}

fn parse_consumed_reset_credit(body: &Value, credit: ResetCredit) -> Result<ConsumedResetCredit> {
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("reset credit consume response missing code"))?;
    if code != "reset" {
        anyhow::bail!("reset credit was not consumed: {code}");
    }

    Ok(ConsumedResetCredit {
        credit,
        code: Some(code.to_string()),
        windows_reset: parse_optional_u64(body.get("windows_reset")),
        redeemed_at: body
            .get("credit")
            .and_then(|v| v.as_object())
            .and_then(|obj| {
                obj.get("redeemed_at")
                    .or_else(|| obj.get("redeemedAt"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string()),
    })
}

fn redeem_request_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let value = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &value[0..8],
        &value[8..12],
        &value[12..16],
        &value[16..20],
        &value[20..32]
    )
}

fn optional_reset_credit_string(
    obj: &serde_json::Map<String, Value>,
    snake_case: &str,
    camel_case: &str,
) -> Result<Option<String>> {
    let value = match (obj.get(snake_case), obj.get(camel_case)) {
        (Some(_), Some(_)) => {
            anyhow::bail!("reset credit must not contain both {snake_case} and {camel_case}")
        }
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.trim().to_string())),
        Some(_) => anyhow::bail!("reset credit {snake_case} must be a string or null"),
    }
}

fn parse_reset_credit(value: &Value) -> Result<Option<ResetCredit>> {
    let obj = value
        .as_object()
        .context("each reset credit must be a JSON object")?;

    let reset_type = optional_reset_credit_string(obj, "reset_type", "resetType")?;
    if let Some(reset_type) = reset_type.as_deref()
        && reset_type != "codex_rate_limits"
    {
        return Ok(None);
    }

    let status = match obj.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(status)) => Some(status.trim()),
        Some(_) => anyhow::bail!("reset credit status must be a string or null"),
    };
    if let Some(status) = status
        && status != "available"
    {
        return Ok(None);
    }

    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .context("available reset credit id must be a non-empty string")?;
    let granted_at = optional_reset_credit_string(obj, "granted_at", "grantedAt")?
        .filter(|value| !value.is_empty());
    let expires_at = optional_reset_credit_string(obj, "expires_at", "expiresAt")?
        .filter(|value| !value.is_empty());

    Ok(Some(ResetCredit {
        id,
        granted_at,
        expires_at,
    }))
}

#[derive(Debug)]
pub(super) struct ResetCreditsSummary {
    pub(super) available_count: Option<u64>,
    /// `None` means the response omitted a list and supplied only summary data;
    /// `Some(vec![])` is an authoritative explicit empty list.
    pub(super) credits: Option<Vec<ResetCredit>>,
}

fn merge_reset_credits(usage: &mut UsageInfo, summary: ResetCreditsSummary) {
    if summary.available_count.is_some() {
        usage.reset_credits_available_count = summary.available_count;
    }
    match summary.credits {
        Some(credits) => usage.reset_credits = credits,
        None if summary.available_count == Some(0) => usage.reset_credits.clear(),
        None => {}
    }
}

pub(super) fn parse_reset_credits_summary(body: &Value) -> Result<ResetCreditsSummary> {
    let obj = body
        .as_object()
        .context("reset credits summary must be a JSON object")?;
    let count_value = match (obj.get("available_count"), obj.get("availableCount")) {
        (Some(_), Some(_)) => {
            anyhow::bail!(
                "reset credits summary must not contain both available_count and availableCount"
            )
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => anyhow::bail!("reset credits summary missing available_count"),
    };
    let available_count = parse_optional_u64(Some(count_value)).context(
        "reset credits available_count must be a non-negative integer or numeric string",
    )?;
    let credits = match obj.get("credits") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    parse_reset_credit(item)
                        .with_context(|| format!("invalid reset credit at index {index}"))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
        ),
        None | Some(Value::Null) => None,
        Some(_) => anyhow::bail!("reset credits credits field must be an array"),
    };

    Ok(ResetCreditsSummary {
        available_count: Some(available_count),
        credits,
    })
}

fn parse_reset_credits_details(body: &Value) -> Result<ResetCreditsSummary> {
    let summary = parse_reset_credits_summary(body)?;
    if summary.credits.is_none() {
        anyhow::bail!("reset credits details response credits field must be an array");
    }
    Ok(summary)
}

/// Revalidate the exact card the user approved against a forced usage fetch
/// made while the caller owns the profile lease. Account/workspace blockers
/// cannot be repaired by a quota reset, and an ambiguous/missing card list must
/// never be treated as permission to issue the irreversible POST.
pub(crate) fn validate_reset_credit_preflight(
    alias: &str,
    usage: &UsageInfo,
    confirmed: &ResetCredit,
) -> Result<()> {
    if let Some(blocker) = super::explicit_account_blocker(usage) {
        anyhow::bail!(
            "{alias}: reset card cannot clear the account/workspace restriction ({blocker}); no reset card was requested"
        );
    }
    if let Some(error) = usage.reset_credits_error.as_deref() {
        anyhow::bail!(
            "{alias}: reset-card ownership could not be revalidated ({error}); no reset card was requested"
        );
    }
    let matches = usage
        .reset_credits
        .iter()
        .filter(|current| {
            current.id == confirmed.id
                && current.granted_at == confirmed.granted_at
                && current.expires_at == confirmed.expires_at
        })
        .count();
    if matches != 1 {
        anyhow::bail!(
            "{alias}: the exact reset card approved by the user changed or disappeared; no reset card was requested"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(name);
            // SAFETY: callers hold the crate-wide environment locks for the
            // guard's complete lifetime.
            unsafe { std::env::set_var(name, value) };
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: the owning test still holds the crate-wide environment
            // locks while restoring the process environment.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }

    struct RefreshRejectionServer {
        token_url: String,
        credits_url: String,
        token_calls: Arc<AtomicUsize>,
        credit_calls: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn start_memorable_refresh_rejection_server(
        credits_status: StatusCode,
    ) -> RefreshRejectionServer {
        let token_calls = Arc::new(AtomicUsize::new(0));
        let credit_calls = Arc::new(AtomicUsize::new(0));
        let token_calls_by_server = Arc::clone(&token_calls);
        let credit_calls_by_server = Arc::clone(&credit_calls);
        let app = axum::Router::new()
            .route(
                "/token",
                post(move || {
                    let calls = Arc::clone(&token_calls_by_server);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": "refresh_token_reused",
                                "error_description": "credential already rotated"
                            })),
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/credits",
                get(move || {
                    let calls = Arc::clone(&credit_calls_by_server);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if credits_status.is_success() {
                            Json(json!({"available_count": 0, "credits": []})).into_response()
                        } else {
                            credits_status.into_response()
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        RefreshRejectionServer {
            token_url: format!("http://{address}/token"),
            credits_url: format!("http://{address}/credits"),
            token_calls,
            credit_calls,
            task,
        }
    }

    fn test_identity_jwt(email: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "email": email,
                "exp": 4_102_444_800_i64
            }))
            .unwrap(),
        );
        format!("header.{payload}.signature")
    }

    fn test_access_jwt(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"exp": exp})).unwrap());
        format!("header.{payload}.signature")
    }

    fn write_reset_refresh_profile(
        alias: &str,
        account_id: &str,
        email: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> (std::path::PathBuf, crate::jwt::StrictAccountBinding) {
        let path = crate::profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "tokens": {
                    "id_token": test_identity_jwt(email),
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                    "account_id": account_id
                }
            }))
            .unwrap(),
        )
        .unwrap();
        (
            path,
            crate::jwt::StrictAccountBinding {
                account_id: account_id.to_string(),
                email: email.to_string(),
            },
        )
    }

    async fn reset_detail_error(
        alias: &str,
        profile_path: &Path,
        binding: &crate::jwt::StrictAccountBinding,
        client: &reqwest::Client,
    ) -> UsageError {
        let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
            .await
            .unwrap();
        let mut usage = UsageInfo::default();
        match prepare_reset_credit_enrichment_with_existing_lease(
            alias,
            profile_path,
            &lease,
            binding,
        )
        .await
        {
            Ok(prepared) => {
                let mut network = NetworkPermitBudget::unlimited();
                execute_prepared_reset_credit_enrichment_with_existing_lease_and_client(
                    prepared,
                    &lease,
                    &mut usage,
                    client,
                    &mut network,
                )
                .await
                .expect_err("the fixture's refresh credential must be rejected")
            }
            Err(error) => error,
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn known_memorable_verdict_skips_proactive_reset_card_refresh() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let server = start_memorable_refresh_rejection_server(StatusCode::OK).await;
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", &server.token_url);
        let _credits_url = EnvVarGuard::set("CS_RESET_CREDITS_URL", &server.credits_url);
        let alias = "known_reset_refresh_rejection";
        let refresh_token = "known-rejected-refresh";
        let (profile_path, binding) = write_reset_refresh_profile(
            alias,
            "acct-known-rejected",
            "known-rejected@example.com",
            &test_access_jwt(1),
            refresh_token,
        );
        let known = UsageError {
            summary: "re-login required (refresh_token_reused)".to_string(),
            detail: "known credential rejection".to_string(),
        };
        crate::cache::put_auth_failure(alias, refresh_token, &known).unwrap();

        let error =
            reset_detail_error(alias, &profile_path, &binding, &reqwest::Client::new()).await;
        server.task.abort();

        assert_eq!(error.summary, known.summary);
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 0);
        assert_eq!(server.credit_calls.load(Ordering::SeqCst), 0);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn proactive_memorable_rejection_is_submitted_only_once() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let server = start_memorable_refresh_rejection_server(StatusCode::OK).await;
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", &server.token_url);
        let _credits_url = EnvVarGuard::set("CS_RESET_CREDITS_URL", &server.credits_url);
        let alias = "proactive_reset_refresh_rejection";
        let refresh_token = "proactive-rejected-refresh";
        let (profile_path, binding) = write_reset_refresh_profile(
            alias,
            "acct-proactive-rejected",
            "proactive-rejected@example.com",
            &test_access_jwt(1),
            refresh_token,
        );
        let client = reqwest::Client::new();

        let first = reset_detail_error(alias, &profile_path, &binding, &client).await;
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.credit_calls.load(Ordering::SeqCst), 0);
        assert!(
            crate::cache::get_auth_failure(alias, refresh_token)
                .unwrap()
                .is_some()
        );
        let second = reset_detail_error(alias, &profile_path, &binding, &client).await;
        server.task.abort();

        assert!(first.summary.contains("refresh_token_reused"));
        assert!(second.summary.contains("refresh_token_reused"));
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.credit_calls.load(Ordering::SeqCst), 0);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reactive_memorable_rejection_is_submitted_only_once() {
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);
        let server = start_memorable_refresh_rejection_server(StatusCode::UNAUTHORIZED).await;
        let _token_url = EnvVarGuard::set("CS_TOKEN_URL", &server.token_url);
        let _credits_url = EnvVarGuard::set("CS_RESET_CREDITS_URL", &server.credits_url);
        let alias = "reactive_reset_refresh_rejection";
        let refresh_token = "reactive-rejected-refresh";
        let (profile_path, binding) = write_reset_refresh_profile(
            alias,
            "acct-reactive-rejected",
            "reactive-rejected@example.com",
            &test_access_jwt(4_102_444_800),
            refresh_token,
        );
        let client = reqwest::Client::new();

        let first = reset_detail_error(alias, &profile_path, &binding, &client).await;
        assert_eq!(server.credit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);
        assert!(
            crate::cache::get_auth_failure(alias, refresh_token)
                .unwrap()
                .is_some()
        );
        let second = reset_detail_error(alias, &profile_path, &binding, &client).await;
        server.task.abort();

        assert!(first.summary.contains("refresh_token_reused"));
        assert!(second.summary.contains("refresh_token_reused"));
        assert_eq!(server.credit_calls.load(Ordering::SeqCst), 2);
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fedramp_routing_headers_reach_reset_credit_fetch_and_consume() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let get_observed = Arc::clone(&observed);
        let post_observed = Arc::clone(&observed);
        let app = axum::Router::new()
            .route(
                "/credits",
                get(move |headers: HeaderMap| {
                    let observed = Arc::clone(&get_observed);
                    async move {
                        observed.lock().unwrap().push((
                            headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok())
                                == Some("workspace-123"),
                            headers.get("x-openai-fedramp").and_then(|v| v.to_str().ok())
                                == Some("true"),
                        ));
                        Json(json!({"available_count": 1, "credits": [{"id": "credit-1", "status": "available"}]}))
                    }
                }),
            )
            .route(
                "/consume",
                post(move |headers: HeaderMap| {
                    let observed = Arc::clone(&post_observed);
                    async move {
                        observed.lock().unwrap().push((
                            headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok())
                                == Some("workspace-123"),
                            headers.get("x-openai-fedramp").and_then(|v| v.to_str().ok())
                                == Some("true"),
                        ));
                        Json(json!({"code": "reset", "windows_reset": 2}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        fetch_reset_credits_at_url(
            &client,
            "access-token",
            Some("workspace-123"),
            true,
            &format!("http://{address}/credits"),
        )
        .await
        .unwrap();
        consume_reset_credit_at_url(
            &client,
            "access-token",
            Some("workspace-123"),
            true,
            ResetCredit {
                id: "credit-1".into(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(*observed.lock().unwrap(), vec![(true, true), (true, true)]);
    }

    #[tokio::test]
    async fn dedicated_fetch_rejects_explicit_malformed_summary_fields() {
        let app = axum::Router::new()
            .route(
                "/bad-credits",
                get(|| async {
                    Json(json!({
                        "available_count": 1,
                        "credits": {"id": "not-an-array"}
                    }))
                }),
            )
            .route(
                "/bad-count",
                get(|| async { Json(json!({"available_count": "many"})) }),
            )
            .route(
                "/null-credits",
                get(|| async { Json(json!({"available_count": 0, "credits": null})) }),
            )
            .route(
                "/missing-credits",
                get(|| async { Json(json!({"available_count": 0})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();

        let bad_credits = fetch_reset_credits_at_url(
            &client,
            "access-token",
            None,
            false,
            &format!("http://{address}/bad-credits"),
        )
        .await
        .unwrap_err();
        let bad_count = fetch_reset_credits_at_url(
            &client,
            "access-token",
            None,
            false,
            &format!("http://{address}/bad-count"),
        )
        .await
        .unwrap_err();
        let null_credits = fetch_reset_credits_at_url(
            &client,
            "access-token",
            None,
            false,
            &format!("http://{address}/null-credits"),
        )
        .await
        .unwrap_err();
        let missing_credits = fetch_reset_credits_at_url(
            &client,
            "access-token",
            None,
            false,
            &format!("http://{address}/missing-credits"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(format!("{bad_credits:#}").contains("credits field must be an array"));
        assert!(format!("{bad_count:#}").contains("available_count must be"));
        assert!(
            format!("{null_credits:#}").contains("details response credits field must be an array")
        );
        assert!(
            format!("{missing_credits:#}")
                .contains("details response credits field must be an array")
        );
    }

    #[test]
    fn test_reset_credit_without_expiry_is_preserved_and_sorted_last() {
        let expiring = parse_reset_credit(&json!({
            "id": "expiring",
            "status": "available",
            "expires_at": "2026-07-08T00:00:00Z"
        }))
        .unwrap()
        .unwrap();
        let no_expiry = parse_reset_credit(&json!({
            "id": "no-expiry",
            "status": "available",
            "expires_at": null
        }))
        .unwrap()
        .unwrap();
        let credits = vec![no_expiry, expiring];

        assert_eq!(credits[0].expires_at, None);
        assert_eq!(earliest_reset_credit(&credits).unwrap().id, "expiring");
    }

    #[test]
    fn empty_credit_id_rejects_the_summary_before_selection() {
        let error = parse_reset_credits_summary(&json!({
            "available_count": 2,
            "credits": [
                {
                    "id": "  ",
                    "status": "available",
                    "expires_at": "2026-07-01T00:00:00Z"
                },
                {
                    "id": "valid-credit",
                    "status": "available",
                    "expires_at": "2026-08-01T00:00:00Z"
                }
            ]
        }))
        .expect_err("an explicit empty card id must not be silently discarded");

        assert!(
            format!("{error:#}").contains("available reset credit id must be a non-empty string")
        );
    }

    #[test]
    fn explicit_empty_credit_list_clears_stale_count_and_cards() {
        let mut usage = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![ResetCredit {
                id: "stale-credit".to_string(),
                granted_at: None,
                expires_at: None,
            }],
            ..UsageInfo::default()
        };

        merge_reset_credits(
            &mut usage,
            parse_reset_credits_summary(&json!({"available_count": 0, "credits": []})).unwrap(),
        );

        assert_eq!(usage.reset_credits_available_count, Some(0));
        assert!(usage.reset_credits.is_empty());
    }

    #[test]
    fn embedded_summary_treats_null_credits_as_omitted() {
        for body in [
            json!({"available_count": 2}),
            json!({"available_count": 2, "credits": null}),
        ] {
            let summary = parse_reset_credits_summary(&body).unwrap();
            assert_eq!(summary.available_count, Some(2));
            assert!(summary.credits.is_none());
        }
    }

    #[test]
    fn explicit_malformed_summary_fields_are_not_treated_as_omitted() {
        for malformed in [
            json!({}),
            json!({"credits": []}),
            json!({"available_count": 0, "credits": {}}),
            json!({"available_count": -1}),
            json!({"available_count": "many"}),
            json!({"available_count": null}),
        ] {
            assert!(
                parse_reset_credits_summary(&malformed).is_err(),
                "explicit malformed shape must fail: {malformed}"
            );
        }
    }

    #[test]
    fn explicit_malformed_credit_item_rejects_the_whole_summary() {
        for malformed in [
            json!({"available_count": 1, "credits": [7]}),
            json!({"available_count": 1, "credits": [{}]}),
            json!({"available_count": 1, "credits": [{"id": "  ", "status": "available"}]}),
            json!({"available_count": 1, "credits": [{"id": "credit-1", "status": 1}]}),
            json!({"available_count": 1, "credits": [{"id": "credit-1", "expires_at": 123}]}),
        ] {
            let error = parse_reset_credits_summary(&malformed)
                .expect_err("a malformed explicit card must reject the summary");
            assert!(
                format!("{error:#}").contains("invalid reset credit at index 0"),
                "unexpected error for {malformed}: {error:#}"
            );
        }

        let summary = parse_reset_credits_summary(&json!({
            "available_count": 1,
            "credits": [
                {"reset_type": "other_product"},
                {"status": "redeemed"},
                {"id": "available-with-optional-fields-omitted"}
            ]
        }))
        .unwrap();
        let credits = summary.credits.unwrap();
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].id, "available-with-optional-fields-omitted");
    }

    #[test]
    fn reset_credit_preflight_rejects_hard_blocker_and_changed_card() {
        let confirmed = ResetCredit {
            id: "confirmed".into(),
            granted_at: Some("2026-08-01T00:00:00Z".into()),
            expires_at: Some("2026-09-01T00:00:00Z".into()),
        };
        let blocked = UsageInfo {
            reset_credits: vec![confirmed.clone()],
            spend_control_reached: true,
            ..UsageInfo::default()
        };
        let blocker = validate_reset_credit_preflight("account", &blocked, &confirmed)
            .expect_err("a reset card cannot repair a hard blocker");
        assert!(format!("{blocker:#}").contains("spend_control_reached"));

        let unknown_reason = UsageInfo {
            reset_credits: vec![confirmed.clone()],
            rate_limit_reached_type: Some("future_server_reason".into()),
            ..UsageInfo::default()
        };
        let blocker = validate_reset_credit_preflight("account", &unknown_reason, &confirmed)
            .expect_err("an unknown non-empty limit reason must fail closed");
        assert!(format!("{blocker:#}").contains("future_server_reason"));

        let changed = UsageInfo {
            reset_credits: vec![ResetCredit {
                expires_at: Some("2026-10-01T00:00:00Z".into()),
                ..confirmed.clone()
            }],
            ..UsageInfo::default()
        };
        let changed = validate_reset_credit_preflight("account", &changed, &confirmed)
            .expect_err("changed metadata must invalidate the user's consent");
        assert!(format!("{changed:#}").contains("changed or disappeared"));
    }

    #[test]
    fn count_only_credit_summary_preserves_details_unless_count_is_zero() {
        let stale = ResetCredit {
            id: "embedded-credit".to_string(),
            granted_at: None,
            expires_at: None,
        };
        let mut usage = UsageInfo {
            reset_credits: vec![stale.clone()],
            ..UsageInfo::default()
        };

        merge_reset_credits(
            &mut usage,
            parse_reset_credits_summary(&json!({"available_count": 1})).unwrap(),
        );
        assert_eq!(usage.reset_credits.len(), 1);
        assert_eq!(usage.reset_credits[0].id, stale.id);

        merge_reset_credits(
            &mut usage,
            parse_reset_credits_summary(&json!({"available_count": 0})).unwrap(),
        );
        assert!(usage.reset_credits.is_empty());
    }

    #[tokio::test]
    async fn exact_credit_consume_rejects_an_empty_confirmed_id_before_io() {
        let error = consume_reset_credit_by_id(
            "account",
            Path::new("does-not-exist"),
            ResetCredit {
                id: "  ".into(),
                granted_at: None,
                expires_at: None,
            },
        )
        .await
        .unwrap_err();

        assert!(error.definitely_not_consumed());
        assert!(error.to_string().contains("missing its id"));
    }

    #[test]
    fn test_consume_outcome_only_accepts_reset() {
        let credit = ResetCredit {
            id: "credit-1".to_string(),
            granted_at: None,
            expires_at: None,
        };

        let consumed = parse_consumed_reset_credit(
            &json!({"code": "reset", "windows_reset": 2}),
            credit.clone(),
        )
        .unwrap();
        assert_eq!(consumed.code.as_deref(), Some("reset"));

        for code in ["nothing_to_reset", "no_credit", "already_redeemed"] {
            let error =
                parse_consumed_reset_credit(&json!({"code": code}), credit.clone()).unwrap_err();
            assert!(error.to_string().contains(code));
        }
    }

    #[tokio::test]
    async fn test_consume_retry_reuses_redeem_request_id() {
        let requests = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let captured = Arc::clone(&requests);
        let app = axum::Router::new().route(
            "/consume",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&captured);
                async move {
                    let request_id = body
                        .get("redeem_request_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let credit_id = body
                        .get("credit_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let attempt = {
                        let mut requests = captured.lock().unwrap();
                        requests.push((request_id, credit_id));
                        requests.len()
                    };
                    if attempt == 1 {
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        Json(json!({"code": "reset", "windows_reset": 2})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = consume_reset_credit_at_url(
            &reqwest::Client::new(),
            "access-token",
            Some("workspace-123"),
            false,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.code.as_deref(), Some("reset"));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].0.is_empty());
        assert_eq!(requests[0].0, requests[1].0);
        assert!(
            requests
                .iter()
                .all(|(_, credit_id)| credit_id == "credit-1")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consume_retry_backoff_releases_the_network_permit() {
        let (first_attempt_tx, mut first_attempt_rx) = tokio::sync::mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/consume",
            post(move || {
                let attempts = Arc::clone(&server_attempts);
                let first_attempt_tx = first_attempt_tx.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        first_attempt_tx.send(()).unwrap();
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        Json(json!({"code": "reset", "windows_reset": 2})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let limiter = Arc::new(tokio::sync::Semaphore::new(1));
        let client = reqwest::Client::new();
        let url = format!("http://{address}/consume");
        let first_permit = crate::usage::first_network_permit(limiter.clone());
        let consume = tokio::spawn(async move {
            consume_reset_credit_at_url_with_first_permit(
                &client,
                "access-token",
                Some("workspace-123"),
                false,
                ResetCredit {
                    id: "credit-1".to_string(),
                    granted_at: None,
                    expires_at: None,
                },
                &url,
                first_permit,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), first_attempt_rx.recv())
            .await
            .expect("first consume attempt did not reach the server")
            .expect("first-attempt channel closed");
        let recovered = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            limiter.clone().acquire_owned(),
        )
        .await
        .expect("consume retry backoff retained the only network permit")
        .unwrap();
        tokio::time::sleep(RETRY_DELAY + std::time::Duration::from_millis(100)).await;
        assert!(
            !consume.is_finished(),
            "the consume retry must reserve fresh capacity before its second request"
        );
        drop(recovered);

        consume.await.unwrap().unwrap();
        server.abort();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn success_with_invalid_json_is_classified_as_outcome_unknown() {
        let app =
            axum::Router::new().route("/consume", post(|| async { (StatusCode::OK, "not-json") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &reqwest::Client::new(),
            "access-token",
            None,
            false,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
    }

    #[tokio::test]
    async fn explicit_client_error_is_classified_as_not_consumed() {
        let app = axum::Router::new().route("/consume", post(|| async { StatusCode::BAD_REQUEST }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &reqwest::Client::new(),
            "access-token",
            None,
            false,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.definitely_not_consumed());
    }

    #[tokio::test]
    async fn first_success_with_non_reset_code_is_outcome_unknown() {
        let app = axum::Router::new().route(
            "/consume",
            post(|| async { Json(json!({"code": "already_redeemed"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &reqwest::Client::new(),
            "access-token",
            None,
            false,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
        let message = error.user_facing_unknown_message("account");
        assert_eq!(
            message,
            "account: reset-card consumption may have occurred; verify before retry"
        );
        assert!(!message.contains("already_redeemed"));
    }

    #[tokio::test]
    async fn invalid_response_followed_by_conflict_remains_outcome_unknown() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/consume",
            post(move || {
                let captured = Arc::clone(&captured);
                async move {
                    if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::OK, "not-json").into_response()
                    } else {
                        StatusCode::CONFLICT.into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &reqwest::Client::new(),
            "access-token",
            None,
            false,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
    }

    #[tokio::test]
    async fn invalid_response_followed_by_already_redeemed_remains_outcome_unknown() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/consume",
            post(move || {
                let captured = Arc::clone(&captured);
                async move {
                    if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::OK, "not-json").into_response()
                    } else {
                        Json(json!({"code": "already_redeemed"})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &reqwest::Client::new(),
            "access-token",
            None,
            false,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
    }
}
