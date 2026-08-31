# Troubleshooting

Start with the complete error message, its file path, and the command that produced it. Configuration, login, update, and permission failures include the path or next command when recovery is known.

| Symptom | Action |
|---|---|
| No saved profiles | Run `codex-switch-global-pace login` or `codex-switch-global-pace import <path>`. |
| Credential store is not file-backed | Set `cli_auth_credentials_store = "file"` in `$CODEX_HOME/config.toml`. |
| Headless login cannot open a browser | Run `codex-switch-global-pace login --device`. |
| Device login says the flow is disabled or unauthorized | Enable device-code login in the ChatGPT personal Security settings. For a managed workspace, ask its administrator to allow device-code authentication. Then start a new `login --device` attempt. |
| Browser login succeeds in the page but the terminal keeps waiting | Permit loopback connections to `127.0.0.1` on ports 1455 and 1457, close another process using those ports, and retry. If local callbacks remain blocked, use `login --device`. |
| `login <alias>` reports incomplete legacy identity | Run it interactively and approve the default-No prompt only after verifying the alias, or use `login <alias> --yes` for an intentional non-interactive run. The authenticated result must contain `account_id` and email and match every known legacy identity field; approval archives the exact old credentials under `deleted-profiles/` before replacing the same alias. |
| The wrong browser account was added | Do not reauthorize a complete existing alias as another identity; that is rejected. An incomplete legacy alias uses the explicit confirmation and archive procedure above. Follow [Correct a wrong browser account](#correct-a-wrong-browser-account). |
| Windows daemon installation is denied | Open PowerShell as Administrator and retry. |
| Switching or restore fails while the Windows Codex app looks closed | Closing the window can leave Codex in the notification area. Quit it from the system-tray menu and confirm in Task Manager that no Codex process remains, then retry. |
| Windows daemon stop says credential work is still in flight | Wait briefly and run `codex-switch-global-pace daemon stop` again. The process is intentionally left running instead of force-killed while a refresh token may be rotating. |
| TUI layout is broken in Git Bash | Use Windows Terminal or PowerShell. |
| macOS/Linux self-update reports that the install directory is not writable | Rerun the current installer once to migrate a legacy `/usr/local/bin` direct install to `$HOME/.local/bin`; see [Updating](Updating.md#install-locations-and-migration). Use `sudo codex-switch-global-pace self-update` only for an intentional `--system` install. |
| A dev build should return to stable | Run `codex-switch-global-pace self-update --stable`. |
| Self-update reports that `gh attestation verify` is unavailable | Install or upgrade [GitHub CLI](https://cli.github.com/), then retry. Direct self-update fails closed until it can verify the release provenance bundle. |
| An installed daemon still uses an earlier `CODEX_SWITCH_HOME` | Service installation captures the resolved state path. Run `daemon uninstall`, set the new `CODEX_SWITCH_HOME`, then run `daemon install` again. See [Configuration](Configuration.md#platform-integration). |
| The daemon says another daemon is running | `codex-switch` and `codex-switch-global-pace` share the daemon PID and state files. Stop and uninstall the other service; do not run both simultaneously. |
| HTTPS fails with `invalid peer certificate: UnknownIssuer` | An intercepting proxy is re-signing traffic. See [HTTPS fails with an unknown issuer](#https-fails-with-invalid-peer-certificate-unknownissuer). |
| An account reports `re-login required (refresh_token_reused)` | The stored refresh token was already spent and cannot be recovered. Run `codex-switch-global-pace login <alias>` for that profile. The verdict is remembered, so the account costs no further requests until you sign in again; `codex-switch-global-pace list -f` asks the server anyway. |
| Refresh reports a preserved `recovery/` path, superseded credential, incomplete commit, or incomplete cleanup | Do not move a named recovery file into `profiles/` or immediately retry the consumed refresh token. Keep any reported file private, resolve the exact local error, and inspect the named profile state first. Cleanup-only errors may report no path when the app cannot prove that the original stage still owns that name; it will not label a foreign replacement as the recovery credential. |
| Import reports preserved rotation material, an incomplete profile commit, or incomplete recovery cleanup | The server replaced the source file's one-time token before import fully settled. Do not retry that consumed source. Keep any exactly reported `$CODEX_SWITCH_HOME/recovery/` file private and inspect whether the profile is absent, merely visible with incomplete durability/security, or durably created with cleanup still pending. A path is omitted when the original stage no longer owns it. Recovery files are deliberately not selectable profiles. |

For network or API failures, rerun the smallest failing command with `--debug`:

```bash
codex-switch-global-pace --debug list
codex-switch-global-pace --debug self-update --check
```

Debug output can contain account or infrastructure identifiers. Before opening an issue, remove tokens, email addresses, account IDs, workspace names, filesystem paths that reveal identity, and proxy credentials.

## Browser and device-code login

Browser login opens the system browser and waits for a loopback PKCE callback.
The listener tries `127.0.0.1:1455`, then `127.0.0.1:1457`; it does not listen on
the LAN. Keep the initiating terminal open until the command confirms the
profile. A browser success page alone does not prove that the local token
exchange finished.

If either port is reserved by another login, a local development server, VPN,
endpoint-security product, or Windows networking service, close the conflicting
process or allow those loopback ports through the local firewall. Do not forward
the callback port to another host. `login --device` avoids the listener and is
the preferred fallback for SSH, containers, WSL without browser integration,
and locked-down desktops:

```bash
codex-switch-global-pace login --device <alias>
```

Device-code login is a beta Codex sign-in method. It must be enabled in the
ChatGPT personal Security settings, or allowed by the administrator of a
managed workspace. Open only the verification URL printed by the command,
confirm the intended account in the browser, and never send the short-lived code
to another person. If the attempt expires, start a new command rather than
reusing the code. See the
[official Codex authentication documentation](https://learn.chatgpt.com/docs/auth)
for the current account and workspace controls.

If the browser step completes but token exchange reports TLS or proxy errors,
the callback worked; diagnose the terminal-side HTTPS request with the next
section. Repeating browser approval does not fix a missing corporate CA.

## Correct a wrong browser account

First run `codex-switch-global-pace list -f` and compare the alias, email, and
workspace. An alias is only a local label; the browser identity determines the
account that was authenticated.

If a requested new alias was not created because the browser reused an identity
that already had a saved profile, correct or sign out that browser session and
run `login <new-alias>` again. If the wrong identity was actually saved under
the alias you wanted, a complete existing-alias login will reject a different
account rather than overwrite credentials. An incomplete legacy alias instead
requires the default-No/`--yes` confirmation and exact `deleted-profiles/`
archive described above. Preserve a complete mistaken profile under a temporary
alias, then create the intended one:

Before running these commands, finish every active Codex CLI session. On
Windows, also [fully quit the Codex tray application](#fully-quit-codex-on-windows).
The successful `login` publishes that account as live authentication
immediately.

```bash
codex-switch-global-pace rename work work-wrong
# Sign out or select the intended account in a separate/private browser profile.
codex-switch-global-pace login work
codex-switch-global-pace list -f
```

The successful distinct login activates `work`, so `work-wrong` is now inactive
and can be deleted recoverably after you verify both identities:

```bash
codex-switch-global-pace use work
codex-switch-global-pace delete work-wrong
```

Choose another valid temporary alias if `work-wrong` already exists. Never
delete the mistaken profile until the intended login is verified.

## Fully quit Codex on Windows

Account switching changes the credential used by the **next** Codex process.
Before restoring data, recovering a deleted profile, or diagnosing a file-in-use
error on Windows:

1. Save work in every Codex task and close Codex CLI and IDE terminal sessions.
2. Open the taskbar's hidden notification icons, right-click **ChatGPT** for the
   Codex desktop app (or **Codex** if that is the label shown), and choose its
   Quit or Exit action. Closing only the main window may leave the app running
   in the tray.
3. Open Task Manager and confirm that no Codex application process remains. End
   a leftover process only after its work is saved.
4. Stop the separate codex-switch-global-pace Beta daemon with
   `codex-switch-global-pace daemon stop` when the recovery procedure says to.

The Codex desktop app and the optional codex-switch-global-pace daemon are
different processes; quitting one does not stop the other.

## HTTPS fails with `invalid peer certificate: UnknownIssuer`

An intercepting proxy — a debugging tool such as Proxyman or Charles, or a
corporate MITM gateway — presents its own certificate instead of the real one.
The browser and `curl` accept it because its CA is installed in the operating
system, so only `codex-switch-global-pace` appears to be broken.

`codex-switch-global-pace` reads the OS trust store, so installing the proxy's CA there is
normally enough. Reaching this error means the CA is missing from that store, or
is trusted only for the current user in a way the store does not expose. Point at
the certificate explicitly:

```bash
# macOS: export the CA, substituting the name shown in Keychain Access
codex_switch_home="${CODEX_SWITCH_HOME:-$HOME/.codex-switch}"
mkdir -p -- "$codex_switch_home"
security find-certificate -c "Proxyman CA" -p > "$codex_switch_home/proxy-ca.pem"
export CODEX_CA_CERTIFICATE="$codex_switch_home/proxy-ca.pem"
```

Set the variable in the shell profile so the TUI and the daemon inherit it, not
just the current shell. `SSL_CERT_FILE` works as a fallback in the same order
Codex itself uses. Turning off interception is equally valid when a capture is
not needed.

The failure is intermittent when the proxy only intercepts part of the time, so
the same command can succeed minutes later. Login is affected the same way: the
browser step completes while the token exchange behind it fails, which looks like
a rejected sign-in rather than a certificate problem.

## Recover a deleted profile

Deletion moves an inactive profile into recoverable storage rather than erasing
it. First quit Codex completely, including the Windows tray application, and
ensure no login, import, switch, or refresh is running. Replace only the example
alias value below; each block chooses the newest exact matching archive, refuses
to overwrite an existing profile, moves it back, and activates it.

macOS/Linux:

```bash
set -eu
alias_name='work'
state_home="${CODEX_SWITCH_HOME:-$HOME/.codex-switch}"
codex-switch-global-pace daemon stop
profiles="$state_home/profiles"
deleted="$state_home/deleted-profiles"
[ -d "$profiles" ] || {
  printf 'Profiles directory is missing: %s\n' "$profiles" >&2
  exit 1
}
[ -d "$deleted" ] || {
  printf 'Deleted-profile directory is missing: %s\n' "$deleted" >&2
  exit 1
}
destination="$state_home/profiles/$alias_name"
[ ! -e "$destination" ] || {
  printf 'Refusing to overwrite existing profile: %s\n' "$destination" >&2
  exit 1
}
archive=
for candidate in "$deleted/${alias_name}.backup-"*; do
  [ -d "$candidate" ] || continue
  suffix="${candidate#"$deleted/${alias_name}.backup-"}"
  case "$suffix" in ""|*[!0-9]*) continue ;; esac
  if [ -z "$archive" ] || [[ "${candidate##*/}" > "${archive##*/}" ]]; then
    archive="$candidate"
  fi
done
[ -n "$archive" ] || {
  printf 'No deleted archive found for alias %s.\n' "$alias_name" >&2
  exit 1
}
mv -- "$archive" "$destination"
codex-switch-global-pace list
codex-switch-global-pace use "$alias_name"
```

Windows PowerShell:

```powershell
$ErrorActionPreference = "Stop"
$Alias = "work"
$StateHome = if ([string]::IsNullOrWhiteSpace($env:CODEX_SWITCH_HOME)) {
    Join-Path $HOME ".codex-switch"
} else {
    $env:CODEX_SWITCH_HOME
}
codex-switch-global-pace daemon stop
if ($LASTEXITCODE -ne 0) { throw "Daemon stop failed; no profile was moved." }
$Profiles = Join-Path $StateHome "profiles"
$Deleted = Join-Path $StateHome "deleted-profiles"
if (-not (Test-Path -LiteralPath $Profiles -PathType Container)) {
    throw "Profiles directory is missing: $Profiles"
}
if (-not (Test-Path -LiteralPath $Deleted -PathType Container)) {
    throw "Deleted-profile directory is missing: $Deleted"
}
$Destination = Join-Path $Profiles $Alias
if (Test-Path -LiteralPath $Destination) {
    throw "Refusing to overwrite existing profile: $Destination"
}
$ArchivePrefix = "$Alias.backup-"
$Archive = Get-ChildItem -LiteralPath $Deleted -Directory |
    Where-Object {
        $_.Name.StartsWith($ArchivePrefix, [StringComparison]::Ordinal) -and
        $_.Name.Substring($ArchivePrefix.Length) -match '\A[0-9]+\z'
    } |
    Sort-Object -Property Name -Descending |
    Select-Object -First 1
if ($null -eq $Archive) {
    throw "No deleted archive found for alias '$Alias'."
}
Move-Item -LiteralPath $Archive.FullName -Destination $Destination
codex-switch-global-pace list
if ($LASTEXITCODE -ne 0) { throw "Restored profile validation failed." }
codex-switch-global-pace use $Alias
if ($LASTEXITCODE -ne 0) { throw "Restored profile activation failed." }
```

Start a new Codex process and verify the intended account. If `list` rejects the
restored credential as expired or already used, keep the archive private and run
`codex-switch-global-pace login <alias>`; do not overwrite it with an older live
backup. A file under `recovery/` is a separate token-rotation recovery stage and
must not be moved into `profiles/` directly. Refresh identity or managed-policy
rejections are quarantined; import operational failures use the neutral
"preserved rotation material" wording. Other files can represent superseded,
commit-incomplete, or cleanup-incomplete states.

## Reset-card outcome is uncertain

If a reset-card request reports that consumption may have occurred, do not immediately retry. Refresh the account state and verify the card count and quota first. This warning means the request reached the service but the client could not prove the final result.

## Report an issue

Include the operating system, terminal, `codex-switch-global-pace --version`, exact command, expected behavior, actual behavior, and redacted diagnostic output. Use the [GitHub issue tracker](https://github.com/chriskooCK/codex-switch-global-pace/issues). Report suspected vulnerabilities only through [GitHub private vulnerability reporting](https://github.com/chriskooCK/codex-switch-global-pace/security/advisories/new), never a public issue.

## Next steps

- Check short behavior and security answers in the [FAQ](FAQ.md).
- Review paths and settings in [Configuration](Configuration.md).
- If the problem remains, report the redacted reproduction in the [GitHub issue tracker](https://github.com/chriskooCK/codex-switch-global-pace/issues).
