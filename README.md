# codex-switch-global-pace

**A multi-account manager and global weekly quota dashboard for
[OpenAI Codex CLI](https://github.com/openai/codex).** It treats the weekly
allowances of all available profiles as one pool, while preserving the login,
switching, warmup, reset-card, JSON, and daemon workflows from its
one-time `codex-switch` source snapshot.

[中文说明](README_CN.md) · [Documentation](docs/wiki/Home.md) ·
[Releases](https://github.com/chriskooCK/codex-switch-global-pace/releases)

> This program manages local authentication files. Never publish profiles,
> `auth.json`, tokens, proxy credentials, or unredacted debug output.

## Quick start

Codex must use its file credential store. If needed, add this to
`$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`):

```toml
cli_auth_credentials_store = "file"
```

Install on macOS or Linux:

```bash
curl -fsSL https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.sh | bash
```

Install from Windows PowerShell:

```powershell
irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
```

Then add an account or open the dashboard:

```bash
codex-switch-global-pace login
codex-switch-global-pace list
codex-switch-global-pace use
codex-switch-global-pace          # open the interactive dashboard
```

Running `codex-switch-global-pace` with no arguments opens the TUI. On Windows,
double-clicking `codex-switch-global-pace.exe` therefore opens the same dashboard.
`use` changes the live account used by the next Codex process; restart an
already-running Codex app or CLI after switching.

## Global Weekly Pace

For every account with a valid weekly window, the dashboard combines remaining
quota with time until reset:

```text
effective capacity = 100% + elapsed% - used%
normal capacity    = included accounts × 100%
global pace        = sum(effective capacity) / normal capacity × 100%
```

`100%` is on pace, more than `100%` is reserve, and less than `100%` is a
deficit. Fully exhausted accounts remain included when their reset timestamp is
valid. Accounts whose usage or weekly reset cannot be trusted are reported as
unavailable. The current API does not expose a reliable comparable weekly
capacity, so accounts are weighted equally.

Quota meters use one relative rule everywhere: yellow means actual usage is
ahead of the elapsed-time pace, while green means usage is at or behind pace.
The Global meter applies the same rule to aggregate usage and aggregate elapsed
time. A fully exhausted quota is red, and unavailable comparisons stay neutral;
quota labels do not append warning punctuation.

## Existing profiles and daemon compatibility

The application deliberately reuses the original data directory:

- macOS/Linux: `~/.codex-switch`
- Windows: `%USERPROFILE%\.codex-switch`
- override: `CODEX_SWITCH_HOME`

Existing `profiles/`, `cache.json`, `config.toml`, `current`, and
`daemon-state.json` remain usable without another login. Installers and
uninstallers preserve this shared directory.

> Do not run the `codex-switch` daemon and the `codex-switch-global-pace` daemon
> at the same time. They intentionally share profiles, cache, locks, current
> account, and daemon state. Stop and uninstall one daemon service before
> enabling the other. The interactive commands and TUI do not require a daemon.

## Updates and release verification

`self-update` reads releases only from
`chriskooCK/codex-switch-global-pace`; it never downloads the original
project's releases. Direct updates verify the archive's SHA-256 checksum and a
GitHub build-provenance bundle with `gh attestation verify`, bound to this
repository, its release workflow, the exact tag ref, and the tag commit digest.
A current [GitHub CLI](https://cli.github.com/) is therefore required for direct
self-update.

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
cargo test --all
cargo clippy --all-targets -- -D warnings
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
