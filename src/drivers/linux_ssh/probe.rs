use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use russh::{
    client,
    keys::{agent::client::AgentClient, ssh_key::HashAlg},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::HostKeyFingerprint;

use super::connection::supported_algorithm;

/// Performs only the SSH handshake needed to obtain a server Host Key.
///
/// Capture mode always rejects the key, so this function never authenticates
/// or opens a remote command channel. The returned fingerprint still requires
/// explicit operator confirmation before it can be stored.
///
/// # Errors
///
/// Returns an error for invalid input, cancellation, timeout, or a connection
/// failure before a Host Key is observed.
pub async fn capture_host_key(
    host: &str,
    port: u16,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<HostKeyFingerprint, SshProbeError> {
    if host.is_empty()
        || host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || port == 0
    {
        return Err(SshProbeError::InvalidEndpoint);
    }
    if cancellation.is_cancelled() {
        return Err(SshProbeError::Cancelled);
    }
    let verifier = HostKeyVerifier::capture();
    let connect = client::connect(
        Arc::new(client::Config::default()),
        (host, port),
        verifier.clone(),
    );
    let result = tokio::select! {
        () = cancellation.cancelled() => return Err(SshProbeError::Cancelled),
        result = tokio::time::timeout(timeout, connect) => result,
    }
    .map_err(|_| SshProbeError::Timeout { timeout })?;

    if let Some(fingerprint) = verifier.observed() {
        return HostKeyFingerprint::parse(fingerprint)
            .map_err(|error| SshProbeError::InvalidHostKey(error.to_string()));
    }
    match result {
        Ok(_) => Err(SshProbeError::MissingHostKey),
        Err(error) => Err(SshProbeError::Connection(error.to_string())),
    }
}

#[derive(Clone, Debug)]
pub struct HostKeyVerifier {
    expected: Option<HostKeyFingerprint>,
    observed: Arc<Mutex<Option<String>>>,
}

impl HostKeyVerifier {
    /// Creates a verifier that records the first Host Key but rejects the
    /// connection. The TUI can show that fingerprint for explicit confirmation.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            expected: None,
            observed: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn strict(expected: HostKeyFingerprint) -> Self {
        Self {
            expected: Some(expected),
            observed: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn observed(&self) -> Option<String> {
        self.observed.lock().ok().and_then(|value| value.clone())
    }

    fn accepts(&self, fingerprint: &str) -> bool {
        if let Ok(mut observed) = self.observed.lock() {
            *observed = Some(fingerprint.into());
        }
        self.expected
            .as_ref()
            .is_some_and(|expected| expected.as_str() == fingerprint)
    }
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        Ok(self.accepts(&fingerprint))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentitySummary {
    pub fingerprint: String,
    pub comment: String,
    pub certificate: bool,
    pub supported: bool,
}

/// Lists identities exposed by the platform SSH Agent without authenticating
/// to a remote host.
///
/// # Errors
///
/// Returns an error if no supported Agent endpoint is available, the Agent
/// protocol fails, or cancellation is requested.
pub async fn probe_agent_identities(
    cancellation: &CancellationToken,
) -> Result<Vec<AgentIdentitySummary>, SshProbeError> {
    if cancellation.is_cancelled() {
        return Err(SshProbeError::Cancelled);
    }
    let mut agent = tokio::select! {
        () = cancellation.cancelled() => return Err(SshProbeError::Cancelled),
        result = connect_platform_agent() => result?,
    };
    let identities = tokio::select! {
        () = cancellation.cancelled() => return Err(SshProbeError::Cancelled),
        result = agent.request_identities() => result?,
    };
    Ok(identities
        .into_iter()
        .map(|identity| AgentIdentitySummary {
            fingerprint: identity
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string(),
            comment: identity.comment().into(),
            certificate: matches!(
                &identity,
                russh::keys::agent::AgentIdentity::Certificate { .. }
            ),
            supported: supported_algorithm(&identity.public_key().algorithm()),
        })
        .collect())
}

type DynamicAgent = AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>;

#[cfg(unix)]
pub(crate) async fn connect_platform_agent() -> Result<DynamicAgent, SshProbeError> {
    Ok(AgentClient::connect_env().await?.dynamic())
}

#[cfg(windows)]
pub(crate) async fn connect_platform_agent() -> Result<DynamicAgent, SshProbeError> {
    const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    match AgentClient::connect_named_pipe(OPENSSH_AGENT_PIPE).await {
        Ok(agent) => Ok(agent.dynamic()),
        Err(open_ssh_error) => AgentClient::connect_pageant()
            .await
            .map(AgentClient::dynamic)
            .map_err(|pageant_error| SshProbeError::NoAgent {
                open_ssh_error: open_ssh_error.to_string(),
                pageant_error: pageant_error.to_string(),
            }),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) async fn connect_platform_agent() -> Result<DynamicAgent, SshProbeError> {
    Err(SshProbeError::UnsupportedPlatform)
}

#[derive(Debug, Error)]
pub enum SshProbeError {
    #[error("SSH Agent operation failed: {0}")]
    Agent(#[from] russh::keys::Error),
    #[error("SSH Agent probe was cancelled")]
    Cancelled,
    #[error("SSH endpoint must have a non-empty host and non-zero port")]
    InvalidEndpoint,
    #[error("SSH Host Key probe timed out after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error("SSH connection failed before a Host Key was received: {0}")]
    Connection(String),
    #[error("SSH handshake completed without exposing a Host Key")]
    MissingHostKey,
    #[error("SSH server returned an invalid Host Key fingerprint: {0}")]
    InvalidHostKey(String),
    #[cfg(windows)]
    #[error(
        "neither Windows OpenSSH Agent nor Pageant is available: OpenSSH={open_ssh_error}; Pageant={pageant_error}"
    )]
    NoAgent {
        open_ssh_error: String,
        pageant_error: String,
    },
    #[cfg(not(any(unix, windows)))]
    #[error("SSH Agent discovery is unsupported on this platform")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_mode_records_but_never_trusts_a_host_key() {
        let verifier = HostKeyVerifier::capture();
        assert!(!verifier.accepts("SHA256:first"));
        assert_eq!(verifier.observed().as_deref(), Some("SHA256:first"));
    }

    #[test]
    fn strict_mode_accepts_only_the_confirmed_host_key() {
        let expected = HostKeyFingerprint::parse("SHA256:confirmed").unwrap();
        let verifier = HostKeyVerifier::strict(expected);
        assert!(verifier.accepts("SHA256:confirmed"));
        assert!(!verifier.accepts("SHA256:changed"));
        assert_eq!(verifier.observed().as_deref(), Some("SHA256:changed"));
    }

    #[tokio::test]
    async fn cancelled_agent_probe_stops_without_contacting_a_remote_host() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = probe_agent_identities(&cancellation).await;
        assert!(matches!(result, Err(SshProbeError::Cancelled)));
    }

    #[tokio::test]
    async fn cancelled_host_key_probe_stops_before_network_access() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result =
            capture_host_key("example.invalid", 22, Duration::from_secs(1), &cancellation).await;
        assert!(matches!(result, Err(SshProbeError::Cancelled)));
    }

    #[tokio::test]
    async fn invalid_host_key_endpoint_is_rejected_before_network_access() {
        let result =
            capture_host_key("", 22, Duration::from_secs(1), &CancellationToken::new()).await;
        assert!(matches!(result, Err(SshProbeError::InvalidEndpoint)));
    }
}
