use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::profile;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum PreparePass {
    Initial,
    AfterLiveSync,
}

impl PreparePass {
    fn phase(self) -> &'static str {
        match self {
            Self::Initial => "prepare",
            Self::AfterLiveSync => "revalidate",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PhaseTiming {
    elapsed: Duration,
    lease_wait: Duration,
}

impl PhaseTiming {
    fn since(started: Instant, lease_wait: Duration) -> Self {
        Self {
            elapsed: started.elapsed(),
            lease_wait,
        }
    }
}

pub(super) enum TaskResult {
    Prepared {
        result: Result<profile::PreparedProfileSwitch>,
        pass: PreparePass,
        timing: PhaseTiming,
    },
    LiveSynchronized {
        result: Result<()>,
        timing: PhaseTiming,
    },
    Committed {
        result: Result<profile::ProfileSwitchOutcome>,
        timing: PhaseTiming,
    },
}

impl TaskResult {
    pub(super) fn record_timing(&self, alias: &str) {
        let (phase, timing) = match self {
            Self::Prepared { pass, timing, .. } => (pass.phase(), timing),
            Self::LiveSynchronized { timing, .. } => ("sync-live", timing),
            Self::Committed { timing, .. } => ("commit", timing),
        };
        tracing::debug!(
            alias,
            phase,
            elapsed_ms = timing.elapsed.as_millis(),
            profile_lease_wait_ms = timing.lease_wait.as_millis(),
            "profile switch phase finished"
        );
    }
}

pub(super) async fn prepare(
    alias: String,
    pass: PreparePass,
    lease_control: profile::ProfileLeaseAcquireControl,
) -> Option<(String, TaskResult)> {
    let started = Instant::now();
    let lease_started = Instant::now();
    let lease =
        match profile::acquire_profile_lease_async_cancellable(alias.clone(), &lease_control).await
        {
            Ok(Some(lease)) => lease,
            Ok(None) => return None,
            Err(error) => {
                let timing = PhaseTiming::since(started, lease_started.elapsed());
                return Some((
                    alias.clone(),
                    TaskResult::Prepared {
                        result: Err(error.context(format!(
                            "acquiring profile lease before preparing switch to '{alias}'"
                        ))),
                        pass,
                        timing,
                    },
                ));
            }
        };
    let lease_wait = lease_started.elapsed();
    let result = match tokio::task::spawn_blocking(move || {
        profile::prepare_profile_switch_with_lease(&lease)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!(
            "profile switch preparation worker stopped: {}",
            crate::task_batch::join_failure_detail(&error)
        )),
    };
    let timing = PhaseTiming::since(started, lease_wait);
    Some((
        alias,
        TaskResult::Prepared {
            result,
            pass,
            timing,
        },
    ))
}

pub(super) async fn synchronize_live(
    target_alias: String,
    lease_control: profile::ProfileLeaseAcquireControl,
    background_leases: Vec<(String, profile::ProfileLeaseAcquireControl)>,
) -> (String, TaskResult) {
    let started = Instant::now();
    let mut lease_wait = Duration::ZERO;
    let result = async {
        let active_alias = tokio::task::spawn_blocking(profile::active_profile_from_live)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "current-login identification worker stopped: {}",
                    crate::task_batch::join_failure_detail(&error)
                )
            })??
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "current Codex login is not saved; switch stopped without overwriting it"
                )
            })?;
        for (_, control) in background_leases
            .iter()
            .filter(|(alias, _)| alias == &active_alias)
        {
            control.cancel_waiting();
        }
        let lease_started = Instant::now();
        let lease_result =
            profile::acquire_profile_lease_async_cancellable(active_alias.clone(), &lease_control)
                .await;
        lease_wait = lease_started.elapsed();
        let lease = lease_result
            .with_context(|| {
                format!("acquiring profile lease before synchronizing '{active_alias}'")
            })?
            .ok_or_else(|| anyhow::anyhow!("live-credential synchronization was cancelled"))?;
        tokio::task::spawn_blocking(move || {
            profile::synchronize_profile_from_live_for_switch_leased(&lease)
        })
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "live-credential synchronization worker stopped: {}",
                crate::task_batch::join_failure_detail(&error)
            )
        })?
        .with_context(|| {
            format!("synchronizing live credentials with profile '{active_alias}' before switching")
        })
    }
    .await;
    let timing = PhaseTiming::since(started, lease_wait);
    (
        target_alias,
        TaskResult::LiveSynchronized { result, timing },
    )
}

pub(super) async fn commit(
    confirmed: profile::ConfirmedProfileSwitch,
    lease_control: profile::ProfileLeaseAcquireControl,
) -> Option<(String, TaskResult)> {
    let alias = confirmed.alias().to_string();
    let started = Instant::now();
    let lease_started = Instant::now();
    let lease =
        match profile::acquire_profile_lease_async_cancellable(alias.clone(), &lease_control).await
        {
            Ok(Some(lease)) => lease,
            Ok(None) => return None,
            Err(error) => {
                let timing = PhaseTiming::since(started, lease_started.elapsed());
                return Some((
                    alias.clone(),
                    TaskResult::Committed {
                        result: Err(error.context(format!(
                            "acquiring profile lease before committing switch to '{alias}'"
                        ))),
                        timing,
                    },
                ));
            }
        };
    let lease_wait = lease_started.elapsed();
    let result = match tokio::task::spawn_blocking(move || {
        profile::commit_confirmed_profile_switch_with_lease(confirmed, &lease)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!(
            "profile switch commit worker stopped: {}",
            crate::task_batch::join_failure_detail(&error)
        )),
    };
    let timing = PhaseTiming::since(started, lease_wait);
    Some((alias, TaskResult::Committed { result, timing }))
}
