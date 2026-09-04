use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ComponentConfig, DestinationRegistry, EnvironmentConfig, ProjectConfig},
    domain::{Capability, ComponentName, ComponentRelease, DestinationKey, ReleaseVersion},
    drivers::{ComponentExecutionContext, ComponentRequest, DriverLog, DriverRegistry, EventSink},
    history::{HistoryError, HistoryStore},
    telemetry::Redactor,
};

use super::{
    ApplicationError, DeploymentComponent, DeploymentOrchestrator, DeploymentReport,
    GitWorktreeState, PackageError, PlannedComponent, inspect_git, package_release, run_build,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug)]
pub struct DeploymentSelection {
    pub project_root: PathBuf,
    pub config: ProjectConfig,
    pub environment: String,
    pub components: BTreeSet<ComponentName>,
}

#[derive(Clone, Debug)]
pub struct DeploymentPlanEntry {
    pub component: ComponentName,
    pub destination: String,
    pub root: String,
    pub current: Option<ReleaseVersion>,
    pub release: ReleaseVersion,
    config: ComponentConfig,
    planned: PlannedComponent,
}

#[derive(Clone, Debug)]
pub struct DeploymentPlan {
    pub selection: DeploymentSelection,
    pub activation_order: Vec<ComponentName>,
    pub entries: Vec<DeploymentPlanEntry>,
    pub git: GitWorktreeState,
}

#[derive(Debug)]
pub struct DeploymentService {
    drivers: Arc<DriverRegistry>,
    history_path: PathBuf,
}

impl DeploymentService {
    #[must_use]
    pub fn new(drivers: Arc<DriverRegistry>, history_path: PathBuf) -> Self {
        Self {
            drivers,
            history_path,
        }
    }

    /// Performs local and remote read-only checks and freezes a Deployment plan.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid selection, missing Destination, local Git
    /// check failure, Driver validation failure, or remote preflight failure.
    pub async fn plan(
        &self,
        selection: DeploymentSelection,
        destinations: &DestinationRegistry,
        cancellation: &CancellationToken,
    ) -> Result<DeploymentPlan, DeploymentServiceError> {
        if selection.components.is_empty() {
            return Err(DeploymentServiceError::InvalidSelection(
                "select at least one Component".into(),
            ));
        }
        let environment = selection
            .config
            .environments
            .get(&selection.environment)
            .ok_or_else(|| {
                DeploymentServiceError::InvalidSelection(format!(
                    "Environment `{}` no longer exists",
                    selection.environment
                ))
            })?;
        let activation_order = environment
            .activation_order(&selection.environment, selection.components.iter())
            .map_err(|error| DeploymentServiceError::InvalidSelection(error.to_string()))?;
        let git = inspect_git(&selection.project_root, COMMAND_TIMEOUT, cancellation).await?;
        let summaries = destinations
            .summaries()
            .into_iter()
            .map(|summary| (summary.key.clone(), summary.endpoint))
            .collect::<BTreeMap<_, _>>();
        let mut entries = Vec::with_capacity(activation_order.len());
        for component in &activation_order {
            entries.push(
                self.plan_entry(
                    &selection,
                    environment,
                    component,
                    destinations,
                    &summaries,
                    cancellation,
                )
                .await?,
            );
        }
        Ok(DeploymentPlan {
            selection,
            activation_order,
            entries,
            git,
        })
    }

    async fn plan_entry(
        &self,
        selection: &DeploymentSelection,
        environment: &EnvironmentConfig,
        component: &ComponentName,
        destinations: &DestinationRegistry,
        summaries: &BTreeMap<DestinationKey, String>,
        cancellation: &CancellationToken,
    ) -> Result<DeploymentPlanEntry, DeploymentServiceError> {
        if cancellation.is_cancelled() {
            return Err(DeploymentServiceError::Cancelled);
        }
        let target = environment.components.get(component).ok_or_else(|| {
            DeploymentServiceError::InvalidSelection(format!(
                "Component `{component}` is not configured in Environment `{}`",
                selection.environment
            ))
        })?;
        let component_config = selection
            .config
            .components
            .get(component)
            .cloned()
            .ok_or_else(|| {
                DeploymentServiceError::InvalidSelection(format!(
                    "Component `{component}` has no build configuration"
                ))
            })?;
        let destination_record = destinations.resolve(&target.destination).ok_or_else(|| {
            DeploymentServiceError::MissingDestination(target.destination.to_string())
        })?;
        let resolved = destination_record.resolve();
        let driver = self
            .drivers
            .get(&resolved.driver)
            .ok_or_else(|| DeploymentServiceError::MissingDriver(resolved.driver.to_string()))?;
        let destination_settings = driver
            .validate_destination(&resolved.settings)
            .map_err(ApplicationError::Driver)?;
        let target_settings = driver
            .validate_target(&target.driver_input())
            .map_err(ApplicationError::Driver)?;
        let context = ComponentExecutionContext {
            project_id: selection.config.project_id.clone(),
            environment_id: environment.id.clone(),
            component: component.clone(),
            generation: target.generation,
            destination: target.destination.clone(),
            destination_revision: destination_record.revision,
            credential: resolved.credential,
            endpoint_fingerprint: resolved.endpoint_fingerprint,
            destination_settings,
            target: target_settings,
            cancellation: cancellation.clone(),
        };
        let release = ComponentRelease {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: component.clone(),
            generation: context.generation,
            version: generate_release_version()?,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
        };
        let component_plan = super::DeploymentPlanner::new(Arc::clone(&self.drivers))
            .plan_component(
                context,
                ComponentRequest {
                    release,
                    required_capabilities: required_capabilities(),
                },
            )
            .await?;
        Ok(DeploymentPlanEntry {
            component: component.clone(),
            destination: summaries
                .get(&target.destination)
                .cloned()
                .unwrap_or_else(|| target.destination.to_string()),
            root: target.root.clone(),
            current: component_plan
                .plan
                .expected_current
                .as_ref()
                .map(|release| release.version.clone()),
            release: component_plan.plan.release.version.clone(),
            config: component_config,
            planned: component_plan,
        })
    }

    /// Builds, packages, and deploys a previously frozen plan.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, build, packaging, persistence, or
    /// orchestration failure. Driver terminal failures are carried by the report.
    pub async fn execute(
        &self,
        plan: DeploymentPlan,
        events: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<DeploymentReport, DeploymentServiceError> {
        execute_plan(self, plan, events, cancellation).await
    }
}

async fn execute_plan(
    service: &DeploymentService,
    plan: DeploymentPlan,
    events: &dyn EventSink,
    cancellation: &CancellationToken,
) -> Result<DeploymentReport, DeploymentServiceError> {
    let packages = tempfile::tempdir().map_err(DeploymentServiceError::TemporaryDirectory)?;
    let mut components = Vec::with_capacity(plan.entries.len());
    for entry in plan.entries {
        let component = build_component(
            &plan.selection.project_root,
            entry,
            packages.path(),
            events,
            cancellation,
        )
        .await?;
        components.push(component);
    }
    let history = HistoryStore::open(&service.history_path)?;
    DeploymentOrchestrator::new(&history, Redactor::default())
        .deploy(components, &plan.activation_order, events, cancellation)
        .await
        .map_err(DeploymentServiceError::Orchestration)
}

async fn build_component(
    project_root: &std::path::Path,
    entry: DeploymentPlanEntry,
    package_directory: &std::path::Path,
    events: &dyn EventSink,
    cancellation: &CancellationToken,
) -> Result<DeploymentComponent, DeploymentServiceError> {
    if cancellation.is_cancelled() {
        return Err(DeploymentServiceError::Cancelled);
    }
    events.emit(DriverLog {
        namespace: "build.started".into(),
        message: format!("Building {}", entry.component),
    });
    run_build(
        project_root,
        &entry.config.working_directory,
        &entry.config.build,
        true,
        COMMAND_TIMEOUT,
        cancellation,
    )
    .await?;
    let artifact = entry
        .config
        .artifact
        .resolve(project_root, &entry.config.working_directory)?;
    events.emit(DriverLog {
        namespace: "build.packaging".into(),
        message: format!("Packaging {} as {}.tar.gz", entry.component, entry.release),
    });
    let package = package_release(
        &artifact,
        &entry.planned.plan.release,
        package_directory,
        unix_seconds()?,
        None,
        cancellation,
    )?;
    Ok(DeploymentComponent {
        planned: entry.planned,
        package,
    })
}

fn generate_release_version() -> Result<ReleaseVersion, DeploymentServiceError> {
    let timestamp = unix_seconds()?;
    let random = uuid::Uuid::now_v7().simple().to_string();
    ReleaseVersion::parse(format!("{timestamp}-{}", &random[..8]))
        .map_err(|error| DeploymentServiceError::Version(error.to_string()))
}

fn required_capabilities() -> BTreeSet<Capability> {
    BTreeSet::from([
        Capability::StagedDeployment,
        Capability::ExplicitActivation,
        Capability::Observe,
        Capability::Rollback,
        Capability::Cancellation,
    ])
}

fn unix_seconds() -> Result<u64, DeploymentServiceError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| DeploymentServiceError::Clock(error.to_string()))
}

#[derive(Debug, Error)]
pub enum DeploymentServiceError {
    #[error("invalid Deployment selection: {0}")]
    InvalidSelection(String),
    #[error("Destination `{0}` is missing from the user registry")]
    MissingDestination(String),
    #[error("no compiled Driver is registered for `{0}`")]
    MissingDriver(String),
    #[error("Deployment was cancelled")]
    Cancelled,
    #[error("system clock error: {0}")]
    Clock(String),
    #[error("could not generate Release version: {0}")]
    Version(String),
    #[error("could not create temporary Release directory: {0}")]
    TemporaryDirectory(std::io::Error),
    #[error(transparent)]
    Build(#[from] super::BuildError),
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Application(#[from] ApplicationError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Orchestration(#[from] super::OrchestrationError),
}
