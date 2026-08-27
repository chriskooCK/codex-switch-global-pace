use super::render::confirm_default_no;
use crate::output::{format_local_datetime, print_json, user_println};
use crate::task_batch::{NamedTaskOutcome, batch_failure_error, drain_named_tasks};
use crate::{auth, cache, color, config, profile, usage, warmup};
use anyhow::{Context, Result};

fn reset_card_credit_from_usage(
    alias: &str,
    current: &usage::UsageInfo,
) -> Result<usage::ResetCredit> {
    if let Some(blocker) = usage::explicit_account_blocker(current) {
        anyhow::bail!(
            "{alias}: reset card cannot clear the account/workspace restriction ({blocker}); no reset card was requested"
        );
    }
    if let Some(error) = current.reset_credits_error.as_deref() {
        anyhow::bail!(
            "{alias}: reset-card details could not be verified ({error}); no reset card was requested"
        );
    }
    usage::earliest_reset_credit(&current.reset_credits)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{alias}: no available reset cards"))
}

async fn fetch_reset_card_usage_observation(
    alias: &str,
    path: &std::path::Path,
    lease: &profile::ProfileLease,
    expected_binding: Option<&crate::jwt::StrictAccountBinding>,
    client: &reqwest::Client,
) -> std::result::Result<usage::UsageObservation, usage::UsageError> {
    usage::fetch_usage_observation_force_with_existing_lease_and_client(
        alias,
        path,
        lease,
        expected_binding,
        client,
    )
    .await
}

pub(crate) async fn reset_card_cmd(alias: &str, yes: bool, json: bool) -> Result<()> {
    profile::validate_alias(alias)?;
    if json && !yes {
        anyhow::bail!("confirmation required; rerun with --yes to consume a reset card");
    }
    let path = profile::profile_auth_path(alias)?;
    if !profile::profile_exists(alias)? {
        anyhow::bail!("profile '{alias}' not found");
    }

    // Build before any credential-bearing request. One command-scoped pool is
    // reused for discovery, an interactive revalidation, and the consume POST.
    let client = auth::build_http_client().context("building reset-card HTTP client")?;
    let initial_lease = profile::acquire_profile_lease_async(alias.to_string())
        .await
        .with_context(|| format!("{alias}: acquiring profile lease for reset-card lookup"))?;
    let initial_observation =
        fetch_reset_card_usage_observation(alias, &path, &initial_lease, None, &client)
            .await
            .map_err(|error| anyhow::anyhow!("{alias}: {}", error.detail))?;
    let usage::UsageObservation {
        usage: initial_usage,
        binding: expected_binding,
    } = initial_observation;
    let credit = reset_card_credit_from_usage(alias, &initial_usage)?;

    let lease = if yes {
        // There is no consent gap to race. Keep the first lease and use the
        // same forced result as the exact-card preflight.
        usage::validate_reset_credit_preflight(alias, &initial_usage, &credit)?;
        initial_lease
    } else {
        // Never hold a credential lease while waiting for terminal input.
        // The strict identity captured above crosses this gap and is checked
        // before the post-confirmation usage request can reach the network.
        drop(initial_lease);
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
        let lease = profile::acquire_profile_lease_async(alias.to_string())
            .await
            .with_context(|| {
                format!("{alias}: acquiring profile lease for reset-card preflight")
            })?;
        let preflight = fetch_reset_card_usage_observation(
            alias,
            &path,
            &lease,
            Some(&expected_binding),
            &client,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.detail))
        .with_context(|| {
            format!("{alias}: reset-card preflight failed; no reset card was requested")
        })?
        .usage;
        usage::validate_reset_credit_preflight(alias, &preflight, &credit)?;
        lease
    };

    let result = match usage::consume_reset_credit_by_id_leased_with_client(
        alias, &path, credit, &lease, &client,
    )
    .await
    {
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
        }))?;
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
    println!(
        "Opened: {}",
        crate::safe_text::terminal_text(&dir.display().to_string())
    );
    Ok(())
}

// ── warmup ────────────────────────────────────────────────

pub(crate) async fn warmup_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    if let Some(a) = alias
        && !profile::profile_exists(a)?
    {
        anyhow::bail!("profile '{a}' not found");
    }

    let profile_accounts = match alias {
        Some(alias) => {
            let path = profile::profile_auth_path(alias)?;
            let auth_value = auth::read_auth_async(&path)
                .await
                .with_context(|| format!("loading profile '{alias}' for warmup preflight"))?;
            vec![profile::ProfileAccountSnapshot {
                alias: alias.to_string(),
                path,
                info: auth::account_info_from_auth_value(&auth_value),
            }]
        }
        None => profile::load_profile_accounts()?,
    };
    let aliases = profile_accounts
        .iter()
        .map(|account| account.alias.clone())
        .collect::<Vec<_>>();

    if aliases.is_empty() {
        if json {
            print_json(&serde_json::json!({"ok": true, "results": []}))?;
        } else {
            user_println("(no saved profiles)");
        }
        return Ok(());
    }

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(aliases.len());

    // Filter out accounts whose usage data proves an active rate-limit window.
    // A window that appears "just started" (elapsed < 5 min) likely means the previous warmup
    // ping didn't consume real quota — allow the user to retry.
    let now = auth::now_unix_secs()?;
    let mut bindings = std::collections::HashMap::with_capacity(aliases.len());
    let mut account_snapshots = std::collections::HashMap::with_capacity(aliases.len());
    for account in profile_accounts {
        let binding = account.info.strict_binding().with_context(|| {
            format!(
                "profile '{}' needs a verified account id and email before warmup",
                account.alias
            )
        })?;
        bindings.insert(account.alias.clone(), binding.clone());
        account_snapshots.insert(account.alias, (account.path, binding));
    }
    let cached_usage = cache::get_many_bound(&bindings)?;
    let mut to_warmup = Vec::new();
    for alias in &aliases {
        let usage_preflight = cached_usage.get(alias).cloned();
        let already_active = usage_preflight
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
            let (path, binding) = account_snapshots
                .get(alias)
                .cloned()
                .with_context(|| format!("profile '{alias}' is missing from warmup preflight"))?;
            to_warmup.push((alias.clone(), path, binding, usage_preflight));
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
            print_json(&serde_json::json!({"ok": true, "results": results}))?;
        }
        return Ok(());
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));
    let http_client =
        auth::build_http_client().context("building the shared warmup HTTP client")?;

    let mut had_error = false;
    let mut failures = Vec::new();

    let mut tasks = tokio::task::JoinSet::new();
    let mut task_aliases = std::collections::HashMap::new();
    for (alias, path, expected_binding, cached_usage) in to_warmup {
        let tracked_alias = alias.clone();
        let sem = semaphore.clone();
        let client = http_client.clone();
        let task = tasks.spawn(async move {
            let lease = profile::acquire_profile_lease_async(alias.clone())
                .await
                .with_context(|| format!("{alias}: failed to lock profile for warmup"))?;
            let lease = warmup::warmup_account_leased_with_client_after_usage_preflight(
                &alias,
                &path,
                lease,
                &client,
                &expected_binding,
                cached_usage,
                warmup::first_network_permit(sem),
            )
            .await?;
            drop(lease);
            Ok::<(), anyhow::Error>(())
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
                let terminal_detail = crate::safe_text::terminal_text(&detail).into_owned();
                tracing::error!(alias = %alias, error = %terminal_detail, "warmup failed");
                if json {
                    results
                        .push(serde_json::json!({"alias": &alias, "ok": false, "error": &detail}));
                } else {
                    user_println(&format!(
                        "  {} failed: {}",
                        color::error(&alias),
                        terminal_detail
                    ));
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
        print_json(&serde_json::json!({"ok": !had_error, "results": results}))?;
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
    use super::*;
    use axum::Json;
    use axum::routing::{get, post};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;
    use std::ffi::{OsStr, OsString};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: reset-card tests retain both crate-wide environment locks
            // until every request and spawned blocking worker has completed.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: the owning test still holds both environment locks while
            // restoring the process-wide variable.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn identity_jwt(account_id: &str, email: &str) -> String {
        let exp = auth::now_unix_secs().unwrap() + 86_400;
        let payload = URL_SAFE_NO_PAD.encode(
            json!({
                "exp": exp,
                "email": email,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id
                }
            })
            .to_string(),
        );
        format!("header.{payload}.signature")
    }

    fn write_test_profile(
        home: &std::path::Path,
        alias: &str,
        account_id: &str,
        email: &str,
    ) -> std::path::PathBuf {
        let path = home.join("profiles").join(alias).join("auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        auth::write_auth(
            &path,
            &json!({
                "tokens": {
                    "id_token": identity_jwt(account_id, email),
                    "access_token": "access-token"
                }
            }),
        )
        .unwrap()
        .assert_durably_published();
        path
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reset_card_yes_adopts_identity_from_one_forced_lookup_before_consuming() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        write_test_profile(home.path(), "alice", "acct-alice", "alice@example.com");

        let usage_calls = Arc::new(AtomicUsize::new(0));
        let credit_calls = Arc::new(AtomicUsize::new(0));
        let consume_calls = Arc::new(AtomicUsize::new(0));
        let usage_hits = Arc::clone(&usage_calls);
        let credit_hits = Arc::clone(&credit_calls);
        let consume_hits = Arc::clone(&consume_calls);
        let app = axum::Router::new()
            .route(
                "/usage",
                get(move || {
                    let hits = Arc::clone(&usage_hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "plan_type": "pro",
                            "rate_limit": null,
                            "credits": null,
                            "spend_control": null,
                            "additional_rate_limits": null,
                            "rate_limit_reached_type": null
                        }))
                    }
                }),
            )
            .route(
                "/credits",
                get(move || {
                    let hits = Arc::clone(&credit_hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "available_count": 1,
                            "credits": [{
                                "id": "credit-1",
                                "status": "available",
                                "expires_at": "2026-09-01T00:00:00Z"
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/consume",
                post(move || {
                    let hits = Arc::clone(&consume_hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"code": "reset", "windows_reset": 2}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));
        let _credits_url =
            EnvVarGuard::set("CS_RESET_CREDITS_URL", format!("http://{address}/credits"));
        let _consume_url = EnvVarGuard::set(
            "CS_RESET_CREDITS_CONSUME_URL",
            format!("http://{address}/consume"),
        );

        let result = reset_card_cmd("alice", true, true).await;
        server.abort();
        result.unwrap();

        assert_eq!(usage_calls.load(Ordering::SeqCst), 1);
        assert_eq!(credit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(consume_calls.load(Ordering::SeqCst), 1);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn reset_card_preflight_rejects_alias_rebound_during_consent_gap() {
        let _url_lock = auth::URL_ENV_LOCK.lock().await;
        let _profile_env_lock = profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        config::init_defaults_for_tests();

        let home = crate::fs_ops::create_direct_tempdir().unwrap();
        let _switch_home = EnvVarGuard::set("CODEX_SWITCH_HOME", home.path());
        let path = write_test_profile(home.path(), "alice", "acct-before", "before@example.com");
        let expected_binding = crate::jwt::StrictAccountBinding {
            account_id: "acct-before".to_string(),
            email: "before@example.com".to_string(),
        };

        write_test_profile(home.path(), "alice", "acct-after", "after@example.com");
        let usage_calls = Arc::new(AtomicUsize::new(0));
        let usage_hits = Arc::clone(&usage_calls);
        let app = axum::Router::new().route(
            "/usage",
            get(move || {
                let hits = Arc::clone(&usage_hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
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
        let _usage_url = EnvVarGuard::set("CS_USAGE_URL", format!("http://{address}/usage"));

        let lease = profile::acquire_profile_lease_async("alice".to_string())
            .await
            .unwrap();
        let error = fetch_reset_card_usage_observation(
            "alice",
            &path,
            &lease,
            Some(&expected_binding),
            &reqwest::Client::new(),
        )
        .await
        .expect_err("a rebound alias must be rejected before reset-card network work");
        server.abort();

        assert_eq!(error.summary, "profile identity changed");
        assert_eq!(usage_calls.load(Ordering::SeqCst), 0);
    }

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
