//! Bounded local history views. These APIs never connect, migrate or create history.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

use thiserror::Error;

use crate::{
    domain::{DeploymentId, EnvironmentId, ProjectId},
    history::{
        DeploymentQuery, DeploymentRecord, HistoryError, HistoryStore, RecoveryQuery,
        RecoveryReport,
    },
    telemetry::Redactor,
};

pub use crate::history::DeploymentDetails;

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOG_GENERATIONS: u32 = 16;
const MAX_LOG_PAGE_BYTES: u32 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage<T> {
    pub items: Vec<T>,
    pub more: bool,
    pub database_missing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoricalLogQuery {
    /// Zero is the active file; higher generations are progressively older rotations.
    pub generation: u32,
    /// UTF-8 byte offset in the sanitized view, never an offset into unredacted bytes.
    pub offset: u64,
    pub max_bytes: u32,
}

impl Default for HistoricalLogQuery {
    fn default() -> Self {
        Self {
            generation: 0,
            offset: 0,
            max_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoricalLogStatus {
    Ready,
    NotIndexed,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoricalLogPage {
    pub status: HistoricalLogStatus,
    pub generation: u32,
    pub offset: u64,
    pub next_offset: Option<u64>,
    pub total_bytes: u64,
    pub available_generations: Vec<u32>,
    pub text: String,
}

/// Failures contain no raw database fields, file contents or credential paths.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HistoryQueryError {
    #[error("Local history database is missing")]
    DatabaseMissing,
    #[error("Local history uses an older schema; viewing never upgrades it")]
    NeedsUpgrade,
    #[error("Local history uses an unsupported future schema")]
    FutureSchema,
    #[error("The requested history record does not exist in this Project and Environment")]
    NotFound,
    #[error("History pagination is outside its bounded limits")]
    InvalidPage,
    #[error("Stored history is corrupt, unavailable or exceeds safe read limits")]
    HistoryUnavailable,
    #[error("Historical log path is linked, non-regular or outside the application log directory")]
    UnsafeLog,
    #[error("Historical log could not be read")]
    LogIo,
    #[error("Historical log changed or rotated during reading; reload the page")]
    LogChanged,
    #[error("Historical log exceeds bounded file or text limits")]
    LogLimit,
}

#[derive(Clone, Debug)]
pub struct HistoryQueryService {
    history_path: PathBuf,
    redactor: Redactor,
}

impl HistoryQueryService {
    #[must_use]
    pub const fn new(history_path: PathBuf, redactor: Redactor) -> Self {
        Self {
            history_path,
            redactor,
        }
    }

    /// Lists persisted Environment IDs independently of the current project YAML.
    /// Includes report-only environments, sorted by exact ID rather than inferred name.
    /// This read-only index never grants permission to operate on an old target.
    ///
    /// # Errors
    /// Rejects invalid pages, unavailable schemas, malformed IDs and index overflow.
    pub fn environments(
        &self,
        project: &ProjectId,
        query: RecoveryQuery,
    ) -> Result<HistoryPage<EnvironmentId>, HistoryQueryError> {
        validate_page(query.limit, query.offset)?;
        let store = match self.open() {
            Ok(store) => store,
            Err(HistoryQueryError::DatabaseMissing) => return Ok(missing_page()),
            Err(error) => return Err(error),
        };
        let (items, more) = store
            .read_environment_page(project, query)
            .map_err(history_error)?;
        Ok(HistoryPage {
            items,
            more,
            database_missing: false,
        })
    }

    /// Lists persisted Deployments without connecting or creating a missing database.
    ///
    /// # Errors
    /// Rejects invalid pages, unavailable schemas and malformed persisted records.
    pub fn deployments(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: DeploymentQuery,
    ) -> Result<HistoryPage<DeploymentRecord>, HistoryQueryError> {
        validate_page(query.limit, query.offset)?;
        let store = match self.open() {
            Ok(store) => store,
            Err(HistoryQueryError::DatabaseMissing) => return Ok(missing_page()),
            Err(error) => return Err(error),
        };
        let (items, more) = store
            .read_deployment_page(project, environment, query)
            .map_err(history_error)?;
        Ok(HistoryPage {
            items,
            more,
            database_missing: false,
        })
    }

    /// Reads an original Deployment, preserving unknown versus absent observations.
    ///
    /// # Errors
    /// Rejects missing/out-of-scope records, corrupt details and bounded-read overflow.
    pub fn deployment(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        id: &DeploymentId,
    ) -> Result<DeploymentDetails, HistoryQueryError> {
        let store = self.open()?;
        let mut details = store
            .read_deployment_details(project, environment, id)
            .map_err(history_error)?
            .ok_or(HistoryQueryError::NotFound)?;
        self.sanitize_details(&mut details)?;
        Ok(details)
    }

    /// Lists saved inspection evidence; no new remote inspection is performed.
    ///
    /// # Errors
    /// Rejects invalid pages, unavailable schemas and corrupt stored evidence.
    pub fn recovery_reports(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: RecoveryQuery,
    ) -> Result<HistoryPage<RecoveryReport>, HistoryQueryError> {
        validate_page(query.limit, query.offset)?;
        let store = match self.open() {
            Ok(store) => store,
            Err(HistoryQueryError::DatabaseMissing) => return Ok(missing_page()),
            Err(error) => return Err(error),
        };
        let (mut items, more) = store
            .read_recovery_page(project, environment, query)
            .map_err(history_error)?;
        for report in &mut items {
            self.sanitize_report(report)?;
        }
        Ok(HistoryPage {
            items,
            more,
            database_missing: false,
        })
    }

    /// Reads one immutable saved inspection report within the selected scope.
    ///
    /// # Errors
    /// Rejects missing/out-of-scope records or corrupt persisted evidence.
    pub fn recovery_report(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        id: &uuid::Uuid,
    ) -> Result<RecoveryReport, HistoryQueryError> {
        let mut report = self
            .open()?
            .read_recovery_detail(project, environment, id)
            .map_err(history_error)?
            .ok_or(HistoryQueryError::NotFound)?;
        self.sanitize_report(&mut report)?;
        Ok(report)
    }

    /// Reads an indexed, fixed-name local rolling log. Redaction precedes pagination.
    /// Offset and size refer to the sanitized view; persisted bytes are never rewritten.
    ///
    /// # Errors
    /// Rejects arbitrary/linked paths, changed files, oversized data and invalid offsets.
    pub fn log_page(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        id: &DeploymentId,
        query: HistoricalLogQuery,
    ) -> Result<HistoricalLogPage, HistoryQueryError> {
        if query.max_bytes == 0
            || query.max_bytes > MAX_LOG_PAGE_BYTES
            || query.generation > MAX_LOG_GENERATIONS
            || query.offset > MAX_TEXT_BYTES as u64
        {
            return Err(HistoryQueryError::InvalidPage);
        }
        let details = self.deployment(project, environment, id)?;
        let Some(index) = details.log else {
            return Ok(empty_log(
                query,
                HistoricalLogStatus::NotIndexed,
                Vec::new(),
            ));
        };
        if index.max_bytes > MAX_LOG_BYTES || index.retained_files > MAX_LOG_GENERATIONS {
            return Err(HistoryQueryError::LogLimit);
        }
        if query.generation > index.retained_files {
            return Err(HistoryQueryError::InvalidPage);
        }
        // The SQL path is validated by the store, but is never used as filesystem authority.
        let parent = self
            .history_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|_| HistoryQueryError::LogIo)?;
        let directory = parent.join("logs");
        if !directory_present(&directory)? {
            return Ok(empty_log(query, HistoricalLogStatus::Missing, Vec::new()));
        }
        let mut available = Vec::new();
        for generation in 0..=index.retained_files {
            if regular_metadata(&log_path(&directory, id, generation))?.is_some() {
                available.push(generation);
            }
        }
        if !available.contains(&query.generation) {
            return Ok(empty_log(query, HistoricalLogStatus::Missing, available));
        }
        let path = log_path(&directory, id, query.generation);
        let bytes = read_log(&directory, &path, index.max_bytes)?;
        // Writers may retain a UTF-8 tail or split a chunk; invalid bytes are visible replacements.
        let text = self.safe_text(&String::from_utf8_lossy(&bytes))?;
        paginate_log(&text, query, available)
    }

    fn open(&self) -> Result<HistoryStore, HistoryQueryError> {
        HistoryStore::open_existing_read_only(&self.history_path).map_err(history_error)
    }

    fn safe_text(&self, text: &str) -> Result<String, HistoryQueryError> {
        let mut text = text.to_owned();
        for secret in self.redactor.values() {
            text = text.replace(secret, "[REDACTED]");
            if text.len() > MAX_TEXT_BYTES {
                return Err(HistoryQueryError::LogLimit);
            }
        }
        text.retain(|character| {
            (!character.is_control() || matches!(character, '\n' | '\t'))
                && !matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        });
        // Removing terminal controls can join previously separated secret bytes.
        // Reapply redaction before deriving any page offsets from the visible text.
        for secret in self.redactor.values() {
            text = text.replace(secret, "[REDACTED]");
            if text.len() > MAX_TEXT_BYTES {
                return Err(HistoryQueryError::LogLimit);
            }
        }
        if text.len() > MAX_TEXT_BYTES {
            return Err(HistoryQueryError::LogLimit);
        }
        Ok(text)
    }

    fn sanitize_details(&self, details: &mut DeploymentDetails) -> Result<(), HistoryQueryError> {
        if let Some(metadata) = &mut details.metadata {
            sanitize_option(&mut metadata.git_branch, |text| self.safe_text(text))?;
            sanitize_option(&mut metadata.operator, |text| self.safe_text(text))?;
        }
        for result in &mut details.results {
            sanitize_option(&mut result.error, |text| self.safe_text(text))?;
        }
        for step in &mut details.steps {
            step.name = self.safe_text(&step.name)?;
            sanitize_option(&mut step.error, |text| self.safe_text(text))?;
        }
        for observation in &mut details.observations {
            observation.stage = self.safe_text(&observation.stage)?;
            if let Err(error) = &mut observation.observed {
                *error = self.safe_text(error)?;
            }
        }
        for pending in &mut details.pending {
            pending.stage = self.safe_text(&pending.stage)?;
            pending.target = self.safe_text(&pending.target)?;
        }
        for receipt in &mut details.receipts {
            receipt.stage = self.safe_text(&receipt.stage)?;
        }
        Ok(())
    }

    fn sanitize_report(&self, report: &mut RecoveryReport) -> Result<(), HistoryQueryError> {
        for component in &mut report.components {
            for notice in &mut component.notices {
                *notice = self.safe_text(notice)?;
            }
            match &mut component.inventory {
                Err(error) => *error = self.safe_text(error)?,
                Ok(inventory) => {
                    if let Err(error) = &mut inventory.releases.current {
                        *error = self.safe_text(error)?;
                    }
                    for issue in &mut inventory.releases.issues {
                        issue.message = self.safe_text(&issue.message)?;
                    }
                    for notice in inventory
                        .releases
                        .notices
                        .iter_mut()
                        .chain(&mut inventory.audit.notices)
                        .chain(&mut inventory.remnants.notices)
                    {
                        *notice = self.safe_text(notice)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn sanitize_option(
    value: &mut Option<String>,
    sanitizer: impl FnOnce(&str) -> Result<String, HistoryQueryError>,
) -> Result<(), HistoryQueryError> {
    if let Some(text) = value {
        *text = sanitizer(text)?;
    }
    Ok(())
}

fn validate_page(limit: u32, offset: u32) -> Result<(), HistoryQueryError> {
    if !(1..=100).contains(&limit) || offset > 1_000_000 {
        Err(HistoryQueryError::InvalidPage)
    } else {
        Ok(())
    }
}

fn missing_page<T>() -> HistoryPage<T> {
    HistoryPage {
        items: Vec::new(),
        more: false,
        database_missing: true,
    }
}

fn history_error(error: HistoryError) -> HistoryQueryError {
    match error {
        HistoryError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            HistoryQueryError::DatabaseMissing
        }
        HistoryError::InvalidMetadata("history schema requires upgrade before viewing") => {
            HistoryQueryError::NeedsUpgrade
        }
        HistoryError::UnsupportedSchema(_) => HistoryQueryError::FutureSchema,
        HistoryError::InvalidPage => HistoryQueryError::InvalidPage,
        _ => HistoryQueryError::HistoryUnavailable,
    }
}

fn empty_log(
    query: HistoricalLogQuery,
    status: HistoricalLogStatus,
    available_generations: Vec<u32>,
) -> HistoricalLogPage {
    HistoricalLogPage {
        status,
        generation: query.generation,
        offset: query.offset,
        next_offset: None,
        total_bytes: 0,
        available_generations,
        text: String::new(),
    }
}

fn log_path(directory: &Path, id: &DeploymentId, generation: u32) -> PathBuf {
    if generation == 0 {
        directory.join(format!("{id}.log"))
    } else {
        directory.join(format!("{id}.log.{generation}"))
    }
}

fn linked(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn directory_present(path: &Path) -> Result<bool, HistoryQueryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !linked(&metadata) && metadata.is_dir() => Ok(true),
        Ok(_) => Err(HistoryQueryError::UnsafeLog),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(HistoryQueryError::LogIo),
    }
}

fn regular_metadata(path: &Path) -> Result<Option<Metadata>, HistoryQueryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !linked(&metadata) && metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err(HistoryQueryError::UnsafeLog),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(HistoryQueryError::LogIo),
    }
}

fn open_log(path: &Path) -> Result<File, HistoryQueryError> {
    regular_metadata(path)?.ok_or(HistoryQueryError::LogChanged)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0002_0000 | 0x800); // O_NOFOLLOW | O_NONBLOCK
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100 | 0x4); // O_NOFOLLOW | O_NONBLOCK
    }
    options.open(path).map_err(|_| HistoryQueryError::LogIo)
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: Option<SystemTime>,
    identity: (u64, u64),
}

fn file_stamp(file: &File) -> Result<FileStamp, HistoryQueryError> {
    let metadata = file.metadata().map_err(|_| HistoryQueryError::LogIo)?;
    if !metadata.is_file() || linked(&metadata) {
        return Err(HistoryQueryError::UnsafeLog);
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(HistoryQueryError::UnsafeLog);
        }
        (metadata.dev(), metadata.ino())
    };
    #[cfg(windows)]
    let identity = {
        let info = winapi_util::file::information(file).map_err(|_| HistoryQueryError::LogIo)?;
        if info.number_of_links() != 1 {
            return Err(HistoryQueryError::UnsafeLog);
        }
        (info.volume_serial_number(), info.file_index())
    };
    Ok(FileStamp {
        size: metadata.len(),
        modified: metadata.modified().ok(),
        identity,
    })
}

fn read_log(
    directory: &Path,
    path: &Path,
    indexed_limit: u64,
) -> Result<Vec<u8>, HistoryQueryError> {
    let mut file = open_log(path)?;
    let before = file_stamp(&file)?;
    if before.size > indexed_limit || before.size > MAX_LOG_BYTES {
        return Err(HistoryQueryError::LogLimit);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(indexed_limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HistoryQueryError::LogIo)?;
    if bytes.len() as u64 != before.size || file_stamp(&file)? != before {
        return Err(HistoryQueryError::LogChanged);
    }
    if !directory_present(directory)? || file_stamp(&open_log(path)?)? != before {
        return Err(HistoryQueryError::LogChanged);
    }
    Ok(bytes)
}

fn paginate_log(
    text: &str,
    query: HistoricalLogQuery,
    available_generations: Vec<u32>,
) -> Result<HistoricalLogPage, HistoryQueryError> {
    let start = usize::try_from(query.offset).map_err(|_| HistoryQueryError::InvalidPage)?;
    if start > text.len() || !text.is_char_boundary(start) {
        return Err(HistoryQueryError::InvalidPage);
    }
    let mut end = start
        .saturating_add(query.max_bytes as usize)
        .min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == start && end < text.len() {
        return Err(HistoryQueryError::InvalidPage);
    }
    Ok(HistoricalLogPage {
        status: HistoricalLogStatus::Ready,
        generation: query.generation,
        offset: query.offset,
        next_offset: (end < text.len()).then_some(end as u64),
        total_bytes: text.len() as u64,
        available_generations,
        text: text[start..end].to_owned(),
    })
}

#[cfg(test)]
mod tests;
