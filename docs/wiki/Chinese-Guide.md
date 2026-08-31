# 中文指南

> 英文文档是 `codex-switch-global-pace` 的主文档与行为依据。本页提供中文快速入口，不单独维护第二套实现说明。

`codex-switch-global-pace` 用于管理本机多个 Codex 登录、查看额度，并在新会话前选择账号。本项目是独立非官方项目，与 OpenAI 无隶属或背书关系；请仅用于您拥有或获准使用的账号。

Global Weekly Pace 是本机 equal-weight 汇总视图，不会在账号之间转移、合并或绕过 quota。填充的 bar 表示汇总后的实际使用量，pace marker 表示按已用时间计算的理想使用位置，文字摘要只显示参与账号数和最早 reset。程序会操作 Codex 的文件型认证，因此请勿分享 profile、`auth.json`、Token、代理凭据或未经脱敏的 debug 输出。

## 最常用：切换 Codex Windows 应用的当前账号

1. 保存当前工作并关闭 Codex 窗口。
2. 打开 Windows 通知区域的隐藏图标，找到 Codex 桌面应用的 **ChatGPT**
   tray 图标（某些版本可能显示 **Codex**），右键后选择 **Quit** 或 **Exit**。
3. 等到 tray 图标完全消失后，再运行：

   ```powershell
   codex-switch-global-pace list -f
   codex-switch-global-pace use work
   # 或自动选择：codex-switch-global-pace use
   ```

4. 重新打开 Codex Windows 应用。新进程会读取切换后的 `$CODEX_HOME/auth.json`。

只关闭窗口并不够；tray 图标仍存在时，后台进程可能还在运行。Codex CLI 用户也应先退出所有正在运行的 Codex session/process（`codex`、`codex resume` 或 `codex exec`），切换成功后再启动新进程。

## 快速开始

Codex 必须使用 file credential store。原生 Windows PowerShell 中的配置通常是
`%USERPROFILE%\.codex\config.toml`；macOS/Linux 中通常是
`~/.codex/config.toml`。不要从 WSL shell 执行 Windows 应用的切换命令，
因为 WSL 默认使用另一套 home 与认证文件。请在对应文件中确认：

```toml
cli_auth_credentials_store = "file"
```

请使用仓库中经过代码审查的[已验证安装流程](Getting-Started.md#install)
安装正式版。该流程先用当前版 GitHub CLI 验证安装器 attestation，再运行本地文件；
验证失败时不会降级为未验证安装。

```bash
gh auth login
gh auth status
```

GitHub 登录只用于下载并验证 Release，不会成为 Codex 账号。

如果未传 `--system`，脚本却显示 `Installing to /usr/local/bin (requires sudo)`，说明运行的是旧 `master` 分支中的已淘汰脚本，请终止并改用上面的 Release 地址。当前脚本默认安装到 `~/.local/bin`；只有清理 `/usr/local/bin` 中由 root 持有的旧二进制时，才会请求一次 `sudo`。

建议用清楚的 alias 分别添加个人与工作账号。不同的新身份会保存并立即成为
当前账号；如果浏览器身份已存在于其他 profile，则会更新并激活匹配的
profile，而不会创建请求的新 alias：

```bash
codex-switch-global-pace login personal
# 确认浏览器显示的是个人账号，再批准登录

codex-switch-global-pace login work
# 必要时先切换浏览器账号，再批准登录

codex-switch-global-pace list -f
codex-switch-global-pace          # 打开交互界面
```

无浏览器服务器使用 `codex-switch-global-pace login --device`。
Alias 长度为 1–64 个 ASCII 字节，只能包含字母、数字、`_`、`-` 和 `.`；`.` 与 `..` 不可使用。重复使用 alias 只能重新授权同一实际账号，不能用另一账号覆盖；如果它原本不是当前账号，请在重新授权后运行 `use <alias>` 才会切换过去。

本程序复用已有的 profile：Windows 原生环境通常位于
`%USERPROFILE%\.codex-switch`，macOS/Linux 通常位于 `~/.codex-switch`。
无需重新登录。不要同时运行原程序与本程序的 daemon；启用一个前请停止并卸载另一个 daemon service。

## 参与开发版测试

开发版属于滚动 prerelease 通道。安装、验证、回退和问题反馈步骤见 [Testing development releases](Development-Releases.md)，其中附有中文摘要。

## 常用入口

- [开始使用](Getting-Started.md) — 安装、登录和首次启动
- [功能指南](Feature-Guide.md) — 主要工作流与安全边界
- [命令参考](Command-Reference.md) — 全部命令、全局选项和 TUI 快捷键
- [配置](Configuration.md) — 路径、代理与 daemon 设置
- [更新](Updating.md) — 更新方式、通道切换和旧版本迁移
- [故障排查](Troubleshooting.md) — 常见错误与恢复方式
- [常见问题](FAQ.md) — 简短项目说明

命令行为以已安装版本的 `codex-switch-global-pace <命令> --help` 为最终依据。

## 反馈问题

提交 Issue 时请附操作系统、终端、`codex-switch-global-pace --version`、完整命令、预期结果、实际结果与最小复现步骤。分享 debug 输出前必须删除 Token、邮箱、account ID、工作区名称、可识别身份的路径和代理凭据。

[提交 GitHub Issue](https://github.com/chriskooCK/codex-switch-global-pace/issues)

## Next steps

- 第一次使用：继续阅读[开始使用](Getting-Started.md)。
- 日常操作：查看[功能指南](Feature-Guide.md)。
- 遇到错误：进入[故障排查](Troubleshooting.md)。
