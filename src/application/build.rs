use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    adapters::{ProcessError, ProcessOutput, ProcessTermination, run_grouped},
    config::BuildCommand,
};

const MAX_GIT_CHANGES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitWorktreeState {
    Clean,
    Dirty {
        changes: Vec<String>,
        truncated: bool,
    },
    NotRepository,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitMetadata {
    pub branch: Option<String>,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildReport {
    pub git: GitWorktreeState,
    pub git_metadata: GitMetadata,
    pub working_directory: PathBuf,
    pub commands: Vec<ProcessOutput>,
}

/// Runs a Component build after path and Git preflight checks.
///
/// A dirty repository is reported for explicit TUI confirmation unless
/// `allow_dirty` is true. Non-Git project directories remain supported.
///
/// # Errors
///
/// Returns an error for unsafe paths, unavailable Git, dirty worktree without
/// confirmation, process failures, cancellation, timeout, or non-zero exit.
pub async fn run_build(
    project_root: &Path,
    working_directory: &Path,
    commands: &[BuildCommand],
    allow_dirty: bool,
    command_timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<BuildReport, BuildError> {
    run_build_with_output(
        project_root,
        working_directory,
        commands,
        allow_dirty,
        command_timeout,
        cancellation,
        &|_, _, _| {},
    )
    .await
}

/// Builds with bounded, caller-managed live output projection.
///
/// # Errors
/// Returns the same preflight and command errors as `run_build`.
#[allow(clippy::too_many_arguments)]
pub async fn run_build_with_output(
    project_root: &Path,
    working_directory: &Path,
    commands: &[BuildCommand],
    allow_dirty: bool,
    command_timeout: Duration,
    cancellation: &CancellationToken,
    observe: &(dyn Fn(usize, crate::adapters::OutputStream, &[u8]) + Sync),
) -> Result<BuildReport, BuildError> {
    run_build_with_events(
        project_root,
        working_directory,
        commands,
        allow_dirty,
        command_timeout,
        cancellation,
        observe,
        &|_, _| {},
    )
    .await
}

/// Reports a failed command from the executing loop, including spawn/I/O errors.
/// The callback is diagnostic only and must not execute or retry the command.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_build_with_events(
    project_root: &Path,
    working_directory: &Path,
    commands: &[BuildCommand],
    allow_dirty: bool,
    command_timeout: Duration,
    cancellation: &CancellationToken,
    observe: &(dyn Fn(usize, crate::adapters::OutputStream, &[u8]) + Sync),
    failed: &(dyn Fn(usize, &BuildCommand) + Sync),
) -> Result<BuildReport, BuildError> {
    let (root, working_directory) = resolve_working_directory(project_root, working_directory)?;
    let git = inspect_git(&root, command_timeout, cancellation).await?;
    if matches!(git, GitWorktreeState::Dirty { .. }) && !allow_dirty {
        return Err(BuildError::DirtyConfirmationRequired(git));
    }
    let git_metadata = inspect_git_metadata(&root, &git, command_timeout, cancellation).await?;
    let mut outputs = Vec::with_capacity(commands.len());
    for (index, command) in commands.iter().enumerate() {
        let output = crate::adapters::run_grouped_with_output(
            command,
            &working_directory,
            command_timeout,
            cancellation,
            &|stream, bytes| observe(index, stream, bytes),
        )
        .await;
        // Empty chunks mark command stream completion, including failure.
        observe(index, crate::adapters::OutputStream::Stdout, &[]);
        observe(index, crate::adapters::OutputStream::Stderr, &[]);
        observe_command_failure(index, command, &output, failed);
        let output = output?;
        match (output.termination, output.exit_code) {
            (ProcessTermination::Exited, Some(0)) => outputs.push(output),
            (ProcessTermination::Cancelled, _) => {
                return Err(BuildError::Cancelled { index, output });
            }
            (ProcessTermination::TimedOut, _) => {
                return Err(BuildError::TimedOut { index, output });
            }
            _ => return Err(BuildError::CommandFailed { index, output }),
        }
    }
    Ok(BuildReport {
        git,
        git_metadata,
        working_directory,
        commands: outputs,
    })
}

fn observe_command_failure(
    index: usize,
    command: &BuildCommand,
    result: &Result<ProcessOutput, ProcessError>,
    failed: &(dyn Fn(usize, &BuildCommand) + Sync),
) {
    if !result.as_ref().is_ok_and(|output| {
        output.termination == ProcessTermination::Exited && output.exit_code == Some(0)
    }) {
        failed(index, command);
    }
}

pub(super) fn command_snapshot(
    index: usize,
    command: &BuildCommand,
) -> crate::telemetry::log_record::RecordedCommand {
    use crate::telemetry::log_record::{CommandLocation, RecordedCommand};

    let (program, args) = if command.shell {
        #[cfg(windows)]
        let invocation = (
            "cmd.exe".into(),
            vec![
                "/D".into(),
                "/S".into(),
                "/C".into(),
                command.program.clone(),
            ],
        );
        #[cfg(not(windows))]
        let invocation = ("sh".into(), vec!["-c".into(), command.program.clone()]);
        invocation
    } else {
        (command.program.clone(), command.args.clone())
    };
    RecordedCommand {
        location: CommandLocation::Local,
        index: index
            .checked_add(1)
            .and_then(|index| u32::try_from(index).ok()),
        program,
        args,
    }
}

/// Inspects the Project Git worktree without changing it.
///
/// # Errors
///
/// Returns an error when Git cannot spawn, times out, is cancelled, or fails
/// for a reason other than the directory not being a repository.
pub async fn inspect_git(
    project_root: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<GitWorktreeState, BuildError> {
    let command = BuildCommand::argv(
        "git",
        ["status", "--porcelain=v1", "--untracked-files=normal"],
    );
    let output = run_grouped(&command, project_root, timeout, cancellation)
        .await
        .map_err(BuildError::GitProcess)?;
    match (output.termination, output.exit_code) {
        (ProcessTermination::Cancelled, _) => Err(BuildError::GitCancelled),
        (ProcessTermination::TimedOut, _) => Err(BuildError::GitTimedOut),
        (ProcessTermination::Exited, Some(0)) => Ok(parse_git_status(&output)),
        (ProcessTermination::Exited, Some(128))
            if String::from_utf8_lossy(&output.stderr)
                .to_ascii_lowercase()
                .contains("not a git repository") =>
        {
            Ok(GitWorktreeState::NotRepository)
        }
        _ => Err(BuildError::GitFailed(output)),
    }
}

/// Reads branch and exact HEAD identity after a successful worktree inspection.
///
/// # Errors
/// Returns an error for failed, cancelled, truncated, or malformed Git output.
pub async fn inspect_git_metadata(
    project_root: &Path,
    state: &GitWorktreeState,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<GitMetadata, BuildError> {
    if *state == GitWorktreeState::NotRepository {
        return Ok(GitMetadata::default());
    }
    let branch = git_optional_value(
        project_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        timeout,
        cancellation,
    )
    .await?;
    let revision = git_optional_value(
        project_root,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        timeout,
        cancellation,
    )
    .await?;
    if revision.as_ref().is_some_and(|value| {
        !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(BuildError::InvalidGitMetadata);
    }
    Ok(GitMetadata { branch, revision })
}

async fn git_optional_value(
    project_root: &Path,
    args: &[&str],
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Option<String>, BuildError> {
    let output = run_grouped(
        &BuildCommand::argv("git", args.iter().copied()),
        project_root,
        timeout,
        cancellation,
    )
    .await
    .map_err(BuildError::GitProcess)?;
    match (output.termination, output.exit_code) {
        (ProcessTermination::Cancelled, _) => Err(BuildError::GitCancelled),
        (ProcessTermination::TimedOut, _) => Err(BuildError::GitTimedOut),
        (ProcessTermination::Exited, Some(1)) if output.stdout.is_empty() => Ok(None),
        (ProcessTermination::Exited, Some(0)) if !output.stdout_truncated => {
            let value = std::str::from_utf8(&output.stdout)
                .map_err(|_| BuildError::InvalidGitMetadata)?
                .trim_end_matches(['\r', '\n']);
            if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
                return Err(BuildError::InvalidGitMetadata);
            }
            Ok(Some(value.to_owned()))
        }
        _ => Err(BuildError::GitFailed(output)),
    }
}

fn resolve_working_directory(
    project_root: &Path,
    working_directory: &Path,
) -> Result<(PathBuf, PathBuf), BuildError> {
    let root = project_root
        .canonicalize()
        .map_err(|source| BuildError::Path {
            path: project_root.to_owned(),
            source,
        })?;
    let candidate = root.join(working_directory);
    let working = candidate
        .canonicalize()
        .map_err(|source| BuildError::Path {
            path: candidate,
            source,
        })?;
    if !working.starts_with(&root) || !working.is_dir() {
        return Err(BuildError::UnsafeWorkingDirectory(working));
    }
    Ok((root, working))
}

pub(super) fn check_build_inputs(
    project_root: &Path,
    working_directory: &Path,
    commands: &[BuildCommand],
) -> Result<(), BuildError> {
    let (_, working_directory) = resolve_working_directory(project_root, working_directory)?;
    for command in commands {
        let program = if command.shell {
            if cfg!(windows) { "cmd.exe" } else { "sh" }
        } else {
            command.program.as_str()
        };
        if !executable_available(program, &working_directory) {
            return Err(BuildError::MissingProgram(program.to_owned()));
        }
    }
    Ok(())
}

fn executable_available(program: &str, working_directory: &Path) -> bool {
    let path = Path::new(program);
    let candidates = if path.is_absolute() {
        vec![path.to_owned()]
    } else if path.components().count() > 1 {
        vec![working_directory.join(path)]
    } else {
        std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .map(|directory| {
                        if directory.is_absolute() {
                            directory.join(path)
                        } else {
                            working_directory.join(directory).join(path)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates.iter().any(|candidate| {
        if is_executable(candidate) {
            return true;
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            return std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
                .split(';')
                .filter(|extension| extension.starts_with('.'))
                .any(|extension| is_executable(&candidate.with_extension(&extension[1..])));
        }
        false
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn parse_git_status(output: &ProcessOutput) -> GitWorktreeState {
    let text = String::from_utf8_lossy(&output.stdout);
    let mut changes = text
        .lines()
        .filter(|line| !line.is_empty())
        .take(MAX_GIT_CHANGES + 1)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return GitWorktreeState::Clean;
    }
    let truncated = changes.len() > MAX_GIT_CHANGES || output.stdout_truncated;
    changes.truncate(MAX_GIT_CHANGES);
    GitWorktreeState::Dirty { changes, truncated }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("build executable `{0}` is unavailable; install it or correct the build configuration")]
    MissingProgram(String),
    #[error("Git returned malformed branch or commit metadata")]
    InvalidGitMetadata,
    #[error("build path `{path}` cannot be resolved: {source}")]
    Path {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("build working directory escapes the Project or is not a directory: `{0}`")]
    UnsafeWorkingDirectory(PathBuf),
    #[error("Git worktree has changes and requires explicit confirmation")]
    DirtyConfirmationRequired(GitWorktreeState),
    #[error("Git preflight process failed: {0}")]
    GitProcess(ProcessError),
    #[error("Git preflight was cancelled")]
    GitCancelled,
    #[error("Git preflight timed out")]
    GitTimedOut,
    #[error("Git preflight exited unsuccessfully")]
    GitFailed(ProcessOutput),
    #[error("build command {index} was cancelled")]
    Cancelled { index: usize, output: ProcessOutput },
    #[error("build command {index} timed out")]
    TimedOut { index: usize, output: ProcessOutput },
    #[error("build command {index} exited unsuccessfully")]
    CommandFailed { index: usize, output: ProcessOutput },
    #[error(transparent)]
    Process(#[from] ProcessError),
}

#[cfg(test)]
#[path = "build/event_tests.rs"]
mod event_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    #[test]
    fn build_input_preflight_checks_paths_and_programs_without_running_commands() {
        let directory = tempfile::tempdir().unwrap();
        let commands = [BuildCommand::argv("rustc", ["--version"])];
        check_build_inputs(directory.path(), Path::new("."), &commands).unwrap();
        assert!(matches!(
            check_build_inputs(directory.path(), Path::new("missing"), &commands),
            Err(BuildError::Path { .. })
        ));
        assert!(matches!(
            check_build_inputs(directory.path(), Path::new(".."), &commands),
            Err(BuildError::UnsafeWorkingDirectory(_))
        ));
        assert!(matches!(
            check_build_inputs(
                directory.path(),
                Path::new("."),
                &[BuildCommand::argv(
                    "shipforge-test-no-such-program",
                    std::iter::empty::<&str>()
                )]
            ),
            Err(BuildError::MissingProgram(_))
        ));
        check_build_inputs(
            directory.path(),
            Path::new("."),
            &[BuildCommand::shell("exit 42")],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn failed_spawn_still_finishes_both_output_streams() {
        let directory = tempfile::tempdir().unwrap();
        let ends = std::sync::Mutex::new(Vec::new());
        let result = run_build_with_output(
            directory.path(),
            Path::new("."),
            &[BuildCommand::argv(
                "shipforge-test-no-such-program",
                std::iter::empty::<&str>(),
            )],
            true,
            Duration::from_secs(5),
            &CancellationToken::new(),
            &|index, stream, bytes| {
                assert!(bytes.is_empty());
                ends.lock().unwrap().push((index, stream));
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(BuildError::Process(ProcessError::Spawn(_)))
        ));
        assert_eq!(
            *ends.lock().unwrap(),
            vec![
                (0, crate::adapters::OutputStream::Stdout),
                (0, crate::adapters::OutputStream::Stderr),
            ]
        );
    }

    #[tokio::test]
    async fn live_build_output_includes_both_streams_and_completion() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let command = BuildCommand::shell("echo build-output & echo build-error 1>&2");
        #[cfg(not(windows))]
        let command = BuildCommand::shell("printf build-output; printf build-error >&2");
        let output = std::sync::Mutex::new((Vec::new(), Vec::new(), 0));
        run_build_with_output(
            directory.path(),
            Path::new("."),
            &[command],
            true,
            Duration::from_secs(5),
            &CancellationToken::new(),
            &|index, stream, bytes| {
                assert_eq!(index, 0);
                let mut captured = output.lock().unwrap();
                if bytes.is_empty() {
                    captured.2 += 1;
                }
                match stream {
                    crate::adapters::OutputStream::Stdout => captured.0.extend_from_slice(bytes),
                    crate::adapters::OutputStream::Stderr => captured.1.extend_from_slice(bytes),
                }
            },
        )
        .await
        .unwrap();
        let captured = output.into_inner().unwrap();
        assert!(
            String::from_utf8(captured.0)
                .unwrap()
                .contains("build-output")
        );
        assert!(
            String::from_utf8(captured.1)
                .unwrap()
                .contains("build-error")
        );
        assert_eq!(captured.2, 2);
    }

    fn init_git(path: &Path) -> bool {
        StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .is_ok_and(|status| status.success())
    }

    #[tokio::test]
    async fn git_metadata_handles_unborn_committed_and_detached_heads() {
        let directory = tempfile::tempdir().unwrap();
        assert!(init_git(directory.path()));
        let cancellation = CancellationToken::new();
        let timeout = Duration::from_secs(5);
        let unborn = inspect_git_metadata(
            directory.path(),
            &GitWorktreeState::Clean,
            timeout,
            &cancellation,
        )
        .await
        .unwrap();
        assert!(unborn.branch.is_some());
        assert!(unborn.revision.is_none());
        assert!(
            StdCommand::new("git")
                .args([
                    "-c",
                    "user.name=ShipForge Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--allow-empty",
                    "--quiet",
                    "-m",
                    "fixture"
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let committed = inspect_git_metadata(
            directory.path(),
            &GitWorktreeState::Clean,
            timeout,
            &cancellation,
        )
        .await
        .unwrap();
        assert_eq!(committed.branch, unborn.branch);
        assert!(matches!(
            committed.revision.as_ref().unwrap().len(),
            40 | 64
        ));
        assert!(
            StdCommand::new("git")
                .args(["checkout", "--detach", "--quiet", "HEAD"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let detached = inspect_git_metadata(
            directory.path(),
            &GitWorktreeState::Clean,
            timeout,
            &cancellation,
        )
        .await
        .unwrap();
        assert_eq!(detached.revision, committed.revision);
        assert_eq!(detached.branch, None);
    }

    #[tokio::test]
    async fn dirty_worktree_requires_confirmation_then_builds() {
        let directory = tempfile::tempdir().unwrap();
        if !init_git(directory.path()) {
            return;
        }
        std::fs::write(directory.path().join("untracked.txt"), "change").unwrap();
        let commands = [BuildCommand::argv("rustc", ["--version"])];
        let cancellation = CancellationToken::new();
        assert!(matches!(
            run_build(
                directory.path(),
                Path::new("."),
                &commands,
                false,
                Duration::from_secs(5),
                &cancellation
            )
            .await,
            Err(BuildError::DirtyConfirmationRequired(_))
        ));
        let report = run_build(
            directory.path(),
            Path::new("."),
            &commands,
            true,
            Duration::from_secs(5),
            &cancellation,
        )
        .await
        .unwrap();
        assert!(matches!(report.git, GitWorktreeState::Dirty { .. }));
        assert_eq!(report.commands[0].exit_code, Some(0));
    }

    #[tokio::test]
    async fn non_git_project_builds_without_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let report = run_build(
            directory.path(),
            Path::new("."),
            &[BuildCommand::argv("rustc", ["--version"])],
            false,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.git, GitWorktreeState::NotRepository);
    }

    #[tokio::test]
    async fn working_directory_cannot_escape_project() {
        let parent = tempfile::tempdir().unwrap();
        let project = parent.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let error = run_build(
            &project,
            Path::new(".."),
            &[BuildCommand::argv("rustc", ["--version"])],
            true,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, BuildError::UnsafeWorkingDirectory(_)));
    }
}
