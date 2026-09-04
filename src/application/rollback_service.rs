//! Explicit historical rollback previews, never inferred from an inventory listing.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{DestinationRegistry, DestinationRevisionRecord, ProjectConfigState},
    domain::{Capability, ComponentName, DeploymentId, DestinationKey, DestinationRevision},
    drivers::{
        ComponentExecutionContext, DeploymentDriver, DriverError, DriverRegistry, ReleaseRef,
    },
    history::{
        CurrentAlignment, HistoryError, HistoryStore, InspectionScope, PackageAlignment,
        RecoveryBasis, RecoveryComponentReport, ReleasePackageRecord, RetentionHistory,
    },
    telemetry::Redactor,
};

use super::{
    DeploymentSelection, DeploymentSession, OrchestrationError, RollbackComponent,
    RollbackOrchestrator, RollbackReport, execution_guard::ExecutionGuard,
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(180);
const PLAN_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_OPTIONS: usize = 100;
const MAX_EVIDENCE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RollbackService {
    drivers: Arc<DriverRegistry>,
    history_path: PathBuf,
    session: Arc<DeploymentSession>,
}

#[derive(Clone, Debug)]
pub struct RollbackCandidates {
    pub source: DeploymentId,
    pub components: Vec<RollbackComponentCandidates>,
}

#[derive(Clone, Debug)]
pub struct RollbackComponentCandidates {
    pub component: ComponentName,
    /// Explicitly identifies the frozen endpoint/revision, including old endpoints.
    pub destination: String,
    pub root: String,
    pub options: Vec<RollbackOption>,
    pub unavailable: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RollbackOption {
    pub target: Option<ReleaseRef>,
    pub unavailable: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackPreviewEntry {
    pub component: ComponentName,
    pub destination: String,
    pub root: String,
    pub current: ReleaseRef,
    pub target: Option<ReleaseRef>,
}

/// Only this service can create or alter the execution proof. UI summaries cannot
/// be edited into an executable plan or rebound to another history database.
#[derive(Clone, Debug)]
pub struct RollbackPlan {
    selection: DeploymentSelection,
    source: DeploymentId,
    entries: Vec<RollbackPreviewEntry>,
    activation_order: Vec<ComponentName>,
    execution_order: Vec<ComponentName>,
    evidence: Evidence,
    contexts: Vec<PlannedContext>,
    history_path: PathBuf,
    registry_path: PathBuf,
    latest: BTreeMap<DestinationKey, DestinationRevisionRecord>,
    used: BTreeMap<(DestinationKey, DestinationRevision), DestinationRevisionRecord>,
}

impl RollbackPlan {
    #[must_use]
    pub fn source(&self) -> &DeploymentId {
        &self.source
    }

    #[must_use]
    pub fn entries(&self) -> &[RollbackPreviewEntry] {
        &self.entries
    }

    #[must_use]
    pub fn execution_order(&self) -> &[ComponentName] {
        &self.execution_order
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Evidence {
    source: RecoveryBasis,
    histories: BTreeMap<ComponentName, RetentionHistory>,
}

#[derive(Clone, Debug)]
struct PlannedContext {
    driver: Arc<dyn DeploymentDriver>,
    context: ComponentExecutionContext,
    used: DestinationRevisionRecord,
    latest: DestinationRevisionRecord,
}

#[derive(Debug, Error)]
pub enum RollbackServiceError {
    #[error("another operation is active in this TUI session")]
    Busy,
    #[error("rollback check was cancelled")]
    Cancelled,
    #[error("rollback check exceeded its time limit; no rollback was started")]
    Timeout,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("rollback preview changed; check and confirm again")]
    StalePlan,
    #[error("Component {component}: {message}")]
    Component {
        component: ComponentName,
        message: &'static str,
    },
    #[error(
        "Component {component}: {operation} failed; remote state is unconfirmed; inspect again before rollback"
    )]
    Remote {
        component: ComponentName,
        operation: &'static str,
        #[source]
        source: Box<DriverError>,
    },
    #[error("rollback history is unavailable: {0}")]
    History(#[from] HistoryError),
    #[error("rollback history worker failed")]
    Worker,
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}

impl RollbackService {
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

    /// Lists bounded local evidence only. Unavailable historical targets retain
    /// their original references and an explicit reason; no SSH is initiated.
    ///
    /// # Errors
    /// Rejects a busy session, invalid YAML/source, cancellation or unreadable history.
    pub async fn candidates(
        &self,
        mut selection: DeploymentSelection,
        source: DeploymentId,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RollbackCandidates, RollbackServiceError> {
        self.session
            .run(async {
                normalize_selection(&mut selection)?;
                cancelled(cancellation)?;
                let registry = load_registry(destinations_path)?;
                let evidence = self.load_evidence(&selection, &source).await?;
                let components = selection
                    .components
                    .iter()
                    .map(|name| {
                        self.component_candidates(
                            &selection,
                            name,
                            &registry,
                            &evidence,
                            cancellation,
                        )
                    })
                    .collect();
                cancelled(cancellation)?;
                validate_selection(&selection)?;
                Ok(RollbackCandidates { source, components })
            })
            .await
            .map_err(|_| RollbackServiceError::Busy)?
    }

    /// Freezes a read-only preview for explicitly selected Component targets.
    /// The target-map keys must exactly equal the selected Component subset.
    ///
    /// # Errors
    /// Rejects unsupported, unproven, missing, changed or unhealthy-history targets.
    pub async fn plan(
        &self,
        selection: DeploymentSelection,
        source: DeploymentId,
        targets: BTreeMap<ComponentName, Option<ReleaseRef>>,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RollbackPlan, RollbackServiceError> {
        self.session.run(async {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(RollbackServiceError::Cancelled),
                result = tokio::time::timeout(PLAN_TIMEOUT,
                    self.plan_exclusive(selection, source, targets, destinations_path, cancellation)) =>
                    result.map_err(|_| RollbackServiceError::Timeout)?,
            }
        }).await.map_err(|_| RollbackServiceError::Busy)?
    }

    /// Executes one confirmed plan as a new, source-linked Rollback Deployment.
    /// No build, prepare, old-intent replay or history repair is performed.
    ///
    /// # Errors
    /// Pre-effect drift rejects execution. Once orchestration begins its durable
    /// result, cancellation and compensation semantics are returned unchanged.
    pub async fn execute(
        &self,
        plan: RollbackPlan,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RollbackReport, RollbackServiceError> {
        self.session
            .run(self.execute_exclusive(plan, destinations_path, cancellation))
            .await
            .map_err(|_| RollbackServiceError::Busy)?
    }

    async fn load_evidence(
        &self,
        selection: &DeploymentSelection,
        source: &DeploymentId,
    ) -> Result<Evidence, RollbackServiceError> {
        let path = self.history_path.clone();
        let selection = selection.clone();
        let source = source.clone();
        tokio::task::spawn_blocking(move || read_evidence(&path, &selection, &source))
            .await
            .map_err(|_| RollbackServiceError::Worker)?
    }

    fn component_candidates(
        &self,
        selection: &DeploymentSelection,
        name: &ComponentName,
        registry: &DestinationRegistry,
        evidence: &Evidence,
        cancellation: &CancellationToken,
    ) -> RollbackComponentCandidates {
        let snapshot = snapshot(evidence, name);
        let scope = InspectionScope::from(&snapshot.release);
        let target = &selection.config.environments[&selection.environment].components[name];
        let context = self.context(selection, &scope, registry, cancellation);
        let destination = registry
            .resolve_revision(&scope.destination, scope.destination_revision)
            .map_or_else(
                || {
                    format!(
                        "{} revision {} (unavailable)",
                        scope.destination,
                        scope.destination_revision.get()
                    )
                },
                |record| endpoint_label(&scope.destination, record),
            );
        RollbackComponentCandidates {
            component: name.clone(),
            destination,
            root: target.root.clone(),
            options: options(evidence, name),
            unavailable: context.err().map(|error| error.to_string()),
        }
    }

    fn context(
        &self,
        selection: &DeploymentSelection,
        scope: &InspectionScope,
        registry: &DestinationRegistry,
        cancellation: &CancellationToken,
    ) -> Result<PlannedContext, RollbackServiceError> {
        let name = &scope.component;
        let target = &selection.config.environments[&selection.environment].components[name];
        if target.generation != scope.generation || target.destination != scope.destination {
            return Err(component_error(
                name,
                "YAML target differs from the frozen generation or Destination",
            ));
        }
        let used = registry
            .resolve_revision(&scope.destination, scope.destination_revision)
            .ok_or_else(|| {
                component_error(
                    name,
                    "the exact historical Destination revision is unavailable",
                )
            })?;
        let latest = registry
            .resolve(&scope.destination)
            .ok_or_else(|| component_error(name, "Destination is unavailable"))?;
        let resolved = used.resolve();
        if resolved.driver != scope.driver
            || resolved.endpoint_fingerprint != scope.endpoint_fingerprint
        {
            return Err(component_error(
                name,
                "historical endpoint identity differs; no fallback is permitted",
            ));
        }
        let driver = self
            .drivers
            .get(&scope.driver)
            .ok_or_else(|| component_error(name, "Driver is unavailable"))?;
        let capabilities = driver.static_capabilities();
        if !required_capabilities()
            .iter()
            .all(|capability| capabilities.contains(*capability))
        {
            return Err(component_error(
                name,
                "Driver lacks rollback, observation or inventory capability",
            ));
        }
        let destination_settings = driver
            .validate_destination(&resolved.settings)
            .map_err(|_| component_error(name, "Destination settings are invalid"))?;
        let target_settings = driver
            .validate_target(&target.driver_input())
            .map_err(|_| component_error(name, "Component target settings are invalid"))?;
        Ok(PlannedContext {
            driver,
            context: ComponentExecutionContext {
                project_id: scope.project.clone(),
                environment_id: scope.environment.clone(),
                component: name.clone(),
                generation: scope.generation,
                destination: scope.destination.clone(),
                destination_revision: scope.destination_revision,
                credential: resolved.credential,
                endpoint_fingerprint: scope.endpoint_fingerprint.clone(),
                destination_settings,
                target: target_settings,
                cancellation: cancellation.clone(),
            },
            used: used.clone(),
            latest: latest.clone(),
        })
    }

    async fn plan_exclusive(
        &self,
        mut selection: DeploymentSelection,
        source: DeploymentId,
        targets: BTreeMap<ComponentName, Option<ReleaseRef>>,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RollbackPlan, RollbackServiceError> {
        normalize_selection(&mut selection)?;
        if targets.keys().cloned().collect::<BTreeSet<_>>() != selection.components {
            return Err(RollbackServiceError::Invalid(
                "targets must match the selected Component subset exactly",
            ));
        }
        let registry_path = destinations_path
            .canonicalize()
            .map_err(|_| RollbackServiceError::StalePlan)?;
        let registry = load_registry(&registry_path)?;
        let evidence = self.load_evidence(&selection, &source).await?;
        // Undo the source's actual frozen execution order, not a newly edited
        // YAML dependency order or alphabetical inventory listing.
        let mut snapshots = evidence
            .source
            .snapshots
            .iter()
            .filter(|snapshot| selection.components.contains(&snapshot.release.component))
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.execution_order);
        let activation_order: Vec<_> = snapshots
            .iter()
            .map(|snapshot| snapshot.release.component.clone())
            .collect();
        let mut plan = RollbackPlan {
            selection,
            source,
            entries: Vec::new(),
            execution_order: activation_order.iter().rev().cloned().collect(),
            activation_order,
            evidence,
            contexts: Vec::new(),
            history_path: self.history_path.clone(),
            registry_path,
            latest: BTreeMap::new(),
            used: BTreeMap::new(),
        };
        for name in &plan.activation_order {
            cancelled(cancellation)?;
            let chosen = &targets[name];
            if !options(&plan.evidence, name)
                .iter()
                .any(|option| option.unavailable.is_none() && &option.target == chosen)
            {
                return Err(component_error(
                    name,
                    "target is not supported by exact frozen package and health evidence",
                ));
            }
            let scope = InspectionScope::from(&snapshot(&plan.evidence, name).release);
            let context = self.context(&plan.selection, &scope, &registry, cancellation)?;
            let current = check_remote(
                &context,
                chosen.as_ref(),
                &plan.evidence.histories[name],
                cancellation,
            )
            .await?;
            if chosen.as_ref() == Some(&current) {
                return Err(component_error(
                    name,
                    "target is already current; deselect this Component",
                ));
            }
            let entry = RollbackPreviewEntry {
                component: name.clone(),
                destination: endpoint_label(&scope.destination, &context.used),
                root: plan.selection.config.environments[&plan.selection.environment].components
                    [name]
                    .root
                    .clone(),
                current,
                target: chosen.clone(),
            };
            plan.latest
                .insert(scope.destination.clone(), context.latest.clone());
            plan.used.insert(
                (scope.destination, scope.destination_revision),
                context.used.clone(),
            );
            plan.entries.push(entry);
            plan.contexts.push(context);
        }
        self.recheck_local(&plan, destinations_path).await?;
        Ok(plan)
    }

    async fn recheck_local(
        &self,
        plan: &RollbackPlan,
        path: &Path,
    ) -> Result<(), RollbackServiceError> {
        if plan.history_path != self.history_path
            || path.canonicalize().ok().as_ref() != Some(&plan.registry_path)
        {
            return Err(RollbackServiceError::StalePlan);
        }
        validate_selection(&plan.selection)?;
        let registry = load_registry(path)?;
        if plan
            .latest
            .iter()
            .any(|(key, record)| registry.resolve(key) != Some(record))
            || plan.used.iter().any(|((key, revision), record)| {
                registry.resolve_revision(key, *revision) != Some(record)
            })
            || self.load_evidence(&plan.selection, &plan.source).await? != plan.evidence
        {
            return Err(RollbackServiceError::StalePlan);
        }
        Ok(())
    }

    async fn execute_exclusive(
        &self,
        plan: RollbackPlan,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<RollbackReport, RollbackServiceError> {
        cancelled(cancellation)?;
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(RollbackServiceError::Cancelled),
            result = tokio::time::timeout(PLAN_TIMEOUT, self.recheck_execution(&plan, destinations_path, cancellation)) =>
                result.map_err(|_| RollbackServiceError::Timeout)??,
        }
        cancelled(cancellation)?;
        let guard = Arc::new(ExecutionGuard::new_historical(
            plan.selection.project_root.clone(),
            plan.selection.config.clone(),
            plan.registry_path.clone(),
            plan.latest,
            plan.used,
        ));
        let components = plan
            .contexts
            .into_iter()
            .zip(plan.entries)
            .map(|(mut execution, entry)| {
                execution.context.cancellation = cancellation.clone();
                RollbackComponent {
                    driver: guard.wrap(execution.driver, execution.context.clone()),
                    context: execution.context,
                    expected_current: entry.current,
                    target: entry.target,
                }
            })
            .collect();
        // A confirmed execution may write a new linked Deployment. Preview never does.
        // No outer timeout may drop the orchestrator after its first side effect.
        let history = HistoryStore::open(&self.history_path)?;
        Ok(RollbackOrchestrator::new(&history, Redactor::default())
            .rollback(
                &plan.source,
                components,
                &plan.activation_order,
                cancellation,
            )
            .await?)
    }

    async fn recheck_execution(
        &self,
        plan: &RollbackPlan,
        destinations_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), RollbackServiceError> {
        self.recheck_local(plan, destinations_path).await?;
        for (context, entry) in plan.contexts.iter().zip(&plan.entries) {
            let mut context = context.clone();
            context.context.cancellation = cancellation.clone();
            let observed = check_remote(
                &context,
                entry.target.as_ref(),
                &plan.evidence.histories[&entry.component],
                cancellation,
            )
            .await?;
            if observed != entry.current {
                return Err(RollbackServiceError::StalePlan);
            }
        }
        self.recheck_local(plan, destinations_path).await
    }
}

fn read_evidence(
    path: &Path,
    selection: &DeploymentSelection,
    source: &DeploymentId,
) -> Result<Evidence, RollbackServiceError> {
    let history = HistoryStore::open_existing_read_only(path)?;
    let basis = history.recovery_basis(source)?;
    let environment = &selection.config.environments[&selection.environment];
    if basis.record.project != selection.config.project_id
        || basis.record.environment != environment.id
        || selection.components.iter().any(|name| {
            !basis
                .snapshots
                .iter()
                .any(|snapshot| &snapshot.release.component == name)
        })
    {
        return Err(RollbackServiceError::Invalid(
            "source Deployment has no matching frozen Project/Environment/Component selection",
        ));
    }
    let mut histories = BTreeMap::new();
    let mut bytes = 0;
    for name in &selection.components {
        let reference = &basis
            .snapshots
            .iter()
            .find(|entry| &entry.release.component == name)
            .ok_or(RollbackServiceError::Invalid("missing frozen Component"))?
            .release;
        let evidence = history.retention_history(&InspectionScope::from(reference))?;
        for package in &evidence.packages {
            bytes += serde_json::to_vec(&(
                &package.release,
                &package.manifest,
                &package.sha256,
                package.size,
            ))
            .map_err(|_| RollbackServiceError::Worker)?
            .len();
        }
        for observation in &evidence.healthy {
            bytes += serde_json::to_vec(&(
                &observation.component,
                &observation.stage,
                &observation.observed,
                observation.healthy,
                observation.observed_at_ms,
            ))
            .map_err(|_| RollbackServiceError::Worker)?
            .len();
        }
        if bytes > MAX_EVIDENCE_BYTES || evidence.packages.len() > MAX_OPTIONS {
            return Err(RollbackServiceError::Invalid(
                "rollback evidence exceeds its bounded window; select fewer Components",
            ));
        }
        histories.insert(name.clone(), evidence);
    }
    Ok(Evidence {
        source: basis,
        histories,
    })
}

fn snapshot<'a>(
    evidence: &'a Evidence,
    name: &ComponentName,
) -> &'a crate::history::DeploymentComponentSnapshot {
    evidence
        .source
        .snapshots
        .iter()
        .find(|entry| &entry.release.component == name)
        .expect("read_evidence verified selected snapshot membership")
}

fn options(evidence: &Evidence, name: &ComponentName) -> Vec<RollbackOption> {
    let snapshot = snapshot(evidence, name);
    let scope = InspectionScope::from(&snapshot.release);
    let history = &evidence.histories[name];
    let mut options = Vec::new();
    for package in &history.packages {
        if options
            .iter()
            .any(|option: &RollbackOption| option.target.as_ref() == Some(&package.release))
        {
            continue;
        }
        let reason = if InspectionScope::from(&package.release) != scope {
            Some(
                "target uses another historical revision/endpoint; the rollback SPI requires current and target in one exact scope",
            )
        } else if !package
            .release
            .effective_capabilities
            .contains(Capability::Rollback)
        {
            Some("historical Release did not record rollback capability")
        } else if !history.healthy.iter().any(|observation| {
            observation.healthy == Some(true)
                && observation.observed.as_ref().ok().and_then(Option::as_ref)
                    == Some(&package.release)
        }) {
            Some("no positive health observation exists for this exact historical Release")
        } else {
            None
        };
        options.push(RollbackOption {
            target: Some(package.release.clone()),
            unavailable: reason.map(str::to_owned),
        });
    }
    for reference in snapshot
        .target
        .iter()
        .chain(snapshot.expected_current.iter())
    {
        if !options
            .iter()
            .any(|option| option.target.as_ref() == Some(reference))
        {
            options.push(RollbackOption {
                target: Some(reference.clone()),
                unavailable: Some("original package evidence is unavailable; an inventory listing cannot reconstruct it".into()),
            });
        }
    }
    if (snapshot.expected_current.is_none() || snapshot.target.is_none())
        && evidence
            .source
            .observations
            .iter()
            .any(|observation| &observation.component == name && observation.observed == Ok(None))
    {
        options.push(RollbackOption {
            target: None,
            unavailable: None,
        });
    }
    options
}

async fn check_remote(
    execution: &PlannedContext,
    target: Option<&ReleaseRef>,
    history: &RetentionHistory,
    cancellation: &CancellationToken,
) -> Result<ReleaseRef, RollbackServiceError> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(RollbackServiceError::Cancelled),
        result = tokio::time::timeout(CHECK_TIMEOUT, remote_evidence(execution, target, history)) =>
            result.map_err(|_| RollbackServiceError::Timeout)?,
    }
}

async fn remote_evidence(
    execution: &PlannedContext,
    target: Option<&ReleaseRef>,
    history: &RetentionHistory,
) -> Result<ReleaseRef, RollbackServiceError> {
    let context = &execution.context;
    let name = &context.component;
    let preflight = execution
        .driver
        .preflight(context)
        .await
        .map_err(|source| remote_error(name, "preflight", source))?;
    if !required_capabilities()
        .iter()
        .all(|capability| preflight.effective_capabilities.contains(*capability))
    {
        return Err(component_error(
            name,
            "remote target does not provide required rollback capabilities",
        ));
    }
    let current = execution
        .driver
        .current(context)
        .await
        .map_err(|source| remote_error(name, "current", source))?
        .ok_or_else(|| {
            component_error(
                name,
                "current is not deployed; this rollback path requires a current Release",
            )
        })?;
    if !matches_context(context, &current) {
        return Err(component_error(
            name,
            "observed current has a different historical scope",
        ));
    }
    let inventory = execution
        .driver
        .inventory(context)
        .await
        .map_err(|source| remote_error(name, "inventory", source))?;
    let report = RecoveryComponentReport {
        scope: InspectionScope::from(&current),
        inventory: Ok(inventory),
        alignment: CurrentAlignment::Unplanned,
        package_alignment: PackageAlignment::Unplanned,
        notices: Vec::new(),
    };
    report
        .validate()
        .map_err(|_| component_error(name, "inventory violated its bounded identity contract"))?;
    let inventory = report
        .inventory
        .as_ref()
        .map_err(|_| RollbackServiceError::Worker)?;
    if inventory.releases.current != Ok(Some(current.version.clone()))
        || !inventory.releases.issues.is_empty()
    {
        return Err(component_error(
            name,
            "inventory is incomplete or current changed during preview",
        ));
    }
    for reference in std::iter::once(&current).chain(target) {
        let package = exact_package(history, reference).ok_or_else(|| {
            component_error(name, "original package evidence is missing or conflicting")
        })?;
        if !inventory.releases.releases.iter().any(|entry| {
            entry.extracted
                && entry.manifest == package.manifest
                && entry.sha256 == package.sha256
                && entry.size == package.size
        }) {
            return Err(component_error(
                name,
                "remote Release is missing or differs from its original package",
            ));
        }
    }
    Ok(current)
}

fn exact_package<'a>(
    history: &'a RetentionHistory,
    reference: &ReleaseRef,
) -> Option<&'a ReleasePackageRecord> {
    let mut matching = history
        .packages
        .iter()
        .filter(|package| &package.release == reference);
    let first = matching.next()?;
    matching.all(|package| package == first).then_some(first)
}

fn matches_context(context: &ComponentExecutionContext, release: &ReleaseRef) -> bool {
    release.driver == *context.target.driver_kind()
        && release.project_id == context.project_id
        && release.environment_id == context.environment_id
        && release.component == context.component
        && release.generation == context.generation
        && release.destination == context.destination
        && release.destination_revision == context.destination_revision
        && release.endpoint_fingerprint == context.endpoint_fingerprint
        && release
            .effective_capabilities
            .contains(Capability::Rollback)
}

const fn required_capabilities() -> [Capability; 3] {
    [
        Capability::Rollback,
        Capability::Observe,
        Capability::Inventory,
    ]
}

fn normalize_selection(selection: &mut DeploymentSelection) -> Result<(), RollbackServiceError> {
    selection.project_root = selection
        .project_root
        .canonicalize()
        .map_err(|_| RollbackServiceError::Invalid("Project directory is unavailable"))?;
    validate_selection(selection)
}

fn validate_selection(selection: &DeploymentSelection) -> Result<(), RollbackServiceError> {
    if selection.components.is_empty() || selection.components.len() > 256 {
        return Err(RollbackServiceError::Invalid(
            "select between 1 and 256 Components",
        ));
    }
    if !matches!(crate::config::load(&selection.project_root), Ok(ProjectConfigState::Loaded(ref config)) if config == &selection.config)
    {
        return Err(RollbackServiceError::Invalid(
            "Project YAML is missing, invalid or changed",
        ));
    }
    let environment = selection
        .config
        .environments
        .get(&selection.environment)
        .ok_or(RollbackServiceError::Invalid("Environment is unavailable"))?;
    if selection
        .components
        .iter()
        .any(|name| !environment.components.contains_key(name))
    {
        return Err(RollbackServiceError::Invalid(
            "Component is unavailable in this Environment",
        ));
    }
    Ok(())
}

fn load_registry(path: &Path) -> Result<DestinationRegistry, RollbackServiceError> {
    DestinationRegistry::load(path).map_err(|_| {
        RollbackServiceError::Invalid("Destination registry is unavailable or invalid")
    })
}

fn endpoint_label(key: &DestinationKey, record: &DestinationRevisionRecord) -> String {
    match &record.settings {
        crate::config::DestinationSettings::LinuxSsh {
            host, port, user, ..
        } => format!(
            "{key} revision {} — {user}@{host}:{port}",
            record.revision.get()
        ),
    }
}

fn cancelled(token: &CancellationToken) -> Result<(), RollbackServiceError> {
    if token.is_cancelled() {
        Err(RollbackServiceError::Cancelled)
    } else {
        Ok(())
    }
}

fn component_error(component: &ComponentName, message: &'static str) -> RollbackServiceError {
    RollbackServiceError::Component {
        component: component.clone(),
        message,
    }
}

fn remote_error(
    component: &ComponentName,
    operation: &'static str,
    source: DriverError,
) -> RollbackServiceError {
    RollbackServiceError::Remote {
        component: component.clone(),
        operation,
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests;
