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
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
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
}

pub fn install(expected_existing_executable: Option<PathBuf>) -> Result<()> {
    validate_install_migration_authority(expected_existing_executable.as_deref())?;
    let _lease = acquire_service_operation_lease()?;
    #[cfg(target_os = "macos")]
    return install_launchd(expected_existing_executable.as_deref());
    #[cfg(target_os = "linux")]
    return install_systemd(expected_existing_executable.as_deref());
    #[cfg(target_os = "windows")]
    return install_task_scheduler(expected_existing_executable.as_deref());
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
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
fn windows_task_file_path() -> Result<PathBuf> {
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
    Ok(PathBuf::from(windows_directory)
        .join("System32")
        .join("Tasks")
        .join(WINDOWS_TASK_NAME.trim_start_matches(['\\', '/'])))
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
            "Task Scheduler reports {}, but its trusted on-disk definition is missing; ownership cannot be proven",
            WINDOWS_TASK_NAME
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
            "Task Scheduler no longer reports {} while its XML was required",
            WINDOWS_TASK_NAME
        )
    })
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
fn optional_file_contents(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!(
            "refusing to replace non-regular service definition {}",
            path.display()
        );
    }
    Ok(Some(std::fs::read(path)?))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn require_service_file_snapshot(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    let current = optional_file_contents(path)?;
    if !definition_snapshot_matches(current.as_deref(), expected) {
        anyhow::bail!(
            "service definition {} changed during the operation; refusing to mutate it",
            path.display()
        );
    }
    Ok(())
}

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
        systemd_quote(expected)
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
fn persist_service_file(staged: tempfile::NamedTempFile, path: &std::path::Path) -> Result<()> {
    staged
        .persist(path)
        .map(|_| ())
        .map_err(|err| anyhow::Error::from(err.error))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn restore_service_file(path: &std::path::Path, previous: Option<&[u8]>) -> Result<()> {
    if let Some(previous) = previous {
        persist_service_file(staged_service_file(path, previous)?, path)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
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
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

fn wait_for_daemon_absence_after_service_stop(action: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match crate::daemon::pidfile::running_pid_checked()
            .with_context(|| format!("checking the daemon PID lock after {action}"))?
        {
            None => return Ok(()),
            Some(pid) if std::time::Instant::now() >= deadline => {
                anyhow::bail!(
                    "{action} completed, but daemon PID {pid} still owns the PID lock after 10s"
                );
            }
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

fn wait_for_daemon_presence_after_service_start(action: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut last_probe_error = None;
    while std::time::Instant::now() < deadline {
        match crate::daemon::pidfile::running_pid_checked() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => last_probe_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if let Some(error) = last_probe_error {
        return Err(error.context(format!(
            "{action} did not publish a readable, locked PID identity within 10 seconds"
        )));
    }
    anyhow::bail!("{action} did not publish a live PID within 10 seconds")
}

pub fn start_installed() -> Result<()> {
    let lease = acquire_service_operation_lease()?;
    let expected_executable = std::env::current_exe().context("locating daemon executable")?;
    start_installed_locked(&expected_executable, &lease)
}

pub(crate) fn start_installed_locked(
    expected_executable: &Path,
    _lease: &ServiceOperationLease,
) -> Result<()> {
    validate_expected_executable(expected_executable)?;
    validate_uninstall_owner(expected_executable)?;
    #[cfg(target_os = "macos")]
    return start_launchd();
    #[cfg(target_os = "linux")]
    return start_systemd();
    #[cfg(target_os = "windows")]
    return start_task_scheduler(expected_executable);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service start is not supported on this platform")
}

pub fn stop_installed() -> Result<()> {
    let lease = acquire_service_operation_lease()?;
    let expected_executable = std::env::current_exe().context("locating daemon executable")?;
    stop_installed_locked(&expected_executable, &lease)
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
        wait_for_scheduled_daemon()?;
    } else if !was_running && running {
        anyhow::bail!(
            "a daemon generation started during failed scheduled-task uninstall; refusing to claim the prior stopped state was restored"
        );
    }
    if crate::daemon::pidfile::running_pid_checked()
        .context("verifying daemon state after failed scheduled-task uninstall")?
        .is_some()
        != was_running
    {
        anyhow::bail!("daemon running state did not match its pre-uninstall value");
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
    if let Err(start_err) = wait_for_scheduled_daemon() {
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
pub(crate) fn stop_failed_start_for_self_update() -> Result<()> {
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
</plist>"#,
        exe = exe,
        home = home,
        codex_home = codex_home,
        app_home = app_home,
        label = LAUNCHD_LABEL,
    )
}

#[cfg(any(target_os = "macos", test))]
fn launchd_list_contains_label(stdout: &[u8]) -> Result<bool> {
    let stdout = std::str::from_utf8(stdout).context("launchctl list output is not UTF-8")?;
    Ok(stdout
        .lines()
        .any(|line| line.split_ascii_whitespace().last() == Some(LAUNCHD_LABEL)))
}

#[cfg(target_os = "macos")]
fn launchctl_failure_detail(output: &std::process::Output) -> String {
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
            launchctl_failure_detail(&output)
        );
    }
    launchd_list_contains_label(&output.stdout)
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
    let output = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .with_context(|| format!("running launchctl for {action}"))?;
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
            launchctl_failure_detail(&output)
        );
    }
    // Legacy `load`/`unload` can report per-job failures without a reliable
    // process exit status. The successful full-list postcondition above is the
    // sole state authority.
    Ok(())
}

#[cfg(target_os = "macos")]
fn unload_launchd(path: &Path) -> Result<()> {
    let path = path.to_str().ok_or_else(|| {
        anyhow::anyhow!("LaunchAgent path is not valid Unicode: {}", path.display())
    })?;
    launchctl_require_loaded_state(&["unload", path], false, "unload LaunchAgent")
}

#[cfg(target_os = "macos")]
fn rollback_launchd_uninstall(path: &Path, previous: &[u8], was_loaded: bool) -> Result<()> {
    match optional_file_contents(path)? {
        Some(current) if current == previous => {}
        Some(_) => anyhow::bail!(
            "LaunchAgent definition {} changed during uninstall; refusing to overwrite it during rollback",
            path.display()
        ),
        None => restore_service_file(path, Some(previous))
            .with_context(|| format!("restoring LaunchAgent definition {}", path.display()))?,
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
        wait_for_daemon_presence_after_service_start("restored LaunchAgent")?;
    }
    if launchd_is_loaded()? != was_loaded {
        anyhow::bail!("restored LaunchAgent loaded state did not match its pre-uninstall state");
    }
    if !was_loaded {
        wait_for_daemon_absence_after_service_stop("LaunchAgent uninstall rollback")?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rollback_launchd_install(
    path: &Path,
    published: &[u8],
    previous: Option<&[u8]>,
    was_loaded: bool,
) -> Result<()> {
    if launchd_is_loaded()? {
        unload_launchd(path).context("unloading failed new LaunchAgent during rollback")?;
    }
    wait_for_daemon_absence_after_service_stop("LaunchAgent install rollback")?;
    require_service_file_snapshot(path, Some(published))?;
    restore_service_file(path, previous)
        .with_context(|| format!("restoring LaunchAgent definition {}", path.display()))?;
    if was_loaded {
        load_launchd(path).context("restoring the previously loaded LaunchAgent")?;
        wait_for_daemon_presence_after_service_start("restored LaunchAgent")?;
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
fn install_launchd(expected_existing_executable: Option<&Path>) -> Result<()> {
    let executable = std::env::current_exe().context("locating daemon executable")?;
    let exe = executable.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let codex_home = effective_codex_home()?.display().to_string();
    let app_home = effective_app_home()?.display().to_string();
    let plist = launchd_plist(&exe, &home, &codex_home, &app_home);

    let path = plist_path()?;
    let previous = optional_file_contents(&path)?;
    if let Some(previous) = previous.as_deref() {
        validate_launchd_definition_owner(
            previous,
            expected_existing_executable.unwrap_or(&executable),
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
        .stdout(std::process::Stdio::null())
        .status()?;
    if !validation.success() {
        anyhow::bail!("generated LaunchAgent failed plutil validation");
    }

    let was_loaded = launchd_is_loaded()?;
    if was_loaded && previous.is_none() {
        anyhow::bail!(
            "LaunchAgent label {LAUNCHD_LABEL} is loaded without a restorable definition at {}; refusing to replace it",
            path.display()
        );
    }
    require_service_file_snapshot(&path, previous.as_deref())?;
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
    require_service_file_snapshot(&path, previous.as_deref())?;

    if let Err(err) = persist_service_file(staged, &path) {
        if was_loaded
            && let Err(rollback_err) = require_service_file_snapshot(&path, previous.as_deref())
                .and_then(|()| load_launchd(&path))
                .and_then(|()| wait_for_daemon_presence_after_service_start("restored LaunchAgent"))
        {
            return Err(err.context(format!(
                "atomically replacing the LaunchAgent failed and the existing LaunchAgent could not be restarted: {rollback_err}"
            )));
        }
        return Err(err.context("atomically replacing LaunchAgent definition"));
    }

    if let Err(install_err) = load_launchd(&path)
        .and_then(|()| wait_for_daemon_presence_after_service_start("new LaunchAgent"))
    {
        if let Err(rollback_err) =
            rollback_launchd_install(&path, plist.as_bytes(), previous.as_deref(), was_loaded)
        {
            return Err(install_err.context(format!(
                "new LaunchAgent failed and rollback also failed: {rollback_err}"
            )));
        }
        return Err(install_err.context("new LaunchAgent failed; previous definition was restored"));
    }
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
fn start_launchd() -> Result<()> {
    let path = plist_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_launchd_service()?;
        anyhow::bail!("LaunchAgent not installed");
    };
    let current_executable = std::env::current_exe().context("locating daemon executable")?;
    validate_launchd_definition_owner(&contents, &current_executable)?;
    if launchd_is_loaded()? {
        let output = std::process::Command::new("launchctl")
            .args(["start", LAUNCHD_LABEL])
            .output()
            .context("starting loaded LaunchAgent")?;
        if !output.status.success() || !launchd_is_loaded()? {
            anyhow::bail!(
                "launchctl start did not preserve a loaded LaunchAgent: {}",
                launchctl_failure_detail(&output)
            );
        }
    } else {
        load_launchd(&path)?;
    }
    wait_for_daemon_presence_after_service_start("LaunchAgent")?;
    user_println("Started LaunchAgent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_launchd(expected_executable: &Path) -> Result<()> {
    let path = plist_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_launchd_service()?;
        user_println("LaunchAgent not installed");
        return Ok(());
    };
    validate_launchd_definition_owner(&contents, expected_executable)?;
    if launchd_is_loaded()? {
        unload_launchd(&path)?;
    }
    wait_for_daemon_absence_after_service_stop("launchctl unload")?;
    user_println("Stopped LaunchAgent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launchd(expected_executable: &Path) -> Result<()> {
    let path = plist_path()?;
    let Some(previous) = optional_file_contents(&path)? else {
        require_no_definitionless_launchd_service()?;
        user_println("LaunchAgent not installed");
        return Ok(());
    };
    validate_launchd_definition_owner(&previous, expected_executable)?;
    let was_loaded = launchd_is_loaded()?;
    let uninstall_result = (|| {
        if was_loaded {
            unload_launchd(&path)?;
        }
        wait_for_daemon_absence_after_service_stop("launchctl unload")?;
        require_service_file_snapshot(&path, Some(&previous))?;
        std::fs::remove_file(&path)
            .with_context(|| format!("removing LaunchAgent definition {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = uninstall_result {
        return Err(launchd_uninstall_error(
            error,
            rollback_launchd_uninstall(&path, &previous, was_loaded),
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
    let exe = systemd_quote(exe);
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
"#,
        exe = exe,
        home = home,
        codex_home = codex_home,
        app_home = app_home,
    )
}

#[cfg(target_os = "linux")]
fn install_systemd(expected_existing_executable: Option<&Path>) -> Result<()> {
    let executable = std::env::current_exe().context("locating daemon executable")?;
    let exe = executable.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let codex_home = effective_codex_home()?.display().to_string();
    let app_home = effective_app_home()?.display().to_string();

    let unit = systemd_unit(&exe, &home, &codex_home, &app_home);

    let path = unit_path()?;
    let previous = optional_file_contents(&path)?;
    if let Some(previous) = previous.as_deref() {
        validate_systemd_definition_owner(
            previous,
            expected_existing_executable.unwrap_or(&executable),
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
        .status()?;
    if !validation.success() {
        anyhow::bail!("generated systemd user service failed validation");
    }

    let was_active = systemctl_query("is-active")?;
    let was_enabled = systemctl_query("is-enabled")?;
    if previous.is_none() && (was_active || was_enabled) {
        anyhow::bail!(
            "systemd unit {SYSTEMD_UNIT_NAME} is active or enabled without a restorable definition at {}; refusing to replace it",
            path.display()
        );
    }
    require_service_file_snapshot(&path, previous.as_deref())?;
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
    require_service_file_snapshot(&path, previous.as_deref())?;

    if let Err(err) = persist_service_file(staged, &path) {
        if was_active
            && let Err(rollback_err) = require_service_file_snapshot(&path, previous.as_deref())
                .and_then(|()| {
                    systemctl_require(&["start", SYSTEMD_UNIT_NAME], "restart existing service")
                })
                .and_then(|()| {
                    wait_for_daemon_presence_after_service_start(
                        "restarted existing systemd service",
                    )
                })
        {
            return Err(err.context(format!(
                "atomically replacing the systemd user service failed and the existing service could not be restarted: {rollback_err}"
            )));
        }
        return Err(err.context("atomically replacing systemd user service"));
    }

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
            wait_for_daemon_presence_after_service_start("new systemd service")
        })
    {
        if let Err(rollback_err) = rollback_systemd_install(
            &path,
            unit.as_bytes(),
            previous.as_deref(),
            was_enabled,
            was_active,
        ) {
            return Err(install_err.context(format!(
                "new systemd service failed and rollback also failed: {rollback_err}"
            )));
        }
        return Err(
            install_err.context("new systemd service failed; previous definition was restored")
        );
    }
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
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to {action}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rollback_systemd_install(
    path: &std::path::Path,
    published: &[u8],
    previous: Option<&[u8]>,
    was_enabled: bool,
    was_active: bool,
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
    require_service_file_snapshot(path, Some(published))?;
    if !was_enabled {
        systemctl_require(
            &["disable", SYSTEMD_UNIT_NAME],
            "remove enablement for failed new systemd service",
        )?;
    }
    restore_service_file(path, previous)?;
    systemctl_require(&["daemon-reload"], "reload restored systemd user units")?;
    if was_enabled {
        systemctl_require(
            &["enable", SYSTEMD_UNIT_NAME],
            "restore enabled service state",
        )?;
    } else if previous.is_some() {
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
        wait_for_daemon_presence_after_service_start("restored systemd service")?;
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
    previous: &[u8],
    was_enabled: bool,
    was_active: bool,
) -> Result<()> {
    match optional_file_contents(path)? {
        Some(current) if current == previous => {}
        Some(_) => anyhow::bail!(
            "systemd definition {} changed during uninstall; refusing to overwrite it during rollback",
            path.display()
        ),
        None => restore_service_file(path, Some(previous))
            .with_context(|| format!("restoring systemd definition {}", path.display()))?,
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
        wait_for_daemon_presence_after_service_start("restored systemd service")?;
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
fn start_systemd() -> Result<()> {
    let path = unit_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_systemd_service()?;
        anyhow::bail!("systemd service not installed");
    };
    let current_executable = std::env::current_exe().context("locating daemon executable")?;
    validate_systemd_definition_owner(&contents, &current_executable)?;
    systemctl_require(&["start", SYSTEMD_UNIT_NAME], "start systemd user service")?;
    if !systemctl_query("is-active")? {
        anyhow::bail!("systemctl start returned without an active systemd user service");
    }
    wait_for_daemon_presence_after_service_start("systemd user service")?;
    user_println("Started systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn stop_systemd(expected_executable: &Path) -> Result<()> {
    let path = unit_path()?;
    let Some(contents) = optional_file_contents(&path)? else {
        require_no_definitionless_systemd_service()?;
        anyhow::bail!("systemd service not installed");
    };
    validate_systemd_definition_owner(&contents, expected_executable)?;
    systemctl_require(&["stop", SYSTEMD_UNIT_NAME], "stop systemd user service")?;
    if systemctl_query("is-active")? {
        anyhow::bail!("systemctl stop returned while the systemd user service remained active");
    }
    wait_for_daemon_absence_after_service_stop("systemctl stop")?;
    user_println("Stopped systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd(expected_executable: &Path) -> Result<()> {
    let path = unit_path()?;
    let Some(previous) = optional_file_contents(&path)? else {
        require_no_definitionless_systemd_service()?;
        user_println("systemd service not installed");
        return Ok(());
    };
    validate_systemd_definition_owner(&previous, expected_executable)?;
    let was_active = systemctl_query("is-active")?;
    let was_enabled = systemctl_query("is-enabled")?;
    let uninstall_result = (|| {
        systemctl_require(
            &["disable", "--now", SYSTEMD_UNIT_NAME],
            "disable and stop systemd user service",
        )?;
        if systemctl_query("is-active")? || systemctl_query("is-enabled")? {
            anyhow::bail!("systemctl disable --now returned without an inactive, disabled service");
        }
        wait_for_daemon_absence_after_service_stop("systemctl disable --now")?;
        require_service_file_snapshot(&path, Some(&previous))?;
        std::fs::remove_file(&path)
            .with_context(|| format!("removing systemd definition {}", path.display()))?;
        systemctl_require(
            &["daemon-reload"],
            "reload systemd user units after uninstall",
        )?;
        Ok(())
    })();
    if let Err(error) = uninstall_result {
        return Err(systemd_uninstall_error(
            error,
            rollback_systemd_uninstall(&path, &previous, was_enabled, was_active),
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

    if previous_definition.is_some() {
        stop_scheduled_daemon_for_rollback().context(
            "staged task was valid, but the existing daemon could not be stopped safely; live task was left unchanged",
        )?;
    }
    require_task_definition_snapshot(&task_file, previous_definition.as_deref())?;
    require_task_xml_snapshot(&task_file, previous_xml.as_deref())?;

    if let Err(install_err) =
        create_scheduled_task(WINDOWS_TASK_NAME, &task_run, previous_definition.is_some())
    {
        let rollback_result = restore_scheduled_task(
            &task_file,
            previous_definition.as_deref(),
            previous_xml.as_deref(),
            previous_definition.as_deref(),
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

    let published_definition = optional_task_definition(&task_file)?.ok_or_else(|| {
        anyhow::anyhow!(
            "scheduled-task creation returned success, but definition {} is missing",
            task_file.display()
        )
    })?;
    let published_xml = query_scheduled_task_xml("export newly installed scheduled task")?;
    let install_result = validate_task_scheduler_definition_owner(&published_definition, &exe)
        .and_then(|()| validate_task_scheduler_definition_owner(&published_xml, &exe))
        .and_then(|()| require_task_definition_snapshot(&task_file, Some(&published_definition)))
        .and_then(|()| require_task_xml_snapshot(&task_file, Some(&published_xml)))
        .and_then(|()| {
            schtasks(&["/Run", "/TN", WINDOWS_TASK_NAME], "start scheduled task")?;
            Ok(())
        })
        .and_then(|_| wait_for_scheduled_daemon())
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
        "Installed Windows scheduled task {}",
        WINDOWS_TASK_NAME
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
fn wait_for_scheduled_daemon() -> Result<()> {
    wait_for_daemon_presence_after_service_start("scheduled daemon")
}

#[cfg(target_os = "windows")]
fn restore_scheduled_task(
    task_file: &Path,
    expected_current: Option<&[u8]>,
    expected_current_xml: Option<&[u8]>,
    previous_definition: Option<&[u8]>,
    previous_xml: Option<&mut tempfile::NamedTempFile>,
    previous_was_running: bool,
) -> Result<()> {
    require_task_definition_snapshot(task_file, expected_current)?;
    require_task_xml_snapshot(task_file, expected_current_xml)?;
    if expected_current != previous_definition {
        stop_scheduled_daemon_for_rollback()?;
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
            wait_for_scheduled_daemon()?;
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
    if crate::daemon::pidfile::running_pid_checked()
        .context("verifying scheduled-task running state after install rollback")?
        .is_some()
        != previous_was_running
    {
        anyhow::bail!("restored scheduled-task running state did not match its pre-install state");
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
            wait_for_scheduled_daemon()?;
        }
    } else {
        schtasks(
            &["/End", "/TN", WINDOWS_TASK_NAME],
            "restore stopped scheduled-task state after failed uninstall",
        )?;
        wait_for_daemon_absence_after_service_stop("scheduled-task uninstall rollback")?;
    }
    if crate::daemon::pidfile::running_pid_checked()
        .context("verifying scheduled-task running state after uninstall rollback")?
        .is_some()
        != was_running
    {
        anyhow::bail!(
            "restored scheduled-task running state did not match its pre-uninstall state"
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
    if let Some(pid) = crate::daemon::pidfile::running_pid_checked()
        .context("checking the scheduled daemon PID lock before rollback cleanup")?
    {
        crate::daemon::pidfile::request_shutdown(pid)
            .context("requesting graceful shutdown of the scheduled daemon")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match crate::daemon::pidfile::running_pid_checked()
                .context("checking scheduled daemon shutdown progress")?
            {
                None => break,
                Some(current_pid) if current_pid == pid => {}
                Some(current_pid) => anyhow::bail!(
                    "scheduled daemon generation changed from PID {pid} to PID {current_pid} during rollback cleanup; refusing to force-stop it"
                ),
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if crate::daemon::pidfile::running_pid_checked()
            .context("performing the final scheduled daemon PID-lock check")?
            .is_some()
        {
            anyhow::bail!(
                "new scheduled daemon did not finish its graceful shutdown; refusing to force-stop it during rollback"
            );
        }
    }
    // With no live generation-bound daemon, `/End` clears Task Scheduler's
    // bookkeeping for a failed or already-exited instance. Its success is
    // required: an absent PID file alone cannot prove a queued task will not
    // start after the executable is rolled back.
    schtasks(
        &["/End", "/TN", WINDOWS_TASK_NAME],
        "end inactive failed task",
    )?;
    crate::daemon::pidfile::cleanup_pidfile()?;
    Ok(())
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
    use super::{
        LAUNCHD_LABEL, SYSTEMD_UNIT_NAME, WINDOWS_TASK_NAME, absolute_service_path,
        definition_snapshot_matches, launchd_list_contains_label, launchd_plist, systemd_unit,
        task_listing_contains_name, task_scheduler_command, task_scheduler_failure_message,
        validate_expected_executable, validate_install_migration_authority,
        validate_launchd_definition_value, validate_systemd_definition_owner,
        validate_task_scheduler_definition_owner,
    };
    use std::path::{Path, PathBuf};

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
    fn definition_snapshot_guard_accepts_only_exact_same_presence_and_bytes() {
        assert!(definition_snapshot_matches(Some(b"same"), Some(b"same")));
        assert!(definition_snapshot_matches(None, None));
        assert!(!definition_snapshot_matches(Some(b"old"), Some(b"new")));
        assert!(!definition_snapshot_matches(Some(b"unexpected"), None));
        assert!(!definition_snapshot_matches(None, Some(b"missing")));
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
            r#"/opt/Codex & Tools\\codex-switch-global-pace"#,
            "/home/a & b",
            r#"/home/a & b/.codex\\custom"#,
            r#"/home/a & b/private\\custom"#,
        );
        assert!(unit.contains(
            r#"ExecStart="/opt/Codex & Tools\\\\codex-switch-global-pace" daemon start --foreground"#
        ));
        assert!(unit.contains(r#"Environment="HOME=/home/a & b""#));
        assert!(unit.contains(r#"Environment="CODEX_HOME=/home/a & b/.codex\\\\custom""#));
        assert!(unit.contains(r#"Environment="CODEX_SWITCH_HOME=/home/a & b/private\\\\custom""#));
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
