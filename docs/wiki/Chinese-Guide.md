# 中文指南

> 英文文档是 `codex-switch-global-pace` 的主文档与行为依据。本页提供中文快速入口，不单独维护第二套实现说明。

`codex-switch-global-pace` 用于管理本机多个 OpenAI Codex CLI 登录、查看额度，并在新会话前选择合适账号。Global Weekly Pace 会把所有可用账号的周配额按 reset 时间合并成一个 equal-weight pool；`100%` 是正常配速，以上是 reserve，以下是 deficit。它会操作 Codex 的文件型认证，因此请勿分享 profile、`auth.json`、Token、代理凭据或未经脱敏的 debug 输出。

## 快速开始

Codex 必须使用 file credential store。在 `$CODEX_HOME/config.toml`（通常是 `~/.codex/config.toml`）中确认：

```toml
cli_auth_credentials_store = "file"
```

请使用仓库中经过代码审查的[已验证安装流程](Getting-Started.md#install)
安装正式版。该流程先用 GitHub CLI 验证安装器 attestation，再运行本地文件；
验证失败时不会降级为未验证安装。

如果未传 `--system`，脚本却显示 `Installing to /usr/local/bin (requires sudo)`，说明运行的是旧 `master` 分支中的已淘汰脚本，请终止并改用上面的 Release 地址。当前脚本默认安装到 `~/.local/bin`；只有清理 `/usr/local/bin` 中由 root 持有的旧二进制时，才会请求一次 `sudo`。

添加账号并打开界面：

```bash
codex-switch-global-pace login
codex-switch-global-pace          # 打开交互界面
```

无浏览器服务器使用 `codex-switch-global-pace login --device`。

本程序复用 `~/.codex-switch` 中已有的 profile，无需重新登录。不要同时运行原程序与本程序的 daemon；启用一个前请停止并卸载另一个 daemon service。

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
