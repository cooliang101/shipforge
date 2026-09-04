use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh::{
    ChannelMsg, Disconnect, client,
    keys::{PrivateKeyWithHashAlg, ssh_key::Algorithm},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{config::SshCredential, drivers::EventSink, telemetry::CommandSpec};

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
    command_events: Option<Arc<dyn EventSink>>,
}

impl fmt::Debug for AuthenticatedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedSession")
            .finish_non_exhaustive()
    }
}

impl AuthenticatedSession {
    pub(super) fn with_command_events(mut self, events: Arc<dyn EventSink>) -> Self {
        self.command_events = Some(events);
        self
    }

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
        self.execute_allowing(command, timeout, cancellation, &[0])
            .await
    }

    /// The caller explicitly supplies protocol-defined nonzero predicate results.
    /// These are observations, not failed commands; status interpretation is unchanged.
    pub(super) async fn execute_allowing(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
        accepted_statuses: &[u32],
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        if cancellation.is_cancelled() {
            return Err(SshConnectionError::Cancelled);
        }
        let rendered = command
            .render_posix()
            .map_err(|error| SshConnectionError::Command(error.to_string()))?;
        let accepted = AtomicBool::new(false);
        let operation = execute_command(&self.handle, &rendered, &accepted);
        let result = tokio::select! {
            () = cancellation.cancelled() => Err(SshConnectionError::Cancelled),
            result = tokio::time::timeout(timeout, operation) => {
                result.unwrap_or(Err(SshConnectionError::Timeout { timeout, phase: "remote command" }))
            }
        };
        if let Some(events) = &self.command_events {
            record_command_result(
                events.as_ref(),
                command,
                accepted.load(Ordering::Acquire),
                accepted_statuses,
                &result,
            );
        }
        result
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
    accepted: &AtomicBool,
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
            ChannelMsg::Success => accepted.store(true, Ordering::Release),
            ChannelMsg::Data { data } => {
                append_bounded(&mut stdout, &data, &mut stdout_truncated);
            }
            ChannelMsg::ExtendedData { data, .. } => {
                append_bounded(&mut stderr, &data, &mut stderr_truncated);
            }
            ChannelMsg::ExitStatus {
                exit_status: status,
            } => {
                accepted.store(true, Ordering::Release);
                exit_status = Some(status);
            }
            ChannelMsg::ExitSignal {
                signal_name,
                error_message,
                ..
            } => {
                accepted.store(true, Ordering::Release);
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
    let operation = connect_and_authenticate(destination, credential, timeout);
    tokio::select! {
        () = cancellation.cancelled() => Err(SshConnectionError::Cancelled),
        result = operation => result,
    }
}

async fn connect_and_authenticate(
    destination: &LinuxSshDestination,
    credential: &SshCredential,
    timeout: Duration,
) -> Result<AuthenticatedSession, SshConnectionError> {
    // One deadline covers all stages. Diagnostics must not grant authentication
    // a fresh timeout or weaken the confirmed Host Key check.
    let deadline = tokio::time::Instant::now().checked_add(timeout);
    let mut handle = connection_phase(
        deadline,
        timeout,
        "network connection and SSH handshake",
        async {
            client::connect(
                client_config(),
                (destination.host.as_str(), destination.port),
                HostKeyVerifier::strict(destination.host_key.clone()),
            )
            .await
            .map_err(|error| SshConnectionError::Protocol(error.to_string()))
        },
    )
    .await?;
    let authenticated = connection_phase(
        deadline,
        timeout,
        "credential loading and authentication",
        authenticate(&mut handle, destination, credential),
    )
    .await?;
    if !authenticated {
        return Err(SshConnectionError::Rejected);
    }
    Ok(AuthenticatedSession {
        handle,
        command_events: None,
    })
}

fn record_command_result(
    events: &dyn EventSink,
    command: &CommandSpec,
    accepted: bool,
    accepted_statuses: &[u32],
    result: &Result<RemoteCommandOutput, SshConnectionError>,
) {
    let event = match result {
        Ok(output) if accepted_statuses.contains(&output.exit_status) => return,
        Ok(_) => super::command_events::failed(
            command,
            "Remote command returned an unsuccessful exit status.",
        ),
        Err(_) if accepted => super::command_events::failed(
            command,
            "Remote command did not complete normally; its final remote outcome may be unknown.",
        ),
        Err(_) => super::command_events::unavailable(
            "Remote command dispatch was not confirmed; a failed-command snapshot is unavailable.",
        ),
    };
    events.emit_record(event);
}

async fn connection_phase<T>(
    deadline: Option<tokio::time::Instant>,
    timeout: Duration,
    phase: &'static str,
    operation: impl std::future::Future<Output = Result<T, SshConnectionError>>,
) -> Result<T, SshConnectionError> {
    // Match Tokio's effectively unbounded timeout for an unrepresentable
    // deadline rather than panicking on a caller-provided Duration.
    let Some(deadline) = deadline else {
        return operation.await;
    };
    tokio::time::timeout_at(deadline, operation)
        .await
        .map_err(|_| SshConnectionError::Timeout { timeout, phase })?
}

async fn authenticate(
    handle: &mut client::Handle<HostKeyVerifier>,
    destination: &LinuxSshDestination,
    credential: &SshCredential,
) -> Result<bool, SshConnectionError> {
    Ok(match credential {
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
    })
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
    #[error("SSH operation timed out after {timeout:?} during {phase}")]
    Timeout {
        timeout: Duration,
        phase: &'static str,
    },
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
mod diagnostics_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_evidence_respects_dispatch_confirmation_and_expected_predicate_results() {
        use crate::{
            drivers::DriverLog,
            telemetry::{
                CommandArgument,
                log_record::{LogEvent, LogEventKind},
            },
        };
        use std::sync::Mutex;

        #[derive(Default)]
        struct Events(Mutex<Vec<LogEvent>>);
        impl EventSink for Events {
            fn emit(&self, _event: DriverLog) {
                panic!("structured event required");
            }
            fn emit_record(&self, event: LogEvent) {
                self.0.lock().unwrap().push(event);
            }
        }
        let events = Events::default();
        let command =
            CommandSpec::structured("test", ["-e", "/actual/path"].map(CommandArgument::plain))
                .unwrap();
        let output = |status| {
            Ok(RemoteCommandOutput {
                exit_status: status,
                stdout: Vec::new(),
                stderr: b"malicious guessed command must not supply argv".to_vec(),
                stdout_truncated: false,
                stderr_truncated: false,
            })
        };
        for status in [0, 1] {
            record_command_result(&events, &command, true, &[0, 1], &output(status));
        }
        record_command_result(&events, &command, true, &[0, 44], &output(44));
        assert!(events.0.lock().unwrap().is_empty());
        record_command_result(&events, &command, true, &[0, 1], &output(2));
        record_command_result(
            &events,
            &command,
            false,
            &[0],
            &Err(SshConnectionError::Protocol("secret raw failure".into())),
        );
        record_command_result(
            &events,
            &command,
            true,
            &[0],
            &Err(SshConnectionError::Cancelled),
        );
        let recorded = events.0.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        let LogEventKind::FailedCommand { command } = &recorded[0].kind else {
            panic!("actual attempted argv expected");
        };
        assert_eq!(command.program, "test");
        assert_eq!(command.args, ["-e", "/actual/path"]);
        assert!(matches!(
            recorded[1].kind,
            LogEventKind::CommandUnavailable { .. }
        ));
        assert!(matches!(
            recorded[2].kind,
            LogEventKind::FailedCommand { .. }
        ));
        let text = serde_json::to_string(&*recorded).unwrap();
        assert!(!text.contains("secret raw"));
        assert!(!text.contains("malicious guessed"));
        assert!(recorded.iter().all(|event| event.scope.is_none()));
    }

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

    #[tokio::test]
    async fn authentication_uses_existing_deadline_instead_of_a_fresh_timeout() {
        let timeout = Duration::from_secs(15);
        let deadline = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            connection_phase::<()>(
                Some(deadline),
                timeout,
                "credential loading and authentication",
                std::future::pending(),
            ),
        )
        .await
        .expect("an exhausted shared deadline must not grant another 15 seconds");
        assert!(matches!(
            result,
            Err(SshConnectionError::Timeout {
                timeout: budget,
                phase: "credential loading and authentication",
            }) if budget == timeout
        ));
    }

    #[tokio::test]
    async fn stalled_loopback_handshake_reports_stage_without_loading_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = LinuxSshDestination {
            driver: crate::drivers::DriverKind::linux_ssh(),
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            user: "deploy".into(),
            host_key: crate::config::HostKeyFingerprint::parse("SHA256:test").unwrap(),
        };
        let credential = SshCredential::IdentityFile {
            path: std::path::PathBuf::from("private-key-sentinel-must-not-be-loaded"),
        };
        let cancellation = CancellationToken::new();
        let (result, ()) = tokio::join!(
            connect_authenticated(
                &destination,
                &credential,
                Duration::from_millis(100),
                &cancellation,
            ),
            async {
                if let Ok(Ok((socket, _))) =
                    tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
                {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    drop(socket);
                }
            }
        );
        let error = result.unwrap_err();
        assert!(matches!(
            error,
            SshConnectionError::Timeout {
                phase: "network connection and SSH handshake",
                ..
            }
        ));
        assert!(!error.to_string().contains("private-key-sentinel"));
    }

    #[tokio::test]
    async fn unrepresentable_deadline_does_not_panic_or_discard_result() {
        let timeout = Duration::MAX;
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        assert!(deadline.is_none());
        assert_eq!(
            connection_phase(
                deadline,
                timeout,
                "network connection and SSH handshake",
                async { Ok(42) }
            )
            .await
            .unwrap(),
            42
        );
    }
}
