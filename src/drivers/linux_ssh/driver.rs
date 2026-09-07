//! Publish application files in the existing directory; keep one previous archive.
use super::{
    AuthenticatedSession, HealthCheckOptions, LinuxSshDestination, LinuxSshTarget, RemotePath,
    UploadOptions, connect_authenticated,
};
use crate::{
    config::{CredentialRegistry, ServiceAction},
    domain::{Capability, DriverCapabilities, ReleaseVersion},
    drivers::{
        ActivationReceipt, CleanupReport, ComponentExecutionContext, ComponentInventory,
        ComponentPlan, ComponentRequest, DeploymentDriver, DriverDestinationInput, DriverError,
        DriverKind, DriverLog, DriverTargetInput, EventSink, PreflightReport, PreparedRelease,
        ReleasePackage, ReleaseRef, RemoteAuditHistory, RetentionPolicy,
        ValidatedDestinationSettings, ValidatedTargetSettings,
        inventory::{InventoryRelease, ReleaseInventory, TemporaryRemnants},
    },
    telemetry::{CommandArgument, CommandSpec},
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
const TIMEOUT: Duration = Duration::from_secs(120);
const SCRIPT: &str = include_str!("inplace.py");
#[derive(Debug)]
pub struct LinuxSshDriver {
    credentials: Arc<CredentialRegistry>,
}
#[derive(Deserialize)]
struct Observation {
    current: Option<StoredRelease>,
    previous: Option<StoredRelease>,
}
#[derive(Deserialize)]
struct StoredRelease {
    manifest: crate::domain::ReleaseManifest,
    sha256: String,
    size: u64,
}

impl LinuxSshDriver {
    #[must_use]
    pub fn new(credentials: Arc<CredentialRegistry>) -> Self {
        Self { credentials }
    }
    fn target(context: &ComponentExecutionContext) -> Result<&LinuxSshTarget, DriverError> {
        context
            .target
            .as_any()
            .downcast_ref()
            .ok_or_else(|| error(context, "configuration", "Invalid SSH target", false))
    }
    async fn session(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<AuthenticatedSession, DriverError> {
        let target = context
            .destination_settings
            .as_any()
            .downcast_ref::<LinuxSshDestination>()
            .ok_or_else(|| error(context, "configuration", "Invalid SSH connection", false))?;
        let credential = self
            .credentials
            .resolve(&context.credential)
            .ok_or_else(|| {
                error(
                    context,
                    "authentication",
                    "Selected SSH credential no longer exists",
                    false,
                )
            })?;
        connect_authenticated(
            target,
            credential,
            Duration::from_secs(15),
            &context.cancellation,
        )
        .await
        .map_err(|_| {
            error(
                context,
                "connect",
                "SSH connection or pinned authentication failed",
                false,
            )
        })
    }
    fn release(context: &ComponentExecutionContext, version: ReleaseVersion) -> ReleaseRef {
        ReleaseRef {
            driver: DriverKind::linux_ssh(),
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            effective_capabilities: capabilities(),
            version,
        }
    }
    async fn rpc(
        session: &AuthenticatedSession,
        context: &ComponentExecutionContext,
        operation: &str,
        mut request: Value,
        cancellation: &CancellationToken,
        mutation: bool,
    ) -> Result<Value, DriverError> {
        if cancellation.is_cancelled() {
            return Err(error(
                context,
                operation,
                "Operation cancelled before dispatch",
                false,
            ));
        }
        request["operation"] = json!(operation);
        request["root"] = json!(Self::target(context)?.root);
        request["identity"] = json!({"project":context.project_id,"environment":context.environment_id,"component":context.component});
        let command = CommandSpec::structured(
            "python3",
            ["-c".to_owned(), SCRIPT.to_owned(), request.to_string()].map(CommandArgument::plain),
        )
        .map_err(|_| error(context, operation, "Invalid application operation", false))?;
        let output = session
            .execute(&command, TIMEOUT, cancellation)
            .await
            .map_err(|_| {
                error(
                    context,
                    operation,
                    "Remote operation has no confirmed result; inspect before retrying",
                    mutation,
                )
            })?;
        if output.stdout_truncated {
            return Err(error(
                context,
                operation,
                "Remote result exceeded limit",
                mutation,
            ));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| error(context, operation, "Remote result is unavailable", mutation))?;
        if output.exit_status != 0 {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Application operation failed");
            let message = if message.len() <= 256 && !message.chars().any(char::is_control) {
                message
            } else {
                "Application operation failed"
            };
            return Err(error(
                context,
                operation,
                message,
                mutation && value["recoverable"].as_bool() != Some(true),
            ));
        }
        Ok(value)
    }
    async fn phase(
        session: &AuthenticatedSession,
        context: &ComponentExecutionContext,
        deployment: &crate::domain::DeploymentId,
        phase: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), DriverError> {
        Self::rpc(
            session,
            context,
            "phase",
            json!({"deployment":deployment,"phase":phase}),
            cancellation,
            true,
        )
        .await
        .map(|_| ())
    }
    async fn action(
        session: &AuthenticatedSession,
        context: &ComponentExecutionContext,
        deployment: &crate::domain::DeploymentId,
        commands: &ServiceAction,
        cancellation: &CancellationToken,
    ) -> Result<(), DriverError> {
        for argv in commands {
            if cancellation.is_cancelled() {
                return Err(error(
                    context,
                    "service",
                    "Service action cancelled before dispatch",
                    false,
                ));
            }
            let command = super::service_command(argv, &Self::target(context)?.root)
                .map_err(|_| error(context, "service", "Invalid service argv", false))?;
            session.validate_sudo_command(&command).map_err(|_| {
                error(
                    context,
                    "service",
                    "Password sudo requires a saved login password and canonical sudo -S -- argv",
                    false,
                )
            })?;
            Self::phase(
                session,
                context,
                deployment,
                "service-pending",
                cancellation,
            )
            .await?;
            let result = session.execute(&command, TIMEOUT, cancellation).await;
            let settling = CancellationToken::new();
            match result {
                Ok(output) => {
                    Self::phase(
                        session,
                        context,
                        deployment,
                        if output.exit_status == 0 {
                            "service-complete"
                        } else {
                            "service-failed"
                        },
                        &settling,
                    )
                    .await?;
                    if output.exit_status != 0 {
                        return Err(error(
                            context,
                            "service",
                            &format!("Service command exited with status {}", output.exit_status),
                            false,
                        ));
                    }
                }
                Err(_) => {
                    return Err(error(
                        context,
                        "service",
                        "Service command outcome is unknown; automatic competing recovery is blocked",
                        true,
                    ));
                }
            }
        }
        Ok(())
    }
    async fn restore(
        &self,
        session: &AuthenticatedSession,
        context: &ComponentExecutionContext,
        deployment: &crate::domain::DeploymentId,
        expected: Option<&ReleaseRef>,
        desired: Option<&ReleaseRef>,
        cancellation: &CancellationToken,
    ) -> Result<ActivationReceipt, DriverError> {
        let result=Self::rpc(session,context,"rollback",json!({"deployment":deployment,"expected":expected.map(|r|&r.version),"desired":desired.map(|r|&r.version)}),cancellation,true).await?;
        let target = Self::target(context)?;
        if let Some(service) = &target.service {
            let commands = if result["previousExists"].as_bool() == Some(true) {
                service.restore_commands()
            } else {
                &service.stop
            };
            Self::action(session, context, deployment, commands, cancellation)
                .await
                .map_err(|mut e| {
                    e.recovery_blocked = true;
                    e
                })?;
        }
        if result["previousExists"].as_bool() == Some(true) {
            session
                .check_health(target, HealthCheckOptions::default(), cancellation)
                .await
                .map_err(|_| {
                    error(
                        context,
                        "recovery",
                        "Previous application health check failed",
                        true,
                    )
                })?;
        }
        Self::phase(session, context, deployment, "stable", cancellation).await?;
        Ok(ActivationReceipt {
            current: desired.cloned(),
            healthy: true,
            warnings: Vec::new(),
        })
    }
    async fn activate_logged(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
        events: Arc<dyn EventSink>,
    ) -> Result<ActivationReceipt, DriverError> {
        validate_ref(context, release)?;
        let session = self.session(context).await?.with_command_events(events);
        let previous = self.current(context).await?;
        Self::rpc(
            &session,
            context,
            "archive",
            json!({"deployment":deployment,"version":release.version}),
            &context.cancellation,
            true,
        )
        .await?;
        let target = Self::target(context)?;
        let mut published = false;
        let result: Result<_, DriverError> = async {
            if let Some(service) = &target.service {
                Self::action(
                    &session,
                    context,
                    deployment,
                    &service.stop,
                    &context.cancellation,
                )
                .await?;
            }
            Self::rpc(
                &session,
                context,
                "publish",
                json!({"deployment":deployment}),
                &context.cancellation,
                true,
            )
            .await?;
            published = true;
            if let Some(service) = &target.service {
                Self::action(
                    &session,
                    context,
                    deployment,
                    if previous.is_some() {
                        service.update_commands()
                    } else {
                        &service.start
                    },
                    &context.cancellation,
                )
                .await?;
            }
            session
                .check_health(target, HealthCheckOptions::default(), &context.cancellation)
                .await
                .map_err(|_| error(context, "health", "Application health check failed", false))?;
            Self::phase(
                &session,
                context,
                deployment,
                "stable",
                &CancellationToken::new(),
            )
            .await?;
            Ok(ActivationReceipt {
                current: Some(release.clone()),
                healthy: true,
                warnings: Vec::new(),
            })
        }
        .await;
        if let Err(failure) = &result
            && !failure.recovery_blocked
        {
            self.restore(
                &session,
                context,
                deployment,
                if published {
                    Some(release)
                } else {
                    previous.as_ref()
                },
                previous.as_ref(),
                &CancellationToken::new(),
            )
            .await?;
        }
        result
    }
    async fn rollback_logged(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        expected: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
        events: Arc<dyn EventSink>,
    ) -> Result<ActivationReceipt, DriverError> {
        for r in expected.into_iter().chain(release) {
            validate_ref(context, r)?;
        }
        let session = self.session(context).await?.with_command_events(events);
        self.restore(
            &session,
            context,
            deployment,
            expected,
            release,
            &context.cancellation,
        )
        .await
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
            .map(|v| Arc::new(v) as Arc<dyn ValidatedTargetSettings>)
            .map_err(|_| DriverError::configuration("target", "Invalid SSH application target"))
    }
    fn validate_destination(
        &self,
        input: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        LinuxSshDestination::validate(input)
            .map(|v| Arc::new(v) as Arc<dyn ValidatedDestinationSettings>)
            .map_err(|_| DriverError::configuration("connection", "Invalid SSH connection"))
    }
    async fn preflight(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        let session = self.session(context).await?;
        let target = Self::target(context)?;
        let mut programs = Vec::new();
        if let Some(service) = &target.service {
            for argv in service
                .start
                .iter()
                .chain(&service.stop)
                .chain(&service.update)
                .chain(&service.restore)
                .chain(match &service.check {
                    Some(crate::config::ServiceCheck::Command { argv }) => Some(argv),
                    _ => None,
                })
            {
                if let Some(program) = argv.first() {
                    programs.push(program.clone());
                }
                if argv.get(1).is_some_and(|v| v == "-S")
                    && argv.get(2).is_some_and(|v| v == "--")
                    && let Some(program) = argv.get(3)
                {
                    programs.push(program.clone());
                }
                let command = super::service_command(argv, &target.root)
                    .map_err(|_| error(context, "preflight", "Invalid service argv", false))?;
                session.validate_sudo_command(&command).map_err(|_| {
                    error(
                        context,
                        "preflight",
                        "Password sudo requires saved login credentials and sudo -S -- argv",
                        false,
                    )
                })?;
            }
        }
        let result = Self::rpc(
            &session,
            context,
            "preflight",
            json!({"programs":programs}),
            &context.cancellation,
            false,
        )
        .await?;
        Ok(PreflightReport{effective_capabilities:capabilities(),notices:vec!["Publish into the existing application directory; preserve runtime files and service configuration.".into(),"Keep only the previous application archive for failed-publish recovery.".into(),format!("Available bytes: {}",result["free"])]})
    }
    async fn plan(
        &self,
        context: &ComponentExecutionContext,
        request: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        Ok(ComponentPlan {
            release: request.release.clone(),
            effective_capabilities: capabilities(),
            expected_current: self.current(context).await?,
            driver_steps: vec![
                "Upload and verify application package".into(),
                "Archive previous application files".into(),
                "Publish in place, run existing service commands and verify".into(),
            ],
        })
    }
    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        let session = self.session(context).await?;
        let value = Self::rpc(
            &session,
            context,
            "observe",
            json!({}),
            &context.cancellation,
            false,
        )
        .await?;
        let observed: Observation = serde_json::from_value(value)
            .map_err(|_| error(context, "observe", "Invalid application observation", false))?;
        Ok(observed
            .current
            .map(|r| Self::release(context, r.manifest.version)))
    }
    async fn inventory(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        let session = self.session(context).await?;
        let value = Self::rpc(
            &session,
            context,
            "observe",
            json!({}),
            &context.cancellation,
            false,
        )
        .await?;
        let observed: Observation = serde_json::from_value(value)
            .map_err(|_| error(context, "inventory", "Invalid application inventory", false))?;
        let current = observed
            .current
            .as_ref()
            .map(|r| r.manifest.version.clone());
        let releases = observed
            .current
            .into_iter()
            .map(|r| (r, true))
            .chain(observed.previous.map(|r| (r, false)))
            .map(|(r, extracted)| InventoryRelease {
                manifest: r.manifest,
                sha256: r.sha256,
                size: r.size,
                extracted,
            })
            .collect();
        Ok(ComponentInventory{releases:ReleaseInventory{releases,current:Ok(current),issues:Vec::new(),notices:vec!["Only the previous application is available for rollback; no multi-version retention.".into()]},audit:RemoteAuditHistory{records:Vec::new(),notices:Vec::new(),incomplete:false},remnants:TemporaryRemnants::default()})
    }
    async fn prepare(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        plan: &ComponentPlan,
        package: &ReleasePackage,
        events: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        let release = Self::release(context, package.release().version.clone());
        if package.release() != &plan.release
            || package.release().project_id != context.project_id
            || package.release().environment_id != context.environment_id
            || package.release().component != context.component
            || package.release().generation != context.generation
            || package.release().destination != context.destination
            || package.release().destination_revision != context.destination_revision
        {
            return Err(error(
                context,
                "prepare",
                "Package identity differs from plan",
                false,
            ));
        }
        let session = self.session(context).await?;
        let begin=Self::rpc(&session,context,"begin",json!({"deployment":deployment,"manifest":package.manifest(),"expected":plan.expected_current.as_ref().map(|r|&r.version)}),&context.cancellation,true).await?;
        let upload = begin["upload"]
            .as_str()
            .ok_or_else(|| error(context, "prepare", "Invalid upload path", true))?;
        let expected = format!(
            "{}/.shipforge-deploy/incoming.tar.gz",
            Self::target(context)?.root
        );
        if upload != expected {
            return Err(error(context, "prepare", "Unexpected upload path", true));
        }
        let path = RemotePath::parse(upload)
            .map_err(|_| error(context, "prepare", "Invalid upload path", true))?;
        let result: Result<_, DriverError> = async {
            session
                .upload_release(
                    package.path(),
                    &path,
                    UploadOptions::default(),
                    &context.cancellation,
                    |p| {
                        events.emit(DriverLog {
                            namespace: "linux-ssh.upload".into(),
                            message: format!("Uploaded {} of {} bytes", p.sent, p.total),
                        });
                    },
                )
                .await
                .map_err(|_| error(context, "upload", "Application upload failed", false))?;
            Self::rpc(
                &session,
                context,
                "prepare",
                json!({"deployment":deployment,"sha256":package.sha256(),"size":package.size()}),
                &context.cancellation,
                true,
            )
            .await?;
            Ok(PreparedRelease {
                release,
                already_active: false,
            })
        }
        .await;
        if result.is_err() {
            let _ = Self::rpc(
                &session,
                context,
                "abort",
                json!({"deployment":deployment}),
                &CancellationToken::new(),
                true,
            )
            .await;
        }
        result
    }
    async fn discard_prepared(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
    ) -> Result<(), DriverError> {
        let session = self.session(context).await?;
        Self::rpc(
            &session,
            context,
            "discard",
            json!({"deployment":deployment}),
            &context.cancellation,
            true,
        )
        .await
        .map(|_| ())
    }
    async fn activate(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        self.activate_with_events(
            deployment,
            context,
            release,
            &super::command_events::QuietEvents,
        )
        .await
    }
    async fn activate_with_events(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        release: &ReleaseRef,
        events: &dyn EventSink,
    ) -> Result<ActivationReceipt, DriverError> {
        super::command_events::relay(events, |sink| {
            self.activate_logged(deployment, context, release, sink)
        })
        .await
    }
    async fn rollback(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        expected: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        self.rollback_with_events(
            deployment,
            context,
            expected,
            release,
            &super::command_events::QuietEvents,
        )
        .await
    }
    async fn rollback_with_events(
        &self,
        deployment: &crate::domain::DeploymentId,
        context: &ComponentExecutionContext,
        expected: Option<&ReleaseRef>,
        release: Option<&ReleaseRef>,
        events: &dyn EventSink,
    ) -> Result<ActivationReceipt, DriverError> {
        super::command_events::relay(events, |sink| {
            self.rollback_logged(deployment, context, expected, release, sink)
        })
        .await
    }
    async fn logs(
        &self,
        context: &ComponentExecutionContext,
        _release: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        Err(error(
            context,
            "logs",
            "Use recorded deployment output; runtime logs are not managed",
            false,
        ))
    }
    async fn cleanup(
        &self,
        context: &ComponentExecutionContext,
        _policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        Err(error(
            context,
            "cleanup",
            "Only the previous application archive is retained",
            false,
        ))
    }
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
fn validate_ref(
    context: &ComponentExecutionContext,
    release: &ReleaseRef,
) -> Result<(), DriverError> {
    if release
        .effective_capabilities
        .contains(Capability::Retention)
        || release.driver != DriverKind::linux_ssh()
        || release.project_id != context.project_id
        || release.environment_id != context.environment_id
        || release.component != context.component
        || release.generation != context.generation
        || release.destination != context.destination
        || release.destination_revision != context.destination_revision
        || release.endpoint_fingerprint != context.endpoint_fingerprint
    {
        return Err(error(
            context,
            "identity",
            "Release identity differs from execution context",
            false,
        ));
    }
    Ok(())
}
fn error(
    context: &ComponentExecutionContext,
    stage: &str,
    message: &str,
    blocked: bool,
) -> DriverError {
    DriverError {
        stage: stage.into(),
        target: format!("{} at {}", context.component, context.destination),
        message: message.into(),
        suggested_action: if blocked {
            "Inspect the operation outcome before any recovery"
        } else {
            "Review the application deployment configuration"
        }
        .into(),
        recovery_blocked: blocked,
    }
}
