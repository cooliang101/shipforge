//! Public setup diagnostics intentionally exclude parser and credential payloads.

use crate::config::ConfigError;

pub(super) const RECENT_REFRESH_FAILED: &str = "Recent-project registry could not be refreshed. Cached entries are retained but disabled. Check registry format and permissions, then press f on Projects to retry.";

pub(super) fn configuration_error(error: &ConfigError) -> String {
    let remedy = match error {
        ConfigError::UnsafePath { .. } => {
            "Component paths must stay inside the project. Press Esc to review Components; correct the working directory or artifact path, then retry."
        }
        ConfigError::UnsafeRoot { .. } | ConfigError::OverlappingRoots { .. } => {
            "Deployment roots must be safe absolute paths and must not overlap on the same server. Press e to review each Component target, then retry."
        }
        ConfigError::EmptyBuild(_) => {
            "A Component needs a non-empty build program. Press Esc to review Components and their structured build commands, then retry."
        }
        ConfigError::Security(crate::telemetry::SecurityError::SensitiveConfig(_)) => {
            "Configuration contains a potentially sensitive value. Remove embedded credentials from build commands or other fields before retrying."
        }
        ConfigError::Security(
            crate::telemetry::SecurityError::InvalidCommand
            | crate::telemetry::SecurityError::AlreadyShell,
        ) => {
            "A build command is invalid. Review the program and individual arguments; keep them as structured values before retrying."
        }
        _ => {
            "Configuration validation failed. Review Component build arguments and paths, then review target settings before retrying."
        }
    };
    format!("Setup preview could not be prepared. {remedy} No project configuration was written.")
}

#[cfg(test)]
mod tests;
