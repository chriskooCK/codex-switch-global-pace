# Configuration

`codex-switch-global-pace` uses `~/.codex-switch` by default. Set `CODEX_SWITCH_HOME` to relocate its profiles, cache, locks, logs, and daemon state. This does not change Codex's own home; set `CODEX_HOME` for that.

Configuration is optional: a missing `config.toml` means defaults. An existing but unreadable or invalid file fails fast with its path instead of being silently ignored.

## Authentication prerequisite

The live Codex credential store must be file-backed because switching replaces `$CODEX_HOME/auth.json` atomically. Add the following to `$CODEX_HOME/config.toml`:

```toml
cli_auth_credentials_store = "file"
```

Explicit `keyring`, `auto`, and `ephemeral` modes are rejected. A managed configuration with `forced_login_method = "api"` is also incompatible with ChatGPT login profiles.

### Why only the file store is supported

This is a deliberate limitation, not a temporary gap:

- Every reliability guarantee codex-switch-global-pace makes — cross-process locking, atomic replacement, backup rotation — is built on file primitives. OS keyrings (macOS Keychain, Windows Credential Manager, Linux Secret Service) expose no locking or atomic-replace semantics, so a switch racing a running Codex process could silently select the wrong account instead of failing loudly.
- Codex's keyring entry layout is an undocumented internal format. It was already reworked once (June 2026, when Windows moved to an encrypted sidecar because of a Credential Manager size limit) and now differs between Windows and other platforms. Depending on it would break silently whenever Codex changes it.
- An `ephemeral` store persists nothing, so there is nothing to switch.

Accounts are added by logging in with `codex-switch-global-pace login` or by importing an existing `auth.json`; codex-switch-global-pace never reads credentials out of an OS keyring. If Codex was previously used with a keyring store, set `cli_auth_credentials_store = "file"` and log in again.

## Paths

| Path | Purpose |
|---|---|
| `$CODEX_HOME/auth.json` | Live authentication read by Codex. |
| `$CODEX_SWITCH_HOME/profiles/<alias>/auth.json` | Saved profile authentication. |
| `$CODEX_SWITCH_HOME/deleted-profiles/` | Recoverable deleted profiles. |
| `$CODEX_SWITCH_HOME/current` | Current alias marker. |
| `$CODEX_SWITCH_HOME/cache.json` | Per-profile usage cache. |
| `$CODEX_SWITCH_HOME/config.toml` | Optional settings. |
| `$CODEX_SWITCH_HOME/daemon-state.json` | Last Beta daemon state snapshot. |
| `$CODEX_SWITCH_HOME/logs/` | Diagnostic logs: one file per day, 3 calendar days retained, 10 MiB total cap. |
| `$CODEX_SWITCH_HOME/*.lock` | Cross-process coordination files. |

Unset variables default to `~/.codex` and `~/.codex-switch` respectively (`%USERPROFILE%\.codex-switch` on Windows).
Each resolved state directory, or its nearest existing parent when the state
directory has not been created yet, must be an ordinary directory rather than a
symbolic link, Windows junction, or other reparse point. The same rule is
checked at startup and at the private-write boundary.

## Settings

All keys with their defaults:

```toml
[proxy]
url = "socks5h://user:pass@127.0.0.1:1080"  # no default; unset means no proxy from config
no_proxy = "localhost,127.0.0.1"

[cache]
ttl = 300                          # usage cache TTL in seconds

[network]
max_concurrent = 20                # 1 through the Tokio semaphore runtime limit

[tui]
auto_refresh_interval_secs = 120   # minimum 30 seconds

[use]
safety_margin_7d = 20              # 7d headroom % below which scoring penalizes
team_priority = true               # prefer Team-plan accounts during selection

[daemon]
poll_interval_secs = 60            # usage poll; minimum 1 second
switch_threshold = 80              # primary usage % that starts candidate search; secondary if no primary
cache_refresh_interval_secs = 300  # all-profile cache refresh; minimum 1 second
auto_warmup = false                # warm inactive quota windows during cache refresh
token_check_interval_secs = 300    # proactive token refresh; minimum 1 second
notify = false                     # desktop notification on switch
log_level = "error"                # non-empty tracing filter level
defer_switch_while_codex_running = true  # hold a pending switch during interactive Codex sessions
```

Configuration is validated once at startup and invalid values stop the command;
they are not replaced with guessed defaults. `safety_margin_7d` and
`switch_threshold` must be finite percentages from 0 through 100. Concurrency
must fit the runtime semaphore, and every interval must fit the runtime timer;
the poll interval validation includes its maximum 16× failure backoff. Unknown
tables or keys are rejected so a misspelling cannot silently select a default,
and a configured proxy URL is parsed at this same startup boundary.

The legacy `[use] mode` and `[use] min_remaining` keys are ignored and produce a startup warning; the unified scoring algorithm replaced the old selection modes.
The removed `[launch]` table is also ignored with a startup warning and can be deleted from existing configuration files.

## Environment variables

| Variable | Effect |
|---|---|
| `CODEX_HOME` | Codex's own home; `auth.json` and Codex's `config.toml` live here (default `~/.codex`). A non-empty override must be absolute, contain no `..`, must not be a filesystem root, and its nearest existing directory must not be a link or reparse point. |
| `CODEX_SWITCH_HOME` | Relocates codex-switch-global-pace state (default `~/.codex-switch`); an empty value is ignored, and a non-empty override must be absolute, contain no `..`, must not be a filesystem root, and its nearest existing directory must not be a link or reparse point. |
| `CS_PROXY` | Proxy URL; same as `--proxy`. |
| `CS_COLOR` | Color mode; same as `--color`. |
| `NO_COLOR` | Disables color output regardless of other settings. |
| `RUST_LOG` | Overrides the log filter; `--debug` has higher priority. |
| `CODEX_CA_CERTIFICATE`, `SSL_CERT_FILE` | Custom CA certificate for HTTPS, in Codex-compatible fallback order. |

## Proxy precedence

Proxy settings resolve in this order:

1. `--proxy`
2. `CS_PROXY`
3. `[proxy]` in `config.toml`
4. `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`

Supported schemes:

| Scheme | DNS resolution | Authentication |
|---|---|---|
| `http://[user:pass@]host:port` | local | supported |
| `https://[user:pass@]host:port` | local | supported |
| `socks4://host:port` | local | not supported |
| `socks5://[user:pass@]host:port` | local | supported |
| `socks5h://[user:pass@]host:port` | remote (at the proxy) | supported |

Do not commit credentials in configuration files.

## Logging

Every command writes diagnostic logs to `$CODEX_SWITCH_HOME/logs/`, one file per calendar day, keeping 3 days and at most 10 MiB. Level resolution: `--debug` wins over `RUST_LOG`, which wins over `daemon.log_level`; the default is `error`. `daemon.log_level` applies only to `daemon` commands — it does not change logging for `list`, `use`, or other commands.

## Platform integration

- macOS uses a LaunchAgent for the Beta daemon.
- Linux uses a systemd user service; headless login should use `login --device`.
- Windows uses Task Scheduler and requires elevated PowerShell for daemon installation. Windows Terminal or PowerShell is recommended for the TUI.

> **`CODEX_SWITCH_HOME` and installed daemon services:** `daemon install` resolves the current state directory to an absolute path and records it as `CODEX_SWITCH_HOME` in the LaunchAgent, systemd unit, or Task Scheduler command. If you later relocate the state directory, uninstall and reinstall the daemon service from a shell using the new value.

On Windows, Task Scheduler limits its command to 262 characters and expands `%NAME%` text through `cmd.exe`. Service installation therefore fails clearly if the executable, `CODEX_HOME`, or `CODEX_SWITCH_HOME` contains `%`, or if their combined task command is too long; choose shorter literal paths and run `daemon install` again.

## Next steps

- See what these settings control in the [Feature guide](Feature-Guide.md).
- Look up the flags that override configuration in the [Command reference](Command-Reference.md).
- Diagnose configuration errors with [Troubleshooting](Troubleshooting.md).
