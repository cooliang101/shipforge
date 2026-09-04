use std::{
    any::Any,
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::*;
use crate::{
    domain::{
        Capability, ComponentGeneration, ComponentRelease, DestinationKey, DestinationRevision,
        DriverCapabilities, EnvironmentId, ProjectId, ReleaseVersion,
    },
    drivers::{
        CleanupReport, CredentialHandle, DeploymentDriver, DriverDestinationInput, DriverKind,
        DriverLog, DriverTargetInput, EndpointFingerprint, PreflightReport, RetentionPolicy,
        ValidatedDestinationSettings, ValidatedTargetSettings,
    },
};

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
    fail_prepare: Option<ComponentName>,
    fail_activate: Option<ComponentName>,
    fail_activate_after_switch: Option<ComponentName>,
    fail_rollback: Option<ComponentName>,
    wrong_prepare: Option<ComponentName>,
    wrong_activation: Option<ComponentName>,
    cancel_after_activate: Option<ComponentName>,
    current: BTreeMap<ComponentName, ReleaseRef>,
}

#[derive(Debug)]
struct FakeDriver {
    state: Arc<Mutex<FakeState>>,
    cancellation: CancellationToken,
}

impl FakeDriver {
    fn record(&self, action: &str, component: &ComponentName) {
        self.state
            .lock()
            .unwrap()
            .actions
            .push(format!("{action}:{component}"));
    }

    fn release(&self, context: &ComponentExecutionContext, version: ReleaseVersion) -> ReleaseRef {
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

    fn error(stage: &str, component: &ComponentName) -> DriverError {
        DriverError {
            stage: stage.into(),
            target: component.to_string(),
            message: "injected failure".into(),
            suggested_action: "fix the test".into(),
        }
    }
}

#[async_trait]
impl DeploymentDriver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::parse("test-driver").unwrap()
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
        unreachable!()
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        _: &crate::drivers::ComponentRequest,
    ) -> Result<crate::drivers::ComponentPlan, DriverError> {
        unreachable!()
    }
    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .current
            .get(&context.component)
            .cloned())
    }
    async fn prepare(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        plan: &crate::drivers::ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        self.record("prepare", &context.component);
        if self.state.lock().unwrap().fail_prepare.as_ref() == Some(&context.component) {
            return Err(Self::error("prepare", &context.component));
        }
        let version =
            if self.state.lock().unwrap().wrong_prepare.as_ref() == Some(&context.component) {
                ReleaseVersion::parse("wrong-prepared").unwrap()
            } else {
                plan.release.version.clone()
            };
        Ok(PreparedRelease {
            release: self.release(context, version),
            already_active: false,
        })
    }
    async fn activate(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        self.record("activate", &context.component);
        if self.state.lock().unwrap().fail_activate.as_ref() == Some(&context.component) {
            return Err(Self::error("activate", &context.component));
        }
        if self
            .state
            .lock()
            .unwrap()
            .fail_activate_after_switch
            .as_ref()
            == Some(&context.component)
        {
            self.state
                .lock()
                .unwrap()
                .current
                .insert(context.component.clone(), release.clone());
            return Err(Self::error("activate", &context.component));
        }
        if self.state.lock().unwrap().cancel_after_activate.as_ref() == Some(&context.component) {
            self.cancellation.cancel();
        }
        let current =
            if self.state.lock().unwrap().wrong_activation.as_ref() == Some(&context.component) {
                Some(self.release(context, ReleaseVersion::parse("external").unwrap()))
            } else {
                Some(release.clone())
            };
        if let Some(current) = &current {
            self.state
                .lock()
                .unwrap()
                .current
                .insert(context.component.clone(), current.clone());
        }
        Ok(ActivationReceipt {
            current,
            healthy: true,
        })
    }
    async fn rollback(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        self.record("rollback", &context.component);
        if context.cancellation.is_cancelled() {
            return Err(Self::error("rollback-cancelled", &context.component));
        }
        if self.state.lock().unwrap().fail_rollback.as_ref() == Some(&context.component) {
            return Err(Self::error("rollback", &context.component));
        }
        let mut state = self.state.lock().unwrap();
        if let Some(release) = release {
            state
                .current
                .insert(context.component.clone(), release.clone());
        } else {
            state.current.remove(&context.component);
        }
        Ok(ActivationReceipt {
            current: release.cloned(),
            healthy: true,
        })
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        unreachable!()
    }
    async fn cleanup(
        &self,
        _: &ComponentExecutionContext,
        _: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        unreachable!()
    }
}

#[derive(Debug)]
struct NoEvents;
impl EventSink for NoEvents {
    fn emit(&self, _: DriverLog) {}
}

struct Fixture {
    _directory: tempfile::TempDir,
    history: HistoryStore,
    state: Arc<Mutex<FakeState>>,
    cancellation: CancellationToken,
    driver: Arc<FakeDriver>,
    project: ProjectId,
    environment: EnvironmentId,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(&directory.path().join("history.sqlite3")).unwrap();
        let state = Arc::new(Mutex::new(FakeState::default()));
        let cancellation = CancellationToken::new();
        let driver = Arc::new(FakeDriver {
            state: Arc::clone(&state),
            cancellation: cancellation.clone(),
        });
        Self {
            _directory: directory,
            history,
            state,
            cancellation,
            driver,
            project: ProjectId::new(),
            environment: EnvironmentId::new(),
        }
    }

    fn component(&self, name: &str) -> DeploymentComponent {
        let component = ComponentName::parse(name).unwrap();
        let version = ReleaseVersion::parse(format!("v1-{name}")).unwrap();
        let settings = Arc::new(Settings(self.driver.kind()));
        let context = ComponentExecutionContext {
            project_id: self.project.clone(),
            environment_id: self.environment.clone(),
            component: component.clone(),
            generation: ComponentGeneration::INITIAL,
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
            credential: CredentialHandle::new(),
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            destination_settings: settings.clone(),
            target: settings,
            cancellation: self.cancellation.clone(),
        };
        let release = ComponentRelease {
            project_id: self.project.clone(),
            environment_id: self.environment.clone(),
            component,
            generation: ComponentGeneration::INITIAL,
            version,
            destination: context.destination.clone(),
            destination_revision: DestinationRevision::INITIAL,
        };
        let capabilities = self.driver.static_capabilities();
        DeploymentComponent {
            planned: PlannedComponent {
                driver: self.driver.clone(),
                context,
                plan: crate::drivers::ComponentPlan {
                    release: release.clone(),
                    effective_capabilities: capabilities,
                    expected_current: None,
                    driver_steps: Vec::new(),
                },
            },
            package: ReleasePackage::new(
                release,
                PathBuf::from("unused.tar.gz"),
                "a".repeat(64),
                1,
            ),
        }
    }

    async fn deploy(&self, order: &[&str]) -> DeploymentReport {
        let components = ["frontend", "backend", "worker"]
            .map(|name| self.component(name))
            .into();
        let order = order
            .iter()
            .map(|name| ComponentName::parse(*name).unwrap())
            .collect::<Vec<_>>();
        DeploymentOrchestrator::new(&self.history, Redactor::default())
            .deploy(components, &order, &NoEvents, &self.cancellation)
            .await
            .unwrap()
    }

    fn actions(&self) -> Vec<String> {
        self.state.lock().unwrap().actions.clone()
    }
}

#[tokio::test]
async fn prepares_every_component_before_topological_activation() {
    let fixture = Fixture::new();
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        fixture.actions(),
        vec![
            "prepare:backend",
            "prepare:frontend",
            "prepare:worker",
            "activate:worker",
            "activate:backend",
            "activate:frontend",
        ]
    );
    assert_eq!(
        fixture
            .history
            .component_results(&report.deployment.id)
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn prepare_failure_prevents_every_activation() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_prepare = Some(ComponentName::parse("frontend").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        fixture.actions(),
        vec!["prepare:backend", "prepare:frontend"]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("frontend").unwrap()].outcome,
        ComponentOutcome::Failed
    );
}

#[tokio::test]
async fn activation_failure_compensates_successes_in_reverse_actual_order() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_activate = Some(ComponentName::parse("frontend").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(
        fixture.actions(),
        vec![
            "prepare:backend",
            "prepare:frontend",
            "prepare:worker",
            "activate:worker",
            "activate:backend",
            "activate:frontend",
            "rollback:backend",
            "rollback:worker",
        ]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("backend").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn partial_activation_failure_is_observed_and_compensated_first() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_activate_after_switch =
        Some(ComponentName::parse("backend").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(
        fixture.actions(),
        vec![
            "prepare:backend",
            "prepare:frontend",
            "prepare:worker",
            "activate:worker",
            "activate:backend",
            "rollback:backend",
            "rollback:worker",
        ]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("backend").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn compensation_failure_preserves_manual_intervention_state() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_activate = Some(ComponentName::parse("frontend").unwrap());
    fixture.state.lock().unwrap().fail_rollback = Some(ComponentName::parse("backend").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(
        report.deployment.components[&ComponentName::parse("backend").unwrap()].outcome,
        ComponentOutcome::CompensationFailed
    );
    assert!(
        report
            .compensation_failures
            .contains_key(&ComponentName::parse("backend").unwrap())
    );
}

#[tokio::test]
async fn rejects_a_prepare_receipt_that_changes_release_identity() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().wrong_prepare = Some(ComponentName::parse("backend").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(fixture.actions(), vec!["prepare:backend"]);
}

#[tokio::test]
async fn activation_receipt_drift_is_failed_without_blind_rollback() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().wrong_activation = Some(ComponentName::parse("worker").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    let worker = &report.deployment.components[&ComponentName::parse("worker").unwrap()];
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(worker.outcome, ComponentOutcome::Failed);
    assert_eq!(
        worker.observed_release.as_ref().unwrap().as_str(),
        "external"
    );
    assert!(
        !fixture
            .actions()
            .iter()
            .any(|action| action.starts_with("rollback:"))
    );
}

#[tokio::test]
async fn cancellation_uses_a_fresh_token_for_compensation() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().cancel_after_activate =
        Some(ComponentName::parse("worker").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Cancelled);
    assert!(
        fixture
            .actions()
            .ends_with(&["activate:worker".into(), "rollback:worker".into()])
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[test]
fn rejects_an_order_that_omits_a_selected_component() {
    let fixture = Fixture::new();
    let components = vec![fixture.component("backend"), fixture.component("worker")];
    let result = validate_components(components, &[ComponentName::parse("backend").unwrap()]);
    assert!(matches!(result, Err(OrchestrationError::InvalidInput(_))));
}
