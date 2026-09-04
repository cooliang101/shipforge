use std::any::Any;

use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::HostKeyFingerprint,
    drivers::{DriverDestinationInput, DriverKind, ValidatedDestinationSettings},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxSshDestination {
    pub(super) driver: DriverKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_key: HostKeyFingerprint,
}

impl LinuxSshDestination {
    /// Validates the non-secret connection fields resolved from the Destination
    /// registry. Credentials remain a separate execution-context handle.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown fields, an empty host or user, port zero,
    /// or an invalid Host Key fingerprint.
    pub fn validate(input: &DriverDestinationInput) -> Result<Self, LinuxSshDestinationError> {
        let raw: RawLinuxSshDestination =
            serde_json::from_value(input.value.clone()).map_err(LinuxSshDestinationError::Shape)?;
        if raw.port == 0 {
            return Err(LinuxSshDestinationError::Port);
        }
        validate_atom("host", &raw.host)?;
        validate_atom("user", &raw.user)?;
        let host_key = HostKeyFingerprint::parse(raw.host_key)
            .map_err(|error| LinuxSshDestinationError::HostKey(error.to_string()))?;
        Ok(Self {
            driver: DriverKind::linux_ssh(),
            host: raw.host,
            port: raw.port,
            user: raw.user,
            host_key,
        })
    }
}

impl ValidatedDestinationSettings for LinuxSshDestination {
    fn driver_kind(&self) -> &DriverKind {
        &self.driver
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn validate_atom(field: &'static str, value: &str) -> Result<(), LinuxSshDestinationError> {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        Err(LinuxSshDestinationError::Field { field })
    } else {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawLinuxSshDestination {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    user: String,
    host_key: String,
}

const fn default_port() -> u16 {
    22
}

#[derive(Debug, Error)]
pub enum LinuxSshDestinationError {
    #[error("invalid linux-ssh Destination fields: {0}")]
    Shape(serde_json::Error),
    #[error("linux-ssh {field} must be non-empty and contain no whitespace")]
    Field { field: &'static str },
    #[error("linux-ssh port must be non-zero")]
    Port,
    #[error("invalid linux-ssh Host Key fingerprint: {0}")]
    HostKey(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_resolved_connection_without_credentials() {
        let destination = LinuxSshDestination::validate(&DriverDestinationInput {
            value: serde_json::json!({
                "host": "app.example.com",
                "port": 22,
                "user": "deploy",
                "hostKey": "SHA256:confirmed"
            }),
        })
        .unwrap();
        assert_eq!(destination.driver_kind().as_str(), DriverKind::LINUX_SSH);
        assert_eq!(destination.port, 22);
    }

    #[test]
    fn rejects_secret_or_unknown_connection_fields() {
        assert!(
            LinuxSshDestination::validate(&DriverDestinationInput {
                value: serde_json::json!({
                    "host": "app.example.com",
                    "user": "deploy",
                    "hostKey": "SHA256:confirmed",
                    "privateKey": "secret"
                }),
            })
            .is_err()
        );
    }
}
