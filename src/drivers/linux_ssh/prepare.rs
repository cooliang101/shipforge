use std::{collections::BTreeSet, path::Path, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{ComponentRelease, DeploymentId},
    drivers::ReleasePackage,
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    AuthenticatedSession, LinuxSshTarget, RemoteCommandOutput, RemotePath, SshConnectionError,
    UploadError, UploadOptions, UploadProgress, transfer::sanitize_remote_error,
};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrepareReleaseOptions {
    pub upload: UploadOptions,
    pub command_timeout: Duration,
}

impl Default for PrepareReleaseOptions {
    fn default() -> Self {
        Self {
            upload: UploadOptions::default(),
            command_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRemoteRelease {
    deployment: DeploymentId,
    release: ComponentRelease,
    archive_path: RemotePath,
    release_directory: String,
    sha256: String,
    size: u64,
}

impl PreparedRemoteRelease {
    pub(super) fn new(
        deployment: DeploymentId,
        release: ComponentRelease,
        archive_path: RemotePath,
        release_directory: String,
        sha256: String,
        size: u64,
    ) -> Self {
        Self {
            deployment,
            release,
            archive_path,
            release_directory,
            sha256,
            size,
        }
    }

    #[must_use]
    pub fn deployment(&self) -> &DeploymentId {
        &self.deployment
    }

    #[must_use]
    pub fn release(&self) -> &ComponentRelease {
        &self.release
    }

    #[must_use]
    pub fn archive_path(&self) -> &RemotePath {
        &self.archive_path
    }

    #[must_use]
    pub fn release_directory(&self) -> &str {
        &self.release_directory
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum OwnedPath {
    TemporaryFile,
    StagingDirectory,
    Archive,
    FinalDirectory,
}

type PrepareState = BTreeSet<OwnedPath>;

#[derive(Clone, Debug)]
pub(super) struct ReleasePaths {
    pub(super) temporary_directory: String,
    pub(super) archives_directory: String,
    pub(super) release_parent: String,
    pub(super) temporary_file: RemotePath,
    pub(super) staging_directory: String,
    pub(super) archive: RemotePath,
    pub(super) final_directory: String,
    pub(super) manifest: String,
}

impl ReleasePaths {
    pub(super) fn new(
        target: &LinuxSshTarget,
        release: &ComponentRelease,
        deployment: &DeploymentId,
    ) -> Self {
        let temporary_directory = format!("{}/temporary", target.root);
        let archives_directory = format!("{}/archives", target.root);
        let release_parent = format!("{}/releases", target.root);
        let deployment = deployment.to_string();
        let version = release.version.as_str();
        let temporary_file =
            RemotePath::parse(format!("{temporary_directory}/{deployment}.tar.gz"))
                .expect("validated root and generated Deployment ID form a safe path");
        let staging_directory = format!("{temporary_directory}/{deployment}.dir");
        let archive = RemotePath::parse(format!("{archives_directory}/{version}.tar.gz"))
            .expect("validated root and Release version form a safe path");
        let final_directory = format!("{release_parent}/{version}");
        let manifest = format!("{staging_directory}/manifest.json");
        Self {
            temporary_directory,
            archives_directory,
            release_parent,
            temporary_file,
            staging_directory,
            archive,
            final_directory,
            manifest,
        }
    }
}

impl AuthenticatedSession {
    /// Uploads, verifies, archives, and extracts one already packaged Release.
    ///
    /// The operation never changes `current` or activates a service. Archive
    /// and version paths use no-clobber operations, and failures trigger
    /// cleanup limited to paths owned by this Deployment.
    ///
    /// # Errors
    ///
    /// Returns an error for a changed local package, cancellation, conflicts,
    /// transfer or remote-command failure, or incomplete cleanup.
    pub async fn prepare_release<F>(
        &self,
        target: &LinuxSshTarget,
        package: &ReleasePackage,
        deployment: &DeploymentId,
        options: PrepareReleaseOptions,
        cancellation: &CancellationToken,
        progress: F,
    ) -> Result<PreparedRemoteRelease, PrepareReleaseError>
    where
        F: Fn(UploadProgress) + Send + Sync,
    {
        prepare_with_remote(
            self,
            target,
            package,
            deployment,
            options,
            cancellation,
            &progress,
        )
        .await
    }
}

#[async_trait]
trait ReleaseRemote: Sync {
    async fn upload(
        &self,
        local_path: &Path,
        remote_path: &RemotePath,
        options: UploadOptions,
        cancellation: &CancellationToken,
        progress: &(dyn Fn(UploadProgress) + Send + Sync),
    ) -> Result<(), UploadError>;

    async fn verify_sha256(
        &self,
        remote_path: &RemotePath,
        expected: &str,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), UploadError>;

    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError>;
}

#[async_trait]
impl ReleaseRemote for AuthenticatedSession {
    async fn upload(
        &self,
        local_path: &Path,
        remote_path: &RemotePath,
        options: UploadOptions,
        cancellation: &CancellationToken,
        progress: &(dyn Fn(UploadProgress) + Send + Sync),
    ) -> Result<(), UploadError> {
        self.upload_release(local_path, remote_path, options, cancellation, progress)
            .await
            .map(|_| ())
    }

    async fn verify_sha256(
        &self,
        remote_path: &RemotePath,
        expected: &str,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), UploadError> {
        self.verify_remote_sha256(remote_path, expected, timeout, cancellation)
            .await
    }

    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.execute(command, timeout, cancellation).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_with_remote<R: ReleaseRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    package: &ReleasePackage,
    deployment: &DeploymentId,
    options: PrepareReleaseOptions,
    cancellation: &CancellationToken,
    progress: &(dyn Fn(UploadProgress) + Send + Sync),
) -> Result<PreparedRemoteRelease, PrepareReleaseError> {
    if cancellation.is_cancelled() {
        return Err(PrepareReleaseError::Cancelled);
    }
    if options.command_timeout.is_zero() {
        return Err(PrepareReleaseError::ZeroCommandTimeout);
    }
    validate_local_package(package).await?;
    let paths = ReleasePaths::new(target, package.release(), deployment);
    let mut state = PrepareState::new();
    let result = prepare_inner(
        remote,
        package,
        &paths,
        options,
        cancellation,
        progress,
        &mut state,
    )
    .await;
    match result {
        Ok(()) => Ok(PreparedRemoteRelease::new(
            deployment.clone(),
            package.release().clone(),
            paths.archive,
            paths.final_directory,
            package.sha256().to_owned(),
            package.size(),
        )),
        Err(error) => match cleanup(remote, &paths, &state, options.command_timeout).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(PrepareReleaseError::CleanupFailed {
                original: error.to_string(),
                cleanup: cleanup.to_string(),
            }),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_inner<R: ReleaseRemote>(
    remote: &R,
    package: &ReleasePackage,
    paths: &ReleasePaths,
    options: PrepareReleaseOptions,
    cancellation: &CancellationToken,
    progress: &(dyn Fn(UploadProgress) + Send + Sync),
    state: &mut PrepareState,
) -> Result<(), PrepareReleaseError> {
    initialize_and_upload(
        remote,
        package,
        paths,
        options,
        cancellation,
        progress,
        state,
    )
    .await?;
    extract_and_commit(remote, paths, options.command_timeout, cancellation, state).await
}

#[allow(clippy::too_many_arguments)]
async fn initialize_and_upload<R: ReleaseRemote>(
    remote: &R,
    package: &ReleasePackage,
    paths: &ReleasePaths,
    options: PrepareReleaseOptions,
    cancellation: &CancellationToken,
    progress: &(dyn Fn(UploadProgress) + Send + Sync),
    state: &mut PrepareState,
) -> Result<(), PrepareReleaseError> {
    run_required(
        remote,
        "create Release parent directories",
        "mkdir",
        [
            "-p",
            "--",
            &paths.temporary_directory,
            &paths.archives_directory,
            &paths.release_parent,
        ],
        options.command_timeout,
        cancellation,
    )
    .await?;
    reject_existing(
        remote,
        &paths.archive,
        options.command_timeout,
        cancellation,
    )
    .await?;
    reject_existing_path(
        remote,
        &paths.final_directory,
        options.command_timeout,
        cancellation,
    )
    .await?;

    remote
        .upload(
            package.path(),
            &paths.temporary_file,
            options.upload,
            cancellation,
            progress,
        )
        .await
        .map_err(map_upload_error)?;
    state.insert(OwnedPath::TemporaryFile);
    remote
        .verify_sha256(
            &paths.temporary_file,
            package.sha256(),
            options.command_timeout,
            cancellation,
        )
        .await
        .map_err(map_upload_error)?;

    run_required(
        remote,
        "create staging directory",
        "mkdir",
        ["--", &paths.staging_directory],
        options.command_timeout,
        cancellation,
    )
    .await?;
    state.insert(OwnedPath::StagingDirectory);
    Ok(())
}

async fn extract_and_commit<R: ReleaseRemote>(
    remote: &R,
    paths: &ReleasePaths,
    command_timeout: Duration,
    cancellation: &CancellationToken,
    state: &mut PrepareState,
) -> Result<(), PrepareReleaseError> {
    run_required(
        remote,
        "extract Release",
        "tar",
        [
            "--extract",
            "--gzip",
            "--file",
            paths.temporary_file.as_str(),
            "--directory",
            &paths.staging_directory,
            "--no-same-owner",
        ],
        command_timeout,
        cancellation,
    )
    .await?;
    run_required(
        remote,
        "validate extracted manifest",
        "test",
        ["-f", &paths.manifest],
        command_timeout,
        cancellation,
    )
    .await?;

    run_required(
        remote,
        "commit Release archive",
        "ln",
        ["--", paths.temporary_file.as_str(), paths.archive.as_str()],
        command_timeout,
        cancellation,
    )
    .await?;
    state.insert(OwnedPath::Archive);
    run_required(
        remote,
        "commit Release directory",
        "mv",
        [
            "--no-clobber",
            "--no-target-directory",
            "--",
            &paths.staging_directory,
            &paths.final_directory,
        ],
        command_timeout,
        cancellation,
    )
    .await?;
    if remote_exists(
        remote,
        &paths.staging_directory,
        command_timeout,
        cancellation,
    )
    .await?
    {
        return Err(PrepareReleaseError::Conflict(paths.final_directory.clone()));
    }
    state.remove(&OwnedPath::StagingDirectory);
    state.insert(OwnedPath::FinalDirectory);
    run_required(
        remote,
        "remove uploaded temporary Release",
        "rm",
        ["--", paths.temporary_file.as_str()],
        command_timeout,
        cancellation,
    )
    .await?;
    state.remove(&OwnedPath::TemporaryFile);
    Ok(())
}

async fn validate_local_package(package: &ReleasePackage) -> Result<(), PrepareReleaseError> {
    let metadata = tokio::fs::symlink_metadata(package.path())
        .await
        .map_err(|source| PrepareReleaseError::LocalPackage {
            path: package.path().to_owned(),
            source,
        })?;
    if !metadata.is_file() || metadata.len() != package.size() || package.size() == 0 {
        return Err(PrepareReleaseError::LocalPackageChanged {
            path: package.path().to_owned(),
            expected: package.size(),
            actual: metadata.len(),
        });
    }
    Ok(())
}

async fn reject_existing<R: ReleaseRemote>(
    remote: &R,
    path: &RemotePath,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), PrepareReleaseError> {
    reject_existing_path(remote, path.as_str(), timeout, cancellation).await
}

async fn reject_existing_path<R: ReleaseRemote>(
    remote: &R,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), PrepareReleaseError> {
    if remote_exists(remote, path, timeout, cancellation).await? {
        Err(PrepareReleaseError::Conflict(path.to_owned()))
    } else {
        Ok(())
    }
}

async fn remote_exists<R: ReleaseRemote>(
    remote: &R,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<bool, PrepareReleaseError> {
    let output = run(
        remote,
        "inspect remote path",
        "test",
        ["-e", path],
        timeout,
        cancellation,
    )
    .await?;
    match output.exit_status {
        0 => Ok(true),
        1 => Ok(false),
        status => Err(PrepareReleaseError::CommandFailed {
            stage: "inspect remote path",
            status,
        }),
    }
}

async fn run_required<R, I, S>(
    remote: &R,
    stage: &'static str,
    program: &str,
    args: I,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), PrepareReleaseError>
where
    R: ReleaseRemote,
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let output = run(remote, stage, program, args, timeout, cancellation).await?;
    if output.exit_status == 0 {
        Ok(())
    } else {
        Err(PrepareReleaseError::CommandFailed {
            stage,
            status: output.exit_status,
        })
    }
}

async fn run<R, I, S>(
    remote: &R,
    stage: &'static str,
    program: &str,
    args: I,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteCommandOutput, PrepareReleaseError>
where
    R: ReleaseRemote,
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let command = CommandSpec::structured(
        program,
        args.into_iter()
            .map(|argument| CommandArgument::plain(argument.into())),
    )
    .map_err(|error| PrepareReleaseError::InvalidCommand(error.to_string()))?;
    remote
        .command(&command, timeout, cancellation)
        .await
        .map_err(|error| map_ssh_error(stage, &error))
}

async fn cleanup<R: ReleaseRemote>(
    remote: &R,
    paths: &ReleasePaths,
    state: &PrepareState,
    command_timeout: Duration,
) -> Result<(), PrepareReleaseError> {
    let timeout = command_timeout.min(CLEANUP_TIMEOUT);
    let cancellation = CancellationToken::new();
    let mut failures = Vec::new();
    if let Some(error) = cleanup_owned(
        remote,
        state,
        OwnedPath::FinalDirectory,
        "cleanup Release directory",
        true,
        &paths.final_directory,
        timeout,
        &cancellation,
    )
    .await
    {
        failures.push(error);
    }
    if let Some(error) = cleanup_owned(
        remote,
        state,
        OwnedPath::StagingDirectory,
        "cleanup staging directory",
        true,
        &paths.staging_directory,
        timeout,
        &cancellation,
    )
    .await
    {
        failures.push(error);
    }
    if let Some(error) = cleanup_owned(
        remote,
        state,
        OwnedPath::Archive,
        "cleanup Release archive",
        false,
        paths.archive.as_str(),
        timeout,
        &cancellation,
    )
    .await
    {
        failures.push(error);
    }
    if let Some(error) = cleanup_owned(
        remote,
        state,
        OwnedPath::TemporaryFile,
        "cleanup temporary Release",
        false,
        paths.temporary_file.as_str(),
        timeout,
        &cancellation,
    )
    .await
    {
        failures.push(error);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(PrepareReleaseError::CleanupOperations(failures.join("; ")))
    }
}

#[allow(clippy::too_many_arguments)]
async fn cleanup_owned<R: ReleaseRemote>(
    remote: &R,
    state: &PrepareState,
    owned: OwnedPath,
    operation: &'static str,
    recursive: bool,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Option<String> {
    if !state.contains(&owned) {
        return None;
    }
    cleanup_path(remote, operation, recursive, path, timeout, cancellation)
        .await
        .err()
        .map(|error| error.to_string())
}

async fn cleanup_path<R: ReleaseRemote>(
    remote: &R,
    stage: &'static str,
    recursive: bool,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), PrepareReleaseError> {
    let flag = if recursive { "-rf" } else { "-f" };
    run_required(
        remote,
        stage,
        "rm",
        [flag, "--", path],
        timeout,
        cancellation,
    )
    .await
}

fn map_upload_error(error: UploadError) -> PrepareReleaseError {
    if matches!(error, UploadError::Cancelled) {
        PrepareReleaseError::Cancelled
    } else {
        PrepareReleaseError::Upload(error)
    }
}

fn map_ssh_error(stage: &'static str, error: &SshConnectionError) -> PrepareReleaseError {
    if matches!(error, SshConnectionError::Cancelled) {
        PrepareReleaseError::Cancelled
    } else {
        PrepareReleaseError::RemoteCommand {
            stage,
            message: sanitize_remote_error(&error.to_string()),
        }
    }
}

#[derive(Debug, Error)]
pub enum PrepareReleaseError {
    #[error("Release preparation was cancelled")]
    Cancelled,
    #[error("Release command timeout must be non-zero")]
    ZeroCommandTimeout,
    #[error("could not inspect local Release `{path}`: {source}")]
    LocalPackage {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "local Release changed after packaging at `{path}`: expected {expected} bytes, got {actual}"
    )]
    LocalPackageChanged {
        path: std::path::PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("remote Release path already exists and will not be overwritten: `{0}`")]
    Conflict(String),
    #[error("Release transfer failed: {0}")]
    Upload(UploadError),
    #[error("could not construct a structured remote command: {0}")]
    InvalidCommand(String),
    #[error("{stage} failed before an exit status was received: {message}")]
    RemoteCommand {
        stage: &'static str,
        message: String,
    },
    #[error("{stage} exited with status {status}")]
    CommandFailed { stage: &'static str, status: u32 },
    #[error("Release preparation failed ({original}); cleanup also failed ({cleanup})")]
    CleanupFailed { original: String, cleanup: String },
    #[error("one or more Release cleanup commands failed: {0}")]
    CleanupOperations(String),
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use super::*;
    use crate::domain::{
        ComponentGeneration, ComponentName, DestinationKey, DestinationRevision, EnvironmentId,
        ProjectId, ReleaseVersion,
    };

    #[derive(Debug, Default)]
    struct FakeRemote {
        commands: Mutex<Vec<String>>,
        statuses: Mutex<VecDeque<u32>>,
        uploads: Mutex<Vec<String>>,
        verifications: Mutex<Vec<String>>,
        fail_verification: bool,
    }

    impl FakeRemote {
        fn with_statuses(statuses: impl IntoIterator<Item = u32>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into_iter().collect()),
                ..Self::default()
            }
        }

        fn with_verification_failure(statuses: impl IntoIterator<Item = u32>) -> Self {
            Self {
                fail_verification: true,
                ..Self::with_statuses(statuses)
            }
        }
    }

    #[async_trait]
    impl ReleaseRemote for FakeRemote {
        async fn upload(
            &self,
            _local_path: &Path,
            remote_path: &RemotePath,
            _options: UploadOptions,
            _cancellation: &CancellationToken,
            progress: &(dyn Fn(UploadProgress) + Send + Sync),
        ) -> Result<(), UploadError> {
            self.uploads
                .lock()
                .unwrap()
                .push(remote_path.as_str().to_owned());
            progress(UploadProgress {
                attempt: 1,
                sent: 7,
                total: 7,
            });
            Ok(())
        }

        async fn verify_sha256(
            &self,
            remote_path: &RemotePath,
            _expected: &str,
            _timeout: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<(), UploadError> {
            self.verifications
                .lock()
                .unwrap()
                .push(remote_path.as_str().to_owned());
            if self.fail_verification {
                Err(UploadError::HashMismatch {
                    expected: "a".repeat(64),
                    actual: "b".repeat(64),
                })
            } else {
                Ok(())
            }
        }

        async fn command(
            &self,
            command: &CommandSpec,
            _timeout: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<RemoteCommandOutput, SshConnectionError> {
            self.commands
                .lock()
                .unwrap()
                .push(command.render_posix().unwrap());
            let status = self.statuses.lock().unwrap().pop_front().unwrap_or(0);
            Ok(RemoteCommandOutput {
                exit_status: status,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    fn release() -> ComponentRelease {
        ComponentRelease {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse("v1").unwrap(),
            destination: DestinationKey::parse("dst_00000000000000000000000000000001").unwrap(),
            destination_revision: DestinationRevision::INITIAL,
        }
    }

    fn target() -> LinuxSshTarget {
        LinuxSshTarget::validate(&crate::drivers::DriverTargetInput {
            value: serde_json::json!({
                "root": "/srv/shipforge/project/production/api",
                "systemd": null,
                "health": null
            }),
        })
        .unwrap()
    }

    fn package(directory: &Path) -> ReleasePackage {
        let path = directory.join("v1.tar.gz");
        std::fs::write(&path, "release").unwrap();
        ReleasePackage::new(release(), path, "a".repeat(64), 7)
    }

    #[tokio::test]
    async fn prepares_without_touching_current_and_uses_no_clobber_commits() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_statuses([0, 1, 1, 0, 0, 0, 0, 0, 1, 0]);
        let prepared = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();

        assert_eq!(&prepared.release, package.release());
        let commands = remote.commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .any(|command| command.starts_with("'ln' '--'"))
        );
        assert!(commands.iter().any(|command| {
            command.starts_with("'mv' '--no-clobber' '--no-target-directory' '--'")
        }));
        assert!(!commands.iter().any(|command| command.contains("current")));
        assert_eq!(remote.uploads.lock().unwrap().len(), 1);
        assert_eq!(remote.verifications.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn archive_conflict_stops_before_upload() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_statuses([0, 0]);
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(result, Err(PrepareReleaseError::Conflict(_))));
        assert!(remote.uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hash_failure_removes_the_owned_temporary_upload() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_verification_failure([0, 1, 1, 0]);
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(result, Err(PrepareReleaseError::Upload(_))));
        let commands = remote.commands.lock().unwrap();
        assert_eq!(
            commands.last().unwrap().split_whitespace().next(),
            Some("'rm'")
        );
        assert!(commands.last().unwrap().contains("/temporary/"));
        assert!(!commands.last().unwrap().contains("/archives/"));
    }

    #[tokio::test]
    async fn extraction_failure_cleans_only_owned_staging_and_temporary_paths() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_statuses([0, 1, 1, 0, 23, 0, 0]);
        let deployment = DeploymentId::new();
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &deployment,
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(
            result,
            Err(PrepareReleaseError::CommandFailed {
                stage: "extract Release",
                status: 23
            })
        ));
        let commands = remote.commands.lock().unwrap();
        let cleanup = &commands[commands.len() - 2..];
        assert!(cleanup[0].starts_with("'rm' '-rf' '--'"));
        assert!(cleanup[1].starts_with("'rm' '-f' '--'"));
        assert!(cleanup.iter().all(|command| !command.contains("current")));
    }

    #[tokio::test]
    async fn archive_commit_conflict_never_removes_the_unowned_archive() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_statuses([0, 1, 1, 0, 0, 0, 1, 0, 0]);
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(
            result,
            Err(PrepareReleaseError::CommandFailed {
                stage: "commit Release archive",
                status: 1
            })
        ));
        let commands = remote.commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .filter(|command| command.starts_with("'rm'"))
                .all(|command| !command.contains("/archives/"))
        );
    }

    #[tokio::test]
    async fn directory_commit_conflict_never_removes_the_unowned_final_directory() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_statuses([0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(result, Err(PrepareReleaseError::Conflict(_))));
        let commands = remote.commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .filter(|command| command.starts_with("'rm'"))
                .all(|command| !command.contains("/releases/v1"))
        );
        assert!(commands.iter().any(|command| {
            command.starts_with("'rm' '-f' '--'") && command.contains("/archives/v1.tar.gz")
        }));
    }

    #[tokio::test]
    async fn cleanup_attempts_remaining_paths_after_one_cleanup_failure() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        let remote = FakeRemote::with_statuses([0, 1, 1, 0, 23, 5, 0]);
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(
            result,
            Err(PrepareReleaseError::CleanupFailed { .. })
        ));
        let commands = remote.commands.lock().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("'rm'"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn changed_local_package_fails_before_remote_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let package = package(directory.path());
        std::fs::write(package.path(), "changed-size").unwrap();
        let remote = FakeRemote::default();
        let result = prepare_with_remote(
            &remote,
            &target(),
            &package,
            &DeploymentId::new(),
            PrepareReleaseOptions::default(),
            &CancellationToken::new(),
            &|_| {},
        )
        .await;
        assert!(matches!(
            result,
            Err(PrepareReleaseError::LocalPackageChanged { .. })
        ));
        assert!(remote.commands.lock().unwrap().is_empty());
        assert!(remote.uploads.lock().unwrap().is_empty());
    }
}
