//! Detection of Codex processes that make replacing `auth.json` unsafe.
//!
//! The daemon must not swap `auth.json` under an active conversation or while
//! `codex login`/`codex logout` is changing that file. Long-lived Codex
//! infrastructure (MCP servers, `app-server` for desktop/IDE hosts, helper
//! binaries) shares the same binary but does not own the interactive auth
//! lifecycle, so those processes remain non-blocking.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexActivity {
    Idle,
    Session,
    AuthMutation,
    Unknown,
}

impl CodexActivity {
    fn merge(self, other: Self) -> Self {
        use CodexActivity::{AuthMutation, Idle, Session, Unknown};
        match (self, other) {
            (AuthMutation, _) | (_, AuthMutation) => AuthMutation,
            (Unknown, _) | (_, Unknown) => Unknown,
            (Session, _) | (_, Session) => Session,
            (Idle, Idle) => Idle,
        }
    }
}

const AUTH_SUBCOMMANDS: &[&str] = &["login", "logout"];
// Long-lived Codex processes that do not own the interactive auth lifecycle.
// Keep this compatibility contract aligned with the Codex CLI help surface.
const INFRA_SUBCOMMANDS: &[&str] = &[
    "app-server",
    "completion",
    "exec-server",
    "mcp",
    "mcp-server",
];

#[cfg(any(unix, test))]
#[derive(Debug, Eq, PartialEq)]
enum ProcessInspection {
    Irrelevant,
    Vanished,
    Arguments(Vec<String>),
    Failed,
}

/// One process returned by the Windows CIM query. Keeping the image name and
/// PID alongside the command line lets us distinguish an unrelated node.exe
/// whose command line is unavailable from a native Codex process that we were
/// unable to inspect safely.
#[cfg(any(windows, test))]
#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsProcess {
    name: String,
    process_id: u32,
    command_line: Option<String>,
}

#[cfg(any(windows, test))]
#[derive(Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsProcessSnapshot {
    processes: Vec<WindowsProcess>,
}

#[cfg(any(unix, test))]
fn collect_process_inspections(
    inspections: impl IntoIterator<Item = ProcessInspection>,
) -> Option<Vec<Vec<String>>> {
    let mut processes = Vec::new();
    for inspection in inspections {
        match inspection {
            ProcessInspection::Irrelevant | ProcessInspection::Vanished => {}
            ProcessInspection::Arguments(arguments) => processes.push(arguments),
            ProcessInspection::Failed => return None,
        }
    }
    Some(processes)
}

/// Classify the current Codex activity. Inspection failure is an explicit
/// state so callers can fail closed instead of replacing auth optimistically.
#[cfg(unix)]
pub fn codex_activity() -> CodexActivity {
    let Some(processes) = list_process_arguments() else {
        return CodexActivity::Unknown;
    };
    processes
        .iter()
        .fold(CodexActivity::Idle, |activity, args| {
            activity.merge(classify_codex_args(args))
        })
}

/// Classify the current Codex activity. Inspection failure is an explicit
/// state so callers can fail closed instead of replacing auth optimistically.
#[cfg(windows)]
pub fn codex_activity() -> CodexActivity {
    let Some(processes) = list_windows_processes() else {
        return CodexActivity::Unknown;
    };
    processes
        .iter()
        .fold(CodexActivity::Idle, |activity, process| {
            activity.merge(classify_windows_process(process))
        })
}

#[cfg(target_os = "linux")]
fn list_process_arguments() -> Option<Vec<Vec<String>>> {
    let mut inspections = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else {
            return None;
        };
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        inspections.push(linux_process_arguments(&entry.path()));
    }
    collect_process_inspections(inspections)
}

#[cfg(target_os = "linux")]
fn linux_process_arguments(process_path: &std::path::Path) -> ProcessInspection {
    use std::os::unix::fs::MetadataExt as _;

    let current_uid = unsafe { libc::geteuid() };
    match std::fs::metadata(process_path) {
        Ok(metadata) if metadata.uid() != current_uid => return ProcessInspection::Irrelevant,
        Ok(_) => {}
        Err(error) if process_vanished(&error) => return ProcessInspection::Vanished,
        Err(_) => return ProcessInspection::Failed,
    }

    let command_name = match std::fs::read_to_string(process_path.join("comm")) {
        Ok(command_name) => command_name,
        Err(error) => return linux_process_file_failure(process_path, &error),
    };
    if !possible_codex_process_name(command_name.trim()) {
        return ProcessInspection::Irrelevant;
    }

    let status = match std::fs::read_to_string(process_path.join("status")) {
        Ok(status) => status,
        Err(error) => return linux_process_file_failure(process_path, &error),
    };
    match linux_effective_uid(&status) {
        Some(uid) if uid == current_uid => {}
        Some(_) => return ProcessInspection::Irrelevant,
        None => return ProcessInspection::Failed,
    }

    let cmdline = match std::fs::read(process_path.join("cmdline")) {
        Ok(cmdline) => cmdline,
        Err(error) => return linux_process_file_failure(process_path, &error),
    };
    let arguments = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect::<Vec<_>>();
    if arguments.is_empty() {
        ProcessInspection::Irrelevant
    } else {
        ProcessInspection::Arguments(arguments)
    }
}

/// A missing `/proc/<pid>` file means the process vanished only when its PID
/// directory is gone too. A missing required file under a live entry is an
/// inspection failure and must remain fail-closed.
#[cfg(any(target_os = "linux", test))]
fn linux_process_file_failure(
    process_path: &std::path::Path,
    error: &std::io::Error,
) -> ProcessInspection {
    if !process_vanished(error) {
        return ProcessInspection::Failed;
    }

    match std::fs::metadata(process_path) {
        Err(root_error) if process_vanished(&root_error) => ProcessInspection::Vanished,
        Ok(_) | Err(_) => ProcessInspection::Failed,
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_effective_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(any(unix, test))]
fn process_vanished(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::NotFound {
        return true;
    }

    #[cfg(unix)]
    return error.raw_os_error() == Some(libc::ESRCH);

    #[cfg(not(unix))]
    false
}

#[cfg(target_os = "macos")]
fn list_process_arguments() -> Option<Vec<Vec<String>>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,uid=,comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let processes = String::from_utf8(output.stdout).ok()?;
    let current_uid = unsafe { libc::geteuid() };
    let mut candidate_pids = Vec::new();
    for line in processes.lines() {
        let (pid, uid, command) = parse_macos_process_row(line)?;
        if uid == current_uid && possible_codex_process_name(command) {
            candidate_pids.push(pid);
        }
    }
    if candidate_pids.is_empty() {
        return Some(Vec::new());
    }
    let arg_max = macos_arg_max()?;
    collect_process_inspections(
        candidate_pids
            .into_iter()
            .map(|pid| macos_process_arguments(pid, arg_max)),
    )
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_process_row(line: &str) -> Option<(i32, u32, &str)> {
    let line = line.trim_start();
    let pid_end = line.find(char::is_whitespace)?;
    let pid = line[..pid_end].parse().ok()?;

    let remainder = line[pid_end..].trim_start();
    let uid_end = remainder.find(char::is_whitespace)?;
    let uid = remainder[..uid_end].parse().ok()?;
    let command = remainder[uid_end..].trim_start();
    if command.is_empty() {
        return None;
    }

    Some((pid, uid, command))
}

#[cfg(target_os = "macos")]
fn macos_arg_max() -> Option<usize> {
    let mut arg_max = 0 as libc::c_int;
    let mut arg_max_size = std::mem::size_of_val(&arg_max);
    let mut arg_max_mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: all pointers reference writable values with the lengths passed to
    // sysctl; no new-value buffer is supplied.
    let status = unsafe {
        libc::sysctl(
            arg_max_mib.as_mut_ptr(),
            arg_max_mib.len() as libc::c_uint,
            std::ptr::from_mut(&mut arg_max).cast(),
            &mut arg_max_size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || arg_max <= 0 {
        return None;
    }

    usize::try_from(arg_max).ok()
}

#[cfg(target_os = "macos")]
fn macos_process_arguments(pid: libc::pid_t, arg_max: usize) -> ProcessInspection {
    let mut buffer = vec![0_u8; arg_max];
    let mut buffer_size = buffer.len();
    let mut process_mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: `buffer` is writable for `buffer_size` bytes and sysctl updates
    // that size to the number of initialized bytes.
    let status = unsafe {
        libc::sysctl(
            process_mib.as_mut_ptr(),
            process_mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut buffer_size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        return if process_vanished(&error) {
            ProcessInspection::Vanished
        } else {
            ProcessInspection::Failed
        };
    }
    buffer.truncate(buffer_size);
    classify_macos_process_arguments(parse_macos_process_arguments(&buffer))
}

/// Re-check the executable after the `ps` snapshot. The PID may have been
/// reused or the process may have executed a different image before sysctl
/// returned its arguments.
#[cfg(any(target_os = "macos", test))]
fn classify_macos_process_arguments(arguments: Option<Vec<String>>) -> ProcessInspection {
    let Some(arguments) = arguments else {
        return ProcessInspection::Failed;
    };
    let Some(executable) = arguments.first() else {
        return ProcessInspection::Failed;
    };
    if possible_codex_process_name(executable) {
        ProcessInspection::Arguments(arguments)
    } else {
        ProcessInspection::Irrelevant
    }
}

#[cfg(target_os = "macos")]
fn parse_macos_process_arguments(buffer: &[u8]) -> Option<Vec<String>> {
    let argc_size = std::mem::size_of::<libc::c_int>();
    let argc = libc::c_int::from_ne_bytes(buffer.get(..argc_size)?.try_into().ok()?);
    if argc <= 0 {
        return None;
    }

    let mut cursor = argc_size;
    cursor += buffer.get(cursor..)?.iter().position(|byte| *byte == 0)? + 1;
    while buffer.get(cursor).is_some_and(|byte| *byte == 0) {
        cursor += 1;
    }

    let mut args = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        let rest = buffer.get(cursor..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        cursor += end + 1;
    }
    Some(args)
}

#[cfg(windows)]
fn list_windows_processes() -> Option<Vec<WindowsProcess>> {
    // tasklist has no command lines; CIM does. Emit one JSON envelope so a
    // null command line cannot disappear as an empty output line and process
    // identity remains attached to every command line.
    const PROCESS_QUERY: &str = concat!(
        "$ErrorActionPreference = 'Stop'; ",
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); ",
        "$processes = @(Get-CimInstance Win32_Process ",
        "-Filter \"Name LIKE 'codex%' OR Name = 'node.exe'\" | ForEach-Object { ",
        "$commandLine = if ($null -eq $_.CommandLine) { $null } else { [string]$_.CommandLine }; ",
        "[pscustomobject]@{ Name = [string]$_.Name; ProcessId = [uint32]$_.ProcessId; ",
        "CommandLine = $commandLine } }); ",
        "[pscustomobject]@{ Processes = $processes } | ConvertTo-Json -Compress -Depth 3"
    );

    let output = std::process::Command::new("powershell")
        .args([
            "-NoLogo",
            "-NonInteractive",
            "-NoProfile",
            "-Command",
            PROCESS_QUERY,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_windows_process_snapshot(&output.stdout)
}

#[cfg(any(windows, test))]
fn parse_windows_process_snapshot(output: &[u8]) -> Option<Vec<WindowsProcess>> {
    serde_json::from_slice::<WindowsProcessSnapshot>(output)
        .ok()
        .map(|snapshot| snapshot.processes)
}

#[cfg(any(windows, test))]
fn classify_windows_process(process: &WindowsProcess) -> CodexActivity {
    match process
        .command_line
        .as_deref()
        .filter(|command_line| !command_line.trim().is_empty())
    {
        Some(command_line) => classify_codex_command(command_line),
        None if is_codex_binary_name(&process.name) => {
            tracing::debug!(
                pid = process.process_id,
                process_name = %process.name,
                "native Codex process command line unavailable; activity is unknown"
            );
            CodexActivity::Unknown
        }
        None => CodexActivity::Idle,
    }
}

/// Classify a Win32 command line returned by CIM.
#[cfg(any(windows, test))]
fn classify_codex_command(cmdline: &str) -> CodexActivity {
    let args = split_windows_command_line(cmdline);
    classify_codex_args(&args)
}

fn classify_codex_args<S: AsRef<str>>(args: &[S]) -> CodexActivity {
    let Some(first) = args.first() else {
        return CodexActivity::Idle;
    };
    let mut bin = basename(first.as_ref());
    let mut arg_index = 1;

    // npm shim: `node /path/to/bin/codex <args…>`
    if bin.eq_ignore_ascii_case("node") || bin.eq_ignore_ascii_case("node.exe") {
        let Some(shim_target) = args.get(arg_index) else {
            return CodexActivity::Idle;
        };
        bin = basename(shim_target.as_ref());
        arg_index += 1;
    }

    if !is_codex_binary_name(bin) {
        return CodexActivity::Idle;
    }

    classify_codex_invocation(&args[arg_index..])
}

fn classify_codex_invocation<S: AsRef<str>>(args: &[S]) -> CodexActivity {
    const VALUE_FLAGS: &[&str] = &[
        "--add-dir",
        "--ask-for-approval",
        "--cd",
        "--config",
        "--disable",
        "--enable",
        "--image",
        "--local-provider",
        "--model",
        "--profile",
        "--remote",
        "--remote-auth-token-env",
        "--sandbox",
        "-C",
        "-a",
        "-c",
        "-i",
        "-m",
        "-p",
        "-s",
    ];
    const BOOLEAN_FLAGS: &[&str] = &[
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-bypass-hook-trust",
        "--no-alt-screen",
        "--oss",
        "--search",
        "--strict-config",
    ];
    const SHORT_VALUE_FLAGS: &[&str] = &["-C", "-a", "-c", "-i", "-m", "-p", "-s"];
    let mut index = 0;
    loop {
        let Some(arg) = args.get(index).map(AsRef::as_ref) else {
            return CodexActivity::Session;
        };
        index += 1;

        if matches!(arg, "--help" | "--version" | "-h" | "-V") {
            return CodexActivity::Idle;
        }
        if arg == "--" {
            return CodexActivity::Session;
        }
        if BOOLEAN_FLAGS.contains(&arg) {
            continue;
        }
        if VALUE_FLAGS.contains(&arg) {
            if args.get(index).is_none() {
                return CodexActivity::Session;
            }
            index += 1;
            continue;
        }
        if arg
            .split_once('=')
            .is_some_and(|(flag, _)| VALUE_FLAGS.contains(&flag))
        {
            continue;
        }
        if SHORT_VALUE_FLAGS
            .iter()
            .any(|flag| arg.len() > flag.len() && arg.starts_with(flag))
        {
            continue;
        }
        if arg.starts_with('-') {
            return CodexActivity::Session;
        }
        if AUTH_SUBCOMMANDS.contains(&arg) {
            return CodexActivity::AuthMutation;
        }
        if INFRA_SUBCOMMANDS.contains(&arg) {
            return CodexActivity::Idle;
        }
        return CodexActivity::Session;
    }
}

/// Parse the Win32 command line returned by CIM while preserving escaped
/// quotes inside arguments. Backslashes only escape a quote when immediately
/// followed by it; pairs collapse according to `CommandLineToArgvW` rules.
#[cfg(any(windows, test))]
fn split_windows_command_line(cmdline: &str) -> Vec<String> {
    let chars: Vec<char> = cmdline.chars().collect();
    let mut args = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        while chars.get(index).is_some_and(|ch| matches!(ch, ' ' | '\t')) {
            index += 1;
        }
        if index == chars.len() {
            break;
        }

        let mut arg = String::new();
        let mut in_quotes = false;
        loop {
            if index == chars.len()
                || (!in_quotes && chars.get(index).is_some_and(|ch| matches!(ch, ' ' | '\t')))
            {
                break;
            }

            let mut backslashes = 0;
            while chars.get(index) == Some(&'\\') {
                backslashes += 1;
                index += 1;
            }

            if chars.get(index) == Some(&'"') {
                for _ in 0..(backslashes / 2) {
                    arg.push('\\');
                }
                if backslashes % 2 == 1 {
                    arg.push('"');
                    index += 1;
                } else if in_quotes && chars.get(index + 1) == Some(&'"') {
                    arg.push('"');
                    index += 2;
                } else {
                    in_quotes = !in_quotes;
                    index += 1;
                }
                continue;
            }

            for _ in 0..backslashes {
                arg.push('\\');
            }
            let Some(ch) = chars.get(index) else {
                break;
            };
            arg.push(*ch);
            index += 1;
        }
        args.push(arg);
    }
    args
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(any(unix, test))]
fn possible_codex_process_name(name: &str) -> bool {
    let base = basename(name);
    base.eq_ignore_ascii_case("node")
        || base.eq_ignore_ascii_case("node.exe")
        || is_codex_binary_name(base)
}

/// Match the Codex CLI by binary name. Accepts plain `codex`, platform
/// binaries like `codex-aarch64-apple-darwin` (npm vendor binary; Linux comm
/// truncation is irrelevant here since we read full command lines), and
/// Windows `codex.exe` — but never our own `codex-switch` or Codex helper
/// binaries like `codex-code-mode-host`.
fn is_codex_binary_name(base: &str) -> bool {
    let mut base = base.trim_matches('"').to_ascii_lowercase();
    if base.ends_with(".exe") {
        base.truncate(base.len() - ".exe".len());
    }
    base == "codex"
        || (base.starts_with("codex-")
            && !base.starts_with("codex-switch")
            && !base.starts_with("codex-code-mode"))
}

#[cfg(test)]
mod tests {
    use super::{
        CodexActivity, ProcessInspection, classify_codex_args, classify_codex_command,
        classify_windows_process, collect_process_inspections, linux_effective_uid,
        parse_macos_process_row, parse_windows_process_snapshot, possible_codex_process_name,
    };

    #[test]
    fn vanished_candidates_are_ignored_but_inspection_failures_fail_closed() {
        assert_eq!(
            collect_process_inspections([
                ProcessInspection::Vanished,
                ProcessInspection::Irrelevant,
                ProcessInspection::Arguments(vec!["codex".to_string(), "login".to_string()]),
            ]),
            Some(vec![vec!["codex".to_string(), "login".to_string()]])
        );
        assert_eq!(
            collect_process_inspections([ProcessInspection::Vanished, ProcessInspection::Failed,]),
            None
        );
    }

    #[test]
    fn candidate_prefilter_covers_native_and_node_codex_processes() {
        assert!(possible_codex_process_name("codex"));
        assert!(possible_codex_process_name("codex-aarch64-apple-darwin"));
        assert!(possible_codex_process_name("node"));
        assert!(!possible_codex_process_name("codex-switch-global-pace"));
        assert!(!possible_codex_process_name("codex-code-mode-host"));
        assert!(!possible_codex_process_name("bash"));
    }

    #[test]
    fn windows_snapshot_preserves_process_identity_and_nullable_command_line() {
        let processes = parse_windows_process_snapshot(
            br#"{"Processes":[{"Name":"codex.exe","ProcessId":41,"CommandLine":null},{"Name":"node.exe","ProcessId":42,"CommandLine":"node.exe codex app-server"}]}"#,
        )
        .expect("valid CIM JSON snapshot");

        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].name, "codex.exe");
        assert_eq!(processes[0].process_id, 41);
        assert_eq!(processes[0].command_line, None);
        assert_eq!(processes[1].name, "node.exe");
        assert_eq!(processes[1].process_id, 42);
        assert_eq!(
            processes[1].command_line.as_deref(),
            Some("node.exe codex app-server")
        );
        assert!(parse_windows_process_snapshot(b"not JSON").is_none());
    }

    #[test]
    fn unavailable_native_windows_codex_command_line_fails_closed() {
        let native = super::WindowsProcess {
            name: "codex.exe".to_string(),
            process_id: 100,
            command_line: None,
        };
        assert_eq!(classify_windows_process(&native), CodexActivity::Unknown);

        let vendor_native = super::WindowsProcess {
            name: "codex-x86_64-pc-windows-msvc.exe".to_string(),
            process_id: 101,
            command_line: Some("  ".to_string()),
        };
        assert_eq!(
            classify_windows_process(&vendor_native),
            CodexActivity::Unknown
        );
    }

    #[test]
    fn unavailable_non_codex_windows_command_line_is_irrelevant() {
        for name in ["node.exe", "codex-switch.exe", "codex-code-mode-host.exe"] {
            let process = super::WindowsProcess {
                name: name.to_string(),
                process_id: 200,
                command_line: None,
            };
            assert_eq!(
                classify_windows_process(&process),
                CodexActivity::Idle,
                "{name}"
            );
        }
    }

    #[test]
    fn macos_process_row_preserves_spaces_in_the_command_path() {
        let (pid, uid, command) = parse_macos_process_row(
            "  123   501 /Applications/Codex Preview.app/Contents/MacOS/codex",
        )
        .unwrap();
        assert_eq!(pid, 123);
        assert_eq!(uid, 501);
        assert_eq!(
            command,
            "/Applications/Codex Preview.app/Contents/MacOS/codex"
        );
        assert!(possible_codex_process_name(command));
        assert_eq!(parse_macos_process_row("123 501   "), None);
    }

    #[test]
    fn macos_arguments_recheck_snapshot_candidates_after_pid_reuse() {
        assert_eq!(
            super::classify_macos_process_arguments(Some(vec![
                "/bin/bash".to_string(),
                "worker.sh".to_string(),
            ])),
            ProcessInspection::Irrelevant
        );
        assert_eq!(
            super::classify_macos_process_arguments(Some(vec![
                "/usr/local/bin/node".to_string(),
                "/usr/local/bin/codex".to_string(),
            ])),
            ProcessInspection::Arguments(vec![
                "/usr/local/bin/node".to_string(),
                "/usr/local/bin/codex".to_string(),
            ])
        );
        assert_eq!(
            super::classify_macos_process_arguments(Some(Vec::new())),
            ProcessInspection::Failed
        );
        assert_eq!(
            super::classify_macos_process_arguments(None),
            ProcessInspection::Failed
        );
    }

    #[test]
    fn linux_status_parser_reads_the_effective_uid() {
        assert_eq!(
            linux_effective_uid("Name:\tcodex\nUid:\t1000\t1001\t1002\t1003\n"),
            Some(1001)
        );
        assert_eq!(linux_effective_uid("Name:\tcodex\n"), None);
    }

    #[test]
    fn linux_missing_process_file_only_vanishes_with_its_process_directory() {
        let existing_process = tempfile::tempdir().unwrap();
        let missing_file = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            super::linux_process_file_failure(existing_process.path(), &missing_file),
            ProcessInspection::Failed
        );

        let vanished_process = existing_process.path().join("already-gone");
        assert_eq!(
            super::linux_process_file_failure(&vanished_process, &missing_file),
            ProcessInspection::Vanished
        );

        let unreadable_file = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            super::linux_process_file_failure(&vanished_process, &unreadable_file),
            ProcessInspection::Failed
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_only_relevant_same_user_inspection_failures_fail_closed() {
        let irrelevant = tempfile::tempdir().unwrap();
        std::fs::write(irrelevant.path().join("comm"), "bash\n").unwrap();
        assert_eq!(
            super::linux_process_arguments(irrelevant.path()),
            ProcessInspection::Irrelevant
        );

        let candidate = tempfile::tempdir().unwrap();
        std::fs::write(candidate.path().join("comm"), "codex\n").unwrap();
        assert_eq!(
            super::linux_process_arguments(candidate.path()),
            ProcessInspection::Failed
        );

        let vanished = candidate.path().join("already-gone");
        assert_eq!(
            super::linux_process_arguments(&vanished),
            ProcessInspection::Vanished
        );
    }

    #[test]
    fn interactive_sessions_are_detected() {
        // Real-world command lines observed on macOS (npm vendor binary + shim).
        assert_eq!(
            classify_codex_command(
                "/Users/u/.nvm/versions/node/v24.14.0/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex"
            ),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command(
                "/usr/local/bin/codex resume 019f400c-35d3-77c3-96b0-85d3ff6b75fe"
            ),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command("codex exec \"fix the tests\""),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command("node /Users/u/.nvm/versions/node/v24.14.0/bin/codex"),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command("\"C:\\Program Files\\codex\\codex.exe\" resume abc"),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command("\"C:\\Program Files\\codex\\Codex.EXE\" resume abc"),
            CodexActivity::Session
        );
    }

    #[test]
    fn interactive_prompt_words_are_not_mistaken_for_infrastructure() {
        assert_eq!(
            classify_codex_command(
                "codex exec \"fix login logout completion mcp mcp-server app-server --help --version\""
            ),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command(
                "codex -m gpt-5.4 -c model_reasoning_effort=high exec \"fix mcp-server\""
            ),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command("codex resume abc \"explain app-server\""),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command("codex -- \"login to the service\""),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_command(
                r#"codex.exe -c "developer_instructions=\"login mode\"" exec "do work""#
            ),
            CodexActivity::Session
        );
    }

    #[test]
    fn argv_boundaries_distinguish_bare_prompts_from_infrastructure() {
        assert_eq!(
            classify_codex_args(&["codex", "login to the service"]),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_args(&["codex", "app-server question"]),
            CodexActivity::Session
        );
        assert_eq!(
            classify_codex_args(&["codex", "login", "--device"]),
            CodexActivity::AuthMutation
        );
        assert_eq!(
            classify_codex_args(&["codex", "logout"]),
            CodexActivity::AuthMutation
        );
        assert_eq!(
            classify_codex_args(&["codex", "app-server"]),
            CodexActivity::Idle
        );
    }

    #[test]
    fn codex_infrastructure_does_not_block_switching() {
        assert_eq!(
            classify_codex_command(
                "/Users/u/.nvm/.../bin/codex -m gpt-5.4 -c model_reasoning_effort=high mcp-server"
            ),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command(
                "node /Users/u/.nvm/versions/node/v24.14.0/bin/codex -m gpt-5.4 mcp-server"
            ),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command("codex -mfoo mcp-server"),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command("codex -C/tmp app-server"),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command(
                "/Applications/ChatGPT.app/Contents/Resources/codex -c features.code_mode_host=true app-server --analytics-default-enabled"
            ),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command("/usr/local/bin/codex app-server"),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command("/usr/local/bin/codex exec-server --listen ws://127.0.0.1:0"),
            CodexActivity::Idle
        );
        assert_eq!(classify_codex_command("codex --help"), CodexActivity::Idle);
        assert_eq!(classify_codex_command("codex -V"), CodexActivity::Idle);
        assert_eq!(
            classify_codex_command("/path/bin/codex-code-mode-host"),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command(
                "/Users/u/Developer/Repos/codex-switch/target/debug/codex-switch daemon start --foreground"
            ),
            CodexActivity::Idle
        );
        assert_eq!(
            classify_codex_command(
                "/Applications/Pencil.app/Contents/Resources/app.asar.unpacked/out/mcp-server-darwin-arm64 --app desktop --agent codexCLI"
            ),
            CodexActivity::Idle
        );
        assert_eq!(classify_codex_command("bash"), CodexActivity::Idle);
        assert_eq!(classify_codex_command(""), CodexActivity::Idle);
    }

    #[test]
    fn auth_mutation_and_unknown_are_fail_closed_priorities() {
        assert_eq!(
            CodexActivity::Idle.merge(CodexActivity::Session),
            CodexActivity::Session
        );
        assert_eq!(
            CodexActivity::Session.merge(CodexActivity::AuthMutation),
            CodexActivity::AuthMutation
        );
        assert_eq!(
            CodexActivity::Session.merge(CodexActivity::Unknown),
            CodexActivity::Unknown
        );
    }
}
