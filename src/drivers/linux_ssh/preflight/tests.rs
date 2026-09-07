use std::{collections::VecDeque, sync::Mutex};

use super::*;
use crate::drivers::DriverTargetInput;

const DESCRIPTOR_INDEX: usize = 7;
const RETENTION_INDEX: usize = 8;
const ROOT_INDEX: usize = 9;
const CAPACITY_INDEX: usize = 10;

struct FakeRemote {
    responses: Mutex<VecDeque<RemoteCommandOutput>>,
    commands: Mutex<Vec<String>>,
    missing_tool: Option<&'static str>,
    cancel_at: Option<usize>,
}

impl FakeRemote {
    fn new(responses: Vec<RemoteCommandOutput>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            commands: Mutex::new(Vec::new()),
            missing_tool: None,
            cancel_at: None,
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
        if self.cancel_at == Some(self.commands.lock().unwrap().len() - 1) {
            cancellation.cancel();
            return Err(SshConnectionError::Cancelled);
        }
        if let Some(missing) = self.missing_tool
            && command.program == "sh"
            && command
                .args
                .iter()
                .any(|argument| argument.expose_for_execution() == missing)
        {
            return Ok(output(127, ""));
        }
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
        output(0, "--kill-after=DURATION --signal=SIGNAL"),
        output(0, "oflag=FLAGS conv=CONVS status=LEVEL append notrunc none"),
        output(0, "--recursive --one-file-system --preserve-root[=all]"),
        output(0, ""),
        output(0, RETENTION_CHECK_OUTPUT),
        output(0, "/srv\n"),
        output(0, "1048576:4096:131072:65536\n"),
    ]
}

fn target(systemd: Option<&str>, health: Option<&str>) -> LinuxSshTarget {
    LinuxSshTarget::validate(&DriverTargetInput {
        value: serde_json::json!({"root": "/srv/app", "service": systemd.map(crate::config::ServiceConfig::systemd), "health": health}),
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
async fn custom_service_probes_only_executable_availability_without_systemd_or_mutation() {
    let service = crate::config::ServiceConfig {
        start: vec![vec!["node".into(), "service.cjs".into(), "activate".into()]],
        stop: vec![vec!["pm2".into(), "delete".into(), "api".into()]],
        update: Vec::new(),
        restore: Vec::new(),
        check: Some(crate::config::ServiceCheck::Command {
            argv: vec!["node".into(), "check.cjs".into()],
        }),
    };
    let mut target = target(None, None);
    target.service = Some(service);
    let remote = FakeRemote::new(responses());
    check(&remote, &target).await.unwrap();
    {
        let commands = remote.commands.lock().unwrap();
        assert!(commands[0].contains("'node'") && commands[0].contains("'pm2'"));
        assert!(!commands.iter().any(|line| line.contains("systemctl")
            || line.contains("service.cjs")
            || line.contains("'delete'")));
    }
    let mut remote = FakeRemote::new(responses());
    remote.missing_tool = Some("node");
    assert!(check(&remote, &target).await.is_err());
    assert_eq!(remote.commands.lock().unwrap().len(), 1);
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
    assert_eq!(commands.len(), 11);
    assert!(commands[0].starts_with("'sh' '-c'"));
    assert!(!commands[0].contains("'systemctl'"));
    assert!(!commands[0].contains("'curl'"));
    assert!(commands[1].starts_with("'tar' '--help'"));
    assert!(commands[2].starts_with("'ln' '--help'"));
    assert!(commands[3].starts_with("'mv' '--help'"));
    assert!(commands[0].contains("'timeout' 'dd'"));
    assert_eq!(commands[4], "'timeout' '--help'");
    assert_eq!(commands[5], "'dd' '--help'");
    assert_eq!(commands[6], "'rm' '--help'");
    assert!(commands[DESCRIPTOR_INDEX].contains(DESCRIPTOR_CHECK));
    assert!(commands[RETENTION_INDEX].starts_with(
        "'timeout' '--signal=TERM' '--kill-after=1s' '5s' 'bash' '-o' 'pipefail' '-c'"
    ));
    assert!(commands[ROOT_INDEX].ends_with("'shipforge-preflight' '/srv/app'"));
    assert!(commands[CAPACITY_INDEX].starts_with("'stat' '--file-system'"));
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
        commands[11],
        "'systemctl' 'show' '--property=LoadState' '--' 'worker.service'"
    );
    assert_eq!(commands[12], "'curl' '--version'");
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
async fn missing_audit_or_timeout_tool_stops_before_feature_or_filesystem_checks() {
    for tool in ["timeout", "dd", "bash", "awk", "sed"] {
        let mut remote = FakeRemote::new(Vec::new());
        remote.missing_tool = Some(tool);
        assert!(matches!(
            check(&remote, &target(None, None)).await,
            Err(PreflightError::Failed {
                stage: "required commands",
                status: 127
            })
        ));
        assert_eq!(remote.commands.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn every_required_timeout_and_append_flag_is_checked_before_filesystem_checks() {
    for (index, tool, options) in [
        (4, "timeout", &["--kill-after", "--signal"][..]),
        (
            5,
            "dd",
            &["oflag=", "conv=", "status=", "append", "notrunc", "none"][..],
        ),
        (
            6,
            "rm",
            &["--recursive", "--one-file-system", "--preserve-root[=all]"][..],
        ),
    ] {
        for option in options {
            let mut replies = responses();
            let incomplete = String::from_utf8(replies[index].stdout.clone())
                .unwrap()
                .replace(option, "");
            replies[index] = output(0, &incomplete);
            let remote = FakeRemote::new(replies);
            assert!(
                matches!(check(&remote,&target(None,None)).await,Err(PreflightError::UnsupportedOption{tool:actual,option:absent}) if actual==tool && absent==*option)
            );
            assert_eq!(remote.commands.lock().unwrap().len(), index + 1);
        }
    }
}

#[tokio::test]
async fn descriptor_filesystem_requires_successful_empty_read_only_probe() {
    for reply in [output(1, ""), output(0, "unexpected output")] {
        let mut replies = responses();
        replies[DESCRIPTOR_INDEX] = reply;
        let remote = FakeRemote::new(replies);
        assert!(check(&remote, &target(None, None)).await.is_err());
        let commands = remote.commands.lock().unwrap();
        assert_eq!(commands.len(), DESCRIPTOR_INDEX + 1);
        assert!(commands[DESCRIPTOR_INDEX].contains("exec 3< /proc/self/status"));
        assert!(commands[DESCRIPTOR_INDEX].contains("head -c 1 /proc/self/fd/3 >/dev/null"));
        assert!(!commands[DESCRIPTOR_INDEX].contains("mkdir"));
    }
}

#[tokio::test]
async fn rejects_unwritable_parent_zero_space_and_malformed_capacity() {
    let mut replies = responses();
    replies[ROOT_INDEX] = output(1, "");
    assert!(matches!(
        check(&FakeRemote::new(replies), &target(None, None)).await,
        Err(PreflightError::Failed { .. })
    ));
    for capacity in ["0:4096:100:100\n", "100:4096:100:0\n"] {
        let mut replies = responses();
        replies[CAPACITY_INDEX] = output(0, capacity);
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
        replies[CAPACITY_INDEX] = output(0, capacity);
        assert!(matches!(
            check(&FakeRemote::new(replies), &target(None, None)).await,
            Err(PreflightError::InvalidOutput(_))
        ));
    }
}

#[tokio::test]
async fn filesystem_without_inode_accounting_keeps_the_space_check() {
    let mut replies = responses();
    replies[CAPACITY_INDEX] = output(0, "1:4096:0:0\n");
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
    assert!(commands[ROOT_INDEX].contains("'shipforge-preflight' '/srv/app'\\''; touch bad'"));
    assert!(!ROOT_CHECK.contains("touch"));
}

#[tokio::test]
async fn retention_semantic_probe_requires_exact_complete_output_before_filesystem_checks() {
    for mut reply in [
        output(1, ""),
        output(124, ""),
        output(137, ""),
        output(0, ""),
        output(0, "wrong\n"),
        output(0, RETENTION_CHECK_OUTPUT),
    ] {
        if reply.stdout == RETENTION_CHECK_OUTPUT.as_bytes() {
            reply.stderr_truncated = true;
        }
        let mut replies = responses();
        replies[RETENTION_INDEX] = reply;
        let remote = FakeRemote::new(replies);
        assert!(check(&remote, &target(None, None)).await.is_err());
        assert_eq!(remote.commands.lock().unwrap().len(), RETENTION_INDEX + 1);
    }
}

#[tokio::test]
async fn cancellation_during_retention_probe_stops_before_capacity_or_service_checks() {
    let mut remote = FakeRemote::new(responses());
    remote.cancel_at = Some(RETENTION_INDEX);
    assert!(matches!(
        check(&remote, &target(None, None)).await,
        Err(PreflightError::Connection(SshConnectionError::Cancelled))
    ));
    assert_eq!(remote.commands.lock().unwrap().len(), RETENTION_INDEX + 1);
}

#[test]
fn retention_probe_has_only_fixed_read_only_paths_and_tests_the_required_semantics() {
    for required in [
        "false | true",
        "exec 3< /",
        "/proc/self/mountinfo",
        "-maxdepth 0 -xdev -printf",
        "stat -L",
        "head -c 1",
        "sed -e",
        "awk -F:",
    ] {
        assert!(RETENTION_CHECK.contains(required));
    }
    // awk's $1 is a field reference, not a Shell argument or a dynamic path.
    for forbidden in [
        "mkdir", "rm ", "touch", "chmod", "\"$1\"", ">/", "> /", ">>",
    ] {
        assert!(!RETENTION_CHECK.contains(forbidden), "{forbidden}");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn retention_semantic_probe_runs_on_linux_without_creating_files() {
    let output = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::process::Command::new("timeout")
            .args([
                "--signal=TERM",
                "--kill-after=1s",
                "5s",
                "bash",
                "-o",
                "pipefail",
                "-c",
                RETENTION_CHECK,
                "shipforge-preflight-retention",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, RETENTION_CHECK_OUTPUT.as_bytes());
    assert!(output.stderr.is_empty(), "{output:?}");
}
