use super::render::print_usage_line;
use crate::output::{self, ProgressReporter, account_to_json, print_json, usage_to_json};
use crate::{auth, color, profile, usage};
use anyhow::{Context, Result};

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

fn format_import_failure_line(status: &str, source: &str, stage: &str, error: &str) -> String {
    let source = crate::safe_text::terminal_text(source);
    let stage = crate::safe_text::terminal_text(stage);
    let error = crate::safe_text::terminal_text(error);
    format!("  {status} {source} [{stage}] {error}")
}

fn json_import_recovery_fields(
    recovery_path: Option<&std::path::Path>,
    cleanup_warning: Option<&str>,
) -> (Option<String>, Option<String>) {
    (
        recovery_path.map(|path| path.display().to_string()),
        cleanup_warning.map(str::to_string),
    )
}

fn validated_import_profile_commit_failure(
    source: &std::path::Path,
    action: &profile::SaveAction,
    incomplete: &profile::ImportProfileCommitIncomplete,
) -> profile::ImportFailure {
    let recovery = incomplete
        .recovery_path
        .as_ref()
        .map(|path| format!(" The exact recovery stage remains at {}.", path.display()))
        .unwrap_or_else(|| {
            " No recovery path is claimed because the original stage is no longer proven there."
                .to_string()
        });
    profile::ImportFailure {
        source: source.to_path_buf(),
        stage: STAGE_TOKEN_ROTATED,
        error: format!(
            "validated import profile '{}' became visible, but its commit is incomplete ({:#}).{} Do not retry the consumed source or use the partial profile until its state is inspected.",
            action.alias(),
            incomplete.cause,
            recovery
        ),
    }
}

fn import_cleanup_projection(
    cleanup: Option<&profile::ImportRecoveryCleanupIncomplete>,
) -> (Option<std::path::PathBuf>, Option<String>) {
    let warning = cleanup.map(|cleanup| {
        let ownership = cleanup
            .recovery_path
            .as_ref()
            .map(|path| format!("exact recovery stage: {}", path.display()))
            .unwrap_or_else(|| {
                "no recovery path is claimed because the original stage is no longer proven there"
                    .to_string()
            });
        format!("{:#}; {ownership}", cleanup.cause)
    });
    let path = cleanup.and_then(|cleanup| cleanup.recovery_path.clone());
    (path, warning)
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
    stage: Option<profile::RotationRecoveryStage>,
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
        Ok(profile::RecoveredImportAction::Profile(outcome)) => {
            if let Some(incomplete) = outcome.profile_commit.as_ref() {
                let recovery = incomplete
                    .recovery_path
                    .as_ref()
                    .map(|path| {
                        format!(
                            " The exact recovery stage remains at {}.",
                            path.display()
                        )
                    })
                    .unwrap_or_else(|| {
                        " No recovery path is claimed because the original stage is no longer proven there."
                            .to_string()
                    });
                return profile::ImportFailure {
                    source: source.to_path_buf(),
                    stage: STAGE_TOKEN_ROTATED,
                    error: format!(
                        "import failed after credential rotation ({cause}). Profile '{}' became visible, but its commit is incomplete ({:#}).{} {} now holds a dead refresh token; do not retry that source or use the partial profile until its state is inspected.",
                        outcome.action.alias(),
                        incomplete.cause,
                        recovery,
                        source.display()
                    ),
                };
            }
            let cleanup = outcome.recovery_cleanup.as_ref().map(|cleanup| {
                let path = cleanup
                    .recovery_path
                    .as_ref()
                    .map(|path| {
                        format!(
                            " The exact remaining recovery copy is at {}.",
                            path.display()
                        )
                    })
                    .unwrap_or_else(|| {
                        " No recovery path is claimed because the original stage is no longer proven there."
                            .to_string()
                    });
                format!(
                    " Recovery-stage cleanup is incomplete ({:#}).{}",
                    cleanup.cause, path
                )
            });
            profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "import failed after credential rotation ({cause}), so the usable credentials \
                     were {} as profile '{}'. {} now holds a dead refresh token — use the profile \
                     instead of importing that file again.{}",
                    outcome.action.action(),
                    outcome.action.alias(),
                    source.display(),
                    cleanup.as_deref().unwrap_or_default()
                ),
            }
        }
        Ok(profile::RecoveredImportAction::RecoveryPreserved {
            recovery_path,
            reason,
        }) => {
            let recovery = recovery_path
                .as_ref()
                .map(|path| {
                    format!(
                        "The exact usable recovery stage remains at {}.",
                        path.display()
                    )
                })
                .unwrap_or_else(|| {
                    "No recovery path is claimed because the original stage is no longer proven there."
                        .to_string()
                });
            profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "import failed after credential rotation ({cause}). Profile recovery did not \
                     complete ({reason}). {recovery} No activatable profile is assumed; keep the \
                     recovery directory private and inspect the reported state before deleting \
                     anything. {} now holds a dead refresh token.",
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
                     credentials also failed ({save_error:#}). A matching stage was observed at {} \
                     before that attempt, but its current file identity could not be confirmed. \
                     Keep the recovery directory private and inspect the exact state before \
                     deleting anything.",
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

    // Build once before processing any source. A later per-file client failure
    // must not leave earlier directory entries with consumed refresh tokens.
    let client = auth::build_http_client().context("building import-validation HTTP client")?;

    if single_file_input {
        let imported = match import_one_file(&files[0], alias, &client).await {
            Ok(imported) => imported,
            Err(failure) => anyhow::bail!("{}: {}", failure.stage, failure.error),
        };
        let now = auth::now_unix_secs()?;
        if json {
            let (recovery_path, cleanup_warning) = json_import_recovery_fields(
                imported.recovery_path.as_deref(),
                imported.cleanup_warning.as_deref(),
            );
            print_json(&output::JsonImportResult {
                ok: true,
                alias: imported.alias,
                action: imported.action.to_string(),
                recovery_path,
                cleanup_warning,
            })?;
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
            print_usage_line(&imported.usage, now);
            if let Some(warning) = imported.cleanup_warning.as_deref() {
                println!(
                    "  {} {}",
                    color::status_tag("WARN"),
                    crate::safe_text::terminal_text(warning)
                );
            }
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
        match import_one_file(&file, None, &client).await {
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
    let now = auth::now_unix_secs()?;
    if json {
        print_json(&output::JsonImportReport {
            ok: !all_skipped,
            credentials_lost,
            imported: report
                .imported
                .iter()
                .map(|item| {
                    let (recovery_path, cleanup_warning) = json_import_recovery_fields(
                        item.recovery_path.as_deref(),
                        item.cleanup_warning.as_deref(),
                    );
                    Ok(output::JsonImportEntry {
                        source: item.source.display().to_string(),
                        alias: item.alias.clone(),
                        action: item.action.to_string(),
                        account: account_to_json(&item.account, item.usage.plan_type.as_deref()),
                        usage: usage_to_json(Ok(&item.usage), now)?,
                        recovery_path,
                        cleanup_warning,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            skipped: report
                .skipped
                .iter()
                .map(|item| output::JsonImportFailure {
                    source: item.source.display().to_string(),
                    stage: item.stage.to_string(),
                    error: item.error.clone(),
                })
                .collect(),
        })?;
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
                crate::safe_text::terminal_text(&item.source.display().to_string()),
                item.alias,
                item.action
            );
            print!("    ");
            print_usage_line(&item.usage, now);
            if let Some(warning) = item.cleanup_warning.as_deref() {
                println!(
                    "    {} {}",
                    color::status_tag("WARN"),
                    crate::safe_text::terminal_text(warning)
                );
            }
        }

        for item in &report.skipped {
            let source = item.source.display().to_string();
            if rotated_credentials(item.stage) {
                let line =
                    format_import_failure_line("[Rotated]", &source, item.stage, &item.error);
                println!("{}", color::warn(&line));
            } else {
                let status = color::status_tag("Skip");
                println!(
                    "{}",
                    format_import_failure_line(&status, &source, item.stage, &item.error)
                );
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
    client: &reqwest::Client,
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
    let mut credential_reservation = Some(
        profile::reserve_import_credential_for_validation(&val).map_err(|e| {
            profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "duplicate_credential",
                error: e.to_string(),
            }
        })?,
    );
    let suggested_alias = source_account
        .email
        .as_deref()
        .map(profile::alias_from_email);

    let mut rotation_stage: Option<profile::RotationRecoveryStage> = None;
    let validation = usage::validate_import_auth_with_client(
        &mut val,
        |rotated| {
            if let Some(stage) = rotation_stage.as_mut() {
                stage.persist(rotated)
            } else {
                rotation_stage = Some(profile::stage_import_rotation(rotated)?);
                Ok(())
            }
        },
        client,
    )
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
            drop(credential_reservation.take());
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
    let account = match auth::validate_auth_value(&val) {
        Ok(account) => account,
        Err(error) if rotated => {
            drop(credential_reservation.take());
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
    let reservation = credential_reservation
        .take()
        .expect("import credential reservation must remain held through commit");
    let outcome = match profile::save_reserved_imported_auth_value_with_stage(
        &val,
        alias,
        &validated_account_id,
        suggested_alias.as_deref(),
        rotation_stage.take(),
        reservation,
    ) {
        Ok(profile::ValidatedImportCommit::Profile(outcome)) => outcome,
        Ok(profile::ValidatedImportCommit::RecoveryPreserved {
            recovery_path,
            cause,
        }) => {
            let recovery = recovery_path
                .as_ref()
                .map(|path| {
                    format!(
                        "The exact usable recovery stage remains at {}.",
                        path.display()
                    )
                })
                .unwrap_or_else(|| {
                    "No recovery path is claimed because the original stage is no longer proven there."
                        .to_string()
                });
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "validated import could not be safely committed ({cause:#}). {recovery} No activatable profile is assumed. Do not retry the consumed source; inspect the reported state first."
                ),
            });
        }
        // A validated commit failure must not be retried under another alias:
        // that could bypass the duplicate-identity or managed-policy boundary.
        // Do not infer final profile or recovery-path ownership from the path
        // captured before the commit attempt; the detailed error owns that
        // classification.
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
                    "validated import could not be safely committed ({error:#}). The recovery \
                     stage was originally observed at {}, but this wrapper does not claim that \
                     path still owns it or that no profile became visible. Do not retry the \
                     consumed source; inspect the reported filesystem state first.",
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

    if let Some(incomplete) = outcome.profile_commit.as_ref() {
        return Err(validated_import_profile_commit_failure(
            source,
            &outcome.action,
            incomplete,
        ));
    }

    let (recovery_path, cleanup_warning) =
        import_cleanup_projection(outcome.recovery_cleanup.as_ref());
    Ok(profile::ImportSuccess {
        source: source.to_path_buf(),
        alias: outcome.action.alias().to_string(),
        action: outcome.action.action(),
        account,
        usage,
        recovery_path,
        cleanup_warning,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        STAGE_TOKEN_ROTATED, format_import_failure_line, import_cleanup_projection,
        json_import_recovery_fields, validated_import_profile_commit_failure,
    };

    #[test]
    fn failure_row_preserves_trusted_ansi_after_sanitizing_untrusted_fields() {
        let status = "\u{1b}[32m[Skip]\u{1b}[39m";
        let rendered = format_import_failure_line(
            status,
            "dump\u{1b}]52;c;path\u{7}.json",
            "save\nnext",
            "server\u{1b}[31merror",
        );

        assert!(rendered.contains(status), "{rendered:?}");
        assert!(rendered.contains("dump]52;c;path.json"), "{rendered:?}");
        assert!(rendered.contains("[savenext]"), "{rendered:?}");
        assert!(rendered.contains("server[31merror"), "{rendered:?}");
        assert_eq!(rendered.matches('\u{1b}').count(), 2, "{rendered:?}");
        assert!(!rendered.contains('\n'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
    }

    #[test]
    fn cleanup_warning_and_exact_path_project_into_both_json_fields() {
        let path = std::path::PathBuf::from("recovery/rotated-import.json");
        let cleanup = crate::profile::ImportRecoveryCleanupIncomplete {
            recovery_path: Some(path.clone()),
            cause: anyhow::anyhow!("cleanup sync failed"),
        };

        let (projected_path, warning) = import_cleanup_projection(Some(&cleanup));
        assert_eq!(projected_path.as_deref(), Some(path.as_path()));
        let warning = warning.expect("cleanup failure must remain visible");
        assert!(warning.contains("cleanup sync failed"), "{warning}");
        assert!(warning.contains(&path.display().to_string()), "{warning}");

        let (json_path, json_warning) =
            json_import_recovery_fields(projected_path.as_deref(), Some(&warning));
        let path_text = path.display().to_string();
        assert_eq!(json_path.as_deref(), Some(path_text.as_str()));
        assert_eq!(json_warning.as_deref(), Some(warning.as_str()));
    }

    #[test]
    fn visible_partial_import_is_not_projected_as_profile_absence() {
        let source = std::path::Path::new("consumed-auth.json");
        let recovery = std::path::PathBuf::from("recovery/rotated-import.json");
        let incomplete = crate::profile::ImportProfileCommitIncomplete {
            recovery_path: Some(recovery.clone()),
            cause: anyhow::anyhow!("profile directory sync failed"),
        };
        let failure = validated_import_profile_commit_failure(
            source,
            &crate::profile::SaveAction::Created("alice".to_string()),
            &incomplete,
        );

        assert_eq!(failure.stage, STAGE_TOKEN_ROTATED);
        assert_eq!(failure.source, source);
        assert!(failure.error.contains("profile 'alice' became visible"));
        assert!(failure.error.contains("commit is incomplete"));
        assert!(failure.error.contains(&recovery.display().to_string()));
        assert!(failure.error.contains("Do not retry the consumed source"));
    }
}
