# Feature guide

`codex-switch-global-pace` manages multiple file-backed Codex logins, observes
their quota state, and selects the account used by the next Codex process. It is
an independent, unofficial project and is not affiliated with or endorsed by
OpenAI. Use it only with accounts you own or are authorized to use.

> **Authentication prerequisite:** Codex must use the file credential store. Set `cli_auth_credentials_store = "file"` in `$CODEX_HOME/config.toml`. Explicit `keyring`, `auto`, and `ephemeral` stores are rejected because they can bypass the `auth.json` file that codex-switch-global-pace switches.

Global Weekly Pace is a local visualization. It does not transfer, merge, or
bypass quota between accounts; switching only changes the local `auth.json`
that a newly started Codex process reads.

## Everyday workflow: switch the active Codex account

The most common operation is choosing which saved account the next Codex
session will use. Codex reads authentication when the process starts, so close
Codex completely before switching and start it again afterwards.

### Codex Windows app

1. Finish or save work in the current Codex session.
2. Quit the Codex window.
3. In the Windows notification area, open the hidden tray icons, right-click
   **ChatGPT** for the Codex desktop app (or **Codex** if that is the label
   shown), and choose **Quit** or **Exit**. A closed window can leave the Codex
   background process running, so do not switch until the tray icon is gone.
4. Switch explicitly, or let the selector choose the best eligible account:

   ```powershell
   codex-switch-global-pace list -f
   codex-switch-global-pace use work
   # or: codex-switch-global-pace use
   ```

5. Start the Codex Windows app again. The new process now reads the selected
   account from `$CODEX_HOME/auth.json`.

The same rule applies to Codex CLI sessions: exit every running `codex`,
`codex resume`, or `codex exec` session process, switch, and then start a new
process. Long-lived app-server or MCP helpers are not session processes, but
closing the Windows app through its tray menu is the clearest safe boundary for
a manual switch.

## Manage accounts

Add each account under a distinct alias. For a normal two-account setup:

```bash
codex-switch-global-pace login personal
# Confirm that the browser shows the intended personal email and workspace.

codex-switch-global-pace login work
# Change the browser session if necessary, then confirm the work identity.

codex-switch-global-pace list -f
codex-switch-global-pace login --device server
```

Authenticating a new identity under a new alias saves and immediately activates
that profile. If the authenticated identity already belongs to a saved profile,
the matching profile is updated and activated instead, and the requested new
alias is not created. Reusing a complete existing alias re-authorizes only that
same identity; if it was inactive, run `use <alias>` afterwards to activate it.
Aliases are 1–64 ASCII bytes and may contain only letters, numbers, `_`, `-`,
and `.`; `.` and `..` are not valid aliases. If the browser is still signed in
to the first account, change accounts before approving the second login and
verify the resulting identities with `list -f`.

Existing `auth.json` files can be imported individually or from a directory. Imports are validated in stages — JSON format, required token structure with a decodable `id_token`, then a live usage-service check — before being saved under collision-free aliases:

```bash
codex-switch-global-pace import ~/auth-backups
```

Every new OAuth save requires both a non-empty `account_id` and email. For a
complete existing profile, interactive login updates it only when both fields
match exactly. Re-login releases the target profile while you complete browser
or device-code authorization, so quota reads and other account maintenance can
continue; commit reacquires the profile and verifies that its strict identity
did not change in the meantime.

An older profile that lacks either identity field follows a separate,
recoverable migration. `login <alias>` asks for explicit confirmation with a
default of **No**; JSON and other non-interactive runs require `--yes` before
OAuth starts. The completed login must include both identity fields and match
every identity field already known by the legacy profile. The app then archives
the exact previous credentials under `deleted-profiles/` before replacing that
same alias. If the profile changed during OAuth or the authenticated identity is
already owned by another profile, neither archival nor replacement proceeds.

Import is deliberately create-only and never updates an existing profile:
Usage API validation proves that the bearer can access a workspace, but a Team
workspace ID can be shared by several users and cannot authorize overwriting
another saved credential.

If a rotated import profile is durably created but its recovery stage cannot be
cleaned exactly, the import remains successful and reports a cleanup warning.
It reports `recovery_path` only when the original stage still owns that name. A
profile publication that is visible but not durably or securely confirmed is a
partial commit, not a false "profile absent" result.

A complete existing alias cannot be reauthorized as a different account. The
incomplete-legacy migration above can prove only the identity fields it already
knows and therefore requires confirmation plus an exact archive. If the wrong
browser identity was saved, follow
[Correct a wrong browser account](Troubleshooting.md#correct-a-wrong-browser-account)
rather than trying to overwrite the alias.

Profile deletion is recoverable. An inactive profile is moved under `deleted-profiles/` after confirmation; the active profile cannot be deleted. See [recovery instructions](Troubleshooting.md#recover-a-deleted-profile).

## External login detection

Explicit interactive CLI commands such as `list` compare the live
`$CODEX_HOME/auth.json` against saved profiles before doing their own work:

- A new account (for example after a plain `codex login`) triggers an offer to save it as a profile.
- A refreshed token for a known account triggers an offer to update that profile.
- A non-interactive human-output run reports the mismatch but does not save it.
- JSON output skips live-auth reconciliation, lists saved profiles only, and
  does not report or save an untracked live account. Run `list` interactively
  to review and approve the live credentials.

The no-argument TUI deliberately does not open a plain-terminal confirmation
prompt during startup. It reports an untracked live account and offers the `a`
account-login action; run `codex-switch-global-pace list` interactively when you
want to save an existing external `codex login` result without logging in again.

When the current marker, its saved profile, and live auth are already identical,
this check reads only that profile instead of scanning every saved account.

## Observe quota and account state

Use the CLI for scripts and quick inspection, or the TUI for an interactive dashboard:

```bash
codex-switch-global-pace list
codex-switch-global-pace --json list
codex-switch-global-pace
```

The usage model includes the main short and weekly windows returned for each
account, additional model-specific pools, reset cards, spend limits, account
restrictions, and model capabilities returned by the authenticated service.
Cached usage entries are scoped by profile alias plus the account's verified ID
and email, carry an exact revision, and retain their own fetch time. Workspace
metadata is keyed by account ID, so aliases for the same verified account share
the same cached organization result.

Normal reads refresh only stale entries. Use `list -f` or the TUI refresh action when a fresh network read is required.

When the service issues a replacement for a single-use refresh token, the app
must first durably stage the returned rotation material under the private
`recovery/` directory. Only then can it attempt the identity-bound profile
commit. Conflicts and failures before that profile is durable preserve and
report the staged material. Once the profile is durable, cleanup removes the
exact staged file by its original file identity; it never unlinks an unrelated
replacement at the same path. A later live-auth activation can still fail after
that cleanup, so the resulting partial commit may report no recovery path.
Failed exact cleanup reports a path only when the original staged file is still
proven there; a replaced or unverifiable name is reported as partial cleanup
without being mislabeled as the rotated credential. No failure path
automatically retries a token the service already consumed.

The Global Weekly Pace box visualizes every account with a valid weekly window
as one equal-weight pool. This is only an aggregate display: quota remains
separate on each account. Its filled bar is aggregate actual usage, while the
`↑ pace` marker is aggregate elapsed time and therefore the ideal usage position
for the current point in the weekly windows. The summary text shows the
participating account count and nearest included-account reset. A fully
exhausted account is still included when its reset timestamp is valid. Missing,
expired, inconsistent, or failed weekly data is counted as unavailable instead
of being guessed.

Every quota meter uses the same relative state: yellow when actual usage is
ahead of elapsed-time pace, and green when usage is at or behind pace. The
Global meter compares aggregate usage with aggregate elapsed time in exactly
the same way. Exhaustion does not create a third warning state: a valid
comparison still uses yellow or green and keeps its pace marker. Unavailable
comparisons are neutral, and quota labels carry no warning suffix.

The TUI account detail page is a single scrollable column with identity and organization labels, token expiry times in the local timezone, every quota pool with a pace marker, available reset cards, and the models the account may use. On startup it requests the selected and active account first and shows core quota before workspace and reset-card metadata. Workspace metadata and selected-account models proceed when that account's own core credential boundary is complete, rather than waiting for every other account. Model names and reasoning-effort capabilities are discovered from the authenticated service at runtime, not hardcoded. The persistent footer reports auto-refresh as `[ON]` or `[OFF]` and keeps only the primary actions visible; the full shortcut list is in the [command reference](Command-Reference.md#tui-shortcuts) and under `h` inside the TUI.

## Select an account

Select an explicit profile:

```bash
codex-switch-global-pace use work
```

Or let the adaptive selector rank all profiles:

```bash
codex-switch-global-pace use
```

Selection has two phases:

1. **Eligibility** requires a valid weekly window and excludes candidates with an exhausted reported window, critically low weekly headroom with a distant reset, or an unsafe Free-plan balance.
2. **Scoring** ranks the eligible candidates by tier preference (Team accounts get priority by default), optional short-window headroom, weekly sustainability, quota that is close to resetting, and recent use.

Fresh quota probes are saved before scoring, so a later automatic selection can reuse them without another quota request. These quota-only entries are deliberately not exposed as complete reset-card cache data; an exhausted pool enriches them before checking for recovery cards. A fresh complete entry already carries authoritative card details and avoids that extra request, while an approved redemption always performs a forced identity-bound preflight. If another process updates the same account while probes are running, its newer generation is preserved and the candidates are scored again from that result.

If every account is ineligible, the best fallback is reported instead of pretending an account is healthy.

Switching replaces the live `$CODEX_HOME/auth.json` atomically while holding the app's credential transaction. Ordinary CLI and TUI switches are bound to the exact live and profile snapshots observed for that decision. An interactive untracked-live overwrite prompt does not retain the target profile lease; after approval, the target and live snapshots plus the planned strict identity are revalidated under a newly acquired lease before any switch or reset-card request. The TUI renders before reconciling live credentials, then saves newer credentials in the background only when the live account strictly matches an existing profile and passes the normal freshness and rollback guards; an untracked live account is reported without being saved. In the TUI, `Enter` then `u` is the complete switch action. If Codex refreshed the currently tracked account, its newer live credentials are saved through the same identity and freshness gate used by explicit profile synchronization before the selected profile is activated. A genuinely untracked live login is never overwritten by that action. Reset-card auto-selection authorizes the exact live snapshot and the target account identity before redemption, then permits only a same-account token rotation during the network request. The final publication rechecks the authorized live bytes and refuses a detected change instead of deliberately overwriting it. Fully quit the Codex Windows app, including its tray icon, or exit all Codex CLI session processes before a manual switch; start Codex again only after the switch succeeds.

## Recover exhausted accounts

When the whole candidate pool is exhausted, an interactive `use` can offer to consume the earliest-expiring reset card. Automation must opt in explicitly:

```bash
codex-switch-global-pace use --consume-card
codex-switch-global-pace reset-card work --yes
```

JSON or non-interactive execution never consumes a card without the explicit flag. Even with `--consume-card`, an untracked live login is rejected before the redemption request because non-interactive execution cannot approve overwriting it.

## Warm quota windows

Fresh accounts show no reset timer until their first real request. `warmup` sends minimal requests to activate inactive main and model-specific quota windows discovered from the official model response:

```bash
codex-switch-global-pace warmup
codex-switch-global-pace warmup work
codex-switch-global-pace --json warmup
```

Model names are discovered at runtime rather than maintained as a hardcoded compatibility list. Read-only model discovery does not retain the profile lease; warmup reacquires it and verifies the exact credential and token freshness before sending a quota-activating request. Its model cache belongs to the verified account and normalized quota-pool set, not merely the alias, and the TUI can cancel discovery before cache publication or quota activation. Already-active or unavailable pools are skipped. JSON mode returns every profile result and a top-level `ok` field. Inside the TUI, `W` toggles automatic warmup: it targets the short window when present, or the weekly window for a weekly-only response. The daemon has a separate `auto_warmup` setting.

## Run the background daemon

The Beta daemon monitors the current profile, refreshes cached usage and expiring tokens, and prepares a better account when the configured threshold is reached.

```bash
codex-switch-global-pace daemon install
codex-switch-global-pace daemon status
```

Service integration is platform-native: LaunchAgent on macOS, a systemd user service on Linux, and Task Scheduler on Windows. Windows installation requires elevated PowerShell.

The data directory and daemon-state files remain compatible with the original
`codex-switch`. Do not install or run both daemon services simultaneously;
stop and uninstall one daemon before enabling the other.

The daemon runs three independent timers: account polling (`poll_interval_secs`), full cache refresh with optional warmup (`cache_refresh_interval_secs`, `auto_warmup`), and proactive token refresh (`token_check_interval_secs`). With at least two profiles, `switch_threshold` starts a candidate search from the current profile's short-window usage when present; a weekly-only response uses its weekly window instead. An unavailable, account-limited, or exhausted current account is treated as 100% for this trigger so a low short-window value cannot suppress recovery. Reaching the threshold does not force a credential change: the unified eligibility and scoring rules must still find a strictly better candidate.

By default, a switch is deferred while the same Windows user's main Codex tray app or a Codex session process (`codex`, `codex resume`, or `codex exec`) is running; the daemon records the pending switch and retries on the next poll. Other Windows users' processes and renderer, MCP, or app-server helpers do not block a switch. An ambiguous same-user candidate remains fail-closed. Operational state lives in `daemon-state.json` and is shown by `daemon status`. Daemon switching uses the same compare-and-switch boundary: if the current marker no longer matches the live credentials observed by the poll — including when live authentication becomes untracked — the commit is refused and a later poll starts from fresh state.

## Update the binary

Direct installs support the stable and rolling development channels, verify release checksums and build provenance before replacing the binary, and retain the exact previous executable until a running daemon has restarted successfully. See [Updating](Updating.md) for channels and installation migration, and [Testing development releases](Development-Releases.md) for the dev channel.

```bash
codex-switch-global-pace self-update --check
codex-switch-global-pace self-update
```

## Automate safely

Most non-interactive commands support `--json` or `--json-pretty`. Structured output stays on stdout; progress and diagnostic messages use stderr. A JSON `list` does not auto-save an untracked live login. Commands that can consume a reset card or delete a profile require explicit non-interactive confirmation.

Never publish profile files, `auth.json`, unredacted debug output, proxy credentials, account IDs, email addresses, or workspace names.

## Next steps

- Need an exact command, flag, or TUI shortcut? Open the [Command reference](Command-Reference.md).
- Tune paths, proxy, and daemon behavior in [Configuration](Configuration.md).
- Something failed? Start with [Troubleshooting](Troubleshooting.md).
