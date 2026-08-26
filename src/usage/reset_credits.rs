use std::path::Path;

use anyhow::{Context, Result};
use rand::Rng;
use serde_json::Value;
use tracing::debug;

use crate::auth::{self, format_reqwest_error};

use super::api::{apply_account_routing_headers, extract_error_summary};
use super::parse::parse_optional_u64;
use super::{ConsumedResetCredit, MAX_RETRIES, RETRY_DELAY, ResetCredit, UsageInfo};

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

pub(super) async fn enrich_reset_credits(
    endpoints: &auth::ServiceEndpoints,
    alias: &str,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    usage: &mut UsageInfo,
) {
    match fetch_reset_credits(endpoints, client, access_token, account_id, is_fedramp).await {
        Ok(summary) => {
            merge_reset_credits(usage, summary);
            usage.reset_credits_error = None;
        }
        Err(err) => {
            let msg = err.to_string();
            debug!("[{alias}] reset credits fetch failed: {msg}");
            usage.reset_credits_error = Some(extract_error_summary(&msg));
        }
    }
}

async fn fetch_reset_credits(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> Result<ResetCreditsSummary> {
    fetch_reset_credits_at_url(
        client,
        access_token,
        account_id,
        is_fedramp,
        endpoints.reset_credits()?,
    )
    .await
}

async fn fetch_reset_credits_at_url(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    url: &str,
) -> Result<ResetCreditsSummary> {
    let req = client
        .get(url)
        .bearer_auth(access_token)
        .header("Accept", "application/json")
        .header("OpenAI-Beta", "codex-1")
        .header("Originator", "Codex Desktop");
    let req = apply_account_routing_headers(req, account_id, is_fedramp);

    let resp = req
        .send()
        .await
        .map_err(|e| format_reqwest_error("reset credits request failed", &e))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("reset credits request failed (HTTP {status})");
    }

    let body: Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("failed to parse reset credits response: {e}"))?;
    parse_reset_credits_summary(&body)
        .context("reset credits response does not match the expected summary shape")
}

pub(crate) fn reset_credit_expiry_sort_key(credit: &ResetCredit) -> i64 {
    credit
        .expires_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|dt| dt.timestamp())
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
) -> std::result::Result<(reqwest::Client, String, Option<String>, bool), ConsumeResetCreditError> {
    let val = auth::read_auth(profile_path).map_err(ConsumeResetCreditError::not_consumed)?;
    let (access_token, _) = auth::extract_tokens(&val);
    let access_token = access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: auth.json missing access_token"))
        .map_err(ConsumeResetCreditError::not_consumed)?;
    let account_info = crate::jwt::parse_account_info(&val);
    let client = auth::build_http_client().map_err(ConsumeResetCreditError::not_consumed)?;
    Ok((
        client,
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
    if credit.id.trim().is_empty() {
        return Err(ConsumeResetCreditError::not_consumed(anyhow::anyhow!(
            "{alias}: reset card is missing its id"
        )));
    }

    // Own the profile lease for the complete credential/network lifetime. This
    // serializes the submission with token rotation and makes rename/delete
    // wait until the exact confirmed redemption has a known outcome.
    let lease = crate::profile::acquire_profile_lease_async(alias.to_string())
        .await
        .map_err(ConsumeResetCreditError::not_consumed)?;
    consume_reset_credit_by_id_leased(alias, profile_path, credit, &lease).await
}

pub(crate) async fn consume_reset_credit_by_id_leased(
    alias: &str,
    profile_path: &Path,
    credit: ResetCredit,
    lease: &crate::profile::ProfileLease,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
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
    let (client, access_token, account_id, is_fedramp) = load_consume_context(alias, profile_path)?;
    let endpoints = auth::service_endpoints().map_err(ConsumeResetCreditError::not_consumed)?;

    send_reset_credit_consume(
        &endpoints,
        &client,
        &access_token,
        account_id.as_deref(),
        is_fedramp,
        credit,
    )
    .await
}

async fn send_reset_credit_consume(
    endpoints: &auth::ServiceEndpoints,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    credit: ResetCredit,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    consume_reset_credit_at_url(
        client,
        access_token,
        account_id,
        is_fedramp,
        credit,
        endpoints
            .reset_credits_consume()
            .map_err(ConsumeResetCreditError::not_consumed)?,
    )
    .await
}

async fn consume_reset_credit_at_url(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    credit: ResetCredit,
    url: &str,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    // Generate once per user action. Any retry after an ambiguous transport/server
    // failure must identify the same logical redemption to the backend.
    let request_id = redeem_request_id();
    let mut outcome_may_have_changed = false;
    for attempt in 0..MAX_RETRIES {
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

        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(error) if attempt + 1 < MAX_RETRIES => {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} failed before response: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    format_reqwest_error("request failed", &error)
                );
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            Err(error) => {
                return Err(ConsumeResetCreditError::outcome_unknown(
                    format_reqwest_error("reset credit consume request failed", &error),
                ));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            if (status.is_server_error() || status.as_u16() == 429) && attempt + 1 < MAX_RETRIES {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} returned HTTP {status}",
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            let error = anyhow::anyhow!("reset credit consume request failed (HTTP {status})");
            if status.is_client_error() && status.as_u16() != 429 && !outcome_may_have_changed {
                return Err(ConsumeResetCreditError::not_consumed(error));
            }
            return Err(ConsumeResetCreditError::outcome_unknown(error));
        }

        match resp.json::<Value>().await {
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
                tokio::time::sleep(RETRY_DELAY).await;
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
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    let mut available_count = count_value
        .map(|value| {
            parse_optional_u64(Some(value)).context(
                "reset credits available_count must be a non-negative integer or numeric string",
            )
        })
        .transpose()?;
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
        Some(_) => anyhow::bail!("reset credits credits field must be an array"),
        None => None,
    };
    if count_value.is_none() && credits.is_none() {
        anyhow::bail!("reset credits response missing expected fields");
    }
    if available_count.is_none() && credits.as_ref().is_some_and(Vec::is_empty) {
        available_count = Some(0);
    }

    Ok(ResetCreditsSummary {
        available_count,
        credits,
    })
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
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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
                get(|| async { Json(json!({"credits": {"id": "not-an-array"}})) }),
            )
            .route(
                "/bad-count",
                get(|| async { Json(json!({"available_count": "many"})) }),
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
        server.abort();

        assert!(format!("{bad_credits:#}").contains("credits field must be an array"));
        assert!(format!("{bad_count:#}").contains("available_count must be"));
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
    fn explicit_empty_credit_list_without_count_clears_stale_count_and_cards() {
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
            parse_reset_credits_summary(&json!({"credits": []})).unwrap(),
        );

        assert_eq!(usage.reset_credits_available_count, Some(0));
        assert!(usage.reset_credits.is_empty());
    }

    #[test]
    fn explicit_malformed_summary_fields_are_not_treated_as_omitted() {
        for malformed in [
            json!({"credits": null}),
            json!({"credits": {}}),
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
            json!({"credits": [7]}),
            json!({"credits": [{}]}),
            json!({"credits": [{"id": "  ", "status": "available"}]}),
            json!({"credits": [{"id": "credit-1", "status": 1}]}),
            json!({"credits": [{"id": "credit-1", "expires_at": 123}]}),
        ] {
            let error = parse_reset_credits_summary(&malformed)
                .expect_err("a malformed explicit card must reject the summary");
            assert!(
                format!("{error:#}").contains("invalid reset credit at index 0"),
                "unexpected error for {malformed}: {error:#}"
            );
        }

        let summary = parse_reset_credits_summary(&json!({
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
