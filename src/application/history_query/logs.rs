//! Complete retained-generation scans, with bounded safe projections and frozen cursors.

use std::fmt::Write as _;

use tokio_util::sync::CancellationToken;

use super::{
    FileStamp, HistoricalLogStatus, HistoryQueryError, HistoryQueryService, MAX_LOG_BYTES,
    MAX_LOG_GENERATIONS, directory_present, file_stamp, fs, log_path, open_log, regular_metadata,
};
use crate::{
    domain::{ComponentName, DeploymentId, EnvironmentId, ProjectId},
    history::{DeploymentLogFormat, DeploymentLogRecord},
    telemetry::log_record::{LogRecord, sanitize_log_text},
};

mod scan;
use scan::{LogScan, ScanSnapshot};
#[cfg(test)]
mod tests;

const MAX_SCAN_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCAN_ENTRIES: usize = 32_768;
const MAX_PAGE_BYTES: usize = 256 * 1024;
const MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PAGE_ENTRIES: u32 = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryLogScope {
    pub project: ProjectId,
    pub environment: EnvironmentId,
    pub deployment: DeploymentId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    pub component: Option<ComponentName>,
    pub step: Option<String>,
    /// Literal, case-insensitive match against complete sanitized messages and command fields.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogReadQuery {
    pub filter: LogFilter,
    pub cursor: Option<LogCursor>,
    pub limit: u32,
}

impl Default for LogReadQuery {
    fn default() -> Self {
        Self {
            filter: LogFilter::default(),
            cursor: None,
            limit: 100,
        }
    }
}

/// Opaque and usable only for the exact scope, safe filter and file snapshot it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCursor {
    scope: HistoryLogScope,
    filter: LogFilter,
    snapshot: ScanSnapshot,
    next: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogEntryContent {
    Structured(LogRecord),
    /// Legacy output never supplies trusted scope, elapsed time or command evidence.
    Legacy(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// A display anchor only; safely reprojected bodies may span several generations.
    pub generation: u32,
    pub source_generations: Vec<u32>,
    /// One-based ordinal in the sanitized view, never an original line or byte offset.
    pub ordinal: u64,
    pub content: LogEntryContent,
}

impl LogEntry {
    #[must_use]
    pub fn message(&self) -> &str {
        match &self.content {
            LogEntryContent::Structured(record) => &record.event.message,
            LogEntryContent::Legacy(text) => text,
        }
    }

    fn bytes(&self) -> usize {
        match &self.content {
            LogEntryContent::Structured(record) => {
                let event = &record.event;
                let command = match &event.kind {
                    crate::telemetry::log_record::LogEventKind::FailedCommand { command } => {
                        command.args.iter().fold(command.program.len(), |sum, arg| {
                            sum.saturating_add(arg.len())
                        })
                    }
                    _ => 0,
                };
                event
                    .message
                    .len()
                    .saturating_add(event.namespace.len())
                    .saturating_add(command)
                    .saturating_add(512)
            }
            LogEntryContent::Legacy(text) => text.len().saturating_add(128),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogCoverageIssue {
    LegacyUnscoped,
    UnscopedRecords { generation: u32 },
    MissingActive,
    GenerationGap,
    TornRecord { generation: u32 },
    UnsupportedRecord { generation: u32 },
    InvalidRecord { generation: u32 },
    IncompleteFragments { generation: u32 },
    IncompleteLegacy { generation: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCoverage {
    pub status: HistoricalLogStatus,
    pub format: Option<DeploymentLogFormat>,
    /// Only these retained files were inspected; older deleted rotations are unknowable.
    pub available_generations: Vec<u32>,
    pub issues: Vec<LogCoverageIssue>,
    /// Completeness concerns matching within the retained files, never lifetime history.
    pub matches_are_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogReadPage {
    pub entries: Vec<LogEntry>,
    pub total_matches: usize,
    pub next_cursor: Option<LogCursor>,
    pub coverage: LogCoverage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogExportFormat {
    Text,
    Json,
}

/// A complete safe local payload; this value does not authorize or perform a file write.
pub struct PreparedLogExport {
    payload: Vec<u8>,
    pub format: LogExportFormat,
    pub coverage: LogCoverage,
}

impl std::fmt::Debug for PreparedLogExport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedLogExport")
            .field("bytes", &self.payload.len())
            .field("format", &self.format)
            .field("coverage", &self.coverage)
            .finish()
    }
}

impl PreparedLogExport {
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl HistoryQueryService {
    /// Searches every retained generation before paging complete safe entries.
    /// Legacy lines have no inferred scope, even if their text resembles event JSON.
    ///
    /// # Errors
    /// Rejects stale cursors, changed/unsafe files, cancelled scans and explicit resource overflow.
    pub fn read_logs(
        &self,
        scope: &HistoryLogScope,
        query: LogReadQuery,
        cancellation: &CancellationToken,
    ) -> Result<LogReadPage, HistoryQueryError> {
        if query.limit == 0 || query.limit > MAX_PAGE_ENTRIES {
            return Err(HistoryQueryError::InvalidPage);
        }
        let filter = self.log_filter(query.filter)?;
        let scan = self.scan_logs(scope, &filter, cancellation)?;
        let start = match query.cursor {
            Some(cursor)
                if cursor.scope == *scope
                    && cursor.filter == filter
                    && cursor.snapshot == scan.snapshot =>
            {
                cursor.next
            }
            Some(_) => return Err(HistoryQueryError::LogChanged),
            None => 0,
        };
        if start > scan.entries.len() {
            return Err(HistoryQueryError::InvalidPage);
        }
        let total_matches = scan.entries.len();
        let mut entries = Vec::new();
        let mut bytes = 0_usize;
        for entry in scan
            .entries
            .into_iter()
            .skip(start)
            .take(query.limit as usize)
        {
            check_cancelled(cancellation)?;
            if bytes.saturating_add(entry.bytes()) > MAX_PAGE_BYTES {
                break;
            }
            bytes = bytes.saturating_add(entry.bytes());
            entries.push(entry);
        }
        if entries.is_empty() && start < total_matches {
            return Err(HistoryQueryError::LogLimit);
        }
        let next = start + entries.len();
        let next_cursor = (next < total_matches).then_some(LogCursor {
            scope: scope.clone(),
            filter,
            snapshot: scan.snapshot,
            next,
        });
        Ok(LogReadPage {
            entries,
            total_matches,
            next_cursor,
            coverage: scan.coverage,
        })
    }

    /// Prepares all matching retained output and coverage, not merely the current page.
    /// No file is created and no raw command is executed.
    ///
    /// # Errors
    /// Rejects scan failures and exports above the complete-payload limit; never returns a truncated success.
    pub fn prepare_log_export(
        &self,
        scope: &HistoryLogScope,
        filter: &LogFilter,
        format: LogExportFormat,
        cancellation: &CancellationToken,
    ) -> Result<PreparedLogExport, HistoryQueryError> {
        let filter = self.log_filter(filter.clone())?;
        let scan = self.scan_logs(scope, &filter, cancellation)?;
        let payload = match format {
            LogExportFormat::Text => export_text(scope, &filter, &scan, cancellation)?,
            LogExportFormat::Json => export_json(scope, &filter, &scan, cancellation)?,
        };
        check_cancelled(cancellation)?;
        Ok(PreparedLogExport {
            payload,
            format,
            coverage: scan.coverage,
        })
    }

    fn log_filter(&self, mut filter: LogFilter) -> Result<LogFilter, HistoryQueryError> {
        if filter.text.chars().count() > 128
            || filter.step.as_ref().is_some_and(|step| {
                step.is_empty() || step.len() > 256 || step.chars().any(char::is_control)
            })
        {
            return Err(HistoryQueryError::InvalidPage);
        }
        if let Some(component) = &filter.component {
            let safe = sanitize_log_text(component.as_str(), &self.redactor, 256)
                .map_err(|_| HistoryQueryError::InvalidPage)?;
            if safe != component.as_str() {
                return Err(HistoryQueryError::InvalidPage);
            }
        }
        filter.text = sanitize_log_text(&filter.text, &self.redactor, 4096)
            .map_err(|_| HistoryQueryError::InvalidPage)?;
        filter.text = filter.text.to_lowercase();
        if filter.text.len() > 4096 {
            return Err(HistoryQueryError::InvalidPage);
        }
        if let Some(step) = &mut filter.step {
            *step = sanitize_log_text(step, &self.redactor, 256)
                .map_err(|_| HistoryQueryError::InvalidPage)?;
        }
        Ok(filter)
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), HistoryQueryError> {
    if cancellation.is_cancelled() {
        Err(HistoryQueryError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn compatibility_text(
    bytes: &[u8],
    redactor: &crate::telemetry::Redactor,
) -> Result<String, HistoryQueryError> {
    let mut text = String::new();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(HistoryQueryError::HistoryUnavailable);
    }
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let record = crate::telemetry::log_record::decode_log_record(line, redactor)
            .map_err(|_| HistoryQueryError::HistoryUnavailable)?;
        if record.fragment.is_some() {
            return Err(HistoryQueryError::HistoryUnavailable);
        }
        writeln!(
            text,
            "{}ms [{}] {}",
            record.elapsed_ms, record.event.namespace, record.event.message
        )
        .map_err(|_| HistoryQueryError::LogLimit)?;
        if text.len() > MAX_SCAN_BYTES {
            return Err(HistoryQueryError::LogLimit);
        }
    }
    Ok(text)
}

pub(super) fn compatibility_legacy_page(
    service: &HistoryQueryService,
    scope: &HistoryLogScope,
    generation: u32,
) -> Result<(String, Vec<u32>), HistoryQueryError> {
    let mut scan = service.scan_logs(scope, &LogFilter::default(), &CancellationToken::new())?;
    if !scan.legacy_independent || scan.coverage.format != Some(DeploymentLogFormat::LegacyText) {
        return Err(HistoryQueryError::HistoryUnavailable);
    }
    // Return bytes from this same verified scan. A later independent-context
    // proof must never authorize bytes read by an earlier file snapshot.
    let text = scan
        .legacy_pages
        .remove(&generation)
        .ok_or(HistoryQueryError::LogChanged)?;
    Ok((text, scan.coverage.available_generations))
}

fn export_text(
    scope: &HistoryLogScope,
    filter: &LogFilter,
    scan: &LogScan,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, HistoryQueryError> {
    let mut text = format!(
        "ShipForge retained local log export\nProject: {}\nEnvironment: {}\nDeployment: {}\nCoverage: {:?}; generations {:?}; matching complete within retained files: {}\nIssues: {:?}\nOlder deleted rotations are not included. Legacy text has no trusted step or command metadata.\n\n",
        scope.project,
        scope.environment,
        scope.deployment,
        scan.coverage.status,
        scan.coverage.available_generations,
        scan.coverage.matches_are_complete,
        scan.coverage.issues
    );
    writeln!(
        text,
        "Filter: component={:?}; step={:?}; text={:?} (literal case-insensitive)\n",
        filter.component, filter.step, filter.text
    )
    .map_err(|_| HistoryQueryError::LogLimit)?;
    for entry in &scan.entries {
        check_cancelled(cancellation)?;
        match &entry.content {
            LogEntryContent::Structured(record) => {
                writeln!(
                    text,
                    "[source generations {:?} / sanitized view fragment {}; not an original record number / {}ms / {}]",
                    entry.source_generations, entry.ordinal, record.elapsed_ms, record.event.namespace
                )
                .map_err(|_| HistoryQueryError::LogLimit)?;
                if let Some(scope) = &record.event.scope {
                    writeln!(text, "Component: {}; step: {}", scope.component, scope.step)
                        .map_err(|_| HistoryQueryError::LogLimit)?;
                }
                let kind = serde_json::to_string(&record.event.kind)
                    .map_err(|_| HistoryQueryError::LogLimit)?;
                writeln!(text, "Evidence: {kind}\n{}", record.event.message)
                    .map_err(|_| HistoryQueryError::LogLimit)?;
            }
            LogEntryContent::Legacy(value) => {
                writeln!(
                    text,
                    "[legacy source generations {:?} / sanitized view fragment {}; not an original line number]\n{value}",
                    entry.source_generations, entry.ordinal
                )
                .map_err(|_| HistoryQueryError::LogLimit)?;
            }
        }
        if text.len() > MAX_EXPORT_BYTES {
            return Err(HistoryQueryError::LogLimit);
        }
    }
    Ok(text.into_bytes())
}

fn export_json(
    scope: &HistoryLogScope,
    filter: &LogFilter,
    scan: &LogScan,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, HistoryQueryError> {
    let header = serde_json::json!({"export_version":1,"project":scope.project,"environment":scope.environment,"deployment":scope.deployment,"filter":{"component":filter.component,"step":filter.step,"text":filter.text,"case_sensitive":false},"coverage":{"status":format!("{:?}",scan.coverage.status),"format":scan.coverage.format.map(|format| format!("{format:?}")),"generations":scan.coverage.available_generations,"matches_are_complete":scan.coverage.matches_are_complete,"issues":scan.coverage.issues.iter().map(|issue| format!("{issue:?}")).collect::<Vec<_>>(),"retained_only":true}});
    let mut text = serde_json::to_string(&header).map_err(|_| HistoryQueryError::LogLimit)?;
    text.pop();
    text.push_str(",\"entries\":[");
    for (index, entry) in scan.entries.iter().enumerate() {
        check_cancelled(cancellation)?;
        if index > 0 {
            text.push(',');
        }
        let value = match &entry.content {
            LogEntryContent::Structured(record) => {
                serde_json::json!({"source_generations":entry.source_generations,"sanitized_view_fragment":entry.ordinal,"record":record})
            }
            LogEntryContent::Legacy(legacy) => {
                serde_json::json!({"source_generations":entry.source_generations,"sanitized_view_fragment":entry.ordinal,"legacy":legacy})
            }
        };
        text.push_str(&serde_json::to_string(&value).map_err(|_| HistoryQueryError::LogLimit)?);
        if text.len().saturating_add(2) > MAX_EXPORT_BYTES {
            return Err(HistoryQueryError::LogLimit);
        }
    }
    text.push_str("]}");
    Ok(text.into_bytes())
}
