use std::collections::{BTreeSet, HashMap, HashSet};
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
    application::package_release,
    config::{CredentialRegistry, ResolvedArtifact, ResolvedArtifactKind, SshCredential},
    domain::{
        Capability, ComponentGeneration, ComponentName, ComponentRelease, DeploymentId,
        DestinationKey, DestinationRevision, EnvironmentId, ProjectId, ReleaseVersion,
    },
    drivers::{
        ComponentExecutionContext, ComponentRequest, DeploymentDriver, DriverDestinationInput,
        DriverLog, DriverTargetInput, EndpointFingerprint, EventSink,
        linux_ssh::{
            AuthenticatedSession, LinuxSshDestination, LinuxSshDriver, RemotePath, UploadError,
            UploadOptions, connect_authenticated,
        },
    },
    telemetry::{CommandArgument, CommandSpec},
};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
#[path = "support/password_protocol.rs"]
mod password_protocol;

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
    fail_writes_remaining: usize,
    root: Option<std::path::PathBuf>,
    fail_service: bool,
    unknown_service: bool,
    removals: usize,
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
        if let Some(action) = command
            .strip_prefix("cd -- '/fixture' && exec 'fixture-")
            .and_then(|s| s.strip_suffix('\''))
        {
            let mut state = self.transfer.lock().await;
            session.channel_success(channel)?;
            if action == "start" && state.unknown_service {
                state.unknown_service = false;
                session.close(channel)?;
                return Ok(());
            }
            let failed = action == "start" && state.fail_service;
            if failed {
                state.fail_service = false;
            }
            session.exit_status_request(channel, u32::from(failed))?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
        let (status, output) = if command.starts_with("'printf' ") {
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
    let args = structured_arguments(command)?;
    if args.first()?.as_str() == "python3" {
        inplace_reply(&args, transfer).await
    } else {
        None
    }
}

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

async fn inplace_reply(
    args: &[String],
    transfer: &Arc<AsyncMutex<TransferState>>,
) -> Option<(u32, Vec<u8>)> {
    if args.len() != 4
        || args[1] != "-c"
        || args[2] != include_str!("../src/drivers/linux_ssh/inplace.py")
    {
        return None;
    }
    let mut request: serde_json::Value = serde_json::from_str(&args[3]).ok()?;
    if request["root"] != "/fixture" {
        return None;
    }
    let mut state = transfer.lock().await;
    let root = state.root.clone()?;
    let incoming = root.join(".shipforge-deploy/incoming.tar.gz");
    if request["operation"] == "prepare" {
        std::fs::write(
            &incoming,
            state
                .files
                .get("/fixture/.shipforge-deploy/incoming.tar.gz")?,
        )
        .ok()?;
    }
    request["root"] = serde_json::json!(root);
    let output = std::process::Command::new("python")
        .args(["-c", &args[2], &request.to_string()])
        .output()
        .ok()?;
    let mut response: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    if response.get("upload").is_some() {
        response["upload"] = serde_json::json!("/fixture/.shipforge-deploy/incoming.tar.gz");
    }
    if !incoming.exists() {
        state
            .files
            .remove("/fixture/.shipforge-deploy/incoming.tar.gz");
    }
    Some((
        u32::try_from(output.status.code()?).ok()?,
        serde_json::to_vec(&response).ok()?,
    ))
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn inplace_driver_publishes_recovers_and_blocks_unknown_service_outcomes() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pin = key
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        ..server::Config::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("app");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("app"), b"original").unwrap();
    std::fs::write(root.join("config.yaml"), b"runtime").unwrap();
    let mut peer = ProtocolServer::default();
    peer.transfer.lock().await.root = Some(root.clone());
    let transfer = peer.transfer.clone();
    let commands = peer.commands.clone();
    let running = peer.run_on_socket(config, &listener);
    let shutdown = running.handle();
    let client = async {
        let identity = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let identity_path = directory.path().join("test-key");
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
        let driver = LinuxSshDriver::new(Arc::new(credentials));
        let context=ComponentExecutionContext {
            project_id:ProjectId::new(),environment_id:EnvironmentId::new(),component:ComponentName::parse("app").unwrap(),
            generation:ComponentGeneration::INITIAL,destination:DestinationKey::new(),destination_revision:DestinationRevision::INITIAL,
            credential,endpoint_fingerprint:EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            destination_settings:driver.validate_destination(&DriverDestinationInput{value:serde_json::json!({"host":"127.0.0.1","port":port,"user":"deploy","hostKey":pin})}).unwrap(),
            target:driver.validate_target(&DriverTargetInput{value:serde_json::json!({"root":"/fixture","service":{"start":[["fixture-start"]],"stop":[["fixture-stop"]]},"health":null})}).unwrap(),
            cancellation:CancellationToken::new(),
        };
        assert!(!driver.static_capabilities().contains(Capability::Retention));
        let artifact = directory.path().join("app-binary");
        std::fs::create_dir(&artifact).unwrap();
        let mut current = None;
        for (index, mode) in ["ok", "fail", "ok", "unknown"].iter().enumerate() {
            std::fs::write(artifact.join("app"), format!("version-{index}")).unwrap();
            let release = ComponentRelease {
                project_id: context.project_id.clone(),
                environment_id: context.environment_id.clone(),
                component: context.component.clone(),
                generation: context.generation,
                destination: context.destination.clone(),
                destination_revision: context.destination_revision,
                version: ReleaseVersion::parse(format!("v{index}")).unwrap(),
            };
            let plan = driver
                .plan(
                    &context,
                    &ComponentRequest {
                        release: release.clone(),
                        required_capabilities: BTreeSet::new(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(plan.expected_current, current);
            let package = package_release(
                &ResolvedArtifact {
                    path: artifact.clone(),
                    kind: ResolvedArtifactKind::Directory,
                },
                &release,
                &directory.path().join("packages"),
                43,
                None,
                &context.cancellation,
            )
            .unwrap();
            let deployment = DeploymentId::new();
            let prepared = driver
                .prepare(&deployment, &context, &plan, &package, &IgnoreEvents)
                .await
                .unwrap();
            if *mode == "fail" {
                transfer.lock().await.fail_service = true;
            }
            if *mode == "unknown" {
                transfer.lock().await.unknown_service = true;
            }
            let result = driver
                .activate_with_events(&deployment, &context, &prepared.release, &IgnoreEvents)
                .await;
            match *mode {
                "ok" => {
                    current = result.unwrap().current;
                    assert_eq!(
                        std::fs::read_to_string(root.join("app")).unwrap(),
                        format!("version-{index}")
                    );
                }
                "fail" => {
                    assert!(!result.unwrap_err().recovery_blocked);
                    assert_eq!(driver.current(&context).await.unwrap(), current);
                    assert_eq!(std::fs::read(root.join("app")).unwrap(), b"version-0");
                }
                _ => {
                    assert!(result.unwrap_err().recovery_blocked);
                    assert!(driver.current(&context).await.is_err());
                    assert!(
                        driver
                            .rollback(
                                &DeploymentId::new(),
                                &context,
                                Some(&prepared.release),
                                current.as_ref()
                            )
                            .await
                            .is_err()
                    );
                }
            }
            assert_eq!(std::fs::read(root.join("config.yaml")).unwrap(), b"runtime");
            assert!(!root.join("releases").exists());
            assert!(!root.join("current").exists());
        }
        assert!(
            commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "cd -- '/fixture' && exec 'fixture-stop'")
        );
        shutdown.shutdown("in-place test complete".into());
    };
    let (server_result, ()) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(running, client)
    })
    .await
    .unwrap();
    server_result.unwrap();
}
