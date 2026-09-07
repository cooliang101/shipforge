use std::sync::Mutex;

use super::*;

#[derive(Debug)]
struct FakeTarget {
    kind: DriverKind,
}

impl ValidatedTargetSettings for FakeTarget {
    fn driver_kind(&self) -> &DriverKind {
        &self.kind
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ValidatedDestinationSettings for FakeTarget {
    fn driver_kind(&self) -> &DriverKind {
        &self.kind
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct FakeDriver {
    calls: Mutex<Vec<&'static str>>,
    capabilities: DriverCapabilities,
}

impl FakeDriver {
    fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            capabilities: DriverCapabilities::new(capabilities),
        }
    }

    fn record(&self, operation: &'static str, context: &ComponentExecutionContext) {
        assert_eq!(context.target.driver_kind(), &self.kind());
        self.calls.lock().unwrap().push(operation);
    }

    fn release_ref(
        &self,
        context: &ComponentExecutionContext,
        version: ReleaseVersion,
    ) -> ReleaseRef {
        ReleaseRef {
            driver: self.kind(),
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            effective_capabilities: self.capabilities.clone(),
        }
    }
}

#[async_trait]
impl DeploymentDriver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::parse("fake").unwrap()
    }

    fn static_capabilities(&self) -> DriverCapabilities {
        self.capabilities.clone()
    }

    fn validate_target(
        &self,
        _input: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        Ok(Arc::new(FakeTarget { kind: self.kind() }))
    }

    fn validate_destination(
        &self,
        _input: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        Ok(Arc::new(FakeTarget { kind: self.kind() }))
    }

    async fn preflight(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        self.record("preflight", context);
        Ok(PreflightReport {
            effective_capabilities: self.capabilities.clone(),
            notices: Vec::new(),
        })
    }

    async fn plan(
        &self,
        context: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        self.record("plan", context);
        self.capabilities
            .require(request.required_capabilities.iter().copied())
            .map_err(|rejection| DriverError {
                recovery_blocked: false,
                stage: "plan".into(),
                target: context.component.to_string(),
                message: format!("missing capabilities: {:?}", rejection.missing),
                suggested_action: "change the deployment policy or Destination".into(),
            })?;
        Ok(ComponentPlan {
            release: request.release.clone(),
            effective_capabilities: self.capabilities.clone(),
            expected_current: None,
            driver_steps: vec!["fake.prepare".into(), "fake.activate".into()],
        })
    }

    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        self.record("current", context);
        Ok(None)
    }

    async fn prepare(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        plan: &ComponentPlan,
        package: &ReleasePackage,
        events: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        self.record("prepare", context);
        assert_eq!(package.release(), &plan.release);
        events.emit(DriverLog {
            namespace: "fake.prepare".into(),
            message: "prepared".into(),
        });
        Ok(PreparedRelease {
            release: self.release_ref(context, plan.release.version.clone()),
            already_active: false,
        })
    }

    async fn activate(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        self.record("activate", context);
        Ok(ActivationReceipt {
            current: Some(release.clone()),
            healthy: true,
            warnings: Vec::new(),
        })
    }

    async fn rollback(
        &self,
        _deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        _expected_current: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        self.record("rollback", context);
        Ok(ActivationReceipt {
            current: release.cloned(),
            healthy: true,
            warnings: Vec::new(),
        })
    }

    async fn logs(
        &self,
        context: &ComponentExecutionContext,
        _release: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        self.record("logs", context);
        Ok(Vec::new())
    }

    async fn cleanup(
        &self,
        context: &ComponentExecutionContext,
        _policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        self.record("cleanup", context);
        Ok(CleanupReport::default())
    }
}

#[derive(Debug, Default)]
struct RecordingEvents(Mutex<Vec<DriverLog>>);

impl EventSink for RecordingEvents {
    fn emit(&self, event: DriverLog) {
        self.0.lock().unwrap().push(event);
    }
}

fn context() -> ComponentExecutionContext {
    let kind = DriverKind::parse("fake").unwrap();
    ComponentExecutionContext {
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("backend").unwrap(),
        generation: ComponentGeneration::INITIAL,
        destination: DestinationKey::parse("dst_00000000000000000000000000000001").unwrap(),
        destination_revision: DestinationRevision::INITIAL,
        credential: CredentialHandle::new(),
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        destination_settings: Arc::new(FakeTarget { kind: kind.clone() }),
        target: Arc::new(FakeTarget { kind }),
        cancellation: CancellationToken::new(),
    }
}

#[test]
fn serialized_driver_identifiers_cannot_bypass_validation() {
    assert!(serde_json::from_str::<CredentialHandle>(r#"""#).is_err());
    assert!(serde_json::from_str::<EndpointFingerprint>(r#""short""#).is_err());
}

fn request(context: &ComponentExecutionContext) -> ComponentRequest {
    ComponentRequest {
        release: ComponentRelease {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version: ReleaseVersion::parse("v1.0.0-test").unwrap(),
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
        },
        required_capabilities: BTreeSet::from([
            Capability::StagedDeployment,
            Capability::ExplicitActivation,
            Capability::Rollback,
        ]),
    }
}

#[tokio::test]
async fn fake_driver_contract_propagates_context_through_every_operation() {
    let driver = FakeDriver::new([
        Capability::StagedDeployment,
        Capability::ExplicitActivation,
        Capability::Rollback,
    ]);
    let context = context();
    let request = request(&context);
    let events = RecordingEvents::default();

    driver.preflight(&context).await.unwrap();
    let plan = driver.plan(&context, &request).await.unwrap();
    assert!(driver.current(&context).await.unwrap().is_none());
    let package = ReleasePackage::new(
        plan.release.clone(),
        std::path::PathBuf::from("release.tar.gz"),
        "a".repeat(64),
        1,
    );
    let deployment = crate::domain::DeploymentId::new();
    let prepared = driver
        .prepare(&deployment, &context, &plan, &package, &events)
        .await
        .unwrap();
    driver
        .activate(&deployment, &context, &prepared.release)
        .await
        .unwrap();
    driver
        .rollback(
            &deployment,
            &context,
            Some(&prepared.release),
            Some(&prepared.release),
        )
        .await
        .unwrap();
    driver.logs(&context, &prepared.release).await.unwrap();
    driver
        .cleanup(
            &context,
            &RetentionPolicy {
                protected_versions: BTreeSet::new(),
                retain_count: 2,
                candidate: CleanupCandidate {
                    release: prepared.release.clone(),
                    package: inventory::InventoryRelease {
                        manifest: package.manifest().clone(),
                        sha256: package.sha256().into(),
                        size: package.size(),
                        extracted: true,
                    },
                    expected_current: None,
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(
        *driver.calls.lock().unwrap(),
        [
            "preflight",
            "plan",
            "current",
            "prepare",
            "activate",
            "rollback",
            "logs",
            "cleanup"
        ]
    );
    assert_eq!(events.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn planning_rejects_missing_effective_capabilities() {
    let driver = FakeDriver::new([Capability::StagedDeployment]);
    let context = context();
    let error = driver.plan(&context, &request(&context)).await.unwrap_err();
    assert_eq!(error.stage, "plan");
    assert!(error.message.contains("ExplicitActivation"));
    assert!(error.message.contains("Rollback"));
}

#[test]
fn registry_rejects_duplicate_driver_kinds() {
    let driver = Arc::new(FakeDriver::new([]));
    let mut registry = DriverRegistry::default();
    registry.register(driver.clone()).unwrap();
    assert!(registry.register(driver).is_err());
}

#[test]
fn execution_context_debug_redacts_credential_reference() {
    let context = context();
    let debug = format!("{context:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains(context.credential.expose_reference()));
}

#[test]
fn release_reference_persists_effective_capabilities() {
    let context = context();
    let driver = FakeDriver::new([Capability::Rollback]);
    let reference = driver.release_ref(&context, ReleaseVersion::parse("v1").unwrap());
    let encoded = serde_json::to_string(&reference).unwrap();
    let decoded: ReleaseRef = serde_json::from_str(&encoded).unwrap();
    assert!(
        decoded
            .effective_capabilities
            .contains(Capability::Rollback)
    );
}
