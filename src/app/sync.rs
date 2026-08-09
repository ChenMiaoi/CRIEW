//! Sync worker orchestration.
//!
//! Fixture, lore, and real IMAP sources all flow through the same batch shape
//! before touching storage. Keeping that normalization here lets the thread and
//! checkpoint code enforce one set of invariants regardless of the source.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::app::patch as patch_worker;
use crate::domain::subscriptions::uses_gnu_qemu_archive;
use crate::infra::config::{IMAP_INBOX_MAILBOX, RuntimeConfig};
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::imap::{
    FixtureImapClient, GnuArchiveClient, ImapClient, LoreImapClient, MailboxSnapshot,
    RemoteImapClient, RemoteMail,
};
use crate::infra::mail_parser::{self, ParsedMailHeaders};
use crate::infra::mail_store::{self, IncomingMail, SyncBatch};

const INITIAL_SYNC_THREAD_LIMIT: usize = 20;

#[derive(Debug)]
struct RemoteMailEnvelope {
    remote: RemoteMail,
    parsed: ParsedMailHeaders,
}

#[derive(Debug, Clone)]
struct InitialInboxSelection {
    scanned: usize,
    patch_related: usize,
    selected_threads: usize,
    selected_uids: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub mailbox: String,
    pub fixture_dir: Option<PathBuf>,
    pub uidvalidity: Option<u64>,
    pub reconnect_attempts: u8,
}

#[derive(Debug, Clone)]
pub struct ThreadFetchRequest {
    pub mailbox: String,
    pub message_id: String,
    pub reconnect_attempts: u8,
}

#[derive(Debug, Clone)]
pub struct SyncSummary {
    pub mailbox: String,
    pub source: String,
    pub fetched: usize,
    pub inserted: usize,
    pub updated: usize,
    pub rebuilt_roots: usize,
    pub mailbox_rebuilt: bool,
    pub uidvalidity: u64,
    pub checkpoint_last_seen_uid: u32,
    pub checkpoint_highest_modseq: Option<u64>,
    pub checkpoint_synced_at: Option<String>,
}

#[derive(Debug, Clone)]
enum SyncSource {
    Fixture {
        fixture_dir: PathBuf,
        uidvalidity_hint: u64,
    },
    Imap,
    GnuArchive,
    Lore {
        base_url: String,
    },
}

impl SyncSource {
    fn label(&self) -> String {
        match self {
            Self::Fixture { fixture_dir, .. } => fixture_dir.display().to_string(),
            Self::Imap => "imap".to_string(),
            Self::GnuArchive => "https://lists.gnu.org/archive/mbox".to_string(),
            Self::Lore { base_url } => base_url.clone(),
        }
    }
}

pub fn run(config: &RuntimeConfig, request: SyncRequest) -> Result<SyncSummary> {
    let source = resolve_sync_source(config, &request)?;
    let attempts = request.reconnect_attempts.max(1);
    let mut last_error: Option<CriewError> = None;

    for attempt in 1..=attempts {
        match run_once(config, &request.mailbox, &source) {
            Ok(summary) => return Ok(summary),
            Err(error) => {
                tracing::warn!(
                    attempt,
                    attempts,
                    mailbox = %request.mailbox,
                    source = %source.label(),
                    error = %error,
                    "sync attempt failed"
                );
                last_error = Some(error);
                if attempt < attempts {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CriewError::new(
            ErrorCode::Imap,
            format!("sync failed after {} attempts", attempts),
        )
    }))
}

pub fn fetch_thread(config: &RuntimeConfig, request: ThreadFetchRequest) -> Result<SyncSummary> {
    let source = resolve_sync_source(
        config,
        &SyncRequest {
            mailbox: request.mailbox.clone(),
            fixture_dir: None,
            uidvalidity: None,
            reconnect_attempts: request.reconnect_attempts,
        },
    )?;
    let attempts = request.reconnect_attempts.max(1);
    let mut last_error: Option<CriewError> = None;

    for attempt in 1..=attempts {
        match fetch_thread_once(config, &request.mailbox, &request.message_id, &source) {
            Ok(summary) => return Ok(summary),
            Err(error) => {
                tracing::warn!(
                    attempt,
                    attempts,
                    mailbox = %request.mailbox,
                    message_id = %request.message_id,
                    source = %source.label(),
                    error = %error,
                    "thread fetch attempt failed"
                );
                last_error = Some(error);
                if attempt < attempts {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CriewError::new(
            ErrorCode::Imap,
            format!("thread fetch failed after {} attempts", attempts),
        )
    }))
}

fn resolve_sync_source(config: &RuntimeConfig, request: &SyncRequest) -> Result<SyncSource> {
    if let Some(fixture_dir) = request.fixture_dir.as_ref() {
        return Ok(SyncSource::Fixture {
            fixture_dir: fixture_dir.clone(),
            uidvalidity_hint: request.uidvalidity.unwrap_or(1),
        });
    }

    if request.mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX) {
        // `INBOX` is the only mailbox that depends on private account state, so
        // treat it as an explicit IMAP-only path and fail fast when credentials
        // are incomplete instead of silently falling back to lore.
        if config.imap.is_complete() {
            return Ok(SyncSource::Imap);
        }

        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP config is incomplete for {}: missing {}",
                IMAP_INBOX_MAILBOX,
                config.imap.missing_required_fields().join(", ")
            ),
        ));
    }

    if uses_gnu_qemu_archive(&request.mailbox) {
        return Ok(SyncSource::GnuArchive);
    }

    Ok(SyncSource::Lore {
        base_url: config.lore_base_url.clone(),
    })
}

fn fetch_thread_once(
    config: &RuntimeConfig,
    mailbox: &str,
    message_id: &str,
    source: &SyncSource,
) -> Result<SyncSummary> {
    let checkpoint = mail_store::load_mailbox_state(&config.database_path, mailbox)?;
    let checkpoint_last_seen_uid = checkpoint
        .as_ref()
        .map(|state| state.last_seen_uid)
        .unwrap_or(0);
    let mut client: Box<dyn ImapClient> = match source {
        SyncSource::Fixture {
            fixture_dir,
            uidvalidity_hint,
        } => Box::new(FixtureImapClient::new(
            fixture_dir.to_path_buf(),
            *uidvalidity_hint,
        )),
        SyncSource::Imap => Box::new(RemoteImapClient::new(config.imap.clone())?),
        SyncSource::GnuArchive => Box::new(GnuArchiveClient::new(None)?),
        SyncSource::Lore { base_url } => Box::new(LoreImapClient::new(Some(base_url))?),
    };

    client.connect()?;
    let snapshot = match source {
        SyncSource::Lore { .. } => {
            // A native public-inbox thread endpoint is independent of the
            // mailbox's recent Atom feed. Reuse the persisted checkpoint so
            // an old thread can still be recovered after it leaves the feed.
            MailboxSnapshot {
                uidvalidity: checkpoint
                    .as_ref()
                    .map(|state| state.uidvalidity)
                    .unwrap_or(1),
                highest_uid: checkpoint_last_seen_uid,
                highest_modseq: checkpoint.as_ref().and_then(|state| state.highest_modseq),
            }
        }
        _ => client.select_mailbox(mailbox)?,
    };
    let remote_messages = client.fetch_thread(mailbox, message_id)?;
    let envelopes = parse_remote_messages(mailbox, remote_messages);
    let requested_message_id = normalize_requested_message_id(message_id);
    if !envelopes
        .iter()
        .any(|envelope| envelope.parsed.message_id == requested_message_id)
    {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!("fetched thread does not contain Message-ID '{message_id}'"),
        ));
    }

    let fetched = envelopes.len();
    let mut incoming = Vec::with_capacity(fetched);
    let mut synthetic_uid = checkpoint_last_seen_uid;
    for envelope in envelopes {
        let mut remote = envelope.remote;
        if remote.uid == 0 {
            synthetic_uid = synthetic_uid.saturating_add(1);
            remote.uid = synthetic_uid;
        }

        let raw_path = persist_raw_mail(config, mailbox, remote.uid, &remote.raw)?;
        incoming.push(IncomingMail {
            mailbox: mailbox.to_string(),
            uid: remote.uid,
            modseq: remote.modseq,
            flags: remote.flags,
            raw_path,
            parsed: envelope.parsed,
        });
    }

    let fetched_highest_uid = incoming
        .iter()
        .map(|mail| mail.uid)
        .max()
        .unwrap_or(checkpoint_last_seen_uid);
    let fetched_highest_modseq = incoming.iter().filter_map(|mail| mail.modseq).max();
    let batch_highest_uid = snapshot
        .highest_uid
        .max(fetched_highest_uid)
        .max(checkpoint_last_seen_uid);
    let batch_highest_modseq = max_option(snapshot.highest_modseq, fetched_highest_modseq);

    let write_result = mail_store::apply_sync_batch(
        &config.database_path,
        SyncBatch {
            mailbox: mailbox.to_string(),
            uidvalidity: snapshot.uidvalidity,
            highest_uid: batch_highest_uid,
            highest_modseq: batch_highest_modseq,
            mails: incoming,
        },
    )?;

    Ok(SyncSummary {
        mailbox: write_result.state.mailbox.clone(),
        source: source.label(),
        fetched,
        inserted: write_result.inserted,
        updated: write_result.updated,
        rebuilt_roots: write_result.rebuilt_roots,
        mailbox_rebuilt: write_result.mailbox_rebuilt,
        uidvalidity: write_result.state.uidvalidity,
        checkpoint_last_seen_uid: write_result.state.last_seen_uid,
        checkpoint_highest_modseq: write_result.state.highest_modseq,
        checkpoint_synced_at: write_result.state.synced_at.clone(),
    })
}

fn run_once(config: &RuntimeConfig, mailbox: &str, source: &SyncSource) -> Result<SyncSummary> {
    let checkpoint = mail_store::load_mailbox_state(&config.database_path, mailbox)?;
    let checkpoint_last_seen_uid = checkpoint
        .as_ref()
        .map(|state| state.last_seen_uid)
        .unwrap_or(0);
    let mailbox_message_count = mail_store::mailbox_message_count(&config.database_path, mailbox)?;
    // An empty mailbox gets a different startup path because paying the cost of
    // a bounded initial window is better than downloading an entire busy inbox
    // before the first TUI frame appears.
    let initial_window_sync = mailbox_message_count == 0;

    let mut client: Box<dyn ImapClient> = match source {
        SyncSource::Fixture {
            fixture_dir,
            uidvalidity_hint,
        } => Box::new(FixtureImapClient::new(
            fixture_dir.to_path_buf(),
            *uidvalidity_hint,
        )),
        SyncSource::Imap => Box::new(RemoteImapClient::new(config.imap.clone())?),
        SyncSource::GnuArchive => Box::new(GnuArchiveClient::new(None)?),
        SyncSource::Lore { base_url } => Box::new(LoreImapClient::new(Some(base_url))?),
    };

    client.connect()?;
    let snapshot = client.select_mailbox(mailbox)?;

    let mailbox_rebuilt = checkpoint
        .as_ref()
        .is_some_and(|state| state.uidvalidity != snapshot.uidvalidity);

    let after_uid = if mailbox_rebuilt {
        0
    } else {
        checkpoint_last_seen_uid
    };

    let since_modseq = if mailbox_rebuilt {
        None
    } else {
        checkpoint.as_ref().and_then(|state| state.highest_modseq)
    };

    // `My Inbox` can contain a large amount of unrelated mail. On the very
    // first sync we inspect headers first so we can bound startup latency and
    // only fetch full raws for the latest patch-related threads.
    let initial_inbox_selection = if initial_window_sync
        && mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX)
    {
        let header_candidates = client.fetch_header_candidates(mailbox, after_uid, since_modseq)?;
        let selection =
            select_initial_inbox_messages(mailbox, header_candidates, INITIAL_SYNC_THREAD_LIMIT);
        tracing::info!(
            op = "inbox_initial_sync",
            mailbox = %mailbox,
            status = "selected",
            scanned = selection.scanned,
            patch_related = selection.patch_related,
            selected_threads = selection.selected_threads,
            selected_messages = selection.selected_uids.len()
        );
        Some(selection)
    } else {
        None
    };

    let remote_messages = if let Some(selection) = initial_inbox_selection.as_ref() {
        client.fetch_full_uids(mailbox, &selection.selected_uids)?
    } else {
        client.fetch_incremental(mailbox, after_uid, since_modseq)?
    };

    let mut envelopes = parse_remote_messages(mailbox, remote_messages);
    if mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX) {
        let before = envelopes.len();
        envelopes
            .retain(|envelope| patch_worker::subject_is_patch_related(&envelope.parsed.subject));
        let filtered_out = before.saturating_sub(envelopes.len());
        if filtered_out > 0 {
            tracing::info!(
                op = "inbox_filter",
                mailbox = %mailbox,
                status = "filtered",
                rule = "patch_related",
                kept = envelopes.len(),
                filtered_out
            );
        }
    }
    if initial_window_sync && initial_inbox_selection.is_none() {
        envelopes = retain_latest_threads(envelopes, INITIAL_SYNC_THREAD_LIMIT);
    }

    let fetched = envelopes.len();
    let mut incoming = Vec::with_capacity(fetched);
    let mut synthetic_uid = checkpoint_last_seen_uid;

    for envelope in envelopes {
        let mut remote = envelope.remote;
        if remote.uid == 0 {
            // Fixture and lore sources do not always provide stable IMAP UIDs.
            // Synthesize monotonic values so raw mail filenames and checkpoint
            // math still follow the same assumptions as the real IMAP path.
            synthetic_uid = synthetic_uid.saturating_add(1);
            remote.uid = synthetic_uid;
        }

        let raw_path = persist_raw_mail(config, mailbox, remote.uid, &remote.raw)?;

        incoming.push(IncomingMail {
            mailbox: mailbox.to_string(),
            uid: remote.uid,
            modseq: remote.modseq,
            flags: remote.flags,
            raw_path,
            parsed: envelope.parsed,
        });
    }

    let fetched_highest_uid = incoming
        .iter()
        .map(|mail| mail.uid)
        .max()
        .unwrap_or(checkpoint_last_seen_uid);
    let fetched_highest_modseq = incoming.iter().filter_map(|mail| mail.modseq).max();

    let batch_highest_uid = snapshot
        .highest_uid
        .max(fetched_highest_uid)
        .max(checkpoint_last_seen_uid);

    // Keep checkpoint progress monotonic even when the selected fetch window is
    // intentionally partial, such as the initial inbox optimization.
    let batch_highest_modseq = max_option(snapshot.highest_modseq, fetched_highest_modseq);

    let write_result = mail_store::apply_sync_batch(
        &config.database_path,
        SyncBatch {
            mailbox: mailbox.to_string(),
            uidvalidity: snapshot.uidvalidity,
            highest_uid: batch_highest_uid,
            highest_modseq: batch_highest_modseq,
            mails: incoming,
        },
    )?;
    if mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX) {
        // The initial header scan is only an optimization. Pruning after the
        // write keeps later incremental syncs from slowly reintroducing
        // unrelated mail into the patch-focused inbox view.
        let pruned =
            mail_store::prune_mailbox_subjects(&config.database_path, mailbox, |subject| {
                patch_worker::subject_is_patch_related(subject)
            })?;
        if pruned > 0 {
            tracing::info!(
                op = "inbox_filter",
                mailbox = %mailbox,
                status = "pruned",
                rule = "patch_related",
                pruned
            );
        }
    }

    Ok(SyncSummary {
        mailbox: write_result.state.mailbox.clone(),
        source: source.label(),
        fetched,
        inserted: write_result.inserted,
        updated: write_result.updated,
        rebuilt_roots: write_result.rebuilt_roots,
        mailbox_rebuilt: write_result.mailbox_rebuilt,
        uidvalidity: write_result.state.uidvalidity,
        checkpoint_last_seen_uid: write_result.state.last_seen_uid,
        checkpoint_highest_modseq: write_result.state.highest_modseq,
        checkpoint_synced_at: write_result.state.synced_at.clone(),
    })
}

fn parse_remote_messages(
    mailbox: &str,
    remote_messages: Vec<RemoteMail>,
) -> Vec<RemoteMailEnvelope> {
    remote_messages
        .into_iter()
        .enumerate()
        .map(|(index, remote)| {
            // Some sources do not provide stable Message-Ids. Generate a
            // mailbox-scoped fallback so the rest of the pipeline can still
            // reason about threading and de-duplication deterministically.
            let fallback_message_id = if remote.uid == 0 {
                format!("synthetic-{mailbox}-{index}@local")
            } else {
                format!("synthetic-{mailbox}-{}@local", remote.uid)
            };
            let parsed = mail_parser::parse_headers(&remote.raw, fallback_message_id);
            RemoteMailEnvelope { remote, parsed }
        })
        .collect()
}

fn normalize_requested_message_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

fn select_initial_inbox_messages(
    mailbox: &str,
    remote_messages: Vec<RemoteMail>,
    thread_limit: usize,
) -> InitialInboxSelection {
    let scanned = remote_messages.len();
    let mut envelopes = parse_remote_messages(mailbox, remote_messages);
    envelopes.retain(|envelope| patch_worker::subject_is_patch_related(&envelope.parsed.subject));
    let patch_related = envelopes.len();
    let selected = retain_latest_threads(envelopes, thread_limit);
    let mut index_by_message_id = HashMap::new();
    // Root detection walks parent chains, so build the lookup map once before
    // counting distinct threads in the selected working set.
    for (index, message) in selected.iter().enumerate() {
        index_by_message_id.insert(message.parsed.message_id.clone(), index);
    }
    let selected_threads = (0..selected.len())
        .map(|index| thread_root_key(index, &selected, &index_by_message_id))
        .collect::<HashSet<String>>()
        .len();
    let selected_uids = selected
        .into_iter()
        .map(|message| message.remote.uid)
        .filter(|uid| *uid > 0)
        .collect();

    InitialInboxSelection {
        scanned,
        patch_related,
        selected_threads,
        selected_uids,
    }
}

fn retain_latest_threads(
    messages: Vec<RemoteMailEnvelope>,
    thread_limit: usize,
) -> Vec<RemoteMailEnvelope> {
    if thread_limit == 0 || messages.is_empty() {
        return Vec::new();
    }

    let mut index_by_message_id = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        index_by_message_id.insert(message.parsed.message_id.clone(), index);
    }

    let root_keys: Vec<String> = (0..messages.len())
        .map(|index| thread_root_key(index, &messages, &index_by_message_id))
        .collect();

    let mut latest_rank_by_thread = HashMap::new();
    for (index, root_key) in root_keys.iter().enumerate() {
        let rank = message_sort_rank(&messages[index]);
        latest_rank_by_thread
            .entry(root_key.clone())
            .and_modify(|existing| {
                if rank > *existing {
                    *existing = rank;
                }
            })
            .or_insert(rank);
    }

    let mut threads: Vec<(String, u64)> = latest_rank_by_thread.into_iter().collect();
    // Rank threads by their newest activity, then keep ordering deterministic
    // for equal timestamps so tests and the first-sync window stay stable.
    threads.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    let selected_roots: HashSet<String> = threads
        .into_iter()
        .take(thread_limit)
        .map(|(root_key, _)| root_key)
        .collect();

    let mut selected: Vec<RemoteMailEnvelope> = messages
        .into_iter()
        .zip(root_keys)
        .filter_map(|(message, root_key)| {
            if selected_roots.contains(&root_key) {
                Some(message)
            } else {
                None
            }
        })
        .collect();

    selected.sort_by_key(message_sort_rank);
    selected
}

fn message_sort_rank(message: &RemoteMailEnvelope) -> u64 {
    let modseq = message.remote.modseq.unwrap_or(0);
    (modseq << 32) | message.remote.uid as u64
}

fn thread_root_key(
    index: usize,
    messages: &[RemoteMailEnvelope],
    index_by_message_id: &HashMap<String, usize>,
) -> String {
    let mut current = index;
    let mut seen = HashSet::new();

    loop {
        if !seen.insert(current) {
            return messages[index].parsed.message_id.clone();
        }

        let Some(parent) = parent_index(current, messages, index_by_message_id) else {
            // If the root mail is missing locally, the first `References`
            // entry is usually the most stable stand-in for the thread root.
            // That keeps siblings grouped together during the initial window
            // selection instead of treating each reply as its own thread.
            if let Some(root_hint) = messages[current].parsed.references.first()
                && !root_hint.is_empty()
            {
                return root_hint.clone();
            }
            return messages[current].parsed.message_id.clone();
        };
        current = parent;
    }
}

fn parent_index(
    index: usize,
    messages: &[RemoteMailEnvelope],
    index_by_message_id: &HashMap<String, usize>,
) -> Option<usize> {
    let current_message_id = messages[index].parsed.message_id.as_str();

    // Prefer `References` because it usually preserves the full ancestry chain;
    // `In-Reply-To` is the fallback when only the direct parent survives.
    if let Some(parent) = messages[index]
        .parsed
        .references
        .iter()
        .rev()
        .find_map(|reference| index_by_message_id.get(reference).copied())
        && messages[parent].parsed.message_id != current_message_id
    {
        return Some(parent);
    }

    messages[index]
        .parsed
        .in_reply_to
        .as_ref()
        .and_then(|reply_to| index_by_message_id.get(reply_to).copied())
        .filter(|parent| messages[*parent].parsed.message_id != current_message_id)
}

fn persist_raw_mail(
    config: &RuntimeConfig,
    mailbox: &str,
    uid: u32,
    raw: &[u8],
) -> Result<PathBuf> {
    let mailbox_dir = config.raw_mail_dir.join(mailbox);
    fs::create_dir_all(&mailbox_dir).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!(
                "failed to create raw mail directory {}",
                mailbox_dir.display()
            ),
            error,
        )
    })?;

    // Zero-padding keeps lexicographic order aligned with UID order, which
    // makes fixture inspection and manual debugging much easier.
    let path = mailbox_dir.join(format!("{:010}.eml", uid));
    fs::write(&path, raw).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!("failed to write raw mail file {}", path.display()),
            error,
        )
    })?;

    Ok(path)
}

fn max_option(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(l), Some(r)) => Some(l.max(r)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::infra::db;
    use crate::infra::mail_store;

    use super::{SyncRequest, SyncSource, resolve_sync_source, run, select_initial_inbox_messages};
    use crate::infra::config::{IMAP_INBOX_MAILBOX, RuntimeConfig};
    use crate::infra::imap::RemoteMail;

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-sync-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn sync_worker_imports_fixture_mails_and_builds_threads() {
        let root = temp_dir("fixture");
        let fixture_dir = root.join("fixture");
        let data_dir = root.join("data");
        let raw_dir = data_dir.join("raw");
        let db_path = data_dir.join("criew.db");
        fs::create_dir_all(&fixture_dir).expect("create fixture dir");
        fs::create_dir_all(&raw_dir).expect("create raw dir");

        fs::write(
            fixture_dir.join("1-root.eml"),
            "Message-ID: <root@example.com>\nSubject: [PATCH 0/2] root\nFrom: alice@example.com\n\nbody\n",
        )
        .expect("write root");
        fs::write(
            fixture_dir.join("2-reply.eml"),
            "Message-ID: <reply@example.com>\nSubject: Re: [PATCH 0/2] root\nFrom: bob@example.com\nIn-Reply-To: <root@example.com>\nReferences: <root@example.com>\n\nbody\n",
        )
        .expect("write reply");

        db::initialize(&db_path).expect("initialize db");

        let runtime = RuntimeConfig {
            config_path: root.join("config.toml"),
            data_dir: data_dir.clone(),
            database_path: db_path.clone(),
            raw_mail_dir: raw_dir,
            patch_dir: data_dir.join("patches"),
            log_dir: data_dir.join("logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "inbox".to_string(),
            imap: crate::infra::config::ImapConfig::default(),
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        };

        let first = run(
            &runtime,
            SyncRequest {
                mailbox: "inbox".to_string(),
                fixture_dir: Some(fixture_dir.clone()),
                uidvalidity: Some(1),
                reconnect_attempts: 1,
            },
        )
        .expect("first sync");
        assert_eq!(first.fetched, 2);
        assert_eq!(first.inserted, 2);
        assert_eq!(first.updated, 0);

        let second = run(
            &runtime,
            SyncRequest {
                mailbox: "inbox".to_string(),
                fixture_dir: Some(fixture_dir),
                uidvalidity: Some(1),
                reconnect_attempts: 1,
            },
        )
        .expect("second sync");
        assert_eq!(second.fetched, 0);
        assert_eq!(second.inserted, 0);
        assert_eq!(second.updated, 0);

        let rows = mail_store::load_thread_rows_by_mailbox(&db_path, "inbox", 20)
            .expect("load thread rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.depth == 1));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initial_inbox_selection_prefers_latest_twenty_patch_threads() {
        let remote_messages: Vec<RemoteMail> = (1..=25u32)
            .flat_map(|uid| {
                let root = RemoteMail {
                    uid: uid * 2 - 1,
                    modseq: Some((uid * 2 - 1) as u64),
                    flags: Vec::new(),
                    raw: format!(
                        "Message-ID: <thread-{uid}@example.com>\nSubject: [PATCH 0/1] thread {uid}\nFrom: user{uid}@example.com\n\n"
                    )
                    .into_bytes(),
                };
                let reply = RemoteMail {
                    uid: uid * 2,
                    modseq: Some((uid * 2) as u64),
                    flags: Vec::new(),
                    raw: format!(
                        "Message-ID: <thread-{uid}-reply@example.com>\nSubject: Re: [PATCH 0/1] thread {uid}\nFrom: reply{uid}@example.com\nIn-Reply-To: <thread-{uid}@example.com>\nReferences: <thread-{uid}@example.com>\n\n"
                    )
                    .into_bytes(),
                };
                [root, reply]
            })
            .chain(std::iter::once(RemoteMail {
                uid: 1000,
                modseq: Some(1000),
                flags: Vec::new(),
                raw: b"Message-ID: <status@example.com>\nSubject: Weekly status update\nFrom: noise@example.com\n\n"
                    .to_vec(),
            }))
            .collect();

        let selection = select_initial_inbox_messages("INBOX", remote_messages, 20);

        assert_eq!(selection.scanned, 51);
        assert_eq!(selection.patch_related, 50);
        assert_eq!(selection.selected_threads, 20);
        assert_eq!(selection.selected_uids.len(), 40);
        assert!(!selection.selected_uids.contains(&1));
        assert!(selection.selected_uids.contains(&11));
        assert!(selection.selected_uids.contains(&50));
    }

    #[test]
    fn inbox_sync_keeps_only_patch_related_mail() {
        let root = temp_dir("inbox-patch-only");
        let fixture_dir = root.join("fixture");
        let data_dir = root.join("data");
        let raw_dir = data_dir.join("raw");
        let db_path = data_dir.join("criew.db");
        fs::create_dir_all(&fixture_dir).expect("create fixture dir");
        fs::create_dir_all(&raw_dir).expect("create raw dir");

        fs::write(
            fixture_dir.join("1-cover.eml"),
            "Message-ID: <cover@example.com>\nSubject: [PATCH 0/1] demo\nFrom: alice@example.com\n\nbody\n",
        )
        .expect("write patch cover");
        fs::write(
            fixture_dir.join("2-reply.eml"),
            "Message-ID: <reply@example.com>\nSubject: Re: [PATCH 0/1] demo\nFrom: bob@example.com\nIn-Reply-To: <cover@example.com>\nReferences: <cover@example.com>\n\nbody\n",
        )
        .expect("write patch reply");
        fs::write(
            fixture_dir.join("3-status.eml"),
            "Message-ID: <status@example.com>\nSubject: Weekly status update\nFrom: carol@example.com\n\nbody\n",
        )
        .expect("write non patch mail");

        db::initialize(&db_path).expect("initialize db");

        let runtime = RuntimeConfig {
            config_path: root.join("config.toml"),
            data_dir: data_dir.clone(),
            database_path: db_path.clone(),
            raw_mail_dir: raw_dir,
            patch_dir: data_dir.join("patches"),
            log_dir: data_dir.join("logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "inbox".to_string(),
            imap: crate::infra::config::ImapConfig::default(),
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        };

        let summary = run(
            &runtime,
            SyncRequest {
                mailbox: "inbox".to_string(),
                fixture_dir: Some(fixture_dir),
                uidvalidity: Some(1),
                reconnect_attempts: 1,
            },
        )
        .expect("sync inbox");

        assert_eq!(summary.fetched, 2);
        assert_eq!(summary.inserted, 2);

        let rows = mail_store::load_thread_rows_by_mailbox(&db_path, "inbox", 20)
            .expect("load thread rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.subject.contains("[PATCH")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn initial_empty_mailbox_sync_keeps_latest_twenty_threads() {
        let root = temp_dir("initial-window");
        let fixture_dir = root.join("fixture");
        let data_dir = root.join("data");
        let raw_dir = data_dir.join("raw");
        let db_path = data_dir.join("criew.db");
        fs::create_dir_all(&fixture_dir).expect("create fixture dir");
        fs::create_dir_all(&raw_dir).expect("create raw dir");

        for uid in 1..=25u32 {
            fs::write(
                fixture_dir.join(format!("{uid:04}-thread-{uid}.eml")),
                format!(
                    "Message-ID: <thread-{uid}@example.com>\nSubject: [PATCH] thread {uid}\nFrom: user{uid}@example.com\n\nbody {uid}\n"
                ),
            )
            .expect("write fixture");
        }

        db::initialize(&db_path).expect("initialize db");

        let runtime = RuntimeConfig {
            config_path: root.join("config.toml"),
            data_dir: data_dir.clone(),
            database_path: db_path.clone(),
            raw_mail_dir: raw_dir,
            patch_dir: data_dir.join("patches"),
            log_dir: data_dir.join("logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "inbox".to_string(),
            imap: crate::infra::config::ImapConfig::default(),
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        };

        let summary = run(
            &runtime,
            SyncRequest {
                mailbox: "inbox".to_string(),
                fixture_dir: Some(fixture_dir),
                uidvalidity: Some(1),
                reconnect_attempts: 1,
            },
        )
        .expect("first sync");

        assert_eq!(summary.fetched, 20);
        assert_eq!(summary.inserted, 20);
        assert_eq!(summary.updated, 0);
        assert_eq!(summary.checkpoint_last_seen_uid, 25);

        let rows = mail_store::load_thread_rows_by_mailbox(&db_path, "inbox", 100)
            .expect("load thread rows");
        assert_eq!(rows.len(), 20);
        assert!(
            !rows
                .iter()
                .any(|row| row.message_id == "thread-1@example.com")
        );
        assert!(
            rows.iter()
                .any(|row| row.message_id == "thread-25@example.com")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn my_inbox_routes_to_real_imap_when_config_is_complete() {
        let runtime = RuntimeConfig {
            config_path: PathBuf::from("/tmp/criew-sync-route.toml"),
            data_dir: PathBuf::from("/tmp/criew-sync-route-data"),
            database_path: PathBuf::from("/tmp/criew-sync-route.db"),
            raw_mail_dir: PathBuf::from("/tmp/criew-sync-route-raw"),
            patch_dir: PathBuf::from("/tmp/criew-sync-route-patches"),
            log_dir: PathBuf::from("/tmp/criew-sync-route-logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "io-uring".to_string(),
            imap: crate::infra::config::ImapConfig {
                email: Some("me@example.com".to_string()),
                user: Some("imap-user".to_string()),
                pass: Some("imap-pass".to_string()),
                server: Some("imap.example.com".to_string()),
                server_port: Some(993),
                encryption: Some(crate::infra::config::ImapEncryption::Tls),
                proxy: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        };

        let source = resolve_sync_source(
            &runtime,
            &SyncRequest {
                mailbox: IMAP_INBOX_MAILBOX.to_string(),
                fixture_dir: None,
                uidvalidity: None,
                reconnect_attempts: 1,
            },
        )
        .expect("resolve source");

        assert!(matches!(source, SyncSource::Imap));
    }

    #[test]
    fn lore_subscriptions_stay_on_lore_when_imap_is_configured() {
        let runtime = RuntimeConfig {
            config_path: PathBuf::from("/tmp/criew-sync-route.toml"),
            data_dir: PathBuf::from("/tmp/criew-sync-route-data"),
            database_path: PathBuf::from("/tmp/criew-sync-route.db"),
            raw_mail_dir: PathBuf::from("/tmp/criew-sync-route-raw"),
            patch_dir: PathBuf::from("/tmp/criew-sync-route-patches"),
            log_dir: PathBuf::from("/tmp/criew-sync-route-logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "io-uring".to_string(),
            imap: crate::infra::config::ImapConfig {
                email: Some("me@example.com".to_string()),
                user: Some("imap-user".to_string()),
                pass: Some("imap-pass".to_string()),
                server: Some("imap.example.com".to_string()),
                server_port: Some(993),
                encryption: Some(crate::infra::config::ImapEncryption::Tls),
                proxy: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        };

        let source = resolve_sync_source(
            &runtime,
            &SyncRequest {
                mailbox: "io-uring".to_string(),
                fixture_dir: None,
                uidvalidity: None,
                reconnect_attempts: 1,
            },
        )
        .expect("resolve source");

        assert!(matches!(source, SyncSource::Lore { .. }));
    }

    #[test]
    fn qemu_subscriptions_route_to_gnu_archive() {
        let runtime = RuntimeConfig {
            config_path: PathBuf::from("/tmp/criew-sync-route.toml"),
            data_dir: PathBuf::from("/tmp/criew-sync-route-data"),
            database_path: PathBuf::from("/tmp/criew-sync-route.db"),
            raw_mail_dir: PathBuf::from("/tmp/criew-sync-route-raw"),
            patch_dir: PathBuf::from("/tmp/criew-sync-route-patches"),
            log_dir: PathBuf::from("/tmp/criew-sync-route-logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "qemu-rust".to_string(),
            imap: crate::infra::config::ImapConfig {
                email: Some("me@example.com".to_string()),
                user: Some("imap-user".to_string()),
                pass: Some("imap-pass".to_string()),
                server: Some("imap.example.com".to_string()),
                server_port: Some(993),
                encryption: Some(crate::infra::config::ImapEncryption::Tls),
                proxy: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees: Vec::new(),
        };

        let source = resolve_sync_source(
            &runtime,
            &SyncRequest {
                mailbox: "qemu-rust".to_string(),
                fixture_dir: None,
                uidvalidity: None,
                reconnect_attempts: 1,
            },
        )
        .expect("resolve source");

        assert!(matches!(source, SyncSource::GnuArchive));
    }
}
