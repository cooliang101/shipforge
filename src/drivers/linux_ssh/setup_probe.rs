use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::telemetry::{CommandArgument, CommandSpec};

use super::{AuthenticatedSession, SshConnectionError};

mod directories;
pub(super) use directories::{browse_remote_directories, validate_browse_path};

const MAX_SYSTEMD_CANDIDATES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSetupCandidates {
    pub root: RemoteRootState,
    pub systemd_units: Vec<String>,
    pub notices: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteRootState {
    Missing,
    WritableDirectory,
    ReadOnlyDirectory,
    NotDirectory,
}

/// Performs read-only discovery after SSH authentication.
///
/// Only fixed `test` and `systemctl list-unit-files` invocations are used.
/// Failure to query systemd is reported as a notice so servers without
/// systemd can still use filesystem-only deployment.
///
/// # Errors
///
/// Returns an error when the root probes cannot be executed or return an
/// unexpected status. A missing `systemctl` command is not fatal.
pub async fn probe_remote_setup(
    session: &AuthenticatedSession,
    root: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteSetupCandidates, RemoteSetupProbeError> {
    let exists = run_test(session, "-e", root, timeout, cancellation).await?;
    let root_state = if !exists {
        RemoteRootState::Missing
    } else if !run_test(session, "-d", root, timeout, cancellation).await? {
        RemoteRootState::NotDirectory
    } else if run_test(session, "-w", root, timeout, cancellation).await? {
        RemoteRootState::WritableDirectory
    } else {
        RemoteRootState::ReadOnlyDirectory
    };

    let command = CommandSpec::structured(
        "systemctl",
        [
            CommandArgument::plain("list-unit-files"),
            CommandArgument::plain("--type=service"),
            CommandArgument::plain("--no-legend"),
            CommandArgument::plain("--no-pager"),
        ],
    )
    .map_err(|error| RemoteSetupProbeError::Command(error.to_string()))?;
    let output = session.execute(&command, timeout, cancellation).await?;
    let mut notices = Vec::new();
    let systemd_units = if output.exit_status == 0 {
        if output.stdout_truncated {
            notices.push(format!(
                "systemd unit output was truncated; showing the first {MAX_SYSTEMD_CANDIDATES} candidates"
            ));
        }
        parse_systemd_units(&output.stdout)
    } else {
        notices.push("systemd units could not be queried; service selection is optional".into());
        Vec::new()
    };
    Ok(RemoteSetupCandidates {
        root: root_state,
        systemd_units,
        notices,
    })
}

async fn run_test(
    session: &AuthenticatedSession,
    predicate: &str,
    root: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<bool, RemoteSetupProbeError> {
    let command = test_command(predicate, root)?;
    let output = session
        .execute_allowing(&command, timeout, cancellation, &[0, 1])
        .await?;
    match output.exit_status {
        0 => Ok(true),
        1 => Ok(false),
        status => Err(RemoteSetupProbeError::UnexpectedTestStatus(status)),
    }
}

fn test_command(predicate: &str, root: &str) -> Result<CommandSpec, RemoteSetupProbeError> {
    CommandSpec::structured(
        "test",
        [
            CommandArgument::plain(predicate),
            CommandArgument::plain(root),
        ],
    )
    .map_err(|error| RemoteSetupProbeError::Command(error.to_string()))
}

fn parse_systemd_units(output: &[u8]) -> Vec<String> {
    let output = String::from_utf8_lossy(output);
    let mut units = output
        .lines()
        .filter_map(|line| line.split_ascii_whitespace().next())
        .filter(|unit| valid_systemd_unit(unit))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    units.sort();
    units.dedup();
    units.truncate(MAX_SYSTEMD_CANDIDATES);
    units
}

fn valid_systemd_unit(value: &str) -> bool {
    value.ends_with(".service")
        && value.len() <= 255
        && !value.starts_with('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b':' | b'\\')
        })
}

#[derive(Debug, Error)]
pub enum RemoteSetupProbeError {
    #[error(transparent)]
    Connection(#[from] SshConnectionError),
    #[error("could not build remote setup command: {0}")]
    Command(String),
    #[error("remote test command returned unexpected exit status {0}")]
    UnexpectedTestStatus(u32),
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    #[test]
    fn parses_sorts_deduplicates_and_filters_systemd_units() {
        let units = parse_systemd_units(
            b"z-worker.service enabled\napi.service disabled\napi.service enabled\nnot-a-unit enabled\n../bad.service enabled\n",
        );
        assert_eq!(units, vec!["api.service", "z-worker.service"]);
    }

    #[test]
    fn caps_systemd_candidates() {
        let output = (0..250).fold(String::new(), |mut output, index| {
            writeln!(output, "unit-{index:03}.service enabled").unwrap();
            output
        });
        assert_eq!(parse_systemd_units(output.as_bytes()).len(), 200);
    }

    #[test]
    fn systemd_unit_validation_rejects_shell_metacharacters() {
        assert!(valid_systemd_unit("api@blue.service"));
        assert!(!valid_systemd_unit("api;restart.service"));
        assert!(!valid_systemd_unit("$(id).service"));
        assert!(!valid_systemd_unit("../api.service"));
    }

    #[test]
    fn root_probe_quotes_shell_metacharacters_as_one_argument() {
        let command = test_command("-e", "/srv/app; touch /tmp/pwned").unwrap();
        assert_eq!(
            command.render_posix().unwrap(),
            "'test' '-e' '/srv/app; touch /tmp/pwned'"
        );
    }
}
