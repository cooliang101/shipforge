//! Structured events and sensitive-data redaction.

mod security;
mod stream_redaction;

pub(crate) use stream_redaction::StreamingRedactor;

pub use security::{
    CommandArgument, CommandSpec, HostKeyPolicy, Redactor, Secret, SecurityError,
    detect_sensitive_config, quote_posix_argument,
};
