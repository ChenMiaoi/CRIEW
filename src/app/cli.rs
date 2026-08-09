//! Command-line surface for CRIEW.
//!
//! The CLI stays intentionally compact: clap defines the public verbs here,
//! while validation and side-effecting policy remain in the application layer
//! so tests can exercise the same behavior without going through argv parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "criew",
    about = "Terminal-first Linux kernel patch mail workflow TUI",
    version
)]
pub struct Cli {
    /// Override config file path.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewInboxMode {
    /// Patch series without a Reviewed-by trailer.
    NeedsReview,
    /// Patch series that have received at least one Reviewed-by trailer.
    Reviewed,
    /// Show every detected patch series with its review trailers.
    All,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Start CRIEW TUI.
    Tui,
    /// Execute mailbox sync worker.
    Sync {
        /// Mailbox name to sync (defaults to `source.mailbox` or `linux-kernel`).
        /// Use `INBOX` to trigger real IMAP sync when the IMAP config is complete.
        #[arg(long)]
        mailbox: Option<String>,
        /// Local fixture directory for offline/local test (.eml files).
        #[arg(long, value_name = "DIR")]
        fixture_dir: Option<PathBuf>,
        /// Override UIDVALIDITY when using --fixture-dir.
        #[arg(long, value_name = "N")]
        uidvalidity: Option<u64>,
        /// Maximum reconnect attempts for the sync loop (default 3).
        #[arg(long, value_name = "N")]
        reconnect_attempts: Option<u8>,
    },
    /// Fetch and persist the complete thread containing a Message-ID.
    FetchThread {
        /// Mailbox or lore list to search.
        #[arg(long)]
        mailbox: Option<String>,
        /// Message-ID identifying a mail in the target thread.
        #[arg(value_name = "MESSAGE_ID")]
        message_id: String,
    },
    /// List patch series grouped by review trailer status.
    ReviewInbox {
        /// Mailbox or lore list to inspect.
        #[arg(long)]
        mailbox: Option<String>,
        /// Review inbox view (default: needs-review).
        #[arg(long, value_enum, default_value = "needs-review")]
        mode: ReviewInboxMode,
    },
    /// Run local patch integrity, style, and maintainer checks for a series.
    Preflight {
        /// Mailbox or lore list to inspect.
        #[arg(long)]
        mailbox: Option<String>,
        /// Message-ID identifying any patch mail in the target series.
        #[arg(value_name = "MESSAGE_ID")]
        message_id: String,
    },
    /// Run environment diagnostics.
    Doctor,
    /// Update CRIEW from crates.io using cargo install.
    Update {
        /// Print the cargo install command without running it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print CRIEW version.
    Version,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, ReviewInboxMode};
    use clap::Parser;

    #[test]
    fn review_inbox_parses_mode_and_mailbox() {
        let cli = Cli::try_parse_from([
            "criew",
            "review-inbox",
            "--mailbox",
            "io-uring",
            "--mode",
            "reviewed",
        ])
        .expect("parse review inbox command");

        match cli.command {
            Some(Command::ReviewInbox { mailbox, mode }) => {
                assert_eq!(mailbox.as_deref(), Some("io-uring"));
                assert_eq!(mode, ReviewInboxMode::Reviewed);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn review_inbox_defaults_to_needs_review() {
        let cli = Cli::try_parse_from(["criew", "review-inbox"])
            .expect("parse default review inbox command");

        assert!(matches!(
            cli.command,
            Some(Command::ReviewInbox {
                mode: ReviewInboxMode::NeedsReview,
                ..
            })
        ));
    }

    #[test]
    fn preflight_parses_mailbox_and_message_id() {
        let cli = Cli::try_parse_from([
            "criew",
            "preflight",
            "--mailbox",
            "io-uring",
            "<patch@example.com>",
        ])
        .expect("parse preflight command");

        match cli.command {
            Some(Command::Preflight {
                mailbox,
                message_id,
            }) => {
                assert_eq!(mailbox.as_deref(), Some("io-uring"));
                assert_eq!(message_id, "<patch@example.com>");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
