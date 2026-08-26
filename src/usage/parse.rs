use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, warn};

use crate::auth;

use super::reset_credits::parse_reset_credits_summary;
use super::{AdditionalRateLimit, UsageInfo, UsageParseIssue, WindowUsage};

const SECS_7D: i64 = 7 * 86_400;
const SECONDS_PER_MINUTE: i64 = 60;

pub(super) fn parse_optional_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_window_checked(val: &Value, path: &str) -> Result<WindowUsage> {
    let object = val
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{path} must be an object"))?;
    // Require used_percent to be present for meaningful scoring data. A window
    // with only reset_at would otherwise look like a fully available 0% window.
    let used_percent = object
        .get("used_percent")
        .and_then(Value::as_f64)
        .filter(|used| used.is_finite() && (0.0..=100.0).contains(used))
        .ok_or_else(|| {
            anyhow::anyhow!("{path}.used_percent must be a number from 0 through 100")
        })?;
    let resets_at = match object.get("reset_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("{path}.reset_at must be an integer or null"))?,
        ),
    };
    let window_minutes =
        parse_window_minutes(object.get("limit_window_seconds")).ok_or_else(|| {
            anyhow::anyhow!(
                "{path}.limit_window_seconds must be an integer of at least 60 seconds or null"
            )
        })?;

    Ok(WindowUsage {
        used_percent: Some(used_percent),
        resets_at,
        window_minutes,
    })
}

fn parse_window_retaining_issue(
    value: Option<&Value>,
    path: &str,
    issue: fn(String) -> UsageParseIssue,
    issues: &mut Vec<UsageParseIssue>,
) -> Option<WindowUsage> {
    let value = value?;
    match parse_window_checked(value, path) {
        Ok(window) => Some(window),
        Err(error) => {
            issues.push(issue(format!("{error:#}")));
            None
        }
    }
}

fn invalid_primary_window(detail: String) -> UsageParseIssue {
    UsageParseIssue::InvalidPrimaryWindow { detail }
}

fn invalid_secondary_window(detail: String) -> UsageParseIssue {
    UsageParseIssue::InvalidSecondaryWindow { detail }
}

/// Convert the API's integer-second duration into `WindowUsage`'s whole-minute
/// representation. A positive sub-minute remainder is deliberately truncated;
/// requiring exact divisibility would invent an upstream schema constraint.
fn parse_window_minutes(value: Option<&Value>) -> Option<Option<i64>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => {
            let seconds = value.as_i64()?;
            if seconds < SECONDS_PER_MINUTE {
                return None;
            }
            seconds
                .checked_div(SECONDS_PER_MINUTE)
                .filter(|minutes| *minutes > 0)
                .map(Some)
        }
    }
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => anyhow::bail!("{path}.{key} must be a string when present"),
    }
}

fn optional_bool(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<bool>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => anyhow::bail!("{path}.{key} must be a boolean when present"),
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct RateLimitFlags {
    allowed: Option<bool>,
    limit_reached: Option<bool>,
}

/// Keep the public parser compatible with stored legacy responses that did not
/// include a plan. The checked network parser separately enforces the current
/// raw response contract, where `plan_type` is required.
fn parse_plan_type(body: &Value) -> std::result::Result<Option<String>, UsageParseIssue> {
    match body.get("plan_type") {
        None => Ok(None),
        Some(Value::String(plan_type)) => Ok(Some(plan_type.clone())),
        Some(_) => Err(UsageParseIssue::InvalidPlanType {
            detail: "must be a string when present".to_string(),
        }),
    }
}

/// Validate the scalar availability flags without requiring them from legacy
/// responses that predate the current backend schema.
fn parse_rate_limit_flags(body: &Value) -> std::result::Result<RateLimitFlags, UsageParseIssue> {
    let object = match body.get("rate_limit") {
        None | Some(Value::Null) => return Ok(RateLimitFlags::default()),
        Some(Value::Object(object)) => object,
        Some(_) => {
            return Err(UsageParseIssue::InvalidRateLimit {
                detail: "must be an object or null when present".to_string(),
            });
        }
    };
    let allowed = optional_bool(object, "allowed", "rate_limit").map_err(|error| {
        UsageParseIssue::InvalidRateLimit {
            detail: format!("{error:#}"),
        }
    })?;
    let limit_reached = optional_bool(object, "limit_reached", "rate_limit").map_err(|error| {
        UsageParseIssue::InvalidRateLimit {
            detail: format!("{error:#}"),
        }
    })?;
    Ok(RateLimitFlags {
        allowed,
        limit_reached,
    })
}

fn parse_spend_control_reached(body: &Value) -> std::result::Result<Option<bool>, UsageParseIssue> {
    let object = match body.get("spend_control") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(object)) => object,
        Some(_) => {
            return Err(UsageParseIssue::InvalidSpendControl {
                detail: "must be an object or null when present".to_string(),
            });
        }
    };
    let reached = optional_bool(object, "reached", "spend_control").map_err(|error| {
        UsageParseIssue::InvalidSpendControl {
            detail: format!("{error:#}"),
        }
    })?;
    match object.get("individual_limit") {
        None | Some(Value::Null | Value::Object(_)) => {}
        Some(_) => {
            return Err(UsageParseIssue::InvalidSpendControl {
                detail: "spend_control.individual_limit must be an object or null when present"
                    .to_string(),
            });
        }
    }
    Ok(reached)
}

fn validate_credits(body: &Value) -> Result<()> {
    let credits = match body.get("credits") {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Object(credits)) => credits,
        Some(_) => anyhow::bail!("credits must be an object or null when present"),
    };

    optional_bool(credits, "has_credits", "credits")?;
    optional_bool(credits, "unlimited", "credits")?;
    match credits.get("balance") {
        None | Some(Value::Null) => {}
        Some(Value::Number(balance)) if balance.as_f64().is_some_and(f64::is_finite) => {}
        Some(Value::String(balance))
            if balance
                .parse::<f64>()
                .is_ok_and(|balance| balance.is_finite()) => {}
        Some(_) => {
            anyhow::bail!("credits.balance must be a finite number, numeric string, or null")
        }
    }
    for key in ["approx_local_messages", "approx_cloud_messages"] {
        match credits.get(key) {
            None | Some(Value::Null | Value::Array(_)) => {}
            Some(_) => anyhow::bail!("credits.{key} must be an array or null when present"),
        }
    }
    Ok(())
}

fn optional_window(
    object: &serde_json::Map<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<WindowUsage>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse_window_checked(value, &format!("{path}.{key}")).map(Some),
    }
}

fn parse_additional_rate_limit_item(
    item: &Value,
    path: &str,
    allow_direct_rate_limit: bool,
) -> Result<AdditionalRateLimit> {
    let item_object = item
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{path} must be an object"))?;
    let limit_name = optional_string(item_object, "limit_name", path)?;
    let metered_feature = optional_string(item_object, "metered_feature", path)?;
    if !allow_direct_rate_limit && (limit_name.is_none() || metered_feature.is_none()) {
        anyhow::bail!("{path} must contain string limit_name and metered_feature");
    }
    let (rate_limit, rate_limit_path) = if allow_direct_rate_limit {
        match item_object.get("rate_limit") {
            None => (Some(item), path.to_string()),
            Some(value) => (Some(value), format!("{path}.rate_limit")),
        }
    } else {
        match item_object.get("rate_limit") {
            None | Some(Value::Null) => (None, format!("{path}.rate_limit")),
            Some(value) => (Some(value), format!("{path}.rate_limit")),
        }
    };
    let Some(rate_limit) = rate_limit else {
        return Ok(AdditionalRateLimit {
            limit_name,
            metered_feature,
            allowed: None,
            limit_reached: None,
            primary: None,
            secondary: None,
        });
    };
    let rate_limit_object = rate_limit
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{rate_limit_path} must be an object"))?;
    let primary = optional_window(rate_limit_object, "primary_window", &rate_limit_path)?;
    let secondary = optional_window(rate_limit_object, "secondary_window", &rate_limit_path)?;
    let (primary, secondary) = if primary
        .as_ref()
        .and_then(|window| window.window_minutes)
        .is_some_and(|minutes| minutes.saturating_mul(60) >= SECS_7D)
        && secondary.is_none()
    {
        (None, primary)
    } else {
        (primary, secondary)
    };
    let allowed = optional_bool(rate_limit_object, "allowed", &rate_limit_path)?;
    let limit_reached = optional_bool(rate_limit_object, "limit_reached", &rate_limit_path)?;
    if allowed.is_none() && limit_reached.is_none() && primary.is_none() && secondary.is_none() {
        anyhow::bail!(
            "{rate_limit_path} must contain allowed, limit_reached, primary_window, or secondary_window"
        );
    }
    Ok(AdditionalRateLimit {
        limit_name,
        metered_feature,
        allowed,
        limit_reached,
        primary,
        secondary,
    })
}

fn parse_additional_rate_limits(body: &Value) -> Result<Vec<AdditionalRateLimit>> {
    let items = match body.get("additional_rate_limits") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => anyhow::bail!("must be an array or null when present"),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            parse_additional_rate_limit_item(
                item,
                &format!("additional_rate_limits[{index}]"),
                false,
            )
        })
        .collect()
}

fn parse_code_review_rate_limit(body: &Value) -> Result<Option<AdditionalRateLimit>> {
    let review = match body.get("code_review_rate_limit") {
        None | Some(Value::Null) => return Ok(None),
        Some(review) => review,
    };
    let mut limit = parse_additional_rate_limit_item(review, "code_review_rate_limit", true)?;
    limit.limit_name = Some("Code review".to_string());
    limit.metered_feature = Some("code_review".to_string());
    Ok(Some(limit))
}

fn invalid_rate_limit_reason(value: &Value, detail: impl Into<String>) -> UsageParseIssue {
    let raw = value
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| value.to_string());
    UsageParseIssue::InvalidRateLimitReachedType {
        raw,
        detail: detail.into(),
    }
}

fn rate_limit_reached_type(body: &Value) -> std::result::Result<Option<String>, UsageParseIssue> {
    let value = match body.get("rate_limit_reached_type") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let reason = match value {
        Value::String(reason) => reason,
        Value::Object(object) => object.get("type").and_then(Value::as_str).ok_or_else(|| {
            invalid_rate_limit_reason(value, "object must contain a string `type`")
        })?,
        _ => {
            return Err(invalid_rate_limit_reason(
                value,
                "must be a non-empty string or an object containing string `type`",
            ));
        }
    };
    if reason.trim().is_empty() {
        return Err(invalid_rate_limit_reason(value, "reason must not be empty"));
    }
    Ok(Some(reason.to_string()))
}

fn reset_credits_summary_value(body: &Value) -> Option<&Value> {
    let value = match body.get("rate_limit_reset_credits") {
        Some(value) => Some(value),
        None => body.get("rateLimitResetCredits"),
    };
    value.filter(|value| !value.is_null())
}

fn validate_usage_response_schema(body: &Value) -> Result<()> {
    match parse_plan_type(body).map_err(|issue| anyhow::anyhow!(issue.to_string()))? {
        Some(_) => {}
        None => anyhow::bail!("plan_type: is required"),
    }

    for (name, pointer) in [
        ("primary_window", "/rate_limit/primary_window"),
        ("secondary_window", "/rate_limit/secondary_window"),
    ] {
        if let Some(window) = body.pointer(pointer).filter(|value| !value.is_null()) {
            parse_window_checked(window, name)
                .with_context(|| format!("usage response contains invalid {name}"))?;
        }
    }
    parse_rate_limit_flags(body).map_err(|issue| anyhow::anyhow!(issue.to_string()))?;
    validate_credits(body).context("usage response contains invalid credits")?;
    parse_spend_control_reached(body).map_err(|issue| anyhow::anyhow!(issue.to_string()))?;
    rate_limit_reached_type(body).map_err(|issue| anyhow::anyhow!(issue.to_string()))?;
    parse_additional_rate_limits(body)
        .context("usage response contains invalid additional_rate_limits")?;
    parse_code_review_rate_limit(body)
        .context("usage response contains invalid code_review_rate_limit")?;
    if let Some(summary) = reset_credits_summary_value(body) {
        parse_reset_credits_summary(summary)
            .context("usage response contains an invalid reset credits summary")?;
    }
    Ok(())
}

pub(super) fn parse_usage_checked(body: &Value) -> Result<UsageInfo> {
    validate_usage_response_schema(body)?;
    parse_usage(body)
}

pub fn parse_usage(body: &Value) -> Result<UsageInfo> {
    Ok(parse_usage_at(body, auth::now_unix_secs()?))
}

fn parse_usage_at(body: &Value, fetched_at: i64) -> UsageInfo {
    let mut parse_issues = Vec::new();
    let primary_raw = body
        .pointer("/rate_limit/primary_window")
        .filter(|v| !v.is_null());

    let secondary_raw = body
        .pointer("/rate_limit/secondary_window")
        .filter(|v| !v.is_null());

    let primary_window_secs = primary_raw
        .and_then(|v| v.get("limit_window_seconds"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let primary_parsed = parse_window_retaining_issue(
        primary_raw,
        "primary_window",
        invalid_primary_window,
        &mut parse_issues,
    );
    let secondary_parsed = parse_window_retaining_issue(
        secondary_raw,
        "secondary_window",
        invalid_secondary_window,
        &mut parse_issues,
    );

    // A weekly-only response places its 7d window in the primary_window slot.
    // Normalize by duration so every consumer reads weekly quota from secondary.
    let (primary, secondary) = if primary_window_secs >= SECS_7D && secondary_parsed.is_none() {
        debug!("parse_usage: primary_window is weekly — remapping to secondary");
        (None, primary_parsed)
    } else {
        if secondary_raw.is_some() && secondary_parsed.is_none() {
            warn!(
                "parse_usage: secondary_window present but failed validation: {:?}",
                secondary_raw
            );
        }
        (primary_parsed, secondary_parsed)
    };

    debug!(
        "parse_usage: primary={} secondary={}",
        primary.is_some(),
        secondary.is_some()
    );

    // has_credits=false means no pay-per-use credits (Plus/Pro included usage only).
    // Default true for old API format which lacked this field.
    let has_credits = body
        .pointer("/credits/has_credits")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // balance changed from number to string "0" in new API — handle both.
    // Skip entirely when has_credits=false to avoid showing "$0.00" for accounts
    // that simply don't use the pay-per-use credits system.
    let credits_balance = if has_credits {
        body.pointer("/credits/balance").and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    } else {
        None
    };

    let unlimited_credits = body.pointer("/credits/unlimited").and_then(|v| v.as_bool());

    let plan_type = match parse_plan_type(body) {
        Ok(plan_type) => plan_type,
        Err(issue) => {
            parse_issues.push(issue);
            None
        }
    };
    let rate_limit_flags = match parse_rate_limit_flags(body) {
        Ok(flags) => flags,
        Err(issue) => {
            parse_issues.push(issue);
            RateLimitFlags::default()
        }
    };
    if let Err(error) = validate_credits(body) {
        parse_issues.push(UsageParseIssue::InvalidCredits {
            detail: format!("{error:#}"),
        });
    }
    let spend_control_reached = match parse_spend_control_reached(body) {
        Ok(reached) => reached.unwrap_or(false),
        Err(issue) => {
            parse_issues.push(issue);
            false
        }
    };
    let rate_limit_reached_type = match rate_limit_reached_type(body) {
        Ok(reason) => reason,
        Err(issue) => {
            let raw = match &issue {
                UsageParseIssue::InvalidRateLimitReachedType { raw, .. } => raw.clone(),
                _ => unreachable!("rate-limit parser returned a different issue kind"),
            };
            parse_issues.push(issue);
            Some(raw)
        }
    };
    let mut account_limited = rate_limit_reached_type.is_some()
        || spend_control_reached
        || rate_limit_flags.allowed == Some(false)
        || rate_limit_flags.limit_reached == Some(true);
    let (reset_credits_summary, reset_credits_error) = match reset_credits_summary_value(body) {
        Some(summary) => match parse_reset_credits_summary(summary) {
            Ok(summary) => (Some(summary), None),
            Err(error) => (
                None,
                Some(format!("invalid embedded reset credits summary: {error:#}")),
            ),
        },
        None => (None, None),
    };
    let reset_credits_available_count = reset_credits_summary
        .as_ref()
        .and_then(|summary| summary.available_count);
    let reset_credits = reset_credits_summary
        .and_then(|summary| summary.credits)
        .unwrap_or_default();

    let mut additional_limits = match parse_additional_rate_limits(body) {
        Ok(limits) => limits,
        Err(error) => {
            parse_issues.push(UsageParseIssue::InvalidAdditionalRateLimits {
                detail: format!("{error:#}"),
            });
            Vec::new()
        }
    };
    match parse_code_review_rate_limit(body) {
        Ok(Some(limit)) => additional_limits.push(limit),
        Ok(None) => {}
        Err(error) => parse_issues.push(UsageParseIssue::InvalidCodeReviewRateLimit {
            detail: format!("{error:#}"),
        }),
    }
    account_limited |= !parse_issues.is_empty();
    let individual_limit = body
        .pointer("/spend_control/individual_limit")
        .filter(|limit| !limit.is_null())
        .and_then(Value::as_object)
        .map(|limit| {
            let string_value = |key: &str| {
                limit.get(key).and_then(|value| {
                    value
                        .as_str()
                        .map(String::from)
                        .or_else(|| value.as_f64().map(|number| number.to_string()))
                })
            };
            Box::new(super::SpendControlLimit {
                source: string_value("source"),
                limit: string_value("limit"),
                used: string_value("used"),
                remaining: string_value("remaining"),
                remaining_percent: limit.get("remaining_percent").and_then(Value::as_f64),
                resets_at: limit.get("reset_at").and_then(Value::as_i64),
            })
        });

    UsageInfo {
        fetched_at: Some(fetched_at),
        primary,
        secondary,
        credits_balance,
        unlimited_credits,
        plan_type,
        reset_credits_available_count,
        reset_credits,
        reset_credits_error,
        account_limited,
        spend_control_reached,
        rate_limit_reached_type,
        individual_limit,
        additional_limits,
        parse_issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use serde_json::json;

    fn parse_usage(body: &Value) -> UsageInfo {
        super::parse_usage(body).expect("test clock must produce a supported Unix timestamp")
    }

    #[test]
    fn test_parse_usage_full_response() {
        let primary_reset = DateTime::parse_from_rfc3339("2026-03-26T10:00:00Z")
            .unwrap()
            .timestamp();
        let secondary_reset = DateTime::parse_from_rfc3339("2026-03-30T00:00:00Z")
            .unwrap()
            .timestamp();
        let body = json!({
            "rate_limit": {
                "primary_window": {
                    "remaining_seconds": 3600,
                    "requests_remaining": 50,
                    "requests_limit": 100,
                    "reset_time": "2026-03-26T10:00:00Z",
                    "used_percent": 50.0,
                    "reset_at": primary_reset
                },
                "secondary_window": {
                    "remaining_seconds": 86400,
                    "requests_remaining": 200,
                    "requests_limit": 500,
                    "reset_time": "2026-03-30T00:00:00Z",
                    "used_percent": 60.0,
                    "reset_at": secondary_reset
                }
            },
            "credits": {
                "balance": 15.50,
                "unlimited": false
            },
            "rate_limit_reset_credits": {
                "available_count": "2"
            }
        });

        let before = auth::now_unix_secs().unwrap();
        let usage = parse_usage(&body);
        let after = auth::now_unix_secs().unwrap();

        assert!(matches!(usage.fetched_at, Some(ts) if ts >= before && ts <= after));
        assert_eq!(
            usage.primary.as_ref().and_then(|w| w.used_percent),
            Some(50.0)
        );
        assert_eq!(
            usage.primary.as_ref().and_then(|w| w.resets_at),
            Some(primary_reset)
        );
        assert_eq!(
            usage.secondary.as_ref().and_then(|w| w.used_percent),
            Some(60.0)
        );
        assert_eq!(
            usage.secondary.as_ref().and_then(|w| w.resets_at),
            Some(secondary_reset)
        );
        assert_eq!(usage.credits_balance, Some(15.5));
        assert_eq!(usage.unlimited_credits, Some(false));
        assert_eq!(usage.reset_credits_available_count, Some(2));
    }

    #[test]
    fn test_parse_usage_reset_credit_details() {
        let usage = parse_usage(&json!({
            "rate_limit_reset_credits": {
                "available_count": 2,
                "credits": [
                    {
                        "id": "cred_1",
                        "reset_type": "codex_rate_limits",
                        "status": "available",
                        "granted_at": "2026-07-01T00:00:00Z",
                        "expires_at": "2026-07-08T00:00:00Z"
                    },
                    {
                        "id": "cred_2",
                        "reset_type": "codex_rate_limits",
                        "status": "consumed",
                        "expires_at": "2026-07-08T00:00:00Z"
                    }
                ]
            }
        }));

        assert_eq!(usage.reset_credits_available_count, Some(2));
        assert_eq!(usage.reset_credits.len(), 1);
        assert_eq!(usage.reset_credits[0].id, "cred_1");
        assert_eq!(
            usage.reset_credits[0].expires_at.as_deref(),
            Some("2026-07-08T00:00:00Z")
        );
    }

    #[test]
    fn test_parse_usage_unlimited_credits() {
        let usage = parse_usage(&json!({
            "credits": {
                "balance": 15.50,
                "unlimited": true
            }
        }));

        assert_eq!(usage.credits_balance, Some(15.5));
        assert_eq!(usage.unlimited_credits, Some(true));
    }

    #[test]
    fn test_parse_usage_no_credits() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 25.0,
                    "reset_at": 123
                }
            }
        }));

        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
    }

    #[test]
    fn test_parse_usage_has_credits_false_hides_balance() {
        // New API: plus accounts return has_credits=false with balance="0" (string).
        // We must NOT show $0.00 for these accounts.
        let usage = parse_usage(&json!({
            "plan_type": "plus",
            "credits": {
                "has_credits": false,
                "unlimited": false,
                "balance": "0"
            }
        }));

        assert_eq!(
            usage.credits_balance, None,
            "has_credits=false must suppress balance"
        );
        assert_eq!(usage.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn test_parse_usage_balance_string() {
        // New API: balance is a string when has_credits=true
        let usage = parse_usage(&json!({
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "5.25"
            }
        }));

        assert_eq!(usage.credits_balance, Some(5.25));
    }

    #[test]
    fn test_parse_usage_free_account_single_window() {
        // New API: free accounts have one 7d window in primary_window slot.
        // Must be remapped to secondary so scoring treats it as 7d data.
        let usage = parse_usage(&json!({
            "plan_type": "free",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {
                    "used_percent": 100,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 437896,
                    "reset_at": 1778468889i64
                },
                "secondary_window": null
            }
        }));

        assert!(
            usage.primary.is_none(),
            "free account must have no 5h window"
        );
        assert!(
            usage.secondary.is_some(),
            "free account 7d data must be in secondary"
        );
        assert_eq!(
            usage.secondary.as_ref().and_then(|w| w.used_percent),
            Some(100.0)
        );
        assert_eq!(usage.plan_type.as_deref(), Some("free"));
    }

    #[test]
    fn test_parse_usage_null_windows() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "primary_window": null,
                "secondary_window": null
            }
        }));

        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn test_parse_usage_empty_response() {
        let usage = parse_usage(&json!({}));

        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
    }

    #[test]
    fn test_checked_usage_rejects_empty_or_drifted_response() {
        assert!(parse_usage_checked(&json!({})).is_err());
        assert!(parse_usage_checked(&json!({"unexpected": true})).is_err());
        assert!(parse_usage_checked(&json!({"plan_type": null})).is_err());
    }

    #[test]
    fn test_checked_usage_empty_object_error_names_required_plan() {
        let err = parse_usage_checked(&json!({})).expect_err("empty body must be rejected");
        assert!(
            err.to_string().contains("plan_type: is required"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn test_checked_usage_drifted_response_is_rejected() {
        let err = parse_usage_checked(&json!({
            "some_new_field": "unrecognized",
            "another": { "nested": 1 }
        }))
        .expect_err("structurally drifted body must be rejected");
        assert!(
            err.to_string().contains("plan_type: is required"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn checked_usage_accepts_valid_all_null_quota_payload_as_unavailable() {
        let usage = parse_usage_checked(&json!({
            "plan_type": "pro",
            "rate_limit": null,
            "credits": null,
            "spend_control": null,
            "additional_rate_limits": null,
            "rate_limit_reached_type": null
        }))
        .expect("nullable optional quota fields are a valid raw response");

        assert_eq!(usage.plan_type.as_deref(), Some("pro"));
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert!(usage.additional_limits.is_empty());
        assert!(!usage.account_limited);
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn checked_usage_accepts_additional_pool_only_payload_as_main_quota_unavailable() {
        let usage = parse_usage_checked(&json!({
            "plan_type": "pro",
            "rate_limit": null,
            "credits": null,
            "spend_control": null,
            "additional_rate_limits": [{
                "limit_name": "Extra pool",
                "metered_feature": "extra_pool",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {
                        "used_percent": 12,
                        "limit_window_seconds": 18_000
                    },
                    "secondary_window": null
                }
            }],
            "rate_limit_reached_type": null
        }))
        .expect("an additional pool does not imply that the main quota exists");

        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert_eq!(usage.additional_limits.len(), 1);
        assert_eq!(
            usage.additional_limits[0].metered_feature.as_deref(),
            Some("extra_pool")
        );
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn checked_usage_validates_non_null_credit_fields() {
        let valid = parse_usage_checked(&json!({
            "plan_type": "plus",
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "5.25",
                "approx_local_messages": null,
                "approx_cloud_messages": []
            }
        }))
        .expect("the supported credits shape must remain valid");
        assert_eq!(valid.credits_balance, Some(5.25));

        for credits in [
            json!("invalid"),
            json!([]),
            json!({"has_credits": "yes"}),
            json!({"unlimited": null}),
            json!({"balance": "not-a-number"}),
            json!({"balance": {}}),
            json!({"approx_local_messages": {}}),
            json!({"approx_cloud_messages": false}),
        ] {
            let body = json!({
                "plan_type": "plus",
                "credits": credits
            });
            let error = parse_usage_checked(&body)
                .expect_err("malformed non-null credits must be rejected");
            assert!(
                format!("{error:#}").contains("invalid credits"),
                "unexpected error for {body}: {error:#}"
            );
            let usage = parse_usage(&body);
            assert!(matches!(
                usage.parse_issues.as_slice(),
                [UsageParseIssue::InvalidCredits { .. }]
            ));
            assert!(usage.account_limited);
        }
    }

    #[test]
    fn test_parse_usage_marks_known_rate_limit_reached_type_as_limited() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 10.0}
            },
            "rate_limit_reached_type": {
                "type": "workspace_member_usage_limit_reached"
            }
        }));

        assert!(usage.account_limited);
        assert!(!usage.spend_control_reached);
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn checked_usage_rejects_out_of_range_percentages() {
        for used_percent in [-0.01, 100.01] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": used_percent,
                        "limit_window_seconds": 18_000
                    }
                }
            });
            let error = parse_usage_checked(&body)
                .expect_err("an invalid percentage must not become a scoring candidate");
            assert!(
                error.to_string().contains("invalid primary_window"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn checked_usage_rejects_nonpositive_or_unrepresentable_window_durations() {
        for seconds in [-60, 0, 59] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25,
                        "limit_window_seconds": seconds
                    }
                }
            });
            assert!(
                parse_usage_checked(&body).is_err(),
                "{seconds} seconds cannot be represented as a positive whole-minute window"
            );
        }

        let wrong_type = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": "18000"
                }
            }
        });
        assert!(parse_usage_checked(&wrong_type).is_err());

        let omitted_metadata = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": null
                }
            }
        });
        assert!(parse_usage_checked(&omitted_metadata).is_ok());
    }

    #[test]
    fn checked_usage_accepts_non_whole_minute_durations_with_explicit_truncation() {
        for (seconds, expected_minutes) in [(60, 1), (61, 1), (119, 1), (120, 2)] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25,
                        "limit_window_seconds": seconds
                    }
                }
            });

            let usage = parse_usage_checked(&body)
                .unwrap_or_else(|error| panic!("{seconds} seconds must be accepted: {error:#}"));
            assert_eq!(
                usage
                    .primary
                    .as_ref()
                    .and_then(|window| window.window_minutes),
                Some(expected_minutes),
                "the whole-minute model truncates only the sub-minute remainder"
            );
        }
    }

    #[test]
    fn one_valid_window_does_not_mask_a_malformed_sibling() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": -20,
                    "limit_window_seconds": 18_000
                },
                "secondary_window": {
                    "used_percent": 40,
                    "limit_window_seconds": 604_800
                }
            }
        });

        assert!(
            parse_usage_checked(&body).is_err(),
            "a malformed primary window must not be hidden by valid weekly data"
        );

        let usage = parse_usage(&body);
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_some());
        assert!(usage.account_limited);
        assert!(matches!(
            usage.parse_issues.as_slice(),
            [UsageParseIssue::InvalidPrimaryWindow { detail }]
                if detail.contains("used_percent")
        ));
        assert!(matches!(
            crate::usage::explicit_account_blocker(&usage),
            Some(crate::usage::ExplicitAccountBlocker::MalformedUsageResponse(_))
        ));
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn checked_usage_rejects_explicit_malformed_reset_credit_summary() {
        for malformed in [json!({"credits": null}), json!({"available_count": "many"})] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {
                    "primary_window": {"used_percent": 10.0}
                },
                "rate_limit_reset_credits": malformed
            });
            let error = parse_usage_checked(&body)
                .expect_err("an explicit malformed reset credit summary must fail closed");
            assert!(
                format!("{error:#}").contains("invalid reset credits summary"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn infallible_usage_parser_preserves_explicit_reset_credit_errors() {
        for malformed in [
            json!({"credits": null}),
            json!({"available_count": "many"}),
            json!({"credits": [{}]}),
        ] {
            let usage = parse_usage(&json!({
                "rate_limit": {
                    "primary_window": {"used_percent": 10.0}
                },
                "rate_limit_reset_credits": malformed
            }));

            assert_eq!(usage.primary.unwrap().used_percent, Some(10.0));
            assert_eq!(usage.reset_credits_available_count, None);
            assert!(usage.reset_credits.is_empty());
            assert!(
                usage.reset_credits_error.as_deref().is_some_and(|error| {
                    error.contains("invalid embedded reset credits summary")
                }),
                "an explicit malformed summary must remain visible: {malformed}"
            );
        }
    }

    #[test]
    fn production_nullable_shape_remaps_single_weekly_window_without_parse_issues() {
        let usage = parse_usage_checked(&json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 98.0,
                    "limit_window_seconds": 604800,
                    "reset_at": 1_800_000_000i64
                },
                "secondary_window": null
            },
            "credits": null,
            "spend_control": {
                "reached": false,
                "individual_limit": null
            },
            "additional_rate_limits": null,
            "code_review_rate_limit": null,
            "rate_limit_reached_type": null,
            "rate_limit_reset_credits": null
        }))
        .expect("nullable optional fields must have the same absence semantics as omission");

        assert!(!usage.account_limited);
        assert_eq!(usage.plan_type.as_deref(), Some("pro"));
        assert!(usage.primary.is_none());
        assert_eq!(
            usage
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(98.0)
        );
        assert_eq!(
            usage
                .secondary
                .as_ref()
                .and_then(|window| window.window_minutes),
            Some(10_080)
        );
        assert_eq!(usage.rate_limit_reached_type, None);
        assert!(usage.additional_limits.is_empty());
        assert!(usage.individual_limit.is_none());
        assert_eq!(usage.reset_credits_available_count, None);
        assert!(usage.reset_credits.is_empty());
        assert_eq!(usage.reset_credits_error, None);
        assert!(usage.parse_issues.is_empty());
    }

    #[test]
    fn nullable_embedded_reset_credit_summary_is_absent_for_both_supported_names() {
        for field in ["rate_limit_reset_credits", "rateLimitResetCredits"] {
            let mut body = json!({
                "plan_type": "plus",
                "rate_limit": {"primary_window": {"used_percent": 10.0}}
            });
            body[field] = Value::Null;

            let usage = parse_usage_checked(&body)
                .expect("an explicitly null embedded summary must be treated as absent");
            assert_eq!(usage.reset_credits_error, None, "field: {field}");
            assert_eq!(usage.reset_credits_available_count, None, "field: {field}");
            assert!(usage.reset_credits.is_empty(), "field: {field}");
        }
    }

    #[test]
    fn test_parse_usage_keeps_weekly_exhaustion_distinct_from_spend_control() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "secondary_window": {
                    "used_percent": 100.0,
                    "limit_window_seconds": 604800,
                    "reset_at": 1_800_000_000i64
                }
            },
            "spend_control": {
                "reached": false,
                "individual_limit": {"remaining_percent": 68.0}
            }
        }));

        assert!(usage.account_limited);
        assert!(!usage.spend_control_reached);
        assert!(usage.individual_limit.is_some());
    }

    #[test]
    fn test_parse_usage_marks_reached_spend_control_as_limited() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 10.0}
            },
            "spend_control": {"reached": true}
        }));

        assert!(usage.account_limited);
        assert!(usage.spend_control_reached);
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn test_parse_usage_additional_rate_limits_parsed_alongside_top_level_window() {
        // Real production shape (Pro 20x account, sanitized). Top-level 42%/84%
        // plus an additional_rate_limits item with its own independent windows.
        // A sibling `code_review_rate_limit` key (observed null) must not break parsing.
        let body = json!({
            "rate_limit": {
                "primary_window": {"used_percent": 42.0, "reset_at": 1000},
                "secondary_window": {"used_percent": 84.0, "reset_at": 2000}
            },
            "code_review_rate_limit": null,
            "additional_rate_limits": [
                {
                    "limit_name": "GPT-5.3-Codex-Spark",
                    "metered_feature": "codex_bengalfox",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {
                            "used_percent": 0,
                            "limit_window_seconds": 18000,
                            "reset_after_seconds": 18000,
                            "reset_at": 1783843614i64
                        },
                        "secondary_window": {
                            "used_percent": 0,
                            "limit_window_seconds": 604800,
                            "reset_after_seconds": 604800,
                            "reset_at": 1784430414i64
                        }
                    }
                }
            ]
        });

        let usage = parse_usage(&body);

        // Top-level primary window unaffected by additional_rate_limits presence.
        assert_eq!(
            usage.primary.as_ref().and_then(|w| w.used_percent),
            Some(42.0)
        );
        assert_eq!(
            usage.secondary.as_ref().and_then(|w| w.used_percent),
            Some(84.0)
        );

        assert_eq!(usage.additional_limits.len(), 1);
        let extra = &usage.additional_limits[0];
        assert_eq!(extra.metered_feature.as_deref(), Some("codex_bengalfox"));
        assert_eq!(extra.limit_name.as_deref(), Some("GPT-5.3-Codex-Spark"));
        assert_eq!(extra.allowed, Some(true));
        assert_eq!(extra.limit_reached, Some(false));
        assert_eq!(
            extra.primary.as_ref().and_then(|w| w.used_percent),
            Some(0.0)
        );
        assert_eq!(
            extra.primary.as_ref().and_then(|w| w.resets_at),
            Some(1783843614i64)
        );
        assert_eq!(
            extra.primary.as_ref().and_then(|w| w.window_minutes),
            Some(300)
        );
        assert_eq!(
            extra.secondary.as_ref().and_then(|w| w.used_percent),
            Some(0.0)
        );
        assert_eq!(
            extra.secondary.as_ref().and_then(|w| w.resets_at),
            Some(1784430414i64)
        );
    }

    #[test]
    fn test_parse_usage_remaps_additional_primary_only_seven_day_window() {
        let body = serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 10.0}},
            "additional_rate_limits": [{
                "limit_name": "GPT-5.3-Codex-Spark",
                "metered_feature": "codex_bengalfox",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 8.0,
                        "limit_window_seconds": 604800,
                        "reset_at": 1784430414i64
                    },
                    "secondary_window": null
                }
            }]
        });

        let usage = parse_usage(&body);
        let spark = &usage.additional_limits[0];
        assert!(
            spark.primary.is_none(),
            "a seven-day window is not a 5h primary window"
        );
        assert_eq!(
            spark
                .secondary
                .as_ref()
                .and_then(|window| window.window_minutes),
            Some(10_080)
        );
    }

    #[test]
    fn test_parse_usage_preserves_code_review_and_individual_limit_details() {
        let usage = parse_usage(&json!({
            "rate_limit": {"primary_window": {"used_percent": 10.0}},
            "rate_limit_reached_type": {"type": "workspace_member_usage_limit_reached"},
            "spend_control": {
                "individual_limit": {
                    "source": "workspace_spend_controls",
                    "limit": "25000",
                    "used": "8000",
                    "remaining": "17000",
                    "remaining_percent": 68,
                    "reset_at": 1784430414i64
                }
            },
            "code_review_rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": 86400,
                    "reset_at": 1784430414i64
                }
            }
        }));

        assert_eq!(
            usage.rate_limit_reached_type.as_deref(),
            Some("workspace_member_usage_limit_reached")
        );
        let limit = usage.individual_limit.expect("individual limit");
        assert_eq!(limit.limit.as_deref(), Some("25000"));
        assert_eq!(limit.used.as_deref(), Some("8000"));
        assert_eq!(limit.remaining.as_deref(), Some("17000"));
        assert_eq!(limit.remaining_percent, Some(68.0));
        assert_eq!(limit.resets_at, Some(1784430414i64));
        assert_eq!(usage.additional_limits.len(), 1);
        assert_eq!(
            usage.additional_limits[0].limit_name.as_deref(),
            Some("Code review")
        );
        assert_eq!(
            usage.additional_limits[0].metered_feature.as_deref(),
            Some("code_review")
        );
        assert_eq!(
            usage.additional_limits[0]
                .primary
                .as_ref()
                .and_then(|window| window.window_minutes),
            Some(1_440)
        );
    }

    #[test]
    fn test_parse_usage_additional_rate_limits_missing_is_empty() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "primary_window": {"used_percent": 10.0}
            }
        }));
        assert!(usage.additional_limits.is_empty());
    }

    #[test]
    fn test_parse_usage_additional_rate_limits_empty_array_is_empty() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "primary_window": {"used_percent": 10.0}
            },
            "additional_rate_limits": []
        }));
        assert!(usage.additional_limits.is_empty());
    }

    #[test]
    fn nullable_nested_additional_rate_limit_preserves_the_pool_identity() {
        let usage = parse_usage_checked(&json!({
            "plan_type": "plus",
            "rate_limit": {"primary_window": {"used_percent": 10.0}},
            "additional_rate_limits": [
                {
                    "limit_name": "Missing details",
                    "metered_feature": "codex_missing_details"
                },
                {
                    "limit_name": "Null details",
                    "metered_feature": "codex_null_details",
                    "rate_limit": null
                }
            ]
        }))
        .expect("the backend schema permits a missing or null nested rate_limit");

        assert_eq!(usage.additional_limits.len(), 2);
        for (limit, expected_name, expected_feature) in [
            (
                &usage.additional_limits[0],
                "Missing details",
                "codex_missing_details",
            ),
            (
                &usage.additional_limits[1],
                "Null details",
                "codex_null_details",
            ),
        ] {
            assert_eq!(limit.limit_name.as_deref(), Some(expected_name));
            assert_eq!(limit.metered_feature.as_deref(), Some(expected_feature));
            assert_eq!(limit.allowed, None);
            assert_eq!(limit.limit_reached, None);
            assert!(limit.primary.is_none());
            assert!(limit.secondary.is_none());
        }
        assert!(usage.parse_issues.is_empty());
    }

    #[test]
    fn infallible_parse_preserves_malformed_additional_limits_as_a_typed_issue() {
        let usage = parse_usage(&json!({
            "rate_limit": {"primary_window": {"used_percent": 10.0}},
            "additional_rate_limits": [
                {"limit_name": "missing_feature", "rate_limit": null},
            ]
        }));

        assert!(usage.additional_limits.is_empty());
        assert!(matches!(
            usage.parse_issues.as_slice(),
            [UsageParseIssue::InvalidAdditionalRateLimits { detail }]
                if detail.contains("must contain string limit_name and metered_feature")
        ));
        assert!(usage.account_limited);
        assert!(crate::usage::explicit_account_blocker(&usage).is_some());
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn checked_parse_rejects_malformed_additional_and_code_review_shapes() {
        let cases = [
            json!({"additional_rate_limits": {}}),
            json!({"additional_rate_limits": [null]}),
            json!({"additional_rate_limits": [{}]}),
            json!({"additional_rate_limits": [{"rate_limit": null}]}),
            json!({"additional_rate_limits": [{
                "limit_name": "Missing feature",
                "rate_limit": {"allowed": true}
            }]}),
            json!({"additional_rate_limits": [{
                "metered_feature": "missing_name",
                "rate_limit": {"allowed": true}
            }]}),
            json!({"additional_rate_limits": [{"rate_limit": "bad"}]}),
            json!({"additional_rate_limits": [{"rate_limit": {}}]}),
            json!({"additional_rate_limits": [{"rate_limit": {"allowed": "yes"}}]}),
            json!({"additional_rate_limits": [{"rate_limit": {"allowed": null}}]}),
            json!({"additional_rate_limits": [{"rate_limit": {
                "primary_window": {"used_percent": 101.0}
            }}]}),
            json!({"code_review_rate_limit": "bad"}),
            json!({"code_review_rate_limit": {}}),
            json!({"code_review_rate_limit": {"limit_reached": 1}}),
            json!({"code_review_rate_limit": {
                "primary_window": {"used_percent": "10"}
            }}),
        ];
        for malformed in cases {
            let mut body = json!({
                "plan_type": "plus",
                "rate_limit": {"primary_window": {"used_percent": 10.0}}
            });
            body.as_object_mut()
                .unwrap()
                .extend(malformed.as_object().unwrap().clone());
            assert!(
                parse_usage_checked(&body).is_err(),
                "explicit malformed shape was accepted: {body}"
            );
        }
    }

    #[test]
    fn unknown_nonempty_limit_reason_is_a_hard_blocker_with_raw_reason_preserved() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 10.0}
            },
            "rate_limit_reached_type": {"type": "future_reason"},
            "spend_control": {"reached": false}
        });
        let usage = parse_usage_checked(&body).unwrap();

        assert!(usage.account_limited);
        assert_eq!(
            usage.rate_limit_reached_type.as_deref(),
            Some("future_reason")
        );
        assert!(matches!(
            crate::usage::explicit_account_blocker(&usage),
            Some(crate::usage::ExplicitAccountBlocker::UnrecognizedRateLimitReason(reason))
                if reason == "future_reason"
        ));
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn legacy_scalar_omissions_remain_valid_but_future_plan_strings_are_preserved() {
        let legacy = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {"used_percent": 10.0}
            },
            "spend_control": {
                "individual_limit": {"remaining_percent": 68.0}
            }
        });
        assert!(parse_usage_checked(&legacy).is_ok());

        let future = json!({
            "plan_type": "future_paid_tier",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 10.0}
            }
        });
        let usage = parse_usage_checked(&future).unwrap();
        assert_eq!(usage.plan_type.as_deref(), Some("future_paid_tier"));
        assert_eq!(
            crate::usage::normalized_plan_kind(&usage, &crate::jwt::AccountInfo::default()),
            crate::jwt::PlanKind::Unknown
        );
    }

    #[test]
    fn malformed_top_level_scalar_types_are_rejected_and_retained() {
        for malformed in [json!(null), json!(7), json!(false), json!({})] {
            let body = json!({
                "plan_type": malformed,
                "rate_limit": {"primary_window": {"used_percent": 10.0}}
            });
            assert!(parse_usage_checked(&body).is_err());
            let usage = parse_usage(&body);
            assert!(matches!(
                usage.parse_issues.as_slice(),
                [UsageParseIssue::InvalidPlanType { .. }]
            ));
            assert!(crate::usage::explicit_account_blocker(&usage).is_some());
        }

        for (field, malformed) in [
            ("allowed", json!("yes")),
            ("allowed", json!(null)),
            ("limit_reached", json!(1)),
            ("limit_reached", json!({})),
        ] {
            let mut body = json!({
                "plan_type": "plus",
                "rate_limit": {"primary_window": {"used_percent": 10.0}}
            });
            body["rate_limit"][field] = malformed;
            assert!(parse_usage_checked(&body).is_err());
            let usage = parse_usage(&body);
            assert!(matches!(
                usage.parse_issues.as_slice(),
                [UsageParseIssue::InvalidRateLimit { .. }]
            ));
            assert!(crate::usage::explicit_account_blocker(&usage).is_some());
        }

        for malformed in [json!("yes"), json!(null), json!(1), json!({})] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {"primary_window": {"used_percent": 10.0}},
                "spend_control": {"reached": malformed}
            });
            assert!(parse_usage_checked(&body).is_err());
            let usage = parse_usage(&body);
            assert!(matches!(
                usage.parse_issues.as_slice(),
                [UsageParseIssue::InvalidSpendControl { .. }]
            ));
            assert!(crate::usage::explicit_account_blocker(&usage).is_some());
        }

        for malformed in [json!("bad"), json!(1), json!([])] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {"primary_window": {"used_percent": 10.0}},
                "spend_control": {
                    "reached": false,
                    "individual_limit": malformed
                }
            });
            assert!(parse_usage_checked(&body).is_err());
            let usage = parse_usage(&body);
            assert!(usage.individual_limit.is_none());
            assert!(matches!(
                usage.parse_issues.as_slice(),
                [UsageParseIssue::InvalidSpendControl { .. }]
            ));
        }
    }

    #[test]
    fn top_level_allowed_false_is_an_account_limit() {
        let usage = parse_usage_checked(&json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": false,
                "limit_reached": false,
                "primary_window": {"used_percent": 10.0},
                "secondary_window": {"used_percent": 20.0}
            }
        }))
        .unwrap();

        assert!(usage.account_limited);
        assert!(!crate::usage::is_available(
            &usage,
            &crate::jwt::AccountInfo::default()
        ));
    }

    #[test]
    fn malformed_limit_reason_is_rejected_checked_and_preserved_infallibly() {
        for malformed in [json!(7), json!({"type": false}), json!("  ")] {
            let body = json!({
                "plan_type": "plus",
                "rate_limit": {"primary_window": {"used_percent": 10.0}},
                "rate_limit_reached_type": malformed
            });
            assert!(parse_usage_checked(&body).is_err());

            let usage = parse_usage(&body);
            assert!(usage.account_limited);
            assert!(matches!(
                usage.parse_issues.as_slice(),
                [UsageParseIssue::InvalidRateLimitReachedType { raw, .. }]
                    if raw == &body["rate_limit_reached_type"].as_str()
                        .map(String::from)
                        .unwrap_or_else(|| body["rate_limit_reached_type"].to_string())
            ));
            assert!(crate::usage::explicit_account_blocker(&usage).is_some());
            assert!(!crate::usage::is_available(
                &usage,
                &crate::jwt::AccountInfo::default()
            ));
        }
    }
}
