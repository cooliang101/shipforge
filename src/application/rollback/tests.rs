use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::*;
use crate::{
    domain::{
        ComponentGeneration, DestinationKey, DestinationRevision, DriverCapabilities,
        EnvironmentId, ProjectId, ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, CredentialHandle, DriverDestinationInput, DriverKind,
        DriverLog, DriverTargetInput, EndpointFingerprint, EventSink, PreflightReport,
        PreparedRelease, ReleasePackage, RetentionPolicy, ValidatedDestinationSettings,
        ValidatedTargetSettings,
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
    current: BTreeMap<ComponentName, ReleaseRef>,
    fail_component: Option<ComponentName>,
    partial_failure: bool,
    fail_compensation: Option<ComponentName>,
    cancel_after: Option<ComponentName>,
    cancel_on_error: bool,
    fail_observation_after_error: Option<ComponentName>,
    failed_observation: Option<ComponentName>,
    hang_observation: bool,
    observation_tokens_cancelled: Vec<bool>,
}

#[derive(Debug)]
struct FakeDriver {
    state: Arc<Mutex<FakeState>>,
    cancellation: CancellationToken,
}

impl FakeDriver {
    fn error(component: &ComponentName) -> DriverError {
        DriverError {
            stage: "rollback".into(),
            target: component.to_string(),
            message: "injected failure".into(),
            suggested_action: "restore the expected Release".into(),
        }
    }
}

#[async_trait]
impl DeploymentDriver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::parse("rollback-test").unwrap()
    }
    fn static_capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::new([Capability::Rollback])
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
        let (failed, hangs) = {
            let mut state = self.state.lock().unwrap();
            state
                .observation_tokens_cancelled
                .push(context.cancellation.is_cancelled());
            (
                state.failed_observation.as_ref() == Some(&context.component),
                state.hang_observation,
            )
        };
        if hangs {
            std::future::pending::<()>().await;
        }
        if failed || context.cancellation.is_cancelled() {
            let mut error = Self::error(&context.component);
            error.stage = "observe".into();
            return Err(error);
        }
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
        _: &ComponentExecutionContext,
        _: &crate::drivers::ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        unreachable!()
    }
    async fn activate(
        &self,
        _deployment: &crate::domain::DeploymentId,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        unreachable!()
    }
    async fn rollback(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        expected_current: Option<&ReleaseRef>,
        target: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        let target_name = target.map_or("not_deployed", |release| release.version.as_str());
        let mut state = self.state.lock().unwrap();
        if context.cancellation.is_cancelled() {
            return Err(Self::error(&context.component));
        }
        if state.current.get(&context.component) != expected_current {
            return Err(Self::error(&context.component));
        }
        state
            .actions
            .push(format!("rollback:{}->{target_name}", context.component));
        let restoring_original = target.is_some_and(|release| release.version.as_str() == "v3");
        if restoring_original && state.fail_compensation.as_ref() == Some(&context.component) {
            if state.fail_observation_after_error.as_ref() == Some(&context.component) {
                state.failed_observation = Some(context.component.clone());
            }
            return Err(Self::error(&context.component));
        }
        if !restoring_original && state.fail_component.as_ref() == Some(&context.component) {
            if state.partial_failure {
                if let Some(target) = target {
                    state
                        .current
                        .insert(context.component.clone(), target.clone());
                } else {
                    state.current.remove(&context.component);
                }
            }
            if state.fail_observation_after_error.as_ref() == Some(&context.component) {
                state.failed_observation = Some(context.component.clone());
            }
            if state.cancel_on_error {
                self.cancellation.cancel();
            }
            return Err(Self::error(&context.component));
        }
        if let Some(target) = target {
            state
                .current
                .insert(context.component.clone(), target.clone());
        } else {
            state.current.remove(&context.component);
        }
        if state.cancel_after.as_ref() == Some(&context.component) && !restoring_original {
            self.cancellation.cancel();
        }
        Ok(ActivationReceipt {
            current: target.cloned(),
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

struct Fixture {
    _directory: tempfile::TempDir,
    history: HistoryStore,
    source: DeploymentId,
    project: ProjectId,
    environment: EnvironmentId,
    driver: Arc<FakeDriver>,
    state: Arc<Mutex<FakeState>>,
    cancellation: CancellationToken,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(&directory.path().join("history.sqlite3")).unwrap();
        let source = DeploymentId::new();
        let project = ProjectId::new();
        let environment = EnvironmentId::new();
        history
            .create_deployment(&source, &project, &environment, 1)
            .unwrap();
        history
            .transition_deployment(
                &source,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        history
            .transition_deployment(
                &source,
                DeploymentState::Running,
                DeploymentState::Succeeded,
                3,
            )
            .unwrap();
        let state = Arc::new(Mutex::new(FakeState::default()));
        let cancellation = CancellationToken::new();
        let driver = Arc::new(FakeDriver {
            state: state.clone(),
            cancellation: cancellation.clone(),
        });
        Self {
            _directory: directory,
            history,
            source,
            project,
            environment,
            driver,
            state,
            cancellation,
        }
    }

    fn release(&self, context: &ComponentExecutionContext, version: &str) -> ReleaseRef {
        ReleaseRef {
            driver: self.driver.kind(),
            project_id: self.project.clone(),
            environment_id: self.environment.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version: ReleaseVersion::parse(version).unwrap(),
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            effective_capabilities: self.driver.static_capabilities(),
        }
    }

    fn component(&self, name: &str, target: Option<&str>) -> RollbackComponent {
        let settings = Arc::new(Settings(self.driver.kind()));
        let context = ComponentExecutionContext {
            project_id: self.project.clone(),
            environment_id: self.environment.clone(),
            component: ComponentName::parse(name).unwrap(),
            generation: ComponentGeneration::INITIAL,
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
            credential: CredentialHandle::new(),
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            destination_settings: settings.clone(),
            target: settings,
            cancellation: self.cancellation.clone(),
        };
        let expected_current = self.release(&context, "v3");
        self.state
            .lock()
            .unwrap()
            .current
            .insert(context.component.clone(), expected_current.clone());
        RollbackComponent {
            driver: self.driver.clone(),
            context: context.clone(),
            expected_current,
            target: target.map(|version| self.release(&context, version)),
        }
    }

    fn components(&self) -> Vec<RollbackComponent> {
        vec![
            self.component("database", Some("v1")),
            self.component("api", Some("v2")),
            self.component("worker", None),
        ]
    }

    async fn rollback(&self) -> RollbackReport {
        let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
        RollbackOrchestrator::new(&self.history, Redactor::default())
            .rollback(&self.source, self.components(), &order, &self.cancellation)
            .await
            .unwrap()
    }

    fn actions(&self) -> Vec<String> {
        self.state.lock().unwrap().actions.clone()
    }
}

#[tokio::test]
async fn rolls_back_in_reverse_topology_and_persists_not_deployed() {
    let fixture = Fixture::new();
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        fixture.actions(),
        vec![
            "rollback:worker->not_deployed",
            "rollback:api->v2",
            "rollback:database->v1"
        ]
    );
    let results = fixture
        .history
        .component_results(&report.deployment.id)
        .unwrap();
    let worker = results
        .iter()
        .find(|result| result.component.as_str() == "worker")
        .unwrap();
    assert_eq!(worker.result.attempted_release, None);
    assert_eq!(worker.result.observed_release, None);
}

#[tokio::test]
async fn preflight_drift_stops_before_side_effects() {
    let fixture = Fixture::new();
    let components = fixture.components();
    fixture
        .state
        .lock()
        .unwrap()
        .current
        .remove(&ComponentName::parse("api").unwrap());
    let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
    let report = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(&fixture.source, components, &order, &fixture.cancellation)
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(fixture.actions().is_empty());
}

#[tokio::test]
async fn later_failure_restores_applied_components_in_reverse_actual_order() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_component = Some(ComponentName::parse("database").unwrap());
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        fixture.actions(),
        vec![
            "rollback:worker->not_deployed",
            "rollback:api->v2",
            "rollback:database->v1",
            "rollback:api->v3",
            "rollback:worker->v3",
        ]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("api").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn partial_failure_is_observed_and_compensated() {
    let fixture = Fixture::new();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_component = Some(ComponentName::parse("api").unwrap());
        state.partial_failure = true;
    }
    let report = fixture.rollback().await;
    assert_eq!(
        fixture.actions(),
        vec![
            "rollback:worker->not_deployed",
            "rollback:api->v2",
            "rollback:api->v3",
            "rollback:worker->v3",
        ]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("api").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn compensation_failure_retains_manual_action() {
    let fixture = Fixture::new();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_component = Some(ComponentName::parse("database").unwrap());
        state.fail_compensation = Some(ComponentName::parse("api").unwrap());
    }
    let report = fixture.rollback().await;
    assert_eq!(
        report.deployment.components[&ComponentName::parse("api").unwrap()].outcome,
        ComponentOutcome::CompensationFailed
    );
    assert!(
        report
            .compensation_failures
            .contains_key(&ComponentName::parse("api").unwrap())
    );
}

#[tokio::test]
async fn cancellation_after_one_change_compensates_with_fresh_context() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().cancel_after = Some(ComponentName::parse("worker").unwrap());
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Cancelled);
    assert_eq!(
        fixture.actions(),
        vec!["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
}

#[tokio::test]
async fn unknown_state_is_never_treated_as_a_confirmed_undeployed_target() {
    let fixture = Fixture::new();
    let worker = ComponentName::parse("worker").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_component = Some(worker.clone());
        state.fail_observation_after_error = Some(worker.clone());
        state.cancel_on_error = true;
    }
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        report.deployment.components[&worker].outcome,
        ComponentOutcome::Failed
    );
    assert_eq!(report.deployment.components[&worker].observed_release, None);
    assert!(report.compensation_failures.is_empty());
    assert_eq!(fixture.actions(), vec!["rollback:worker->not_deployed"]);
    assert_eq!(
        fixture.state.lock().unwrap().current[&worker]
            .version
            .as_str(),
        "v3"
    );
    assert!(
        report
            .failure
            .as_ref()
            .unwrap()
            .diagnostic()
            .contains("current state is unknown")
    );
    let persisted = fixture
        .history
        .component_results(&report.deployment.id)
        .unwrap();
    let result = persisted
        .iter()
        .find(|result| result.component == worker)
        .unwrap();
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .contains("inspect the remote current Release")
    );
}

#[tokio::test]
async fn cancelled_partial_rollback_is_observed_and_restored_with_independent_tokens() {
    let fixture = Fixture::new();
    let worker = ComponentName::parse("worker").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_component = Some(worker.clone());
        state.partial_failure = true;
        state.cancel_on_error = true;
    }
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Cancelled);
    assert_eq!(
        report.deployment.components[&worker].outcome,
        ComponentOutcome::Compensated
    );
    assert_eq!(
        fixture.actions(),
        vec!["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.current[&worker].version.as_str(), "v3");
    assert!(
        !state
            .observation_tokens_cancelled
            .iter()
            .any(|cancelled| *cancelled)
    );
}

#[tokio::test]
async fn failed_compensation_observation_preserves_unknown_state_and_manual_context() {
    let fixture = Fixture::new();
    let api = ComponentName::parse("api").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_component = Some(ComponentName::parse("database").unwrap());
        state.fail_compensation = Some(api.clone());
        state.fail_observation_after_error = Some(api.clone());
    }
    let report = fixture.rollback().await;
    let result = &report.deployment.components[&api];
    assert_eq!(result.outcome, ComponentOutcome::CompensationFailed);
    assert_eq!(result.observed_release, None);
    let error = &report.compensation_failures[&api];
    assert!(error.message.contains("current state is unknown"));
    assert!(
        error
            .suggested_action
            .contains("inspect the remote current Release")
    );
}

#[tokio::test]
async fn cancellation_on_the_last_success_still_compensates_that_change() {
    let fixture = Fixture::new();
    let component = fixture.component("worker", None);
    let worker = component.context.component.clone();
    fixture.state.lock().unwrap().cancel_after = Some(worker.clone());
    let report = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(
            &fixture.source,
            vec![component],
            std::slice::from_ref(&worker),
            &fixture.cancellation,
        )
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Cancelled);
    assert_eq!(
        fixture.actions(),
        vec!["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    assert_eq!(
        report.deployment.components[&worker].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn execution_replaces_the_old_planning_token_before_preflight() {
    let fixture = Fixture::new();
    let mut component = fixture.component("worker", None);
    let worker = component.context.component.clone();
    let old_token = CancellationToken::new();
    old_token.cancel();
    component.context.cancellation = old_token;
    let report = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(
            &fixture.source,
            vec![component],
            std::slice::from_ref(&worker),
            &fixture.cancellation,
        )
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(
        !fixture
            .state
            .lock()
            .unwrap()
            .observation_tokens_cancelled
            .iter()
            .any(|cancelled| *cancelled)
    );
}

#[tokio::test]
async fn failed_rollback_observation_has_a_finite_deadline() {
    let fixture = Fixture::new();
    let component = fixture.component("worker", None);
    fixture.state.lock().unwrap().hang_observation = true;
    let error = observe_after_failure(&component, Duration::from_millis(1))
        .await
        .unwrap_err();
    assert_eq!(error.stage, "observe");
    assert!(error.message.contains("timed out"));
}
