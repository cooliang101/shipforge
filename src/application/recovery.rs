//! Read-only reconciliation of frozen local intentions with present remote facts.
//!
//! Inspection never completes old intentions, recreates a Project, or changes a
//! service. A later user-requested mutation must create its own durable intention.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{DestinationRegistry, DestinationRevisionRecord, ProjectConfigState, TargetConfig},
    domain::DeploymentId,
    drivers::{
        ComponentExecutionContext, ComponentInventory, DeploymentDriver, DriverRegistry,
        ReleaseRef, audit::RemoteAuditPhase,
    },
    history::{
        CurrentAlignment, DeploymentComponentSnapshot, HistoryError, HistoryStore, InspectionScope,
        PackageAlignment, RecoveryBasis, RecoveryComponentReport, RecoveryReport,
        ReleasePackageRecord,
    },
    telemetry::Redactor,
};

use super::{DeploymentSelection, DeploymentSession};

const COMPONENT_TIMEOUT: Duration = Duration::from_secs(180);
const INSPECTION_TIMEOUT: Duration = Duration::from_secs(600);
// Leave room in the 4 MiB persisted report for scopes and explicit unknowns.
const MAX_EVIDENCE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct RecoveryService {
    drivers: Arc<DriverRegistry>,
    history_path: PathBuf,
    session: Arc<DeploymentSession>,
}

/// Verified observations remain available even if their cache cannot be saved.
#[derive(Debug)]
pub struct RecoveryInspection {
    pub report: RecoveryReport,
    pub persistence_warning: Option<String>,
}

impl RecoveryInspection {
    /// Mixed target/source facts suggest partial application, not its cause/order.
    #[must_use]
    pub fn possibly_partially_applied(&self) -> bool {
        self.report
            .components
            .iter()
            .any(|entry| entry.alignment == CurrentAlignment::Target)
            && self
                .report
                .components
                .iter()
                .any(|entry| entry.alignment == CurrentAlignment::Previous)
    }
}

impl RecoveryService {
    #[must_use]
    pub fn new(
        drivers: Arc<DriverRegistry>,
        history_path: PathBuf,
        session: Arc<DeploymentSession>,
    ) -> Self {
        Self {
            drivers,
            history_path,
            session,
        }
    }

    /// Inspects user-selected Components, optionally against one local Deployment.
    ///
    /// `source == None` rebuilds only an observation cache, never deployment history.
    /// No build programs, Git commands, preflight, or remote mutation are executed.
    ///
    /// # Errors
    /// Returns an error before observation for a busy session, invalid/missing
    /// saved configuration, corrupt history, or missing frozen source context.
    /// Remote errors are explicit unknown Component observations in the report.
    pub async fn inspect(
        &self,
        selection: DeploymentSelection,
        source: Option<DeploymentId>,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RecoveryInspection, RecoveryError> {
        self.session
            .run(self.inspect_exclusive(selection, source, destinations_path, cancellation))
            .await
            .map_err(|_| RecoveryError::Busy)?
    }

    async fn inspect_exclusive(
        &self,
        mut selection: DeploymentSelection,
        source: Option<DeploymentId>,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RecoveryInspection, RecoveryError> {
        selection.project_root = selection
            .project_root
            .canonicalize()
            .map_err(|_| RecoveryError::Configuration("Project directory is unavailable"))?;
        validate_selection(&selection)?;
        if cancellation.is_cancelled() {
            return Err(RecoveryError::Cancelled);
        }
        let destinations = DestinationRegistry::load(destinations_path).map_err(|_| {
            RecoveryError::Configuration("Destination registry cannot be validated")
        })?;
        let basis = self.load_basis(source.clone()).await?;
        validate_basis(&selection, basis.as_ref())?;
        let started_at_ms = timestamp()?;
        let deadline = Instant::now() + INSPECTION_TIMEOUT;
        let targets =
            self.inspection_targets(&selection, basis.as_ref(), &destinations, cancellation)?;
        let redactor = inspection_redactor(&targets);
        let mut report = RecoveryReport {
            id: uuid::Uuid::now_v7(),
            related_deployment: source,
            source_revision: basis.as_ref().map(|basis| basis.revision),
            started_at_ms,
            completed_at_ms: started_at_ms,
            components: Vec::new(),
        };
        let mut evidence_bytes = 0;
        for target in &targets {
            let inventory = match &target.execution {
                Err(reason) => Err((*reason).to_owned()),
                Ok(execution) => {
                    match validate_saved_context(&selection, destinations_path, &targets) {
                        Err(_) => Err(
                            "Saved configuration changed; this Component was not inspected".into(),
                        ),
                        Ok(()) if evidence_bytes >= MAX_EVIDENCE_BYTES => Err(
                            "Inspection evidence limit reached; this Component was not inspected"
                                .into(),
                        ),
                        Ok(()) => observe(execution, cancellation, deadline).await,
                    }
                }
            };
            let snapshot = basis.as_ref().and_then(|basis| {
                basis
                    .snapshots
                    .iter()
                    .find(|entry| entry.release.component == target.scope.component)
            });
            let package = basis.as_ref().and_then(|basis| {
                basis
                    .packages
                    .iter()
                    .find(|entry| entry.release.component == target.scope.component)
            });
            let mut component = reconcile(target.scope.clone(), inventory, snapshot, package);
            component.sanitize(&redactor);
            // An invalid Driver response is unknown, not a partially trusted cache.
            if component.validate().is_err() {
                component.inventory =
                    Err("Remote inventory violated the inspection contract".into());
                component.alignment = CurrentAlignment::Unknown;
                component.package_alignment = PackageAlignment::Unknown;
                component.notices.clear();
            }
            let bytes = serde_json::to_vec(&component)
                .map_err(|_| RecoveryError::Worker)?
                .len();
            if component.inventory.is_ok()
                && evidence_bytes.saturating_add(bytes) > MAX_EVIDENCE_BYTES
            {
                component.inventory = Err("Component evidence exceeds the remaining inspection limit; inspect fewer Components separately".into());
                component.alignment = CurrentAlignment::Unknown;
                component.package_alignment = PackageAlignment::Unknown;
                component.notices.clear();
                evidence_bytes = MAX_EVIDENCE_BYTES;
            } else {
                evidence_bytes = evidence_bytes.saturating_add(bytes).min(MAX_EVIDENCE_BYTES);
            }
            report.components.push(component);
        }
        report.completed_at_ms = timestamp()?.max(started_at_ms);
        report.sanitize(&redactor);
        report.validate()?;
        let persistence_warning = match validate_saved_context(&selection, destinations_path, &targets) {
            Ok(()) => self.persist(&report, redactor).await.err().map(|error| match error {
                RecoveryError::History(HistoryError::StaleRecoveryBasis) =>
                    "Local history changed during inspection; observations were not cached. Inspect again.".into(),
                _ => "Observations could not be saved; original deployment history is unchanged.".into(),
            }),
            Err(_) => Some("Saved configuration changed during inspection; observations were not cached. Inspect again.".into()),
        };
        Ok(RecoveryInspection {
            report,
            persistence_warning,
        })
    }

    async fn load_basis(
        &self,
        source: Option<DeploymentId>,
    ) -> Result<Option<RecoveryBasis>, RecoveryError> {
        let path = self.history_path.clone();
        tokio::task::spawn_blocking(move || {
            // Only an absent file is a fresh cache. Do not initialize an existing
            // empty/schema-0 database or silently replace unreadable history.
            let attention = HistoryStore::local_attention(&path, None, None)?;
            if source.is_some() && attention.database_missing {
                return Err(RecoveryError::MissingSource);
            }
            let history = HistoryStore::open(&path)?;
            source
                .map(|id| history.recovery_basis(&id))
                .transpose()
                .map_err(RecoveryError::History)
        })
        .await
        .map_err(|_| RecoveryError::Worker)?
    }

    async fn persist(
        &self,
        report: &RecoveryReport,
        redactor: Redactor,
    ) -> Result<(), RecoveryError> {
        let path = self.history_path.clone();
        let report = report.clone();
        tokio::task::spawn_blocking(move || {
            let attention = HistoryStore::local_attention(&path, None, None)?;
            if report.related_deployment.is_some() && attention.database_missing {
                return Err(HistoryError::StaleRecoveryBasis);
            }
            HistoryStore::open(&path)?.append_recovery_report(&report, &redactor)
        })
        .await
        .map_err(|_| RecoveryError::Worker)?
        .map_err(RecoveryError::History)
    }

    fn inspection_targets(
        &self,
        selection: &DeploymentSelection,
        basis: Option<&RecoveryBasis>,
        destinations: &DestinationRegistry,
        cancellation: &CancellationToken,
    ) -> Result<Vec<InspectionTarget>, RecoveryError> {
        let environment = &selection.config.environments[&selection.environment];
        selection
            .components
            .iter()
            .map(|component| {
                let target = &environment.components[component];
                let frozen = basis.and_then(|basis| {
                    basis
                        .snapshots
                        .iter()
                        .find(|entry| &entry.release.component == component)
                });
                let destination = match frozen {
                    Some(snapshot) => destinations.resolve_revision(
                        &snapshot.release.destination,
                        snapshot.release.destination_revision,
                    ),
                    None => destinations.resolve(&target.destination),
                };
                let scope = if let Some(snapshot) = frozen {
                    scope_from_release(&snapshot.release)
                } else {
                    let destination = destination.ok_or(RecoveryError::Configuration(
                        "A selected Destination is unavailable",
                    ))?;
                    InspectionScope {
                        project: selection.config.project_id.clone(),
                        environment: environment.id.clone(),
                        component: component.clone(),
                        generation: target.generation,
                        driver: destination.settings.driver_kind(),
                        destination: target.destination.clone(),
                        destination_revision: destination.revision,
                        endpoint_fingerprint: destination.endpoint_fingerprint.clone(),
                    }
                };
                let execution = self.execution_context(&scope, target, destination, cancellation);
                Ok(InspectionTarget {
                    scope,
                    destination: destination.cloned(),
                    require_latest: frozen.is_none(),
                    execution,
                })
            })
            .collect()
    }

    fn execution_context(
        &self,
        scope: &InspectionScope,
        target: &TargetConfig,
        destination: Option<&DestinationRevisionRecord>,
        cancellation: &CancellationToken,
    ) -> Result<InspectionExecution, &'static str> {
        if target.generation != scope.generation || target.destination != scope.destination {
            return Err(
                "Component configuration differs from the frozen context; old target settings are unavailable",
            );
        }
        let destination = destination
            .ok_or("Frozen Destination revision is unavailable; no fallback was used")?;
        let resolved = destination.resolve();
        if resolved.driver != scope.driver
            || resolved.endpoint_fingerprint != scope.endpoint_fingerprint
        {
            return Err("Destination no longer matches the frozen endpoint; no fallback was used");
        }
        let driver = self
            .drivers
            .get(&scope.driver)
            .ok_or("Inspection is unavailable for this Destination type")?;
        let destination_settings = driver
            .validate_destination(&resolved.settings)
            .map_err(|_| "Destination settings could not be validated")?;
        let target_settings = driver
            .validate_target(&target.driver_input())
            .map_err(|_| "Component target could not be validated")?;
        let context = ComponentExecutionContext {
            project_id: scope.project.clone(),
            environment_id: scope.environment.clone(),
            component: scope.component.clone(),
            generation: scope.generation,
            destination: scope.destination.clone(),
            destination_revision: scope.destination_revision,
            credential: resolved.credential,
            endpoint_fingerprint: scope.endpoint_fingerprint.clone(),
            destination_settings,
            target: target_settings,
            cancellation: cancellation.child_token(),
        };
        Ok(InspectionExecution { driver, context })
    }
}

#[derive(Debug)]
struct InspectionTarget {
    scope: InspectionScope,
    destination: Option<DestinationRevisionRecord>,
    require_latest: bool,
    execution: Result<InspectionExecution, &'static str>,
}

#[derive(Debug)]
struct InspectionExecution {
    driver: Arc<dyn DeploymentDriver>,
    context: ComponentExecutionContext,
}

async fn observe(
    execution: &InspectionExecution,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<ComponentInventory, String> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err("Inspection time limit reached; Component state is unknown".into());
    };
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err("Inspection cancelled; Component state is unknown".into()),
        result = tokio::time::timeout(remaining.min(COMPONENT_TIMEOUT), execution.driver.inventory(&execution.context)) => {
            match result {
                Ok(Ok(inventory)) => Ok(inventory),
                // Raw Driver diagnostics may include connection settings. The
                // common report deliberately does not copy arbitrary Driver text.
                Ok(Err(_)) => Err("Remote inventory could not be verified; check the connection and target, then inspect again".into()),
                Err(_) => Err("Remote inventory timed out; Component state is unknown".into()),
            }
        }
    };
    execution.context.cancellation.cancel();
    result
}

fn validate_selection(selection: &DeploymentSelection) -> Result<(), RecoveryError> {
    if selection.components.is_empty() || selection.components.len() > 256 {
        return Err(RecoveryError::Configuration(
            "Select between 1 and 256 Components",
        ));
    }
    if !matches!(crate::config::load(&selection.project_root),
        Ok(ProjectConfigState::Loaded(ref project)) if project == &selection.config)
    {
        return Err(RecoveryError::Configuration(
            "Project YAML is missing, invalid, or changed; it cannot be recovered from remote state",
        ));
    }
    let environment = selection
        .config
        .environments
        .get(&selection.environment)
        .ok_or(RecoveryError::Configuration(
            "Selected Environment is unavailable",
        ))?;
    if selection
        .components
        .iter()
        .any(|component| !environment.components.contains_key(component))
    {
        return Err(RecoveryError::Configuration(
            "Selected Component is not configured in this Environment",
        ));
    }
    Ok(())
}

fn validate_basis(
    selection: &DeploymentSelection,
    basis: Option<&RecoveryBasis>,
) -> Result<(), RecoveryError> {
    let Some(basis) = basis else {
        return Ok(());
    };
    if basis.record.project != selection.config.project_id
        || basis.record.environment != selection.config.environments[&selection.environment].id
    {
        return Err(RecoveryError::Configuration(
            "Local Deployment belongs to another Project or Environment",
        ));
    }
    let snapshots: BTreeMap<_, _> = basis
        .snapshots
        .iter()
        .map(|snapshot| (&snapshot.release.component, snapshot))
        .collect();
    if selection
        .components
        .iter()
        .any(|component| !snapshots.contains_key(component))
    {
        return Err(RecoveryError::Configuration(
            "Frozen Component context is missing; inspect the configured target separately without reconstructing old history",
        ));
    }
    Ok(())
}

fn validate_saved_context(
    selection: &DeploymentSelection,
    path: &Path,
    targets: &[InspectionTarget],
) -> Result<(), RecoveryError> {
    validate_selection(selection)?;
    let registry = DestinationRegistry::load(path)
        .map_err(|_| RecoveryError::Configuration("Destination registry cannot be validated"))?;
    for target in targets {
        let actual = if target.require_latest {
            registry.resolve(&target.scope.destination)
        } else {
            registry.resolve_revision(&target.scope.destination, target.scope.destination_revision)
        };
        if actual != target.destination.as_ref() {
            return Err(RecoveryError::Configuration(
                "A selected Destination revision changed during inspection",
            ));
        }
    }
    Ok(())
}

fn scope_from_release(release: &ReleaseRef) -> InspectionScope {
    InspectionScope {
        project: release.project_id.clone(),
        environment: release.environment_id.clone(),
        component: release.component.clone(),
        generation: release.generation,
        driver: release.driver.clone(),
        destination: release.destination.clone(),
        destination_revision: release.destination_revision,
        endpoint_fingerprint: release.endpoint_fingerprint.clone(),
    }
}

fn reconcile(
    scope: InspectionScope,
    inventory: Result<ComponentInventory, String>,
    snapshot: Option<&DeploymentComponentSnapshot>,
    package: Option<&ReleasePackageRecord>,
) -> RecoveryComponentReport {
    let mut notices = vec!["Current version does not establish service health or completion of a historical operation.".into()];
    let alignment = match (&inventory, snapshot) {
        (Ok(inventory), Some(snapshot)) => match &inventory.releases.current {
            Ok(current)
                if current.as_ref() == snapshot.target.as_ref().map(|release| &release.version) =>
            {
                CurrentAlignment::Target
            }
            Ok(current)
                if current.as_ref()
                    == snapshot
                        .expected_current
                        .as_ref()
                        .map(|release| &release.version) =>
            {
                CurrentAlignment::Previous
            }
            Ok(_) => CurrentAlignment::Other,
            Err(_) => CurrentAlignment::Unknown,
        },
        (Ok(_), None) => CurrentAlignment::Unplanned,
        (Err(_), _) => CurrentAlignment::Unknown,
    };
    let package_alignment = package_alignment(&inventory, snapshot, package, &mut notices);
    RecoveryComponentReport {
        scope,
        inventory,
        alignment,
        package_alignment,
        notices,
    }
}

fn package_alignment(
    inventory: &Result<ComponentInventory, String>,
    snapshot: Option<&DeploymentComponentSnapshot>,
    package: Option<&ReleasePackageRecord>,
    notices: &mut Vec<String>,
) -> PackageAlignment {
    let Ok(inventory) = inventory else {
        return PackageAlignment::Unknown;
    };
    let Some(snapshot) = snapshot else {
        return PackageAlignment::Unplanned;
    };
    let Some(target) = &snapshot.target else {
        return PackageAlignment::Unplanned;
    };
    let Some(actual) = inventory
        .releases
        .releases
        .iter()
        .find(|entry| entry.manifest.version == target.version)
    else {
        return if inventory
            .releases
            .issues
            .iter()
            .any(|issue| issue.version.as_ref() == Some(&target.version))
        {
            notices.push(
                "Target archive is incomplete or invalid; absence is not established.".into(),
            );
            PackageAlignment::Unknown
        } else {
            PackageAlignment::Missing
        };
    };
    let conflicting_audit = inventory
        .audit
        .records
        .iter()
        .filter(|record| record.phase == RemoteAuditPhase::Prepare && record.release == *target)
        .filter_map(|record| record.package.as_ref())
        .any(|audit| {
            audit.manifest != actual.manifest
                || audit.sha256 != actual.sha256
                || audit.size != actual.size
        });
    if conflicting_audit {
        notices.push("Archive differs from historical prepare audit evidence; do not reuse it without investigation.".into());
        return PackageAlignment::Mismatch;
    }
    let Some(expected) = package.filter(|expected| expected.release == *target) else {
        notices.push("No matching local package receipt exists; archive identity alone cannot prove the frozen build output.".into());
        return PackageAlignment::Unknown;
    };
    if expected.manifest != actual.manifest
        || expected.sha256 != actual.sha256
        || expected.size != actual.size
    {
        PackageAlignment::Mismatch
    } else if actual.extracted {
        PackageAlignment::Matches
    } else {
        PackageAlignment::ArchiveOnly
    }
}

fn inspection_redactor(targets: &[InspectionTarget]) -> Redactor {
    fn strings(value: &serde_json::Value, values: &mut Vec<String>) {
        match value {
            serde_json::Value::String(value) => values.push(value.clone()),
            serde_json::Value::Array(items) => items.iter().for_each(|item| strings(item, values)),
            serde_json::Value::Object(items) => {
                items.values().for_each(|item| strings(item, values));
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    for target in targets {
        if let Some(destination) = &target.destination {
            let resolved = destination.resolve();
            strings(&resolved.settings.value, &mut values);
            values.push(resolved.credential.expose_reference().to_owned());
        }
    }
    Redactor::new(values)
}

fn timestamp() -> Result<u64, RecoveryError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .ok_or(RecoveryError::Clock)
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("This TUI session already has an active operation")]
    Busy,
    #[error("Inspection was cancelled")]
    Cancelled,
    #[error("{0}")]
    Configuration(&'static str),
    #[error("Local history is unavailable")]
    HistoryUnavailable,
    #[error(
        "The source Deployment is absent; inspect configured targets without reconstructing history"
    )]
    MissingSource,
    #[error("The inspection worker did not complete")]
    Worker,
    #[error("The system clock cannot timestamp the inspection")]
    Clock,
    #[error(transparent)]
    History(#[from] HistoryError),
}

#[cfg(test)]
mod tests;
