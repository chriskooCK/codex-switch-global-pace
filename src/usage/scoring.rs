use crate::jwt::{AccountInfo, PlanKind};

use super::{
    Candidate, FREE_FLOOR_PCT, MIN_WARMUP_ELAPSED_SECS, ScoredCandidate, UsageInfo, WINDOW_5H_SECS,
    WINDOW_7D_SECS, WindowUsage, main_weekly_quota_available, quota_window_duration_secs,
    validated_quota_window,
};

fn seconds_until(timestamp: i64, now: i64) -> Option<i64> {
    if timestamp <= now {
        Some(0)
    } else {
        timestamp.checked_sub(now)
    }
}

/// Returns true only when usage data proves a warmup-opened window is active.
pub fn warmup_window_active(w: &WindowUsage, window_secs: i64, now: i64) -> bool {
    let resets_at = match w.resets_at {
        Some(t) if t > now => t,
        _ => return false,
    };
    if !w
        .used_percent
        .is_some_and(|used| used.is_finite() && used > 0.0 && used <= 100.0)
    {
        return false;
    }
    let Some(remaining) = resets_at.checked_sub(now) else {
        return false;
    };
    if window_secs <= 0 || remaining > window_secs {
        return false;
    }
    let elapsed = window_secs - remaining;
    elapsed >= MIN_WARMUP_ELAPSED_SECS
}

/// Decide whether warmup should be skipped because the relevant window is already active.
///
/// When the API provides a short primary window, that is what warmup is meant
/// to (re)open, so a still-active weekly window must not suppress warmup after
/// the short window closes. Some responses expose only the weekly window; in
/// that shape it is the only available signal.
pub fn usage_has_active_warmup_window(u: &UsageInfo, now: i64) -> bool {
    let main_active = match u.primary.as_ref() {
        Some(w) => quota_window_duration_secs(w, WINDOW_5H_SECS)
            .is_some_and(|duration| warmup_window_active(w, duration, now)),
        None => u
            .secondary
            .as_ref()
            .and_then(|w| {
                quota_window_duration_secs(w, WINDOW_7D_SECS).map(|duration| (w, duration))
            })
            .is_some_and(|(w, duration)| warmup_window_active(w, duration, now)),
    };
    let additional_active = u
        .additional_limits
        .iter()
        .filter(|limit| {
            limit
                .metered_feature
                .as_deref()
                .is_some_and(|feature| feature.starts_with("codex_"))
        })
        .all(|limit| {
            if limit.allowed == Some(false) || limit.limit_reached == Some(true) {
                return true;
            }
            match limit.primary.as_ref() {
                Some(w) => quota_window_duration_secs(w, WINDOW_5H_SECS)
                    .is_some_and(|duration| warmup_window_active(w, duration, now)),
                None => limit
                    .secondary
                    .as_ref()
                    .and_then(|w| {
                        quota_window_duration_secs(w, WINDOW_7D_SECS).map(|duration| (w, duration))
                    })
                    .is_some_and(|(w, duration)| warmup_window_active(w, duration, now)),
            }
        });
    main_active && additional_active
}

/// Calculate pace: the expected used percentage if consumption were even across the window.
/// Invalid or stale windows cannot produce a meaningful comparison.
pub fn pace_percent_at(w: &WindowUsage, window_secs: i64, now: i64) -> Option<f64> {
    if window_secs <= 0 {
        return None;
    }
    let resets_at = w.resets_at?;
    let remaining_secs = resets_at.checked_sub(now)?;
    if remaining_secs <= 0 || remaining_secs > window_secs {
        return None;
    }
    let elapsed_secs = window_secs - remaining_secs;
    Some(elapsed_secs as f64 / window_secs as f64 * 100.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotaPaceState {
    UsageAhead,
    PaceAheadOrEqual,
    Unavailable,
}

/// Clamp a valid quota percentage for display without inventing missing data.
pub(crate) fn normalized_quota_usage(used_percent: Option<f64>) -> Option<f64> {
    used_percent
        .filter(|used| used.is_finite() && *used >= 0.0)
        .map(|used| used.min(100.0))
}

/// Classify quota presentation once so every renderer maps the same state to color.
pub(crate) fn quota_pace_state(
    used_percent: Option<f64>,
    pace_percent: Option<f64>,
) -> QuotaPaceState {
    let Some(used) = normalized_quota_usage(used_percent) else {
        return QuotaPaceState::Unavailable;
    };
    let Some(pace) = pace_percent.filter(|pace| pace.is_finite() && (0.0..=100.0).contains(pace))
    else {
        return QuotaPaceState::Unavailable;
    };
    if used > pace {
        QuotaPaceState::UsageAhead
    } else {
        QuotaPaceState::PaceAheadOrEqual
    }
}

/// Keep a pace marker whenever both usage and elapsed-time pace are known.
/// Display rounding and exhaustion must not erase the comparison point.
pub(crate) fn visible_pace_marker(
    used_percent: Option<f64>,
    pace_percent: Option<f64>,
) -> Option<f64> {
    normalized_quota_usage(used_percent)?;
    pace_percent.filter(|pace| pace.is_finite() && (0.0..=100.0).contains(pace))
}

/// Public helper for library consumers that need a window marker at one
/// caller-supplied observation time.
#[allow(dead_code)]
pub fn visible_pace_percent_at(w: &WindowUsage, window_secs: i64, now: i64) -> Option<f64> {
    visible_pace_marker(w.used_percent, pace_percent_at(w, window_secs, now))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageAvailability {
    Available,
    Limited,
    Unavailable,
}

/// Classify one loaded usage sample using the same validated weekly-quota
/// contract required by automatic selection and global weekly pace. Plan
/// metadata affects scoring preferences, not whether API-provided quota exists.
pub(crate) fn usage_availability(u: &UsageInfo, _info: &AccountInfo) -> UsageAvailability {
    if !u.parse_issues.is_empty() {
        return UsageAvailability::Unavailable;
    }
    if super::explicit_account_blocker(u).is_some() {
        return UsageAvailability::Limited;
    }

    let primary = u
        .primary
        .as_ref()
        .and_then(|window| validated_quota_window(window, WINDOW_5H_SECS))
        .map(|(used, _)| used);
    let weekly = u
        .secondary
        .as_ref()
        .and_then(|window| validated_quota_window(window, WINDOW_7D_SECS))
        .map(|(used, _)| used);
    if !main_weekly_quota_available(weekly.as_ref()) {
        return UsageAvailability::Unavailable;
    }
    if u.account_limited {
        return UsageAvailability::Limited;
    }
    if [primary, weekly]
        .into_iter()
        .flatten()
        .any(|used| used >= 100.0)
    {
        UsageAvailability::Limited
    } else {
        UsageAvailability::Available
    }
}

/// Whether an account's loaded quota sample proves it currently usable.
pub fn is_available(u: &UsageInfo, info: &AccountInfo) -> bool {
    usage_availability(u, info) == UsageAvailability::Available
}

/// Eligibility check on a Candidate (reset-aware).
pub fn is_candidate_eligible(c: &Candidate, safety_margin_7d: f64) -> bool {
    if c.account_limit_active() || !c.has_required_quota_data() {
        return false;
    }
    let used_5h = c.effective_used_5h();
    let Some(used_7d) = c.effective_used_7d() else {
        return false;
    };

    // Gate 1: 5h exhausted (and not past reset)
    if used_5h.is_some_and(|used| used >= 100.0) {
        return false;
    }
    // Gate 2: 7d exhausted (and not past reset)
    if used_7d >= 100.0 {
        return false;
    }
    // Gate 3: 7d critically low and reset far away
    if let Some(weekly) = &c.weekly {
        let remaining_7d = 100.0 - used_7d;
        let critical_pct = (safety_margin_7d * 0.25_f64).max(1.0);
        if remaining_7d < critical_pct {
            let reset_is_far = match weekly.resets_at {
                Some(timestamp) => seconds_until(timestamp, c.now)
                    .is_none_or(|seconds| seconds as f64 / 3600.0 > 48.0),
                None => true,
            };
            if reset_is_far {
                return false;
            }
        }
    }
    // Gate 4: Free plan safety floor
    if c.is_free()
        && let Some(used_5h) = used_5h
    {
        let remaining_5h = 100.0 - used_5h;
        if remaining_5h < FREE_FLOOR_PCT {
            return false;
        }
    }
    true
}

// ── adaptive scoring algorithm ─────────────────────────────

/// Adaptive scoring algorithm. Pure function, no I/O.
///
/// Automatically adjusts strategy based on pool state. No mode selection needed.
///
/// Components:
///   tier_bonus   — Team priority (0 or 500, configurable)
///   headroom     — Pace-aware effective remaining time (0..1100)
///   drain_value  — Quota that will be wasted if not used before reset (0..300)
///   sustain      — 7d budget-per-window sustainability (-800..0)
///   recency      — Spread usage across accounts (-60..0)
///
/// Pool-adaptive: drain_weight scales with pool_size and exhausted ratio.
pub fn score_unified(c: &Candidate, safety_margin_7d: f64) -> f64 {
    let used_5h = c.effective_used_5h();
    let used_7d = c.effective_used_7d();

    // ── Component A: tier_bonus (0 or 500) ──
    let tier_bonus = if c.is_team() && c.team_priority {
        500.0
    } else {
        0.0
    };

    // ── Component B: headroom (0..1100) ──
    // Pace-aware: uses burn rate to project effective remaining time,
    // not just static remaining%.
    let headroom = match (c.primary.as_ref(), used_5h) {
        (None, _) | (Some(_), None) => 50.0,
        (Some(primary), Some(used_5h)) if used_5h >= 100.0 => {
            // Exhausted: score by time-to-reset (closer = higher, range 0..500).
            // The 500 ceiling (vs 1000+ for active accounts) is intentional:
            // is_candidate_eligible() marks exhausted accounts as ineligible,
            // and the caller sorts eligible-first. This branch only ranks among
            // ineligible fallback candidates when no eligible account exists.
            match primary.resets_at {
                None => 0.0,
                Some(reset_ts) => match seconds_until(reset_ts, c.now) {
                    None => 0.0,
                    Some(remaining_secs) => {
                        let remaining_secs = remaining_secs as f64;
                        (500.0 - remaining_secs / 60.0).max(0.0)
                    }
                },
            }
        }
        (Some(primary), Some(used_5h)) => {
            // Pace-aware headroom: project remaining minutes using burn rate
            let remaining_pct = 100.0 - used_5h;
            match primary.resets_at {
                Some(reset_ts) => match seconds_until(reset_ts, c.now) {
                    None => 0.0,
                    Some(remaining_secs) => {
                        let remaining_secs = remaining_secs as f64;
                        let duration_secs = primary.duration_secs as f64;
                        let elapsed_secs = (duration_secs - remaining_secs).max(1.0);
                        let burn_rate = used_5h / elapsed_secs; // %/sec

                        if burn_rate > 0.001 {
                            // Project minutes until exhaustion at current rate
                            let projected_min = (remaining_pct / burn_rate) / 60.0;
                            let duration_min = duration_secs / 60.0;
                            // Normalize the projected reserve to this window's actual duration.
                            1000.0 + (projected_min.min(duration_min) / duration_min * 100.0)
                        } else {
                            // Near-zero burn rate → effectively full capacity
                            1000.0 + remaining_pct
                        }
                    }
                },
                None => 1000.0 + remaining_pct,
            }
        }
    };

    // ── Component C: sustain — 7d sustainability (-800..0) ──
    // Uses budget-per-window: how much 7d quota is available per remaining 5h window.
    const RELIEF_WINDOW_HOURS: f64 = 48.0;
    const MAX_RELIEF: f64 = 0.8;

    let sustain = match (c.weekly.as_ref(), used_7d) {
        (None, _) | (Some(_), None) => -50.0,
        (Some(weekly), Some(used_7d)) if used_7d >= 100.0 => {
            // 7d exhausted: heavy penalty, relieved as reset approaches
            match weekly.resets_at {
                None => -800.0, // no reset info: maximum penalty
                Some(reset_ts) => match seconds_until(reset_ts, c.now) {
                    None => -800.0,
                    Some(remaining_secs) => {
                        let remaining_fraction =
                            remaining_secs as f64 / weekly.duration_secs as f64;
                        let relief = (1.0 - remaining_fraction).clamp(0.0, 1.0);
                        -800.0 * (1.0 - relief)
                    }
                },
            }
        }
        (Some(weekly), Some(used_7d)) => {
            let remaining_7d = 100.0 - used_7d;
            if remaining_7d >= safety_margin_7d {
                0.0
            } else {
                // Compute budget per remaining 5h window
                let budget_penalty = if let (Some(reset_ts_7d), Some(primary)) =
                    (weekly.resets_at, c.primary.as_ref())
                {
                    let hours_to_7d_reset =
                        seconds_until(reset_ts_7d, c.now).map(|seconds| seconds as f64 / 3600.0);
                    let primary_hours = primary.duration_secs as f64 / 3600.0;
                    match hours_to_7d_reset {
                        Some(hours_to_7d_reset) => {
                            let remaining_windows = (hours_to_7d_reset / primary_hours).max(1.0);
                            let budget_per_window = remaining_7d / remaining_windows;
                            // If each window gets ≥ safety_margin worth of budget, it's fine
                            if budget_per_window >= safety_margin_7d {
                                0.0
                            } else {
                                // Shortfall: 0..1, higher = more pressure
                                ((safety_margin_7d - budget_per_window) / safety_margin_7d)
                                    .clamp(0.0, 1.0)
                            }
                        }
                        None => 1.0,
                    }
                } else {
                    // No reset time: use simple pressure
                    if safety_margin_7d > 0.0 {
                        ((safety_margin_7d - remaining_7d) / safety_margin_7d).clamp(0.0, 1.0)
                    } else {
                        1.0
                    }
                };

                // Time relief: if 7d resets within 48h, reduce penalty
                let time_relief = match weekly.resets_at {
                    Some(ts) => match seconds_until(ts, c.now) {
                        Some(seconds) => {
                            let hours = seconds as f64 / 3600.0;
                            if hours < RELIEF_WINDOW_HOURS {
                                (1.0 - hours / RELIEF_WINDOW_HOURS).clamp(0.0, 1.0)
                            } else {
                                0.0
                            }
                        }
                        None => 0.0,
                    },
                    None => 0.0,
                };

                let effective = budget_penalty * (1.0 - time_relief * MAX_RELIEF);
                -800.0 * effective
            }
        }
    };

    // ── Component D: drain_value (0..300) ──
    // Only activates when 5h reset is within 60 minutes AND there's quota to waste.
    // Pool-adaptive: larger pools with more available accounts → more aggressive drain.
    const DRAIN_WINDOW_MIN: f64 = 60.0;

    let raw_drain = if let (Some(primary), Some(used_5h)) = (c.primary.as_ref(), used_5h)
        && used_5h < 100.0
    {
        if let Some(reset_ts) = primary.resets_at
            && reset_ts > c.now
        {
            match seconds_until(reset_ts, c.now) {
                Some(remaining_secs) => {
                    let remaining_min = remaining_secs as f64 / 60.0;
                    if remaining_min <= DRAIN_WINDOW_MIN {
                        let remaining_pct = 100.0 - used_5h;
                        let urgency =
                            ((DRAIN_WINDOW_MIN - remaining_min) / DRAIN_WINDOW_MIN).clamp(0.0, 1.0);
                        // waste = remaining quota × urgency, scaled to 0..300
                        (remaining_pct * urgency * 3.0).min(300.0)
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            }
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Pool-adaptive drain weight
    let drain_weight = if c.pool_size <= 2 {
        0.5 // Few accounts: be conservative, don't chase drain
    } else {
        let exhausted_ratio = c.pool_exhausted as f64 / c.pool_size as f64;
        if exhausted_ratio > 0.7 {
            0.3 // Most accounts exhausted: conserve what we have
        } else if c.pool_size >= 5 && exhausted_ratio < 0.3 {
            1.5 // Plenty of backup: drain aggressively
        } else {
            1.0
        }
    };

    let drain_value = raw_drain * drain_weight;

    // ── Component E: recency (-60..0) ──
    // Light spread penalty to avoid hammering the same account
    let recency = if c.last_used == 0 {
        0.0
    } else {
        let seconds_ago = match c.now.checked_sub(c.last_used) {
            Some(seconds) => seconds.max(0) as f64,
            None if c.last_used < c.now => f64::INFINITY,
            None => 0.0,
        };
        -(60.0 - (seconds_ago / 30.0)).clamp(0.0, 60.0)
    };

    tier_bonus + headroom + sustain + drain_value + recency
}

// ── Shared candidate building and selection ───────────────
//
// CLI `use` and the daemon score the same way through these helpers; only
// the final ranking/selection policy differs per caller.

/// Normalize every plan signal into one exclusive tier. A present API plan is
/// authoritative, then a JWT plan, while organization/workspace evidence is
/// consulted only when neither source names a plan. A truly signal-free
/// account retains the historical Free classification without also becoming
/// Team.
pub(crate) fn normalized_plan_kind(usage: &UsageInfo, info: &crate::jwt::AccountInfo) -> PlanKind {
    if let Some(api_plan) = usage.plan_type.as_deref() {
        return PlanKind::from_wire(Some(api_plan));
    }
    if let Some(jwt_plan) = info.plan_type.as_deref() {
        return PlanKind::from_wire(Some(jwt_plan));
    }
    if info.is_team() {
        PlanKind::Team
    } else {
        PlanKind::Free
    }
}

/// Build and score candidates uniformly: the API `plan_type` is
/// authoritative over the JWT (handles plan downgrades), and
/// `pool_exhausted` counts every account unavailable to automatic selection.
/// Input order is preserved.
pub fn score_candidates(
    fetched: Vec<(String, UsageInfo, crate::jwt::AccountInfo, i64)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<ScoredCandidate> {
    let pool_size = fetched.len();

    let mut candidates: Vec<(Candidate, UsageInfo)> = fetched
        .into_iter()
        .map(|(alias, u, info, last_used)| {
            let plan_kind = normalized_plan_kind(&u, &info);
            let mut candidate = Candidate::from_usage(alias, &u, plan_kind, last_used, now);
            candidate.pool_size = pool_size;
            candidate.team_priority = team_priority;
            (candidate, u)
        })
        .collect();

    let pool_exhausted = candidates
        .iter()
        .filter(|(candidate, _)| !is_candidate_eligible(candidate, safety_7d))
        .count();
    for (candidate, _) in &mut candidates {
        candidate.pool_exhausted = pool_exhausted;
    }

    candidates
        .into_iter()
        .map(|(candidate, usage)| {
            let score = score_unified(&candidate, safety_7d);
            ScoredCandidate {
                candidate,
                usage,
                score,
            }
        })
        .collect()
}

/// Daemon switch policy over already-scored candidates. Eligibility is a hard
/// boundary: an eligible current account is replaced only by a higher-scoring
/// eligible candidate, while an ineligible current account yields to the best
/// eligible candidate regardless of raw score. An ineligible alternative is
/// considered only when every account is ineligible.
pub fn pick_switch_target<'a>(
    current: &ScoredCandidate,
    others: &'a [ScoredCandidate],
    safety_7d: f64,
) -> Option<(&'a str, f64)> {
    let current_eligible = is_candidate_eligible(&current.candidate, safety_7d);
    let current_unselectable = current.candidate.explicit_account_blocker.is_some()
        || !current.candidate.has_required_quota_data();
    let mut best_eligible: Option<(&'a str, f64)> = None;
    let mut best_ineligible: Option<(&'a str, f64)> = None;
    let mut any_eligible = false;

    for s in others {
        if s.candidate.explicit_account_blocker.is_some() || !s.candidate.has_required_quota_data()
        {
            continue;
        }
        let eligible = is_candidate_eligible(&s.candidate, safety_7d);
        if eligible {
            any_eligible = true;
            if (!current_eligible || s.score > current.score)
                && best_eligible.is_none_or(|(_, bs)| s.score > bs)
            {
                best_eligible = Some((s.candidate.alias.as_str(), s.score));
            }
        } else if (current_unselectable || s.score > current.score)
            && best_ineligible.is_none_or(|(_, bs)| s.score > bs)
        {
            best_ineligible = Some((s.candidate.alias.as_str(), s.score));
        }
    }

    match best_eligible {
        Some(best) => Some(best),
        None if any_eligible || current_eligible => None,
        None => best_ineligible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_with(primary: Option<WindowUsage>, secondary: Option<WindowUsage>) -> UsageInfo {
        UsageInfo {
            cache_revision: None,
            fetched_at: None,
            primary,
            secondary,
            credits_balance: None,
            unlimited_credits: None,
            plan_type: None,
            reset_credits_available_count: None,
            reset_credits: vec![],
            reset_credits_error: None,
            account_limited: false,
            spend_control_reached: false,
            rate_limit_reached_type: None,
            individual_limit: None,
            additional_limits: vec![],
            parse_issues: vec![],
        }
    }

    fn window(used_percent: f64, resets_at: Option<i64>) -> WindowUsage {
        WindowUsage {
            used_percent: Some(used_percent),
            resets_at,
            window_minutes: None,
        }
    }

    #[test]
    fn test_default_usage_is_not_available() {
        assert!(!is_available(
            &UsageInfo::default(),
            &AccountInfo::default()
        ));
        let candidate = Candidate::from_usage(
            "empty".to_string(),
            &UsageInfo::default(),
            PlanKind::Unknown,
            0,
            1,
        );
        assert!(!is_candidate_eligible(&candidate, 20.0));
    }

    #[test]
    fn test_is_available_both_under_100() {
        let usage = usage_with(
            Some(window(50.0, Some(1_000))),
            Some(window(30.0, Some(2_000))),
        );

        assert!(is_available(&usage, &AccountInfo::default()));
    }

    #[test]
    fn weekly_quota_contract_is_shared_by_selection_status_and_global_pace() {
        let now = 1_000_000;
        let plus = AccountInfo {
            plan_type: Some("plus".to_string()),
            ..AccountInfo::default()
        };
        let weekly_only = UsageInfo {
            plan_type: Some("pro".to_string()),
            secondary: Some(WindowUsage {
                used_percent: Some(20.0),
                resets_at: Some(now + WINDOW_7D_SECS / 2),
                window_minutes: Some(WINDOW_7D_SECS / 60),
            }),
            ..UsageInfo::default()
        };
        let weekly_only_plan = normalized_plan_kind(&weekly_only, &plus);
        let weekly_only_candidate = Candidate::from_usage(
            "weekly-only".to_string(),
            &weekly_only,
            weekly_only_plan,
            0,
            now,
        );

        assert_eq!(weekly_only_plan, PlanKind::Pro);
        assert!(weekly_only_candidate.primary.is_none());
        assert!(weekly_only_candidate.has_required_quota_data());
        assert!(is_candidate_eligible(&weekly_only_candidate, 20.0));
        assert_eq!(
            usage_availability(&weekly_only, &plus),
            UsageAvailability::Available
        );
        let global_input =
            super::super::GlobalPaceAccountInput::from_usage("weekly-only", &weekly_only);
        assert!(
            super::super::calculate_account_weekly_pace(&global_input, now).is_some(),
            "the same weekly-only account must participate in global pace"
        );
        let primary_only = UsageInfo {
            primary: Some(window(10.0, Some(now + WINDOW_5H_SECS / 2))),
            ..UsageInfo::default()
        };
        let current = scored(
            Candidate::from_usage(
                "primary-only".to_string(),
                &primary_only,
                PlanKind::Plus,
                0,
                now,
            ),
            20.0,
        );
        let alternatives = [scored(weekly_only_candidate.clone(), 20.0)];
        assert_eq!(
            pick_switch_target(&current, &alternatives, 20.0).map(|(alias, _)| alias),
            Some("weekly-only"),
            "automatic selection must accept the same weekly-only account"
        );

        let unknown_plan = UsageInfo {
            plan_type: Some("future_plan".to_string()),
            ..weekly_only.clone()
        };
        let unknown_kind = normalized_plan_kind(&unknown_plan, &plus);
        let unknown_candidate = Candidate::from_usage(
            "unknown-plan".to_string(),
            &unknown_plan,
            unknown_kind,
            0,
            now,
        );
        assert_eq!(unknown_kind, PlanKind::Unknown);
        assert!(unknown_candidate.has_required_quota_data());
        assert!(is_candidate_eligible(&unknown_candidate, 20.0));
        assert_eq!(
            usage_availability(&unknown_plan, &plus),
            UsageAvailability::Available
        );

        let explicit_spend_blocker = UsageInfo {
            account_limited: true,
            spend_control_reached: true,
            ..weekly_only.clone()
        };
        let blocked_candidate = Candidate::from_usage(
            "blocked".to_string(),
            &explicit_spend_blocker,
            PlanKind::Pro,
            0,
            now,
        );
        assert!(blocked_candidate.has_required_quota_data());
        assert!(!is_candidate_eligible(&blocked_candidate, 20.0));
        assert_eq!(
            usage_availability(&explicit_spend_blocker, &plus),
            UsageAvailability::Limited
        );
        let blocked_global =
            super::super::GlobalPaceAccountInput::from_usage("blocked", &explicit_spend_blocker);
        assert!(
            super::super::calculate_account_weekly_pace(&blocked_global, now).is_none(),
            "an explicit blocker must exclude the account from global pace"
        );

        let blocker_without_window = UsageInfo {
            account_limited: true,
            spend_control_reached: true,
            ..UsageInfo::default()
        };
        assert_eq!(
            usage_availability(&blocker_without_window, &plus),
            UsageAvailability::Limited,
            "an explicit backend blocker remains authoritative without quota data"
        );
    }

    #[test]
    fn test_is_available_primary_exhausted() {
        let usage = usage_with(
            Some(window(100.0, Some(1_000))),
            Some(window(30.0, Some(2_000))),
        );

        assert!(!is_available(&usage, &AccountInfo::default()));
    }

    #[test]
    fn test_is_available_secondary_exhausted() {
        let usage = usage_with(
            Some(window(50.0, Some(1_000))),
            Some(window(100.0, Some(2_000))),
        );

        assert!(!is_available(&usage, &AccountInfo::default()));
    }

    #[test]
    fn test_is_available_no_data() {
        assert!(!is_available(
            &UsageInfo::default(),
            &AccountInfo::default()
        ));
    }

    #[test]
    fn ordinary_window_exhaustion_preserves_reset_data_for_ranking() {
        let usage = UsageInfo {
            primary: Some(window(100.0, Some(1_001_800))),
            secondary: Some(window(80.0, Some(1_604_800))),
            account_limited: true,
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            ..UsageInfo::default()
        };

        let candidate =
            Candidate::from_usage("ordinary".to_string(), &usage, PlanKind::Plus, 0, 1_000_000);

        assert_eq!(
            candidate.primary.as_ref().map(|window| window.used_percent),
            Some(100.0)
        );
        assert_eq!(
            candidate
                .primary
                .as_ref()
                .and_then(|window| window.resets_at),
            Some(1_001_800)
        );
        assert_eq!(
            candidate.weekly.as_ref().map(|window| window.used_percent),
            Some(80.0)
        );
        assert_eq!(
            candidate
                .weekly
                .as_ref()
                .and_then(|window| window.resets_at),
            Some(1_604_800)
        );
        assert_eq!(candidate.explicit_account_blocker, None);
        assert_eq!(
            candidate.ordinary_account_limit,
            Some(super::super::OrdinaryAccountLimit::UntilReset(1_001_800))
        );
    }

    #[test]
    fn broad_limit_verdict_blocks_inconsistent_percentages_until_reset() {
        for used in [10.0, 99.0] {
            let usage = UsageInfo {
                primary: Some(window(used, Some(1_003_600))),
                secondary: Some(window(20.0, Some(1_604_800))),
                account_limited: true,
                ..UsageInfo::default()
            };
            let mut candidate = Candidate::from_usage(
                format!("limited-{used}"),
                &usage,
                PlanKind::Plus,
                0,
                1_000_000,
            );

            assert_eq!(
                candidate.primary.as_ref().map(|window| window.used_percent),
                Some(used)
            );
            assert_eq!(
                candidate.ordinary_account_limit,
                Some(super::super::OrdinaryAccountLimit::UntilReset(1_604_800))
            );
            assert!(!is_candidate_eligible(&candidate, 20.0));

            candidate.now = 1_003_600;
            assert!(
                !is_candidate_eligible(&candidate, 20.0),
                "an ambiguous broad verdict remains active until every possible window resets"
            );
            candidate.now = 1_604_800;
            assert!(
                is_candidate_eligible(&candidate, 20.0),
                "the stale broad verdict may be ignored once every reported window reset"
            );
        }
    }

    #[test]
    fn broad_limit_without_window_metadata_fails_closed() {
        let usage = UsageInfo {
            account_limited: true,
            ..UsageInfo::default()
        };
        let candidate = Candidate::from_usage(
            "missing-window".to_string(),
            &usage,
            PlanKind::Plus,
            0,
            1_000_000,
        );

        assert_eq!(
            candidate.ordinary_account_limit,
            Some(super::super::OrdinaryAccountLimit::ResetUnknown)
        );
        assert!(!is_candidate_eligible(&candidate, 20.0));
    }

    #[test]
    fn unrecognized_limit_reason_is_hard_blocked_regardless_of_window_shape_or_reset() {
        for primary in [
            Some(window(10.0, Some(999_000))),
            Some(window(99.0, Some(1_003_600))),
            None,
        ] {
            let usage = UsageInfo {
                primary,
                account_limited: true,
                rate_limit_reached_type: Some("future_server_reason".to_string()),
                ..UsageInfo::default()
            };
            let candidate = Candidate::from_usage(
                "unknown-reason".to_string(),
                &usage,
                PlanKind::Plus,
                0,
                1_000_000,
            );

            assert!(matches!(
                candidate.explicit_account_blocker,
                Some(super::super::ExplicitAccountBlocker::UnrecognizedRateLimitReason(ref reason))
                    if reason == "future_server_reason"
            ));
            assert_eq!(candidate.ordinary_account_limit, None);
            assert!(!is_candidate_eligible(&candidate, 20.0));
            assert!(!is_available(&usage, &AccountInfo::default()));
        }
    }

    #[test]
    fn explicit_spend_blocker_is_typed_and_never_eligible() {
        let usage = UsageInfo {
            primary: Some(window(10.0, Some(1_003_600))),
            secondary: Some(window(10.0, Some(1_604_800))),
            account_limited: true,
            spend_control_reached: true,
            ..UsageInfo::default()
        };

        let candidate =
            Candidate::from_usage("blocked".to_string(), &usage, PlanKind::Plus, 0, 1_000_000);

        assert_eq!(
            candidate.explicit_account_blocker,
            Some(super::super::ExplicitAccountBlocker::SpendControlReached)
        );
        assert!(!is_candidate_eligible(&candidate, 20.0));
        assert!(!is_available(&usage, &AccountInfo::default()));
    }

    // ── adaptive scoring tests ──

    fn make_candidate(
        alias: &str,
        used_5h: f64,
        reset_5h: Option<i64>,
        used_7d: f64,
        reset_7d: Option<i64>,
    ) -> Candidate {
        Candidate {
            alias: alias.to_string(),
            primary: Some(super::super::CandidateWindow {
                used_percent: used_5h,
                resets_at: reset_5h,
                duration_secs: WINDOW_5H_SECS,
            }),
            weekly: Some(super::super::CandidateWindow {
                used_percent: used_7d,
                resets_at: reset_7d,
                duration_secs: WINDOW_7D_SECS,
            }),
            explicit_account_blocker: None,
            ordinary_account_limit: None,
            plan_kind: PlanKind::Plus,
            last_used: 0,
            now: 1_000_000,
            pool_size: 5,
            pool_exhausted: 0,
            team_priority: true,
        }
    }

    fn usage_with_5h(used_percent: f64, resets_at: i64, plan_type: Option<&str>) -> UsageInfo {
        UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(used_percent),
                resets_at: Some(resets_at),
                window_minutes: None,
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(resets_at + 5 * 86400),
                window_minutes: None,
            }),
            plan_type: plan_type.map(|p| p.to_string()),
            ..UsageInfo::default()
        }
    }

    #[test]
    fn test_score_candidates_api_plan_overrides_jwt_and_counts_pool_exhausted() {
        let now = 1_000_000i64;
        let jwt_team = crate::jwt::AccountInfo {
            plan_type: Some("team".to_string()),
            ..Default::default()
        };
        let items = vec![
            // API says free although the JWT still claims team (plan downgrade)
            (
                "downgraded".to_string(),
                usage_with_5h(100.0, now + 3600, Some("free")),
                jwt_team,
                0,
            ),
            // No API plan — JWT (default: not team/free) applies
            (
                "healthy".to_string(),
                usage_with_5h(20.0, now + 3600, None),
                Default::default(),
                0,
            ),
        ];

        let scored = score_candidates(items, now, 20.0, true);

        assert_eq!(scored.len(), 2);
        assert_eq!(scored[0].candidate.alias, "downgraded"); // input order preserved
        assert!(scored[0].candidate.is_free());
        assert!(!scored[0].candidate.is_team());
        // One exhausted account (100% 5h), visible to every candidate
        assert_eq!(scored[0].candidate.pool_exhausted, 1);
        assert_eq!(scored[1].candidate.pool_exhausted, 1);
        assert_eq!(scored[1].candidate.pool_size, 2);
    }

    #[test]
    fn plan_evidence_normalizes_to_one_exclusive_kind() {
        let usage = usage_with_5h(20.0, 1_003_600, None);
        let workspace_only = crate::jwt::AccountInfo {
            organizations: vec![crate::jwt::OrgInfo::default()],
            ..Default::default()
        };
        assert_eq!(
            normalized_plan_kind(&usage, &workspace_only),
            PlanKind::Team
        );

        let api_free = UsageInfo {
            plan_type: Some("free".to_string()),
            ..usage
        };
        assert_eq!(
            normalized_plan_kind(&api_free, &workspace_only),
            PlanKind::Free
        );
    }

    #[test]
    fn candidate_without_weekly_window_is_unavailable_everywhere() {
        let now = 1_000_000;
        let usage = UsageInfo {
            primary: Some(window(10.0, Some(now + 3_600))),
            ..UsageInfo::default()
        };
        let candidate =
            Candidate::from_usage("partial".to_string(), &usage, PlanKind::Plus, 0, now);

        assert!(!candidate.has_required_quota_data());
        assert!(!is_candidate_eligible(&candidate, 20.0));
        assert_eq!(
            usage_availability(&usage, &AccountInfo::default()),
            UsageAvailability::Unavailable
        );
        let global_input = super::super::GlobalPaceAccountInput::from_usage("partial", &usage);
        assert!(super::super::calculate_account_weekly_pace(&global_input, now).is_none());

        let current = scored(candidate, 20.0);
        let complete_but_exhausted = scored(
            make_candidate(
                "complete",
                100.0,
                Some(now + 3_600),
                20.0,
                Some(now + 5 * 86_400),
            ),
            20.0,
        );
        assert_eq!(
            pick_switch_target(&current, &[complete_but_exhausted], 20.0).map(|(alias, _)| alias),
            Some("complete")
        );
    }

    fn scored(candidate: Candidate, safety_7d: f64) -> ScoredCandidate {
        let score = score_unified(&candidate, safety_7d);
        ScoredCandidate {
            candidate,
            usage: UsageInfo::default(),
            score,
        }
    }

    #[test]
    fn test_pick_switch_target_prefers_eligible_above_current() {
        let now = 1_000_000i64;
        let current = scored(
            make_candidate("current", 90.0, Some(now + 3600), 50.0, Some(now + 86400)),
            20.0,
        );
        let good = make_candidate("good", 10.0, Some(now + 3600), 10.0, Some(now + 5 * 86400));

        let others = vec![scored(good, 20.0)];
        let pick = pick_switch_target(&current, &others, 20.0);
        assert_eq!(pick.map(|(a, _)| a), Some("good"));
    }

    #[test]
    fn test_pick_switch_target_ignores_ineligible_when_an_eligible_exists() {
        let now = 1_000_000i64;
        let current = scored(
            make_candidate(
                "current",
                90.0,
                Some(now + 3600),
                94.0,
                Some(now + 5 * 86400),
            ),
            20.0,
        );

        // Eligible but worse than current; ineligible (7d over safety margin) better.
        let weak_eligible =
            make_candidate("weak", 95.0, Some(now + 3600), 94.0, Some(now + 5 * 86400));
        let strong_ineligible = make_candidate(
            "strong",
            0.0,
            Some(now + 18000),
            96.0,
            Some(now + 5 * 86400),
        );

        let others = vec![scored(weak_eligible, 20.0), scored(strong_ineligible, 20.0)];
        // An eligible candidate exists, so the ineligible one must not be picked,
        // and the eligible one does not beat current — no switch.
        assert!(pick_switch_target(&current, &others, 20.0).is_none());
    }

    #[test]
    fn test_pick_switch_target_falls_back_when_nothing_is_eligible() {
        let now = 1_000_000i64;
        let current = scored(
            make_candidate("current", 100.0, Some(now + 3600), 96.0, Some(now + 86400)),
            20.0,
        );

        let ineligible = make_candidate(
            "fallback",
            0.0,
            Some(now + 18000),
            96.0,
            Some(now + 5 * 86400),
        );
        let others = vec![scored(ineligible, 20.0)];

        let pick = pick_switch_target(&current, &others, 20.0);
        assert_eq!(pick.map(|(a, _)| a), Some("fallback"));
    }

    #[test]
    fn test_pick_switch_target_keeps_eligible_current_over_ineligible_alternative() {
        let now = 1_000_000i64;
        let current = scored(
            make_candidate(
                "current",
                80.0,
                Some(now + 4 * 3600),
                90.0,
                Some(now + 5 * 86400),
            ),
            20.0,
        );
        let exhausted = scored(
            make_candidate(
                "exhausted",
                100.0,
                Some(now + 60),
                10.0,
                Some(now + 5 * 86400),
            ),
            20.0,
        );

        assert!(is_candidate_eligible(&current.candidate, 20.0));
        assert!(!is_candidate_eligible(&exhausted.candidate, 20.0));
        assert!(exhausted.score > current.score);
        assert!(pick_switch_target(&current, &[exhausted], 20.0).is_none());
    }

    #[test]
    fn test_pick_switch_target_leaves_ineligible_current_for_any_eligible_alternative() {
        let now = 1_000_000i64;
        let mut current = scored(
            make_candidate(
                "current",
                100.0,
                Some(now + 60),
                10.0,
                Some(now + 5 * 86400),
            ),
            20.0,
        );
        let eligible = scored(
            make_candidate(
                "eligible",
                90.0,
                Some(now + 4 * 3600),
                90.0,
                Some(now + 5 * 86400),
            ),
            20.0,
        );

        // Eligibility must dominate any score component (for example team or
        // reset timing bonuses) left on an account that cannot currently run.
        current.score = eligible.score + 10_000.0;
        assert!(!is_candidate_eligible(&current.candidate, 20.0));
        assert!(is_candidate_eligible(&eligible.candidate, 20.0));
        assert_eq!(
            pick_switch_target(&current, &[eligible], 20.0).map(|(alias, _)| alias),
            Some("eligible")
        );
    }

    #[test]
    fn test_adaptive_prefers_more_remaining() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 60.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(score_unified(&a, 20.0) > score_unified(&b, 20.0));
    }

    #[test]
    fn test_adaptive_team_priority_dominates() {
        let now = 1_000_000i64;
        // Non-team with 0% used vs Team with 50% used → Team wins with priority
        let a = make_candidate("a", 0.0, Some(now + 18000), 10.0, Some(now + 5 * 86400));
        let mut b = make_candidate("b", 50.0, Some(now + 7200), 10.0, Some(now + 5 * 86400));
        b.plan_kind = PlanKind::Team;
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sb > sa,
            "team account should beat non-team even with worse 5h: {sb} > {sa}"
        );
    }

    #[test]
    fn test_adaptive_team_priority_disabled() {
        let now = 1_000_000i64;
        // With team_priority=false, Team should not get +500 bonus
        let mut a = make_candidate("a", 0.0, Some(now + 18000), 10.0, Some(now + 5 * 86400));
        a.team_priority = false;
        let mut b = make_candidate("b", 50.0, Some(now + 7200), 10.0, Some(now + 5 * 86400));
        b.plan_kind = PlanKind::Team;
        b.team_priority = false;
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sa > sb,
            "without team_priority, more remaining should win: {sa} > {sb}"
        );
    }

    #[test]
    fn test_adaptive_drain_near_reset() {
        let now = 1_000_000i64;
        // Account A: 40% used, resets in 30 min (within drain window)
        let a = make_candidate("a", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        // Account B: 40% used, resets in 4h (outside drain window)
        let b = make_candidate("b", 40.0, Some(now + 14400), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sa > sb,
            "near-reset account should score higher due to drain: {sa} > {sb}"
        );
    }

    #[test]
    fn past_reset_never_receives_drain_bonus() {
        let now = 1_000_000;
        let past_reset = make_candidate("past", 90.0, Some(now - 1), 20.0, Some(now + 5 * 86_400));
        let unused = make_candidate("unused", 0.0, Some(now - 1), 20.0, Some(now + 5 * 86_400));

        assert_eq!(
            score_unified(&past_reset, 20.0),
            score_unified(&unused, 20.0)
        );
    }

    #[test]
    fn scoring_uses_the_primary_windows_actual_duration() {
        let now = 1_000_000;
        let mut ten_hour = make_candidate(
            "ten-hour",
            25.0,
            Some(now + 5 * 3_600),
            20.0,
            Some(now + 5 * 86_400),
        );
        ten_hour.primary.as_mut().unwrap().duration_secs = 10 * 3_600;
        let five_hour = make_candidate(
            "five-hour",
            50.0,
            Some(now + 4 * 3_600),
            20.0,
            Some(now + 5 * 86_400),
        );

        assert!(score_unified(&ten_hour, 20.0) > score_unified(&five_hour, 20.0));

        let usage = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(1.0),
                resets_at: Some(now + 10 * 3_600 - MIN_WARMUP_ELAPSED_SECS),
                window_minutes: Some(600),
            }),
            secondary: Some(window(1.0, Some(now + WINDOW_7D_SECS - 3_600))),
            ..UsageInfo::default()
        };
        assert!(usage_has_active_warmup_window(&usage, now));
    }

    #[test]
    fn exhausted_weekly_relief_uses_the_windows_actual_duration() {
        let now = 1_000_000;
        let seven_day = make_candidate(
            "seven-day",
            20.0,
            Some(now + 4 * 3_600),
            100.0,
            Some(now + WINDOW_7D_SECS / 2),
        );
        let mut fourteen_day = seven_day.clone();
        fourteen_day.alias = "fourteen-day".to_string();
        let fourteen_day_secs = 2 * WINDOW_7D_SECS;
        let weekly = fourteen_day.weekly.as_mut().unwrap();
        weekly.duration_secs = fourteen_day_secs;
        weekly.resets_at = Some(now + fourteen_day_secs / 2);

        let seven_day_score = score_unified(&seven_day, 20.0);
        let fourteen_day_score = score_unified(&fourteen_day, 20.0);

        assert!((seven_day_score - fourteen_day_score).abs() < 1e-9);
    }

    #[test]
    fn test_adaptive_no_drain_outside_window() {
        let now = 1_000_000i64;
        // Both accounts reset in 2h+ (outside 60-min drain window)
        // A: 40% used, resets in 2h → elapsed 3h → burn=40/3h → low rate, more headroom
        // B: 40% used, resets in 4h → elapsed 1h → burn=40/1h → high rate, less headroom
        let a = make_candidate("a", 40.0, Some(now + 7200), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 40.0, Some(now + 14400), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sa > 1000.0 && sb > 1000.0,
            "both should be usable: {sa}, {sb}"
        );
        // A consumed 40% over 3h (lower burn rate) → more projected headroom
        assert!(sa > sb, "lower burn rate gives more headroom: {sa} > {sb}");
    }

    #[test]
    fn test_adaptive_7d_critical_overrides_5h() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 0.0, Some(now + 18000), 95.0, Some(now + 6 * 86400));
        let b = make_candidate("b", 50.0, Some(now + 7200), 30.0, Some(now + 5 * 86400));
        assert!(
            score_unified(&b, 20.0) > score_unified(&a, 20.0),
            "7d-critical should lose"
        );
    }

    #[test]
    fn test_adaptive_7d_budget_per_window() {
        let now = 1_000_000i64;
        // Account A: 7d 15% remaining, resets in 3 windows (15h) → 5%/window (tight)
        let a = make_candidate("a", 30.0, Some(now + 3600), 85.0, Some(now + 15 * 3600));
        // Account B: 7d 15% remaining, resets in 1 window (5h) → 15%/window (ok)
        let b = make_candidate("b", 30.0, Some(now + 3600), 85.0, Some(now + 5 * 3600));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sb > sa,
            "higher budget-per-window should score better: {sb} > {sa}"
        );
    }

    #[test]
    fn test_adaptive_recency_breaks_tie() {
        let now = 1_000_000i64;
        let mut a = make_candidate("a", 40.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        a.last_used = now - 5; // used 5 seconds ago
        let mut b = make_candidate("b", 40.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        b.last_used = now - 1200; // used 20 minutes ago
        assert!(
            score_unified(&b, 20.0) > score_unified(&a, 20.0),
            "recently-used should score lower"
        );
    }

    #[test]
    fn test_adaptive_reset_aware() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 80.0, Some(now - 600), 20.0, Some(now + 5 * 86400));
        let score = score_unified(&a, 20.0);
        assert!(
            score > 1000.0,
            "past-reset account should score as fully available, got {score}"
        );
    }

    #[test]
    fn test_adaptive_exhausted_scores_low() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 100.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 50.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "exhausted should score much lower: {sb} > {sa}");
        assert!(sa < 500.0, "exhausted score should be low: {sa}");
    }

    #[test]
    fn test_adaptive_pool_exhausted_conservative_drain() {
        let now = 1_000_000i64;
        // Most accounts exhausted → drain weight should be low
        let mut a = make_candidate("a", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        a.pool_size = 10;
        a.pool_exhausted = 8; // 80% exhausted
        let mut b = make_candidate("b", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        b.pool_size = 10;
        b.pool_exhausted = 1; // 10% exhausted
        // Both should have drain but b's pool allows more aggressive drain
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "healthy pool should allow more drain: {sb} > {sa}");
    }

    #[test]
    fn test_adaptive_free_floor_ineligible() {
        let now = 1_000_000i64;
        let mut c = make_candidate("free1", 70.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        c.plan_kind = PlanKind::Free;
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_adaptive_no_data_low_score() {
        let c = Candidate {
            alias: "unknown".to_string(),
            primary: None,
            weekly: None,
            explicit_account_blocker: None,
            ordinary_account_limit: None,
            plan_kind: PlanKind::Unknown,
            last_used: 0,
            now: 1_000_000,
            pool_size: 1,
            pool_exhausted: 0,
            team_priority: true,
        };
        // headroom=50 (no 5h data) + sustain=-50 (no 7d data) = 0
        assert_eq!(
            score_unified(&c, 20.0),
            0.0,
            "no-data account should score exactly 0"
        );
    }

    #[test]
    fn test_adaptive_both_windows_exhausted() {
        let now = 1_000_000i64;
        // 5h exhausted (no reset info) + 7d exhausted (resets in 7 days)
        let c = make_candidate("both_dead", 100.0, None, 100.0, Some(now + 7 * 86400));
        let s = score_unified(&c, 20.0);
        // headroom=0 (exhausted, no reset), sustain should still be heavily negative
        assert!(
            s < -700.0,
            "doubly-exhausted account must score very low, got {s}"
        );
    }

    #[test]
    fn test_adaptive_both_windows_exhausted_no_reset_info() {
        // Worst case: both exhausted, no reset info at all
        let c = Candidate {
            alias: "dead".to_string(),
            primary: Some(super::super::CandidateWindow {
                used_percent: 100.0,
                resets_at: None,
                duration_secs: WINDOW_5H_SECS,
            }),
            weekly: Some(super::super::CandidateWindow {
                used_percent: 100.0,
                resets_at: None,
                duration_secs: WINDOW_7D_SECS,
            }),
            explicit_account_blocker: None,
            ordinary_account_limit: None,
            plan_kind: PlanKind::Plus,
            last_used: 0,
            now: 1_000_000,
            pool_size: 1,
            pool_exhausted: 1,
            team_priority: false,
        };
        let s = score_unified(&c, 20.0);
        assert!(
            s < -700.0,
            "doubly-exhausted no-reset account must score very low, got {s}"
        );
    }

    #[test]
    fn test_adaptive_pace_aware_headroom() {
        let now = 1_000_000i64;
        // Account A: 30% used, resets in 4h → elapsed 1h → burn=30%/3600s (fast)
        // projected exhaustion = 70 / (30/3600) / 60 ≈ 140 min
        let a = make_candidate("a", 30.0, Some(now + 4 * 3600), 20.0, Some(now + 5 * 86400));
        // Account B: 30% used, resets in 1h → elapsed 4h → burn=30%/14400s (slow)
        // projected exhaustion = 70 / (30/14400) / 60 ≈ 560 min → capped 300 min
        let b = make_candidate("b", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        // B has slower burn rate → higher projected exhaustion → higher headroom
        assert!(
            sb > sa,
            "slower burn rate should give higher headroom: {sb} > {sa}"
        );
    }

    #[test]
    fn test_candidate_eligible_basic() {
        let now = 1_000_000i64;
        let c = make_candidate("ok", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_ineligible_5h_exhausted() {
        let now = 1_000_000i64;
        let c = make_candidate("ex", 100.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_ineligible_7d_critical_far() {
        let now = 1_000_000i64;
        // 7d at 97% (3% remaining < critical 5%), resets in 5 days
        let c = make_candidate("crit", 30.0, Some(now + 3600), 97.0, Some(now + 5 * 86400));
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_eligible_7d_critical_near_reset() {
        let now = 1_000_000i64;
        // 7d at 97%, but resets in 12h → still eligible
        let c = make_candidate("near", 30.0, Some(now + 3600), 97.0, Some(now + 12 * 3600));
        assert!(is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn pace_marker_remains_visible_when_ui_rounds_remaining_to_zero() {
        let now = 1_000_000;
        let w = WindowUsage {
            used_percent: Some(99.6),
            resets_at: Some(now + 3600),
            window_minutes: None,
        };
        assert!(
            visible_pace_marker(w.used_percent, pace_percent_at(&w, WINDOW_5H_SECS, now)).is_some()
        );
    }

    #[test]
    fn quota_pace_state_owns_all_presentation_boundaries() {
        assert_eq!(
            quota_pace_state(Some(1.0), Some(0.0)),
            QuotaPaceState::UsageAhead
        );
        assert_eq!(
            quota_pace_state(Some(50.0), Some(50.0)),
            QuotaPaceState::PaceAheadOrEqual
        );
        assert_eq!(
            quota_pace_state(Some(95.0), Some(99.0)),
            QuotaPaceState::PaceAheadOrEqual
        );
        assert_eq!(
            quota_pace_state(Some(99.6), Some(50.0)),
            QuotaPaceState::UsageAhead
        );
        assert_eq!(
            quota_pace_state(Some(100.0), Some(50.0)),
            QuotaPaceState::UsageAhead
        );
        assert_eq!(
            quota_pace_state(Some(100.0), None),
            QuotaPaceState::Unavailable
        );
        assert_eq!(
            quota_pace_state(None, Some(20.0)),
            QuotaPaceState::Unavailable
        );
        assert_eq!(
            quota_pace_state(Some(f64::NAN), Some(20.0)),
            QuotaPaceState::Unavailable
        );
    }

    #[test]
    fn pace_requires_a_current_consistent_window() {
        let now = 1_000_000;
        let window = |resets_at| WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(resets_at),
            window_minutes: None,
        };

        assert_eq!(pace_percent_at(&window(now - 1), WINDOW_5H_SECS, now), None);
        assert_eq!(
            pace_percent_at(&window(now + WINDOW_5H_SECS + 60), WINDOW_5H_SECS, now,),
            None
        );
        assert_eq!(pace_percent_at(&window(now + 60), 0, now), None);
        assert_eq!(pace_percent_at(&window(now + 60), -1, now), None);
    }

    #[test]
    fn pace_marker_is_shown_at_exact_exhaustion() {
        let now = 1_000_000;
        let w = WindowUsage {
            used_percent: Some(100.0),
            resets_at: Some(now + 3600),
            window_minutes: None,
        };
        assert!(
            visible_pace_marker(w.used_percent, pace_percent_at(&w, WINDOW_5H_SECS, now)).is_some()
        );
    }

    #[test]
    fn visible_pace_marker_requires_known_usage_and_valid_pace() {
        assert_eq!(visible_pace_marker(None, Some(50.0)), None);
        assert_eq!(visible_pace_marker(Some(99.6), Some(50.0)), Some(50.0));
        assert_eq!(visible_pace_marker(Some(100.0), Some(50.0)), Some(50.0));
        assert_eq!(visible_pace_marker(Some(20.0), None), None);
        assert_eq!(visible_pace_marker(Some(20.0), Some(f64::NAN)), None);
    }

    #[test]
    fn test_warmup_window_active_requires_elapsed_threshold() {
        let now = 1_000_000i64;
        let just_started = WindowUsage {
            used_percent: Some(1.0),
            resets_at: Some(now + WINDOW_5H_SECS - 60),
            window_minutes: None,
        };
        let past_threshold = WindowUsage {
            used_percent: Some(1.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };

        assert!(!warmup_window_active(&just_started, WINDOW_5H_SECS, now));
        assert!(warmup_window_active(&past_threshold, WINDOW_5H_SECS, now));
    }

    #[test]
    fn test_warmup_window_active_requires_real_usage() {
        let now = 1_000_000i64;
        let no_usage = WindowUsage {
            used_percent: Some(0.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let no_reset = WindowUsage {
            used_percent: Some(1.0),
            resets_at: None,
            window_minutes: None,
        };

        assert!(!warmup_window_active(&no_usage, WINDOW_5H_SECS, now));
        assert!(!warmup_window_active(&no_reset, WINDOW_5H_SECS, now));
    }

    #[test]
    fn test_paid_account_with_expired_5h_but_active_7d_is_not_already_warmed() {
        // Regression: previously OR-ed primary and secondary, so a paid account
        // whose 7d window was still active (the normal case after any real use)
        // would never re-warm after its 5h window expired.
        let now = 1_000_000i64;
        let expired_5h = WindowUsage {
            used_percent: Some(99.0),
            resets_at: Some(now - 60), // already reset server-side
            window_minutes: None,
        };
        let active_7d = WindowUsage {
            used_percent: Some(40.0),
            resets_at: Some(now + WINDOW_7D_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let u = UsageInfo {
            primary: Some(expired_5h),
            secondary: Some(active_7d),
            ..Default::default()
        };
        assert!(!usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_paid_account_with_active_5h_is_already_warmed() {
        let now = 1_000_000i64;
        let active_5h = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let u = UsageInfo {
            primary: Some(active_5h),
            secondary: None,
            ..Default::default()
        };
        assert!(usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_inactive_future_model_pool_requires_warmup_even_when_main_pool_is_active() {
        let now = 1_000_000i64;
        let active_5h = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let inactive_future_pool = super::super::AdditionalRateLimit {
            limit_name: Some("GPT-6-Codex-Burst".to_string()),
            metered_feature: Some("codex_futureburst".to_string()),
            allowed: Some(true),
            limit_reached: Some(false),
            primary: None,
            secondary: None,
        };
        let u = UsageInfo {
            primary: Some(active_5h),
            additional_limits: vec![inactive_future_pool],
            ..Default::default()
        };

        assert!(!usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_code_review_pool_does_not_trigger_model_warmup() {
        let now = 1_000_000i64;
        let active_5h = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: Some(300),
        };
        let u = UsageInfo {
            primary: Some(active_5h),
            additional_limits: vec![super::super::AdditionalRateLimit {
                limit_name: Some("Code review".to_string()),
                metered_feature: Some("code_review".to_string()),
                allowed: Some(true),
                limit_reached: Some(false),
                primary: None,
                secondary: None,
            }],
            ..Default::default()
        };

        assert!(usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_free_account_falls_back_to_7d_window() {
        // Free accounts have primary=None (remapped to secondary in parse_usage).
        let now = 1_000_000i64;
        let active_7d = WindowUsage {
            used_percent: Some(10.0),
            resets_at: Some(now + WINDOW_7D_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let u = UsageInfo {
            primary: None,
            secondary: Some(active_7d),
            ..Default::default()
        };
        assert!(usage_has_active_warmup_window(&u, now));
    }
}
