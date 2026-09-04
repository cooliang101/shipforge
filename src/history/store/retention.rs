//! Bounded historical evidence for conservative Release retention decisions.

use std::collections::BTreeSet;

use rusqlite::{Connection, Row, params};
use serde::de::DeserializeOwned;

use super::{
    DeploymentComponentSnapshot, DeploymentId, HistoryError, HistoryStore, InspectionScope,
    ObservationRecord, ReleasePackageRecord, ReleaseVersion, validate_text,
};
use crate::{domain::ReleaseManifest, drivers::ReleaseRef};

const MAX_ROWS: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const ACTIVE: &str = "(d.state IN ('created','running') OR EXISTS(SELECT 1 FROM operation_intents i WHERE i.deployment_id=d.id AND i.status='pending'))";
const CONTEXT_COLUMNS: &str = "d.id,d.created_at_ms,d.kind,d.related_deployment_id,s.snapshot,s.component,s.execution_order,h.component_count,d.state,d.updated_at_ms";
const CONTEXT_BYTES: &str = "length(CAST(d.id AS BLOB))+length(CAST(d.kind AS BLOB))+length(CAST(d.state AS BLOB))+COALESCE(length(CAST(d.related_deployment_id AS BLOB)),0)+COALESCE(length(CAST(s.snapshot AS BLOB)),0)+COALESCE(length(CAST(s.component AS BLOB)),0)";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionHistory {
    /// Original full references and canonical package receipts, never rewritten.
    pub packages: Vec<ReleasePackageRecord>,
    /// Actual positive health observations, newest insertion first (not wall clock).
    pub healthy: Vec<ObservationRecord>,
    /// Names conservatively protected across revisions of the same Destination.
    pub protected_versions: BTreeSet<ReleaseVersion>,
}

impl HistoryStore {
    /// Reads scoped historical retention evidence in one consistent transaction.
    /// Each record class is limited to 4096 rows and 4 MiB; overflow fails closed.
    /// Old endpoint/revision references are preserved for the caller to compare.
    ///
    /// # Errors
    /// Rejects missing active snapshots, ambiguous pending targets, corrupt records,
    /// excessive evidence, or database errors. Never infers health from status.
    pub fn retention_history(
        &self,
        scope: &InspectionScope,
    ) -> Result<RetentionHistory, HistoryError> {
        crate::drivers::DriverKind::parse(scope.driver.as_str())
            .map_err(|_| corrupt("invalid retention Driver"))?;
        let transaction = self.connection.unchecked_transaction()?;
        let mut history = RetentionHistory::default();
        let active = self.retention_active(scope, &mut history.protected_versions)?;
        read_packages(&transaction, scope, &active, &mut history)?;
        read_observations(&transaction, scope, &active, &mut history)?;
        read_receipts(&transaction, scope, &mut history.protected_versions)?;
        transaction.commit()?;
        Ok(history)
    }

    fn retention_active(
        &self,
        scope: &InspectionScope,
        protected: &mut BTreeSet<ReleaseVersion>,
    ) -> Result<BTreeSet<String>, HistoryError> {
        let source = format!(
            "FROM deployments d WHERE d.project_id=?1 AND d.environment_id=?2 AND ({ACTIVE}
             OR d.state NOT IN ('created','running','succeeded','failed','cancelled'))"
        );
        let arguments = [scope.project.to_string(), scope.environment.to_string()];
        let mut records = Budget::default();
        records.check(&self.connection, "length(CAST(d.id AS BLOB))+length(CAST(d.project_id AS BLOB))+length(CAST(d.environment_id AS BLOB))+length(CAST(d.kind AS BLOB))+length(CAST(d.state AS BLOB))+COALESCE(length(CAST(d.related_deployment_id AS BLOB)),0)", &source, &arguments)?;
        let mut statement = self
            .connection
            .prepare(&format!("SELECT d.id {source} ORDER BY d.id"))?;
        let ids = statement
            .query_map(params![arguments[0], arguments[1]], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut snapshots_budget = Budget::default();
        let mut intents_budget = Budget::default();
        let mut active = BTreeSet::new();
        for id in ids {
            let deployment: DeploymentId = id
                .parse()
                .map_err(|_| corrupt("invalid active Deployment ID"))?;
            snapshots_budget.check(
                &self.connection,
                "length(CAST(snapshot AS BLOB))+length(CAST(component AS BLOB))",
                "FROM component_snapshots WHERE deployment_id=?1",
                std::slice::from_ref(&id),
            )?;
            let snapshots = self.component_snapshots(&deployment)?;
            if snapshots.is_empty() {
                return Err(corrupt(
                    "active Deployment has no trustworthy frozen selection",
                ));
            }
            let selected = snapshots
                .iter()
                .find(|snapshot| snapshot.release.component == scope.component);
            let pending = self.retention_pending(&deployment, scope, &mut intents_budget)?;
            let Some(snapshot) = selected else {
                if !pending.is_empty() {
                    return Err(corrupt("pending Component is absent from frozen selection"));
                }
                continue;
            };
            if !matches_scope(scope, &snapshot.release) {
                continue;
            }
            active.insert(id);
            for release in std::iter::once(&snapshot.release)
                .chain(snapshot.target.iter())
                .chain(snapshot.expected_current.iter())
            {
                protected.insert(release.version.clone());
            }
            protect_pending(&pending, snapshot, protected)?;
        }
        Ok(active)
    }

    fn retention_pending(
        &self,
        deployment: &DeploymentId,
        scope: &InspectionScope,
        budget: &mut Budget,
    ) -> Result<Vec<Pending>, HistoryError> {
        let arguments = [deployment.to_string(), scope.component.to_string()];
        let source =
            "FROM operation_intents WHERE deployment_id=?1 AND component=?2 AND status='pending'";
        budget.check(&self.connection,"length(CAST(stage AS BLOB))+length(CAST(target AS BLOB))+COALESCE(length(CAST(error AS BLOB)),0)",source,&arguments)?;
        let mut statement = self.connection.prepare(&format!(
            "SELECT id,stage,target,created_at_ms,completed_at_ms,error {source} ORDER BY id"
        ))?;
        let created = self
            .deployment(deployment)?
            .ok_or_else(|| corrupt("missing active Deployment"))?
            .created_at_ms;
        let rows = statement.query_map(params![arguments[0], arguments[1]], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        rows.map(|row| {
            let (id, stage, target, at, completed, error) = row?;
            if id <= 0 || nonnegative(at)? < created || completed.is_some() || error.is_some() {
                return Err(corrupt("inconsistent pending intent"));
            }
            validate_text("stage", &stage).map_err(|_| corrupt("invalid pending stage"))?;
            validate_text("target", &target).map_err(|_| corrupt("invalid pending target"))?;
            Ok(Pending { stage, target })
        })
        .collect()
    }
}

#[derive(Default)]
struct Budget {
    rows: usize,
    bytes: usize,
}

impl Budget {
    fn check(
        &mut self,
        connection: &Connection,
        bytes: &str,
        source: &str,
        arguments: &[String],
    ) -> Result<(), HistoryError> {
        let mut statement = connection.prepare(&format!("SELECT {bytes} {source} LIMIT 4097"))?;
        let rows = statement.query_map(rusqlite::params_from_iter(arguments), |row| {
            row.get::<_, i64>(0)
        })?;
        for size in rows {
            self.rows += 1;
            self.bytes = self.bytes.saturating_add(
                usize::try_from(size?).map_err(|_| corrupt("invalid retention byte size"))?,
            );
            if self.rows > MAX_ROWS || self.bytes > MAX_BYTES {
                return Err(HistoryError::InvalidMetadata(
                    "retention evidence exceeds bounded row or byte limits",
                ));
            }
        }
        Ok(())
    }
}

struct Context {
    deployment: String,
    created: i64,
    kind: String,
    related: Option<String>,
    snapshot: String,
    component: String,
    order: u32,
    count: u32,
    state: String,
    updated: i64,
}

impl Context {
    fn read(row: &Row<'_>, start: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            deployment: row.get(start)?,
            created: row.get(start + 1)?,
            kind: row.get(start + 2)?,
            related: row.get(start + 3)?,
            snapshot: row.get(start + 4)?,
            component: row.get(start + 5)?,
            order: row.get(start + 6)?,
            count: row.get(start + 7)?,
            state: row.get(start + 8)?,
            updated: row.get(start + 9)?,
        })
    }

    fn validate(
        &self,
        scope: &InspectionScope,
    ) -> Result<DeploymentComponentSnapshot, HistoryError> {
        self.deployment
            .parse::<DeploymentId>()
            .map_err(|_| corrupt("invalid evidence Deployment ID"))?;
        if let Some(related) = &self.related {
            related
                .parse::<DeploymentId>()
                .map_err(|_| corrupt("invalid related Deployment ID"))?;
        }
        if self.created < 0
            || self.updated < self.created
            || !matches!(
                self.state.as_str(),
                "created" | "running" | "succeeded" | "failed" | "cancelled"
            )
            || !(1..=256).contains(&self.count)
            || self.order >= self.count
            || !matches!(
                (self.kind.as_str(), self.related.is_some()),
                ("deploy", false) | ("rollback", true)
            )
        {
            return Err(corrupt("invalid evidence parent or snapshot index"));
        }
        let snapshot: DeploymentComponentSnapshot = decode(&self.snapshot)?;
        let release = &snapshot.release;
        if release.project_id != scope.project
            || release.environment_id != scope.environment
            || release.component != scope.component
            || self.component != scope.component.as_str()
            || snapshot.execution_order != self.order
            || snapshot
                .target
                .as_ref()
                .or(snapshot.expected_current.as_ref())
                != Some(release)
            || (self.kind == "deploy" && snapshot.target.is_none())
        {
            return Err(corrupt("retention snapshot differs from indexed scope"));
        }
        validate_ref(release)?;
        for reference in snapshot
            .target
            .iter()
            .chain(snapshot.expected_current.iter())
        {
            if InspectionScope::from(reference) != InspectionScope::from(release) {
                return Err(corrupt("inconsistent frozen Release scope"));
            }
            validate_ref(reference)?;
        }
        Ok(snapshot)
    }
}

fn evidence_source(table: &str) -> String {
    format!("FROM {table} e JOIN deployments d ON d.id=e.deployment_id
        LEFT JOIN component_snapshots s ON s.deployment_id=e.deployment_id AND s.component=e.component
        LEFT JOIN deployment_snapshots h ON h.deployment_id=e.deployment_id
        WHERE d.project_id=?1 AND d.environment_id=?2 AND e.component=?3")
}

fn scope_arguments(scope: &InspectionScope) -> [String; 3] {
    [
        scope.project.to_string(),
        scope.environment.to_string(),
        scope.component.to_string(),
    ]
}

fn read_packages(
    connection: &Connection,
    scope: &InspectionScope,
    active: &BTreeSet<String>,
    history: &mut RetentionHistory,
) -> Result<(), HistoryError> {
    let source = evidence_source("release_packages");
    let arguments = scope_arguments(scope);
    Budget::default().check(connection,&format!("{CONTEXT_BYTES}+length(CAST(e.release_ref AS BLOB))+length(CAST(e.manifest AS BLOB))+length(CAST(e.sha256 AS BLOB))"),&source,&arguments)?;
    let mut statement = connection.prepare(&format!("SELECT e.release_ref,e.manifest,e.sha256,e.size,{CONTEXT_COLUMNS} {source} ORDER BY e.rowid DESC"))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            Context::read(row, 4)?,
        ))
    })?;
    for row in rows {
        let (reference, manifest, sha256, size, context) = row?;
        let snapshot = context.validate(scope)?;
        if context.kind != "deploy" {
            return Err(corrupt("package receipt belongs to a non-Deploy operation"));
        }
        let release: ReleaseRef = decode(&reference)?;
        let manifest: ReleaseManifest = decode(&manifest)?;
        validate_package(&release, &manifest, &sha256, size)?;
        if snapshot.target.as_ref() != Some(&release) {
            return Err(corrupt("package differs from frozen target"));
        }
        if matches_scope(scope, &release) {
            if active.contains(&context.deployment) {
                history.protected_versions.insert(release.version.clone());
            }
            history.packages.push(ReleasePackageRecord {
                release,
                manifest,
                sha256,
                size: nonnegative(size)?,
            });
        }
    }
    Ok(())
}

fn read_observations(
    connection: &Connection,
    scope: &InspectionScope,
    active: &BTreeSet<String>,
    history: &mut RetentionHistory,
) -> Result<(), HistoryError> {
    let source = evidence_source("deployment_observations");
    let arguments = scope_arguments(scope);
    Budget::default().check(connection,&format!("{CONTEXT_BYTES}+length(CAST(e.stage AS BLOB))+COALESCE(length(CAST(e.observed_ref AS BLOB)),0)+COALESCE(length(CAST(e.error AS BLOB)),0)"),&source,&arguments)?;
    let mut statement = connection.prepare(&format!("SELECT e.id,e.stage,e.observed_ref,e.error,e.healthy,e.observed_at_ms,{CONTEXT_COLUMNS} {source} ORDER BY e.id DESC"))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, i64>(5)?,
            Context::read(row, 6)?,
        ))
    })?;
    for row in rows {
        let (sequence, stage, reference, error, health, at, context) = row?;
        let snapshot = context.validate(scope)?;
        validate_text("stage", &stage).map_err(|_| corrupt("invalid observation stage"))?;
        if sequence <= 0 || at < context.created || !matches!(health, None | Some(0 | 1)) {
            return Err(corrupt("invalid observation time or health"));
        }
        let release: Option<ReleaseRef> = reference.as_deref().map(decode).transpose()?;
        if let Some(release) = &release {
            validate_ref(release)?;
            if InspectionScope::from(release) != InspectionScope::from(&snapshot.release) {
                return Err(corrupt("observation differs from frozen context"));
            }
        }
        if let Some(error) = error {
            validate_text("error", &error)
                .map_err(|_| corrupt("invalid observation diagnostic"))?;
            if release.is_some() || health.is_some() {
                return Err(corrupt("unknown observation claims Release or health"));
            }
        }
        let Some(release) = release else {
            continue;
        };
        if !matches_scope(scope, &release) {
            continue;
        }
        if active.contains(&context.deployment) {
            history.protected_versions.insert(release.version.clone());
        }
        if health == Some(1) {
            history.healthy.push(ObservationRecord {
                sequence,
                component: scope.component.clone(),
                stage,
                observed: Ok(Some(release)),
                healthy: Some(true),
                observed_at_ms: nonnegative(at)?,
            });
        }
    }
    Ok(())
}

fn read_receipts(
    connection: &Connection,
    scope: &InspectionScope,
    protected: &mut BTreeSet<ReleaseVersion>,
) -> Result<(), HistoryError> {
    let source = format!("{} AND {ACTIVE}", evidence_source("release_receipts"));
    let arguments = scope_arguments(scope);
    Budget::default().check(
        connection,
        &format!(
            "{CONTEXT_BYTES}+length(CAST(e.stage AS BLOB))+length(CAST(e.release_ref AS BLOB))"
        ),
        &source,
        &arguments,
    )?;
    let mut statement = connection.prepare(&format!(
        "SELECT e.stage,e.release_ref,e.recorded_at_ms,{CONTEXT_COLUMNS} {source} ORDER BY e.id"
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&arguments), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            Context::read(row, 3)?,
        ))
    })?;
    for row in rows {
        let (stage, reference, at, context) = row?;
        let snapshot = context.validate(scope)?;
        let release: ReleaseRef = decode(&reference)?;
        validate_ref(&release)?;
        validate_text("stage", &stage).map_err(|_| corrupt("invalid receipt stage"))?;
        if at < context.created || snapshot.target.as_ref() != Some(&release) {
            return Err(corrupt("receipt differs from frozen target or time"));
        }
        if matches_scope(scope, &release) {
            protected.insert(release.version);
        }
    }
    Ok(())
}

struct Pending {
    stage: String,
    target: String,
}

fn protect_pending(
    pending: &[Pending],
    snapshot: &DeploymentComponentSnapshot,
    protected: &mut BTreeSet<ReleaseVersion>,
) -> Result<(), HistoryError> {
    for intent in pending {
        let confirmed_absence = intent.target == "not_deployed"
            && match intent.stage.as_str() {
                "rollback" => snapshot.target.is_none(),
                "compensate" => snapshot.expected_current.is_none(),
                _ => false,
            };
        if confirmed_absence {
            continue;
        }
        let version = ReleaseVersion::parse(&intent.target)
            .map_err(|_| corrupt("pending intent target cannot be safely interpreted"))?;
        protected.insert(version);
    }
    Ok(())
}

fn matches_scope(scope: &InspectionScope, release: &ReleaseRef) -> bool {
    release.project_id == scope.project
        && release.environment_id == scope.environment
        && release.component == scope.component
        && release.generation == scope.generation
        && release.driver == scope.driver
        && release.destination == scope.destination
}

fn validate_ref(reference: &ReleaseRef) -> Result<(), HistoryError> {
    crate::drivers::DriverKind::parse(reference.driver.as_str())
        .map_err(|_| corrupt("invalid historical Driver"))?;
    Ok(())
}

fn validate_package(
    reference: &ReleaseRef,
    manifest: &ReleaseManifest,
    sha256: &str,
    size: i64,
) -> Result<(), HistoryError> {
    validate_ref(reference)?;
    if manifest.schema_version != 1
        || manifest.project_id != reference.project_id
        || manifest.environment_id != reference.environment_id
        || manifest.component != reference.component
        || manifest.generation != reference.generation
        || manifest.version != reference.version
        || manifest.source_revision.as_ref().is_some_and(|revision| {
            !(7..=64).contains(&revision.len())
                || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || size <= 0
    {
        return Err(corrupt("invalid historical package metadata"));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(value: &str) -> Result<T, HistoryError> {
    serde_json::from_str(value).map_err(|_| corrupt("invalid structured retention evidence"))
}

fn nonnegative(value: i64) -> Result<u64, HistoryError> {
    u64::try_from(value).map_err(|_| corrupt("negative retention timestamp or size"))
}

fn corrupt(message: &str) -> HistoryError {
    HistoryError::Corrupt(message.into())
}

#[cfg(test)]
mod tests;
