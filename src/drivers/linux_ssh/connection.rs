use std::{
    fmt,
    io::Read,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh::{
    ChannelMsg, Disconnect, Sig, client,
    keys::{PrivateKey, PrivateKeyWithHashAlg, ssh_key::Algorithm},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{config::SshCredential, drivers::EventSink, telemetry::CommandSpec};

use super::{HostKeyVerifier, LinuxSshDestination, probe::connect_platform_agent};

const TCP_CONNECTION_PHASE: &str = "TCP connection";
const SSH_HANDSHAKE_PHASE: &str = "SSH handshake and Host Key verification";
const IDENTITY_FILE_LOADING_PHASE: &str = "IdentityFile credential loading";
const SSH_AGENT_LOADING_PHASE: &str = "SSH Agent credential loading";
const USER_AUTHENTICATION_PHASE: &str = "SSH user authentication";
const MAX_IDENTITY_FILE_BYTES: u64 = 1024 * 1024;
const REMOTE_COMMAND_SETTLE_GRACE: Duration = Duration::from_millis(50);
const REMOTE_COMMAND_TERM_GRACE: Duration = Duration::from_millis(400);
const REMOTE_COMMAND_KILL_GRACE: Duration = Duration::from_millis(400);
const REMOTE_COMMAND_CLEANUP_BOUND: Duration = Duration::from_secs(1);
static IDENTITY_FILE_LOAD_LIMITER: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(1)));

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
    sudo_password: Option<crate::config::ProtectedPassword>,
}

impl fmt::Debug for AuthenticatedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthenticatedSession")
            .finish_non_exhaustive()
    }
}

impl AuthenticatedSession {
    pub(super) fn validate_sudo_command(
        &self,
        command: &CommandSpec,
    ) -> Result<(), SshConnectionError> {
        sudo::Request::for_command(command, self.sudo_password.as_ref()).map(|_| ())
    }

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
        let sudo = sudo::Request::for_command(command, self.sudo_password.as_ref())?;
        let execution = sudo.as_ref().map_or(command, |request| &request.command);
        let rendered = execution
            .render_posix()
            .map_err(|error| SshConnectionError::Command(error.to_string()))?;
        let accepted = AtomicBool::new(false);
        let result = execute_command(
            &self.handle,
            &rendered,
            &accepted,
            timeout,
            cancellation,
            sudo.as_ref(),
        )
        .await;
        let result = if sudo.is_some() {
            sudo::sanitize(result)
        } else {
            result
        };
        if let Some(events) = &self.command_events {
            record_command_result(
                events.as_ref(),
                execution,
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

#[derive(Default)]
struct RemoteCommandState {
    exit_status: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl RemoteCommandState {
    fn into_known_output(self) -> Option<RemoteCommandOutput> {
        self.exit_status.map(|exit_status| RemoteCommandOutput {
            exit_status,
            stdout: self.stdout,
            stderr: self.stderr,
            stdout_truncated: self.stdout_truncated,
            stderr_truncated: self.stderr_truncated,
        })
    }

    fn into_output(self) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.into_known_output()
            .ok_or(SshConnectionError::MissingExitStatus)
    }
}

async fn execute_command(
    handle: &client::Handle<HostKeyVerifier>,
    command: &str,
    accepted: &AtomicBool,
    timeout: Duration,
    cancellation: &CancellationToken,
    sudo: Option<&sudo::Request>,
) -> Result<RemoteCommandOutput, SshConnectionError> {
    let deadline = tokio::time::Instant::now().checked_add(timeout);
    let mut channel = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(SshConnectionError::Cancelled),
        () = wait_for_deadline(deadline) => return Err(remote_command_timeout(timeout)),
        result = handle.channel_open_session() => {
            result.map_err(|error| SshConnectionError::Protocol(error.to_string()))?
        }
    };

    let dispatch_interruption = tokio::select! {
        biased;
        () = cancellation.cancelled() => Some(SshConnectionError::Cancelled),
        () = wait_for_deadline(deadline) => Some(remote_command_timeout(timeout)),
        result = channel.exec(true, command) => {
            result
                .map_err(|error| SshConnectionError::Protocol(error.to_string()))?;
            None
        }
    };
    if let Some(error) = dispatch_interruption {
        // Once `exec` has been polled, cancellation may race after its request
        // was queued. Treat the command as possibly dispatched and terminate
        // conservatively instead of only dropping the channel.
        return finish_interrupted_command(
            &mut channel,
            accepted,
            RemoteCommandState::default(),
            error,
        )
        .await;
    }

    receive_command_output(
        &mut channel,
        accepted,
        timeout,
        deadline,
        cancellation,
        sudo,
    )
    .await
}

async fn receive_command_output(
    channel: &mut russh::Channel<client::Msg>,
    accepted: &AtomicBool,
    timeout: Duration,
    deadline: Option<tokio::time::Instant>,
    cancellation: &CancellationToken,
    sudo: Option<&sudo::Request>,
) -> Result<RemoteCommandOutput, SshConnectionError> {
    let mut state = RemoteCommandState::default();
    let mut prompt = sudo::Prompt::default();
    loop {
        let message = tokio::select! {
            biased;
            message = channel.wait() => message,
            () = cancellation.cancelled() => {
                return finish_interrupted_command(
                    channel,
                    accepted,
                    state,
                    SshConnectionError::Cancelled,
                ).await;
            }
            () = wait_for_deadline(deadline) => {
                return finish_interrupted_command(
                    channel,
                    accepted,
                    state,
                    remote_command_timeout(timeout),
                ).await;
            }
        };
        let Some(message) = message else {
            break;
        };
        if let (Some(request), ChannelMsg::ExtendedData { data, .. }) = (sudo, &message)
            && prompt.observe(data)
        {
            let response = async {
                let input = request.response()?;
                channel
                    .data(input.as_bytes())
                    .await
                    .map_err(|_| sudo::error())?;
                channel.eof().await.map_err(|_| sudo::error())
            };
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(SshConnectionError::Cancelled),
                () = wait_for_deadline(deadline) => Err(remote_command_timeout(timeout)),
                result = response => result,
            };
            if let Err(error) = result {
                return finish_interrupted_command(channel, accepted, state, error).await;
            }
        }
        // Password channels may echo input or partial input. Drain without
        // retaining stdout/stderr, even when cancellation later settles a result.
        let message = if sudo.is_some()
            && matches!(
                message,
                ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. }
            ) {
            ChannelMsg::Eof
        } else {
            message
        };
        if let Some(error) = record_command_message(&mut state, accepted, message) {
            close_command_channel(
                channel,
                tokio::time::Instant::now() + REMOTE_COMMAND_CLEANUP_BOUND,
            )
            .await;
            return Err(error);
        }
        // A continuously ready data stream must not starve cancellation or
        // the deadline merely because the message branch is biased. The bias
        // still gives an already-ready terminal message first consideration.
        if let Some(error) = observed_command_interruption(cancellation, deadline, timeout) {
            return finish_interrupted_command(channel, accepted, state, error).await;
        }
    }
    state.into_output()
}

fn record_command_message(
    state: &mut RemoteCommandState,
    accepted: &AtomicBool,
    message: ChannelMsg,
) -> Option<SshConnectionError> {
    match message {
        ChannelMsg::Success => accepted.store(true, Ordering::Release),
        ChannelMsg::Data { data } => {
            append_bounded(&mut state.stdout, &data, &mut state.stdout_truncated);
        }
        ChannelMsg::ExtendedData { data, .. } => {
            append_bounded(&mut state.stderr, &data, &mut state.stderr_truncated);
        }
        ChannelMsg::ExitStatus {
            exit_status: status,
        } => {
            accepted.store(true, Ordering::Release);
            state.exit_status = Some(status);
        }
        ChannelMsg::ExitSignal {
            signal_name,
            error_message,
            ..
        } => {
            accepted.store(true, Ordering::Release);
            return Some(SshConnectionError::ExitSignal {
                signal: format!("{signal_name:?}"),
                message: error_message,
            });
        }
        _ => {}
    }
    None
}

fn observed_command_interruption(
    cancellation: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
    timeout: Duration,
) -> Option<SshConnectionError> {
    if cancellation.is_cancelled() {
        Some(SshConnectionError::Cancelled)
    } else if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        Some(remote_command_timeout(timeout))
    } else {
        None
    }
}

async fn finish_interrupted_command(
    channel: &mut russh::Channel<client::Msg>,
    accepted: &AtomicBool,
    mut state: RemoteCommandState,
    error: SshConnectionError,
) -> Result<RemoteCommandOutput, SshConnectionError> {
    let cleanup_started = tokio::time::Instant::now();
    let cleanup_deadline = cleanup_started + REMOTE_COMMAND_CLEANUP_BOUND;
    if state.exit_status.is_some() {
        close_command_channel(channel, cleanup_deadline).await;
        return state.into_output();
    }

    let settle_deadline = (cleanup_started + REMOTE_COMMAND_SETTLE_GRACE).min(cleanup_deadline);
    if let Some(result) =
        settle_remote_command(channel, accepted, &mut state, settle_deadline).await
    {
        close_command_channel(channel, cleanup_deadline).await;
        return result;
    }
    // Outcomes observed after TERM may be caused by our interruption request;
    // they confirm cleanup but must not replace the original classification.
    terminate_remote_command(channel, accepted, cleanup_deadline).await;
    Err(error)
}

async fn settle_remote_command(
    channel: &mut russh::Channel<client::Msg>,
    accepted: &AtomicBool,
    state: &mut RemoteCommandState,
    deadline: tokio::time::Instant,
) -> Option<Result<RemoteCommandOutput, SshConnectionError>> {
    loop {
        let message = tokio::select! {
            biased;
            message = channel.wait() => message,
            () = tokio::time::sleep_until(deadline) => return None,
        };
        let Some(message) = message else {
            return Some(Err(SshConnectionError::MissingExitStatus));
        };
        if let Some(error) = record_command_message(state, accepted, message) {
            return Some(Err(error));
        }
        if state.exit_status.is_some() {
            return Some(std::mem::take(state).into_output());
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
    }
}

fn remote_command_timeout(timeout: Duration) -> SshConnectionError {
    SshConnectionError::Timeout {
        timeout,
        phase: "remote command",
    }
}

async fn wait_for_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn terminate_remote_command(
    channel: &mut russh::Channel<client::Msg>,
    accepted: &AtomicBool,
    cleanup_deadline: tokio::time::Instant,
) {
    let term_deadline =
        (tokio::time::Instant::now() + REMOTE_COMMAND_TERM_GRACE).min(cleanup_deadline);
    let _ = tokio::time::timeout_at(term_deadline, channel.signal(Sig::TERM)).await;
    if wait_for_remote_termination(channel, accepted, term_deadline).await {
        close_command_channel(channel, cleanup_deadline).await;
        return;
    }
    let kill_deadline =
        (tokio::time::Instant::now() + REMOTE_COMMAND_KILL_GRACE).min(cleanup_deadline);
    let _ = tokio::time::timeout_at(kill_deadline, channel.signal(Sig::KILL)).await;
    let _ = wait_for_remote_termination(channel, accepted, kill_deadline).await;
    close_command_channel(channel, cleanup_deadline).await;
}

async fn wait_for_remote_termination(
    channel: &mut russh::Channel<client::Msg>,
    accepted: &AtomicBool,
    deadline: tokio::time::Instant,
) -> bool {
    loop {
        let Ok(message) = tokio::time::timeout_at(deadline, channel.wait()).await else {
            return false;
        };
        match message {
            None | Some(ChannelMsg::Close | ChannelMsg::Failure | ChannelMsg::OpenFailure(_)) => {
                return true;
            }
            Some(ChannelMsg::Success) => accepted.store(true, Ordering::Release),
            Some(ChannelMsg::ExitStatus { .. } | ChannelMsg::ExitSignal { .. }) => {
                accepted.store(true, Ordering::Release);
                return true;
            }
            Some(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
    }
}

async fn close_command_channel(
    channel: &russh::Channel<client::Msg>,
    deadline: tokio::time::Instant,
) {
    let _ = tokio::time::timeout_at(deadline, channel.close()).await;
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
/// MVP key-file loader and should be loaded through SSH Agent instead.
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
    let config = client_config();
    let socket = connection_phase(deadline, timeout, TCP_CONNECTION_PHASE, async {
        tokio::net::TcpStream::connect((destination.host.as_str(), destination.port))
            .await
            .map_err(|error| SshConnectionError::Protocol(error.to_string()))
    })
    .await?;
    // Match `russh::client::connect`: TCP_NODELAY is best-effort and a
    // platform refusal does not change the protocol or authentication result.
    if config.nodelay {
        let _ = socket.set_nodelay(true);
    }
    let mut handle = connection_phase(deadline, timeout, SSH_HANDSHAKE_PHASE, async {
        client::connect_stream(
            config,
            socket,
            HostKeyVerifier::strict(destination.host_key.clone()),
        )
        .await
        .map_err(|error| SshConnectionError::Protocol(error.to_string()))
    })
    .await?;
    let authenticated =
        authenticate(&mut handle, destination, credential, deadline, timeout).await?;
    if !authenticated {
        return Err(SshConnectionError::Rejected);
    }
    Ok(AuthenticatedSession {
        handle,
        command_events: None,
        sudo_password: match credential {
            SshCredential::Password { protected } => Some(protected.clone()),
            _ => None,
        },
    })
}

fn record_command_result(
    events: &dyn EventSink,
    command: &CommandSpec,
    accepted: bool,
    accepted_statuses: &[u32],
    result: &Result<RemoteCommandOutput, SshConnectionError>,
) {
    // Only service/check commands have a version working directory. Internal
    // probes can return sensitive protocol/configuration data and stay quiet.
    if command.working_directory.is_some() {
        record_service_output(events, result);
    }
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

fn record_service_output(
    events: &dyn EventSink,
    result: &Result<RemoteCommandOutput, SshConnectionError>,
) {
    use crate::telemetry::log_record::{LogEvent, LogEventKind};
    let Ok(output) = result else {
        return;
    };
    for (stream, bytes, truncated) in [
        ("stdout", &output.stdout, output.stdout_truncated),
        ("stderr", &output.stderr, output.stderr_truncated),
    ] {
        if !bytes.is_empty() {
            // Preserve the bounded whole stream for the application log sink's
            // registered-secret/private-key redaction BEFORE fragmentation.
            events.emit_record(LogEvent {
                namespace: format!("linux-ssh.service.{stream}"),
                message: String::from_utf8_lossy(bytes).into_owned(),
                scope: None,
                kind: LogEventKind::Output,
            });
        }
        if truncated {
            events.emit_record(LogEvent {
                namespace: "linux-ssh.service.output".into(),
                message: format!(
                    "Service {stream} exceeded 64 KiB; remaining output is unavailable."
                ),
                scope: None,
                kind: LogEventKind::Output,
            });
        }
    }
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
    deadline: Option<tokio::time::Instant>,
    timeout: Duration,
) -> Result<bool, SshConnectionError> {
    match credential {
        SshCredential::Password { protected } => {
            let password = protected
                .unlock()
                .map_err(|message| SshConnectionError::Protocol(message.into()))?;
            connection_phase(deadline, timeout, USER_AUTHENTICATION_PHASE, async {
                handle
                    .authenticate_password(destination.user.clone(), password.as_str())
                    .await
                    .map(|result| result.success())
                    .map_err(|_| {
                        SshConnectionError::Protocol("SSH password authentication failed".into())
                    })
            })
            .await
        }
        SshCredential::IdentityFile { path } => {
            let key = load_identity_file(path, deadline, timeout).await?;
            connection_phase(deadline, timeout, USER_AUTHENTICATION_PHASE, async {
                handle
                    .authenticate_publickey(
                        destination.user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), None),
                    )
                    .await
                    .map(|result| result.success())
                    .map_err(|error| SshConnectionError::Protocol(error.to_string()))
            })
            .await
        }
        SshCredential::Agent { fingerprint } => {
            let (mut agent, public_key) =
                connection_phase(deadline, timeout, SSH_AGENT_LOADING_PHASE, async {
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
                        .ok_or_else(|| {
                            SshConnectionError::MissingAgentIdentity(fingerprint.clone())
                        })?;
                    if matches!(
                        identity,
                        russh::keys::agent::AgentIdentity::Certificate { .. }
                    ) {
                        return Err(SshConnectionError::UnsupportedCertificate);
                    }
                    let public_key = identity.public_key().into_owned();
                    ensure_supported_algorithm(&public_key.algorithm())?;
                    Ok((agent, public_key))
                })
                .await?;
            connection_phase(deadline, timeout, USER_AUTHENTICATION_PHASE, async {
                handle
                    .authenticate_publickey_with(
                        destination.user.clone(),
                        public_key,
                        None,
                        &mut agent,
                    )
                    .await
                    .map(|result| result.success())
                    .map_err(|error| SshConnectionError::Agent(error.to_string()))
            })
            .await
        }
    }
}

async fn load_identity_file(
    path: &std::path::Path,
    deadline: Option<tokio::time::Instant>,
    timeout: Duration,
) -> Result<PrivateKey, SshConnectionError> {
    let path = path.to_path_buf();
    load_identity_file_with_limiter(
        Arc::clone(&IDENTITY_FILE_LOAD_LIMITER),
        deadline,
        timeout,
        move || load_identity_file_blocking(&path),
    )
    .await
}

async fn load_identity_file_with_limiter(
    limiter: Arc<Semaphore>,
    deadline: Option<tokio::time::Instant>,
    timeout: Duration,
    operation: impl FnOnce() -> Result<PrivateKey, SshConnectionError> + Send + 'static,
) -> Result<PrivateKey, SshConnectionError> {
    connection_phase(deadline, timeout, IDENTITY_FILE_LOADING_PHASE, async move {
        // Filesystem calls can block indefinitely on a disconnected network or
        // virtual filesystem even when their async wrapper is dropped. Keep the
        // whole read/parse operation on one detached, single-flight OS thread:
        // the connection deadline remains effective, retries cannot accumulate
        // blocked workers, and Tokio runtime shutdown never waits for the worker.
        let permit = limiter
            .acquire_owned()
            .await
            .map_err(|_| identity_key_error("the IdentityFile loader is unavailable"))?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let _worker = std::thread::Builder::new()
            .name("shipforge-identity-loader".into())
            .spawn(move || {
                let result = operation();
                drop(permit);
                let _ = sender.send(result);
            })
            .map_err(|_| identity_key_error("the IdentityFile loader could not start"))?;
        receiver
            .await
            .map_err(|_| identity_key_error("the IdentityFile loader stopped unexpectedly"))?
    })
    .await
}

fn load_identity_file_blocking(path: &std::path::Path) -> Result<PrivateKey, SshConnectionError> {
    #[cfg(windows)]
    ensure_direct_windows_identity_path(path)?;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| identity_key_error("the selected file is unavailable"))?;
    ensure_safe_identity_metadata(&metadata)?;
    if metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(identity_key_error(
            "the selected file exceeds the 1 MiB limit",
        ));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // A path swapped after metadata validation must neither follow a
        // symlink nor block this worker on a FIFO/device open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Inspect a swapped reparse point itself instead of following it.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|_| identity_key_error("the selected file could not be opened safely"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| identity_key_error("the selected file metadata is unavailable"))?;
    ensure_safe_identity_metadata(&opened_metadata)?;
    if opened_metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(identity_key_error(
            "the selected file exceeds the 1 MiB limit",
        ));
    }

    let capacity = usize::try_from(opened_metadata.len())
        .map_err(|_| identity_key_error("the selected file exceeds this platform's limit"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_IDENTITY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| identity_key_error("the selected file could not be read"))?;
    if bytes.len() as u64 > MAX_IDENTITY_FILE_BYTES {
        return Err(identity_key_error(
            "the selected file exceeds the 1 MiB limit",
        ));
    }
    let secret = String::from_utf8(bytes)
        .map_err(|_| identity_key_error("the selected file is not valid UTF-8"))?;
    let key = russh::keys::decode_secret_key(&secret, None)
        .map_err(|_| identity_key_error("the selected file is not an unencrypted SSH key"))?;
    ensure_supported_algorithm(&key.algorithm())?;
    Ok(key)
}

fn ensure_safe_identity_metadata(metadata: &std::fs::Metadata) -> Result<(), SshConnectionError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(identity_key_error(
            "the selected path must be a regular, non-symbolic-link file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(identity_key_error(
                "the selected path must not be a Windows reparse point",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_direct_windows_identity_path(path: &std::path::Path) -> Result<(), SshConnectionError> {
    use std::path::{Component, Prefix};

    if path.is_absolute()
        && matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        )
    {
        Ok(())
    } else {
        Err(identity_key_error(
            "the selected IdentityFile must use a direct Windows drive path",
        ))
    }
}

fn identity_key_error(message: &'static str) -> SshConnectionError {
    SshConnectionError::Key(message.into())
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

mod sudo;

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
    async fn identity_file_loader_accepts_only_bounded_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("identity_ed25519");
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        std::fs::write(
            &key_path,
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap(),
        )
        .unwrap();
        let timeout = Duration::from_secs(1);
        let loaded = load_identity_file(
            &key_path,
            tokio::time::Instant::now().checked_add(timeout),
            timeout,
        )
        .await
        .unwrap();
        assert_eq!(loaded.algorithm(), Algorithm::Ed25519);
        #[cfg(windows)]
        {
            let canonical_key_path = std::fs::canonicalize(&key_path).unwrap();
            let loaded = load_identity_file(
                &canonical_key_path,
                tokio::time::Instant::now().checked_add(timeout),
                timeout,
            )
            .await
            .unwrap();
            assert_eq!(loaded.algorithm(), Algorithm::Ed25519);
        }

        let oversized = directory.path().join("oversized-key");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_IDENTITY_FILE_BYTES + 1)
            .unwrap();
        let error = load_identity_file(
            &oversized,
            tokio::time::Instant::now().checked_add(timeout),
            timeout,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SshConnectionError::Key(_)));
        assert!(
            !error
                .to_string()
                .contains(oversized.to_string_lossy().as_ref())
        );

        let error = load_identity_file(
            directory.path(),
            tokio::time::Instant::now().checked_add(timeout),
            timeout,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SshConnectionError::Key(_)));
    }

    #[test]
    fn stalled_identity_loader_is_single_flight_and_does_not_hold_runtime_shutdown() {
        use std::sync::atomic::AtomicUsize;

        let limiter = Arc::new(Semaphore::new(1));
        let release = Arc::new(AtomicBool::new(false));
        let spawned = Arc::new(AtomicUsize::new(0));
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .unwrap();
        let worker_release = Arc::clone(&release);
        let worker_spawned = Arc::clone(&spawned);
        let timeout = Duration::from_millis(200);
        let result = runtime.block_on(async {
            load_identity_file_with_limiter(
                Arc::clone(&limiter),
                tokio::time::Instant::now().checked_add(timeout),
                timeout,
                move || {
                    worker_spawned.fetch_add(1, Ordering::SeqCst);
                    started_sender.send(()).unwrap();
                    while !worker_release.load(Ordering::Acquire) {
                        std::thread::park_timeout(Duration::from_millis(5));
                    }
                    finished_sender.send(()).unwrap();
                    Err(identity_key_error("simulated stalled local read"))
                },
            )
            .await
        });
        assert!(matches!(
            result,
            Err(SshConnectionError::Timeout {
                phase: IDENTITY_FILE_LOADING_PHASE,
                ..
            })
        ));
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(limiter.available_permits(), 0);

        let runtime_drop_started = std::time::Instant::now();
        drop(runtime);
        assert!(runtime_drop_started.elapsed() < Duration::from_secs(1));

        let retry_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .unwrap();
        let retry_spawned = Arc::clone(&spawned);
        let retry_timeout = Duration::from_millis(50);
        let retry = retry_runtime.block_on(async {
            load_identity_file_with_limiter(
                Arc::clone(&limiter),
                tokio::time::Instant::now().checked_add(retry_timeout),
                retry_timeout,
                move || {
                    retry_spawned.fetch_add(1, Ordering::SeqCst);
                    Err(identity_key_error("unexpected concurrent loader"))
                },
            )
            .await
        });
        assert!(matches!(retry, Err(SshConnectionError::Timeout { .. })));
        assert_eq!(spawned.load(Ordering::SeqCst), 1);
        drop(retry_runtime);

        release.store(true, Ordering::Release);
        finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        for _ in 0..100 {
            if limiter.available_permits() == 1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("stalled identity worker did not release its single-flight permit");
    }

    #[cfg(windows)]
    #[test]
    fn identity_file_loader_rejects_nonlocal_windows_paths_before_io() {
        let path = std::path::Path::new(r"\\fixture-server\keys\identity_ed25519");
        let error = load_identity_file_blocking(path).unwrap_err();
        assert!(matches!(error, SshConnectionError::Key(_)));
        assert!(!error.to_string().contains("fixture-server"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_file_loader_rejects_symlinks_and_special_files_without_opening_them() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("regular-key");
        std::fs::write(&regular, "not read through symlink").unwrap();
        let link = directory.path().join("linked-key");
        symlink(&regular, &link).unwrap();
        let socket = directory.path().join("socket-key");
        let _listener = UnixListener::bind(&socket).unwrap();
        let timeout = Duration::from_millis(250);

        for path in [&link, &socket] {
            let started = tokio::time::Instant::now();
            let error = load_identity_file(path, started.checked_add(timeout), timeout)
                .await
                .unwrap_err();
            assert!(matches!(error, SshConnectionError::Key(_)));
            assert!(started.elapsed() < timeout);
            assert!(!error.to_string().contains(path.to_string_lossy().as_ref()));
        }
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
    async fn later_connection_phase_uses_existing_deadline_instead_of_a_fresh_timeout() {
        let timeout = Duration::from_millis(500);
        let started = tokio::time::Instant::now();
        let deadline = started.checked_add(timeout);
        // Simulate earlier connection phases consuming most of the one shared
        // budget before user authentication begins.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let userauth_started = tokio::time::Instant::now();
        let result = connection_phase::<()>(
            deadline,
            timeout,
            USER_AUTHENTICATION_PHASE,
            std::future::pending(),
        )
        .await;
        assert!(matches!(
            result,
            Err(SshConnectionError::Timeout {
                timeout: budget,
                phase: USER_AUTHENTICATION_PHASE,
            }) if budget == timeout
        ));
        assert!(
            userauth_started.elapsed() < Duration::from_millis(350),
            "userauth must receive only the remaining shared budget"
        );
        assert!(
            started.elapsed() < Duration::from_millis(650),
            "a fresh userauth timeout would extend the total budget"
        );
    }

    #[tokio::test]
    async fn split_phase_labels_report_only_static_stage_and_shared_budget() {
        let timeout = Duration::from_secs(15);
        for phase in [
            TCP_CONNECTION_PHASE,
            SSH_HANDSHAKE_PHASE,
            IDENTITY_FILE_LOADING_PHASE,
            SSH_AGENT_LOADING_PHASE,
            USER_AUTHENTICATION_PHASE,
        ] {
            assert_eq!(
                connection_phase(
                    tokio::time::Instant::now().checked_add(timeout),
                    timeout,
                    phase,
                    async { Ok(42) },
                )
                .await
                .unwrap(),
                42
            );
            let error = connection_phase::<()>(
                Some(tokio::time::Instant::now()),
                timeout,
                phase,
                std::future::pending(),
            )
            .await
            .unwrap_err();
            assert!(matches!(
                &error,
                SshConnectionError::Timeout {
                    timeout: budget,
                    phase: observed,
                } if *budget == timeout && *observed == phase
            ));
            assert_eq!(
                error.to_string(),
                format!("SSH operation timed out after {timeout:?} during {phase}")
            );
        }
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
                phase: SSH_HANDSHAKE_PHASE,
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
            connection_phase(deadline, timeout, TCP_CONNECTION_PHASE, async { Ok(42) })
                .await
                .unwrap(),
            42
        );
    }
}
