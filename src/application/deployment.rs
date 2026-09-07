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
    GitWorktreeState, PackageError, PlannedComponent, inspect_git, package_release,
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
    pub notices: Vec<String>,
    config: ComponentConfig,
    planned: PlannedComponent,
    destination_snapshot: crate::config::DestinationRevisionRecord,
}

#[derive(Clone, Debug)]
pub struct DeploymentPlan {
    pub selection: DeploymentSelection,
    pub activation_order: Vec<ComponentName>,
    pub entries: Vec<DeploymentPlanEntry>,
    pub git: GitWorktreeState,
    pub git_metadata: super::build::GitMetadata,
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
        if selection.config.schema_version != 2 {
            return Err(DeploymentServiceError::InvalidSelection("Open the project editor, preview and confirm the service configuration upgrade before deployment.".into()));
        }
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
        let git_metadata = super::build::inspect_git_metadata(
            &selection.project_root,
            &git,
            COMMAND_TIMEOUT,
            cancellation,
        )
        .await?;
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
            git_metadata,
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
        super::build::check_build_inputs(
            &selection.project_root,
            &component_config.working_directory,
            &component_config.build,
        )?;
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
        let mut notices = component_plan.notices.clone();
        if component_plan
            .plan
            .effective_capabilities
            .contains(Capability::Retention)
        {
            notices.push(format!(
                "After success, keep the newest {} versions plus current, previous healthy, and referenced versions; safely eligible older versions may be removed.",
                super::retention::DEFAULT_RETAIN_COUNT,
            ));
        }
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
            notices,
            planned: component_plan,
            destination_snapshot: destination_record.clone(),
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
        destinations_path: &std::path::Path,
        events: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<DeploymentReport, DeploymentServiceError> {
        let git = inspect_git(&plan.selection.project_root, COMMAND_TIMEOUT, cancellation).await?;
        let metadata = super::build::inspect_git_metadata(
            &plan.selection.project_root,
            &git,
            COMMAND_TIMEOUT,
            cancellation,
        )
        .await?;
        if git != plan.git || metadata != plan.git_metadata {
            return Err(DeploymentServiceError::StalePlan("Git branch, commit, or worktree status changed after preview; check and confirm again".into()));
        }
        execute_plan(self, plan, destinations_path, events, cancellation).await
    }
}

async fn execute_plan(
    service: &DeploymentService,
    mut plan: DeploymentPlan,
    destinations_path: &std::path::Path,
    events: &dyn EventSink,
    cancellation: &CancellationToken,
) -> Result<DeploymentReport, DeploymentServiceError> {
    validate_saved_plan(&plan, destinations_path)?;
    if cancellation.is_cancelled() {
        return Err(DeploymentServiceError::Cancelled);
    }
    guard_driver_mutations(&mut plan, destinations_path);
    let packages = tempfile::tempdir().map_err(DeploymentServiceError::TemporaryDirectory)?;
    let history = HistoryStore::open(&service.history_path)?;
    let orchestrator = DeploymentOrchestrator::new(&history, Redactor::default());
    let first = plan.entries.first().ok_or_else(|| {
        DeploymentServiceError::InvalidSelection("select at least one Component".into())
    })?;
    let deployment = orchestrator.start_for_context(&first.planned.context)?;
    let clock = orchestrator.clock();
    let setup = snapshot_plan(&history, &orchestrator, &deployment.id, &plan).and_then(|()| {
        open_deployment_log(
            &service.history_path,
            &history,
            &deployment.id,
            events,
            cancellation,
        )
    });
    let logs = match setup {
        Ok(logs) => logs,
        Err(error) => {
            return Err(before_remote_failure(
                &history,
                &deployment.id,
                clock,
                crate::domain::DeploymentState::Failed,
                error,
                None,
            ));
        }
    };
    let events = &logs as &dyn EventSink;
    events.emit_record(crate::telemetry::log_record::LogEvent {
        namespace: "deployment.started".into(),
        message: format!("Deployment {} started", deployment.id),
        scope: None,
        kind: crate::telemetry::log_record::LogEventKind::DeploymentStarted {
            deployment: deployment.id.clone(),
        },
    });
    let built = build_plan(
        &plan,
        destinations_path,
        packages.path(),
        events,
        cancellation,
        &history,
        &deployment.id,
        clock,
    )
    .await;
    let components = match built {
        Ok(components) => components,
        Err(error) => {
            events.emit(DriverLog {
                namespace: "deployment.failed".into(),
                message: error.to_string(),
            });
            let log_failure = logs.finish().await;
            let terminal = if cancellation.is_cancelled() && log_failure.is_none() {
                crate::domain::DeploymentState::Cancelled
            } else {
                crate::domain::DeploymentState::Failed
            };
            return Err(before_remote_failure(
                &history,
                &deployment.id,
                clock,
                terminal,
                error,
                log_failure,
            ));
        }
    };
    let deployment_id = deployment.id.clone();
    let result = orchestrator
        .deploy_started(
            deployment,
            components,
            &plan.activation_order,
            events,
            cancellation,
        )
        .await;
    let mut report = match result {
        Ok(report) => report,
        Err(error) => {
            let log_failure = logs.finish().await;
            return Err(execution_error(&deployment_id, error.into(), log_failure));
        }
    };
    emit_finished(&report, events);
    attach_log_failure(&mut report, logs.finish().await);
    Ok(report)
}

fn emit_finished(report: &DeploymentReport, events: &dyn EventSink) {
    events.emit(DriverLog {
        namespace: "deployment.finished".into(),
        message: format!(
            "Deployment {} finished with {:?}",
            report.deployment.id, report.deployment.state,
        ),
    });
}

fn snapshot_plan(
    history: &HistoryStore,
    orchestrator: &DeploymentOrchestrator<'_>,
    deployment: &crate::domain::DeploymentId,
    plan: &DeploymentPlan,
) -> Result<(), DeploymentServiceError> {
    let ordered = plan
        .activation_order
        .iter()
        .map(|name| {
            plan.entries
                .iter()
                .find(|entry| &entry.component == name)
                .map(|entry| &entry.planned)
                .ok_or_else(|| {
                    DeploymentServiceError::InvalidSelection(
                        "activation order contains an unselected Component".into(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if ordered.len() != plan.entries.len() {
        return Err(DeploymentServiceError::InvalidSelection(
            "activation order must include every selected Component".into(),
        ));
    }
    orchestrator.snapshot_components(deployment, ordered, true)?;
    history.record_deployment_metadata(
        deployment,
        &crate::history::DeploymentMetadata {
            git_branch: plan.git_metadata.branch.clone(),
            git_revision: plan.git_metadata.revision.clone(),
            git_worktree: match &plan.git {
                GitWorktreeState::Clean => crate::history::GitWorktree::Clean,
                GitWorktreeState::Dirty { .. } => crate::history::GitWorktree::Dirty,
                GitWorktreeState::NotRepository => crate::history::GitWorktree::NotRepository,
            },
            operator: std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok(),
        },
        &Redactor::default(),
    )?;
    Ok(())
}

pub(super) fn open_deployment_log<'a>(
    history_path: &std::path::Path,
    history: &HistoryStore,
    deployment: &crate::domain::DeploymentId,
    events: &'a dyn EventSink,
    cancellation: &CancellationToken,
) -> Result<super::deployment_logs::DeploymentLogSink<'a>, DeploymentServiceError> {
    history.register_event_log(deployment, super::deployment_logs::LOG_MAX_BYTES, 3)?;
    super::deployment_logs::DeploymentLogSink::open_events(
        &history_path.with_file_name("logs"),
        deployment,
        events,
        cancellation,
    )
    .map_err(|error| DeploymentServiceError::Log(error.to_string()))
}

fn execution_error(
    deployment: &crate::domain::DeploymentId,
    source: DeploymentServiceError,
    log_failure: Option<String>,
) -> DeploymentServiceError {
    DeploymentServiceError::Execution {
        deployment: deployment.clone(),
        source: Box::new(source),
        log_diagnostic: log_failure.map_or_else(String::new, |error| {
            format!("; Deployment log incomplete: {error}")
        }),
    }
}

fn before_remote_failure(
    history: &HistoryStore,
    deployment: &crate::domain::DeploymentId,
    clock: &super::clock::MonotonicClock,
    terminal: crate::domain::DeploymentState,
    original: DeploymentServiceError,
    log_failure: Option<String>,
) -> DeploymentServiceError {
    let persisted = clock
        .timestamp()
        .map_err(DeploymentServiceError::from)
        .and_then(|timestamp| {
            history
                .transition_deployment(
                    deployment,
                    crate::domain::DeploymentState::Running,
                    terminal,
                    timestamp,
                )
                .map_err(DeploymentServiceError::from)
        });
    let error = if let Err(persistence) = persisted {
        DeploymentServiceError::PersistenceAfterFailure {
            original: Box::new(original),
            persistence: Box::new(persistence),
        }
    } else {
        original
    };
    execution_error(deployment, error, log_failure)
}

fn attach_log_failure(report: &mut DeploymentReport, log_failure: Option<String>) {
    if let Some(error) = log_failure {
        report.warnings.push(format!(
            "Deployment log incomplete: {error}. The reported deployment outcome and recovery instructions still apply; inspect history before retrying."
        ));
    }
}

fn guard_driver_mutations(plan: &mut DeploymentPlan, destinations_path: &std::path::Path) {
    let guard = Arc::new(super::execution_guard::ExecutionGuard::new(
        plan.selection.project_root.clone(),
        plan.selection.config.clone(),
        destinations_path.to_owned(),
        plan.entries
            .iter()
            .map(|entry| {
                (
                    entry.planned.context.destination.clone(),
                    entry.destination_snapshot.clone(),
                )
            })
            .collect(),
    ));
    for entry in &mut plan.entries {
        entry.planned.driver = guard.wrap(
            Arc::clone(&entry.planned.driver),
            entry.planned.context.clone(),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_plan(
    plan: &DeploymentPlan,
    destinations_path: &std::path::Path,
    packages: &std::path::Path,
    events: &dyn EventSink,
    cancellation: &CancellationToken,
    history: &HistoryStore,
    deployment: &crate::domain::DeploymentId,
    clock: &super::clock::MonotonicClock,
) -> Result<Vec<DeploymentComponent>, DeploymentServiceError> {
    let mut components = Vec::with_capacity(plan.entries.len());
    let mut build_entries = plan.entries.clone();
    build_entries.sort_by(|left, right| left.component.cmp(&right.component));
    for entry in build_entries {
        validate_saved_plan(plan, destinations_path)?;
        let intent = history.record_intent(
            deployment,
            &entry.component,
            "build-package",
            entry.release.as_str(),
            clock.timestamp()?,
        )?;
        let step_events =
            super::step_events::StepEvents::start(events, &entry.component, "build-package");
        let result = build_component(
            &plan.selection.project_root,
            entry,
            packages,
            &step_events,
            cancellation,
        )
        .await;
        let built = result.is_ok();
        let result = result.and_then(|component| {
            DeploymentOrchestrator::new(history, Redactor::default())
                .record_package(deployment, &component)?;
            Ok(component)
        });
        let (status, diagnostic) = match &result {
            Ok(_) => (crate::history::IntentStatus::Succeeded, None),
            Err(error) => (
                crate::history::IntentStatus::Failed,
                Some(error.to_string()),
            ),
        };
        let completed = clock
            .timestamp()
            .map_err(DeploymentServiceError::from)
            .and_then(|timestamp| {
                history
                    .complete_intent(
                        intent,
                        status,
                        diagnostic.as_deref(),
                        timestamp,
                        &Redactor::default(),
                    )
                    .map_err(DeploymentServiceError::from)
            });
        step_events.finish(
            super::step_events::step_state(built),
            super::step_events::persistence(completed.is_ok() && (!built || result.is_ok())),
        );
        if let Err(persistence) = completed {
            return Err(match result {
                Ok(_) => persistence,
                Err(original) => DeploymentServiceError::PersistenceAfterFailure {
                    original: Box::new(original),
                    persistence: Box::new(persistence),
                },
            });
        }
        components.push(result?);
    }
    validate_saved_plan(plan, destinations_path)?;
    Ok(components)
}

fn validate_saved_plan(
    plan: &DeploymentPlan,
    destinations_path: &std::path::Path,
) -> Result<(), DeploymentServiceError> {
    let current = crate::config::load(&plan.selection.project_root)?;
    if !matches!(current, crate::config::ProjectConfigState::Loaded(ref config) if config == &plan.selection.config)
    {
        return Err(DeploymentServiceError::StalePlan(
            "Project configuration changed or disappeared; run the environment check again".into(),
        ));
    }
    let destinations = DestinationRegistry::load(destinations_path)
        .map_err(|error| DeploymentServiceError::StalePlan(error.to_string()))?;
    for entry in &plan.entries {
        validate_destination_snapshot(
            &destinations,
            &entry.planned.context.destination,
            &entry.destination_snapshot,
        )?;
    }
    Ok(())
}

fn validate_destination_snapshot(
    destinations: &DestinationRegistry,
    key: &DestinationKey,
    expected: &crate::config::DestinationRevisionRecord,
) -> Result<(), DeploymentServiceError> {
    if destinations.resolve(key) != Some(expected) {
        return Err(DeploymentServiceError::StalePlan(format!(
            "Destination {key} changed or disappeared; run the environment check again",
        )));
    }
    Ok(())
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
    let output = super::deployment_logs::BuildOutputProjector::new(&entry.component, events);
    let build = super::build::run_build_with_events(
        project_root,
        &entry.config.working_directory,
        &entry.config.build,
        true,
        COMMAND_TIMEOUT,
        cancellation,
        &|index, stream, bytes| output.output(index, stream, bytes),
        &|index, command| {
            events.emit_record(crate::telemetry::log_record::LogEvent {
                namespace: "build.command_failed".into(),
                message: format!("Build command {} did not complete successfully", index + 1),
                scope: None,
                kind: crate::telemetry::log_record::LogEventKind::FailedCommand {
                    command: super::build::command_snapshot(index, command),
                },
            });
        },
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
        build.git_metadata.revision,
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
    ReleaseVersion::parse(format!("{timestamp}-{random}"))
        .map_err(|error| DeploymentServiceError::Version(error.to_string()))
}

#[cfg(test)]
#[path = "deployment/event_tests.rs"]
mod event_tests;

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn terminal_persistence_failure_preserves_id_original_error_and_log_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let (plan, _) = failing_build_plan(directory.path());
        let path = directory.path().join("history.sqlite3");
        let history = HistoryStore::open(&path).unwrap();
        let deployment = DeploymentOrchestrator::new(&history, Redactor::default())
            .start_for_context(&plan.entries[0].planned.context)
            .unwrap();
        let database = rusqlite::Connection::open(&path).unwrap();
        database.execute_batch("CREATE TRIGGER fail_terminal BEFORE UPDATE ON deployments BEGIN SELECT RAISE(ABORT, 'test persistence failure'); END;").unwrap();
        let error = before_remote_failure(
            &history,
            &deployment.id,
            &super::super::clock::MonotonicClock::default(),
            crate::domain::DeploymentState::Failed,
            DeploymentServiceError::InvalidSelection("original failure".into()),
            Some("log disk full".into()),
        )
        .to_string();
        assert!(error.contains(&deployment.id.to_string()));
        assert!(error.contains("original failure"));
        assert!(error.contains("test persistence failure"));
        assert!(error.contains("log disk full"));
        let state: String = database
            .query_row("SELECT state FROM deployments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            state, "running",
            "failed persistence must not claim a saved terminal state"
        );
    }

    pub(super) fn failing_build_plan(directory: &std::path::Path) -> (DeploymentPlan, PathBuf) {
        use crate::{
            config::{CredentialRegistry, DestinationSettings, HostKeyFingerprint},
            drivers::{DeploymentDriver, linux_ssh::LinuxSshDriver},
        };
        let yaml = include_str!("../../docs/examples/shipforge.yaml")
            .replace("npm", "shipforge-test-nonexistent-executable");
        std::fs::write(directory.join("shipforge.yaml"), yaml).unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory).unwrap()
        else {
            panic!("valid fixture required");
        };
        let name = ComponentName::parse("frontend").unwrap();
        let environment = &config.environments["production"];
        let target = &environment.components[&name];
        let mut destinations = DestinationRegistry::new();
        let snapshot = destinations
            .create(
                target.destination.clone(),
                DestinationSettings::LinuxSsh {
                    host: "127.0.0.1".into(),
                    port: 1,
                    user: "test".into(),
                    credential: crate::drivers::CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:test").unwrap(),
                },
            )
            .unwrap()
            .clone();
        let resolved = snapshot.resolve();
        let driver = Arc::new(LinuxSshDriver::new(Arc::new(CredentialRegistry::new())));
        let context = ComponentExecutionContext {
            project_id: config.project_id.clone(),
            environment_id: environment.id.clone(),
            component: name.clone(),
            generation: target.generation,
            destination: target.destination.clone(),
            destination_revision: snapshot.revision,
            credential: resolved.credential,
            endpoint_fingerprint: resolved.endpoint_fingerprint,
            destination_settings: driver.validate_destination(&resolved.settings).unwrap(),
            target: driver.validate_target(&target.driver_input()).unwrap(),
            cancellation: CancellationToken::new(),
        };
        let release = ComponentRelease {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: name.clone(),
            generation: context.generation,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            version: ReleaseVersion::parse("test-build-failure").unwrap(),
        };
        let entry = DeploymentPlanEntry {
            notices: Vec::new(),
            component: name.clone(),
            destination: "loopback-test".into(),
            root: target.root.clone(),
            current: None,
            release: release.version.clone(),
            config: config.components[&name].clone(),
            destination_snapshot: snapshot,
            planned: PlannedComponent {
                notices: Vec::new(),
                context,
                plan: crate::drivers::ComponentPlan {
                    release,
                    effective_capabilities: driver.static_capabilities(),
                    expected_current: None,
                    driver_steps: Vec::new(),
                },
                driver,
            },
        };
        let path = directory.join("destinations.yaml");
        destinations.save(&path).unwrap();
        (
            DeploymentPlan {
                selection: DeploymentSelection {
                    project_root: directory.to_owned(),
                    config,
                    environment: "production".into(),
                    components: BTreeSet::from([name.clone()]),
                },
                activation_order: vec![name],
                entries: vec![entry],
                git: GitWorktreeState::NotRepository,
                git_metadata: super::super::build::GitMetadata::default(),
            },
            path,
        )
    }

    #[derive(Debug)]
    struct InspectBuildIntent(PathBuf);

    #[tokio::test]
    async fn setup_history_failure_is_terminal_and_identified_before_build() {
        let directory = tempfile::tempdir().unwrap();
        let (plan, destinations) = failing_build_plan(directory.path());
        let path = directory.path().join("history.sqlite3");
        let history = HistoryStore::open(&path).unwrap();
        rusqlite::Connection::open(&path).unwrap().execute_batch(
            "CREATE TRIGGER fail_metadata BEFORE INSERT ON deployment_metadata BEGIN SELECT RAISE(ABORT,'injected metadata failure'); END;"
        ).unwrap();
        let service = DeploymentService::new(Arc::new(DriverRegistry::default()), path);
        let error = service
            .execute(
                plan,
                &destinations,
                &NoTestEvents,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        let DeploymentServiceError::Execution { deployment, .. } = error else {
            panic!("error must identify Deployment");
        };
        assert_eq!(
            history.deployment(&deployment).unwrap().unwrap().state,
            crate::domain::DeploymentState::Failed
        );
        assert!(history.pending_intents(&deployment).unwrap().is_empty());
        assert!(
            history
                .steps(&deployment)
                .unwrap()
                .iter()
                .all(|step| step.status == crate::history::StepStatus::Skipped)
        );
    }

    #[derive(Debug)]
    struct NoTestEvents;

    impl EventSink for NoTestEvents {
        fn emit(&self, _: DriverLog) {}
    }

    #[tokio::test]
    async fn package_metadata_failure_closes_build_step_without_starting_remote_work() {
        let directory = tempfile::tempdir().unwrap();
        let (mut plan, destinations) = failing_build_plan(directory.path());
        let config_path = directory.path().join("shipforge.yaml");
        let yaml = std::fs::read_to_string(&config_path)
            .unwrap()
            .replace(
                "shipforge-test-nonexistent-executable, ci",
                "rustc, --version",
            )
            .replace(
                "shipforge-test-nonexistent-executable, run, build",
                "rustc, --version",
            );
        std::fs::write(config_path, yaml).unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory.path()).unwrap()
        else {
            panic!("valid config");
        };
        plan.entries[0].config = config.components[&plan.entries[0].component].clone();
        plan.selection.config = config;
        let output = directory.path().join("frontend/dist");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(output.join("index.html"), "fixture").unwrap();
        let path = directory.path().join("history.sqlite3");
        let history = HistoryStore::open(&path).unwrap();
        rusqlite::Connection::open(&path).unwrap().execute_batch(
            "CREATE TRIGGER fail_package BEFORE INSERT ON release_packages BEGIN SELECT RAISE(ABORT,'injected package failure'); END;"
        ).unwrap();
        let service = DeploymentService::new(Arc::new(DriverRegistry::default()), path);
        let error = service
            .execute(
                plan,
                &destinations,
                &NoTestEvents,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected package failure"));
        let DeploymentServiceError::Execution { deployment, .. } = error else {
            panic!("Deployment ID required");
        };
        assert_eq!(
            history.deployment(&deployment).unwrap().unwrap().state,
            crate::domain::DeploymentState::Failed
        );
        assert!(history.pending_intents(&deployment).unwrap().is_empty());
        let steps = history.steps(&deployment).unwrap();
        assert_eq!(steps[0].name, "build-package");
        assert_eq!(steps[0].status, crate::history::StepStatus::Failed);
        assert!(
            steps[1..]
                .iter()
                .all(|step| step.status == crate::history::StepStatus::Skipped)
        );
    }

    impl EventSink for InspectBuildIntent {
        fn emit(&self, event: DriverLog) {
            if event.namespace == "build.started" {
                let connection = rusqlite::Connection::open(&self.0).unwrap();
                let count: i64 = connection.query_row("SELECT count(*) FROM operation_intents WHERE stage='build-package' AND status='pending'", [], |row| row.get(0)).unwrap();
                assert_eq!(count, 1, "intent must be durable before starting the build");
                let count: i64 = connection
                    .query_row("SELECT count(*) FROM component_snapshots", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(
                    count, 1,
                    "selected target must be durable before starting the build"
                );
                let count: i64 = connection
                    .query_row("SELECT count(*) FROM deployment_metadata", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, 1, "confirmed source context must precede the build");
            }
        }
    }

    #[tokio::test]
    async fn failing_build_has_a_durable_intent_and_terminal_deployment() {
        let directory = tempfile::tempdir().unwrap();
        let (plan, destinations) = failing_build_plan(directory.path());
        let history = directory.path().join("history.sqlite3");
        let service = DeploymentService::new(Arc::new(DriverRegistry::default()), history.clone());
        let error = service
            .execute(
                plan,
                &destinations,
                &InspectBuildIntent(history.clone()),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, DeploymentServiceError::Execution { ref source, .. }
            if matches!(**source, DeploymentServiceError::Build(_)))
        );
        let connection = rusqlite::Connection::open(history).unwrap();
        let state: String = connection
            .query_row("SELECT state FROM deployments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "failed");
        let status: String = connection
            .query_row("SELECT status FROM operation_intents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "failed");
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM operation_intents WHERE stage != 'build-package'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "build failure must not begin remote preparation");
        let log_path: String = connection
            .query_row("SELECT relative_path FROM deployment_logs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(directory.path().join(log_path).is_file());
    }

    #[tokio::test]
    async fn git_change_after_preview_is_rejected_before_creating_a_deployment() {
        let directory = tempfile::tempdir().unwrap();
        let (plan, destinations) = failing_build_plan(directory.path());
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
        let history = directory.path().join("history.sqlite3");
        let service = DeploymentService::new(Arc::new(DriverRegistry::default()), history.clone());
        let result = service
            .execute(
                plan,
                &destinations,
                &InspectBuildIntent(history.clone()),
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(DeploymentServiceError::StalePlan(_))));
        assert!(!history.exists());
    }

    #[test]
    fn logging_failure_preserves_successful_and_manual_recovery_reports() {
        use crate::domain::{
            ComponentDeploymentResult, ComponentOutcome, Deployment, DeploymentState,
        };
        for succeeded in [true, false] {
            let mut deployment = Deployment::new();
            deployment.start().unwrap();
            let name = ComponentName::parse("worker").unwrap();
            let mut compensation_failures = BTreeMap::new();
            if succeeded {
                deployment.succeed().unwrap();
            } else {
                deployment.fail().unwrap();
                deployment.components.insert(
                    name.clone(),
                    ComponentDeploymentResult {
                        outcome: ComponentOutcome::CompensationFailed,
                        attempted_release: None,
                        observed_release: None,
                    },
                );
                compensation_failures.insert(
                    name.clone(),
                    crate::drivers::DriverError {
                        recovery_blocked: false,
                        stage: "compensate".into(),
                        target: name.to_string(),
                        message: "state unknown".into(),
                        suggested_action: "inspect remote current before retrying".into(),
                    },
                );
            }
            let original = deployment.clone();
            let mut report = DeploymentReport {
                deployment,
                failure: None,
                compensation_failures,
                warnings: Vec::new(),
            };
            attach_log_failure(&mut report, Some("disk full".into()));
            assert_eq!(report.deployment, original);
            assert_eq!(report.warnings.len(), 1);
            assert!(report.warnings[0].contains("disk full"));
            if succeeded {
                assert_eq!(report.deployment.state, DeploymentState::Succeeded);
            } else {
                assert!(
                    report.compensation_failures[&name]
                        .suggested_action
                        .contains("inspect remote current")
                );
            }
        }
    }

    #[test]
    fn destination_snapshot_rejects_revisions_missing_targets_and_changed_settings() {
        use crate::config::{DestinationSettings, HostKeyFingerprint};
        let key = DestinationKey::new();
        let settings = DestinationSettings::LinuxSsh {
            host: "test.example".into(),
            port: 22,
            user: "deploy".into(),
            credential: crate::drivers::CredentialHandle::new(),
            host_key: HostKeyFingerprint::parse("SHA256:test").unwrap(),
        };
        let mut registry = DestinationRegistry::new();
        let snapshot = registry
            .create(key.clone(), settings.clone())
            .unwrap()
            .clone();
        assert!(validate_destination_snapshot(&registry, &key, &snapshot).is_ok());
        assert!(
            validate_destination_snapshot(&DestinationRegistry::new(), &key, &snapshot).is_err()
        );
        registry.revise(&key, settings).unwrap();
        assert!(validate_destination_snapshot(&registry, &key, &snapshot).is_err());
        let mut altered = snapshot.clone();
        let DestinationSettings::LinuxSsh { port, .. } = &mut altered.settings;
        *port = 2222;
        let mut same_revision = DestinationRegistry::new();
        same_revision.create(key.clone(), altered.settings).unwrap();
        assert!(validate_destination_snapshot(&same_revision, &key, &snapshot).is_err());
    }

    #[test]
    fn saved_plan_rejects_changed_or_missing_project_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let project_file = directory.path().join("shipforge.yaml");
        std::fs::write(
            &project_file,
            include_str!("../../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory.path()).unwrap()
        else {
            panic!("example must contain valid Project configuration");
        };
        let registry_path = directory.path().join("destinations.yaml");
        DestinationRegistry::new().save(&registry_path).unwrap();
        let mut plan = DeploymentPlan {
            selection: DeploymentSelection {
                project_root: directory.path().to_owned(),
                config,
                environment: "production".into(),
                components: BTreeSet::new(),
            },
            activation_order: Vec::new(),
            entries: Vec::new(),
            git: GitWorktreeState::NotRepository,
            git_metadata: super::super::build::GitMetadata::default(),
        };
        assert!(validate_saved_plan(&plan, &registry_path).is_ok());
        plan.selection.config.project = "changed-preview".into();
        assert!(matches!(
            validate_saved_plan(&plan, &registry_path),
            Err(DeploymentServiceError::StalePlan(_))
        ));
        std::fs::remove_file(project_file).unwrap();
        assert!(matches!(
            validate_saved_plan(&plan, &registry_path),
            Err(DeploymentServiceError::StalePlan(_))
        ));
    }

    #[test]
    fn release_versions_preserve_full_uuid_and_are_unique_in_a_burst() {
        let mut versions = std::collections::BTreeSet::new();
        for _ in 0..1000 {
            let version = generate_release_version().unwrap();
            let (_, suffix) = version.as_str().split_once('-').unwrap();
            assert_eq!(suffix.len(), 32);
            assert!(uuid::Uuid::parse_str(suffix).is_ok());
            assert!(versions.insert(version));
        }
    }
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
    #[error(
        "{original}; local terminal state could not be saved: {persistence}; inspect history before retrying"
    )]
    PersistenceAfterFailure {
        #[source]
        original: Box<Self>,
        persistence: Box<Self>,
    },
    #[error("Deployment {deployment}: {source}{log_diagnostic}")]
    Execution {
        deployment: crate::domain::DeploymentId,
        #[source]
        source: Box<Self>,
        log_diagnostic: String,
    },
    #[error("Deployment log could not be written: {0}; inspect deployment history before retrying")]
    Log(String),
    #[error("Deployment plan is stale: {0}")]
    StalePlan(String),
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
