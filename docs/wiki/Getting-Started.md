# Getting started

This page takes you from nothing to a working multi-account setup: install codex-switch-global-pace, add accounts, and pick the best one before a Codex session.

## Requirements

- [OpenAI Codex CLI](https://github.com/openai/codex) installed, plus at least one ChatGPT account that can log in to Codex.
- Codex must use its **file credential store**, because codex-switch-global-pace works by atomically replacing `$CODEX_HOME/auth.json`. If needed, add this to `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`):

```toml
cli_auth_credentials_store = "file"
```

Explicit `keyring`, `auto`, and `ephemeral` stores are rejected — permanently by design, because OS keyrings cannot provide the locking and atomic-replace guarantees switching depends on (see [why only the file store is supported](Configuration#why-only-the-file-store-is-supported)). A managed Codex configuration with `forced_login_method = "api"` is also incompatible, because codex-switch-global-pace manages ChatGPT login profiles. In both cases codex-switch-global-pace stops with an actionable error instead of modifying authentication state; after switching to the file store, log in again.

## Install

**macOS / Linux:**

```bash
curl -fsSL https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.sh | bash
```

This installs to the user-owned `$HOME/.local/bin` and configures PATH for zsh, bash, and fish; other shells receive a manual PATH instruction. An older direct install under `/usr/local/bin` is migrated once: the new user binary is installed first, then the installer removes the old copy with one elevated operation when required. Administrators can explicitly keep a system-wide install with `--system`; system installs may require `sudo` for later updates.

> If the installer says `Installing to /usr/local/bin (requires sudo)` without an explicit `--system`, stop it: that is the retired script from the repository's old `master` branch. Use the Release URL above.

**Windows PowerShell:**

```powershell
irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
```

Windows installs under `%LOCALAPPDATA%\Programs\codex-switch-global-pace` and updates the user PATH.

> **Note:** this project currently publishes direct GitHub Release downloads;
> it does not publish a Homebrew formula or crates.io package.

Verify the installation:

```bash
codex-switch-global-pace --version
```

## Add your first account

```bash
codex-switch-global-pace login work
```

`login` opens a browser PKCE flow; the alias (`work`) is optional and can be renamed later. On a headless machine, use the device-code flow instead:

```bash
codex-switch-global-pace login --device server
```

If you already have `auth.json` backups, import a file or scan a whole directory. Imports are parsed, identity-checked, validated against the usage service, and saved under collision-free aliases. An import never overwrites an existing profile: a Team workspace ID proves access to that workspace, not ownership of another user's saved credentials.

```bash
codex-switch-global-pace import ~/auth-backups
```

codex-switch-global-pace also notices logins performed outside of it: when the live `auth.json` contains an account it does not track (for example after a plain `codex login`), an interactive run offers to save it as a profile.

## Inspect quota and pick an account

```bash
codex-switch-global-pace list        # accounts, quota, availability
codex-switch-global-pace tui         # interactive dashboard
codex-switch-global-pace use         # switch to the best eligible account
```

`use` without an alias ranks all accounts with the adaptive scoring algorithm; `use <alias>` switches explicitly. Codex reads authentication at startup, so restart Codex after switching.

## Where your data lives

Saved profiles, cache, configuration, and daemon state default to `~/.codex-switch` (`%USERPROFILE%\.codex-switch` on Windows). The live Codex file stays at `$CODEX_HOME/auth.json`. See [Configuration](Configuration) for every path and setting.

Never share profile files, `auth.json`, tokens, proxy credentials, or unredacted `--debug` output.

The data directory is shared with `codex-switch` for compatibility. Do not run
both daemon services at once; stop and uninstall one daemon before enabling the
other.

## Next steps

- Learn account, quota, switching, and daemon workflows in the [Feature guide](Feature-Guide).
- Look up exact commands and TUI shortcuts in the [Command reference](Command-Reference).
- Keep the binary current with [Updating](Updating).
