//! Bounded command diagnostics relayed within the caller's operation future.
//! No task, worker, timer, global state, or command execution is added here.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use tokio::sync::mpsc;

use crate::{
    drivers::{DriverLog, EventSink},
    telemetry::{
        CommandSpec,
        log_record::{CommandLocation, LogEvent, LogEventKind},
    },
};

const MAX_QUEUED_RECORDS: usize = 32;
const MAX_QUEUED_BYTES: usize = 256 * 1024;

pub(super) struct QuietEvents;

impl EventSink for QuietEvents {
    fn emit(&self, _event: DriverLog) {}
}

struct CommandEvents {
    sender: mpsc::Sender<(LogEvent, usize)>,
    bytes: AtomicUsize,
    omitted: AtomicBool,
}

impl EventSink for CommandEvents {
    fn emit(&self, event: DriverLog) {
        self.emit_record(LogEvent {
            namespace: event.namespace,
            message: event.message,
            scope: None,
            kind: LogEventKind::Output,
        });
    }

    fn emit_record(&self, event: LogEvent) {
        let bytes = event_bytes(&event);
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_QUEUED_BYTES)
            })
            .is_err()
        {
            self.omitted.store(true, Ordering::Release);
            return;
        }
        if self.sender.try_send((event, bytes)).is_err() {
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
            self.omitted.store(true, Ordering::Release);
        }
    }
}

fn event_bytes(event: &LogEvent) -> usize {
    let mut bytes = std::mem::size_of::<LogEvent>()
        .saturating_add(event.namespace.capacity())
        .saturating_add(event.message.capacity());
    if let Some(scope) = &event.scope {
        bytes = bytes
            .saturating_add(scope.component.as_str().len())
            .saturating_add(scope.step.capacity());
    }
    if let LogEventKind::FailedCommand { command } = &event.kind {
        bytes = bytes
            .saturating_add(command.program.capacity())
            .saturating_add(
                command
                    .args
                    .capacity()
                    .saturating_mul(std::mem::size_of::<String>()),
            );
        for argument in &command.args {
            bytes = bytes.saturating_add(argument.capacity());
        }
    }
    bytes
}

pub(super) async fn relay<F, T>(
    events: &dyn EventSink,
    operation: impl FnOnce(Arc<dyn EventSink>) -> F,
) -> T
where
    F: Future<Output = T>,
{
    let (sender, mut receiver) = mpsc::channel(MAX_QUEUED_RECORDS);
    let sink = Arc::new(CommandEvents {
        sender,
        bytes: AtomicUsize::new(0),
        omitted: AtomicBool::new(false),
    });
    let operation = operation(Arc::clone(&sink) as Arc<dyn EventSink>);
    tokio::pin!(operation);
    let result = loop {
        tokio::select! {
            result = &mut operation => break result,
            Some((event, bytes)) = receiver.recv() => {
                sink.bytes.fetch_sub(bytes, Ordering::AcqRel);
                events.emit_record(event);
            }
        }
    };
    // Drain synchronously before this scope returns; the session has no worker
    // and can no longer produce a delayed event after the operation completes.
    while let Ok((event, bytes)) = receiver.try_recv() {
        sink.bytes.fetch_sub(bytes, Ordering::AcqRel);
        events.emit_record(event);
    }
    if sink.omitted.load(Ordering::Acquire) {
        events.emit_record(unavailable(
            "Some remote command diagnostics exceeded the bounded queue; omitted snapshots are unavailable.",
        ));
    }
    result
}

pub(super) fn failed(command: &CommandSpec, message: &'static str) -> LogEvent {
    let kind = command.diagnostic_snapshot(CommandLocation::Remote).map_or(
        LogEventKind::CommandUnavailable {
            location: CommandLocation::Remote,
            index: None,
        },
        |command| LogEventKind::FailedCommand { command },
    );
    LogEvent {
        namespace: "linux-ssh.command".into(),
        message: message.into(),
        scope: None,
        kind,
    }
}

pub(super) fn unavailable(message: &'static str) -> LogEvent {
    LogEvent {
        namespace: "linux-ssh.command".into(),
        message: message.into(),
        scope: None,
        kind: LogEventKind::CommandUnavailable {
            location: CommandLocation::Remote,
            index: None,
        },
    }
}

#[cfg(test)]
mod tests;
