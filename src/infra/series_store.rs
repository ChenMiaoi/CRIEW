//! Canonical patch-series catalog.
//!
//! Mail threading and patch-series identity are related, but they are not the
//! same graph.  This module materializes a series catalog from all active
//! source memberships and keeps revisions/members/follow-ups independent from
//! the current TUI page or from a transient SQLite thread row id.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::sqlite::open as open_connection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesMember {
    pub seq: u32,
    pub total: u32,
    pub mail_id: i64,
    pub message_id: String,
    pub subject: String,
    pub raw_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesCatalog {
    pub change_key: String,
    pub revision: u32,
    pub thread_id: Option<i64>,
    pub subject: String,
    pub author: String,
    pub expected_total: u32,
    pub members: Vec<SeriesMember>,
    pub missing: Vec<u32>,
    pub followup_count: usize,
    pub completeness: String,
    pub status: String,
}

#[derive(Debug, Clone)]
struct CatalogMail {
    thread_id: i64,
    thread_key: Option<String>,
    mail_id: i64,
    change_id: Option<String>,
    subject: String,
    from_addr: String,
    parsed: ParsedPatchSubject,
    is_reply: bool,
}

#[derive(Debug, Clone)]
struct ParsedPatchSubject {
    version: u32,
    seq: u32,
    total: u32,
    title: String,
}

pub fn refresh(path: &Path, source_key: &str) -> Result<usize> {
    let rows = load_source_mails(path, source_key)?;
    let groups = build_groups(rows);
    let mut connection = open_connection(path)?;
    let tx = connection.transaction().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to open series refresh transaction",
            error,
        )
    })?;

    let mut persisted = 0usize;
    for group in groups.values() {
        let change_id = upsert_change(&tx, source_key, group)?;
        for revision in group.revisions.values() {
            let revision_id = upsert_revision(&tx, change_id, group, revision)?;
            tx.execute(
                "DELETE FROM patch_member WHERE revision_id = ?1",
                params![revision_id],
            )
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    format!("failed to replace members for revision {revision_id}"),
                    error,
                )
            })?;
            for member in revision.members.values() {
                tx.execute(
                    "INSERT INTO patch_member(revision_id, seq, total, mail_id, subject, is_cover)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        revision_id,
                        member.parsed.seq,
                        member.parsed.total,
                        member.mail_id,
                        member.subject,
                        i64::from(member.parsed.seq == 0),
                    ],
                )
                .map_err(|error| {
                    CriewError::with_source(
                        ErrorCode::Database,
                        format!("failed to insert patch member {}", member.mail_id),
                        error,
                    )
                })?;
            }

            tx.execute(
                "DELETE FROM patch_followup WHERE revision_id = ?1",
                params![revision_id],
            )
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to replace patch follow-ups",
                    error,
                )
            })?;
            for followup in revision.followups.values() {
                tx.execute(
                    "INSERT INTO patch_followup(mail_id, revision_id, member_seq, relation, confidence)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        followup.mail_id,
                        revision_id,
                        if followup.parsed.seq == 0 { None } else { Some(followup.parsed.seq) },
                        "discussion",
                        "heuristic",
                    ],
                )
                .map_err(|error| {
                    CriewError::with_source(ErrorCode::Database, "failed to insert patch follow-up", error)
                })?;
            }
            persisted += 1;
        }
    }

    // A sync can expunge a whole series. Remove catalog rows that are no
    // longer discoverable from this source, while leaving revision review
    // status intact for still-present series.
    let mut existing = Vec::new();
    {
        let mut statement = tx
            .prepare("SELECT change_key FROM patch_change WHERE source_key = ?1")
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to prepare stale series query",
                    error,
                )
            })?;
        let rows = statement
            .query_map(params![source_key], |row| row.get::<_, String>(0))
            .map_err(|error| {
                CriewError::with_source(ErrorCode::Database, "failed to query stale series", error)
            })?;
        for row in rows {
            existing.push(row.map_err(|error| {
                CriewError::with_source(ErrorCode::Database, "failed to decode stale series", error)
            })?);
        }
    }
    let present_keys: HashSet<String> = groups
        .values()
        .map(|group| group.change_key.clone())
        .collect();
    for change_key in existing {
        if !present_keys.contains(&change_key) {
            tx.execute(
                "DELETE FROM patch_change WHERE source_key = ?1 AND change_key = ?2",
                params![source_key, change_key],
            )
            .map_err(|error| {
                CriewError::with_source(ErrorCode::Database, "failed to remove stale series", error)
            })?;
        }
    }

    tx.commit().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Database,
            "failed to commit series refresh",
            error,
        )
    })?;
    Ok(persisted)
}

pub fn load_catalog(path: &Path, source_key: &str) -> Result<Vec<SeriesCatalog>> {
    let connection = open_connection(path)?;
    let mut statement = connection
        .prepare(
            "SELECT pc.change_key, pr.revision, pr.thread_key, pc.subject, pc.author,
                    pr.expected_total, pr.completeness, pr.status, pr.id
             FROM patch_change pc
             JOIN patch_revision pr ON pr.change_id = pc.id
             WHERE pc.source_key = ?1
             ORDER BY pr.updated_at DESC, pc.author, pc.subject",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare series catalog query",
                error,
            )
        })?;
    let rows = statement
        .query_map(params![source_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u32,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)? as u32,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })
        .map_err(|error| {
            CriewError::with_source(ErrorCode::Database, "failed to query series catalog", error)
        })?;

    let catalog_rows: Vec<_> = rows
        .map(|row| {
            row.map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to decode series catalog",
                    error,
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    drop(statement);

    let mut result = Vec::new();
    for (
        change_key,
        revision,
        thread_key,
        subject,
        author,
        expected_total,
        completeness,
        status,
        revision_id,
    ) in catalog_rows
    {
        let mut member_statement = connection
            .prepare(
                "SELECT pm.seq, pm.total, pm.mail_id, m.message_id, pm.subject,
                        (SELECT ms.raw_path FROM mail_source ms
                         WHERE ms.mail_id = m.id AND ms.is_expunged = 0
                         ORDER BY ms.last_seen_at DESC LIMIT 1)
                 FROM patch_member pm JOIN mail m ON m.id = pm.mail_id
                 WHERE pm.revision_id = ?1 ORDER BY pm.seq, pm.mail_id",
            )
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to prepare series member query",
                    error,
                )
            })?;
        let member_rows = member_statement
            .query_map(params![revision_id], |row| {
                Ok(SeriesMember {
                    seq: row.get::<_, i64>(0)? as u32,
                    total: row.get::<_, i64>(1)? as u32,
                    mail_id: row.get(2)?,
                    message_id: row.get(3)?,
                    subject: row.get(4)?,
                    raw_path: row.get(5)?,
                })
            })
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to query series members",
                    error,
                )
            })?;
        let members: Vec<SeriesMember> = member_rows
            .map(|row| {
                row.map_err(|error| {
                    CriewError::with_source(
                        ErrorCode::Database,
                        "failed to decode series member",
                        error,
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let present: std::collections::HashSet<u32> = members
            .iter()
            .filter(|member| member.seq > 0)
            .map(|member| member.seq)
            .collect();
        let missing = (1..=expected_total)
            .filter(|seq| !present.contains(seq))
            .collect();
        let followup_count = connection
            .query_row(
                "SELECT COUNT(1) FROM patch_followup WHERE revision_id = ?1",
                params![revision_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Database,
                    "failed to count patch follow-ups",
                    error,
                )
            })? as usize;
        let thread_id = thread_key
            .as_deref()
            .map(|key| {
                connection
                    .query_row(
                        "SELECT id FROM thread WHERE stable_key = ?1",
                        params![key],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(|error| {
                        CriewError::with_source(
                            ErrorCode::Database,
                            "failed to resolve series thread",
                            error,
                        )
                    })
            })
            .transpose()?
            .flatten();
        result.push(SeriesCatalog {
            change_key,
            revision,
            thread_id,
            subject,
            author,
            expected_total,
            members,
            missing,
            followup_count,
            completeness,
            status,
        });
    }
    Ok(result)
}

fn load_source_mails(path: &Path, source_key: &str) -> Result<Vec<CatalogMail>> {
    let connection = open_connection(path)?;
    let mut statement = connection
        .prepare(
            "SELECT tn.thread_id, t.stable_key, m.id, m.message_id, m.change_id,
                    m.subject, m.from_addr,
                    (SELECT ms.raw_path FROM mail_source ms
                     WHERE ms.mail_id = m.id AND ms.is_expunged = 0
                     ORDER BY ms.last_seen_at DESC LIMIT 1)
             FROM thread_node tn
             JOIN thread t ON t.id = tn.thread_id
             JOIN mail m ON m.id = tn.mail_id
             JOIN mail_source source ON source.mail_id = m.id AND source.source_key = ?1
             WHERE source.is_expunged = 0 AND m.is_expunged = 0
             ORDER BY tn.thread_id, tn.depth, tn.sort_ts, m.id",
        )
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to prepare series source query",
                error,
            )
        })?;
    let rows = statement
        .query_map(params![source_key], |row| {
            let subject: String = row.get(5)?;
            let (parsed, is_reply) = parse_subject(&subject).unwrap_or((
                ParsedPatchSubject {
                    version: 1,
                    seq: 0,
                    total: 0,
                    title: String::new(),
                },
                true,
            ));
            Ok(CatalogMail {
                thread_id: row.get(0)?,
                thread_key: row.get(1)?,
                mail_id: row.get(2)?,
                change_id: row.get(4)?,
                subject,
                from_addr: row.get(6)?,
                parsed,
                is_reply,
            })
        })
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to query series source rows",
                error,
            )
        })?;
    rows.map(|row| {
        row.map_err(|error| {
            CriewError::with_source(
                ErrorCode::Database,
                "failed to decode series source row",
                error,
            )
        })
    })
    .collect()
}

#[derive(Debug, Default)]
struct Group {
    change_key: String,
    change_id: Option<String>,
    subject: String,
    author: String,
    thread_key: Option<String>,
    revisions: BTreeMap<u32, RevisionGroup>,
}

#[derive(Debug, Default)]
struct RevisionGroup {
    version: u32,
    expected_total: u32,
    members: BTreeMap<u32, CatalogMail>,
    followups: BTreeMap<i64, CatalogMail>,
}

fn build_groups(rows: Vec<CatalogMail>) -> HashMap<String, Group> {
    let mut thread_titles: HashMap<i64, String> = HashMap::new();
    let mut cover_titles: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows {
        if !row.is_reply && row.parsed.seq == 0 && row.parsed.total > 0 {
            let title = normalize_display_title(&row.parsed.title);
            thread_titles
                .entry(row.thread_id)
                .or_insert_with(|| title.clone());
            cover_titles
                .entry(normalize_author(&row.from_addr))
                .or_default()
                .push(title);
        }
    }
    let mut change_id_counts: HashMap<String, usize> = HashMap::new();
    for row in &rows {
        if let Some(change_id) = row.change_id.as_deref() {
            *change_id_counts
                .entry(change_id.to_ascii_lowercase())
                .or_default() += 1;
        }
    }

    let mut groups: HashMap<String, Group> = HashMap::new();
    for row in rows {
        let Some((parsed, _)) = parse_subject(&row.subject) else {
            continue;
        };
        let display_title = thread_titles
            .get(&row.thread_id)
            .cloned()
            .unwrap_or_else(|| normalize_display_title(&parsed.title));
        let author = normalize_author(&row.from_addr);
        let series_title = choose_series_title(
            &display_title,
            cover_titles.get(&author).map(Vec::as_slice).unwrap_or(&[]),
        );
        let title = normalize_title(&series_title);
        // Change-ID is useful when a sender reuses one across the entire
        // series, but many producers emit a different one per patch. Use it
        // only when it is observed more than once; otherwise the stable
        // author/title family keeps all members and rerolls together.
        let key = row
            .change_id
            .as_deref()
            .filter(|change_id| {
                change_id_counts
                    .get(&change_id.to_ascii_lowercase())
                    .copied()
                    .unwrap_or_default()
                    > 1
            })
            .map(|change_id| format!("change:{}", change_id.to_ascii_lowercase()))
            .unwrap_or_else(|| format!("family:{}|{}", author, title));
        let group = groups.entry(key.clone()).or_insert_with(|| Group {
            change_key: format!("change:{:016x}", fnv1a(&key)),
            change_id: row.change_id.clone(),
            subject: series_title.clone(),
            author: row.from_addr.clone(),
            thread_key: row.thread_key.clone(),
            revisions: BTreeMap::new(),
        });
        if group.change_id.as_deref() != row.change_id.as_deref() {
            group.change_id = None;
        }
        if group.thread_key.is_none() {
            group.thread_key = row.thread_key.clone();
        }
        let revision = group
            .revisions
            .entry(parsed.version)
            .or_insert_with(|| RevisionGroup {
                version: parsed.version,
                ..RevisionGroup::default()
            });
        revision.expected_total = revision.expected_total.max(parsed.total);
        if row.is_reply {
            revision.followups.insert(row.mail_id, row);
        } else {
            revision.members.entry(parsed.seq).or_insert(row);
        }
    }
    groups
}

fn upsert_change(tx: &rusqlite::Transaction<'_>, source_key: &str, group: &Group) -> Result<i64> {
    tx.execute(
        "INSERT INTO patch_change(change_key, change_id, subject, author, source_key, resolver, confidence, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'heuristic', 'medium', strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(change_key) DO UPDATE SET change_id=excluded.change_id, subject=excluded.subject, author=excluded.author,
         source_key=excluded.source_key, updated_at=excluded.updated_at",
        params![group.change_key, group.change_id, group.subject, group.author, source_key],
    ).map_err(|error| CriewError::with_source(ErrorCode::Database, "failed to upsert patch change", error))?;
    tx.query_row(
        "SELECT id FROM patch_change WHERE change_key = ?1",
        params![group.change_key],
        |row| row.get::<_, i64>(0),
    )
    .map_err(|error| {
        CriewError::with_source(ErrorCode::Database, "failed to load patch change", error)
    })
}

fn upsert_revision(
    tx: &rusqlite::Transaction<'_>,
    change_id: i64,
    group: &Group,
    revision: &RevisionGroup,
) -> Result<i64> {
    let completeness = if revision.expected_total == 0 {
        "ambiguous"
    } else if (1..=revision.expected_total).all(|seq| revision.members.contains_key(&seq)) {
        "complete"
    } else {
        "partial"
    };
    let cover_mail_id = revision.members.get(&0).map(|mail| mail.mail_id);
    let thread_key = group.thread_key.clone();
    tx.execute(
        "INSERT INTO patch_revision(change_id, revision, expected_total, cover_mail_id, completeness, fingerprint, thread_key, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
         ON CONFLICT(change_id, revision) DO UPDATE SET expected_total=excluded.expected_total,
         cover_mail_id=excluded.cover_mail_id, completeness=excluded.completeness,
         fingerprint=excluded.fingerprint, thread_key=COALESCE(excluded.thread_key, patch_revision.thread_key),
         updated_at=excluded.updated_at",
        params![change_id, revision.version, revision.expected_total, cover_mail_id, completeness, format!("{}:{}", group.change_key, revision.version), thread_key],
    ).map_err(|error| CriewError::with_source(ErrorCode::Database, "failed to upsert patch revision", error))?;
    tx.query_row(
        "SELECT id FROM patch_revision WHERE change_id = ?1 AND revision = ?2",
        params![change_id, revision.version],
        |row| row.get::<_, i64>(0),
    )
    .map_err(|error| {
        CriewError::with_source(ErrorCode::Database, "failed to load patch revision", error)
    })
}

fn parse_subject(subject: &str) -> Option<(ParsedPatchSubject, bool)> {
    let mut value = subject.trim();
    let mut is_reply = false;
    loop {
        let lower = value.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("re:") {
            value = value[value.len() - rest.len()..].trim_start();
            is_reply = true;
            continue;
        }
        if let Some(rest) = lower.strip_prefix("fwd:") {
            value = value[value.len() - rest.len()..].trim_start();
            is_reply = true;
            continue;
        }
        break;
    }
    let end = value.find(']')?;
    let tag = value.get(1..end)?;
    if !tag.to_ascii_lowercase().contains("patch") {
        return None;
    }
    let mut version = 1;
    let mut seq = 1;
    let mut total = 1;
    for token in tag.split_whitespace() {
        let token = token.trim_matches(|c: char| c == ',' || c == ';');
        if let Some(rest) = token.strip_prefix('v')
            && let Ok(value) = rest.parse()
        {
            version = value;
        }
        if let Some((left, right)) = token.split_once('/')
            && let (Ok(left), Ok(right)) = (left.parse(), right.parse())
        {
            seq = left;
            total = right;
        }
    }
    if total == 0 || (seq > total && seq != 0) {
        return None;
    }
    Some((
        ParsedPatchSubject {
            version,
            seq,
            total,
            title: value[end + 1..].trim().to_string(),
        },
        is_reply,
    ))
}

fn normalize_title(value: &str) -> String {
    value
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn choose_series_title(display_title: &str, covers: &[String]) -> String {
    if covers.is_empty() {
        return display_title.to_string();
    }
    let mut unique_covers: Vec<&String> = Vec::new();
    for cover in covers {
        if !unique_covers.contains(&cover) {
            unique_covers.push(cover);
        }
    }
    let display_words: Vec<String> = display_title
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| !character.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    let best = unique_covers.iter().find(|cover| {
        let cover_words: Vec<String> = cover
            .split_whitespace()
            .map(|word| {
                word.trim_matches(|character: char| !character.is_alphanumeric())
                    .to_ascii_lowercase()
            })
            .filter(|word| !word.is_empty())
            .collect();
        !cover_words.is_empty()
            && cover_words
                .iter()
                .take(2)
                .enumerate()
                .all(|(index, word)| display_words.get(index) == Some(word))
    });
    best.cloned()
        .cloned()
        .or_else(|| (unique_covers.len() == 1).then(|| unique_covers[0].clone()))
        .unwrap_or_else(|| display_title.to_string())
}

fn normalize_display_title(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_author(value: &str) -> String {
    value.to_ascii_lowercase().replace(['<', '>', ' '], "")
}

fn fnv1a(value: &str) -> u64 {
    value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{load_catalog, parse_subject, refresh};
    use crate::infra::db;
    use crate::infra::mail_parser;
    use crate::infra::mail_store::{IncomingMail, SyncBatch, apply_sync_batch};

    #[test]
    fn patch_parser_keeps_replies_out_of_members() {
        assert!(!parse_subject("[PATCH v2 1/3] demo").unwrap().1);
        assert!(parse_subject("Re: [PATCH v2 1/3] demo").unwrap().1);
    }

    #[test]
    fn refresh_materializes_members_revisions_and_followups() {
        let path = std::env::temp_dir().join(format!(
            "criew-series-store-{}-{}.db",
            std::process::id(),
            unique_suffix()
        ));
        let _ = fs::remove_file(&path);
        db::initialize(&path).expect("initialize database");

        let messages = [
            (
                1,
                "<cover-v1@example.com>",
                "[PATCH v1 0/2] Demo series",
                None,
            ),
            (
                2,
                "<patch-v1-1@example.com>",
                "[PATCH v1 1/2] add first",
                None,
            ),
            (
                3,
                "<patch-v1-2@example.com>",
                "[PATCH v1 2/2] add second",
                None,
            ),
            (
                4,
                "<reply-v1@example.com>",
                "Re: [PATCH v1 1/2] add first",
                Some("<patch-v1-1@example.com>"),
            ),
            (
                5,
                "<cover-v2@example.com>",
                "[PATCH v2 0/2] Demo series",
                None,
            ),
        ];
        let mails = messages
            .into_iter()
            .map(|(uid, message_id, subject, in_reply_to)| {
                let reply = in_reply_to
                    .map(|value| format!("In-Reply-To: {value}\n"))
                    .unwrap_or_default();
                let raw = format!(
                    "Message-ID: {message_id}\nSubject: {subject}\nFrom: Alice <alice@example.com>\n{reply}Change-ID: per-message-{uid}\n\nbody\n"
                );
                IncomingMail {
                    mailbox: "linux".to_string(),
                    uid,
                    modseq: Some(uid as u64),
                    flags: Vec::new(),
                    raw_path: PathBuf::from(format!("{uid}.eml")),
                    parsed: mail_parser::parse_headers(raw.as_bytes(), format!("fallback-{uid}")),
                }
            })
            .collect();
        apply_sync_batch(
            &path,
            SyncBatch {
                mailbox: "linux".to_string(),
                uidvalidity: 1,
                highest_uid: 5,
                highest_modseq: Some(5),
                mails,
            },
        )
        .expect("write sync batch");

        let persisted = refresh(&path, "linux").expect("refresh catalog");
        assert_eq!(persisted, 2);
        let catalog = load_catalog(&path, "linux").expect("load catalog");
        assert_eq!(catalog.len(), 2, "one entry per revision");

        let v1 = catalog.iter().find(|item| item.revision == 1).expect("v1");
        assert_eq!(v1.expected_total, 2);
        assert_eq!(v1.completeness, "complete");
        assert_eq!(v1.members.iter().filter(|member| member.seq > 0).count(), 2);
        assert_eq!(v1.followup_count, 1);

        let v2 = catalog.iter().find(|item| item.revision == 2).expect("v2");
        assert_eq!(v2.completeness, "partial");
        assert_eq!(v2.missing, vec![1, 2]);

        let _ = fs::remove_file(path);
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    }
}
