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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptedSelfUpdateRecoveryStep {
    StopReplacement,
    RollbackExecutable,
    RestorePriorDaemon,
    ReleaseCommittedReplacement,
}

fn interrupted_self_update_recovery_plan(
    replacement_running: bool,
    replacement_state: update::SelfUpdateReplacementState,
    final_state_verified: bool,
    boundary_finished: bool,
) -> Vec<InterruptedSelfUpdateRecoveryStep> {
    if boundary_finished {
        return Vec::new();
    }
    if matches!(
        replacement_state,
        update::SelfUpdateReplacementState::Committed
            | update::SelfUpdateReplacementState::Preserved
    ) {
        return final_state_verified
            .then_some(InterruptedSelfUpdateRecoveryStep::ReleaseCommittedReplacement)
            .into_iter()
            .collect();
    }
    let mut plan = Vec::with_capacity(3);
    if replacement_running {
        plan.push(InterruptedSelfUpdateRecoveryStep::StopReplacement);
    }
    if replacement_state == update::SelfUpdateReplacementState::Pending {
        plan.push(InterruptedSelfUpdateRecoveryStep::RollbackExecutable);
    }
    plan.push(InterruptedSelfUpdateRecoveryStep::RestorePriorDaemon);
    plan
}

fn recover_interrupted_self_update(
    boundary: &mut daemon::SelfUpdateDaemonBoundaryClient,
    result: &mut update::SelfUpdateResult,
) -> Result<()> {
    if boundary.transition_is_ambiguous() {
        anyhow::bail!(
            "the lifecycle control channel unwound during a phase transition; no executable or process mutation was guessed, and the independent holder retained final-state classification authority"
        );
    }
    let plan = interrupted_self_update_recovery_plan(
        boundary.replacement_is_running(),
        result.replacement_state(),
        boundary.replacement_is_finally_verified(),
        boundary.is_finished(),
    );
    if plan.is_empty() && !boundary.is_finished() {
        anyhow::bail!(
            "the executable replacement is {:?}, but the independent holder has no matching verified release or rollback boundary; no prior-state claim was made",
            result.replacement_state()
        );
    }
    for step in plan {
        match step {
            InterruptedSelfUpdateRecoveryStep::StopReplacement => {
                boundary.stop_replacement_for_rollback()?
            }
            InterruptedSelfUpdateRecoveryStep::RollbackExecutable => result
                .rollback_replacement()
                .context("rolling back the executable after self-update interruption")?,
            InterruptedSelfUpdateRecoveryStep::RestorePriorDaemon => boundary
                .restore_prior()
                .context("restoring the exact prior daemon after self-update interruption")?,
            InterruptedSelfUpdateRecoveryStep::ReleaseCommittedReplacement => boundary
                .release_verified_replacement()
                .context("releasing the verified committed replacement after interruption")?,
        }
    }
    Ok(())
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

fn finish_self_update_result_inner(
    daemon_boundary: &mut daemon::SelfUpdateDaemonBoundaryClient,
    result: &mut update::SelfUpdateResult,
) -> Result<()> {
    let restart = match daemon_boundary.restart_replacement() {
        Ok(restart) => restart,
        Err(boundary_error) if result.updated => {
            let recovery = result
                .preserve_replacement_for_recovery()
                .context("preserving the previous executable after lifecycle-holder failure")?;
            return Err(boundary_error.context(format!(
                "the executable was replaced, but the independent holder did not return an exact restart classification; automatic rollback was refused. {}",
                format_recovery_paths(&recovery)
            )));
        }
        Err(boundary_error) => {
            return Err(boundary_error.context(
                "the no-op self-update lifecycle holder did not return an exact restart classification",
            ));
        }
    };

    if restart == daemon::SelfUpdateBoundaryRestart::FailedStopped {
        if result.updated {
            result.rollback_replacement().context(
                "the replacement daemon failed to start safely, and restoring the exact previous executable failed while daemon absence remained held",
            )?;
        }
        return match daemon_boundary.restore_prior() {
            Ok(()) => Err(anyhow::anyhow!(
                "self-update daemon restart failed; the exact previous executable and prior daemon state were restored"
            )),
            Err(restoration_error) => Err(restoration_error.context(
                "self-update daemon restart failed; the previous executable was restored, but exact prior daemon-state restoration was not confirmed",
            )),
        };
    }

    // Verify while recovery material still exists. `finish` retains both
    // lifecycle authorities; only a later `release` lets the holder exit.
    if let Err(verification_error) = daemon_boundary.verify_replacement_before_commit() {
        if result.replacement_state() == update::SelfUpdateReplacementState::Pending {
            let recovery = result.preserve_replacement_for_recovery().context(
                "preserving executable recovery material after final-state verification failed",
            )?;
            return Err(verification_error.context(format_recovery_paths(&recovery)));
        }
        return Err(verification_error);
    }

    let commit_result = result
        .commit_replacement()
        .context("committing the verified executable replacement");
    let commit_state = result.replacement_state();
    let recovery_result = if commit_result.is_err() {
        result
            .preserve_failed_commit_for_recovery()
            .context("retaining exact recovery paths after executable commit failed")
    } else {
        Ok(None)
    };
    let replacement_state = result.replacement_state();
    let release_result = daemon_boundary
        .release_verified_replacement()
        .context("releasing the independently verified daemon boundary after executable commit");
    match commit_result {
        Ok(()) => match (recovery_result, release_result) {
            (Ok(None), Ok(())) => Ok(()),
            (Ok(None), Err(release_error)) => Err(release_error.context(format!(
                "the executable reached replacement state {replacement_state:?}; lifecycle authority release was not confirmed"
            ))),
            (Ok(Some(paths)), release) => {
                let release = release
                    .err()
                    .map(|error| format!("; lifecycle authority release also failed: {error:#}"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "self-update retained recovery material despite a successful executable commit. {}{release}",
                    format_recovery_paths(&paths)
                )
            }
            (Err(recovery_error), release) => {
                let release = release
                    .err()
                    .map(|error| format!("; lifecycle authority release also failed: {error:#}"))
                    .unwrap_or_default();
                Err(recovery_error.context(format!(
                    "a successful executable commit unexpectedly entered recovery classification{release}"
                )))
            }
        },
        Err(commit_error) => {
            let recovery = match recovery_result {
                Ok(Some(paths)) => format!(". {}", format_recovery_paths(&paths)),
                Ok(None) if commit_state == update::SelfUpdateReplacementState::Committed => {
                    ". The replacement was committed and its final daemon state was verified, but cleanup durability was not fully confirmed".to_string()
                }
                Ok(None) => format!(
                    ". No executable recovery entries exist for exact replacement state {commit_state:?}"
                ),
                Err(recovery_error) => format!(
                    ". Exact executable recovery paths could not be retained: {recovery_error:#}"
                ),
            };
            let release = release_result
                .err()
                .map(|error| format!("; lifecycle authority release also failed: {error:#}"))
                .unwrap_or_default();
            Err(commit_error.context(format!(
                "executable commit failed from replacement state {commit_state:?} and ended in state {replacement_state:?}{recovery}{release}"
            )))
        }
    }
}

fn format_recovery_paths(paths: &update::ReplacementRecoveryPaths) -> String {
    format!("manual recovery observation: {}", paths.describe())
}

async fn catch_async_unwind<F>(future: F) -> std::thread::Result<F::Output>
where
    F: std::future::Future,
{
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|context| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            future.as_mut().poll(context)
        })) {
            Ok(std::task::Poll::Ready(value)) => std::task::Poll::Ready(Ok(value)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => std::task::Poll::Ready(Err(payload)),
        }
    })
    .await
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
                    update::detect_install_source()?.as_str().to_string(),
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
            })?;
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

    // Capture the cache timestamp before cleanup, daemon, or executable state
    // can change. Recording a successful commit below is then infallible and
    // cannot reverse the outcome of an already-completed update.
    let self_update_record_time = update::capture_self_update_record_time()
        .context("reading the system clock before self-update mutation")?;
    update::recover_pending_self_update_cleanup_on_startup().context(
        "a previous Windows self-update cleanup remains pending; exact recovery must succeed before another executable publication",
    )?;

    let show_progress = !json && update::should_show_download_progress();
    // Serialize the complete daemon snapshot/stop/replace/restart transaction.
    // Acquiring this after stopping the daemon would let a second updater wait
    // on the lock while the service remains unnecessarily offline.
    let update_lease = update::acquire_self_update_lease()
        .context("acquiring exclusive self-update transaction")?;
    // The ownership marker can change between preflight and lock acquisition.
    // Revalidate it under the same lease that protects the replacement.
    ensure_system_install_migrated(use_dev, version, json)?;
    // A separate holder process owns the service-operation and PID-absence
    // leases across the async network work. If this command future is
    // cancelled, closing its control pipe makes the holder restore the exact
    // prior state before it releases either lifecycle authority.
    let mut daemon_boundary = daemon::SelfUpdateDaemonBoundaryClient::start()
        .context("establishing the independent daemon boundary before self-update")?;
    let mut result_slot = None;
    // Start the unwind boundary before the first network await. Publication
    // itself is guarded inside `update`; after publication there are no async
    // suspension points before the typed result is installed in `result_slot`.
    let transaction = catch_async_unwind(async {
        let update_result = if use_dev {
            update::self_update_dev(show_progress, update_lease.clone()).await
        } else {
            update::self_update(version, show_progress, update_lease.clone()).await
        };
        let result = match update_result {
            Ok(result) => result,
            Err(update_error) => {
                return match daemon_boundary.restore_prior() {
                    Ok(()) => Err(update_error.context(
                        "self-update failed; the independent holder restored the exact prior daemon state",
                    )),
                    Err(restoration_error) => Err(update_error.context(format!(
                        "self-update failed and the independent holder could not confirm exact prior daemon-state restoration: {restoration_error:#}"
                    ))),
                };
            }
        };
        result_slot = Some(result);
        finish_self_update_result_inner(
            &mut daemon_boundary,
            result_slot
                .as_mut()
                .context("self-update coordinator lost its replacement result")?,
        )
    })
    .await;
    match transaction {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(payload) => {
            let panic_message = panic_payload_message(payload.as_ref()).to_string();
            let recovery = match result_slot.as_mut() {
                Some(result) => recover_interrupted_self_update(&mut daemon_boundary, result),
                None => daemon_boundary.restore_prior().context(
                    "restoring the exact prior daemon after a pre-result self-update panic",
                ),
            };
            return match recovery {
                Ok(()) => Err(anyhow::anyhow!(
                    "self-update transaction panicked ({panic_message}); its typed executable state and daemon lifecycle boundary were recovered"
                )),
                Err(recovery_error) => Err(anyhow::anyhow!(
                    "self-update transaction panicked ({panic_message}), and exact interruption recovery was incomplete: {recovery_error:#}"
                )),
            };
        }
    }
    let result = result_slot.context("self-update coordinator completed without a result")?;
    update::record_successful_self_update(&result, self_update_record_time);

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
        })?;
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
        #[cfg(windows)]
        output::user_println(&color::dim(
            "Previous executable cleanup is journaled and will finish after this updater exits; a later startup retries it if needed.",
        ));
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

#[cfg(test)]
mod tests {
    use super::{InterruptedSelfUpdateRecoveryStep as Step, interrupted_self_update_recovery_plan};
    use crate::update::SelfUpdateReplacementState as State;

    #[test]
    fn interrupted_new_generation_is_stopped_before_file_rollback_and_prior_restore() {
        assert_eq!(
            interrupted_self_update_recovery_plan(true, State::Pending, false, false),
            vec![
                Step::StopReplacement,
                Step::RollbackExecutable,
                Step::RestorePriorDaemon,
            ]
        );
        assert_eq!(
            interrupted_self_update_recovery_plan(false, State::Pending, false, false),
            vec![Step::RollbackExecutable, Step::RestorePriorDaemon]
        );
        assert_eq!(
            interrupted_self_update_recovery_plan(true, State::NotReplaced, false, false),
            vec![Step::StopReplacement, Step::RestorePriorDaemon]
        );
        assert!(
            interrupted_self_update_recovery_plan(true, State::Committed, true, true).is_empty()
        );
        assert_eq!(
            interrupted_self_update_recovery_plan(true, State::Committed, true, false),
            vec![Step::ReleaseCommittedReplacement],
            "a committed binary must never be rolled back or described as the prior executable"
        );
        assert!(
            interrupted_self_update_recovery_plan(true, State::Committed, false, false).is_empty(),
            "an unverified committed phase must fail closed instead of guessing a rollback"
        );
    }
}
