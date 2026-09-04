use std::{fmt, sync::Arc, time::Duration};

use russh::{
    ChannelMsg, Disconnect, client,
    keys::{PrivateKeyWithHashAlg, ssh_key::Algorithm},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{config::SshCredential, telemetry::CommandSpec};

use super::{HostKeyVerifier, LinuxSshDestination, probe::connect_platform_agent};

// Both fingerprint capture and authenticated sessions issue latency-sensitive
// SSH exchanges. Keep all protocol/security defaults and only disable Nagle.
pub(super) fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        nodelay: true,
        ..client::Config::default()
    })
}

pub struct AuthenticatedSession {
    pub(super) handle: client::Handle<HostKeyVerifier>,
}

impl fmt::Debug for AuthenticatedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedSession")
            .finish_non_exhaustive()
    }
}

impl AuthenticatedSession {
    /// Executes a structured command on the authenticated endpoint.
    ///
    /// The command is rendered by quoting its program and every argument for
    /// the POSIX Shell used by the SSH exec protocol. Retained output is
    /// bounded, while the channel is always drained to completion.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid commands, cancellation, timeout, SSH
    /// protocol failure, an exit signal, or a missing exit status.
    pub async fn execute(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        if cancellation.is_cancelled() {
            return Err(SshConnectionError::Cancelled);
        }
        let rendered = command
            .render_posix()
            .map_err(|error| SshConnectionError::Command(error.to_string()))?;
        let operation = execute_command(&self.handle, &rendered);
        tokio::select! {
            () = cancellation.cancelled() => Err(SshConnectionError::Cancelled),
            result = tokio::time::timeout(timeout, operation) => {
                result.map_err(|_| SshConnectionError::Timeout { timeout })?
            }
        }
    }

    /// Closes the SSH connection without opening a remote channel.
    ///
    /// # Errors
    ///
    /// Returns an error if the disconnect message cannot be sent.
    pub async fn disconnect(self) -> Result<(), SshConnectionError> {
        self.handle
            .disconnect(Disconnect::ByApplication, "", "English")
            .await
            .map_err(|error| SshConnectionError::Protocol(error.to_string()))
    }
}

const MAX_RETAINED_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteCommandOutput {
    pub exit_status: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

async fn execute_command(
    handle: &client::Handle<HostKeyVerifier>,
    command: &str,
) -> Result<RemoteCommandOutput, SshConnectionError> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|error| SshConnectionError::Protocol(error.to_string()))?;
    channel
        .exec(true, command)
        .await
        .map_err(|error| SshConnectionError::Protocol(error.to_string()))?;

    let mut exit_status = None;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                append_bounded(&mut stdout, &data, &mut stdout_truncated);
            }
            ChannelMsg::ExtendedData { data, .. } => {
                append_bounded(&mut stderr, &data, &mut stderr_truncated);
            }
            ChannelMsg::ExitStatus {
                exit_status: status,
            } => exit_status = Some(status),
            ChannelMsg::ExitSignal {
                signal_name,
                error_message,
                ..
            } => {
                return Err(SshConnectionError::ExitSignal {
                    signal: format!("{signal_name:?}"),
                    message: error_message,
                });
            }
            _ => {}
        }
    }
    let exit_status = exit_status.ok_or(SshConnectionError::MissingExitStatus)?;
    Ok(RemoteCommandOutput {
        exit_status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn append_bounded(target: &mut Vec<u8>, source: &[u8], truncated: &mut bool) {
    let remaining = MAX_RETAINED_OUTPUT_BYTES.saturating_sub(target.len());
    let retained = source.len().min(remaining);
    target.extend_from_slice(&source[..retained]);
    *truncated |= retained < source.len();
}

/// Connects and authenticates against a previously confirmed Host Key.
///
/// No command or subsystem channel is opened. `IdentityFile` authentication
/// supports unencrypted modern keys; encrypted key passphrases are outside the
/// password-free MVP and should be loaded through SSH Agent instead.
///
/// # Errors
///
/// Returns an error for cancellation, timeout, Host Key mismatch, missing or
/// unsupported credentials, protocol failure, or rejected authentication.
pub async fn connect_authenticated(
    destination: &LinuxSshDestination,
    credential: &SshCredential,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<AuthenticatedSession, SshConnectionError> {
    if cancellation.is_cancelled() {
        return Err(SshConnectionError::Cancelled);
    }
    let operation = connect_and_authenticate(destination, credential);
    tokio::select! {
        () = cancellation.cancelled() => Err(SshConnectionError::Cancelled),
        result = tokio::time::timeout(timeout, operation) => {
            result.map_err(|_| SshConnectionError::Timeout { timeout })?
        }
    }
}

async fn connect_and_authenticate(
    destination: &LinuxSshDestination,
    credential: &SshCredential,
) -> Result<AuthenticatedSession, SshConnectionError> {
    let mut handle = client::connect(
        client_config(),
        (destination.host.as_str(), destination.port),
        HostKeyVerifier::strict(destination.host_key.clone()),
    )
    .await
    .map_err(|error| SshConnectionError::Protocol(error.to_string()))?;

    let authenticated = match credential {
        SshCredential::IdentityFile { path } => {
            let path = path.clone();
            let key = tokio::task::spawn_blocking(move || russh::keys::load_secret_key(path, None))
                .await
                .map_err(|error| SshConnectionError::Key(error.to_string()))?
                .map_err(|error| SshConnectionError::Key(error.to_string()))?;
            ensure_supported_algorithm(&key.algorithm())?;
            handle
                .authenticate_publickey(
                    destination.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), None),
                )
                .await
                .map_err(|error| SshConnectionError::Protocol(error.to_string()))?
                .success()
        }
        SshCredential::Agent { fingerprint } => {
            let mut agent = connect_platform_agent()
                .await
                .map_err(|error| SshConnectionError::Agent(error.to_string()))?;
            let identities = agent
                .request_identities()
                .await
                .map_err(|error| SshConnectionError::Agent(error.to_string()))?;
            let identity = identities
                .into_iter()
                .find(|identity| {
                    identity
                        .public_key()
                        .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
                        .to_string()
                        == *fingerprint
                })
                .ok_or_else(|| SshConnectionError::MissingAgentIdentity(fingerprint.clone()))?;
            if matches!(
                identity,
                russh::keys::agent::AgentIdentity::Certificate { .. }
            ) {
                return Err(SshConnectionError::UnsupportedCertificate);
            }
            let public_key = identity.public_key().into_owned();
            ensure_supported_algorithm(&public_key.algorithm())?;
            handle
                .authenticate_publickey_with(destination.user.clone(), public_key, None, &mut agent)
                .await
                .map_err(|error| SshConnectionError::Agent(error.to_string()))?
                .success()
        }
    };
    if !authenticated {
        return Err(SshConnectionError::Rejected);
    }
    Ok(AuthenticatedSession { handle })
}

fn ensure_supported_algorithm(algorithm: &Algorithm) -> Result<(), SshConnectionError> {
    if supported_algorithm(algorithm) {
        Ok(())
    } else {
        Err(SshConnectionError::UnsupportedKeyAlgorithm(
            algorithm.to_string(),
        ))
    }
}

pub(super) fn supported_algorithm(algorithm: &Algorithm) -> bool {
    matches!(
        algorithm,
        Algorithm::Ed25519
            | Algorithm::Ecdsa { .. }
            | Algorithm::SkEd25519
            | Algorithm::SkEcdsaSha2NistP256
    )
}

#[derive(Debug, Error)]
pub enum SshConnectionError {
    #[error("SSH connection was cancelled")]
    Cancelled,
    #[error("SSH connection timed out after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error("SSH protocol or Host Key verification failed: {0}")]
    Protocol(String),
    #[error("SSH private key could not be loaded; use an unencrypted modern key or SSH Agent: {0}")]
    Key(String),
    #[error("SSH Agent operation failed: {0}")]
    Agent(String),
    #[error("the selected SSH Agent identity is no longer available: {0}")]
    MissingAgentIdentity(String),
    #[error("SSH Agent certificates are not supported by the MVP")]
    UnsupportedCertificate,
    #[error("SSH key algorithm `{0}` is not supported; use Ed25519 or ECDSA")]
    UnsupportedKeyAlgorithm(String),
    #[error("SSH server rejected the selected identity")]
    Rejected,
    #[error("remote command is invalid: {0}")]
    Command(String),
    #[error("remote command exited after signal {signal}: {message}")]
    ExitSignal { signal: String, message: String },
    #[error("remote command channel closed without an exit status")]
    MissingExitStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_client_config_changes_only_tcp_no_delay_from_protocol_defaults() {
        let defaults = client::Config::default();
        let mut configured = Arc::try_unwrap(client_config()).unwrap();
        assert!(configured.nodelay);
        configured.nodelay = defaults.nodelay;
        // Config has no PartialEq; its derived Debug includes every public field,
        // including negotiation, timeouts, limits, buffers and anonymous mode.
        assert_eq!(format!("{configured:?}"), format!("{defaults:?}"));
    }

    #[test]
    fn mvp_accepts_only_modern_non_rsa_algorithms() {
        assert!(ensure_supported_algorithm(&Algorithm::Ed25519).is_ok());
        assert!(ensure_supported_algorithm(&Algorithm::SkEd25519).is_ok());
        assert!(ensure_supported_algorithm(&Algorithm::Rsa { hash: None }).is_err());
        assert!(ensure_supported_algorithm(&Algorithm::Dsa).is_err());
    }

    #[test]
    fn bounded_output_retains_prefix_and_marks_truncation() {
        let mut output = vec![b'a'; MAX_RETAINED_OUTPUT_BYTES - 2];
        let mut truncated = false;
        append_bounded(&mut output, b"bcdef", &mut truncated);
        assert_eq!(output.len(), MAX_RETAINED_OUTPUT_BYTES);
        assert!(output.ends_with(b"bc"));
        assert!(truncated);
    }

    #[tokio::test]
    async fn cancelled_connection_does_not_touch_the_network_or_key_file() {
        let destination = LinuxSshDestination {
            driver: crate::drivers::DriverKind::linux_ssh(),
            host: "example.invalid".into(),
            port: 22,
            user: "deploy".into(),
            host_key: crate::config::HostKeyFingerprint::parse("SHA256:test").unwrap(),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = connect_authenticated(
            &destination,
            &SshCredential::IdentityFile {
                path: std::path::PathBuf::from("does-not-exist"),
            },
            Duration::from_secs(1),
            &cancellation,
        )
        .await;
        assert!(matches!(result, Err(SshConnectionError::Cancelled)));
    }
}
