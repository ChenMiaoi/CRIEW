//! Review-trailer aggregation and Review Inbox policy.
//!
//! Mail parsing and persistence keep individual trailer records, while this
//! module turns those records plus the existing patch-series analysis into a
//! user-facing review queue. The same index powers the CLI report and the TUI
//! filter so both surfaces agree on what still needs review.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::app::cli::ReviewInboxMode;
use crate::app::patch::{self, SeriesIntegrity};
use crate::infra::error::Result;
use crate::infra::mail_parser::ParsedTrailer;
use crate::infra::mail_store::{self, ThreadRow};

const REVIEW_TRAILER_KINDS: &[&str] = &["Reviewed-by", "Acked-by", "Tested-by"];
const REVIEW_INBOX_ROW_LIMIT: usize = 50_000;

#[derive(Debug, Clone)]
pub struct ReviewInboxEntry {
    pub thread_id: i64,
    pub subject: String,
    pub anchor_message_id: String,
    pub version: u32,
    pub present_count: usize,
    pub expected_total: u32,
    pub integrity: SeriesIntegrity,
    pub trailers: Vec<ParsedTrailer>,
}

impl ReviewInboxEntry {
    pub fn has_reviewed_by(&self) -> bool {
        self.trailers
            .iter()
            .any(|trailer| trailer.kind.eq_ignore_ascii_case("Reviewed-by"))
    }

    pub fn review_count(&self) -> usize {
        self.trailers
            .iter()
            .filter(|trailer| is_review_trailer_kind(&trailer.kind))
            .count()
    }

    pub fn status_label(&self) -> &'static str {
        if self.has_reviewed_by() {
            "reviewed"
        } else {
            "needs-review"
        }
    }

    pub fn matches_mode(&self, mode: ReviewInboxMode) -> bool {
        match mode {
            ReviewInboxMode::NeedsReview => !self.has_reviewed_by(),
            ReviewInboxMode::Reviewed => self.has_reviewed_by(),
            ReviewInboxMode::All => true,
        }
    }

    pub fn trailer_groups(&self) -> BTreeMap<String, Vec<String>> {
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut seen = HashSet::new();
        for trailer in &self.trailers {
            if !is_review_trailer_kind(&trailer.kind) {
                continue;
            }
            let key = (trailer.kind.clone(), trailer.value.clone());
            if !seen.insert(key) {
                continue;
            }
            groups
                .entry(trailer.kind.clone())
                .or_default()
                .push(trailer.value.clone());
        }
        groups
    }
}

pub fn build_review_index(
    database_path: &Path,
    mailbox: &str,
    threads: &[ThreadRow],
) -> Result<HashMap<i64, ReviewInboxEntry>> {
    let trailers_by_thread = mail_store::load_thread_trailers_by_mailbox(database_path, mailbox)?;
    let series_by_thread = patch::build_series_index(mailbox, threads);

    Ok(series_by_thread
        .into_iter()
        .map(|(thread_id, series)| {
            let present_count = series.present_count();
            let trailers = trailers_by_thread
                .get(&thread_id)
                .cloned()
                .unwrap_or_default();
            (
                thread_id,
                ReviewInboxEntry {
                    thread_id,
                    subject: series.subject,
                    anchor_message_id: series.anchor_message_id,
                    version: series.version,
                    present_count,
                    expected_total: series.expected_total,
                    integrity: series.integrity,
                    trailers,
                },
            )
        })
        .collect())
}

pub fn load_review_inbox(
    database_path: &Path,
    mailbox: &str,
    mode: ReviewInboxMode,
) -> Result<Vec<ReviewInboxEntry>> {
    let threads =
        mail_store::load_thread_rows_by_mailbox(database_path, mailbox, REVIEW_INBOX_ROW_LIMIT)?;
    let index = build_review_index(database_path, mailbox, &threads)?;
    let mut seen_threads = HashSet::new();
    let mut entries = Vec::new();

    for row in threads {
        if !seen_threads.insert(row.thread_id) {
            continue;
        }
        if let Some(entry) = index.get(&row.thread_id)
            && entry.matches_mode(mode)
        {
            entries.push(entry.clone());
        }
    }

    Ok(entries)
}

pub fn format_review_inbox(
    mailbox: &str,
    mode: ReviewInboxMode,
    entries: &[ReviewInboxEntry],
) -> String {
    let mut lines = vec![format!(
        "Review Inbox: mailbox={} mode={} entries={}",
        mailbox,
        mode_label(mode),
        entries.len()
    )];

    if entries.is_empty() {
        lines.push("(no matching patch series)".to_string());
        return lines.join("\n");
    }

    for entry in entries {
        lines.push(format!(
            "thread={} status={} patch=v{} {}/{} integrity={} reviews={} anchor=<{}> subject={}",
            entry.thread_id,
            entry.status_label(),
            entry.version,
            entry.present_count,
            entry.expected_total,
            entry.integrity.short_label(),
            entry.review_count(),
            entry.anchor_message_id,
            entry.subject
        ));
        for (kind, values) in entry.trailer_groups() {
            lines.push(format!("  {kind}: {}", values.join(", ")));
        }
    }

    lines.join("\n")
}

pub fn mode_label(mode: ReviewInboxMode) -> &'static str {
    match mode {
        ReviewInboxMode::NeedsReview => "needs-review",
        ReviewInboxMode::Reviewed => "reviewed",
        ReviewInboxMode::All => "all",
    }
}

fn is_review_trailer_kind(kind: &str) -> bool {
    REVIEW_TRAILER_KINDS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(kind))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ReviewInboxEntry, build_review_index, format_review_inbox, mode_label};
    use crate::app::cli::ReviewInboxMode;
    use crate::app::patch::SeriesIntegrity;
    use crate::infra::db;
    use crate::infra::mail_parser::ParsedTrailer;
    use crate::infra::mail_store::{self, IncomingMail, SyncBatch};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-review-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn sample_entry(reviewed: bool) -> ReviewInboxEntry {
        ReviewInboxEntry {
            thread_id: 7,
            subject: "[PATCH 1/1] demo".to_string(),
            anchor_message_id: "patch@example.com".to_string(),
            version: 2,
            present_count: 1,
            expected_total: 1,
            integrity: SeriesIntegrity::Complete,
            trailers: if reviewed {
                vec![ParsedTrailer {
                    kind: "Reviewed-by".to_string(),
                    value: "Reviewer <reviewer@example.com>".to_string(),
                }]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn review_modes_follow_reviewed_by_presence() {
        let needs_review = sample_entry(false);
        let reviewed = sample_entry(true);

        assert!(needs_review.matches_mode(ReviewInboxMode::NeedsReview));
        assert!(!needs_review.matches_mode(ReviewInboxMode::Reviewed));
        assert!(!reviewed.matches_mode(ReviewInboxMode::NeedsReview));
        assert!(reviewed.matches_mode(ReviewInboxMode::Reviewed));
        assert!(reviewed.matches_mode(ReviewInboxMode::All));
    }

    #[test]
    fn report_includes_review_trailer_values() {
        let report = format_review_inbox("io-uring", ReviewInboxMode::All, &[sample_entry(true)]);
        assert!(report.contains("mode=all"));
        assert!(report.contains("Reviewed-by: Reviewer <reviewer@example.com>"));
        assert_eq!(mode_label(ReviewInboxMode::NeedsReview), "needs-review");
    }

    #[test]
    fn build_index_aggregates_review_trailers_for_patch_thread() {
        let root = temp_dir("index");
        let database_path = root.join("criew.db");
        db::initialize(&database_path).expect("initialize database");
        let root_raw = b"Message-ID: <patch@example.com>\nSubject: [PATCH 1/1] demo\n\npatch\n";
        let reply_raw = b"Message-ID: <reply@example.com>\nIn-Reply-To: <patch@example.com>\nSubject: Re: [PATCH 1/1] demo\n\nReviewed-by: Reviewer <reviewer@example.com>\n";

        mail_store::apply_sync_batch(
            &database_path,
            SyncBatch {
                mailbox: "io-uring".to_string(),
                uidvalidity: 1,
                highest_uid: 2,
                highest_modseq: Some(2),
                mails: vec![
                    IncomingMail {
                        mailbox: "io-uring".to_string(),
                        uid: 1,
                        modseq: Some(1),
                        flags: Vec::new(),
                        raw_path: root.join("1.eml"),
                        parsed: crate::infra::mail_parser::parse_headers(
                            root_raw,
                            "patch@example.com".to_string(),
                        ),
                    },
                    IncomingMail {
                        mailbox: "io-uring".to_string(),
                        uid: 2,
                        modseq: Some(2),
                        flags: Vec::new(),
                        raw_path: root.join("2.eml"),
                        parsed: crate::infra::mail_parser::parse_headers(
                            reply_raw,
                            "reply@example.com".to_string(),
                        ),
                    },
                ],
            },
        )
        .expect("write review thread");

        let rows = mail_store::load_thread_rows_by_mailbox(&database_path, "io-uring", 20)
            .expect("load thread rows");
        let index =
            build_review_index(&database_path, "io-uring", &rows).expect("build review index");
        assert_eq!(index.len(), 1);
        let entry = index.values().next().expect("review entry");
        assert!(entry.has_reviewed_by());
        assert_eq!(entry.review_count(), 1);

        let _ = fs::remove_dir_all(root);
    }
}
