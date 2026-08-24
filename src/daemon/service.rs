use crate::output::user_println;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "windows", test))]
const WINDOWS_TASK_NAME: &str = r"\codex-switch-global-pace-daemon";
#[cfg(any(target_os = "macos", test))]
const LAUNCHD_LABEL: &str = "com.codex-switch-global-pace.daemon";
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT_NAME: &str = "codex-switch-global-pace-daemon";

pub fn install() -> Result<()> {
    #[cfg(target_os = "macos")]
    return install_launchd();
    #[cfg(target_os = "linux")]
    return install_systemd();
    #[cfg(target_os = "windows")]
    return install_task_scheduler();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service install is not supported on this platform")
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    return uninstall_launchd();
    #[cfg(target_os = "linux")]
    return uninstall_systemd();
    #[cfg(target_os = "windows")]
    return uninstall_task_scheduler();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service uninstall is not supported on this platform")
}

/// Read the installed-service marker without treating metadata or scheduler
/// errors as "not installed". Self-update uses this before stopping a running
/// daemon because it must restore the same launch mechanism on rollback.
pub(crate) fn is_installed_checked() -> Result<bool> {
    #[cfg(target_os = "macos")]
    return checked_regular_file(&plist_path()?, "LaunchAgent definition");
    #[cfg(target_os = "linux")]
    return checked_regular_file(&unit_path()?, "systemd user-service definition");
    #[cfg(target_os = "windows")]
    {
        let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
            anyhow::anyhow!("SystemRoot is not set; cannot inspect scheduled task")
        })?;
        let task_file = PathBuf::from(system_root)
            .join("System32")
            .join("Tasks")
            .join(WINDOWS_TASK_NAME.trim_start_matches(['\\', '/']));
        if !checked_regular_file(&task_file, "Windows scheduled-task definition")? {
            return Ok(false);
        }
        schtasks(
            &["/Query", "/TN", WINDOWS_TASK_NAME],
            "verify installed scheduled task",
        )?;
        Ok(true)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    Ok(false)
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

fn verify_daemon_absent_after_service_stop(
    action: &str,
    stop_succeeded: bool,
    running_pid: Result<Option<u32>>,
) -> Result<()> {
    let running_pid =
        running_pid.with_context(|| format!("checking the daemon PID lock after {action}"))?;
    if let Some(pid) = running_pid {
        let service_result = if stop_succeeded {
            "reported success"
        } else {
            "failed"
        };
        anyhow::bail!(
            "{action} {service_result}, but daemon PID {pid} still owns the PID lock; refusing to remove the service definition"
        );
    }
    if !stop_succeeded {
        tracing::warn!("{action} failed, but the checked daemon PID lock is absent");
    }
    Ok(())
}

pub fn start_installed() -> Result<()> {
    #[cfg(target_os = "macos")]
    return start_launchd();
    #[cfg(target_os = "linux")]
    return start_systemd();
    #[cfg(target_os = "windows")]
    return start_task_scheduler();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service start is not supported on this platform")
}

pub fn stop_installed() -> Result<()> {
    #[cfg(target_os = "macos")]
    return stop_launchd();
    #[cfg(target_os = "linux")]
    return stop_systemd();
    #[cfg(target_os = "windows")]
    return stop_task_scheduler();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("Service stop is not supported on this platform")
}

#[cfg(target_os = "windows")]
fn start_task_scheduler() -> Result<()> {
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
fn stop_task_scheduler() -> Result<()> {
    schtasks(&["/End", "/TN", WINDOWS_TASK_NAME], "stop scheduled task")?;
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

#[cfg(target_os = "macos")]
fn install_launchd() -> Result<()> {
    let exe = std::env::current_exe()?.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let codex_home = effective_codex_home()?.display().to_string();
    let app_home = effective_app_home()?.display().to_string();
    let plist = launchd_plist(&exe, &home, &codex_home, &app_home);

    let path = plist_path()?;
    let previous = optional_file_contents(&path)?;
    if previous.is_some() {
        user_println(&format!(
            "Warning: overwriting existing LaunchAgent at {}",
            path.display()
        ));
    }
    let staged = staged_service_file(&path, plist.as_bytes())?;
    let validation = std::process::Command::new("plutil")
        .args(["-lint", &staged.path().display().to_string()])
        .stdout(std::process::Stdio::null())
        .status()?;
    if !validation.success() {
        anyhow::bail!("generated LaunchAgent failed plutil validation");
    }

    let was_loaded = std::process::Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?
        .success();
    if was_loaded && previous.is_none() {
        anyhow::bail!(
            "LaunchAgent label {LAUNCHD_LABEL} is loaded without a restorable definition at {}; refusing to replace it",
            path.display()
        );
    }
    if was_loaded {
        let stopped = std::process::Command::new("launchctl")
            .args(["unload", &path.display().to_string()])
            .status()?;
        if !stopped.success() {
            anyhow::bail!("launchctl unload failed; existing LaunchAgent was left unchanged");
        }
    }

    if let Err(err) = persist_service_file(staged, &path) {
        if was_loaded && let Err(rollback_err) = load_launchd(&path) {
            return Err(err.context(format!(
                "atomically replacing the LaunchAgent failed and the existing LaunchAgent could not be restarted: {rollback_err}"
            )));
        }
        return Err(err.context("atomically replacing LaunchAgent definition"));
    }

    if let Err(install_err) = load_launchd(&path) {
        let restore_result = restore_service_file(&path, previous.as_deref());
        let restart_result = if was_loaded && restore_result.is_ok() {
            load_launchd(&path)
        } else {
            Ok(())
        };
        if let Err(rollback_err) = restore_result.and(restart_result) {
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
    let status = std::process::Command::new("launchctl")
        .args(["load", &path.display().to_string()])
        .status()?;
    if !status.success() {
        anyhow::bail!("launchctl load failed");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_launchd() -> Result<()> {
    let path = plist_path()?;
    if !path.exists() {
        anyhow::bail!("LaunchAgent not installed");
    }
    let status = std::process::Command::new("launchctl")
        .args(["load", &path.display().to_string()])
        .status()?;
    if !status.success() {
        let start_status = std::process::Command::new("launchctl")
            .args(["start", LAUNCHD_LABEL])
            .status()?;
        if !start_status.success() {
            anyhow::bail!("launchctl load/start failed");
        }
    }
    user_println("Started LaunchAgent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_launchd() -> Result<()> {
    let path = plist_path()?;
    if !path.exists() {
        user_println("LaunchAgent not installed");
        return Ok(());
    }
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.display().to_string()])
        .status();
    user_println("Stopped LaunchAgent");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launchd() -> Result<()> {
    let path = plist_path()?;
    if !path.exists() {
        user_println("LaunchAgent not installed");
        return Ok(());
    }
    let stopped = std::process::Command::new("launchctl")
        .args(["unload", &path.display().to_string()])
        .status()
        .is_ok_and(|status| status.success());
    verify_daemon_absent_after_service_stop(
        "launchctl unload",
        stopped,
        crate::daemon::pidfile::running_pid_checked(),
    )?;
    std::fs::remove_file(&path)?;
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
fn install_systemd() -> Result<()> {
    let exe = std::env::current_exe()?.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let codex_home = effective_codex_home()?.display().to_string();
    let app_home = effective_app_home()?.display().to_string();

    let unit = systemd_unit(&exe, &home, &codex_home, &app_home);

    let path = unit_path()?;
    let previous = optional_file_contents(&path)?;
    if previous.is_some() {
        user_println(&format!(
            "Warning: overwriting existing systemd service at {}",
            path.display()
        ));
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
    if was_active {
        systemctl_require(
            &["stop", SYSTEMD_UNIT_NAME],
            "stop existing systemd service",
        )?;
    }

    if let Err(err) = persist_service_file(staged, &path) {
        if was_active
            && let Err(rollback_err) =
                systemctl_require(&["start", SYSTEMD_UNIT_NAME], "restart existing service")
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
    {
        if let Err(rollback_err) =
            rollback_systemd_install(&path, previous.as_deref(), was_enabled, was_active)
        {
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
    let removed_new_enablement = if previous.is_none() {
        systemctl_require(
            &["disable", SYSTEMD_UNIT_NAME],
            "remove enablement for failed new systemd service",
        )
    } else {
        Ok(())
    };
    restore_service_file(path, previous)?;
    systemctl_require(&["daemon-reload"], "reload restored systemd user units")?;
    removed_new_enablement?;
    if previous.is_some() {
        if was_enabled {
            systemctl_require(
                &["enable", SYSTEMD_UNIT_NAME],
                "restore enabled service state",
            )?;
        } else {
            systemctl_require(
                &["disable", SYSTEMD_UNIT_NAME],
                "restore disabled service state",
            )?;
        }
        if was_active {
            systemctl_require(
                &["start", SYSTEMD_UNIT_NAME],
                "restart restored systemd service",
            )?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_systemd() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", SYSTEMD_UNIT_NAME])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl start failed");
    }
    user_println("Started systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn stop_systemd() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "stop", SYSTEMD_UNIT_NAME])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl stop failed");
    }
    user_println("Stopped systemd user service");
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        user_println("systemd service not installed");
        return Ok(());
    }
    let stopped = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", SYSTEMD_UNIT_NAME])
        .status()
        .is_ok_and(|status| status.success());
    verify_daemon_absent_after_service_stop(
        "systemctl disable --now",
        stopped,
        crate::daemon::pidfile::running_pid_checked(),
    )?;
    std::fs::remove_file(&path)?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
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
fn install_task_scheduler() -> Result<()> {
    use std::io::Write;

    let exe = std::env::current_exe()?;
    let codex_home = effective_codex_home()?;
    let app_home = effective_app_home()?;
    let task_run = task_scheduler_command(&exe, &codex_home, &app_home)?;

    let previous_xml = if is_installed_checked()? {
        Some(
            schtasks(
                &["/Query", "/TN", WINDOWS_TASK_NAME, "/XML"],
                "export existing scheduled task",
            )?
            .stdout,
        )
    } else {
        None
    };
    let previous_was_running = crate::daemon::pidfile::running_pid_checked()
        .context("checking the existing daemon PID lock before scheduled-task installation")?
        .is_some();

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
    create_scheduled_task(&stage_name, &task_run)?;
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

    if previous_was_running {
        stop_scheduled_daemon_for_rollback().context(
            "staged task was valid, but the existing daemon could not be stopped safely; live task was left unchanged",
        )?;
    }

    let install_result = create_scheduled_task(WINDOWS_TASK_NAME, &task_run).and_then(|()| {
        schtasks(&["/Run", "/TN", WINDOWS_TASK_NAME], "start scheduled task")?;
        wait_for_scheduled_daemon()
    });
    if let Err(install_err) = install_result {
        let rollback_result =
            restore_scheduled_task(previous_xml_file.as_mut(), previous_was_running);
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
fn create_scheduled_task(name: &str, task_run: &str) -> Result<()> {
    schtasks(
        &[
            "/Create", "/TN", name, "/TR", task_run, "/SC", "ONLOGON", "/RL", "LIMITED", "/IT",
            "/F",
        ],
        "create scheduled task",
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn wait_for_scheduled_daemon() -> Result<()> {
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
        return Err(error.context(
            "scheduled daemon did not publish a readable, locked PID identity within 10 seconds",
        ));
    }
    anyhow::bail!("scheduled daemon did not publish a live PID within 10 seconds")
}

#[cfg(target_os = "windows")]
fn restore_scheduled_task(
    previous_xml: Option<&mut tempfile::NamedTempFile>,
    previous_was_running: bool,
) -> Result<()> {
    stop_scheduled_daemon_for_rollback()?;
    if let Some(previous_xml) = previous_xml {
        let xml_path = previous_xml.path().to_string_lossy().into_owned();
        schtasks(
            &["/Create", "/TN", WINDOWS_TASK_NAME, "/XML", &xml_path, "/F"],
            "restore previous scheduled task",
        )?;
        if previous_was_running {
            schtasks(
                &["/Run", "/TN", WINDOWS_TASK_NAME],
                "restart previous scheduled task",
            )?;
            wait_for_scheduled_daemon()?;
        }
    } else {
        if is_installed_checked()? {
            schtasks(
                &["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"],
                "remove failed new scheduled task",
            )?;
        }
        if previous_was_running {
            let executable = std::env::current_exe()?;
            let status = std::process::Command::new(executable)
                .args(["daemon", "start"])
                .status()?;
            if !status.success() {
                anyhow::bail!("failed to restore the previously running detached daemon");
            }
            wait_for_scheduled_daemon()?;
        }
    }
    Ok(())
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
fn uninstall_task_scheduler() -> Result<()> {
    let stopped = schtasks(&["/End", "/TN", WINDOWS_TASK_NAME], "stop scheduled task").is_ok();
    verify_daemon_absent_after_service_stop(
        "stopping the Windows scheduled task",
        stopped,
        crate::daemon::pidfile::running_pid_checked(),
    )?;
    if !is_installed_checked()? {
        user_println("Windows scheduled task not installed");
        return Ok(());
    }
    schtasks(
        &["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"],
        "delete scheduled task",
    )?;
    user_println("Uninstalled Windows scheduled task");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        LAUNCHD_LABEL, SYSTEMD_UNIT_NAME, WINDOWS_TASK_NAME, absolute_service_path, launchd_plist,
        systemd_unit, task_scheduler_command, task_scheduler_failure_message,
        verify_daemon_absent_after_service_stop,
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

    #[test]
    fn uninstall_requires_checked_pid_lock_absence_even_after_service_success() {
        let still_running =
            verify_daemon_absent_after_service_stop("test service stop", true, Ok(Some(4242)))
                .expect_err("service success cannot override a live PID lock");
        assert!(still_running.to_string().contains("still owns"));

        verify_daemon_absent_after_service_stop("test service stop", false, Ok(None))
            .expect("a checked absent PID lock makes definition removal safe");

        let probe_error = verify_daemon_absent_after_service_stop(
            "test service stop",
            true,
            Err(anyhow::anyhow!("PID lock probe denied")),
        )
        .expect_err("a PID-lock probe error must fail closed");
        assert!(format!("{probe_error:#}").contains("PID lock probe denied"));
    }
}
