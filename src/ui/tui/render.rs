//! Frame rendering for the ratatui interface.
//!
//! This module turns coarse-grained app state into widgets. Keeping rendering
//! separate from input/state mutation preserves the top-down readability of the
//! main TUI loop.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::config::draw_config_editor;
use super::palette::palette_overlay_suggestions;
use super::reply::ReplyPreviewLineKind;
use super::*;

const REPLY_BODY_GUIDE_COLUMN: usize = 80;
const HEADER_BG: Color = Color::Blue;

#[derive(Clone, Copy)]
enum VerticalScrollWrapMode {
    Disabled,
    Enabled,
}

pub(super) fn draw(
    frame: &mut Frame<'_>,
    state: &AppState,
    config: &RuntimeConfig,
    _bootstrap: &BootstrapState,
) {
    // Keep header, body, and footer in fixed bands so transient overlays do
    // not cause the main navigation chrome to jump between frames.
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let uptime_label = format_uptime_label(state.started_at.elapsed().as_secs());
    let sync_progress_text = state
        .background_sync_progress_text()
        .map(|value| sanitize_inline_ui_text(&value));
    let header_sections = if let Some(progress_text) = sync_progress_text.as_ref() {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(progress_text.chars().count().min(64) as u16 + 1),
            ])
            .split(areas[0])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1)])
            .split(areas[0])
    };
    let header = vec![
        Span::styled(
            " CRIEW ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(Color::Yellow)
                .bg(HEADER_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            header_context_text(state),
            Style::default()
                .fg(Color::White)
                .bg(HEADER_BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("{} threads", state.filtered_thread_indices.len()),
            Style::default()
                .fg(Color::White)
                .bg(HEADER_BG)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(" | ", Style::default().fg(Color::White).bg(HEADER_BG)),
        Span::styled(
            format!("keymap {}", state.runtime.ui_keymap.as_str()),
            Style::default()
                .fg(Color::White)
                .bg(HEADER_BG)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(" | ", Style::default().fg(Color::White).bg(HEADER_BG)),
        Span::styled(
            format!("up {uptime_label}"),
            Style::default()
                .fg(Color::White)
                .bg(HEADER_BG)
                .add_modifier(Modifier::DIM),
        ),
    ];
    let header_background = Paragraph::new("").style(Style::default().bg(HEADER_BG));
    frame.render_widget(header_background, areas[0]);

    let header_widget = Paragraph::new(Line::from(header)).style(Style::default().bg(HEADER_BG));
    frame.render_widget(header_widget, header_sections[0]);

    if let Some(progress_text) = sync_progress_text {
        let progress = Paragraph::new(format!("{progress_text} "))
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::Yellow).bg(HEADER_BG));
        frame.render_widget(progress, header_sections[1]);
    }

    match state.ui_page {
        UiPage::Mail => {
            let panes = mail_page_panes(areas[1], state.mail_pane_layout);
            draw_subscriptions(frame, panes[0], state);
            draw_threads(frame, panes[1], state);
            draw_preview(frame, panes[2], state, config);
        }
        UiPage::CodeBrowser => {
            draw_code_browser_page(frame, areas[1], state);
        }
    }

    let shortcuts_text = match state.ui_page {
        UiPage::Mail if state.reply_panel.is_some() => {
            if state
                .reply_panel
                .as_ref()
                .is_some_and(|panel| panel.preview_open)
            {
                "j/k scroll preview | Enter/c confirm | Esc close | S send".to_string()
            } else if state
                .reply_panel
                .as_ref()
                .is_some_and(|panel| panel.reply_notice.is_some())
            {
                "Enter/Esc close notice | P preview | S send".to_string()
            } else {
                "Esc normal/close | Enter/o open below+insert | h/j/k/l move | i insert | x delete | p send preview | S send | :preview :send :q :q!".to_string()
            }
        }
        UiPage::Mail if state.palette.open => {
            "/ search | Tab page | : palette | Enter | e/r reply".to_string()
        }
        UiPage::Mail => format!(
            "/ search | Tab page | : palette | Enter | e/r reply | [ ] expand pane | {{ }} shrink pane | {}",
            main_page_navigation_shortcuts(&state.main_page_keymap)
        ),
        UiPage::CodeBrowser if state.is_code_edit_active() => {
            "Esc normal/exit | h/j/k/l move | i insert | x delete | s save | E external vim | :w :q :q! :wq :vim".to_string()
        }
        UiPage::CodeBrowser if state.palette.open => {
            "Tab page | : palette | Enter expand/collapse | e inline edit | E external vim"
                .to_string()
        }
        UiPage::CodeBrowser => format!(
            "Tab page | : palette | Enter expand/collapse | e inline edit | E external vim | {}",
            main_page_navigation_shortcuts(&state.main_page_keymap)
        ),
    };
    let footer_background =
        Paragraph::new("").style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(footer_background, areas[2]);

    let footer_status_text = footer_status_text(&state.status);
    let footer_sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(shortcuts_text.chars().count() as u16),
            Constraint::Min(1),
        ])
        .split(areas[2]);

    let shortcuts =
        Paragraph::new(shortcuts_text).style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(shortcuts, footer_sections[0]);

    if let Some(status_line) = footer_status_text {
        let status = Paragraph::new(status_line)
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::White).bg(Color::DarkGray));
        frame.render_widget(status, footer_sections[1]);
    }

    if state.palette.open {
        draw_command_palette(frame, state);
    }
    if state.search.active {
        draw_search_overlay(frame, state);
    }
    if state.config_editor.open {
        draw_config_editor(frame, state);
    }
    if state.keymap_editor.open {
        draw_keymap_editor(frame, state);
    }
    if state.reply_panel.is_some() {
        draw_reply_panel(frame, state);
    }
}

fn format_uptime_label(uptime_secs: u64) -> String {
    let hours = uptime_secs / 3_600;
    let minutes = (uptime_secs % 3_600) / 60;
    let seconds = uptime_secs % 60;

    if hours > 0 {
        return format!("{hours:02}h:{minutes:02}m:{seconds:02}s");
    }
    if minutes > 0 {
        return format!("{minutes:02}m:{seconds:02}s");
    }
    format!("{seconds}s")
}

fn footer_status_text(status: &str) -> Option<String> {
    let sanitized = sanitize_inline_ui_text(status);
    if sanitized.trim().is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn header_context_text(state: &AppState) -> String {
    let page_label = match state.ui_page {
        UiPage::Mail => "Mail",
        UiPage::CodeBrowser => "Code",
    };
    let context = format!("{page_label} / {}", state.active_thread_mailbox);
    state
        .review_inbox_mode
        .map(|mode| {
            format!(
                "{context} / Review Inbox {}",
                review_worker::mode_label(mode)
            )
        })
        .unwrap_or(context)
}

fn draw_code_browser_page(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    draw_kernel_tree(frame, panes[0], state);
    draw_code_source_preview(frame, panes[1], state);
}

pub(super) fn mail_page_panes(area: Rect, layout: MailPaneLayout) -> [Rect; 3] {
    if area.width == 0 {
        return [area, area, area];
    }

    let preview_width = area.width.min(layout.preview_width);
    let left_width = area.width.saturating_sub(preview_width);
    let preview = Rect {
        x: area.x + left_width,
        y: area.y,
        width: preview_width,
        height: area.height,
    };

    if left_width == 0 {
        let empty = Rect {
            x: area.x,
            y: area.y,
            width: 0,
            height: area.height,
        };
        return [empty, empty, preview];
    }

    let subscriptions_width = left_width.min(layout.subscriptions_width);
    let threads_width = left_width.saturating_sub(subscriptions_width);
    let subscriptions = Rect {
        x: area.x,
        y: area.y,
        width: subscriptions_width,
        height: area.height,
    };
    let threads = Rect {
        x: area.x + subscriptions_width,
        y: area.y,
        width: threads_width,
        height: area.height,
    };

    [subscriptions, threads, preview]
}

fn draw_subscriptions(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let rows = state.subscription_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| ListItem::new(row.text.clone()))
        .collect();
    let selected_row = if rows.is_empty() {
        None
    } else {
        Some(
            state
                .subscription_row_index
                .min(rows.len().saturating_sub(1)),
        )
    };

    let mut list_state = ListState::default();
    list_state.select(selected_row);

    let list = List::new(items)
        .block(panel_block(Pane::Subscriptions, state.focus))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_kernel_tree(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    frame.render_widget(Clear, area);
    let block = panel_block_with_title("Kernel Tree", state.code_focus == CodePaneFocus::Tree);
    let items: Vec<ListItem> = if state.kernel_tree_rows.is_empty() {
        vec![ListItem::new(
            "<no files found under configured kernel trees>",
        )]
    } else {
        state
            .kernel_tree_rows
            .iter()
            .map(|row| ListItem::new(row.display_text()))
            .collect()
    };

    let selected_row = if state.kernel_tree_rows.is_empty() {
        None
    } else {
        Some(
            state
                .kernel_tree_row_index
                .min(state.kernel_tree_rows.len().saturating_sub(1)),
        )
    };

    let mut list_state = ListState::default();
    list_state.select(selected_row);

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_code_source_preview(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    frame.render_widget(Clear, area);
    let title = if state.is_code_edit_active() {
        let dirty = if state.code_edit_dirty { "*" } else { "-" };
        format!(
            "Source Preview [{} dirty:{}]",
            state.code_edit_mode.label(),
            dirty
        )
    } else {
        "Source Preview".to_string()
    };
    let block = panel_block_with_title(&title, state.code_focus == CodePaneFocus::Source);
    let inner_area = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Clear, inner_area);

    let preview = load_code_source_preview(state);
    let code_preview_scroll_limit = max_vertical_scroll(
        &preview,
        inner_area.width,
        inner_area.height,
        VerticalScrollWrapMode::Disabled,
    );
    state
        .code_preview_scroll_limit
        .set(code_preview_scroll_limit);
    let scroll = clamp_vertical_scroll(
        &preview,
        inner_area.width,
        inner_area.height,
        state.code_preview_scroll,
        VerticalScrollWrapMode::Disabled,
    );
    let paragraph = Paragraph::new(preview).scroll((scroll, 0));
    frame.render_widget(paragraph, inner_area);

    if let Some(cursor_position) = code_edit_cursor_position(state, inner_area) {
        frame.set_cursor_position(cursor_position);
    }
}

pub(super) fn subscription_line(
    item: &SubscriptionItem,
    startup_sync_status: Option<StartupSyncMailboxStatus>,
) -> String {
    let marker = if item.enabled { "y" } else { "n" };
    let suffix = startup_sync_status
        .map(StartupSyncMailboxStatus::ui_suffix)
        .unwrap_or("");
    format!("[{marker}] {}{suffix}", item.label)
}

fn draw_threads(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let max_thread_line_chars = thread_line_max_chars(area);
    let mut visible_count_by_thread: HashMap<i64, usize> = HashMap::new();
    for index in &state.filtered_thread_indices {
        if let Some(row) = state.threads.get(*index) {
            *visible_count_by_thread.entry(row.thread_id).or_insert(0) += 1;
        }
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected = None;
    let mut previous_thread_id: Option<i64> = None;
    for (position, index) in state.filtered_thread_indices.iter().enumerate() {
        let Some(row) = state.threads.get(*index) else {
            continue;
        };

        if previous_thread_id != Some(row.thread_id) {
            previous_thread_id = Some(row.thread_id);
            let visible_count = visible_count_by_thread
                .get(&row.thread_id)
                .copied()
                .unwrap_or(1);
            items.push(
                ListItem::new(thread_group_line(
                    row.thread_id,
                    visible_count,
                    state.series_summaries.get(&row.thread_id),
                    state.review_summaries.get(&row.thread_id),
                ))
                .style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
            );
        }

        if position == state.thread_index {
            selected = Some(items.len());
        }
        items.push(ListItem::new(thread_line(row, max_thread_line_chars)));
    }

    let mut list_state = ListState::default();
    list_state.select(selected);

    let list = List::new(items)
        .block(panel_block(Pane::Threads, state.focus))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(list, area, &mut list_state);
}

fn thread_group_line(
    thread_id: i64,
    visible_count: usize,
    series: Option<&patch_worker::SeriesSummary>,
    review: Option<&review_worker::ReviewInboxEntry>,
) -> String {
    let noun = if visible_count == 1 { "msg" } else { "msgs" };
    let mut line = format!("Thread {thread_id} ({visible_count} {noun})");
    if let Some(series) = series {
        line.push_str(&format!(
            " | v{} {}/{} | integrity={} | status={}",
            series.version,
            series.present_count(),
            series.expected_total,
            series.integrity.short_label(),
            series.status_label()
        ));
    }
    if let Some(review) = review {
        line.push_str(&format!(
            " | review={} ({})",
            review.status_label(),
            review.review_count()
        ));
    }
    line
}

fn thread_line_max_chars(area: Rect) -> usize {
    let available = area.width.saturating_sub(2) as usize;
    available.min(THREAD_LINE_MAX_CHARS)
}

pub(super) fn thread_line(row: &ThreadRow, max_chars: usize) -> String {
    let max_chars = max_chars.min(THREAD_LINE_MAX_CHARS);
    let indent = "  ".repeat(row.depth as usize);
    let subject = if row.subject.trim().is_empty() {
        "(no subject)"
    } else {
        row.subject.trim()
    };
    truncate_with_ellipsis(&format!("{indent}{subject}"), max_chars)
}

pub(super) fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let value_len = value.chars().count();
    if value_len <= max_chars {
        return value.to_string();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut truncated = String::new();
    for ch in value.chars().take(max_chars - 3) {
        truncated.push(ch);
    }
    truncated.push_str("...");
    truncated
}

pub(super) fn sanitize_inline_ui_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut pending_space = false;

    for character in value.chars() {
        if character.is_control() || character.is_whitespace() {
            pending_space = !sanitized.is_empty();
            continue;
        }

        if pending_space {
            sanitized.push(' ');
            pending_space = false;
        }
        sanitized.push(character);
    }

    sanitized.trim().to_string()
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, state: &AppState, config: &RuntimeConfig) {
    let (warning, preview) = if let Some(thread) = state.selected_thread() {
        if let Some(mail_preview) = state.selected_mail_preview() {
            let preview = if let Some(series_details) =
                load_series_preview(state, config, thread.thread_id)
            {
                Cow::Owned(format!("{series_details}\n\n{}", mail_preview.content))
            } else {
                Cow::Borrowed(mail_preview.content.as_str())
            };
            (mail_preview.warning.as_deref(), preview)
        } else {
            (
                None,
                Cow::Borrowed("<mail preview unavailable; change selection to reload>"),
            )
        }
    } else {
        (
            None,
            Cow::Owned(format!(
                "No synced thread data\n\nRun:\n  criew sync --fixture-dir <DIR>\n\nConfig: {}\nDatabase: {}",
                config.config_path.display(),
                config.database_path.display(),
            )),
        )
    };

    let block = panel_block(Pane::Preview, state.focus);
    let inner_area = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Clear, inner_area);

    let content_area = if let Some(warning_text) = warning {
        let warning_height = warning_text
            .lines()
            .count()
            .min(inner_area.height.saturating_sub(1) as usize) as u16;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(warning_height), Constraint::Min(1)])
            .split(inner_area);
        let warning = Paragraph::new(Text::from(warning_text))
            .style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(warning, sections[0]);
        sections[1]
    } else {
        inner_area
    };

    let preview_scroll_limit = max_vertical_scroll(
        preview.as_ref(),
        content_area.width,
        content_area.height,
        VerticalScrollWrapMode::Enabled,
    );
    state.preview_scroll_limit.set(preview_scroll_limit);
    let scroll = clamp_vertical_scroll(
        preview.as_ref(),
        content_area.width,
        content_area.height,
        state.preview_scroll,
        VerticalScrollWrapMode::Enabled,
    );
    let paragraph = Paragraph::new(preview.as_ref())
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, content_area);
}

fn clamp_vertical_scroll(
    text: &str,
    area_width: u16,
    area_height: u16,
    requested_scroll: u16,
    wrap_mode: VerticalScrollWrapMode,
) -> u16 {
    requested_scroll.min(max_vertical_scroll(
        text,
        area_width,
        area_height,
        wrap_mode,
    ))
}

fn max_vertical_scroll(
    text: &str,
    area_width: u16,
    area_height: u16,
    wrap_mode: VerticalScrollWrapMode,
) -> u16 {
    if area_height == 0 {
        return 0;
    }

    let visible_lines = area_height as usize;
    let total_lines = visual_line_count(text, area_width, wrap_mode);
    total_lines
        .saturating_sub(visible_lines)
        .min(u16::MAX as usize) as u16
}

fn visual_line_count(text: &str, area_width: u16, wrap_mode: VerticalScrollWrapMode) -> usize {
    match wrap_mode {
        VerticalScrollWrapMode::Disabled => text.split('\n').count().max(1),
        VerticalScrollWrapMode::Enabled => {
            if area_width == 0 {
                return 0;
            }

            text.split('\n')
                .map(|line| wrapped_visual_line_count(line, area_width))
                .sum::<usize>()
                .max(1)
        }
    }
}

fn wrapped_visual_line_count(line: &str, area_width: u16) -> usize {
    if area_width == 0 {
        return 0;
    }

    let width = area_width as usize;
    let display_width = display_column(line, line.chars().count()).max(1);
    display_width.saturating_add(width - 1) / width
}

fn load_series_preview(state: &AppState, config: &RuntimeConfig, thread_id: i64) -> Option<String> {
    let series = state.series_summaries.get(&thread_id)?;
    let mut lines = vec![
        format!(
            "Series: v{} {}/{} | integrity={} | status={}",
            series.version,
            series.present_count(),
            series.expected_total,
            series.integrity.short_label(),
            series.status_label()
        ),
        format!("Anchor: <{}>", series.anchor_message_id),
    ];

    if let Some(review) = state.review_summaries.get(&thread_id) {
        lines.push(format!(
            "Review: {} | trailers={}",
            review.status_label(),
            review.review_count()
        ));
        for (kind, values) in review.trailer_groups() {
            lines.push(format!("{kind}: {}", values.join(", ")));
        }
    }

    if !series.missing_seq.is_empty() {
        lines.push(format!(
            "Missing: {}",
            series
                .missing_seq
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !series.duplicate_seq.is_empty() {
        lines.push(format!(
            "Duplicate: {}",
            series
                .duplicate_seq
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }

    match patch_worker::load_latest_report(
        &config.database_path,
        &state.active_thread_mailbox,
        thread_id,
    ) {
        Ok(Some(report)) => {
            if let Some(summary) = report.last_summary.as_deref() {
                lines.push(format!("Last run: {summary}"));
            }
            if let Some(exit_code) = report.last_exit_code {
                lines.push(format!("Exit code: {exit_code}"));
            }
            if let Some(command) = report.last_command.as_deref() {
                lines.push(format!("Command: {command}"));
            }
            if let Some(error) = report.last_error.as_deref() {
                lines.push(format!("Error: {error}"));
            }
        }
        Ok(None) => {}
        Err(error) => {
            lines.push(format!("Series report load failed: {error}"));
        }
    }

    Some(lines.join("\n"))
}

fn load_code_source_preview(state: &AppState) -> String {
    if state.is_code_edit_active() {
        return render_code_edit_preview(state);
    }

    if !state.supports_code_browser() {
        return "No kernel tree configured.\n\nSet [kernel].tree or [kernel].trees in config."
            .to_string();
    }

    let Some(row) = state.selected_kernel_tree_row() else {
        return "<kernel tree is empty>".to_string();
    };

    match row.kind {
        KernelTreeRowKind::File | KernelTreeRowKind::RootFile => {
            load_source_file_preview(&row.path)
        }
        KernelTreeRowKind::MissingPath => {
            format!("<missing path>\n\n{}", row.path.display())
        }
        KernelTreeRowKind::RootDirectory | KernelTreeRowKind::Directory => format!(
            "Directory: {}\n\nSelect a file in the tree to preview source content.",
            row.path.display()
        ),
    }
}

fn render_code_edit_preview(state: &AppState) -> String {
    let target = state
        .code_edit_target
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unknown file>".to_string());
    let dirty = if state.code_edit_dirty { "yes" } else { "no" };
    let mut lines = vec![
        format!("File: {target}"),
        format!(
            "Mode: {} | dirty: {} | cursor: {}:{}",
            state.code_edit_mode.label(),
            dirty,
            state.code_edit_cursor_row + 1,
            state.code_edit_cursor_col + 1
        ),
        "Use h/j/k/l move, i insert, x delete, s save, E external vim, : command".to_string(),
        String::new(),
    ];

    if state.code_edit_buffer.is_empty() {
        lines.push("<empty file>".to_string());
    } else {
        for (index, line) in state.code_edit_buffer.iter().enumerate() {
            let marker = if index == state.code_edit_cursor_row {
                ">"
            } else {
                " "
            };
            let rendered_line = sanitize_source_preview_text(line);
            lines.push(format!("{:>4}{marker} {}", index + 1, rendered_line));
        }
    }

    if matches!(state.code_edit_mode, CodeEditMode::VimCommand) {
        lines.push(String::new());
        lines.push(format!(":{}", state.code_edit_command_input));
    }

    lines.join("\n")
}

pub(super) fn code_edit_cursor_position(state: &AppState, inner_area: Rect) -> Option<(u16, u16)> {
    if !state.is_code_edit_active() || inner_area.width == 0 || inner_area.height == 0 {
        return None;
    }

    let (logical_row, logical_col) = if matches!(state.code_edit_mode, CodeEditMode::VimCommand) {
        (
            code_edit_command_line_logical_row(state),
            1 + state.code_edit_command_input.chars().count(),
        )
    } else {
        let row = state
            .code_edit_cursor_row
            .min(state.code_edit_buffer.len().saturating_sub(1));
        let line = state
            .code_edit_buffer
            .get(row)
            .map(String::as_str)
            .unwrap_or_default();
        let column = state.code_edit_cursor_col.min(line.chars().count());
        (
            code_edit_source_line_logical_row(row),
            code_edit_source_line_prefix_width(row) + display_column(line, column),
        )
    };

    let scroll = state.code_preview_scroll as usize;
    if logical_row < scroll {
        return None;
    }
    let visible_row = logical_row - scroll;
    if visible_row >= inner_area.height as usize {
        return None;
    }

    let clamped_col = logical_col.min(inner_area.width.saturating_sub(1) as usize);
    Some((
        inner_area.x.saturating_add(clamped_col as u16),
        inner_area.y.saturating_add(visible_row as u16),
    ))
}

fn draw_reply_panel(frame: &mut Frame<'_>, state: &AppState) {
    let Some(panel) = state.reply_panel.as_ref() else {
        return;
    };

    let area = centered_rect(88, 84, frame.area());
    if panel.preview_open {
        draw_send_preview_panel(frame, area, panel);
        return;
    }
    if panel.reply_notice.is_some() {
        draw_reply_notice_panel(frame, area, panel);
        return;
    }

    frame.render_widget(Clear, area);
    let title = format!(
        "Reply Panel [{} dirty:{} confirmed:{} focus:{}]",
        panel.mode.label(),
        if panel.dirty { "*" } else { "-" },
        if panel.preview_confirmed { "yes" } else { "no" },
        panel.section.label()
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightGreen));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Clear, inner);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(8)])
        .split(inner);
    let header_area = sections[0];
    let body_area = sections[1];

    let header_block = Block::default()
        .title("Headers ([edit] / [read-only])")
        .borders(Borders::ALL)
        .border_style(reply_panel_section_style(
            matches!(
                panel.section,
                ReplySection::From | ReplySection::To | ReplySection::Cc | ReplySection::Subject
            ),
            false,
        ));
    let header_inner = header_block.inner(header_area);
    frame.render_widget(header_block, header_area);
    frame.render_widget(Clear, header_inner);
    frame.render_widget(
        Paragraph::new(render_reply_header_content(panel)).wrap(Wrap { trim: false }),
        header_inner,
    );

    let body_block = Block::default()
        .title("Reply Body")
        .borders(Borders::ALL)
        .border_style(reply_panel_section_style(
            matches!(panel.section, ReplySection::Body)
                || matches!(panel.mode, ReplyEditMode::Command),
            matches!(panel.mode, ReplyEditMode::Command),
        ));
    let body_inner = body_block.inner(body_area);
    frame.render_widget(body_block, body_area);
    frame.render_widget(Clear, body_inner);
    let body_content_area = if body_inner.height > 1 {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(body_inner);
        frame.render_widget(
            Paragraph::new(render_reply_body_guide_line(sections[0].width as usize)).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            sections[0],
        );
        sections[1]
    } else {
        body_inner
    };
    frame.render_widget(
        Paragraph::new(render_reply_body_content(panel))
            .scroll((panel.scroll, 0))
            .wrap(Wrap { trim: false }),
        body_content_area,
    );

    if let Some(cursor) = reply_panel_cursor_position(panel, header_inner, body_content_area) {
        frame.set_cursor_position(cursor);
    }
}

fn render_reply_header_content(panel: &ReplyPanelState) -> String {
    [
        format!(
            "{} {}{}",
            reply_section_marker(panel, ReplySection::From),
            reply_editable_field_prefix(ReplySection::From),
            sanitize_source_preview_text(&panel.from)
        ),
        format!(
            "{} {}{}",
            reply_section_marker(panel, ReplySection::To),
            reply_editable_field_prefix(ReplySection::To),
            sanitize_source_preview_text(&panel.to)
        ),
        format!(
            "{} {}{}",
            reply_section_marker(panel, ReplySection::Cc),
            reply_editable_field_prefix(ReplySection::Cc),
            sanitize_source_preview_text(&panel.cc)
        ),
        format!(
            "{} {}{}",
            reply_section_marker(panel, ReplySection::Subject),
            reply_editable_field_prefix(ReplySection::Subject),
            sanitize_source_preview_text(&panel.subject)
        ),
        format!("  [read-only] In-Reply-To: <{}>", panel.in_reply_to),
        format!(
            "  [read-only] References: {}",
            panel
                .references
                .iter()
                .map(|value| format!("<{value}>"))
                .collect::<Vec<String>>()
                .join(" ")
        ),
    ]
    .join("\n")
}

fn render_reply_body_content(panel: &ReplyPanelState) -> String {
    let mut lines = Vec::with_capacity(panel.body.len() + 2);
    for (index, line) in panel.body.iter().enumerate() {
        let marker = if matches!(panel.section, ReplySection::Body) && panel.body_row == index {
            ">"
        } else {
            " "
        };
        lines.push(format!(
            "{:>4}{marker} {}",
            index + 1,
            sanitize_source_preview_text(line)
        ));
    }

    if matches!(panel.mode, ReplyEditMode::Command) {
        lines.push(String::new());
        lines.push(format!(":{}", panel.command_input));
    }

    lines.join("\n")
}

fn render_reply_body_guide_line(width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let body_start = reply_body_prefix_width(0);
    if body_start >= width {
        return String::new();
    }

    let guide_offset = reply_body_prefix_width(0) + REPLY_BODY_GUIDE_COLUMN - 1;
    let label = "| 80 cols";

    let mut chars = vec![' '; width];
    let pre_label_end = guide_offset.min(width);
    for ch in chars.iter_mut().take(pre_label_end).skip(body_start) {
        *ch = '=';
    }

    for (index, ch) in label.chars().enumerate() {
        let position = guide_offset + index;
        if position >= width {
            break;
        }
        chars[position] = ch;
    }

    let label_end = (guide_offset + label.len()).min(width);
    for ch in chars.iter_mut().take(width).skip(label_end) {
        *ch = '=';
    }

    chars.into_iter().collect()
}

fn reply_panel_section_style(focused: bool, command_mode: bool) -> Style {
    if focused {
        if command_mode {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::LightGreen)
        }
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn reply_section_marker(panel: &ReplyPanelState, section: ReplySection) -> &'static str {
    if panel.section == section { ">" } else { " " }
}

fn reply_panel_cursor_position(
    panel: &ReplyPanelState,
    header_inner: Rect,
    body_inner: Rect,
) -> Option<(u16, u16)> {
    if panel.preview_open || panel.reply_notice.is_some() {
        return None;
    }

    if matches!(panel.mode, ReplyEditMode::Command) {
        return reply_body_cursor_position(
            body_inner,
            panel.scroll,
            reply_command_line_logical_row(panel),
            1 + panel.command_input.chars().count(),
        );
    }

    match panel.section {
        ReplySection::From => reply_fixed_cursor_position(
            header_inner,
            0,
            reply_field_prefix_width(ReplySection::From)
                + display_column(&panel.from, panel.cursor_col),
        ),
        ReplySection::To => reply_fixed_cursor_position(
            header_inner,
            1,
            reply_field_prefix_width(ReplySection::To)
                + display_column(&panel.to, panel.cursor_col),
        ),
        ReplySection::Cc => reply_fixed_cursor_position(
            header_inner,
            2,
            reply_field_prefix_width(ReplySection::Cc)
                + display_column(&panel.cc, panel.cursor_col),
        ),
        ReplySection::Subject => reply_fixed_cursor_position(
            header_inner,
            3,
            reply_field_prefix_width(ReplySection::Subject)
                + display_column(&panel.subject, panel.cursor_col),
        ),
        ReplySection::Body => {
            let line = panel
                .body
                .get(panel.body_row)
                .map(String::as_str)
                .unwrap_or_default();
            reply_body_cursor_position(
                body_inner,
                panel.scroll,
                reply_body_line_logical_row(panel.body_row),
                reply_body_prefix_width(panel.body_row) + display_column(line, panel.cursor_col),
            )
        }
    }
}

fn reply_fixed_cursor_position(
    inner_area: Rect,
    logical_row: usize,
    logical_col: usize,
) -> Option<(u16, u16)> {
    if inner_area.width == 0 || inner_area.height == 0 || logical_row >= inner_area.height as usize
    {
        return None;
    }

    let clamped_col = logical_col.min(inner_area.width.saturating_sub(1) as usize);
    Some((
        inner_area.x.saturating_add(clamped_col as u16),
        inner_area.y.saturating_add(logical_row as u16),
    ))
}

fn reply_body_cursor_position(
    inner_area: Rect,
    scroll: u16,
    logical_row: usize,
    logical_col: usize,
) -> Option<(u16, u16)> {
    if inner_area.width == 0 || inner_area.height == 0 {
        return None;
    }

    let scroll = scroll as usize;
    if logical_row < scroll {
        return None;
    }
    let visible_row = logical_row - scroll;
    if visible_row >= inner_area.height as usize {
        return None;
    }

    let clamped_col = logical_col.min(inner_area.width.saturating_sub(1) as usize);
    Some((
        inner_area.x.saturating_add(clamped_col as u16),
        inner_area.y.saturating_add(visible_row as u16),
    ))
}

fn draw_send_preview_panel(frame: &mut Frame<'_>, area: Rect, panel: &ReplyPanelState) {
    frame.render_widget(Clear, area);

    let title = if !panel.preview_errors.is_empty() {
        "Send Preview [invalid]"
    } else if !panel.preview_warnings.is_empty() {
        "Send Preview [warning]"
    } else if has_authored_reply_lines(panel) {
        "Send Preview [reply highlighted]"
    } else {
        "Send Preview"
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let content_area = draw_send_preview_messages(frame, inner, panel);
    let preview = Paragraph::new(render_reply_preview_text(panel))
        .scroll((panel.preview_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(preview, content_area);
}

fn draw_send_preview_messages(frame: &mut Frame<'_>, area: Rect, panel: &ReplyPanelState) -> Rect {
    let mut remaining_height = area.height.saturating_sub(1);
    let error_height = preview_message_height(&panel.preview_errors, remaining_height);
    remaining_height = remaining_height.saturating_sub(error_height);
    let warning_height = preview_message_height(&panel.preview_warnings, remaining_height);
    remaining_height = remaining_height.saturating_sub(warning_height);
    let info_messages = preview_info_messages(panel);
    let info_height = preview_message_height(&info_messages, remaining_height);

    if error_height == 0 && warning_height == 0 && info_height == 0 {
        return area;
    }

    let mut constraints = Vec::new();
    if error_height > 0 {
        constraints.push(Constraint::Length(error_height));
    }
    if warning_height > 0 {
        constraints.push(Constraint::Length(warning_height));
    }
    if info_height > 0 {
        constraints.push(Constraint::Length(info_height));
    }
    constraints.push(Constraint::Min(1));

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut section_index = 0usize;
    if error_height > 0 {
        draw_send_preview_message_block(
            frame,
            sections[section_index],
            &panel.preview_errors,
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        );
        section_index += 1;
    }
    if warning_height > 0 {
        draw_send_preview_message_block(
            frame,
            sections[section_index],
            &panel.preview_warnings,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        section_index += 1;
    }
    if info_height > 0 {
        draw_send_preview_message_block(
            frame,
            sections[section_index],
            &info_messages,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        section_index += 1;
    }

    sections[section_index]
}

fn preview_message_height(messages: &[String], remaining_height: u16) -> u16 {
    if messages.is_empty() || remaining_height == 0 {
        return 0;
    }

    let line_count = messages
        .iter()
        .map(|message| message.lines().count().max(1))
        .sum::<usize>();
    line_count.min(remaining_height as usize) as u16
}

fn draw_send_preview_message_block(
    frame: &mut Frame<'_>,
    area: Rect,
    messages: &[String],
    style: Style,
) {
    let text = messages
        .iter()
        .map(|value| format!("- {value}"))
        .collect::<Vec<String>>()
        .join("\n");
    let paragraph = Paragraph::new(text).style(style).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_reply_preview_text(panel: &ReplyPanelState) -> Text<'static> {
    Text::from(
        panel
            .preview_lines
            .iter()
            .map(|line| {
                Line::from(Span::styled(
                    line.text.clone(),
                    reply_preview_line_style(line.kind),
                ))
            })
            .collect::<Vec<Line<'static>>>(),
    )
}

fn preview_info_messages(panel: &ReplyPanelState) -> Vec<String> {
    if has_authored_reply_lines(panel) {
        vec!["Your authored reply lines are highlighted below.".to_string()]
    } else {
        Vec::new()
    }
}

fn has_authored_reply_lines(panel: &ReplyPanelState) -> bool {
    panel
        .preview_lines
        .iter()
        .any(|line| matches!(line.kind, ReplyPreviewLineKind::Authored))
}

fn reply_preview_line_style(kind: ReplyPreviewLineKind) -> Style {
    match kind {
        ReplyPreviewLineKind::Header => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        ReplyPreviewLineKind::Blank => Style::default(),
        ReplyPreviewLineKind::Authored => Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ReplyPreviewLineKind::QuoteAttribution => Style::default().fg(Color::Cyan),
        ReplyPreviewLineKind::Quoted => Style::default().fg(Color::White),
        ReplyPreviewLineKind::Placeholder => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    }
}

fn draw_reply_notice_panel(frame: &mut Frame<'_>, area: Rect, panel: &ReplyPanelState) {
    let Some(notice) = panel.reply_notice.as_ref() else {
        return;
    };

    frame.render_widget(Clear, area);

    let border = match notice.kind {
        ReplyNoticeKind::Warning => Style::default().fg(Color::LightRed),
        ReplyNoticeKind::Info => Style::default().fg(Color::LightGreen),
    };
    let block = Block::default()
        .title(notice.title.as_str())
        .borders(Borders::ALL)
        .border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Clear, inner);

    let content_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Min(3),
            Constraint::Percentage(35),
        ])
        .split(inner);
    let text = format!("{}\n\n{}", notice.message, notice.hint);
    let paragraph = Paragraph::new(Text::from(text))
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, content_sections[1]);
}

fn code_edit_source_line_logical_row(buffer_row: usize) -> usize {
    4 + buffer_row
}

fn code_edit_source_line_prefix_width(buffer_row: usize) -> usize {
    let number_width = ((buffer_row + 1).to_string().chars().count()).max(4);
    number_width + 2
}

fn code_edit_command_line_logical_row(state: &AppState) -> usize {
    4 + state.code_edit_buffer.len() + 1
}

fn display_column(line: &str, char_col: usize) -> usize {
    let mut display_col = 0usize;
    for character in line.chars().take(char_col) {
        match character {
            '\t' => {
                display_col += PREVIEW_TAB_SPACES.chars().count();
            }
            '\n' | '\r' => {}
            _ if character.is_control() => {}
            _ => {
                display_col += 1;
            }
        }
    }
    display_col
}

pub(super) fn load_source_file_preview(path: &Path) -> String {
    let content = match fs::read(path) {
        Ok(value) => value,
        Err(error) => return format!("<failed to read {}: {}>", path.display(), error),
    };

    let truncated_by_bytes = content.len() > CODE_PREVIEW_MAX_BYTES;
    let content_slice = if truncated_by_bytes {
        &content[..CODE_PREVIEW_MAX_BYTES]
    } else {
        content.as_slice()
    };

    let text = String::from_utf8_lossy(content_slice)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let sanitized = sanitize_source_preview_text(&text);
    let mut source_lines = sanitized.lines();
    let mut lines = Vec::new();
    for line in source_lines.by_ref().take(CODE_PREVIEW_MAX_LINES) {
        lines.push(line);
    }
    let truncated_by_lines = source_lines.next().is_some();

    let body = if lines.is_empty() {
        "<empty file>".to_string()
    } else {
        lines.join("\n")
    };

    let mut preview = format!("File: {}\n\n{}", path.display(), body);
    if truncated_by_bytes || truncated_by_lines {
        preview.push_str("\n\n<truncated preview>");
    }
    preview
}

fn sanitize_source_preview_text(input: &str) -> String {
    let mut sanitized = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push_str(PREVIEW_TAB_SPACES),
            _ if character.is_control() => {}
            _ => sanitized.push(character),
        }
    }
    sanitized
}

fn draw_search_overlay(frame: &mut Frame<'_>, state: &AppState) {
    let area = centered_rect(70, 20, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title("Search Threads")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let input = Paragraph::new(format!("> {}", state.search.input));
    frame.render_widget(input, sections[0]);

    let hint = Paragraph::new(
        "Enter: apply  Esc: cancel  syntax: subject:, from:, id:, after:, before:, review:, is:patch, -term",
    );
    frame.render_widget(hint, sections[1]);

    let current = if state.search.applied_query.is_empty() {
        "Current filter: <none>".to_string()
    } else {
        format!("Current filter: {}", state.search.applied_query)
    };
    frame.render_widget(Paragraph::new(current), sections[2]);
}

fn draw_command_palette(frame: &mut Frame<'_>, state: &AppState) {
    let area = centered_rect(70, 28, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title("Command Palette")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightGreen));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let input = Paragraph::new(format!("> {}", state.palette.input));
    frame.render_widget(input, sections[0]);

    let hints = Paragraph::new(
        "Tab: complete (built-in + !local)  Enter: execute  !<cmd>: local shell in [kernel].tree (or ~)  Esc: close",
    );
    frame.render_widget(hints, sections[1]);

    let show_local_result = state.palette.last_local_result.is_some()
        && (state.palette.input.trim().is_empty()
            || state.palette.input.trim_start().starts_with('!'));

    if show_local_result {
        if let Some(result) = state.palette.last_local_result.as_ref() {
            let header = Paragraph::new(format!(
                "Local Result: !{} | exit={} | cwd={}",
                result.command,
                result.exit_code,
                result.cwd.display()
            ));
            frame.render_widget(header, sections[2]);

            let output = Paragraph::new(result.output.clone())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Gray)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(output, sections[3]);
        }
    } else {
        let suggestions = palette_overlay_suggestions(state);
        let candidate_title = if suggestions.is_empty() {
            "Candidates: <none>"
        } else if state.palette.show_suggestions {
            "Completion Candidates"
        } else {
            "Command Candidates"
        };
        let candidate_header = Paragraph::new(candidate_title);
        frame.render_widget(candidate_header, sections[2]);

        let items: Vec<ListItem> = suggestions
            .iter()
            .take(8)
            .map(|suggestion| match suggestion.description.as_deref() {
                Some(description) if !description.is_empty() => {
                    ListItem::new(format!("{} - {}", suggestion.value, description))
                }
                _ => ListItem::new(suggestion.value.clone()),
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray)),
        );
        frame.render_widget(list, sections[3]);
    }
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);

    horizontal[1]
}

#[cfg(test)]
mod tests {
    use super::format_uptime_label;

    #[test]
    fn uptime_label_uses_the_largest_needed_unit() {
        assert_eq!(format_uptime_label(59), "59s");
        assert_eq!(format_uptime_label(61), "01m:01s");
        assert_eq!(format_uptime_label(3_661), "01h:01m:01s");
    }
}
