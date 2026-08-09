//! Command-palette execution and completions.
//!
//! The palette is the escape hatch for actions that do not fit well into a
//! single-key workflow. Keeping command parsing here avoids leaking shell-like
//! concerns into the rest of the TUI state machine.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::config::config_completion_suggestions;
use super::*;

pub(super) fn run_palette_local_command(state: &mut AppState, local_command: &str) {
    let local_command = local_command.trim();
    if local_command.is_empty() {
        state.palette.clear_local_result();
        state.status = "empty local command after !".to_string();
        return;
    }
    tracing::info!(
        op = "local_command",
        status = "started",
        command = %local_command
    );

    let cwd = match resolve_palette_local_workdir(state) {
        Ok(path) => path,
        Err(message) => {
            tracing::error!(
                op = "local_command",
                status = "failed",
                command = %local_command,
                error = %message
            );
            state.palette.clear_local_result();
            state.status = format!("local command setup failed: {message}");
            return;
        }
    };

    let output = ProcessCommand::new("bash")
        .arg("-lc")
        .arg(local_command)
        .current_dir(&cwd)
        .output();

    match output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let output_text = render_local_command_output(&stdout, &stderr);
            let summary = first_non_empty_line(&stdout)
                .or_else(|| first_non_empty_line(&stderr))
                .unwrap_or_else(|| "<no output>".to_string());
            let exit_code = output
                .status
                .code()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "signal".to_string());
            state.palette.last_local_result = Some(LocalCommandResult {
                command: local_command.to_string(),
                cwd: cwd.clone(),
                exit_code: exit_code.clone(),
                output: output_text,
            });

            if output.status.success() {
                state.status = format!(
                    "local command ok (exit={} cwd={}): {}",
                    exit_code,
                    cwd.display(),
                    summary
                );
                tracing::info!(
                    op = "local_command",
                    status = "succeeded",
                    command = local_command,
                    cwd = %cwd.display(),
                    exit_code = %exit_code
                );
            } else {
                state.status = format!(
                    "local command failed (exit={} cwd={}): {}",
                    exit_code,
                    cwd.display(),
                    summary
                );
                tracing::error!(
                    op = "local_command",
                    status = "failed",
                    command = local_command,
                    cwd = %cwd.display(),
                    exit_code = %exit_code
                );
            }
        }
        Err(error) => {
            tracing::error!(
                op = "local_command",
                status = "failed",
                command = %local_command,
                cwd = %cwd.display(),
                error = %error
            );
            state.palette.last_local_result = Some(LocalCommandResult {
                command: local_command.to_string(),
                cwd: cwd.clone(),
                exit_code: "spawn-error".to_string(),
                output: format!("{error}"),
            });
            state.status = format!(
                "failed to launch local command in {}: {}",
                cwd.display(),
                error
            );
        }
    }
}

fn render_local_command_output(stdout: &str, stderr: &str) -> String {
    let stdout_trimmed = stdout.trim_end();
    let stderr_trimmed = stderr.trim_end();
    match (stdout_trimmed.is_empty(), stderr_trimmed.is_empty()) {
        (true, true) => "<no output>".to_string(),
        (false, true) => stdout_trimmed.to_string(),
        (true, false) => format!("[stderr]\n{stderr_trimmed}"),
        (false, false) => format!("{stdout_trimmed}\n\n[stderr]\n{stderr_trimmed}"),
    }
}

pub(super) fn short_commit_id(value: &str) -> String {
    value.chars().take(12).collect()
}

pub(super) fn resolve_palette_local_workdir(
    state: &AppState,
) -> std::result::Result<PathBuf, String> {
    // Prefer the configured kernel tree so local commands run in the same repo
    // the code browser and patch actions are already talking about.
    if let Some(path) = state.runtime.kernel_trees.first() {
        if !path.exists() {
            return Err(format!("[kernel].tree does not exist: {}", path.display()));
        }
        if !path.is_dir() {
            return Err(format!(
                "[kernel].tree is not a directory: {}",
                path.display()
            ));
        }
        return Ok(path.clone());
    }

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set and [kernel].tree is not configured".to_string())?;
    if !home.exists() || !home.is_dir() {
        return Err(format!("home directory is unavailable: {}", home.display()));
    }

    Ok(home)
}

fn first_non_empty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn run_palette_sync(state: &mut AppState, command: &str) {
    tracing::debug!(command = %command, "user executed sync command from palette");
    let mailbox_override = command
        .split_whitespace()
        .nth(1)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    let mailboxes = if let Some(mailbox) = mailbox_override {
        vec![mailbox]
    } else {
        let enabled = state.enabled_mailboxes();
        if enabled.is_empty() {
            vec![state.runtime.default_active_mailbox().to_string()]
        } else {
            enabled
        }
    };
    tracing::info!(
        op = "sync",
        status = "started",
        command = %command,
        mailboxes = %mailboxes.join(",")
    );
    state.queue_palette_sync(mailboxes);
}

pub(super) fn apply_palette_completion(state: &mut AppState) {
    if state.palette.input.trim_start().starts_with('!') {
        apply_local_palette_completion(state);
        return;
    }

    let input_before_completion = state.palette.input.clone();
    let context = parse_palette_completion_context(&state.palette.input);
    let mut suggestions = palette_completion_suggestions(state, &context);
    let prefix_lower = context.active_token.to_ascii_lowercase();
    suggestions.retain(|suggestion| {
        suggestion
            .value
            .to_ascii_lowercase()
            .starts_with(&prefix_lower)
    });
    suggestions.sort_by(|left, right| left.value.cmp(&right.value));
    suggestions.dedup_by(|left, right| left.value == right.value);

    if suggestions.is_empty() {
        state.palette.clear_completion();
        state.status = "no completion candidates".to_string();
        return;
    }

    let completion_values: Vec<String> = suggestions
        .iter()
        .map(|suggestion| suggestion.value.clone())
        .collect();

    if completion_values.len() == 1 {
        let candidate = completion_values[0].clone();
        state.palette.input = format!("{}{} ", context.prefix, candidate);
        state.palette.clear_completion();
        state.status = format!("completed: {candidate}");
        return;
    }

    let common_prefix = longest_common_prefix(&completion_values);
    if common_prefix.len() > context.active_token.len() {
        state.palette.input = format!("{}{}", context.prefix, common_prefix);
    }

    let show_suggestions =
        context.active_token.is_empty() || state.palette.last_tab_input == input_before_completion;
    state.palette.suggestions = suggestions;
    state.palette.show_suggestions = show_suggestions;
    state.palette.last_tab_input = state.palette.input.clone();

    let summary = state
        .palette
        .suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .take(5)
        .collect::<Vec<_>>()
        .join(", ");

    state.status = if show_suggestions {
        format!(
            "completion options: {}",
            if summary.is_empty() {
                "<none>".to_string()
            } else {
                summary
            }
        )
    } else {
        format!(
            "{} completion candidates (Tab again to list)",
            state.palette.suggestions.len()
        )
    };
}

fn apply_local_palette_completion(state: &mut AppState) {
    let Some((local_prefix, local_input)) = split_local_palette_input(&state.palette.input) else {
        state.palette.clear_completion();
        state.status = "invalid local command mode".to_string();
        return;
    };

    let input_before_completion = state.palette.input.clone();
    let context = parse_palette_completion_context(&local_input);
    let mut suggestions = local_completion_suggestions(state, &context);
    let prefix_lower = context.active_token.to_ascii_lowercase();
    suggestions.retain(|suggestion| {
        suggestion
            .value
            .to_ascii_lowercase()
            .starts_with(&prefix_lower)
    });
    suggestions.sort_by(|left, right| left.value.cmp(&right.value));
    suggestions.dedup_by(|left, right| left.value == right.value);

    if suggestions.is_empty() {
        state.palette.clear_completion();
        state.status = "no completion candidates".to_string();
        return;
    }

    let completion_values: Vec<String> = suggestions
        .iter()
        .map(|suggestion| suggestion.value.clone())
        .collect();
    let completion_prefix = format!("{local_prefix}{}", context.prefix);

    if completion_values.len() == 1 {
        let candidate = completion_values[0].clone();
        state.palette.input = format!(
            "{}{}{}",
            completion_prefix,
            candidate,
            completion_suffix(&candidate)
        );
        state.palette.clear_completion();
        state.status = format!("completed: {candidate}");
        return;
    }

    let common_prefix = longest_common_prefix(&completion_values);
    if common_prefix.len() > context.active_token.len() {
        state.palette.input = format!("{completion_prefix}{common_prefix}");
    }

    let show_suggestions =
        context.active_token.is_empty() || state.palette.last_tab_input == input_before_completion;
    state.palette.suggestions = suggestions;
    state.palette.show_suggestions = show_suggestions;
    state.palette.last_tab_input = state.palette.input.clone();

    let summary = state
        .palette
        .suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .take(5)
        .collect::<Vec<_>>()
        .join(", ");

    state.status = if show_suggestions {
        format!(
            "completion options: {}",
            if summary.is_empty() {
                "<none>".to_string()
            } else {
                summary
            }
        )
    } else {
        format!(
            "{} completion candidates (Tab again to list)",
            state.palette.suggestions.len()
        )
    };
}

fn completion_suffix(candidate: &str) -> &'static str {
    if candidate.ends_with('/') { "" } else { " " }
}

fn split_local_palette_input(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('!') {
        return None;
    }
    let leading_whitespace_len = input.len() - trimmed.len();
    let leading = &input[..leading_whitespace_len];
    let content = trimmed.strip_prefix('!')?.to_string();
    Some((format!("{leading}!"), content))
}

fn local_completion_suggestions(
    state: &mut AppState,
    context: &PaletteCompletionContext,
) -> Vec<PaletteSuggestion> {
    let token = context.active_token.as_str();
    let token_looks_like_path =
        token.contains('/') || token.starts_with('.') || token.starts_with('~');
    let Ok(workdir) = resolve_palette_local_workdir(state) else {
        return Vec::new();
    };

    if context.active_index == 0 && !token_looks_like_path {
        return local_command_completion_suggestions(
            &mut state.palette.local_command_completion_cache,
        );
    }

    local_path_completion_suggestions(&workdir, token)
}

fn local_command_completion_suggestions(
    cache: &mut LocalCommandCompletionCache,
) -> Vec<PaletteSuggestion> {
    let current_path = env::var_os("PATH");
    if cache.is_loaded && cache.path_value == current_path {
        return cache.suggestions.clone();
    }

    let mut seen = HashSet::new();
    let mut suggestions = Vec::new();

    for builtin in ["cd", "echo", "pwd", "true", "false", "test"] {
        if seen.insert(builtin.to_string()) {
            suggestions.push(PaletteSuggestion {
                value: builtin.to_string(),
                description: Some("Shell builtin".to_string()),
            });
        }
    }

    if let Some(path_os) = current_path.as_deref() {
        for directory in env::split_paths(&path_os) {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_executable_path(&path) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if name.is_empty() || !seen.insert(name.clone()) {
                    continue;
                }
                suggestions.push(PaletteSuggestion {
                    value: name,
                    description: Some("Executable in PATH".to_string()),
                });
            }
        }
    }

    cache.is_loaded = true;
    cache.path_value = current_path;
    cache.suggestions = suggestions;
    cache.suggestions.clone()
}

fn local_path_completion_suggestions(base_dir: &Path, token: &str) -> Vec<PaletteSuggestion> {
    if token == "~" {
        return vec![PaletteSuggestion {
            value: "~/".to_string(),
            description: Some("Home directory".to_string()),
        }];
    }

    let (dir_part, entry_prefix) = token
        .rsplit_once('/')
        .map(|(left, right)| (Some(left), right))
        .unwrap_or((None, token));

    let (search_dir, display_prefix) = match dir_part {
        Some(part) if token.starts_with('/') && part.is_empty() => {
            (PathBuf::from("/"), "/".to_string())
        }
        Some("~") => match env::var("HOME") {
            Ok(home) => (PathBuf::from(home), "~/".to_string()),
            Err(_) => return Vec::new(),
        },
        Some(part) if part.starts_with("~/") => match env::var("HOME") {
            Ok(home) => {
                let suffix = part.strip_prefix("~/").unwrap_or_default();
                (PathBuf::from(home).join(suffix), format!("{part}/"))
            }
            Err(_) => return Vec::new(),
        },
        Some(part) => {
            let search = if Path::new(part).is_absolute() {
                PathBuf::from(part)
            } else {
                base_dir.join(part)
            };
            (search, format!("{part}/"))
        }
        None => (base_dir.to_path_buf(), String::new()),
    };

    let Ok(entries) = fs::read_dir(search_dir) else {
        return Vec::new();
    };

    let mut suggestions = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(entry_prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let mut value = format!("{display_prefix}{name}");
        if is_dir {
            value.push('/');
        }
        suggestions.push(PaletteSuggestion {
            value,
            description: Some(if is_dir {
                "Directory".to_string()
            } else {
                "Path".to_string()
            }),
        });
    }
    suggestions
}

fn is_executable_path(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path)
            .map(|metadata| metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn palette_completion_suggestions(
    state: &AppState,
    context: &PaletteCompletionContext,
) -> Vec<PaletteSuggestion> {
    if context.active_index == 0 {
        return PALETTE_COMMANDS
            .iter()
            .map(|command| PaletteSuggestion {
                value: command.name.to_string(),
                description: Some(command.description.to_string()),
            })
            .collect();
    }

    let command = context
        .tokens
        .first()
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();

    match command.as_str() {
        "config" => config_completion_suggestions(state, context),
        "sync" => sync_completion_suggestions(state, context),
        "review-inbox" => review_inbox_completion_suggestions(context),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone)]
pub(super) struct PaletteCompletionContext {
    pub(super) tokens: Vec<String>,
    pub(super) active_index: usize,
    pub(super) active_token: String,
    pub(super) prefix: String,
}

fn parse_palette_completion_context(input: &str) -> PaletteCompletionContext {
    let tokens: Vec<String> = input.split_whitespace().map(ToOwned::to_owned).collect();
    let trailing_space = input.chars().last().is_some_and(char::is_whitespace);

    if trailing_space {
        return PaletteCompletionContext {
            active_index: tokens.len(),
            active_token: String::new(),
            prefix: input.to_string(),
            tokens,
        };
    }

    let split_index = input
        .char_indices()
        .rev()
        .find_map(|(index, character)| character.is_whitespace().then_some(index));

    if let Some(index) = split_index {
        return PaletteCompletionContext {
            active_index: tokens.len().saturating_sub(1),
            active_token: input[index + 1..].to_string(),
            prefix: input[..=index].to_string(),
            tokens,
        };
    }

    PaletteCompletionContext {
        active_index: 0,
        active_token: input.to_string(),
        prefix: String::new(),
        tokens,
    }
}

fn sync_completion_suggestions(
    state: &AppState,
    context: &PaletteCompletionContext,
) -> Vec<PaletteSuggestion> {
    if context.active_index != 1 {
        return Vec::new();
    }

    let mut candidates: Vec<String> = state
        .subscriptions
        .iter()
        .map(|subscription| subscription.mailbox.clone())
        .collect();
    candidates.push(state.active_thread_mailbox.clone());
    candidates.push(state.runtime.source_mailbox.clone());
    candidates.sort();
    candidates.dedup();
    candidates
        .into_iter()
        .map(|value| PaletteSuggestion {
            value,
            description: Some("Mailbox".to_string()),
        })
        .collect()
}

fn review_inbox_completion_suggestions(
    context: &PaletteCompletionContext,
) -> Vec<PaletteSuggestion> {
    if context.active_index != 1 {
        return Vec::new();
    }

    ["needs-review", "reviewed", "all", "off"]
        .into_iter()
        .map(|value| PaletteSuggestion {
            value: value.to_string(),
            description: Some("Review Inbox view".to_string()),
        })
        .collect()
}

fn longest_common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };

    let mut prefix = first.clone();
    for value in values.iter().skip(1) {
        let mut matched_bytes = 0usize;
        for (left, right) in prefix.chars().zip(value.chars()) {
            if left == right {
                matched_bytes += left.len_utf8();
            } else {
                break;
            }
        }
        prefix.truncate(matched_bytes);
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

pub(super) fn is_palette_toggle(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::F(1))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('`')
                    | KeyCode::Char('~')
                    | KeyCode::Char('/')
                    | KeyCode::Char('?')
                    | KeyCode::Char('_')
                    | KeyCode::Null
            ))
}

pub(super) fn is_palette_open_shortcut(key: KeyEvent) -> bool {
    is_palette_toggle(key) || is_palette_open_fallback_key(key)
}

fn is_palette_open_fallback_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(':'))
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

pub(super) fn palette_overlay_suggestions(state: &AppState) -> Vec<PaletteSuggestion> {
    if state.palette.show_suggestions && !state.palette.suggestions.is_empty() {
        return state.palette.suggestions.clone();
    }

    matching_commands(&state.palette.input)
        .into_iter()
        .map(|command| PaletteSuggestion {
            value: command.name.to_string(),
            description: Some(command.description.to_string()),
        })
        .collect()
}
