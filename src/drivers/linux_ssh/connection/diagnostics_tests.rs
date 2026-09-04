//! Real SSH exec traffic over memory streams, without sockets or credential files.

use std::{collections::HashMap, sync::Mutex};

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
}

impl server::Handler for Peer {
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
