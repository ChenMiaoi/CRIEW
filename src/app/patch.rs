//! Patch-series analysis and `b4` orchestration.
//!
//! Thread inspection, integrity checks, and external command execution live in
//! one module so the TUI can ask higher-level questions such as "is this
//! series ready to apply?" without knowing how `b4` or patch metadata storage
//! work underneath.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::domain::models::PatchSeriesStatus;
use crate::infra::b4;
use crate::infra::config::RuntimeConfig;
use crate::infra::error::{CriewError, ErrorCode, Result};
use crate::infra::mail_store::ThreadRow;
use crate::infra::patch_store;

const B4_ACTION_TIMEOUT: Duration = Duration::from_secs(120);
const PREFLIGHT_ACTION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LOG_BYTES: usize = 16 * 1024;
const DOWNLOAD_NAME_MAX_CHARS: usize = 72;
const APPLY_ARTIFACTS_DIR: &str = "apply-artifacts";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchAction {
    Apply,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightToolStatus {
    Passed,
    Findings,
    Failed,
    Skipped,
}

impl PreflightToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Findings => "findings",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    fn is_blocking(self) -> bool {
        matches!(self, Self::Findings | Self::Failed)
    }
}

#[derive(Debug, Clone)]
pub struct PreflightToolResult {
    pub name: String,
    pub status: PreflightToolStatus,
    pub command_line: Option<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct PatchPreflightResult {
    pub passed: bool,
    pub integrity: SeriesIntegrity,
    pub checked_files: usize,
    pub expected_files: usize,
    pub missing_files: Vec<String>,
    pub checkpatch: PreflightToolResult,
    pub maintainers: PreflightToolResult,
    pub summary: String,
}

impl PatchPreflightResult {
    pub fn status_label(&self) -> &'static str {
        if self.passed { "passed" } else { "failed" }
    }

    pub fn timed_out(&self) -> bool {
        self.checkpatch.timed_out || self.maintainers.timed_out
    }

    fn command_line(&self) -> String {
        let commands = [&self.checkpatch, &self.maintainers]
            .into_iter()
            .filter_map(|tool| tool.command_line.as_deref())
            .collect::<Vec<_>>();
        if commands.is_empty() {
            "preflight (no local checker available)".to_string()
        } else {
            format!("preflight: {}", commands.join("; "))
        }
    }

    fn output(&self) -> String {
        let mut sections = Vec::new();
        for tool in [&self.checkpatch, &self.maintainers] {
            if !tool.output.trim().is_empty() {
                sections.push(format!("[{}]\n{}", tool.name, tool.output.trim()));
            }
        }
        sections.join("\n\n")
    }
}

impl PatchAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Download => "download",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesIntegrity {
    Complete,
    Missing,
    Duplicate,
    OutOfOrder,
    Invalid,
}

impl SeriesIntegrity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Missing => "missing",
            Self::Duplicate => "duplicate",
            Self::OutOfOrder => "out-of-order",
            Self::Invalid => "invalid",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Complete => "ready",
            Self::Missing => "missing",
            Self::Duplicate => "duplicate",
            Self::OutOfOrder => "out-of-order",
            Self::Invalid => "invalid",
        }
    }

    pub fn is_ready(self) -> bool {
        self == Self::Complete
    }
}

#[derive(Debug, Clone)]
pub struct SeriesItem {
    pub seq: u32,
    pub total: u32,
    pub mail_id: i64,
    pub message_id: String,
    pub subject: String,
    pub raw_path: Option<PathBuf>,
    pub sort_ord: usize,
}

#[derive(Debug, Clone)]
pub struct SeriesSummary {
    pub mailbox: String,
    pub thread_id: i64,
    pub version: u32,
    pub expected_total: u32,
    pub subject: String,
    pub author: String,
    pub anchor_message_id: String,
    pub items: Vec<SeriesItem>,
    pub missing_seq: Vec<u32>,
    pub duplicate_seq: Vec<u32>,
    pub out_of_order: bool,
    pub integrity: SeriesIntegrity,
    pub status: PatchSeriesStatus,
}

impl SeriesSummary {
    pub fn present_count(&self) -> usize {
        self.items.len()
    }

    pub fn status_label(&self) -> &'static str {
        status_to_label(self.status)
    }

    pub fn integrity_reason(&self) -> Option<String> {
        match self.integrity {
            SeriesIntegrity::Complete => None,
            SeriesIntegrity::Missing => Some(format!(
                "missing patch index: {}",
                format_seq(&self.missing_seq)
            )),
            SeriesIntegrity::Duplicate => Some(format!(
                "duplicate patch index: {}",
                format_seq(&self.duplicate_seq)
            )),
            SeriesIntegrity::OutOfOrder => {
                Some("patch order is out-of-order in thread".to_string())
            }
            SeriesIntegrity::Invalid => Some("no valid [PATCH vN M/N] sequence found".to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PatchActionResult {
    pub status: PatchSeriesStatus,
    pub summary: String,
    pub command_line: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_path: Option<PathBuf>,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedPatchSubject {
    version: u32,
    seq: u32,
    total: u32,
    title: String,
}

#[derive(Debug, Clone)]
struct CandidatePatch<'a> {
    row: &'a ThreadRow,
    parsed: ParsedPatchSubject,
    sort_ord: usize,
}

pub fn build_series_index(mailbox: &str, threads: &[ThreadRow]) -> HashMap<i64, SeriesSummary> {
    let mut rows_by_thread: HashMap<i64, Vec<(usize, &ThreadRow)>> = HashMap::new();
    for (index, row) in threads.iter().enumerate() {
        // Patch readiness is a thread-level property, so group rows first and
        // only analyze ordering/integrity within that conversation boundary.
        rows_by_thread
            .entry(row.thread_id)
            .or_default()
            .push((index, row));
    }

    let mut summaries = HashMap::new();
    for (thread_id, rows) in rows_by_thread {
        if let Some(summary) = analyze_thread_series(mailbox, thread_id, &rows) {
            summaries.insert(thread_id, summary);
        }
    }
    summaries
}

pub fn hydrate_series_statuses(
    path: &Path,
    mailbox: &str,
    summaries: &mut HashMap<i64, SeriesSummary>,
) -> Result<()> {
    // Recompute structure from mail every time, then overlay persisted workflow
    // state so stale database status cannot invent patch series that no longer
    // exist in the visible thread set.
    let thread_ids: Vec<i64> = summaries.keys().copied().collect();
    let statuses = patch_store::load_series_statuses(path, mailbox, &thread_ids)?;
    for (thread_id, status) in statuses {
        if let Some(summary) = summaries.get_mut(&thread_id) {
            summary.status = status;
        }
    }
    Ok(())
}

pub fn load_latest_report(
    database_path: &Path,
    mailbox: &str,
    thread_id: i64,
) -> Result<Option<patch_store::SeriesLatestReport>> {
    patch_store::load_latest_report(database_path, mailbox, thread_id)
}

pub fn run_preflight(
    runtime: &RuntimeConfig,
    summary: &SeriesSummary,
) -> Result<PatchPreflightResult> {
    let record = upsert_series_summary(runtime, summary)?;
    let (files, missing_files) = collect_preflight_files(summary);
    let checker_ready =
        summary.integrity.is_ready() && missing_files.is_empty() && !files.is_empty();

    let (checkpatch, maintainers) = if checker_ready {
        let mut checkpatch_args = vec![
            "--no-tree".to_string(),
            "--terse".to_string(),
            "--".to_string(),
        ];
        checkpatch_args.extend(files.iter().map(|path| path.display().to_string()));

        let mut maintainer_args = vec!["--no-git-fallback".to_string(), "--".to_string()];
        maintainer_args.extend(files.iter().map(|path| path.display().to_string()));

        (
            run_preflight_tool(runtime, "checkpatch.pl", &checkpatch_args, true),
            run_preflight_tool(runtime, "get_maintainer.pl", &maintainer_args, false),
        )
    } else {
        let reason = if !summary.integrity.is_ready() {
            summary
                .integrity_reason()
                .unwrap_or_else(|| "series is not complete".to_string())
        } else if !missing_files.is_empty() {
            "one or more raw patch files are missing".to_string()
        } else {
            "no raw patch files are available".to_string()
        };
        (
            skipped_preflight_tool("checkpatch.pl", &reason),
            skipped_preflight_tool("get_maintainer.pl", &reason),
        )
    };

    let passed = summary.integrity.is_ready()
        && missing_files.is_empty()
        && !files.is_empty()
        && !checkpatch.status.is_blocking()
        && !maintainers.status.is_blocking();
    let mut result = PatchPreflightResult {
        passed,
        integrity: summary.integrity,
        checked_files: files.len(),
        expected_files: summary.items.len(),
        missing_files,
        checkpatch,
        maintainers,
        summary: String::new(),
    };

    // Build the human-readable summary after the tool results are known so the
    // same line can be persisted and shown in the TUI status bar.
    result.summary = format!(
        "patch preflight {}: integrity={} files={}/{} checkpatch={} maintainers={}",
        result.status_label(),
        result.integrity.as_str(),
        result.checked_files,
        result.expected_files,
        result.checkpatch.status.as_str(),
        result.maintainers.status.as_str(),
    );

    let command_line = result.command_line();
    let output = truncate_output(&result.output());
    let last_error = (!result.passed).then(|| result.summary.clone());
    patch_store::update_series_result(
        &runtime.database_path,
        record.id,
        &patch_store::SeriesResultUpdate {
            status: summary.status,
            last_error,
            last_command: Some(command_line.clone()),
            last_exit_code: result.checkpatch.exit_code.or(result.maintainers.exit_code),
            last_stdout: Some(output.clone()),
            last_stderr: None,
            output_path: None,
        },
    )?;
    patch_store::insert_series_run(
        &runtime.database_path,
        &patch_store::SeriesRunRequest {
            series_id: record.id,
            action: "preflight".to_string(),
            command: command_line,
            status: result.status_label().to_string(),
            exit_code: result.checkpatch.exit_code.or(result.maintainers.exit_code),
            timed_out: result.timed_out(),
            summary: Some(result.summary.clone()),
            stdout: Some(output),
            stderr: None,
            output_path: None,
        },
    )?;

    Ok(result)
}

pub fn format_preflight_report(summary: &SeriesSummary, result: &PatchPreflightResult) -> String {
    let mut lines = vec![
        "patch preflight".to_string(),
        format!("  series: v{} {}", summary.version, summary.subject),
        format!("  integrity: {}", result.integrity.as_str()),
        format!(
            "  files: {}/{} checked",
            result.checked_files, result.expected_files
        ),
        format!("  checkpatch.pl: {}", result.checkpatch.status.as_str()),
        format!(
            "  get_maintainer.pl: {}",
            result.maintainers.status.as_str()
        ),
        format!("  result: {}", result.status_label()),
    ];
    if !result.missing_files.is_empty() {
        lines.push(format!(
            "  missing_files: {}",
            result.missing_files.join(", ")
        ));
    }
    for tool in [&result.checkpatch, &result.maintainers] {
        if let Some(command) = tool.command_line.as_deref() {
            lines.push(format!("  {} command: {}", tool.name, command));
        }
        if !tool.output.trim().is_empty() {
            lines.push(format!("  {} output:\n{}", tool.name, tool.output.trim()));
        }
    }
    lines.join("\n")
}

fn upsert_series_summary(
    runtime: &RuntimeConfig,
    summary: &SeriesSummary,
) -> Result<patch_store::SeriesRecord> {
    patch_store::upsert_series(
        &runtime.database_path,
        &patch_store::UpsertSeriesRequest {
            mailbox: summary.mailbox.clone(),
            thread_id: summary.thread_id,
            version: summary.version,
            expected_total: summary.expected_total,
            author: summary.author.clone(),
            subject: summary.subject.clone(),
            anchor_message_id: summary.anchor_message_id.clone(),
            integrity: summary.integrity.as_str().to_string(),
            missing_seq: summary.missing_seq.clone(),
            duplicate_seq: summary.duplicate_seq.clone(),
            out_of_order: summary.out_of_order,
            items: summary
                .items
                .iter()
                .map(|item| patch_store::UpsertSeriesItem {
                    seq: item.seq,
                    total: item.total,
                    mail_id: item.mail_id,
                    message_id: item.message_id.clone(),
                    subject: item.subject.clone(),
                    raw_path: item.raw_path.clone(),
                    sort_ord: item.sort_ord,
                })
                .collect(),
        },
    )
}

fn collect_preflight_files(summary: &SeriesSummary) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for item in &summary.items {
        let Some(path) = item.raw_path.as_ref() else {
            missing.push(format!("mail_id={}", item.mail_id));
            continue;
        };
        if path.is_file() {
            if seen.insert(path.clone()) {
                files.push(path.clone());
            }
        } else {
            missing.push(path.display().to_string());
        }
    }
    (files, missing)
}

fn run_preflight_tool(
    runtime: &RuntimeConfig,
    name: &str,
    args: &[String],
    nonzero_is_findings: bool,
) -> PreflightToolResult {
    let Some(program) = resolve_preflight_tool(runtime, name) else {
        return skipped_preflight_tool(name, "not found in configured kernel trees or PATH");
    };
    let command_line = render_preflight_command(&program, args);
    let working_dir = program
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "scripts"))
        .and_then(Path::parent)
        .or_else(|| runtime.kernel_trees.first().map(PathBuf::as_path));
    match b4::run_program(&program, args, PREFLIGHT_ACTION_TIMEOUT, working_dir) {
        Ok(output) => {
            let status = if output.timed_out {
                PreflightToolStatus::Failed
            } else if output.exit_code == Some(0) {
                PreflightToolStatus::Passed
            } else if nonzero_is_findings {
                PreflightToolStatus::Findings
            } else {
                PreflightToolStatus::Failed
            };
            let mut details = output.stdout;
            if !output.stderr.trim().is_empty() {
                if !details.trim().is_empty() {
                    details.push_str("\n\n[stderr]\n");
                }
                details.push_str(&output.stderr);
            }
            PreflightToolResult {
                name: name.to_string(),
                status,
                command_line: Some(output.command_line),
                exit_code: output.exit_code,
                timed_out: output.timed_out,
                output: truncate_output(&details),
            }
        }
        Err(error) => PreflightToolResult {
            name: name.to_string(),
            status: PreflightToolStatus::Failed,
            command_line: Some(command_line),
            exit_code: None,
            timed_out: false,
            output: error.to_string(),
        },
    }
}

fn skipped_preflight_tool(name: &str, reason: &str) -> PreflightToolResult {
    PreflightToolResult {
        name: name.to_string(),
        status: PreflightToolStatus::Skipped,
        command_line: None,
        exit_code: None,
        timed_out: false,
        output: reason.to_string(),
    }
}

fn resolve_preflight_tool(runtime: &RuntimeConfig, name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for tree in &runtime.kernel_trees {
        candidates.push(tree.join("scripts").join(name));
    }
    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("scripts").join(name));
    }
    if let Some(path_var) = env::var_os("PATH") {
        for directory in env::split_paths(&path_var) {
            candidates.push(directory.join(name));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn render_preflight_command(program: &Path, args: &[String]) -> String {
    let mut pieces = Vec::with_capacity(args.len() + 1);
    pieces.push(program.display().to_string());
    pieces.extend(args.iter().cloned());
    pieces.join(" ")
}

pub fn run_action(
    runtime: &RuntimeConfig,
    summary: &SeriesSummary,
    action: PatchAction,
) -> Result<PatchActionResult> {
    if !summary.integrity.is_ready() {
        let reason = summary
            .integrity_reason()
            .unwrap_or_else(|| "series is not ready".to_string());
        return Err(CriewError::new(
            ErrorCode::Command,
            format!("cannot {} series: {}", action.name(), reason),
        ));
    }

    let record = patch_store::upsert_series(
        &runtime.database_path,
        &patch_store::UpsertSeriesRequest {
            mailbox: summary.mailbox.clone(),
            thread_id: summary.thread_id,
            version: summary.version,
            expected_total: summary.expected_total,
            author: summary.author.clone(),
            subject: summary.subject.clone(),
            anchor_message_id: summary.anchor_message_id.clone(),
            integrity: summary.integrity.as_str().to_string(),
            missing_seq: summary.missing_seq.clone(),
            duplicate_seq: summary.duplicate_seq.clone(),
            out_of_order: summary.out_of_order,
            items: summary
                .items
                .iter()
                .map(|item| patch_store::UpsertSeriesItem {
                    seq: item.seq,
                    total: item.total,
                    mail_id: item.mail_id,
                    message_id: item.message_id.clone(),
                    subject: item.subject.clone(),
                    raw_path: item.raw_path.clone(),
                    sort_ord: item.sort_ord,
                })
                .collect(),
        },
    )?;

    // Mark the series as actively being worked on before spawning `b4` so the
    // UI reflects in-flight intent even if the external process later fails.
    patch_store::update_series_result(
        &runtime.database_path,
        record.id,
        &patch_store::SeriesResultUpdate {
            status: PatchSeriesStatus::Reviewing,
            last_error: None,
            last_command: None,
            last_exit_code: None,
            last_stdout: None,
            last_stderr: None,
            output_path: None,
        },
    )?;

    let working_dir = action_working_dir(runtime, action)?;
    let baseline_head = if matches!(action, PatchAction::Apply) {
        let Some(path) = working_dir.as_deref() else {
            return Err(CriewError::new(
                ErrorCode::Command,
                "apply requires [kernel].tree to be configured",
            ));
        };
        Some(resolve_git_head(path)?)
    } else {
        None
    };
    let mut output_path: Option<PathBuf> = None;
    let apply_artifacts_before = if matches!(action, PatchAction::Apply) {
        // Snapshot existing artifacts so we only relocate files created by this
        // apply run, not unrelated leftovers already present in the tree.
        working_dir
            .as_deref()
            .map(snapshot_apply_artifacts)
            .transpose()?
    } else {
        None
    };
    let args = action_args(runtime, summary, action, &mut output_path)?;
    let subcommand = action_subcommand(action);
    let fallback_command = fallback_command_line("b4", subcommand, &args);

    let output = match b4::run(
        runtime.b4_path.as_deref(),
        Some(&runtime.data_dir),
        subcommand,
        &args,
        B4_ACTION_TIMEOUT,
        working_dir.as_deref(),
    ) {
        Ok(output) => output,
        Err(error) => {
            let message = error.to_string();
            patch_store::update_series_result(
                &runtime.database_path,
                record.id,
                &patch_store::SeriesResultUpdate {
                    status: PatchSeriesStatus::Failed,
                    last_error: Some(message.clone()),
                    last_command: Some(fallback_command.clone()),
                    last_exit_code: None,
                    last_stdout: None,
                    last_stderr: None,
                    output_path: output_path.clone(),
                },
            )?;
            patch_store::insert_series_run(
                &runtime.database_path,
                &patch_store::SeriesRunRequest {
                    series_id: record.id,
                    action: action.name().to_string(),
                    command: fallback_command.clone(),
                    status: "failed".to_string(),
                    exit_code: None,
                    timed_out: false,
                    summary: Some(message.clone()),
                    stdout: None,
                    stderr: Some(message.clone()),
                    output_path: output_path.clone(),
                },
            )?;
            return Err(error);
        }
    };

    let apply_artifact_dir = if matches!(action, PatchAction::Apply) {
        if let (Some(path), Some(before)) =
            (working_dir.as_deref(), apply_artifacts_before.as_ref())
        {
            relocate_new_apply_artifacts(path, before, &runtime.patch_dir, summary)?
        } else {
            None
        }
    } else {
        None
    };
    if output_path.is_none() {
        output_path = apply_artifact_dir.clone();
    }

    let (mut status, mut summary_line) = map_b4_result(action, &output, output_path.as_deref());
    let mut applied_head_before: Option<String> = None;
    let mut applied_head_after: Option<String> = None;
    if matches!(action, PatchAction::Apply)
        && status == PatchSeriesStatus::Applied
        && let (Some(path), Some(before_head)) = (working_dir.as_deref(), baseline_head.as_deref())
    {
        // A zero exit code is not enough for apply: verify that git history
        // actually moved so "no-op success" does not look like a real apply.
        match resolve_git_head(path) {
            Ok(after_head) => {
                if after_head == *before_head {
                    status = PatchSeriesStatus::Failed;
                    summary_line =
                        "b4 shazam exited successfully, but git HEAD did not move (no new commit in git log)"
                            .to_string();
                } else {
                    applied_head_before = Some(before_head.to_string());
                    applied_head_after = Some(after_head);
                }
            }
            Err(error) => {
                status = PatchSeriesStatus::Failed;
                summary_line = format!("apply verification failed: {error}");
            }
        }
    }
    if matches!(action, PatchAction::Apply)
        && status == PatchSeriesStatus::Applied
        && let Some(path) = apply_artifact_dir.as_deref()
    {
        summary_line = format!("{summary_line}; artifacts moved to {}", path.display());
    }
    let command_line = output.command_line.clone();
    let truncated_stdout = truncate_output(&output.stdout);
    let truncated_stderr = truncate_output(&output.stderr);
    let last_error = if matches!(
        status,
        PatchSeriesStatus::Applied | PatchSeriesStatus::Reviewing
    ) {
        None
    } else {
        Some(summary_line.as_str())
    };

    patch_store::update_series_result(
        &runtime.database_path,
        record.id,
        &patch_store::SeriesResultUpdate {
            status,
            last_error: last_error.map(ToOwned::to_owned),
            last_command: Some(command_line.clone()),
            last_exit_code: output.exit_code,
            last_stdout: Some(truncated_stdout.clone()),
            last_stderr: Some(truncated_stderr.clone()),
            output_path: output_path.clone(),
        },
    )?;

    patch_store::insert_series_run(
        &runtime.database_path,
        &patch_store::SeriesRunRequest {
            series_id: record.id,
            action: action.name().to_string(),
            command: command_line.clone(),
            status: status_to_label(status).to_string(),
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            summary: Some(summary_line.clone()),
            stdout: Some(truncated_stdout),
            stderr: Some(truncated_stderr),
            output_path: output_path.clone(),
        },
    )?;

    Ok(PatchActionResult {
        status,
        summary: summary_line,
        command_line,
        exit_code: output.exit_code,
        timed_out: output.timed_out,
        output_path,
        head_before: applied_head_before,
        head_after: applied_head_after,
    })
}

pub fn undo_last_apply(
    runtime: &RuntimeConfig,
    before_head: &str,
    expected_current_head: &str,
) -> Result<String> {
    let working_dir = action_working_dir(runtime, PatchAction::Apply)?.ok_or_else(|| {
        CriewError::new(
            ErrorCode::Command,
            "apply undo requires [kernel].tree to be configured",
        )
    })?;
    let current_head = resolve_git_head(&working_dir)?;
    // Refuse to undo if HEAD no longer matches the previously applied commit,
    // otherwise we could silently discard unrelated user work.
    if current_head != expected_current_head {
        return Err(CriewError::new(
            ErrorCode::Command,
            format!(
                "cannot undo apply: expected HEAD {} but found {} in {}",
                expected_current_head,
                current_head,
                working_dir.display()
            ),
        ));
    }

    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(&working_dir)
        .arg("reset")
        .arg("--hard")
        .arg(before_head)
        .output()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Command,
                format!(
                    "failed to execute git reset --hard in {}",
                    working_dir.display()
                ),
                error,
            )
        })?;
    if !output.status.success() {
        let reason = first_non_empty_line(&String::from_utf8_lossy(&output.stderr))
            .or_else(|| first_non_empty_line(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_else(|| "git reset --hard returned non-zero exit code".to_string());
        return Err(CriewError::new(
            ErrorCode::Command,
            format!(
                "failed to undo apply in {}: {}",
                working_dir.display(),
                reason
            ),
        ));
    }

    let head_after_reset = resolve_git_head(&working_dir)?;
    if head_after_reset != before_head {
        return Err(CriewError::new(
            ErrorCode::Command,
            format!(
                "undo apply verification failed: expected {} but got {}",
                before_head, head_after_reset
            ),
        ));
    }
    Ok(head_after_reset)
}

fn analyze_thread_series(
    mailbox: &str,
    thread_id: i64,
    rows: &[(usize, &ThreadRow)],
) -> Option<SeriesSummary> {
    let mut parsed_candidates = Vec::new();
    for (sort_ord, row) in rows {
        if let Some(parsed) = parse_patch_subject(&row.subject) {
            parsed_candidates.push(CandidatePatch {
                row,
                parsed,
                sort_ord: *sort_ord,
            });
        }
    }

    if parsed_candidates.is_empty() {
        return None;
    }

    // When multiple rerolls exist in one thread, surface only the newest
    // version; mixing v1/v2 patches would produce misleading integrity results.
    let selected_version = parsed_candidates
        .iter()
        .map(|candidate| candidate.parsed.version)
        .max()
        .unwrap_or(1);
    let selected: Vec<CandidatePatch<'_>> = parsed_candidates
        .into_iter()
        .filter(|candidate| candidate.parsed.version == selected_version)
        .collect();

    if selected.is_empty() {
        return None;
    }

    let expected_total = selected
        .iter()
        .map(|candidate| candidate.parsed.total)
        .max()
        .unwrap_or(0);

    let mut by_seq: BTreeMap<u32, Vec<&CandidatePatch<'_>>> = BTreeMap::new();
    for candidate in &selected {
        if candidate.parsed.seq > 0 {
            by_seq
                .entry(candidate.parsed.seq)
                .or_default()
                .push(candidate);
        }
    }

    let duplicates: Vec<u32> = by_seq
        .iter()
        .filter_map(|(seq, values)| if values.len() > 1 { Some(*seq) } else { None })
        .collect();

    let items: Vec<SeriesItem> = by_seq
        .values()
        .filter_map(|values| values.iter().min_by_key(|candidate| candidate.sort_ord))
        .map(|candidate| SeriesItem {
            seq: candidate.parsed.seq,
            total: candidate.parsed.total,
            mail_id: candidate.row.mail_id,
            message_id: candidate.row.message_id.clone(),
            subject: candidate.row.subject.clone(),
            raw_path: candidate.row.raw_path.clone(),
            sort_ord: candidate.sort_ord,
        })
        .collect();

    let mut missing = Vec::new();
    if expected_total > 0 {
        for seq in 1..=expected_total {
            if !by_seq.contains_key(&seq) {
                missing.push(seq);
            }
        }
    }

    let mut ordered = selected
        .iter()
        .filter(|candidate| candidate.parsed.seq > 0)
        .collect::<Vec<_>>();
    ordered.sort_by_key(|candidate| candidate.sort_ord);
    let out_of_order = ordered
        .windows(2)
        .any(|pair| pair[1].parsed.seq < pair[0].parsed.seq);

    let anchor = selected
        .iter()
        .filter(|candidate| candidate.parsed.seq == 0)
        .min_by_key(|candidate| candidate.sort_ord)
        .or_else(|| {
            selected
                .iter()
                .filter(|candidate| candidate.parsed.seq == 1)
                .min_by_key(|candidate| candidate.sort_ord)
        })
        .or_else(|| selected.iter().min_by_key(|candidate| candidate.sort_ord))?;
    // Prefer the cover letter as the anchor when present because it usually has
    // the best series title; otherwise fall back to the first real patch/mail.

    let subject = if anchor.parsed.title.is_empty() {
        anchor.row.subject.trim().to_string()
    } else {
        anchor.parsed.title.clone()
    };
    let author = anchor.row.from_addr.trim().to_string();

    let integrity = if expected_total == 0 || items.is_empty() {
        SeriesIntegrity::Invalid
    } else if !duplicates.is_empty() {
        SeriesIntegrity::Duplicate
    } else if !missing.is_empty() {
        SeriesIntegrity::Missing
    } else if out_of_order {
        SeriesIntegrity::OutOfOrder
    } else {
        SeriesIntegrity::Complete
    };

    Some(SeriesSummary {
        mailbox: mailbox.to_string(),
        thread_id,
        version: selected_version,
        expected_total,
        subject,
        author,
        anchor_message_id: anchor.row.message_id.clone(),
        items,
        missing_seq: missing,
        duplicate_seq: duplicates,
        out_of_order,
        integrity,
        status: PatchSeriesStatus::New,
    })
}

pub(crate) fn subject_is_patch_related(subject: &str) -> bool {
    parse_patch_subject_with_reply_mode(subject, true).is_some()
}

fn parse_patch_subject(subject: &str) -> Option<ParsedPatchSubject> {
    parse_patch_subject_with_reply_mode(subject, false)
}

fn parse_patch_subject_with_reply_mode(
    subject: &str,
    allow_reply_or_forward_prefixes: bool,
) -> Option<ParsedPatchSubject> {
    let (trimmed, has_reply_or_forward_prefix) = strip_reply_or_forward_prefixes(subject);
    if has_reply_or_forward_prefix && !allow_reply_or_forward_prefixes {
        return None;
    }

    parse_patch_subject_core(trimmed)
}

fn strip_reply_or_forward_prefixes(subject: &str) -> (&str, bool) {
    let mut trimmed = subject.trim();
    let mut has_reply_or_forward_prefix = false;
    loop {
        let lowered = trimmed.to_ascii_lowercase();
        if let Some(rest) = lowered.strip_prefix("re:") {
            let consumed = trimmed.len() - rest.len();
            trimmed = trimmed[consumed..].trim_start();
            has_reply_or_forward_prefix = true;
            continue;
        }
        if let Some(rest) = lowered.strip_prefix("fwd:") {
            let consumed = trimmed.len() - rest.len();
            trimmed = trimmed[consumed..].trim_start();
            has_reply_or_forward_prefix = true;
            continue;
        }
        break;
    }

    (trimmed, has_reply_or_forward_prefix)
}

fn parse_patch_subject_core(trimmed: &str) -> Option<ParsedPatchSubject> {
    if !trimmed.starts_with('[') {
        return None;
    }
    let end = trimmed.find(']')?;
    let tag = &trimmed[1..end];

    let mut has_patch = false;
    let mut version = 1u32;
    let mut seq_total: Option<(u32, u32)> = None;

    for token in tag.split_whitespace() {
        let normalized = token
            .trim_matches(|character: char| character == ',' || character == ';')
            .to_ascii_lowercase();
        if normalized.contains("patch") {
            has_patch = true;
        }
        if let Some(parsed) = parse_version_token(&normalized) {
            version = parsed;
        }
        if let Some(parsed) = parse_seq_total_token(&normalized) {
            seq_total = Some(parsed);
        }
    }

    if !has_patch {
        return None;
    }

    let (seq, total) = seq_total.unwrap_or((1, 1));
    if total == 0 || (seq > total && seq != 0) {
        return None;
    }

    let title = trimmed[end + 1..].trim().to_string();
    Some(ParsedPatchSubject {
        version,
        seq,
        total,
        title,
    })
}

fn parse_version_token(token: &str) -> Option<u32> {
    let value = token.strip_prefix('v')?;
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    value.parse::<u32>().ok().filter(|parsed| *parsed > 0)
}

fn parse_seq_total_token(token: &str) -> Option<(u32, u32)> {
    let normalized = token.trim_matches(|character: char| {
        !character.is_ascii_digit() && character != '/' && character != '-'
    });
    let (left, right) = normalized.split_once('/')?;
    if left.is_empty() || right.is_empty() {
        return None;
    }
    if !left.chars().all(|character| character.is_ascii_digit())
        || !right.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let seq = left.parse::<u32>().ok()?;
    let total = right.parse::<u32>().ok()?;
    Some((seq, total))
}

fn action_args(
    runtime: &RuntimeConfig,
    summary: &SeriesSummary,
    action: PatchAction,
    output_path: &mut Option<PathBuf>,
) -> Result<Vec<String>> {
    let mut args = vec!["--no-parent".to_string()];

    if matches!(action, PatchAction::Download) {
        let dir = prepare_download_dir(&runtime.patch_dir, summary)?;
        let name = download_series_name(summary);
        args.push("-o".to_string());
        args.push(dir.display().to_string());
        args.push("-n".to_string());
        args.push(name);
        *output_path = Some(dir);
    }

    args.push(summary.anchor_message_id.clone());
    Ok(args)
}

fn action_working_dir(runtime: &RuntimeConfig, action: PatchAction) -> Result<Option<PathBuf>> {
    if !matches!(action, PatchAction::Apply) {
        return Ok(None);
    }

    let Some(path) = runtime.kernel_trees.first() else {
        return Err(CriewError::new(
            ErrorCode::Command,
            "apply requires [kernel].tree to be configured",
        ));
    };
    if !path.exists() {
        return Err(CriewError::new(
            ErrorCode::Command,
            format!(
                "apply requires existing [kernel].tree, but '{}' does not exist",
                path.display()
            ),
        ));
    }
    if !path.is_dir() {
        return Err(CriewError::new(
            ErrorCode::Command,
            format!(
                "apply requires [kernel].tree to be a directory, but '{}' is not",
                path.display()
            ),
        ));
    }

    Ok(Some(path.clone()))
}

fn prepare_download_dir(root: &Path, summary: &SeriesSummary) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // Include a time component so repeated downloads of the same series do not
    // clobber earlier exports the user may still want to inspect.
    let name = download_series_name(summary);
    let dir = root.join(format!("{name}-{nonce}"));
    fs::create_dir_all(&dir).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!("failed to create patch export directory {}", dir.display()),
            error,
        )
    })?;
    Ok(dir)
}

fn action_subcommand(action: PatchAction) -> &'static str {
    match action {
        PatchAction::Apply => "shazam",
        PatchAction::Download => "am",
    }
}

fn snapshot_apply_artifacts(root: &Path) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    let entries = fs::read_dir(root).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!("failed to inspect apply artifacts under {}", root.display()),
            error,
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            CriewError::with_source(
                ErrorCode::Io,
                format!(
                    "failed to inspect apply artifact entry under {}",
                    root.display()
                ),
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type().map_err(|error| {
            CriewError::with_source(
                ErrorCode::Io,
                format!(
                    "failed to inspect apply artifact type for {}",
                    entry.path().display()
                ),
                error,
            )
        })?;
        if is_apply_artifact_name(&name, file_type.is_dir()) {
            names.insert(name);
        }
    }

    Ok(names)
}

fn is_apply_artifact_name(name: &str, is_dir: bool) -> bool {
    if is_dir {
        return name.ends_with(".patches");
    }
    name.ends_with(".mbx") || name.ends_with(".cover")
}

fn relocate_new_apply_artifacts(
    root: &Path,
    before: &HashSet<String>,
    patch_root: &Path,
    summary: &SeriesSummary,
) -> Result<Option<PathBuf>> {
    let mut created: Vec<String> = snapshot_apply_artifacts(root)?
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    if created.is_empty() {
        return Ok(None);
    }
    created.sort();

    // Move apply artifacts under CRIEW-managed storage so later cleanups and
    // previews do not depend on temporary files remaining in the kernel tree.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let destination = patch_root
        .join(APPLY_ARTIFACTS_DIR)
        .join(format!("{}-{nonce}", download_series_name(summary)));
    fs::create_dir_all(&destination).map_err(|error| {
        CriewError::with_source(
            ErrorCode::Io,
            format!(
                "failed to create apply artifact directory {}",
                destination.display()
            ),
            error,
        )
    })?;

    for name in created {
        let source = root.join(&name);
        let target = destination.join(&name);
        fs::rename(&source, &target).map_err(|error| {
            CriewError::with_source(
                ErrorCode::Io,
                format!(
                    "failed to move apply artifact from {} to {}",
                    source.display(),
                    target.display()
                ),
                error,
            )
        })?;
    }

    Ok(Some(destination))
}

fn resolve_git_head(path: &Path) -> Result<String> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--verify")
        .arg("HEAD")
        .output()
        .map_err(|error| {
            CriewError::with_source(
                ErrorCode::Command,
                format!("failed to execute git rev-parse under {}", path.display()),
                error,
            )
        })?;

    if !output.status.success() {
        let reason = first_non_empty_line(&String::from_utf8_lossy(&output.stderr))
            .or_else(|| first_non_empty_line(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_else(|| "git rev-parse returned non-zero exit code".to_string());
        return Err(CriewError::new(
            ErrorCode::Command,
            format!(
                "apply requires a valid git repository at '{}': {}",
                path.display(),
                reason
            ),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    first_non_empty_line(&stdout).ok_or_else(|| {
        CriewError::new(
            ErrorCode::Command,
            format!(
                "failed to resolve git HEAD under '{}': empty output",
                path.display()
            ),
        )
    })
}

fn download_series_name(summary: &SeriesSummary) -> String {
    let mut slug = slugify_series_subject(&summary.subject);
    if slug.is_empty() {
        slug = format!("series-t{}", summary.thread_id);
    }
    format!("{slug}-v{}", summary.version)
}

fn slugify_series_subject(subject: &str) -> String {
    let mut value = String::new();
    let mut previous_is_dash = false;
    for character in subject.chars() {
        if character.is_ascii_alphanumeric() {
            value.push(character.to_ascii_lowercase());
            previous_is_dash = false;
            continue;
        }
        if !previous_is_dash && !value.is_empty() {
            value.push('-');
            previous_is_dash = true;
        }
    }
    while value.ends_with('-') {
        value.pop();
    }
    if value.len() > DOWNLOAD_NAME_MAX_CHARS {
        value.truncate(DOWNLOAD_NAME_MAX_CHARS);
        while value.ends_with('-') {
            value.pop();
        }
    }
    value
}

fn map_b4_result(
    action: PatchAction,
    output: &b4::B4CommandResult,
    output_path: Option<&Path>,
) -> (PatchSeriesStatus, String) {
    if output.timed_out {
        return (
            PatchSeriesStatus::Failed,
            format!(
                "b4 {} timed out after {}s",
                action.name(),
                B4_ACTION_TIMEOUT.as_secs()
            ),
        );
    }

    if output.exit_code == Some(0) {
        if matches!(action, PatchAction::Apply) {
            return (
                PatchSeriesStatus::Applied,
                "series applied by b4 shazam".to_string(),
            );
        }
        // Download keeps the series in a reviewable state because fetching
        // patches does not imply they were applied anywhere yet.
        if let Some(path) = output_path {
            return (
                PatchSeriesStatus::Reviewing,
                format!("series downloaded to {}", path.display()),
            );
        }
        return (
            PatchSeriesStatus::Reviewing,
            "series downloaded by b4 am".to_string(),
        );
    }

    let reason = first_non_empty_line(&output.stderr)
        .or_else(|| first_non_empty_line(&output.stdout))
        .unwrap_or_else(|| "unknown b4 error".to_string());
    if looks_like_conflict(&reason) {
        return (
            PatchSeriesStatus::Conflict,
            format!("series apply conflict: {}", reason),
        );
    }

    let exit_code = output
        .exit_code
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (
        PatchSeriesStatus::Failed,
        format!("b4 exited with {}: {}", exit_code, reason),
    )
}

fn looks_like_conflict(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    lowered.contains("conflict")
        || lowered.contains("does not apply")
        || lowered.contains("failed to apply")
        || lowered.contains("patch failed")
}

fn first_non_empty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

fn truncate_output(value: &str) -> String {
    if value.len() <= MAX_LOG_BYTES {
        return value.to_string();
    }
    // Persist a bounded amount of tool output so the DB stays readable and one
    // noisy command cannot bloat the patch history tables indefinitely.
    let mut truncated = value[..MAX_LOG_BYTES].to_string();
    truncated.push_str("\n<truncated>");
    truncated
}

fn fallback_command_line(command: &str, subcommand: &str, args: &[String]) -> String {
    let mut pieces = Vec::with_capacity(2 + args.len());
    pieces.push(command.to_string());
    pieces.push(subcommand.to_string());
    pieces.extend(args.iter().cloned());
    pieces.join(" ")
}

fn status_to_label(status: PatchSeriesStatus) -> &'static str {
    match status {
        PatchSeriesStatus::New => "new",
        PatchSeriesStatus::Reviewing => "reviewing",
        PatchSeriesStatus::Applied => "applied",
        PatchSeriesStatus::Failed => "failed",
        PatchSeriesStatus::Conflict => "conflict",
    }
}

fn format_seq(values: &[u32]) -> String {
    values
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command as ProcessCommand;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::{Connection, params};

    use crate::domain::models::PatchSeriesStatus;
    use crate::infra::config::RuntimeConfig;
    use crate::infra::db;
    use crate::infra::error::ErrorCode;
    use crate::infra::mail_store::ThreadRow;
    use crate::infra::patch_store;

    use super::{
        APPLY_ARTIFACTS_DIR, PatchAction, PreflightToolStatus, SeriesIntegrity, action_args,
        action_subcommand, action_working_dir, build_series_index, download_series_name,
        hydrate_series_statuses, load_latest_report, parse_patch_subject, parse_seq_total_token,
        parse_version_token, relocate_new_apply_artifacts, run_action, run_preflight,
        snapshot_apply_artifacts, subject_is_patch_related, undo_last_apply,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "criew-patch-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn runtime_with_kernel_trees(kernel_trees: Vec<PathBuf>) -> RuntimeConfig {
        let root = PathBuf::from("/tmp/criew-patch-runtime");
        RuntimeConfig {
            config_path: root.join("config.toml"),
            data_dir: root.join("data"),
            database_path: root.join("data/criew.db"),
            raw_mail_dir: root.join("data/raw"),
            patch_dir: root.join("data/patches"),
            log_dir: root.join("data/logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "io-uring".to_string(),
            imap: crate::infra::config::ImapConfig::default(),
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees,
        }
    }

    fn runtime_in(root: &Path, kernel_trees: Vec<PathBuf>) -> RuntimeConfig {
        RuntimeConfig {
            config_path: root.join("config.toml"),
            data_dir: root.join("data"),
            database_path: root.join("data").join("criew.db"),
            raw_mail_dir: root.join("data").join("raw"),
            patch_dir: root.join("data").join("patches"),
            log_dir: root.join("data").join("logs"),
            b4_path: None,
            log_filter: "info".to_string(),
            source_mailbox: "io-uring".to_string(),
            imap: crate::infra::config::ImapConfig::default(),
            lore_base_url: "https://lore.kernel.org".to_string(),
            startup_sync: true,
            ui_keymap: crate::infra::config::UiKeymap::Default,
            ui_keymap_base: crate::infra::config::UiKeymapBase::Default,
            ui_custom_keymap: crate::infra::config::UiCustomKeymapConfig::default(),
            inbox_auto_sync_interval_secs:
                crate::infra::config::DEFAULT_INBOX_AUTO_SYNC_INTERVAL_SECS,
            kernel_trees,
        }
    }

    fn run_git(repo: &PathBuf, args: &[&str]) {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(repo: &PathBuf, args: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, body).expect("write script");
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("mark executable");
        }
        path
    }

    fn seed_mail_rows(path: &Path, rows: &[(i64, &str)]) {
        let connection = Connection::open(path).expect("open db");
        for (id, message_id) in rows {
            connection
                .execute(
                    "
INSERT INTO mail(id, message_id, subject, from_addr, imap_mailbox, imap_uid)
VALUES (?1, ?2, ?3, ?4, 'io-uring', ?1)
",
                    params![id, message_id, format!("subject-{id}"), "alice@example.com"],
                )
                .expect("insert mail row");
        }
    }

    fn initialize_patch_runtime(runtime: &RuntimeConfig, mail_rows: &[(i64, &str)]) {
        if let Some(parent) = runtime.database_path.parent() {
            fs::create_dir_all(parent).expect("create db parent");
        }
        let _ = db::initialize(&runtime.database_path).expect("initialize db");
        seed_mail_rows(&runtime.database_path, mail_rows);
    }

    fn thread_row(
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
            subject: subject.to_string(),
            from_addr: "alice@example.com".to_string(),
            message_id: message_id.to_string(),
            in_reply_to: None,
            date: None,
            raw_path: Some(PathBuf::from(format!("/tmp/{message_id}.eml"))),
        }
    }

    fn sample_summary(subject: &str, version: u32) -> super::SeriesSummary {
        super::SeriesSummary {
            mailbox: "io-uring".to_string(),
            thread_id: 42,
            version,
            expected_total: 1,
            subject: subject.to_string(),
            author: "alice@example.com".to_string(),
            anchor_message_id: "patch@example.com".to_string(),
            items: vec![super::SeriesItem {
                seq: 1,
                total: 1,
                mail_id: 1,
                message_id: "patch@example.com".to_string(),
                subject: subject.to_string(),
                raw_path: None,
                sort_ord: 0,
            }],
            missing_seq: Vec::new(),
            duplicate_seq: Vec::new(),
            out_of_order: false,
            integrity: super::SeriesIntegrity::Complete,
            status: crate::domain::models::PatchSeriesStatus::New,
        }
    }

    #[test]
    fn integrity_helpers_match_user_visible_patch_status_contract() {
        let mut summary = sample_summary("[PATCH 1/1] io_uring: demo", 1);

        assert_eq!(summary.present_count(), 1);
        assert_eq!(summary.status_label(), "new");
        assert_eq!(SeriesIntegrity::Complete.as_str(), "complete");
        assert_eq!(SeriesIntegrity::Complete.short_label(), "ready");
        assert!(SeriesIntegrity::Complete.is_ready());
        assert_eq!(summary.integrity_reason(), None);

        summary.integrity = SeriesIntegrity::Missing;
        summary.missing_seq = vec![2, 4];
        assert_eq!(summary.integrity.as_str(), "missing");
        assert_eq!(summary.integrity.short_label(), "missing");
        assert!(!summary.integrity.is_ready());
        assert_eq!(
            summary.integrity_reason().as_deref(),
            Some("missing patch index: 2,4")
        );

        summary.integrity = SeriesIntegrity::Duplicate;
        summary.duplicate_seq = vec![1];
        assert_eq!(summary.integrity.as_str(), "duplicate");
        assert_eq!(summary.integrity.short_label(), "duplicate");
        assert_eq!(
            summary.integrity_reason().as_deref(),
            Some("duplicate patch index: 1")
        );

        summary.integrity = SeriesIntegrity::OutOfOrder;
        assert_eq!(summary.integrity.as_str(), "out-of-order");
        assert_eq!(summary.integrity.short_label(), "out-of-order");
        assert_eq!(
            summary.integrity_reason().as_deref(),
            Some("patch order is out-of-order in thread")
        );

        summary.integrity = SeriesIntegrity::Invalid;
        assert_eq!(summary.integrity.as_str(), "invalid");
        assert_eq!(summary.integrity.short_label(), "invalid");
        assert_eq!(
            summary.integrity_reason().as_deref(),
            Some("no valid [PATCH vN M/N] sequence found")
        );

        summary.status = PatchSeriesStatus::Conflict;
        assert_eq!(summary.status_label(), "conflict");
    }

    #[test]
    fn preflight_runs_kernel_checkers_and_persists_audit() {
        let root = temp_dir("preflight-checkers");
        let tree = root.join("linux");
        let scripts = tree.join("scripts");
        fs::create_dir_all(&scripts).expect("create scripts directory");
        write_script(
            &scripts,
            "checkpatch.pl",
            "#!/bin/sh\nprintf 'checkpatch inspected %s\\n' \"$4\"\nexit 0\n",
        );
        write_script(
            &scripts,
            "get_maintainer.pl",
            "#!/bin/sh\nprintf 'Alice Maintainer <alice@example.com>\\n'\nexit 0\n",
        );
        let raw_path = root.join("patch.eml");
        fs::write(&raw_path, "Subject: [PATCH 1/1] demo\n\nbody\n").expect("write raw patch");

        let runtime = runtime_in(&root, vec![tree]);
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);
        let mut summary = sample_summary("[PATCH 1/1] demo", 1);
        summary.items[0].raw_path = Some(raw_path);

        let result = run_preflight(&runtime, &summary).expect("run preflight");

        assert!(result.passed);
        assert_eq!(result.checked_files, 1);
        assert_eq!(result.checkpatch.status, PreflightToolStatus::Passed);
        assert_eq!(result.maintainers.status, PreflightToolStatus::Passed);
        assert!(result.maintainers.output.contains("Alice Maintainer"));
        assert!(result.summary.contains("checkpatch=passed"));
        let report = load_latest_report(&runtime.database_path, "io-uring", 42)
            .expect("load preflight report")
            .expect("preflight report should exist");
        assert!(
            report
                .last_summary
                .as_deref()
                .is_some_and(|summary| summary.contains("patch preflight passed"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preflight_blocks_incomplete_series_without_running_tools() {
        let root = temp_dir("preflight-incomplete");
        let runtime = runtime_in(&root, Vec::new());
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);
        let mut summary = sample_summary("[PATCH 1/2] demo", 1);
        summary.expected_total = 2;
        summary.integrity = SeriesIntegrity::Missing;
        summary.missing_seq = vec![2];

        let result = run_preflight(&runtime, &summary).expect("run preflight");

        assert!(!result.passed);
        assert_eq!(result.checkpatch.status, PreflightToolStatus::Skipped);
        assert_eq!(result.maintainers.status, PreflightToolStatus::Skipped);
        assert!(result.summary.contains("integrity=missing"));
        assert!(result.checkpatch.output.contains("missing patch index: 2"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hydrate_series_statuses_overlays_only_visible_threads() {
        let root = temp_dir("hydrate-status");
        let runtime = runtime_in(&root, Vec::new());
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);

        let series = patch_store::upsert_series(
            &runtime.database_path,
            &patch_store::UpsertSeriesRequest {
                mailbox: "io-uring".to_string(),
                thread_id: 42,
                version: 1,
                expected_total: 1,
                author: "Alice".to_string(),
                subject: "demo".to_string(),
                anchor_message_id: "patch@example.com".to_string(),
                integrity: "complete".to_string(),
                missing_seq: Vec::new(),
                duplicate_seq: Vec::new(),
                out_of_order: false,
                items: vec![patch_store::UpsertSeriesItem {
                    seq: 1,
                    total: 1,
                    mail_id: 1,
                    message_id: "patch@example.com".to_string(),
                    subject: "demo".to_string(),
                    raw_path: None,
                    sort_ord: 0,
                }],
            },
        )
        .expect("persist series");
        patch_store::update_series_result(
            &runtime.database_path,
            series.id,
            &patch_store::SeriesResultUpdate {
                status: PatchSeriesStatus::Applied,
                last_error: None,
                last_command: Some("b4 am patch@example.com".to_string()),
                last_exit_code: Some(0),
                last_stdout: Some("applied".to_string()),
                last_stderr: None,
                output_path: None,
            },
        )
        .expect("update series status");

        let mut visible_summaries = HashMap::from([
            (42, sample_summary("[PATCH 1/1] io_uring: demo", 1)),
            (
                99,
                super::SeriesSummary {
                    thread_id: 99,
                    status: PatchSeriesStatus::Failed,
                    ..sample_summary("[PATCH 1/1] io_uring: another", 1)
                },
            ),
        ]);

        hydrate_series_statuses(&runtime.database_path, "io-uring", &mut visible_summaries)
            .expect("hydrate statuses");

        assert_eq!(
            visible_summaries.get(&42).map(|summary| summary.status),
            Some(PatchSeriesStatus::Applied)
        );
        assert_eq!(
            visible_summaries.get(&99).map(|summary| summary.status),
            Some(PatchSeriesStatus::Failed)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_latest_report_returns_none_when_thread_has_no_patch_state() {
        let root = temp_dir("latest-report-none");
        let runtime = runtime_in(&root, Vec::new());
        initialize_patch_runtime(&runtime, &[]);

        let report =
            load_latest_report(&runtime.database_path, "io-uring", 42).expect("load latest report");
        assert!(report.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_action_rejects_incomplete_series_before_persisting_or_running_b4() {
        let root = temp_dir("reject-incomplete");
        let runtime = runtime_in(&root, Vec::new());
        let mut summary = sample_summary("[PATCH 1/2] io_uring: demo", 1);
        summary.integrity = SeriesIntegrity::Missing;
        summary.expected_total = 2;
        summary.missing_seq = vec![2];

        let error = run_action(&runtime, &summary, PatchAction::Apply).expect_err("reject apply");

        assert_eq!(error.code(), ErrorCode::Command);
        assert!(error.to_string().contains("cannot apply series"));
        assert!(error.to_string().contains("missing patch index: 2"));
        assert!(!runtime.database_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parses_patch_subject_with_version_and_seq() {
        let parsed = parse_patch_subject("[PATCH v5 2/9] io_uring: demo").expect("parsed");
        assert_eq!(parsed.version, 5);
        assert_eq!(parsed.seq, 2);
        assert_eq!(parsed.total, 9);
        assert_eq!(parsed.title, "io_uring: demo");
    }

    #[test]
    fn ignores_reply_or_forward_prefix_for_patch_series_detection() {
        assert!(parse_patch_subject("Re: [PATCH v5 2/9] io_uring: demo").is_none());
        assert!(parse_patch_subject("fwd: [PATCH 1/1] io_uring: demo").is_none());
    }

    #[test]
    fn patch_related_helper_keeps_patch_replies() {
        assert!(subject_is_patch_related("[PATCH v5 2/9] io_uring: demo"));
        assert!(subject_is_patch_related(
            "Re: [PATCH v5 2/9] io_uring: demo"
        ));
        assert!(subject_is_patch_related("fwd: [PATCH 1/1] io_uring: demo"));
        assert!(!subject_is_patch_related("Weekly status update"));
    }

    #[test]
    fn parse_token_helpers_handle_expected_variants() {
        assert_eq!(parse_version_token("v2"), Some(2));
        assert_eq!(parse_version_token("v"), None);
        assert_eq!(parse_seq_total_token("03/09"), Some((3, 9)));
        assert_eq!(parse_seq_total_token("1/x"), None);
    }

    #[test]
    fn apply_uses_b4_shazam_subcommand() {
        assert_eq!(action_subcommand(PatchAction::Apply), "shazam");
        assert_eq!(action_subcommand(PatchAction::Download), "am");
    }

    #[test]
    fn download_uses_patch_subject_in_export_name() {
        let root = temp_dir("download-name");
        let mut runtime = runtime_with_kernel_trees(Vec::new());
        runtime.patch_dir = root.clone();
        let summary = sample_summary("io_uring: keep PATCH thread name", 3);

        let mut output_path = None;
        let args = action_args(&runtime, &summary, PatchAction::Download, &mut output_path)
            .expect("download args");

        assert_eq!(
            download_series_name(&summary),
            "io-uring-keep-patch-thread-name-v3"
        );
        assert_eq!(args[4], "io-uring-keep-patch-thread-name-v3");
        assert!(
            output_path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("io-uring-keep-patch-thread-name-v3-"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_series_index_detects_missing_patch() {
        let rows = vec![
            thread_row(10, 1, "[PATCH v3 0/3] demo", "cover@example.com", 0),
            thread_row(10, 2, "[PATCH v3 1/3] a", "p1@example.com", 1),
            thread_row(10, 3, "[PATCH v3 3/3] c", "p3@example.com", 1),
        ];
        let index = build_series_index("io-uring", &rows);
        let series = index.get(&10).expect("series exists");
        assert_eq!(series.version, 3);
        assert_eq!(series.expected_total, 3);
        assert_eq!(series.present_count(), 2);
        assert_eq!(series.missing_seq, vec![2]);
        assert_eq!(series.integrity, SeriesIntegrity::Missing);
    }

    #[test]
    fn build_series_index_prefers_latest_version() {
        let rows = vec![
            thread_row(12, 1, "[PATCH v1 1/1] old", "old@example.com", 0),
            thread_row(12, 2, "[PATCH v2 1/2] new-a", "new-a@example.com", 0),
            thread_row(12, 3, "[PATCH v2 2/2] new-b", "new-b@example.com", 0),
        ];
        let index = build_series_index("io-uring", &rows);
        let series = index.get(&12).expect("series exists");
        assert_eq!(series.version, 2);
        assert_eq!(series.expected_total, 2);
        assert_eq!(series.integrity, SeriesIntegrity::Complete);
    }

    #[test]
    fn build_series_index_ignores_reply_subject_duplicate_indices() {
        let rows = vec![
            thread_row(13, 1, "[PATCH v1 1/1] demo", "patch@example.com", 0),
            thread_row(13, 2, "Re: [PATCH v1 1/1] demo", "reply@example.com", 1),
        ];
        let index = build_series_index("io-uring", &rows);
        let series = index.get(&13).expect("series exists");
        assert_eq!(series.present_count(), 1);
        assert!(series.duplicate_seq.is_empty());
        assert_eq!(series.integrity, SeriesIntegrity::Complete);
    }

    #[test]
    fn build_series_index_detects_duplicate_and_out_of_order_series() {
        let duplicate_rows = vec![
            thread_row(20, 1, "[PATCH v2 1/2] first", "a@example.com", 0),
            thread_row(20, 2, "[PATCH v2 1/2] reroll", "b@example.com", 1),
            thread_row(20, 3, "[PATCH v2 2/2] second", "c@example.com", 1),
        ];
        let duplicate_index = build_series_index("io-uring", &duplicate_rows);
        let duplicate = duplicate_index.get(&20).expect("duplicate series exists");
        assert_eq!(duplicate.integrity, SeriesIntegrity::Duplicate);
        assert_eq!(duplicate.duplicate_seq, vec![1]);
        assert_eq!(
            duplicate.integrity_reason().as_deref(),
            Some("duplicate patch index: 1")
        );
        assert_eq!(duplicate.integrity.short_label(), "duplicate");

        let out_of_order_rows = vec![
            thread_row(21, 1, "[PATCH v1 2/2] second", "p2@example.com", 0),
            thread_row(21, 2, "[PATCH v1 1/2] first", "p1@example.com", 1),
        ];
        let out_of_order_index = build_series_index("io-uring", &out_of_order_rows);
        let out_of_order = out_of_order_index
            .get(&21)
            .expect("out-of-order series exists");
        assert_eq!(out_of_order.integrity, SeriesIntegrity::OutOfOrder);
        assert!(out_of_order.out_of_order);
        assert_eq!(
            out_of_order.integrity_reason().as_deref(),
            Some("patch order is out-of-order in thread")
        );
    }

    #[test]
    fn build_series_index_marks_cover_only_series_invalid() {
        let rows = vec![thread_row(
            22,
            1,
            "[PATCH v4 0/2] io_uring: cover letter only",
            "cover@example.com",
            0,
        )];
        let index = build_series_index("io-uring", &rows);
        let series = index.get(&22).expect("series exists");
        assert_eq!(series.integrity, SeriesIntegrity::Invalid);
        assert!(series.items.is_empty());
        assert_eq!(series.anchor_message_id, "cover@example.com");
        assert_eq!(
            series.integrity_reason().as_deref(),
            Some("no valid [PATCH vN M/N] sequence found")
        );
    }

    #[test]
    fn apply_requires_kernel_tree_configuration() {
        let runtime = runtime_with_kernel_trees(Vec::new());
        let error = action_working_dir(&runtime, PatchAction::Apply).expect_err("should fail");
        assert!(error.to_string().contains("[kernel].tree"));
    }

    #[test]
    fn apply_requires_existing_kernel_tree_directory() {
        let root = temp_dir("apply-missing-tree");
        let missing = root.join("missing-tree");
        let runtime = runtime_in(&root, vec![missing.clone()]);

        let error = action_working_dir(&runtime, PatchAction::Apply).expect_err("missing tree");
        assert!(error.to_string().contains("does not exist"));

        let file_path = root.join("not-a-directory");
        fs::write(&file_path, "file").expect("write file");
        let runtime = runtime_in(&root, vec![file_path.clone()]);
        let error = action_working_dir(&runtime, PatchAction::Apply).expect_err("file tree");
        assert!(error.to_string().contains("is not"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn apply_uses_first_kernel_tree_directory() {
        let first = temp_dir("apply-first");
        let second = temp_dir("apply-second");
        let runtime = runtime_with_kernel_trees(vec![first.clone(), second]);

        let working_dir = action_working_dir(&runtime, PatchAction::Apply)
            .expect("resolve apply dir")
            .expect("apply should have working directory");
        assert_eq!(working_dir, first);
    }

    #[test]
    fn download_does_not_require_kernel_tree_directory() {
        let runtime = runtime_with_kernel_trees(Vec::new());
        let working_dir =
            action_working_dir(&runtime, PatchAction::Download).expect("download dir resolution");
        assert!(working_dir.is_none());
    }

    #[test]
    fn run_action_download_records_reviewing_report() {
        let root = temp_dir("run-action-download");
        let b4_script = write_script(
            &root,
            "fake-b4.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 0.14.0'\n  exit 0\nfi\nif [ \"$1\" = \"am\" ]; then\n  echo 'downloaded patch series'\n  exit 0\nfi\nexit 1\n",
        );
        let mut runtime = runtime_in(&root, Vec::new());
        runtime.b4_path = Some(b4_script);
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);
        let summary = sample_summary("io_uring: exported series", 2);

        let result = run_action(&runtime, &summary, PatchAction::Download).expect("download runs");

        assert_eq!(result.status, PatchSeriesStatus::Reviewing);
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert!(result.command_line.contains("am"));
        assert!(result.summary.starts_with("series downloaded to "));
        let output_path = result.output_path.expect("download path");
        assert!(output_path.starts_with(&runtime.patch_dir));
        assert!(output_path.is_dir());

        let latest = load_latest_report(&runtime.database_path, "io-uring", summary.thread_id)
            .expect("load latest report")
            .expect("report exists");
        assert_eq!(latest.status, PatchSeriesStatus::Reviewing);
        assert_eq!(latest.last_error, None);
        assert_eq!(latest.last_exit_code, Some(0));
        assert_eq!(
            latest.last_summary.as_deref(),
            Some(result.summary.as_str())
        );
        assert_eq!(
            latest.last_command.as_deref(),
            Some(result.command_line.as_str())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_action_apply_marks_conflicts_in_latest_report() {
        let root = temp_dir("run-action-conflict");
        let repo = root.join("linux");
        fs::create_dir_all(&repo).expect("create repo");
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.name", "CRIEW Test"]);
        run_git(&repo, &["config", "user.email", "criew@example.com"]);
        fs::write(repo.join("base.txt"), "base\n").expect("write base");
        run_git(&repo, &["add", "base.txt"]);
        run_git(&repo, &["commit", "-m", "base"]);

        let b4_script = write_script(
            &root,
            "fake-b4.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 0.14.0'\n  exit 0\nfi\nif [ \"$1\" = \"shazam\" ]; then\n  echo 'patch failed at 0001 demo' >&2\n  exit 1\nfi\nexit 1\n",
        );
        let mut runtime = runtime_in(&root, vec![repo.clone()]);
        runtime.b4_path = Some(b4_script);
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);
        let summary = sample_summary("io_uring: conflict series", 1);

        let result = run_action(&runtime, &summary, PatchAction::Apply).expect("apply runs");

        assert_eq!(result.status, PatchSeriesStatus::Conflict);
        assert_eq!(result.exit_code, Some(1));
        assert!(result.summary.contains("series apply conflict"));
        assert!(result.head_before.is_none());
        assert!(result.head_after.is_none());

        let latest = load_latest_report(&runtime.database_path, "io-uring", summary.thread_id)
            .expect("load latest report")
            .expect("report exists");
        assert_eq!(latest.status, PatchSeriesStatus::Conflict);
        assert_eq!(latest.last_error.as_deref(), Some(result.summary.as_str()));
        assert_eq!(
            latest.last_summary.as_deref(),
            Some(result.summary.as_str())
        );
        assert_eq!(latest.last_exit_code, Some(1));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_action_apply_rejects_noop_success_when_head_does_not_move() {
        let root = temp_dir("run-action-noop");
        let repo = root.join("linux");
        fs::create_dir_all(&repo).expect("create repo");
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.name", "CRIEW Test"]);
        run_git(&repo, &["config", "user.email", "criew@example.com"]);
        fs::write(repo.join("base.txt"), "base\n").expect("write base");
        run_git(&repo, &["add", "base.txt"]);
        run_git(&repo, &["commit", "-m", "base"]);

        let b4_script = write_script(
            &root,
            "fake-b4.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 0.14.0'\n  exit 0\nfi\nif [ \"$1\" = \"shazam\" ]; then\n  echo 'applied without commit'\n  exit 0\nfi\nexit 1\n",
        );
        let mut runtime = runtime_in(&root, vec![repo.clone()]);
        runtime.b4_path = Some(b4_script);
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);
        let summary = sample_summary("io_uring: noop series", 1);

        let result = run_action(&runtime, &summary, PatchAction::Apply).expect("apply runs");

        assert_eq!(result.status, PatchSeriesStatus::Failed);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.summary.contains("git HEAD did not move"));
        assert!(result.head_before.is_none());
        assert!(result.head_after.is_none());

        let latest = load_latest_report(&runtime.database_path, "io-uring", summary.thread_id)
            .expect("load latest report")
            .expect("report exists");
        assert_eq!(latest.status, PatchSeriesStatus::Failed);
        assert_eq!(latest.last_error.as_deref(), Some(result.summary.as_str()));
        assert_eq!(
            latest.last_summary.as_deref(),
            Some(result.summary.as_str())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_action_apply_records_head_change_and_moves_artifacts() {
        let root = temp_dir("run-action-apply-success");
        let repo = root.join("linux");
        fs::create_dir_all(&repo).expect("create repo");
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.name", "CRIEW Test"]);
        run_git(&repo, &["config", "user.email", "criew@example.com"]);
        fs::write(repo.join("base.txt"), "base\n").expect("write base");
        run_git(&repo, &["add", "base.txt"]);
        run_git(&repo, &["commit", "-m", "base"]);

        let b4_script = write_script(
            &root,
            "fake-b4.sh",
            "#!/bin/sh\nset -e\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 0.14.0'\n  exit 0\nfi\nif [ \"$1\" = \"shazam\" ]; then\n  printf 'applied by fake b4\\n' > applied.txt\n  git add applied.txt\n  git commit -m 'apply-series' >/dev/null 2>&1\n  printf 'mbx\\n' > demo-series.mbx\n  printf 'cover\\n' > demo-series.cover\n  exit 0\nfi\nexit 1\n",
        );
        let mut runtime = runtime_in(&root, vec![repo.clone()]);
        runtime.b4_path = Some(b4_script);
        initialize_patch_runtime(&runtime, &[(1, "patch@example.com")]);
        let summary = sample_summary("io_uring: applied series", 1);

        let result = run_action(&runtime, &summary, PatchAction::Apply).expect("apply runs");

        assert_eq!(result.status, PatchSeriesStatus::Applied);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.summary.contains("series applied by b4 shazam"));
        assert!(result.summary.contains("artifacts moved to"));
        let head_before = result.head_before.expect("head before");
        let head_after = result.head_after.expect("head after");
        assert_ne!(head_before, head_after);

        let output_path = result.output_path.expect("artifact path");
        assert!(output_path.starts_with(runtime.patch_dir.join(APPLY_ARTIFACTS_DIR)));
        assert!(output_path.join("demo-series.mbx").exists());
        assert!(output_path.join("demo-series.cover").exists());
        assert!(!repo.join("demo-series.mbx").exists());
        assert!(!repo.join("demo-series.cover").exists());

        let latest = load_latest_report(&runtime.database_path, "io-uring", summary.thread_id)
            .expect("load latest report")
            .expect("report exists");
        assert_eq!(latest.status, PatchSeriesStatus::Applied);
        assert_eq!(latest.last_error, None);
        assert_eq!(
            latest.last_summary.as_deref(),
            Some(result.summary.as_str())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn undo_last_apply_resets_head_to_previous_commit() {
        let repo = temp_dir("undo-apply");
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.name", "CRIEW Test"]);
        run_git(&repo, &["config", "user.email", "criew@example.com"]);

        fs::write(repo.join("demo.txt"), "v1\n").expect("write v1");
        run_git(&repo, &["add", "demo.txt"]);
        run_git(&repo, &["commit", "-m", "commit-1"]);
        let before_head = git_stdout(&repo, &["rev-parse", "--verify", "HEAD"]);

        fs::write(repo.join("demo.txt"), "v2\n").expect("write v2");
        run_git(&repo, &["add", "demo.txt"]);
        run_git(&repo, &["commit", "-m", "commit-2"]);
        let after_head = git_stdout(&repo, &["rev-parse", "--verify", "HEAD"]);

        let runtime = runtime_with_kernel_trees(vec![repo.clone()]);
        let reset_head =
            undo_last_apply(&runtime, &before_head, &after_head).expect("undo apply succeeds");
        assert_eq!(reset_head, before_head);
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--verify", "HEAD"]),
            before_head
        );

        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn undo_last_apply_rejects_head_mismatch() {
        let repo = temp_dir("undo-apply-mismatch");
        run_git(&repo, &["init"]);
        run_git(&repo, &["config", "user.name", "CRIEW Test"]);
        run_git(&repo, &["config", "user.email", "criew@example.com"]);

        fs::write(repo.join("demo.txt"), "v1\n").expect("write v1");
        run_git(&repo, &["add", "demo.txt"]);
        run_git(&repo, &["commit", "-m", "commit-1"]);
        let first = git_stdout(&repo, &["rev-parse", "--verify", "HEAD"]);

        fs::write(repo.join("demo.txt"), "v2\n").expect("write v2");
        run_git(&repo, &["add", "demo.txt"]);
        run_git(&repo, &["commit", "-m", "commit-2"]);
        let second = git_stdout(&repo, &["rev-parse", "--verify", "HEAD"]);

        let runtime = runtime_with_kernel_trees(vec![repo.clone()]);
        let error = undo_last_apply(&runtime, &first, &first).expect_err("must reject mismatch");
        assert!(error.to_string().contains("expected HEAD"));
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--verify", "HEAD"]),
            second
        );

        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn relocate_new_apply_artifacts_moves_mbx_and_cover_to_patch_dir() {
        let root = temp_dir("apply-artifacts-root");
        let patch_dir = temp_dir("apply-artifacts-dest");
        let summary = sample_summary("io_uring: demo", 1);

        fs::write(root.join("existing.mbx"), "old").expect("write existing");
        let before = snapshot_apply_artifacts(&root).expect("snapshot before");
        fs::write(root.join("new-series.mbx"), "new").expect("write new mbx");
        fs::write(root.join("new-series.cover"), "cover").expect("write new cover");

        let moved_dir = relocate_new_apply_artifacts(&root, &before, &patch_dir, &summary)
            .expect("relocate succeeds")
            .expect("moved directory");
        assert!(moved_dir.starts_with(patch_dir.join(APPLY_ARTIFACTS_DIR)));
        assert!(moved_dir.join("new-series.mbx").exists());
        assert!(moved_dir.join("new-series.cover").exists());
        assert!(root.join("existing.mbx").exists());
        assert!(!root.join("new-series.mbx").exists());

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(patch_dir);
    }
}
