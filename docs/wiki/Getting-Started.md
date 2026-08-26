# Getting started

This page takes you from nothing to a working multi-account setup: install codex-switch-global-pace, add accounts, and pick the best one before a Codex session.

## Requirements

- [OpenAI Codex CLI](https://github.com/openai/codex) installed, plus at least one ChatGPT account that can log in to Codex.
- Codex must use its **file credential store**, because codex-switch-global-pace works by atomically replacing `$CODEX_HOME/auth.json`. If needed, add this to `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`):

```toml
cli_auth_credentials_store = "file"
```

Explicit `keyring`, `auto`, and `ephemeral` stores are rejected — permanently by design, because OS keyrings cannot provide the locking and atomic-replace guarantees switching depends on (see [why only the file store is supported](Configuration.md#why-only-the-file-store-is-supported)). A managed Codex configuration with `forced_login_method = "api"` is also incompatible, because codex-switch-global-pace manages ChatGPT login profiles. In both cases codex-switch-global-pace stops with an actionable error instead of modifying authentication state; after switching to the file store, log in again.

## Install

Use one of the source-controlled blocks below. They are the trust anchor: do not
replace them with commands copied from an installer or an unverified Release
asset. A current [GitHub CLI](https://cli.github.com/) downloads the installer
and provenance bundle separately, binds the attestation to this repository,
release workflow, exact tag ref (including annotated-tag peeling), and current
tag commit, rechecks that commit,
and only then executes the local installer. Any failure stops the install; there
is no unverified fallback.

For the stable release, use `channel=stable` / `$Channel = "stable"` exactly as
shown. To install the rolling development release instead, change only that one
value to `dev`. The Unix block passes `--dev`; the packaged Windows installer
recognizes its embedded `-dev` version without a persistent environment setting.

macOS / Linux:

```bash
set -eu
repo=chriskooCK/codex-switch-global-pace
bootstrap_error() {
  printf '%s\n' "$1" >&2
  exit 1
}
resolve_release_tag_commit() {
  local tag_to_resolve="$1" endpoint tuple kind sha extra depth=0
  endpoint="repos/$repo/git/ref/tags/$tag_to_resolve"
  while [ "$depth" -le 5 ]; do
    tuple="$(gh api --hostname github.com "$endpoint" \
      --jq '[.object.type, .object.sha] | @tsv')" \
      || bootstrap_error "Could not resolve release tag $tag_to_resolve."
    case "$tuple" in
      *$'\n'*) bootstrap_error "Release tag $tag_to_resolve returned more than one object record." ;;
    esac
    IFS=$'\t' read -r kind sha extra <<< "$tuple"
    [ -n "$kind" ] && [ -n "$sha" ] && [ -z "${extra:-}" ] \
      || bootstrap_error "Release tag $tag_to_resolve returned an invalid object record."
    [ "${#sha}" -eq 40 ] && [[ "$sha" != *[!0-9A-Fa-f]* ]] \
      || bootstrap_error "Release tag $tag_to_resolve returned an invalid Git object digest."
    if [ "$kind" = commit ]; then
      printf '%s\n' "$sha" | tr '[:upper:]' '[:lower:]'
      return 0
    fi
    [ "$kind" = tag ] \
      || bootstrap_error "Release tag $tag_to_resolve resolved to unsupported Git object type $kind."
    [ "$depth" -lt 5 ] \
      || bootstrap_error "Release tag $tag_to_resolve contains more than five nested annotated tags."
    endpoint="repos/$repo/git/tags/$sha"
    depth=$((depth + 1))
  done
}
channel=stable
case "$channel" in
  stable)
    tag="$(gh api --hostname github.com "repos/$repo/releases/latest" --jq .tag_name)"
    ;;
  dev)
    tag=dev
    ;;
  *)
    printf 'Invalid release channel.\n' >&2
    exit 1
    ;;
esac
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
[ "$confirmed_digest" = "$source_digest" ] || {
  printf 'Release tag moved during verification; refusing installer execution.\n' >&2
  exit 1
}
# macOS ships an older Bash where an empty array expansion fails under `set -u`.
# Keep both supported channel invocations explicit instead of synthesizing args.
case "$channel" in
  stable) bash "$work/install.sh" ;;
  dev) bash "$work/install.sh" --dev ;;
  *) bootstrap_error "Invalid release channel." ;;
esac
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
        if ($Kind -ceq 'commit') { return $Sha }
        if ($Kind -cne 'tag') {
            throw "Release tag '$TagToResolve' resolved to unsupported Git object type '$Kind'."
        }
        if ($Depth -eq 5) {
            throw "Release tag '$TagToResolve' contains more than five nested annotated tags."
        }
        $Endpoint = "repos/$Repo/git/tags/$Sha"
    }
    throw "Release tag '$TagToResolve' could not be resolved to a commit."
}
$Channel = "stable"
switch -CaseSensitive ($Channel) {
    "stable" {
        $TagResult = @(gh api --hostname github.com "repos/$Repo/releases/latest" --jq .tag_name)
        if ($LASTEXITCODE -ne 0 -or $TagResult.Count -ne 1 -or
            [string]::IsNullOrWhiteSpace($TagResult[0])) {
            throw "Stable Release tag lookup failed."
        }
        $Tag = $TagResult[0]
    }
    "dev" { $Tag = "dev" }
    default { throw "Invalid release channel." }
}
$SourceDigest = Resolve-ReleaseTagCommit -TagToResolve $Tag
$Work = Join-Path ([IO.Path]::GetTempPath()) "codex-switch-install-$([guid]::NewGuid().ToString('N'))"
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
    if ($LASTEXITCODE -ne 0) { throw "Installer provenance verification failed." }
    $ConfirmedDigest = Resolve-ReleaseTagCommit -TagToResolve $Tag
    if ($ConfirmedDigest -cne $SourceDigest) {
        throw "Release tag moved during verification; refusing installer execution."
    }
    & (Join-Path $Work "install.ps1")
} finally {
    Remove-Item -LiteralPath (Join-Path $Work "install.ps1") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $Work "codex-switch-global-pace-build-provenance.json") -Force -ErrorAction SilentlyContinue
    [IO.Directory]::Delete($Work, $false)
}
```

This installs to the user-owned `$HOME/.local/bin` and configures PATH for zsh, bash, and fish; other shells receive a manual PATH instruction. An older direct install under `/usr/local/bin` is migrated once: the new user binary is installed first, then the installer removes the old copy with one elevated operation when required. Administrators can explicitly keep a system-wide install with `--system`; system installs may require `sudo` for later updates.

> If the installer says `Installing to /usr/local/bin (requires sudo)` without an explicit `--system`, stop it: that is the retired script from the repository's old `master` branch. Use the verified block above.

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
codex-switch-global-pace             # interactive dashboard
codex-switch-global-pace use         # switch to the best eligible account
```

`use` without an alias ranks all accounts with the adaptive scoring algorithm; `use <alias>` switches explicitly. Codex reads authentication at startup, so restart Codex after switching.

## Where your data lives

Saved profiles, cache, configuration, and daemon state default to `~/.codex-switch` (`%USERPROFILE%\.codex-switch` on Windows). The live Codex file stays at `$CODEX_HOME/auth.json`. See [Configuration](Configuration.md) for every path and setting.

Never share profile files, `auth.json`, tokens, proxy credentials, or unredacted `--debug` output.

The data directory is shared with `codex-switch` for compatibility. Do not run
both daemon services at once; stop and uninstall one daemon before enabling the
other.

## Next steps

- Learn account, quota, switching, and daemon workflows in the [Feature guide](Feature-Guide.md).
- Look up exact commands and TUI shortcuts in the [Command reference](Command-Reference.md).
- Keep the binary current with [Updating](Updating.md).
