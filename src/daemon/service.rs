use crate::output::user_println;
use anyhow::Result;
#[cfg(any(target_os = "windows", test))]
use std::path::Path;
use std::path::PathBuf;

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

pub fn is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        plist_path().is_ok_and(|path| path.exists())
    }
    #[cfg(target_os = "linux")]
    {
        unit_path().is_ok_and(|path| path.exists())
    }
    #[cfg(target_os = "windows")]
    {
        schtasks_status(&["/Query", "/TN", WINDOWS_TASK_NAME])
            .is_ok_and(|status| exit_code_indicates_installed(status.code()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

fn effective_codex_home() -> Result<PathBuf> {
    crate::auth::codex_auth_path()?
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("Codex auth path has no parent directory"))
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

#[cfg(any(target_os = "windows", test))]
fn exit_code_indicates_installed(code: Option<i32>) -> bool {
    code == Some(0)
}

fn uninstall_may_continue(stop_succeeded: bool, daemon_running: bool) -> bool {
    stop_succeeded || !daemon_running
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
    user_println("Started Windows scheduled task");
    Ok(())
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
fn launchd_plist(exe: &str, home: &str, codex_home: &str) -> String {
    let exe = xml_escape(exe);
    let home = xml_escape(home);
    let codex_home = xml_escape(codex_home);
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
    </dict>
</dict>
</plist>"#,
        exe = exe,
        home = home,
        codex_home = codex_home,
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
    let plist = launchd_plist(&exe, &home, &codex_home);

    let path = plist_path()?;
    if path.exists() {
        user_println(&format!(
            "Warning: overwriting existing LaunchAgent at {}",
            path.display()
        ));
        // Unload the old service first to avoid launchctl conflicts
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &path.display().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist)?;

    load_launchd(&path)?;
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
    if !uninstall_may_continue(stopped, crate::daemon::pidfile::is_daemon_running()) {
        anyhow::bail!("launchctl unload failed while the daemon is still running");
    }
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
fn systemd_unit(exe: &str, home: &str, codex_home: &str) -> String {
    let exe = systemd_quote(exe);
    let home = systemd_quote(&format!("HOME={home}"));
    let codex_home = systemd_quote(&format!("CODEX_HOME={codex_home}"));
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

[Install]
WantedBy=default.target
"#,
        exe = exe,
        home = home,
        codex_home = codex_home,
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

    let unit = systemd_unit(&exe, &home, &codex_home);

    let path = unit_path()?;
    if path.exists() {
        user_println(&format!(
            "Warning: overwriting existing systemd service at {}",
            path.display()
        ));
        // Stop the old service first to avoid conflicts
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", SYSTEMD_UNIT_NAME])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, unit)?;

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", SYSTEMD_UNIT_NAME])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl enable failed");
    }
    user_println(&format!(
        "Installed systemd user service at {}",
        path.display()
    ));
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
    if !uninstall_may_continue(stopped, crate::daemon::pidfile::is_daemon_running()) {
        anyhow::bail!("systemctl disable --now failed while the daemon is still running");
    }
    std::fs::remove_file(&path)?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    user_println("Uninstalled systemd user service");
    Ok(())
}

// -- Windows Task Scheduler --

#[cfg(any(target_os = "windows", test))]
fn task_scheduler_command(exe: &Path, codex_home: &Path) -> String {
    format!(
        "cmd.exe /D /S /C \"\"set \"CODEX_HOME={}\" && \"{}\" daemon start --foreground\"\"",
        codex_home.display().to_string().replace('"', ""),
        exe.display().to_string().replace('"', "")
    )
}

#[cfg(target_os = "windows")]
fn schtasks_status(args: &[&str]) -> Result<std::process::ExitStatus> {
    Ok(std::process::Command::new("schtasks")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?)
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
    let exe = std::env::current_exe()?;
    let codex_home = effective_codex_home()?;
    let task_run = task_scheduler_command(&exe, &codex_home);

    schtasks(
        &[
            "/Create",
            "/TN",
            WINDOWS_TASK_NAME,
            "/TR",
            &task_run,
            "/SC",
            "ONLOGON",
            "/RL",
            "LIMITED",
            "/IT",
            "/F",
        ],
        "create scheduled task",
    )?;
    schtasks(&["/Run", "/TN", WINDOWS_TASK_NAME], "start scheduled task")?;
    user_println(&format!(
        "Installed Windows scheduled task {}",
        WINDOWS_TASK_NAME
    ));
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_task_scheduler() -> Result<()> {
    let stopped = schtasks(&["/End", "/TN", WINDOWS_TASK_NAME], "stop scheduled task").is_ok();
    if !uninstall_may_continue(stopped, crate::daemon::pidfile::is_daemon_running()) {
        anyhow::bail!("failed to stop scheduled task while the daemon is still running");
    }
    if !is_installed() {
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
        LAUNCHD_LABEL, SYSTEMD_UNIT_NAME, WINDOWS_TASK_NAME, exit_code_indicates_installed,
        launchd_plist, systemd_unit, task_scheduler_command, task_scheduler_failure_message,
        uninstall_may_continue,
    };
    use std::path::Path;

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
        );
        assert!(plist.contains("<string>/usr/local/bin/codex-switch-global-pace</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("<string>--foreground</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>CODEX_HOME</key>"));
        assert!(plist.contains("<string>/Users/alice/.codex</string>"));
    }

    #[test]
    fn launchd_plist_escapes_paths() {
        let plist = launchd_plist(
            "/Applications/A & B/codex-switch-global-pace",
            "/Users/a<b",
            "/Users/a&b/.codex",
        );
        assert!(plist.contains("/Applications/A &amp; B/codex-switch-global-pace"));
        assert!(plist.contains("/Users/a&lt;b"));
        assert!(plist.contains("/Users/a&amp;b/.codex"));
    }

    #[test]
    fn systemd_unit_runs_foreground_daemon() {
        let unit = systemd_unit(
            "/usr/local/bin/codex-switch-global-pace",
            "/home/alice",
            "/home/alice/.codex",
        );
        assert!(unit.contains(
            "ExecStart=\"/usr/local/bin/codex-switch-global-pace\" daemon start --foreground"
        ));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("Environment=\"HOME=/home/alice\""));
        assert!(unit.contains("Environment=\"CODEX_HOME=/home/alice/.codex\""));
    }

    #[test]
    fn systemd_unit_quotes_special_paths() {
        let unit = systemd_unit(
            r#"/opt/Codex & Tools\\codex-switch-global-pace"#,
            "/home/a & b",
            r#"/home/a & b/.codex\\custom"#,
        );
        assert!(unit.contains(
            r#"ExecStart="/opt/Codex & Tools\\\\codex-switch-global-pace" daemon start --foreground"#
        ));
        assert!(unit.contains(r#"Environment="HOME=/home/a & b""#));
        assert!(unit.contains(r#"Environment="CODEX_HOME=/home/a & b/.codex\\\\custom""#));
    }

    #[test]
    fn windows_task_scheduler_command_quotes_exe_path() {
        let cmd = task_scheduler_command(
            Path::new(r"C:\Program Files\codex-switch-global-pace.exe"),
            Path::new(r"C:\Users\A & B\.codex"),
        );
        assert_eq!(
            cmd,
            r#"cmd.exe /D /S /C ""set "CODEX_HOME=C:\Users\A & B\.codex" && "C:\Program Files\codex-switch-global-pace.exe" daemon start --foreground"""#
        );
    }

    #[test]
    fn windows_task_scheduler_create_error_includes_elevation_guidance() {
        assert_eq!(
            task_scheduler_failure_message("create scheduled task", "ERROR: Access is denied."),
            "failed to create scheduled task: ERROR: Access is denied. Re-run `codex-switch-global-pace daemon install` from an elevated PowerShell session."
        );
    }

    #[test]
    fn windows_task_exists_only_for_success_exit_code() {
        assert!(exit_code_indicates_installed(Some(0)));
        assert!(!exit_code_indicates_installed(Some(1)));
        assert!(!exit_code_indicates_installed(None));
    }

    #[test]
    fn uninstall_stops_when_service_command_failed_and_daemon_is_running() {
        assert!(!uninstall_may_continue(false, true));
        assert!(uninstall_may_continue(false, false));
        assert!(uninstall_may_continue(true, true));
    }
}
