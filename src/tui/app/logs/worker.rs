//! Tracked local-only operations; a cancelled task is still joined before dismissal.

use std::{fmt::Write as _, path::PathBuf, thread::JoinHandle};

use tokio_util::sync::CancellationToken;

use crate::{
    application::{
        history_query::{
            HistoryLogScope, HistoryQueryService, LogExportFormat, LogFilter, LogReadPage,
            LogReadQuery,
        },
        local_export::{LocalExportOutcome, LocalExportPreview, LocalExportService},
    },
    telemetry::{Redactor, log_record::sanitize_log_text},
};

use super::DirectoryBrowser;

pub(super) enum Request {
    Read {
        scope: HistoryLogScope,
        query: Box<LogReadQuery>,
    },
    Payload {
        scope: HistoryLogScope,
        filter: LogFilter,
        summary: bool,
        directory: PathBuf,
    },
    Directory {
        directory: PathBuf,
        payload: String,
        name: String,
    },
    Prepare {
        directory: PathBuf,
        payload: String,
        name: String,
    },
    Save(LocalExportPreview),
}

impl std::fmt::Debug for Request {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalLogRequest")
            .finish_non_exhaustive()
    }
}

pub(super) enum Response {
    Page {
        page: Box<LogReadPage>,
        steps: Vec<crate::history::StepRecord>,
    },
    Payload {
        text: String,
        name: String,
        browser: DirectoryBrowser,
    },
    Directory {
        browser: DirectoryBrowser,
        payload: String,
        name: String,
    },
    Preview(LocalExportPreview),
    Published(LocalExportOutcome),
}

impl std::fmt::Debug for Response {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalLogResponse")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(super) struct LogTask {
    cancellation: CancellationToken,
    handle: JoinHandle<Result<Response, String>>,
}

impl LogTask {
    pub fn spawn(history: PathBuf, request: Request) -> Result<Self, ()> {
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let handle = std::thread::Builder::new().name("shipforge-local-log-view".into()).spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(history, request, &worker_cancellation)))
                .unwrap_or_else(|_| Err("Local log/export worker stopped unexpectedly. If publication was requested, inspect the chosen path before retrying.".into()))
        }).map_err(|_| ())?;
        Ok(Self {
            cancellation,
            handle,
        })
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    pub fn join(self) -> Result<Response, String> {
        self.handle.join().unwrap_or_else(|_| Err("Local log/export worker stopped. Inspect the selected directory before retrying an export.".into()))
    }
}

fn run(
    history: PathBuf,
    request: Request,
    cancellation: &CancellationToken,
) -> Result<Response, String> {
    if cancellation.is_cancelled() {
        return Err("Local log operation cancelled before execution.".into());
    }
    let service = HistoryQueryService::new(history, Redactor::default());
    let response = match request {
        Request::Read { scope, query } => {
            let details = service
                .deployment(&scope.project, &scope.environment, &scope.deployment)
                .map_err(|error| error.to_string())?;
            let page = service
                .read_logs(&scope, *query, cancellation)
                .map_err(|error| error.to_string())?;
            Response::Page {
                page: Box::new(page),
                steps: details.steps,
            }
        }
        Request::Payload {
            scope,
            filter,
            summary,
            directory,
        } => {
            let text = if summary {
                // Summary deliberately covers the whole recorded deployment, not the log filter.
                let details = service
                    .deployment(&scope.project, &scope.environment, &scope.deployment)
                    .map_err(|error| error.to_string())?;
                summary_text(&details)?
            } else {
                let prepared = service
                    .prepare_log_export(&scope, &filter, LogExportFormat::Text, cancellation)
                    .map_err(|error| error.to_string())?;
                String::from_utf8(prepared.payload().to_vec())
                    .map_err(|_| "Prepared export was not valid text.".to_owned())?
            };
            let kind = if summary { "summary" } else { "logs" };
            let name = format!(
                "shipforge-{}-{kind}-{}.txt",
                scope.deployment,
                uuid::Uuid::now_v7()
            );
            let browser = open_directory(&directory)?;
            Response::Payload {
                text,
                name,
                browser,
            }
        }
        Request::Directory {
            directory,
            payload,
            name,
        } => Response::Directory {
            browser: open_directory(&directory)?,
            payload,
            name,
        },
        Request::Prepare {
            directory,
            payload,
            name,
        } => Response::Preview(
            LocalExportService::prepare(&directory, &name, payload)
                .map_err(|error| error.to_string())?,
        ),
        Request::Save(preview) => {
            // Publication is a known outcome even if cancellation arrives afterwards.
            return LocalExportService::confirm(preview, cancellation)
                .map(Response::Published)
                .map_err(|error| error.to_string());
        }
    };
    if cancellation.is_cancelled() {
        return Err("Local log operation cancelled; its prepared result was discarded. No export was published.".into());
    }
    Ok(response)
}

fn open_directory(directory: &std::path::Path) -> Result<DirectoryBrowser, String> {
    DirectoryBrowser::open(directory).map_err(|_| "Could not browse this local directory within safe limits. Check that it exists and is readable.".into())
}

fn summary_text(details: &crate::history::DeploymentDetails) -> Result<String, String> {
    let record = &details.record;
    let mut text = format!(
        "ShipForge recorded deployment summary\nScope: entire recorded deployment (log filters do not apply)\nProject: {}\nEnvironment: {}\nDeployment: {}\nKind: {:?}\nRecorded outcome: {:?}\nCreated: {} ms since Unix epoch\nLast recorded update: {} ms since Unix epoch (not proof of operation finish)\nPending intents: {}\n\nThis is local historical evidence, not a fresh remote health check. Missing evidence is unknown, never confirmed absence. Original outcomes are not replaced by later inspection reports.\n",
        record.project,
        record.environment,
        record.deployment,
        record.kind,
        record.state,
        record.created_at_ms,
        record.updated_at_ms,
        record.pending_intent_count
    );
    let _ = writeln!(text, "\nSelected Components and frozen targets:");
    if details.snapshots.is_empty() {
        text.push_str("Unknown: no frozen Component selection was recorded.\n");
    }
    for snapshot in &details.snapshots {
        let release = &snapshot.release;
        let _ = writeln!(
            text,
            "{}: Destination {} revision {}; Release {}",
            release.component,
            release.destination,
            release.destination_revision.get(),
            release.version
        );
    }
    text.push_str("\nRecorded Component outcomes:\n");
    if details.results.is_empty() {
        text.push_str("Unknown: no Component outcomes were recorded.\n");
    }
    for result in &details.results {
        let _ = writeln!(
            text,
            "{}: {:?}; reported version: {}",
            result.component,
            result.result.outcome,
            result
                .result
                .observed_release
                .as_ref()
                .map_or("unknown / none reported", |version| version.as_str())
        );
        if result.error.is_some() {
            text.push_str(
                "Warning: this Component has a recorded error; inspect the local detail view.\n",
            );
        }
    }
    text.push_str("\nRecorded steps:\n");
    for step in &details.steps {
        let duration = step
            .started_at_ms
            .zip(step.completed_at_ms)
            .and_then(|(start, end)| end.checked_sub(start))
            .map_or_else(|| "unknown".into(), |duration| format!("{duration} ms"));
        let _ = writeln!(
            text,
            "{} / {}: {:?}; start: {:?}; completion: {:?} (epoch ms; None means unknown); duration: {duration}",
            step.component, step.name, step.status, step.started_at_ms, step.completed_at_ms
        );
        if step.error.is_some() {
            text.push_str("Warning: a step error was recorded.\n");
        }
    }
    text.push_str("\nRecorded observations (not current health):\n");
    for observation in &details.observations {
        let version = match &observation.observed {
            Ok(Some(release)) => release.version.as_str(),
            Ok(None) => "confirmed not deployed at this observation",
            Err(_) => "unknown: observation failed",
        };
        let _ = writeln!(
            text,
            "{} / {}: {}; health: {:?}; observed: {} epoch ms",
            observation.component,
            observation.stage,
            version,
            observation.healthy,
            observation.observed_at_ms
        );
    }
    if !details.pending.is_empty() {
        text.push_str(
            "\nWarning: unresolved intents remain. Do not infer that their effects completed.\n",
        );
    }
    if details.log.is_none() {
        text.push_str("Warning: no local log index was recorded.\n");
    }
    sanitize_log_text(&text, &Redactor::default(), 8 * 1024 * 1024)
        .map_err(|_| "Recorded summary exceeds safe export limits.".into())
}

#[cfg(test)]
impl LogTask {
    /// Uses a real tracked thread while tests control its cancellation boundary.
    pub(super) fn controlled(
        cancellation: CancellationToken,
        work: impl FnOnce(CancellationToken) -> Result<Response, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let worker_cancellation = cancellation.clone();
        let handle = std::thread::Builder::new()
            .name("shipforge-log-test-barrier".into())
            .spawn(move || work(worker_cancellation))?;
        Ok(Self {
            cancellation,
            handle,
        })
    }
}
