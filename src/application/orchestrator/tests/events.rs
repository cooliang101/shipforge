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
async fn post_failure_observation_commands_keep_activation_or_compensation_scope_and_unknown_state()
{
    use crate::telemetry::log_record::LogEventKind;
    for (name, step, stage) in [
        ("backend", "activate", "activate.failure"),
        ("worker", "compensate", "compensate.failure"),
    ] {
        let fixture = Fixture::new();
        let component = ComponentName::parse(name).unwrap();
        {
            let mut state = fixture.state.lock().unwrap();
            state.fail_activate = Some(ComponentName::parse("backend").unwrap());
            state.fail_current = Some(component.clone());
            if step == "compensate" {
                state.fail_rollback = Some(component.clone());
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
        assert!(
            fixture
                .state
                .lock()
                .unwrap()
                .observation_tokens_cancelled
                .iter()
                .all(|value| !value)
        );
    }
}

async fn run(fixture: &Fixture, records: &Records) -> DeploymentReport {
    let components = ["frontend", "backend", "worker"]
        .map(|name| fixture.component(name))
        .into();
    let order = ["worker", "backend", "frontend"].map(|name| ComponentName::parse(name).unwrap());
    DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(components, &order, records, &fixture.cancellation)
        .await
        .unwrap()
}

#[tokio::test]
async fn actual_components_receive_distinct_durable_step_lifecycles() {
    let fixture = Fixture::new();
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    for name in ["frontend", "backend", "worker"] {
        for step in ["prepare", "activate"] {
            assert_eq!(
                records.states(name, step),
                [(Started, Recorded), (Succeeded, Recorded)]
            );
        }
        assert!(records.states(name, "compensate").is_empty());
    }
}

#[tokio::test]
async fn rejected_intent_never_claims_started_and_previous_component_is_compensated() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_intent BEFORE INSERT ON operation_intents WHEN NEW.stage='activate' AND NEW.component='backend' BEGIN SELECT RAISE(ABORT,'injected'); END;",
    );
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert!(records.states("backend", "activate").is_empty());
    assert!(!fixture.actions().contains(&"activate:backend".into()));
    assert_eq!(
        records.states("worker", "compensate"),
        [(Started, Recorded), (Succeeded, Recorded)]
    );
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
    );
}

#[tokio::test]
async fn known_activation_and_compensation_survive_failed_intent_persistence() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_completion BEFORE UPDATE ON operation_intents WHEN OLD.stage IN ('activate','compensate') BEGIN SELECT RAISE(ABORT,'injected'); END;",
    );
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    for step in ["activate", "compensate"] {
        assert_eq!(
            records.states("worker", step),
            [(Started, Recorded), (Succeeded, Unconfirmed)]
        );
    }
    assert_eq!(fixture.actions().last().unwrap(), "rollback:worker");
    assert_eq!(
        report.deployment.components[&ComponentName::parse("worker").unwrap()].outcome,
        ComponentOutcome::Compensated
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
async fn invalid_prepared_receipt_is_a_failed_step_not_a_success() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().wrong_prepare = Some(ComponentName::parse("backend").unwrap());
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        records.states("backend", "prepare"),
        [(Started, Recorded), (Failed, Recorded)]
    );
    assert!(records.states("backend", "activate").is_empty());
}

#[tokio::test]
async fn known_preparation_is_unconfirmed_when_receipt_storage_fails() {
    let fixture = Fixture::new();
    inject_history_failure(
        &fixture,
        "CREATE TRIGGER fail_receipt BEFORE INSERT ON release_receipts BEGIN SELECT RAISE(ABORT,'injected'); END;",
    );
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        records.states("backend", "prepare"),
        [(Started, Recorded), (Succeeded, Unconfirmed)]
    );
    assert!(records.states("backend", "activate").is_empty());
}

#[tokio::test]
async fn invalid_activation_and_failed_compensation_never_claim_success() {
    let fixture = Fixture::new();
    fixture.state.lock().unwrap().fail_activate = Some(ComponentName::parse("backend").unwrap());
    fixture.state.lock().unwrap().fail_rollback = Some(ComponentName::parse("worker").unwrap());
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Failed);
    assert_eq!(
        records.states("backend", "activate"),
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
