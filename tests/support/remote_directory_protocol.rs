//! Real loopback SSH transport with exact, read-only protocol replies.
//! This fixture never runs a Shell or simulates evidence from a real filesystem.

use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, ssh_key},
    server::{self, Msg, Server as _, Session},
};
use shipforge::{
    application::{DestinationSetupRequest, DestinationSetupService, SetupCredential},
    config::SshCredential,
    drivers::{DriverDestinationInput, DriverKind, linux_ssh::LinuxSshSetupGateway},
};
use tokio::{net::TcpListener, sync::Notify};
use tokio_util::sync::CancellationToken;

const ROOT: &str = "/srv/literal;$(not-executed) ' directory";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum ListingReply {
    Output {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        status: Option<u32>,
    },
    Hold,
}

impl ListingReply {
    fn complete(stdout: Vec<u8>) -> Self {
        Self::Output {
            stdout,
            stderr: Vec::new(),
            status: Some(0),
        }
    }
}

#[derive(Debug, Default)]
struct Evidence {
    connections: AtomicUsize,
    closed: AtomicUsize,
    signed_auth_attempts: AtomicUsize,
    authenticated: AtomicUsize,
    unexpected_requests: AtomicUsize,
    commands: Mutex<Vec<String>>,
    enumeration_started: Notify,
}

struct DirectoryServer {
    expected_identity: Arc<ssh_key::PublicKey>,
    listing: Arc<ListingReply>,
    evidence: Arc<Evidence>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    client_handler: bool,
}

impl Drop for DirectoryServer {
    fn drop(&mut self) {
        if self.client_handler {
            self.evidence.closed.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl server::Server for DirectoryServer {
    type Handler = Self;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.evidence.connections.fetch_add(1, Ordering::SeqCst);
        Self {
            expected_identity: Arc::clone(&self.expected_identity),
            listing: Arc::clone(&self.listing),
            evidence: Arc::clone(&self.evidence),
            channels: HashMap::new(),
            client_handler: true,
        }
    }
}

impl server::Handler for DirectoryServer {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        // Russh invokes this callback only after verifying the client's signature.
        self.evidence
            .signed_auth_attempts
            .fetch_add(1, Ordering::SeqCst);
        if user == "deploy" && public_key == self.expected_identity.as_ref() {
            self.evidence.authenticated.fetch_add(1, Ordering::SeqCst);
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8(data.to_vec()).expect("structured UTF-8 SSH command");
        self.evidence.commands.lock().unwrap().push(command.clone());
        let arguments = super::structured_arguments(&command).unwrap_or_default();
        let words = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        session.channel_success(channel)?;
        match words.as_slice() {
            ["readlink", "-e", "--", ROOT] => {
                session.data(channel, format!("{ROOT}\n").into_bytes())?;
                session.exit_status_request(channel, 0)?;
            }
            ["test", "-d", ROOT] => session.exit_status_request(channel, 0)?,
            [
                "find",
                "-P",
                ROOT,
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "-type",
                "d",
                "-printf",
                "%p\\0",
            ] => {
                self.evidence.enumeration_started.notify_one();
                match self.listing.as_ref() {
                    ListingReply::Hold => {
                        // Leave the command pending, but keep processing SSH
                        // messages so client cancellation can close the connection.
                        return Ok(());
                    }
                    ListingReply::Output {
                        stdout,
                        stderr,
                        status,
                    } => {
                        for bytes in stdout.chunks(16 * 1024) {
                            session.data(channel, bytes.to_vec())?;
                        }
                        if !stderr.is_empty() {
                            session.extended_data(channel, 1, stderr.clone())?;
                        }
                        if let Some(status) = status {
                            session.exit_status_request(channel, *status)?;
                        }
                    }
                }
            }
            _ => {
                self.evidence
                    .unexpected_requests
                    .fetch_add(1, Ordering::SeqCst);
                session.exit_status_request(channel, 127)?;
            }
        }
        self.channels.remove(&channel);
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.evidence
            .unexpected_requests
            .fetch_add(1, Ordering::SeqCst);
        session.channel_failure(channel)?;
        Ok(())
    }
}

struct ClientFixture {
    directory: PathBuf,
    service: DestinationSetupService,
    request: DestinationSetupRequest,
    evidence: Arc<Evidence>,
}

async fn with_protocol<F, Fut>(listing: ListingReply, client: F)
where
    F: FnOnce(ClientFixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = host_key
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let identity = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let expected_identity = Arc::new(identity.public_key().clone());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("id_ed25519");
    std::fs::write(&path, identity.to_openssh(ssh_key::LineEnding::LF).unwrap()).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    let evidence = Arc::new(Evidence::default());
    let mut server = DirectoryServer {
        expected_identity,
        listing: Arc::new(listing),
        evidence: Arc::clone(&evidence),
        channels: HashMap::new(),
        client_handler: false,
    };
    let config = Arc::new(server::Config {
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        nodelay: true,
        keys: vec![host_key],
        ..server::Config::default()
    });
    let running = server.run_on_socket(config, &listener);
    let shutdown = running.handle();
    let client = async {
        client(ClientFixture {
            // Keep the owning TempDir in this harness. Async closure capture
            // can drop unused fixture fields before their future is polled.
            directory: directory.path().to_owned(),
            service: DestinationSetupService::new(Arc::new(LinuxSshSetupGateway)),
            request: DestinationSetupRequest {
                driver: DriverKind::parse("linux-ssh").unwrap(),
                destination: DriverDestinationInput {
                    value: serde_json::json!({
                        "host": "127.0.0.1",
                        "port": address.port(),
                        "user": "deploy",
                        "hostKey": fingerprint,
                    }),
                },
                credential: SetupCredential::new(SshCredential::IdentityFile { path }),
                remote_root: ROOT.into(),
            },
            evidence: Arc::clone(&evidence),
        })
        .await;
        assert!(directory.path().join("id_ed25519").is_file());
        // Observe connection teardown before stopping the listener; shutdown
        // itself must not masquerade as the client's cancellation cleanup.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if evidence.closed.load(Ordering::SeqCst)
                    == evidence.connections.load(Ordering::SeqCst)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("directory client closes its SSH connection");
        shutdown.shutdown("loopback directory test complete".into());
    };
    let (server_result, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(running, client)
    })
    .await
    .expect("loopback directory protocol test finishes");
    server_result.unwrap();
}

fn assert_read_only_commands(evidence: &Evidence, count: usize) {
    let canonical = vec!["readlink", "-e", "--", ROOT];
    let expected = [
        canonical.clone(),
        vec!["test", "-d", ROOT],
        vec![
            "find",
            "-P",
            ROOT,
            "-mindepth",
            "1",
            "-maxdepth",
            "1",
            "-type",
            "d",
            "-printf",
            "%p\\0",
        ],
        canonical,
    ];
    let commands = evidence.commands.lock().unwrap();
    assert_eq!(commands.len(), count);
    for (command, expected) in commands.iter().zip(expected.iter()) {
        assert_eq!(super::structured_arguments(command).unwrap(), *expected);
    }
    assert_eq!(evidence.unexpected_requests.load(Ordering::SeqCst), 0);
    assert_eq!(evidence.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_service_uses_pinned_authenticated_ssh_and_literal_nul_children() {
    let children = [
        format!("{ROOT}/worker 'quoted';$(not-executed)"),
        format!("{ROOT}/api"),
    ];
    let stdout = format!("{}\0{}\0", children[0], children[1]).into_bytes();
    with_protocol(ListingReply::complete(stdout), |fixture| async move {
        let result = fixture
            .service
            .browse_directories(
                &fixture.request,
                CONNECT_TIMEOUT,
                COMMAND_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.directory, ROOT);
        assert_eq!(
            result.directories,
            vec![children[1].clone(), children[0].clone()]
        );
        assert_eq!(
            fixture.evidence.signed_auth_attempts.load(Ordering::SeqCst),
            1
        );
        assert_eq!(fixture.evidence.authenticated.load(Ordering::SeqCst), 1);
        assert_read_only_commands(&fixture.evidence, 4);
        assert_eq!(
            fixture.evidence.commands.lock().unwrap()[0],
            "'readlink' '-e' '--' '/srv/literal;$(not-executed) '\\'' directory'"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_service_rejects_wrong_pin_or_identity_before_any_exec() {
    for wrong_pin in [true, false] {
        with_protocol(
            ListingReply::complete(Vec::new()),
            |mut fixture| async move {
                let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
                if wrong_pin {
                    fixture.request.destination.value["hostKey"] = other
                        .public_key()
                        .fingerprint(ssh_key::HashAlg::Sha256)
                        .to_string()
                        .into();
                } else {
                    let path = fixture.directory.join("other_ed25519");
                    std::fs::write(&path, other.to_openssh(ssh_key::LineEnding::LF).unwrap())
                        .unwrap();
                    fixture.request.credential =
                        SetupCredential::new(SshCredential::IdentityFile { path });
                }
                let error = fixture
                    .service
                    .browse_directories(
                        &fixture.request,
                        CONNECT_TIMEOUT,
                        COMMAND_TIMEOUT,
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("could not be authenticated"));
                assert_eq!(fixture.evidence.authenticated.load(Ordering::SeqCst), 0);
                assert_eq!(
                    fixture.evidence.signed_auth_attempts.load(Ordering::SeqCst),
                    usize::from(!wrong_pin)
                );
                assert_read_only_commands(&fixture.evidence, 0);
            },
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_service_rejects_real_transport_truncation_even_when_prefix_is_valid() {
    let mut stdout = Vec::new();
    for index in 0..256 {
        let padding = "x".repeat(255 - ROOT.len() - 1 - 3);
        stdout.extend_from_slice(format!("{ROOT}/{index:03}{padding}\0").as_bytes());
    }
    assert_eq!(stdout.len(), 64 * 1024);
    // The retained prefix alone is a valid list of 256 direct children. Only
    // the transport's truncation evidence distinguishes it from a full reply.
    stdout.extend_from_slice(format!("{ROOT}/extra-not-accepted\0").as_bytes());
    with_protocol(ListingReply::complete(stdout), |fixture| async move {
        let error = fixture
            .service
            .browse_directories(
                &fixture.request,
                CONNECT_TIMEOUT,
                COMMAND_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("evidence is incomplete"));
        assert_read_only_commands(&fixture.evidence, 3);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_service_rejects_invalid_ssh_output_instead_of_accepting_empty_evidence() {
    let valid = format!("{ROOT}/api\0").into_bytes();
    let replies = [
        ListingReply::complete(format!("{ROOT}/missing-nul-sentinel").into_bytes()),
        ListingReply::complete(b"/outside-private-sentinel\0".to_vec()),
        ListingReply::complete(vec![0xff, 0]),
        ListingReply::Output {
            stdout: valid.clone(),
            stderr: b"stderr-private-sentinel".to_vec(),
            status: Some(0),
        },
        ListingReply::Output {
            stdout: valid.clone(),
            stderr: Vec::new(),
            status: Some(1),
        },
        ListingReply::Output {
            stdout: valid,
            stderr: Vec::new(),
            status: None,
        },
    ];
    for reply in replies {
        with_protocol(reply, |fixture| async move {
            let error = fixture
                .service
                .browse_directories(
                    &fixture.request,
                    CONNECT_TIMEOUT,
                    COMMAND_TIMEOUT,
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("private-sentinel"));
            assert!(!error.to_string().contains("missing-nul-sentinel"));
            assert_read_only_commands(&fixture.evidence, 3);
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_service_cancels_an_in_flight_exec_and_closes_the_ssh_connection() {
    with_protocol(ListingReply::Hold, |fixture| async move {
        let cancellation = CancellationToken::new();
        let browse = fixture.service.browse_directories(
            &fixture.request,
            CONNECT_TIMEOUT,
            COMMAND_TIMEOUT,
            &cancellation,
        );
        let cancel = async {
            tokio::time::timeout(
                Duration::from_secs(5),
                fixture.evidence.enumeration_started.notified(),
            )
            .await
            .expect("the real SSH find request reaches the server before cancellation");
            cancellation.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(browse, cancel)
        })
        .await
        .expect("cancelled remote enumeration returns within its deadline");
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_read_only_commands(&fixture.evidence, 3);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_service_times_out_an_in_flight_exec_and_closes_the_ssh_connection() {
    with_protocol(ListingReply::Hold, |fixture| async move {
        let cancellation = CancellationToken::new();
        let browse = fixture.service.browse_directories(
            &fixture.request,
            CONNECT_TIMEOUT,
            Duration::from_secs(1),
            &cancellation,
        );
        tokio::pin!(browse);
        tokio::select! {
            biased;
            observed = tokio::time::timeout(
                Duration::from_secs(5), fixture.evidence.enumeration_started.notified(),
            ) => observed.expect("the real SSH find request is pending before its timeout is accepted"),
            result = &mut browse => panic!("directory request completed before a pending find was observed: {result:?}"),
        };
        // Bound the remaining wait from the observed pending exec, not merely
        // from starting a connection. A different early protocol error is not
        // accepted as evidence that the directory deadline worked.
        let error = tokio::time::timeout(Duration::from_secs(3), browse)
            .await
            .expect("one-second deadline returns within three seconds of the observed pending find")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "directory browsing failed: directory browsing timed out",
        );
        assert!(!cancellation.is_cancelled());
        assert_read_only_commands(&fixture.evidence, 3);
    })
    .await;
}
