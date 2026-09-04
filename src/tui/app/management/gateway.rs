use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    application::{
        DeploymentSession, OrchestrationError, RecoveryError, RecoveryService, RollbackService,
        RollbackServiceError, history_query::HistoryQueryService,
    },
    config::CredentialRegistry,
    telemetry::Redactor,
};

use super::{Arc, CancellationToken, ManagementPage, ManagementRequest, ManagementScope, PathBuf};

#[async_trait(?Send)]
pub(in crate::tui::app) trait ManagementGateway:
    std::fmt::Debug + Send + Sync
{
    async fn run(
        &self,
        scope: &ManagementScope,
        request: ManagementRequest,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String>;
}

#[derive(Debug)]
pub(in crate::tui::app) struct LocalManagementGateway {
    pub destinations: PathBuf,
    pub credentials: PathBuf,
    pub history: PathBuf,
    pub session: Arc<DeploymentSession>,
}

impl LocalManagementGateway {
    fn drivers(&self) -> Result<Arc<crate::drivers::DriverRegistry>, String> {
        let credentials = CredentialRegistry::load(&self.credentials).map_err(|_| {
            "Credential registry is unreadable or invalid; check saved identities in connection management."
                .to_owned()
        })?;
        crate::bootstrap::deployment_driver_registry(Arc::new(credentials))
            .map(Arc::new)
            .map_err(|_| {
                "Could not initialize deployment connections; check saved identities.".into()
            })
    }
}

#[async_trait(?Send)]
impl ManagementGateway for LocalManagementGateway {
    async fn run(
        &self,
        scope: &ManagementScope,
        request: ManagementRequest,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        if cancellation.is_cancelled() {
            return Err("Operation cancelled.".into());
        }
        match request {
            request @ (ManagementRequest::Environments(_)
            | ManagementRequest::History(_)
            | ManagementRequest::Detail(_)
            | ManagementRequest::Logs { .. }
            | ManagementRequest::Reports(_)
            | ManagementRequest::Report(_)) => self.local(scope, request),
            request @ (ManagementRequest::Inspect { .. }
            | ManagementRequest::Candidates { .. }
            | ManagementRequest::Plan { .. }
            | ManagementRequest::Execute(_)) => self.remote(scope, request, cancellation).await,
        }
    }
}

impl LocalManagementGateway {
    fn local(
        &self,
        scope: &ManagementScope,
        request: ManagementRequest,
    ) -> Result<ManagementPage, String> {
        let history = HistoryQueryService::new(self.history.clone(), Redactor::default());
        let project = &scope.config.project_id;
        if let ManagementRequest::Environments(query) = &request {
            return history
                .environments(project, *query)
                .map(|page| ManagementPage::Environments {
                    page: Arc::new(page),
                    offset: query.offset,
                    cursor: 0,
                })
                .map_err(|error| error.to_string());
        }
        let environment = scope
            .environment_id()
            .ok_or("The selected Environment is no longer available.")?;
        match request {
            ManagementRequest::History(query) => history
                .deployments(project, environment, query)
                .map(|page| ManagementPage::History {
                    page: Arc::new(page),
                    offset: query.offset,
                    cursor: 0,
                })
                .map_err(|error| error.to_string()),
            ManagementRequest::Detail(id) => history
                .deployment(project, environment, &id)
                .map(|details| ManagementPage::Detail(Arc::new(details)))
                .map_err(|error| error.to_string()),
            ManagementRequest::Logs {
                details,
                query,
                previous_offsets,
            } => {
                let page = history
                    .log_page(project, environment, &details.record.deployment, query)
                    .map_err(|error| error.to_string())?;
                Ok(ManagementPage::Logs {
                    details,
                    page: Arc::new(page),
                    query,
                    previous_offsets,
                })
            }
            ManagementRequest::Reports(query) => history
                .recovery_reports(project, environment, query)
                .map(|page| ManagementPage::Reports {
                    page: Arc::new(page),
                    offset: query.offset,
                    cursor: 0,
                })
                .map_err(|error| error.to_string()),
            ManagementRequest::Report(id) => history
                .recovery_report(project, environment, &id)
                .map(|report| ManagementPage::Report {
                    report: Arc::new(report),
                    warning: None,
                })
                .map_err(|error| error.to_string()),
            _ => unreachable!("only local queries are routed here"),
        }
    }

    async fn remote(
        &self,
        scope: &ManagementScope,
        request: ManagementRequest,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        // Even a manually constructed Execute request cannot cross from a
        // historical identity into the current configuration or connections.
        if scope.historical_environment.is_some() {
            return Err(super::HISTORICAL_READ_ONLY.into());
        }
        match request {
            ManagementRequest::Inspect { source, selected } => {
                let service = RecoveryService::new(
                    self.drivers()?,
                    self.history.clone(),
                    Arc::clone(&self.session),
                );
                let inspection = service
                    .inspect(
                        scope.selection(selected),
                        source,
                        &self.destinations,
                        cancellation,
                    )
                    .await
                    .map_err(|error| inspection_error(&error))?;
                Ok(ManagementPage::Report {
                    report: Arc::new(inspection.report),
                    warning: inspection.persistence_warning,
                })
            }
            ManagementRequest::Candidates { source, selected } => {
                let service = RollbackService::new(
                    self.drivers()?,
                    self.history.clone(),
                    Arc::clone(&self.session),
                );
                let candidates = service
                    .candidates(
                        scope.selection(selected),
                        source,
                        &self.destinations,
                        cancellation,
                    )
                    .await
                    .map_err(rollback_error)?;
                Ok(ManagementPage::RollbackTargets {
                    candidates: Arc::new(candidates),
                    selected: BTreeSet::new(),
                    options: BTreeMap::new(),
                    cursor: 0,
                })
            }
            ManagementRequest::Plan { source, targets } => {
                let service = RollbackService::new(
                    self.drivers()?,
                    self.history.clone(),
                    Arc::clone(&self.session),
                );
                let plan = service
                    .plan(
                        scope.selection(targets.keys().cloned().collect()),
                        source,
                        targets,
                        &self.destinations,
                        cancellation,
                    )
                    .await
                    .map_err(rollback_error)?;
                Ok(ManagementPage::RollbackReview(Arc::new(plan)))
            }
            ManagementRequest::Execute(plan) => {
                let service = RollbackService::new(
                    self.drivers()?,
                    self.history.clone(),
                    Arc::clone(&self.session),
                );
                let report = service
                    .execute((*plan).clone(), &self.destinations, cancellation)
                    .await
                    .map_err(rollback_error)?;
                Ok(ManagementPage::RollbackFinished(Arc::new(report)))
            }
            _ => unreachable!("only explicit checks and rollback are routed here"),
        }
    }
}

fn inspection_error(error: &RecoveryError) -> String {
    match error {
        RecoveryError::History(_) =>
            "Inspection history could not be read or saved. No remote state was changed; inspect again after checking local storage.".into(),
        _ => error.to_string(),
    }
}

fn rollback_error(error: RollbackServiceError) -> String {
    match error {
        RollbackServiceError::History(_) =>
            "Rollback history is unavailable. Check local storage and inspect current state before retrying.".into(),
        RollbackServiceError::Orchestration(OrchestrationError::Execution { deployment, .. }) => format!(
            "Rollback Deployment {deployment} did not complete normally. Its recorded outcome and remote state may need inspection; do not retry blindly."
        ),
        RollbackServiceError::Orchestration(_) =>
            "Rollback could not complete. Check local history and inspect current state before retrying.".into(),
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    fn database_error() -> crate::history::HistoryError {
        let database = rusqlite::Connection::open_in_memory().unwrap();
        database.execute_batch(
            "CREATE TABLE fault (id INTEGER); CREATE TRIGGER fail BEFORE INSERT ON fault BEGIN SELECT RAISE(ABORT, 'private-history-sentinel'); END;"
        ).unwrap();
        let error = database
            .execute("INSERT INTO fault VALUES (1)", [])
            .unwrap_err();
        assert!(error.to_string().contains("private-history-sentinel"));
        error.into()
    }

    #[test]
    fn database_diagnostics_are_hidden_without_losing_the_failed_deployment_identity() {
        let deployment = crate::domain::DeploymentId::new();
        let execution = rollback_error(RollbackServiceError::Orchestration(
            OrchestrationError::Execution {
                deployment: deployment.clone(),
                source: Box::new(OrchestrationError::History(database_error())),
                persistence: Some("private-history-sentinel".into()),
            },
        ));
        assert!(execution.contains(&deployment.to_string()));
        assert!(execution.contains("inspect"));
        for message in [
            execution,
            rollback_error(RollbackServiceError::History(database_error())),
            rollback_error(RollbackServiceError::Orchestration(
                OrchestrationError::History(database_error()),
            )),
            inspection_error(&RecoveryError::History(database_error())),
        ] {
            assert!(!message.contains("private-history-sentinel"));
        }
    }
}
