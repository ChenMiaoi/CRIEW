//! Ratatui-based interface for CRIEW's interactive workflow.
//!
//! Rendering details are split into submodules, but the top-level state
//! machine stays here so key handling, background work, and side effects remain
//! readable in one place.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Stdout};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::app::cli::ReviewInboxMode;
use crate::app::patch as patch_worker;
use crate::app::review as review_worker;
use crate::app::sync as sync_worker;
use crate::domain::following::FollowingKind;
use crate::domain::subscriptions::{
    DEFAULT_SUBSCRIPTIONS, SubscriptionCategory, category_for_mailbox,
};
use crate::infra::bootstrap::BootstrapState;
use crate::infra::config::{FOLLOWING_VIEW, IMAP_INBOX_MAILBOX, RuntimeConfig};
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::mail_store::{self, ThreadRow};
use crate::infra::reply_store::{
    self, ReplyDraft, ReplyDraftRequest, ReplyDraftStatus, ReplySendRecordRequest, ReplySendStatus,
};
use crate::infra::sendmail::{self, SendOutcome, SendRequest, SendStatus};
use crate::infra::ui_state::{self, SavedView, UiState};
use chrono::Utc;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};

mod config;
mod input;
mod keymap;
mod palette;
mod preview;
mod query;
mod render;
mod reply;
#[cfg(test)]
mod tests;

use input::{LoopAction, handle_key_event};
use keymap::{
    KeymapEditorState, PendingMainPageSequenceState, ResolvedMainPageKeymap, draw_keymap_editor,
    handle_keymap_editor_key_event, handle_main_page_key_event, main_page_focus_shortcuts,
    main_page_move_shortcuts, main_page_navigation_shortcuts, resolve_active_main_page_keymap,
};
use palette::short_commit_id;
#[cfg(test)]
use palette::{is_palette_open_shortcut, is_palette_toggle, resolve_palette_local_workdir};
#[cfg(test)]
use render::{
    code_edit_cursor_position, load_source_file_preview, mail_page_panes, sanitize_inline_ui_text,
    thread_line,
};
use render::{draw, subscription_line};

use preview::{MailPreview, extract_mail_body_text, load_mail_preview};
#[cfg(test)]
use preview::{extract_mail_body_preview, extract_mail_preview};
use query::parse_query;
use reply::{
    PreparedReplyMessage, ReplyIdentity, ReplyPreview, ReplyPreviewLine, ReplyPreviewRequest,
    ReplySeed, build_reply_seed, prepare_reply_message, render_reply_preview,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Subscriptions,
    Threads,
    Preview,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Self::Subscriptions => "Subscriptions",
            Self::Threads => "Threads",
            Self::Preview => "Preview",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Subscriptions => Self::Threads,
            Self::Threads => Self::Preview,
            Self::Preview => Self::Subscriptions,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Subscriptions => Self::Preview,
            Self::Threads => Self::Subscriptions,
            Self::Preview => Self::Threads,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MailPaneLayout {
    subscriptions_width: u16,
    preview_width: u16,
}

impl Default for MailPaneLayout {
    fn default() -> Self {
        Self {
            subscriptions_width: ui_state::DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH,
            preview_width: ui_state::DEFAULT_MAIL_PREVIEW_WIDTH,
        }
    }
}

impl MailPaneLayout {
    fn from_persisted(state: Option<&UiState>) -> Self {
        let defaults = Self::default();
        Self {
            subscriptions_width: state
                .map(|persisted| persisted.mail_subscriptions_width)
                .unwrap_or(defaults.subscriptions_width)
                .max(MIN_MAIL_SUBSCRIPTIONS_WIDTH),
            preview_width: state
                .map(|persisted| persisted.mail_preview_width)
                .unwrap_or(defaults.preview_width)
                .max(MIN_MAIL_PREVIEW_WIDTH),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HorizontalResizeDirection {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailPaneResizeMode {
    Expand,
    Shrink,
}

#[derive(Debug, Clone, Copy)]
struct PaletteCommand {
    name: &'static str,
    description: &'static str,
}

#[derive(Debug, Clone)]
struct PaletteSuggestion {
    value: String,
    description: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalCommandResult {
    command: String,
    cwd: PathBuf,
    exit_code: String,
    output: String,
}

#[derive(Debug, Clone)]
struct LastApplySnapshot {
    thread_id: i64,
    before_head: String,
    after_head: String,
}

const PALETTE_COMMANDS: &[PaletteCommand] = &[
    PaletteCommand {
        name: "compose",
        description: "Compose a new message",
    },
    PaletteCommand {
        name: "quit",
        description: "Exit CRIEW",
    },
    PaletteCommand {
        name: "exit",
        description: "Exit CRIEW",
    },
    PaletteCommand {
        name: "help",
        description: "Show available commands",
    },
    PaletteCommand {
        name: "restart",
        description: "Restart TUI with startup config",
    },
    PaletteCommand {
        name: "sync",
        description: "Sync mailbox now",
    },
    PaletteCommand {
        name: "fetch-thread",
        description: "Fetch the complete thread for a Message-ID",
    },
    PaletteCommand {
        name: "forward",
        description: "Forward the selected message",
    },
    PaletteCommand {
        name: "review-inbox",
        description: "Show patch series that need review or have reviews",
    },
    PaletteCommand {
        name: "preflight",
        description: "Run checkpatch and maintainer checks for the selected patch series",
    },
    PaletteCommand {
        name: "outbox",
        description: "List persisted reply drafts and failed sends",
    },
    PaletteCommand {
        name: "config",
        description: "Open visual config editor or update runtime config",
    },
    PaletteCommand {
        name: "keymap",
        description: "Open visual keymap editor for main-page navigation",
    },
    PaletteCommand {
        name: "vim",
        description: "Open selected source file in external vim",
    },
    PaletteCommand {
        name: "view",
        description: "List, save, apply, or delete saved thread views",
    },
];

const PALETTE_SYNC_RECONNECT_ATTEMPTS: u8 = 3;
const MAIL_PANE_RESIZE_STEP: u16 = 4;
const MIN_MAIL_PREVIEW_WIDTH: u16 = 40;
const MIN_MAIL_SUBSCRIPTIONS_WIDTH: u16 = 12;
const PREVIEW_TAB_SPACES: &str = "    ";
const PREVIEW_RECIPIENT_PREVIEW_LIMIT: usize = 2;
const THREAD_LINE_MAX_CHARS: usize = 120;
const KERNEL_TREE_MAX_ROWS: usize = 2048;
const CODE_PREVIEW_MAX_BYTES: usize = 256 * 1024;
const CODE_PREVIEW_MAX_LINES: usize = 800;
const FOLLOWING_LABEL: &str = "Following";
// Kept as a source-level compatibility alias for existing TUI tests and
// helper code; the visible subscription is now named Following.
#[cfg(test)]
const MY_INBOX_LABEL: &str = FOLLOWING_LABEL;
const CONFIG_GET_KEYS: &[&str] = &[
    "config.path",
    "storage.data_dir",
    "storage.database",
    "storage.raw_mail_dir",
    "storage.patch_dir",
    "logging.dir",
    "logging.filter",
    "b4.path",
    "source.mailbox",
    "imap.mailbox",
    "imap.email",
    "imap.user",
    "imap.pass",
    "imap.server",
    "imap.serverport",
    "imap.encryption",
    "imap.proxy",
    "source.lore_base_url",
    "ui.startup_sync",
    "ui.keymap",
    "ui.keymap_base",
    "ui.inbox_auto_sync_interval_secs",
    "kernel.tree",
    "kernel.trees",
];
const CONFIG_SET_KEYS: &[&str] = &[
    "storage.data_dir",
    "storage.database",
    "storage.raw_mail_dir",
    "storage.patch_dir",
    "logging.dir",
    "logging.filter",
    "b4.path",
    "source.mailbox",
    "imap.mailbox",
    "imap.email",
    "imap.user",
    "imap.pass",
    "imap.server",
    "imap.serverport",
    "imap.encryption",
    "imap.proxy",
    "source.lore_base_url",
    "ui.startup_sync",
    "ui.keymap",
    "ui.keymap_base",
    "ui.inbox_auto_sync_interval_secs",
    "kernel.tree",
    "kernel.trees",
];
const CONFIG_EDITOR_FIELDS: &[ConfigEditorField] = &[
    ConfigEditorField {
        key: "source.mailbox",
        description: "Default lore mailbox used when sync runs without an explicit mailbox.",
    },
    ConfigEditorField {
        key: "source.lore_base_url",
        description: "Base URL used for lore links and message lookups.",
    },
    ConfigEditorField {
        key: "ui.startup_sync",
        description: "Whether enabled subscriptions start syncing automatically after TUI launch.",
    },
    ConfigEditorField {
        key: "ui.keymap",
        description: "Main-page navigation scheme. default=j/l+i/k+count, vim=h/l+j/k+count+gg/G+qq, custom=ui.keymap_base plus ui.custom_keymap overrides.",
    },
    ConfigEditorField {
        key: "ui.inbox_auto_sync_interval_secs",
        description: "Seconds between Following background auto-sync runs while TUI stays open.",
    },
    ConfigEditorField {
        key: "logging.filter",
        description: "Tracing/logging filter level for CRIEW runtime logs.",
    },
    ConfigEditorField {
        key: "logging.dir",
        description: "Directory where CRIEW writes runtime log files.",
    },
    ConfigEditorField {
        key: "storage.data_dir",
        description: "Runtime data root used for db, mail cache, patches and UI state defaults.",
    },
    ConfigEditorField {
        key: "storage.database",
        description: "SQLite database path used for synced mail and patch metadata.",
    },
    ConfigEditorField {
        key: "storage.raw_mail_dir",
        description: "Directory where raw downloaded .eml files are stored.",
    },
    ConfigEditorField {
        key: "storage.patch_dir",
        description: "Directory where downloaded patch files are written.",
    },
    ConfigEditorField {
        key: "b4.path",
        description: "Optional explicit path to the b4 executable or wrapper script.",
    },
    ConfigEditorField {
        key: "imap.email",
        description: "Self email address for matching your own mail; also used as login when imap.user is omitted.",
    },
    ConfigEditorField {
        key: "imap.user",
        description: "IMAP login account. Gmail usually expects the full email address.",
    },
    ConfigEditorField {
        key: "imap.pass",
        description: "IMAP login password or app password.",
    },
    ConfigEditorField {
        key: "imap.server",
        description: "IMAP server host name.",
    },
    ConfigEditorField {
        key: "imap.serverport",
        description: "IMAP server port number.",
    },
    ConfigEditorField {
        key: "imap.encryption",
        description: "Connection security mode. Gmail 993 uses ssl/tls here.",
    },
    ConfigEditorField {
        key: "imap.proxy",
        description: "Optional proxy URL for IMAP. Supports http://, socks5:// and socks5h://.",
    },
    ConfigEditorField {
        key: "kernel.tree",
        description: "Single kernel tree root shown in the code browser pane.",
    },
    ConfigEditorField {
        key: "kernel.trees",
        description: "Multiple kernel tree roots, written as a TOML array of paths.",
    },
];
const CODE_EDIT_ENTRY_HINT: &str = "select a source file in Source pane, then press e";
const EXTERNAL_EDITOR_ENTRY_HINT: &str = "select a source file in Source pane, then press E";

fn shrink_mail_pane_width(width: &mut u16, minimum_width: u16) -> bool {
    if *width <= minimum_width {
        return false;
    }

    *width = width
        .saturating_sub(MAIL_PANE_RESIZE_STEP)
        .max(minimum_width);
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExternalEditorProcessResult {
    success: bool,
    exit_code: Option<i32>,
}

type ExternalEditorRunner =
    fn(&str, &Path) -> std::result::Result<ExternalEditorProcessResult, String>;
type ReplyIdentityResolver = fn() -> std::result::Result<ReplyIdentity, String>;
type ReplySendExecutor = fn(&RuntimeConfig, &SendRequest) -> SendOutcome;
type MailboxSyncSpawner = fn(RuntimeConfig, Vec<String>) -> Receiver<StartupSyncEvent>;
type ThreadFetchSpawner = fn(RuntimeConfig, String, String) -> Receiver<ThreadFetchEvent>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingMainPageCountState {
    count: u16,
    ui_page: UiPage,
    focus: Pane,
    code_focus: CodePaneFocus,
}

#[derive(Debug, Clone)]
enum StartupSyncEvent {
    MailboxStarted {
        mailbox: String,
        index: usize,
        total: usize,
    },
    MailboxFinished {
        mailbox: String,
        fetched: usize,
        inserted: usize,
        updated: usize,
    },
    MailboxFailed {
        mailbox: String,
        error: String,
    },
    WorkerCompleted,
}

#[derive(Debug, Clone)]
enum ThreadFetchEvent {
    Finished {
        mailbox: String,
        message_id: String,
        fetched: usize,
        inserted: usize,
        updated: usize,
    },
    Failed {
        mailbox: String,
        message_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupSyncMailboxStatus {
    Pending,
    InFlight,
    Finished,
    Failed,
}

impl StartupSyncMailboxStatus {
    fn ui_suffix(self) -> &'static str {
        match self {
            Self::Pending => " [queued]",
            Self::InFlight => " [sync]",
            Self::Finished => " [done]",
            Self::Failed => " [failed]",
        }
    }

    fn log_label(self) -> &'static str {
        match self {
            Self::Pending => "queued",
            Self::InFlight => "syncing",
            Self::Finished => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
struct StartupSyncState {
    receiver: Receiver<StartupSyncEvent>,
    mailbox_order: Vec<String>,
    mailboxes: HashMap<String, StartupSyncMailboxStatus>,
    total: usize,
    completed: usize,
    succeeded: usize,
    failed: usize,
}

impl StartupSyncState {
    fn pending_count(&self) -> usize {
        self.mailbox_order
            .iter()
            .filter(|mailbox| {
                matches!(
                    self.mailboxes.get(mailbox.as_str()),
                    Some(StartupSyncMailboxStatus::Pending)
                )
            })
            .count()
    }

    fn inflight_mailboxes_display(&self) -> String {
        let running: Vec<&str> = self
            .mailbox_order
            .iter()
            .filter_map(|mailbox| {
                matches!(
                    self.mailboxes.get(mailbox.as_str()),
                    Some(StartupSyncMailboxStatus::InFlight)
                )
                .then_some(mailbox.as_str())
            })
            .collect();
        if running.is_empty() {
            "-".to_string()
        } else {
            running.join(",")
        }
    }

    fn mailbox_states_display(&self) -> String {
        self.mailbox_order
            .iter()
            .map(|mailbox| {
                let status = self
                    .mailboxes
                    .get(mailbox.as_str())
                    .copied()
                    .unwrap_or(StartupSyncMailboxStatus::Pending);
                format!("{mailbox}:{}", status.log_label())
            })
            .collect::<Vec<String>>()
            .join(" ")
    }

    #[cfg(test)]
    fn progress_summary(&self) -> String {
        format!(
            "{}/{} ok={} fail={} queued={} running={}",
            self.completed,
            self.total,
            self.succeeded,
            self.failed,
            self.pending_count(),
            self.inflight_mailboxes_display()
        )
    }
}

#[derive(Debug)]
struct InboxAutoSyncState {
    receiver: Option<Receiver<StartupSyncEvent>>,
    next_due_at: Instant,
}

impl InboxAutoSyncState {
    fn new(next_due_at: Instant) -> Self {
        Self {
            receiver: None,
            next_due_at,
        }
    }

    fn in_flight(&self) -> bool {
        self.receiver.is_some()
    }
}

#[derive(Debug, Clone, Copy)]
enum ManualSyncOrigin {
    PaletteCommand,
    SubscriptionOpen,
}

impl ManualSyncOrigin {
    fn log_label(self) -> &'static str {
        match self {
            Self::PaletteCommand => "palette",
            Self::SubscriptionOpen => "subscription_open",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualSyncRequestOutcome {
    Started,
    AlreadySyncing,
    Busy,
}

#[derive(Debug)]
struct ManualSyncState {
    receiver: Receiver<StartupSyncEvent>,
    mailbox_order: Vec<String>,
    mailboxes: HashMap<String, StartupSyncMailboxStatus>,
    total: usize,
    completed: usize,
    succeeded: usize,
    failed: usize,
    total_fetched: usize,
    total_inserted: usize,
    total_updated: usize,
    first_error: Option<String>,
}

impl ManualSyncState {
    fn pending_count(&self) -> usize {
        self.mailbox_order
            .iter()
            .filter(|mailbox| {
                matches!(
                    self.mailboxes.get(mailbox.as_str()),
                    Some(StartupSyncMailboxStatus::Pending)
                )
            })
            .count()
    }

    fn inflight_mailboxes_display(&self) -> String {
        let running: Vec<&str> = self
            .mailbox_order
            .iter()
            .filter_map(|mailbox| {
                matches!(
                    self.mailboxes.get(mailbox.as_str()),
                    Some(StartupSyncMailboxStatus::InFlight)
                )
                .then_some(mailbox.as_str())
            })
            .collect();
        if running.is_empty() {
            "-".to_string()
        } else {
            running.join(",")
        }
    }

    fn progress_summary(&self) -> String {
        format!(
            "{}/{} ok={} fail={} queued={} running={}",
            self.completed,
            self.total,
            self.succeeded,
            self.failed,
            self.pending_count(),
            self.inflight_mailboxes_display()
        )
    }

    fn mailbox_states_display(&self) -> String {
        self.mailbox_order
            .iter()
            .map(|mailbox| {
                let status = self
                    .mailboxes
                    .get(mailbox.as_str())
                    .copied()
                    .unwrap_or(StartupSyncMailboxStatus::Pending);
                format!("{mailbox}:{}", status.log_label())
            })
            .collect::<Vec<String>>()
            .join(" ")
    }
}

#[derive(Debug)]
struct SubscriptionAutoSyncState {
    receiver: Option<Receiver<StartupSyncEvent>>,
    next_due_at: Instant,
    in_flight_mailboxes: HashSet<String>,
}

#[derive(Debug)]
struct ThreadFetchState {
    receiver: Receiver<ThreadFetchEvent>,
    mailbox: String,
    message_id: String,
}

impl SubscriptionAutoSyncState {
    fn new(next_due_at: Instant) -> Self {
        Self {
            receiver: None,
            next_due_at,
            in_flight_mailboxes: HashSet::new(),
        }
    }

    fn in_flight(&self) -> bool {
        self.receiver.is_some()
    }

    fn mailbox_in_flight(&self, mailbox: &str) -> bool {
        self.in_flight_mailboxes
            .iter()
            .any(|in_flight| same_mailbox_name(in_flight, mailbox))
    }

    fn inflight_mailboxes_display(&self) -> String {
        if self.in_flight_mailboxes.is_empty() {
            "-".to_string()
        } else {
            let mut mailboxes: Vec<&str> = self
                .in_flight_mailboxes
                .iter()
                .map(|mailbox| mailbox.as_str())
                .collect();
            mailboxes.sort_unstable();
            mailboxes.join(",")
        }
    }
}

#[derive(Debug, Default)]
struct CommandPaletteState {
    open: bool,
    input: String,
    suggestions: Vec<PaletteSuggestion>,
    show_suggestions: bool,
    last_tab_input: String,
    last_local_result: Option<LocalCommandResult>,
    local_command_completion_cache: LocalCommandCompletionCache,
}

impl CommandPaletteState {
    fn clear_completion(&mut self) {
        self.suggestions.clear();
        self.show_suggestions = false;
        self.last_tab_input.clear();
    }

    fn clear_local_result(&mut self) {
        self.last_local_result = None;
    }
}

#[derive(Debug, Default)]
struct LocalCommandCompletionCache {
    is_loaded: bool,
    path_value: Option<OsString>,
    suggestions: Vec<PaletteSuggestion>,
}

#[derive(Debug, Default)]
struct SearchState {
    active: bool,
    input: String,
    applied_query: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ConfigEditorMode {
    #[default]
    Browse,
    Edit,
}

#[derive(Debug, Default)]
struct ConfigEditorState {
    open: bool,
    selected_field: usize,
    mode: ConfigEditorMode,
    input: String,
}

#[derive(Debug, Clone, Copy)]
struct ConfigEditorField {
    key: &'static str,
    description: &'static str,
}

#[derive(Debug, Clone)]
struct SubscriptionItem {
    mailbox: String,
    label: String,
    enabled: bool,
    category: Option<SubscriptionCategory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionSection {
    Enabled,
    Disabled,
}

impl SubscriptionSection {
    const fn label(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionRowKind {
    EnabledHeader,
    DisabledHeader,
    CategoryHeader {
        section: SubscriptionSection,
        category: SubscriptionCategory,
    },
    Item(usize),
}

#[derive(Debug, Clone)]
struct SubscriptionRow {
    kind: SubscriptionRowKind,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiPage {
    Mail,
    CodeBrowser,
}

impl UiPage {
    fn toggled(self) -> Self {
        match self {
            Self::Mail => Self::CodeBrowser,
            Self::CodeBrowser => Self::Mail,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodePaneFocus {
    Tree,
    Source,
}

impl CodePaneFocus {
    fn next(self) -> Self {
        match self {
            Self::Tree => Self::Source,
            Self::Source => Self::Tree,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Tree => Self::Source,
            Self::Source => Self::Tree,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeEditMode {
    Browse,
    VimNormal,
    VimInsert,
    VimCommand,
}

impl CodeEditMode {
    fn is_active(self) -> bool {
        !matches!(self, Self::Browse)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Browse => "BROWSE",
            Self::VimNormal => "NORMAL",
            Self::VimInsert => "INSERT",
            Self::VimCommand => "COMMAND",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyEditMode {
    Normal,
    Insert,
    Command,
}

impl ReplyEditMode {
    fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Command => "COMMAND",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplySection {
    From,
    To,
    Cc,
    Subject,
    Body,
}

impl ReplySection {
    fn label(self) -> &'static str {
        match self {
            Self::From => "From",
            Self::To => "To",
            Self::Cc => "Cc",
            Self::Subject => "Subject",
            Self::Body => "Body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyNoticeKind {
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyNoticeAction {
    OpenPreview,
    Send,
}

#[derive(Debug, Clone)]
struct ReplyNoticeState {
    kind: ReplyNoticeKind,
    title: String,
    message: String,
    hint: String,
    action: Option<ReplyNoticeAction>,
}

#[derive(Debug, Clone)]
struct ReplyPanelState {
    thread_id: i64,
    mail_id: i64,
    from: String,
    to: String,
    cc: String,
    subject: String,
    in_reply_to: String,
    references: Vec<String>,
    body: Vec<String>,
    self_addresses: Vec<String>,
    mode: ReplyEditMode,
    section: ReplySection,
    body_row: usize,
    cursor_col: usize,
    dirty: bool,
    scroll: u16,
    command_input: String,
    preview_open: bool,
    preview_scroll: u16,
    preview_rendered: String,
    preview_lines: Vec<ReplyPreviewLine>,
    preview_errors: Vec<String>,
    preview_warnings: Vec<String>,
    preview_confirmed: bool,
    preview_confirmed_at: Option<String>,
    reply_notice: Option<ReplyNoticeState>,
    draft_status: ReplyDraftStatus,
    draft_path: Option<PathBuf>,
    last_error: Option<String>,
}

impl ReplyPanelState {
    fn new(seed: ReplySeed, self_addresses: Vec<String>, mail_id: i64, thread_id: i64) -> Self {
        let mut state = Self {
            thread_id,
            mail_id,
            from: seed.from,
            to: seed.to,
            cc: seed.cc,
            subject: seed.subject,
            in_reply_to: seed.in_reply_to,
            references: seed.references,
            body: if seed.body.is_empty() {
                vec![String::new()]
            } else {
                seed.body
            },
            self_addresses,
            mode: ReplyEditMode::Normal,
            section: ReplySection::From,
            body_row: 0,
            cursor_col: 0,
            dirty: false,
            scroll: 0,
            command_input: String::new(),
            preview_open: false,
            preview_scroll: 0,
            preview_rendered: String::new(),
            preview_lines: Vec::new(),
            preview_errors: Vec::new(),
            preview_warnings: Vec::new(),
            preview_confirmed: false,
            preview_confirmed_at: None,
            reply_notice: None,
            draft_status: ReplyDraftStatus::Draft,
            draft_path: None,
            last_error: None,
        };
        state.clamp_cursor();
        state
    }

    fn from_draft(draft: ReplyDraft, self_addresses: Vec<String>) -> Self {
        let mut state = Self {
            thread_id: draft.thread_id,
            mail_id: draft.mail_id,
            from: draft.from_addr,
            to: draft.to_addrs,
            cc: draft.cc_addrs,
            subject: draft.subject,
            in_reply_to: draft.in_reply_to,
            references: draft.references,
            body: if draft.body.is_empty() {
                vec![String::new()]
            } else {
                draft.body
            },
            self_addresses,
            mode: ReplyEditMode::Normal,
            section: ReplySection::From,
            body_row: 0,
            cursor_col: 0,
            dirty: false,
            scroll: 0,
            command_input: String::new(),
            preview_open: false,
            preview_scroll: 0,
            preview_rendered: String::new(),
            preview_lines: Vec::new(),
            preview_errors: Vec::new(),
            preview_warnings: Vec::new(),
            // A persisted preview confirmation must not authorize an
            // unattended send after a restart; the user reviews it again.
            preview_confirmed: false,
            preview_confirmed_at: None,
            reply_notice: None,
            draft_status: draft.status,
            draft_path: draft.draft_path,
            last_error: draft.last_error,
        };
        state.clamp_cursor();
        state
    }

    fn to_draft_request(&self) -> ReplyDraftRequest {
        ReplyDraftRequest {
            thread_id: self.thread_id,
            mail_id: self.mail_id,
            from_addr: self.from.clone(),
            to_addrs: self.to.clone(),
            cc_addrs: self.cc.clone(),
            subject: self.subject.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            body: self.body.clone(),
            preview_confirmed_at: self.preview_confirmed_at.clone(),
            status: self.draft_status,
            draft_path: self.draft_path.clone(),
            last_error: self.last_error.clone(),
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.preview_confirmed = false;
        self.preview_confirmed_at = None;
        self.reply_notice = None;
        self.draft_status = ReplyDraftStatus::Draft;
        self.draft_path = None;
        self.last_error = None;
    }

    fn current_value(&self) -> &str {
        match self.section {
            ReplySection::From => self.from.as_str(),
            ReplySection::To => self.to.as_str(),
            ReplySection::Cc => self.cc.as_str(),
            ReplySection::Subject => self.subject.as_str(),
            ReplySection::Body => self
                .body
                .get(self.body_row)
                .map(String::as_str)
                .unwrap_or_default(),
        }
    }

    fn current_value_mut(&mut self) -> &mut String {
        match self.section {
            ReplySection::From => &mut self.from,
            ReplySection::To => &mut self.to,
            ReplySection::Cc => &mut self.cc,
            ReplySection::Subject => &mut self.subject,
            ReplySection::Body => {
                if self.body.is_empty() {
                    self.body.push(String::new());
                }
                let row = self.body_row.min(self.body.len().saturating_sub(1));
                &mut self.body[row]
            }
        }
    }

    fn current_value_len(&self) -> usize {
        self.current_value().chars().count()
    }

    fn clamp_cursor(&mut self) {
        if self.body.is_empty() {
            self.body.push(String::new());
        }
        if matches!(self.section, ReplySection::Body) && self.body_row >= self.body.len() {
            self.body_row = self.body.len().saturating_sub(1);
        }
        self.cursor_col = self.cursor_col.min(self.current_value_len());
    }

    fn current_body_logical_row(&self) -> usize {
        if matches!(self.mode, ReplyEditMode::Command) {
            return reply_command_line_logical_row(self);
        }

        reply_body_line_logical_row(self.body_row)
    }

    fn adjust_scroll(&mut self) {
        if matches!(self.mode, ReplyEditMode::Command) || matches!(self.section, ReplySection::Body)
        {
            let scroll_target = self.current_body_logical_row().saturating_sub(3);
            self.scroll = scroll_target.min(u16::MAX as usize) as u16;
        }
    }

    fn move_left(&mut self) {
        self.clamp_cursor();
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
        self.adjust_scroll();
    }

    fn move_right(&mut self) {
        self.clamp_cursor();
        if self.cursor_col < self.current_value_len() {
            self.cursor_col += 1;
        }
        self.adjust_scroll();
    }

    fn move_up(&mut self) {
        self.clamp_cursor();
        match self.section {
            ReplySection::From => {}
            ReplySection::To => self.section = ReplySection::From,
            ReplySection::Cc => self.section = ReplySection::To,
            ReplySection::Subject => self.section = ReplySection::Cc,
            ReplySection::Body => {
                if self.body_row > 0 {
                    self.body_row -= 1;
                } else {
                    self.section = ReplySection::Subject;
                }
            }
        }
        self.clamp_cursor();
        self.adjust_scroll();
    }

    fn move_down(&mut self) {
        self.clamp_cursor();
        match self.section {
            ReplySection::From => self.section = ReplySection::To,
            ReplySection::To => self.section = ReplySection::Cc,
            ReplySection::Cc => self.section = ReplySection::Subject,
            ReplySection::Subject => self.section = ReplySection::Body,
            ReplySection::Body => {
                if self.body_row + 1 < self.body.len() {
                    self.body_row += 1;
                }
            }
        }
        self.clamp_cursor();
        self.adjust_scroll();
    }

    fn insert_char(&mut self, character: char) {
        self.clamp_cursor();
        let cursor_col = self.cursor_col;
        let value = self.current_value_mut();
        let byte_index = char_to_byte_index(value, cursor_col);
        value.insert(byte_index, character);
        self.cursor_col += 1;
        self.mark_dirty();
        self.adjust_scroll();
    }

    fn backspace(&mut self) -> bool {
        self.clamp_cursor();

        if self.cursor_col > 0 {
            let cursor_col = self.cursor_col;
            let value = self.current_value_mut();
            let remove_at = cursor_col - 1;
            let start = char_to_byte_index(value, remove_at);
            let end = char_to_byte_index(value, remove_at + 1);
            value.replace_range(start..end, "");
            self.cursor_col -= 1;
            self.mark_dirty();
            self.adjust_scroll();
            return true;
        }

        if !matches!(self.section, ReplySection::Body) || self.body_row == 0 {
            return false;
        }

        let current = self.body.remove(self.body_row);
        self.body_row -= 1;
        let previous_len = self.body[self.body_row].chars().count();
        self.body[self.body_row].push_str(&current);
        self.cursor_col = previous_len;
        self.mark_dirty();
        self.adjust_scroll();
        true
    }

    fn delete_char(&mut self) -> bool {
        self.clamp_cursor();
        if matches!(self.section, ReplySection::Body) {
            let row = self.body_row.min(self.body.len().saturating_sub(1));
            let line_len = self.body[row].chars().count();
            if self.cursor_col < line_len {
                let value = &mut self.body[row];
                let start = char_to_byte_index(value, self.cursor_col);
                let end = char_to_byte_index(value, self.cursor_col + 1);
                value.replace_range(start..end, "");
                self.mark_dirty();
                self.adjust_scroll();
                return true;
            }
            if row + 1 < self.body.len() {
                let next = self.body.remove(row + 1);
                self.body[row].push_str(&next);
                self.mark_dirty();
                self.adjust_scroll();
                return true;
            }
            return false;
        }

        let line_len = self.current_value_len();
        if self.cursor_col >= line_len {
            return false;
        }
        let cursor_col = self.cursor_col;
        let value = self.current_value_mut();
        let start = char_to_byte_index(value, cursor_col);
        let end = char_to_byte_index(value, cursor_col + 1);
        value.replace_range(start..end, "");
        self.mark_dirty();
        self.adjust_scroll();
        true
    }

    fn insert_newline(&mut self) {
        self.clamp_cursor();
        if !matches!(self.section, ReplySection::Body) {
            self.move_down();
            self.cursor_col = self.current_value_len();
            return;
        }

        let row = self.body_row.min(self.body.len().saturating_sub(1));
        let original_line = self.body[row].clone();
        let byte_index = char_to_byte_index(&original_line, self.cursor_col);
        let tail = original_line[byte_index..].to_string();
        let head = original_line[..byte_index].to_string();

        self.body[row] = head;
        self.body.insert(row + 1, tail);
        self.body_row += 1;
        self.cursor_col = 0;
        self.mark_dirty();
        self.adjust_scroll();
    }

    fn open_line_below(&mut self) {
        self.clamp_cursor();
        if !matches!(self.section, ReplySection::Body) {
            self.move_down();
            self.cursor_col = self.current_value_len();
            return;
        }

        let insert_at = self.body_row.min(self.body.len().saturating_sub(1)) + 1;
        self.body.insert(insert_at, String::new());
        self.body_row = insert_at;
        self.cursor_col = 0;
        self.mark_dirty();
        self.adjust_scroll();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KernelTreeRowKind {
    RootDirectory,
    Directory,
    File,
    RootFile,
    MissingPath,
}

#[derive(Debug, Clone)]
struct KernelTreeRow {
    path: PathBuf,
    name: String,
    depth: usize,
    kind: KernelTreeRowKind,
    expandable: bool,
    expanded: bool,
}

impl KernelTreeRow {
    fn is_file(&self) -> bool {
        matches!(
            self.kind,
            KernelTreeRowKind::File | KernelTreeRowKind::RootFile
        )
    }

    fn display_text(&self) -> String {
        match self.kind {
            KernelTreeRowKind::RootDirectory => {
                let marker = if self.expandable {
                    if self.expanded { "▼" } else { "▶" }
                } else {
                    "•"
                };
                format!("{marker} [root] {}", self.path.display())
            }
            KernelTreeRowKind::Directory => {
                let marker = if self.expandable {
                    if self.expanded { "▼" } else { "▶" }
                } else {
                    "•"
                };
                format!("{}{} {}/", "  ".repeat(self.depth), marker, self.name)
            }
            KernelTreeRowKind::File => {
                format!("{}  {}", "  ".repeat(self.depth), self.name)
            }
            KernelTreeRowKind::RootFile => format!("[file] {}", self.path.display()),
            KernelTreeRowKind::MissingPath => format!("[missing] {}", self.path.display()),
        }
    }
}

#[derive(Debug, Default)]
struct KernelTreeDirectoryEntries {
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

impl KernelTreeDirectoryEntries {
    fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.files.is_empty()
    }

    fn into_children(self) -> impl Iterator<Item = PathBuf> {
        self.dirs.into_iter().chain(self.files)
    }
}

#[derive(Debug)]
struct AppState {
    runtime: RuntimeConfig,
    ui_state_path: PathBuf,
    active_thread_mailbox: String,
    imap_defaults_initialized: bool,
    ui_page: UiPage,
    focus: Pane,
    mail_pane_layout: MailPaneLayout,
    code_focus: CodePaneFocus,
    subscriptions: Vec<SubscriptionItem>,
    enabled_group_expanded: bool,
    disabled_group_expanded: bool,
    enabled_linux_subsystem_expanded: bool,
    enabled_qemu_subsystem_expanded: bool,
    disabled_linux_subsystem_expanded: bool,
    disabled_qemu_subsystem_expanded: bool,
    threads: Vec<ThreadRow>,
    self_email: Option<String>,
    following_kinds: HashMap<i64, Vec<FollowingKind>>,
    series_summaries: HashMap<i64, patch_worker::SeriesSummary>,
    review_summaries: HashMap<i64, review_worker::ReviewInboxEntry>,
    review_inbox_mode: Option<ReviewInboxMode>,
    filtered_thread_indices: Vec<usize>,
    subscription_index: usize,
    subscription_row_index: usize,
    kernel_tree_rows: Vec<KernelTreeRow>,
    kernel_tree_expanded_paths: HashSet<PathBuf>,
    kernel_tree_row_index: usize,
    code_preview_scroll: u16,
    code_preview_scroll_limit: Cell<u16>,
    code_edit_mode: CodeEditMode,
    code_edit_target: Option<PathBuf>,
    code_edit_buffer: Vec<String>,
    code_edit_cursor_row: usize,
    code_edit_cursor_col: usize,
    code_edit_dirty: bool,
    code_edit_command_input: String,
    reply_panel: Option<ReplyPanelState>,
    thread_index: usize,
    preview_scroll: u16,
    preview_scroll_limit: Cell<u16>,
    selected_mail_preview: Option<MailPreview>,
    started_at: Instant,
    status: String,
    last_apply_snapshot: Option<LastApplySnapshot>,
    palette: CommandPaletteState,
    search: SearchState,
    saved_views: Vec<SavedView>,
    config_editor: ConfigEditorState,
    keymap_editor: KeymapEditorState,
    external_editor_runner: ExternalEditorRunner,
    reply_identity_resolver: ReplyIdentityResolver,
    reply_send_executor: ReplySendExecutor,
    mailbox_sync_spawner: MailboxSyncSpawner,
    manual_sync_spawner: MailboxSyncSpawner,
    thread_fetch_spawner: ThreadFetchSpawner,
    needs_terminal_refresh: bool,
    startup_sync: Option<StartupSyncState>,
    inbox_auto_sync: Option<InboxAutoSyncState>,
    manual_sync: Option<ManualSyncState>,
    subscription_auto_sync: Option<SubscriptionAutoSyncState>,
    thread_fetch: Option<ThreadFetchState>,
    main_page_keymap: ResolvedMainPageKeymap,
    pending_main_page_sequence: Option<PendingMainPageSequenceState>,
    pending_main_page_count: Option<PendingMainPageCountState>,
}

impl AppState {
    fn new(threads: Vec<ThreadRow>, runtime: RuntimeConfig) -> Self {
        Self::new_with_ui_state(threads, runtime, None)
    }

    fn new_with_ui_state(
        threads: Vec<ThreadRow>,
        runtime: RuntimeConfig,
        persisted: Option<UiState>,
    ) -> Self {
        let ui_state_path = ui_state::path_for_data_dir(&runtime.data_dir);
        let mail_pane_layout = MailPaneLayout::from_persisted(persisted.as_ref());
        let persisted_imap_defaults_initialized = persisted
            .as_ref()
            .map(|state| state.imap_defaults_initialized)
            .unwrap_or(false);
        let enabled_mailboxes = persisted
            .as_ref()
            .map(UiState::normalized_enabled_mailboxes)
            .unwrap_or_default();
        let saved_views = persisted
            .as_ref()
            .map(UiState::normalized_saved_views)
            .unwrap_or_default();
        let enabled_mailboxes: HashSet<String> = enabled_mailboxes
            .into_iter()
            .map(|mailbox| canonical_view_mailbox(&mailbox))
            .collect();
        let active_thread_mailbox = persisted
            .as_ref()
            .and_then(|state| state.active_mailbox.as_ref())
            .map(|mailbox| mailbox.trim())
            .filter(|mailbox| !mailbox.is_empty())
            .map(canonical_view_mailbox)
            .unwrap_or_else(|| runtime.default_active_mailbox().to_string());
        let self_email = crate::infra::config::resolve_self_email(&runtime).email;
        let my_inbox_default = if runtime.imap.is_complete() && !persisted_imap_defaults_initialized
        {
            MyInboxDefault::EnableOnFirstOpen
        } else {
            MyInboxDefault::PreservePersistedChoice
        };
        let subscriptions = default_subscriptions(
            &runtime,
            &enabled_mailboxes,
            Some(active_thread_mailbox.as_str()),
            my_inbox_default,
        );
        let kernel_tree_expanded_paths = default_kernel_tree_expanded_paths(&runtime.kernel_trees);
        let kernel_tree_rows =
            build_kernel_tree_rows(&runtime.kernel_trees, &kernel_tree_expanded_paths);
        let main_page_keymap = resolve_active_main_page_keymap(&runtime);
        let mut state = Self {
            active_thread_mailbox,
            runtime,
            ui_state_path,
            imap_defaults_initialized: persisted_imap_defaults_initialized,
            ui_page: UiPage::Mail,
            focus: Pane::Subscriptions,
            mail_pane_layout,
            code_focus: CodePaneFocus::Tree,
            subscriptions,
            enabled_group_expanded: persisted
                .as_ref()
                .map(|state| state.enabled_group_expanded)
                .unwrap_or(true),
            disabled_group_expanded: persisted
                .as_ref()
                .map(|state| state.disabled_group_expanded)
                .unwrap_or(true),
            enabled_linux_subsystem_expanded: persisted
                .as_ref()
                .map(|state| state.enabled_linux_subsystem_expanded)
                .unwrap_or(true),
            enabled_qemu_subsystem_expanded: persisted
                .as_ref()
                .map(|state| state.enabled_qemu_subsystem_expanded)
                .unwrap_or(true),
            disabled_linux_subsystem_expanded: persisted
                .as_ref()
                .map(|state| state.disabled_linux_subsystem_expanded)
                .unwrap_or(true),
            disabled_qemu_subsystem_expanded: persisted
                .as_ref()
                .map(|state| state.disabled_qemu_subsystem_expanded)
                .unwrap_or(true),
            threads,
            self_email,
            following_kinds: HashMap::new(),
            series_summaries: HashMap::new(),
            review_summaries: HashMap::new(),
            review_inbox_mode: None,
            filtered_thread_indices: Vec::new(),
            subscription_index: 0,
            subscription_row_index: 0,
            kernel_tree_rows,
            kernel_tree_expanded_paths,
            kernel_tree_row_index: 0,
            code_preview_scroll: 0,
            code_preview_scroll_limit: Cell::new(u16::MAX),
            code_edit_mode: CodeEditMode::Browse,
            code_edit_target: None,
            code_edit_buffer: Vec::new(),
            code_edit_cursor_row: 0,
            code_edit_cursor_col: 0,
            code_edit_dirty: false,
            code_edit_command_input: String::new(),
            reply_panel: None,
            thread_index: 0,
            preview_scroll: 0,
            preview_scroll_limit: Cell::new(u16::MAX),
            selected_mail_preview: None,
            started_at: Instant::now(),
            status: String::new(),
            last_apply_snapshot: None,
            palette: CommandPaletteState::default(),
            search: SearchState::default(),
            saved_views,
            config_editor: ConfigEditorState::default(),
            keymap_editor: KeymapEditorState::default(),
            external_editor_runner: run_external_editor_session,
            reply_identity_resolver: resolve_git_reply_identity,
            reply_send_executor: send_reply_message,
            mailbox_sync_spawner: spawn_startup_sync_worker,
            manual_sync_spawner: spawn_startup_sync_worker,
            thread_fetch_spawner: spawn_thread_fetch_worker,
            needs_terminal_refresh: false,
            startup_sync: None,
            inbox_auto_sync: None,
            manual_sync: None,
            subscription_auto_sync: None,
            thread_fetch: None,
            main_page_keymap,
            pending_main_page_sequence: None,
            pending_main_page_count: None,
        };
        if state.runtime.imap.is_complete() {
            state.imap_defaults_initialized = true;
        }
        if let Some(index) = state
            .subscriptions
            .iter()
            .position(|item| same_mailbox_name(&item.mailbox, &state.active_thread_mailbox))
        {
            state.subscription_index = index;
        }
        state.refresh_following_kinds();
        state.refresh_series_summaries();
        state.refresh_review_summaries();
        state.apply_thread_filter();
        state.sync_subscription_row_to_selected_item();
        state.reconcile_inbox_auto_sync();
        state.reconcile_subscription_auto_sync();
        state
    }

    fn apply_thread_filter(&mut self) {
        let query = match parse_query(&self.search.applied_query) {
            Ok(query) => query,
            Err(error) => {
                self.filtered_thread_indices.clear();
                self.thread_index = 0;
                self.refresh_selected_mail_preview();
                self.status = format!("search error: {error}");
                return;
            }
        };

        self.filtered_thread_indices = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let review_matches = self.review_inbox_mode.is_none_or(|mode| {
                    self.review_summaries
                        .get(&row.thread_id)
                        .is_some_and(|summary| summary.matches_mode(mode))
                });
                let review_status = self
                    .review_summaries
                    .get(&row.thread_id)
                    .map(|summary| summary.status_label());
                if review_matches && query.matches(row, review_status) {
                    Some(index)
                } else {
                    None
                }
            })
            .collect();

        if self.thread_index >= self.filtered_thread_indices.len() {
            self.thread_index = self.filtered_thread_indices.len().saturating_sub(1);
        }

        if !query.is_empty() {
            self.status = format!(
                "search '{}': {} matches",
                self.search.applied_query,
                self.filtered_thread_indices.len()
            );
        }

        self.refresh_selected_mail_preview();
    }

    fn replace_threads(&mut self, threads: Vec<ThreadRow>) {
        self.threads = threads;
        self.refresh_following_kinds();
        self.refresh_series_summaries();
        self.refresh_review_summaries();
        self.thread_index = 0;
        self.preview_scroll = 0;
        self.apply_thread_filter();
    }

    fn replace_threads_preserving_selection(&mut self, threads: Vec<ThreadRow>) {
        let selected_message_id = self.selected_thread().map(|row| row.message_id.clone());
        let selected_thread_id = self.selected_thread().map(|row| row.thread_id);
        let selected_preview_scroll = self.preview_scroll;

        self.replace_threads(threads);

        if let Some(message_id) = selected_message_id
            && let Some(position) = self.filtered_thread_indices.iter().position(|index| {
                self.threads
                    .get(*index)
                    .is_some_and(|row| row.message_id == message_id)
            })
        {
            self.thread_index = position;
            self.preview_scroll = selected_preview_scroll;
            self.refresh_selected_mail_preview();
            return;
        }

        if let Some(thread_id) = selected_thread_id
            && let Some(position) = self.filtered_thread_indices.iter().position(|index| {
                self.threads
                    .get(*index)
                    .is_some_and(|row| row.thread_id == thread_id)
            })
        {
            self.thread_index = position;
            self.preview_scroll = selected_preview_scroll;
            self.refresh_selected_mail_preview();
        }
    }

    fn show_mailbox_threads(
        &mut self,
        mailbox: &str,
        threads: Vec<ThreadRow>,
        status: String,
        persist_ui_state: bool,
    ) {
        let mailbox = canonical_view_mailbox(mailbox);
        self.active_thread_mailbox = mailbox.clone();
        if let Some(index) = self
            .subscriptions
            .iter()
            .position(|item| same_mailbox_name(&item.mailbox, &mailbox))
        {
            self.subscription_index = index;
            self.sync_subscription_row_to_selected_item();
        }
        self.replace_threads(threads);
        self.status = status;
        if persist_ui_state {
            self.persist_ui_state();
        }
    }

    fn recover_from_empty_active_mailbox(&mut self, reason: &str) -> bool {
        if !self.threads.is_empty() {
            return false;
        }

        let current_mailbox = self.active_thread_mailbox.clone();
        let mut candidates = self.enabled_mailboxes();
        if !candidates
            .iter()
            .any(|mailbox| same_mailbox_name(mailbox, &self.runtime.source_mailbox))
        {
            candidates.push(self.runtime.source_mailbox.clone());
        }
        let mut unique_candidates: Vec<String> = Vec::new();
        for mailbox in candidates {
            if same_mailbox_name(&mailbox, &current_mailbox)
                || unique_candidates
                    .iter()
                    .any(|candidate| same_mailbox_name(candidate, &mailbox))
            {
                continue;
            }
            unique_candidates.push(mailbox);
        }

        for mailbox in unique_candidates {
            match self.load_thread_rows_for_view(&mailbox, 500) {
                Ok(rows) if !rows.is_empty() => {
                    self.show_mailbox_threads(
                        &mailbox,
                        rows,
                        format!("{reason}; showing threads for {mailbox}"),
                        true,
                    );
                    return true;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        mailbox = %mailbox,
                        error = %error,
                        "failed to load fallback mailbox thread rows"
                    );
                }
            }
        }

        false
    }

    fn startup_sync_mailbox_status(&self, mailbox: &str) -> Option<StartupSyncMailboxStatus> {
        self.startup_sync.as_ref().and_then(|state| {
            state
                .mailboxes
                .iter()
                .find(|(name, _)| same_mailbox_name(name, mailbox))
                .map(|(_, status)| *status)
        })
    }

    fn manual_sync_mailbox_status(&self, mailbox: &str) -> Option<StartupSyncMailboxStatus> {
        self.manual_sync.as_ref().and_then(|state| {
            state
                .mailboxes
                .iter()
                .find(|(name, _)| same_mailbox_name(name, mailbox))
                .map(|(_, status)| *status)
        })
    }

    fn inbox_auto_sync_mailbox_status(&self, mailbox: &str) -> Option<StartupSyncMailboxStatus> {
        is_following_view(mailbox)
            .then(|| {
                self.inbox_auto_sync
                    .as_ref()
                    .filter(|state| state.in_flight())
                    .map(|_| StartupSyncMailboxStatus::InFlight)
            })
            .flatten()
    }

    fn subscription_auto_sync_mailbox_status(
        &self,
        mailbox: &str,
    ) -> Option<StartupSyncMailboxStatus> {
        self.subscription_auto_sync
            .as_ref()
            .filter(|state| state.mailbox_in_flight(mailbox))
            .map(|_| StartupSyncMailboxStatus::InFlight)
    }

    fn mailbox_sync_status(&self, mailbox: &str) -> Option<StartupSyncMailboxStatus> {
        self.startup_sync_mailbox_status(mailbox)
            .or_else(|| self.manual_sync_mailbox_status(mailbox))
            .or_else(|| self.inbox_auto_sync_mailbox_status(mailbox))
            .or_else(|| self.subscription_auto_sync_mailbox_status(mailbox))
    }

    fn startup_sync_mailbox_pending(&self, mailbox: &str) -> bool {
        matches!(
            self.startup_sync_mailbox_status(mailbox),
            Some(StartupSyncMailboxStatus::Pending | StartupSyncMailboxStatus::InFlight)
        )
    }

    fn manual_sync_mailbox_pending(&self, mailbox: &str) -> bool {
        matches!(
            self.manual_sync_mailbox_status(mailbox),
            Some(StartupSyncMailboxStatus::Pending | StartupSyncMailboxStatus::InFlight)
        )
    }

    fn mailbox_sync_pending(&self, mailbox: &str) -> bool {
        self.startup_sync_mailbox_pending(mailbox)
            || self.manual_sync_mailbox_pending(mailbox)
            || matches!(
                self.inbox_auto_sync_mailbox_status(mailbox),
                Some(StartupSyncMailboxStatus::InFlight)
            )
            || matches!(
                self.subscription_auto_sync_mailbox_status(mailbox),
                Some(StartupSyncMailboxStatus::InFlight)
            )
    }

    fn background_sync_progress_text(&self) -> Option<String> {
        self.manual_sync
            .as_ref()
            .map(|state| {
                format!(
                    "sync {} {}/{} {}",
                    self.render_progress_bar(state.completed, state.total),
                    state.completed,
                    state.total,
                    state.inflight_mailboxes_display()
                )
            })
            .or_else(|| {
                self.startup_sync.as_ref().map(|state| {
                    format!(
                        "sync {} {}/{} {}",
                        self.render_progress_bar(state.completed, state.total),
                        state.completed,
                        state.total,
                        state.inflight_mailboxes_display()
                    )
                })
            })
            .or_else(|| {
                self.inbox_auto_sync
                    .as_ref()
                    .filter(|state| state.in_flight())
                    .map(|_| {
                        format!(
                            "sync {} auto {}",
                            self.render_indeterminate_progress_bar(),
                            FOLLOWING_VIEW
                        )
                    })
            })
            .or_else(|| {
                self.subscription_auto_sync
                    .as_ref()
                    .filter(|state| state.in_flight())
                    .map(|state| {
                        format!(
                            "sync {} auto {}",
                            self.render_indeterminate_progress_bar(),
                            state.inflight_mailboxes_display()
                        )
                    })
            })
    }

    fn render_progress_bar(&self, completed: usize, total: usize) -> String {
        const PROGRESS_BAR_WIDTH: usize = 12;

        let mut cells = vec!['.'; PROGRESS_BAR_WIDTH];
        if total == 0 {
            return format!("[{}]", cells.into_iter().collect::<String>());
        }

        let filled = completed.saturating_mul(PROGRESS_BAR_WIDTH) / total;
        for cell in cells.iter_mut().take(filled.min(PROGRESS_BAR_WIDTH)) {
            *cell = '=';
        }
        if completed < total {
            let pulse_width = PROGRESS_BAR_WIDTH.saturating_sub(filled).max(1);
            let pulse_offset = self.sync_animation_tick() % pulse_width;
            let pulse_index = (filled + pulse_offset).min(PROGRESS_BAR_WIDTH - 1);
            cells[pulse_index] = '>';
        }

        format!("[{}]", cells.into_iter().collect::<String>())
    }

    fn render_indeterminate_progress_bar(&self) -> String {
        const PROGRESS_BAR_WIDTH: usize = 12;
        const RUNNER_WIDTH: usize = 3;

        let mut cells = vec!['.'; PROGRESS_BAR_WIDTH];
        let start = self.sync_animation_tick() % PROGRESS_BAR_WIDTH;
        for step in 0..RUNNER_WIDTH {
            let index = (start + step) % PROGRESS_BAR_WIDTH;
            cells[index] = '>';
        }

        format!("[{}]", cells.into_iter().collect::<String>())
    }

    fn sync_animation_tick(&self) -> usize {
        (self.started_at.elapsed().as_millis() / 200) as usize
    }

    fn refresh_series_summaries(&mut self) {
        self.series_summaries =
            patch_worker::build_series_index(&self.active_thread_mailbox, &self.threads);
        if let Err(error) = patch_worker::hydrate_series_statuses(
            &self.runtime.database_path,
            &self.active_thread_mailbox,
            &mut self.series_summaries,
        ) {
            tracing::warn!(
                mailbox = %self.active_thread_mailbox,
                error = %error,
                "failed to hydrate patch series status from database"
            );
        }
    }

    fn refresh_following_kinds(&mut self) {
        if !is_following_view(&self.active_thread_mailbox) {
            self.following_kinds.clear();
            return;
        }

        let Some(self_email) = self.self_email.as_deref() else {
            self.following_kinds.clear();
            return;
        };

        match mail_store::load_following_kinds_by_thread(&self.runtime.database_path, self_email) {
            Ok(kinds) => self.following_kinds = kinds,
            Err(error) => {
                self.following_kinds.clear();
                tracing::warn!(error = %error, "failed to hydrate Following badges");
            }
        }
    }

    fn load_thread_rows_for_view(&self, mailbox: &str, limit: usize) -> Result<Vec<ThreadRow>> {
        if is_following_view(mailbox) {
            let Some(self_email) = self.self_email.as_deref() else {
                return Ok(Vec::new());
            };
            return mail_store::load_following_thread_rows(
                &self.runtime.database_path,
                self_email,
                limit,
            );
        }

        mail_store::load_thread_rows_by_mailbox(&self.runtime.database_path, mailbox, limit)
    }

    fn refresh_review_summaries(&mut self) {
        match review_worker::build_review_index(
            &self.runtime.database_path,
            &self.active_thread_mailbox,
            &self.threads,
        ) {
            Ok(summaries) => self.review_summaries = summaries,
            Err(error) => {
                self.review_summaries.clear();
                tracing::warn!(
                    mailbox = %self.active_thread_mailbox,
                    error = %error,
                    "failed to hydrate review trailer summaries"
                );
            }
        }
    }

    fn enabled_mailboxes(&self) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter(|item| item.enabled)
            .map(|item| item.mailbox.clone())
            .collect()
    }

    fn enabled_background_sync_mailboxes(&self) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter(|item| item.enabled && !is_following_view(&item.mailbox))
            .map(|item| item.mailbox.clone())
            .collect()
    }

    fn my_inbox_auto_sync_enabled(&self) -> bool {
        self.runtime.imap.is_complete()
            && self
                .subscriptions
                .iter()
                .any(|item| item.enabled && is_following_view(&item.mailbox))
    }

    fn subscription_auto_sync_enabled(&self) -> bool {
        !self.enabled_background_sync_mailboxes().is_empty()
    }

    fn reconcile_inbox_auto_sync(&mut self) {
        if self.my_inbox_auto_sync_enabled() {
            self.inbox_auto_sync.get_or_insert_with(|| {
                InboxAutoSyncState::new(Instant::now() + self.runtime.inbox_auto_sync_interval())
            });
        } else {
            self.inbox_auto_sync = None;
        }
    }

    fn reconcile_subscription_auto_sync(&mut self) {
        let enabled_mailboxes = self.enabled_background_sync_mailboxes();
        if self.subscription_auto_sync_enabled() {
            // Reuse the existing background sync cadence so enabled mailing-list
            // subscriptions keep refreshing without adding a second timer knob.
            let state = self.subscription_auto_sync.get_or_insert_with(|| {
                SubscriptionAutoSyncState::new(
                    Instant::now() + self.runtime.inbox_auto_sync_interval(),
                )
            });
            state.in_flight_mailboxes.retain(|mailbox| {
                enabled_mailboxes
                    .iter()
                    .any(|enabled| same_mailbox_name(enabled, mailbox))
            });
        } else {
            self.subscription_auto_sync = None;
        }
    }

    fn defer_inbox_auto_sync(&mut self) {
        if let Some(state) = self.inbox_auto_sync.as_mut() {
            state.next_due_at = Instant::now() + self.runtime.inbox_auto_sync_interval();
        }
    }

    fn defer_subscription_auto_sync(&mut self) {
        if let Some(state) = self.subscription_auto_sync.as_mut() {
            state.next_due_at = Instant::now() + self.runtime.inbox_auto_sync_interval();
        }
    }

    fn start_manual_sync(
        &mut self,
        requested_mailboxes: Vec<String>,
        origin: ManualSyncOrigin,
    ) -> ManualSyncRequestOutcome {
        let requested_mailboxes = dedup_mailboxes(
            requested_mailboxes
                .into_iter()
                .map(|mailbox| canonical_view_mailbox(&mailbox))
                .collect(),
        );
        if requested_mailboxes.is_empty() {
            self.status = "sync skipped: no mailbox selected".to_string();
            tracing::info!(
                op = "manual_sync",
                status = "skipped",
                reason = "no_mailboxes",
                origin = origin.log_label()
            );
            return ManualSyncRequestOutcome::Busy;
        }

        if let Some(sync_state) = self.manual_sync.as_ref() {
            let all_tracked = requested_mailboxes.iter().all(|mailbox| {
                sync_state
                    .mailboxes
                    .keys()
                    .any(|tracked| same_mailbox_name(tracked, mailbox))
            });
            self.status = if all_tracked {
                format!(
                    "sync already running in background: {}",
                    requested_mailboxes.join(", ")
                )
            } else {
                format!("background sync busy: {}", sync_state.progress_summary())
            };
            tracing::info!(
                op = "manual_sync",
                status = "skipped",
                reason = if all_tracked {
                    "mailboxes_already_syncing"
                } else {
                    "manual_sync_busy"
                },
                origin = origin.log_label(),
                requested_mailboxes = %requested_mailboxes.join(",")
            );
            return if all_tracked {
                ManualSyncRequestOutcome::AlreadySyncing
            } else {
                ManualSyncRequestOutcome::Busy
            };
        }

        let mut skipped_mailboxes = Vec::new();
        let mut queued_mailboxes = Vec::new();
        for mailbox in requested_mailboxes {
            if self.mailbox_sync_pending(&mailbox) {
                skipped_mailboxes.push(mailbox);
            } else {
                queued_mailboxes.push(mailbox);
            }
        }

        if queued_mailboxes.is_empty() {
            self.status = format!(
                "sync already running in background: {}",
                skipped_mailboxes.join(", ")
            );
            tracing::info!(
                op = "manual_sync",
                status = "skipped",
                reason = "mailboxes_already_syncing",
                origin = origin.log_label(),
                requested_mailboxes = %skipped_mailboxes.join(",")
            );
            return ManualSyncRequestOutcome::AlreadySyncing;
        }

        if queued_mailboxes
            .iter()
            .any(|mailbox| is_following_view(mailbox))
        {
            self.defer_inbox_auto_sync();
        }
        if queued_mailboxes
            .iter()
            .any(|mailbox| !is_following_view(mailbox))
        {
            self.defer_subscription_auto_sync();
        }

        let receiver = (self.manual_sync_spawner)(self.runtime.clone(), queued_mailboxes.clone());
        self.manual_sync = Some(ManualSyncState {
            receiver,
            mailbox_order: queued_mailboxes.clone(),
            mailboxes: queued_mailboxes
                .iter()
                .cloned()
                .map(|mailbox| (mailbox, StartupSyncMailboxStatus::Pending))
                .collect(),
            total: queued_mailboxes.len(),
            completed: 0,
            succeeded: 0,
            failed: 0,
            total_fetched: 0,
            total_inserted: 0,
            total_updated: 0,
            first_error: None,
        });

        self.status = if skipped_mailboxes.is_empty() {
            format!("sync queued in background: {}", queued_mailboxes.join(", "))
        } else {
            format!(
                "sync queued in background: {}; skipped already-running: {}",
                queued_mailboxes.join(", "),
                skipped_mailboxes.join(", ")
            )
        };
        if let Some(sync_state) = self.manual_sync.as_ref() {
            tracing::info!(
                op = "manual_sync",
                status = "started",
                origin = origin.log_label(),
                total = sync_state.total,
                completed = sync_state.completed,
                succeeded = sync_state.succeeded,
                failed = sync_state.failed,
                queued = sync_state.pending_count(),
                running = %sync_state.inflight_mailboxes_display(),
                mailbox_states = %sync_state.mailbox_states_display(),
                requested_mailboxes = %sync_state.mailbox_order.join(",")
            );
        }

        ManualSyncRequestOutcome::Started
    }

    fn queue_palette_sync(&mut self, requested_mailboxes: Vec<String>) {
        let _ = self.start_manual_sync(requested_mailboxes, ManualSyncOrigin::PaletteCommand);
    }

    fn set_review_inbox_mode(&mut self, mode: Option<ReviewInboxMode>) {
        self.review_inbox_mode = mode;
        self.apply_thread_filter();
        self.status = match mode {
            Some(mode) => format!(
                "Review Inbox: {} ({} matching threads)",
                review_worker::mode_label(mode),
                self.filtered_thread_indices.len()
            ),
            None => "Review Inbox filter cleared".to_string(),
        };
    }

    fn queue_palette_review_inbox(&mut self, command: &str) {
        let command = command.to_ascii_lowercase();
        let mut parts = command.split_whitespace();
        let _ = parts.next();
        let mode = match parts.next().unwrap_or("needs-review") {
            "needs-review" | "review" => Some(ReviewInboxMode::NeedsReview),
            "reviewed" | "picked" => Some(ReviewInboxMode::Reviewed),
            "all" => Some(ReviewInboxMode::All),
            "off" | "clear" => None,
            _ => {
                self.status = "usage: review-inbox [needs-review|reviewed|all|off]".to_string();
                return;
            }
        };
        if parts.next().is_some() {
            self.status = "usage: review-inbox [needs-review|reviewed|all|off]".to_string();
            return;
        }
        self.set_review_inbox_mode(mode);
    }

    fn start_thread_fetch(&mut self, message_id: String) {
        let mailbox = if is_following_view(&self.active_thread_mailbox) {
            self.selected_thread()
                .and_then(|row| (!row.mailbox.trim().is_empty()).then_some(row.mailbox.clone()))
                .unwrap_or_else(|| self.active_thread_mailbox.clone())
        } else {
            self.active_thread_mailbox.clone()
        };
        self.start_thread_fetch_for_mailbox(mailbox, message_id);
    }

    fn start_thread_fetch_for_mailbox(&mut self, mailbox: String, message_id: String) {
        let message_id = message_id.trim().to_string();
        if message_id.is_empty() {
            self.status = "thread fetch requires a Message-ID".to_string();
            return;
        }
        if self.thread_fetch.is_some() {
            self.status = "a complete thread fetch is already running".to_string();
            return;
        }

        let receiver =
            (self.thread_fetch_spawner)(self.runtime.clone(), mailbox.clone(), message_id.clone());
        self.thread_fetch = Some(ThreadFetchState {
            receiver,
            mailbox: mailbox.clone(),
            message_id: message_id.clone(),
        });
        self.status = format!(
            "fetching complete thread for <{}> in {}...",
            message_id.trim_matches(['<', '>']),
            mailbox
        );
        tracing::info!(
            op = "thread_fetch",
            status = "started",
            mailbox = %mailbox,
            message_id = %message_id
        );
    }

    fn queue_palette_thread_fetch(&mut self, command: &str) {
        let mut parts = command.split_whitespace().skip(1);
        let mut mailbox = self.active_thread_mailbox.clone();
        let mut message_id = None;

        while let Some(part) = parts.next() {
            if part == "--mailbox" {
                let Some(value) = parts.next() else {
                    self.status = "fetch-thread --mailbox requires a mailbox".to_string();
                    return;
                };
                mailbox = value.to_string();
            } else if message_id.is_none() {
                message_id = Some(part.to_string());
            } else {
                self.status = "usage: fetch-thread [--mailbox NAME] MESSAGE_ID".to_string();
                return;
            }
        }

        let Some(message_id) = message_id else {
            self.status = "usage: fetch-thread [--mailbox NAME] MESSAGE_ID".to_string();
            return;
        };
        self.start_thread_fetch_for_mailbox(mailbox, message_id);
    }

    fn pump_thread_fetch_events(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        {
            let Some(state) = self.thread_fetch.as_ref() else {
                return;
            };
            loop {
                match state.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        for event in events {
            match event {
                ThreadFetchEvent::Finished {
                    mailbox,
                    message_id,
                    fetched,
                    inserted,
                    updated,
                } => {
                    self.thread_fetch = None;
                    if same_mailbox_name(&mailbox, &self.active_thread_mailbox)
                        && let Err(error) =
                            self.reload_mailbox_threads_preserving_selection(&mailbox)
                    {
                        self.status =
                            format!("thread fetched but failed to reload {}: {}", mailbox, error);
                    } else {
                        self.status = format!(
                            "thread fetched <{}>: {} mail(s), {} inserted, {} updated",
                            message_id.trim_matches(['<', '>']),
                            fetched,
                            inserted,
                            updated
                        );
                    }
                    tracing::info!(
                        op = "thread_fetch",
                        status = "succeeded",
                        mailbox = %mailbox,
                        message_id = %message_id,
                        fetched,
                        inserted,
                        updated
                    );
                }
                ThreadFetchEvent::Failed {
                    mailbox,
                    message_id,
                    error,
                } => {
                    self.thread_fetch = None;
                    self.status = format!(
                        "thread fetch failed for <{}> in {}: {}",
                        message_id.trim_matches(['<', '>']),
                        mailbox,
                        error
                    );
                    tracing::error!(
                        op = "thread_fetch",
                        status = "failed",
                        mailbox = %mailbox,
                        message_id = %message_id,
                        error = %error
                    );
                }
            }
        }

        if disconnected && self.thread_fetch.is_some() {
            let state = self.thread_fetch.take();
            if let Some(state) = state {
                self.status = format!(
                    "thread fetch worker disconnected for <{}> in {}",
                    state.message_id.trim_matches(['<', '>']),
                    state.mailbox
                );
            }
        }
    }

    fn maybe_start_inbox_auto_sync(&mut self) {
        self.reconcile_inbox_auto_sync();
        let inbox_sync_pending = self.startup_sync_mailbox_pending(IMAP_INBOX_MAILBOX)
            || self.manual_sync_mailbox_pending(IMAP_INBOX_MAILBOX);
        let now = Instant::now();
        let Some(state) = self.inbox_auto_sync.as_mut() else {
            return;
        };
        if state.in_flight() || inbox_sync_pending || now < state.next_due_at {
            return;
        }

        tracing::info!(
            op = "following_auto_sync",
            status = "started",
            mailbox = FOLLOWING_VIEW
        );
        state.receiver = Some((self.mailbox_sync_spawner)(
            self.runtime.clone(),
            vec![IMAP_INBOX_MAILBOX.to_string()],
        ));
    }

    fn maybe_start_subscription_auto_sync(&mut self) {
        self.reconcile_subscription_auto_sync();
        let mailboxes = self.enabled_background_sync_mailboxes();
        let background_pending = mailboxes.iter().any(|mailbox| {
            self.startup_sync_mailbox_pending(mailbox) || self.manual_sync_mailbox_pending(mailbox)
        });
        let now = Instant::now();
        let Some(state) = self.subscription_auto_sync.as_mut() else {
            return;
        };
        if mailboxes.is_empty()
            || state.in_flight()
            || background_pending
            || now < state.next_due_at
        {
            return;
        }

        tracing::info!(
            op = "subscription_auto_sync",
            status = "started",
            mailboxes = %mailboxes.join(",")
        );
        state.in_flight_mailboxes.clear();
        state.receiver = Some((self.mailbox_sync_spawner)(self.runtime.clone(), mailboxes));
    }

    fn pump_inbox_auto_sync_events(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        {
            let Some(sync_state) = self.inbox_auto_sync.as_ref() else {
                return;
            };
            let Some(receiver) = sync_state.receiver.as_ref() else {
                return;
            };
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut worker_completed = false;
        for event in events {
            if matches!(event, StartupSyncEvent::WorkerCompleted) {
                worker_completed = true;
            }
            self.apply_inbox_auto_sync_event(event);
        }

        if (disconnected || worker_completed)
            && let Some(state) = self.inbox_auto_sync.as_mut()
        {
            state.receiver = None;
        }
    }

    fn pump_manual_sync_events(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        {
            let Some(sync_state) = self.manual_sync.as_ref() else {
                return;
            };
            loop {
                match sync_state.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        for event in events {
            self.apply_manual_sync_event(event);
        }

        if disconnected && self.manual_sync.is_some() {
            let (completed, total, succeeded, failed) = self
                .manual_sync
                .as_ref()
                .map(|state| (state.completed, state.total, state.succeeded, state.failed))
                .unwrap_or((0, 0, 0, 0));
            self.manual_sync = None;
            self.status = format!(
                "background sync worker disconnected (completed={completed}/{total} ok={succeeded} failed={failed})"
            );
            tracing::warn!(
                op = "manual_sync",
                status = "failed",
                completed,
                total,
                succeeded,
                failed,
                "manual sync worker disconnected unexpectedly"
            );
        }
    }

    fn pump_subscription_auto_sync_events(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        {
            let Some(sync_state) = self.subscription_auto_sync.as_ref() else {
                return;
            };
            let Some(receiver) = sync_state.receiver.as_ref() else {
                return;
            };
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut worker_completed = false;
        for event in events {
            if matches!(event, StartupSyncEvent::WorkerCompleted) {
                worker_completed = true;
            }
            self.apply_subscription_auto_sync_event(event);
        }

        if (disconnected || worker_completed)
            && let Some(state) = self.subscription_auto_sync.as_mut()
        {
            state.receiver = None;
            state.in_flight_mailboxes.clear();
            state.next_due_at = Instant::now() + self.runtime.inbox_auto_sync_interval();
        }
    }

    fn apply_manual_sync_event(&mut self, event: StartupSyncEvent) {
        match event {
            StartupSyncEvent::MailboxStarted {
                mailbox,
                index,
                total,
            } => {
                if let Some(sync_state) = self.manual_sync.as_mut() {
                    sync_state
                        .mailboxes
                        .insert(mailbox.clone(), StartupSyncMailboxStatus::InFlight);
                }
                self.status = format!("sync [{index}/{total}] syncing {mailbox} in background...");
                if let Some(sync_state) = self.manual_sync.as_ref() {
                    tracing::info!(
                        op = "manual_sync",
                        status = "progress",
                        phase = "started",
                        mailbox = %mailbox,
                        index,
                        total,
                        completed = sync_state.completed,
                        succeeded = sync_state.succeeded,
                        failed = sync_state.failed,
                        queued = sync_state.pending_count(),
                        running = %sync_state.inflight_mailboxes_display(),
                        mailbox_states = %sync_state.mailbox_states_display()
                    );
                }
            }
            StartupSyncEvent::MailboxFinished {
                mailbox,
                fetched,
                inserted,
                updated,
            } => {
                if is_following_view(&mailbox) {
                    self.defer_inbox_auto_sync();
                } else {
                    self.defer_subscription_auto_sync();
                }
                if let Some(sync_state) = self.manual_sync.as_mut() {
                    sync_state
                        .mailboxes
                        .insert(mailbox.clone(), StartupSyncMailboxStatus::Finished);
                    sync_state.completed += 1;
                    sync_state.succeeded += 1;
                    sync_state.total_fetched += fetched;
                    sync_state.total_inserted += inserted;
                    sync_state.total_updated += updated;
                }

                if let Some(sync_state) = self.manual_sync.as_ref() {
                    self.status = format!(
                        "sync [{}/{}] finished {}",
                        sync_state.completed, sync_state.total, mailbox
                    );
                    tracing::info!(
                        op = "manual_sync",
                        status = "succeeded",
                        phase = "finished",
                        mailbox = %mailbox,
                        fetched,
                        inserted,
                        updated,
                        completed = sync_state.completed,
                        total = sync_state.total,
                        succeeded = sync_state.succeeded,
                        failed = sync_state.failed,
                        queued = sync_state.pending_count(),
                        running = %sync_state.inflight_mailboxes_display(),
                        mailbox_states = %sync_state.mailbox_states_display()
                    );
                }
            }
            StartupSyncEvent::MailboxFailed { mailbox, error } => {
                if is_following_view(&mailbox) {
                    self.defer_inbox_auto_sync();
                } else {
                    self.defer_subscription_auto_sync();
                }
                if let Some(sync_state) = self.manual_sync.as_mut() {
                    sync_state
                        .mailboxes
                        .insert(mailbox.clone(), StartupSyncMailboxStatus::Failed);
                    sync_state.completed += 1;
                    sync_state.failed += 1;
                    if sync_state.first_error.is_none() {
                        sync_state.first_error = Some(format!("{mailbox}: {error}"));
                    }
                }
                self.status = format!("sync failed for {mailbox}: {error}");
                if let Some(sync_state) = self.manual_sync.as_ref() {
                    tracing::error!(
                        op = "manual_sync",
                        status = "failed",
                        phase = "finished",
                        mailbox = %mailbox,
                        error = %error,
                        completed = sync_state.completed,
                        total = sync_state.total,
                        succeeded = sync_state.succeeded,
                        failed = sync_state.failed,
                        queued = sync_state.pending_count(),
                        running = %sync_state.inflight_mailboxes_display(),
                        mailbox_states = %sync_state.mailbox_states_display()
                    );
                }
            }
            StartupSyncEvent::WorkerCompleted => {}
        }

        self.maybe_finish_manual_sync();
    }

    fn apply_inbox_auto_sync_event(&mut self, event: StartupSyncEvent) {
        match event {
            StartupSyncEvent::MailboxStarted { mailbox, .. } => {
                tracing::info!(
                    op = "inbox_auto_sync",
                    status = "progress",
                    phase = "started",
                    mailbox = %mailbox
                );
            }
            StartupSyncEvent::MailboxFinished {
                mailbox,
                fetched,
                inserted,
                updated,
            } => {
                if let Some(state) = self.inbox_auto_sync.as_mut() {
                    state.next_due_at = Instant::now() + self.runtime.inbox_auto_sync_interval();
                }
                if same_mailbox_name(&mailbox, &self.active_thread_mailbox)
                    && let Err(error) = self.reload_mailbox_threads_preserving_selection(&mailbox)
                {
                    tracing::error!(
                        op = "inbox_auto_sync",
                        status = "failed",
                        mailbox = %mailbox,
                        error = %error
                    );
                    self.status = format!(
                        "background sync ok but failed to reload threads for {mailbox}: {error}"
                    );
                }
                if inserted > 0 || updated > 0 {
                    self.status = format!(
                        "Following auto-sync: fetched={} inserted={} updated={}",
                        fetched, inserted, updated
                    );
                }
                tracing::info!(
                    op = "inbox_auto_sync",
                    status = "succeeded",
                    mailbox = %mailbox,
                    fetched,
                    inserted,
                    updated
                );
            }
            StartupSyncEvent::MailboxFailed { mailbox, error } => {
                if let Some(state) = self.inbox_auto_sync.as_mut() {
                    state.next_due_at = Instant::now() + self.runtime.inbox_auto_sync_interval();
                }
                self.status = format!("Following auto-sync failed: {error}");
                tracing::error!(
                    op = "inbox_auto_sync",
                    status = "failed",
                    mailbox = %mailbox,
                    error = %error
                );
            }
            StartupSyncEvent::WorkerCompleted => {}
        }
    }

    fn apply_subscription_auto_sync_event(&mut self, event: StartupSyncEvent) {
        match event {
            StartupSyncEvent::MailboxStarted { mailbox, .. } => {
                if let Some(state) = self.subscription_auto_sync.as_mut() {
                    state.in_flight_mailboxes.insert(mailbox.clone());
                }
                tracing::info!(
                    op = "subscription_auto_sync",
                    status = "progress",
                    phase = "started",
                    mailbox = %mailbox
                );
            }
            StartupSyncEvent::MailboxFinished {
                mailbox,
                fetched,
                inserted,
                updated,
            } => {
                if let Some(state) = self.subscription_auto_sync.as_mut() {
                    state
                        .in_flight_mailboxes
                        .retain(|in_flight| !same_mailbox_name(in_flight, &mailbox));
                }
                if same_mailbox_name(&mailbox, &self.active_thread_mailbox)
                    && let Err(error) = self.reload_mailbox_threads_preserving_selection(&mailbox)
                {
                    tracing::error!(
                        op = "subscription_auto_sync",
                        status = "failed",
                        mailbox = %mailbox,
                        error = %error
                    );
                    self.status = format!(
                        "background sync ok but failed to reload threads for {mailbox}: {error}"
                    );
                }
                if inserted > 0 || updated > 0 {
                    self.status = format!(
                        "Subscription auto-sync {mailbox}: fetched={} inserted={} updated={}",
                        fetched, inserted, updated
                    );
                }
                tracing::info!(
                    op = "subscription_auto_sync",
                    status = "succeeded",
                    mailbox = %mailbox,
                    fetched,
                    inserted,
                    updated
                );
            }
            StartupSyncEvent::MailboxFailed { mailbox, error } => {
                if let Some(state) = self.subscription_auto_sync.as_mut() {
                    state
                        .in_flight_mailboxes
                        .retain(|in_flight| !same_mailbox_name(in_flight, &mailbox));
                }
                self.status = format!("subscription auto-sync failed for {mailbox}: {error}");
                tracing::error!(
                    op = "subscription_auto_sync",
                    status = "failed",
                    mailbox = %mailbox,
                    error = %error
                );
            }
            StartupSyncEvent::WorkerCompleted => {}
        }
    }

    fn start_startup_sync_if_enabled(&mut self) {
        if !self.runtime.startup_sync {
            tracing::info!(
                op = "startup_sync",
                status = "disabled",
                reason = "ui.startup_sync=false"
            );
            return;
        }

        let mailboxes = self.enabled_mailboxes();
        if mailboxes.is_empty() {
            tracing::info!(
                op = "startup_sync",
                status = "skipped",
                reason = "no_enabled_subscriptions"
            );
            return;
        }

        let receiver = spawn_startup_sync_worker(self.runtime.clone(), mailboxes.clone());
        self.startup_sync = Some(StartupSyncState {
            receiver,
            mailbox_order: mailboxes.clone(),
            mailboxes: mailboxes
                .iter()
                .cloned()
                .map(|mailbox| (mailbox, StartupSyncMailboxStatus::Pending))
                .collect(),
            total: mailboxes.len(),
            completed: 0,
            succeeded: 0,
            failed: 0,
        });
        self.status = format!(
            "startup sync queued: {} mailbox(es): {}",
            mailboxes.len(),
            mailboxes.join(", ")
        );
        if let Some(sync_state) = self.startup_sync.as_ref() {
            tracing::info!(
                op = "startup_sync",
                status = "started",
                total = sync_state.total,
                completed = sync_state.completed,
                succeeded = sync_state.succeeded,
                failed = sync_state.failed,
                queued = sync_state.pending_count(),
                running = %sync_state.inflight_mailboxes_display(),
                mailbox_states = %sync_state.mailbox_states_display(),
                mailboxes = %mailboxes.join(",")
            );
        }
    }

    fn pump_startup_sync_events(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;
        {
            let Some(sync_state) = self.startup_sync.as_ref() else {
                return;
            };
            loop {
                match sync_state.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        for event in events {
            self.apply_startup_sync_event(event);
        }

        if disconnected && self.startup_sync.is_some() {
            let (completed, total, succeeded, failed) = self
                .startup_sync
                .as_ref()
                .map(|state| (state.completed, state.total, state.succeeded, state.failed))
                .unwrap_or((0, 0, 0, 0));
            self.startup_sync = None;
            self.status = format!(
                "startup sync worker disconnected (completed={completed}/{total} ok={succeeded} failed={failed})"
            );
            tracing::warn!(
                op = "startup_sync",
                status = "failed",
                completed,
                total,
                succeeded,
                failed,
                "startup sync worker disconnected unexpectedly"
            );
        }
    }

    fn apply_startup_sync_event(&mut self, event: StartupSyncEvent) {
        match event {
            StartupSyncEvent::MailboxStarted {
                mailbox,
                index,
                total,
            } => {
                if let Some(sync_state) = self.startup_sync.as_mut() {
                    sync_state
                        .mailboxes
                        .insert(mailbox.clone(), StartupSyncMailboxStatus::InFlight);
                }
                self.status = format!("startup sync [{index}/{total}] syncing {mailbox}...");
                if let Some(sync_state) = self.startup_sync.as_ref() {
                    tracing::info!(
                        op = "startup_sync",
                        status = "progress",
                        phase = "started",
                        mailbox = %mailbox,
                        index,
                        total,
                        completed = sync_state.completed,
                        succeeded = sync_state.succeeded,
                        failed = sync_state.failed,
                        queued = sync_state.pending_count(),
                        running = %sync_state.inflight_mailboxes_display(),
                        mailbox_states = %sync_state.mailbox_states_display()
                    );
                }
            }
            StartupSyncEvent::MailboxFinished {
                mailbox,
                fetched,
                inserted,
                updated,
            } => {
                if is_following_view(&mailbox) {
                    self.defer_inbox_auto_sync();
                }
                if !is_following_view(&mailbox) {
                    self.defer_subscription_auto_sync();
                }
                if let Some(sync_state) = self.startup_sync.as_mut() {
                    sync_state
                        .mailboxes
                        .insert(mailbox.clone(), StartupSyncMailboxStatus::Finished);
                    sync_state.completed += 1;
                    sync_state.succeeded += 1;
                }

                if same_mailbox_name(&mailbox, &self.active_thread_mailbox) {
                    self.reload_active_mailbox_threads_after_sync();
                }
                if self.threads.is_empty()
                    && !self.startup_sync_mailbox_pending(&self.active_thread_mailbox)
                {
                    let _ = self.recover_from_empty_active_mailbox(&format!(
                        "startup sync ready for {mailbox}"
                    ));
                }

                if let Some(sync_state) = self.startup_sync.as_ref() {
                    tracing::info!(
                        op = "startup_sync",
                        status = "succeeded",
                        phase = "finished",
                        mailbox = %mailbox,
                        fetched,
                        inserted,
                        updated,
                        completed = sync_state.completed,
                        total = sync_state.total,
                        succeeded = sync_state.succeeded,
                        failed = sync_state.failed,
                        queued = sync_state.pending_count(),
                        running = %sync_state.inflight_mailboxes_display(),
                        mailbox_states = %sync_state.mailbox_states_display()
                    );
                }
            }
            StartupSyncEvent::MailboxFailed { mailbox, error } => {
                if is_following_view(&mailbox) {
                    self.defer_inbox_auto_sync();
                }
                if !is_following_view(&mailbox) {
                    self.defer_subscription_auto_sync();
                }
                if let Some(sync_state) = self.startup_sync.as_mut() {
                    sync_state
                        .mailboxes
                        .insert(mailbox.clone(), StartupSyncMailboxStatus::Failed);
                    sync_state.completed += 1;
                    sync_state.failed += 1;
                }
                self.status = format!("startup sync failed for {mailbox}: {error}");
                if same_mailbox_name(&mailbox, &self.active_thread_mailbox)
                    && self.threads.is_empty()
                {
                    let _ = self.recover_from_empty_active_mailbox(&format!(
                        "startup sync failed for {mailbox}: {error}"
                    ));
                }
                if let Some(sync_state) = self.startup_sync.as_ref() {
                    tracing::error!(
                        op = "startup_sync",
                        status = "failed",
                        phase = "finished",
                        mailbox = %mailbox,
                        error = %error,
                        completed = sync_state.completed,
                        total = sync_state.total,
                        succeeded = sync_state.succeeded,
                        failed = sync_state.failed,
                        queued = sync_state.pending_count(),
                        running = %sync_state.inflight_mailboxes_display(),
                        mailbox_states = %sync_state.mailbox_states_display()
                    );
                }
            }
            StartupSyncEvent::WorkerCompleted => {}
        }

        self.maybe_finish_startup_sync();
    }

    fn maybe_finish_startup_sync(&mut self) {
        let Some(sync_state) = self.startup_sync.as_ref() else {
            return;
        };
        if sync_state.completed < sync_state.total {
            return;
        }

        let succeeded = sync_state.succeeded;
        let failed = sync_state.failed;
        let total = sync_state.total;
        let mailbox_states = sync_state.mailbox_states_display();
        self.startup_sync = None;
        self.status =
            format!("startup sync finished: ok={succeeded} failed={failed} total={total}");
        tracing::info!(
            op = "startup_sync",
            status = if failed == 0 { "succeeded" } else { "partial" },
            succeeded,
            failed,
            total,
            mailbox_states = %mailbox_states
        );
    }

    fn maybe_finish_manual_sync(&mut self) {
        let Some(sync_state) = self.manual_sync.as_ref() else {
            return;
        };
        if sync_state.completed < sync_state.total {
            return;
        }

        let succeeded = sync_state.succeeded;
        let failed = sync_state.failed;
        let total = sync_state.total;
        let total_fetched = sync_state.total_fetched;
        let total_inserted = sync_state.total_inserted;
        let total_updated = sync_state.total_updated;
        let first_error = sync_state.first_error.clone();
        let first_error_text = first_error
            .clone()
            .unwrap_or_else(|| "worker reported no success".to_string());
        let mailbox_states = sync_state.mailbox_states_display();
        let active_mailbox = self.active_thread_mailbox.clone();
        let should_reload_active_mailbox = sync_state.mailboxes.iter().any(|(mailbox, status)| {
            same_mailbox_name(mailbox, &active_mailbox)
                && *status == StartupSyncMailboxStatus::Finished
        });

        self.manual_sync = None;

        if should_reload_active_mailbox
            && let Err(error) = self.reload_mailbox_threads_preserving_selection(&active_mailbox)
        {
            tracing::error!(
                op = "manual_sync",
                status = "failed",
                mailbox = %active_mailbox,
                error = %error
            );
            self.status =
                format!("sync ok but failed to reload threads for {active_mailbox}: {error}");
            return;
        }

        self.status = if failed == 0 {
            format!(
                "sync finished: ok={succeeded} total={total} fetched={total_fetched} inserted={total_inserted} updated={total_updated}"
            )
        } else if succeeded == 0 {
            format!("sync failed: {first_error_text}")
        } else {
            format!(
                "sync finished with failures: ok={succeeded} failed={failed} fetched={total_fetched} inserted={total_inserted} updated={total_updated}"
            )
        };
        tracing::info!(
            op = "manual_sync",
            status = if failed == 0 {
                "succeeded"
            } else if succeeded == 0 {
                "failed"
            } else {
                "partial"
            },
            succeeded,
            failed,
            total,
            total_fetched,
            total_inserted,
            total_updated,
            first_error = %first_error.as_deref().unwrap_or("-"),
            mailbox_states = %mailbox_states
        );
    }

    fn reload_active_mailbox_threads_after_sync(&mut self) {
        let mailbox = self.active_thread_mailbox.clone();
        match self.reload_mailbox_threads_preserving_selection(&mailbox) {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(
                    op = "startup_sync",
                    status = "failed",
                    mailbox = %self.active_thread_mailbox,
                    error = %error
                );
                self.status = format!(
                    "startup sync ok but failed to reload threads for {}: {}",
                    self.active_thread_mailbox, error
                );
            }
        }
    }

    fn reload_mailbox_threads_preserving_selection(&mut self, mailbox: &str) -> Result<()> {
        let rows = self.load_thread_rows_for_view(mailbox, 500)?;
        if same_mailbox_name(mailbox, &self.active_thread_mailbox) {
            self.replace_threads_preserving_selection(rows);
        }

        Ok(())
    }

    fn to_ui_state(&self) -> UiState {
        UiState {
            enabled_mailboxes: self.enabled_mailboxes(),
            enabled_group_expanded: self.enabled_group_expanded,
            disabled_group_expanded: self.disabled_group_expanded,
            enabled_linux_subsystem_expanded: self.enabled_linux_subsystem_expanded,
            enabled_qemu_subsystem_expanded: self.enabled_qemu_subsystem_expanded,
            disabled_linux_subsystem_expanded: self.disabled_linux_subsystem_expanded,
            disabled_qemu_subsystem_expanded: self.disabled_qemu_subsystem_expanded,
            imap_defaults_initialized: self.imap_defaults_initialized,
            active_mailbox: Some(self.active_thread_mailbox.clone()),
            mail_subscriptions_width: self.mail_pane_layout.subscriptions_width,
            mail_preview_width: self.mail_pane_layout.preview_width,
            saved_views: self.saved_views.clone(),
        }
    }

    fn persist_ui_state(&self) {
        if let Err(error) = ui_state::save(&self.ui_state_path, &self.to_ui_state()) {
            tracing::warn!(
                path = %self.ui_state_path.display(),
                error = %error,
                "failed to persist ui state"
            );
        }
    }

    fn subscription_category_expanded(
        &self,
        section: SubscriptionSection,
        category: SubscriptionCategory,
    ) -> bool {
        // Keep expansion state scoped to the enabled/disabled section so
        // collapsing one bucket does not unexpectedly hide the other one.
        match (section, category) {
            (SubscriptionSection::Enabled, SubscriptionCategory::LinuxSubsystem) => {
                self.enabled_linux_subsystem_expanded
            }
            (SubscriptionSection::Enabled, SubscriptionCategory::QemuSubsystem) => {
                self.enabled_qemu_subsystem_expanded
            }
            (SubscriptionSection::Disabled, SubscriptionCategory::LinuxSubsystem) => {
                self.disabled_linux_subsystem_expanded
            }
            (SubscriptionSection::Disabled, SubscriptionCategory::QemuSubsystem) => {
                self.disabled_qemu_subsystem_expanded
            }
        }
    }

    fn toggle_subscription_category_group(
        &mut self,
        section: SubscriptionSection,
        category: SubscriptionCategory,
    ) {
        let expanded = match (section, category) {
            (SubscriptionSection::Enabled, SubscriptionCategory::LinuxSubsystem) => {
                self.enabled_linux_subsystem_expanded = !self.enabled_linux_subsystem_expanded;
                self.enabled_linux_subsystem_expanded
            }
            (SubscriptionSection::Enabled, SubscriptionCategory::QemuSubsystem) => {
                self.enabled_qemu_subsystem_expanded = !self.enabled_qemu_subsystem_expanded;
                self.enabled_qemu_subsystem_expanded
            }
            (SubscriptionSection::Disabled, SubscriptionCategory::LinuxSubsystem) => {
                self.disabled_linux_subsystem_expanded = !self.disabled_linux_subsystem_expanded;
                self.disabled_linux_subsystem_expanded
            }
            (SubscriptionSection::Disabled, SubscriptionCategory::QemuSubsystem) => {
                self.disabled_qemu_subsystem_expanded = !self.disabled_qemu_subsystem_expanded;
                self.disabled_qemu_subsystem_expanded
            }
        };
        let state = if expanded { "expanded" } else { "collapsed" };
        self.status = format!("{} {} group {}", section.label(), category.label(), state);
        self.clamp_subscription_row_selection();
        self.persist_ui_state();
    }

    fn push_subscription_group_rows(
        &self,
        rows: &mut Vec<SubscriptionRow>,
        section: SubscriptionSection,
        items: Vec<(usize, &SubscriptionItem)>,
    ) {
        // Leave uncategorized entries visible above subsystem buckets so
        // `Following` and user-added mailboxes stay easy to discover.
        for (index, item) in items
            .iter()
            .copied()
            .filter(|(_, item)| item.category.is_none())
        {
            rows.push(SubscriptionRow {
                kind: SubscriptionRowKind::Item(index),
                text: format!(
                    "  {}",
                    subscription_line(item, self.mailbox_sync_status(&item.mailbox))
                ),
            });
        }

        for category in SubscriptionCategory::ALL {
            let category_items: Vec<(usize, &SubscriptionItem)> = items
                .iter()
                .copied()
                .filter(|(_, item)| item.category == Some(category))
                .collect();
            if category_items.is_empty() {
                continue;
            }

            let expanded = self.subscription_category_expanded(section, category);
            let marker = if expanded { "▼" } else { "▶" };
            rows.push(SubscriptionRow {
                kind: SubscriptionRowKind::CategoryHeader { section, category },
                text: format!("  {marker} {} ({})", category.label(), category_items.len()),
            });

            if expanded {
                for (index, item) in category_items {
                    rows.push(SubscriptionRow {
                        kind: SubscriptionRowKind::Item(index),
                        text: format!(
                            "    {}",
                            subscription_line(item, self.mailbox_sync_status(&item.mailbox))
                        ),
                    });
                }
            }
        }
    }

    fn subscription_rows(&self) -> Vec<SubscriptionRow> {
        let enabled_count = self
            .subscriptions
            .iter()
            .filter(|item| item.enabled)
            .count();
        let disabled_count = self.subscriptions.len().saturating_sub(enabled_count);

        let mut rows = Vec::new();
        let enabled_marker = if self.enabled_group_expanded {
            "▼"
        } else {
            "▶"
        };
        rows.push(SubscriptionRow {
            kind: SubscriptionRowKind::EnabledHeader,
            text: format!("{enabled_marker} enabled ({enabled_count})"),
        });

        if self.enabled_group_expanded {
            let items: Vec<(usize, &SubscriptionItem)> = self
                .subscriptions
                .iter()
                .enumerate()
                .filter(|(_, item)| item.enabled)
                .collect();
            self.push_subscription_group_rows(&mut rows, SubscriptionSection::Enabled, items);
        }

        let disabled_marker = if self.disabled_group_expanded {
            "▼"
        } else {
            "▶"
        };
        rows.push(SubscriptionRow {
            kind: SubscriptionRowKind::DisabledHeader,
            text: format!("{disabled_marker} disabled ({disabled_count})"),
        });

        if self.disabled_group_expanded {
            let items: Vec<(usize, &SubscriptionItem)> = self
                .subscriptions
                .iter()
                .enumerate()
                .filter(|(_, item)| !item.enabled)
                .collect();
            self.push_subscription_group_rows(&mut rows, SubscriptionSection::Disabled, items);
        }

        rows
    }

    fn selected_subscription_row_kind(&self) -> Option<SubscriptionRowKind> {
        let rows = self.subscription_rows();
        if rows.is_empty() {
            return None;
        }

        let selected = self
            .subscription_row_index
            .min(rows.len().saturating_sub(1));
        rows.get(selected).map(|row| row.kind)
    }

    fn selected_subscription_index(&self) -> Option<usize> {
        match self.selected_subscription_row_kind() {
            Some(SubscriptionRowKind::Item(index)) => Some(index),
            _ => None,
        }
    }

    fn sync_subscription_row_to_selected_item(&mut self) {
        let rows = self.subscription_rows();
        if rows.is_empty() {
            self.subscription_row_index = 0;
            return;
        }

        self.subscription_row_index = rows
            .iter()
            .position(
                |row| matches!(row.kind, SubscriptionRowKind::Item(index) if index == self.subscription_index),
            )
            .unwrap_or(0);
    }

    fn clamp_subscription_row_selection(&mut self) {
        let rows = self.subscription_rows();
        if rows.is_empty() {
            self.subscription_row_index = 0;
            return;
        }

        if self.subscription_row_index >= rows.len() {
            self.subscription_row_index = rows.len().saturating_sub(1);
        }

        if let Some(SubscriptionRowKind::Item(index)) =
            rows.get(self.subscription_row_index).map(|row| row.kind)
        {
            self.subscription_index = index;
        }
    }

    fn set_current_subscription_enabled(&mut self, enabled: bool) {
        let Some(selected_index) = self.selected_subscription_index() else {
            self.status = "move to a subscription item, then press y/n".to_string();
            return;
        };

        let mailbox = self.subscriptions[selected_index].mailbox.clone();
        let label = self.subscriptions[selected_index].label.clone();
        if let Some(item) = self.subscriptions.get_mut(selected_index) {
            item.enabled = enabled;
        }

        self.sort_subscriptions_keep_selected(&mailbox);
        let marker = if enabled { "enabled" } else { "disabled" };
        self.status = format!("{marker} subscription {label}");
        self.reconcile_inbox_auto_sync();
        self.reconcile_subscription_auto_sync();
        self.persist_ui_state();
    }

    fn sort_subscriptions_keep_selected(&mut self, selected_mailbox: &str) {
        self.subscriptions.sort_by(compare_subscription_items);

        self.subscription_index = self
            .subscriptions
            .iter()
            .position(|item| same_mailbox_name(&item.mailbox, selected_mailbox))
            .unwrap_or(0);
        self.sync_subscription_row_to_selected_item();
    }

    fn toggle_selected_subscription_group(&mut self) {
        match self.selected_subscription_row_kind() {
            Some(SubscriptionRowKind::EnabledHeader) => {
                self.enabled_group_expanded = !self.enabled_group_expanded;
                let state = if self.enabled_group_expanded {
                    "expanded"
                } else {
                    "collapsed"
                };
                self.status = format!("enabled group {state}");
                self.clamp_subscription_row_selection();
                self.persist_ui_state();
            }
            Some(SubscriptionRowKind::DisabledHeader) => {
                self.disabled_group_expanded = !self.disabled_group_expanded;
                let state = if self.disabled_group_expanded {
                    "expanded"
                } else {
                    "collapsed"
                };
                self.status = format!("disabled group {state}");
                self.clamp_subscription_row_selection();
                self.persist_ui_state();
            }
            Some(SubscriptionRowKind::CategoryHeader { section, category }) => {
                self.toggle_subscription_category_group(section, category);
            }
            _ => {}
        }
    }

    fn handle_subscription_enter(&mut self) {
        match self.selected_subscription_row_kind() {
            Some(SubscriptionRowKind::EnabledHeader)
            | Some(SubscriptionRowKind::DisabledHeader)
            | Some(SubscriptionRowKind::CategoryHeader { .. }) => {
                self.toggle_selected_subscription_group()
            }
            Some(SubscriptionRowKind::Item(_)) => self.open_threads_for_selected_subscription(),
            None => {}
        }
    }

    fn open_threads_for_selected_subscription(&mut self) {
        let Some(selected_index) = self.selected_subscription_index() else {
            self.status = "press Enter on a subscription item".to_string();
            return;
        };
        let Some(item) = self.subscriptions.get(selected_index) else {
            return;
        };
        let mailbox = item.mailbox.clone();
        let enabled = item.enabled;
        tracing::debug!(mailbox = %mailbox, enabled, "user opened subscription");

        if !enabled {
            self.status = format!("subscription {} is disabled, press y to enable", mailbox);
            return;
        }

        match self.load_thread_rows_for_view(&mailbox, 500) {
            Ok(rows) if !rows.is_empty() => {
                self.show_mailbox_threads(
                    &mailbox,
                    rows,
                    format!("showing threads for {}", mailbox),
                    true,
                );
                self.focus = Pane::Threads;
            }
            Ok(_) => {
                if self.mailbox_sync_pending(&mailbox) {
                    self.show_mailbox_threads(
                        &mailbox,
                        Vec::new(),
                        format!("{mailbox} is syncing in background; page stays responsive"),
                        true,
                    );
                    self.focus = Pane::Threads;
                    return;
                }

                let outcome = self
                    .start_manual_sync(vec![mailbox.clone()], ManualSyncOrigin::SubscriptionOpen);
                let background_status = match outcome {
                    ManualSyncRequestOutcome::Started
                    | ManualSyncRequestOutcome::AlreadySyncing => {
                        format!("{mailbox} is syncing in background; page stays responsive")
                    }
                    ManualSyncRequestOutcome::Busy => {
                        "another background sync is running; page stays responsive".to_string()
                    }
                };
                self.show_mailbox_threads(&mailbox, Vec::new(), background_status, true);
                self.focus = Pane::Threads;
            }
            Err(error) => {
                tracing::error!(
                    mailbox = %mailbox,
                    error = %error,
                    "failed to load mailbox thread rows"
                );
                self.status = format!("failed to load threads for {}: {error}", mailbox);
            }
        }
    }

    fn selected_thread(&self) -> Option<&ThreadRow> {
        self.filtered_thread_indices
            .get(self.thread_index)
            .and_then(|index| self.threads.get(*index))
    }

    fn selected_mail_preview(&self) -> Option<&MailPreview> {
        self.selected_mail_preview.as_ref()
    }

    fn refresh_selected_mail_preview(&mut self) {
        self.selected_mail_preview = self.selected_thread().map(load_mail_preview);
    }

    fn selected_series(&self) -> Option<&patch_worker::SeriesSummary> {
        let thread = self.selected_thread()?;
        self.series_summaries.get(&thread.thread_id)
    }

    fn open_reply_panel(&mut self, require_preview_focus: bool) {
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "reply is only available on mail page".to_string();
            return;
        }
        if require_preview_focus && !matches!(self.focus, Pane::Preview) {
            self.status = "move focus to Preview, then press e to reply".to_string();
            return;
        }

        let Some(thread) = self.selected_thread().cloned() else {
            self.status = "select a mail thread before replying".to_string();
            return;
        };
        let Some(raw_path) = thread.raw_path.clone() else {
            self.status = "selected mail has no raw source; cannot build reply draft".to_string();
            return;
        };

        let raw = match fs::read(&raw_path) {
            Ok(raw) => raw,
            Err(error) => {
                self.status = format!("failed to read {}: {}", raw_path.display(), error);
                return;
            }
        };

        let identity_resolver = self.reply_identity_resolver;
        let identity = match identity_resolver() {
            Ok(identity) => identity,
            Err(error) => {
                self.status = format!("reply identity unavailable: {error}");
                return;
            }
        };

        let mut self_addresses = vec![identity.email.clone()];
        if let Some(email) = self.runtime.imap.email.as_ref() {
            self_addresses.push(email.clone());
        }

        let draft = if self.runtime.database_path.exists() {
            match reply_store::load_reply_draft(
                &self.runtime.database_path,
                thread.thread_id,
                thread.mail_id,
            ) {
                Ok(draft) => draft,
                Err(error) => {
                    tracing::warn!(
                        thread_id = thread.thread_id,
                        mail_id = thread.mail_id,
                        error = %error,
                        "failed to restore reply draft; using a fresh seed"
                    );
                    None
                }
            }
        } else {
            None
        };
        let restored = draft.is_some();
        let panel = if let Some(draft) = draft {
            ReplyPanelState::from_draft(draft, self_addresses)
        } else {
            let seed = build_reply_seed(&raw, &thread, &identity, &self_addresses);
            ReplyPanelState::new(seed, self_addresses, thread.mail_id, thread.thread_id)
        };
        self.reply_panel = Some(panel);
        self.persist_reply_draft();
        self.status = if restored {
            format!(
                "reply draft restored for <{}>; review it before Send Preview",
                thread.message_id
            )
        } else {
            format!(
                "reply panel opened for <{}>; edit From/To/Cc/Subject before Send Preview",
                thread.message_id
            )
        };
    }

    fn reply_self_addresses(&self, identity: &ReplyIdentity) -> Vec<String> {
        let mut self_addresses = vec![identity.email.clone()];
        if let Some(email) = self.runtime.imap.email.as_ref() {
            self_addresses.push(email.clone());
        }
        self_addresses
    }

    fn open_compose_panel(&mut self) {
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "compose is only available on mail page".to_string();
            return;
        }
        if !self.runtime.database_path.exists() {
            self.status = "compose is unavailable before database bootstrap".to_string();
            return;
        }

        let identity = match (self.reply_identity_resolver)() {
            Ok(identity) => identity,
            Err(error) => {
                self.status = format!("compose identity unavailable: {error}");
                return;
            }
        };
        let self_addresses = self.reply_self_addresses(&identity);
        let message_id = standalone_draft_message_id();
        let (thread_id, mail_id) = match reply_store::create_draft_anchor(
            &self.runtime.database_path,
            &message_id,
            "",
            &identity.display,
        ) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::error!(op = "reply.compose", status = "failed", error = %error);
                self.status = format!("compose draft initialization failed: {error}");
                return;
            }
        };
        let panel = ReplyPanelState::new(
            ReplySeed {
                from: identity.display,
                to: String::new(),
                cc: String::new(),
                subject: String::new(),
                in_reply_to: String::new(),
                references: Vec::new(),
                body: vec![String::new()],
            },
            self_addresses,
            mail_id,
            thread_id,
        );
        self.reply_panel = Some(panel);
        let draft_id = self.persist_reply_draft();
        self.status = match draft_id {
            Some(draft_id) => format!(
                "compose panel opened; enter recipients and subject before Send Preview (draft #{draft_id})"
            ),
            None => {
                "compose panel opened; enter recipients and subject before Send Preview".to_string()
            }
        };
    }

    fn open_forward_panel(&mut self) {
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "forward is only available on mail page".to_string();
            return;
        }
        let Some(thread) = self.selected_thread().cloned() else {
            self.status = "select a mail thread before forwarding".to_string();
            return;
        };
        let Some(raw_path) = thread.raw_path.clone() else {
            self.status = "selected mail has no raw source; cannot build forward draft".to_string();
            return;
        };
        if !self.runtime.database_path.exists() {
            self.status = "forward is unavailable before database bootstrap".to_string();
            return;
        }
        let raw = match fs::read(&raw_path) {
            Ok(raw) => raw,
            Err(error) => {
                self.status = format!("failed to read {}: {}", raw_path.display(), error);
                return;
            }
        };
        let identity = match (self.reply_identity_resolver)() {
            Ok(identity) => identity,
            Err(error) => {
                self.status = format!("forward identity unavailable: {error}");
                return;
            }
        };
        let self_addresses = self.reply_self_addresses(&identity);
        let mut body = vec![
            "---------- Forwarded message ----------".to_string(),
            format!("From: {}", thread.from_addr),
            format!("Subject: {}", thread.subject),
            String::new(),
        ];
        let forwarded = extract_mail_body_text(&raw);
        if !forwarded.trim().is_empty() {
            body.extend(forwarded.lines().map(ToOwned::to_owned));
        }
        let subject = if thread.subject.trim().is_empty() {
            "Fwd: <no subject>".to_string()
        } else {
            format!("Fwd: {}", thread.subject.trim())
        };
        let panel = ReplyPanelState::new(
            ReplySeed {
                from: identity.display,
                to: String::new(),
                cc: String::new(),
                subject,
                in_reply_to: String::new(),
                references: Vec::new(),
                body,
            },
            self_addresses,
            thread.mail_id,
            thread.thread_id,
        );
        self.reply_panel = Some(panel);
        self.persist_reply_draft();
        self.status = format!(
            "forward panel opened for <{}>; enter recipients before Send Preview",
            thread.message_id
        );
    }

    fn open_reply_draft_by_id(&mut self, draft_id: i64) {
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "outbox drafts are only available on mail page".to_string();
            return;
        }
        if !self.runtime.database_path.exists() {
            self.status = "reply outbox is unavailable before database bootstrap".to_string();
            return;
        }
        let draft = match reply_store::load_reply_draft_by_id(&self.runtime.database_path, draft_id)
        {
            Ok(Some(draft)) => draft,
            Ok(None) => {
                self.status = format!("reply draft #{draft_id} was not found");
                return;
            }
            Err(error) => {
                tracing::error!(op = "reply.outbox.open", draft_id, error = %error);
                self.status = format!("failed to load reply draft #{draft_id}: {error}");
                return;
            }
        };
        let identity = match (self.reply_identity_resolver)() {
            Ok(identity) => identity,
            Err(error) => {
                self.status = format!("reply identity unavailable: {error}");
                return;
            }
        };
        let self_addresses = self.reply_self_addresses(&identity);
        self.reply_panel = Some(ReplyPanelState::from_draft(draft, self_addresses));
        self.persist_reply_draft();
        self.status = format!("reply draft #{draft_id} restored; review it before Send Preview");
    }

    fn handle_palette_outbox(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let _ = parts.next();
        let Some(value) = parts.next() else {
            self.show_reply_outbox();
            return;
        };
        if parts.next().is_some() {
            self.status = "usage: outbox [DRAFT_ID]".to_string();
            return;
        }
        let Ok(draft_id) = value.parse::<i64>() else {
            self.status = "usage: outbox [DRAFT_ID]".to_string();
            return;
        };
        if draft_id <= 0 {
            self.status = "usage: outbox [DRAFT_ID]".to_string();
            return;
        }
        self.open_reply_draft_by_id(draft_id);
    }

    fn close_reply_panel(&mut self, status: impl Into<String>) {
        self.persist_reply_draft();
        self.reply_panel = None;
        self.status = status.into();
    }

    fn discard_reply_panel(&mut self, status: impl Into<String>) {
        if let Some(panel) = self.reply_panel.as_ref()
            && self.runtime.database_path.exists()
            && let Err(error) = reply_store::delete_reply_draft(
                &self.runtime.database_path,
                panel.thread_id,
                panel.mail_id,
            )
        {
            tracing::warn!(
                thread_id = panel.thread_id,
                mail_id = panel.mail_id,
                error = %error,
                "failed to delete reply draft"
            );
        }
        self.reply_panel = None;
        self.status = status.into();
    }

    fn persist_reply_draft(&mut self) -> Option<i64> {
        let panel = self.reply_panel.as_ref()?;
        if !self.runtime.database_path.exists() {
            // Unit-test runtimes and pre-bootstrap callers may not have a DB;
            // the live TUI always runs after bootstrap creates it.
            return None;
        }
        match reply_store::upsert_reply_draft(
            &self.runtime.database_path,
            &panel.to_draft_request(),
        ) {
            Ok(id) => Some(id),
            Err(error) => {
                tracing::warn!(
                    thread_id = panel.thread_id,
                    mail_id = panel.mail_id,
                    error = %error,
                    "failed to persist reply draft"
                );
                None
            }
        }
    }

    fn open_reply_notice(
        &mut self,
        kind: ReplyNoticeKind,
        title: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
        action: Option<ReplyNoticeAction>,
        status: impl Into<String>,
    ) {
        if let Some(panel) = self.reply_panel.as_mut() {
            panel.reply_notice = Some(ReplyNoticeState {
                kind,
                title: title.into(),
                message: message.into(),
                hint: hint.into(),
                action,
            });
        }
        self.status = status.into();
    }

    fn close_reply_notice(&mut self, status: impl Into<String>) {
        if let Some(panel) = self.reply_panel.as_mut() {
            panel.reply_notice = None;
        }
        self.status = status.into();
    }

    fn open_send_preview(&mut self) {
        let Some(panel) = self.reply_panel.as_mut() else {
            self.status = "reply panel is not open".to_string();
            return;
        };
        panel.reply_notice = None;

        let ReplyPreview {
            content,
            lines,
            errors,
            warnings,
        } = render_reply_preview(ReplyPreviewRequest {
            from: &panel.from,
            to: &panel.to,
            cc: &panel.cc,
            subject: &panel.subject,
            in_reply_to: &panel.in_reply_to,
            references: &panel.references,
            body: &panel.body,
            self_addresses: &panel.self_addresses,
        });
        panel.preview_rendered = content;
        panel.preview_lines = lines;
        panel.preview_errors = errors;
        panel.preview_warnings = warnings;
        panel.preview_open = true;
        panel.preview_scroll = 0;

        if !panel.preview_errors.is_empty() {
            self.status = format!("send preview blocked: {}", panel.preview_errors.join("; "));
        } else if !panel.preview_warnings.is_empty() {
            self.status = format!(
                "send preview warning: {}; press Enter/c to confirm anyway",
                panel.preview_warnings.join("; ")
            );
        } else {
            self.status = "send preview ready; press Enter/c to confirm".to_string();
        }
    }

    fn close_send_preview(&mut self, status: impl Into<String>) {
        if let Some(panel) = self.reply_panel.as_mut() {
            panel.preview_open = false;
        }
        self.status = status.into();
    }

    fn confirm_send_preview(&mut self) {
        let Some(panel) = self.reply_panel.as_mut() else {
            self.status = "reply panel is not open".to_string();
            return;
        };
        if !panel.preview_open {
            self.status = "open Send Preview first".to_string();
            return;
        }
        if !panel.preview_errors.is_empty() {
            self.status = format!(
                "cannot confirm send preview: {}",
                panel.preview_errors.join("; ")
            );
            return;
        }

        panel.preview_open = false;
        panel.preview_confirmed = true;
        panel.preview_confirmed_at = Some(now_timestamp());
        self.open_reply_notice(
            ReplyNoticeKind::Info,
            "Ready To Send",
            "Send Preview has been confirmed. Press S to send the reply, or Esc/Enter to keep editing.",
            "S send | Esc/Enter close",
            Some(ReplyNoticeAction::Send),
            "send preview confirmed; ready to send",
        );
    }

    fn attempt_reply_send(&mut self) {
        let Some(panel) = self.reply_panel.as_ref().cloned() else {
            self.status = "reply panel is not open".to_string();
            return;
        };
        if !panel.preview_confirmed {
            self.open_reply_notice(
                ReplyNoticeKind::Warning,
                "Send Blocked",
                "You must open Send Preview and confirm it before CRIEW will send this reply.",
                "P preview | Esc/Enter close",
                Some(ReplyNoticeAction::OpenPreview),
                "send blocked: run Send Preview and confirm first",
            );
            return;
        }

        let (prepared, errors) = prepare_reply_message(ReplyPreviewRequest {
            from: &panel.from,
            to: &panel.to,
            cc: &panel.cc,
            subject: &panel.subject,
            in_reply_to: &panel.in_reply_to,
            references: &panel.references,
            body: &panel.body,
            self_addresses: &panel.self_addresses,
        });
        if !errors.is_empty() {
            self.status = format!("send blocked: {}", errors.join("; "));
            return;
        }

        let request = build_send_request(&panel, prepared);
        tracing::info!(
            op = "reply.send",
            status = "started",
            mail_id = request.mail_id,
            thread_id = request.thread_id,
            "sending reply"
        );
        let outcome = (self.reply_send_executor)(&self.runtime, &request);
        let persist_result = persist_reply_send_result(&self.runtime, &request, &outcome);

        match outcome.status {
            SendStatus::Sent => {
                tracing::info!(
                    op = "reply.send",
                    status = "sent",
                    mail_id = request.mail_id,
                    thread_id = request.thread_id,
                    message_id = %outcome.message_id,
                    "reply sent"
                );
                let persist_error = persist_result.err();
                let status = if let Some(error) = persist_error.as_ref() {
                    format!(
                        "reply sent as <{}> but failed to persist send result: {}",
                        outcome.message_id, error
                    )
                } else {
                    format!("reply sent as <{}>", outcome.message_id)
                };
                if persist_error.is_none() && is_following_view(&self.active_thread_mailbox) {
                    let active_mailbox = self.active_thread_mailbox.clone();
                    if let Err(error) =
                        self.reload_mailbox_threads_preserving_selection(&active_mailbox)
                    {
                        tracing::warn!(
                            op = "following.local_send",
                            error = %error,
                            "sent patch was recorded but Following view could not be refreshed"
                        );
                    }
                }
                self.discard_reply_panel(status);
            }
            SendStatus::Failed | SendStatus::TimedOut => {
                let summary = outcome
                    .error_summary
                    .as_deref()
                    .unwrap_or("reply send failed");
                tracing::error!(
                    op = "reply.send",
                    status = "failed",
                    mail_id = request.mail_id,
                    thread_id = request.thread_id,
                    message_id = %outcome.message_id,
                    exit_code = outcome.exit_code,
                    timed_out = outcome.timed_out,
                    error = %summary,
                    "reply send failed"
                );
                if let Some(panel) = self.reply_panel.as_mut() {
                    panel.draft_status = ReplyDraftStatus::Failed;
                    panel.draft_path = outcome.draft_path.clone();
                    panel.last_error = outcome.error_summary.clone();
                }
                self.status = if let Err(error) = persist_result {
                    format!(
                        "send failed: {}; retry with S after fixing the issue (persist failed: {})",
                        summary, error
                    )
                } else {
                    format!(
                        "send failed: {}; retry with S after fixing the issue",
                        summary
                    )
                };
            }
        }
    }

    fn execute_reply_command(&mut self) {
        let command = if let Some(panel) = self.reply_panel.as_mut() {
            let command = panel.command_input.trim().to_string();
            panel.command_input.clear();
            panel.mode = ReplyEditMode::Normal;
            command
        } else {
            self.status = "reply panel is not open".to_string();
            return;
        };

        if command.is_empty() {
            self.status = "empty command".to_string();
            return;
        }

        match command.as_str() {
            "q!" => self.discard_reply_panel("discarded reply draft"),
            "q" => {
                if self.reply_panel.as_ref().is_some_and(|panel| panel.dirty) {
                    self.status = "unsaved reply draft, run :q! to discard".to_string();
                } else {
                    self.close_reply_panel("closed reply panel");
                }
            }
            "preview" => self.open_send_preview(),
            "send" => self.attempt_reply_send(),
            _ => {
                self.status = format!("unsupported command: :{command}");
            }
        }
    }

    fn run_patch_action(&mut self, action: patch_worker::PatchAction) {
        tracing::info!(
            op = "patch.action",
            status = "started",
            action = action.name()
        );
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "patch action is only available on mail page".to_string();
            return;
        }

        let Some(series) = self.selected_series().cloned() else {
            self.status = "current thread is not a patch series".to_string();
            return;
        };

        match patch_worker::run_action(&self.runtime, &series, action) {
            Ok(result) => {
                if let Some(series_summary) = self.series_summaries.get_mut(&series.thread_id) {
                    series_summary.status = result.status;
                }
                if matches!(action, patch_worker::PatchAction::Apply)
                    && result.status == crate::domain::models::PatchSeriesStatus::Applied
                    && let (Some(before_head), Some(after_head)) =
                        (result.head_before.as_deref(), result.head_after.as_deref())
                {
                    self.last_apply_snapshot = Some(LastApplySnapshot {
                        thread_id: series.thread_id,
                        before_head: before_head.to_string(),
                        after_head: after_head.to_string(),
                    });
                }
                let exit_code = result
                    .exit_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let output_dir = result
                    .output_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "-".to_string());
                self.status = format!(
                    "{}: {} (status={} exit={} timeout={})",
                    action.name(),
                    result.summary,
                    match result.status {
                        crate::domain::models::PatchSeriesStatus::New => "new",
                        crate::domain::models::PatchSeriesStatus::Reviewing => "reviewing",
                        crate::domain::models::PatchSeriesStatus::Applied => "applied",
                        crate::domain::models::PatchSeriesStatus::Failed => "failed",
                        crate::domain::models::PatchSeriesStatus::Conflict => "conflict",
                    },
                    exit_code,
                    result.timed_out
                );
                tracing::info!(
                    op = "patch.action",
                    status = "succeeded",
                    action = action.name(),
                    command = %result.command_line,
                    output_dir = %output_dir,
                );
            }
            Err(error) => {
                tracing::error!(
                    op = "patch.action",
                    status = "failed",
                    action = action.name(),
                    error = %error
                );
                self.status = format!("{} failed: {}", action.name(), error);
            }
        }
    }

    fn run_patch_preflight(&mut self) {
        tracing::info!(op = "patch.preflight", status = "started");
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "patch preflight is only available on mail page".to_string();
            return;
        }

        let Some(series) = self.selected_series().cloned() else {
            self.status = "current thread is not a patch series".to_string();
            return;
        };

        match patch_worker::run_preflight(&self.runtime, &series) {
            Ok(result) => {
                self.status = format!("preflight: {}", result.summary);
                tracing::info!(
                    op = "patch.preflight",
                    status = result.status_label(),
                    summary = %result.summary,
                    checkpatch = result.checkpatch.status.as_str(),
                    maintainers = result.maintainers.status.as_str(),
                );
            }
            Err(error) => {
                tracing::error!(op = "patch.preflight", status = "failed", error = %error);
                self.status = format!("preflight failed: {error}");
            }
        }
    }

    fn show_reply_outbox(&mut self) {
        if !self.runtime.database_path.exists() {
            self.status = "reply outbox is unavailable before database bootstrap".to_string();
            return;
        }
        match reply_store::list_reply_outbox(&self.runtime.database_path) {
            Ok(entries) if entries.is_empty() => {
                self.status = "reply outbox is empty".to_string();
            }
            Ok(entries) => {
                let failed = entries
                    .iter()
                    .filter(|entry| entry.status == ReplyDraftStatus::Failed)
                    .count();
                self.status = format!(
                    "reply outbox: {} pending ({} failed; use outbox ID to reopen)",
                    entries.len(),
                    failed
                );
            }
            Err(error) => {
                tracing::error!(op = "reply.outbox", status = "failed", error = %error);
                self.status = format!("reply outbox failed: {error}");
            }
        }
    }

    fn run_patch_undo_action(&mut self) {
        tracing::info!(op = "patch.undo", status = "started");
        if !matches!(self.ui_page, UiPage::Mail) {
            self.status = "undo is only available on mail page".to_string();
            return;
        }

        let Some(snapshot) = self.last_apply_snapshot.clone() else {
            self.status = "no apply action to undo in this session".to_string();
            return;
        };

        match patch_worker::undo_last_apply(
            &self.runtime,
            &snapshot.before_head,
            &snapshot.after_head,
        ) {
            Ok(head_after_reset) => {
                if let Some(series_summary) = self.series_summaries.get_mut(&snapshot.thread_id) {
                    series_summary.status = crate::domain::models::PatchSeriesStatus::New;
                }
                self.last_apply_snapshot = None;
                self.status = format!(
                    "undo apply: reset HEAD to {}",
                    short_commit_id(&head_after_reset)
                );
                tracing::info!(
                    op = "patch.undo",
                    status = "succeeded",
                    thread_id = snapshot.thread_id,
                    head = %head_after_reset
                );
            }
            Err(error) => {
                tracing::error!(op = "patch.undo", status = "failed", error = %error);
                self.status = format!("undo apply failed: {error}");
            }
        }
    }

    fn selected_kernel_tree_row(&self) -> Option<&KernelTreeRow> {
        self.kernel_tree_rows.get(
            self.kernel_tree_row_index
                .min(self.kernel_tree_rows.len().saturating_sub(1)),
        )
    }

    fn selected_kernel_tree_path(&self) -> Option<PathBuf> {
        self.selected_kernel_tree_row().map(|row| row.path.clone())
    }

    fn selected_kernel_tree_file_path(&self) -> Option<&Path> {
        self.selected_kernel_tree_row()
            .filter(|row| row.is_file())
            .map(|row| row.path.as_path())
    }

    fn refresh_kernel_tree_rows(&mut self, selected_path_hint: Option<&Path>) {
        self.kernel_tree_rows =
            build_kernel_tree_rows(&self.runtime.kernel_trees, &self.kernel_tree_expanded_paths);
        if self.kernel_tree_rows.is_empty() {
            self.kernel_tree_row_index = 0;
            return;
        }

        if let Some(path) = selected_path_hint
            && let Some(index) = self
                .kernel_tree_rows
                .iter()
                .position(|row| row.path == path)
        {
            self.kernel_tree_row_index = index;
            return;
        }

        if self.kernel_tree_row_index >= self.kernel_tree_rows.len() {
            self.kernel_tree_row_index = self.kernel_tree_rows.len().saturating_sub(1);
        }
    }

    fn supports_code_browser(&self) -> bool {
        !self.runtime.kernel_trees.is_empty()
    }

    fn toggle_ui_page(&mut self) {
        if matches!(self.ui_page, UiPage::Mail) && !self.supports_code_browser() {
            self.status =
                "no kernel tree configured; set [kernel].tree or [kernel].trees".to_string();
            return;
        }

        self.ui_page = self.ui_page.toggled();
        match self.ui_page {
            UiPage::Mail => {
                self.status = "switched to mail page".to_string();
            }
            UiPage::CodeBrowser => {
                self.refresh_kernel_tree_rows(self.selected_kernel_tree_path().as_deref());
                self.code_preview_scroll = 0;
                self.status = "switched to code browser page".to_string();
            }
        }
    }

    fn move_subscription_up(&mut self) {
        let rows = self.subscription_rows();
        if rows.is_empty() {
            return;
        }
        if self.subscription_row_index >= rows.len() {
            self.subscription_row_index = rows.len().saturating_sub(1);
        }
        if self.subscription_row_index > 0 {
            self.subscription_row_index -= 1;
        }
        if let Some(SubscriptionRowKind::Item(index)) =
            rows.get(self.subscription_row_index).map(|row| row.kind)
        {
            self.subscription_index = index;
        }
    }

    fn move_subscription_down(&mut self) {
        let rows = self.subscription_rows();
        if rows.is_empty() {
            return;
        }
        if self.subscription_row_index >= rows.len() {
            self.subscription_row_index = rows.len().saturating_sub(1);
        } else if self.subscription_row_index + 1 < rows.len() {
            self.subscription_row_index += 1;
        }
        if let Some(SubscriptionRowKind::Item(index)) =
            rows.get(self.subscription_row_index).map(|row| row.kind)
        {
            self.subscription_index = index;
        }
    }

    fn move_kernel_tree_up(&mut self) {
        let previous_file = self.selected_kernel_tree_file_path().map(Path::to_path_buf);
        if self.kernel_tree_row_index > 0 {
            self.kernel_tree_row_index -= 1;
        }
        let next_file = self.selected_kernel_tree_file_path().map(Path::to_path_buf);
        if previous_file != next_file {
            self.code_preview_scroll = 0;
        }
    }

    fn move_kernel_tree_down(&mut self) {
        let previous_file = self.selected_kernel_tree_file_path().map(Path::to_path_buf);
        if self.kernel_tree_row_index + 1 < self.kernel_tree_rows.len() {
            self.kernel_tree_row_index += 1;
        }
        let next_file = self.selected_kernel_tree_file_path().map(Path::to_path_buf);
        if previous_file != next_file {
            self.code_preview_scroll = 0;
        }
    }

    fn handle_kernel_tree_enter(&mut self) {
        let Some(row) = self.selected_kernel_tree_row().cloned() else {
            self.status = "kernel tree is empty".to_string();
            return;
        };

        if !row.expandable {
            self.code_preview_scroll = 0;
            self.status = format!("selected {}", row.path.display());
            return;
        }

        if row.expanded {
            self.kernel_tree_expanded_paths.remove(&row.path);
            self.status = format!("collapsed {}", row.path.display());
        } else {
            self.kernel_tree_expanded_paths.insert(row.path.clone());
            self.status = format!("expanded {}", row.path.display());
        }
        self.refresh_kernel_tree_rows(Some(&row.path));
        self.code_preview_scroll = 0;
    }

    fn move_focus_next(&mut self) {
        match self.ui_page {
            UiPage::Mail => {
                self.focus = self.focus.next();
            }
            UiPage::CodeBrowser => {
                self.code_focus = self.code_focus.next();
            }
        }
    }

    fn move_focus_previous(&mut self) {
        match self.ui_page {
            UiPage::Mail => {
                self.focus = self.focus.previous();
            }
            UiPage::CodeBrowser => {
                self.code_focus = self.code_focus.previous();
            }
        }
    }

    fn resize_mail_panes(
        &mut self,
        direction: HorizontalResizeDirection,
        resize_mode: MailPaneResizeMode,
    ) {
        let did_resize = match (self.focus, direction, resize_mode) {
            (Pane::Subscriptions, HorizontalResizeDirection::Left, _) => false,
            (Pane::Subscriptions, HorizontalResizeDirection::Right, MailPaneResizeMode::Expand) => {
                self.grow_mail_subscriptions_pane()
            }
            (Pane::Subscriptions, HorizontalResizeDirection::Right, MailPaneResizeMode::Shrink) => {
                self.shrink_mail_subscriptions_pane()
            }
            (Pane::Threads, HorizontalResizeDirection::Left, MailPaneResizeMode::Expand) => {
                self.shrink_mail_subscriptions_pane()
            }
            (Pane::Threads, HorizontalResizeDirection::Left, MailPaneResizeMode::Shrink) => {
                self.grow_mail_subscriptions_pane()
            }
            (Pane::Threads, HorizontalResizeDirection::Right, MailPaneResizeMode::Expand) => {
                self.shrink_mail_preview_pane()
            }
            (Pane::Threads, HorizontalResizeDirection::Right, MailPaneResizeMode::Shrink) => {
                self.grow_mail_preview_pane()
            }
            (Pane::Preview, HorizontalResizeDirection::Left, MailPaneResizeMode::Expand) => {
                self.grow_mail_preview_pane()
            }
            (Pane::Preview, HorizontalResizeDirection::Left, MailPaneResizeMode::Shrink) => {
                self.shrink_mail_preview_pane()
            }
            (Pane::Preview, HorizontalResizeDirection::Right, _) => false,
        };

        if did_resize {
            self.persist_ui_state();
            self.status = format!(
                "mail panes saved: subscriptions {} cols, preview {} cols",
                self.mail_pane_layout.subscriptions_width, self.mail_pane_layout.preview_width
            );
        } else {
            self.status = match resize_mode {
                MailPaneResizeMode::Expand => "mail pane cannot expand in that direction",
                MailPaneResizeMode::Shrink => "mail pane cannot shrink in that direction",
            }
            .to_string();
        }
    }

    fn grow_mail_subscriptions_pane(&mut self) -> bool {
        self.mail_pane_layout.subscriptions_width = self
            .mail_pane_layout
            .subscriptions_width
            .saturating_add(MAIL_PANE_RESIZE_STEP);
        true
    }

    fn shrink_mail_subscriptions_pane(&mut self) -> bool {
        shrink_mail_pane_width(
            &mut self.mail_pane_layout.subscriptions_width,
            MIN_MAIL_SUBSCRIPTIONS_WIDTH,
        )
    }

    fn grow_mail_preview_pane(&mut self) -> bool {
        self.mail_pane_layout.preview_width = self
            .mail_pane_layout
            .preview_width
            .saturating_add(MAIL_PANE_RESIZE_STEP);
        true
    }

    fn shrink_mail_preview_pane(&mut self) -> bool {
        shrink_mail_pane_width(
            &mut self.mail_pane_layout.preview_width,
            MIN_MAIL_PREVIEW_WIDTH,
        )
    }

    fn select_filtered_thread_at(&mut self, filtered_index: usize) {
        debug_assert!(filtered_index < self.filtered_thread_indices.len());
        self.thread_index = filtered_index;
        self.preview_scroll = 0;
        self.refresh_selected_mail_preview();
    }

    fn select_previous_thread(&mut self) {
        if self.thread_index == 0 {
            return;
        }

        self.select_filtered_thread_at(self.thread_index - 1);
    }

    fn select_next_thread(&mut self) {
        if self.thread_index + 1 >= self.filtered_thread_indices.len() {
            return;
        }

        self.select_filtered_thread_at(self.thread_index + 1);
    }

    fn move_up(&mut self) {
        match self.ui_page {
            UiPage::Mail => match self.focus {
                Pane::Subscriptions => {
                    self.move_subscription_up();
                }
                Pane::Threads => {
                    self.select_previous_thread();
                }
                Pane::Preview => {
                    self.preview_scroll = self
                        .preview_scroll
                        .min(self.preview_scroll_limit.get())
                        .saturating_sub(1);
                }
            },
            UiPage::CodeBrowser => match self.code_focus {
                CodePaneFocus::Tree => self.move_kernel_tree_up(),
                CodePaneFocus::Source => {
                    self.code_preview_scroll = self
                        .code_preview_scroll
                        .min(self.code_preview_scroll_limit.get())
                        .saturating_sub(1);
                }
            },
        }
    }

    fn move_down(&mut self) {
        match self.ui_page {
            UiPage::Mail => match self.focus {
                Pane::Subscriptions => {
                    self.move_subscription_down();
                }
                Pane::Threads => {
                    self.select_next_thread();
                }
                Pane::Preview => {
                    let preview_scroll_limit = self.preview_scroll_limit.get();
                    self.preview_scroll = self
                        .preview_scroll
                        .min(preview_scroll_limit)
                        .saturating_add(1)
                        .min(preview_scroll_limit);
                }
            },
            UiPage::CodeBrowser => match self.code_focus {
                CodePaneFocus::Tree => self.move_kernel_tree_down(),
                CodePaneFocus::Source => {
                    let code_preview_scroll_limit = self.code_preview_scroll_limit.get();
                    self.code_preview_scroll = self
                        .code_preview_scroll
                        .min(code_preview_scroll_limit)
                        .saturating_add(1)
                        .min(code_preview_scroll_limit);
                }
            },
        }
    }

    fn open_search(&mut self) {
        self.search.active = true;
        self.search.input = self.search.applied_query.clone();
        self.status = "search mode".to_string();
    }

    fn jump_current_pane_to_start(&mut self) {
        match self.ui_page {
            UiPage::Mail => match self.focus {
                Pane::Subscriptions => {
                    self.subscription_row_index = 0;
                    self.clamp_subscription_row_selection();
                }
                Pane::Threads => {
                    if !self.filtered_thread_indices.is_empty() {
                        self.select_filtered_thread_at(0);
                    }
                }
                Pane::Preview => {
                    self.preview_scroll = 0;
                }
            },
            UiPage::CodeBrowser => match self.code_focus {
                CodePaneFocus::Tree => {
                    let previous_file =
                        self.selected_kernel_tree_file_path().map(Path::to_path_buf);
                    self.kernel_tree_row_index = 0;
                    let next_file = self.selected_kernel_tree_file_path().map(Path::to_path_buf);
                    if previous_file != next_file {
                        self.code_preview_scroll = 0;
                    }
                }
                CodePaneFocus::Source => {
                    self.code_preview_scroll = 0;
                }
            },
        }
    }

    fn jump_current_pane_to_end(&mut self) {
        match self.ui_page {
            UiPage::Mail => match self.focus {
                Pane::Subscriptions => {
                    let rows = self.subscription_rows();
                    if rows.is_empty() {
                        return;
                    }
                    self.subscription_row_index = rows.len().saturating_sub(1);
                    self.clamp_subscription_row_selection();
                }
                Pane::Threads => {
                    if !self.filtered_thread_indices.is_empty() {
                        self.select_filtered_thread_at(
                            self.filtered_thread_indices.len().saturating_sub(1),
                        );
                    }
                }
                Pane::Preview => {
                    self.preview_scroll = u16::MAX;
                }
            },
            UiPage::CodeBrowser => match self.code_focus {
                CodePaneFocus::Tree => {
                    if self.kernel_tree_rows.is_empty() {
                        self.kernel_tree_row_index = 0;
                        return;
                    }
                    let previous_file =
                        self.selected_kernel_tree_file_path().map(Path::to_path_buf);
                    self.kernel_tree_row_index = self.kernel_tree_rows.len().saturating_sub(1);
                    let next_file = self.selected_kernel_tree_file_path().map(Path::to_path_buf);
                    if previous_file != next_file {
                        self.code_preview_scroll = 0;
                    }
                }
                CodePaneFocus::Source => {
                    self.code_preview_scroll = self.code_preview_scroll_limit.get();
                }
            },
        }
    }

    fn pending_main_page_count_state(&self, count: u16) -> PendingMainPageCountState {
        PendingMainPageCountState {
            count,
            ui_page: self.ui_page,
            focus: self.focus,
            code_focus: self.code_focus,
        }
    }

    fn clear_pending_main_page_inputs(&mut self) {
        self.pending_main_page_sequence = None;
        self.pending_main_page_count = None;
    }

    fn clear_pending_main_page_count(&mut self) {
        self.pending_main_page_count = None;
    }

    fn has_pending_main_page_count(&self) -> bool {
        self.pending_main_page_count.is_some_and(|state| {
            state.ui_page == self.ui_page
                && state.focus == self.focus
                && state.code_focus == self.code_focus
        })
    }

    fn push_pending_main_page_count_digit(&mut self, digit: u16) {
        let next_count = self
            .pending_main_page_count
            .filter(|state| {
                state.ui_page == self.ui_page
                    && state.focus == self.focus
                    && state.code_focus == self.code_focus
            })
            .map(|state| state.count.saturating_mul(10).saturating_add(digit))
            .unwrap_or(digit);
        self.pending_main_page_count = Some(self.pending_main_page_count_state(next_count));
    }

    fn take_pending_main_page_count(&mut self) -> Option<u16> {
        let pending_state = self.pending_main_page_count.take()?;
        let same_scope = pending_state.ui_page == self.ui_page
            && pending_state.focus == self.focus
            && pending_state.code_focus == self.code_focus;
        same_scope.then_some(pending_state.count)
    }

    fn close_search(&mut self) {
        self.search.active = false;
        self.search.input.clear();
        self.status = "search cancelled".to_string();
    }

    fn apply_search(&mut self) {
        self.search.active = false;
        self.search.applied_query = self.search.input.trim().to_string();
        self.thread_index = 0;
        self.apply_thread_filter();
    }

    fn list_saved_views(&mut self) {
        if self.saved_views.is_empty() {
            self.status = "saved views: <none>".to_string();
            return;
        }
        let entries = self
            .saved_views
            .iter()
            .map(|view| format!("{}={}", view.name, view.query))
            .collect::<Vec<String>>();
        self.status = format!("saved views: {}", entries.join("; "));
    }

    fn save_current_view(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty()
            || !name.chars().all(|character| {
                character.is_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            self.status =
                "usage: view save NAME (NAME may contain letters, digits, -, _, or .)".to_string();
            return;
        }
        let query = self.search.applied_query.trim();
        if query.is_empty() {
            self.status = "cannot save an empty search; apply a query first".to_string();
            return;
        }
        if let Err(error) = parse_query(query) {
            self.status = format!("cannot save invalid search: {error}");
            return;
        }

        if let Some(view) = self
            .saved_views
            .iter_mut()
            .find(|view| view.name.eq_ignore_ascii_case(name))
        {
            view.name = name.to_string();
            view.query = query.to_string();
        } else {
            self.saved_views.push(SavedView {
                name: name.to_string(),
                query: query.to_string(),
            });
        }
        self.saved_views.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        });
        self.persist_ui_state();
        self.status = format!("saved view '{name}' = {query}");
    }

    fn use_saved_view(&mut self, name: &str) {
        let Some(view) = self
            .saved_views
            .iter()
            .find(|view| view.name.eq_ignore_ascii_case(name))
            .cloned()
        else {
            self.status = format!("saved view '{name}' was not found");
            return;
        };
        if let Err(error) = parse_query(&view.query) {
            self.status = format!("saved view '{}' is invalid: {error}", view.name);
            return;
        }
        self.search.active = false;
        self.search.input = view.query.clone();
        self.search.applied_query = view.query.clone();
        self.thread_index = 0;
        self.apply_thread_filter();
        self.status = format!(
            "saved view '{}' applied: {} matches",
            view.name,
            self.filtered_thread_indices.len()
        );
    }

    fn delete_saved_view(&mut self, name: &str) {
        let before = self.saved_views.len();
        self.saved_views
            .retain(|view| !view.name.eq_ignore_ascii_case(name));
        if self.saved_views.len() == before {
            self.status = format!("saved view '{name}' was not found");
            return;
        }
        self.persist_ui_state();
        self.status = format!("saved view '{name}' deleted");
    }

    fn handle_palette_view(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let _ = parts.next();
        let Some(action) = parts.next() else {
            self.list_saved_views();
            return;
        };
        match action.to_ascii_lowercase().as_str() {
            "list" => {
                if parts.next().is_some() {
                    self.status = "usage: view list".to_string();
                } else {
                    self.list_saved_views();
                }
            }
            "save" => {
                let Some(name) = parts.next() else {
                    self.status = "usage: view save NAME".to_string();
                    return;
                };
                if parts.next().is_some() {
                    self.status = "usage: view save NAME".to_string();
                } else {
                    self.save_current_view(name);
                }
            }
            "use" | "open" => {
                let Some(name) = parts.next() else {
                    self.status = "usage: view use NAME".to_string();
                    return;
                };
                if parts.next().is_some() {
                    self.status = "usage: view use NAME".to_string();
                } else {
                    self.use_saved_view(name);
                }
            }
            "delete" | "remove" | "rm" => {
                let Some(name) = parts.next() else {
                    self.status = "usage: view delete NAME".to_string();
                    return;
                };
                if parts.next().is_some() {
                    self.status = "usage: view delete NAME".to_string();
                } else {
                    self.delete_saved_view(name);
                }
            }
            _ => {
                self.status = "usage: view [list|save NAME|use NAME|delete NAME]".to_string();
            }
        }
    }

    fn toggle_palette(&mut self) {
        self.palette.open = !self.palette.open;
        if self.palette.open {
            self.palette.clear_completion();
            self.palette.clear_local_result();
            self.status = "command palette opened".to_string();
        } else {
            self.palette.input.clear();
            self.palette.clear_completion();
            self.palette.clear_local_result();
            self.status = "command palette closed".to_string();
        }
    }

    fn close_palette(&mut self) {
        self.palette.open = false;
        self.palette.input.clear();
        self.palette.clear_completion();
        self.palette.clear_local_result();
        self.status = "command palette closed".to_string();
    }

    fn dismiss_palette(&mut self) {
        self.palette.open = false;
        self.palette.input.clear();
        self.palette.clear_completion();
        self.palette.clear_local_result();
    }

    fn is_code_edit_active(&self) -> bool {
        self.code_edit_mode.is_active()
    }

    fn mark_terminal_refresh_needed(&mut self) {
        self.needs_terminal_refresh = true;
    }

    fn take_terminal_refresh_needed(&mut self) -> bool {
        std::mem::take(&mut self.needs_terminal_refresh)
    }

    fn enter_code_edit_mode(&mut self) {
        if !matches!(self.ui_page, UiPage::CodeBrowser)
            || !matches!(self.code_focus, CodePaneFocus::Source)
        {
            self.status = CODE_EDIT_ENTRY_HINT.to_string();
            return;
        }

        let Some(path) = self.selected_kernel_tree_file_path().map(Path::to_path_buf) else {
            self.status = CODE_EDIT_ENTRY_HINT.to_string();
            return;
        };

        let buffer = match load_code_edit_buffer_from_path(&path) {
            Ok(buffer) => buffer,
            Err(error) => {
                self.status = error;
                return;
            }
        };

        self.code_edit_mode = CodeEditMode::VimNormal;
        self.code_edit_target = Some(path.clone());
        self.code_edit_buffer = buffer;
        self.code_edit_cursor_row = 0;
        self.code_edit_cursor_col = 0;
        self.code_edit_dirty = false;
        self.code_edit_command_input.clear();
        self.code_preview_scroll = 0;
        self.status = format!("editing {} (NORMAL)", path.display());
        tracing::info!(
            op = "code.edit",
            status = "started",
            file = %path.display()
        );
    }

    fn exit_code_edit_mode(&mut self, status: String) {
        let target_for_log = self
            .code_edit_target
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string());
        self.code_edit_mode = CodeEditMode::Browse;
        self.code_edit_target = None;
        self.code_edit_buffer.clear();
        self.code_edit_cursor_row = 0;
        self.code_edit_cursor_col = 0;
        self.code_edit_dirty = false;
        self.code_edit_command_input.clear();
        self.code_preview_scroll = 0;
        self.status = status;
        tracing::info!(
            op = "code.edit",
            status = "succeeded",
            file = %target_for_log,
            detail = %self.status
        );
    }

    fn save_code_edit_buffer(&mut self) -> bool {
        let Some(path) = self.code_edit_target.clone() else {
            self.status = "no file is being edited".to_string();
            return false;
        };

        let content = self.code_edit_buffer.join("\n");
        match fs::write(&path, content) {
            Ok(_) => {
                self.code_edit_dirty = false;
                self.status = format!("saved {}", path.display());
                tracing::info!(
                    op = "code.save",
                    status = "succeeded",
                    file = %path.display(),
                    lines = self.code_edit_buffer.len()
                );
                true
            }
            Err(error) => {
                self.status = format!("failed to save {}: {}", path.display(), error);
                tracing::error!(
                    op = "code.save",
                    status = "failed",
                    file = %path.display(),
                    error = %error
                );
                false
            }
        }
    }

    fn current_external_editor_target(&self) -> Option<PathBuf> {
        if self.is_code_edit_active() {
            self.code_edit_target
                .clone()
                .or_else(|| self.selected_kernel_tree_file_path().map(Path::to_path_buf))
        } else {
            self.selected_kernel_tree_file_path().map(Path::to_path_buf)
        }
    }

    fn reload_code_edit_target_from_disk(
        &mut self,
        path: &Path,
    ) -> std::result::Result<(), String> {
        let Some(target) = self.code_edit_target.as_ref() else {
            return Ok(());
        };
        if target != path {
            return Ok(());
        }

        let buffer = load_code_edit_buffer_from_path(path)?;
        self.code_edit_buffer = buffer;
        self.code_edit_dirty = false;
        self.clamp_code_edit_cursor();
        self.adjust_code_edit_scroll();
        Ok(())
    }

    fn open_external_editor(&mut self) {
        if !matches!(self.ui_page, UiPage::CodeBrowser)
            || !matches!(self.code_focus, CodePaneFocus::Source)
        {
            tracing::info!(
                op = "external_editor",
                status = "blocked",
                ui_page = ?self.ui_page,
                code_focus = ?self.code_focus,
                reason = "invalid_page_or_focus"
            );
            self.status = EXTERNAL_EDITOR_ENTRY_HINT.to_string();
            return;
        }

        if self.code_edit_dirty {
            tracing::info!(
                op = "external_editor",
                status = "blocked",
                reason = "inline_buffer_dirty"
            );
            self.status = "unsaved changes, run :w before external vim".to_string();
            return;
        }

        let Some(path) = self.current_external_editor_target() else {
            tracing::info!(
                op = "external_editor",
                status = "blocked",
                reason = "no_source_file_selected"
            );
            self.status = EXTERNAL_EDITOR_ENTRY_HINT.to_string();
            return;
        };

        let editor = resolve_external_editor();
        let from_inline_edit = self.is_code_edit_active();
        tracing::info!(
            op = "external_editor",
            status = "started",
            editor = %editor,
            file = %path.display(),
            from_inline_edit
        );
        let runner = self.external_editor_runner;
        match runner(&editor, &path) {
            Ok(result) => {
                self.mark_terminal_refresh_needed();
                let reload_status = self.reload_code_edit_target_from_disk(&path);
                self.code_preview_scroll = 0;
                self.status = match (result.success, result.exit_code) {
                    (true, Some(code)) => format!(
                        "external vim exited successfully (editor={} exit={} file={})",
                        editor,
                        code,
                        path.display()
                    ),
                    (true, None) => format!(
                        "external vim exited successfully (editor={} file={})",
                        editor,
                        path.display()
                    ),
                    (false, Some(code)) => format!(
                        "external vim exited with code {} (editor={} file={})",
                        code,
                        editor,
                        path.display()
                    ),
                    (false, None) => format!(
                        "external vim terminated by signal (editor={} file={})",
                        editor,
                        path.display()
                    ),
                };

                if let Err(error) = reload_status {
                    self.status = format!(
                        "{}; failed to reload {}: {}",
                        self.status,
                        path.display(),
                        error
                    );
                }

                tracing::info!(
                    op = "external_editor",
                    status = if result.success { "succeeded" } else { "failed" },
                    editor = %editor,
                    file = %path.display(),
                    success = result.success,
                    exit_code = ?result.exit_code
                );
            }
            Err(error) => {
                self.mark_terminal_refresh_needed();
                self.status = format!("external vim failed for {}: {}", path.display(), error);
                tracing::error!(
                    op = "external_editor",
                    status = "failed",
                    editor = %editor,
                    file = %path.display(),
                    error = %error
                );
            }
        }
    }

    fn code_edit_line_len(&self, row: usize) -> usize {
        self.code_edit_buffer
            .get(row)
            .map(|line| line.chars().count())
            .unwrap_or(0)
    }

    fn clamp_code_edit_cursor(&mut self) {
        if self.code_edit_buffer.is_empty() {
            self.code_edit_buffer.push(String::new());
        }
        if self.code_edit_cursor_row >= self.code_edit_buffer.len() {
            self.code_edit_cursor_row = self.code_edit_buffer.len().saturating_sub(1);
        }
        let line_len = self.code_edit_line_len(self.code_edit_cursor_row);
        if self.code_edit_cursor_col > line_len {
            self.code_edit_cursor_col = line_len;
        }
    }

    fn adjust_code_edit_scroll(&mut self) {
        const EDIT_HEADER_LINES: usize = 4;
        let logical_cursor_line = self.code_edit_cursor_row.saturating_add(EDIT_HEADER_LINES);
        let scroll_target = logical_cursor_line.saturating_sub(3);
        self.code_preview_scroll = scroll_target.min(u16::MAX as usize) as u16;
    }

    fn move_code_edit_cursor_left(&mut self) {
        self.clamp_code_edit_cursor();
        if self.code_edit_cursor_col > 0 {
            self.code_edit_cursor_col -= 1;
        } else if self.code_edit_cursor_row > 0 {
            self.code_edit_cursor_row -= 1;
            self.code_edit_cursor_col = self.code_edit_line_len(self.code_edit_cursor_row);
        }
        self.adjust_code_edit_scroll();
    }

    fn move_code_edit_cursor_right(&mut self) {
        self.clamp_code_edit_cursor();
        let line_len = self.code_edit_line_len(self.code_edit_cursor_row);
        if self.code_edit_cursor_col < line_len {
            self.code_edit_cursor_col += 1;
        } else if self.code_edit_cursor_row + 1 < self.code_edit_buffer.len() {
            self.code_edit_cursor_row += 1;
            self.code_edit_cursor_col = 0;
        }
        self.adjust_code_edit_scroll();
    }

    fn move_code_edit_cursor_up(&mut self) {
        self.clamp_code_edit_cursor();
        if self.code_edit_cursor_row > 0 {
            self.code_edit_cursor_row -= 1;
            let line_len = self.code_edit_line_len(self.code_edit_cursor_row);
            self.code_edit_cursor_col = self.code_edit_cursor_col.min(line_len);
        }
        self.adjust_code_edit_scroll();
    }

    fn move_code_edit_cursor_down(&mut self) {
        self.clamp_code_edit_cursor();
        if self.code_edit_cursor_row + 1 < self.code_edit_buffer.len() {
            self.code_edit_cursor_row += 1;
            let line_len = self.code_edit_line_len(self.code_edit_cursor_row);
            self.code_edit_cursor_col = self.code_edit_cursor_col.min(line_len);
        }
        self.adjust_code_edit_scroll();
    }

    fn insert_code_edit_character(&mut self, character: char) -> bool {
        self.clamp_code_edit_cursor();
        let Some(line) = self.code_edit_buffer.get_mut(self.code_edit_cursor_row) else {
            return false;
        };
        let byte_index = char_to_byte_index(line, self.code_edit_cursor_col);
        line.insert(byte_index, character);
        self.code_edit_cursor_col += 1;
        self.code_edit_dirty = true;
        self.adjust_code_edit_scroll();
        true
    }

    fn backspace_code_edit_character(&mut self) -> bool {
        self.clamp_code_edit_cursor();

        if self.code_edit_cursor_col > 0 {
            let Some(line) = self.code_edit_buffer.get_mut(self.code_edit_cursor_row) else {
                return false;
            };
            let remove_at = self.code_edit_cursor_col - 1;
            let start = char_to_byte_index(line, remove_at);
            let end = char_to_byte_index(line, remove_at + 1);
            line.replace_range(start..end, "");
            self.code_edit_cursor_col -= 1;
            self.code_edit_dirty = true;
            self.adjust_code_edit_scroll();
            return true;
        }

        if self.code_edit_cursor_row == 0 {
            return false;
        }

        let current = self.code_edit_buffer.remove(self.code_edit_cursor_row);
        self.code_edit_cursor_row -= 1;
        let Some(previous_line) = self.code_edit_buffer.get_mut(self.code_edit_cursor_row) else {
            return false;
        };
        let previous_len = previous_line.chars().count();
        previous_line.push_str(&current);
        self.code_edit_cursor_col = previous_len;
        self.code_edit_dirty = true;
        self.adjust_code_edit_scroll();
        true
    }

    fn insert_code_edit_newline(&mut self) -> bool {
        self.clamp_code_edit_cursor();
        let Some(line) = self.code_edit_buffer.get_mut(self.code_edit_cursor_row) else {
            return false;
        };
        let byte_index = char_to_byte_index(line, self.code_edit_cursor_col);
        let tail = line.split_off(byte_index);
        self.code_edit_buffer
            .insert(self.code_edit_cursor_row + 1, tail);
        self.code_edit_cursor_row += 1;
        self.code_edit_cursor_col = 0;
        self.code_edit_dirty = true;
        self.adjust_code_edit_scroll();
        true
    }

    fn delete_code_edit_character(&mut self) -> bool {
        self.clamp_code_edit_cursor();
        let row = self.code_edit_cursor_row;
        if row >= self.code_edit_buffer.len() {
            return false;
        }

        let line_len = self.code_edit_line_len(row);
        if self.code_edit_cursor_col < line_len {
            let Some(line) = self.code_edit_buffer.get_mut(row) else {
                return false;
            };
            let start = char_to_byte_index(line, self.code_edit_cursor_col);
            let end = char_to_byte_index(line, self.code_edit_cursor_col + 1);
            line.replace_range(start..end, "");
            self.clamp_code_edit_cursor();
            self.code_edit_dirty = true;
            self.adjust_code_edit_scroll();
            return true;
        }

        if self.code_edit_cursor_col == line_len && row + 1 < self.code_edit_buffer.len() {
            let next = self.code_edit_buffer.remove(row + 1);
            let Some(line) = self.code_edit_buffer.get_mut(row) else {
                return false;
            };
            line.push_str(&next);
            self.code_edit_dirty = true;
            self.adjust_code_edit_scroll();
            return true;
        }

        false
    }

    fn enter_code_edit_command_mode(&mut self) {
        self.code_edit_mode = CodeEditMode::VimCommand;
        self.code_edit_command_input.clear();
        self.status = "command mode".to_string();
    }

    fn execute_code_edit_command(&mut self) {
        let command = self.code_edit_command_input.trim().to_string();
        self.code_edit_command_input.clear();
        self.code_edit_mode = CodeEditMode::VimNormal;

        if command.is_empty() {
            self.status = "empty command".to_string();
            return;
        }

        let target_for_log = self
            .code_edit_target
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string());
        tracing::info!(
            op = "code.command",
            status = "started",
            command = %command,
            file = %target_for_log
        );
        match command.as_str() {
            "w" => {
                let _ = self.save_code_edit_buffer();
            }
            "q!" => {
                let target = self
                    .code_edit_target
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<file>".to_string());
                self.exit_code_edit_mode(format!("discarded unsaved changes for {target}"));
            }
            "q" => {
                if self.code_edit_dirty {
                    self.status = "unsaved changes, run :w, :wq, or :q!".to_string();
                } else {
                    let target = self
                        .code_edit_target
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<file>".to_string());
                    self.exit_code_edit_mode(format!("exit edit mode for {target}"));
                }
            }
            "wq" => {
                if self.save_code_edit_buffer() {
                    let target = self
                        .code_edit_target
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<file>".to_string());
                    self.exit_code_edit_mode(format!("saved and exited {target}"));
                }
            }
            "vim" => {
                self.open_external_editor();
            }
            _ => {
                self.status = format!("unsupported command: :{command}");
            }
        }
    }
}

fn spawn_startup_sync_worker(
    runtime: RuntimeConfig,
    mailboxes: Vec<String>,
) -> Receiver<StartupSyncEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let total = mailboxes.len();
        for (index, mailbox) in mailboxes.into_iter().enumerate() {
            // Emit progress before the work starts so the UI can show which
            // mailbox is currently blocking startup, even if the sync hangs.
            if sender
                .send(StartupSyncEvent::MailboxStarted {
                    mailbox: mailbox.clone(),
                    index: index + 1,
                    total,
                })
                .is_err()
            {
                return;
            }

            let request = sync_worker::SyncRequest {
                mailbox: mailbox.clone(),
                fixture_dir: None,
                uidvalidity: None,
                reconnect_attempts: PALETTE_SYNC_RECONNECT_ATTEMPTS,
            };
            match run_sync_request_guarded(&runtime, request) {
                Ok(summary) => {
                    if sender
                        .send(StartupSyncEvent::MailboxFinished {
                            mailbox,
                            fetched: summary.fetched,
                            inserted: summary.inserted,
                            updated: summary.updated,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    if sender
                        .send(StartupSyncEvent::MailboxFailed {
                            mailbox,
                            error: error.to_string(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }

        let _ = sender.send(StartupSyncEvent::WorkerCompleted);
    });

    receiver
}

fn spawn_thread_fetch_worker(
    runtime: RuntimeConfig,
    mailbox: String,
    message_id: String,
) -> Receiver<ThreadFetchEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let request = sync_worker::ThreadFetchRequest {
            mailbox: mailbox.clone(),
            message_id: message_id.clone(),
            reconnect_attempts: PALETTE_SYNC_RECONNECT_ATTEMPTS,
        };
        match sync_worker::fetch_thread(&runtime, request) {
            Ok(summary) => {
                let _ = sender.send(ThreadFetchEvent::Finished {
                    mailbox,
                    message_id,
                    fetched: summary.fetched,
                    inserted: summary.inserted,
                    updated: summary.updated,
                });
            }
            Err(error) => {
                let _ = sender.send(ThreadFetchEvent::Failed {
                    mailbox,
                    message_id,
                    error: error.to_string(),
                });
            }
        }
    });

    receiver
}

fn dedup_mailboxes(mailboxes: Vec<String>) -> Vec<String> {
    let mut deduped: Vec<String> = Vec::new();
    for mailbox in mailboxes {
        if deduped
            .iter()
            .any(|existing| same_mailbox_name(existing, &mailbox))
        {
            continue;
        }
        deduped.push(mailbox);
    }

    deduped
}

fn run_sync_request_guarded(
    runtime: &RuntimeConfig,
    request: sync_worker::SyncRequest,
) -> Result<sync_worker::SyncSummary> {
    let mailbox = request.mailbox.clone();
    catch_sync_panic(&mailbox, || sync_worker::run(runtime, request))
}

fn catch_sync_panic<T, F>(mailbox: &str, operation: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    // Background sync must not tear down the entire TUI process. Convert panics
    // into structured errors so the user can keep interacting and inspect the
    // failing mailbox name from the status line or logs.
    match panic::catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(payload) => {
            let message = if let Some(message) = payload.downcast_ref::<String>() {
                message.clone()
            } else if let Some(message) = payload.downcast_ref::<&str>() {
                (*message).to_string()
            } else {
                "unknown panic payload".to_string()
            };

            Err(CriewError::new(
                ErrorCode::Tui,
                format!("sync panicked for {mailbox}: {message}"),
            ))
        }
    }
}

fn load_code_edit_buffer_from_path(path: &Path) -> std::result::Result<Vec<String>, String> {
    let content =
        fs::read(path).map_err(|error| format!("failed to read {}: {}", path.display(), error))?;
    let text = String::from_utf8_lossy(&content)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut buffer: Vec<String> = text.split('\n').map(ToOwned::to_owned).collect();
    if buffer.is_empty() {
        buffer.push(String::new());
    }
    Ok(buffer)
}

fn pick_external_editor(visual: Option<&str>, editor: Option<&str>) -> String {
    visual
        .and_then(normalized_external_editor_value)
        .or_else(|| editor.and_then(normalized_external_editor_value))
        .unwrap_or_else(|| "vim".to_string())
}

fn normalized_external_editor_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn resolve_external_editor() -> String {
    let visual = env::var("VISUAL").ok();
    let editor = env::var("EDITOR").ok();
    pick_external_editor(visual.as_deref(), editor.as_deref())
}

fn run_external_editor_session(
    editor_spec: &str,
    file_path: &Path,
) -> std::result::Result<ExternalEditorProcessResult, String> {
    run_external_editor_session_with(
        editor_spec,
        file_path,
        || disable_raw_mode().map_err(|error| error.to_string()),
        || {
            let mut stdout = io::stdout();
            execute!(stdout, LeaveAlternateScreen).map_err(|error| error.to_string())
        },
        launch_external_editor_process,
        || {
            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen).map_err(|error| error.to_string())
        },
        || enable_raw_mode().map_err(|error| error.to_string()),
    )
}

fn run_external_editor_session_with<DisableRaw, LeaveAlt, LaunchEditor, EnterAlt, EnableRaw>(
    editor_spec: &str,
    file_path: &Path,
    mut disable_raw: DisableRaw,
    mut leave_alternate_screen: LeaveAlt,
    mut launch_editor: LaunchEditor,
    mut enter_alternate_screen: EnterAlt,
    mut enable_raw: EnableRaw,
) -> std::result::Result<ExternalEditorProcessResult, String>
where
    DisableRaw: FnMut() -> std::result::Result<(), String>,
    LeaveAlt: FnMut() -> std::result::Result<(), String>,
    LaunchEditor: FnMut(&str, &Path) -> std::result::Result<ExternalEditorProcessResult, String>,
    EnterAlt: FnMut() -> std::result::Result<(), String>,
    EnableRaw: FnMut() -> std::result::Result<(), String>,
{
    disable_raw().map_err(|error| format!("failed to disable raw mode: {error}"))?;

    if let Err(error) = leave_alternate_screen() {
        let _ = enable_raw();
        return Err(format!("failed to leave alternate screen: {error}"));
    }

    let launch_result = launch_editor(editor_spec, file_path);
    let enter_result = enter_alternate_screen();
    let enable_result = enable_raw();

    // Prefer returning a terminal-restore failure over silently leaving the
    // user in a broken terminal, even if the editor command itself succeeded.
    let mut restore_errors = Vec::new();
    if let Err(error) = enter_result {
        restore_errors.push(format!("failed to re-enter alternate screen: {error}"));
    }
    if let Err(error) = enable_result {
        restore_errors.push(format!("failed to re-enable raw mode: {error}"));
    }

    if !restore_errors.is_empty() {
        return match launch_result {
            Ok(result) => Err(format!(
                "terminal restore failed after external editor session (exit={:?}): {}",
                result.exit_code,
                restore_errors.join("; ")
            )),
            Err(error) => Err(format!("{error}; {}", restore_errors.join("; "))),
        };
    }

    launch_result
}

fn launch_external_editor_process(
    editor_spec: &str,
    file_path: &Path,
) -> std::result::Result<ExternalEditorProcessResult, String> {
    let (program, args) = split_external_editor_command(editor_spec)
        .ok_or_else(|| "external editor command is empty".to_string())?;
    let status = ProcessCommand::new(&program)
        .args(args)
        .arg(file_path)
        .status()
        .map_err(|error| format!("failed to launch {program}: {error}"))?;
    Ok(ExternalEditorProcessResult {
        success: status.success(),
        exit_code: status.code(),
    })
}

fn split_external_editor_command(editor_spec: &str) -> Option<(String, Vec<String>)> {
    let mut parts = editor_spec.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(ToOwned::to_owned).collect();
    Some((program, args))
}

fn resolve_git_reply_identity() -> std::result::Result<ReplyIdentity, String> {
    reply::resolve_git_identity()
}

fn send_reply_message(runtime: &RuntimeConfig, request: &SendRequest) -> SendOutcome {
    sendmail::send(runtime, request)
}

fn build_send_request(panel: &ReplyPanelState, prepared: PreparedReplyMessage) -> SendRequest {
    SendRequest {
        mail_id: panel.mail_id,
        thread_id: panel.thread_id,
        from: prepared.from,
        to: prepared.to,
        cc: prepared.cc,
        subject: prepared.subject,
        in_reply_to: prepared.in_reply_to,
        references: prepared.references,
        body: prepared.body,
        preview_confirmed_at: panel
            .preview_confirmed_at
            .clone()
            .unwrap_or_else(now_timestamp),
    }
}

fn persist_reply_send_result(
    runtime: &RuntimeConfig,
    request: &SendRequest,
    outcome: &SendOutcome,
) -> Result<i64> {
    let record_id = reply_store::insert_reply_send(
        &runtime.database_path,
        &ReplySendRecordRequest {
            thread_id: request.thread_id,
            mail_id: request.mail_id,
            transport: outcome.transport.clone(),
            message_id: outcome.message_id.clone(),
            from_addr: request.from.clone(),
            to_addrs: request.to.join(", "),
            cc_addrs: request.cc.join(", "),
            subject: request.subject.clone(),
            preview_confirmed_at: request.preview_confirmed_at.clone(),
            status: match outcome.status {
                SendStatus::Sent => ReplySendStatus::Sent,
                SendStatus::Failed => ReplySendStatus::Failed,
                SendStatus::TimedOut => ReplySendStatus::TimedOut,
            },
            command: outcome.command_line.clone(),
            draft_path: outcome.draft_path.clone(),
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            error_summary: outcome.error_summary.clone(),
            stdout: if outcome.stdout.is_empty() {
                None
            } else {
                Some(outcome.stdout.clone())
            },
            stderr: if outcome.stderr.is_empty() {
                None
            } else {
                Some(outcome.stderr.clone())
            },
            started_at: outcome.started_at.clone(),
            finished_at: outcome.finished_at.clone(),
        },
    )?;

    if matches!(outcome.status, SendStatus::Sent) && is_outgoing_patch_subject(&request.subject) {
        mail_store::record_following_sent_patch(
            &runtime.database_path,
            &mail_store::FollowingSentPatch {
                mail_id: request.mail_id,
                thread_id: request.thread_id,
                message_id: outcome.message_id.clone(),
                from_addr: request.from.clone(),
                to_addrs: request.to.clone(),
                cc_addrs: request.cc.clone(),
                subject: request.subject.clone(),
                occurred_at: outcome.finished_at.clone(),
            },
        )?;
    }

    Ok(record_id)
}

fn is_outgoing_patch_subject(subject: &str) -> bool {
    let normalized = subject.trim().to_ascii_lowercase();
    !normalized.starts_with("re:")
        && !normalized.starts_with("fwd:")
        && patch_worker::subject_is_patch_related(subject)
}

fn now_timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn standalone_draft_message_id() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("compose-{}-{}@criew.local", std::process::id(), nonce)
}

fn reply_body_line_logical_row(body_row: usize) -> usize {
    body_row
}

fn reply_command_line_logical_row(panel: &ReplyPanelState) -> usize {
    reply_body_line_logical_row(panel.body.len()) + 1
}

fn reply_editable_field_prefix(section: ReplySection) -> &'static str {
    match section {
        ReplySection::From => "[edit] From: ",
        ReplySection::To => "[edit] To: ",
        ReplySection::Cc => "[edit] Cc: ",
        ReplySection::Subject => "[edit] Subject: ",
        ReplySection::Body => "",
    }
}

fn reply_field_prefix_width(section: ReplySection) -> usize {
    1 + 1 + reply_editable_field_prefix(section).chars().count()
}

fn reply_body_prefix_width(body_row: usize) -> usize {
    let number_width = ((body_row + 1).to_string().chars().count()).max(4);
    number_width + 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiAction {
    Exit,
    Restart,
}

pub fn run(config: &RuntimeConfig, bootstrap: &BootstrapState) -> Result<TuiAction> {
    let ui_state_path = ui_state::path_for_data_dir(&config.data_dir);
    let persisted_ui_state = load_persisted_ui_state(&ui_state_path);
    let should_persist_imap_defaults = config.imap.is_complete()
        && !persisted_ui_state
            .as_ref()
            .map(|state| state.imap_defaults_initialized)
            .unwrap_or(false);
    // Resume the last active mailbox when possible, but always fall back to a
    // live runtime default so a stale persisted value cannot block startup.
    let initial_mailbox = persisted_ui_state
        .as_ref()
        .and_then(|state| state.active_mailbox.as_ref())
        .map(|mailbox| mailbox.trim())
        .filter(|mailbox| !mailbox.is_empty())
        .map(canonical_view_mailbox)
        .unwrap_or_else(|| config.default_active_mailbox().to_string());
    let threads = load_thread_rows_for_runtime(config, &initial_mailbox, 500)?;
    let mut terminal = setup_terminal()?;
    let guard = TerminalGuard;
    let mut state = if let Some(persisted) = persisted_ui_state {
        AppState::new_with_ui_state(threads, config.clone(), Some(persisted))
    } else {
        AppState::new(threads, config.clone())
    };
    if should_persist_imap_defaults {
        // Persist once after IMAP becomes available so future launches can tell
        // whether inbox defaults were already seeded for this user.
        state.persist_ui_state();
    }
    if state.filtered_thread_indices.is_empty()
        && !state.recover_from_empty_active_mailbox("active mailbox has no local data")
    {
        state.status = "no synced thread data, run `criew sync` first".to_string();
    }
    state.start_startup_sync_if_enabled();

    let result = tui_loop(&mut terminal, &mut state, config, bootstrap);

    drop(guard);
    result
}

fn load_persisted_ui_state(path: &std::path::Path) -> Option<UiState> {
    match ui_state::load(path) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "failed to load persisted ui state"
            );
            None
        }
    }
}

fn load_thread_rows_for_runtime(
    config: &RuntimeConfig,
    mailbox: &str,
    limit: usize,
) -> Result<Vec<ThreadRow>> {
    if is_following_view(mailbox) {
        let Some(self_email) = crate::infra::config::resolve_self_email(config).email else {
            return Ok(Vec::new());
        };
        return mail_store::load_following_thread_rows(&config.database_path, &self_email, limit);
    }

    mail_store::load_thread_rows_by_mailbox(&config.database_path, mailbox, limit)
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(|error| {
        CriewError::with_source(ErrorCode::Tui, "failed to enable raw mode", error)
    })?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|error| {
        CriewError::with_source(ErrorCode::Tui, "failed to enter alternate screen", error)
    })?;

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Tui,
            "failed to initialize terminal backend",
            error,
        )
    })?;

    Ok(terminal)
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    config: &RuntimeConfig,
    bootstrap: &BootstrapState,
) -> Result<TuiAction> {
    loop {
        // Pump worker events before drawing so each frame reflects the newest
        // background sync state and can request a full refresh when needed.
        state.pump_startup_sync_events();
        state.pump_manual_sync_events();
        state.pump_inbox_auto_sync_events();
        state.pump_subscription_auto_sync_events();
        state.pump_thread_fetch_events();
        state.maybe_start_inbox_auto_sync();
        state.maybe_start_subscription_auto_sync();

        if state.take_terminal_refresh_needed() {
            terminal.clear().map_err(|error| {
                CriewError::with_source(ErrorCode::Tui, "failed to clear terminal", error)
            })?;
            terminal.hide_cursor().map_err(|error| {
                CriewError::with_source(ErrorCode::Tui, "failed to hide terminal cursor", error)
            })?;
        }

        terminal
            .draw(|frame| draw(frame, state, config, bootstrap))
            .map_err(|error| {
                CriewError::with_source(ErrorCode::Tui, "failed to render frame", error)
            })?;

        if event::poll(Duration::from_millis(200)).map_err(|error| {
            CriewError::with_source(ErrorCode::Tui, "failed to poll terminal events", error)
        })? {
            let event = event::read().map_err(|error| {
                CriewError::with_source(ErrorCode::Tui, "failed to read terminal event", error)
            })?;

            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match handle_key_event(state, key) {
                    LoopAction::Continue => {}
                    LoopAction::Exit => return Ok(TuiAction::Exit),
                    LoopAction::Restart => return Ok(TuiAction::Restart),
                }
            }
        }
    }
}

fn default_kernel_tree_expanded_paths(root_paths: &[PathBuf]) -> HashSet<PathBuf> {
    root_paths
        .iter()
        .filter(|path| path.exists() && path.is_dir())
        .cloned()
        .collect()
}

fn build_kernel_tree_rows(
    root_paths: &[PathBuf],
    expanded_paths: &HashSet<PathBuf>,
) -> Vec<KernelTreeRow> {
    let mut rows = Vec::new();
    for root in root_paths {
        // Cap row generation defensively so an unexpectedly large tree cannot
        // turn one render pass into an unbounded filesystem walk.
        if rows.len() >= KERNEL_TREE_MAX_ROWS {
            break;
        }

        if !root.exists() {
            rows.push(KernelTreeRow {
                path: root.clone(),
                name: String::new(),
                depth: 0,
                kind: KernelTreeRowKind::MissingPath,
                expandable: false,
                expanded: false,
            });
            continue;
        }

        if !root.is_dir() {
            rows.push(KernelTreeRow {
                path: root.clone(),
                name: root
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| root.display().to_string()),
                depth: 0,
                kind: KernelTreeRowKind::RootFile,
                expandable: false,
                expanded: false,
            });
            continue;
        }

        let children = child_entries(root);
        let has_children = !children.is_empty();
        let is_expanded = expanded_paths.contains(root);
        rows.push(KernelTreeRow {
            path: root.clone(),
            name: root
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| root.display().to_string()),
            depth: 0,
            kind: KernelTreeRowKind::RootDirectory,
            expandable: has_children,
            expanded: has_children && is_expanded,
        });

        if has_children && is_expanded {
            append_kernel_tree_rows(children, 1, expanded_paths, &mut rows);
        }
    }
    rows
}

fn append_kernel_tree_rows(
    entries: KernelTreeDirectoryEntries,
    depth: usize,
    expanded_paths: &HashSet<PathBuf>,
    rows: &mut Vec<KernelTreeRow>,
) {
    if rows.len() >= KERNEL_TREE_MAX_ROWS {
        return;
    }

    for child in entries.into_children() {
        if rows.len() >= KERNEL_TREE_MAX_ROWS {
            break;
        }

        if child.is_dir() {
            let children = child_entries(&child);
            let has_children = !children.is_empty();
            let is_expanded = expanded_paths.contains(&child);
            let name = child
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| child.display().to_string());
            rows.push(KernelTreeRow {
                path: child.clone(),
                name,
                depth,
                kind: KernelTreeRowKind::Directory,
                expandable: has_children,
                expanded: has_children && is_expanded,
            });

            if has_children && is_expanded {
                append_kernel_tree_rows(children, depth + 1, expanded_paths, rows);
            }
            continue;
        }

        let name = child
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| child.display().to_string());
        rows.push(KernelTreeRow {
            path: child,
            name,
            depth,
            kind: KernelTreeRowKind::File,
            expandable: false,
            expanded: false,
        });
    }
}

fn child_entries(path: &Path) -> KernelTreeDirectoryEntries {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return KernelTreeDirectoryEntries::default(),
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            dirs.push(child);
        } else if child.is_file() {
            files.push(child);
        }
    }
    dirs.sort_by(|left, right| {
        left.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .cmp(
                &right
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string()),
            )
    });
    files.sort_by(|left, right| {
        left.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .cmp(
                &right
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string()),
            )
    });
    // Show directories before files so expanding the tree behaves like a
    // navigator, not like a flat alphabetical dump of mixed entries.
    KernelTreeDirectoryEntries { dirs, files }
}

fn default_subscriptions(
    runtime: &RuntimeConfig,
    enabled_mailboxes: &HashSet<String>,
    active_mailbox: Option<&str>,
    my_inbox_default: MyInboxDefault,
) -> Vec<SubscriptionItem> {
    let mut items: Vec<SubscriptionItem> = DEFAULT_SUBSCRIPTIONS
        .iter()
        .map(|entry| SubscriptionItem {
            mailbox: entry.mailbox.to_string(),
            label: entry.mailbox.to_string(),
            enabled: mailbox_set_contains(enabled_mailboxes, entry.mailbox),
            category: Some(entry.category),
        })
        .collect();

    if runtime.imap.is_complete() {
        // Following is special: it is backed by the private IMAP INBOX but is
        // discovered through recipient searches instead of exposing raw INBOX.
        let enable_following = mailbox_set_contains(enabled_mailboxes, FOLLOWING_VIEW)
            || my_inbox_default.should_enable_when_missing();
        items.insert(
            0,
            SubscriptionItem {
                mailbox: IMAP_INBOX_MAILBOX.to_string(),
                label: FOLLOWING_LABEL.to_string(),
                enabled: enable_following,
                category: None,
            },
        );
    }

    if !subscription_items_contain_mailbox(&items, &runtime.source_mailbox) {
        // Always keep the configured source mailbox visible even if the static
        // catalog changes, because it is the user's declared sync target.
        items.insert(
            0,
            SubscriptionItem {
                mailbox: runtime.source_mailbox.clone(),
                label: runtime.source_mailbox.clone(),
                enabled: mailbox_set_contains(enabled_mailboxes, runtime.source_mailbox.as_str()),
                category: category_for_mailbox(&runtime.source_mailbox),
            },
        );
    }

    for mailbox in enabled_mailboxes {
        if subscription_items_contain_mailbox(&items, mailbox) {
            continue;
        }
        items.push(SubscriptionItem {
            mailbox: mailbox.clone(),
            label: if is_following_view(mailbox) {
                FOLLOWING_LABEL.to_string()
            } else {
                mailbox.clone()
            },
            enabled: true,
            category: category_for_mailbox(mailbox),
        });
    }

    if let Some(mailbox) = active_mailbox
        && !mailbox.is_empty()
        && !subscription_items_contain_mailbox(&items, mailbox)
    {
        // Preserve access to the last active mailbox even if it no longer
        // appears in defaults, otherwise persisted UI state could point at an
        // invisible selection.
        items.push(SubscriptionItem {
            mailbox: mailbox.to_string(),
            label: if is_following_view(mailbox) {
                FOLLOWING_LABEL.to_string()
            } else {
                mailbox.to_string()
            },
            enabled: mailbox_set_contains(enabled_mailboxes, mailbox),
            category: category_for_mailbox(mailbox),
        });
    }

    items.sort_by(compare_subscription_items);

    items
}

fn subscription_category_rank(category: Option<SubscriptionCategory>) -> u8 {
    category.map_or(0, SubscriptionCategory::sort_rank)
}

#[derive(Debug, Clone, Copy)]
enum MyInboxDefault {
    EnableOnFirstOpen,
    PreservePersistedChoice,
}

impl MyInboxDefault {
    fn should_enable_when_missing(self) -> bool {
        matches!(self, Self::EnableOnFirstOpen)
    }
}

fn same_mailbox_name(left: &str, right: &str) -> bool {
    if is_following_view(left) && is_following_view(right) {
        return true;
    }
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn is_following_view(mailbox: &str) -> bool {
    mailbox.eq_ignore_ascii_case(FOLLOWING_VIEW) || mailbox.trim() == IMAP_INBOX_MAILBOX
}

fn canonical_view_mailbox(mailbox: &str) -> String {
    if is_following_view(mailbox) {
        IMAP_INBOX_MAILBOX.to_string()
    } else {
        mailbox.trim().to_string()
    }
}

fn display_mailbox_name(mailbox: &str) -> &str {
    if is_following_view(mailbox) {
        FOLLOWING_VIEW
    } else {
        mailbox
    }
}

fn mailbox_set_contains(mailboxes: &HashSet<String>, candidate: &str) -> bool {
    mailboxes
        .iter()
        .any(|mailbox| same_mailbox_name(mailbox, candidate))
}

fn subscription_items_contain_mailbox(items: &[SubscriptionItem], candidate: &str) -> bool {
    items
        .iter()
        .any(|item| same_mailbox_name(&item.mailbox, candidate))
}

fn compare_subscription_items(
    left: &SubscriptionItem,
    right: &SubscriptionItem,
) -> std::cmp::Ordering {
    right
        .enabled
        .cmp(&left.enabled)
        .then_with(|| {
            subscription_category_rank(left.category)
                .cmp(&subscription_category_rank(right.category))
        })
        .then_with(|| left.label.cmp(&right.label))
        .then_with(|| left.mailbox.cmp(&right.mailbox))
}

fn char_to_byte_index(value: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }

    value
        .char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(value.len())
}

fn matching_commands(input: &str) -> Vec<&'static PaletteCommand> {
    let query = input.trim().to_ascii_lowercase();
    if query.starts_with('!') {
        return Vec::new();
    }
    let mut matched: Vec<(u8, &PaletteCommand)> = Vec::new();

    for command in PALETTE_COMMANDS {
        if query.is_empty() || command.name.starts_with(&query) {
            matched.push((0, command));
            continue;
        }

        let description = command.description.to_ascii_lowercase();
        if command.name.contains(&query) || description.contains(&query) {
            matched.push((1, command));
        }
    }

    matched.sort_by_key(|(score, command)| (*score, command.name));
    matched.into_iter().map(|(_, command)| command).collect()
}

fn panel_block(panel: Pane, focus: Pane) -> Block<'static> {
    let is_focused = panel == focus;
    panel_block_with_title(panel.title(), is_focused)
}

fn panel_block_with_title(title: &str, is_focused: bool) -> Block<'static> {
    let decorated_title = if is_focused {
        format!("{title} *")
    } else {
        title.to_string()
    };

    let border_style = if is_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    Block::default()
        .title(decorated_title)
        .borders(Borders::ALL)
        .border_style(border_style)
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}
