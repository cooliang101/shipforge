//! Bounded diagnostics for interrupted temporary work; never cleanup authority.

use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::DeploymentId,
    drivers::inventory::{TemporaryRemnant, TemporaryRemnantKind, TemporaryRemnants},
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    AuthenticatedSession, DeploymentMarker, LinuxSshTarget, MarkerError, RemoteCommandOutput,
    RemotePath, SshConnectionError,
};

const MAX_ENTRIES: usize = 256;
const MAX_LIST_BYTES: usize = 64 * 1024;
const MAX_NOTICES: usize = 32;
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RemnantsError {
    #[error("temporary-remnant inspection was cancelled; remnants remain unknown")]
    Cancelled,
    #[error("temporary-remnant inspection timed out; remnants remain unknown")]
    Timeout,
    #[error("temporary-remnant inspection exceeded its bounded limits; remnants remain unknown")]
    Limit,
    #[error(
        "temporary-remnant root is unsafe, unmarked and nonempty, or belongs to another Component"
    )]
    UnsafeRoot,
    #[error(
        "matching Deployment Marker has multiple hard links; interrupted marker publication or an external link requires manual inspection"
    )]
    SharedMarker,
    #[error(
        "temporary-remnant inspection failed or changed during observation; remnants remain unknown"
    )]
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    name: Vec<u8>,
    kind: u8,
    size: u64,
}

#[async_trait]
trait RemnantsRemote: Sync {
    async fn root(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<bool, RemnantsError>;
    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, RemnantsError>;
}

#[async_trait]
impl RemnantsRemote for AuthenticatedSession {
    async fn root(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<bool, RemnantsError> {
        if self
            .check_deployment_marker(target, marker, cancellation)
            .await
            .map_err(|error| marker_error(&error))?
        {
            Ok(true)
        } else {
            self.check_unmarked_root(target, cancellation)
                .await
                .map(|()| false)
                .map_err(|error| marker_error(&error))
        }
    }

    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, RemnantsError> {
        self.execute(command, COMMAND_TIMEOUT, cancellation)
            .await
            .map_err(|error| match error {
                SshConnectionError::Cancelled => RemnantsError::Cancelled,
                SshConnectionError::Timeout { .. } => RemnantsError::Timeout,
                _ => RemnantsError::Remote,
            })
    }
}

impl AuthenticatedSession {
    /// Reads known temporary filenames without traversing them or inferring ownership.
    ///
    /// # Errors
    /// Rejects unsafe identity/paths, shared markers, unstable listings and bounded limits.
    pub async fn temporary_remnants(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<TemporaryRemnants, RemnantsError> {
        inspect(self, target, marker, cancellation, SCAN_TIMEOUT).await
    }
}

async fn inspect<R: RemnantsRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    marker: &DeploymentMarker,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<TemporaryRemnants, RemnantsError> {
    RemotePath::parse(target.root.clone()).map_err(|_| RemnantsError::UnsafeRoot)?;
    if cancellation.is_cancelled() {
        return Err(RemnantsError::Cancelled);
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(RemnantsError::Cancelled),
        result = tokio::time::timeout(timeout, scan(remote, target, marker, cancellation)) => result.map_err(|_| RemnantsError::Timeout)?,
    }
}

async fn scan<R: RemnantsRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    marker: &DeploymentMarker,
    cancellation: &CancellationToken,
) -> Result<TemporaryRemnants, RemnantsError> {
    if !remote.root(target, marker, cancellation).await? {
        return Ok(TemporaryRemnants::default());
    }
    require_single_marker(remote, target, cancellation).await?;
    let root = list(remote, &target.root, cancellation).await?;
    let temporary_path = format!("{}/temporary", target.root);
    let temporary = list(remote, &temporary_path, cancellation).await?;
    if root.len() + temporary.len() > MAX_ENTRIES {
        return Err(RemnantsError::Limit);
    }
    let mut result = TemporaryRemnants::default();
    for entry in &root {
        root_entry(entry, &mut result);
    }
    for entry in &temporary {
        temporary_entry(entry, &mut result);
    }
    if root != list(remote, &target.root, cancellation).await?
        || temporary != list(remote, &temporary_path, cancellation).await?
    {
        return Err(RemnantsError::Remote);
    }
    if !remote.root(target, marker, cancellation).await? {
        return Err(RemnantsError::UnsafeRoot);
    }
    require_single_marker(remote, target, cancellation).await?;
    Ok(result)
}

async fn require_single_marker<R: RemnantsRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    cancellation: &CancellationToken,
) -> Result<(), RemnantsError> {
    let path = format!("{}/.shipforge-project.json", target.root);
    let output = run(remote, "stat", &["--format=%h", "--", &path], cancellation).await?;
    match std::str::from_utf8(&output)
        .ok()
        .and_then(|value| value.trim_end_matches('\n').parse::<u64>().ok())
    {
        Some(1) => Ok(()),
        Some(2..) => Err(RemnantsError::SharedMarker),
        _ => Err(RemnantsError::Remote),
    }
}

async fn list<R: RemnantsRemote>(
    remote: &R,
    path: &str,
    cancellation: &CancellationToken,
) -> Result<Vec<Entry>, RemnantsError> {
    if test(remote, "-L", path, cancellation).await? {
        return Err(RemnantsError::UnsafeRoot);
    }
    if !test(remote, "-e", path, cancellation).await? {
        return Ok(Vec::new());
    }
    if !test(remote, "-d", path, cancellation).await? {
        return Err(RemnantsError::UnsafeRoot);
    }
    let bytes = run(
        remote,
        "timeout",
        &[
            "--signal=TERM",
            "--kill-after=2s",
            "15s",
            "find",
            path,
            "-mindepth",
            "1",
            "-maxdepth",
            "1",
            "-printf",
            "%f\\0%y\\0%s\\0",
        ],
        cancellation,
    )
    .await?;
    parse_listing(&bytes)
}

fn parse_listing(bytes: &[u8]) -> Result<Vec<Entry>, RemnantsError> {
    if bytes.len() > MAX_LIST_BYTES {
        return Err(RemnantsError::Limit);
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let fields = bytes
        .strip_suffix(&[0])
        .ok_or(RemnantsError::Remote)?
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    if fields.len() % 3 != 0 {
        return Err(RemnantsError::Remote);
    }
    if fields.len() / 3 > MAX_ENTRIES {
        return Err(RemnantsError::Limit);
    }
    let mut entries = BTreeMap::new();
    for fields in fields.chunks_exact(3) {
        if fields[0].is_empty() || fields[0].contains(&b'/') || fields[1].len() != 1 {
            return Err(RemnantsError::Remote);
        }
        let size = std::str::from_utf8(fields[2])
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or(RemnantsError::Remote)?;
        let entry = Entry {
            name: fields[0].to_vec(),
            kind: fields[1][0],
            size,
        };
        if entries.insert(entry.name.clone(), entry).is_some() {
            return Err(RemnantsError::Remote);
        }
    }
    Ok(entries.into_values().collect())
}

fn root_entry(entry: &Entry, result: &mut TemporaryRemnants) {
    let Ok(name) = std::str::from_utf8(&entry.name) else {
        notice(
            result,
            "An unrecognized root entry was not inspected or attributed.",
        );
        return;
    };
    if matches!(
        name,
        ".shipforge-project.json" | "archives" | "releases" | "temporary" | "metadata" | "current"
    ) {
        return;
    }
    let marker = name
        .strip_prefix(".shipforge-marker-")
        .and_then(|name| name.strip_suffix(".tmp"));
    if entry.kind == b'f'
        && marker.is_some_and(|value| {
            uuid::Uuid::parse_str(value)
                .is_ok_and(|id| !id.is_nil() && id.simple().to_string() == value)
        })
    {
        result.entries.push(TemporaryRemnant {
            kind: TemporaryRemnantKind::MarkerPublication,
            deployment: None,
        });
    } else {
        notice(
            result,
            "An unrecognized or unsafe root entry was not inspected or attributed.",
        );
    }
}

fn temporary_entry(entry: &Entry, result: &mut TemporaryRemnants) {
    let Ok(name) = std::str::from_utf8(&entry.name) else {
        notice(
            result,
            "An unrecognized temporary entry was not inspected or attributed.",
        );
        return;
    };
    for (suffix, kind, expected_type) in [
        (".tar.gz", TemporaryRemnantKind::UploadArchive, b'f'),
        (".dir", TemporaryRemnantKind::ExtractedDirectory, b'd'),
        (".current", TemporaryRemnantKind::ActivationLink, b'l'),
        (
            ".rollback-current",
            TemporaryRemnantKind::RollbackLink,
            b'l',
        ),
    ] {
        if let Some(id) = name
            .strip_suffix(suffix)
            .and_then(|id| id.parse::<DeploymentId>().ok())
        {
            if entry.kind == expected_type {
                result.entries.push(TemporaryRemnant {
                    kind,
                    deployment: Some(id),
                });
            } else {
                notice(
                    result,
                    "A recognized temporary filename has an unsafe or unexpected file type; it was not followed.",
                );
            }
            return;
        }
    }
    notice(
        result,
        "An unrecognized temporary entry was not inspected or attributed.",
    );
}

fn notice(result: &mut TemporaryRemnants, message: &str) {
    result.incomplete = true;
    if result.notices.len() < MAX_NOTICES {
        result.notices.push(message.into());
    }
}

async fn test<R: RemnantsRemote>(
    remote: &R,
    flag: &str,
    path: &str,
    cancellation: &CancellationToken,
) -> Result<bool, RemnantsError> {
    let command = CommandSpec::structured("test", [flag, path].map(CommandArgument::plain))
        .map_err(|_| RemnantsError::Remote)?;
    let output = remote.command(&command, cancellation).await?;
    if output.stdout_truncated || output.stderr_truncated || !output.stdout.is_empty() {
        return Err(RemnantsError::Remote);
    }
    match output.exit_status {
        0 => Ok(true),
        1 => Ok(false),
        124 | 137 => Err(RemnantsError::Timeout),
        _ => Err(RemnantsError::Remote),
    }
}

async fn run<R: RemnantsRemote>(
    remote: &R,
    program: &str,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, RemnantsError> {
    let command =
        CommandSpec::structured(program, args.iter().map(|arg| CommandArgument::plain(*arg)))
            .map_err(|_| RemnantsError::Remote)?;
    let output = remote.command(&command, cancellation).await?;
    if output.stdout_truncated || output.stderr_truncated {
        return Err(RemnantsError::Limit);
    }
    match output.exit_status {
        0 => Ok(output.stdout),
        124 | 137 => Err(RemnantsError::Timeout),
        _ => Err(RemnantsError::Remote),
    }
}

fn marker_error(error: &MarkerError) -> RemnantsError {
    match error {
        MarkerError::Cancelled => RemnantsError::Cancelled,
        MarkerError::Remote => RemnantsError::Remote,
        _ => RemnantsError::UnsafeRoot,
    }
}

#[cfg(test)]
mod tests;
