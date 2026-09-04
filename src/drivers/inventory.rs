use serde::{Deserialize, Serialize};

use crate::domain::{DeploymentId, ReleaseManifest, ReleaseVersion};

/// Read-only filesystem evidence; neither listing nor `current` establishes health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseInventory {
    pub releases: Vec<InventoryRelease>,
    pub issues: Vec<InventoryIssue>,
    /// A missing link is known absence; failed observation remains explicitly unknown.
    pub current: Result<Option<ReleaseVersion>, String>,
    pub notices: Vec<String>,
}

/// Identity-checked archive metadata, not a claim that its payload is safe to activate.
/// A later rollback/recovery operation must perform its own current-state checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventoryRelease {
    pub manifest: ReleaseManifest,
    pub sha256: String,
    pub size: u64,
    pub extracted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventoryIssue {
    /// Only a parsed canonical version is retained, never an arbitrary remote path.
    pub version: Option<ReleaseVersion>,
    pub message: String,
}

/// Read-only diagnostic evidence, never proof of ownership or permission to clean up.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemporaryRemnants {
    pub entries: Vec<TemporaryRemnant>,
    pub notices: Vec<String>,
    /// False means a complete scan, including a confirmed empty result.
    pub incomplete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemporaryRemnant {
    pub kind: TemporaryRemnantKind,
    /// Derived only from a canonical filename; marker UUIDs are not Deployment IDs.
    pub deployment: Option<DeploymentId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemporaryRemnantKind {
    UploadArchive,
    ExtractedDirectory,
    ActivationLink,
    RollbackLink,
    MarkerPublication,
}

impl TemporaryRemnant {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        (self.kind == TemporaryRemnantKind::MarkerPublication) == self.deployment.is_none()
    }
}
