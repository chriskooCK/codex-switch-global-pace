# codex-switch-global-pace

**本机 Codex 多账号管理、当前账号切换与 Global Weekly Pace 仪表盘。**
它会为下一次启动的 Codex 选择账号，并以 equal-weight 方式显示多个账号的
Weekly quota；quota 始终分别属于各账号，不会被转移、合并或绕过。

[English README](README.md) · [文档](docs/wiki/Home.md) ·
[Releases](https://github.com/chriskooCK/codex-switch-global-pace/releases)

> **独立非官方项目：**本项目与 OpenAI 无隶属或背书关系。请仅用于您拥有或获准使用的账号。

> 本程序会管理本机认证文件。请勿公开 profile、`auth.json`、Token、代理凭据或未脱敏的 debug 输出。

## 最常用：切换 Codex Windows 应用的当前账号

交互式仪表盘是日常切换账号的主要方式。

**账号切换按键：** `↑`/`↓`（或 `j`/`k`）→ `Enter` → `u`

1. 保存当前工作并关闭所有 Codex 窗口。
2. 打开 Windows 通知区域（包括 `^` 后的隐藏图标），找到 Codex 桌面应用的
   **ChatGPT** tray 图标（某些版本可能显示 **Codex**），右键选择 **Quit** 或
   **Exit**，并确认图标已经消失。
3. 图标完全消失后，在 PowerShell 或命令提示符中打开交互式仪表盘：

   ```powershell
   codex-switch-global-pace
   ```

4. 使用 `↑`/`↓` 或 `j`/`k` 选择账号，按 `Enter` 打开该账号的菜单，然后按
   `u`（**Use**）将它设为当前账号。`Enter` 和 `u` 是连续操作，不是二选一的
   快捷键。
5. 等待界面显示 `Switched to <alias>`，然后按 `q` 关闭仪表盘。
6. 重新打开 Codex Windows 应用；新进程会从 `$CODEX_HOME/auth.json` 读取已切换的账号。

### 命令行备选方式

完全退出 Codex 后，也可以直接使用命令行切换：

```powershell
codex-switch-global-pace list -f
codex-switch-global-pace use work
# 或自动选择：codex-switch-global-pace use
```

只关闭窗口并不够；tray 图标仍存在时，后台进程可能还在运行。使用 Codex CLI
时也应先退出所有正在运行的 Codex session/process（`codex`、`codex resume`
或 `codex exec`），切换成功后再启动新进程。

## 快速开始

Codex 必须使用文件型凭据存储。在原生 Windows PowerShell 中，该文件通常为
`%USERPROFILE%\.codex\config.toml`；在 macOS/Linux 中通常为
`~/.codex/config.toml`。不要在 WSL shell 中执行 Windows 应用的切换命令，
因为 WSL 默认使用另一套 home 与认证文件。请在对应文件中确认：

```toml
cli_auth_credentials_store = "file"
```

请使用仓库中经过代码审查的
[已验证安装流程](docs/wiki/Getting-Started.md#install)。该流程使用当前版 [GitHub CLI](https://cli.github.com/)
通过 `gh attestation verify` 先验证安装器 attestation，再运行本地文件；
验证失败时不会降级为未验证安装。

安装前先确认 GitHub CLI 已登录：

```bash
gh auth login
gh auth status
```

GitHub 登录只用于下载并验证 Release，不会成为 Codex 账号。

## 添加并验证个人与工作账号

先用不同 alias 保存账号。新身份会保存并立即成为当前账号；如果浏览器身份已
存在于其他 profile，则会更新并激活匹配的 profile，而不会创建请求的新 alias：

```powershell
codex-switch-global-pace login personal
# 确认浏览器显示的是个人账号，再批准登录

codex-switch-global-pace login work
# 必要时先切换浏览器账号，再批准登录

codex-switch-global-pace list -f
```

Alias 长度为 1–64 个 ASCII 字节，只能包含字母、数字、`_`、`-` 和 `.`；`.` 与 `..` 不可使用。身份字段完整的既有 alias 只能重新授权同一实际账号，不能用另一账号覆盖；如果它原本不是当前账号，请在重新授权后运行 `use <alias>` 才会切换过去。

每次 OAuth 登录都必须同时提供非空的 `account_id` 和邮箱。旧版本创建的
profile 如果缺少其中一项，交互式 `login <alias>` 会显示默认 **No** 的确认；
JSON 或其他非交互运行必须显式使用 `login <alias> --yes`。确认后，程序先把
原认证文件完整归档到 `deleted-profiles/`，并且只有新账号与所有已知身份字段
一致时，才会替换同一个 alias。

服务轮换一次性 refresh token 后，返回的轮换材料会先持久化到私有
`recovery/` 目录，再尝试写入 profile。profile 持久化之前发生冲突或失败时，
程序会保留并报告该材料。profile 已持久化后，即使随后的实时认证激活失败，
也可以精确清理原暂存文件，因此这种部分提交不一定有恢复路径。精确清理失败
时，只有仍能确认原暂存文件身份才会报告该路径，否则不会把无关文件说成恢复
文件，也不会自行重试已经消耗的 token。

常用命令：

```bash
codex-switch-global-pace login
codex-switch-global-pace list
codex-switch-global-pace use
codex-switch-global-pace          # 打开交互式仪表盘
```

Windows 双击 `codex-switch-global-pace.exe` 也会打开相同的 TUI。
`use` 会切换下一个 Codex 进程使用的实际账号；切换前须完全退出已运行的
Codex 应用（包括 tray）或 CLI。

## Global Weekly Pace

仪表盘会把所有具有有效 weekly window 的账号以 equal-weight 方式显示在一个本机汇总视图中。
这只是可视化：账号 quota 仍彼此独立。填充的 bar 表示汇总后的实际使用量，
`↑ pace` marker 表示汇总后的已用时间，也就是当前理想使用位置。meter 下方只显示参与账号数和最早到来的账号 reset。

只要 reset timestamp 有效，即使账号已用到 `100%` 也会纳入。数据或 reset 不可信的账号会计为
unavailable，而不会猜测。当前 API 没有可靠且可比较的 weekly capacity，因此账号采用
equal weighting。

所有 quota meter 都使用同一个相对规则：实际使用量高于按已用时间计算的 pace 时显示黄色，
等于或低于 pace 时显示绿色。Global meter 对汇总后的使用量和已用时间应用相同规则。
配额完全耗尽不会形成第三种警告状态；只要比较有效，仍按上述规则显示黄色或绿色并保留 pace marker。无法比较时保持中性色，quota 标签不再追加警告符号。

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
