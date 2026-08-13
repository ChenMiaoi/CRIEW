//! Runtime bootstrap for filesystem and database prerequisites.
//!
//! Startup code goes through this module before touching higher-level features
//! so later layers can assume the basic runtime directories and schema already
//! exist.

use std::fs;

use crate::infra::config::RuntimeConfig;
use crate::infra::db::{self, DatabaseState};
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::mail_store;

const THREAD_DATE_ORDER_MIGRATION_VERSION: i64 = 4;

#[derive(Debug, Clone)]
pub struct BootstrapState {
    pub db: DatabaseState,
}

pub fn prepare(config: &RuntimeConfig) -> Result<BootstrapState> {
    // Make filesystem state explicit up front so later failures point at the
    // actual feature operation, not at some missing directory deep in the call
    // stack.
    ensure_runtime_dirs(config)?;
    let db_state = db::initialize(&config.database_path)?;
    if db_state
        .applied_migrations
        .contains(&THREAD_DATE_ORDER_MIGRATION_VERSION)
    {
        let rebuilt = mail_store::rebuild_all_threads(&config.database_path)?;
        tracing::info!(
            database = %config.database_path.display(),
            rebuilt_threads = rebuilt,
            "rebuilt thread ordering after schema upgrade"
        );
    }

    Ok(BootstrapState { db: db_state })
}

fn ensure_runtime_dirs(config: &RuntimeConfig) -> Result<()> {
    if let Some(config_dir) = config.config_path.parent() {
        ensure_directory(config_dir, "config directory")?;
    }

    ensure_directory(&config.data_dir, "data directory")?;
    ensure_directory(&config.raw_mail_dir, "raw mail directory")?;
    ensure_directory(&config.patch_dir, "patch directory")?;
    ensure_directory(&config.log_dir, "log directory")?;

    if let Some(db_dir) = config.database_path.parent() {
        ensure_directory(db_dir, "database directory")?;
    }

    Ok(())
}

fn ensure_directory(path: &std::path::Path, description: &str) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!("failed to create {description} {}", path.display()),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::infra::config::{DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS, ImapConfig, UiKeymap};
    use crate::infra::db::CURRENT_SCHEMA_VERSION;
    use crate::infra::error::ErrorCode;

    use super::{RuntimeConfig, prepare};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-bootstrap-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn test_runtime_in(root: PathBuf) -> RuntimeConfig {
        RuntimeConfig {
            config_path: root.join("config").join("criew-config.toml"),
            data_dir: root.join("data"),
            database_path: root.join("data").join("db").join("criew.db"),
            raw_mail_dir: root.join("data").join("raw"),
            patch_dir: root.join("data").join("patches"),
            log_dir: root.join("logs"),
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
            allow_preview_resize: false,
            kernel_trees: Vec::new(),
        }
    }

    #[test]
    fn prepare_creates_runtime_state_and_is_idempotent() {
        let root = temp_dir("prepare");
        let runtime = test_runtime_in(root.clone());

        let first = prepare(&runtime).expect("prepare runtime");
        let second = prepare(&runtime).expect("prepare existing runtime");

        assert!(runtime.config_path.parent().expect("config dir").is_dir());
        assert!(runtime.data_dir.is_dir());
        assert!(runtime.raw_mail_dir.is_dir());
        assert!(runtime.patch_dir.is_dir());
        assert!(runtime.log_dir.is_dir());
        assert!(runtime.database_path.is_file());

        assert_eq!(first.db.path, runtime.database_path);
        assert!(first.db.created);
        assert_eq!(first.db.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            first.db.applied_migrations,
            (1..=CURRENT_SCHEMA_VERSION).collect::<Vec<_>>()
        );

        assert_eq!(second.db.path, runtime.database_path);
        assert!(!second.db.created);
        assert_eq!(second.db.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(second.db.applied_migrations.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_reports_runtime_directory_conflicts() {
        let root = temp_dir("prepare-conflict");
        let blocked_path = root.join("blocked");
        fs::write(&blocked_path, "not a directory").expect("write blocking file");

        let mut runtime = test_runtime_in(root.clone());
        runtime.data_dir = blocked_path.join("data");

        let error = prepare(&runtime).expect_err("conflicting data directory should fail");

        assert_eq!(error.code(), ErrorCode::Io);
        assert!(
            error
                .to_string()
                .contains("failed to create data directory")
        );

        let _ = fs::remove_dir_all(root);
    }
}
