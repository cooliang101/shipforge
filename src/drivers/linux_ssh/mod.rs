mod command_events;
mod connection;
mod destination;
mod driver;
mod health;
mod probe;
mod setup_gateway;
mod setup_probe;
mod target;
mod transfer;

pub use connection::{
    AuthenticatedSession, RemoteCommandOutput, SshConnectionError, connect_authenticated,
};
pub use destination::{LinuxSshDestination, LinuxSshDestinationError};
pub use driver::LinuxSshDriver;
pub use health::{
    HealthCheckError, HealthCheckOptions, HealthCheckReport, HttpHealth, SystemdHealth,
};
pub use probe::{
    AgentIdentitySummary, HostKeyVerifier, SshProbeError, capture_host_key, probe_agent_identities,
};
pub use setup_gateway::LinuxSshSetupGateway;
pub use setup_probe::{
    RemoteRootState, RemoteSetupCandidates, RemoteSetupProbeError, probe_remote_setup,
};
pub use target::{LinuxSshTarget, LinuxSshTargetError};
pub use transfer::{
    RemotePath, RemotePathError, UploadError, UploadOptions, UploadProgress, UploadReceipt,
};
/// Shared command construction for service presets, custom actions and checks.
pub(super) fn service_command(
    argv: &[String],
    directory: &str,
) -> Result<crate::telemetry::CommandSpec, crate::telemetry::SecurityError> {
    let (program, arguments) = argv
        .split_first()
        .ok_or(crate::telemetry::SecurityError::InvalidCommand)?;
    crate::telemetry::CommandSpec::structured(
        program,
        arguments
            .iter()
            .cloned()
            .map(crate::telemetry::CommandArgument::plain),
    )?
    .in_directory(directory)
}
