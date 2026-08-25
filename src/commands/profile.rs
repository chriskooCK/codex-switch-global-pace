use super::render::{confirm_default_no, print_usage_line};
use crate::output::{
    self, ProgressReporter, account_to_json, global_weekly_to_json, print_json, usage_to_json,
    user_println,
};
use crate::task_batch::{NamedTaskOutcome, batch_failure_error, drain_named_tasks};
use crate::{auth, cache, color, config, jwt, profile, usage, workspace};
use anyhow::{Context, Result};

/// Surface profiles whose rotated credentials could not be written.
///
/// The auth server has already invalidated their previous refresh token, so
/// staying quiet hands the user an account that stops working later with no
/// clue why. Printed to stderr so `--json` stdout stays machine-readable.
fn report_token_persist_failures(failures: &[usage::TokenPersistFailure]) {
    for failure in failures {
        eprintln!(
            "{}",
            color::error(&format!("Warning: {}", failure.error.detail))
        );
    }
}

// ── use ──────────────────────────────────────────────────

pub(crate) async fn use_cmd(alias: Option<&str>, json: bool, consume_card: bool) -> Result<()> {
    use std::io::IsTerminal;

    match alias {
        Some(a) => {
            profile::cmd_use(a, !json && std::io::stdin().is_terminal())?;
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a.to_string(),
                    action: "switched".into(),
                })?;
            }
        }
        None => best_cmd(json, consume_card).await?,
    }
    Ok(())
}

// ── list (all profiles + usage, concurrent) ──────────────

pub(crate) async fn list_cmd(force: bool, json: bool, auth_already_handled: bool) -> Result<()> {
    if !auth_already_handled {
        profile::auto_track_current()?;
    }

    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        if json {
            let summary = usage::calculate_global_weekly_summary(&[], crate::auth::now_unix_secs());
            print_json(&output::JsonUsageResult {
                profiles: vec![],
                global_weekly: global_weekly_to_json(&summary),
            })?;
        } else {
            println!("{}", color::dim("(no saved profiles)"));
        }
        return Ok(());
    }

    // Derive the active row from live credentials. A stale marker must not make
    // an unrelated profile look active while live auth is untracked.
    let current = profile::active_profile_from_live()?.unwrap_or_default();

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    struct ListRow {
        name: String,
        path: std::path::PathBuf,
        is_current: bool,
        info: jwt::AccountInfo,
        usage_result: Option<std::result::Result<usage::UsageInfo, usage::UsageError>>,
    }

    let mut rows: Vec<ListRow> = Vec::with_capacity(profiles.len());
    for name in profiles {
        let path = profile::profile_auth_path(&name)
            .with_context(|| format!("resolving profile path for '{name}'"))?;
        let info = auth::read_account_info_checked(&path)
            .with_context(|| format!("loading profile '{name}' for list output"))?;
        let usage_result = if force {
            None
        } else {
            cache::get(&name)?.map(Ok)
        };
        rows.push(ListRow {
            is_current: name == current,
            name,
            path,
            info,
            usage_result,
        });
    }

    let refresh_count = rows.iter().filter(|row| row.usage_result.is_none()).count();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Refreshing usage", refresh_count))
    };

    let mut tasks = tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();
    let usage_aliases: std::collections::HashSet<String> = rows
        .iter()
        .filter(|row| row.usage_result.is_none())
        .map(|row| row.name.clone())
        .collect();
    for (idx, row) in rows.iter().enumerate() {
        let needs_usage = row.usage_result.is_none();
        let needs_workspace = match row.info.account_id.as_deref() {
            Some(id) => force || !cache::workspace_name_is_known(id)?,
            None => false,
        };
        if !needs_usage && !needs_workspace {
            continue;
        }

        let alias = row.name.clone();
        let path = row.path.clone();
        let sem = semaphore.clone();
        let task = tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (
                    idx,
                    needs_usage.then(|| {
                        Err(usage::UsageError {
                            summary: "limiter closed".into(),
                            detail: "usage limiter closed".into(),
                        })
                    }),
                );
            };
            let usage_result = if needs_usage {
                Some(if force {
                    usage::fetch_usage_retried_force(&alias, &path).await
                } else {
                    usage::fetch_usage_retried(&alias, &path).await
                })
            } else {
                None
            };
            // Read auth after usage: that path may have refreshed and persisted the token.
            if let Ok(auth) = auth::read_auth(&path)
                && let Err(err) = workspace::refresh_for_auth_if_needed(&auth, force).await
            {
                tracing::debug!("[{alias}] workspace metadata unavailable: {err}");
            }
            (idx, usage_result)
        });
        let previous = task_aliases.insert(task.id(), row.name.clone());
        debug_assert!(previous.is_none());
    }

    let mut completed = 0usize;
    let outcomes = drain_named_tasks(&mut tasks, &mut task_aliases, |alias| {
        if usage_aliases.contains(alias) {
            completed += 1;
            if let Some(progress) = progress.as_mut() {
                progress.advance(completed);
            }
        }
    })
    .await;

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let mut worker_failures = Vec::new();
    for outcome in outcomes {
        match outcome {
            NamedTaskOutcome::Completed {
                value: (idx, usage_result),
                ..
            } => {
                if let Some(usage_result) = usage_result {
                    rows[idx].usage_result = Some(usage_result);
                }
                cache::apply_workspace_name(&mut rows[idx].info)?;
            }
            NamedTaskOutcome::Failed { alias, detail } => {
                worker_failures.push((alias, detail));
            }
        }
    }
    if !worker_failures.is_empty() {
        return Err(batch_failure_error(
            "one or more list usage workers failed",
            worker_failures,
        ));
    }

    let global_now = auth::now_unix_secs();
    let global_inputs: Vec<usage::GlobalPaceAccountInput> = rows
        .iter()
        .map(|row| match row.usage_result.as_ref() {
            Some(Ok(usage)) => usage::GlobalPaceAccountInput::from_usage(row.name.clone(), usage),
            _ => usage::GlobalPaceAccountInput::unavailable(row.name.clone()),
        })
        .collect();
    let global_weekly = usage::calculate_global_weekly_summary(&global_inputs, global_now);

    let mut json_items = vec![];

    for row in rows {
        let usage_result = row.usage_result.unwrap_or_else(|| {
            Err(usage::UsageError {
                summary: "unknown".into(),
                detail: "usage result missing".into(),
            })
        });
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u)),
                Err(e) => usage_to_json(Err(&e.detail)),
            };
            json_items.push(output::JsonProfileWithUsage {
                alias: row.name,
                is_current: row.is_current,
                account: account_to_json(
                    &row.info,
                    usage_result
                        .as_ref()
                        .ok()
                        .and_then(|u| u.plan_type.as_deref()),
                ),
                usage: ju,
            });
        } else {
            let mark = if row.is_current {
                color::active("*")
            } else {
                " ".to_string()
            };
            let alias_str = if row.is_current {
                color::bold(&row.name)
            } else {
                row.name.clone()
            };
            print!("{mark} {alias_str}");
            if let Some(email) = &row.info.email {
                print!("  {}", color::dim(email));
            }
            // API plan_type is authoritative over JWT claims (handles plan downgrades)
            let effective_plan = if let Ok(u) = &usage_result {
                u.plan_type.as_deref().or(row.info.plan_type.as_deref())
            } else {
                row.info.plan_type.as_deref()
            };
            if effective_plan.is_some() {
                let label = if let Ok(u) = &usage_result
                    && u.plan_type.is_some()
                {
                    row.info.plan_label_with(u.plan_type.as_deref())
                } else {
                    row.info.plan_label()
                };
                print!("  {}", color::plan(&label, effective_plan));
            }
            println!();
            match usage_result {
                Ok(u) => print_usage_line(&u),
                Err(e) => println!("  {} {}", color::error("!!"), color::error(&e.summary)),
            }
            println!(); // blank line between accounts
        }
    }

    if json {
        print_json(&output::JsonUsageResult {
            profiles: json_items,
            global_weekly: global_weekly_to_json(&global_weekly),
        })?;
    }

    // Opportunistically refresh tokens about to expire (background, bounded)
    report_token_persist_failures(&usage::refresh_expiring_tokens().await);

    Ok(())
}

// ── rename ───────────────────────────────────────────────

pub(crate) fn rename_cmd(old: &str, new: &str, json: bool) -> Result<()> {
    let outcome = profile::rename_profile(old, new)?;
    let durability_warning = outcome
        .durability_warning()
        .map(|warning| format!("{warning:#}"));
    if json {
        if let Some(warning) = durability_warning {
            print_json(&serde_json::json!({
                "ok": true,
                "alias": new,
                "action": "renamed",
                "durability_warning": warning,
            }))?;
        } else {
            print_json(&output::JsonOk {
                ok: true,
                alias: new.to_string(),
                action: "renamed".into(),
            })?;
        }
    } else {
        user_println(&format!("Renamed profile: {old} -> {new}"));
        if let Some(warning) = durability_warning {
            user_println(&color::warn(&format!(
                "Warning: rename committed, but durability could not be confirmed: {warning}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn delete_cmd(alias: &str, yes: bool, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    profile::validate_alias(alias)?;
    if profile::active_profile_from_live()?.as_deref() == Some(alias) {
        anyhow::bail!("cannot delete the active profile '{alias}'");
    }
    if !profile::profile_exists(alias)? {
        anyhow::bail!("profile '{alias}' not found");
    }

    if !yes {
        if json || !std::io::stdin().is_terminal() {
            anyhow::bail!("confirmation required; rerun with --yes to delete profile '{alias}'");
        }
        if !confirm_default_no(&format!(
            "Delete profile '{alias}'? It will remain recoverable. [y/N] "
        )) {
            user_println("Deletion cancelled.");
            return Ok(());
        }
    }
    let outcome = profile::cmd_delete(alias)?;
    let durability_warning = outcome
        .durability_warning()
        .map(|warning| format!("{warning:#}"));
    if json {
        if let Some(warning) = durability_warning {
            print_json(&serde_json::json!({
                "ok": true,
                "alias": alias,
                "action": "deleted",
                "durability_warning": warning,
            }))?;
        } else {
            print_json(&output::JsonOk {
                ok: true,
                alias: alias.to_string(),
                action: "deleted".into(),
            })?;
        }
    } else {
        user_println(&format!("Deleted profile: {alias} (recoverable)"));
        if let Some(warning) = durability_warning {
            user_println(&color::warn(&format!(
                "Warning: deletion committed, but durability could not be confirmed: {warning}"
            )));
        }
    }
    Ok(())
}

// ── best (internal, called by `use` with no alias) ────────

fn score_profile_candidates(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Result<Vec<(usage::Candidate, usage::UsageInfo, f64)>> {
    let last_used = cache::last_used_snapshot_checked()
        .context("loading profile-selection history for automatic ranking")?;
    let items = fetched
        .into_iter()
        .map(|(alias, u)| {
            let path = profile::profile_auth_path(&alias).with_context(|| {
                format!("resolving profile path for automatic ranking: {alias}")
            })?;
            let info = auth::read_account_info_checked(&path).with_context(|| {
                format!("reading profile metadata for automatic ranking: {alias}")
            })?;
            let last_used = last_used.get(&alias).copied().unwrap_or(0);
            Ok((alias, u, info, last_used))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut scored: Vec<(usage::Candidate, usage::UsageInfo, f64)> =
        usage::score_candidates(items, now, safety_7d, team_priority)
            .into_iter()
            .map(|s| (s.candidate, s.usage, s.score))
            .collect();

    scored.sort_by(|a, b| {
        let eligible_a = usage::is_candidate_eligible(&a.0, safety_7d);
        let eligible_b = usage::is_candidate_eligible(&b.0, safety_7d);
        let blocked_a = a.0.explicit_account_blocker.is_some();
        let blocked_b = b.0.explicit_account_blocker.is_some();
        eligible_b
            .cmp(&eligible_a)
            .then(blocked_a.cmp(&blocked_b))
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.last_used.cmp(&b.0.last_used))
            .then(a.0.alias.cmp(&b.0.alias))
    });

    Ok(scored)
}

// ── reset-card-aware revival ──────────────────────────────

/// How an exhausted-pool recovery may consume a reset card to revive an
/// otherwise-ineligible account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardPolicy {
    /// Ask the user interactively before consuming a card.
    Prompt,
    /// Consume without asking (user passed --consume-card, or already confirmed).
    PreApproved,
    /// Never consume; surface a hint instead (JSON / non-TTY without the flag).
    Deny,
}

/// Surfaced to the caller when the pool was exhausted, an account held a
/// reset card, but nothing was consumed (denied or declined).
pub(crate) struct RevivalHint {
    pub(crate) alias: String,
    pub(crate) card_count: u64,
}

pub(crate) struct SelectOutcome {
    pub(crate) alias: String,
    pub(crate) usage: usage::UsageInfo,
    pub(crate) score: f64,
    pub(crate) revival_hint: Option<RevivalHint>,
}

struct PendingRevival {
    target_candidate: usage::Candidate,
    target_credit: usage::ResetCredit,
    safety_7d: f64,
}

enum SelectionPlan {
    Ready(SelectOutcome),
    Revive(PendingRevival),
}

enum RevivalSideEffect {
    None,
    Consumed { alias: String },
}

struct RevivalExecution {
    outcome: SelectOutcome,
    side_effect: RevivalSideEffect,
}

#[derive(Debug, thiserror::Error)]
enum ResetCardActivationError {
    #[error(
        "reset card for '{alias}' was consumed, but activating that profile failed; do not consume another card, retry only the profile switch: {source:#}"
    )]
    Consumed {
        alias: String,
        #[source]
        source: anyhow::Error,
    },
}

#[derive(Debug, thiserror::Error)]
enum ResetCardRevivalError {
    #[error(
        "reset card for '{alias}' was consumed, but the account could not be confirmed eligible ({reason}); live auth was left unchanged and no profile was switched; do not consume another card before verifying quota"
    )]
    ConsumedUnconfirmed { alias: String, reason: &'static str },
    #[error(
        "reset-card outcome for '{alias}' is unknown: {warning}; live auth was left unchanged and no profile was switched; verify the card and quota before any retry"
    )]
    OutcomeUnknown { alias: String, warning: String },
}

impl RevivalSideEffect {
    fn commit_result<T>(self, result: Result<T>) -> Result<T> {
        let source = match result {
            Ok(value) => return Ok(value),
            Err(source) => source,
        };
        match self {
            Self::None => Err(source),
            Self::Consumed { alias } => {
                Err(anyhow::Error::new(ResetCardActivationError::Consumed {
                    alias,
                    source,
                }))
            }
        }
    }
}

pub(crate) fn revival_hint_message(hint: &RevivalHint) -> String {
    format!(
        "{} holds {} reset card(s); rerun with --consume-card to revive",
        hint.alias, hint.card_count
    )
}

/// Interactive confirmation prompt text for reviving an account by consuming
/// its earliest-expiring reset card. Pure formatting, no I/O.
fn revival_prompt_message(alias: &str, card_count: u64, earliest_expiry: &str) -> String {
    format!(
        "'{alias}' holds {card_count} reset card(s) (earliest expiry {earliest_expiry}); consume one to revive it? [y/N] "
    )
}

/// One scored candidate as seen by `pick_revival_target`. Pure data, no I/O.
struct RevivalCandidate<'a> {
    alias: &'a str,
    eligible: bool,
    score: f64,
    reset_credits: &'a [usage::ResetCredit],
}

/// Pick which ineligible, card-holding account should be revived by
/// consuming its earliest-expiring reset card.
///
/// Meaningful only when none of `candidates` are eligible (caller-guaranteed).
/// Ties break by card count (more cards first), then by existing score.
fn pick_revival_target(candidates: &[RevivalCandidate]) -> Option<String> {
    candidates
        .iter()
        .filter(|c| !c.eligible && !c.reset_credits.is_empty())
        .filter_map(|c| {
            let earliest = usage::earliest_reset_credit(c.reset_credits)?;
            Some((c, usage::reset_credit_expiry_sort_key(earliest)))
        })
        .min_by(|(a, a_key), (b, b_key)| {
            a_key
                .cmp(b_key)
                .then_with(|| b.reset_credits.len().cmp(&a.reset_credits.len()))
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        })
        .map(|(c, _)| c.alias.to_string())
}

async fn collect_best_profile_usage_with<
    CacheLookup,
    CacheFuture,
    PathLookup,
    Worker,
    WorkerFuture,
>(
    profiles: Vec<String>,
    json: bool,
    max_concurrent: usize,
    mut cache_lookup: CacheLookup,
    mut path_lookup: PathLookup,
    worker: Worker,
) -> Result<Vec<(String, usage::UsageInfo)>>
where
    CacheLookup: FnMut(String) -> CacheFuture,
    CacheFuture: std::future::Future<Output = Result<Option<usage::UsageInfo>>>,
    PathLookup: FnMut(&str) -> Result<std::path::PathBuf>,
    Worker: Fn(String, std::path::PathBuf) -> WorkerFuture + Send + Sync + 'static,
    WorkerFuture: std::future::Future<Output = Result<Option<usage::UsageInfo>>> + Send + 'static,
{
    let mut fetched = Vec::with_capacity(profiles.len());
    let mut pending = Vec::new();

    // Complete every fallible local preflight before any worker can contact the
    // auth server. A later cache/path failure must not abort an earlier token
    // rotation by dropping its task.
    for alias in profiles {
        let cached = cache_lookup(alias.clone())
            .await
            .with_context(|| format!("reading cached usage during auto-select: {alias}"))?;
        if let Some(cached) = cached {
            fetched.push((alias, cached));
            continue;
        }
        let path = path_lookup(&alias)
            .with_context(|| format!("resolving profile path during auto-select: {alias}"))?;
        pending.push((alias, path));
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let worker = std::sync::Arc::new(worker);
    let mut tasks: tokio::task::JoinSet<Result<Option<usage::UsageInfo>>> =
        tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();
    for (alias, path) in pending {
        let tracked_alias = alias.clone();
        let sem = semaphore.clone();
        let worker = worker.clone();
        let task = tasks.spawn(async move {
            let _permit = sem
                .acquire_owned()
                .await
                .context("automatic-selection usage limiter closed")?;
            worker(alias, path).await
        });
        let previous = task_aliases.insert(task.id(), tracked_alias);
        debug_assert!(previous.is_none());
    }

    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Testing accounts", tasks.len()))
    };
    let mut completed = 0usize;
    let outcomes = drain_named_tasks(&mut tasks, &mut task_aliases, |_| {
        completed += 1;
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
    })
    .await;
    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let mut worker_failures = Vec::new();
    for outcome in outcomes {
        match outcome {
            NamedTaskOutcome::Completed { alias, value } => match value {
                Ok(Some(usage)) => fetched.push((alias, usage)),
                Ok(None) => {}
                Err(error) => worker_failures.push((alias, format!("{error:#}"))),
            },
            NamedTaskOutcome::Failed { alias, detail } => {
                worker_failures.push((alias, detail));
            }
        }
    }
    if !worker_failures.is_empty() {
        return Err(batch_failure_error(
            "one or more automatic-selection usage workers failed",
            worker_failures,
        ));
    }

    Ok(fetched)
}

async fn plan_best_profile(json: bool, card_policy: CardPolicy) -> Result<SelectionPlan> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        anyhow::bail!(
            "no saved profiles; run `codex-switch-global-pace login` or `codex-switch-global-pace import <path>` first"
        );
    }

    let fetched = collect_best_profile_usage_with(
        profiles,
        json,
        config::get().network.max_concurrent,
        |alias| async move { cache::get_async(&alias).await },
        profile::profile_auth_path,
        |alias, path| async move {
            match usage::fetch_usage_retried(&alias, &path).await {
                Ok(usage) => Ok(Some(usage)),
                Err(e) => {
                    tracing::warn!("[{alias}] usage fetch failed during auto-select: {e}");
                    Ok(None)
                }
            }
        },
    )
    .await?;

    if fetched.is_empty() {
        anyhow::bail!("all usage queries failed");
    }

    let safety_7d = config::get().use_cfg.safety_margin_7d;
    let team_priority = config::get().use_cfg.team_priority;
    let now = auth::now_unix_secs();
    let scored = score_profile_candidates(fetched, now, safety_7d, team_priority)?;
    let (top_candidate, top_usage, top_score) = scored
        .first()
        .map(|(c, u, s)| (c.clone(), u.clone(), *s))
        .context("failed to select best profile")?;

    if usage::is_candidate_eligible(&top_candidate, safety_7d) {
        return Ok(SelectionPlan::Ready(SelectOutcome {
            alias: top_candidate.alias,
            usage: top_usage,
            score: top_score,
            revival_hint: None,
        }));
    }

    // Pool exhausted: see if a card-holding account can be revived.
    let revival_candidates: Vec<RevivalCandidate> = scored
        .iter()
        .filter(|(_, usage, _)| usage::explicit_account_blocker(usage).is_none())
        .map(|(c, u, s)| RevivalCandidate {
            alias: &c.alias,
            eligible: usage::is_candidate_eligible(c, safety_7d),
            score: *s,
            reset_credits: &u.reset_credits,
        })
        .collect();
    let revival_target = pick_revival_target(&revival_candidates);

    let Some(target_alias) = revival_target else {
        if let Some(blocker) = usage::explicit_account_blocker(&top_usage) {
            anyhow::bail!(
                "all selectable profiles are blocked by account/workspace restrictions ({blocker}); no reset card was requested and no profile was switched"
            );
        }
        return Ok(SelectionPlan::Ready(SelectOutcome {
            alias: top_candidate.alias,
            usage: top_usage,
            score: top_score,
            revival_hint: None,
        }));
    };

    let target_candidate = scored
        .iter()
        .find(|(c, _, _)| c.alias == target_alias)
        .map(|(c, u, _)| (c.clone(), u.clone()))
        .context("revival target disappeared from scored candidates")?;
    let (target_candidate, target_usage) = target_candidate;
    let card_count = target_usage.reset_credits.len() as u64;
    let target_credit = usage::earliest_reset_credit(&target_usage.reset_credits)
        .cloned()
        .context("revival target has no reset card")?;

    let approved = match card_policy {
        CardPolicy::Deny => false,
        CardPolicy::PreApproved => true,
        CardPolicy::Prompt => {
            let expires = target_credit
                .expires_at
                .as_deref()
                .map(output::format_local_datetime)
                .unwrap_or_else(|| "no expiry".to_string());
            confirm_default_no(&revival_prompt_message(&target_alias, card_count, &expires))
        }
    };

    let top_outcome = |hint: Option<RevivalHint>| SelectOutcome {
        alias: top_candidate.alias.clone(),
        usage: top_usage.clone(),
        score: top_score,
        revival_hint: hint,
    };

    if !approved {
        return Ok(SelectionPlan::Ready(top_outcome(Some(RevivalHint {
            alias: target_alias,
            card_count,
        }))));
    }

    Ok(SelectionPlan::Revive(PendingRevival {
        target_candidate,
        target_credit,
        safety_7d,
    }))
}

fn same_reset_credit(left: &usage::ResetCredit, right: &usage::ResetCredit) -> bool {
    left.id == right.id
        && left.granted_at == right.granted_at
        && left.expires_at == right.expires_at
}

fn candidate_from_revival_usage(
    base: &usage::Candidate,
    usage: &usage::UsageInfo,
    now: i64,
    released_one_pool_member: bool,
) -> usage::Candidate {
    let mut candidate = usage::Candidate::from_usage(
        base.alias.clone(),
        usage,
        base.is_team,
        base.is_free,
        base.last_used,
        now,
    );
    candidate.pool_size = base.pool_size;
    candidate.team_priority = base.team_priority;
    candidate.pool_exhausted = if released_one_pool_member {
        base.pool_exhausted.saturating_sub(1)
    } else {
        base.pool_exhausted
    };
    candidate
}

async fn execute_revival(
    plan: PendingRevival,
    authorized: &profile::AuthorizedProfileSwitch,
) -> Result<RevivalExecution> {
    let PendingRevival {
        target_candidate,
        target_credit,
        safety_7d,
    } = plan;
    let target_alias = target_candidate.alias.clone();
    if authorized.alias() != target_alias {
        anyhow::bail!(
            "reset-card authorization was prepared for '{}' instead of '{target_alias}'; no card was requested",
            authorized.alias()
        );
    }
    let target_path = profile::profile_auth_path(&target_alias)?;

    // Re-read both quota and reset-card state while the authorization-owned
    // profile lease is held. The exact card presented to the user must still
    // belong to this target before the irreversible request is allowed.
    let preflight_usage = usage::fetch_usage_retried_with_existing_lease(
        &target_alias,
        &target_path,
        usage::Refresh::Forced,
        authorized.lease(),
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.detail))
    .context("reset-card preflight failed; no card was requested and no profile was switched")?;
    let preflight_candidate = candidate_from_revival_usage(
        &target_candidate,
        &preflight_usage,
        auth::now_unix_secs(),
        false,
    );
    let preflight_score = usage::score_unified(&preflight_candidate, safety_7d);
    if let Some(blocker) = usage::explicit_account_blocker(&preflight_usage) {
        anyhow::bail!(
            "'{target_alias}' became blocked by an account/workspace restriction ({blocker}); no reset card was requested and no profile was switched"
        );
    }
    if usage::is_candidate_eligible(&preflight_candidate, safety_7d) {
        return Ok(RevivalExecution {
            outcome: SelectOutcome {
                alias: target_alias,
                usage: preflight_usage,
                score: preflight_score,
                revival_hint: None,
            },
            side_effect: RevivalSideEffect::None,
        });
    }
    if let Some(error) = preflight_usage.reset_credits_error.as_deref() {
        anyhow::bail!(
            "reset-card ownership could not be revalidated for '{target_alias}' ({error}); no card was requested and no profile was switched"
        );
    }
    let matching_cards = preflight_usage
        .reset_credits
        .iter()
        .filter(|current| same_reset_credit(current, &target_credit))
        .count();
    if matching_cards != 1 {
        anyhow::bail!(
            "the exact reset card approved for '{target_alias}' changed or disappeared before redemption; no card was requested and no profile was switched"
        );
    }
    profile::revalidate_authorized_profile_switch(authorized).with_context(|| {
        format!(
            "reset-card target '{target_alias}' no longer matches its authorization; no card was requested and no profile was switched"
        )
    })?;

    match usage::consume_reset_credit_by_id_leased(
        &target_alias,
        &target_path,
        target_credit,
        authorized.lease(),
    )
    .await
    {
        Ok(_consumed) => {
            if let Err(err) = cache::invalidate(&target_alias) {
                tracing::warn!("Failed to invalidate usage cache for {target_alias}: {err}");
            }
            let failure_summary = match usage::fetch_usage_retried_with_existing_lease(
                &target_alias,
                &target_path,
                usage::Refresh::Forced,
                authorized.lease(),
            )
            .await
            {
                Ok(revived_usage) => {
                    let revived_candidate = candidate_from_revival_usage(
                        &target_candidate,
                        &revived_usage,
                        auth::now_unix_secs(),
                        true,
                    );
                    let score = usage::score_unified(&revived_candidate, safety_7d);
                    if usage::is_candidate_eligible(&revived_candidate, safety_7d) {
                        return Ok(RevivalExecution {
                            outcome: SelectOutcome {
                                alias: target_alias.clone(),
                                usage: revived_usage,
                                score,
                                revival_hint: None,
                            },
                            side_effect: RevivalSideEffect::Consumed {
                                alias: target_alias,
                            },
                        });
                    }
                    tracing::warn!(
                        "[{target_alias}] still exhausted after consuming a reset card; not consuming a second card"
                    );
                    "quota remained exhausted after refresh"
                }
                Err(e) => {
                    tracing::warn!(
                        "[{target_alias}] failed to refresh usage after consuming reset card: {e}"
                    );
                    "usage refresh failed"
                }
            };
            Err(anyhow::Error::new(
                ResetCardRevivalError::ConsumedUnconfirmed {
                    alias: target_alias,
                    reason: failure_summary,
                },
            ))
        }
        Err(e) => {
            tracing::warn!("[{target_alias}] failed to consume reset card: {e}");
            if e.outcome_unknown_after_request() {
                if let Err(err) = cache::invalidate(&target_alias) {
                    tracing::warn!("Failed to invalidate usage cache for {target_alias}: {err}");
                }
                let message = e.user_facing_unknown_message(&target_alias);
                Err(anyhow::Error::new(ResetCardRevivalError::OutcomeUnknown {
                    alias: target_alias,
                    warning: message,
                }))
            } else {
                debug_assert!(e.definitely_not_consumed());
                Err(anyhow::Error::new(e).context(format!(
                    "reset card for '{target_alias}' was not consumed; no profile was switched"
                )))
            }
        }
    }
}

async fn best_cmd(json: bool, consume_card: bool) -> Result<()> {
    use std::io::IsTerminal;

    let card_policy = if consume_card {
        CardPolicy::PreApproved
    } else if !json && std::io::stdin().is_terminal() {
        CardPolicy::Prompt
    } else {
        CardPolicy::Deny
    };

    let allow_prompt = !json && std::io::stdin().is_terminal();
    let (outcome, switch_outcome) = match plan_best_profile(json, card_policy).await? {
        SelectionPlan::Ready(outcome) => {
            let switch_outcome = profile::switch_profile_with_prompt(&outcome.alias, allow_prompt)?;
            (outcome, switch_outcome)
        }
        SelectionPlan::Revive(plan) => {
            // The overwrite authorization must precede card redemption. In
            // particular, JSON/non-TTY mode cannot spend a card and only then
            // discover that untracked live auth requires a prompt.
            let target_alias = plan.target_candidate.alias.clone();
            let lease = profile::acquire_profile_lease_async(target_alias).await?;
            let authorized =
                profile::authorize_profile_switch_before_side_effect(lease, allow_prompt)?;
            let RevivalExecution {
                outcome,
                side_effect,
            } = execute_revival(plan, &authorized).await?;
            let switch_outcome =
                side_effect.commit_result(profile::commit_authorized_profile_switch(authorized))?;
            (outcome, switch_outcome)
        }
    };
    let SelectOutcome {
        alias: best_alias,
        usage: best_usage,
        score: best_score,
        revival_hint,
    } = outcome;

    if let Some(error) = switch_outcome.selection_history_warning() {
        eprintln!(
            "{}",
            color::warn(&format!(
                "Warning: switched to '{best_alias}', but its selection history could not be recorded: {error:#}"
            ))
        );
    }

    let path = profile::profile_auth_path(&best_alias)?;
    let info = auth::read_account_info_checked(&path)
        .with_context(|| format!("reading selected profile metadata: {best_alias}"))?;

    if json {
        print_json(&output::JsonBest {
            switched_to: best_alias.clone(),
            account: account_to_json(&info, best_usage.plan_type.as_deref()),
            usage: usage_to_json(Ok(&best_usage)),
            score: best_score,
            mode: "unified".to_string(),
            hint: revival_hint.as_ref().map(revival_hint_message),
        })?;
    } else {
        println!("{}", color::success(&format!("Switched to: {best_alias}")));
        print_usage_line(&best_usage);
        if let Some(hint) = &revival_hint {
            println!("  {}", color::dim(&revival_hint_message(hint)));
        }
    }

    // Opportunistically refresh tokens about to expire (background, bounded)
    report_token_persist_failures(&usage::refresh_expiring_tokens().await);

    Ok(())
}

// ── tests: pick_revival_target ────────────────────────────

#[cfg(test)]
mod revival_target_tests {
    use super::*;

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(name);
            // SAFETY: tests that mutate application paths hold the crate-wide
            // profile environment lock for the guard's full lifetime.
            unsafe { std::env::set_var(name, value) };
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: the owning test still holds the crate-wide profile
            // environment lock while restoring the process environment.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }

    fn lock_profile_test_environment() -> std::sync::MutexGuard<'static, ()> {
        crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn automatic_ranking_rejects_malformed_selection_history() {
        let _lock = lock_profile_test_environment();
        let home = tempfile::tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_dir = home.path().join("profiles/alice");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("auth.json"), "{}").unwrap();
        std::fs::write(home.path().join("cache.json"), "not-json").unwrap();

        let error = score_profile_candidates(
            vec![("alice".to_string(), usage::UsageInfo::default())],
            0,
            0.0,
            false,
        )
        .expect_err("corrupt ranking history must stop automatic selection");

        assert!(format!("{error:#}").contains("parsing cache file"));
    }

    #[test]
    fn automatic_ranking_rejects_unreadable_profile_metadata() {
        let _lock = lock_profile_test_environment();
        let home = tempfile::tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_dir = home.path().join("profiles/alice");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("auth.json"), "not-json").unwrap();
        std::fs::write(home.path().join("cache.json"), r#"{"entries": {}}"#).unwrap();

        let error = score_profile_candidates(
            vec![("alice".to_string(), usage::UsageInfo::default())],
            0,
            0.0,
            false,
        )
        .expect_err("invalid profile auth must not become empty ranking metadata");

        let error = format!("{error:#}");
        assert!(error.contains("reading profile metadata for automatic ranking"));
        assert!(error.contains("parsing"));
    }

    fn credit(id: &str, expires_at: Option<&str>) -> usage::ResetCredit {
        usage::ResetCredit {
            id: id.to_string(),
            granted_at: None,
            expires_at: expires_at.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_pick_revival_target_returns_none_when_nobody_holds_card() {
        let no_cards: Vec<usage::ResetCredit> = vec![];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 10.0,
                reset_credits: &no_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 20.0,
                reset_credits: &no_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates), None);
    }

    #[test]
    fn test_pick_revival_target_returns_earliest_expiring_card_holder() {
        let a_cards = vec![credit("a1", Some("2026-07-10T00:00:00Z"))];
        let b_cards = vec![credit("b1", Some("2026-07-05T00:00:00Z"))];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 10.0,
                reset_credits: &a_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 20.0,
                reset_credits: &b_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates).as_deref(), Some("b"));
    }

    #[test]
    fn test_pick_revival_target_treats_missing_expiry_as_latest() {
        let a_cards = vec![credit("a1", None)]; // never expires -> sorts as latest
        let b_cards = vec![credit("b1", Some("2026-07-05T00:00:00Z"))];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 10.0,
                reset_credits: &a_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 20.0,
                reset_credits: &b_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates).as_deref(), Some("b"));
    }

    #[test]
    fn test_pick_revival_target_tie_breaks_by_card_count_then_score() {
        // Same earliest expiry: a has 1 card, b has 2 cards -> b wins (more cards).
        let a_cards = vec![credit("a1", Some("2026-07-05T00:00:00Z"))];
        let b_cards = vec![
            credit("b1", Some("2026-07-05T00:00:00Z")),
            credit("b2", Some("2026-07-20T00:00:00Z")),
        ];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 50.0,
                reset_credits: &a_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 10.0,
                reset_credits: &b_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates).as_deref(), Some("b"));

        // Same earliest expiry, same card count -> higher score wins.
        let c_cards = vec![credit("c1", Some("2026-07-05T00:00:00Z"))];
        let d_cards = vec![credit("d1", Some("2026-07-05T00:00:00Z"))];
        let candidates2 = vec![
            RevivalCandidate {
                alias: "c",
                eligible: false,
                score: 5.0,
                reset_credits: &c_cards,
            },
            RevivalCandidate {
                alias: "d",
                eligible: false,
                score: 15.0,
                reset_credits: &d_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates2).as_deref(), Some("d"));
    }

    #[test]
    fn test_revival_prompt_message_includes_alias_count_and_expiry() {
        let msg = revival_prompt_message("acct-a", 2, "07-08 00:00");
        assert!(msg.contains("acct-a"));
        assert!(msg.contains('2'));
        assert!(msg.contains("07-08 00:00"));
        assert!(msg.contains("[y/N]"));
    }

    #[test]
    fn test_revival_hint_message_includes_alias_and_flag() {
        let hint = RevivalHint {
            alias: "acct-b".to_string(),
            card_count: 3,
        };
        let msg = revival_hint_message(&hint);
        assert!(msg.contains("acct-b"));
        assert!(msg.contains('3'));
        assert!(msg.contains("--consume-card"));
    }

    #[test]
    fn test_pick_revival_target_ignores_eligible_candidates() {
        let cards = vec![credit("x1", Some("2026-07-05T00:00:00Z"))];
        let candidates = vec![RevivalCandidate {
            alias: "eligible_holder",
            eligible: true,
            score: 999.0,
            reset_credits: &cards,
        }];
        assert_eq!(pick_revival_target(&candidates), None);
    }

    #[test]
    fn reset_card_binding_includes_grant_and_expiry_metadata() {
        let approved = usage::ResetCredit {
            id: "credit-1".to_string(),
            granted_at: Some("2026-07-01T00:00:00Z".to_string()),
            expires_at: Some("2026-07-08T00:00:00Z".to_string()),
        };
        assert!(same_reset_credit(&approved, &approved));

        let mut rebound = approved.clone();
        rebound.expires_at = Some("2026-07-09T00:00:00Z".to_string());
        assert!(!same_reset_credit(&approved, &rebound));
    }

    #[test]
    fn revival_recheck_uses_response_time_across_a_reset_boundary() {
        let usage = usage::UsageInfo {
            primary: Some(usage::WindowUsage {
                used_percent: Some(100.0),
                resets_at: Some(1_001),
                window_minutes: Some(300),
            }),
            secondary: Some(usage::WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(2_000),
                window_minutes: Some(10_080),
            }),
            account_limited: true,
            ..usage::UsageInfo::default()
        };
        let base =
            usage::Candidate::from_usage("boundary".to_string(), &usage, false, false, 0, 1_000);
        assert!(!usage::is_candidate_eligible(&base, 20.0));

        let rechecked = candidate_from_revival_usage(&base, &usage, 1_002, true);

        assert_eq!(rechecked.now, 1_002);
        assert!(
            usage::is_candidate_eligible(&rechecked, 20.0),
            "a reset crossed while the request was in flight must be evaluated at response time"
        );
    }

    #[test]
    fn post_card_activation_errors_preserve_side_effect_status() {
        let consumed = RevivalSideEffect::Consumed {
            alias: "acct-a".to_string(),
        }
        .commit_result::<()>(Err(anyhow::anyhow!("live auth changed")))
        .unwrap_err();
        let consumed = format!("{consumed:#}");
        assert!(consumed.contains("was consumed"), "{consumed}");
        assert!(consumed.contains("do not consume another"), "{consumed}");
        assert!(
            consumed.contains("retry only the profile switch"),
            "{consumed}"
        );

        let unknown = anyhow::Error::new(ResetCardRevivalError::OutcomeUnknown {
            alias: "acct-b".to_string(),
            warning: "consumption may have occurred; verify before retry".to_string(),
        });
        let unknown = format!("{unknown:#}");
        assert!(
            unknown.contains("consumption may have occurred"),
            "{unknown}"
        );
        assert!(
            unknown.contains("live auth was left unchanged"),
            "{unknown}"
        );
        assert!(unknown.contains("no profile was switched"), "{unknown}");
    }

    #[tokio::test]
    async fn later_cache_preflight_failure_starts_no_usage_worker() {
        let worker_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let starts_in_worker = worker_starts.clone();

        let error = collect_best_profile_usage_with(
            vec!["alice".to_string(), "bob".to_string()],
            true,
            2,
            |alias| async move {
                if alias == "bob" {
                    anyhow::bail!("injected later cache failure");
                }
                Ok(None)
            },
            |alias| Ok(std::path::PathBuf::from(alias)),
            move |_alias, _path| {
                let starts_in_worker = starts_in_worker.clone();
                async move {
                    starts_in_worker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(None)
                }
            },
        )
        .await
        .expect_err("a later cache preflight failure must fail the batch");

        assert!(format!("{error:#}").contains("injected later cache failure"));
        assert_eq!(worker_starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn later_path_preflight_failure_starts_no_usage_worker() {
        let worker_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let starts_in_worker = worker_starts.clone();

        let error = collect_best_profile_usage_with(
            vec!["alice".to_string(), "bob".to_string()],
            true,
            2,
            |_alias| async { Ok(None) },
            |alias| {
                if alias == "bob" {
                    anyhow::bail!("injected later path failure");
                }
                Ok(std::path::PathBuf::from(alias))
            },
            move |_alias, _path| {
                let starts_in_worker = starts_in_worker.clone();
                async move {
                    starts_in_worker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(None)
                }
            },
        )
        .await
        .expect_err("a later path preflight failure must fail the batch");

        assert!(format!("{error:#}").contains("injected later path failure"));
        assert_eq!(worker_starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn usage_worker_failures_are_reported_only_after_other_workers_persist() {
        let temp = tempfile::tempdir().unwrap();
        let persisted_path = temp.path().join("rotated-auth.json");
        let path_in_worker = persisted_path.clone();
        let workers_started = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let barrier_in_worker = workers_started.clone();

        let error = collect_best_profile_usage_with(
            vec![
                "error".to_string(),
                "panic".to_string(),
                "persist".to_string(),
            ],
            true,
            3,
            |_alias| async { Ok(None) },
            |alias| Ok(std::path::PathBuf::from(alias)),
            move |alias, _path| {
                let barrier_in_worker = barrier_in_worker.clone();
                let path_in_worker = path_in_worker.clone();
                async move {
                    barrier_in_worker.wait().await;
                    match alias.as_str() {
                        "error" => anyhow::bail!("injected worker error"),
                        "panic" => panic!("secret panic payload must stay private"),
                        "persist" => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            std::fs::write(&path_in_worker, b"rotated credential persisted")
                                .with_context(|| {
                                    format!(
                                        "writing persistence marker {}",
                                        path_in_worker.display()
                                    )
                                })?;
                            Ok(None)
                        }
                        other => anyhow::bail!("unexpected test worker: {other}"),
                    }
                }
            },
        )
        .await
        .expect_err("worker error and panic must be returned after the whole batch drains");

        assert_eq!(
            std::fs::read(&persisted_path).unwrap(),
            b"rotated credential persisted"
        );
        let error = format!("{error:#}");
        assert!(error.contains("[error] injected worker error"), "{error}");
        assert!(error.contains("[panic] worker panicked"), "{error}");
        assert!(!error.contains("secret panic payload"), "{error}");
    }
}
