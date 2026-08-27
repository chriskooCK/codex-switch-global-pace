use crate::output::{self, print_json};
use crate::{color, login, profile};
use anyhow::Result;

// ── login / reauth ────────────────────────────────────────

pub(crate) async fn login_cmd(alias: Option<&str>, device: bool, json: bool) -> Result<()> {
    if let Some(a) = alias {
        profile::validate_alias(a)?;
    }

    if let Some(a) = alias
        && profile::profile_exists(a)?
    {
        return reauth_profile(a, device, json).await;
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

async fn reauth_profile(alias: &str, device: bool, json: bool) -> Result<()> {
    // Capture only the stable account identity under a short lease. The user
    // may spend several minutes in OAuth, so unrelated refresh, switch, rename,
    // and delete operations must not wait behind that interactive pause.
    let prepared = {
        let lease = profile::acquire_profile_lease_async(alias.to_string()).await?;
        profile::prepare_profile_reauth_with_lease(&lease)?
    };

    if !json {
        println!(
            "Re-authorizing profile '{}' ({})...",
            color::bold(alias),
            crate::safe_text::terminal_text(prepared.email())
        );
    }

    let tokens = if device {
        login::run_device_code_auth().await?
    } else {
        login::run_device_auth().await?
    };
    let (auth_val, new_info) = login::build_auth_from_tokens(&tokens)?;
    let lease = profile::acquire_profile_lease_async(alias.to_string()).await?;
    profile::commit_prepared_profile_reauth_with_lease(prepared, &lease, &auth_val)?;
    drop(lease);

    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: "reauthed".into(),
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
    }
    Ok(())
}
