use std::{any::Any, collections::BTreeSet, sync::Mutex};

use tokio_util::sync::CancellationToken;

use super::*;
mod events;
use crate::{
    application::{DeploymentComponent, DeploymentOrchestrator, PlannedComponent},
    config::{DestinationSettings, HostKeyFingerprint},
    domain::{Capability, ComponentName, ComponentOutcome, ComponentRelease, ReleaseVersion},
    drivers::{CredentialHandle, EndpointFingerprint},
    history::HistoryStore,
    telemetry::Redactor,
};

const PROJECT: &str = include_str!("../../../docs/examples/shipforge.yaml");

fn cleanup_candidate(component: &DeploymentComponent) -> crate::drivers::CleanupCandidate {
    crate::drivers::CleanupCandidate {
        release: crate::application::orchestrator::planned_release_ref(&component.planned),
        package: crate::drivers::inventory::InventoryRelease {
            manifest: component.package.manifest().clone(),
            sha256: component.package.sha256().into(),
            size: component.package.size(),
            extracted: true,
        },
        expected_current: None,
    }
}

#[derive(Debug)]
struct Settings(DriverKind);

impl ValidatedTargetSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
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
struct FakeState {
    actions: Vec<String>,
    current: BTreeMap<ComponentName, ReleaseRef>,
    change_config_on_activation: Option<(ComponentName, PathBuf)>,
}

#[derive(Debug, Default)]
struct FakeDriver(Mutex<FakeState>);

impl FakeDriver {
    fn reference(
        &self,
        context: &ComponentExecutionContext,
        version: ReleaseVersion,
    ) -> ReleaseRef {
        ReleaseRef {
            driver: self.kind(),
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            effective_capabilities: self.static_capabilities(),
        }
    }
}

#[async_trait]
impl DeploymentDriver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }
    fn static_capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::new([
            Capability::StagedDeployment,
            Capability::ExplicitActivation,
            Capability::Rollback,
        ])
    }
    fn validate_target(
        &self,
        _: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind())))
    }
    fn validate_destination(
        &self,
        _: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind())))
    }
    async fn preflight(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        Ok(PreflightReport {
            effective_capabilities: self.static_capabilities(),
            notices: Vec::new(),
        })
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        Ok(ComponentPlan {
            release: request.release.clone(),
            expected_current: None,
            effective_capabilities: self.static_capabilities(),
            driver_steps: Vec::new(),
        })
    }
    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .current
            .get(&context.component)
            .cloned())
    }
    async fn current_with_events(
        &self,
        context: &ComponentExecutionContext,
        events: &dyn EventSink,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        events::driver_event(events, "observe");
        self.current(context).await
    }
    async fn prepare(
        &self,
        _: &DeploymentId,
        context: &ComponentExecutionContext,
        plan: &ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        self.0
            .lock()
            .unwrap()
            .actions
            .push(format!("prepare:{}", context.component));
        Ok(PreparedRelease {
            release: self.reference(context, plan.release.version.clone()),
            already_active: false,
        })
    }
    async fn activate(
        &self,
        _: &DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        let mut state = self.0.lock().unwrap();
        state
            .actions
            .push(format!("activate:{}", context.component));
        state
            .current
            .insert(context.component.clone(), release.clone());
        if let Some((name, path)) = &state.change_config_on_activation
            && name == &context.component
        {
            std::fs::write(path, PROJECT.replace("generation: 1", "generation: 2")).unwrap();
            return Err(DriverError {
                stage: "activate".into(),
                target: context.component.to_string(),
                message: "injected failure after activation".into(),
                suggested_action: "inspect the test".into(),
            });
        }
        Ok(ActivationReceipt {
            current: Some(release.clone()),
            healthy: true,
            warnings: Vec::new(),
        })
    }
    async fn rollback(
        &self,
        _: &DeploymentId,
        context: &ComponentExecutionContext,
        expected_current: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        let mut state = self.0.lock().unwrap();
        assert_eq!(state.current.get(&context.component), expected_current);
        state
            .actions
            .push(format!("rollback:{}", context.component));
        state.current.remove(&context.component);
        if let Some(release) = release {
            state
                .current
                .insert(context.component.clone(), release.clone());
        }
        Ok(ActivationReceipt {
            current: release.cloned(),
            healthy: true,
            warnings: Vec::new(),
        })
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        Ok(Vec::new())
    }
    async fn cleanup(
        &self,
        context: &ComponentExecutionContext,
        _: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        self.0
            .lock()
            .unwrap()
            .actions
            .push(format!("cleanup:{}", context.component));
        Ok(CleanupReport::default())
    }

    async fn activate_with_events(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
        events: &dyn EventSink,
    ) -> Result<ActivationReceipt, DriverError> {
        events::driver_event(events, "activate");
        self.activate(deployment, context, release).await
    }

    async fn rollback_with_events(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        expected_current: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
        events: &dyn EventSink,
    ) -> Result<ActivationReceipt, DriverError> {
        events::driver_event(events, "rollback");
        self.rollback(deployment, context, expected_current, release)
            .await
    }

    async fn cleanup_with_events(
        &self,
        context: &ComponentExecutionContext,
        policy: &RetentionPolicy,
        events: &dyn EventSink,
    ) -> Result<CleanupReport, DriverError> {
        events::driver_event(events, "cleanup");
        self.cleanup(context, policy).await
    }
}

struct NoEvents;
impl EventSink for NoEvents {
    fn emit(&self, _: DriverLog) {}
}

struct Fixture {
    directory: tempfile::TempDir,
    project: ProjectConfig,
    registry: DestinationRegistry,
    registry_path: PathBuf,
    inner: Arc<FakeDriver>,
    guard: Arc<ExecutionGuard>,
}

fn historical_guard(fixture: &mut Fixture) -> (ExecutionGuard, DestinationKey) {
    let key = fixture.project.environments["production"].components
        [&ComponentName::parse("frontend").unwrap()]
        .destination
        .clone();
    let old = fixture.registry.resolve(&key).unwrap().clone();
    let mut settings = old.settings.clone();
    let DestinationSettings::LinuxSsh { host, .. } = &mut settings;
    *host = "new-target.invalid".into();
    let latest = fixture.registry.revise(&key, settings).unwrap().clone();
    fixture.registry.save(&fixture.registry_path).unwrap();
    let guard = ExecutionGuard::new_historical(
        fixture.directory.path().to_owned(),
        fixture.project.clone(),
        fixture.registry_path.clone(),
        BTreeMap::from([(key.clone(), latest.clone())]),
        BTreeMap::from([
            ((key.clone(), old.revision), old),
            ((key.clone(), latest.revision), latest),
        ]),
    );
    (guard, key)
}

#[test]
fn historical_guard_pins_multiple_used_revisions_without_weakening_latest_guard() {
    let mut fixture = Fixture::new();
    let (guard, key) = historical_guard(&mut fixture);
    assert!(
        fixture.guard.validate().is_err(),
        "ordinary deployment still rejects latest drift"
    );
    assert!(guard.validate().is_ok());
    assert_eq!(guard.historical.len(), 2);
    let mut changed = fixture.registry.resolve(&key).unwrap().settings.clone();
    let DestinationSettings::LinuxSsh { port, .. } = &mut changed;
    *port = 2222;
    fixture.registry.revise(&key, changed).unwrap();
    fixture.registry.save(&fixture.registry_path).unwrap();
    assert!(guard.validate().is_err());
}

#[test]
fn historical_guard_rejects_used_revision_removal_or_replacement() {
    for remove in [false, true] {
        let mut fixture = Fixture::new();
        let (guard, key) = historical_guard(&mut fixture);
        let mut value = serde_yaml_ng::to_value(&fixture.registry).unwrap();
        let revisions = value["destinations"][key.as_str()]["revisions"]
            .as_sequence_mut()
            .unwrap();
        if remove {
            revisions.remove(0);
        } else {
            let latest = revisions[1].clone();
            revisions[0] = latest;
            revisions[0]["revision"] = serde_yaml_ng::Value::Number(1.into());
        }
        std::fs::write(
            &fixture.registry_path,
            serde_yaml_ng::to_string(&value).unwrap(),
        )
        .unwrap();
        assert!(
            guard.validate().is_err(),
            "old revision must never fall back to latest"
        );
    }
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("shipforge.yaml"), PROJECT).unwrap();
        let ProjectConfigState::Loaded(project) = crate::config::load(directory.path()).unwrap()
        else {
            panic!("example Project must load");
        };
        let mut registry = DestinationRegistry::new();
        let destinations = project.environments["production"]
            .components
            .values()
            .map(|target| target.destination.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut snapshots = BTreeMap::new();
        for destination in destinations {
            let settings = DestinationSettings::LinuxSsh {
                host: "test.invalid".into(),
                port: 22,
                user: "deploy".into(),
                credential: CredentialHandle::new(),
                host_key: HostKeyFingerprint::parse("SHA256:fake").unwrap(),
            };
            let record = registry
                .create(destination.clone(), settings)
                .unwrap()
                .clone();
            snapshots.insert(destination, record);
        }
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let guard = Arc::new(ExecutionGuard::new(
            directory.path().to_owned(),
            project.clone(),
            registry_path.clone(),
            snapshots,
        ));
        Self {
            directory,
            project,
            registry,
            registry_path,
            inner: Arc::new(FakeDriver::default()),
            guard,
        }
    }

    fn component(&self, name: &str) -> DeploymentComponent {
        let name = ComponentName::parse(name).unwrap();
        let environment = &self.project.environments["production"];
        let target = &environment.components[&name];
        let record = self.registry.resolve(&target.destination).unwrap();
        let resolved = record.resolve();
        let settings = Arc::new(Settings(self.inner.kind()));
        let context = ComponentExecutionContext {
            project_id: self.project.project_id.clone(),
            environment_id: environment.id.clone(),
            component: name,
            generation: target.generation,
            destination: target.destination.clone(),
            destination_revision: record.revision,
            credential: resolved.credential,
            endpoint_fingerprint: resolved.endpoint_fingerprint,
            destination_settings: settings.clone(),
            target: settings,
            cancellation: CancellationToken::new(),
        };
        let release = ComponentRelease {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version: ReleaseVersion::parse("v1").unwrap(),
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
        };
        DeploymentComponent {
            planned: PlannedComponent {
                notices: Vec::new(),
                driver: self.guard.wrap(self.inner.clone(), context.clone()),
                context,
                plan: ComponentPlan {
                    release: release.clone(),
                    effective_capabilities: self.inner.static_capabilities(),
                    expected_current: None,
                    driver_steps: Vec::new(),
                },
            },
            package: ReleasePackage::new(release, "unused.tar.gz".into(), "a".repeat(64), 1),
        }
    }

    fn actions(&self) -> Vec<String> {
        self.inner.0.lock().unwrap().actions.clone()
    }
}

#[tokio::test]
async fn unchanged_snapshot_forwards_mutations_with_replaced_cancellation_tokens() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let driver = &component.planned.driver;
    let mut context = component.planned.context.clone();
    context.cancellation = CancellationToken::new();
    let deployment = DeploymentId::new();
    let prepared = driver
        .prepare(
            &deployment,
            &context,
            &component.planned.plan,
            &component.package,
            &NoEvents,
        )
        .await
        .unwrap();
    driver
        .activate(&deployment, &context, &prepared.release)
        .await
        .unwrap();
    context.cancellation = CancellationToken::new();
    driver
        .rollback(&deployment, &context, Some(&prepared.release), None)
        .await
        .unwrap();
    driver
        .cleanup(
            &context,
            &RetentionPolicy {
                protected_versions: BTreeSet::new(),
                retain_count: 1,
                candidate: cleanup_candidate(&component),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.actions(),
        [
            "prepare:backend",
            "activate:backend",
            "rollback:backend",
            "cleanup:backend"
        ]
    );
}

#[tokio::test]
async fn destination_revision_and_same_revision_endpoint_changes_block_activation_after_prepare() {
    for revise in [true, false] {
        let mut fixture = Fixture::new();
        let component = fixture.component("backend");
        let context = &component.planned.context;
        let deployment = DeploymentId::new();
        let prepared = component
            .planned
            .driver
            .prepare(
                &deployment,
                context,
                &component.planned.plan,
                &component.package,
                &NoEvents,
            )
            .await
            .unwrap();
        let mut settings = fixture
            .registry
            .resolve(&context.destination)
            .unwrap()
            .settings
            .clone();
        if revise {
            fixture
                .registry
                .revise(&context.destination, settings)
                .unwrap();
        } else {
            let DestinationSettings::LinuxSsh { port, .. } = &mut settings;
            *port = 2222;
            let mut registry = DestinationRegistry::new();
            for (key, expected) in &fixture.guard.destinations {
                registry
                    .create(
                        key.clone(),
                        if *key == context.destination {
                            settings.clone()
                        } else {
                            expected.settings.clone()
                        },
                    )
                    .unwrap();
            }
            fixture.registry = registry;
        }
        fixture.registry.save(&fixture.registry_path).unwrap();
        let error = component
            .planned
            .driver
            .activate(&deployment, context, &prepared.release)
            .await
            .unwrap_err();
        assert_eq!(error.stage, "activate");
        assert!(error.message.contains("Destination changed"));
        assert_eq!(fixture.actions(), ["prepare:backend"]);
    }
}

#[tokio::test]
async fn saved_target_build_and_generation_changes_block_prepare() {
    for (old, new) in [
        ("generation: 1", "generation: 2"),
        ("mall-api.service", "other-api.service"),
        (
            "http://127.0.0.1:8080/health",
            "http://127.0.0.1:8081/health",
        ),
        ("[npm, ci]", "[npm, install]"),
        ("project: mall", "project: renamed"),
    ] {
        let fixture = Fixture::new();
        let component = fixture.component("backend");
        std::fs::write(
            fixture.directory.path().join("shipforge.yaml"),
            PROJECT.replace(old, new),
        )
        .unwrap();
        let error = component
            .planned
            .driver
            .prepare(
                &DeploymentId::new(),
                &component.planned.context,
                &component.planned.plan,
                &component.package,
                &NoEvents,
            )
            .await
            .unwrap_err();
        assert_eq!(error.stage, "prepare");
        assert!(error.message.contains("Project configuration"));
        assert!(fixture.actions().is_empty());
    }
}

#[tokio::test]
async fn missing_or_invalid_saved_configuration_blocks_rollback_without_exposing_contents() {
    for invalid in [None, Some("secret: dont-print-this\ninvalid: [")] {
        let fixture = Fixture::new();
        let component = fixture.component("backend");
        let path = fixture.directory.path().join("shipforge.yaml");
        if let Some(contents) = invalid {
            std::fs::write(path, contents).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
        let error = component
            .planned
            .driver
            .rollback(&DeploymentId::new(), &component.planned.context, None, None)
            .await
            .unwrap_err();
        assert_eq!(error.stage, "rollback");
        assert!(!error.to_string().contains("dont-print-this"));
        assert!(fixture.actions().is_empty());
    }
}

#[tokio::test]
async fn altered_execution_context_is_rejected_before_the_underlying_driver() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let mut context = component.planned.context.clone();
    context.endpoint_fingerprint = EndpointFingerprint::parse("a".repeat(64)).unwrap();
    assert!(
        component
            .planned
            .driver
            .rollback(&DeploymentId::new(), &context, None, None)
            .await
            .is_err()
    );
    assert!(fixture.actions().is_empty());
}

#[tokio::test]
async fn changed_credential_with_unchanged_endpoint_blocks_rollback_and_cleanup() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let context = &component.planned.context;
    let mut registry = DestinationRegistry::new();
    for (key, expected) in &fixture.guard.destinations {
        let mut settings = expected.settings.clone();
        if *key == context.destination {
            let DestinationSettings::LinuxSsh { credential, .. } = &mut settings;
            *credential = CredentialHandle::new();
        }
        let record = registry.create(key.clone(), settings).unwrap();
        assert_eq!(record.endpoint_fingerprint, expected.endpoint_fingerprint);
        assert_eq!(record.revision, expected.revision);
    }
    registry.save(&fixture.registry_path).unwrap();
    let error = component
        .planned
        .driver
        .rollback(&DeploymentId::new(), context, None, None)
        .await
        .unwrap_err();
    assert!(error.message.contains("Destination changed"));
    assert!(
        component
            .planned
            .driver
            .cleanup(
                context,
                &RetentionPolicy {
                    protected_versions: BTreeSet::new(),
                    retain_count: 1,
                    candidate: cleanup_candidate(&component),
                }
            )
            .await
            .is_err()
    );
    assert!(fixture.actions().is_empty());
}

#[tokio::test]
async fn disappearing_registry_refuses_mutations_but_keeps_frozen_observation_available() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let context = &component.planned.context;
    let current = fixture
        .inner
        .reference(context, ReleaseVersion::parse("v0").unwrap());
    fixture
        .inner
        .0
        .lock()
        .unwrap()
        .current
        .insert(context.component.clone(), current.clone());
    std::fs::remove_file(&fixture.registry_path).unwrap();
    assert!(
        component
            .planned
            .driver
            .activate(&DeploymentId::new(), context, &current)
            .await
            .is_err()
    );
    assert_eq!(
        component.planned.driver.current(context).await.unwrap(),
        Some(current)
    );
    assert!(fixture.actions().is_empty());
}

#[tokio::test]
async fn deployment_compensation_rechecks_config_and_records_failed_manual_recovery() {
    let fixture = Fixture::new();
    let history = HistoryStore::open(&fixture.directory.path().join("history.sqlite3")).unwrap();
    let components = vec![fixture.component("backend"), fixture.component("worker")];
    let order = components
        .iter()
        .map(|component| component.planned.context.component.clone())
        .collect::<Vec<_>>();
    fixture.inner.0.lock().unwrap().change_config_on_activation = Some((
        ComponentName::parse("worker").unwrap(),
        fixture.directory.path().join("shipforge.yaml"),
    ));
    let report = DeploymentOrchestrator::new(&history, Redactor::default())
        .deploy(components, &order, &NoEvents, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fixture.actions(),
        [
            "prepare:backend",
            "prepare:worker",
            "activate:backend",
            "activate:worker"
        ]
    );
    assert_eq!(report.compensation_failures.len(), 2);
    for name in order {
        assert_eq!(
            report.deployment.components[&name].outcome,
            ComponentOutcome::CompensationFailed
        );
        assert!(
            report.compensation_failures[&name]
                .message
                .contains("Project configuration changed")
        );
    }
    let results = history.component_results(&report.deployment.id).unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|result| result.result.outcome == ComponentOutcome::CompensationFailed)
    );
}
