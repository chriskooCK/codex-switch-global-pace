use crate::output::{self, print_json};
use crate::{color, daemon, update};
use anyhow::{Context, Result};

fn ensure_system_install_migrated(use_dev: bool, version: Option<&str>, json: bool) -> Result<()> {
    if let Err(error) = update::ensure_legacy_system_install_migrated(use_dev, version) {
        if !json
            && error
                .downcast_ref::<update::LegacySystemInstallMigrationRequired>()
                .is_some()
        {
            output::user_println(&color::warn(&error.to_string()));
            return Err(crate::OutputAlreadyReported.into());
        }
        return Err(error);
    }
    Ok(())
}

// ── self-update ──────────────────────────────────────────

pub(crate) async fn self_update_cmd(
    check: bool,
    version: Option<&str>,
    dev: bool,
    stable: bool,
    json: bool,
) -> Result<()> {
    // Resolve the effective channel:
    // --dev → dev, --stable → stable, otherwise auto-detect from current version.
    let use_dev = if dev {
        true
    } else if stable || version.is_some() {
        false
    } else {
        update::is_dev_version(update::current_version())
    };

    // Preserve the migration-specific guidance before any network or lock error.
    ensure_system_install_migrated(use_dev, version, json)?;

    if check {
        let current_version = update::current_version().to_string();
        let result = if use_dev {
            update::check_for_dev_update().await?
        } else {
            update::check_for_update(true).await?
        };

        if json {
            let (latest_version, update_available, install_source) = match &result {
                Some(info) => (
                    info.latest_version.clone(),
                    true,
                    info.install_source.as_str().to_string(),
                ),
                None => (
                    current_version.clone(),
                    false,
                    update::detect_install_source().as_str().to_string(),
                ),
            };
            print_json(&output::JsonSelfUpdate {
                ok: true,
                current_version,
                latest_version,
                update_available,
                updated: false,
                install_source,
                action: "checked".into(),
            });
            return Ok(());
        }

        let channel_label = if use_dev { " (dev)" } else { "" };
        match result {
            Some(info) => {
                let homebrew_to_dev = use_dev
                    && info.install_source == update::InstallSource::Homebrew
                    && !update::is_dev_version(&info.current_version);
                let instruction = if homebrew_to_dev {
                    format!("To switch to dev, {}.", update::homebrew_dev_install_hint())
                } else {
                    let hint = if use_dev && dev {
                        // Explicit --dev flag: include it in the hint.
                        "codex-switch-global-pace self-update --dev"
                    } else if use_dev {
                        // Already on dev (auto-detected): plain self-update stays in dev.
                        "codex-switch-global-pace self-update"
                    } else if stable {
                        "codex-switch-global-pace self-update --stable"
                    } else {
                        info.install_source.upgrade_hint()
                    };
                    format!("Run `{hint}`.")
                };
                println!(
                    "{}",
                    color::warn(&format!(
                        "New version available{channel_label}: v{} (current v{}). {instruction}",
                        info.latest_version, info.current_version
                    ))
                );
            }
            None => {
                println!(
                    "{}",
                    color::success(&format!(
                        "Already up to date{channel_label}: v{}",
                        update::current_version()
                    ))
                );
            }
        }
        return Ok(());
    }

    let show_progress = !json && update::should_show_download_progress();
    // Serialize the complete daemon snapshot/stop/replace/restart transaction.
    // Acquiring this after stopping the daemon would let a second updater wait
    // on the lock while the service remains unnecessarily offline.
    let update_lease = update::acquire_self_update_lease()
        .context("acquiring exclusive self-update transaction")?;
    // The ownership marker can change between preflight and lock acquisition.
    // Revalidate it under the same lease that protects the replacement.
    ensure_system_install_migrated(use_dev, version, json)?;
    let mut daemon_restart = daemon::SelfUpdateDaemonRestart::capture()
        .context("capturing daemon state before self-update")?;
    if daemon_restart.is_needed() {
        daemon_restart.stop_before_update()?;
    }
    let update_result = if use_dev {
        update::self_update_dev(show_progress, update_lease.clone()).await
    } else {
        update::self_update(version, show_progress, update_lease.clone()).await
    };
    let result = match update_result {
        Ok(mut result) => {
            if let Err(restart_err) = daemon_restart.restart_after_update() {
                #[cfg(target_os = "windows")]
                if result.updated {
                    if let Err(stop_err) = daemon_restart.stop_failed_restart_before_rollback() {
                        let recovery = result
                            .preserve_replacement_for_recovery()
                            .context("preserving the previous executable for manual recovery")?;
                        return Err(restart_err.context(format!(
                            "self-update installed the new binary, but its daemon did not restart; the new daemon could not be proven stopped, so automatic rollback was refused: {stop_err}. Manual recovery paths: current executable {}, previous executable backup {}",
                            recovery.executable.display(),
                            recovery.previous_executable.display()
                        )));
                    }
                    if let Err(rollback_err) = result.rollback_replacement() {
                        return Err(restart_err.context(format!(
                            "self-update installed the new binary, but its daemon did not restart and restoring the previous binary failed: {rollback_err}"
                        )));
                    }
                    if let Err(old_restart_err) = daemon_restart.restart_after_update() {
                        return Err(restart_err.context(format!(
                            "self-update daemon restart failed; the previous binary was restored, but its daemon also could not be restarted: {old_restart_err}"
                        )));
                    }
                    return Err(restart_err.context(
                        "self-update daemon restart failed; the previous binary and daemon state were restored",
                    ));
                }
                return Err(restart_err.context("self-update completed, but daemon restart failed"));
            }
            result.commit_replacement();
            result
        }
        Err(err) => {
            if let Err(restart_err) = daemon_restart.restart_after_update() {
                return Err(err.context(format!(
                    "self-update failed; additionally failed to restart daemon: {restart_err}"
                )));
            }
            return Err(err);
        }
    };

    if json {
        print_json(&output::JsonSelfUpdate {
            ok: true,
            current_version: result.current_version.clone(),
            latest_version: result.latest_version.clone(),
            update_available: result.updated,
            updated: result.updated,
            install_source: result.install_source.as_str().to_string(),
            action: if result.updated {
                "updated".into()
            } else {
                "up_to_date".into()
            },
        });
        return Ok(());
    }

    if result.updated {
        let channel_label = if use_dev { " (dev)" } else { "" };
        println!(
            "{}",
            color::success(&format!(
                "Updated codex-switch-global-pace{channel_label}: v{} -> v{}",
                result.current_version, result.latest_version
            ))
        );
        if dev && !update::is_dev_version(&result.current_version) {
            output::user_println(&color::dim(
                "Switched to dev channel. Run `codex-switch-global-pace self-update --stable` to return.",
            ));
        } else if stable && update::is_dev_version(&result.current_version) {
            output::user_println(&color::dim("Switched back to stable channel."));
        }
    } else {
        println!(
            "{}",
            color::success(&format!("Already up to date: v{}", result.current_version))
        );
    }

    Ok(())
}
