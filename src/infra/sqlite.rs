//! Shared SQLite connection setup.
//!
//! Every persistence adapter uses the same connection policy. Keeping it here
//! prevents a new store from silently forgetting foreign-key enforcement or
//! drifting away from the database error contract.

use std::path::Path;

use rusqlite::Connection;

use crate::infra::error::{CriewError, ErrorCode, Result};

pub(crate) fn open(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            format!("failed to open sqlite database {}", path.display()),
            error,
        )
    })?;

    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to enable sqlite foreign key support",
                error,
            )
        })?;

    Ok(connection)
}
