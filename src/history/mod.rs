//! Deployment intent journal and bounded sanitized raw logs.

mod logs;
mod store;

pub use logs::{RollingLogError, RollingLogWriter};
pub use store::{
    HistoryError, HistoryStore, IntentId, IntentRecord, IntentStatus, PersistedComponentResult,
};

/// Returns the platform-local history database path.
///
/// # Errors
///
/// Returns an error when the user configuration directory is unavailable.
pub fn default_history_path() -> Result<std::path::PathBuf, HistoryError> {
    crate::adapters::user_config_directory()
        .map(|directory| directory.join("history.sqlite3"))
        .map_err(|error| HistoryError::ConfigurationDirectory(error.to_string()))
}

/// Returns the platform-local rolling log directory.
///
/// # Errors
///
/// Returns an error when the user configuration directory is unavailable.
pub fn default_log_directory() -> Result<std::path::PathBuf, HistoryError> {
    crate::adapters::user_config_directory()
        .map(|directory| directory.join("logs"))
        .map_err(|error| HistoryError::ConfigurationDirectory(error.to_string()))
}
