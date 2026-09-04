//! Capability-based Deployment Driver SPI and built-in Driver registry.

pub mod linux_ssh;

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    Capability, ComponentGeneration, ComponentName, ComponentRelease, DestinationKey,
    DestinationRevision, DriverCapabilities, EnvironmentId, ProjectId, ReleaseVersion,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DriverKind(String);

impl DriverKind {
    pub const LINUX_SSH: &'static str = "linux-ssh";

    /// Creates a Driver kind used for registry lookup and persisted metadata.
    ///
    /// # Errors
    ///
    /// Returns an error unless `value` is lowercase kebab-case.
    pub fn parse(value: impl Into<String>) -> Result<Self, DriverError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 63
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-')
            && !value.contains("--");
        if valid {
            Ok(Self(value))
        } else {
            Err(DriverError::configuration(
                "driver.kind",
                "Driver kind must be lowercase kebab-case",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub(crate) fn linux_ssh() -> Self {
        Self(Self::LINUX_SSH.into())
    }
}

impl fmt::Display for DriverKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CredentialHandle(String);

impl fmt::Debug for CredentialHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialHandle([REDACTED])")
    }
}

impl CredentialHandle {
    #[must_use]
    pub fn new() -> Self {
        Self(format!("cred_{}", uuid::Uuid::now_v7().simple()))
    }

    /// Creates a non-secret reference to credentials held outside Project config.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or control-character-containing handle.
    pub fn parse(value: impl Into<String>) -> Result<Self, DriverError> {
        let value = value.into();
        let valid = value.strip_prefix("cred_").is_some_and(|identifier| {
            identifier.len() == 32
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if valid {
            Ok(Self(value))
        } else {
            Err(DriverError::configuration(
                "credential.handle",
                "Credential handle must be an auto-generated cred_ identifier",
            ))
        }
    }

    #[must_use]
    pub fn expose_reference(&self) -> &str {
        &self.0
    }
}

impl Default for CredentialHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<String> for CredentialHandle {
    type Error = DriverError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<CredentialHandle> for String {
    fn from(value: CredentialHandle) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EndpointFingerprint(String);

impl EndpointFingerprint {
    /// Parses a stable non-secret endpoint fingerprint.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value is a lowercase SHA-256 hex digest.
    pub fn parse(value: impl Into<String>) -> Result<Self, DriverError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(DriverError::configuration(
                "destination.endpointFingerprint",
                "Endpoint fingerprint must be a lowercase SHA-256 digest",
            ))
        }
    }

    pub(crate) fn from_sha256_hex(value: String) -> Self {
        debug_assert_eq!(value.len(), 64);
        Self(value)
    }
}

impl TryFrom<String> for EndpointFingerprint {
    type Error = DriverError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<EndpointFingerprint> for String {
    fn from(value: EndpointFingerprint) -> Self {
        value.0
    }
}

pub trait ValidatedTargetSettings: Any + fmt::Debug + Send + Sync {
    fn driver_kind(&self) -> &DriverKind;
    fn as_any(&self) -> &dyn Any;
}

pub trait ValidatedDestinationSettings: Any + fmt::Debug + Send + Sync {
    fn driver_kind(&self) -> &DriverKind;
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug)]
pub struct DriverDestinationInput {
    pub value: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct DriverTargetInput {
    pub value: serde_json::Value,
}

#[derive(Clone)]
pub struct ComponentExecutionContext {
    pub project_id: ProjectId,
    pub environment_id: EnvironmentId,
    pub component: ComponentName,
    pub generation: ComponentGeneration,
    pub destination: DestinationKey,
    pub destination_revision: DestinationRevision,
    pub credential: CredentialHandle,
    pub endpoint_fingerprint: EndpointFingerprint,
    pub destination_settings: Arc<dyn ValidatedDestinationSettings>,
    pub target: Arc<dyn ValidatedTargetSettings>,
    pub cancellation: CancellationToken,
}

impl fmt::Debug for ComponentExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComponentExecutionContext")
            .field("project_id", &self.project_id)
            .field("environment_id", &self.environment_id)
            .field("component", &self.component)
            .field("generation", &self.generation)
            .field("destination", &self.destination)
            .field("destination_revision", &self.destination_revision)
            .field("credential", &"[REDACTED]")
            .field("endpoint_fingerprint", &self.endpoint_fingerprint)
            .field(
                "destination_driver",
                &self.destination_settings.driver_kind(),
            )
            .field("target_driver", &self.target.driver_kind())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRef {
    pub driver: DriverKind,
    pub project_id: ProjectId,
    pub environment_id: EnvironmentId,
    pub component: ComponentName,
    pub generation: ComponentGeneration,
    pub version: ReleaseVersion,
    pub destination: DestinationKey,
    pub destination_revision: DestinationRevision,
    pub endpoint_fingerprint: EndpointFingerprint,
    pub effective_capabilities: DriverCapabilities,
}

#[derive(Clone, Debug)]
pub struct ComponentRequest {
    pub release: ComponentRelease,
    pub required_capabilities: BTreeSet<Capability>,
}

/// One immutable core-packaged Release ready for Driver consumption.
///
/// Fields are private so only the core packager can establish the digest,
/// size, path, and Release identity as one consistent value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasePackage {
    release: ComponentRelease,
    path: PathBuf,
    sha256: String,
    size: u64,
}

impl ReleasePackage {
    pub(crate) fn new(release: ComponentRelease, path: PathBuf, sha256: String, size: u64) -> Self {
        Self {
            release,
            path,
            sha256,
            size,
        }
    }

    #[must_use]
    pub fn release(&self) -> &ComponentRelease {
        &self.release
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

#[derive(Clone, Debug)]
pub struct PreflightReport {
    pub effective_capabilities: DriverCapabilities,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ComponentPlan {
    pub release: ComponentRelease,
    pub effective_capabilities: DriverCapabilities,
    pub expected_current: Option<ReleaseRef>,
    pub driver_steps: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedRelease {
    pub release: ReleaseRef,
    pub already_active: bool,
}

#[derive(Clone, Debug)]
pub struct ActivationReceipt {
    pub current: Option<ReleaseRef>,
    pub healthy: bool,
}

#[derive(Clone, Debug)]
pub struct RetentionPolicy {
    pub protected_versions: BTreeSet<ReleaseVersion>,
    pub retain_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CleanupReport {
    pub removed: Vec<ReleaseVersion>,
    pub retained: Vec<ReleaseVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverLog {
    pub namespace: String,
    pub message: String,
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: DriverLog);
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{stage} failed for {target}: {message}; suggested action: {suggested_action}")]
pub struct DriverError {
    pub stage: String,
    pub target: String,
    pub message: String,
    pub suggested_action: String,
}

impl DriverError {
    fn configuration(target: &str, message: &str) -> Self {
        Self {
            stage: "configuration".into(),
            target: target.into(),
            message: message.into(),
            suggested_action: "correct the value in the TUI and retry".into(),
        }
    }
}

#[async_trait]
pub trait DeploymentDriver: fmt::Debug + Send + Sync {
    fn kind(&self) -> DriverKind;
    fn static_capabilities(&self) -> DriverCapabilities;
    /// Validates Driver-specific target input and returns an opaque typed value.
    ///
    /// # Errors
    ///
    /// Returns field-specific configuration errors for unsupported or invalid
    /// target settings.
    fn validate_target(
        &self,
        input: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError>;
    /// Validates Driver-specific connection settings before planning.
    ///
    /// # Errors
    ///
    /// Returns field-specific configuration errors for unsupported or invalid
    /// Destination settings.
    fn validate_destination(
        &self,
        input: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError>;
    async fn preflight(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError>;
    async fn plan(
        &self,
        context: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError>;
    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError>;
    async fn prepare(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        plan: &ComponentPlan,
        package: &ReleasePackage,
        events: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError>;
    async fn activate(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError>;
    async fn rollback(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError>;
    async fn logs(
        &self,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError>;
    async fn cleanup(
        &self,
        context: &ComponentExecutionContext,
        policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError>;
}

#[derive(Debug, Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<DriverKind, Arc<dyn DeploymentDriver>>,
}

impl DriverRegistry {
    /// Registers one compiled-in Driver kind.
    ///
    /// # Errors
    ///
    /// Returns an error if that kind is already registered.
    pub fn register(&mut self, driver: Arc<dyn DeploymentDriver>) -> Result<(), DriverError> {
        let kind = driver.kind();
        if self.drivers.contains_key(&kind) {
            return Err(DriverError::configuration(
                "driver.registry",
                &format!("Driver `{kind}` is already registered"),
            ));
        }
        self.drivers.insert(kind, driver);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, kind: &DriverKind) -> Option<Arc<dyn DeploymentDriver>> {
        self.drivers.get(kind).cloned()
    }
}

#[cfg(test)]
mod tests;
