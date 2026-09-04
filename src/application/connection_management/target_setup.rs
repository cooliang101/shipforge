//! Read-only remote setup using an exact, freshly resolved saved connection.

use std::{future::Future, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::application::{DestinationSetupError, RemoteDirectoryCandidates, RemoteSetupCandidates};

use super::{
    ConnectionDetails, ConnectionManagementError, ConnectionManagementService,
    DestinationSetupRequest, FileSnapshot, ManagementSource, SETUP_TIMEOUT, SetupCredential,
    check_cancelled, details, resolve_credential,
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_GRACE: Duration = Duration::from_secs(11);

struct SavedTargetQuery {
    destinations: FileSnapshot,
    credentials: FileSnapshot,
    request: DestinationSetupRequest,
}

impl SavedTargetQuery {
    fn ensure_unchanged(&self) -> Result<(), ConnectionManagementError> {
        self.destinations.ensure_unchanged()?;
        self.credentials.ensure_unchanged()
    }
}

impl ConnectionManagementService {
    /// Inspects only the selected root and optional service candidates using the
    /// exact saved connection revision. Never creates paths or saves registrations.
    ///
    /// # Errors
    /// Rejects missing/stale connections, changed local evidence, unsafe roots,
    /// cancellation, busy sessions, timeout or failed pinned authentication/probing.
    pub async fn inspect_saved_target(
        &self,
        selected: &ConnectionDetails,
        root: &str,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, ConnectionManagementError> {
        self.session
            .run(async {
                let query = self.saved_target_query(selected, root, cancellation)?;
                let worker_cancel = cancellation.child_token();
                let result = wait_read(
                    self.setup.authenticate_and_probe(
                        &query.request,
                        SETUP_TIMEOUT,
                        SETUP_TIMEOUT,
                        &worker_cancel,
                    ),
                    &worker_cancel,
                    cancellation,
                    "saved target inspection",
                    QUERY_TIMEOUT,
                    CANCEL_GRACE,
                )
                .await;
                check_cancelled(cancellation)?;
                query.ensure_unchanged()?;
                result
            })
            .await
            .map_err(|_| ConnectionManagementError::Busy)?
    }

    /// Lists direct children of one explicitly chosen physical remote directory.
    /// Enumeration does not establish write access, deployment permission or health.
    ///
    /// # Errors
    /// Rejects missing/stale connections, changed local evidence, unsafe roots,
    /// incomplete/unsupported browsing, cancellation, busy sessions and timeout.
    pub async fn browse_saved_directories(
        &self,
        selected: &ConnectionDetails,
        root: &str,
        cancellation: &CancellationToken,
    ) -> Result<RemoteDirectoryCandidates, ConnectionManagementError> {
        self.session
            .run(async {
                let query = self.saved_target_query(selected, root, cancellation)?;
                let worker_cancel = cancellation.child_token();
                let result = wait_read(
                    self.setup.browse_directories(
                        &query.request,
                        SETUP_TIMEOUT,
                        SETUP_TIMEOUT,
                        &worker_cancel,
                    ),
                    &worker_cancel,
                    cancellation,
                    "saved directory browsing",
                    QUERY_TIMEOUT,
                    CANCEL_GRACE,
                )
                .await;
                check_cancelled(cancellation)?;
                query.ensure_unchanged()?;
                let candidates = result?;
                let mut unique = std::collections::BTreeSet::new();
                if candidates.directory != root
                    || candidates.directories.len() > 512
                    || candidates.directories.iter().any(|path| {
                        !safe_root(path)
                            || !unique.insert(path)
                            || !path.rsplit_once('/').is_some_and(|(parent, name)| {
                                !name.is_empty()
                                    && name.len() <= 255
                                    && parent == if root == "/" { "" } else { root }
                            })
                    })
                    || candidates
                        .directories
                        .iter()
                        .map(String::len)
                        .sum::<usize>()
                        > 64 * 1024
                {
                    return Err(ConnectionManagementError::Setup(
                        "directory evidence validation",
                    ));
                }
                Ok(candidates)
            })
            .await
            .map_err(|_| ConnectionManagementError::Busy)?
    }

    fn saved_target_query(
        &self,
        selected: &ConnectionDetails,
        root: &str,
        cancellation: &CancellationToken,
    ) -> Result<SavedTargetQuery, ConnectionManagementError> {
        check_cancelled(cancellation)?;
        if !safe_root(root) {
            return Err(ConnectionManagementError::Setup(
                "absolute directory validation",
            ));
        }
        let (destinations, registry) = self.destinations()?;
        let current = details(&registry, &selected.key)?;
        if &current != selected {
            return Err(ConnectionManagementError::Stale);
        }
        let connection = current.current.resolve();
        let credentials =
            FileSnapshot::read(&self.paths.credentials, ManagementSource::Credentials)?;
        let credential = resolve_credential(&credentials, &connection.credential)?;
        let query = SavedTargetQuery {
            destinations,
            credentials,
            request: DestinationSetupRequest {
                driver: connection.driver,
                destination: connection.settings,
                credential: SetupCredential::new(credential),
                remote_root: root.to_owned(),
            },
        };
        query.ensure_unchanged()?;
        check_cancelled(cancellation)?;
        Ok(query)
    }
}

fn safe_root(path: &str) -> bool {
    path.starts_with('/') && path.len() <= 4096
        && !path.chars().any(|character| character.is_control()
            || matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
        && (path == "/" || path.split('/').skip(1).all(|part| !matches!(part, "" | "." | "..")))
}

async fn wait_read<T>(
    operation: impl Future<Output = Result<T, DestinationSetupError>>,
    worker_cancel: &CancellationToken,
    cancellation: &CancellationToken,
    stage: &'static str,
    deadline: Duration,
    grace: Duration,
) -> Result<T, ConnectionManagementError> {
    tokio::pin!(operation);
    let interrupted = tokio::select! {
        biased;
        () = cancellation.cancelled() => ConnectionManagementError::Cancelled,
        result = tokio::time::timeout(deadline, &mut operation) => match result {
            Ok(result) => return result.map_err(|_| ConnectionManagementError::Setup(stage)),
            Err(_) => ConnectionManagementError::Setup("saved target query deadline"),
        },
    };
    // Do not immediately drop an in-flight gateway: allow its bounded disconnect
    // path to finish, while an uncooperative implementation still has a deadline.
    worker_cancel.cancel();
    let _ = tokio::time::timeout(grace, &mut operation).await;
    Err(interrupted)
}

#[cfg(test)]
mod tests;
