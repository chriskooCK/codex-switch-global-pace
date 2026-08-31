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
| `$CODEX_HOME/auth.json.bak.<timestamp>-<nonce>` | Managed recovery copies of the displaced live authentication; the three newest are retained after normal cleanup. |
| `$CODEX_SWITCH_HOME/profiles/<alias>/auth.json` | Saved profile authentication. |
| `$CODEX_SWITCH_HOME/deleted-profiles/` | Recoverable deleted profiles. |
| `$CODEX_SWITCH_HOME/recovery/` | Quarantined credentials from an interrupted or rejected token-rotation path; never selectable as profiles. |
| `$CODEX_SWITCH_HOME/current` | Current alias marker. |
| `$CODEX_SWITCH_HOME/cache.json` | Identity-bound usage, workspace metadata, rejected-credential verdicts, and selection history. |
| `$CODEX_SWITCH_HOME/config.toml` | Optional settings. |
| `$CODEX_SWITCH_HOME/daemon-state.json` | Last Beta daemon state snapshot. |
| `$CODEX_SWITCH_HOME/logs/` | Diagnostic logs: one file per day, 3 calendar days retained, with a 10 MiB maintenance target. |
| `$CODEX_SWITCH_HOME/*.lock` | Cross-process coordination files. |

Unset variables default to `~/.codex` and `~/.codex-switch` respectively (`%USERPROFILE%\.codex-switch` on Windows).
Overrides may point anywhere on an absolute path, but a private state
path is accepted only when its full ancestry can be kept private and stable:

- Every existing component must be an ordinary directory, never a symbolic
  link, Windows junction, or other reparse point.
- On Unix, existing components must be owned by the effective user or root. A
  group- or other-writable ancestor is rejected unless it is sticky (for
  example `/tmp`) and the protected child entry belongs to the effective user
  or root. The final state directory must belong to the effective user and is
  kept at mode `0700`.
- On Windows, a private directory must belong to the current user and receives
  a protected ACL for that user, Local System, and Administrators. Credential
  publication and the log writer keep direct handles to every path component,
  without delete sharing, for the entire path-based operation. A writable
  shared parent is therefore never trusted merely because an earlier path
  check succeeded.

These checks run whenever a private state directory is prepared. Operations
that hold a private-directory guard — including live-auth temporary-file
publication, rotated-credential recovery staging, and the log writer — keep it
for the complete path-based operation. An override whose ancestry cannot meet
the rules fails explicitly instead of falling back to a different directory.

## Credential lifecycle and backups

There are four distinct credential states:

1. `$CODEX_HOME/auth.json` is the single **live** credential Codex reads when a
   new Codex process starts. Switching publishes the selected profile here; it
   does not change a Codex process that is already running.
2. `$CODEX_SWITCH_HOME/profiles/<alias>/auth.json` is a **saved profile**.
   Profiles are long-lived until they are reauthorized, deleted, or removed by
   the user.
3. Deleting an inactive profile moves its complete directory to
   `deleted-profiles/<alias>.backup-<timestamp>`. These archives do not expire
   automatically and can be moved back with the
   [platform-specific recovery commands](Troubleshooting.md#recover-a-deleted-profile).
4. `recovery/` holds a token-rotation result when an import or refresh cannot
   safely finish. A recovery file is intentionally not a profile and is never
   selected. Keep the exact path named by the error private until the account
   works again; then remove that exact file rather than deleting the directory
   by wildcard.

Before replacing an existing live credential, the application creates an
independent `auth.json.bak.<timestamp>-<nonce>` beside `auth.json`. After normal
cleanup it keeps the three newest managed live-auth backups. If exact identity
checks cannot prove that an older backup is still the file the application
created, it is preserved and a warning is emitted; do not assume that an extra
file is safe to erase. Backups, deleted profiles, and recovery files all contain
usable or identifying authentication material. Never commit, sync, email, or
attach them to an issue.

Logs and `cache.json` do not contain full profile credentials, but they can
contain account, workspace, quota, path, or infrastructure identifiers. Treat
them as private and redact them before sharing.

## Back up, restore, or move to a new machine

Use an encrypted, access-controlled destination. Copying `~/.codex-switch` to a
public Git repository or an unencrypted cloud folder is equivalent to sharing
the saved account credentials.

1. End every interactive process started by `codex`, `codex resume`, or
   `codex exec`. On Windows, also quit the Codex application from its
   notification-area (system-tray) menu and confirm in Task Manager that it is
   no longer running; closing its window can leave it resident. Long-lived MCP
   and app-server helpers are not interactive-session blockers, although their
   parent clients should still be closed for a consistent filesystem copy.
2. Run `codex-switch-global-pace daemon stop`. Do not copy while a login,
   import, refresh, switch, or daemon operation is active.
3. Back up the complete `$CODEX_SWITCH_HOME` directory as one unit, preserving
   permissions and directory structure. Also save `$CODEX_HOME/config.toml` so
   `cli_auth_credentials_store = "file"` follows the profiles. Copying the live
   `auth.json` and its managed backups is optional: the selected saved profile
   can republish live authentication with `use`.
4. On the destination machine, install Codex and codex-switch-global-pace, keep
   both applications stopped, set the same `CODEX_HOME` and
   `CODEX_SWITCH_HOME` overrides if used, and restore into an absent or empty
   destination. Do not merge two state directories or overwrite aliases by
   hand; use `import` when only selected auth files should be added.
5. Run `codex-switch-global-pace list`, then
   `codex-switch-global-pace use <alias>`. Start a new Codex process and verify
   the intended account. If a refresh token expired or was already consumed,
   reauthorize that alias with `login <alias>` instead of restoring older
   backups over newer state.

The next command re-applies the private directory and file protections and
fails if the restored path, owner, ACL, permissions, symlink, or junction is
unsafe. Keep the backup until every profile has been verified, then dispose of
it through the encrypted storage system rather than a normal public recycle
location.

## Profile alias rules

Aliases contain 1 through 64 ASCII characters. Letters, digits, `_`, `-`, and
`.` are allowed; `.` and `..` by themselves are rejected. Spaces, slashes,
Unicode characters, and shell metacharacters are not allowed. Because each
alias is also a directory name and common Windows/macOS filesystems ignore
case, do not create aliases that differ only by capitalization. Use a short
purpose name such as `personal`, `work-team`, or `client_2`, and quote it in
scripts even though the accepted character set is narrow.

## Codex interoperability target

This release explicitly targets the authentication and ChatGPT backend contract
used by Codex CLI **0.149.0**. The target controls the compatibility version sent
by this application; it does not install, pin, upgrade, or downgrade the Codex
binary on the machine. Newer or older Codex versions may interoperate, but they
are not claimed compatible until validated because upstream auth formats,
endpoints, and managed-login rules can change independently.

When login, model discovery, usage, or token refresh stops working after a Codex
upgrade, first update codex-switch-global-pace to the latest stable release and
retry. If the failure remains, report both exact versions with redacted output.
The file credential-store requirement remains mandatory regardless of Codex
version.

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
defer_switch_while_codex_running = true  # defer for codex/resume/exec sessions and the Windows tray app; not MCP/app-server helpers
```

Configuration is validated once at startup and invalid values stop the command;
they are not replaced with guessed defaults. `safety_margin_7d` and
`switch_threshold` must be finite percentages from 0 through 100. Concurrency
must fit the runtime semaphore, and every interval must fit the runtime timer;
the poll interval validation includes its maximum 16× failure backoff. Unknown
tables or keys are rejected so a misspelling cannot silently select a default,
and a configured proxy URL is parsed at this same startup boundary.

`network.max_concurrent` is the shared HTTP-request limit inside each process.
Usage, token refresh, reset-card, workspace, model-discovery, and warmup work
acquires it for each request and returns it before retry sleep or local cache and
credential persistence. Refresh authorization and profile/auth transaction
waits also happen outside the limit; a follow-up HTTP request must acquire fresh
capacity. It is not a cross-process global limit.

The legacy `[use] mode` and `[use] min_remaining` keys are ignored and produce a startup warning; the unified scoring algorithm replaced the old selection modes.
The removed `[launch]` table is also ignored with a startup warning and can be deleted from existing configuration files.

## Environment variables

| Variable | Effect |
|---|---|
| `CODEX_HOME` | Codex's own home; `auth.json` and Codex's `config.toml` live here (default `~/.codex`). A non-empty override must be absolute, contain no `..`, must not be a filesystem root, and must satisfy the full private-path ownership, permission, and direct-component rules above. |
| `CODEX_SWITCH_HOME` | Relocates codex-switch-global-pace state (default `~/.codex-switch`); an empty value is ignored, and a non-empty override must be absolute, contain no `..`, must not be a filesystem root, and must satisfy the same private-path rules. |
| `CS_PROXY` | Proxy URL; same as `--proxy`. |
| `CS_COLOR` | Color mode; same as `--color`. |
| `NO_COLOR` | If present, even with an empty value, disables all CLI and TUI color. It overrides `--color always` and `CS_COLOR=always`; non-color emphasis needed to show TUI selection remains visible. |
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

Enabled diagnostic records are written to `$CODEX_SWITCH_HOME/logs/`, one file per calendar day, keeping 3 days with a 10 MiB maintenance target. To avoid a directory scan for every record, each writer process runs size and age maintenance on its first record and then after either 60 seconds or 1 MiB of appended records. Concurrent writers or an existing oversized directory can therefore remain above the target until a scheduled maintenance pass. Ordinary CLI and TUI commands that emit no enabled record do not initialize the log path. `daemon start` is the deliberate exception: before spawning or publishing PID readiness, it validates the secure log directory, lock, and current daily append handle without emitting a synthetic record or running retention. Once active, secured handles are reused until date rollover rather than reopened for every record. Level resolution: `--debug` wins over `RUST_LOG`, which wins over `daemon.log_level`; the default is `error`. `daemon.log_level` applies only to `daemon` commands — it does not change logging for `list`, `use`, or other commands.

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
