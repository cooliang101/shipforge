use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;

use crate::{
    config::{CredentialRegistry, SshCredential},
    domain::{
        Capability, ComponentName, ComponentRelease, DeploymentId, DriverCapabilities,
        ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, ComponentExecutionContext, ComponentInventory,
        ComponentPlan, ComponentRequest, DeploymentDriver, DriverDestinationInput, DriverError,
        DriverKind, DriverLog, DriverTargetInput, EventSink, PreflightReport, PreparedRelease,
        ReleasePackage, ReleaseRef, RetentionPolicy, ValidatedDestinationSettings,
        ValidatedTargetSettings,
        audit::{RemoteAuditHistory, RemoteAuditPhase},
    },
};

use super::{
    ActivationOptions, HealthCheckOptions, LinuxSshDestination, LinuxSshTarget,
    PrepareReleaseOptions, PreparedRemoteRelease, RemoteRootState, connect_authenticated,
    probe_remote_setup,
};

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

type PreparedKey = (DeploymentId, ComponentName, ReleaseVersion);

#[derive(Clone, Debug)]
struct PreparedState {
    receipt: PreparedRemoteRelease,
    expected_current: Option<ReleaseVersion>,
}

/// Production adapter from the Driver SPI to the built-in SSH implementation.
#[derive(Debug)]
pub struct LinuxSshDriver {
    credentials: Arc<CredentialRegistry>,
    prepared: Mutex<HashMap<PreparedKey, PreparedState>>,
}

impl LinuxSshDriver {
    #[must_use]
    pub fn new(credentials: Arc<CredentialRegistry>) -> Self {
        Self {
            credentials,
            prepared: Mutex::new(HashMap::new()),
        }
    }

    fn settings<'a>(
        &'a self,
        context: &'a ComponentExecutionContext,
    ) -> Result<(&'a LinuxSshDestination, &'a LinuxSshTarget, SshCredential), DriverError> {
        let destination = context
            .destination_settings
            .as_any()
            .downcast_ref::<LinuxSshDestination>()
            .ok_or_else(|| {
                error(
                    "context",
                    &context.component,
                    "invalid SSH Destination settings",
                )
            })?;
        let target = context
            .target
            .as_any()
            .downcast_ref::<LinuxSshTarget>()
            .ok_or_else(|| error("context", &context.component, "invalid SSH target settings"))?;
        let credential = self
            .credentials
            .resolve(&context.credential)
            .cloned()
            .ok_or_else(|| {
                error(
                    "authentication",
                    &context.component,
                    "selected SSH credential no longer exists",
                )
            })?;
        Ok((destination, target, credential))
    }

    fn release_ref(context: &ComponentExecutionContext, version: ReleaseVersion) -> ReleaseRef {
        ReleaseRef {
            driver: DriverKind::linux_ssh(),
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            effective_capabilities: capabilities(),
        }
    }

    fn prepared_key(
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        version: &ReleaseVersion,
    ) -> PreparedKey {
        (
            deployment.clone(),
            context.component.clone(),
            version.clone(),
        )
    }
}

#[async_trait]
impl DeploymentDriver for LinuxSshDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }

    fn static_capabilities(&self) -> DriverCapabilities {
        capabilities()
    }

    fn validate_target(
        &self,
        input: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        LinuxSshTarget::validate(input)
            .map(|target| Arc::new(target) as Arc<dyn ValidatedTargetSettings>)
            .map_err(|source| validation_error("target", source))
    }

    fn validate_destination(
        &self,
        input: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        LinuxSshDestination::validate(input)
            .map(|destination| Arc::new(destination) as Arc<dyn ValidatedDestinationSettings>)
            .map_err(|source| validation_error("destination", source))
    }

    async fn preflight(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        let (destination, target, credential) = self.settings(context)?;
        let session = connect_authenticated(
            destination,
            &credential,
            CONNECTION_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("connect", context, source))?;
        check_marker(&session, target, context, true).await?;
        let candidates = probe_remote_setup(
            &session,
            &target.root,
            PREFLIGHT_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("preflight", context, source))?;
        match candidates.root {
            RemoteRootState::Missing | RemoteRootState::WritableDirectory => {}
            RemoteRootState::ReadOnlyDirectory => {
                return Err(error(
                    "preflight",
                    &context.component,
                    "remote Component root is not writable",
                ));
            }
            RemoteRootState::NotDirectory => {
                return Err(error(
                    "preflight",
                    &context.component,
                    "remote Component root exists but is not a directory",
                ));
            }
        }
        let mut notices = candidates.notices;
        notices.extend(
            super::preflight::check_preflight(
                &session,
                target,
                PREFLIGHT_TIMEOUT,
                &context.cancellation,
            )
            .await
            .map_err(|source| operation_error("preflight", context, source))?,
        );
        Ok(PreflightReport {
            effective_capabilities: capabilities(),
            notices,
        })
    }

    async fn plan(
        &self,
        context: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        let expected_current = self.current(context).await?;
        Ok(ComponentPlan {
            release: request.release.clone(),
            effective_capabilities: capabilities(),
            expected_current,
            driver_steps: vec![
                "Upload and verify Release".into(),
                "Extract immutable version".into(),
                "Switch current and verify health".into(),
            ],
        })
    }

    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        let (destination, target, credential) = self.settings(context)?;
        let session = connect_authenticated(
            destination,
            &credential,
            CONNECTION_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("connect", context, source))?;
        check_marker(&session, target, context, true).await?;
        let observed = session
            .observe_current(target, ActivationOptions::default(), &context.cancellation)
            .await
            .map_err(|source| operation_error("observe", context, source))?;
        if let Some(version) = &observed {
            session
                .check_release_manifest(
                    target,
                    &super::DeploymentMarker::for_context(context),
                    version,
                    &context.cancellation,
                )
                .await
                .map_err(|source| operation_error("observe", context, source))?;
        }
        Ok(observed.map(|version| Self::release_ref(context, version)))
    }

    async fn inventory(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        let (destination, target, credential) = self.settings(context)?;
        let session = connect_authenticated(
            destination,
            &credential,
            CONNECTION_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("connect", context, source))?;
        let marker = super::DeploymentMarker::for_context(context);
        let releases = session
            .release_inventory(target, &marker, &context.cancellation)
            .await
            .map_err(|source| operation_error("inventory", context, source))?;
        // Damaged or missing auxiliary audit must not erase verified file facts.
        let audit = session
            .read_audit(target, &marker, &context.cancellation)
            .await
            .unwrap_or_else(|_| RemoteAuditHistory {
                records: Vec::new(),
                notices: vec![
                    "remote audit could not be read; historical outcomes are unknown".into(),
                ],
                incomplete: true,
            });
        let remnants = session
            .temporary_remnants(target, &marker, &context.cancellation)
            .await
            .unwrap_or_else(|error| crate::drivers::inventory::TemporaryRemnants {
                entries: Vec::new(),
                notices: vec![error.to_string()],
                incomplete: true,
            });
        if context.cancellation.is_cancelled() {
            return Err(error(
                "inventory",
                &context.component,
                "inventory cancelled",
            ));
        }
        Ok(ComponentInventory {
            releases,
            audit,
            remnants,
        })
    }

    async fn prepare(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        plan: &ComponentPlan,
        package: &ReleasePackage,
        events: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        validate_prepare_request(context, plan, package)?;
        let (destination, target, credential) = self.settings(context)?;
        let session = connect_authenticated(
            destination,
            &credential,
            CONNECTION_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("connect", context, source))?;
        super::space::check_release_space(
            &session,
            target,
            package,
            PREFLIGHT_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("space", context, source))?;
        check_preparation_state(&session, target, context, plan).await?;
        session
            .ensure_deployment_marker(
                target,
                &super::DeploymentMarker::for_context(context),
                &context.cancellation,
            )
            .await
            .map_err(|source| operation_error("marker", context, source))?;
        let component = context.component.clone();
        let receipt = session
            .prepare_release(
                target,
                package,
                deployment,
                PrepareReleaseOptions::default(),
                &context.cancellation,
                |progress| {
                    events.emit(DriverLog {
                        namespace: "linux-ssh.upload".into(),
                        message: format!(
                            "{component}: uploaded {} of {} bytes",
                            progress.sent, progress.total
                        ),
                    });
                },
            )
            .await
            .map_err(|source| operation_error("prepare", context, source))?;
        let release = Self::release_ref(context, package.release().version.clone());
        let phase = super::driver_audit::AuditPhase {
            deployment,
            context,
            release: &release,
            phase: RemoteAuditPhase::Prepare,
            expected_current: plan
                .expected_current
                .as_ref()
                .map(|release| release.version.clone()),
            target: Some(release.version.clone()),
        };
        super::driver_audit::record_prepared(&session, target, &phase, package).await?;
        let key = Self::prepared_key(deployment, context, &release.version);
        self.prepared
            .lock()
            .map_err(|_| {
                error(
                    "prepare",
                    &context.component,
                    "prepared state is unavailable",
                )
            })?
            .insert(
                key,
                PreparedState {
                    receipt,
                    expected_current: plan
                        .expected_current
                        .as_ref()
                        .map(|release| release.version.clone()),
                },
            );
        Ok(PreparedRelease {
            release,
            already_active: false,
        })
    }

    async fn activate(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        validate_release_ref("activate", context, release)?;
        let key = Self::prepared_key(deployment, context, &release.version);
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| {
                error(
                    "activate",
                    &context.component,
                    "prepared state is unavailable",
                )
            })?
            .remove(&key)
            .ok_or_else(|| {
                error(
                    "activate",
                    &context.component,
                    "prepared Release receipt is missing",
                )
            })?;
        let (destination, target, credential) = self.settings(context)?;
        let session = connect_authenticated(
            destination,
            &credential,
            CONNECTION_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("connect", context, source))?;
        check_marker(&session, target, context, false).await?;
        let phase = super::driver_audit::AuditPhase {
            deployment,
            context,
            release,
            phase: RemoteAuditPhase::Activate,
            expected_current: prepared.expected_current.clone(),
            target: Some(release.version.clone()),
        };
        let result = async {
            let activation = session
                .activate_release(
                    target,
                    &prepared.receipt,
                    prepared.expected_current.as_ref(),
                    deployment,
                    ActivationOptions::default(),
                    &context.cancellation,
                )
                .await
                .map_err(|source| operation_error("activate", context, source))?;
            session
                .verify_activation_health(
                    target,
                    &activation,
                    deployment,
                    HealthCheckOptions::default(),
                    ActivationOptions::default(),
                    &context.cancellation,
                )
                .await
                .map_err(|source| operation_error("health", context, source))?;
            Ok(ActivationReceipt {
                current: Some(release.clone()),
                healthy: true,
                warnings: Vec::new(),
            })
        }
        .await;
        super::driver_audit::record_phase_result(&session, target, &phase, result).await
    }

    async fn rollback(
        &self,
        deployment: &DeploymentId,
        context: &ComponentExecutionContext,
        expected_current: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        if let Some(expected) = expected_current {
            validate_release_ref("rollback", context, expected)?;
        }
        if let Some(release) = release {
            validate_release_ref("rollback", context, release)?;
        }
        let (destination, target, credential) = self.settings(context)?;
        let session = connect_authenticated(
            destination,
            &credential,
            CONNECTION_TIMEOUT,
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("connect", context, source))?;
        check_marker(&session, target, context, false).await?;
        let current = observe_validated_current(&session, target, context).await?;
        if current.as_ref() != expected_current.map(|release| &release.version) {
            return Err(error(
                "rollback",
                &context.component,
                "current changed since the rollback was planned; inspect the external change before retrying",
            ));
        }
        let version = current
            .clone()
            .or_else(|| release.map(|release| release.version.clone()))
            .ok_or_else(|| {
                error(
                    "rollback",
                    &context.component,
                    "Component is not currently deployed",
                )
            })?;
        let current_is_absent = current.is_none();
        let current = ComponentRelease {
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
        };
        let desired = release.map(|release| &release.version);
        let audit_release = release.or(expected_current).ok_or_else(|| {
            error(
                "rollback",
                &context.component,
                "rollback has no source or target Release",
            )
        })?;
        let phase = super::driver_audit::AuditPhase {
            deployment,
            context,
            release: audit_release,
            phase: RemoteAuditPhase::Rollback,
            expected_current: expected_current.map(|release| release.version.clone()),
            target: desired.cloned(),
        };
        let result = async {
            if current_is_absent {
                session
                    .restore_undeployed_release(
                        target,
                        &current,
                        deployment,
                        ActivationOptions::default(),
                        &context.cancellation,
                    )
                    .await
                    .map_err(|source| operation_error("rollback", context, source))?;
            } else {
                session
                    .rollback_release(
                        target,
                        &current,
                        desired,
                        deployment,
                        ActivationOptions::default(),
                        &context.cancellation,
                    )
                    .await
                    .map_err(|source| operation_error("rollback", context, source))?;
            }
            if release.is_some() {
                session
                    .check_health(target, HealthCheckOptions::default(), &context.cancellation)
                    .await
                    .map_err(|source| operation_error("health", context, source))?;
            }
            Ok(ActivationReceipt {
                current: release.cloned(),
                healthy: true,
                warnings: Vec::new(),
            })
        }
        .await;
        super::driver_audit::record_phase_result(&session, target, &phase, result).await
    }

    async fn logs(
        &self,
        context: &ComponentExecutionContext,
        _release: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        Err(error(
            "logs",
            &context.component,
            "remote log retrieval is not implemented",
        ))
    }

    async fn cleanup(
        &self,
        context: &ComponentExecutionContext,
        _policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        Err(error(
            "cleanup",
            &context.component,
            "Release retention is not implemented",
        ))
    }
}

async fn observe_validated_current(
    session: &super::AuthenticatedSession,
    target: &LinuxSshTarget,
    context: &ComponentExecutionContext,
) -> Result<Option<ReleaseVersion>, DriverError> {
    let current = session
        .observe_current(target, ActivationOptions::default(), &context.cancellation)
        .await
        .map_err(|source| operation_error("observe", context, source))?;
    if let Some(version) = &current {
        session
            .check_release_manifest(
                target,
                &super::DeploymentMarker::for_context(context),
                version,
                &context.cancellation,
            )
            .await
            .map_err(|source| operation_error("observe", context, source))?;
    }
    Ok(current)
}

fn capabilities() -> DriverCapabilities {
    DriverCapabilities::new([
        Capability::StagedDeployment,
        Capability::ExplicitActivation,
        Capability::Observe,
        Capability::Inventory,
        Capability::Rollback,
        Capability::Cancellation,
    ])
}

fn validation_error(target: &str, source: impl std::fmt::Display) -> DriverError {
    DriverError {
        stage: "configuration".into(),
        target: target.into(),
        message: source.to_string(),
        suggested_action: "correct the value in the TUI and retry".into(),
    }
}

fn operation_error(
    stage: &str,
    context: &ComponentExecutionContext,
    source: impl std::fmt::Display,
) -> DriverError {
    DriverError {
        stage: stage.into(),
        target: context.component.to_string(),
        message: source.to_string(),
        suggested_action: "review the connection and remote state, then run the check again".into(),
    }
}

fn validate_prepare_request(
    context: &ComponentExecutionContext,
    plan: &ComponentPlan,
    package: &ReleasePackage,
) -> Result<(), DriverError> {
    let release = package.release();
    if release != &plan.release
        || release.project_id != context.project_id
        || release.environment_id != context.environment_id
        || release.component != context.component
        || release.generation != context.generation
        || release.destination != context.destination
        || release.destination_revision != context.destination_revision
    {
        return Err(error(
            "prepare",
            &context.component,
            "Release package identity differs from the frozen plan or execution context",
        ));
    }
    if let Some(previous) = &plan.expected_current {
        validate_release_ref("prepare", context, previous)?;
    }
    Ok(())
}

async fn check_preparation_state(
    session: &super::AuthenticatedSession,
    target: &LinuxSshTarget,
    context: &ComponentExecutionContext,
    plan: &ComponentPlan,
) -> Result<(), DriverError> {
    check_marker(session, target, context, true).await?;
    let observed = session
        .observe_current(target, ActivationOptions::default(), &context.cancellation)
        .await
        .map_err(|source| operation_error("prepare", context, source))?;
    if let Some(version) = &observed {
        session
            .check_release_manifest(
                target,
                &super::DeploymentMarker::for_context(context),
                version,
                &context.cancellation,
            )
            .await
            .map_err(|source| operation_error("prepare", context, source))?;
    }
    if observed.as_ref()
        != plan
            .expected_current
            .as_ref()
            .map(|release| &release.version)
    {
        return Err(error(
            "prepare",
            &context.component,
            "current changed since the plan; no preparation writes were made",
        ));
    }
    Ok(())
}

async fn check_marker(
    session: &super::AuthenticatedSession,
    target: &LinuxSshTarget,
    context: &ComponentExecutionContext,
    allow_new: bool,
) -> Result<(), DriverError> {
    let present = session
        .check_deployment_marker(
            target,
            &super::DeploymentMarker::for_context(context),
            &context.cancellation,
        )
        .await
        .map_err(|source| operation_error("marker", context, source))?;
    if !present {
        if !allow_new {
            return Err(operation_error(
                "marker",
                context,
                super::MarkerError::Missing,
            ));
        }
        session
            .check_unmarked_root(target, &context.cancellation)
            .await
            .map_err(|source| operation_error("marker", context, source))?;
    }
    Ok(())
}

fn validate_release_ref(
    stage: &str,
    context: &ComponentExecutionContext,
    release: &ReleaseRef,
) -> Result<(), DriverError> {
    if release.driver != crate::drivers::DriverKind::linux_ssh()
        || release.project_id != context.project_id
        || release.environment_id != context.environment_id
        || release.component != context.component
        || release.generation != context.generation
        || release.destination != context.destination
        || release.destination_revision != context.destination_revision
        || release.endpoint_fingerprint != context.endpoint_fingerprint
    {
        return Err(error(
            stage,
            &context.component,
            "Release identity differs from execution context; create a new plan",
        ));
    }
    Ok(())
}

fn error(stage: &str, component: &ComponentName, message: &str) -> DriverError {
    DriverError {
        stage: stage.into(),
        target: component.to_string(),
        message: message.into(),
        suggested_action: "refresh the deployment plan and retry".into(),
    }
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use crate::{
        domain::{
            Capability, ComponentGeneration, ComponentName, DeploymentId, DestinationKey,
            DestinationRevision, EnvironmentId, ProjectId, ReleaseVersion,
        },
        drivers::{
            ComponentExecutionContext, CredentialHandle, DeploymentDriver, DriverDestinationInput,
            DriverTargetInput, EndpointFingerprint,
        },
    };

    use super::*;

    #[tokio::test]
    async fn mismatched_release_is_rejected_before_credentials_or_network_access() {
        let driver = LinuxSshDriver::new(Arc::new(CredentialRegistry::new()));
        let context = context(&driver);
        let valid = LinuxSshDriver::release_ref(&context, ReleaseVersion::parse("v1").unwrap());
        assert!(validate_release_ref("activate", &context, &valid).is_ok());
        let mut wrong = valid;
        wrong.project_id = ProjectId::new();
        let id = DeploymentId::new();
        let activation = driver.activate(&id, &context, &wrong).await.unwrap_err();
        let rollback = driver
            .rollback(&id, &context, None, Some(&wrong))
            .await
            .unwrap_err();
        assert_eq!(activation.stage, "activate");
        assert_eq!(rollback.stage, "rollback");
        assert!(activation.message.contains("identity differs"));
        assert!(rollback.message.contains("identity differs"));
    }

    fn context(driver: &LinuxSshDriver) -> ComponentExecutionContext {
        let destination = driver
            .validate_destination(&DriverDestinationInput {
                value: serde_json::json!({
                    "host": "127.0.0.1",
                    "port": 22,
                    "user": "deploy",
                    "hostKey": "SHA256:confirmed"
                }),
            })
            .unwrap();
        let target = driver
            .validate_target(&DriverTargetInput {
                value: serde_json::json!({
                    "root": "/srv/app",
                    "systemd": null,
                    "health": null
                }),
            })
            .unwrap();
        ComponentExecutionContext {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
            credential: CredentialHandle::new(),
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            destination_settings: destination,
            target,
            cancellation: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn missing_credential_stops_before_network_access() {
        let driver = LinuxSshDriver::new(Arc::new(CredentialRegistry::new()));
        let error = driver.current(&context(&driver)).await.unwrap_err();
        assert_eq!(error.stage, "authentication");
        assert!(error.message.contains("no longer exists"));
    }

    #[test]
    fn capabilities_match_only_implemented_mvp_operations() {
        let driver = LinuxSshDriver::new(Arc::new(CredentialRegistry::new()));
        let capabilities = driver.static_capabilities();
        for capability in [
            Capability::StagedDeployment,
            Capability::ExplicitActivation,
            Capability::Observe,
            Capability::Rollback,
            Capability::Cancellation,
        ] {
            assert!(capabilities.contains(capability));
        }
        assert!(!capabilities.contains(Capability::RemoteLogs));
        assert!(!capabilities.contains(Capability::Retention));
    }

    #[test]
    fn prepared_state_keys_include_the_real_deployment_id() {
        let driver = LinuxSshDriver::new(Arc::new(CredentialRegistry::new()));
        let context = context(&driver);
        let version = ReleaseVersion::parse("v1").unwrap();
        let first = LinuxSshDriver::prepared_key(&DeploymentId::new(), &context, &version);
        let second = LinuxSshDriver::prepared_key(&DeploymentId::new(), &context, &version);
        assert_ne!(first, second);
    }
}
