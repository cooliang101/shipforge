use super::*;
use crate::{
    application::step_events::tests::Records,
    telemetry::log_record::{
        LogPersistence::{Recorded, Unconfirmed},
        LogStepState::{Failed, Started, Succeeded},
    },
};

pub(super) fn failed_observation(events: &dyn EventSink, component: &ComponentName) {
    use crate::telemetry::{
        CommandArgument, CommandSpec,
        log_record::{CommandLocation, LogEvent, LogEventKind},
    };
    let command = CommandSpec::structured(
        "fixture-observe",
        [
            CommandArgument::plain(component.as_str()),
            CommandArgument::sensitive("observation-fixture-secret"),
        ],
    )
    .unwrap();
    events.emit_record(LogEvent {
        namespace: "fixture.observe".into(),
        message: "Read-only observation command failed".into(),
        scope: None,
        kind: LogEventKind::FailedCommand {
            command: command
                .diagnostic_snapshot(CommandLocation::Remote)
                .unwrap(),
        },
    });
}

#[tokio::test]
async fn failed_rollback_and_compensation_observations_record_commands_without_inventing_absence() {
    use crate::telemetry::log_record::LogEventKind;
    for (name, step, stage) in [
        ("api", "rollback", "rollback-after-failure"),
        ("worker", "compensate", "compensation-after-failure"),
    ] {
        let fixture = Fixture::new();
        let component = ComponentName::parse(name).unwrap();
        {
            let mut state = fixture.state.lock().unwrap();
            state.fail_component = Some(ComponentName::parse("api").unwrap());
            state.fail_observation_after_error = Some(component.clone());
            if step == "compensate" {
                state.fail_compensation = Some(component.clone());
            }
        }
        let records = Records::default();
        let report = run(&fixture, &records).await;
        assert_eq!(report.deployment.state, DeploymentState::Failed);
        assert_eq!(
            report.deployment.components[&component].observed_release,
            None
        );
        let observations = fixture.history.observations(&report.deployment.id).unwrap();
        let observation = observations
            .iter()
            .find(|value| value.component == component && value.stage == stage)
            .unwrap();
        assert!(observation.observed.is_err());
        assert_eq!(observation.healthy, None);
        assert!(records.states(name, "observe").is_empty());
        let events = records.0.lock().unwrap();
        let failures: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.kind, LogEventKind::FailedCommand { .. }))
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].scope.as_ref().unwrap().component, component);
        assert_eq!(failures[0].scope.as_ref().unwrap().step, step);
        let LogEventKind::FailedCommand { command } = &failures[0].kind else {
            unreachable!()
        };
        assert_eq!(command.program, "fixture-observe");
        assert_eq!(command.args, [name, "[REDACTED]"]);
        assert!(
            !serde_json::to_string(&*events)
                .unwrap()
                .contains("observation-fixture-secret")
        );
    }
}

#[tokio::test]
async fn execution_preflight_observation_emits_a_command_but_no_unstarted_rollback_step() {
    use crate::telemetry::log_record::LogEventKind;
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().failed_observation =
        Some(ComponentName::parse("worker").unwrap());
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(fixture.actions().is_empty());
    assert!(records.states("worker", "rollback").is_empty());
    let events = records.0.lock().unwrap();
    let event = events
        .iter()
        .find(|event| matches!(event.kind, LogEventKind::FailedCommand { .. }))
        .unwrap();
    assert_eq!(event.scope.as_ref().unwrap().component.as_str(), "worker");
    assert_eq!(event.scope.as_ref().unwrap().step, "rollback");
    assert!(
        fixture.history.observations(&report.deployment.id).unwrap()[0]
            .observed
            .is_err()
    );
}

async fn run(fixture: &Fixture, records: &Records) -> RollbackReport {
    let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
    RollbackOrchestrator::new(&fixture.history, Redactor::default())
        .with_events(records)
        .rollback(
            &fixture.source,
            fixture.components(),
            &order,
            &fixture.cancellation,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn explicit_rollback_scopes_real_reverse_order_and_ignores_audit_warnings_for_local_persistence()
 {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().audit_warning = Some("remote audit unavailable".into());
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    for name in ["database", "api", "worker"] {
        assert_eq!(
            records.states(name, "rollback"),
            [(Started, Recorded), (Succeeded, Recorded)]
        );
        assert!(records.states(name, "compensate").is_empty());
    }
    assert_eq!(
        fixture.actions(),
        [
            "rollback:worker->not_deployed",
            "rollback:api->v2",
            "rollback:database->v1"
        ]
    );
}

#[tokio::test]
async fn rollback_intent_write_failure_has_no_started_event_or_unjournaled_mutation() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_intent BEFORE INSERT ON operation_intents WHEN NEW.stage='rollback' AND NEW.component='api' BEGIN SELECT RAISE(FAIL,'injected'); END;");
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(records.states("api", "rollback").is_empty());
    assert_eq!(
        records.states("worker", "compensate"),
        [(Started, Recorded), (Succeeded, Recorded)]
    );
    assert_eq!(
        fixture.actions(),
        ["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
}

#[tokio::test]
async fn known_rollback_and_compensation_are_unconfirmed_when_durable_completion_fails() {
    let fixture = Fixture::new();
    fixture.inject_history_failure("CREATE TRIGGER fail_completion BEFORE UPDATE OF status ON operation_intents WHEN OLD.stage IN ('rollback','compensate') BEGIN SELECT RAISE(FAIL,'injected'); END;");
    let records = Records::default();
    let report = run(&fixture, &records).await;
    for step in ["rollback", "compensate"] {
        assert_eq!(
            records.states("worker", step),
            [(Started, Recorded), (Succeeded, Unconfirmed)]
        );
    }
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
    assert_eq!(
        fixture.actions(),
        ["rollback:worker->not_deployed", "rollback:worker->v3"]
    );
    assert_eq!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn failed_rollback_and_compensation_keep_distinct_scopes_and_outcomes() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_component = Some(ComponentName::parse("api").unwrap());
    fixture.state.lock().unwrap().fail_compensation = Some(ComponentName::parse("worker").unwrap());
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        records.states("api", "rollback"),
        [(Started, Recorded), (Failed, Recorded)]
    );
    assert_eq!(
        records.states("worker", "compensate"),
        [(Started, Recorded), (Failed, Recorded)]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::CompensationFailed
    );
}

#[test]
fn log_initialization_failure_is_failed_not_cancelled_and_retains_deployment_id() {
    for cancelled in [false, true] {
        let fixture = Fixture::new();
        let order = ["database", "api", "worker"].map(|name| ComponentName::parse(name).unwrap());
        let orchestrator = RollbackOrchestrator::new(&fixture.history, Redactor::default());
        let (deployment, _) = orchestrator
            .prepare_rollback(
                &fixture.source,
                fixture.components(),
                &order,
                &fixture.cancellation,
            )
            .unwrap();
        let id = deployment.id.clone();
        let error = orchestrator.log_initialization_failed(deployment, cancelled);
        assert!(
            matches!(&error, OrchestrationError::Execution { deployment, persistence: None, .. } if deployment == &id)
        );
        assert!(error.to_string().contains("no remote mutation started"));
        assert_eq!(
            fixture.history.deployment(&id).unwrap().unwrap().state,
            if cancelled {
                DeploymentState::Cancelled
            } else {
                DeploymentState::Failed
            }
        );
        assert!(fixture.actions().is_empty());
    }
}
