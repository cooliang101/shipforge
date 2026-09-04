use super::*;
use crate::{
    application::step_events::tests::Records,
    telemetry::log_record::{
        LogEventKind,
        LogPersistence::Recorded,
        LogRecord,
        LogStepState::{Started, Succeeded},
    },
};

#[tokio::test]
async fn confirmed_service_rollback_streams_and_persists_the_new_deployment_not_its_source() {
    let fixture = Fixture::new(&["frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    let plan = fixture.plan(source.clone(), targets).await.unwrap();
    let records = Records::default();
    let report = fixture
        .service
        .execute_with_events(
            plan,
            &fixture.registry_path,
            &records,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.warnings.is_empty());
    assert_eq!(
        records.states("frontend", "rollback"),
        [(Started, Recorded), (Succeeded, Recorded)]
    );
    assert!(records.0.lock().unwrap().iter().any(|event| matches!(&event.kind, LogEventKind::DeploymentStarted { deployment } if deployment == &report.deployment.id && deployment != &source)));
    let history = HistoryStore::open_existing_read_only(&fixture.history_path).unwrap();
    assert!(history.deployment_log(&source).unwrap().is_none());
    let log = history
        .deployment_log(&report.deployment.id)
        .unwrap()
        .unwrap();
    assert_eq!(log.format, crate::history::DeploymentLogFormat::JsonlV1);
    let saved = std::fs::read_to_string(fixture.directory.path().join(log.relative_path)).unwrap();
    let saved = saved
        .lines()
        .map(|line| serde_json::from_str::<LogRecord>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(saved.iter().any(|record| matches!(&record.event.kind, LogEventKind::DeploymentStarted { deployment } if deployment == &report.deployment.id)));
    assert!(saved.iter().any(|record| {
        matches!(
            record.event.kind,
            LogEventKind::Step {
                state: Succeeded,
                persistence: Recorded
            }
        ) && record.event.scope.as_ref().is_some_and(|scope| {
            scope.component == component("frontend") && scope.step == "rollback"
        })
    }));
}

#[tokio::test]
async fn real_log_open_failure_returns_typed_id_without_any_remote_mutation() {
    let fixture = Fixture::new(&["frontend"]);
    let (_, targets) = fixture.publish("v1", true, true);
    let (source, _) = fixture.publish("v2", true, true);
    let plan = fixture.plan(source, targets).await.unwrap();
    std::fs::write(fixture.directory.path().join("logs"), "not a directory").unwrap();
    let records = Records::default();
    let error = fixture
        .service
        .execute_with_events(
            plan,
            &fixture.registry_path,
            &records,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    let RollbackServiceError::Orchestration(OrchestrationError::Execution {
        deployment,
        persistence: None,
        ..
    }) = &error
    else {
        panic!("typed known Deployment required");
    };
    assert!(error.to_string().contains("no remote mutation started"));
    assert!(!error.to_string().contains("not a directory"));
    let history = HistoryStore::open_existing_read_only(&fixture.history_path).unwrap();
    assert_eq!(
        history.deployment(deployment).unwrap().unwrap().state,
        DeploymentState::Failed
    );
    assert!(fixture.mutations().is_empty());
    assert!(records.states("frontend", "rollback").is_empty());
}
