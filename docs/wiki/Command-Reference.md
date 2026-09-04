# Command reference

The installed binary remains authoritative: use `codex-switch-global-pace --help` and `codex-switch-global-pace <command> --help` for the exact flags and examples supported by your version.

Running `codex-switch-global-pace` with no subcommand opens the TUI. This also
applies when the Windows executable is launched directly.

## Commands

| Command | Purpose |
|---|---|
| `login [--device] [-y\|--yes] [alias]` | Add or reauthorize a profile through browser PKCE or device-code login. Every saved OAuth result requires a non-empty `account_id` and email. A complete existing alias accepts only the same identity. An incomplete legacy alias requires a default-No confirmation, archives its exact previous credentials under `deleted-profiles/`, and replaces that same alias only when all known identity fields match; `-y` / `--yes` supplies that confirmation for an intentional non-interactive run. For a new alias, a distinct identity creates and activates it; an identity already saved under another alias updates and activates that matching profile instead. |
| `import <path> [alias]` | Validate and import one `auth.json`, or recursively scan a directory for JSON files. The alias applies to single-file imports only; directories auto-assign aliases. |
| `list [-f]` | Show profiles, usage, and availability; `-f` / `--force` bypasses the cache. |
| `use [alias] [--consume-card]` | Switch explicitly, or omit the alias to auto-select with the unified scoring algorithm. When the pool is exhausted, `--consume-card` consumes the earliest-expiring reset card to revive an account (auto-select only; ignored when an alias is given). |
| `reset-card <alias> [-y]` | Consume the earliest-expiring reset card for a profile after confirmation; `-y` / `--yes` skips the prompt. |
| `warmup [alias]` | Send a minimal request to activate the quota-window countdown for one or all profiles. Supports the global `--json` mode with per-profile results and a top-level `ok` field. |
| `rename <old> <new>` | Rename a saved profile. |
| `delete <alias> [-y]` | Move an inactive profile into recoverable deleted storage; `-y` / `--yes` skips the prompt. |
| `daemon start [--foreground]` | Start the Beta daemon, detached by default; `--foreground` is for service managers. |
| `daemon stop` | Stop a running Beta daemon. |
| `daemon status` | Report daemon support, process and pending-switch state, whether the native service is installed and which manager owns it, plus the effective daemon configuration. JSON includes the same information. |
| `daemon install` | Install the native user service: LaunchAgent on macOS, systemd on Linux, Task Scheduler on Windows (elevated PowerShell required). |
| `daemon uninstall` | Remove the native user service. |
| `self-update [--check] [--dev\|--stable]`<br>`self-update --version <VERSION>` | Check or update a direct installation. Without flags it stays on the current channel; `--version` installs a specific newer stable version and is mutually exclusive with `--check`, `--dev`, and `--stable`. |
| `open` | Open the codex-switch-global-pace data directory in the platform file manager. |

## Global options

| Option | Environment variable | Behavior |
|---|---|---|
| `--json` | — | Compact structured output (supported by `list`, `use`, `reset-card`, `warmup`, `rename`, `delete`, `login`, `import`, `self-update`, `daemon status`). |
| `--json-pretty` | — | Indented structured output for the same commands as `--json`; it selects JSON by itself, and wins over compact formatting if both flags are present. |
| `--proxy <URL>` | `CS_PROXY` | Override proxy configuration for this process; supports `http(s)://`, `socks4://`, `socks5://`, and `socks5h://` (remote DNS). |
| `--color <auto\|always\|never>` | `CS_COLOR` | Control CLI and TUI color. The presence of `NO_COLOR`, even with an empty value, always disables color and overrides `always`; non-color emphasis needed to show TUI selection remains visible. |
| `--debug` | — | Emit diagnostic information (HTTP requests, API responses, cache status) to stderr; redact it before sharing. |
| `-V`, `--version` | — | Print the binary version. |

## Automation contract

- Structured data is written to stdout; progress and diagnostics are written to stderr.
- JSON and other non-interactive execution never consumes a reset card or deletes a profile without an explicit opt-in flag.
- Reauthorizing an incomplete legacy alias is also explicit: JSON and other non-interactive runs stop before OAuth unless `login <alias> --yes` was requested. The approved operation archives the prior credentials before replacing the same alias.
- `list` asks whether to save an untracked live login or refresh a saved profile only in an interactive terminal. `--json`, `--json-pretty`, and stdin-driven non-interactive runs never register or update live credentials implicitly. JSON lists saved profiles only and returns `"profiles": []` when there are none.
- Human `daemon status` prints the process state, `Service: installed|not installed (manager: ...)`, then `Config:` with `poll_interval_secs`, `cache_refresh_interval_secs`, `auto_warmup`, `token_check_interval_secs`, `switch_threshold`, `notify`, `defer_switch_while_codex_running`, and `log_level`. Its JSON `config` object includes the same settings, including the boolean `defer_switch_while_codex_running`.
- A manual `use` affects only the next Codex process. Finish and exit `codex`, `codex resume`, or `codex exec` sessions first; on Windows, quit the notification-area Codex app rather than only closing its window. Then run `use` and start Codex again. MCP/app-server helper processes alone are not interactive-session blockers for daemon deferral.
- Update checks are manual except for the one check performed when the TUI starts.

## Credential persistence

OAuth credentials are not saved unless both `account_id` and email are present.
When a refresh response rotates its single-use refresh token, the returned
rotation material is first written durably to the private `recovery/` directory.
The profile commit then uses the normal identity and compare-and-swap checks.
Conflicts and failures before a durable profile commit preserve and report that
material. After the profile is durable, the exact stage can be removed even if a
later live-auth activation fails, so that partial commit may report no recovery
path. A failed exact cleanup reports its path only while the original stage
remains there; otherwise the command reports the partial state without claiming
a foreign path or automatically retrying the spent token. Successful JSON
re-authorization of an incomplete legacy alias also returns `archive_path` for
the exact `deleted-profiles/` archive.

For an import whose profile publication is durable but recovery cleanup is not,
the profile remains a successful import and human output prints a warning. JSON
adds optional `cleanup_warning` and `recovery_path` fields; the latter appears
only when the original stage still owns that exact path. If the profile is only
visible and its durability or security is incomplete, import reports a partial
commit instead of claiming either full success or that no profile exists.

Examples:

```bash
codex-switch-global-pace --json list
codex-switch-global-pace --json use work
codex-switch-global-pace --json warmup
codex-switch-global-pace self-update --check
```

## TUI shortcuts

`Enter` opens the scrollable detail and action menu for the selected account; if accounts are marked, it opens the batch menu instead.
The persistent footer keeps the primary `/`, `Enter`, `a`, `r`, `t`, and `h`
actions visible. Its `t auto refresh` label reports `[ON]` or `[OFF]`; open
Help with `h` for the complete shortcut list.

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `Enter` | Open the account menu, or the batch menu when accounts are marked |
| `/` | Filter accounts |
| `r` | Refresh visible accounts |
| `a` | Add a new account |
| `t` | Toggle auto-refresh |
| `W` | Toggle auto-warmup for the short window when present, or the weekly window for a weekly-only response |
| `i` | Toggle the compact quota panel on the main view |
| `s` | Cycle sort order (name / quota / status) |
| `Space` | Mark or unmark an account |
| `u` (account menu) | Switch to the selected account |
| `c` (account menu) | Confirm and consume the earliest-expiring reset card |
| `w` (account menu) | Warm up the selected account |
| `l` (account menu) | Re-login the selected account |
| `n` (account menu) | Rename the selected account |
| `d` (account menu) | Delete the selected account (confirmation required) |
| `r` / `w` / `l` / `d` (batch menu) | Refresh, warm up, re-login, or delete the marked accounts |
| `h` | Show the complete shortcut list |
| `Esc` | Clear filter/marks or close the current popup |
| `q` | Quit |

Destructive or consumptive actions always require confirmation.
Batch re-login skips an incomplete legacy identity with an error; recover that
profile individually so its default-No confirmation and exact credential
archive cannot be bypassed.

## Next steps

- See how these commands combine into workflows in the [Feature guide](Feature-Guide.md).
- Adjust defaults, proxy, and daemon behavior in [Configuration](Configuration.md).
- Check update channels and flags in [Updating](Updating.md).
