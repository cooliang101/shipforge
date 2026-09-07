use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::{
    domain::{DeploymentId, ReleaseVersion},
    drivers::{
        ActivationReceipt, ComponentExecutionContext, DriverError, ReleasePackage, ReleaseRef,
        audit::{
            RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPackage, RemoteAuditPhase,
            RemoteAuditRecord,
        },
    },
};

use super::{ActivationOptions, AuthenticatedSession, DeploymentMarker, LinuxSshTarget};

/// Frozen phase identity; no connection configuration or secret is serialized.
pub(super) struct AuditPhase<'a> {
    pub deployment: &'a DeploymentId,
    pub context: &'a ComponentExecutionContext,
    pub release: &'a ReleaseRef,
    pub phase: RemoteAuditPhase,
    pub expected_current: Option<ReleaseVersion>,
    pub target: Option<ReleaseVersion>,
}

impl AuditPhase<'_> {
    pub(super) fn record(&self) -> Result<RemoteAuditRecord, DriverError> {
        let recorded_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .ok_or_else(|| DriverError {
                recovery_blocked: false,
                stage: "audit".into(),
                target: self.context.component.to_string(),
                message: "system clock cannot timestamp the remote audit".into(),
                suggested_action: "correct the system clock and inspect deployment history".into(),
            })?;
        Ok(RemoteAuditRecord {
            schema_version: 1,
            event_id: uuid::Uuid::now_v7(),
            deployment: self.deployment.clone(),
            recorded_at_ms,
            release: self.release.clone(),
            phase: self.phase,
            outcome: RemoteAuditOutcome::Succeeded,
            expected_current: self.expected_current.clone(),
            target: self.target.clone(),
            observed: RemoteAuditObserved::Unknown,
            healthy: None,
            package: None,
        })
    }
}

pub(super) async fn record_prepared(
    session: &AuthenticatedSession,
    target: &LinuxSshTarget,
    phase: &AuditPhase<'_>,
    package: &ReleasePackage,
) -> Result<(), DriverError> {
    let mut record = phase.record()?;
    record.package = Some(RemoteAuditPackage {
        manifest: package.manifest().clone(),
        sha256: package.sha256().to_owned(),
        size: package.size(),
    });
    session
        .append_audit(
            target,
            &DeploymentMarker::for_context(phase.context),
            &record,
            &CancellationToken::new(),
        )
        .await
        .map_err(|source| DriverError {
            recovery_blocked: false,
            stage: "prepare.audit".into(),
            target: phase.context.component.to_string(),
            message: source.to_string(),
            suggested_action:
                "inspect the staged Release and remote audit directory before retrying".into(),
        })
}

/// Audit is attempted after the business operation (including its compensation).
/// A failed append must neither replace known effects nor prevent compensation.
pub(super) async fn record_phase_result(
    session: &AuthenticatedSession,
    target: &LinuxSshTarget,
    phase: &AuditPhase<'_>,
    mut result: Result<ActivationReceipt, DriverError>,
) -> Result<ActivationReceipt, DriverError> {
    let mut record = match phase.record() {
        Ok(record) => record,
        Err(error) => return with_warning(result, &error.to_string()),
    };
    if let Ok(receipt) = &result {
        record.observed = receipt
            .current
            .as_ref()
            .map_or(RemoteAuditObserved::NotDeployed, |release| {
                RemoteAuditObserved::Release(release.version.clone())
            });
        record.healthy = receipt.current.as_ref().map(|_| receipt.healthy);
    } else {
        record.outcome = RemoteAuditOutcome::Failed;
        // A restored current link does not constitute a fresh health check.
        // Failure diagnostics are bounded independently of user cancellation.
        let cancellation = CancellationToken::new();
        let observation = async {
            let current = session
                .observe_current(target, ActivationOptions::default(), &cancellation)
                .await
                .ok()?;
            if let Some(version) = &current {
                session
                    .check_release_manifest(
                        target,
                        &DeploymentMarker::for_context(phase.context),
                        version,
                        &cancellation,
                    )
                    .await
                    .ok()?;
            }
            Some(current)
        };
        if let Ok(Some(observed)) = tokio::time::timeout(Duration::from_secs(10), observation).await
        {
            record.observed = observed.map_or(
                RemoteAuditObserved::NotDeployed,
                RemoteAuditObserved::Release,
            );
        }
    }
    if let Err(error) = session
        .append_audit(
            target,
            &DeploymentMarker::for_context(phase.context),
            &record,
            &CancellationToken::new(),
        )
        .await
    {
        result = with_warning(result, &error.to_string());
    }
    result
}

fn with_warning(
    mut result: Result<ActivationReceipt, DriverError>,
    reason: &str,
) -> Result<ActivationReceipt, DriverError> {
    let warning = format!(
        "remote audit was not saved ({reason}); inspect remote state before relying on historical records"
    );
    match &mut result {
        Ok(receipt) => receipt.warnings.push(warning),
        Err(error) => {
            error.message.push_str("; ");
            error.message.push_str(&warning);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_failure_preserves_successful_undeploy_receipt() {
        let receipt = with_warning(
            Ok(ActivationReceipt {
                current: None,
                healthy: true,
                warnings: Vec::new(),
            }),
            "injected write failure",
        )
        .unwrap();
        assert!(receipt.current.is_none());
        assert!(receipt.healthy);
        assert_eq!(receipt.warnings.len(), 1);
    }

    #[test]
    fn audit_failure_keeps_original_operation_error_and_stage() {
        let error = with_warning(
            Err(DriverError {
                recovery_blocked: false,
                stage: "health".into(),
                target: "api".into(),
                message: "unhealthy; restored previous version".into(),
                suggested_action: "inspect service".into(),
            }),
            "injected write failure",
        )
        .unwrap_err();
        assert_eq!(error.stage, "health");
        assert!(
            error
                .message
                .starts_with("unhealthy; restored previous version")
        );
        assert!(error.message.contains("remote audit was not saved"));
    }
}
