//! Revalidates the user's saved choices immediately before Driver mutations.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::{DestinationRegistry, DestinationRevisionRecord, ProjectConfig, ProjectConfigState},
    domain::{DeploymentId, DestinationKey, DriverCapabilities},
    drivers::{
        ActivationReceipt, CleanupReport, ComponentExecutionContext, ComponentPlan,
        ComponentRequest, DeploymentDriver, DriverDestinationInput, DriverError, DriverKind,
        DriverLog, DriverTargetInput, EventSink, PreflightReport, PreparedRelease, ReleasePackage,
        ReleaseRef, RetentionPolicy, ValidatedDestinationSettings, ValidatedTargetSettings,
    },
};

/// Shared by every Component in one confirmed plan, including compensation.
/// This is a local recheck, not a lock or an atomic filesystem/network transaction.
#[derive(Debug)]
pub(super) struct ExecutionGuard {
    project_root: PathBuf,
    project: ProjectConfig,
    destinations_path: PathBuf,
    destinations: BTreeMap<DestinationKey, DestinationRevisionRecord>,
}

impl ExecutionGuard {
    pub(super) fn new(
        project_root: PathBuf,
        project: ProjectConfig,
        destinations_path: PathBuf,
        destinations: BTreeMap<DestinationKey, DestinationRevisionRecord>,
    ) -> Self {
        Self {
            project_root,
            project,
            destinations_path,
            destinations,
        }
    }

    pub(super) fn wrap(
        self: &Arc<Self>,
        driver: Arc<dyn DeploymentDriver>,
        context: ComponentExecutionContext,
    ) -> Arc<dyn DeploymentDriver> {
        Arc::new(GuardedDriver {
            inner: driver,
            guard: Arc::clone(self),
            context,
        })
    }

    fn validate(&self) -> Result<(), &'static str> {
        let current = crate::config::load(&self.project_root)
            .map_err(|_| "Project configuration cannot be validated")?;
        if !matches!(current, ProjectConfigState::Loaded(ref project) if project == &self.project) {
            return Err("Project configuration changed or disappeared");
        }
        let registry = DestinationRegistry::load(&self.destinations_path)
            .map_err(|_| "Destination registry cannot be validated")?;
        if self
            .destinations
            .iter()
            .any(|(key, record)| registry.resolve(key) != Some(record))
        {
            return Err("A selected Destination changed or disappeared");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct GuardedDriver {
    inner: Arc<dyn DeploymentDriver>,
    guard: Arc<ExecutionGuard>,
    context: ComponentExecutionContext,
}

impl GuardedDriver {
    fn validate(
        &self,
        stage: &str,
        context: &ComponentExecutionContext,
    ) -> Result<(), DriverError> {
        let expected = &self.context;
        let unchanged = context.project_id == expected.project_id
            && context.environment_id == expected.environment_id
            && context.component == expected.component
            && context.generation == expected.generation
            && context.destination == expected.destination
            && context.destination_revision == expected.destination_revision
            && context.credential == expected.credential
            && context.endpoint_fingerprint == expected.endpoint_fingerprint
            && Arc::ptr_eq(&context.target, &expected.target)
            && Arc::ptr_eq(
                &context.destination_settings,
                &expected.destination_settings,
            );
        // Execution and compensation may replace only the cancellation token.
        let result = if unchanged {
            self.guard.validate()
        } else {
            Err("Execution context differs from the confirmed plan")
        };
        result.map_err(|message| DriverError {
            stage: stage.into(),
            target: format!(
                "Project {} Environment {} Component {} Destination {}",
                expected.project_id,
                expected.environment_id,
                expected.component,
                expected.destination,
            ),
            message: message.into(),
            suggested_action: "review the saved configuration and run the environment check again; if a Release was activated, inspect it before manual recovery".into(),
        })
    }
}

#[async_trait]
impl DeploymentDriver for GuardedDriver {
    fn kind(&self) -> DriverKind {
        self.inner.kind()
    }

    fn static_capabilities(&self) -> DriverCapabilities {
        self.inner.static_capabilities()
    }

    fn validate_target(
        &self,
        input: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        self.inner.validate_target(input)
    }

    fn validate_destination(
        &self,
        input: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        self.inner.validate_destination(input)
    }

    async fn preflight(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        self.inner.preflight(context).await
    }

    async fn plan(
        &self,
        context: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        self.inner.plan(context, request).await
    }

    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        // Keep read-only observation on the frozen endpoint available for
        // failure diagnostics; a stale plan never permits mutation.
        self.inner.current(context).await
    }

    async fn inventory(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<crate::drivers::ComponentInventory, DriverError> {
        self.inner.inventory(context).await
    }

    async fn prepare(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        plan: &ComponentPlan,
        package: &ReleasePackage,
        events: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        self.validate("prepare", context)?;
        self.inner
            .prepare(deployment, context, plan, package, events)
            .await
    }

    async fn activate(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        self.validate("activate", context)?;
        self.inner.activate(deployment, context, release).await
    }

    async fn rollback(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        expected_current: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        self.validate("rollback", context)?;
        self.inner
            .rollback(deployment, context, expected_current, release)
            .await
    }

    async fn logs(
        &self,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        self.inner.logs(context, release).await
    }

    async fn cleanup(
        &self,
        context: &ComponentExecutionContext,
        policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        self.validate("cleanup", context)?;
        self.inner.cleanup(context, policy).await
    }
}

#[cfg(test)]
mod tests;
