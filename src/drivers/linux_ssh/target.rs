use std::{any::Any, fmt};

use serde::Deserialize;
use thiserror::Error;

use crate::drivers::{DriverKind, DriverTargetInput, ValidatedTargetSettings};

const MAX_REMOTE_ROOT_BYTES: usize = 3900;
const MAX_HEALTH_URL_BYTES: usize = 2048;

#[derive(Clone, PartialEq, Eq)]
pub struct LinuxSshTarget {
    driver: DriverKind,
    pub root: String,
    pub service: Option<crate::config::ServiceConfig>,
    pub health: Option<String>,
}

impl fmt::Debug for LinuxSshTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinuxSshTarget")
            .field("driver", &self.driver)
            .field("root", &self.root)
            .field("service", &self.service)
            .field("health", &self.health.as_ref().map(|_| "[REDACTED URL]"))
            .finish()
    }
}

impl LinuxSshTarget {
    /// Validates the `linux-ssh` Component target payload.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown fields, unsafe roots, incomplete systemd
    /// unit names, or unsupported health URLs.
    pub fn validate(input: &DriverTargetInput) -> Result<Self, LinuxSshTargetError> {
        let raw: RawLinuxSshTarget =
            serde_json::from_value(input.value.clone()).map_err(LinuxSshTargetError::Shape)?;
        if !valid_root(&raw.root) {
            return Err(LinuxSshTargetError::Root(raw.root));
        }
        let service = raw.service;
        if let Some(service) = &service {
            service
                .validate()
                .map_err(|_| LinuxSshTargetError::Service)?;
        }
        if let Some(health) = &raw.health
            && !valid_health_url(health)
        {
            return Err(LinuxSshTargetError::Health);
        }
        Ok(Self {
            driver: DriverKind::linux_ssh(),
            root: raw.root,
            service,
            health: raw.health,
        })
    }
}

fn valid_health_url(value: &str) -> bool {
    if value.is_empty()
        || value.len() > MAX_HEALTH_URL_BYTES
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b'\\')
        || value.contains('#')
    {
        return false;
    }
    let Some(remainder) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = remainder
        .split_once(['/', '?'])
        .map_or(remainder, |(authority, _)| authority);
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    if authority.starts_with('[') {
        let Some(end) = authority.find(']') else {
            return false;
        };
        let host = &authority[1..end];
        let suffix = &authority[end + 1..];
        return !host.is_empty()
            && host
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':')
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port));
    }
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        && port.is_none_or(valid_port)
}

fn valid_port(value: &str) -> bool {
    value.parse::<u16>().is_ok_and(|port| port != 0)
}

impl ValidatedTargetSettings for LinuxSshTarget {
    fn driver_kind(&self) -> &DriverKind {
        &self.driver
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
    fn snapshot(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"root": self.root, "service": self.service, "health": self.health}))
    }
    fn requires_recovery_snapshot(&self) -> bool {
        self.service.is_some()
    }
}

fn valid_root(root: &str) -> bool {
    root.starts_with('/')
        && root.len() > 1
        && root.len() <= MAX_REMOTE_ROOT_BYTES
        && root.split('/').skip(1).all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.chars().any(char::is_control)
        })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLinuxSshTarget {
    root: String,
    service: Option<crate::config::ServiceConfig>,
    health: Option<String>,
}

#[derive(Debug, Error)]
pub enum LinuxSshTargetError {
    #[error("invalid or ambiguous remote service command configuration")]
    Service,
    #[error("invalid linux-ssh target fields: {0}")]
    Shape(serde_json::Error),
    #[error(
        "linux-ssh root must be a normalized absolute POSIX path of at most {MAX_REMOTE_ROOT_BYTES} bytes: `{0}`"
    )]
    Root(String),
    #[error("health must be a bounded HTTP or HTTPS URL without credentials or fragments")]
    Health,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_driver_specific_target_without_leaking_it_into_core() {
        let target = LinuxSshTarget::validate(&DriverTargetInput {
            value: serde_json::json!({
                "root": "/srv/shipforge/mall/production/api",
                "service": crate::config::ServiceConfig::systemd("mall-api.service"),
                "health": "http://127.0.0.1:8080/health"
            }),
        })
        .unwrap();
        assert_eq!(target.driver_kind().as_str(), DriverKind::LINUX_SSH);
        let debug = format!("{target:?}");
        assert!(!debug.contains("127.0.0.1"));
        assert!(debug.contains("[REDACTED URL]"));
    }

    #[test]
    fn rejects_traversal_and_incomplete_service_names() {
        assert!(matches!(
            LinuxSshTarget::validate(&DriverTargetInput {
                value: serde_json::json!({
                    "root": "/srv/app/../other",
                    "service": null,
                    "health": null
                }),
            }),
            Err(LinuxSshTargetError::Root(_))
        ));
        assert!(matches!(
            LinuxSshTarget::validate(&DriverTargetInput {
                value: serde_json::json!({
                    "root": "/srv/app",
                    "service": crate::config::ServiceConfig::systemd("api"),
                    "health": null
                }),
            }),
            Err(LinuxSshTargetError::Service)
        ));
        assert!(matches!(
            LinuxSshTarget::validate(&DriverTargetInput {
                value: serde_json::json!({
                    "root": "/srv/app",
                    "service": crate::config::ServiceConfig::systemd("api;restart.service"),
                    "health": null
                }),
            }),
            Err(LinuxSshTargetError::Service)
        ));
        assert!(matches!(
            LinuxSshTarget::validate(&DriverTargetInput {
                value: serde_json::json!({
                    "root": format!("/{}", "a".repeat(MAX_REMOTE_ROOT_BYTES)),
                    "service": null,
                    "health": null
                }),
            }),
            Err(LinuxSshTargetError::Root(_))
        ));
    }

    #[test]
    fn rejects_ambiguous_or_credential_bearing_health_urls() {
        for health in [
            "http://",
            "HTTP://localhost/health",
            "http://user:pass@localhost/health",
            "http://localhost:0/health",
            "http://localhost/health#secret",
            "http://local host/health",
        ] {
            assert!(matches!(
                LinuxSshTarget::validate(&DriverTargetInput {
                    value: serde_json::json!({
                        "root": "/srv/app",
                        "service": null,
                        "health": health,
                    }),
                }),
                Err(LinuxSshTargetError::Health)
            ));
        }
    }
}
