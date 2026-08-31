# FAQ

## Does codex-switch-global-pace support keyring-backed Codex credentials?

No, and this is permanent by design. OS keyrings provide no locking or atomic-replace semantics, and Codex's keyring entry format is an undocumented internal layout that has already changed between versions and platforms. Codex must use `cli_auth_credentials_store = "file"`; if it previously used a keyring store, switch the setting and log in again. See [why only the file store is supported](Configuration.md#why-only-the-file-store-is-supported).

## Does switching affect an already-running Codex session?

No. Codex reads authentication at startup. Finish and exit processes started by
`codex`, `codex resume`, or `codex exec`; on Windows, also quit the Codex app
from its notification-area (system-tray) menu because closing its window can
leave it resident. Then switch and start a new Codex process. Long-lived MCP and
app-server helper processes are not interactive sessions and do not by
themselves defer an automatic daemon switch.

## Where is account data stored?

Saved profiles and application state default to `~/.codex-switch`; the live Codex file and up to three normally retained managed backups default to `~/.codex/auth.json` and `~/.codex/auth.json.bak.*`. `CODEX_SWITCH_HOME` and `CODEX_HOME` relocate them independently. See the [credential lifecycle](Configuration.md#credential-lifecycle-and-backups) before copying or deleting either directory.

## Is profile deletion permanent?

No. Inactive profiles are archived under `deleted-profiles/`. The active profile cannot be deleted.

## Does uninstalling remove my accounts?

No. The verified uninstaller removes the executable, PATH entry, and installed
daemon service but deliberately preserves `$CODEX_SWITCH_HOME`, saved/deleted
profiles, recovery files, and Codex's live authentication. Follow
[Remove all local credentials](Updating.md#remove-all-local-credentials) only
when neither codex-switch program nor Codex should keep those logins.

## How do I move my accounts to another computer?

Stop the daemon and all interactive `codex`, `codex resume`, and `codex exec`
sessions; on Windows, quit the tray application as well. Close parent clients
before the filesystem copy even though helper-only MCP/app-server processes do
not trigger normal switch deferral. Copy the complete state directory to
encrypted private storage, restore it without merging, then run `list` and
`use <alias>` on the new machine. The complete procedure and optional files are
in [Back up, restore, or move to a new machine](Configuration.md#back-up-restore-or-move-to-a-new-machine).

## Which Codex version is supported?

The explicit interoperability target is Codex CLI **0.149.0**. That is a tested
protocol target, not an installer constraint; other versions may work but are
not claimed compatible until validated. Update both applications before
reporting a regression and include both version strings. See the
[compatibility policy](Configuration.md#codex-interoperability-target).

## What aliases can I use?

Use 1–64 ASCII letters, digits, `_`, `-`, or `.`; `.` and `..` alone are not
valid. Avoid names that differ only by capitalization because profiles are
directories and filesystem case rules vary. See [Profile alias rules](Configuration.md#profile-alias-rules).

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
