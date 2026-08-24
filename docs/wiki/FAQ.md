# FAQ

## Does codex-switch-global-pace support keyring-backed Codex credentials?

No, and this is permanent by design. OS keyrings provide no locking or atomic-replace semantics, and Codex's keyring entry format is an undocumented internal layout that has already changed between versions and platforms. Codex must use `cli_auth_credentials_store = "file"`; if it previously used a keyring store, switch the setting and log in again. See [why only the file store is supported](Configuration.md#why-only-the-file-store-is-supported).

## Does switching affect an already-running Codex session?

No. Codex reads authentication at startup. Restart Codex after switching accounts.

## Where is account data stored?

Saved profiles and application state default to `~/.codex-switch`; the live Codex file defaults to `~/.codex/auth.json`. `CODEX_SWITCH_HOME` and `CODEX_HOME` relocate them independently.

## Is profile deletion permanent?

No. Inactive profiles are archived under `deleted-profiles/`. The active profile cannot be deleted.

## Is the daemon required?

No. It is an optional Beta feature. `codex-switch-global-pace use`, `list`, and the TUI work without it.

## What do the version numbers mean?

Releases use calendar versions in the form `YYYYMMDD.N.0`. Rolling dev builds end in `-dev`; see [Updating](Updating.md).

## How do I test the next release?

Use the rolling dev channel only when you are prepared to test prerelease behavior. Follow [Testing development releases](Development-Releases.md) for installation, verification, rollback, and issue-reporting steps.

## How are release binaries verified?

Archives are checked against SHA-256 files and a GitHub build-provenance bundle. Direct self-update runs `gh attestation verify` pinned to this repository, its release workflow, the exact tag ref, and the tag commit digest; see [Updating](Updating.md#verification).

## Where should documentation fixes go?

Documentation fixes belong in `docs/wiki/`. Open a pull request that updates the
relevant page together with any behavior it describes.

## Next steps

- New installation: [Getting started](Getting-Started.md).
- Daily workflows: [Feature guide](Feature-Guide.md).
- Errors and recovery: [Troubleshooting](Troubleshooting.md).
