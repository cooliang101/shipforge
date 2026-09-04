use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::domain::{
    ComponentDeploymentResult, ComponentName, ComponentOutcome, DeploymentId, DeploymentState,
    EnvironmentId, ProjectId, ReleaseVersion,
};
use crate::telemetry::Redactor;

mod details;
pub use details::*;

const LATEST_SCHEMA_VERSION: u32 = 5;
const MIGRATION_1: &str = r"
CREATE TABLE deployments (
 id TEXT PRIMARY KEY NOT NULL, project_id TEXT NOT NULL, environment_id TEXT NOT NULL,
 state TEXT NOT NULL CHECK (state IN ('created','running','succeeded','failed','cancelled')),
 created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
 updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
) STRICT;
CREATE TABLE operation_intents (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 deployment_id TEXT NOT NULL REFERENCES deployments(id), component TEXT NOT NULL,
 stage TEXT NOT NULL, target TEXT NOT NULL,
 status TEXT NOT NULL CHECK (status IN ('pending','succeeded','failed')),
 created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0), completed_at_ms INTEGER, error TEXT,
 CHECK ((status='pending' AND completed_at_ms IS NULL AND error IS NULL)
     OR (status='succeeded' AND completed_at_ms >= created_at_ms AND error IS NULL)
     OR (status='failed' AND completed_at_ms >= created_at_ms AND error IS NOT NULL))
) STRICT;
CREATE INDEX operation_intents_pending ON operation_intents(deployment_id,status,id);
";
const MIGRATION_2: &str = r"
CREATE TABLE component_results (
 deployment_id TEXT NOT NULL REFERENCES deployments(id), component TEXT NOT NULL,
 outcome TEXT NOT NULL CHECK (outcome IN ('succeeded','failed','cancelled','compensated','compensation_failed')),
 attempted_release TEXT NOT NULL, observed_release TEXT, error TEXT,
 PRIMARY KEY (deployment_id,component)
) STRICT;
";
const MIGRATION_3: &str = r"
ALTER TABLE deployments ADD COLUMN kind TEXT NOT NULL DEFAULT 'deploy'
 CHECK (kind IN ('deploy','rollback'));
ALTER TABLE deployments ADD COLUMN related_deployment_id TEXT REFERENCES deployments(id);
CREATE TABLE component_results_v3 (
 deployment_id TEXT NOT NULL REFERENCES deployments(id), component TEXT NOT NULL,
 outcome TEXT NOT NULL CHECK (outcome IN ('succeeded','failed','cancelled','compensated','compensation_failed')),
 attempted_release TEXT, observed_release TEXT, error TEXT,
 PRIMARY KEY (deployment_id,component)
) STRICT;
INSERT INTO component_results_v3 SELECT * FROM component_results;
DROP TABLE component_results;
ALTER TABLE component_results_v3 RENAME TO component_results;
";
const MIGRATION_4: &str = r"
CREATE TABLE deployment_logs (
 deployment_id TEXT PRIMARY KEY NOT NULL REFERENCES deployments(id),
 relative_path TEXT NOT NULL UNIQUE
   CHECK (relative_path = 'logs/' || deployment_id || '.log'),
 max_bytes INTEGER NOT NULL CHECK (max_bytes > 0),
 retained_files INTEGER NOT NULL CHECK (retained_files > 0)
) STRICT;
";

pub struct HistoryStore {
    connection: Connection,
}

impl fmt::Debug for HistoryStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryStore")
            .finish_non_exhaustive()
    }
}

impl HistoryStore {
    /// Opens or creates the local history database and applies migrations.
    ///
    /// # Errors
    ///
    /// Returns an error for directory creation, `SQLite` configuration, or a
    /// failed/unsupported migration.
    pub fn open(path: &Path) -> Result<Self, HistoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| HistoryError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let connection = Connection::open(path)?;
        #[cfg(unix)]
        secure_file_permissions(path)?;
        Self::configure(connection)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self, HistoryError> {
        Self::configure(Connection::open_in_memory()?)
    }

    fn configure(connection: Connection) -> Result<Self, HistoryError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), HistoryError> {
        let version = self.schema_version()?;
        if version > LATEST_SCHEMA_VERSION {
            return Err(HistoryError::UnsupportedSchema(version));
        }
        if version == 0 {
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(MIGRATION_1)?;
            transaction.pragma_update(None, "user_version", 1_u32)?;
            transaction.commit()?;
        }
        if self.schema_version()? == 1 {
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(MIGRATION_2)?;
            transaction.pragma_update(None, "user_version", 2_u32)?;
            transaction.commit()?;
        }
        if self.schema_version()? == 2 {
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(MIGRATION_3)?;
            transaction.pragma_update(None, "user_version", 3_u32)?;
            transaction.commit()?;
        }
        if self.schema_version()? == 3 {
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(MIGRATION_4)?;
            transaction.pragma_update(None, "user_version", 4_u32)?;
            transaction.commit()?;
        }
        if self.schema_version()? == 4 {
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(details::MIGRATION_5)?;
            transaction.pragma_update(None, "user_version", 5_u32)?;
            transaction.commit()?;
        }
        Ok(())
    }

    /// Returns the applied numbered schema version.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot read `user_version`.
    pub fn schema_version(&self) -> Result<u32, HistoryError> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(HistoryError::Sqlite)
    }

    /// Persists a newly created Deployment before it can run.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate identity, invalid timestamp, or `SQLite` failure.
    pub fn create_deployment(
        &self,
        deployment: &DeploymentId,
        project: &ProjectId,
        environment: &EnvironmentId,
        created_at_ms: u64,
    ) -> Result<(), HistoryError> {
        let timestamp = timestamp(created_at_ms)?;
        self.connection.execute(
            "INSERT INTO deployments (id,project_id,environment_id,state,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'created',?4,?4)",
            params![deployment.to_string(), project.to_string(), environment.to_string(), timestamp],
        )?;
        Ok(())
    }

    /// Indexes one bounded log beneath the history database's sibling `logs` directory.
    ///
    /// The path is generated from the Deployment ID, never supplied by output
    /// or configuration. `retained_files` counts rotated files, excluding the
    /// active log. Registration does not create or modify log files.
    ///
    /// # Errors
    ///
    /// Returns an error for zero/out-of-range limits, an unknown Deployment,
    /// duplicate registration, or database failure.
    pub fn register_deployment_log(
        &self,
        deployment: &DeploymentId,
        max_bytes: u64,
        retained_files: u32,
    ) -> Result<DeploymentLogRecord, HistoryError> {
        if max_bytes == 0 || retained_files == 0 {
            return Err(HistoryError::InvalidLogLimits);
        }
        let stored_max = i64::try_from(max_bytes).map_err(|_| HistoryError::InvalidLogLimits)?;
        let relative_path = format!("logs/{deployment}.log");
        self.connection.execute(
            "INSERT INTO deployment_logs (deployment_id,relative_path,max_bytes,retained_files)
             VALUES (?1,?2,?3,?4)",
            params![
                deployment.to_string(),
                relative_path,
                stored_max,
                retained_files
            ],
        )?;
        Ok(DeploymentLogRecord {
            deployment: deployment.clone(),
            relative_path: relative_path.into(),
            max_bytes,
            retained_files,
        })
    }

    /// Returns the registered relative log path and rotation limits, if present.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid persisted metadata or a database failure.
    pub fn deployment_log(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<DeploymentLogRecord>, HistoryError> {
        let row = self.connection.query_row(
            "SELECT relative_path,max_bytes,retained_files FROM deployment_logs WHERE deployment_id=?1",
            [deployment.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, u32>(2)?)),
        ).optional()?;
        let Some((relative_path, max_bytes, retained_files)) = row else {
            return Ok(None);
        };
        let max_bytes = u64::try_from(max_bytes)
            .map_err(|_| HistoryError::Corrupt("invalid Deployment log size limit".into()))?;
        if relative_path != format!("logs/{deployment}.log")
            || max_bytes == 0
            || retained_files == 0
        {
            return Err(HistoryError::Corrupt(
                "invalid Deployment log metadata".into(),
            ));
        }
        Ok(Some(DeploymentLogRecord {
            deployment: deployment.clone(),
            relative_path: relative_path.into(),
            max_bytes,
            retained_files,
        }))
    }

    /// Persists a Rollback Deployment linked to an existing Deployment in the
    /// same Project and Environment.
    ///
    /// # Errors
    ///
    /// Returns an error when the related Deployment is absent or belongs to a
    /// different scope, or when persistence fails.
    pub fn create_rollback_deployment(
        &self,
        deployment: &DeploymentId,
        related: &DeploymentId,
        project: &ProjectId,
        environment: &EnvironmentId,
        created_at_ms: u64,
    ) -> Result<(), HistoryError> {
        let timestamp = timestamp(created_at_ms)?;
        let inserted = self.connection.execute(
            "INSERT INTO deployments
             (id,project_id,environment_id,state,created_at_ms,updated_at_ms,kind,related_deployment_id)
             SELECT ?1,project_id,environment_id,'created',?5,?5,'rollback',id
             FROM deployments WHERE id=?2 AND project_id=?3 AND environment_id=?4
              AND state IN ('succeeded','failed')",
            params![
                deployment.to_string(),
                related.to_string(),
                project.to_string(),
                environment.to_string(),
                timestamp,
            ],
        )?;
        if inserted == 1 {
            Ok(())
        } else {
            Err(HistoryError::MissingRelatedDeployment(related.clone()))
        }
    }

    /// Atomically updates a Deployment from the expected state.
    ///
    /// # Errors
    ///
    /// Returns an error when absent, concurrently changed, invalid, or `SQLite` fails.
    pub fn transition_deployment(
        &self,
        deployment: &DeploymentId,
        expected: DeploymentState,
        next: DeploymentState,
        updated_at_ms: u64,
    ) -> Result<(), HistoryError> {
        if !valid_transition(expected, next) {
            return Err(HistoryError::InvalidTransition { expected, next });
        }
        let transaction = self.connection.unchecked_transaction()?;
        let updated = transaction.execute(
            "UPDATE deployments SET state=?1,updated_at_ms=?2 WHERE id=?3 AND state=?4 AND updated_at_ms<=?2",
            params![
                state(next),
                timestamp(updated_at_ms)?,
                deployment.to_string(),
                state(expected)
            ],
        )?;
        if updated == 1 {
            if matches!(
                next,
                DeploymentState::Succeeded | DeploymentState::Failed | DeploymentState::Cancelled
            ) {
                transaction.execute(
                    "UPDATE deployment_steps SET status='skipped',completed_at_ms=?2 WHERE deployment_id=?1 AND status='pending'",
                    params![deployment.to_string(), timestamp(updated_at_ms)?],
                )?;
            }
            transaction.commit()?;
            Ok(())
        } else {
            Err(HistoryError::StateConflict(deployment.clone()))
        }
    }

    /// Records durable intent immediately before one external side effect.
    ///
    /// # Errors
    ///
    /// Returns an error unless the Deployment is running and fields are valid.
    pub fn record_intent(
        &self,
        deployment: &DeploymentId,
        component: &ComponentName,
        stage: &str,
        target: &str,
        created_at_ms: u64,
    ) -> Result<IntentId, HistoryError> {
        validate_text("stage", stage)?;
        validate_text("target", target)?;
        let transaction = self.connection.unchecked_transaction()?;
        let inserted = transaction.execute(
            "INSERT INTO operation_intents (deployment_id,component,stage,target,status,created_at_ms)
             SELECT id,?2,?3,?4,'pending',?5 FROM deployments WHERE id=?1 AND state='running' AND created_at_ms<=?5",
            params![deployment.to_string(), component.as_str(), stage, target, timestamp(created_at_ms)?],
        )?;
        if inserted == 1 {
            let intent = IntentId(transaction.last_insert_rowid());
            details::begin_step(
                &transaction,
                deployment,
                component,
                stage,
                intent,
                created_at_ms,
            )?;
            transaction.commit()?;
            Ok(intent)
        } else {
            Err(HistoryError::NotRunning(deployment.clone()))
        }
    }

    /// Completes a pending intent exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid outcome, missing/completed intent, or `SQLite` failure.
    pub fn complete_intent(
        &self,
        intent: IntentId,
        outcome: IntentStatus,
        error: Option<&str>,
        completed_at_ms: u64,
        redactor: &Redactor,
    ) -> Result<(), HistoryError> {
        let redacted_error = error.map(|error| sanitize_diagnostic(&redactor.redact(error)));
        let (status, error) = match (outcome, redacted_error.as_deref()) {
            (IntentStatus::Succeeded, None) => ("succeeded", None),
            (IntentStatus::Failed, Some(error)) => ("failed", Some(error)),
            _ => return Err(HistoryError::InvalidOutcome),
        };
        let transaction = self.connection.unchecked_transaction()?;
        details::validate_step_links(&transaction, None, Some(intent))?;
        let updated = transaction.execute(
            "UPDATE operation_intents SET status=?1,completed_at_ms=?2,error=?3 WHERE id=?4 AND status='pending'",
            params![status, timestamp(completed_at_ms)?, error, intent.0],
        )?;
        if updated == 1 {
            transaction.execute(
                "UPDATE deployment_steps SET status=?1,completed_at_ms=?2,error=?3 WHERE intent_id=?4 AND status='running'",
                params![status, timestamp(completed_at_ms)?, error, intent.0],
            )?;
            transaction.commit()?;
            Ok(())
        } else {
            Err(HistoryError::IntentConflict(intent))
        }
    }

    /// Loads pending intents in their durable sequence order.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt identifiers or `SQLite` failure.
    pub fn pending_intents(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Vec<IntentRecord>, HistoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id,deployment_id,component,stage,target,created_at_ms FROM operation_intents
             WHERE deployment_id=?1 AND status='pending' ORDER BY id",
        )?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, deployment, component, stage, target, created_at_ms) = row?;
            Ok(IntentRecord {
                id: IntentId(id),
                deployment: DeploymentId::from_str(&deployment).map_err(|error| corrupt(&error))?,
                component: ComponentName::parse(component).map_err(|error| corrupt(&error))?,
                stage,
                target,
                created_at_ms: u64::try_from(created_at_ms).map_err(|error| corrupt(&error))?,
            })
        })
        .collect()
    }

    /// Persists the terminal result for one selected Component exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error unless the Deployment is running, the diagnostic is
    /// safe to persist, and no result already exists for this Component.
    pub fn record_component_result(
        &self,
        deployment: &DeploymentId,
        component: &ComponentName,
        result: &ComponentDeploymentResult,
        error: Option<&str>,
        redactor: &Redactor,
    ) -> Result<(), HistoryError> {
        let error = error.map(|value| sanitize_diagnostic(&redactor.redact(value)));
        details::validate_component_result(&self.connection, deployment, component, result)?;
        let inserted = self.connection.execute(
            "INSERT INTO component_results
             (deployment_id,component,outcome,attempted_release,observed_release,error)
             SELECT id,?2,?3,?4,?5,?6 FROM deployments WHERE id=?1 AND state='running'",
            params![
                deployment.to_string(),
                component.as_str(),
                outcome(&result.outcome),
                result.attempted_release.as_ref().map(ToString::to_string),
                result.observed_release.as_ref().map(ToString::to_string),
                error,
            ],
        )?;
        if inserted == 1 {
            Ok(())
        } else {
            Err(HistoryError::NotRunning(deployment.clone()))
        }
    }

    /// Loads persisted per-Component terminal results in stable name order.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt persisted values or a `SQLite` failure.
    pub fn component_results(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Vec<PersistedComponentResult>, HistoryError> {
        let mut statement = self.connection.prepare(
            "SELECT component,outcome,attempted_release,observed_release,error
             FROM component_results WHERE deployment_id=?1 ORDER BY component",
        )?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (component, outcome, attempted, observed, error) = row?;
            let record = PersistedComponentResult {
                component: ComponentName::parse(component).map_err(|error| corrupt(&error))?,
                result: ComponentDeploymentResult {
                    outcome: parse_outcome(&outcome)?,
                    attempted_release: attempted
                        .map(ReleaseVersion::parse)
                        .transpose()
                        .map_err(|error| corrupt(&error))?,
                    observed_release: observed
                        .map(ReleaseVersion::parse)
                        .transpose()
                        .map_err(|error| corrupt(&error))?,
                },
                error,
            };
            details::validate_component_result(
                &self.connection,
                deployment,
                &record.component,
                &record.result,
            )
            .map_err(|_| corrupt(&"Component result differs from frozen target"))?;
            Ok(record)
        })
        .collect()
    }

    #[cfg(test)]
    fn deployment_state(&self, deployment: &DeploymentId) -> Result<Option<String>, HistoryError> {
        self.connection
            .query_row(
                "SELECT state FROM deployments WHERE id=?1",
                [deployment.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(HistoryError::Sqlite)
    }

    #[cfg(test)]
    fn deployment_kind(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<(String, Option<String>)>, HistoryError> {
        self.connection
            .query_row(
                "SELECT kind,related_deployment_id FROM deployments WHERE id=?1",
                [deployment.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(HistoryError::Sqlite)
    }

    #[cfg(test)]
    fn intent_error(&self, intent: IntentId) -> Result<Option<String>, HistoryError> {
        self.connection
            .query_row(
                "SELECT error FROM operation_intents WHERE id=?1",
                [intent.0],
                |row| row.get(0),
            )
            .map_err(HistoryError::Sqlite)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntentId(i64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentLogRecord {
    pub deployment: DeploymentId,
    /// Relative to the directory containing the history database.
    pub relative_path: PathBuf,
    pub max_bytes: u64,
    /// Number of rotated files kept in addition to the active log.
    pub retained_files: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentStatus {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentRecord {
    pub id: IntentId,
    pub deployment: DeploymentId,
    pub component: ComponentName,
    pub stage: String,
    pub target: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedComponentResult {
    pub component: ComponentName,
    pub result: ComponentDeploymentResult,
    pub error: Option<String>,
}

const fn outcome(value: &ComponentOutcome) -> &'static str {
    match value {
        ComponentOutcome::Succeeded => "succeeded",
        ComponentOutcome::Failed => "failed",
        ComponentOutcome::Cancelled => "cancelled",
        ComponentOutcome::Compensated => "compensated",
        ComponentOutcome::CompensationFailed => "compensation_failed",
    }
}

fn parse_outcome(value: &str) -> Result<ComponentOutcome, HistoryError> {
    match value {
        "succeeded" => Ok(ComponentOutcome::Succeeded),
        "failed" => Ok(ComponentOutcome::Failed),
        "cancelled" => Ok(ComponentOutcome::Cancelled),
        "compensated" => Ok(ComponentOutcome::Compensated),
        "compensation_failed" => Ok(ComponentOutcome::CompensationFailed),
        _ => Err(HistoryError::Corrupt(format!(
            "unknown Component outcome `{value}`"
        ))),
    }
}

const fn state(value: DeploymentState) -> &'static str {
    match value {
        DeploymentState::Created => "created",
        DeploymentState::Running => "running",
        DeploymentState::Succeeded => "succeeded",
        DeploymentState::Failed => "failed",
        DeploymentState::Cancelled => "cancelled",
    }
}

const fn valid_transition(expected: DeploymentState, next: DeploymentState) -> bool {
    matches!(
        (expected, next),
        (
            DeploymentState::Created,
            DeploymentState::Running | DeploymentState::Cancelled
        ) | (
            DeploymentState::Running,
            DeploymentState::Succeeded | DeploymentState::Failed | DeploymentState::Cancelled
        )
    )
}

fn timestamp(value: u64) -> Result<i64, HistoryError> {
    i64::try_from(value).map_err(|_| HistoryError::InvalidTimestamp(value))
}

fn validate_text(field: &'static str, value: &str) -> Result<(), HistoryError> {
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        Err(HistoryError::InvalidText { field })
    } else {
        Ok(())
    }
}

fn sanitize_diagnostic(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(1024));
    for character in value.chars() {
        if sanitized.len() + character.len_utf8() > 1024 {
            break;
        }
        sanitized.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    if sanitized.is_empty() {
        "unknown error".into()
    } else {
        sanitized
    }
}

fn corrupt(error: &impl ToString) -> HistoryError {
    HistoryError::Corrupt(error.to_string())
}

#[cfg(unix)]
fn secure_file_permissions(path: &Path) -> Result<(), HistoryError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .map_err(|source| HistoryError::Io {
            path: path.to_owned(),
            source,
        })?
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions).map_err(|source| HistoryError::Io {
        path: path.to_owned(),
        source,
    })
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("history configuration directory is unavailable: {0}")]
    ConfigurationDirectory(String),
    #[error("history I/O failed at `{path}`: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite history operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("history schema version {0} is newer than supported")]
    UnsupportedSchema(u32),
    #[error("Deployment {0} is not running")]
    NotRunning(DeploymentId),
    #[error("Deployment {0} is absent or no longer in the expected state")]
    StateConflict(DeploymentId),
    #[error("related Deployment {0} is absent or belongs to another Project or Environment")]
    MissingRelatedDeployment(DeploymentId),
    #[error("invalid persisted Deployment transition from {expected:?} to {next:?}")]
    InvalidTransition {
        expected: DeploymentState,
        next: DeploymentState,
    },
    #[error("intent {0:?} is absent or already complete")]
    IntentConflict(IntentId),
    #[error("intent outcome/error combination is invalid")]
    InvalidOutcome,
    #[error("Deployment log limits must be non-zero and fit the SQLite integer range")]
    InvalidLogLimits,
    #[error("{field} must contain 1-1024 non-control characters")]
    InvalidText { field: &'static str },
    #[error("timestamp {0} exceeds SQLite range")]
    InvalidTimestamp(u64),
    #[error("history database contains invalid data: {0}")]
    Corrupt(String),
    #[error("invalid history metadata: {0}")]
    InvalidMetadata(&'static str),
    #[error("history pagination requires a limit of 1-100 and offset at most 1000000")]
    InvalidPage,
}

#[cfg(test)]
mod log_index_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (DeploymentId, ProjectId, EnvironmentId, ComponentName) {
        (
            DeploymentId::new(),
            ProjectId::new(),
            EnvironmentId::new(),
            ComponentName::parse("api").unwrap(),
        )
    }

    #[test]
    fn migration_is_numbered_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/history.sqlite3");
        assert_eq!(
            HistoryStore::open(&path).unwrap().schema_version().unwrap(),
            5
        );
        assert_eq!(
            HistoryStore::open(&path).unwrap().schema_version().unwrap(),
            5
        );
    }

    #[test]
    fn version_one_database_migrates_and_round_trips_component_results() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(MIGRATION_1).unwrap();
        connection
            .pragma_update(None, "user_version", 1_u32)
            .unwrap();
        drop(connection);

        let store = HistoryStore::open(&path).unwrap();
        let (deployment, project, environment, component) = ids();
        store
            .create_deployment(&deployment, &project, &environment, 1)
            .unwrap();
        store
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        let result = ComponentDeploymentResult {
            outcome: ComponentOutcome::Compensated,
            attempted_release: Some(ReleaseVersion::parse("v2").unwrap()),
            observed_release: Some(ReleaseVersion::parse("v1").unwrap()),
        };
        store
            .record_component_result(
                &deployment,
                &component,
                &result,
                Some("TOKEN failed"),
                &Redactor::new(["TOKEN".into()]),
            )
            .unwrap();
        let persisted = store.component_results(&deployment).unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
        assert_eq!(persisted[0].result, result);
        assert_eq!(persisted[0].error.as_deref(), Some("[REDACTED] failed"));
    }

    #[test]
    fn rollback_deployment_requires_and_persists_its_related_deployment() {
        let store = HistoryStore::in_memory().unwrap();
        let (source, project, environment, _) = ids();
        let rollback = DeploymentId::new();
        assert!(matches!(
            store.create_rollback_deployment(&rollback, &source, &project, &environment, 1),
            Err(HistoryError::MissingRelatedDeployment(_))
        ));
        store
            .create_deployment(&source, &project, &environment, 2)
            .unwrap();
        assert!(matches!(
            store.create_rollback_deployment(&rollback, &source, &project, &environment, 3),
            Err(HistoryError::MissingRelatedDeployment(_))
        ));
        store
            .transition_deployment(
                &source,
                DeploymentState::Created,
                DeploymentState::Running,
                3,
            )
            .unwrap();
        store
            .transition_deployment(
                &source,
                DeploymentState::Running,
                DeploymentState::Succeeded,
                4,
            )
            .unwrap();
        store
            .create_rollback_deployment(&rollback, &source, &project, &environment, 5)
            .unwrap();
        assert_eq!(
            store.deployment_kind(&rollback).unwrap(),
            Some(("rollback".into(), Some(source.to_string())))
        );
    }

    #[test]
    fn newer_schema_is_rejected_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("future.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", 99_u32)
            .unwrap();
        drop(connection);
        assert!(matches!(
            HistoryStore::open(&path),
            Err(HistoryError::UnsupportedSchema(99))
        ));
        let connection = Connection::open(path).unwrap();
        let version: u32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 99);
    }

    #[test]
    fn intent_requires_running_deployment_and_completes_once() {
        let store = HistoryStore::in_memory().unwrap();
        let (deployment, project, environment, component) = ids();
        store
            .create_deployment(&deployment, &project, &environment, 10)
            .unwrap();
        assert!(matches!(
            store.record_intent(&deployment, &component, "upload", "v1", 11),
            Err(HistoryError::NotRunning(_))
        ));
        store
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                12,
            )
            .unwrap();
        let intent = store
            .record_intent(&deployment, &component, "upload", "v1", 13)
            .unwrap();
        assert_eq!(store.pending_intents(&deployment).unwrap().len(), 1);
        store
            .complete_intent(
                intent,
                IntentStatus::Succeeded,
                None,
                14,
                &Redactor::default(),
            )
            .unwrap();
        assert!(matches!(
            store.complete_intent(
                intent,
                IntentStatus::Succeeded,
                None,
                15,
                &Redactor::default()
            ),
            Err(HistoryError::IntentConflict(_))
        ));
    }

    #[test]
    fn pending_intent_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let (deployment, project, environment, component) = ids();
        let store = HistoryStore::open(&path).unwrap();
        store
            .create_deployment(&deployment, &project, &environment, 1)
            .unwrap();
        store
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        store
            .record_intent(&deployment, &component, "activate", "current", 3)
            .unwrap();
        drop(store);
        let pending = HistoryStore::open(&path)
            .unwrap()
            .pending_intents(&deployment)
            .unwrap();
        assert_eq!(pending[0].stage, "activate");
    }

    #[test]
    fn deployment_transition_is_compare_and_set() {
        let store = HistoryStore::in_memory().unwrap();
        let (deployment, project, environment, _) = ids();
        store
            .create_deployment(&deployment, &project, &environment, 1)
            .unwrap();
        assert!(matches!(
            store.transition_deployment(
                &deployment,
                DeploymentState::Running,
                DeploymentState::Succeeded,
                2
            ),
            Err(HistoryError::StateConflict(_))
        ));
        assert_eq!(
            store.deployment_state(&deployment).unwrap().as_deref(),
            Some("created")
        );
        assert!(matches!(
            store.transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Succeeded,
                2
            ),
            Err(HistoryError::InvalidTransition { .. })
        ));
        store
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                10,
            )
            .unwrap();
        assert!(matches!(
            store.transition_deployment(
                &deployment,
                DeploymentState::Running,
                DeploymentState::Succeeded,
                9
            ),
            Err(HistoryError::StateConflict(_))
        ));
    }

    #[test]
    fn failed_intent_is_redacted_before_persistence() {
        let store = HistoryStore::in_memory().unwrap();
        let (deployment, project, environment, component) = ids();
        store
            .create_deployment(&deployment, &project, &environment, 1)
            .unwrap();
        store
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        let intent = store
            .record_intent(&deployment, &component, "upload", "v1", 3)
            .unwrap();
        store
            .complete_intent(
                intent,
                IntentStatus::Failed,
                Some("server rejected TOKEN"),
                4,
                &Redactor::new(["TOKEN".into()]),
            )
            .unwrap();
        assert_eq!(
            store.intent_error(intent).unwrap().as_deref(),
            Some("server rejected [REDACTED]")
        );
    }

    #[test]
    fn diagnostics_are_redacted_single_line_and_bounded() {
        let store = HistoryStore::in_memory().unwrap();
        let (deployment, project, environment, component) = ids();
        store
            .create_deployment(&deployment, &project, &environment, 1)
            .unwrap();
        store
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        let intent = store
            .record_intent(&deployment, &component, "activate", "v1", 3)
            .unwrap();
        let diagnostic = format!("TOKEN\n{}", "x".repeat(2048));
        store
            .complete_intent(
                intent,
                IntentStatus::Failed,
                Some(&diagnostic),
                4,
                &Redactor::new(["TOKEN".into()]),
            )
            .unwrap();
        let persisted = store.intent_error(intent).unwrap().unwrap();
        assert!(persisted.starts_with("[REDACTED] "));
        assert!(!persisted.contains('\n'));
        assert!(persisted.len() <= 1024);
    }
}
