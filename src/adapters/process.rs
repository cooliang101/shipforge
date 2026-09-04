use std::{fmt, path::Path, process::Stdio, time::Duration};

use command_group::AsyncCommandGroup;
use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

use crate::config::BuildCommand;

const MAX_RETAINED_STREAM_BYTES: usize = 256 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

struct ProcessGroup(command_group::AsyncGroupChild);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The crate's kill_on_drop is a Windows Job Object option. Explicitly
        // signal the POSIX group too when this operation is dropped or unwinds.
        let _ = self.0.start_kill();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessTermination {
    Exited,
    Cancelled,
    TimedOut,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub termination: ProcessTermination,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl fmt::Debug for ProcessOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessOutput")
            .field("termination", &self.termination)
            .field("exit_code", &self.exit_code)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .finish()
    }
}

/// Runs one normalized build command in a cross-platform process group.
///
/// Both streams are drained concurrently. Cancellation, timeout, or dropping
/// the operation terminates the tree via a POSIX process group or Windows Job Object.
///
/// # Errors
///
/// Returns an error for spawn, pipe, wait, termination, or output I/O failure.
pub async fn run_grouped(
    command: &BuildCommand,
    working_directory: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    run_grouped_with_output(
        command,
        working_directory,
        timeout,
        cancellation,
        &|_, _| {},
    )
    .await
}

/// Runs a grouped process and observes both output streams as they are drained.
/// The observer must be bounded and must not panic or wait for UI consumption.
///
/// # Errors
/// Returns the same process/pipe errors as `run_grouped`.
pub async fn run_grouped_with_output(
    command: &BuildCommand,
    working_directory: &Path,
    timeout: Duration,
    cancellation: &CancellationToken,
    observe: &(dyn Fn(OutputStream, &[u8]) + Sync),
) -> Result<ProcessOutput, ProcessError> {
    if cancellation.is_cancelled() {
        return Ok(cancelled_output());
    }
    let mut process = platform_command(command);
    process
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut group = process.group();
    group.kill_on_drop(true);
    let mut child = ProcessGroup(group.spawn().map_err(ProcessError::Spawn)?);
    let mut stdout = child
        .0
        .inner()
        .stdout
        .take()
        .ok_or(ProcessError::MissingPipe("stdout"))?;
    let mut stderr = child
        .0
        .inner()
        .stderr
        .take()
        .ok_or(ProcessError::MissingPipe("stderr"))?;
    collect_output(
        &mut child.0,
        &mut stdout,
        &mut stderr,
        timeout,
        cancellation,
        observe,
    )
    .await
}

async fn collect_output(
    child: &mut command_group::AsyncGroupChild,
    stdout: &mut (impl tokio::io::AsyncRead + Unpin),
    stderr: &mut (impl tokio::io::AsyncRead + Unpin),
    timeout: Duration,
    cancellation: &CancellationToken,
    observe: &(dyn Fn(OutputStream, &[u8]) + Sync),
) -> Result<ProcessOutput, ProcessError> {
    let stdout_observer = |bytes: &[u8]| observe(OutputStream::Stdout, bytes);
    let stderr_observer = |bytes: &[u8]| observe(OutputStream::Stderr, bytes);
    let mut captured_stdout = CapturedStream::default();
    let mut captured_stderr = CapturedStream::default();
    let completed = tokio::select! {
        result = wait_and_drain(
            child,
            stdout,
            stderr,
            &mut captured_stdout,
            &mut captured_stderr,
            &stdout_observer,
            &stderr_observer,
        ) => Some(result),
        () = cancellation.cancelled() => None,
        () = tokio::time::sleep(timeout) => None,
    };
    let (termination, exit_code) = if let Some(result) = completed {
        (ProcessTermination::Exited, result?)
    } else {
        let termination = if cancellation.is_cancelled() {
            ProcessTermination::Cancelled
        } else {
            ProcessTermination::TimedOut
        };
        start_termination(child)?;
        let drained = tokio::time::timeout(
            TERMINATION_GRACE,
            wait_and_drain(
                child,
                stdout,
                stderr,
                &mut captured_stdout,
                &mut captured_stderr,
                &stdout_observer,
                &stderr_observer,
            ),
        )
        .await;
        captured_stdout.truncated |= !captured_stdout.eof;
        captured_stderr.truncated |= !captured_stderr.eof;
        let exit_code = match drained {
            Ok(result) => result?,
            Err(_) => child
                .inner()
                .try_wait()
                .map_err(ProcessError::Wait)?
                .and_then(|status| status.code()),
        };
        (termination, exit_code)
    };
    Ok(ProcessOutput {
        termination,
        exit_code,
        stdout: captured_stdout.bytes,
        stderr: captured_stderr.bytes,
        stdout_truncated: captured_stdout.truncated,
        stderr_truncated: captured_stderr.truncated,
    })
}

fn platform_command(command: &BuildCommand) -> Command {
    if command.shell {
        #[cfg(windows)]
        let process = {
            let mut process = Command::new("cmd.exe");
            process.args(["/D", "/S", "/C", command.program.as_str()]);
            process
        };
        #[cfg(not(windows))]
        let process = {
            let mut process = Command::new("sh");
            process.args(["-c", command.program.as_str()]);
            process
        };
        process
    } else {
        let mut process = Command::new(&command.program);
        process.args(&command.args);
        process
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_and_drain(
    child: &mut command_group::AsyncGroupChild,
    stdout: &mut (impl tokio::io::AsyncRead + Unpin),
    stderr: &mut (impl tokio::io::AsyncRead + Unpin),
    captured_stdout: &mut CapturedStream,
    captured_stderr: &mut CapturedStream,
    stdout_observer: &(dyn Fn(&[u8]) + Sync),
    stderr_observer: &(dyn Fn(&[u8]) + Sync),
) -> Result<Option<i32>, ProcessError> {
    // Waiting for the leader alone is not completion: descendants may still
    // hold either pipe. The caller's deadline guards this entire join.
    let (status, (), ()) = tokio::try_join!(
        async { child.inner().wait().await.map_err(ProcessError::Wait) },
        drain_into(stdout, captured_stdout, stdout_observer),
        drain_into(stderr, captured_stderr, stderr_observer),
    )?;
    Ok(status.code())
}

fn start_termination(child: &mut command_group::AsyncGroupChild) -> Result<(), ProcessError> {
    match child.start_kill() {
        Ok(()) => Ok(()),
        // On Unix an already empty process group returns ESRCH. Escaped pipe
        // holders cannot be signalled as this group; the drain grace still ends.
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(3) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
        Err(error) => Err(ProcessError::Kill(error)),
    }
}

#[derive(Default)]
struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
    eof: bool,
}

#[cfg(test)]
async fn drain_stream(
    stream: impl tokio::io::AsyncRead + Unpin,
) -> Result<CapturedStream, ProcessError> {
    drain_stream_observed(stream, &|_| {}).await
}

#[cfg(test)]
async fn drain_stream_observed(
    mut stream: impl tokio::io::AsyncRead + Unpin,
    observe: &(dyn Fn(&[u8]) + Sync),
) -> Result<CapturedStream, ProcessError> {
    let mut captured = CapturedStream::default();
    drain_into(&mut stream, &mut captured, observe).await?;
    Ok(captured)
}

async fn drain_into(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    captured: &mut CapturedStream,
    observe: &(dyn Fn(&[u8]) + Sync),
) -> Result<(), ProcessError> {
    if captured.eof {
        return Ok(());
    }
    // Keep the read buffer on the heap so two concurrent drain futures do not
    // inflate every caller's future by 16 KiB.
    let mut chunk = vec![0_u8; 8192];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(ProcessError::Output)?;
        if read == 0 {
            captured.eof = true;
            break;
        }
        observe(&chunk[..read]);
        let retained = read.min(MAX_RETAINED_STREAM_BYTES.saturating_sub(captured.bytes.len()));
        captured.bytes.extend_from_slice(&chunk[..retained]);
        captured.truncated |= retained < read;
    }
    Ok(())
}

const fn cancelled_output() -> ProcessOutput {
    ProcessOutput {
        termination: ProcessTermination::Cancelled,
        exit_code: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        stdout_truncated: false,
        stderr_truncated: false,
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not spawn build process group: {0}")]
    Spawn(std::io::Error),
    #[error("build process {0} pipe was not created")]
    MissingPipe(&'static str),
    #[error("could not wait for build process group: {0}")]
    Wait(std::io::Error),
    #[error("could not terminate build process group: {0}")]
    Kill(std::io::Error),
    #[error("could not drain build output: {0}")]
    Output(std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn output_observer_receives_bytes_before_process_stream_closes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (mut writer, reader) = tokio::io::duplex(64);
        let received = AtomicBool::new(false);
        let observer = |bytes: &[u8]| {
            assert_eq!(bytes, b"live output");
            received.store(true, Ordering::SeqCst);
        };
        let producer = async {
            writer.write_all(b"live output").await.unwrap();
            while !received.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            drop(writer);
        };
        let ((), result) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(producer, drain_stream_observed(reader, &observer))
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap().bytes, b"live output");
    }

    #[tokio::test]
    async fn early_cancellation_never_spawns_the_command() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let output = run_grouped(
            &BuildCommand::argv("definitely-does-not-exist", std::iter::empty::<&str>()),
            Path::new("."),
            Duration::from_secs(1),
            &cancellation,
        )
        .await
        .unwrap();
        assert_eq!(output.termination, ProcessTermination::Cancelled);
    }

    #[tokio::test]
    async fn explicit_shell_drains_stdout_and_stderr() {
        #[cfg(windows)]
        let script = "echo stdout-line & echo stderr-line 1>&2";
        #[cfg(not(windows))]
        let script = "printf stdout-line; printf stderr-line >&2";
        let output = run_grouped(
            &BuildCommand::shell(script),
            Path::new("."),
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.termination, ProcessTermination::Exited);
        assert_eq!(output.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&output.stdout).contains("stdout-line"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("stderr-line"));
    }

    #[tokio::test]
    async fn timeout_terminates_long_running_process_group() {
        #[cfg(windows)]
        let script = "ping -n 20 127.0.0.1 >nul";
        #[cfg(not(windows))]
        let script = "sleep 20";
        let output = run_grouped(
            &BuildCommand::shell(script),
            Path::new("."),
            Duration::from_millis(50),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(output.termination, ProcessTermination::TimedOut);
    }

    #[tokio::test]
    async fn cancellation_terminates_running_process_group() {
        #[cfg(windows)]
        let script = "ping -n 20 127.0.0.1 >nul";
        #[cfg(not(windows))]
        let script = "sleep 20";
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let output = run_grouped(
            &BuildCommand::shell(script),
            Path::new("."),
            Duration::from_secs(5),
            &cancellation,
        )
        .await
        .unwrap();
        assert_eq!(output.termination, ProcessTermination::Cancelled);
    }

    #[cfg(unix)]
    fn inherited_pipe_command() -> BuildCommand {
        // This controlled descendant holds both inherited pipes until its
        // 20-second sleep completes or it is terminated. The tests below must
        // observe EOF, not merely return after truncating at the drain grace.
        let script = "sleep 20 & printf 'leader-exited\\n'; exit 0";
        BuildCommand::shell(script)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_still_applies_when_exited_parent_leaves_a_descendant_holding_pipes() {
        let output = tokio::time::timeout(
            Duration::from_secs(4),
            run_grouped(
                &inherited_pipe_command(),
                Path::new("."),
                Duration::from_millis(500),
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("the full process and stream lifecycle must be bounded")
        .unwrap();
        assert_eq!(output.termination, ProcessTermination::TimedOut);
        assert_eq!(output.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&output.stdout).contains("leader-exited"));
        assert!(!output.stdout_truncated, "descendant stdout must reach EOF");
        assert!(!output.stderr_truncated, "descendant stderr must reach EOF");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_after_parent_exit_kills_descendants_and_keeps_partial_output() {
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let output_ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let ready = output_ready.clone();
        let cancel_task = tokio::spawn(async move {
            ready.notified().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            trigger.cancel();
        });
        let observer = |stream, bytes: &[u8]| {
            if stream == OutputStream::Stdout && !bytes.is_empty() {
                output_ready.notify_one();
            }
        };
        let output = tokio::time::timeout(
            Duration::from_secs(4),
            run_grouped_with_output(
                &inherited_pipe_command(),
                Path::new("."),
                Duration::from_secs(20),
                &cancellation,
                &observer,
            ),
        )
        .await
        .expect("cancellation must still be polled while inherited pipes remain open")
        .unwrap();
        cancel_task.await.unwrap();
        assert_eq!(output.termination, ProcessTermination::Cancelled);
        assert_eq!(output.exit_code, Some(0));
        assert!(String::from_utf8_lossy(&output.stdout).contains("leader-exited"));
        assert!(!output.stdout_truncated, "descendant stdout must reach EOF");
        assert!(!output.stderr_truncated, "descendant stderr must reach EOF");
    }

    async fn exited_group() -> ProcessGroup {
        #[cfg(windows)]
        let script = "exit /b 0";
        #[cfg(not(windows))]
        let script = "exit 0";
        let mut command = platform_command(&BuildCommand::shell(script));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut group = ProcessGroup(command.group_spawn().unwrap());
        group.0.inner().wait().await.unwrap();
        group
    }

    #[tokio::test]
    async fn stream_deadline_survives_an_already_exited_leader_and_keeps_captured_bytes() {
        let mut group = exited_group().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"partial-output").await.unwrap();
        let mut stderr = tokio::io::empty();
        let output = tokio::time::timeout(
            Duration::from_secs(4),
            collect_output(
                &mut group.0,
                &mut reader,
                &mut stderr,
                Duration::from_millis(10),
                &CancellationToken::new(),
                &|_, _| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.termination, ProcessTermination::TimedOut);
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, b"partial-output");
        assert!(output.stdout_truncated);
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stream_cancellation_survives_an_already_exited_leader() {
        let mut group = exited_group().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"partial-output").await.unwrap();
        let mut stderr = tokio::io::empty();
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            trigger.cancel();
        });
        let output = tokio::time::timeout(
            Duration::from_secs(4),
            collect_output(
                &mut group.0,
                &mut reader,
                &mut stderr,
                Duration::from_secs(20),
                &cancellation,
                &|_, _| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        cancel_task.await.unwrap();
        assert_eq!(output.termination, ProcessTermination::Cancelled);
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, b"partial-output");
        assert!(output.stdout_truncated);
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_stream_is_drained_while_retention_stays_bounded() {
        let (mut writer, reader) = tokio::io::duplex(8192);
        let writer = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_RETAINED_STREAM_BYTES + 8192])
                .await
                .unwrap();
        });
        let captured = drain_stream(reader).await.unwrap();
        writer.await.unwrap();
        assert_eq!(captured.bytes.len(), MAX_RETAINED_STREAM_BYTES);
        assert!(captured.truncated);
    }

    #[test]
    fn debug_output_never_contains_process_bytes() {
        let output = ProcessOutput {
            termination: ProcessTermination::Exited,
            exit_code: Some(1),
            stdout: b"SECRET-STDOUT".to_vec(),
            stderr: b"SECRET-STDERR".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let debug = format!("{output:?}");
        assert!(!debug.contains("SECRET"));
        assert!(debug.contains("stdout_bytes: 13"));
    }
}
