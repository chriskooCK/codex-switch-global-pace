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

## Uninstall the application

The blocks below are complete: do not download an installer separately or edit
an installation block. They resolve the current stable tag, verify the
downloaded uninstaller's GitHub build provenance against this repository,
workflow, exact tag ref, and commit, confirm that the tag did not move, and only
then invoke uninstall. Install a current [GitHub CLI](https://cli.github.com/)
and authenticate it with `gh auth login --hostname github.com` first; confirm
with `gh auth status --hostname github.com`.

macOS/Linux:

```bash
set -eu
repo=chriskooCK/codex-switch-global-pace
uninstall_error() {
  printf '%s\n' "$1" >&2
  exit 1
}
resolve_release_tag_commit() {
  local tag_to_resolve="$1" endpoint tuple kind sha extra depth=0
  endpoint="repos/$repo/git/ref/tags/$tag_to_resolve"
  while [ "$depth" -le 5 ]; do
    tuple="$(gh api --hostname github.com "$endpoint" \
      --jq '[.object.type, .object.sha] | @tsv')" \
      || uninstall_error "Could not resolve release tag $tag_to_resolve."
    case "$tuple" in
      *$'\n'*) uninstall_error "Release tag $tag_to_resolve returned more than one object record." ;;
    esac
    IFS=$'\t' read -r kind sha extra <<< "$tuple"
    [ -n "$kind" ] && [ -n "$sha" ] && [ -z "${extra:-}" ] \
      || uninstall_error "Release tag $tag_to_resolve returned an invalid object record."
    [ "${#sha}" -eq 40 ] && [[ "$sha" != *[!0-9A-Fa-f]* ]] \
      || uninstall_error "Release tag $tag_to_resolve returned an invalid Git object digest."
    if [ "$kind" = commit ]; then
      printf '%s\n' "$sha" | tr '[:upper:]' '[:lower:]'
      return 0
    fi
    [ "$kind" = tag ] \
      || uninstall_error "Release tag $tag_to_resolve resolved to unsupported Git object type $kind."
    [ "$depth" -lt 5 ] \
      || uninstall_error "Release tag $tag_to_resolve contains more than five nested annotated tags."
    endpoint="repos/$repo/git/tags/$sha"
    depth=$((depth + 1))
  done
}
tag="$(gh api --hostname github.com "repos/$repo/releases/latest" --jq .tag_name)"
[ -n "$tag" ] || uninstall_error "Stable Release tag lookup returned an empty value."
source_digest="$(resolve_release_tag_commit "$tag")"
work="$(mktemp -d)"
cleanup() {
  status=$?
  rm -f -- \
    "$work/install.sh" \
    "$work/codex-switch-global-pace-build-provenance.json" || status=1
  rmdir -- "$work" || status=1
  exit "$status"
}
trap cleanup EXIT
gh release download "$tag" --repo "$repo" --dir "$work" \
  --pattern install.sh \
  --pattern codex-switch-global-pace-build-provenance.json
gh attestation verify "$work/install.sh" \
  --bundle "$work/codex-switch-global-pace-build-provenance.json" \
  --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref "refs/tags/$tag" \
  --source-digest "$source_digest" \
  --deny-self-hosted-runners
confirmed_digest="$(resolve_release_tag_commit "$tag")"
[ "$confirmed_digest" = "$source_digest" ] \
  || uninstall_error "Release tag moved during verification; refusing uninstaller execution."
bash "$work/install.sh" --uninstall
```

Windows PowerShell:

```powershell
$ErrorActionPreference = "Stop"
$Repo = "chriskooCK/codex-switch-global-pace"
function Resolve-ReleaseTagCommit {
    param([Parameter(Mandatory = $true)][string]$TagToResolve)

    $Endpoint = "repos/$Repo/git/ref/tags/$TagToResolve"
    foreach ($Depth in 0..5) {
        $Response = @(gh api --hostname github.com $Endpoint `
            --jq '[.object.type, .object.sha] | @tsv' 2>&1)
        $ExitCode = $LASTEXITCODE
        if ($ExitCode -ne 0) {
            throw "Could not resolve release tag '$TagToResolve': $([string]::Join(' ', $Response))"
        }
        if ($Response.Count -ne 1) {
            throw "Release tag '$TagToResolve' returned $($Response.Count) object records; expected exactly one."
        }
        $Fields = ([string]$Response[0]).Split([char]"`t")
        if ($Fields.Count -ne 2 -or
            [string]::IsNullOrWhiteSpace($Fields[0]) -or
            $Fields[1] -cnotmatch '\A[0-9A-Fa-f]{40}\z') {
            throw "Release tag '$TagToResolve' returned an invalid Git object record."
        }
        $Kind = $Fields[0]
        $Sha = $Fields[1].ToLowerInvariant()
        if ($Kind -ceq "commit") { return $Sha }
        if ($Kind -cne "tag") {
            throw "Release tag '$TagToResolve' resolved to unsupported Git object type '$Kind'."
        }
        if ($Depth -eq 5) {
            throw "Release tag '$TagToResolve' contains more than five nested annotated tags."
        }
        $Endpoint = "repos/$Repo/git/tags/$Sha"
    }
    throw "Release tag '$TagToResolve' could not be resolved to a commit."
}
$TagResult = @(gh api --hostname github.com "repos/$Repo/releases/latest" --jq .tag_name)
if ($LASTEXITCODE -ne 0 -or $TagResult.Count -ne 1 -or
    [string]::IsNullOrWhiteSpace($TagResult[0])) {
    throw "Stable Release tag lookup failed."
}
$Tag = $TagResult[0]
$SourceDigest = Resolve-ReleaseTagCommit -TagToResolve $Tag
$Work = Join-Path ([IO.Path]::GetTempPath()) "codex-switch-uninstall-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $Work | Out-Null
try {
    gh release download $Tag --repo $Repo --dir $Work `
        --pattern install.ps1 `
        --pattern codex-switch-global-pace-build-provenance.json
    if ($LASTEXITCODE -ne 0) { throw "Release download failed." }
    gh attestation verify (Join-Path $Work "install.ps1") `
        --bundle (Join-Path $Work "codex-switch-global-pace-build-provenance.json") `
        --repo $Repo `
        --signer-workflow "$Repo/.github/workflows/release.yml" `
        --source-ref "refs/tags/$Tag" `
        --source-digest $SourceDigest `
        --deny-self-hosted-runners
    if ($LASTEXITCODE -ne 0) { throw "Uninstaller provenance verification failed." }
    $ConfirmedDigest = Resolve-ReleaseTagCommit -TagToResolve $Tag
    if ($ConfirmedDigest -cne $SourceDigest) {
        throw "Release tag moved during verification; refusing uninstaller execution."
    }
    & (Join-Path $Work "install.ps1") -Uninstall
} finally {
    Remove-Item -LiteralPath (Join-Path $Work "install.ps1") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $Work "codex-switch-global-pace-build-provenance.json") -Force -ErrorAction SilentlyContinue
    [IO.Directory]::Delete($Work, $false)
}
```

This removes the direct executable, its PATH entry, and the installed Beta
daemon service. It deliberately retains `$CODEX_SWITCH_HOME` (normally
`~/.codex-switch` or `%USERPROFILE%\.codex-switch`) because it is shared with
`codex-switch` and contains saved, deleted, and recovery credentials. It also
retains Codex's `$CODEX_HOME/auth.json`, managed live backups, configuration,
and other Codex data. Reinstalling later therefore preserves the accounts.

## Remove all local credentials

This procedure is separate and irreversible. Use it only when Codex and both
codex-switch applications should forget every local account. If the accounts
may be needed again, first follow the encrypted
[backup procedure](Configuration.md#back-up-restore-or-move-to-a-new-machine).
Finish and exit every interactive process started by `codex`, `codex resume`,
or `codex exec`; on Windows, quit the Codex notification-area application and
confirm in Task Manager that it stopped. MCP/app-server helpers alone are not
interactive-session blockers, but close their parent clients before erasing the
files. Run the verified application uninstaller above first so no daemon can
rewrite state.

Native Windows and WSL use separate home directories and credential stores by
default. To remove credentials used by the Codex Windows app, run the Windows
PowerShell block in native Windows, not a WSL shell. If Codex or this switcher
was also used inside WSL, run the macOS/Linux block separately inside every WSL
distribution whose own state should also be erased. A successful block proves
removal only for the exact state and Codex-home targets it prints.

The original `codex-switch` binary uses the same state and may have its own
installed service. If `codex-switch` is still installed, run the following as
the same user (from elevated PowerShell on Windows), and do not continue until
`status` reports that its process is stopped and its native service is not
installed:

```bash
codex-switch daemon stop
codex-switch daemon uninstall
codex-switch daemon status
```

The verified codex-switch-global-pace uninstaller and this original-binary check
are both required when both programs have ever installed a daemon. Quitting the
Codex desktop app does not stop either daemon.

Codex can cache credentials either in `auth.json` or in the operating-system
credential store; `auto` selects one of those backends. After validating every
target and receiving the exact deletion confirmation, each block explicitly
logs out both the `keyring` and `file` backends. This also covers a keyring used
before this switcher required file mode. Resolve any logout error before
continuing; deleting files after a failed logout is not a verified complete
removal. See the
[official credential-storage description](https://learn.chatgpt.com/docs/auth#credential-storage).

macOS/Linux:

```bash
set -eu
state_home="${CODEX_SWITCH_HOME:-$HOME/.codex-switch}"
codex_home="${CODEX_HOME:-$HOME/.codex}"
case "$state_home" in /*) ;; *) printf 'CODEX_SWITCH_HOME must be absolute.\n' >&2; exit 1 ;; esac
case "$codex_home" in /*) ;; *) printf 'CODEX_HOME must be absolute.\n' >&2; exit 1 ;; esac
home_real="$(cd -P -- "$HOME" && pwd -P)"
state_target=
if [ -e "$state_home" ]; then
  [ -d "$state_home" ] && [ ! -L "$state_home" ] || {
    printf 'State path is not a direct directory: %s\n' "$state_home" >&2
    exit 1
  }
  state_target="$(cd -P -- "$state_home" && pwd -P)"
fi
codex_target=
if [ -e "$codex_home" ]; then
  [ -d "$codex_home" ] && [ ! -L "$codex_home" ] || {
    printf 'Codex home is not a direct directory: %s\n' "$codex_home" >&2
    exit 1
  }
  codex_target="$(cd -P -- "$codex_home" && pwd -P)"
fi
case "$state_target" in "" ) ;; /|"$home_real") printf 'Refusing broad state target: %s\n' "$state_target" >&2; exit 1 ;; esac
[ "$codex_target" != / ] \
  || { printf 'Refusing filesystem-root CODEX_HOME.\n' >&2; exit 1; }
if [ -n "$state_target" ]; then
  case "$home_real/" in
    "$state_target/"*) printf 'Refusing a state target that contains the home directory: %s\n' "$state_target" >&2; exit 1 ;;
  esac
  case "$codex_target/" in
    "$state_target/"*) printf 'Refusing a state target that contains CODEX_HOME: %s\n' "$state_target" >&2; exit 1 ;;
  esac
fi
[ -z "$state_target" ] || [ "$state_target" != "$codex_target" ] || {
  printf 'CODEX_SWITCH_HOME must not be the Codex home for this removal.\n' >&2
  exit 1
}
printf 'State directory to erase: %s\n' "${state_target:-<absent>}"
printf 'Live credentials to erase under: %s\n' "${codex_target:-<absent>}"
expected_confirmation="DELETE ${state_target:-<absent>} AND AUTH ${codex_target:-<absent>}"
printf 'Type exactly "%s" to continue: ' "$expected_confirmation"
IFS= read -r confirmation
[ "$confirmation" = "$expected_confirmation" ] || { printf 'Cancelled.\n'; exit 1; }
codex -c 'cli_auth_credentials_store="keyring"' logout
codex -c 'cli_auth_credentials_store="file"' logout
if [ -n "$state_target" ]; then
  rm -rf -- "$state_target"
fi
if [ -n "$codex_target" ]; then
  find "$codex_target" -mindepth 1 -maxdepth 1 \
    \( -name 'auth.json' -o -name 'auth.json.bak.*' \
       -o -name '.auth.json.codex-switch-*' \) \
    -exec rm -f -- {} +
fi
[ -z "$state_target" ] || [ ! -e "$state_target" ] \
  || { printf 'State directory still exists.\n' >&2; exit 1; }
if [ -n "$codex_target" ] && find "$codex_target" -mindepth 1 -maxdepth 1 \
  \( -name 'auth.json' -o -name 'auth.json.bak.*' \
     -o -name '.auth.json.codex-switch-*' \) -print -quit | grep -q .; then
  printf 'One or more Codex credential files remain.\n' >&2
  exit 1
fi
printf 'Verified: local codex-switch state and Codex credential files are absent.\n'
```

Windows PowerShell:

```powershell
$ErrorActionPreference = "Stop"
$StateHome = if ([string]::IsNullOrWhiteSpace($env:CODEX_SWITCH_HOME)) {
    Join-Path $HOME ".codex-switch"
} else { $env:CODEX_SWITCH_HOME }
$CodexHome = if ([string]::IsNullOrWhiteSpace($env:CODEX_HOME)) {
    Join-Path $HOME ".codex"
} else { $env:CODEX_HOME }
if (-not [IO.Path]::IsPathRooted($StateHome) -or
    -not [IO.Path]::IsPathRooted($CodexHome)) {
    throw "CODEX_SWITCH_HOME and CODEX_HOME must be absolute."
}
$StateTarget = [IO.Path]::GetFullPath($StateHome).TrimEnd('\', '/')
$CodexTarget = [IO.Path]::GetFullPath($CodexHome).TrimEnd('\', '/')
$HomeTarget = [IO.Path]::GetFullPath($HOME).TrimEnd('\', '/')
$StateRoot = [IO.Path]::GetPathRoot($StateTarget).TrimEnd('\', '/')
$CodexRoot = [IO.Path]::GetPathRoot($CodexTarget).TrimEnd('\', '/')
if ($StateTarget -eq $StateRoot -or
    $StateTarget.Equals($HomeTarget, [StringComparison]::OrdinalIgnoreCase) -or
    $HomeTarget.StartsWith("$StateTarget\", [StringComparison]::OrdinalIgnoreCase) -or
    $CodexTarget.StartsWith("$StateTarget\", [StringComparison]::OrdinalIgnoreCase) -or
    $StateTarget.Equals($CodexTarget, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing broad or overlapping state target: $StateTarget"
}
if ($CodexTarget -eq $CodexRoot) { throw "Refusing filesystem-root CODEX_HOME." }
if (Test-Path -LiteralPath $StateTarget) {
    $StateItem = Get-Item -LiteralPath $StateTarget -Force
    if (-not $StateItem.PSIsContainer -or
        ($StateItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "State path is not a direct directory: $StateTarget"
    }
}
if (Test-Path -LiteralPath $CodexTarget) {
    $CodexItem = Get-Item -LiteralPath $CodexTarget -Force
    if (-not $CodexItem.PSIsContainer -or
        ($CodexItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Codex home is not a direct directory: $CodexTarget"
    }
}
Write-Host "State directory to erase: $StateTarget"
Write-Host "Live credentials to erase under: $CodexTarget"
$ExpectedConfirmation = "DELETE $StateTarget AND AUTH $CodexTarget"
$Confirmation = Read-Host "Type exactly '$ExpectedConfirmation' to continue"
if ($Confirmation -cne $ExpectedConfirmation) { throw "Cancelled; no files were removed." }
codex -c 'cli_auth_credentials_store="keyring"' logout
if ($LASTEXITCODE -ne 0) { throw "Codex keyring logout failed; filesystem deletion was not started." }
codex -c 'cli_auth_credentials_store="file"' logout
if ($LASTEXITCODE -ne 0) { throw "Codex file-store logout failed; filesystem deletion was not started. Keyring logout may already have succeeded." }
if (Test-Path -LiteralPath $StateTarget) {
    Remove-Item -LiteralPath $StateTarget -Recurse -Force
}
if (Test-Path -LiteralPath $CodexTarget) {
    Get-ChildItem -LiteralPath $CodexTarget -Force |
        Where-Object {
            $_.Name -ceq "auth.json" -or
            $_.Name.StartsWith("auth.json.bak.", [StringComparison]::Ordinal) -or
            $_.Name.StartsWith(".auth.json.codex-switch-", [StringComparison]::Ordinal)
        } |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force }
}
if (Test-Path -LiteralPath $StateTarget) {
    throw "State directory still exists: $StateTarget"
}
$Remaining = if (Test-Path -LiteralPath $CodexTarget) {
    @(Get-ChildItem -LiteralPath $CodexTarget -Force | Where-Object {
        $_.Name -ceq "auth.json" -or
        $_.Name.StartsWith("auth.json.bak.", [StringComparison]::Ordinal) -or
        $_.Name.StartsWith(".auth.json.codex-switch-", [StringComparison]::Ordinal)
    })
} else { @() }
if ($Remaining.Count -ne 0) { throw "One or more Codex credential files remain." }
Write-Host "Verified: local codex-switch state and Codex credential files are absent."
```

These commands preserve other files under `$CODEX_HOME`, such as Codex
configuration and history, while removing the live file, all managed live
backups, and any interrupted auth-publication artifacts. Removing the complete
`$CODEX_SWITCH_HOME` also removes saved profiles, deleted archives,
token-rotation recovery credentials (including any explicitly quarantined
identity/policy rejection), proxy configuration, caches, logs, and daemon state.

## Next steps

- Opt into prerelease testing with [Testing development releases](Development-Releases.md).
- Diagnose update failures in [Troubleshooting](Troubleshooting.md).
- Return to the documentation [Home](Home.md).
