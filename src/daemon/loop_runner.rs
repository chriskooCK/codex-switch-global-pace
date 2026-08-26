use anyhow::{Context, Result};

use super::state::{self, DaemonState, PendingSwitch, SwitchRecord};
use crate::signals::ShutdownListener;
use crate::{auth, cache, config, profile, task_batch, usage, warmup};

async fn shutdown_request_received() {
    loop {
        if super::pidfile::shutdown_requested() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Outcome of one monitor poll.
enum PollOutcome {
    NoAction,
    Switched {
        from: String,
        to: String,
        score: f64,
    },
    Deferred {
        to: String,
    },
}

/// Backoff after failed polls, capped at the configuration-validated horizon.
fn poll_backoff_secs(poll_secs: u64, consecutive_failures: u32) -> Result<u64> {
    let multiplier = 2u64
        .saturating_pow(consecutive_failures)
        .min(config::POLL_BACKOFF_MAX_MULTIPLIER);
    poll_secs
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("daemon poll backoff exceeds the supported timer range"))
}

fn current_usage_percent_for_switch(current_usage: &usage::UsageInfo) -> Option<f64> {
    // The threshold controls when to consider an optional optimization. It
    // must not suppress recovery from an account that cannot run at all,
    // including weekly exhaustion hidden behind a low primary-window value.
    if current_usage.account_limited
        || current_usage
            .primary
            .iter()
            .chain(current_usage.secondary.iter())
            .any(|window| window.used_percent.is_some_and(|used| used >= 100.0))
    {
        return Some(100.0);
    }

    current_usage
        .primary
        .as_ref()
        .or(current_usage.secondary.as_ref())
        .and_then(|w| w.used_percent)
}

fn switch_defer_reason(
    activity: super::codex_process::CodexActivity,
    defer_interactive_session: bool,
) -> Option<&'static str> {
    match activity {
        super::codex_process::CodexActivity::AuthMutation => {
            Some("Codex is changing authentication")
        }
        super::codex_process::CodexActivity::Unknown => Some("Codex process inspection failed"),
        super::codex_process::CodexActivity::Session if defer_interactive_session => {
            Some("a Codex session is running")
        }
        super::codex_process::CodexActivity::Idle
        | super::codex_process::CodexActivity::Session => None,
    }
}

#[derive(Debug)]
struct CandidateProfile {
    alias: String,
    path: std::path::PathBuf,
    info: crate::jwt::AccountInfo,
    last_used: i64,
}

fn preflight_candidate_profiles_with(
    profiles: &[String],
    current: &str,
    last_used: &std::collections::HashMap<String, i64>,
    mut resolve_path: impl FnMut(&str) -> Result<std::path::PathBuf>,
    mut read_info: impl FnMut(&str, &std::path::Path) -> Result<crate::jwt::AccountInfo>,
) -> Result<Vec<CandidateProfile>> {
    let mut candidates = Vec::with_capacity(profiles.len().saturating_sub(1));
    for alias in profiles {
        if alias == current {
            continue;
        }
        let path = resolve_path(alias)
            .with_context(|| format!("resolving candidate profile path: {alias}"))?;
        let info = read_info(alias, &path)
            .with_context(|| format!("reading candidate profile metadata: {alias}"))?;
        candidates.push(CandidateProfile {
            alias: alias.clone(),
            path,
            info,
            last_used: last_used.get(alias).copied().unwrap_or(0),
        });
    }
    Ok(candidates)
}

/// Main daemon event loop: periodically checks usage and switches account when needed.
pub async fn run_daemon_loop() -> Result<()> {
    // Registered before anything else can block: from here on every signal is
    // recorded, even while a branch body is busy.
    let mut shutdown = ShutdownListener::new()?;

    let cfg = config::get();
    let poll_secs = cfg.daemon.poll_interval_secs;
    let token_secs = cfg.daemon.token_check_interval_secs;
    let cache_refresh_secs = cfg.daemon.cache_refresh_interval_secs;
    let auto_warmup = cfg.daemon.auto_warmup;

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut token_interval = tokio::time::interval(std::time::Duration::from_secs(token_secs));
    token_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let cache_refresh_period = std::time::Duration::from_secs(cache_refresh_secs);
    let mut cache_refresh_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + cache_refresh_period,
        cache_refresh_period,
    );
    cache_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut st = DaemonState {
        pid: std::process::id(),
        started_at: auth::now_unix_secs()?,
        ..DaemonState::default()
    };
    state::write(&mut st);

    tracing::info!(
        "Daemon loop started: poll={}s, token_check={}s, cache_refresh={}s, auto_warmup={}, threshold={}%",
        poll_secs,
        token_secs,
        cache_refresh_secs,
        auto_warmup,
        cfg.daemon.switch_threshold,
    );

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                // Failure backoff suspends polling only; token and cache
                // timers keep running.
                let now = auth::now_unix_secs()?;
                if let Some(until) = st.backoff_until {
                    if now < until {
                        tracing::debug!("Poll suspended by backoff for {}s more", until - now);
                        continue;
                    }
                    st.backoff_until = None;
                }

                match check_and_switch().await {
                    Ok(outcome) => {
                        st.consecutive_failures = 0;
                        st.last_error = None;
                        st.last_poll_at = Some(auth::now_unix_secs()?);
                        match outcome {
                            PollOutcome::Switched { from, to, score } => {
                                tracing::info!("Account switch completed");
                                st.pending_switch = None;
                                st.last_switch = Some(SwitchRecord {
                                    from,
                                    to,
                                    at: auth::now_unix_secs()?,
                                    score,
                                });
                            }
                            PollOutcome::Deferred { to } => {
                                // Keep the original `since` while the same target stays pending.
                                let since = match st
                                    .pending_switch
                                    .as_ref()
                                    .filter(|p| p.to == to)
                                {
                                    Some(pending) => pending.since,
                                    None => auth::now_unix_secs()?,
                                };
                                st.pending_switch = Some(PendingSwitch { to, since });
                            }
                            PollOutcome::NoAction => {
                                st.pending_switch = None;
                            }
                        }
                    }
                    Err(e) => {
                        st.consecutive_failures += 1;
                        st.last_poll_at = Some(auth::now_unix_secs()?);
                        st.last_error = Some(e.to_string());
                        let backoff_secs = poll_backoff_secs(poll_secs, st.consecutive_failures)?;
                        let backoff_secs_i64 = i64::try_from(backoff_secs)
                            .context("daemon poll backoff exceeds persisted state range")?;
                        st.backoff_until = Some(
                            auth::now_unix_secs()?
                                .checked_add(backoff_secs_i64)
                                .context("daemon poll backoff timestamp overflowed")?,
                        );
                        tracing::error!(
                            "Monitor cycle failed ({}x): {e}, backing off {backoff_secs}s",
                            st.consecutive_failures
                        );
                    }
                }
                state::write(&mut st);
            }
            _ = token_interval.tick() => {
                // Runs unattended on a timer: a lost write here bricks the
                // profile with nobody watching, so it gets ERROR, not debug.
                match usage::refresh_expiring_tokens().await {
                    Ok(failures) => for failure in failures {
                        // `detail` already opens with `[alias]` and carries the
                        // underlying IO/permission cause; the field makes the
                        // affected profile filterable in structured log output.
                        tracing::error!(alias = %failure.alias, "{}", failure.error.detail);
                    },
                    Err(error) => tracing::error!(
                        "opportunistic token refresh could not start safely: {error:#}"
                    ),
                }
            }
            _ = cache_refresh_interval.tick() => {
                match refresh_profile_cache(auto_warmup).await {
                    Ok(summary) => tracing::debug!(
                        "Cache refresh completed: refreshed={}, warmed={}, failed={}",
                        summary.refreshed,
                        summary.warmed,
                        summary.failed
                    ),
                    Err(e) => tracing::warn!("Cache refresh skipped: {e}"),
                }
                st.last_cache_refresh_at = Some(auth::now_unix_secs()?);
                state::write(&mut st);
            }
            _ = shutdown.recv() => {
                tracing::info!("Received shutdown signal, exiting daemon loop");
                break;
            }
            _ = shutdown_request_received() => {
                tracing::info!("Received generation-bound shutdown request, exiting daemon loop");
                break;
            }
        }
    }
    Ok(())
}

/// Check current account usage and switch to a better candidate if threshold exceeded.
async fn check_and_switch() -> Result<PollOutcome> {
    let profiles = profile::list_profiles()?;
    if profiles.len() < 2 {
        return Ok(PollOutcome::NoAction);
    }

    let Some(current) = profile::sync_current_from_live()? else {
        tracing::debug!("No saved profile matches the live Codex authentication");
        return Ok(PollOutcome::NoAction);
    };

    let cfg = config::get();
    let safety_7d = cfg.use_cfg.safety_margin_7d;
    let threshold = cfg.daemon.switch_threshold;
    let now = auth::now_unix_secs()?;

    // 1. Force-fetch current account's usage (bypass cache)
    let current_path = profile::profile_auth_path(&current)?;
    let current_usage = usage::fetch_usage_retried_unattended(&current, &current_path)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.detail))?;

    // 2. Check if current account exceeds threshold
    // Free accounts have no primary window (7d is remapped to secondary),
    // so fall back to secondary when primary is absent.
    let Some(current_used) = current_usage_percent_for_switch(&current_usage) else {
        tracing::warn!(
            "Current account '{}' has no usable quota-window percentage; skipping automatic switch",
            current,
        );
        return Ok(PollOutcome::NoAction);
    };

    if current_used < threshold {
        tracing::debug!(
            "Current account '{}' at {:.1}%, below threshold {:.1}%",
            current,
            current_used,
            threshold,
        );
        return Ok(PollOutcome::NoAction);
    }

    tracing::info!(
        "Current account '{}' at {:.1}%, above threshold {:.1}% -- searching for better candidate",
        current,
        current_used,
        threshold,
    );

    let last_used = cache::last_used_snapshot_checked()
        .context("loading profile-selection history for automatic ranking")?;
    let current_info = auth::read_account_info_checked(&current_path).with_context(|| {
        format!("reading current profile metadata for automatic ranking: {current}")
    })?;

    // 3. Fetch all other candidates concurrently
    let team_priority = cfg.use_cfg.team_priority;
    let candidates = preflight_candidate_profiles_with(
        &profiles,
        &current,
        &last_used,
        profile::profile_auth_path,
        |_alias, path| auth::read_account_info_checked(path),
    )?;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(cfg.network.max_concurrent));
    let mut tasks = tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();

    for candidate in candidates {
        let tracked_alias = candidate.alias.clone();
        let semaphore = semaphore.clone();
        let task = tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .context("candidate usage limiter closed")?;
            let fetched = usage::fetch_usage_retried(&candidate.alias, &candidate.path).await;
            Ok::<_, anyhow::Error>((candidate.info, candidate.last_used, fetched))
        });
        let previous = task_aliases.insert(task.id(), tracked_alias);
        debug_assert!(previous.is_none());
    }

    // 4. Score everything uniformly (same helper as CLI `use`); the current
    // account goes first so it can be split back off after scoring.
    let mut items = vec![(
        current.clone(),
        current_usage.clone(),
        current_info,
        last_used.get(&current).copied().unwrap_or(0),
    )];
    let outcomes = task_batch::drain_named_tasks(&mut tasks, &mut task_aliases, |_| {}).await;
    let mut worker_failures = Vec::new();
    for outcome in outcomes {
        match outcome {
            task_batch::NamedTaskOutcome::Completed { alias, value } => match value {
                Ok((info, alias_last_used, fetched)) => match fetched {
                    Ok(fetched) => items.push((alias, fetched, info, alias_last_used)),
                    Err(error) => {
                        tracing::warn!("[{alias}] fetch failed: {}", error.summary);
                    }
                },
                Err(error) => worker_failures.push((alias, format!("{error:#}"))),
            },
            task_batch::NamedTaskOutcome::Failed { alias, detail } => {
                worker_failures.push((alias, detail));
            }
        }
    }
    if !worker_failures.is_empty() {
        return Err(task_batch::batch_failure_error(
            "one or more automatic-selection candidate workers failed",
            worker_failures,
        ));
    }

    let mut scored = usage::score_candidates(items, now, safety_7d, team_priority);
    let current_scored = scored.remove(0);
    let current_score = current_scored.score;

    // 5. Switch if a better candidate was found
    if let Some((best_alias, best_score)) =
        usage::pick_switch_target(&current_scored, &scored, safety_7d)
    {
        let (best_alias, best_score) = (best_alias.to_string(), best_score);
        // `codex login` and `codex logout` mutate the same live auth file and
        // are always protected. Interactive sessions remain configurable, but
        // an inspection failure is never interpreted as permission to replace
        // credentials.
        let activity = super::codex_process::codex_activity();
        let defer_reason =
            switch_defer_reason(activity, cfg.daemon.defer_switch_while_codex_running);
        if let Some(reason) = defer_reason {
            tracing::info!(
                "Deferring switch '{}' -> '{}': {}",
                current,
                best_alias,
                reason,
            );
            return Ok(PollOutcome::Deferred { to: best_alias });
        }

        tracing::info!(
            "Switching: '{}' (score {:.1}) -> '{}' (score {:.1})",
            current,
            current_score,
            best_alias,
            best_score,
        );
        let Some(switch_outcome) = profile::switch_profile_if_current(&current, &best_alias)?
        else {
            tracing::info!(
                "Skipping stale daemon switch '{}' -> '{}': the active profile changed during the poll",
                current,
                best_alias,
            );
            return Ok(PollOutcome::NoAction);
        };
        if let Some(error) = switch_outcome.selection_history_warning() {
            tracing::warn!(
                alias = best_alias,
                "profile switched but selection history was not recorded: {error:#}"
            );
        }

        if cfg.daemon.notify {
            super::notify::send_notification(&format!(
                "Switched to '{best_alias}' (score: {best_score:.0})"
            ));
        }
        return Ok(PollOutcome::Switched {
            from: current,
            to: best_alias,
            score: best_score,
        });
    }

    tracing::debug!("No better candidate found");
    Ok(PollOutcome::NoAction)
}

#[cfg(test)]
mod tests {
    use super::{
        current_usage_percent_for_switch, poll_backoff_secs, preflight_candidate_profiles_with,
        switch_defer_reason,
    };
    use crate::daemon::codex_process::CodexActivity;
    use crate::usage::{UsageInfo, WindowUsage};
    use anyhow::Result;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn poll_backoff_doubles_and_caps_at_sixteen_intervals() -> Result<()> {
        assert_eq!(poll_backoff_secs(60, 1)?, 120);
        assert_eq!(poll_backoff_secs(60, 2)?, 240);
        assert_eq!(poll_backoff_secs(60, 4)?, 960);
        assert_eq!(poll_backoff_secs(60, 10)?, 960);
        assert!(poll_backoff_secs(u64::MAX, 1).is_err());
        Ok(())
    }

    #[test]
    fn account_limited_usage_bypasses_low_usage_switch_threshold() {
        let usage = UsageInfo {
            account_limited: true,
            primary: Some(WindowUsage {
                used_percent: Some(1.0),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };

        assert_eq!(current_usage_percent_for_switch(&usage), Some(100.0));
    }

    #[test]
    fn auth_mutation_and_failed_inspection_always_defer_switching() {
        for defer_sessions in [false, true] {
            assert!(switch_defer_reason(CodexActivity::AuthMutation, defer_sessions).is_some());
            assert!(switch_defer_reason(CodexActivity::Unknown, defer_sessions).is_some());
        }
        assert!(switch_defer_reason(CodexActivity::Session, true).is_some());
        assert!(switch_defer_reason(CodexActivity::Session, false).is_none());
        assert!(switch_defer_reason(CodexActivity::Idle, true).is_none());
    }

    #[test]
    fn exhausted_weekly_usage_bypasses_low_primary_switch_threshold() {
        let usage = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(20.0),
                ..WindowUsage::default()
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(100.0),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };

        assert_eq!(current_usage_percent_for_switch(&usage), Some(100.0));
    }

    #[test]
    fn usage_without_any_quota_window_does_not_authorize_a_switch() {
        let credits_only = UsageInfo {
            credits_balance: Some(10.0),
            unlimited_credits: Some(true),
            ..UsageInfo::default()
        };

        assert_eq!(current_usage_percent_for_switch(&credits_only), None);
        assert_eq!(
            current_usage_percent_for_switch(&UsageInfo::default()),
            None
        );
    }

    #[test]
    fn quota_exhaustion_boundary_requires_a_full_hundred_percent() {
        let below = UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(20.0),
                ..WindowUsage::default()
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(99.999),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };
        let exhausted = UsageInfo {
            secondary: Some(WindowUsage {
                used_percent: Some(100.0),
                ..WindowUsage::default()
            }),
            ..below.clone()
        };

        assert_eq!(current_usage_percent_for_switch(&below), Some(20.0));
        assert_eq!(current_usage_percent_for_switch(&exhausted), Some(100.0));
    }

    #[tokio::test]
    async fn candidate_usage_requests_never_exceed_network_limit() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();

        for _ in 0..6 {
            let semaphore = semaphore.clone();
            let active = active.clone();
            let peak = peak.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.unwrap();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while tasks.join_next().await.is_some() {}
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn automatic_selection_preflights_every_candidate_before_spawning_workers() {
        let profiles = vec![
            "current".to_string(),
            "alice".to_string(),
            "bob".to_string(),
        ];
        let path_reads = AtomicUsize::new(0);
        let info_reads = AtomicUsize::new(0);

        let error = preflight_candidate_profiles_with(
            &profiles,
            "current",
            &std::collections::HashMap::new(),
            |alias| {
                path_reads.fetch_add(1, Ordering::SeqCst);
                Ok(std::path::PathBuf::from(alias))
            },
            |alias, _path| {
                info_reads.fetch_add(1, Ordering::SeqCst);
                if alias == "bob" {
                    anyhow::bail!("injected later metadata failure");
                }
                Ok(crate::jwt::AccountInfo::default())
            },
        )
        .expect_err("a later preflight failure must reject the complete candidate batch");

        assert!(format!("{error:#}").contains("injected later metadata failure"));
        assert_eq!(path_reads.load(Ordering::SeqCst), 2);
        assert_eq!(info_reads.load(Ordering::SeqCst), 2);
    }
}

#[derive(Default)]
struct CacheRefreshSummary {
    refreshed: usize,
    warmed: usize,
    failed: usize,
}

async fn refresh_profile_cache(auto_warmup: bool) -> Result<CacheRefreshSummary> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        return Ok(CacheRefreshSummary::default());
    }

    let now = auth::now_unix_secs()?;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));
    let mut tasks = tokio::task::JoinSet::new();

    for alias in profiles {
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (
                    alias,
                    false,
                    false,
                    Some("usage limiter closed".to_string()),
                );
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(path) => path,
                Err(e) => return (alias, false, false, Some(e.to_string())),
            };

            let usage = match usage::fetch_usage_retried_unattended(&alias, &path).await {
                Ok(usage) => usage,
                Err(e) => return (alias, false, false, Some(e.summary)),
            };

            if !auto_warmup || usage::usage_has_active_warmup_window(&usage, now) {
                return (alias, true, false, None);
            }

            if let Err(e) = warmup::warmup_account(&alias, &path).await {
                return (alias, true, false, Some(format!("warmup failed: {e}")));
            }

            if let Err(e) = usage::fetch_usage_retried_unattended(&alias, &path).await {
                tracing::warn!("[{alias}] post-warmup cache refresh failed: {}", e.summary);
            }
            (alias, true, true, None)
        });
    }

    let mut summary = CacheRefreshSummary::default();
    while let Some(res) = tasks.join_next().await {
        let (alias, refreshed, warmed, err) = match res {
            Ok(value) => value,
            Err(e) => {
                summary.failed += 1;
                tracing::warn!("Cache refresh worker failed: {e}");
                continue;
            }
        };
        if refreshed {
            summary.refreshed += 1;
        }
        if warmed {
            summary.warmed += 1;
        }
        if let Some(err) = err {
            summary.failed += 1;
            tracing::warn!("[{alias}] cache refresh failed: {err}");
        }
    }

    Ok(summary)
}
