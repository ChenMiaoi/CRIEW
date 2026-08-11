//! Domain models and static subscription data.
//!
//! The domain layer stays intentionally small: it defines CRIEW's core
//! concepts without coupling them to SQLite, IMAP, or ratatui details.

pub mod following;
pub mod models;
pub mod subscriptions;
