# Troubleshooting

Start with the complete error message, its file path, and the command that produced it. Configuration, login, update, and permission failures include the path or next command when recovery is known.

| Symptom | Action |
|---|---|
| No saved profiles | Run `codex-switch-global-pace login` or `codex-switch-global-pace import <path>`. |
| Credential store is not file-backed | Set `cli_auth_credentials_store = "file"` in `$CODEX_HOME/config.toml`. |
| Headless login cannot open a browser | Run `codex-switch-global-pace login --device`. |
| Windows daemon installation is denied | Open PowerShell as Administrator and retry. |
| Windows daemon stop says credential work is still in flight | Wait briefly and run `codex-switch-global-pace daemon stop` again. The process is intentionally left running instead of force-killed while a refresh token may be rotating. |
| TUI layout is broken in Git Bash | Use Windows Terminal or PowerShell. |
| macOS/Linux self-update reports that the install directory is not writable | Rerun the current installer once to migrate a legacy `/usr/local/bin` direct install to `$HOME/.local/bin`; see [Updating](Updating.md#install-locations-and-migration). Use `sudo codex-switch-global-pace self-update` only for an intentional `--system` install. |
| A dev build should return to stable | Run `codex-switch-global-pace self-update --stable`. |
| Self-update reports that `gh attestation verify` is unavailable | Install or upgrade [GitHub CLI](https://cli.github.com/), then retry. Direct self-update fails closed until it can verify the release provenance bundle. |
| An installed daemon still uses an earlier `CODEX_SWITCH_HOME` | Service installation captures the resolved state path. Run `daemon uninstall`, set the new `CODEX_SWITCH_HOME`, then run `daemon install` again. See [Configuration](Configuration.md#platform-integration). |
| The daemon says another daemon is running | `codex-switch` and `codex-switch-global-pace` share the daemon PID and state files. Stop and uninstall the other service; do not run both simultaneously. |
| HTTPS fails with `invalid peer certificate: UnknownIssuer` | An intercepting proxy is re-signing traffic. See [HTTPS fails with an unknown issuer](#https-fails-with-invalid-peer-certificate-unknownissuer). |
| An account reports `re-login required (refresh_token_reused)` | The stored refresh token was already spent and cannot be recovered. Run `codex-switch-global-pace login <alias>` for that profile. The verdict is remembered, so the account costs no further requests until you sign in again; `codex-switch-global-pace list -f` asks the server anyway. |
| Import reports a quarantined rotated credential | The server replaced the source file's one-time token before identity or managed-policy validation failed. Keep the named file under `~/.codex-switch/recovery/` private, sign in again, then remove it only after the account works. Recovery files are deliberately not selectable profiles. |

For network or API failures, rerun the smallest failing command with `--debug`:

```bash
codex-switch-global-pace --debug list
codex-switch-global-pace --debug self-update --check
```

Debug output can contain account or infrastructure identifiers. Before opening an issue, remove tokens, email addresses, account IDs, workspace names, filesystem paths that reveal identity, and proxy credentials.

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
security find-certificate -c "Proxyman CA" -p > ~/.codex-switch/proxy-ca.pem
export CODEX_CA_CERTIFICATE=~/.codex-switch/proxy-ca.pem
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

Deletion moves an inactive profile into recoverable storage rather than erasing it. Stop the daemon, move the newest matching directory back into `profiles/`, and confirm that it appears:

```bash
codex-switch-global-pace daemon stop
# Move deleted-profiles/<alias>.backup-<timestamp> to profiles/<alias>
codex-switch-global-pace list
```

The base directory is `~/.codex-switch`, `%USERPROFILE%\.codex-switch` on Windows, or the value of `CODEX_SWITCH_HOME`.

## Reset-card outcome is uncertain

If a reset-card request reports that consumption may have occurred, do not immediately retry. Refresh the account state and verify the card count and quota first. This warning means the request reached the service but the client could not prove the final result.

## Report an issue

Include the operating system, terminal, `codex-switch-global-pace --version`, exact command, expected behavior, actual behavior, and redacted diagnostic output. Use the [GitHub issue tracker](https://github.com/chriskooCK/codex-switch-global-pace/issues).

## Next steps

- Check short behavior and security answers in the [FAQ](FAQ.md).
- Review paths and settings in [Configuration](Configuration.md).
- If the problem remains, report the redacted reproduction in the [GitHub issue tracker](https://github.com/chriskooCK/codex-switch-global-pace/issues).
