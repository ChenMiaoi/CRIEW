# CRIEW

[![build](https://github.com/ChenMiaoi/CRIEW/actions/workflows/ci.yml/badge.svg)](https://github.com/ChenMiaoi/CRIEW/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/criew?label=latest)](https://crates.io/crates/criew)
[![docs](https://docs.rs/criew/badge.svg)](https://docs.rs/criew/)
[![codecov](https://codecov.io/github/ChenMiaoi/CRIEW/graph/badge.svg?token=AH99YLKKPD)](https://codecov.io/github/ChenMiaoi/CRIEW)

CRIEW is a Rust TUI for Linux kernel patch mail workflows.
It keeps subscription,
sync,
review,
patch application,
and reply in one terminal-first local workflow.
`CRIEW` is the repository name,
while the crate and CLI use lowercase `criew`.

Full documentation lives in the
[CRIEW wiki](https://github.com/ChenMiaoi/CRIEW/wiki).

![CRIEW TUI demo](docs/media/criew-tui-demo.gif)

Chinese quick start: [README-zh.md](README-zh.md)

## Quick Start

```bash
cargo install criew
criew doctor
criew sync --mailbox io-uring
# Fetch every message connected to one Message-ID and rebuild the local thread.
criew fetch-thread --mailbox io-uring '<reply@example.com>'
# List patch series that still need a Reviewed-by trailer.
criew review-inbox --mailbox io-uring
# Run checkpatch.pl and get_maintainer.pl before applying a series.
criew preflight --mailbox io-uring '<patch@example.com>'
# List drafts and failed replies that can be retried.
criew outbox
criew tui
```

When an incremental sync only captured part of a conversation, run
`fetch-thread` with any Message-ID from that conversation. The command follows
`References`/`In-Reply-To`, stores the newly fetched messages idempotently, and
the TUI exposes the same action as `F` on a selected thread or
`fetch-thread MESSAGE_ID` in the command palette.

`review-inbox` aggregates `Reviewed-by`, `Acked-by`, and `Tested-by` trailers
from each patch thread. Use `--mode reviewed` to show picked series or
`--mode all` to inspect both queues; the TUI offers the same views through
`review-inbox [needs-review|reviewed|all|off]` in the command palette.

`preflight` validates series completeness, runs the kernel tree's
`scripts/checkpatch.pl` and `scripts/get_maintainer.pl` when available, and
persists the result in the patch run history. The TUI exposes the same check
as `P` on a selected patch series or `preflight` in the command palette.

Press `/` in the TUI to search with structured terms such as
`subject:"mm cleanup" from:alice after:2026-01-01 is:patch`; terms can be
combined, negated with `-`, or left unqualified for the legacy subject/from/
Message-ID match.

Save a useful filter with `view save NAME`, reuse it with `view use NAME`, and
manage the persisted list with `view list` or `view delete NAME`. Saved views
are stored in the local UI state file and survive TUI restarts.

Reply drafts are saved in SQLite while the Reply Panel is open, and a failed
`git send-email` keeps both the editable draft and failure details. Reopen the
same mail to retry after fixing credentials or transport; `criew outbox` (or
the TUI `outbox` command) lists pending drafts and failed sends. In the TUI,
`C` starts an independent compose draft, `f` forwards the selected message,
and `outbox DRAFT_ID` reopens any persisted draft without returning to its
original mailbox row.

GitHub Releases publish source archives,
standalone binaries,
bundle archives,
and a `SHA256SUMS` manifest for Linux x86_64/aarch64/riscv64,
macOS x86_64/aarch64,
and Windows x86_64.
Downloaded standalone Unix binaries may need `chmod +x` after download.
If CRIEW was installed from crates.io,
run `criew update` to reinstall the latest crates.io release with Cargo.

Use the wiki before enabling IMAP,
patch application,
or reply sending.

## Documentation

- [CRIEW wiki](https://github.com/ChenMiaoi/CRIEW/wiki)
- [Install and Setup](https://github.com/ChenMiaoi/CRIEW/wiki/Install-and-Setup)
- [Configuration](https://github.com/ChenMiaoi/CRIEW/wiki/Configuration)
- [Sync and TUI](https://github.com/ChenMiaoi/CRIEW/wiki/Sync-and-TUI)
- [Patch and Reply](https://github.com/ChenMiaoi/CRIEW/wiki/Patch-and-Reply)
- [Development](https://github.com/ChenMiaoi/CRIEW/wiki/Development)
- [Contribution](https://github.com/ChenMiaoi/CRIEW/wiki/Contribution)
- [API docs on docs.rs](https://docs.rs/criew/)

## Current Release Workflow

The current source version in this branch is `v0.0.3`.
For each matching `v*` tag,
GitHub Releases publish the matching source archive together with
standalone binaries,
bundle archives,
and `SHA256SUMS` for Linux x86_64/aarch64/riscv64,
macOS x86_64/aarch64,
and Windows x86_64.

## Release Baseline

`v0.0.1` is the first supported public baseline for CRIEW.
From `v0.0.1` onward,
CRIEW supports only the CRIEW naming set:
`criew`,
`~/.criew/`,
`criew-config.toml`,
`criew.db`,
`CRIEW_B4_PATH`,
and `CRIEW_IMAP_PROXY`.
Courier-era names are unsupported.

## License

CRIEW's Rust code is licensed under [LGPL-2.1](LICENSE).
Bundled vendored components keep their upstream licenses,
including `vendor/b4` (GPL-2.0)
and `vendor/b4/patatt` (MIT-0).
