use std::{
    any::Any,
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::*;
mod events;
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
    audit_warning: Option<String>,
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
    async fn current_with_events(
        &self,
        context: &ComponentExecutionContext,
        events: &dyn EventSink,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        let result = self.current(context).await;
        if result.is_err() {
            events::failed_observation(events, &context.component);
        }
        result
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
            warnings: state.audit_warning.iter().cloned().collect(),
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
    directory: tempfile::TempDir,
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
            directory,
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

    fn inject_history_failure(&self, sql: &str) {
        rusqlite::Connection::open(self.directory.path().join("history.sqlite3"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
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
async fn remote_audit_warnings_do_not_fail_an_explicit_rollback() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().audit_warning = Some("remote audit unavailable".into());
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.failure.is_none());
    assert_eq!(report.warnings.len(), 3);
    assert_eq!(fixture.actions().len(), 3);
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
    let records = crate::application::step_events::tests::Records::default();
    let error = observe_after_failure(&component, Duration::from_millis(1), &records)
        .await
        .unwrap_err();
    assert_eq!(error.stage, "observe");
    assert!(error.message.contains("timed out"));
    assert!(matches!(
        records.0.lock().unwrap()[0].kind,
        crate::telemetry::log_record::LogEventKind::CommandUnavailable { .. }
    ));
}

#[tokio::test]
async fn frozen_rollback_refs_execution_order_and_evidence_survive_reopen() {
    let fixture = Fixture::new();
    let components = fixture.components();
    let expected = components
        .iter()
        .rev()
        .enumerate()
        .map(|(index, component)| DeploymentComponentSnapshot {
            release: component
                .target
                .as_ref()
                .unwrap_or(&component.expected_current)
                .clone(),
            expected_current: Some(component.expected_current.clone()),
            target: component.target.clone(),
            execution_order: u32::try_from(index).unwrap(),
        })
        .collect::<Vec<_>>();
    let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
    let report = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(&fixture.source, components, &order, &fixture.cancellation)
        .await
        .unwrap();
    assert!(report.warnings.is_empty());
    let reopened = HistoryStore::open(&fixture.directory.path().join("history.sqlite3")).unwrap();
    assert_eq!(
        reopened.component_snapshots(&report.deployment.id).unwrap(),
        expected
    );
    let observations = reopened.observations(&report.deployment.id).unwrap();
    let worker = observations
        .iter()
        .filter(|observation| observation.component.as_str() == "worker")
        .collect::<Vec<_>>();
    assert_eq!(worker.len(), 3);
    assert_eq!(worker[0].stage, "rollback-preflight");
    assert_eq!(worker[0].observed, Ok(expected[0].expected_current.clone()));
    assert_eq!(worker[0].healthy, None);
    assert_eq!(worker[1].stage, "rollback-before-mutation");
    assert_eq!(worker[1].healthy, None);
    assert_eq!(worker[2].stage, "rollback-receipt");
    assert_eq!(worker[2].observed, Ok(None));
    assert_eq!(worker[2].healthy, Some(true));
    assert!(
        observations
            .windows(2)
            .all(|pair| pair[0].observed_at_ms < pair[1].observed_at_ms)
    );
    let steps = reopened.steps(&report.deployment.id).unwrap();
    assert_eq!(steps.len(), 6);
    for step in steps {
        assert!(step.planned);
        assert_eq!(
            step.status,
            if step.name == "rollback" {
                crate::history::StepStatus::Succeeded
            } else {
                crate::history::StepStatus::Skipped
            }
        );
    }
}

#[tokio::test]
async fn history_distinguishes_failed_observation_from_confirmed_absence() {
    let fixture = Fixture::new();
    let worker = ComponentName::parse("worker").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_component = Some(worker.clone());
        state.fail_observation_after_error = Some(worker.clone());
    }
    let report = fixture.rollback().await;
    let observations = fixture.history.observations(&report.deployment.id).unwrap();
    let failure = observations
        .iter()
        .find(|observation| observation.stage == "rollback-after-failure")
        .unwrap();
    assert_eq!(failure.component, worker);
    assert!(failure.observed.is_err());
    assert_eq!(failure.healthy, None);
    assert!(
        observations
            .iter()
            .all(|observation| observation.observed != Ok(None))
    );
    assert_eq!(
        fixture
            .history
            .component_snapshots(&report.deployment.id)
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn confirmed_absence_during_preflight_is_not_inferred_to_be_healthy() {
    let fixture = Fixture::new();
    let components = fixture.components();
    fixture
        .state
        .lock()
        .unwrap()
        .current
        .remove(&ComponentName::parse("worker").unwrap());
    let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
    let report = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(&fixture.source, components, &order, &fixture.cancellation)
        .await
        .unwrap();
    let observations = fixture.history.observations(&report.deployment.id).unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].observed, Ok(None));
    assert_eq!(observations[0].healthy, None);
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(fixture.actions().is_empty());
}

#[tokio::test]
async fn failed_snapshot_write_prevents_even_preflight_and_all_effects() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_snapshot BEFORE INSERT ON component_snapshots WHEN NEW.component='api' BEGIN SELECT RAISE(FAIL,'snapshot failure'); END;");
    let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
    let error = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(
            &fixture.source,
            fixture.components(),
            &order,
            &fixture.cancellation,
        )
        .await
        .unwrap_err();
    let diagnostic = error.to_string();
    let OrchestrationError::Execution {
        deployment,
        source,
        persistence,
    } = error
    else {
        panic!("initialization failure must retain the Deployment ID");
    };
    assert!(diagnostic.contains(&deployment.to_string()));
    assert!(matches!(*source, OrchestrationError::History(_)));
    assert!(source.to_string().contains("snapshot failure"));
    assert!(persistence.is_none());
    let recorded = fixture.history.deployment(&deployment).unwrap().unwrap();
    assert_eq!(recorded.state, DeploymentState::Cancelled);
    assert_eq!(recorded.related_deployment, Some(fixture.source.clone()));
    assert!(
        fixture
            .history
            .component_snapshots(&deployment)
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .history
            .pending_intents(&deployment)
            .unwrap()
            .is_empty()
    );
    assert!(fixture.actions().is_empty());
    assert!(
        fixture
            .state
            .lock()
            .unwrap()
            .observation_tokens_cancelled
            .is_empty()
    );
}

#[tokio::test]
async fn initialization_and_cancellation_persistence_failures_retain_both_errors_and_id() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_metadata BEFORE INSERT ON deployment_metadata BEGIN SELECT RAISE(FAIL,'metadata failure'); END; CREATE TRIGGER fail_cancel BEFORE UPDATE OF state ON deployments WHEN OLD.kind='rollback' AND NEW.state='cancelled' BEGIN SELECT RAISE(FAIL,'cancellation persistence failure'); END;");
    let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
    let error = RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .rollback(
            &fixture.source,
            fixture.components(),
            &order,
            &fixture.cancellation,
        )
        .await
        .unwrap_err();
    let diagnostic = error.to_string();
    let OrchestrationError::Execution {
        deployment,
        source,
        persistence,
    } = error
    else {
        panic!("initialization failure must retain the Deployment ID");
    };
    assert!(diagnostic.contains(&deployment.to_string()));
    assert!(source.to_string().contains("metadata failure"));
    assert!(
        persistence
            .unwrap()
            .contains("initialization cancellation could not be persisted")
    );
    let recorded = fixture.history.deployment(&deployment).unwrap().unwrap();
    assert_eq!(recorded.state, DeploymentState::Created);
    assert_eq!(recorded.related_deployment, Some(fixture.source.clone()));
    assert_eq!(
        fixture
            .history
            .component_snapshots(&deployment)
            .unwrap()
            .len(),
        3
    );
    assert!(
        fixture
            .history
            .pending_intents(&deployment)
            .unwrap()
            .is_empty()
    );
    assert!(fixture.actions().is_empty());
    assert!(
        fixture
            .state
            .lock()
            .unwrap()
            .observation_tokens_cancelled
            .is_empty()
    );
}

#[tokio::test]
async fn preflight_observation_persistence_failure_forbids_forward_effects() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_observation BEFORE INSERT ON deployment_observations WHEN NEW.stage='rollback-preflight' BEGIN SELECT RAISE(FAIL,'observation failure'); END;");
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(fixture.actions().is_empty());
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("rollback-preflight"));
}

#[tokio::test]
async fn receipt_observation_failure_still_compensates_the_known_effect() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_observation BEFORE INSERT ON deployment_observations WHEN NEW.stage='rollback-receipt' BEGIN SELECT RAISE(FAIL,'receipt observation failure'); END;");
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        fixture.actions(),
        ["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
    assert!(report.compensation_failures.is_empty());
    assert_eq!(report.warnings.len(), 1);
}

#[tokio::test]
async fn intent_completion_failure_does_not_erase_effect_evidence() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_completion BEFORE UPDATE OF status ON operation_intents WHEN OLD.stage='rollback' BEGIN SELECT RAISE(FAIL,'intent completion failure'); END;");
    let report = fixture.rollback().await;
    assert_eq!(
        fixture.actions(),
        ["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("complete Rollback intent"))
    );
    assert_eq!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn failed_later_forward_intent_compensates_prior_effects_without_unjournaled_write() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_forward_intent BEFORE INSERT ON operation_intents WHEN NEW.stage='rollback' AND NEW.component='api' BEGIN SELECT RAISE(FAIL,'intent failure'); END;");
    let report = fixture.rollback().await;
    assert_eq!(
        fixture.actions(),
        ["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(report.compensation_failures.is_empty());
    assert_eq!(report.warnings.len(), 1);
}

#[tokio::test]
async fn failed_compensation_intent_skips_only_that_effect_and_recovers_other_components() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_component = Some(ComponentName::parse("database").unwrap());
    fixture.inject_history_failure("CREATE TRIGGER fail_compensation_intent BEFORE INSERT ON operation_intents WHEN NEW.stage='compensate' AND NEW.component='api' BEGIN SELECT RAISE(FAIL,'compensation intent failure'); END;");
    let report = fixture.rollback().await;
    assert_eq!(
        fixture.actions(),
        [
            "rollback:worker->not_deployed",
            "rollback:api->v2",
            "rollback:database->v1",
            "rollback:worker->v3"
        ]
    );
    let api = ComponentName::parse("api").unwrap();
    assert_eq!(
        report.deployment.components[&api].outcome,
        ComponentOutcome::CompensationFailed
    );
    assert_eq!(
        report.deployment.components[&api]
            .observed_release
            .as_ref()
            .unwrap()
            .as_str(),
        "v2"
    );
    assert!(
        report.compensation_failures[&api]
            .message
            .contains("no mutation attempted")
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn auxiliary_compensation_write_failures_do_not_interrupt_other_recovery() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_component = Some(ComponentName::parse("database").unwrap());
    fixture.inject_history_failure("CREATE TRIGGER fail_recovery_observation BEFORE INSERT ON deployment_observations WHEN NEW.stage LIKE 'compensation-%' BEGIN SELECT RAISE(FAIL,'recovery observation failure'); END; CREATE TRIGGER fail_recovery_completion BEFORE UPDATE OF status ON operation_intents WHEN OLD.stage='compensate' BEGIN SELECT RAISE(FAIL,'recovery completion failure'); END;");
    let report = fixture.rollback().await;
    assert!(report.compensation_failures.is_empty());
    for name in ["api", "worker"] {
        assert_eq!(
            report.deployment.components[&ComponentName::parse(name).unwrap()].outcome,
            ComponentOutcome::Compensated
        );
    }
    assert_eq!(report.warnings.len(), 6);
    assert!(
        fixture
            .actions()
            .ends_with(&["rollback:api->v3".into(), "rollback:worker->v3".into()])
    );
}

#[tokio::test]
async fn terminal_persistence_failure_retains_known_successful_rollback_report() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_results BEFORE INSERT ON component_results BEGIN SELECT RAISE(FAIL,'UNREGISTERED_SQL_SECRET /private/history.sqlite3 result failure'); END; CREATE TRIGGER fail_terminal BEFORE UPDATE OF state ON deployments WHEN OLD.kind='rollback' AND NEW.state='succeeded' BEGIN SELECT RAISE(FAIL,'UNREGISTERED_SQL_SECRET /private/history.sqlite3 terminal failure'); END;");
    let report = fixture.rollback().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.failure.is_none());
    assert!(report.compensation_failures.is_empty());
    assert!(
        report
            .deployment
            .components
            .values()
            .all(|result| result.outcome == ComponentOutcome::Succeeded)
    );
    assert_eq!(report.warnings.len(), 4);
    assert_safe_history_warnings(&report);
    assert_eq!(
        fixture
            .history
            .deployment(&report.deployment.id)
            .unwrap()
            .unwrap()
            .state,
        DeploymentState::Running
    );
}

fn assert_safe_history_warnings(report: &RollbackReport) {
    assert!(!report.warnings.is_empty());
    for warning in &report.warnings {
        assert!(warning.contains("local history persistence failed"));
        assert!(warning.contains("inspect durable history before retrying"));
        assert!(!warning.contains("UNREGISTERED_SQL_SECRET"));
        assert!(!warning.contains("/private/history.sqlite3"));
    }
    let diagnostic = format!("{report:?}");
    assert!(!diagnostic.contains("UNREGISTERED_SQL_SECRET"));
    assert!(!diagnostic.contains("/private/history.sqlite3"));
}

#[tokio::test]
async fn sqlite_error_details_never_leak_while_compensation_and_pending_evidence_are_retained() {
    let fixture = Fixture::new();
    fixture.inject_history_failure(
        "CREATE TRIGGER fail_completion BEFORE UPDATE OF status ON operation_intents
         BEGIN SELECT RAISE(FAIL,'UNREGISTERED_SQL_SECRET /private/history.sqlite3 completion'); END;
         CREATE TRIGGER fail_compensation_observation BEFORE INSERT ON deployment_observations
         WHEN NEW.stage LIKE 'compensation-%'
         BEGIN SELECT RAISE(FAIL,'UNREGISTERED_SQL_SECRET /private/history.sqlite3 observation'); END;
         CREATE TRIGGER fail_results BEFORE INSERT ON component_results
         BEGIN SELECT RAISE(FAIL,'UNREGISTERED_SQL_SECRET /private/history.sqlite3 results'); END;
         CREATE TRIGGER fail_terminal BEFORE UPDATE OF state ON deployments
         WHEN OLD.kind='rollback' AND NEW.state='failed'
         BEGIN SELECT RAISE(FAIL,'UNREGISTERED_SQL_SECRET /private/history.sqlite3 terminal'); END;",
    );
    let report = fixture.rollback().await;
    assert_eq!(
        fixture.actions(),
        ["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(report.failure.is_some());
    assert!(report.compensation_failures.is_empty());
    let worker = ComponentName::parse("worker").unwrap();
    let result = &report.deployment.components[&worker];
    assert_eq!(result.outcome, ComponentOutcome::Compensated);
    assert_eq!(result.observed_release.as_ref().unwrap().as_str(), "v3");
    assert_eq!(
        fixture.state.lock().unwrap().current[&worker]
            .version
            .as_str(),
        "v3"
    );
    assert_eq!(report.warnings.len(), 8);
    assert_safe_history_warnings(&report);
    for operation in [
        "complete Rollback intent",
        "compensation-before-mutation",
        "compensation-receipt",
        "complete Rollback compensation intent",
        "persist Rollback Component result",
        "persist Rollback terminal state",
    ] {
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains(operation))
        );
    }
    let pending = fixture
        .history
        .pending_intents(&report.deployment.id)
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|intent| intent.component == worker));
    assert_eq!(
        pending
            .iter()
            .map(|intent| intent.stage.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["compensate", "rollback"])
    );
    assert!(
        fixture
            .history
            .component_results(&report.deployment.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture
            .history
            .deployment(&report.deployment.id)
            .unwrap()
            .unwrap()
            .state,
        DeploymentState::Running
    );
}
