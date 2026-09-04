//! Operation scopes come from frozen application inputs, never log messages.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    domain::ComponentName,
    drivers::{DriverLog, EventSink},
    telemetry::log_record::{LogEvent, LogEventKind, LogPersistence, LogScope, LogStepState},
};

pub(crate) struct NoEvents;

impl EventSink for NoEvents {
    fn emit(&self, _: DriverLog) {}
}

pub(crate) struct ScopedEvents<'a> {
    events: &'a dyn EventSink,
    scope: LogScope,
}

impl<'a> ScopedEvents<'a> {
    pub(crate) fn new(events: &'a dyn EventSink, component: &ComponentName, step: &str) -> Self {
        Self {
            events,
            scope: LogScope {
                component: component.clone(),
                step: step.to_owned(),
            },
        }
    }
}

impl EventSink for ScopedEvents<'_> {
    fn emit(&self, event: DriverLog) {
        self.emit_record(LogEvent {
            namespace: event.namespace,
            message: event.message,
            scope: None,
            kind: LogEventKind::Output,
        });
    }

    fn emit_record(&self, mut event: LogEvent) {
        event.scope = Some(self.scope.clone());
        self.events.emit_record(event);
    }
}

/// A live step begins only after its durable intent has been acknowledged.
/// Dropping this guard never completes that intent or changes execution state.
pub(crate) struct StepEvents<'a> {
    scoped: ScopedEvents<'a>,
    finished: AtomicBool,
}

impl<'a> StepEvents<'a> {
    pub(crate) fn start(events: &'a dyn EventSink, component: &ComponentName, step: &str) -> Self {
        let events = Self {
            scoped: ScopedEvents::new(events, component, step),
            finished: AtomicBool::new(false),
        };
        events.state(LogStepState::Started, LogPersistence::Recorded);
        events
    }

    pub(crate) fn finish(&self, state: LogStepState, persistence: LogPersistence) {
        if !self.finished.swap(true, Ordering::Relaxed) {
            self.state(state, persistence);
        }
    }

    fn state(&self, state: LogStepState, persistence: LogPersistence) {
        self.scoped.emit_record(LogEvent {
            namespace: "step.state".into(),
            message: format!("{}: {state:?} ({persistence:?})", self.scoped.scope.step),
            scope: None,
            kind: LogEventKind::Step { state, persistence },
        });
    }
}

impl EventSink for StepEvents<'_> {
    fn emit(&self, event: DriverLog) {
        self.scoped.emit(event);
    }

    fn emit_record(&self, event: LogEvent) {
        self.scoped.emit_record(event);
    }
}

impl Drop for StepEvents<'_> {
    fn drop(&mut self) {
        self.finish(LogStepState::Unknown, LogPersistence::Unconfirmed);
    }
}

pub(super) const fn step_state(succeeded: bool) -> LogStepState {
    if succeeded {
        LogStepState::Succeeded
    } else {
        LogStepState::Failed
    }
}

pub(super) const fn persistence(recorded: bool) -> LogPersistence {
    if recorded {
        LogPersistence::Recorded
    } else {
        LogPersistence::Unconfirmed
    }
}

#[cfg(test)]
pub(super) mod tests;
