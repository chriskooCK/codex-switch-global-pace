use super::render::{confirm_default_no, print_usage_line};
use crate::output::{
    self, ProgressReporter, account_to_json, global_weekly_to_json, print_json, usage_to_json,
    user_println,
};
use crate::task_batch::{NamedTaskOutcome, batch_failure_error, drain_named_tasks};
use crate::{auth, cache, color, config, jwt, profile, usage, workspace};
use anyhow::{Context, Result};

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

fn cached_workspace_state(
    snapshot: &cache::CacheSnapshot,
    binding: Option<&jwt::StrictAccountBinding>,
) -> Option<cache::WorkspaceState> {
    binding
        .and_then(|binding| snapshot.workspaces.get(&binding.account_id))
        .cloned()
}

fn list_workspace_needs_refresh(
    binding: Option<&jwt::StrictAccountBinding>,
    state: &cache::WorkspaceState,
    force: bool,
) -> bool {
    binding.is_some() && (force || matches!(state, cache::WorkspaceState::Unresolved))
}

async fn lookup_and_publish_list_workspace_state(
    network_permit: tokio::sync::OwnedSemaphorePermit,
    auth: &serde_json::Value,
    account_id: &str,
    client: &reqwest::Client,
) -> Result<cache::WorkspaceState> {
    let lookup = workspace::lookup_state_for_auth_with_client(auth, client).await;
    drop(network_permit);
    let state = lookup?;
    workspace::publish_workspace_state(account_id, &state).await?;
    Ok(state)
}

pub(crate) async fn list_cmd(force: bool, json: bool) -> Result<()> {
    let (profile_accounts, current) = profile::load_profile_accounts_checked_with_active()?;
    if profile_accounts.is_empty() {
        if json {
            let summary =
                usage::calculate_global_weekly_summary(&[], crate::auth::now_unix_secs()?);
            print_json(&output::JsonUsageResult {
                profiles: vec![],
                global_weekly: global_weekly_to_json(&summary),
            })?;
        } else {
            println!("{}", color::dim("(no saved profiles)"));
        }
        return Ok(());
    }

    // Derive the active row from the same immutable registry generation used
    // for the rows. A stale marker must not make an unrelated profile look
    // active while live auth is untracked.
    let current = current.unwrap_or_default();

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    struct ListRow {
        name: String,
        path: std::path::PathBuf,
        is_current: bool,
        info: jwt::AccountInfo,
        binding: Option<jwt::StrictAccountBinding>,
        usage_result: Option<std::result::Result<usage::UsageInfo, usage::UsageError>>,
        workspace_state: cache::WorkspaceState,
    }

    let mut rows: Vec<ListRow> = Vec::with_capacity(profile_accounts.len());
    for account in profile_accounts {
        let profile::ProfileAccountSnapshot {
            alias: name,
            path,
            info,
        } = account;
        let binding = info.strict_binding();
        rows.push(ListRow {
            is_current: name == current,
            name,
            path,
            info,
            binding,
            usage_result: None,
            workspace_state: cache::WorkspaceState::Unresolved,
        });
    }
    let cache_bindings = if force {
        std::collections::HashMap::new()
    } else {
        rows.iter()
            .filter_map(|row| {
                row.binding
                    .clone()
                    .map(|binding| (row.name.clone(), binding))
            })
            .collect()
    };
    let mut cache_account_ids = rows
        .iter()
        .filter_map(|row| {
            row.binding
                .as_ref()
                .map(|binding| binding.account_id.clone())
        })
        .collect::<Vec<_>>();
    cache_account_ids.sort();
    cache_account_ids.dedup();
    let mut cache_snapshot = cache::get_snapshot_bound(&cache_bindings, &cache_account_ids)?;
    for row in &mut rows {
        row.usage_result = match &row.binding {
            Some(_) => cache_snapshot.usage.remove(&row.name).map(Ok),
            None => Some(Err(usage::UsageError {
                summary: "account identity incomplete".to_string(),
                detail: format!(
                    "[{}] usage refresh requires a verified account id and email",
                    row.name
                ),
            })),
        };
        if let Some(state) = cached_workspace_state(&cache_snapshot, row.binding.as_ref()) {
            cache::apply_workspace_state(&mut row.info, &state);
            row.workspace_state = state;
        }
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
    let needs_network = rows.iter().any(|row| {
        row.usage_result.is_none()
            || list_workspace_needs_refresh(row.binding.as_ref(), &row.workspace_state, force)
    });
    let http_client = needs_network
        .then(auth::build_http_client)
        .transpose()
        .context("building the shared account-list HTTP client")?;
    for (idx, row) in rows.iter().enumerate() {
        let needs_usage = row.usage_result.is_none();
        let needs_workspace =
            list_workspace_needs_refresh(row.binding.as_ref(), &row.workspace_state, force);
        if !needs_usage && !needs_workspace {
            continue;
        }

        let alias = row.name.clone();
        let path = row.path.clone();
        let Some(expected_binding) = row.binding.clone() else {
            continue;
        };
        let sem = semaphore.clone();
        let client = http_client
            .as_ref()
            .expect("network work requires the shared HTTP client")
            .clone();
        let task = tasks.spawn(async move {
            let lease = match profile::acquire_profile_lease_async(alias.clone()).await {
                Ok(lease) => lease,
                Err(error) => {
                    return (
                        idx,
                        needs_usage.then(|| {
                            Err(usage::UsageError {
                                summary: "profile lock failed".to_string(),
                                detail: format!(
                                    "[{alias}] could not lock profile for list: {error:#}"
                                ),
                            })
                        }),
                        None,
                    );
                }
            };
            let usage_result = if needs_usage {
                let prepared = usage::prepare_full_usage_with_existing_lease(
                    &alias,
                    &path,
                    if force {
                        usage::Refresh::Forced
                    } else {
                        // The batch snapshot above already established that
                        // this identity has no fresh usage entry.
                        usage::Refresh::Unattended
                    },
                    &lease,
                    Some(&expected_binding),
                )
                .await;
                Some(match prepared {
                    Ok(prepared) => {
                        let mut network = usage::NetworkPermitBudget::new(
                            usage::first_network_permit(sem.clone()),
                        );
                        usage::execute_prepared_full_usage_with_existing_lease_and_client(
                            prepared,
                            &lease,
                            &client,
                            &mut network,
                        )
                        .await
                        .map(|observation| observation.usage)
                    }
                    Err(error) => Err(error),
                })
            } else {
                None
            };
            // Snapshot refreshed auth while the identity lease is still held,
            // then release the credential boundary before the independent
            // workspace endpoint can add latency.
            let workspace_auth = if needs_workspace {
                Some(auth::read_auth_async(&path).await)
            } else {
                None
            };
            drop(lease);
            let workspace_result = match workspace_auth {
                Some(Ok(auth)) => {
                    let actual_binding = auth::account_info_from_auth_value(&auth).strict_binding();
                    if actual_binding != Some(expected_binding.clone()) {
                        tracing::debug!(
                            "[{alias}] workspace metadata skipped because profile identity changed"
                        );
                        None
                    } else {
                        match sem.acquire_owned().await {
                            Ok(workspace_permit) => {
                                match lookup_and_publish_list_workspace_state(
                                    workspace_permit,
                                    &auth,
                                    &expected_binding.account_id,
                                    &client,
                                )
                                .await
                                {
                                    Ok(state) => {
                                        Some((expected_binding.account_id.clone(), state))
                                    }
                                    Err(err) => {
                                        tracing::debug!(
                                            "[{alias}] workspace metadata unavailable: {err}"
                                        );
                                        None
                                    }
                                }
                            }
                            Err(_) => {
                                tracing::debug!(
                                    "[{alias}] workspace metadata skipped because the network limiter closed"
                                );
                                None
                            }
                        }
                    }
                }
                Some(Err(err)) => {
                    tracing::debug!("[{alias}] auth unavailable for workspace metadata: {err}");
                    None
                }
                None => None,
            };
            (idx, usage_result, workspace_result)
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
                value: (idx, usage_result, workspace_result),
                ..
            } => {
                if let Some(usage_result) = usage_result {
                    rows[idx].usage_result = Some(usage_result);
                }
                if let Some((account_id, state)) = workspace_result
                    && rows[idx].info.account_id.as_deref() == Some(account_id.as_str())
                {
                    cache::apply_workspace_state(&mut rows[idx].info, &state);
                    rows[idx].workspace_state = state;
                }
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

    let global_now = auth::now_unix_secs()?;
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
        let usage_result = row
            .usage_result
            .with_context(|| format!("usage worker returned no result for '{}'", row.name))?;
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u), global_now)?,
                Err(e) => usage_to_json(Err(&e.detail), global_now)?,
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
                Ok(u) => print_usage_line(&u, global_now),
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

fn score_profile_candidates_with_info(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
    mut info_lookup: impl FnMut(&str) -> Result<jwt::AccountInfo>,
) -> Result<Vec<(usage::Candidate, usage::UsageInfo, f64)>> {
    let last_used = cache::last_used_snapshot_checked()
        .context("loading profile-selection history for automatic ranking")?;
    let items = fetched
        .into_iter()
        .map(|(alias, u)| {
            let info = info_lookup(&alias)?;
            let last_used = last_used.get(&alias).copied().unwrap_or(0);
            Ok((alias, u, info, last_used))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut scored: Vec<(usage::Candidate, usage::UsageInfo, f64)> =
        usage::score_candidates(items, now, safety_7d, team_priority)
            .into_iter()
            .map(|s| (s.candidate, s.usage, s.score))
            .collect();

    // An incomplete snapshot cannot participate in automatic selection. Keep
    // explicit account blockers so callers can still surface their concrete
    // server verdict when no selectable profile remains.
    scored.retain(|(candidate, _, _)| {
        candidate.has_required_quota_data() || candidate.explicit_account_blocker.is_some()
    });
    if scored.is_empty() {
        anyhow::bail!("no profile has complete authoritative quota data for automatic selection");
    }

    scored.sort_by(|a, b| {
        let eligible_a = usage::is_candidate_eligible(&a.0, safety_7d);
        let eligible_b = usage::is_candidate_eligible(&b.0, safety_7d);
        let blocked_a = a.0.explicit_account_blocker.is_some();
        let blocked_b = b.0.explicit_account_blocker.is_some();
        eligible_b
            .cmp(&eligible_a)
            .then(blocked_a.cmp(&blocked_b))
            .then_with(|| b.2.total_cmp(&a.2))
            .then(a.0.last_used.cmp(&b.0.last_used))
            .then(a.0.alias.cmp(&b.0.alias))
    });

    Ok(scored)
}

#[cfg(test)]
fn score_profile_candidates(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Result<Vec<(usage::Candidate, usage::UsageInfo, f64)>> {
    score_profile_candidates_with_info(fetched, now, safety_7d, team_priority, |alias| {
        let path = profile::profile_auth_path(alias)
            .with_context(|| format!("resolving profile path for automatic ranking: {alias}"))?;
        auth::read_account_info_checked(&path)
            .with_context(|| format!("reading profile metadata for automatic ranking: {alias}"))
    })
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
    pub(crate) evaluated_at: i64,
    pub(crate) revival_hint: Option<RevivalHint>,
}

struct PendingRevival {
    target_candidate: usage::Candidate,
    target_credit: usage::ResetCredit,
    target_binding: jwt::StrictAccountBinding,
    safety_7d: f64,
    client: Option<reqwest::Client>,
}

struct ReadySelection {
    outcome: SelectOutcome,
    target_binding: jwt::StrictAccountBinding,
}

enum SelectionPlan {
    Ready(ReadySelection),
    Revive(PendingRevival),
}

enum RevivalSideEffect {
    None,
    Consumed { alias: String },
}

struct RevivalExecution {
    outcome: SelectOutcome,
    side_effect: RevivalSideEffect,
    info: jwt::AccountInfo,
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
                .then_with(|| b.score.total_cmp(&a.score))
        })
        .map(|(c, _)| c.alias.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoSelectUsageOrigin {
    /// A normal cache generation whose quota and reset-card fields are both
    /// authoritative.
    CachedComplete,
    /// A core-only generation published by a previous automatic selection.
    /// It avoids another quota probe but still requires a reset-card lookup if
    /// the pool is exhausted.
    CachedQuotaOnly,
    CoreProbe,
}

#[derive(Debug, Clone)]
struct AutoSelectUsage {
    alias: String,
    usage: usage::UsageInfo,
    origin: AutoSelectUsageOrigin,
    /// Exact cache generation required only while publishing a core probe or
    /// completing reset metadata for a quota-only generation.
    cache_baseline: Option<cache::UsageCacheBaseline>,
}

#[derive(Debug, Clone, Copy)]
struct UsageCollectionOptions {
    json: bool,
    max_concurrent: usize,
}

fn auto_select_scoring_input(collected: &[AutoSelectUsage]) -> Vec<(String, usage::UsageInfo)> {
    collected
        .iter()
        .map(|item| (item.alias.clone(), item.usage.clone()))
        .collect()
}

fn reset_detail_aliases_if_pool_exhausted(
    scored: &[(usage::Candidate, usage::UsageInfo, f64)],
    safety_7d: f64,
) -> Vec<String> {
    if scored
        .iter()
        .any(|(candidate, _, _)| usage::is_candidate_eligible(candidate, safety_7d))
    {
        return Vec::new();
    }

    scored
        .iter()
        .filter(|(candidate, _, _)| candidate.explicit_account_blocker.is_none())
        .map(|(candidate, _, _)| candidate.alias.clone())
        .collect()
}

fn needs_reset_card_enrichment(candidate: &AutoSelectUsage) -> bool {
    candidate.origin != AutoSelectUsageOrigin::CachedComplete
}

async fn collect_best_profile_usage_with<
    CacheLookup,
    CacheFuture,
    PathLookup,
    AcquireLease,
    AcquireLeaseFuture,
    Lease,
    Prepare,
    PrepareFuture,
    Prepared,
    Worker,
    WorkerFuture,
>(
    profiles: Vec<String>,
    options: UsageCollectionOptions,
    mut cache_lookup: CacheLookup,
    mut path_lookup: PathLookup,
    acquire_lease: AcquireLease,
    prepare: Prepare,
    worker: Worker,
) -> Result<Vec<AutoSelectUsage>>
where
    CacheLookup: FnMut(String) -> CacheFuture,
    CacheFuture: std::future::Future<Output = Result<cache::AutoSelectUsageCacheLookup>>,
    PathLookup: FnMut(&str) -> Result<std::path::PathBuf>,
    AcquireLease: Fn(String) -> AcquireLeaseFuture + Send + Sync + 'static,
    AcquireLeaseFuture: std::future::Future<Output = Result<Option<Lease>>> + Send + 'static,
    Lease: Send + 'static,
    Prepare: Fn(String, std::path::PathBuf, Lease) -> PrepareFuture + Send + Sync + 'static,
    PrepareFuture: std::future::Future<Output = Result<Option<(Prepared, Lease)>>> + Send + 'static,
    Prepared: Send + 'static,
    Worker: Fn(String, Prepared, Lease, usage::NetworkPermitBudget) -> WorkerFuture
        + Send
        + Sync
        + 'static,
    WorkerFuture: std::future::Future<Output = Result<Option<usage::UsageInfo>>> + Send + 'static,
{
    let mut fetched = Vec::with_capacity(profiles.len());
    let mut pending = Vec::new();

    // Complete every fallible local preflight before any worker can contact the
    // auth server. A later cache/path failure must not abort an earlier token
    // rotation by dropping its task.
    for alias in profiles {
        let lookup = cache_lookup(alias.clone())
            .await
            .with_context(|| format!("reading cached usage during auto-select: {alias}"))?;
        let reset_metadata_complete = lookup.reset_metadata_complete();
        let (cached, baseline) = lookup.into_parts();
        if let Some(cached) = cached {
            fetched.push(AutoSelectUsage {
                alias,
                usage: cached,
                origin: if reset_metadata_complete {
                    AutoSelectUsageOrigin::CachedComplete
                } else {
                    AutoSelectUsageOrigin::CachedQuotaOnly
                },
                cache_baseline: (!reset_metadata_complete).then_some(baseline),
            });
            continue;
        }
        let path = path_lookup(&alias)
            .with_context(|| format!("resolving profile path during auto-select: {alias}"))?;
        pending.push((alias, path, baseline));
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(options.max_concurrent));
    let acquire_lease = std::sync::Arc::new(acquire_lease);
    let prepare = std::sync::Arc::new(prepare);
    let worker = std::sync::Arc::new(worker);
    let mut tasks: tokio::task::JoinSet<
        Result<Option<(usage::UsageInfo, cache::UsageCacheBaseline)>>,
    > = tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();
    for (alias, path, baseline) in pending {
        let tracked_alias = alias.clone();
        let sem = semaphore.clone();
        let acquire_lease = acquire_lease.clone();
        let prepare = prepare.clone();
        let worker = worker.clone();
        let task = tasks.spawn(async move {
            // A locked profile must wait outside the scarce network budget;
            // otherwise enough lock waiters can prevent every ready alias
            // from reaching the server.
            let Some(lease) = acquire_lease(alias.clone()).await? else {
                return Ok(None);
            };
            // Auth reads, strict identity checks, endpoint resolution, and
            // standing credential-verdict reads are local preparation. Keep
            // all of them outside the scarce network budget as well.
            let Some((prepared, lease)) = prepare(alias.clone(), path, lease).await? else {
                return Ok(None);
            };
            let network = usage::NetworkPermitBudget::new(usage::first_network_permit(sem));
            worker(alias, prepared, lease, network)
                .await
                .map(|usage| usage.map(|usage| (usage, baseline)))
        });
        let previous = task_aliases.insert(task.id(), tracked_alias);
        debug_assert!(previous.is_none());
    }

    let mut progress = if options.json {
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
                Ok(Some((usage, baseline))) => fetched.push(AutoSelectUsage {
                    alias,
                    usage,
                    origin: AutoSelectUsageOrigin::CoreProbe,
                    cache_baseline: Some(baseline),
                }),
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

/// Publish every cache-neutral core probe as one quota-only CAS batch before
/// scoring. The returned generation is authoritative: an intervening writer
/// for the same account wins and replaces the candidate's probe result, so the
/// selection is scored against the cache state that actually survived.
async fn publish_auto_select_core_probes(
    collected: &mut [AutoSelectUsage],
    bindings: &std::collections::HashMap<String, jwt::StrictAccountBinding>,
) -> Result<()> {
    let mut updates = Vec::new();
    for candidate in collected.iter() {
        if candidate.origin != AutoSelectUsageOrigin::CoreProbe {
            continue;
        }
        let baseline = candidate.cache_baseline.clone().with_context(|| {
            format!(
                "core probe for '{}' has no exact cache baseline",
                candidate.alias
            )
        })?;
        let binding = bindings.get(&candidate.alias).cloned().with_context(|| {
            format!(
                "profile '{}' has no auto-select identity snapshot",
                candidate.alias
            )
        })?;
        updates.push(cache::CoreProbeCacheUpdate {
            alias: candidate.alias.clone(),
            binding,
            baseline,
            usage: candidate.usage.clone(),
            reset_metadata: cache::CoreProbeResetMetadata::PreserveExisting,
        });
    }
    if updates.is_empty() {
        return Ok(());
    }

    let outcomes = cache::complete_core_probes_bound_async(updates)
        .await
        .context(
            "conditionally publishing automatic-selection quota probes; no reset card was requested and no profile was switched",
        )?;
    let mut by_alias = std::collections::HashMap::with_capacity(outcomes.len());
    for outcome in outcomes {
        let alias = outcome.alias.clone();
        if by_alias.insert(alias.clone(), outcome).is_some() {
            anyhow::bail!(
                "duplicate core-probe cache result for profile '{alias}'; no reset card was requested and no profile was switched"
            );
        }
    }
    for candidate in collected.iter_mut() {
        if candidate.origin != AutoSelectUsageOrigin::CoreProbe {
            continue;
        }
        let outcome = by_alias.remove(&candidate.alias).with_context(|| {
            format!(
                "core-probe cache results have no profile '{}'; no reset card was requested and no profile was switched",
                candidate.alias
            )
        })?;
        candidate.usage = outcome.usage;
        if outcome.reset_metadata_complete {
            candidate.origin = AutoSelectUsageOrigin::CachedComplete;
            candidate.cache_baseline = None;
        } else {
            candidate.origin = AutoSelectUsageOrigin::CachedQuotaOnly;
            candidate.cache_baseline = Some(outcome.baseline);
        }
    }
    if !by_alias.is_empty() {
        anyhow::bail!(
            "core-probe cache results did not match the automatic-selection snapshot; no reset card was requested and no profile was switched"
        );
    }
    Ok(())
}

async fn collect_reset_card_details_with<
    AcquireLease,
    AcquireLeaseFuture,
    Lease,
    Prepare,
    PrepareFuture,
    Prepared,
    Worker,
    WorkerFuture,
>(
    candidates: Vec<AutoSelectUsage>,
    json: bool,
    max_concurrent: usize,
    acquire_lease: AcquireLease,
    prepare: Prepare,
    worker: Worker,
) -> Result<Vec<AutoSelectUsage>>
where
    AcquireLease: Fn(String) -> AcquireLeaseFuture + Send + Sync + 'static,
    AcquireLeaseFuture: std::future::Future<Output = Result<Lease>> + Send + 'static,
    Lease: Send + 'static,
    Prepare: Fn(AutoSelectUsage, Lease) -> PrepareFuture + Send + Sync + 'static,
    PrepareFuture:
        std::future::Future<Output = Result<(AutoSelectUsage, Prepared, Lease)>> + Send + 'static,
    Prepared: Send + 'static,
    Worker: Fn(AutoSelectUsage, Prepared, Lease, usage::NetworkPermitBudget) -> WorkerFuture
        + Send
        + Sync
        + 'static,
    WorkerFuture: std::future::Future<Output = Result<AutoSelectUsage>> + Send + 'static,
{
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let acquire_lease = std::sync::Arc::new(acquire_lease);
    let prepare = std::sync::Arc::new(prepare);
    let worker = std::sync::Arc::new(worker);
    let mut tasks: tokio::task::JoinSet<Result<AutoSelectUsage>> = tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();

    for candidate in candidates {
        let tracked_alias = candidate.alias.clone();
        let semaphore = std::sync::Arc::clone(&semaphore);
        let acquire_lease = std::sync::Arc::clone(&acquire_lease);
        let prepare = std::sync::Arc::clone(&prepare);
        let worker = std::sync::Arc::clone(&worker);
        let task = tasks.spawn(async move {
            // Keep profile-lock wait time outside the network concurrency
            // budget while carrying the acquired lease through all credential
            // preparation and reset-card I/O below.
            let lease = acquire_lease(candidate.alias.clone()).await?;
            let (candidate, prepared, lease) = prepare(candidate, lease).await?;
            let network = usage::NetworkPermitBudget::new(usage::first_network_permit(semaphore));
            worker(candidate, prepared, lease, network).await
        });
        let previous = task_aliases.insert(task.id(), tracked_alias);
        debug_assert!(previous.is_none());
    }

    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Checking reset cards", tasks.len()))
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

    let mut enriched = Vec::with_capacity(outcomes.len());
    let mut worker_failures = Vec::new();
    for outcome in outcomes {
        match outcome {
            NamedTaskOutcome::Completed { alias, value } => match value {
                Ok(candidate) if candidate.alias == alias => {
                    if let Some(error) = candidate.usage.reset_credits_error.as_deref() {
                        worker_failures.push((
                            alias,
                            format!("reset-card details are unavailable: {error}"),
                        ));
                    } else {
                        enriched.push(candidate);
                    }
                }
                Ok(candidate) => worker_failures.push((
                    alias.clone(),
                    format!(
                        "reset-card worker for '{alias}' returned data for '{}'",
                        candidate.alias
                    ),
                )),
                Err(error) => worker_failures.push((alias, format!("{error:#}"))),
            },
            NamedTaskOutcome::Failed { alias, detail } => {
                worker_failures.push((alias, detail));
            }
        }
    }
    if !worker_failures.is_empty() {
        return Err(batch_failure_error(
            "one or more reset-card detail workers failed; no reset card was requested and no profile was switched",
            worker_failures,
        ));
    }

    Ok(enriched)
}

#[derive(Clone)]
struct AutoSelectProfileSnapshot {
    path: std::path::PathBuf,
    info: jwt::AccountInfo,
    binding: jwt::StrictAccountBinding,
}

struct PreparedAutoSelectResetCardDetails {
    snapshot: AutoSelectProfileSnapshot,
    request: usage::PreparedResetCreditEnrichment,
}

async fn prepare_auto_select_reset_card_details(
    candidate: AutoSelectUsage,
    snapshot: AutoSelectProfileSnapshot,
    lease: profile::ProfileLease,
) -> Result<(
    AutoSelectUsage,
    PreparedAutoSelectResetCardDetails,
    profile::ProfileLease,
)> {
    let alias = candidate.alias.as_str();
    let request = usage::prepare_reset_credit_enrichment_with_existing_lease(
        alias,
        &snapshot.path,
        &lease,
        &snapshot.binding,
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.detail))
    .with_context(|| {
        format!(
            "reset-card details for '{alias}' could not be prepared; no reset card was requested and no profile was switched"
        )
    })?;
    Ok((
        candidate,
        PreparedAutoSelectResetCardDetails { snapshot, request },
        lease,
    ))
}

async fn enrich_auto_select_reset_card_details(
    mut candidate: AutoSelectUsage,
    prepared: PreparedAutoSelectResetCardDetails,
    lease: profile::ProfileLease,
    mut network: usage::NetworkPermitBudget,
    client: &reqwest::Client,
) -> Result<AutoSelectUsage> {
    let alias = candidate.alias.as_str();
    let PreparedAutoSelectResetCardDetails { snapshot, request } = prepared;
    usage::execute_prepared_reset_credit_enrichment_with_existing_lease_and_client(
        request,
        &lease,
        &mut candidate.usage,
        client,
        &mut network,
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.detail))
    .with_context(|| {
        format!(
            "reset-card details for '{alias}' could not be loaded; no reset card was requested and no profile was switched"
        )
    })?;

    if matches!(
        candidate.origin,
        AutoSelectUsageOrigin::CoreProbe | AutoSelectUsageOrigin::CachedQuotaOnly
    ) {
        let baseline = candidate.cache_baseline.take().with_context(|| {
            format!("quota-only usage for '{alias}' has no exact cache baseline")
        })?;
        let mut outcomes = cache::complete_core_probes_bound_async(vec![
            cache::CoreProbeCacheUpdate {
                alias: alias.to_string(),
                binding: snapshot.binding.clone(),
                baseline,
                usage: candidate.usage.clone(),
                reset_metadata: cache::CoreProbeResetMetadata::Complete,
            },
        ])
        .await
        .with_context(|| {
            format!(
                "conditionally publishing reset-card details for '{alias}'; no reset card was requested and no profile was switched"
            )
        })?;
        let outcome = outcomes
            .pop()
            .context("single reset-card cache completion returned no result")?;
        if outcome.alias != alias || !outcomes.is_empty() {
            anyhow::bail!(
                "reset-card cache completion did not match profile '{alias}'; no reset card was requested and no profile was switched"
            );
        }
        candidate.usage = outcome.usage;
        if outcome.reset_metadata_complete {
            candidate.origin = AutoSelectUsageOrigin::CachedComplete;
            candidate.cache_baseline = None;
        } else {
            candidate.origin = AutoSelectUsageOrigin::CachedQuotaOnly;
            candidate.cache_baseline = Some(outcome.baseline);
        }
    }

    drop(lease);
    Ok(candidate)
}

fn ensure_expected_profile_binding(
    alias: &str,
    expected: &jwt::StrictAccountBinding,
    actual: Option<jwt::StrictAccountBinding>,
) -> Result<()> {
    if actual.as_ref() != Some(expected) {
        anyhow::bail!(
            "profile '{alias}' changed account identity during automatic selection; no reset card was requested and no profile was switched"
        );
    }
    Ok(())
}

fn verify_selected_profile_binding(
    lease: &profile::ProfileLease,
    expected: &jwt::StrictAccountBinding,
) -> Result<jwt::AccountInfo> {
    let alias = lease.alias();
    let path = profile::profile_auth_path(alias)?;
    let info = auth::read_account_info_checked(&path)
        .with_context(|| format!("revalidating automatic-selection target '{alias}'"))?;
    ensure_expected_profile_binding(alias, expected, info.strict_binding())?;
    Ok(info)
}

async fn plan_best_profile(json: bool, card_policy: CardPolicy) -> Result<SelectionPlan> {
    let profile_accounts = profile::load_profile_accounts_checked()?;
    if profile_accounts.is_empty() {
        anyhow::bail!(
            "no saved profiles; run `codex-switch-global-pace login` or `codex-switch-global-pace import <path>` first"
        );
    }

    let profiles = profile_accounts
        .iter()
        .map(|account| account.alias.clone())
        .collect::<Vec<_>>();
    let mut profile_snapshots = std::collections::HashMap::with_capacity(profile_accounts.len());
    for account in profile_accounts {
        let profile::ProfileAccountSnapshot { alias, path, info } = account;
        let binding = info.strict_binding().with_context(|| {
            format!("profile '{alias}' needs a verified account id and email for auto-select")
        })?;
        profile_snapshots.insert(
            alias.clone(),
            AutoSelectProfileSnapshot {
                path,
                info,
                binding,
            },
        );
    }
    let profile_snapshots = std::sync::Arc::new(profile_snapshots);
    let bindings = profile_snapshots
        .iter()
        .map(|(alias, snapshot)| (alias.clone(), snapshot.binding.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let cache_snapshot = cache::get_auto_select_usage_snapshot(&bindings)?;
    let needs_usage_network = profiles
        .iter()
        .any(|alias| !cache_snapshot.has_fresh_usage(alias));
    let mut shared_client = needs_usage_network
        .then(auth::build_http_client)
        .transpose()
        .context("building the shared automatic-selection HTTP client")?;
    let cache_snapshot = std::sync::Arc::new(std::sync::Mutex::new(cache_snapshot));
    let mut collected = collect_best_profile_usage_with(
        profiles,
        UsageCollectionOptions {
            json,
            max_concurrent: config::get().network.max_concurrent,
        },
        {
            let cache_snapshot = std::sync::Arc::clone(&cache_snapshot);
            move |alias| {
                let cache_snapshot = std::sync::Arc::clone(&cache_snapshot);
                async move {
                    let mut cache_snapshot = cache_snapshot
                        .lock()
                        .map_err(|_| anyhow::anyhow!("auto-select cache snapshot lock poisoned"))?;
                    cache_snapshot.take(&alias)
                }
            }
        },
        {
            let profile_snapshots = std::sync::Arc::clone(&profile_snapshots);
            move |alias| {
                profile_snapshots
                    .get(alias)
                    .map(|snapshot| snapshot.path.clone())
                    .with_context(|| {
                        format!("profile '{alias}' disappeared from the auto-select snapshot")
                    })
            }
        },
        move |alias| async move {
            match profile::acquire_profile_lease_async(alias.clone()).await {
                Ok(lease) => Ok(Some(lease)),
                Err(error) => {
                    tracing::warn!("[{alias}] profile lock failed during auto-select: {error:#}");
                    Ok(None)
                }
            }
        },
        {
            let profile_snapshots = std::sync::Arc::clone(&profile_snapshots);
            move |alias, path, lease| {
                let profile_snapshots = std::sync::Arc::clone(&profile_snapshots);
                async move {
                    let expected_binding = profile_snapshots
                        .get(&alias)
                        .map(|snapshot| snapshot.binding.clone())
                        .with_context(|| {
                            format!("profile '{alias}' disappeared from the auto-select snapshot")
                        })?;
                    match usage::prepare_core_usage_unattended_with_existing_lease(
                        &alias,
                        &path,
                        &lease,
                        &expected_binding,
                    )
                    .await
                    {
                        Ok(prepared) => Ok(Some((prepared, lease))),
                        Err(error) => {
                            tracing::warn!(
                                "[{alias}] usage preparation failed during auto-select: {error}"
                            );
                            Ok(None)
                        }
                    }
                }
            }
        },
        {
            let shared_client = shared_client.clone();
            move |alias, prepared, lease, mut network| {
                let client = shared_client.clone();
                async move {
                    let client = client
                        .context("automatic-selection network work has no shared HTTP client")?;
                    match usage::execute_prepared_core_usage_with_existing_lease_and_client(
                        prepared,
                        &lease,
                        &client,
                        &mut network,
                    )
                    .await
                    {
                        Ok(usage) => Ok(Some(usage)),
                        Err(e) => {
                            tracing::warn!("[{alias}] usage fetch failed during auto-select: {e}");
                            Ok(None)
                        }
                    }
                }
            }
        },
    )
    .await?;

    if collected.is_empty() {
        anyhow::bail!("all usage queries failed");
    }

    publish_auto_select_core_probes(&mut collected, &bindings).await?;

    let safety_7d = config::get().use_cfg.safety_margin_7d;
    let team_priority = config::get().use_cfg.team_priority;
    let initial_now = auth::now_unix_secs()?;
    let initial_scored = score_profile_candidates_with_info(
        auto_select_scoring_input(&collected),
        initial_now,
        safety_7d,
        team_priority,
        |alias| {
            profile_snapshots
                .get(alias)
                .map(|snapshot| snapshot.info.clone())
                .with_context(|| {
                    format!("profile '{alias}' disappeared from the auto-select snapshot")
                })
        },
    )?;
    let (initial_top_candidate, initial_top_usage, initial_top_score) = initial_scored
        .first()
        .map(|(c, u, s)| (c.clone(), u.clone(), *s))
        .context("failed to select best profile")?;
    let binding_for = |alias: &str| {
        bindings
            .get(alias)
            .cloned()
            .with_context(|| format!("profile '{alias}' has no auto-select identity snapshot"))
    };

    if usage::is_candidate_eligible(&initial_top_candidate, safety_7d) {
        let target_binding = binding_for(&initial_top_candidate.alias)?;
        return Ok(SelectionPlan::Ready(ReadySelection {
            outcome: SelectOutcome {
                alias: initial_top_candidate.alias,
                usage: initial_top_usage,
                score: initial_top_score,
                evaluated_at: initial_now,
                revival_hint: None,
            },
            target_binding,
        }));
    }

    let reset_detail_aliases = reset_detail_aliases_if_pool_exhausted(&initial_scored, safety_7d);
    if reset_detail_aliases.is_empty() {
        let blocker = usage::explicit_account_blocker(&initial_top_usage)
            .context("exhausted automatic-selection pool has no resettable profile")?;
        anyhow::bail!(
            "all selectable profiles are blocked by account/workspace restrictions ({blocker}); no reset card was requested and no profile was switched"
        );
    }

    let reset_detail_aliases = reset_detail_aliases
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let matched_candidates = collected
        .iter()
        .filter(|candidate| reset_detail_aliases.contains(&candidate.alias))
        .count();
    if matched_candidates != reset_detail_aliases.len() {
        anyhow::bail!(
            "reset-card candidates did not match the automatic-selection snapshot; no reset card was requested and no profile was switched"
        );
    }
    let reset_detail_inputs = collected
        .iter()
        .filter(|candidate| {
            reset_detail_aliases.contains(&candidate.alias)
                && needs_reset_card_enrichment(candidate)
        })
        .cloned()
        .collect::<Vec<_>>();
    let enriched = if reset_detail_inputs.is_empty() {
        Vec::new()
    } else {
        let reset_client = match shared_client.as_ref() {
            Some(client) => client.clone(),
            None => {
                let client = auth::build_http_client()
                    .context("building the shared automatic-selection reset-card HTTP client")?;
                shared_client = Some(client.clone());
                client
            }
        };
        collect_reset_card_details_with(
            reset_detail_inputs,
            json,
            config::get().network.max_concurrent,
            move |alias| async move {
                profile::acquire_profile_lease_async(alias.clone())
                    .await
                    .with_context(|| format!("locking profile for reset-card lookup: {alias}"))
            },
            {
                let profile_snapshots = std::sync::Arc::clone(&profile_snapshots);
                move |candidate, lease| {
                    let profile_snapshots = std::sync::Arc::clone(&profile_snapshots);
                    async move {
                        let snapshot = profile_snapshots
                            .get(&candidate.alias)
                            .cloned()
                            .with_context(|| {
                                format!(
                                    "profile '{}' disappeared from the reset-card snapshot",
                                    candidate.alias
                                )
                            })?;
                        prepare_auto_select_reset_card_details(candidate, snapshot, lease).await
                    }
                }
            },
            {
                let reset_client = reset_client.clone();
                move |candidate, prepared, lease, network_permit| {
                    let reset_client = reset_client.clone();
                    async move {
                        enrich_auto_select_reset_card_details(
                            candidate,
                            prepared,
                            lease,
                            network_permit,
                            &reset_client,
                        )
                        .await
                    }
                }
            },
        )
        .await?
    };
    let mut enriched_by_alias = std::collections::HashMap::with_capacity(enriched.len());
    for candidate in enriched {
        let alias = candidate.alias.clone();
        if enriched_by_alias.insert(alias.clone(), candidate).is_some() {
            anyhow::bail!(
                "duplicate reset-card result for profile '{alias}'; no reset card was requested and no profile was switched"
            );
        }
    }
    for candidate in &mut collected {
        if let Some(enriched) = enriched_by_alias.remove(&candidate.alias) {
            *candidate = enriched;
        }
    }
    if !enriched_by_alias.is_empty() {
        anyhow::bail!(
            "reset-card results did not match the automatic-selection snapshot; no reset card was requested and no profile was switched"
        );
    }

    // Reset-card lookups can cross a quota-window reset boundary. Rebuild all
    // candidates at the response time before deciding whether a card is still
    // needed.
    let now = auth::now_unix_secs()?;
    let scored = score_profile_candidates_with_info(
        auto_select_scoring_input(&collected),
        now,
        safety_7d,
        team_priority,
        |alias| {
            profile_snapshots
                .get(alias)
                .map(|snapshot| snapshot.info.clone())
                .with_context(|| {
                    format!("profile '{alias}' disappeared from the auto-select snapshot")
                })
        },
    )?;
    let (top_candidate, top_usage, top_score) = scored
        .first()
        .map(|(candidate, usage, score)| (candidate.clone(), usage.clone(), *score))
        .context("failed to select best profile after reset-card lookup")?;

    if usage::is_candidate_eligible(&top_candidate, safety_7d) {
        let target_binding = binding_for(&top_candidate.alias)?;
        return Ok(SelectionPlan::Ready(ReadySelection {
            outcome: SelectOutcome {
                alias: top_candidate.alias,
                usage: top_usage,
                score: top_score,
                evaluated_at: now,
                revival_hint: None,
            },
            target_binding,
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
        let target_binding = binding_for(&top_candidate.alias)?;
        return Ok(SelectionPlan::Ready(ReadySelection {
            outcome: SelectOutcome {
                alias: top_candidate.alias,
                usage: top_usage,
                score: top_score,
                evaluated_at: now,
                revival_hint: None,
            },
            target_binding,
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
        evaluated_at: now,
        revival_hint: hint,
    };

    if !approved {
        let target_binding = binding_for(&top_candidate.alias)?;
        return Ok(SelectionPlan::Ready(ReadySelection {
            outcome: top_outcome(Some(RevivalHint {
                alias: target_alias,
                card_count,
            })),
            target_binding,
        }));
    }

    let target_binding = binding_for(&target_candidate.alias)?;
    Ok(SelectionPlan::Revive(PendingRevival {
        target_candidate,
        target_credit,
        target_binding,
        safety_7d,
        client: shared_client,
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
    account_info: &jwt::AccountInfo,
    now: i64,
    released_one_pool_member: bool,
) -> usage::Candidate {
    let mut candidate = usage::Candidate::from_usage(
        base.alias.clone(),
        usage,
        usage::normalized_plan_kind(usage, account_info),
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
        target_binding,
        safety_7d,
        client,
    } = plan;
    let client = match client {
        Some(client) => client,
        None => auth::build_http_client().context("building the reset-card revival HTTP client")?,
    };
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
    let preflight_usage = usage::fetch_usage_retried_with_existing_lease_and_client(
        &target_alias,
        &target_path,
        usage::Refresh::Forced,
        authorized.lease(),
        &target_binding,
        &client,
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.detail))
    .context("reset-card preflight failed; no card was requested and no profile was switched")?;
    let preflight_info = auth::read_account_info_checked(&target_path).with_context(|| {
        format!(
            "reading current profile metadata for reset-card preflight: {target_alias}; no card was requested and no profile was switched"
        )
    })?;
    let preflight_now = auth::now_unix_secs()?;
    let preflight_candidate = candidate_from_revival_usage(
        &target_candidate,
        &preflight_usage,
        &preflight_info,
        preflight_now,
        false,
    );
    let preflight_score = usage::score_unified(&preflight_candidate, safety_7d);
    if let Some(blocker) = usage::explicit_account_blocker(&preflight_usage) {
        anyhow::bail!(
            "'{target_alias}' became blocked by an account/workspace restriction ({blocker}); no reset card was requested and no profile was switched"
        );
    }
    if !preflight_candidate.has_required_quota_data() {
        anyhow::bail!(
            "'{target_alias}' returned incomplete authoritative quota data during reset-card preflight; no reset card was requested and no profile was switched"
        );
    }
    if usage::is_candidate_eligible(&preflight_candidate, safety_7d) {
        return Ok(RevivalExecution {
            outcome: SelectOutcome {
                alias: target_alias,
                usage: preflight_usage,
                score: preflight_score,
                evaluated_at: preflight_now,
                revival_hint: None,
            },
            side_effect: RevivalSideEffect::None,
            info: preflight_info,
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

    match usage::consume_reset_credit_by_id_leased_with_client(
        &target_alias,
        &target_path,
        target_credit,
        authorized.lease(),
        &client,
    )
    .await
    {
        Ok(_consumed) => {
            if let Err(err) = cache::invalidate(&target_alias) {
                tracing::warn!("Failed to invalidate usage cache for {target_alias}: {err}");
            }
            let failure_summary = match usage::fetch_usage_retried_with_existing_lease_and_client(
                &target_alias,
                &target_path,
                usage::Refresh::Forced,
                authorized.lease(),
                &target_binding,
                &client,
            )
            .await
            {
                Ok(revived_usage) => {
                    let current_metadata = auth::read_account_info_checked(&target_path)
                        .context("reading current profile metadata after reset-card redemption")
                        .and_then(|info| {
                            let now = auth::now_unix_secs()
                                .context("reading evaluation time after reset-card redemption")?;
                            Ok((info, now))
                        });
                    match current_metadata {
                        Ok((revived_info, revived_now)) => {
                            let revived_candidate = candidate_from_revival_usage(
                                &target_candidate,
                                &revived_usage,
                                &revived_info,
                                revived_now,
                                true,
                            );
                            let score = usage::score_unified(&revived_candidate, safety_7d);
                            if usage::is_candidate_eligible(&revived_candidate, safety_7d) {
                                return Ok(RevivalExecution {
                                    outcome: SelectOutcome {
                                        alias: target_alias.clone(),
                                        usage: revived_usage,
                                        score,
                                        evaluated_at: revived_now,
                                        revival_hint: None,
                                    },
                                    side_effect: RevivalSideEffect::Consumed {
                                        alias: target_alias,
                                    },
                                    info: revived_info,
                                });
                            }
                            tracing::warn!(
                                "[{target_alias}] still exhausted after consuming a reset card; not consuming a second card"
                            );
                            "quota remained exhausted after refresh"
                        }
                        Err(error) => {
                            tracing::warn!(
                                "[{target_alias}] could not revalidate current profile metadata after consuming a reset card: {error:#}"
                            );
                            "current profile metadata could not be revalidated"
                        }
                    }
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
    let (outcome, switch_outcome, selected_info) = match plan_best_profile(json, card_policy)
        .await?
    {
        SelectionPlan::Ready(plan) => {
            let confirmed =
                profile::prepare_and_confirm_profile_switch(&plan.outcome.alias, allow_prompt)?;
            let lease = profile::acquire_profile_lease_async(plan.outcome.alias.clone()).await?;
            let selected_info = verify_selected_profile_binding(&lease, &plan.target_binding)?;
            let switch_outcome =
                profile::commit_confirmed_profile_switch_with_lease(confirmed, &lease)?;
            (plan.outcome, switch_outcome, selected_info)
        }
        SelectionPlan::Revive(plan) => {
            // Prompt without owning the target lease, then reacquire it and
            // revalidate the exact target/live snapshots and planned identity
            // before card redemption can begin.
            let target_alias = plan.target_candidate.alias.clone();
            let target_binding = plan.target_binding.clone();
            let confirmed =
                profile::prepare_and_confirm_profile_switch(&target_alias, allow_prompt)?;
            let lease = profile::acquire_profile_lease_async(target_alias).await?;
            verify_selected_profile_binding(&lease, &target_binding)?;
            let authorized =
                profile::authorize_confirmed_profile_switch_before_side_effect(confirmed, lease)?;
            let RevivalExecution {
                outcome,
                side_effect,
                info,
            } = execute_revival(plan, &authorized).await?;
            let switch_outcome =
                side_effect.commit_result(profile::commit_authorized_profile_switch(authorized))?;
            (outcome, switch_outcome, info)
        }
    };
    let SelectOutcome {
        alias: best_alias,
        usage: best_usage,
        score: best_score,
        evaluated_at,
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

    if json {
        print_json(&output::JsonBest {
            switched_to: best_alias.clone(),
            account: account_to_json(&selected_info, best_usage.plan_type.as_deref()),
            usage: usage_to_json(Ok(&best_usage), evaluated_at)?,
            score: best_score,
            mode: "unified".to_string(),
            hint: revival_hint.as_ref().map(revival_hint_message),
        })?;
    } else {
        println!("{}", color::success(&format!("Switched to: {best_alias}")));
        print_usage_line(&best_usage, evaluated_at);
        if let Some(hint) = &revival_hint {
            println!("  {}", color::dim(&revival_hint_message(hint)));
        }
    }

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
            Self::set_value(name, value)
        }

        fn set_value(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(name);
            // SAFETY: callers hold the crate-wide locks for every environment
            // namespace they mutate for the guard's full lifetime.
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

    fn binding(account_id: &str, email: &str) -> jwt::StrictAccountBinding {
        jwt::StrictAccountBinding {
            account_id: account_id.to_string(),
            email: email.to_string(),
        }
    }

    fn scored_quota_candidate(
        alias: &str,
        weekly_used: f64,
        explicit_blocker: bool,
    ) -> (usage::Candidate, usage::UsageInfo, f64) {
        const NOW: i64 = 1_000_000;
        let usage = usage::UsageInfo {
            secondary: Some(usage::WindowUsage {
                used_percent: Some(weekly_used),
                resets_at: Some(NOW + usage::WINDOW_7D_SECS),
                window_minutes: Some(usage::WINDOW_7D_SECS / 60),
            }),
            spend_control_reached: explicit_blocker,
            ..usage::UsageInfo::default()
        };
        let candidate =
            usage::Candidate::from_usage(alias.to_string(), &usage, jwt::PlanKind::Plus, 0, NOW);
        (candidate, usage, 0.0)
    }

    fn collected_usage(alias: &str, origin: AutoSelectUsageOrigin) -> AutoSelectUsage {
        let cache_baseline = (origin != AutoSelectUsageOrigin::CachedComplete).then(|| {
            cache::AutoSelectUsageCacheLookup::absent_for_test(alias)
                .into_parts()
                .1
        });
        AutoSelectUsage {
            alias: alias.to_string(),
            usage: usage::UsageInfo::default(),
            origin,
            cache_baseline,
        }
    }

    fn reset_detail_test_network_budget() -> usage::NetworkPermitBudget {
        usage::NetworkPermitBudget::new(usage::first_network_permit(std::sync::Arc::new(
            tokio::sync::Semaphore::new(1),
        )))
    }

    async fn prepare_and_enrich_reset_card_details_for_test(
        candidate: AutoSelectUsage,
        snapshot: AutoSelectProfileSnapshot,
        lease: profile::ProfileLease,
        client: &reqwest::Client,
    ) -> Result<AutoSelectUsage> {
        let (candidate, prepared, lease) =
            prepare_auto_select_reset_card_details(candidate, snapshot, lease).await?;
        enrich_auto_select_reset_card_details(
            candidate,
            prepared,
            lease,
            reset_detail_test_network_budget(),
            client,
        )
        .await
    }

    fn identity_jwt(email: &str) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "email": email,
                "exp": 4_102_444_800_i64
            }))
            .unwrap(),
        );
        format!("header.{payload}.signature")
    }

    fn access_jwt(exp: i64) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "exp": exp
            }))
            .unwrap(),
        );
        format!("header.{payload}.signature")
    }

    fn write_auto_select_test_snapshot(
        alias: &str,
        account_id: &str,
        email: &str,
        refresh_token: Option<&str>,
    ) -> AutoSelectProfileSnapshot {
        let path = profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut tokens = serde_json::json!({
            "id_token": identity_jwt(email),
            "access_token": access_jwt(4_102_444_800_i64),
            "account_id": account_id
        });
        if let Some(refresh_token) = refresh_token {
            tokens
                .as_object_mut()
                .unwrap()
                .insert("refresh_token".to_string(), refresh_token.into());
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "tokens": tokens })).unwrap(),
        )
        .unwrap();
        AutoSelectProfileSnapshot {
            path,
            info: jwt::AccountInfo::default(),
            binding: binding(account_id, email),
        }
    }

    #[test]
    fn incomplete_list_identity_never_schedules_workspace_network_work() {
        assert!(!list_workspace_needs_refresh(
            None,
            &cache::WorkspaceState::Unresolved,
            false,
        ));
        assert!(!list_workspace_needs_refresh(
            None,
            &cache::WorkspaceState::Unresolved,
            true,
        ));
        assert!(cached_workspace_state(&cache::CacheSnapshot::default(), None).is_none());
    }

    #[test]
    fn cached_workspace_state_is_shared_by_every_alias_with_the_same_account_id() {
        let mut snapshot = cache::CacheSnapshot::default();
        snapshot.workspaces.insert(
            "acct-shared".to_string(),
            cache::WorkspaceState::Named("Shared workspace".to_string()),
        );
        let first = binding("acct-shared", "first@example.com");
        let second = binding("acct-shared", "second@example.com");

        assert_eq!(
            cached_workspace_state(&snapshot, Some(&first)),
            Some(cache::WorkspaceState::Named("Shared workspace".to_string()))
        );
        assert_eq!(
            cached_workspace_state(&snapshot, Some(&second)),
            Some(cache::WorkspaceState::Named("Shared workspace".to_string()))
        );
        assert_eq!(snapshot.workspaces.len(), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn list_workspace_cache_publication_does_not_hold_the_network_slot() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());

        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/accounts",
            axum::routing::get(move || {
                let request_tx = request_tx.clone();
                async move {
                    request_tx.send(()).unwrap();
                    axum::Json(serde_json::json!({
                        "accounts": [{
                            "id": "acct-workspace-slot",
                            "name": "Workspace Slot",
                            "structure": "workspace"
                        }],
                        "account_ordering": ["acct-workspace-slot"]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _accounts_url = EnvVarGuard::set_value(
            "CS_ACCOUNTS_CHECK_URL",
            format!("http://{address}/accounts"),
        );

        let cache_lock_holder = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock_holder).unwrap();

        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let network_permit = semaphore.clone().acquire_owned().await.unwrap();
        let auth = serde_json::json!({
            "tokens": {
                "account_id": "acct-workspace-slot",
                "access_token": "workspace-access-token",
                "id_token": ""
            }
        });
        let client = reqwest::Client::new();
        let worker = tokio::spawn(async move {
            lookup_and_publish_list_workspace_state(
                network_permit,
                &auth,
                "acct-workspace-slot",
                &client,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv())
            .await
            .expect("workspace lookup did not reach the server")
            .expect("workspace lookup request channel closed");
        let reacquired = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            semaphore.clone().acquire_owned(),
        )
        .await;
        let publication_was_waiting = !worker.is_finished();
        let network_slot_was_released = reacquired.is_ok();
        drop(reacquired);
        fs4::FileExt::unlock(&cache_lock_holder).unwrap();

        let state = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .expect("workspace cache publication did not finish")
            .expect("workspace list worker panicked")
            .unwrap();
        server.abort();

        assert!(
            network_slot_was_released,
            "workspace cache publication held the only network slot"
        );
        assert!(
            publication_was_waiting,
            "workspace cache publication did not wait on the held cache lock"
        );
        assert_eq!(
            state,
            cache::WorkspaceState::Named("Workspace Slot".to_string())
        );
    }

    #[test]
    fn automatic_selection_rejects_an_alias_rebound_after_scoring() {
        let expected = binding("acct-before", "owner@example.com");
        assert!(
            ensure_expected_profile_binding("target", &expected, Some(expected.clone())).is_ok()
        );

        let error = ensure_expected_profile_binding(
            "target",
            &expected,
            Some(binding("acct-after", "other@example.com")),
        )
        .expect_err("a rebound alias must not be switched or receive a reset-card request");
        let detail = format!("{error:#}");
        assert!(detail.contains("changed account identity"), "{detail}");
        assert!(detail.contains("no reset card was requested"), "{detail}");
        assert!(detail.contains("no profile was switched"), "{detail}");
    }

    #[tokio::test]
    async fn best_usage_collection_distinguishes_complete_quota_only_and_core_probe_origins() {
        let mut collected = collect_best_profile_usage_with(
            vec![
                "cached".to_string(),
                "probed".to_string(),
                "quota".to_string(),
            ],
            UsageCollectionOptions {
                json: true,
                max_concurrent: 2,
            },
            |alias| async move {
                Ok(match alias.as_str() {
                    "cached" => cache::AutoSelectUsageCacheLookup::fresh_for_test(
                        alias,
                        usage::UsageInfo::default(),
                    ),
                    "quota" => cache::AutoSelectUsageCacheLookup::quota_only_for_test(
                        alias,
                        usage::UsageInfo::default(),
                    ),
                    _ => cache::AutoSelectUsageCacheLookup::absent_for_test(alias),
                })
            },
            |alias| Ok(std::path::PathBuf::from(alias)),
            |_alias| async { Ok(Some(())) },
            |_alias, path, lease| async move { Ok(Some((path, lease))) },
            |_alias, _path, _lease, _network_permit| async {
                Ok(Some(usage::UsageInfo::default()))
            },
        )
        .await
        .unwrap();
        collected.sort_by(|left, right| left.alias.cmp(&right.alias));

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].alias, "cached");
        assert_eq!(collected[0].origin, AutoSelectUsageOrigin::CachedComplete);
        assert_eq!(collected[1].alias, "probed");
        assert_eq!(collected[1].origin, AutoSelectUsageOrigin::CoreProbe);
        assert_eq!(collected[2].alias, "quota");
        assert_eq!(collected[2].origin, AutoSelectUsageOrigin::CachedQuotaOnly);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn core_probe_batch_caches_quota_and_rescores_an_intervening_generation() {
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let new_binding = binding("acct-new", "new@example.com");
        let race_binding = binding("acct-race", "race@example.com");
        let bindings = std::collections::HashMap::from([
            ("new".to_string(), new_binding.clone()),
            ("race".to_string(), race_binding.clone()),
        ]);
        let mut snapshot = cache::get_auto_select_usage_snapshot(&bindings).unwrap();
        let new_baseline = snapshot.take("new").unwrap().into_parts().1;
        let race_baseline = snapshot.take("race").unwrap().into_parts().1;

        let intervening = cache::put_bound_versioned(
            "race",
            &race_binding,
            &usage::UsageInfo {
                secondary: Some(usage::WindowUsage {
                    used_percent: Some(12.0),
                    ..usage::WindowUsage::default()
                }),
                ..usage::UsageInfo::default()
            },
        )
        .unwrap();
        let mut collected = vec![
            AutoSelectUsage {
                alias: "new".to_string(),
                usage: usage::UsageInfo {
                    secondary: Some(usage::WindowUsage {
                        used_percent: Some(10.0),
                        ..usage::WindowUsage::default()
                    }),
                    ..usage::UsageInfo::default()
                },
                origin: AutoSelectUsageOrigin::CoreProbe,
                cache_baseline: Some(new_baseline),
            },
            AutoSelectUsage {
                alias: "race".to_string(),
                usage: usage::UsageInfo {
                    secondary: Some(usage::WindowUsage {
                        used_percent: Some(88.0),
                        ..usage::WindowUsage::default()
                    }),
                    ..usage::UsageInfo::default()
                },
                origin: AutoSelectUsageOrigin::CoreProbe,
                cache_baseline: Some(race_baseline),
            },
        ];

        publish_auto_select_core_probes(&mut collected, &bindings)
            .await
            .unwrap();

        assert_eq!(collected[0].origin, AutoSelectUsageOrigin::CachedQuotaOnly);
        assert_eq!(
            collected[0]
                .usage
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(10.0)
        );
        assert_eq!(collected[1].origin, AutoSelectUsageOrigin::CachedComplete);
        assert_eq!(
            collected[1].usage.cache_revision,
            intervening.cache_revision
        );
        assert_eq!(
            collected[1]
                .usage
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.0),
            "the surviving same-account generation must replace the stale probe before scoring"
        );

        let mut repeated = cache::get_auto_select_usage_snapshot(&bindings).unwrap();
        let new_lookup = repeated.take("new").unwrap();
        assert!(new_lookup.into_parts().0.is_some());
        assert!(
            cache::get_bound("new", &new_binding).unwrap().is_none(),
            "quota-only publication must not masquerade as complete usage"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_lease_wait_does_not_occupy_the_network_permit() {
        let locked_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let gate_for_acquire = std::sync::Arc::clone(&locked_gate);
        let acquire_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let barrier_for_acquire = std::sync::Arc::clone(&acquire_barrier);
        let (locked_started_tx, mut locked_started_rx) = tokio::sync::mpsc::channel(1);
        let (worker_started_tx, mut worker_started_rx) = tokio::sync::mpsc::channel(2);

        let collection = tokio::spawn(collect_best_profile_usage_with(
            vec!["locked".to_string(), "ready".to_string()],
            UsageCollectionOptions {
                json: true,
                max_concurrent: 1,
            },
            |alias| async move { Ok(cache::AutoSelectUsageCacheLookup::absent_for_test(alias)) },
            |alias| Ok(std::path::PathBuf::from(alias)),
            move |alias| {
                let gate = std::sync::Arc::clone(&gate_for_acquire);
                let barrier = std::sync::Arc::clone(&barrier_for_acquire);
                let locked_started_tx = locked_started_tx.clone();
                async move {
                    barrier.wait().await;
                    if alias == "locked" {
                        locked_started_tx.send(()).await.unwrap();
                        let _gate_permit = gate.acquire().await.unwrap();
                    }
                    Ok(Some(alias))
                }
            },
            |_alias, path, lease| async { Ok(Some((path, lease))) },
            move |alias, _path, lease_alias, _network_permit| {
                let worker_started_tx = worker_started_tx.clone();
                async move {
                    assert_eq!(lease_alias, alias);
                    worker_started_tx.send(alias).await.unwrap();
                    Ok(Some(usage::UsageInfo::default()))
                }
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(1), locked_started_rx.recv())
            .await
            .expect("locked lease acquisition did not start")
            .expect("locked lease start channel closed");
        let first_worker =
            tokio::time::timeout(std::time::Duration::from_secs(1), worker_started_rx.recv())
                .await
                .expect("ready alias was blocked behind a lease waiter")
                .expect("usage worker start channel closed");
        assert_eq!(first_worker, "ready");

        locked_gate.add_permits(1);
        let collected = collection.await.unwrap().unwrap();
        assert_eq!(collected.len(), 2);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn core_cache_preparation_wait_does_not_occupy_the_network_permit() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());

        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/usage",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let request_tx = request_tx.clone();
                async move {
                    let account_id = headers
                        .get("chatgpt-account-id")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    request_tx.send(account_id).unwrap();
                    axum::Json(serde_json::json!({
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
        let _usage_url = EnvVarGuard::set_value("CS_USAGE_URL", format!("http://{address}/usage"));

        let snapshots = std::sync::Arc::new(std::collections::HashMap::from([
            (
                "blocked".to_string(),
                write_auto_select_test_snapshot(
                    "blocked",
                    "acct-core-blocked",
                    "core-blocked@example.com",
                    Some("blocked-refresh-token"),
                ),
            ),
            (
                "ready".to_string(),
                write_auto_select_test_snapshot(
                    "ready",
                    "acct-core-ready",
                    "core-ready@example.com",
                    None,
                ),
            ),
        ]));
        let snapshots_by_path = std::sync::Arc::clone(&snapshots);
        let snapshots_by_prepare = std::sync::Arc::clone(&snapshots);
        let ready_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let gate_by_lease = std::sync::Arc::clone(&ready_gate);
        let (blocked_prepare_tx, mut blocked_prepare_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = reqwest::Client::new();

        let cache_lock_holder = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock_holder).unwrap();

        let collection = tokio::spawn(collect_best_profile_usage_with(
            vec!["blocked".to_string(), "ready".to_string()],
            UsageCollectionOptions {
                json: true,
                max_concurrent: 1,
            },
            |alias| async move { Ok(cache::AutoSelectUsageCacheLookup::absent_for_test(alias)) },
            move |alias| {
                snapshots_by_path
                    .get(alias)
                    .map(|snapshot| snapshot.path.clone())
                    .context("missing core preparation test snapshot")
            },
            move |alias| {
                let ready_gate = std::sync::Arc::clone(&gate_by_lease);
                async move {
                    if alias == "ready" {
                        let _gate_permit = ready_gate.acquire().await.unwrap();
                    }
                    profile::acquire_profile_lease_async(alias.clone())
                        .await
                        .map(Some)
                        .with_context(|| format!("locking core preparation test profile: {alias}"))
                }
            },
            move |alias, path, lease| {
                let snapshots = std::sync::Arc::clone(&snapshots_by_prepare);
                let blocked_prepare_tx = blocked_prepare_tx.clone();
                async move {
                    if alias == "blocked" {
                        blocked_prepare_tx.send(()).unwrap();
                    }
                    let expected_binding = snapshots
                        .get(&alias)
                        .map(|snapshot| snapshot.binding.clone())
                        .context("missing core preparation test binding")?;
                    let prepared = usage::prepare_core_usage_unattended_with_existing_lease(
                        &alias,
                        &path,
                        &lease,
                        &expected_binding,
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!(error.detail))?;
                    Ok(Some((prepared, lease)))
                }
            },
            move |_alias, prepared, lease, mut network| {
                let client = client.clone();
                async move {
                    usage::execute_prepared_core_usage_with_existing_lease_and_client(
                        prepared,
                        &lease,
                        &client,
                        &mut network,
                    )
                    .await
                    .map(Some)
                    .map_err(|error| anyhow::anyhow!(error.detail))
                }
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(5), blocked_prepare_rx.recv())
            .await
            .expect("blocked core preparation did not start")
            .expect("blocked core preparation channel closed");
        ready_gate.add_permits(1);
        let ready_request =
            tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv()).await;
        fs4::FileExt::unlock(&cache_lock_holder).unwrap();
        let ready_request = ready_request
            .expect("cache preparation held the only core network permit")
            .expect("core request channel closed early")
            .expect("core request omitted its account routing header");
        assert_eq!(ready_request, "acct-core-ready");

        let collected = collection.await.unwrap().unwrap();
        server.abort();
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn eligible_pool_schedules_no_reset_card_details() {
        let scored = vec![
            scored_quota_candidate("eligible", 10.0, false),
            scored_quota_candidate("exhausted", 100.0, false),
        ];

        assert!(reset_detail_aliases_if_pool_exhausted(&scored, 20.0).is_empty());
    }

    #[test]
    fn post_detail_rescore_observes_a_quota_reset_boundary() {
        let _lock = lock_profile_test_environment();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let quota = usage::UsageInfo {
            secondary: Some(usage::WindowUsage {
                used_percent: Some(100.0),
                resets_at: Some(1_001),
                window_minutes: Some(usage::WINDOW_7D_SECS / 60),
            }),
            account_limited: true,
            ..usage::UsageInfo::default()
        };
        let info = || jwt::AccountInfo {
            plan_type: Some("plus".to_string()),
            ..jwt::AccountInfo::default()
        };

        let before = score_profile_candidates_with_info(
            vec![("boundary".to_string(), quota.clone())],
            1_000,
            20.0,
            false,
            |_| Ok(info()),
        )
        .unwrap();
        assert!(!usage::is_candidate_eligible(&before[0].0, 20.0));

        let after = score_profile_candidates_with_info(
            vec![("boundary".to_string(), quota)],
            1_002,
            20.0,
            false,
            |_| Ok(info()),
        )
        .unwrap();
        assert!(
            usage::is_candidate_eligible(&after[0].0, 20.0),
            "a reset crossed during detail lookup must avoid unnecessary card redemption"
        );
    }

    #[tokio::test]
    async fn exhausted_pool_enriches_only_incomplete_nonblocked_candidates() {
        let scored = vec![
            scored_quota_candidate("cached", 100.0, false),
            scored_quota_candidate("probed", 100.0, false),
            scored_quota_candidate("blocked", 100.0, true),
        ];
        let aliases = reset_detail_aliases_if_pool_exhausted(&scored, 20.0)
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let inputs = vec![
            collected_usage("cached", AutoSelectUsageOrigin::CachedComplete),
            collected_usage("probed", AutoSelectUsageOrigin::CoreProbe),
            collected_usage("blocked", AutoSelectUsageOrigin::CachedComplete),
        ]
        .into_iter()
        .filter(|candidate| {
            aliases.contains(&candidate.alias) && needs_reset_card_enrichment(candidate)
        })
        .collect::<Vec<_>>();
        let visited = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let visited_by_worker = std::sync::Arc::clone(&visited);

        let enriched = collect_reset_card_details_with(
            inputs,
            true,
            2,
            |_alias| async { Ok(()) },
            |candidate, lease| async move { Ok((candidate, (), lease)) },
            move |mut candidate, (), _lease, _network_permit| {
                let visited = std::sync::Arc::clone(&visited_by_worker);
                async move {
                    visited.lock().unwrap().push(candidate.alias.clone());
                    candidate.usage.reset_credits_available_count = Some(0);
                    candidate.usage.reset_credits_error = None;
                    Ok(candidate)
                }
            },
        )
        .await
        .unwrap();

        let mut visited = visited.lock().unwrap().clone();
        visited.sort();
        assert_eq!(visited, vec!["probed"]);
        assert_eq!(enriched.len(), 1);
        assert_eq!(enriched[0].alias, "probed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_detail_lease_wait_does_not_occupy_the_network_permit() {
        let locked_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let gate_for_acquire = std::sync::Arc::clone(&locked_gate);
        let acquire_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let barrier_for_acquire = std::sync::Arc::clone(&acquire_barrier);
        let (locked_started_tx, mut locked_started_rx) = tokio::sync::mpsc::channel(1);
        let (worker_started_tx, mut worker_started_rx) = tokio::sync::mpsc::channel(2);

        let collection = tokio::spawn(collect_reset_card_details_with(
            vec![
                collected_usage("locked", AutoSelectUsageOrigin::CachedComplete),
                collected_usage("ready", AutoSelectUsageOrigin::CachedComplete),
            ],
            true,
            1,
            move |alias| {
                let gate = std::sync::Arc::clone(&gate_for_acquire);
                let barrier = std::sync::Arc::clone(&barrier_for_acquire);
                let locked_started_tx = locked_started_tx.clone();
                async move {
                    barrier.wait().await;
                    if alias == "locked" {
                        locked_started_tx.send(()).await.unwrap();
                        let _gate_permit = gate.acquire().await.unwrap();
                    }
                    Ok(alias)
                }
            },
            |candidate, lease| async move { Ok((candidate, (), lease)) },
            move |candidate, (), lease_alias, _network_permit| {
                let worker_started_tx = worker_started_tx.clone();
                async move {
                    assert_eq!(lease_alias, candidate.alias);
                    worker_started_tx
                        .send(candidate.alias.clone())
                        .await
                        .unwrap();
                    Ok(candidate)
                }
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(1), locked_started_rx.recv())
            .await
            .expect("locked reset-detail lease acquisition did not start")
            .expect("locked reset-detail start channel closed");
        let first_worker =
            tokio::time::timeout(std::time::Duration::from_secs(1), worker_started_rx.recv())
                .await
                .expect("ready reset-detail alias was blocked behind a lease waiter")
                .expect("reset-detail worker start channel closed");
        assert_eq!(first_worker, "ready");

        locked_gate.add_permits(1);
        let enriched = collection.await.unwrap().unwrap();
        assert_eq!(enriched.len(), 2);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reset_cache_preparation_wait_does_not_occupy_the_network_permit() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());

        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/credits",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let request_tx = request_tx.clone();
                async move {
                    let account_id = headers
                        .get("chatgpt-account-id")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    request_tx.send(account_id).unwrap();
                    axum::Json(serde_json::json!({
                        "available_count": 0,
                        "credits": []
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _credits_url =
            EnvVarGuard::set_value("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let snapshots = std::sync::Arc::new(std::collections::HashMap::from([
            (
                "blocked".to_string(),
                write_auto_select_test_snapshot(
                    "blocked",
                    "acct-reset-blocked",
                    "reset-blocked@example.com",
                    Some("blocked-refresh-token"),
                ),
            ),
            (
                "ready".to_string(),
                write_auto_select_test_snapshot(
                    "ready",
                    "acct-reset-ready",
                    "reset-ready@example.com",
                    None,
                ),
            ),
        ]));
        let snapshots_by_prepare = std::sync::Arc::clone(&snapshots);
        let ready_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let gate_by_lease = std::sync::Arc::clone(&ready_gate);
        let (blocked_prepare_tx, mut blocked_prepare_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = reqwest::Client::new();

        let cache_lock_holder = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(home.path().join("cache.lock"))
            .unwrap();
        fs4::FileExt::lock(&cache_lock_holder).unwrap();

        let collection = tokio::spawn(collect_reset_card_details_with(
            vec![
                collected_usage("blocked", AutoSelectUsageOrigin::CachedComplete),
                collected_usage("ready", AutoSelectUsageOrigin::CachedComplete),
            ],
            true,
            1,
            move |alias| {
                let ready_gate = std::sync::Arc::clone(&gate_by_lease);
                async move {
                    if alias == "ready" {
                        let _gate_permit = ready_gate.acquire().await.unwrap();
                    }
                    profile::acquire_profile_lease_async(alias.clone())
                        .await
                        .with_context(|| format!("locking reset preparation test profile: {alias}"))
                }
            },
            move |candidate, lease| {
                let snapshots = std::sync::Arc::clone(&snapshots_by_prepare);
                let blocked_prepare_tx = blocked_prepare_tx.clone();
                async move {
                    if candidate.alias == "blocked" {
                        blocked_prepare_tx.send(()).unwrap();
                    }
                    let snapshot = snapshots
                        .get(&candidate.alias)
                        .cloned()
                        .context("missing reset preparation test snapshot")?;
                    prepare_auto_select_reset_card_details(candidate, snapshot, lease).await
                }
            },
            move |candidate, prepared, lease, network_permit| {
                let client = client.clone();
                async move {
                    enrich_auto_select_reset_card_details(
                        candidate,
                        prepared,
                        lease,
                        network_permit,
                        &client,
                    )
                    .await
                }
            },
        ));

        tokio::time::timeout(std::time::Duration::from_secs(5), blocked_prepare_rx.recv())
            .await
            .expect("blocked reset preparation did not start")
            .expect("blocked reset preparation channel closed");
        ready_gate.add_permits(1);
        let ready_request =
            tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv()).await;
        fs4::FileExt::unlock(&cache_lock_holder).unwrap();
        let ready_request = ready_request
            .expect("cache preparation held the only reset-detail network permit")
            .expect("reset-detail request channel closed early")
            .expect("reset-detail request omitted its account routing header");
        assert_eq!(ready_request, "acct-reset-ready");

        let enriched = collection.await.unwrap().unwrap();
        server.abort();
        assert_eq!(enriched.len(), 2);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reset_detail_cache_completion_does_not_occupy_the_network_permit() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());

        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/credits",
            axum::routing::get(move || {
                let request_tx = request_tx.clone();
                async move {
                    request_tx.send(()).unwrap();
                    axum::Json(serde_json::json!({
                        "available_count": 0,
                        "credits": []
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _credits_url =
            EnvVarGuard::set_value("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let mut snapshots = std::collections::HashMap::new();
        for (alias, account_id, email) in [
            ("first", "acct-first", "first@example.com"),
            ("second", "acct-second", "second@example.com"),
        ] {
            let path = profile::profile_auth_path(alias).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "tokens": {
                        "id_token": identity_jwt(email),
                        "access_token": "reset-detail-access-token",
                        "account_id": account_id
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            snapshots.insert(
                alias.to_string(),
                AutoSelectProfileSnapshot {
                    path,
                    info: jwt::AccountInfo::default(),
                    binding: binding(account_id, email),
                },
            );
        }
        let snapshots = std::sync::Arc::new(snapshots);
        let snapshots_by_prepare = std::sync::Arc::clone(&snapshots);
        let client = reqwest::Client::new();

        let cache_lock_path = home.path().join("cache.lock");
        let cache_lock_holder = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(cache_lock_path)
            .unwrap();
        fs4::FileExt::lock(&cache_lock_holder).unwrap();

        let collection = tokio::spawn(collect_reset_card_details_with(
            vec![
                collected_usage("first", AutoSelectUsageOrigin::CoreProbe),
                collected_usage("second", AutoSelectUsageOrigin::CoreProbe),
            ],
            true,
            1,
            |alias| async move {
                profile::acquire_profile_lease_async(alias.clone())
                    .await
                    .with_context(|| format!("locking test profile: {alias}"))
            },
            move |candidate, lease| {
                let snapshots = std::sync::Arc::clone(&snapshots_by_prepare);
                async move {
                    let snapshot = snapshots
                        .get(&candidate.alias)
                        .cloned()
                        .context("missing reset-detail test snapshot")?;
                    prepare_auto_select_reset_card_details(candidate, snapshot, lease).await
                }
            },
            move |candidate, prepared, lease, network_permit| {
                let client = client.clone();
                async move {
                    enrich_auto_select_reset_card_details(
                        candidate,
                        prepared,
                        lease,
                        network_permit,
                        &client,
                    )
                    .await
                }
            },
        ));

        let both_network_phases_started =
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                request_rx.recv().await?;
                request_rx.recv().await
            })
            .await;
        fs4::FileExt::unlock(&cache_lock_holder).unwrap();
        both_network_phases_started
            .expect("cache completion kept the only network permit")
            .expect("reset-detail request channel closed early");

        let enriched = collection.await.unwrap().unwrap();
        server.abort();
        assert_eq!(enriched.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_card_detail_failure_returns_no_partial_result_after_drain() {
        let temp = crate::fs_ops::create_direct_tempdir().unwrap();
        let completion_marker = temp.path().join("details-completed");
        let marker_by_worker = completion_marker.clone();
        let workers_started = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let barrier_by_worker = std::sync::Arc::clone(&workers_started);

        let error = collect_reset_card_details_with(
            vec![
                collected_usage("failed", AutoSelectUsageOrigin::CachedComplete),
                collected_usage("completed", AutoSelectUsageOrigin::CoreProbe),
            ],
            true,
            2,
            |_alias| async { Ok(()) },
            |candidate, lease| async move { Ok((candidate, (), lease)) },
            move |candidate, (), _lease, _network_permit| {
                let barrier = std::sync::Arc::clone(&barrier_by_worker);
                let marker = marker_by_worker.clone();
                async move {
                    barrier.wait().await;
                    if candidate.alias == "failed" {
                        let mut candidate = candidate;
                        candidate.usage.reset_credits_error =
                            Some("injected reset-detail failure".to_string());
                        return Ok(candidate);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    std::fs::write(marker, b"complete")?;
                    Ok(candidate)
                }
            },
        )
        .await
        .expect_err("one failed detail lookup must abort the complete selection batch");

        assert_eq!(std::fs::read(completion_marker).unwrap(), b"complete");
        let error = format!("{error:#}");
        assert!(error.contains("injected reset-detail failure"), "{error}");
        assert!(error.contains("no reset card was requested"), "{error}");
        assert!(error.contains("no profile was switched"), "{error}");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn rebound_before_reset_details_starts_no_network_or_cache_publication() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let reset_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_by_server = std::sync::Arc::clone(&reset_calls);
        let app = axum::Router::new().route(
            "/credits",
            axum::routing::get(move || {
                let calls = std::sync::Arc::clone(&calls_by_server);
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(serde_json::json!({"available_count": 0, "credits": []}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _credits_url =
            EnvVarGuard::set_value("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));
        let profile_path = profile::profile_auth_path("rebound").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::write(
            &profile_path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "id_token": identity_jwt("actual@example.com"),
                    "access_token": "unused-access-token",
                    "account_id": "acct-actual"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let snapshot = AutoSelectProfileSnapshot {
            path: profile_path,
            info: jwt::AccountInfo::default(),
            binding: binding("acct-expected", "expected@example.com"),
        };
        let lease = profile::acquire_profile_lease_async("rebound".to_string())
            .await
            .unwrap();

        let error = prepare_and_enrich_reset_card_details_for_test(
            collected_usage("rebound", AutoSelectUsageOrigin::CoreProbe),
            snapshot,
            lease,
            &reqwest::Client::new(),
        )
        .await
        .expect_err("a rebound alias must fail before reset-card network or cache work");
        server.abort();

        let error = format!("{error:#}");
        assert!(error.contains("changed account identity"), "{error}");
        assert!(error.contains("no reset card was requested"), "{error}");
        assert_eq!(reset_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            !home.path().join("cache.json").exists(),
            "a rebound alias must not publish its core probe to cache"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reset_details_publish_only_core_probe_usage() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let reset_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_by_server = std::sync::Arc::clone(&reset_calls);
        let app = axum::Router::new().route(
            "/credits",
            axum::routing::get(move || {
                let calls = std::sync::Arc::clone(&calls_by_server);
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "available_count": 1,
                        "credits": [{"id": "fresh-card", "status": "available"}]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _credits_url =
            EnvVarGuard::set_value("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));
        let client = reqwest::Client::new();

        let core_binding = binding("acct-core", "core@example.com");
        let core_path = profile::profile_auth_path("core").unwrap();
        std::fs::create_dir_all(core_path.parent().unwrap()).unwrap();
        std::fs::write(
            &core_path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "id_token": identity_jwt("core@example.com"),
                    "access_token": "core-access-token",
                    "account_id": "acct-core"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let core_lease = profile::acquire_profile_lease_async("core".to_string())
            .await
            .unwrap();
        let completed_core = prepare_and_enrich_reset_card_details_for_test(
            collected_usage("core", AutoSelectUsageOrigin::CoreProbe),
            AutoSelectProfileSnapshot {
                path: core_path,
                info: jwt::AccountInfo::default(),
                binding: core_binding.clone(),
            },
            core_lease,
            &client,
        )
        .await
        .unwrap();
        assert_eq!(completed_core.usage.reset_credits.len(), 1);
        assert!(completed_core.usage.cache_revision.is_some());
        let stored_core = cache::get_bound("core", &core_binding).unwrap().unwrap();
        assert_eq!(stored_core.reset_credits[0].id, "fresh-card");

        let cached_binding = binding("acct-cached", "cached@example.com");
        let cached_path = profile::profile_auth_path("cached").unwrap();
        std::fs::create_dir_all(cached_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cached_path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "id_token": identity_jwt("cached@example.com"),
                    "access_token": "cached-access-token",
                    "account_id": "acct-cached"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let cached_before = cache::put_bound_versioned(
            "cached",
            &cached_binding,
            &usage::UsageInfo {
                reset_credits_available_count: Some(1),
                reset_credits: vec![usage::ResetCredit {
                    id: "cached-card".to_string(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..usage::UsageInfo::default()
            },
        )
        .unwrap();
        let cached_revision = cached_before.cache_revision.clone();
        let cached_lease = profile::acquire_profile_lease_async("cached".to_string())
            .await
            .unwrap();
        let completed_cached = prepare_and_enrich_reset_card_details_for_test(
            AutoSelectUsage {
                alias: "cached".to_string(),
                usage: cached_before,
                origin: AutoSelectUsageOrigin::CachedComplete,
                cache_baseline: None,
            },
            AutoSelectProfileSnapshot {
                path: cached_path,
                info: jwt::AccountInfo::default(),
                binding: cached_binding.clone(),
            },
            cached_lease,
            &client,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(reset_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(completed_cached.usage.reset_credits[0].id, "fresh-card");
        let still_cached = cache::get_bound("cached", &cached_binding)
            .unwrap()
            .unwrap();
        assert_eq!(still_cached.cache_revision, cached_revision);
        assert_eq!(still_cached.reset_credits[0].id, "cached-card");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn cached_quota_prepares_expired_credentials_without_refetching_quota() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let codex_home = home.path().join("codex");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", &codex_home);

        let usage_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let token_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let credit_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let usage_calls_by_server = std::sync::Arc::clone(&usage_calls);
        let token_calls_by_server = std::sync::Arc::clone(&token_calls);
        let credit_calls_by_server = std::sync::Arc::clone(&credit_calls);
        let refreshed_id = identity_jwt("cached-expired@example.com");
        let refreshed_access = access_jwt(4_102_444_800);
        let refreshed_id_by_server = refreshed_id.clone();
        let refreshed_access_by_server = refreshed_access.clone();
        let expected_bearer = format!("Bearer {refreshed_access}");
        let app = axum::Router::new()
            .route(
                "/usage",
                axum::routing::get(move || {
                    let calls = std::sync::Arc::clone(&usage_calls_by_server);
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        axum::Json(serde_json::json!({}))
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post(move || {
                    let calls = std::sync::Arc::clone(&token_calls_by_server);
                    let id_token = refreshed_id_by_server.clone();
                    let access_token = refreshed_access_by_server.clone();
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "id_token": id_token,
                            "access_token": access_token,
                            "refresh_token": "new-refresh-token"
                        }))
                    }
                }),
            )
            .route(
                "/credits",
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let calls = std::sync::Arc::clone(&credit_calls_by_server);
                    let expected_bearer = expected_bearer.clone();
                    async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        assert_eq!(
                            headers
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok()),
                            Some(expected_bearer.as_str())
                        );
                        axum::Json(serde_json::json!({
                            "available_count": 1,
                            "credits": [{"id": "rotated-card", "status": "available"}]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set_value("CS_USAGE_URL", format!("http://{address}/usage"));
        let _token_url = EnvVarGuard::set_value("CS_TOKEN_URL", format!("http://{address}/token"));
        let _credits_url =
            EnvVarGuard::set_value("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));

        let alias = "cached_expired";
        let expected_binding = binding("acct-cached-expired", "cached-expired@example.com");
        let profile_path = profile::profile_auth_path(alias).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::write(
            &profile_path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "id_token": identity_jwt("cached-expired@example.com"),
                    "access_token": access_jwt(1),
                    "refresh_token": "old-refresh-token",
                    "account_id": "acct-cached-expired"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let cached_before = cache::put_bound_versioned(
            alias,
            &expected_binding,
            &usage::UsageInfo {
                secondary: Some(usage::WindowUsage {
                    used_percent: Some(100.0),
                    ..usage::WindowUsage::default()
                }),
                ..usage::UsageInfo::default()
            },
        )
        .unwrap();
        let cached_revision = cached_before.cache_revision.clone();
        let lease = profile::acquire_profile_lease_async(alias.to_string())
            .await
            .unwrap();

        let completed = prepare_and_enrich_reset_card_details_for_test(
            AutoSelectUsage {
                alias: alias.to_string(),
                usage: cached_before,
                origin: AutoSelectUsageOrigin::CachedComplete,
                cache_baseline: None,
            },
            AutoSelectProfileSnapshot {
                path: profile_path.clone(),
                info: jwt::AccountInfo::default(),
                binding: expected_binding.clone(),
            },
            lease,
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(usage_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(token_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(credit_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(completed.usage.reset_credits[0].id, "rotated-card");
        let persisted = auth::read_auth(&profile_path).unwrap();
        assert_eq!(
            persisted
                .pointer("/tokens/access_token")
                .and_then(serde_json::Value::as_str),
            Some(refreshed_access.as_str())
        );
        assert_eq!(
            persisted
                .pointer("/tokens/refresh_token")
                .and_then(serde_json::Value::as_str),
            Some("new-refresh-token")
        );
        let still_cached = cache::get_bound(alias, &expected_binding).unwrap().unwrap();
        assert_eq!(still_cached.cache_revision, cached_revision);
        assert!(still_cached.reset_credits.is_empty());
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn core_probe_completion_preserves_an_intervening_cache_generation() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _lock = lock_profile_test_environment();
        crate::config::init_defaults_for_tests();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let app = axum::Router::new().route(
            "/credits",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "available_count": 1,
                    "credits": [{"id": "fresh-card", "status": "available"}]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _credits_url =
            EnvVarGuard::set_value("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));
        let expected_binding = binding("acct-race", "race@example.com");
        let profile_path = profile::profile_auth_path("race").unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::write(
            &profile_path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "id_token": identity_jwt("race@example.com"),
                    "access_token": "race-access-token",
                    "account_id": "acct-race"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let stale_probe = AutoSelectUsage {
            alias: "race".to_string(),
            usage: usage::UsageInfo {
                secondary: Some(usage::WindowUsage {
                    used_percent: Some(88.0),
                    ..usage::WindowUsage::default()
                }),
                ..usage::UsageInfo::default()
            },
            origin: AutoSelectUsageOrigin::CoreProbe,
            cache_baseline: Some(
                cache::get_auto_select_usage_snapshot(&std::collections::HashMap::from([(
                    "race".to_string(),
                    expected_binding.clone(),
                )]))
                .unwrap()
                .take("race")
                .unwrap()
                .into_parts()
                .1,
            ),
        };
        assert!(
            cache::get_bound("race", &expected_binding)
                .unwrap()
                .is_none()
        );

        // Simulate a daemon/TUI publication after stage 1 released its lease
        // but before stage 2 reacquired it.
        cache::put_bound_versioned(
            "race",
            &expected_binding,
            &usage::UsageInfo {
                secondary: Some(usage::WindowUsage {
                    used_percent: Some(12.0),
                    ..usage::WindowUsage::default()
                }),
                reset_credits_available_count: Some(1),
                reset_credits: vec![usage::ResetCredit {
                    id: "older-card-metadata".to_string(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..usage::UsageInfo::default()
            },
        )
        .unwrap();
        let intervening = cache::get_bound("race", &expected_binding)
            .unwrap()
            .unwrap();
        let intervening_revision = intervening.cache_revision.clone();
        let intervening_fetched_at = intervening.fetched_at;
        let lease = profile::acquire_profile_lease_async("race".to_string())
            .await
            .unwrap();

        let completed = prepare_and_enrich_reset_card_details_for_test(
            stale_probe,
            AutoSelectProfileSnapshot {
                path: profile_path,
                info: jwt::AccountInfo::default(),
                binding: expected_binding.clone(),
            },
            lease,
            &reqwest::Client::new(),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(
            completed
                .usage
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.0)
        );
        assert_eq!(completed.usage.cache_revision, intervening_revision);
        assert_eq!(completed.usage.fetched_at, intervening_fetched_at);
        assert_eq!(completed.usage.reset_credits[0].id, "older-card-metadata");
        let stored = cache::get_bound("race", &expected_binding)
            .unwrap()
            .unwrap();
        assert_eq!(
            stored
                .secondary
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(12.0)
        );
        assert_eq!(stored.cache_revision, intervening_revision);
        assert_eq!(stored.fetched_at, intervening_fetched_at);
        assert_eq!(stored.reset_credits[0].id, "older-card-metadata");
    }

    #[test]
    fn automatic_ranking_rejects_malformed_selection_history() {
        let _lock = lock_profile_test_environment();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
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
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
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

    #[test]
    fn automatic_ranking_rejects_paid_snapshot_without_weekly_quota() {
        let _lock = lock_profile_test_environment();
        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _app_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let profile_dir = home.path().join("profiles/alice");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(profile_dir.join("auth.json"), "{}").unwrap();
        std::fs::write(home.path().join("cache.json"), r#"{"entries": {}}"#).unwrap();
        let usage = usage::UsageInfo {
            primary: Some(usage::WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(1_003_600),
                window_minutes: Some(300),
            }),
            plan_type: Some("plus".to_string()),
            ..usage::UsageInfo::default()
        };

        let error =
            score_profile_candidates(vec![("alice".to_string(), usage)], 1_000_000, 20.0, false)
                .expect_err("paid automatic selection must require the weekly window");

        assert!(
            format!("{error:#}").contains("complete authoritative quota data"),
            "{error:#}"
        );
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
        let base = usage::Candidate::from_usage(
            "boundary".to_string(),
            &usage,
            crate::jwt::PlanKind::Plus,
            0,
            1_000,
        );
        assert!(!usage::is_candidate_eligible(&base, 20.0));

        let current_info = jwt::AccountInfo {
            plan_type: Some("plus".to_string()),
            ..jwt::AccountInfo::default()
        };
        let rechecked = candidate_from_revival_usage(&base, &usage, &current_info, 1_002, true);

        assert_eq!(rechecked.now, 1_002);
        assert!(
            usage::is_candidate_eligible(&rechecked, 20.0),
            "a reset crossed while the request was in flight must be evaluated at response time"
        );
    }

    #[test]
    fn revival_recheck_renormalizes_plan_without_changing_the_weekly_contract() {
        let now = 1_000_000;
        let weekly_only = usage::UsageInfo {
            secondary: Some(usage::WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(now + usage::WINDOW_7D_SECS),
                window_minutes: Some(usage::WINDOW_7D_SECS / 60),
            }),
            ..usage::UsageInfo::default()
        };
        let base = usage::Candidate::from_usage(
            "plan-change".to_string(),
            &weekly_only,
            jwt::PlanKind::Free,
            0,
            now,
        );
        let stale_free_jwt = jwt::AccountInfo {
            plan_type: Some("free".to_string()),
            ..jwt::AccountInfo::default()
        };
        let fresh_plus_usage = usage::UsageInfo {
            plan_type: Some("plus".to_string()),
            ..weekly_only.clone()
        };

        let upgraded =
            candidate_from_revival_usage(&base, &fresh_plus_usage, &stale_free_jwt, now, false);
        assert_eq!(upgraded.plan_kind, jwt::PlanKind::Plus);
        assert!(upgraded.has_required_quota_data());

        let current_plus_jwt = jwt::AccountInfo {
            plan_type: Some("plus".to_string()),
            ..jwt::AccountInfo::default()
        };
        let fresh_free_usage = usage::UsageInfo {
            plan_type: Some("free".to_string()),
            ..weekly_only.clone()
        };
        let downgraded =
            candidate_from_revival_usage(&base, &fresh_free_usage, &current_plus_jwt, now, false);
        assert_eq!(downgraded.plan_kind, jwt::PlanKind::Free);
        assert!(downgraded.has_required_quota_data());

        let jwt_only =
            candidate_from_revival_usage(&base, &weekly_only, &current_plus_jwt, now, false);
        assert_eq!(jwt_only.plan_kind, jwt::PlanKind::Plus);
        assert!(jwt_only.has_required_quota_data());
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
            UsageCollectionOptions {
                json: true,
                max_concurrent: 2,
            },
            |alias| async move {
                if alias == "bob" {
                    anyhow::bail!("injected later cache failure");
                }
                Ok(cache::AutoSelectUsageCacheLookup::absent_for_test(alias))
            },
            |alias| Ok(std::path::PathBuf::from(alias)),
            |_alias| async { Ok(Some(())) },
            |_alias, path, lease| async move { Ok(Some((path, lease))) },
            move |_alias, _path, _lease, _network_permit| {
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
            UsageCollectionOptions {
                json: true,
                max_concurrent: 2,
            },
            |alias| async move { Ok(cache::AutoSelectUsageCacheLookup::absent_for_test(alias)) },
            |alias| {
                if alias == "bob" {
                    anyhow::bail!("injected later path failure");
                }
                Ok(std::path::PathBuf::from(alias))
            },
            |_alias| async { Ok(Some(())) },
            |_alias, path, lease| async move { Ok(Some((path, lease))) },
            move |_alias, _path, _lease, _network_permit| {
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
        let temp = crate::fs_ops::create_direct_tempdir().unwrap();
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
            UsageCollectionOptions {
                json: true,
                max_concurrent: 3,
            },
            |alias| async move { Ok(cache::AutoSelectUsageCacheLookup::absent_for_test(alias)) },
            |alias| Ok(std::path::PathBuf::from(alias)),
            |_alias| async { Ok(Some(())) },
            |_alias, path, lease| async move { Ok(Some((path, lease))) },
            move |alias, _path, _lease, _network_permit| {
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
