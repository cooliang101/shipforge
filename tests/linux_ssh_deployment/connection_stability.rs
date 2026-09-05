//! Read-only troubleshooting, deliberately excluded from deployment acceptance.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use shipforge::{
    config::SshCredential,
    drivers::linux_ssh::{LinuxSshDestination, SshConnectionError, connect_authenticated},
    telemetry::{CommandArgument, CommandSpec},
};
use tokio_util::sync::CancellationToken;

use super::{Fixture, required_env};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(180);
const ITERATIONS: usize = 100;
const EXPECTED_MARKER: &[u8] = b"shipforge-m1-disposable-v1\n";

#[tokio::test]
#[ignore = "diagnostic only: requires two independently pinned disposable Linux endpoints"]
async fn real_linux_fresh_connection_drop_stability() {
    tokio::time::timeout(TOTAL_TIMEOUT, Box::pin(run()))
        .await
        .expect("connection-stability total 180-second deadline exceeded; no retry");
}

async fn run() {
    let total_started = Instant::now();
    // Reuse opt-in checks, distinct loopback ports, independent pinned Host Keys
    // and marker attestations. Fixture setup creates local files only.
    let fixture = Fixture::new().await;
    let credential = SshCredential::IdentityFile {
        path: PathBuf::from(required_env("SHIPFORGE_TEST_KEY")),
    };
    let marker = CommandSpec::structured(
        "cat",
        [CommandArgument::plain("/etc/shipforge-disposable-fixture")],
    )
    .expect("fixed marker command must be valid");
    for index in 1..=ITERATIONS {
        verify_iteration(&fixture, &credential, &marker, index, total_started).await;
    }
    println!(
        "connection-stability phase=complete successful={ITERATIONS} total_ms={}",
        total_started.elapsed().as_millis(),
    );
}

async fn verify_iteration(
    fixture: &Fixture,
    credential: &SshCredential,
    marker: &CommandSpec,
    index: usize,
    total_started: Instant,
) {
    let (component, endpoint) = if index % 2 == 1 {
        ("frontend", "A")
    } else {
        ("backend", "B")
    };
    let context = fixture.context(component);
    let destination = context
        .destination_settings
        .as_any()
        .downcast_ref::<LinuxSshDestination>()
        .expect("fixture must use the Linux SSH Driver");
    let cancellation = CancellationToken::new();
    let connect_started = Instant::now();
    progress(
        index,
        endpoint,
        "connect-start",
        connect_started,
        total_started,
    );
    let session = connect_authenticated(destination, credential, CONNECTION_TIMEOUT, &cancellation)
        .await
        .unwrap_or_else(|error| {
            fail(
                index,
                endpoint,
                "connect",
                &error,
                connect_started,
                total_started,
            )
        });
    progress(index, endpoint, "connected", connect_started, total_started);
    let read_started = Instant::now();
    progress(index, endpoint, "marker-start", read_started, total_started);
    let output = session
        .execute(marker, CONNECTION_TIMEOUT, &cancellation)
        .await
        .unwrap_or_else(|error| {
            fail(
                index,
                endpoint,
                "marker",
                &error,
                read_started,
                total_started,
            )
        });
    let valid = output.exit_status == 0
        && output.stdout == EXPECTED_MARKER
        && output.stderr.is_empty()
        && !output.stdout_truncated
        && !output.stderr_truncated;
    assert!(
        valid,
        "connection-stability index={index:03} endpoint={endpoint} phase=marker-invalid elapsed_ms={} total_ms={}; no retry",
        read_started.elapsed().as_millis(),
        total_started.elapsed().as_millis(),
    );
    progress(
        index,
        endpoint,
        "marker-verified",
        read_started,
        total_started,
    );
    // Match the production Driver's session lifetime: do not send an
    // explicit disconnect or reuse a handle in the next iteration.
    drop(session);
    progress(index, endpoint, "dropped", connect_started, total_started);
}

fn progress(index: usize, endpoint: &str, phase: &str, started: Instant, total_started: Instant) {
    println!(
        "connection-stability index={index:03} endpoint={endpoint} phase={phase} elapsed_ms={} total_ms={}",
        started.elapsed().as_millis(),
        total_started.elapsed().as_millis(),
    );
}

fn fail(
    index: usize,
    endpoint: &str,
    phase: &str,
    error: &SshConnectionError,
    started: Instant,
    total_started: Instant,
) -> ! {
    // Never display protocol text, paths, agent identities or remote output.
    let category = match error {
        SshConnectionError::Timeout {
            phase: "TCP connection",
            ..
        } => "tcp-timeout",
        SshConnectionError::Timeout {
            phase: "SSH handshake and Host Key verification",
            ..
        } => "handshake-or-host-key-timeout",
        SshConnectionError::Timeout {
            phase: "IdentityFile credential loading",
            ..
        } => "identity-file-timeout",
        SshConnectionError::Timeout {
            phase: "SSH Agent credential loading",
            ..
        } => "agent-timeout",
        SshConnectionError::Timeout {
            phase: "SSH user authentication",
            ..
        } => "userauth-timeout",
        SshConnectionError::Timeout { .. } => "operation-timeout",
        SshConnectionError::Cancelled => "cancelled",
        SshConnectionError::Protocol(_) => "protocol-or-host-key",
        SshConnectionError::Rejected => "authentication-rejected",
        SshConnectionError::Key(_)
        | SshConnectionError::Agent(_)
        | SshConnectionError::MissingAgentIdentity(_)
        | SshConnectionError::UnsupportedCertificate
        | SshConnectionError::UnsupportedKeyAlgorithm(_) => "credential",
        SshConnectionError::Command(_)
        | SshConnectionError::ExitSignal { .. }
        | SshConnectionError::MissingExitStatus => "command",
    };
    panic!(
        "connection-stability index={index:03} endpoint={endpoint} phase={phase} failure={category} elapsed_ms={} total_ms={}; no retry",
        started.elapsed().as_millis(),
        total_started.elapsed().as_millis(),
    );
}
