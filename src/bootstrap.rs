//! Composition root for built-in adapters.

use std::sync::Arc;

use crate::{
    application::DestinationSetupService,
    config::CredentialRegistry,
    drivers::{
        DriverError, DriverRegistry,
        linux_ssh::{LinuxSshDriver, LinuxSshSetupGateway},
    },
};

#[must_use]
pub fn destination_setup_service() -> DestinationSetupService {
    DestinationSetupService::new(Arc::new(LinuxSshSetupGateway))
}

/// Builds the registry of production Deployment Drivers compiled into the MVP.
///
/// # Errors
///
/// Returns an error if two compiled Drivers claim the same kind.
pub fn deployment_driver_registry(
    credentials: Arc<CredentialRegistry>,
) -> Result<DriverRegistry, DriverError> {
    let mut registry = DriverRegistry::default();
    registry.register(Arc::new(LinuxSshDriver::new(credentials)))?;
    Ok(registry)
}
