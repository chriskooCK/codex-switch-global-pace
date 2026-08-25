use super::render::confirm_default_no;
use crate::output::{format_local_datetime, print_json, user_println};
use crate::task_batch::{NamedTaskOutcome, batch_failure_error, drain_named_tasks};
use crate::{auth, cache, color, config, profile, usage, warmup};
use anyhow::{Context, Result};

pub(crate) async fn reset_card_cmd(alias: &str, yes: bool, json: bool) -> Result<()> {
    profile::validate_alias(alias)?;
    if json && !yes {
        anyhow::bail!("confirmation required; rerun with --yes to consume a reset card");
    }
    let path = profile::profile_auth_path(alias)?;
    if !profile::profile_exists(alias)? {
        anyhow::bail!("profile '{alias}' not found");
    }

    let usage = usage::fetch_usage_retried_force(alias, &path)
        .await
        .map_err(|e| anyhow::anyhow!("{alias}: {}", e.detail))?;
    let credit = usage::earliest_reset_credit(&usage.reset_credits)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{alias}: no available reset cards"))?;

    if !yes {
        let expires = credit
            .expires_at
            .as_deref()
            .map(format_local_datetime)
            .unwrap_or_else(|| "no expiry".to_string());
        if !confirm_default_no(&format!(
            "Use earliest reset card for '{alias}' (expires {expires})? [y/N] "
        )) {
            anyhow::bail!("aborted");
        }
    }

    let result = match usage::consume_reset_credit_by_id(alias, &path, credit).await {
        Ok(result) => result,
        Err(error) if error.outcome_unknown_after_request() => {
            if let Err(err) = cache::invalidate(alias) {
                tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
            }
            anyhow::bail!(error.user_facing_unknown_message(alias));
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(err) = cache::invalidate(alias) {
        tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
    }
    if json {
        print_json(&serde_json::json!({
            "ok": true,
            "alias": alias,
            "action": "reset-card-consumed",
            "credit_id": result.credit.id,
            "expires_at": result.credit.expires_at,
            "code": result.code,
            "windows_reset": result.windows_reset,
            "redeemed_at": result.redeemed_at,
        }));
    } else {
        println!(
            "{}",
            color::success(&format!(
                "[ok] Consumed reset card for {alias} (was expiring at {})",
                result
                    .credit
                    .expires_at
                    .as_deref()
                    .map(format_local_datetime)
                    .unwrap_or_else(|| "no expiry".to_string())
            ))
        );
        if let Some(windows_reset) = result.windows_reset {
            println!("  windows reset: {windows_reset}");
        }
    }
    Ok(())
}

// ── open ─────────────────────────────────────────────────

pub(crate) fn open_cmd() -> Result<()> {
    let dir = auth::app_home()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating directory {}", dir.display()))?;
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe")
        .arg(dir.as_os_str())
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&dir).spawn();
    result.with_context(|| format!("opening file manager for {}", dir.display()))?;
    println!("Opened: {}", dir.display());
    Ok(())
}

// ── warmup ────────────────────────────────────────────────

pub(crate) async fn warmup_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    let aliases: Vec<String> = match alias {
        Some(a) => vec![a.to_string()],
        None => profile::list_profiles()?,
    };
    if let Some(a) = alias
        && !profile::profile_exists(a)?
    {
        anyhow::bail!("profile '{}' not found", a);
    }

    if aliases.is_empty() {
        if json {
            print_json(&serde_json::json!({"ok": true, "results": []}));
        } else {
            user_println("(no saved profiles)");
        }
        return Ok(());
    }

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(aliases.len());

    // Filter out accounts whose usage data proves an active rate-limit window.
    // A window that appears "just started" (elapsed < 5 min) likely means the previous warmup
    // ping didn't consume real quota — allow the user to retry.
    let now = auth::now_unix_secs();
    let mut to_warmup = Vec::new();
    for alias in &aliases {
        let already_active = cache::get(alias)?
            .as_ref()
            .is_some_and(|u| usage::usage_has_active_warmup_window(u, now));
        if already_active {
            if json {
                results.push(serde_json::json!({"alias": alias, "ok": true, "skipped": true}));
            } else {
                user_println(&format!(
                    "  {} {}",
                    color::dim(alias),
                    color::dim("already active, skipped")
                ));
            }
        } else {
            to_warmup.push(alias.clone());
        }
    }

    if to_warmup.is_empty() {
        if json {
            results.sort_by(|a, b| {
                a["alias"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["alias"].as_str().unwrap_or(""))
            });
            print_json(&serde_json::json!({"ok": true, "results": results}));
        }
        return Ok(());
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    let mut had_error = false;
    let mut failures = Vec::new();
    let mut pending = Vec::with_capacity(to_warmup.len());
    for alias in to_warmup {
        match profile::profile_auth_path(&alias) {
            Ok(path) => pending.push((alias, path)),
            Err(error) => {
                let detail = format!("{error:#}");
                tracing::warn!("[{alias}] failed to resolve profile path: {detail}");
                if json {
                    results
                        .push(serde_json::json!({"alias": &alias, "ok": false, "error": &detail}));
                } else {
                    user_println(&format!("  {} failed: {}", color::error(&alias), detail));
                }
                failures.push((alias, detail));
                had_error = true;
            }
        }
    }

    let mut tasks = tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();
    for (alias, path) in pending {
        let tracked_alias = alias.clone();
        let sem = semaphore.clone();
        let task = tasks.spawn(async move {
            let _permit = sem.acquire_owned().await.context("warmup limiter closed")?;
            warmup::warmup_account(&alias, &path).await
        });
        let previous = task_aliases.insert(task.id(), tracked_alias);
        debug_assert!(previous.is_none());
    }

    let outcomes = drain_named_tasks(&mut tasks, &mut task_aliases, |_| {}).await;
    for outcome in outcomes {
        let (alias, result) = match outcome {
            NamedTaskOutcome::Completed { alias, value } => (alias, value),
            NamedTaskOutcome::Failed { alias, detail } => (alias, Err(anyhow::anyhow!(detail))),
        };
        match result {
            Ok(()) => {
                if json {
                    results.push(serde_json::json!({"alias": alias, "ok": true}));
                } else {
                    user_println(&format!(
                        "  {} {}",
                        color::success(&alias),
                        color::dim("warmed up")
                    ));
                }
            }
            Err(error) => {
                let detail = format!("{error:#}");
                tracing::error!(alias = %alias, error = %detail, "warmup failed");
                if json {
                    results
                        .push(serde_json::json!({"alias": &alias, "ok": false, "error": &detail}));
                } else {
                    user_println(&format!("  {} failed: {}", color::error(&alias), detail));
                }
                failures.push((alias, detail));
                had_error = true;
            }
        }
    }

    if json {
        results.sort_by(|a, b| {
            a["alias"]
                .as_str()
                .unwrap_or("")
                .cmp(b["alias"].as_str().unwrap_or(""))
        });
        // Embed overall status in JSON so callers get a single valid object.
        print_json(&serde_json::json!({"ok": !had_error, "results": results}));
        if had_error {
            return Err(crate::OutputAlreadyReported.into());
        }
    } else if had_error {
        return Err(batch_failure_error(
            "one or more warmup operations failed",
            failures,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn warmup_rejects_path_traversal_alias_at_the_command_boundary() {
        let error = super::warmup_cmd(Some("../outside"), false)
            .await
            .expect_err("a traversal alias must be rejected before resolving its path");
        assert_eq!(
            error.to_string(),
            "alias may only contain ASCII letters, digits, '_', '-', '.'"
        );
    }

    #[tokio::test]
    async fn warmup_rejects_absolute_alias_at_the_command_boundary() {
        let absolute = if cfg!(windows) {
            r"C:\outside"
        } else {
            "/tmp/outside"
        };
        let error = super::warmup_cmd(Some(absolute), false)
            .await
            .expect_err("an absolute alias must be rejected before resolving its path");
        assert_eq!(
            error.to_string(),
            "alias may only contain ASCII letters, digits, '_', '-', '.'"
        );
    }
}
