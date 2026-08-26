use serde_json::{Value, json};

fn now_unix() -> i64 {
    codex_switch::auth::now_unix_secs().expect("test clock must be a supported Unix timestamp")
}

/// Build a complete usage API response JSON matching the real structure.
///
/// - `plan_type`: "plus", "team", or "free"
/// - `used_5h`: primary window used_percent (0.0..100.0)
/// - `reset_5h_in`: seconds until 5h window resets
/// - `used_7d`: secondary window used_percent (0.0..100.0)
/// - `reset_7d_in`: seconds until 7d window resets
///
/// Uses a single clock reading for all timestamps to avoid skew.
pub fn base_response(
    plan_type: &str,
    used_5h: f64,
    reset_5h_in: i64,
    used_7d: f64,
    reset_7d_in: i64,
) -> Value {
    base_response_at(
        plan_type,
        used_5h,
        reset_5h_in,
        used_7d,
        reset_7d_in,
        now_unix(),
    )
}

/// Build a response with a specific `now` timestamp (for clock-consistent test scenarios).
pub fn base_response_at(
    plan_type: &str,
    used_5h: f64,
    reset_5h_in: i64,
    used_7d: f64,
    reset_7d_in: i64,
    now: i64,
) -> Value {
    let reset_5h_at = now + reset_5h_in;
    let reset_7d_at = now + reset_7d_in;

    json!({
        "user_id": format!("user_{plan_type}_mock"),
        "account_id": format!("acct_{plan_type}_mock"),
        "email": format!("{plan_type}@mock.test"),
        "plan_type": plan_type,
        "rate_limit": {
            "allowed": true,
            "limit_reached": used_5h >= 100.0,
            "primary_window": {
                "used_percent": used_5h,
                "limit_window_seconds": 18000,
                "reset_after_seconds": reset_5h_in,
                "reset_at": reset_5h_at
            },
            "secondary_window": {
                "used_percent": used_7d,
                "limit_window_seconds": 604800,
                "reset_after_seconds": reset_7d_in,
                "reset_at": reset_7d_at
            }
        },
        "credits": {
            "balance": 10.0,
            "unlimited": plan_type == "team"
        }
    })
}
