use std::{collections::VecDeque, sync::Mutex};

use super::*;
use crate::drivers::DriverTargetInput;

#[derive(Debug)]
struct FakeRemote {
    responses: Mutex<VecDeque<Result<RemoteCommandOutput, SshConnectionError>>>,
    commands: Mutex<Vec<String>>,
}

impl FakeRemote {
    fn new(
        responses: impl IntoIterator<Item = Result<RemoteCommandOutput, SshConnectionError>>,
    ) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            commands: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait]
impl HealthRemote for FakeRemote {
    async fn command(
        &self,
        command: &CommandSpec,
        _: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        if cancellation.is_cancelled() {
            return Err(SshConnectionError::Cancelled);
        }
        if command.program == "curl" {
            let debug = format!("{command:?}");
            assert!(!debug.contains("ready=1"));
            assert!(debug.contains("[REDACTED]"));
        }
        self.commands
            .lock()
            .unwrap()
            .push(command.render_posix().unwrap());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test response")
    }
}

fn output(status: u32, stdout: &str) -> RemoteCommandOutput {
    RemoteCommandOutput {
        exit_status: status,
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

fn target(systemd: Option<&str>, health: Option<&str>) -> LinuxSshTarget {
    LinuxSshTarget::validate(&DriverTargetInput {
        value: serde_json::json!({
            "root": "/srv/app",
            "service": systemd.map(crate::config::ServiceConfig::systemd),
            "health": health,
        }),
    })
    .unwrap()
}

fn options() -> HealthCheckOptions {
    HealthCheckOptions {
        command_timeout: Duration::from_millis(50),
        interval: Duration::from_millis(1),
        attempts: 3,
        stable_for: Duration::from_millis(1),
    }
}

#[tokio::test]
async fn custom_check_retries_in_current_directory_without_systemd() {
    let mut target = target(None, None);
    let mut service = crate::config::ServiceConfig::systemd("api.service");
    service.check = Some(crate::config::ServiceCheck::Command {
        argv: vec!["node".into(), "check.cjs".into()],
    });
    target.service = Some(service);
    let remote = FakeRemote::new([Ok(output(1, "not ready")), Ok(output(0, "ready"))]);
    let report = check_with_remote(&remote, &target, options(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.command_attempts, Some(2));
    assert!(report.systemd.is_none() && report.http.is_none());
    assert_eq!(
        remote.commands(),
        vec!["cd -- '/srv/app/current' && exec 'node' 'check.cjs'"; 2]
    );
    let remote = FakeRemote::new([Err(SshConnectionError::Cancelled)]);
    assert!(matches!(
        check_with_remote(&remote, &target, options(), &CancellationToken::new()).await,
        Err(HealthCheckError::Cancelled)
    ));
    assert_eq!(remote.commands().len(), 1);
    let remote = FakeRemote::new((0..3).map(|_| Ok(output(1, "private-output"))));
    let error = check_with_remote(&remote, &target, options(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, HealthCheckError::ServiceProbeFailed));
    assert!(!error.to_string().contains("private-output"));
    assert_eq!(remote.commands().len(), 3);
}

#[tokio::test]
async fn checks_systemd_stability_then_http_from_destination() {
    let remote = FakeRemote::new([
        Ok(output(0, "ActiveState=active\nNRestarts=4\n")),
        Ok(output(0, "NRestarts=4\nActiveState=active\n")),
        Ok(output(0, "204")),
    ]);
    let report = check_with_remote(
        &remote,
        &target(
            Some("api.service"),
            Some("http://127.0.0.1:8080/health?ready=1"),
        ),
        options(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.systemd.unwrap().restart_baseline, 4);
    assert_eq!(report.http.unwrap().status, 204);
    let commands = remote.commands();
    assert!(commands[0].starts_with("'systemctl' 'show'"));
    assert!(commands[2].starts_with("'curl' '--silent'"));
    assert!(commands[2].ends_with("'http://127.0.0.1:8080/health?ready=1'"));
}

#[tokio::test]
async fn waits_for_active_before_starting_stability_baseline() {
    let remote = FakeRemote::new([
        Ok(output(0, "ActiveState=activating\nNRestarts=2\n")),
        Ok(output(0, "ActiveState=active\nNRestarts=3\n")),
        Ok(output(0, "ActiveState=active\nNRestarts=3\n")),
    ]);
    let report = check_with_remote(
        &remote,
        &target(Some("worker.service"), None),
        options(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let systemd = report.systemd.unwrap();
    assert_eq!(systemd.restart_baseline, 3);
    assert_eq!(systemd.observations, 3);
}

#[tokio::test]
async fn restart_increase_fails_the_stability_window() {
    let remote = FakeRemote::new([
        Ok(output(0, "ActiveState=active\nNRestarts=1\n")),
        Ok(output(0, "ActiveState=active\nNRestarts=2\n")),
    ]);
    let error = check_with_remote(
        &remote,
        &target(Some("worker.service"), None),
        options(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        HealthCheckError::SystemdUnstable {
            baseline: 1,
            observed: 2,
            ..
        }
    ));
}

#[tokio::test]
async fn transient_systemd_command_failure_is_retried() {
    let remote = FakeRemote::new([
        Ok(output(0, "ActiveState=active\nNRestarts=1\n")),
        Err(SshConnectionError::Protocol(
            "temporary channel failure".into(),
        )),
        Ok(output(0, "ActiveState=active\nNRestarts=1\n")),
    ]);
    let report = check_with_remote(
        &remote,
        &target(Some("worker.service"), None),
        options(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.systemd.unwrap().restart_baseline, 1);
    assert_eq!(remote.commands().len(), 3);
}

#[tokio::test]
async fn http_retries_until_a_two_xx_response() {
    let remote = FakeRemote::new([
        Ok(output(0, "503")),
        Ok(output(7, "000")),
        Ok(output(0, "200")),
    ]);
    let report = check_with_remote(
        &remote,
        &target(None, Some("https://internal.example/ready")),
        options(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.http.unwrap().attempts, 3);
}

#[tokio::test]
async fn cancellation_and_invalid_options_stop_before_remote_work() {
    let remote = FakeRemote::new([]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        check_with_remote(&remote, &target(None, None), options(), &cancellation).await,
        Err(HealthCheckError::Cancelled)
    ));
    let mut invalid = options();
    invalid.attempts = 0;
    assert!(matches!(
        check_with_remote(
            &remote,
            &target(None, None),
            invalid,
            &CancellationToken::new()
        )
        .await,
        Err(HealthCheckError::ZeroOption)
    ));
    assert!(remote.commands().is_empty());
}

#[tokio::test]
async fn remote_diagnostics_are_bounded_and_single_line() {
    let responses = (0..3).map(|_| {
        Err(SshConnectionError::Protocol(format!(
            "bad\n{}",
            "x".repeat(4096)
        )))
    });
    let remote = FakeRemote::new(responses);
    let error = check_with_remote(
        &remote,
        &target(None, Some("http://localhost/health")),
        options(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(!error.contains('\n'));
    assert!(error.len() < 1200);
}

#[test]
fn parsers_reject_truncated_or_ambiguous_output() {
    assert_eq!(
        parse_systemd_state(b"NRestarts=7\nActiveState=active\n"),
        Some(SystemdState {
            active: true,
            restarts: 7,
        })
    );
    assert_eq!(parse_systemd_state(b"ActiveState=active\n"), None);
    let mut response = output(0, "200");
    response.stdout_truncated = true;
    assert_eq!(parse_http_status(&response), None);
    response.stdout_truncated = false;
    response.stdout = b"200 extra".to_vec();
    assert_eq!(parse_http_status(&response), None);
}
