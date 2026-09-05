//! Real SSH traffic over memory streams and loopback sockets with temporary credentials.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, ssh_key},
    server,
};

use crate::{
    config::HostKeyFingerprint,
    drivers::DriverLog,
    telemetry::{
        CommandArgument,
        log_record::{CommandLocation, LogEvent, LogEventKind},
    },
};

use super::*;

#[derive(Default)]
struct Events(Mutex<Vec<LogEvent>>);

impl EventSink for Events {
    fn emit(&self, _event: DriverLog) {
        panic!("structured event expected");
    }
    fn emit_record(&self, event: LogEvent) {
        self.0.lock().unwrap().push(event);
    }
}

struct Peer {
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    authentication_started: Option<Arc<AtomicBool>>,
    authentication_gate: Option<Arc<tokio::sync::Notify>>,
}

impl server::Handler for Peer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _public_key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        if let Some(started) = &self.authentication_started {
            started.store(true, Ordering::SeqCst);
        }
        if let Some(gate) = &self.authentication_gate {
            gate.notified().await;
        }
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        self.commands.lock().unwrap().push(data.to_vec());
        session.channel_success(channel)?;
        session.data(channel, b"stdout must not invent the failed argv".to_vec())?;
        session.exit_status_request(channel, if data.starts_with(b"'test' ") { 1 } else { 23 })?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[derive(Default)]
struct InterruptEvidence {
    channels_opened: AtomicUsize,
    commands: AtomicUsize,
    signals: Mutex<Vec<String>>,
    command_started: tokio::sync::Notify,
}

struct InterruptPeer {
    evidence: Arc<InterruptEvidence>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    ignore_term: bool,
}

impl server::Handler for InterruptPeer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.evidence.channels_opened.fetch_add(1, Ordering::SeqCst);
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.evidence.commands.fetch_add(1, Ordering::SeqCst);
        session.channel_success(channel)?;
        self.evidence.command_started.notify_one();
        Ok(())
    }

    async fn signal(
        &mut self,
        channel: ChannelId,
        signal: Sig,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.evidence
            .signals
            .lock()
            .unwrap()
            .push(format!("{signal:?}"));
        if self.ignore_term && matches!(&signal, Sig::TERM) {
            return Ok(());
        }
        self.channels.remove(&channel);
        session.exit_signal_request(channel, signal, false, "", "")?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

#[derive(Default)]
struct KnownStatusEvidence {
    commands: AtomicUsize,
    channels_closed: AtomicUsize,
    signals: Mutex<Vec<String>>,
    command_started: tokio::sync::Notify,
    status_sent: tokio::sync::Notify,
    channel_closed: tokio::sync::Notify,
}

struct KnownStatusPeer {
    evidence: Arc<KnownStatusEvidence>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    status_gates: VecDeque<Arc<tokio::sync::Notify>>,
}

impl server::Handler for KnownStatusPeer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.evidence.commands.fetch_add(1, Ordering::SeqCst);
        session.channel_success(channel)?;
        self.evidence.command_started.notify_one();
        if let Some(gate) = self.status_gates.pop_front() {
            gate.notified().await;
        }
        session.data(channel, b"known stdout".to_vec())?;
        session.extended_data(channel, 1, b"known stderr".to_vec())?;
        session.exit_status_request(channel, 23)?;
        // Intentionally retain the channel and omit Close. This makes a late
        // cancellation/deadline race with Close impossible and requires the
        // client to preserve the already-known outcome while closing it.
        self.evidence.status_sent.notify_one();
        Ok(())
    }

    async fn signal(
        &mut self,
        _channel: ChannelId,
        signal: Sig,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.evidence
            .signals
            .lock()
            .unwrap()
            .push(format!("{signal:?}"));
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        self.evidence.channels_closed.fetch_add(1, Ordering::SeqCst);
        self.evidence.channel_closed.notify_one();
        Ok(())
    }
}

#[derive(Default)]
struct FloodEvidence {
    chunks_sent: AtomicUsize,
    signals: Mutex<Vec<String>>,
    output_started: tokio::sync::Notify,
}

struct FloodPeer {
    evidence: Arc<FloodEvidence>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    producers: HashMap<ChannelId, CancellationToken>,
    ignore_term: bool,
}

impl server::Handler for FloodPeer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        _data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel_id)?;
        let channel = self
            .channels
            .remove(&channel_id)
            .expect("opened flood channel must be retained");
        let (_, writer) = channel.split();
        let cancellation = CancellationToken::new();
        self.producers.insert(channel_id, cancellation.clone());
        let evidence = Arc::clone(&self.evidence);
        drop(tokio::spawn(async move {
            loop {
                let sent = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => false,
                    result = writer.data_bytes(vec![b'x'; 16 * 1024]) => result.is_ok(),
                };
                if !sent {
                    break;
                }
                evidence.chunks_sent.fetch_add(1, Ordering::SeqCst);
                evidence.output_started.notify_one();
            }
        }));
        Ok(())
    }

    async fn signal(
        &mut self,
        channel: ChannelId,
        signal: Sig,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.evidence
            .signals
            .lock()
            .unwrap()
            .push(format!("{signal:?}"));
        if self.ignore_term && matches!(&signal, Sig::TERM) {
            return Ok(());
        }
        if let Some(cancellation) = self.producers.remove(&channel) {
            cancellation.cancel();
        }
        session.exit_signal_request(channel, signal, false, "", "")?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if let Some(cancellation) = self.producers.remove(&channel) {
            cancellation.cancel();
        }
        Ok(())
    }
}

fn failure_command() -> CommandSpec {
    CommandSpec::structured(
        "fixture-failure",
        [
            CommandArgument::plain("actual argv ; ' with spaces"),
            CommandArgument::sensitive("fixture-secret-do-not-copy"),
        ],
    )
    .unwrap()
}

async fn client_operations(handle: client::Handle<HostKeyVerifier>, events: &Events) {
    super::super::command_events::relay(events, |sink| async move {
        let session = AuthenticatedSession {
            handle,
            command_events: None,
        }
        .with_command_events(sink);
        let command = failure_command();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            session
                .execute(&command, Duration::from_secs(1), &cancellation)
                .await,
            Err(SshConnectionError::Cancelled)
        ));
        let cancellation = CancellationToken::new();
        let mut invalid = command.clone();
        invalid.shell = true;
        assert!(matches!(
            session
                .execute(&invalid, Duration::from_secs(1), &cancellation)
                .await,
            Err(SshConnectionError::Command(_))
        ));
        let output = session
            .execute(&command, Duration::from_secs(1), &cancellation)
            .await
            .unwrap();
        assert_eq!(output.exit_status, 23);
        let predicate = CommandSpec::structured(
            "test",
            ["-e", "/fixture/absent"].map(CommandArgument::plain),
        )
        .unwrap();
        let output = session
            .execute_allowing(&predicate, Duration::from_secs(1), &cancellation, &[0, 1])
            .await
            .unwrap();
        assert_eq!(output.exit_status, 1);
        session.disconnect().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn actual_ssh_exec_emits_one_exact_redacted_snapshot_and_no_predicate_or_predispatch_failure()
{
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = HostKeyFingerprint::parse(
        key.public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string(),
    )
    .unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        ..server::Config::default()
    });
    let commands = Arc::new(Mutex::new(Vec::new()));
    let peer = Peer {
        commands: Arc::clone(&commands),
        channels: HashMap::new(),
        authentication_started: None,
        authentication_gate: None,
    };
    let (client_stream, server_stream) = tokio::io::duplex(128 * 1024);
    let events = Events::default();
    let server = async {
        server::run_stream(config, server_stream, peer)
            .await
            .unwrap()
            .await
            .unwrap();
    };
    let client = async {
        let mut handle = client::connect_stream(
            client_config(),
            client_stream,
            HostKeyVerifier::strict(fingerprint),
        )
        .await
        .unwrap();
        assert!(handle.authenticate_none("fixture").await.unwrap().success());
        client_operations(handle, &events).await;
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, client);
    })
    .await
    .unwrap();
    let sent = commands.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0],
        failure_command().render_posix().unwrap().as_bytes()
    );
    let recorded = events.0.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let LogEventKind::FailedCommand { command } = &recorded[0].kind else {
        panic!("real failed command expected");
    };
    assert_eq!(command.location, CommandLocation::Remote);
    assert_eq!(command.program, "fixture-failure");
    assert_eq!(command.args, ["actual argv ; ' with spaces", "[REDACTED]"]);
    assert!(recorded[0].scope.is_none());
    let text = serde_json::to_string(&*recorded).unwrap();
    assert!(!text.contains("fixture-secret"));
    assert!(!text.contains("stdout must not"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatched_interruptions_signal_and_return_within_cleanup_bound() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = HostKeyFingerprint::parse(
        key.public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string(),
    )
    .unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        ..server::Config::default()
    });
    let evidence = Arc::new(InterruptEvidence::default());
    let peer = InterruptPeer {
        evidence: Arc::clone(&evidence),
        channels: HashMap::new(),
        ignore_term: true,
    };
    let (client_stream, server_stream) = tokio::io::duplex(128 * 1024);
    let server = async {
        server::run_stream(config, server_stream, peer)
            .await
            .unwrap()
            .await
            .unwrap();
    };
    let client = async {
        let mut handle = client::connect_stream(
            client_config(),
            client_stream,
            HostKeyVerifier::strict(fingerprint),
        )
        .await
        .unwrap();
        assert!(handle.authenticate_none("fixture").await.unwrap().success());
        let session = AuthenticatedSession {
            handle,
            command_events: None,
        };
        let command =
            CommandSpec::structured("held-command", std::iter::empty::<CommandArgument>()).unwrap();

        let pre_dispatch = CancellationToken::new();
        pre_dispatch.cancel();
        assert!(matches!(
            session
                .execute(&command, Duration::from_secs(5), &pre_dispatch)
                .await,
            Err(SshConnectionError::Cancelled)
        ));
        assert_eq!(evidence.channels_opened.load(Ordering::SeqCst), 0);

        let cancellation = CancellationToken::new();
        let execution = session.execute(&command, Duration::from_secs(5), &cancellation);
        let cancel = async {
            evidence.command_started.notified().await;
            let cancelled_at = tokio::time::Instant::now();
            cancellation.cancel();
            cancelled_at
        };
        let (result, cancelled_at) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(execution, cancel)
        })
        .await
        .expect("cancelled command must finish within its cleanup bound");
        assert!(matches!(result, Err(SshConnectionError::Cancelled)));
        assert!(cancelled_at.elapsed() < Duration::from_secs(2));
        assert_eq!(evidence.channels_opened.load(Ordering::SeqCst), 1);
        assert_eq!(evidence.commands.load(Ordering::SeqCst), 1);
        assert_eq!(&*evidence.signals.lock().unwrap(), &["TERM", "KILL"]);

        let timeout = Duration::from_secs(1);
        let timeout_started = tokio::time::Instant::now();
        let error = session
            .execute(&command, timeout, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(timeout_started.elapsed() < Duration::from_secs(3));
        assert!(matches!(
            error,
            SshConnectionError::Timeout {
                timeout: budget,
                phase: "remote command",
            } if budget == timeout
        ));
        assert_eq!(evidence.channels_opened.load(Ordering::SeqCst), 2);
        assert_eq!(evidence.commands.load(Ordering::SeqCst), 2);
        assert_eq!(
            &*evidence.signals.lock().unwrap(),
            &["TERM", "KILL", "TERM", "KILL"]
        );
        session.disconnect().await.unwrap();
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(server, client);
    }))
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuous_remote_output_cannot_starve_cancellation_or_timeout() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = HostKeyFingerprint::parse(
        key.public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string(),
    )
    .unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        ..server::Config::default()
    });
    let evidence = Arc::new(FloodEvidence::default());
    let peer = FloodPeer {
        evidence: Arc::clone(&evidence),
        channels: HashMap::new(),
        producers: HashMap::new(),
        ignore_term: true,
    };
    let (client_stream, server_stream) = tokio::io::duplex(512 * 1024);
    let server = async {
        server::run_stream(config, server_stream, peer)
            .await
            .unwrap()
            .await
            .unwrap();
    };
    let client = async {
        let mut handle = client::connect_stream(
            client_config(),
            client_stream,
            HostKeyVerifier::strict(fingerprint),
        )
        .await
        .unwrap();
        assert!(handle.authenticate_none("fixture").await.unwrap().success());
        let session = AuthenticatedSession {
            handle,
            command_events: None,
        };
        let command =
            CommandSpec::structured("flood", std::iter::empty::<CommandArgument>()).unwrap();

        let cancellation = CancellationToken::new();
        let execution = session.execute(&command, Duration::from_secs(5), &cancellation);
        let cancel_during_output = async {
            evidence.output_started.notified().await;
            cancellation.cancel();
        };
        let started = tokio::time::Instant::now();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(execution, cancel_during_output)
        })
        .await
        .expect("continuous output must not starve cancellation");
        assert!(matches!(result, Err(SshConnectionError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));

        let timeout = Duration::from_millis(200);
        let started = tokio::time::Instant::now();
        let error = session
            .execute(&command, timeout, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SshConnectionError::Timeout {
                timeout: budget,
                phase: "remote command",
            } if budget == timeout
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(evidence.chunks_sent.load(Ordering::SeqCst) > 1);
        assert_eq!(
            &*evidence.signals.lock().unwrap(),
            &["TERM", "KILL", "TERM", "KILL"]
        );
        session.disconnect().await.unwrap();
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(server, client);
    }))
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_exit_status_survives_late_cancellation_and_timeout_before_close() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = HostKeyFingerprint::parse(
        key.public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string(),
    )
    .unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        ..server::Config::default()
    });
    let evidence = Arc::new(KnownStatusEvidence::default());
    let cancellation_status_gate = Arc::new(tokio::sync::Notify::new());
    let timeout_status_gate = Arc::new(tokio::sync::Notify::new());
    let peer = KnownStatusPeer {
        evidence: Arc::clone(&evidence),
        channels: HashMap::new(),
        status_gates: VecDeque::from([
            Arc::clone(&cancellation_status_gate),
            Arc::clone(&timeout_status_gate),
        ]),
    };
    let (client_stream, server_stream) = tokio::io::duplex(128 * 1024);
    let server = async {
        server::run_stream(config, server_stream, peer)
            .await
            .unwrap()
            .await
            .unwrap();
    };
    let client = async {
        let mut handle = client::connect_stream(
            client_config(),
            client_stream,
            HostKeyVerifier::strict(fingerprint),
        )
        .await
        .unwrap();
        assert!(handle.authenticate_none("fixture").await.unwrap().success());
        let session = AuthenticatedSession {
            handle,
            command_events: None,
        };
        let command = CommandSpec::structured(
            "known-status-command",
            std::iter::empty::<CommandArgument>(),
        )
        .unwrap();

        let cancellation = CancellationToken::new();
        let execution = session.execute(&command, Duration::from_secs(5), &cancellation);
        let cancel_before_status = async {
            evidence.command_started.notified().await;
            cancellation.cancel();
            // Release an ExitStatus only after cancellation is observable.
            // The pre-signal settle window must retain this independently
            // completed outcome instead of sending TERM first.
            cancellation_status_gate.notify_one();
        };
        let (output, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(execution, cancel_before_status)
        })
        .await
        .expect("a known result must survive late cancellation");
        assert_known_output(&output.unwrap());
        wait_for_channel_closes(&evidence, 1).await;

        let timeout = Duration::from_millis(400);
        let started = tokio::time::Instant::now();
        let status_release = started + timeout + Duration::from_millis(10);
        let timeout_cancellation = CancellationToken::new();
        let execution = session.execute(&command, timeout, &timeout_cancellation);
        let release_status_after_deadline = async {
            evidence.command_started.notified().await;
            // Anchor this to the same wall-clock start as `execute`. Sleeping
            // for a full timeout after dispatch would also include channel
            // setup and could release outside the 50 ms settle window.
            tokio::time::sleep_until(status_release).await;
            timeout_status_gate.notify_one();
        };
        let (output, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(execution, release_status_after_deadline)
        })
        .await
        .expect("a status in the settle window must survive the elapsed deadline");
        let output = output.unwrap();
        assert!(started.elapsed() >= timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_known_output(&output);
        wait_for_channel_closes(&evidence, 2).await;

        assert_eq!(evidence.commands.load(Ordering::SeqCst), 2);
        assert!(evidence.signals.lock().unwrap().is_empty());
        session.disconnect().await.unwrap();
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(server, client);
    }))
    .await
    .unwrap();
}

fn assert_known_output(output: &RemoteCommandOutput) {
    assert_eq!(output.exit_status, 23);
    assert_eq!(output.stdout, b"known stdout");
    assert_eq!(output.stderr, b"known stderr");
    assert!(!output.stdout_truncated);
    assert!(!output.stderr_truncated);
}

async fn wait_for_channel_closes(evidence: &KnownStatusEvidence, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let notified = evidence.channel_closed.notified();
            if evidence.channels_closed.load(Ordering::SeqCst) >= expected {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("client must close a channel after preserving its known result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_tcp_handshake_identity_load_and_userauth_path_succeeds() {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = HostKeyFingerprint::parse(
        host_key
            .public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string(),
    )
    .unwrap();
    let identity = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let identity_path = directory.path().join("identity-file-sentinel");
    std::fs::write(
        &identity_path,
        identity.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let peer = Peer {
        commands: Arc::clone(&commands),
        channels: HashMap::new(),
        authentication_started: None,
        authentication_gate: None,
    };
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..server::Config::default()
    });
    let server = async {
        let (socket, _) = listener.accept().await.unwrap();
        server::run_stream(config, socket, peer)
            .await
            .unwrap()
            .await
            .unwrap();
    };
    let client = async {
        let destination = LinuxSshDestination {
            driver: crate::drivers::DriverKind::linux_ssh(),
            host: address.ip().to_string(),
            port: address.port(),
            user: "deploy".into(),
            host_key: fingerprint,
        };
        let session = connect_authenticated(
            &destination,
            &SshCredential::IdentityFile {
                path: identity_path,
            },
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        session.disconnect().await.unwrap();
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, client);
    }))
    .await
    .unwrap();
    assert!(commands.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_userauth_uses_original_deadline_and_reports_only_static_phase() {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fingerprint = HostKeyFingerprint::parse(
        host_key
            .public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string(),
    )
    .unwrap();
    let identity = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let identity_path = directory.path().join("private-identity-path-sentinel");
    std::fs::write(
        &identity_path,
        identity.to_openssh(ssh_key::LineEnding::LF).unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let authentication_started = Arc::new(AtomicBool::new(false));
    let authentication_gate = Arc::new(tokio::sync::Notify::new());
    let peer = Peer {
        commands: Arc::new(Mutex::new(Vec::new())),
        channels: HashMap::new(),
        authentication_started: Some(Arc::clone(&authentication_started)),
        authentication_gate: Some(Arc::clone(&authentication_gate)),
    };
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..server::Config::default()
    });
    let handshake_delay = Duration::from_secs(1);
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        // TCP is established, but delaying the server protocol consumes the
        // shared budget specifically in the handshake/Host Key phase.
        tokio::time::sleep(handshake_delay).await;
        let running = server::run_stream(config, socket, peer).await.unwrap();
        let _ = running.await;
    });
    let destination = LinuxSshDestination {
        driver: crate::drivers::DriverKind::linux_ssh(),
        host: address.ip().to_string(),
        port: address.port(),
        user: "deploy-user-sentinel".into(),
        host_key: fingerprint,
    };
    let timeout = Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    let error = connect_authenticated(
        &destination,
        &SshCredential::IdentityFile {
            path: identity_path.clone(),
        },
        timeout,
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    let elapsed = started.elapsed();
    assert!(authentication_started.load(Ordering::SeqCst));
    assert!(matches!(
        &error,
        SshConnectionError::Timeout {
            timeout: budget,
            phase: USER_AUTHENTICATION_PHASE,
        } if *budget == timeout
    ));
    assert_eq!(
        error.to_string(),
        format!("SSH operation timed out after {timeout:?} during {USER_AUTHENTICATION_PHASE}")
    );
    let diagnostic = error.to_string();
    assert!(!diagnostic.contains("deploy-user-sentinel"));
    assert!(!diagnostic.contains(identity_path.to_string_lossy().as_ref()));
    assert!(elapsed >= handshake_delay);
    assert!(
        elapsed < timeout + Duration::from_millis(500),
        "userauth must not receive a fresh timeout after the delayed handshake"
    );
    // `notify_one` retains a permit if the handler was descheduled after
    // publishing `authentication_started` but before awaiting the gate.
    authentication_gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("stalled authentication server must stop after the client times out")
        .unwrap();
}
