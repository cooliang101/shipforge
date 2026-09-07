use std::{any::Any, sync::Mutex};

use async_trait::async_trait;

use super::*;
mod events;
use crate::{
    config::{DestinationSettings, HostKeyFingerprint},
    domain::{
        ComponentRelease, DeploymentState, DriverCapabilities, ReleaseManifest, ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, ComponentInventory, ComponentPlan, ComponentRequest,
        CredentialHandle, DriverDestinationInput, DriverError, DriverKind, DriverLog,
        DriverTargetInput, EventSink, PreflightReport, PreparedRelease, ReleaseInventory,
        ReleasePackage, RemoteAuditHistory, RetentionPolicy, ValidatedDestinationSettings,
        ValidatedTargetSettings,
        inventory::{InventoryRelease, TemporaryRemnants},
    },
    history::{DeploymentComponentSnapshot, DeploymentQuery, StepStatus},
};

#[derive(Debug)]
struct Settings(DriverKind, Option<serde_json::Value>);
impl ValidatedTargetSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn snapshot(&self) -> Option<serde_json::Value> {
        self.1.clone()
    }
    fn requires_recovery_snapshot(&self) -> bool {
        self.1
            .as_ref()
            .is_some_and(|value| !value["service"].is_null())
    }
}
impl ValidatedDestinationSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Default)]
struct State {
    current: BTreeMap<ComponentName, ReleaseRef>,
    packages: BTreeMap<ComponentName, Vec<InventoryRelease>>,
    calls: Vec<(String, InspectionScope)>,
    remove_yaml_on_rollback: Option<PathBuf>,
    faults: BTreeSet<Fault>,
    fail_persistence: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Fault {
    UnhealthyReceipt,
    CancelAfter,
    MissingCapability,
    Unknown,
    PreflightError,
    InventoryError,
}

#[derive(Debug, Default)]
struct FakeDriver(Mutex<State>);

impl FakeDriver {
    fn call(&self, action: &str, context: &ComponentExecutionContext) {
        self.0.lock().unwrap().calls.push((
            action.into(),
            InspectionScope {
                project: context.project_id.clone(),
                environment: context.environment_id.clone(),
                component: context.component.clone(),
                generation: context.generation,
                driver: self.kind(),
                destination: context.destination.clone(),
                destination_revision: context.destination_revision,
                endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            },
        ));
    }
}

#[async_trait]
impl DeploymentDriver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }
    fn static_capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::new(required_capabilities())
    }
    fn validate_destination(
        &self,
        _: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind(), None)))
    }
    fn validate_target(
        &self,
        input: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind(), Some(input.value.clone()))))
    }
    async fn preflight(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        self.call("preflight", context);
        if self
            .0
            .lock()
            .unwrap()
            .faults
            .contains(&Fault::PreflightError)
        {
            return Err(remote_failure("preflight"));
        }
        Ok(PreflightReport {
            effective_capabilities: if self
                .0
                .lock()
                .unwrap()
                .faults
                .contains(&Fault::MissingCapability)
            {
                DriverCapabilities::new([Capability::Observe])
            } else {
                self.static_capabilities()
            },
            notices: Vec::new(),
        })
    }
    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        self.call("current", context);
        let state = self.0.lock().unwrap();
        if state.faults.contains(&Fault::Unknown) {
            return Err(remote_failure("current"));
        }
        Ok(state.current.get(&context.component).cloned())
    }
    async fn inventory(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        self.call("inventory", context);
        let state = self.0.lock().unwrap();
        if state.faults.contains(&Fault::InventoryError) {
            return Err(remote_failure("inventory"));
        }
        Ok(ComponentInventory {
            releases: ReleaseInventory {
                releases: state.packages[&context.component].clone(),
                issues: Vec::new(),
                current: Ok(state
                    .current
                    .get(&context.component)
                    .map(|release| release.version.clone())),
                notices: Vec::new(),
            },
            audit: RemoteAuditHistory::default(),
            remnants: TemporaryRemnants::default(),
        })
    }
    async fn rollback(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        expected: Option<&ReleaseRef>,
        target: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        self.call("rollback", context);
        let mut state = self.0.lock().unwrap();
        assert_eq!(state.current.get(&context.component), expected);
        if let Some(target) = target {
            state
                .current
                .insert(context.component.clone(), target.clone());
        } else {
            state.current.remove(&context.component);
        }
        if let Some(path) = state.remove_yaml_on_rollback.take() {
            std::fs::remove_file(path).unwrap();
        }
        if let Some(path) = state.fail_persistence.take() {
            let history = HistoryStore::open(&path).unwrap();
            assert!(!history.pending_intents(deployment).unwrap().is_empty());
            rusqlite::Connection::open(path).unwrap().execute_batch(
                "CREATE TRIGGER deny_completion BEFORE UPDATE ON operation_intents BEGIN SELECT RAISE(ABORT, 'fixture full'); END;"
            ).unwrap();
        }
        if state.faults.remove(&Fault::CancelAfter) {
            context.cancellation.cancel();
        }
        let healthy = !state.faults.remove(&Fault::UnhealthyReceipt);
        Ok(ActivationReceipt {
            current: target.cloned(),
            healthy,
            warnings: Vec::new(),
        })
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        _: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        panic!("rollback never creates a new Release plan")
    }
    async fn prepare(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        panic!("rollback never prepares")
    }
    async fn activate(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("rollback must use the rollback operation")
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        panic!("no log query")
    }
    async fn cleanup(
        &self,
        _: &ComponentExecutionContext,
        _: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        panic!("no cleanup")
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    selection: DeploymentSelection,
    registry_path: PathBuf,
    history_path: PathBuf,
    driver: Arc<FakeDriver>,
    session: Arc<DeploymentSession>,
    service: RollbackService,
}

impl Fixture {
    fn new(names: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../../../docs/examples/shipforge.yaml")
                .replace("npm", "missing-build-program"),
        )
        .unwrap();
        let ProjectConfigState::Loaded(config) = crate::config::load(directory.path()).unwrap()
        else {
            panic!()
        };
        let mut registry = DestinationRegistry::new();
        for target in config.environments["production"].components.values() {
            if registry.resolve(&target.destination).is_none() {
                registry
                    .create(target.destination.clone(), settings("old.example.invalid"))
                    .unwrap();
            }
        }
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let history_path = directory.path().join("history.sqlite3");
        let driver = Arc::new(FakeDriver::default());
        let mut drivers = DriverRegistry::default();
        drivers.register(driver.clone()).unwrap();
        let session = Arc::new(DeploymentSession::default());
        let service = RollbackService::new(
            Arc::new(drivers),
            history_path.clone(),
            Arc::clone(&session),
        );
        let selection = DeploymentSelection {
            project_root: directory.path().to_owned(),
            config,
            environment: "production".into(),
            components: names.iter().map(|name| component(name)).collect(),
        };
        Self {
            directory,
            selection,
            registry_path,
            history_path,
            driver,
            session,
            service,
        }
    }

    fn publish(
        &self,
        version_text: &str,
        healthy: bool,
        known_previous: bool,
    ) -> (DeploymentId, BTreeMap<ComponentName, Option<ReleaseRef>>) {
        let order = self
            .selection
            .components
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        self.publish_ordered(version_text, healthy, known_previous, &order)
    }

    fn publish_ordered(
        &self,
        version_text: &str,
        healthy: bool,
        known_previous: bool,
        order: &[ComponentName],
    ) -> (DeploymentId, BTreeMap<ComponentName, Option<ReleaseRef>>) {
        let history = HistoryStore::open(&self.history_path).unwrap();
        let id = DeploymentId::new();
        let environment = &self.selection.config.environments["production"];
        let registry = load_registry(&self.registry_path).unwrap();
        history
            .create_deployment(&id, &self.selection.config.project_id, &environment.id, 1)
            .unwrap();
        let snapshots: Vec<_> = order
            .iter()
            .enumerate()
            .map(|(order, name)| self.snapshot(name, order, version_text, &registry))
            .collect();
        history.record_component_snapshots(&id, &snapshots).unwrap();
        history
            .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 2)
            .unwrap();
        let mut refs = BTreeMap::new();
        for snapshot in &snapshots {
            let release = &snapshot.release;
            let manifest = ReleaseManifest::new(
                &ComponentRelease {
                    project_id: release.project_id.clone(),
                    environment_id: release.environment_id.clone(),
                    component: release.component.clone(),
                    generation: release.generation,
                    version: release.version.clone(),
                    destination: release.destination.clone(),
                    destination_revision: release.destination_revision,
                },
                100,
                None,
            );
            history
                .record_release_package(&id, release, &manifest, &"a".repeat(64), 512)
                .unwrap();
            history
                .record_observation(
                    &id,
                    &release.component,
                    "preflight",
                    if known_previous {
                        Ok(snapshot.expected_current.as_ref())
                    } else {
                        Err("unknown")
                    },
                    None,
                    3,
                    &Redactor::default(),
                )
                .unwrap();
            history
                .record_observation(
                    &id,
                    &release.component,
                    "activate-receipt",
                    Ok(Some(release)),
                    Some(healthy),
                    4,
                    &Redactor::default(),
                )
                .unwrap();
            let mut state = self.driver.0.lock().unwrap();
            state
                .packages
                .entry(release.component.clone())
                .or_default()
                .push(InventoryRelease {
                    manifest,
                    sha256: "a".repeat(64),
                    size: 512,
                    extracted: true,
                });
            state
                .current
                .insert(release.component.clone(), release.clone());
            refs.insert(release.component.clone(), Some(release.clone()));
        }
        history
            .transition_deployment(&id, DeploymentState::Running, DeploymentState::Succeeded, 5)
            .unwrap();
        (id, refs)
    }

    fn snapshot(
        &self,
        name: &ComponentName,
        order: usize,
        version_text: &str,
        registry: &DestinationRegistry,
    ) -> DeploymentComponentSnapshot {
        let environment = &self.selection.config.environments["production"];
        let target = &environment.components[name];
        let destination = registry.resolve(&target.destination).unwrap();
        let release = ReleaseRef {
            driver: self.driver.kind(),
            project_id: self.selection.config.project_id.clone(),
            environment_id: environment.id.clone(),
            component: name.clone(),
            generation: target.generation,
            version: version(version_text),
            destination: target.destination.clone(),
            destination_revision: destination.revision,
            endpoint_fingerprint: destination.endpoint_fingerprint.clone(),
            effective_capabilities: self.driver.static_capabilities(),
        };
        DeploymentComponentSnapshot {
            target_snapshot: Some(target.driver_input().value),
            release: release.clone(),
            target: Some(release),
            expected_current: self.driver.0.lock().unwrap().current.get(name).cloned(),
            execution_order: u32::try_from(order).unwrap(),
        }
    }

    async fn plan(
        &self,
        source: DeploymentId,
        targets: BTreeMap<ComponentName, Option<ReleaseRef>>,
    ) -> Result<RollbackPlan, RollbackServiceError> {
        let mut selection = self.selection.clone();
        selection.components = targets.keys().cloned().collect();
        self.service
            .plan(
                selection,
                source,
                targets,
                &self.registry_path,
                &CancellationToken::new(),
            )
            .await
    }

    async fn execute(&self, plan: RollbackPlan) -> Result<RollbackReport, RollbackServiceError> {
        self.service
            .execute(plan, &self.registry_path, &CancellationToken::new())
            .await
    }

    fn mutations(&self) -> Vec<ComponentName> {
        self.driver
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(action, _)| action == "rollback")
            .map(|(_, scope)| scope.component.clone())
            .collect()
    }

    fn revise(&self) {
        let mut registry = load_registry(&self.registry_path).unwrap();
        let key = self.selection.config.environments["production"].components
            [&component("frontend")]
            .destination
            .clone();
        registry
            .revise(&key, settings("new.example.invalid"))
            .unwrap();
        registry.save(&self.registry_path).unwrap();
    }
}

fn settings(host: &str) -> DestinationSettings {
    DestinationSettings::LinuxSsh {
        host: host.into(),
        port: 22,
        user: "deploy".into(),
        credential: CredentialHandle::new(),
        host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
    }
}
fn component(name: &str) -> ComponentName {
    ComponentName::parse(name).unwrap()
}
fn version(name: &str) -> ReleaseVersion {
    ReleaseVersion::parse(name).unwrap()
}

#[tokio::test]
async fn legacy_service_history_is_not_reconstructed_from_current_commands() {
    for name in ["frontend", "backend"] {
        let fixture = Fixture::new(&[name]);
        let (_, targets) = fixture.publish("v1", true, true);
        let (source, _) = fixture.publish("v2", true, true);
        rusqlite::Connection::open(&fixture.history_path).unwrap().execute(
            "UPDATE component_snapshots SET snapshot=json_remove(snapshot,'$.target_snapshot') WHERE deployment_id=?1",
            [source.to_string()],
        ).unwrap();
        let result = fixture.plan(source, targets).await;
        if name == "backend" {
            assert!(
                format!("{:?}", result.unwrap_err())
                    .contains("Historical service commands are unavailable")
            );
            assert!(fixture.driver.0.lock().unwrap().calls.is_empty());
        } else {
            assert!(result.is_ok(), "file-only legacy history remains usable");
        }
        assert!(fixture.mutations().is_empty());
    }
}

#[tokio::test]
async fn changed_frozen_commands_are_rejected_even_with_matching_generation() {
    let fixture = Fixture::new(&["backend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    rusqlite::Connection::open(&fixture.history_path).unwrap().execute(
        "UPDATE component_snapshots SET snapshot=json_set(snapshot,'$.target_snapshot.service.start[0][1]','stop') WHERE deployment_id=?1",
        [source.to_string()],
    ).unwrap();
    let error = fixture.plan(source, targets).await.unwrap_err();
    assert!(format!("{error:?}").contains("differ from the frozen history"));
    assert!(fixture.driver.0.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn candidates_are_local_require_health_and_distinguish_unknown_from_absent() {
    for known in [false, true] {
        let fixture = Fixture::new(&["frontend"]);
        let (source, _) = fixture.publish("v1", false, known);
        let candidates = fixture
            .service
            .candidates(
                fixture.selection.clone(),
                source,
                &fixture.registry_path,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(fixture.driver.0.lock().unwrap().calls.is_empty());
        assert!(
            candidates.components[0].options[0]
                .unavailable
                .as_ref()
                .unwrap()
                .contains("health")
        );
        assert_eq!(
            candidates.components[0]
                .options
                .iter()
                .any(|option| option.target.is_none()),
            known
        );
    }
}

#[tokio::test]
async fn subset_rollback_uses_only_proven_targets_and_creates_linked_durable_history() {
    let fixture = Fixture::new(&["frontend", "backend"]);
    let (_, mut targets) = fixture.publish("v1", true, true);
    let (source, latest) = fixture.publish("v2", true, true);
    targets.remove(&component("backend"));
    let before = std::fs::read(&fixture.history_path).unwrap();
    let plan = fixture.plan(source.clone(), targets.clone()).await.unwrap();
    assert_eq!(std::fs::read(&fixture.history_path).unwrap(), before);
    assert_eq!(plan.entries().len(), 1);
    assert_eq!(plan.source(), &source);
    assert_eq!(plan.execution_order(), [component("frontend")]);
    assert!(fixture.mutations().is_empty());
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.failure.is_none() && report.warnings.is_empty());
    assert_eq!(fixture.mutations(), [component("frontend")]);
    let state = fixture.driver.0.lock().unwrap();
    assert_eq!(
        state.current.get(&component("frontend")),
        targets[&component("frontend")].as_ref()
    );
    assert_eq!(
        state.current.get(&component("backend")),
        latest[&component("backend")].as_ref()
    );
    drop(state);
    let history = HistoryStore::open_existing_read_only(&fixture.history_path).unwrap();
    let record = history.deployment(&report.deployment.id).unwrap().unwrap();
    assert_eq!(record.related_deployment, Some(source));
    assert!(
        history
            .pending_intents(&record.deployment)
            .unwrap()
            .is_empty()
    );
    let steps = history.steps(&record.deployment).unwrap();
    assert_eq!(steps[0].name, "rollback");
    assert_eq!(steps[0].status, StepStatus::Succeeded);
}

#[tokio::test]
async fn multiple_targets_execute_in_reverse_order_without_build_or_prepare() {
    let fixture = Fixture::new(&["backend", "worker"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    let plan = fixture.plan(source, targets).await.unwrap();
    assert_eq!(
        plan.execution_order(),
        [component("worker"), component("backend")]
    );
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        fixture.mutations(),
        [component("worker"), component("backend")]
    );
}

#[tokio::test]
async fn known_absent_target_is_explicit_and_not_inferred_from_missing_inventory() {
    let fixture = Fixture::new(&["frontend"]);
    let (source, _) = fixture.publish("v1", true, true);
    let plan = fixture
        .plan(source, BTreeMap::from([(component("frontend"), None)]))
        .await
        .unwrap();
    assert!(plan.entries()[0].target.is_none());
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(fixture.driver.0.lock().unwrap().current.is_empty());
}

#[tokio::test]
async fn unavailable_history_never_creates_a_database_or_contacts_driver() {
    let fixture = Fixture::new(&["frontend"]);
    let error = fixture
        .service
        .candidates(
            fixture.selection.clone(),
            DeploymentId::new(),
            &fixture.registry_path,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, RollbackServiceError::History(_)));
    assert!(!fixture.history_path.exists());
    assert!(fixture.driver.0.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn historical_revision_uses_its_exact_endpoint_without_rewriting_release() {
    let fixture = Fixture::new(&["frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    fixture.revise();
    let plan = fixture.plan(source, targets.clone()).await.unwrap();
    assert!(
        plan.entries()[0]
            .destination
            .contains("old.example.invalid")
    );
    assert_eq!(plan.entries()[0].target, targets[&component("frontend")]);
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    let target = targets[&component("frontend")].as_ref().unwrap();
    assert!(
        fixture
            .driver
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(
                |(_, scope)| scope.destination_revision == target.destination_revision
                    && scope.endpoint_fingerprint == target.endpoint_fingerprint
            )
    );
}

#[tokio::test]
async fn yaml_registry_history_and_current_drift_reject_before_rollback() {
    for drift in 0..4 {
        let fixture = Fixture::new(&["frontend"]);
        let (_, targets) = fixture.publish("v1", true, true);
        let (source, _) = fixture.publish("v2", true, true);
        let plan = fixture.plan(source.clone(), targets).await.unwrap();
        match drift {
            0 => std::fs::remove_file(fixture.directory.path().join("shipforge.yaml")).unwrap(),
            1 => fixture.revise(),
            2 => HistoryStore::open(&fixture.history_path)
                .unwrap()
                .record_observation(
                    &source,
                    &component("frontend"),
                    "late",
                    Err("unknown"),
                    None,
                    8,
                    &Redactor::default(),
                )
                .unwrap(),
            _ => {
                fixture
                    .driver
                    .0
                    .lock()
                    .unwrap()
                    .current
                    .get_mut(&component("frontend"))
                    .unwrap()
                    .version = version("other");
            }
        }
        assert!(fixture.execute(plan).await.is_err());
        assert!(fixture.mutations().is_empty());
        assert_eq!(
            HistoryStore::open_existing_read_only(&fixture.history_path)
                .unwrap()
                .deployments(
                    &fixture.selection.config.project_id,
                    &fixture.selection.config.environments["production"].id,
                    DeploymentQuery::default(),
                )
                .unwrap()
                .len(),
            2
        );
    }
}

#[tokio::test]
async fn missing_capability_corrupt_package_or_unknown_current_never_mutates() {
    for fault in 0..3 {
        let fixture = Fixture::new(&["frontend"]);
        let (_, targets) = fixture.publish("v1", true, true);
        let (source, _) = fixture.publish("v2", true, true);
        {
            let mut state = fixture.driver.0.lock().unwrap();
            match fault {
                0 => {
                    state.faults.insert(Fault::MissingCapability);
                }
                1 => {
                    state.packages.get_mut(&component("frontend")).unwrap()[0].sha256 =
                        "b".repeat(64);
                }
                _ => {
                    state.faults.insert(Fault::Unknown);
                }
            }
        }
        let error = fixture.plan(source, targets).await.unwrap_err();
        assert!(!error.to_string().contains("UNREGISTERED_DRIVER_SENTINEL"));
        assert!(fixture.mutations().is_empty());
    }
}

fn remote_failure(operation: &str) -> DriverError {
    DriverError {
        recovery_blocked: false,
        stage: format!("UNREGISTERED_DRIVER_SENTINEL {operation}"),
        target: "UNREGISTERED_DRIVER_SENTINEL /private/fixture".into(),
        message: "UNREGISTERED_DRIVER_SENTINEL private credential diagnostic".into(),
        suggested_action: "UNREGISTERED_DRIVER_SENTINEL private recovery detail".into(),
    }
}

#[tokio::test]
async fn remote_preflight_current_and_inventory_errors_keep_typed_sources_but_public_display_is_safe()
 {
    for (fault, expected) in [
        (Fault::PreflightError, "preflight"),
        (Fault::Unknown, "current"),
        (Fault::InventoryError, "inventory"),
    ] {
        let fixture = Fixture::new(&["frontend"]);
        let (_, targets) = fixture.publish("v1", true, true);
        let (source, _) = fixture.publish("v2", true, true);
        fixture.driver.0.lock().unwrap().faults.insert(fault);
        let error = fixture.plan(source, targets).await.unwrap_err();
        let public = error.to_string();
        assert!(public.contains("frontend"));
        assert!(public.contains(expected));
        assert!(public.contains("inspect again before rollback"));
        assert!(!public.contains("UNREGISTERED_DRIVER_SENTINEL"));
        assert!(!public.contains("/private/fixture"));
        let retained = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<Box<DriverError>>()
            .unwrap();
        assert_eq!(retained.as_ref(), &remote_failure(expected));
        assert!(
            matches!(error, RollbackServiceError::Remote { operation, component: ref name, .. } if operation == expected && name == &component("frontend"))
        );
        assert!(fixture.mutations().is_empty());
    }
}

#[tokio::test]
async fn execution_guard_rechecks_configuration_between_component_mutations() {
    let fixture = Fixture::new(&["backend", "worker"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    let plan = fixture.plan(source, targets).await.unwrap();
    fixture.driver.0.lock().unwrap().remove_yaml_on_rollback =
        Some(fixture.directory.path().join("shipforge.yaml"));
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(fixture.mutations(), [component("worker")]);
    assert!(!report.compensation_failures.is_empty());
}

#[tokio::test]
async fn unhealthy_receipt_and_cancellation_keep_orchestrator_compensation_semantics() {
    for cancel in [false, true] {
        let fixture = Fixture::new(&["frontend"]);
        let (_, targets) = fixture.publish("v1", true, true);
        let (source, current) = fixture.publish("v2", true, true);
        let plan = fixture.plan(source, targets).await.unwrap();
        {
            let mut state = fixture.driver.0.lock().unwrap();
            state.faults.insert(if cancel {
                Fault::CancelAfter
            } else {
                Fault::UnhealthyReceipt
            });
        }
        let report = fixture.execute(plan).await.unwrap();
        assert_eq!(
            report.deployment.state,
            if cancel {
                DeploymentState::Cancelled
            } else {
                DeploymentState::Failed
            }
        );
        assert_eq!(
            fixture
                .driver
                .0
                .lock()
                .unwrap()
                .current
                .get(&component("frontend")),
            current[&component("frontend")].as_ref()
        );
        assert_eq!(fixture.mutations().len(), 2);
    }
}

#[tokio::test]
async fn post_effect_persistence_failure_preserves_report_and_compensates() {
    let fixture = Fixture::new(&["frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    let plan = fixture.plan(source, targets).await.unwrap();
    fixture.driver.0.lock().unwrap().fail_persistence = Some(fixture.history_path.clone());
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(!report.warnings.is_empty());
    assert_eq!(fixture.mutations().len(), 2);
}

#[tokio::test]
async fn shared_session_rejects_candidates_plan_and_execute_without_polling() {
    let fixture = Fixture::new(&["frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    let plan = fixture.plan(source.clone(), targets.clone()).await.unwrap();
    fixture.driver.0.lock().unwrap().calls.clear();
    fixture
        .session
        .run(Box::pin(async {
            assert!(matches!(
                fixture
                    .service
                    .candidates(
                        fixture.selection.clone(),
                        source.clone(),
                        &fixture.registry_path,
                        &CancellationToken::new()
                    )
                    .await,
                Err(RollbackServiceError::Busy)
            ));
            assert!(matches!(
                fixture.plan(source, targets).await,
                Err(RollbackServiceError::Busy)
            ));
            assert!(matches!(
                fixture.execute(plan).await,
                Err(RollbackServiceError::Busy)
            ));
        }))
        .await
        .unwrap();
    assert!(fixture.driver.0.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn other_revision_candidate_is_visible_but_never_rebound_to_source_scope() {
    let fixture = Fixture::new(&["frontend"]);
    let (source, _) = fixture.publish("old", true, true);
    fixture.revise();
    fixture.driver.0.lock().unwrap().current.clear();
    let (_, newer) = fixture.publish("new", true, true);
    let candidates = fixture
        .service
        .candidates(
            fixture.selection.clone(),
            source.clone(),
            &fixture.registry_path,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let option = candidates.components[0]
        .options
        .iter()
        .find(|option| option.target == newer[&component("frontend")])
        .unwrap();
    assert!(option.unavailable.as_ref().unwrap().contains("revision"));
    assert!(fixture.plan(source, newer).await.is_err());
    assert!(fixture.mutations().is_empty());
}

#[tokio::test]
async fn missing_package_keeps_an_unavailable_original_reference() {
    let fixture = Fixture::new(&["frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    rusqlite::Connection::open(&fixture.history_path)
        .unwrap()
        .execute(
            "DELETE FROM release_packages WHERE release_ref LIKE '%v1%'",
            [],
        )
        .unwrap();
    let candidates = fixture
        .service
        .candidates(
            fixture.selection.clone(),
            source.clone(),
            &fixture.registry_path,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let option = candidates.components[0]
        .options
        .iter()
        .find(|option| option.target == targets[&component("frontend")])
        .unwrap();
    assert!(option.unavailable.as_ref().unwrap().contains("package"));
    assert!(fixture.plan(source, targets).await.is_err());
    assert!(fixture.mutations().is_empty());
}

#[tokio::test]
async fn unknown_absence_and_modified_target_identity_are_not_executable() {
    let fixture = Fixture::new(&["frontend"]);
    let (source, mut targets) = fixture.publish("v1", true, false);
    assert!(
        fixture
            .plan(
                source.clone(),
                BTreeMap::from([(component("frontend"), None)])
            )
            .await
            .is_err()
    );
    targets
        .get_mut(&component("frontend"))
        .unwrap()
        .as_mut()
        .unwrap()
        .version = version("invented");
    assert!(fixture.plan(source, targets).await.is_err());
    assert!(fixture.mutations().is_empty());
}

#[tokio::test]
async fn rollback_reverses_non_alphabetical_frozen_order_not_current_yaml_order() {
    let fixture = Fixture::new(&["backend", "frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish_ordered(
        "v2",
        true,
        true,
        &[component("frontend"), component("backend")],
    );
    let yaml_order = fixture.selection.config.environments["production"]
        .activation_order("production", fixture.selection.components.iter())
        .unwrap();
    assert_eq!(yaml_order, [component("backend"), component("frontend")]);
    let plan = fixture.plan(source, targets).await.unwrap();
    assert_eq!(
        plan.execution_order(),
        [component("backend"), component("frontend")]
    );
    let report = fixture.execute(plan).await.unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        fixture.mutations(),
        [component("backend"), component("frontend")]
    );
}

#[tokio::test]
async fn changed_historical_target_package_or_health_invalidates_preview_before_effects() {
    for change_package in [false, true] {
        let fixture = Fixture::new(&["frontend"]);
        let (old, targets) = fixture.publish("v1", true, true);
        let (source, _) = fixture.publish("v2", true, true);
        let plan = fixture.plan(source, targets).await.unwrap();
        let database = rusqlite::Connection::open(&fixture.history_path).unwrap();
        if change_package {
            database
                .execute(
                    "UPDATE release_packages SET sha256=?1 WHERE deployment_id=?2",
                    [&"b".repeat(64), &old.to_string()],
                )
                .unwrap();
        } else {
            database.execute("UPDATE deployment_observations SET healthy=0 WHERE deployment_id=?1 AND healthy=1",
                [old.to_string()]).unwrap();
        }
        assert!(matches!(
            fixture.execute(plan).await,
            Err(RollbackServiceError::StalePlan)
        ));
        assert!(fixture.mutations().is_empty());
        assert_eq!(
            fixture.driver.0.lock().unwrap().current[&component("frontend")].version,
            version("v2")
        );
    }
}
