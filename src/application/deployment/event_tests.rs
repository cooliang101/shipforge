use super::*;
use crate::{
    application::step_events::tests::Records,
    telemetry::log_record::{
        LogEventKind,
        LogPersistence::{Recorded, Unconfirmed},
        LogRecord,
        LogStepState::{Failed, Started, Succeeded},
    },
};

#[tokio::test]
async fn build_spawn_failure_records_exact_component_step_and_command_in_live_and_history_logs() {
    let directory = tempfile::tempdir().unwrap();
    let (plan, destinations) = super::version_tests::failing_build_plan(directory.path());
    // The shared fixture deliberately has no source tree. This case must pass
    // path preflight and actually attempt the configured missing executable.
    std::fs::create_dir_all(
        directory
            .path()
            .join(&plan.entries[0].config.working_directory),
    )
    .unwrap();
    let command = plan.entries[0].config.build[0].clone();
    let history_path = directory.path().join("history.sqlite3");
    let records = Records::default();
    let error = DeploymentService::new(Arc::new(DriverRegistry::default()), history_path.clone())
        .execute(plan, &destinations, &records, &CancellationToken::new())
        .await
        .unwrap_err();
    let DeploymentServiceError::Execution {
        deployment, source, ..
    } = error
    else {
        panic!("ID required");
    };
    assert!(matches!(
        *source,
        DeploymentServiceError::Build(crate::application::BuildError::Process(
            crate::adapters::ProcessError::Spawn(_)
        ))
    ));
    assert_eq!(
        records.states("frontend", "build-package"),
        [(Started, Recorded), (Failed, Recorded)]
    );
    let live = records.0.lock().unwrap();
    let failed = live
        .iter()
        .find(|event| matches!(event.kind, LogEventKind::FailedCommand { .. }))
        .unwrap();
    assert_eq!(
        failed.scope.as_ref().unwrap().component.as_str(),
        "frontend"
    );
    assert_eq!(failed.scope.as_ref().unwrap().step, "build-package");
    let LogEventKind::FailedCommand { command: captured } = &failed.kind else {
        unreachable!();
    };
    assert_eq!(captured.index, Some(1));
    assert_eq!(captured.program, command.program);
    assert_eq!(captured.args, command.args);
    let history = HistoryStore::open_existing_read_only(&history_path).unwrap();
    let log = history.deployment_log(&deployment).unwrap().unwrap();
    assert_eq!(log.format, crate::history::DeploymentLogFormat::JsonlV1);
    let saved = std::fs::read_to_string(directory.path().join(log.relative_path)).unwrap();
    let saved = saved
        .lines()
        .map(|line| serde_json::from_str::<LogRecord>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(saved.iter().any(|record| record.event == *failed));
    assert!(history.pending_intents(&deployment).unwrap().is_empty());
}

#[tokio::test]
async fn built_package_remains_known_when_package_metadata_persistence_fails() {
    let directory = tempfile::tempdir().unwrap();
    let (mut plan, destinations) = super::version_tests::failing_build_plan(directory.path());
    let config_path = directory.path().join("shipforge.yaml");
    let yaml = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace(
            "shipforge-test-nonexistent-executable, ci",
            "rustc, --version",
        )
        .replace(
            "shipforge-test-nonexistent-executable, run, build",
            "rustc, --version",
        );
    std::fs::write(config_path, yaml).unwrap();
    let crate::config::ProjectConfigState::Loaded(config) =
        crate::config::load(directory.path()).unwrap()
    else {
        panic!("valid config");
    };
    plan.entries[0].config = config.components[&plan.entries[0].component].clone();
    plan.selection.config = config;
    let artifact = directory.path().join("frontend/dist");
    std::fs::create_dir_all(&artifact).unwrap();
    std::fs::write(artifact.join("index.html"), "fixture").unwrap();
    let history_path = directory.path().join("history.sqlite3");
    let history = HistoryStore::open(&history_path).unwrap();
    rusqlite::Connection::open(&history_path).unwrap().execute_batch(
        "CREATE TRIGGER fail_package BEFORE INSERT ON release_packages BEGIN SELECT RAISE(ABORT,'injected'); END;"
    ).unwrap();
    let records = Records::default();
    let error = DeploymentService::new(Arc::new(DriverRegistry::default()), history_path)
        .execute(plan, &destinations, &records, &CancellationToken::new())
        .await
        .unwrap_err();
    let DeploymentServiceError::Execution { deployment, .. } = error else {
        panic!("ID required");
    };
    assert_eq!(
        records.states("frontend", "build-package"),
        [(Started, Recorded), (Succeeded, Unconfirmed)]
    );
    assert_eq!(
        history.steps(&deployment).unwrap()[0].status,
        crate::history::StepStatus::Failed
    );
    assert!(records.states("frontend", "prepare").is_empty());
    assert!(
        !records
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, LogEventKind::FailedCommand { .. }))
    );
}
