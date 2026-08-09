//! Command-line surface for CRIEW.
//!
//! The CLI stays intentionally compact: clap defines the public verbs here,
//! while validation and side-effecting policy remain in the application layer
//! so tests can exercise the same behavior without going through argv parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
