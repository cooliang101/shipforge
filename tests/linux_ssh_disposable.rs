use std::{path::PathBuf, time::Duration};

use shipforge::{
    config::{HostKeyFingerprint, SshCredential},
    drivers::{
        DriverDestinationInput,
        linux_ssh::{
            LinuxSshDestination, capture_host_key, connect_authenticated, probe_remote_setup,
        },
    },
    telemetry::{CommandArgument, CommandSpec},
};
use tokio_util::sync::CancellationToken;

/// Runs only against an explicitly opted-in, loopback-only disposable Linux
/// SSH server. The test is read-only and never discovers saved Destinations.
#[tokio::test]
#[ignore = "requires an explicit disposable loopback Linux SSH server"]
async fn validates_linux_ssh_setup_contract_on_disposable_server() {
    assert_eq!(required_env("SHIPFORGE_DISPOSABLE_SSH"), "1");
    let host = required_env("SHIPFORGE_TEST_SSH_HOST");
    assert!(
        matches!(host.as_str(), "127.0.0.1" | "::1" | "localhost"),
        "integration test refuses non-loopback SSH host"
    );
    let port = required_env("SHIPFORGE_TEST_SSH_PORT")
        .parse::<u16>()
        .expect("SHIPFORGE_TEST_SSH_PORT must be a non-zero u16");
    assert_ne!(port, 0);
    let user = required_env("SHIPFORGE_TEST_SSH_USER");
    let expected_host_key = required_env("SHIPFORGE_TEST_SSH_HOST_KEY");
    let identity_file = PathBuf::from(required_env("SHIPFORGE_TEST_SSH_IDENTITY_FILE"));
    assert!(identity_file.is_absolute());

    let cancellation = CancellationToken::new();
    let captured = capture_host_key(&host, port, Duration::from_secs(10), &cancellation)
        .await
        .expect("capture disposable Host Key");
    assert_eq!(captured.as_str(), expected_host_key);
    let destination = LinuxSshDestination::validate(&DriverDestinationInput {
        value: serde_json::json!({
            "host": host,
            "port": port,
            "user": user,
            "hostKey": HostKeyFingerprint::parse(expected_host_key).unwrap().as_str(),
        }),
    })
    .expect("validate disposable Destination");
    let session = connect_authenticated(
        &destination,
        &SshCredential::IdentityFile {
            path: identity_file,
        },
        Duration::from_secs(10),
        &cancellation,
    )
    .await
    .expect("authenticate to disposable server");

    let sentinel = "literal;$(not-executed) ' value";
    let output = session
        .execute(
            &CommandSpec::structured("printf", [CommandArgument::plain(sentinel)]).unwrap(),
            Duration::from_secs(10),
            &cancellation,
        )
        .await
        .expect("execute quoted read-only command");
    assert_eq!(output.exit_status, 0);
    assert_eq!(output.stdout, sentinel.as_bytes());
    assert!(output.stderr.is_empty());

    let candidates = probe_remote_setup(
        &session,
        "/tmp/shipforge-disposable-contract",
        Duration::from_secs(10),
        &cancellation,
    )
    .await
    .expect("probe disposable server");
    assert!(candidates.systemd_units.len() <= 200);
    session.disconnect().await.expect("disconnect cleanly");
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set explicitly"))
}
