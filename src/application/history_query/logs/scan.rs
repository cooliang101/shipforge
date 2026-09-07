use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{
    CancellationToken, DeploymentId, DeploymentLogFormat, DeploymentLogRecord, FileStamp,
    HistoricalLogStatus, HistoryLogScope, HistoryQueryError, HistoryQueryService, LogCoverage,
    LogCoverageIssue, LogEntry, LogEntryContent, LogFilter, LogRecord, MAX_LOG_BYTES,
    MAX_LOG_GENERATIONS, MAX_SCAN_BYTES, MAX_SCAN_ENTRIES, check_cancelled, directory_present,
    file_stamp, fs, log_path, open_log, regular_metadata,
};
use crate::telemetry::log_record::{LogCodecError, LogEventKind, parse_log_record};
mod fragments;
mod legacy;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ScanSnapshot {
    index: Option<DeploymentLogRecord>,
    directory: Option<DirectoryStamp>,
    files: Vec<(u32, FileStamp)>,
    content_digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryStamp {
    identity: (u64, u64),
    modified: Option<SystemTime>,
}

pub(super) struct LogScan {
    pub(super) entries: Vec<LogEntry>,
    pub(super) coverage: LogCoverage,
    pub(super) snapshot: ScanSnapshot,
    bytes: usize,
    inspected: usize,
    pending: Vec<fragments::RawEntry>,
    pub(super) legacy_independent: bool,
    pub(super) legacy_pages: BTreeMap<u32, String>,
}

impl LogScan {
    fn empty(index: Option<DeploymentLogRecord>) -> Self {
        let format = index.as_ref().map(|index| index.format);
        Self {
            entries: Vec::new(),
            coverage: LogCoverage {
                status: if index.is_none() {
                    HistoricalLogStatus::NotIndexed
                } else {
                    HistoricalLogStatus::Missing
                },
                format,
                available_generations: Vec::new(),
                issues: Vec::new(),
                matches_are_complete: false,
            },
            snapshot: ScanSnapshot {
                index,
                directory: None,
                files: Vec::new(),
                content_digest: [0; 32],
            },
            bytes: 0,
            inspected: 0,
            pending: Vec::new(),
            legacy_independent: true,
            legacy_pages: BTreeMap::new(),
        }
    }

    fn issue(&mut self, issue: LogCoverageIssue) {
        if !self.coverage.issues.contains(&issue) {
            self.coverage.issues.push(issue);
        }
        self.coverage.matches_are_complete = false;
    }

    fn push(
        &mut self,
        generation: u32,
        source_generations: Vec<u32>,
        content: LogEntryContent,
        matches: bool,
    ) -> Result<(), HistoryQueryError> {
        self.inspected = self.inspected.saturating_add(1);
        if self.inspected > MAX_SCAN_ENTRIES {
            return Err(HistoryQueryError::LogLimit);
        }
        if matches {
            let entry = LogEntry {
                generation,
                source_generations,
                ordinal: u64::try_from(self.inspected).map_err(|_| HistoryQueryError::LogLimit)?,
                content,
            };
            self.bytes = self.bytes.saturating_add(entry.bytes());
            if self.bytes > MAX_SCAN_BYTES {
                return Err(HistoryQueryError::LogLimit);
            }
            self.entries.push(entry);
        }
        Ok(())
    }
}

impl HistoryQueryService {
    pub(super) fn scan_logs(
        &self,
        scope: &HistoryLogScope,
        filter: &LogFilter,
        cancellation: &CancellationToken,
    ) -> Result<LogScan, HistoryQueryError> {
        check_cancelled(cancellation)?;
        let index = self
            .deployment(&scope.project, &scope.environment, &scope.deployment)?
            .log;
        let mut scan = LogScan::empty(index.clone());
        let Some(index) = index else {
            return Ok(scan);
        };
        if index.max_bytes > MAX_LOG_BYTES || index.retained_files > MAX_LOG_GENERATIONS {
            return Err(HistoryQueryError::LogLimit);
        }
        let directory = self.log_directory()?;
        let before = snapshot(&directory, &scope.deployment, &index, cancellation)?;
        scan.snapshot = before.clone();
        scan.coverage.available_generations = before
            .files
            .iter()
            .map(|(generation, _)| *generation)
            .collect();
        if !before.files.is_empty() {
            scan.coverage.status = HistoricalLogStatus::Ready;
            scan.coverage.matches_are_complete = true;
        }
        assess_coverage(&mut scan, filter);
        let mut total_bytes = 0_usize;
        let mut digest = Sha256::new();
        let mut legacy_runs = legacy::LegacyRuns::default();
        for (generation, expected) in before.files.iter().rev() {
            check_cancelled(cancellation)?;
            total_bytes = total_bytes.saturating_add(
                usize::try_from(expected.size).map_err(|_| HistoryQueryError::LogLimit)?,
            );
            if total_bytes > MAX_SCAN_BYTES {
                return Err(HistoryQueryError::LogLimit);
            }
            let path = log_path(&directory, &scope.deployment, *generation);
            let bytes = read_generation(&path, expected, cancellation)?;
            digest.update(generation.to_le_bytes());
            digest.update(expected.size.to_le_bytes());
            digest.update(&bytes);
            match index.format {
                DeploymentLogFormat::JsonlV1 => {
                    self.scan_records(&mut scan, *generation, &bytes, filter, cancellation)?;
                }
                DeploymentLogFormat::LegacyText => {
                    legacy_runs.push(*generation, bytes);
                }
            }
        }
        legacy_runs.project(&mut scan, &self.redactor, filter, cancellation)?;
        scan.finish_fragments(&self.redactor, filter, cancellation)?;
        check_cancelled(cancellation)?;
        if snapshot(&directory, &scope.deployment, &index, cancellation)? != before
            || self
                .deployment(&scope.project, &scope.environment, &scope.deployment)?
                .log
                .as_ref()
                != Some(&index)
        {
            return Err(HistoryQueryError::LogChanged);
        }
        scan.snapshot.content_digest = digest.finalize().into();
        Ok(scan)
    }

    fn log_directory(&self) -> Result<PathBuf, HistoryQueryError> {
        let parent = self
            .history_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|_| HistoryQueryError::LogIo)?;
        Ok(parent.join("logs"))
    }

    fn scan_records(
        &self,
        scan: &mut LogScan,
        generation: u32,
        bytes: &[u8],
        filter: &LogFilter,
        cancellation: &CancellationToken,
    ) -> Result<(), HistoryQueryError> {
        let mut offset = 0_usize;
        let mut ordinal = 0_u64;
        while offset < bytes.len() {
            check_cancelled(cancellation)?;
            let Some(end) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
                scan.finish_fragments(&self.redactor, filter, cancellation)?;
                scan.issue(LogCoverageIssue::TornRecord { generation });
                break;
            };
            ordinal = ordinal.saturating_add(1);
            if ordinal > MAX_SCAN_ENTRIES as u64 {
                return Err(HistoryQueryError::LogLimit);
            }
            let line = &bytes[offset..offset + end];
            offset += end + 1;
            match parse_log_record(line) {
                Ok(record) => {
                    if record.event.scope.is_none()
                        && (filter.component.is_some() || filter.step.is_some())
                    {
                        scan.issue(LogCoverageIssue::UnscopedRecords { generation });
                    }
                    scan.receive_record(
                        fragments::RawEntry { generation, record },
                        &self.redactor,
                        filter,
                        cancellation,
                    )?;
                }
                Err(LogCodecError::UnsupportedVersion) => {
                    scan.finish_fragments(&self.redactor, filter, cancellation)?;
                    scan.issue(LogCoverageIssue::UnsupportedRecord { generation });
                }
                Err(_) => {
                    scan.finish_fragments(&self.redactor, filter, cancellation)?;
                    scan.issue(LogCoverageIssue::InvalidRecord { generation });
                }
            }
        }
        Ok(())
    }
}

fn record_matches(record: &LogRecord, filter: &LogFilter) -> bool {
    let scope = record.event.scope.as_ref();
    if filter
        .component
        .as_ref()
        .is_some_and(|component| scope.is_none_or(|scope| &scope.component != component))
        || filter
            .step
            .as_ref()
            .is_some_and(|step| scope.is_none_or(|scope| &scope.step != step))
    {
        return false;
    }
    let event = &record.event;
    contains_text(&event.message, &filter.text)
        || contains_text(&event.namespace, &filter.text)
        || scope.is_some_and(|scope| {
            contains_text(&scope.step, &filter.text)
                || contains_text(scope.component.as_str(), &filter.text)
        })
        || match &event.kind {
            LogEventKind::FailedCommand { command } => {
                contains_text(&command.program, &filter.text)
                    || command
                        .working_directory
                        .as_ref()
                        .is_some_and(|directory| contains_text(directory, &filter.text))
                    || command
                        .args
                        .iter()
                        .any(|argument| contains_text(argument, &filter.text))
            }
            _ => false,
        }
}

fn contains_text(value: &str, query: &str) -> bool {
    query.is_empty() || value.to_lowercase().contains(query)
}

fn assess_coverage(scan: &mut LogScan, filter: &LogFilter) {
    if scan.coverage.format == Some(DeploymentLogFormat::LegacyText) {
        scan.coverage.issues.push(LogCoverageIssue::LegacyUnscoped);
        if filter.component.is_some() || filter.step.is_some() {
            scan.coverage.matches_are_complete = false;
        }
    }
    if !scan.coverage.available_generations.contains(&0) {
        scan.issue(LogCoverageIssue::MissingActive);
    }
    if let Some(last) = scan.coverage.available_generations.last().copied()
        && (0..=last).any(|generation| !scan.coverage.available_generations.contains(&generation))
    {
        scan.issue(LogCoverageIssue::GenerationGap);
    }
}

fn snapshot(
    directory: &Path,
    id: &DeploymentId,
    index: &DeploymentLogRecord,
    cancellation: &CancellationToken,
) -> Result<ScanSnapshot, HistoryQueryError> {
    let directory_before = directory_stamp(directory)?;
    let mut files = Vec::new();
    if directory_before.is_some() {
        for generation in 0..=index.retained_files {
            check_cancelled(cancellation)?;
            let path = log_path(directory, id, generation);
            if regular_metadata(&path)?.is_some() {
                let stamp = file_stamp(&open_log(&path)?)?;
                if stamp.size > index.max_bytes || stamp.size > MAX_LOG_BYTES {
                    return Err(HistoryQueryError::LogLimit);
                }
                files.push((generation, stamp));
            }
        }
    }
    if directory_stamp(directory)? != directory_before {
        return Err(HistoryQueryError::LogChanged);
    }
    Ok(ScanSnapshot {
        index: Some(index.clone()),
        directory: directory_before,
        files,
        content_digest: [0; 32],
    })
}

fn directory_stamp(path: &Path) -> Result<Option<DirectoryStamp>, HistoryQueryError> {
    if !directory_present(path)? {
        return Ok(None);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0200_0000 | 0x0020_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "linux")]
        options.custom_flags(0x0002_0000 | 0x800);
        #[cfg(target_os = "macos")]
        options.custom_flags(0x100 | 0x4);
    }
    let file = options.open(path).map_err(|_| HistoryQueryError::LogIo)?;
    let metadata = file.metadata().map_err(|_| HistoryQueryError::LogIo)?;
    if !metadata.is_dir() || super::super::linked(&metadata) {
        return Err(HistoryQueryError::UnsafeLog);
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(windows)]
    let identity = {
        let info = winapi_util::file::information(&file).map_err(|_| HistoryQueryError::LogIo)?;
        (info.volume_serial_number(), info.file_index())
    };
    Ok(Some(DirectoryStamp {
        identity,
        modified: metadata.modified().ok(),
    }))
}

fn read_generation(
    path: &Path,
    expected: &FileStamp,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, HistoryQueryError> {
    let mut file = open_log(path)?;
    if file_stamp(&file)? != *expected {
        return Err(HistoryQueryError::LogChanged);
    }
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        check_cancelled(cancellation)?;
        let count = file
            .read(&mut buffer)
            .map_err(|_| HistoryQueryError::LogIo)?;
        if count == 0 {
            break;
        }
        if u64::try_from(bytes.len().saturating_add(count))
            .map_err(|_| HistoryQueryError::LogLimit)?
            > MAX_LOG_BYTES
        {
            return Err(HistoryQueryError::LogLimit);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as u64 != expected.size
        || file_stamp(&file)? != *expected
        || file_stamp(&open_log(path)?)? != *expected
    {
        return Err(HistoryQueryError::LogChanged);
    }
    Ok(bytes)
}
