//! Infrastructure adapters for side-effecting concerns.
//!
//! Filesystem access, SQLite, IMAP/lore sync, external commands, and persisted
//! UI state all live here so the rest of the program can depend on narrower
//! interfaces.

pub mod b4;
mod b4_vendor;
pub mod bootstrap;
pub mod config;
pub mod db;
pub mod error;
pub mod imap;
pub mod logging;
pub mod mail_parser;
pub mod mail_store;
pub mod patch_store;
pub mod reply_store;
pub mod sendmail;
pub(crate) mod sqlite;
pub mod ui_state;
