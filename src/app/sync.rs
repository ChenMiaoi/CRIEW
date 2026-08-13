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
use crate::infra::config::{FOLLOWING_VIEW, IMAP_INBOX_MAILBOX, RuntimeConfig};
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::imap::{
    FixtureImapClient, GnuArchiveClient, ImapClient, LoreImapClient, MailboxSnapshot,
    RemoteImapClient, RemoteMail,
};
use crate::infra::mail_parser::{self, ParsedMailHeaders, normalize_email_address};
use crate::infra::mail_store::{self, IncomingMail, SyncBatch};
use crate::infra::series_store;

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
    Following {
        self_email: String,
    },
    FollowingSent {
        self_email: String,
        mailbox: String,
    },
    GnuArchive,
    Lore {
        base_url: String,
    },
}

impl SyncSource {
    fn label(&self) -> String {
        match self {
            Self::Fixture { fixture_dir, .. } => fixture_dir.display().to_string(),
            Self::Following { .. } => "imap-following".to_string(),
            Self::FollowingSent { mailbox, .. } => format!("imap-following-sent:{mailbox}"),
            Self::GnuArchive => "https://lists.gnu.org/archive/mbox".to_string(),
            Self::Lore { base_url } => base_url.clone(),
        }
    }

    fn is_fixture(&self) -> bool {
        matches!(self, Self::Fixture { .. })
    }

    fn is_following_sent(&self) -> bool {
        matches!(self, Self::FollowingSent { .. })
    }

    fn following_self_email(&self) -> Option<&str> {
        match self {
            Self::Following { self_email } | Self::FollowingSent { self_email, .. } => {
                Some(self_email)
            }
            _ => None,
        }
    }

    fn storage_mailbox<'a>(&self, mailbox: &'a str) -> &'a str {
        if matches!(self, Self::Following { .. }) {
            IMAP_INBOX_MAILBOX
        } else {
            mailbox
        }
    }

    fn client(&self, config: &RuntimeConfig) -> Result<Box<dyn ImapClient>> {
        match self {
            Self::Fixture {
                fixture_dir,
                uidvalidity_hint,
            } => Ok(Box::new(FixtureImapClient::new(
                fixture_dir.clone(),
                *uidvalidity_hint,
            ))),
            Self::Following { .. } => Ok(Box::new(RemoteImapClient::new(config.imap.clone())?)),
            Self::FollowingSent { .. } => Ok(Box::new(RemoteImapClient::new(config.imap.clone())?)),
            Self::GnuArchive => Ok(Box::new(GnuArchiveClient::new(None)?)),
            Self::Lore { base_url } => Ok(Box::new(LoreImapClient::new(Some(base_url))?)),
        }
    }

    fn select_thread_snapshot(
        &self,
        client: &mut dyn ImapClient,
        mailbox: &str,
        checkpoint: Option<&mail_store::MailboxState>,
        checkpoint_last_seen_uid: u32,
    ) -> Result<MailboxSnapshot> {
        match self {
            Self::Lore { .. } => {
                // A native public-inbox thread endpoint is independent of the
                // mailbox's recent Atom feed. Reuse the persisted checkpoint so
                // an old thread can still be recovered after it leaves the feed.
                Ok(MailboxSnapshot {
                    uidvalidity: checkpoint.map(|state| state.uidvalidity).unwrap_or(1),
                    highest_uid: checkpoint_last_seen_uid,
                    highest_modseq: checkpoint.and_then(|state| state.highest_modseq),
                })
            }
            _ => client.select_mailbox(mailbox),
        }
    }
}

pub fn run(config: &RuntimeConfig, request: SyncRequest) -> Result<SyncSummary> {
    let source = resolve_sync_source(config, &request)?;
    let mut summary = retry_sync(
        request.reconnect_attempts,
        &request.mailbox,
        &source,
        None,
        || run_once(config, &request.mailbox, &source),
    )?;

    if let SyncSource::Following { self_email } = &source
        && let Some(sent_mailbox) =
            config
                .imap
                .sent_mailbox
                .as_deref()
                .map(str::trim)
                .filter(|mailbox| {
                    !mailbox.is_empty() && !mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX)
                })
    {
        let sent_source = SyncSource::FollowingSent {
            self_email: self_email.clone(),
            mailbox: sent_mailbox.to_string(),
        };
        match retry_sync(
            request.reconnect_attempts,
            sent_mailbox,
            &sent_source,
            None,
            || run_once(config, sent_mailbox, &sent_source),
        ) {
            Ok(sent_summary) => {
                summary.fetched += sent_summary.fetched;
                summary.inserted += sent_summary.inserted;
                summary.updated += sent_summary.updated;
                summary.rebuilt_roots += sent_summary.rebuilt_roots;
            }
            Err(error) => {
                tracing::warn!(
                    mailbox = %sent_mailbox,
                    error = %error,
                    "optional IMAP sent-mail sync failed; Following INBOX remains available"
                );
            }
        }
    }

    Ok(summary)
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
    retry_sync(
        request.reconnect_attempts,
        &request.mailbox,
        &source,
        Some(&request.message_id),
        || fetch_thread_once(config, &request.mailbox, &request.message_id, &source),
    )
}

fn retry_sync<T, F>(
    requested_attempts: u8,
    mailbox: &str,
    source: &SyncSource,
    message_id: Option<&str>,
    mut operation: F,
) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let attempts = requested_attempts.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                if let Some(message_id) = message_id {
                    tracing::warn!(
                        attempt,
                        attempts,
                        mailbox,
                        message_id,
                        source = %source.label(),
                        error = %error,
                        "thread fetch attempt failed"
                    );
                } else {
                    tracing::warn!(
                        attempt,
                        attempts,
                        mailbox,
                        source = %source.label(),
                        error = %error,
                        "sync attempt failed"
                    );
                }
                last_error = Some(error);
                if attempt < attempts {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        let operation_name = if message_id.is_some() {
            "thread fetch"
        } else {
            "sync"
        };
        CriewError::new(
            ErrorCode::Imap,
            format!("{operation_name} failed after {attempts} attempts"),
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

    if request.mailbox.eq_ignore_ascii_case(FOLLOWING_VIEW)
        || request.mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX)
    {
        // Following is a virtual view backed by the private IMAP INBOX. Keep
        // the legacy INBOX spelling accepted for CLI/config compatibility, but
        // never expose it as a separate user-facing subscription.
        if config.imap.is_complete() {
            let self_email = crate::infra::config::resolve_self_email(config)
                .email
                .ok_or_else(|| {
                    CriewError::new(
                        ErrorCode::Imap,
                        "self email is required for Following discovery; set imap.email or git config user.email",
                    )
                })?;
            return Ok(SyncSource::Following { self_email });
        }

        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP config is incomplete for Following: missing {}",
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
    let storage_mailbox = source.storage_mailbox(mailbox);
    let (checkpoint, checkpoint_last_seen_uid) = load_checkpoint(config, storage_mailbox)?;
    let mut client = source.client(config)?;

    client.connect()?;
    let snapshot = source.select_thread_snapshot(
        client.as_mut(),
        storage_mailbox,
        checkpoint.as_ref(),
        checkpoint_last_seen_uid,
    )?;
    let remote_messages = client.fetch_thread(storage_mailbox, message_id)?;
    let envelopes = parse_remote_messages(storage_mailbox, remote_messages);
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
    let write_result = persist_and_write(
        config,
        storage_mailbox,
        checkpoint_last_seen_uid,
        snapshot,
        envelopes,
        source,
    )?;

    Ok(build_summary(mailbox, source, fetched, write_result))
}

fn run_once(config: &RuntimeConfig, mailbox: &str, source: &SyncSource) -> Result<SyncSummary> {
    let storage_mailbox = source.storage_mailbox(mailbox);
    let (checkpoint, checkpoint_last_seen_uid) = load_checkpoint(config, storage_mailbox)?;
    let mailbox_message_count =
        mail_store::mailbox_message_count(&config.database_path, storage_mailbox)?;
    // An empty mailbox gets a different startup path because paying the cost of
    // a bounded initial window is better than downloading an entire busy inbox
    // before the first TUI frame appears.
    let initial_window_sync = mailbox_message_count == 0;

    let mut client = source.client(config)?;

    client.connect()?;
    let snapshot = client.select_mailbox(storage_mailbox)?;

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

    // The hidden IMAP INBOX can contain a large amount of unrelated mail. On
    // the very first fixture sync we inspect headers first so we can bound
    // startup latency and only fetch full raws for the latest patch threads.
    let initial_inbox_selection = select_initial_inbox(
        client.as_mut(),
        source,
        storage_mailbox,
        initial_window_sync,
        after_uid,
        since_modseq,
    )?;

    let remote_messages = fetch_messages(
        client.as_mut(),
        source,
        storage_mailbox,
        initial_inbox_selection.as_ref(),
        after_uid,
        since_modseq,
    )?;

    let mut envelopes = parse_remote_messages(storage_mailbox, remote_messages);
    complete_incomplete_patch_series(client.as_mut(), storage_mailbox, &mut envelopes, source)?;
    filter_messages(&mut envelopes, source, storage_mailbox);

    let fetched = envelopes.len();
    let write_result = persist_and_write(
        config,
        storage_mailbox,
        checkpoint_last_seen_uid,
        snapshot,
        envelopes,
        source,
    )?;

    prune_fixture_inbox(config, mailbox, storage_mailbox, source)?;

    Ok(build_summary(mailbox, source, fetched, write_result))
}
fn complete_incomplete_patch_series(
    client: &mut dyn ImapClient,
    mailbox: &str,
    envelopes: &mut Vec<RemoteMailEnvelope>,
    source: &SyncSource,
) -> Result<()> {
    type SeriesKey = (u32, u32, String, String, String);
    let mut present: HashMap<SeriesKey, HashSet<u32>> = HashMap::new();
    for envelope in envelopes.iter() {
        if is_reply_or_forward_subject(&envelope.parsed.subject) {
            continue;
        }
        if let Some((version, seq, total, title)) =
            patch_worker::patch_series_signature(&envelope.parsed.subject)
        {
            let key = (
                version,
                total,
                normalize_email_address(&envelope.parsed.from_addr).unwrap_or_default(),
                envelope.parsed.list_id.clone().unwrap_or_default(),
                series_family_key(&title),
            );
            present.entry(key).or_default().insert(seq);
        }
    }
    let incomplete: Vec<SeriesKey> = present
        .iter()
        .filter_map(|(key, seqs)| (key.1 > 0 && seqs.len() < key.1 as usize).then_some(key.clone()))
        .collect();
    if incomplete.is_empty() && !matches!(source, SyncSource::Lore { .. }) {
        return Ok(());
    }

    if matches!(source, SyncSource::Lore { .. }) {
        return complete_lore_patch_threads(client, mailbox, envelopes, &present, &incomplete);
    }

    let candidates = client.fetch_header_candidates(mailbox, 0, None)?;
    let mut missing_uids = HashSet::new();
    for candidate in &candidates {
        let parsed =
            mail_parser::parse_headers(&candidate.raw, format!("uid-{}@criew", candidate.uid));
        if is_reply_or_forward_subject(&parsed.subject) {
            continue;
        }
        let Some((candidate_version, seq, candidate_total, candidate_title)) =
            patch_worker::patch_series_signature(&parsed.subject)
        else {
            continue;
        };
        let candidate_key = (
            candidate_version,
            candidate_total,
            normalize_email_address(&parsed.from_addr).unwrap_or_default(),
            parsed.list_id.clone().unwrap_or_default(),
            series_family_key(&candidate_title),
        );
        if incomplete.contains(&candidate_key)
            && seq > 0
            && !present
                .get(&candidate_key)
                .is_some_and(|seqs| seqs.contains(&seq))
        {
            missing_uids.insert(candidate.uid);
        }
    }
    let missing_uids: Vec<u32> = missing_uids.into_iter().collect();
    if missing_uids.is_empty() {
        return Ok(());
    }
    let fetched = client.fetch_full_uids(mailbox, &missing_uids)?;
    let known: HashSet<String> = envelopes
        .iter()
        .map(|envelope| envelope.parsed.message_id.clone())
        .collect();
    for remote in fetched {
        let parsed = mail_parser::parse_headers(&remote.raw, format!("uid-{}@criew", remote.uid));
        if !known.contains(&parsed.message_id) {
            envelopes.push(RemoteMailEnvelope { remote, parsed });
        }
    }
    Ok(())
}

fn complete_lore_patch_threads(
    client: &mut dyn ImapClient,
    mailbox: &str,
    envelopes: &mut Vec<RemoteMailEnvelope>,
    present: &HashMap<(u32, u32, String, String, String), HashSet<u32>>,
    incomplete: &[(u32, u32, String, String, String)],
) -> Result<()> {
    type FamilyKey = (u32, u32, String);

    let mut present_by_family: HashMap<FamilyKey, HashSet<u32>> = HashMap::new();
    for (key, sequences) in present {
        present_by_family
            .entry((key.0, key.1, key.4.clone()))
            .or_default()
            .extend(sequences.iter().copied());
    }

    let incomplete_families: HashSet<FamilyKey> = incomplete
        .iter()
        .map(|key| (key.0, key.1, key.4.clone()))
        .collect();
    let known_non_reply_ids: HashSet<String> = envelopes
        .iter()
        .filter(|envelope| !is_reply_or_forward_subject(&envelope.parsed.subject))
        .map(|envelope| envelope.parsed.message_id.clone())
        .collect();
    let mut seeds: HashMap<FamilyKey, String> = HashMap::new();
    for envelope in envelopes.iter() {
        let Some((version, _seq, total, title)) =
            patch_worker::patch_series_signature(&envelope.parsed.subject)
        else {
            continue;
        };
        let family = (version, total, series_family_key(&title));
        let is_reply = is_reply_or_forward_subject(&envelope.parsed.subject);
        let needs_thread = if is_reply {
            let references_known = envelope
                .parsed
                .references
                .iter()
                .chain(envelope.parsed.in_reply_to.iter())
                .any(|reference| known_non_reply_ids.contains(reference));
            !references_known
                || !present_by_family
                    .get(&family)
                    .is_some_and(|sequences| sequences.len() >= total as usize)
        } else {
            incomplete_families.contains(&family)
        };
        if needs_thread {
            seeds
                .entry(family)
                .or_insert_with(|| envelope.parsed.message_id.clone());
        }
    }

    if seeds.is_empty() {
        return Ok(());
    }

    let mut known: HashSet<String> = envelopes
        .iter()
        .map(|envelope| envelope.parsed.message_id.clone())
        .collect();
    for message_id in seeds.values() {
        let fetched = match client.fetch_thread(mailbox, message_id) {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!(
                    mailbox = %mailbox,
                    message_id = %message_id,
                    error = %error,
                    "failed to complete lore patch thread"
                );
                continue;
            }
        };
        for remote in fetched {
            let parsed =
                mail_parser::parse_headers(&remote.raw, format!("uid-{}@criew", remote.uid));
            if known.insert(parsed.message_id.clone()) {
                envelopes.push(RemoteMailEnvelope { remote, parsed });
            }
        }
    }

    Ok(())
}

fn is_reply_or_forward_subject(subject: &str) -> bool {
    let value = subject.trim_start().to_ascii_lowercase();
    value.starts_with("re:") || value.starts_with("fwd:") || value.starts_with("fw:")
}

fn series_family_key(title: &str) -> String {
    title
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| !character.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .find(|word| !word.is_empty())
        .unwrap_or_default()
}

fn load_checkpoint(
    config: &RuntimeConfig,
    mailbox: &str,
) -> Result<(Option<mail_store::MailboxState>, u32)> {
    let checkpoint = mail_store::load_mailbox_state(&config.database_path, mailbox)?;
    let last_seen_uid = checkpoint
        .as_ref()
        .map(|state| state.last_seen_uid)
        .unwrap_or(0);
    Ok((checkpoint, last_seen_uid))
}

fn select_initial_inbox(
    client: &mut dyn ImapClient,
    source: &SyncSource,
    mailbox: &str,
    initial_window_sync: bool,
    after_uid: u32,
    since_modseq: Option<u64>,
) -> Result<Option<InitialInboxSelection>> {
    if !initial_window_sync || !source.is_fixture() || !is_inbox(mailbox) {
        return Ok(None);
    }

    let header_candidates = client.fetch_header_candidates(mailbox, after_uid, since_modseq)?;
    let selection = select_initial_inbox_messages(mailbox, header_candidates);
    tracing::info!(
        op = "inbox_initial_sync",
        mailbox = %mailbox,
        status = "selected",
        scanned = selection.scanned,
        patch_related = selection.patch_related,
        selected_threads = selection.selected_threads,
        selected_messages = selection.selected_uids.len()
    );
    Ok(Some(selection))
}

fn fetch_messages(
    client: &mut dyn ImapClient,
    source: &SyncSource,
    mailbox: &str,
    initial_selection: Option<&InitialInboxSelection>,
    after_uid: u32,
    since_modseq: Option<u64>,
) -> Result<Vec<RemoteMail>> {
    if let Some(selection) = initial_selection {
        return client.fetch_full_uids(mailbox, &selection.selected_uids);
    }

    if let Some(self_email) = source.following_self_email() {
        return client.fetch_following_incremental(mailbox, self_email, after_uid, since_modseq);
    }

    client.fetch_incremental(mailbox, after_uid, since_modseq)
}

fn filter_messages(envelopes: &mut Vec<RemoteMailEnvelope>, source: &SyncSource, mailbox: &str) {
    if let Some(self_email) = source.following_self_email() {
        let before = envelopes.len();
        if source.is_following_sent() {
            envelopes.retain(|envelope| is_following_sent_match(&envelope.parsed, self_email));
        } else {
            envelopes.retain(|envelope| is_following_match(&envelope.parsed, self_email));
        }
        log_filter_result("following_filter", mailbox, envelopes.len(), before);
        return;
    }

    if source.is_fixture() && is_inbox(mailbox) {
        let before = envelopes.len();
        envelopes
            .retain(|envelope| patch_worker::subject_is_patch_related(&envelope.parsed.subject));
        log_filter_result("inbox_filter", mailbox, envelopes.len(), before);
    }
}

fn log_filter_result(operation: &str, mailbox: &str, kept: usize, before: usize) {
    let filtered_out = before.saturating_sub(kept);
    if filtered_out == 0 {
        return;
    }

    tracing::info!(
        op = operation,
        mailbox = %mailbox,
        status = "filtered",
        kept,
        filtered_out
    );
}

fn persist_and_write(
    config: &RuntimeConfig,
    mailbox: &str,
    checkpoint_last_seen_uid: u32,
    snapshot: MailboxSnapshot,
    envelopes: Vec<RemoteMailEnvelope>,
    source: &SyncSource,
) -> Result<mail_store::SyncWriteResult> {
    let incoming = build_incoming_mail(config, mailbox, checkpoint_last_seen_uid, envelopes)?;
    let fetched_highest_uid = incoming
        .iter()
        .map(|mail| mail.uid)
        .max()
        .unwrap_or(checkpoint_last_seen_uid);
    let fetched_highest_modseq = incoming.iter().filter_map(|mail| mail.modseq).max();

    let write_result = mail_store::apply_sync_batch(
        &config.database_path,
        SyncBatch {
            mailbox: mailbox.to_string(),
            uidvalidity: snapshot.uidvalidity,
            highest_uid: snapshot
                .highest_uid
                .max(fetched_highest_uid)
                .max(checkpoint_last_seen_uid),
            // Keep checkpoint progress monotonic even when the selected fetch
            // window is intentionally partial.
            highest_modseq: max_option(snapshot.highest_modseq, fetched_highest_modseq),
            mails: incoming,
        },
    )?;

    if let Some(self_email) = source.following_self_email() {
        mail_store::refresh_following_updates(&config.database_path, self_email)?;
    }

    // Series resolution is a durable projection of all messages in the
    // source, not a calculation performed from the currently visible TUI
    // slice.  Refresh it after every ingest so later views can page safely.
    series_store::refresh(&config.database_path, mailbox)?;

    Ok(write_result)
}

fn build_incoming_mail(
    config: &RuntimeConfig,
    mailbox: &str,
    checkpoint_last_seen_uid: u32,
    envelopes: Vec<RemoteMailEnvelope>,
) -> Result<Vec<IncomingMail>> {
    let mut incoming = Vec::with_capacity(envelopes.len());
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

    Ok(incoming)
}

fn prune_fixture_inbox(
    config: &RuntimeConfig,
    mailbox: &str,
    storage_mailbox: &str,
    source: &SyncSource,
) -> Result<()> {
    if !source.is_fixture() || !is_inbox(storage_mailbox) {
        return Ok(());
    }

    // The initial header scan is only an optimization. Pruning after the write
    // keeps later incremental syncs from slowly reintroducing unrelated mail
    // into the patch-focused inbox view.
    let pruned = mail_store::prune_mailbox_subjects(
        &config.database_path,
        mailbox,
        patch_worker::subject_is_patch_related,
    )?;
    if pruned > 0 {
        tracing::info!(
            op = "inbox_filter",
            mailbox = %storage_mailbox,
            status = "pruned",
            rule = "patch_related",
            pruned
        );
    }

    Ok(())
}

fn is_inbox(mailbox: &str) -> bool {
    mailbox.eq_ignore_ascii_case(IMAP_INBOX_MAILBOX)
}

fn build_summary(
    mailbox: &str,
    source: &SyncSource,
    fetched: usize,
    write_result: mail_store::SyncWriteResult,
) -> SyncSummary {
    SyncSummary {
        mailbox: mailbox.to_string(),
        source: source.label(),
        fetched,
        inserted: write_result.inserted,
        updated: write_result.updated,
        rebuilt_roots: write_result.rebuilt_roots,
        mailbox_rebuilt: write_result.mailbox_rebuilt,
        uidvalidity: write_result.state.uidvalidity,
        checkpoint_last_seen_uid: write_result.state.last_seen_uid,
        checkpoint_highest_modseq: write_result.state.highest_modseq,
        checkpoint_synced_at: write_result.state.synced_at,
    }
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

fn is_following_match(parsed: &ParsedMailHeaders, self_email: &str) -> bool {
    let Some(self_email) = normalize_email_address(self_email) else {
        return false;
    };

    let matches_to = parsed
        .to_addresses
        .iter()
        .filter_map(|address| normalize_email_address(address))
        .any(|address| address == self_email);
    let matches_cc = parsed
        .cc_addresses
        .iter()
        .filter_map(|address| normalize_email_address(address))
        .any(|address| address == self_email);
    let is_sent_patch = normalize_email_address(&parsed.from_addr)
        .is_some_and(|address| address == self_email)
        && is_sent_patch_subject(&parsed.subject);
    let is_sent_reply = normalize_email_address(&parsed.from_addr)
        .is_some_and(|address| address == self_email)
        && is_reply_or_forward_subject(&parsed.subject)
        && patch_worker::subject_is_patch_related(&parsed.subject);

    matches_to || matches_cc || is_sent_patch || is_sent_reply
}

fn is_following_sent_match(parsed: &ParsedMailHeaders, self_email: &str) -> bool {
    normalize_email_address(&parsed.from_addr).is_some_and(|address| address == self_email)
        && patch_worker::subject_is_patch_related(&parsed.subject)
}

fn is_sent_patch_subject(subject: &str) -> bool {
    let normalized = subject.trim().to_ascii_lowercase();
    !normalized.starts_with("re:")
        && !normalized.starts_with("fwd:")
        && patch_worker::subject_is_patch_related(subject)
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
) -> InitialInboxSelection {
    let scanned = remote_messages.len();
    let mut selected = parse_remote_messages(mailbox, remote_messages);
    // Header discovery is already the bounded remote operation. Keep every
    // patch candidate so larger series are never truncated by message count.
    selected.retain(|envelope| patch_worker::subject_is_patch_related(&envelope.parsed.subject));
    let patch_related = selected.len();
    let mut index_by_message_id = HashMap::new();
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
    left.max(right)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::infra::db;
    use crate::infra::error::Result;
    use crate::infra::mail_parser;
    use crate::infra::mail_store;

    use super::{
        RemoteMailEnvelope, SyncRequest, SyncSource, complete_incomplete_patch_series,
        is_following_match, is_following_sent_match, resolve_sync_source, run,
        select_initial_inbox_messages,
    };
    use crate::infra::config::{IMAP_INBOX_MAILBOX, RuntimeConfig};
    use crate::infra::imap::{ImapClient, MailboxSnapshot, RemoteMail};

    struct ThreadFixtureClient {
        thread: Vec<RemoteMail>,
        requested_ids: Vec<String>,
    }

    impl ImapClient for ThreadFixtureClient {
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }

        fn select_mailbox(&mut self, _mailbox: &str) -> Result<MailboxSnapshot> {
            Ok(MailboxSnapshot {
                uidvalidity: 1,
                highest_uid: 0,
                highest_modseq: None,
            })
        }

        fn fetch_incremental(
            &mut self,
            _mailbox: &str,
            _after_uid: u32,
            _since_modseq: Option<u64>,
        ) -> Result<Vec<RemoteMail>> {
            Ok(Vec::new())
        }

        fn fetch_thread(&mut self, _mailbox: &str, message_id: &str) -> Result<Vec<RemoteMail>> {
            self.requested_ids.push(message_id.to_string());
            Ok(self.thread.clone())
        }
    }

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
            allow_preview_resize: false,
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
    fn initial_inbox_selection_keeps_all_patch_threads() {
        let remote_messages: Vec<RemoteMail> = (1..=25u32)
            .flat_map(|uid| {
                let root = RemoteMail {
                    uid: uid * 2 - 1,
                    modseq: Some((uid * 2 - 1) as u64),
                    flags: Vec::new(),
                    raw: format!("Message-ID: <thread-{uid}@example.com>\nSubject: [PATCH 0/1] thread {uid}\n\n").into_bytes(),
                };
                let reply = RemoteMail {
                    uid: uid * 2,
                    modseq: Some((uid * 2) as u64),
                    flags: Vec::new(),
                    raw: format!("Message-ID: <thread-{uid}-reply@example.com>\nSubject: Re: [PATCH 0/1] thread {uid}\nIn-Reply-To: <thread-{uid}@example.com>\nReferences: <thread-{uid}@example.com>\n\n").into_bytes(),
                };
                [root, reply]
            })
            .chain(std::iter::once(RemoteMail {
                uid: 1000,
                modseq: Some(1000),
                flags: Vec::new(),
                raw: b"Message-ID: <status@example.com>\nSubject: Weekly status update\n\n".to_vec(),
            }))
            .collect();

        let selection = select_initial_inbox_messages("INBOX", remote_messages);
        assert_eq!(selection.scanned, 51);
        assert_eq!(selection.patch_related, 50);
        assert_eq!(selection.selected_threads, 25);
        assert_eq!(selection.selected_uids.len(), 50);
        assert!(selection.selected_uids.contains(&1));
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
            allow_preview_resize: false,
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
    fn initial_empty_mailbox_sync_keeps_all_patch_threads() {
        let root = temp_dir("initial-window");
        let fixture_dir = root.join("fixture");
        let data_dir = root.join("data");
        let raw_dir = data_dir.join("raw");
        let db_path = data_dir.join("criew.db");
        fs::create_dir_all(&fixture_dir).expect("create fixture dir");
        fs::create_dir_all(&raw_dir).expect("create raw dir");
        for uid in 1..=25u32 {
            fs::write(fixture_dir.join(format!("{uid:04}-thread-{uid}.eml")), format!("Message-ID: <thread-{uid}@example.com>\nSubject: [PATCH] thread {uid}\n\nbody\n")).expect("write fixture");
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
            allow_preview_resize: false,
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
        assert_eq!(summary.fetched, 25);
        assert_eq!(summary.inserted, 25);
        assert_eq!(summary.checkpoint_last_seen_uid, 25);
        let rows = mail_store::load_thread_rows_by_mailbox(&db_path, "inbox", 100)
            .expect("load thread rows");
        assert_eq!(rows.len(), 25);
        assert!(
            rows.iter()
                .any(|row| row.message_id == "thread-1@example.com")
        );
        assert!(
            rows.iter()
                .any(|row| row.message_id == "thread-25@example.com")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn following_routes_to_real_imap_when_config_is_complete() {
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
                sent_mailbox: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            allow_preview_resize: false,
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

        assert!(matches!(source, SyncSource::Following { .. }));
    }

    #[test]
    fn following_view_name_routes_to_real_imap_when_config_is_complete() {
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
                sent_mailbox: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            allow_preview_resize: false,
            kernel_trees: Vec::new(),
        };

        let source = resolve_sync_source(
            &runtime,
            &SyncRequest {
                mailbox: crate::infra::config::FOLLOWING_VIEW.to_string(),
                fixture_dir: None,
                uidvalidity: None,
                reconnect_attempts: 1,
            },
        )
        .expect("resolve Following source");

        assert!(matches!(source, SyncSource::Following { .. }));
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
                sent_mailbox: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            allow_preview_resize: false,
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
                sent_mailbox: None,
            },
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            allow_preview_resize: false,

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

    #[test]
    fn initial_selection_keeps_patch_reply_without_root_candidate() {
        let messages = vec![
            RemoteMail {
                uid: 1,
                modseq: None,
                flags: Vec::new(),
                raw: b"Message-ID: <reply@example.com>\nSubject: Re: [PATCH] missing\nReferences: <root@example.com>\n\n".to_vec(),
            },
            RemoteMail {
                uid: 2,
                modseq: None,
                flags: Vec::new(),
                raw: b"Message-ID: <root2@example.com>\nSubject: [PATCH] complete\n\n".to_vec(),
            },
        ];
        let selection = select_initial_inbox_messages("list", messages);
        assert_eq!(selection.selected_uids, vec![1, 2]);
    }

    #[test]
    fn following_keeps_own_reply_mail() {
        let parsed = mail_parser::parse_headers(
            b"Message-ID: <reply@example.com>\nSubject: Re: [PATCH 1/1] demo\nFrom: Me <me@example.com>\nTo: list@example.com\n\nreply\n",
            "fallback@example.com".to_string(),
        );
        assert!(is_following_match(&parsed, "me@example.com"));
    }

    #[test]
    fn sent_mail_sync_keeps_only_own_patch_messages() {
        let own_patch = mail_parser::parse_headers(
            b"Message-ID: <patch@example.com>\nSubject: Re: [PATCH 1/1] demo\nFrom: me@example.com\n\nreply\n",
            "patch-fallback@example.com".to_string(),
        );
        let own_regular = mail_parser::parse_headers(
            b"Message-ID: <note@example.com>\nSubject: status update\nFrom: me@example.com\n\nnote\n",
            "note-fallback@example.com".to_string(),
        );
        let someone_else_patch = mail_parser::parse_headers(
            b"Message-ID: <other@example.com>\nSubject: [PATCH 1/1] demo\nFrom: other@example.com\n\npatch\n",
            "other-fallback@example.com".to_string(),
        );

        assert!(is_following_sent_match(&own_patch, "me@example.com"));
        assert!(!is_following_sent_match(&own_regular, "me@example.com"));
        assert!(!is_following_sent_match(
            &someone_else_patch,
            "me@example.com"
        ));
    }

    #[test]
    fn lore_sync_completes_reply_only_patch_series_from_thread_endpoint() {
        let reply_raw = b"Message-ID: <reply@example.com>\nSubject: Re: [PATCH v3 2/2] demo\nIn-Reply-To: <cover@example.com>\nReferences: <cover@example.com>\n\nreply\n".to_vec();
        let thread = vec![
            RemoteMail {
                uid: 0,
                modseq: None,
                flags: Vec::new(),
                raw: b"Message-ID: <cover@example.com>\nSubject: [PATCH v3 0/2] demo\n\ncover\n".to_vec(),
            },
            RemoteMail {
                uid: 0,
                modseq: None,
                flags: Vec::new(),
                raw: b"Message-ID: <patch-1@example.com>\nSubject: [PATCH v3 1/2] demo\nIn-Reply-To: <cover@example.com>\nReferences: <cover@example.com>\n\npatch 1\n".to_vec(),
            },
            RemoteMail {
                uid: 0,
                modseq: None,
                flags: Vec::new(),
                raw: b"Message-ID: <patch-2@example.com>\nSubject: [PATCH v3 2/2] demo\nIn-Reply-To: <cover@example.com>\nReferences: <cover@example.com>\n\npatch 2\n".to_vec(),
            },
            RemoteMail {
                uid: 0,
                modseq: None,
                flags: Vec::new(),
                raw: reply_raw.clone(),
            },
        ];
        let parsed = mail_parser::parse_headers(&reply_raw, "fallback@example.com".to_string());
        let mut envelopes = vec![RemoteMailEnvelope {
            remote: RemoteMail {
                uid: 0,
                modseq: None,
                flags: Vec::new(),
                raw: reply_raw,
            },
            parsed,
        }];
        let mut client = ThreadFixtureClient {
            thread,
            requested_ids: Vec::new(),
        };

        complete_incomplete_patch_series(
            &mut client,
            "io-uring",
            &mut envelopes,
            &SyncSource::Lore {
                base_url: "https://lore.test".to_string(),
            },
        )
        .expect("complete lore patch thread");

        assert_eq!(client.requested_ids, vec!["reply@example.com"]);
        assert_eq!(envelopes.len(), 4);
        assert!(
            envelopes
                .iter()
                .any(|envelope| envelope.parsed.message_id == "cover@example.com")
        );
    }
}
