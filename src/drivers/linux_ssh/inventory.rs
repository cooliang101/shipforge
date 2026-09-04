use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{ReleaseManifest, ReleaseVersion},
    drivers::inventory::{InventoryIssue, InventoryRelease, ReleaseInventory},
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    ActivationOptions, AuthenticatedSession, DeploymentMarker, LinuxSshTarget, MarkerError,
    RemoteCommandOutput, SshConnectionError,
};

const MAX_ENTRIES: usize = 1024;
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_TOTAL_ARCHIVE_BYTES: u64 = 4 * MAX_ARCHIVE_BYTES;
const MAX_LIST_BYTES: usize = 64 * 1024;
const MANIFEST_PREFIX_BYTES: usize = 9216;
const MANIFEST_BYTES: usize = 8192;
const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
// The script is fixed. The archive is supplied as $1, never interpolated into Shell text.
const MANIFEST_PREFIX_SCRIPT: &str = "gzip -cd -- \"$1\" | head -c 9216";

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum InventoryError {
    #[error("Release inventory was cancelled")]
    Cancelled,
    #[error("Release inventory exceeded its time limit; no complete inventory is available")]
    Timeout,
    #[error("Release inventory exceeds its bounded scan limits; no entries were silently omitted")]
    Limit,
    #[error(
        "Release inventory root is unsafe, unmarked and nonempty, or belongs to a different Component"
    )]
    Root,
    #[error("Release inventory could not inspect remote filesystem metadata safely")]
    Remote,
    #[error("Release archive is not a regular, unchanged file")]
    UnsafeArchive,
    #[error("Release archive has invalid gzip data or could not be checked within its time limit")]
    InvalidArchive,
    #[error("Release archive does not start with a valid bounded Component manifest")]
    Manifest,
    #[error("Extracted Release manifest is missing, unsafe, or differs from its archive")]
    ExtractedManifest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteEntry {
    name: Vec<u8>,
    kind: u8,
    size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    identity: String,
    size: u64,
}

#[derive(Default)]
struct Candidate {
    archive: Option<RemoteEntry>,
    directory: Option<RemoteEntry>,
}

#[async_trait]
trait InventoryRemote: Sync {
    async fn predicate(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, InventoryError> {
        self.command(command, cancellation).await
    }

    async fn check_root(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<bool, InventoryError>;
    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, InventoryError>;
    async fn current(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<Option<ReleaseVersion>, InventoryError>;
}

#[async_trait]
impl InventoryRemote for AuthenticatedSession {
    async fn predicate(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, InventoryError> {
        self.execute_allowing(command, COMMAND_TIMEOUT, cancellation, &[0, 1])
            .await
            .map_err(|error| match error {
                SshConnectionError::Cancelled => InventoryError::Cancelled,
                SshConnectionError::Timeout { .. } => InventoryError::Timeout,
                _ => InventoryError::Remote,
            })
    }

    async fn check_root(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<bool, InventoryError> {
        match self
            .check_deployment_marker(target, expected, cancellation)
            .await
        {
            Ok(true) => Ok(true),
            Ok(false) => self
                .check_unmarked_root(target, cancellation)
                .await
                .map(|()| false)
                .map_err(|error| marker_error(&error)),
            Err(error) => Err(marker_error(&error)),
        }
    }

    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, InventoryError> {
        self.execute(command, COMMAND_TIMEOUT, cancellation)
            .await
            .map_err(|error| match error {
                SshConnectionError::Cancelled => InventoryError::Cancelled,
                SshConnectionError::Timeout { .. } => InventoryError::Timeout,
                _ => InventoryError::Remote,
            })
    }

    async fn current(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<Option<ReleaseVersion>, InventoryError> {
        let current = self
            .observe_current(
                target,
                ActivationOptions {
                    command_timeout: COMMAND_TIMEOUT,
                },
                cancellation,
            )
            .await
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    InventoryError::Cancelled
                } else {
                    InventoryError::Remote
                }
            })?;
        if let Some(version) = &current {
            self.check_release_manifest(target, expected, version, cancellation)
                .await
                .map_err(|error| marker_error(&error))?;
        }
        Ok(current)
    }
}

impl AuthenticatedSession {
    /// Reconstructs read-only inventory using the caller's existing YAML identity.
    /// No root, archive, manifest, marker or current link is created or repaired.
    /// Archive data stays remote; only bounded metadata is returned to the client.
    ///
    /// # Errors
    /// Rejects unsafe roots, cancellation, incomplete scans and resource-limit overflow.
    pub async fn release_inventory(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<ReleaseInventory, InventoryError> {
        inventory_with_remote(self, target, expected, cancellation, SCAN_TIMEOUT).await
    }
}

async fn inventory_with_remote<R: InventoryRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    expected: &DeploymentMarker,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<ReleaseInventory, InventoryError> {
    if cancellation.is_cancelled() {
        return Err(InventoryError::Cancelled);
    }
    tokio::select! {
        biased;
        ()=cancellation.cancelled()=>Err(InventoryError::Cancelled),
        result=tokio::time::timeout(timeout,scan(remote,target,expected,cancellation))=>result.map_err(|_|InventoryError::Timeout)?,
    }
}

async fn scan<R: InventoryRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    expected: &DeploymentMarker,
    cancellation: &CancellationToken,
) -> Result<ReleaseInventory, InventoryError> {
    let mut inventory=ReleaseInventory{releases:Vec::new(),issues:Vec::new(),current:Ok(None),notices:vec!["Inventory records archive identity and filesystem facts, not historical health or payload activation safety.".into()]};
    if !remote.check_root(target, expected, cancellation).await? {
        return Ok(inventory);
    }
    let archives = list(remote, &format!("{}/archives", target.root), cancellation).await?;
    let directories = list(remote, &format!("{}/releases", target.root), cancellation).await?;
    if archives.len() + directories.len() > MAX_ENTRIES {
        return Err(InventoryError::Limit);
    }
    let candidates = collect_candidates(&archives, &directories, &mut inventory.issues);
    let mut bytes = 0_u64;
    for (version, candidate) in candidates {
        if cancellation.is_cancelled() {
            return Err(InventoryError::Cancelled);
        }
        let Some(archive) = candidate.archive else {
            inventory.issues.push(issue(Some(version),"Extracted directory has no versioned archive; it cannot establish a complete Release."));
            continue;
        };
        if archive.kind != b'f' {
            inventory.issues.push(issue(
                Some(version),
                "Archive is linked or not a regular file.",
            ));
            continue;
        }
        bytes = bytes
            .checked_add(archive.size)
            .ok_or(InventoryError::Limit)?;
        if archive.size > MAX_ARCHIVE_BYTES || bytes > MAX_TOTAL_ARCHIVE_BYTES {
            return Err(InventoryError::Limit);
        }
        match inspect_archive(
            remote,
            target,
            expected,
            &version,
            &archive,
            candidate.directory.as_ref(),
            cancellation,
        )
        .await
        {
            Ok(release) => {
                if !release.extracted {
                    inventory.issues.push(issue(Some(version),"Verified archive has no extracted directory; it is not immediately available for activation."));
                }
                inventory.releases.push(release);
            }
            Err(
                error @ (InventoryError::Cancelled
                | InventoryError::Timeout
                | InventoryError::Limit),
            ) => return Err(error),
            Err(error) => inventory
                .issues
                .push(issue(Some(version), &error.to_string())),
        }
    }
    // A second namespace observation detects concurrent additions/removals/type changes.
    if archives != list(remote, &format!("{}/archives", target.root), cancellation).await?
        || directories != list(remote, &format!("{}/releases", target.root), cancellation).await?
    {
        return Err(InventoryError::Remote);
    }
    inventory.current = match remote.current(target, expected, cancellation).await {
        Ok(current) => Ok(current),
        Err(
            error @ (InventoryError::Cancelled | InventoryError::Timeout | InventoryError::Limit),
        ) => return Err(error),
        Err(_) => Err(
            "Current state could not be verified against this Component's marker and manifest."
                .into(),
        ),
    };
    if !remote.check_root(target, expected, cancellation).await? {
        return Err(InventoryError::Root);
    }
    Ok(inventory)
}

fn issue(version: Option<ReleaseVersion>, message: &str) -> InventoryIssue {
    InventoryIssue {
        version,
        message: message.into(),
    }
}

fn collect_candidates(
    archives: &[RemoteEntry],
    directories: &[RemoteEntry],
    issues: &mut Vec<InventoryIssue>,
) -> BTreeMap<ReleaseVersion, Candidate> {
    let mut values = BTreeMap::<ReleaseVersion, Candidate>::new();
    for (archive, entries) in [(true, archives), (false, directories)] {
        for entry in entries {
            let name = std::str::from_utf8(&entry.name)
                .ok()
                .and_then(|name| {
                    if archive {
                        name.strip_suffix(".tar.gz")
                    } else {
                        Some(name)
                    }
                })
                .and_then(|name| ReleaseVersion::parse(name).ok());
            let Some(version) = name else {
                issues.push(issue(None,"Namespace contains an unsupported entry name; its path is not displayed or followed."));
                continue;
            };
            let value = values.entry(version).or_default();
            if archive {
                value.archive = Some(entry.clone());
            } else {
                value.directory = Some(entry.clone());
            }
        }
    }
    values
}

async fn list<R: InventoryRemote>(
    remote: &R,
    path: &str,
    cancellation: &CancellationToken,
) -> Result<Vec<RemoteEntry>, InventoryError> {
    if !test(remote, "-e", path, cancellation).await? {
        return Ok(Vec::new());
    }
    if test(remote, "-L", path, cancellation).await?
        || !test(remote, "-d", path, cancellation).await?
    {
        return Err(InventoryError::Root);
    }
    let bytes = timed_command(
        remote,
        "find",
        &[
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

fn parse_listing(bytes: &[u8]) -> Result<Vec<RemoteEntry>, InventoryError> {
    if bytes.len() > MAX_LIST_BYTES {
        return Err(InventoryError::Limit);
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let Some(bytes) = bytes.strip_suffix(&[0]) else {
        return Err(InventoryError::Remote);
    };
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.len() % 3 != 0 {
        return Err(InventoryError::Remote);
    }
    if fields.len() / 3 > MAX_ENTRIES {
        return Err(InventoryError::Limit);
    }
    let mut entries = BTreeMap::new();
    for chunk in fields.chunks_exact(3) {
        if chunk[0].is_empty() || chunk[1].len() != 1 {
            return Err(InventoryError::Remote);
        }
        let size = std::str::from_utf8(chunk[2])
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(InventoryError::Remote)?;
        let entry = RemoteEntry {
            name: chunk[0].to_vec(),
            kind: chunk[1][0],
            size,
        };
        if entries.insert(entry.name.clone(), entry).is_some() {
            return Err(InventoryError::Remote);
        }
    }
    Ok(entries.into_values().collect())
}

async fn inspect_archive<R: InventoryRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    expected: &DeploymentMarker,
    version: &ReleaseVersion,
    entry: &RemoteEntry,
    directory: Option<&RemoteEntry>,
    cancellation: &CancellationToken,
) -> Result<InventoryRelease, InventoryError> {
    let path = format!("{}/archives/{version}.tar.gz", target.root);
    let before = archive_stamp(remote, &path, cancellation).await?;
    if before.size != entry.size || before.size == 0 {
        return Err(InventoryError::UnsafeArchive);
    }
    let digest = timed_command(remote, "sha256sum", &["--", &path], cancellation).await?;
    let sha256 = parse_hash(&digest)?;
    timed_command(remote, "gzip", &["--test", "--", &path], cancellation)
        .await
        .map_err(|error| match error {
            InventoryError::Remote => InventoryError::InvalidArchive,
            other => other,
        })?;
    let prefix = timed_command(
        remote,
        "sh",
        &["-c", MANIFEST_PREFIX_SCRIPT, "shipforge-inventory", &path],
        cancellation,
    )
    .await?;
    let manifest = parse_archive_manifest(&prefix, expected, version)?;
    if archive_stamp(remote, &path, cancellation).await? != before {
        return Err(InventoryError::UnsafeArchive);
    }
    let extracted = if let Some(directory) = directory {
        if directory.kind != b'd' {
            return Err(InventoryError::ExtractedManifest);
        }
        let extracted_path = format!("{}/releases/{version}", target.root);
        if test(remote, "-L", &extracted_path, cancellation).await?
            || !test(remote, "-d", &extracted_path, cancellation).await?
        {
            return Err(InventoryError::ExtractedManifest);
        }
        let path = format!("{extracted_path}/manifest.json");
        if test(remote, "-L", &path, cancellation).await?
            || !test(remote, "-f", &path, cancellation).await?
        {
            return Err(InventoryError::ExtractedManifest);
        }
        let bytes = command(remote, "head", &["-c", "8193", "--", &path], cancellation).await?;
        let extracted = parse_manifest(&bytes, expected, version)
            .map_err(|_| InventoryError::ExtractedManifest)?;
        if extracted != manifest {
            return Err(InventoryError::ExtractedManifest);
        }
        true
    } else {
        false
    };
    Ok(InventoryRelease {
        manifest,
        sha256,
        size: before.size,
        extracted,
    })
}

async fn archive_stamp<R: InventoryRemote>(
    remote: &R,
    path: &str,
    cancellation: &CancellationToken,
) -> Result<FileStamp, InventoryError> {
    if test(remote, "-L", path, cancellation).await?
        || !test(remote, "-f", path, cancellation).await?
    {
        return Err(InventoryError::UnsafeArchive);
    }
    let bytes = command(
        remote,
        "stat",
        &["--format=%d:%i:%s:%y:%z", "--", path],
        cancellation,
    )
    .await?;
    let identity = std::str::from_utf8(&bytes)
        .map_err(|_| InventoryError::Remote)?
        .trim_end_matches('\n');
    if identity.len() > 256 || identity.chars().any(char::is_control) {
        return Err(InventoryError::Remote);
    }
    let mut fields = identity.splitn(4, ':');
    for _ in 0..2 {
        fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(InventoryError::Remote)?;
    }
    let size = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(InventoryError::Remote)?;
    if fields.next().is_none() {
        return Err(InventoryError::Remote);
    }
    Ok(FileStamp {
        identity: identity.into(),
        size,
    })
}

fn parse_hash(bytes: &[u8]) -> Result<String, InventoryError> {
    let value = bytes.get(..64).ok_or(InventoryError::InvalidArchive)?;
    if !value.iter().all(u8::is_ascii_hexdigit) || bytes.get(64) != Some(&b' ') {
        return Err(InventoryError::InvalidArchive);
    }
    String::from_utf8(value.to_ascii_lowercase()).map_err(|_| InventoryError::InvalidArchive)
}

fn parse_archive_manifest(
    prefix: &[u8],
    expected: &DeploymentMarker,
    version: &ReleaseVersion,
) -> Result<ReleaseManifest, InventoryError> {
    if prefix.len() > MANIFEST_PREFIX_BYTES || prefix.len() < 512 {
        return Err(InventoryError::Manifest);
    }
    let header = tar::Header::from_byte_slice(&prefix[..512]);
    let size = usize::try_from(header.size().map_err(|_| InventoryError::Manifest)?)
        .map_err(|_| InventoryError::Manifest)?;
    if header.path_bytes().as_ref() != b"manifest.json"
        || !header.entry_type().is_file()
        || size > MANIFEST_BYTES
        || size == 0
        || prefix.len() < 512 + size
    {
        return Err(InventoryError::Manifest);
    }
    let actual_checksum: u32 = prefix[..512]
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                32
            } else {
                u32::from(*byte)
            }
        })
        .sum();
    if header.cksum().map_err(|_| InventoryError::Manifest)? != actual_checksum {
        return Err(InventoryError::Manifest);
    }
    parse_manifest(&prefix[512..512 + size], expected, version)
}

fn parse_manifest(
    bytes: &[u8],
    expected: &DeploymentMarker,
    version: &ReleaseVersion,
) -> Result<ReleaseManifest, InventoryError> {
    expected
        .validate_manifest(bytes, version)
        .map_err(|_| InventoryError::Manifest)?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(bytes).map_err(|_| InventoryError::Manifest)?;
    if manifest.source_revision.as_ref().is_some_and(|revision| {
        !(7..=64).contains(&revision.len())
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(InventoryError::Manifest);
    }
    Ok(manifest)
}

async fn timed_command<R: InventoryRemote>(
    remote: &R,
    program: &str,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, InventoryError> {
    let mut all = vec!["--signal=TERM", "--kill-after=2s", "15s", program];
    all.extend_from_slice(args);
    command(remote, "timeout", &all, cancellation).await
}

async fn command<R: InventoryRemote>(
    remote: &R,
    program: &str,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, InventoryError> {
    let output = raw_command(remote, program, args, cancellation).await?;
    if output.stdout_truncated || output.stderr_truncated {
        return Err(InventoryError::Limit);
    }
    if matches!(output.exit_status, 124 | 137) {
        return Err(InventoryError::Timeout);
    }
    if output.exit_status != 0 {
        return Err(InventoryError::Remote);
    }
    Ok(output.stdout)
}

async fn test<R: InventoryRemote>(
    remote: &R,
    flag: &str,
    path: &str,
    cancellation: &CancellationToken,
) -> Result<bool, InventoryError> {
    if cancellation.is_cancelled() {
        return Err(InventoryError::Cancelled);
    }
    let command = CommandSpec::structured("test", [flag, path].map(CommandArgument::plain))
        .map_err(|_| InventoryError::Remote)?;
    let output = remote.predicate(&command, cancellation).await?;
    if output.stdout_truncated || output.stderr_truncated || !output.stdout.is_empty() {
        return Err(InventoryError::Remote);
    }
    match output.exit_status {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(InventoryError::Remote),
    }
}

async fn raw_command<R: InventoryRemote>(
    remote: &R,
    program: &str,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<RemoteCommandOutput, InventoryError> {
    if cancellation.is_cancelled() {
        return Err(InventoryError::Cancelled);
    }
    let command =
        CommandSpec::structured(program, args.iter().map(|arg| CommandArgument::plain(*arg)))
            .map_err(|_| InventoryError::Remote)?;
    remote.command(&command, cancellation).await
}

fn marker_error(error: &MarkerError) -> InventoryError {
    match error {
        MarkerError::Cancelled => InventoryError::Cancelled,
        MarkerError::Remote => InventoryError::Remote,
        _ => InventoryError::Root,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::domain::{
        ComponentGeneration, ComponentName, ComponentRelease, DestinationKey, DestinationRevision,
        EnvironmentId, ProjectId,
    };

    #[derive(Clone)]
    struct Node {
        kind: u8,
        data: Vec<u8>,
        size: u64,
        bad_gzip: bool,
    }
    impl Node {
        fn directory() -> Self {
            Self {
                kind: b'd',
                data: Vec::new(),
                size: 0,
                bad_gzip: false,
            }
        }
        fn file(data: Vec<u8>) -> Self {
            Self {
                kind: b'f',
                size: u64::try_from(data.len()).unwrap(),
                data,
                bad_gzip: false,
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        Hang,
        ChangedStamp,
        ChangedListing,
        Truncated,
        CommandTimeout(u32),
    }
    struct MockRemote {
        nodes: BTreeMap<String, Node>,
        commands: Mutex<Vec<CommandSpec>>,
        root: Result<bool, InventoryError>,
        current: Result<Option<ReleaseVersion>, InventoryError>,
        fault: Fault,
    }
    impl MockRemote {
        fn new() -> Self {
            Self {
                nodes: BTreeMap::from([
                    ("/srv/app/archives".into(), Node::directory()),
                    ("/srv/app/releases".into(), Node::directory()),
                ]),
                commands: Mutex::new(Vec::new()),
                root: Ok(true),
                current: Ok(None),
                fault: Fault::None,
            }
        }
        fn add(&mut self, manifest: &ReleaseManifest, extracted: bool) {
            self.nodes.insert(
                format!("/srv/app/archives/{}.tar.gz", manifest.version),
                Node::file(prefix(manifest)),
            );
            if extracted {
                self.nodes.insert(
                    format!("/srv/app/releases/{}", manifest.version),
                    Node::directory(),
                );
                self.nodes.insert(
                    format!("/srv/app/releases/{}/manifest.json", manifest.version),
                    Node::file(serde_json::to_vec(manifest).unwrap()),
                );
            }
        }
        fn listing(&self, root: &str, previous: usize) -> Vec<u8> {
            let mut bytes = Vec::new();
            for (path, node) in &self.nodes {
                if let Some(name) = path.strip_prefix(&format!("{root}/"))
                    && !name.contains('/')
                {
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.push(0);
                    bytes.push(node.kind);
                    bytes.push(0);
                    bytes.extend_from_slice(node.size.to_string().as_bytes());
                    bytes.push(0);
                }
            }
            if self.fault == Fault::ChangedListing && previous > 0 {
                bytes.extend_from_slice(b"injected\0f\x001\0");
            }
            bytes
        }
    }

    #[async_trait]
    impl InventoryRemote for MockRemote {
        async fn check_root(
            &self,
            _: &LinuxSshTarget,
            _: &DeploymentMarker,
            _: &CancellationToken,
        ) -> Result<bool, InventoryError> {
            self.root.clone()
        }
        async fn current(
            &self,
            _: &LinuxSshTarget,
            _: &DeploymentMarker,
            _: &CancellationToken,
        ) -> Result<Option<ReleaseVersion>, InventoryError> {
            self.current.clone()
        }
        async fn command(
            &self,
            command: &CommandSpec,
            cancellation: &CancellationToken,
        ) -> Result<RemoteCommandOutput, InventoryError> {
            if self.fault == Fault::Hang {
                cancellation.cancelled().await;
                return Err(InventoryError::Cancelled);
            }
            let mut args = command
                .args
                .iter()
                .map(CommandArgument::expose_for_execution)
                .collect::<Vec<_>>();
            let mut program = command.program.as_str();
            if program == "timeout" {
                assert_eq!(&args[..3], &["--signal=TERM", "--kill-after=2s", "15s"]);
                program = args[3];
                args.drain(..4);
            }
            let previous = self
                .commands
                .lock()
                .unwrap()
                .iter()
                .filter(|previous| *previous == command)
                .count();
            self.commands.lock().unwrap().push(command.clone());
            let path = *args.last().unwrap();
            let mut output = RemoteCommandOutput {
                exit_status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            };
            match program {
                "test" => {
                    let node = self.nodes.get(path);
                    let yes = match args[0] {
                        "-e" => node.is_some(),
                        "-L" => node.is_some_and(|node| node.kind == b'l'),
                        "-f" => node.is_some_and(|node| node.kind == b'f'),
                        "-d" => node.is_some_and(|node| node.kind == b'd'),
                        _ => panic!("unexpected test"),
                    };
                    output.exit_status = u32::from(!yes);
                }
                "find" => {
                    assert_eq!(
                        &args[1..],
                        &[
                            "-mindepth",
                            "1",
                            "-maxdepth",
                            "1",
                            "-printf",
                            "%f\\0%y\\0%s\\0"
                        ]
                    );
                    output.stdout = self.listing(args[0], previous);
                    output.stdout_truncated = self.fault == Fault::Truncated;
                }
                "stat" => {
                    assert_eq!(args[0], "--format=%d:%i:%s:%y:%z");
                    let node = self.nodes.get(path).unwrap();
                    let inode = if self.fault == Fault::ChangedStamp && previous > 0 {
                        2
                    } else {
                        1
                    };
                    output.stdout=format!("1:{inode}:{}:2026-09-04 12:00:00.000000000 +0000:2026-09-04 12:00:00.000000000 +0000\n",node.size).into_bytes();
                }
                "sha256sum" => output.stdout = format!("{}  {path}\n", "a".repeat(64)).into_bytes(),
                "gzip" => {
                    assert_eq!(&args[..2], &["--test", "--"]);
                    if self.nodes[path].bad_gzip {
                        output.exit_status = 1;
                        output.stderr = b"SECRET arbitrary server output".to_vec();
                    }
                    if let Fault::CommandTimeout(status) = self.fault {
                        output.exit_status = status;
                    }
                }
                "sh" => {
                    assert_eq!(
                        &args[..3],
                        &["-c", MANIFEST_PREFIX_SCRIPT, "shipforge-inventory"]
                    );
                    output.stdout = self.nodes[path].data.clone();
                }
                "head" => {
                    assert_eq!(&args[..3], &["-c", "8193", "--"]);
                    output.stdout = self.nodes[path].data.iter().copied().take(8193).collect();
                }
                _ => panic!("unexpected command {program}"),
            }
            Ok(output)
        }
    }

    fn fixture() -> (LinuxSshTarget, DeploymentMarker, ReleaseManifest) {
        let release = ComponentRelease {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("worker").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse("v1").unwrap(),
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
        };
        let target = LinuxSshTarget::validate(&crate::drivers::DriverTargetInput {
            value: serde_json::json!({"root":"/srv/app"}),
        })
        .unwrap();
        (
            target,
            DeploymentMarker::for_release(&release),
            ReleaseManifest::new(&release, 123, Some("abcdef12".into())),
        )
    }
    fn prefix(manifest: &ReleaseManifest) -> Vec<u8> {
        let data = serde_json::to_vec(manifest).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_path("manifest.json").unwrap();
        header.set_size(u64::try_from(data.len()).unwrap());
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(&data);
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
        bytes
    }
    async fn inventory(
        remote: &MockRemote,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
    ) -> Result<ReleaseInventory, InventoryError> {
        inventory_with_remote(
            remote,
            target,
            marker,
            &CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
    }

    #[tokio::test]
    async fn inventory_combines_archive_and_extracted_namespace_without_inferred_health() {
        let (target, marker, manifest) = fixture();
        let mut remote = MockRemote::new();
        remote.add(&manifest, true);
        remote.current = Ok(Some(manifest.version.clone()));
        let result = inventory(&remote, &target, &marker).await.unwrap();
        assert_eq!(result.releases.len(), 1);
        assert_eq!(result.releases[0].manifest, manifest);
        assert!(result.releases[0].extracted);
        assert_eq!(result.releases[0].sha256, "a".repeat(64));
        assert_eq!(result.current, Ok(Some(manifest.version)));
        assert!(result.issues.is_empty());
        assert!(result.notices[0].contains("not historical health"));
        assert!(
            remote
                .commands
                .lock()
                .unwrap()
                .iter()
                .all(|command| !matches!(command.program.as_str(), "mkdir" | "rm" | "ln" | "mv"))
        );
    }

    #[tokio::test]
    async fn missing_or_empty_root_is_empty_but_nonempty_unmarked_root_is_rejected() {
        let (target, marker, _) = fixture();
        let mut remote = MockRemote::new();
        remote.root = Ok(false);
        let result = inventory(&remote, &target, &marker).await.unwrap();
        assert!(result.releases.is_empty());
        assert_eq!(result.current, Ok(None));
        assert!(remote.commands.lock().unwrap().is_empty());
        remote.root = Err(InventoryError::Root);
        assert_eq!(
            inventory(&remote, &target, &marker).await,
            Err(InventoryError::Root)
        );
    }

    #[tokio::test]
    async fn archive_only_is_available_metadata_with_explicit_not_extracted_issue() {
        let (target, marker, manifest) = fixture();
        let mut remote = MockRemote::new();
        remote.add(&manifest, false);
        let result = inventory(&remote, &target, &marker).await.unwrap();
        assert_eq!(result.releases.len(), 1);
        assert!(!result.releases[0].extracted);
        assert!(result.issues[0].message.contains("no extracted directory"));
        assert_eq!(result.issues[0].version, Some(manifest.version));
    }

    #[tokio::test]
    async fn directory_only_symlink_archives_and_invalid_names_are_safe_issues() {
        let (target, marker, _) = fixture();
        let mut remote = MockRemote::new();
        remote
            .nodes
            .insert("/srv/app/releases/orphan".into(), Node::directory());
        let mut link = Node::file(Vec::new());
        link.kind = b'l';
        remote
            .nodes
            .insert("/srv/app/archives/linked.tar.gz".into(), link);
        remote.nodes.insert(
            "/srv/app/archives/SECRET\nfile".into(),
            Node::file(Vec::new()),
        );
        let result = inventory(&remote, &target, &marker).await.unwrap();
        assert!(result.releases.is_empty());
        assert_eq!(result.issues.len(), 3);
        assert!(
            result
                .issues
                .iter()
                .all(|issue| !issue.message.contains("SECRET"))
        );
        assert!(remote.commands.lock().unwrap().iter().all(|command| {
            !command
                .args
                .iter()
                .any(|arg| arg.expose_for_execution().contains("SECRET"))
        }));
    }

    #[tokio::test]
    async fn invalid_gzip_and_changed_archive_are_reported_without_raw_output() {
        let (target, marker, manifest) = fixture();
        let mut remote = MockRemote::new();
        remote.add(&manifest, true);
        remote
            .nodes
            .get_mut("/srv/app/archives/v1.tar.gz")
            .unwrap()
            .bad_gzip = true;
        let result = inventory(&remote, &target, &marker).await.unwrap();
        assert!(result.releases.is_empty());
        assert!(!result.issues[0].message.contains("SECRET"));
        remote
            .nodes
            .get_mut("/srv/app/archives/v1.tar.gz")
            .unwrap()
            .bad_gzip = false;
        remote.fault = Fault::ChangedStamp;
        remote.commands.lock().unwrap().clear();
        assert!(
            inventory(&remote, &target, &marker)
                .await
                .unwrap()
                .releases
                .is_empty()
        );
    }

    #[tokio::test]
    async fn mismatched_or_linked_extracted_manifest_does_not_create_verified_release() {
        for linked in [false, true] {
            let (target, marker, manifest) = fixture();
            let mut remote = MockRemote::new();
            remote.add(&manifest, true);
            let entry = remote
                .nodes
                .get_mut("/srv/app/releases/v1/manifest.json")
                .unwrap();
            if linked {
                entry.kind = b'l';
            } else {
                let mut wrong = manifest.clone();
                wrong.created_at_unix += 1;
                entry.data = serde_json::to_vec(&wrong).unwrap();
            }
            let result = inventory(&remote, &target, &marker).await.unwrap();
            assert!(result.releases.is_empty());
            assert_eq!(result.issues.len(), 1);
        }
    }

    #[tokio::test]
    async fn current_observation_error_is_not_converted_to_absence() {
        let (target, marker, manifest) = fixture();
        let mut remote = MockRemote::new();
        remote.add(&manifest, true);
        remote.current = Err(InventoryError::Manifest);
        let result = inventory(&remote, &target, &marker).await.unwrap();
        assert_eq!(result.releases.len(), 1);
        assert!(result.current.is_err());
    }

    #[tokio::test]
    async fn incomplete_namespace_scan_and_truncated_output_never_return_partial_inventory() {
        let (target, marker, manifest) = fixture();
        let mut remote = MockRemote::new();
        remote.add(&manifest, true);
        remote.fault = Fault::Truncated;
        assert_eq!(
            inventory(&remote, &target, &marker).await,
            Err(InventoryError::Limit)
        );
        remote.fault = Fault::ChangedListing;
        remote.commands.lock().unwrap().clear();
        assert_eq!(
            inventory(&remote, &target, &marker).await,
            Err(InventoryError::Remote)
        );
    }

    #[tokio::test]
    async fn oversized_archive_and_total_archive_bytes_abort_explicitly() {
        let (target, marker, manifest) = fixture();
        let mut remote = MockRemote::new();
        remote.add(&manifest, false);
        remote
            .nodes
            .get_mut("/srv/app/archives/v1.tar.gz")
            .unwrap()
            .size = MAX_ARCHIVE_BYTES + 1;
        assert_eq!(
            inventory(&remote, &target, &marker).await,
            Err(InventoryError::Limit)
        );
        remote.nodes.remove("/srv/app/archives/v1.tar.gz");
        for index in 0..5 {
            let mut manifest = manifest.clone();
            manifest.version = ReleaseVersion::parse(format!("v{index}")).unwrap();
            remote.add(&manifest, false);
            remote
                .nodes
                .get_mut(&format!("/srv/app/archives/v{index}.tar.gz"))
                .unwrap()
                .size = MAX_ARCHIVE_BYTES;
        }
        assert_eq!(
            inventory(&remote, &target, &marker).await,
            Err(InventoryError::Limit)
        );
    }

    #[tokio::test]
    async fn cancelled_and_stalled_scans_are_bounded() {
        let (target, marker, _) = fixture();
        let mut remote = MockRemote::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert_eq!(
            inventory_with_remote(
                &remote,
                &target,
                &marker,
                &cancellation,
                Duration::from_secs(1)
            )
            .await,
            Err(InventoryError::Cancelled)
        );
        assert!(remote.commands.lock().unwrap().is_empty());
        remote.fault = Fault::Hang;
        assert_eq!(
            inventory_with_remote(
                &remote,
                &target,
                &marker,
                &CancellationToken::new(),
                Duration::from_millis(1)
            )
            .await,
            Err(InventoryError::Timeout)
        );
    }

    #[tokio::test]
    async fn remote_timeout_is_not_misreported_as_a_corrupt_archive_or_partial_success() {
        let (target, marker, manifest) = fixture();
        for status in [124, 137] {
            let mut remote = MockRemote::new();
            remote.add(&manifest, true);
            remote.fault = Fault::CommandTimeout(status);
            assert_eq!(
                inventory(&remote, &target, &marker).await,
                Err(InventoryError::Timeout)
            );
        }
    }

    #[test]
    fn listing_parser_rejects_bad_framing_duplicates_and_entry_overflow() {
        for bytes in [
            b"name\0f\x001".as_slice(),
            b"name\0f\0secret\0",
            b"name\0ff\x001\0",
            b"name\0f\x001\0name\0f\x001\0",
        ] {
            assert!(parse_listing(bytes).is_err());
        }
        let bytes = (0..=MAX_ENTRIES)
            .flat_map(|index| format!("v{index}\0f\x001\0").into_bytes())
            .collect::<Vec<_>>();
        assert_eq!(parse_listing(&bytes), Err(InventoryError::Limit));
        assert_eq!(
            parse_listing(&vec![1; MAX_LIST_BYTES + 1]),
            Err(InventoryError::Limit)
        );
    }

    #[test]
    fn manifest_prefix_requires_canonical_first_member_checksum_type_size_and_identity() {
        let (_, marker, manifest) = fixture();
        let good = prefix(&manifest);
        assert_eq!(
            parse_archive_manifest(&good, &marker, &manifest.version).unwrap(),
            manifest
        );
        for change in 0..6 {
            let mut invalid = good.clone();
            match change {
                0 => invalid[0] = b'x',
                1 => {
                    let header = tar::Header::from_byte_slice(&invalid[..512]).clone();
                    let mut header = header;
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_cksum();
                    invalid[..512].copy_from_slice(header.as_bytes());
                }
                2 => {
                    let mut header = tar::Header::from_byte_slice(&invalid[..512]).clone();
                    header.set_size(u64::try_from(MANIFEST_BYTES + 1).unwrap());
                    header.set_cksum();
                    invalid[..512].copy_from_slice(header.as_bytes());
                }
                3 => invalid.truncate(511),
                4 => invalid.resize(MANIFEST_PREFIX_BYTES + 1, 0),
                _ => {
                    let mut wrong = manifest.clone();
                    wrong.generation = ComponentGeneration::INITIAL.checked_next().unwrap();
                    invalid = prefix(&wrong);
                }
            }
            assert_eq!(
                parse_archive_manifest(&invalid, &marker, &manifest.version),
                Err(InventoryError::Manifest)
            );
        }
    }

    #[test]
    fn source_revision_and_manifest_size_are_bounded_without_exposing_secret_content() {
        let (_, marker, manifest) = fixture();
        let mut wrong = manifest.clone();
        wrong.source_revision = Some("SECRET\n".into());
        assert_eq!(
            parse_archive_manifest(&prefix(&wrong), &marker, &manifest.version),
            Err(InventoryError::Manifest)
        );
        assert_eq!(
            parse_manifest(&vec![b' '; MANIFEST_BYTES + 1], &marker, &manifest.version),
            Err(InventoryError::Manifest)
        );
        assert!(parse_hash(b"SECRET invalid digest").is_err());
    }
}
