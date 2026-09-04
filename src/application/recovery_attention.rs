//! Local-only startup detection. This is neither a reconciliation nor a state transition.

use std::path::Path;

use crate::{
    domain::{EnvironmentId, ProjectId},
    history::{HistoryError, HistoryStore},
};

pub use crate::history::LocalAttentionSummary;

/// Queries bounded local records that need verification, without creating history.
///
/// Missing history remains unknown. No Driver, SSH connection, configuration
/// recovery, or Deployment state change is performed by this query.
///
/// # Errors
/// Returns an error when existing history cannot safely be read or understood.
pub fn local_attention(
    history_path: &Path,
    project: Option<&ProjectId>,
    environment: Option<&EnvironmentId>,
) -> Result<LocalAttentionSummary, HistoryError> {
    HistoryStore::local_attention(history_path, project, environment)
}
