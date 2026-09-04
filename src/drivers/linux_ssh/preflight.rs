//! Read-only checks of the exact remote tools used by the SSH deployment path.

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::telemetry::{CommandArgument, CommandSpec};

use super::{AuthenticatedSession, LinuxSshTarget, RemoteCommandOutput, SshConnectionError};

const TOOL_CHECK: &str = "for tool do command -v \"$tool\" >/dev/null || exit 1; done";
const ROOT_CHECK: &str = "path=$1; while ! test -e \"$path\"; do test ! -L \"$path\" || exit 1; path=${path%/*}; test -n \"$path\" || path=/; done; test -d \"$path\" && test -w \"$path\" && test -x \"$path\" || exit 1; printf '%s\\n' \"$path\"";
const TOOLS: &[&str] = &[
    "test",
    "printf",
    "mkdir",
    "tar",
    "gzip",
    "sha256sum",
    "ln",
    "mv",
    "rm",
    "stat",
    "readlink",
    "find",
    "head",
];

/// Checks tools, filesystem capacity, and configured service/health clients.
/// This never creates probe files or starts/restarts services. Available space
/// is a planning snapshot, not a guarantee that a future build will fit.
pub(super) async fn check_preflight(
    session: &AuthenticatedSession,
    target: &LinuxSshTarget,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Vec<String>, PreflightError> {
    check_with_remote(session, target, timeout, cancellation).await
}

#[async_trait]
pub(super) trait PreflightRemote: Sync {
    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError>;
}

#[async_trait]
impl PreflightRemote for AuthenticatedSession {
    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.execute(command, timeout, cancellation).await
    }
}

async fn check_with_remote<R: PreflightRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Vec<String>, PreflightError> {
    let mut tools = TOOLS.to_vec();
    if target.systemd.is_some() {
        tools.push("systemctl");
    }
    if target.health.is_some() {
        tools.push("curl");
    }
    let mut arguments = vec!["-c", TOOL_CHECK, "shipforge-preflight"];
    arguments.extend(tools.iter().copied());
    run(
        remote,
        "required commands",
        "sh",
        &arguments,
        timeout,
        cancellation,
    )
    .await?;
    check_features(remote, timeout, cancellation).await?;
    let capacity = probe_capacity(remote, target, timeout, cancellation).await?;
    if capacity.available_bytes == 0 || capacity.available_inodes == Some(0) {
        return Err(PreflightError::NoSpace);
    }
    let inodes = capacity.available_inodes.map_or_else(
        || "inode capacity is not reported".into(),
        |count| format!("{count} inodes available"),
    );
    check_optional_tools(remote, target, timeout, cancellation).await?;
    Ok(vec![
        format!("Remote commands: sh; {}", tools.join(", ")),
        format!("Remote filesystem at {}: {} bytes available; {inodes} (planning snapshot; archive size is not yet known)", capacity.parent, capacity.available_bytes),
        "GNU tar extraction, ln symlinks and mv no-clobber/no-target-directory options checked; same-filesystem atomic switching is checked again before activation".into(),
    ])
}

#[derive(Clone, Debug)]
pub(super) struct FilesystemCapacity {
    pub parent: String,
    pub available_bytes: u64,
    pub block_size: u64,
    pub available_inodes: Option<u64>,
}

pub(super) async fn probe_capacity<R: PreflightRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<FilesystemCapacity, PreflightError> {
    let parent = run(
        remote,
        "writable Component root or parent",
        "sh",
        &["-c", ROOT_CHECK, "shipforge-preflight", &target.root],
        timeout,
        cancellation,
    )
    .await?;
    let parent = parent.strip_suffix('\n').unwrap_or(&parent);
    if !parent.starts_with('/')
        || parent.chars().any(char::is_control)
        || (parent != "/"
            && parent != target.root
            && !target.root.starts_with(&format!("{parent}/")))
    {
        return Err(PreflightError::InvalidOutput("root probe"));
    }
    let filesystem = run(
        remote,
        "available filesystem space",
        "stat",
        &["--file-system", "--format=%a:%S:%c:%d", "--", parent],
        timeout,
        cancellation,
    )
    .await?;
    parse_capacity(&filesystem, parent).ok_or(PreflightError::InvalidOutput("filesystem space"))
}

async fn check_features<R: PreflightRemote>(
    remote: &R,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), PreflightError> {
    for (tool, options) in [
        (
            "tar",
            &["--extract", "--gzip", "--directory", "--no-same-owner"][..],
        ),
        ("ln", &["--symbolic"][..]),
        ("mv", &["--no-clobber", "--no-target-directory"][..]),
    ] {
        let output = run(remote, tool, tool, &["--help"], timeout, cancellation).await?;
        for option in options {
            if !output.contains(option) {
                return Err(PreflightError::UnsupportedOption { tool, option });
            }
        }
    }
    Ok(())
}

async fn check_optional_tools<R: PreflightRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), PreflightError> {
    if let Some(unit) = &target.systemd {
        let output = run(
            remote,
            "configured systemd service",
            "systemctl",
            &["show", "--property=LoadState", "--", unit],
            timeout,
            cancellation,
        )
        .await?;
        if output.trim() != "LoadState=loaded" {
            return Err(PreflightError::ServiceNotLoaded);
        }
    }
    if let Some(url) = &target.health {
        let output = run(
            remote,
            "HTTP client",
            "curl",
            &["--version"],
            timeout,
            cancellation,
        )
        .await?;
        let protocol = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        if !output
            .lines()
            .filter_map(|line| line.strip_prefix("Protocols:"))
            .any(|line| line.split_ascii_whitespace().any(|value| value == protocol))
        {
            return Err(PreflightError::HttpProtocol(protocol));
        }
    }
    Ok(())
}

fn parse_capacity(output: &str, parent: &str) -> Option<FilesystemCapacity> {
    let mut values = output.trim().split(':');
    let blocks = values.next()?.parse::<u64>().ok()?;
    let block_size = values.next()?.parse::<u64>().ok()?;
    let total_inodes = values.next()?.parse::<u64>().ok()?;
    let inodes = values.next()?.parse::<u64>().ok()?;
    if values.next().is_some() || block_size == 0 || inodes > total_inodes {
        return None;
    }
    Some(FilesystemCapacity {
        parent: parent.to_owned(),
        available_bytes: blocks.checked_mul(block_size)?,
        block_size,
        available_inodes: (total_inodes != 0).then_some(inodes),
    })
}

async fn run<R: PreflightRemote>(
    remote: &R,
    stage: &'static str,
    program: &str,
    arguments: &[&str],
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, PreflightError> {
    if cancellation.is_cancelled() {
        return Err(PreflightError::Cancelled);
    }
    let command = CommandSpec::structured(
        program,
        arguments.iter().map(|value| CommandArgument::plain(*value)),
    )
    .map_err(|error| PreflightError::Command(error.to_string()))?;
    let output = remote.command(&command, timeout, cancellation).await?;
    if output.exit_status != 0 {
        return Err(PreflightError::Failed {
            stage,
            status: output.exit_status,
        });
    }
    if output.stdout_truncated || output.stderr_truncated {
        return Err(PreflightError::InvalidOutput(stage));
    }
    String::from_utf8(output.stdout).map_err(|_| PreflightError::InvalidOutput(stage))
}

#[derive(Debug, Error)]
pub(super) enum PreflightError {
    #[error("environment check was cancelled")]
    Cancelled,
    #[error(transparent)]
    Connection(#[from] SshConnectionError),
    #[error("could not create preflight command: {0}")]
    Command(String),
    #[error(
        "remote check `{stage}` failed with exit status {status}; install the required tools or correct directory/service permissions"
    )]
    Failed { stage: &'static str, status: u32 },
    #[error("remote `{tool}` does not support required option `{option}`")]
    UnsupportedOption {
        tool: &'static str,
        option: &'static str,
    },
    #[error("remote check `{0}` returned malformed or truncated output")]
    InvalidOutput(&'static str),
    #[error("the deployment filesystem has no available bytes or inodes")]
    NoSpace,
    #[error(
        "configured systemd service is not loaded; install or correct the unit before deploying"
    )]
    ServiceNotLoaded,
    #[error("remote curl does not support the configured {0} health check protocol")]
    HttpProtocol(&'static str),
}

#[cfg(test)]
mod tests;
