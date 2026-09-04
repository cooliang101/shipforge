//! Explicit, previewed local registration edits. No remote deployment mutation.

mod files;
mod target_setup;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{
    DeploymentSession, DestinationSetupRequest, DestinationSetupService, EndpointProbeRequest,
    RemoteSetupCandidates, SetupCredential,
};
use crate::{
    config::{
        self, CredentialRegistry, DestinationReferences, DestinationRegistry,
        DestinationRevisionRecord, DestinationSettings, HostKeyFingerprint,
    },
    domain::DestinationKey,
    drivers::{CredentialHandle, DriverDestinationInput, DriverKind},
    history::{DestinationReferenceSummary, HistoryStore},
    projects::ProjectRegistry,
};
use files::FileSnapshot;

const MAX_PROJECTS: usize = 256;
const MAX_PROJECT_BYTES: usize = 8 * 1024 * 1024;
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct ManagementPaths {
    pub projects: PathBuf,
    pub destinations: PathBuf,
    pub credentials: PathBuf,
    pub history: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionDetails {
    pub key: DestinationKey,
    pub current: DestinationRevisionRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionCredentialDraft {
    Saved(CredentialHandle),
    New(config::SshCredential),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshConnectionDraft {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Only an existing handle or local identity reference, never secret/key bytes.
    pub credential: ConnectionCredentialDraft,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagementSource {
    Projects,
    Destinations,
    Credentials,
    History,
    ProjectConfig(PathBuf),
}

impl std::fmt::Display for ManagementSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Projects => formatter.write_str("recent-project registry"),
            Self::Destinations => formatter.write_str("connection registry"),
            Self::Credentials => formatter.write_str("credential registry"),
            Self::History => formatter.write_str("local history"),
            Self::ProjectConfig(path) => {
                let label: String = path
                    .to_string_lossy()
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect();
                write!(formatter, "Project configuration {label}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagementBlocker {
    pub source: ManagementSource,
    pub reason: &'static str,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectionManagementError {
    #[error("management is unavailable while this session is busy")]
    Busy,
    #[error("{origin}: {reason}")]
    Unavailable {
        origin: ManagementSource,
        reason: &'static str,
    },
    #[error("the preview is stale; inspect current details and confirm again")]
    Stale,
    #[error("the selected registration no longer exists")]
    Missing,
    #[error("the connection is still referenced or some references could not be verified")]
    Referenced,
    #[error("SSH setup failed during {0}; no connection settings were saved")]
    Setup(&'static str),
    #[error("operation cancelled; no connection settings were saved")]
    Cancelled,
    #[error(
        "credential reference was saved, but connection persistence is unconfirmed: {reason}; inspect current settings before retrying"
    )]
    CredentialSavedConnectionUnconfirmed {
        credential: CredentialHandle,
        reason: &'static str,
    },
}

#[derive(Clone, Debug)]
pub struct ProjectRemovalPreview {
    root: PathBuf,
    registry: FileSnapshot,
}

impl ProjectRemovalPreview {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Clone, Debug)]
pub struct DestinationRemovalPreview {
    details: ConnectionDetails,
    registry: FileSnapshot,
    projects: Option<FileSnapshot>,
    configs: Vec<FileSnapshot>,
    project_references: Vec<PathBuf>,
    history: Option<DestinationReferenceSummary>,
    history_missing: bool,
    blockers: Vec<ManagementBlocker>,
}

impl DestinationRemovalPreview {
    #[must_use]
    pub fn details(&self) -> &ConnectionDetails {
        &self.details
    }
    #[must_use]
    pub fn project_references(&self) -> &[PathBuf] {
        &self.project_references
    }
    #[must_use]
    pub fn history_references(&self) -> Option<&DestinationReferenceSummary> {
        self.history.as_ref()
    }
    #[must_use]
    pub const fn history_missing(&self) -> bool {
        self.history_missing
    }
    #[must_use]
    pub fn blockers(&self) -> &[ManagementBlocker] {
        &self.blockers
    }
    #[must_use]
    pub fn can_remove(&self) -> bool {
        self.blockers.is_empty()
            && self.project_references.is_empty()
            && self.history.as_ref().is_none_or(|history| {
                history.deployments == 0 && history.releases == 0 && history.recovery_reports == 0
            })
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionEditPreview {
    key: DestinationKey,
    details: Option<ConnectionDetails>,
    draft: SshConnectionDraft,
    registry: FileSnapshot,
    credentials: FileSnapshot,
    credential_handle: CredentialHandle,
    credential: config::SshCredential,
    updated_credentials: Option<CredentialRegistry>,
}

impl ConnectionEditPreview {
    #[must_use]
    pub fn details(&self) -> Option<&ConnectionDetails> {
        self.details.as_ref()
    }
    #[must_use]
    pub fn key(&self) -> &DestinationKey {
        &self.key
    }
    #[must_use]
    pub fn draft(&self) -> &SshConnectionDraft {
        &self.draft
    }
}

#[derive(Clone, Debug)]
pub struct HostKeyConfirmation {
    preview: ConnectionEditPreview,
    fingerprint: HostKeyFingerprint,
}

impl HostKeyConfirmation {
    #[must_use]
    pub fn preview(&self) -> &ConnectionEditPreview {
        &self.preview
    }
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        self.fingerprint.as_str()
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionManagementService {
    paths: ManagementPaths,
    session: Arc<DeploymentSession>,
    setup: DestinationSetupService,
}

impl ConnectionManagementService {
    #[must_use]
    pub fn new(
        paths: ManagementPaths,
        session: Arc<DeploymentSession>,
        setup: DestinationSetupService,
    ) -> Self {
        Self {
            paths,
            session,
            setup,
        }
    }

    /// Lists validated current records without changing registration or contacting SSH.
    /// # Errors
    /// Returns a source-labelled error for unsafe, unreadable, invalid, or excessive data.
    pub fn list_connections(&self) -> Result<Vec<ConnectionDetails>, ConnectionManagementError> {
        let (_, registry) = self.destinations()?;
        registry
            .summaries()
            .into_iter()
            .map(|summary| details(&registry, &summary.key))
            .collect()
    }

    /// Lists bounded validated credential labels without exposing private-key paths.
    /// # Errors
    /// Rejects unreadable, unsafe, malformed, or excessive registry data.
    pub fn list_credentials(
        &self,
    ) -> Result<Vec<config::CredentialSummary>, ConnectionManagementError> {
        let snapshot = FileSnapshot::read(&self.paths.credentials, ManagementSource::Credentials)?;
        Ok(credential_registry(&snapshot)?.summaries())
    }

    /// Lists recent registrations; does not open or rewrite Project configuration.
    /// # Errors
    /// Rejects an invalid or excessive recent-project registry.
    pub fn list_projects(
        &self,
    ) -> Result<Vec<crate::projects::ProjectStatus>, ConnectionManagementError> {
        let snapshot = FileSnapshot::read(&self.paths.projects, ManagementSource::Projects)?;
        let projects = project_registry(&snapshot)?.statuses();
        if projects.len() > MAX_PROJECTS {
            return Err(unavailable(
                ManagementSource::Projects,
                "too many registered Projects to display safely",
            ));
        }
        Ok(projects)
    }

    /// Previews unregistering a recent Project, even when its directory is missing.
    /// # Errors
    /// Rejects invalid registries and unregistered roots.
    pub fn preview_project_removal(
        &self,
        root: &Path,
    ) -> Result<ProjectRemovalPreview, ConnectionManagementError> {
        let snapshot = FileSnapshot::read(&self.paths.projects, ManagementSource::Projects)?;
        let registry = project_registry(&snapshot)?;
        if !registry
            .statuses()
            .iter()
            .any(|status| status.project.root == root)
        {
            return Err(ConnectionManagementError::Missing);
        }
        Ok(ProjectRemovalPreview {
            root: root.to_owned(),
            registry: snapshot,
        })
    }

    /// Unregisters only from recents. YAML, directories, credentials, and history stay intact.
    /// # Errors
    /// Rejects a busy session, stale preview, invalid data, or failed atomic registry write.
    pub async fn remove_project(
        &self,
        preview: ProjectRemovalPreview,
    ) -> Result<(), ConnectionManagementError> {
        self.session
            .run(async {
                Self::ensure_path(&preview.registry, &self.paths.projects)?;
                preview.registry.ensure_unchanged()?;
                let mut registry = project_registry(&preview.registry)?;
                if !registry.unregister(&preview.root) {
                    return Err(ConnectionManagementError::Missing);
                }
                registry
                    .save(&self.paths.projects)
                    .map_err(|_| unavailable(ManagementSource::Projects, "could not save registry"))
            })
            .await
            .map_err(|_| ConnectionManagementError::Busy)?
    }

    /// Scans all registered Project YAML and all local historical references.
    /// Missing databases are reported explicitly and never created. Unknown sources block removal.
    /// # Errors
    /// Returns an error if the connection itself cannot be inspected; reference-source
    /// failures are retained in the preview's blockers with their origin.
    pub fn preview_destination_removal(
        &self,
        key: &DestinationKey,
    ) -> Result<DestinationRemovalPreview, ConnectionManagementError> {
        let (registry, destinations) = self.destinations()?;
        let mut preview = DestinationRemovalPreview {
            details: details(&destinations, key)?,
            registry,
            projects: None,
            configs: Vec::new(),
            project_references: Vec::new(),
            history: None,
            history_missing: false,
            blockers: Vec::new(),
        };
        if let Err(error) = self.scan_projects(key, &mut preview) {
            add_blocker(&mut preview, error);
        }
        match self.read_history(key) {
            Ok(Some(history)) => preview.history = Some(history),
            Ok(None) => preview.history_missing = true,
            Err(error) => add_blocker(&mut preview, error),
        }
        Ok(preview)
    }

    /// Deletes only an unreferenced connection registration after a fresh complete scan.
    /// Retained credential records and every remote path remain untouched.
    /// # Errors
    /// Rejects busy sessions, references, unknown sources, changed previews, or write failures.
    pub async fn remove_destination(
        &self,
        preview: DestinationRemovalPreview,
    ) -> Result<(), ConnectionManagementError> {
        self.session
            .run(async {
                Self::ensure_path(&preview.registry, &self.paths.destinations)?;
                if !preview.can_remove() {
                    return Err(ConnectionManagementError::Referenced);
                }
                preview.registry.ensure_unchanged()?;
                let fresh = self.preview_destination_removal(&preview.details.key)?;
                if !fresh.can_remove() {
                    return Err(ConnectionManagementError::Referenced);
                }
                if preview.projects != fresh.projects
                    || preview.configs != fresh.configs
                    || preview.history != fresh.history
                    || preview.history_missing != fresh.history_missing
                    || preview.registry != fresh.registry
                {
                    return Err(ConnectionManagementError::Stale);
                }
                let mut registry = destination_registry(&fresh.registry)?;
                registry
                    .remove(&fresh.details.key, DestinationReferences::default())
                    .map_err(|_| ConnectionManagementError::Referenced)?;
                registry.save(&self.paths.destinations).map_err(|_| {
                    unavailable(ManagementSource::Destinations, "could not save registry")
                })
            })
            .await
            .map_err(|_| ConnectionManagementError::Busy)?
    }

    /// Freezes settings and credential references before showing an SSH identity prompt.
    /// # Errors
    /// Rejects missing credentials, invalid fields, or unreadable local registries.
    pub fn preview_edit(
        &self,
        key: &DestinationKey,
        draft: SshConnectionDraft,
    ) -> Result<ConnectionEditPreview, ConnectionManagementError> {
        let (registry, destinations) = self.destinations()?;
        let details = details(&destinations, key)?;
        self.prepare_connection(registry, key.clone(), Some(details), draft)
    }

    /// Previews a standalone connection, generating its local identity automatically.
    /// # Errors
    /// Rejects invalid fields, credentials, or local registries without changing files.
    pub fn preview_create(
        &self,
        draft: SshConnectionDraft,
    ) -> Result<ConnectionEditPreview, ConnectionManagementError> {
        let (registry, _) = self.destinations()?;
        self.prepare_connection(registry, DestinationKey::new(), None, draft)
    }

    fn prepare_connection(
        &self,
        registry: FileSnapshot,
        key: DestinationKey,
        details: Option<ConnectionDetails>,
        draft: SshConnectionDraft,
    ) -> Result<ConnectionEditPreview, ConnectionManagementError> {
        let credentials =
            FileSnapshot::read(&self.paths.credentials, ManagementSource::Credentials)?;
        let mut credential_registry = credential_registry(&credentials)?;
        let (credential_handle, credential, updated_credentials) = match &draft.credential {
            ConnectionCredentialDraft::Saved(handle) => (
                handle.clone(),
                resolve_credential(&credentials, handle)?,
                None,
            ),
            ConnectionCredentialDraft::New(credential) => {
                let handle = credential_registry
                    .create(credential.clone())
                    .map_err(|_| {
                        unavailable(ManagementSource::Credentials, "invalid identity reference")
                    })?;
                (handle, credential.clone(), Some(credential_registry))
            }
        };
        validate_draft(&draft, &credential_handle)?;
        Ok(ConnectionEditPreview {
            key,
            details,
            draft,
            registry,
            credentials,
            credential_handle,
            credential,
            updated_credentials,
        })
    }

    /// Captures the proposed endpoint fingerprint without authenticating or persisting it.
    /// The caller must show the returned fingerprint for explicit user confirmation.
    /// # Errors
    /// Rejects changed input, cancellation, or failed identity capture.
    pub async fn capture_identity(
        &self,
        preview: ConnectionEditPreview,
        cancellation: &CancellationToken,
    ) -> Result<HostKeyConfirmation, ConnectionManagementError> {
        self.validate_edit_preview(&preview)?;
        check_cancelled(cancellation)?;
        let fingerprint = self.setup.capture_endpoint_identity(&EndpointProbeRequest {
                    driver: DriverKind::linux_ssh(),
            destination: DriverDestinationInput { value: serde_json::json!({"host": preview.draft.host, "port": preview.draft.port}) },
        }, SETUP_TIMEOUT, cancellation).await.map_err(|_| ConnectionManagementError::Setup("host-key capture"))?;
        check_cancelled(cancellation)?;
        if fingerprint.len() > 512 {
            return Err(ConnectionManagementError::Setup("host-key validation"));
        }
        let fingerprint = HostKeyFingerprint::parse(fingerprint)
            .map_err(|_| ConnectionManagementError::Setup("host-key validation"))?;
        Ok(HostKeyConfirmation {
            preview,
            fingerprint,
        })
    }

    /// Call only after the operator accepts the displayed SSH fingerprint.
    /// Authenticates using that exact pin, then appends a revision under the same key.
    /// # Errors
    /// Rejects a busy session, changed local state, cancellation, failed pinned authentication,
    /// or persistence failure. Earlier revisions are never rewritten.
    pub async fn confirm_and_save(
        &self,
        confirmation: HostKeyConfirmation,
        cancellation: &CancellationToken,
    ) -> Result<ConnectionDetails, ConnectionManagementError> {
        self.session
            .run(async {
                let HostKeyConfirmation {
                    preview,
                    fingerprint,
                } = confirmation;
                self.validate_edit_preview(&preview)?;
                check_cancelled(cancellation)?;
                let settings =
                    draft_settings(&preview.draft, &preview.credential_handle, fingerprint);
                let mut registry = destination_registry(&preview.registry)?;
                let revised = if preview.details.is_some() {
                    registry.revise(&preview.key, settings)
                } else {
                    registry.create(preview.key.clone(), settings)
                }
                .map_err(|_| {
                    unavailable(
                        ManagementSource::Destinations,
                        "could not create connection revision",
                    )
                })?
                .clone();
                self.setup
                    .authenticate_and_probe(
                        &DestinationSetupRequest {
                            driver: revised.settings.driver_kind(),
                            destination: revised.resolve().settings,
                            credential: SetupCredential::new(preview.credential.clone()),
                            remote_root: "/".into(),
                        },
                        SETUP_TIMEOUT,
                        SETUP_TIMEOUT,
                        cancellation,
                    )
                    .await
                    .map_err(|_| ConnectionManagementError::Setup("pinned authentication"))?;
                check_cancelled(cancellation)?;
                self.validate_edit_preview(&preview)?;
                if let Some(credentials) = &preview.updated_credentials {
                    credentials.save(&self.paths.credentials).map_err(|_| {
                        unavailable(
                            ManagementSource::Credentials,
                            "could not save credential reference; connection not saved",
                        )
                    })?;
                    preview.registry.ensure_unchanged().map_err(|_| {
                        ConnectionManagementError::CredentialSavedConnectionUnconfirmed {
                            credential: preview.credential_handle.clone(),
                            reason: "connection registry changed; review again",
                        }
                    })?;
                }
                registry.save(&self.paths.destinations).map_err(|_| {
                    if preview.updated_credentials.is_some() {
                        ConnectionManagementError::CredentialSavedConnectionUnconfirmed {
                            credential: preview.credential_handle.clone(),
                            reason: "connection registry write failed",
                        }
                    } else {
                        unavailable(ManagementSource::Destinations, "could not save registry")
                    }
                })?;
                Ok(ConnectionDetails {
                    key: preview.key,
                    current: revised,
                })
            })
            .await
            .map_err(|_| ConnectionManagementError::Busy)?
    }

    /// Performs read-only SSH authentication with the saved host-key pin and credential.
    /// Never accepts a replacement host key or changes either registry.
    /// # Errors
    /// Rejects invalid or changed local data, cancellation, and failed pinned authentication.
    pub async fn verify_saved_connection(
        &self,
        key: &DestinationKey,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, ConnectionManagementError> {
        let (snapshot, registry) = self.destinations()?;
        let connection = details(&registry, key)?.current.resolve();
        let credentials =
            FileSnapshot::read(&self.paths.credentials, ManagementSource::Credentials)?;
        let credential = resolve_credential(&credentials, &connection.credential)?;
        check_cancelled(cancellation)?;
        let result = self
            .setup
            .authenticate_and_probe(
                &DestinationSetupRequest {
                    driver: connection.driver,
                    destination: connection.settings,
                    credential: SetupCredential::new(credential),
                    remote_root: "/".into(),
                },
                SETUP_TIMEOUT,
                SETUP_TIMEOUT,
                cancellation,
            )
            .await
            .map_err(|_| ConnectionManagementError::Setup("pinned authentication"))?;
        check_cancelled(cancellation)?;
        snapshot.ensure_unchanged()?;
        credentials.ensure_unchanged()?;
        Ok(result)
    }

    fn destinations(
        &self,
    ) -> Result<(FileSnapshot, DestinationRegistry), ConnectionManagementError> {
        let snapshot =
            FileSnapshot::read(&self.paths.destinations, ManagementSource::Destinations)?;
        let registry = destination_registry(&snapshot)?;
        Ok((snapshot, registry))
    }

    fn ensure_path(
        snapshot: &FileSnapshot,
        expected: &Path,
    ) -> Result<(), ConnectionManagementError> {
        if snapshot.path() == expected {
            Ok(())
        } else {
            Err(ConnectionManagementError::Stale)
        }
    }

    fn validate_edit_preview(
        &self,
        preview: &ConnectionEditPreview,
    ) -> Result<(), ConnectionManagementError> {
        Self::ensure_path(&preview.registry, &self.paths.destinations)?;
        Self::ensure_path(&preview.credentials, &self.paths.credentials)?;
        preview.registry.ensure_unchanged()?;
        preview.credentials.ensure_unchanged()
    }

    fn scan_projects(
        &self,
        key: &DestinationKey,
        preview: &mut DestinationRemovalPreview,
    ) -> Result<(), ConnectionManagementError> {
        let snapshot = FileSnapshot::read(&self.paths.projects, ManagementSource::Projects)?;
        let registry = project_registry(&snapshot)?;
        let projects = registry.statuses();
        preview.projects = Some(snapshot);
        if projects.len() > MAX_PROJECTS {
            return Err(unavailable(
                ManagementSource::Projects,
                "too many registered Projects to verify safely",
            ));
        }
        let mut bytes = 0_usize;
        for status in projects {
            let path = status.project.root.join(config::PROJECT_FILE);
            let source = ManagementSource::ProjectConfig(path.clone());
            let result = (|| {
                let snapshot = FileSnapshot::read(&path, source.clone())?;
                let contents = snapshot.required_text()?;
                bytes = bytes.saturating_add(contents.len());
                if bytes > MAX_PROJECT_BYTES {
                    return Err(unavailable(
                        source.clone(),
                        "Project scan exceeds bounded byte limits",
                    ));
                }
                let config =
                    config::parse_contents(&status.project.root, contents).map_err(|_| {
                        unavailable(
                            source.clone(),
                            "configuration is invalid; references are unknown",
                        )
                    })?;
                if config.environments.values().any(|environment| {
                    environment
                        .components
                        .values()
                        .any(|target| &target.destination == key)
                }) {
                    preview.project_references.push(status.project.root);
                }
                preview.configs.push(snapshot);
                Ok(())
            })();
            if let Err(error) = result {
                add_blocker(preview, error);
            }
        }
        Ok(())
    }

    fn read_history(
        &self,
        key: &DestinationKey,
    ) -> Result<Option<DestinationReferenceSummary>, ConnectionManagementError> {
        match std::fs::symlink_metadata(&self.paths.history) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for suffix in ["-wal", "-shm", "-journal"] {
                    let mut name = self.paths.history.as_os_str().to_os_string();
                    name.push(suffix);
                    match std::fs::symlink_metadata(PathBuf::from(name)) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        _ => {
                            return Err(unavailable(
                                ManagementSource::History,
                                "database is missing but sidecar evidence remains; references are unknown",
                            ));
                        }
                    }
                }
                return Ok(None);
            }
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            _ => {
                return Err(unavailable(
                    ManagementSource::History,
                    "database is unreadable or not a regular file; references are unknown",
                ));
            }
        }
        let store = HistoryStore::open_existing_read_only(&self.paths.history).map_err(|_| {
            unavailable(
                ManagementSource::History,
                "database cannot be safely opened; references are unknown",
            )
        })?;
        store.destination_references(key).map(Some).map_err(|_| {
            unavailable(
                ManagementSource::History,
                "historical references are invalid, incomplete, or exceed scan limits",
            )
        })
    }
}

fn details(
    registry: &DestinationRegistry,
    key: &DestinationKey,
) -> Result<ConnectionDetails, ConnectionManagementError> {
    Ok(ConnectionDetails {
        key: key.clone(),
        current: registry
            .resolve(key)
            .ok_or(ConnectionManagementError::Missing)?
            .clone(),
    })
}

fn project_registry(snapshot: &FileSnapshot) -> Result<ProjectRegistry, ConnectionManagementError> {
    snapshot.text()?.map_or_else(
        || Ok(ProjectRegistry::new()),
        |text| {
            ProjectRegistry::from_yaml(snapshot.path(), text).map_err(|_| {
                unavailable(
                    ManagementSource::Projects,
                    "registry is invalid; references are unknown",
                )
            })
        },
    )
}

fn destination_registry(
    snapshot: &FileSnapshot,
) -> Result<DestinationRegistry, ConnectionManagementError> {
    snapshot.text()?.map_or_else(
        || Ok(DestinationRegistry::new()),
        |text| {
            DestinationRegistry::from_yaml(snapshot.path(), text)
                .map_err(|_| unavailable(ManagementSource::Destinations, "registry is invalid"))
        },
    )
}

fn resolve_credential(
    snapshot: &FileSnapshot,
    handle: &CredentialHandle,
) -> Result<config::SshCredential, ConnectionManagementError> {
    let registry = credential_registry(snapshot)?;
    registry.resolve(handle).cloned().ok_or_else(|| {
        unavailable(
            ManagementSource::Credentials,
            "selected credential is not registered",
        )
    })
}

fn credential_registry(
    snapshot: &FileSnapshot,
) -> Result<CredentialRegistry, ConnectionManagementError> {
    snapshot.text()?.map_or_else(
        || Ok(CredentialRegistry::new()),
        |text| {
            CredentialRegistry::from_yaml(snapshot.path(), text)
                .map_err(|_| unavailable(ManagementSource::Credentials, "registry is invalid"))
        },
    )
}

fn draft_settings(
    draft: &SshConnectionDraft,
    credential: &CredentialHandle,
    host_key: HostKeyFingerprint,
) -> DestinationSettings {
    DestinationSettings::LinuxSsh {
        host: draft.host.clone(),
        port: draft.port,
        user: draft.user.clone(),
        credential: credential.clone(),
        host_key,
    }
}

fn validate_draft(
    draft: &SshConnectionDraft,
    credential: &CredentialHandle,
) -> Result<(), ConnectionManagementError> {
    if draft.host.len() > 255 || draft.user.len() > 255 {
        return Err(unavailable(
            ManagementSource::Destinations,
            "connection fields exceed bounded lengths",
        ));
    }
    draft_settings(
        draft,
        credential,
        HostKeyFingerprint::parse("pending-confirmation").expect("fixed nonempty text"),
    )
    .validate()
    .map_err(|_| unavailable(ManagementSource::Destinations, "invalid connection fields"))
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ConnectionManagementError> {
    if cancellation.is_cancelled() {
        Err(ConnectionManagementError::Cancelled)
    } else {
        Ok(())
    }
}

fn unavailable(source: ManagementSource, reason: &'static str) -> ConnectionManagementError {
    ConnectionManagementError::Unavailable {
        origin: source,
        reason,
    }
}

fn add_blocker(preview: &mut DestinationRemovalPreview, error: ConnectionManagementError) {
    if let ConnectionManagementError::Unavailable {
        origin: source,
        reason,
    } = error
    {
        preview.blockers.push(ManagementBlocker { source, reason });
    } else {
        preview.blockers.push(ManagementBlocker {
            source: ManagementSource::Projects,
            reason: "references could not be verified",
        });
    }
}

#[cfg(test)]
mod tests;
