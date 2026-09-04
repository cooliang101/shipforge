use std::{collections::BTreeMap, path::PathBuf};

use serde::Deserialize;

use crate::domain::{ComponentGeneration, ComponentName, DestinationKey, EnvironmentId, ProjectId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectConfig {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub project: String,
    pub components: BTreeMap<ComponentName, ComponentConfig>,
    pub environments: BTreeMap<String, EnvironmentConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentConfig {
    pub working_directory: PathBuf,
    pub build: Vec<BuildCommand>,
    pub artifact: ArtifactSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentConfig {
    pub id: EnvironmentId,
    pub components: BTreeMap<ComponentName, TargetConfig>,
}

impl EnvironmentConfig {
    /// Calculates deterministic activation order for a selected Component set.
    ///
    /// Dependencies not present in `selected` affect no ordering and are not
    /// automatically selected.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown selected Component or a dependency cycle.
    pub fn activation_order<'a>(
        &'a self,
        environment_name: &str,
        selected: impl IntoIterator<Item = &'a ComponentName>,
    ) -> Result<Vec<ComponentName>, super::ConfigError> {
        super::topology::activation_order(environment_name, &self.components, selected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetConfig {
    pub destination: DestinationKey,
    pub generation: ComponentGeneration,
    pub root: String,
    pub systemd: Option<String>,
    pub health: Option<String>,
    pub after: Vec<ComponentName>,
}

impl TargetConfig {
    #[must_use]
    pub fn driver_input(&self) -> crate::drivers::DriverTargetInput {
        crate::drivers::DriverTargetInput {
            value: serde_json::json!({
                "root": self.root,
                "systemd": self.systemd,
                "health": self.health,
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildCommand {
    pub program: String,
    pub args: Vec<String>,
    pub shell: bool,
}

impl BuildCommand {
    #[must_use]
    pub fn argv(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            shell: false,
        }
    }

    #[must_use]
    pub fn shell(command: impl Into<String>) -> Self {
        Self {
            program: command.into(),
            args: Vec::new(),
            shell: true,
        }
    }

    pub(super) fn from_argv(mut argv: Vec<String>) -> Self {
        let program = if argv.is_empty() {
            String::new()
        } else {
            argv.remove(0)
        };
        Self {
            program,
            args: argv,
            shell: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactSpec {
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedArtifactKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedArtifact {
    pub path: PathBuf,
    pub kind: ResolvedArtifactKind,
}

impl ArtifactSpec {
    /// Resolves and validates an Artifact after its build has completed.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the Project, is missing or empty,
    /// or is not a regular file/directory.
    pub fn resolve(
        &self,
        project_root: &std::path::Path,
        working_directory: &std::path::Path,
    ) -> Result<ResolvedArtifact, super::ConfigError> {
        let canonical_root =
            project_root
                .canonicalize()
                .map_err(|source| super::ConfigError::ArtifactIo {
                    path: project_root.to_owned(),
                    source,
                })?;
        let candidate = project_root.join(working_directory).join(&self.path);
        let source_metadata =
            candidate
                .symlink_metadata()
                .map_err(|source| super::ConfigError::ArtifactIo {
                    path: candidate.clone(),
                    source,
                })?;
        if source_metadata.file_type().is_symlink() {
            return Err(super::ConfigError::UnsupportedArtifact { path: candidate });
        }
        let canonical =
            candidate
                .canonicalize()
                .map_err(|source| super::ConfigError::ArtifactIo {
                    path: candidate.clone(),
                    source,
                })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(super::ConfigError::ArtifactOutsideProject { path: canonical });
        }
        let metadata =
            canonical
                .symlink_metadata()
                .map_err(|source| super::ConfigError::ArtifactIo {
                    path: canonical.clone(),
                    source,
                })?;
        let resolved_kind = if metadata.is_file() {
            if metadata.len() == 0 {
                return Err(super::ConfigError::EmptyArtifact { path: canonical });
            }
            ResolvedArtifactKind::File
        } else if metadata.is_dir() {
            let mut entries =
                std::fs::read_dir(&canonical).map_err(|source| super::ConfigError::ArtifactIo {
                    path: canonical.clone(),
                    source,
                })?;
            if entries.next().is_none() {
                return Err(super::ConfigError::EmptyArtifact { path: canonical });
            }
            ResolvedArtifactKind::Directory
        } else {
            return Err(super::ConfigError::UnsupportedArtifact { path: canonical });
        };
        Ok(ResolvedArtifact {
            path: canonical,
            kind: resolved_kind,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RawProjectConfig {
    pub schema_version: u32,
    #[serde(rename = "_shipforge")]
    pub managed: Option<ManagedProject>,
    pub project: String,
    pub components: BTreeMap<ComponentName, RawComponent>,
    pub environments: BTreeMap<String, RawEnvironment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ManagedProject {
    pub project_id: ProjectId,
    pub environments: BTreeMap<String, ManagedEnvironment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ManagedEnvironment {
    pub id: EnvironmentId,
    pub components: BTreeMap<ComponentName, ManagedComponent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ManagedComponent {
    pub generation: ComponentGeneration,
    pub resolved_root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RawComponent {
    pub build: Vec<Vec<String>>,
    pub artifact: PathBuf,
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawEnvironment {
    pub components: BTreeMap<ComponentName, RawTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTarget {
    #[serde(rename = "to")]
    pub destination: DestinationKey,
    pub root: Option<String>,
    pub systemd: Option<String>,
    pub health: Option<String>,
    #[serde(default)]
    pub after: Vec<ComponentName>,
}
