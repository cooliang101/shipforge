use std::sync::Mutex;

use super::*;

#[tokio::test]
async fn actual_failed_spawn_reports_frozen_second_command_after_both_stream_ends() {
    let directory = tempfile::tempdir().unwrap();
    let commands = [
        BuildCommand::argv("rustc", ["--version"]),
        BuildCommand::argv(
            "shipforge-event-test-no-such-executable",
            ["literal ; argument"],
        ),
    ];
    let ended = Mutex::new(Vec::new());
    let failures = Mutex::new(Vec::new());
    let result = run_build_with_events(
        directory.path(),
        Path::new("."),
        &commands,
        true,
        Duration::from_secs(5),
        &CancellationToken::new(),
        &|index, stream, bytes| {
            if bytes.is_empty() {
                ended.lock().unwrap().push((index, stream));
            }
        },
        &|index, command| {
            assert!(
                ended
                    .lock()
                    .unwrap()
                    .contains(&(index, crate::adapters::OutputStream::Stdout))
            );
            assert!(
                ended
                    .lock()
                    .unwrap()
                    .contains(&(index, crate::adapters::OutputStream::Stderr))
            );
            failures.lock().unwrap().push((index, command.clone()));
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(BuildError::Process(ProcessError::Spawn(_)))
    ));
    assert_eq!(*failures.lock().unwrap(), [(1, commands[1].clone())]);
    let snapshot = command_snapshot(1, &commands[1]);
    assert_eq!(snapshot.index, Some(2));
    assert_eq!(snapshot.program, commands[1].program);
    assert_eq!(snapshot.args, ["literal ; argument"]);
}

#[tokio::test]
async fn failed_preflight_never_invents_a_build_command_failure() {
    let directory = tempfile::tempdir().unwrap();
    let result = run_build_with_events(
        directory.path(),
        Path::new("missing"),
        &[BuildCommand::argv("rustc", ["--version"])],
        true,
        Duration::from_secs(5),
        &CancellationToken::new(),
        &|_, _, _| {},
        &|_, _| panic!("no configured command was attempted"),
    )
    .await;
    assert!(matches!(result, Err(BuildError::Path { .. })));
}

#[test]
fn all_process_io_failures_report_the_exact_loop_command() {
    let command = BuildCommand::argv("fixture", ["not another command"]);
    for error in [
        ProcessError::Spawn(std::io::Error::other("spawn")),
        ProcessError::MissingPipe("stderr"),
        ProcessError::Wait(std::io::Error::other("wait")),
        ProcessError::Kill(std::io::Error::other("kill")),
        ProcessError::Output(std::io::Error::other("read")),
    ] {
        let failures = Mutex::new(Vec::new());
        observe_command_failure(7, &command, &Err(error), &|index, attempted| {
            failures.lock().unwrap().push((index, attempted.clone()));
        });
        assert_eq!(*failures.lock().unwrap(), [(7, command.clone())]);
    }
}

#[test]
fn nonzero_cancelled_and_timed_out_results_are_failures_but_zero_exit_is_not() {
    for (termination, exit_code, failed) in [
        (ProcessTermination::Exited, Some(0), false),
        (ProcessTermination::Exited, Some(2), true),
        (ProcessTermination::Exited, None, true),
        (ProcessTermination::Cancelled, None, true),
        (ProcessTermination::TimedOut, None, true),
    ] {
        let calls = Mutex::new(Vec::new());
        observe_command_failure(
            3,
            &BuildCommand::argv("program", ["arg"]),
            &Ok(ProcessOutput {
                termination,
                exit_code,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
            }),
            &|index, _| calls.lock().unwrap().push(index),
        );
        assert_eq!(!calls.lock().unwrap().is_empty(), failed);
    }
}

#[test]
fn legacy_shell_snapshot_records_the_actual_platform_interpreter() {
    let command = command_snapshot(0, &BuildCommand::shell("exit 7"));
    #[cfg(windows)]
    {
        assert_eq!(command.program, "cmd.exe");
        assert_eq!(command.args, ["/D", "/S", "/C", "exit 7"]);
    }
    #[cfg(not(windows))]
    {
        assert_eq!(command.program, "sh");
        assert_eq!(command.args, ["-c", "exit 7"]);
    }
}
