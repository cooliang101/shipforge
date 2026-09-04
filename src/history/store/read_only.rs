use std::{path::Path, time::Duration};

use rusqlite::OpenFlags;

use super::{
    Connection, DeploymentComponentSnapshot, DeploymentId, DeploymentLogRecord, DeploymentMetadata,
    DeploymentQuery, DeploymentRecord, EnvironmentId, HistoryError, HistoryStore, IntentRecord,
    ObservationRecord, PersistedComponentResult, ProjectId, RecoveryQuery, RecoveryReport,
    ReleasePackageRecord, ReleaseReceiptRecord, StepRecord, params,
};

const MAX_DETAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DETAIL_ROWS: usize = 4096;
const MAX_ENVIRONMENT_IDS: u32 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentDetails {
    pub record: DeploymentRecord,
    pub metadata: Option<DeploymentMetadata>,
    pub snapshots: Vec<DeploymentComponentSnapshot>,
    pub results: Vec<PersistedComponentResult>,
    pub steps: Vec<StepRecord>,
    pub observations: Vec<ObservationRecord>,
    pub pending: Vec<IntentRecord>,
    pub packages: Vec<ReleasePackageRecord>,
    pub receipts: Vec<ReleaseReceiptRecord>,
    pub log: Option<DeploymentLogRecord>,
}

impl HistoryStore {
    /// Opens an existing current-schema database without creation or migration.
    /// `SQLite` may maintain its own WAL coordination sidecars; history stays read-only.
    ///
    /// # Errors
    /// Rejects missing, linked, non-file, uninitialized, old or future databases.
    pub fn open_existing_read_only(path: &Path) -> Result<Self, HistoryError> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(source) => {
                if source.kind() == std::io::ErrorKind::NotFound {
                    for suffix in ["-wal", "-shm"] {
                        let mut sidecar = path.as_os_str().to_os_string();
                        sidecar.push(suffix);
                        match std::fs::symlink_metadata(Path::new(&sidecar)) {
                            Ok(_) => {
                                return invalid(
                                    "history database is missing but SQLite sidecars remain",
                                );
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(HistoryError::Io {
                                    path: sidecar.into(),
                                    source: error,
                                });
                            }
                        }
                    }
                }
                return Err(HistoryError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return invalid("history path is not a regular unlinked file");
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(1))?;
        connection.pragma_update(None, "query_only", true)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        let store = Self { connection };
        let schema = store.schema_version()?;
        if schema > super::LATEST_SCHEMA_VERSION {
            return Err(HistoryError::UnsupportedSchema(schema));
        }
        if schema == 0 {
            return invalid("history schema is uninitialized");
        }
        if schema != super::LATEST_SCHEMA_VERSION {
            return Err(HistoryError::InvalidMetadata(
                "history schema requires upgrade before viewing",
            ));
        }
        Ok(store)
    }

    /// Lists historical Environment identities, including report-only environments.
    /// The complete bounded index is validated before returning a page; it supplies
    /// no historical configuration or authority for remote operations.
    ///
    /// # Errors
    /// Rejects invalid pages, malformed identities and more than 4096 distinct IDs.
    pub fn read_environment_page(
        &self,
        project: &ProjectId,
        query: RecoveryQuery,
    ) -> Result<(Vec<EnvironmentId>, bool), HistoryError> {
        validate_page(query.limit, query.offset)?;
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            "WITH deployment_environments(environment_id) AS (
               SELECT DISTINCT CASE WHEN typeof(environment_id)='text'
                 AND length(CAST(environment_id AS BLOB))<=68
                 THEN environment_id ELSE '' END COLLATE BINARY
               FROM deployments WHERE project_id=?1 LIMIT ?2
             ), report_environments(environment_id) AS (
               SELECT DISTINCT CASE WHEN typeof(environment_id)='text'
                 AND length(CAST(environment_id AS BLOB))<=68
                 THEN environment_id ELSE '' END COLLATE BINARY
               FROM recovery_reports WHERE project_id=?1 LIMIT ?2
             ), environments(environment_id) AS (
               SELECT environment_id FROM deployment_environments
               UNION SELECT environment_id FROM report_environments
             )
             SELECT environment_id FROM environments
             ORDER BY environment_id COLLATE BINARY LIMIT ?2",
        )?;
        let mut environments = Vec::new();
        for row in statement.query_map(
            params![project.to_string(), MAX_ENVIRONMENT_IDS + 1],
            |row| row.get::<_, String>(0),
        )? {
            if environments.len() >= MAX_ENVIRONMENT_IDS as usize {
                return invalid("historical Environment index exceeds bounded read limits");
            }
            environments.push(
                row?.parse::<EnvironmentId>()
                    .map_err(|_| corrupt("invalid historical Environment identity"))?,
            );
        }
        let end = query.offset as usize + query.limit as usize;
        let more = environments.len() > end;
        let items = environments
            .into_iter()
            .skip(query.offset as usize)
            .take(query.limit as usize)
            .collect();
        drop(statement);
        transaction.commit()?;
        Ok((items, more))
    }

    /// Lists original history within one read snapshot, with one look-ahead row.
    ///
    /// # Errors
    /// Rejects invalid pages and corrupt or oversized persisted identities.
    pub fn read_deployment_page(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: DeploymentQuery,
    ) -> Result<(Vec<DeploymentRecord>, bool), HistoryError> {
        validate_page(query.limit, query.offset)?;
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            "SELECT CASE WHEN length(CAST(id AS BLOB))=36 THEN id ELSE '' END
             FROM deployments WHERE project_id=?1 AND environment_id=?2
             AND (?3=0 OR state IN ('created','running'))
             ORDER BY created_at_ms DESC,id DESC LIMIT ?4 OFFSET ?5",
        )?;
        let ids = statement
            .query_map(
                params![
                    project.to_string(),
                    environment.to_string(),
                    query.nonterminal_only,
                    query.limit + 1,
                    query.offset
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let more = ids.len() > query.limit as usize;
        let mut records = Vec::new();
        for id in ids.into_iter().take(query.limit as usize) {
            let id = id
                .parse()
                .map_err(|_| corrupt("invalid Deployment identity"))?;
            bound_record(&transaction, &id)?;
            records.push(
                self.deployment(&id)?
                    .ok_or_else(|| corrupt("missing Deployment"))?,
            );
        }
        drop(statement);
        transaction.commit()?;
        Ok((records, more))
    }

    /// Reads every detail from a bounded, coherent local snapshot.
    /// Legacy unavailable metadata remains absent rather than being reconstructed.
    ///
    /// # Errors
    /// Rejects corrupt, oversized or out-of-scope records without returning a prefix.
    pub fn read_deployment_details(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        deployment: &DeploymentId,
    ) -> Result<Option<DeploymentDetails>, HistoryError> {
        let transaction = self.connection.unchecked_transaction()?;
        bound_record(&transaction, deployment)?;
        let Some(record) = self.deployment(deployment)? else {
            return Ok(None);
        };
        if record.project != *project || record.environment != *environment {
            return Ok(None);
        }
        bound_details(&transaction, deployment)?;
        let details = DeploymentDetails {
            record,
            metadata: self.deployment_metadata(deployment)?,
            snapshots: self.component_snapshots(deployment)?,
            results: self.component_results(deployment)?,
            steps: self.steps(deployment)?,
            observations: self.observations(deployment)?,
            pending: self.pending_intents(deployment)?,
            packages: self.release_packages(deployment)?,
            receipts: self.release_receipts(deployment)?,
            log: self.deployment_log(deployment)?,
        };
        validate_pending(&details)?;
        transaction.commit()?;
        Ok(Some(details))
    }

    /// Reads saved inspection reports by insertion order, not wall-clock order.
    ///
    /// # Errors
    /// Rejects invalid pages, corrupt reports and pages exceeding eight MiB.
    pub fn read_recovery_page(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: RecoveryQuery,
    ) -> Result<(Vec<RecoveryReport>, bool), HistoryError> {
        validate_page(query.limit, query.offset)?;
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            "SELECT CASE WHEN length(CAST(id AS BLOB))=36 THEN id ELSE '' END,
             length(CAST(report AS BLOB)) FROM recovery_reports
             WHERE project_id=?1 AND environment_id=?2 ORDER BY rowid DESC LIMIT ?3 OFFSET ?4",
        )?;
        let rows = statement
            .query_map(
                params![
                    project.to_string(),
                    environment.to_string(),
                    query.limit + 1,
                    query.offset
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let more = rows.len() > query.limit as usize;
        let mut total = 0_u64;
        let mut reports = Vec::new();
        for (id, bytes) in rows.into_iter().take(query.limit as usize) {
            total = total
                .saturating_add(u64::try_from(bytes).map_err(|_| corrupt("invalid report size"))?);
            if total > 8 * 1024 * 1024 {
                return invalid("history report page exceeds byte limit");
            }
            let id = uuid::Uuid::parse_str(&id).map_err(|_| corrupt("invalid report identity"))?;
            let report = self
                .recovery_report(&id)?
                .ok_or_else(|| corrupt("missing report"))?;
            if !report_scope(&report, project, environment) {
                return invalid("report scope differs from its index");
            }
            reports.push(report);
        }
        drop(statement);
        transaction.commit()?;
        Ok((reports, more))
    }

    /// Loads a saved report without consulting or modifying its historical source.
    ///
    /// # Errors
    /// Rejects malformed stored evidence and database errors.
    pub fn read_recovery_detail(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        id: &uuid::Uuid,
    ) -> Result<Option<RecoveryReport>, HistoryError> {
        let transaction = self.connection.unchecked_transaction()?;
        let report = self
            .recovery_report(id)?
            .filter(|report| report_scope(report, project, environment));
        transaction.commit()?;
        Ok(report)
    }
}

fn report_scope(report: &RecoveryReport, project: &ProjectId, environment: &EnvironmentId) -> bool {
    !report.components.is_empty()
        && report.components.iter().all(|component| {
            component.scope.project == *project && component.scope.environment == *environment
        })
}

fn validate_page(limit: u32, offset: u32) -> Result<(), HistoryError> {
    if !(1..=100).contains(&limit) || offset > 1_000_000 {
        Err(HistoryError::InvalidPage)
    } else {
        Ok(())
    }
}

fn bound_record(connection: &Connection, deployment: &DeploymentId) -> Result<(), HistoryError> {
    let invalid: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND
         (length(CAST(project_id AS BLOB))>80 OR length(CAST(environment_id AS BLOB))>80
         OR length(CAST(related_deployment_id AS BLOB))>80
         OR length(CAST(kind AS BLOB))>8 OR length(CAST(state AS BLOB))>9))",
        [deployment.to_string()],
        |row| row.get(0),
    )?;
    if invalid {
        return self::invalid("history identity exceeds byte limit");
    }
    Ok(())
}

fn bound_details(connection: &Connection, deployment: &DeploymentId) -> Result<(), HistoryError> {
    let mut bytes = 0_u64;
    let mut count = 0_usize;
    for (table, columns, limit) in [
        ("deployment_metadata", "metadata", 1_u32),
        ("deployment_logs", "relative_path,format", 1),
        ("component_snapshots", "component,snapshot", 256),
        (
            "component_results",
            "component,outcome,attempted_release,observed_release,error",
            256,
        ),
        (
            "release_packages",
            "component,release_ref,manifest,sha256",
            256,
        ),
        ("release_receipts", "component,stage,release_ref", 4096),
        (
            "operation_intents",
            "component,stage,target,status,error",
            4096,
        ),
        ("deployment_steps", "component,name,status,error", 4096),
        (
            "deployment_observations",
            "component,stage,observed_ref,error",
            4096,
        ),
    ] {
        let expression = columns
            .split(',')
            .map(|column| format!("COALESCE(length(CAST({column} AS BLOB)),0)"))
            .collect::<Vec<_>>()
            .join("+");
        let mut statement = connection.prepare(&format!(
            "SELECT {expression} FROM {table} WHERE deployment_id=?1 LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![deployment.to_string(), limit + 1], |row| {
            row.get::<_, i64>(0)
        })?;
        for (index, size) in rows.enumerate() {
            count += 1;
            bytes = bytes.saturating_add(
                u64::try_from(size?).map_err(|_| corrupt("invalid metadata size"))?,
            );
            if index >= limit as usize || count > MAX_DETAIL_ROWS || bytes > MAX_DETAIL_BYTES {
                return invalid("history details exceed bounded read limits");
            }
        }
    }
    Ok(())
}

fn validate_pending(details: &DeploymentDetails) -> Result<(), HistoryError> {
    if details.record.kind != super::DeploymentKind::Deploy && !details.packages.is_empty() {
        return invalid("rollback history contains an impossible package record");
    }
    for step in &details.steps {
        if step
            .started_at_ms
            .is_some_and(|time| time < details.record.created_at_ms)
            || step
                .completed_at_ms
                .is_some_and(|time| time < details.record.created_at_ms)
            || (!details.snapshots.is_empty()
                && !details
                    .snapshots
                    .iter()
                    .any(|snapshot| snapshot.release.component == step.component))
        {
            return invalid("step predates its Deployment or is outside its frozen selection");
        }
    }
    for pending in &details.pending {
        if pending.id.0 <= 0 || pending.created_at_ms < details.record.created_at_ms {
            return invalid("pending intent has invalid identity or time");
        }
        super::validate_text("step", &pending.stage)?;
        super::validate_text("target", &pending.target)?;
    }
    Ok(())
}

fn corrupt(message: &str) -> HistoryError {
    HistoryError::Corrupt(message.into())
}
fn invalid<T>(message: &str) -> Result<T, HistoryError> {
    Err(corrupt(message))
}

#[cfg(test)]
mod tests;
