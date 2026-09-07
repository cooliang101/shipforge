use super::*;
use crate::{
    application::step_events::{ScopedEvents, tests::Records},
    drivers::{CleanupCandidate, inventory::InventoryRelease},
    telemetry::log_record::{CommandLocation, LogEvent, LogEventKind, RecordedCommand},
};

pub(super) fn driver_event(events: &dyn EventSink, operation: &str) {
    events.emit_record(LogEvent {
        namespace: "driver.fixture".into(),
        message: "diagnostic fixture".into(),
        scope: None,
        kind: LogEventKind::FailedCommand {
            command: RecordedCommand {
                working_directory: None,
                location: CommandLocation::Remote,
                index: None,
                program: "fixture".into(),
                args: vec![operation.into()],
            },
        },
    });
}

fn policy(component: &DeploymentComponent, reference: &ReleaseRef) -> RetentionPolicy {
    RetentionPolicy {
        protected_versions: std::collections::BTreeSet::new(),
        retain_count: 5,
        candidate: CleanupCandidate {
            release: reference.clone(),
            expected_current: None,
            package: InventoryRelease {
                manifest: component.package.manifest().clone(),
                sha256: "a".repeat(64),
                size: 1,
                extracted: true,
            },
        },
    }
}

#[tokio::test]
async fn changed_yaml_keeps_frozen_read_only_observation_diagnostics_available() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let context = &component.planned.context;
    std::fs::write(
        fixture.directory.path().join("shipforge.yaml"),
        PROJECT.replace("generation: 1", "generation: 2"),
    )
    .unwrap();
    let records = Records::default();
    let observed = component
        .planned
        .driver
        .current_with_events(
            context,
            &ScopedEvents::new(&records, &context.component, "compensate"),
        )
        .await
        .unwrap();
    assert_eq!(observed, None);
    assert!(fixture.actions().is_empty());
    let events = records.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].scope.as_ref().unwrap().step, "compensate");
    assert!(
        matches!(&events[0].kind, LogEventKind::FailedCommand { command } if command.args == ["observe"])
    );
}

#[tokio::test]
async fn guarded_event_entrypoints_forward_exact_core_scope_without_changing_mutations() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let context = &component.planned.context;
    let driver = &component.planned.driver;
    let reference = fixture
        .inner
        .reference(context, ReleaseVersion::parse("v1").unwrap());
    let deployment = DeploymentId::new();
    let records = Records::default();
    driver
        .activate_with_events(
            &deployment,
            context,
            &reference,
            &ScopedEvents::new(&records, &context.component, "activate"),
        )
        .await
        .unwrap();
    driver
        .rollback_with_events(
            &deployment,
            context,
            Some(&reference),
            None,
            &ScopedEvents::new(&records, &context.component, "compensate"),
        )
        .await
        .unwrap();
    driver
        .cleanup_with_events(
            context,
            &policy(&component, &reference),
            &ScopedEvents::new(&records, &context.component, "cleanup.v1"),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.actions(),
        ["activate:backend", "rollback:backend", "cleanup:backend"]
    );
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 3);
    for (event, (scope, command)) in records.iter().zip([
        ("activate", "activate"),
        ("compensate", "rollback"),
        ("cleanup.v1", "cleanup"),
    ]) {
        assert_eq!(event.scope.as_ref().unwrap().component.as_str(), "backend");
        assert_eq!(event.scope.as_ref().unwrap().step, scope);
        assert!(
            matches!(&event.kind, LogEventKind::FailedCommand { command: actual } if actual.args == [command])
        );
    }
}

#[tokio::test]
async fn stale_yaml_blocks_all_event_entrypoints_before_commands_or_diagnostics() {
    let fixture = Fixture::new();
    let component = fixture.component("backend");
    let context = &component.planned.context;
    let driver = &component.planned.driver;
    let reference = fixture
        .inner
        .reference(context, ReleaseVersion::parse("v1").unwrap());
    let deployment = DeploymentId::new();
    let records = Records::default();
    std::fs::write(
        fixture.directory.path().join("shipforge.yaml"),
        PROJECT.replace("generation: 1", "generation: 2"),
    )
    .unwrap();
    assert!(
        driver
            .activate_with_events(&deployment, context, &reference, &records)
            .await
            .is_err()
    );
    assert!(
        driver
            .rollback_with_events(&deployment, context, Some(&reference), None, &records)
            .await
            .is_err()
    );
    assert!(
        driver
            .cleanup_with_events(context, &policy(&component, &reference), &records)
            .await
            .is_err()
    );
    assert!(fixture.actions().is_empty());
    assert!(records.0.lock().unwrap().is_empty());
}
