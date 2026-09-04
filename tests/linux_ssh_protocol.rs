use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read as _;
use std::num::NonZeroU8;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, ssh_key},
    server::{self, Msg, Server as _, Session},
};
use sha2::Digest;
use shipforge::{
    application::{DeploymentPlanner, package_release},
    config::{CredentialRegistry, ResolvedArtifact, ResolvedArtifactKind, SshCredential},
    domain::{
        Capability, ComponentGeneration, ComponentName, ComponentRelease, DeploymentId,
        DestinationKey, DestinationRevision, DriverCapabilities, EnvironmentId, ProjectId,
        ReleaseVersion,
    },
    drivers::{
        ComponentExecutionContext, ComponentRequest, DeploymentDriver, DriverDestinationInput,
        DriverLog, DriverRegistry, DriverTargetInput, EndpointFingerprint, EventSink, ReleaseRef,
        linux_ssh::{
            ActivatedRemoteRelease, ActivationOptions, AuthenticatedSession, HealthCheckOptions,
            HealthVerificationError, LinuxSshDestination, LinuxSshDriver, LinuxSshTarget,
            PrepareReleaseOptions, RemotePath, RemoteRootState, UploadError, UploadOptions,
            capture_host_key, connect_authenticated, probe_remote_setup,
        },
    },
    telemetry::{CommandArgument, CommandSpec},
};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[path = "support/deployment_service.rs"]
mod deployment_service;
#[path = "support/preflight_protocol.rs"]
mod preflight_protocol;

const SENTINEL: &str = "literal;$(not-executed) ' value";

#[derive(Debug)]
struct IgnoreEvents;

impl EventSink for IgnoreEvents {
    fn emit(&self, _event: DriverLog) {}
}

#[derive(Clone, Debug, Default)]
struct ProtocolServer {
    commands: Arc<Mutex<Vec<String>>>,
    channels: Arc<AsyncMutex<HashMap<ChannelId, Channel<Msg>>>>,
    transfer: Arc<AsyncMutex<TransferState>>,
}

#[derive(Debug, Default)]
struct TransferState {
    files: HashMap<String, Vec<u8>>,
    directories: HashSet<String>,
    links: HashMap<String, String>,
    fail_writes_remaining: usize,
    removals: usize,
    health_status: u16,
}

impl server::Server for ProtocolServer {
    type Handler = Self;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl server::Handler for ProtocolServer {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _public_key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.remove(&channel);
        let command = String::from_utf8_lossy(data).into_owned();
        self.commands.lock().unwrap().push(command.clone());
        let (status, output) = if let Some(reply) = preflight_protocol::reply(&command) {
            reply
        } else if command.starts_with("'printf' ") {
            (0, SENTINEL.as_bytes())
        } else if command
            == "'systemctl' 'list-unit-files' '--type=service' '--no-legend' '--no-pager'"
        {
            (0, &b"worker.service enabled\napi.service disabled\n"[..])
        } else if let Some(path) = command
            .strip_prefix("'sha256sum' '--' '")
            .and_then(|value| value.strip_suffix('\''))
        {
            let transfer = self.transfer.lock().await;
            let Some(bytes) = transfer.files.get(path) else {
                session.channel_success(channel)?;
                session.exit_status_request(channel, 1)?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            };
            let digest = format!("{:x}", sha2::Sha256::digest(bytes));
            let output = format!("{digest}  {path}\n");
            session.channel_success(channel)?;
            session.data(channel, output.into_bytes())?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        } else if let Some((status, output)) = release_command(&command, &self.transfer).await {
            session.channel_success(channel)?;
            if !output.is_empty() {
                session.data(channel, output)?;
            }
            session.exit_status_request(channel, status)?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        } else {
            (127, &b""[..])
        };
        session.channel_success(channel)?;
        if !output.is_empty() {
            session.data(channel, output.to_vec())?;
        }
        session.exit_status_request(channel, status)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel)?;
            return Ok(());
        }
        let Some(channel_stream) = self.channels.lock().await.remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let transfer = self.transfer.clone();
        tokio::spawn(async move {
            russh_sftp::server::run(channel_stream.into_stream(), MemorySftp::new(transfer)).await;
        });
        Ok(())
    }
}

async fn release_command(
    command: &str,
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let words = command
        .split("' '")
        .map(|word| word.trim_matches('\''))
        .collect::<Vec<_>>();
    match words.first().copied()? {
        "mkdir" => mkdir_command(&words, transfer).await,
        "tar" => tar_command(&words, transfer).await,
        "mv" => move_command(&words, transfer).await,
        "test" => test_command(&words, transfer).await,
        "head" => {
            let state = transfer.lock().await;
            let limit: usize = words.get(2)?.parse().ok()?;
            match state.files.get(*words.last()?) {
                Some(bytes) => Some((0, bytes.iter().take(limit).copied().collect())),
                None => Some((1, Vec::new())),
            }
        }
        "find" => {
            let state = transfer.lock().await;
            let prefix = format!("{}/", words.get(1)?);
            let child = state
                .files
                .keys()
                .chain(state.links.keys())
                .chain(state.directories.iter())
                .find(|path| path.starts_with(&prefix));
            Some((
                0,
                child.map_or_else(Vec::new, |path| format!("{path}\n").into_bytes()),
            ))
        }
        "ln" => link_command(&words, transfer).await,
        "rm" => {
            let path = *words.last()?;
            let mut state = transfer.lock().await;
            state.files.remove(path);
            state.links.remove(path);
            Some((0, Vec::new()))
        }
        "stat" => Some((0, b"1\n1\n".to_vec())),
        "readlink" => {
            let path = *words.last()?;
            let target = transfer.lock().await.links.get(path)?.clone();
            Some((0, format!("{target}\n").into_bytes()))
        }
        "systemctl" => Some((0, Vec::new())),
        "curl" => Some((
            0,
            transfer.lock().await.health_status.to_string().into_bytes(),
        )),
        _ => None,
    }
}

async fn mkdir_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let mut state = transfer.lock().await;
    for path in words.iter().skip_while(|word| **word != "--").skip(1) {
        state.directories.insert((*path).to_owned());
    }
    Some((0, Vec::new()))
}

async fn tar_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let directory = *words.get(words.iter().position(|word| *word == "--directory")? + 1)?;
    let file = *words.get(words.iter().position(|word| *word == "--file")? + 1)?;
    let bytes = transfer.lock().await.files.get(file)?.clone();
    let manifest = archive_manifest(&bytes)?;
    transfer
        .lock()
        .await
        .files
        .insert(format!("{directory}/manifest.json"), manifest);
    Some((0, Vec::new()))
}

fn archive_manifest(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if entry.path().ok()?.as_ref() == Path::new("manifest.json") {
            let mut manifest = Vec::new();
            entry.read_to_end(&mut manifest).ok()?;
            return Some(manifest);
        }
    }
    None
}

async fn move_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let source = *words.get(words.len().checked_sub(2)?)?;
    let destination = *words.last()?;
    let mut state = transfer.lock().await;
    if let Some(link) = state.links.remove(source) {
        state.links.insert(destination.to_owned(), link);
        return Some((0, Vec::new()));
    }
    if !state.directories.remove(source) {
        return Some((1, Vec::new()));
    }
    state.directories.insert(destination.to_owned());
    let moved = state
        .files
        .iter()
        .filter(|(path, _)| path.starts_with(&format!("{source}/")))
        .map(|(path, bytes)| {
            (
                path.replacen(source, destination, 1),
                path.clone(),
                bytes.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (new_path, old_path, bytes) in moved {
        state.files.remove(&old_path);
        state.files.insert(new_path, bytes);
    }
    Some((0, Vec::new()))
}

async fn test_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let predicate = *words.get(1)?;
    let path = *words.get(2)?;
    let state = transfer.lock().await;
    let exists = match predicate {
        "-L" => state.links.contains_key(path),
        "-e" => {
            state.links.contains_key(path)
                || state.files.contains_key(path)
                || state.directories.contains(path)
        }
        "-f" => state.files.contains_key(path),
        "-d" => state.links.get(path).map_or_else(
            || state.directories.contains(path),
            |link| {
                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                state.directories.contains(&format!("{parent}/{link}"))
            },
        ),
        _ => false,
    };
    Some((u32::from(!exists), Vec::new()))
}

async fn link_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let mut state = transfer.lock().await;
    if words.get(1) == Some(&"--symbolic") {
        let target = *words.get(3)?;
        let destination = *words.get(4)?;
        if state.links.contains_key(destination)
            || state.files.contains_key(destination)
            || state.directories.contains(destination)
        {
            return Some((1, Vec::new()));
        }
        state
            .links
            .insert(destination.to_owned(), target.to_owned());
        return Some((0, Vec::new()));
    }
    let source = *words.get(words.len().checked_sub(2)?)?;
    let destination = *words.last()?;
    if state.files.contains_key(destination)
        || state.links.contains_key(destination)
        || state.directories.contains(destination)
    {
        return Some((1, Vec::new()));
    }
    let bytes = state.files.get(source)?.clone();
    state.files.insert(destination.to_owned(), bytes);
    Some((0, Vec::new()))
}

struct MemorySftp {
    transfer: Arc<AsyncMutex<TransferState>>,
    handles: HashMap<String, String>,
    next_handle: u64,
}

impl MemorySftp {
    fn new(transfer: Arc<AsyncMutex<TransferState>>) -> Self {
        Self {
            transfer,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }
}

impl russh_sftp::server::Handler for MemorySftp {
    type Error = russh_sftp::protocol::StatusCode;

    fn unimplemented(&self) -> Self::Error {
        russh_sftp::protocol::StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: russh_sftp::protocol::OpenFlags,
        _attrs: russh_sftp::protocol::FileAttributes,
    ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
        use russh_sftp::protocol::{OpenFlags, StatusCode};
        let mut transfer = self.transfer.lock().await;
        if transfer.files.contains_key(&filename) && flags.contains(OpenFlags::EXCLUDE) {
            return Err(StatusCode::Failure);
        }
        if !transfer.files.contains_key(&filename) && !flags.contains(OpenFlags::CREATE) {
            return Err(StatusCode::NoSuchFile);
        }
        transfer.files.insert(filename.clone(), Vec::new());
        self.next_handle += 1;
        let handle = format!("handle-{}", self.next_handle);
        self.handles.insert(handle.clone(), filename);
        Ok(russh_sftp::protocol::Handle { id, handle })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        use russh_sftp::protocol::StatusCode;
        let path = self
            .handles
            .get(&handle)
            .ok_or(StatusCode::Failure)?
            .clone();
        let offset: usize = offset.try_into().map_err(|_| StatusCode::Failure)?;
        let mut transfer = self.transfer.lock().await;
        let file = transfer
            .files
            .get_mut(&path)
            .ok_or(StatusCode::NoSuchFile)?;
        if file.len() < offset {
            file.resize(offset, 0);
        }
        let end = offset.checked_add(data.len()).ok_or(StatusCode::Failure)?;
        if file.len() < end {
            file.resize(end, 0);
        }
        file[offset..end].copy_from_slice(&data);
        if transfer.fail_writes_remaining > 0 {
            transfer.fail_writes_remaining -= 1;
            return Err(StatusCode::Failure);
        }
        Ok(ok_status(id))
    }

    async fn close(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok_status(id))
    }

    async fn stat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        use russh_sftp::protocol::{FileAttributes, StatusCode};
        let transfer = self.transfer.lock().await;
        let bytes = transfer.files.get(&path).ok_or(StatusCode::NoSuchFile)?;
        Ok(russh_sftp::protocol::Attrs {
            id,
            attrs: FileAttributes {
                size: Some(bytes.len() as u64),
                permissions: Some(0o100_600),
                ..FileAttributes::default()
            },
        })
    }

    async fn remove(
        &mut self,
        id: u32,
        filename: String,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        use russh_sftp::protocol::StatusCode;
        let mut transfer = self.transfer.lock().await;
        transfer
            .files
            .remove(&filename)
            .ok_or(StatusCode::NoSuchFile)?;
        transfer.removals += 1;
        Ok(ok_status(id))
    }
}

fn ok_status(id: u32) -> russh_sftp::protocol::Status {
    russh_sftp::protocol::Status {
        id,
        status_code: russh_sftp::protocol::StatusCode::Ok,
        error_message: "ok".into(),
        language_tag: "en".into(),
    }
}

async fn connect_client(
    address: std::net::SocketAddr,
    host_fingerprint: &str,
    directory: &Path,
    cancellation: &CancellationToken,
) -> AuthenticatedSession {
    let identity = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let identity_path = directory.join("id_ed25519");
    std::fs::write(
        &identity_path,
        identity.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let captured = capture_host_key(
        "127.0.0.1",
        address.port(),
        Duration::from_secs(5),
        cancellation,
    )
    .await
    .unwrap();
    assert_eq!(captured.as_str(), host_fingerprint);
    let destination = LinuxSshDestination::validate(&DriverDestinationInput {
        value: serde_json::json!({
            "host": "127.0.0.1",
            "port": address.port(),
            "user": "deploy",
            "hostKey": host_fingerprint,
        }),
    })
    .unwrap();
    connect_authenticated(
        &destination,
        &SshCredential::IdentityFile {
            path: identity_path,
        },
        Duration::from_secs(5),
        cancellation,
    )
    .await
    .unwrap()
}

async fn validate_exec(session: &AuthenticatedSession, cancellation: &CancellationToken) {
    let output = session
        .execute(
            &CommandSpec::structured("printf", [CommandArgument::plain(SENTINEL)]).unwrap(),
            Duration::from_secs(5),
            cancellation,
        )
        .await
        .unwrap();
    assert_eq!(output.exit_status, 0);
    assert_eq!(output.stdout, SENTINEL.as_bytes());
}

async fn validate_transfer(
    session: &AuthenticatedSession,
    directory: &Path,
    cancellation: &CancellationToken,
    transfer: &Arc<AsyncMutex<TransferState>>,
) {
    let release_path = directory.join("release.tar.gz");
    let release_bytes = vec![b'x'; 150_000];
    std::fs::write(&release_path, &release_bytes).unwrap();
    let remote_path = RemotePath::parse("/tmp/shipforge-protocol-release.tar.gz").unwrap();
    let progress = Arc::new(Mutex::new(Vec::new()));
    let progress_events = progress.clone();
    let receipt = session
        .upload_release(
            &release_path,
            &remote_path,
            UploadOptions {
                max_attempts: NonZeroU8::new(2).unwrap(),
                attempt_timeout: Duration::from_secs(5),
                retry_delay: Duration::ZERO,
            },
            cancellation,
            move |event| progress_events.lock().unwrap().push(event),
        )
        .await
        .unwrap();
    assert_eq!(receipt.bytes, release_bytes.len() as u64);
    assert_eq!(receipt.attempts, 2);
    {
        let events = progress.lock().unwrap();
        assert_eq!(events.first().unwrap().sent, 0);
        assert_eq!(events.last().unwrap().sent, release_bytes.len() as u64);
        assert!(events.iter().any(|event| event.attempt == 2));
    }

    let digest = format!("{:x}", sha2::Sha256::digest(&release_bytes));
    session
        .verify_remote_sha256(&remote_path, &digest, Duration::from_secs(5), cancellation)
        .await
        .unwrap();
    let mismatch = session
        .verify_remote_sha256(
            &remote_path,
            &"0".repeat(64),
            Duration::from_secs(5),
            cancellation,
        )
        .await;
    assert!(matches!(mismatch, Err(UploadError::HashMismatch { .. })));

    let conflict = session
        .upload_release(
            &release_path,
            &remote_path,
            UploadOptions::default(),
            cancellation,
            |_| {},
        )
        .await;
    assert!(matches!(conflict, Err(UploadError::RemoteConflict(_))));

    let cancelled_path = RemotePath::parse("/tmp/shipforge-protocol-cancelled.tar.gz").unwrap();
    let upload_cancellation = CancellationToken::new();
    let cancel_after_progress = upload_cancellation.clone();
    let cancelled = session
        .upload_release(
            &release_path,
            &cancelled_path,
            UploadOptions::default(),
            &upload_cancellation,
            move |event| {
                if event.sent > 0 {
                    cancel_after_progress.cancel();
                }
            },
        )
        .await;
    assert!(matches!(cancelled, Err(UploadError::Cancelled)));

    let empty_path = directory.join("empty.tar.gz");
    std::fs::write(&empty_path, []).unwrap();
    let empty_remote = RemotePath::parse("/tmp/shipforge-protocol-empty.tar.gz").unwrap();
    let empty = session
        .upload_release(
            &empty_path,
            &empty_remote,
            UploadOptions::default(),
            cancellation,
            |_| {},
        )
        .await;
    assert!(matches!(empty, Err(UploadError::EmptyRelease(_))));

    let state = transfer.lock().await;
    assert_eq!(state.removals, 2);
    assert_eq!(state.files.get(remote_path.as_str()), Some(&release_bytes));
    assert!(!state.files.contains_key(cancelled_path.as_str()));
    assert!(!state.files.contains_key(empty_remote.as_str()));
}

async fn validate_release_prepare(
    session: &AuthenticatedSession,
    directory: &Path,
    cancellation: &CancellationToken,
    transfer: &Arc<AsyncMutex<TransferState>>,
    deployment: &DeploymentId,
) -> ActivatedRemoteRelease {
    let artifact_path = directory.join("server-binary");
    std::fs::write(&artifact_path, "executable").unwrap();
    let release = ComponentRelease {
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("api").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: ReleaseVersion::parse("v2").unwrap(),
        destination: DestinationKey::parse("dst_00000000000000000000000000000001").unwrap(),
        destination_revision: DestinationRevision::INITIAL,
    };
    let package = package_release(
        &ResolvedArtifact {
            path: artifact_path,
            kind: ResolvedArtifactKind::File,
        },
        &release,
        &directory.join("packages"),
        42,
        Some("abc1234".into()),
        cancellation,
    )
    .unwrap();
    let target = protocol_target();
    let prepared = session
        .prepare_release(
            &target,
            &package,
            deployment,
            PrepareReleaseOptions {
                command_timeout: Duration::from_secs(5),
                ..PrepareReleaseOptions::default()
            },
            cancellation,
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(prepared.release(), &release);
    assert_eq!(prepared.sha256(), package.sha256());
    assert_eq!(prepared.size(), package.size());

    let temporary = format!("{}/temporary/{deployment}.tar.gz", target.root);
    let archive = format!("{}/archives/v2.tar.gz", target.root);
    let expected = std::fs::read(package.path()).unwrap();
    {
        let state = transfer.lock().await;
        assert_eq!(state.files.get(&archive), Some(&expected));
        assert!(!state.files.contains_key(&temporary));
    }

    let activated = session
        .activate_release(
            &target,
            &prepared,
            None,
            deployment,
            ActivationOptions {
                command_timeout: Duration::from_secs(5),
            },
            cancellation,
        )
        .await
        .unwrap();
    assert_eq!(activated.release(), &release);
    assert_eq!(activated.previous(), None);
    assert_eq!(
        session
            .observe_current(
                &target,
                ActivationOptions {
                    command_timeout: Duration::from_secs(5),
                },
                cancellation,
            )
            .await
            .unwrap(),
        Some(ReleaseVersion::parse("v2").unwrap())
    );
    let state = transfer.lock().await;
    assert_eq!(
        state.links.get(&format!("{}/current", target.root)),
        Some(&"releases/v2".to_owned())
    );
    drop(state);
    validate_health(session, &target, cancellation).await;
    activated
}

fn protocol_target() -> LinuxSshTarget {
    LinuxSshTarget::validate(&DriverTargetInput {
        value: serde_json::json!({
            "root": "/srv/shipforge/protocol/production/api",
            "systemd": null,
            "health": "http://127.0.0.1:8080/health"
        }),
    })
    .unwrap()
}

async fn validate_health(
    session: &AuthenticatedSession,
    target: &LinuxSshTarget,
    cancellation: &CancellationToken,
) {
    let health = session
        .check_health(
            target,
            HealthCheckOptions {
                command_timeout: Duration::from_secs(5),
                interval: Duration::from_millis(1),
                attempts: 1,
                stable_for: Duration::from_millis(1),
            },
            cancellation,
        )
        .await
        .unwrap();
    assert_eq!(health.http.unwrap().status, 204);
}

async fn validate_marker_reads(
    session: &AuthenticatedSession,
    transfer: &Arc<AsyncMutex<TransferState>>,
    release: &ComponentRelease,
    cancellation: &CancellationToken,
) {
    use shipforge::drivers::linux_ssh::{DeploymentMarker, MarkerError};
    let mut target = protocol_target();
    target.root = "/srv/marker-contract".into();
    let path = format!("{}/.shipforge-project.json", target.root);
    let marker = DeploymentMarker::for_release(release);
    assert!(
        !session
            .check_deployment_marker(&target, &marker, cancellation)
            .await
            .unwrap()
    );
    session
        .ensure_deployment_marker(&target, &marker, cancellation)
        .await
        .unwrap();
    session
        .ensure_deployment_marker(&target, &marker, cancellation)
        .await
        .unwrap();
    assert!(
        session
            .check_deployment_marker(&target, &marker, cancellation)
            .await
            .unwrap()
    );
    let mut other = release.clone();
    other.project_id = ProjectId::new();
    assert_eq!(
        session
            .ensure_deployment_marker(
                &target,
                &DeploymentMarker::for_release(&other),
                cancellation
            )
            .await,
        Err(MarkerError::Conflict)
    );
    assert_eq!(
        transfer.lock().await.files.get(&path),
        Some(&marker.encode().unwrap())
    );
    assert_eq!(
        session
            .check_deployment_marker(
                &target,
                &DeploymentMarker::for_release(&other),
                cancellation
            )
            .await,
        Err(MarkerError::Conflict)
    );
    transfer
        .lock()
        .await
        .files
        .insert(path.clone(), vec![b' '; 5000]);
    assert_eq!(
        session
            .check_deployment_marker(&target, &marker, cancellation)
            .await,
        Err(MarkerError::Oversized)
    );
    transfer
        .lock()
        .await
        .files
        .insert(path.clone(), b"invalid".to_vec());
    assert_eq!(
        session
            .check_deployment_marker(&target, &marker, cancellation)
            .await,
        Err(MarkerError::Malformed)
    );
    transfer
        .lock()
        .await
        .links
        .insert(path.clone(), "somewhere".into());
    assert_eq!(
        session
            .check_deployment_marker(&target, &marker, cancellation)
            .await,
        Err(MarkerError::UnsafePath)
    );
    let mut state = transfer.lock().await;
    state.links.remove(&path);
    state.files.remove(&path);
    state
        .files
        .insert(format!("{}/user-data", target.root), b"preserve".to_vec());
    drop(state);
    assert_eq!(
        session
            .ensure_deployment_marker(&target, &marker, cancellation)
            .await,
        Err(MarkerError::UnmarkedNonempty)
    );
    assert!(!transfer.lock().await.files.contains_key(&path));
}

async fn validate_probe(session: &AuthenticatedSession, cancellation: &CancellationToken) {
    let candidates = probe_remote_setup(
        session,
        "/tmp/shipforge-protocol-contract",
        Duration::from_secs(5),
        cancellation,
    )
    .await
    .unwrap();
    assert_eq!(candidates.root, RemoteRootState::Missing);
    assert_eq!(
        candidates.systemd_units,
        vec!["api.service", "worker.service"]
    );
}

async fn validate_layout_and_manifest_safety(
    session: &AuthenticatedSession,
    transfer: &Arc<AsyncMutex<TransferState>>,
    release: &ComponentRelease,
    cancellation: &CancellationToken,
) {
    use shipforge::drivers::linux_ssh::{DeploymentMarker, MarkerError};
    let target = protocol_target();
    let expected = DeploymentMarker::for_release(release);
    let path = format!("{}/releases/{}/manifest.json", target.root, release.version);
    let original = transfer.lock().await.files.get(&path).unwrap().clone();
    session
        .check_release_manifest(&target, &expected, &release.version, cancellation)
        .await
        .unwrap();
    let mut foreign: serde_json::Value = serde_json::from_slice(&original).unwrap();
    foreign["projectId"] = serde_json::json!(ProjectId::new());
    for (bytes, error) in [
        (
            Some(serde_json::to_vec(&foreign).unwrap()),
            MarkerError::ManifestConflict,
        ),
        (Some(b"invalid".to_vec()), MarkerError::ManifestMalformed),
        (Some(vec![b' '; 9000]), MarkerError::ManifestMalformed),
        (None, MarkerError::ManifestMalformed),
    ] {
        if let Some(bytes) = bytes {
            transfer.lock().await.files.insert(path.clone(), bytes);
        } else {
            transfer.lock().await.files.remove(&path);
        }
        assert_eq!(
            session
                .check_release_manifest(&target, &expected, &release.version, cancellation)
                .await,
            Err(error)
        );
    }
    transfer.lock().await.files.insert(path.clone(), original);
    for linked in [
        format!("{}/temporary", target.root),
        format!("{}/archives", target.root),
        format!("{}/releases", target.root),
        target.root.clone(),
        "/srv/shipforge".into(),
    ] {
        transfer
            .lock()
            .await
            .links
            .insert(linked.clone(), "/srv/external".into());
        assert_eq!(
            session.check_release_layout(&target, cancellation).await,
            Err(MarkerError::UnsafePath)
        );
        assert!(
            session
                .observe_current(&target, ActivationOptions::default(), cancellation)
                .await
                .is_err()
        );
        assert_eq!(
            session
                .check_release_manifest(&target, &expected, &release.version, cancellation)
                .await,
            Err(MarkerError::UnsafePath)
        );
        transfer.lock().await.links.remove(&linked);
    }
    transfer
        .lock()
        .await
        .links
        .insert(path.clone(), "/srv/external/manifest.json".into());
    assert_eq!(
        session
            .check_release_manifest(&target, &expected, &release.version, cancellation)
            .await,
        Err(MarkerError::ManifestMalformed)
    );
    transfer.lock().await.links.remove(&path);
}

async fn validate_prepare_drift(
    planned: &shipforge::application::PlannedComponent,
    package: &shipforge::drivers::ReleasePackage,
    transfer: &Arc<AsyncMutex<TransferState>>,
) {
    let root = protocol_target().root;
    let path = format!("{root}/current");
    let previous = transfer.lock().await.links.get(&path).cloned();
    let mut external = package.release().clone();
    external.version = ReleaseVersion::parse("external-change").unwrap();
    let snapshot = {
        let mut state = transfer.lock().await;
        state
            .links
            .insert(path.clone(), "releases/external-change".into());
        state
            .directories
            .insert(format!("{root}/releases/external-change"));
        state.files.insert(
            format!("{root}/releases/external-change/manifest.json"),
            serde_json::to_vec(&shipforge::domain::ReleaseManifest::new(&external, 1, None))
                .unwrap(),
        );
        (
            state.files.clone(),
            state.directories.clone(),
            state.links.clone(),
        )
    };
    let result = planned
        .driver
        .prepare(
            &DeploymentId::new(),
            &planned.context,
            &planned.plan,
            package,
            &IgnoreEvents,
        )
        .await
        .unwrap_err();
    assert!(result.message.contains("current changed"));
    let mut state = transfer.lock().await;
    assert_eq!(
        state.files, snapshot.0,
        "drift must not create a marker, temporary file, or archive"
    );
    assert_eq!(state.directories, snapshot.1);
    assert_eq!(state.links, snapshot.2);
    if let Some(previous) = previous {
        state.links.insert(path, previous);
    } else {
        state.links.remove(&path);
    }
}

#[allow(clippy::too_many_lines)]
async fn validate_production_driver(
    address: std::net::SocketAddr,
    host_fingerprint: &str,
    directory: &Path,
    base_release: &ComponentRelease,
    cancellation: &CancellationToken,
    transfer: &Arc<AsyncMutex<TransferState>>,
) {
    let identity = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let identity_path = directory.join("driver_id_ed25519");
    std::fs::write(
        &identity_path,
        identity.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let mut credentials = CredentialRegistry::new();
    let credential = credentials
        .create(SshCredential::IdentityFile {
            path: identity_path,
        })
        .unwrap();
    let driver = Arc::new(LinuxSshDriver::new(Arc::new(credentials)));
    let destination_settings = driver
        .validate_destination(&DriverDestinationInput {
            value: serde_json::json!({
                "host": "127.0.0.1",
                "port": address.port(),
                "user": "deploy",
                "hostKey": host_fingerprint,
            }),
        })
        .unwrap();
    let target = driver
        .validate_target(&DriverTargetInput {
            value: serde_json::json!({
                "root": "/srv/shipforge/protocol/production/api",
                "systemd": null,
                "health": "http://127.0.0.1:8080/health"
            }),
        })
        .unwrap();
    let context = ComponentExecutionContext {
        project_id: base_release.project_id.clone(),
        environment_id: base_release.environment_id.clone(),
        component: base_release.component.clone(),
        generation: base_release.generation,
        destination: base_release.destination.clone(),
        destination_revision: base_release.destination_revision,
        credential,
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        destination_settings,
        target,
        cancellation: cancellation.clone(),
    };
    let release = ComponentRelease {
        project_id: context.project_id.clone(),
        environment_id: context.environment_id.clone(),
        component: context.component.clone(),
        generation: context.generation,
        version: ReleaseVersion::parse("v3").unwrap(),
        destination: context.destination.clone(),
        destination_revision: context.destination_revision,
    };
    let mut registry = DriverRegistry::default();
    registry.register(driver).unwrap();
    let deployment_planner = DeploymentPlanner::new(Arc::new(registry));
    let planned = deployment_planner
        .plan_component(
            context,
            ComponentRequest {
                release: release.clone(),
                required_capabilities: BTreeSet::from([
                    Capability::StagedDeployment,
                    Capability::ExplicitActivation,
                    Capability::Observe,
                    Capability::Rollback,
                    Capability::Cancellation,
                ]),
            },
        )
        .await
        .unwrap();
    let artifact = directory.join("driver-server-binary");
    std::fs::write(&artifact, "production adapter").unwrap();
    let package = package_release(
        &ResolvedArtifact {
            path: artifact,
            kind: ResolvedArtifactKind::File,
        },
        &release,
        &directory.join("driver-packages"),
        43,
        Some("def5678".into()),
        cancellation,
    )
    .unwrap();
    let deployment = DeploymentId::new();
    validate_prepare_drift(&planned, &package, transfer).await;
    let prepared = planned
        .driver
        .prepare(
            &deployment,
            &planned.context,
            &planned.plan,
            &package,
            &IgnoreEvents,
        )
        .await
        .unwrap();
    let activated = planned
        .driver
        .activate(&deployment, &planned.context, &prepared.release)
        .await
        .unwrap();
    assert_eq!(activated.current.as_ref(), Some(&prepared.release));
    assert!(activated.healthy);

    let previous = ReleaseRef {
        driver: prepared.release.driver.clone(),
        project_id: prepared.release.project_id.clone(),
        environment_id: prepared.release.environment_id.clone(),
        component: prepared.release.component.clone(),
        generation: prepared.release.generation,
        version: ReleaseVersion::parse("v2").unwrap(),
        destination: prepared.release.destination.clone(),
        destination_revision: prepared.release.destination_revision,
        endpoint_fingerprint: prepared.release.endpoint_fingerprint.clone(),
        effective_capabilities: DriverCapabilities::new([
            Capability::StagedDeployment,
            Capability::ExplicitActivation,
            Capability::Observe,
            Capability::Rollback,
            Capability::Cancellation,
        ]),
    };
    let drift = planned
        .driver
        .rollback(
            &DeploymentId::new(),
            &planned.context,
            Some(&previous),
            None,
        )
        .await
        .unwrap_err();
    assert!(drift.message.contains("current changed"));
    assert_eq!(
        planned.driver.current(&planned.context).await.unwrap(),
        Some(prepared.release.clone())
    );
    let rolled_back = planned
        .driver
        .rollback(
            &DeploymentId::new(),
            &planned.context,
            Some(&prepared.release),
            Some(&previous),
        )
        .await
        .unwrap();
    assert_eq!(rolled_back.current, Some(previous.clone()));
    assert!(rolled_back.healthy);
    let undeployed = planned
        .driver
        .rollback(
            &DeploymentId::new(),
            &planned.context,
            Some(&previous),
            None,
        )
        .await
        .unwrap();
    assert_eq!(undeployed.current, None);
    assert_eq!(
        planned.driver.current(&planned.context).await.unwrap(),
        None
    );
    let restored = planned
        .driver
        .rollback(
            &DeploymentId::new(),
            &planned.context,
            None,
            Some(&previous),
        )
        .await
        .unwrap();
    assert_eq!(restored.current, Some(previous.clone()));
    assert!(restored.healthy);
    assert_eq!(
        planned.driver.current(&planned.context).await.unwrap(),
        Some(previous)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn validates_real_ssh_transfer_prepare_activate_hash_and_probe() {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let host_fingerprint = host_key
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let config = Arc::new(server::Config {
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![host_key],
        ..server::Config::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let transfer = Arc::new(AsyncMutex::new(TransferState {
        fail_writes_remaining: 1,
        health_status: 204,
        ..TransferState::default()
    }));
    let mut server = ProtocolServer {
        commands: commands.clone(),
        channels: Arc::new(AsyncMutex::new(HashMap::new())),
        transfer: transfer.clone(),
    };
    let running = server.run_on_socket(config, &listener);
    let shutdown = running.handle();
    let client = async {
        let directory = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let session =
            connect_client(address, &host_fingerprint, directory.path(), &cancellation).await;
        validate_exec(&session, &cancellation).await;
        validate_transfer(&session, directory.path(), &cancellation, &transfer).await;
        let deployment = DeploymentId::new();
        let activation = validate_release_prepare(
            &session,
            directory.path(),
            &cancellation,
            &transfer,
            &deployment,
        )
        .await;
        transfer.lock().await.health_status = 503;
        let health_failure = session
            .verify_activation_health(
                &protocol_target(),
                &activation,
                &deployment,
                HealthCheckOptions {
                    command_timeout: Duration::from_secs(5),
                    interval: Duration::from_millis(1),
                    attempts: 1,
                    stable_for: Duration::from_millis(1),
                },
                ActivationOptions {
                    command_timeout: Duration::from_secs(5),
                },
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            health_failure,
            HealthVerificationError::FailedAndCompensated { .. }
        ));
        assert_eq!(
            session
                .observe_current(
                    &protocol_target(),
                    ActivationOptions {
                        command_timeout: Duration::from_secs(5),
                    },
                    &cancellation,
                )
                .await
                .unwrap(),
            None
        );
        validate_probe(&session, &cancellation).await;
        transfer.lock().await.health_status = 204;
        // The earlier low-level activation fixture represents an existing deployment.
        transfer.lock().await.files.insert(
            format!("{}/.shipforge-project.json", protocol_target().root),
            shipforge::drivers::linux_ssh::DeploymentMarker::for_release(activation.release())
                .encode()
                .unwrap(),
        );
        validate_production_driver(
            address,
            &host_fingerprint,
            directory.path(),
            activation.release(),
            &cancellation,
            &transfer,
        )
        .await;
        validate_marker_reads(&session, &transfer, activation.release(), &cancellation).await;
        validate_layout_and_manifest_safety(
            &session,
            &transfer,
            activation.release(),
            &cancellation,
        )
        .await;
        session.disconnect().await.unwrap();

        deployment_service::validate(
            address,
            &host_fingerprint,
            directory.path(),
            &transfer,
            &commands,
        )
        .await;

        let commands = commands.lock().unwrap().clone();
        assert_eq!(
            commands[0],
            "'printf' 'literal;$(not-executed) '\\'' value'"
        );
        assert!(commands.iter().any(|command| {
            command.starts_with("'mv' '--no-target-directory' '--'")
                && command.ends_with("'/srv/shipforge/protocol/production/api/current'")
        }));
        assert!(commands.iter().any(|command| {
            command == "'readlink' '--' '/srv/shipforge/protocol/production/api/current'"
        }));
        assert!(commands.iter().any(|command| {
            command.starts_with("'curl' '--silent'")
                && command.ends_with("'http://127.0.0.1:8080/health'")
        }));
        shutdown.shutdown("test complete".into());
    };
    let (server_result, ()) = tokio::join!(running, client);
    server_result.unwrap();
}
