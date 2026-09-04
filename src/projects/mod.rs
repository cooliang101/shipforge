//! Local Project selection and recent-project registration.

mod discovery;

pub use discovery::{
    ComponentCandidate, DiscoveryConfidence, DiscoveryError, DiscoveryReport, discover_components,
    suggest_project_name,
};

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{self, PROJECT_FILE, ProjectConfig, ProjectConfigState};

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const REGISTRY_FILE: &str = "projects.yaml";

/// Returns the platform-local recent-project registry path.
///
/// # Errors
///
/// Returns an error when the required user configuration directory cannot be
/// resolved to an absolute path.
pub fn default_registry_path() -> Result<PathBuf, ProjectRegistryError> {
    crate::adapters::user_config_directory()
        .map(|directory| directory.join(REGISTRY_FILE))
        .map_err(|_| ProjectRegistryError::ConfigurationDirectoryUnavailable)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredProject {
    pub root: PathBuf,
    pub last_opened_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectStatus {
    pub project: RegisteredProject,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRegistry {
    schema_version: u32,
    projects: Vec<RegisteredProject>,
}

impl ProjectRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            projects: Vec::new(),
        }
    }

    /// Loads the local recent-project registry. A missing file is an empty registry.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable, malformed, unsupported, relative, or
    /// duplicate registry entries.
    pub fn load(path: &Path) -> Result<Self, ProjectRegistryError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(source) => return Err(ProjectRegistryError::io(path, source)),
        };
        Self::from_yaml(path, &contents)
    }

    pub(crate) fn from_yaml(path: &Path, contents: &str) -> Result<Self, ProjectRegistryError> {
        let registry: Self =
            serde_yaml_ng::from_str(contents).map_err(|source| ProjectRegistryError::Yaml {
                path: path.to_owned(),
                source,
            })?;
        registry.validate()?;
        Ok(registry)
    }

    /// Saves the registry through an atomic replacement.
    ///
    /// # Errors
    ///
    /// Returns an error if validation, directory creation, serialization, or
    /// atomic replacement fails.
    pub fn save(&self, path: &Path) -> Result<(), ProjectRegistryError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| ProjectRegistryError::io(parent, source))?;
        }
        let contents = serde_yaml_ng::to_string(self).map_err(ProjectRegistryError::Serialize)?;
        let mut file =
            AtomicWriteFile::open(path).map_err(|source| ProjectRegistryError::io(path, source))?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.commit())
            .map_err(|source| ProjectRegistryError::io(path, source))
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<ProjectStatus> {
        self.projects
            .iter()
            .cloned()
            .map(|project| ProjectStatus {
                available: project.root.is_dir() && project.root.join(PROJECT_FILE).is_file(),
                project,
            })
            .collect()
    }

    /// Removes only this exact recent-project registration, including unavailable roots.
    /// Does not inspect or modify the Project directory, YAML, or deployment history.
    pub fn unregister(&mut self, root: &Path) -> bool {
        let count = self.projects.len();
        self.projects.retain(|project| project.root != root);
        self.projects.len() != count
    }

    fn touch(&mut self, root: PathBuf, opened_at_unix_ms: u64) {
        self.projects.retain(|project| project.root != root);
        self.projects.push(RegisteredProject {
            root,
            last_opened_unix_ms: opened_at_unix_ms,
        });
        self.projects.sort_by(|left, right| {
            right
                .last_opened_unix_ms
                .cmp(&left.last_opened_unix_ms)
                .then_with(|| left.root.cmp(&right.root))
        });
    }

    fn validate(&self) -> Result<(), ProjectRegistryError> {
        if self.schema_version != REGISTRY_SCHEMA_VERSION {
            return Err(ProjectRegistryError::UnsupportedSchema(self.schema_version));
        }
        let mut roots = std::collections::BTreeSet::new();
        for project in &self.projects {
            if !project.root.is_absolute() {
                return Err(ProjectRegistryError::RelativeRoot(project.root.clone()));
            }
            if !roots.insert(&project.root) {
                return Err(ProjectRegistryError::DuplicateRoot(project.root.clone()));
            }
        }
        Ok(())
    }
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub enum ProjectSelection {
    Existing {
        root: PathBuf,
        config: ProjectConfig,
    },
    New {
        root: PathBuf,
    },
}

/// Selects a Project directory. Existing valid Projects are added to recents;
/// directories without `shipforge.yaml` remain unregistered until setup is confirmed.
///
/// # Errors
///
/// Returns an error when the root cannot be canonicalized, configuration is
/// invalid, or the local registry cannot be updated.
pub fn select_project(
    registry_path: &Path,
    project_root: &Path,
    opened_at_unix_ms: u64,
) -> Result<ProjectSelection, ProjectRegistryError> {
    let root = canonical_project_root(project_root)?;
    match config::load(&root)? {
        ProjectConfigState::Missing => Ok(ProjectSelection::New { root }),
        ProjectConfigState::Loaded(config) => {
            let mut registry = ProjectRegistry::load(registry_path)?;
            registry.touch(root.clone(), opened_at_unix_ms);
            registry.save(registry_path)?;
            Ok(ProjectSelection::Existing { root, config })
        }
    }
}

/// Registers a Project only after its configuration has been successfully
/// created and reloaded.
///
/// # Errors
///
/// Returns an error for an invalid root, missing/invalid configuration, or a
/// registry persistence failure.
pub fn register_initialized_project(
    registry_path: &Path,
    project_root: &Path,
    opened_at_unix_ms: u64,
) -> Result<ProjectConfig, ProjectRegistryError> {
    let root = canonical_project_root(project_root)?;
    let config = match config::load(&root)? {
        ProjectConfigState::Missing => {
            return Err(ProjectRegistryError::MissingConfiguration(root));
        }
        ProjectConfigState::Loaded(config) => config,
    };
    let mut registry = ProjectRegistry::load(registry_path)?;
    registry.touch(root, opened_at_unix_ms);
    registry.save(registry_path)?;
    Ok(config)
}

fn canonical_project_root(root: &Path) -> Result<PathBuf, ProjectRegistryError> {
    if !root.is_dir() {
        return Err(ProjectRegistryError::NotDirectory(root.to_owned()));
    }
    std::fs::canonicalize(root).map_err(|source| ProjectRegistryError::io(root, source))
}

#[derive(Debug, Error)]
pub enum ProjectRegistryError {
    #[error("project registry I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("project registry YAML is invalid at `{path}`: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error("project registry serialization failed: {0}")]
    Serialize(serde_yaml_ng::Error),
    #[error("unsupported project registry schemaVersion {0}")]
    UnsupportedSchema(u32),
    #[error("project registry root must be absolute: `{0}`")]
    RelativeRoot(PathBuf),
    #[error("project registry contains duplicate root: `{0}`")]
    DuplicateRoot(PathBuf),
    #[error("selected Project root is not a directory: `{0}`")]
    NotDirectory(PathBuf),
    #[error("Project has no shipforge.yaml after setup: `{0}`")]
    MissingConfiguration(PathBuf),
    #[error("the platform user configuration directory is unavailable or not absolute")]
    ConfigurationDirectoryUnavailable,
    #[error(transparent)]
    Config(#[from] config::ConfigError),
}

impl ProjectRegistryError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }

    pub(crate) fn browser(path: &Path, source: std::io::Error) -> Self {
        Self::io(path, source)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use crate::{
        config::{
            ArtifactSpec, BuildCommand, ComponentSetup, EnvironmentSetup, ProjectSetup,
            TargetSetup, initialize,
        },
        domain::{ComponentName, DestinationKey},
    };

    fn setup() -> ProjectSetup {
        let component = ComponentName::parse("app").unwrap();
        ProjectSetup {
            project: "demo".into(),
            components: BTreeMap::from([(
                component.clone(),
                ComponentSetup {
                    working_directory: None,
                    build: vec![BuildCommand {
                        program: "cargo".into(),
                        args: vec!["build".into()],
                        shell: false,
                    }],
                    artifact: ArtifactSpec {
                        path: PathBuf::from("target/release/demo"),
                    },
                },
            )]),
            environments: BTreeMap::from([(
                "production".into(),
                EnvironmentSetup {
                    components: BTreeMap::from([(
                        component,
                        TargetSetup {
                            destination: DestinationKey::parse(
                                "dst_00000000000000000000000000000001",
                            )
                            .unwrap(),
                            root: None,
                            systemd: Some("demo.service".into()),
                            health: None,
                            after: Vec::new(),
                        },
                    )]),
                },
            )]),
        }
    }

    #[test]
    fn missing_config_enters_setup_without_registering_project() {
        let directory = tempdir().unwrap();
        let registry_path = directory.path().join("state/projects.yaml");
        let project_root = directory.path().join("project");
        std::fs::create_dir(&project_root).unwrap();

        let selected = select_project(&registry_path, &project_root, 10).unwrap();
        assert!(matches!(selected, ProjectSelection::New { .. }));
        assert!(!registry_path.exists());
    }

    #[test]
    fn initialized_projects_are_registered_in_recent_order() {
        let directory = tempdir().unwrap();
        let registry_path = directory.path().join("state/projects.yaml");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        initialize(&first, setup()).unwrap();
        initialize(&second, setup()).unwrap();

        register_initialized_project(&registry_path, &first, 10).unwrap();
        register_initialized_project(&registry_path, &second, 20).unwrap();
        select_project(&registry_path, &first, 30).unwrap();

        let statuses = ProjectRegistry::load(&registry_path).unwrap().statuses();
        assert_eq!(statuses.len(), 2);
        assert_eq!(
            statuses[0].project.root,
            std::fs::canonicalize(first).unwrap()
        );
        assert!(statuses.iter().all(|status| status.available));
    }

    #[test]
    fn unavailable_recent_project_is_retained_and_marked() {
        let directory = tempdir().unwrap();
        let registry_path = directory.path().join("projects.yaml");
        let registry = ProjectRegistry {
            schema_version: REGISTRY_SCHEMA_VERSION,
            projects: vec![RegisteredProject {
                root: directory.path().join("missing"),
                last_opened_unix_ms: 10,
            }],
        };
        registry.save(&registry_path).unwrap();

        let statuses = ProjectRegistry::load(&registry_path).unwrap().statuses();
        assert!(!statuses[0].available);
    }

    #[test]
    fn registration_requires_a_valid_project_config() {
        let directory = tempdir().unwrap();
        let registry_path = directory.path().join("projects.yaml");
        assert!(matches!(
            register_initialized_project(&registry_path, directory.path(), 10),
            Err(ProjectRegistryError::MissingConfiguration(_))
        ));
        assert!(!registry_path.exists());
    }

    #[test]
    fn registry_rejects_relative_and_duplicate_roots() {
        let relative = ProjectRegistry {
            schema_version: REGISTRY_SCHEMA_VERSION,
            projects: vec![RegisteredProject {
                root: PathBuf::from("relative/project"),
                last_opened_unix_ms: 10,
            }],
        };
        assert!(matches!(
            relative.validate(),
            Err(ProjectRegistryError::RelativeRoot(_))
        ));

        let directory = tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let duplicate = ProjectRegistry {
            schema_version: REGISTRY_SCHEMA_VERSION,
            projects: vec![
                RegisteredProject {
                    root: root.clone(),
                    last_opened_unix_ms: 20,
                },
                RegisteredProject {
                    root,
                    last_opened_unix_ms: 10,
                },
            ],
        };
        assert!(matches!(
            duplicate.validate(),
            Err(ProjectRegistryError::DuplicateRoot(_))
        ));
    }
}
