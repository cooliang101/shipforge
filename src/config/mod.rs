//! Project configuration loading, normalization, and validation.

mod credentials;
mod destinations;
mod model;
mod setup;
mod topology;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component as PathComponent, Path, PathBuf},
};

pub use credentials::{
    CredentialReferences, CredentialRegistry, CredentialRegistryError, CredentialSummary,
    SshCredential, default_credential_registry_path,
};
pub use destinations::{
    DestinationEntry, DestinationReferences, DestinationRegistry, DestinationRegistryError,
    DestinationRevisionRecord, DestinationSettings, DestinationSummary, HostKeyFingerprint,
    LocalSshDiscovery, ResolvedDestination, SshCandidate, default_destination_registry_path,
    discover_local_ssh, discover_ssh_candidates, ssh_agent_available,
};
pub use model::{
    ArtifactSpec, BuildCommand, ComponentConfig, EnvironmentConfig, ProjectConfig,
    ResolvedArtifact, ResolvedArtifactKind, TargetConfig,
};
use model::{ManagedEnvironment, RawEnvironment, RawProjectConfig};
pub use setup::{
    ComponentSetup, EnvironmentRename, EnvironmentSetup, PreparedProjectInitialization,
    PreparedProjectUpdate, ProjectSetup, ReinitializeConfirmation, TargetSetup,
    default_remote_root, initialize, prepare_initialize, prepare_update, reinitialize, update,
};
use thiserror::Error;

use crate::domain::{ComponentName, DestinationKey};

pub const PROJECT_FILE: &str = "shipforge.yaml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid YAML in {path}: {source}")]
    Yaml {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("unsupported schemaVersion {0}; expected 1")]
    SchemaVersion(u32),
    #[error("invalid {field} name `{value}`; use lowercase letters, digits, or single hyphens")]
    InvalidName { field: &'static str, value: String },
    #[error("_shipforge is missing or inconsistent: {0}")]
    ManagedState(String),
    #[error("Component `{component}` has invalid {field} path `{path}`")]
    UnsafePath {
        component: ComponentName,
        field: &'static str,
        path: PathBuf,
    },
    #[error("Environment `{environment}` Component `{component}` has invalid root `{root}`")]
    UnsafeRoot {
        environment: String,
        component: ComponentName,
        root: String,
    },
    #[error(
        "Environment `{environment}` roots overlap on Destination `{destination}`: `{first}` and `{second}`"
    )]
    OverlappingRoots {
        environment: String,
        destination: DestinationKey,
        first: String,
        second: String,
    },
    #[error(
        "Environment `{environment}` Component `{component}` references unknown Component `{dependency}` in after"
    )]
    UnknownDependency {
        environment: String,
        component: ComponentName,
        dependency: ComponentName,
    },
    #[error("Environment `{environment}` Component `{component}` cannot list itself in after")]
    SelfDependency {
        environment: String,
        component: ComponentName,
    },
    #[error("Environment `{environment}` contains an after dependency cycle")]
    DependencyCycle { environment: String },
    #[error("Environment `{environment}` does not configure selected Component `{component}`")]
    UnknownSelectedComponent {
        environment: String,
        component: ComponentName,
    },
    #[error("Project must define at least one Component and one Environment")]
    EmptyProject,
    #[error("Component `{0}` must define at least one non-empty build command")]
    EmptyBuild(ComponentName),
    #[error("{0} already exists; use explicit reinitialization to replace it")]
    AlreadyExists(PathBuf),
    #[error("failed to serialize generated Project configuration: {0}")]
    Serialize(serde_yaml_ng::Error),
    #[error("failed to atomically write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Security(#[from] crate::telemetry::SecurityError),
    #[error("Artifact `{path}` cannot be inspected: {source}")]
    ArtifactIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Artifact `{path}` resolves outside the Project root")]
    ArtifactOutsideProject { path: PathBuf },
    #[error("Artifact `{path}` is empty")]
    EmptyArtifact { path: PathBuf },
    #[error("Artifact `{path}` is neither a regular file nor a directory")]
    UnsupportedArtifact { path: PathBuf },
    #[error("cannot update Project configuration because shipforge.yaml is missing")]
    MissingExistingConfig,
    #[error("invalid Environment rename: {0}")]
    InvalidEnvironmentRename(String),
    #[error("Component generation counter is exhausted for `{environment}/{component}`")]
    GenerationOverflow {
        environment: String,
        component: ComponentName,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProjectConfigState {
    Missing,
    Loaded(ProjectConfig),
}

/// Loads and validates the Project configuration at a selected root.
///
/// A missing file is reported as [`ProjectConfigState::Missing`]; this function
/// never creates a Project or changes the filesystem.
///
/// # Errors
///
/// Returns a field-specific error for unreadable, malformed, unsafe, or
/// internally inconsistent configuration.
pub fn load(project_root: &Path) -> Result<ProjectConfigState, ConfigError> {
    let path = project_root.join(PROJECT_FILE);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectConfigState::Missing);
        }
        Err(source) => return Err(ConfigError::Read { path, source }),
    };

    parse_contents(project_root, &contents).map(ProjectConfigState::Loaded)
}

/// Validates one already-read configuration snapshot without reopening the file.
///
/// # Errors
/// Returns the same validation errors as [`load`]. Never writes configuration.
pub(crate) fn parse_contents(
    project_root: &Path,
    contents: &str,
) -> Result<ProjectConfig, ConfigError> {
    let path = project_root.join(PROJECT_FILE);
    crate::telemetry::detect_sensitive_config(contents)?;
    let raw: RawProjectConfig =
        serde_yaml_ng::from_str(contents).map_err(|source| ConfigError::Yaml { path, source })?;
    normalize(raw)
}

fn normalize(raw: RawProjectConfig) -> Result<ProjectConfig, ConfigError> {
    if raw.schema_version != 1 {
        return Err(ConfigError::SchemaVersion(raw.schema_version));
    }
    validate_name("Project", &raw.project)?;
    if raw.components.is_empty() || raw.environments.is_empty() {
        return Err(ConfigError::EmptyProject);
    }
    let managed = raw.managed.ok_or_else(|| {
        ConfigError::ManagedState(
            "managed section is absent; explicit reinitialization required".into(),
        )
    })?;

    let component_names = raw.components.keys().cloned().collect::<BTreeSet<_>>();
    let components = raw
        .components
        .into_iter()
        .map(|(name, component)| normalize_component(name, component))
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    if managed.environments.len() != raw.environments.len() {
        return Err(ConfigError::ManagedState(
            "Environment entries do not match user configuration".into(),
        ));
    }

    let mut environments = BTreeMap::new();
    for (environment_name, environment) in &raw.environments {
        validate_name("Environment", environment_name)?;
        let managed_environment = managed.environments.get(environment_name).ok_or_else(|| {
            ConfigError::ManagedState(format!(
                "Environment `{environment_name}` has no generated identity"
            ))
        })?;
        if managed_environment.components.len() != environment.components.len() {
            return Err(ConfigError::ManagedState(format!(
                "Environment `{environment_name}` Component entries do not match"
            )));
        }

        environments.insert(
            environment_name.clone(),
            normalize_environment(
                environment_name,
                environment,
                managed_environment,
                &component_names,
            )?,
        );
    }

    Ok(ProjectConfig {
        schema_version: raw.schema_version,
        project_id: managed.project_id,
        project: raw.project,
        components,
        environments,
    })
}

fn normalize_environment(
    environment_name: &str,
    environment: &RawEnvironment,
    managed: &ManagedEnvironment,
    project_components: &BTreeSet<ComponentName>,
) -> Result<EnvironmentConfig, ConfigError> {
    let mut targets = BTreeMap::new();
    for (component_name, target) in &environment.components {
        if !project_components.contains(component_name) {
            return Err(ConfigError::ManagedState(format!(
                "Environment `{environment_name}` references undefined Component `{component_name}`"
            )));
        }
        let managed_component = managed.components.get(component_name).ok_or_else(|| {
            ConfigError::ManagedState(format!(
                "Environment `{environment_name}` Component `{component_name}` has no generation"
            ))
        })?;
        let root = managed_component.resolved_root.clone();
        validate_root(environment_name, component_name, &root)?;
        if let Some(configured_root) = &target.root
            && configured_root != &root
        {
            return Err(ConfigError::ManagedState(format!(
                "Environment `{environment_name}` Component `{component_name}` root does not match resolvedRoot"
            )));
        }
        let mut after = target.after.clone();
        after.sort();
        after.dedup();
        for dependency in &after {
            if dependency == component_name {
                return Err(ConfigError::SelfDependency {
                    environment: environment_name.into(),
                    component: component_name.clone(),
                });
            }
            if !environment.components.contains_key(dependency) {
                return Err(ConfigError::UnknownDependency {
                    environment: environment_name.into(),
                    component: component_name.clone(),
                    dependency: dependency.clone(),
                });
            }
        }
        targets.insert(
            component_name.clone(),
            TargetConfig {
                destination: target.destination.clone(),
                generation: managed_component.generation,
                root,
                systemd: target.systemd.clone(),
                health: target.health.clone(),
                after,
            },
        );
    }
    validate_root_overlaps(environment_name, &targets)?;
    topology::activation_order(environment_name, &targets, targets.keys())?;
    Ok(EnvironmentConfig {
        id: managed.id.clone(),
        components: targets,
    })
}

fn normalize_component(
    name: ComponentName,
    raw: model::RawComponent,
) -> Result<(ComponentName, ComponentConfig), ConfigError> {
    let working_directory = raw
        .working_directory
        .unwrap_or_else(|| PathBuf::from(name.as_str()));
    validate_relative_path(&name, "working directory", &working_directory)?;
    let artifact = ArtifactSpec { path: raw.artifact };
    validate_relative_path(&name, "Artifact", &artifact.path)?;
    let build = raw
        .build
        .into_iter()
        .map(BuildCommand::from_argv)
        .collect::<Vec<_>>();
    if build.is_empty() || build.iter().any(|command| command.program.is_empty()) {
        return Err(ConfigError::EmptyBuild(name));
    }
    Ok((
        name,
        ComponentConfig {
            working_directory,
            build,
            artifact,
        },
    ))
}

fn validate_name(field: &'static str, value: &str) -> Result<(), ConfigError> {
    ComponentName::parse(value.to_owned())
        .map(|_| ())
        .map_err(|_| ConfigError::InvalidName {
            field,
            value: value.to_owned(),
        })
}

fn validate_relative_path(
    component: &ComponentName,
    field: &'static str,
    path: &Path,
) -> Result<(), ConfigError> {
    let safe = !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, PathComponent::Normal(_) | PathComponent::CurDir));
    if safe {
        Ok(())
    } else {
        Err(ConfigError::UnsafePath {
            component: component.clone(),
            field,
            path: path.to_owned(),
        })
    }
}

fn validate_root(
    environment: &str,
    component: &ComponentName,
    root: &str,
) -> Result<(), ConfigError> {
    let segments = root_segments(root);
    if !root.starts_with('/') || segments.is_none() {
        return Err(ConfigError::UnsafeRoot {
            environment: environment.into(),
            component: component.clone(),
            root: root.into(),
        });
    }
    Ok(())
}

fn root_segments(root: &str) -> Option<Vec<&str>> {
    let segments = root.split('/').skip(1).collect::<Vec<_>>();
    (!segments.is_empty()
        && segments
            .iter()
            .all(|segment| !segment.is_empty() && *segment != "." && *segment != ".."))
    .then_some(segments)
}

fn validate_root_overlaps(
    environment: &str,
    targets: &BTreeMap<ComponentName, TargetConfig>,
) -> Result<(), ConfigError> {
    for (left_name, left) in targets {
        for (_, right) in targets.range((
            std::ops::Bound::Excluded(left_name),
            std::ops::Bound::Unbounded,
        )) {
            if left.destination != right.destination {
                continue;
            }
            let left_segments = root_segments(&left.root).expect("roots were already validated");
            let right_segments = root_segments(&right.root).expect("roots were already validated");
            if left_segments.starts_with(&right_segments)
                || right_segments.starts_with(&left_segments)
            {
                return Err(ConfigError::OverlappingRoots {
                    environment: environment.into(),
                    destination: left.destination.clone(),
                    first: left.root.clone(),
                    second: right.root.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
