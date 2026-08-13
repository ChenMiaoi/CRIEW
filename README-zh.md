# CRIEW 中文使用说明

[![build](https://github.com/ChenMiaoi/CRIEW/actions/workflows/ci.yml/badge.svg)](https://github.com/ChenMiaoi/CRIEW/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/criew?label=latest)](https://crates.io/crates/criew)
[![docs](https://docs.rs/criew/badge.svg)](https://docs.rs/criew/)
[![codecov](https://codecov.io/github/ChenMiaoi/CRIEW/graph/badge.svg?token=AH99YLKKPD)](https://codecov.io/github/ChenMiaoi/CRIEW)

CRIEW 是一个面向 Linux kernel patch 邮件工作流的 Rust TUI 工具，
把“订阅 -> 同步 -> 阅读 -> 应用 patch -> 回复邮件”
放进同一条终端内、本地优先的工作流中。
仓库名保持大写 `CRIEW`，
crate 和 CLI 命令使用小写 `criew`。

完整文档现在以 wiki 为主：
[CRIEW wiki](https://github.com/ChenMiaoi/CRIEW/wiki)

![CRIEW TUI demo](docs/media/criew-tui-demo.gif)

English README: [README.md](README.md)

## 快速开始

```bash
cargo install criew
criew doctor
criew sync --mailbox io-uring
# 通过 IMAP 邮件头检索同步虚拟的个人关注视图。
criew sync --mailbox Following
# 按 Message-ID 补齐完整对话，并重建本地 thread。
criew fetch-thread --mailbox io-uring '<reply@example.com>'
# 列出还没有 Reviewed-by trailer 的 patch series。
criew review-inbox --mailbox io-uring
# 在应用前运行 checkpatch.pl 和 get_maintainer.pl。
criew preflight --mailbox io-uring '<patch@example.com>'
# 列出可恢复的草稿和失败回信。
criew outbox
criew tui
```

配置好 IMAP 后，TUI 会显示虚拟视图 `My Mail（我的邮件）`，不再要求单独配置个人收件箱订阅。
它包含发给你的邮件（`To`）、抄送给你的邮件（`Cc`）以及你发送的 patch（`From`），
并显示 `TO`、`CC`、`SENT` 标记。CRIEW 实际检索并同步 IMAP `INBOX`，但 `INBOX`
只是内部数据源，不会作为第二个可见订阅出现。
如果邮件服务商把已发送邮件只放在独立文件夹中，可在 `[imap]` 设置
`sent_mailbox = "Sent"`（Gmail 通常是 `[Gmail]/Sent Mail`）；Following 会用独立
checkpoint 增量导入你自己发送的 patch/reply。

如果增量同步只拿到了一部分对话，可以对其中任意一封邮件的
`Message-ID` 运行 `fetch-thread`。命令会沿着 `References`/`In-Reply-To`
查找并幂等写入缺失邮件；TUI 中也可以在线程列表选中邮件后按 `F`，或在
命令栏输入 `fetch-thread MESSAGE_ID`。

`review-inbox` 会聚合每个 patch thread 中的 `Reviewed-by`、`Acked-by` 和
`Tested-by` trailer。使用 `--mode reviewed` 查看已经收到 review 的 series，
使用 `--mode all` 查看全部；TUI 命令栏对应支持
`review-inbox [needs-review|reviewed|all|off]`。

`preflight` 会先校验 series 是否完整；如果本地配置了 kernel tree，就执行
`scripts/checkpatch.pl` 和 `scripts/get_maintainer.pl`，并把结果写入 patch
运行历史。TUI 在线程列表选中 patch series 后按 `P`，或在命令栏输入
`preflight`，可以执行同一项检查。

TUI 中按 `/` 可以使用结构化搜索，例如
`subject:"mm cleanup" from:alice after:2026-01-01 is:patch`；多个条件会同时生效，
在条件前加 `-` 表示排除，不带字段的词仍按原来的 subject/from/Message-ID 全文匹配。
可以用命令栏 `view save NAME` 保存当前条件，用 `view use NAME` 重用，或用
`view list`、`view delete NAME` 管理；Saved View 会写入本地 UI 状态文件，重启 TUI 后仍在。

邮件 `Preview` 面板仍然保持正文的纯文本语义，但会用终端颜色帮助快速定位审阅重点：
头部使用青色，diff 文件标记使用洋红色，hunk 标记使用蓝色，新增行使用绿色，删除行使用红色，
引用上下文使用浅灰色，代码围栏使用青色，解析警告使用黄色。普通叙述保持终端默认样式，颜色不会
改变邮件文本或滚动行为。

Reply Panel 打开后会把草稿保存到 SQLite；`git send-email` 失败时会保留可编辑
草稿、失败原因和 draft 文件。修复凭据或发送链路后，重新打开同一封邮件即可重试；
`criew outbox` 或 TUI 命令栏的 `outbox` 会列出待处理草稿和失败回信。TUI 中按
`C` 可新建独立邮件，按 `f` 转发当前邮件，输入 `outbox DRAFT_ID` 可直接恢复任意草稿，
不必先回到原邮箱行。

GitHub Releases 会发布源码包、
独立二进制文件、
压缩包，
以及 `SHA256SUMS` 校验清单，
覆盖 Linux x86_64/aarch64/riscv64、
macOS x86_64/aarch64，
和 Windows x86_64。
在类 Unix 系统上，
直接下载独立二进制后可能还需要执行一次 `chmod +x`。
如果 CRIEW 是通过 crates.io 安装的，
可以运行 `criew update`，
让 Cargo 重新安装最新的 crates.io 发布版本。

启用 IMAP、
patch apply、
或回信发送前，
请先阅读 wiki 中对应的页面。

## 文档

- [CRIEW wiki](https://github.com/ChenMiaoi/CRIEW/wiki)
- [安装与初始化](https://github.com/ChenMiaoi/CRIEW/wiki/Install-and-Setup)
- [配置说明](https://github.com/ChenMiaoi/CRIEW/wiki/Configuration)
- [同步与 TUI](https://github.com/ChenMiaoi/CRIEW/wiki/Sync-and-TUI)
- [Patch 与回信](https://github.com/ChenMiaoi/CRIEW/wiki/Patch-and-Reply)
- [开发与本地 wiki 构建](https://github.com/ChenMiaoi/CRIEW/wiki/Development)
- [贡献流程](https://github.com/ChenMiaoi/CRIEW/wiki/Contribution)
- [docs.rs API 文档](https://docs.rs/criew/)

## 当前发布流程

当前分支里的源码版本为 `v0.0.3`。
对每个匹配的 `v*` tag，
GitHub Releases 都会发布对应的源码包，
以及 Linux x86_64/aarch64/riscv64、
macOS x86_64/aarch64、
Windows x86_64 的独立二进制、
压缩包和 `SHA256SUMS` 校验清单。

## 发布基线

`v0.0.1` 是 CRIEW 第一版对外支持的发布基线。
从 `v0.0.1` 开始，
项目只支持 CRIEW 这一套命名：
`criew`、
`~/.criew/`,
`criew-config.toml`,
`criew.db`,
`CRIEW_B4_PATH`,
和 `CRIEW_IMAP_PROXY`。
Courier 时代的命名不再受支持。

## License

CRIEW 自身的 Rust 代码使用 [LGPL-2.1](LICENSE) 许可证发布。
打包进来的 vendored 组件保留各自上游许可证，
包括 `vendor/b4`（GPL-2.0）
和 `vendor/b4/patatt`（MIT-0）。
