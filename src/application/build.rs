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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildReport {
    pub git: GitWorktreeState,
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
    let (root, working_directory) = resolve_working_directory(project_root, working_directory)?;
    let git = inspect_git(&root, command_timeout, cancellation).await?;
    if matches!(git, GitWorktreeState::Dirty { .. }) && !allow_dirty {
        return Err(BuildError::DirtyConfirmationRequired(git));
    }
    let mut outputs = Vec::with_capacity(commands.len());
    for (index, command) in commands.iter().enumerate() {
        let output =
            run_grouped(command, &working_directory, command_timeout, cancellation).await?;
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
        working_directory,
        commands: outputs,
    })
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
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn init_git(path: &Path) -> bool {
        StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .is_ok_and(|status| status.success())
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
