use std::{any::Any, collections::BTreeSet};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    domain::{
        Capability, ComponentGeneration, ComponentName, ComponentRelease, DestinationKey,
        DestinationRevision, DriverCapabilities, EnvironmentId, ProjectId, ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, CredentialHandle, DriverDestinationInput, DriverKind,
        DriverLog, DriverTargetInput, EndpointFingerprint, EventSink, PreflightReport,
        PreparedRelease, ReleasePackage, ReleaseRef, RetentionPolicy, ValidatedDestinationSettings,
        ValidatedTargetSettings,
    },
};

#[derive(Debug)]
struct Target(DriverKind);

impl ValidatedTargetSettings for Target {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ValidatedDestinationSettings for Target {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct PlanningDriver {
    static_capabilities: DriverCapabilities,
    effective_capabilities: DriverCapabilities,
    alter_release: bool,
}

#[async_trait]
impl DeploymentDriver for PlanningDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::parse("planning-test").unwrap()
    }

    fn static_capabilities(&self) -> DriverCapabilities {
        self.static_capabilities.clone()
    }

    fn validate_target(
        &self,
        _input: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        Ok(Arc::new(Target(self.kind())))
    }

    fn validate_destination(
        &self,
        _input: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        Ok(Arc::new(Target(self.kind())))
    }

    async fn preflight(
        &self,
        _context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        Ok(PreflightReport {
            effective_capabilities: self.effective_capabilities.clone(),
            notices: Vec::new(),
        })
    }

    async fn plan(
        &self,
        _context: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        let mut release = request.release.clone();
        if self.alter_release {
            release.version = ReleaseVersion::parse("wrong").unwrap();
        }
        Ok(ComponentPlan {
            release,
            effective_capabilities: self.effective_capabilities.clone(),
            expected_current: None,
            driver_steps: Vec::new(),
        })
    }

    async fn current(
        &self,
        _context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        Ok(None)
    }

    async fn prepare(
        &self,
        _deployment: &crate::domain::DeploymentId,
        _context: &ComponentExecutionContext,
        _plan: &ComponentPlan,
        _package: &ReleasePackage,
        _events: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        unreachable!("planning tests do not execute")
    }

    async fn activate(
        &self,
        _deployment: &crate::domain::DeploymentId,
        _context: &ComponentExecutionContext,
        _release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        unreachable!("planning tests do not execute")
    }

    async fn rollback(
        &self,
        _deployment: &crate::domain::DeploymentId,
        _context: &ComponentExecutionContext,
        _release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        unreachable!("planning tests do not execute")
    }

    async fn logs(
        &self,
        _context: &ComponentExecutionContext,
        _release: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        unreachable!("planning tests do not execute")
    }

    async fn cleanup(
        &self,
        _context: &ComponentExecutionContext,
        _policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        unreachable!("planning tests do not execute")
    }
}

fn fixture(
    capabilities: DriverCapabilities,
    alter_release: bool,
) -> (
    DeploymentPlanner,
    ComponentExecutionContext,
    ComponentRequest,
) {
    let driver = Arc::new(PlanningDriver {
        static_capabilities: capabilities.clone(),
        effective_capabilities: capabilities,
        alter_release,
    });
    let target = driver
        .validate_target(&DriverTargetInput {
            value: serde_json::json!({}),
        })
        .unwrap();
    let destination_settings = driver
        .validate_destination(&DriverDestinationInput {
            value: serde_json::json!({}),
        })
        .unwrap();
    let mut registry = DriverRegistry::default();
    registry.register(driver).unwrap();
    let context = ComponentExecutionContext {
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("backend").unwrap(),
        generation: ComponentGeneration::INITIAL,
        destination: DestinationKey::parse("dst_00000000000000000000000000000001").unwrap(),
        destination_revision: DestinationRevision::INITIAL,
        credential: CredentialHandle::new(),
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        destination_settings,
        target,
        cancellation: CancellationToken::new(),
    };
    let request = ComponentRequest {
        release: ComponentRelease {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version: ReleaseVersion::parse("v1").unwrap(),
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
        },
        required_capabilities: BTreeSet::from([Capability::StagedDeployment]),
    };
    (DeploymentPlanner::new(Arc::new(registry)), context, request)
}

#[tokio::test]
async fn planner_freezes_a_consistent_driver_plan() {
    let capabilities = DriverCapabilities::new([Capability::StagedDeployment]);
    let (planner, context, request) = fixture(capabilities, false);
    let result = planner.plan_component(context, request).await.unwrap();
    assert_eq!(result.plan.expected_current, None);
}

#[tokio::test]
async fn planner_rejects_unsupported_effective_capability_before_driver_plan() {
    let (planner, context, request) = fixture(DriverCapabilities::default(), false);
    assert!(matches!(
        planner.plan_component(context, request).await,
        Err(ApplicationError::UnsupportedCapabilities(_))
    ));
}

#[tokio::test]
async fn planner_rejects_driver_release_identity_changes() {
    let capabilities = DriverCapabilities::new([Capability::StagedDeployment]);
    let (planner, context, request) = fixture(capabilities, true);
    assert!(matches!(
        planner.plan_component(context, request).await,
        Err(ApplicationError::Contract(_))
    ));
}

#[tokio::test]
async fn planner_honors_cancellation_before_preflight() {
    let capabilities = DriverCapabilities::new([Capability::StagedDeployment]);
    let (planner, context, request) = fixture(capabilities, false);
    context.cancellation.cancel();
    assert!(matches!(
        planner.plan_component(context, request).await,
        Err(ApplicationError::Cancelled)
    ));
}

#[tokio::test]
async fn planner_rejects_request_identity_mismatch_before_preflight() {
    let capabilities = DriverCapabilities::new([Capability::StagedDeployment]);
    let (planner, context, mut request) = fixture(capabilities, false);
    request.release.component = ComponentName::parse("worker").unwrap();
    assert!(matches!(
        planner.plan_component(context, request).await,
        Err(ApplicationError::Contract(_))
    ));
}
