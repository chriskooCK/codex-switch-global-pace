# codex-switch-global-pace

**[OpenAI Codex CLI](https://github.com/openai/codex) 多账号管理与全局周配额仪表盘。**
它把所有可用账号的 Weekly quota 视为一个 pool，同时保留登录、切换、预热、
重置卡、JSON 与 daemon 等既有功能。

[English README](README.md) · [文档](docs/wiki/Home.md) ·
[Releases](https://github.com/chriskooCK/codex-switch-global-pace/releases)

> 本程序会管理本机认证文件。请勿公开 profile、`auth.json`、Token、代理凭据或未脱敏的 debug 输出。

## 快速开始

Codex 必须使用文件型凭据存储。在 `$CODEX_HOME/config.toml`（通常为
`~/.codex/config.toml`）中确认：

```toml
cli_auth_credentials_store = "file"
```

macOS / Linux：

```bash
curl -fsSL https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.sh | bash
```

Windows PowerShell：

```powershell
irm https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1 | iex
```

常用命令：

```bash
codex-switch-global-pace login
codex-switch-global-pace list
codex-switch-global-pace use
codex-switch-global-pace          # 打开交互式仪表盘
```

不带参数运行会直接打开 TUI；Windows 双击
`codex-switch-global-pace.exe` 也是相同效果。
`use` 会切换下一个 Codex 进程使用的实际账号；切换后请重新启动已运行的 Codex 应用或 CLI。

## Global Weekly Pace

每个 weekly window 的计算同时考虑剩余配额与 reset 距离：

```text
effective capacity = 100% + elapsed% - used%
normal capacity    = included accounts × 100%
global pace        = sum(effective capacity) / normal capacity × 100%
```

`100%` 表示正常配速，超过 `100%` 表示有 reserve，低于 `100%` 表示 deficit。
只要 reset timestamp 有效，即使账号已用到 `100%` 也会纳入。数据或 reset 不可信的账号会计为
unavailable。当前 API 没有可靠且可比较的 weekly capacity，因此账号采用 equal weighting。

## 复用既有 profile

本程序有意继续使用原数据目录：macOS/Linux 为 `~/.codex-switch`，Windows 为
`%USERPROFILE%\.codex-switch`，也继续支持 `CODEX_SWITCH_HOME`。既有
`profiles/`、`cache.json`、`config.toml`、`current` 与 `daemon-state.json`
无需重新登录即可使用，安装器和卸载器都不会删除这个共享目录。

> 不要同时运行 `codex-switch` 与 `codex-switch-global-pace` 的 daemon。
> 两者会共享 profile、cache、lock、当前账号与 daemon state。启用一个 daemon 前，请先停止并
> 卸载另一个 daemon service。TUI 和普通交互命令不依赖 daemon。

## 独立更新与来源

`self-update` 只读取
`chriskooCK/codex-switch-global-pace` 的 Release，绝不会下载原项目的 Release。
版本号采用 `YYYYMMDD.N.0`；其中日期是 dev candidate 的版本分配日期，stable
promotion 即使跨日也继续使用同一版本。

```bash
codex-switch-global-pace self-update --check
codex-switch-global-pace self-update
codex-switch-global-pace self-update --dev
codex-switch-global-pace self-update --stable
```

直装更新同时验证 SHA-256 与 GitHub build provenance，并用
`gh attestation verify` 绑定本 repository、release workflow、tag ref 与 tag commit；
因此需要当前版 [GitHub CLI](https://cli.github.com/)。

本项目来自 `xjoker/codex-switch` 的一次性 source snapshot，之后独立开发，
不追踪或自动同步原 repository。准确 revision 与 attribution 见 [NOTICE.md](NOTICE.md)。

## 许可证

[MIT](LICENSE) — 保留原作者的 copyright 与 license notice。
