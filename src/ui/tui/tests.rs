//! Focused TUI regression tests.
//!
//! These tests exercise the visible behavior of the interactive layer without
//! depending on a real terminal or network so refactors can change internals
//! while keeping key bindings, rendering, and workflow guarantees stable.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier};

use crate::app::cli::ReviewInboxMode;
use crate::app::patch::SeriesIntegrity;
use crate::app::review::ReviewInboxEntry;
use crate::domain::subscriptions::SubscriptionCategory;
use crate::infra::bootstrap::BootstrapState;
use crate::infra::config::{IMAP_INBOX_MAILBOX, RuntimeConfig, UiKeymap};
use crate::infra::db;
use crate::infra::db::DatabaseState;
use crate::infra::mail_parser;
use crate::infra::mail_store::{self, IncomingMail, SyncBatch, ThreadRow};
use crate::infra::reply_store::{self, ReplyDraftRequest, ReplyDraftStatus, ReplySendStatus};
use crate::infra::sendmail::{SendOutcome, SendRequest, SendStatus};
use crate::infra::ui_state::{self, SavedView, UiState};

use super::palette::run_palette_sync;
use super::preview::preview_warning_message;
use super::reply::ReplyIdentity;
use super::{
    AppState, CodeEditMode, CodePaneFocus, ExternalEditorProcessResult, LoopAction,
    MIN_MAIL_PREVIEW_WIDTH, MIN_MAIL_SUBSCRIPTIONS_WIDTH, MY_INBOX_LABEL, MailPaneLayout,
    ManualSyncOrigin, ManualSyncRequestOutcome, ManualSyncState, PALETTE_COMMANDS, Pane,
    ReplyEditMode, ReplySection, StartupSyncEvent, StartupSyncMailboxStatus, StartupSyncState,
    SubscriptionItem, UiPage, catch_sync_panic, code_edit_cursor_position, draw,
    extract_mail_body_preview, extract_mail_preview, handle_key_event, is_palette_open_shortcut,
    is_palette_toggle, load_source_file_preview, mail_page_panes, matching_commands,
    pick_external_editor, resolve_palette_local_workdir, run_external_editor_session_with,
    sanitize_inline_ui_text, subscription_line, thread_line,
};

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("criew-ui-{label}-{nonce}"));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn sample_thread(subject: &str, message_id: &str, depth: u16) -> ThreadRow {
    ThreadRow {
        thread_id: 1,
        mail_id: 1,
        depth,
        mailbox: "io-uring".to_string(),
        subject: subject.to_string(),
        from_addr: "alice@example.com".to_string(),
        message_id: message_id.to_string(),
        in_reply_to: None,
        date: None,
        raw_path: None,
    }
}

fn sample_thread_with_raw(
    subject: &str,
    message_id: &str,
    depth: u16,
    raw_path: PathBuf,
) -> ThreadRow {
    ThreadRow {
        thread_id: 1,
        mail_id: 1,
        depth,
        mailbox: "io-uring".to_string(),
        subject: subject.to_string(),
        from_addr: "alice@example.com".to_string(),
        message_id: message_id.to_string(),
        in_reply_to: None,
        date: None,
        raw_path: Some(raw_path),
    }
}

fn sample_threads(count: usize) -> Vec<ThreadRow> {
    (0..count)
        .map(|index| {
            let subject = format!("t{index}");
            let message_id = format!("{index}@example.com");
            sample_thread(&subject, &message_id, index as u16)
        })
        .collect()
}

fn sample_thread_in_thread(
    thread_id: i64,
    mail_id: i64,
    subject: &str,
    message_id: &str,
    depth: u16,
) -> ThreadRow {
    ThreadRow {
        thread_id,
        mail_id,
        depth,
        mailbox: "io-uring".to_string(),
        subject: subject.to_string(),
        from_addr: "alice@example.com".to_string(),
        message_id: message_id.to_string(),
        in_reply_to: None,
        date: None,
        raw_path: None,
    }
}

fn rendered_row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    let start = row as usize * width;
    let end = start + width;
    buffer.content()[start..end]
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}

fn rendered_cell_style_for_substring(
    terminal: &Terminal<TestBackend>,
    needle: &str,
) -> Option<(Color, Color, Modifier)> {
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;

    for row in 0..buffer.area().height {
        let row_text = rendered_row_text(terminal, row);
        if let Some(column) = row_text.find(needle) {
            let cell = &buffer.content()[row as usize * width + column];
            return Some((cell.fg, cell.bg, cell.modifier));
        }
    }

    None
}

fn test_runtime_in(root: PathBuf) -> RuntimeConfig {
    RuntimeConfig {
        config_path: root.join("config.toml"),
        data_dir: root.join("data"),
        database_path: root.join("data/criew.db"),
        raw_mail_dir: root.join("data/raw"),
        patch_dir: root.join("data/patches"),
        log_dir: root.join("data/logs"),
        b4_path: None,
        log_filter: "info".to_string(),
        source_mailbox: "inbox".to_string(),
        imap: crate::infra::config::ImapConfig::default(),
        lore_base_url: "https://lore.kernel.org".to_string(),
        startup_sync: true,
        ui_keymap: UiKeymap::Default,
        ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
        ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
        inbox_auto_sync_interval_secs: crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
        kernel_trees: Vec::new(),
    }
}

fn subscription_category_rank(category: Option<SubscriptionCategory>) -> u8 {
    category.map_or(0, SubscriptionCategory::sort_rank)
}

fn subscription_sort_key(item: &SubscriptionItem) -> (u8, &str, &str) {
    (
        subscription_category_rank(item.category),
        item.label.as_str(),
        item.mailbox.as_str(),
    )
}

fn test_runtime() -> RuntimeConfig {
    test_runtime_in(PathBuf::from("/tmp/criew-ui-test"))
}

fn test_runtime_with_kernel_tree(tree: PathBuf) -> RuntimeConfig {
    let mut runtime = test_runtime();
    runtime.kernel_trees = vec![tree];
    runtime
}

fn test_runtime_with_imap_in(root: PathBuf) -> RuntimeConfig {
    let mut runtime = test_runtime_in(root);
    runtime.imap = crate::infra::config::ImapConfig {
        email: Some("me@example.com".to_string()),
        user: Some("imap-user".to_string()),
        pass: Some("imap-pass".to_string()),
        server: Some("imap.example.com".to_string()),
        server_port: Some(993),
        encryption: Some(crate::infra::config::ImapEncryption::Tls),
        proxy: None,
    };
    runtime
}

fn test_runtime_with_imap() -> RuntimeConfig {
    test_runtime_with_imap_in(PathBuf::from("/tmp/criew-ui-test"))
}

fn seed_mailbox_thread(db_path: &Path, mailbox: &str, uid: u32, message_id: &str, subject: &str) {
    fs::create_dir_all(db_path.parent().expect("db parent")).expect("create db parent");
    db::initialize(db_path).expect("initialize db");
    let batch = SyncBatch {
            mailbox: mailbox.to_string(),
            uidvalidity: 1,
            highest_uid: uid,
            highest_modseq: Some(uid as u64),
            mails: vec![IncomingMail {
                mailbox: mailbox.to_string(),
                uid,
                modseq: Some(uid as u64),
                flags: vec!["Seen".to_string()],
                raw_path: PathBuf::from(format!("/tmp/{mailbox}-{uid}.eml")),
                parsed: mail_parser::parse_headers(
                    format!(
                        "Message-ID: <{message_id}>\nSubject: {subject}\nFrom: Alice <alice@example.com>\nTo: me@example.com\n\nbody\n"
                    )
                    .as_bytes(),
                    format!("synthetic-{mailbox}-{uid}@local"),
                ),
            }],
        };

    mail_store::apply_sync_batch(db_path, batch).expect("apply mailbox sync batch");
}

fn startup_sync_state(mailboxes: &[(&str, StartupSyncMailboxStatus)]) -> StartupSyncState {
    let (_sender, receiver) = mpsc::channel();
    StartupSyncState {
        receiver,
        mailbox_order: mailboxes
            .iter()
            .map(|(mailbox, _)| (*mailbox).to_string())
            .collect(),
        mailboxes: mailboxes
            .iter()
            .map(|(mailbox, status)| ((*mailbox).to_string(), *status))
            .collect(),
        total: mailboxes.len(),
        completed: mailboxes
            .iter()
            .filter(|(_, status)| {
                matches!(
                    status,
                    StartupSyncMailboxStatus::Finished | StartupSyncMailboxStatus::Failed
                )
            })
            .count(),
        succeeded: mailboxes
            .iter()
            .filter(|(_, status)| matches!(status, StartupSyncMailboxStatus::Finished))
            .count(),
        failed: mailboxes
            .iter()
            .filter(|(_, status)| matches!(status, StartupSyncMailboxStatus::Failed))
            .count(),
    }
}

fn manual_sync_state(mailboxes: &[(&str, StartupSyncMailboxStatus)]) -> ManualSyncState {
    let (_sender, receiver) = mpsc::channel();
    ManualSyncState {
        receiver,
        mailbox_order: mailboxes
            .iter()
            .map(|(mailbox, _)| (*mailbox).to_string())
            .collect(),
        mailboxes: mailboxes
            .iter()
            .map(|(mailbox, status)| ((*mailbox).to_string(), *status))
            .collect(),
        total: mailboxes.len(),
        completed: mailboxes
            .iter()
            .filter(|(_, status)| {
                matches!(
                    status,
                    StartupSyncMailboxStatus::Finished | StartupSyncMailboxStatus::Failed
                )
            })
            .count(),
        succeeded: mailboxes
            .iter()
            .filter(|(_, status)| matches!(status, StartupSyncMailboxStatus::Finished))
            .count(),
        failed: mailboxes
            .iter()
            .filter(|(_, status)| matches!(status, StartupSyncMailboxStatus::Failed))
            .count(),
        total_fetched: 0,
        total_inserted: 0,
        total_updated: 0,
        first_error: None,
    }
}

fn test_bootstrap(runtime: &RuntimeConfig) -> BootstrapState {
    BootstrapState {
        db: DatabaseState {
            path: runtime.database_path.clone(),
            schema_version: 1,
            created: false,
            applied_migrations: vec![],
        },
    }
}

#[test]
fn startup_sync_is_not_started_when_disabled_in_config() {
    let mut runtime = test_runtime();
    runtime.startup_sync = false;
    let mut state = AppState::new(vec![], runtime);

    state.start_startup_sync_if_enabled();

    assert!(state.startup_sync.is_none());
}

#[test]
fn startup_sync_progress_summary_renders_counts_and_running_mailbox() {
    let sync_state = startup_sync_state(&[
        ("INBOX", StartupSyncMailboxStatus::InFlight),
        ("io-uring", StartupSyncMailboxStatus::Pending),
        ("kvm", StartupSyncMailboxStatus::Finished),
    ]);

    assert_eq!(
        sync_state.progress_summary(),
        "1/3 ok=1 fail=0 queued=1 running=INBOX"
    );
    assert_eq!(
        sync_state.mailbox_states_display(),
        "INBOX:syncing io-uring:queued kvm:done"
    );
}

#[test]
fn inbox_auto_sync_starts_when_due_for_enabled_my_inbox() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.mailbox_sync_spawner = mailbox_sync_spawner_stub;
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.maybe_start_inbox_auto_sync();

    assert!(
        state
            .inbox_auto_sync
            .as_ref()
            .and_then(|sync| sync.receiver.as_ref())
            .is_some()
    );
}

#[test]
fn inbox_auto_sync_waits_for_startup_sync_to_finish() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.mailbox_sync_spawner = mailbox_sync_spawner_stub;
    state.startup_sync = Some(startup_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::InFlight,
    )]));
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.maybe_start_inbox_auto_sync();

    assert!(
        state
            .inbox_auto_sync
            .as_ref()
            .and_then(|sync| sync.receiver.as_ref())
            .is_none()
    );
}

#[test]
fn inbox_auto_sync_waits_for_manual_sync_to_finish() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.mailbox_sync_spawner = mailbox_sync_spawner_stub;
    state.manual_sync = Some(manual_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::InFlight,
    )]));
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.maybe_start_inbox_auto_sync();

    assert!(
        state
            .inbox_auto_sync
            .as_ref()
            .and_then(|sync| sync.receiver.as_ref())
            .is_none()
    );
}

#[test]
fn subscription_auto_sync_starts_when_due_for_enabled_linux_subscription() {
    let mut state = AppState::new(vec![], test_runtime());
    state.mailbox_sync_spawner = mailbox_sync_spawner_stub;
    let io_uring_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "io-uring")
        .expect("io-uring subscription exists");
    state.subscriptions[io_uring_index].enabled = true;
    state.reconcile_subscription_auto_sync();
    state
        .subscription_auto_sync
        .as_mut()
        .expect("subscription auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.maybe_start_subscription_auto_sync();

    assert!(
        state
            .subscription_auto_sync
            .as_ref()
            .and_then(|sync| sync.receiver.as_ref())
            .is_some()
    );
}

#[test]
fn subscription_auto_sync_waits_for_startup_sync_to_finish() {
    let mut state = AppState::new(vec![], test_runtime());
    state.mailbox_sync_spawner = mailbox_sync_spawner_stub;
    let qemu_devel_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "qemu-devel")
        .expect("qemu-devel subscription exists");
    state.subscriptions[qemu_devel_index].enabled = true;
    state.reconcile_subscription_auto_sync();
    state.startup_sync = Some(startup_sync_state(&[(
        "qemu-devel",
        StartupSyncMailboxStatus::InFlight,
    )]));
    state
        .subscription_auto_sync
        .as_mut()
        .expect("subscription auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.maybe_start_subscription_auto_sync();

    assert!(
        state
            .subscription_auto_sync
            .as_ref()
            .and_then(|sync| sync.receiver.as_ref())
            .is_none()
    );
}

#[test]
fn subscription_auto_sync_waits_for_manual_sync_to_finish() {
    let mut state = AppState::new(vec![], test_runtime());
    state.mailbox_sync_spawner = mailbox_sync_spawner_stub;
    let qemu_devel_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "qemu-devel")
        .expect("qemu-devel subscription exists");
    state.subscriptions[qemu_devel_index].enabled = true;
    state.reconcile_subscription_auto_sync();
    state.manual_sync = Some(manual_sync_state(&[(
        "qemu-devel",
        StartupSyncMailboxStatus::InFlight,
    )]));
    state
        .subscription_auto_sync
        .as_mut()
        .expect("subscription auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.maybe_start_subscription_auto_sync();

    assert!(
        state
            .subscription_auto_sync
            .as_ref()
            .and_then(|sync| sync.receiver.as_ref())
            .is_none()
    );
}

fn external_editor_mock_success(
    _editor: &str,
    file_path: &Path,
) -> std::result::Result<ExternalEditorProcessResult, String> {
    fs::write(file_path, "externally edited\n")
        .map_err(|error| format!("failed to write fixture: {error}"))?;
    Ok(ExternalEditorProcessResult {
        success: true,
        exit_code: Some(0),
    })
}

fn external_editor_mock_failure(
    _editor: &str,
    _file_path: &Path,
) -> std::result::Result<ExternalEditorProcessResult, String> {
    Err("mock launch failure".to_string())
}

fn reply_identity_mock() -> std::result::Result<ReplyIdentity, String> {
    Ok(ReplyIdentity {
        display: "CRIEW Test <criew@example.com>".to_string(),
        email: "criew@example.com".to_string(),
    })
}

fn reply_send_mock_success(_runtime: &RuntimeConfig, _request: &SendRequest) -> SendOutcome {
    SendOutcome {
        transport: "git-send-email".to_string(),
        message_id: "sent@example.com".to_string(),
        command_line: Some("git send-email reply.eml".to_string()),
        draft_path: None,
        exit_code: Some(0),
        timed_out: false,
        stdout: "sent".to_string(),
        stderr: String::new(),
        error_summary: None,
        started_at: "2026-03-07T10:00:01Z".to_string(),
        finished_at: "2026-03-07T10:00:02Z".to_string(),
        status: SendStatus::Sent,
    }
}

fn reply_send_mock_failure(_runtime: &RuntimeConfig, _request: &SendRequest) -> SendOutcome {
    SendOutcome {
        transport: "git-send-email".to_string(),
        message_id: "failed@example.com".to_string(),
        command_line: Some("git send-email reply.eml".to_string()),
        draft_path: Some(PathBuf::from("/tmp/reply-failed.eml")),
        exit_code: Some(1),
        timed_out: false,
        stdout: String::new(),
        stderr: "smtp auth failed".to_string(),
        error_summary: Some("smtp auth failed".to_string()),
        started_at: "2026-03-07T10:00:01Z".to_string(),
        finished_at: "2026-03-07T10:00:02Z".to_string(),
        status: SendStatus::Failed,
    }
}

fn mailbox_sync_spawner_stub(
    _runtime: RuntimeConfig,
    _mailboxes: Vec<String>,
) -> mpsc::Receiver<StartupSyncEvent> {
    let (_sender, receiver) = mpsc::channel();
    receiver
}

fn manual_sync_spawner_idle(
    _runtime: RuntimeConfig,
    _mailboxes: Vec<String>,
) -> mpsc::Receiver<StartupSyncEvent> {
    let (_sender, receiver) = mpsc::channel();
    receiver
}

fn thread_fetch_spawner_idle(
    _runtime: RuntimeConfig,
    _mailbox: String,
    _message_id: String,
) -> mpsc::Receiver<super::ThreadFetchEvent> {
    let (_sender, receiver) = mpsc::channel();
    receiver
}

fn manual_sync_spawner_seed_success(
    runtime: RuntimeConfig,
    mailboxes: Vec<String>,
) -> mpsc::Receiver<StartupSyncEvent> {
    let (sender, receiver) = mpsc::channel();
    let total = mailboxes.len();

    for (index, mailbox) in mailboxes.into_iter().enumerate() {
        sender
            .send(StartupSyncEvent::MailboxStarted {
                mailbox: mailbox.clone(),
                index: index + 1,
                total,
            })
            .expect("send mailbox started");
        seed_mailbox_thread(
            &runtime.database_path,
            &mailbox,
            index as u32 + 1,
            &format!("{mailbox}-{index}@example.com"),
            &format!("{mailbox} thread"),
        );
        sender
            .send(StartupSyncEvent::MailboxFinished {
                mailbox,
                fetched: 1,
                inserted: 1,
                updated: 0,
            })
            .expect("send mailbox finished");
    }
    sender
        .send(StartupSyncEvent::WorkerCompleted)
        .expect("send worker completed");

    receiver
}

fn type_text(state: &mut AppState, text: &str) {
    for character in text.chars() {
        let _ = handle_key_event(
            state,
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
        );
    }
}

#[test]
fn empty_query_returns_all_palette_commands() {
    let all = matching_commands("");
    assert_eq!(all.len(), PALETTE_COMMANDS.len());
    assert_eq!(
        all.iter().map(|command| command.name).collect::<Vec<_>>(),
        vec![
            "compose",
            "config",
            "exit",
            "fetch-thread",
            "forward",
            "help",
            "keymap",
            "outbox",
            "preflight",
            "quit",
            "restart",
            "review-inbox",
            "sync",
            "view",
            "vim",
        ]
    );
}

#[test]
fn prefix_matches_rank_before_fuzzy_matches() {
    let commands = matching_commands("ex");
    assert_eq!(commands[0].name, "exit");
}

#[test]
fn bang_mode_is_not_matched_as_builtin_command() {
    let commands = matching_commands("!pwd");
    assert!(commands.is_empty());
}

#[test]
fn external_editor_selection_prefers_visual_then_editor_then_vim() {
    assert_eq!(
        pick_external_editor(Some("nvim"), Some("vim")),
        "nvim".to_string()
    );
    assert_eq!(
        pick_external_editor(Some("  "), Some("hx")),
        "hx".to_string()
    );
    assert_eq!(pick_external_editor(None, Some("nano")), "nano".to_string());
    assert_eq!(pick_external_editor(None, None), "vim".to_string());
}

#[test]
fn external_editor_session_restores_terminal_after_editor_exit() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let steps: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
    let result = run_external_editor_session_with(
        "vim",
        Path::new("/tmp/demo.rs"),
        {
            let steps = steps.clone();
            move || {
                steps.borrow_mut().push("disable_raw");
                Ok(())
            }
        },
        {
            let steps = steps.clone();
            move || {
                steps.borrow_mut().push("leave_alt");
                Ok(())
            }
        },
        {
            let steps = steps.clone();
            move |_, _| {
                steps.borrow_mut().push("launch");
                Ok(ExternalEditorProcessResult {
                    success: true,
                    exit_code: Some(0),
                })
            }
        },
        {
            let steps = steps.clone();
            move || {
                steps.borrow_mut().push("enter_alt");
                Ok(())
            }
        },
        {
            let steps = steps.clone();
            move || {
                steps.borrow_mut().push("enable_raw");
                Ok(())
            }
        },
    )
    .expect("external editor session should succeed");

    assert!(result.success);
    assert_eq!(
        *steps.borrow(),
        vec![
            "disable_raw",
            "leave_alt",
            "launch",
            "enter_alt",
            "enable_raw"
        ]
    );
}

#[test]
fn mail_page_layout_keeps_preview_at_fixed_85_columns() {
    let panes = mail_page_panes(Rect::new(0, 0, 180, 20), MailPaneLayout::default());

    assert_eq!(panes[2].width, 85);
    assert_eq!(panes[2].x, 95);
    assert_eq!(panes[0].width, 23);
    assert_eq!(panes[1].width, 72);
    assert_eq!(panes[0].width + panes[1].width + panes[2].width, 180);
}

#[test]
fn mail_page_layout_falls_back_to_available_width_when_terminal_is_narrow() {
    let panes = mail_page_panes(Rect::new(0, 0, 60, 20), MailPaneLayout::default());

    assert_eq!(panes[2].width, 60);
    assert_eq!(panes[0].width, 0);
    assert_eq!(panes[1].width, 0);
}

#[test]
fn mail_page_layout_uses_persisted_fixed_mail_pane_widths_when_enabled() {
    let panes = mail_page_panes(
        Rect::new(0, 0, 180, 20),
        MailPaneLayout {
            subscriptions_width: 31,
            preview_width: 84,
        },
    );

    assert_eq!(panes[0].width, 31);
    assert_eq!(panes[1].width, 65);
    assert_eq!(panes[2].width, 84);
}

#[test]
fn subscription_line_shows_marker_and_mailbox_name_only() {
    let enabled = SubscriptionItem {
        mailbox: "io-uring".to_string(),
        label: "io-uring".to_string(),
        enabled: true,
        category: Some(SubscriptionCategory::LinuxSubsystem),
    };
    let disabled = SubscriptionItem {
        mailbox: "linux-mm".to_string(),
        label: "linux-mm".to_string(),
        enabled: false,
        category: Some(SubscriptionCategory::LinuxSubsystem),
    };

    assert_eq!(subscription_line(&enabled, None), "[y] io-uring");
    assert_eq!(subscription_line(&disabled, None), "[n] linux-mm");
}

#[test]
fn subscription_line_shows_sync_suffix_when_progress_is_active() {
    let enabled = SubscriptionItem {
        mailbox: "INBOX".to_string(),
        label: "My Inbox".to_string(),
        enabled: true,
        category: None,
    };

    assert_eq!(
        subscription_line(&enabled, Some(StartupSyncMailboxStatus::Pending)),
        "[y] My Inbox [queued]"
    );
    assert_eq!(
        subscription_line(&enabled, Some(StartupSyncMailboxStatus::InFlight)),
        "[y] My Inbox [sync]"
    );
}

#[test]
fn thread_line_hides_sender() {
    let row = sample_thread("thread subject", "x@example.com", 0);
    let line = thread_line(&row, 120);

    assert_eq!(line, "thread subject");
    assert!(!line.contains("alice@example.com"));
}

#[test]
fn thread_line_truncates_by_max_chars_and_available_width() {
    let long_subject = "x".repeat(240);
    let row = sample_thread(&long_subject, "x@example.com", 0);

    let line_capped_at_120 = thread_line(&row, 200);
    assert_eq!(
        line_capped_at_120.chars().count(),
        super::THREAD_LINE_MAX_CHARS
    );
    assert!(line_capped_at_120.ends_with("..."));

    let line_capped_by_width = thread_line(&row, 30);
    assert_eq!(line_capped_by_width.chars().count(), 30);
    assert!(line_capped_by_width.ends_with("..."));
}

#[test]
fn command_palette_quit_exits_application() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "quit".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(action, LoopAction::Exit));
}

#[test]
fn command_palette_restart_requests_tui_restart() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "restart".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(action, LoopAction::Restart));
}

#[test]
fn command_palette_help_includes_keyboard_shortcuts() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "help".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(action, LoopAction::Continue));
    assert!(state.status.contains("keys:"));
    assert!(state.status.contains("j/l focus"));
    assert!(state.status.contains("i/k move"));
    assert!(state.status.contains("[ ] expand pane"));
    assert!(state.status.contains("{ } shrink pane"));
    assert!(state.status.contains("-/= preview switch"));
    assert!(state.status.contains("y/n enable"));
    assert!(state.status.contains("a apply"));
    assert!(state.status.contains("d download"));
    assert!(state.status.contains("P preflight"));
    assert!(state.status.contains("outbox"));
    assert!(state.status.contains("u undo apply"));
}

#[test]
fn command_palette_help_uses_vim_keymap_labels() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(vec![], runtime);
    state.palette.open = true;
    state.palette.input = "help".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(action, LoopAction::Continue));
    assert!(state.status.contains("h/l focus"));
    assert!(state.status.contains("j/k move"));
}

#[test]
fn keymap_palette_command_opens_keymap_editor() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "keymap".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(action, LoopAction::Continue));
    assert!(state.keymap_editor.open);
    assert!(!state.palette.open);
    assert_eq!(state.status, "keymap editor opened");
}

#[test]
fn config_palette_get_and_set_roundtrip() {
    let root = temp_dir("palette-config");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[source]
mailbox = "inbox"
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.palette.open = true;
    state.palette.input = "config get source.mailbox".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.status.contains("source.mailbox"));
    assert!(state.status.contains("inbox"));

    state.palette.open = true;
    state.palette.input = "config set source.mailbox io-uring".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.status.contains("config updated"));
    assert_eq!(state.runtime.source_mailbox, "io-uring");

    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("mailbox = \"io-uring\""));

    state.palette.open = true;
    state.palette.input = "config get source.mailbox".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.status.contains("io-uring"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_palette_set_keymap_updates_navigation_immediately() {
    let root = temp_dir("palette-keymap");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
keymap = "default"
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let stale_runtime = runtime.clone();
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        runtime,
    );

    state.palette.open = true;
    state.palette.input = "config set ui.keymap vim".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert_eq!(state.runtime.ui_keymap, UiKeymap::Vim);
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("keymap = \"vim\""));
    state.palette.open = false;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));
    assert_eq!(state.thread_index, 1);

    let bootstrap = test_bootstrap(&stale_runtime);
    let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &stale_runtime, &bootstrap))
        .expect("draw updated keymap footer");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("h/l focus | j/k move"));
    assert!(!rendered.contains("j/l focus | i/k move"));
    assert!(!rendered.contains("gg/G jump"));
    assert!(!rendered.contains("qq quit"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn keymap_editor_custom_binding_updates_navigation_immediately() {
    let root = temp_dir("keymap-editor-custom");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
keymap = "custom"
keymap_base = "default"
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    runtime.ui_keymap = UiKeymap::Custom;
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        runtime,
    );

    state.palette.open = true;
    state.palette.input = "keymap".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.keymap_editor.open);

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(
        state.keymap_editor.mode,
        super::keymap::KeymapEditorMode::Capture
    ));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(
        state.keymap_editor.mode,
        super::keymap::KeymapEditorMode::Browse
    ));
    assert_eq!(
        state.runtime.ui_custom_keymap.focus_prev,
        Some(vec!["h".to_string()])
    );

    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("focus_prev = [\"h\"]"));

    state.keymap_editor.open = false;
    state.focus = Pane::Threads;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Subscriptions));

    state.focus = Pane::Threads;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn custom_bindings_do_not_leak_into_default_or_vim_schemes() {
    let root = temp_dir("keymap-scheme-isolation");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
keymap = "default"
keymap_base = "default"

[ui.custom_keymap]
focus_prev = ["x"]
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        runtime,
    );
    state.palette.open = true;
    state.palette.input = "keymap".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.keymap_editor.open);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
    );
    assert_eq!(state.runtime.ui_keymap, UiKeymap::Vim);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    state.focus = Pane::Threads;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Subscriptions));

    state.palette.open = true;
    state.palette.input = "keymap".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.keymap_editor.open);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
    );
    assert_eq!(state.runtime.ui_keymap, UiKeymap::Custom);
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
    );
    assert_eq!(state.runtime.ui_keymap, UiKeymap::Default);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    state.focus = Pane::Threads;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Subscriptions));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn keymap_editor_lists_active_bindings_for_selected_scheme() {
    let root = temp_dir("keymap-editor-active-bindings");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
keymap = "default"
keymap_base = "default"

[ui.custom_keymap]
focus_prev = ["x"]
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path;
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.open_keymap_editor();
    state.keymap_editor.selected_field = 2;

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw default keymap editor");
    let rendered_default = format!("{}", terminal.backend());
    assert!(rendered_default.contains("j [default]"));

    state.keymap_editor.selected_field = 0;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
    );
    state.keymap_editor.selected_field = 2;
    let runtime_vim = state.runtime.clone();
    let bootstrap_vim = test_bootstrap(&runtime_vim);
    terminal
        .draw(|frame| draw(frame, &state, &runtime_vim, &bootstrap_vim))
        .expect("draw vim keymap editor");
    let rendered_vim = format!("{}", terminal.backend());
    assert!(rendered_vim.contains("h [vim]"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_get_ui_keymap_returns_current_value() {
    let root = temp_dir("get-keymap");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
keymap = "vim"
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(vec![], runtime);

    state.palette.open = true;
    state.palette.input = "config get ui.keymap".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(
        state.status.contains("vim"),
        "config get should report vim, got: {}",
        state.status
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn loaded_vim_keymap_drives_navigation_keys() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        runtime,
    );
    state.subscription_index = 1;

    // Default keymap key 'j' would move focus left; in vim mode it moves down.
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert_eq!(state.thread_index, 1, "j should move down in vim keymap");

    // 'i' should NOT navigate (it is not a vim navigation key).
    let prev_index = state.thread_index;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert_eq!(
        state.thread_index, prev_index,
        "i should not navigate in vim keymap"
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    );
    assert!(
        matches!(state.focus, Pane::Subscriptions),
        "h should move focus left in vim keymap"
    );
}

#[test]
fn default_keymap_supports_counted_ik_navigation() {
    let mut state = AppState::new(sample_threads(15), test_runtime());

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
    );
    let action_down = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert!(matches!(action_down, LoopAction::Continue));
    assert_eq!(state.thread_index, 12);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
    );
    let action_up = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert!(matches!(action_up, LoopAction::Continue));
    assert_eq!(state.thread_index, 7);
}

#[test]
fn vim_keymap_supports_counted_jk_navigation() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(sample_threads(15), runtime);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Threads));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
    );
    let action_down = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(action_down, LoopAction::Continue));
    assert_eq!(state.thread_index, 12);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE),
    );
    let action_up = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert!(matches!(action_up, LoopAction::Continue));
    assert_eq!(state.thread_index, 7);
}

#[test]
fn counted_main_page_navigation_does_not_leak_into_focus_changes() {
    let mut state = AppState::new(sample_threads(4), test_runtime());

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
    );
    let focus_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(focus_action, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Threads));

    let move_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert!(matches!(move_action, LoopAction::Continue));
    assert_eq!(state.thread_index, 1);
}

#[test]
fn preview_focus_supports_minus_equals_shifted_equals_and_plus_thread_navigation() {
    let mut state = AppState::new(sample_threads(4), test_runtime());

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Preview));
    assert_eq!(state.thread_index, 1);

    state.preview_scroll = 7;
    let action_previous = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE),
    );
    assert!(matches!(action_previous, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Preview));
    assert_eq!(state.thread_index, 0);
    assert_eq!(state.preview_scroll, 0);

    state.preview_scroll = 5;
    let action_next_with_equals = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('='), KeyModifiers::NONE),
    );
    assert!(matches!(action_next_with_equals, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Preview));
    assert_eq!(state.thread_index, 1);
    assert_eq!(state.preview_scroll, 0);

    state.preview_scroll = 5;
    let action_next_with_shifted_equals = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('='), KeyModifiers::SHIFT),
    );
    assert!(matches!(
        action_next_with_shifted_equals,
        LoopAction::Continue
    ));
    assert!(matches!(state.focus, Pane::Preview));
    assert_eq!(state.thread_index, 2);
    assert_eq!(state.preview_scroll, 0);

    state.preview_scroll = 5;
    let action_next_with_plus = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('+'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action_next_with_plus, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Preview));
    assert_eq!(state.thread_index, 3);
    assert_eq!(state.preview_scroll, 0);
}

#[test]
fn resize_shortcuts_follow_the_focused_mail_pane_and_persist_layout() {
    let root = temp_dir("mail-pane-resize");
    let runtime = test_runtime_in(root.clone());
    fs::write(&runtime.config_path, "[ui]\nallow_preview_resize = true\n")
        .expect("write preview resize config");
    let mut state = AppState::new(vec![], runtime);

    let expand_subscriptions = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
    );
    assert!(matches!(expand_subscriptions, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.subscriptions_width, 27);
    assert_eq!(state.mail_pane_layout.preview_width, 85);

    let shrink_subscriptions = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('}'), KeyModifiers::SHIFT),
    );
    assert!(matches!(shrink_subscriptions, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.subscriptions_width, 23);

    state.focus = Pane::Threads;
    let expand_threads_left = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
    );
    assert!(matches!(expand_threads_left, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.subscriptions_width, 19);

    let shrink_threads_left = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('{'), KeyModifiers::SHIFT),
    );
    assert!(matches!(shrink_threads_left, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.subscriptions_width, 23);

    let expand_threads_right = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
    );
    assert!(matches!(expand_threads_right, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.preview_width, 81);

    let shrink_threads_right = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('}'), KeyModifiers::SHIFT),
    );
    assert!(matches!(shrink_threads_right, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.preview_width, 85);

    state.focus = Pane::Preview;
    let expand_preview = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
    );
    assert!(matches!(expand_preview, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.preview_width, 89);

    let shrink_preview = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('{'), KeyModifiers::SHIFT),
    );
    assert!(matches!(shrink_preview, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.preview_width, 85);

    let persisted = ui_state::load(&state.ui_state_path)
        .expect("load persisted ui state")
        .expect("ui state exists");
    assert_eq!(persisted.mail_subscriptions_width, 23);
    assert_eq!(persisted.mail_preview_width, 85);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resize_shortcuts_stop_at_fixed_edges_and_minimum_mail_pane_widths() {
    let mut state = AppState::new(vec![], test_runtime());

    let fixed_edge_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
    );
    assert!(matches!(fixed_edge_action, LoopAction::Continue));
    assert_eq!(
        state.mail_pane_layout.subscriptions_width,
        ui_state::DEFAULT_MAIL_SUBSCRIPTIONS_WIDTH
    );
    assert_eq!(state.status, "mail pane cannot expand in that direction");

    state.mail_pane_layout.subscriptions_width = MIN_MAIL_SUBSCRIPTIONS_WIDTH;

    let min_subscriptions_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('}'), KeyModifiers::SHIFT),
    );
    assert!(matches!(min_subscriptions_action, LoopAction::Continue));
    assert_eq!(
        state.mail_pane_layout.subscriptions_width,
        MIN_MAIL_SUBSCRIPTIONS_WIDTH
    );
    assert_eq!(state.status, "mail pane cannot shrink in that direction");

    state.focus = Pane::Preview;
    let preview_fixed_edge_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
    );
    assert!(matches!(preview_fixed_edge_action, LoopAction::Continue));
    assert_eq!(
        state.status,
        "preview pane resizing is disabled (set ui.allow_preview_resize = true)"
    );

    state.mail_pane_layout.preview_width = MIN_MAIL_PREVIEW_WIDTH;

    let min_preview_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('{'), KeyModifiers::SHIFT),
    );
    assert!(matches!(min_preview_action, LoopAction::Continue));
    assert_eq!(state.mail_pane_layout.preview_width, MIN_MAIL_PREVIEW_WIDTH);
    assert_eq!(
        state.status,
        "preview pane resizing is disabled (set ui.allow_preview_resize = true)"
    );
}

#[test]
fn vim_keymap_supports_gg_and_capital_g_jumps_on_mail_panes() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
            sample_thread("t2", "c@example.com", 2),
        ],
        runtime,
    );

    let subscription_rows = state.subscription_rows();
    assert!(!subscription_rows.is_empty());

    let action_bottom = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action_bottom, LoopAction::Continue));
    assert_eq!(
        state.subscription_row_index,
        subscription_rows.len().saturating_sub(1)
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let action_top = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(action_top, LoopAction::Continue));
    assert_eq!(state.subscription_row_index, 0);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    let action_thread_bottom = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action_thread_bottom, LoopAction::Continue));
    assert_eq!(state.thread_index, 2);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let action_thread_top = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(action_thread_top, LoopAction::Continue));
    assert_eq!(state.thread_index, 0);

    state.focus = Pane::Preview;
    state.preview_scroll = 7;
    let action_preview_bottom = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action_preview_bottom, LoopAction::Continue));
    assert_eq!(state.preview_scroll, u16::MAX);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let action_preview_top = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(action_preview_top, LoopAction::Continue));
    assert_eq!(state.preview_scroll, 0);
}

#[test]
fn vim_keymap_supports_gg_and_capital_g_jumps_in_code_browser() {
    let tree_root = temp_dir("vim-jump-code-browser");
    let file_path = tree_root.join("demo.c");
    let source = (1..=40)
        .map(|line| format!("int line_{line} = {line};"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file_path, format!("{source}\n")).expect("write file");

    let mut runtime = test_runtime_with_kernel_tree(tree_root.clone());
    runtime.ui_keymap = UiKeymap::Vim;
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));

    let action_tree_bottom = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action_tree_bottom, LoopAction::Continue));
    assert_eq!(
        state.kernel_tree_row_index,
        state.kernel_tree_rows.len().saturating_sub(1)
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let action_tree_top = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(action_tree_top, LoopAction::Continue));
    assert_eq!(state.kernel_tree_row_index, 0);

    state.code_focus = CodePaneFocus::Source;
    state.kernel_tree_row_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("file row exists");

    let mut terminal = Terminal::new(TestBackend::new(140, 18)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw source preview before vim jumps");

    state.code_preview_scroll = 9;
    let action_source_bottom = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action_source_bottom, LoopAction::Continue));
    let code_preview_scroll_limit = state.code_preview_scroll_limit.get();
    assert!(code_preview_scroll_limit > 0);
    assert_eq!(state.code_preview_scroll, code_preview_scroll_limit);

    let action_source_up = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert!(matches!(action_source_up, LoopAction::Continue));
    assert_eq!(
        state.code_preview_scroll,
        code_preview_scroll_limit.saturating_sub(1)
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let action_source_top = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(action_source_top, LoopAction::Continue));
    assert_eq!(state.code_preview_scroll, 0);

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn vim_keymap_supports_qq_quit_chord() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(vec![], runtime);

    let first = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    assert!(matches!(first, LoopAction::Continue));
    assert_eq!(
        state.status,
        "press qq to quit or use command palette quit/exit"
    );

    let second = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    assert!(matches!(second, LoopAction::Exit));
}

#[test]
fn vim_chords_do_not_leak_into_right_preview_pane() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        runtime,
    );

    let arm_quit = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    assert!(matches!(arm_quit, LoopAction::Continue));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(state.focus, Pane::Preview));

    state.preview_scroll = 11;
    let first_g = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(first_g, LoopAction::Continue));
    assert_eq!(state.preview_scroll, 11);

    let second_g = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(matches!(second_g, LoopAction::Continue));
    assert_eq!(state.preview_scroll, 0);
}

#[test]
fn preview_pane_shift_g_keeps_tui_renderable() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread("preview thread", "preview@example.com", 0)],
        runtime.clone(),
    );
    state.focus = Pane::Preview;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action, LoopAction::Continue));

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw after preview shift-g");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("Preview"));
}

#[test]
fn preview_pane_can_move_up_after_reaching_bottom() {
    let root = temp_dir("preview-scroll-bottom");
    let raw_path = root.join("preview.eml");
    let body = (1..=40)
        .map(|line| format!("preview line {line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        &raw_path,
        format!(
            "Message-ID: <preview-scroll@example.com>\r\nSubject: preview scroll\r\nFrom: preview@example.com\r\n\r\n{body}\n"
        ),
    )
    .expect("write raw mail");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "preview scroll",
            "preview-scroll@example.com",
            0,
            raw_path.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;

    let mut terminal = Terminal::new(TestBackend::new(120, 16)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw initial preview frame");

    let preview_scroll_limit = state.preview_scroll_limit.get();
    assert!(preview_scroll_limit > 0);

    for _ in 0..(preview_scroll_limit as usize + 10) {
        let _ = handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
    }
    assert_eq!(state.preview_scroll, preview_scroll_limit);

    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw bottom preview frame");
    let bottom_frame = format!("{}", terminal.backend());

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert_eq!(state.preview_scroll, preview_scroll_limit.saturating_sub(1));

    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw preview frame after moving up");
    let after_up_frame = format!("{}", terminal.backend());

    assert_ne!(after_up_frame, bottom_frame);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn preview_scroll_limit_accounts_for_wrapped_long_lines() {
    let root = temp_dir("preview-wrap-scroll");
    let raw_path = root.join("wrapped-preview.eml");
    let wrapped_body = format!("{} WRAP_TAIL_TOKEN\n", "x".repeat(380));
    fs::write(
        &raw_path,
        format!(
            "Message-ID: <wrapped-preview@example.com>\r\nSubject: wrapped preview\r\nFrom: preview@example.com\r\n\r\n{wrapped_body}"
        ),
    )
    .expect("write wrapped preview mail");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "wrapped preview",
            "wrapped-preview@example.com",
            0,
            raw_path.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;

    let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw wrapped preview");
    let top_frame = format!("{}", terminal.backend());
    let preview_scroll_limit = state.preview_scroll_limit.get();
    assert!(preview_scroll_limit > 0);
    assert!(!top_frame.contains("WRAP_TAIL_TOKEN"));

    for _ in 0..(preview_scroll_limit as usize + 5) {
        let _ = handle_key_event(
            &mut state,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
    }

    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw wrapped preview after scrolling");
    let bottom_frame = format!("{}", terminal.backend());

    assert!(bottom_frame.contains("WRAP_TAIL_TOKEN"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn source_pane_shift_g_keeps_tui_renderable() {
    let tree_root = temp_dir("source-pane-shift-g");
    let file_path = tree_root.join("demo.c");
    fs::write(
        &file_path,
        "int line_1;\nint line_2;\nint line_3;\nint line_4;\nint line_5;\n",
    )
    .expect("write source file");

    let mut runtime = test_runtime_with_kernel_tree(tree_root.clone());
    runtime.ui_keymap = UiKeymap::Vim;
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));
    state.code_focus = CodePaneFocus::Source;
    state.kernel_tree_row_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("file row exists");

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
    );
    assert!(matches!(action, LoopAction::Continue));

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw after source shift-g");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("Source Preview"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn config_command_opens_visual_editor() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "config".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert!(state.config_editor.open);
    assert!(!state.palette.open);
    assert_eq!(state.selected_config_editor_field().key, "source.mailbox");
}

#[test]
fn config_editor_saves_selected_value() {
    let root = temp_dir("config-editor-save");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[source]
mailbox = "inbox"
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("source.mailbox"));
    state.start_config_editor_edit();
    state.config_editor.input = "io-uring".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert_eq!(state.runtime.source_mailbox, "io-uring");
    assert!(!state.config_editor.open || state.config_editor.input.is_empty());
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("mailbox = \"io-uring\""));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_tab_cycles_boolean_presets() {
    let root = temp_dir("config-editor-toggle");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
startup_sync = true
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("ui.startup_sync"));
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert!(!state.runtime.startup_sync);
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("startup_sync = false"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_saves_inbox_auto_sync_interval() {
    let root = temp_dir("config-editor-auto-sync-interval");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
inbox_auto_sync_interval_secs = 30
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime_with_imap();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("ui.inbox_auto_sync_interval_secs"));
    state.start_config_editor_edit();
    state.config_editor.input = "45".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert_eq!(state.runtime.inbox_auto_sync_interval_secs, 45);
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("inbox_auto_sync_interval_secs = 45"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_can_unset_optional_key() {
    let root = temp_dir("config-editor-unset");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[b4]
path = "/usr/bin/b4"
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("b4.path"));
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );

    assert!(state.runtime.b4_path.is_none());
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(!persisted.contains("path = "));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_rejects_invalid_runtime_value_without_writing_file() {
    let root = temp_dir("config-editor-invalid");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
startup_sync = true
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("ui.startup_sync"));
    state.start_config_editor_edit();
    state.config_editor.input = "maybe".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(
        state
            .status
            .contains("failed to set config key ui.startup_sync")
    );
    assert!(state.runtime.startup_sync);
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("startup_sync = true"));
    assert!(!persisted.contains("maybe"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_rejects_zero_inbox_auto_sync_interval_without_writing_file() {
    let root = temp_dir("config-editor-zero-auto-sync-interval");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
inbox_auto_sync_interval_secs = 30
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime_with_imap();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("ui.inbox_auto_sync_interval_secs"));
    state.start_config_editor_edit();
    state.config_editor.input = "0".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(
        state
            .status
            .contains("failed to set config key ui.inbox_auto_sync_interval_secs")
    );
    assert_eq!(state.runtime.inbox_auto_sync_interval_secs, 30);
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert!(persisted.contains("inbox_auto_sync_interval_secs = 30"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_reports_unsupported_key_hint_and_allows_keyboard_navigation() {
    let mut state = AppState::new(vec![], test_runtime());

    state.open_config_editor(Some("unsupported.key"));

    assert!(state.config_editor.open);
    assert!(
        state
            .status
            .contains("config editor does not support unsupported.key")
    );
    assert_eq!(state.selected_config_editor_field().key, "source.mailbox");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_ne!(state.selected_config_editor_field().key, "source.mailbox");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(state.selected_config_editor_field().key, "source.mailbox");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.config_editor.open);
    assert_eq!(state.status, "config editor closed");
}

#[test]
fn config_editor_edit_mode_handles_char_backspace_tab_and_escape() {
    let root = temp_dir("config-editor-keyboard");
    let config_path = root.join("criew-config.toml");
    fs::write(
        &config_path,
        r#"
[ui]
startup_sync = true
"#,
    )
    .expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path;
    let mut state = AppState::new(vec![], runtime);

    state.open_config_editor(Some("ui.startup_sync"));
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(
        state.config_editor.mode,
        super::ConfigEditorMode::Edit
    ));
    assert_eq!(state.config_editor.input, "true");

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    assert_eq!(state.config_editor.input, "truex");

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(state.config_editor.input, "true");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.config_editor.input, "false");
    assert_eq!(state.status, "preset value selected for ui.startup_sync");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(
        state.config_editor.mode,
        super::ConfigEditorMode::Browse
    ));
    assert!(state.config_editor.input.is_empty());
    assert_eq!(state.status, "config edit cancelled");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_palette_help_and_usage_are_reported() {
    let mut state = AppState::new(vec![], test_runtime());

    state.palette.open = true;
    state.palette.input = "config help".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.status.contains("config usage:"));

    state.palette.open = true;
    state.palette.input = "config get".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.status, "usage: config get <key>");

    state.palette.open = true;
    state.palette.input = "config set".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.status, "usage: config set <key> <value>");
}

#[test]
fn config_palette_reports_effective_and_missing_values() {
    let mut state = AppState::new(vec![], test_runtime());

    state.palette.open = true;
    state.palette.input = "config get source.mailbox".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(
        state
            .status
            .contains("config effective source.mailbox = inbox (default/runtime)")
    );

    state.palette.open = true;
    state.palette.input = "config get no.such.key".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.status, "config key not found: no.such.key");
}

#[test]
fn config_palette_set_does_not_overwrite_scalar_parent_keys() {
    let root = temp_dir("config-palette-scalar-parent");
    let config_path = root.join("criew-config.toml");
    fs::write(&config_path, "source = \"broken\"\n").expect("write config file");

    let mut runtime = test_runtime();
    runtime.config_path = config_path.clone();
    let mut state = AppState::new(vec![], runtime);

    state.palette.open = true;
    state.palette.input = "config set source.mailbox io-uring".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(state.status.contains("cannot set source.mailbox"));
    assert_eq!(state.runtime.source_mailbox, "inbox");
    let persisted = fs::read_to_string(&config_path).expect("read config file");
    assert_eq!(persisted, "source = \"broken\"\n");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_editor_overlay_is_rendered() {
    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.open_config_editor(Some("source.mailbox"));

    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw config editor");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("Runtime Config"));
    assert!(rendered.contains("source.mailbox"));
    assert!(rendered.contains("Selected Field"));
}

#[test]
fn ctrl_backtick_toggles_command_palette() {
    let key = KeyEvent::new(KeyCode::Char('`'), KeyModifiers::CONTROL);
    assert!(is_palette_toggle(key));
}

#[test]
fn colon_opens_command_palette() {
    let mut state = AppState::new(vec![], test_runtime());

    let key = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT);
    assert!(is_palette_open_shortcut(key));

    let action = handle_key_event(&mut state, key);
    assert!(matches!(action, LoopAction::Continue));
    assert!(state.palette.open);
}

#[test]
fn palette_tab_completes_top_level_command() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "com".to_string();

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(state.palette.input, "compose ");
}

#[test]
fn palette_tab_completes_saved_view_actions_and_names() {
    let mut state = AppState::new(vec![], test_runtime());
    state.saved_views.push(SavedView {
        name: "patches".to_string(),
        query: "is:patch".to_string(),
    });
    state.palette.open = true;
    state.palette.input = "view u".to_string();
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.palette.input, "view use ");

    state.palette.input = "view use p".to_string();
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.palette.input, "view use patches ");
}

#[test]
fn palette_tab_completes_config_subcommand_and_key() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "config g".to_string();
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.palette.input, "config get ");

    state.palette.input = "config get source.m".to_string();
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.palette.input, "config get source.mailbox ");
}

#[test]
fn palette_tab_completes_sync_mailbox() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "sync bp".to_string();

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(state.palette.input, "sync bpf ");
}

#[test]
fn palette_tab_completes_review_inbox_mode() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "review-inbox rev".to_string();

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(state.palette.input, "review-inbox reviewed ");
}

#[test]
fn palette_double_tab_lists_config_arguments() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "config".to_string();

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(state.palette.input, "config ");
    assert!(!state.palette.show_suggestions);

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(state.palette.show_suggestions);
    let values: Vec<String> = state
        .palette
        .suggestions
        .iter()
        .map(|item| item.value.clone())
        .collect();
    assert!(values.contains(&"show".to_string()));
    assert!(values.contains(&"get".to_string()));
    assert!(values.contains(&"set".to_string()));
    assert!(values.contains(&"help".to_string()));
}

#[test]
fn palette_tab_completes_local_command_path() {
    let tree_root = temp_dir("palette-bang-complete");
    fs::write(tree_root.join("echo-local"), "#!/bin/sh\n").expect("write executable");
    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    state.palette.open = true;
    state.palette.input = "!./ec".to_string();

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(state.palette.input, "!./echo-local ");

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn local_command_mode_uses_kernel_tree_as_workdir() {
    let tree_root = temp_dir("palette-bang-kernel-tree");
    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let state = AppState::new(vec![], runtime);

    let workdir = resolve_palette_local_workdir(&state).expect("resolve local workdir");
    assert_eq!(workdir, tree_root);

    let _ = fs::remove_dir_all(workdir);
}

#[test]
fn local_command_mode_falls_back_to_home_workdir() {
    let state = AppState::new(vec![], test_runtime());
    let resolved = resolve_palette_local_workdir(&state);
    match std::env::var("HOME") {
        Ok(home) => assert_eq!(resolved.expect("resolve home"), PathBuf::from(home)),
        Err(_) => assert!(resolved.is_err()),
    }
}

#[test]
fn palette_bang_executes_local_command() {
    let tree_root = temp_dir("palette-bang-exec");
    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    state.palette.open = true;
    state.palette.input = "!pwd".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(state.status.contains("local command ok"));
    assert!(state.status.contains(&tree_root.display().to_string()));
    let local_result = state
        .palette
        .last_local_result
        .as_ref()
        .expect("local result should exist");
    assert_eq!(local_result.command, "pwd");
    assert!(
        local_result
            .output
            .contains(&tree_root.display().to_string())
    );

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn command_palette_renders_local_command_result() {
    let tree_root = temp_dir("palette-bang-render");
    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.palette.open = true;
    state.palette.input = "!pwd".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    let mut terminal = Terminal::new(TestBackend::new(180, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw frame");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Local Result"));
    assert!(rendered.contains("!pwd"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn tab_toggles_between_mail_page_and_code_browser_page() {
    let tree_root = temp_dir("kernel-tree-tab");
    fs::create_dir_all(tree_root.join("io_uring")).expect("create kernel dir");
    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);

    assert!(matches!(state.ui_page, UiPage::Mail));
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::Mail));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn kernel_tree_enter_expands_and_collapses_selected_directory() {
    let tree_root = temp_dir("kernel-tree-expand");
    let dir_a = tree_root.join("a");
    let dir_b = dir_a.join("b");
    let dir_c = tree_root.join("c");
    fs::create_dir_all(&dir_b).expect("create nested directory");
    fs::create_dir_all(&dir_c).expect("create sibling directory");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));

    let index_a = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == dir_a)
        .expect("directory a row exists");
    state.kernel_tree_row_index = index_a;
    assert!(state.kernel_tree_rows[index_a].expandable);
    assert!(!state.kernel_tree_rows.iter().any(|row| row.path == dir_b));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.kernel_tree_rows.iter().any(|row| row.path == dir_b));

    let index_a_after_expand = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == dir_a)
        .expect("directory a row exists after expand");
    state.kernel_tree_row_index = index_a_after_expand;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(!state.kernel_tree_rows.iter().any(|row| row.path == dir_b));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn kernel_tree_lists_files_and_source_preview_preserves_indentation() {
    let tree_root = temp_dir("kernel-tree-files");
    let dir_a = tree_root.join("a");
    let file_path = dir_a.join("demo.c");
    fs::create_dir_all(&dir_a).expect("create directory");
    fs::write(
        &file_path,
        "fn demo() {\n\tif true {\n        return;\n\t}\n}\n",
    )
    .expect("write source file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));

    let index_a = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == dir_a)
        .expect("directory a row exists");
    state.kernel_tree_row_index = index_a;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("file row exists");
    state.kernel_tree_row_index = file_index;

    let preview = load_source_file_preview(&file_path);
    assert!(preview.contains("    if true {"));
    assert!(preview.contains("        return;"));
    assert!(!preview.contains('\t'));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_mode_enters_only_on_source_file_focus() {
    let tree_root = temp_dir("code-edit-enter");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "fn demo() {}\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));

    state.code_focus = CodePaneFocus::Tree;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::Browse));
    assert!(state.status.contains("select a source file"));

    state.code_focus = CodePaneFocus::Source;
    state.kernel_tree_row_index = 0;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::Browse));
    assert!(state.status.contains("select a source file"));

    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));
    assert_eq!(state.code_edit_target.as_ref(), Some(&file_path));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_insert_save_and_escape_exit_updates_file() {
    let tree_root = temp_dir("code-edit-save-esc");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "alpha\nbeta\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimInsert));
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );
    let saved = fs::read_to_string(&file_path).expect("read saved file");
    assert!(saved.starts_with("!alpha"));

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(state.code_edit_mode, CodeEditMode::Browse));
    let preview = load_source_file_preview(&file_path);
    assert!(preview.contains("!alpha"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_command_mode_handles_dirty_q_w_and_wq() {
    let tree_root = temp_dir("code-edit-command");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "hello\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));
    assert!(state.code_edit_dirty);
    assert!(state.status.contains("unsaved changes"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(!state.code_edit_dirty);
    let saved_once = fs::read_to_string(&file_path).expect("read saved file");
    assert!(saved_once.starts_with("xhello"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.code_edit_dirty);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::Browse));
    let saved_twice = fs::read_to_string(&file_path).expect("read saved file");
    assert!(saved_twice.starts_with("xyhello"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_command_mode_rejects_unsupported_command() {
    let tree_root = temp_dir("code-edit-unsupported-command");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "hello\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));
    assert!(state.status.contains("unsupported command"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_command_mode_supports_force_quit_without_saving() {
    let tree_root = temp_dir("code-edit-force-quit");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "hello\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.code_edit_dirty);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(state.code_edit_mode, CodeEditMode::Browse));
    assert!(state.status.contains("discarded unsaved changes"));
    let disk = fs::read_to_string(&file_path).expect("read file");
    assert_eq!(disk, "hello\n");

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_browser_external_vim_key_updates_selected_file_preview() {
    let tree_root = temp_dir("external-vim-key");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "before\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    state.external_editor_runner = external_editor_mock_success;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
    );

    assert!(state.status.contains("external vim exited successfully"));
    let preview = load_source_file_preview(&file_path);
    assert!(preview.contains("externally edited"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_external_vim_rejects_dirty_buffer() {
    let tree_root = temp_dir("external-vim-dirty");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "hello\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    state.external_editor_runner = external_editor_mock_success;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(state.code_edit_dirty);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
    );

    assert!(
        state
            .status
            .contains("unsaved changes, run :w before external vim")
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));
    assert!(state.code_edit_dirty);
    let disk = fs::read_to_string(&file_path).expect("read file");
    assert_eq!(disk, "hello\n");

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_command_mode_vim_reloads_buffer_after_external_edit() {
    let tree_root = temp_dir("external-vim-command");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "hello\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    state.external_editor_runner = external_editor_mock_success;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));
    assert!(!state.code_edit_dirty);
    assert_eq!(
        state.code_edit_buffer.first().map(String::as_str),
        Some("externally edited")
    );
    assert!(state.status.contains("external vim exited successfully"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn command_palette_vim_runs_external_editor() {
    let tree_root = temp_dir("external-vim-palette");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "before\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    state.external_editor_runner = external_editor_mock_success;
    state.palette.open = true;
    state.palette.input = "vim".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(state.status.contains("external vim exited successfully"));
    let preview = load_source_file_preview(&file_path);
    assert!(preview.contains("externally edited"));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn external_vim_launch_failure_keeps_tui_interactive() {
    let tree_root = temp_dir("external-vim-failure");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "before\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    state.external_editor_runner = external_editor_mock_failure;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
    );
    assert!(state.status.contains("external vim failed"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_focus, CodePaneFocus::Tree));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn external_vim_marks_terminal_refresh_needed_after_return() {
    let tree_root = temp_dir("external-vim-refresh");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "before\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    state.external_editor_runner = external_editor_mock_success;

    assert!(!state.needs_terminal_refresh);
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('E'), KeyModifiers::SHIFT),
    );
    assert!(state.needs_terminal_refresh);
    assert!(state.take_terminal_refresh_needed());
    assert!(!state.needs_terminal_refresh);

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_edit_draw_sets_terminal_cursor_position() {
    let tree_root = temp_dir("code-edit-cursor");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "hello\nworld\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    state.code_focus = CodePaneFocus::Source;
    let file_index = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_path)
        .expect("find source file");
    state.kernel_tree_row_index = file_index;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_edit_mode, CodeEditMode::VimNormal));

    let mut terminal = Terminal::new(TestBackend::new(180, 30)).expect("create test terminal");
    let mut expected_cursor: Option<(u16, u16)> = None;
    terminal
        .draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(10),
                    Constraint::Length(1),
                ])
                .split(frame.area());
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(areas[1]);
            let inner_area = Rect::new(
                panes[1].x + 1,
                panes[1].y + 1,
                panes[1].width.saturating_sub(2),
                panes[1].height.saturating_sub(2),
            );
            expected_cursor = code_edit_cursor_position(&state, inner_area);
            draw(frame, &state, &runtime, &bootstrap);
        })
        .expect("draw frame");

    let expected = expected_cursor.expect("cursor position should be visible");
    terminal
        .backend_mut()
        .assert_cursor_position(Position::new(expected.0, expected.1));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn code_browser_navigation_keys_unchanged_when_not_editing() {
    let tree_root = temp_dir("code-edit-regression");
    let file_path = tree_root.join("demo.rs");
    fs::write(&file_path, "line1\nline2\n").expect("write demo file");

    let runtime = test_runtime_with_kernel_tree(tree_root.clone());
    let mut state = AppState::new(vec![], runtime);
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(state.ui_page, UiPage::CodeBrowser));
    assert!(matches!(state.code_focus, CodePaneFocus::Tree));

    state.code_focus = CodePaneFocus::Source;
    state.code_preview_scroll = 2;
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert_eq!(state.code_preview_scroll, 1);
    assert!(matches!(state.code_edit_mode, CodeEditMode::Browse));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert_eq!(state.code_preview_scroll, 2);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_focus, CodePaneFocus::Tree));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(state.code_focus, CodePaneFocus::Source));

    let _ = fs::remove_dir_all(tree_root);
}

#[test]
fn enter_on_subscription_opens_threads_and_focuses_threads_pane_without_toggling_enabled_state() {
    let root = temp_dir("enter-open-subscription");
    let runtime = test_runtime_with_imap_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "io-uring",
        1,
        "io-uring@example.com",
        "io-uring thread",
    );

    let mut state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec![IMAP_INBOX_MAILBOX.to_string(), "io-uring".to_string()],
            active_mailbox: Some(IMAP_INBOX_MAILBOX.to_string()),
            ..UiState::default()
        }),
    );
    state.focus = Pane::Subscriptions;
    let io_uring_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "io-uring")
        .expect("io-uring subscription exists");
    state.subscription_index = io_uring_index;
    state.sync_subscription_row_to_selected_item();
    let initial = state.subscriptions[io_uring_index].enabled;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert_eq!(state.subscriptions[io_uring_index].enabled, initial);
    assert!(matches!(state.focus, Pane::Threads));
    assert_eq!(state.active_thread_mailbox, "io-uring");
    assert_eq!(state.threads.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn enter_on_group_header_toggles_expand_and_collapse() {
    let mut state = AppState::new(vec![], test_runtime());
    state.focus = Pane::Subscriptions;
    state.subscription_row_index = 0;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(!state.enabled_group_expanded);
    let rows_after_collapse = state.subscription_rows();
    assert!(rows_after_collapse[0].text.starts_with('▶'));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.enabled_group_expanded);
    let rows_after_expand = state.subscription_rows();
    assert!(rows_after_expand[0].text.starts_with('▼'));
}

#[test]
fn enter_on_category_header_toggles_expand_and_collapse() {
    let mut state = AppState::new(vec![], test_runtime());
    state.focus = Pane::Subscriptions;
    state.subscription_row_index = state
        .subscription_rows()
        .iter()
        .position(|row| row.text.contains("linux subsystem"))
        .expect("linux category header exists");

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(!state.disabled_linux_subsystem_expanded);
    let rows_after_collapse = state.subscription_rows();
    let linux_header_after_collapse = rows_after_collapse
        .iter()
        .find(|row| row.text.contains("linux subsystem"))
        .expect("linux category header exists after collapse");
    assert!(linux_header_after_collapse.text.starts_with("  ▶"));
    assert!(
        !rows_after_collapse
            .iter()
            .any(|row| row.text.contains("[n] io-uring"))
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.disabled_linux_subsystem_expanded);
    let rows_after_expand = state.subscription_rows();
    let linux_header_after_expand = rows_after_expand
        .iter()
        .find(|row| row.text.contains("linux subsystem"))
        .expect("linux category header exists after expand");
    assert!(linux_header_after_expand.text.starts_with("  ▼"));
    assert!(
        rows_after_expand
            .iter()
            .any(|row| row.text.contains("[n] io-uring"))
    );
}

#[test]
fn first_open_starts_with_all_subscriptions_disabled() {
    let state = AppState::new(vec![], test_runtime());
    assert!(state.subscriptions.iter().all(|item| !item.enabled));
}

#[test]
fn subscription_rows_show_linux_and_qemu_categories() {
    let state = AppState::new(vec![], test_runtime());
    let rows = state.subscription_rows();

    let linux_header = rows
        .iter()
        .position(|row| row.text.contains("linux subsystem"))
        .expect("linux category header exists");
    let qemu_header = rows
        .iter()
        .position(|row| row.text.contains("qemu subsystem"))
        .expect("qemu category header exists");
    let qemu_devel = rows
        .iter()
        .position(|row| row.text.contains("[n] qemu-devel"))
        .expect("qemu-devel row exists");

    assert!(linux_header < qemu_header);
    assert!(qemu_header < qemu_devel);
}

#[test]
fn qemu_mailbox_case_variants_reuse_the_default_subscription() {
    let mut runtime = test_runtime();
    runtime.source_mailbox = "QEMU-devel".to_string();

    let state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec!["QEMU-devel".to_string()],
            active_mailbox: Some("QEMU-devel".to_string()),
            ..UiState::default()
        }),
    );

    let qemu_devel_items: Vec<&SubscriptionItem> = state
        .subscriptions
        .iter()
        .filter(|item| item.mailbox.eq_ignore_ascii_case("qemu-devel"))
        .collect();

    assert_eq!(qemu_devel_items.len(), 1);
    assert_eq!(qemu_devel_items[0].mailbox, "qemu-devel");
    assert!(qemu_devel_items[0].enabled);
    assert_eq!(
        qemu_devel_items[0].category,
        Some(SubscriptionCategory::QemuSubsystem)
    );
    assert_eq!(
        state.subscriptions[state.subscription_index].mailbox,
        "qemu-devel"
    );
}

#[test]
fn following_view_remains_visible_before_imap_configuration() {
    let state = AppState::new(vec![], test_runtime());
    let following = state
        .subscriptions
        .iter()
        .find(|item| item.mailbox == IMAP_INBOX_MAILBOX)
        .expect("My Mail subscription exists");

    assert_eq!(following.label, MY_INBOX_LABEL);
    assert!(!following.enabled);
    assert!(
        state
            .subscription_rows()
            .iter()
            .any(|row| row.text.contains("My Mail"))
    );
}

#[test]
fn first_open_with_complete_imap_enables_my_inbox() {
    let state = AppState::new(vec![], test_runtime_with_imap());
    let my_inbox = state
        .subscriptions
        .iter()
        .find(|item| item.mailbox == IMAP_INBOX_MAILBOX)
        .expect("my inbox subscription exists");

    assert!(my_inbox.enabled);
    assert_eq!(my_inbox.label, MY_INBOX_LABEL);
    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
}

#[test]
fn app_state_restores_and_re_persists_mail_pane_layout_from_ui_state() {
    let state = AppState::new_with_ui_state(
        vec![],
        test_runtime(),
        Some(UiState {
            mail_subscriptions_width: 29,
            mail_preview_width: 82,
            ..UiState::default()
        }),
    );

    assert_eq!(state.mail_pane_layout.subscriptions_width, 29);
    assert_eq!(state.mail_pane_layout.preview_width, 85);

    let persisted = state.to_ui_state();
    assert_eq!(persisted.mail_subscriptions_width, 29);
    assert_eq!(persisted.mail_preview_width, 85);
}

#[test]
fn legacy_ui_state_with_complete_imap_enables_my_inbox_once() {
    let state = AppState::new_with_ui_state(
        vec![],
        test_runtime_with_imap(),
        Some(UiState {
            enabled_mailboxes: vec!["io-uring".to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: false,
            active_mailbox: Some("io-uring".to_string()),
            ..UiState::default()
        }),
    );

    let my_inbox = state
        .subscriptions
        .iter()
        .find(|item| item.mailbox == IMAP_INBOX_MAILBOX)
        .expect("my inbox subscription exists");

    assert!(my_inbox.enabled);
    assert!(state.imap_defaults_initialized);
}

#[test]
fn initialized_ui_state_keeps_my_inbox_disabled_when_user_opted_out() {
    let state = AppState::new_with_ui_state(
        vec![],
        test_runtime_with_imap(),
        Some(UiState {
            enabled_mailboxes: vec!["io-uring".to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: true,
            active_mailbox: Some("io-uring".to_string()),
            ..UiState::default()
        }),
    );

    let my_inbox = state
        .subscriptions
        .iter()
        .find(|item| item.mailbox == IMAP_INBOX_MAILBOX)
        .expect("my inbox subscription exists");

    assert!(!my_inbox.enabled);
    assert!(state.imap_defaults_initialized);
}

#[test]
fn catch_sync_panic_converts_panics_into_errors() {
    let error = catch_sync_panic("INBOX", || -> crate::infra::error::Result<()> {
        panic!("boom");
    })
    .expect_err("panic should become criew error");

    assert!(error.to_string().contains("sync panicked for INBOX: boom"));
}

#[test]
fn empty_active_inbox_recovers_to_cached_enabled_mailbox() {
    let root = temp_dir("imap-fallback-cache");
    let runtime = test_runtime_with_imap_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "kvm",
        1,
        "kvm@example.com",
        "kvm thread",
    );

    let mut state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec![IMAP_INBOX_MAILBOX.to_string(), "kvm".to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: true,
            active_mailbox: Some(IMAP_INBOX_MAILBOX.to_string()),
            ..UiState::default()
        }),
    );

    assert!(state.recover_from_empty_active_mailbox("inbox unavailable"));
    assert_eq!(state.active_thread_mailbox, "kvm");
    assert_eq!(state.threads.len(), 1);
    assert!(state.status.contains("showing threads for kvm"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn startup_sync_failure_for_empty_inbox_falls_back_to_cached_mailbox() {
    let root = temp_dir("imap-fallback-startup");
    let runtime = test_runtime_with_imap_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "io-uring",
        1,
        "io-uring@example.com",
        "io_uring thread",
    );

    let mut state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec![IMAP_INBOX_MAILBOX.to_string(), "io-uring".to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: true,
            active_mailbox: Some(IMAP_INBOX_MAILBOX.to_string()),
            ..UiState::default()
        }),
    );

    state.apply_startup_sync_event(StartupSyncEvent::MailboxFailed {
        mailbox: IMAP_INBOX_MAILBOX.to_string(),
        error: "imap unavailable".to_string(),
    });

    assert_eq!(state.active_thread_mailbox, "io-uring");
    assert_eq!(state.threads.len(), 1);
    assert!(state.status.contains("showing threads for io-uring"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn command_palette_sync_queues_background_job_and_resets_my_inbox_auto_sync_deadline() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.manual_sync_spawner = manual_sync_spawner_idle;
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    run_palette_sync(&mut state, "sync INBOX");

    assert!(state.status.contains("sync queued in background"));
    assert!(state.manual_sync.is_some());
    assert!(
        state
            .inbox_auto_sync
            .as_ref()
            .expect("inbox auto-sync state")
            .next_due_at
            > Instant::now() + Duration::from_secs(20)
    );
}

#[test]
fn opening_empty_inbox_queues_background_sync_and_defers_next_auto_sync_tick() {
    let root = temp_dir("imap-open-inbox-sync");
    let runtime = test_runtime_with_imap_in(root.clone());
    fs::create_dir_all(runtime.database_path.parent().expect("db parent"))
        .expect("create db parent");
    db::initialize(&runtime.database_path).expect("initialize db");

    let mut state = AppState::new(vec![], runtime);
    state.manual_sync_spawner = manual_sync_spawner_idle;
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    state.open_threads_for_selected_subscription();

    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
    assert!(state.threads.is_empty());
    assert!(state.status.contains("syncing in background"));
    assert!(state.manual_sync.is_some());
    assert!(matches!(state.focus, Pane::Threads));
    assert!(
        state
            .inbox_auto_sync
            .as_ref()
            .expect("inbox auto-sync state")
            .next_due_at
            > Instant::now() + Duration::from_secs(20)
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn manual_sync_same_mailbox_request_reports_already_syncing() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.manual_sync = Some(manual_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::InFlight,
    )]));

    let outcome =
        state.start_manual_sync(vec!["inbox".to_string()], ManualSyncOrigin::PaletteCommand);

    assert_eq!(outcome, ManualSyncRequestOutcome::AlreadySyncing);
    assert!(state.status.contains("sync already running in background"));
}

#[test]
fn manual_sync_different_mailbox_request_reports_busy() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.manual_sync = Some(manual_sync_state(&[(
        "io-uring",
        StartupSyncMailboxStatus::InFlight,
    )]));

    let outcome = state.start_manual_sync(
        vec![IMAP_INBOX_MAILBOX.to_string()],
        ManualSyncOrigin::PaletteCommand,
    );

    assert_eq!(outcome, ManualSyncRequestOutcome::Busy);
    assert!(state.status.contains("background sync busy"));
    assert!(state.status.contains("0/1"));
}

#[test]
fn manual_sync_dedups_case_variants_and_defers_auto_sync_deadlines() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.manual_sync_spawner = manual_sync_spawner_idle;
    let io_uring_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "io-uring")
        .expect("io-uring subscription exists");
    state.subscriptions[io_uring_index].enabled = true;
    state.reconcile_subscription_auto_sync();
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);
    state
        .subscription_auto_sync
        .as_mut()
        .expect("subscription auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    let outcome = state.start_manual_sync(
        vec![
            IMAP_INBOX_MAILBOX.to_string(),
            "inbox".to_string(),
            "io-uring".to_string(),
            "IO-URING".to_string(),
        ],
        ManualSyncOrigin::PaletteCommand,
    );

    assert_eq!(outcome, ManualSyncRequestOutcome::Started);
    let sync_state = state.manual_sync.as_ref().expect("manual sync state");
    assert_eq!(
        sync_state.mailbox_order,
        vec!["INBOX".to_string(), "io-uring".to_string()]
    );
    assert!(
        state
            .inbox_auto_sync
            .as_ref()
            .expect("inbox auto-sync state")
            .next_due_at
            > Instant::now() + Duration::from_secs(20)
    );
    assert!(
        state
            .subscription_auto_sync
            .as_ref()
            .expect("subscription auto-sync state")
            .next_due_at
            > Instant::now() + Duration::from_secs(20)
    );
}

#[test]
fn command_palette_sync_queues_background_job_and_resets_subscription_auto_sync_deadline() {
    let mut state = AppState::new(vec![], test_runtime());
    state.manual_sync_spawner = manual_sync_spawner_idle;
    let io_uring_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "io-uring")
        .expect("io-uring subscription exists");
    state.subscriptions[io_uring_index].enabled = true;
    state.reconcile_subscription_auto_sync();
    state
        .subscription_auto_sync
        .as_mut()
        .expect("subscription auto-sync state")
        .next_due_at = Instant::now() - Duration::from_secs(1);

    run_palette_sync(&mut state, "sync io-uring");

    assert!(state.status.contains("sync queued in background"));
    assert!(state.manual_sync.is_some());
    assert!(
        state
            .subscription_auto_sync
            .as_ref()
            .expect("subscription auto-sync state")
            .next_due_at
            > Instant::now() + Duration::from_secs(20)
    );
}

#[test]
fn manual_sync_completion_refreshes_active_mailbox_after_worker_finishes() {
    let root = temp_dir("manual-sync-finish-refresh");
    let runtime = test_runtime_with_imap_in(root.clone());
    fs::create_dir_all(runtime.database_path.parent().expect("db parent"))
        .expect("create db parent");
    db::initialize(&runtime.database_path).expect("initialize db");

    let mut state = AppState::new(vec![], runtime);
    state.manual_sync_spawner = manual_sync_spawner_seed_success;

    state.open_threads_for_selected_subscription();
    assert!(state.threads.is_empty());
    assert!(state.manual_sync.is_some());

    state.pump_manual_sync_events();

    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
    assert_eq!(state.threads.len(), 1);
    assert!(state.status.contains("sync finished: ok=1"));
    assert!(state.manual_sync.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn manual_sync_failure_finishes_with_first_error_summary() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.manual_sync = Some(manual_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::Pending,
    )]));

    state.apply_manual_sync_event(StartupSyncEvent::MailboxFailed {
        mailbox: IMAP_INBOX_MAILBOX.to_string(),
        error: "imap unavailable".to_string(),
    });

    assert!(state.manual_sync.is_none());
    assert_eq!(state.status, "sync failed: INBOX: imap unavailable");
}

#[test]
fn manual_sync_partial_failure_reports_partial_summary() {
    let mut state = AppState::new(vec![], test_runtime());
    state.manual_sync = Some(manual_sync_state(&[
        ("io-uring", StartupSyncMailboxStatus::Pending),
        ("kvm", StartupSyncMailboxStatus::Pending),
    ]));

    state.apply_manual_sync_event(StartupSyncEvent::MailboxFinished {
        mailbox: "io-uring".to_string(),
        fetched: 2,
        inserted: 1,
        updated: 0,
    });
    assert!(state.manual_sync.is_some());

    state.apply_manual_sync_event(StartupSyncEvent::MailboxFailed {
        mailbox: "kvm".to_string(),
        error: "network timeout".to_string(),
    });

    assert!(state.manual_sync.is_none());
    assert!(state.status.contains("sync finished with failures"));
    assert!(state.status.contains("ok=1 failed=1"));
    assert!(state.status.contains("fetched=2 inserted=1 updated=0"));
}

#[test]
fn manual_sync_worker_disconnect_reports_failure_summary() {
    let mut state = AppState::new(vec![], test_runtime());
    state.manual_sync_spawner = manual_sync_spawner_idle;

    let outcome = state.start_manual_sync(
        vec!["io-uring".to_string()],
        ManualSyncOrigin::PaletteCommand,
    );
    assert_eq!(outcome, ManualSyncRequestOutcome::Started);

    state.pump_manual_sync_events();

    assert!(state.manual_sync.is_none());
    assert!(state.status.contains("background sync worker disconnected"));
}

#[test]
fn enter_on_mailbox_pending_startup_sync_stays_non_blocking() {
    let root = temp_dir("imap-pending-enter");
    let runtime = test_runtime_with_imap_in(root.clone());
    fs::create_dir_all(runtime.database_path.parent().expect("db parent"))
        .expect("create db parent");
    db::initialize(&runtime.database_path).expect("initialize db");

    let mut state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec![IMAP_INBOX_MAILBOX.to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: true,
            active_mailbox: Some(IMAP_INBOX_MAILBOX.to_string()),
            ..UiState::default()
        }),
    );
    state.focus = Pane::Subscriptions;
    state.startup_sync = Some(startup_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::InFlight,
    )]));

    state.open_threads_for_selected_subscription();

    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
    assert!(matches!(state.focus, Pane::Threads));
    assert!(state.threads.is_empty());
    assert!(state.status.contains("syncing in background"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn enter_on_mailbox_pending_manual_sync_stays_non_blocking() {
    let root = temp_dir("imap-pending-manual-enter");
    let runtime = test_runtime_with_imap_in(root.clone());
    fs::create_dir_all(runtime.database_path.parent().expect("db parent"))
        .expect("create db parent");
    db::initialize(&runtime.database_path).expect("initialize db");

    let mut state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec![IMAP_INBOX_MAILBOX.to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: true,
            active_mailbox: Some(IMAP_INBOX_MAILBOX.to_string()),
            ..UiState::default()
        }),
    );
    state.focus = Pane::Subscriptions;
    state.manual_sync = Some(manual_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::InFlight,
    )]));

    state.open_threads_for_selected_subscription();

    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
    assert!(state.threads.is_empty());
    assert!(state.status.contains("syncing in background"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn opening_empty_mailbox_while_other_manual_sync_is_busy_shows_busy_hint() {
    let root = temp_dir("manual-sync-busy-open");
    let runtime = test_runtime_with_imap_in(root.clone());
    fs::create_dir_all(runtime.database_path.parent().expect("db parent"))
        .expect("create db parent");
    db::initialize(&runtime.database_path).expect("initialize db");

    let mut state = AppState::new(vec![], runtime);
    state.manual_sync = Some(manual_sync_state(&[(
        "io-uring",
        StartupSyncMailboxStatus::InFlight,
    )]));

    state.open_threads_for_selected_subscription();

    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
    assert!(state.threads.is_empty());
    assert!(state.status.contains("another background sync is running"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn background_success_does_not_steal_focus_from_pending_inbox() {
    let root = temp_dir("imap-pending-focus");
    let runtime = test_runtime_with_imap_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "kvm",
        1,
        "kvm@example.com",
        "kvm thread",
    );

    let mut state = AppState::new_with_ui_state(
        vec![],
        runtime,
        Some(UiState {
            enabled_mailboxes: vec![IMAP_INBOX_MAILBOX.to_string(), "kvm".to_string()],
            enabled_group_expanded: true,
            disabled_group_expanded: true,
            enabled_linux_subsystem_expanded: true,
            enabled_qemu_subsystem_expanded: true,
            disabled_linux_subsystem_expanded: true,
            disabled_qemu_subsystem_expanded: true,
            imap_defaults_initialized: true,
            active_mailbox: Some(IMAP_INBOX_MAILBOX.to_string()),
            ..UiState::default()
        }),
    );
    state.startup_sync = Some(startup_sync_state(&[
        (IMAP_INBOX_MAILBOX, StartupSyncMailboxStatus::InFlight),
        ("kvm", StartupSyncMailboxStatus::Pending),
    ]));

    state.apply_startup_sync_event(StartupSyncEvent::MailboxFinished {
        mailbox: "kvm".to_string(),
        fetched: 1,
        inserted: 1,
        updated: 0,
    });

    assert_eq!(state.active_thread_mailbox, IMAP_INBOX_MAILBOX);
    assert!(state.threads.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn y_and_n_toggle_subscription_and_keep_grouped_sort_order() {
    let mut state = AppState::new(vec![], test_runtime());
    state.focus = Pane::Subscriptions;

    let target_index = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "bpf")
        .expect("bpf subscription exists");
    state.subscription_index = target_index;
    state.sync_subscription_row_to_selected_item();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
    );

    let bpf_after_enable = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "bpf")
        .expect("bpf exists after enable");
    assert!(state.subscriptions[bpf_after_enable].enabled);

    let first_disabled = state
        .subscriptions
        .iter()
        .position(|item| !item.enabled)
        .expect("has disabled subscriptions");
    assert!(bpf_after_enable < first_disabled);

    let enabled_group = &state.subscriptions[..first_disabled];
    assert!(
        enabled_group
            .windows(2)
            .all(|pair| subscription_sort_key(&pair[0]) <= subscription_sort_key(&pair[1]))
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );

    let bpf_after_disable = state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "bpf")
        .expect("bpf exists after disable");
    assert!(!state.subscriptions[bpf_after_disable].enabled);

    let last_enabled = state.subscriptions.iter().rposition(|item| item.enabled);
    if let Some(last_enabled) = last_enabled {
        assert!(bpf_after_disable > last_enabled);

        let disabled_group = &state.subscriptions[last_enabled + 1..];
        assert!(
            disabled_group
                .windows(2)
                .all(|pair| subscription_sort_key(&pair[0]) <= subscription_sort_key(&pair[1]))
        );
    } else {
        assert!(state.subscriptions.iter().all(|item| !item.enabled));
        assert!(
            state
                .subscriptions
                .windows(2)
                .all(|pair| subscription_sort_key(&pair[0]) <= subscription_sort_key(&pair[1]))
        );
    }
}

#[test]
fn slash_opens_search_and_filters_threads() {
    let mut state = AppState::new(
        vec![
            sample_thread("[PATCH] mm cleanup", "a@example.com", 0),
            sample_thread("[PATCH] net fix", "b@example.com", 0),
        ],
        test_runtime(),
    );

    let action_search = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    assert!(matches!(action_search, LoopAction::Continue));
    assert!(state.search.active);

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(!state.search.active);
    assert_eq!(state.filtered_thread_indices.len(), 1);
    let selected = state.selected_thread().expect("selected thread");
    assert_eq!(selected.message_id, "b@example.com");
}

#[test]
fn structured_search_filters_subject_sender_id_and_negation() {
    let mut second = sample_thread("[PATCH] net fix", "net@example.com", 0);
    second.from_addr = "Bob <bob@example.com>".to_string();
    let mut state = AppState::new(
        vec![
            sample_thread("[PATCH] mm cleanup", "mm@example.com", 0),
            second,
            sample_thread("[PATCH] docs update", "docs@example.com", 0),
        ],
        test_runtime(),
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    type_text(
        &mut state,
        "subject:\"net fix\" from:bob id:net -subject:docs",
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert_eq!(state.filtered_thread_indices.len(), 1);
    assert_eq!(
        state
            .selected_thread()
            .expect("selected structured result")
            .message_id,
        "net@example.com"
    );
    assert!(state.status.contains("1 matches"));
}

#[test]
fn invalid_structured_search_reports_query_error_without_matches() {
    let mut state = AppState::new(
        vec![sample_thread("[PATCH] demo", "demo@example.com", 0)],
        test_runtime(),
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    type_text(&mut state, "label:demo");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(state.filtered_thread_indices.is_empty());
    assert!(state.status.contains("unknown query field 'label'"));
}

#[test]
fn saved_views_persist_apply_and_delete_structured_queries() {
    let root = temp_dir("saved-views");
    let runtime = test_runtime_in(root.clone());
    let mut state = AppState::new(
        vec![
            sample_thread("[PATCH] mm cleanup", "mm@example.com", 0),
            sample_thread("Documentation", "docs@example.com", 0),
        ],
        runtime.clone(),
    );
    state.search.applied_query = "is:patch".to_string();
    state.search.input = "is:patch".to_string();
    state.apply_thread_filter();

    state.palette.open = true;
    state.palette.input = "view save patches".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.saved_views.len(), 1);
    assert_eq!(state.saved_views[0].query, "is:patch");
    let persisted = ui_state::load(&ui_state::path_for_data_dir(&runtime.data_dir))
        .expect("load saved views")
        .expect("saved view state");
    assert_eq!(persisted.saved_views[0].name, "patches");

    state.search.applied_query.clear();
    state.search.input.clear();
    state.apply_thread_filter();
    state.palette.open = true;
    state.palette.input = "view use patches".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.search.applied_query, "is:patch");
    assert_eq!(state.filtered_thread_indices.len(), 1);
    assert!(state.status.contains("saved view 'patches' applied"));

    state.palette.open = true;
    state.palette.input = "view delete patches".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.saved_views.is_empty());
    let persisted = ui_state::load(&ui_state::path_for_data_dir(&runtime.data_dir))
        .expect("reload saved views")
        .expect("saved view state");
    assert!(persisted.saved_views.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_on_code_browser_reports_mail_only_scope() {
    let mut state = AppState::new(vec![], test_runtime());
    state.ui_page = UiPage::CodeBrowser;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert!(!state.search.active);
    assert_eq!(state.status, "search is only available on mail page");
}

#[test]
fn search_backspace_and_escape_clear_pending_query() {
    let mut state = AppState::new(vec![], test_runtime());

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    type_text(&mut state, "ab");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(state.search.input, "a");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(!state.search.active);
    assert!(state.search.input.is_empty());
    assert_eq!(state.status, "search cancelled");
}

#[test]
fn ctrl_backtick_closes_open_palette() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "help".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('`'), KeyModifiers::CONTROL),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert!(!state.palette.open);
    assert!(state.palette.input.is_empty());
    assert_eq!(state.status, "command palette closed");
}

#[test]
fn palette_reports_empty_and_unknown_commands() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "   ".to_string();

    let empty_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(empty_action, LoopAction::Continue));
    assert_eq!(state.status, "empty command");

    state.palette.open = true;
    state.palette.input = "wat".to_string();
    let unknown_action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(matches!(unknown_action, LoopAction::Continue));
    assert_eq!(state.status, "unknown command: wat");
}

#[test]
fn palette_escape_backspace_and_char_input_update_buffer() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "he".to_string();

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(state.palette.input, "h");

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert_eq!(state.palette.input, "hi");

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.palette.open);
    assert!(state.palette.input.is_empty());
}

#[test]
fn palette_sync_command_runs_via_handle_key_event() {
    let mut state = AppState::new(vec![], test_runtime());
    state.manual_sync_spawner = manual_sync_spawner_idle;
    state.palette.open = true;
    state.palette.input = "sync io-uring".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert!(state.status.contains("sync queued in background"));
    assert!(state.manual_sync.is_some());
    assert!(!state.palette.open);
    assert!(state.palette.input.is_empty());
}

#[test]
fn palette_fetch_thread_preserves_message_id_case_and_explicit_mailbox() {
    let mut state = AppState::new(vec![], test_runtime());
    state.thread_fetch_spawner = thread_fetch_spawner_idle;
    state.palette.open = true;
    state.palette.input = "fetch-thread --mailbox IO-URING <Patch@EXAMPLE.COM>".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    let fetch = state.thread_fetch.as_ref().expect("thread fetch queued");
    assert_eq!(fetch.mailbox, "IO-URING");
    assert_eq!(fetch.message_id, "<Patch@EXAMPLE.COM>");
    assert!(!state.palette.open);
}

#[test]
fn palette_review_inbox_filters_to_reviewed_patch_series() {
    let mut state = AppState::new(
        vec![
            sample_thread_in_thread(1, 1, "[PATCH 1/1] needs review", "needs@example.com", 0),
            sample_thread_in_thread(2, 2, "[PATCH 1/1] already reviewed", "done@example.com", 0),
        ],
        test_runtime(),
    );
    state.review_summaries.insert(
        1,
        ReviewInboxEntry {
            thread_id: 1,
            subject: "[PATCH 1/1] needs review".to_string(),
            anchor_message_id: "needs@example.com".to_string(),
            version: 1,
            present_count: 1,
            expected_total: 1,
            integrity: SeriesIntegrity::Complete,
            trailers: Vec::new(),
        },
    );
    state.review_summaries.insert(
        2,
        ReviewInboxEntry {
            thread_id: 2,
            subject: "[PATCH 1/1] already reviewed".to_string(),
            anchor_message_id: "done@example.com".to_string(),
            version: 1,
            present_count: 1,
            expected_total: 1,
            integrity: SeriesIntegrity::Complete,
            trailers: vec![crate::infra::mail_parser::ParsedTrailer {
                kind: "Reviewed-by".to_string(),
                value: "Reviewer <reviewer@example.com>".to_string(),
            }],
        },
    );
    state.palette.open = true;
    state.palette.input = "review-inbox reviewed".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert_eq!(state.review_inbox_mode, Some(ReviewInboxMode::Reviewed));
    assert_eq!(state.filtered_thread_indices, vec![1]);
    assert_eq!(state.status, "Review Inbox: reviewed (1 matching threads)");
}

#[test]
fn capital_f_queues_complete_thread_fetch_for_selected_thread() {
    let mut state = AppState::new(
        vec![sample_thread("thread", "Patch@EXAMPLE.COM", 0)],
        test_runtime(),
    );
    state.thread_fetch_spawner = thread_fetch_spawner_idle;
    state.focus = Pane::Threads;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    let fetch = state.thread_fetch.as_ref().expect("thread fetch queued");
    assert_eq!(fetch.mailbox, state.active_thread_mailbox);
    assert_eq!(fetch.message_id, "Patch@EXAMPLE.COM");
}

#[test]
fn palette_bang_reports_empty_local_command() {
    let mut state = AppState::new(vec![], test_runtime());
    state.palette.open = true;
    state.palette.input = "!   ".to_string();

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert_eq!(state.status, "empty local command after !");
}

#[test]
fn enter_on_thread_focuses_preview_and_sets_selected_status_message() {
    let mut state = AppState::new(
        vec![sample_thread("normal mail", "plain@example.com", 0)],
        test_runtime(),
    );
    state.focus = Pane::Threads;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Preview));
    assert_eq!(state.status, "selected plain@example.com");
}

#[test]
fn escape_quit_and_ctrl_c_show_exit_guidance() {
    let mut state = AppState::new(vec![], test_runtime());

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        state.status,
        "open command palette with : (preferred) or Ctrl+`"
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    );
    assert_eq!(
        state.status,
        "q emergency exit disabled; use command palette quit/exit"
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        state.status,
        "Ctrl+C is disabled, use command palette quit/exit"
    );
}

#[test]
fn jl_focus_and_ik_move_selection() {
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        test_runtime(),
    );
    state.subscription_index = 1;

    let action_l = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(action_l, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Threads));

    let action_i = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert!(matches!(action_i, LoopAction::Continue));
    assert_eq!(state.thread_index, 0);

    let action_k = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert!(matches!(action_k, LoopAction::Continue));
    assert_eq!(state.thread_index, 1);

    let action_j = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(action_j, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Subscriptions));
}

#[test]
fn vim_keymap_uses_hl_focus_and_jk_move_selection() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Vim;
    let mut state = AppState::new(
        vec![
            sample_thread("t0", "a@example.com", 0),
            sample_thread("t1", "b@example.com", 1),
        ],
        runtime,
    );
    state.subscription_index = 1;

    let action_l = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    );
    assert!(matches!(action_l, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Threads));

    let action_j = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert!(matches!(action_j, LoopAction::Continue));
    assert_eq!(state.thread_index, 1);

    let action_k = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert!(matches!(action_k, LoopAction::Continue));
    assert_eq!(state.thread_index, 0);

    let action_h = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
    );
    assert!(matches!(action_h, LoopAction::Continue));
    assert!(matches!(state.focus, Pane::Subscriptions));
}

#[test]
fn a_d_and_u_require_patch_series_or_apply_snapshot_on_thread_focus() {
    let mut state = AppState::new(
        vec![sample_thread("normal mail", "plain@example.com", 0)],
        test_runtime(),
    );
    state.focus = Pane::Threads;

    let action_apply = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
    );
    assert!(matches!(action_apply, LoopAction::Continue));
    assert!(state.status.contains("not a patch series"));

    let action_download = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    );
    assert!(matches!(action_download, LoopAction::Continue));
    assert!(state.status.contains("not a patch series"));

    let action_preflight = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('P'), KeyModifiers::NONE),
    );
    assert!(matches!(action_preflight, LoopAction::Continue));
    assert!(state.status.contains("not a patch series"));

    let action_undo = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
    );
    assert!(matches!(action_undo, LoopAction::Continue));
    assert!(state.status.contains("no apply action to undo"));
}

#[test]
fn inline_ui_text_collapses_multiline_errors() {
    let sanitized = sanitize_inline_ui_text(
        "sync failed:\nCould not automatically determine provider\r\n\tline2",
    );

    assert_eq!(
        sanitized,
        "sync failed: Could not automatically determine provider line2"
    );
}

#[test]
fn preview_hides_rfc_headers_and_keeps_body() {
    let raw = b"Message-ID: <a@example.com>\r\nSubject: test\r\nFrom: a@example.com\r\n\r\nhello\nworld\n";
    let preview = extract_mail_body_preview(raw);
    assert!(!preview.contains("Message-ID:"));
    assert!(!preview.contains("Subject: test"));
    assert!(preview.contains("hello"));
    assert!(preview.contains("world"));
}

#[test]
fn preview_skips_first_mime_part_headers() {
    let raw = b"Content-Type: multipart/alternative; boundary=\"abc\"\r\n\r\n--abc\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nplain body line\r\n--abc--\r\n";
    let preview = extract_mail_body_preview(raw);
    assert!(!preview.contains("Content-Transfer-Encoding"));
    assert!(preview.contains("plain body line"));
}

#[test]
fn preview_strips_control_characters() {
    let raw =
        b"Message-ID: <a@example.com>\r\nSubject: test\r\n\r\nline1\x1b[31m\x07\nline2\tok\r\n";
    let preview = extract_mail_body_preview(raw);
    assert!(!preview.contains('\u{001b}'));
    assert!(!preview.contains('\u{0007}'));
    assert!(!preview.contains('\t'));
    assert!(preview.contains("line1"));
    assert!(preview.contains("line2    ok"));
}

#[test]
fn preview_shows_from_sent_to_cc_headers() {
    let raw = b"From: Chen Miao <chenmiao.ku@gmail.com>\r\nDate: Monday, March 2, 2026 5:29 PM\r\nTo: Daniel Baluta <daniel.baluta@nxp.com>; Simona Toaca <simona.toaca@nxp.com>\r\nCc: Team One <team1@example.com>\r\nSubject: [PATCH] demo\r\n\r\nmail body line\n";
    let preview = extract_mail_preview(raw, "(no subject)", "<unknown sender>", None);

    assert!(preview.contains("From: Chen Miao <chenmiao.ku@gmail.com>"));
    assert!(preview.contains("Sent: Monday, March 2, 2026 5:29 PM"));
    assert!(preview.contains(
        "To: Daniel Baluta <daniel.baluta@nxp.com>; Simona Toaca <simona.toaca@nxp.com>"
    ));
    assert!(preview.contains("Cc: Team One <team1@example.com>"));
    assert!(preview.contains("Subject: [PATCH] demo"));
    assert!(preview.contains("mail body line"));
}

#[test]
fn preview_truncates_to_and_cc_recipient_lists() {
    let raw = b"From: sender@example.com\r\nDate: Tue, 3 Mar 2026 12:00:00 +0000\r\nTo: A <a@example.com>, B <b@example.com>, C <c@example.com>\r\nCc: X <x@example.com>; Y <y@example.com>; Z <z@example.com>\r\nSubject: test\r\n\r\nbody\n";
    let preview = extract_mail_preview(raw, "(no subject)", "<unknown sender>", None);

    assert!(preview.contains("To: A <a@example.com>; B <b@example.com>; ..."));
    assert!(preview.contains("Cc: X <x@example.com>; Y <y@example.com>; ..."));
    assert!(!preview.contains("C <c@example.com>"));
    assert!(!preview.contains("Z <z@example.com>"));
}

#[test]
fn preview_warns_for_multipart_mail() {
    let raw = b"Content-Type: multipart/alternative; boundary=\"abc\"\r\n\r\n--abc\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nplain body line\r\n--abc--\r\n";
    let warning = preview_warning_message(raw).expect("warning expected");

    assert!(warning.contains("NON-PLAIN-TEXT MAIL"));
    assert!(warning.contains("Parse artifacts/errors are normal"));
    assert!(warning.contains("Content-Type: multipart/alternative; boundary=\"abc\""));
}

#[test]
fn preview_warns_for_encoded_html_mail() {
    let raw = b"Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<html><body>hello</body></html>\r\n";
    let warning = preview_warning_message(raw).expect("warning expected");

    assert!(warning.contains("NON-PLAIN-TEXT MAIL"));
    assert!(warning.contains("Content-Type: text/html; charset=utf-8"));
    assert!(warning.contains("Transfer-Encoding: quoted-printable"));
}

#[test]
fn multiline_sync_error_does_not_break_footer_or_palette_render() {
    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.status = "sync failed: E1007:\nCould not automatically determine provider".to_string();
    state.palette.open = true;

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw multiline status");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("sync failed: E1007: Could not automatically determine provider"));
    assert!(rendered.contains("Command Palette"));
}

#[test]
fn header_shows_criew_brand_and_default_footer_hides_empty_status() {
    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let state = AppState::new(vec![], runtime.clone());

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw branded header");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("CRIEW"));
    assert!(rendered.contains(env!("CARGO_PKG_VERSION")));
    assert!(rendered.contains("Mail / inbox"));
    assert!(rendered.contains("keymap default"));
    assert!(!rendered.contains("db schema"));
    assert!(!rendered.contains("db:"));
    assert!(!rendered.contains("status:"));
    assert!(!rendered.contains(" ready "));
}

#[test]
fn header_shows_custom_keymap_scheme_when_configured() {
    let mut runtime = test_runtime();
    runtime.ui_keymap = UiKeymap::Custom;
    let bootstrap = test_bootstrap(&runtime);
    let state = AppState::new(vec![], runtime.clone());

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw custom keymap header");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("keymap custom"));
}

#[test]
fn header_formats_uptime_with_clock_units() {
    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.started_at = Instant::now() - Duration::from_secs(61);

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw formatted uptime header");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("up 01m:"));
    assert!(!rendered.contains("up 61s"));
}

#[test]
fn startup_sync_progress_bar_renders_at_right_edge_of_header() {
    let runtime = test_runtime_in(PathBuf::from("/t"));
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.startup_sync = Some(startup_sync_state(&[
        ("INBOX", StartupSyncMailboxStatus::InFlight),
        ("io-uring", StartupSyncMailboxStatus::Pending),
        ("kvm", StartupSyncMailboxStatus::Finished),
    ]));
    state.status = "startup sync [1/3] syncing INBOX...".to_string();

    let mut terminal = Terminal::new(TestBackend::new(260, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw startup sync");
    let rendered = format!("{}", terminal.backend());
    let header_row = rendered_row_text(&terminal, 0);
    let footer_row = rendered_row_text(&terminal, terminal.backend().buffer().area().height - 1);
    let progress_text = sanitize_inline_ui_text(
        &state
            .background_sync_progress_text()
            .expect("background sync progress"),
    );

    assert!(rendered.contains("Mail / inbox"));
    assert!(rendered.contains("sync ["));
    assert!(rendered.contains("1/3"));
    assert!(rendered.contains("INBOX"));
    assert!(rendered.contains("startup sync [1/3] syncing INBOX..."));
    assert!(header_row.trim_end().ends_with(&progress_text));
    assert!(header_row.ends_with(' '));
    assert!(!footer_row.contains(&progress_text));
}

#[test]
fn background_sync_progress_text_prefers_manual_sync_over_other_sources() {
    let mut state = AppState::new(vec![], test_runtime_with_imap());
    state.manual_sync = Some(manual_sync_state(&[(
        "io-uring",
        StartupSyncMailboxStatus::InFlight,
    )]));
    state.startup_sync = Some(startup_sync_state(&[(
        IMAP_INBOX_MAILBOX,
        StartupSyncMailboxStatus::InFlight,
    )]));
    state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .receiver = Some(mpsc::channel().1);

    let progress = state
        .background_sync_progress_text()
        .expect("background progress");

    assert!(progress.contains("0/1"));
    assert!(progress.contains("io-uring"));
    assert!(!progress.contains("auto INBOX"));
}

#[test]
fn background_sync_progress_text_reports_auto_sync_sources() {
    let mut inbox_state = AppState::new(vec![], test_runtime_with_imap());
    inbox_state
        .inbox_auto_sync
        .as_mut()
        .expect("inbox auto-sync state")
        .receiver = Some(mpsc::channel().1);
    let inbox_progress = inbox_state
        .background_sync_progress_text()
        .expect("inbox progress");
    assert!(inbox_progress.contains("auto Following"));
    assert_eq!(inbox_progress.matches('>').count(), 3);

    let mut subscription_state = AppState::new(vec![], test_runtime());
    let io_uring_index = subscription_state
        .subscriptions
        .iter()
        .position(|item| item.mailbox == "io-uring")
        .expect("io-uring subscription exists");
    subscription_state.subscriptions[io_uring_index].enabled = true;
    subscription_state.reconcile_subscription_auto_sync();
    let state = subscription_state
        .subscription_auto_sync
        .as_mut()
        .expect("subscription auto-sync state");
    state.receiver = Some(mpsc::channel().1);
    state.in_flight_mailboxes.insert("io-uring".to_string());

    let subscription_progress = subscription_state
        .background_sync_progress_text()
        .expect("subscription progress");
    assert!(subscription_progress.contains("auto io-uring"));
    assert_eq!(subscription_progress.matches('>').count(), 3);
}

#[test]
fn progress_bar_helpers_cover_zero_total_and_completed_states() {
    let state = AppState::new(vec![], test_runtime());

    let zero_total = state.render_progress_bar(0, 0);
    let completed = state.render_progress_bar(3, 3);
    let indeterminate = state.render_indeterminate_progress_bar();

    assert_eq!(zero_total, "[............]");
    assert!(completed.starts_with('['));
    assert!(completed.ends_with(']'));
    assert_eq!(completed.matches('=').count(), 12);
    assert_eq!(completed.matches('>').count(), 0);
    assert_eq!(indeterminate.len(), 14);
    assert_eq!(indeterminate.matches('>').count(), 3);
}

#[test]
fn manual_sync_progress_bar_is_rendered_at_right_edge_of_header() {
    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.manual_sync = Some(manual_sync_state(&[
        (IMAP_INBOX_MAILBOX, StartupSyncMailboxStatus::InFlight),
        ("io-uring", StartupSyncMailboxStatus::Pending),
    ]));

    let mut terminal = Terminal::new(TestBackend::new(140, 35)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw manual sync progress");
    let rendered = format!("{}", terminal.backend());
    let header_row = rendered_row_text(&terminal, 0);
    let footer_row = rendered_row_text(&terminal, terminal.backend().buffer().area().height - 1);
    let progress_text = sanitize_inline_ui_text(
        &state
            .background_sync_progress_text()
            .expect("background sync progress"),
    );

    assert!(rendered.contains("sync ["));
    assert!(rendered.contains("0/2"));
    assert!(rendered.contains(IMAP_INBOX_MAILBOX));
    assert!(header_row.trim_end().ends_with(&progress_text));
    assert!(header_row.ends_with(' '));
    assert!(!footer_row.contains(&progress_text));
}

#[test]
fn mail_preview_e_opens_reply_panel_with_autofilled_headers() {
    let root = temp_dir("reply-open");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: CRIEW Test <criew@example.com>, Bob <bob@example.com>\r\nCc: Alice <alice@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );

    let panel = state.reply_panel.as_ref().expect("reply panel should open");
    assert_eq!(panel.from, "CRIEW Test <criew@example.com>");
    assert_eq!(panel.to, "Bob <bob@example.com>");
    assert_eq!(panel.cc, "Alice <alice@example.com>");
    assert_eq!(panel.subject, "Re: [PATCH] demo");
    assert_eq!(panel.in_reply_to, "patch@example.com");
    assert_eq!(panel.references, vec!["patch@example.com"]);
    assert_eq!(panel.section, ReplySection::From);

    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw reply panel");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Reply Panel"));
    assert!(rendered.contains("focus:From"));
    assert!(rendered.contains("Headers ([edit] / [read-only])"));
    assert!(rendered.contains("Reply Body"));
    assert!(rendered.contains("[edit] To: Bob <bob@example.com>"));
    assert!(rendered.contains("[edit] Cc: Alice <alice@example.com>"));
    assert!(rendered.contains("[read-only] In-Reply-To: <patch@example.com>"));
    assert!(rendered.contains("Subject: Re: [PATCH] demo"));
    assert!(
        state
            .status
            .contains("edit From/To/Cc/Subject before Send Preview")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_panel_body_renders_80_column_guide_marker() {
    let root = temp_dir("reply-body-guide");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let panel = state.reply_panel.as_mut().expect("reply panel should open");
    panel.body = vec!["short line".to_string(), String::new()];
    panel.body_row = 0;

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw reply panel with guide");
    let rendered = format!("{}", terminal.backend());

    assert!(rendered.contains("Reply Body"));
    assert!(rendered.contains("80 cols"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_preview_requires_confirmation_before_send() {
    let root = temp_dir("reply-send-gate");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let runtime = test_runtime_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "inbox",
        1,
        "patch@example.com",
        "[PATCH] demo",
    );

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;
    state.reply_send_executor = reply_send_mock_success;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );
    assert!(state.status.contains("run Send Preview and confirm first"));
    assert!(
        state
            .reply_panel
            .as_ref()
            .and_then(|panel| panel.reply_notice.as_ref())
            .is_some_and(|notice| notice.title == "Send Blocked")
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_open)
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_confirmed)
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .and_then(|panel| panel.reply_notice.as_ref())
            .is_some_and(|notice| notice.title == "Ready To Send")
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );
    assert!(state.status.contains("reply sent as <sent@example.com>"));
    assert!(state.reply_panel.is_none());

    let record = reply_store::latest_reply_send_for_mail(&runtime.database_path, 1)
        .expect("load latest reply send")
        .expect("reply send record");
    assert_eq!(record.status, ReplySendStatus::Sent);
    assert_eq!(record.message_id, "sent@example.com");
    assert_eq!(record.subject, "Re: [PATCH] demo");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_blocked_notice_and_ready_notice_replace_reply_panel_view() {
    let root = temp_dir("reply-notice-overlay");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );

    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw blocked notice");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Send Blocked"));
    assert!(rendered.contains("You must open Send Preview"));
    assert!(!rendered.contains("Headers ([edit] / [read-only])"));
    assert!(!rendered.contains("Reply Body"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw ready notice");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Ready To Send"));
    assert!(rendered.contains("Press S to send the reply"));
    assert!(!rendered.contains("Headers ([edit] / [read-only])"));
    assert!(!rendered.contains("Reply Body"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_failure_keeps_panel_open_and_persists_failure() {
    let root = temp_dir("reply-send-failure");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let runtime = test_runtime_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "inbox",
        1,
        "patch@example.com",
        "[PATCH] demo",
    );
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;
    state.reply_send_executor = reply_send_mock_failure;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );

    assert!(state.status.contains("smtp auth failed"));
    assert!(state.reply_panel.is_some());

    let record = reply_store::latest_reply_send_for_mail(&runtime.database_path, 1)
        .expect("load latest reply send")
        .expect("reply send record");
    assert_eq!(record.status, ReplySendStatus::Failed);
    assert_eq!(record.message_id, "failed@example.com");
    assert_eq!(record.error_summary.as_deref(), Some("smtp auth failed"));

    let draft = reply_store::load_reply_draft(&runtime.database_path, 1, 1)
        .expect("load failed reply draft")
        .expect("failed reply draft should remain in outbox");
    assert_eq!(draft.status, ReplyDraftStatus::Failed);
    assert_eq!(draft.last_error.as_deref(), Some("smtp auth failed"));
    assert_eq!(
        draft.draft_path,
        Some(PathBuf::from("/tmp/reply-failed.eml"))
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_preview_validation_blocks_confirm_on_missing_recipients() {
    let root = temp_dir("reply-preview-validation");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.to.clear();
        panel.cc = "criew@example.com".to_string();
    }

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| !panel.preview_errors.is_empty())
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    assert!(state.status.contains("cannot confirm send preview"));
    assert!(
        !state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_confirmed)
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_preview_warns_but_allows_confirm_without_authored_reply_text() {
    let root = temp_dir("reply-preview-empty-authored");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );

    let panel = state.reply_panel.as_ref().expect("reply panel");
    assert!(panel.preview_open);
    assert!(panel.preview_errors.is_empty());
    assert!(
        panel
            .preview_warnings
            .iter()
            .any(|value| value.contains("no authored reply content"))
    );
    assert!(state.status.contains("send preview warning"));

    let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw warning preview");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Send Preview [warning]"));
    assert!(rendered.contains("draft has no authored reply content"));
    assert!(!rendered.contains("Headers ([edit] / [read-only])"));
    assert!(!rendered.contains("Reply Body"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_confirmed)
    );
    assert_eq!(state.status, "send preview confirmed; ready to send");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_preview_highlights_authored_lines_and_keeps_quotes_bright() {
    let root = temp_dir("reply-preview-highlighted-authored");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.body = vec![
            "Looks good to me.".to_string(),
            String::new(),
            "On Fri, 6 Mar 2026 09:30:00 +0000, Alice wrote:".to_string(),
            "> body line".to_string(),
        ];
        panel.mark_dirty();
    }
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );

    let mut terminal = Terminal::new(TestBackend::new(160, 40)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw highlighted preview");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Send Preview [reply highlighted]"));
    assert!(rendered.contains("Your authored reply lines are highlighted below."));
    assert!(!rendered.contains("Headers ([edit] / [read-only])"));
    assert!(!rendered.contains("Reply Body"));

    let (authored_fg, authored_bg, authored_modifier) =
        rendered_cell_style_for_substring(&terminal, "Looks good to me.")
            .expect("authored line style");
    assert_eq!(authored_fg, Color::Black);
    assert_eq!(authored_bg, Color::Yellow);
    assert!(authored_modifier.contains(Modifier::BOLD));

    let (quoted_fg, quoted_bg, _) =
        rendered_cell_style_for_substring(&terminal, "> body line").expect("quoted line style");
    assert_eq!(quoted_fg, Color::White);
    assert_eq!(quoted_bg, Color::Reset);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_preview_uses_edited_header_values() {
    let root = temp_dir("reply-preview-edited-headers");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nCc: Carol <carol@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.from = "Reviewer Bot <reviewer@example.com>".to_string();
        panel.to = "Maintainer <maintainer@example.com>".to_string();
        panel.cc = "List <list@example.com>".to_string();
        panel.mark_dirty();
    }

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );

    let panel = state
        .reply_panel
        .as_ref()
        .expect("reply panel should stay open");
    assert!(panel.preview_open);
    assert!(panel.preview_errors.is_empty());
    assert!(
        panel
            .preview_rendered
            .contains("From: Reviewer Bot <reviewer@example.com>")
    );
    assert!(
        panel
            .preview_rendered
            .contains("To: Maintainer <maintainer@example.com>")
    );
    assert!(
        panel
            .preview_rendered
            .contains("Cc: List <list@example.com>")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn mail_page_r_opens_reply_panel_from_threads_focus() {
    let root = temp_dir("reply-open-r");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Threads;
    state.reply_identity_resolver = reply_identity_mock;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    assert!(state.reply_panel.is_some());
    assert!(
        state
            .status
            .contains("reply panel opened for <patch@example.com>")
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn compose_shortcut_creates_persisted_independent_draft() {
    let root = temp_dir("compose-shortcut");
    let runtime = test_runtime_in(root.clone());
    fs::create_dir_all(runtime.database_path.parent().expect("compose db parent"))
        .expect("create compose db parent");
    db::initialize(&runtime.database_path).expect("initialize compose db");
    let mut state = AppState::new(vec![], runtime.clone());
    state.reply_identity_resolver = reply_identity_mock;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    let panel = state
        .reply_panel
        .as_ref()
        .expect("compose panel should open");
    assert_eq!(panel.from, "CRIEW Test <criew@example.com>");
    assert!(panel.to.is_empty());
    assert!(panel.subject.is_empty());
    assert!(panel.in_reply_to.is_empty());
    assert!(panel.references.is_empty());
    assert!(panel.thread_id > 0);
    assert!(panel.mail_id > 0);

    let drafts =
        reply_store::list_reply_outbox(&runtime.database_path).expect("list compose outbox");
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].thread_id, panel.thread_id);
    assert_eq!(drafts[0].mail_id, panel.mail_id);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn forward_shortcut_seeds_forwarded_message_and_persists_draft() {
    let root = temp_dir("forward-shortcut");
    let raw = root.join("original.eml");
    fs::write(
        &raw,
        b"Message-ID: <original@example.com>\r\nSubject: Original subject\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\n\r\nforwarded body\r\n",
    )
    .expect("write forward fixture");
    let runtime = test_runtime_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "inbox",
        1,
        "original@example.com",
        "Original subject",
    );
    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "Original subject",
            "original@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );
    state.focus = Pane::Threads;
    state.reply_identity_resolver = reply_identity_mock;

    let action = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
    );

    assert!(matches!(action, LoopAction::Continue));
    let panel = state
        .reply_panel
        .as_ref()
        .expect("forward panel should open");
    assert_eq!(panel.subject, "Fwd: Original subject");
    assert!(panel.to.is_empty());
    assert!(panel.in_reply_to.is_empty());
    assert!(
        panel
            .body
            .iter()
            .any(|line| line == "From: alice@example.com")
    );
    assert!(panel.body.iter().any(|line| line == "forwarded body"));
    assert_eq!(panel.thread_id, 1);
    assert_eq!(panel.mail_id, 1);

    let drafts =
        reply_store::list_reply_outbox(&runtime.database_path).expect("list forward outbox");
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].subject, "Fwd: Original subject");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_panel_restores_persisted_draft_and_outbox_command_lists_it() {
    let root = temp_dir("reply-draft-restore");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");
    let runtime = test_runtime_in(root.clone());
    seed_mailbox_thread(
        &runtime.database_path,
        "inbox",
        1,
        "patch@example.com",
        "[PATCH] demo",
    );
    reply_store::upsert_reply_draft(
        &runtime.database_path,
        &ReplyDraftRequest {
            thread_id: 1,
            mail_id: 1,
            from_addr: "CRIEW Test <criew@example.com>".to_string(),
            to_addrs: "Maintainer <maintainer@example.com>".to_string(),
            cc_addrs: "List <list@example.com>".to_string(),
            subject: "Re: [PATCH] demo".to_string(),
            in_reply_to: "patch@example.com".to_string(),
            references: vec!["patch@example.com".to_string()],
            body: vec!["Persisted reply".to_string(), String::new()],
            preview_confirmed_at: Some("2026-03-07T10:00:00Z".to_string()),
            status: ReplyDraftStatus::Failed,
            draft_path: Some(root.join("reply.eml")),
            last_error: Some("smtp auth failed".to_string()),
        },
    )
    .expect("persist draft");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw,
        )],
        runtime.clone(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let panel = state.reply_panel.as_ref().expect("reply panel should open");
    assert_eq!(panel.to, "Maintainer <maintainer@example.com>");
    assert_eq!(
        panel.body,
        vec!["Persisted reply".to_string(), String::new()]
    );
    assert!(!panel.preview_confirmed);
    assert_eq!(panel.draft_status, ReplyDraftStatus::Failed);

    let draft_id = reply_store::list_reply_outbox(&runtime.database_path)
        .expect("list persisted draft")
        .first()
        .expect("persisted draft entry")
        .id;
    state.reply_panel = None;
    state.palette.open = true;
    state.palette.input = format!("outbox {draft_id}");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.reply_panel.is_some());
    assert!(
        state
            .status
            .contains(&format!("reply draft #{draft_id} restored"))
    );

    state.reply_panel = None;
    state.palette.open = true;
    state.palette.input = "outbox".to_string();
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.status.contains("reply outbox: 1 pending"));
    assert!(state.status.contains("1 failed"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_notice_escape_closes_blocked_notice() {
    let root = temp_dir("reply-notice-esc");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .and_then(|panel| panel.reply_notice.as_ref())
            .is_some()
    );

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.reply_notice.is_none())
    );
    assert_eq!(state.status, "reply notice closed");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_preview_scrolls_with_j_and_k() {
    let root = temp_dir("reply-preview-scroll");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_open)
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert_eq!(
        state
            .reply_panel
            .as_ref()
            .expect("reply panel")
            .preview_scroll,
        1
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    assert_eq!(
        state
            .reply_panel
            .as_ref()
            .expect("reply panel")
            .preview_scroll,
        0
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_send_preview_escape_closes_preview() {
    let root = temp_dir("reply-preview-esc");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| !panel.preview_open)
    );
    assert_eq!(state.status, "send preview closed");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_notice_enter_closes_blocked_notice() {
    let root = temp_dir("reply-notice-enter");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.reply_notice.is_none())
    );
    assert_eq!(state.status, "reply notice closed");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_command_mode_handles_empty_unsupported_and_discard_commands() {
    let root = temp_dir("reply-command-mode");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.status, "empty command");

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    type_text(&mut state, "zzz");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert_eq!(state.status, "unsupported command: :zzz");

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    type_text(&mut state, "q!");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(state.reply_panel.is_none());
    assert_eq!(state.status, "discarded reply draft");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_command_mode_escape_and_backspace_restore_normal_mode() {
    let root = temp_dir("reply-command-cancel");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    type_text(&mut state, "ab");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(
        state
            .reply_panel
            .as_ref()
            .expect("reply panel")
            .command_input,
        "a"
    );

    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    let panel = state.reply_panel.as_ref().expect("reply panel");
    assert!(matches!(panel.mode, ReplyEditMode::Normal));
    assert!(panel.command_input.is_empty());
    assert_eq!(state.status, "reply command cancelled");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_command_q_closes_clean_panel_but_blocks_dirty_draft() {
    let root = temp_dir("reply-command-q");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut clean_state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    clean_state.focus = Pane::Preview;
    clean_state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut clean_state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut clean_state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    type_text(&mut clean_state, "q");
    let _ = handle_key_event(
        &mut clean_state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(clean_state.reply_panel.is_none());
    assert_eq!(clean_state.status, "closed reply panel");

    let mut dirty_state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    dirty_state.focus = Pane::Preview;
    dirty_state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut dirty_state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = dirty_state.reply_panel.as_mut() {
        panel.mark_dirty();
    }
    let _ = handle_key_event(
        &mut dirty_state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    type_text(&mut dirty_state, "q");
    let _ = handle_key_event(
        &mut dirty_state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(dirty_state.reply_panel.is_some());
    assert_eq!(
        dirty_state.status,
        "unsaved reply draft, run :q! to discard"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_command_preview_and_preview_enter_cover_remaining_preview_shortcuts() {
    let root = temp_dir("reply-command-preview");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
    );
    type_text(&mut state, "preview");
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_open)
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );
    assert!(
        state
            .reply_panel
            .as_ref()
            .is_some_and(|panel| panel.preview_confirmed)
    );
    assert_eq!(state.status, "send preview confirmed; ready to send");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_insert_mode_tab_and_backspace_modify_body() {
    let root = temp_dir("reply-insert-tab");
    let raw = root.join("patch.eml");
    fs::write(
        &raw,
        b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
    )
    .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.section = ReplySection::Body;
        panel.body = vec![String::new()];
        panel.body_row = 0;
        panel.cursor_col = 0;
        panel.adjust_scroll();
    }

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(&mut state, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        state.reply_panel.as_ref().expect("reply panel").body[0],
        "    "
    );

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(
        state.reply_panel.as_ref().expect("reply panel").body[0],
        "   "
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_insert_enter_on_quote_line_starts_unquoted_reply_line() {
    let root = temp_dir("reply-quote-enter");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.section = ReplySection::Body;
        panel.body_row = 2;
        panel.cursor_col = panel.body[2].chars().count();
        panel.adjust_scroll();
    }

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    let panel = state
        .reply_panel
        .as_ref()
        .expect("reply panel should stay open");
    assert_eq!(panel.body_row, 3);
    assert_eq!(panel.cursor_col, 0);
    assert_eq!(panel.body[3], "");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_normal_mode_enter_opens_unquoted_reply_line_below_current_line_and_enters_insert() {
    let root = temp_dir("reply-normal-enter");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.section = ReplySection::Body;
        panel.body = vec!["> xxxx".to_string(), "> yyyy".to_string()];
        panel.body_row = 1;
        panel.cursor_col = 0;
        panel.adjust_scroll();
    }

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    );

    let panel = state
        .reply_panel
        .as_ref()
        .expect("reply panel should stay open");
    assert_eq!(panel.body_row, 2);
    assert_eq!(panel.cursor_col, 0);
    assert_eq!(
        panel.body,
        vec!["> xxxx".to_string(), "> yyyy".to_string(), String::new(),]
    );
    assert!(matches!(panel.mode, ReplyEditMode::Insert));
    assert_eq!(state.status, "reply insert mode");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn reply_normal_mode_o_opens_unquoted_reply_line_below_current_line_and_enters_insert() {
    let root = temp_dir("reply-normal-open-below");
    let raw = root.join("patch.eml");
    fs::write(
            &raw,
            b"Message-ID: <patch@example.com>\r\nSubject: [PATCH] demo\r\nFrom: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nDate: Fri, 6 Mar 2026 09:30:00 +0000\r\n\r\nbody line\r\n",
        )
        .expect("write raw reply fixture");

    let mut state = AppState::new(
        vec![sample_thread_with_raw(
            "[PATCH] demo",
            "patch@example.com",
            0,
            raw.clone(),
        )],
        test_runtime(),
    );
    state.focus = Pane::Preview;
    state.reply_identity_resolver = reply_identity_mock;

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
    );
    if let Some(panel) = state.reply_panel.as_mut() {
        panel.section = ReplySection::Body;
        panel.body = vec!["> xxxx".to_string(), "> yyyy".to_string()];
        panel.body_row = 1;
        panel.cursor_col = 0;
        panel.adjust_scroll();
    }

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
    );

    let panel = state
        .reply_panel
        .as_ref()
        .expect("reply panel should stay open");
    assert_eq!(panel.body_row, 2);
    assert_eq!(panel.cursor_col, 0);
    assert_eq!(
        panel.body,
        vec!["> xxxx".to_string(), "> yyyy".to_string(), String::new(),]
    );
    assert!(matches!(panel.mode, ReplyEditMode::Insert));
    assert_eq!(state.status, "reply insert mode");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn preview_redraw_clears_stale_characters_after_thread_switch() {
    let root = temp_dir("preview-clear");
    let first_raw = root.join("first.eml");
    let second_raw = root.join("second.eml");

    fs::write(
            &first_raw,
            b"Message-ID: <first@example.com>\r\nSubject: first\r\nFrom: a@example.com\r\n\r\nSTALE_PREVIEW_TOKEN_123456\nlong line that should disappear\n",
        )
        .expect("write first raw mail");
    fs::write(
            &second_raw,
            b"Message-ID: <second@example.com>\r\nSubject: second\r\nFrom: b@example.com\r\n\r\nshort\n",
        )
        .expect("write second raw mail");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![
            sample_thread_with_raw("first", "first@example.com", 0, first_raw.clone()),
            sample_thread_with_raw("second", "second@example.com", 0, second_raw.clone()),
        ],
        runtime.clone(),
    );
    state.focus = Pane::Threads;

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw first frame");
    let first_frame = format!("{}", terminal.backend());
    assert!(first_frame.contains("STALE_PREVIEW_TOKEN_123456"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw second frame");
    let second_frame = format!("{}", terminal.backend());
    assert!(!second_frame.contains("STALE_PREVIEW_TOKEN_123456"));
    assert!(second_frame.contains("short"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn preview_redraw_uses_cached_mail_body_after_raw_file_is_removed() {
    let root = temp_dir("preview-cache");
    let raw_path = root.join("cached.eml");
    fs::write(
        &raw_path,
        b"Message-ID: <cached@example.com>\r\nSubject: cached\r\nFrom: cache@example.com\r\n\r\ncached body line\n",
    )
    .expect("write raw mail");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let state = AppState::new(
        vec![sample_thread_with_raw(
            "cached",
            "cached@example.com",
            0,
            raw_path.clone(),
        )],
        runtime.clone(),
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw first frame");
    let first_frame = format!("{}", terminal.backend());
    assert!(first_frame.contains("cached body line"));

    fs::remove_file(&raw_path).expect("remove raw mail");

    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw second frame");
    let second_frame = format!("{}", terminal.backend());
    assert!(second_frame.contains("cached body line"));
    assert!(!second_frame.contains("failed to read"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn code_source_preview_redraw_clears_stale_characters_after_file_switch() {
    let root = temp_dir("code-preview-clear");
    let file_a = root.join("a-long.rs");
    let file_b = root.join("b-short.rs");
    fs::write(
        &file_a,
        "fn demo() {\n    let _x = \"STALE_SOURCE_TOKEN_987654\";\n}\n",
    )
    .expect("write file a");
    fs::write(&file_b, "fn demo() {}\n").expect("write file b");

    let runtime = test_runtime_with_kernel_tree(root.clone());
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(vec![], runtime.clone());
    state.ui_page = UiPage::CodeBrowser;
    state.code_focus = CodePaneFocus::Tree;

    let index_a = state
        .kernel_tree_rows
        .iter()
        .position(|row| row.path == file_a)
        .expect("find file a");
    state.kernel_tree_row_index = index_a;

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw first frame");
    let first_frame = format!("{}", terminal.backend());
    assert!(first_frame.contains("STALE_SOURCE_TOKEN_987654"));

    let _ = handle_key_event(
        &mut state,
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
    );
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw second frame");
    let second_frame = format!("{}", terminal.backend());
    assert!(!second_frame.contains("STALE_SOURCE_TOKEN_987654"));
    assert!(second_frame.contains("fn demo() {}"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn preview_render_preserves_code_indentation() {
    let root = temp_dir("preview-indent");
    let raw = root.join("indent.eml");

    fs::write(
            &raw,
            b"Message-ID: <indent@example.com>\r\nSubject: indent\r\nFrom: a@example.com\r\n\r\nfn demo() {\n\tif true {\n        return;\n\t}\n}\n",
        )
        .expect("write raw mail");

    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let state = AppState::new(
        vec![sample_thread_with_raw(
            "indent",
            "indent@example.com",
            0,
            raw.clone(),
        )],
        runtime.clone(),
    );

    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw frame");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("    if true {"));
    assert!(rendered.contains("        return;"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn threads_panel_renders_thread_group_headers() {
    let runtime = test_runtime();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![
            sample_thread_in_thread(100, 1, "thread a root", "a-root@example.com", 0),
            sample_thread_in_thread(100, 2, "thread a reply", "a-reply@example.com", 1),
            sample_thread_in_thread(200, 3, "thread b root", "b-root@example.com", 0),
        ],
        runtime.clone(),
    );
    state.focus = Pane::Threads;

    let mut terminal = Terminal::new(TestBackend::new(180, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw frame");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("Thread 100 (2 msgs)"));
    assert!(rendered.contains("Thread 200 (1 msg)"));
    assert!(rendered.contains("thread a root"));
    assert!(rendered.contains("thread b root"));
}

#[test]
fn following_thread_group_headers_render_recipient_badges() {
    let runtime = test_runtime_with_imap();
    let bootstrap = test_bootstrap(&runtime);
    let mut state = AppState::new(
        vec![sample_thread_in_thread(
            100,
            1,
            "thread a root",
            "a-root@example.com",
            0,
        )],
        runtime.clone(),
    );
    state.active_thread_mailbox = IMAP_INBOX_MAILBOX.to_string();
    state.following_kinds.insert(
        100,
        vec![
            crate::domain::following::FollowingKind::ToMe,
            crate::domain::following::FollowingKind::CcMe,
            crate::domain::following::FollowingKind::SentPatch,
        ],
    );
    state.focus = Pane::Threads;

    let mut terminal = Terminal::new(TestBackend::new(180, 30)).expect("create test terminal");
    terminal
        .draw(|frame| draw(frame, &state, &runtime, &bootstrap))
        .expect("draw Following frame");
    let rendered = format!("{}", terminal.backend());
    assert!(rendered.contains("follow=TO,CC,SENT"));
}
