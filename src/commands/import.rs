use super::render::print_usage_line;
use crate::output::{self, ProgressReporter, account_to_json, print_json, usage_to_json};
use crate::{auth, cache, color, profile, usage};
use anyhow::Result;

/// Validation failed, but the auth server had already rotated the credentials
/// and they were rescued into a profile.
const STAGE_TOKEN_ROTATED: &str = "token_rotated";
/// Same rotation, but nothing could be written — the account is lost unless the
/// user acts.
const STAGE_TOKEN_ROTATION_LOST: &str = "token_rotation_lost";

/// Whether a failure also consumed the account's single-use `refresh_token`.
///
/// These entries look like any other line in a directory report, yet they mean
/// the source file is now worthless and a profile may have appeared — so they
/// get their own marker instead of blending into the skip list.
fn rotated_credentials(stage: &str) -> bool {
    stage == STAGE_TOKEN_ROTATED || stage == STAGE_TOKEN_ROTATION_LOST
}

/// Promote or preserve the durable stage written immediately after the auth
/// server rotated credentials when a later import step fails.
///
/// They go to the profile store rather than back to the source file: it is the
/// tool's own storage, so it stays writable when the imported dump is not (auth
/// dumps are routinely copied in read-only), and it is where a successful
/// import would have put them. Validation never completed, so recovery never
/// overwrites an existing identity: it creates a unique profile when safe and
/// otherwise retains the exact credential in the private recovery directory.
/// The source file keeps the consumed token either way — that is unavoidable
/// once the server has rotated it — so the message has to steer the user away
/// from re-importing it.
fn rescue_rotated_credentials(
    source: &std::path::Path,
    val: serde_json::Value,
    alias: Option<&str>,
    suggested_alias: Option<&str>,
    stage: Option<profile::ImportRotationStage>,
    cause: &anyhow::Error,
) -> profile::ImportFailure {
    let staged_path = stage.as_ref().and_then(|stage| {
        stage
            .contains(&val)
            .ok()
            .filter(|contains_latest| *contains_latest)
            .map(|_| stage.path().to_path_buf())
    });
    match profile::save_recovered_import_auth_value_with_stage(val, alias, suggested_alias, stage) {
        Ok(profile::RecoveredImportAction::Profile(action)) => profile::ImportFailure {
            source: source.to_path_buf(),
            stage: STAGE_TOKEN_ROTATED,
            error: format!(
                "import failed after credential rotation ({cause}), so the usable credentials \
                 were {} as profile '{}'. {} now holds a dead refresh token — use the profile \
                 instead of importing that file again.",
                action.action(),
                action.alias(),
                source.display()
            ),
        },
        Ok(profile::RecoveredImportAction::Quarantined { path, reason }) => {
            profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "import failed after credential rotation ({cause}). Identity/policy \
                     validation or profile persistence also failed ({reason}), so the only usable \
                     credential copy was quarantined at {} and was not made into an activatable \
                     profile. Keep that file private and sign in again before deleting it. {} now \
                     holds a dead refresh token.",
                    path.display(),
                    source.display()
                ),
            }
        }
        Err(save_error) => match staged_path {
            Some(path) => profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "import failed after credential rotation ({cause}), and promoting the staged \
                     credentials also failed ({save_error:#}). The usable credentials remain at \
                     {}. Keep that file private and sign in again before deleting it.",
                    path.display()
                ),
            },
            None => profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATION_LOST,
                error: format!(
                    "import failed after the auth server rotated this account's credentials \
                     ({cause}), and {}",
                    unsaveable_rotation_reason(&save_error)
                ),
            },
        },
    }
}

fn unsaveable_rotation_reason(save_error: &anyhow::Error) -> String {
    format!(
        "saving them failed ({save_error:#}). The previous refresh token is already invalidated, \
         so this account has to sign in again."
    )
}

// ── import ───────────────────────────────────────────────

pub(crate) async fn import_cmd(path: &str, alias: Option<&str>, json: bool) -> Result<()> {
    let input = std::path::PathBuf::from(path);
    let files = profile::collect_import_files(&input)?;
    let single_file_input = files.len() == 1 && files[0] == input;

    if !single_file_input {
        if let Some(alias) = alias {
            anyhow::bail!(
                "alias '{alias}' can only be used when importing a single file, not a directory"
            );
        }
        if files.is_empty() {
            anyhow::bail!("no JSON files found under {}", input.display());
        }
    }

    if single_file_input {
        let imported = match import_one_file(&files[0], alias).await {
            Ok(imported) => imported,
            Err(failure) => anyhow::bail!("{}: {}", failure.stage, failure.error),
        };
        if json {
            print_json(&output::JsonOk {
                ok: true,
                alias: imported.alias,
                action: imported.action.to_string(),
            });
        } else {
            println!(
                "{}",
                color::success(&format!(
                    "Validated and {}: {} -> profile '{}'",
                    imported.action,
                    imported.source.display(),
                    imported.alias
                ))
            );
            print!("  ");
            print_usage_line(&imported.usage);
        }
        return Ok(());
    }

    let mut report = profile::ImportReport::default();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Validating auth files", files.len()))
    };

    for (idx, file) in files.into_iter().enumerate() {
        match import_one_file(&file, None).await {
            Ok(success) => report.imported.push(success),
            Err(failure) => report.skipped.push(failure),
        }
        if let Some(progress) = progress.as_mut() {
            progress.advance(idx + 1);
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let all_skipped = report.imported.is_empty();
    let credentials_lost = report
        .skipped
        .iter()
        .any(|item| item.stage == STAGE_TOKEN_ROTATION_LOST);
    if json {
        print_json(&output::JsonImportReport {
            ok: !all_skipped,
            credentials_lost,
            imported: report
                .imported
                .iter()
                .map(|item| output::JsonImportEntry {
                    source: item.source.display().to_string(),
                    alias: item.alias.clone(),
                    action: item.action.to_string(),
                    account: account_to_json(&item.account, item.usage.plan_type.as_deref()),
                    usage: usage_to_json(Ok(&item.usage)),
                })
                .collect(),
            skipped: report
                .skipped
                .iter()
                .map(|item| output::JsonImportFailure {
                    source: item.source.display().to_string(),
                    stage: item.stage.to_string(),
                    error: item.error.clone(),
                })
                .collect(),
        });
        if all_skipped {
            return Err(super::super::OutputAlreadyReported.into());
        }
    } else {
        println!(
            "{}",
            color::success(&format!(
                "Imported {} profile(s); skipped {} file(s)",
                report.imported.len(),
                report.skipped.len()
            ))
        );

        for item in &report.imported {
            println!(
                "  {} {} -> {} ({})",
                color::status_tag("OK"),
                item.source.display(),
                item.alias,
                item.action
            );
            print!("    ");
            print_usage_line(&item.usage);
        }

        for item in &report.skipped {
            let line = format!(
                "  {} {} [{}] {}",
                color::status_tag(if rotated_credentials(item.stage) {
                    "Rotated"
                } else {
                    "Skip"
                }),
                item.source.display(),
                item.stage,
                item.error
            );
            if rotated_credentials(item.stage) {
                println!("{}", color::warn(&line));
            } else {
                println!("{line}");
            }
        }

        let rotated = report
            .skipped
            .iter()
            .filter(|item| rotated_credentials(item.stage))
            .count();
        if rotated > 0 {
            println!(
                "{}",
                color::warn(&format!(
                    "  {rotated} file(s) had their credentials rotated during validation; their \
                     refresh token is spent and importing those files again will fail."
                ))
            );
        }

        if all_skipped {
            anyhow::bail!(
                "no profiles imported; all {} files were skipped",
                report.skipped.len()
            );
        }
    }
    Ok(())
}

async fn import_one_file(
    source: &std::path::Path,
    alias: Option<&str>,
) -> std::result::Result<profile::ImportSuccess, profile::ImportFailure> {
    let mut val = auth::read_auth(source).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "file_format",
        error: e.to_string(),
    })?;

    let source_account = auth::validate_auth_value(&val).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "structure",
        error: e.to_string(),
    })?;
    if let Some(alias) = alias {
        profile::validate_alias(alias).map_err(|e| profile::ImportFailure {
            source: source.to_path_buf(),
            stage: "alias",
            error: e.to_string(),
        })?;
    }
    auth::validate_managed_auth_value(&val).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "managed_policy",
        error: e.to_string(),
    })?;
    let suggested_alias = source_account
        .email
        .as_deref()
        .map(profile::alias_from_email);

    let mut rotation_stage: Option<profile::ImportRotationStage> = None;
    let validation = usage::validate_import_auth(&mut val, |rotated| {
        if let Some(stage) = rotation_stage.as_mut() {
            stage.persist(rotated)
        } else {
            rotation_stage = Some(profile::stage_import_rotation(rotated)?);
            Ok(())
        }
    })
    .await;
    let usage::ImportValidation {
        refreshed,
        validated_account_id,
        result,
    } = validation;
    let rotated = refreshed.is_some();
    let usage = match result {
        Ok(usage) => usage,
        // The rotation callback already staged the only credentials the auth
        // server still accepts. Hand that stage to recovery before reporting
        // the later validation failure.
        Err(error) if rotated => {
            return Err(rescue_rotated_credentials(
                source,
                val,
                alias,
                suggested_alias.as_deref(),
                rotation_stage.take(),
                &error,
            ));
        }
        Err(error) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "usage_validation",
                error: error.to_string(),
            });
        }
    };

    // This second structure check inspects the *refreshed* value, so a
    // malformed refresh reply fails it at a point where the source file's
    // token is already spent. The durable stage must be promoted or preserved,
    // exactly as above.
    let mut account = match auth::validate_auth_value(&val) {
        Ok(account) => account,
        Err(error) if rotated => {
            return Err(rescue_rotated_credentials(
                source,
                val,
                alias,
                suggested_alias.as_deref(),
                rotation_stage.take(),
                &error,
            ));
        }
        Err(error) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "structure",
                error: error.to_string(),
            });
        }
    };
    cache::apply_workspace_name(&mut account);

    let validated_account_id = validated_account_id.ok_or_else(|| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "usage_validation",
        error: "Usage API validation did not bind an account_id".to_string(),
    })?;
    let staged_path = match (rotated, rotation_stage.as_ref()) {
        (true, Some(stage)) => Some(stage.path().to_path_buf()),
        (true, None) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATION_LOST,
                error: "usage validation completed with rotated credentials but no durable import stage"
                    .to_string(),
            });
        }
        (false, _) => None,
    };
    let action = match profile::save_imported_auth_value_with_stage(
        &val,
        alias,
        &validated_account_id,
        suggested_alias.as_deref(),
        rotation_stage.take(),
    ) {
        Ok(action) => action,
        // A validated commit failure must not be retried under another alias:
        // that could bypass the duplicate-identity or managed-policy boundary.
        // The exact staged file stays in recovery and is the durable copy.
        Err(error) if rotated => {
            let Some(recovery) = staged_path.as_deref() else {
                return Err(profile::ImportFailure {
                    source: source.to_path_buf(),
                    stage: STAGE_TOKEN_ROTATION_LOST,
                    error: "validated rotated credentials lost their durable import stage"
                        .to_string(),
                });
            };
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "validated import could not be committed ({error:#}); the rotated credential \
                     copy was quarantined at {} and was not made into an activatable profile. \
                     Keep that file private and sign in again before deleting it.",
                    recovery.display()
                ),
            });
        }
        Err(error) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "save",
                error: error.to_string(),
            });
        }
    };

    Ok(profile::ImportSuccess {
        source: source.to_path_buf(),
        alias: action.alias().to_string(),
        action: action.action(),
        account,
        usage,
    })
}
