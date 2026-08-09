//! SQLite persistence for mails, checkpoints, and materialized threads.
//!
//! The sync layer feeds raw mail batches into this module, which owns the
//! invariants around idempotent upserts, checkpoint monotonicity, and the
//! scope of thread rebuilds.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::mail_parser::{ParsedMailHeaders, ParsedTrailer, normalize_subject};

#[derive(Debug, Clone)]
pub struct MailboxState {
    pub mailbox: String,
    pub uidvalidity: u64,
    pub last_seen_uid: u32,
    pub highest_modseq: Option<u64>,
    pub synced_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IncomingMail {
    pub mailbox: String,
    pub uid: u32,
    pub modseq: Option<u64>,
    pub flags: Vec<String>,
    pub raw_path: PathBuf,
    pub parsed: ParsedMailHeaders,
}

#[derive(Debug, Clone)]
pub struct SyncBatch {
    pub mailbox: String,
    pub uidvalidity: u64,
    pub highest_uid: u32,
    pub highest_modseq: Option<u64>,
    pub mails: Vec<IncomingMail>,
}

#[derive(Debug, Clone)]
pub struct SyncWriteResult {
    pub inserted: usize,
    pub updated: usize,
    pub rebuilt_roots: usize,
    pub mailbox_rebuilt: bool,
    pub state: MailboxState,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ThreadRow {
    pub thread_id: i64,
    pub mail_id: i64,
    pub depth: u16,
    pub subject: String,
    pub from_addr: String,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub date: Option<String>,
    pub raw_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct MailGraphNode {
    id: i64,
    message_id: String,
    subject: String,
    sort_ts: String,
    in_reply_to: Option<String>,
    refs: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct ThreadAssignment {
    root_mail_id: i64,
    parent_mail_id: Option<i64>,
    depth: u16,
}

#[derive(Debug, Default)]
struct ThreadBuild {
    nodes: HashMap<i64, MailGraphNode>,
    assignments: HashMap<i64, ThreadAssignment>,
    groups: HashMap<i64, Vec<i64>>,
}

pub fn load_mailbox_state(path: &Path, mailbox: &str) -> Result<Option<MailboxState>> {
    let connection = open_connection(path)?;

    connection
        .query_row(
            "SELECT mailbox, uidvalidity, last_seen_uid, highest_modseq, synced_at FROM imap_mailbox_state WHERE mailbox = ?1",
            params![mailbox],
            |row| {
                Ok(MailboxState {
                    mailbox: row.get::<_, String>(0)?,
                    uidvalidity: row.get::<_, i64>(1)? as u64,
                    last_seen_uid: row.get::<_, i64>(2)? as u32,
                    highest_modseq: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                    synced_at: row.get::<_, Option<String>>(4)?,
                })
            },
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to load mailbox checkpoint for '{mailbox}'"),
                error,
            )
        })
}

pub fn mailbox_message_count(path: &Path, mailbox: &str) -> Result<usize> {
    let connection = open_connection(path)?;
    let count = connection
        .query_row(
            "SELECT COUNT(1) FROM mail WHERE imap_mailbox = ?1 AND is_expunged = 0",
            params![mailbox],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to count mails for mailbox '{mailbox}'"),
                error,
            )
        })?;

    Ok(count.max(0) as usize)
}

pub fn prune_mailbox_subjects<F>(path: &Path, mailbox: &str, mut keep_subject: F) -> Result<usize>
where
    F: FnMut(&str) -> bool,
{
    let mut connection = open_connection(path)?;
    let tx = connection.transaction().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to open mailbox prune transaction",
            error,
        )
    })?;

    let mut pruned_mail_ids = Vec::new();
    {
        let mut statement = tx
            .prepare(
                "SELECT id, subject, raw_path FROM mail WHERE imap_mailbox = ?1 AND is_expunged = 0",
            )
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!("failed to prepare mailbox prune query for '{mailbox}'"),
                    error,
                )
            })?;

        let rows = statement
            .query_map(params![mailbox], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!("failed to query mailbox prune candidates for '{mailbox}'"),
                    error,
                )
            })?;

        for row in rows {
            let (mail_id, subject, raw_path) = row.map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to decode mailbox prune candidate row",
                    error,
                )
            })?;
            if !keep_subject(&subject) {
                pruned_mail_ids.push((mail_id, raw_path.map(PathBuf::from)));
            }
        }
    }

    if pruned_mail_ids.is_empty() {
        tx.commit().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to commit no-op mailbox prune transaction",
                error,
            )
        })?;
        return Ok(0);
    }

    for (mail_id, _) in &pruned_mail_ids {
        tx.execute("DELETE FROM mail WHERE id = ?1", params![mail_id])
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!("failed to delete pruned mail {}", mail_id),
                    error,
                )
            })?;
    }

    let build = build_thread_index_tx(&tx)?;
    let _ = rebuild_all_threads_tx(&tx, &build)?;

    tx.commit().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            format!("failed to commit mailbox prune for '{mailbox}'"),
            error,
        )
    })?;

    // Delete raw files only after the database commit succeeds so a filesystem
    // cleanup failure cannot leave persisted rows pointing at missing files.
    for (_, raw_path) in &pruned_mail_ids {
        let Some(raw_path) = raw_path.as_ref() else {
            continue;
        };
        match fs::remove_file(raw_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    mailbox = %mailbox,
                    path = %raw_path.display(),
                    error = %error,
                    "failed to delete pruned raw mail file"
                );
            }
        }
    }

    Ok(pruned_mail_ids.len())
}

pub fn rebuild_all_threads(path: &Path) -> Result<usize> {
    let mut connection = open_connection(path)?;
    let tx = connection.transaction().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to open thread rebuild transaction",
            error,
        )
    })?;

    let build = build_thread_index_tx(&tx)?;
    let rebuilt = rebuild_all_threads_tx(&tx, &build)?;

    tx.commit().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to commit thread rebuild transaction",
            error,
        )
    })?;

    Ok(rebuilt)
}

pub fn apply_sync_batch(path: &Path, batch: SyncBatch) -> Result<SyncWriteResult> {
    let mut connection = open_connection(path)?;
    let tx = connection.transaction().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to open sync transaction",
            error,
        )
    })?;

    let previous_state = load_mailbox_state_tx(&tx, &batch.mailbox)?;
    let mailbox_rebuilt = previous_state
        .as_ref()
        .is_some_and(|state| state.uidvalidity != batch.uidvalidity);

    if mailbox_rebuilt {
        // Once UIDVALIDITY changes, every stored UID for this mailbox is stale.
        // Clearing mailbox rows is cheaper and safer than trying to salvage
        // incremental state across two unrelated UID namespaces.
        tx.execute(
            "DELETE FROM mail WHERE imap_mailbox = ?1",
            params![batch.mailbox],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to clear mailbox rows after UIDVALIDITY change",
                error,
            )
        })?;
    }

    let mut inserted = 0usize;
    let mut updated = 0usize;
    let mut touched_mail_ids = HashSet::new();
    let mut touched_message_ids = HashSet::new();

    for mail in &batch.mails {
        let (mail_id, is_inserted) = upsert_mail_tx(&tx, mail)?;
        if is_inserted {
            inserted += 1;
        } else {
            updated += 1;
        }

        touched_mail_ids.insert(mail_id);
        // Track message ids separately because descendants can refer to a mail
        // by Message-Id even when their own rows were untouched in this batch.
        touched_message_ids.insert(mail.parsed.message_id.clone());
    }

    let rebuilt_roots = if mailbox_rebuilt {
        let build = build_thread_index_tx(&tx)?;
        rebuild_all_threads_tx(&tx, &build)?
    } else if touched_mail_ids.is_empty() {
        0
    } else {
        // In steady state, only rebuild roots touched by new mail or by
        // references that can change parentage. Recomputing every thread on
        // each incremental sync would be correct but scales poorly.
        let affected_mail_ids =
            expand_affected_mail_ids_tx(&tx, &touched_mail_ids, &touched_message_ids)?;
        let stale_roots = load_stale_roots_tx(&tx, &affected_mail_ids)?;

        let build = build_thread_index_tx(&tx)?;
        let mut roots_to_rebuild = stale_roots;
        for mail_id in affected_mail_ids {
            if let Some(assignment) = build.assignments.get(&mail_id) {
                roots_to_rebuild.insert(assignment.root_mail_id);
            }
        }

        rebuild_thread_roots_tx(&tx, &build, &roots_to_rebuild)?
    };

    let state = persist_mailbox_state_tx(&tx, &batch, previous_state.as_ref())?;

    tx.commit().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to commit sync transaction",
            error,
        )
    })?;

    Ok(SyncWriteResult {
        inserted,
        updated,
        rebuilt_roots,
        mailbox_rebuilt,
        state,
    })
}

pub fn load_thread_rows_by_mailbox(
    path: &Path,
    mailbox: &str,
    limit: usize,
) -> Result<Vec<ThreadRow>> {
    let connection = open_connection(path)?;
    let mut statement = connection
        .prepare(
            "
SELECT
    t.id,
    m.id,
    tn.depth,
    m.subject,
    m.from_addr,
    m.message_id,
    m.in_reply_to,
    m.date,
    m.raw_path
FROM thread_node tn
JOIN thread t ON t.id = tn.thread_id
JOIN mail m ON m.id = tn.mail_id
WHERE m.imap_mailbox = ?1 AND m.is_expunged = 0
ORDER BY
    t.last_activity_at DESC,
    tn.root_mail_id ASC,
    t.id ASC,
    tn.depth ASC,
    tn.sort_ts ASC,
    tn.mail_id ASC
LIMIT ?2
",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare mailbox-specific thread query",
                error,
            )
        })?;

    let rows = statement
        .query_map(params![mailbox, limit as i64], map_thread_row)
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to query thread rows for mailbox '{mailbox}'"),
                error,
            )
        })?;

    collect_thread_rows(rows)
}

pub fn load_thread_trailers_by_mailbox(
    path: &Path,
    mailbox: &str,
) -> Result<HashMap<i64, Vec<ParsedTrailer>>> {
    let connection = open_connection(path)?;
    let mut statement = connection
        .prepare(
            "
SELECT tn.thread_id, mt.kind, mt.value
FROM thread_node tn
JOIN mail m ON m.id = tn.mail_id
JOIN mail_trailer mt ON mt.mail_id = m.id
WHERE m.imap_mailbox = ?1 AND m.is_expunged = 0
ORDER BY tn.thread_id ASC, mt.mail_id ASC, mt.ord ASC
",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare thread trailer query",
                error,
            )
        })?;

    let rows = statement
        .query_map(params![mailbox], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ParsedTrailer {
                    kind: row.get::<_, String>(1)?,
                    value: row.get::<_, String>(2)?,
                },
            ))
        })
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to query thread trailers for mailbox '{mailbox}'"),
                error,
            )
        })?;

    let mut trailers_by_thread = HashMap::new();
    for row in rows {
        let (thread_id, trailer) = row.map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to decode thread trailer",
                error,
            )
        })?;
        trailers_by_thread
            .entry(thread_id)
            .or_insert_with(Vec::new)
            .push(trailer);
    }

    Ok(trailers_by_thread)
}

fn map_thread_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        thread_id: row.get::<_, i64>(0)?,
        mail_id: row.get::<_, i64>(1)?,
        depth: row.get::<_, i64>(2)? as u16,
        subject: row.get::<_, String>(3)?,
        from_addr: row.get::<_, String>(4)?,
        message_id: row.get::<_, String>(5)?,
        in_reply_to: row.get::<_, Option<String>>(6)?,
        date: row.get::<_, Option<String>>(7)?,
        raw_path: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
    })
}

fn collect_thread_rows(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<ThreadRow>>,
) -> Result<Vec<ThreadRow>> {
    let mut collected = Vec::new();
    for row in rows {
        collected.push(row.map_err(|error| {
            CriewError::with_source(ErrorCode::Database, "failed to decode thread row", error)
        })?);
    }

    Ok(collected)
}

fn open_connection(path: &Path) -> Result<Connection> {
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

fn load_mailbox_state_tx(tx: &Transaction<'_>, mailbox: &str) -> Result<Option<MailboxState>> {
    tx.query_row(
        "SELECT mailbox, uidvalidity, last_seen_uid, highest_modseq, synced_at FROM imap_mailbox_state WHERE mailbox = ?1",
        params![mailbox],
        |row| {
            Ok(MailboxState {
                mailbox: row.get::<_, String>(0)?,
                uidvalidity: row.get::<_, i64>(1)? as u64,
                last_seen_uid: row.get::<_, i64>(2)? as u32,
                highest_modseq: row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                synced_at: row.get::<_, Option<String>>(4)?,
            })
        },
    )
    .optional()
    .map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            format!("failed to load mailbox checkpoint for '{mailbox}'"),
            error,
        )
    })
}

fn persist_mailbox_state_tx(
    tx: &Transaction<'_>,
    batch: &SyncBatch,
    previous_state: Option<&MailboxState>,
) -> Result<MailboxState> {
    let previous_last_seen = previous_state.map(|state| state.last_seen_uid).unwrap_or(0);
    let previous_modseq = previous_state.and_then(|state| state.highest_modseq);

    // Checkpoints only move forward. A single batch can be sparse, so preserve
    // the highest seen UID/MODSEQ rather than assuming the current fetch is a
    // complete view of mailbox progress.
    let next_last_seen_uid = previous_last_seen.max(batch.highest_uid);
    let next_highest_modseq = max_option(previous_modseq, batch.highest_modseq);

    tx.execute(
        "
INSERT INTO imap_mailbox_state(mailbox, uidvalidity, last_seen_uid, highest_modseq, synced_at)
VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
ON CONFLICT(mailbox) DO UPDATE SET
    uidvalidity = excluded.uidvalidity,
    last_seen_uid = excluded.last_seen_uid,
    highest_modseq = excluded.highest_modseq,
    synced_at = excluded.synced_at
",
        params![
            batch.mailbox,
            to_i64(batch.uidvalidity)?,
            next_last_seen_uid as i64,
            next_highest_modseq.map(|value| value as i64),
        ],
    )
    .map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to persist mailbox checkpoint",
            error,
        )
    })?;

    load_mailbox_state_tx(tx, &batch.mailbox)?.ok_or_else(|| {
        CriewError::new(
            ErrorCode::Database,
            format!(
                "missing mailbox checkpoint after update for '{}'",
                batch.mailbox
            ),
        )
    })
}

fn upsert_mail_tx(tx: &Transaction<'_>, mail: &IncomingMail) -> Result<(i64, bool)> {
    let existing_id = tx
        .query_row(
            "SELECT id FROM mail WHERE message_id = ?1",
            params![mail.parsed.message_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to lookup existing mail by message id '{}'",
                    mail.parsed.message_id
                ),
                error,
            )
        })?;

    let flags = if mail.flags.is_empty() {
        None
    } else {
        Some(mail.flags.join(" "))
    };

    let mail_id = if let Some(id) = existing_id {
        tx.execute(
            "
UPDATE mail
SET
    subject = ?1,
    from_addr = ?2,
    date = ?3,
    raw_path = ?4,
    in_reply_to = ?5,
    list_id = ?6,
    flags = ?7,
    imap_mailbox = ?8,
    imap_uid = ?9,
    modseq = ?10,
    is_expunged = 0
WHERE id = ?11
",
            params![
                mail.parsed.subject,
                mail.parsed.from_addr,
                mail.parsed.date,
                mail.raw_path.to_string_lossy().to_string(),
                mail.parsed.in_reply_to,
                mail.parsed.list_id,
                flags,
                mail.mailbox,
                mail.uid as i64,
                mail.modseq.map(|value| value as i64),
                id,
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to update mail row for message id '{}'",
                    mail.parsed.message_id
                ),
                error,
            )
        })?;

        id
    } else {
        tx.execute(
            "
INSERT INTO mail(
    message_id,
    subject,
    from_addr,
    date,
    raw_path,
    in_reply_to,
    list_id,
    flags,
    imap_mailbox,
    imap_uid,
    modseq,
    is_expunged
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)
",
            params![
                mail.parsed.message_id,
                mail.parsed.subject,
                mail.parsed.from_addr,
                mail.parsed.date,
                mail.raw_path.to_string_lossy().to_string(),
                mail.parsed.in_reply_to,
                mail.parsed.list_id,
                flags,
                mail.mailbox,
                mail.uid as i64,
                mail.modseq.map(|value| value as i64),
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to insert mail row for message id '{}'",
                    mail.parsed.message_id
                ),
                error,
            )
        })?;

        tx.last_insert_rowid()
    };

    tx.execute("DELETE FROM mail_ref WHERE mail_id = ?1", params![mail_id])
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to clear references for mail id {}", mail_id),
                error,
            )
        })?;

    for (index, reference) in mail.parsed.references.iter().enumerate() {
        tx.execute(
            "INSERT INTO mail_ref(mail_id, ref_message_id, ord) VALUES (?1, ?2, ?3)",
            params![mail_id, reference, index as i64],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to insert reference '{}' for mail id {}",
                    reference, mail_id
                ),
                error,
            )
        })?;
    }

    tx.execute(
        "DELETE FROM mail_trailer WHERE mail_id = ?1",
        params![mail_id],
    )
    .map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            format!("failed to clear trailers for mail id {mail_id}"),
            error,
        )
    })?;

    for (ord, trailer) in mail.parsed.trailers.iter().enumerate() {
        tx.execute(
            "INSERT INTO mail_trailer(mail_id, kind, value, ord) VALUES (?1, ?2, ?3, ?4)",
            params![mail_id, trailer.kind, trailer.value, ord as i64],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!(
                    "failed to insert trailer '{}' for mail id {}",
                    trailer.kind, mail_id
                ),
                error,
            )
        })?;
    }

    Ok((mail_id, existing_id.is_none()))
}

fn expand_affected_mail_ids_tx(
    tx: &Transaction<'_>,
    touched_mail_ids: &HashSet<i64>,
    touched_message_ids: &HashSet<String>,
) -> Result<HashSet<i64>> {
    let mut affected = touched_mail_ids.clone();
    let mut seen_messages = touched_message_ids.clone();
    let mut queue: VecDeque<String> = touched_message_ids.iter().cloned().collect();

    let mut statement = tx
        .prepare(
            "
SELECT DISTINCT m.id, m.message_id
FROM mail m
LEFT JOIN mail_ref r ON r.mail_id = m.id
WHERE m.in_reply_to = ?1 OR r.ref_message_id = ?1
",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare affected mail query",
                error,
            )
        })?;

    while let Some(message_id) = queue.pop_front() {
        let rows = statement
            .query_map(params![message_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to query affected mail rows",
                    error,
                )
            })?;

        for row in rows {
            let (mail_id, child_message_id) = row.map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to decode affected mail row",
                    error,
                )
            })?;
            affected.insert(mail_id);
            if seen_messages.insert(child_message_id.clone()) {
                queue.push_back(child_message_id);
            }
        }
    }

    Ok(affected)
}

fn load_stale_roots_tx(
    tx: &Transaction<'_>,
    affected_mail_ids: &HashSet<i64>,
) -> Result<HashSet<i64>> {
    let mut roots = HashSet::new();

    let mut statement = tx
        .prepare("SELECT root_mail_id FROM thread_node WHERE mail_id = ?1")
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare stale root query",
                error,
            )
        })?;

    for mail_id in affected_mail_ids {
        let root = statement
            .query_row(params![mail_id], |row| row.get::<_, Option<i64>>(0))
            .optional()
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!("failed to query stale root for mail {}", mail_id),
                    error,
                )
            })?;

        if let Some(Some(root_id)) = root {
            roots.insert(root_id);
        }
    }

    Ok(roots)
}

fn normalize_mail_sort_ts(date_header: Option<&str>, created_at: &str) -> String {
    parse_mail_date_header(date_header).unwrap_or_else(|| created_at.to_string())
}

fn parse_mail_date_header(date_header: Option<&str>) -> Option<String> {
    let value = date_header?.trim();
    if value.is_empty() {
        return None;
    }

    DateTime::parse_from_rfc2822(value)
        .or_else(|_| DateTime::parse_from_rfc3339(value))
        .ok()
        .map(|datetime| {
            datetime
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
}

fn build_thread_index_tx(tx: &Transaction<'_>) -> Result<ThreadBuild> {
    let mut build = ThreadBuild::default();

    let mut mail_statement = tx
        .prepare(
            "
SELECT id, message_id, subject, in_reply_to, date, created_at
FROM mail
WHERE is_expunged = 0
",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare mail graph query",
                error,
            )
        })?;

    let mail_rows = mail_statement
        .query_map([], |row| {
            let date = row.get::<_, Option<String>>(4)?;
            let created_at = row.get::<_, String>(5)?;
            Ok(MailGraphNode {
                id: row.get::<_, i64>(0)?,
                message_id: row.get::<_, String>(1)?,
                subject: row.get::<_, String>(2)?,
                in_reply_to: row.get::<_, Option<String>>(3)?,
                sort_ts: normalize_mail_sort_ts(date.as_deref(), &created_at),
                refs: Vec::new(),
            })
        })
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to query mail graph rows",
                error,
            )
        })?;

    for row in mail_rows {
        let node = row.map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to decode mail graph row",
                error,
            )
        })?;
        build.nodes.insert(node.id, node);
    }

    if build.nodes.is_empty() {
        return Ok(build);
    }

    let mut refs_map: HashMap<i64, Vec<String>> = HashMap::new();
    // Load references separately so the first pass can build the mail node set
    // without repeating row data for every reference edge.
    let mut ref_statement = tx
        .prepare("SELECT mail_id, ref_message_id FROM mail_ref ORDER BY mail_id, ord ASC")
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare mail_ref graph query",
                error,
            )
        })?;

    let ref_rows = ref_statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| {
            CriewError::with_source(ErrorCode::Database, "failed to query mail_ref rows", error)
        })?;

    for row in ref_rows {
        let (mail_id, ref_id) = row.map_err(|error| {
            CriewError::with_source(ErrorCode::Database, "failed to decode mail_ref row", error)
        })?;
        refs_map.entry(mail_id).or_default().push(ref_id);
    }

    for (mail_id, refs) in refs_map {
        if let Some(node) = build.nodes.get_mut(&mail_id) {
            node.refs = refs;
        }
    }

    let message_to_id: HashMap<String, i64> = build
        .nodes
        .values()
        .map(|node| (node.message_id.clone(), node.id))
        .collect();

    let mut parent_map: HashMap<i64, Option<i64>> = HashMap::new();
    for node in build.nodes.values() {
        // Prefer the most recent resolvable `References` entry because it keeps
        // threading stable for mails that include the full ancestry chain.
        let parent = node
            .refs
            .iter()
            .rev()
            .find_map(|reference| {
                message_to_id
                    .get(reference)
                    .copied()
                    .filter(|candidate| *candidate != node.id)
            })
            .or_else(|| {
                node.in_reply_to.as_ref().and_then(|reply_to| {
                    message_to_id
                        .get(reply_to)
                        .copied()
                        .filter(|candidate| *candidate != node.id)
                })
            });

        parent_map.insert(node.id, parent);
    }

    let mut memo = HashMap::new();
    for mail_id in build.nodes.keys().copied() {
        let mut stack = HashSet::new();
        let (root_mail_id, depth) =
            resolve_thread_assignment(mail_id, &parent_map, &mut memo, &mut stack);
        let parent_mail_id = parent_map
            .get(&mail_id)
            .copied()
            .flatten()
            .filter(|candidate| {
                memo.get(candidate)
                    .is_some_and(|assignment| assignment.0 == root_mail_id)
            });

        build.assignments.insert(
            mail_id,
            ThreadAssignment {
                root_mail_id,
                parent_mail_id,
                depth,
            },
        );
        build.groups.entry(root_mail_id).or_default().push(mail_id);
    }

    for mail_ids in build.groups.values_mut() {
        // Keep a stable order inside each thread so incremental rebuilds do not
        // reshuffle visible rows when the ancestry itself has not changed.
        mail_ids.sort_by(|left, right| {
            let left_assignment =
                build
                    .assignments
                    .get(left)
                    .copied()
                    .unwrap_or(ThreadAssignment {
                        root_mail_id: *left,
                        parent_mail_id: None,
                        depth: 0,
                    });
            let right_assignment =
                build
                    .assignments
                    .get(right)
                    .copied()
                    .unwrap_or(ThreadAssignment {
                        root_mail_id: *right,
                        parent_mail_id: None,
                        depth: 0,
                    });

            let left_node = build.nodes.get(left);
            let right_node = build.nodes.get(right);
            left_assignment
                .depth
                .cmp(&right_assignment.depth)
                .then_with(|| {
                    left_node
                        .map(|node| node.sort_ts.as_str())
                        .cmp(&right_node.map(|node| node.sort_ts.as_str()))
                })
                .then_with(|| left.cmp(right))
        });
    }

    Ok(build)
}

fn resolve_thread_assignment(
    mail_id: i64,
    parent_map: &HashMap<i64, Option<i64>>,
    memo: &mut HashMap<i64, (i64, u16)>,
    stack: &mut HashSet<i64>,
) -> (i64, u16) {
    if let Some(cached) = memo.get(&mail_id) {
        return *cached;
    }

    if !stack.insert(mail_id) {
        // Broken or cyclic reference chains should degrade to a self-rooted
        // node rather than recursing forever or cross-linking unrelated mail.
        memo.insert(mail_id, (mail_id, 0));
        return (mail_id, 0);
    }

    let resolved = if let Some(parent_mail_id) = parent_map.get(&mail_id).copied().flatten() {
        if stack.contains(&parent_mail_id) {
            (mail_id, 0)
        } else {
            let (root_mail_id, parent_depth) =
                resolve_thread_assignment(parent_mail_id, parent_map, memo, stack);
            (root_mail_id, parent_depth.saturating_add(1))
        }
    } else {
        (mail_id, 0)
    };

    stack.remove(&mail_id);
    memo.insert(mail_id, resolved);
    resolved
}

fn rebuild_all_threads_tx(tx: &Transaction<'_>, build: &ThreadBuild) -> Result<usize> {
    tx.execute("DELETE FROM thread_node", []).map_err(|error| {
        CriewError::with_source(ErrorCode::Database, "failed to clear thread_node", error)
    })?;
    tx.execute("DELETE FROM thread", []).map_err(|error| {
        CriewError::with_source(ErrorCode::Database, "failed to clear thread", error)
    })?;

    let roots: HashSet<i64> = build.groups.keys().copied().collect();
    rebuild_thread_roots_tx(tx, build, &roots)
}

fn rebuild_thread_roots_tx(
    tx: &Transaction<'_>,
    build: &ThreadBuild,
    roots: &HashSet<i64>,
) -> Result<usize> {
    let mut rebuilt = 0usize;
    let mut ordered_roots: Vec<i64> = roots.iter().copied().collect();
    ordered_roots.sort_unstable();

    for root_mail_id in ordered_roots {
        tx.execute(
            "DELETE FROM thread WHERE root_mail_id = ?1",
            params![root_mail_id],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to delete stale thread for root {}", root_mail_id),
                error,
            )
        })?;

        let Some(group_mail_ids) = build.groups.get(&root_mail_id) else {
            continue;
        };

        if group_mail_ids.is_empty() {
            continue;
        }

        let root_subject = build
            .nodes
            .get(&root_mail_id)
            .map(|node| node.subject.clone())
            .unwrap_or_default();
        let subject_norm = normalize_subject(&root_subject);

        // Thread activity is derived from the newest member mail so list
        // ordering reflects conversation freshness, not only root age.
        let last_activity_at = group_mail_ids
            .iter()
            .filter_map(|mail_id| build.nodes.get(mail_id).map(|node| node.sort_ts.clone()))
            .max();

        tx.execute(
            "
INSERT INTO thread(root_mail_id, subject_norm, last_activity_at, message_count)
VALUES (?1, ?2, ?3, ?4)
",
            params![
                root_mail_id,
                subject_norm,
                last_activity_at,
                group_mail_ids.len() as i64,
            ],
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                format!("failed to insert thread for root {}", root_mail_id),
                error,
            )
        })?;
        let thread_id = tx.last_insert_rowid();

        for mail_id in group_mail_ids {
            let assignment = build.assignments.get(mail_id).copied().ok_or_else(|| {
                CriewError::new(
                    ErrorCode::Database,
                    format!("missing thread assignment for mail {}", mail_id),
                )
            })?;

            let sort_ts = build
                .nodes
                .get(mail_id)
                .map(|node| node.sort_ts.clone())
                .unwrap_or_default();

            tx.execute(
                "
INSERT INTO thread_node(mail_id, thread_id, parent_mail_id, root_mail_id, depth, sort_ts)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
",
                params![
                    mail_id,
                    thread_id,
                    assignment.parent_mail_id,
                    assignment.root_mail_id,
                    assignment.depth as i64,
                    sort_ts,
                ],
            )
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!("failed to insert thread node for mail {}", mail_id),
                    error,
                )
            })?;
        }

        rebuilt += 1;
    }

    Ok(rebuilt)
}

fn max_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        CriewError::new(
            ErrorCode::Database,
            format!("u64 value {} overflows i64 sqlite field", value),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use crate::infra::db;
    use crate::infra::mail_parser;

    use super::{
        IncomingMail, SyncBatch, apply_sync_batch, load_mailbox_state, load_thread_rows_by_mailbox,
        load_thread_trailers_by_mailbox, mailbox_message_count, prune_mailbox_subjects,
        rebuild_all_threads,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-mail-store-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn incoming(mailbox: &str, uid: u32, raw: &str) -> IncomingMail {
        let fallback_id = format!("synthetic-{mailbox}-{uid}@local");
        IncomingMail {
            mailbox: mailbox.to_string(),
            uid,
            modseq: Some(uid as u64),
            flags: vec!["Seen".to_string()],
            raw_path: PathBuf::from(format!("/tmp/{mailbox}-{uid}.eml")),
            parsed: mail_parser::parse_headers(raw.as_bytes(), fallback_id),
        }
    }

    fn incoming_at_path(mailbox: &str, uid: u32, raw_path: PathBuf, raw: &str) -> IncomingMail {
        let fallback_id = format!("synthetic-{mailbox}-{uid}@local");
        IncomingMail {
            mailbox: mailbox.to_string(),
            uid,
            modseq: Some(uid as u64),
            flags: vec!["Seen".to_string()],
            raw_path,
            parsed: mail_parser::parse_headers(raw.as_bytes(), fallback_id),
        }
    }

    fn empty_batch(mailbox: &str) -> SyncBatch {
        SyncBatch {
            mailbox: mailbox.to_string(),
            uidvalidity: 1,
            highest_uid: 0,
            highest_modseq: None,
            mails: Vec::new(),
        }
    }

    #[test]
    fn mailbox_state_starts_empty_and_message_count_skips_expunged_rows() {
        let root = temp_dir("mailbox-state");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let state = load_mailbox_state(&db_path, "inbox").expect("load initial state");
        assert!(state.is_none());

        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <keep@example.com>\nSubject: keep\nFrom: keep@example.com\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <drop@example.com>\nSubject: drop\nFrom: drop@example.com\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, batch).expect("seed mailbox");

        let connection = Connection::open(&db_path).expect("open db");
        connection
            .execute(
                "UPDATE mail SET is_expunged = 1 WHERE message_id = 'drop@example.com'",
                [],
            )
            .expect("mark expunged");
        drop(connection);

        let count = mailbox_message_count(&db_path, "inbox").expect("count active mail");
        assert_eq!(count, 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_sync_batch_preserves_checkpoint_and_skips_thread_rebuild() {
        let root = temp_dir("empty-batch");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let first_batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 5,
            highest_modseq: Some(10),
            mails: vec![incoming(
                "inbox",
                5,
                "Message-ID: <seed@example.com>\nSubject: seed\nFrom: seed@example.com\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, first_batch).expect("first sync");

        let second_batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 3,
            highest_modseq: Some(8),
            mails: Vec::new(),
        };
        let result = apply_sync_batch(&db_path, second_batch).expect("empty sync");

        assert_eq!(result.inserted, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.rebuilt_roots, 0);
        assert!(!result.mailbox_rebuilt);
        assert_eq!(result.state.last_seen_uid, 5);
        assert_eq!(result.state.highest_modseq, Some(10));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn review_trailers_are_persisted_grouped_by_thread_and_replaced_on_update() {
        let root = temp_dir("review-trailers");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 2,
                highest_modseq: Some(2),
                mails: vec![
                    incoming(
                        "inbox",
                        1,
                        "Message-ID: <root@example.com>\nSubject: [PATCH] demo\n\nbody\n",
                    ),
                    incoming(
                        "inbox",
                        2,
                        "Message-ID: <review@example.com>\nIn-Reply-To: <root@example.com>\nSubject: Re: [PATCH] demo\n\nReviewed-by: Reviewer <reviewer@example.com>\nAcked-by: Ack <ack@example.com>\n",
                    ),
                ],
            },
        )
        .expect("seed review trailers");

        let trailers =
            load_thread_trailers_by_mailbox(&db_path, "inbox").expect("load review trailers");
        let values: Vec<String> = trailers
            .values()
            .flatten()
            .map(|trailer| format!("{}={}", trailer.kind, trailer.value))
            .collect();
        assert_eq!(values.len(), 2);
        assert!(values.iter().any(|value| value.starts_with("Reviewed-by=")));
        assert!(values.iter().any(|value| value.starts_with("Acked-by=")));

        apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 2,
                highest_modseq: Some(3),
                mails: vec![incoming(
                    "inbox",
                    2,
                    "Message-ID: <review@example.com>\nIn-Reply-To: <root@example.com>\nSubject: Re: [PATCH] demo\n\nNo trailer now\n",
                )],
            },
        )
        .expect("update review mail");

        assert!(
            load_thread_trailers_by_mailbox(&db_path, "inbox")
                .expect("reload review trailers")
                .is_empty()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn storage_functions_report_clear_errors_for_uninitialized_database() {
        let root = temp_dir("uninitialized");
        let db_path = root.join("criew.db");
        Connection::open(&db_path).expect("create raw sqlite database");

        let load_state_error = load_mailbox_state(&db_path, "inbox")
            .expect_err("raw database should reject state lookup");
        assert!(
            load_state_error
                .to_string()
                .contains("failed to load mailbox checkpoint")
        );

        let count_error = mailbox_message_count(&db_path, "inbox")
            .expect_err("raw database should reject message counts");
        assert!(count_error.to_string().contains("failed to count mails"));

        let prune_error = prune_mailbox_subjects(&db_path, "inbox", |_| true)
            .expect_err("raw database should reject prune queries");
        assert!(
            prune_error
                .to_string()
                .contains("failed to prepare mailbox prune query")
        );

        let sync_error = apply_sync_batch(&db_path, empty_batch("inbox"))
            .expect_err("raw database should reject sync writes");
        assert!(
            sync_error
                .to_string()
                .contains("failed to load mailbox checkpoint")
        );

        let rows_error = load_thread_rows_by_mailbox(&db_path, "inbox", 10)
            .expect_err("raw database should reject thread row loads");
        assert!(
            rows_error
                .to_string()
                .contains("mailbox-specific thread query")
        );

        let rebuild_error =
            rebuild_all_threads(&db_path).expect_err("raw database should reject thread rebuilds");
        assert!(
            rebuild_error
                .to_string()
                .contains("failed to prepare mail graph query")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_handles_missing_modseq_and_rejects_uidvalidity_overflow() {
        let root = temp_dir("checkpoint-modseq");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let mut first_mail = incoming(
            "inbox",
            1,
            "Message-ID: <root@example.com>\nSubject: root\nFrom: root@example.com\n\nbody\n",
        );
        first_mail.modseq = None;
        let first = apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 1,
                highest_modseq: None,
                mails: vec![first_mail],
            },
        )
        .expect("persist checkpoint without modseq");
        assert_eq!(first.state.highest_modseq, None);

        let second = apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 1,
                highest_modseq: Some(7),
                mails: Vec::new(),
            },
        )
        .expect("adopt first modseq checkpoint");
        assert_eq!(second.state.highest_modseq, Some(7));

        let third = apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 1,
                highest_modseq: None,
                mails: Vec::new(),
            },
        )
        .expect("preserve previous modseq checkpoint");
        assert_eq!(third.state.highest_modseq, Some(7));

        let overflow = apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: u64::MAX,
                highest_uid: 1,
                highest_modseq: Some(8),
                mails: Vec::new(),
            },
        )
        .expect_err("overflowing uidvalidity should fail");
        assert!(overflow.to_string().contains("overflows i64 sqlite field"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apply_sync_batch_reports_missing_checkpoint_after_trigger_removes_it() {
        let root = temp_dir("checkpoint-trigger");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let connection = Connection::open(&db_path).expect("open db");
        connection
            .execute(
                "
CREATE TRIGGER delete_mailbox_checkpoint
AFTER INSERT ON imap_mailbox_state
BEGIN
    DELETE FROM imap_mailbox_state WHERE mailbox = NEW.mailbox;
END
",
                [],
            )
            .expect("create checkpoint trigger");
        drop(connection);

        let error = apply_sync_batch(
            &db_path,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 1,
                highest_modseq: Some(1),
                mails: vec![incoming(
                    "inbox",
                    1,
                    "Message-ID: <root@example.com>\nSubject: root\nFrom: root@example.com\n\nbody\n",
                )],
            },
        )
        .expect_err("missing checkpoint row should fail");
        assert!(
            error
                .to_string()
                .contains("missing mailbox checkpoint after update")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repeated_sync_is_idempotent() {
        let root = temp_dir("idempotent");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let first_batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <root@example.com>\nSubject: [PATCH 0/2] root\nFrom: Alice <a@example.com>\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <reply@example.com>\nSubject: Re: [PATCH 0/2] root\nFrom: Bob <b@example.com>\nIn-Reply-To: <root@example.com>\nReferences: <root@example.com>\n\nbody\n",
                ),
            ],
        };

        let first_result = apply_sync_batch(&db_path, first_batch.clone()).expect("first sync");
        assert_eq!(first_result.inserted, 2);
        assert_eq!(first_result.updated, 0);

        let second_result = apply_sync_batch(&db_path, first_batch).expect("second sync");
        assert_eq!(second_result.inserted, 0);
        assert_eq!(second_result.updated, 2);

        let connection = Connection::open(&db_path).expect("open db");
        let mail_count = connection
            .query_row("SELECT COUNT(*) FROM mail", [], |row| row.get::<_, i64>(0))
            .expect("count mail");
        assert_eq!(mail_count, 2);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uidvalidity_change_rebuilds_mailbox() {
        let root = temp_dir("uidvalidity");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch_a = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 1,
            highest_modseq: Some(1),
            mails: vec![incoming(
                "inbox",
                1,
                "Message-ID: <old@example.com>\nSubject: old\nFrom: old@example.com\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, batch_a).expect("initial sync");

        let batch_b = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 99,
            highest_uid: 1,
            highest_modseq: Some(1),
            mails: vec![incoming(
                "inbox",
                1,
                "Message-ID: <new@example.com>\nSubject: new\nFrom: new@example.com\n\nbody\n",
            )],
        };
        let result = apply_sync_batch(&db_path, batch_b).expect("rebuild sync");
        assert!(result.mailbox_rebuilt);

        let connection = Connection::open(&db_path).expect("open db");
        let mail_count = connection
            .query_row("SELECT COUNT(*) FROM mail", [], |row| row.get::<_, i64>(0))
            .expect("count mail");
        assert_eq!(mail_count, 1);

        let only_message_id = connection
            .query_row("SELECT message_id FROM mail", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("query message id");
        assert_eq!(only_message_id, "new@example.com");

        let checkpoint = load_mailbox_state(&db_path, "inbox")
            .expect("load checkpoint")
            .expect("checkpoint exists");
        assert_eq!(checkpoint.uidvalidity, 99);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkpoint_advances_between_batches() {
        let root = temp_dir("checkpoint");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch_one = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <a@example.com>\nSubject: a\nFrom: a@example.com\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <b@example.com>\nSubject: b\nFrom: b@example.com\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, batch_one).expect("sync one");

        let batch_two = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 3,
            highest_modseq: Some(3),
            mails: vec![incoming(
                "inbox",
                3,
                "Message-ID: <c@example.com>\nSubject: c\nFrom: c@example.com\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, batch_two).expect("sync two");

        let checkpoint = load_mailbox_state(&db_path, "inbox")
            .expect("load checkpoint")
            .expect("checkpoint exists");
        assert_eq!(checkpoint.last_seen_uid, 3);
        assert_eq!(checkpoint.highest_modseq, Some(3));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_mailbox_subjects_removes_non_matching_rows() {
        let root = temp_dir("prune-mailbox");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch = SyncBatch {
            mailbox: "INBOX".to_string(),
            uidvalidity: 1,
            highest_uid: 3,
            highest_modseq: Some(3),
            mails: vec![
                incoming(
                    "INBOX",
                    1,
                    "Message-ID: <patch@example.com>\nSubject: [PATCH 1/1] demo\nFrom: a@example.com\n\nbody\n",
                ),
                incoming(
                    "INBOX",
                    2,
                    "Message-ID: <reply@example.com>\nSubject: Re: [PATCH 1/1] demo\nFrom: b@example.com\nIn-Reply-To: <patch@example.com>\nReferences: <patch@example.com>\n\nbody\n",
                ),
                incoming(
                    "INBOX",
                    3,
                    "Message-ID: <status@example.com>\nSubject: Weekly status update\nFrom: c@example.com\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, batch).expect("seed mailbox");

        let pruned =
            prune_mailbox_subjects(&db_path, "INBOX", |subject| subject.contains("[PATCH"))
                .expect("prune mailbox");
        assert_eq!(pruned, 1);

        let rows = load_thread_rows_by_mailbox(&db_path, "INBOX", 20).expect("load pruned rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.subject.contains("[PATCH")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_mailbox_subjects_noop_keeps_rows_unchanged() {
        let root = temp_dir("prune-noop");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 1,
            highest_modseq: Some(1),
            mails: vec![incoming(
                "inbox",
                1,
                "Message-ID: <keep@example.com>\nSubject: [PATCH] keep\nFrom: keep@example.com\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, batch).expect("seed mailbox");

        let pruned = prune_mailbox_subjects(&db_path, "inbox", |_| true).expect("noop prune");
        assert_eq!(pruned, 0);
        assert_eq!(
            mailbox_message_count(&db_path, "inbox").expect("count mails"),
            1
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_mailbox_subjects_tolerates_missing_raw_files() {
        let root = temp_dir("prune-missing-raw");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let raw_path = root.join("status.eml");
        fs::write(&raw_path, "raw mail").expect("write raw mail");
        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 1,
            highest_modseq: Some(1),
            mails: vec![incoming_at_path(
                "inbox",
                1,
                raw_path.clone(),
                "Message-ID: <status@example.com>\nSubject: status\nFrom: status@example.com\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, batch).expect("seed mailbox");
        fs::remove_file(&raw_path).expect("remove raw mail before prune");

        let pruned =
            prune_mailbox_subjects(&db_path, "inbox", |_| false).expect("prune missing raw file");
        assert_eq!(pruned, 1);
        assert_eq!(
            mailbox_message_count(&db_path, "inbox").expect("count remaining mail"),
            0
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prune_mailbox_subjects_keeps_database_progress_when_raw_cleanup_fails() {
        let root = temp_dir("prune-raw-error");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let raw_dir = root.join("raw-dir");
        fs::create_dir_all(&raw_dir).expect("create raw directory");
        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 1,
            highest_modseq: Some(1),
            mails: vec![incoming_at_path(
                "inbox",
                1,
                raw_dir.clone(),
                "Message-ID: <status@example.com>\nSubject: status\nFrom: status@example.com\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, batch).expect("seed mailbox");

        let pruned = prune_mailbox_subjects(&db_path, "inbox", |_| false)
            .expect("cleanup failures should not roll back prune");
        assert_eq!(pruned, 1);
        assert_eq!(
            mailbox_message_count(&db_path, "inbox").expect("count remaining mail"),
            0
        );
        assert!(
            raw_dir.exists(),
            "raw cleanup failure should leave directory intact"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn threading_prefers_references_then_in_reply_to() {
        let root = temp_dir("threading");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 3,
            highest_modseq: Some(3),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <root@example.com>\nSubject: root\nFrom: alice@example.com\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <child@example.com>\nSubject: re root\nFrom: bob@example.com\nIn-Reply-To: <root@example.com>\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    3,
                    "Message-ID: <grand@example.com>\nSubject: re re root\nFrom: carol@example.com\nReferences: <root@example.com> <child@example.com>\n\nbody\n",
                ),
            ],
        };

        apply_sync_batch(&db_path, batch).expect("sync batch");

        let connection = Connection::open(&db_path).expect("open db");
        let parent_child = connection
            .query_row(
                "
SELECT p.message_id
FROM thread_node n
JOIN mail m ON m.id = n.mail_id
LEFT JOIN mail p ON p.id = n.parent_mail_id
WHERE m.message_id = 'child@example.com'
",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("parent for child");
        assert_eq!(parent_child.as_deref(), Some("root@example.com"));

        let parent_grand = connection
            .query_row(
                "
SELECT p.message_id
FROM thread_node n
JOIN mail m ON m.id = n.mail_id
LEFT JOIN mail p ON p.id = n.parent_mail_id
WHERE m.message_id = 'grand@example.com'
",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("parent for grand");
        assert_eq!(parent_grand.as_deref(), Some("child@example.com"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mailbox_thread_rows_do_not_interleave_threads_when_activity_ties() {
        let root = temp_dir("thread-row-order");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 4,
            highest_modseq: Some(4),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <root-a@example.com>\nSubject: thread a\nFrom: alice@example.com\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <reply-a@example.com>\nSubject: Re: thread a\nFrom: bob@example.com\nIn-Reply-To: <root-a@example.com>\nReferences: <root-a@example.com>\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    3,
                    "Message-ID: <root-b@example.com>\nSubject: thread b\nFrom: carol@example.com\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    4,
                    "Message-ID: <reply-b@example.com>\nSubject: Re: thread b\nFrom: dave@example.com\nIn-Reply-To: <root-b@example.com>\nReferences: <root-b@example.com>\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, batch).expect("sync batch");

        let connection = Connection::open(&db_path).expect("open db");
        connection
            .execute(
                "UPDATE thread SET last_activity_at = '2026-01-01T00:00:00.000Z'",
                [],
            )
            .expect("normalize thread activity");
        drop(connection);

        let rows = load_thread_rows_by_mailbox(&db_path, "inbox", 20).expect("load thread rows");
        assert_eq!(rows.len(), 4);

        let mut seen = HashSet::new();
        let mut completed = HashSet::new();
        let mut previous_thread_id: Option<i64> = None;
        for row in rows {
            if let Some(previous) = previous_thread_id
                && row.thread_id != previous
            {
                completed.insert(previous);
            }

            assert!(
                !completed.contains(&row.thread_id),
                "thread {} became non-contiguous in ordered rows",
                row.thread_id
            );

            seen.insert(row.thread_id);
            previous_thread_id = Some(row.thread_id);
        }
        assert_eq!(seen.len(), 2);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mailbox_thread_rows_order_threads_by_mail_date_not_insert_time() {
        let root = temp_dir("thread-date-order");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let newer_thread = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 1,
            highest_modseq: Some(1),
            mails: vec![incoming(
                "inbox",
                1,
                "Message-ID: <thread-a@example.com>\nSubject: thread a\nFrom: alice@example.com\nDate: Sun, 08 Mar 2026 12:00:00 +0000\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, newer_thread).expect("sync newer thread");

        std::thread::sleep(std::time::Duration::from_millis(5));

        let older_thread = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![incoming(
                "inbox",
                2,
                "Message-ID: <thread-b@example.com>\nSubject: thread b\nFrom: bob@example.com\nDate: Sun, 08 Mar 2026 11:00:00 +0000\n\nbody\n",
            )],
        };
        apply_sync_batch(&db_path, older_thread).expect("sync older thread");

        let rows = load_thread_rows_by_mailbox(&db_path, "inbox", 20).expect("load thread rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message_id, "thread-a@example.com");
        assert_eq!(rows[1].message_id, "thread-b@example.com");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rebuild_all_threads_restores_materialized_rows_after_manual_deletion() {
        let root = temp_dir("rebuild-all");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <root@example.com>\nSubject: root\nFrom: root@example.com\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <reply@example.com>\nSubject: Re: root\nFrom: reply@example.com\nIn-Reply-To: <root@example.com>\nReferences: <root@example.com>\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, batch).expect("seed mailbox");

        let connection = Connection::open(&db_path).expect("open db");
        connection
            .execute("DELETE FROM thread_node", [])
            .expect("clear thread nodes");
        connection
            .execute("DELETE FROM thread", [])
            .expect("clear threads");
        drop(connection);

        let rebuilt = rebuild_all_threads(&db_path).expect("rebuild threads");
        assert_eq!(rebuilt, 1);

        let rows = load_thread_rows_by_mailbox(&db_path, "inbox", 20).expect("load rebuilt rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message_id, "root@example.com");
        assert_eq!(rows[1].message_id, "reply@example.com");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cyclic_references_degrade_to_a_stable_thread_shape() {
        let root = temp_dir("cycle");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <a@example.com>\nSubject: loop a\nFrom: a@example.com\nIn-Reply-To: <b@example.com>\nReferences: <b@example.com>\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <b@example.com>\nSubject: loop b\nFrom: b@example.com\nIn-Reply-To: <a@example.com>\nReferences: <a@example.com>\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, batch).expect("sync cyclic thread");

        let rows = load_thread_rows_by_mailbox(&db_path, "inbox", 20).expect("load thread rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[0].thread_id, rows[1].thread_id);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rebuild_all_threads_is_noop_for_an_empty_store() {
        let root = temp_dir("rebuild-empty");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let rebuilt = rebuild_all_threads(&db_path).expect("rebuild empty store");
        assert_eq!(rebuilt, 0);
        assert!(
            load_thread_rows_by_mailbox(&db_path, "inbox", 10)
                .expect("load empty thread rows")
                .is_empty()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rebuild_all_threads_reports_corrupted_mail_ref_and_thread_node_tables() {
        let root = temp_dir("rebuild-corruption");

        let missing_ref_db = root.join("missing-ref.db");
        db::initialize(&missing_ref_db).expect("initialize missing-ref db");
        apply_sync_batch(
            &missing_ref_db,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 1,
                highest_modseq: Some(1),
                mails: vec![incoming(
                    "inbox",
                    1,
                    "Message-ID: <root@example.com>\nSubject: root\nFrom: root@example.com\n\nbody\n",
                )],
            },
        )
        .expect("seed missing-ref db");
        let connection = Connection::open(&missing_ref_db).expect("open missing-ref db");
        connection
            .execute("DROP TABLE mail_ref", [])
            .expect("drop mail_ref");
        drop(connection);

        let ref_error = rebuild_all_threads(&missing_ref_db)
            .expect_err("missing mail_ref table should fail rebuild");
        assert!(
            ref_error
                .to_string()
                .contains("failed to prepare mail_ref graph query")
        );

        let missing_thread_node_db = root.join("missing-thread-node.db");
        db::initialize(&missing_thread_node_db).expect("initialize missing-thread-node db");
        apply_sync_batch(
            &missing_thread_node_db,
            SyncBatch {
                mailbox: "inbox".to_string(),
                uidvalidity: 1,
                highest_uid: 1,
                highest_modseq: Some(1),
                mails: vec![incoming(
                    "inbox",
                    1,
                    "Message-ID: <root2@example.com>\nSubject: root2\nFrom: root2@example.com\n\nbody\n",
                )],
            },
        )
        .expect("seed missing-thread-node db");
        let connection =
            Connection::open(&missing_thread_node_db).expect("open missing-thread-node db");
        connection
            .execute("DROP TABLE thread_node", [])
            .expect("drop thread_node");
        drop(connection);

        let thread_node_error = rebuild_all_threads(&missing_thread_node_db)
            .expect_err("missing thread_node table should fail rebuild");
        assert!(
            thread_node_error
                .to_string()
                .contains("failed to clear thread_node")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn late_arriving_parent_rethreads_existing_descendants() {
        let root = temp_dir("late-parent");
        let db_path = root.join("criew.db");
        db::initialize(&db_path).expect("initialize db");

        let orphan_batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 2,
            highest_modseq: Some(2),
            mails: vec![
                incoming(
                    "inbox",
                    1,
                    "Message-ID: <child@example.com>\nSubject: Re: root\nFrom: child@example.com\nIn-Reply-To: <root@example.com>\nReferences: <root@example.com>\n\nbody\n",
                ),
                incoming(
                    "inbox",
                    2,
                    "Message-ID: <grand@example.com>\nSubject: Re: root\nFrom: grand@example.com\nIn-Reply-To: <child@example.com>\nReferences: <root@example.com> <child@example.com>\n\nbody\n",
                ),
            ],
        };
        apply_sync_batch(&db_path, orphan_batch).expect("sync orphaned descendants");

        let orphan_rows =
            load_thread_rows_by_mailbox(&db_path, "inbox", 20).expect("load orphaned rows");
        assert_eq!(
            orphan_rows
                .iter()
                .map(|row| (row.message_id.as_str(), row.depth))
                .collect::<Vec<_>>(),
            vec![("child@example.com", 0), ("grand@example.com", 1)]
        );

        let parent_batch = SyncBatch {
            mailbox: "inbox".to_string(),
            uidvalidity: 1,
            highest_uid: 3,
            highest_modseq: Some(3),
            mails: vec![incoming(
                "inbox",
                3,
                "Message-ID: <root@example.com>\nSubject: root\nFrom: root@example.com\n\nbody\n",
            )],
        };
        let result = apply_sync_batch(&db_path, parent_batch).expect("sync late parent");
        assert_eq!(result.inserted, 1);
        assert_eq!(result.updated, 0);
        assert_eq!(result.rebuilt_roots, 1);
        assert!(!result.mailbox_rebuilt);

        let rethreaded_rows =
            load_thread_rows_by_mailbox(&db_path, "inbox", 20).expect("load rethreaded rows");
        assert_eq!(
            rethreaded_rows
                .iter()
                .map(|row| (row.message_id.as_str(), row.depth))
                .collect::<Vec<_>>(),
            vec![
                ("root@example.com", 0),
                ("child@example.com", 1),
                ("grand@example.com", 2),
            ]
        );
        assert!(
            rethreaded_rows
                .windows(2)
                .all(|rows| rows[0].thread_id == rows[1].thread_id)
        );

        let _ = fs::remove_dir_all(root);
    }
}
