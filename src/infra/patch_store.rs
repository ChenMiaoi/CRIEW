//! Persistence for patch-series metadata and execution history.
//!
//! Patch analysis is recomputable from mail, but keeping normalized series and
//! run records in SQLite makes the TUI fast and preserves workflow status
//! across restarts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::domain::models::PatchSeriesStatus;
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::sqlite::open as open_connection;

#[derive(Debug, Clone)]
pub struct UpsertSeriesRequest {
    pub mailbox: String,
    pub thread_id: i64,
    pub version: u32,
    pub expected_total: u32,
    pub author: String,
    pub subject: String,
    pub anchor_message_id: String,
    pub integrity: String,
    pub missing_seq: Vec<u32>,
    pub duplicate_seq: Vec<u32>,
    pub out_of_order: bool,
    pub items: Vec<UpsertSeriesItem>,
}

#[derive(Debug, Clone)]
pub struct UpsertSeriesItem {
    pub seq: u32,
    pub total: u32,
    pub mail_id: i64,
    pub message_id: String,
    pub subject: String,
    pub raw_path: Option<PathBuf>,
    pub sort_ord: usize,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SeriesRecord {
    pub id: i64,
    pub mailbox: String,
    pub thread_id: i64,
    pub version: u32,
    pub expected_total: u32,
    pub author: String,
    pub subject: String,
    pub anchor_message_id: String,
    pub status: PatchSeriesStatus,
    pub integrity: String,
    pub missing_seq: Vec<u32>,
    pub duplicate_seq: Vec<u32>,
    pub out_of_order: bool,
}

#[derive(Debug, Clone)]
pub struct SeriesRunRequest {
    pub series_id: i64,
    pub action: String,
    pub command: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub summary: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub output_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SeriesResultUpdate {
    pub status: PatchSeriesStatus,
    pub last_error: Option<String>,
    pub last_command: Option<String>,
    pub last_exit_code: Option<i32>,
    pub last_stdout: Option<String>,
    pub last_stderr: Option<String>,
    pub output_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SeriesLatestReport {
    pub series_id: i64,
    pub status: PatchSeriesStatus,
    pub integrity: String,
    pub expected_total: u32,
    pub version: u32,
    pub subject: String,
    pub last_error: Option<String>,
    pub last_command: Option<String>,
    pub last_exit_code: Option<i32>,
    pub last_summary: Option<String>,
}

pub fn upsert_series(path: &Path, request: &UpsertSeriesRequest) -> Result<SeriesRecord> {
    let mut connection = open_connection(path)?;
    let tx = connection.transaction().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to open patch series transaction",
            error,
        )
    })?;

    tx.execute(
        "
INSERT INTO patch_series(
    mailbox, thread_id, version, expected_total, author, subject, anchor_message_id,
    status, integrity, missing_seq, duplicate_seq, out_of_order, updated_at
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'new', ?8, ?9, ?10, ?11, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
ON CONFLICT(mailbox, thread_id, version) DO UPDATE SET
    expected_total = excluded.expected_total,
    author = excluded.author,
    subject = excluded.subject,
    anchor_message_id = excluded.anchor_message_id,
    integrity = excluded.integrity,
    missing_seq = excluded.missing_seq,
    duplicate_seq = excluded.duplicate_seq,
    out_of_order = excluded.out_of_order,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
",
        params![
            request.mailbox,
            request.thread_id,
            request.version as i64,
            request.expected_total as i64,
            request.author,
            request.subject,
            request.anchor_message_id,
            request.integrity,
            join_seq(&request.missing_seq),
            join_seq(&request.duplicate_seq),
            bool_to_i64(request.out_of_order),
        ],
    )
    .map_err(|error| {
        CriewError::with_source(ErrorCode::Database, "failed to upsert patch series", error)
    })?;

    let (series_id, status_text): (i64, String) = tx
        .query_row(
            "SELECT id, status FROM patch_series WHERE mailbox = ?1 AND thread_id = ?2 AND version = ?3",
            params![request.mailbox, request.thread_id, request.version as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to load upserted patch series row",
                error,
            )
        })?;

    tx.execute(
        "DELETE FROM patch_series_item WHERE series_id = ?1",
        params![series_id],
    )
    .map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            format!("failed to clear patch items for series {series_id}"),
            error,
        )
    })?;

    // Replace the item set wholesale so the stored series shape always mirrors
    // the latest thread analysis, including reorder, duplicate, or missing
    // patch changes discovered on a later sync.
    for item in &request.items {
        tx.execute(
            "
INSERT INTO patch_series_item(
    series_id, seq, total, mail_id, message_id, subject, raw_path, sort_ord
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
",
            params![
                series_id,
                item.seq as i64,
                item.total as i64,
                item.mail_id,
                item.message_id,
                item.subject,
                item.raw_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                item.sort_ord as i64,
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to insert patch item seq={} for series {series_id}",
                    item.seq
                ),
                error,
            )
        })?;
    }

    tx.commit().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to commit patch series transaction",
            error,
        )
    })?;

    Ok(SeriesRecord {
        id: series_id,
        mailbox: request.mailbox.clone(),
        thread_id: request.thread_id,
        version: request.version,
        expected_total: request.expected_total,
        author: request.author.clone(),
        subject: request.subject.clone(),
        anchor_message_id: request.anchor_message_id.clone(),
        status: status_from_db(&status_text),
        integrity: request.integrity.clone(),
        missing_seq: request.missing_seq.clone(),
        duplicate_seq: request.duplicate_seq.clone(),
        out_of_order: request.out_of_order,
    })
}

pub fn update_series_result(
    path: &Path,
    series_id: i64,
    update: &SeriesResultUpdate,
) -> Result<()> {
    let connection = open_connection(path)?;
    let output_path = update
        .output_path
        .as_ref()
        .map(|path| path.display().to_string());

    connection
        .execute(
            "
UPDATE patch_series
SET
    status = ?2,
    last_error = ?3,
    last_command = ?4,
    last_exit_code = ?5,
    last_stdout = ?6,
    last_stderr = ?7,
    exported_path = ?8,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE id = ?1
",
            params![
                series_id,
                status_to_db(update.status),
                update.last_error,
                update.last_command,
                update.last_exit_code,
                update.last_stdout,
                update.last_stderr,
                output_path,
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to update patch series result for series {series_id}"),
                error,
            )
        })?;

    Ok(())
}

pub fn insert_series_run(path: &Path, run: &SeriesRunRequest) -> Result<()> {
    let connection = open_connection(path)?;
    // Keep an append-only run history separate from the latest status so users
    // can inspect prior attempts even after a newer retry succeeds.
    connection
        .execute(
            "
INSERT INTO patch_series_run(
    series_id, action, command, status, exit_code, timed_out, summary,
    stdout, stderr, output_path
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
",
            params![
                run.series_id,
                run.action,
                run.command,
                run.status,
                run.exit_code,
                bool_to_i64(run.timed_out),
                run.summary,
                run.stdout,
                run.stderr,
                run.output_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to insert patch series run for series {}",
                    run.series_id
                ),
                error,
            )
        })?;
    Ok(())
}

pub fn load_series_statuses(
    path: &Path,
    mailbox: &str,
    thread_ids: &[i64],
) -> Result<HashMap<i64, PatchSeriesStatus>> {
    let connection = open_connection(path)?;
    let mut statuses = HashMap::new();
    let mut seen = HashSet::new();

    for thread_id in thread_ids {
        // The caller may hand us repeated thread ids from a larger UI list; de-
        // duplicate here so status hydration stays predictable and cheap.
        if !seen.insert(*thread_id) {
            continue;
        }

        let status = connection
            .query_row(
                "
SELECT status
FROM patch_series
WHERE mailbox = ?1 AND thread_id = ?2
ORDER BY version DESC
LIMIT 1
",
                params![mailbox, thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!(
                        "failed to query patch series status for mailbox '{}' thread {}",
                        mailbox, thread_id
                    ),
                    error,
                )
            })?;

        if let Some(status) = status {
            statuses.insert(*thread_id, status_from_db(&status));
        }
    }

    Ok(statuses)
}

#[allow(dead_code)]
pub fn load_latest_report(
    path: &Path,
    mailbox: &str,
    thread_id: i64,
) -> Result<Option<SeriesLatestReport>> {
    let connection = open_connection(path)?;
    let row = connection
        .query_row(
            "
SELECT
    id,
    status,
    integrity,
    expected_total,
    version,
    subject,
    last_error,
    last_command,
    last_exit_code
FROM patch_series
WHERE mailbox = ?1 AND thread_id = ?2
ORDER BY version DESC
LIMIT 1
",
            params![mailbox, thread_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? as u32,
                    row.get::<_, i64>(4)? as u32,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<i32>>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to load patch series report for mailbox '{}' thread {}",
                    mailbox, thread_id
                ),
                error,
            )
        })?;

    let Some((
        series_id,
        status_text,
        integrity,
        expected_total,
        version,
        subject,
        last_error,
        last_command,
        last_exit_code,
    )) = row
    else {
        return Ok(None);
    };

    let last_summary = connection
        .query_row(
            "
SELECT summary
FROM patch_series_run
WHERE series_id = ?1
ORDER BY id DESC
LIMIT 1
",
            params![series_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to load latest run summary for series {series_id}"),
                error,
            )
        })?
        .flatten();

    Ok(Some(SeriesLatestReport {
        series_id,
        status: status_from_db(&status_text),
        integrity,
        expected_total,
        version,
        subject,
        last_error,
        last_command,
        last_exit_code,
        last_summary,
    }))
}

fn join_seq(values: &[u32]) -> String {
    values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn status_to_db(status: PatchSeriesStatus) -> &'static str {
    match status {
        PatchSeriesStatus::New => "new",
        PatchSeriesStatus::Reviewing => "reviewing",
        PatchSeriesStatus::Applied => "applied",
        PatchSeriesStatus::Failed => "failed",
        PatchSeriesStatus::Conflict => "conflict",
    }
}

fn status_from_db(value: &str) -> PatchSeriesStatus {
    match value.trim().to_ascii_lowercase().as_str() {
        "new" => PatchSeriesStatus::New,
        "reviewing" => PatchSeriesStatus::Reviewing,
        "applied" => PatchSeriesStatus::Applied,
        "failed" => PatchSeriesStatus::Failed,
        "conflict" => PatchSeriesStatus::Conflict,
        _ => PatchSeriesStatus::New,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::{Connection, params};

    use super::{
        SeriesRunRequest, UpsertSeriesItem, UpsertSeriesRequest, insert_series_run,
        load_latest_report, load_series_statuses, update_series_result, upsert_series,
    };
    use crate::domain::models::PatchSeriesStatus;
    use crate::infra::db;

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-patch-store-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn seed_mail_rows(path: &Path, rows: &[(i64, &str)]) {
        let connection = Connection::open(path).expect("open db");
        for (id, message_id) in rows {
            connection
                .execute(
                    "
INSERT INTO mail(id, message_id, subject, from_addr, imap_mailbox, imap_uid)
VALUES (?1, ?2, ?3, ?4, 'io-uring', ?1)
",
                    params![id, message_id, format!("subject-{id}"), "alice@example.com"],
                )
                .expect("insert mail row");
        }
    }

    #[test]
    fn upsert_series_creates_rows_and_items() {
        let root = temp_dir("upsert");
        let db_path = root.join("criew.db");
        let _ = db::initialize(&db_path).expect("initialize db");
        seed_mail_rows(
            &db_path,
            &[(100, "p1@example.com"), (101, "p2@example.com")],
        );

        let series = upsert_series(
            &db_path,
            &UpsertSeriesRequest {
                mailbox: "io-uring".to_string(),
                thread_id: 42,
                version: 2,
                expected_total: 3,
                author: "Alice".to_string(),
                subject: "demo".to_string(),
                anchor_message_id: "cover@example.com".to_string(),
                integrity: "complete".to_string(),
                missing_seq: vec![],
                duplicate_seq: vec![],
                out_of_order: false,
                items: vec![
                    UpsertSeriesItem {
                        seq: 1,
                        total: 3,
                        mail_id: 100,
                        message_id: "p1@example.com".to_string(),
                        subject: "p1".to_string(),
                        raw_path: None,
                        sort_ord: 0,
                    },
                    UpsertSeriesItem {
                        seq: 2,
                        total: 3,
                        mail_id: 101,
                        message_id: "p2@example.com".to_string(),
                        subject: "p2".to_string(),
                        raw_path: None,
                        sort_ord: 1,
                    },
                ],
            },
        )
        .expect("upsert series");

        assert_eq!(series.thread_id, 42);
        assert_eq!(series.status, PatchSeriesStatus::New);

        let statuses =
            load_series_statuses(&db_path, "io-uring", &[42]).expect("load series statuses");
        assert_eq!(statuses.get(&42), Some(&PatchSeriesStatus::New));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_and_run_are_visible_from_latest_report() {
        let root = temp_dir("report");
        let db_path = root.join("criew.db");
        let _ = db::initialize(&db_path).expect("initialize db");
        seed_mail_rows(&db_path, &[(1, "p1@example.com")]);

        let series = upsert_series(
            &db_path,
            &UpsertSeriesRequest {
                mailbox: "io-uring".to_string(),
                thread_id: 7,
                version: 1,
                expected_total: 1,
                author: "Alice".to_string(),
                subject: "demo".to_string(),
                anchor_message_id: "p1@example.com".to_string(),
                integrity: "complete".to_string(),
                missing_seq: vec![],
                duplicate_seq: vec![],
                out_of_order: false,
                items: vec![UpsertSeriesItem {
                    seq: 1,
                    total: 1,
                    mail_id: 1,
                    message_id: "p1@example.com".to_string(),
                    subject: "p1".to_string(),
                    raw_path: None,
                    sort_ord: 0,
                }],
            },
        )
        .expect("upsert series");

        update_series_result(
            &db_path,
            series.id,
            &super::SeriesResultUpdate {
                status: PatchSeriesStatus::Failed,
                last_error: Some("network error".to_string()),
                last_command: Some("b4 am p1@example.com".to_string()),
                last_exit_code: Some(1),
                last_stdout: Some(String::new()),
                last_stderr: Some("failed".to_string()),
                output_path: None,
            },
        )
        .expect("update series result");

        insert_series_run(
            &db_path,
            &SeriesRunRequest {
                series_id: series.id,
                action: "apply".to_string(),
                command: "b4 am p1@example.com".to_string(),
                status: "failed".to_string(),
                exit_code: Some(1),
                timed_out: false,
                summary: Some("failed".to_string()),
                stdout: Some(String::new()),
                stderr: Some("failed".to_string()),
                output_path: None,
            },
        )
        .expect("insert series run");

        let report = load_latest_report(&db_path, "io-uring", 7)
            .expect("load latest report")
            .expect("report exists");
        assert_eq!(report.status, PatchSeriesStatus::Failed);
        assert_eq!(report.last_exit_code, Some(1));
        assert!(
            report
                .last_command
                .as_deref()
                .is_some_and(|value| value.contains("b4 am"))
        );
        assert_eq!(report.last_summary.as_deref(), Some("failed"));

        let _ = fs::remove_dir_all(root);
    }
}
