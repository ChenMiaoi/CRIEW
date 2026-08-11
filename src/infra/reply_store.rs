//! Persistence for reply-send history.
//!
//! Send attempts are stored separately from the live reply composer state so
//! CRIEW can retain an audit trail even after the TUI session exits.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::sqlite::open as open_connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyDraftStatus {
    Draft,
    Failed,
}

impl ReplyDraftStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "failed" => Self::Failed,
            _ => Self::Draft,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplyDraftRequest {
    pub thread_id: i64,
    pub mail_id: i64,
    pub from_addr: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub body: Vec<String>,
    pub preview_confirmed_at: Option<String>,
    pub status: ReplyDraftStatus,
    pub draft_path: Option<PathBuf>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReplyDraft {
    pub id: i64,
    pub thread_id: i64,
    pub mail_id: i64,
    pub from_addr: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub body: Vec<String>,
    pub preview_confirmed_at: Option<String>,
    pub status: ReplyDraftStatus,
    pub draft_path: Option<PathBuf>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ReplyOutboxEntry {
    pub id: i64,
    pub thread_id: i64,
    pub mail_id: i64,
    pub subject: String,
    pub to_addrs: String,
    pub status: ReplyDraftStatus,
    pub draft_path: Option<PathBuf>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplySendStatus {
    Sent,
    Failed,
    TimedOut,
}

impl ReplySendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }

    #[allow(dead_code)]
    fn from_db(value: &str) -> Self {
        match value {
            "sent" => Self::Sent,
            "timed_out" => Self::TimedOut,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplySendRecordRequest {
    pub thread_id: i64,
    pub mail_id: i64,
    pub transport: String,
    pub message_id: String,
    pub from_addr: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub subject: String,
    pub preview_confirmed_at: String,
    pub status: ReplySendStatus,
    pub command: Option<String>,
    pub draft_path: Option<PathBuf>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub error_summary: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub started_at: String,
    pub finished_at: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReplySendRecord {
    pub id: i64,
    pub thread_id: i64,
    pub mail_id: i64,
    pub transport: String,
    pub message_id: String,
    pub from_addr: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub subject: String,
    pub preview_confirmed_at: String,
    pub status: ReplySendStatus,
    pub command: Option<String>,
    pub draft_path: Option<PathBuf>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub error_summary: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub started_at: String,
    pub finished_at: String,
}

pub fn upsert_reply_draft(path: &Path, request: &ReplyDraftRequest) -> Result<i64> {
    let connection = open_connection(path)?;
    let references = request.references.join("\n");
    let body = request.body.join("\n");
    let draft_path = request
        .draft_path
        .as_ref()
        .map(|path| path.display().to_string());

    connection
        .execute(
            "
INSERT INTO reply_draft(
    thread_id, mail_id, from_addr, to_addrs, cc_addrs, subject, in_reply_to,
    references_text, body, preview_confirmed_at, status, draft_path, last_error, updated_at
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
    strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
ON CONFLICT(thread_id, mail_id) DO UPDATE SET
    from_addr = excluded.from_addr,
    to_addrs = excluded.to_addrs,
    cc_addrs = excluded.cc_addrs,
    subject = excluded.subject,
    in_reply_to = excluded.in_reply_to,
    references_text = excluded.references_text,
    body = excluded.body,
    preview_confirmed_at = excluded.preview_confirmed_at,
    status = excluded.status,
    draft_path = excluded.draft_path,
    last_error = excluded.last_error,
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
",
            params![
                request.thread_id,
                request.mail_id,
                request.from_addr,
                request.to_addrs,
                request.cc_addrs,
                request.subject,
                request.in_reply_to,
                references,
                body,
                request.preview_confirmed_at,
                request.status.as_str(),
                draft_path,
                request.last_error,
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to persist reply draft for mail {} thread {}",
                    request.mail_id, request.thread_id
                ),
                error,
            )
        })?;

    Ok(connection.last_insert_rowid())
}

pub fn load_reply_draft(path: &Path, thread_id: i64, mail_id: i64) -> Result<Option<ReplyDraft>> {
    let connection = open_connection(path)?;
    connection
        .query_row(
            "
SELECT
    id, thread_id, mail_id, from_addr, to_addrs, cc_addrs, subject, in_reply_to,
    references_text, body, preview_confirmed_at, status, draft_path, last_error, updated_at
FROM reply_draft
WHERE thread_id = ?1 AND mail_id = ?2
LIMIT 1
",
            params![thread_id, mail_id],
            map_reply_draft,
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to load reply draft for mail {} thread {}",
                    mail_id, thread_id
                ),
                error,
            )
        })
}

pub fn load_reply_draft_by_id(path: &Path, draft_id: i64) -> Result<Option<ReplyDraft>> {
    let connection = open_connection(path)?;
    connection
        .query_row(
            "
SELECT
    id, thread_id, mail_id, from_addr, to_addrs, cc_addrs, subject, in_reply_to,
    references_text, body, preview_confirmed_at, status, draft_path, last_error, updated_at
FROM reply_draft
WHERE id = ?1
LIMIT 1
",
            params![draft_id],
            map_reply_draft,
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to load reply draft {draft_id}"),
                error,
            )
        })
}

/// Create an invisible mail/thread pair for a standalone compose draft.
///
/// Reply drafts use the same foreign keys for replies and independent
/// messages.  Keeping a synthetic anchor lets the outbox retain one stable
/// identity without making the send-history schema nullable, while the NULL
/// mailbox keeps the anchor out of normal mailbox thread lists.
pub fn create_draft_anchor(
    path: &Path,
    message_id: &str,
    subject: &str,
    from_addr: &str,
) -> Result<(i64, i64)> {
    let mut connection = open_connection(path)?;
    let transaction = connection.transaction().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to begin compose draft anchor transaction",
            error,
        )
    })?;
    transaction
        .execute(
            "
INSERT INTO mail(message_id, subject, from_addr, imap_mailbox, is_expunged)
VALUES (?1, ?2, ?3, NULL, 0)
",
            params![message_id, subject, from_addr],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to create compose mail anchor <{message_id}>"),
                error,
            )
        })?;
    let mail_id = transaction.last_insert_rowid();
    transaction
        .execute(
            "
INSERT INTO thread(root_mail_id, subject_norm, last_activity_at, message_count)
VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 1)
",
            params![mail_id, subject.trim().to_ascii_lowercase()],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to create compose thread anchor for mail {mail_id}"),
                error,
            )
        })?;
    let thread_id = transaction.last_insert_rowid();
    transaction
        .execute(
            "
INSERT INTO thread_node(mail_id, thread_id, parent_mail_id, root_mail_id, depth, sort_ts)
VALUES (?1, ?2, NULL, ?1, 0, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
",
            params![mail_id, thread_id],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to create compose thread node for mail {mail_id}"),
                error,
            )
        })?;
    transaction.commit().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            format!("failed to commit compose draft anchor for mail {mail_id}"),
            error,
        )
    })?;
    Ok((thread_id, mail_id))
}

pub fn delete_reply_draft(path: &Path, thread_id: i64, mail_id: i64) -> Result<()> {
    let connection = open_connection(path)?;
    connection
        .execute(
            "DELETE FROM reply_draft WHERE thread_id = ?1 AND mail_id = ?2",
            params![thread_id, mail_id],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to delete reply draft for mail {} thread {}",
                    mail_id, thread_id
                ),
                error,
            )
        })?;
    Ok(())
}

pub fn list_reply_outbox(path: &Path) -> Result<Vec<ReplyOutboxEntry>> {
    let connection = open_connection(path)?;
    let mut statement = connection
        .prepare(
            "
SELECT id, thread_id, mail_id, subject, to_addrs, status, draft_path, last_error, updated_at
FROM reply_draft
WHERE status IN ('draft', 'failed')
ORDER BY updated_at DESC, id DESC
",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare reply outbox query",
                error,
            )
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok(ReplyOutboxEntry {
                id: row.get::<_, i64>(0)?,
                thread_id: row.get::<_, i64>(1)?,
                mail_id: row.get::<_, i64>(2)?,
                subject: row.get::<_, String>(3)?,
                to_addrs: row.get::<_, String>(4)?,
                status: ReplyDraftStatus::from_db(&row.get::<_, String>(5)?),
                draft_path: row.get::<_, Option<String>>(6)?.map(PathBuf::from),
                last_error: row.get::<_, Option<String>>(7)?,
                updated_at: row.get::<_, String>(8)?,
            })
        })
        .map_err(|error| {
            CriewError::with_source(ErrorCode::Database, "failed to query reply outbox", error)
        })?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to decode reply outbox entry",
                error,
            )
        })?);
    }
    Ok(entries)
}

pub fn insert_reply_send(path: &Path, request: &ReplySendRecordRequest) -> Result<i64> {
    let connection = open_connection(path)?;
    let draft_path = request
        .draft_path
        .as_ref()
        .map(|path| path.display().to_string());

    // Persist the draft path and transport outcome together so a failed send
    // still leaves behind enough context for manual recovery.
    connection
        .execute(
            "
INSERT INTO reply_send(
    thread_id, mail_id, transport, message_id, from_addr, to_addrs, cc_addrs, subject,
    preview_confirmed_at, status, command, draft_path, exit_code, timed_out, error_summary,
    stdout, stderr, started_at, finished_at
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
",
            params![
                request.thread_id,
                request.mail_id,
                request.transport,
                request.message_id,
                request.from_addr,
                request.to_addrs,
                request.cc_addrs,
                request.subject,
                request.preview_confirmed_at,
                request.status.as_str(),
                request.command,
                draft_path,
                request.exit_code,
                bool_to_i64(request.timed_out),
                request.error_summary,
                request.stdout,
                request.stderr,
                request.started_at,
                request.finished_at,
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to persist reply send record for mail {} thread {}",
                    request.mail_id, request.thread_id
                ),
                error,
            )
        })?;

    Ok(connection.last_insert_rowid())
}

#[allow(dead_code)]
pub fn latest_reply_send_for_mail(path: &Path, mail_id: i64) -> Result<Option<ReplySendRecord>> {
    let connection = open_connection(path)?;
    connection
        .query_row(
            "
SELECT
    id, thread_id, mail_id, transport, message_id, from_addr, to_addrs, cc_addrs, subject,
    preview_confirmed_at, status, command, draft_path, exit_code, timed_out, error_summary,
    stdout, stderr, started_at, finished_at
FROM reply_send
WHERE mail_id = ?1
ORDER BY id DESC
LIMIT 1
",
            // Use insertion order as recency because send attempts are append-
            // only and ids remain monotonic within one database.
            params![mail_id],
            |row| {
                Ok(ReplySendRecord {
                    id: row.get::<_, i64>(0)?,
                    thread_id: row.get::<_, i64>(1)?,
                    mail_id: row.get::<_, i64>(2)?,
                    transport: row.get::<_, String>(3)?,
                    message_id: row.get::<_, String>(4)?,
                    from_addr: row.get::<_, String>(5)?,
                    to_addrs: row.get::<_, String>(6)?,
                    cc_addrs: row.get::<_, String>(7)?,
                    subject: row.get::<_, String>(8)?,
                    preview_confirmed_at: row.get::<_, String>(9)?,
                    status: ReplySendStatus::from_db(&row.get::<_, String>(10)?),
                    command: row.get::<_, Option<String>>(11)?,
                    draft_path: row.get::<_, Option<String>>(12)?.map(PathBuf::from),
                    exit_code: row.get::<_, Option<i32>>(13)?,
                    timed_out: row.get::<_, i64>(14)? != 0,
                    error_summary: row.get::<_, Option<String>>(15)?,
                    stdout: row.get::<_, Option<String>>(16)?,
                    stderr: row.get::<_, Option<String>>(17)?,
                    started_at: row.get::<_, String>(18)?,
                    finished_at: row.get::<_, String>(19)?,
                })
            },
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to load latest reply send record for mail {mail_id}"),
                error,
            )
        })
}

fn map_reply_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReplyDraft> {
    let references = row
        .get::<_, String>(8)?
        .lines()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let body_text = row.get::<_, String>(9)?;
    let body = if body_text.is_empty() {
        vec![String::new()]
    } else {
        body_text.split('\n').map(ToOwned::to_owned).collect()
    };
    Ok(ReplyDraft {
        id: row.get::<_, i64>(0)?,
        thread_id: row.get::<_, i64>(1)?,
        mail_id: row.get::<_, i64>(2)?,
        from_addr: row.get::<_, String>(3)?,
        to_addrs: row.get::<_, String>(4)?,
        cc_addrs: row.get::<_, String>(5)?,
        subject: row.get::<_, String>(6)?,
        in_reply_to: row.get::<_, String>(7)?,
        references,
        body,
        preview_confirmed_at: row.get::<_, Option<String>>(10)?,
        status: ReplyDraftStatus::from_db(&row.get::<_, String>(11)?),
        draft_path: row.get::<_, Option<String>>(12)?.map(PathBuf::from),
        last_error: row.get::<_, Option<String>>(13)?,
        updated_at: row.get::<_, String>(14)?,
    })
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use crate::infra::db;

    use super::{
        ReplyDraftRequest, ReplyDraftStatus, ReplySendRecordRequest, ReplySendStatus,
        create_draft_anchor, delete_reply_draft, insert_reply_send, latest_reply_send_for_mail,
        list_reply_outbox, load_reply_draft, load_reply_draft_by_id, upsert_reply_draft,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-reply-store-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn persists_and_loads_latest_reply_send_record() {
        let root = temp_dir("latest");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");
        let connection = Connection::open(&db_path).expect("open db");
        connection
            .execute(
                "INSERT INTO mail(id, message_id, subject, from_addr) VALUES (11, 'patch@example.com', '[PATCH] demo', 'tester@example.com')",
                [],
            )
            .expect("insert mail");
        connection
            .execute(
                "INSERT INTO thread(id, root_mail_id, subject_norm, message_count) VALUES (7, 11, '[patch] demo', 1)",
                [],
            )
            .expect("insert thread");

        insert_reply_send(
            &db_path,
            &ReplySendRecordRequest {
                thread_id: 7,
                mail_id: 11,
                transport: "git-send-email".to_string(),
                message_id: "msg-1@example.com".to_string(),
                from_addr: "Tester <tester@example.com>".to_string(),
                to_addrs: "maintainer@example.com".to_string(),
                cc_addrs: "list@example.com".to_string(),
                subject: "Re: [PATCH] demo".to_string(),
                preview_confirmed_at: "2026-03-07T10:00:00Z".to_string(),
                status: ReplySendStatus::Sent,
                command: Some("git send-email /tmp/reply.eml".to_string()),
                draft_path: Some(PathBuf::from("/tmp/reply.eml")),
                exit_code: Some(0),
                timed_out: false,
                error_summary: None,
                stdout: Some("ok".to_string()),
                stderr: Some(String::new()),
                started_at: "2026-03-07T10:00:01Z".to_string(),
                finished_at: "2026-03-07T10:00:02Z".to_string(),
            },
        )
        .expect("persist reply send");

        let record = latest_reply_send_for_mail(&db_path, 11)
            .expect("load latest reply send")
            .expect("reply send record");
        assert_eq!(record.thread_id, 7);
        assert_eq!(record.status, ReplySendStatus::Sent);
        assert_eq!(record.message_id, "msg-1@example.com");
        assert_eq!(
            record.command.as_deref(),
            Some("git send-email /tmp/reply.eml")
        );
        assert_eq!(record.draft_path, Some(PathBuf::from("/tmp/reply.eml")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persists_updates_lists_and_deletes_reply_draft() {
        let root = temp_dir("draft");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");
        let connection = Connection::open(&db_path).expect("open db");
        connection
            .execute(
                "INSERT INTO mail(id, message_id, subject, from_addr) VALUES (11, 'patch@example.com', '[PATCH] demo', 'tester@example.com')",
                [],
            )
            .expect("insert mail");
        connection
            .execute(
                "INSERT INTO thread(id, root_mail_id, subject_norm, message_count) VALUES (7, 11, '[patch] demo', 1)",
                [],
            )
            .expect("insert thread");

        let request = ReplyDraftRequest {
            thread_id: 7,
            mail_id: 11,
            from_addr: "Tester <tester@example.com>".to_string(),
            to_addrs: "maintainer@example.com".to_string(),
            cc_addrs: "list@example.com".to_string(),
            subject: "Re: [PATCH] demo".to_string(),
            in_reply_to: "patch@example.com".to_string(),
            references: vec![
                "patch@example.com".to_string(),
                "root@example.com".to_string(),
            ],
            body: vec!["Looks good".to_string(), String::new()],
            preview_confirmed_at: None,
            status: ReplyDraftStatus::Draft,
            draft_path: None,
            last_error: None,
        };
        upsert_reply_draft(&db_path, &request).expect("persist draft");

        let loaded = load_reply_draft(&db_path, 7, 11)
            .expect("load draft")
            .expect("draft exists");
        assert_eq!(loaded.references, request.references);
        assert_eq!(loaded.body, request.body);
        assert_eq!(loaded.status, ReplyDraftStatus::Draft);

        let mut failed = request.clone();
        failed.status = ReplyDraftStatus::Failed;
        failed.last_error = Some("smtp auth failed".to_string());
        failed.draft_path = Some(PathBuf::from("/tmp/reply.eml"));
        upsert_reply_draft(&db_path, &failed).expect("update draft failure");

        let outbox = list_reply_outbox(&db_path).expect("list outbox");
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].status, ReplyDraftStatus::Failed);
        assert_eq!(outbox[0].last_error.as_deref(), Some("smtp auth failed"));
        assert_eq!(outbox[0].draft_path, Some(PathBuf::from("/tmp/reply.eml")));

        delete_reply_draft(&db_path, 7, 11).expect("delete draft");
        assert!(
            load_reply_draft(&db_path, 7, 11)
                .expect("load deleted draft")
                .is_none()
        );
        assert!(
            list_reply_outbox(&db_path)
                .expect("list empty outbox")
                .is_empty()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn creates_hidden_compose_anchor_and_loads_draft_by_id() {
        let root = temp_dir("compose-anchor");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let (thread_id, mail_id) = create_draft_anchor(
            &db_path,
            "compose-test@example.com",
            "Draft subject",
            "Tester <tester@example.com>",
        )
        .expect("create compose anchor");
        assert!(thread_id > 0);
        assert!(mail_id > 0);

        let draft_id = upsert_reply_draft(
            &db_path,
            &ReplyDraftRequest {
                thread_id,
                mail_id,
                from_addr: "Tester <tester@example.com>".to_string(),
                to_addrs: "recipient@example.com".to_string(),
                cc_addrs: String::new(),
                subject: "Draft subject".to_string(),
                in_reply_to: String::new(),
                references: Vec::new(),
                body: vec!["body".to_string()],
                preview_confirmed_at: None,
                status: ReplyDraftStatus::Draft,
                draft_path: None,
                last_error: None,
            },
        )
        .expect("persist compose draft");
        let loaded = load_reply_draft_by_id(&db_path, draft_id)
            .expect("load compose draft by id")
            .expect("compose draft exists");
        assert_eq!(loaded.thread_id, thread_id);
        assert_eq!(loaded.mail_id, mail_id);
        assert!(loaded.in_reply_to.is_empty());

        let connection = Connection::open(&db_path).expect("open db");
        let mailbox: Option<String> = connection
            .query_row(
                "SELECT imap_mailbox FROM mail WHERE id = ?1",
                [mail_id],
                |row| row.get(0),
            )
            .expect("load anchor mailbox");
        assert!(mailbox.is_none());

        let _ = fs::remove_dir_all(root);
    }
}
