//! Key-event routing for the TUI state machine.
//!
//! Modal surfaces such as the palette, reply editor, and config editor all
//! share the same event stream. Centralizing dispatch order here prevents
//! conflicting shortcuts from being interpreted by multiple layers at once.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::config::run_palette_config;
use super::palette::{
    apply_palette_completion, is_palette_open_shortcut, is_palette_toggle,
    run_palette_local_command, run_palette_sync,
};
use super::*;

pub(super) enum LoopAction {
    Continue,
    Exit,
    Restart,
}

pub(super) fn pending_main_page_move_count(state: &mut AppState) -> u16 {
    state.take_pending_main_page_count().unwrap_or(1)
}

fn handle_main_page_count_prefix(state: &mut AppState, key: KeyEvent) -> bool {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        state.clear_pending_main_page_count();
        return false;
    }

    let KeyCode::Char(character) = key.code else {
        return false;
    };
    if !character.is_ascii_digit() {
        return false;
    }
    if character == '0' && !state.has_pending_main_page_count() {
        return false;
    }

    let digit = character
        .to_digit(10)
        .expect("ascii digit should convert to decimal") as u16;
    state.push_pending_main_page_count_digit(digit);
    true
}

pub(super) fn handle_key_event(state: &mut AppState, key: KeyEvent) -> LoopAction {
    tracing::debug!(
        key = ?key,
        ui_page = ?state.ui_page,
        focus = ?state.focus,
        code_focus = ?state.code_focus,
        code_edit_mode = ?state.code_edit_mode,
        config_editor_open = state.config_editor.open,
        keymap_editor_open = state.keymap_editor.open,
        palette_open = state.palette.open,
        search_active = state.search.active,
        "user key event"
    );

    // Modal UI surfaces take precedence over the base page shortcuts so keys
    // keep local meaning while a dialog, editor, or search interaction is open.
    if state.config_editor.open {
        state.clear_pending_main_page_inputs();
        return handle_config_editor_key_event(state, key);
    }

    if state.keymap_editor.open {
        state.clear_pending_main_page_inputs();
        return handle_keymap_editor_key_event(state, key);
    }

    if state.palette.open {
        state.clear_pending_main_page_inputs();
        if is_palette_toggle(key) {
            state.close_palette();
            return LoopAction::Continue;
        }
        return handle_palette_key_event(state, key);
    }

    if state.search.active {
        state.clear_pending_main_page_inputs();
        return handle_search_key_event(state, key);
    }

    if state.reply_panel.is_some() {
        state.clear_pending_main_page_inputs();
        let action = handle_reply_key_event(state, key);
        state.persist_reply_draft();
        return action;
    }

    if state.is_code_edit_active() {
        state.clear_pending_main_page_inputs();
        return handle_code_edit_key_event(state, key);
    }

    if is_palette_open_shortcut(key) {
        state.clear_pending_main_page_inputs();
        state.toggle_palette();
        return LoopAction::Continue;
    }

    if let Some(action) = handle_main_page_key_event(state, key) {
        return action;
    }

    if handle_main_page_count_prefix(state, key) {
        return LoopAction::Continue;
    }

    state.clear_pending_main_page_count();

    match key.code {
        KeyCode::Char('/') => {
            if matches!(state.ui_page, UiPage::Mail) {
                state.open_search();
            } else {
                state.status = "search is only available on mail page".to_string();
            }
        }
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail)
                && matches!(state.focus, Pane::Subscriptions)
                && character.eq_ignore_ascii_case(&'y') =>
        {
            state.set_current_subscription_enabled(true);
        }
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail)
                && matches!(state.focus, Pane::Subscriptions)
                && character.eq_ignore_ascii_case(&'n') =>
        {
            state.set_current_subscription_enabled(false);
        }
        KeyCode::Char('-')
            if matches!(state.ui_page, UiPage::Mail) && matches!(state.focus, Pane::Preview) =>
        {
            state.select_previous_thread();
        }
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail)
                && matches!(state.focus, Pane::Preview)
                && matches!(character, '=' | '+') =>
        {
            state.select_next_thread();
        }
        KeyCode::Char('e')
            if matches!(state.ui_page, UiPage::Mail) && matches!(state.focus, Pane::Preview) =>
        {
            state.open_reply_panel(true);
        }
        KeyCode::Char('C') if matches!(state.ui_page, UiPage::Mail) => {
            state.open_compose_panel();
        }
        KeyCode::Char('f')
            if matches!(state.ui_page, UiPage::Mail) && matches!(state.focus, Pane::Threads) =>
        {
            state.open_forward_panel();
        }
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail) && character.eq_ignore_ascii_case(&'r') =>
        {
            state.open_reply_panel(false);
        }
        KeyCode::Char('[') if matches!(state.ui_page, UiPage::Mail) => {
            state.resize_mail_panes(HorizontalResizeDirection::Left, MailPaneResizeMode::Expand);
        }
        KeyCode::Char(']') if matches!(state.ui_page, UiPage::Mail) => {
            state.resize_mail_panes(HorizontalResizeDirection::Right, MailPaneResizeMode::Expand);
        }
        KeyCode::Char('{') if matches!(state.ui_page, UiPage::Mail) => {
            state.resize_mail_panes(HorizontalResizeDirection::Left, MailPaneResizeMode::Shrink);
        }
        KeyCode::Char('}') if matches!(state.ui_page, UiPage::Mail) => {
            state.resize_mail_panes(HorizontalResizeDirection::Right, MailPaneResizeMode::Shrink);
        }
        KeyCode::Char('e') if matches!(state.ui_page, UiPage::CodeBrowser) => {
            state.enter_code_edit_mode();
        }
        KeyCode::Char('E') if matches!(state.ui_page, UiPage::CodeBrowser) => {
            state.open_external_editor();
        }
        KeyCode::Tab => state.toggle_ui_page(),
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail)
                && matches!(state.focus, Pane::Threads)
                && character.eq_ignore_ascii_case(&'a') =>
        {
            state.run_patch_action(patch_worker::PatchAction::Apply);
        }
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail)
                && matches!(state.focus, Pane::Threads)
                && character.eq_ignore_ascii_case(&'d') =>
        {
            state.run_patch_action(patch_worker::PatchAction::Download);
        }
        KeyCode::Char('P')
            if matches!(state.ui_page, UiPage::Mail) && matches!(state.focus, Pane::Threads) =>
        {
            state.run_patch_preflight();
        }
        KeyCode::Char('F')
            if matches!(state.ui_page, UiPage::Mail) && matches!(state.focus, Pane::Threads) =>
        {
            if let Some(message_id) = state
                .selected_thread()
                .map(|thread| thread.message_id.clone())
            {
                state.start_thread_fetch(message_id);
            } else {
                state.status = "select a thread before fetching its complete thread".to_string();
            }
        }
        KeyCode::Char(character)
            if matches!(state.ui_page, UiPage::Mail)
                && matches!(state.focus, Pane::Threads)
                && character.eq_ignore_ascii_case(&'u') =>
        {
            state.run_patch_undo_action();
        }
        KeyCode::Enter => match state.ui_page {
            UiPage::Mail => match state.focus {
                Pane::Subscriptions => state.handle_subscription_enter(),
                Pane::Threads => {
                    if let Some(thread) = state.selected_thread() {
                        state.status = format!("selected {}", thread.message_id);
                        state.focus = Pane::Preview;
                    }
                }
                Pane::Preview => {}
            },
            UiPage::CodeBrowser => {
                if matches!(state.code_focus, CodePaneFocus::Tree) {
                    state.handle_kernel_tree_enter();
                }
            }
        },
        KeyCode::Esc => {
            state.status = "open command palette with : (preferred) or Ctrl+`".to_string();
        }
        KeyCode::Char('q') => {
            state.status = "q emergency exit disabled; use command palette quit/exit".to_string();
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.status = "Ctrl+C is disabled, use command palette quit/exit".to_string();
        }
        _ => {}
    }

    LoopAction::Continue
}

fn handle_config_editor_key_event(state: &mut AppState, key: KeyEvent) -> LoopAction {
    match state.config_editor.mode {
        ConfigEditorMode::Browse => match key.code {
            KeyCode::Esc => state.close_config_editor(),
            KeyCode::Up | KeyCode::Char('i') => state.move_config_editor_up(),
            KeyCode::Down | KeyCode::Char('k') => state.move_config_editor_down(),
            KeyCode::Enter | KeyCode::Char('e') => state.start_config_editor_edit(),
            KeyCode::Tab => state.cycle_config_editor_value(),
            KeyCode::Char('x') => state.unset_selected_config_key(),
            _ => {}
        },
        ConfigEditorMode::Edit => match key.code {
            KeyCode::Esc => {
                state.config_editor.mode = ConfigEditorMode::Browse;
                state.config_editor.input.clear();
                state.status = "config edit cancelled".to_string();
            }
            KeyCode::Enter => state.save_config_editor_value(),
            KeyCode::Tab => state.cycle_config_editor_value(),
            KeyCode::Backspace => {
                state.config_editor.input.pop();
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                state.config_editor.input.push(character);
            }
            _ => {}
        },
    }

    LoopAction::Continue
}

fn handle_code_edit_key_event(state: &mut AppState, key: KeyEvent) -> LoopAction {
    match state.code_edit_mode {
        CodeEditMode::Browse => {}
        CodeEditMode::VimNormal => match key.code {
            KeyCode::Char('h') => state.move_code_edit_cursor_left(),
            KeyCode::Char('j') => state.move_code_edit_cursor_down(),
            KeyCode::Char('k') => state.move_code_edit_cursor_up(),
            KeyCode::Char('l') => state.move_code_edit_cursor_right(),
            KeyCode::Char('i') => {
                state.code_edit_mode = CodeEditMode::VimInsert;
                state.status = "insert mode".to_string();
            }
            KeyCode::Char('x') => {
                if !state.delete_code_edit_character() {
                    state.status = "nothing to delete".to_string();
                }
            }
            KeyCode::Char('s') => {
                let _ = state.save_code_edit_buffer();
            }
            KeyCode::Char('E') => {
                state.open_external_editor();
            }
            KeyCode::Char(':')
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                state.enter_code_edit_command_mode();
            }
            KeyCode::Esc => {
                if state.code_edit_dirty {
                    state.status = "unsaved changes, run :w, :wq, or :q!".to_string();
                } else {
                    let target = state
                        .code_edit_target
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<file>".to_string());
                    state.exit_code_edit_mode(format!("exit edit mode for {target}"));
                }
            }
            _ => {}
        },
        CodeEditMode::VimInsert => match key.code {
            KeyCode::Esc => {
                state.code_edit_mode = CodeEditMode::VimNormal;
                state.status = "normal mode".to_string();
            }
            KeyCode::Backspace => {
                if !state.backspace_code_edit_character() {
                    state.status = "nothing to delete".to_string();
                }
            }
            KeyCode::Enter => {
                let _ = state.insert_code_edit_newline();
            }
            KeyCode::Tab => {
                for character in PREVIEW_TAB_SPACES.chars() {
                    let _ = state.insert_code_edit_character(character);
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                let _ = state.insert_code_edit_character(character);
            }
            _ => {}
        },
        CodeEditMode::VimCommand => match key.code {
            KeyCode::Esc => {
                state.code_edit_command_input.clear();
                state.code_edit_mode = CodeEditMode::VimNormal;
                state.status = "command cancelled".to_string();
            }
            KeyCode::Backspace => {
                state.code_edit_command_input.pop();
            }
            KeyCode::Enter => {
                state.execute_code_edit_command();
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                state.code_edit_command_input.push(character);
            }
            _ => {}
        },
    }

    LoopAction::Continue
}

fn handle_reply_key_event(state: &mut AppState, key: KeyEvent) -> LoopAction {
    if state
        .reply_panel
        .as_ref()
        .is_some_and(|panel| panel.preview_open)
    {
        match key.code {
            KeyCode::Esc => state.close_send_preview("send preview closed"),
            KeyCode::Enter => state.confirm_send_preview(),
            KeyCode::Char(character) if character.eq_ignore_ascii_case(&'c') => {
                state.confirm_send_preview()
            }
            KeyCode::Char(character) if character.eq_ignore_ascii_case(&'s') => {
                state.attempt_reply_send()
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.preview_scroll = panel.preview_scroll.saturating_add(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.preview_scroll = panel.preview_scroll.saturating_sub(1);
                }
            }
            _ => {}
        }
        return LoopAction::Continue;
    }

    if let Some(notice_action) = state
        .reply_panel
        .as_ref()
        .and_then(|panel| panel.reply_notice.as_ref().and_then(|notice| notice.action))
    {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                state.close_reply_notice("reply notice closed");
            }
            KeyCode::Char(character)
                if character.eq_ignore_ascii_case(&'p')
                    && matches!(notice_action, super::ReplyNoticeAction::OpenPreview) =>
            {
                state.close_reply_notice("opening send preview");
                state.open_send_preview();
            }
            KeyCode::Char(character)
                if character.eq_ignore_ascii_case(&'s')
                    && matches!(notice_action, super::ReplyNoticeAction::Send) =>
            {
                state.close_reply_notice("sending reply");
                state.attempt_reply_send();
            }
            _ => {
                state.close_reply_notice("reply notice closed");
            }
        }
        return LoopAction::Continue;
    }

    let Some(mode) = state.reply_panel.as_ref().map(|panel| panel.mode) else {
        return LoopAction::Continue;
    };

    match mode {
        ReplyEditMode::Normal => match key.code {
            KeyCode::Char('h') => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.move_left();
                }
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.open_line_below();
                    panel.mode = ReplyEditMode::Insert;
                }
                state.status = "reply insert mode".to_string();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.move_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.move_up();
                }
            }
            KeyCode::Char('l') => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.move_right();
                }
            }
            KeyCode::Char('i') => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.mode = ReplyEditMode::Insert;
                }
                state.status = "reply insert mode".to_string();
            }
            KeyCode::Char('x') => {
                let deleted = state
                    .reply_panel
                    .as_mut()
                    .map(|panel| panel.delete_char())
                    .unwrap_or(false);
                if !deleted {
                    state.status = "nothing to delete".to_string();
                }
            }
            KeyCode::Char('p') => state.open_send_preview(),
            KeyCode::Char(character) if character.eq_ignore_ascii_case(&'s') => {
                state.attempt_reply_send()
            }
            KeyCode::Char(':')
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.mode = ReplyEditMode::Command;
                    panel.command_input.clear();
                    panel.adjust_scroll();
                }
                state.status = "reply command mode".to_string();
            }
            KeyCode::Esc => {
                if state.reply_panel.as_ref().is_some_and(|panel| panel.dirty) {
                    state.status = "unsaved reply draft, run :q! to discard".to_string();
                } else {
                    state.close_reply_panel("closed reply panel");
                }
            }
            _ => {}
        },
        ReplyEditMode::Insert => match key.code {
            KeyCode::Esc => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.mode = ReplyEditMode::Normal;
                    panel.adjust_scroll();
                }
                state.status = "reply normal mode".to_string();
            }
            KeyCode::Backspace => {
                let deleted = state
                    .reply_panel
                    .as_mut()
                    .map(|panel| panel.backspace())
                    .unwrap_or(false);
                if !deleted {
                    state.status = "nothing to delete".to_string();
                }
            }
            KeyCode::Enter => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.insert_newline();
                }
            }
            KeyCode::Tab => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    for character in PREVIEW_TAB_SPACES.chars() {
                        panel.insert_char(character);
                    }
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.insert_char(character);
                }
            }
            _ => {}
        },
        ReplyEditMode::Command => match key.code {
            KeyCode::Esc => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.command_input.clear();
                    panel.mode = ReplyEditMode::Normal;
                    panel.adjust_scroll();
                }
                state.status = "reply command cancelled".to_string();
            }
            KeyCode::Backspace => {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.command_input.pop();
                }
            }
            KeyCode::Enter => state.execute_reply_command(),
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                if let Some(panel) = state.reply_panel.as_mut() {
                    panel.command_input.push(character);
                }
            }
            _ => {}
        },
    }

    LoopAction::Continue
}

fn handle_search_key_event(state: &mut AppState, key: KeyEvent) -> LoopAction {
    match key.code {
        KeyCode::Esc => state.close_search(),
        KeyCode::Enter => state.apply_search(),
        KeyCode::Backspace => {
            state.search.input.pop();
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            state.search.input.push(character);
        }
        _ => {}
    }

    LoopAction::Continue
}

fn handle_palette_key_event(state: &mut AppState, key: KeyEvent) -> LoopAction {
    match key.code {
        KeyCode::Esc => {
            state.close_palette();
        }
        KeyCode::Enter => {
            let raw_command = state.palette.input.trim().to_string();
            tracing::debug!(command = %raw_command, "user submitted command palette input");
            state.palette.input.clear();
            state.palette.clear_completion();

            if raw_command.is_empty() {
                state.status = "empty command".to_string();
                return LoopAction::Continue;
            }

            if let Some(local_command) = raw_command.strip_prefix('!') {
                run_palette_local_command(state, local_command);
                return LoopAction::Continue;
            }

            state.palette.clear_local_result();
            let command = raw_command.to_ascii_lowercase();
            match command.as_str() {
                "quit" | "exit" => return LoopAction::Exit,
                "restart" => return LoopAction::Restart,
                "help" => {
                    state.status = format!(
                        "commands: quit, exit, restart, help, sync [mailbox], fetch-thread [--mailbox NAME] MESSAGE_ID, review-inbox [needs-review|reviewed|all|off], preflight, compose, forward, outbox [DRAFT_ID], view [list|save NAME|use NAME|delete NAME], config ..., keymap, vim, !<local shell command> | keys: {} focus, {} move, [ ] expand pane, {{ }} shrink pane, -/= preview switch, y/n enable, a apply, d download, P preflight, F fetch thread, C compose, f forward, u undo apply, e reply/inline edit, r reply, E external vim",
                        main_page_focus_shortcuts(&state.main_page_keymap),
                        main_page_move_shortcuts(&state.main_page_keymap)
                    );
                }
                value if value.split_whitespace().next() == Some("sync") => {
                    run_palette_sync(state, value);
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("fetch-thread") => {
                    state.queue_palette_thread_fetch(&raw_command);
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("review-inbox") => {
                    state.queue_palette_review_inbox(&raw_command);
                    state.dismiss_palette();
                }
                "preflight" => {
                    state.run_patch_preflight();
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("compose") => {
                    if value.split_whitespace().count() > 1 {
                        state.status = "usage: compose".to_string();
                    } else {
                        state.open_compose_panel();
                    }
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("forward") => {
                    if value.split_whitespace().count() > 1 {
                        state.status = "usage: forward".to_string();
                    } else {
                        state.open_forward_panel();
                    }
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("outbox") => {
                    state.handle_palette_outbox(&raw_command);
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("view") => {
                    state.handle_palette_view(&raw_command);
                    state.dismiss_palette();
                }
                value if value.split_whitespace().next() == Some("config") => {
                    run_palette_config(state, value);
                }
                "keymap" => {
                    state.open_keymap_editor();
                }
                "vim" => {
                    state.open_external_editor();
                }
                _ => {
                    state.status = format!("unknown command: {command}");
                }
            }
        }
        KeyCode::Backspace => {
            state.palette.input.pop();
            state.palette.clear_completion();
            if !state.palette.input.trim_start().starts_with('!') {
                state.palette.clear_local_result();
            }
        }
        KeyCode::Tab => {
            apply_palette_completion(state);
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            state.palette.input.push(character);
            state.palette.clear_completion();
            if !state.palette.input.trim_start().starts_with('!') {
                state.palette.clear_local_result();
            }
        }
        _ => {}
    }

    LoopAction::Continue
}
