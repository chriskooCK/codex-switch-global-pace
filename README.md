# codex-switch-global-pace

**A local multi-account switcher and global weekly pace dashboard for
[OpenAI Codex](https://github.com/openai/codex).** Switch the active Codex
credential among accounts you are authorized to use, and inspect their weekly
usage in one local dashboard.

[한국어 안내](docs/wiki/Korean-Guide.md) · [中文说明](README_CN.md) ·
[Documentation](docs/wiki/Home.md) ·
[Releases](https://github.com/chriskooCK/codex-switch-global-pace/releases)

> **Independent project:** this project is unofficial and is not affiliated
> with or endorsed by OpenAI. Use it only with accounts and workspaces you are
> authorized to access. It does not merge, transfer, share, or bypass service
> quotas; the global meter is only a local aggregate display.

> **Credentials stay sensitive:** this program manages local authentication
> files. Never publish profiles, `auth.json`, tokens, proxy credentials, or
> unredacted debug output.

## Most common workflow: switch the Windows Codex app account

The interactive dashboard is the main day-to-day way to change accounts. The
Windows Codex app can remain in the notification area after its window closes,
so fully stop it before changing the active account.

**Account switch keys:** `↑`/`↓` (or `j`/`k`) → `Enter` → `u`

1. Save your work and close every Codex window.
2. Open the Windows notification area, including the hidden icons behind `^`.
   Find the **ChatGPT** tray icon for the Codex desktop app (some versions may
   label it **Codex**), choose its **Quit** or **Exit** command, and confirm that
   the icon disappears.
3. In PowerShell or Command Prompt, open the interactive dashboard:

   ```powershell
   codex-switch-global-pace
   ```

4. Move to the account you want with `↑`/`↓` or `j`/`k`, press `Enter` to open
   its account menu, and then press `u` (**Use**) to make it active. `Enter` and
   `u` are consecutive actions, not alternative keys.
5. Wait for `Switched to <alias>` to appear, then press `q` to close the
   dashboard.
6. Start the Codex app again. The new process reads the selected account.

### Command-line alternative

After Codex is fully stopped, users who prefer direct commands can refresh the
account view and switch explicitly:

```powershell
codex-switch-global-pace list -f
codex-switch-global-pace use work
```

To let the adaptive scoring algorithm select the best eligible account, omit
the alias:

```powershell
codex-switch-global-pace use
```

> Closing only the app window is not enough if its tray icon remains. Do not
> switch while the Codex Windows tray app or an active Codex session (`codex`,
> `codex resume`, or `codex exec`) is still running. Long-lived MCP or app-server
> helpers are not session processes. For the Codex CLI on Windows, macOS, or
> Linux, finish or terminate each session, run `use <alias>` (or automatic
> `use`), and then start a new session.

## Quick start

### Requirements

- Windows, macOS, or Linux on x64/AMD64 or ARM64. Release artifacts are
  published for all six OS/architecture combinations.
- The Codex Windows app or [Codex CLI](https://github.com/openai/codex), plus at
  least one ChatGPT account that can sign in to Codex.
- A current [GitHub CLI](https://cli.github.com/) with attestation support,
  installed and authenticated to GitHub. Before installing, run `gh --version`,
  `gh auth login` if needed, `gh auth status`, and
  `gh attestation verify --help`.
- Windows PowerShell 5.1 or PowerShell 7 on Windows. On macOS/Linux: Bash,
  `curl`, `tar`, `mktemp`, and either `sha256sum` or `shasum`.

GitHub CLI authentication is used only to download and verify the release; it
is separate from the ChatGPT accounts you add below.

Codex must use its
[file credential store](https://learn.chatgpt.com/docs/auth). If needed, add this to
`$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`; on Windows, normally
`%USERPROFILE%\.codex\config.toml`):

```toml
cli_auth_credentials_store = "file"
```

Install with the source-controlled
[verified bootstrap](docs/wiki/Getting-Started.md#install). It binds the
installer attestation to this repository, the exact Release tag, and its commit
before running the local file. Verification failure stops the installation;
there is no direct-download fallback.

For a typical two-account setup, fully exit Codex first, then add each account
under an explicit alias:

```powershell
codex-switch-global-pace login personal
# In the browser, verify the personal email/workspace before approving.

codex-switch-global-pace login work
# Switch the browser account and verify the work email/workspace before approving.

codex-switch-global-pace list -f
```

A distinct identity authenticated under a new alias becomes the live/current
Codex account as soon as login succeeds. If `work` is a distinct identity, the
sequence above creates and activates `work`. If the browser silently reuses the
personal account, the matching existing profile is updated and activated
instead, and a separate `work` alias is not created. Reauthorizing an inactive
existing alias does not activate it; run `use <alias>` afterward. Use a separate
browser profile or sign out between logins when necessary, then verify the
alias, email, and workspace in `list -f`.

Every OAuth login must provide both a non-empty `account_id` and email before
credentials are saved. If an older profile is missing either identity field,
`login <alias>` does not silently overwrite it: an interactive run asks for
explicit confirmation (default **No**), while JSON or other non-interactive use
requires `--yes`. After approval, the exact previous credentials are archived
under `deleted-profiles/` and the completed login replaces the same alias. Any
known legacy identity field must still match the authenticated account.

Aliases are 1–64 ASCII bytes and may contain only letters, digits, `_`, `-`, and
`.` (`.` and `..` alone are reserved). For example, `personal`, `work`, and
`team-a` are valid; non-ASCII aliases are not.

On Windows, double-clicking `codex-switch-global-pace.exe` opens the same
interactive dashboard. See the full
[Getting started](docs/wiki/Getting-Started.md) guide for installation,
device-code login, imports, and troubleshooting links.

## Global Weekly Pace

The dashboard renders a local aggregate view of every account with a valid
weekly window. The filled bar is aggregate actual usage, and the `↑ pace`
marker is aggregate elapsed time: the ideal amount of the displayed capacity
to have used by now. The text below the meter shows the participating account
count and the nearest included-account reset.

This view does not combine quotas on the service, move capacity between
accounts, or circumvent account limits. Fully exhausted accounts remain
included when their reset timestamp is valid. Accounts whose usage or weekly
reset cannot be trusted are counted as unavailable instead of being guessed.
The current API does not expose a reliable comparable weekly capacity, so
participating accounts are weighted equally.

Quota meters use one relative rule everywhere: yellow means actual usage is
ahead of the elapsed-time pace, while green means usage is at or behind pace.
The Global meter applies the same rule to aggregate usage and aggregate elapsed
time. Exhaustion does not create a third warning state: a valid comparison still
uses yellow or green and keeps its pace marker. Unavailable comparisons stay
neutral, and quota labels do not append warning punctuation.

## Existing profiles and daemon compatibility

The application deliberately reuses the original data directory:

- macOS/Linux: `~/.codex-switch`
- Windows: `%USERPROFILE%\.codex-switch`
- override: `CODEX_SWITCH_HOME`

Existing `profiles/`, `cache.json`, `config.toml`, `current`, and
`daemon-state.json` remain usable without another login. Installers and
uninstallers preserve this shared directory.

When the service rotates a single-use refresh token, the returned rotation
material is durably staged in the private `recovery/` directory before the
profile commit begins. Conflicts and failures before profile durability preserve
and report that material. After the profile is durable, the exact stage can be
removed even if a later live-auth step fails, so that partial commit may report
no recovery path. An exact-cleanup error names a path only while the original
staged file is still proven there; it never claims or deletes an unrelated file,
and it never retries a consumed token.

> Do not run the `codex-switch` daemon and the `codex-switch-global-pace` daemon
> at the same time. They intentionally share profiles, cache, locks, current
> account, and daemon state. Stop and uninstall one daemon service before
> enabling the other. The interactive commands and TUI do not require a daemon.

## Updates and release verification

`self-update` reads releases only from
`chriskooCK/codex-switch-global-pace`. Direct installs and updates verify the
archive checksum and GitHub build provenance against this repository, its
release workflow, the exact tag, and the tag commit. Replacement and daemon
restart failures preserve or restore the prior installation when it is safe to
do so. See [Updating](docs/wiki/Updating.md) for the verification model,
rollback behavior, release channels, and uninstall instructions.

```bash
codex-switch-global-pace self-update --check
codex-switch-global-pace self-update
codex-switch-global-pace self-update --dev
codex-switch-global-pace self-update --stable
```

## Development

Requires Rust 1.88 or newer:

```bash
cargo fmt --check
cargo test --all --locked --features test-endpoints
cargo clippy --all-targets --locked --features test-endpoints -- -D warnings
```

See the [developer onboarding](docs/wiki/Developer-Onboarding.md),
[architecture](docs/wiki/Architecture-Overview.md), and
[release process](docs/RELEASE.md).

## Origin and license

This is an independent project created from a one-time snapshot of
`xjoker/codex-switch`; it does not track or automatically synchronize with that
repository. See [NOTICE.md](NOTICE.md) for the exact source revision and
attribution.

[MIT](LICENSE) — the original copyright and license notice are retained.
