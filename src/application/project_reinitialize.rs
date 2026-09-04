//! Confirmed replacement of damaged Project identity, without remote effects.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use atomic_write_file::AtomicWriteFile;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::{
    self, DestinationRegistry, DestinationSummary, PreparedProjectInitialization, ProjectConfig,
    ReinitializeConfirmation,
};

use super::{
    DeploymentSession,
    project_edit::{
        FileSnapshot, ProjectEditError, bound_setup, config_error, directory_identity,
        selected_destinations,
    },
};

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_DESTINATIONS: usize = 4096;

#[derive(Clone, Debug)]
pub struct ProjectReinitializeService {
    destinations_path: PathBuf,
    session: Arc<DeploymentSession>,
}

/// The exact bytes and fresh identities shown before confirmation. File evidence
/// is opaque so callers cannot substitute a different Project or source file.
#[derive(Clone)]
pub struct ProjectReinitializePreview {
    root: PathBuf,
    root_identity: (u64, u64),
    source: FileSnapshot,
    registry: FileSnapshot,
    destinations: Vec<DestinationSummary>,
    prepared: PreparedProjectInitialization,
}

impl std::fmt::Debug for ProjectReinitializePreview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectReinitializePreview")
            .finish_non_exhaustive()
    }
}

impl ProjectReinitializePreview {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn config(&self) -> &ProjectConfig {
        self.prepared.config()
    }

    #[must_use]
    pub fn yaml(&self) -> &str {
        self.prepared.preview()
    }

    #[must_use]
    pub fn destinations(&self) -> &[DestinationSummary] {
        &self.destinations
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProjectReinitializeError {
    #[error("Project reinitialization is unavailable while another operation is active")]
    Busy,
    #[error("Project reinitialization cancelled; configuration was not replaced")]
    Cancelled,
    #[error(
        "Project configuration or its connection registry is missing; missing YAML requires new-Project setup"
    )]
    Missing,
    #[error(
        "Project configuration is valid; use the existing Project instead of reinitializing it"
    )]
    NotRequired,
    #[error("Project configuration or connections changed; reload and review again")]
    Stale,
    #[error("configuration must use regular unlinked files inside an unchanged Project directory")]
    UnsafePath,
    #[error("configuration exceeds bounded reinitialization limits")]
    Limit,
    #[error("cannot safely reinitialize Project configuration: {0}")]
    Invalid(&'static str),
    #[error("connection registry is invalid or a configured connection no longer exists")]
    Destination,
    #[error("Project configuration could not be safely read")]
    Read,
    #[error("Project reinitialization save is unconfirmed; reload before retrying")]
    SaveUnconfirmed,
}

impl From<ProjectEditError> for ProjectReinitializeError {
    fn from(error: ProjectEditError) -> Self {
        match error {
            ProjectEditError::Busy => Self::Busy,
            ProjectEditError::Cancelled => Self::Cancelled,
            ProjectEditError::Missing => Self::Missing,
            ProjectEditError::Stale => Self::Stale,
            ProjectEditError::UnsafePath => Self::UnsafePath,
            ProjectEditError::Limit => Self::Limit,
            ProjectEditError::Invalid(reason) => Self::Invalid(reason),
            ProjectEditError::Destination => Self::Destination,
            ProjectEditError::Read | ProjectEditError::Discovery => Self::Read,
            ProjectEditError::SaveUnconfirmed => Self::SaveUnconfirmed,
        }
    }
}

impl ProjectReinitializeService {
    #[must_use]
    pub const fn new(destinations_path: PathBuf, session: Arc<DeploymentSession>) -> Self {
        Self {
            destinations_path,
            session,
        }
    }

    /// Prepares fresh identities only for an existing damaged managed section.
    /// Reads bounded local snapshots, never history, credentials or remote state.
    ///
    /// # Errors
    /// Rejects valid or missing Projects, unsafe paths, invalid human intent,
    /// ambiguous roots, stale files, missing connections and cancellation.
    pub async fn preview(
        &self,
        root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<ProjectReinitializePreview, ProjectReinitializeError> {
        self.session
            .run(async {
                cancelled(cancellation)?;
                let root_identity = directory_identity(root)?;
                let root = root
                    .canonicalize()
                    .map_err(|_| ProjectReinitializeError::Read)?;
                let source = FileSnapshot::read(&root.join(config::PROJECT_FILE))?;
                let setup = config::reinitialize_setup(&root, source.text()?)
                    .map_err(reinitialize_config_error)?
                    .ok_or(ProjectReinitializeError::NotRequired)?;
                bound_setup(&setup, &[])?;
                let prepared =
                    config::prepare_initialize(setup).map_err(reinitialize_config_error)?;
                ensure_displayable_yaml(prepared.preview())?;
                if prepared.preview().len() > MAX_FILE_BYTES {
                    return Err(ProjectReinitializeError::Limit);
                }
                let registry = FileSnapshot::read(&self.destinations_path)?;
                let parsed =
                    DestinationRegistry::from_yaml(&self.destinations_path, registry.text()?)
                        .map_err(|_| ProjectReinitializeError::Destination)?;
                let destinations = parsed.summaries();
                if destinations.len() > MAX_DESTINATIONS {
                    return Err(ProjectReinitializeError::Limit);
                }
                selected_destinations(prepared.config(), &parsed)?;
                let preview = ProjectReinitializePreview {
                    root,
                    root_identity,
                    source,
                    registry,
                    destinations,
                    prepared,
                };
                self.ensure_unchanged(&preview)?;
                cancelled(cancellation)?;
                Ok(preview)
            })
            .await
            .map_err(|_| ProjectReinitializeError::Busy)?
    }

    /// Replaces only the exact reviewed YAML after explicit TUI confirmation.
    /// It is a same-session guard, not an external-editor lock or backup protocol.
    ///
    /// # Errors
    /// Rejects active operations, cancellation or changed local evidence. A failed
    /// filesystem commit is unconfirmed; old bytes are never written back.
    pub async fn save(
        &self,
        preview: ProjectReinitializePreview,
        _confirmation: ReinitializeConfirmation,
        cancellation: &CancellationToken,
    ) -> Result<ProjectConfig, ProjectReinitializeError> {
        self.session
            .run(async {
                cancelled(cancellation)?;
                self.ensure_unchanged(&preview)?;
                let mut file = AtomicWriteFile::open(&preview.source.path)
                    .map_err(|_| ProjectReinitializeError::SaveUnconfirmed)?;
                file.write_all(preview.yaml().as_bytes())
                    .and_then(|()| {
                        file.as_file()
                            .set_permissions(preview.source.permissions.clone())
                    })
                    .map_err(|_| ProjectReinitializeError::SaveUnconfirmed)?;
                self.ensure_unchanged(&preview)?;
                cancelled(cancellation)?;
                file.commit()
                    .map_err(|_| ProjectReinitializeError::SaveUnconfirmed)?;
                Ok(preview.config().clone())
            })
            .await
            .map_err(|_| ProjectReinitializeError::Busy)?
    }

    fn ensure_unchanged(
        &self,
        preview: &ProjectReinitializePreview,
    ) -> Result<(), ProjectReinitializeError> {
        if self.destinations_path != preview.registry.path
            || directory_identity(&preview.root)? != preview.root_identity
        {
            return Err(ProjectReinitializeError::Stale);
        }
        preview.source.ensure_unchanged()?;
        preview.registry.ensure_unchanged()?;
        Ok(())
    }
}

fn ensure_displayable_yaml(yaml: &str) -> Result<(), ProjectReinitializeError> {
    if yaml.chars().any(|value| {
        (value.is_control() && value != '\n')
            || matches!(value, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
    }) {
        Err(ProjectReinitializeError::Invalid(
            "generated YAML contains invisible or terminal-control characters and cannot be faithfully previewed",
        ))
    } else {
        Ok(())
    }
}

fn reinitialize_config_error(error: config::ConfigError) -> ProjectReinitializeError {
    match error {
        config::ConfigError::ManagedState(_) => ProjectReinitializeError::Invalid(
            "human intent or frozen roots are inconsistent; review the intended roots and select a valid project file before retrying",
        ),
        config::ConfigError::SchemaVersion(_) => {
            ProjectReinitializeError::Invalid("unsupported schema version")
        }
        config::ConfigError::Yaml { .. }
        | config::ConfigError::Security(crate::telemetry::SecurityError::InvalidYaml(_)) => {
            ProjectReinitializeError::Invalid(
                "YAML structure or human configuration fields are invalid",
            )
        }
        other => config_error(&other).into(),
    }
}

fn cancelled(cancellation: &CancellationToken) -> Result<(), ProjectReinitializeError> {
    if cancellation.is_cancelled() {
        Err(ProjectReinitializeError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
