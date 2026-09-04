use std::sync::Arc;

use thiserror::Error;

use crate::{
    domain::CapabilityRejection,
    drivers::{
        ComponentExecutionContext, ComponentPlan, ComponentRequest, DeploymentDriver, DriverError,
        DriverRegistry,
    },
};

#[derive(Clone, Debug)]
pub struct PlannedComponent {
    pub driver: Arc<dyn DeploymentDriver>,
    pub context: ComponentExecutionContext,
    pub plan: ComponentPlan,
}

#[derive(Debug)]
pub struct DeploymentPlanner {
    drivers: Arc<DriverRegistry>,
}

impl DeploymentPlanner {
    #[must_use]
    pub fn new(drivers: Arc<DriverRegistry>) -> Self {
        Self { drivers }
    }

    /// Preflights and freezes a Driver-neutral Component plan.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, a missing Driver, unsupported
    /// capabilities, Driver failure, or a Driver contract violation.
    pub async fn plan_component(
        &self,
        context: ComponentExecutionContext,
        request: ComponentRequest,
    ) -> Result<PlannedComponent, ApplicationError> {
        if context.cancellation.is_cancelled() {
            return Err(ApplicationError::Cancelled);
        }
        let release = &request.release;
        if release.project_id != context.project_id
            || release.environment_id != context.environment_id
            || release.component != context.component
            || release.generation != context.generation
            || release.destination != context.destination
            || release.destination_revision != context.destination_revision
        {
            return Err(ApplicationError::Contract(
                "Component request identity differs from execution context".into(),
            ));
        }
        let kind = context.target.driver_kind();
        if context.destination_settings.driver_kind() != kind {
            return Err(ApplicationError::Contract(
                "Destination settings and Component target use different Drivers".into(),
            ));
        }
        let driver = self
            .drivers
            .get(kind)
            .ok_or_else(|| ApplicationError::MissingDriver(kind.to_string()))?;
        let static_capabilities = driver.static_capabilities();
        let preflight = driver
            .preflight(&context)
            .await
            .map_err(ApplicationError::Driver)?;
        if !preflight
            .effective_capabilities
            .is_subset_of(&static_capabilities)
        {
            return Err(ApplicationError::Contract(
                "preflight effective capabilities exceed static capabilities".into(),
            ));
        }
        preflight
            .effective_capabilities
            .require(request.required_capabilities.iter().copied())
            .map_err(ApplicationError::UnsupportedCapabilities)?;
        let current = driver
            .current(&context)
            .await
            .map_err(ApplicationError::Driver)?;
        let plan = driver
            .plan(&context, &request)
            .await
            .map_err(ApplicationError::Driver)?;
        if plan.release != request.release {
            return Err(ApplicationError::Contract(
                "Driver changed Component Release identity while planning".into(),
            ));
        }
        if plan.effective_capabilities != preflight.effective_capabilities {
            return Err(ApplicationError::Contract(
                "Driver plan capability snapshot differs from preflight".into(),
            ));
        }
        if plan.expected_current != current {
            return Err(ApplicationError::Contract(
                "Driver plan current Release differs from observation".into(),
            ));
        }
        Ok(PlannedComponent {
            driver,
            context,
            plan,
        })
    }
}

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("Deployment planning was cancelled")]
    Cancelled,
    #[error("no compiled Driver is registered for `{0}`")]
    MissingDriver(String),
    #[error("deployment policy requires unsupported capabilities: {0:?}")]
    UnsupportedCapabilities(CapabilityRejection),
    #[error(transparent)]
    Driver(DriverError),
    #[error("Driver contract violation: {0}")]
    Contract(String),
}

#[cfg(test)]
mod tests;
