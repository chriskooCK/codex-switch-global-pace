# codex-switch-global-pace Wiki

`codex-switch-global-pace` manages multiple local OpenAI Codex CLI logins, shows each account's quota plus one combined Global Weekly Pace, and selects an account for the next Codex session.

> **Required:** Codex must use `cli_auth_credentials_store = "file"`. Start with [Getting started](Getting-Started) before importing or switching accounts.

## Start here

- New users: [install codex-switch-global-pace and add the first account](Getting-Started).
- Existing users: [choose a task](#choose-your-task).
- 中文读者：[从中文指南开始](Chinese-Guide)。

## Choose your task

| I want to… | Start here |
|---|---|
| Install codex-switch-global-pace and add my first account | [Getting started](Getting-Started) |
| Manage accounts, watch quota, select, launch, or run the daemon | [Feature guide](Feature-Guide) |
| Look up an exact command, flag, or TUI shortcut | [Command reference](Command-Reference) |
| Configure paths, proxy, cache, daemon, or launch behavior | [Configuration](Configuration) |
| Update the binary or move between release channels | [Updating](Updating) |
| Install or test the rolling `dev` build | [Testing development releases](Development-Releases) |
| Diagnose an error or recover a profile | [Troubleshooting](Troubleshooting) |
| Check a short behavior or security answer | [FAQ](FAQ) |

## Contribute

1. [Prepare a development environment](Developer-Onboarding).
2. [Understand state ownership and safety boundaries](Architecture-Overview).
3. [Follow the contribution and verification contract](Contributing).

## Documentation model

These Wiki pages are the user and contributor documentation for `codex-switch-global-pace`. Their sources live in [`docs/wiki/` on the `dev` branch](https://github.com/chriskooCK/codex-switch-global-pace/tree/dev/docs/wiki), are reviewed in pull requests with the code, and are published here automatically. Maintainer-only material stays in the repository: the [release process](https://github.com/chriskooCK/codex-switch-global-pace/blob/dev/docs/RELEASE.md) and the [changelog](https://github.com/chriskooCK/codex-switch-global-pace/blob/dev/docs/CHANGELOG.md). Stable installers and binaries come from [GitHub Releases](https://github.com/chriskooCK/codex-switch-global-pace/releases).

Do not publish auth files, profile files, tokens, unredacted debug output, proxy credentials, account IDs, email addresses, or workspace names.
