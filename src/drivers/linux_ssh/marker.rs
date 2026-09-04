use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    ComponentGeneration, ComponentName, ComponentRelease, EnvironmentId, ProjectId,
    ReleaseManifest, ReleaseVersion,
};

const MAX_MARKER_BYTES: usize = 4096;
pub(super) const MAX_MANIFEST_BYTES: usize = 8192;
const MARKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl super::AuthenticatedSession {
    /// Rejects linked or non-directory roots and internal layout directories.
    /// Missing directories are allowed for a first deployment.
    ///
    /// # Errors
    /// Rejects unsafe paths, failed inspection, and cancellation.
    pub async fn check_release_layout(
        &self,
        target: &super::LinuxSshTarget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), MarkerError> {
        let mut ancestor = target.root.as_str();
        loop {
            check_optional_directory(self, ancestor, cancellation).await?;
            let Some((parent, _)) = ancestor.rsplit_once('/') else {
                break;
            };
            if parent.is_empty() {
                break;
            }
            ancestor = parent;
        }
        for child in ["temporary", "archives", "releases", "metadata"] {
            check_optional_directory(self, &format!("{}/{child}", target.root), cancellation)
                .await?;
        }
        Ok(())
    }

    /// Reads the actual Release identity rather than inferring it from `current`.
    ///
    /// # Errors
    /// Rejects unsafe paths, missing or invalid metadata, and identity mismatch.
    pub async fn check_release_manifest(
        &self,
        target: &super::LinuxSshTarget,
        expected: &DeploymentMarker,
        version: &ReleaseVersion,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), MarkerError> {
        self.check_release_layout(target, cancellation).await?;
        let directory = format!("{}/releases/{version}", target.root);
        check_optional_directory(self, &directory, cancellation).await?;
        let manifest = format!("{directory}/manifest.json");
        if marker_test(self, "-L", &manifest, cancellation).await?
            || !marker_test(self, "-f", &manifest, cancellation).await?
        {
            return Err(MarkerError::ManifestMalformed);
        }
        let bytes =
            marker_command(self, "head", &["-c", "8193", "--", &manifest], cancellation).await?;
        expected.validate_manifest(&bytes, version)
    }

    /// Creates a marker only for a new or empty root, without replacing files.
    ///
    /// # Errors
    /// Rejects nonempty unmarked roots, conflicts, unsafe paths, and I/O failures.
    /// A failed publish may leave its uniquely named temporary marker for inspection.
    pub async fn ensure_deployment_marker(
        &self,
        target: &super::LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), MarkerError> {
        use std::io::Write as _;
        if self
            .check_deployment_marker(target, expected, cancellation)
            .await?
        {
            return Ok(());
        }
        self.check_unmarked_root(target, cancellation).await?;
        marker_command(
            self,
            "mkdir",
            &["--parents", "--", &target.root],
            cancellation,
        )
        .await?;
        self.check_unmarked_root(target, cancellation).await?;
        let mut local = tempfile::NamedTempFile::new().map_err(|_| MarkerError::Remote)?;
        local
            .write_all(&expected.encode()?)
            .map_err(|_| MarkerError::Remote)?;
        let temporary = super::RemotePath::parse(format!(
            "{}/.shipforge-marker-{}.tmp",
            target.root,
            uuid::Uuid::now_v7().simple()
        ))
        .map_err(|_| MarkerError::UnsafePath)?;
        self.upload_release(
            local.path(),
            &temporary,
            super::UploadOptions::default(),
            cancellation,
            |_| {},
        )
        .await
        .map_err(|_| MarkerError::Remote)?;
        let marker = format!("{}/.shipforge-project.json", target.root);
        // A hard link publishes the complete file exclusively; an existing
        // marker (including a dangling symlink) cannot be overwritten.
        marker_command(
            self,
            "ln",
            &["--no-target-directory", "--", temporary.as_str(), &marker],
            cancellation,
        )
        .await?;
        marker_command(self, "rm", &["--", temporary.as_str()], cancellation).await?;
        if self
            .check_deployment_marker(target, expected, cancellation)
            .await?
        {
            Ok(())
        } else {
            Err(MarkerError::Missing)
        }
    }

    /// Checks whether an unmarked root is safe to initialize without mutation.
    ///
    /// # Errors
    /// Rejects nonempty roots, non-directories, or remote inspection failure.
    pub async fn check_unmarked_root(
        &self,
        target: &super::LinuxSshTarget,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<(), MarkerError> {
        if !marker_test(self, "-e", &target.root, cancellation).await? {
            return Ok(());
        }
        if marker_test(self, "-L", &target.root, cancellation).await?
            || !marker_test(self, "-d", &target.root, cancellation).await?
        {
            return Err(MarkerError::UnsafePath);
        }
        let output = marker_command(
            self,
            "find",
            &[
                &target.root,
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "-print",
                "-quit",
            ],
            cancellation,
        )
        .await?;
        if !output.is_empty() {
            return Err(MarkerError::UnmarkedNonempty);
        }
        Ok(())
    }

    /// Reads a marker without changing the remote filesystem.
    /// Returns false only when the marker is absent; malformed or unsafe paths fail.
    ///
    /// # Errors
    /// Rejects linked paths, non-regular markers, read failures, oversized data,
    /// identity conflicts, cancellation, and timeout.
    pub async fn check_deployment_marker(
        &self,
        target: &super::LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, MarkerError> {
        self.check_release_layout(target, cancellation).await?;
        tokio::select! {
            () = cancellation.cancelled() => Err(MarkerError::Cancelled),
            result = tokio::time::timeout(MARKER_TIMEOUT, read_marker(self, target, expected, cancellation)) => {
                result.map_err(|_| MarkerError::Remote)?.map(|()| true).or_else(|error| {
                    if error == MarkerError::Missing { Ok(false) } else { Err(error) }
                })
            }
        }
    }
}

async fn check_optional_directory(
    session: &super::AuthenticatedSession,
    path: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(), MarkerError> {
    if marker_test(session, "-L", path, cancellation).await?
        || (marker_test(session, "-e", path, cancellation).await?
            && !marker_test(session, "-d", path, cancellation).await?)
    {
        return Err(MarkerError::UnsafePath);
    }
    Ok(())
}

async fn marker_command(
    session: &super::AuthenticatedSession,
    program: &str,
    args: &[&str],
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>, MarkerError> {
    let command = crate::telemetry::CommandSpec::structured(
        program,
        args.iter()
            .map(|arg| crate::telemetry::CommandArgument::plain(*arg)),
    )
    .map_err(|_| MarkerError::Remote)?;
    let output = session
        .execute(&command, MARKER_TIMEOUT, cancellation)
        .await
        .map_err(|_| MarkerError::Remote)?;
    if output.exit_status != 0 || output.stdout_truncated {
        return Err(MarkerError::Remote);
    }
    Ok(output.stdout)
}

async fn read_marker(
    session: &super::AuthenticatedSession,
    target: &super::LinuxSshTarget,
    expected: &DeploymentMarker,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(), MarkerError> {
    let path = format!("{}/.shipforge-project.json", target.root);
    let mut ancestor = path.as_str();
    loop {
        if marker_test(session, "-L", ancestor, cancellation).await? {
            return Err(MarkerError::UnsafePath);
        }
        let Some((parent, _)) = ancestor.rsplit_once('/') else {
            break;
        };
        if parent.is_empty() {
            break;
        }
        ancestor = parent;
    }
    if !marker_test(session, "-e", &path, cancellation).await? {
        return Err(MarkerError::Missing);
    }
    if !marker_test(session, "-f", &path, cancellation).await? {
        return Err(MarkerError::UnsafePath);
    }
    let command = crate::telemetry::CommandSpec::structured(
        "head",
        ["-c", "4097", "--", &path].map(crate::telemetry::CommandArgument::plain),
    )
    .map_err(|_| MarkerError::Remote)?;
    let output = session
        .execute(&command, MARKER_TIMEOUT, cancellation)
        .await
        .map_err(|_| MarkerError::Remote)?;
    if output.exit_status != 0 || output.stdout_truncated {
        return Err(MarkerError::Remote);
    }
    expected.validate(&output.stdout)
}

async fn marker_test(
    session: &super::AuthenticatedSession,
    flag: &str,
    path: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<bool, MarkerError> {
    let command = crate::telemetry::CommandSpec::structured(
        "test",
        [flag, path].map(crate::telemetry::CommandArgument::plain),
    )
    .map_err(|_| MarkerError::Remote)?;
    let output = session
        .execute_allowing(&command, MARKER_TIMEOUT, cancellation, &[0, 1])
        .await
        .map_err(|_| MarkerError::Remote)?;
    match output.exit_status {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(MarkerError::Remote),
    }
}

/// Static identity of a Component root, independent of any running process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentMarker {
    project_id: ProjectId,
    environment_id: EnvironmentId,
    component: ComponentName,
    generation: ComponentGeneration,
}

impl DeploymentMarker {
    pub(super) fn validate_manifest(
        &self,
        bytes: &[u8],
        version: &ReleaseVersion,
    ) -> Result<(), MarkerError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(MarkerError::ManifestMalformed);
        }
        let manifest: ReleaseManifest =
            serde_json::from_slice(bytes).map_err(|_| MarkerError::ManifestMalformed)?;
        if manifest.schema_version != 1 {
            return Err(MarkerError::ManifestMalformed);
        }
        if manifest.project_id != self.project_id
            || manifest.environment_id != self.environment_id
            || manifest.component != self.component
            || manifest.generation != self.generation
            || &manifest.version != version
        {
            return Err(MarkerError::ManifestConflict);
        }
        Ok(())
    }

    #[must_use]
    pub fn for_context(context: &crate::drivers::ComponentExecutionContext) -> Self {
        Self {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
        }
    }

    #[must_use]
    pub fn for_release(release: &ComponentRelease) -> Self {
        Self {
            project_id: release.project_id.clone(),
            environment_id: release.environment_id.clone(),
            component: release.component.clone(),
            generation: release.generation,
        }
    }

    /// Parses a bounded marker and checks all four identity fields.
    ///
    /// # Errors
    /// Rejects malformed, oversized, incomplete, or conflicting markers.
    pub fn validate(&self, bytes: &[u8]) -> Result<(), MarkerError> {
        if bytes.len() > MAX_MARKER_BYTES {
            return Err(MarkerError::Oversized);
        }
        let actual: Self = serde_json::from_slice(bytes).map_err(|_| MarkerError::Malformed)?;
        if &actual != self {
            return Err(MarkerError::Conflict);
        }
        Ok(())
    }

    /// Encodes the non-secret identity for exclusive remote creation.
    ///
    /// # Errors
    /// Returns an error if the identity cannot be serialized.
    pub fn encode(&self) -> Result<Vec<u8>, MarkerError> {
        serde_json::to_vec(self).map_err(|_| MarkerError::Malformed)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MarkerError {
    #[error("Release manifest is missing, unsafe, malformed, or oversized")]
    ManifestMalformed,
    #[error("Release manifest identity or version differs from the requested Component Release")]
    ManifestConflict,
    #[error("Component root is nonempty but has no Deployment Marker; choose a new empty root")]
    UnmarkedNonempty,
    #[error("Deployment Marker does not exist")]
    Missing,
    #[error("Deployment Marker path is not a regular unlinked file")]
    UnsafePath,
    #[error("Deployment Marker operation was cancelled")]
    Cancelled,
    #[error("Deployment Marker could not be read safely")]
    Remote,
    #[error("Deployment Marker exceeds the size limit")]
    Oversized,
    #[error("Deployment Marker is malformed; do not overwrite it automatically")]
    Malformed,
    #[error("Component root belongs to a different Project, Environment, Component, or generation")]
    Conflict,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> DeploymentMarker {
        DeploymentMarker {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
        }
    }

    #[test]
    fn marker_round_trip_and_each_identity_field_are_checked() {
        let expected = expected();
        let encoded = expected.encode().unwrap();
        assert_eq!(expected.validate(&encoded), Ok(()));
        let original: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        for (field, replacement) in [
            ("projectId", serde_json::json!(ProjectId::new())),
            ("environmentId", serde_json::json!(EnvironmentId::new())),
            ("component", serde_json::json!("worker")),
            ("generation", serde_json::json!(2)),
        ] {
            let mut changed = original.clone();
            changed[field] = replacement;
            assert_eq!(
                expected.validate(&serde_json::to_vec(&changed).unwrap()),
                Err(MarkerError::Conflict)
            );
        }
    }

    #[test]
    fn invalid_marker_data_is_rejected_without_echoing_its_content() {
        let expected = expected();
        for bytes in [
            b"{}".as_slice(),
            b"secret invalid JSON",
            b"{\"generation\":0}",
        ] {
            assert_eq!(expected.validate(bytes), Err(MarkerError::Malformed));
        }
        assert_eq!(
            expected.validate(&vec![b' '; MAX_MARKER_BYTES + 1]),
            Err(MarkerError::Oversized)
        );
        let mut extra: serde_json::Value =
            serde_json::from_slice(&expected.encode().unwrap()).unwrap();
        extra["unexpected"] = serde_json::json!(true);
        assert_eq!(
            expected.validate(&serde_json::to_vec(&extra).unwrap()),
            Err(MarkerError::Malformed)
        );
    }

    #[test]
    fn remote_manifest_checks_every_identity_field_and_schema() {
        let expected = expected();
        let version = ReleaseVersion::parse("v1").unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "projectId": expected.project_id,
            "environmentId": expected.environment_id,
            "component": expected.component,
            "generation": expected.generation,
            "version": version,
            "createdAtUnix": 1,
            "sourceRevision": null,
        });
        assert_eq!(
            expected.validate_manifest(&serde_json::to_vec(&manifest).unwrap(), &version),
            Ok(())
        );
        for (field, value) in [
            ("projectId", serde_json::json!(ProjectId::new())),
            ("environmentId", serde_json::json!(EnvironmentId::new())),
            ("component", serde_json::json!("worker")),
            ("generation", serde_json::json!(2)),
            ("version", serde_json::json!("v2")),
        ] {
            let mut wrong = manifest.clone();
            wrong[field] = value;
            assert_eq!(
                expected.validate_manifest(&serde_json::to_vec(&wrong).unwrap(), &version),
                Err(MarkerError::ManifestConflict)
            );
        }
        for bytes in [b"invalid".to_vec(), vec![b' '; MAX_MANIFEST_BYTES + 1]] {
            assert_eq!(
                expected.validate_manifest(&bytes, &version),
                Err(MarkerError::ManifestMalformed)
            );
        }
        let mut wrong_schema = manifest;
        wrong_schema["schemaVersion"] = serde_json::json!(2);
        assert_eq!(
            expected.validate_manifest(&serde_json::to_vec(&wrong_schema).unwrap(), &version),
            Err(MarkerError::ManifestMalformed)
        );
    }
}
