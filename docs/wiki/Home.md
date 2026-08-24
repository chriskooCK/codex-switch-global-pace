# codex-switch-global-pace documentation

`codex-switch-global-pace` manages multiple local OpenAI Codex CLI logins, shows each account's quota plus one combined Global Weekly Pace, and selects an account for the next Codex session.

> **Required:** Codex must use `cli_auth_credentials_store = "file"`. Start with [Getting started](Getting-Started.md) before importing or switching accounts.

## Start here

- New users: [install codex-switch-global-pace and add the first account](Getting-Started.md).
- Existing users: [choose a task](#choose-your-task).
- 中文读者：[从中文指南开始](Chinese-Guide.md)。

## Choose your task

| I want to… | Start here |
|---|---|
| Install codex-switch-global-pace and add my first account | [Getting started](Getting-Started.md) |
| Manage accounts, watch quota, switch accounts, or run the daemon | [Feature guide](Feature-Guide.md) |
| Look up an exact command, flag, or TUI shortcut | [Command reference](Command-Reference.md) |
| Configure paths, proxy, cache, or daemon behavior | [Configuration](Configuration.md) |
| Update the binary or move between release channels | [Updating](Updating.md) |
| Install or test the rolling `dev` build | [Testing development releases](Development-Releases.md) |
| Diagnose an error or recover a profile | [Troubleshooting](Troubleshooting.md) |
| Check a short behavior or security answer | [FAQ](FAQ.md) |

## Contribute

1. [Prepare a development environment](Developer-Onboarding.md).
2. [Understand state ownership and safety boundaries](Architecture-Overview.md).
3. [Follow the contribution and verification contract](Contributing.md).

## Documentation model

These pages are the repository-hosted user and contributor documentation for
`codex-switch-global-pace`. They are reviewed in pull requests with the code.
Maintainer-only material lives alongside them in the [release process](../RELEASE.md)
and the [changelog](../CHANGELOG.md). Stable installers and binaries come from
[GitHub Releases](https://github.com/chriskooCK/codex-switch-global-pace/releases).

Do not publish auth files, profile files, tokens, unredacted debug output, proxy credentials, account IDs, email addresses, or workspace names.
