//! Immutable read-only reconciliation evidence, separate from Deployment history.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{
    DeploymentComponentSnapshot, DeploymentId, DeploymentRecord, EnvironmentId, HistoryError,
    HistoryStore, IntentRecord, ObservationRecord, ProjectId, Redactor, ReleasePackageRecord,
    sanitize_diagnostic, timestamp, validate_text,
};
use crate::{
    domain::{
        ComponentGeneration, ComponentName, DestinationKey, DestinationRevision, ReleaseManifest,
    },
    drivers::{ComponentInventory, DriverKind, EndpointFingerprint, ReleaseRef},
};

mod persistence;
pub(super) use persistence::migrate;

const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPONENTS: usize = 256;
const MAX_DIAGNOSTICS: usize = 32;

/// The endpoint inspected now, without fabricating a historical Release identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectionScope {
    pub project: ProjectId,
    pub environment: EnvironmentId,
    pub component: ComponentName,
    pub generation: ComponentGeneration,
    pub driver: DriverKind,
    pub destination: DestinationKey,
    pub destination_revision: DestinationRevision,
    pub endpoint_fingerprint: EndpointFingerprint,
}

impl From<&ReleaseRef> for InspectionScope {
    fn from(release: &ReleaseRef) -> Self {
        Self {
            project: release.project_id.clone(),
            environment: release.environment_id.clone(),
            component: release.component.clone(),
            generation: release.generation,
            driver: release.driver.clone(),
            destination: release.destination.clone(),
            destination_revision: release.destination_revision,
            endpoint_fingerprint: release.endpoint_fingerprint.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CurrentAlignment {
    Unplanned,
    Target,
    Previous,
    Other,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageAlignment {
    Unplanned,
    Matches,
    ArchiveOnly,
    Missing,
    Mismatch,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryComponentReport {
    pub scope: InspectionScope,
    pub inventory: Result<ComponentInventory, String>,
    pub alignment: CurrentAlignment,
    pub package_alignment: PackageAlignment,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryReport {
    pub id: uuid::Uuid,
    pub related_deployment: Option<DeploymentId>,
    pub source_revision: Option<u64>,
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
    pub components: Vec<RecoveryComponentReport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryBasis {
    pub record: DeploymentRecord,
    pub snapshots: Vec<DeploymentComponentSnapshot>,
    pub packages: Vec<ReleasePackageRecord>,
    /// Only pending intents require reconciliation; completed stages stay untouched.
    pub intents: Vec<IntentRecord>,
    pub observations: Vec<ObservationRecord>,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAttentionSummary {
    pub database_missing: bool,
    /// At most 100 newest Created/Running or unresolved-intent Deployments.
    pub candidates: Vec<DeploymentRecord>,
    pub more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryQuery {
    pub limit: u32,
    pub offset: u32,
}

impl Default for RecoveryQuery {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
        }
    }
}

impl RecoveryReport {
    /// Sanitizes every free-form diagnostic, including nested Driver failures.
    pub fn sanitize(&mut self, redactor: &Redactor) {
        for component in &mut self.components {
            component.sanitize(redactor);
        }
    }

    /// Checks bounds and internal evidence consistency, without inferring health.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers, bounds, scope, or conclusions.
    pub fn validate(&self) -> Result<(), HistoryError> {
        if self.id.is_nil()
            || self.related_deployment.is_some() != self.source_revision.is_some()
            || self.components.is_empty()
            || self.components.len() > MAX_COMPONENTS
            || self.completed_at_ms < self.started_at_ms
        {
            return invalid("invalid recovery report identity, size, or timing");
        }
        timestamp(self.started_at_ms)?;
        timestamp(self.completed_at_ms)?;
        if let Some(revision) = self.source_revision {
            timestamp(revision)?;
        }
        let first = &self.components[0].scope;
        let mut names = BTreeSet::new();
        for component in &self.components {
            component.validate()?;
            if component.scope.project != first.project
                || component.scope.environment != first.environment
                || !names.insert(component.scope.component.as_str())
            {
                return invalid("recovery report has duplicate Components or mixed scope");
            }
            if self.related_deployment.is_none() {
                validate_unplanned(component)?;
            }
        }
        if encode(self)?.len() > MAX_REPORT_BYTES {
            return invalid("recovery report exceeds byte limit");
        }
        Ok(())
    }
}

impl RecoveryComponentReport {
    /// Sanitizes diagnostics for display even if a later cache write fails.
    pub fn sanitize(&mut self, redactor: &Redactor) {
        sanitize_messages(&mut self.notices, redactor);
        match &mut self.inventory {
            Err(error) => sanitize_message(error, redactor),
            Ok(inventory) => {
                if let Err(error) = &mut inventory.releases.current {
                    sanitize_message(error, redactor);
                }
                for issue in &mut inventory.releases.issues {
                    sanitize_message(&mut issue.message, redactor);
                }
                sanitize_messages(&mut inventory.releases.notices, redactor);
                sanitize_messages(&mut inventory.audit.notices, redactor);
                sanitize_messages(&mut inventory.remnants.notices, redactor);
            }
        }
    }

    /// Validates bounded, scoped filesystem/audit evidence and safe diagnostics.
    ///
    /// # Errors
    /// Returns an error for invalid or contradictory evidence.
    pub fn validate(&self) -> Result<(), HistoryError> {
        DriverKind::parse(self.scope.driver.as_str())
            .map_err(|_| HistoryError::InvalidMetadata("invalid inspection Driver"))?;
        validate_messages(&self.notices)?;
        match &self.inventory {
            Err(error) => {
                validate_text("inspection error", error)?;
                if !matches!(
                    self.alignment,
                    CurrentAlignment::Unknown | CurrentAlignment::Unplanned
                ) || !matches!(
                    self.package_alignment,
                    PackageAlignment::Unknown | PackageAlignment::Unplanned
                ) {
                    return invalid("unknown inventory cannot establish alignment");
                }
            }
            Ok(inventory) => {
                validate_inventory(&self.scope, inventory)?;
                if inventory.releases.current.is_err()
                    && !matches!(
                        self.alignment,
                        CurrentAlignment::Unknown | CurrentAlignment::Unplanned
                    )
                {
                    return invalid("unknown current cannot establish alignment");
                }
            }
        }
        Ok(())
    }
}

fn validate_inventory(
    scope: &InspectionScope,
    inventory: &ComponentInventory,
) -> Result<(), HistoryError> {
    let releases = &inventory.releases;
    if releases.releases.len() > 1024
        || releases.issues.len() > 1024
        || inventory.audit.records.len() > 128
        || inventory.remnants.entries.len() > 256
    {
        return invalid("inventory exceeds entry limits");
    }
    validate_messages(&releases.notices)?;
    validate_messages(&inventory.audit.notices)?;
    validate_messages(&inventory.remnants.notices)?;
    if let Err(error) = &releases.current {
        validate_text("current observation error", error)?;
    }
    let mut versions = BTreeSet::new();
    for release in &releases.releases {
        validate_manifest(scope, &release.manifest)?;
        validate_digest(&release.sha256, release.size)?;
        if !versions.insert(release.manifest.version.as_str()) {
            return invalid("duplicate inventory Release version");
        }
    }
    for issue in &releases.issues {
        validate_text("inventory issue", &issue.message)?;
    }
    let mut events = BTreeSet::new();
    for record in &inventory.audit.records {
        let reference = &record.release;
        // Old endpoint/revision are evidence, never replaced by the inspected endpoint.
        if !record.is_valid()
            || !events.insert(record.event_id)
            || reference.project_id != scope.project
            || reference.environment_id != scope.environment
            || reference.component != scope.component
            || reference.generation != scope.generation
            || reference.driver != scope.driver
        {
            return invalid("invalid, duplicate, or out-of-scope remote audit evidence");
        }
        timestamp(record.recorded_at_ms)?;
        if let Some(package) = &record.package {
            validate_manifest(scope, &package.manifest)?;
            validate_digest(&package.sha256, package.size)?;
        }
    }
    if inventory
        .remnants
        .entries
        .iter()
        .any(|entry| !entry.is_valid())
    {
        return invalid("invalid temporary remnant evidence");
    }
    Ok(())
}

fn validate_manifest(
    scope: &InspectionScope,
    manifest: &ReleaseManifest,
) -> Result<(), HistoryError> {
    if manifest.schema_version != 1
        || manifest.project_id != scope.project
        || manifest.environment_id != scope.environment
        || manifest.component != scope.component
        || manifest.generation != scope.generation
        || manifest.source_revision.as_ref().is_some_and(|revision| {
            !(7..=64).contains(&revision.len())
                || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return invalid("invalid or out-of-scope inventory manifest");
    }
    timestamp(manifest.created_at_unix)?;
    Ok(())
}

fn validate_digest(digest: &str, size: u64) -> Result<(), HistoryError> {
    if digest.len() != 64
        || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        || size == 0
        || size > i64::MAX as u64
    {
        return invalid("invalid inventory package digest or size");
    }
    Ok(())
}

fn validate_unplanned(component: &RecoveryComponentReport) -> Result<(), HistoryError> {
    if matches!(
        component.alignment,
        CurrentAlignment::Unplanned | CurrentAlignment::Unknown
    ) && matches!(
        component.package_alignment,
        PackageAlignment::Unplanned | PackageAlignment::Unknown
    ) {
        Ok(())
    } else {
        invalid("unplanned inspection cannot claim agreement with a local plan")
    }
}

fn validate_messages(messages: &[String]) -> Result<(), HistoryError> {
    if messages.len() > MAX_DIAGNOSTICS {
        return invalid("too many recovery diagnostics");
    }
    for message in messages {
        validate_text("recovery diagnostic", message)?;
    }
    Ok(())
}

fn sanitize_messages(messages: &mut [String], redactor: &Redactor) {
    for message in messages {
        sanitize_message(message, redactor);
    }
}

fn sanitize_message(message: &mut String, redactor: &Redactor) {
    *message = sanitize_diagnostic(&redactor.redact(message));
}

fn invalid<T>(message: &'static str) -> Result<T, HistoryError> {
    Err(HistoryError::InvalidMetadata(message))
}

fn encode(value: &impl Serialize) -> Result<String, HistoryError> {
    serde_json::to_string(value)
        .map_err(|_| HistoryError::InvalidMetadata("recovery report serialization failed"))
}

#[cfg(test)]
mod tests;
