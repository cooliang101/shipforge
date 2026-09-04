use std::{any::Any, fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::drivers::{DriverDestinationInput, DriverKind};

#[derive(Clone)]
pub struct SetupCredential(Arc<dyn Any + Send + Sync>);

impl SetupCredential {
    #[must_use]
    pub fn new(value: impl Any + Send + Sync) -> Self {
        Self(Arc::new(value))
    }

    #[must_use]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

impl fmt::Debug for SetupCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SetupCredential([REDACTED])")
    }
}

#[derive(Clone, Debug)]
pub struct DestinationSetupRequest {
    pub driver: DriverKind,
    pub destination: DriverDestinationInput,
    pub credential: SetupCredential,
    pub remote_root: String,
}

#[derive(Clone, Debug)]
pub struct EndpointProbeRequest {
    pub driver: DriverKind,
    pub destination: DriverDestinationInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIdentityCandidate {
    pub reference: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupRootState {
    Missing,
    WritableDirectory,
    ReadOnlyDirectory,
    NotDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSetupCandidates {
    pub root: SetupRootState,
    pub services: Vec<String>,
    pub notices: Vec<String>,
}

/// One explicitly requested directory and its observed direct child directories.
/// Enumeration is not proof of write access, deployment ownership or service health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDirectoryCandidates {
    pub directory: String,
    pub directories: Vec<String>,
}

#[async_trait]
pub trait DestinationSetupGateway: fmt::Debug + Send + Sync {
    fn driver_kind(&self) -> DriverKind;

    async fn discover_local_identities(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError>;

    async fn capture_endpoint_identity(
        &self,
        request: &EndpointProbeRequest,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<String, DestinationSetupError>;

    async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        connect_timeout: Duration,
        command_timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError>;

    async fn browse_directories(
        &self,
        _request: &DestinationSetupRequest,
        _connect_timeout: Duration,
        _command_timeout: Duration,
        _cancellation: &CancellationToken,
    ) -> Result<RemoteDirectoryCandidates, DestinationSetupError> {
        Err(DestinationSetupError::operation(
            "directory browsing",
            "directory browsing is unavailable for this connection",
        ))
    }
}

#[derive(Clone, Debug)]
pub struct DestinationSetupService {
    gateway: Arc<dyn DestinationSetupGateway>,
}

impl DestinationSetupService {
    #[must_use]
    pub fn new(gateway: Arc<dyn DestinationSetupGateway>) -> Self {
        Self { gateway }
    }

    /// Discovers locally available identities through the configured Driver.
    ///
    /// # Errors
    ///
    /// Returns a Driver-owned discovery or cancellation error.
    pub async fn discover_local_identities(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError> {
        self.gateway.discover_local_identities(cancellation).await
    }

    /// Captures the endpoint identity without authenticating.
    ///
    /// # Errors
    ///
    /// Returns an error for a Driver mismatch, invalid endpoint, timeout,
    /// cancellation, or transport failure.
    pub async fn capture_endpoint_identity(
        &self,
        request: &EndpointProbeRequest,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        self.ensure_driver(&request.driver)?;
        self.gateway
            .capture_endpoint_identity(request, timeout, cancellation)
            .await
    }

    /// Authenticates and returns read-only remote setup candidates.
    ///
    /// # Errors
    ///
    /// Returns an error for a Driver mismatch, authentication failure,
    /// timeout, cancellation, probe failure, or disconnect failure.
    pub async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        connect_timeout: Duration,
        command_timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        self.ensure_driver(&request.driver)?;
        self.gateway
            .authenticate_and_probe(request, connect_timeout, command_timeout, cancellation)
            .await
    }

    /// Lists only the direct directories under the explicitly requested remote path.
    /// The gateway authenticates the saved host-key pin; no paths are created.
    ///
    /// # Errors
    /// Returns an error for unsupported browsing, invalid/noncanonical paths,
    /// unavailable or incomplete directory evidence, timeout or cancellation.
    pub async fn browse_directories(
        &self,
        request: &DestinationSetupRequest,
        connect_timeout: Duration,
        command_timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteDirectoryCandidates, DestinationSetupError> {
        self.ensure_driver(&request.driver)?;
        if cancellation.is_cancelled() {
            return Err(DestinationSetupError::operation(
                "directory browsing",
                "cancelled",
            ));
        }
        self.gateway
            .browse_directories(request, connect_timeout, command_timeout, cancellation)
            .await
    }

    fn ensure_driver(&self, requested: &DriverKind) -> Result<(), DestinationSetupError> {
        let available = self.gateway.driver_kind();
        if &available == requested {
            Ok(())
        } else {
            Err(DestinationSetupError::UnsupportedDriver {
                requested: requested.clone(),
                available,
            })
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DestinationSetupError {
    #[error("setup Driver `{requested}` is unavailable; active Driver is `{available}`")]
    UnsupportedDriver {
        requested: DriverKind,
        available: DriverKind,
    },
    #[error("{stage} failed: {message}")]
    Operation {
        stage: &'static str,
        message: String,
    },
}

impl DestinationSetupError {
    #[must_use]
    pub fn operation(stage: &'static str, message: impl Into<String>) -> Self {
        Self::Operation {
            stage,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeSetupGateway;

    #[async_trait]
    impl DestinationSetupGateway for FakeSetupGateway {
        fn driver_kind(&self) -> DriverKind {
            DriverKind::parse("fake").unwrap()
        }

        async fn discover_local_identities(
            &self,
            cancellation: &CancellationToken,
        ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError> {
            if cancellation.is_cancelled() {
                return Err(DestinationSetupError::operation("discovery", "cancelled"));
            }
            Ok(vec![LocalIdentityCandidate {
                reference: "identity-1".into(),
                label: "test identity".into(),
            }])
        }

        async fn capture_endpoint_identity(
            &self,
            _request: &EndpointProbeRequest,
            _timeout: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<String, DestinationSetupError> {
            Ok("endpoint-fingerprint".into())
        }

        async fn authenticate_and_probe(
            &self,
            request: &DestinationSetupRequest,
            _connect_timeout: Duration,
            _command_timeout: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
            assert_eq!(request.remote_root, "/srv/example");
            assert_eq!(request.credential.downcast_ref::<u8>(), Some(&7));
            Ok(RemoteSetupCandidates {
                root: SetupRootState::WritableDirectory,
                services: vec!["example.service".into()],
                notices: Vec::new(),
            })
        }
    }

    fn endpoint(driver: DriverKind) -> EndpointProbeRequest {
        EndpointProbeRequest {
            driver,
            destination: DriverDestinationInput {
                value: serde_json::json!({ "host": "example.test", "port": 22 }),
            },
        }
    }

    #[tokio::test]
    async fn service_delegates_without_exposing_transport_types() {
        let service = DestinationSetupService::new(Arc::new(FakeSetupGateway));
        let cancellation = CancellationToken::new();
        let identities = service
            .discover_local_identities(&cancellation)
            .await
            .unwrap();
        assert_eq!(identities[0].reference, "identity-1");
        let fingerprint = service
            .capture_endpoint_identity(
                &endpoint(DriverKind::parse("fake").unwrap()),
                Duration::from_secs(1),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(fingerprint, "endpoint-fingerprint");
        let candidates = service
            .authenticate_and_probe(
                &DestinationSetupRequest {
                    driver: DriverKind::parse("fake").unwrap(),
                    destination: endpoint(DriverKind::parse("fake").unwrap()).destination,
                    credential: SetupCredential::new(7_u8),
                    remote_root: "/srv/example".into(),
                },
                Duration::from_secs(1),
                Duration::from_secs(1),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(candidates.services, vec!["example.service"]);
    }

    #[tokio::test]
    async fn service_rejects_driver_mismatch_before_calling_gateway() {
        let service = DestinationSetupService::new(Arc::new(FakeSetupGateway));
        let error = service
            .capture_endpoint_identity(
                &endpoint(DriverKind::parse("other").unwrap()),
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DestinationSetupError::UnsupportedDriver { .. }
        ));
    }

    #[test]
    fn setup_credential_debug_is_always_redacted() {
        let credential = SetupCredential::new("private-path-or-token".to_owned());
        assert_eq!(format!("{credential:?}"), "SetupCredential([REDACTED])");
        let request = DestinationSetupRequest {
            driver: DriverKind::parse("fake").unwrap(),
            destination: endpoint(DriverKind::parse("fake").unwrap()).destination,
            credential,
            remote_root: "/srv/example".into(),
        };
        assert!(!format!("{request:?}").contains("private-path-or-token"));
    }

    #[tokio::test]
    async fn directory_service_checks_driver_cancellation_and_explicit_unsupported_default() {
        let service = DestinationSetupService::new(Arc::new(FakeSetupGateway));
        let mut request = DestinationSetupRequest {
            driver: DriverKind::parse("other").unwrap(),
            destination: endpoint(DriverKind::parse("fake").unwrap()).destination,
            credential: SetupCredential::new(7_u8),
            remote_root: "/srv".into(),
        };
        let cancellation = CancellationToken::new();
        assert!(matches!(
            service
                .browse_directories(
                    &request,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    &cancellation
                )
                .await,
            Err(DestinationSetupError::UnsupportedDriver { .. })
        ));
        request.driver = DriverKind::parse("fake").unwrap();
        let unsupported = service
            .browse_directories(
                &request,
                Duration::from_secs(1),
                Duration::from_secs(1),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(
            unsupported
                .to_string()
                .contains("directory browsing is unavailable")
        );
        cancellation.cancel();
        let cancelled = service
            .browse_directories(
                &request,
                Duration::from_secs(1),
                Duration::from_secs(1),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(cancelled.to_string().contains("cancelled"));
    }
}
