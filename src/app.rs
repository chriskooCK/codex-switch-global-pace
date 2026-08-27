use crate::{
    auth,
    cli::{self, Cli, Commands},
    color, commands, config, daemon, installer_fs, logging,
    output::{self, MessageMode, print_error, user_println},
    profile, safe_text, tui, update,
};
use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

const PENDING_SELF_UPDATE_CLEANUP_WARNING_MAX_CHARS: usize = 1024;

#[derive(Debug)]
pub(crate) struct OutputAlreadyReported;

impl std::fmt::Display for OutputAlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("command failed; details were already reported")
    }
}

impl std::error::Error for OutputAlreadyReported {}

fn exit_with_plain_error(error: &anyhow::Error) -> ! {
    eprintln!("Error: {}", safe_text::terminal_text(&format!("{error:#}")));
    std::process::exit(1);
}

fn installer_owner_check_request(command: &Option<Commands>) -> Option<Option<std::path::PathBuf>> {
    match command {
        Some(Commands::Daemon(cli::DaemonCommand::Uninstall {
            expected_executable,
            check_owner: true,
        })) => Some(expected_executable.clone()),
        _ => None,
    }
}

fn should_recover_self_update_cleanup_before_dispatch(command: &Option<Commands>) -> bool {
    command.is_some() && !matches!(command, Some(Commands::SelfUpdate { check: false, .. }))
}

fn should_report_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<OutputAlreadyReported>().is_none()
}

/// Post-command re-sync is best-effort so the command's result remains intact,
/// but every failure is surfaced. Silent I/O, identity, or freshness failures
/// can otherwise leave a newly-rotated profile credential out of sync with the
/// live Codex credential without giving the user a way to diagnose it.
fn format_post_command_sync_warning(error: &anyhow::Error) -> String {
    format!("Warning: post-command profile sync did not fully complete: {error:#}")
}

pub(crate) fn format_pending_self_update_cleanup_warning(error: &impl std::fmt::Display) -> String {
    let detail = safe_text::bounded_terminal_text(
        &error.to_string(),
        PENDING_SELF_UPDATE_CLEANUP_WARNING_MAX_CHARS,
    );
    format!(
        "Warning: previous executable cleanup remains pending and will be retried; \
         this command will continue, but another self-update is blocked until exact cleanup succeeds: {detail}"
    )
}

fn preserve_command_result_after_sync<T>(
    command_result: Result<T>,
    sync_result: Result<()>,
    mut report_sync_error: impl FnMut(&anyhow::Error),
) -> Result<T> {
    if let Err(error) = &sync_result {
        report_sync_error(error);
    }
    command_result
}

fn confirmed_sync_marker(alias: &str) -> Result<profile::CurrentMarkerSnapshot> {
    let marker = profile::read_current_marker_snapshot_checked()?.ok_or_else(|| {
        anyhow::anyhow!(
            "current profile marker disappeared after profile '{alias}' was synchronized"
        )
    })?;
    if marker.alias() != alias {
        anyhow::bail!(
            "current profile marker changed from synchronized profile '{alias}' to '{}'",
            marker.alias()
        );
    }
    Ok(marker)
}

fn resync_profile_after_command(expected_marker: &profile::CurrentMarkerSnapshot) -> Result<()> {
    profile::ensure_current_marker_unchanged(expected_marker)?;
    let live_path = auth::codex_auth_path()?;
    match profile::find_matching_profile_checked(&live_path)? {
        Some(actual) if actual == expected_marker.alias() => {
            profile::ensure_current_marker_unchanged(expected_marker)?;
        }
        Some(actual) => {
            anyhow::bail!(
                "live auth now belongs to profile '{actual}' instead of the startup-synchronized profile '{}'; no profile was guessed or overwritten",
                expected_marker.alias()
            );
        }
        None => {
            profile::update_profile_from_live_if_current_marker(
                expected_marker.alias(),
                expected_marker,
            )?;
        }
    }
    Ok(())
}

/// Build the confirmation prompt for syncing live `auth.json` credentials back into a
/// profile, showing both timestamps so the user can tell a normal "codex just logged in"
/// sync from a direction that looks wrong before hitting enter (default remains Yes).
fn format_resync_confirm_prompt(
    alias: &str,
    live_last_refresh: Option<&str>,
    profile_last_refresh: Option<&str>,
) -> String {
    let live_ts = safe_text::terminal_text(live_last_refresh.unwrap_or("unknown"));
    let profile_ts = safe_text::terminal_text(profile_last_refresh.unwrap_or("unknown"));
    format!(
        "Update profile '{alias}' with live credentials? (live last_refresh={live_ts} -> profile last_refresh={profile_ts}) [Y/n] "
    )
}

/// Best-effort read of the `last_refresh` field from an auth.json at `path`.
fn read_last_refresh(path: Result<std::path::PathBuf>) -> Option<String> {
    let val = auth::read_auth(&path.ok()?).ok()?;
    val.get("last_refresh")?.as_str().map(str::to_string)
}

fn read_live_account_label() -> Result<String> {
    let path = auth::codex_auth_path().context("resolving the live auth path for display")?;
    let value = auth::read_auth(&path).with_context(|| {
        format!(
            "reading live auth account information at {}",
            path.display()
        )
    })?;
    let email = profile::extract_identity(&value)
        .email
        .unwrap_or_else(|| "unknown".to_string());
    Ok(safe_text::terminal_text(&email).into_owned())
}

pub async fn run() {
    let cli = Cli::parse();

    // The installer uses this internal command solely as an OS-lock holder. It
    // must not initialize application configuration, inspect authentication, or
    // create logs while an installation transaction is waiting for its turn.
    if matches!(&cli.command, Some(Commands::HoldUpdateLock)) {
        if let Err(error) = update::hold_update_lock_from_env() {
            exit_with_plain_error(&error);
        }
        return;
    }

    // Keep the direct installer's daemon lifecycle authority in one
    // process from the pre-replacement stop through an explicit commit or
    // rollback acknowledgement. Protocol markers are the only stdout output.
    if let Some(Commands::HoldDaemonUpdateBoundary {
        initial_executable,
        replacement_executable,
        expected_executable_token,
        ready_nonce,
    }) = &cli.command
    {
        output::set_message_mode(MessageMode::Silent);
        let result = daemon::hold_installer_daemon_update_boundary(
            initial_executable.clone(),
            replacement_executable.clone(),
            expected_executable_token.as_deref(),
            ready_nonce.as_deref(),
        );
        if let Err(error) = result {
            exit_with_plain_error(&error);
        }
        return;
    }

    // The release-verified direct installer uses this hidden boundary for one
    // explicit file at a time. Keep it ahead of all application initialization.
    if let Some(Commands::InstallerFileOp {
        operation,
        source,
        destination,
        displaced,
        expected_token,
        expected_destination_token,
    }) = &cli.command
    {
        match installer_fs::execute(
            *operation,
            source.as_deref(),
            destination.as_deref(),
            displaced.as_deref(),
            expected_token.as_deref(),
            expected_destination_token.as_deref(),
        ) {
            Ok(result) => println!("{result}"),
            Err(error) => {
                exit_with_plain_error(&error);
            }
        }
        return;
    }

    // A committed Windows updater cannot delete its own mapped old image.
    // This dedicated worker opens the exact parent process and validates both
    // file tokens before emitting its only stdout marker. Keep the protocol
    // ahead of configuration, authentication, and logging initialization.
    if let Some(Commands::CleanupSelfUpdate {
        parent_pid,
        displaced,
        expected_token,
        expected_executable_token,
        journal,
        expected_journal_token,
        ready_nonce,
    }) = &cli.command
    {
        output::set_message_mode(MessageMode::Silent);
        if let Err(error) = update::run_self_update_cleanup_worker(
            *parent_pid,
            displaced,
            expected_token,
            expected_executable_token,
            journal,
            expected_journal_token,
            ready_nonce,
        ) {
            exit_with_plain_error(&error);
        }
        return;
    }

    // The release-verified installer uses this as a read-only ownership
    // precondition. Keep it ahead of config/auth/log initialization so success
    // has no observable side effect and failure is stderr-only.
    if let Some(expected_executable) = installer_owner_check_request(&cli.command) {
        if let Err(error) = daemon::check_uninstall_owner(expected_executable) {
            exit_with_plain_error(&error);
        }
        return;
    }

    // The direct installers require one strict state tuple without config
    // warnings, logging initialization, or other command output. Ownership and
    // manager-state errors remain fatal and are written only to stderr.
    if matches!(
        &cli.command,
        Some(Commands::Daemon(cli::DaemonCommand::Status {
            installer_state: true
        }))
    ) {
        if let Err(error) = daemon::print_installer_state(cli.json || cli.json_pretty) {
            exit_with_plain_error(&error);
        }
        return;
    }

    let use_json = cli.json || cli.json_pretty;
    let is_bare_tui = cli.command.is_none();
    let message_mode = if cli.command.is_none() {
        MessageMode::Silent
    } else if use_json {
        MessageMode::Stderr
    } else {
        MessageMode::Stdout
    };

    color::init(cli.color);
    output::set_json_pretty(cli.json_pretty);
    output::set_message_mode(message_mode);
    if let Err(e) = config::init() {
        if use_json {
            print_error(&e.to_string());
        } else {
            eprintln!("{}", color::error(&format!("Error: {e}")));
        }
        std::process::exit(1);
    }

    // A corrupt or temporarily locked cleanup journal must not brick normal
    // commands or the TUI. Surface a bounded warning after output mode is
    // initialized. A mutating self-update skips this best-effort pass because
    // its command boundary performs the same recovery as a hard precondition.
    if should_recover_self_update_cleanup_before_dispatch(&cli.command)
        && let Err(error) = update::recover_pending_self_update_cleanup_on_startup()
    {
        eprintln!(
            "{}",
            color::warn(&format_pending_self_update_cleanup_warning(&error))
        );
    }

    // Priority: --debug flag > RUST_LOG env > config.toml daemon.log_level > default "error"
    let filter = if cli.debug {
        EnvFilter::new(logging::application_filter("debug"))
    } else if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::from_default_env()
    } else if matches!(&cli.command, Some(Commands::Daemon(_))) {
        let level = config::daemon_log_level().unwrap_or_else(|error| {
            eprintln!(
                "{}",
                color::error(&format!("Error: failed to read daemon log level: {error}"))
            );
            std::process::exit(1);
        });
        EnvFilter::new(logging::application_filter(&level))
    } else {
        EnvFilter::new(logging::application_filter("error"))
    };
    // Keep diagnostic logs even when the daemon detaches and discards stdio.
    // The bare TUI keeps its pre-frame deferral; other commands remain lazy
    // unless their own lifecycle requires an explicit readiness boundary.
    let file_writer = if is_bare_tui {
        logging::deferred_file_log_writer()
    } else {
        logging::file_log_writer()
    };
    let command_file_writer = file_writer.clone();
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr.and(file_writer))
        .init();
    for warning in config::startup_warnings() {
        eprintln!("{}", color::warn(&format!("Warning: {warning}")));
    }
    if let Some(proxy) = cli.proxy.clone()
        && let Err(error) = config::set_cli_proxy(proxy)
    {
        if use_json {
            print_error(&error.to_string());
        } else {
            eprintln!("{}", color::error(&format!("Error: {error}")));
        }
        std::process::exit(1);
    }

    let result = dispatch(cli.command, use_json, command_file_writer).await;

    if let Err(e) = result {
        if should_report_error(&e) {
            tracing::error!(error = %format!("{e:#}"), "command failed");
            if use_json {
                print_error(&format!("{e:#}"));
            } else {
                eprintln!("{}", color::error(&format!("Error: {e:#}")));
            }
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod error_reporting_tests {
    use super::{
        Cli, Commands, OutputAlreadyReported, PENDING_SELF_UPDATE_CLEANUP_WARNING_MAX_CHARS,
        format_pending_self_update_cleanup_warning, installer_owner_check_request,
        should_check_auth_change, should_recover_self_update_cleanup_before_dispatch,
        should_report_error,
    };
    use clap::Parser;

    #[test]
    fn already_reported_errors_are_not_printed_or_logged_again() {
        assert!(!should_report_error(&OutputAlreadyReported.into()));
        assert!(should_report_error(&anyhow::anyhow!("new failure")));
    }

    #[test]
    fn pending_cleanup_warning_is_control_free_and_bounded() {
        let error = anyhow::anyhow!(
            "locked journal\u{1b}]52;c;clipboard\u{7}\n{}",
            "x".repeat(PENDING_SELF_UPDATE_CLEANUP_WARNING_MAX_CHARS + 100)
        );
        let warning = format_pending_self_update_cleanup_warning(&error);

        assert!(warning.starts_with("Warning: previous executable cleanup remains pending"));
        assert!(warning.chars().all(|character| !character.is_control()));
        assert!(
            warning.chars().count()
                <= PENDING_SELF_UPDATE_CLEANUP_WARNING_MAX_CHARS
                    + "Warning: previous executable cleanup remains pending and will be retried; this command will continue, but another self-update is blocked until exact cleanup succeeds: ".chars().count()
        );
    }

    #[test]
    fn a_bare_executable_defaults_to_tui() {
        let cli = Cli::try_parse_from(["codex-switch-global-pace"])
            .expect("bare executable should parse without a subcommand");
        assert!(cli.command.is_none());
    }

    #[test]
    fn only_bare_tui_defers_normal_pending_cleanup_recovery() {
        assert!(!should_recover_self_update_cleanup_before_dispatch(&None));
        assert!(should_recover_self_update_cleanup_before_dispatch(&Some(
            Commands::List { force: false }
        )));
        assert!(!should_recover_self_update_cleanup_before_dispatch(&Some(
            Commands::SelfUpdate {
                check: false,
                version: None,
                dev: false,
                stable: false,
            }
        )));
        assert!(should_recover_self_update_cleanup_before_dispatch(&Some(
            Commands::SelfUpdate {
                check: true,
                version: None,
                dev: false,
                stable: false,
            }
        )));
    }

    #[test]
    fn tui_skips_plain_terminal_auth_change_prompt() {
        assert!(!should_check_auth_change(&None, false));
        assert!(should_check_auth_change(
            &Some(Commands::List { force: false }),
            false
        ));
        assert!(!should_check_auth_change(&Some(Commands::Open), false));
        assert!(!should_check_auth_change(
            &Some(Commands::List { force: false }),
            true
        ));
    }

    #[test]
    fn removed_tui_command_is_rejected() {
        let error = Cli::try_parse_from(["codex-switch-global-pace", "tui"])
            .err()
            .expect("tui must no longer be accepted as a subcommand");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn removed_launch_command_is_rejected() {
        let error = Cli::try_parse_from(["codex-switch-global-pace", "launch"])
            .err()
            .expect("launch must no longer be accepted as a subcommand");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn installer_owner_check_is_selected_before_normal_dispatch() {
        let expected = if cfg!(windows) {
            r"C:\Program Files\codex-switch-global-pace.exe"
        } else {
            "/opt/codex-switch-global-pace"
        };
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "daemon",
            "uninstall",
            "--expected-executable",
            expected,
            "--check-owner",
        ])
        .unwrap();
        assert_eq!(
            installer_owner_check_request(&cli.command),
            Some(Some(std::path::PathBuf::from(expected)))
        );

        let actual_uninstall = Cli::try_parse_from([
            "codex-switch-global-pace",
            "daemon",
            "uninstall",
            "--expected-executable",
            expected,
        ])
        .unwrap();
        assert_eq!(
            installer_owner_check_request(&actual_uninstall.command),
            None
        );
    }
}

#[cfg(test)]
mod resync_reporting_tests {
    use super::{
        Commands, dispatch, format_post_command_sync_warning, format_resync_confirm_prompt,
        preserve_command_result_after_sync,
    };

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn every_resync_failure_is_reported_without_changing_the_command_result() {
        let error = anyhow::anyhow!("authenticated account does not match profile 'acme'");
        let warning = format_post_command_sync_warning(&error);
        assert!(warning.contains("Warning: post-command profile sync did not fully complete"));
        assert!(warning.contains("authenticated account does not match"));
    }

    #[test]
    fn command_errors_remain_primary_after_a_post_sync_error() {
        let command_error = anyhow::anyhow!("command failed after a credential rotation");
        let sync_error = anyhow::anyhow!("current marker changed");
        let mut reported = None;

        let result: anyhow::Result<()> =
            preserve_command_result_after_sync(Err(command_error), Err(sync_error), |error| {
                reported = Some(error.to_string())
            });

        assert_eq!(
            result.unwrap_err().to_string(),
            "command failed after a credential rotation"
        );
        assert_eq!(reported.as_deref(), Some("current marker changed"));

        let mut success_warning = None;
        let success = preserve_command_result_after_sync(
            Ok(7),
            Err(anyhow::anyhow!("marker disappeared")),
            |error| success_warning = Some(error.to_string()),
        )
        .unwrap();
        assert_eq!(success, 7);
        assert_eq!(success_warning.as_deref(), Some("marker disappeared"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn dispatch_fails_before_running_a_command_when_auth_detection_errors() {
        crate::config::init_defaults_for_tests();
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _codex_home = EnvVarGuard::set("CODEX_HOME", "relative-auth-home");
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let log_dir = root.path().join("logs");
        let file_log_writer = crate::logging::FileLogWriter::lazy_for_directory(log_dir.clone());

        let error = dispatch(
            Some(Commands::List { force: false }),
            false,
            file_log_writer,
        )
        .await
        .expect_err("a live-auth path error must stop command dispatch");
        let detail = format!("{error:#}");
        assert!(detail.contains("checking live auth changes"), "{detail}");
        assert!(
            detail.contains("CODEX_HOME must be an absolute path"),
            "{detail}"
        );
        assert!(
            !log_dir.exists(),
            "an ordinary no-log command failure must not initialize file logging"
        );
    }

    #[test]
    fn confirm_prompt_shows_direction_and_both_timestamps() {
        let prompt = format_resync_confirm_prompt(
            "acme",
            Some("2026-07-20T00:00:00Z"),
            Some("2026-07-10T00:00:00Z"),
        );
        assert!(prompt.contains("acme"));
        assert!(prompt.contains("live"));
        assert!(prompt.contains("2026-07-20T00:00:00Z"));
        assert!(prompt.contains("profile"));
        assert!(prompt.contains("2026-07-10T00:00:00Z"));
        assert!(prompt.contains("[Y/n]"));
    }

    #[test]
    fn confirm_prompt_falls_back_to_unknown_when_timestamp_missing() {
        let prompt = format_resync_confirm_prompt("acme", None, None);
        assert!(prompt.contains("unknown"));
    }

    #[test]
    fn confirm_prompt_removes_terminal_controls_from_auth_timestamps() {
        let prompt = format_resync_confirm_prompt(
            "acme",
            Some("2026-08-25T00:00:00Z\u{1b}]52;c;clipboard\u{7}"),
            Some("bad\nvalue"),
        );

        assert!(prompt.contains("2026-08-25T00:00:00Z]52;c;clipboard"));
        assert!(prompt.contains("badvalue"));
        assert!(!prompt.chars().any(char::is_control), "{prompt:?}");
    }
}

fn should_check_auth_change(cmd: &Option<Commands>, json: bool) -> bool {
    !json
        && cmd.is_some()
        && !matches!(
            cmd,
            Some(Commands::Login { .. })
                | Some(Commands::Import { .. })
                | Some(Commands::SelfUpdate { .. })
                | Some(Commands::Open)
                | Some(Commands::Daemon(_))
        )
}

async fn dispatch(
    cmd: Option<Commands>,
    json: bool,
    file_log_writer: logging::FileLogWriter,
) -> Result<()> {
    // The TUI reconciles live credentials after its first render so startup can
    // never be held behind a plain-terminal prompt. CLI commands retain their
    // existing interactive synchronization boundary.
    let auth_preflight_performed = should_check_auth_change(&cmd, json);
    let auth_check = if auth_preflight_performed {
        check_auth_change()?
    } else {
        AuthCheckResult::NoChange
    };

    let command_result = match cmd {
        Some(Commands::HoldUpdateLock) => Err(anyhow::anyhow!(
            "internal update-lock command reached normal command dispatch"
        )),
        Some(Commands::HoldDaemonUpdateBoundary { .. }) => Err(anyhow::anyhow!(
            "internal installer daemon-boundary command reached normal command dispatch"
        )),
        Some(Commands::InstallerFileOp { .. }) => Err(anyhow::anyhow!(
            "internal installer file command reached normal command dispatch"
        )),
        Some(Commands::CleanupSelfUpdate { .. }) => Err(anyhow::anyhow!(
            "internal self-update cleanup command reached normal command dispatch"
        )),
        Some(Commands::Use {
            alias,
            consume_card,
        }) => commands::use_cmd(alias.as_deref(), json, consume_card).await,
        Some(Commands::List { force }) => {
            commands::list_cmd(force, json, auth_preflight_performed).await
        }
        Some(Commands::ResetCard { alias, yes }) => {
            commands::reset_card_cmd(&alias, yes, json).await
        }
        Some(Commands::Rename { old, new }) => commands::rename_cmd(&old, &new, json),
        Some(Commands::Delete { alias, yes }) => commands::delete_cmd(&alias, yes, json),
        Some(Commands::Login { alias, device }) => {
            commands::login_cmd(alias.as_deref(), device, json).await
        }
        Some(Commands::Import { path, alias }) => {
            commands::import_cmd(&path, alias.as_deref(), json).await
        }
        Some(Commands::SelfUpdate {
            check,
            version,
            dev,
            stable,
        }) => commands::self_update_cmd(check, version.as_deref(), dev, stable, json).await,
        Some(Commands::Warmup { alias }) => commands::warmup_cmd(alias.as_deref(), json).await,
        Some(Commands::Open) => commands::open_cmd(),
        Some(Commands::Daemon(sub)) => daemon::dispatch(sub, json, file_log_writer).await,
        None => tui::run_tui(file_log_writer).await,
    };

    // Run this even when the command failed: a request may have rotated a
    // single-use credential before a later step returned the command error.
    // The original command result remains authoritative.
    let sync_result = match &auth_check {
        AuthCheckResult::Synced(expected_marker) => resync_profile_after_command(expected_marker),
        AuthCheckResult::NoChange | AuthCheckResult::Detected => Ok(()),
    };
    preserve_command_result_after_sync(command_result, sync_result, |error| {
        eprintln!("{}", color::warn(&format_post_command_sync_warning(error)));
    })
}

// ── startup auth change detection ────────────────────────

#[derive(Debug)]
enum AuthCheckResult {
    NoChange,
    Detected, // change detected but not synced (non-interactive or user declined)
    /// Change detected and synchronized to this exact app-owned marker.
    Synced(profile::CurrentMarkerSnapshot),
}

fn check_auth_change() -> Result<AuthCheckResult> {
    use std::io::{self, IsTerminal};

    let change = profile::detect_auth_change().context("checking live auth changes")?;
    if matches!(
        change,
        profile::AuthChange::NoChange | profile::AuthChange::NoLiveAuth
    ) {
        return Ok(AuthCheckResult::NoChange);
    }

    if let profile::AuthChange::UnresolvedIdentity { aliases } = &change {
        user_println(&format!(
            "auth.json and {} saved profile(s) ({}) share only part of the required account identity — \
             refusing to update credentials without matching both account id and email. \
             Run `codex-switch-global-pace use <alias>` to restore a known profile, \
             or `codex-switch-global-pace login <alias>` to re-authenticate the intended profile.",
            aliases.len(),
            aliases.join(", ")
        ));
        return Ok(AuthCheckResult::Detected);
    }

    if matches!(&change, profile::AuthChange::UnidentifiedAccount) {
        user_println(
            "auth.json does not match a saved profile and contains neither account_id nor email — \
             refusing to bind it to the previous profile marker. \
             Run `codex-switch-global-pace use <alias>` to restore a known profile, \
             or `codex-switch-global-pace login <alias>` to authenticate the intended profile.",
        );
        return Ok(AuthCheckResult::Detected);
    }

    // Non-interactive stdin — don't prompt, don't silently mutate state
    if !io::stdin().is_terminal() {
        match &change {
            profile::AuthChange::NewAccount => {
                let label = read_live_account_label()?;
                user_println(&format!(
                    "Detected new account ({label}) in auth.json (use `codex-switch-global-pace list` interactively to save)."
                ));
            }
            profile::AuthChange::TokensUpdated { alias } => {
                user_println(&format!(
                    "auth.json credentials changed for profile '{alias}' (use `codex-switch-global-pace list` interactively to update)."
                ));
            }
            profile::AuthChange::NoLiveAuth
            | profile::AuthChange::UnidentifiedAccount
            | profile::AuthChange::UnresolvedIdentity { .. }
            | profile::AuthChange::NoChange => unreachable!(),
        }
        return Ok(AuthCheckResult::Detected);
    }

    let mut synced_alias: Option<String> = None;

    match change {
        profile::AuthChange::NewAccount => {
            let label = read_live_account_label()?;
            user_println(&format!(
                "Detected new account ({label}) in auth.json — not in any saved profile."
            ));
            if commands::confirm("Save as a new profile? [Y/n] ") {
                match profile::cmd_save(None) {
                    Ok(action) => {
                        user_println(&format!("Profile {}: {}", action.action(), action.alias()));
                        synced_alias = Some(action.alias().to_string());
                    }
                    Err(e) => eprintln!("{}", color::error(&format!("Failed to save: {e}"))),
                }
            }
        }
        profile::AuthChange::TokensUpdated { alias } => {
            let label = read_live_account_label()?;
            user_println(&format!(
                "auth.json credentials changed for account '{alias}' ({label})."
            ));
            let live_ts = read_last_refresh(auth::codex_auth_path());
            let profile_ts = read_last_refresh(profile::profile_auth_path(&alias));
            let prompt =
                format_resync_confirm_prompt(&alias, live_ts.as_deref(), profile_ts.as_deref());
            if commands::confirm(&prompt) {
                match profile::update_profile_from_live(&alias) {
                    Ok(()) => {
                        user_println(&format!("Profile '{alias}' updated."));
                        synced_alias = Some(alias);
                    }
                    Err(e) => eprintln!("{}", color::error(&format!("Failed to update: {e}"))),
                }
            }
        }
        profile::AuthChange::NoLiveAuth
        | profile::AuthChange::UnidentifiedAccount
        | profile::AuthChange::UnresolvedIdentity { .. }
        | profile::AuthChange::NoChange => unreachable!(),
    }

    let Some(alias) = synced_alias else {
        return Ok(AuthCheckResult::Detected);
    };
    Ok(match confirmed_sync_marker(&alias) {
        Ok(marker) => AuthCheckResult::Synced(marker),
        Err(error) => {
            eprintln!(
                "{}",
                color::error(&format!(
                    "Failed to confirm the synchronized profile marker: {error:#}"
                ))
            );
            AuthCheckResult::Detected
        }
    })
}
