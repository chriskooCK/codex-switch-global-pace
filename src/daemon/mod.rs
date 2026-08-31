pub mod codex_process;
pub mod loop_runner;
pub mod notify;
pub mod pidfile;
pub mod service;
pub mod state;

use crate::cli::DaemonCommand;
use crate::output::{print_json, user_println};
use anyhow::{Context, Result};

const INSTALLER_DAEMON_BOUNDARY_PREFIX: &str = "codex-switch-global-pace daemon update boundary";
const DAEMON_BOUNDARY_NEW_READY: &str = "new state ready";
const DAEMON_BOUNDARY_NEW_FAILED: &str = "new state failed";
const DAEMON_BOUNDARY_NEW_STOPPED: &str = "new state stopped";
const DAEMON_BOUNDARY_OLD_RESTORED: &str = "old state restored";
const DAEMON_BOUNDARY_OLD_FAILED: &str = "old state failed";
const DAEMON_BOUNDARY_FINAL_CONFIRMED: &str = "final state confirmed";
const DAEMON_BOUNDARY_AUTHORITY_RELEASED: &str = "lifecycle authority released";
const STATUS_LAST_ERROR_MAX_CHARS: usize = 512;
pub(crate) const DAEMON_TRANSITION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);
const GENERATION_SETTLE_DIAGNOSTIC_INTERVAL: std::time::Duration = DAEMON_TRANSITION_TIMEOUT;

enum InstallerContenderResolution<Lease> {
    Absence(Lease),
    Published(pidfile::DaemonGeneration),
}

struct InstallerContenderSettlement<Lease> {
    resolution: InstallerContenderResolution<Lease>,
    diagnostics: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelfUpdateBoundaryRestart {
    Ready,
    FailedStopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelfUpdateBoundaryClientPhase {
    Stopped,
    ReplacementTransition,
    NewReady,
    AbortTransition,
    ReplacementFailedStopped,
    RollbackTransition,
    FinishTransition,
    FinalConfirmed,
    ReleaseTransition,
    Finished,
}

/// Client for the same independent lifecycle holder used by direct installers.
/// The child, rather than the async command future, owns the service-operation
/// and PID-absence leases. Dropping this client closes stdin and synchronously
/// waits for the child's phase-aware EOF finalizer, so cancelling the network
/// future cannot silently leave an originally-running daemon stopped.
pub(crate) struct SelfUpdateDaemonBoundaryClient {
    child: Option<std::process::Child>,
    input: Option<std::process::ChildStdin>,
    output: Option<std::io::BufReader<std::process::ChildStdout>>,
    phase: SelfUpdateBoundaryClientPhase,
}

pub(crate) fn isolate_background_child_from_terminal_interrupt(
    command: &mut std::process::Command,
) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            // SAFETY: setsid is async-signal-safe and this pre-exec closure
            // performs no allocation or other process-global mutation. A new
            // session also creates a new process group and drops the inherited
            // controlling terminal, covering both Ctrl+C and terminal hangup.
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

#[cfg(windows)]
pub(crate) struct VerifiedBackgroundSpawn {
    _executable_pin: std::fs::File,
    ready_nonce: String,
}

#[cfg(windows)]
impl VerifiedBackgroundSpawn {
    pub(crate) fn ready_nonce(&self) -> &str {
        &self.ready_nonce
    }
}

#[cfg(windows)]
pub(crate) fn prepare_verified_background_spawn(
    executable: &std::path::Path,
    expected: &crate::fs_ops::FileToken,
) -> Result<VerifiedBackgroundSpawn> {
    use rand::Rng as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ,
    };

    let metadata = std::fs::symlink_metadata(executable).with_context(|| {
        format!(
            "inspecting verified background executable {}",
            executable.display()
        )
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "verified background executable is not a direct regular file: {}",
            executable.display()
        );
    }
    let mut executable_pin = std::fs::OpenOptions::new()
        .access_mode(FILE_GENERIC_READ)
        // Read sharing is sufficient for the child image loader. Excluding
        // delete and write sharing pins both this namespace occupant and its
        // bytes through spawn/readiness, closing A->B->A races.
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(executable)
        .with_context(|| {
            format!(
                "pinning verified background executable {}",
                executable.display()
            )
        })?;
    let observed = crate::fs_ops::token_for_file(&mut executable_pin).with_context(|| {
        format!(
            "binding verified background executable {}",
            executable.display()
        )
    })?;
    if &observed != expected {
        anyhow::bail!(
            "background executable changed before it could be pinned: {}",
            executable.display()
        );
    }
    let mut nonce = [0_u8; 16];
    rand::rng().fill_bytes(&mut nonce);
    Ok(VerifiedBackgroundSpawn {
        _executable_pin: executable_pin,
        ready_nonce: hex::encode(nonce),
    })
}

pub(crate) fn validate_background_ready_nonce(ready_nonce: &str) -> Result<()> {
    let decoded =
        hex::decode(ready_nonce).context("background readiness nonce is not hexadecimal")?;
    if decoded.len() != 16 || ready_nonce.len() != 32 {
        anyhow::bail!("background readiness nonce must encode exactly 128 bits");
    }
    Ok(())
}

const BACKGROUND_MARKER_LINE_MAX_BYTES: usize = 512;
const LIFECYCLE_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn read_bounded_background_marker<R: std::io::BufRead>(
    reader: &mut R,
    purpose: &str,
) -> Result<(String, usize)> {
    use std::io::BufRead as _;

    let mut line = Vec::with_capacity(BACKGROUND_MARKER_LINE_MAX_BYTES + 1);
    let mut limited =
        std::io::Read::take(&mut *reader, (BACKGROUND_MARKER_LINE_MAX_BYTES + 1) as u64);
    let read = limited
        .read_until(b'\n', &mut line)
        .with_context(|| format!("reading bounded {purpose} marker"))?;
    if read == 0 {
        anyhow::bail!("{purpose} closed stdout before its marker");
    }
    if read > BACKGROUND_MARKER_LINE_MAX_BYTES {
        anyhow::bail!(
            "{purpose} exceeded the {BACKGROUND_MARKER_LINE_MAX_BYTES}-byte marker line limit"
        );
    }
    if line.last() != Some(&b'\n') {
        anyhow::bail!("{purpose} closed stdout with an incomplete marker");
    }
    while matches!(line.last(), Some(b'\r' | b'\n')) {
        line.pop();
    }
    let marker =
        String::from_utf8(line).with_context(|| format!("{purpose} emitted a non-UTF-8 marker"))?;
    Ok((marker, read))
}

pub(crate) fn terminate_and_reap_background_child(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus> {
    if let Some(status) = child
        .try_wait()
        .context("checking failed background child")?
    {
        return Ok(status);
    }
    if let Err(kill_error) = child.kill() {
        if let Some(status) = child
            .try_wait()
            .context("rechecking background child after termination failed")?
        {
            return Ok(status);
        }
        return Err(kill_error).context("terminating failed background child");
    }
    child.wait().context("reaping terminated background child")
}

pub(crate) fn terminate_background_child_on_error(
    child: &mut std::process::Child,
    error: anyhow::Error,
) -> anyhow::Error {
    error.context(
        terminate_and_reap_background_child(child)
            .map(|status| format!("child was terminated and reaped ({status})"))
            .unwrap_or_else(|reap| format!("child termination/reap failed: {reap:#}")),
    )
}

fn background_marker_failure(
    marker_error: anyhow::Error,
    reap: Result<std::process::ExitStatus>,
    reader_joined: std::thread::Result<()>,
) -> anyhow::Error {
    let cleanup_detail = match (reap, reader_joined) {
        (Ok(status), Ok(())) => format!("child was terminated and reaped ({status})"),
        (Err(error), Ok(())) => format!("child termination/reap failed: {error:#}"),
        (Ok(status), Err(_)) => {
            format!("child was reaped ({status}), but its marker reader panicked")
        }
        (Err(error), Err(_)) => {
            format!("child termination/reap failed ({error:#}) and its marker reader panicked")
        }
    };
    marker_error.context(cleanup_detail)
}

pub(crate) fn read_background_marker_line(
    child: &mut std::process::Child,
    output: &mut Option<std::io::BufReader<std::process::ChildStdout>>,
    timeout: Option<std::time::Duration>,
    purpose: &'static str,
) -> Result<(String, usize)> {
    let mut reader = output
        .take()
        .with_context(|| format!("{purpose} marker channel is closed"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader_thread = std::thread::spawn(move || {
        let result = read_bounded_background_marker(&mut reader, purpose);
        let _ = sender.send((reader, result));
    });

    enum ReceiveFailure {
        Timeout(std::time::Duration),
        Disconnected,
    }
    let received = match timeout {
        Some(timeout) => receiver.recv_timeout(timeout).map_err(|error| match error {
            std::sync::mpsc::RecvTimeoutError::Timeout => ReceiveFailure::Timeout(timeout),
            std::sync::mpsc::RecvTimeoutError::Disconnected => ReceiveFailure::Disconnected,
        }),
        None => receiver.recv().map_err(|_| ReceiveFailure::Disconnected),
    };
    match received {
        Ok((reader, Ok(marker))) => {
            reader_thread
                .join()
                .map_err(|_| anyhow::anyhow!("{purpose} marker reader panicked"))?;
            *output = Some(reader);
            Ok(marker)
        }
        Ok((reader, Err(marker_error))) => {
            drop(reader);
            let reap = terminate_and_reap_background_child(child);
            Err(background_marker_failure(
                marker_error,
                reap,
                reader_thread.join(),
            ))
        }
        Err(ReceiveFailure::Timeout(timeout)) => {
            let reap = terminate_and_reap_background_child(child);
            let marker_error = anyhow::anyhow!(
                "{purpose} did not emit a complete marker within {}s",
                timeout.as_secs_f64()
            );
            // Killing and reaping the exact child is the readiness boundary.
            // Do not join a blocked pipe reader here: an untrusted descendant
            // could inherit stdout and otherwise defeat the deadline even
            // after the spawned child was conclusively reaped.
            drop(reader_thread);
            let cleanup_detail = reap
                .map(|status| format!("child was terminated and reaped ({status})"))
                .unwrap_or_else(|error| format!("child termination/reap failed: {error:#}"));
            Err(marker_error.context(cleanup_detail))
        }
        Err(ReceiveFailure::Disconnected) => {
            let reap = terminate_and_reap_background_child(child);
            let marker_error = anyhow::anyhow!("{purpose} marker reader stopped unexpectedly");
            Err(background_marker_failure(
                marker_error,
                reap,
                reader_thread.join(),
            ))
        }
    }
}

#[cfg(windows)]
pub(crate) fn await_expected_background_marker(
    child: &mut std::process::Child,
    stdout: std::process::ChildStdout,
    expected_marker: &str,
    timeout: std::time::Duration,
    total_max_bytes: usize,
    purpose: &'static str,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let mut output = Some(std::io::BufReader::new(stdout));
    let mut total = 0_usize;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let (marker, bytes) =
            read_background_marker_line(child, &mut output, Some(remaining), purpose)?;
        total = total
            .checked_add(bytes)
            .with_context(|| format!("{purpose} marker byte count overflowed"))?;
        if total > total_max_bytes {
            output.take();
            let error = anyhow::anyhow!(
                "{purpose} exceeded the {total_max_bytes}-byte total marker output limit"
            );
            return Err(terminate_background_child_on_error(child, error));
        }
        if marker == expected_marker {
            output.take();
            return Ok(());
        }
    }
}

impl SelfUpdateDaemonBoundaryClient {
    pub(crate) fn start() -> Result<Self> {
        let executable = std::env::current_exe()
            .context("locating the public executable for the self-update lifecycle holder")?;
        service::validate_expected_executable(&executable)?;
        #[cfg(windows)]
        let executable_token = crate::fs_ops::token_for_path(&executable)
            .context("binding the public executable for the self-update lifecycle holder")?;
        #[cfg(windows)]
        let verified_spawn = prepare_verified_background_spawn(&executable, &executable_token)?;
        #[cfg(windows)]
        let ready_nonce = verified_spawn.ready_nonce().to_string();
        let mut command = std::process::Command::new(&executable);
        command
            .arg("__hold-daemon-update-boundary")
            .arg("--initial-executable")
            .arg(&executable)
            .arg("--replacement-executable")
            .arg(&executable);
        #[cfg(windows)]
        command
            .arg("--expected-executable-token")
            .arg(executable_token.to_string())
            .arg("--ready-nonce")
            .arg(&ready_nonce);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        isolate_background_child_from_terminal_interrupt(&mut command);
        let mut child = command
            .spawn()
            .context("starting the independent self-update daemon lifecycle holder")?;
        let input = match child.stdin.take() {
            Some(input) => input,
            None => {
                let error =
                    anyhow::anyhow!("self-update lifecycle holder has no stdin control channel");
                return Err(terminate_background_child_on_error(&mut child, error));
            }
        };
        let output = match child.stdout.take() {
            Some(output) => output,
            None => {
                drop(input);
                let error =
                    anyhow::anyhow!("self-update lifecycle holder has no stdout marker channel");
                return Err(terminate_background_child_on_error(&mut child, error));
            }
        };
        let mut client = Self {
            child: Some(child),
            input: Some(input),
            output: Some(std::io::BufReader::new(output)),
            phase: SelfUpdateBoundaryClientPhase::Stopped,
        };
        let marker = client.read_marker_with_timeout(Some(LIFECYCLE_READY_TIMEOUT))?;
        #[cfg(windows)]
        let ready_prefix = format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} ready {ready_nonce} ");
        #[cfg(not(windows))]
        let ready_prefix = format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} ready ");
        let state = match marker.strip_prefix(&ready_prefix) {
            Some(state) => state,
            None => {
                let error = anyhow::anyhow!(
                    "self-update lifecycle holder returned an unexpected initial marker: {marker}"
                );
                return Err(client.reject_initial_readiness(error));
            }
        };
        let valid_state = [
            installer_state_line(true, true),
            installer_state_line(true, false),
            installer_state_line(false, true),
            installer_state_line(false, false),
        ]
        .contains(&state);
        if !valid_state {
            let error = anyhow::anyhow!(
                "self-update lifecycle holder returned an invalid initial state: {marker}"
            );
            return Err(client.reject_initial_readiness(error));
        }
        #[cfg(windows)]
        drop(verified_spawn);
        Ok(client)
    }

    fn reject_initial_readiness(&mut self, error: anyhow::Error) -> anyhow::Error {
        self.input.take();
        self.output.take();
        let Some(mut child) = self.child.take() else {
            return error.context("self-update lifecycle holder was already released");
        };
        terminate_background_child_on_error(&mut child, error)
    }

    pub(crate) fn restart_replacement(&mut self) -> Result<SelfUpdateBoundaryRestart> {
        if self.phase != SelfUpdateBoundaryClientPhase::Stopped {
            anyhow::bail!("self-update lifecycle holder was not at its stopped boundary");
        }
        self.phase = SelfUpdateBoundaryClientPhase::ReplacementTransition;
        self.write_command("new")?;
        let marker = self.read_marker()?;
        match marker.strip_prefix(&format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} ")) {
            Some(DAEMON_BOUNDARY_NEW_READY) => {
                self.phase = SelfUpdateBoundaryClientPhase::NewReady;
                Ok(SelfUpdateBoundaryRestart::Ready)
            }
            Some(DAEMON_BOUNDARY_NEW_FAILED) => {
                self.phase = SelfUpdateBoundaryClientPhase::ReplacementFailedStopped;
                Ok(SelfUpdateBoundaryRestart::FailedStopped)
            }
            _ => anyhow::bail!(
                "self-update lifecycle holder returned an unexpected replacement marker: {marker}"
            ),
        }
    }

    pub(crate) fn restore_prior(&mut self) -> Result<()> {
        if !matches!(
            self.phase,
            SelfUpdateBoundaryClientPhase::Stopped
                | SelfUpdateBoundaryClientPhase::ReplacementFailedStopped
        ) {
            anyhow::bail!("self-update lifecycle holder was not at a rollback-safe boundary");
        }
        self.phase = SelfUpdateBoundaryClientPhase::RollbackTransition;
        self.write_command("rollback")?;
        let marker = self.read_marker()?;
        match marker.strip_prefix(&format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} ")) {
            Some(DAEMON_BOUNDARY_OLD_RESTORED) => {
                self.phase = SelfUpdateBoundaryClientPhase::Finished;
                self.close_and_wait()
            }
            Some(DAEMON_BOUNDARY_OLD_FAILED) => {
                self.phase = SelfUpdateBoundaryClientPhase::ReplacementFailedStopped;
                anyhow::bail!(
                    "self-update lifecycle holder could not restart the exact prior daemon; it retained and verified daemon absence"
                )
            }
            _ => anyhow::bail!(
                "self-update lifecycle holder returned an unexpected rollback marker: {marker}"
            ),
        }
    }

    pub(crate) fn stop_replacement_for_rollback(&mut self) -> Result<()> {
        if !matches!(
            self.phase,
            SelfUpdateBoundaryClientPhase::NewReady | SelfUpdateBoundaryClientPhase::FinalConfirmed
        ) {
            anyhow::bail!(
                "self-update lifecycle holder has no verified replacement generation to stop"
            );
        }
        self.phase = SelfUpdateBoundaryClientPhase::AbortTransition;
        self.write_command("abort")?;
        let marker = self.read_marker()?;
        let expected = format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} {DAEMON_BOUNDARY_NEW_STOPPED}");
        if marker != expected {
            anyhow::bail!(
                "self-update lifecycle holder returned an unexpected abort marker: {marker}"
            );
        }
        self.phase = SelfUpdateBoundaryClientPhase::ReplacementFailedStopped;
        Ok(())
    }

    pub(crate) fn replacement_is_running(&self) -> bool {
        matches!(
            self.phase,
            SelfUpdateBoundaryClientPhase::NewReady | SelfUpdateBoundaryClientPhase::FinalConfirmed
        )
    }

    pub(crate) fn replacement_is_finally_verified(&self) -> bool {
        self.phase == SelfUpdateBoundaryClientPhase::FinalConfirmed
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.phase == SelfUpdateBoundaryClientPhase::Finished
    }

    pub(crate) fn transition_is_ambiguous(&self) -> bool {
        matches!(
            self.phase,
            SelfUpdateBoundaryClientPhase::ReplacementTransition
                | SelfUpdateBoundaryClientPhase::AbortTransition
                | SelfUpdateBoundaryClientPhase::RollbackTransition
                | SelfUpdateBoundaryClientPhase::FinishTransition
                | SelfUpdateBoundaryClientPhase::ReleaseTransition
        )
    }

    pub(crate) fn verify_replacement_before_commit(&mut self) -> Result<()> {
        if self.phase != SelfUpdateBoundaryClientPhase::NewReady {
            anyhow::bail!("self-update lifecycle holder has no verified replacement to finish");
        }
        self.phase = SelfUpdateBoundaryClientPhase::FinishTransition;
        self.write_command("finish")?;
        let marker = self.read_marker()?;
        let expected =
            format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} {DAEMON_BOUNDARY_FINAL_CONFIRMED}");
        if marker != expected {
            anyhow::bail!(
                "self-update lifecycle holder returned an unexpected final marker: {marker}"
            );
        }
        self.phase = SelfUpdateBoundaryClientPhase::FinalConfirmed;
        Ok(())
    }

    pub(crate) fn release_verified_replacement(&mut self) -> Result<()> {
        if self.phase != SelfUpdateBoundaryClientPhase::FinalConfirmed {
            anyhow::bail!(
                "self-update lifecycle holder has not verified the replacement commit boundary"
            );
        }
        self.phase = SelfUpdateBoundaryClientPhase::ReleaseTransition;
        self.write_command("release")?;
        let marker = self.read_marker()?;
        let expected =
            format!("{INSTALLER_DAEMON_BOUNDARY_PREFIX} {DAEMON_BOUNDARY_AUTHORITY_RELEASED}");
        if marker != expected {
            anyhow::bail!(
                "self-update lifecycle holder returned an unexpected release marker: {marker}"
            );
        }
        self.phase = SelfUpdateBoundaryClientPhase::Finished;
        self.close_and_wait()
    }

    fn write_command(&mut self, command: &str) -> Result<()> {
        let input = self
            .input
            .as_mut()
            .context("self-update lifecycle holder control channel is closed")?;
        use std::io::Write as _;
        writeln!(input, "{command}")?;
        input
            .flush()
            .context("flushing self-update lifecycle holder command")
    }

    fn read_marker(&mut self) -> Result<String> {
        // Once the pinned image has attested its initial readiness, lifecycle
        // operations may legitimately retain service/PID authority while an
        // external contender settles. Never release that authority on a wall
        // clock; the reader still enforces the bounded marker line.
        self.read_marker_with_timeout(None)
    }

    fn read_marker_with_timeout(&mut self, timeout: Option<std::time::Duration>) -> Result<String> {
        let result = read_background_marker_line(
            self.child
                .as_mut()
                .context("self-update lifecycle holder process was already released")?,
            &mut self.output,
            timeout,
            "self-update lifecycle holder",
        )
        .map(|(marker, _)| marker);
        if result.is_err() {
            self.input.take();
            self.output.take();
            self.child.take();
        }
        result
    }

    fn close_and_wait(&mut self) -> Result<()> {
        self.input.take();
        self.output.take();
        let status = self
            .child
            .take()
            .context("self-update lifecycle holder process was already reaped")?
            .wait()
            .context("waiting for self-update lifecycle holder to exit")?;
        if !status.success() {
            anyhow::bail!("self-update lifecycle holder exited with status {status}");
        }
        Ok(())
    }
}

impl Drop for SelfUpdateDaemonBoundaryClient {
    fn drop(&mut self) {
        self.input.take();
        let Some(mut child) = self.child.take() else {
            return;
        };
        match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!(
                "Error: self-update lifecycle holder EOF finalization exited with status {status}; inspect the preceding lifecycle diagnostics before starting another daemon"
            ),
            Err(error) => eprintln!(
                "Error: waiting for self-update lifecycle holder EOF finalization failed: {error}"
            ),
        }
    }
}

enum InstallerUninstallTransition {
    Ready,
    RestoredStopped(anyhow::Error),
}

enum InstallerRollbackTransition {
    Restored,
    FailedStopped(anyhow::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialGenerationStopState {
    NotRequested,
    MayHaveBeenRequested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialLaunchMechanism {
    Stopped,
    Detached,
    ServiceManager,
}

fn classify_initial_launch_mechanism(
    daemon_pid: Option<u32>,
    manager_pid: Option<u32>,
) -> Result<InitialLaunchMechanism> {
    match (daemon_pid, manager_pid) {
        (None, None) => Ok(InitialLaunchMechanism::Stopped),
        (Some(_), None) => Ok(InitialLaunchMechanism::Detached),
        (Some(pid), Some(manager_pid)) if pid == manager_pid => {
            Ok(InitialLaunchMechanism::ServiceManager)
        }
        (None, Some(manager_pid)) => anyhow::bail!(
            "service manager reports daemon PID {manager_pid}, but no process owns the authoritative PID identity"
        ),
        (Some(pid), Some(manager_pid)) => anyhow::bail!(
            "service manager owns PID {manager_pid}, while authoritative daemon PID {pid} belongs to a different generation"
        ),
    }
}

impl InitialGenerationStopState {
    fn can_reuse_observed_generation(self) -> bool {
        self == Self::NotRequested
    }

    fn record_issued<T>(&mut self, request: Result<T>) -> Result<T> {
        let request = request?;
        *self = Self::MayHaveBeenRequested;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallerUninstallState {
    NotApplied,
    AppliedPendingVerification,
    Ready,
}

impl InstallerUninstallState {
    fn is_applied(self) -> bool {
        self != Self::NotApplied
    }

    fn mark_applied(&mut self) {
        *self = Self::AppliedPendingVerification;
    }

    fn record_applied_and_verify<F>(&mut self, verification: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.mark_applied();
        verification()
    }

    fn mark_ready(&mut self) {
        debug_assert_eq!(*self, Self::AppliedPendingVerification);
        *self = Self::Ready;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallerUninstallBoundaryState {
    Applied,
    PriorStopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallerBoundaryPhase {
    Stopping,
    Stopped,
    ReplacementTransition,
    ReplacementFailedStopped,
    RollbackTransition,
    RollbackFailedStopped,
    UninstallTransition,
    NewReady,
    UninstallReady,
    PriorRestored,
    FinalConfirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallerBoundaryFinalization {
    RestoreFailedStop,
    RestorePrior,
    ReestablishStopped,
    VerifyStopped,
    ClassifyUninstall,
    VerifyFinal,
}

fn installer_boundary_finalization(phase: InstallerBoundaryPhase) -> InstallerBoundaryFinalization {
    match phase {
        InstallerBoundaryPhase::Stopping => InstallerBoundaryFinalization::RestoreFailedStop,
        InstallerBoundaryPhase::Stopped => InstallerBoundaryFinalization::RestorePrior,
        InstallerBoundaryPhase::ReplacementTransition
        | InstallerBoundaryPhase::RollbackTransition => {
            InstallerBoundaryFinalization::ReestablishStopped
        }
        InstallerBoundaryPhase::ReplacementFailedStopped
        | InstallerBoundaryPhase::RollbackFailedStopped => {
            InstallerBoundaryFinalization::VerifyStopped
        }
        InstallerBoundaryPhase::UninstallTransition => {
            InstallerBoundaryFinalization::ClassifyUninstall
        }
        InstallerBoundaryPhase::NewReady
        | InstallerBoundaryPhase::UninstallReady
        | InstallerBoundaryPhase::PriorRestored
        | InstallerBoundaryPhase::FinalConfirmed => InstallerBoundaryFinalization::VerifyFinal,
    }
}

fn catch_installer_boundary_unwind<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(anyhow::anyhow!(
                "installer daemon-boundary operation panicked: {message}"
            ))
        }
    }
}

fn finalize_installer_boundary_result<T>(
    result: Result<T>,
    finalize: impl FnOnce(anyhow::Error) -> anyhow::Error,
) -> Result<T> {
    result.map_err(finalize)
}

fn write_installer_daemon_boundary_marker(
    output: &mut impl std::io::Write,
    state: &str,
) -> Result<()> {
    writeln!(output, "{INSTALLER_DAEMON_BOUNDARY_PREFIX} {state}")?;
    Ok(())
}

fn publish_installer_daemon_boundary_state(
    output: &mut impl std::io::Write,
    phase: &mut InstallerBoundaryPhase,
    published_phase: InstallerBoundaryPhase,
    state: &str,
) -> Result<()> {
    // The in-memory phase must describe the already-applied transaction state
    // even when writing or flushing the corresponding wire marker fails.
    *phase = published_phase;
    write_installer_daemon_boundary_marker(output, state)?;
    output
        .flush()
        .context("flushing installer daemon-boundary marker")
}

pub async fn dispatch(
    cmd: DaemonCommand,
    json: bool,
    file_log_writer: crate::logging::FileLogWriter,
) -> Result<()> {
    match cmd {
        DaemonCommand::Start {
            foreground,
            expected_executable,
        } => start(foreground, expected_executable, file_log_writer).await,
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
                stop_detached_locked(&service_lease)?;
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

async fn start(
    foreground: bool,
    expected_executable: Option<std::path::PathBuf>,
    file_log_writer: crate::logging::FileLogWriter,
) -> Result<()> {
    if expected_executable.is_some() && foreground {
        anyhow::bail!("--expected-executable cannot be combined with --foreground");
    }
    #[cfg(not(target_os = "windows"))]
    if expected_executable.is_some() {
        anyhow::bail!("--expected-executable is supported only on Windows");
    }
    #[cfg(target_os = "windows")]
    if let Some(expected_executable) = expected_executable.as_deref() {
        return start_windows_installer_owned(expected_executable, &file_log_writer);
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = (foreground, file_log_writer);
        anyhow::bail!("The background daemon is not supported on this platform.");
    }
    #[cfg(any(unix, target_os = "windows"))]
    {
        if foreground {
            if let Some(pid) = pidfile::running_pid_checked()? {
                anyhow::bail!("Daemon is already running (PID {pid})");
            }
            ensure_startup_file_logging(&file_log_writer)?;
            pidfile::cleanup_pidfile()?;
            return run_foreground().await;
        }

        // All CLI-initiated detached starts share the service-operation lease.
        // The foreground child does not reacquire it: its parent retains this
        // guard until the child publishes its authoritative PID identity.
        let service_lease = service::acquire_service_operation_lease()?;
        if let Some(pid) = pidfile::running_pid_checked()? {
            anyhow::bail!("Daemon is already running (PID {pid})");
        }
        let executable = std::env::current_exe()?;
        let service_installed = service::is_installed_checked()?;
        ensure_startup_file_logging(&file_log_writer)?;
        pidfile::cleanup_pidfile()?;
        if service_installed {
            return service::start_installed_locked(&executable, &service_lease);
        }
        start_detached_executable_locked(&executable, &service_lease)
    }
}

fn ensure_startup_file_logging(file_log_writer: &crate::logging::FileLogWriter) -> Result<()> {
    file_log_writer
        .ensure_initialized()
        .context("initializing secure file logging before daemon readiness")
}

async fn run_foreground() -> Result<()> {
    let shutdown_request = pidfile::write_pidfile_exclusive()?;
    // RAII guard ensures PID file is cleaned up even on panic
    let guard = pidfile::PidGuard::new();
    tracing::info!(
        "codex-switch-global-pace daemon started (PID {})",
        std::process::id()
    );
    let run_result = loop_runner::run_daemon_loop(shutdown_request).await;
    let cleanup_result = guard.cleanup();
    match (run_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run_error), Ok(())) => Err(run_error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(run_error), Err(cleanup_error)) => Err(run_error.context(format!(
            "daemon loop failed and PID cleanup was also incomplete: {cleanup_error:#}"
        ))),
    }
}

fn start_detached_executable_locked(
    exe: &std::path::Path,
    _service_lease: &service::ServiceOperationLease,
) -> Result<()> {
    let mut command = std::process::Command::new(exe);
    command
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    isolate_background_child_from_terminal_interrupt(&mut command);
    let mut child = command.spawn()?;

    let pid = await_daemon_ready(&mut child, STARTUP_TIMEOUT)?;
    user_println(&format!("Daemon started (PID {pid})"));
    Ok(())
}

#[cfg(target_os = "windows")]
fn start_windows_installer_owned(
    expected_executable: &std::path::Path,
    file_log_writer: &crate::logging::FileLogWriter,
) -> Result<()> {
    service::validate_expected_executable(expected_executable)?;
    let service_lease = service::acquire_service_operation_lease()?;
    if pidfile::running_pid_checked()?.is_some() {
        anyhow::bail!("Daemon is already running");
    }
    let service_installed = service::is_installed_checked()?;
    ensure_startup_file_logging(file_log_writer)?;
    pidfile::cleanup_pidfile()?;
    if service_installed {
        service::start_installed_locked(expected_executable, &service_lease)
    } else {
        start_detached_executable_locked(expected_executable, &service_lease)
    }
}

/// How long a freshly spawned daemon gets to publish its PID file.
///
/// Generous on purpose: the wait below returns the moment the file appears, so
/// the only thing a large value costs is how long a genuinely broken start
/// takes to be reported. A tight bound, on the other hand, turns a cold binary
/// on a slow disk — a fresh self-update, an on-access virus scan, a loaded CI
/// runner — into a spurious "start failed".
const STARTUP_TIMEOUT: std::time::Duration = DAEMON_TRANSITION_TIMEOUT;

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
                let startup_error = anyhow::anyhow!(
                    "Daemon startup PID {pid} lost the PID-lock authority to PID {running_pid}; the new child was rejected"
                );
                return match terminate_failed_daemon_child(child) {
                    Ok(()) => Err(anyhow::anyhow!(
                        "{startup_error:#}; the rejected startup child was stopped"
                    )),
                    Err(cleanup_error) => Err(startup_error.context(format!(
                        "failed startup child cleanup was incomplete: {cleanup_error:#}"
                    ))),
                };
            }
            Ok(None) => {}
            Err(error) => last_probe_error = Some(error),
        }
        if std::time::Instant::now() >= deadline {
            let timeout_error = if let Some(error) = last_probe_error {
                error.context(format!(
                    "Daemon (PID {pid}) did not publish a valid, locked identity within {}s",
                    timeout.as_secs()
                ))
            } else {
                anyhow::anyhow!(
                    "Daemon (PID {pid}) did not initialize within {}s (no locked PID identity published); check logs",
                    timeout.as_secs()
                )
            };
            return match terminate_failed_daemon_child(child) {
                Ok(()) => Err(anyhow::anyhow!(
                    "{timeout_error:#}; the timed-out startup child was stopped"
                )),
                Err(cleanup_error) => Err(timeout_error.context(format!(
                    "timed-out startup child cleanup was incomplete: {cleanup_error:#}"
                ))),
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn terminate_failed_daemon_child(child: &mut std::process::Child) -> Result<()> {
    match child.kill() {
        Ok(()) => {
            child
                .wait()
                .context("reaping the terminated daemon startup child")?;
            Ok(())
        }
        Err(kill_error) => match child
            .try_wait()
            .context("classifying the daemon startup child after kill failed")?
        {
            Some(_) => Ok(()),
            None => Err(kill_error).context(
                "daemon startup child remained live after its cleanup kill request failed",
            ),
        },
    }
}

fn stop(expected_service_executable: Option<std::path::PathBuf>) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    if expected_service_executable.is_some() {
        anyhow::bail!("--expected-service-executable is supported only on Windows");
    }
    #[cfg(target_os = "windows")]
    if let Some(expected_executable) = expected_service_executable.as_deref() {
        return stop_windows_installer_owned(expected_executable);
    }

    let service_lease = service::acquire_service_operation_lease()?;
    let executable = std::env::current_exe().context("locating daemon executable for stop")?;
    service::validate_expected_executable(&executable)?;

    #[cfg(target_os = "windows")]
    {
        // A scheduled task runs the same foreground daemon. Ask that process
        // to unwind first; `/End` would terminate it during credential writes.
        if pidfile::running_pid_checked()?.is_some() {
            return stop_detached_locked(&service_lease);
        }
        pidfile::cleanup_pidfile()?;
        // An older or still-starting scheduled task may not have a trusted
        // pidfile. There is no generation-bound process to signal, so Task
        // Scheduler is the only remaining stop authority.
        if service::is_installed_checked()? {
            service::stop_installed_locked(&executable, &service_lease)?;
            pidfile::cleanup_pidfile()?;
            return Ok(());
        }
        stop_detached_locked(&service_lease)
    }

    #[cfg(not(target_os = "windows"))]
    {
        if service::is_installed_checked()? {
            service::stop_installed_locked(&executable, &service_lease)?;
            pidfile::cleanup_pidfile()?;
            return Ok(());
        }

        stop_detached_locked(&service_lease)
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
            stop_detached_locked(&service_lease)?;
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
                    start_detached_executable_locked(expected_executable, &service_lease)?;
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

fn stop_detached_locked(_service_lease: &service::ServiceOperationLease) -> Result<()> {
    let observed_pid = pidfile::read_pidfile();
    let Some(target) = pidfile::running_generation_checked()? else {
        if observed_pid.is_none() {
            anyhow::bail!("No daemon PID file found; daemon may not be running");
        }
        pidfile::cleanup_pidfile()?;
        user_println("Daemon was not running (stale PID file cleaned up)");
        return Ok(());
    };
    let pid = target.pid();
    stop_daemon_generation(target)?;
    pidfile::cleanup_pidfile()?;
    user_println(&format!("Stopped daemon (PID {pid})"));
    Ok(())
}

#[must_use = "an issued daemon stop request must be waited to a classified generation state"]
struct DaemonGenerationStopRequest {
    shutdown_request: pidfile::ShutdownRequestOutcome,
}

#[derive(Debug)]
struct RequestedGenerationSettlement {
    observed_generation: Option<pidfile::DaemonGeneration>,
    diagnostics: Vec<String>,
}

#[derive(Default)]
pub(crate) struct TransientDiagnostics {
    first: Option<String>,
    last: Option<String>,
    count: usize,
}

impl TransientDiagnostics {
    pub(crate) fn record(&mut self, diagnostic: String) {
        self.count = self.count.saturating_add(1);
        if self.first.is_none() {
            self.first = Some(diagnostic.clone());
        }
        self.last = Some(diagnostic);
    }

    pub(crate) fn into_messages(self) -> Vec<String> {
        let Some(first) = self.first else {
            return Vec::new();
        };
        let mut messages = vec![first.clone()];
        if self.count > 1 {
            if let Some(last) = self.last
                && last != first
            {
                messages.push(format!("last transient observation: {last}"));
            }
            messages.push(format!(
                "{} transient lifecycle observation failures occurred while authority remained held",
                self.count
            ));
        }
        messages
    }

    pub(crate) fn into_summary(self, label: &str) -> Option<String> {
        let first = self.first?;
        if self.count == 1 {
            return Some(format!("1 {label}: {first}"));
        }
        let last = self
            .last
            .expect("a recorded transient diagnostic has a last entry");
        Some(format!(
            "{} {label}s; first: {first}; last: {last}",
            self.count
        ))
    }
}

enum RequestedGenerationObservation {
    TargetRunning,
    Settled(Option<pidfile::DaemonGeneration>),
}

fn observe_requested_generation(
    target: &pidfile::DaemonGeneration,
) -> Result<RequestedGenerationObservation> {
    let observed = pidfile::running_generation_checked()?;
    if observed.as_ref() == Some(target) {
        Ok(RequestedGenerationObservation::TargetRunning)
    } else {
        Ok(RequestedGenerationObservation::Settled(observed))
    }
}

impl DaemonGenerationStopRequest {
    fn issue(target: pidfile::DaemonGeneration) -> Result<Self> {
        let shutdown_request = pidfile::request_shutdown(&target)?;
        Ok(Self { shutdown_request })
    }

    fn wait(self) -> Result<()> {
        let pid = self.shutdown_request.target().pid();
        let target = self.shutdown_request.target().clone();
        let started = std::time::Instant::now();
        let stopped = wait_for_requested_generation_stop_with(
            pid,
            || observe_requested_generation(&target),
            || started.elapsed(),
            std::thread::sleep,
        )
        .map_err(|err| {
            anyhow::anyhow!(
                "{err}. The daemon may still be finishing an in-flight credential rotation; \
             refusing to force-terminate it. Retry `codex-switch-global-pace daemon stop` shortly."
            )
        });
        let request_durability = self.shutdown_request.require_durable();
        match (stopped, request_durability) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(stop_error), Ok(())) => Err(stop_error),
            (Ok(()), Err(request_error)) => Err(request_error.context(
                "daemon stopped from a visible shutdown request whose durability was not confirmed",
            )),
            (Err(stop_error), Err(request_error)) => Err(stop_error.context(format!(
                "daemon shutdown request durability was also unconfirmed: {request_error:#}"
            ))),
        }
    }

    fn settle_for_lifecycle(self) -> RequestedGenerationSettlement {
        let pid = self.shutdown_request.target().pid();
        let mut settlement =
            wait_for_requested_generation_to_settle(self.shutdown_request.target().clone());
        if let Err(error) = self.shutdown_request.require_durable() {
            settlement.diagnostics.push(format!(
                "the exact shutdown request for daemon PID {pid} was visible, but its durability was not confirmed: {error:#}"
            ));
        }
        settlement
    }
}

fn stop_daemon_generation(target: pidfile::DaemonGeneration) -> Result<()> {
    DaemonGenerationStopRequest::issue(target)?.wait()
}

fn wait_for_requested_generation_stop_with<Observe, Elapsed, Sleep>(
    pid: u32,
    mut observe: Observe,
    mut elapsed: Elapsed,
    mut sleep: Sleep,
) -> Result<()>
where
    Observe: FnMut() -> Result<RequestedGenerationObservation>,
    Elapsed: FnMut() -> std::time::Duration,
    Sleep: FnMut(std::time::Duration),
{
    let mut diagnostics = TransientDiagnostics::default();
    loop {
        match observe() {
            Ok(RequestedGenerationObservation::Settled(None)) => return Ok(()),
            Ok(RequestedGenerationObservation::Settled(Some(current))) => {
                anyhow::bail!(
                    "Daemon changed to a different generation while waiting for PID {pid} to stop (current PID {})",
                    current.pid()
                );
            }
            Ok(RequestedGenerationObservation::TargetRunning) => {}
            Err(error) => diagnostics.record(format!(
                "failed to inspect the already-requested daemon PID {pid}: {error:#}"
            )),
        }
        if elapsed() >= DAEMON_TRANSITION_TIMEOUT {
            let diagnostic = diagnostics
                .into_summary("transient daemon-state observation failure")
                .map(|summary| format!("; {summary}"))
                .unwrap_or_default();
            anyhow::bail!(
                "Daemon did not stop within {}s{diagnostic}",
                DAEMON_TRANSITION_TIMEOUT.as_secs()
            );
        }
        sleep(std::time::Duration::from_millis(100));
    }
}

fn wait_for_requested_generation_to_settle(
    target: pidfile::DaemonGeneration,
) -> RequestedGenerationSettlement {
    let pid = target.pid();
    let started = std::time::Instant::now();
    wait_for_requested_generation_to_settle_with(
        pid,
        || observe_requested_generation(&target),
        || started.elapsed(),
        std::thread::sleep,
        |elapsed| {
            eprintln!(
                "Daemon PID {pid} is still settling {elapsed:.0}s after its already-issued stop request; lifecycle authority remains held and no additional request will be published"
            );
        },
    )
}

fn wait_for_requested_generation_to_settle_with<Observe, Elapsed, Sleep, Report>(
    pid: u32,
    mut observe: Observe,
    mut elapsed: Elapsed,
    mut sleep: Sleep,
    mut report: Report,
) -> RequestedGenerationSettlement
where
    Observe: FnMut() -> Result<RequestedGenerationObservation>,
    Elapsed: FnMut() -> std::time::Duration,
    Sleep: FnMut(std::time::Duration),
    Report: FnMut(f64),
{
    let mut next_diagnostic = GENERATION_SETTLE_DIAGNOSTIC_INTERVAL;
    let mut diagnostics = TransientDiagnostics::default();
    loop {
        match observe() {
            Ok(RequestedGenerationObservation::Settled(observed_generation)) => {
                return RequestedGenerationSettlement {
                    observed_generation,
                    diagnostics: diagnostics.into_messages(),
                };
            }
            Ok(RequestedGenerationObservation::TargetRunning) => {}
            Err(error) => {
                let diagnostic = format!(
                    "transiently failed to inspect the already-requested daemon PID {pid}: {error:#}"
                );
                eprintln!("{diagnostic}; lifecycle authority remains held");
                diagnostics.record(diagnostic);
            }
        }
        let current_elapsed = elapsed();
        if current_elapsed >= next_diagnostic {
            report(current_elapsed.as_secs_f64());
            next_diagnostic = current_elapsed.saturating_add(GENERATION_SETTLE_DIAGNOSTIC_INTERVAL);
        }
        sleep(std::time::Duration::from_millis(100));
    }
}

pub struct SelfUpdateDaemonRestart {
    initial_pid: Option<u32>,
    initial_generation: Option<pidfile::DaemonGeneration>,
    initial_executable: std::path::PathBuf,
    initial_executable_state: InitialExecutableState,
    expected_executable_token: Option<crate::fs_ops::FileToken>,
    initial_service_snapshot: service::ServiceStateSnapshot,
    expected_service_snapshot: service::ServiceStateSnapshot,
    stopped_service_snapshot: Option<service::ServiceStateSnapshot>,
    initial_launch_mechanism: InitialLaunchMechanism,
    executable: std::path::PathBuf,
    service_executable: std::path::PathBuf,
    service_installed: bool,
    service_lease: service::ServiceOperationLease,
    absence_lease: Option<pidfile::DaemonAbsenceLease>,
    restarted_pid: Option<u32>,
    uninstall_state: InstallerUninstallState,
    initial_stop_state: InitialGenerationStopState,
    lifecycle_stop_diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InitialExecutableState {
    Absent,
    Present(crate::fs_ops::FileToken),
}

fn require_captured_executable_state(
    path: &std::path::Path,
    expected: &InitialExecutableState,
    exact_identity: bool,
) -> Result<()> {
    match expected {
        InitialExecutableState::Absent => {
            if crate::fs_ops::token_if_present(path)?.is_some() {
                anyhow::bail!(
                    "an executable appeared at the initially absent public path: {}",
                    path.display()
                );
            }
        }
        InitialExecutableState::Present(expected_token) => {
            let observed = crate::fs_ops::token_for_path(path)
                .with_context(|| format!("binding captured executable {}", path.display()))?;
            let matches = if exact_identity {
                &observed == expected_token
            } else {
                observed.same_contents(expected_token)
            };
            if !matches {
                anyhow::bail!(
                    "captured executable {} did not match its exact pre-transaction {}",
                    path.display(),
                    if exact_identity {
                        "identity"
                    } else {
                        "contents"
                    }
                );
            }
        }
    }
    Ok(())
}

fn validate_running_daemon_executable(
    expected_executable: &std::path::Path,
    pid: u32,
    running_executable: &std::path::Path,
) -> Result<crate::fs_ops::FileToken> {
    service::validate_expected_executable(running_executable)?;
    let expected_token = crate::fs_ops::token_for_path(expected_executable).with_context(|| {
        format!(
            "binding the installer executable before stopping daemon PID {pid}: {}",
            expected_executable.display()
        )
    })?;
    let running_token = crate::fs_ops::token_for_path(running_executable).with_context(|| {
        format!(
            "binding the published executable identity for daemon PID {pid}: {}",
            running_executable.display()
        )
    })?;
    if running_token != expected_token {
        anyhow::bail!(
            "daemon PID {pid} is running from {}, not the installer-bound executable {}; refusing to guess which executable should be restored",
            running_executable.display(),
            expected_executable.display()
        );
    }
    Ok(expected_token)
}

fn require_running_daemon_executable(
    expected_executable: &std::path::Path,
    missing_context: &'static str,
) -> Result<u32> {
    let (pid, running_executable) =
        pidfile::running_identity_checked()?.context(missing_context)?;
    validate_running_daemon_executable(expected_executable, pid, &running_executable)?;
    Ok(pid)
}

enum FailedStopRestoration {
    Exact,
    ExactWithDiagnostics(anyhow::Error),
}

fn finalize_prior_restart_restoration<State, Diagnostics, ReestablishAbsence>(
    state: &mut State,
    restoration: Result<()>,
    diagnostics: Diagnostics,
    reestablish_absence: ReestablishAbsence,
) -> Result<FailedStopRestoration>
where
    Diagnostics: FnOnce(&mut State) -> Result<()>,
    ReestablishAbsence: FnOnce(&mut State) -> Result<()>,
{
    match restoration {
        Ok(()) => Ok(match diagnostics(state) {
            Ok(()) => FailedStopRestoration::Exact,
            Err(error) => FailedStopRestoration::ExactWithDiagnostics(error.context(
                "the exact prior daemon state was restored and remains running; historical lifecycle observation diagnostics do not authorize stopping it again",
            )),
        }),
        Err(restoration_error) => match reestablish_absence(state) {
            Ok(()) => Err(restoration_error.context(
                "the prior daemon restart failed; exact daemon absence was re-established before lifecycle authority was released",
            )),
            Err(absence_error) => Err(restoration_error.context(format!(
                "the prior daemon restart failed and exact daemon absence could not be re-established: {absence_error:#}"
            ))),
        },
    }
}

impl SelfUpdateDaemonRestart {
    /// Variant used by the release-verified direct installer. The helper itself
    /// runs from the verified download directory, so the public executable
    /// whose daemon/service state is being protected must be supplied
    /// explicitly.
    fn capture_for_executable(executable: std::path::PathBuf) -> Result<Self> {
        service::validate_expected_executable(&executable)?;
        let service_lease = service::acquire_service_operation_lease()?;
        let initial_executable_state = match crate::fs_ops::token_if_present(&executable)? {
            Some(token) => InitialExecutableState::Present(token),
            None => InitialExecutableState::Absent,
        };
        let running_identity = pidfile::running_identity_checked()?;
        let initial_generation = pidfile::running_generation_checked()?;
        let initial_running_token = running_identity
            .as_ref()
            .map(|(pid, running_executable)| {
                validate_running_daemon_executable(&executable, *pid, running_executable)
            })
            .transpose()?;
        if let Some(running_token) = initial_running_token.as_ref() {
            match &initial_executable_state {
                InitialExecutableState::Present(initial_token)
                    if initial_token == running_token => {}
                _ => anyhow::bail!(
                    "running daemon executable changed while its initial public identity was captured"
                ),
            }
        }
        let initial_pid = running_identity.as_ref().map(|(pid, _)| *pid);
        if initial_generation
            .as_ref()
            .map(pidfile::DaemonGeneration::pid)
            != initial_pid
        {
            anyhow::bail!("daemon generation changed while its executable identity was captured");
        }
        let initial_service_snapshot =
            service::capture_service_state_snapshot(&executable, initial_pid)?;
        let service_installed = initial_service_snapshot.is_installed();
        let initial_launch_mechanism =
            classify_initial_launch_mechanism(initial_pid, initial_service_snapshot.manager_pid())?;
        let running_after_snapshot = pidfile::running_identity_checked()?;
        let generation_after_snapshot = pidfile::running_generation_checked()?;
        if generation_after_snapshot != initial_generation {
            anyhow::bail!(
                "daemon generation changed while its exact launch mechanism was captured"
            );
        }
        let executable_after_snapshot = crate::fs_ops::token_if_present(&executable)?;
        let executable_unchanged = match (&initial_executable_state, executable_after_snapshot) {
            (InitialExecutableState::Absent, None) => true,
            (InitialExecutableState::Present(expected), Some(observed)) => expected == &observed,
            _ => false,
        };
        if !executable_unchanged {
            anyhow::bail!(
                "initial executable identity changed while daemon/service state was captured: {}",
                executable.display()
            );
        }
        match (initial_pid, running_after_snapshot) {
            (None, None) => {}
            (Some(expected_pid), Some((observed_pid, observed_executable)))
                if expected_pid == observed_pid =>
            {
                let observed_token = validate_running_daemon_executable(
                    &executable,
                    observed_pid,
                    &observed_executable,
                )?;
                if initial_running_token.as_ref() != Some(&observed_token) {
                    anyhow::bail!(
                        "daemon executable identity changed while its exact launch mechanism was captured"
                    );
                }
            }
            _ => anyhow::bail!(
                "daemon PID identity changed while its exact launch mechanism was captured"
            ),
        }
        Ok(Self {
            initial_pid,
            initial_generation,
            initial_executable: executable.clone(),
            expected_executable_token: match &initial_executable_state {
                InitialExecutableState::Present(token) => Some(token.clone()),
                InitialExecutableState::Absent => None,
            },
            initial_executable_state,
            expected_service_snapshot: initial_service_snapshot.clone(),
            initial_service_snapshot,
            stopped_service_snapshot: None,
            initial_launch_mechanism,
            executable: executable.clone(),
            service_executable: executable,
            service_installed,
            service_lease,
            absence_lease: None,
            restarted_pid: None,
            uninstall_state: InstallerUninstallState::NotApplied,
            initial_stop_state: InitialGenerationStopState::NotRequested,
            lifecycle_stop_diagnostics: Vec::new(),
        })
    }

    fn record_lifecycle_settlement(
        &mut self,
        settlement: RequestedGenerationSettlement,
    ) -> Option<pidfile::DaemonGeneration> {
        self.lifecycle_stop_diagnostics
            .extend(settlement.diagnostics);
        settlement.observed_generation
    }

    fn stop_lifecycle_generation(
        &mut self,
        target: pidfile::DaemonGeneration,
    ) -> Result<Option<pidfile::DaemonGeneration>> {
        let request = DaemonGenerationStopRequest::issue(target)?;
        Ok(self.record_lifecycle_settlement(request.settle_for_lifecycle()))
    }

    fn require_clean_lifecycle_settlement(&mut self, context: &str) -> Result<()> {
        if self.lifecycle_stop_diagnostics.is_empty() {
            return Ok(());
        }
        let diagnostics = std::mem::take(&mut self.lifecycle_stop_diagnostics);
        anyhow::bail!("{context}: {}", diagnostics.join("; "))
    }

    fn stop_before_update_inner(&mut self) -> Result<()> {
        if self.absence_lease.is_some() {
            return Ok(());
        }

        if self.initial_pid.is_some() {
            user_println("Stopping daemon before self-update...");
            #[cfg(not(target_os = "windows"))]
            if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager {
                let mut request_spawned = false;
                let manager_stop = service::stop_installed_manager_observed_locked(
                    &self.executable,
                    &self.service_lease,
                    || request_spawned = true,
                );
                if request_spawned {
                    self.initial_stop_state = InitialGenerationStopState::MayHaveBeenRequested;
                }
                if request_spawned {
                    let target = self.initial_generation.clone().context(
                        "captured daemon PID had no exact generation token before manager stop",
                    )?;
                    let settlement = wait_for_requested_generation_to_settle(target);
                    self.record_lifecycle_settlement(settlement);
                }
                if !manager_stop? {
                    anyhow::bail!(
                        "the captured service-managed daemon generation was no longer owned by its manager at the stop boundary"
                    );
                }
            } else {
                // Issue the generation-bound graceful request separately from
                // its wait. A pre-publication error leaves `NotRequested`;
                // once the request is visible, even a later timeout must
                // settle and restart this exact prior generation before
                // restoration can be claimed.
                let stop_request =
                    self.initial_stop_state
                        .record_issued(DaemonGenerationStopRequest::issue(
                            self.initial_generation.clone().context(
                                "captured daemon PID had no exact generation token before shutdown",
                            )?,
                        ))?;
                let settlement = stop_request.settle_for_lifecycle();
                self.record_lifecycle_settlement(settlement);
            }
            #[cfg(target_os = "windows")]
            {
                let stop_request =
                    self.initial_stop_state
                        .record_issued(DaemonGenerationStopRequest::issue(
                            self.initial_generation.clone().context(
                                "captured daemon PID had no exact generation token before shutdown",
                            )?,
                        ))?;
                let settlement = stop_request.settle_for_lifecycle();
                self.record_lifecycle_settlement(settlement);
            }
        }

        // Take the same authority lock used by every foreground daemon before
        // touching the executable. This both removes an exactly revalidated
        // stale PID identity and keeps an initially stopped daemon stopped. A
        // one-shot foreground process that won the stop-to-lock handoff is
        // resolved by the same typed contender protocol used after a failed
        // restart; arbitrary lock or identity errors still fail closed.
        self.reacquire_absence_after_foreground_contenders(self.initial_generation.clone())?;
        self.record_stopped_service_snapshot(true)?;
        self.require_clean_lifecycle_settlement(
            "daemon stop settled to an exact state, but one or more lifecycle observations were unreliable",
        )
    }

    fn verify_initial_executable_unchanged(&self) -> Result<()> {
        require_captured_executable_state(
            &self.initial_executable,
            &self.initial_executable_state,
            true,
        )
        .context("initial daemon executable changed before its running state could be restored")
    }

    fn verify_initial_executable_contents_restored(&self) -> Result<()> {
        require_captured_executable_state(
            &self.initial_executable,
            &self.initial_executable_state,
            false,
        )
        .context("installer rollback did not restore the exact initial executable contents")
    }

    fn verify_initial_service_state_unchanged(&self) -> Result<()> {
        let expected_manager_pid = match self.initial_launch_mechanism {
            InitialLaunchMechanism::ServiceManager => self.initial_pid,
            InitialLaunchMechanism::Stopped | InitialLaunchMechanism::Detached => None,
        };
        service::require_service_state_snapshot_with_manager(
            &self.initial_executable,
            &self.initial_service_snapshot,
            expected_manager_pid,
        )
    }

    fn verify_service_state_ready_for_initial_restart(&self) -> Result<()> {
        match self.initial_launch_mechanism {
            InitialLaunchMechanism::ServiceManager => {
                service::require_service_manager_stopped_state(
                    &self.initial_executable,
                    &self.initial_service_snapshot,
                )
            }
            InitialLaunchMechanism::Detached | InitialLaunchMechanism::Stopped => {
                service::require_service_state_snapshot(
                    &self.initial_executable,
                    &self.initial_service_snapshot,
                )
            }
        }
    }

    fn record_stopped_service_snapshot(&mut self, require_initial_state: bool) -> Result<()> {
        if require_initial_state {
            self.verify_service_state_ready_for_initial_restart()?;
        }
        let snapshot = service::capture_service_state_snapshot(&self.service_executable, None)?;
        if snapshot.manager_pid().is_some() {
            anyhow::bail!(
                "service manager still owns a daemon generation after exact PID absence was acquired"
            );
        }
        self.stopped_service_snapshot = Some(snapshot);
        Ok(())
    }

    fn record_expected_service_snapshot_after_restart(&mut self, restarted_pid: u32) -> Result<()> {
        let observed =
            service::capture_service_state_snapshot(&self.service_executable, Some(restarted_pid))?;
        match self.initial_launch_mechanism {
            InitialLaunchMechanism::ServiceManager => {
                if observed.manager_pid() != Some(restarted_pid) || !observed.is_installed() {
                    anyhow::bail!(
                        "the restarted daemon PID {restarted_pid} was not the exact service-manager generation"
                    );
                }
                if self.service_executable == self.initial_executable
                    && !observed.matches_snapshot_with_manager(
                        &self.initial_service_snapshot,
                        Some(restarted_pid),
                    )
                {
                    anyhow::bail!(
                        "the original service definition or runtime state changed while its daemon was restarted"
                    );
                }
            }
            InitialLaunchMechanism::Detached => {
                if self.service_executable != self.initial_executable
                    || !observed.matches_snapshot_with_manager(&self.initial_service_snapshot, None)
                {
                    anyhow::bail!(
                        "the inactive service definition changed while the foreground daemon generation was restarted"
                    );
                }
            }
            InitialLaunchMechanism::Stopped => anyhow::bail!(
                "an initially stopped daemon cannot record a restarted service snapshot"
            ),
        }
        self.expected_service_snapshot = observed;
        Ok(())
    }

    fn restart_initial_generation_after_failed_stop(
        &mut self,
        observed_generation: Option<pidfile::DaemonGeneration>,
    ) -> Result<FailedStopRestoration> {
        // If the requested original generation was replaced, bind and stop
        // that exact published contender before using its PID as the stale
        // identity authorized for absence acquisition. The acquisition loop
        // then handles only a later handoff winner; it does not repeat this
        // already-settled mutation.
        if let Some(target) = observed_generation.as_ref() {
            self.stop_lifecycle_generation(target.clone())?;
        }
        self.reacquire_absence_after_foreground_contenders(observed_generation.clone())?;
        self.verify_service_state_ready_for_initial_restart()?;
        self.verify_initial_executable_unchanged()?;
        self.release_absence_for_restart()?;
        let restoration = (|| -> Result<()> {
            match self.initial_launch_mechanism {
                InitialLaunchMechanism::ServiceManager => {
                    service::start_installed_locked(&self.initial_executable, &self.service_lease)?
                }
                InitialLaunchMechanism::Detached => {
                    start_detached_executable_locked(&self.initial_executable, &self.service_lease)?
                }
                InitialLaunchMechanism::Stopped => anyhow::bail!(
                    "an initially stopped daemon cannot enter running-state restoration"
                ),
            }
            let (restored_pid, restored_executable) = pidfile::running_identity_checked()?
                .context("the prior running daemon state could not be restored")?;
            validate_running_daemon_executable(
                &self.initial_executable,
                restored_pid,
                &restored_executable,
            )?;
            self.executable = self.initial_executable.clone();
            self.service_executable = self.initial_executable.clone();
            self.record_expected_service_snapshot_after_restart(restored_pid)?;
            self.restarted_pid = Some(restored_pid);
            self.initial_stop_state = InitialGenerationStopState::NotRequested;
            Ok(())
        })();
        finalize_prior_restart_restoration(
            self,
            restoration,
            |transaction| {
                transaction.require_clean_lifecycle_settlement(
                    "the exact prior daemon state was restored after a delayed stop, but lifecycle observation diagnostics remain",
                )
            },
            |transaction| {
                transaction.reestablish_absence_after_failed_restart_with_alternative(None)
            },
        )
    }

    fn restore_after_failed_stop(&mut self) -> Result<FailedStopRestoration> {
        let mut observed_generation = pidfile::running_generation_checked()?;

        if self.initial_pid.is_some() {
            if observed_generation == self.initial_generation
                && self.initial_stop_state.can_reuse_observed_generation()
            {
                self.verify_initial_service_state_unchanged()?;
                self.verify_initial_executable_unchanged()?;
                return Ok(FailedStopRestoration::Exact);
            }

            if self.initial_stop_state == InitialGenerationStopState::MayHaveBeenRequested {
                // The first wait may have timed out after a valid signal or
                // shutdown request. Do not send it again and do not treat a
                // still-visible PID as stable: wait for that exact request to
                // settle before acquiring absence and starting a new copy.
                let target = self
                    .initial_generation
                    .clone()
                    .context("an issued initial stop had no retained daemon generation token")?;
                observed_generation = self
                    .record_lifecycle_settlement(wait_for_requested_generation_to_settle(target));
            }

            return self.restart_initial_generation_after_failed_stop(observed_generation);
        }

        if let Some(target) = observed_generation.as_ref() {
            self.stop_lifecycle_generation(target.clone())?;
        }
        self.reacquire_absence_after_foreground_contenders(observed_generation.clone())?;
        self.verify_initial_service_state_unchanged()?;
        self.absence_lease
            .as_ref()
            .context("prior daemon absence was not restored")?
            .verify()?;
        Ok(match self.require_clean_lifecycle_settlement(
            "the prior stopped state was restored, but lifecycle observation diagnostics remain",
        ) {
            Ok(()) => FailedStopRestoration::Exact,
            Err(error) => FailedStopRestoration::ExactWithDiagnostics(error),
        })
    }

    fn reestablish_absence_after_failed_restart_with_alternative(
        &mut self,
        alternative_service_executable: Option<&std::path::Path>,
    ) -> Result<()> {
        if let Some(absence_lease) = self.absence_lease.as_ref() {
            return absence_lease.verify();
        }

        let observed_generation = pidfile::running_generation_checked()?;
        if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager {
            if let Some(alternative) = alternative_service_executable {
                self.classify_service_executable(alternative)?;
            }
            #[cfg(target_os = "windows")]
            service::stop_failed_start_for_self_update_locked(&self.service_lease)?;
            #[cfg(unix)]
            {
                // A direct foreground start does not participate in the
                // service-operation lease. If it wins the short PID-lock race
                // after absence is released, stopping the service manager is
                // insufficient: stop that exact observed generation too, then
                // revalidate the owned service as stopped.
                service::stop_installed_manager_locked(
                    &self.service_executable,
                    &self.service_lease,
                )?;
                if let Some(target) = pidfile::running_generation_checked()? {
                    self.stop_lifecycle_generation(target)?;
                }
                service::validate_uninstall_owner(&self.service_executable)?;
            }
            #[cfg(not(any(unix, target_os = "windows")))]
            service::stop_installed_locked(&self.service_executable, &self.service_lease)?;
        } else if let Some(target) = observed_generation.as_ref() {
            self.stop_lifecycle_generation(target.clone())?;
        }

        self.reacquire_absence_after_foreground_contenders(observed_generation.clone())?;
        self.record_stopped_service_snapshot(false)?;
        self.restarted_pid = None;
        self.require_clean_lifecycle_settlement(
            "daemon absence was re-established, but lifecycle observation diagnostics remain",
        )
    }

    fn reacquire_absence_after_foreground_contenders(
        &mut self,
        mut expected_stale_generation: Option<pidfile::DaemonGeneration>,
    ) -> Result<()> {
        if let Some(absence_lease) = self.absence_lease.as_ref() {
            return absence_lease.verify();
        }
        loop {
            let settlement =
                wait_for_contending_daemon_resolution(expected_stale_generation.as_ref());
            self.lifecycle_stop_diagnostics
                .extend(settlement.diagnostics);
            match settlement.resolution {
                InstallerContenderResolution::Absence(lease) => {
                    self.absence_lease = Some(lease);
                    return Ok(());
                }
                InstallerContenderResolution::Published(target) => {
                    self.stop_lifecycle_generation(target.clone())?;
                    expected_stale_generation = Some(target);
                }
            }
        }
    }

    fn classify_service_executable(&mut self, alternative: &std::path::Path) -> Result<()> {
        let mut candidates = Vec::new();
        for candidate in [
            self.service_executable.clone(),
            alternative.to_path_buf(),
            self.initial_executable.clone(),
        ] {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        let mut matched = Vec::new();
        let mut failures = Vec::new();
        for candidate in candidates {
            match service::validate_uninstall_owner(&candidate) {
                Ok(()) => matched.push(candidate),
                Err(error) => failures.push(format!("{} ({error:#})", candidate.display())),
            }
        }
        match matched.as_slice() {
            [owner] => {
                self.service_executable = owner.clone();
                Ok(())
            }
            [] => anyhow::bail!(
                "daemon service owner could not be classified among the exact lifecycle paths: {}",
                failures.join("; ")
            ),
            owners => anyhow::bail!(
                "daemon service definition matched multiple executable owners during transition: {}",
                owners
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn release_absence_for_restart(&mut self) -> Result<()> {
        let absence_lease = self
            .absence_lease
            .take()
            .context("daemon absence lease was not held before installer restart")?;
        absence_lease.verify()?;
        drop(absence_lease);
        Ok(())
    }

    /// Publish the installer-selected executable as the daemon generation
    /// without releasing the service-operation lease. A running daemon owns
    /// the PID lock before this returns; an initially stopped daemon retains
    /// the absence lease instead.
    fn restart_after_installer_update(
        &mut self,
        replacement_executable: &std::path::Path,
    ) -> Result<()> {
        if self.uninstall_state.is_applied() {
            anyhow::bail!("an applied installer uninstall cannot enter replacement restart");
        }
        service::validate_expected_executable(replacement_executable)?;
        let replacement_token = crate::fs_ops::token_for_path(replacement_executable)
            .with_context(|| {
                format!(
                    "binding the installer-published replacement executable {}",
                    replacement_executable.display()
                )
            })?;
        self.executable = replacement_executable.to_path_buf();
        self.expected_executable_token = Some(replacement_token.clone());
        if self.service_installed
            && self.service_executable != replacement_executable
            && self.initial_launch_mechanism != InitialLaunchMechanism::ServiceManager
        {
            anyhow::bail!(
                "an inactive daemon service owned by {} cannot be migrated to {} while preserving the captured detached/stopped launch state; uninstall that service before migrating the executable",
                self.service_executable.display(),
                replacement_executable.display()
            );
        }
        if self.initial_pid.is_none() {
            return self.verify_final_state();
        }
        if self.restarted_pid.is_some() {
            return self.verify_final_state();
        }

        #[cfg(target_os = "windows")]
        if self.service_installed && self.service_executable != replacement_executable {
            anyhow::bail!(
                "Windows scheduled-task executable migration from {} to {} is not supported; the daemon remains stopped under the PID absence lease",
                self.service_executable.display(),
                replacement_executable.display()
            );
        }
        self.release_absence_for_restart()?;
        let previous_service_executable = self.service_executable.clone();
        let start_result = if self.initial_launch_mechanism
            == InitialLaunchMechanism::ServiceManager
        {
            #[cfg(target_os = "windows")]
            {
                service::start_installed_locked(&previous_service_executable, &self.service_lease)
            }
            #[cfg(not(target_os = "windows"))]
            {
                if previous_service_executable == replacement_executable {
                    service::start_installed_locked(
                        &previous_service_executable,
                        &self.service_lease,
                    )
                } else {
                    service::install_for_executable_locked(
                        replacement_executable,
                        Some(&previous_service_executable),
                        &self.service_lease,
                    )
                }
            }
        } else {
            start_detached_executable_locked(replacement_executable, &self.service_lease)
        };
        if let Err(error) = start_result {
            if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager
                && previous_service_executable != replacement_executable
            {
                self.classify_service_executable(replacement_executable)?;
            }
            return Err(error);
        }
        if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager {
            self.service_executable = replacement_executable.to_path_buf();
        }
        let restarted_pid = require_running_daemon_executable(
            replacement_executable,
            "installer replacement daemon did not retain its authoritative PID identity",
        )?;
        let restarted_token = crate::fs_ops::token_for_path(replacement_executable)?;
        if restarted_token != replacement_token {
            anyhow::bail!(
                "replacement executable identity changed while its daemon generation started: {}",
                replacement_executable.display()
            );
        }
        self.record_expected_service_snapshot_after_restart(restarted_pid)?;
        self.restarted_pid = Some(restarted_pid);
        self.verify_final_state()
    }

    /// Restore the original executable/service owner after the shell has
    /// rolled back its file and PATH transaction. The absence lease remains
    /// held until this exact restart boundary.
    fn restart_after_installer_rollback(
        &mut self,
        replacement_executable: &std::path::Path,
    ) -> Result<()> {
        if self.uninstall_state.is_applied() {
            anyhow::bail!(
                "an applied installer uninstall cannot be rolled back after its ready boundary"
            );
        }
        self.verify_initial_executable_contents_restored()?;
        self.executable = self.initial_executable.clone();
        self.expected_executable_token = crate::fs_ops::token_if_present(&self.initial_executable)?;
        self.expected_service_snapshot = self.initial_service_snapshot.clone();
        if self.initial_pid.is_none() {
            return self.verify_final_state();
        }
        if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager {
            self.classify_service_executable(replacement_executable)?;
        }
        #[cfg(target_os = "windows")]
        if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager
            && self.service_executable != self.initial_executable
        {
            anyhow::bail!(
                "Windows scheduled-task executable owner changed from {} to {}; automatic path migration rollback is not supported and daemon absence remains held",
                self.initial_executable.display(),
                self.service_executable.display()
            );
        }
        self.release_absence_for_restart()?;
        let restart_result = if self.initial_launch_mechanism
            == InitialLaunchMechanism::ServiceManager
        {
            #[cfg(target_os = "windows")]
            {
                service::start_installed_locked(&self.initial_executable, &self.service_lease)
            }
            #[cfg(not(target_os = "windows"))]
            {
                if self.service_executable == self.initial_executable {
                    service::start_installed_locked(&self.initial_executable, &self.service_lease)
                } else {
                    service::install_for_executable_locked(
                        &self.initial_executable,
                        Some(&self.service_executable),
                        &self.service_lease,
                    )
                }
            }
        } else {
            start_detached_executable_locked(&self.initial_executable, &self.service_lease)
        };
        restart_result
            .context("restoring the original daemon generation after installer rollback")?;
        self.service_executable = self.initial_executable.clone();
        let restarted_pid = require_running_daemon_executable(
            &self.initial_executable,
            "restored daemon did not retain its authoritative PID identity",
        )?;
        self.record_expected_service_snapshot_after_restart(restarted_pid)?;
        self.restarted_pid = Some(restarted_pid);
        self.verify_final_state()
    }

    fn restore_installer_prior_state(
        &mut self,
        replacement_executable: &std::path::Path,
    ) -> Result<InstallerRollbackTransition> {
        match self.restart_after_installer_rollback(replacement_executable) {
            Ok(()) => Ok(InstallerRollbackTransition::Restored),
            Err(restart_error) => {
                if self.initial_launch_mechanism == InitialLaunchMechanism::ServiceManager {
                    let initial_service_executable = self.initial_executable.clone();
                    if let Err(classification_error) =
                        self.classify_service_executable(&initial_service_executable)
                    {
                        return Err(restart_error.context(format!(
                            "original daemon restart failed and the stopped service owner could not be classified: {classification_error:#}"
                        )));
                    }
                }
                match self.reestablish_absence_after_failed_restart_with_alternative(Some(
                    replacement_executable,
                )) {
                    Ok(()) => Ok(InstallerRollbackTransition::FailedStopped(restart_error)),
                    Err(stop_error) => Err(restart_error.context(format!(
                        "original daemon restart failed and daemon absence could not be re-established: {stop_error:#}"
                    ))),
                }
            }
        }
    }

    fn prepare_installer_uninstall(&mut self) -> Result<InstallerUninstallTransition> {
        if self.uninstall_state != InstallerUninstallState::NotApplied {
            anyhow::bail!("installer uninstall state was already prepared");
        }
        self.absence_lease
            .as_ref()
            .context("daemon absence lease was not held before installer uninstall")?
            .verify()?;
        if pidfile::running_pid_checked()?.is_some() {
            anyhow::bail!("a daemon generation appeared before installer uninstall");
        }

        let prior = self.stopped_service_snapshot.as_ref().context(
            "exact stopped service snapshot was not retained before installer uninstall",
        )?;
        let outcome = if self.service_installed {
            service::uninstall_locked_exact(
                &self.service_executable,
                false,
                prior,
                &self.service_lease,
            )?
        } else {
            service::require_service_state_snapshot(&self.service_executable, prior)?;
            service::ExactServiceUninstallOutcome::Applied {
                operation_error: None,
            }
        };

        match outcome {
            service::ExactServiceUninstallOutcome::Applied { operation_error } => {
                // The typed service result proves removal before any later
                // probe. Preserve that fact even when final verification or
                // the underlying operation's response is fallible.
                let absence_lease = self.absence_lease.as_ref();
                let service_executable = self.service_executable.clone();
                self.uninstall_state.record_applied_and_verify(|| {
                    Self::verify_uninstalled_boundary_state_with_lease(
                        &service_executable,
                        absence_lease,
                    )
                })?;
                if let Some(operation_error) = operation_error {
                    return Err(operation_error.context(
                        "installer service uninstall reported an error after exact removal was observed; the applied state remains held for final verification",
                    ));
                }
                self.uninstall_state.mark_ready();
                Ok(InstallerUninstallTransition::Ready)
            }
            service::ExactServiceUninstallOutcome::AppliedPendingVerification {
                post_state_error,
            } => {
                self.uninstall_state.mark_applied();
                Err(post_state_error.context(
                    "service removal was applied before its exact post-state probe failed; the uninstall-applied state and lifecycle authorities were retained",
                ))
            }
            service::ExactServiceUninstallOutcome::PriorExact { operation_error } => {
                self.verify_stopped_boundary_state()?;
                Ok(InstallerUninstallTransition::RestoredStopped(
                    operation_error,
                ))
            }
        }
    }

    fn verify_uninstalled_boundary_state(&self) -> Result<()> {
        Self::verify_uninstalled_boundary_state_with_lease(
            &self.service_executable,
            self.absence_lease.as_ref(),
        )
    }

    fn verify_uninstalled_boundary_state_with_lease(
        service_executable: &std::path::Path,
        absence_lease: Option<&pidfile::DaemonAbsenceLease>,
    ) -> Result<()> {
        service::require_service_absent_state(service_executable)?;
        absence_lease
            .context("daemon absence lease was lost during installer uninstall")?
            .verify()?;
        if let Some(pid) = pidfile::running_pid_checked()? {
            anyhow::bail!("daemon PID {pid} appeared after installer service removal");
        }
        Ok(())
    }

    fn classify_uninstall_boundary_state(&mut self) -> Result<InstallerUninstallBoundaryState> {
        if self.uninstall_state.is_applied() {
            self.verify_uninstalled_boundary_state()?;
            return Ok(InstallerUninstallBoundaryState::Applied);
        }

        let prior = self
            .stopped_service_snapshot
            .as_ref()
            .context("uninstall finalization has no exact prior stopped snapshot")?;
        match service::classify_current_service_state(&self.service_executable, prior)? {
            service::ExactServiceBoundaryState::Absent => {
                let absence_lease = self.absence_lease.as_ref();
                let service_executable = self.service_executable.clone();
                self.uninstall_state.record_applied_and_verify(|| {
                    Self::verify_uninstalled_boundary_state_with_lease(
                        &service_executable,
                        absence_lease,
                    )
                })?;
                Ok(InstallerUninstallBoundaryState::Applied)
            }
            service::ExactServiceBoundaryState::PriorExact => {
                self.verify_stopped_boundary_state()?;
                Ok(InstallerUninstallBoundaryState::PriorStopped)
            }
            service::ExactServiceBoundaryState::Ambiguous => anyhow::bail!(
                "installer uninstall left a service definition/runtime state that matched neither exact removal nor the exact prior stopped snapshot"
            ),
        }
    }

    fn verify_stopped_boundary_state(&self) -> Result<()> {
        let expected = self
            .stopped_service_snapshot
            .as_ref()
            .context("stopped installer boundary has no exact service snapshot")?;
        service::require_service_state_snapshot(&self.service_executable, expected)?;
        if let Some(pid) = pidfile::running_pid_checked()? {
            anyhow::bail!(
                "daemon PID {pid} still owns the PID authority at a stopped installer boundary"
            );
        }
        self.absence_lease
            .as_ref()
            .context("stopped installer boundary lost its daemon absence lease")?
            .verify()
    }

    fn finalize_installer_boundary_error(
        &mut self,
        phase: InstallerBoundaryPhase,
        replacement_executable: &std::path::Path,
        protocol_error: anyhow::Error,
    ) -> anyhow::Error {
        match installer_boundary_finalization(phase) {
            InstallerBoundaryFinalization::RestoreFailedStop => {
                match self.restore_after_failed_stop() {
                    Ok(FailedStopRestoration::Exact) => protocol_error.context(
                        "installer daemon-boundary stop failed or unwound; the exact prior daemon state was restored before authority was released",
                    ),
                    Ok(FailedStopRestoration::ExactWithDiagnostics(diagnostics)) => {
                        protocol_error.context(format!(
                            "installer daemon-boundary stop failed or unwound; the exact prior daemon state was restored before authority was released, but historical lifecycle observation diagnostics remain: {diagnostics:#}"
                        ))
                    }
                    Err(restoration_error) => protocol_error.context(format!(
                        "installer daemon-boundary stop failed or unwound and exact prior-state restoration was incomplete: {restoration_error:#}"
                    )),
                }
            }
            InstallerBoundaryFinalization::RestorePrior => {
                match self.restore_installer_prior_state(replacement_executable) {
                    Ok(InstallerRollbackTransition::Restored) => protocol_error.context(
                        "installer daemon-boundary protocol failed; the exact prior daemon state was restored before authority was released",
                    ),
                    Ok(InstallerRollbackTransition::FailedStopped(restart_error)) => {
                        protocol_error.context(format!(
                            "installer daemon-boundary protocol failed; prior restart also failed, but exact daemon absence was re-established before authority was released: {restart_error:#}"
                        ))
                    }
                    Err(restoration_error) => protocol_error.context(format!(
                        "installer daemon-boundary protocol failed and exact prior-state restoration could not be classified safely: {restoration_error:#}"
                    )),
                }
            }
            InstallerBoundaryFinalization::ReestablishStopped => {
                match self.reestablish_absence_after_failed_restart_with_alternative(Some(
                    replacement_executable,
                )) {
                    Ok(()) => protocol_error.context(format!(
                        "installer daemon-boundary protocol failed or unwound during {phase:?}; the exact observed generation was stopped and daemon absence was re-established before authority was released"
                    )),
                    Err(state_error) => protocol_error.context(format!(
                        "installer daemon-boundary protocol failed or unwound during {phase:?}, and exact daemon absence could not be re-established safely: {state_error:#}"
                    )),
                }
            }
            InstallerBoundaryFinalization::VerifyStopped => {
                match self.verify_stopped_boundary_state() {
                    Ok(()) => protocol_error.context(format!(
                        "installer daemon-boundary protocol failed during {phase:?}; exact stopped service state and daemon absence were verified before authority was released"
                    )),
                    Err(state_error) => protocol_error.context(format!(
                        "installer daemon-boundary protocol failed during {phase:?}, and the retained stopped state could not be classified safely: {state_error:#}"
                    )),
                }
            }
            InstallerBoundaryFinalization::ClassifyUninstall => {
                match self.classify_uninstall_boundary_state() {
                    Ok(InstallerUninstallBoundaryState::Applied) => protocol_error.context(
                        "installer daemon-boundary protocol failed during uninstall; exact service removal and daemon absence were verified before authority was released",
                    ),
                    Ok(InstallerUninstallBoundaryState::PriorStopped) => {
                        protocol_error.context(
                            "installer daemon-boundary protocol failed during uninstall; the exact prior stopped service state and daemon absence were verified before authority was released",
                        )
                    }
                    Err(state_error) => protocol_error.context(format!(
                        "installer daemon-boundary protocol failed during uninstall, and neither the exact prior stopped state nor the exact applied removal could be classified safely: {state_error:#}"
                    )),
                }
            }
            InstallerBoundaryFinalization::VerifyFinal => match self.verify_final_state() {
                Ok(()) => protocol_error.context(format!(
                    "installer daemon-boundary protocol failed during {phase:?}; its exact final daemon/service state was revalidated before authority was released"
                )),
                Err(state_error) if !self.uninstall_state.is_applied() => {
                    match self.verify_stopped_boundary_state() {
                        Ok(()) => protocol_error.context(format!(
                            "installer daemon-boundary protocol failed during {phase:?}; final-state verification also failed, but the exact failed generation was stopped and daemon absence was re-established before authority was released: {state_error:#}"
                        )),
                        Err(stopped_error) => protocol_error.context(format!(
                            "installer daemon-boundary protocol failed during {phase:?}, final-state verification failed, and neither the final nor a safe stopped state could be classified: {state_error:#}; stopped-state verification: {stopped_error:#}"
                        )),
                    }
                }
                Err(state_error) => protocol_error.context(format!(
                    "installer daemon-boundary protocol failed during {phase:?}, and its uninstall final state could not be classified safely: {state_error:#}"
                )),
            },
        }
    }

    /// Revalidate the exact final process/service state immediately before the
    /// executable transaction is committed and its recovery copies are
    /// removed.
    pub fn verify_final_state(&mut self) -> Result<()> {
        let verification = self.verify_final_state_snapshot();
        let Err(verification_error) = verification else {
            return Ok(());
        };
        if self.uninstall_state.is_applied() {
            return Err(verification_error);
        }
        match self.reestablish_absence_after_failed_restart_with_alternative(None) {
            Ok(()) => Err(verification_error.context(
                "final daemon-state verification failed; the exact observed generation was stopped and daemon absence was re-established",
            )),
            Err(stop_error) => Err(verification_error.context(format!(
                "final daemon-state verification failed and the observed generation could not be proven stopped: {stop_error:#}"
            ))),
        }
    }

    fn verify_final_state_snapshot(&self) -> Result<()> {
        if self.uninstall_state.is_applied() {
            service::require_service_absent_state(&self.service_executable)?;
            if let Some(pid) = pidfile::running_pid_checked()? {
                anyhow::bail!("daemon PID {pid} appeared before installer uninstall commit");
            }
            return self
                .absence_lease
                .as_ref()
                .context("installer uninstall lost its daemon absence lease")?
                .verify();
        }

        match &self.expected_executable_token {
            Some(expected) => {
                let observed =
                    crate::fs_ops::token_for_path(&self.executable).with_context(|| {
                        format!(
                            "binding the executable at the final daemon boundary: {}",
                            self.executable.display()
                        )
                    })?;
                if &observed != expected {
                    anyhow::bail!(
                        "executable identity changed before the final daemon boundary: {}",
                        self.executable.display()
                    );
                }
            }
            None => {
                if crate::fs_ops::token_if_present(&self.executable)?.is_some() {
                    anyhow::bail!(
                        "an executable appeared at the final initially-absent path: {}",
                        self.executable.display()
                    );
                }
            }
        }

        service::require_service_state_snapshot(
            &self.service_executable,
            &self.expected_service_snapshot,
        )?;

        match self.initial_pid {
            Some(_) => {
                let expected_pid = self
                    .restarted_pid
                    .context("the initially running daemon has not been restarted")?;
                let (observed_pid, observed_executable) = pidfile::running_identity_checked()?
                    .context("the restarted daemon no longer owns a PID identity")?;
                if observed_pid != expected_pid {
                    anyhow::bail!(
                        "restarted daemon PID identity changed before self-update commit (expected {expected_pid}, observed {observed_pid})"
                    );
                }
                validate_running_daemon_executable(
                    &self.executable,
                    observed_pid,
                    &observed_executable,
                )
                .context("restarted daemon executable identity changed before commit")?;
                Ok(())
            }
            None => self
                .absence_lease
                .as_ref()
                .context("initially stopped daemon lost its absence lease")?
                .verify(),
        }
    }
}

fn wait_for_contending_daemon_resolution(
    expected_stale_generation: Option<&pidfile::DaemonGeneration>,
) -> InstallerContenderSettlement<pidfile::DaemonAbsenceLease> {
    let started = std::time::Instant::now();
    wait_for_contending_daemon_resolution_with(
        || pidfile::try_acquire_daemon_absence_lease(expected_stale_generation),
        || pidfile::contending_daemon_identity(expected_stale_generation),
        || started.elapsed(),
        std::thread::sleep,
        |elapsed| {
            eprintln!(
                "A foreground daemon still owns the PID authority {elapsed:.0}s into lifecycle settlement; authority remains held and no process mutation is retried"
            );
        },
    )
}

fn wait_for_contending_daemon_resolution_with<Lease, Acquire, Observe, Elapsed, Sleep, Report>(
    mut acquire: Acquire,
    mut observe: Observe,
    mut elapsed: Elapsed,
    mut sleep: Sleep,
    mut report: Report,
) -> InstallerContenderSettlement<Lease>
where
    Acquire: FnMut() -> Result<pidfile::DaemonAbsenceAcquireFor<Lease>>,
    Observe: FnMut() -> Result<pidfile::ContendingDaemonIdentity>,
    Elapsed: FnMut() -> std::time::Duration,
    Sleep: FnMut(std::time::Duration),
    Report: FnMut(f64),
{
    let mut diagnostics = TransientDiagnostics::default();
    let mut next_diagnostic = GENERATION_SETTLE_DIAGNOSTIC_INTERVAL;
    loop {
        match acquire() {
            Ok(pidfile::DaemonAbsenceAcquireFor::Acquired(lease)) => {
                return InstallerContenderSettlement {
                    resolution: InstallerContenderResolution::Absence(lease),
                    diagnostics: diagnostics.into_messages(),
                };
            }
            Ok(pidfile::DaemonAbsenceAcquireFor::Contended) => {}
            Err(error) => {
                let diagnostic = format!(
                    "transiently failed to acquire the daemon absence lease while a foreground contender was settling: {error:#}"
                );
                eprintln!("{diagnostic}; lifecycle authority remains held");
                diagnostics.record(diagnostic);
            }
        }
        match observe() {
            Ok(pidfile::ContendingDaemonIdentity::Published(pid)) => {
                return InstallerContenderSettlement {
                    resolution: InstallerContenderResolution::Published(pid),
                    diagnostics: diagnostics.into_messages(),
                };
            }
            Ok(pidfile::ContendingDaemonIdentity::Pending) => {}
            Err(error) => {
                let diagnostic = format!(
                    "transiently failed to inspect the foreground daemon contender: {error:#}"
                );
                eprintln!("{diagnostic}; lifecycle authority remains held");
                diagnostics.record(diagnostic);
            }
        }
        let current_elapsed = elapsed();
        if current_elapsed >= next_diagnostic {
            report(current_elapsed.as_secs_f64());
            next_diagnostic = current_elapsed.saturating_add(GENERATION_SETTLE_DIAGNOSTIC_INTERVAL);
        }
        sleep(std::time::Duration::from_millis(25));
    }
}

fn run_installer_daemon_boundary_protocol<R: std::io::BufRead, W: std::io::Write>(
    transaction: &mut SelfUpdateDaemonRestart,
    replacement_executable: &std::path::Path,
    uninstall_paths_match: bool,
    ready_nonce: Option<&str>,
    input: &mut R,
    output: &mut W,
    phase: &mut InstallerBoundaryPhase,
) -> Result<()> {
    let initial_running = transaction.initial_pid.is_some();
    let initial_service_installed = transaction.service_installed;
    let ready_state = if let Some(ready_nonce) = ready_nonce {
        format!(
            "ready {ready_nonce} {}",
            installer_state_line(initial_running, initial_service_installed)
        )
    } else {
        format!(
            "ready {}",
            installer_state_line(initial_running, initial_service_installed)
        )
    };
    publish_installer_daemon_boundary_state(
        output,
        phase,
        InstallerBoundaryPhase::Stopped,
        &ready_state,
    )?;

    loop {
        let mut command = String::new();
        if input
            .read_line(&mut command)
            .context("reading installer daemon-boundary command")?
            == 0
        {
            anyhow::bail!(
                "installer daemon-boundary control channel closed before an explicit final state acknowledgement"
            );
        }
        let command = command.trim_end_matches(['\r', '\n']);
        match (command, *phase) {
            ("new", InstallerBoundaryPhase::Stopped) => {
                *phase = InstallerBoundaryPhase::ReplacementTransition;
                match transaction.restart_after_installer_update(replacement_executable) {
                    Ok(()) => {
                        publish_installer_daemon_boundary_state(
                            output,
                            phase,
                            InstallerBoundaryPhase::NewReady,
                            DAEMON_BOUNDARY_NEW_READY,
                        )?;
                    }
                    Err(restart_error) => {
                        match transaction.reestablish_absence_after_failed_restart_with_alternative(
                            Some(replacement_executable),
                        ) {
                            Ok(()) => {
                                eprintln!(
                                    "Error: replacement daemon start failed and daemon absence was re-established: {restart_error:#}"
                                );
                                publish_installer_daemon_boundary_state(
                                    output,
                                    phase,
                                    InstallerBoundaryPhase::ReplacementFailedStopped,
                                    DAEMON_BOUNDARY_NEW_FAILED,
                                )?;
                            }
                            Err(stop_error) => {
                                return Err(restart_error.context(format!(
                                    "replacement daemon start failed and daemon absence could not be re-established: {stop_error:#}"
                                )));
                            }
                        }
                    }
                }
            }
            ("uninstall", InstallerBoundaryPhase::Stopped) => {
                if !uninstall_paths_match {
                    anyhow::bail!(
                        "installer uninstall requires identical initial and replacement executable paths"
                    );
                }
                *phase = InstallerBoundaryPhase::UninstallTransition;
                match transaction.prepare_installer_uninstall()? {
                    InstallerUninstallTransition::Ready => {
                        publish_installer_daemon_boundary_state(
                            output,
                            phase,
                            InstallerBoundaryPhase::UninstallReady,
                            "uninstall state ready",
                        )?;
                    }
                    InstallerUninstallTransition::RestoredStopped(uninstall_error) => {
                        eprintln!(
                            "Error: service uninstall failed; its exact definition was restored stopped and daemon absence remains held: {uninstall_error:#}"
                        );
                        publish_installer_daemon_boundary_state(
                            output,
                            phase,
                            InstallerBoundaryPhase::Stopped,
                            "uninstall state failed",
                        )?;
                    }
                }
            }
            (
                "rollback",
                InstallerBoundaryPhase::Stopped | InstallerBoundaryPhase::ReplacementFailedStopped,
            ) => {
                *phase = InstallerBoundaryPhase::RollbackTransition;
                match transaction.restore_installer_prior_state(replacement_executable)? {
                    InstallerRollbackTransition::Restored => {
                        publish_installer_daemon_boundary_state(
                            output,
                            phase,
                            InstallerBoundaryPhase::PriorRestored,
                            DAEMON_BOUNDARY_OLD_RESTORED,
                        )?;
                        return Ok(());
                    }
                    InstallerRollbackTransition::FailedStopped(restart_error) => {
                        eprintln!(
                            "Error: original daemon restart failed and daemon absence was re-established: {restart_error:#}"
                        );
                        publish_installer_daemon_boundary_state(
                            output,
                            phase,
                            InstallerBoundaryPhase::RollbackFailedStopped,
                            DAEMON_BOUNDARY_OLD_FAILED,
                        )?;
                    }
                }
            }
            ("finish", InstallerBoundaryPhase::NewReady) => {
                *phase = InstallerBoundaryPhase::ReplacementTransition;
                transaction.verify_final_state()?;
                publish_installer_daemon_boundary_state(
                    output,
                    phase,
                    InstallerBoundaryPhase::FinalConfirmed,
                    DAEMON_BOUNDARY_FINAL_CONFIRMED,
                )?;
            }
            ("abort", InstallerBoundaryPhase::NewReady) => {
                *phase = InstallerBoundaryPhase::ReplacementTransition;
                transaction.reestablish_absence_after_failed_restart_with_alternative(Some(
                    replacement_executable,
                ))?;
                publish_installer_daemon_boundary_state(
                    output,
                    phase,
                    InstallerBoundaryPhase::ReplacementFailedStopped,
                    DAEMON_BOUNDARY_NEW_STOPPED,
                )?;
            }
            ("abort", InstallerBoundaryPhase::FinalConfirmed) => {
                *phase = InstallerBoundaryPhase::ReplacementTransition;
                transaction.reestablish_absence_after_failed_restart_with_alternative(Some(
                    replacement_executable,
                ))?;
                publish_installer_daemon_boundary_state(
                    output,
                    phase,
                    InstallerBoundaryPhase::ReplacementFailedStopped,
                    DAEMON_BOUNDARY_NEW_STOPPED,
                )?;
            }
            ("release", InstallerBoundaryPhase::FinalConfirmed) => {
                transaction.verify_final_state()?;
                publish_installer_daemon_boundary_state(
                    output,
                    phase,
                    InstallerBoundaryPhase::FinalConfirmed,
                    DAEMON_BOUNDARY_AUTHORITY_RELEASED,
                )?;
                return Ok(());
            }
            ("finish", InstallerBoundaryPhase::UninstallReady) => {
                transaction.verify_final_state()?;
                publish_installer_daemon_boundary_state(
                    output,
                    phase,
                    InstallerBoundaryPhase::FinalConfirmed,
                    DAEMON_BOUNDARY_FINAL_CONFIRMED,
                )?;
            }
            _ => anyhow::bail!(
                "invalid installer daemon-boundary command '{command}' for the current transaction phase"
            ),
        }
    }
}

pub(crate) fn hold_installer_daemon_update_boundary(
    initial_executable: std::path::PathBuf,
    replacement_executable: std::path::PathBuf,
    expected_executable_token: Option<&str>,
    ready_nonce: Option<&str>,
) -> Result<()> {
    service::validate_expected_executable(&replacement_executable)?;
    match (expected_executable_token, ready_nonce) {
        (Some(expected_executable_token), Some(ready_nonce)) => {
            validate_background_ready_nonce(ready_nonce)?;
            let expected_executable_token = expected_executable_token
                .parse::<crate::fs_ops::FileToken>()
                .context("parsing self-update lifecycle holder executable token")?;
            let current_executable = std::env::current_exe()
                .context("locating the self-update lifecycle holder executable")?;
            let observed = crate::fs_ops::token_for_path(&current_executable)
                .context("binding the self-update lifecycle holder executable")?;
            if observed != expected_executable_token {
                anyhow::bail!(
                    "self-update lifecycle holder executable changed before readiness: {}",
                    current_executable.display()
                );
            }
        }
        (None, None) => {}
        _ => anyhow::bail!(
            "self-update lifecycle holder requires both executable token and readiness nonce"
        ),
    }
    let uninstall_paths_match = initial_executable == replacement_executable;
    let mut transaction = SelfUpdateDaemonRestart::capture_for_executable(initial_executable)?;
    let mut phase = InstallerBoundaryPhase::Stopping;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let boundary_result = catch_installer_boundary_unwind(|| {
        transaction.stop_before_update_inner()?;
        phase = InstallerBoundaryPhase::Stopped;
        run_installer_daemon_boundary_protocol(
            &mut transaction,
            &replacement_executable,
            uninstall_paths_match,
            ready_nonce,
            &mut input,
            &mut output,
            &mut phase,
        )
    });
    finalize_installer_boundary_result(boundary_result, |protocol_error| {
        transaction.finalize_installer_boundary_error(
            phase,
            &replacement_executable,
            protocol_error,
        )
    })
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
    let snapshot = if running { state::read()? } else { None };
    let service_installed = service::is_installed_checked()?;
    let cfg = &crate::config::get().daemon;

    if json {
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
                "poll_interval_secs": cfg.poll_interval_secs,
                "cache_refresh_interval_secs": cfg.cache_refresh_interval_secs,
                "auto_warmup": cfg.auto_warmup,
                "token_check_interval_secs": cfg.token_check_interval_secs,
                "switch_threshold": cfg.switch_threshold,
                "notify": cfg.notify,
                "defer_switch_while_codex_running": cfg.defer_switch_while_codex_running,
                "log_level": cfg.log_level,
            }
        }))?;
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
                        let err = bounded_status_last_error(err);
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
                        let remaining = until
                            .checked_sub(crate::auth::now_unix_secs()?)
                            .context("daemon backoff timestamp exceeds the signed time range")?;
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
    for line in status_metadata_lines(service_installed, cfg) {
        user_println(&line);
    }
    Ok(())
}

fn status_metadata_lines(
    service_installed: bool,
    cfg: &crate::config::DaemonConfig,
) -> Vec<String> {
    vec![
        format!(
            "Service: {} (manager: {})",
            if service_installed {
                "installed"
            } else {
                "not installed"
            },
            service_manager_name()
        ),
        "Config:".to_string(),
        format!("  poll_interval_secs: {}", cfg.poll_interval_secs),
        format!(
            "  cache_refresh_interval_secs: {}",
            cfg.cache_refresh_interval_secs
        ),
        format!("  auto_warmup: {}", cfg.auto_warmup),
        format!(
            "  token_check_interval_secs: {}",
            cfg.token_check_interval_secs
        ),
        format!("  switch_threshold: {}", cfg.switch_threshold),
        format!("  notify: {}", cfg.notify),
        format!(
            "  defer_switch_while_codex_running: {}",
            cfg.defer_switch_while_codex_running
        ),
        format!("  log_level: {}", cfg.log_level),
    ]
}

fn bounded_status_last_error(error: &str) -> String {
    crate::safe_text::bounded_terminal_text(error, STATUS_LAST_ERROR_MAX_CHARS)
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
mod startup_file_log_tests {
    use super::ensure_startup_file_logging;

    #[test]
    fn readiness_preserves_the_file_log_cause_and_stores_it_for_one_observer() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let occupied = root.path().join("occupied");
        std::fs::write(&occupied, b"not a directory").unwrap();
        let writer = crate::logging::FileLogWriter::lazy_for_directory(occupied);

        let error = ensure_startup_file_logging(&writer)
            .expect_err("daemon readiness must reject an unusable log directory");

        let detail = format!("{error:#}");
        assert!(detail.contains("initializing secure file logging before daemon readiness"));
        assert!(detail.contains("creating log directory"));
        assert!(writer.take_initialization_error().unwrap().is_some());
        assert_eq!(writer.take_initialization_error().unwrap(), None);
    }
}

#[cfg(test)]
mod status_text_tests {
    use super::{STATUS_LAST_ERROR_MAX_CHARS, bounded_status_last_error, status_metadata_lines};

    #[test]
    fn persisted_last_error_is_control_free_and_bounded_at_terminal_boundary() {
        let persisted = format!(
            "upstream\u{1b}]52;clipboard\u{7}\n{}",
            "x".repeat(STATUS_LAST_ERROR_MAX_CHARS + 100)
        );

        let rendered = bounded_status_last_error(&persisted);

        assert!(rendered.chars().all(|ch| !ch.is_control()));
        assert_eq!(rendered.chars().count(), STATUS_LAST_ERROR_MAX_CHARS);
        assert!(rendered.starts_with("upstream]52;clipboard"));
    }

    #[test]
    fn human_status_metadata_includes_service_manager_and_complete_config() {
        let cfg = crate::config::DaemonConfig::default();

        let lines = status_metadata_lines(false, &cfg);

        assert_eq!(
            lines[0],
            format!(
                "Service: not installed (manager: {})",
                super::service_manager_name()
            )
        );
        assert_eq!(lines[1], "Config:");
        assert!(lines.iter().any(|line| line == "  poll_interval_secs: 60"));
        assert!(
            lines
                .iter()
                .any(|line| line == "  defer_switch_while_codex_running: true")
        );
        assert!(lines.iter().any(|line| line == "  log_level: error"));
    }
}

#[cfg(all(test, windows))]
mod background_marker_tests {
    use super::*;
    use std::io::Write as _;

    const ROLE_ENV: &str = "CSGP_BACKGROUND_MARKER_TEST_ROLE";
    const TEST_NAME: &str =
        "daemon::background_marker_tests::bounded_marker_failures_terminate_and_reap_child";
    const NOMINAL_TRANSITION_TIMEOUT_REGRESSION: std::time::Duration =
        std::time::Duration::from_millis(500);

    fn spawn_fixture(role: &str) -> std::process::Child {
        let executable = std::env::current_exe().expect("locate unit-test executable");
        let mut command = std::process::Command::new(executable);
        command
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(ROLE_ENV, role)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        super::isolate_background_child_from_terminal_interrupt(&mut command);
        command.spawn().expect("spawn marker protocol fixture")
    }

    fn assert_reaped(child: &mut std::process::Child) {
        assert!(
            child.try_wait().expect("inspect marker fixture").is_some(),
            "marker protocol failure returned while its exact child remained live"
        );
    }

    #[test]
    fn bounded_marker_failures_terminate_and_reap_child() {
        match std::env::var(ROLE_ENV).as_deref() {
            Ok("silent") => {
                std::thread::sleep(std::time::Duration::from_secs(30));
                return;
            }
            Ok("oversized") => {
                println!("{}", "x".repeat(BACKGROUND_MARKER_LINE_MAX_BYTES + 1));
                std::io::stdout().flush().expect("flush oversized marker");
                std::thread::sleep(std::time::Duration::from_secs(30));
                return;
            }
            Ok("chatter") => {
                for _ in 0..256 {
                    println!("untrusted marker chatter");
                }
                std::io::stdout().flush().expect("flush marker chatter");
                std::thread::sleep(std::time::Duration::from_secs(30));
                return;
            }
            Ok("inherited-pipe-parent") => {
                let executable = std::env::current_exe().expect("locate pipe descendant");
                let mut descendant = std::process::Command::new(executable)
                    .arg(TEST_NAME)
                    .arg("--exact")
                    .arg("--nocapture")
                    .env(ROLE_ENV, "inherited-pipe-descendant")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn inherited-pipe descendant");
                let _descendant_reaper = std::thread::spawn(move || {
                    let _ = descendant.wait();
                });
                std::thread::sleep(std::time::Duration::from_secs(30));
                return;
            }
            Ok("inherited-pipe-descendant") => {
                std::thread::sleep(std::time::Duration::from_secs(3));
                return;
            }
            Ok("delayed-authority") => {
                std::thread::sleep(std::time::Duration::from_millis(750));
                println!("authority settled");
                std::io::stdout()
                    .flush()
                    .expect("flush delayed authority marker");
                return;
            }
            Ok(other) => panic!("unexpected background marker fixture role {other}"),
            Err(_) => {}
        }

        let mut silent = spawn_fixture("silent");
        let silent_stdout = silent.stdout.take().expect("silent fixture stdout");
        let mut silent_output = Some(std::io::BufReader::new(silent_stdout));
        let silent_error = loop {
            match super::read_background_marker_line(
                &mut silent,
                &mut silent_output,
                Some(std::time::Duration::from_millis(500)),
                "silent lifecycle marker fixture",
            ) {
                Ok(_) => continue,
                Err(error) => break error,
            }
        };
        assert!(
            format!("{silent_error:#}").contains("did not emit a complete marker"),
            "{silent_error:#}"
        );
        assert_reaped(&mut silent);

        let mut oversized = spawn_fixture("oversized");
        let oversized_stdout = oversized.stdout.take().expect("oversized fixture stdout");
        let mut oversized_output = Some(std::io::BufReader::new(oversized_stdout));
        let oversized_error = loop {
            match super::read_background_marker_line(
                &mut oversized,
                &mut oversized_output,
                Some(std::time::Duration::from_secs(10)),
                "malformed lifecycle marker fixture",
            ) {
                Ok(_) => continue,
                Err(error) => break error,
            }
        };
        assert!(
            format!("{oversized_error:#}").contains("marker line limit"),
            "{oversized_error:#}"
        );
        assert_reaped(&mut oversized);

        let mut chatter = spawn_fixture("chatter");
        let chatter_stdout = chatter.stdout.take().expect("chatter fixture stdout");
        let chatter_error = super::await_expected_background_marker(
            &mut chatter,
            chatter_stdout,
            "never ready",
            std::time::Duration::from_secs(10),
            1024,
            "chattering cleanup marker fixture",
        )
        .expect_err("marker chatter must hit the aggregate byte bound");
        assert!(
            format!("{chatter_error:#}").contains("total marker output limit"),
            "{chatter_error:#}"
        );
        assert_reaped(&mut chatter);

        let mut inherited_pipe = spawn_fixture("inherited-pipe-parent");
        let inherited_stdout = inherited_pipe
            .stdout
            .take()
            .expect("inherited-pipe fixture stdout");
        let mut inherited_output = Some(std::io::BufReader::new(inherited_stdout));
        let started = std::time::Instant::now();
        let inherited_error = loop {
            match super::read_background_marker_line(
                &mut inherited_pipe,
                &mut inherited_output,
                Some(std::time::Duration::from_millis(500)),
                "inherited-pipe marker fixture",
            ) {
                Ok(_) => continue,
                Err(error) => break error,
            }
        };
        assert!(
            format!("{inherited_error:#}").contains("did not emit a complete marker"),
            "{inherited_error:#}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "an inherited stdout handle defeated the marker deadline"
        );
        assert_reaped(&mut inherited_pipe);

        let mut delayed = spawn_fixture("delayed-authority");
        let delayed_stdout = delayed.stdout.take().expect("delayed fixture stdout");
        let mut delayed_output = Some(std::io::BufReader::new(delayed_stdout));
        let started = std::time::Instant::now();
        let marker = loop {
            let (marker, _) = super::read_background_marker_line(
                &mut delayed,
                &mut delayed_output,
                None,
                "trusted lifecycle authority fixture",
            )
            .expect("trusted lifecycle marker has no wall-clock deadline");
            if marker == "authority settled" {
                break marker;
            }
        };
        assert_eq!(marker, "authority settled");
        assert!(
            started.elapsed() > NOMINAL_TRANSITION_TIMEOUT_REGRESSION,
            "trusted lifecycle authority did not exercise a wait beyond the nominal transition timeout"
        );
        assert!(
            delayed
                .wait()
                .expect("reap delayed authority fixture")
                .success(),
            "delayed trusted authority fixture failed"
        );
    }
}

#[cfg(test)]
mod installer_state_tests {
    use super::{
        DAEMON_TRANSITION_TIMEOUT, FailedStopRestoration, InitialExecutableState,
        InitialGenerationStopState, InitialLaunchMechanism, InstallerBoundaryFinalization,
        InstallerBoundaryPhase, InstallerContenderResolution, InstallerUninstallState,
        RequestedGenerationObservation, catch_installer_boundary_unwind,
        classify_initial_launch_mechanism, finalize_installer_boundary_result,
        finalize_prior_restart_restoration, installer_boundary_finalization, installer_state_line,
        publish_installer_daemon_boundary_state, require_captured_executable_state,
        validate_running_daemon_executable, wait_for_contending_daemon_resolution_with,
        wait_for_requested_generation_stop_with, wait_for_requested_generation_to_settle_with,
    };

    struct BrokenMarkerWriter;

    impl std::io::Write for BrokenMarkerWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture closed the marker pipe",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture closed the marker pipe",
            ))
        }
    }

    #[test]
    fn exact_prior_restart_is_not_stopped_again_for_historical_diagnostics() {
        use std::cell::Cell;

        let diagnostic_checks = Cell::new(0usize);
        let absence_reestablishments = Cell::new(0usize);
        let restoration = finalize_prior_restart_restoration(
            &mut (),
            Ok(()),
            |_| {
                diagnostic_checks.set(diagnostic_checks.get() + 1);
                Err(anyhow::anyhow!(
                    "injected earlier stop-observation diagnostic"
                ))
            },
            |_| {
                absence_reestablishments.set(absence_reestablishments.get() + 1);
                Ok(())
            },
        )
        .expect("historical diagnostics must not erase exact restoration");

        assert_eq!(diagnostic_checks.get(), 1);
        assert_eq!(
            absence_reestablishments.get(),
            0,
            "an exactly restored prior daemon must not be stopped again"
        );
        let FailedStopRestoration::ExactWithDiagnostics(error) = restoration else {
            panic!("the exact restoration must retain its historical diagnostic")
        };
        let message = format!("{error:#}");
        assert!(message.contains("remains running"), "{message}");
        assert!(message.contains("earlier stop-observation"), "{message}");
    }

    #[test]
    fn bounded_generation_stop_rechecks_a_transient_cleanup_observation() {
        use std::cell::{Cell, RefCell};
        use std::collections::VecDeque;

        let samples = RefCell::new(VecDeque::from([
            Ok(RequestedGenerationObservation::TargetRunning),
            Err(anyhow::anyhow!(
                "injected held-lock identity-removal transition"
            )),
            Ok(RequestedGenerationObservation::Settled(None)),
        ]));
        let elapsed = Cell::new(std::time::Duration::ZERO);
        wait_for_requested_generation_stop_with(
            4242,
            || samples.borrow_mut().pop_front().unwrap(),
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        )
        .expect("the exact stop must settle after the PID cleanup transition");

        assert!(samples.borrow().is_empty());
        assert_eq!(elapsed.get(), std::time::Duration::from_millis(200));
    }

    #[test]
    fn bounded_generation_stop_retains_transient_diagnostics_at_its_deadline() {
        let error = wait_for_requested_generation_stop_with(
            4242,
            || Err(anyhow::anyhow!("injected unreadable PID identity")),
            || DAEMON_TRANSITION_TIMEOUT,
            |_| panic!("a reached deadline must not sleep"),
        )
        .expect_err("an unclassified daemon state must never count as stopped");
        let message = format!("{error:#}");

        assert!(message.contains("Daemon did not stop within"), "{message}");
        assert!(
            message.contains("injected unreadable PID identity"),
            "{message}"
        );
    }

    #[test]
    fn bounded_generation_stop_rejects_a_successor_immediately() {
        let error = wait_for_requested_generation_stop_with(
            4242,
            || {
                Ok(RequestedGenerationObservation::Settled(Some(
                    crate::daemon::pidfile::DaemonGeneration {
                        pid: 4242,
                        generation: "successor".to_string(),
                    },
                )))
            },
            || panic!("a classified successor must return immediately"),
            |_| panic!("a classified successor must not sleep"),
        )
        .expect_err("a successor generation must not be reported as stopped");
        let message = format!("{error:#}");

        assert!(message.contains("different generation"), "{message}");
        assert!(message.contains("current PID 4242"), "{message}");
    }

    #[test]
    fn requested_generation_settle_retains_authority_past_every_transition_deadline() {
        use std::cell::{Cell, RefCell};
        use std::collections::VecDeque;

        let samples = RefCell::new(VecDeque::from([
            Ok(RequestedGenerationObservation::TargetRunning),
            Err(anyhow::anyhow!("injected transient PID probe failure")),
            Ok(RequestedGenerationObservation::TargetRunning),
            Ok(RequestedGenerationObservation::TargetRunning),
            Ok(RequestedGenerationObservation::Settled(None)),
        ]));
        let observations = Cell::new(0usize);
        let elapsed = Cell::new(std::time::Duration::ZERO);
        let diagnostics = RefCell::new(Vec::new());
        let settled = wait_for_requested_generation_to_settle_with(
            4242,
            || {
                observations.set(observations.get() + 1);
                samples.borrow_mut().pop_front().unwrap()
            },
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration + DAEMON_TRANSITION_TIMEOUT),
            |seconds| diagnostics.borrow_mut().push(seconds),
        );

        assert_eq!(settled.observed_generation, None);
        assert_eq!(settled.diagnostics.len(), 1);
        assert!(settled.diagnostics[0].contains("injected transient PID probe failure"));
        assert_eq!(observations.get(), 5);
        assert!(elapsed.get() > DAEMON_TRANSITION_TIMEOUT * 2);
        assert!(diagnostics.borrow().len() >= 2);
    }

    #[test]
    fn requested_generation_settle_returns_only_after_the_exact_generation_changes() {
        use std::cell::RefCell;
        use std::collections::VecDeque;

        let samples = RefCell::new(VecDeque::from([
            Ok(RequestedGenerationObservation::TargetRunning),
            Ok(RequestedGenerationObservation::TargetRunning),
            Ok(RequestedGenerationObservation::Settled(Some(
                crate::daemon::pidfile::DaemonGeneration {
                    pid: 9001,
                    generation: "successor".to_string(),
                },
            ))),
        ]));
        let settled = wait_for_requested_generation_to_settle_with(
            4242,
            || samples.borrow_mut().pop_front().unwrap(),
            std::time::Duration::default,
            |_| {},
            |_| {},
        );

        assert_eq!(
            settled
                .observed_generation
                .as_ref()
                .map(crate::daemon::pidfile::DaemonGeneration::pid),
            Some(9001)
        );
        assert!(settled.diagnostics.is_empty());
        assert!(samples.borrow().is_empty());
    }

    #[test]
    fn contender_settlement_reacquires_absence_after_prepublication_exit() {
        use std::cell::{Cell, RefCell};
        use std::collections::VecDeque;

        let acquisitions = RefCell::new(VecDeque::from([
            Err(anyhow::anyhow!("injected transient acquire failure")),
            Ok(crate::daemon::pidfile::DaemonAbsenceAcquireFor::Contended),
            Ok(crate::daemon::pidfile::DaemonAbsenceAcquireFor::Acquired(
                "exact absence lease",
            )),
        ]));
        let identities = RefCell::new(VecDeque::from([
            Ok(crate::daemon::pidfile::ContendingDaemonIdentity::Pending),
            Ok(crate::daemon::pidfile::ContendingDaemonIdentity::Pending),
        ]));
        let elapsed = Cell::new(std::time::Duration::ZERO);
        let settlement = wait_for_contending_daemon_resolution_with(
            || acquisitions.borrow_mut().pop_front().unwrap(),
            || identities.borrow_mut().pop_front().unwrap(),
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration + DAEMON_TRANSITION_TIMEOUT),
            |_| {},
        );

        match settlement.resolution {
            InstallerContenderResolution::Absence(lease) => {
                assert_eq!(lease, "exact absence lease")
            }
            InstallerContenderResolution::Published(_) => {
                panic!("a contender that exited before publication must yield exact absence")
            }
        }
        assert_eq!(settlement.diagnostics.len(), 1);
        assert!(settlement.diagnostics[0].contains("transient acquire failure"));
        assert!(acquisitions.borrow().is_empty());
        assert!(identities.borrow().is_empty());
        assert!(elapsed.get() > DAEMON_TRANSITION_TIMEOUT);
    }

    #[test]
    fn contender_settlement_returns_the_exact_published_generation() {
        use std::cell::RefCell;
        use std::collections::VecDeque;

        let target = crate::daemon::pidfile::DaemonGeneration {
            pid: 4242,
            generation: "published-generation".to_string(),
        };
        let identities = RefCell::new(VecDeque::from([
            Err(anyhow::anyhow!("injected transient identity failure")),
            Ok(crate::daemon::pidfile::ContendingDaemonIdentity::Published(
                target.clone(),
            )),
        ]));
        let settlement = wait_for_contending_daemon_resolution_with(
            || {
                Ok::<_, anyhow::Error>(
                    crate::daemon::pidfile::DaemonAbsenceAcquireFor::<()>::Contended,
                )
            },
            || identities.borrow_mut().pop_front().unwrap(),
            std::time::Duration::default,
            |_| {},
            |_| {},
        );

        match settlement.resolution {
            InstallerContenderResolution::Published(observed) => assert_eq!(observed, target),
            InstallerContenderResolution::Absence(()) => {
                panic!("a published contender must retain its exact generation")
            }
        }
        assert_eq!(settlement.diagnostics.len(), 1);
        assert!(settlement.diagnostics[0].contains("transient identity failure"));
        assert!(identities.borrow().is_empty());
    }

    #[test]
    fn launch_mechanism_comes_from_exact_manager_ownership_not_installation() {
        assert_eq!(
            classify_initial_launch_mechanism(None, None).unwrap(),
            InitialLaunchMechanism::Stopped
        );
        assert_eq!(
            classify_initial_launch_mechanism(Some(42), None).unwrap(),
            InitialLaunchMechanism::Detached,
            "an installed but inactive manager must not claim a foreground generation"
        );
        assert_eq!(
            classify_initial_launch_mechanism(Some(42), Some(42)).unwrap(),
            InitialLaunchMechanism::ServiceManager
        );
        assert!(classify_initial_launch_mechanism(None, Some(42)).is_err());
        assert!(classify_initial_launch_mechanism(Some(42), Some(43)).is_err());
    }

    #[test]
    fn initially_stopped_transaction_keeps_the_exact_executable_identity() {
        let home = crate::fs_ops::create_direct_tempdir().expect("temp stopped executable state");
        let executable = home.path().join(if cfg!(windows) {
            "codex-switch-global-pace.exe"
        } else {
            "codex-switch-global-pace"
        });
        std::fs::write(&executable, b"captured executable").expect("write captured executable");
        let captured = crate::fs_ops::token_for_path(&executable).expect("capture exact token");
        let state = InitialExecutableState::Present(captured);

        require_captured_executable_state(&executable, &state, false)
            .expect("unchanged stopped executable must verify");
        std::fs::write(&executable, b"different executable").expect("replace executable contents");
        require_captured_executable_state(&executable, &state, false)
            .expect_err("rollback/final verification must reject changed stopped bytes");
        std::fs::remove_file(&executable).expect("remove changed executable");
        require_captured_executable_state(&executable, &state, false)
            .expect_err("rollback/final verification must reject a missing stopped executable");
    }

    #[test]
    fn caught_unwind_keeps_the_applied_phase_and_invokes_one_finalizer() {
        use std::cell::Cell;

        for (publish_stopped, expected_finalization) in [
            (false, InstallerBoundaryFinalization::RestoreFailedStop),
            (true, InstallerBoundaryFinalization::RestorePrior),
        ] {
            let phase = Cell::new(InstallerBoundaryPhase::Stopping);
            let caught = catch_installer_boundary_unwind(|| -> anyhow::Result<()> {
                if publish_stopped {
                    phase.set(InstallerBoundaryPhase::Stopped);
                }
                panic!("injected lifecycle unwind");
            });
            let finalizer_calls = Cell::new(0usize);
            let result = finalize_installer_boundary_result(caught, |error| {
                finalizer_calls.set(finalizer_calls.get() + 1);
                assert_eq!(
                    installer_boundary_finalization(phase.get()),
                    expected_finalization
                );
                anyhow::anyhow!("injected exact phase finalization: {error:#}")
            });
            let error = result.expect_err("the panic must become a recoverable boundary error");
            assert!(error.to_string().contains("exact phase finalization"));
            assert_eq!(finalizer_calls.get(), 1);
        }
    }

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

    #[test]
    fn boundary_finalization_is_phase_aware_and_never_restarts_a_failed_transition() {
        assert_eq!(
            installer_boundary_finalization(InstallerBoundaryPhase::Stopping),
            InstallerBoundaryFinalization::RestoreFailedStop
        );
        assert_eq!(
            installer_boundary_finalization(InstallerBoundaryPhase::Stopped),
            InstallerBoundaryFinalization::RestorePrior
        );
        for phase in [
            InstallerBoundaryPhase::ReplacementTransition,
            InstallerBoundaryPhase::RollbackTransition,
        ] {
            assert_eq!(
                installer_boundary_finalization(phase),
                InstallerBoundaryFinalization::ReestablishStopped
            );
        }
        for phase in [
            InstallerBoundaryPhase::ReplacementFailedStopped,
            InstallerBoundaryPhase::RollbackFailedStopped,
        ] {
            assert_eq!(
                installer_boundary_finalization(phase),
                InstallerBoundaryFinalization::VerifyStopped
            );
        }
        assert_eq!(
            installer_boundary_finalization(InstallerBoundaryPhase::UninstallTransition),
            InstallerBoundaryFinalization::ClassifyUninstall
        );
        for phase in [
            InstallerBoundaryPhase::NewReady,
            InstallerBoundaryPhase::UninstallReady,
            InstallerBoundaryPhase::PriorRestored,
            InstallerBoundaryPhase::FinalConfirmed,
        ] {
            assert_eq!(
                installer_boundary_finalization(phase),
                InstallerBoundaryFinalization::VerifyFinal
            );
        }
    }

    #[test]
    fn stop_request_state_changes_only_after_request_publication() {
        let mut state = InitialGenerationStopState::NotRequested;
        let injected_pre_request_error: anyhow::Result<()> =
            Err(anyhow::anyhow!("injected request publication failure"));
        state
            .record_issued(injected_pre_request_error)
            .expect_err("pre-request failure must remain distinguishable");
        assert_eq!(state, InitialGenerationStopState::NotRequested);
        assert!(state.can_reuse_observed_generation());

        state
            .record_issued(Ok(()))
            .expect("fixture request was published");
        assert_eq!(state, InitialGenerationStopState::MayHaveBeenRequested);
        assert!(
            !state.can_reuse_observed_generation(),
            "an issued stop may complete later, so the observed generation must settle and restart"
        );
    }

    #[test]
    fn uninstall_applied_state_survives_a_later_probe_failure() {
        let mut state = InstallerUninstallState::NotApplied;
        state
            .record_applied_and_verify(|| {
                Err(anyhow::anyhow!("injected post-removal probe failure"))
            })
            .expect_err("the injected post-removal probe must fail");
        assert_eq!(state, InstallerUninstallState::AppliedPendingVerification);
        assert!(
            state.is_applied(),
            "a probe error must not make an already-applied removal look like the prior stopped state"
        );

        state.mark_ready();
        assert_eq!(state, InstallerUninstallState::Ready);
    }

    #[test]
    fn marker_failure_keeps_the_already_applied_phase_for_finalization() {
        for published_phase in [
            InstallerBoundaryPhase::Stopped,
            InstallerBoundaryPhase::ReplacementFailedStopped,
            InstallerBoundaryPhase::PriorRestored,
            InstallerBoundaryPhase::FinalConfirmed,
        ] {
            let mut phase = InstallerBoundaryPhase::ReplacementTransition;
            let error = publish_installer_daemon_boundary_state(
                &mut BrokenMarkerWriter,
                &mut phase,
                published_phase,
                "fixture marker",
            )
            .expect_err("the fixture marker pipe is closed");
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::BrokenPipe)
            );
            assert_eq!(phase, published_phase);
        }
    }

    #[test]
    fn installer_capture_rejects_a_daemon_from_another_public_copy() {
        let home = crate::fs_ops::create_direct_tempdir().expect("temp executable identities");
        let expected = home.path().join(if cfg!(windows) {
            "expected.exe"
        } else {
            "expected"
        });
        let other = home
            .path()
            .join(if cfg!(windows) { "other.exe" } else { "other" });
        std::fs::write(&expected, b"expected executable").expect("write expected fixture");
        std::fs::write(&other, b"other executable").expect("write other fixture");

        validate_running_daemon_executable(&expected, 4242, &expected).unwrap();
        let error = validate_running_daemon_executable(&expected, 4242, &other)
            .expect_err("installer must not guess how to restore a different daemon copy");
        assert!(error.to_string().contains("running from"), "{error:#}");
        assert!(error.to_string().contains("refusing to guess"), "{error:#}");

        std::fs::write(&other, b"expected executable")
            .expect("make the other executable content-identical");
        let same_contents = validate_running_daemon_executable(&expected, 4242, &other)
            .expect_err("matching bytes cannot substitute for the expected executable identity");
        assert!(same_contents.to_string().contains("running from"));

        std::fs::remove_file(&expected).expect("remove expected fixture");
        let missing = validate_running_daemon_executable(&expected, 4242, &expected)
            .expect_err("a matching identity string cannot substitute for a missing executable");
        assert!(
            missing
                .to_string()
                .contains("binding the installer executable"),
            "{missing:#}"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::process::Stdio;
    use std::time::Duration;

    const SIGNAL_ISOLATION_TEST_ROLE: &str = "CSGP_SIGNAL_ISOLATION_TEST_ROLE";
    const SIGNAL_ISOLATION_TEST_SENTINEL: &str = "CSGP_SIGNAL_ISOLATION_TEST_SENTINEL";
    const SIGNAL_ISOLATION_TEST_READY: &str = "CSGP_SIGNAL_ISOLATION_TEST_READY";
    const SIGNAL_ISOLATION_SIGINT_TEST_NAME: &str =
        "daemon::tests::detached_background_child_survives_parent_process_group_interrupt";
    const SIGNAL_ISOLATION_SIGHUP_TEST_NAME: &str =
        "daemon::tests::detached_background_child_survives_parent_session_hangup";
    const SIGNAL_ISOLATION_TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn wait_for_test_path(path: &std::path::Path, purpose: &str) {
        let deadline = std::time::Instant::now() + SIGNAL_ISOLATION_TEST_TIMEOUT;
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {purpose} at {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn publish_test_path(path: &std::path::Path, contents: &[u8]) {
        let staged = path.with_extension("publishing");
        std::fs::write(&staged, contents).expect("write staged test marker");
        std::fs::rename(&staged, path).expect("publish complete test marker");
    }

    fn run_signal_isolation_test(test_name: &str, signal: libc::c_int, signal_name: &str) {
        match std::env::var(SIGNAL_ISOLATION_TEST_ROLE).as_deref() {
            Ok("holder") => {
                println!("holder ready");
                std::io::stdout().flush().expect("flush holder readiness");
                let mut input = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut input)
                    .expect("wait for coordinator EOF");
                let sentinel =
                    std::env::var_os(SIGNAL_ISOLATION_TEST_SENTINEL).expect("holder sentinel path");
                publish_test_path(
                    std::path::Path::new(&sentinel),
                    b"holder finalized after EOF",
                );
                return;
            }
            Ok("coordinator") => {
                let executable = std::env::current_exe().expect("locate test executable");
                let mut command = std::process::Command::new(executable);
                command
                    .arg(test_name)
                    .arg("--exact")
                    .arg("--nocapture")
                    .env(SIGNAL_ISOLATION_TEST_ROLE, "holder")
                    .env(
                        SIGNAL_ISOLATION_TEST_SENTINEL,
                        std::env::var_os(SIGNAL_ISOLATION_TEST_SENTINEL)
                            .expect("coordinator sentinel path"),
                    )
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                super::isolate_background_child_from_terminal_interrupt(&mut command);
                // The coordinator is deliberately killed while this isolated child survives;
                // waiting here would invalidate the process-group isolation contract.
                #[allow(
                    clippy::zombie_processes,
                    reason = "the coordinator intentionally exits before its isolated child"
                )]
                let mut holder = command.spawn().expect("spawn isolated holder helper");
                let holder_session = unsafe {
                    // SAFETY: this read-only query accepts the live child PID.
                    libc::getsid(holder.id() as libc::pid_t)
                };
                assert_eq!(
                    holder_session,
                    holder.id() as libc::pid_t,
                    "isolated child did not become its own session leader"
                );
                let mut output =
                    std::io::BufReader::new(holder.stdout.take().expect("holder stdout"));
                let mut marker = String::new();
                loop {
                    marker.clear();
                    let read = output
                        .read_line(&mut marker)
                        .expect("read holder readiness");
                    assert_ne!(read, 0, "holder exited before publishing readiness");
                    if marker.contains("holder ready") {
                        break;
                    }
                }
                let previous_handler = unsafe {
                    // SAFETY: this disposable coordinator resets only the
                    // signal selected by its parent test before advertising
                    // readiness, making termination deterministic even if the
                    // test runner inherited SIG_IGN from its shell.
                    libc::signal(signal, libc::SIG_DFL)
                };
                assert_ne!(
                    previous_handler,
                    libc::SIG_ERR,
                    "reset {signal_name} disposition in coordinator"
                );
                let ready =
                    std::env::var_os(SIGNAL_ISOLATION_TEST_READY).expect("coordinator ready path");
                publish_test_path(std::path::Path::new(&ready), b"ready");
                loop {
                    std::thread::park();
                }
            }
            Ok(other) => panic!("unexpected signal-isolation helper role {other}"),
            Err(_) => {}
        }

        let temp = crate::fs_ops::create_direct_tempdir().expect("signal isolation temp directory");
        let ready = temp.path().join("coordinator-ready");
        let finalized = temp.path().join("holder-finalized");
        let executable = std::env::current_exe().expect("locate test executable");
        let mut command = std::process::Command::new(executable);
        command
            .arg(test_name)
            .arg("--exact")
            .arg("--nocapture")
            .env(SIGNAL_ISOLATION_TEST_ROLE, "coordinator")
            .env(SIGNAL_ISOLATION_TEST_READY, &ready)
            .env(SIGNAL_ISOLATION_TEST_SENTINEL, &finalized)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        super::isolate_background_child_from_terminal_interrupt(&mut command);
        let mut coordinator = command.spawn().expect("spawn interrupt coordinator");
        let coordinator_session = unsafe {
            // SAFETY: this read-only query accepts the live child PID.
            libc::getsid(coordinator.id() as libc::pid_t)
        };
        assert_eq!(
            coordinator_session,
            coordinator.id() as libc::pid_t,
            "coordinator did not enter its disposable session"
        );
        wait_for_test_path(&ready, "coordinator readiness");

        let coordinator_group = -(coordinator.id() as i32);
        // SAFETY: setsid made the child a session and process-group leader, so
        // the negative PID targets only that disposable coordinator group.
        let signal_result = unsafe { libc::kill(coordinator_group, signal) };
        assert_eq!(
            signal_result, 0,
            "send {signal_name} to coordinator process group"
        );
        let status = coordinator
            .wait()
            .expect("wait for interrupted coordinator");
        assert!(
            !status.success(),
            "{signal_name} did not terminate the coordinator"
        );
        wait_for_test_path(&finalized, "isolated holder EOF finalization");
        assert_eq!(
            std::fs::read(&finalized).expect("read holder finalization sentinel"),
            b"holder finalized after EOF"
        );
    }

    #[test]
    fn detached_background_child_survives_parent_process_group_interrupt() {
        run_signal_isolation_test(SIGNAL_ISOLATION_SIGINT_TEST_NAME, libc::SIGINT, "SIGINT");
    }

    #[test]
    fn detached_background_child_survives_parent_session_hangup() {
        run_signal_isolation_test(SIGNAL_ISOLATION_SIGHUP_TEST_NAME, libc::SIGHUP, "SIGHUP");
    }

    #[test]
    fn installer_capture_rejects_the_same_file_through_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let home = crate::fs_ops::create_direct_tempdir().expect("temp identity root");
        let real_root = home.path().join("real");
        let real_bin = real_root.join("bin");
        std::fs::create_dir_all(&real_bin).expect("create real bin");
        let running = real_bin.join("codex-switch-global-pace");
        std::fs::write(&running, b"same executable identity").expect("write executable fixture");
        let alias_root = home.path().join("alias");
        symlink(&real_root, &alias_root).expect("create ancestor symlink");
        let installer_bound = alias_root.join("bin/codex-switch-global-pace");

        let error = super::validate_running_daemon_executable(&installer_bound, 4242, &running)
            .expect_err("an ancestor symlink must not bypass direct executable binding");
        let detail = format!("{error:#}");
        assert!(
            detail.contains("opening direct transaction directory component"),
            "{detail}"
        );
        assert!(
            detail.contains(&alias_root.display().to_string()),
            "{detail}"
        );
    }

    #[test]
    fn user_path_migration_rejects_a_legacy_foreground_winner_with_identical_bytes() {
        let home = crate::fs_ops::create_direct_tempdir().expect("temp migration identities");
        let legacy = home.path().join("legacy/codex-switch-global-pace");
        let user = home.path().join("user/codex-switch-global-pace");
        std::fs::create_dir_all(legacy.parent().unwrap()).expect("create legacy parent");
        std::fs::create_dir_all(user.parent().unwrap()).expect("create user parent");
        std::fs::write(&legacy, b"identical executable bytes").expect("write legacy fixture");
        std::fs::write(&user, b"identical executable bytes").expect("write user fixture");

        let error = super::validate_running_daemon_executable(&user, 4242, &legacy)
            .expect_err("a legacy foreground winner is not the newly published user executable");
        assert!(error.to_string().contains("running from"), "{error:#}");
    }

    /// A `daemon start` that reports failure must leave no daemon behind. The
    /// child is spawned detached, so abandoning it on timeout hands the user a
    /// process they were just told does not exist — and a second
    /// `daemon start` that then refuses with "already running".
    #[test]
    fn a_daemon_that_never_signals_readiness_is_killed_not_abandoned() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = crate::fs_ops::create_direct_tempdir().expect("temp home");
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
