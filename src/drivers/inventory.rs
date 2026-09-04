use crate::domain::{ReleaseManifest, ReleaseVersion};

/// Read-only filesystem evidence; neither listing nor `current` establishes health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInventory {
    pub releases: Vec<InventoryRelease>,
    pub issues: Vec<InventoryIssue>,
    /// A missing link is known absence; failed observation remains explicitly unknown.
    pub current: Result<Option<ReleaseVersion>, String>,
    pub notices: Vec<String>,
}

/// Identity-checked archive metadata, not a claim that its payload is safe to activate.
/// A later rollback/recovery operation must perform its own current-state checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryRelease {
    pub manifest: ReleaseManifest,
    pub sha256: String,
    pub size: u64,
    pub extracted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryIssue {
    /// Only a parsed canonical version is retained, never an arbitrary remote path.
    pub version: Option<ReleaseVersion>,
    pub message: String,
}
