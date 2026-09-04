use std::{
    ffi::OsString,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::telemetry::log_record::{
    LOG_RECORD_VERSION, LogEvent, LogEventKind, LogFragment, LogRecord, encode_log_record,
    split_log_event,
};
use crate::{
    adapters::OutputStream,
    domain::{ComponentName, DeploymentId},
    drivers::{DriverLog, EventSink},
    history::{RollingLogError, RollingLogWriter},
    telemetry::{Redactor, StreamingRedactor},
};

pub(super) const LOG_MAX_BYTES: u64 = 1024 * 1024;
pub(super) const LOG_RETAINED_FILES: usize = 3;
const MAX_UI_CHARACTERS: usize = 4096;
const MAX_WRITE_BYTES: usize = 8192;
const QUEUED_WRITES: usize = 128;
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

struct WriterState {
    error: Mutex<Option<String>>,
    cancellation: CancellationToken,
    finished: AtomicBool,
    completion: Notify,
}

impl WriterState {
    fn fail(&self, error: String) {
        if let Ok(mut first_error) = self.error.lock() {
            first_error.get_or_insert(error);
        }
        self.cancellation.cancel();
    }

    fn failure(&self) -> Option<String> {
        self.error.lock().map_or_else(
            |_| Some("deployment log state is unavailable".into()),
            |error| error.clone(),
        )
    }

    async fn completed(&self) {
        loop {
            let notification = self.completion.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notification.await;
        }
    }
}

pub(super) struct DeploymentLogSink<'a> {
    ui: &'a dyn EventSink,
    sender: Mutex<Option<SyncSender<String>>>,
    state: Arc<WriterState>,
    redactor: Redactor,
    started: Instant,
    events: bool,
    event_sequence: Mutex<()>,
}

impl<'a> DeploymentLogSink<'a> {
    #[cfg(test)]
    pub(super) fn open(
        directory: &Path,
        deployment: &DeploymentId,
        ui: &'a dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<Self, RollingLogError> {
        let mut writer =
            RollingLogWriter::open(directory, deployment, LOG_MAX_BYTES, LOG_RETAINED_FILES)?;
        Self::with_writer(
            ui,
            cancellation,
            environment_redactor(),
            QUEUED_WRITES,
            move |text| {
                writer
                    .append(text, &Redactor::default())
                    .map_err(|error| error.to_string())
            },
        )
        .map_err(|source| RollingLogError::Io {
            path: directory.to_owned(),
            source,
        })
    }

    pub(super) fn open_events(
        directory: &Path,
        deployment: &DeploymentId,
        ui: &'a dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<Self, RollingLogError> {
        let mut writer =
            RollingLogWriter::open(directory, deployment, LOG_MAX_BYTES, LOG_RETAINED_FILES)?;
        let mut sink = Self::with_writer(
            ui,
            cancellation,
            environment_redactor(),
            QUEUED_WRITES,
            move |text| {
                writer
                    .append_record(text)
                    .map_err(|_| "deployment log write failed".into())
            },
        )
        .map_err(|source| RollingLogError::Io {
            path: directory.to_owned(),
            source,
        })?;
        sink.events = true;
        Ok(sink)
    }

    fn with_writer(
        ui: &'a dyn EventSink,
        cancellation: &CancellationToken,
        redactor: Redactor,
        capacity: usize,
        mut append: impl FnMut(&str) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, std::io::Error> {
        let (sender, receiver) = mpsc::sync_channel::<String>(capacity);
        let state = Arc::new(WriterState {
            error: Mutex::new(None),
            cancellation: cancellation.clone(),
            finished: AtomicBool::new(false),
            completion: Notify::new(),
        });
        let worker = Arc::clone(&state);
        std::thread::Builder::new()
            .name("shipforge-log-writer".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for text in receiver {
                        if let Err(error) = append(&text) {
                            worker.fail(error);
                            break;
                        }
                    }
                }));
                if result.is_err() {
                    worker.fail("deployment log writer stopped unexpectedly".into());
                }
                worker.finished.store(true, Ordering::Release);
                worker.completion.notify_waiters();
            })?;
        Ok(Self {
            ui,
            sender: Mutex::new(Some(sender)),
            state,
            redactor,
            started: Instant::now(),
            events: false,
            event_sequence: Mutex::new(()),
        })
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.state.failure()
    }

    /// Closes admission and waits for queued writes without blocking the runtime.
    /// Call only after all producers and the final Deployment event have completed.
    pub(super) async fn finish(&self) -> Option<String> {
        self.finish_with_timeout(FINISH_TIMEOUT).await
    }

    async fn finish_with_timeout(&self, timeout: Duration) -> Option<String> {
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        } else {
            self.state
                .fail("deployment log sender is unavailable".into());
        }
        if tokio::time::timeout(timeout, self.state.completed())
            .await
            .is_err()
        {
            self.state
                .fail("deployment log writer did not finish before the shutdown deadline".into());
        }
        self.failure()
    }

    fn enqueue(&self, text: &str) {
        let mut remaining = text;
        while !remaining.is_empty() && self.failure().is_none() {
            let mut end = remaining.len().min(MAX_WRITE_BYTES);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            self.enqueue_complete(remaining[..end].to_owned());
            remaining = &remaining[end..];
        }
    }

    fn enqueue_complete(&self, text: String) {
        if self.failure().is_some() {
            return;
        }
        let result = self
            .sender
            .lock()
            .map_err(|_| "deployment log sender is unavailable")
            .and_then(|sender| {
                let sender = sender.as_ref().ok_or("deployment log is already closed")?;
                sender.try_send(text).map_err(|error| match error {
                    TrySendError::Full(_) => {
                        "deployment log queue is full; output could not be persisted"
                    }
                    TrySendError::Disconnected(_) => "deployment log writer disconnected",
                })
            });
        if let Err(error) = result {
            self.state.fail(error.into());
        }
    }
}

impl EventSink for DeploymentLogSink<'_> {
    fn emit(&self, event: DriverLog) {
        if self.events {
            self.emit_record(LogEvent {
                namespace: event.namespace,
                message: event.message,
                scope: None,
                kind: LogEventKind::Output,
            });
            return;
        }
        let namespace = plain_text(&self.redactor.redact(&event.namespace), 128);
        let message = self.redactor.redact(&event.message);
        // Disk receives the complete sanitized text. Only the UI projection is
        // shortened and strips terminal controls. Every queued write is bounded.
        self.enqueue(&format!(
            "{:.3}s [{namespace}] {message}\n",
            self.started.elapsed().as_secs_f64()
        ));
        self.ui.emit(DriverLog {
            namespace,
            message: plain_text(&message, MAX_UI_CHARACTERS),
        });
    }

    fn emit_record(&self, event: LogEvent) {
        if !self.events {
            self.emit(DriverLog {
                namespace: event.namespace,
                message: event.message,
            });
            return;
        }
        let Ok(events) = split_log_event(&event, &self.redactor) else {
            self.state
                .fail("deployment log event could not be safely recorded".into());
            return;
        };
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let Ok(_sequence) = self.event_sequence.lock() else {
            self.state
                .fail("deployment event ordering is unavailable".into());
            return;
        };
        let count = u32::try_from(events.len()).unwrap_or(u32::MAX);
        let event_id = uuid::Uuid::now_v7();
        for (index, event) in events.into_iter().enumerate() {
            let record = LogRecord {
                version: LOG_RECORD_VERSION,
                elapsed_ms,
                fragment: (count > 1).then_some(LogFragment {
                    event_id,
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    count,
                }),
                event,
            };
            match encode_log_record(&record, &Redactor::default()) {
                Ok(text) => self.enqueue_complete(text),
                Err(_) => self
                    .state
                    .fail("deployment log record exceeded its safe limits".into()),
            }
            // Structured events already contain bounded, sanitized fields. The
            // viewer owns window eviction; do not silently truncate full-record
            // detail or erase newlines before that viewer receives the event.
            self.ui.emit_record(record.event);
        }
    }
}

fn environment_redactor() -> Redactor {
    redactor_for_environment(std::env::vars_os())
}

fn redactor_for_environment(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Redactor {
    Redactor::new(environment.into_iter().filter_map(|(name, value)| {
        let name = name.to_string_lossy().to_ascii_uppercase();
        [
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "PASSWD",
            "API_KEY",
            "PRIVATE_KEY",
            "ACCESS_KEY",
            "CREDENTIAL",
        ]
        .iter()
        .any(|key| name.contains(key))
        .then(|| value.to_string_lossy().into_owned())
    }))
}

fn plain_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .take(limit)
        .collect()
}

/// Converts invalid text lossily, retaining incomplete UTF-8 between reads.
/// Decoding before matching also protects non-UTF-8 environment values rendered
/// through the same lossy conversion as the platform environment scanner.
struct OutputText {
    pending: Vec<u8>,
    redactor: StreamingRedactor,
}

impl OutputText {
    fn new(redactor: &Redactor) -> Self {
        Self {
            pending: Vec::new(),
            redactor: StreamingRedactor::new(redactor),
        }
    }

    fn push(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            match std::str::from_utf8(&self.pending[offset..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    offset = self.pending.len();
                }
                Err(error) => {
                    let end = offset + error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&self.pending[offset..end]));
                    offset = end;
                    if let Some(length) = error.error_len() {
                        text.push('\u{fffd}');
                        offset += length;
                    } else if eof {
                        text.push('\u{fffd}');
                        offset = self.pending.len();
                    } else {
                        break;
                    }
                }
            }
        }
        self.pending.drain(..offset);
        String::from_utf8_lossy(&self.redactor.push(text.as_bytes(), eof)).into_owned()
    }
}

pub(super) struct BuildOutputProjector<'a> {
    component: &'a ComponentName,
    events: &'a dyn EventSink,
    stdout: Mutex<OutputText>,
    stderr: Mutex<OutputText>,
}

impl<'a> BuildOutputProjector<'a> {
    pub(super) fn new(component: &'a ComponentName, events: &'a dyn EventSink) -> Self {
        Self::with_redactor(component, events, &environment_redactor())
    }

    fn with_redactor(
        component: &'a ComponentName,
        events: &'a dyn EventSink,
        redactor: &Redactor,
    ) -> Self {
        Self {
            component,
            events,
            stdout: Mutex::new(OutputText::new(redactor)),
            stderr: Mutex::new(OutputText::new(redactor)),
        }
    }

    pub(super) fn output(&self, index: usize, stream: OutputStream, bytes: &[u8]) {
        let (buffer, label) = match stream {
            OutputStream::Stdout => (&self.stdout, "stdout"),
            OutputStream::Stderr => (&self.stderr, "stderr"),
        };
        if let Ok(mut buffer) = buffer.lock() {
            let text = buffer.push(bytes, bytes.is_empty());
            if !text.is_empty() {
                self.events.emit(DriverLog {
                    namespace: format!("build.{label}"),
                    message: format!("{} / command {}: {text}", self.component, index + 1),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
