# Updating

Direct installations update only from verified releases of
`chriskooCK/codex-switch-global-pace`.

```bash
codex-switch-global-pace self-update --check
codex-switch-global-pace self-update
codex-switch-global-pace self-update --version <VERSION>
```

Update checks are manual except for the check performed when the TUI starts.
There is no fallback to `xjoker/codex-switch`; if this repository has no matching
release, the updater reports that the release is unavailable.

## Verification

The updater requires the matching archive, `.sha256` file, and
`codex-switch-global-pace-build-provenance.json` bundle from the same GitHub
Release. It verifies SHA-256, resolves the release tag to its commit, and runs
`gh attestation verify` pinned to this repository, `.github/workflows/release.yml`,
the exact tag ref, and that commit digest. Self-hosted-runner attestations are
rejected, and the tag is resolved again before replacement so a moved tag aborts
the update.

A current [GitHub CLI](https://cli.github.com/) with attestation support is
required. If a daemon is running, self-update stops it before replacement and
restarts it afterwards. On Windows, an in-flight credential rotation is never
force-killed merely to complete an update.

## Channels

Releases use SemVer-compatible calendar versions in the form `YYYYMMDD.N.0`:

The `YYYYMMDD` component is the version-allocation date for the accepted dev
candidate. A later stable promotion keeps that exact version rather than
encoding the promotion date.

- **stable** — permanent `v*` GitHub Release tags.
- **dev** — a rolling prerelease under the `dev` tag.

```bash
codex-switch-global-pace self-update --dev
codex-switch-global-pace self-update --stable
```

Without a channel flag, self-update stays on the current binary's channel.

## Install locations and migration

The direct installer uses `$HOME/.local/bin` on macOS/Linux and
`%LOCALAPPDATA%\Programs\codex-switch-global-pace` on Windows. Unix
administrators may opt into `/usr/local/bin` with `--system`. Rerunning the
installer migrates an older system-wide copy of this same binary name to the
user-owned location and preserves profile data.

Before downloading, self-update confirms that the current executable directory
can accept a replacement. Permission failures therefore surface before any
archive is fetched.

## Uninstall

macOS/Linux:

```bash
curl -fsSL https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.sh | bash -s -- --uninstall
```

Windows PowerShell:

```powershell
$env:CS_UNINSTALL="1"; irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
```

The uninstaller always retains `~/.codex-switch`
(`%USERPROFILE%\.codex-switch` on Windows), because this directory is shared
with `codex-switch` and contains profiles and credentials. Remove it manually
only when neither program needs the data.

## Next steps

- Opt into prerelease testing with [Testing development releases](Development-Releases.md).
- Diagnose update failures in [Troubleshooting](Troubleshooting.md).
- Return to the documentation [Home](Home.md).
