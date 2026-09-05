use std::{
    collections::BTreeMap,
    net::IpAddr,
    num::NonZeroU8,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh::client;
use sha2::{Digest, Sha256};
use shipforge::{
    config::SshCredential,
    drivers::{
        DriverDestinationInput,
        linux_ssh::{
            AuthenticatedSession, HostKeyVerifier, LinuxSshDestination, RemotePath,
            SshConnectionError, UploadError, UploadOptions, capture_host_key,
            connect_authenticated, probe_agent_identities,
        },
    },
    telemetry::{CommandArgument, CommandSpec},
};
use tempfile::tempdir;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const OPT_IN: &str = "SHIPFORGE_QA01_OPENSSH";
const SSH_HOST: &str = "SHIPFORGE_QA01_SSH_HOST";
const SSH_PORT: &str = "SHIPFORGE_QA01_SSH_PORT";
const SSH_USER: &str = "SHIPFORGE_QA01_SSH_USER";
const CURRENT_HOST_KEY: &str = "SHIPFORGE_QA01_SSH_CURRENT_HOST_KEY";
const PREVIOUS_HOST_KEY: &str = "SHIPFORGE_QA01_SSH_PREVIOUS_HOST_KEY";
const IDENTITY_FILE: &str = "SHIPFORGE_QA01_SSH_IDENTITY_FILE";
const AGENT_FINGERPRINT: &str = "SHIPFORGE_QA01_SSH_AGENT_FINGERPRINT";

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const WORK_DEADLINE: Duration = Duration::from_secs(75);
const HARD_DEADLINE: Duration = Duration::from_secs(110);
const ABORT_JOIN_TIMEOUT: Duration = Duration::from_secs(1);
const REMOTE_CREATION_SETTLE: Duration = Duration::from_secs(10);
const REMOTE_RECONCILE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
struct GateConfig {
    host: String,
    port: u16,
    user: String,
    current_host_key: String,
    previous_host_key: String,
    identity_file: PathBuf,
    agent_fingerprint: String,
}

#[derive(Debug)]
struct RemoteRootGuard {
    path: String,
    owner_token: String,
    attempted: AtomicBool,
    confirmed: AtomicBool,
}

impl RemoteRootGuard {
    fn new(path_id: Uuid, owner_id: Uuid) -> Self {
        Self {
            path: qa_root(path_id),
            owner_token: owner_id.simple().to_string(),
            attempted: AtomicBool::new(false),
            confirmed: AtomicBool::new(false),
        }
    }

    fn mark_creation_possible(&self) {
        self.attempted.store(true, Ordering::Release);
    }
}

/// This release-profile gate contacts only an explicitly configured disposable
/// OpenSSH server on a numeric loopback address. It never loads `ShipForge`'s
/// saved Project, Destination, or Credential registries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicit disposable loopback OpenSSH fixture and SSH Agent"]
async fn qa01_release_gate_validates_host_key_rotation_agent_sftp_and_cancellation() {
    let config = load_gate_config().unwrap_or_else(|error| panic!("{error}"));
    require_release_profile().unwrap_or_else(|error| panic!("{error}"));
    let remote_root = Arc::new(RemoteRootGuard::new(Uuid::now_v7(), Uuid::now_v7()));
    let started = Instant::now();
    let work_deadline = started + WORK_DEADLINE;
    let hard_deadline = started + HARD_DEADLINE;
    let cleanup_deadline = hard_deadline - ABORT_JOIN_TIMEOUT;

    let work_config = config.clone();
    let work_root = Arc::clone(&remote_root);
    let mut work = tokio::spawn(async move { run_gate(&work_config, &work_root).await });
    let work_result = match timeout_at(work_deadline, &mut work).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("QA-01 OpenSSH gate task failed unexpectedly".into()),
        Err(_) => {
            let joined = abort_gate_work(&mut work, &remote_root).await;
            let join_note = if joined {
                ""
            } else {
                "; the aborted work task did not join within its bounded cleanup interval"
            };
            Err(format!(
                "QA-01 OpenSSH gate exceeded its {WORK_DEADLINE:?} work deadline{join_note}"
            ))
        }
    };

    let cleanup_config = config.clone();
    let cleanup_root = Arc::clone(&remote_root);
    let mut cleanup =
        tokio::spawn(async move { cleanup_remote_root(&cleanup_config, &cleanup_root).await });
    let cleanup_result = match timeout_at(cleanup_deadline, &mut cleanup).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("QA-01 cleanup task failed unexpectedly".into()),
        Err(_) => {
            cleanup.abort();
            let _ = timeout_at(hard_deadline, &mut cleanup).await;
            Err(format!(
                "QA-01 cleanup exceeded the gate's {HARD_DEADLINE:?} hard deadline"
            ))
        }
    };

    combine_gate_and_cleanup(work_result, cleanup_result).unwrap_or_else(|error| panic!("{error}"));
}

async fn abort_gate_work(
    work: &mut tokio::task::JoinHandle<Result<(), String>>,
    remote_root: &RemoteRootGuard,
) -> bool {
    // Once the work task reaches its hard stop, conservatively assume the
    // remote mkdir may have been dispatched. Tokio cancellation is cooperative
    // at poll boundaries, so cleanup must not trust a stale worker observation.
    remote_root.mark_creation_possible();
    work.abort();
    timeout(ABORT_JOIN_TIMEOUT, work).await.is_ok()
}

fn require_release_profile() -> Result<(), String> {
    #[cfg(debug_assertions)]
    {
        Err(
            "run this gate with `cargo test --release --test linux_ssh_release_gate qa01_release_gate_validates_host_key_rotation_agent_sftp_and_cancellation -- --ignored --exact --nocapture --test-threads=1`".into(),
        )
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(())
    }
}

async fn run_gate(config: &GateConfig, remote_root: &RemoteRootGuard) -> Result<(), String> {
    require_regular_identity_file(&config.identity_file)?;
    let identity_fingerprint = identity_file_fingerprint(&config.identity_file)?;
    require_distinct_credential_fingerprints(&identity_fingerprint, &config.agent_fingerprint)?;
    let current = destination(config, &config.current_host_key)?;
    verify_captured_and_previous_host_keys(config, &current).await?;

    let identity = SshCredential::IdentityFile {
        path: config.identity_file.clone(),
    };
    let identity_session = connect_authenticated(
        &current,
        &identity,
        CONNECTION_TIMEOUT,
        &CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("IdentityFile authentication failed: {error}"))?;
    verify_marker(&identity_session, "identity-file", &remote_root.path).await?;
    verify_agent_authentication(config, &current, &remote_root.path, &identity_fingerprint).await?;
    create_remote_root(&identity_session, remote_root).await?;
    verify_successful_upload(&identity_session, &remote_root.path).await?;
    verify_cancelled_upload(&identity_session, &remote_root.path).await?;
    verify_cancelled_long_command(config, &current, &identity_session, &remote_root.path).await?;
    identity_session
        .disconnect()
        .await
        .map_err(|error| format!("IdentityFile session disconnect failed: {error}"))
}

async fn verify_captured_and_previous_host_keys(
    config: &GateConfig,
    current: &LinuxSshDestination,
) -> Result<(), String> {
    let cancellation = CancellationToken::new();
    let captured = capture_host_key(&config.host, config.port, CONNECTION_TIMEOUT, &cancellation)
        .await
        .map_err(|error| format!("current Host Key capture failed: {error}"))?;
    if captured.as_str() != config.current_host_key {
        return Err("captured Host Key does not match the configured current Host Key".into());
    }

    let previous = destination(config, &config.previous_host_key)?;
    if current.host != previous.host
        || current.port != previous.port
        || current.user != previous.user
    {
        return Err("Host Key rotation check did not use the same endpoint".into());
    }

    let verifier = HostKeyVerifier::strict(previous.host_key.clone());
    let rejected = timeout(
        CONNECTION_TIMEOUT,
        client::connect(
            Arc::new(client::Config::default()),
            (previous.host.as_str(), previous.port),
            verifier.clone(),
        ),
    )
    .await;
    match rejected {
        Err(_) => return Err("previous Host Key verification timed out".into()),
        Ok(Ok(handle)) => {
            drop(handle);
            return Err("the endpoint accepted the configured previous Host Key".into());
        }
        Ok(Err(_)) => {}
    }
    if verifier.observed().as_deref() != Some(config.current_host_key.as_str()) {
        return Err(
            "previous Host Key rejection did not observe the endpoint's current Host Key".into(),
        );
    }
    Ok(())
}

async fn verify_agent_authentication(
    config: &GateConfig,
    destination: &LinuxSshDestination,
    root: &str,
    identity_fingerprint: &str,
) -> Result<(), String> {
    let identities = probe_agent_identities(&CancellationToken::new())
        .await
        .map_err(|error| format!("SSH Agent probe failed: {error}"))?;
    let selected = identities.iter().find(|identity| {
        identity.fingerprint == config.agent_fingerprint
            && identity.supported
            && !identity.certificate
    });
    if selected.is_none() {
        return Err(
            "configured SSH Agent fingerprint was not exposed as a supported plain identity".into(),
        );
    }
    if identities
        .iter()
        .any(|identity| identity.fingerprint == identity_fingerprint)
    {
        return Err("private SSH Agent unexpectedly exposes the IdentityFile key".into());
    }

    let session = connect_authenticated(
        destination,
        &SshCredential::Agent {
            fingerprint: config.agent_fingerprint.clone(),
        },
        CONNECTION_TIMEOUT,
        &CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("SSH Agent authentication failed: {error}"))?;
    verify_marker(&session, "ssh-agent", root).await?;
    session
        .disconnect()
        .await
        .map_err(|error| format!("SSH Agent session disconnect failed: {error}"))
}

async fn verify_marker(
    session: &AuthenticatedSession,
    credential: &str,
    root: &str,
) -> Result<(), String> {
    let suffix = validate_qa_root(root)?;
    let marker = format!("shipforge-qa01-{suffix}-{credential}");
    let command = remote_command(
        "sh",
        &[
            "-c",
            "printf '%s:%s' \"${SHIPFORGE_QA01_AUTH-}\" \"$1\"",
            "shipforge-qa01-auth-marker",
            &marker,
        ],
    )?;
    let output = execute_zero(session, &command, "read-only marker command").await?;
    let expected = format!("{credential}:{marker}");
    if output.stdout != expected.as_bytes()
        || !output.stderr.is_empty()
        || output.stdout_truncated
        || output.stderr_truncated
    {
        return Err(format!(
            "{credential} authentication returned an invalid marker response"
        ));
    }
    Ok(())
}

async fn create_remote_root(
    session: &AuthenticatedSession,
    remote_root: &RemoteRootGuard,
) -> Result<(), String> {
    validate_remote_root_guard(remote_root)?;
    let marker = owner_marker_path(&remote_root.path)?;
    let command = remote_command(
        "sh",
        &[
            "-c",
            "mkdir -m 700 -- \"$1\" && printf '%s\\n' \"$2\" > \"$3\"",
            "shipforge-qa01-owner",
            &remote_root.path,
            &remote_root.owner_token,
            marker.as_str(),
        ],
    )?;
    remote_root.mark_creation_possible();
    execute_zero(session, &command, "create isolated remote root").await?;
    match observe_remote_root(session, remote_root).await? {
        RemoteRootObservation::Owned => {
            remote_root.confirmed.store(true, Ordering::Release);
            Ok(())
        }
        RemoteRootObservation::Absent => Err("created QA-01 remote root was not observable".into()),
        RemoteRootObservation::Unowned => {
            Err("created QA-01 remote root did not retain its owner marker".into())
        }
    }
}

async fn verify_successful_upload(
    session: &AuthenticatedSession,
    root: &str,
) -> Result<(), String> {
    let local = tempdir().map_err(|error| format!("create local upload fixture: {error}"))?;
    let local_path = local.path().join("successful-release.tar.gz");
    let payload = b"shipforge-qa01-successful-release\n".repeat(8_192);
    std::fs::write(&local_path, &payload)
        .map_err(|error| format!("write local upload fixture: {error}"))?;
    let digest = format!("{:x}", Sha256::digest(&payload));
    let remote_path = qa_child(root, "successful-release.tar.gz")?;

    let receipt = session
        .upload_release(
            &local_path,
            &remote_path,
            one_attempt_upload_options(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .map_err(|error| format!("single-attempt SFTP upload failed: {error}"))?;
    if receipt.attempts != 1
        || receipt.bytes != payload.len() as u64
        || receipt.remote_path != remote_path
    {
        return Err("successful SFTP upload returned an invalid receipt".into());
    }
    session
        .verify_remote_sha256(
            &remote_path,
            &digest,
            COMMAND_TIMEOUT,
            &CancellationToken::new(),
        )
        .await
        .map_err(|error| format!("remote SHA-256 verification failed: {error}"))
}

async fn verify_cancelled_upload(session: &AuthenticatedSession, root: &str) -> Result<(), String> {
    let local = tempdir().map_err(|error| format!("create cancellation fixture: {error}"))?;
    let local_path = local.path().join("cancelled-release.tar.gz");
    let payload = vec![0xa5; 4 * 1024 * 1024];
    std::fs::write(&local_path, payload)
        .map_err(|error| format!("write cancellation fixture: {error}"))?;
    let partial = qa_child(root, "cancelled-release.tar.gz.partial")?;
    let final_path = qa_child(root, "cancelled-release.tar.gz")?;
    let cancellation = CancellationToken::new();
    let callback_cancellation = cancellation.clone();
    let observed_batch = Arc::new(AtomicBool::new(false));
    let callback_observed = Arc::clone(&observed_batch);

    let result = session
        .upload_release(
            &local_path,
            &partial,
            one_attempt_upload_options(),
            &cancellation,
            move |progress| {
                if progress.sent > 0 && !callback_observed.swap(true, Ordering::AcqRel) {
                    callback_cancellation.cancel();
                }
            },
        )
        .await;
    if !observed_batch.load(Ordering::Acquire) {
        return Err("cancelled SFTP upload never reported its first written batch".into());
    }
    if !matches!(result, Err(UploadError::Cancelled)) {
        return Err("SFTP upload did not return Cancelled after first-batch cancellation".into());
    }
    require_remote_path(session, partial.as_str(), RemoteExpectation::Absent).await?;
    require_remote_path(session, final_path.as_str(), RemoteExpectation::Absent).await
}

async fn verify_cancelled_long_command(
    config: &GateConfig,
    destination: &LinuxSshDestination,
    observer: &AuthenticatedSession,
    root: &str,
) -> Result<(), String> {
    let started = qa_child(root, "long-command.started")?;
    let finished = qa_child(root, "long-command.finished")?;
    let command = remote_command(
        "sh",
        &[
            "-c",
            ": > \"$1\"; sleep 5; : > \"$2\"",
            "shipforge-qa01",
            started.as_str(),
            finished.as_str(),
        ],
    )?;
    let session = connect_authenticated(
        destination,
        &SshCredential::IdentityFile {
            path: config.identity_file.clone(),
        },
        CONNECTION_TIMEOUT,
        &CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("long-command session authentication failed: {error}"))?;
    {
        let cancellation = CancellationToken::new();
        let command_future = session.execute(&command, Duration::from_secs(30), &cancellation);
        tokio::pin!(command_future);
        tokio::select! {
            result = &mut command_future => {
                return Err(format!("long command ended before cancellation: {result:?}"));
            }
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        require_remote_path(observer, started.as_str(), RemoteExpectation::Present).await?;
        cancellation.cancel();
        let result = timeout(Duration::from_secs(5), &mut command_future)
            .await
            .map_err(|_| "cancelled long command did not return within five seconds".to_owned())?;
        if !matches!(result, Err(SshConnectionError::Cancelled)) {
            return Err("long command did not report Cancelled".into());
        }
    }
    session
        .disconnect()
        .await
        .map_err(|error| format!("long-command session disconnect failed: {error}"))?;

    // The remote command sleeps for five seconds after creating `started`.
    // Observe beyond that full interval so an ineffective cancellation cannot
    // pass merely because the delayed finish marker has not been written yet.
    tokio::time::sleep(Duration::from_secs(6)).await;
    require_remote_path(observer, finished.as_str(), RemoteExpectation::Absent).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteRootObservation {
    Absent,
    Owned,
    Unowned,
}

async fn observe_remote_root(
    session: &AuthenticatedSession,
    remote_root: &RemoteRootGuard,
) -> Result<RemoteRootObservation, String> {
    validate_remote_root_guard(remote_root)?;
    let marker = owner_marker_path(&remote_root.path)?;
    let inspect = remote_command(
        "sh",
        &[
            "-c",
            "if [ ! -e \"$1\" ] && [ ! -L \"$1\" ]; then exit 3; fi; if [ ! -d \"$1\" ] || [ -L \"$1\" ] || [ ! -f \"$2\" ] || [ -L \"$2\" ]; then exit 4; fi",
            "shipforge-qa01-owner",
            &remote_root.path,
            marker.as_str(),
        ],
    )?;
    let inspected = session
        .execute(&inspect, COMMAND_TIMEOUT, &CancellationToken::new())
        .await
        .map_err(|error| format!("QA-01 owner-marker inspection failed: {error}"))?;
    if !inspected.stdout.is_empty()
        || !inspected.stderr.is_empty()
        || inspected.stdout_truncated
        || inspected.stderr_truncated
    {
        return Err("QA-01 owner-marker inspection returned unexpected output".into());
    }
    match inspected.exit_status {
        3 => return Ok(RemoteRootObservation::Absent),
        4 => return Ok(RemoteRootObservation::Unowned),
        0 => {}
        _ => return Err("QA-01 owner-marker inspection returned an invalid status".into()),
    }

    let read = remote_command("cat", &[marker.as_str()])?;
    let marker_output = session
        .execute(&read, COMMAND_TIMEOUT, &CancellationToken::new())
        .await
        .map_err(|error| format!("QA-01 owner-marker read failed: {error}"))?;
    let expected = format!("{}\n", remote_root.owner_token);
    if marker_output.exit_status == 0
        && marker_output.stdout == expected.as_bytes()
        && marker_output.stderr.is_empty()
        && !marker_output.stdout_truncated
        && !marker_output.stderr_truncated
    {
        Ok(RemoteRootObservation::Owned)
    } else {
        Ok(RemoteRootObservation::Unowned)
    }
}

async fn cleanup_remote_root(
    config: &GateConfig,
    remote_root: &RemoteRootGuard,
) -> Result<(), String> {
    validate_remote_root_guard(remote_root)?;
    if !remote_root.attempted.load(Ordering::Acquire) {
        return Ok(());
    }
    let destination = destination(config, &config.current_host_key)?;
    let session = connect_authenticated(
        &destination,
        &SshCredential::IdentityFile {
            path: config.identity_file.clone(),
        },
        CONNECTION_TIMEOUT,
        &CancellationToken::new(),
    )
    .await
    .map_err(|error| format!("cleanup authentication failed: {error}"))?;

    let reconciliation = reconcile_remote_root(&session, remote_root).await;
    let disconnect = session
        .disconnect()
        .await
        .map_err(|error| format!("cleanup disconnect failed: {error}"));
    combine_cleanup_steps(reconciliation, Ok(()), disconnect)
}

async fn reconcile_remote_root(
    session: &AuthenticatedSession,
    remote_root: &RemoteRootGuard,
) -> Result<(), String> {
    let confirmed = remote_root.confirmed.load(Ordering::Acquire);
    let phase = if confirmed { "confirmed" } else { "attempted" };
    let mut absence = AbsenceSettle::new(!confirmed);

    loop {
        match observe_remote_root(session, remote_root).await? {
            RemoteRootObservation::Absent => {
                let Some(wait) = absence.observe_absent(Instant::now()) else {
                    return Ok(());
                };
                tokio::time::sleep(wait).await;
            }
            RemoteRootObservation::Owned => {
                absence.reset();
                let remove = remote_command("rm", &["-rf", "--", &remote_root.path])?;
                execute_zero(session, &remove, "remove isolated remote root").await?;
            }
            RemoteRootObservation::Unowned => {
                return Err(format!(
                    "{phase} QA-01 remote root failed exact owner-marker verification and was preserved"
                ));
            }
        }
    }
}

#[derive(Debug)]
struct AbsenceSettle {
    required: bool,
    since: Option<Instant>,
}

impl AbsenceSettle {
    fn new(required: bool) -> Self {
        Self {
            required,
            since: None,
        }
    }

    fn reset(&mut self) {
        self.since = None;
    }

    fn observe_absent(&mut self, now: Instant) -> Option<Duration> {
        if !self.required {
            return None;
        }
        let since = *self.since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(since);
        REMOTE_CREATION_SETTLE
            .checked_sub(elapsed)
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(REMOTE_RECONCILE_INTERVAL))
    }
}

fn load_gate_config() -> Result<GateConfig, String> {
    parse_gate_config(&|name| std::env::var(name).ok())
}

fn parse_gate_config(lookup: &impl Fn(&str) -> Option<String>) -> Result<GateConfig, String> {
    if required(lookup, OPT_IN)? != "1" {
        return Err(format!("{OPT_IN} must be exactly `1`"));
    }
    let host = parse_loopback_host(&required(lookup, SSH_HOST)?)?;
    let port = required(lookup, SSH_PORT)?
        .parse::<u16>()
        .map_err(|_| format!("{SSH_PORT} must be a non-zero u16"))?;
    if port == 0 {
        return Err(format!("{SSH_PORT} must be a non-zero u16"));
    }
    let user = required(lookup, SSH_USER)?;
    if user.chars().any(char::is_whitespace) {
        return Err(format!("{SSH_USER} must not contain whitespace"));
    }
    let current_host_key =
        parse_sha256_fingerprint(&required(lookup, CURRENT_HOST_KEY)?, CURRENT_HOST_KEY)?;
    let previous_host_key =
        parse_sha256_fingerprint(&required(lookup, PREVIOUS_HOST_KEY)?, PREVIOUS_HOST_KEY)?;
    if current_host_key == previous_host_key {
        return Err("current and previous Host Key fingerprints must differ".into());
    }
    let identity_file = PathBuf::from(required(lookup, IDENTITY_FILE)?);
    if !identity_file.is_absolute() {
        return Err(format!("{IDENTITY_FILE} must be absolute"));
    }
    let agent_fingerprint =
        parse_sha256_fingerprint(&required(lookup, AGENT_FINGERPRINT)?, AGENT_FINGERPRINT)?;
    Ok(GateConfig {
        host,
        port,
        user,
        current_host_key,
        previous_host_key,
        identity_file,
        agent_fingerprint,
    })
}

fn required(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, String> {
    lookup(name)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .ok_or_else(|| format!("{name} must be set explicitly to a non-empty value"))
}

fn parse_loopback_host(value: &str) -> Result<String, String> {
    let address = value
        .parse::<IpAddr>()
        .map_err(|_| format!("{SSH_HOST} must be a numeric loopback address"))?;
    if !address.is_loopback() {
        return Err(format!("{SSH_HOST} refuses non-loopback addresses"));
    }
    Ok(address.to_string())
}

fn parse_sha256_fingerprint(value: &str, name: &str) -> Result<String, String> {
    let Some(digest) = value.strip_prefix("SHA256:") else {
        return Err(format!("{name} must use OpenSSH SHA256 fingerprint form"));
    };
    if digest.len() != 43
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return Err(format!("{name} must contain one complete SHA-256 digest"));
    }
    Ok(value.to_owned())
}

fn require_regular_identity_file(path: &PathBuf) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("IdentityFile is unavailable: {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("IdentityFile must be a regular, non-symlink file".into());
    }
    Ok(())
}

fn identity_file_fingerprint(path: &PathBuf) -> Result<String, String> {
    let key = russh::keys::load_secret_key(path, None)
        .map_err(|_| "IdentityFile key could not be loaded for credential isolation".to_owned())?;
    Ok(key
        .public_key()
        .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
        .to_string())
}

fn require_distinct_credential_fingerprints(
    identity_fingerprint: &str,
    agent_fingerprint: &str,
) -> Result<(), String> {
    if identity_fingerprint == agent_fingerprint {
        Err("IdentityFile and SSH Agent must use distinct keys".into())
    } else {
        Ok(())
    }
}

fn destination(config: &GateConfig, host_key: &str) -> Result<LinuxSshDestination, String> {
    LinuxSshDestination::validate(&DriverDestinationInput {
        value: serde_json::json!({
            "host": config.host,
            "port": config.port,
            "user": config.user,
            "hostKey": host_key,
        }),
    })
    .map_err(|error| format!("invalid disposable Destination: {error}"))
}

fn qa_root(id: Uuid) -> String {
    format!("/tmp/shipforge-qa01-{}", id.simple())
}

fn validate_remote_root_guard(remote_root: &RemoteRootGuard) -> Result<(), String> {
    let path_token = validate_qa_root(&remote_root.path)?;
    validate_owner_token(&remote_root.owner_token)?;
    if path_token == remote_root.owner_token {
        return Err("QA-01 path and owner tokens must be independent".into());
    }
    Ok(())
}

fn validate_owner_token(token: &str) -> Result<(), String> {
    if token.len() != 32
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("QA-01 owner marker does not contain one lowercase UUID token".into());
    }
    Ok(())
}

fn owner_marker_path(root: &str) -> Result<RemotePath, String> {
    qa_child(root, ".shipforge-qa01-owner")
}

fn validate_qa_root(root: &str) -> Result<&str, String> {
    let Some(suffix) = root.strip_prefix("/tmp/shipforge-qa01-") else {
        return Err("remote root is outside the QA-01 namespace".into());
    };
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("remote root does not contain one lowercase UUID token".into());
    }
    Ok(suffix)
}

fn qa_child(root: &str, name: &str) -> Result<RemotePath, String> {
    validate_qa_root(root)?;
    if name.is_empty()
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err("remote child name is not one safe path segment".into());
    }
    RemotePath::parse(format!("{root}/{name}"))
        .map_err(|error| format!("invalid QA-01 remote path: {error}"))
}

fn one_attempt_upload_options() -> UploadOptions {
    UploadOptions {
        max_attempts: NonZeroU8::MIN,
        attempt_timeout: UPLOAD_TIMEOUT,
        retry_delay: Duration::ZERO,
    }
}

fn remote_command(program: &str, arguments: &[&str]) -> Result<CommandSpec, String> {
    CommandSpec::structured(
        program,
        arguments
            .iter()
            .map(|argument| CommandArgument::plain(*argument)),
    )
    .map_err(|error| format!("invalid QA-01 command: {error}"))
}

async fn execute_zero(
    session: &AuthenticatedSession,
    command: &CommandSpec,
    stage: &str,
) -> Result<shipforge::drivers::linux_ssh::RemoteCommandOutput, String> {
    let output = session
        .execute(command, COMMAND_TIMEOUT, &CancellationToken::new())
        .await
        .map_err(|error| format!("{stage} failed: {error}"))?;
    if output.exit_status != 0 {
        return Err(format!("{stage} exited with status {}", output.exit_status));
    }
    Ok(output)
}

#[derive(Clone, Copy)]
enum RemoteExpectation {
    Absent,
    Present,
}

async fn require_remote_path(
    session: &AuthenticatedSession,
    path: &str,
    expectation: RemoteExpectation,
) -> Result<(), String> {
    let command = match expectation {
        RemoteExpectation::Absent => remote_command(
            "sh",
            &[
                "-c",
                "[ ! -e \"$1\" ] && [ ! -L \"$1\" ]",
                "shipforge-qa01-absence",
                path,
            ],
        )?,
        RemoteExpectation::Present => remote_command("test", &["-e", path])?,
    };
    execute_zero(session, &command, "remote path assertion")
        .await
        .map(|_| ())
}

fn combine_gate_and_cleanup(
    gate: Result<(), String>,
    cleanup: Result<(), String>,
) -> Result<(), String> {
    match (gate, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(gate), Ok(())) => Err(gate),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(gate), Err(cleanup)) => Err(format!("{gate}; additionally, {cleanup}")),
    }
}

fn combine_cleanup_steps(
    removal: Result<(), String>,
    verification: Result<(), String>,
    disconnect: Result<(), String>,
) -> Result<(), String> {
    let failures = [removal, verification, disconnect]
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("QA-01 cleanup failed: {}", failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(character: char) -> String {
        format!("SHA256:{}", character.to_string().repeat(43))
    }

    fn valid_environment() -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            (OPT_IN, "1".into()),
            (SSH_HOST, "127.0.0.1".into()),
            (SSH_PORT, "2222".into()),
            (SSH_USER, "deploy".into()),
            (CURRENT_HOST_KEY, fingerprint('A')),
            (PREVIOUS_HOST_KEY, fingerprint('B')),
            (IDENTITY_FILE, absolute_test_identity()),
            (AGENT_FINGERPRINT, fingerprint('C')),
        ])
    }

    fn absolute_test_identity() -> String {
        if cfg!(windows) {
            r"C:\shipforge-qa01-test-key".into()
        } else {
            "/tmp/shipforge-qa01-test-key".into()
        }
    }

    fn parse(values: &BTreeMap<&'static str, String>) -> Result<GateConfig, String> {
        parse_gate_config(&|name| values.get(name).cloned())
    }

    #[test]
    fn environment_requires_explicit_opt_in_and_numeric_loopback() {
        let values = valid_environment();
        let config = parse(&values).unwrap();
        assert_eq!(config.host, "127.0.0.1");

        for invalid in ["localhost", "192.0.2.1", "example.test"] {
            let mut values = valid_environment();
            values.insert(SSH_HOST, invalid.into());
            assert!(parse(&values).is_err(), "accepted host {invalid}");
        }
        let mut values = valid_environment();
        values.insert(OPT_IN, "true".into());
        assert!(parse(&values).is_err());
    }

    #[test]
    fn environment_rejects_ambiguous_keys_and_identity_paths() {
        let mut same_keys = valid_environment();
        same_keys.insert(PREVIOUS_HOST_KEY, fingerprint('A'));
        assert!(parse(&same_keys).is_err());

        let mut relative_identity = valid_environment();
        relative_identity.insert(IDENTITY_FILE, "keys/id_ed25519".into());
        assert!(parse(&relative_identity).is_err());

        let mut malformed_agent = valid_environment();
        malformed_agent.insert(AGENT_FINGERPRINT, "SHA256:short".into());
        assert!(parse(&malformed_agent).is_err());
    }

    #[test]
    fn identity_file_and_agent_fingerprints_must_be_distinct() {
        assert!(
            require_distinct_credential_fingerprints(&fingerprint('A'), &fingerprint('B')).is_ok()
        );
        assert!(
            require_distinct_credential_fingerprints(&fingerprint('A'), &fingerprint('A')).is_err()
        );
    }

    #[test]
    fn remote_paths_are_confined_to_one_random_qa01_root() {
        let id = Uuid::parse_str("018f47d2-39e7-7b73-8000-000000000001").unwrap();
        let root = qa_root(id);
        assert_eq!(root, "/tmp/shipforge-qa01-018f47d239e77b738000000000000001");
        assert_eq!(
            qa_child(&root, "release.tar.gz").unwrap().as_str(),
            format!("{root}/release.tar.gz")
        );
        for invalid in ["../escape", "nested/file", ".", ""] {
            assert!(qa_child(&root, invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(
            owner_marker_path(&root).unwrap().as_str(),
            format!("{root}/.shipforge-qa01-owner")
        );
        assert!(validate_qa_root("/tmp/shipforge-qa01-short").is_err());
        assert!(validate_qa_root("/srv/shipforge-qa01-018f47d239e77b738000000000000001").is_err());
    }

    #[test]
    fn remote_root_guard_tracks_attempted_and_confirmed_ownership_independently() {
        let path_id = Uuid::parse_str("018f47d2-39e7-7b73-8000-000000000001").unwrap();
        let owner_id = Uuid::parse_str("018f47d2-39e7-7b73-8000-000000000002").unwrap();
        let guard = RemoteRootGuard::new(path_id, owner_id);
        assert!(validate_remote_root_guard(&guard).is_ok());
        assert!(!guard.attempted.load(Ordering::Acquire));
        assert!(!guard.confirmed.load(Ordering::Acquire));
        guard.mark_creation_possible();
        guard.confirmed.store(true, Ordering::Release);
        assert!(guard.attempted.load(Ordering::Acquire));
        assert!(guard.confirmed.load(Ordering::Acquire));

        let invalid = RemoteRootGuard {
            path: guard.path.clone(),
            owner_token: validate_qa_root(&guard.path).unwrap().to_owned(),
            attempted: AtomicBool::new(false),
            confirmed: AtomicBool::new(false),
        };
        assert!(validate_remote_root_guard(&invalid).is_err());
    }

    #[tokio::test]
    async fn work_deadline_marks_remote_creation_possible_before_aborting() {
        let path_id = Uuid::parse_str("018f47d2-39e7-7b73-8000-000000000001").unwrap();
        let owner_id = Uuid::parse_str("018f47d2-39e7-7b73-8000-000000000002").unwrap();
        let guard = RemoteRootGuard::new(path_id, owner_id);
        let mut work = tokio::spawn(std::future::pending::<Result<(), String>>());

        assert!(!guard.attempted.load(Ordering::Acquire));
        assert!(abort_gate_work(&mut work, &guard).await);
        assert!(guard.attempted.load(Ordering::Acquire));
        assert!(work.is_finished());
    }

    #[test]
    fn unconfirmed_remote_root_requires_continuous_absence_before_cleanup_completes() {
        let started = Instant::now();
        let mut settle = AbsenceSettle::new(true);
        assert_eq!(
            settle.observe_absent(started),
            Some(REMOTE_RECONCILE_INTERVAL)
        );
        assert_eq!(
            settle.observe_absent(started + REMOTE_CREATION_SETTLE - Duration::from_millis(1)),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            settle.observe_absent(started + REMOTE_CREATION_SETTLE),
            None
        );

        settle.reset();
        assert_eq!(
            settle.observe_absent(started + REMOTE_CREATION_SETTLE),
            Some(REMOTE_RECONCILE_INTERVAL)
        );
        assert_eq!(AbsenceSettle::new(false).observe_absent(started), None);
    }
}
