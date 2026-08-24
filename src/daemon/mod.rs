pub mod codex_process;
pub mod loop_runner;
pub mod notify;
pub mod pidfile;
pub mod service;
pub mod state;

use crate::cli::DaemonCommand;
use crate::output::{print_json, user_println};
use anyhow::{Context, Result};

pub async fn dispatch(cmd: DaemonCommand, json: bool) -> Result<()> {
    match cmd {
        DaemonCommand::Start {
            foreground,
            expected_executable,
        } => start(foreground, expected_executable).await,
        DaemonCommand::Stop {
            expected_service_executable,
        } => stop(expected_service_executable),
        DaemonCommand::Status { installer_state } => {
            if installer_state {
                print_installer_state(json)
            } else {
                status(json)
            }
        }
        DaemonCommand::Install {
            expected_existing_executable,
        } => service::install(expected_existing_executable),
        DaemonCommand::Uninstall {
            expected_executable,
            check_owner,
        } => uninstall(expected_executable, check_owner),
    }
}

fn uninstall(expected_executable: Option<std::path::PathBuf>, check_owner: bool) -> Result<()> {
    let expected_executable = validated_uninstall_owner(expected_executable)?;
    if check_owner {
        return Ok(());
    }
    let service_lease = service::acquire_service_operation_lease()?;
    // The read-only preflight may have raced a different service command.
    // Re-prove ownership while holding the service-operation lease before any
    // daemon process state is changed.
    service::validate_uninstall_owner(&expected_executable)?;
    let previous_daemon_running = pidfile::running_pid_checked()?.is_some();

    #[cfg(target_os = "windows")]
    {
        let uninstall_result = (|| {
            // Task Scheduler's `/End` is a forced stop. If its daemon is live,
            // give the process the same generation-bound graceful request used by
            // `daemon stop` before removing the task.
            if previous_daemon_running {
                stop_detached()?;
            } else {
                pidfile::cleanup_pidfile()?;
            }
            // Re-check immediately before `service::uninstall_locked()` reaches
            // Task Scheduler's `/End`. Only checked PID-lock absence authorizes
            // the service layer to reach that force-stop boundary.
            if let Some(pid) = pidfile::running_pid_checked()? {
                anyhow::bail!(
                    "Daemon PID {pid} still owns the PID lock after the graceful stop request; \
                     refusing to force-terminate it during uninstall"
                );
            }
            service::uninstall_locked(
                &expected_executable,
                previous_daemon_running,
                &service_lease,
            )
        })();
        if let Err(error) = uninstall_result {
            let restoration = service::restore_uninstall_running_state_locked(
                &expected_executable,
                previous_daemon_running,
                &service_lease,
            );
            return match restoration {
                Ok(()) => Err(error.context(
                    "Windows scheduled-task uninstall failed; the prior daemon running state was restored",
                )),
                Err(restoration_error) => Err(error.context(format!(
                    "Windows scheduled-task uninstall failed and prior daemon running-state restoration was incomplete: {restoration_error:#}"
                ))),
            };
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    service::uninstall_locked(
        &expected_executable,
        previous_daemon_running,
        &service_lease,
    )
}

fn validated_uninstall_owner(
    expected_executable: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf> {
    let expected_executable = match expected_executable {
        Some(path) => path,
        None => std::env::current_exe().context("locating the daemon executable for uninstall")?,
    };
    service::validate_expected_executable(&expected_executable)?;
    service::validate_uninstall_owner(&expected_executable)?;
    Ok(expected_executable)
}

pub(crate) fn check_uninstall_owner(expected_executable: Option<std::path::PathBuf>) -> Result<()> {
    validated_uninstall_owner(expected_executable).map(|_| ())
}

async fn start(foreground: bool, expected_executable: Option<std::path::PathBuf>) -> Result<()> {
    if let Some(expected_executable) = expected_executable.as_deref() {
        if foreground {
            anyhow::bail!("--expected-executable cannot be combined with --foreground");
        }
        #[cfg(not(target_os = "windows"))]
        anyhow::bail!("--expected-executable is supported only on Windows");
        #[cfg(target_os = "windows")]
        return start_windows_installer_owned(expected_executable);
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = foreground;
        anyhow::bail!("The background daemon is not supported on this platform.");
    }
    #[cfg(any(unix, target_os = "windows"))]
    {
        if let Some(pid) = pidfile::running_pid_checked()? {
            anyhow::bail!("Daemon is already running (PID {pid})");
        }
        // Clean up stale PID file before starting
        pidfile::cleanup_pidfile()?;
        if foreground {
            return run_foreground().await;
        }
        if service::is_installed_checked()? {
            return service::start_installed();
        }
        start_detached()
    }
}

async fn run_foreground() -> Result<()> {
    pidfile::write_pidfile_exclusive()?;
    // RAII guard ensures PID file is cleaned up even on panic
    let _guard = pidfile::PidGuard;
    tracing::info!(
        "codex-switch-global-pace daemon started (PID {})",
        std::process::id()
    );
    loop_runner::run_daemon_loop().await
}

fn start_detached() -> Result<()> {
    let exe = std::env::current_exe()?;
    start_detached_executable(&exe)
}

fn start_detached_executable(exe: &std::path::Path) -> Result<()> {
    let mut child = std::process::Command::new(exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let pid = await_daemon_ready(&mut child, STARTUP_TIMEOUT)?;
    user_println(&format!("Daemon started (PID {pid})"));
    Ok(())
}

#[cfg(target_os = "windows")]
fn start_windows_installer_owned(expected_executable: &std::path::Path) -> Result<()> {
    service::validate_expected_executable(expected_executable)?;
    let service_lease = service::acquire_service_operation_lease()?;
    if pidfile::running_pid_checked()?.is_some() {
        anyhow::bail!("Daemon is already running");
    }
    pidfile::cleanup_pidfile()?;
    if service::is_installed_checked()? {
        service::start_installed_locked(expected_executable, &service_lease)
    } else {
        start_detached_executable(expected_executable)
    }
}

/// How long a freshly spawned daemon gets to publish its PID file.
///
/// Generous on purpose: the wait below returns the moment the file appears, so
/// the only thing a large value costs is how long a genuinely broken start
/// takes to be reported. A tight bound, on the other hand, turns a cold binary
/// on a slow disk — a fresh self-update, an on-access virus scan, a loaded CI
/// runner — into a spurious "start failed".
const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits for the daemon to write its PID file, which signals it reached the
/// event loop. Polling the actual readiness signal is more reliable than a
/// fixed sleep on slow disks / CI / containers.
///
/// A child that never gets there is killed rather than left running. It is
/// spawned detached, so abandoning it would report a failed start while an
/// initializing daemon is still on its way — leaving the user with a process
/// they were told does not exist, and a retry that refuses with "already
/// running". Nothing is lost by killing it: not having written the PID file is
/// exactly what says it has not begun touching credentials yet.
fn await_daemon_ready(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Result<u32> {
    let pid = child.id();
    let deadline = std::time::Instant::now() + timeout;
    let mut last_probe_error = None;
    loop {
        // Did the child exit before initializing?
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "Daemon process (PID {pid}) exited immediately ({status}); check logs for details"
            );
        }
        match pidfile::running_pid_checked() {
            Ok(Some(running_pid)) if running_pid == pid => return Ok(pid),
            Ok(Some(running_pid)) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "Daemon startup PID {pid} lost the PID-lock authority to PID {running_pid}; the new child was stopped"
                );
            }
            Ok(None) => {}
            Err(error) => last_probe_error = Some(error),
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(error) = last_probe_error {
                return Err(error.context(format!(
                    "Daemon (PID {pid}) did not publish a valid, locked identity within {}s and was stopped",
                    timeout.as_secs()
                )));
            }
            anyhow::bail!(
                "Daemon (PID {pid}) did not initialize within {}s (no locked PID identity published) and was stopped; check logs",
                timeout.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn stop(expected_service_executable: Option<std::path::PathBuf>) -> Result<()> {
    if let Some(expected_executable) = expected_service_executable.as_deref() {
        #[cfg(not(target_os = "windows"))]
        anyhow::bail!("--expected-service-executable is supported only on Windows");
        #[cfg(target_os = "windows")]
        return stop_windows_installer_owned(expected_executable);
    }

    #[cfg(target_os = "windows")]
    {
        // A scheduled task runs the same foreground daemon. Ask that process
        // to unwind first; `/End` would terminate it during credential writes.
        if pidfile::running_pid_checked()?.is_some() {
            return stop_detached();
        }
        pidfile::cleanup_pidfile()?;
        // An older or still-starting scheduled task may not have a trusted
        // pidfile. There is no generation-bound process to signal, so Task
        // Scheduler is the only remaining stop authority.
        if service::is_installed_checked()? {
            service::stop_installed()?;
            pidfile::cleanup_pidfile()?;
            return Ok(());
        }
        stop_detached()
    }

    #[cfg(not(target_os = "windows"))]
    {
        if service::is_installed_checked()? {
            let pid = pidfile::running_pid_checked()?;
            service::stop_installed()?;
            wait_until_stopped(pid)?;
            pidfile::cleanup_pidfile()?;
            return Ok(());
        }

        stop_detached()
    }
}

#[cfg(target_os = "windows")]
fn stop_windows_installer_owned(expected_executable: &std::path::Path) -> Result<()> {
    service::validate_expected_executable(expected_executable)?;
    let service_lease = service::acquire_service_operation_lease()?;
    service::validate_uninstall_owner(expected_executable)?;
    let service_installed = service::is_installed_checked()?;
    let was_running = pidfile::running_pid_checked()?.is_some();

    let stop_result = (|| {
        if was_running {
            stop_detached()?;
        } else {
            pidfile::cleanup_pidfile()?;
        }
        if service_installed {
            service::stop_installed_locked(expected_executable, &service_lease)?;
            pidfile::cleanup_pidfile()?;
        }
        if let Some(pid) = pidfile::running_pid_checked()? {
            anyhow::bail!(
                "Daemon PID {pid} still owns the PID lock after the installer stop boundary"
            );
        }
        Ok(())
    })();
    if let Err(error) = stop_result {
        let restoration = (|| {
            let running = pidfile::running_pid_checked()?.is_some();
            if was_running && !running {
                if service_installed {
                    service::start_installed_locked(expected_executable, &service_lease)?;
                } else {
                    start_detached_executable(expected_executable)?;
                }
            } else if !was_running && running {
                anyhow::bail!("daemon started while the installer stop boundary was failing");
            }
            if pidfile::running_pid_checked()?.is_some() != was_running {
                anyhow::bail!("daemon running state did not match its pre-stop value");
            }
            Ok(())
        })();
        return match restoration {
            Ok(()) => Err(error.context("installer stop failed; prior daemon state was restored")),
            Err(restoration_error) => Err(error.context(format!(
                "installer stop failed and prior daemon state restoration was incomplete: {restoration_error:#}"
            ))),
        };
    }
    Ok(())
}

fn stop_detached() -> Result<()> {
    let observed_pid = pidfile::read_pidfile();
    let Some(pid) = pidfile::running_pid_checked()? else {
        if observed_pid.is_none() {
            anyhow::bail!("No daemon PID file found; daemon may not be running");
        }
        pidfile::cleanup_pidfile()?;
        user_println("Daemon was not running (stale PID file cleaned up)");
        return Ok(());
    };
    #[cfg(target_os = "windows")]
    pidfile::request_shutdown(pid)?;
    #[cfg(not(target_os = "windows"))]
    pidfile::send_sigterm(pid)?;
    wait_until_stopped(Some(pid)).map_err(|err| {
        anyhow::anyhow!(
            "{err}. The daemon may still be finishing an in-flight credential rotation; \
             refusing to force-terminate it. Retry `codex-switch-global-pace daemon stop` shortly."
        )
    })?;
    pidfile::cleanup_pidfile()?;
    user_println(&format!("Stopped daemon (PID {pid})"));
    Ok(())
}

fn wait_until_stopped(pid: Option<u32>) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match pidfile::running_pid_checked()? {
            None => return Ok(()),
            Some(current_pid) if pid.is_none() || pid == Some(current_pid) => {}
            Some(current_pid) => anyhow::bail!(
                "Daemon generation changed to PID {current_pid} while waiting for PID {:?} to stop",
                pid
            ),
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("Daemon did not stop within 10s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub struct SelfUpdateDaemonRestart {
    pid: Option<u32>,
    service_installed: bool,
    stopped: bool,
}

impl SelfUpdateDaemonRestart {
    pub fn capture() -> Result<Self> {
        let pid = pidfile::running_pid_checked()?;
        let service_installed = if pid.is_some() {
            service::is_installed_checked()?
        } else {
            false
        };
        Ok(Self {
            pid,
            service_installed,
            stopped: false,
        })
    }

    pub fn is_needed(&self) -> bool {
        self.pid.is_some()
    }

    pub fn stop_before_update(&mut self) -> Result<()> {
        if !self.is_needed() || self.stopped {
            return Ok(());
        }

        user_println("Stopping daemon before self-update...");
        if self.service_installed && !cfg!(target_os = "windows") {
            service::stop_installed()?;
            wait_until_stopped(self.pid)?;
            pidfile::cleanup_pidfile()?;
        } else {
            stop_detached()?;
        }
        self.stopped = true;
        Ok(())
    }

    pub fn restart_after_update(&mut self) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }

        user_println("Restarting daemon after self-update...");
        if self.service_installed {
            service::start_installed()?;
        } else {
            start_detached()?;
        }
        self.stopped = false;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    pub fn stop_failed_restart_before_rollback(&self) -> Result<()> {
        if !self.stopped {
            anyhow::bail!("daemon restart state was already committed; refusing binary rollback");
        }
        if self.service_installed {
            service::stop_failed_start_for_self_update()?;
        } else {
            // `start_detached` owns the child and kills it before returning an
            // initialization error. The PID lock is the final absence proof.
            pidfile::cleanup_pidfile()?;
        }
        Ok(())
    }
}

pub(crate) fn print_installer_state(json: bool) -> Result<()> {
    if json {
        anyhow::bail!("--installer-state cannot be combined with a JSON output mode");
    }
    let running = pidfile::running_pid_checked()?.is_some();
    let service_installed = service::is_installed_checked()?;
    println!("{}", installer_state_line(running, service_installed));
    Ok(())
}

fn installer_state_line(running: bool, service_installed: bool) -> &'static str {
    match (running, service_installed) {
        (true, true) => "running=true service_installed=true",
        (true, false) => "running=true service_installed=false",
        (false, true) => "running=false service_installed=true",
        (false, false) => "running=false service_installed=false",
    }
}

fn status(json: bool) -> Result<()> {
    let pidfile = pidfile::pidfile_path()?;
    let running_pid = pidfile::running_pid_checked()?;
    let running = running_pid.is_some();
    let pid = running_pid.or_else(pidfile::read_pidfile);
    let state = match (pid, running) {
        (Some(_), true) => "running",
        (Some(_), false) => "stale",
        (None, _) => "stopped",
    };

    // Loop-written snapshot; only meaningful while the daemon is running.
    let snapshot = if running { state::read() } else { None };

    if json {
        let service_installed = service::is_installed_checked()?;
        let cfg = crate::config::get();
        print_json(&serde_json::json!({
            "running": running,
            "state": state,
            "pid": pid,
            "pidfile": pidfile,
            "stale_pid_cleaned": state == "stale",
            "snapshot": snapshot,
            "platform": {
                "os": std::env::consts::OS,
                "daemon_start_supported": cfg!(any(unix, target_os = "windows")),
                "service_install_supported": cfg!(any(target_os = "macos", target_os = "linux", target_os = "windows")),
                "service_manager": service_manager_name(),
                "service_installed": service_installed,
            },
            "config": {
                "poll_interval_secs": cfg.daemon.poll_interval_secs,
                "cache_refresh_interval_secs": cfg.daemon.cache_refresh_interval_secs,
                "auto_warmup": cfg.daemon.auto_warmup,
                "token_check_interval_secs": cfg.daemon.token_check_interval_secs,
                "switch_threshold": cfg.daemon.switch_threshold,
                "notify": cfg.daemon.notify,
                "log_level": cfg.daemon.log_level,
            }
        }));
        if state == "stale" {
            pidfile::cleanup_pidfile()?;
        }
        return Ok(());
    }

    #[cfg(any(unix, target_os = "windows"))]
    {
        match (pid, running) {
            (Some(pid), true) => {
                user_println(&format!("Daemon is running (PID {pid})"));
                if let Some(snap) = &snapshot {
                    if let Some(at) = snap.last_poll_at {
                        user_println(&format!("  Last poll: {}", format_unix(at)));
                    }
                    if let Some(sw) = &snap.last_switch {
                        user_println(&format!(
                            "  Last switch: '{}' -> '{}' at {} (score {:.0})",
                            sw.from,
                            sw.to,
                            format_unix(sw.at),
                            sw.score
                        ));
                    }
                    if let Some(p) = &snap.pending_switch {
                        user_println(&format!(
                            "  Pending switch to '{}' since {} (waiting for Codex session to end)",
                            p.to,
                            format_unix(p.since)
                        ));
                    }
                    if let Some(err) = &snap.last_error {
                        user_println(&format!(
                            "  Last error ({} consecutive): {err}",
                            snap.consecutive_failures
                        ));
                    }
                    // Repeated failures back polling off by up to sixteen
                    // intervals. Without this line the daemon reads as healthy
                    // while it is deliberately idle, so someone who has just
                    // fixed the cause has no way to tell how long the fix will
                    // take to show up — or that restarting would apply it now.
                    if let Some(until) = snap.backoff_until {
                        let remaining = until - crate::auth::now_unix_secs();
                        if remaining > 0 {
                            user_println(&format!(
                                "  Polling suspended for another {remaining}s (until {}) after \
                                 repeated failures; `daemon stop` then `daemon start` resumes it \
                                 immediately",
                                format_unix(until)
                            ));
                        }
                    }
                }
            }
            (Some(pid), false) => {
                user_println(&format!("Daemon is not running (stale PID {pid})"));
                pidfile::cleanup_pidfile()?;
            }
            (None, _) => {
                user_println("Daemon is not running");
            }
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        user_println(&format!(
            "Daemon is not supported on this platform ({})",
            std::env::consts::OS
        ));
    }
    Ok(())
}

#[cfg(any(unix, target_os = "windows"))]
fn format_unix(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn service_manager_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "linux")]
    {
        "systemd-user"
    }
    #[cfg(target_os = "windows")]
    {
        "task-scheduler"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "unsupported"
    }
}

#[cfg(test)]
mod installer_state_tests {
    use super::installer_state_line;

    #[test]
    fn installer_state_has_only_four_exact_single_line_payloads() {
        assert_eq!(
            installer_state_line(true, true),
            "running=true service_installed=true"
        );
        assert_eq!(
            installer_state_line(true, false),
            "running=true service_installed=false"
        );
        assert_eq!(
            installer_state_line(false, true),
            "running=false service_installed=true"
        );
        assert_eq!(
            installer_state_line(false, false),
            "running=false service_installed=false"
        );
        for output in [
            installer_state_line(true, true),
            installer_state_line(true, false),
            installer_state_line(false, true),
            installer_state_line(false, false),
        ] {
            assert!(!output.contains(['\r', '\n']));
        }
    }

    #[test]
    fn installer_state_rejects_json_before_any_state_probe() {
        let error = super::print_installer_state(true)
            .expect_err("installer state must not enter either JSON output mode");
        assert_eq!(
            error.to_string(),
            "--installer-state cannot be combined with a JSON output mode"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    /// A `daemon start` that reports failure must leave no daemon behind. The
    /// child is spawned detached, so abandoning it on timeout hands the user a
    /// process they were just told does not exist — and a second
    /// `daemon start` that then refuses with "already running".
    #[test]
    fn a_daemon_that_never_signals_readiness_is_killed_not_abandoned() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os("CODEX_SWITCH_HOME");
        // SAFETY: the process-wide env lock above is held for the whole test.
        unsafe { std::env::set_var("CODEX_SWITCH_HOME", home.path()) };

        // Stands in for a daemon that starts but never reaches the event loop:
        // it stays alive and writes no PID file into the empty home above.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn stand-in daemon");
        let pid = child.id();

        let err = super::await_daemon_ready(&mut child, Duration::from_millis(200))
            .expect_err("no PID file is ever written, so readiness cannot be reached");

        // SAFETY: same held lock.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }

        assert!(
            err.to_string().contains("did not initialize"),
            "unexpected error: {err}"
        );
        // The PID file is intentionally absent in this fixture. Reaping the
        // child is the direct evidence — it can only have been killed and
        // waited for.
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "the daemon reported as failed is still running as PID {pid}"
        );
    }
}
