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
    let Some(output) = list_process_command_lines() else {
        return CodexActivity::Unknown;
    };
    output.lines().fold(CodexActivity::Idle, |activity, line| {
        activity.merge(classify_codex_command(line))
    })
}

#[cfg(target_os = "linux")]
fn list_process_arguments() -> Option<Vec<Vec<String>>> {
    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_none_or(|name| name.parse::<u32>().is_err())
        {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        if !args.is_empty() {
            processes.push(args);
        }
    }
    Some(processes)
}

#[cfg(target_os = "macos")]
fn list_process_arguments() -> Option<Vec<Vec<String>>> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pids = String::from_utf8(output.stdout).ok()?;
    Some(
        pids.lines()
            .filter_map(|line| line.trim().parse::<libc::pid_t>().ok())
            .filter_map(macos_process_arguments)
            .collect(),
    )
}

#[cfg(target_os = "macos")]
fn macos_process_arguments(pid: libc::pid_t) -> Option<Vec<String>> {
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

    let mut buffer = vec![0_u8; usize::try_from(arg_max).ok()?];
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
        return None;
    }
    buffer.truncate(buffer_size);
    parse_macos_process_arguments(&buffer)
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
fn list_process_command_lines() -> Option<String> {
    // tasklist has no command lines; CIM does. Filter server-side to codex*.
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name LIKE 'codex%' OR Name = 'node.exe'\" | ForEach-Object { $_.CommandLine }",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
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
    const INFRA_SUBCOMMANDS: &[&str] = &["app-server", "completion", "mcp", "mcp-server"];
    const AUTH_SUBCOMMANDS: &[&str] = &["login", "logout"];

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
    use super::{CodexActivity, classify_codex_args, classify_codex_command};

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
