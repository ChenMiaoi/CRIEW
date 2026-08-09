//! Application-layer entrypoints and orchestration.
//!
//! This module keeps command routing and high-level policy in one place so the
//! lower layers can stay focused on storage, network, and UI primitives.

pub mod cli;
pub mod patch;
pub mod sync;
pub mod update;

use std::path::PathBuf;

use clap::Parser;

use crate::infra::b4::{self, B4Status};
use crate::infra::bootstrap;
use crate::infra::config::{self, IMAP_INBOX_MAILBOX};
use crate::infra::error::Result;
use crate::infra::imap::{ImapClient, MailboxSnapshot, RemoteImapClient};
use crate::infra::logging;
use crate::infra::sendmail::{self, GitSendEmailCheck, GitSendEmailStatus, ReplyIdentity};

const DEFAULT_SYNC_RECONNECT_ATTEMPTS: u8 = 3;

enum DoctorImapStatus {
    Skipped(Vec<String>),
    Connected(MailboxSnapshot),
    Error(String),
}

pub fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    let command = cli.command.unwrap_or(cli::Command::Tui);

    if matches!(command, cli::Command::Version) {
        println!("criew {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let cli::Command::Update { dry_run } = &command {
        let summary = update::run(update::UpdateRequest { dry_run: *dry_run })?;
        println!("{}", format_update_summary(&summary));
        return Ok(());
    }

    let mut runtime = config::load(cli.config.as_deref())?;
    logging::init(&runtime.log_filter, &runtime.log_dir)?;
    tracing::debug!(command = ?command, "user invoked cli command");

    let startup_config_path = runtime.config_path.clone();
    let mut bootstrap_state = bootstrap::prepare(&runtime)?;

    match command {
        cli::Command::Tui => loop {
            match crate::ui::run(&runtime, &bootstrap_state)? {
                crate::ui::TuiAction::Exit => break Ok(()),
                crate::ui::TuiAction::Restart => {
                    tracing::info!(
                        config_path = %startup_config_path.display(),
                        "user requested tui restart"
                    );
                    runtime = config::load(Some(startup_config_path.as_path()))?;
                    bootstrap_state = bootstrap::prepare(&runtime)?;
                }
            }
        },
        cli::Command::Sync {
            mailbox,
            fixture_dir,
            uidvalidity,
            reconnect_attempts,
        } => {
            let request = build_sync_request(
                &runtime,
                mailbox,
                fixture_dir,
                uidvalidity,
                reconnect_attempts,
            );
            run_sync_command(&runtime, &bootstrap_state, request, true)?;
            Ok(())
        }
        cli::Command::FetchThread {
            mailbox,
            message_id,
        } => {
            let mailbox = mailbox.unwrap_or_else(|| runtime.source_mailbox.clone());
            let summary = sync::fetch_thread(
                &runtime,
                sync::ThreadFetchRequest {
                    mailbox,
                    message_id,
                    reconnect_attempts: DEFAULT_SYNC_RECONNECT_ATTEMPTS,
                },
            )?;
            println!("{}", format_sync_summary(&summary));
            Ok(())
        }
        cli::Command::Doctor => {
            let b4_status = b4::check(runtime.b4_path.as_deref(), Some(&runtime.data_dir));
            let send_email_status = sendmail::check();
            let reply_identity = sendmail::resolve_reply_identity();

            let self_email = config::resolve_self_email(&runtime);
            let imap_status = probe_doctor_imap(&runtime);
            println!(
                "{}",
                format_doctor_report(
                    &runtime,
                    &bootstrap_state,
                    &self_email,
                    &imap_status,
                    &send_email_status,
                    &reply_identity,
                    &b4_status,
                )
            );

            Ok(())
        }
        cli::Command::Update { .. } => Ok(()),
        cli::Command::Version => Ok(()),
    }
}

fn run_sync_command(
    runtime: &config::RuntimeConfig,
    bootstrap_state: &bootstrap::BootstrapState,
    request: sync::SyncRequest,
    print_summary: bool,
) -> Result<sync::SyncSummary> {
    let summary = sync::run(runtime, request)?;

    tracing::info!(
        database = %bootstrap_state.db.path.display(),
        schema_version = bootstrap_state.db.schema_version,
        mailbox = %summary.mailbox,
        fetched = summary.fetched,
        inserted = summary.inserted,
        updated = summary.updated,
        rebuilt_roots = summary.rebuilt_roots,
        "sync command finished"
    );

    if print_summary {
        println!("{}", format_sync_summary(&summary));
    }

    Ok(summary)
}

fn build_sync_request(
    runtime: &config::RuntimeConfig,
    mailbox: Option<String>,
    fixture_dir: Option<PathBuf>,
    uidvalidity: Option<u64>,
    reconnect_attempts: Option<u8>,
) -> sync::SyncRequest {
    sync::SyncRequest {
        mailbox: mailbox.unwrap_or_else(|| runtime.source_mailbox.clone()),
        fixture_dir,
        uidvalidity,
        reconnect_attempts: reconnect_attempts.unwrap_or(DEFAULT_SYNC_RECONNECT_ATTEMPTS),
    }
}

fn probe_doctor_imap(runtime: &config::RuntimeConfig) -> DoctorImapStatus {
    if !runtime.imap.is_complete() {
        return DoctorImapStatus::Skipped(
            runtime
                .imap
                .missing_required_fields()
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        );
    }

    match RemoteImapClient::new(runtime.imap.clone()).and_then(|mut client| {
        client.connect()?;
        client.select_mailbox(IMAP_INBOX_MAILBOX)
    }) {
        Ok(snapshot) => DoctorImapStatus::Connected(snapshot),
        Err(error) => DoctorImapStatus::Error(error.to_string()),
    }
}

fn format_doctor_report(
    runtime: &config::RuntimeConfig,
    bootstrap_state: &bootstrap::BootstrapState,
    self_email: &config::SelfEmailResolution,
    imap_status: &DoctorImapStatus,
    send_email_status: &GitSendEmailCheck,
    reply_identity: &std::result::Result<ReplyIdentity, String>,
    b4_status: &b4::B4Check,
) -> String {
    let mut lines = vec![
        "criew doctor".to_string(),
        format!("  config_path: {}", runtime.config_path.display()),
        format!("  data_dir: {}", runtime.data_dir.display()),
        format!("  database_path: {}", runtime.database_path.display()),
        format!("  raw_mail_dir: {}", runtime.raw_mail_dir.display()),
        format!("  patch_dir: {}", runtime.patch_dir.display()),
        format!("  log_dir: {}", runtime.log_dir.display()),
        format!("  source_mailbox: {}", runtime.source_mailbox),
        format!(
            "  default_active_mailbox: {}",
            runtime.default_active_mailbox()
        ),
        format!("  lore_base_url: {}", runtime.lore_base_url),
        format!("  startup_sync: {}", runtime.startup_sync),
        format!(
            "  inbox_auto_sync_interval_secs: {}",
            runtime.inbox_auto_sync_interval_secs
        ),
    ];
    if runtime.kernel_trees.is_empty() {
        lines.push("  kernel_trees: <none>".to_string());
    } else {
        lines.extend(
            runtime
                .kernel_trees
                .iter()
                .map(|tree| format!("  kernel_tree: {}", tree.display())),
        );
    }
    lines.push(format!(
        "  schema_version: {}",
        bootstrap_state.db.schema_version
    ));
    lines.push(format!(
        "  schema_version_expected: {}",
        crate::infra::db::CURRENT_SCHEMA_VERSION
    ));
    lines.push(format!(
        "  migrations_applied: {:?}",
        bootstrap_state.db.applied_migrations
    ));
    lines.push(format!(
        "  database_created_this_run: {}",
        bootstrap_state.db.created
    ));
    lines.push(format!(
        "  self_email: {}",
        self_email.email.as_deref().unwrap_or("<missing>")
    ));
    lines.push(format!(
        "  self_email_source: {}",
        self_email
            .source
            .map(|source| source.as_str())
            .unwrap_or("<none>")
    ));
    if let Some(error) = self_email.git_error.as_deref() {
        lines.push(format!("  self_email_lookup: error ({error})"));
    }
    lines.push(format!(
        "  imap_config_status: {}",
        if runtime.imap.is_complete() {
            "complete"
        } else {
            "incomplete"
        }
    ));
    match imap_status {
        DoctorImapStatus::Skipped(missing_fields) => {
            lines.push(format!(
                "  imap_missing_fields: {}",
                if missing_fields.is_empty() {
                    "<none>".to_string()
                } else {
                    missing_fields.join(", ")
                }
            ));
            lines.push("  imap_connect_status: skipped".to_string());
        }
        DoctorImapStatus::Connected(snapshot) => {
            lines.push(format!(
                "  imap_connection_target: {}:{} ({})",
                runtime.imap.server.as_deref().unwrap_or("<missing>"),
                runtime.imap.server_port.unwrap_or_default(),
                runtime
                    .imap
                    .encryption
                    .map(|value| value.as_str())
                    .unwrap_or("<missing>")
            ));
            lines.push("  imap_connect_status: ok".to_string());
            lines.push(format!("  imap_select_mailbox: {}", IMAP_INBOX_MAILBOX));
            lines.push(format!("  imap_uidvalidity: {}", snapshot.uidvalidity));
            lines.push(format!("  imap_highest_uid: {}", snapshot.highest_uid));
            lines.push(format!(
                "  imap_highest_modseq: {}",
                snapshot
                    .highest_modseq
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            ));
        }
        DoctorImapStatus::Error(error) => {
            lines.push(format!(
                "  imap_connection_target: {}:{} ({})",
                runtime.imap.server.as_deref().unwrap_or("<missing>"),
                runtime.imap.server_port.unwrap_or_default(),
                runtime
                    .imap
                    .encryption
                    .map(|value| value.as_str())
                    .unwrap_or("<missing>")
            ));
            lines.push(format!("  imap_connect_status: error ({error})"));
        }
    }
    match &send_email_status.status {
        GitSendEmailStatus::Available { path, version } => {
            lines.push(format!("  git_send_email_path: {}", path.display()));
            lines.push(format!("  git_send_email_version: {}", version));
            lines.push("  git_send_email_status: ok".to_string());
        }
        GitSendEmailStatus::Broken { path, reason } => {
            lines.push(format!("  git_send_email_path: {}", path.display()));
            lines.push(format!("  git_send_email_status: broken ({reason})"));
        }
        GitSendEmailStatus::Missing => {
            lines.push("  git_send_email_path: <not found>".to_string());
            lines.push("  git_send_email_status: missing".to_string());
        }
    }
    match reply_identity {
        Ok(identity) => {
            lines.push(format!("  reply_from: {}", identity.display));
            lines.push(format!("  reply_from_email: {}", identity.email));
            lines.push(format!("  reply_from_source: {}", identity.source.as_str()));
            lines.push("  reply_identity_status: ok".to_string());
        }
        Err(error) => {
            lines.push("  reply_from: <missing>".to_string());
            lines.push("  reply_from_email: <missing>".to_string());
            lines.push("  reply_from_source: <none>".to_string());
            lines.push(format!("  reply_identity_status: error ({error})"));
        }
    }
    match &b4_status.status {
        B4Status::Available { path, version } => {
            lines.push(format!("  b4_path: {}", path.display()));
            lines.push(format!("  b4_version: {}", version));
            lines.push("  b4_status: ok".to_string());
        }
        B4Status::Broken { path, reason } => {
            lines.push(format!("  b4_path: {}", path.display()));
            lines.push(format!("  b4_status: broken ({reason})"));
        }
        B4Status::Missing => {
            lines.push("  b4_path: <not found>".to_string());
            lines.push("  b4_status: missing".to_string());
        }
    }
    lines.join("\n")
}

fn format_sync_summary(summary: &sync::SyncSummary) -> String {
    format!(
        "sync complete: mailbox={} source={} fetched={} inserted={} updated={} rebuilt_roots={} uidvalidity={} last_seen_uid={} highest_modseq={:?} synced_at={} mailbox_rebuilt={}",
        summary.mailbox,
        summary.source,
        summary.fetched,
        summary.inserted,
        summary.updated,
        summary.rebuilt_roots,
        summary.uidvalidity,
        summary.checkpoint_last_seen_uid,
        summary.checkpoint_highest_modseq,
        summary
            .checkpoint_synced_at
            .as_deref()
            .unwrap_or("<unknown>"),
        summary.mailbox_rebuilt
    )
}

fn format_update_summary(summary: &update::UpdateSummary) -> String {
    let status = match summary.status {
        update::UpdateStatus::DryRun => "dry-run",
        update::UpdateStatus::Updated => "updated",
    };
    format!(
        "criew update\n  command: {}\n  status: {}",
        summary.command_line, status
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::infra::b4::B4Status;
    use crate::infra::bootstrap::BootstrapState;
    use crate::infra::config::{
        DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS, ImapConfig, ImapEncryption, RuntimeConfig,
        SelfEmailResolution, SelfEmailSource, UiKeymap,
    };
    use crate::infra::db::DatabaseState;
    use crate::infra::imap::MailboxSnapshot;
    use crate::infra::sendmail::{
        GitSendEmailCheck, GitSendEmailStatus, ReplyIdentity, ReplyIdentitySource,
    };

    use super::{
        DoctorImapStatus, build_sync_request, format_doctor_report, format_sync_summary,
        format_update_summary, probe_doctor_imap,
    };

    fn test_runtime() -> RuntimeConfig {
        RuntimeConfig {
            config_path: PathBuf::from("/tmp/criew-config.toml"),
            data_dir: PathBuf::from("/tmp/criew"),
            database_path: PathBuf::from("/tmp/criew/criew.db"),
            raw_mail_dir: PathBuf::from("/tmp/criew/raw"),
            patch_dir: PathBuf::from("/tmp/criew/patches"),
            log_dir: PathBuf::from("/tmp/criew/logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "linux-kernel".to_string(),
            imap: ImapConfig::default(),
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs: DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        }
    }

    fn test_bootstrap_state() -> BootstrapState {
        BootstrapState {
            db: DatabaseState {
                path: PathBuf::from("/tmp/criew/criew.db"),
                schema_version: crate::infra::db::CURRENT_SCHEMA_VERSION,
                created: false,
                applied_migrations: (1..=crate::infra::db::CURRENT_SCHEMA_VERSION).collect(),
            },
        }
    }

    #[test]
    fn build_sync_request_uses_runtime_defaults_and_overrides() {
        let runtime = test_runtime();

        let default_request = build_sync_request(&runtime, None, None, None, None);
        assert_eq!(default_request.mailbox, "linux-kernel");
        assert_eq!(
            default_request.reconnect_attempts,
            super::DEFAULT_SYNC_RECONNECT_ATTEMPTS
        );

        let overridden_request = build_sync_request(
            &runtime,
            Some("INBOX".to_string()),
            Some(PathBuf::from("/tmp/fixtures")),
            Some(42),
            Some(7),
        );
        assert_eq!(overridden_request.mailbox, "INBOX");
        assert_eq!(
            overridden_request.fixture_dir,
            Some(PathBuf::from("/tmp/fixtures"))
        );
        assert_eq!(overridden_request.uidvalidity, Some(42));
        assert_eq!(overridden_request.reconnect_attempts, 7);
    }

    #[test]
    fn format_sync_summary_renders_unknown_synced_at_fallback() {
        let summary = crate::app::sync::SyncSummary {
            mailbox: "linux-kernel".to_string(),
            source: "fixture".to_string(),
            fetched: 3,
            inserted: 2,
            updated: 1,
            rebuilt_roots: 4,
            mailbox_rebuilt: false,
            uidvalidity: 11,
            checkpoint_last_seen_uid: 99,
            checkpoint_highest_modseq: Some(123),
            checkpoint_synced_at: None,
        };

        let rendered = format_sync_summary(&summary);

        assert!(rendered.contains("mailbox=linux-kernel"));
        assert!(rendered.contains("source=fixture"));
        assert!(rendered.contains("synced_at=<unknown>"));
    }

    #[test]
    fn format_doctor_report_covers_incomplete_imap_and_missing_tools() {
        let runtime = test_runtime();
        let self_email = SelfEmailResolution {
            email: None,
            source: None,
            git_error: Some("git config failed".to_string()),
        };
        let report = format_doctor_report(
            &runtime,
            &test_bootstrap_state(),
            &self_email,
            &DoctorImapStatus::Skipped(vec!["imap.user".to_string(), "imap.pass".to_string()]),
            &GitSendEmailCheck {
                status: GitSendEmailStatus::Missing,
            },
            &Err("missing reply identity".to_string()),
            &crate::infra::b4::B4Check {
                status: B4Status::Missing,
            },
        );

        assert!(report.contains("criew doctor"));
        assert!(report.contains("kernel_trees: <none>"));
        assert!(report.contains("self_email_lookup: error (git config failed)"));
        assert!(report.contains("imap_config_status: incomplete"));
        assert!(report.contains("imap_missing_fields: imap.user, imap.pass"));
        assert!(report.contains("imap_connect_status: skipped"));
        assert!(report.contains("git_send_email_status: missing"));
        assert!(report.contains("reply_identity_status: error (missing reply identity)"));
        assert!(report.contains("b4_status: missing"));
    }

    #[test]
    fn format_doctor_report_covers_connected_imap_and_available_tools() {
        let mut runtime = test_runtime();
        runtime.kernel_trees = vec![PathBuf::from("/tmp/linux")];
        runtime.imap = ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: Some(993),
            encryption: Some(ImapEncryption::Tls),
            proxy: None,
        };
        let self_email = SelfEmailResolution {
            email: Some("me@example.com".to_string()),
            source: Some(SelfEmailSource::CriewImapConfig),
            git_error: None,
        };
        let report = format_doctor_report(
            &runtime,
            &test_bootstrap_state(),
            &self_email,
            &DoctorImapStatus::Connected(MailboxSnapshot {
                uidvalidity: 7,
                highest_uid: 88,
                highest_modseq: Some(99),
            }),
            &GitSendEmailCheck {
                status: GitSendEmailStatus::Available {
                    path: PathBuf::from("/usr/bin/git"),
                    version: "2.45".to_string(),
                },
            },
            &Ok(ReplyIdentity {
                display: "CRIEW <me@example.com>".to_string(),
                email: "me@example.com".to_string(),
                source: ReplyIdentitySource::SendEmailFrom,
            }),
            &crate::infra::b4::B4Check {
                status: B4Status::Available {
                    path: PathBuf::from("/usr/bin/b4"),
                    version: "0.14".to_string(),
                },
            },
        );

        assert!(report.contains("kernel_tree: /tmp/linux"));
        assert!(report.contains("imap_config_status: complete"));
        assert!(report.contains("imap_connection_target: imap.example.com:993 (tls)"));
        assert!(report.contains("imap_connect_status: ok"));
        assert!(report.contains("imap_uidvalidity: 7"));
        assert!(report.contains("git_send_email_status: ok"));
        assert!(report.contains("reply_identity_status: ok"));
        assert!(report.contains("b4_status: ok"));
    }

    #[test]
    fn probe_doctor_imap_skips_incomplete_config_without_network_access() {
        let status = probe_doctor_imap(&test_runtime());

        match status {
            DoctorImapStatus::Skipped(missing_fields) => {
                assert!(missing_fields.contains(&"imap.user".to_string()));
                assert!(missing_fields.contains(&"imap.pass".to_string()));
            }
            _ => panic!("expected incomplete IMAP config to skip probing"),
        }
    }

    #[test]
    fn format_doctor_report_covers_imap_error_and_broken_tools() {
        let mut runtime = test_runtime();
        runtime.imap = ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: Some(993),
            encryption: Some(ImapEncryption::Tls),
            proxy: None,
        };

        let report = format_doctor_report(
            &runtime,
            &test_bootstrap_state(),
            &SelfEmailResolution::default(),
            &DoctorImapStatus::Error("connect failed".to_string()),
            &GitSendEmailCheck {
                status: GitSendEmailStatus::Broken {
                    path: PathBuf::from("/usr/bin/git"),
                    reason: "missing send-email".to_string(),
                },
            },
            &Err("missing reply identity".to_string()),
            &crate::infra::b4::B4Check {
                status: B4Status::Broken {
                    path: PathBuf::from("/usr/bin/b4"),
                    reason: "bad runtime".to_string(),
                },
            },
        );

        assert!(report.contains("imap_connect_status: error (connect failed)"));
        assert!(report.contains("git_send_email_status: broken (missing send-email)"));
        assert!(report.contains("b4_status: broken (bad runtime)"));
    }

    #[test]
    fn format_update_summary_renders_command_and_status() {
        let summary = crate::app::update::UpdateSummary {
            command_line: "cargo install --locked criew --force".to_string(),
            status: crate::app::update::UpdateStatus::DryRun,
        };

        assert_eq!(
            format_update_summary(&summary),
            "criew update\n  command: cargo install --locked criew --force\n  status: dry-run"
        );
    }
}
