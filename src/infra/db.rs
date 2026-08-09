//! SQLite schema initialization and migrations.
//!
//! Schema setup stays isolated from higher-level storage code so the rest of
//! the program can assume a ready database and focus on data invariants rather
//! than migration bookkeeping.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::infra::error::{CriewError, ErrorCode, Result};

pub const CURRENT_SCHEMA_VERSION: i64 = 7;

const CREATE_SCHEMA_VERSION_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    description TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
"#;

struct Migration {
    version: i64,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema",
        sql: include_str!("../../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        description: "patch workflow schema",
        sql: include_str!("../../migrations/0002_patch_workflow.sql"),
    },
    Migration {
        version: 3,
        description: "reply send workflow schema",
        sql: include_str!("../../migrations/0003_reply_send_workflow.sql"),
    },
    Migration {
        version: 4,
        description: "thread ordering uses mail date",
        sql: include_str!("../../migrations/0004_thread_sort_by_mail_date.sql"),
    },
    Migration {
        version: 5,
        description: "mail query indexes",
        sql: include_str!("../../migrations/0005_mail_query_indexes.sql"),
    },
    Migration {
        version: 6,
        description: "review trailer storage",
        sql: include_str!("../../migrations/0006_review_trailers.sql"),
    },
    Migration {
        version: 7,
        description: "persistent reply drafts",
        sql: include_str!("../../migrations/0007_reply_drafts.sql"),
    },
];

#[derive(Debug, Clone)]
pub struct DatabaseState {
    pub path: PathBuf,
    pub schema_version: i64,
    pub created: bool,
    pub applied_migrations: Vec<i64>,
}

pub fn initialize(path: &Path) -> Result<DatabaseState> {
    let created = !path.exists();
    let mut connection = Connection::open(path).map_err(|error| {
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

    connection
        .execute_batch(CREATE_SCHEMA_VERSION_TABLE)
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to create schema_version table",
                error,
            )
        })?;

    let current = current_version(&connection)?;
    let mut applied = Vec::new();

    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
    {
        // Apply each migration atomically so a partial upgrade cannot leave the
        // schema_version table claiming success for SQL that never committed.
        let tx = connection.transaction().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to open migration transaction",
                error,
            )
        })?;

        tx.execute_batch(migration.sql).map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to run migration {}", migration.version),
                error,
            )
        })?;

        tx.execute(
            "INSERT INTO schema_version(version, description) VALUES (?1, ?2)",
            params![migration.version, migration.description],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to register migration {}", migration.version),
                error,
            )
        })?;

        tx.commit().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to commit migration {}", migration.version),
                error,
            )
        })?;

        applied.push(migration.version);
    }

    let schema_version = current_version(&connection)?;

    Ok(DatabaseState {
        path: path.to_path_buf(),
        schema_version,
        created,
        applied_migrations: applied,
    })
}

fn current_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            CriewError::with_source(ErrorCode::Database, "failed to query schema version", error)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use crate::infra::error::ErrorCode;

    use super::{CURRENT_SCHEMA_VERSION, initialize};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn initialize_runs_initial_migration() {
        let root = temp_dir("db-init");
        let db_path = root.join("criew.db");

        let state = initialize(&db_path).expect("initialize db");
        assert!(state.created);
        assert_eq!(state.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            state.applied_migrations,
            (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>()
        );

        let connection = Connection::open(&db_path).expect("open sqlite");
        let version = connection
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("query version");
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initialize_is_idempotent_for_existing_database() {
        let root = temp_dir("db-reinitialize");
        let db_path = root.join("criew.db");

        let first = initialize(&db_path).expect("initialize db");
        let second = initialize(&db_path).expect("reinitialize db");

        assert!(first.created);
        assert_eq!(second.path, db_path);
        assert!(!second.created);
        assert_eq!(second.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(second.applied_migrations.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initialize_reports_missing_parent_directory() {
        let root = temp_dir("db-missing-parent");
        let db_path = root.join("missing").join("criew.db");

        let error = initialize(&db_path).expect_err("missing parent directory should fail");

        assert_eq!(error.code(), ErrorCode::Database);
        assert!(error.to_string().contains("failed to open sqlite database"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initialize_reports_schema_version_table_creation_conflicts() {
        let root = temp_dir("db-schema-version-conflict");
        let db_path = root.join("criew.db");
        let connection = Connection::open(&db_path).expect("open sqlite");
        connection
            .execute("CREATE VIEW schema_version AS SELECT 1 AS version", [])
            .expect("create conflicting view");
        drop(connection);

        let error = initialize(&db_path).expect_err("schema_version conflict should fail");

        assert_eq!(error.code(), ErrorCode::Database);
        assert!(error.to_string().contains("schema") || error.to_string().contains("migration"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initialize_reports_schema_version_query_failure_for_invalid_table_shape() {
        let root = temp_dir("db-schema-version-shape");
        let db_path = root.join("criew.db");
        let connection = Connection::open(&db_path).expect("open sqlite");
        connection
            .execute(
                "CREATE TABLE schema_version (description TEXT NOT NULL)",
                [],
            )
            .expect("create malformed schema_version");
        drop(connection);

        let error = initialize(&db_path).expect_err("invalid schema_version shape should fail");

        assert_eq!(error.code(), ErrorCode::Database);
        assert!(error.to_string().contains("failed to query schema version"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initialize_reports_migration_and_registration_failures() {
        let root = temp_dir("db-migration-failures");

        let conflicting_db_path = root.join("migration-sql.db");
        let connection = Connection::open(&conflicting_db_path).expect("open conflicting sqlite");
        connection
            .execute(
                "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, description TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT '')",
                [],
            )
            .expect("create schema_version");
        connection
            .execute("CREATE VIEW mail AS SELECT 1 AS id", [])
            .expect("create conflicting mail view");
        drop(connection);

        let migration_error =
            initialize(&conflicting_db_path).expect_err("migration SQL conflict should fail");
        assert_eq!(migration_error.code(), ErrorCode::Database);
        assert!(
            migration_error
                .to_string()
                .contains("failed to run migration 1")
        );

        let missing_column_db_path = root.join("migration-register.db");
        let connection = Connection::open(&missing_column_db_path).expect("open sqlite");
        connection
            .execute(
                "CREATE TABLE schema_version (version INTEGER PRIMARY KEY)",
                [],
            )
            .expect("create truncated schema_version");
        drop(connection);

        let register_error =
            initialize(&missing_column_db_path).expect_err("migration registration should fail");
        assert_eq!(register_error.code(), ErrorCode::Database);
        assert!(
            register_error
                .to_string()
                .contains("failed to register migration 1")
        );

        let _ = fs::remove_dir_all(root);
    }
}
