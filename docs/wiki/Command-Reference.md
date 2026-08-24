# Command reference

The installed binary remains authoritative: use `codex-switch-global-pace --help` and `codex-switch-global-pace <command> --help` for the exact flags and examples supported by your version.

Running `codex-switch-global-pace` with no subcommand opens the TUI. This also
applies when the Windows executable is launched directly.

## Commands

| Command | Purpose |
|---|---|
| `login [--device] [alias]` | Add or reauthorize a profile through browser PKCE or device-code login. If the alias already exists, it is reauthorized; otherwise a new profile is created. |
| `import <path> [alias]` | Validate and import one `auth.json`, or recursively scan a directory for JSON files. The alias applies to single-file imports only; directories auto-assign aliases. |
| `list [-f]` | Show profiles, usage, and availability; `-f` / `--force` bypasses the cache. |
| `use [alias] [--consume-card]` | Switch explicitly, or omit the alias to auto-select with the unified scoring algorithm. When the pool is exhausted, `--consume-card` consumes the earliest-expiring reset card to revive an account (auto-select only; ignored when an alias is given). |
| `reset-card <alias> [-y]` | Consume the earliest-expiring reset card for a profile after confirmation; `-y` / `--yes` skips the prompt. |
| `warmup [alias]` | Send a minimal request to activate the quota-window countdown for one or all profiles. |
| `rename <old> <new>` | Rename a saved profile. |
| `delete <alias> [-y]` | Move an inactive profile into recoverable deleted storage; `-y` / `--yes` skips the prompt. |
| `daemon start [--foreground]` | Start the Beta daemon, detached by default; `--foreground` is for service managers. |
| `daemon stop` | Stop a running Beta daemon. |
| `daemon status` | Report daemon support, service, process, configuration, and pending-switch state. |
| `daemon install` | Install the native user service: LaunchAgent on macOS, systemd on Linux, Task Scheduler on Windows (elevated PowerShell required). |
| `daemon uninstall` | Remove the native user service. |
| `self-update [--check] [--dev\|--stable] [--version <VERSION>]` | Check or update a direct installation. Without flags it stays on the current channel; `--version` installs a specific newer stable version and conflicts with the channel flags. |
| `open` | Open the codex-switch-global-pace data directory in the platform file manager. |

## Global options

| Option | Environment variable | Behavior |
|---|---|---|
| `--json` | — | Compact structured output (supported by `list`, `use`, `reset-card`, `rename`, `delete`, `login`, `import`, `self-update`, `daemon status`). |
| `--json-pretty` | — | Indented structured output. |
| `--proxy <URL>` | `CS_PROXY` | Override proxy configuration for this process; supports `http(s)://`, `socks4://`, `socks5://`, and `socks5h://` (remote DNS). |
| `--color <auto\|always\|never>` | `CS_COLOR` | Control terminal color. `NO_COLOR` disables color regardless of this option. |
| `--debug` | — | Emit diagnostic information (HTTP requests, API responses, cache status) to stderr; redact it before sharing. |
| `-V`, `--version` | — | Print the binary version. |

## Automation contract

- Structured data is written to stdout; progress and diagnostics are written to stderr.
- JSON and other non-interactive execution never consumes a reset card or deletes a profile without an explicit opt-in flag.
- A manual `use` affects the next Codex process. Restart an already-running Codex process to load the new `auth.json`.
- Update checks are manual except for the one check performed when the TUI starts.

Examples:

```bash
codex-switch-global-pace --json list
codex-switch-global-pace --json use work
codex-switch-global-pace self-update --check
```

## TUI shortcuts

`Enter` opens the scrollable detail and action menu for the selected account; if accounts are marked, it opens the batch menu instead.

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `Enter` | Open the account menu, or the batch menu when accounts are marked |
| `/` | Filter accounts |
| `r` | Refresh visible accounts |
| `a` | Add a new account |
| `t` | Toggle auto-refresh |
| `W` | Toggle auto-warmup for accounts whose 5h window has expired |
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

## Next steps

- See how these commands combine into workflows in the [Feature guide](Feature-Guide.md).
- Adjust defaults, proxy, and daemon behavior in [Configuration](Configuration.md).
- Check update channels and flags in [Updating](Updating.md).
