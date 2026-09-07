//! Isolated password-only SSH traffic, including reuse after registry reload.

use super::*;
use shipforge::config::ProtectedPassword;

fn credential(password: &str) -> SshCredential {
    SshCredential::Password {
        protected: ProtectedPassword::protect(password).unwrap(),
    }
}

fn destination(port: u16, pin: &str, user: &str) -> LinuxSshDestination {
    LinuxSshDestination::validate(&DriverDestinationInput {
        value: serde_json::json!({"host":"127.0.0.1", "port":port, "user":user, "hostKey":pin}),
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn password_authentication_preserves_host_pin_cancellation_and_release_protocol() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pin = key
        .public_key()
        .fingerprint(ssh_key::HashAlg::Sha256)
        .to_string();
    let config = Arc::new(server::Config {
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        nodelay: true,
        keys: vec![key],
        ..server::Config::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut peer = ProtocolServer::default();
    let transfer = Arc::clone(&peer.transfer);
    transfer.lock().await.health_status = 204;
    let commands = Arc::clone(&peer.commands);
    let running = peer.run_on_socket(config, &listener);
    let shutdown = running.handle();
    let client = async {
        let cancellation = CancellationToken::new();
        let timeout = Duration::from_secs(5);
        let target = destination(port, &pin, "deploy");
        let password = "fixture 密码 q$' value";
        let secret = credential(password);
        let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let wrong_pin = other
            .public_key()
            .fingerprint(ssh_key::HashAlg::Sha256)
            .to_string();
        assert!(
            connect_authenticated(
                &destination(port, &wrong_pin, "deploy"),
                &secret,
                timeout,
                &cancellation
            )
            .await
            .is_err()
        );
        assert_eq!(
            transfer.lock().await.password_attempts,
            0,
            "never send password before pinned handshake"
        );
        let error = connect_authenticated(
            &target,
            &credential("wrong password"),
            timeout,
            &cancellation,
        )
        .await
        .unwrap_err();
        assert!(!format!("{error:?}").contains("wrong password"));
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(transfer.lock().await.password_attempts, 1);

        let directory = tempfile::tempdir().unwrap();
        let registry_path = directory.path().join("credentials.yaml");
        let mut registry = CredentialRegistry::new();
        let handle = registry.create(secret.clone()).unwrap();
        registry.save(&registry_path).unwrap();
        let loaded = CredentialRegistry::load(&registry_path).unwrap();
        let session = connect_authenticated(
            &target,
            loaded.resolve(&handle).unwrap(),
            timeout,
            &cancellation,
        )
        .await
        .unwrap();
        validate_exec(&session, &cancellation).await;
        validate_sudo(&session, &cancellation, &transfer).await;
        transfer.lock().await.fail_writes_remaining = 1;
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
        assert_eq!(activation.release().component.as_str(), "api");
        assert!(
            !commands
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains(password))
        );
        drop(session);

        let before = transfer.lock().await.password_attempts;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            connect_authenticated(&target, &secret, timeout, &cancelled)
                .await
                .is_err()
        );
        assert_eq!(transfer.lock().await.password_attempts, before);
        let slow = destination(port, &pin, "slow");
        let token = CancellationToken::new();
        let request = connect_authenticated(&slow, &secret, timeout, &token);
        let cancel_after_auth = async {
            while transfer.lock().await.password_attempts == before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            token.cancel();
        };
        let (result, ()) = tokio::join!(request, cancel_after_auth);
        assert!(matches!(
            result,
            Err(shipforge::drivers::linux_ssh::SshConnectionError::Cancelled)
        ));
        let result =
            connect_authenticated(&slow, &secret, Duration::from_millis(200), &cancellation).await;
        assert!(matches!(
            result,
            Err(shipforge::drivers::linux_ssh::SshConnectionError::Timeout { .. })
        ));
        shutdown.shutdown("password test complete".into());
    };
    let (result, ()) = Box::pin(tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(running, client)
    }))
    .await
    .unwrap();
    result.unwrap();
}

async fn validate_sudo(
    session: &AuthenticatedSession,
    cancellation: &CancellationToken,
    transfer: &Arc<AsyncMutex<TransferState>>,
) {
    let command = |action| {
        CommandSpec::structured(
            "/usr/bin/sudo",
            [
                "-S",
                "--",
                "/usr/bin/systemctl",
                action,
                "--",
                "fixture.service",
            ]
            .map(CommandArgument::plain),
        )
        .unwrap()
        .in_directory("/fixture/current")
        .unwrap()
    };
    let output = session
        .execute(&command("restart"), Duration::from_secs(5), cancellation)
        .await
        .unwrap();
    assert_eq!(output.exit_status, 0);
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "sudo echoes must not escape transport"
    );
    assert_eq!(transfer.lock().await.sudo_responses, 1);
    let denied = session
        .execute(&command("denied"), Duration::from_secs(5), cancellation)
        .await
        .unwrap();
    assert_eq!(denied.exit_status, 1);
    assert!(denied.stdout.is_empty() && denied.stderr.is_empty());
    assert_eq!(transfer.lock().await.sudo_responses, 2);
    let output = session
        .execute(&command("cached"), Duration::from_secs(5), cancellation)
        .await
        .unwrap();
    assert_eq!(output.exit_status, 0);
    assert_eq!(
        transfer.lock().await.sudo_responses,
        2,
        "no prompt means no password"
    );
    let result = session
        .execute(&command("stall"), Duration::from_millis(50), cancellation)
        .await;
    assert!(matches!(
        result,
        Err(shipforge::drivers::linux_ssh::SshConnectionError::Timeout { .. })
    ));
    let cancelled = CancellationToken::new();
    let stalled_command = command("stall");
    let task = session.execute(&stalled_command, Duration::from_secs(5), &cancelled);
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancelled.cancel();
    };
    let (result, ()) = tokio::join!(task, cancel);
    assert!(matches!(
        result,
        Err(shipforge::drivers::linux_ssh::SshConnectionError::Cancelled)
    ));
    assert_eq!(transfer.lock().await.sudo_responses, 2);
}
