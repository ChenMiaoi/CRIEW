//! Mail source adapters behind a common sync trait.
//!
//! Fixture data, lore.kernel.org, and real IMAP all implement the same
//! high-level contract so sync orchestration can share checkpoint logic and
//! storage invariants regardless of where mail bytes came from.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, NaiveDateTime, Utc};
use flate2::read::GzDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use webpki_roots::TLS_SERVER_ROOTS;

use crate::infra::config::{ImapConfig, ImapEncryption};
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::mail_parser::parse_headers;

const LORE_BASE_URL: &str = "https://lore.kernel.org";
const GNU_ARCHIVE_MBOX_BASE_URL: &str = "https://lists.gnu.org/archive/mbox";
const LORE_HTTP_TIMEOUT_SECS: u64 = 20;
const REMOTE_IMAP_TIMEOUT_SECS: u64 = 20;
const HTTP_PROXY_RESPONSE_MAX_BYTES: usize = 8 * 1024;
const IMAP_FETCH_BATCH_SIZE: usize = 100;
const GNU_ARCHIVE_INITIAL_MONTH_LIMIT: usize = 2;
const GNU_ARCHIVE_UID_STRIDE: u32 = 1_000_000;
const MAX_THREAD_DISCOVERY_IDS: usize = 512;

#[cfg(test)]
trait TestTransportIo: Read + Write + Send {}

#[cfg(test)]
impl<T> TestTransportIo for T where T: Read + Write + Send {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ImapErrorKind {
    Connection,
    Authentication,
    MailboxSelection,
    Protocol,
}

#[derive(Debug, Clone)]
pub struct MailboxSnapshot {
    pub uidvalidity: u64,
    pub highest_uid: u32,
    pub highest_modseq: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RemoteMail {
    pub uid: u32,
    pub modseq: Option<u64>,
    pub flags: Vec<String>,
    pub raw: Vec<u8>,
}

pub trait ImapClient {
    fn connect(&mut self) -> Result<()>;
    fn select_mailbox(&mut self, mailbox: &str) -> Result<MailboxSnapshot>;
    fn fetch_incremental(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>>;

    fn fetch_header_candidates(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        self.fetch_incremental(mailbox, after_uid, since_modseq)
    }

    fn fetch_following_incremental(
        &mut self,
        mailbox: &str,
        _self_email: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        self.fetch_incremental(mailbox, after_uid, since_modseq)
    }

    fn fetch_full_uids(&mut self, mailbox: &str, uids: &[u32]) -> Result<Vec<RemoteMail>> {
        let wanted: HashSet<u32> = uids.iter().copied().collect();
        let mut mails = self.fetch_incremental(mailbox, 0, None)?;
        mails.retain(|mail| wanted.contains(&mail.uid));
        Ok(mails)
    }

    /// Fetch all locally visible messages belonging to the requested thread.
    ///
    /// Source adapters with a native thread endpoint or server-side search
    /// should override this method. The fallback is intentionally bounded by
    /// the source's normal incremental view so fixture and archive adapters
    /// remain useful without duplicating their storage logic.
    fn fetch_thread(&mut self, mailbox: &str, message_id: &str) -> Result<Vec<RemoteMail>> {
        let messages = self.fetch_incremental(mailbox, 0, None)?;
        let messages = retain_thread_messages(messages, message_id);
        if messages.is_empty() {
            return Err(imap_error(
                ImapErrorKind::Protocol,
                format!("thread containing Message-ID '{message_id}' was not found"),
            ));
        }
        Ok(messages)
    }
}

#[derive(Debug, Clone)]
pub struct FixtureImapClient {
    root_dir: PathBuf,
    default_uidvalidity: u64,
    connected: bool,
}

impl FixtureImapClient {
    pub fn new(root_dir: PathBuf, default_uidvalidity: u64) -> Self {
        Self {
            root_dir,
            default_uidvalidity,
            connected: false,
        }
    }

    fn mailbox_dir(&self, mailbox: &str) -> PathBuf {
        let mailbox_candidate = self.root_dir.join(mailbox);
        if mailbox_candidate.is_dir() {
            mailbox_candidate
        } else {
            self.root_dir.clone()
        }
    }

    fn ensure_connected(&self) -> Result<()> {
        if self.connected {
            return Ok(());
        }

        Err(imap_error(
            ImapErrorKind::Connection,
            "client is not connected",
        ))
    }

    fn read_uidvalidity(&self, mailbox: &str) -> Result<u64> {
        let path = self.mailbox_dir(mailbox).join(".uidvalidity");
        if !path.exists() {
            return Ok(self.default_uidvalidity);
        }

        let content = fs::read_to_string(&path).map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to read UIDVALIDITY from {}", path.display()),
                error,
            )
        })?;

        let value = content.trim();
        if value.is_empty() {
            return Ok(self.default_uidvalidity);
        }

        value.parse::<u64>().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!(
                    "invalid UIDVALIDITY value '{}' in {}",
                    value,
                    path.display()
                ),
                error,
            )
        })
    }

    fn scan_entries(&self, mailbox: &str) -> Result<Vec<FixtureEntry>> {
        let dir = self.mailbox_dir(mailbox);
        if !dir.exists() {
            return Err(imap_error(
                ImapErrorKind::MailboxSelection,
                format!("mailbox directory {} not found", dir.display()),
            ));
        }

        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to read mailbox directory {}", dir.display()),
                    error,
                )
            })?
            .filter_map(|entry| entry.ok().map(|item| item.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("eml"))
            })
            .collect();

        files.sort_by(|left, right| {
            left.file_name()
                .and_then(|name| name.to_str())
                .cmp(&right.file_name().and_then(|name| name.to_str()))
        });

        let mut used_uids = HashSet::new();
        let mut entries = Vec::with_capacity(files.len());

        for (index, path) in files.into_iter().enumerate() {
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();

            let mut uid = leading_uid(&file_name).unwrap_or(index as u32 + 1);
            while !used_uids.insert(uid) {
                uid = uid.saturating_add(1);
            }

            let metadata = fs::metadata(&path).map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to read metadata for {}", path.display()),
                    error,
                )
            })?;

            let modseq = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());

            entries.push(FixtureEntry { path, uid, modseq });
        }

        entries.sort_by_key(|entry| entry.uid);
        Ok(entries)
    }
}

impl ImapClient for FixtureImapClient {
    fn connect(&mut self) -> Result<()> {
        if !self.root_dir.exists() {
            return Err(imap_error(
                ImapErrorKind::Connection,
                format!("fixture root {} does not exist", self.root_dir.display()),
            ));
        }

        if !self.root_dir.is_dir() {
            return Err(imap_error(
                ImapErrorKind::Connection,
                format!(
                    "fixture root {} is not a directory",
                    self.root_dir.display()
                ),
            ));
        }

        self.connected = true;
        Ok(())
    }

    fn select_mailbox(&mut self, mailbox: &str) -> Result<MailboxSnapshot> {
        self.ensure_connected()?;

        let uidvalidity = self.read_uidvalidity(mailbox)?;
        let entries = self.scan_entries(mailbox)?;

        let highest_uid = entries.iter().map(|entry| entry.uid).max().unwrap_or(0);
        let highest_modseq = entries.iter().filter_map(|entry| entry.modseq).max();

        Ok(MailboxSnapshot {
            uidvalidity,
            highest_uid,
            highest_modseq,
        })
    }

    fn fetch_incremental(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        self.ensure_connected()?;

        let entries = self.scan_entries(mailbox)?;
        let mut fetched = Vec::new();

        for entry in entries {
            let fetch_by_uid = entry.uid > after_uid;
            let fetch_by_modseq = since_modseq
                .zip(entry.modseq)
                .is_some_and(|(checkpoint, current)| current > checkpoint);

            if !fetch_by_uid && !fetch_by_modseq {
                continue;
            }

            let raw = fs::read(&entry.path).map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to read fixture mail {}", entry.path.display()),
                    error,
                )
            })?;

            let flags = parse_flags(&raw);
            fetched.push(RemoteMail {
                uid: entry.uid,
                modseq: entry.modseq,
                flags,
                raw,
            });
        }

        fetched.sort_by_key(|mail| mail.uid);
        Ok(fetched)
    }
}

#[derive(Debug, Clone)]
pub struct LoreImapClient {
    base_url: String,
    connected: bool,
    client: reqwest::blocking::Client,
}

#[derive(Debug, Clone)]
struct LoreFeedEntry {
    message_url: String,
    modseq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GnuArchiveMonthEntry {
    month_key: String,
    modseq: u64,
}

impl LoreImapClient {
    pub fn new(base_url: Option<&str>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(LORE_HTTP_TIMEOUT_SECS))
            .user_agent(format!("criew/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    "failed to initialize lore HTTP client",
                    error,
                )
            })?;

        Ok(Self {
            base_url: base_url
                .unwrap_or(LORE_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            connected: false,
            client,
        })
    }

    fn ensure_connected(&self) -> Result<()> {
        if self.connected {
            return Ok(());
        }

        Err(imap_error(
            ImapErrorKind::Connection,
            "client is not connected",
        ))
    }

    fn feed_url(&self, mailbox: &str) -> String {
        let mailbox = mailbox.trim_matches('/');
        format!("{}/{}/new.atom", self.base_url, mailbox)
    }

    fn fetch_feed_entries(&self, mailbox: &str) -> Result<Vec<LoreFeedEntry>> {
        let url = self.feed_url(mailbox);
        let response = self.client.get(&url).send().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to fetch lore feed {url}"),
                error,
            )
        })?;

        let status_code = response.status().as_u16();
        let body = response.text().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to read lore feed body {url}"),
                error,
            )
        })?;

        parse_lore_feed_response(&url, status_code, &body)
    }

    fn fetch_raw_mail(&self, message_url: &str) -> Result<Vec<u8>> {
        fetch_lore_raw_with(message_url, |raw_url| {
            let response = self.client.get(raw_url).send().map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to fetch lore raw message {raw_url}"),
                    error,
                )
            })?;
            let status_code = response.status().as_u16();
            if !(200..300).contains(&status_code) {
                return Ok((status_code, Vec::new()));
            }

            let bytes = response.bytes().map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to read lore raw message body {raw_url}"),
                    error,
                )
            })?;
            Ok((status_code, bytes.to_vec()))
        })
    }

    fn thread_url_candidates(&self, mailbox: &str, message_id: &str) -> Vec<String> {
        let message_id = normalize_requested_message_id(message_id);
        let mailbox = mailbox.trim_matches('/');
        let base_url = self.base_url.trim_end_matches('/');
        let message_id = encode_lore_path_segment(&message_id);
        let mailbox = encode_lore_path_segment(mailbox);

        [
            format!("{base_url}/{mailbox}/{message_id}/t.mbox.gz"),
            format!("{base_url}/r/{message_id}/t.mbox.gz"),
        ]
        .into_iter()
        .filter(|candidate| reqwest::Url::parse(candidate).is_ok())
        .collect()
    }

    fn fetch_thread_mbox(&self, mailbox: &str, message_id: &str) -> Result<Vec<u8>> {
        let mut last_error: Option<CriewError> = None;
        for url in self.thread_url_candidates(mailbox, message_id) {
            let response = match self.client.get(&url).send() {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(CriewError::with_source(
                        ErrorCode::Imap,
                        format!("failed to fetch lore thread {url}"),
                        error,
                    ));
                    continue;
                }
            };
            let status_code = response.status().as_u16();
            let bytes = match response.bytes() {
                Ok(bytes) => bytes.to_vec(),
                Err(error) => {
                    last_error = Some(CriewError::with_source(
                        ErrorCode::Imap,
                        format!("failed to read lore thread body {url}"),
                        error,
                    ));
                    continue;
                }
            };
            if !(200..300).contains(&status_code) {
                last_error = Some(imap_error(
                    ImapErrorKind::Protocol,
                    format!("failed to fetch lore thread {url}: HTTP {status_code}"),
                ));
                continue;
            }

            return decode_gzip_payload(&bytes, &url);
        }

        Err(last_error.unwrap_or_else(|| {
            imap_error(
                ImapErrorKind::Protocol,
                format!("failed to resolve lore thread for Message-ID '{message_id}'"),
            )
        }))
    }
}

fn parse_lore_feed_response(url: &str, status_code: u16, body: &str) -> Result<Vec<LoreFeedEntry>> {
    if !(200..300).contains(&status_code) {
        return Err(imap_error(
            ImapErrorKind::MailboxSelection,
            format!("failed to fetch lore feed {url}: HTTP {status_code}"),
        ));
    }

    parse_lore_atom_entries(body)
}

fn fetch_lore_raw_with<F>(message_url: &str, mut fetch_response: F) -> Result<Vec<u8>>
where
    F: FnMut(&str) -> Result<(u16, Vec<u8>)>,
{
    let mut last_error: Option<CriewError> = None;

    for raw_url in lore_raw_url_candidates(message_url) {
        let (status_code, bytes) = match fetch_response(&raw_url) {
            Ok(response) => response,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };

        if !(200..300).contains(&status_code) {
            last_error = Some(imap_error(
                ImapErrorKind::Protocol,
                format!("failed to fetch lore raw message {raw_url}: HTTP {status_code}"),
            ));
            continue;
        }

        if !bytes.is_empty() {
            return Ok(bytes);
        }

        last_error = Some(imap_error(
            ImapErrorKind::Protocol,
            format!("lore raw message is empty: {raw_url}"),
        ));
    }

    Err(last_error.unwrap_or_else(|| {
        imap_error(
            ImapErrorKind::Protocol,
            format!("failed to resolve raw message URL for {message_url}"),
        )
    }))
}

fn build_lore_incremental_mails<F>(
    entries: Vec<LoreFeedEntry>,
    since_modseq: Option<u64>,
    mut fetch_raw_mail: F,
) -> Result<Vec<RemoteMail>>
where
    F: FnMut(&str) -> Result<Vec<u8>>,
{
    let mut fetched = Vec::new();
    for entry in entries {
        if since_modseq.is_some_and(|checkpoint| entry.modseq <= checkpoint) {
            continue;
        }

        let raw = fetch_raw_mail(&entry.message_url)?;
        fetched.push(RemoteMail {
            uid: 0,
            modseq: Some(entry.modseq),
            flags: Vec::new(),
            raw,
        });
    }

    fetched.sort_by_key(|mail| mail.modseq.unwrap_or(0));
    Ok(fetched)
}

impl ImapClient for LoreImapClient {
    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }

    fn select_mailbox(&mut self, mailbox: &str) -> Result<MailboxSnapshot> {
        self.ensure_connected()?;
        let entries = self.fetch_feed_entries(mailbox)?;
        let highest_modseq = entries.iter().map(|entry| entry.modseq).max();

        Ok(MailboxSnapshot {
            uidvalidity: 1,
            highest_uid: 0,
            highest_modseq,
        })
    }

    fn fetch_incremental(
        &mut self,
        mailbox: &str,
        _after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        self.ensure_connected()?;
        let entries = self.fetch_feed_entries(mailbox)?;
        build_lore_incremental_mails(entries, since_modseq, |message_url| {
            self.fetch_raw_mail(message_url)
        })
    }

    fn fetch_thread(&mut self, mailbox: &str, message_id: &str) -> Result<Vec<RemoteMail>> {
        self.ensure_connected()?;
        let raw_mbox = self.fetch_thread_mbox(mailbox, message_id)?;
        let messages = parse_gnu_archive_mbox_messages(&raw_mbox);
        let mut remote = Vec::with_capacity(messages.len());
        for raw in messages {
            remote.push(RemoteMail {
                uid: 0,
                modseq: None,
                flags: Vec::new(),
                raw,
            });
        }

        let filtered = retain_thread_messages(remote, message_id);
        if filtered.is_empty() {
            return Err(imap_error(
                ImapErrorKind::Protocol,
                format!("lore thread did not contain Message-ID '{message_id}'"),
            ));
        }
        Ok(filtered)
    }
}

#[derive(Debug, Clone)]
pub struct GnuArchiveClient {
    base_url: String,
    connected: bool,
    client: reqwest::blocking::Client,
}

impl GnuArchiveClient {
    pub fn new(base_url: Option<&str>) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(LORE_HTTP_TIMEOUT_SECS))
            .user_agent(format!("criew/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    "failed to initialize GNU archive HTTP client",
                    error,
                )
            })?;

        Ok(Self {
            base_url: base_url
                .unwrap_or(GNU_ARCHIVE_MBOX_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            connected: false,
            client,
        })
    }

    fn ensure_connected(&self) -> Result<()> {
        if self.connected {
            return Ok(());
        }

        Err(imap_error(
            ImapErrorKind::Connection,
            "client is not connected",
        ))
    }

    fn index_url(&self, mailbox: &str) -> String {
        let mailbox = mailbox.trim_matches('/');
        format!("{}/{mailbox}/", self.base_url)
    }

    fn month_url(&self, mailbox: &str, month_key: &str) -> String {
        let mailbox = mailbox.trim_matches('/');
        format!("{}/{mailbox}/{month_key}", self.base_url)
    }

    fn fetch_month_entries(&self, mailbox: &str) -> Result<Vec<GnuArchiveMonthEntry>> {
        let url = self.index_url(mailbox);
        let response = self.client.get(&url).send().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to fetch GNU archive index {url}"),
                error,
            )
        })?;

        let status_code = response.status().as_u16();
        let body = response.text().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to read GNU archive index body {url}"),
                error,
            )
        })?;

        parse_gnu_archive_index_response(&url, status_code, &body)
    }

    fn fetch_month_mbox(&self, mailbox: &str, month_key: &str) -> Result<Vec<u8>> {
        let url = self.month_url(mailbox, month_key);
        let response = self.client.get(&url).send().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to fetch GNU archive mbox {url}"),
                error,
            )
        })?;

        let status_code = response.status().as_u16();
        validate_gnu_archive_mbox_response(&url, status_code)?;

        let bytes = response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to read GNU archive mbox body {url}"),
                    error,
                )
            })?;

        Ok(bytes)
    }
}

fn parse_gnu_archive_index_response(
    url: &str,
    status_code: u16,
    body: &str,
) -> Result<Vec<GnuArchiveMonthEntry>> {
    if !(200..300).contains(&status_code) {
        return Err(imap_error(
            ImapErrorKind::MailboxSelection,
            format!("failed to fetch GNU archive index {url}: HTTP {status_code}"),
        ));
    }

    parse_gnu_archive_month_entries(body)
}

fn validate_gnu_archive_mbox_response(url: &str, status_code: u16) -> Result<()> {
    if !(200..300).contains(&status_code) {
        return Err(imap_error(
            ImapErrorKind::Protocol,
            format!("failed to fetch GNU archive mbox {url}: HTTP {status_code}"),
        ));
    }

    Ok(())
}

fn build_gnu_archive_incremental_mails<F>(
    months: &[GnuArchiveMonthEntry],
    since_modseq: Option<u64>,
    mut fetch_month_mbox: F,
) -> Result<Vec<RemoteMail>>
where
    F: FnMut(&str) -> Result<Vec<u8>>,
{
    let selected_months = select_gnu_archive_months(months, since_modseq);

    let mut fetched = Vec::new();
    for month in selected_months {
        let raw_mbox = fetch_month_mbox(&month.month_key)?;
        for (index, raw) in parse_gnu_archive_mbox_messages(&raw_mbox)
            .into_iter()
            .enumerate()
        {
            fetched.push(RemoteMail {
                uid: gnu_archive_message_uid(&month.month_key, index),
                modseq: Some(month.modseq),
                flags: Vec::new(),
                raw,
            });
        }
    }

    fetched.sort_by(|left, right| {
        left.modseq
            .cmp(&right.modseq)
            .then_with(|| left.uid.cmp(&right.uid))
    });
    Ok(fetched)
}

impl ImapClient for GnuArchiveClient {
    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }

    fn select_mailbox(&mut self, mailbox: &str) -> Result<MailboxSnapshot> {
        self.ensure_connected()?;
        let entries = self.fetch_month_entries(mailbox)?;
        let highest_modseq = entries.iter().map(|entry| entry.modseq).max();

        Ok(MailboxSnapshot {
            uidvalidity: 1,
            highest_uid: 0,
            highest_modseq,
        })
    }

    fn fetch_incremental(
        &mut self,
        mailbox: &str,
        _after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        self.ensure_connected()?;
        let months = self.fetch_month_entries(mailbox)?;
        build_gnu_archive_incremental_mails(&months, since_modseq, |month_key| {
            self.fetch_month_mbox(mailbox, month_key)
        })
    }
}

pub struct RemoteImapClient {
    config: ImapConfig,
    session: Option<ImapSession>,
}

impl RemoteImapClient {
    pub fn new(config: ImapConfig) -> Result<Self> {
        let missing = config.missing_required_fields();
        if !missing.is_empty() {
            return Err(imap_error(
                ImapErrorKind::Connection,
                format!("incomplete IMAP config: missing {}", missing.join(", ")),
            ));
        }

        Ok(Self {
            config,
            session: None,
        })
    }

    fn session_mut(&mut self) -> Result<&mut ImapSession> {
        self.session.as_mut().ok_or_else(|| {
            imap_error(
                ImapErrorKind::Connection,
                "remote IMAP session is not connected",
            )
        })
    }
}

impl ImapClient for RemoteImapClient {
    fn connect(&mut self) -> Result<()> {
        let session = ImapSession::connect(&self.config)?;
        self.session = Some(session);
        Ok(())
    }

    fn select_mailbox(&mut self, mailbox: &str) -> Result<MailboxSnapshot> {
        self.session_mut()?.select_mailbox(mailbox)
    }

    fn fetch_incremental(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        let session = self.session_mut()?;
        let snapshot = session.select_mailbox(mailbox)?;
        let uids = collect_incremental_uids(session, snapshot, after_uid, since_modseq)?;
        session.fetch_uids(&uids, "BODY.PEEK[]")
    }

    fn fetch_header_candidates(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        let session = self.session_mut()?;
        let snapshot = session.select_mailbox(mailbox)?;
        let uids = collect_incremental_uids(session, snapshot, after_uid, since_modseq)?;
        session.fetch_uids(
            &uids,
            "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT FROM TO CC DATE IN-REPLY-TO REFERENCES LIST-ID)]",
        )
    }

    fn fetch_following_incremental(
        &mut self,
        mailbox: &str,
        self_email: &str,
        after_uid: u32,
        since_modseq: Option<u64>,
    ) -> Result<Vec<RemoteMail>> {
        let session = self.session_mut()?;
        let snapshot = session.select_mailbox(mailbox)?;
        let incremental_uids =
            collect_incremental_uids(session, snapshot, after_uid, since_modseq)?;
        if incremental_uids.is_empty() {
            return Ok(Vec::new());
        }

        let incremental_set: HashSet<u32> = incremental_uids.into_iter().collect();
        let mut matching_uids = BTreeSet::new();
        for header in ["TO", "CC", "FROM"] {
            for uid in session.search_header_value(header, self_email)? {
                if incremental_set.contains(&uid) {
                    matching_uids.insert(uid);
                }
            }
        }

        let matching_uids: Vec<u32> = matching_uids.into_iter().collect();
        session.fetch_uids(&matching_uids, "BODY.PEEK[]")
    }

    fn fetch_full_uids(&mut self, mailbox: &str, uids: &[u32]) -> Result<Vec<RemoteMail>> {
        let session = self.session_mut()?;
        let _ = session.select_mailbox(mailbox)?;
        session.fetch_uids(uids, "BODY.PEEK[]")
    }

    fn fetch_thread(&mut self, mailbox: &str, message_id: &str) -> Result<Vec<RemoteMail>> {
        let session = self.session_mut()?;
        let _ = session.select_mailbox(mailbox)?;

        let mut pending_ids = vec![normalize_requested_message_id(message_id)];
        let mut seen_ids = HashSet::new();
        let mut discovered_uids = BTreeSet::new();
        let mut fetched_by_uid = std::collections::HashMap::new();

        while let Some(current_id) = pending_ids.pop() {
            if seen_ids.len() >= MAX_THREAD_DISCOVERY_IDS {
                tracing::warn!(
                    mailbox = %mailbox,
                    message_id = %message_id,
                    limit = MAX_THREAD_DISCOVERY_IDS,
                    "stopping IMAP thread discovery at the reference limit"
                );
                break;
            }
            if !seen_ids.insert(current_id.clone()) {
                continue;
            }

            let mut matching_uids = session.search_header("MESSAGE-ID", &current_id)?;
            matching_uids.extend(session.search_header("IN-REPLY-TO", &current_id)?);
            let new_uids: Vec<u32> = matching_uids
                .into_iter()
                .filter(|uid| discovered_uids.insert(*uid))
                .collect();
            if new_uids.is_empty() {
                continue;
            }

            for mail in session.fetch_uids(&new_uids, "BODY.PEEK[]")? {
                let fallback = format!("synthetic-imap-{}@local", mail.uid);
                let parsed = parse_headers(&mail.raw, fallback);
                pending_ids.extend(parsed.references);
                if let Some(reply_to) = parsed.in_reply_to {
                    pending_ids.push(reply_to);
                }
                fetched_by_uid.insert(mail.uid, mail);
            }
        }

        if fetched_by_uid.is_empty() {
            return Err(imap_error(
                ImapErrorKind::Protocol,
                format!("thread containing Message-ID '{message_id}' was not found"),
            ));
        }

        let mut fetched: Vec<RemoteMail> = fetched_by_uid.into_values().collect();
        fetched.sort_by_key(|mail| mail.uid);
        Ok(fetched)
    }
}

fn collect_incremental_uids(
    session: &mut ImapSession,
    snapshot: MailboxSnapshot,
    after_uid: u32,
    since_modseq: Option<u64>,
) -> Result<Vec<u32>> {
    let mut uids = BTreeSet::new();

    if snapshot.highest_uid > after_uid {
        for uid in session.search_uid_range(after_uid.saturating_add(1))? {
            uids.insert(uid);
        }
    }

    if let Some(modseq) = since_modseq
        && snapshot.highest_modseq.is_some()
    {
        for uid in session.search_modseq(modseq)? {
            uids.insert(uid);
        }
    }

    Ok(uids.into_iter().collect())
}

enum ImapTransport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
    #[cfg(test)]
    Mock(Box<dyn TestTransportIo>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImapProxyScheme {
    Http,
    Socks5,
}

impl ImapProxyScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImapProxy {
    scheme: ImapProxyScheme,
    host: String,
    port: u16,
}

impl ImapProxy {
    fn redacted_url(&self) -> String {
        format!("{}://{}:{}", self.scheme.as_str(), self.host, self.port)
    }
}

impl Read for ImapTransport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
            #[cfg(test)]
            Self::Mock(stream) => stream.read(buf),
        }
    }
}

impl Write for ImapTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
            #[cfg(test)]
            Self::Mock(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
            #[cfg(test)]
            Self::Mock(stream) => stream.flush(),
        }
    }
}

struct ImapSession {
    transport: ImapTransport,
    read_buffer: Vec<u8>,
    next_tag: u32,
    capabilities: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GreetingKind {
    Ok,
    Preauth,
}

impl ImapSession {
    #[cfg(test)]
    fn with_mock_stream(stream: impl TestTransportIo + 'static) -> Self {
        Self {
            transport: ImapTransport::Mock(Box::new(stream)),
            read_buffer: Vec::new(),
            next_tag: 1,
            capabilities: HashSet::new(),
        }
    }

    fn connect(config: &ImapConfig) -> Result<Self> {
        let server = config.server.as_deref().ok_or_else(|| {
            imap_error(
                ImapErrorKind::Connection,
                "missing imap.server in runtime config",
            )
        })?;
        let port = config.server_port.ok_or_else(|| {
            imap_error(
                ImapErrorKind::Connection,
                "missing imap.serverport in runtime config",
            )
        })?;
        let encryption = config.encryption.ok_or_else(|| {
            imap_error(
                ImapErrorKind::Connection,
                "missing imap.encryption in runtime config",
            )
        })?;

        let tcp_stream = connect_tcp(config, server, port)?;
        let transport = match encryption {
            ImapEncryption::Tls => ImapTransport::Tls(Box::new(connect_tls(server, tcp_stream)?)),
            ImapEncryption::Starttls | ImapEncryption::None => ImapTransport::Plain(tcp_stream),
        };

        let mut session = Self {
            transport,
            read_buffer: Vec::new(),
            next_tag: 1,
            capabilities: HashSet::new(),
        };

        let greeting = session.read_greeting()?;
        if matches!(encryption, ImapEncryption::Starttls) {
            session.command_ok("STARTTLS", ImapErrorKind::Connection)?;
            session.transport = match session.transport {
                ImapTransport::Plain(stream) => {
                    ImapTransport::Tls(Box::new(connect_tls(server, stream)?))
                }
                ImapTransport::Tls(_) => {
                    return Err(imap_error(
                        ImapErrorKind::Connection,
                        "STARTTLS attempted on TLS transport",
                    ));
                }
                #[cfg(test)]
                ImapTransport::Mock(_) => {
                    return Err(imap_error(
                        ImapErrorKind::Connection,
                        "STARTTLS attempted on mock transport",
                    ));
                }
            };
            session.read_buffer.clear();
        }

        session.capabilities = session.fetch_capabilities()?;
        if !matches!(greeting, GreetingKind::Preauth) {
            session.login(config)?;
        }

        Ok(session)
    }

    fn read_greeting(&mut self) -> Result<GreetingKind> {
        let line = self.read_line_string()?;
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("* PREAUTH") {
            Ok(GreetingKind::Preauth)
        } else if upper.starts_with("* OK") {
            Ok(GreetingKind::Ok)
        } else if upper.starts_with("* BYE") {
            Err(imap_error(
                ImapErrorKind::Connection,
                format!("server closed connection during greeting: {line}"),
            ))
        } else {
            Err(imap_error(
                ImapErrorKind::Connection,
                format!("unexpected IMAP greeting: {line}"),
            ))
        }
    }

    fn login(&mut self, config: &ImapConfig) -> Result<()> {
        let user = config.login_user().ok_or_else(|| {
            imap_error(
                ImapErrorKind::Authentication,
                "missing imap.user in runtime config",
            )
        })?;
        let pass = config.pass.as_deref().ok_or_else(|| {
            imap_error(
                ImapErrorKind::Authentication,
                "missing imap.pass in runtime config",
            )
        })?;

        if config.encryption == Some(ImapEncryption::None)
            && self.capabilities.contains("LOGINDISABLED")
        {
            return Err(imap_error(
                ImapErrorKind::Authentication,
                "server disallows LOGIN over plaintext connections",
            ));
        }

        self.command_ok(
            &format!(
                "LOGIN {} {}",
                quote_imap_string(user),
                quote_imap_string(pass)
            ),
            ImapErrorKind::Authentication,
        )
    }

    fn fetch_capabilities(&mut self) -> Result<HashSet<String>> {
        let lines = self.command_ok_lines("CAPABILITY", ImapErrorKind::Protocol)?;
        let mut capabilities = HashSet::new();

        for line in lines {
            let upper = line.to_ascii_uppercase();
            if !upper.starts_with("* CAPABILITY ") {
                continue;
            }

            for token in line.split_whitespace().skip(2) {
                capabilities.insert(token.trim().to_ascii_uppercase());
            }
        }

        Ok(capabilities)
    }

    fn select_mailbox(&mut self, mailbox: &str) -> Result<MailboxSnapshot> {
        let lines = self.command_ok_lines(
            &format!("SELECT {}", quote_imap_string(mailbox)),
            ImapErrorKind::MailboxSelection,
        )?;

        let mut uidvalidity = None;
        let mut uidnext = None;
        let mut highest_modseq = None;

        for line in lines {
            if let Some(value) = parse_status_code_u64(&line, "UIDVALIDITY") {
                uidvalidity = Some(value);
            }
            if let Some(value) = parse_status_code_u64(&line, "UIDNEXT") {
                uidnext = Some(value as u32);
            }
            if let Some(value) = parse_status_code_u64(&line, "HIGHESTMODSEQ") {
                highest_modseq = Some(value);
            }
        }

        Ok(MailboxSnapshot {
            uidvalidity: uidvalidity.unwrap_or(1),
            highest_uid: uidnext.unwrap_or(1).saturating_sub(1),
            highest_modseq,
        })
    }

    fn search_uid_range(&mut self, first_uid: u32) -> Result<Vec<u32>> {
        self.search_uids(&format!("UID SEARCH UID {first_uid}:*"))
    }

    fn search_modseq(&mut self, modseq: u64) -> Result<Vec<u32>> {
        self.search_uids(&format!("UID SEARCH MODSEQ {modseq}"))
    }

    fn search_header(&mut self, header: &str, value: &str) -> Result<Vec<u32>> {
        self.search_uids(&format!(
            "UID SEARCH HEADER {header} {}",
            quote_imap_string(&format!("<{value}>"))
        ))
    }

    fn search_header_value(&mut self, header: &str, value: &str) -> Result<Vec<u32>> {
        self.search_uids(&format!(
            "UID SEARCH HEADER {header} {}",
            quote_imap_string(value)
        ))
    }

    fn search_uids(&mut self, command: &str) -> Result<Vec<u32>> {
        let lines = self.command_ok_lines(command, ImapErrorKind::Protocol)?;
        let mut uids = BTreeSet::new();

        for line in lines {
            let upper = line.to_ascii_uppercase();
            if !upper.starts_with("* SEARCH") {
                continue;
            }
            for token in line.split_whitespace().skip(2) {
                if let Ok(uid) = token.parse::<u32>() {
                    uids.insert(uid);
                }
            }
        }

        Ok(uids.into_iter().collect())
    }

    fn fetch_uids(&mut self, uids: &[u32], body_peek: &str) -> Result<Vec<RemoteMail>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let mut fetched = Vec::new();
        for chunk in uids.chunks(IMAP_FETCH_BATCH_SIZE) {
            fetched.extend(self.fetch_uid_chunk(chunk, body_peek)?);
        }
        fetched.sort_by_key(|mail| mail.uid);
        Ok(fetched)
    }

    fn fetch_uid_chunk(&mut self, uids: &[u32], body_peek: &str) -> Result<Vec<RemoteMail>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }

        let tag = self.next_tag();
        let uid_set = format_uid_sequence_set(uids);
        self.write_command(
            &tag,
            &format!("UID FETCH {uid_set} (UID FLAGS MODSEQ {body_peek})"),
        )?;

        let mut fetched = Vec::new();
        loop {
            let line = self.read_line_string()?;
            if line.starts_with('*') {
                if !line.to_ascii_uppercase().contains(" FETCH (") {
                    continue;
                }

                let fetched_uid = parse_fetch_uid(&line).ok_or_else(|| {
                    imap_error(
                        ImapErrorKind::Protocol,
                        format!("missing UID in FETCH response: {line}"),
                    )
                })?;
                let flags = parse_fetch_flags(&line);
                let modseq = parse_fetch_modseq(&line);
                let literal_len = parse_literal_len(&line).ok_or_else(|| {
                    imap_error(
                        ImapErrorKind::Protocol,
                        format!("missing literal size in FETCH response: {line}"),
                    )
                })?;
                let raw = self.read_exact_bytes(literal_len)?;
                self.consume_fetch_trailer()?;
                fetched.push(RemoteMail {
                    uid: fetched_uid,
                    modseq,
                    flags,
                    raw,
                });
                continue;
            }

            if line.starts_with(&tag) {
                ensure_tagged_ok(&line, ImapErrorKind::Protocol)?;
                return Ok(fetched);
            }
        }
    }

    fn consume_fetch_trailer(&mut self) -> Result<()> {
        loop {
            let line = self.read_line_string()?;
            if line.trim().is_empty() {
                continue;
            }
            if line.trim_end().ends_with(')') {
                return Ok(());
            }
            if line.starts_with('A') {
                return Err(imap_error(
                    ImapErrorKind::Protocol,
                    format!("truncated FETCH trailer before tagged completion: {line}"),
                ));
            }
        }
    }

    fn command_ok(&mut self, command: &str, kind: ImapErrorKind) -> Result<()> {
        self.command_ok_lines(command, kind).map(|_| ())
    }

    fn command_ok_lines(&mut self, command: &str, kind: ImapErrorKind) -> Result<Vec<String>> {
        let tag = self.next_tag();
        self.write_command(&tag, command)?;

        let mut lines = Vec::new();
        loop {
            let line = self.read_line_string()?;
            if line.starts_with(&tag) {
                ensure_tagged_ok(&line, kind)?;
                return Ok(lines);
            }
            lines.push(line);
        }
    }

    fn next_tag(&mut self) -> String {
        let tag = format!("A{:04}", self.next_tag);
        self.next_tag = self.next_tag.saturating_add(1);
        tag
    }

    fn write_command(&mut self, tag: &str, command: &str) -> Result<()> {
        let payload = format!("{tag} {command}\r\n");
        self.transport
            .write_all(payload.as_bytes())
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!("failed to write IMAP command '{command}'"),
                    error,
                )
            })?;
        self.transport.flush().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to flush IMAP command '{command}'"),
                error,
            )
        })
    }

    fn read_line_string(&mut self) -> Result<String> {
        let bytes = self.read_line_bytes()?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    fn read_line_bytes(&mut self) -> Result<Vec<u8>> {
        loop {
            if let Some(position) = self.read_buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.read_buffer.drain(..=position).collect();
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }

            let mut chunk = [0u8; 4096];
            let read = self.transport.read(&mut chunk).map_err(|error| {
                CriewError::with_source(ErrorCode::Imap, "failed to read from IMAP socket", error)
            })?;
            if read == 0 {
                return Err(imap_error(
                    ImapErrorKind::Connection,
                    "unexpected EOF while reading IMAP response",
                ));
            }
            self.read_buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn read_exact_bytes(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let drain = len.min(self.read_buffer.len());
        out.extend(self.read_buffer.drain(..drain));

        while out.len() < len {
            let remaining = len - out.len();
            let mut chunk = vec![0u8; remaining.min(4096)];
            let read = self.transport.read(&mut chunk).map_err(|error| {
                CriewError::with_source(ErrorCode::Imap, "failed to read IMAP literal", error)
            })?;
            if read == 0 {
                return Err(imap_error(
                    ImapErrorKind::Connection,
                    "unexpected EOF while reading IMAP literal",
                ));
            }
            out.extend_from_slice(&chunk[..read]);
        }

        Ok(out)
    }
}

fn format_uid_sequence_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }

    let mut ordered: Vec<u32> = uids.to_vec();
    ordered.sort_unstable();
    ordered.dedup();

    let mut parts = Vec::new();
    let mut start = ordered[0];
    let mut end = ordered[0];

    for uid in ordered.into_iter().skip(1) {
        if uid == end.saturating_add(1) {
            end = uid;
            continue;
        }
        parts.push(render_uid_range(start, end));
        start = uid;
        end = uid;
    }
    parts.push(render_uid_range(start, end));

    parts.join(",")
}

fn render_uid_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

fn parse_imap_proxy(proxy_url: &str) -> Result<ImapProxy> {
    let parsed = reqwest::Url::parse(proxy_url).map_err(|error| {
        CriewError::with_source(
            ErrorCode::ConfigParse,
            format!("invalid IMAP proxy URL '{proxy_url}'"),
            error,
        )
    })?;

    let scheme = match parsed.scheme() {
        "http" => ImapProxyScheme::Http,
        "socks5" | "socks5h" => ImapProxyScheme::Socks5,
        unsupported => {
            return Err(CriewError::new(
                ErrorCode::ConfigParse,
                format!(
                    "unsupported IMAP proxy scheme '{unsupported}'; use http://, socks5://, or socks5h://"
                ),
            ));
        }
    };

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CriewError::new(
            ErrorCode::ConfigParse,
            "IMAP proxy authentication is not supported yet; use an unauthenticated local proxy",
        ));
    }

    if parsed.query().is_some()
        || parsed.fragment().is_some()
        || !(parsed.path().is_empty() || parsed.path() == "/")
    {
        return Err(CriewError::new(
            ErrorCode::ConfigParse,
            format!("invalid IMAP proxy URL '{proxy_url}': remove path, query, and fragment"),
        ));
    }

    let host = parsed.host_str().ok_or_else(|| {
        CriewError::new(
            ErrorCode::ConfigParse,
            format!("invalid IMAP proxy URL '{proxy_url}': missing host"),
        )
    })?;
    let port = parsed.port().unwrap_or(match scheme {
        ImapProxyScheme::Http => 80,
        ImapProxyScheme::Socks5 => 1080,
    });

    Ok(ImapProxy {
        scheme,
        host: host.to_string(),
        port,
    })
}

fn configure_tcp_timeouts(stream: &TcpStream, label: &str) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(REMOTE_IMAP_TIMEOUT_SECS)))
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to configure IMAP read timeout for {label}"),
                error,
            )
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(REMOTE_IMAP_TIMEOUT_SECS)))
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to configure IMAP write timeout for {label}"),
                error,
            )
        })?;

    Ok(())
}

fn connect_direct_tcp(server: &str, port: u16) -> Result<TcpStream> {
    let stream = TcpStream::connect((server, port)).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!("failed to connect to IMAP server {server}:{port}"),
            error,
        )
    })?;
    configure_tcp_timeouts(&stream, &format!("IMAP server {server}:{port}"))?;

    Ok(stream)
}

fn read_http_proxy_response<S: Read>(
    stream: &mut S,
    proxy: &ImapProxy,
    target: &str,
) -> Result<String> {
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!(
                    "failed while reading IMAP proxy {} response for {target}",
                    proxy.redacted_url()
                ),
                error,
            )
        })?;
        if read == 0 {
            return Err(CriewError::new(
                ErrorCode::Imap,
                format!(
                    "IMAP proxy {} closed the connection before CONNECT to {target} completed",
                    proxy.redacted_url()
                ),
            ));
        }
        response.push(byte[0]);
        if response.len() > HTTP_PROXY_RESPONSE_MAX_BYTES {
            return Err(CriewError::new(
                ErrorCode::Imap,
                format!(
                    "IMAP proxy {} sent too much HTTP response data while tunneling {target}",
                    proxy.redacted_url()
                ),
            ));
        }
    }

    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn establish_http_connect_tunnel<S: Read + Write>(
    stream: &mut S,
    proxy: &ImapProxy,
    server: &str,
    port: u16,
) -> Result<()> {
    let target = format!("{server}:{port}");
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to send IMAP CONNECT request through proxy {} for {target}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;
    stream.flush().map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to flush IMAP CONNECT request through proxy {} for {target}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;

    let response = read_http_proxy_response(stream, proxy, &target)?;
    let status_line = response
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r')
        .to_string();
    let mut parts = status_line.split_whitespace();
    let protocol = parts.next().unwrap_or_default();
    let status_code = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or_default();
    if !protocol.starts_with("HTTP/") || !(200..300).contains(&status_code) {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP proxy {} rejected CONNECT to {target}: {status_line}",
                proxy.redacted_url()
            ),
        ));
    }

    Ok(())
}

fn read_socks5_reply_address<S: Read>(stream: &mut S, proxy: &ImapProxy) -> Result<()> {
    let mut atyp = [0u8; 1];
    stream.read_exact(&mut atyp).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to read SOCKS5 reply type from IMAP proxy {}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;
    let address_len = match atyp[0] {
        0x01 => 4usize,
        0x03 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    format!(
                        "failed to read SOCKS5 domain length from IMAP proxy {}",
                        proxy.redacted_url()
                    ),
                    error,
                )
            })?;
            length[0] as usize
        }
        0x04 => 16usize,
        value => {
            return Err(CriewError::new(
                ErrorCode::Imap,
                format!(
                    "IMAP proxy {} returned unsupported SOCKS5 address type 0x{value:02x}",
                    proxy.redacted_url()
                ),
            ));
        }
    };

    let mut discard = vec![0u8; address_len + 2];
    stream.read_exact(&mut discard).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to read SOCKS5 bind address from IMAP proxy {}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;
    Ok(())
}

fn socks5_reply_text(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown SOCKS5 error",
    }
}

fn establish_socks5_tunnel<S: Read + Write>(
    stream: &mut S,
    proxy: &ImapProxy,
    server: &str,
    port: u16,
) -> Result<()> {
    stream.write_all(&[0x05, 0x01, 0x00]).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to send SOCKS5 greeting to IMAP proxy {}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;

    let mut greeting_reply = [0u8; 2];
    stream.read_exact(&mut greeting_reply).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to read SOCKS5 greeting reply from IMAP proxy {}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;
    if greeting_reply[0] != 0x05 {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP proxy {} returned invalid SOCKS5 version 0x{:02x}",
                proxy.redacted_url(),
                greeting_reply[0]
            ),
        ));
    }
    if greeting_reply[1] != 0x00 {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP proxy {} does not allow unauthenticated SOCKS5 connections",
                proxy.redacted_url()
            ),
        ));
    }

    let host = server.as_bytes();
    if host.len() > u8::MAX as usize {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!("IMAP server name '{server}' is too long for SOCKS5 proxying"),
        ));
    }

    let mut request = Vec::with_capacity(4 + 1 + host.len() + 2);
    request.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, host.len() as u8]);
    request.extend_from_slice(host);
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to send SOCKS5 CONNECT request through IMAP proxy {}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;

    let mut reply = [0u8; 3];
    stream.read_exact(&mut reply).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to read SOCKS5 CONNECT status from IMAP proxy {}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;
    if reply[0] != 0x05 {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP proxy {} returned invalid SOCKS5 version 0x{:02x}",
                proxy.redacted_url(),
                reply[0]
            ),
        ));
    }
    if reply[1] != 0x00 {
        return Err(CriewError::new(
            ErrorCode::Imap,
            format!(
                "IMAP proxy {} failed to connect to {server}:{port}: {}",
                proxy.redacted_url(),
                socks5_reply_text(reply[1])
            ),
        ));
    }
    read_socks5_reply_address(stream, proxy)?;

    Ok(())
}

fn connect_tcp_via_proxy(proxy: &ImapProxy, server: &str, port: u16) -> Result<TcpStream> {
    let target = format!("{server}:{port}");
    let stream = TcpStream::connect((proxy.host.as_str(), proxy.port)).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!(
                "failed to connect to IMAP proxy {} for {target}",
                proxy.redacted_url()
            ),
            error,
        )
    })?;
    configure_tcp_timeouts(&stream, &format!("IMAP proxy {}", proxy.redacted_url()))?;

    let mut stream = stream;
    match proxy.scheme {
        ImapProxyScheme::Http => establish_http_connect_tunnel(&mut stream, proxy, server, port)?,
        ImapProxyScheme::Socks5 => establish_socks5_tunnel(&mut stream, proxy, server, port)?,
    }

    Ok(stream)
}

fn connect_tcp(config: &ImapConfig, server: &str, port: u16) -> Result<TcpStream> {
    if let Some(proxy_url) = config.proxy.as_deref() {
        let proxy = parse_imap_proxy(proxy_url)?;
        tracing::info!(
            op = "imap_connect",
            mode = "proxy",
            proxy = %proxy.redacted_url(),
            target = %format!("{server}:{port}")
        );
        connect_tcp_via_proxy(&proxy, server, port)
    } else {
        connect_direct_tcp(server, port)
    }
}

fn connect_tls(
    server: &str,
    stream: TcpStream,
) -> Result<StreamOwned<ClientConnection, TcpStream>> {
    let root_store = RootCertStore::from_iter(TLS_SERVER_ROOTS.iter().cloned());
    let client_config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|error| {
                CriewError::with_source(
                    ErrorCode::Imap,
                    "failed to configure TLS protocol versions for IMAP client",
                    error,
                )
            })?
            .with_root_certificates(root_store)
            .with_no_client_auth();
    let server_name = ServerName::try_from(server.to_string()).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!("invalid IMAP server name '{server}' for TLS"),
            error,
        )
    })?;
    let connection =
        ClientConnection::new(Arc::new(client_config), server_name).map_err(|error| {
            CriewError::with_source(
                ErrorCode::Imap,
                format!("failed to initialize TLS session for IMAP server '{server}'"),
                error,
            )
        })?;

    Ok(StreamOwned::new(connection, stream))
}

fn quote_imap_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn ensure_tagged_ok(line: &str, kind: ImapErrorKind) -> Result<()> {
    let mut parts = line.split_whitespace();
    let _tag = parts.next();
    match parts
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
        .as_str()
    {
        "OK" => Ok(()),
        _ => Err(imap_error(kind, line.to_string())),
    }
}

fn parse_status_code_u64(line: &str, key: &str) -> Option<u64> {
    let needle = format!("[{key} ");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find(']')?;
    rest[..end].trim().parse::<u64>().ok()
}

fn parse_fetch_uid(line: &str) -> Option<u32> {
    parse_numeric_token(line, "UID ").and_then(|value| value.parse::<u32>().ok())
}

fn parse_fetch_modseq(line: &str) -> Option<u64> {
    parse_numeric_token(line, "MODSEQ (").and_then(|value| value.parse::<u64>().ok())
}

fn parse_numeric_token<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let start = line.find(needle)? + needle.len();
    let rest = &line[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 { None } else { Some(&rest[..end]) }
}

fn parse_fetch_flags(line: &str) -> Vec<String> {
    let Some(start) = line.find("FLAGS (") else {
        return Vec::new();
    };
    let rest = &line[start + "FLAGS (".len()..];
    let Some(end) = rest.find(')') else {
        return Vec::new();
    };
    split_flags(&rest[..end])
}

fn parse_literal_len(line: &str) -> Option<usize> {
    let trimmed = line.trim_end();
    let suffix = trimmed.strip_suffix('}')?;
    let start = suffix.rfind('{')?;
    suffix[start + 1..].parse::<usize>().ok()
}

#[derive(Debug, Clone)]
struct FixtureEntry {
    path: PathBuf,
    uid: u32,
    modseq: Option<u64>,
}

fn parse_flags(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let mut current_header = String::new();
    let mut current_value = String::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }

        if line.starts_with(' ') || line.starts_with('\t') {
            if current_header.eq_ignore_ascii_case("x-flags") {
                if !current_value.is_empty() {
                    current_value.push(' ');
                }
                current_value.push_str(line.trim());
            }
            continue;
        }

        if current_header.eq_ignore_ascii_case("x-flags") {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            current_header = name.trim().to_string();
            current_value = value.trim().to_string();
        }
    }

    if current_header.eq_ignore_ascii_case("x-flags") {
        return split_flags(&current_value);
    }

    Vec::new()
}

fn split_flags(raw: &str) -> Vec<String> {
    raw.split(|ch: char| ch.is_whitespace() || ch == ',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn leading_uid(file_name: &str) -> Option<u32> {
    let prefix: String = file_name
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();

    if prefix.is_empty() {
        None
    } else {
        prefix.parse::<u32>().ok()
    }
}

fn parse_lore_atom_entries(xml: &str) -> Result<Vec<LoreFeedEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buffer = Vec::new();
    let mut entries = Vec::new();
    let mut seen_urls = HashSet::new();

    let mut in_entry = false;
    let mut current_tag: Option<Vec<u8>> = None;
    let mut current_link: Option<String> = None;
    let mut current_id: Option<String> = None;
    let mut current_modseq: Option<u64> = None;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => {
                let tag = event.name().as_ref().to_vec();
                if tag == b"entry" {
                    in_entry = true;
                    current_tag = None;
                    current_link = None;
                    current_id = None;
                    current_modseq = None;
                } else if in_entry && tag == b"link" {
                    if let Some(link) = parse_link_href(&event) {
                        current_link = Some(link);
                    }
                    current_tag = None;
                } else if in_entry {
                    current_tag = Some(tag);
                }
            }
            Ok(Event::Empty(event)) => {
                if in_entry
                    && event.name().as_ref() == b"link"
                    && let Some(link) = parse_link_href(&event)
                {
                    current_link = Some(link);
                }
            }
            Ok(Event::Text(event)) => {
                if !in_entry {
                    buffer.clear();
                    continue;
                }

                let text = event.unescape().map_err(|error| {
                    CriewError::with_source(
                        ErrorCode::Imap,
                        "failed to decode lore atom text",
                        error,
                    )
                })?;

                match current_tag.as_deref() {
                    Some(b"updated") | Some(b"published") => {
                        if let Some(modseq) = parse_atom_timestamp(text.as_ref()) {
                            current_modseq =
                                Some(current_modseq.map_or(modseq, |prev| prev.max(modseq)));
                        }
                    }
                    Some(b"id") if current_id.is_none() => {
                        current_id = Some(text.into_owned());
                    }
                    _ => {}
                }
            }
            Ok(Event::End(event)) => {
                let tag = event.name().as_ref().to_vec();
                if tag == b"entry" {
                    let message_url = current_link.take().or_else(|| current_id.take());
                    if let (Some(message_url), Some(modseq)) = (message_url, current_modseq)
                        && let Some(normalized) = normalize_lore_message_url(&message_url)
                        && seen_urls.insert(normalized.clone())
                    {
                        entries.push(LoreFeedEntry {
                            message_url: normalized,
                            modseq,
                        });
                    }

                    in_entry = false;
                    current_tag = None;
                    current_modseq = None;
                } else if in_entry {
                    current_tag = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(CriewError::with_source(
                    ErrorCode::Imap,
                    "failed to parse lore atom feed",
                    error,
                ));
            }
            _ => {}
        }

        buffer.clear();
    }

    entries.sort_by(|left, right| {
        left.modseq
            .cmp(&right.modseq)
            .then_with(|| left.message_url.cmp(&right.message_url))
    });

    Ok(entries)
}

fn parse_link_href(event: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    let mut rel: Option<String> = None;
    let mut href: Option<String> = None;

    for attr in event.attributes().flatten() {
        if attr.key.as_ref() == b"rel" {
            rel = Some(String::from_utf8_lossy(attr.value.as_ref()).to_string());
        }
        if attr.key.as_ref() == b"href" {
            href = Some(String::from_utf8_lossy(attr.value.as_ref()).to_string());
        }
    }

    let relation = rel.unwrap_or_else(|| "alternate".to_string());
    if relation == "alternate" || relation == "self" {
        href
    } else {
        None
    }
}

fn parse_atom_timestamp(value: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|datetime| datetime.timestamp().try_into().ok())
}

fn parse_gnu_archive_month_entries(html: &str) -> Result<Vec<GnuArchiveMonthEntry>> {
    let mut entries = Vec::new();
    let mut seen_months = HashSet::new();

    for line in html.lines() {
        let Some(anchor_start) = line.find("<a href=\"") else {
            continue;
        };
        let href_start = anchor_start + "<a href=\"".len();
        let Some(href_end) = line[href_start..].find('"') else {
            continue;
        };
        let href = &line[href_start..href_start + href_end];
        let Some((year, month)) = parse_year_month_key(href) else {
            continue;
        };

        let Some(anchor_end) = line.find("</a>") else {
            continue;
        };
        let mut parts = line[anchor_end + "</a>".len()..].split_whitespace();
        let (Some(date), Some(time)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(modseq) = parse_gnu_archive_listing_timestamp(date, time) else {
            continue;
        };

        let month_key = format!("{year:04}-{month:02}");
        if seen_months.insert(month_key.clone()) {
            entries.push(GnuArchiveMonthEntry { month_key, modseq });
        }
    }

    entries.sort_by(|left, right| {
        left.month_key
            .cmp(&right.month_key)
            .then_with(|| left.modseq.cmp(&right.modseq))
    });

    Ok(entries)
}

fn parse_year_month_key(value: &str) -> Option<(u32, u32)> {
    let trimmed = value.trim_matches('/');
    let (year, month) = trimmed.split_once('-')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }

    let year = year.parse::<u32>().ok()?;
    let month = month.parse::<u32>().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }

    Some((year, month))
}

fn parse_gnu_archive_listing_timestamp(date: &str, time: &str) -> Option<u64> {
    let naive = NaiveDateTime::parse_from_str(&format!("{date} {time}"), "%Y-%m-%d %H:%M").ok()?;
    DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
        .timestamp()
        .try_into()
        .ok()
}

fn select_gnu_archive_months(
    months: &[GnuArchiveMonthEntry],
    since_modseq: Option<u64>,
) -> Vec<GnuArchiveMonthEntry> {
    if months.is_empty() {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    if let Some(checkpoint) = since_modseq {
        for entry in months.iter().filter(|entry| entry.modseq > checkpoint) {
            if seen.insert(entry.month_key.clone()) {
                selected.push(entry.clone());
            }
        }

        if let Some(latest) = months.last()
            && seen.insert(latest.month_key.clone())
        {
            // GNU archive directory timestamps are only minute-resolution. Keep
            // polling the newest month even when the index timestamp has not
            // advanced yet so current-month mail is not missed.
            selected.push(latest.clone());
        }
    } else {
        for entry in months
            .iter()
            .rev()
            .take(GNU_ARCHIVE_INITIAL_MONTH_LIMIT)
            .rev()
        {
            if seen.insert(entry.month_key.clone()) {
                selected.push(entry.clone());
            }
        }
    }

    selected.sort_by(|left, right| left.month_key.cmp(&right.month_key));
    selected
}

fn parse_gnu_archive_mbox_messages(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut messages = Vec::new();
    let mut current = Vec::new();

    for line in raw.split_inclusive(|byte| *byte == b'\n') {
        let normalized = trim_ascii_line_ending(line);
        if normalized.starts_with(b"From ") {
            if !current.is_empty() {
                messages.push(current);
                current = Vec::new();
            }
            continue;
        }

        if normalized.starts_with(b">From ") {
            current.extend_from_slice(&line[1..]);
        } else {
            current.extend_from_slice(line);
        }
    }

    if !current.is_empty() {
        messages.push(current);
    }

    messages
}

fn trim_ascii_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn gnu_archive_message_uid(month_key: &str, index: usize) -> u32 {
    let Some((year, month)) = parse_year_month_key(month_key) else {
        return 0;
    };
    if year < 2000 || index + 1 >= GNU_ARCHIVE_UID_STRIDE as usize {
        return 0;
    }

    let month_ordinal = (year - 2000) * 12 + (month - 1);
    let uid = month_ordinal as u64 * GNU_ARCHIVE_UID_STRIDE as u64 + index as u64 + 1;
    u32::try_from(uid).unwrap_or(0)
}

fn normalize_lore_message_url(value: &str) -> Option<String> {
    if !value.contains("//") {
        return None;
    }

    let without_fragment = value.split('#').next().unwrap_or(value);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let trimmed = without_query.trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(format!("{trimmed}/"))
    }
}

fn normalize_requested_message_id(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

fn encode_lore_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

fn decode_gzip_payload(bytes: &[u8], url: &str) -> Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }

    let mut decoder = GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Imap,
            format!("failed to decompress lore thread {url}"),
            error,
        )
    })?;
    Ok(decoded)
}

fn lore_raw_url_candidates(message_url: &str) -> Vec<String> {
    let normalized =
        normalize_lore_message_url(message_url).unwrap_or_else(|| message_url.to_string());
    let base = normalized.trim_end_matches('/');

    let candidates = vec![
        format!("{base}/raw"),
        format!("{base}/raw/"),
        format!("{normalized}raw"),
    ];

    let mut uniq = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
        if seen.insert(candidate.clone()) {
            uniq.push(candidate);
        }
    }

    uniq
}

fn retain_thread_messages(messages: Vec<RemoteMail>, message_id: &str) -> Vec<RemoteMail> {
    let target = normalize_requested_message_id(message_id);
    if target.is_empty() {
        return Vec::new();
    }

    let parsed: Vec<_> = messages
        .iter()
        .enumerate()
        .map(|(index, mail)| {
            let fallback = format!("synthetic-thread-{index}@local");
            (index, parse_headers(&mail.raw, fallback))
        })
        .collect();
    let known_ids: HashSet<String> = parsed
        .iter()
        .map(|(_, headers)| headers.message_id.clone())
        .collect();
    if !known_ids.contains(&target) {
        return Vec::new();
    }

    let mut adjacency: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();
    for (_, headers) in &parsed {
        let mut references = headers.references.clone();
        if let Some(reply_to) = headers.in_reply_to.as_ref() {
            references.push(reply_to.clone());
        }
        for reference in references {
            if !known_ids.contains(&reference) {
                continue;
            }
            adjacency
                .entry(headers.message_id.clone())
                .or_default()
                .insert(reference.clone());
            adjacency
                .entry(reference)
                .or_default()
                .insert(headers.message_id.clone());
        }
    }

    let mut component = HashSet::from([target]);
    let mut queue: Vec<String> = component.iter().cloned().collect();
    while let Some(current) = queue.pop() {
        for neighbor in adjacency.get(&current).into_iter().flatten() {
            if component.insert(neighbor.clone()) {
                queue.push(neighbor.clone());
            }
        }
    }

    let mut selected: Vec<RemoteMail> = messages
        .into_iter()
        .zip(parsed)
        .filter_map(|(mail, (_, headers))| component.contains(&headers.message_id).then_some(mail))
        .collect();
    selected.sort_by_key(|mail| mail.uid);
    selected
}

fn imap_error(kind: ImapErrorKind, message: impl Into<String>) -> CriewError {
    CriewError::new(
        ErrorCode::Imap,
        format!("{}: {}", classify(kind), message.into()),
    )
}

fn classify(kind: ImapErrorKind) -> &'static str {
    match kind {
        ImapErrorKind::Connection => "connection",
        ImapErrorKind::Authentication => "authentication",
        ImapErrorKind::MailboxSelection => "mailbox",
        ImapErrorKind::Protocol => "protocol",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::infra::config::{ImapConfig, ImapEncryption};
    use crate::infra::mail_parser::parse_headers;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::{
        FixtureImapClient, GnuArchiveClient, GnuArchiveMonthEntry, GreetingKind, ImapClient,
        ImapErrorKind, ImapSession, LoreImapClient, MailboxSnapshot, RemoteImapClient, RemoteMail,
        build_gnu_archive_incremental_mails, build_lore_incremental_mails,
        collect_incremental_uids, decode_gzip_payload, ensure_tagged_ok,
        establish_http_connect_tunnel, establish_socks5_tunnel, fetch_lore_raw_with,
        format_uid_sequence_set, gnu_archive_message_uid, lore_raw_url_candidates,
        normalize_lore_message_url, parse_atom_timestamp, parse_fetch_flags, parse_fetch_modseq,
        parse_fetch_uid, parse_flags, parse_gnu_archive_index_response,
        parse_gnu_archive_listing_timestamp, parse_gnu_archive_mbox_messages,
        parse_gnu_archive_month_entries, parse_imap_proxy, parse_literal_len,
        parse_lore_atom_entries, parse_lore_feed_response, parse_status_code_u64,
        parse_year_month_key, quote_imap_string, read_http_proxy_response, retain_thread_messages,
        select_gnu_archive_months, validate_gnu_archive_mbox_response,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("criew-imap-{label}-{nonce}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[derive(Default)]
    struct MockStream {
        reads: Vec<u8>,
        read_offset: usize,
        writes: Vec<u8>,
    }

    impl MockStream {
        fn with_reads(reads: &[u8]) -> Self {
            Self {
                reads: reads.to_vec(),
                read_offset: 0,
                writes: Vec::new(),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.read_offset >= self.reads.len() {
                return Ok(0);
            }
            let available = self.reads.len() - self.read_offset;
            let count = available.min(buf.len());
            buf[..count].copy_from_slice(&self.reads[self.read_offset..self.read_offset + count]);
            self.read_offset += count;
            Ok(count)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct WriteFailsStream;

    impl Read for WriteFailsStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for WriteFailsStream {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "write failed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FlushFailsStream;

    impl Read for FlushFailsStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    impl Write for FlushFailsStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "flush failed",
            ))
        }
    }

    struct ReadFailsStream;

    impl Read for ReadFailsStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "read failed",
            ))
        }
    }

    impl Write for ReadFailsStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct StubHttpResponse {
        status_code: u16,
        content_type: &'static str,
        body: Vec<u8>,
    }

    impl StubHttpResponse {
        fn text(status_code: u16, body: impl Into<String>) -> Self {
            Self {
                status_code,
                content_type: "text/plain; charset=utf-8",
                body: body.into().into_bytes(),
            }
        }

        fn bytes(status_code: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status_code,
                content_type: "application/octet-stream",
                body: body.into(),
            }
        }
    }

    fn start_http_server<F>(
        expected_requests: usize,
        build_routes: F,
    ) -> (String, thread::JoinHandle<()>)
    where
        F: FnOnce(&str) -> HashMap<String, StubHttpResponse>,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind http listener");
        let base_url = format!("http://{}", listener.local_addr().expect("listener addr"));
        let routes = build_routes(&base_url);
        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept http request");
                let path = read_http_request_path(&mut stream);
                let response = routes
                    .get(&path)
                    .unwrap_or_else(|| panic!("unexpected HTTP path {path}"));
                let reason = match response.status_code {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Response",
                };
                write!(
                    stream,
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                    response.status_code,
                    reason,
                    response.body.len(),
                    response.content_type,
                )
                .expect("write HTTP headers");
                stream.write_all(&response.body).expect("write HTTP body");
                stream.flush().expect("flush HTTP response");
            }
        });

        (base_url, handle)
    }

    fn start_http_server_with_raw_responses<F>(
        expected_requests: usize,
        build_response: F,
    ) -> (String, thread::JoinHandle<()>)
    where
        F: Fn(&str) -> Vec<u8> + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind http listener");
        let base_url = format!("http://{}", listener.local_addr().expect("listener addr"));
        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept http request");
                let path = read_http_request_path(&mut stream);
                let response = build_response(&path);
                stream
                    .write_all(&response)
                    .expect("write raw HTTP response");
                stream.flush().expect("flush raw HTTP response");
            }
        });

        (base_url, handle)
    }

    fn read_http_request_path(stream: &mut impl Read) -> String {
        let mut request = Vec::new();
        let mut buf = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buf).expect("read HTTP request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
        }

        let request_line = String::from_utf8_lossy(&request);
        request_line
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string()
    }

    fn complete_imap_config(encryption: ImapEncryption) -> ImapConfig {
        ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: Some(993),
            encryption: Some(encryption),
            proxy: None,
        }
    }

    #[test]
    fn fixture_client_fetches_incremental_messages() {
        let root = temp_dir("incremental");
        fs::write(
            root.join("1-root.eml"),
            "Message-ID: <a@example.com>\nSubject: root\n\nbody\n",
        )
        .expect("write root");
        thread::sleep(Duration::from_millis(5));
        fs::write(
            root.join("2-reply.eml"),
            "Message-ID: <b@example.com>\nSubject: re\n\nbody\n",
        )
        .expect("write reply");

        let mut client = FixtureImapClient::new(root.clone(), 42);
        client.connect().expect("connect");

        let selected = client.select_mailbox("inbox").expect("select mailbox");
        assert_eq!(selected.uidvalidity, 42);
        assert_eq!(selected.highest_uid, 2);

        let first_batch = client
            .fetch_incremental("inbox", 0, None)
            .expect("fetch first batch");
        assert_eq!(first_batch.len(), 2);

        let second_batch = client
            .fetch_incremental("inbox", 2, selected.highest_modseq)
            .expect("fetch second batch");
        assert!(second_batch.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_client_fetches_complete_connected_thread() {
        let root = temp_dir("thread-fetch");
        fs::write(
            root.join("1-root.eml"),
            "Message-ID: <root@example.com>\nSubject: [PATCH] root\n\nroot\n",
        )
        .expect("write root");
        fs::write(
            root.join("2-reply.eml"),
            "Message-ID: <reply@example.com>\nIn-Reply-To: <root@example.com>\nReferences: <root@example.com>\nSubject: Re: [PATCH] root\n\nreply\n",
        )
        .expect("write reply");
        fs::write(
            root.join("3-nested.eml"),
            "Message-ID: <nested@example.com>\nIn-Reply-To: <reply@example.com>\nReferences: <root@example.com> <reply@example.com>\nSubject: Re: [PATCH] root\n\nnested\n",
        )
        .expect("write nested reply");
        fs::write(
            root.join("4-unrelated.eml"),
            "Message-ID: <other@example.com>\nSubject: unrelated\n\nother\n",
        )
        .expect("write unrelated");

        let mut client = FixtureImapClient::new(root.clone(), 1);
        client.connect().expect("connect fixture");
        let fetched = client
            .fetch_thread("inbox", "<reply@example.com>")
            .expect("fetch complete thread");
        let ids: Vec<String> = fetched
            .iter()
            .map(|mail| parse_headers(&mail.raw, "fallback@example.com".to_string()).message_id)
            .collect();

        assert_eq!(
            ids,
            vec![
                "root@example.com",
                "reply@example.com",
                "nested@example.com"
            ]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lore_thread_url_candidates_encode_message_id_and_support_routing() {
        let client = LoreImapClient::new(Some("https://lore.example")).expect("create client");
        assert_eq!(
            client.thread_url_candidates("io-uring", "<abc@example.com>"),
            vec![
                "https://lore.example/io-uring/abc%40example.com/t.mbox.gz",
                "https://lore.example/r/abc%40example.com/t.mbox.gz"
            ]
        );
    }

    #[test]
    fn gzip_thread_payload_is_decoded_before_mbox_parsing() {
        let raw =
            b"From sender Tue Mar 03 04:39:31 2026\nMessage-ID: <thread@example.com>\n\nbody\n";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(raw).expect("write gzip payload");
        let compressed = encoder.finish().expect("finish gzip payload");

        let decoded = decode_gzip_payload(&compressed, "https://lore.example/thread")
            .expect("decode gzip payload");
        assert_eq!(decoded, raw);
        assert_eq!(parse_gnu_archive_mbox_messages(&decoded).len(), 1);
    }

    #[test]
    fn thread_filter_rejects_missing_target_message_id() {
        let messages = vec![RemoteMail {
            uid: 1,
            modseq: None,
            flags: Vec::new(),
            raw: b"Message-ID: <present@example.com>\n\nbody\n".to_vec(),
        }];

        assert!(retain_thread_messages(messages, "missing@example.com").is_empty());
    }

    #[test]
    fn reads_uidvalidity_from_marker_file() {
        let root = temp_dir("uidvalidity");
        fs::write(root.join(".uidvalidity"), "9001\n").expect("write marker");
        fs::write(
            root.join("1.eml"),
            "Message-ID: <x@example.com>\nSubject: x\n\nbody\n",
        )
        .expect("write message");

        let mut client = FixtureImapClient::new(root.clone(), 1);
        client.connect().expect("connect");
        let snapshot = client.select_mailbox("inbox").expect("select");
        assert_eq!(snapshot.uidvalidity, 9001);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_client_requires_connect_and_reports_invalid_sources() {
        let missing_root = temp_dir("fixture-missing-root");
        fs::remove_dir_all(&missing_root).expect("remove root");
        let mut client = FixtureImapClient::new(missing_root.clone(), 1);
        let error = client.connect().expect_err("missing root should fail");
        assert!(error.to_string().contains("does not exist"));

        let file_root = temp_dir("fixture-file-root");
        let file_path = file_root.join("fixture.eml");
        fs::write(&file_path, "mail").expect("write fixture file");
        let mut file_client = FixtureImapClient::new(file_path.clone(), 1);
        let error = file_client.connect().expect_err("file root should fail");
        assert!(error.to_string().contains("is not a directory"));

        let valid_root = temp_dir("fixture-disconnected");
        fs::write(
            valid_root.join("1.eml"),
            "Message-ID: <disconnected@example.com>\nSubject: disconnected\n\nbody\n",
        )
        .expect("write fixture mail");
        let mut disconnected = FixtureImapClient::new(valid_root.clone(), 1);
        let error = disconnected
            .select_mailbox("inbox")
            .expect_err("select without connect should fail");
        assert!(error.to_string().contains("client is not connected"));

        disconnected.connect().expect("connect fixture");
        fs::remove_dir_all(&valid_root).expect("remove fixture root after connect");
        let error = disconnected
            .select_mailbox("inbox")
            .expect_err("missing mailbox directory should fail");
        assert!(error.to_string().contains("mailbox directory"));

        let _ = fs::remove_dir_all(file_root);
    }

    #[test]
    fn fixture_client_scans_subdirectories_deduplicates_uids_and_parses_flags() {
        let root = temp_dir("fixture-subdir");
        let mailbox_dir = root.join("inbox");
        fs::create_dir_all(&mailbox_dir).expect("create mailbox dir");
        fs::write(
            mailbox_dir.join("1-a.eml"),
            "Message-ID: <a@example.com>\nSubject: a\nX-Flags: Seen, Flagged\n\nbody\n",
        )
        .expect("write first mail");
        thread::sleep(Duration::from_millis(5));
        fs::write(
            mailbox_dir.join("1-b.eml"),
            "Message-ID: <b@example.com>\nSubject: b\nX-Flags: Answered,\n Flagged\n\nbody\n",
        )
        .expect("write second mail");
        fs::write(
            mailbox_dir.join("note.eml"),
            "Message-ID: <c@example.com>\nSubject: c\n\nbody\n",
        )
        .expect("write third mail");

        let mut client = FixtureImapClient::new(root.clone(), 77);
        client.connect().expect("connect");
        let snapshot = client.select_mailbox("inbox").expect("select mailbox");
        assert_eq!(snapshot.uidvalidity, 77);
        assert_eq!(snapshot.highest_uid, 3);
        assert!(snapshot.highest_modseq.is_some());

        let fetched = client
            .fetch_incremental("inbox", 0, None)
            .expect("fetch fixture mail");
        assert_eq!(
            fetched.iter().map(|mail| mail.uid).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            fetched[0].flags,
            vec!["Seen".to_string(), "Flagged".to_string()]
        );
        assert_eq!(
            fetched[1].flags,
            vec!["Answered".to_string(), "Flagged".to_string()]
        );

        let incremental = client
            .fetch_incremental("inbox", 1, None)
            .expect("fetch after uid");
        assert_eq!(
            incremental.iter().map(|mail| mail.uid).collect::<Vec<_>>(),
            vec![2, 3]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixture_client_reports_invalid_uidvalidity_marker() {
        let root = temp_dir("uidvalidity-invalid");
        fs::write(root.join(".uidvalidity"), "not-a-number\n").expect("write marker");
        fs::write(
            root.join("1.eml"),
            "Message-ID: <x@example.com>\nSubject: x\n\nbody\n",
        )
        .expect("write message");

        let mut client = FixtureImapClient::new(root.clone(), 1);
        client.connect().expect("connect");
        let error = client
            .select_mailbox("inbox")
            .expect_err("invalid UIDVALIDITY should fail");
        assert!(error.to_string().contains("invalid UIDVALIDITY value"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blank_uidvalidity_marker_falls_back_to_default() {
        let root = temp_dir("uidvalidity-blank");
        fs::write(root.join(".uidvalidity"), " \n").expect("write marker");
        fs::write(
            root.join("1.eml"),
            "Message-ID: <x@example.com>\nSubject: x\n\nbody\n",
        )
        .expect("write message");

        let mut client = FixtureImapClient::new(root.clone(), 55);
        client.connect().expect("connect");
        let snapshot = client.select_mailbox("inbox").expect("select mailbox");
        assert_eq!(snapshot.uidvalidity, 55);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_client_accepts_complete_config() {
        let client = RemoteImapClient::new(complete_imap_config(ImapEncryption::Tls));

        assert!(client.is_ok());
    }

    #[test]
    fn remote_client_rejects_incomplete_config() {
        let error = RemoteImapClient::new(ImapConfig {
            email: None,
            user: Some("imap-user".to_string()),
            pass: None,
            server: Some("imap.example.com".to_string()),
            server_port: Some(993),
            encryption: Some(ImapEncryption::Tls),
            proxy: None,
        })
        .err()
        .expect("incomplete config should fail");

        assert!(error.to_string().contains("imap.pass"));
    }

    #[test]
    fn lore_client_selects_and_fetches_incremental_from_local_server() {
        let first_modseq = parse_atom_timestamp("2026-03-03T09:00:00+00:00").expect("first ts");
        let second_modseq = parse_atom_timestamp("2026-03-03T10:00:00+00:00").expect("second ts");
        let feed = r#"
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>https://lore.kernel.org/io-uring/msg-a/</id>
    <updated>2026-03-03T09:00:00+00:00</updated>
    <link rel="alternate" href="https://lore.kernel.org/io-uring/msg-a/" />
  </entry>
  <entry>
    <id>https://lore.kernel.org/io-uring/msg-b/</id>
    <updated>2026-03-03T10:00:00+00:00</updated>
    <link rel="alternate" href="https://lore.kernel.org/io-uring/msg-b/" />
  </entry>
</feed>
"#;
        let base_url = "https://lore.test";
        let mut client = LoreImapClient::new(Some(base_url)).expect("create lore client");
        let error = client
            .select_mailbox("io-uring")
            .expect_err("select before connect should fail");
        assert!(error.to_string().contains("client is not connected"));

        client.connect().expect("connect lore");
        assert_eq!(
            client.feed_url("io-uring"),
            "https://lore.test/io-uring/new.atom"
        );
        let entries = parse_lore_feed_response(&client.feed_url("io-uring"), 200, feed)
            .expect("parse lore feed response");
        let snapshot = MailboxSnapshot {
            uidvalidity: 1,
            highest_uid: 0,
            highest_modseq: entries.iter().map(|entry| entry.modseq).max(),
        };
        assert_eq!(snapshot.uidvalidity, 1);
        assert_eq!(snapshot.highest_uid, 0);
        assert_eq!(snapshot.highest_modseq, Some(second_modseq));

        let raw_by_url = HashMap::from([(
            "https://lore.kernel.org/io-uring/msg-b/raw".to_string(),
            b"Message-ID: <msg-b@example.com>\n\nbody\n".to_vec(),
        )]);
        let fetched = build_lore_incremental_mails(entries, Some(first_modseq), |message_url| {
            fetch_lore_raw_with(message_url, |raw_url| {
                Ok((200, raw_by_url.get(raw_url).cloned().unwrap_or_default()))
            })
        })
        .expect("fetch incremental lore");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].modseq, Some(second_modseq));
        assert!(String::from_utf8_lossy(&fetched[0].raw).contains("msg-b@example.com"));
    }

    #[test]
    fn lore_client_fetches_feed_and_raw_over_http() {
        let first_modseq = parse_atom_timestamp("2026-03-03T09:00:00+00:00").expect("first ts");
        let second_modseq = parse_atom_timestamp("2026-03-03T10:00:00+00:00").expect("second ts");
        let (base_url, handle) = start_http_server(4, |base_url| {
            let feed = format!(
                r#"
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>{base_url}/io-uring/msg-a/</id>
    <updated>2026-03-03T09:00:00+00:00</updated>
    <link rel="alternate" href="{base_url}/io-uring/msg-a/" />
  </entry>
  <entry>
    <id>{base_url}/io-uring/msg-b/</id>
    <updated>2026-03-03T10:00:00+00:00</updated>
    <link rel="alternate" href="{base_url}/io-uring/msg-b/" />
  </entry>
</feed>
"#
            );
            HashMap::from([
                (
                    "/io-uring/new.atom".to_string(),
                    StubHttpResponse::text(200, feed),
                ),
                (
                    "/io-uring/msg-b/raw".to_string(),
                    StubHttpResponse::text(404, ""),
                ),
                (
                    "/io-uring/msg-b/raw/".to_string(),
                    StubHttpResponse::bytes(
                        200,
                        b"Message-ID: <msg-b@example.com>\nSubject: message b\n\nbody\n".to_vec(),
                    ),
                ),
            ])
        });

        let mut client = LoreImapClient::new(Some(&base_url)).expect("create lore client");
        client.connect().expect("connect lore");

        let snapshot = client
            .select_mailbox("/io-uring/")
            .expect("select lore mailbox");
        assert_eq!(snapshot.uidvalidity, 1);
        assert_eq!(snapshot.highest_uid, 0);
        assert_eq!(snapshot.highest_modseq, Some(second_modseq));

        let fetched = client
            .fetch_incremental("io-uring", 0, Some(first_modseq))
            .expect("fetch lore mails");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].modseq, Some(second_modseq));
        assert!(String::from_utf8_lossy(&fetched[0].raw).contains("msg-b@example.com"));

        handle.join().expect("join HTTP server");
    }

    #[test]
    fn lore_client_reports_feed_http_errors_from_server() {
        let (base_url, handle) = start_http_server(1, |_| {
            HashMap::from([(
                "/broken/new.atom".to_string(),
                StubHttpResponse::text(404, "missing mailbox"),
            )])
        });

        let mut client = LoreImapClient::new(Some(&base_url)).expect("create lore client");
        client.connect().expect("connect lore");

        let error = client
            .select_mailbox("broken")
            .expect_err("HTTP errors should fail mailbox selection");
        assert!(error.to_string().contains("HTTP 404"));

        handle.join().expect("join HTTP server");
    }

    #[test]
    fn lore_client_checks_status_before_reading_error_body() {
        let (base_url, handle) = start_http_server_with_raw_responses(2, move |path| {
            match path {
            "/io-uring/msg-b/raw" | "/io-uring/msg-b/raw/" => b"HTTP/1.1 404 Not Found\r\nContent-Length: 64\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nshort".to_vec(),
            _ => panic!("unexpected HTTP path {path}"),
        }
        });

        let mut client = LoreImapClient::new(Some(&base_url)).expect("create lore client");
        client.connect().expect("connect lore");

        let error = client
            .fetch_raw_mail(&format!("{base_url}/io-uring/msg-b/"))
            .expect_err("HTTP status should fail before reading body");
        assert!(error.to_string().contains("HTTP 404"));

        handle.join().expect("join HTTP server");
    }

    #[test]
    fn lore_client_reports_empty_raw_message_after_trying_candidates() {
        let error = fetch_lore_raw_with("https://lore.kernel.org/io-uring/msg-empty/", |_| {
            Ok((200, Vec::new()))
        })
        .expect_err("empty raw should fail");
        assert!(error.to_string().contains("lore raw message is empty"));
    }

    #[test]
    fn gnu_archive_client_selects_and_fetches_incremental_from_local_server() {
        let feb_modseq =
            parse_gnu_archive_listing_timestamp("2026-02-26", "09:12").expect("feb timestamp");
        let html = r#"
<pre>
<a href="2026-02">2026-02</a> 2026-02-26 09:12  855K
<a href="2026-03">2026-03</a> 2026-03-07 06:37  341K
</pre>
"#;
        let mbox = b"From MAILER-DAEMON Tue Mar 03 04:39:31 2026\nMessage-ID: <msg-a@example.com>\nSubject: one\n\nbody\nFrom MAILER-DAEMON Tue Mar 03 04:40:31 2026\nMessage-ID: <msg-b@example.com>\nSubject: two\n\nbody two\n";
        let mut client =
            GnuArchiveClient::new(Some("https://archive.test")).expect("create archive client");
        let error = client
            .select_mailbox("mailbox")
            .expect_err("select before connect should fail");
        assert!(error.to_string().contains("client is not connected"));

        client.connect().expect("connect archive");
        assert_eq!(client.index_url("mailbox"), "https://archive.test/mailbox/");
        assert_eq!(
            client.month_url("mailbox", "2026-03"),
            "https://archive.test/mailbox/2026-03"
        );
        let months = parse_gnu_archive_index_response(&client.index_url("mailbox"), 200, html)
            .expect("parse archive index");
        let snapshot = MailboxSnapshot {
            uidvalidity: 1,
            highest_uid: 0,
            highest_modseq: months.iter().map(|entry| entry.modseq).max(),
        };
        assert_eq!(snapshot.uidvalidity, 1);
        assert_eq!(snapshot.highest_uid, 0);
        assert!(snapshot.highest_modseq.is_some());

        let fetched = build_gnu_archive_incremental_mails(&months, Some(feb_modseq), |month_key| {
            let url = client.month_url("mailbox", month_key);
            validate_gnu_archive_mbox_response(&url, 200)?;
            Ok(mbox.to_vec())
        })
        .expect("fetch gnu archive");
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].uid, 314_000_001);
        assert_eq!(fetched[1].uid, 314_000_002);
    }

    #[test]
    fn gnu_archive_client_fetches_index_and_mbox_over_http() {
        let feb_modseq =
            parse_gnu_archive_listing_timestamp("2026-02-26", "09:12").expect("feb timestamp");
        let (base_url, handle) = start_http_server(3, |_| {
            let html = r#"
<pre>
<a href="2026-02">2026-02</a> 2026-02-26 09:12  855K
<a href="2026-03">2026-03</a> 2026-03-07 06:37  341K
</pre>
"#;
            let mbox = b"From MAILER-DAEMON Tue Mar 03 04:39:31 2026\nMessage-ID: <msg-a@example.com>\nSubject: one\n\nbody\nFrom MAILER-DAEMON Tue Mar 03 04:40:31 2026\nMessage-ID: <msg-b@example.com>\nSubject: two\n\nbody two\n";
            HashMap::from([
                ("/mailbox/".to_string(), StubHttpResponse::text(200, html)),
                (
                    "/mailbox/2026-03".to_string(),
                    StubHttpResponse::bytes(200, mbox.to_vec()),
                ),
            ])
        });

        let mut client = GnuArchiveClient::new(Some(&base_url)).expect("create archive client");
        client.connect().expect("connect archive");

        let snapshot = client
            .select_mailbox("/mailbox/")
            .expect("select archive mailbox");
        assert_eq!(snapshot.uidvalidity, 1);
        assert_eq!(snapshot.highest_uid, 0);
        assert!(snapshot.highest_modseq.is_some());

        let fetched = client
            .fetch_incremental("mailbox", 0, Some(feb_modseq))
            .expect("fetch archive month");
        assert_eq!(
            fetched.iter().map(|mail| mail.uid).collect::<Vec<_>>(),
            vec![314_000_001, 314_000_002]
        );

        handle.join().expect("join HTTP server");
    }

    #[test]
    fn gnu_archive_client_reports_mbox_http_errors_from_server() {
        let feb_modseq =
            parse_gnu_archive_listing_timestamp("2026-02-26", "09:12").expect("feb timestamp");
        let (base_url, handle) = start_http_server(2, |_| {
            let html = r#"
<pre>
<a href="2026-02">2026-02</a> 2026-02-26 09:12  855K
<a href="2026-03">2026-03</a> 2026-03-07 06:37  341K
</pre>
"#;
            HashMap::from([
                ("/mailbox/".to_string(), StubHttpResponse::text(200, html)),
                (
                    "/mailbox/2026-03".to_string(),
                    StubHttpResponse::text(404, "month not found"),
                ),
            ])
        });

        let mut client = GnuArchiveClient::new(Some(&base_url)).expect("create archive client");
        client.connect().expect("connect archive");

        let error = client
            .fetch_incremental("mailbox", 0, Some(feb_modseq))
            .expect_err("HTTP errors should fail archive fetch");
        assert!(error.to_string().contains("HTTP 404"));

        handle.join().expect("join HTTP server");
    }

    #[test]
    fn gnu_archive_client_checks_status_before_reading_error_body() {
        let feb_modseq =
            parse_gnu_archive_listing_timestamp("2026-02-26", "09:12").expect("feb timestamp");
        let index_body = r#"
<pre>
<a href="2026-02">2026-02</a> 2026-02-26 09:12  855K
<a href="2026-03">2026-03</a> 2026-03-07 06:37  341K
</pre>
"#;
        let (base_url, handle) = start_http_server_with_raw_responses(2, move |path| {
            match path {
            "/mailbox/" => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n{}",
                index_body.len(),
                index_body,
            )
            .into_bytes(),
            "/mailbox/2026-03" => b"HTTP/1.1 404 Not Found\r\nContent-Length: 64\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nshort".to_vec(),
            _ => panic!("unexpected HTTP path {path}"),
        }
        });

        let mut client = GnuArchiveClient::new(Some(&base_url)).expect("create archive client");
        client.connect().expect("connect archive");

        let error = client
            .fetch_incremental("mailbox", 0, Some(feb_modseq))
            .expect_err("HTTP status should fail before reading body");
        assert!(error.to_string().contains("HTTP 404"));

        handle.join().expect("join HTTP server");
    }

    #[test]
    fn http_clients_surface_transport_errors_before_receiving_responses() {
        let mut lore_client =
            LoreImapClient::new(Some("http://127.0.0.1:1")).expect("create lore client");
        lore_client.connect().expect("connect lore");
        let lore_error = lore_client
            .select_mailbox("io-uring")
            .expect_err("missing listener should fail feed fetch");
        assert!(lore_error.to_string().contains("failed to fetch lore feed"));

        let mut archive_client =
            GnuArchiveClient::new(Some("http://127.0.0.1:1")).expect("create archive client");
        archive_client.connect().expect("connect archive");
        let archive_error = archive_client
            .select_mailbox("mailbox")
            .expect_err("missing listener should fail archive fetch");
        assert!(
            archive_error
                .to_string()
                .contains("failed to fetch GNU archive index")
        );
    }

    #[test]
    fn parses_imap_fetch_metadata() {
        let line = "* 2 FETCH (UID 2 FLAGS (\\Seen \\Answered) MODSEQ (20) BODY[] {123}";
        assert_eq!(parse_fetch_uid(line), Some(2));
        assert_eq!(parse_fetch_modseq(line), Some(20));
        assert_eq!(parse_literal_len(line), Some(123));
        assert_eq!(
            parse_fetch_flags(line),
            vec!["\\Seen".to_string(), "\\Answered".to_string()]
        );
    }

    #[test]
    fn parses_select_status_codes() {
        assert_eq!(
            parse_status_code_u64("* OK [UIDVALIDITY 77] UIDs valid", "UIDVALIDITY"),
            Some(77)
        );
        assert_eq!(
            parse_status_code_u64("* OK [UIDNEXT 42] Predicted next UID", "UIDNEXT"),
            Some(42)
        );
        assert_eq!(
            parse_status_code_u64("* OK [HIGHESTMODSEQ 9001] Highest", "HIGHESTMODSEQ"),
            Some(9001)
        );
    }

    #[test]
    fn tagged_errors_keep_imap_error_classification() {
        let error = ensure_tagged_ok(
            "A0002 NO [AUTHENTICATIONFAILED] invalid credentials",
            ImapErrorKind::Authentication,
        )
        .expect_err("tagged failure should surface");

        assert!(error.to_string().contains("authentication"));
    }

    #[test]
    fn tls_client_config_uses_explicit_crypto_provider() {
        let root_store =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let _config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("configure protocol versions")
        .with_root_certificates(root_store)
        .with_no_client_auth();
    }

    #[test]
    fn uid_sequence_set_compacts_contiguous_ranges() {
        assert_eq!(format_uid_sequence_set(&[1, 2, 3, 5, 8, 9]), "1:3,5,8:9");
        assert_eq!(format_uid_sequence_set(&[5]), "5");
        assert_eq!(format_uid_sequence_set(&[9, 7, 8, 8]), "7:9");
    }

    #[test]
    fn imap_proxy_parser_rejects_invalid_proxy_urls() {
        let proxy = parse_imap_proxy("http://127.0.0.1").expect("default http port");
        assert_eq!(proxy.redacted_url(), "http://127.0.0.1:80");
        let socks_proxy = parse_imap_proxy("socks5h://127.0.0.1").expect("default socks5 port");
        assert_eq!(socks_proxy.redacted_url(), "socks5://127.0.0.1:1080");

        let unsupported =
            parse_imap_proxy("https://127.0.0.1:443").expect_err("unsupported scheme should fail");
        assert!(
            unsupported
                .to_string()
                .contains("unsupported IMAP proxy scheme")
        );

        let auth = parse_imap_proxy("http://user:pass@127.0.0.1:8080")
            .expect_err("proxy auth should fail");
        assert!(auth.to_string().contains("authentication is not supported"));

        let path_error =
            parse_imap_proxy("http://127.0.0.1:8080/proxy").expect_err("path should fail");
        assert!(
            path_error
                .to_string()
                .contains("remove path, query, and fragment")
        );

        let missing_host =
            parse_imap_proxy("http:///").expect_err("missing host should fail proxy parsing");
        assert!(missing_host.to_string().contains("invalid IMAP proxy URL"));
    }

    #[test]
    fn http_proxy_connect_tunnels_imap_socket() {
        let proxy = parse_imap_proxy("http://127.0.0.1:7890").expect("parse proxy");
        let mut stream = MockStream::with_reads(b"HTTP/1.1 200 Connection established\r\n\r\n");

        establish_http_connect_tunnel(&mut stream, &proxy, "imap.gmail.com", 993)
            .expect("proxy tunnel should connect");

        let request_text = String::from_utf8(stream.writes).expect("request utf8");
        assert!(request_text.starts_with("CONNECT imap.gmail.com:993 HTTP/1.1\r\n"));
        assert!(request_text.contains("\r\nHost: imap.gmail.com:993\r\n"));
    }

    #[test]
    fn http_proxy_helpers_report_truncated_and_rejected_responses() {
        let proxy = parse_imap_proxy("http://127.0.0.1:7890").expect("parse proxy");

        let mut truncated = MockStream::default();
        let error = read_http_proxy_response(&mut truncated, &proxy, "imap.gmail.com:993")
            .expect_err("truncated response should fail");
        assert!(
            error
                .to_string()
                .contains("closed the connection before CONNECT")
        );

        let mut rejected = MockStream::with_reads(b"HTTP/1.1 403 Forbidden\r\n\r\n");
        let error = establish_http_connect_tunnel(&mut rejected, &proxy, "imap.gmail.com", 993)
            .expect_err("non-2xx connect should fail");
        assert!(error.to_string().contains("rejected CONNECT"));

        let oversized = vec![b'a'; super::HTTP_PROXY_RESPONSE_MAX_BYTES + 1];
        let mut oversized_stream = MockStream::with_reads(&oversized);
        let error = read_http_proxy_response(&mut oversized_stream, &proxy, "imap.gmail.com:993")
            .expect_err("oversized responses should fail");
        assert!(
            error
                .to_string()
                .contains("sent too much HTTP response data")
        );

        let mut read_error_stream = ReadFailsStream;
        let error = read_http_proxy_response(&mut read_error_stream, &proxy, "imap.gmail.com:993")
            .expect_err("proxy read failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed while reading IMAP proxy")
        );

        let mut write_error_stream = WriteFailsStream;
        let error =
            establish_http_connect_tunnel(&mut write_error_stream, &proxy, "imap.gmail.com", 993)
                .expect_err("write failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to send IMAP CONNECT request")
        );

        let mut flush_error_stream = FlushFailsStream;
        let error =
            establish_http_connect_tunnel(&mut flush_error_stream, &proxy, "imap.gmail.com", 993)
                .expect_err("flush failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to flush IMAP CONNECT request")
        );
    }

    #[test]
    fn socks5_proxy_connect_tunnels_imap_socket() {
        let proxy = parse_imap_proxy("socks5://127.0.0.1:7890").expect("parse proxy");
        let mut stream = MockStream::with_reads(&[
            0x05, 0x00, // greeting reply
            0x05, 0x00, 0x00, // connect reply status
            0x03, 0x0e, b'i', b'm', b'a', b'p', b'.', b'g', b'm', b'a', b'i', b'l', b'.', b'c',
            b'o', b'm', 0x03, 0xe1, // bound address + port
        ]);

        establish_socks5_tunnel(&mut stream, &proxy, "imap.gmail.com", 993)
            .expect("proxy tunnel should connect");

        assert_eq!(
            stream.writes,
            [
                0x05, 0x01, 0x00, // greeting
                0x05, 0x01, 0x00, 0x03, 14, b'i', b'm', b'a', b'p', b'.', b'g', b'm', b'a', b'i',
                b'l', b'.', b'c', b'o', b'm', 0x03, 0xe1
            ]
        );
    }

    #[test]
    fn socks5_proxy_reports_handshake_and_connect_failures() {
        let proxy = parse_imap_proxy("socks5://127.0.0.1:7890").expect("parse proxy");

        let mut invalid_version = MockStream::with_reads(&[0x04, 0x00]);
        let error = establish_socks5_tunnel(&mut invalid_version, &proxy, "imap.gmail.com", 993)
            .expect_err("invalid version should fail");
        assert!(error.to_string().contains("invalid SOCKS5 version"));

        let mut unauthenticated = MockStream::with_reads(&[0x05, 0x02]);
        let error = establish_socks5_tunnel(&mut unauthenticated, &proxy, "imap.gmail.com", 993)
            .expect_err("auth required should fail");
        assert!(error.to_string().contains("does not allow unauthenticated"));

        let mut connect_failed = MockStream::with_reads(&[0x05, 0x00, 0x05, 0x05, 0x00]);
        let error = establish_socks5_tunnel(&mut connect_failed, &proxy, "imap.gmail.com", 993)
            .expect_err("connect reply should fail");
        assert!(error.to_string().contains("connection refused"));

        let mut greeting_read_error = ReadFailsStream;
        let error =
            establish_socks5_tunnel(&mut greeting_read_error, &proxy, "imap.gmail.com", 993)
                .expect_err("greeting read failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to read SOCKS5 greeting reply")
        );

        let mut greeting_write_error = WriteFailsStream;
        let error =
            establish_socks5_tunnel(&mut greeting_write_error, &proxy, "imap.gmail.com", 993)
                .expect_err("greeting write failures should surface");
        assert!(error.to_string().contains("failed to send SOCKS5 greeting"));

        let too_long_host = "a".repeat(256);
        let mut host_length_stream = MockStream::with_reads(&[0x05, 0x00]);
        let error = establish_socks5_tunnel(&mut host_length_stream, &proxy, &too_long_host, 993)
            .expect_err("long hostnames should fail");
        assert!(error.to_string().contains("too long for SOCKS5 proxying"));
    }

    #[test]
    fn socks5_reply_address_reports_decode_failures() {
        let proxy = parse_imap_proxy("socks5://127.0.0.1:7890").expect("parse proxy");

        let mut atyp_error = ReadFailsStream;
        let error = super::read_socks5_reply_address(&mut atyp_error, &proxy)
            .expect_err("ATYP read failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to read SOCKS5 reply type")
        );

        let mut domain_length_error = MockStream::with_reads(&[0x03]);
        let error = super::read_socks5_reply_address(&mut domain_length_error, &proxy)
            .expect_err("domain length failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to read SOCKS5 domain length")
        );

        let mut bind_address_error = MockStream::with_reads(&[0x01]);
        let error = super::read_socks5_reply_address(&mut bind_address_error, &proxy)
            .expect_err("bind address failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to read SOCKS5 bind address")
        );
    }

    #[test]
    fn imap_session_executes_command_flow_over_mock_transport() {
        let responses = concat!(
            "* OK hello\r\n",
            "* CAPABILITY IMAP4rev1 CONDSTORE\r\n",
            "A0001 OK capability\r\n",
            "A0002 OK login\r\n",
            "* OK [UIDVALIDITY 77] valid\r\n",
            "* OK [UIDNEXT 42] next\r\n",
            "* OK [HIGHESTMODSEQ 9001] modseq\r\n",
            "A0003 OK select\r\n",
            "* SEARCH 4 5 5 9\r\n",
            "A0004 OK search\r\n",
            "* 1 FETCH (UID 4 FLAGS (\\Seen \\Answered) MODSEQ (20) BODY.PEEK[] {5}\r\n",
            "hello\r\n",
            ")\r\n",
            "A0005 OK fetch\r\n"
        );
        let mut session =
            ImapSession::with_mock_stream(MockStream::with_reads(responses.as_bytes()));

        assert_eq!(
            session.read_greeting().expect("read greeting"),
            GreetingKind::Ok
        );
        let capabilities = session.fetch_capabilities().expect("fetch capabilities");
        assert!(capabilities.contains("IMAP4REV1"));
        assert!(capabilities.contains("CONDSTORE"));
        session.capabilities = capabilities;

        let config = ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: Some(993),
            encryption: Some(ImapEncryption::Tls),
            proxy: None,
        };
        session.login(&config).expect("login");

        let snapshot = session.select_mailbox("INBOX").expect("select");
        assert_eq!(snapshot.uidvalidity, 77);
        assert_eq!(snapshot.highest_uid, 41);
        assert_eq!(snapshot.highest_modseq, Some(9001));

        let uids = session.search_uid_range(4).expect("search uids");
        assert_eq!(uids, vec![4, 5, 9]);

        let fetched = session.fetch_uids(&[4], "BODY.PEEK[]").expect("fetch uids");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].uid, 4);
        assert_eq!(fetched[0].modseq, Some(20));
        assert_eq!(
            fetched[0].flags,
            vec!["\\Seen".to_string(), "\\Answered".to_string()]
        );
        assert_eq!(fetched[0].raw, b"hello".to_vec());
    }

    #[test]
    fn imap_session_handles_greeting_login_and_fetch_failures() {
        let mut bye_session =
            ImapSession::with_mock_stream(MockStream::with_reads(b"* BYE go away\r\n"));
        let error = bye_session
            .read_greeting()
            .expect_err("BYE greeting should fail");
        assert!(
            error
                .to_string()
                .contains("server closed connection during greeting")
        );

        let mut plaintext_session = ImapSession::with_mock_stream(MockStream::default());
        plaintext_session
            .capabilities
            .insert("LOGINDISABLED".to_string());
        let config = ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: Some(143),
            encryption: Some(ImapEncryption::None),
            proxy: None,
        };
        let error = plaintext_session
            .login(&config)
            .expect_err("plaintext login should be rejected");
        assert!(error.to_string().contains("disallows LOGIN over plaintext"));

        let mut fetch_session = ImapSession::with_mock_stream(MockStream::with_reads(
            b"* 1 FETCH (FLAGS (\\Seen) BODY.PEEK[] {5}\r\nhello\r\n)\r\nA0001 OK fetch\r\n",
        ));
        let error = fetch_session
            .fetch_uid_chunk(&[4], "BODY.PEEK[]")
            .expect_err("missing UID should fail");
        assert!(error.to_string().contains("missing UID in FETCH response"));

        let mut eof_session = ImapSession::with_mock_stream(MockStream::default());
        let error = eof_session
            .read_line_string()
            .expect_err("EOF while reading line should fail");
        assert!(
            error
                .to_string()
                .contains("unexpected EOF while reading IMAP response")
        );
    }

    #[test]
    fn imap_session_covers_misc_defaults_and_protocol_edges() {
        let mut preauth_session =
            ImapSession::with_mock_stream(MockStream::with_reads(b"* PREAUTH welcome\r\n"));
        assert_eq!(
            preauth_session.read_greeting().expect("PREAUTH greeting"),
            GreetingKind::Preauth
        );

        let mut unexpected_session =
            ImapSession::with_mock_stream(MockStream::with_reads(b"* WHAT welcome\r\n"));
        let error = unexpected_session
            .read_greeting()
            .expect_err("unexpected greeting should fail");
        assert!(error.to_string().contains("unexpected IMAP greeting"));

        let mut missing_user_session = ImapSession::with_mock_stream(MockStream::default());
        let error = missing_user_session
            .login(&ImapConfig {
                email: None,
                user: None,
                pass: Some("imap-pass".to_string()),
                server: Some("imap.example.com".to_string()),
                server_port: Some(993),
                encryption: Some(ImapEncryption::Tls),
                proxy: None,
            })
            .expect_err("missing user should fail");
        assert!(error.to_string().contains("missing imap.user"));

        let mut missing_pass_session = ImapSession::with_mock_stream(MockStream::default());
        let error = missing_pass_session
            .login(&ImapConfig {
                email: Some("me@example.com".to_string()),
                user: Some("imap-user".to_string()),
                pass: None,
                server: Some("imap.example.com".to_string()),
                server_port: Some(993),
                encryption: Some(ImapEncryption::Tls),
                proxy: None,
            })
            .expect_err("missing pass should fail");
        assert!(error.to_string().contains("missing imap.pass"));

        let mut default_select = ImapSession::with_mock_stream(MockStream::with_reads(
            b"* FLAGS (\\Seen)\r\nA0001 OK [READ-WRITE] select\r\n",
        ));
        let snapshot = default_select
            .select_mailbox("INBOX")
            .expect("select mailbox with defaults");
        assert_eq!(snapshot.uidvalidity, 1);
        assert_eq!(snapshot.highest_uid, 0);
        assert_eq!(snapshot.highest_modseq, None);

        let mut noisy_search = ImapSession::with_mock_stream(MockStream::with_reads(
            b"* OK noop\r\n* SEARCH 7 nope 9\r\nA0001 OK search\r\n",
        ));
        let uids = noisy_search.search_uid_range(7).expect("search with noise");
        assert_eq!(uids, vec![7, 9]);

        let mut empty_fetches = ImapSession::with_mock_stream(MockStream::default());
        assert!(
            empty_fetches
                .fetch_uids(&[], "BODY.PEEK[]")
                .expect("empty uid list")
                .is_empty()
        );
        assert!(
            empty_fetches
                .fetch_uid_chunk(&[], "BODY.PEEK[]")
                .expect("empty uid chunk")
                .is_empty()
        );
    }

    #[test]
    fn imap_session_reports_fetch_trailer_and_io_failures() {
        let mut missing_literal = ImapSession::with_mock_stream(MockStream::with_reads(
            b"* 1 FETCH (UID 4 FLAGS (\\Seen) BODY.PEEK[]\r\nA0001 OK fetch\r\n",
        ));
        let error = missing_literal
            .fetch_uid_chunk(&[4], "BODY.PEEK[]")
            .expect_err("missing literal should fail");
        assert!(error.to_string().contains("missing literal size"));

        let mut truncated_trailer = ImapSession::with_mock_stream(MockStream::with_reads(
            b"* 1 FETCH (UID 4 FLAGS (\\Seen) MODSEQ (20) BODY.PEEK[] {5}\r\nhello\r\nA0001 OK fetch\r\n",
        ));
        let error = truncated_trailer
            .fetch_uid_chunk(&[4], "BODY.PEEK[]")
            .expect_err("truncated trailer should fail");
        assert!(error.to_string().contains("truncated FETCH trailer"));

        let mut write_error_session = ImapSession::with_mock_stream(WriteFailsStream);
        let error = write_error_session
            .write_command("A0001", "NOOP")
            .expect_err("write failures should surface");
        assert!(error.to_string().contains("failed to write IMAP command"));

        let mut flush_error_session = ImapSession::with_mock_stream(FlushFailsStream);
        let error = flush_error_session
            .write_command("A0001", "NOOP")
            .expect_err("flush failures should surface");
        assert!(error.to_string().contains("failed to flush IMAP command"));

        let mut read_line_error_session = ImapSession::with_mock_stream(ReadFailsStream);
        let error = read_line_error_session
            .read_line_bytes()
            .expect_err("socket read failures should surface");
        assert!(
            error
                .to_string()
                .contains("failed to read from IMAP socket")
        );

        let mut read_literal_error_session = ImapSession::with_mock_stream(ReadFailsStream);
        let error = read_literal_error_session
            .read_exact_bytes(4)
            .expect_err("literal read failures should surface");
        assert!(error.to_string().contains("failed to read IMAP literal"));
    }

    #[test]
    fn collect_incremental_uids_merges_uid_and_modseq_search_results() {
        let responses = concat!(
            "* SEARCH 7 8\r\n",
            "A0001 OK search\r\n",
            "* SEARCH 8 9\r\n",
            "A0002 OK search\r\n"
        );
        let mut session =
            ImapSession::with_mock_stream(MockStream::with_reads(responses.as_bytes()));
        let snapshot = MailboxSnapshot {
            uidvalidity: 1,
            highest_uid: 9,
            highest_modseq: Some(30),
        };

        let uids = collect_incremental_uids(&mut session, snapshot, 6, Some(20))
            .expect("collect incremental uids");
        assert_eq!(uids, vec![7, 8, 9]);
    }

    #[test]
    fn imap_session_connect_rejects_missing_runtime_fields() {
        let missing_server = ImapSession::connect(&ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: None,
            server_port: Some(993),
            encryption: Some(ImapEncryption::Tls),
            proxy: None,
        })
        .err()
        .expect("missing server should fail");
        assert!(missing_server.to_string().contains("missing imap.server"));

        let missing_port = ImapSession::connect(&ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: None,
            encryption: Some(ImapEncryption::Tls),
            proxy: None,
        })
        .err()
        .expect("missing port should fail");
        assert!(missing_port.to_string().contains("missing imap.serverport"));

        let missing_encryption = ImapSession::connect(&ImapConfig {
            email: Some("me@example.com".to_string()),
            user: Some("imap-user".to_string()),
            pass: Some("imap-pass".to_string()),
            server: Some("imap.example.com".to_string()),
            server_port: Some(993),
            encryption: None,
            proxy: None,
        })
        .err()
        .expect("missing encryption should fail");
        assert!(
            missing_encryption
                .to_string()
                .contains("missing imap.encryption")
        );
    }

    #[test]
    fn remote_client_requires_connected_session_and_delegates_requests() {
        let config = complete_imap_config(ImapEncryption::Tls);

        let mut disconnected =
            RemoteImapClient::new(config.clone()).expect("create disconnected remote client");
        let error = disconnected
            .select_mailbox("INBOX")
            .expect_err("missing session should fail");
        assert!(
            error
                .to_string()
                .contains("remote IMAP session is not connected")
        );

        let mut select_client = RemoteImapClient {
            config: config.clone(),
            session: Some(ImapSession::with_mock_stream(MockStream::with_reads(
                concat!(
                    "* OK [UIDVALIDITY 11] valid\r\n",
                    "* OK [UIDNEXT 4] next\r\n",
                    "* OK [HIGHESTMODSEQ 22] modseq\r\n",
                    "A0001 OK select\r\n"
                )
                .as_bytes(),
            ))),
        };
        let snapshot = select_client
            .select_mailbox("INBOX")
            .expect("delegate mailbox selection");
        assert_eq!(snapshot.uidvalidity, 11);
        assert_eq!(snapshot.highest_uid, 3);
        assert_eq!(snapshot.highest_modseq, Some(22));

        let mut incremental_client = RemoteImapClient {
            config: config.clone(),
            session: Some(ImapSession::with_mock_stream(MockStream::with_reads(
                concat!(
                    "* OK [UIDVALIDITY 11] valid\r\n",
                    "* OK [UIDNEXT 4] next\r\n",
                    "* OK [HIGHESTMODSEQ 22] modseq\r\n",
                    "A0001 OK select\r\n",
                    "* SEARCH 2 3\r\n",
                    "A0002 OK search\r\n",
                    "* SEARCH 3\r\n",
                    "A0003 OK search\r\n",
                    "* 1 FETCH (UID 2 FLAGS (\\Seen) MODSEQ (21) BODY.PEEK[] {5}\r\n",
                    "hello\r\n",
                    ")\r\n",
                    "* 2 FETCH (UID 3 FLAGS (\\Seen) MODSEQ (22) BODY.PEEK[] {5}\r\n",
                    "world\r\n",
                    ")\r\n",
                    "A0004 OK fetch\r\n"
                )
                .as_bytes(),
            ))),
        };
        let fetched = incremental_client
            .fetch_incremental("INBOX", 1, Some(20))
            .expect("delegate incremental fetch");
        assert_eq!(
            fetched.iter().map(|mail| mail.uid).collect::<Vec<_>>(),
            vec![2, 3]
        );

        let mut header_client = RemoteImapClient {
            config: config.clone(),
            session: Some(ImapSession::with_mock_stream(MockStream::with_reads(
                concat!(
                    "* OK [UIDVALIDITY 11] valid\r\n",
                    "* OK [UIDNEXT 8] next\r\n",
                    "* OK [HIGHESTMODSEQ 22] modseq\r\n",
                    "A0001 OK select\r\n",
                    "* SEARCH 7\r\n",
                    "A0002 OK search\r\n",
                    "* 1 FETCH (UID 7 FLAGS (\\Seen) MODSEQ (22) BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT FROM DATE IN-REPLY-TO REFERENCES LIST-ID)] {5}\r\n",
                    "headr\r\n",
                    ")\r\n",
                    "A0003 OK fetch\r\n"
                )
                .as_bytes(),
            ))),
        };
        let header_candidates = header_client
            .fetch_header_candidates("INBOX", 6, None)
            .expect("delegate header fetch");
        assert_eq!(
            header_candidates
                .iter()
                .map(|mail| mail.uid)
                .collect::<Vec<_>>(),
            vec![7]
        );

        let mut following_client = RemoteImapClient {
            config: config.clone(),
            session: Some(ImapSession::with_mock_stream(MockStream::with_reads(
                concat!(
                    "* OK [UIDVALIDITY 11] valid\r\n",
                    "* OK [UIDNEXT 4] next\r\n",
                    "* OK [HIGHESTMODSEQ 22] modseq\r\n",
                    "A0001 OK select\r\n",
                    "* SEARCH 2 3\r\n",
                    "A0002 OK search\r\n",
                    "* SEARCH 2\r\n",
                    "A0003 OK search\r\n",
                    "* SEARCH 3\r\n",
                    "A0004 OK search\r\n",
                    "* SEARCH 9\r\n",
                    "A0005 OK search\r\n",
                    "* 1 FETCH (UID 2 FLAGS (\\Seen) MODSEQ (21) BODY.PEEK[] {5}\r\n",
                    "hello\r\n",
                    ")\r\n",
                    "* 2 FETCH (UID 3 FLAGS (\\Seen) MODSEQ (22) BODY.PEEK[] {5}\r\n",
                    "world\r\n",
                    ")\r\n",
                    "A0006 OK fetch\r\n"
                )
                .as_bytes(),
            ))),
        };
        let following = following_client
            .fetch_following_incremental("INBOX", "me@example.com", 1, None)
            .expect("delegate Following search fetch");
        assert_eq!(
            following.iter().map(|mail| mail.uid).collect::<Vec<_>>(),
            vec![2, 3]
        );

        let mut full_uid_client = RemoteImapClient {
            config,
            session: Some(ImapSession::with_mock_stream(MockStream::with_reads(
                concat!(
                    "* OK [UIDVALIDITY 11] valid\r\n",
                    "* OK [UIDNEXT 10] next\r\n",
                    "* OK [HIGHESTMODSEQ 22] modseq\r\n",
                    "A0001 OK select\r\n",
                    "* 1 FETCH (UID 9 FLAGS (\\Seen) MODSEQ (22) BODY.PEEK[] {4}\r\n",
                    "full\r\n",
                    ")\r\n",
                    "A0002 OK fetch\r\n"
                )
                .as_bytes(),
            ))),
        };
        let full = full_uid_client
            .fetch_full_uids("INBOX", &[9])
            .expect("delegate full UID fetch");
        assert_eq!(
            full.iter().map(|mail| mail.uid).collect::<Vec<_>>(),
            vec![9]
        );
    }

    #[test]
    fn parses_lore_atom_entries() {
        let xml = r#"
<feed xmlns=\"http://www.w3.org/2005/Atom\">
  <entry>
    <id>https://lore.kernel.org/io-uring/msg-a/</id>
    <updated>2026-03-03T09:00:00+00:00</updated>
    <link rel=\"alternate\" href=\"https://lore.kernel.org/io-uring/msg-a/\" />
  </entry>
  <entry>
    <id>https://lore.kernel.org/io-uring/msg-b/</id>
    <published>2026-03-03T10:00:00+00:00</published>
    <link rel=\"alternate\" href=\"https://lore.kernel.org/io-uring/msg-b/\" />
  </entry>
</feed>
"#;

        let entries = parse_lore_atom_entries(xml).expect("parse atom");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].message_url,
            "https://lore.kernel.org/io-uring/msg-a/"
        );
        assert_eq!(
            entries[1].message_url,
            "https://lore.kernel.org/io-uring/msg-b/"
        );
        assert!(entries[1].modseq > entries[0].modseq);
    }

    #[test]
    fn builds_lore_raw_candidates() {
        let candidates = lore_raw_url_candidates("https://lore.kernel.org/io-uring/abc123/");
        assert!(candidates.iter().any(|url| url.ends_with("/raw")));
    }

    #[test]
    fn parses_atom_timestamps() {
        let ts = parse_atom_timestamp("2026-03-03T10:00:00+00:00").expect("timestamp");
        assert!(ts > 0);
    }

    #[test]
    fn parsing_helpers_cover_url_normalization_dates_and_flags() {
        assert_eq!(quote_imap_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(
            normalize_lore_message_url("https://lore.kernel.org/io-uring/msg/#fragment?query=1")
                .as_deref(),
            Some("https://lore.kernel.org/io-uring/msg/")
        );
        assert_eq!(normalize_lore_message_url("not-a-url"), None);

        assert_eq!(parse_year_month_key("2026-03"), Some((2026, 3)));
        assert_eq!(parse_year_month_key("2026-13"), None);
        assert!(parse_gnu_archive_listing_timestamp("2026-03-07", "06:37").is_some());

        assert_eq!(
            parse_flags(
                b"Message-ID: <x@example.com>\r\nX-Flags: Seen,\r\n Flagged Answered\r\n\r\nbody\r\n"
            ),
            vec![
                "Seen".to_string(),
                "Flagged".to_string(),
                "Answered".to_string()
            ]
        );
    }

    #[test]
    fn parses_gnu_archive_month_entries() {
        let html = r#"
<pre>
<img src="/icons/unknown.gif" alt="[   ]"> <a href="2026-02">2026-02</a>                 2026-02-26 09:12  855K
<img src="/icons/unknown.gif" alt="[   ]"> <a href="2026-03">2026-03</a>                 2026-03-07 06:37  341K
</pre>
"#;

        let entries = parse_gnu_archive_month_entries(html).expect("parse month entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].month_key, "2026-02");
        assert_eq!(entries[1].month_key, "2026-03");
        assert!(entries[1].modseq >= entries[0].modseq);
    }

    #[test]
    fn selects_latest_gnu_archive_month_and_recent_history() {
        let months = vec![
            GnuArchiveMonthEntry {
                month_key: "2026-01".to_string(),
                modseq: 10,
            },
            GnuArchiveMonthEntry {
                month_key: "2026-02".to_string(),
                modseq: 20,
            },
            GnuArchiveMonthEntry {
                month_key: "2026-03".to_string(),
                modseq: 30,
            },
        ];

        let initial = select_gnu_archive_months(&months, None);
        assert_eq!(
            initial
                .iter()
                .map(|entry| entry.month_key.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-02", "2026-03"]
        );

        let incremental = select_gnu_archive_months(&months, Some(30));
        assert_eq!(
            incremental
                .iter()
                .map(|entry| entry.month_key.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-03"]
        );
    }

    #[test]
    fn parses_gnu_archive_mbox_messages() {
        let raw = b"From MAILER-DAEMON Tue Mar 03 04:39:31 2026\nMessage-ID: <msg-a@example.com>\nSubject: one\n\nbody\n>From escaped\nFrom MAILER-DAEMON Tue Mar 03 04:40:31 2026\nMessage-ID: <msg-b@example.com>\nSubject: two\n\nbody two\n";

        let messages = parse_gnu_archive_mbox_messages(raw);
        assert_eq!(messages.len(), 2);
        assert!(String::from_utf8_lossy(&messages[0]).contains("Message-ID: <msg-a@example.com>"));
        assert!(String::from_utf8_lossy(&messages[0]).contains("\nFrom escaped\n"));
        assert!(String::from_utf8_lossy(&messages[1]).contains("Message-ID: <msg-b@example.com>"));
    }

    #[test]
    fn assigns_stable_gnu_archive_uids() {
        assert_eq!(gnu_archive_message_uid("2026-03", 0), 314_000_001);
        assert_eq!(gnu_archive_message_uid("2026-03", 41), 314_000_042);
    }
}
