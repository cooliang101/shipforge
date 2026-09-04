use std::{collections::VecDeque, sync::Mutex};

use super::*;
use crate::drivers::DriverTargetInput;

struct FakeRemote {
    responses: Mutex<VecDeque<RemoteCommandOutput>>,
    commands: Mutex<Vec<String>>,
}

impl FakeRemote {
    fn new(responses: Vec<RemoteCommandOutput>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            commands: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl PreflightRemote for FakeRemote {
    async fn command(
        &self,
        command: &CommandSpec,
        _: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        assert!(!cancellation.is_cancelled());
        self.commands
            .lock()
            .unwrap()
            .push(command.render_posix().unwrap());
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("test response"))
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

fn responses() -> Vec<RemoteCommandOutput> {
    vec![
        output(0, ""),
        output(0, "--extract --gzip --directory --no-same-owner"),
        output(0, "--symbolic"),
        output(0, "--no-clobber --no-target-directory"),
        output(0, "/srv\n"),
        output(0, "1048576:4096:131072:65536\n"),
    ]
}

fn target(systemd: Option<&str>, health: Option<&str>) -> LinuxSshTarget {
    LinuxSshTarget::validate(&DriverTargetInput {
        value: serde_json::json!({"root": "/srv/app", "systemd": systemd, "health": health}),
    })
    .unwrap()
}

async fn check(
    remote: &FakeRemote,
    target: &LinuxSshTarget,
) -> Result<Vec<String>, PreflightError> {
    check_with_remote(
        remote,
        target,
        Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await
}

#[tokio::test]
async fn files_only_preflight_checks_tools_features_parent_and_disk_without_mutation() {
    let remote = FakeRemote::new(responses());
    let notices = check(&remote, &target(None, None)).await.unwrap();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("4294967296 bytes"))
    );
    assert!(notices.iter().any(|notice| notice.contains("sha256sum")));
    let commands = remote.commands.lock().unwrap();
    assert_eq!(commands.len(), 6);
    assert!(commands[0].starts_with("'sh' '-c'"));
    assert!(!commands[0].contains("'systemctl'"));
    assert!(!commands[0].contains("'curl'"));
    assert!(commands[1].starts_with("'tar' '--help'"));
    assert!(commands[2].starts_with("'ln' '--help'"));
    assert!(commands[3].starts_with("'mv' '--help'"));
    assert!(commands[4].ends_with("'shipforge-preflight' '/srv/app'"));
    assert!(commands[5].starts_with("'stat' '--file-system'"));
}

#[tokio::test]
async fn configured_service_and_https_are_checked_without_restarting_or_requesting_url() {
    let mut replies = responses();
    replies.extend([
        output(0, "LoadState=loaded\n"),
        output(0, "curl 8\nProtocols: http https ftp\n"),
    ]);
    let remote = FakeRemote::new(replies);
    check(
        &remote,
        &target(
            Some("worker.service"),
            Some("https://localhost/health?secret=value"),
        ),
    )
    .await
    .unwrap();
    let commands = remote.commands.lock().unwrap();
    assert!(commands[0].contains("'systemctl' 'curl'"));
    assert_eq!(
        commands[6],
        "'systemctl' 'show' '--property=LoadState' '--' 'worker.service'"
    );
    assert_eq!(commands[7], "'curl' '--version'");
    assert!(!commands.join("\n").contains("secret=value"));
}

#[tokio::test]
async fn missing_commands_or_required_options_stop_before_filesystem_checks() {
    let missing = FakeRemote::new(vec![output(127, "")]);
    assert!(matches!(
        check(&missing, &target(None, None)).await,
        Err(PreflightError::Failed { .. })
    ));
    assert_eq!(missing.commands.lock().unwrap().len(), 1);
    let mut replies = responses();
    replies[3] = output(0, "--no-clobber");
    let unsupported = FakeRemote::new(replies);
    assert!(matches!(
        check(&unsupported, &target(None, None)).await,
        Err(PreflightError::UnsupportedOption {
            tool: "mv",
            option: "--no-target-directory"
        })
    ));
    assert_eq!(unsupported.commands.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn rejects_unwritable_parent_zero_space_and_malformed_capacity() {
    let mut replies = responses();
    replies[4] = output(1, "");
    assert!(matches!(
        check(&FakeRemote::new(replies), &target(None, None)).await,
        Err(PreflightError::Failed { .. })
    ));
    for capacity in ["0:4096:100:100\n", "100:4096:100:0\n"] {
        let mut replies = responses();
        replies[5] = output(0, capacity);
        assert!(matches!(
            check(&FakeRemote::new(replies), &target(None, None)).await,
            Err(PreflightError::NoSpace)
        ));
    }
    for capacity in [
        "bad\n",
        "1:0:10:10",
        "18446744073709551615:4096:1:1",
        "1:1:1:1:1",
    ] {
        let mut replies = responses();
        replies[5] = output(0, capacity);
        assert!(matches!(
            check(&FakeRemote::new(replies), &target(None, None)).await,
            Err(PreflightError::InvalidOutput(_))
        ));
    }
}

#[tokio::test]
async fn filesystem_without_inode_accounting_keeps_the_space_check() {
    let mut replies = responses();
    replies[5] = output(0, "1:4096:0:0\n");
    let notices = check(&FakeRemote::new(replies), &target(None, None))
        .await
        .unwrap();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("inode capacity is not reported"))
    );
}

#[tokio::test]
async fn rejects_missing_service_and_unsupported_https() {
    let mut replies = responses();
    replies.push(output(0, "LoadState=not-found\n"));
    assert!(matches!(
        check(
            &FakeRemote::new(replies),
            &target(Some("worker.service"), None)
        )
        .await,
        Err(PreflightError::ServiceNotLoaded)
    ));
    let mut replies = responses();
    replies.push(output(0, "Protocols: http ftp\n"));
    assert!(matches!(
        check(
            &FakeRemote::new(replies),
            &target(None, Some("https://localhost/health"))
        )
        .await,
        Err(PreflightError::HttpProtocol("https"))
    ));
}

#[tokio::test]
async fn rejects_truncated_output_instead_of_accepting_partial_help() {
    let mut replies = responses();
    replies[1].stdout_truncated = true;
    assert!(matches!(
        check(&FakeRemote::new(replies), &target(None, None)).await,
        Err(PreflightError::InvalidOutput("tar"))
    ));
}

#[tokio::test]
async fn cancellation_stops_before_any_remote_command() {
    let remote = FakeRemote::new(Vec::new());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        check_with_remote(
            &remote,
            &target(None, None),
            Duration::from_secs(1),
            &cancellation
        )
        .await,
        Err(PreflightError::Cancelled)
    ));
    assert!(remote.commands.lock().unwrap().is_empty());
}

#[tokio::test]
async fn root_is_passed_as_one_quoted_argument_not_inserted_into_shell_source() {
    let remote = FakeRemote::new(responses());
    let mut target = target(None, None);
    target.root = "/srv/app'; touch bad".into();
    check(&remote, &target).await.unwrap();
    let commands = remote.commands.lock().unwrap();
    assert!(commands[4].contains("'shipforge-preflight' '/srv/app'\\''; touch bad'"));
    assert!(!ROOT_CHECK.contains("touch"));
}
