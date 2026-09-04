//! Confirmed edits of existing project intent, without remote effects or new identities at save.

use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata, OpenOptions, Permissions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use atomic_write_file::AtomicWriteFile;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{
        self, ComponentSetup, DestinationRegistry, DestinationRevisionRecord, DestinationSummary,
        EnvironmentRename, EnvironmentSetup, PreparedProjectUpdate, ProjectConfig, ProjectSetup,
        TargetSetup,
    },
    domain::DestinationKey,
    projects::DiscoveryReport,
};

use super::DeploymentSession;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_COMPONENTS: usize = 256;
const MAX_ENVIRONMENTS: usize = 256;
const MAX_TARGETS: usize = 4096;
const MAX_FIELD_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct ProjectEditService {
    destinations_path: PathBuf,
    session: Arc<DeploymentSession>,
}

/// UI edits only user intent. The original identities and file evidence are private.
#[derive(Clone)]
pub struct ProjectEditDraft {
    pub setup: ProjectSetup,
    pub environment_renames: Vec<EnvironmentRename>,
    basis: Arc<EditBasis>,
}

impl std::fmt::Debug for ProjectEditDraft {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectEditDraft")
            .finish_non_exhaustive()
    }
}

impl ProjectEditDraft {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.basis.root
    }

    #[must_use]
    pub fn original(&self) -> &ProjectConfig {
        &self.basis.original
    }

    #[must_use]
    pub fn destinations(&self) -> &[DestinationSummary] {
        &self.basis.destinations
    }
}

#[derive(Clone)]
pub struct ProjectEditPreview {
    draft: ProjectEditDraft,
    prepared: PreparedProjectUpdate,
    destinations: BTreeMap<DestinationKey, DestinationRevisionRecord>,
}

impl std::fmt::Debug for ProjectEditPreview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectEditPreview")
            .finish_non_exhaustive()
    }
}

impl ProjectEditPreview {
    #[must_use]
    pub fn root(&self) -> &Path {
        self.draft.root()
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
    pub const fn draft(&self) -> &ProjectEditDraft {
        &self.draft
    }

    #[must_use]
    pub fn destinations(&self) -> &[DestinationSummary] {
        self.draft.destinations()
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProjectEditError {
    #[error("project editing is unavailable while another operation is active")]
    Busy,
    #[error("project edit cancelled; configuration was not saved")]
    Cancelled,
    #[error("the project configuration or connection registry is missing")]
    Missing,
    #[error("configuration changed since editing began; reload and review again")]
    Stale,
    #[error("configuration must use regular unlinked files inside an unchanged project directory")]
    UnsafePath,
    #[error("configuration exceeds bounded editing limits")]
    Limit,
    #[error("invalid project configuration: {0}")]
    Invalid(&'static str),
    #[error("connection registry is invalid or a selected connection no longer exists")]
    Destination,
    #[error("project configuration could not be safely read")]
    Read,
    #[error("project configuration save is unconfirmed; reload before retrying")]
    SaveUnconfirmed,
    #[error("component discovery could not be completed safely")]
    Discovery,
}

impl ProjectEditService {
    #[must_use]
    pub const fn new(destinations_path: PathBuf, session: Arc<DeploymentSession>) -> Self {
        Self {
            destinations_path,
            session,
        }
    }

    /// Reads existing intent and bounded connection choices without writing or connecting.
    ///
    /// # Errors
    /// Rejects busy sessions, cancellation and missing, linked, corrupt or oversized files.
    pub async fn load(
        &self,
        root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<ProjectEditDraft, ProjectEditError> {
        self.session
            .run(async {
                cancelled(cancellation)?;
                let root_identity = directory_identity(root)?;
                let root = root.canonicalize().map_err(|_| ProjectEditError::Read)?;
                let source = FileSnapshot::read(&root.join(config::PROJECT_FILE))?;
                let original = config::parse_contents(&root, source.text()?)
                    .map_err(|error| config_error(&error))?;
                let registry = FileSnapshot::read(&self.destinations_path)?;
                let parsed = parse_registry(&registry)?;
                let setup = setup_from_config(&original);
                bound_setup(&setup, &[])?;
                if parsed.summaries().len() > MAX_TARGETS {
                    return Err(ProjectEditError::Limit);
                }
                let basis = EditBasis {
                    root,
                    root_identity,
                    original,
                    source,
                    registry,
                    destinations: parsed.summaries(),
                };
                basis.ensure_unchanged()?;
                cancelled(cancellation)?;
                Ok(ProjectEditDraft {
                    setup,
                    environment_renames: Vec::new(),
                    basis: Arc::new(basis),
                })
            })
            .await
            .map_err(|_| ProjectEditError::Busy)?
    }

    /// Creates exact normalized YAML and new Environment IDs once, without file writes.
    ///
    /// # Errors
    /// Rejects stale drafts, invalid user intent, missing connections and cancellation.
    pub async fn preview(
        &self,
        draft: ProjectEditDraft,
        cancellation: &CancellationToken,
    ) -> Result<ProjectEditPreview, ProjectEditError> {
        self.session
            .run(async {
                cancelled(cancellation)?;
                self.ensure_basis(&draft.basis)?;
                bound_setup(&draft.setup, &draft.environment_renames)?;
                let prepared = config::prepare_update(
                    draft.original(),
                    draft.setup.clone(),
                    &draft.environment_renames,
                )
                .map_err(|error| config_error(&error))?;
                if prepared.preview().len() > MAX_FILE_BYTES {
                    return Err(ProjectEditError::Limit);
                }
                let registry = parse_registry(&draft.basis.registry)?;
                let destinations = selected_destinations(prepared.config(), &registry)?;
                self.ensure_basis(&draft.basis)?;
                cancelled(cancellation)?;
                Ok(ProjectEditPreview {
                    draft,
                    prepared,
                    destinations,
                })
            })
            .await
            .map_err(|_| ProjectEditError::Busy)?
    }

    /// Atomically saves only the frozen preview after rechecking local evidence.
    /// This is a same-session guard, not a lock against external editors. A failed
    /// save never attempts to restore old bytes over a possibly changed file.
    ///
    /// # Errors
    /// Rejects busy sessions, cancellation and drift; reports uncertain filesystem failures.
    pub async fn save(
        &self,
        preview: ProjectEditPreview,
        cancellation: &CancellationToken,
    ) -> Result<ProjectConfig, ProjectEditError> {
        self.session
            .run(async {
                cancelled(cancellation)?;
                self.ensure_preview(&preview)?;
                let mut file = AtomicWriteFile::open(&preview.draft.basis.source.path)
                    .map_err(|_| ProjectEditError::SaveUnconfirmed)?;
                file.write_all(preview.yaml().as_bytes())
                    .and_then(|()| {
                        file.as_file()
                            .set_permissions(preview.draft.basis.source.permissions.clone())
                    })
                    .map_err(|_| ProjectEditError::SaveUnconfirmed)?;
                // Recheck after writing the temporary file, immediately before its atomic rename.
                self.ensure_preview(&preview)?;
                cancelled(cancellation)?;
                file.commit()
                    .map_err(|_| ProjectEditError::SaveUnconfirmed)?;
                Ok(preview.config().clone())
            })
            .await
            .map_err(|_| ProjectEditError::Busy)?
    }

    /// Offers existing repository-discovery candidates; never executes build scripts.
    ///
    /// # Errors
    /// Rejects stale input, cancellation, discovery failures and unsafe candidate data.
    pub async fn discover(
        &self,
        draft: &ProjectEditDraft,
        cancellation: &CancellationToken,
    ) -> Result<DiscoveryReport, ProjectEditError> {
        self.session
            .run(async {
                cancelled(cancellation)?;
                self.ensure_basis(&draft.basis)?;
                let report = crate::projects::discover_components(draft.root())
                    .map_err(|_| ProjectEditError::Discovery)?;
                if report.components.len() > MAX_COMPONENTS || report.notices.len() > MAX_COMPONENTS
                {
                    return Err(ProjectEditError::Limit);
                }
                let mut bytes = 0;
                for candidate in &report.components {
                    bound_component(&candidate.setup, &mut bytes)?;
                    add_text(&candidate.source.to_string_lossy(), &mut bytes)?;
                    let argv: Vec<Vec<&str>> = candidate
                        .setup
                        .build
                        .iter()
                        .map(|command| {
                            std::iter::once(command.program.as_str())
                                .chain(command.args.iter().map(String::as_str))
                                .collect()
                        })
                        .collect();
                    let text =
                        serde_json::to_string(&argv).map_err(|_| ProjectEditError::Discovery)?;
                    crate::telemetry::detect_sensitive_config(&text)
                        .map_err(|_| ProjectEditError::Invalid("sensitive data is not allowed"))?;
                }
                for notice in &report.notices {
                    add_text(notice, &mut bytes)?;
                }
                self.ensure_basis(&draft.basis)?;
                cancelled(cancellation)?;
                Ok(report)
            })
            .await
            .map_err(|_| ProjectEditError::Busy)?
    }

    fn ensure_basis(&self, basis: &EditBasis) -> Result<(), ProjectEditError> {
        if self.destinations_path != basis.registry.path {
            return Err(ProjectEditError::Stale);
        }
        basis.ensure_unchanged()
    }

    fn ensure_preview(&self, preview: &ProjectEditPreview) -> Result<(), ProjectEditError> {
        self.ensure_basis(&preview.draft.basis)?;
        let registry = parse_registry(&preview.draft.basis.registry)?;
        if selected_destinations(preview.config(), &registry)? != preview.destinations {
            return Err(ProjectEditError::Stale);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct EditBasis {
    root: PathBuf,
    root_identity: (u64, u64),
    original: ProjectConfig,
    source: FileSnapshot,
    registry: FileSnapshot,
    destinations: Vec<DestinationSummary>,
}

impl EditBasis {
    fn ensure_unchanged(&self) -> Result<(), ProjectEditError> {
        if directory_identity(&self.root)? != self.root_identity {
            return Err(ProjectEditError::Stale);
        }
        self.source.ensure_unchanged()?;
        self.registry.ensure_unchanged()
    }
}

fn setup_from_config(config: &ProjectConfig) -> ProjectSetup {
    ProjectSetup {
        project: config.project.clone(),
        components: config
            .components
            .iter()
            .map(|(name, component)| {
                (
                    name.clone(),
                    ComponentSetup {
                        working_directory: Some(component.working_directory.clone()),
                        build: component.build.clone(),
                        artifact: component.artifact.clone(),
                    },
                )
            })
            .collect(),
        environments: config
            .environments
            .iter()
            .map(|(name, environment)| {
                (
                    name.clone(),
                    EnvironmentSetup {
                        components: environment
                            .components
                            .iter()
                            .map(|(name, target)| {
                                (
                                    name.clone(),
                                    TargetSetup {
                                        destination: target.destination.clone(),
                                        root: Some(target.root.clone()),
                                        systemd: target.systemd.clone(),
                                        health: target.health.clone(),
                                        after: target.after.clone(),
                                    },
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

fn selected_destinations(
    config: &ProjectConfig,
    registry: &DestinationRegistry,
) -> Result<BTreeMap<DestinationKey, DestinationRevisionRecord>, ProjectEditError> {
    config
        .environments
        .values()
        .flat_map(|environment| environment.components.values())
        .map(|target| {
            registry
                .resolve(&target.destination)
                .cloned()
                .map(|record| (target.destination.clone(), record))
                .ok_or(ProjectEditError::Destination)
        })
        .collect()
}

fn bound_setup(
    setup: &ProjectSetup,
    renames: &[EnvironmentRename],
) -> Result<(), ProjectEditError> {
    if setup.components.len() > MAX_COMPONENTS
        || setup.environments.len() > MAX_ENVIRONMENTS
        || renames.len() > MAX_ENVIRONMENTS
    {
        return Err(ProjectEditError::Limit);
    }
    let mut bytes = 0;
    add_text(&setup.project, &mut bytes)?;
    for (name, component) in &setup.components {
        add_text(name.as_str(), &mut bytes)?;
        bound_component(component, &mut bytes)?;
    }
    let mut targets = 0;
    for (name, environment) in &setup.environments {
        add_text(name, &mut bytes)?;
        targets += environment.components.len();
        if targets > MAX_TARGETS {
            return Err(ProjectEditError::Limit);
        }
        for (name, target) in &environment.components {
            add_text(name.as_str(), &mut bytes)?;
            add_text(target.destination.as_str(), &mut bytes)?;
            for text in [
                target.root.as_deref(),
                target.systemd.as_deref(),
                target.health.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                add_text(text, &mut bytes)?;
            }
            if target.after.len() > MAX_COMPONENTS {
                return Err(ProjectEditError::Limit);
            }
            for dependency in &target.after {
                add_text(dependency.as_str(), &mut bytes)?;
            }
        }
    }
    for rename in renames {
        add_text(&rename.from, &mut bytes)?;
        add_text(&rename.to, &mut bytes)?;
    }
    Ok(())
}

fn bound_component(component: &ComponentSetup, bytes: &mut usize) -> Result<(), ProjectEditError> {
    if let Some(directory) = &component.working_directory {
        add_text(&directory.to_string_lossy(), bytes)?;
    }
    add_text(&component.artifact.path.to_string_lossy(), bytes)?;
    if component.build.len() > 64 {
        return Err(ProjectEditError::Limit);
    }
    for command in &component.build {
        if command.shell {
            return Err(ProjectEditError::Invalid(
                "build requires argv commands, never Shell strings",
            ));
        }
        if command.args.len() > 256 {
            return Err(ProjectEditError::Limit);
        }
        add_text(&command.program, bytes)?;
        for argument in &command.args {
            add_text(argument, bytes)?;
        }
    }
    Ok(())
}

fn add_text(text: &str, total: &mut usize) -> Result<(), ProjectEditError> {
    *total = total.saturating_add(text.len());
    if text.len() > MAX_FIELD_BYTES || *total > MAX_FILE_BYTES {
        return Err(ProjectEditError::Limit);
    }
    if text.chars().any(|value| {
        value.is_control() || matches!(value, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        return Err(ProjectEditError::Invalid(
            "fields cannot contain terminal control characters",
        ));
    }
    Ok(())
}

fn parse_registry(snapshot: &FileSnapshot) -> Result<DestinationRegistry, ProjectEditError> {
    DestinationRegistry::from_yaml(&snapshot.path, snapshot.text()?)
        .map_err(|_| ProjectEditError::Destination)
}

fn config_error(error: &config::ConfigError) -> ProjectEditError {
    use config::ConfigError;
    let reason = match error {
        ConfigError::Security(_) => "sensitive values or unsafe command data are not allowed",
        ConfigError::InvalidName { .. } => {
            "names must use lowercase letters, digits and single hyphens"
        }
        ConfigError::EmptyBuild(_) => "every Component needs a nonempty argv build command",
        ConfigError::EmptyProject => "at least one Component and Environment are required",
        ConfigError::UnsafePath { .. } => "Component paths must remain relative to the Project",
        ConfigError::UnsafeRoot { .. } | ConfigError::OverlappingRoots { .. } => {
            "deployment roots must be safe, absolute and nonoverlapping"
        }
        ConfigError::UnknownDependency { .. }
        | ConfigError::SelfDependency { .. }
        | ConfigError::DependencyCycle { .. } => {
            "after must reference configured Components without cycles"
        }
        ConfigError::InvalidEnvironmentRename(_) => {
            "Environment renames must be explicit one-to-one replacements"
        }
        ConfigError::GenerationOverflow { .. } => "Component generation counter is exhausted",
        _ => "YAML structure or system-maintained identity is invalid",
    };
    ProjectEditError::Invalid(reason)
}

fn cancelled(cancellation: &CancellationToken) -> Result<(), ProjectEditError> {
    if cancellation.is_cancelled() {
        Err(ProjectEditError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Clone)]
struct FileSnapshot {
    path: PathBuf,
    bytes: Vec<u8>,
    stamp: FileStamp,
    permissions: Permissions,
}

impl FileSnapshot {
    fn read(path: &Path) -> Result<Self, ProjectEditError> {
        let mut file = open_safe(path, false)?;
        let metadata = file.metadata().map_err(|_| ProjectEditError::Read)?;
        let stamp = file_stamp(&file, &metadata)?;
        if stamp.size > MAX_FILE_BYTES as u64 {
            return Err(ProjectEditError::Limit);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ProjectEditError::Read)?;
        if bytes.len() as u64 != stamp.size
            || file_stamp(&file, &file.metadata().map_err(|_| ProjectEditError::Read)?)? != stamp
        {
            return Err(ProjectEditError::Stale);
        }
        let named = open_safe(path, false)?;
        if file_stamp(
            &named,
            &named.metadata().map_err(|_| ProjectEditError::Read)?,
        )? != stamp
        {
            return Err(ProjectEditError::Stale);
        }
        Ok(Self {
            path: path.to_owned(),
            bytes,
            stamp,
            permissions: metadata.permissions(),
        })
    }

    fn text(&self) -> Result<&str, ProjectEditError> {
        std::str::from_utf8(&self.bytes)
            .map_err(|_| ProjectEditError::Invalid("files must use UTF-8"))
    }

    fn ensure_unchanged(&self) -> Result<(), ProjectEditError> {
        let current = Self::read(&self.path)?;
        if current.stamp != self.stamp || current.bytes != self.bytes {
            Err(ProjectEditError::Stale)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    identity: (u64, u64),
    size: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    readonly: bool,
    mode: u32,
}

fn linked(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn open_safe(path: &Path, directory: bool) -> Result<File, ProjectEditError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProjectEditError::Missing
        } else {
            ProjectEditError::Read
        }
    })?;
    if linked(&metadata)
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        }
    {
        return Err(ProjectEditError::UnsafePath);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000 | if directory { 0x0200_0000 } else { 0 });
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0002_0000 | 0x800);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100 | 0x4);
    }
    let file = options.open(path).map_err(|_| ProjectEditError::Read)?;
    let opened = file.metadata().map_err(|_| ProjectEditError::Read)?;
    if linked(&opened)
        || if directory {
            !opened.is_dir()
        } else {
            !opened.is_file()
        }
    {
        return Err(ProjectEditError::UnsafePath);
    }
    Ok(file)
}

fn identity(file: &File, metadata: &Metadata) -> Result<(u64, u64), ProjectEditError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = file;
        if metadata.is_file() && metadata.nlink() != 1 {
            return Err(ProjectEditError::UnsafePath);
        }
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let info = winapi_util::file::information(file).map_err(|_| ProjectEditError::Read)?;
        if metadata.is_file() && info.number_of_links() != 1 {
            return Err(ProjectEditError::UnsafePath);
        }
        Ok((info.volume_serial_number(), info.file_index()))
    }
}

fn directory_identity(path: &Path) -> Result<(u64, u64), ProjectEditError> {
    let file = open_safe(path, true)?;
    identity(&file, &file.metadata().map_err(|_| ProjectEditError::Read)?)
}

fn file_stamp(file: &File, metadata: &Metadata) -> Result<FileStamp, ProjectEditError> {
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        metadata.mode()
    };
    #[cfg(not(unix))]
    let mode = 0;
    Ok(FileStamp {
        identity: identity(file, metadata)?,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        readonly: metadata.permissions().readonly(),
        mode,
    })
}

#[cfg(test)]
mod tests;
