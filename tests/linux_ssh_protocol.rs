use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read as _;
use std::num::NonZeroU8;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
        DestinationKey, DestinationRevision, EnvironmentId, ProjectId, ReleaseVersion,
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
#[cfg(windows)]
#[path = "support/password_protocol.rs"]
mod password_protocol;
#[path = "support/preflight_protocol.rs"]
mod preflight_protocol;
#[path = "support/remote_directory_protocol.rs"]
mod remote_directory_protocol;

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
    sudo_channels: HashMap<ChannelId, Vec<u8>>,
}

#[derive(Debug, Default)]
struct TransferState {
    password_attempts: usize,
    sudo_responses: usize,
    files: HashMap<String, Vec<u8>>,
    directories: HashSet<String>,
    links: HashMap<String, String>,
    hard_link_counts: HashMap<String, u64>,
    fail_writes_remaining: usize,
    fail_audit_appends_remaining: usize,
    removals: usize,
    health_status: u16,
}

impl server::Server for ProtocolServer {
    type Handler = Self;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self {
        Self {
            commands: Arc::clone(&self.commands),
            // Channel identifiers are local to an SSH connection.
            channels: Arc::new(AsyncMutex::new(HashMap::new())),
            transfer: Arc::clone(&self.transfer),
            sudo_channels: HashMap::new(),
        }
    }
}

#[tokio::test]
async fn protocol_handlers_isolate_channels_and_share_fixture_facts() {
    let mut server = ProtocolServer::default();
    let first = server.new_client(None);
    let second = server.new_client(None);
    assert!(!Arc::ptr_eq(&server.channels, &first.channels));
    assert!(!Arc::ptr_eq(&server.channels, &second.channels));
    assert!(!Arc::ptr_eq(&first.channels, &second.channels));
    for handler in [&first, &second] {
        assert!(Arc::ptr_eq(&server.commands, &handler.commands));
        assert!(Arc::ptr_eq(&server.transfer, &handler.transfer));
    }
    let first_channels = first.channels.lock().await;
    assert!(server.channels.try_lock().is_ok());
    assert!(second.channels.try_lock().is_ok());
    drop(first_channels);

    first
        .commands
        .lock()
        .unwrap()
        .push("fixture command".into());
    first
        .transfer
        .lock()
        .await
        .files
        .insert("/fixture/shared".into(), b"shared".to_vec());
    for handler in [&server, &second] {
        assert_eq!(
            *handler.commands.lock().unwrap(),
            vec!["fixture command".to_owned()]
        );
        assert_eq!(
            handler
                .transfer
                .lock()
                .await
                .files
                .get("/fixture/shared")
                .map(Vec::as_slice),
            Some(&b"shared"[..])
        );
    }
}

impl server::Handler for ProtocolServer {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.transfer.lock().await.password_attempts += 1;
        if user == "slow" {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Ok(
            if user == "deploy" && password == "fixture 密码 q$' value" {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }

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
        if command.contains("'[shipforge-sudo-password]'") {
            session.channel_success(channel)?;
            if command.contains("'cached'") {
                session.exit_status_request(channel, 0)?;
                session.close(channel)?;
            } else if !command.contains("'stall'") {
                self.sudo_channels.insert(
                    channel,
                    if command.contains("'denied'") {
                        b"rejected:".to_vec()
                    } else {
                        Vec::new()
                    },
                );
                session.extended_data(channel, 1, b"[shipforge-".to_vec())?;
                session.extended_data(channel, 1, b"sudo-password]".to_vec())?;
            }
            return Ok(());
        }
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

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(input) = self.sudo_channels.get_mut(&channel) {
            input.extend_from_slice(data);
            if input.ends_with(b"\n") {
                self.transfer.lock().await.sudo_responses += 1;
                let valid = input == "fixture 密码 q$' value\n".as_bytes();
                // Deliberate hostile echo: the caller must never return or log it.
                session.data(channel, input.clone())?;
                session.extended_data(channel, 1, input.clone())?;
                session.exit_status_request(channel, u32::from(!valid))?;
                session.eof(channel)?;
                session.close(channel)?;
                self.sudo_channels.remove(&channel);
            }
        } else {
            self.transfer.lock().await.sudo_responses += 1;
        }
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
    let arguments = structured_arguments(command)?;
    let words = arguments.iter().map(String::as_str).collect::<Vec<_>>();
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
        "find" => find_command(&words, transfer).await,
        "timeout" => match words.as_slice() {
            ["timeout", "--kill-after=2", "25", inner @ ..] => audit_command(inner, transfer).await,
            _ => inventory_command(&words, transfer).await,
        },
        "ln" => link_command(&words, transfer).await,
        "rm" => {
            let path = *words.last()?;
            let mut state = transfer.lock().await;
            state.files.remove(path);
            state.links.remove(path);
            Some((0, Vec::new()))
        }
        "stat" => match words.as_slice() {
            ["stat", "--format=%h", "--", path] => {
                let state = transfer.lock().await;
                if !state.files.contains_key(*path) {
                    return Some((1, Vec::new()));
                }
                let count = state.hard_link_counts.get(*path).copied().unwrap_or(1);
                Some((0, format!("{count}\n").into_bytes()))
            }
            ["stat", "--dereference", "--format=%d", "--", _, _] => Some((0, b"1\n1\n".to_vec())),
            ["stat", "--format=%d:%i:%s:%y:%z", "--", path] => {
                let state = transfer.lock().await;
                let bytes = state.files.get(*path)?;
                // Content-derived stamp detects fixture mutations without claiming real inode races.
                let identity = format!("{:x}", sha2::Sha256::digest(bytes));
                Some((
                    0,
                    format!("1:1:{}:{identity}:0\n", bytes.len()).into_bytes(),
                ))
            }
            _ => None,
        },
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

// Decode only CommandSpec's single-quoted argv, never a general Shell program.
fn structured_arguments(mut command: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    while !command.is_empty() {
        command = command.strip_prefix('\'')?;
        let mut argument = String::new();
        loop {
            let end = command.find('\'')?;
            argument.push_str(&command[..end]);
            command = &command[end + 1..];
            if let Some(rest) = command.strip_prefix("\\''") {
                argument.push('\'');
                command = rest;
            } else {
                break;
            }
        }
        arguments.push(argument);
        if !command.is_empty() {
            command = command.strip_prefix(' ')?;
            if command.is_empty() {
                return None;
            }
        }
    }
    Some(arguments)
}

async fn find_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let [
        "find",
        root,
        "-mindepth",
        "1",
        "-maxdepth",
        "1",
        mode,
        format,
    ] = words
    else {
        return None;
    };
    if !matches!(
        (*mode, *format),
        ("-print", "-quit") | ("-printf", "%f\\0%y\\0%s\\0")
    ) {
        return None;
    }
    let state = transfer.lock().await;
    if !state.directories.contains(*root) {
        return Some((1, Vec::new()));
    }
    let prefix = format!("{root}/");
    let mut children = std::collections::BTreeMap::new();
    for (path, kind, size) in state
        .files
        .iter()
        .map(|(path, bytes)| (path, 'f', bytes.len()))
        .chain(
            state
                .links
                .iter()
                .map(|(path, target)| (path, 'l', target.len())),
        )
        .chain(state.directories.iter().map(|path| (path, 'd', 0)))
    {
        if let Some(name) = path
            .strip_prefix(&prefix)
            .filter(|name| !name.is_empty() && !name.contains('/'))
        {
            children.insert(name, (kind, size));
        }
    }
    if *mode == "-print" {
        return Some((
            0,
            children
                .first_key_value()
                .map_or_else(Vec::new, |(name, _)| {
                    format!("{prefix}{name}\n").into_bytes()
                }),
        ));
    }
    let mut output = Vec::new();
    for (name, (kind, size)) in children {
        output.extend_from_slice(format!("{name}\0{kind}\0{size}\0").as_bytes());
    }
    Some((0, output))
}

async fn inventory_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let [
        "timeout",
        "--signal=TERM",
        "--kill-after=2s",
        "15s",
        inner @ ..,
    ] = words
    else {
        return None;
    };
    if inner.first() == Some(&"find") {
        return find_command(inner, transfer).await;
    }
    let path = match inner {
        ["sha256sum", "--", path]
        | ["gzip", "--test", "--", path]
        | [
            "sh",
            "-c",
            "gzip -cd -- \"$1\" | head -c 9216",
            "shipforge-inventory",
            path,
        ] => *path,
        _ => return None,
    };
    let state = transfer.lock().await;
    let Some(bytes) = state.files.get(path) else {
        return Some((1, Vec::new()));
    };
    if inner[0] == "sha256sum" {
        return Some((
            0,
            format!("{:x}  {path}\n", sha2::Sha256::digest(bytes)).into_bytes(),
        ));
    }
    let limit = if inner[0] == "sh" {
        9216
    } else {
        16 * 1024 * 1024
    };
    let mut decoded = Vec::new();
    let result = flate2::read::MultiGzDecoder::new(bytes.as_slice())
        .take(limit + 1)
        .read_to_end(&mut decoded);
    if result.is_err() || (inner[0] == "gzip" && decoded.len() as u64 > limit) {
        return Some((1, Vec::new()));
    }
    decoded.truncate(usize::try_from(limit).ok()?);
    Some((
        0,
        if inner[0] == "sh" {
            decoded
        } else {
            Vec::new()
        },
    ))
}

fn expected_audit_script() -> String {
    // Equality pins the simulated command to the production script, not arbitrary `sh`.
    let source = include_str!("../src/drivers/linux_ssh/audit.rs").replace("\r\n", "\n");
    source
        .split_once("const AUDIT_SCRIPT: &str = r#\"")
        .unwrap()
        .1
        .split_once("\"#;")
        .unwrap()
        .0
        .to_owned()
}

async fn audit_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let [
        "sh",
        "-c",
        script,
        "shipforge-audit",
        root,
        file,
        marker,
        mode,
        payload,
    ] = words
    else {
        return None;
    };
    if *script != expected_audit_script() {
        return None;
    }
    if !matches!(*file, "releases.jsonl" | "deployments.jsonl")
        || !matches!(*mode, "read" | "append")
    {
        return Some((42, Vec::new()));
    }
    let mut state = transfer.lock().await;
    let mut ancestor = *root;
    while ancestor != "/" {
        if state.links.contains_key(ancestor) || !state.directories.contains(ancestor) {
            return Some((42, Vec::new()));
        }
        ancestor =
            ancestor.rsplit_once('/').map_or(
                "/",
                |(parent, _)| if parent.is_empty() { "/" } else { parent },
            );
    }
    let marker_path = format!("{root}/.shipforge-project.json");
    if state.links.contains_key(&marker_path)
        || state
            .hard_link_counts
            .get(&marker_path)
            .copied()
            .unwrap_or(1)
            != 1
        || state
            .files
            .get(&marker_path)
            .is_none_or(|bytes| bytes.as_slice() != marker.as_bytes())
    {
        return Some((43, Vec::new()));
    }
    let metadata = format!("{root}/metadata");
    if state.links.contains_key(&metadata) || state.files.contains_key(&metadata) {
        return Some((42, Vec::new()));
    }
    if !state.directories.contains(&metadata) {
        if *mode == "read" {
            return Some((44, Vec::new()));
        }
        state.directories.insert(metadata.clone());
    }
    let path = format!("{metadata}/{file}");
    if state.links.contains_key(&path) || state.directories.contains(&path) {
        return Some((42, Vec::new()));
    }
    if state.hard_link_counts.get(&path).copied().unwrap_or(1) != 1 {
        return Some((42, Vec::new()));
    }
    if *mode == "read" {
        return Some(state.files.get(&path).map_or((44, Vec::new()), |bytes| {
            (0, bytes.iter().take(49_153).copied().collect())
        }));
    }
    let record: shipforge::drivers::audit::RemoteAuditRecord =
        serde_json::from_str(payload).ok()?;
    if !record.is_valid() || payload.len() > 8192 {
        return Some((42, Vec::new()));
    }
    if state.fail_audit_appends_remaining > 0 {
        state.fail_audit_appends_remaining -= 1;
        return Some((1, Vec::new()));
    }
    let bytes = state.files.entry(path).or_default();
    bytes.extend_from_slice(format!("\n{payload}\n").as_bytes());
    Some((0, Vec::new()))
}

async fn mkdir_command(
    words: &[&str],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    let mut state = transfer.lock().await;
    for path in words.iter().skip_while(|word| **word != "--").skip(1) {
        state.directories.insert((*path).to_owned());
        if words.contains(&"--parents") || words.contains(&"-p") {
            let mut parent = *path;
            while let Some((ancestor, _)) = parent.rsplit_once('/') {
                if ancestor.is_empty() {
                    break;
                }
                state.directories.insert(ancestor.to_owned());
                parent = ancestor;
            }
        }
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
        "-w" => state.files.contains_key(path) || state.directories.contains(path),
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
    handles: HashMap<String, MemoryFileHandle>,
    next_handle: u64,
}

#[derive(Clone)]
struct MemoryFileHandle {
    path: String,
    flags: russh_sftp::protocol::OpenFlags,
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
        if !flags.intersects(OpenFlags::READ | OpenFlags::WRITE | OpenFlags::APPEND)
            || (flags.contains(OpenFlags::EXCLUDE) && !flags.contains(OpenFlags::CREATE))
            || (flags.contains(OpenFlags::TRUNCATE)
                && !flags.intersects(OpenFlags::WRITE | OpenFlags::APPEND))
            || transfer.directories.contains(&filename)
            || transfer.links.contains_key(&filename)
        {
            return Err(StatusCode::Failure);
        }
        if transfer.files.contains_key(&filename) && flags.contains(OpenFlags::EXCLUDE) {
            return Err(StatusCode::Failure);
        }
        if !transfer.files.contains_key(&filename) && !flags.contains(OpenFlags::CREATE) {
            return Err(StatusCode::NoSuchFile);
        }
        let file = transfer.files.entry(filename.clone()).or_default();
        if flags.contains(OpenFlags::TRUNCATE) {
            file.clear();
        }
        self.next_handle += 1;
        let handle = format!("handle-{}", self.next_handle);
        self.handles.insert(
            handle.clone(),
            MemoryFileHandle {
                path: filename,
                flags,
            },
        );
        Ok(russh_sftp::protocol::Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<russh_sftp::protocol::Data, Self::Error> {
        use russh_sftp::protocol::{OpenFlags, StatusCode};
        let opened = self.handles.get(&handle).ok_or(StatusCode::Failure)?;
        if !opened.flags.contains(OpenFlags::READ) {
            return Err(StatusCode::PermissionDenied);
        }
        let transfer = self.transfer.lock().await;
        let bytes = transfer
            .files
            .get(&opened.path)
            .ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::Eof)?;
        if offset >= bytes.len() && len != 0 {
            return Err(StatusCode::Eof);
        }
        let data = bytes
            .get(offset..)
            .unwrap_or_default()
            .iter()
            .take(len as usize)
            .copied()
            .collect();
        Ok(russh_sftp::protocol::Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        use russh_sftp::protocol::{OpenFlags, StatusCode};
        let opened = self
            .handles
            .get(&handle)
            .ok_or(StatusCode::Failure)?
            .clone();
        if !opened
            .flags
            .intersects(OpenFlags::WRITE | OpenFlags::APPEND)
        {
            return Err(StatusCode::PermissionDenied);
        }
        let mut transfer = self.transfer.lock().await;
        let file = transfer
            .files
            .get_mut(&opened.path)
            .ok_or(StatusCode::NoSuchFile)?;
        let offset = if opened.flags.contains(OpenFlags::APPEND) {
            file.len()
        } else {
            usize::try_from(offset).map_err(|_| StatusCode::Failure)?
        };
        let end = offset.checked_add(data.len()).ok_or(StatusCode::Failure)?;
        // This deliberately bounded fixture does not allocate arbitrary sparse files.
        if end > 16 * 1024 * 1024 {
            return Err(StatusCode::Failure);
        }
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
        self.handles.remove(&handle).ok_or(Self::Error::Failure)?;
        Ok(ok_status(id))
    }

    async fn stat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        let transfer = self.transfer.lock().await;
        memory_attrs(id, &path, &transfer, true)
    }

    async fn lstat(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        memory_attrs(id, &path, &*self.transfer.lock().await, false)
    }

    async fn fstat(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
        let opened = self.handles.get(&handle).ok_or(Self::Error::Failure)?;
        memory_attrs(id, &opened.path, &*self.transfer.lock().await, false)
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

fn memory_attrs(
    id: u32,
    path: &str,
    transfer: &TransferState,
    follow: bool,
) -> Result<russh_sftp::protocol::Attrs, russh_sftp::protocol::StatusCode> {
    use russh_sftp::protocol::{Attrs, FileAttributes, StatusCode};
    let mut path = path.to_owned();
    for _ in 0..16 {
        let (size, permissions) = if let Some(bytes) = transfer.files.get(&path) {
            (bytes.len(), 0o100_600)
        } else if transfer.directories.contains(&path) {
            (0, 0o040_700)
        } else if let Some(target) = transfer.links.get(&path) {
            if follow {
                path = if target.starts_with('/') {
                    target.clone()
                } else {
                    format!(
                        "{}/{target}",
                        path.rsplit_once('/').map_or("", |(parent, _)| parent)
                    )
                };
                continue;
            }
            (target.len(), 0o120_777)
        } else {
            return Err(StatusCode::NoSuchFile);
        };
        return Ok(Attrs {
            id,
            attrs: FileAttributes {
                size: Some(size as u64),
                permissions: Some(permissions),
                ..FileAttributes::default()
            },
        });
    }
    Err(StatusCode::Failure)
}

fn ok_status(id: u32) -> russh_sftp::protocol::Status {
    russh_sftp::protocol::Status {
        id,
        status_code: russh_sftp::protocol::StatusCode::Ok,
        error_message: "ok".into(),
        language_tag: "en".into(),
    }
}

#[test]
fn protocol_parser_accepts_only_structured_quoted_arguments() {
    let values = [SENTINEL, "", "\n$HOME && rm no", "尾部'", "''"];
    let command = CommandSpec::structured("printf", values.map(CommandArgument::plain)).unwrap();
    let arguments = structured_arguments(&command.render_posix().unwrap()).unwrap();
    assert_eq!(arguments[0], "printf");
    assert_eq!(arguments[1..], values);
    for invalid in [
        "printf hello",
        "'printf'; 'extra'",
        "'printf'  'extra'",
        "'unterminated",
        "'printf' ",
    ] {
        assert!(
            structured_arguments(invalid).is_none(),
            "unexpectedly accepted {invalid}"
        );
    }
}

#[tokio::test]
async fn memory_sftp_preserves_read_and_append_contents_and_enforces_access() {
    use russh_sftp::{
        protocol::{FileAttributes, OpenFlags, StatusCode},
        server::Handler as _,
    };
    let transfer = Arc::new(AsyncMutex::new(TransferState::default()));
    transfer
        .lock()
        .await
        .files
        .insert("/file".into(), b"original".to_vec());
    let mut sftp = MemorySftp::new(transfer.clone());
    let reader = sftp
        .open(
            1,
            "/file".into(),
            OpenFlags::READ,
            FileAttributes::default(),
        )
        .await
        .unwrap()
        .handle;
    assert_eq!(
        sftp.read(2, reader.clone(), 2, u32::MAX)
            .await
            .unwrap()
            .data,
        b"iginal"
    );
    assert_eq!(
        sftp.read(3, reader.clone(), u64::MAX, 1).await.unwrap_err(),
        StatusCode::Eof
    );
    assert_eq!(
        sftp.write(4, reader.clone(), 0, b"overwrite".to_vec())
            .await
            .unwrap_err(),
        StatusCode::PermissionDenied
    );
    let appender = sftp
        .open(
            5,
            "/file".into(),
            OpenFlags::WRITE | OpenFlags::APPEND,
            FileAttributes::default(),
        )
        .await
        .unwrap()
        .handle;
    sftp.write(6, appender.clone(), 0, b"-added".to_vec())
        .await
        .unwrap();
    assert_eq!(transfer.lock().await.files["/file"], b"original-added");
    assert_eq!(
        sftp.read(7, appender.clone(), 0, 1).await.unwrap_err(),
        StatusCode::PermissionDenied
    );
    assert_eq!(
        sftp.fstat(8, reader.clone()).await.unwrap().attrs.size,
        Some(14)
    );
    sftp.close(9, reader.clone()).await.unwrap();
    assert_eq!(
        sftp.fstat(10, reader).await.unwrap_err(),
        StatusCode::Failure
    );
    let writer = sftp
        .open(
            11,
            "/file".into(),
            OpenFlags::WRITE,
            FileAttributes::default(),
        )
        .await
        .unwrap()
        .handle;
    sftp.write(12, writer.clone(), 0, b"O".to_vec())
        .await
        .unwrap();
    assert_eq!(transfer.lock().await.files["/file"], b"Original-added");
    assert_eq!(
        sftp.write(13, writer, u64::MAX, vec![1]).await.unwrap_err(),
        StatusCode::Failure
    );
    assert_eq!(transfer.lock().await.files["/file"], b"Original-added");
}

#[tokio::test]
async fn memory_sftp_honors_exclusive_create_and_explicit_truncate() {
    use russh_sftp::{
        protocol::{FileAttributes, OpenFlags, StatusCode},
        server::Handler as _,
    };
    let transfer = Arc::new(AsyncMutex::new(TransferState::default()));
    let mut sftp = MemorySftp::new(transfer.clone());
    assert_eq!(
        sftp.open(
            1,
            "/missing".into(),
            OpenFlags::READ,
            FileAttributes::default()
        )
        .await
        .unwrap_err(),
        StatusCode::NoSuchFile
    );
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE;
    let created = sftp
        .open(2, "/file".into(), flags, FileAttributes::default())
        .await
        .unwrap()
        .handle;
    sftp.write(3, created, 0, b"keep".to_vec()).await.unwrap();
    assert_eq!(
        sftp.open(4, "/file".into(), flags, FileAttributes::default())
            .await
            .unwrap_err(),
        StatusCode::Failure
    );
    assert_eq!(transfer.lock().await.files["/file"], b"keep");
    assert_eq!(
        sftp.open(
            5,
            "/file".into(),
            OpenFlags::READ | OpenFlags::TRUNCATE,
            FileAttributes::default()
        )
        .await
        .unwrap_err(),
        StatusCode::Failure
    );
    sftp.open(
        6,
        "/file".into(),
        OpenFlags::WRITE | OpenFlags::TRUNCATE,
        FileAttributes::default(),
    )
    .await
    .unwrap();
    assert!(transfer.lock().await.files["/file"].is_empty());
}

#[tokio::test]
async fn memory_sftp_lstat_distinguishes_links_and_directories() {
    use russh_sftp::{
        protocol::{FileAttributes, OpenFlags, StatusCode},
        server::Handler as _,
    };
    let transfer = Arc::new(AsyncMutex::new(TransferState::default()));
    {
        let mut state = transfer.lock().await;
        state.directories.insert("/dir".into());
        state.files.insert("/dir/file".into(), vec![1, 2, 3]);
        state.links.insert("/dir/link".into(), "file".into());
        state.links.insert("/dir/loop".into(), "loop".into());
    }
    let mut sftp = MemorySftp::new(transfer);
    assert_eq!(
        sftp.lstat(1, "/dir".into())
            .await
            .unwrap()
            .attrs
            .permissions,
        Some(0o040_700)
    );
    assert_eq!(
        sftp.lstat(2, "/dir/link".into())
            .await
            .unwrap()
            .attrs
            .permissions,
        Some(0o120_777)
    );
    assert_eq!(
        sftp.stat(3, "/dir/link".into()).await.unwrap().attrs.size,
        Some(3)
    );
    assert_eq!(
        sftp.stat(4, "/dir/loop".into()).await.unwrap_err(),
        StatusCode::Failure
    );
    assert_eq!(
        sftp.open(
            5,
            "/dir/link".into(),
            OpenFlags::READ,
            FileAttributes::default()
        )
        .await
        .unwrap_err(),
        StatusCode::Failure
    );
}

#[tokio::test]
async fn protocol_simulator_rejects_unrecognized_scripts_and_timeout_options() {
    let transfer = Arc::new(AsyncMutex::new(TransferState::default()));
    for arguments in [
        vec![
            "--kill-after=2",
            "25",
            "sh",
            "-c",
            "exit 0",
            "shipforge-audit",
            "/root",
            "releases.jsonl",
            "{}",
            "read",
            "",
        ],
        vec![
            "--signal=TERM",
            "--kill-after=2s",
            "15s",
            "sh",
            "-c",
            "printf safe",
            "shipforge-inventory",
            "/archive",
        ],
        vec![
            "--signal=KILL",
            "--kill-after=2s",
            "15s",
            "sha256sum",
            "--",
            "/archive",
        ],
    ] {
        let command =
            CommandSpec::structured("timeout", arguments.into_iter().map(CommandArgument::plain))
                .unwrap();
        assert!(
            release_command(&command.render_posix().unwrap(), &transfer)
                .await
                .is_none()
        );
    }
    assert!(transfer.lock().await.files.is_empty());
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
            "service": null,
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
    commands: &Arc<Mutex<Vec<String>>>,
) {
    let started = Instant::now();
    let phase = |stage| report_protocol_phase(started, commands, stage);
    phase("driver.start");
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
                "service": null,
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
    phase("driver.plan.done");
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
    phase("driver.prepare-drift.before");
    validate_prepare_drift(&planned, &package, transfer).await;
    phase("driver.prepare-drift.done");
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
    phase("driver.prepare.done");
    let activated = planned
        .driver
        .activate(&deployment, &planned.context, &prepared.release)
        .await
        .unwrap();
    assert_eq!(activated.current.as_ref(), Some(&prepared.release));
    assert!(activated.healthy);
    assert!(activated.warnings.is_empty());
    phase("driver.activate.done");
    phase("driver.inventory.before");
    validate_driver_inventory(&planned, &package, transfer).await;
    phase("driver.inventory.done");
    phase("driver.remnants.before");
    validate_driver_remnants(&planned, transfer).await;
    phase("driver.remnants.done");

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
        effective_capabilities: prepared.release.effective_capabilities.clone(),
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
    phase("driver.rollback-drift.done");
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
    phase("driver.rollback.done");
    phase("driver.undeploy.before");
    validate_undeploy_preserves_original_audit_ref(&planned, &previous).await;
    phase("driver.undeploy.done");
    assert_eq!(
        planned.driver.current(&planned.context).await.unwrap(),
        None
    );
    transfer.lock().await.fail_audit_appends_remaining = 1;
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
        restored.warnings.len(),
        1,
        "post-effect audit failure retains the healthy receipt"
    );
    assert_eq!(
        planned.driver.current(&planned.context).await.unwrap(),
        Some(previous)
    );
    phase("driver.restore.done");
}

async fn validate_undeploy_preserves_original_audit_ref(
    planned: &shipforge::application::PlannedComponent,
    current: &ReleaseRef,
) {
    use shipforge::drivers::audit::{RemoteAuditObserved, RemoteAuditPhase};
    let mut original = current.clone();
    original.effective_capabilities = shipforge::domain::DriverCapabilities::new([
        Capability::StagedDeployment,
        Capability::ExplicitActivation,
        Capability::Observe,
        Capability::Rollback,
        Capability::Cancellation,
    ]);
    assert!(
        planned
            .driver
            .static_capabilities()
            .contains(Capability::Inventory)
    );
    assert!(
        !original
            .effective_capabilities
            .contains(Capability::Inventory)
    );
    let deployment = DeploymentId::new();
    let undeployed = planned
        .driver
        .rollback(&deployment, &planned.context, Some(&original), None)
        .await
        .unwrap();
    assert_eq!(undeployed.current, None);
    assert!(undeployed.warnings.is_empty());
    let inventory = planned.driver.inventory(&planned.context).await.unwrap();
    let record = inventory
        .audit
        .records
        .iter()
        .find(|record| {
            record.deployment == deployment && record.phase == RemoteAuditPhase::Rollback
        })
        .unwrap();
    assert_eq!(
        record.release, original,
        "undeploy audit must not substitute the current Driver's capabilities for the frozen source reference"
    );
    assert_eq!(record.expected_current.as_ref(), Some(&current.version));
    assert_eq!(record.target, None);
    assert_eq!(record.observed, RemoteAuditObserved::NotDeployed);
    assert_eq!(record.healthy, None);
}

async fn validate_driver_remnants(
    planned: &shipforge::application::PlannedComponent,
    transfer: &Arc<AsyncMutex<TransferState>>,
) {
    use shipforge::drivers::inventory::TemporaryRemnantKind;
    let baseline = planned.driver.inventory(&planned.context).await.unwrap();
    assert!(!baseline.remnants.incomplete, "{baseline:?}");
    assert!(baseline.remnants.entries.is_empty());
    let root = protocol_target().root;
    let deployment = DeploymentId::new();
    let upload = format!("{root}/temporary/{deployment}.tar.gz");
    let extracted = format!("{root}/temporary/{deployment}.dir");
    let activation = format!("{root}/temporary/{deployment}.current");
    let marker = format!(
        "{root}/.shipforge-marker-{}.tmp",
        uuid::Uuid::now_v7().simple()
    );
    {
        let mut state = transfer.lock().await;
        state
            .files
            .insert(upload.clone(), b"partial upload".to_vec());
        state.directories.insert(extracted.clone());
        state
            .links
            .insert(activation.clone(), "/outside/never-follow".into());
        state
            .files
            .insert(marker.clone(), b"marker candidate".to_vec());
    }
    let found = planned.driver.inventory(&planned.context).await.unwrap();
    assert_eq!(found.releases, baseline.releases);
    assert_eq!(found.audit, baseline.audit);
    assert!(!found.remnants.incomplete);
    assert_eq!(found.remnants.entries.len(), 4);
    for entry in &found.remnants.entries {
        assert_eq!(
            entry.deployment.as_ref(),
            (entry.kind != TemporaryRemnantKind::MarkerPublication).then_some(&deployment)
        );
    }
    {
        let mut state = transfer.lock().await;
        assert_eq!(state.files[&upload], b"partial upload");
        assert_eq!(state.links[&activation], "/outside/never-follow");
        state
            .hard_link_counts
            .insert(format!("{root}/.shipforge-project.json"), 2);
    }
    let unknown = planned.driver.inventory(&planned.context).await.unwrap();
    assert_eq!(unknown.releases, baseline.releases);
    assert!(unknown.audit.incomplete);
    assert!(unknown.remnants.entries.is_empty());
    assert!(unknown.remnants.incomplete);
    assert!(
        unknown
            .remnants
            .notices
            .iter()
            .any(|notice| notice.contains("multiple hard links"))
    );
    let mut state = transfer.lock().await;
    state
        .hard_link_counts
        .remove(&format!("{root}/.shipforge-project.json"));
    state.files.remove(&upload);
    state.directories.remove(&extracted);
    state.links.remove(&activation);
    state.files.remove(&marker);
}

async fn validate_driver_inventory(
    planned: &shipforge::application::PlannedComponent,
    package: &shipforge::drivers::ReleasePackage,
    transfer: &Arc<AsyncMutex<TransferState>>,
) {
    use shipforge::drivers::audit::{RemoteAuditOutcome, RemoteAuditPhase};
    let inventory = planned.driver.inventory(&planned.context).await.unwrap();
    let found = inventory
        .releases
        .releases
        .iter()
        .find(|entry| entry.manifest.version == package.release().version)
        .unwrap();
    assert_eq!(&found.manifest, package.manifest());
    assert_eq!(found.sha256, package.sha256());
    assert_eq!(found.size, package.size());
    assert!(found.extracted);
    assert_eq!(
        inventory.releases.current,
        Ok(Some(package.release().version.clone()))
    );
    assert!(inventory.audit.records.iter().any(|record| {
        record.phase == RemoteAuditPhase::Prepare
            && record
                .package
                .as_ref()
                .is_some_and(|metadata| metadata.sha256 == package.sha256())
    }));
    assert!(
        inventory
            .audit
            .records
            .iter()
            .any(|record| record.phase == RemoteAuditPhase::Activate
                && record.outcome == RemoteAuditOutcome::Succeeded
                && record.healthy == Some(true))
    );
    let root = protocol_target().root;
    let paths = [
        format!("{root}/metadata/releases.jsonl"),
        format!("{root}/metadata/deployments.jsonl"),
    ];
    let saved = {
        let mut state = transfer.lock().await;
        paths
            .iter()
            .map(|path| state.files.remove(path).unwrap())
            .collect::<Vec<_>>()
    };
    let without_audit = planned.driver.inventory(&planned.context).await.unwrap();
    assert_eq!(
        without_audit.releases, inventory.releases,
        "archive facts do not depend on auxiliary audit history"
    );
    assert!(without_audit.audit.records.is_empty());
    assert!(without_audit.audit.incomplete);
    {
        let mut state = transfer.lock().await;
        for (path, bytes) in paths.iter().zip(saved) {
            state.files.insert(path.clone(), bytes);
        }
        state
            .files
            .get_mut(&paths[1])
            .unwrap()
            .extend_from_slice(b"{\"torn");
    }
    let torn = planned.driver.inventory(&planned.context).await.unwrap();
    assert_eq!(torn.releases, inventory.releases);
    assert_eq!(torn.audit.records, inventory.audit.records);
    assert!(torn.audit.incomplete);
    let archive_path = format!("{root}/archives/{}.tar.gz", package.release().version);
    let bytes = transfer
        .lock()
        .await
        .files
        .insert(archive_path.clone(), b"invalid gzip".to_vec())
        .unwrap();
    let corrupt = planned.driver.inventory(&planned.context).await.unwrap();
    assert!(
        !corrupt
            .releases
            .releases
            .iter()
            .any(|entry| entry.manifest.version == package.release().version)
    );
    assert!(
        corrupt
            .releases
            .issues
            .iter()
            .any(|issue| issue.version.as_ref() == Some(&package.release().version))
    );
    transfer.lock().await.files.insert(archive_path, bytes);
}

fn report_protocol_phase(started: Instant, commands: &Mutex<Vec<String>>, stage: &str) {
    let command_count = commands.lock().unwrap().len();
    eprintln!(
        "protocol phase={stage} elapsed_ms={} commands={command_count}",
        started.elapsed().as_millis()
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
        // Send the loopback fixture's immediate protocol responses without Nagle buffering.
        nodelay: true,
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
        sudo_channels: HashMap::new(),
        commands: commands.clone(),
        channels: Arc::new(AsyncMutex::new(HashMap::new())),
        transfer: transfer.clone(),
    };
    let running = server.run_on_socket(config, &listener);
    let shutdown = running.handle();
    let client = async {
        let started = Instant::now();
        let phase = |stage| report_protocol_phase(started, &commands, stage);
        phase("main.start");
        let directory = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let session =
            connect_client(address, &host_fingerprint, directory.path(), &cancellation).await;
        phase("main.connect.done");
        validate_exec(&session, &cancellation).await;
        phase("main.exec.done");
        validate_transfer(&session, directory.path(), &cancellation, &transfer).await;
        phase("main.transfer.done");
        let deployment = DeploymentId::new();
        let activation = validate_release_prepare(
            &session,
            directory.path(),
            &cancellation,
            &transfer,
            &deployment,
        )
        .await;
        phase("main.prepare.done");
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
        phase("main.compensation.done");
        validate_probe(&session, &cancellation).await;
        phase("main.probe.done");
        transfer.lock().await.health_status = 204;
        // The earlier low-level activation fixture represents an existing deployment.
        transfer.lock().await.files.insert(
            format!("{}/.shipforge-project.json", protocol_target().root),
            shipforge::drivers::linux_ssh::DeploymentMarker::for_release(activation.release())
                .encode()
                .unwrap(),
        );
        phase("main.production-driver.before");
        validate_production_driver(
            address,
            &host_fingerprint,
            directory.path(),
            activation.release(),
            &cancellation,
            &transfer,
            &commands,
        )
        .await;
        phase("main.production-driver.done");
        validate_marker_reads(&session, &transfer, activation.release(), &cancellation).await;
        phase("main.marker.done");
        validate_layout_and_manifest_safety(
            &session,
            &transfer,
            activation.release(),
            &cancellation,
        )
        .await;
        phase("main.layout.done");
        session.disconnect().await.unwrap();

        phase("main.service.before");
        deployment_service::validate(
            address,
            &host_fingerprint,
            directory.path(),
            &transfer,
            &commands,
        )
        .await;
        phase("main.service.done");

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
    let (server_result, ()) = Box::pin(tokio::time::timeout(Duration::from_secs(180), async {
        tokio::join!(running, client)
    }))
    .await
    .expect("the complete loopback protocol fixture must finish within 180 seconds");
    server_result.unwrap();
}
