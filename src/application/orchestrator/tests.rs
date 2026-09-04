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
    cancel_activation_error: bool,
    fail_rollback: Option<ComponentName>,
    fail_rollback_after_switch: Option<ComponentName>,
    fail_current: Option<ComponentName>,
    hang_current: bool,
    observation_tokens_cancelled: Vec<bool>,
    wrong_prepare: Option<ComponentName>,
    wrong_activation: Option<ComponentName>,
    cancel_after_activate: Option<ComponentName>,
    cancel_during_prepare: bool,
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
        let (fails, hangs) = {
            let mut state = self.state.lock().unwrap();
            state
                .observation_tokens_cancelled
                .push(context.cancellation.is_cancelled());
            (
                state.fail_current.as_ref() == Some(&context.component),
                state.hang_current,
            )
        };
        if hangs {
            std::future::pending::<()>().await;
        }
        if context.cancellation.is_cancelled() || fails {
            return Err(Self::error("observe", &context.component));
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
        context: &ComponentExecutionContext,
        plan: &crate::drivers::ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        self.record("prepare", &context.component);
        if self.state.lock().unwrap().cancel_during_prepare {
            self.cancellation.cancel();
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                context.cancellation.cancelled(),
            )
            .await
            .expect("execution cancellation must reach the in-flight Driver");
            return Err(Self::error("prepare", &context.component));
        }
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
            if self.state.lock().unwrap().cancel_activation_error {
                self.cancellation.cancel();
            }
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
        expected_current: Option<&ReleaseRef>,
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
        if state.current.get(&context.component) != expected_current {
            return Err(Self::error("rollback-drift", &context.component));
        }
        if let Some(release) = release {
            state
                .current
                .insert(context.component.clone(), release.clone());
        } else {
            state.current.remove(&context.component);
        }
        if state.fail_rollback_after_switch.as_ref() == Some(&context.component) {
            return Err(Self::error("rollback-after-switch", &context.component));
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
    directory: tempfile::TempDir,
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
            directory,
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
                notices: Vec::new(),
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
async fn history_reopens_frozen_component_packages_receipts_and_timed_steps() {
    let fixture = Fixture::new();
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert!(report.warnings.is_empty());
    let reopened = HistoryStore::open(&fixture.directory.path().join("history.sqlite3")).unwrap();
    let id = &report.deployment.id;
    let snapshots = reopened.component_snapshots(id).unwrap();
    assert_eq!(
        snapshots
            .iter()
            .map(|snapshot| snapshot.release.component.as_str())
            .collect::<Vec<_>>(),
        ["worker", "backend", "frontend"]
    );
    for snapshot in &snapshots {
        assert_eq!(snapshot.release.generation, ComponentGeneration::INITIAL);
        assert_eq!(
            snapshot.release.destination_revision,
            DestinationRevision::INITIAL
        );
        assert_eq!(snapshot.target.as_ref(), Some(&snapshot.release));
        assert!(snapshot.expected_current.is_none());
    }
    let packages = reopened.release_packages(id).unwrap();
    assert_eq!(packages.len(), 3);
    for package in packages {
        assert_eq!(package.sha256, "a".repeat(64));
        assert_eq!(package.size, 1);
        assert_eq!(package.release.version, package.manifest.version);
    }
    assert_eq!(reopened.release_receipts(id).unwrap().len(), 3);
    let observed = reopened.observations(id).unwrap();
    assert_eq!(observed.len(), 3);
    assert!(
        observed
            .iter()
            .all(|row| matches!(&row.observed, Ok(Some(_))) && row.healthy == Some(true))
    );
    let steps = reopened.steps(id).unwrap();
    assert_eq!(steps.len(), 9);
    for step in steps {
        if step.name == "compensate" {
            assert_eq!(step.status, crate::history::StepStatus::Skipped);
            assert!(step.started_at_ms.is_none());
        } else {
            assert_eq!(step.status, crate::history::StepStatus::Succeeded);
            assert!(step.started_at_ms.unwrap() <= step.completed_at_ms.unwrap());
        }
    }
}

#[tokio::test]
async fn prepare_failure_preserves_plan_and_skips_unattempted_activation() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_prepare = Some(ComponentName::parse("backend").unwrap());
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        fixture
            .history
            .component_snapshots(&report.deployment.id)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        fixture
            .history
            .release_packages(&report.deployment.id)
            .unwrap()
            .len(),
        3
    );
    assert!(
        fixture
            .history
            .release_receipts(&report.deployment.id)
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .history
            .observations(&report.deployment.id)
            .unwrap()
            .is_empty()
    );
    let steps = fixture.history.steps(&report.deployment.id).unwrap();
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.status == crate::history::StepStatus::Failed)
            .count(),
        1
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.status == crate::history::StepStatus::Skipped)
            .count(),
        8
    );
}

fn inject_history_failure(fixture: &Fixture, sql: &str) {
    rusqlite::Connection::open(fixture.directory.path().join("history.sqlite3"))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[tokio::test]
async fn activation_outcome_write_failure_compensates_before_returning_known_report() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_activation_outcome BEFORE UPDATE ON operation_intents WHEN OLD.stage='activate' BEGIN SELECT RAISE(ABORT,'injected history failure'); END;",
    );
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(
        fixture.actions(),
        [
            "prepare:backend",
            "prepare:frontend",
            "prepare:worker",
            "activate:worker",
            "rollback:worker"
        ]
    );
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
    assert!(!report.warnings.is_empty());
    assert!(fixture.state.lock().unwrap().current.is_empty());
    assert_eq!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .len(),
        1
    );
    let observations = fixture.history.observations(&report.deployment.id).unwrap();
    assert!(
        observations
            .iter()
            .any(|row| row.stage == "compensate.receipt"
                && row.observed == Ok(None)
                && row.healthy == Some(true))
    );
}

#[tokio::test]
async fn observation_write_failure_stops_forward_deploy_without_losing_compensation() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_observation BEFORE INSERT ON deployment_observations WHEN NEW.stage='activate.receipt' BEGIN SELECT RAISE(ABORT,'injected observation failure'); END;",
    );
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(!report.warnings.is_empty());
    assert_eq!(fixture.actions().last().unwrap(), "rollback:worker");
    assert!(!fixture.actions().contains(&"activate:backend".into()));
    assert!(fixture.state.lock().unwrap().current.is_empty());
}

#[tokio::test]
async fn later_activation_intent_failure_recovers_preceding_component() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_intent BEFORE INSERT ON operation_intents WHEN NEW.stage='activate' AND NEW.component='backend' BEGIN SELECT RAISE(ABORT,'injected pre-effect failure'); END;",
    );
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(fixture.actions().last().unwrap(), "rollback:worker");
    assert!(!fixture.actions().contains(&"activate:backend".into()));
    assert!(fixture.state.lock().unwrap().current.is_empty());
}

#[tokio::test]
async fn recovery_outcome_write_failure_does_not_skip_remaining_compensation() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_activate = Some(ComponentName::parse("frontend").unwrap());
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_recovery_outcome BEFORE UPDATE ON operation_intents WHEN OLD.stage='compensate' BEGIN SELECT RAISE(ABORT,'injected recovery history failure'); END;",
    );
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        &fixture.actions()[6..],
        ["rollback:backend", "rollback:worker"]
    );
    assert!(fixture.state.lock().unwrap().current.is_empty());
    assert!(report.compensation_failures.is_empty());
    assert_eq!(report.warnings.len(), 2);
    assert!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .iter()
            .all(|intent| intent.target == "not_deployed")
    );
}

#[tokio::test]
async fn compensation_intent_names_the_actual_previous_release() {
    let fixture = Fixture::new();
    let mut component = fixture.component("worker");
    let name = component.planned.context.component.clone();
    let previous = fixture.driver.release(
        &component.planned.context,
        ReleaseVersion::parse("previous").unwrap(),
    );
    component.planned.plan.expected_current = Some(previous.clone());
    fixture
        .state
        .lock()
        .unwrap()
        .current
        .insert(name.clone(), previous);
    fixture.state.lock().unwrap().cancel_after_activate = Some(name.clone());
    let report = DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(vec![component], &[name], &NoEvents, &fixture.cancellation)
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Cancelled);
    let connection =
        rusqlite::Connection::open(fixture.directory.path().join("history.sqlite3")).unwrap();
    let target: String = connection
        .query_row(
            "SELECT target FROM operation_intents WHERE stage='compensate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(target, "previous");
}

#[tokio::test]
async fn unknown_post_failure_observation_is_never_confirmed_absence() {
    let fixture = Fixture::new();
    let name = ComponentName::parse("worker").unwrap();
    fixture.state.lock().unwrap().fail_activate = Some(name.clone());
    fixture.state.lock().unwrap().fail_current = Some(name);
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    let observations = fixture.history.observations(&report.deployment.id).unwrap();
    assert_eq!(observations.len(), 2);
    assert!(
        observations
            .iter()
            .all(|row| row.observed.is_err() && row.healthy.is_none())
    );
}

#[tokio::test]
async fn terminal_history_failure_preserves_successful_remote_outcomes_with_warning() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_terminal BEFORE UPDATE ON deployments WHEN NEW.state='succeeded' BEGIN SELECT RAISE(ABORT,'injected terminal history failure'); END;",
    );
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(fixture.state.lock().unwrap().current.len(), 3);
    assert!(!report.warnings.is_empty());
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

#[tokio::test]
async fn snapshot_failure_rejects_all_effects() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_snapshot BEFORE INSERT ON component_snapshots BEGIN SELECT RAISE(ABORT,'injected snapshot failure'); END;",
    );
    let component = fixture.component("worker");
    let result = DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(
            vec![component],
            &[ComponentName::parse("worker").unwrap()],
            &NoEvents,
            &fixture.cancellation,
        )
        .await;
    assert!(result.is_err());
    assert!(fixture.actions().is_empty());
}

#[tokio::test]
async fn package_history_failure_finalizes_direct_deployment_before_any_remote_effect() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_package BEFORE INSERT ON release_packages BEGIN SELECT RAISE(ABORT,'injected package history failure'); END;",
    );
    let result = DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(
            vec![fixture.component("worker")],
            &[ComponentName::parse("worker").unwrap()],
            &NoEvents,
            &fixture.cancellation,
        )
        .await;
    let Err(OrchestrationError::Execution { deployment, .. }) = result else {
        panic!("error must identify the failed Deployment");
    };
    assert_eq!(
        fixture
            .history
            .deployment(&deployment)
            .unwrap()
            .unwrap()
            .state,
        DeploymentState::Failed
    );
    assert!(fixture.actions().is_empty());
    assert!(
        fixture
            .history
            .pending_intents(&deployment)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn prepared_receipt_history_failure_completes_failed_step_and_never_activates() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_receipt BEFORE INSERT ON release_receipts BEGIN SELECT RAISE(ABORT,'injected receipt history failure'); END;",
    );
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(fixture.actions(), ["prepare:backend"]);
    assert!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .is_empty()
    );
    let steps = fixture.history.steps(&report.deployment.id).unwrap();
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.status == crate::history::StepStatus::Failed)
            .count(),
        1
    );
    assert!(
        steps
            .iter()
            .filter(|step| step.name == "activate")
            .all(|step| step.status == crate::history::StepStatus::Skipped)
    );
}

#[tokio::test]
async fn execution_cancellation_replaces_the_read_only_plan_token() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().cancel_during_prepare = true;
    let planning = CancellationToken::new();
    let mut component = fixture.component("backend");
    component.planned.context.cancellation = planning.clone();
    let report = DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(
            vec![component],
            &[ComponentName::parse("backend").unwrap()],
            &NoEvents,
            &fixture.cancellation,
        )
        .await
        .unwrap();
    assert!(!planning.is_cancelled());
    assert!(report.failure.is_some());
    assert_eq!(fixture.actions(), vec!["prepare:backend"]);
}

#[tokio::test]
async fn remote_orchestration_continues_the_persisted_build_deployment() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let orchestrator = DeploymentOrchestrator::new(&fixture.history, Redactor::default());
    let deployment = orchestrator
        .start_for_context(&component.planned.context)
        .unwrap();
    let id = deployment.id.clone();
    orchestrator
        .snapshot_components(&id, [&component.planned], true)
        .unwrap();
    let build = fixture
        .history
        .record_intent(
            &id,
            &component.planned.context.component,
            "build-package",
            "v1-backend",
            orchestrator.timestamp().unwrap(),
        )
        .unwrap();
    let reopened = HistoryStore::open(&fixture.directory.path().join("history.sqlite3")).unwrap();
    assert_eq!(reopened.pending_intents(&id).unwrap().len(), 1);
    fixture
        .history
        .complete_intent(
            build,
            IntentStatus::Succeeded,
            None,
            orchestrator.timestamp().unwrap(),
            &Redactor::default(),
        )
        .unwrap();
    let report = orchestrator
        .deploy_started(
            deployment,
            vec![component],
            &[ComponentName::parse("backend").unwrap()],
            &NoEvents,
            &fixture.cancellation,
        )
        .await
        .unwrap();
    assert_eq!(report.deployment.id, id);
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(reopened.pending_intents(&id).unwrap().is_empty());
    assert_eq!(reopened.component_results(&id).unwrap().len(), 1);
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
    let observations = fixture.history.observations(&report.deployment.id).unwrap();
    assert!(observations.iter().all(|row| row.healthy.is_none()));
    assert!(observations.iter().any(|row| row.observed.is_err()));
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

#[tokio::test]
async fn cancellation_after_an_uncertain_switch_observes_with_a_fresh_token() {
    let fixture = Fixture::new();
    let backend = ComponentName::parse("backend").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_activate_after_switch = Some(backend.clone());
        state.cancel_activation_error = true;
    }
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    assert_eq!(report.deployment.state, DeploymentState::Cancelled);
    assert_eq!(
        report.deployment.components[&backend].outcome,
        ComponentOutcome::Compensated
    );
    let state = fixture.state.lock().unwrap();
    assert_eq!(state.observation_tokens_cancelled, vec![false]);
    assert!(!state.current.contains_key(&backend));
    assert!(
        state
            .actions
            .ends_with(&["rollback:backend".into(), "rollback:worker".into()])
    );
}

#[tokio::test]
async fn failed_activation_observation_never_falls_back_to_a_planned_version() {
    let fixture = Fixture::new();
    let mut component = fixture.component("backend");
    let backend = component.planned.context.component.clone();
    let previous = fixture.driver.release(
        &component.planned.context,
        ReleaseVersion::parse("previous").unwrap(),
    );
    component.planned.plan.expected_current = Some(previous);
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_activate_after_switch = Some(backend.clone());
        state.cancel_activation_error = true;
        state.fail_current = Some(backend.clone());
    }
    let report = DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(
            vec![component],
            std::slice::from_ref(&backend),
            &NoEvents,
            &fixture.cancellation,
        )
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        report.deployment.components[&backend].outcome,
        ComponentOutcome::Failed
    );
    assert_eq!(
        report.deployment.components[&backend].observed_release,
        None
    );
    let diagnostic = report.failure.as_ref().unwrap().diagnostic();
    assert!(diagnostic.contains("current state is unknown"));
    assert!(diagnostic.contains("post-failure observation failed"));
    assert!(
        !fixture
            .actions()
            .iter()
            .any(|action| action.starts_with("rollback:"))
    );
    let persisted = fixture
        .history
        .component_results(&report.deployment.id)
        .unwrap();
    assert!(
        persisted[0]
            .error
            .as_ref()
            .unwrap()
            .contains("current state is unknown")
    );
}

#[tokio::test]
async fn observed_not_deployed_is_not_replaced_by_the_previous_plan() {
    let fixture = Fixture::new();
    let mut component = fixture.component("backend");
    let backend = component.planned.context.component.clone();
    component.planned.plan.expected_current = Some(fixture.driver.release(
        &component.planned.context,
        ReleaseVersion::parse("previous").unwrap(),
    ));
    fixture.state.lock().unwrap().fail_activate = Some(backend.clone());
    let report = DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(
            vec![component],
            std::slice::from_ref(&backend),
            &NoEvents,
            &fixture.cancellation,
        )
        .await
        .unwrap();
    assert_eq!(
        report.deployment.components[&backend].observed_release,
        None
    );
    assert_eq!(
        fixture.state.lock().unwrap().observation_tokens_cancelled,
        vec![false]
    );
}

#[tokio::test]
async fn failed_compensation_records_the_post_switch_observation() {
    let fixture = Fixture::new();
    let backend = ComponentName::parse("backend").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_activate = Some(ComponentName::parse("frontend").unwrap());
        state.fail_rollback_after_switch = Some(backend.clone());
    }
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    let result = &report.deployment.components[&backend];
    assert_eq!(result.outcome, ComponentOutcome::CompensationFailed);
    assert_eq!(result.observed_release, None);
    assert!(!fixture.state.lock().unwrap().current.contains_key(&backend));
    assert!(report.compensation_failures.contains_key(&backend));
}

#[tokio::test]
async fn failed_compensation_observation_retains_manual_recovery_diagnostics() {
    let fixture = Fixture::new();
    let backend = ComponentName::parse("backend").unwrap();
    {
        let mut state = fixture.state.lock().unwrap();
        state.fail_activate = Some(ComponentName::parse("frontend").unwrap());
        state.fail_rollback = Some(backend.clone());
        state.fail_current = Some(backend.clone());
    }
    let report = fixture.deploy(&["worker", "backend", "frontend"]).await;
    let result = &report.deployment.components[&backend];
    assert_eq!(result.outcome, ComponentOutcome::CompensationFailed);
    assert_eq!(result.observed_release, None);
    let error = &report.compensation_failures[&backend];
    assert!(error.message.contains("current state is unknown"));
    assert!(
        error
            .suggested_action
            .contains("inspect the remote current Release")
    );
}

#[tokio::test]
async fn recovery_observation_is_bounded_even_if_a_driver_does_not_return() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().hang_current = true;
    let error = observe_after_failure(&fixture.component("backend"), Duration::from_millis(1))
        .await
        .unwrap_err();
    assert_eq!(error.stage, "observe");
    assert!(error.message.contains("timed out"));
}
