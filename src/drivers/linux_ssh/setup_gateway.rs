use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{
    application::{
        DestinationSetupError, DestinationSetupGateway, DestinationSetupRequest,
        EndpointProbeRequest, LocalIdentityCandidate, RemoteSetupCandidates, SetupRootState,
    },
    config::SshCredential,
    drivers::DriverKind,
};

use super::{
    LinuxSshDestination, RemoteRootState, capture_host_key, connect_authenticated,
    probe_agent_identities, probe_remote_setup,
};

#[derive(Debug, Default)]
pub struct LinuxSshSetupGateway;

#[async_trait]
impl DestinationSetupGateway for LinuxSshSetupGateway {
    fn driver_kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }

    async fn discover_local_identities(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError> {
        probe_agent_identities(cancellation)
            .await
            .map(|identities| {
                identities
                    .into_iter()
                    .filter(|identity| identity.supported && !identity.certificate)
                    .map(|identity| LocalIdentityCandidate {
                        reference: identity.fingerprint,
                        label: identity.comment,
                    })
                    .collect()
            })
            .map_err(|error| {
                DestinationSetupError::operation("identity discovery", error.to_string())
            })
    }

    async fn capture_endpoint_identity(
        &self,
        request: &EndpointProbeRequest,
        timeout: std::time::Duration,
        cancellation: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        let host = request
            .destination
            .value
            .get("host")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DestinationSetupError::operation("Host Key probe", "host is missing"))?;
        let port = request
            .destination
            .value
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| DestinationSetupError::operation("Host Key probe", "port is invalid"))?;
        capture_host_key(host, port, timeout, cancellation)
            .await
            .map(|fingerprint| fingerprint.as_str().to_owned())
            .map_err(|error| DestinationSetupError::operation("Host Key probe", error.to_string()))
    }

    async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        connect_timeout: std::time::Duration,
        command_timeout: std::time::Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        let destination = LinuxSshDestination::validate(&request.destination).map_err(|error| {
            DestinationSetupError::operation("destination validation", error.to_string())
        })?;
        let credential = request
            .credential
            .downcast_ref::<SshCredential>()
            .ok_or_else(|| {
                DestinationSetupError::operation(
                    "authentication",
                    "credential type does not match linux-ssh",
                )
            })?;
        let session =
            connect_authenticated(&destination, credential, connect_timeout, cancellation)
                .await
                .map_err(|error| {
                    DestinationSetupError::operation("authentication", error.to_string())
                })?;
        let probe = probe_remote_setup(
            &session,
            &request.remote_root,
            command_timeout,
            cancellation,
        )
        .await
        .map(|candidates| RemoteSetupCandidates {
            root: map_root_state(candidates.root),
            services: candidates.systemd_units,
            notices: candidates.notices,
        })
        .map_err(|error| DestinationSetupError::operation("remote discovery", error.to_string()));
        let disconnect = tokio::time::timeout(command_timeout, session.disconnect())
            .await
            .map_err(|_| {
                DestinationSetupError::operation(
                    "disconnect",
                    format!("timed out after {command_timeout:?}"),
                )
            })?
            .map_err(|error| DestinationSetupError::operation("disconnect", error.to_string()));
        match (probe, disconnect) {
            (Ok(candidates), Ok(())) => Ok(candidates),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

const fn map_root_state(state: RemoteRootState) -> SetupRootState {
    match state {
        RemoteRootState::Missing => SetupRootState::Missing,
        RemoteRootState::WritableDirectory => SetupRootState::WritableDirectory,
        RemoteRootState::ReadOnlyDirectory => SetupRootState::ReadOnlyDirectory,
        RemoteRootState::NotDirectory => SetupRootState::NotDirectory,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        application::{DestinationSetupRequest, SetupCredential},
        drivers::DriverDestinationInput,
    };

    fn destination() -> DriverDestinationInput {
        DriverDestinationInput {
            value: serde_json::json!({
                "host": "example.invalid",
                "port": 22,
                "user": "deploy",
                "hostKey": "SHA256:test"
            }),
        }
    }

    #[tokio::test]
    async fn rejects_wrong_credential_type_before_network_access() {
        let gateway = LinuxSshSetupGateway;
        let error = gateway
            .authenticate_and_probe(
                &DestinationSetupRequest {
                    driver: DriverKind::linux_ssh(),
                    destination: destination(),
                    credential: SetupCredential::new(42_u8),
                    remote_root: "/srv/shipforge/test".into(),
                },
                Duration::from_secs(1),
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("credential type"));
    }

    #[tokio::test]
    async fn early_cancellation_does_not_open_network_or_read_identity_file() {
        let gateway = LinuxSshSetupGateway;
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = gateway
            .authenticate_and_probe(
                &DestinationSetupRequest {
                    driver: DriverKind::linux_ssh(),
                    destination: destination(),
                    credential: SetupCredential::new(SshCredential::IdentityFile {
                        path: std::path::PathBuf::from("does-not-exist"),
                    }),
                    remote_root: "/srv/shipforge/test".into(),
                },
                Duration::from_secs(1),
                Duration::from_secs(1),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn invalid_endpoint_probe_is_rejected_without_network_access() {
        let gateway = LinuxSshSetupGateway;
        let error = gateway
            .capture_endpoint_identity(
                &EndpointProbeRequest {
                    driver: DriverKind::linux_ssh(),
                    destination: DriverDestinationInput {
                        value: serde_json::json!({ "host": "example.invalid", "port": 0 }),
                    },
                },
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("port is invalid"));
    }
}
