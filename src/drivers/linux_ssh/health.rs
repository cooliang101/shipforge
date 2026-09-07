use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::DeploymentId,
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    ActivateReleaseError, ActivatedRemoteRelease, ActivationOptions, AuthenticatedSession,
    LinuxSshTarget, RemoteCommandOutput, SshConnectionError, transfer::sanitize_remote_error,
};

const MAX_ATTEMPTS: u32 = 100;
const MAX_DURATION: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthCheckOptions {
    pub command_timeout: Duration,
    pub interval: Duration,
    pub attempts: u32,
    pub stable_for: Duration,
}

impl Default for HealthCheckOptions {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(10),
            interval: Duration::from_secs(1),
            attempts: 5,
            stable_for: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdHealth {
    pub unit: String,
    pub restart_baseline: u64,
    pub observations: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpHealth {
    pub status: u16,
    pub attempts: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthCheckReport {
    pub systemd: Option<SystemdHealth>,
    pub http: Option<HttpHealth>,
    pub command_attempts: Option<u32>,
}

impl AuthenticatedSession {
    /// Runs all configured health checks from the Destination.
    ///
    /// A configured systemd unit must become active and remain active without
    /// increasing `NRestarts` for the stability window. A configured HTTP URL
    /// must return a 2xx response to remote `curl`.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid options, cancellation, malformed probe
    /// output, exhausted retries, or a remote command failure.
    pub async fn check_health(
        &self,
        target: &LinuxSshTarget,
        options: HealthCheckOptions,
        cancellation: &CancellationToken,
    ) -> Result<HealthCheckReport, HealthCheckError> {
        check_with_remote(self, target, options, cancellation).await
    }

    /// Verifies health and compensates the activation on any failed check.
    ///
    /// Compensation uses the activation layer's independent bounded token, so
    /// cancelling a health wait cannot interrupt restoration.
    ///
    /// # Errors
    ///
    /// Returns the health failure after successful compensation, or both the
    /// health and compensation failures with manual recovery context.
    pub async fn verify_activation_health(
        &self,
        target: &LinuxSshTarget,
        activation: &ActivatedRemoteRelease,
        deployment: &DeploymentId,
        health_options: HealthCheckOptions,
        activation_options: ActivationOptions,
        cancellation: &CancellationToken,
    ) -> Result<HealthCheckReport, HealthVerificationError> {
        match self
            .check_health(target, health_options, cancellation)
            .await
        {
            Ok(report) => Ok(report),
            Err(health) => match self
                .compensate_activation(target, activation, deployment, activation_options)
                .await
            {
                Ok(()) => Err(HealthVerificationError::FailedAndCompensated { health }),
                Err(compensation) => Err(HealthVerificationError::CompensationFailed {
                    health,
                    compensation,
                }),
            },
        }
    }
}

#[async_trait]
trait HealthRemote: Sync {
    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError>;
}

#[async_trait]
impl HealthRemote for AuthenticatedSession {
    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.execute(command, timeout, cancellation).await
    }
}

async fn check_with_remote<R: HealthRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    options: HealthCheckOptions,
    cancellation: &CancellationToken,
) -> Result<HealthCheckReport, HealthCheckError> {
    validate_options(options)?;
    if cancellation.is_cancelled() {
        return Err(HealthCheckError::Cancelled);
    }
    let systemd = match target
        .service
        .as_ref()
        .and_then(crate::config::ServiceConfig::systemd_unit)
    {
        Some(unit) => Some(check_systemd(remote, unit, options, cancellation).await?),
        None => None,
    };
    let http = match &target.health {
        Some(url) => Some(check_http(remote, url, options, cancellation).await?),
        None => None,
    };
    let command_attempts = match target
        .service
        .as_ref()
        .and_then(|service| service.check.as_ref())
    {
        Some(crate::config::ServiceCheck::Command { argv }) => {
            let command = super::service_command(argv, &format!("{}/current", target.root))
                .map_err(|_| {
                    HealthCheckError::InvalidCommand("Invalid service check context".into())
                })?;
            let mut passed = None;
            for attempt in 1..=options.attempts {
                let result = execute(
                    remote,
                    "service health check",
                    &command,
                    options.command_timeout,
                    cancellation,
                )
                .await;
                match result {
                    Ok(output) if output.exit_status == 0 => {
                        passed = Some(attempt);
                        break;
                    }
                    Err(HealthCheckError::Cancelled) => return Err(HealthCheckError::Cancelled),
                    _ => {}
                }
                if attempt < options.attempts {
                    wait(options.interval, cancellation).await?;
                }
            }
            Some(passed.ok_or(HealthCheckError::ServiceProbeFailed)?)
        }
        _ => None,
    };
    Ok(HealthCheckReport {
        systemd,
        http,
        command_attempts,
    })
}

async fn check_systemd<R: HealthRemote>(
    remote: &R,
    unit: &str,
    options: HealthCheckOptions,
    cancellation: &CancellationToken,
) -> Result<SystemdHealth, HealthCheckError> {
    let mut observations = 0;
    let baseline = loop {
        observations += 1;
        match systemd_state(remote, unit, options.command_timeout, cancellation).await {
            Ok(state) if state.active => break state.restarts,
            Ok(_) if observations >= options.attempts => {
                return Err(HealthCheckError::SystemdNotActive {
                    unit: unit.into(),
                    attempts: observations,
                });
            }
            Err(HealthCheckError::Cancelled) => return Err(HealthCheckError::Cancelled),
            Err(error) if observations >= options.attempts => {
                return Err(HealthCheckError::SystemdProbeFailed {
                    unit: unit.into(),
                    attempts: observations,
                    last_error: error.to_string(),
                });
            }
            Ok(_) | Err(_) => {}
        }
        wait(options.interval, cancellation).await?;
    };

    let deadline = Instant::now() + options.stable_for;
    while Instant::now() < deadline {
        wait(
            options
                .interval
                .min(deadline.saturating_duration_since(Instant::now())),
            cancellation,
        )
        .await?;
        observations += 1;
        let state = retry_systemd_state(remote, unit, options, cancellation).await?;
        if !state.active || state.restarts > baseline {
            return Err(HealthCheckError::SystemdUnstable {
                unit: unit.into(),
                baseline,
                observed: state.restarts,
                active: state.active,
            });
        }
    }
    Ok(SystemdHealth {
        unit: unit.into(),
        restart_baseline: baseline,
        observations,
    })
}

async fn retry_systemd_state<R: HealthRemote>(
    remote: &R,
    unit: &str,
    options: HealthCheckOptions,
    cancellation: &CancellationToken,
) -> Result<SystemdState, HealthCheckError> {
    for attempt in 1..=options.attempts {
        match systemd_state(remote, unit, options.command_timeout, cancellation).await {
            Ok(state) => return Ok(state),
            Err(HealthCheckError::Cancelled) => return Err(HealthCheckError::Cancelled),
            Err(error) if attempt == options.attempts => {
                return Err(HealthCheckError::SystemdProbeFailed {
                    unit: unit.into(),
                    attempts: attempt,
                    last_error: error.to_string(),
                });
            }
            Err(_) => wait(options.interval, cancellation).await?,
        }
    }
    unreachable!("validated attempts is non-zero")
}

async fn check_http<R: HealthRemote>(
    remote: &R,
    url: &str,
    options: HealthCheckOptions,
    cancellation: &CancellationToken,
) -> Result<HttpHealth, HealthCheckError> {
    let mut last_error = "no successful HTTP response".to_owned();
    for attempt in 1..=options.attempts {
        match run_http(remote, url, options.command_timeout, cancellation).await {
            Ok(output) => {
                if output.exit_status == 0
                    && let Some(status) = parse_http_status(&output)
                    && (200..300).contains(&status)
                {
                    return Ok(HttpHealth {
                        status,
                        attempts: attempt,
                    });
                }
                last_error = if output.exit_status == 0 {
                    parse_http_status(&output).map_or_else(
                        || "malformed HTTP status output".into(),
                        |status| format!("HTTP status {status}"),
                    )
                } else {
                    format!("curl exited with status {}", output.exit_status)
                };
            }
            Err(HealthCheckError::Cancelled) => return Err(HealthCheckError::Cancelled),
            Err(error) => last_error = error.to_string(),
        }
        if attempt < options.attempts {
            wait(options.interval, cancellation).await?;
        }
    }
    Err(HealthCheckError::HttpUnhealthy {
        attempts: options.attempts,
        last_error,
    })
}

async fn run_http<R: HealthRemote>(
    remote: &R,
    url: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteCommandOutput, HealthCheckError> {
    let command = CommandSpec::structured(
        "curl",
        [
            CommandArgument::plain("--silent"),
            CommandArgument::plain("--show-error"),
            CommandArgument::plain("--output"),
            CommandArgument::plain("/dev/null"),
            CommandArgument::plain("--write-out"),
            CommandArgument::plain("%{http_code}"),
            CommandArgument::plain("--max-time"),
            CommandArgument::plain(format!("{:.3}", timeout.as_secs_f64())),
            CommandArgument::plain("--"),
            CommandArgument::sensitive(url),
        ],
    )
    .map_err(|error| HealthCheckError::InvalidCommand(error.to_string()))?;
    execute(remote, "HTTP health check", &command, timeout, cancellation).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SystemdState {
    active: bool,
    restarts: u64,
}

async fn systemd_state<R: HealthRemote>(
    remote: &R,
    unit: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<SystemdState, HealthCheckError> {
    let output = run(
        remote,
        "systemd health check",
        "systemctl",
        ["show", "--property=ActiveState,NRestarts", "--", unit],
        timeout,
        cancellation,
    )
    .await?;
    if output.exit_status != 0 {
        return Err(HealthCheckError::CommandFailed {
            stage: "systemd health check",
            status: output.exit_status,
        });
    }
    if output.stdout_truncated {
        return Err(HealthCheckError::InvalidSystemdOutput);
    }
    parse_systemd_state(&output.stdout).ok_or(HealthCheckError::InvalidSystemdOutput)
}

fn parse_systemd_state(output: &[u8]) -> Option<SystemdState> {
    let text = std::str::from_utf8(output).ok()?;
    let mut active = None;
    let mut restarts = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ActiveState=") {
            active = Some(value == "active");
        } else if let Some(value) = line.strip_prefix("NRestarts=") {
            restarts = value.parse().ok();
        }
    }
    Some(SystemdState {
        active: active?,
        restarts: restarts?,
    })
}

fn parse_http_status(output: &RemoteCommandOutput) -> Option<u16> {
    if output.stdout_truncated {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?.trim();
    if text.len() != 3 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

async fn run<R, I, S>(
    remote: &R,
    stage: &'static str,
    program: &str,
    args: I,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteCommandOutput, HealthCheckError>
where
    R: HealthRemote,
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let command = CommandSpec::structured(
        program,
        args.into_iter()
            .map(|argument| CommandArgument::plain(argument.into())),
    )
    .map_err(|error| HealthCheckError::InvalidCommand(error.to_string()))?;
    execute(remote, stage, &command, timeout, cancellation).await
}

async fn execute<R: HealthRemote>(
    remote: &R,
    stage: &'static str,
    command: &CommandSpec,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteCommandOutput, HealthCheckError> {
    remote
        .command(command, timeout, cancellation)
        .await
        .map_err(|error| match error {
            SshConnectionError::Cancelled => HealthCheckError::Cancelled,
            _ => HealthCheckError::RemoteCommand {
                stage,
                message: sanitize_remote_error(&error.to_string()),
            },
        })
}

async fn wait(
    duration: Duration,
    cancellation: &CancellationToken,
) -> Result<(), HealthCheckError> {
    tokio::select! {
        () = cancellation.cancelled() => Err(HealthCheckError::Cancelled),
        () = tokio::time::sleep(duration) => Ok(()),
    }
}

fn validate_options(options: HealthCheckOptions) -> Result<(), HealthCheckError> {
    if options.command_timeout.is_zero()
        || options.interval.is_zero()
        || options.stable_for.is_zero()
        || options.attempts == 0
    {
        return Err(HealthCheckError::ZeroOption);
    }
    if options.command_timeout > MAX_DURATION
        || options.interval > MAX_DURATION
        || options.stable_for > MAX_DURATION
        || options.attempts > MAX_ATTEMPTS
    {
        return Err(HealthCheckError::OptionTooLarge);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum HealthCheckError {
    #[error("service health command did not pass within the configured attempts")]
    ServiceProbeFailed,
    #[error("health timeout, interval, attempts, and stability window must be non-zero")]
    ZeroOption,
    #[error("health options exceed the MVP safety limits")]
    OptionTooLarge,
    #[error("health check was cancelled")]
    Cancelled,
    #[error("could not construct a structured health command: {0}")]
    InvalidCommand(String),
    #[error("{stage} failed before an exit status was received: {message}")]
    RemoteCommand {
        stage: &'static str,
        message: String,
    },
    #[error("{stage} exited with status {status}")]
    CommandFailed { stage: &'static str, status: u32 },
    #[error("systemd returned malformed ActiveState/NRestarts output")]
    InvalidSystemdOutput,
    #[error("systemd unit `{unit}` did not become active after {attempts} attempts")]
    SystemdNotActive { unit: String, attempts: u32 },
    #[error("systemd unit `{unit}` could not be observed after {attempts} attempts: {last_error}")]
    SystemdProbeFailed {
        unit: String,
        attempts: u32,
        last_error: String,
    },
    #[error(
        "systemd unit `{unit}` was unstable: active={active}, restart baseline={baseline}, observed={observed}"
    )]
    SystemdUnstable {
        unit: String,
        baseline: u64,
        observed: u64,
        active: bool,
    },
    #[error("Destination-side HTTP health check failed after {attempts} attempts: {last_error}")]
    HttpUnhealthy { attempts: u32, last_error: String },
}

#[derive(Debug, Error)]
pub enum HealthVerificationError {
    #[error("health check failed and activation was compensated: {health}")]
    FailedAndCompensated { health: HealthCheckError },
    #[error("health check failed ({health}); activation compensation failed ({compensation})")]
    CompensationFailed {
        health: HealthCheckError,
        compensation: ActivateReleaseError,
    },
}

#[cfg(test)]
mod tests;
