use crate::output::{self, print_json};
use crate::{color, login, profile};
use anyhow::Result;
use std::io::IsTerminal as _;

// ── login / reauth ────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum IncompleteReauthConfirmation {
    NotRequired,
    Confirmed,
    Prompt,
}

fn incomplete_reauth_confirmation(
    alias: &str,
    required: bool,
    yes: bool,
    interactive: bool,
) -> Result<IncompleteReauthConfirmation> {
    if !required {
        return Ok(IncompleteReauthConfirmation::NotRequired);
    }
    if yes {
        return Ok(IncompleteReauthConfirmation::Confirmed);
    }
    if !interactive {
        anyhow::bail!(
            "profile '{alias}' has incomplete legacy account identity; rerun `codex-switch-global-pace login {alias} --yes` to archive its previous credentials before replacement"
        );
    }
    Ok(IncompleteReauthConfirmation::Prompt)
}

pub(crate) async fn login_cmd(
    alias: Option<&str>,
    device: bool,
    yes: bool,
    json: bool,
) -> Result<()> {
    if let Some(a) = alias {
        profile::validate_alias(a)?;
    }

    if let Some(a) = alias
        && profile::profile_exists(a)?
    {
        return reauth_profile(a, device, yes, json).await;
    }

    let tokens = if device {
        login::run_device_code_auth().await?
    } else {
        login::run_device_auth().await?
    };
    let (auth_val, _info) = login::build_auth_from_tokens(&tokens)?;
    let action = profile::save_auth_value(auth_val, alias)?;
    match action {
        profile::SaveAction::Created(a) => {
            if !json {
                println!(
                    "{}",
                    color::success(&format!("[ok] Logged in -- saved as new profile: {a}"))
                );
            }
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a,
                    action: "created".into(),
                })?;
            }
        }
        profile::SaveAction::Updated(a) => {
            if !json {
                println!(
                    "{}",
                    color::success(&format!("[ok] Logged in -- updated existing profile: {a}"))
                );
            }
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a,
                    action: "updated".into(),
                })?;
            }
        }
    }
    Ok(())
}

async fn reauth_profile(alias: &str, device: bool, yes: bool, json: bool) -> Result<()> {
    // Capture only the stable account identity under a short lease. The user
    // may spend several minutes in OAuth, so unrelated refresh, switch, rename,
    // and delete operations must not wait behind that interactive pause.
    let prepared = {
        let lease = profile::acquire_profile_lease_async(alias.to_string()).await?;
        profile::prepare_profile_reauth_with_lease(&lease)?
    };

    let recover_incomplete = prepared.requires_recoverable_replacement();
    let confirmation = incomplete_reauth_confirmation(
        alias,
        recover_incomplete,
        yes,
        !json && std::io::stdin().is_terminal(),
    )?;
    if confirmation == IncompleteReauthConfirmation::Prompt {
        let prompt = format!(
            "Profile '{alias}' has incomplete legacy account identity. Archive its current credentials recoverably, then replace it with this re-login? [y/N] "
        );
        if !crate::commands::confirm_default_no(&prompt) {
            return Err(crate::error::CsError::Aborted.into());
        }
    }

    if !json {
        println!(
            "Re-authorizing profile '{}' ({})...",
            color::bold(alias),
            crate::safe_text::terminal_text(prepared.email().unwrap_or("identity unavailable"))
        );
    }

    let tokens = if device {
        login::run_device_code_auth().await?
    } else {
        login::run_device_auth().await?
    };
    let (auth_val, new_info) = login::build_auth_from_tokens(&tokens)?;
    let lease = profile::acquire_profile_lease_async(alias.to_string()).await?;
    let outcome = profile::commit_prepared_profile_reauth_with_lease(
        prepared,
        &lease,
        &auth_val,
        recover_incomplete,
    )?;
    drop(lease);

    if json {
        print_json(&output::JsonReauth {
            ok: true,
            alias: alias.to_string(),
            action: "reauthed".into(),
            archive_path: outcome
                .archive_path()
                .map(|path| path.display().to_string()),
        })?;
    } else {
        println!(
            "{}",
            color::success(&format!(
                "[ok] Profile '{}' re-authorized (account: {})",
                alias,
                new_info.email.as_deref().unwrap_or("unknown")
            ))
        );
        if let Some(path) = outcome.archive_path() {
            let archive_path = path.display().to_string();
            let archive_path = crate::safe_text::terminal_text(&archive_path);
            println!(
                "{}",
                color::dim(&format!(
                    "Previous incomplete credentials archived for recovery at {}",
                    archive_path
                ))
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{IncompleteReauthConfirmation, incomplete_reauth_confirmation};

    #[test]
    fn incomplete_reauth_json_and_non_tty_policy_stops_before_oauth_without_yes() {
        // JSON mode and a non-terminal stdin both enter this non-interactive
        // policy branch before either OAuth implementation is selected.
        let error = incomplete_reauth_confirmation("legacy", true, false, false)
            .expect_err("non-interactive recovery must require an explicit flag");
        assert!(
            error
                .to_string()
                .contains("codex-switch-global-pace login legacy --yes"),
            "{error:#}"
        );
    }

    #[test]
    fn incomplete_reauth_policy_distinguishes_prompt_flag_and_complete_profile() {
        assert_eq!(
            incomplete_reauth_confirmation("legacy", true, false, true).unwrap(),
            IncompleteReauthConfirmation::Prompt
        );
        assert_eq!(
            incomplete_reauth_confirmation("legacy", true, true, false).unwrap(),
            IncompleteReauthConfirmation::Confirmed
        );
        assert_eq!(
            incomplete_reauth_confirmation("legacy", false, false, false).unwrap(),
            IncompleteReauthConfirmation::NotRequired
        );
    }

    #[test]
    fn reauth_json_exposes_only_a_real_incomplete_profile_archive() {
        let archived = serde_json::to_value(crate::output::JsonReauth {
            ok: true,
            alias: "legacy".to_string(),
            action: "reauthed".to_string(),
            archive_path: Some("deleted-profiles/legacy.backup-1".to_string()),
        })
        .unwrap();
        assert_eq!(archived["archive_path"], "deleted-profiles/legacy.backup-1");

        let strict = serde_json::to_value(crate::output::JsonReauth {
            ok: true,
            alias: "complete".to_string(),
            action: "reauthed".to_string(),
            archive_path: None,
        })
        .unwrap();
        assert!(strict.get("archive_path").is_none());
    }
}
