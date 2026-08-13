//! Discovery and execution helpers for the external `b4` tool.
//!
//! Higher layers care about patch download/apply outcomes, not about PATH
//! probing or timeout loops, so this module isolates those process-management
//! details behind a small result type.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::b4_vendor;
use crate::infra::error::{CriewError, ErrorCode, Result};

const EXECUTABLE_BUSY_RETRY_ATTEMPTS: u8 = 50;
const EXECUTABLE_BUSY_RETRY_DELAY: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub struct B4Check {
    pub status: B4Status,
}

#[derive(Debug, Clone)]
pub enum B4Status {
    Available { path: PathBuf, version: String },
    Broken { path: PathBuf, reason: String },
    Missing,
}

#[derive(Debug, Clone)]
pub struct B4CommandResult {
    pub command_line: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

enum Candidate {
    Path(PathBuf),
    EmbeddedVendor(PathBuf),
    Program(String),
}

#[derive(Debug, Clone)]
struct ResolvedCommand {
    command: String,
    display_path: PathBuf,
}

pub fn check(configured_path: Option<&Path>, runtime_data_dir: Option<&Path>) -> B4Check {
    check_from_candidates(candidates(configured_path, runtime_data_dir))
}

fn check_from_candidates(candidates: Vec<Candidate>) -> B4Check {
    let mut last_failure: Option<(PathBuf, String)> = None;

    for candidate in candidates {
        match probe(&candidate) {
            Probe::Available { path, version, .. } => {
                return B4Check {
                    status: B4Status::Available { path, version },
                };
            }
            Probe::Missing => continue,
            Probe::Broken { path, reason } => {
                last_failure = Some((path, reason));
            }
        }
    }

    if let Some((path, reason)) = last_failure {
        B4Check {
            status: B4Status::Broken { path, reason },
        }
    } else {
        B4Check {
            status: B4Status::Missing,
        }
    }
}

pub fn run(
    configured_path: Option<&Path>,
    runtime_data_dir: Option<&Path>,
    subcommand: &str,
    args: &[String],
    timeout: Duration,
    working_dir: Option<&Path>,
) -> Result<B4CommandResult> {
    let resolved = resolve_command(configured_path, runtime_data_dir)?;
    run_with_resolved_command(&resolved, subcommand, args, timeout, working_dir)
}

/// Run an arbitrary local helper with the same timeout and output-capture
/// guarantees as `b4`.
///
/// Patch preflight uses this for kernel-tree scripts such as `checkpatch.pl`
/// and `get_maintainer.pl`; keeping process lifecycle here prevents each
/// caller from inventing a subtly different timeout or spawn-error policy.
pub fn run_program(
    program: &Path,
    args: &[String],
    timeout: Duration,
    working_dir: Option<&Path>,
) -> Result<B4CommandResult> {
    let resolved = ResolvedCommand {
        command: program.display().to_string(),
        display_path: program.to_path_buf(),
    };
    run_with_resolved_program(&resolved, args, timeout, working_dir)
}

fn run_with_resolved_command(
    resolved: &ResolvedCommand,
    subcommand: &str,
    args: &[String],
    timeout: Duration,
    working_dir: Option<&Path>,
) -> Result<B4CommandResult> {
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push(subcommand.to_string());
    full_args.extend(args.iter().cloned());
    run_with_resolved_program(resolved, &full_args, timeout, working_dir)
}

fn run_with_resolved_program(
    resolved: &ResolvedCommand,
    args: &[String],
    timeout: Duration,
    working_dir: Option<&Path>,
) -> Result<B4CommandResult> {
    let mut command = command_for_program(resolved.command.as_ref());
    if let Some(working_dir) = working_dir {
        command.current_dir(working_dir);
    }
    for arg in args {
        command.arg(arg);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let command_line = render_program_command_line(&resolved.command, args);
    let child = spawn_command_with_retry(&mut command).map_err(|error| {
        CriewError::with_source(
            ErrorCode::B4,
            format!(
                "failed to spawn external command '{}' ({})",
                command_line,
                resolved.display_path.display()
            ),
            error,
        )
    })?;

    let (output, timed_out) = wait_for_command(child, timeout).map_err(|error| match error {
        ProcessWaitError::Polling(error) => CriewError::with_source(
            ErrorCode::B4,
            format!(
                "failed while waiting for external command '{}'",
                command_line
            ),
            error,
        ),
        ProcessWaitError::Collecting(error) => CriewError::with_source(
            ErrorCode::B4,
            format!(
                "failed to collect output for external command '{}'",
                command_line
            ),
            error,
        ),
    })?;

    Ok(B4CommandResult {
        command_line,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
        timed_out,
    })
}

#[derive(Debug)]
enum ProcessWaitError {
    Polling(std::io::Error),
    Collecting(std::io::Error),
}

fn wait_for_command(
    mut child: std::process::Child,
    timeout: Duration,
) -> std::result::Result<(std::process::Output, bool), ProcessWaitError> {
    let started_at = Instant::now();
    let mut timed_out = false;

    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                // Poll with an explicit timeout so a wedged `b4` process cannot
                // freeze the TUI or patch workflow indefinitely.
                if started_at.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
            Err(error) => return Err(ProcessWaitError::Polling(error)),
        }
    }

    child
        .wait_with_output()
        .map(|output| (output, timed_out))
        .map_err(ProcessWaitError::Collecting)
}

fn candidates(configured_path: Option<&Path>, runtime_data_dir: Option<&Path>) -> Vec<Candidate> {
    let env_b4_path = env::var_os("CRIEW_B4_PATH").map(PathBuf::from);
    let cwd = env::current_dir().ok();
    candidates_with(
        configured_path,
        runtime_data_dir,
        env_b4_path.as_deref(),
        cwd.as_deref(),
    )
}

fn candidates_with(
    configured_path: Option<&Path>,
    runtime_data_dir: Option<&Path>,
    env_b4_path: Option<&Path>,
    cwd: Option<&Path>,
) -> Vec<Candidate> {
    let mut values = Vec::new();

    // Discovery order is from most explicit to most implicit so user config
    // wins over environment hints and PATH-based fallback.
    if let Some(path) = configured_path {
        values.push(Candidate::Path(path.to_path_buf()));
    }

    if let Some(path) = env_b4_path
        && !path.as_os_str().is_empty()
    {
        values.push(Candidate::Path(path.to_path_buf()));
    }

    if let Some(cwd) = cwd {
        values.push(Candidate::Path(cwd.join("vendor/b4/b4.sh")));
    }

    if let Some(data_dir) = runtime_data_dir {
        values.push(Candidate::EmbeddedVendor(data_dir.to_path_buf()));
    }

    values.push(Candidate::Program("b4".to_string()));

    values
}

enum Probe {
    Available {
        command: String,
        path: PathBuf,
        version: String,
    },
    Broken {
        path: PathBuf,
        reason: String,
    },
    Missing,
}

fn probe(candidate: &Candidate) -> Probe {
    match candidate {
        Candidate::Path(path) => {
            if !path.exists() {
                return Probe::Missing;
            }
            run_probe(path, path, path.display().to_string())
        }
        Candidate::EmbeddedVendor(data_dir) => {
            let script_path = b4_vendor::script_path(data_dir);
            match b4_vendor::ensure_installed(data_dir) {
                Ok(Some(path)) => run_probe(&path, &path, path.display().to_string()),
                Ok(None) => Probe::Missing,
                Err(error) => Probe::Broken {
                    path: script_path,
                    reason: error.to_string(),
                },
            }
        }
        Candidate::Program(program) => {
            let label = PathBuf::from(format!("{program} (PATH)"));
            run_probe(program, &label, program.clone())
        }
    }
}

fn spawn_command_with_retry(command: &mut Command) -> std::io::Result<std::process::Child> {
    retry_executable_busy(|| command.spawn())
}

fn run_probe<T>(command: T, label: &Path, command_value: String) -> Probe
where
    T: AsRef<std::ffi::OsStr>,
{
    let mut process = command_for_program(command.as_ref());
    process.arg("--version");
    match output_with_retry(&mut process) {
        Ok(output) if output.status.success() => {
            let version = normalize_output(&output.stdout)
                .or_else(|| normalize_output(&output.stderr))
                .unwrap_or_else(|| "unknown".to_string());

            Probe::Available {
                command: command_value,
                path: label.to_path_buf(),
                version,
            }
        }
        Ok(output) => {
            let reason = normalize_output(&output.stderr)
                .or_else(|| normalize_output(&output.stdout))
                .unwrap_or_else(|| format!("exit status {}", output.status));

            Probe::Broken {
                path: label.to_path_buf(),
                reason,
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Probe::Missing,
        Err(error) => Probe::Broken {
            path: label.to_path_buf(),
            reason: error.to_string(),
        },
    }
}

/// Windows does not execute Unix shell scripts directly.  CRIEW ships and
/// commonly discovers `b4.sh`, so use the user's available POSIX shell as a
/// transparent launcher while preserving the original path in diagnostics.
fn command_for_program(program: &std::ffi::OsStr) -> Command {
    #[cfg(windows)]
    {
        let path = Path::new(program);
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sh"))
        {
            let mut command = Command::new("sh");
            command.arg(program);
            return command;
        }
    }

    Command::new(program)
}

fn output_with_retry(command: &mut Command) -> std::io::Result<std::process::Output> {
    retry_executable_busy(|| command.output())
}

fn retry_executable_busy<T, F>(mut operation: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    let mut attempts_remaining = EXECUTABLE_BUSY_RETRY_ATTEMPTS;

    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_retryable_executable_busy(&error) && attempts_remaining > 0 => {
                attempts_remaining -= 1;
                thread::sleep(EXECUTABLE_BUSY_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_retryable_executable_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ExecutableFileBusy
}

fn resolve_command(
    configured_path: Option<&Path>,
    runtime_data_dir: Option<&Path>,
) -> Result<ResolvedCommand> {
    resolve_from_candidates(candidates(configured_path, runtime_data_dir))
}

fn resolve_from_candidates(candidates: Vec<Candidate>) -> Result<ResolvedCommand> {
    let mut last_failure: Option<(PathBuf, String)> = None;
    for candidate in candidates {
        match probe(&candidate) {
            Probe::Available { command, path, .. } => {
                return Ok(ResolvedCommand {
                    command,
                    display_path: path,
                });
            }
            Probe::Broken { path, reason } => {
                last_failure = Some((path, reason));
            }
            Probe::Missing => {}
        }
    }

    if let Some((path, reason)) = last_failure {
        // Report the last broken candidate explicitly because "not found" would
        // hide a misconfigured path that the user can actually fix.
        return Err(CriewError::new(
            ErrorCode::B4,
            format!("b4 executable '{}' is broken: {}", path.display(), reason),
        ));
    }

    Err(CriewError::new(
        ErrorCode::B4,
        "b4 executable not found (checked config path, CRIEW_B4_PATH, ./vendor/b4/b4.sh, embedded runtime vendor, and PATH)",
    ))
}

#[cfg(test)]
fn render_command_line(command: &str, subcommand: &str, args: &[String]) -> String {
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push(subcommand.to_string());
    full_args.extend(args.iter().cloned());
    render_program_command_line(command, &full_args)
}

fn render_program_command_line(command: &str, args: &[String]) -> String {
    let mut pieces = Vec::with_capacity(1 + args.len());
    pieces.push(render_shell_token(command));
    for arg in args {
        pieces.push(render_shell_token(arg));
    }
    pieces.join(" ")
}

fn render_shell_token(token: &str) -> String {
    if token.is_empty() {
        return "''".to_string();
    }
    if token
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_-./\\:@".contains(character))
    {
        return token.to_string();
    }
    format!("'{}'", token.replace('\'', "'\\''"))
}

fn normalize_output(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::infra::error::ErrorCode;

    use super::{
        B4Status, Candidate, Probe, candidates_with, check_from_candidates, normalize_output,
        probe, render_command_line, resolve_from_candidates, run_with_resolved_command,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("criew-b4-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        let staging_path = root.join(format!(".{name}.tmp"));
        let mut staging_file = fs::File::create(&staging_path).expect("create staging script");
        staging_file
            .write_all(body.as_bytes())
            .expect("write staging script");
        staging_file.sync_all().expect("sync staging script");
        drop(staging_file);
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&staging_path)
                .expect("staging metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&staging_path, permissions).expect("mark executable");
        }
        fs::rename(&staging_path, &path).expect("install script");
        path
    }

    fn canonicalize_existing_path(path: &Path) -> PathBuf {
        let path = normalize_shell_path(path);
        fs::canonicalize(&path).unwrap_or(path)
    }

    #[cfg(windows)]
    fn normalize_shell_path(path: &Path) -> PathBuf {
        let text = path.to_string_lossy();
        if let Some(relative) = text.strip_prefix("/tmp/") {
            return std::env::temp_dir().join(relative);
        }
        if text.len() >= 3 && text.starts_with('/') && text.as_bytes()[2] == b'/' {
            let drive = text.as_bytes()[1] as char;
            let relative = text[3..].replace('/', "\\");
            return PathBuf::from(format!("{}:\\{relative}", drive.to_ascii_uppercase()));
        }
        path.to_path_buf()
    }

    #[cfg(not(windows))]
    fn normalize_shell_path(path: &Path) -> PathBuf {
        path.to_path_buf()
    }

    fn canonicalized_path_from_output(output: &str, prefix: &str) -> PathBuf {
        let raw_path = output
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("missing {prefix} line in output: {output}"));
        canonicalize_existing_path(Path::new(raw_path))
    }

    #[test]
    fn check_prefers_available_configured_script() {
        let root = temp_dir("configured-ok");
        let configured_script = write_script(
            &root,
            "b4-ok.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 1.2.3'\n  exit 0\nfi\nexit 0\n",
        );
        let fallback_script = write_script(
            &root,
            "b4-fallback.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 9.9.9'\n  exit 0\nfi\nexit 0\n",
        );

        let result = check_from_candidates(vec![
            Candidate::Path(configured_script.clone()),
            Candidate::Path(fallback_script),
        ]);

        match result.status {
            B4Status::Available { path, version } => {
                assert_eq!(path, configured_script);
                assert_eq!(version, "b4 1.2.3");
            }
            status => panic!("expected available status, got {status:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn check_reports_broken_configured_script_when_no_fallback_exists() {
        let root = temp_dir("configured-broken");
        let script = write_script(
            &root,
            "b4-broken.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'broken runtime' >&2\n  exit 1\nfi\nexit 1\n",
        );

        let result = check_from_candidates(vec![Candidate::Path(script.clone())]);

        match result.status {
            B4Status::Broken { path, reason } => {
                assert_eq!(path, script);
                assert_eq!(reason, "broken runtime");
            }
            status => panic!("expected broken status, got {status:?}"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_executes_configured_script_in_requested_workdir() {
        let root = temp_dir("run-ok");
        let workdir = root.join("workdir");
        fs::create_dir_all(&workdir).expect("create workdir");
        let script = write_script(
            &root,
            "b4-run.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 2.0.0'\n  exit 0\nfi\nprintf 'cwd=%s\\n' \"$PWD\"\nprintf 'subcommand=%s\\n' \"$1\"\nprintf 'arg1=%s\\n' \"$2\"\nprintf 'arg2=%s\\n' \"$3\"\nprintf 'stderr-line\\n' >&2\n",
        );

        let resolved = resolve_from_candidates(vec![Candidate::Path(script.clone())])
            .expect("resolve configured b4");
        let result = run_with_resolved_command(
            &resolved,
            "am",
            &["--foo".to_string(), "bar baz".to_string()],
            Duration::from_secs(1),
            Some(&workdir),
        )
        .expect("run b4");

        assert_eq!(result.exit_code, Some(0));
        assert!(!result.timed_out);
        assert_eq!(
            result.command_line,
            format!("{} am --foo 'bar baz'", script.display())
        );
        assert_eq!(
            canonicalized_path_from_output(&result.stdout, "cwd="),
            canonicalize_existing_path(&workdir)
        );
        assert!(result.stdout.contains("subcommand=am"));
        assert!(result.stdout.contains("arg1=--foo"));
        assert!(result.stdout.contains("arg2=bar baz"));
        assert!(result.stderr.contains("stderr-line"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_marks_timed_out_processes() {
        let root = temp_dir("run-timeout");
        let script = write_script(
            &root,
            "b4-timeout.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo 'b4 2.1.0'\n  exit 0\nfi\nwhile :; do :; done\n",
        );

        let resolved =
            resolve_from_candidates(vec![Candidate::Path(script)]).expect("resolve timeout b4");
        let result =
            run_with_resolved_command(&resolved, "am", &[], Duration::from_millis(10), None)
                .expect("run b4");

        assert!(result.timed_out);
        assert_ne!(result.exit_code, Some(0));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn candidates_include_config_env_cwd_runtime_and_program_in_order() {
        let root = temp_dir("candidates");
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).expect("create cwd");
        let configured = root.join("configured-b4");
        let env_path = root.join("env-b4");
        let runtime = root.join("runtime");

        let values = candidates_with(
            Some(&configured),
            Some(&runtime),
            Some(&env_path),
            Some(&cwd),
        );

        assert!(matches!(&values[0], Candidate::Path(path) if path == &configured));
        assert!(matches!(&values[1], Candidate::Path(path) if path == &env_path));
        assert!(
            matches!(&values[2], Candidate::Path(path) if path == &cwd.join("vendor/b4/b4.sh"))
        );
        assert!(matches!(&values[3], Candidate::EmbeddedVendor(path) if path == &runtime));
        assert!(matches!(&values[4], Candidate::Program(program) if program == "b4"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn probe_reports_missing_and_embedded_vendor_failures() {
        let missing = probe(&Candidate::Path(PathBuf::from("/definitely/missing/b4")));
        assert!(matches!(missing, Probe::Missing));

        let root = temp_dir("embedded-broken");
        let vendor_root = root.join("vendor");
        fs::write(&vendor_root, "not a directory").expect("block vendor root");

        let broken = probe(&Candidate::EmbeddedVendor(root.clone()));
        match broken {
            Probe::Broken { path, reason } => {
                assert_eq!(path, root.join("vendor/b4/b4.sh"));
                assert!(reason.contains("failed to create embedded b4 directory"));
            }
            _ => panic!("expected broken embedded vendor probe"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_command_reports_not_found_without_candidates() {
        let error = resolve_from_candidates(vec![
            Candidate::Path(PathBuf::from("/definitely/missing/b4")),
            Candidate::Program("criew-b4-definitely-missing".to_string()),
        ])
        .expect_err("missing b4 should fail");

        assert_eq!(error.code(), ErrorCode::B4);
        assert!(error.to_string().contains("b4 executable not found"));
    }

    #[test]
    fn render_command_line_quotes_special_tokens() {
        let rendered = render_command_line(
            "/tmp/demo path/b4.sh",
            "am",
            &["bar baz".to_string(), "quote'char".to_string()],
        );

        assert_eq!(
            rendered,
            "'/tmp/demo path/b4.sh' am 'bar baz' 'quote'\\''char'"
        );
    }

    #[test]
    fn normalize_output_returns_first_non_empty_trimmed_line() {
        assert_eq!(
            normalize_output(b"\n  \n  b4 3.0.0  \nnext line\n"),
            Some("b4 3.0.0".to_string())
        );
        assert_eq!(normalize_output(b"\n\t \n"), None);
    }
}
