/// Send a desktop notification. Best-effort, never fails.
pub fn send_notification(message: &str) {
    // Sanitize: keep only printable ASCII, no control chars or AppleScript metacharacters
    let safe: String = message
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(200)
        .collect();

    #[cfg(target_os = "macos")]
    {
        // Escape both backslashes and quotes for AppleScript string safety
        let escaped = safe.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "display notification \"{escaped}\" with title \"codex-switch-global-pace\""
                ),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["codex-switch-global-pace", &safe])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &windows_toast_script(&safe),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = safe;
    }
}

/// WinRT toast via PowerShell. Uses the well-known PowerShell AppUserModelID
/// so no app registration is needed; single quotes are doubled for the
/// single-quoted PowerShell string literals.
#[cfg(any(target_os = "windows", test))]
fn windows_toast_script(message: &str) -> String {
    let escaped = message.replace('\'', "''");
    format!(
        "$null = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]; \
         $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $x = $t.GetElementsByTagName('text'); \
         $null = $x.Item(0).AppendChild($t.CreateTextNode('codex-switch-global-pace')); \
         $null = $x.Item(1).AppendChild($t.CreateTextNode('{escaped}')); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($t))"
    )
}

#[cfg(test)]
mod tests {
    use super::windows_toast_script;

    #[test]
    fn toast_script_embeds_message_and_escapes_quotes() {
        let script = windows_toast_script("Switched to 'alice' (score: 87)");
        assert!(script.contains("CreateTextNode('Switched to ''alice'' (score: 87)')"));
        assert!(script.contains("CreateTextNode('codex-switch-global-pace')"));
        assert!(script.contains("WindowsPowerShell\\v1.0\\powershell.exe"));
    }
}
