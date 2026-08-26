# Testing development releases

> The rolling `dev` build contains changes intended for the next stable
> release and may change again. Do not use it when you require stable behavior.

## Install the rolling dev build

For an existing direct installation:

```bash
codex-switch-global-pace self-update --dev
```

For a new installation, use the source-controlled
[verified bootstrap](Getting-Started.md#install) and change only its documented
channel value from `stable` to `dev`. It verifies the installer attestation
against the current `dev` tag commit before execution. The Unix block passes
`--dev`; the packaged Windows installer recognizes its embedded `-dev` version
automatically, without a persistent process environment variable.

The installer and updater retain profiles and configuration under
`~/.codex-switch`. Do not substitute an installer URL from the original
repository or from a source branch.

## Verify and test

Confirm that the version ends in `-dev`, then run the smallest smoke test that
covers the intended behavior:

```bash
codex-switch-global-pace --version
codex-switch-global-pace self-update --check --dev
codex-switch-global-pace list
codex-switch-global-pace
```

Do not consume reset cards, delete profiles, install a daemon, or switch a live
account unless that action is part of the test. Never run the original and new
daemon services simultaneously because they share profile, lock, PID, cache,
current-account, and daemon-state files.

## Report a problem

Include the operating system, architecture, terminal, installation method,
exact command, expected and actual behavior, and the output from
`codex-switch-global-pace --version`. Remove tokens, profile contents, email
addresses, account IDs, workspace names, identifying filesystem paths, and proxy
credentials before sharing debug output.

## Return to stable

```bash
codex-switch-global-pace self-update --stable
```

## 中文摘要

`dev` 是滚动测试通道。直装用户可运行
`codex-switch-global-pace self-update --dev`；测试结束后运行
`codex-switch-global-pace self-update --stable` 回到正式版。安装器和卸载器
都会保留共享的 `~/.codex-switch` profile 数据。不要同时运行原程序与本程序的 daemon。

## Next steps

- Review stable update and verification details in [Updating](Updating.md).
- Diagnose a failed install or update with [Troubleshooting](Troubleshooting.md).
- Review supported workflows in the [Feature guide](Feature-Guide.md).
