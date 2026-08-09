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
# 按 Message-ID 补齐完整对话，并重建本地 thread。
criew fetch-thread --mailbox io-uring '<reply@example.com>'
criew tui
```

如果增量同步只拿到了一部分对话，可以对其中任意一封邮件的
`Message-ID` 运行 `fetch-thread`。命令会沿着 `References`/`In-Reply-To`
查找并幂等写入缺失邮件；TUI 中也可以在线程列表选中邮件后按 `F`，或在
命令栏输入 `fetch-thread MESSAGE_ID`。

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
