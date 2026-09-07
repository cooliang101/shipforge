use std::sync::{Mutex, Weak};

use crate::telemetry::{CommandArgument, log_record::RecordedCommand};

use super::*;

#[derive(Default)]
struct Events(Mutex<Vec<LogEvent>>);

impl EventSink for Events {
    fn emit(&self, _event: DriverLog) {
        panic!("structured command evidence must retain its event kind");
    }

    fn emit_record(&self, event: LogEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn command() -> CommandSpec {
    CommandSpec::structured(
        "systemctl",
        ["restart", "--", "actual-api.service"].map(CommandArgument::plain),
    )
    .unwrap()
}

#[tokio::test]
async fn relays_during_the_operation_and_drains_before_return_without_a_worker() {
    struct NotifyingEvents {
        events: Events,
        notify: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }
    impl EventSink for NotifyingEvents {
        fn emit(&self, _event: DriverLog) {
            panic!("structured evidence required");
        }
        fn emit_record(&self, event: LogEvent) {
            self.events.emit_record(event);
            if let Some(notify) = self.notify.lock().unwrap().take() {
                notify.send(()).unwrap();
            }
        }
    }
    let (notify, ready) = tokio::sync::oneshot::channel();
    let events = NotifyingEvents {
        events: Events::default(),
        notify: Mutex::new(Some(notify)),
    };
    let weak = Mutex::new(None::<Weak<dyn EventSink>>);
    let weak_ref = &weak;
    let result = relay(&events, |sink| async move {
        *weak_ref.lock().unwrap() = Some(Arc::downgrade(&sink));
        sink.emit_record(failed(&command(), "First failure"));
        ready.await.unwrap();
        sink.emit_record(failed(&command(), "Final failure"));
        42
    })
    .await;
    assert_eq!(result, 42);
    assert_eq!(events.events.0.lock().unwrap().len(), 2);
    assert!(weak.lock().unwrap().as_ref().unwrap().upgrade().is_none());
}

#[tokio::test]
async fn queue_count_and_bytes_are_bounded_and_omission_is_explicit() {
    for oversized in [false, true] {
        let events = Events::default();
        let result = relay(&events, |sink| async move {
            if oversized {
                let mut event = failed(&command(), "Failure");
                event.message = "x".repeat(MAX_QUEUED_BYTES + 1);
                sink.emit_record(event);
            } else {
                for _ in 0..MAX_QUEUED_RECORDS + 5 {
                    sink.emit_record(failed(&command(), "Failure"));
                }
            }
            "unchanged result"
        })
        .await;
        assert_eq!(result, "unchanged result");
        let recorded = events.0.lock().unwrap();
        assert!(recorded.len() <= MAX_QUEUED_RECORDS + 1);
        assert_eq!(
            recorded
                .iter()
                .filter(|event| matches!(event.kind, LogEventKind::CommandUnavailable { .. }))
                .count(),
            1
        );
        assert!(recorded.iter().all(|event| event.scope.is_none()));
    }
}

#[tokio::test]
async fn final_diagnostic_preserves_the_operations_cancellation_or_failure_result() {
    for result in [Err("cancelled"), Err("failed"), Ok("succeeded after retry")] {
        let events = Events::default();
        let actual = relay(&events, |sink| async move {
            sink.emit_record(failed(&command(), "Actual attempted command"));
            result
        })
        .await;
        assert_eq!(actual, result);
        assert_eq!(events.0.lock().unwrap().len(), 1);
    }
}

#[test]
fn snapshot_is_the_actual_argv_not_shell_rendering_or_output_text() {
    let command = CommandSpec::structured(
        "actual-tool",
        [
            CommandArgument::plain("arg with spaces ' and ;"),
            CommandArgument::sensitive("url?token=never-release"),
            CommandArgument::plain("--password"),
            CommandArgument::plain("named-secret"),
        ],
    )
    .unwrap();
    let event = failed(&command, "Static execution failure");
    let LogEventKind::FailedCommand { command: snapshot } = event.kind else {
        panic!("bounded complete argv expected");
    };
    assert_eq!(
        snapshot,
        RecordedCommand {
            working_directory: None,
            location: CommandLocation::Remote,
            index: None,
            program: "actual-tool".into(),
            args: vec![
                "arg with spaces ' and ;".into(),
                "[REDACTED]".into(),
                "--password".into(),
                "[REDACTED]".into()
            ],
        }
    );
    assert!(command.render_posix().unwrap().contains("never-release"));
    let diagnostic = serde_json::to_string(&snapshot).unwrap();
    assert!(!diagnostic.contains("never-release"));
    assert!(!diagnostic.contains("named-secret"));
}

#[test]
fn oversize_or_explicit_shell_snapshots_are_unavailable_and_never_shortened() {
    let mut command = command();
    command.args =
        vec![CommandArgument::plain(""); crate::telemetry::log_record::MAX_COMMAND_ARGUMENTS + 1];
    assert!(matches!(
        failed(&command, "Failure").kind,
        LogEventKind::CommandUnavailable {
            location: CommandLocation::Remote,
            index: None
        }
    ));
    command.args = vec![CommandArgument::plain(
        "x".repeat(crate::telemetry::log_record::MAX_COMMAND_BYTES + 1),
    )];
    assert!(matches!(
        failed(&command, "Failure").kind,
        LogEventKind::CommandUnavailable { .. }
    ));
    command.args.clear();
    command.shell = true;
    assert!(matches!(
        failed(&command, "Failure").kind,
        LogEventKind::CommandUnavailable { .. }
    ));
}
