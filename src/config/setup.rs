use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::Serialize;

use crate::domain::{ComponentGeneration, ComponentName, DestinationKey, EnvironmentId, ProjectId};

use super::{
    ArtifactSpec, BuildCommand, ConfigError, PROJECT_FILE, ProjectConfig, ProjectConfigState,
    RawProjectConfig, load, normalize,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSetup {
    pub project: String,
    pub components: BTreeMap<ComponentName, ComponentSetup>,
    pub environments: BTreeMap<String, EnvironmentSetup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentSetup {
    pub working_directory: Option<PathBuf>,
    pub build: Vec<BuildCommand>,
    pub artifact: ArtifactSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentSetup {
    pub components: BTreeMap<ComponentName, TargetSetup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSetup {
    pub destination: DestinationKey,
    pub root: Option<String>,
    pub service: Option<crate::config::ServiceConfig>,
    pub health: Option<String>,
    pub after: Vec<ComponentName>,
}

/// Returns the stable default remote root used when setup omits `root`.
#[must_use]
pub fn default_remote_root(project: &str, environment: &str, component: &ComponentName) -> String {
    format!("/srv/shipforge/{project}/{environment}/{component}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentRename {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Copy, Debug)]
pub struct ReinitializeConfirmation(());

impl ReinitializeConfirmation {
    /// Creates the marker only after the TUI has obtained explicit confirmation.
    #[must_use]
    pub const fn confirmed() -> Self {
        Self(())
    }
}

#[derive(Clone, Debug)]
pub struct PreparedProjectInitialization {
    contents: String,
    config: ProjectConfig,
}

/// Exact normalized edit bytes and identities, generated once before confirmation.
#[derive(Clone, Debug)]
pub struct PreparedProjectUpdate {
    contents: String,
    config: ProjectConfig,
}

impl PreparedProjectUpdate {
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.contents
    }

    #[must_use]
    pub const fn config(&self) -> &ProjectConfig {
        &self.config
    }
}

/// Prepares an existing Project edit without reading or writing any files.
/// New Environment identities are generated here, never again at confirmation.
///
/// # Errors
/// Rejects invalid intent, ambiguous renames, generation overflow and sensitive data.
pub fn prepare_update(
    current: &ProjectConfig,
    setup: ProjectSetup,
    environment_renames: &[EnvironmentRename],
) -> Result<PreparedProjectUpdate, ConfigError> {
    let generated = GeneratedProject::from_update(setup, current, environment_renames)?;
    let (contents, config) = prepare_generated(&generated)?;
    Ok(PreparedProjectUpdate { contents, config })
}

impl PreparedProjectInitialization {
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.contents
    }

    #[must_use]
    pub const fn config(&self) -> &ProjectConfig {
        &self.config
    }

    /// Commits exactly the previewed configuration without replacing an
    /// existing `shipforge.yaml`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file already exists or the no-clobber atomic
    /// commit fails.
    pub fn commit(self, project_root: &Path) -> Result<ProjectConfig, ConfigError> {
        let path = project_root.join(PROJECT_FILE);
        write_new_file(&path, self.contents.as_bytes())?;
        Ok(self.config)
    }
}

/// Generates and validates the exact configuration shown by the TUI before
/// any file is written.
///
/// # Errors
///
/// Returns an error if serialization, sensitive-value detection, or
/// normalization fails.
pub fn prepare_initialize(
    setup: ProjectSetup,
) -> Result<PreparedProjectInitialization, ConfigError> {
    let generated = GeneratedProject::from_setup(setup);
    let (contents, config) = prepare_generated(&generated)?;
    Ok(PreparedProjectInitialization { contents, config })
}

/// Creates a new Project configuration without overwriting an existing file.
///
/// # Errors
///
/// Returns an error if the file exists, setup is invalid, serialization fails,
/// or the atomic write cannot be committed.
pub fn initialize(project_root: &Path, setup: ProjectSetup) -> Result<ProjectConfig, ConfigError> {
    prepare_initialize(setup)?.commit(project_root)
}

/// Replaces a Project configuration with a newly generated identity.
///
/// # Errors
///
/// Returns an error if setup is invalid, serialization fails, or the atomic
/// replacement cannot be committed.
pub fn reinitialize(
    project_root: &Path,
    setup: ProjectSetup,
    _confirmation: ReinitializeConfirmation,
) -> Result<ProjectConfig, ConfigError> {
    write_generated(
        &project_root.join(PROJECT_FILE),
        &GeneratedProject::from_setup(setup),
    )
}

/// Updates user intent while preserving stable identities and roots.
///
/// Environment renames must be supplied explicitly. Target changes increment
/// the affected Component generation; build-only changes do not.
///
/// # Errors
///
/// Returns an error if the existing file is missing/invalid, rename mappings
/// are ambiguous, generation overflows, setup is invalid, or the atomic write
/// fails.
pub fn update(
    project_root: &Path,
    setup: ProjectSetup,
    environment_renames: &[EnvironmentRename],
) -> Result<ProjectConfig, ConfigError> {
    let current = match load(project_root)? {
        ProjectConfigState::Missing => return Err(ConfigError::MissingExistingConfig),
        ProjectConfigState::Loaded(config) => config,
    };
    let generated = GeneratedProject::from_update(setup, &current, environment_renames)?;
    write_generated(&project_root.join(PROJECT_FILE), &generated)
}

fn write_generated(
    path: &Path,
    generated: &GeneratedProject,
) -> Result<ProjectConfig, ConfigError> {
    let (contents, normalized) = prepare_generated(generated)?;
    let mut file = AtomicWriteFile::open(path).map_err(|source| ConfigError::Write {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.commit())
        .map_err(|source| ConfigError::Write {
            path: path.to_owned(),
            source,
        })?;
    Ok(normalized)
}

fn prepare_generated(generated: &GeneratedProject) -> Result<(String, ProjectConfig), ConfigError> {
    let contents = serde_yaml_ng::to_string(generated).map_err(ConfigError::Serialize)?;
    crate::telemetry::detect_sensitive_config(&contents)?;
    let raw: RawProjectConfig =
        serde_yaml_ng::from_str(&contents).map_err(ConfigError::Serialize)?;
    let normalized = normalize(raw)?;
    Ok((contents, normalized))
}

fn write_new_file(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    if path.exists() {
        return Err(ConfigError::AlreadyExists(path.to_owned()));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("shipforge.yaml");
    let temporary_path = path.with_file_name(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::now_v7().simple()
    ));
    let temporary = TemporaryFileGuard(temporary_path.clone());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|source| ConfigError::Write {
            path: temporary_path.clone(),
            source,
        })?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|source| ConfigError::Write {
            path: temporary_path.clone(),
            source,
        })?;
    match std::fs::hard_link(&temporary_path, path) {
        Ok(()) => {
            drop(file);
            drop(temporary);
            Ok(())
        }
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(ConfigError::AlreadyExists(path.to_owned()))
        }
        Err(source) => Err(ConfigError::Write {
            path: path.to_owned(),
            source,
        }),
    }
}

struct TemporaryFileGuard(PathBuf);

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedProject {
    schema_version: u32,
    #[serde(rename = "_shipforge")]
    managed: GeneratedManagedProject,
    project: String,
    components: BTreeMap<ComponentName, GeneratedComponent>,
    environments: BTreeMap<String, GeneratedEnvironment>,
}

impl GeneratedProject {
    fn from_setup(setup: ProjectSetup) -> Self {
        let managed_environments = setup
            .environments
            .iter()
            .map(|(environment_name, environment)| {
                let components = environment
                    .components
                    .iter()
                    .map(|(component_name, target)| {
                        let resolved_root = target.root.clone().unwrap_or_else(|| {
                            format!(
                                "/srv/shipforge/{}/{}/{}",
                                setup.project, environment_name, component_name
                            )
                        });
                        (
                            component_name.clone(),
                            GeneratedManagedComponent {
                                generation: ComponentGeneration::INITIAL,
                                resolved_root,
                            },
                        )
                    })
                    .collect();
                (
                    environment_name.clone(),
                    GeneratedManagedEnvironment {
                        id: EnvironmentId::new(),
                        components,
                    },
                )
            })
            .collect();

        Self {
            schema_version: 2,
            managed: GeneratedManagedProject {
                project_id: ProjectId::new(),
                environments: managed_environments,
            },
            project: setup.project,
            components: setup
                .components
                .into_iter()
                .map(|(name, component)| (name, component.into()))
                .collect(),
            environments: setup
                .environments
                .into_iter()
                .map(|(name, environment)| (name, environment.into()))
                .collect(),
        }
    }

    fn from_update(
        setup: ProjectSetup,
        current: &ProjectConfig,
        renames: &[EnvironmentRename],
    ) -> Result<Self, ConfigError> {
        validate_renames(&setup, current, renames)?;
        let renamed_sources = renames
            .iter()
            .map(|rename| (rename.to.as_str(), rename.from.as_str()))
            .collect::<BTreeMap<_, _>>();
        let managed_environments = setup
            .environments
            .iter()
            .map(|(environment_name, environment)| {
                let source_name = renamed_sources
                    .get(environment_name.as_str())
                    .copied()
                    .unwrap_or(environment_name);
                let previous = current.environments.get(source_name);
                build_updated_environment(&setup.project, environment_name, environment, previous)
                    .map(|managed| (environment_name.clone(), managed))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        Ok(Self {
            schema_version: 2,
            managed: GeneratedManagedProject {
                project_id: current.project_id.clone(),
                environments: managed_environments,
            },
            project: setup.project,
            components: setup
                .components
                .into_iter()
                .map(|(name, component)| (name, component.into()))
                .collect(),
            environments: setup
                .environments
                .into_iter()
                .map(|(name, environment)| (name, environment.into()))
                .collect(),
        })
    }
}

fn validate_renames(
    setup: &ProjectSetup,
    current: &ProjectConfig,
    renames: &[EnvironmentRename],
) -> Result<(), ConfigError> {
    let mut sources = std::collections::BTreeSet::new();
    let mut destinations = std::collections::BTreeSet::new();
    for rename in renames {
        if rename.from == rename.to
            || !sources.insert(rename.from.as_str())
            || !destinations.insert(rename.to.as_str())
            || !current.environments.contains_key(&rename.from)
            || !setup.environments.contains_key(&rename.to)
            || setup.environments.contains_key(&rename.from)
            || current.environments.contains_key(&rename.to)
        {
            return Err(ConfigError::InvalidEnvironmentRename(format!(
                "`{}` to `{}` is not a one-to-one replacement",
                rename.from, rename.to
            )));
        }
    }
    Ok(())
}

fn build_updated_environment(
    project_name: &str,
    environment_name: &str,
    environment: &EnvironmentSetup,
    previous: Option<&super::EnvironmentConfig>,
) -> Result<GeneratedManagedEnvironment, ConfigError> {
    let components = environment
        .components
        .iter()
        .map(|(component_name, target)| {
            let previous_target = previous.and_then(|item| item.components.get(component_name));
            let resolved_root = target
                .root
                .clone()
                .or_else(|| previous_target.map(|item| item.root.clone()))
                .unwrap_or_else(|| {
                    default_remote_root(project_name, environment_name, component_name)
                });
            let generation = match previous_target {
                Some(previous_target)
                    if target_matches_previous(target, &resolved_root, previous_target) =>
                {
                    previous_target.generation
                }
                Some(previous_target) => {
                    previous_target.generation.checked_next().ok_or_else(|| {
                        ConfigError::GenerationOverflow {
                            environment: environment_name.into(),
                            component: component_name.clone(),
                        }
                    })?
                }
                None => ComponentGeneration::INITIAL,
            };
            Ok((
                component_name.clone(),
                GeneratedManagedComponent {
                    generation,
                    resolved_root,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
    Ok(GeneratedManagedEnvironment {
        id: previous
            .map(|environment| environment.id.clone())
            .unwrap_or_default(),
        components,
    })
}

fn target_matches_previous(
    target: &TargetSetup,
    resolved_root: &str,
    previous: &super::TargetConfig,
) -> bool {
    let mut after = target.after.clone();
    after.sort();
    after.dedup();
    target.destination == previous.destination
        && resolved_root == previous.root
        && target.service == previous.service
        && target.health == previous.health
        && after == previous.after
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedManagedProject {
    project_id: ProjectId,
    environments: BTreeMap<String, GeneratedManagedEnvironment>,
}

#[derive(Serialize)]
struct GeneratedManagedEnvironment {
    id: EnvironmentId,
    components: BTreeMap<ComponentName, GeneratedManagedComponent>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedManagedComponent {
    generation: ComponentGeneration,
    resolved_root: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedComponent {
    #[serde(skip_serializing_if = "Option::is_none")]
    working_directory: Option<PathBuf>,
    build: Vec<GeneratedCommand>,
    artifact: PathBuf,
}

impl From<ComponentSetup> for GeneratedComponent {
    fn from(component: ComponentSetup) -> Self {
        Self {
            working_directory: component.working_directory,
            build: component.build.into_iter().map(Into::into).collect(),
            artifact: component.artifact.path,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeneratedCommand {
    Argv(Vec<String>),
    Structured(GeneratedStructuredCommand),
}

impl From<BuildCommand> for GeneratedCommand {
    fn from(command: BuildCommand) -> Self {
        if command.shell {
            Self::Structured(command.into())
        } else {
            let mut argv = vec![command.program];
            argv.extend(command.args);
            Self::Argv(argv)
        }
    }
}

#[derive(Serialize)]
struct GeneratedStructuredCommand {
    program: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    shell: bool,
}

impl From<BuildCommand> for GeneratedStructuredCommand {
    fn from(command: BuildCommand) -> Self {
        Self {
            program: command.program,
            args: command.args,
            shell: command.shell,
        }
    }
}

#[derive(Serialize)]
struct GeneratedEnvironment {
    components: BTreeMap<ComponentName, GeneratedTarget>,
}

impl From<EnvironmentSetup> for GeneratedEnvironment {
    fn from(environment: EnvironmentSetup) -> Self {
        Self {
            components: environment
                .components
                .into_iter()
                .map(|(name, target)| (name, target.into()))
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct GeneratedTarget {
    #[serde(rename = "to")]
    destination: DestinationKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<crate::config::ServiceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    after: Vec<ComponentName>,
}

impl From<TargetSetup> for GeneratedTarget {
    fn from(target: TargetSetup) -> Self {
        Self {
            destination: target.destination,
            root: target.root,
            service: target.service,
            health: target.health,
            after: target.after,
        }
    }
}
