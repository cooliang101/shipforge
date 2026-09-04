//! Structured events and sensitive-data redaction.

mod security;

pub use security::{
    CommandArgument, CommandSpec, HostKeyPolicy, Redactor, Secret, SecurityError,
    detect_sensitive_config, quote_posix_argument,
};
