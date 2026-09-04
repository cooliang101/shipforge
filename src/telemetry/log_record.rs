//! Internal execution evidence, separate from arbitrary process output.
//!
//! Only indexed versioned log files may be decoded as these records. Text that
//! resembles JSON inside a command's output never supplies scope or commands.

use serde::{Deserialize, Serialize};

use crate::domain::{ComponentName, DeploymentId};

mod codec;
pub(crate) use codec::parse_log_record;
pub use codec::{
    LogCodecError, decode_log_record, encode_log_record, sanitize_log_event, sanitize_log_text,
    split_log_event,
};

pub const LOG_RECORD_VERSION: u8 = 1;
pub const MAX_RECORD_BYTES: usize = 128 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;
pub const MAX_COMMAND_ARGUMENTS: usize = 128;

/// The Component and durable operation step known by the emitting application.
/// A Driver namespace alone does not establish either field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogScope {
    pub component: ComponentName,
    pub step: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandLocation {
    Local,
    Remote,
}

/// A diagnostic argv snapshot, never an executable request. Producers and
/// readers must redact every field; modified values need not be runnable.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedCommand {
    pub location: CommandLocation,
    /// One-based build-command index when known; remote commands need no index.
    pub index: Option<u32>,
    pub program: String,
    pub args: Vec<String>,
}

impl std::fmt::Debug for RecordedCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordedCommand")
            .field("location", &self.location)
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStepState {
    Started,
    Succeeded,
    Failed,
    Skipped,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogPersistence {
    Recorded,
    Unconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LogEventKind {
    Output,
    Step {
        state: LogStepState,
        persistence: LogPersistence,
    },
    FailedCommand {
        command: RecordedCommand,
    },
    CommandUnavailable {
        location: CommandLocation,
        index: Option<u32>,
    },
    DeploymentStarted {
        deployment: DeploymentId,
    },
}

/// Structured evidence travels alongside existing Driver logs. Missing scope
/// stays missing, including legacy history and unscoped diagnostic output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEvent {
    pub namespace: String,
    pub message: String,
    pub scope: Option<LogScope>,
    pub kind: LogEventKind,
}

impl std::fmt::Debug for LogEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LogEvent").finish_non_exhaustive()
    }
}

/// One complete JSON line in a positively identified versioned log file.
/// `elapsed_ms` is recorded by the operation's monotonic log sink, not inferred
/// from message text, file timestamps or the current wall clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRecord {
    pub version: u8,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment: Option<LogFragment>,
    pub event: LogEvent,
}

/// Explicit adjacency evidence for one output event split into bounded records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogFragment {
    pub event_id: uuid::Uuid,
    /// Zero-based index; count is at least two and bounded by the event text budget.
    pub index: u32,
    pub count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_command_arguments_or_message() {
        let command = RecordedCommand {
            location: CommandLocation::Local,
            index: Some(1),
            program: "secret-program-sentinel".into(),
            args: vec!["secret-argument-sentinel".into()],
        };
        assert!(!format!("{command:?}").contains("sentinel"));
        let record = LogRecord {
            version: LOG_RECORD_VERSION,
            elapsed_ms: 12,
            fragment: None,
            event: LogEvent {
                namespace: "secret-namespace-sentinel".into(),
                message: "secret-message-sentinel".into(),
                scope: None,
                kind: LogEventKind::FailedCommand { command },
            },
        };
        assert!(!format!("{record:?}").contains("sentinel"));
    }

    #[test]
    fn serialization_keeps_output_distinct_from_event_metadata() {
        let record = LogRecord {
            version: LOG_RECORD_VERSION,
            elapsed_ms: 99,
            fragment: None,
            event: LogEvent {
                namespace: "build.stdout".into(),
                message: "{\"kind\":\"failed_command\"}\nsecond line".into(),
                scope: None,
                kind: LogEventKind::Output,
            },
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains('\n'));
        assert_eq!(serde_json::from_str::<LogRecord>(&json).unwrap(), record);
    }

    #[test]
    fn unknown_fields_and_kinds_are_not_execution_evidence() {
        assert!(serde_json::from_str::<LogEventKind>("{\"kind\":\"future\"}").is_err());
        assert!(
            serde_json::from_str::<LogRecord>(
                "{\"version\":1,\"elapsed_ms\":1,\"event\":{},\"scope\":\"injected\"}"
            )
            .is_err()
        );
    }
}
