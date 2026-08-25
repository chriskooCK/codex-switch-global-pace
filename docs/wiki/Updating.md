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
rejected. The extracted candidate must execute and report the exact Release
version; only then is the tag resolved again immediately before replacement, so
a moved tag or unusable package leaves the installed binary unchanged.

A current [GitHub CLI](https://cli.github.com/) with attestation support is
required. If a daemon is running, self-update stops it before replacement and
restarts it afterwards. On Windows, an in-flight credential rotation is never
force-killed merely to complete an update. Self-update holds the shared daemon
lifecycle lease from the initial snapshot through restart and executable commit.
After stop, it also retains the existing `daemon.pid.lock` until the controlled
restart handoff; this keeps an initially stopped daemon stopped and rejects a
direct foreground start during replacement. A foreground command that wins the
short restart handoff is classified by its published PID generation, stopped,
and followed by reacquisition of exact absence before rollback. Normal CLI
start, stop, and service operations use the same lifecycle lease. Direct, non-cooperating
`systemctl`/`launchctl`/`schtasks` mutations are outside that serialization
contract and are not described as protected.

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
user-owned location and preserves profile data. If a running daemon service
still records the legacy absolute path, the installer transactionally reinstalls
it with the new user binary before removing the old one. An installed but
inactive legacy service is left untouched with explicit uninstall/reinstall
instructions because silently starting it would change the user's service state.

On Windows, macOS, and Linux, the verified installer candidate acquires the
shared update lock first, then retains the daemon service-operation lease
continuously from the pre-mutation state capture through an explicit commit or
rollback. It also owns `daemon.pid.lock` as an absence lease while the daemon is
stopped. This applies to fresh installs, upgrades, and uninstall: a foreground
daemon cannot enter after first publication or before service removal and
survive a later PATH/file rollback. If a foreground start wins the short PID
handoff while the intended daemon is being restarted, the holder stops that
exact published PID generation and reacquires absence before rollback; it does
not release the transaction on an unclassified contender. Replacement or
uninstall PID/service state is revalidated before executable recovery copies
are removed and again before the lifecycle lease is released. Direct,
non-cooperating service-manager mutations remain outside this serialization
contract.

Before downloading, self-update confirms that the current executable directory
can accept a replacement. Permission failures therefore surface before any
archive is fetched. An independent copy of the previous executable and the
actual file displaced by publication remain beside the installed path until the
replacement and any previously running daemon are healthy. If that daemon
cannot restart, self-update first proves the failed process stopped, restores
the exact displaced executable, and restarts the previous daemon state.

Linux and macOS publish with one atomic name exchange, so the public executable
name is never temporarily absent. Windows uses `ReplaceFileW` with a separate
displaced-file path. The Windows displaced and failed-candidate recovery names
each contain a CSPRNG-generated 128-bit nonce; allocation retries a named,
bounded number of collisions, and `ReplaceFileW` is never given a fixed or
guessable backup name. The pending transaction retains both exact random paths,
and manual-recovery errors print them rather than trying to rediscover them.
Both platforms identify the public, candidate, displaced, and backup files
again after the operating-system call; cleanup and rollback only touch a file
whose identity and digest still match the transaction. A
non-cooperating writer can still change the public name between observations,
so self-update does not claim a strict content compare-and-swap. When the
post-state proves which file was displaced, that actual writer is restored;
otherwise every recovery entry is preserved and the command fails closed with
their exact paths. Existing transaction residue is never guessed or
overwritten. Linux and macOS also sync the containing directory after namespace
changes and retry that same durability boundary when a verified post-state shows
that a namespace call applied before reporting an error. Windows flushes and
rebinds each recovery file, uses `MOVEFILE_WRITE_THROUGH` where that Win32 API
supports it, and verifies every resulting name. Windows does not expose a
supported directory-fsync contract for `ReplaceFileW`, so the updater does not
claim power-loss durability for that directory entry beyond those Win32
guarantees. Unsupported atomic primitives are rejected rather than replaced by
a weaker fallback.

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
