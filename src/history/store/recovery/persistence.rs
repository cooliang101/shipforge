use std::{path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use super::{
    CurrentAlignment, DeploymentId, EnvironmentId, HistoryError, HistoryStore, InspectionScope,
    LocalAttentionSummary, MAX_COMPONENTS, MAX_PAGE_BYTES, MAX_REPORT_BYTES, PackageAlignment,
    ProjectId, RecoveryBasis, RecoveryComponentReport, RecoveryQuery, RecoveryReport, Redactor,
    encode, invalid, timestamp, validate_unplanned,
};
use crate::{domain::DeploymentState, history::DeploymentKind};

const MIGRATION_6: &str = r"
CREATE TABLE deployment_revisions (
 deployment_id TEXT PRIMARY KEY NOT NULL, revision INTEGER NOT NULL CHECK(revision>=0)
) STRICT;
INSERT INTO deployment_revisions SELECT id,0 FROM deployments;
CREATE TABLE recovery_reports (
 id TEXT PRIMARY KEY NOT NULL CHECK(length(id)=36),
 project_id TEXT NOT NULL CHECK(length(CAST(project_id AS BLOB))<=80),
 environment_id TEXT NOT NULL CHECK(length(CAST(environment_id AS BLOB))<=80),
 related_deployment_id TEXT CHECK(length(CAST(related_deployment_id AS BLOB))<=80),
 source_revision INTEGER CHECK(source_revision>=0),
 started_at_ms INTEGER NOT NULL CHECK(started_at_ms>=0),
 completed_at_ms INTEGER NOT NULL CHECK(completed_at_ms>=started_at_ms),
 report TEXT NOT NULL CHECK(length(CAST(report AS BLOB))<=4194304),
 CHECK((related_deployment_id IS NULL)=(source_revision IS NULL))
) STRICT;
CREATE INDEX recovery_report_page ON recovery_reports(project_id,environment_id,completed_at_ms DESC,id DESC);
CREATE TABLE recovery_report_components (
 report_id TEXT NOT NULL REFERENCES recovery_reports(id),
 component TEXT NOT NULL CHECK(length(CAST(component AS BLOB)) BETWEEN 1 AND 128),
 scope TEXT NOT NULL CHECK(length(CAST(scope AS BLOB)) BETWEEN 1 AND 4096),
 PRIMARY KEY(report_id,component)
) STRICT;
CREATE INDEX recovery_component_scope ON recovery_report_components(scope,report_id);
CREATE TRIGGER recovery_reports_immutable_update BEFORE UPDATE ON recovery_reports
 BEGIN SELECT RAISE(ABORT,'recovery reports are immutable'); END;
CREATE TRIGGER recovery_reports_immutable_delete BEFORE DELETE ON recovery_reports
 BEGIN SELECT RAISE(ABORT,'recovery reports are immutable'); END;
CREATE TRIGGER recovery_components_immutable_update BEFORE UPDATE ON recovery_report_components
 BEGIN SELECT RAISE(ABORT,'recovery scope is immutable'); END;
CREATE TRIGGER recovery_components_immutable_delete BEFORE DELETE ON recovery_report_components
 BEGIN SELECT RAISE(ABORT,'recovery scope is immutable'); END;
";

pub(in super::super) fn migrate(connection: &Connection) -> Result<(), HistoryError> {
    connection.execute_batch(MIGRATION_6)?;
    for table in [
        "deployments",
        "operation_intents",
        "component_results",
        "deployment_logs",
        "deployment_metadata",
        "deployment_snapshots",
        "component_snapshots",
        "release_packages",
        "deployment_observations",
        "release_receipts",
        "deployment_steps",
    ] {
        let column = if table == "deployments" {
            "id"
        } else {
            "deployment_id"
        };
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let reference = if operation == "DELETE" { "OLD" } else { "NEW" };
            let previous = if operation == "UPDATE" {
                format!(
                    "UPDATE deployment_revisions SET revision=revision+1 WHERE deployment_id=OLD.{column} AND OLD.{column}!=NEW.{column};"
                )
            } else {
                String::new()
            };
            connection.execute_batch(&format!(
                "CREATE TRIGGER recovery_revision_{table}_{operation} AFTER {operation} ON {table}
                 BEGIN INSERT INTO deployment_revisions(deployment_id,revision) VALUES ({reference}.{column},1)
                 ON CONFLICT(deployment_id) DO UPDATE SET revision=revision+1; {previous} END;"
            ))?;
        }
    }
    Ok(())
}

impl HistoryStore {
    /// Reads a bounded, consistent source snapshot, without holding it across SSH work.
    ///
    /// # Errors
    /// Returns an error for an absent/corrupt Deployment or excessive historical data.
    pub fn recovery_basis(&self, deployment: &DeploymentId) -> Result<RecoveryBasis, HistoryError> {
        let transaction = self.connection.unchecked_transaction()?;
        validate_basis_size(&transaction, deployment)?;
        let record = self
            .deployment(deployment)?
            .ok_or(HistoryError::InvalidMetadata(
                "missing recovery source Deployment",
            ))?;
        let basis = RecoveryBasis {
            record,
            snapshots: self.component_snapshots(deployment)?,
            packages: self.release_packages(deployment)?,
            intents: self.pending_intents(deployment)?,
            observations: self.observations(deployment)?,
            revision: source_revision(&transaction, deployment)?,
        };
        transaction.commit()?;
        Ok(basis)
    }

    /// Appends an immutable sanitized report, checking its source revision atomically.
    /// Cache-only reports never manufacture Deployment rows, intents, or log entries.
    ///
    /// # Errors
    /// Returns an error for stale basis, invalid evidence, duplicate ID, or database failure.
    pub fn append_recovery_report(
        &self,
        report: &RecoveryReport,
        redactor: &Redactor,
    ) -> Result<(), HistoryError> {
        let mut report = report.clone();
        report.sanitize(redactor);
        report.validate()?;
        let transaction =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        self.validate_report_source(&report)?;
        let first = &report.components[0].scope;
        transaction.execute(
            "INSERT INTO recovery_reports VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                report.id.to_string(),
                first.project.to_string(),
                first.environment.to_string(),
                report.related_deployment.as_ref().map(ToString::to_string),
                report.source_revision.map(timestamp).transpose()?,
                timestamp(report.started_at_ms)?,
                timestamp(report.completed_at_ms)?,
                encode(&report)?
            ],
        )?;
        for component in &report.components {
            transaction.execute(
                "INSERT INTO recovery_report_components VALUES (?1,?2,?3)",
                params![
                    report.id.to_string(),
                    component.scope.component.as_str(),
                    encode(&component.scope)?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Loads one original report; source changes do not rewrite old conclusions.
    ///
    /// # Errors
    /// Returns an error for corrupt payloads/indexes or database failure.
    pub fn recovery_report(&self, id: &uuid::Uuid) -> Result<Option<RecoveryReport>, HistoryError> {
        let row = self.connection.query_row(
            "SELECT CASE WHEN length(CAST(project_id AS BLOB))<=80 THEN project_id END,
             CASE WHEN length(CAST(environment_id AS BLOB))<=80 THEN environment_id END,
             CASE WHEN length(CAST(related_deployment_id AS BLOB))>80 THEN '' ELSE related_deployment_id END,
             source_revision,started_at_ms,completed_at_ms,
             CASE WHEN length(CAST(report AS BLOB))<=4194304 THEN report END FROM recovery_reports WHERE id=?1",
            [id.to_string()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?,
                row.get::<_,Option<i64>>(3)?,row.get::<_,i64>(4)?,row.get::<_,i64>(5)?,row.get::<_,Option<String>>(6)?)),
        ).optional()?;
        let Some((project, environment, related, revision, started, completed, json)) = row else {
            return Ok(None);
        };
        let json = json.ok_or_else(|| corrupt("oversized recovery report"))?;
        let report: RecoveryReport =
            serde_json::from_str(&json).map_err(|_| corrupt("invalid recovery report JSON"))?;
        report
            .validate()
            .map_err(|_| corrupt("invalid recovery report evidence"))?;
        let first = &report.components[0].scope;
        if report.id != *id
            || first.project.to_string() != project
            || first.environment.to_string() != environment
            || report.related_deployment.as_ref().map(ToString::to_string) != related
            || report.source_revision != revision.map(nonnegative).transpose()?
            || report.started_at_ms != nonnegative(started)?
            || report.completed_at_ms != nonnegative(completed)?
        {
            return Err(corrupt("recovery report index differs from payload"));
        }
        self.validate_component_indexes(&report)?;
        Ok(Some(report))
    }

    /// Lists scoped reports newest-first with bounded deterministic pagination.
    ///
    /// # Errors
    /// Returns an error for invalid limits, excessive aggregate bytes, or corrupt data.
    pub fn recovery_reports(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: RecoveryQuery,
    ) -> Result<Vec<RecoveryReport>, HistoryError> {
        validate_page(query)?;
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare("SELECT CASE WHEN length(CAST(id AS BLOB))=36 THEN id ELSE '' END,length(CAST(report AS BLOB)) FROM recovery_reports WHERE project_id=?1 AND environment_id=?2 ORDER BY rowid DESC LIMIT ?3 OFFSET ?4")?;
        let rows = statement.query_map(
            params![
                project.to_string(),
                environment.to_string(),
                query.limit,
                query.offset
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let mut reports = Vec::new();
        let mut bytes = 0_usize;
        for row in rows {
            let (id, size) = row?;
            bytes = bytes.saturating_add(
                usize::try_from(size).map_err(|_| corrupt("invalid report byte size"))?,
            );
            if bytes > MAX_PAGE_BYTES {
                return invalid("recovery page exceeds byte limit; request fewer reports");
            }
            let id =
                uuid::Uuid::parse_str(&id).map_err(|_| corrupt("invalid recovery report ID"))?;
            reports.push(
                self.recovery_report(&id)?
                    .ok_or_else(|| corrupt("missing recovery report"))?,
            );
        }
        drop(statement);
        transaction.commit()?;
        Ok(reports)
    }

    /// Returns the newest exactly compatible inspection, including unknown results.
    /// A newer failure must never silently fall back to an older successful cache.
    ///
    /// # Errors
    /// Returns an error for invalid stored report/index data or database failure.
    pub fn latest_recovery_report(
        &self,
        scope: &InspectionScope,
    ) -> Result<Option<RecoveryReport>, HistoryError> {
        let transaction = self.connection.unchecked_transaction()?;
        let id: Option<String> = transaction.query_row(
            "SELECT CASE WHEN length(CAST(r.id AS BLOB))=36 THEN r.id ELSE '' END FROM recovery_reports r JOIN recovery_report_components c ON c.report_id=r.id
             WHERE c.scope=?1 ORDER BY r.rowid DESC LIMIT 1", [encode(scope)?], |row| row.get(0)).optional()?;
        let result = id
            .map(|id| {
                self.recovery_report(
                    &uuid::Uuid::parse_str(&id)
                        .map_err(|_| corrupt("invalid recovery report ID"))?,
                )?
                .ok_or_else(|| corrupt("missing recovery report"))
            })
            .transpose()?;
        transaction.commit()?;
        Ok(result)
    }

    /// Lists both unfinished Deployments and terminal Deployments with pending intents.
    ///
    /// # Errors
    /// Returns an error for invalid pagination or corrupt source metadata.
    pub fn recovery_candidates(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: RecoveryQuery,
    ) -> Result<Vec<super::super::DeploymentRecord>, HistoryError> {
        validate_page(query)?;
        let transaction = self.connection.unchecked_transaction()?;
        let records = candidates(&transaction, Some(project), Some(environment), query, 6)?;
        transaction.commit()?;
        Ok(records)
    }

    /// Reads local attention without creating a directory/database or applying migrations.
    /// Missing history is distinct from an existing database with no candidates.
    ///
    /// # Errors
    /// Returns an error for inaccessible/non-file paths, unsupported schemas, or corrupt data.
    pub fn local_attention(
        path: &Path,
        project: Option<&ProjectId>,
        environment: Option<&EnvironmentId>,
    ) -> Result<LocalAttentionSummary, HistoryError> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LocalAttentionSummary {
                    database_missing: true,
                    candidates: Vec::new(),
                    more: false,
                });
            }
            Err(source) => {
                return Err(HistoryError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        if !metadata.is_file() {
            return invalid("history path is not a regular file");
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(1))?;
        connection.pragma_update(None, "query_only", true)?;
        let transaction = connection.unchecked_transaction()?;
        let schema: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if schema > 6 {
            return Err(HistoryError::UnsupportedSchema(schema));
        }
        if schema == 0 {
            return Err(corrupt("history schema is uninitialized"));
        }
        let mut candidates = candidates(
            &transaction,
            project,
            environment,
            RecoveryQuery {
                limit: 101,
                offset: 0,
            },
            schema,
        )?;
        let more = candidates.len() > 100;
        candidates.truncate(100);
        transaction.commit()?;
        Ok(LocalAttentionSummary {
            database_missing: false,
            candidates,
            more,
        })
    }

    fn validate_report_source(&self, report: &RecoveryReport) -> Result<(), HistoryError> {
        let Some(deployment) = &report.related_deployment else {
            return Ok(());
        };
        if report.source_revision != Some(source_revision(&self.connection, deployment)?) {
            return Err(HistoryError::StaleRecoveryBasis);
        }
        let record = self
            .deployment(deployment)?
            .ok_or(HistoryError::StaleRecoveryBasis)?;
        validate_basis_size(&self.connection, deployment)?;
        let snapshots = self.component_snapshots(deployment)?;
        let packages = self.release_packages(deployment)?;
        if report.started_at_ms < record.created_at_ms {
            return invalid("inspection predates source Deployment");
        }
        for component in &report.components {
            if component.scope.project != record.project
                || component.scope.environment != record.environment
            {
                return invalid("recovery source Project/Environment mismatch");
            }
            if snapshots.is_empty() {
                validate_unplanned(component)?;
                continue;
            }
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.release.component == component.scope.component)
                .ok_or(HistoryError::InvalidMetadata(
                    "recovery Component is not selected by source",
                ))?;
            if InspectionScope::from(&snapshot.release) != component.scope {
                return invalid("recovery scope differs from frozen source");
            }
            validate_alignment(component, snapshot, &packages)?;
        }
        Ok(())
    }

    fn validate_component_indexes(&self, report: &RecoveryReport) -> Result<(), HistoryError> {
        let mut statement = self.connection.prepare(
            "SELECT CASE WHEN length(CAST(component AS BLOB))<=128 THEN component END,CASE WHEN length(CAST(scope AS BLOB))<=4096 THEN scope END FROM recovery_report_components WHERE report_id=?1 LIMIT 257",
        )?;
        let indexes = statement
            .query_map([report.id.to_string()], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if indexes.len() != report.components.len() {
            return Err(corrupt("recovery Component index count mismatch"));
        }
        for (component, scope) in indexes {
            let component =
                component.ok_or_else(|| corrupt("oversized recovery Component index"))?;
            let scope = scope.ok_or_else(|| corrupt("oversized recovery scope index"))?;
            if !report.components.iter().any(|entry| {
                entry.scope.component.as_str() == component
                    && encode(&entry.scope).is_ok_and(|json| json == scope)
            }) {
                return Err(corrupt("recovery Component scope index mismatch"));
            }
        }
        Ok(())
    }
}

fn source_revision(
    connection: &Connection,
    deployment: &DeploymentId,
) -> Result<u64, HistoryError> {
    let revision: i64 = connection
        .query_row(
            "SELECT revision FROM deployment_revisions WHERE deployment_id=?1",
            [deployment.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(HistoryError::StaleRecoveryBasis)?;
    nonnegative(revision)
}

fn validate_basis_size(
    connection: &Connection,
    deployment: &DeploymentId,
) -> Result<(), HistoryError> {
    let invalid_identity: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND
         (length(CAST(project_id AS BLOB))>80 OR length(CAST(environment_id AS BLOB))>80
         OR length(CAST(related_deployment_id AS BLOB))>80 OR length(kind)>8 OR length(state)>9))",
        [deployment.to_string()],
        |row| row.get(0),
    )?;
    if invalid_identity {
        return invalid("recovery source identity exceeds bounds");
    }
    let mut total = 0_usize;
    for (table, columns, limit) in [
        (
            "component_snapshots",
            "length(CAST(snapshot AS BLOB))+length(CAST(component AS BLOB))",
            MAX_COMPONENTS,
        ),
        (
            "release_packages",
            "length(CAST(release_ref AS BLOB))+length(CAST(manifest AS BLOB))+length(CAST(sha256 AS BLOB))+length(CAST(component AS BLOB))",
            MAX_COMPONENTS,
        ),
        (
            "operation_intents",
            "length(CAST(stage AS BLOB))+length(CAST(target AS BLOB))+COALESCE(length(CAST(error AS BLOB)),0)+length(CAST(component AS BLOB))",
            4096,
        ),
        (
            "deployment_observations",
            "length(CAST(stage AS BLOB))+COALESCE(length(CAST(observed_ref AS BLOB)),0)+COALESCE(length(CAST(error AS BLOB)),0)+length(CAST(component AS BLOB))",
            4096,
        ),
    ] {
        let mut statement = connection.prepare(&format!(
            "SELECT {columns} FROM {table} WHERE deployment_id=?1 LIMIT ?2"
        ))?;
        let sizes = statement.query_map(
            params![
                deployment.to_string(),
                u32::try_from(limit + 1).unwrap_or(u32::MAX)
            ],
            |row| row.get::<_, i64>(0),
        )?;
        for (index, size) in sizes.enumerate() {
            total = total.saturating_add(
                usize::try_from(size?).map_err(|_| corrupt("invalid source byte size"))?,
            );
            if index == limit || total > MAX_REPORT_BYTES {
                return invalid("recovery source exceeds bounded snapshot limits");
            }
        }
    }
    Ok(())
}

fn validate_alignment(
    component: &RecoveryComponentReport,
    snapshot: &super::DeploymentComponentSnapshot,
    packages: &[super::ReleasePackageRecord],
) -> Result<(), HistoryError> {
    let Ok(inventory) = &component.inventory else {
        return Ok(());
    };
    let target = snapshot.target.as_ref().map(|release| &release.version);
    let previous = snapshot
        .expected_current
        .as_ref()
        .map(|release| &release.version);
    let valid = match (component.alignment, &inventory.releases.current) {
        (CurrentAlignment::Unknown | CurrentAlignment::Unplanned, _) => true,
        (CurrentAlignment::Target, Ok(current)) => current.as_ref() == target,
        (CurrentAlignment::Previous, Ok(current)) => current.as_ref() == previous,
        (CurrentAlignment::Other, Ok(current)) => {
            current.as_ref() != target && current.as_ref() != previous
        }
        _ => false,
    };
    if !valid {
        return invalid("current alignment contradicts frozen target/previous");
    }
    let archive = inventory
        .releases
        .releases
        .iter()
        .find(|release| Some(&release.manifest.version) == target);
    let local = packages
        .iter()
        .find(|package| package.release.component == component.scope.component);
    let matches = archive.zip(local).is_some_and(|(archive, local)| {
        archive.manifest == local.manifest
            && archive.sha256.eq_ignore_ascii_case(&local.sha256)
            && archive.size == local.size
    });
    let conflicting_audit = archive.is_some_and(|archive| {
        inventory
            .audit
            .records
            .iter()
            .filter(|record| {
                record.phase == crate::drivers::audit::RemoteAuditPhase::Prepare
                    && snapshot.target.as_ref() == Some(&record.release)
            })
            .filter_map(|record| record.package.as_ref())
            .any(|package| {
                package.manifest != archive.manifest
                    || !package.sha256.eq_ignore_ascii_case(&archive.sha256)
                    || package.size != archive.size
            })
    });
    let valid = match component.package_alignment {
        PackageAlignment::Unknown | PackageAlignment::Unplanned => true,
        PackageAlignment::Missing => {
            target.is_some()
                && archive.is_none()
                && !inventory
                    .releases
                    .issues
                    .iter()
                    .any(|issue| issue.version.as_ref() == target)
        }
        PackageAlignment::Matches => {
            matches && !conflicting_audit && archive.is_some_and(|archive| archive.extracted)
        }
        PackageAlignment::ArchiveOnly => {
            matches && !conflicting_audit && archive.is_some_and(|archive| !archive.extracted)
        }
        PackageAlignment::Mismatch => {
            conflicting_audit || (archive.is_some() && local.is_some() && !matches)
        }
    };
    if valid {
        Ok(())
    } else {
        invalid("package alignment contradicts frozen package evidence")
    }
}

fn validate_page(query: RecoveryQuery) -> Result<(), HistoryError> {
    if !(1..=100).contains(&query.limit) || query.offset > 1_000_000 {
        Err(HistoryError::InvalidPage)
    } else {
        Ok(())
    }
}

fn candidates(
    connection: &Connection,
    project: Option<&ProjectId>,
    environment: Option<&EnvironmentId>,
    query: RecoveryQuery,
    schema: u32,
) -> Result<Vec<super::DeploymentRecord>, HistoryError> {
    let kind = if schema >= 3 {
        "CASE WHEN length(kind)<=8 THEN kind END,CASE WHEN length(CAST(related_deployment_id AS BLOB))>80 THEN '' ELSE related_deployment_id END"
    } else {
        "'deploy',NULL"
    };
    let mut statement = connection.prepare(&format!(
        "SELECT CASE WHEN length(CAST(id AS BLOB))<=80 THEN id END,
         CASE WHEN length(CAST(project_id AS BLOB))<=80 THEN project_id END,
         CASE WHEN length(CAST(environment_id AS BLOB))<=80 THEN environment_id END,
         CASE WHEN length(state)<=9 THEN state END,created_at_ms,updated_at_ms,{kind},
         (SELECT COUNT(*) FROM operation_intents WHERE deployment_id=d.id AND status='pending')
         FROM deployments d WHERE (?1 IS NULL OR project_id=?1) AND (?2 IS NULL OR environment_id=?2)
         AND (state IN ('created','running') OR EXISTS(SELECT 1 FROM operation_intents WHERE deployment_id=d.id AND status='pending'))
         ORDER BY created_at_ms DESC,id DESC LIMIT ?3 OFFSET ?4"
    ))?;
    let rows = statement.query_map(
        params![
            project.map(ToString::to_string),
            environment.map(ToString::to_string),
            query.limit,
            query.offset
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (id, project, environment, state, created, updated, kind, related, pending) = row?;
        if updated < created
            || (kind == "deploy" && related.is_some())
            || (kind == "rollback" && related.is_none())
        {
            return Err(corrupt("invalid Deployment context or timestamps"));
        }
        Ok(super::DeploymentRecord {
            deployment: id.parse().map_err(|_| corrupt("invalid Deployment ID"))?,
            project: project.parse().map_err(|_| corrupt("invalid Project ID"))?,
            environment: environment
                .parse()
                .map_err(|_| corrupt("invalid Environment ID"))?,
            state: match state.as_str() {
                "created" => DeploymentState::Created,
                "running" => DeploymentState::Running,
                "succeeded" => DeploymentState::Succeeded,
                "failed" => DeploymentState::Failed,
                "cancelled" => DeploymentState::Cancelled,
                _ => return Err(corrupt("invalid Deployment state")),
            },
            kind: match kind.as_str() {
                "deploy" => DeploymentKind::Deploy,
                "rollback" => DeploymentKind::Rollback,
                _ => return Err(corrupt("invalid Deployment kind")),
            },
            related_deployment: related
                .map(|id| {
                    id.parse()
                        .map_err(|_| corrupt("invalid related Deployment ID"))
                })
                .transpose()?,
            created_at_ms: nonnegative(created)?,
            updated_at_ms: nonnegative(updated)?,
            pending_intent_count: nonnegative(pending)?,
        })
    })
    .collect()
}

fn corrupt(message: &str) -> HistoryError {
    HistoryError::Corrupt(message.into())
}

fn nonnegative(value: i64) -> Result<u64, HistoryError> {
    u64::try_from(value).map_err(|_| corrupt("negative persisted recovery value"))
}
