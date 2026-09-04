use std::sync::Mutex;

use super::*;

#[derive(Default)]
pub(in crate::application) struct Records(pub(in crate::application) Mutex<Vec<LogEvent>>);

impl Records {
    pub(in crate::application) fn states(
        &self,
        component: &str,
        step: &str,
    ) -> Vec<(LogStepState, LogPersistence)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| {
                let scope = event.scope.as_ref()?;
                if scope.component.as_str() != component || scope.step != step {
                    return None;
                }
                if let LogEventKind::Step { state, persistence } = event.kind {
                    Some((state, persistence))
                } else {
                    None
                }
            })
            .collect()
    }
}

impl EventSink for Records {
    fn emit(&self, _: DriverLog) {
        // Diagnostic-only legacy output is not step or command evidence.
    }

    fn emit_record(&self, event: LogEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn same_step_on_different_components_keeps_authoritative_scope() {
    let records = Records::default();
    for component in ["api", "worker"] {
        let name = ComponentName::parse(component).unwrap();
        let step = StepEvents::start(&records, &name, "prepare");
        step.emit_record(LogEvent {
            namespace: "untrusted.component.step".into(),
            message: "worker / activate: not metadata".into(),
            scope: Some(LogScope {
                component: ComponentName::parse("unrelated").unwrap(),
                step: "activate".into(),
            }),
            kind: LogEventKind::Output,
        });
        step.finish(LogStepState::Succeeded, LogPersistence::Recorded);
    }
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 6);
    for (events, name) in records.chunks_exact(3).zip(["api", "worker"]) {
        for event in events {
            let scope = event.scope.as_ref().unwrap();
            assert_eq!(scope.component.as_str(), name);
            assert_eq!(scope.step, "prepare");
        }
        assert_eq!(events[1].kind, LogEventKind::Output);
    }
}

#[test]
fn known_success_with_unconfirmed_persistence_is_not_rewritten_by_drop() {
    let records = Records::default();
    {
        let step = StepEvents::start(&records, &ComponentName::parse("api").unwrap(), "activate");
        step.finish(LogStepState::Succeeded, LogPersistence::Unconfirmed);
        step.finish(LogStepState::Failed, LogPersistence::Recorded);
    }
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1].kind,
        LogEventKind::Step {
            state: LogStepState::Succeeded,
            persistence: LogPersistence::Unconfirmed,
        }
    );
}

#[test]
fn abandoned_scope_only_reports_unknown_unconfirmed() {
    let records = Records::default();
    drop(StepEvents::start(
        &records,
        &ComponentName::parse("api").unwrap(),
        "cleanup.v2",
    ));
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1].kind,
        LogEventKind::Step {
            state: LogStepState::Unknown,
            persistence: LogPersistence::Unconfirmed,
        }
    );
}
