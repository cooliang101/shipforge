//! Bounded, non-secret remote audit evidence; never a replacement for observation.

use serde::{Deserialize, Serialize};

use crate::domain::{DeploymentId, ReleaseManifest, ReleaseVersion};

use super::ReleaseRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteAuditPhase {
    Prepare,
    Activate,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteAuditOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    content = "version",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum RemoteAuditObserved {
    Unknown,
    NotDeployed,
    Release(ReleaseVersion),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteAuditPackage {
    pub manifest: ReleaseManifest,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteAuditRecord {
    pub schema_version: u32,
    pub event_id: uuid::Uuid,
    pub deployment: DeploymentId,
    pub recorded_at_ms: u64,
    pub release: ReleaseRef,
    pub phase: RemoteAuditPhase,
    pub outcome: RemoteAuditOutcome,
    pub expected_current: Option<ReleaseVersion>,
    pub target: Option<ReleaseVersion>,
    pub observed: RemoteAuditObserved,
    pub healthy: Option<bool>,
    pub package: Option<RemoteAuditPackage>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteAuditHistory {
    pub records: Vec<RemoteAuditRecord>,
    /// Sanitized explanations of absent, invalid, conflicting or omitted evidence.
    pub notices: Vec<String>,
    /// True also for absent logs: absence never proves no deployment occurred.
    pub incomplete: bool,
}

impl RemoteAuditRecord {
    /// Validates the typed evidence without trusting a serialized Driver payload.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.schema_version != 1
            || self.event_id.is_nil()
            || super::DriverKind::parse(self.release.driver.as_str()).is_err()
            || (self.observed == RemoteAuditObserved::Unknown && self.healthy.is_some())
            || (self.outcome == RemoteAuditOutcome::Failed && self.healthy.is_some())
        {
            return false;
        }
        let identity = match self.phase {
            RemoteAuditPhase::Prepare | RemoteAuditPhase::Activate => self.target.as_ref(),
            RemoteAuditPhase::Rollback => self.target.as_ref().or(self.expected_current.as_ref()),
        };
        if identity != Some(&self.release.version) {
            return false;
        }
        if let Some(package) = &self.package {
            let manifest = &package.manifest;
            if self.phase != RemoteAuditPhase::Prepare
                || package.size == 0
                || package.sha256.len() != 64
                || !package
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || manifest.schema_version != 1
                || manifest.project_id != self.release.project_id
                || manifest.environment_id != self.release.environment_id
                || manifest.component != self.release.component
                || manifest.generation != self.release.generation
                || manifest.version != self.release.version
                || manifest.source_revision.as_ref().is_some_and(|revision| {
                    !(7..=64).contains(&revision.len())
                        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return false;
            }
        }
        if self.phase == RemoteAuditPhase::Prepare {
            return self.healthy.is_none()
                && (self.outcome != RemoteAuditOutcome::Succeeded || self.package.is_some());
        }
        self.outcome != RemoteAuditOutcome::Succeeded
            || match (&self.observed, &self.target) {
                (RemoteAuditObserved::NotDeployed, None) => self.healthy.is_none(),
                (RemoteAuditObserved::Release(observed), Some(target)) => {
                    observed == target && self.healthy == Some(true)
                }
                _ => false,
            }
    }
}
