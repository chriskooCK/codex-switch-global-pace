use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Detect terminal capabilities automatically
    Auto,
    /// Always use colors unless NO_COLOR is set
    Always,
    /// Never use colors
    Never,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon (Beta; foreground if --foreground, otherwise detached)
    Start {
        /// Run in foreground (for service managers)
        #[arg(long)]
        foreground: bool,
        /// Exact executable restored by a verified installer and selected for restart.
        #[arg(long, hide = true, value_name = "ABSOLUTE_PATH")]
        expected_executable: Option<std::path::PathBuf>,
    },
    /// Stop a running Beta daemon
    Stop {
        /// Exact executable owned by the installed service being stopped by a verified installer.
        #[arg(long, hide = true, value_name = "ABSOLUTE_PATH")]
        expected_service_executable: Option<std::path::PathBuf>,
    },
    /// Show Beta daemon status
    Status {
        /// Print the strict state tuple consumed by the direct installers.
        #[arg(long, hide = true)]
        installer_state: bool,
    },
    /// Install the Beta daemon as a system service (LaunchAgent on macOS, systemd on Linux, Task Scheduler on Windows)
    Install {
        /// Exact executable currently owned by a service being migrated.
        #[arg(long, hide = true, value_name = "ABSOLUTE_PATH")]
        expected_existing_executable: Option<std::path::PathBuf>,
    },
    /// Uninstall the Beta daemon system service
    Uninstall {
        /// Exact executable path that the service definition must own.
        /// Used by the verified direct uninstaller, whose helper executable is temporary.
        #[arg(long, hide = true, value_name = "ABSOLUTE_PATH")]
        expected_executable: Option<std::path::PathBuf>,
        /// Verify service-definition ownership without changing service state.
        #[arg(long, hide = true)]
        check_owner: bool,
    },
}

#[derive(Parser)]
#[command(
    name = "codex-switch-global-pace",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", env!("CARGO_PKG_REPOSITORY")),
    about = concat!(
        "Codex multi-profile manager with a global weekly pace dashboard\n",
        env!("CARGO_PKG_REPOSITORY")
    ),
    long_about = None,
    override_usage = "codex-switch-global-pace [OPTIONS] [COMMAND]",
    after_help = "Examples:\n  codex-switch-global-pace                 # open the TUI\n  codex-switch-global-pace list\n  codex-switch-global-pace use\n  codex-switch-global-pace rename old-alias new-alias\n  codex-switch-global-pace import ./auth-backups\n  codex-switch-global-pace self-update --check\n\nRun `codex-switch-global-pace <command> --help` for command-specific options."
)]
pub struct Cli {
    /// Output as compact JSON (supported by list, use, reset-card, warmup, rename, delete, login, import, self-update, daemon status)
    #[arg(long, global = true)]
    pub json: bool,

    /// Output as pretty-printed JSON
    #[arg(long, global = true)]
    pub json_pretty: bool,

    /// Proxy URL (overrides CS_PROXY / HTTP_PROXY / HTTPS_PROXY / ALL_PROXY env vars)
    ///
    /// Supported formats:
    ///   http://[user:pass@]host:port
    ///   https://[user:pass@]host:port
    ///   socks4://host:port
    ///   socks5://[user:pass@]host:port      (local DNS)
    ///   socks5h://[user:pass@]host:port     (remote DNS)
    #[arg(long, global = true, env = "CS_PROXY")]
    pub proxy: Option<String>,

    /// Color output mode (NO_COLOR always disables color)
    #[arg(long, global = true, default_value = "auto", env = "CS_COLOR")]
    pub color: ColorMode,

    /// Enable debug logging (shows HTTP requests, API responses, cache status)
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(name = "__hold-update-lock", hide = true)]
    HoldUpdateLock,
    #[command(name = "__hold-daemon-update-boundary", hide = true)]
    HoldDaemonUpdateBoundary {
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        initial_executable: std::path::PathBuf,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        replacement_executable: std::path::PathBuf,
        #[arg(long)]
        expected_executable_token: Option<String>,
        #[arg(long)]
        ready_nonce: Option<String>,
    },
    #[command(name = "__installer-file-op", hide = true)]
    InstallerFileOp {
        #[arg(value_enum)]
        operation: crate::installer_fs::InstallerFileOperation,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        source: Option<std::path::PathBuf>,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        destination: Option<std::path::PathBuf>,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        displaced: Option<std::path::PathBuf>,
        #[arg(long)]
        expected_token: Option<String>,
        #[arg(long)]
        expected_destination_token: Option<String>,
    },
    #[command(name = "__cleanup-self-update", hide = true)]
    CleanupSelfUpdate {
        #[arg(long)]
        parent_pid: u32,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        displaced: std::path::PathBuf,
        #[arg(long)]
        expected_token: String,
        #[arg(long)]
        expected_executable_token: String,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        journal: std::path::PathBuf,
        #[arg(long)]
        expected_journal_token: String,
        #[arg(long)]
        ready_nonce: String,
    },
    /// Switch to a profile; omit alias to auto-select using the unified scoring algorithm
    Use {
        /// Profile alias (omit to auto-select)
        alias: Option<String>,
        /// When the pool is exhausted, automatically consume the earliest-expiring
        /// reset card to revive an account (only applies when alias is omitted;
        /// ignored when an alias is given)
        #[arg(long)]
        consume_card: bool,
    },
    /// List all profiles with account info, usage, and availability
    List {
        /// Force refresh, bypass cache
        #[arg(long, short)]
        force: bool,
    },
    /// Consume the earliest-expiring Codex reset card for a profile
    ResetCard {
        /// Profile alias
        alias: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Rename a profile
    Rename {
        /// Current profile alias
        old: String,
        /// New profile alias
        new: String,
    },
    /// Delete a profile (archived for recovery)
    Delete {
        /// Profile alias
        alias: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Log in via browser or --device code flow; re-authorizes if alias already exists
    Login {
        /// Profile alias -- if it already exists, re-authorizes it; otherwise creates a new profile
        alias: Option<String>,

        /// Use device code flow (for headless servers without a browser)
        #[arg(long)]
        device: bool,
    },
    /// Import an auth.json file, or recursively scan a directory for JSON files to validate and import
    Import {
        /// Path to an auth.json file or a directory containing JSON files
        path: String,
        /// Optional profile alias (single-file import only; directories auto-assign aliases)
        alias: Option<String>,
    },
    /// Manually check GitHub Releases (`--check`) or update this binary
    #[command(
        after_help = "Examples:\n  codex-switch-global-pace self-update --check\n  codex-switch-global-pace self-update\n  codex-switch-global-pace self-update --dev\n  codex-switch-global-pace self-update --stable\n\nOnly the TUI checks automatically at startup. Other commands never check automatically.\nWithout flags, updates within the current channel (stable or dev).\n`--dev` switches to the dev channel. `--stable` switches back to stable."
    )]
    SelfUpdate {
        /// Check whether a newer version is available without installing it
        #[arg(long)]
        check: bool,
        /// Install a specific newer stable version; cannot be combined with --check or channel flags
        #[arg(long, conflicts_with_all = ["check", "dev", "stable"])]
        version: Option<String>,
        /// Switch to the dev channel (latest dev build)
        #[arg(long, conflicts_with = "stable")]
        dev: bool,
        /// Switch back to the stable channel (from dev)
        #[arg(long, conflicts_with = "dev")]
        stable: bool,
    },
    /// Send a minimal request to activate the quota window countdown for one or all profiles
    ///
    /// Fresh accounts show no reset timer until their first real request.
    /// This command triggers that timer without running a real task.
    #[command(
        after_help = "Examples:\n  codex-switch-global-pace warmup          # warmup all profiles\n  codex-switch-global-pace warmup myalias  # warmup a specific profile\n  codex-switch-global-pace --json warmup   # report per-profile JSON results"
    )]
    Warmup {
        /// Profile alias to warm up (omit to warm up all profiles)
        alias: Option<String>,
    },
    /// Open the application data directory in the system file manager
    Open,
    /// Background daemon (Beta) for automatic account switching
    #[command(subcommand)]
    Daemon(DaemonCommand),
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, DaemonCommand};
    use clap::{CommandFactory, Parser};
    use std::path::PathBuf;

    #[test]
    fn exact_self_update_version_conflicts_with_check_and_help_explains_it() {
        let error = Cli::try_parse_from([
            "codex-switch-global-pace",
            "self-update",
            "--check",
            "--version",
            "20260826.1.0",
        ])
        .err()
        .expect("an exact install version must not be ignored by check mode");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);

        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("self-update")
            .expect("self-update subcommand exists")
            .render_long_help()
            .to_string();
        assert!(help.contains(
            "Install a specific newer stable version; cannot be combined with --check or channel flags"
        ));
    }

    #[test]
    fn hidden_daemon_owner_check_keeps_the_exact_expected_path() {
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

        let Some(Commands::Daemon(DaemonCommand::Uninstall {
            expected_executable,
            check_owner,
        })) = cli.command
        else {
            panic!("expected daemon uninstall command");
        };
        assert_eq!(expected_executable, Some(PathBuf::from(expected)));
        assert!(check_owner);
    }

    #[test]
    fn hidden_installer_stop_keeps_the_exact_service_executable() {
        let expected = if cfg!(windows) {
            r"C:\Program Files\codex-switch-global-pace.exe"
        } else {
            "/opt/codex-switch-global-pace"
        };
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "daemon",
            "stop",
            "--expected-service-executable",
            expected,
        ])
        .unwrap();

        let Some(Commands::Daemon(DaemonCommand::Stop {
            expected_service_executable,
        })) = cli.command
        else {
            panic!("expected daemon stop command");
        };
        assert_eq!(
            expected_service_executable,
            Some(std::path::PathBuf::from(expected))
        );
    }

    #[test]
    fn hidden_installer_start_keeps_the_exact_restored_executable() {
        let expected = if cfg!(windows) {
            r"C:\Program Files\codex-switch-global-pace.exe"
        } else {
            "/opt/codex-switch-global-pace"
        };
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "daemon",
            "start",
            "--expected-executable",
            expected,
        ])
        .unwrap();

        let Some(Commands::Daemon(DaemonCommand::Start {
            foreground,
            expected_executable,
        })) = cli.command
        else {
            panic!("expected daemon start command");
        };
        assert!(!foreground);
        assert_eq!(
            expected_executable,
            Some(std::path::PathBuf::from(expected))
        );
    }

    #[test]
    fn hidden_service_migration_keeps_the_exact_existing_executable() {
        let expected = if cfg!(windows) {
            r"C:\Program Files\legacy-codex-switch.exe"
        } else {
            "/usr/local/bin/codex-switch-global-pace"
        };
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "daemon",
            "install",
            "--expected-existing-executable",
            expected,
        ])
        .unwrap();

        let Some(Commands::Daemon(DaemonCommand::Install {
            expected_existing_executable,
        })) = cli.command
        else {
            panic!("expected daemon install command");
        };
        assert_eq!(expected_existing_executable, Some(PathBuf::from(expected)));
    }

    #[test]
    fn hidden_installer_state_flag_parses_without_json_mode() {
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "daemon",
            "status",
            "--installer-state",
        ])
        .unwrap();

        let Some(Commands::Daemon(DaemonCommand::Status { installer_state })) = cli.command else {
            panic!("expected daemon status command");
        };
        assert!(installer_state);
    }

    #[test]
    fn hidden_installer_state_preserves_global_json_modes_for_runtime_rejection() {
        for json_flag in ["--json", "--json-pretty"] {
            let cli = Cli::try_parse_from([
                "codex-switch-global-pace",
                json_flag,
                "daemon",
                "status",
                "--installer-state",
            ])
            .unwrap();
            assert!(cli.json || cli.json_pretty);
            assert!(matches!(
                cli.command,
                Some(Commands::Daemon(DaemonCommand::Status {
                    installer_state: true
                }))
            ));
        }
    }

    #[test]
    fn hidden_self_update_cleanup_keeps_every_exact_attestation() {
        const DISPLACED: &str = r"C:\Program Files\.codex-switch-global-pace.exe.self-update-displaced-00112233445566778899aabbccddeeff";
        const JOURNAL: &str =
            r"C:\Program Files\.codex-switch-global-pace.exe.self-update-cleanup-journal";
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "__cleanup-self-update",
            "--parent-pid",
            "42",
            "--displaced",
            DISPLACED,
            "--expected-token",
            "1:2|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--expected-executable-token",
            "3:4|bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--journal",
            JOURNAL,
            "--expected-journal-token",
            "5:6|cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "--ready-nonce",
            "00112233445566778899aabbccddeeff",
        ])
        .unwrap();

        let Some(Commands::CleanupSelfUpdate {
            parent_pid,
            displaced,
            expected_token,
            expected_executable_token,
            journal,
            expected_journal_token,
            ready_nonce,
        }) = cli.command
        else {
            panic!("expected self-update cleanup command");
        };
        assert_eq!(parent_pid, 42);
        assert_eq!(displaced, std::path::PathBuf::from(DISPLACED));
        assert_eq!(
            expected_token,
            "1:2|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            expected_executable_token,
            "3:4|bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert_eq!(journal, std::path::PathBuf::from(JOURNAL));
        assert_eq!(
            expected_journal_token,
            "5:6|cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        );
        assert_eq!(ready_nonce, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn hidden_lifecycle_holder_accepts_the_bound_image_attestation_pair() {
        let cli = Cli::try_parse_from([
            "codex-switch-global-pace",
            "__hold-daemon-update-boundary",
            "--initial-executable",
            r"C:\Program Files\codex-switch-global-pace.exe",
            "--replacement-executable",
            r"C:\Program Files\codex-switch-global-pace.exe",
            "--expected-executable-token",
            "3:4|bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--ready-nonce",
            "00112233445566778899aabbccddeeff",
        ])
        .unwrap();

        let Some(Commands::HoldDaemonUpdateBoundary {
            expected_executable_token,
            ready_nonce,
            ..
        }) = cli.command
        else {
            panic!("expected daemon lifecycle holder command");
        };
        assert_eq!(
            expected_executable_token.as_deref(),
            Some("3:4|bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            ready_nonce.as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    #[test]
    fn warmup_accepts_global_json_mode_with_or_without_an_alias() {
        for args in [
            vec!["codex-switch-global-pace", "--json", "warmup"],
            vec!["codex-switch-global-pace", "warmup", "work", "--json"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(cli.json);
            assert!(matches!(cli.command, Some(Commands::Warmup { .. })));
        }
    }
}
