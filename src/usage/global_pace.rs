use super::{UsageInfo, WINDOW_7D_SECS, WindowUsage, explicit_account_blocker};

const WINDOW_7D_MINUTES: i64 = WINDOW_7D_SECS / 60;

/// The weekly quota data for one registered profile.
///
/// Missing fields deliberately represent an unavailable account. Keeping that
/// state in the input means the aggregate can report both included and excluded
/// profile counts without knowing anything about fetch or authentication errors.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalPaceAccountInput {
    pub alias: String,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub window_duration_secs: Option<i64>,
    /// Optional comparable quota capacity. Production inputs currently leave
    /// this unset because the usage API does not expose a reliable capacity.
    pub capacity: Option<f64>,
}

impl GlobalPaceAccountInput {
    pub fn unavailable(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            used_percent: None,
            resets_at: None,
            window_duration_secs: None,
            capacity: None,
        }
    }

    /// Build an equal-weight production input from a fetched usage response.
    pub fn from_usage(alias: impl Into<String>, usage: &UsageInfo) -> Self {
        let alias = alias.into();
        if explicit_account_blocker(usage).is_some() {
            return Self::unavailable(alias);
        }
        let Some(window) = main_weekly_window(usage) else {
            return Self::unavailable(alias);
        };
        let window_duration_secs = match window.window_minutes {
            Some(minutes) => minutes.checked_mul(60),
            None => Some(WINDOW_7D_SECS),
        };

        Self {
            alias,
            used_percent: window.used_percent,
            resets_at: window.resets_at,
            window_duration_secs,
            capacity: None,
        }
    }
}

/// Per-account values after validating a weekly window.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountWeeklyPace {
    pub alias: String,
    pub used_percent: f64,
    pub elapsed_percent: f64,
    pub remaining_percent: f64,
    pub effective_capacity: f64,
    pub reserve_percent_points: f64,
    pub resets_at: i64,
    pub capacity: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalPaceWeighting {
    Equal,
    Capacity,
}

impl GlobalPaceWeighting {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Capacity => "capacity",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalWeeklySummary {
    /// `None` when no account has a valid current weekly window.
    pub pace_percent: Option<f64>,
    /// Global pace minus the 100% normal baseline.
    pub reserve_percent_points: Option<f64>,
    /// Equal- or capacity-weighted usage across the included weekly windows.
    pub aggregate_used_percent: Option<f64>,
    /// Equal- or capacity-weighted elapsed time across those same windows.
    pub aggregate_elapsed_percent: Option<f64>,
    /// Sum of account effective percentages, weighted when capacities exist.
    pub effective_capacity: f64,
    /// The corresponding normal baseline (`100` per unit of weight).
    pub normal_capacity: f64,
    pub included_accounts: usize,
    pub excluded_accounts: usize,
    pub weighting: GlobalPaceWeighting,
    pub next_reset_at: Option<i64>,
    pub next_reset_alias: Option<String>,
}

/// Effective quota an account contributes over the coming window.
pub fn calculate_effective_capacity(used_percent: f64, elapsed_percent: f64) -> f64 {
    100.0 - used_percent + elapsed_percent
}

/// Validate and calculate one account at a caller-supplied timestamp.
///
/// A reset at or before `now`, or more than one full window away, is stale or
/// inconsistent and excluded. Exact 100% usage remains valid: time until reset
/// still determines how much effective capacity it has. The fetch/cache layer's
/// existing TTL plus these reset bounds form the freshness boundary; this pure
/// calculation intentionally invents no second arbitrary sample-age policy.
pub fn calculate_account_weekly_pace(
    input: &GlobalPaceAccountInput,
    now: i64,
) -> Option<AccountWeeklyPace> {
    let used_percent = input.used_percent?;
    if !used_percent.is_finite() || !(0.0..=100.0).contains(&used_percent) {
        return None;
    }

    let resets_at = input.resets_at?;
    if resets_at <= now {
        return None;
    }

    let window_duration_secs = input.window_duration_secs?;
    if window_duration_secs <= 0 {
        return None;
    }

    let duration = window_duration_secs as f64;
    let time_until_reset = resets_at.checked_sub(now)?;
    if time_until_reset > window_duration_secs {
        return None;
    }
    let elapsed_secs = (duration - time_until_reset as f64).clamp(0.0, duration);
    let elapsed_percent = (elapsed_secs / duration * 100.0).clamp(0.0, 100.0);
    let remaining_percent = 100.0 - used_percent;
    let effective_capacity = calculate_effective_capacity(used_percent, elapsed_percent);
    let reserve_percent_points = elapsed_percent - used_percent;
    let capacity = input
        .capacity
        .filter(|value| value.is_finite() && *value > 0.0);

    Some(AccountWeeklyPace {
        alias: input.alias.clone(),
        used_percent,
        elapsed_percent,
        remaining_percent,
        effective_capacity,
        reserve_percent_points,
        resets_at,
        capacity,
    })
}

/// Aggregate every registered profile into one weekly pool.
///
/// Capacity weighting is used only when every included account supplies a
/// finite positive comparable capacity. A partial set falls back wholly to
/// equal weighting rather than mixing unlike units.
pub fn calculate_global_weekly_summary(
    inputs: &[GlobalPaceAccountInput],
    now: i64,
) -> GlobalWeeklySummary {
    let accounts: Vec<AccountWeeklyPace> = inputs
        .iter()
        .filter_map(|input| calculate_account_weekly_pace(input, now))
        .collect();
    let included_accounts = accounts.len();
    let excluded_accounts = inputs.len().saturating_sub(included_accounts);

    let next_reset = accounts.iter().min_by(|left, right| {
        left.resets_at
            .cmp(&right.resets_at)
            .then_with(|| left.alias.cmp(&right.alias))
    });

    let capacity_totals = (!accounts.is_empty()
        && accounts.iter().all(|account| account.capacity.is_some()))
    .then(|| {
        accounts
            .iter()
            .fold((0.0, 0.0, 0.0, 0.0), |totals, account| {
                let weight = account.capacity.expect("capacity checked above");
                let (effective, normal, used, elapsed) = totals;
                (
                    effective + weight * account.effective_capacity,
                    normal + weight * 100.0,
                    used + weight * account.used_percent,
                    elapsed + weight * account.elapsed_percent,
                )
            })
    })
    .filter(|(effective, normal, used, elapsed)| {
        effective.is_finite()
            && normal.is_finite()
            && used.is_finite()
            && elapsed.is_finite()
            && *normal > 0.0
    });

    let (
        weighting,
        effective_capacity,
        normal_capacity,
        aggregate_used_percent,
        aggregate_elapsed_percent,
    ) = match capacity_totals {
        Some((effective, normal, weighted_used, weighted_elapsed)) => (
            GlobalPaceWeighting::Capacity,
            effective,
            normal,
            Some(weighted_used / normal * 100.0),
            Some(weighted_elapsed / normal * 100.0),
        ),
        None => {
            let account_count = included_accounts as f64;
            let averages = (included_accounts > 0).then(|| {
                (
                    accounts
                        .iter()
                        .map(|account| account.used_percent)
                        .sum::<f64>()
                        / account_count,
                    accounts
                        .iter()
                        .map(|account| account.elapsed_percent)
                        .sum::<f64>()
                        / account_count,
                )
            });
            (
                GlobalPaceWeighting::Equal,
                accounts
                    .iter()
                    .fold(0.0, |total, account| total + account.effective_capacity),
                account_count * 100.0,
                averages.map(|values| values.0),
                averages.map(|values| values.1),
            )
        }
    };

    let pace_percent = if normal_capacity > 0.0 {
        Some(effective_capacity / normal_capacity * 100.0)
    } else {
        None
    };

    GlobalWeeklySummary {
        pace_percent,
        reserve_percent_points: pace_percent.map(|pace| pace - 100.0),
        aggregate_used_percent,
        aggregate_elapsed_percent,
        effective_capacity,
        normal_capacity,
        included_accounts,
        excluded_accounts,
        weighting,
        next_reset_at: next_reset.map(|account| account.resets_at),
        next_reset_alias: next_reset.map(|account| account.alias.clone()),
    }
}

/// Select the main Codex weekly window, excluding model-specific pools.
///
/// Current parsing normalizes primary-only seven-day responses into
/// `secondary`. The primary fallback keeps this layer robust when it receives a
/// pre-normalized `UsageInfo` directly. A secondary window without duration is
/// accepted as the legacy seven-day shape.
fn main_weekly_window(usage: &UsageInfo) -> Option<&WindowUsage> {
    usage
        .secondary
        .as_ref()
        .filter(|window| {
            window
                .window_minutes
                .is_none_or(|minutes| minutes == WINDOW_7D_MINUTES)
        })
        .or_else(|| {
            usage.primary.as_ref().filter(|window| {
                window
                    .window_minutes
                    .is_some_and(|minutes| minutes == WINDOW_7D_MINUTES)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn input(alias: &str, used_percent: f64, elapsed_percent: i64) -> GlobalPaceAccountInput {
        let elapsed_secs = WINDOW_7D_SECS * elapsed_percent / 100;
        GlobalPaceAccountInput {
            alias: alias.to_string(),
            used_percent: Some(used_percent),
            resets_at: Some(NOW + WINDOW_7D_SECS - elapsed_secs),
            window_duration_secs: Some(WINDOW_7D_SECS),
            capacity: None,
        }
    }

    fn account(used_percent: f64, elapsed_percent: i64) -> AccountWeeklyPace {
        calculate_account_weekly_pace(&input("account", used_percent, elapsed_percent), NOW)
            .expect("valid account")
    }

    fn assert_near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn assert_summary_identity(summary: &GlobalWeeklySummary) {
        let pace = summary.pace_percent.expect("valid pace");
        let used = summary
            .aggregate_used_percent
            .expect("valid aggregate usage");
        let elapsed = summary
            .aggregate_elapsed_percent
            .expect("valid aggregate elapsed time");
        assert_near(pace, 100.0 - used + elapsed);
        assert_near(
            summary.reserve_percent_points.expect("valid reserve"),
            elapsed - used,
        );
    }

    #[test]
    fn normal_pace_has_one_hundred_effective_and_zero_reserve() {
        let pace = account(50.0, 50);
        assert_near(pace.elapsed_percent, 50.0);
        assert_near(pace.remaining_percent, 50.0);
        assert_near(pace.effective_capacity, 100.0);
        assert_near(pace.reserve_percent_points, 0.0);
    }

    #[test]
    fn slower_usage_creates_reserve() {
        let pace = account(50.0, 70);
        assert_near(pace.effective_capacity, 120.0);
        assert_near(pace.reserve_percent_points, 20.0);
    }

    #[test]
    fn faster_usage_creates_deficit() {
        let pace = account(70.0, 30);
        assert_near(pace.effective_capacity, 60.0);
        assert_near(pace.reserve_percent_points, -40.0);
    }

    #[test]
    fn one_percent_remaining_near_reset_is_normal() {
        let pace = account(99.0, 99);
        assert_near(pace.effective_capacity, 100.0);
        assert_near(pace.reserve_percent_points, 0.0);
    }

    #[test]
    fn one_percent_remaining_far_from_reset_is_a_large_deficit() {
        let pace = account(99.0, 10);
        assert_near(pace.effective_capacity, 11.0);
        assert_near(pace.reserve_percent_points, -89.0);
    }

    #[test]
    fn exhausted_account_near_reset_remains_included() {
        let pace = account(100.0, 95);
        assert_near(pace.effective_capacity, 95.0);
        assert_near(pace.reserve_percent_points, -5.0);
    }

    #[test]
    fn multi_account_summary_matches_equal_weight_example() {
        let inputs = vec![
            input("a", 0.0, 10),
            input("b", 20.0, 0),
            input("c", 0.0, 30),
        ];
        let summary = calculate_global_weekly_summary(&inputs, NOW);

        assert_eq!(summary.weighting, GlobalPaceWeighting::Equal);
        assert_near(summary.effective_capacity, 320.0);
        assert_near(summary.normal_capacity, 300.0);
        assert_near(summary.pace_percent.unwrap(), 106.66666666666667);
        assert_near(summary.reserve_percent_points.unwrap(), 6.666666666666671);
        assert_near(summary.aggregate_used_percent.unwrap(), 6.666666666666667);
        assert_near(
            summary.aggregate_elapsed_percent.unwrap(),
            13.333333333333334,
        );
        assert_summary_identity(&summary);
    }

    #[test]
    fn complete_capacity_data_uses_weighted_result() {
        let mut reserve = input("reserve", 50.0, 70);
        reserve.capacity = Some(1.0);
        let mut deficit = input("deficit", 50.0, 30);
        deficit.capacity = Some(3.0);

        let summary = calculate_global_weekly_summary(&[reserve, deficit], NOW);

        assert_eq!(summary.weighting, GlobalPaceWeighting::Capacity);
        assert_near(summary.effective_capacity, 360.0);
        assert_near(summary.normal_capacity, 400.0);
        assert_near(summary.pace_percent.unwrap(), 90.0);
        assert_near(summary.reserve_percent_points.unwrap(), -10.0);
        assert_near(summary.aggregate_used_percent.unwrap(), 50.0);
        assert_near(summary.aggregate_elapsed_percent.unwrap(), 40.0);
        assert_summary_identity(&summary);
    }

    #[test]
    fn partial_or_invalid_capacity_data_falls_back_to_equal_weighting() {
        let mut reserve = input("reserve", 50.0, 70);
        reserve.capacity = Some(f64::NAN);
        let mut deficit = input("deficit", 50.0, 30);
        deficit.capacity = Some(3.0);

        let summary = calculate_global_weekly_summary(&[reserve, deficit], NOW);

        assert_eq!(summary.weighting, GlobalPaceWeighting::Equal);
        assert_near(summary.effective_capacity, 200.0);
        assert_near(summary.normal_capacity, 200.0);
        assert_near(summary.pace_percent.unwrap(), 100.0);
        assert_near(summary.aggregate_used_percent.unwrap(), 50.0);
        assert_near(summary.aggregate_elapsed_percent.unwrap(), 50.0);
    }

    #[test]
    fn capacity_that_overflows_the_normal_baseline_uses_equal_weighting() {
        let mut account = input("huge", 50.0, 0);
        account.capacity = Some(f64::MAX / 75.0);

        let summary = calculate_global_weekly_summary(&[account], NOW);

        assert_eq!(summary.weighting, GlobalPaceWeighting::Equal);
        assert_near(summary.effective_capacity, 50.0);
        assert_near(summary.normal_capacity, 100.0);
        assert_summary_identity(&summary);
    }

    #[test]
    fn invalid_and_stale_accounts_are_excluded_without_dropping_exhausted_accounts() {
        let mut stale = input("stale", 10.0, 50);
        stale.resets_at = Some(NOW);
        let mut invalid_used = input("invalid-used", 10.0, 50);
        invalid_used.used_percent = Some(101.0);
        let mut invalid_duration = input("invalid-duration", 10.0, 50);
        invalid_duration.window_duration_secs = Some(0);
        let exhausted = input("exhausted", 100.0, 95);
        let inputs = vec![
            GlobalPaceAccountInput::unavailable("missing"),
            stale,
            invalid_used,
            invalid_duration,
            exhausted,
        ];

        let summary = calculate_global_weekly_summary(&inputs, NOW);

        assert_eq!(summary.included_accounts, 1);
        assert_eq!(summary.excluded_accounts, 4);
        assert_near(summary.effective_capacity, 95.0);
        assert_near(summary.aggregate_used_percent.unwrap(), 100.0);
        assert_near(summary.aggregate_elapsed_percent.unwrap(), 95.0);
        assert_summary_identity(&summary);
        assert_eq!(summary.next_reset_alias.as_deref(), Some("exhausted"));
    }

    #[test]
    fn reset_more_than_one_window_away_is_inconsistent() {
        let mut future = input("future", 20.0, 0);
        future.resets_at = Some(NOW + WINDOW_7D_SECS + 60);

        assert!(calculate_account_weekly_pace(&future, NOW).is_none());
    }

    #[test]
    fn next_reset_ties_are_deterministic_by_alias() {
        let reset = NOW + WINDOW_7D_SECS / 2;
        let mut zeta = input("zeta", 50.0, 50);
        zeta.resets_at = Some(reset);
        let mut alpha = input("alpha", 50.0, 50);
        alpha.resets_at = Some(reset);

        let summary = calculate_global_weekly_summary(&[zeta, alpha], NOW);

        assert_eq!(summary.next_reset_at, Some(reset));
        assert_eq!(summary.next_reset_alias.as_deref(), Some("alpha"));
    }

    #[test]
    fn no_valid_accounts_returns_nullable_pace_and_counts_unavailable() {
        let summary = calculate_global_weekly_summary(
            &[
                GlobalPaceAccountInput::unavailable("one"),
                GlobalPaceAccountInput::unavailable("two"),
            ],
            NOW,
        );

        assert_eq!(summary.pace_percent, None);
        assert_eq!(summary.reserve_percent_points, None);
        assert_eq!(summary.aggregate_used_percent, None);
        assert_eq!(summary.aggregate_elapsed_percent, None);
        assert_eq!(summary.effective_capacity, 0.0);
        assert!(summary.effective_capacity.is_sign_positive());
        assert_eq!(summary.normal_capacity, 0.0);
        assert_eq!(summary.included_accounts, 0);
        assert_eq!(summary.excluded_accounts, 2);
        assert_eq!(summary.next_reset_at, None);
        assert_eq!(summary.next_reset_alias, None);
    }

    #[test]
    fn usage_mapping_prefers_main_secondary_weekly_window() {
        let usage = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(25.0),
                resets_at: Some(NOW + 3_600),
                window_minutes: Some(300),
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(40.0),
                resets_at: Some(NOW + WINDOW_7D_SECS / 2),
                window_minutes: Some(WINDOW_7D_MINUTES),
            }),
            ..UsageInfo::default()
        };

        let mapped = GlobalPaceAccountInput::from_usage("account", &usage);

        assert_eq!(mapped.used_percent, Some(40.0));
        assert_eq!(mapped.window_duration_secs, Some(WINDOW_7D_SECS));
        assert_eq!(mapped.capacity, None);
    }

    #[test]
    fn usage_mapping_accepts_primary_only_weekly_but_not_primary_five_hour_window() {
        let weekly = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(25.0),
                resets_at: Some(NOW + WINDOW_7D_SECS),
                window_minutes: Some(WINDOW_7D_MINUTES),
            }),
            ..UsageInfo::default()
        };
        let five_hour = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(25.0),
                resets_at: Some(NOW + 3_600),
                window_minutes: Some(300),
            }),
            ..UsageInfo::default()
        };

        assert_eq!(
            GlobalPaceAccountInput::from_usage("weekly", &weekly).used_percent,
            Some(25.0)
        );
        assert_eq!(
            GlobalPaceAccountInput::from_usage("five-hour", &five_hour).used_percent,
            None
        );
    }

    #[test]
    fn usage_mapping_excludes_explicit_workspace_blocker_but_keeps_generic_exhaustion() {
        let window = WindowUsage {
            used_percent: Some(100.0),
            resets_at: Some(NOW + WINDOW_7D_SECS / 20),
            window_minutes: Some(WINDOW_7D_MINUTES),
        };
        let blocked = UsageInfo {
            secondary: Some(window.clone()),
            account_limited: true,
            rate_limit_reached_type: Some("workspace_member_usage_limit_reached".to_string()),
            ..UsageInfo::default()
        };
        let exhausted = UsageInfo {
            secondary: Some(window),
            account_limited: true,
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            ..UsageInfo::default()
        };
        let unknown = UsageInfo {
            secondary: exhausted.secondary.clone(),
            account_limited: true,
            rate_limit_reached_type: Some("future_server_reason".to_string()),
            ..UsageInfo::default()
        };

        assert_eq!(
            GlobalPaceAccountInput::from_usage("blocked", &blocked).used_percent,
            None
        );
        assert_eq!(
            GlobalPaceAccountInput::from_usage("exhausted", &exhausted).used_percent,
            Some(100.0)
        );
        assert_eq!(
            GlobalPaceAccountInput::from_usage("unknown", &unknown).used_percent,
            None
        );
    }

    #[test]
    fn generic_weekly_exhaustion_still_contributes_zero_remaining_capacity() {
        let usage = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(100.0),
                resets_at: Some(NOW + WINDOW_7D_SECS / 20),
                window_minutes: Some(WINDOW_7D_MINUTES),
            }),
            account_limited: true,
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            ..UsageInfo::default()
        };
        let input = GlobalPaceAccountInput::from_usage("exhausted", &usage);
        let summary = calculate_global_weekly_summary(&[input], NOW);

        assert_eq!(summary.included_accounts, 1);
        assert_eq!(summary.excluded_accounts, 0);
        assert_eq!(summary.aggregate_used_percent, Some(100.0));
    }

    #[test]
    fn primary_exhaustion_does_not_remove_a_valid_weekly_window() {
        let usage = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(100.0),
                resets_at: Some(NOW + 3_600),
                window_minutes: Some(300),
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(40.0),
                resets_at: Some(NOW + WINDOW_7D_SECS / 2),
                window_minutes: Some(WINDOW_7D_MINUTES),
            }),
            account_limited: true,
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            ..UsageInfo::default()
        };
        let input = GlobalPaceAccountInput::from_usage("primary-exhausted", &usage);
        let summary = calculate_global_weekly_summary(&[input], NOW);

        assert_eq!(summary.included_accounts, 1);
        assert_eq!(summary.excluded_accounts, 0);
        assert_eq!(summary.aggregate_used_percent, Some(40.0));
    }

    #[test]
    fn informational_spend_limit_does_not_exclude_ordinary_weekly_exhaustion() {
        let usage = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(100.0),
                resets_at: Some(NOW + WINDOW_7D_SECS / 20),
                window_minutes: Some(WINDOW_7D_MINUTES),
            }),
            account_limited: true,
            individual_limit: Some(Box::new(crate::usage::SpendControlLimit::default())),
            ..UsageInfo::default()
        };

        assert_eq!(
            GlobalPaceAccountInput::from_usage("exhausted", &usage).used_percent,
            Some(100.0)
        );
    }

    #[test]
    fn reached_spend_control_without_individual_limit_is_excluded() {
        let usage = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(25.0),
                resets_at: Some(NOW + WINDOW_7D_SECS / 2),
                window_minutes: Some(WINDOW_7D_MINUTES),
            }),
            account_limited: true,
            spend_control_reached: true,
            ..UsageInfo::default()
        };

        assert_eq!(
            GlobalPaceAccountInput::from_usage("blocked", &usage).used_percent,
            None
        );
    }
}
