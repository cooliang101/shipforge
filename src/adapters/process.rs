use std::{fmt, path::Path, process::Stdio, time::Duration};

use command_group::AsyncCommandGroup;
use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

use crate::config::BuildCommand;

const MAX_RETAINED_STREAM_BYTES: usize = 256 * 1024;

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
    let mut child = group.spawn().map_err(ProcessError::Spawn)?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or(ProcessError::MissingPipe("stdout"))?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or(ProcessError::MissingPipe("stderr"))?;
    let (termination, stdout, stderr) = tokio::join!(
        wait_for_process(&mut child, timeout, cancellation),
        drain_stream(stdout),
        drain_stream(stderr),
    );
    let (termination, exit_code) = termination?;
    let stdout = stdout?;
    let stderr = stderr?;
    Ok(ProcessOutput {
        termination,
        exit_code,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
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

async fn wait_for_process(
    child: &mut command_group::AsyncGroupChild,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(ProcessTermination, Option<i32>), ProcessError> {
    let result = tokio::select! {
        status = child.wait() => Some(status.map_err(ProcessError::Wait)?),
        () = cancellation.cancelled() => None,
        () = tokio::time::sleep(timeout) => None,
    };
    if let Some(status) = result {
        return Ok((ProcessTermination::Exited, status.code()));
    }
    let termination = if cancellation.is_cancelled() {
        ProcessTermination::Cancelled
    } else {
        ProcessTermination::TimedOut
    };
    match child.kill().await {
        Ok(()) => Ok((
            termination,
            child
                .try_wait()
                .map_err(ProcessError::Wait)?
                .and_then(|status| status.code()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok((
            termination,
            child.wait().await.map_err(ProcessError::Wait)?.code(),
        )),
        Err(error) => Err(ProcessError::Kill(error)),
    }
}

struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn drain_stream(
    mut stream: impl tokio::io::AsyncRead + Unpin,
) -> Result<CapturedStream, ProcessError> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    // Keep the read buffer on the heap so two concurrent drain futures do not
    // inflate every caller's future by 16 KiB.
    let mut chunk = vec![0_u8; 8192];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(ProcessError::Output)?;
        if read == 0 {
            break;
        }
        let retained = read.min(MAX_RETAINED_STREAM_BYTES.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < read;
    }
    Ok(CapturedStream { bytes, truncated })
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
