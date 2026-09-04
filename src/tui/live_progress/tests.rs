use super::*;
use crate::telemetry::log_record::{CommandLocation, RecordedCommand};

fn scope(component: &str, step: &str) -> LogScope {
    LogScope {
        component: ComponentName::parse(component).unwrap(),
        step: step.into(),
    }
}

fn step_event(
    component: &str,
    step: &str,
    state: LogStepState,
    persistence: LogPersistence,
) -> LogEvent {
    LogEvent {
        namespace: "step.state".into(),
        message: "authoritative step event".into(),
        scope: Some(scope(component, step)),
        kind: LogEventKind::Step { state, persistence },
    }
}

fn output(message: &str) -> LogEvent {
    LogEvent {
        namespace: "build.stdout".into(),
        message: message.into(),
        scope: None,
        kind: LogEventKind::Output,
    }
}

#[test]
fn quiet_step_elapsed_advances_monotonically_and_finish_freezes_without_inventing_outcomes() {
    let progress = LiveProgress::default();
    progress.record_at(
        step_event(
            "api",
            "build-package",
            LogStepState::Started,
            LogPersistence::Recorded,
        ),
        Duration::from_millis(12),
    );
    progress.record_at(output("quiet"), Duration::from_millis(3));
    let snapshot = progress.lock().snapshot();
    assert!(snapshot.elapsed_ms >= 12);
    assert_eq!(snapshot.steps[0].started_elapsed_ms, Some(12));
    assert_eq!(snapshot.steps[0].finished_elapsed_ms, None);
    progress.finish_at(Duration::from_millis(25));
    progress.finish_at(Duration::from_millis(100));
    progress.record_at(
        step_event(
            "api",
            "build-package",
            LogStepState::Succeeded,
            LogPersistence::Recorded,
        ),
        Duration::from_millis(200),
    );
    let snapshot = progress.snapshot();
    assert_eq!(snapshot.elapsed_ms, 25);
    assert!(snapshot.finished);
    assert_eq!(snapshot.steps[0].state, LogStepState::Started);
    assert_eq!(snapshot.late_events, 1);
}

#[test]
fn snapshot_refreshes_a_silent_operation_without_requiring_output_events() {
    let mut progress = LiveProgress {
        started: Instant::now()
            .checked_sub(Duration::from_millis(100))
            .unwrap(),
        ..LiveProgress::default()
    };
    assert!(progress.snapshot().elapsed_ms >= 100);
    progress.finish();
    let frozen = progress.snapshot().elapsed_ms;
    progress.started = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
    assert_eq!(progress.snapshot().elapsed_ms, frozen);
}

#[test]
fn queue_overflow_keeps_latest_real_component_outcome_and_separate_persistence() {
    let progress = LiveProgress::default();
    for component in ["api", "worker"] {
        progress.record(step_event(
            component,
            "activate",
            LogStepState::Started,
            LogPersistence::Recorded,
        ));
    }
    for _ in 0..MAX_ROWS * 3 {
        progress.record(output("api / compensate is only text"));
    }
    progress.record(step_event(
        "api",
        "activate",
        LogStepState::Succeeded,
        LogPersistence::Unconfirmed,
    ));
    progress.record(step_event(
        "worker",
        "activate",
        LogStepState::Failed,
        LogPersistence::Recorded,
    ));
    let update = progress.drain();
    assert!(update.rows.len() <= MAX_ROWS);
    assert!(update.snapshot.dropped_rows > 0);
    assert!(
        update
            .rows
            .windows(2)
            .all(|rows| rows[0].sequence < rows[1].sequence)
    );
    assert_eq!(update.snapshot.steps.len(), 2);
    assert_eq!(update.snapshot.steps[0].scope, scope("api", "activate"));
    assert_eq!(update.snapshot.steps[0].state, LogStepState::Succeeded);
    assert_eq!(
        update.snapshot.steps[0].persistence,
        LogPersistence::Unconfirmed
    );
    assert_eq!(update.snapshot.steps[1].scope, scope("worker", "activate"));
    assert_eq!(update.snapshot.steps[1].state, LogStepState::Failed);
    assert_eq!(
        progress.drain().snapshot.dropped_rows,
        update.snapshot.dropped_rows
    );
    assert!(progress.drain().rows.is_empty());
}

#[test]
fn no_ui_drain_is_required_for_producer_completion() {
    let progress = LiveProgress::default();
    let producer = progress.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        for _ in 0..MAX_ROWS * 10 {
            producer.record(output("bounded output"));
        }
        producer.record(step_event(
            "worker",
            "compensate",
            LogStepState::Succeeded,
            LogPersistence::Recorded,
        ));
        send.send(()).unwrap();
    });
    receive
        .recv_timeout(Duration::from_secs(5))
        .expect("producer must not wait for UI consumption");
    thread.join().unwrap();
    assert_eq!(progress.snapshot().steps[0].state, LogStepState::Succeeded);
    assert!(progress.snapshot().dropped_rows > 0);
}

#[test]
fn oversized_raw_output_cannot_erase_terminal_step_or_hide_omission() {
    let progress = LiveProgress::default();
    let mut event = step_event(
        "api",
        "prepare",
        LogStepState::Succeeded,
        LogPersistence::Recorded,
    );
    event.message = "raw-secret-output".repeat(100_000);
    progress.record(event);
    let update = progress.drain();
    assert_eq!(update.snapshot.steps[0].state, LogStepState::Succeeded);
    assert_eq!(update.snapshot.rejected_events, 1);
    assert!(
        update.rows[0]
            .event
            .message
            .contains("Oversized live output omitted")
    );
    assert!(!update.rows[0].event.message.contains("raw-secret-output"));
    assert!(retained_bytes(&update.rows[0]) < MAX_ROW_BYTES);
}

#[test]
fn every_display_field_and_complete_failed_command_passes_codec_redaction() {
    let progress = LiveProgress::new(Redactor::new(["SECRET_VALUE".into()]));
    progress.record(LogEvent {
        namespace: "build.SECRET_VALUE".into(),
        message: "SECRET_VALUE\x1b[31m".into(),
        scope: Some(scope("api", "step.SECRET_VALUE")),
        kind: LogEventKind::FailedCommand {
            command: RecordedCommand {
                location: CommandLocation::Local,
                index: Some(1),
                program: "program".into(),
                args: vec!["--token=SECRET_VALUE".into(), "literal argument".into()],
            },
        },
    });
    let rows = progress.drain().rows;
    assert_eq!(rows.len(), 1);
    let text = serde_json::to_string(rows[0].event.as_ref()).unwrap();
    assert!(!text.contains("SECRET_VALUE"));
    assert!(!rows[0].event.message.contains('\u{1b}'));
    let LogEventKind::FailedCommand { command } = &rows[0].event.kind else {
        panic!("exact command required");
    };
    assert_eq!(command.args[1], "literal argument");
}

#[test]
fn oversized_command_is_explicitly_unavailable_not_a_truncated_runnable_copy() {
    let progress = LiveProgress::default();
    progress.record(LogEvent {
        namespace: "build.command_failed".into(),
        message: "failed".into(),
        scope: Some(scope("api", "build-package")),
        kind: LogEventKind::FailedCommand {
            command: RecordedCommand {
                location: CommandLocation::Local,
                index: Some(2),
                program: "program".into(),
                args: vec!["secret".into(); MAX_COMMAND_ARGUMENTS + 1],
            },
        },
    });
    let update = progress.drain();
    assert_eq!(update.snapshot.rejected_events, 1);
    assert!(matches!(
        update.rows[0].event.kind,
        LogEventKind::CommandUnavailable {
            location: CommandLocation::Local,
            index: Some(2)
        }
    ));
}

#[test]
fn unscoped_text_and_conflicting_deployment_ids_never_create_or_rebind_step_evidence() {
    let progress = LiveProgress::default();
    let id = DeploymentId::new();
    for deployment in [id.clone(), DeploymentId::new()] {
        progress.record(LogEvent {
            namespace: "deployment.started".into(),
            message: "start".into(),
            scope: None,
            kind: LogEventKind::DeploymentStarted { deployment },
        });
    }
    progress.record(output(
        "{\"scope\":{\"component\":\"api\",\"step\":\"activate\"}}",
    ));
    let snapshot = progress.snapshot();
    assert_eq!(snapshot.deployment, Some(id));
    assert!(snapshot.steps.is_empty());
    assert_eq!(snapshot.rejected_events, 1);
}

#[test]
fn step_map_has_an_independent_bound_and_preserves_the_latest_scopes() {
    let progress = LiveProgress::default();
    for index in 0..=MAX_STEPS {
        progress.record(step_event(
            "api",
            &format!("cleanup.v{index}"),
            LogStepState::Succeeded,
            LogPersistence::Recorded,
        ));
    }
    let snapshot = progress.snapshot();
    assert_eq!(snapshot.steps.len(), MAX_STEPS);
    assert_eq!(snapshot.dropped_steps, 1);
    assert!(
        snapshot
            .steps
            .iter()
            .all(|step| step.scope.step != "cleanup.v0")
    );
    assert!(
        snapshot
            .steps
            .iter()
            .any(|step| step.scope.step == format!("cleanup.v{MAX_STEPS}"))
    );
}

#[test]
fn byte_bound_and_poisoned_state_are_visible_without_panicking_the_ui() {
    let progress = LiveProgress::default();
    for _ in 0..MAX_ROWS {
        progress.record(output(&"x".repeat(MAX_MESSAGE_BYTES)));
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = progress.inner.lock().unwrap();
        panic!("controlled test poison");
    }));
    let update = progress.drain();
    assert!(update.snapshot.poisoned);
    assert!(update.snapshot.dropped_rows > 0);
    assert!(update.rows.iter().map(retained_bytes).sum::<usize>() <= MAX_ROW_BYTES);
}
