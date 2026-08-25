use crate::output::user_println;
use anyhow::{Context, Result};
#[cfg(not(target_os = "windows"))]
use fs4::{FileExt, TryLockError};
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "windows", test))]
const WINDOWS_TASK_NAME: &str = r"\codex-switch-global-pace-daemon";
#[cfg(any(target_os = "macos", test))]
const LAUNCHD_LABEL: &str = "com.codex-switch-global-pace.daemon";
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT_NAME: &str = "codex-switch-global-pace-daemon";

pub(crate) struct ServiceOperationLease {
    #[cfg(not(target_os = "windows"))]
    file: std::fs::File,
    #[cfg(target_os = "windows")]
    mutex: windows_sys::Win32::Foundation::HANDLE,
}

impl Drop for ServiceOperationLease {
    fn drop(&mut self) {
        #[cfg(not(target_os = "windows"))]
        let _ = FileExt::unlock(&self.file);
        #[cfg(target_os = "windows")]
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.mutex);
            windows_sys::Win32::Foundation::CloseHandle(self.mutex);
        }
    }
}

pub(crate) fn acquire_service_operation_lease() -> Result<ServiceOperationLease> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

        const MUTEX_NAME: &str = "Global\\codex-switch-global-pace-daemon-service-operation-v1";
        let mut name = MUTEX_NAME.encode_utf16().collect::<Vec<_>>();
        name.push(0);
        let mutex = unsafe { CreateMutexW(std::ptr::null(), false.into(), name.as_ptr()) };
        if mutex.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("opening the global daemon service-operation mutex");
        }
        match unsafe { WaitForSingleObject(mutex, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(ServiceOperationLease { mutex }),
            WAIT_TIMEOUT => {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(mutex) };
                anyhow::bail!("another daemon service operation is already in progress")
            }
            _ => {
                let error = std::io::Error::last_os_error();
                unsafe { windows_sys::Win32::Foundation::CloseHandle(mutex) };
                Err(error).context("waiting on the global daemon service-operation mutex")
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory for service lock"))?;
        let path = home.join(".codex-switch-global-pace-daemon-service.lock");
        acquire_service_operation_lease_at(&path)
    }
}

#[cfg(not(target_os = "windows"))]
fn acquire_service_operation_lease_at(path: &Path) -> Result<ServiceOperationLease> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening service-operation lock {}", path.display()))?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(ServiceOperationLease { file }),
        Err(TryLockError::WouldBlock) => anyhow::bail!(
            "another daemon service operation is already in progress at {}",
            path.display()
        ),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking service operation {}", path.display()))
        }
    }
}

pub fn install(expected_existing_executable: Option<PathBuf>) -> Result<()> {
    validate_install_migration_authority(expected_existing_executable.as_deref())?;
    let _lease = acquire_service_operation_lease()?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let executable = std::env::current_exe().context("locating daemon executable")?;
        install_for_executable_locked(
            &executable,
            expected_existing_executable.as_deref(),
            &_lease,
        )
    }
    #[cfg(target_os = "windows")]
    {
        install_task_scheduler(expected_existing_executable.as_deref())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service install is not supported on this platform")
}

/// Install a Unix user-service definition for an explicit public executable
/// while the caller retains the single service-operation lease. The direct
/// installer uses this during a path migration so it never has to drop and
/// reacquire lifecycle authority between stopping the old daemon and starting
/// the replacement.
#[cfg(not(target_os = "windows"))]
pub(crate) fn install_for_executable_locked(
    executable: &Path,
    expected_existing_executable: Option<&Path>,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_expected_executable(executable)?;
    validate_install_migration_authority(expected_existing_executable)?;
    #[cfg(target_os = "macos")]
    return install_launchd(executable, expected_existing_executable);
    #[cfg(target_os = "linux")]
    return install_systemd(executable, expected_existing_executable);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("Service install is not supported on this platform")
}

fn validate_install_migration_authority(expected_existing_executable: Option<&Path>) -> Result<()> {
    let Some(expected) = expected_existing_executable else {
        return Ok(());
    };
    validate_expected_executable(expected)?;
    #[cfg(target_os = "windows")]
    anyhow::bail!(
        "scheduled-task executable migration is not supported on Windows; uninstall the exactly owned task before installing a different executable"
    );
    #[cfg(not(target_os = "windows"))]
    Ok(())
}

pub(crate) fn uninstall_locked(
    expected_executable: &Path,
    _previous_daemon_running: bool,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_expected_executable(expected_executable)?;
    validate_uninstall_owner(expected_executable)?;
    #[cfg(target_os = "macos")]
    return uninstall_launchd(expected_executable);
    #[cfg(target_os = "linux")]
    return uninstall_systemd(expected_executable);
    #[cfg(target_os = "windows")]
    return uninstall_task_scheduler(expected_executable, _previous_daemon_running);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service uninstall is not supported on this platform")
}

#[derive(Debug)]
pub(crate) enum ExactServiceUninstallOutcome {
    Applied {
        operation_error: Option<anyhow::Error>,
    },
    AppliedPendingVerification {
        post_state_error: anyhow::Error,
    },
    PriorExact {
        operation_error: anyhow::Error,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactServiceBoundaryState {
    Absent,
    PriorExact,
    Ambiguous,
}

fn classify_observed_service_state(
    observed: &ServiceStateSnapshot,
    prior: &ServiceStateSnapshot,
) -> ExactServiceBoundaryState {
    if !observed.is_installed() && observed.manager_pid().is_none() {
        ExactServiceBoundaryState::Absent
    } else if observed.matches_exact_state(prior) {
        ExactServiceBoundaryState::PriorExact
    } else {
        ExactServiceBoundaryState::Ambiguous
    }
}

pub(crate) fn classify_current_service_state(
    expected_executable: &Path,
    prior: &ServiceStateSnapshot,
) -> Result<ExactServiceBoundaryState> {
    let observed = capture_service_state_snapshot(expected_executable, None)?;
    Ok(classify_observed_service_state(&observed, prior))
}

fn classify_exact_uninstall_transition(
    operation: Result<()>,
    observed_absent: bool,
    observed_prior_exact: bool,
) -> Result<ExactServiceUninstallOutcome> {
    if observed_absent && observed_prior_exact {
        anyhow::bail!("service uninstall post-state cannot be both absent and the prior snapshot");
    }
    if observed_absent {
        return Ok(ExactServiceUninstallOutcome::Applied {
            operation_error: operation.err(),
        });
    }
    if observed_prior_exact {
        return match operation {
            Err(operation_error) => {
                Ok(ExactServiceUninstallOutcome::PriorExact { operation_error })
            }
            Ok(()) => anyhow::bail!(
                "service uninstall returned success without changing the exact prior service snapshot"
            ),
        };
    }

    match operation {
        Ok(()) => anyhow::bail!(
            "service uninstall returned success but the observed service state was neither exact removal nor the exact prior snapshot"
        ),
        Err(operation_error) => Err(operation_error.context(
            "service uninstall failed and the observed service state was neither exact removal nor the exact prior snapshot",
        )),
    }
}

fn classify_exact_uninstall_after_observation(
    operation: Result<()>,
    observed: Result<ExactServiceBoundaryState>,
) -> Result<ExactServiceUninstallOutcome> {
    match observed {
        Ok(state) => classify_exact_uninstall_transition(
            operation,
            state == ExactServiceBoundaryState::Absent,
            state == ExactServiceBoundaryState::PriorExact,
        ),
        Err(state_error) => match operation {
            Ok(()) => Ok(ExactServiceUninstallOutcome::AppliedPendingVerification {
                post_state_error: state_error.context(
                    "service uninstall returned success but its exact post-state could not be captured",
                ),
            }),
            Err(operation_error) => Err(state_error.context(format!(
                "service uninstall also failed before its exact post-state could be captured: {operation_error:#}"
            ))),
        },
    }
}

fn uninstall_locked_exact_with<Uninstall, Capture>(
    uninstall: Uninstall,
    capture_post_state: Capture,
) -> Result<ExactServiceUninstallOutcome>
where
    Uninstall: FnOnce() -> Result<()>,
    Capture: FnOnce() -> Result<ExactServiceBoundaryState>,
{
    let operation = uninstall();
    let observed = capture_post_state();
    classify_exact_uninstall_after_observation(operation, observed)
}

pub(crate) fn uninstall_locked_exact(
    expected_executable: &Path,
    previous_daemon_running: bool,
    prior: &ServiceStateSnapshot,
    lease: &ServiceOperationLease,
) -> Result<ExactServiceUninstallOutcome> {
    uninstall_locked_exact_with(
        || uninstall_locked(expected_executable, previous_daemon_running, lease),
        || {
            capture_service_state_snapshot(expected_executable, None)
                .map(|observed| classify_observed_service_state(&observed, prior))
        },
    )
}

/// Prove service-definition ownership before the caller changes process state.
/// `uninstall` repeats this check at the deletion boundary because the
/// definition may change between the graceful stop and service removal.
pub(crate) fn validate_uninstall_owner(expected_executable: &Path) -> Result<()> {
    validate_expected_executable(expected_executable)?;
    #[cfg(target_os = "macos")]
    {
        if let Some(contents) = optional_file_contents(&plist_path()?)? {
            validate_launchd_definition_owner(&contents, expected_executable)?;
        } else {
            require_no_definitionless_launchd_service()?;
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(contents) = optional_file_contents(&unit_path()?)? {
            validate_systemd_definition_owner(&contents, expected_executable)?;
        } else {
            require_no_definitionless_systemd_service()?;
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        let task_file = windows_task_file_path()?;
        let Some(contents) = optional_task_definition(&task_file)? else {
            return Ok(());
        };
        let exported = query_scheduled_task_xml("export scheduled task for ownership check")?;
        validate_task_scheduler_definition_owner(&contents, expected_executable)?;
        validate_task_scheduler_definition_owner(&exported, expected_executable)?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    Ok(())
}

pub(crate) fn validate_expected_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!(
            "expected service executable must be an absolute path: {}",
            path.display()
        );
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        anyhow::bail!(
            "expected service executable must not contain '.' or '..': {}",
            path.display()
        );
    }
    Ok(())
}

/// Read the installed-service marker without treating metadata or scheduler
/// errors as "not installed". Self-update uses this before stopping a running
/// daemon because it must restore the same launch mechanism on rollback.
pub(crate) fn is_installed_checked() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let installed = checked_regular_file(&plist_path()?, "LaunchAgent definition")?;
        if !installed {
            require_no_definitionless_launchd_service()?;
        }
        Ok(installed)
    }
    #[cfg(target_os = "linux")]
    {
        let installed = checked_regular_file(&unit_path()?, "systemd user-service definition")?;
        if !installed {
            require_no_definitionless_systemd_service()?;
        }
        Ok(installed)
    }
    #[cfg(target_os = "windows")]
    {
        let task_file = windows_task_file_path()?;
        if optional_task_definition(&task_file)?.is_none() {
            return Ok(false);
        }
        Ok(true)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    Ok(false)
}

#[cfg(target_os = "windows")]
fn windows_directory() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    let mut buffer = vec![0u16; 260];
    let windows_directory = loop {
        let length = unsafe {
            windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        if length == 0 {
            return Err(std::io::Error::last_os_error())
                .context("locating the trusted Windows directory");
        }
        let length = length as usize;
        if length < buffer.len() {
            break std::ffi::OsString::from_wide(&buffer[..length]);
        }
        buffer.resize(length.saturating_add(1), 0);
    };
    Ok(PathBuf::from(windows_directory))
}

#[cfg(target_os = "windows")]
fn windows_task_file_path() -> Result<PathBuf> {
    Ok(windows_directory()?
        .join("System32")
        .join("Tasks")
        .join(WINDOWS_TASK_NAME.trim_start_matches(['\\', '/'])))
}

#[cfg(target_os = "windows")]
fn windows_powershell_path() -> Result<PathBuf> {
    Ok(windows_directory()?
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe"))
}

#[cfg(target_os = "windows")]
fn optional_task_definition(path: &Path) -> Result<Option<Vec<u8>>> {
    let scheduler_definition = optional_scheduled_task_xml(
        "export scheduled task while inspecting its trusted definition",
    )?;
    let file_definition = if checked_regular_file(path, "Windows scheduled-task definition")? {
        Some(
            std::fs::read(path)
                .with_context(|| format!("reading scheduled-task definition {}", path.display()))?,
        )
    } else {
        None
    };
    match (scheduler_definition.is_some(), file_definition) {
        (false, None) => Ok(None),
        (true, Some(definition)) => Ok(Some(definition)),
        (true, None) => anyhow::bail!(
            "Task Scheduler reports {WINDOWS_TASK_NAME}, but its trusted on-disk definition is missing; ownership cannot be proven"
        ),
        (false, Some(_)) => anyhow::bail!(
            "trusted on-disk definition {} exists, but Task Scheduler does not report the task; refusing an inconsistent service state",
            path.display()
        ),
    }
}

#[cfg(any(target_os = "windows", test))]
fn task_listing_contains_name(listing: &[u8], expected_name: &str) -> Result<bool> {
    let mut found = false;
    for (index, raw_line) in listing.split(|byte| *byte == b'\n').enumerate() {
        let mut line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if index == 0 {
            line = line.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(line);
        }
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix(b"\"") else {
            anyhow::bail!("Task Scheduler CSV listing contained a non-CSV row");
        };
        let Some(end) = rest.windows(2).position(|window| window == b"\",") else {
            anyhow::bail!("Task Scheduler CSV listing contained an unterminated task name");
        };
        let name = &rest[..end];
        if name.eq_ignore_ascii_case(expected_name.as_bytes()) {
            if found {
                anyhow::bail!(
                    "Task Scheduler CSV listing contained the service task more than once"
                );
            }
            found = true;
        }
    }
    Ok(found)
}

#[cfg(target_os = "windows")]
fn optional_scheduled_task_xml(action: &str) -> Result<Option<Vec<u8>>> {
    let listing = schtasks(
        &["/Query", "/FO", "CSV", "/NH"],
        "list scheduled tasks for exact service-state inspection",
    )?;
    if !task_listing_contains_name(&listing.stdout, WINDOWS_TASK_NAME)? {
        return Ok(None);
    }
    Ok(Some(
        schtasks(&["/Query", "/TN", WINDOWS_TASK_NAME, "/XML"], action)?.stdout,
    ))
}

#[cfg(target_os = "windows")]
fn require_task_definition_snapshot(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    let current = optional_task_definition(path)?;
    if !definition_snapshot_matches(current.as_deref(), expected) {
        anyhow::bail!(
            "scheduled-task definition {} changed during the operation; refusing to mutate it",
            path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn query_scheduled_task_xml(action: &str) -> Result<Vec<u8>> {
    optional_scheduled_task_xml(action)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Task Scheduler no longer reports {WINDOWS_TASK_NAME} while its XML was required"
        )
    })
}

#[cfg(target_os = "windows")]
fn optional_windows_task_snapshot(path: &Path) -> Result<Option<WindowsTaskSnapshot>> {
    use std::io::Read as _;

    let scheduler_xml = optional_scheduled_task_xml(
        "export scheduled task while capturing its exact lifecycle snapshot",
    )?;
    let path_token = crate::fs_ops::token_if_present(path)?;
    let (scheduler_xml, path_token) = match (scheduler_xml, path_token) {
        (None, None) => return Ok(None),
        (Some(scheduler_xml), Some(path_token)) => (scheduler_xml, path_token),
        _ => anyhow::bail!(
            "Task Scheduler and its trusted definition {} disagreed while capturing lifecycle state",
            path.display()
        ),
    };

    let mut file = crate::fs_ops::open_direct_regular(path)?;
    let opened_token = crate::fs_ops::token_for_file(&mut file)?;
    if opened_token != path_token {
        anyhow::bail!(
            "scheduled-task definition changed while it was opened: {}",
            path.display()
        );
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .with_context(|| format!("reading scheduled-task definition {}", path.display()))?;
    let after_read = crate::fs_ops::token_for_file(&mut file)?;
    let live_after = crate::fs_ops::token_for_path(path)?;
    if after_read != path_token || live_after != path_token || !path_token.matches_bytes(&contents)
    {
        anyhow::bail!(
            "scheduled-task definition changed while its lifecycle snapshot was read: {}",
            path.display()
        );
    }
    let scheduler_after = query_scheduled_task_xml(
        "re-export scheduled task after capturing its lifecycle snapshot",
    )?;
    if scheduler_after != scheduler_xml {
        anyhow::bail!("Task Scheduler XML changed while its lifecycle snapshot was captured");
    }
    Ok(Some(WindowsTaskSnapshot {
        contents,
        token: path_token,
        scheduler_xml,
    }))
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowsProcessGeneration {
    pid: u32,
    parent_pid: u32,
    creation_ticks: u64,
}

#[cfg(any(target_os = "windows", test))]
fn parse_task_scheduler_instance_row<'a>(
    line: &'a str,
    expected_tag: &str,
) -> Result<(&'a str, u32)> {
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() != 3 || fields[0] != expected_tag {
        anyhow::bail!("Task Scheduler ownership proof expected {expected_tag}, got '{line}'");
    }
    let instance_guid = fields[1];
    let canonical_guid = instance_guid.len() == 36
        && instance_guid
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            });
    if !canonical_guid || instance_guid == "00000000-0000-0000-0000-000000000000" {
        anyhow::bail!("Task Scheduler ownership proof contained an invalid instance GUID");
    }
    let engine_pid = fields[2].parse::<u32>().with_context(|| {
        format!(
            "Task Scheduler reported an invalid engine PID '{}'",
            fields[2]
        )
    })?;
    if engine_pid == 0 {
        anyhow::bail!("Task Scheduler reported reserved engine PID zero");
    }
    Ok((instance_guid, engine_pid))
}

#[cfg(any(target_os = "windows", test))]
fn parse_task_scheduler_process_row(
    line: &str,
    expected_tag: &str,
) -> Result<WindowsProcessGeneration> {
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() != 4 || fields[0] != expected_tag {
        anyhow::bail!("Task Scheduler ownership proof expected {expected_tag}, got '{line}'");
    }
    let pid = fields[1].parse::<u32>().with_context(|| {
        format!(
            "Task Scheduler reported an invalid process PID '{}'",
            fields[1]
        )
    })?;
    let parent_pid = fields[2].parse::<u32>().with_context(|| {
        format!(
            "Task Scheduler reported an invalid parent PID '{}'",
            fields[2]
        )
    })?;
    let creation_ticks = fields[3].parse::<u64>().with_context(|| {
        format!(
            "Task Scheduler reported an invalid process creation time '{}'",
            fields[3]
        )
    })?;
    if pid == 0 || creation_ticks == 0 {
        anyhow::bail!("Task Scheduler ownership proof contained a reserved PID or creation time");
    }
    Ok(WindowsProcessGeneration {
        pid,
        parent_pid,
        creation_ticks,
    })
}

#[cfg(any(target_os = "windows", test))]
fn parse_task_scheduler_manager_proof_output(
    output: &[u8],
    authoritative_daemon_pid: Option<u32>,
) -> Result<Option<u32>> {
    let output = std::str::from_utf8(output)
        .context("Task Scheduler ownership proof output is not UTF-8")?;
    let lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines == ["none"] {
        return Ok(None);
    }
    let daemon_pid = authoritative_daemon_pid
        .filter(|pid| *pid != 0)
        .context("Task Scheduler has a running instance without an authoritative daemon PID")?;
    if lines.len() < 4 {
        anyhow::bail!("Task Scheduler ownership proof was incomplete");
    }

    let (start_instance_guid, engine_pid) =
        parse_task_scheduler_instance_row(lines[0], "instance-start")?;
    let end_instance_index = lines
        .iter()
        .position(|line| line.starts_with("instance-end\t"))
        .context("Task Scheduler ownership proof omitted its final instance observation")?;
    if end_instance_index < 3 || end_instance_index + 1 != lines.len() {
        anyhow::bail!("Task Scheduler ownership proof had an invalid observation order");
    }
    let process_rows = &lines[1..end_instance_index];
    if process_rows.len() % 2 != 0 {
        anyhow::bail!("Task Scheduler ownership proof had unmatched process observations");
    }
    let generation_count = process_rows.len() / 2;
    if generation_count == 0 {
        anyhow::bail!("Task Scheduler ownership proof omitted the process ancestry");
    }
    let (start_rows, end_rows) = process_rows.split_at(generation_count);
    let start_generations = start_rows
        .iter()
        .map(|line| parse_task_scheduler_process_row(line, "process-start"))
        .collect::<Result<Vec<_>>>()?;
    let end_generations = end_rows
        .iter()
        .map(|line| parse_task_scheduler_process_row(line, "process-end"))
        .collect::<Result<Vec<_>>>()?;

    let mut expected_pid = daemon_pid;
    let mut child_creation_ticks = None;
    let mut seen = std::collections::HashSet::new();
    for (index, generation) in start_generations.iter().enumerate() {
        if generation.pid != expected_pid {
            anyhow::bail!(
                "Task Scheduler process ancestry changed before reaching PID {expected_pid}"
            );
        }
        if !seen.insert(generation.pid) {
            anyhow::bail!("cycle in Windows process parent chain");
        }
        if let Some(child_creation_ticks) = child_creation_ticks
            && generation.creation_ticks > child_creation_ticks
        {
            anyhow::bail!(
                "Windows process parent {} is newer than its child",
                generation.pid
            );
        }
        if generation.pid == engine_pid {
            if index + 1 != start_generations.len() {
                anyhow::bail!("Task Scheduler ownership proof continued past its engine process");
            }
            break;
        }
        if generation.parent_pid == 0 {
            anyhow::bail!(
                "authoritative daemon PID {daemon_pid} is not descended from Task Scheduler engine PID {engine_pid}"
            );
        }
        expected_pid = generation.parent_pid;
        child_creation_ticks = Some(generation.creation_ticks);
    }
    if start_generations.last().map(|generation| generation.pid) != Some(engine_pid) {
        anyhow::bail!(
            "authoritative daemon PID {daemon_pid} is not descended from Task Scheduler engine PID {engine_pid}"
        );
    }
    if start_generations != end_generations {
        anyhow::bail!(
            "a Windows process generation changed during Task Scheduler ownership inspection"
        );
    }

    let (end_instance_guid, end_engine_pid) =
        parse_task_scheduler_instance_row(lines[end_instance_index], "instance-end")?;
    if start_instance_guid != end_instance_guid || engine_pid != end_engine_pid {
        anyhow::bail!(
            "Task Scheduler running-instance identity changed during ownership inspection"
        );
    }
    Ok(Some(daemon_pid))
}

#[cfg(target_os = "windows")]
fn task_scheduler_manager_pid_checked(
    authoritative_daemon_pid: Option<u32>,
) -> Result<Option<u32>> {
    const SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$task = $service.GetFolder('\').GetTask($env:CODEX_SWITCH_TASK_NAME)
$instances = @($task.GetInstances(0))
if ($instances.Count -eq 0) { Write-Output 'none'; exit 0 }
if ($instances.Count -ne 1) { throw "Task Scheduler reported $($instances.Count) running instances" }
$instanceGuid = ([guid]$instances[0].InstanceGuid).ToString('D')
$enginePid = [uint32]$instances[0].EnginePID
if ($enginePid -eq 0) { throw 'Task Scheduler reported reserved engine PID zero' }
$daemonPid = [uint32]$env:CODEX_SWITCH_DAEMON_PID
if ($daemonPid -eq 0) { throw 'Task Scheduler has a running instance without an authoritative daemon PID' }
$seen = @{}
$cursor = $daemonPid
$startProcesses = [System.Collections.Generic.List[object]]::new()
while ($cursor -ne 0) {
    if ($seen.ContainsKey($cursor)) { throw 'cycle in Windows process parent chain' }
    $seen[$cursor] = $true
    $processMatches = @(Get-CimInstance Win32_Process -Filter ("ProcessId = " + $cursor))
    if ($processMatches.Count -ne 1) { throw "process $cursor disappeared during Task Scheduler ownership inspection" }
    $process = $processMatches[0]
    $startProcesses.Add([pscustomobject]@{
        Pid = [uint32]$process.ProcessId
        ParentPid = [uint32]$process.ParentProcessId
        CreationTicks = [uint64]([datetime]$process.CreationDate).ToUniversalTime().Ticks
    })
    if ($cursor -eq $enginePid) { break }
    $cursor = [uint32]$process.ParentProcessId
}
if ($cursor -ne $enginePid) { throw "authoritative daemon PID $daemonPid is not descended from Task Scheduler engine PID $enginePid" }

$endProcesses = [System.Collections.Generic.List[object]]::new()
foreach ($startProcess in $startProcesses) {
    $processMatches = @(Get-CimInstance Win32_Process -Filter ("ProcessId = " + $startProcess.Pid))
    if ($processMatches.Count -ne 1) { throw "process $($startProcess.Pid) disappeared during Task Scheduler ownership revalidation" }
    $process = $processMatches[0]
    $endProcesses.Add([pscustomobject]@{
        Pid = [uint32]$process.ProcessId
        ParentPid = [uint32]$process.ParentProcessId
        CreationTicks = [uint64]([datetime]$process.CreationDate).ToUniversalTime().Ticks
    })
}
$after = @($task.GetInstances(0))
if ($after.Count -ne 1) { throw 'Task Scheduler running instance disappeared during ownership revalidation' }
$afterInstanceGuid = ([guid]$after[0].InstanceGuid).ToString('D')
$afterEnginePid = [uint32]$after[0].EnginePID

Write-Output ([string]::Join("`t", @('instance-start', $instanceGuid, $enginePid)))
foreach ($process in $startProcesses) {
    Write-Output ([string]::Join("`t", @('process-start', $process.Pid, $process.ParentPid, $process.CreationTicks)))
}
foreach ($process in $endProcesses) {
    Write-Output ([string]::Join("`t", @('process-end', $process.Pid, $process.ParentPid, $process.CreationTicks)))
}
Write-Output ([string]::Join("`t", @('instance-end', $afterInstanceGuid, $afterEnginePid)))"#;

    let output = std::process::Command::new(windows_powershell_path()?)
        .env(
            "CODEX_SWITCH_TASK_NAME",
            WINDOWS_TASK_NAME.trim_start_matches(['\\', '/']),
        )
        .env(
            "CODEX_SWITCH_DAEMON_PID",
            authoritative_daemon_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "0".to_string()),
        )
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .output()
        .context("querying the Task Scheduler running-instance PID")?;
    if !output.status.success() {
        anyhow::bail!(
            "Task Scheduler running-instance PID query failed: {}",
            task_scheduler_failure_message(
                "query running scheduled-task instance",
                &String::from_utf8_lossy(&output.stderr)
            )
        );
    }
    parse_task_scheduler_manager_proof_output(&output.stdout, authoritative_daemon_pid)
}

#[cfg(target_os = "windows")]
fn require_task_xml_snapshot(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    let current = optional_scheduled_task_xml("re-export scheduled task snapshot")?;
    match (current, expected) {
        (None, None) => Ok(()),
        (None, Some(_)) => anyhow::bail!(
            "scheduled-task definition {} disappeared during the operation",
            path.display()
        ),
        (Some(_), None) => anyhow::bail!(
            "a scheduled-task definition appeared at {} during the operation",
            path.display()
        ),
        (Some(current), Some(expected)) => {
            if current != expected {
                anyhow::bail!(
                    "Task Scheduler XML changed during the operation; refusing to mutate the task"
                );
            }
            Ok(())
        }
    }
}

fn checked_regular_file(path: &Path, description: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => anyhow::bail!("{description} is not a regular file: {}", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => {
            Err(err).with_context(|| format!("inspecting {description} {}", path.display()))
        }
    }
}

fn effective_codex_home() -> Result<PathBuf> {
    let path = crate::auth::codex_auth_path()?
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("Codex auth path has no parent directory"))?;
    absolute_service_path(path)
}

fn effective_app_home() -> Result<PathBuf> {
    absolute_service_path(crate::auth::app_home()?)
}

fn absolute_service_path(path: PathBuf) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceFileSnapshot {
    contents: Vec<u8>,
    token: crate::fs_ops::FileToken,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowsTaskSnapshot {
    contents: Vec<u8>,
    token: crate::fs_ops::FileToken,
    scheduler_xml: Vec<u8>,
}

/// Exact service definition and runtime state captured while the caller holds
/// the global service-operation lease. The definition identity is token-bound
/// on disk; runtime fields are authoritative manager observations rather than
/// inferences from whether a definition happens to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceStateSnapshot {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    definition: Option<ServiceFileSnapshot>,
    #[cfg(target_os = "macos")]
    loaded: bool,
    #[cfg(target_os = "linux")]
    enabled: bool,
    #[cfg(target_os = "linux")]
    active: bool,
    #[cfg(target_os = "windows")]
    definition: Option<WindowsTaskSnapshot>,
    manager_pid: Option<u32>,
}

impl ServiceStateSnapshot {
    pub(crate) fn is_installed(&self) -> bool {
        self.definition.is_some()
    }

    pub(crate) fn manager_pid(&self) -> Option<u32> {
        self.manager_pid
    }

    pub(crate) fn matches_snapshot_with_manager(
        &self,
        expected: &Self,
        manager_pid: Option<u32>,
    ) -> bool {
        self.matches_exact_state(&expected.with_manager_pid(manager_pid))
    }

    fn with_manager_pid(&self, manager_pid: Option<u32>) -> Self {
        let mut expected = self.clone();
        expected.manager_pid = manager_pid;
        expected
    }

    fn with_manager_stopped(&self) -> Self {
        let mut expected = self.clone();
        #[cfg(target_os = "macos")]
        {
            expected.loaded = false;
        }
        #[cfg(target_os = "linux")]
        {
            expected.active = false;
        }
        expected.manager_pid = None;
        expected
    }

    fn matches_exact_state(&self, expected: &Self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let definition_matches = match (&self.definition, &expected.definition) {
            (None, None) => true,
            (Some(observed), Some(expected)) => observed.contents == expected.contents,
            _ => false,
        };
        #[cfg(target_os = "windows")]
        let definition_matches = match (&self.definition, &expected.definition) {
            (None, None) => true,
            (Some(observed), Some(expected)) => {
                observed.contents == expected.contents
                    && observed.scheduler_xml == expected.scheduler_xml
            }
            _ => false,
        };

        definition_matches
            && self.manager_pid == expected.manager_pid
            && {
                #[cfg(target_os = "macos")]
                {
                    self.loaded == expected.loaded
                }
                #[cfg(not(target_os = "macos"))]
                {
                    true
                }
            }
            && {
                #[cfg(target_os = "linux")]
                {
                    self.enabled == expected.enabled && self.active == expected.active
                }
                #[cfg(not(target_os = "linux"))]
                {
                    true
                }
            }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn optional_service_file_snapshot(path: &Path) -> Result<Option<ServiceFileSnapshot>> {
    use std::io::Read as _;

    let Some(path_token) = crate::fs_ops::token_if_present(path)? else {
        return Ok(None);
    };
    let mut file = crate::fs_ops::open_direct_regular(path)?;
    let opened_token = crate::fs_ops::token_for_file(&mut file)?;
    if opened_token != path_token {
        anyhow::bail!(
            "service definition changed while it was opened: {}",
            path.display()
        );
    }
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .with_context(|| format!("reading service definition {}", path.display()))?;
    let after_read = crate::fs_ops::token_for_file(&mut file)?;
    let live_after = crate::fs_ops::token_for_path(path)?;
    if after_read != path_token || live_after != path_token || !path_token.matches_bytes(&contents)
    {
        anyhow::bail!(
            "service definition changed while it was read: {}",
            path.display()
        );
    }
    Ok(Some(ServiceFileSnapshot {
        contents,
        token: path_token,
    }))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn optional_file_contents(path: &Path) -> Result<Option<Vec<u8>>> {
    optional_service_file_snapshot(path).map(|snapshot| snapshot.map(|snapshot| snapshot.contents))
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn require_manager_runtime_consistency(
    manager: &str,
    state_name: &str,
    state: bool,
    pid_name: &str,
    manager_pid: Option<u32>,
) -> Result<()> {
    if state != manager_pid.is_some() {
        anyhow::bail!(
            "{manager} runtime state is inconsistent: {state_name}={state}, {pid_name}={manager_pid:?}"
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LaunchdRuntimeSnapshot {
    loaded: bool,
    manager_pid: Option<u32>,
}

#[cfg(any(target_os = "macos", test))]
fn capture_launchd_runtime_for_snapshot_with<Loaded, ManagerPid>(
    loaded: Loaded,
    manager_pid: ManagerPid,
) -> Result<LaunchdRuntimeSnapshot>
where
    Loaded: FnOnce() -> Result<bool>,
    ManagerPid: FnOnce(bool) -> Result<Option<u32>>,
{
    let loaded = loaded()?;
    let manager_pid = manager_pid(loaded)?;
    require_manager_runtime_consistency("launchd", "loaded", loaded, "PID", manager_pid)?;
    Ok(LaunchdRuntimeSnapshot {
        loaded,
        manager_pid,
    })
}

pub(crate) fn capture_service_state_snapshot(
    expected_executable: &Path,
    _authoritative_daemon_pid: Option<u32>,
) -> Result<ServiceStateSnapshot> {
    validate_expected_executable(expected_executable)?;
    #[cfg(target_os = "macos")]
    {
        let definition = optional_service_file_snapshot(&plist_path()?)?;
        if let Some(definition) = definition.as_ref() {
            validate_launchd_definition_owner(&definition.contents, expected_executable)?;
        }
        let runtime = capture_launchd_runtime_for_snapshot_with(
            launchd_is_loaded,
            launchd_manager_pid_checked,
        )?;
        if definition.is_none() && runtime.loaded {
            anyhow::bail!(
                "LaunchAgent is loaded without an exact definition that can bind runtime ownership"
            );
        }
        Ok(ServiceStateSnapshot {
            definition,
            loaded: runtime.loaded,
            manager_pid: runtime.manager_pid,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let definition = optional_service_file_snapshot(&unit_path()?)?;
        if let Some(definition) = definition.as_ref() {
            validate_systemd_definition_owner(&definition.contents, expected_executable)?;
        }
        let active = systemctl_query("is-active")?;
        let enabled = systemctl_query("is-enabled")?;
        if definition.is_none() && (active || enabled) {
            anyhow::bail!(
                "systemd user service is active or enabled without an exact definition that can bind runtime ownership"
            );
        }
        let manager_pid = systemd_manager_pid_checked()?;
        require_manager_runtime_consistency("systemd", "active", active, "MainPID", manager_pid)?;
        Ok(ServiceStateSnapshot {
            definition,
            enabled,
            active,
            manager_pid,
        })
    }
    #[cfg(target_os = "windows")]
    {
        let definition = optional_windows_task_snapshot(&windows_task_file_path()?)?;
        if let Some(definition) = definition.as_ref() {
            validate_task_scheduler_definition_owner(&definition.contents, expected_executable)?;
            validate_task_scheduler_definition_owner(
                &definition.scheduler_xml,
                expected_executable,
            )?;
        }
        let manager_pid = if definition.is_some() {
            task_scheduler_manager_pid_checked(_authoritative_daemon_pid)?
        } else {
            None
        };
        Ok(ServiceStateSnapshot {
            definition,
            manager_pid,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service lifecycle snapshots are not supported on this platform")
}

pub(crate) fn require_service_state_snapshot(
    expected_executable: &Path,
    expected: &ServiceStateSnapshot,
) -> Result<()> {
    require_service_state_snapshot_with_manager(
        expected_executable,
        expected,
        expected.manager_pid(),
    )
}

pub(crate) fn require_service_state_snapshot_with_manager(
    expected_executable: &Path,
    expected: &ServiceStateSnapshot,
    expected_manager_pid: Option<u32>,
) -> Result<()> {
    let observed = capture_service_state_snapshot(expected_executable, expected_manager_pid)?;
    if !observed.matches_exact_state(&expected.with_manager_pid(expected_manager_pid)) {
        anyhow::bail!(
            "daemon service definition or exact manager runtime state changed during the lifecycle transaction"
        );
    }
    Ok(())
}

pub(crate) fn require_service_manager_stopped_state(
    expected_executable: &Path,
    expected: &ServiceStateSnapshot,
) -> Result<()> {
    let observed = capture_service_state_snapshot(expected_executable, None)?;
    if !observed.matches_exact_state(&expected.with_manager_stopped()) {
        anyhow::bail!(
            "daemon service definition or exact stopped-manager state changed during lifecycle restoration"
        );
    }
    Ok(())
}

pub(crate) fn require_service_absent_state(expected_executable: &Path) -> Result<()> {
    let observed = capture_service_state_snapshot(expected_executable, None)?;
    if observed.is_installed() || observed.manager_pid().is_some() {
        anyhow::bail!("daemon service definition or manager runtime remained after uninstall");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn require_service_snapshot(path: &Path, expected: Option<&ServiceFileSnapshot>) -> Result<()> {
    match expected {
        Some(expected) => {
            let observed = crate::fs_ops::token_for_path(path)
                .with_context(|| format!("binding service definition {}", path.display()))?;
            if observed != expected.token {
                anyhow::bail!(
                    "service definition {} changed during the operation; expected token {}, observed {}",
                    path.display(),
                    expected.token,
                    observed
                );
            }
        }
        None => {
            if crate::fs_ops::token_if_present(path)?.is_some() {
                anyhow::bail!(
                    "service definition {} appeared during the operation; refusing to replace it",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn definition_snapshot_matches(current: Option<&[u8]>, expected: Option<&[u8]>) -> bool {
    match (current, expected) {
        (Some(current), Some(expected)) => current == expected,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(any(target_os = "macos", test))]
fn validate_launchd_definition_value(
    definition: &serde_json::Value,
    expected_executable: &Path,
) -> Result<()> {
    let expected = expected_executable.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "expected LaunchAgent executable is not valid Unicode: {}",
            expected_executable.display()
        )
    })?;
    let label = definition.get("Label").and_then(serde_json::Value::as_str);
    let arguments = definition
        .get("ProgramArguments")
        .and_then(serde_json::Value::as_array);
    let owned = label == Some(LAUNCHD_LABEL)
        && arguments.is_some_and(|arguments| {
            arguments.len() == 4
                && arguments[0].as_str() == Some(expected)
                && arguments[1].as_str() == Some("daemon")
                && arguments[2].as_str() == Some("start")
                && arguments[3].as_str() == Some("--foreground")
        });
    if !owned {
        anyhow::bail!(
            "LaunchAgent definition is not owned by executable {}",
            expected_executable.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_launchd_definition_owner(contents: &[u8], expected_executable: &Path) -> Result<()> {
    use std::io::Write;

    // Parse the exact bytes that were snapshotted for the transaction. Reading
    // the live path through `plutil` would introduce a validation/removal race.
    let mut snapshot = tempfile::NamedTempFile::new()
        .context("creating temporary LaunchAgent ownership snapshot")?;
    snapshot
        .write_all(contents)
        .context("writing temporary LaunchAgent ownership snapshot")?;
    snapshot
        .flush()
        .context("flushing temporary LaunchAgent ownership snapshot")?;
    let output = std::process::Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(snapshot.path())
        .output()
        .context("parsing LaunchAgent definition with plutil")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "could not parse LaunchAgent definition for ownership: {}",
            if detail.is_empty() {
                "plutil failed without diagnostics"
            } else {
                &detail
            }
        );
    }
    let definition: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing plutil LaunchAgent JSON output")?;
    validate_launchd_definition_value(&definition, expected_executable)
}

#[cfg(any(target_os = "linux", test))]
fn validate_systemd_definition_owner(contents: &[u8], expected_executable: &Path) -> Result<()> {
    let expected = expected_executable.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "expected systemd executable is not valid Unicode: {}",
            expected_executable.display()
        )
    })?;
    let expected_exec = format!(
        "ExecStart={} daemon start --foreground",
        systemd_exec_quote(expected)
    );
    let definition = std::str::from_utf8(contents).context("systemd definition is not UTF-8")?;
    let exec_lines = definition
        .lines()
        .filter(|line| line.trim_start().starts_with("ExecStart="))
        .collect::<Vec<_>>();
    if exec_lines != [expected_exec.as_str()] {
        anyhow::bail!(
            "systemd definition is not owned by executable {}",
            expected_executable.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn staged_service_file(path: &std::path::Path, contents: &[u8]) -> Result<tempfile::NamedTempFile> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("service path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let suffix = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let mut staged = tempfile::Builder::new()
        .prefix(".codex-switch-global-pace-service-")
        .suffix(&suffix)
        .tempfile_in(parent)?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o644))?;
    staged.write_all(contents)?;
    staged.as_file().sync_all()?;
    Ok(staged)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn service_transaction_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .with_context(|| format!("service definition has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("service definition has no file name: {}", path.display()))?;
    let mut name = std::ffi::OsString::from(".");
    name.push(file_name);
    name.push(suffix);
    Ok(parent.join(name))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn require_service_transaction_path_absent(path: &Path, purpose: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting {purpose} {}", path.display()))
        }
        Ok(_) => anyhow::bail!(
            "{purpose} already exists at {}; refusing to overwrite recovery data",
            path.display()
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
enum ServiceFileCleanupOutcome {
    Durable,
    DurabilityUnconfirmed(String),
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ServiceFileCleanupOutcome {
    fn unconfirmed_note(&self) -> Option<&str> {
        match self {
            Self::Durable => None,
            Self::DurabilityUnconfirmed(note) => Some(note),
        }
    }

    fn require_durable(self) -> Result<()> {
        match self {
            Self::Durable => Ok(()),
            Self::DurabilityUnconfirmed(note) => Err(anyhow::anyhow!(note)),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn remove_service_file_exact(
    path: &Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<ServiceFileCleanupOutcome> {
    let outcome = crate::fs_ops::remove_exact(path, expected)
        .with_context(|| format!("removing {purpose} at {}", path.display()))?;
    match outcome {
        crate::fs_ops::RemoveExactOutcome::Removed => Ok(ServiceFileCleanupOutcome::Durable),
        crate::fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed => {
            match crate::fs_ops::sync_parent(path) {
                Ok(()) => Ok(ServiceFileCleanupOutcome::Durable),
                Err(error) => Ok(ServiceFileCleanupOutcome::DurabilityUnconfirmed(format!(
                    "the exact {purpose} was removed from {}, but retrying parent-directory durability failed: {error:#}",
                    path.display()
                ))),
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn create_service_file_copy_durable(
    source: &Path,
    destination: &Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<crate::fs_ops::FileToken> {
    let outcome = crate::fs_ops::create_exclusive_copy(source, destination, expected)
        .with_context(|| format!("creating {purpose} at {}", destination.display()))?;
    match outcome {
        crate::fs_ops::CreateExactOutcome::Created(token) => Ok(token),
        crate::fs_ops::CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(token) => {
            match crate::fs_ops::sync_parent(destination) {
                Ok(()) => Ok(token),
                Err(sync_error) => {
                    let cleanup = remove_service_file_exact(destination, &token, purpose);
                    anyhow::bail!(
                        "{purpose} was created at {}, but retrying parent-directory durability failed: {sync_error:#}. Exact cleanup was {}",
                        destination.display(),
                        service_cleanup_result(cleanup)
                    )
                }
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn service_cleanup_result(result: Result<ServiceFileCleanupOutcome>) -> String {
    match result {
        Ok(ServiceFileCleanupOutcome::Durable) => "complete and durable".to_string(),
        Ok(ServiceFileCleanupOutcome::DurabilityUnconfirmed(note)) => {
            format!("applied, but durability is unconfirmed: {note}")
        }
        Err(error) => format!("incomplete: {error:#}"),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn service_operation_result(result: Result<()>) -> String {
    match result {
        Ok(()) => "complete".to_string(),
        Err(error) => format!("incomplete: {error:#}"),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn confirm_service_namespace_durability(
    boundary: Result<()>,
    path: &Path,
    operation: &str,
) -> Result<()> {
    match crate::fs_ops::sync_parent(path) {
        Ok(()) => Ok(()),
        Err(sync_error) => match boundary {
            Ok(()) => Err(sync_error).with_context(|| {
                format!(
                    "{operation} reached its verified namespace state, but directory durability was not confirmed"
                )
            }),
            Err(boundary_error) => Err(boundary_error).context(format!(
                "{operation} reached its verified namespace state after the namespace call reported an error, and retrying directory durability also failed: {sync_error:#}"
            )),
        },
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceFileTransactionState {
    Pending,
    Finished,
    Preserved,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct ServiceFilePublication {
    path: PathBuf,
    published_token: crate::fs_ops::FileToken,
    displaced: Option<(PathBuf, crate::fs_ops::FileToken)>,
    independent_backup: Option<(PathBuf, crate::fs_ops::FileToken)>,
    state: ServiceFileTransactionState,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ServiceFilePublication {
    fn commit(&mut self) -> Result<()> {
        match self.state {
            ServiceFileTransactionState::Pending => {}
            ServiceFileTransactionState::Finished => return Ok(()),
            ServiceFileTransactionState::Preserved => {
                anyhow::bail!("service-definition publication is preserved for manual recovery")
            }
        }
        let result = (|| -> Result<Vec<String>> {
            let live = crate::fs_ops::token_for_path(&self.path).with_context(|| {
                format!(
                    "binding published service definition {}",
                    self.path.display()
                )
            })?;
            if live != self.published_token {
                anyhow::bail!(
                    "published service definition changed before commit at {}; expected token {}, observed {}",
                    self.path.display(),
                    self.published_token,
                    live
                );
            }
            let mut unconfirmed = Vec::new();
            if let Some((path, token)) = self.displaced.as_ref() {
                let outcome =
                    remove_service_file_exact(path, token, "displaced service definition")?;
                if let Some(note) = outcome.unconfirmed_note() {
                    unconfirmed.push(note.to_string());
                }
            }
            if let Some((path, token)) = self.independent_backup.as_ref() {
                let outcome = remove_service_file_exact(
                    path,
                    token,
                    "independent service-definition backup",
                )?;
                if let Some(note) = outcome.unconfirmed_note() {
                    unconfirmed.push(note.to_string());
                }
            }
            Ok(unconfirmed)
        })();
        match result {
            Ok(unconfirmed) => {
                self.state = ServiceFileTransactionState::Finished;
                if unconfirmed.is_empty() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "the service definition was committed and its recovery entries were removed, but cleanup durability remained unconfirmed: {}",
                        unconfirmed.join("; ")
                    ))
                }
            }
            Err(error) => {
                self.state = ServiceFileTransactionState::Preserved;
                Err(error)
            }
        }
    }

    fn rollback(&mut self) -> Result<()> {
        match self.state {
            ServiceFileTransactionState::Pending => {}
            ServiceFileTransactionState::Finished => return Ok(()),
            ServiceFileTransactionState::Preserved => {
                anyhow::bail!("service-definition publication is preserved for manual recovery")
            }
        }
        self.state = ServiceFileTransactionState::Preserved;
        if let Some((displaced_path, previous_token)) = self.displaced.as_ref() {
            require_service_file_token(
                &self.path,
                &self.published_token,
                "published service definition before rollback",
            )?;
            require_service_file_token(
                displaced_path,
                previous_token,
                "displaced previous service definition before rollback",
            )?;
            let exchange_result = crate::fs_ops::exchange(displaced_path, &self.path);
            let live_after = crate::fs_ops::token_if_present(&self.path)?;
            let displaced_after = crate::fs_ops::token_if_present(displaced_path)?;
            if live_after.as_ref() != Some(previous_token)
                || displaced_after.as_ref() != Some(&self.published_token)
            {
                anyhow::bail!(
                    "service-definition rollback ended in an unclassified state; live {}, displaced {}, and backup recovery files were preserved",
                    self.path.display(),
                    displaced_path.display()
                );
            }
            confirm_service_namespace_durability(
                exchange_result,
                &self.path,
                "restoring the previous service definition",
            )?;
            remove_service_file_exact(
                displaced_path,
                &self.published_token,
                "rolled-back service candidate",
            )?
            .require_durable()?;
        } else {
            require_service_file_token(
                &self.path,
                &self.published_token,
                "new service definition before rollback",
            )?;
            remove_service_file_exact(
                &self.path,
                &self.published_token,
                "rolled-back new service definition",
            )?
            .require_durable()?;
        }
        if let Some((path, token)) = self.independent_backup.as_ref() {
            remove_service_file_exact(path, token, "redundant service-definition backup")?
                .require_durable()?;
        }
        self.state = ServiceFileTransactionState::Finished;
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for ServiceFilePublication {
    fn drop(&mut self) {
        if self.state == ServiceFileTransactionState::Pending {
            self.state = ServiceFileTransactionState::Preserved;
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn require_service_file_token(
    path: &Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<()> {
    let observed = crate::fs_ops::token_for_path(path)
        .with_context(|| format!("binding {purpose} at {}", path.display()))?;
    if &observed != expected {
        anyhow::bail!(
            "{purpose} changed at {}; expected token {}, observed {}",
            path.display(),
            expected,
            observed
        );
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn publish_service_file(
    staged: tempfile::NamedTempFile,
    path: &Path,
    previous: Option<&ServiceFileSnapshot>,
) -> Result<ServiceFilePublication> {
    let candidate = service_transaction_path(path, ".candidate")?;
    let backup = service_transaction_path(path, ".backup")?;
    require_service_transaction_path_absent(&candidate, "staged service definition")?;
    require_service_transaction_path_absent(&backup, "service-definition backup")?;

    let staged_token = crate::fs_ops::token_for_path(staged.path())
        .context("binding validated staged service definition")?;
    let published_token = create_service_file_copy_durable(
        staged.path(),
        &candidate,
        &staged_token,
        "staged service definition",
    )
    .with_context(|| format!("staging service definition at {}", candidate.display()))?;
    drop(staged);

    let independent_backup = if let Some(previous) = previous {
        match create_service_file_copy_durable(
            path,
            &backup,
            &previous.token,
            "independent service-definition backup",
        ) {
            Ok(token) => Some((backup.clone(), token)),
            Err(error) => {
                let candidate_cleanup = remove_service_file_exact(
                    &candidate,
                    &published_token,
                    "unused staged service definition",
                );
                return Err(error.context(format!(
                    "preserving the existing service definition failed; candidate cleanup was {}",
                    service_cleanup_result(candidate_cleanup)
                )));
            }
        }
    } else {
        None
    };

    if let Some(previous) = previous {
        require_service_snapshot(path, Some(previous))?;
        if let Some((backup_path, backup_token)) = independent_backup.as_ref() {
            require_service_file_token(
                backup_path,
                backup_token,
                "independent service-definition backup",
            )?;
        }
        let exchange_result = crate::fs_ops::exchange(&candidate, path);
        let live_after = crate::fs_ops::token_if_present(path)?;
        let displaced_after = crate::fs_ops::token_if_present(&candidate)?;
        if live_after.as_ref() == Some(&published_token)
            && displaced_after.as_ref() == Some(&previous.token)
        {
            let mut publication = ServiceFilePublication {
                path: path.to_path_buf(),
                published_token,
                displaced: Some((candidate, previous.token.clone())),
                independent_backup,
                state: ServiceFileTransactionState::Pending,
            };
            if let Err(error) = confirm_service_namespace_durability(
                exchange_result,
                path,
                "publishing the service definition",
            ) {
                let rollback = publication.rollback();
                return Err(error.context(format!(
                    "service definition was exchanged but publication durability was not confirmed; rollback was {}",
                    service_operation_result(rollback)
                )));
            }
            return Ok(publication);
        }

        if live_after.as_ref() == Some(&published_token)
            && let Some(displaced_token) = displaced_after.as_ref()
        {
            let restore_result = crate::fs_ops::exchange(&candidate, path);
            let restored_live = crate::fs_ops::token_if_present(path)?;
            let restored_candidate = crate::fs_ops::token_if_present(&candidate)?;
            if restored_live.as_ref() == Some(displaced_token)
                && restored_candidate.as_ref() == Some(&published_token)
                && confirm_service_namespace_durability(
                    restore_result,
                    path,
                    "restoring the actual displaced service-definition writer",
                )
                .is_ok()
            {
                let candidate_cleanup = remove_service_file_exact(
                    &candidate,
                    &published_token,
                    "restored staged service definition",
                );
                let backup_cleanup = match independent_backup.as_ref() {
                    Some((backup_path, backup_token)) => remove_service_file_exact(
                        backup_path,
                        backup_token,
                        "unused service-definition backup",
                    ),
                    None => Ok(ServiceFileCleanupOutcome::Durable),
                };
                anyhow::bail!(
                    "service definition changed at the exchange boundary; the actual displaced writer was restored. Candidate cleanup was {} and backup cleanup was {}",
                    service_cleanup_result(candidate_cleanup),
                    service_cleanup_result(backup_cleanup)
                );
            }
            anyhow::bail!(
                "service definition changed at the exchange boundary and exact restoration failed; live {}, displaced {}, and backup recovery files were preserved",
                path.display(),
                candidate.display()
            );
        }

        if live_after.as_ref() == Some(&previous.token)
            && displaced_after.as_ref() == Some(&published_token)
        {
            let candidate_cleanup = remove_service_file_exact(
                &candidate,
                &published_token,
                "unpublished staged service definition",
            );
            let backup_cleanup = match independent_backup.as_ref() {
                Some((backup_path, backup_token)) => remove_service_file_exact(
                    backup_path,
                    backup_token,
                    "unused service-definition backup",
                ),
                None => Ok(ServiceFileCleanupOutcome::Durable),
            };
            let boundary_error = exchange_result
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_else(|| "exchange returned an unexpected post-state".to_string());
            anyhow::bail!(
                "service definition was not published ({boundary_error}); candidate cleanup was {} and backup cleanup was {}",
                service_cleanup_result(candidate_cleanup),
                service_cleanup_result(backup_cleanup)
            );
        }

        anyhow::bail!(
            "service-definition exchange ended in an unclassified external-writer state; live {}, displaced {}, and backup recovery files were preserved",
            path.display(),
            candidate.display()
        );
    }

    let publish_result = crate::fs_ops::rename_noreplace(&candidate, path);
    let live_after = crate::fs_ops::token_if_present(path)?;
    let candidate_after = crate::fs_ops::token_if_present(&candidate)?;
    if live_after.as_ref() == Some(&published_token) && candidate_after.is_none() {
        let mut publication = ServiceFilePublication {
            path: path.to_path_buf(),
            published_token,
            displaced: None,
            independent_backup: None,
            state: ServiceFileTransactionState::Pending,
        };
        if let Err(error) = confirm_service_namespace_durability(
            publish_result,
            path,
            "publishing the new service definition without replacement",
        ) {
            let rollback = publication.rollback();
            return Err(error.context(format!(
                "new service definition reached its public path but publication durability was not confirmed; rollback was {}",
                service_operation_result(rollback)
            )));
        }
        return Ok(publication);
    }
    if candidate_after.as_ref() == Some(&published_token) {
        let candidate_cleanup = remove_service_file_exact(
            &candidate,
            &published_token,
            "unpublished staged service definition",
        );
        anyhow::bail!(
            "new service definition was not published without replacement; candidate cleanup was {}",
            service_cleanup_result(candidate_cleanup)
        );
    }
    anyhow::bail!(
        "new service-definition publication ended in an unclassified external-writer state; live {} and candidate {} were preserved",
        path.display(),
        candidate.display()
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct ServiceFileRemoval {
    path: PathBuf,
    removed: PathBuf,
    removed_token: crate::fs_ops::FileToken,
    state: ServiceFileTransactionState,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
enum ServiceFileRemovalCommit {
    Durable,
    AppliedCleanupUnconfirmed(anyhow::Error),
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ServiceFileRemoval {
    fn rollback(&mut self) -> Result<()> {
        match self.state {
            ServiceFileTransactionState::Pending => {}
            ServiceFileTransactionState::Finished => return Ok(()),
            ServiceFileTransactionState::Preserved => {
                anyhow::bail!("removed service definition is preserved for manual recovery")
            }
        }
        self.state = ServiceFileTransactionState::Preserved;
        if crate::fs_ops::token_if_present(&self.path)?.is_some() {
            anyhow::bail!(
                "service definition {} was claimed during uninstall; removed definition remains preserved at {}",
                self.path.display(),
                self.removed.display()
            );
        }
        require_service_file_token(
            &self.removed,
            &self.removed_token,
            "removed service definition before rollback",
        )?;
        let restore_result = crate::fs_ops::rename_noreplace(&self.removed, &self.path);
        let live_after = crate::fs_ops::token_if_present(&self.path)?;
        let removed_after = crate::fs_ops::token_if_present(&self.removed)?;
        if live_after.as_ref() != Some(&self.removed_token) || removed_after.is_some() {
            anyhow::bail!(
                "service-definition uninstall rollback ended in an unclassified state; live {} and removed {} were preserved",
                self.path.display(),
                self.removed.display()
            );
        }
        self.state = ServiceFileTransactionState::Finished;
        confirm_service_namespace_durability(
            restore_result,
            &self.path,
            "restoring the removed service definition",
        )
    }

    fn commit(&mut self) -> Result<ServiceFileRemovalCommit> {
        match self.state {
            ServiceFileTransactionState::Pending => {}
            ServiceFileTransactionState::Finished => {
                return Ok(ServiceFileRemovalCommit::Durable);
            }
            ServiceFileTransactionState::Preserved => {
                anyhow::bail!("removed service definition is preserved for manual recovery")
            }
        }
        if crate::fs_ops::token_if_present(&self.path)?.is_some() {
            self.state = ServiceFileTransactionState::Preserved;
            anyhow::bail!(
                "service definition {} appeared before uninstall commit; owned removed definition was preserved at {}",
                self.path.display(),
                self.removed.display()
            );
        }
        let cleanup = match remove_service_file_exact(
            &self.removed,
            &self.removed_token,
            "removed service definition",
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                let removed_after = crate::fs_ops::token_if_present(&self.removed).with_context(|| {
                    format!(
                        "classifying preserved service-uninstall recovery path after cleanup failure: {}",
                        self.removed.display()
                    )
                })?;
                self.state = if removed_after.as_ref() == Some(&self.removed_token)
                    && crate::fs_ops::token_if_present(&self.path)?.is_none()
                {
                    ServiceFileTransactionState::Pending
                } else {
                    ServiceFileTransactionState::Preserved
                };
                return Err(error.context(format!(
                    "committing service uninstall at {}",
                    self.path.display()
                )));
            }
        };
        Ok(self.finish_commit(cleanup))
    }

    fn finish_commit(&mut self, cleanup: ServiceFileCleanupOutcome) -> ServiceFileRemovalCommit {
        self.state = ServiceFileTransactionState::Finished;
        match cleanup {
            ServiceFileCleanupOutcome::Durable => ServiceFileRemovalCommit::Durable,
            ServiceFileCleanupOutcome::DurabilityUnconfirmed(note) => {
                ServiceFileRemovalCommit::AppliedCleanupUnconfirmed(anyhow::anyhow!(note))
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for ServiceFileRemoval {
    fn drop(&mut self) {
        if self.state == ServiceFileTransactionState::Pending {
            self.state = ServiceFileTransactionState::Preserved;
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn begin_service_file_removal(
    path: &Path,
    snapshot: &ServiceFileSnapshot,
) -> Result<ServiceFileRemoval> {
    let removed = service_transaction_path(path, ".removed")?;
    require_service_transaction_path_absent(&removed, "removed service definition")?;
    require_service_snapshot(path, Some(snapshot))?;

    let remove_result = crate::fs_ops::rename_noreplace(path, &removed);
    let live_after = crate::fs_ops::token_if_present(path)?;
    let removed_after = crate::fs_ops::token_if_present(&removed)?;
    if live_after.is_none() && removed_after.as_ref() == Some(&snapshot.token) {
        let mut removal = ServiceFileRemoval {
            path: path.to_path_buf(),
            removed,
            removed_token: snapshot.token.clone(),
            state: ServiceFileTransactionState::Pending,
        };
        if let Err(error) = confirm_service_namespace_durability(
            remove_result,
            path,
            "moving the service definition to its uninstall recovery path",
        ) {
            let rollback = removal.rollback();
            return Err(error.context(format!(
                "service definition was removed but directory durability was not confirmed; rollback was {}",
                service_operation_result(rollback)
            )));
        }
        return Ok(removal);
    }

    if live_after.as_ref() == Some(&snapshot.token) && removed_after.is_none() {
        let boundary_error = remove_result
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_else(|| "no-replace removal returned an unexpected post-state".to_string());
        anyhow::bail!("service definition was not removed: {boundary_error}");
    }

    if live_after.is_none()
        && let Some(displaced_token) = removed_after.as_ref()
        && displaced_token != &snapshot.token
    {
        let restoration = crate::fs_ops::rename_noreplace(&removed, path);
        let restored_live = crate::fs_ops::token_if_present(path)?;
        let restored_removed = crate::fs_ops::token_if_present(&removed)?;
        if restored_live.as_ref() == Some(displaced_token)
            && restored_removed.is_none()
            && restoration.is_ok()
        {
            anyhow::bail!(
                "service definition changed at the uninstall boundary; the actual displaced writer was restored without replacement"
            );
        }
        anyhow::bail!(
            "service definition changed at the uninstall boundary and exact restoration failed; live {} and removed {} were preserved",
            path.display(),
            removed.display()
        );
    }

    anyhow::bail!(
        "service-definition removal ended in an unclassified external-writer state; live {} and removed {} were preserved",
        path.display(),
        removed.display()
    )
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &str) -> String {
    systemd_quote_with(value, false)
}

#[cfg(any(target_os = "linux", test))]
fn systemd_exec_quote(value: &str) -> String {
    // systemd expands `$NAME` in command lines, including inside quotes. `$$`
    // is the documented literal-dollar form. Environment= values do not use
    // that command-line expansion and must retain their single dollar signs.
    systemd_quote_with(value, true)
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote_with(value: &str, escape_dollar: bool) -> String {
    use std::fmt::Write as _;

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            '$' if escape_dollar => escaped.push_str("$$"),
            '\u{7}' => escaped.push_str("\\a"),
            '\u{8}' => escaped.push_str("\\b"),
            '\u{c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{b}' => escaped.push_str("\\v"),
            control if control.is_control() => {
                let mut encoded = [0_u8; 4];
                for byte in control.encode_utf8(&mut encoded).bytes() {
                    write!(escaped, "\\x{byte:02x}").expect("writing to a String cannot fail");
                }
            }
            ordinary => escaped.push(ordinary),
        }
    }
    escaped.push('"');
    escaped
}

fn wait_for_daemon_absence_after_service_stop(action: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + super::DAEMON_TRANSITION_TIMEOUT;
    loop {
        match crate::daemon::pidfile::running_pid_checked()
            .with_context(|| format!("checking the daemon PID lock after {action}"))?
        {
            None => return Ok(()),
            Some(pid) if std::time::Instant::now() >= deadline => {
                anyhow::bail!(
                    "{action} completed, but daemon PID {pid} still owns the PID lock after {}s",
                    super::DAEMON_TRANSITION_TIMEOUT.as_secs()
                );
            }
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

fn wait_for_daemon_presence_after_service_start(
    action: &str,
    expected_executable: &Path,
) -> Result<()> {
    let deadline = std::time::Instant::now() + super::DAEMON_TRANSITION_TIMEOUT;
    let mut last_probe_error = None;
    while std::time::Instant::now() < deadline {
        match crate::daemon::pidfile::running_identity_checked() {
            Ok(Some((pid, running_executable))) => {
                validate_started_service_daemon_identity(
                    action,
                    expected_executable,
                    pid,
                    &running_executable,
                )?;
                return Ok(());
            }
            Ok(None) => {}
            Err(error) => last_probe_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if let Some(error) = last_probe_error {
        return Err(error.context(format!(
            "{action} did not publish a readable, locked PID identity within {} seconds",
            super::DAEMON_TRANSITION_TIMEOUT.as_secs()
        )));
    }
    anyhow::bail!(
        "{action} did not publish a live PID within {} seconds",
        super::DAEMON_TRANSITION_TIMEOUT.as_secs()
    )
}

fn validate_started_service_daemon_identity(
    action: &str,
    expected_executable: &Path,
    pid: u32,
    running_executable: &Path,
) -> Result<()> {
    super::validate_running_daemon_executable(expected_executable, pid, running_executable)
        .map(|_| ())
        .with_context(|| {
            format!("{action} published a daemon identity from an unexpected executable")
        })
}

fn require_started_service_daemon_identity(
    action: &str,
    expected_executable: &Path,
) -> Result<u32> {
    let (pid, running_executable) = crate::daemon::pidfile::running_identity_checked()?
        .with_context(|| format!("{action} did not retain a locked daemon identity"))?;
    validate_started_service_daemon_identity(
        action,
        expected_executable,
        pid,
        &running_executable,
    )?;
    Ok(pid)
}

fn transaction_error_with_restoration(
    error: anyhow::Error,
    restoration: Result<()>,
    restored_context: &str,
    incomplete_context: &str,
) -> anyhow::Error {
    match restoration {
        Ok(()) => error.context(restored_context.to_owned()),
        Err(restoration_error) => {
            error.context(format!("{incomplete_context}: {restoration_error:#}"))
        }
    }
}

pub(crate) fn start_installed_locked(
    expected_executable: &Path,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_expected_executable(expected_executable)?;
    validate_uninstall_owner(expected_executable)?;
    #[cfg(target_os = "macos")]
    return start_launchd(expected_executable);
    #[cfg(target_os = "linux")]
    return start_systemd(expected_executable);
    #[cfg(target_os = "windows")]
    return start_task_scheduler(expected_executable);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service start is not supported on this platform")
}

pub(crate) fn stop_installed_locked(
    expected_executable: &Path,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_expected_executable(expected_executable)?;
    validate_uninstall_owner(expected_executable)?;
    #[cfg(target_os = "macos")]
    return stop_launchd(expected_executable);
    #[cfg(target_os = "linux")]
    return stop_systemd(expected_executable);
    #[cfg(target_os = "windows")]
    return stop_task_scheduler(expected_executable);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service stop is not supported on this platform")
}

/// Stop only the owned Unix service manager entry while retaining the
/// service-operation lease. The caller is responsible for resolving any
/// independently started foreground PID generation and proving final PID
/// absence. This separation is required at the installer restart race: a
/// foreground winner is not owned by launchd/systemd, so manager shutdown and
/// generation-bound shutdown are two distinct authorities.
#[cfg(not(target_os = "windows"))]
pub(crate) fn stop_installed_manager_locked(
    expected_executable: &Path,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_expected_executable(expected_executable)?;
    validate_uninstall_owner(expected_executable)?;
    #[cfg(target_os = "macos")]
    return stop_launchd_manager(expected_executable);
    #[cfg(target_os = "linux")]
    return stop_systemd_manager(expected_executable);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("Service stop is not supported on this platform")
}

/// Stop the owned Unix service-manager generation while reporting the exact
/// boundary at which the manager command was successfully spawned. This lets
/// the installer distinguish pre-mutation validation/spawn failures from a
/// request that may still finish after a later postcondition error.
#[cfg(not(target_os = "windows"))]
pub(crate) fn stop_installed_manager_observed_locked<F>(
    expected_executable: &Path,
    _lease: &ServiceOperationLease,
    request_spawned: F,
) -> Result<bool>
where
    F: FnOnce(),
{
    validate_expected_executable(expected_executable)?;
    validate_uninstall_owner(expected_executable)?;
    #[cfg(target_os = "macos")]
    return stop_launchd_manager_observed(expected_executable, request_spawned);
    #[cfg(target_os = "linux")]
    return stop_systemd_manager_observed(expected_executable, request_spawned);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = request_spawned;
        anyhow::bail!("Service stop is not supported on this platform")
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn restore_uninstall_running_state_locked(
    expected_executable: &Path,
    was_running: bool,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_uninstall_owner(expected_executable)?;
    let running = crate::daemon::pidfile::running_pid_checked()
        .context("checking daemon state after failed scheduled-task uninstall")?
        .is_some();
    if was_running && !running {
        schtasks(
            &["/Run", "/TN", WINDOWS_TASK_NAME],
            "restore daemon after failed scheduled-task uninstall",
        )?;
        wait_for_scheduled_daemon(expected_executable)?;
    } else if !was_running && running {
        anyhow::bail!(
            "a daemon generation started during failed scheduled-task uninstall; refusing to claim the prior stopped state was restored"
        );
    }
    if was_running {
        require_started_service_daemon_identity(
            "restored scheduled-task daemon",
            expected_executable,
        )?;
    } else if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
        .context("verifying daemon state after failed scheduled-task uninstall")?
    {
        anyhow::bail!("daemon PID {pid} appeared while restoring a stopped scheduled-task state");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn start_task_scheduler(expected_executable: &Path) -> Result<()> {
    let task_file = windows_task_file_path()?;
    let definition =
        std::fs::read(&task_file).context("reading Windows scheduled-task definition")?;
    let exported = query_scheduled_task_xml("export scheduled task before start")?;
    validate_task_scheduler_definition_owner(&definition, expected_executable)?;
    validate_task_scheduler_definition_owner(&exported, expected_executable)?;
    require_task_definition_snapshot(&task_file, Some(&definition))?;
    require_task_xml_snapshot(&task_file, Some(&exported))?;
    schtasks(&["/Run", "/TN", WINDOWS_TASK_NAME], "start scheduled task")?;
    if let Err(start_err) = wait_for_scheduled_daemon(expected_executable) {
        if let Err(stop_err) = stop_scheduled_daemon_for_rollback() {
            return Err(start_err.context(format!(
                "scheduled daemon did not become ready and cleanup also failed: {stop_err}"
            )));
        }
        return Err(start_err.context("scheduled daemon did not become ready and was stopped"));
    }
    user_println("Started Windows scheduled task");
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn stop_failed_start_for_self_update_locked(
    _lease: &ServiceOperationLease,
) -> Result<()> {
    stop_scheduled_daemon_for_rollback()
}

#[cfg(target_os = "windows")]
fn stop_task_scheduler(expected_executable: &Path) -> Result<()> {
    let task_file = windows_task_file_path()?;
    let definition =
        std::fs::read(&task_file).context("reading Windows scheduled-task definition")?;
    let exported = query_scheduled_task_xml("export scheduled task before stop")?;
    validate_task_scheduler_definition_owner(&definition, expected_executable)?;
    validate_task_scheduler_definition_owner(&exported, expected_executable)?;
    require_task_definition_snapshot(&task_file, Some(&definition))?;
    require_task_xml_snapshot(&task_file, Some(&exported))?;
    schtasks(&["/End", "/TN", WINDOWS_TASK_NAME], "stop scheduled task")?;
    wait_for_daemon_absence_after_service_stop("stopping the Windows scheduled task")?;
    user_println("Stopped Windows scheduled task");
    Ok(())
}

// -- macOS LaunchAgent --

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(home.join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist")))
}

#[cfg(any(target_os = "macos", test))]
fn launchd_plist(exe: &str, home: &str, codex_home: &str, app_home: &str) -> String {
    let exe = xml_escape(exe);
    let home = xml_escape(home);
    let codex_home = xml_escape(codex_home);
    let app_home = xml_escape(app_home);
    let label = LAUNCHD_LABEL;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{home}</string>
        <key>CODEX_HOME</key>
        <string>{codex_home}</string>
        <key>CODEX_SWITCH_HOME</key>
        <string>{app_home}</string>
    </dict>
</dict>
</plist>"#
    )
}

#[cfg(any(target_os = "macos", test))]
fn launchd_list_contains_label(stdout: &[u8]) -> Result<bool> {
    let stdout = std::str::from_utf8(stdout).context("launchctl list output is not UTF-8")?;
    Ok(stdout
        .lines()
        .any(|line| line.split_ascii_whitespace().last() == Some(LAUNCHD_LABEL)))
}

#[cfg(any(target_os = "macos", test))]
fn launchd_job_pid(stdout: &[u8]) -> Result<Option<u32>> {
    let stdout = std::str::from_utf8(stdout).context("launchctl job output is not UTF-8")?;
    let mut pid = None;
    for line in stdout.lines() {
        let line = line.trim();
        let Some((field, value)) = line.split_once('=') else {
            continue;
        };
        if field.trim().trim_matches('"') != "PID" {
            continue;
        }
        if pid.is_some() {
            anyhow::bail!("launchctl job output contained more than one PID field");
        }
        let value = value.trim().trim_end_matches(';').trim();
        let parsed = value
            .parse::<u32>()
            .with_context(|| format!("launchctl reported invalid job PID '{value}'"))?;
        if parsed == 0 {
            anyhow::bail!("launchctl reported reserved job PID zero");
        }
        pid = Some(parsed);
    }
    Ok(pid)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn service_command_failure_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("exit status {}", output.status)
}

#[cfg(target_os = "macos")]
fn launchd_is_loaded() -> Result<bool> {
    // `launchctl list <label>` uses a failing exit status for both absence and
    // query failures. A successful full listing makes exact label absence an
    // authoritative state instead of a guessed fallback.
    let output = std::process::Command::new("launchctl")
        .arg("list")
        .output()
        .context("querying loaded LaunchAgents")?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl list could not determine loaded LaunchAgents: {}",
            service_command_failure_detail(&output)
        );
    }
    launchd_list_contains_label(&output.stdout)
}

#[cfg(target_os = "macos")]
fn launchd_manager_pid_checked(loaded: bool) -> Result<Option<u32>> {
    if !loaded {
        return Ok(None);
    }
    let output = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output()
        .context("querying the loaded LaunchAgent runtime identity")?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl could not inspect the loaded LaunchAgent runtime identity: {}",
            service_command_failure_detail(&output)
        );
    }
    launchd_job_pid(&output.stdout)
}

#[cfg(target_os = "macos")]
fn require_no_definitionless_launchd_service() -> Result<()> {
    if launchd_is_loaded()? {
        anyhow::bail!(
            "LaunchAgent {LAUNCHD_LABEL} is loaded without a definition that can prove ownership"
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchctl_require_loaded_state(
    args: &[&str],
    expected_loaded: bool,
    action: &str,
) -> Result<()> {
    launchctl_require_loaded_state_observed(args, expected_loaded, action, || {})
}

#[cfg(target_os = "macos")]
fn launchctl_require_loaded_state_observed<F>(
    args: &[&str],
    expected_loaded: bool,
    action: &str,
    request_spawned: F,
) -> Result<()>
where
    F: FnOnce(),
{
    let child = std::process::Command::new("launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting launchctl for {action}"))?;
    request_spawned();
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for launchctl to {action}"))?;
    let loaded = launchd_is_loaded()
        .with_context(|| format!("verifying LaunchAgent state after {action}"))?;
    if loaded != expected_loaded {
        anyhow::bail!(
            "{action} did not leave LaunchAgent {LAUNCHD_LABEL} {}: {}",
            if expected_loaded {
                "loaded"
            } else {
                "unloaded"
            },
            service_command_failure_detail(&output)
        );
    }
    // Legacy `load`/`unload` can report per-job failures without a reliable
    // process exit status. The successful full-list postcondition above is the
    // sole state authority.
    Ok(())
}

#[cfg(target_os = "macos")]
fn unload_launchd(path: &Path) -> Result<()> {
    unload_launchd_observed(path, || {})
}

#[cfg(target_os = "macos")]
fn unload_launchd_observed<F>(path: &Path, request_spawned: F) -> Result<()>
where
    F: FnOnce(),
{
    let path = path.to_str().ok_or_else(|| {
        anyhow::anyhow!("LaunchAgent path is not valid Unicode: {}", path.display())
    })?;
    launchctl_require_loaded_state_observed(
        &["unload", path],
        false,
        "unload LaunchAgent",
        request_spawned,
    )
}

#[cfg(target_os = "macos")]
fn rollback_launchd_uninstall(
    path: &Path,
    previous: &ServiceFileSnapshot,
    removal: Option<&mut ServiceFileRemoval>,
    was_loaded: bool,
    expected_executable: &Path,
) -> Result<()> {
    if let Some(removal) = removal {
        removal
            .rollback()
            .with_context(|| format!("restoring LaunchAgent definition {}", path.display()))?;
    } else {
        require_service_snapshot(path, Some(previous))?;
    }
    let loaded = launchd_is_loaded()?;
    if was_loaded && !loaded {
        if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon absence before LaunchAgent rollback restart")?
        {
            anyhow::bail!(
                "daemon PID {pid} still owns the PID lock; refusing to load a second generation during LaunchAgent rollback"
            );
        }
        load_launchd(path).context("restoring the previously loaded LaunchAgent")?;
        wait_for_daemon_presence_after_service_start("restored LaunchAgent", expected_executable)?;
    }
    if launchd_is_loaded()? != was_loaded {
        anyhow::bail!("restored LaunchAgent loaded state did not match its pre-uninstall state");
    }
    if was_loaded {
        require_started_service_daemon_identity("restored LaunchAgent", expected_executable)?;
    }
    if !was_loaded {
        wait_for_daemon_absence_after_service_stop("LaunchAgent uninstall rollback")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rollback_launchd_install(
    path: &Path,
    publication: &mut ServiceFilePublication,
    was_loaded: bool,
    previous_executable: &Path,
) -> Result<()> {
    if launchd_is_loaded()? {
        unload_launchd(path).context("unloading failed new LaunchAgent during rollback")?;
    }
    wait_for_daemon_absence_after_service_stop("LaunchAgent install rollback")?;
    publication
        .rollback()
        .with_context(|| format!("restoring LaunchAgent definition {}", path.display()))?;
    if was_loaded {
        load_launchd(path).context("restoring the previously loaded LaunchAgent")?;
        wait_for_daemon_presence_after_service_start("restored LaunchAgent", previous_executable)?;
    }
    if launchd_is_loaded()? != was_loaded {
        anyhow::bail!("restored LaunchAgent loaded state did not match its pre-install state");
    }
    if !was_loaded {
        wait_for_daemon_absence_after_service_stop("LaunchAgent install rollback")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn restore_launchd_after_prepublication_stop(
    path: &Path,
    previous: Option<&ServiceFileSnapshot>,
    was_loaded: bool,
    previous_executable: &Path,
) -> Result<()> {
    require_service_snapshot(path, previous)?;
    let loaded = launchd_is_loaded()?;
    if was_loaded && !loaded {
        if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon absence before restoring the pre-install LaunchAgent")?
        {
            anyhow::bail!(
                "daemon PID {pid} appeared while restoring the pre-install LaunchAgent state"
            );
        }
        load_launchd(path).context("restoring LaunchAgent after pre-publication failure")?;
        wait_for_daemon_presence_after_service_start(
            "restored pre-install LaunchAgent",
            previous_executable,
        )?;
    } else if !was_loaded && loaded {
        anyhow::bail!(
            "LaunchAgent became loaded while restoring an initially unloaded install state"
        );
    }
    if launchd_is_loaded()? != was_loaded {
        anyhow::bail!("LaunchAgent loaded state was not restored after pre-publication failure");
    }
    if was_loaded {
        require_started_service_daemon_identity(
            "restored pre-install LaunchAgent",
            previous_executable,
        )?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn launchd_uninstall_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error.context(
            "LaunchAgent uninstall failed; its prior definition and loaded state were restored",
        ),
        Err(rollback_error) => error.context(format!(
            "LaunchAgent uninstall failed and rollback was incomplete: {rollback_error:#}"
        )),
    }
}

#[cfg(target_os = "macos")]
fn install_launchd(executable: &Path, expected_existing_executable: Option<&Path>) -> Result<()> {
    let exe = executable.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let codex_home = effective_codex_home()?.display().to_string();
    let app_home = effective_app_home()?.display().to_string();
    let plist = launchd_plist(&exe, &home, &codex_home, &app_home);

    let path = plist_path()?;
    let previous_executable = expected_existing_executable.unwrap_or(executable);
    let previous = optional_service_file_snapshot(&path)?;
    if let Some(previous) = previous.as_ref() {
        validate_launchd_definition_owner(
            &previous.contents,
            expected_existing_executable.unwrap_or(executable),
        )?;
        user_println(&format!(
            "Warning: overwriting existing LaunchAgent at {}",
            path.display()
        ));
    } else if expected_existing_executable.is_some() {
        anyhow::bail!(
            "an existing LaunchAgent was required for executable migration, but no definition is installed"
        );
    }
    let staged = staged_service_file(&path, plist.as_bytes())?;
    let validation = std::process::Command::new("plutil")
        .args(["-lint", &staged.path().display().to_string()])
        .output()
        .context("validating generated LaunchAgent with plutil")?;
    if !validation.status.success() {
        anyhow::bail!(
            "generated LaunchAgent failed plutil validation: {}",
            service_command_failure_detail(&validation)
        );
    }

    let was_loaded = launchd_is_loaded()?;
    if was_loaded && previous.is_none() {
        anyhow::bail!(
            "LaunchAgent label {LAUNCHD_LABEL} is loaded without a restorable definition at {}; refusing to replace it",
            path.display()
        );
    }
    require_service_snapshot(&path, previous.as_ref())?;
    let preparation = (|| -> Result<()> {
        if was_loaded {
            unload_launchd(&path).context(
                "existing LaunchAgent could not be confirmed unloaded; definition unchanged",
            )?;
            wait_for_daemon_absence_after_service_stop("LaunchAgent replacement")?;
        } else if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking for a detached daemon before LaunchAgent installation")?
        {
            anyhow::bail!(
                "daemon PID {pid} is running outside the unloaded LaunchAgent; stop it before installing the service"
            );
        }
        require_service_snapshot(&path, previous.as_ref())
    })();
    if let Err(preparation_error) = preparation {
        if was_loaded {
            return Err(transaction_error_with_restoration(
                preparation_error,
                restore_launchd_after_prepublication_stop(
                    &path,
                    previous.as_ref(),
                    was_loaded,
                    previous_executable,
                ),
                "LaunchAgent installation preparation failed; its exact prior loaded state was restored",
                "LaunchAgent installation preparation failed and prior-state restoration was incomplete",
            ));
        }
        return Err(preparation_error.context(
            "LaunchAgent installation preparation failed before any service state was changed",
        ));
    }

    let mut publication = match publish_service_file(staged, &path, previous.as_ref()) {
        Ok(publication) => publication,
        Err(err) => {
            if was_loaded
                && let Err(rollback_err) = require_service_snapshot(&path, previous.as_ref())
                    .and_then(|()| load_launchd(&path))
                    .and_then(|()| {
                        wait_for_daemon_presence_after_service_start(
                            "restored LaunchAgent",
                            previous_executable,
                        )
                    })
            {
                return Err(err.context(format!(
                "atomically replacing the LaunchAgent failed and the existing LaunchAgent could not be restarted: {rollback_err}"
            )));
            }
            return Err(err.context("atomically replacing LaunchAgent definition"));
        }
    };

    if let Err(install_err) = load_launchd(&path)
        .and_then(|()| wait_for_daemon_presence_after_service_start("new LaunchAgent", executable))
    {
        if let Err(rollback_err) =
            rollback_launchd_install(&path, &mut publication, was_loaded, previous_executable)
        {
            return Err(install_err.context(format!(
                "new LaunchAgent failed and rollback also failed: {rollback_err}"
            )));
        }
        return Err(install_err.context("new LaunchAgent failed; previous definition was restored"));
    }
    publication
        .commit()
        .context("committing the installed LaunchAgent definition")?;
    user_println(&format!("Installed LaunchAgent at {}", path.display()));
    Ok(())
}

#[cfg(target_os = "macos")]
fn load_launchd(path: &std::path::Path) -> Result<()> {
    let path = path.to_str().ok_or_else(|| {
        anyhow::anyhow!("LaunchAgent path is not valid Unicode: {}", path.display())
    })?;
    launchctl_require_loaded_state(&["load", path], true, "load LaunchAgent")
}

#[cfg(target_os = "macos")]
fn start_launchd(expected_executable: &Path) -> Result<()> {
    let path = plist_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_launchd_service()?;
        anyhow::bail!("LaunchAgent not installed");
    };
    validate_launchd_definition_owner(&contents, expected_executable)?;
    if launchd_is_loaded()? {
        let output = std::process::Command::new("launchctl")
            .args(["start", LAUNCHD_LABEL])
            .output()
            .context("starting loaded LaunchAgent")?;
        if !output.status.success() || !launchd_is_loaded()? {
            anyhow::bail!(
                "launchctl start did not preserve a loaded LaunchAgent: {}",
                service_command_failure_detail(&output)
            );
        }
    } else {
        load_launchd(&path)?;
    }
    wait_for_daemon_presence_after_service_start("LaunchAgent", expected_executable)?;
    user_println("Started LaunchAgent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_launchd(expected_executable: &Path) -> Result<()> {
    stop_launchd_manager(expected_executable)?;
    wait_for_daemon_absence_after_service_stop("launchctl unload")?;
    user_println("Stopped LaunchAgent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_launchd_manager(expected_executable: &Path) -> Result<()> {
    stop_launchd_manager_observed(expected_executable, || {}).map(|_| ())
}

#[cfg(target_os = "macos")]
fn stop_launchd_manager_observed<F>(expected_executable: &Path, request_spawned: F) -> Result<bool>
where
    F: FnOnce(),
{
    let path = plist_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_launchd_service()?;
        user_println("LaunchAgent not installed");
        return Ok(false);
    };
    validate_launchd_definition_owner(&contents, expected_executable)?;
    if launchd_is_loaded()? {
        unload_launchd_observed(&path, request_spawned)?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn uninstall_launchd(expected_executable: &Path) -> Result<()> {
    let path = plist_path()?;
    let Some(previous) = optional_service_file_snapshot(&path)? else {
        require_no_definitionless_launchd_service()?;
        user_println("LaunchAgent not installed");
        return Ok(());
    };
    validate_launchd_definition_owner(&previous.contents, expected_executable)?;
    let was_loaded = launchd_is_loaded()?;
    let mut removal = None;
    let mut applied_cleanup_error = None;
    let uninstall_result = (|| {
        if was_loaded {
            unload_launchd(&path)?;
        }
        wait_for_daemon_absence_after_service_stop("launchctl unload")?;
        removal = Some(begin_service_file_removal(&path, &previous)?);
        let commit = removal
            .as_mut()
            .expect("removal was just initialized")
            .commit()?;
        if let ServiceFileRemovalCommit::AppliedCleanupUnconfirmed(error) = commit {
            applied_cleanup_error = Some(error);
        }
        Ok(())
    })();
    if let Err(error) = uninstall_result {
        return Err(launchd_uninstall_error(
            error,
            rollback_launchd_uninstall(
                &path,
                &previous,
                removal.as_mut(),
                was_loaded,
                expected_executable,
            ),
        ));
    }
    if let Some(error) = applied_cleanup_error {
        return Err(error.context(
            "LaunchAgent uninstall was applied and its prior loaded state was not restored, but cleanup durability could not be confirmed",
        ));
    }
    user_println("Uninstalled LaunchAgent");
    Ok(())
}

// -- Linux systemd --

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(home.join(format!(".config/systemd/user/{SYSTEMD_UNIT_NAME}.service")))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_unit(exe: &str, home: &str, codex_home: &str, app_home: &str) -> String {
    let exe = systemd_exec_quote(exe);
    let home = systemd_quote(&format!("HOME={home}"));
    let codex_home = systemd_quote(&format!("CODEX_HOME={codex_home}"));
    let app_home = systemd_quote(&format!("CODEX_SWITCH_HOME={app_home}"));
    format!(
        r#"[Unit]
Description=codex-switch-global-pace auto-switching daemon
After=network-online.target

[Service]
Type=simple
ExecStart={exe} daemon start --foreground
Restart=on-failure
RestartSec=10
Environment={home}
Environment={codex_home}
Environment={app_home}

[Install]
WantedBy=default.target
"#
    )
}

#[cfg(target_os = "linux")]
fn install_systemd(executable: &Path, expected_existing_executable: Option<&Path>) -> Result<()> {
    let exe = executable.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let codex_home = effective_codex_home()?.display().to_string();
    let app_home = effective_app_home()?.display().to_string();

    let unit = systemd_unit(&exe, &home, &codex_home, &app_home);

    let path = unit_path()?;
    let previous_executable = expected_existing_executable.unwrap_or(executable);
    let previous = optional_service_file_snapshot(&path)?;
    if let Some(previous) = previous.as_ref() {
        validate_systemd_definition_owner(
            &previous.contents,
            expected_existing_executable.unwrap_or(executable),
        )?;
        user_println(&format!(
            "Warning: overwriting existing systemd service at {}",
            path.display()
        ));
    } else if expected_existing_executable.is_some() {
        anyhow::bail!(
            "an existing systemd service was required for executable migration, but no definition is installed"
        );
    }
    let staged = staged_service_file(&path, unit.as_bytes())?;
    let validation = std::process::Command::new("systemd-analyze")
        .args(["--user", "verify"])
        .arg(staged.path())
        .output()
        .context("validating generated systemd user service")?;
    if !validation.status.success() {
        anyhow::bail!(
            "generated systemd user service failed validation: {}",
            service_command_failure_detail(&validation)
        );
    }

    let was_active = systemctl_query("is-active")?;
    let was_enabled = systemctl_query("is-enabled")?;
    if previous.is_none() && (was_active || was_enabled) {
        anyhow::bail!(
            "systemd unit {SYSTEMD_UNIT_NAME} is active or enabled without a restorable definition at {}; refusing to replace it",
            path.display()
        );
    }
    require_service_snapshot(&path, previous.as_ref())?;
    let preparation = (|| -> Result<()> {
        if was_active {
            systemctl_require(
                &["stop", SYSTEMD_UNIT_NAME],
                "stop existing systemd service",
            )?;
            if systemctl_query("is-active")? {
                anyhow::bail!("systemctl stop returned while the existing service remained active");
            }
            wait_for_daemon_absence_after_service_stop("systemd service replacement")?;
        } else if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking for a detached daemon before systemd installation")?
        {
            anyhow::bail!(
                "daemon PID {pid} is running outside the inactive systemd service; stop it before installing the service"
            );
        }
        require_service_snapshot(&path, previous.as_ref())
    })();
    if let Err(preparation_error) = preparation {
        if was_active {
            return Err(transaction_error_with_restoration(
                preparation_error,
                restore_systemd_after_prepublication_stop(
                    &path,
                    previous.as_ref(),
                    was_enabled,
                    was_active,
                    previous_executable,
                ),
                "systemd installation preparation failed; its exact prior active state was restored",
                "systemd installation preparation failed and prior-state restoration was incomplete",
            ));
        }
        return Err(preparation_error.context(
            "systemd installation preparation failed before any service state was changed",
        ));
    }

    let mut publication = match publish_service_file(staged, &path, previous.as_ref()) {
        Ok(publication) => publication,
        Err(err) => {
            if was_active
                && let Err(rollback_err) = require_service_snapshot(&path, previous.as_ref())
                    .and_then(|()| {
                        systemctl_require(&["start", SYSTEMD_UNIT_NAME], "restart existing service")
                    })
                    .and_then(|()| {
                        wait_for_daemon_presence_after_service_start(
                            "restarted existing systemd service",
                            previous_executable,
                        )
                    })
            {
                return Err(err.context(format!(
                "atomically replacing the systemd user service failed and the existing service could not be restarted: {rollback_err}"
            )));
            }
            return Err(err.context("atomically replacing systemd user service"));
        }
    };

    if let Err(install_err) = systemctl_require(&["daemon-reload"], "reload systemd user units")
        .and_then(|()| {
            systemctl_require(
                &["enable", "--now", SYSTEMD_UNIT_NAME],
                "enable new systemd service",
            )
        })
        .and_then(|()| {
            if !systemctl_query("is-enabled")? || !systemctl_query("is-active")? {
                anyhow::bail!("systemctl enable --now returned without an enabled, active service");
            }
            wait_for_daemon_presence_after_service_start("new systemd service", executable)
        })
    {
        if let Err(rollback_err) = rollback_systemd_install(
            &mut publication,
            was_enabled,
            was_active,
            previous_executable,
        ) {
            return Err(install_err.context(format!(
                "new systemd service failed and rollback also failed: {rollback_err}"
            )));
        }
        return Err(
            install_err.context("new systemd service failed; previous definition was restored")
        );
    }
    publication
        .commit()
        .context("committing the installed systemd user service definition")?;
    user_println(&format!(
        "Installed systemd user service at {}",
        path.display()
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl_query(action: &str) -> Result<bool> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", action, SYSTEMD_UNIT_NAME])
        .output()?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        let known_active = match action {
            "is-active" => matches!(state.as_str(), "active" | "reloading"),
            "is-enabled" => state == "enabled",
            _ => false,
        };
        if known_active {
            return Ok(true);
        }
        anyhow::bail!(
            "systemctl --user {action} returned unsupported state '{state}'; refusing to change service state"
        );
    }
    let known_inactive = match action {
        "is-active" => matches!(
            state.as_str(),
            "inactive" | "failed" | "unknown" | "not-found"
        ),
        "is-enabled" => matches!(state.as_str(), "disabled" | "static" | "not-found"),
        _ => false,
    };
    if known_inactive {
        return Ok(false);
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    anyhow::bail!(
        "systemctl --user {action} could not determine service state: {}",
        if detail.is_empty() { state } else { detail }
    )
}

#[cfg(target_os = "linux")]
fn systemd_manager_pid_checked() -> Result<Option<u32>> {
    let output = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            SYSTEMD_UNIT_NAME,
            "--property=MainPID",
            "--value",
        ])
        .output()
        .context("querying the systemd user-service MainPID")?;
    if !output.status.success() {
        anyhow::bail!(
            "systemctl could not determine the systemd user-service MainPID: {}",
            service_command_failure_detail(&output)
        );
    }
    let value = std::str::from_utf8(&output.stdout)
        .context("systemctl MainPID output is not UTF-8")?
        .trim();
    let pid = value
        .parse::<u32>()
        .with_context(|| format!("systemctl reported invalid MainPID '{value}'"))?;
    Ok((pid != 0).then_some(pid))
}

#[cfg(target_os = "linux")]
fn require_no_definitionless_systemd_service() -> Result<()> {
    if systemctl_query("is-active")? || systemctl_query("is-enabled")? {
        anyhow::bail!(
            "systemd unit {SYSTEMD_UNIT_NAME} is active or enabled without a definition that can prove ownership"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl_require(args: &[&str], action: &str) -> Result<()> {
    systemctl_require_observed(args, action, || {})
}

#[cfg(target_os = "linux")]
fn systemctl_require_observed<F>(args: &[&str], action: &str, request_spawned: F) -> Result<()>
where
    F: FnOnce(),
{
    let child = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting systemctl to {action}"))?;
    request_spawned();
    let output = child
        .wait_with_output()
        .with_context(|| format!("waiting for systemctl to {action}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed to {action}: {}",
            service_command_failure_detail(&output)
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_systemd_after_prepublication_stop(
    path: &Path,
    previous: Option<&ServiceFileSnapshot>,
    was_enabled: bool,
    was_active: bool,
    previous_executable: &Path,
) -> Result<()> {
    require_service_snapshot(path, previous)?;
    if systemctl_query("is-enabled")? != was_enabled {
        anyhow::bail!("systemd enablement changed while restoring a failed install preparation");
    }
    let active = systemctl_query("is-active")?;
    if was_active && !active {
        if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon absence before restoring pre-install systemd state")?
        {
            anyhow::bail!(
                "daemon PID {pid} appeared while restoring the pre-install systemd state"
            );
        }
        systemctl_require(
            &["start", SYSTEMD_UNIT_NAME],
            "restore systemd service after pre-publication failure",
        )?;
        wait_for_daemon_presence_after_service_start(
            "restored pre-install systemd service",
            previous_executable,
        )?;
    } else if !was_active && active {
        anyhow::bail!(
            "systemd service became active while restoring an initially inactive install state"
        );
    }
    if systemctl_query("is-enabled")? != was_enabled || systemctl_query("is-active")? != was_active
    {
        anyhow::bail!("systemd state was not restored after pre-publication failure");
    }
    if was_active {
        require_started_service_daemon_identity(
            "restored pre-install systemd service",
            previous_executable,
        )?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rollback_systemd_install(
    publication: &mut ServiceFilePublication,
    was_enabled: bool,
    was_active: bool,
    previous_executable: &Path,
) -> Result<()> {
    if let Err(stop_err) = systemctl_require(
        &["stop", SYSTEMD_UNIT_NAME],
        "stop failed new systemd service",
    ) {
        match systemctl_query("is-active") {
            Ok(false) => {}
            Ok(true) => {
                return Err(stop_err.context(
                    "failed new systemd service is still active; refusing to restore its definition underneath the running process",
                ));
            }
            Err(state_err) => {
                return Err(stop_err.context(format!(
                    "could not verify that the failed new systemd service stopped: {state_err}"
                )));
            }
        }
    }
    if systemctl_query("is-active")? {
        anyhow::bail!("failed new systemd service remained active during rollback");
    }
    wait_for_daemon_absence_after_service_stop("systemd install rollback")?;
    if !was_enabled {
        systemctl_require(
            &["disable", SYSTEMD_UNIT_NAME],
            "remove enablement for failed new systemd service",
        )?;
    }
    publication.rollback()?;
    systemctl_require(&["daemon-reload"], "reload restored systemd user units")?;
    if was_enabled {
        systemctl_require(
            &["enable", SYSTEMD_UNIT_NAME],
            "restore enabled service state",
        )?;
    } else if publication.displaced.is_some() {
        systemctl_require(
            &["disable", SYSTEMD_UNIT_NAME],
            "restore disabled service state",
        )?;
    }
    if was_active {
        if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon absence before systemd install rollback restart")?
        {
            anyhow::bail!(
                "daemon PID {pid} still owns the PID lock; refusing to start a second generation during systemd install rollback"
            );
        }
        systemctl_require(
            &["start", SYSTEMD_UNIT_NAME],
            "restart restored systemd service",
        )?;
        wait_for_daemon_presence_after_service_start(
            "restored systemd service",
            previous_executable,
        )?;
    } else {
        wait_for_daemon_absence_after_service_stop("systemd install rollback")?;
    }
    if systemctl_query("is-enabled")? != was_enabled || systemctl_query("is-active")? != was_active
    {
        anyhow::bail!("restored systemd service state did not match its pre-install state");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rollback_systemd_uninstall(
    path: &Path,
    previous: &ServiceFileSnapshot,
    removal: Option<&mut ServiceFileRemoval>,
    was_enabled: bool,
    was_active: bool,
    expected_executable: &Path,
) -> Result<()> {
    if let Some(removal) = removal {
        removal
            .rollback()
            .with_context(|| format!("restoring systemd definition {}", path.display()))?;
    } else {
        require_service_snapshot(path, Some(previous))?;
    }
    systemctl_require(&["daemon-reload"], "reload restored systemd user units")?;
    if was_enabled {
        systemctl_require(
            &["enable", SYSTEMD_UNIT_NAME],
            "restore enabled systemd service state",
        )?;
    } else {
        systemctl_require(
            &["disable", SYSTEMD_UNIT_NAME],
            "restore disabled systemd service state",
        )?;
    }
    let currently_active = systemctl_query("is-active")?;
    if was_active && !currently_active {
        if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon absence before systemd rollback restart")?
        {
            anyhow::bail!(
                "daemon PID {pid} still owns the PID lock; refusing to start a second generation during systemd rollback"
            );
        }
        systemctl_require(
            &["start", SYSTEMD_UNIT_NAME],
            "restore active systemd service state",
        )?;
        wait_for_daemon_presence_after_service_start(
            "restored systemd service",
            expected_executable,
        )?;
    } else if !was_active && currently_active {
        systemctl_require(
            &["stop", SYSTEMD_UNIT_NAME],
            "restore inactive systemd service state",
        )?;
        wait_for_daemon_absence_after_service_stop("systemd uninstall rollback")?;
    }
    if systemctl_query("is-enabled")? != was_enabled || systemctl_query("is-active")? != was_active
    {
        anyhow::bail!("restored systemd service state did not match its pre-uninstall state");
    }
    if was_active {
        require_started_service_daemon_identity("restored systemd service", expected_executable)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_uninstall_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error.context(
            "systemd service uninstall failed; its prior definition, enabled state, and active state were restored",
        ),
        Err(rollback_error) => error.context(format!(
            "systemd service uninstall failed and rollback was incomplete: {rollback_error:#}"
        )),
    }
}

#[cfg(target_os = "linux")]
fn start_systemd(expected_executable: &Path) -> Result<()> {
    let path = unit_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_systemd_service()?;
        anyhow::bail!("systemd service not installed");
    };
    validate_systemd_definition_owner(&contents, expected_executable)?;
    systemctl_require(&["start", SYSTEMD_UNIT_NAME], "start systemd user service")?;
    if !systemctl_query("is-active")? {
        anyhow::bail!("systemctl start returned without an active systemd user service");
    }
    wait_for_daemon_presence_after_service_start("systemd user service", expected_executable)?;
    user_println("Started systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn stop_systemd(expected_executable: &Path) -> Result<()> {
    stop_systemd_manager(expected_executable)?;
    wait_for_daemon_absence_after_service_stop("systemctl stop")?;
    user_println("Stopped systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn stop_systemd_manager(expected_executable: &Path) -> Result<()> {
    stop_systemd_manager_observed(expected_executable, || {}).map(|_| ())
}

#[cfg(target_os = "linux")]
fn stop_systemd_manager_observed<F>(expected_executable: &Path, request_spawned: F) -> Result<bool>
where
    F: FnOnce(),
{
    let path = unit_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_systemd_service()?;
        anyhow::bail!("systemd service not installed");
    };
    validate_systemd_definition_owner(&contents, expected_executable)?;
    if !systemctl_query("is-active")? {
        return Ok(false);
    }
    systemctl_require_observed(
        &["stop", SYSTEMD_UNIT_NAME],
        "stop systemd user service",
        request_spawned,
    )?;
    if systemctl_query("is-active")? {
        anyhow::bail!("systemctl stop returned while the systemd user service remained active");
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn uninstall_systemd(expected_executable: &Path) -> Result<()> {
    let path = unit_path()?;
    let Some(previous) = optional_service_file_snapshot(&path)? else {
        require_no_definitionless_systemd_service()?;
        user_println("systemd service not installed");
        return Ok(());
    };
    validate_systemd_definition_owner(&previous.contents, expected_executable)?;
    let was_active = systemctl_query("is-active")?;
    let was_enabled = systemctl_query("is-enabled")?;
    let mut removal = None;
    let mut applied_cleanup_error = None;
    let uninstall_result = (|| {
        systemctl_require(
            &["disable", "--now", SYSTEMD_UNIT_NAME],
            "disable and stop systemd user service",
        )?;
        if systemctl_query("is-active")? || systemctl_query("is-enabled")? {
            anyhow::bail!("systemctl disable --now returned without an inactive, disabled service");
        }
        wait_for_daemon_absence_after_service_stop("systemctl disable --now")?;
        removal = Some(begin_service_file_removal(&path, &previous)?);
        systemctl_require(
            &["daemon-reload"],
            "reload systemd user units after uninstall",
        )?;
        let commit = removal
            .as_mut()
            .expect("removal was just initialized")
            .commit()?;
        if let ServiceFileRemovalCommit::AppliedCleanupUnconfirmed(error) = commit {
            applied_cleanup_error = Some(error);
        }
        Ok(())
    })();
    if let Err(error) = uninstall_result {
        return Err(systemd_uninstall_error(
            error,
            rollback_systemd_uninstall(
                &path,
                &previous,
                removal.as_mut(),
                was_enabled,
                was_active,
                expected_executable,
            ),
        ));
    }
    if let Some(error) = applied_cleanup_error {
        return Err(error.context(
            "systemd service uninstall was applied after daemon-reload and remains inactive and disabled, but cleanup durability could not be confirmed",
        ));
    }
    user_println("Uninstalled systemd user service");
    Ok(())
}

// -- Windows Task Scheduler --

#[cfg(any(target_os = "windows", test))]
fn task_scheduler_path<'a>(label: &str, path: &'a Path) -> Result<&'a str> {
    let value = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("{label} is not valid Unicode: {}", path.display()))?;
    if value.contains('%') {
        anyhow::bail!(
            "{label} contains '%', which Windows Task Scheduler cannot preserve safely: {}",
            path.display()
        );
    }
    if value.contains('"') || value.contains('\r') || value.contains('\n') {
        anyhow::bail!(
            "{label} contains an unsupported character: {}",
            path.display()
        );
    }
    Ok(value)
}

#[cfg(any(target_os = "windows", test))]
fn cmd_set_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '^' | '&' | '|' | '<' | '>' | '(' | ')') {
            escaped.push('^');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(any(target_os = "windows", test))]
fn task_scheduler_command(exe: &Path, codex_home: &Path, app_home: &Path) -> Result<String> {
    let exe = task_scheduler_path("executable path", exe)?;
    let codex_home = cmd_set_value(task_scheduler_path("CODEX_HOME", codex_home)?);
    let app_home = cmd_set_value(task_scheduler_path("CODEX_SWITCH_HOME", app_home)?);
    let command = format!(
        "cmd.exe /D /V:OFF /S /C set CODEX_HOME={codex_home}&& set CODEX_SWITCH_HOME={app_home}&& \"{exe}\" daemon start --foreground"
    );
    if command.encode_utf16().count() > 262 {
        anyhow::bail!(
            "Windows Task Scheduler command exceeds its 262-character limit; use shorter executable, CODEX_HOME, or CODEX_SWITCH_HOME paths"
        );
    }
    Ok(command)
}

#[cfg(any(target_os = "windows", test))]
fn decode_xml_text(text: &str) -> Result<String> {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        decoded.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        let end = rest.find(';').ok_or_else(|| {
            anyhow::anyhow!("unterminated XML entity in scheduled-task definition")
        })?;
        let entity = &rest[..end];
        match entity {
            "amp" => decoded.push('&'),
            "lt" => decoded.push('<'),
            "gt" => decoded.push('>'),
            "quot" => decoded.push('"'),
            "apos" => decoded.push('\''),
            value if value.starts_with("#x") => {
                let value = u32::from_str_radix(&value[2..], 16)
                    .context("invalid hexadecimal XML entity")?;
                decoded.push(
                    char::from_u32(value)
                        .ok_or_else(|| anyhow::anyhow!("invalid Unicode XML entity"))?,
                );
            }
            value if value.starts_with('#') => {
                let value = value[1..]
                    .parse::<u32>()
                    .context("invalid decimal XML entity")?;
                decoded.push(
                    char::from_u32(value)
                        .ok_or_else(|| anyhow::anyhow!("invalid Unicode XML entity"))?,
                );
            }
            _ => anyhow::bail!("unsupported XML entity '&{entity};' in scheduled-task definition"),
        }
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    Ok(decoded)
}

#[cfg(any(target_os = "windows", test))]
fn single_xml_element_text(xml: &str, element: &str) -> Result<String> {
    let opening = format!("<{element}>");
    let closing = format!("</{element}>");
    let start = xml
        .find(&opening)
        .ok_or_else(|| anyhow::anyhow!("scheduled-task XML is missing <{element}>"))?
        + opening.len();
    let relative_end = xml[start..]
        .find(&closing)
        .ok_or_else(|| anyhow::anyhow!("scheduled-task XML is missing </{element}>"))?;
    let end = start + relative_end;
    if xml[end + closing.len()..].contains(&opening) {
        anyhow::bail!("scheduled-task XML contains multiple <{element}> elements");
    }
    decode_xml_text(&xml[start..end])
}

#[cfg(any(target_os = "windows", test))]
fn strip_generated_cmd_set_value<'a>(value: &'a str, separator: &str) -> Option<&'a str> {
    let mut index = 0;
    let bytes = value.as_bytes();
    let mut saw_value = false;
    while index < bytes.len() {
        if value[index..].starts_with(separator) {
            return saw_value.then(|| &value[index + separator.len()..]);
        }
        let character = value[index..].chars().next()?;
        if character == '^' {
            let escaped_index = index + character.len_utf8();
            let escaped = value[escaped_index..].chars().next()?;
            if !matches!(escaped, '^' | '&' | '|' | '<' | '>' | '(' | ')') {
                return None;
            }
            index = escaped_index + escaped.len_utf8();
            saw_value = true;
            continue;
        }
        if matches!(
            character,
            '&' | '|' | '<' | '>' | '(' | ')' | '%' | '"' | '\r' | '\n'
        ) {
            return None;
        }
        index += character.len_utf8();
        saw_value = true;
    }
    None
}

#[cfg(any(target_os = "windows", test))]
fn validate_task_scheduler_definition_owner(
    definition: &[u8],
    expected_executable: &Path,
) -> Result<()> {
    let expected = expected_executable.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "expected scheduled-task executable is not valid Unicode: {}",
            expected_executable.display()
        )
    })?;
    let xml = decode_task_definition_xml(definition)?;
    let command = single_xml_element_text(&xml, "Command")?;
    let arguments = single_xml_element_text(&xml, "Arguments")?;
    let executable_suffix = "\" daemon start --foreground";
    let Some(environment) = arguments.strip_prefix("/D /V:OFF /S /C set CODEX_HOME=") else {
        anyhow::bail!(
            "scheduled-task definition is not owned by executable {}",
            expected_executable.display()
        );
    };
    let Some(environment) = strip_generated_cmd_set_value(environment, "&& set CODEX_SWITCH_HOME=")
    else {
        anyhow::bail!(
            "scheduled-task definition is not owned by executable {}",
            expected_executable.display()
        );
    };
    let Some(executable) = strip_generated_cmd_set_value(environment, "&& \"") else {
        anyhow::bail!(
            "scheduled-task definition is not owned by executable {}",
            expected_executable.display()
        );
    };
    let owned = command.eq_ignore_ascii_case("cmd.exe")
        && executable
            .strip_suffix(executable_suffix)
            .is_some_and(|executable| executable == expected);
    if !owned {
        anyhow::bail!(
            "scheduled-task definition is not owned by executable {}",
            expected_executable.display()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn decode_task_definition_xml(definition: &[u8]) -> Result<String> {
    let decode_utf16 = |bytes: &[u8], little_endian: bool| -> Result<String> {
        if !bytes.len().is_multiple_of(2) {
            anyhow::bail!("scheduled-task UTF-16 XML has an odd byte length");
        }
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if little_endian {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect::<Vec<_>>();
        String::from_utf16(&units).context("scheduled-task XML contains invalid UTF-16")
    };

    if let Some(bytes) = definition.strip_prefix(&[0xff, 0xfe]) {
        return decode_utf16(bytes, true);
    }
    if let Some(bytes) = definition.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16(bytes, false);
    }
    let definition = definition
        .strip_prefix(&[0xef, 0xbb, 0xbf])
        .unwrap_or(definition);
    std::str::from_utf8(definition)
        .map(str::to_string)
        .context("scheduled-task XML is neither valid UTF-8 nor BOM-marked UTF-16")
}

#[cfg(target_os = "windows")]
fn schtasks(args: &[&str], action: &str) -> Result<std::process::Output> {
    let output = std::process::Command::new("schtasks").args(args).output()?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    anyhow::bail!(task_scheduler_failure_message(action, &detail));
}

#[cfg(any(target_os = "windows", test))]
fn task_scheduler_failure_message(action: &str, detail: &str) -> String {
    let message = format!("failed to {action}: {detail}");
    if action == "create scheduled task" {
        format!(
            "{message} Re-run `codex-switch-global-pace daemon install` from an elevated PowerShell session."
        )
    } else {
        message
    }
}

#[cfg(target_os = "windows")]
fn restore_after_uncertain_task_publication(
    task_file: &Path,
    replacement_executable: &Path,
    previous_executable: Option<&Path>,
    previous_definition: Option<&[u8]>,
    previous_xml: Option<&[u8]>,
    previous_xml_file: Option<&mut tempfile::NamedTempFile>,
    previous_was_running: bool,
) -> Result<()> {
    let current_definition = optional_task_definition(task_file)?;
    let current_xml = match current_definition.as_ref() {
        Some(_) => Some(query_scheduled_task_xml(
            "classify scheduled task after uncertain publication",
        )?),
        None => None,
    };
    let prior_definition_remains = current_definition.as_deref() == previous_definition
        && current_xml.as_deref() == previous_xml;
    if !prior_definition_remains {
        match (current_definition.as_deref(), current_xml.as_deref()) {
            (Some(definition), Some(xml)) => {
                validate_task_scheduler_definition_owner(definition, replacement_executable)?;
                validate_task_scheduler_definition_owner(xml, replacement_executable)?;
            }
            (None, None) if previous_definition.is_none() && previous_xml.is_none() => {}
            _ => anyhow::bail!(
                "scheduled-task publication post-state was neither the exact prior definition nor an exactly owned replacement"
            ),
        }
    }
    restore_scheduled_task(
        task_file,
        current_definition.as_deref(),
        current_xml.as_deref(),
        previous_definition,
        previous_xml_file,
        previous_was_running,
        previous_executable,
    )
}

#[cfg(target_os = "windows")]
fn install_task_scheduler(expected_existing_executable: Option<&Path>) -> Result<()> {
    use std::io::Write;

    let exe = std::env::current_exe().context("locating daemon executable")?;
    let codex_home = effective_codex_home()?;
    let app_home = effective_app_home()?;
    let task_run = task_scheduler_command(&exe, &codex_home, &app_home)?;

    let task_file = windows_task_file_path()?;
    let previous_definition = optional_task_definition(&task_file)?;
    let previous_xml = if let Some(previous_definition) = previous_definition.as_deref() {
        let xml = query_scheduled_task_xml("export existing scheduled task")?;
        validate_task_scheduler_definition_owner(
            previous_definition,
            expected_existing_executable.unwrap_or(&exe),
        )?;
        validate_task_scheduler_definition_owner(
            &xml,
            expected_existing_executable.unwrap_or(&exe),
        )?;
        Some(xml)
    } else if expected_existing_executable.is_some() {
        anyhow::bail!(
            "an existing scheduled task was required for executable migration, but no definition is installed"
        );
    } else {
        None
    };
    let previous_was_running = crate::daemon::pidfile::running_pid_checked()
        .context("checking the existing daemon PID lock before scheduled-task installation")?
        .is_some();
    let previous_executable = previous_definition
        .as_ref()
        .map(|_| expected_existing_executable.unwrap_or(exe.as_path()));
    if previous_definition.is_none() && previous_was_running {
        anyhow::bail!(
            "a detached daemon is running without an installed scheduled task; stop it before installing the service"
        );
    }

    // Task Scheduler has no validate-only API. Create and remove a uniquely
    // named task first so syntax, permissions and the complete /TR payload are
    // accepted before `/F` is allowed to replace the live definition.
    let stage_name = format!(
        r"\codex-switch-global-pace-daemon-install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    create_scheduled_task(&stage_name, &task_run, false)?;
    if let Err(err) = schtasks(&["/Delete", "/TN", &stage_name, "/F"], "remove staged task") {
        return Err(err.context(format!(
            "staged task {stage_name} could not be cleaned up; live task was left unchanged"
        )));
    }

    let mut previous_xml_file = if let Some(xml) = previous_xml.as_deref() {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(xml)?;
        file.as_file().sync_all()?;
        Some(file)
    } else {
        None
    };

    let preparation = (|| -> Result<()> {
        if previous_definition.is_some() {
            stop_scheduled_daemon_for_rollback()
                .context("stopping the existing daemon after staged-task validation")?;
        }
        require_task_definition_snapshot(&task_file, previous_definition.as_deref())?;
        require_task_xml_snapshot(&task_file, previous_xml.as_deref())
    })();
    if let Err(preparation_error) = preparation {
        if previous_definition.is_some() {
            return Err(transaction_error_with_restoration(
                preparation_error,
                restore_scheduled_task(
                    &task_file,
                    previous_definition.as_deref(),
                    previous_xml.as_deref(),
                    previous_definition.as_deref(),
                    previous_xml_file.as_mut(),
                    previous_was_running,
                    previous_executable,
                ),
                "scheduled-task installation preparation failed; its exact prior definition and running state were restored",
                "scheduled-task installation preparation failed and prior-state restoration was incomplete",
            ));
        }
        return Err(preparation_error.context(
            "scheduled-task installation preparation failed before any live task state was changed",
        ));
    }

    if let Err(install_err) =
        create_scheduled_task(WINDOWS_TASK_NAME, &task_run, previous_definition.is_some())
    {
        let rollback_result = restore_after_uncertain_task_publication(
            &task_file,
            &exe,
            previous_executable,
            previous_definition.as_deref(),
            previous_xml.as_deref(),
            previous_xml_file.as_mut(),
            previous_was_running,
        );
        if let Err(rollback_err) = rollback_result {
            return Err(install_err.context(format!(
                "new scheduled task failed and rollback also failed: {rollback_err}"
            )));
        }
        return Err(
            install_err.context("new scheduled task failed; previous definition was restored")
        );
    }

    let published_snapshot = (|| -> Result<(Vec<u8>, Vec<u8>)> {
        let definition = optional_task_definition(&task_file)?.ok_or_else(|| {
            anyhow::anyhow!(
                "scheduled-task creation returned success, but definition {} is missing",
                task_file.display()
            )
        })?;
        let xml = query_scheduled_task_xml("export newly installed scheduled task")?;
        Ok((definition, xml))
    })();
    let (published_definition, published_xml) = match published_snapshot {
        Ok(snapshot) => snapshot,
        Err(snapshot_error) => {
            return Err(transaction_error_with_restoration(
                snapshot_error,
                restore_after_uncertain_task_publication(
                    &task_file,
                    &exe,
                    previous_executable,
                    previous_definition.as_deref(),
                    previous_xml.as_deref(),
                    previous_xml_file.as_mut(),
                    previous_was_running,
                ),
                "scheduled-task publication could not be inspected; the exact prior definition and running state were restored",
                "scheduled-task publication could not be inspected and exact prior-state restoration was incomplete",
            ));
        }
    };
    let install_result = validate_task_scheduler_definition_owner(&published_definition, &exe)
        .and_then(|()| validate_task_scheduler_definition_owner(&published_xml, &exe))
        .and_then(|()| require_task_definition_snapshot(&task_file, Some(&published_definition)))
        .and_then(|()| require_task_xml_snapshot(&task_file, Some(&published_xml)))
        .and_then(|()| {
            schtasks(&["/Run", "/TN", WINDOWS_TASK_NAME], "start scheduled task")?;
            Ok(())
        })
        .and_then(|_| wait_for_scheduled_daemon(&exe))
        .and_then(|()| require_task_definition_snapshot(&task_file, Some(&published_definition)))
        .and_then(|()| require_task_xml_snapshot(&task_file, Some(&published_xml)));
    if let Err(install_err) = install_result {
        let rollback_result = restore_scheduled_task(
            &task_file,
            Some(&published_definition),
            Some(&published_xml),
            previous_definition.as_deref(),
            previous_xml_file.as_mut(),
            previous_was_running,
            previous_executable,
        );
        if let Err(rollback_err) = rollback_result {
            return Err(install_err.context(format!(
                "new scheduled task failed and rollback also failed: {rollback_err}"
            )));
        }
        return Err(
            install_err.context("new scheduled task failed; previous definition was restored")
        );
    }
    user_println(&format!(
        "Installed Windows scheduled task {WINDOWS_TASK_NAME}"
    ));
    Ok(())
}

#[cfg(target_os = "windows")]
fn create_scheduled_task(name: &str, task_run: &str, replace_existing: bool) -> Result<()> {
    let mut args = vec![
        "/Create", "/TN", name, "/TR", task_run, "/SC", "ONLOGON", "/RL", "LIMITED", "/IT",
    ];
    if replace_existing {
        args.push("/F");
    }
    schtasks(&args, "create scheduled task")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn wait_for_scheduled_daemon(expected_executable: &Path) -> Result<()> {
    wait_for_daemon_presence_after_service_start("scheduled daemon", expected_executable)
}

#[cfg(target_os = "windows")]
fn restore_scheduled_task(
    task_file: &Path,
    expected_current: Option<&[u8]>,
    expected_current_xml: Option<&[u8]>,
    previous_definition: Option<&[u8]>,
    previous_xml: Option<&mut tempfile::NamedTempFile>,
    previous_was_running: bool,
    previous_executable: Option<&Path>,
) -> Result<()> {
    require_task_definition_snapshot(task_file, expected_current)?;
    require_task_xml_snapshot(task_file, expected_current_xml)?;
    if expected_current != previous_definition {
        if expected_current.is_some() {
            stop_scheduled_daemon_for_rollback()?;
        } else {
            wait_for_daemon_absence_after_service_stop(
                "missing scheduled-task publication rollback",
            )?;
        }
        require_task_definition_snapshot(task_file, expected_current)?;
        require_task_xml_snapshot(task_file, expected_current_xml)?;
        if let Some(previous_xml) = previous_xml.as_deref() {
            let xml_path = previous_xml.path().to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduled-task rollback XML path is not valid Unicode: {}",
                    previous_xml.path().display()
                )
            })?;
            schtasks(
                &["/Create", "/TN", WINDOWS_TASK_NAME, "/XML", xml_path, "/F"],
                "restore previous scheduled task definition",
            )?;
        } else {
            schtasks(
                &["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"],
                "remove failed new scheduled task",
            )?;
        }
    }

    match (previous_definition, previous_xml.as_deref()) {
        (Some(_), Some(previous_xml)) => {
            let expected_xml = std::fs::read(previous_xml.path())
                .context("reading scheduled-task rollback XML snapshot")?;
            let restored_xml = schtasks(
                &["/Query", "/TN", WINDOWS_TASK_NAME, "/XML"],
                "verify restored scheduled task definition",
            )?
            .stdout;
            if restored_xml != expected_xml {
                anyhow::bail!(
                    "restored scheduled-task definition did not match its exact pre-install XML"
                );
            }
        }
        (None, None) => {
            require_task_definition_snapshot(task_file, None)?;
        }
        _ => anyhow::bail!("scheduled-task rollback snapshots were internally inconsistent"),
    }

    if previous_was_running {
        if crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon absence before scheduled-task install rollback restart")?
            .is_none()
        {
            schtasks(
                &["/Run", "/TN", WINDOWS_TASK_NAME],
                "restart previous scheduled task",
            )?;
            wait_for_scheduled_daemon(previous_executable.context(
                "a running scheduled-task rollback requires its exact prior executable",
            )?)?;
        }
    } else {
        if previous_definition.is_some() {
            schtasks(
                &["/End", "/TN", WINDOWS_TASK_NAME],
                "restore stopped scheduled-task state",
            )?;
        }
        wait_for_daemon_absence_after_service_stop("scheduled-task install rollback")?;
    }
    if previous_was_running {
        require_started_service_daemon_identity(
            "restored scheduled-task daemon after install rollback",
            previous_executable
                .context("a running scheduled-task rollback requires its exact prior executable")?,
        )?;
    } else if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
        .context("verifying scheduled-task running state after install rollback")?
    {
        anyhow::bail!(
            "daemon PID {pid} appeared while restoring the stopped scheduled-task install state"
        );
    }
    if previous_definition.is_some() {
        let current = optional_task_definition(task_file)?.ok_or_else(|| {
            anyhow::anyhow!(
                "restored scheduled-task definition {} is missing",
                task_file.display()
            )
        })?;
        if expected_current == previous_definition
            && Some(current.as_slice()) != previous_definition
        {
            anyhow::bail!(
                "scheduled-task definition changed while restoring its pre-install state"
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn rollback_task_scheduler_uninstall(
    task_file: &Path,
    previous_definition: &[u8],
    previous_xml: &mut tempfile::NamedTempFile,
    was_running: bool,
    expected_executable: &Path,
) -> Result<()> {
    match checked_regular_file(task_file, "Windows scheduled-task definition")? {
        true => {
            let current = std::fs::read(task_file).with_context(|| {
                format!(
                    "reading scheduled-task definition during rollback {}",
                    task_file.display()
                )
            })?;
            if current != previous_definition {
                anyhow::bail!(
                    "scheduled-task definition {} changed during uninstall; refusing to overwrite it during rollback",
                    task_file.display()
                );
            }
            schtasks(
                &["/Query", "/TN", WINDOWS_TASK_NAME],
                "verify scheduled task during uninstall rollback",
            )?;
        }
        false => {
            let xml_path = previous_xml
                .path()
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "scheduled-task uninstall rollback XML path is not valid Unicode: {}",
                        previous_xml.path().display()
                    )
                })?
                .to_string();
            schtasks(
                &["/Create", "/TN", WINDOWS_TASK_NAME, "/XML", &xml_path, "/F"],
                "restore scheduled task after failed uninstall",
            )?;
        }
    }
    let expected_xml = std::fs::read(previous_xml.path())
        .context("reading scheduled-task uninstall rollback XML snapshot")?;
    let restored_xml = schtasks(
        &["/Query", "/TN", WINDOWS_TASK_NAME, "/XML"],
        "verify restored scheduled task after failed uninstall",
    )?
    .stdout;
    if restored_xml != expected_xml {
        anyhow::bail!(
            "restored scheduled-task definition did not match its exact pre-uninstall XML"
        );
    }
    if was_running {
        if crate::daemon::pidfile::running_pid_checked()
            .context("checking daemon state before scheduled-task rollback restart")?
            .is_none()
        {
            schtasks(
                &["/Run", "/TN", WINDOWS_TASK_NAME],
                "restore running scheduled-task state",
            )?;
            wait_for_scheduled_daemon(expected_executable)?;
        }
    } else {
        schtasks(
            &["/End", "/TN", WINDOWS_TASK_NAME],
            "restore stopped scheduled-task state after failed uninstall",
        )?;
        wait_for_daemon_absence_after_service_stop("scheduled-task uninstall rollback")?;
    }
    if was_running {
        require_started_service_daemon_identity(
            "restored scheduled-task daemon after uninstall rollback",
            expected_executable,
        )?;
    } else if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
        .context("verifying scheduled-task running state after uninstall rollback")?
    {
        anyhow::bail!(
            "daemon PID {pid} appeared while restoring the stopped scheduled-task uninstall state"
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn task_scheduler_uninstall_error(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error.context(
            "Windows scheduled-task uninstall failed; its prior definition and running state were restored",
        ),
        Err(rollback_error) => error.context(format!(
            "Windows scheduled-task uninstall failed and rollback was incomplete: {rollback_error:#}"
        )),
    }
}

#[cfg(target_os = "windows")]
fn stop_scheduled_daemon_for_rollback() -> Result<()> {
    let Some(target) = crate::daemon::pidfile::running_generation_checked()
        .context("checking the scheduled daemon generation before rollback cleanup")?
    else {
        return finalize_scheduled_daemon_rollback_stop();
    };
    let pid = target.pid();
    let started = std::time::Instant::now();
    let mut next_report = super::DAEMON_TRANSITION_TIMEOUT;
    stop_exact_scheduled_daemon_generation_with(
        pid,
        || {
            crate::daemon::pidfile::request_shutdown(&target)
                .context("requesting graceful shutdown of the scheduled daemon")
        },
        |request| {
            request
                .target_is_running()
                .context("checking the exact scheduled daemon shutdown target")
        },
        finalize_scheduled_daemon_rollback_stop,
        |request| request.require_durable(),
        || {
            let elapsed = started.elapsed();
            if elapsed >= next_report {
                eprintln!(
                    "Scheduled daemon PID {pid} is still settling {:.0}s after its already-issued rollback stop request; lifecycle authority remains held and no additional request will be published",
                    elapsed.as_secs_f64()
                );
                next_report = elapsed.saturating_add(super::DAEMON_TRANSITION_TIMEOUT);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        },
    )
}

#[cfg(target_os = "windows")]
fn finalize_scheduled_daemon_rollback_stop() -> Result<()> {
    // With no live generation-bound daemon, `/End` clears Task Scheduler's
    // bookkeeping for a failed or already-exited instance. Its success is
    // required: an absent PID file alone cannot prove a queued task will not
    // start after the executable is rolled back.
    schtasks(
        &["/End", "/TN", WINDOWS_TASK_NAME],
        "end inactive failed task",
    )?;
    crate::daemon::pidfile::cleanup_pidfile()
}

#[cfg(any(target_os = "windows", test))]
fn stop_exact_scheduled_daemon_generation_with<
    Request,
    RequestOutcome,
    Observe,
    Finalize,
    Durability,
    Wait,
>(
    pid: u32,
    request: Request,
    mut target_is_running: Observe,
    finalize: Finalize,
    require_durable: Durability,
    mut wait: Wait,
) -> Result<()>
where
    Request: FnOnce() -> Result<RequestOutcome>,
    Observe: FnMut(&RequestOutcome) -> Result<bool>,
    Finalize: FnOnce() -> Result<()>,
    Durability: FnOnce(RequestOutcome) -> Result<()>,
    Wait: FnMut(),
{
    let request = request()?;
    let mut diagnostics = super::TransientDiagnostics::default();
    loop {
        match target_is_running(&request) {
            Ok(false) => break,
            Ok(true) => {}
            Err(error) => {
                let diagnostic = format!(
                    "transiently failed to inspect already-requested scheduled daemon PID {pid}: {error:#}"
                );
                eprintln!("{diagnostic}; lifecycle authority remains held");
                diagnostics.record(diagnostic);
            }
        }
        wait();
    }

    // Both classifications happen only after the exact requested generation
    // has settled. Finalization is attempted exactly once even when the
    // already-published request later reports uncertain durability.
    let finalization = finalize();
    let request_durability = require_durable(request);
    classify_scheduled_daemon_stop_completion(finalization, request_durability, diagnostics)
}

#[cfg(any(target_os = "windows", test))]
fn classify_scheduled_daemon_stop_completion(
    finalization: Result<()>,
    request_durability: Result<()>,
    diagnostics: super::TransientDiagnostics,
) -> Result<()> {
    let transient_diagnostics = diagnostics.into_summary("transient authority-probe failure");
    match (finalization, request_durability, transient_diagnostics) {
        (Ok(()), Ok(()), None) => Ok(()),
        (Ok(()), Ok(()), Some(diagnostics)) => anyhow::bail!(
            "scheduled daemon stopped and Task Scheduler was finalized after transient authority-probe failures: {diagnostics}"
        ),
        (Ok(()), Err(request_error), None) => Err(request_error.context(
            "scheduled daemon stopped and Task Scheduler was finalized, but shutdown-request durability was unconfirmed",
        )),
        (Ok(()), Err(request_error), Some(diagnostics)) => Err(request_error.context(format!(
            "scheduled daemon stopped and Task Scheduler was finalized after transient authority-probe failures ({diagnostics}), but shutdown-request durability was unconfirmed"
        ))),
        (Err(finalization_error), Ok(()), None) => Err(finalization_error),
        (Err(finalization_error), Ok(()), Some(diagnostics)) => Err(finalization_error.context(
            format!("scheduled-daemon authority probes also failed transiently: {diagnostics}"),
        )),
        (Err(finalization_error), Err(request_error), None) => Err(finalization_error.context(
            format!("shutdown-request durability was also unconfirmed: {request_error:#}"),
        )),
        (Err(finalization_error), Err(request_error), Some(diagnostics)) => {
            Err(finalization_error.context(format!(
                "shutdown-request durability was also unconfirmed ({request_error:#}); scheduled-daemon authority probes also failed transiently: {diagnostics}"
            )))
        }
    }
}

#[cfg(target_os = "windows")]
fn uninstall_task_scheduler(
    expected_executable: &Path,
    previous_daemon_running: bool,
) -> Result<()> {
    let task_file = windows_task_file_path()?;
    let Some(definition) = optional_task_definition(&task_file)? else {
        if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
            .context("checking the daemon PID lock without an installed scheduled task")?
        {
            anyhow::bail!(
                "Windows scheduled task is not installed, but daemon PID {pid} still owns the PID lock"
            );
        }
        user_println("Windows scheduled task not installed");
        return Ok(());
    };
    validate_task_scheduler_definition_owner(&definition, expected_executable)?;
    let previous_xml = query_scheduled_task_xml("export scheduled task before uninstall")?;
    validate_task_scheduler_definition_owner(&previous_xml, expected_executable)?;
    let mut previous_xml_file = tempfile::NamedTempFile::new()
        .context("creating scheduled-task uninstall rollback snapshot")?;
    {
        use std::io::Write;
        previous_xml_file
            .write_all(&previous_xml)
            .context("writing scheduled-task uninstall rollback snapshot")?;
        previous_xml_file
            .as_file()
            .sync_all()
            .context("syncing scheduled-task uninstall rollback snapshot")?;
    }

    let uninstall_result = (|| {
        schtasks(&["/End", "/TN", WINDOWS_TASK_NAME], "stop scheduled task")?;
        wait_for_daemon_absence_after_service_stop("stopping the Windows scheduled task")?;
        let current = std::fs::read(&task_file).with_context(|| {
            format!(
                "re-reading scheduled-task definition {}",
                task_file.display()
            )
        })?;
        if current != definition {
            anyhow::bail!(
                "scheduled-task definition {} changed during uninstall; refusing to delete it",
                task_file.display()
            );
        }
        require_task_xml_snapshot(&task_file, Some(&previous_xml))?;
        schtasks(
            &["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"],
            "delete scheduled task",
        )?;
        if checked_regular_file(&task_file, "Windows scheduled-task definition")? {
            anyhow::bail!(
                "scheduled-task deletion returned success, but definition {} still exists",
                task_file.display()
            );
        }
        require_task_xml_snapshot(&task_file, None)?;
        Ok(())
    })();
    if let Err(error) = uninstall_result {
        return Err(task_scheduler_uninstall_error(
            error,
            rollback_task_scheduler_uninstall(
                &task_file,
                &definition,
                &mut previous_xml_file,
                previous_daemon_running,
                expected_executable,
            ),
        ));
    }
    user_println("Uninstalled Windows scheduled task");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use super::acquire_service_operation_lease;
    #[cfg(not(target_os = "windows"))]
    use super::acquire_service_operation_lease_at;
    use super::{
        ExactServiceUninstallOutcome, LAUNCHD_LABEL, SYSTEMD_UNIT_NAME, WINDOWS_TASK_NAME,
        absolute_service_path, capture_launchd_runtime_for_snapshot_with,
        classify_exact_uninstall_transition, definition_snapshot_matches, launchd_job_pid,
        launchd_list_contains_label, launchd_plist, parse_task_scheduler_manager_proof_output,
        require_manager_runtime_consistency, stop_exact_scheduled_daemon_generation_with,
        systemd_exec_quote, systemd_quote, systemd_unit, task_listing_contains_name,
        task_scheduler_command, task_scheduler_failure_message, transaction_error_with_restoration,
        uninstall_locked_exact_with, validate_expected_executable,
        validate_install_migration_authority, validate_launchd_definition_value,
        validate_started_service_daemon_identity, validate_systemd_definition_owner,
        validate_task_scheduler_definition_owner,
    };
    #[cfg(unix)]
    use super::{
        ServiceFileCleanupOutcome, ServiceFileRemovalCommit, ServiceStateSnapshot,
        begin_service_file_removal, optional_service_file_snapshot, publish_service_file,
        service_transaction_path, staged_service_file,
    };
    #[cfg(windows)]
    use super::{ServiceStateSnapshot, WindowsTaskSnapshot};
    use std::path::{Path, PathBuf};

    #[test]
    fn exact_uninstall_transition_preserves_applied_prior_and_ambiguous_states() {
        let ExactServiceUninstallOutcome::Applied { operation_error } =
            classify_exact_uninstall_transition(
                Err(anyhow::anyhow!("injected lost uninstall response")),
                true,
                false,
            )
            .unwrap()
        else {
            panic!("exact absence must classify an applied uninstall");
        };
        assert!(
            operation_error
                .unwrap()
                .to_string()
                .contains("lost uninstall response")
        );

        let ExactServiceUninstallOutcome::PriorExact { operation_error } =
            classify_exact_uninstall_transition(
                Err(anyhow::anyhow!("injected preserved prior state")),
                false,
                true,
            )
            .unwrap()
        else {
            panic!("the exact prior snapshot must remain distinguishable");
        };
        assert!(
            operation_error
                .to_string()
                .contains("preserved prior state")
        );

        assert!(classify_exact_uninstall_transition(Ok(()), false, true).is_err());
        assert!(
            classify_exact_uninstall_transition(
                Err(anyhow::anyhow!("injected ambiguous state")),
                false,
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn successful_exact_uninstall_with_failed_post_state_capture_remains_applied() {
        use std::cell::RefCell;

        let calls = RefCell::new(Vec::new());
        let outcome = uninstall_locked_exact_with(
            || {
                calls.borrow_mut().push("uninstall");
                Ok(())
            },
            || {
                assert_eq!(calls.borrow().as_slice(), ["uninstall"]);
                calls.borrow_mut().push("capture");
                Err(anyhow::anyhow!("injected post-state capture failure"))
            },
        )
        .unwrap();
        let mut caller_state = super::super::InstallerUninstallState::NotApplied;
        let post_state_error = match outcome {
            ExactServiceUninstallOutcome::AppliedPendingVerification { post_state_error } => {
                caller_state.mark_applied();
                post_state_error
            }
            _ => panic!("a successful destructive uninstall must remain typed as applied"),
        };
        let message = format!("{post_state_error:#}");
        assert!(message.contains("returned success"));
        assert!(message.contains("injected post-state capture failure"));
        assert_eq!(
            caller_state,
            super::super::InstallerUninstallState::AppliedPendingVerification
        );
        assert_eq!(calls.into_inner(), ["uninstall", "capture"]);

        let failed_operation = uninstall_locked_exact_with(
            || Err(anyhow::anyhow!("injected uninstall failure")),
            || Err(anyhow::anyhow!("injected post-state capture failure")),
        )
        .unwrap_err();
        let message = format!("{failed_operation:#}");
        assert!(message.contains("injected uninstall failure"));
        assert!(message.contains("injected post-state capture failure"));
    }

    #[test]
    fn service_manager_runtime_state_requires_exact_pid_consistency() {
        assert!(
            require_manager_runtime_consistency("launchd", "loaded", true, "PID", None)
                .unwrap_err()
                .to_string()
                .contains("loaded=true")
        );
        assert!(
            require_manager_runtime_consistency("launchd", "loaded", false, "PID", Some(42))
                .unwrap_err()
                .to_string()
                .contains("PID=Some(42)")
        );
        require_manager_runtime_consistency("launchd", "loaded", true, "PID", Some(42)).unwrap();
        require_manager_runtime_consistency("launchd", "loaded", false, "PID", None).unwrap();
    }

    #[test]
    fn launchd_snapshot_capture_rejects_loaded_job_without_manager_pid() {
        use std::cell::Cell;

        let loaded_probes = Cell::new(0_u32);
        let pid_probes = Cell::new(0_u32);
        let snapshot_returned = Cell::new(false);
        let result = capture_launchd_runtime_for_snapshot_with(
            || {
                loaded_probes.set(loaded_probes.get() + 1);
                Ok(true)
            },
            |loaded| {
                assert!(loaded);
                pid_probes.set(pid_probes.get() + 1);
                Ok(None)
            },
        )
        .inspect(|_| {
            snapshot_returned.set(true);
        });

        let error = result.expect_err("loaded=true with no PID must not produce a snapshot");
        assert!(format!("{error:#}").contains("loaded=true"));
        assert_eq!(loaded_probes.get(), 1);
        assert_eq!(pid_probes.get(), 1);
        assert!(!snapshot_returned.get());
    }

    #[test]
    fn scheduled_rollback_stop_requests_and_finalizes_once_after_exact_settlement() {
        use std::cell::Cell;
        use std::collections::VecDeque;

        let request_count = Cell::new(0_u32);
        let finalization_count = Cell::new(0_u32);
        let durability_count = Cell::new(0_u32);
        let elapsed = Cell::new(std::time::Duration::ZERO);
        let wait_count = Cell::new(0_u32);
        let mut observations = VecDeque::from([
            Ok(true),
            Err(anyhow::anyhow!(
                "injected first transient generation probe failure"
            )),
            Ok(true),
            Err(anyhow::anyhow!(
                "injected middle transient generation probe failure"
            )),
            Ok(true),
            Err(anyhow::anyhow!(
                "injected last transient generation probe failure"
            )),
            Ok(true),
            Ok(false),
        ]);

        let result = stop_exact_scheduled_daemon_generation_with(
            4242,
            || {
                request_count.set(request_count.get() + 1);
                Ok(())
            },
            |_| observations.pop_front().expect("one injected observation"),
            || {
                finalization_count.set(finalization_count.get() + 1);
                Ok(())
            },
            |_| {
                durability_count.set(durability_count.get() + 1);
                Ok(())
            },
            || {
                wait_count.set(wait_count.get() + 1);
                elapsed.set(
                    elapsed.get()
                        + std::time::Duration::from_millis(50)
                        + super::super::DAEMON_TRANSITION_TIMEOUT,
                );
            },
        );

        let error = result.expect_err(
            "transient exact-generation probe failures must be reported after safe finalization",
        );
        let message = format!("{error:#}");
        assert!(message.contains("3 transient authority-probe failures"));
        assert!(message.contains("injected first transient generation probe failure"));
        assert!(message.contains("injected last transient generation probe failure"));
        assert!(!message.contains("injected middle transient generation probe failure"));
        assert_eq!(request_count.get(), 1);
        assert_eq!(finalization_count.get(), 1);
        assert_eq!(durability_count.get(), 1);
        assert!(elapsed.get() > super::super::DAEMON_TRANSITION_TIMEOUT * 2);
        assert_eq!(wait_count.get(), 7);
        assert!(observations.is_empty());
    }

    #[test]
    fn exact_service_snapshot_compares_definition_bytes_and_runtime_not_recreated_file_ids() {
        let temp = tempfile::tempdir().unwrap();
        let first_path = temp.path().join("first-definition");
        let second_path = temp.path().join("second-definition");
        std::fs::write(&first_path, b"exact owned definition").unwrap();
        std::fs::write(&second_path, b"exact owned definition").unwrap();
        let first_token = crate::fs_ops::token_for_path(&first_path).unwrap();
        let second_token = crate::fs_ops::token_for_path(&second_path).unwrap();
        assert_ne!(first_token, second_token);

        #[cfg(unix)]
        let first = ServiceStateSnapshot {
            definition: Some(super::ServiceFileSnapshot {
                contents: b"exact owned definition".to_vec(),
                token: first_token,
            }),
            #[cfg(target_os = "macos")]
            loaded: false,
            #[cfg(target_os = "linux")]
            enabled: true,
            #[cfg(target_os = "linux")]
            active: false,
            manager_pid: None,
        };
        #[cfg(unix)]
        let mut recreated = ServiceStateSnapshot {
            definition: Some(super::ServiceFileSnapshot {
                contents: b"exact owned definition".to_vec(),
                token: second_token,
            }),
            #[cfg(target_os = "macos")]
            loaded: false,
            #[cfg(target_os = "linux")]
            enabled: true,
            #[cfg(target_os = "linux")]
            active: false,
            manager_pid: None,
        };

        #[cfg(windows)]
        let first = ServiceStateSnapshot {
            definition: Some(WindowsTaskSnapshot {
                contents: b"exact owned definition".to_vec(),
                token: first_token,
                scheduler_xml: b"<Task>exact</Task>".to_vec(),
            }),
            manager_pid: None,
        };
        #[cfg(windows)]
        let mut recreated = ServiceStateSnapshot {
            definition: Some(WindowsTaskSnapshot {
                contents: b"exact owned definition".to_vec(),
                token: second_token,
                scheduler_xml: b"<Task>exact</Task>".to_vec(),
            }),
            manager_pid: None,
        };

        assert!(recreated.matches_snapshot_with_manager(&first, None));
        #[cfg(unix)]
        {
            recreated.definition.as_mut().unwrap().contents =
                b"changed same-owner definition".to_vec();
        }
        #[cfg(windows)]
        {
            recreated.definition.as_mut().unwrap().scheduler_xml =
                b"<Task>changed same-owner runtime</Task>".to_vec();
        }
        assert!(!recreated.matches_snapshot_with_manager(&first, None));
    }

    #[test]
    fn service_manager_pid_parsers_require_one_exact_generation() {
        assert_eq!(
            parse_task_scheduler_manager_proof_output(b"none\r\n", None).unwrap(),
            None
        );
        assert_eq!(
            launchd_job_pid(b"{\n    \"PID\" = 4242;\n}\n").unwrap(),
            Some(4242)
        );
        assert_eq!(
            launchd_job_pid(b"{\n    \"LastExitStatus\" = 0;\n}\n").unwrap(),
            None
        );
        assert!(launchd_job_pid(b"PID = 42;\nPID = 43;\n").is_err());
    }

    fn task_scheduler_proof_fixture(
        start_instance: (&str, u32),
        start_processes: &[(u32, u32, u64)],
        end_processes: &[(u32, u32, u64)],
        end_instance: (&str, u32),
    ) -> Vec<u8> {
        let mut rows = vec![format!(
            "instance-start\t{}\t{}",
            start_instance.0, start_instance.1
        )];
        rows.extend(
            start_processes
                .iter()
                .map(|(pid, parent_pid, creation_ticks)| {
                    format!("process-start\t{pid}\t{parent_pid}\t{creation_ticks}")
                }),
        );
        rows.extend(
            end_processes
                .iter()
                .map(|(pid, parent_pid, creation_ticks)| {
                    format!("process-end\t{pid}\t{parent_pid}\t{creation_ticks}")
                }),
        );
        rows.push(format!(
            "instance-end\t{}\t{}",
            end_instance.0, end_instance.1
        ));
        rows.join("\r\n").into_bytes()
    }

    #[test]
    fn task_scheduler_manager_proof_requires_a_stable_instance_and_process_generations() {
        const INSTANCE: &str = "11111111-2222-3333-4444-555555555555";
        let generations = [(200, 250, 3_000), (250, 300, 2_000), (300, 4, 1_000)];
        let proof = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &generations,
            &generations,
            (INSTANCE, 300),
        );
        assert_eq!(
            parse_task_scheduler_manager_proof_output(&proof, Some(200)).unwrap(),
            Some(200)
        );
        assert!(parse_task_scheduler_manager_proof_output(&proof, None).is_err());

        let changed_instance = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &generations,
            &generations,
            ("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", 300),
        );
        assert!(parse_task_scheduler_manager_proof_output(&changed_instance, Some(200)).is_err());
        let changed_engine = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &generations,
            &generations,
            (INSTANCE, 301),
        );
        assert!(parse_task_scheduler_manager_proof_output(&changed_engine, Some(200)).is_err());

        let reused_engine_pid = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &generations,
            &[(200, 250, 3_000), (250, 300, 2_000), (300, 4, 9_000)],
            (INSTANCE, 300),
        );
        assert!(parse_task_scheduler_manager_proof_output(&reused_engine_pid, Some(200)).is_err());
        let changed_parent = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &generations,
            &[(200, 251, 3_000), (250, 300, 2_000), (300, 4, 1_000)],
            (INSTANCE, 300),
        );
        assert!(parse_task_scheduler_manager_proof_output(&changed_parent, Some(200)).is_err());
        let disappeared_process = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &generations,
            &[(200, 250, 3_000), (250, 300, 2_000)],
            (INSTANCE, 300),
        );
        assert!(
            parse_task_scheduler_manager_proof_output(&disappeared_process, Some(200)).is_err()
        );
    }

    #[test]
    fn task_scheduler_manager_proof_rejects_cycles_and_newer_parents() {
        const INSTANCE: &str = "11111111-2222-3333-4444-555555555555";
        let cycle = [(200, 250, 3_000), (250, 200, 2_000), (200, 250, 3_000)];
        let cyclic_proof =
            task_scheduler_proof_fixture((INSTANCE, 300), &cycle, &cycle, (INSTANCE, 300));
        assert!(
            parse_task_scheduler_manager_proof_output(&cyclic_proof, Some(200))
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );

        let newer_parent = [(200, 250, 3_000), (250, 300, 4_000), (300, 4, 1_000)];
        let newer_parent_proof = task_scheduler_proof_fixture(
            (INSTANCE, 300),
            &newer_parent,
            &newer_parent,
            (INSTANCE, 300),
        );
        assert!(
            parse_task_scheduler_manager_proof_output(&newer_parent_proof, Some(200))
                .unwrap_err()
                .to_string()
                .contains("newer than its child")
        );

        let invalid_instance = task_scheduler_proof_fixture(
            ("not-a-guid", 300),
            &[(300, 4, 1_000)],
            &[(300, 4, 1_000)],
            ("not-a-guid", 300),
        );
        assert!(parse_task_scheduler_manager_proof_output(&invalid_instance, Some(300)).is_err());
    }

    #[test]
    fn service_state_home_is_resolved_to_an_absolute_path() {
        let relative = PathBuf::from(".").join("private-state");
        let expected = std::path::absolute(&relative).unwrap();
        let resolved = absolute_service_path(relative).unwrap();

        assert!(resolved.is_absolute());
        assert_eq!(resolved, expected);
    }

    #[test]
    fn service_identifiers_are_project_specific() {
        assert_eq!(WINDOWS_TASK_NAME, r"\codex-switch-global-pace-daemon");
        assert_eq!(LAUNCHD_LABEL, "com.codex-switch-global-pace.daemon");
        assert_eq!(SYSTEMD_UNIT_NAME, "codex-switch-global-pace-daemon");
    }

    #[test]
    fn service_readiness_rejects_a_foreground_winner_from_another_file() {
        let home = tempfile::tempdir().expect("temp service executable identities");
        let expected = home.path().join(if cfg!(windows) {
            "expected.exe"
        } else {
            "expected"
        });
        let contender = home.path().join(if cfg!(windows) {
            "contender.exe"
        } else {
            "contender"
        });
        std::fs::write(&expected, b"same service executable bytes")
            .expect("write expected service executable");
        std::fs::write(&contender, b"same service executable bytes")
            .expect("write contender service executable");

        let error = validate_started_service_daemon_identity(
            "fixture service",
            &expected,
            4242,
            &contender,
        )
        .expect_err("service readiness requires the exact expected executable file");
        assert!(
            error.to_string().contains("unexpected executable"),
            "{error:#}"
        );
    }

    #[test]
    fn definition_snapshot_guard_accepts_only_exact_same_presence_and_bytes() {
        assert!(definition_snapshot_matches(Some(b"same"), Some(b"same")));
        assert!(definition_snapshot_matches(None, None));
        assert!(!definition_snapshot_matches(Some(b"old"), Some(b"new")));
        assert!(!definition_snapshot_matches(Some(b"unexpected"), None));
        assert!(!definition_snapshot_matches(None, Some(b"missing")));
    }

    #[test]
    fn transaction_failure_reports_both_the_original_and_restoration_errors() {
        let error = transaction_error_with_restoration(
            anyhow::anyhow!("original stop-followup failure"),
            Err(anyhow::anyhow!("exact restoration failure")),
            "prior state restored",
            "prior state restoration incomplete",
        );
        let detail = format!("{error:#}");
        assert!(detail.contains("original stop-followup failure"));
        assert!(detail.contains("prior state restoration incomplete"));
        assert!(detail.contains("exact restoration failure"));
    }

    #[cfg(unix)]
    #[test]
    fn service_definition_publication_rolls_back_and_commits_exact_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.service");
        std::fs::write(&path, b"old definition").unwrap();
        let previous = optional_service_file_snapshot(&path).unwrap().unwrap();

        let mut rollback = publish_service_file(
            staged_service_file(&path, b"first candidate").unwrap(),
            &path,
            Some(&previous),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first candidate");
        rollback.rollback().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old definition");

        let restored = optional_service_file_snapshot(&path).unwrap().unwrap();
        let mut commit = publish_service_file(
            staged_service_file(&path, b"second candidate").unwrap(),
            &path,
            Some(&restored),
        )
        .unwrap();
        commit.commit().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second candidate");
        assert!(
            !service_transaction_path(&path, ".candidate")
                .unwrap()
                .exists()
        );
        assert!(!service_transaction_path(&path, ".backup").unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn service_definition_removal_is_token_bound_and_reversible() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.service");
        std::fs::write(&path, b"owned definition").unwrap();
        let previous = optional_service_file_snapshot(&path).unwrap().unwrap();

        let mut rollback = begin_service_file_removal(&path, &previous).unwrap();
        assert!(!path.exists());
        rollback.rollback().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"owned definition");

        let restored = optional_service_file_snapshot(&path).unwrap().unwrap();
        let mut commit = begin_service_file_removal(&path, &restored).unwrap();
        assert!(matches!(
            commit.commit().unwrap(),
            ServiceFileRemovalCommit::Durable
        ));
        assert!(!path.exists());
        assert!(
            !service_transaction_path(&path, ".removed")
                .unwrap()
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn applied_service_removal_cleanup_failure_is_not_rolled_back() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.service");
        std::fs::write(&path, b"owned definition").unwrap();
        let previous = optional_service_file_snapshot(&path).unwrap().unwrap();
        let mut removal = begin_service_file_removal(&path, &previous).unwrap();

        std::fs::remove_file(&removal.removed).unwrap();
        let outcome = removal.finish_commit(ServiceFileCleanupOutcome::DurabilityUnconfirmed(
            "injected post-unlink sync failure".to_string(),
        ));

        let ServiceFileRemovalCommit::AppliedCleanupUnconfirmed(error) = outcome else {
            panic!("an explicit unconfirmed cleanup must remain classified as applied");
        };
        assert!(
            format!("{error:#}").contains("injected post-unlink sync failure"),
            "{error:#}"
        );
        removal
            .rollback()
            .expect("an applied removal must not attempt to recreate the definition");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn service_definition_commit_never_deletes_a_concurrent_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.service");
        let external = temp.path().join("external.service");
        std::fs::write(&path, b"old definition").unwrap();
        let previous = optional_service_file_snapshot(&path).unwrap().unwrap();
        let mut publication = publish_service_file(
            staged_service_file(&path, b"candidate").unwrap(),
            &path,
            Some(&previous),
        )
        .unwrap();
        std::fs::write(&external, b"external definition").unwrap();
        std::fs::rename(&external, &path).unwrap();

        publication.commit().unwrap_err();

        assert_eq!(std::fs::read(&path).unwrap(), b"external definition");
        assert!(service_transaction_path(&path, ".backup").unwrap().exists());
    }

    #[test]
    fn task_scheduler_listing_uses_one_exact_task_name_field() {
        let listing = b"\"\\other\",\"N/A\",\"Ready\"\r\n\
\"\\CODEX-SWITCH-GLOBAL-PACE-DAEMON\",\"N/A\",\"Ready\"\r\n";
        assert!(task_listing_contains_name(listing, WINDOWS_TASK_NAME).unwrap());
        assert!(
            !task_listing_contains_name(
                b"\"\\codex-switch-global-pace-daemon-other\",\"N/A\",\"Ready\"\r\n",
                WINDOWS_TASK_NAME
            )
            .unwrap()
        );
        assert!(task_listing_contains_name(b"localized error\r\n", WINDOWS_TASK_NAME).is_err());
        let duplicate = format!(
            "\"{WINDOWS_TASK_NAME}\",\"N/A\",\"Ready\"\r\n\"{WINDOWS_TASK_NAME}\",\"N/A\",\"Ready\"\r\n"
        );
        assert!(task_listing_contains_name(duplicate.as_bytes(), WINDOWS_TASK_NAME).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_service_operation_lease_is_one_global_kernel_mutex() {
        let first = acquire_service_operation_lease().unwrap();
        assert!(
            std::thread::spawn(|| acquire_service_operation_lease().is_err())
                .join()
                .unwrap()
        );
        drop(first);
        acquire_service_operation_lease().unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_service_operation_lease_rejects_a_concurrent_lifecycle_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon-service-operation.lock");
        let first = acquire_service_operation_lease_at(&path).unwrap();
        let contender_path = path.clone();
        let contender = std::thread::spawn(move || {
            acquire_service_operation_lease_at(&contender_path).map(|_| ())
        });
        let error = contender
            .join()
            .unwrap()
            .expect_err("a concurrent daemon start must not enter the held lifecycle lease");
        assert!(
            error.to_string().contains("already in progress"),
            "{error:#}"
        );

        drop(first);
        acquire_service_operation_lease_at(&path).unwrap();
    }

    #[test]
    fn expected_service_executable_must_be_absolute_and_lexically_normal() {
        let current = std::env::current_exe().unwrap();
        validate_expected_executable(&current).unwrap();
        assert!(validate_expected_executable(Path::new("relative/binary")).is_err());

        let non_normal = current
            .parent()
            .unwrap()
            .join("..")
            .join(current.file_name().unwrap());
        assert!(validate_expected_executable(&non_normal).is_err());
    }

    #[test]
    fn service_migration_authority_is_explicit_and_platform_scoped() {
        let absolute = std::env::current_exe().unwrap();
        let result = validate_install_migration_authority(Some(&absolute));
        if cfg!(windows) {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("not supported on Windows")
            );
        } else {
            result.unwrap();
        }
        validate_install_migration_authority(None).unwrap();
        assert!(validate_install_migration_authority(Some(Path::new("relative/service"))).is_err());
    }

    #[test]
    fn launchd_plist_runs_foreground_daemon() {
        let plist = launchd_plist(
            "/usr/local/bin/codex-switch-global-pace",
            "/Users/alice",
            "/Users/alice/.codex",
            "/Volumes/private/codex-switch",
        );
        assert!(plist.contains("<string>/usr/local/bin/codex-switch-global-pace</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("<string>--foreground</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>CODEX_HOME</key>"));
        assert!(plist.contains("<string>/Users/alice/.codex</string>"));
        assert!(plist.contains("<key>CODEX_SWITCH_HOME</key>"));
        assert!(plist.contains("<string>/Volumes/private/codex-switch</string>"));
    }

    #[test]
    fn launchd_plist_escapes_paths() {
        let plist = launchd_plist(
            "/Applications/A & B/codex-switch-global-pace",
            "/Users/a<b",
            "/Users/a&b/.codex",
            "/Users/a&b/private",
        );
        assert!(plist.contains("/Applications/A &amp; B/codex-switch-global-pace"));
        assert!(plist.contains("/Users/a&lt;b"));
        assert!(plist.contains("/Users/a&amp;b/.codex"));
        assert!(plist.contains("/Users/a&amp;b/private"));
    }

    #[test]
    fn launchd_loaded_state_uses_an_exact_label_from_a_successful_listing() {
        let listing = format!("PID\tStatus\tLabel\n-\t0\tother.agent\n4242\t0\t{LAUNCHD_LABEL}\n");
        assert!(launchd_list_contains_label(listing.as_bytes()).unwrap());
        assert!(
            !launchd_list_contains_label(
                format!("PID\tStatus\tLabel\n-\t0\t{LAUNCHD_LABEL}.other\n").as_bytes()
            )
            .unwrap()
        );
        assert!(launchd_list_contains_label(&[0xff]).is_err());
    }

    #[test]
    fn launchd_ownership_requires_the_exact_executable_and_daemon_arguments() {
        let expected = Path::new("/usr/local/bin/codex-switch-global-pace");
        let definition = serde_json::json!({
            "Label": LAUNCHD_LABEL,
            "ProgramArguments": [
                expected.to_str().unwrap(),
                "daemon",
                "start",
                "--foreground"
            ]
        });
        validate_launchd_definition_value(&definition, expected).unwrap();
        assert!(
            validate_launchd_definition_value(
                &definition,
                Path::new("/tmp/codex-switch-global-pace")
            )
            .is_err()
        );
        let mut wrong_arguments = definition;
        wrong_arguments["ProgramArguments"][1] = serde_json::json!("other");
        assert!(validate_launchd_definition_value(&wrong_arguments, expected).is_err());
    }

    #[test]
    fn systemd_unit_runs_foreground_daemon() {
        let unit = systemd_unit(
            "/usr/local/bin/codex-switch-global-pace",
            "/home/alice",
            "/home/alice/.codex",
            "/mnt/private/codex-switch",
        );
        assert!(unit.contains(
            "ExecStart=\"/usr/local/bin/codex-switch-global-pace\" daemon start --foreground"
        ));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("Environment=\"HOME=/home/alice\""));
        assert!(unit.contains("Environment=\"CODEX_HOME=/home/alice/.codex\""));
        assert!(unit.contains("Environment=\"CODEX_SWITCH_HOME=/mnt/private/codex-switch\""));
    }

    #[test]
    fn systemd_unit_quotes_special_paths() {
        let unit = systemd_unit(
            r#"/opt/$release/Codex & Tools\\codex-switch-global-pace"#,
            "/home/$USER/a & b",
            r#"/home/$USER/a & b/.codex\\custom"#,
            r#"/home/$USER/a & b/private\\custom"#,
        );
        assert!(unit.contains(
            r#"ExecStart="/opt/$$release/Codex & Tools\\\\codex-switch-global-pace" daemon start --foreground"#
        ));
        assert!(unit.contains(r#"Environment="HOME=/home/$USER/a & b""#));
        assert!(unit.contains(r#"Environment="CODEX_HOME=/home/$USER/a & b/.codex\\\\custom""#));
        assert!(
            unit.contains(r#"Environment="CODEX_SWITCH_HOME=/home/$USER/a & b/private\\\\custom""#)
        );
        assert!(!unit.contains(r#"Environment="HOME=/home/$$USER"#));
    }

    #[test]
    fn systemd_ownership_requires_the_exact_exec_start() {
        let expected = Path::new("/usr/local/bin/codex-switch-global-pace");
        let unit = systemd_unit(
            expected.to_str().unwrap(),
            "/home/alice",
            "/home/alice/.codex",
            "/home/alice/.codex-switch",
        );
        validate_systemd_definition_owner(unit.as_bytes(), expected).unwrap();
        assert!(
            validate_systemd_definition_owner(
                unit.as_bytes(),
                Path::new("/tmp/codex-switch-global-pace")
            )
            .is_err()
        );
        let duplicated = format!("{unit}ExecStart=/tmp/other daemon start --foreground\n");
        assert!(validate_systemd_definition_owner(duplicated.as_bytes(), expected).is_err());
    }

    #[test]
    fn systemd_ownership_matches_a_literal_dollar_in_the_executable_path() {
        let expected = Path::new("/opt/$release/codex-switch-global-pace");
        let unit = systemd_unit(
            expected.to_str().unwrap(),
            "/home/alice",
            "/home/alice/.codex",
            "/home/alice/.codex-switch",
        );

        validate_systemd_definition_owner(unit.as_bytes(), expected).unwrap();
        let expanding = unit.replace("/opt/$$release/", "/opt/$release/");
        assert!(validate_systemd_definition_owner(expanding.as_bytes(), expected).is_err());
    }

    #[test]
    fn systemd_unit_values_use_c_style_control_escapes() {
        let value = "line\ncarriage\rreturn\ttab\u{7}\u{8}\u{b}\u{c}\u{1}\u{85}$value";

        assert_eq!(
            systemd_quote(value),
            r#""line\ncarriage\rreturn\ttab\a\b\v\f\x01\xc2\x85$value""#
        );
        assert_eq!(
            systemd_exec_quote(value),
            r#""line\ncarriage\rreturn\ttab\a\b\v\f\x01\xc2\x85$$value""#
        );
    }

    #[test]
    fn windows_task_scheduler_command_quotes_supported_paths() {
        let command = task_scheduler_command(
            Path::new(r"C:\Program Files\codex-switch-global-pace.exe"),
            Path::new(r"C:\Users\A & B\.codex"),
            Path::new(r"D:\Private & Pace\state"),
        )
        .unwrap();

        assert_eq!(
            command,
            r#"cmd.exe /D /V:OFF /S /C set CODEX_HOME=C:\Users\A ^& B\.codex&& set CODEX_SWITCH_HOME=D:\Private ^& Pace\state&& "C:\Program Files\codex-switch-global-pace.exe" daemon start --foreground"#
        );
    }

    #[test]
    fn windows_task_scheduler_rejects_expanding_or_overlong_paths() {
        let percent_error = task_scheduler_command(
            Path::new(r"C:\bin\codex-switch-global-pace.exe"),
            Path::new(r"C:\Users\A\%TEMP%\.codex"),
            Path::new(r"C:\Users\A\.codex-switch"),
        )
        .unwrap_err();
        assert!(percent_error.to_string().contains("contains '%'"));

        let long_home = format!(r"C:\{}", "x".repeat(300));
        let length_error = task_scheduler_command(
            Path::new(r"C:\bin\codex-switch-global-pace.exe"),
            Path::new(r"C:\Users\A\.codex"),
            Path::new(&long_home),
        )
        .unwrap_err();
        assert!(length_error.to_string().contains("262-character limit"));
    }

    #[test]
    fn scheduled_task_ownership_requires_the_exact_embedded_executable() {
        let expected = Path::new(r"C:\Program Files\codex-switch-global-pace.exe");
        let task_run = task_scheduler_command(
            expected,
            Path::new(r"C:\Users\Alice & Bob\.codex"),
            Path::new(r"C:\Users\Alice & Bob\.codex-switch"),
        )
        .unwrap();
        let arguments = task_run.strip_prefix("cmd.exe ").unwrap();
        let escaped_arguments = arguments
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let xml = format!(
            "<?xml version=\"1.0\"?><Task><Actions><Exec><Command>cmd.exe</Command><Arguments>{escaped_arguments}</Arguments></Exec></Actions></Task>"
        );
        validate_task_scheduler_definition_owner(xml.as_bytes(), expected).unwrap();

        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        validate_task_scheduler_definition_owner(&utf16, expected).unwrap();
        assert!(
            validate_task_scheduler_definition_owner(
                xml.as_bytes(),
                Path::new(r"C:\Other\codex-switch-global-pace.exe")
            )
            .is_err()
        );

        let injected_arguments = format!(
            r#"/D /V:OFF /S /C set CODEX_HOME=C:\Users\Alice&& calc.exe&& set CODEX_SWITCH_HOME=C:\state&& \"{}\" daemon start --foreground"#,
            expected.display()
        )
        .replace('&', "&amp;")
        .replace('"', "&quot;");
        let injected_xml = format!(
            "<?xml version=\"1.0\"?><Task><Actions><Exec><Command>cmd.exe</Command><Arguments>{injected_arguments}</Arguments></Exec></Actions></Task>"
        );
        assert!(
            validate_task_scheduler_definition_owner(injected_xml.as_bytes(), expected).is_err(),
            "ownership parsing must reject commands inserted between the exact environment assignments"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_task_scheduler_command_runs_with_the_stored_argument_shape() {
        use std::os::windows::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let probe = PathBuf::from("daemon probe.cmd");
        // `!PATH!` proves TaskRun explicitly disables registry-configured
        // delayed expansion instead of relying on the machine default.
        let codex_home = PathBuf::from(r"A !PATH! & B\.codex");
        let app_home = PathBuf::from(r"Private & Pace\state");
        std::fs::write(
            dir.path().join(&probe),
            "@echo off\r\nset CODEX_HOME\r\nset CODEX_SWITCH_HOME\r\nexit /b 0\r\n",
        )
        .unwrap();

        let task_run = task_scheduler_command(&probe, &codex_home, &app_home).unwrap();
        let command_argument = task_run
            .strip_prefix("cmd.exe /D /V:OFF /S /C ")
            .expect("TaskRun must keep cmd arguments separate from its executable");
        let mut command = std::process::Command::new("cmd.exe");
        command.raw_arg(format!("/D /V:OFF /S /C {command_argument}"));
        let output = command.current_dir(dir.path()).output().unwrap();

        assert!(
            output.status.success(),
            "TaskRun={task_run:?} argument={command_argument:?} stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(&format!("CODEX_HOME={}\r\n", codex_home.display())));
        assert!(stdout.contains(&format!("CODEX_SWITCH_HOME={}\r\n", app_home.display())));
    }

    #[test]
    fn windows_task_scheduler_create_error_includes_elevation_guidance() {
        assert_eq!(
            task_scheduler_failure_message("create scheduled task", "ERROR: Access is denied."),
            "failed to create scheduled task: ERROR: Access is denied. Re-run `codex-switch-global-pace daemon install` from an elevated PowerShell session."
        );
    }
}
