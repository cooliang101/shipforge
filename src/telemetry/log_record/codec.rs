//! Bounded encoding and safe projection of positively indexed structured logs.

use thiserror::Error;

use super::{
    LOG_RECORD_VERSION, LogEvent, LogEventKind, LogRecord, MAX_COMMAND_ARGUMENTS,
    MAX_COMMAND_BYTES, MAX_MESSAGE_BYTES, MAX_RECORD_BYTES, RecordedCommand,
};
use crate::telemetry::{Redactor, StreamingRedactor};

const MAX_EVENT_TEXT: usize = 16 * 1024 * 1024;
const MAX_LABEL_BYTES: usize = 256;

#[derive(serde::Deserialize)]
struct RecordVersion {
    version: u64,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum LogCodecError {
    #[error("structured log record exceeds its bounded limits")]
    Limit,
    #[error("structured log record has an unsupported version")]
    UnsupportedVersion,
    #[error("structured log record is malformed or has invalid fields")]
    Invalid,
}

/// Decodes only an indexed JSONL record, never arbitrary legacy stdout.
///
/// # Errors
/// Rejects excess bytes, unknown fields/versions and malformed field values.
pub fn decode_log_record(bytes: &[u8], redactor: &Redactor) -> Result<LogRecord, LogCodecError> {
    let mut record = parse_log_record(bytes)?;
    if record.fragment.is_some() {
        record.event.message =
            "Fragment body requires complete retained-event reconstruction".into();
    }
    record.event = sanitize_log_event(&record.event, redactor)?;
    if record.event.message.len() > MAX_MESSAGE_BYTES {
        return Err(LogCodecError::Limit);
    }
    Ok(record)
}

// Only the historical reader may retain this unprojected value, in its private
// raw-entry state. No caller may display its message before full event sanitation.
pub(crate) fn parse_log_record(bytes: &[u8]) -> Result<LogRecord, LogCodecError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(LogCodecError::Limit);
    }
    let version: RecordVersion =
        serde_json::from_slice(bytes).map_err(|_| LogCodecError::Invalid)?;
    if version.version != u64::from(LOG_RECORD_VERSION) {
        return Err(LogCodecError::UnsupportedVersion);
    }
    let record: LogRecord = serde_json::from_slice(bytes).map_err(|_| LogCodecError::Invalid)?;
    if record.version != LOG_RECORD_VERSION {
        return Err(LogCodecError::UnsupportedVersion);
    }
    if record.event.message.len() > MAX_MESSAGE_BYTES {
        return Err(LogCodecError::Limit);
    }
    validate_fragment(&record)?;
    validate_label(&record.event.namespace)?;
    if let Some(scope) = &record.event.scope {
        validate_label(&scope.step)?;
    }
    Ok(record)
}

/// Encodes one complete, sanitized JSON line including its final newline.
///
/// # Errors
/// Rejects unsupported versions, oversized messages or invalid metadata.
pub fn encode_log_record(record: &LogRecord, redactor: &Redactor) -> Result<String, LogCodecError> {
    if record.version != LOG_RECORD_VERSION {
        return Err(LogCodecError::UnsupportedVersion);
    }
    if record.event.message.len() > MAX_MESSAGE_BYTES {
        return Err(LogCodecError::Limit);
    }
    validate_fragment(record)?;
    let record = LogRecord {
        version: record.version,
        elapsed_ms: record.elapsed_ms,
        fragment: record.fragment,
        event: sanitize_log_event(&record.event, redactor)?,
    };
    if record.event.message.len() > MAX_MESSAGE_BYTES {
        return Err(LogCodecError::Limit);
    }
    let mut text = serde_json::to_string(&record).map_err(|_| LogCodecError::Invalid)?;
    if text.len() >= MAX_RECORD_BYTES {
        return Err(LogCodecError::Limit);
    }
    text.push('\n');
    Ok(text)
}

/// Removes secrets and terminal controls from fields, not from serialized JSON.
/// Oversized command snapshots become explicit unavailable-command evidence.
///
/// # Errors
/// Rejects invalid scope/namespace or messages exceeding the full event budget.
pub fn sanitize_log_event(
    event: &LogEvent,
    redactor: &Redactor,
) -> Result<LogEvent, LogCodecError> {
    validate_label(&event.namespace)?;
    if let Some(scope) = &event.scope {
        validate_label(&scope.step)?;
    }
    if event.message.len() > MAX_EVENT_TEXT {
        return Err(LogCodecError::Limit);
    }
    let kind = match &event.kind {
        LogEventKind::FailedCommand { command } => sanitize_command(command, redactor).map_or_else(
            |_| LogEventKind::CommandUnavailable {
                location: command.location,
                index: command.index.filter(|index| *index > 0),
            },
            |command| LogEventKind::FailedCommand { command },
        ),
        LogEventKind::CommandUnavailable { index: Some(0), .. } => {
            return Err(LogCodecError::Invalid);
        }
        kind => kind.clone(),
    };
    let mut event = LogEvent {
        namespace: event.namespace.clone(),
        message: sanitize_log_text(&event.message, redactor, MAX_EVENT_TEXT)?,
        scope: event.scope.clone(),
        kind,
    };
    event.namespace = sanitize_log_text(&event.namespace, redactor, MAX_LABEL_BYTES)?;
    validate_label(&event.namespace)?;
    if let Some(scope) = &mut event.scope {
        validate_label(&scope.step)?;
        scope.step = sanitize_log_text(&scope.step, redactor, MAX_LABEL_BYTES)?;
        validate_label(&scope.step)?;
    }
    if event.scope.as_ref().is_some_and(|scope| {
        sanitize_log_text(scope.component.as_str(), redactor, MAX_LABEL_BYTES)
            .map_or(true, |safe| safe != scope.component.as_str())
    }) {
        // Never invent a different Component identity after redaction.
        event.scope = None;
    }
    Ok(event)
}

/// Preserves complete sanitized text using bounded UTF-8 records with the same scope.
/// Only the first fragment repeats a lifecycle or failed-command event kind.
///
/// # Errors
/// Rejects invalid metadata and an event exceeding the aggregate text budget.
pub fn split_log_event(
    event: &LogEvent,
    redactor: &Redactor,
) -> Result<Vec<LogEvent>, LogCodecError> {
    let mut event = sanitize_log_event(event, redactor)?;
    let message = std::mem::take(&mut event.message);
    if message.is_empty() {
        return Ok(vec![event]);
    }
    let mut remaining = message.as_str();
    let mut events = Vec::new();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(MAX_MESSAGE_BYTES);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        let mut fragment = event.clone();
        remaining[..end].clone_into(&mut fragment.message);
        if !events.is_empty() {
            fragment.kind = LogEventKind::Output;
        }
        events.push(fragment);
        remaining = &remaining[end..];
    }
    Ok(events)
}

/// Produces the same full safe text for display, matching, copying and export.
///
/// # Errors
/// Rejects original or expanded text above the supplied byte budget.
pub fn sanitize_log_text(
    value: &str,
    redactor: &Redactor,
    max_bytes: usize,
) -> Result<String, LogCodecError> {
    if value.len() > max_bytes {
        return Err(LogCodecError::Limit);
    }
    // Preserve PEM syntax until it has been recognized. A registered value may
    // itself contain "BEGIN"; substituting it first would destroy key boundaries.
    let normalized: String = value
        .chars()
        .filter(|character| terminal_safe(*character))
        .collect();
    let protected = strip_private_key(&normalized);
    let text = redact_bounded(&protected, redactor, max_bytes)?;
    if text.len() > max_bytes {
        return Err(LogCodecError::Limit);
    }
    Ok(text)
}

fn redact_bounded(
    value: &str,
    redactor: &Redactor,
    max_bytes: usize,
) -> Result<String, LogCodecError> {
    if value.len() > max_bytes {
        return Err(LogCodecError::Limit);
    }
    let mut text = value.to_owned();
    for secret in normalized_secrets(redactor, max_bytes)? {
        let growth = "[REDACTED]".len().saturating_sub(secret.len());
        if text
            .len()
            .saturating_add(text.matches(secret.as_str()).count().saturating_mul(growth))
            > max_bytes
        {
            return Err(LogCodecError::Limit);
        }
        text = text.replace(&secret, "[REDACTED]");
    }
    Ok(text)
}

fn terminal_safe(character: char) -> bool {
    (!character.is_control() || matches!(character, '\n' | '\t'))
        && !matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn normalized_secret(value: &str, max_bytes: usize) -> Option<String> {
    let mut safe = String::new();
    for character in value.chars().filter(|character| terminal_safe(*character)) {
        if safe.len().saturating_add(character.len_utf8()) > max_bytes {
            return None;
        }
        safe.push(character);
    }
    Some(safe)
}

fn normalized_secrets(redactor: &Redactor, max_bytes: usize) -> Result<Vec<String>, LogCodecError> {
    let mut secrets = Vec::new();
    let mut budget = 0_usize;
    for original in redactor.values() {
        let Some(secret) =
            normalized_secret(original, max_bytes).filter(|secret| !secret.is_empty())
        else {
            continue;
        };
        budget = budget.saturating_add(secret.len()).saturating_add(128);
        if budget > MAX_EVENT_TEXT {
            return Err(LogCodecError::Limit);
        }
        secrets.push(secret);
    }
    // Removing controls can change relative lengths; retain longest-first
    // replacement after normalization, not merely the original registry order.
    secrets
        .sort_unstable_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    secrets.dedup();
    Ok(secrets)
}

fn strip_private_key(value: &str) -> String {
    String::from_utf8_lossy(
        &StreamingRedactor::new(&Redactor::default()).push(value.as_bytes(), true),
    )
    .into_owned()
}

fn validate_label(value: &str) -> Result<(), LogCodecError> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES || value.chars().any(char::is_control) {
        Err(LogCodecError::Invalid)
    } else {
        Ok(())
    }
}

fn validate_fragment(record: &LogRecord) -> Result<(), LogCodecError> {
    if record.fragment.is_some_and(|fragment| {
        fragment.event_id.is_nil()
            || fragment.count < 2
            || fragment.count > 1025
            || fragment.index >= fragment.count
            || (fragment.index > 0 && record.event.kind != LogEventKind::Output)
    }) {
        Err(LogCodecError::Invalid)
    } else {
        Ok(())
    }
}

fn sanitize_command(
    command: &RecordedCommand,
    redactor: &Redactor,
) -> Result<RecordedCommand, LogCodecError> {
    if command.program.is_empty()
        || command.index == Some(0)
        || command.args.len() > MAX_COMMAND_ARGUMENTS
    {
        return Err(LogCodecError::Invalid);
    }
    let bytes = command
        .args
        .iter()
        .fold(command.program.len(), |sum, argument| {
            sum.saturating_add(argument.len())
        });
    if bytes > MAX_COMMAND_BYTES {
        return Err(LogCodecError::Limit);
    }
    let mut safe = command.clone();
    safe.program = sanitize_log_text(&safe.program, redactor, MAX_COMMAND_BYTES)?;
    let mut hide_next = false;
    for argument in &mut safe.args {
        // Controls can conceal a sensitive flag name, so classify its safe spelling.
        let original = sanitize_log_text(argument, &Redactor::default(), MAX_COMMAND_BYTES)?;
        *argument = if hide_next {
            "[REDACTED]".into()
        } else if let Some((name, _)) = original.split_once('=')
            && sensitive_name(name)
        {
            format!(
                "{}=[REDACTED]",
                sanitize_log_text(name, redactor, MAX_COMMAND_BYTES)?
            )
        } else if sensitive_header(&original)
            || (sensitive_name(&original) && original.chars().any(char::is_whitespace))
        {
            "[REDACTED]".into()
        } else {
            sanitize_log_text(&original, redactor, MAX_COMMAND_BYTES)?
        };
        hide_next = !hide_next
            && !original.contains(['=', ':'])
            && !original.chars().any(char::is_whitespace)
            && sensitive_name(&original);
    }
    let bytes = safe.args.iter().fold(safe.program.len(), |sum, argument| {
        sum.saturating_add(argument.len())
    });
    if bytes > MAX_COMMAND_BYTES || safe.program.is_empty() {
        return Err(LogCodecError::Limit);
    }
    Ok(safe)
}

fn sensitive_name(value: &str) -> bool {
    let normalized = value
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace(['-', '_'], "");
    [
        "password",
        "passwd",
        "token",
        "secret",
        "apikey",
        "privatekey",
        "accesskey",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|name| normalized.contains(name))
}

fn sensitive_header(value: &str) -> bool {
    value.split_once(':').is_some_and(|(name, _)| {
        sensitive_name(name)
            || matches!(
                name.trim().to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
            )
    })
}

#[cfg(test)]
mod tests;
