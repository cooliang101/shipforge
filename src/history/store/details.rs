use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    ComponentDeploymentResult, ComponentName, Connection, DeploymentId, DeploymentState,
    EnvironmentId, FromStr, HistoryError, HistoryStore, IntentId, OptionalExtension, ProjectId,
    Redactor, corrupt, params, sanitize_diagnostic, timestamp, validate_text,
};
use crate::{domain::ReleaseManifest, drivers::ReleaseRef};

pub(super) const MIGRATION_5: &str = r"
CREATE INDEX deployments_scope_page ON deployments(project_id,environment_id,created_at_ms DESC,id DESC);
CREATE TABLE deployment_metadata (
 deployment_id TEXT PRIMARY KEY NOT NULL REFERENCES deployments(id), metadata TEXT NOT NULL
) STRICT;
CREATE TABLE deployment_snapshots (
 deployment_id TEXT PRIMARY KEY NOT NULL REFERENCES deployments(id),
 component_count INTEGER NOT NULL CHECK(component_count BETWEEN 1 AND 256)
) STRICT;
CREATE TABLE component_snapshots (
 deployment_id TEXT NOT NULL REFERENCES deployment_snapshots(deployment_id),
 component TEXT NOT NULL, execution_order INTEGER NOT NULL CHECK(execution_order BETWEEN 0 AND 255),
 snapshot TEXT NOT NULL, PRIMARY KEY(deployment_id,component), UNIQUE(deployment_id,execution_order)
) STRICT;
CREATE TABLE release_packages (
 deployment_id TEXT NOT NULL, component TEXT NOT NULL, release_ref TEXT NOT NULL,
 manifest TEXT NOT NULL, sha256 TEXT NOT NULL CHECK(length(sha256)=64),
 size INTEGER NOT NULL CHECK(size>0), PRIMARY KEY(deployment_id,component),
 FOREIGN KEY(deployment_id,component) REFERENCES component_snapshots(deployment_id,component)
) STRICT;
CREATE TABLE deployment_observations (
 id INTEGER PRIMARY KEY AUTOINCREMENT, deployment_id TEXT NOT NULL, component TEXT NOT NULL,
 stage TEXT NOT NULL, observed_ref TEXT, error TEXT, healthy INTEGER CHECK(healthy IN (0,1)),
 observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms>=0),
 CHECK(error IS NULL OR (observed_ref IS NULL AND healthy IS NULL)),
 FOREIGN KEY(deployment_id,component) REFERENCES component_snapshots(deployment_id,component)
) STRICT;
CREATE INDEX observations_deployment ON deployment_observations(deployment_id,id);
CREATE TABLE release_receipts (
 id INTEGER PRIMARY KEY AUTOINCREMENT, deployment_id TEXT NOT NULL, component TEXT NOT NULL,
 stage TEXT NOT NULL, release_ref TEXT NOT NULL, recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0),
 UNIQUE(deployment_id,component,stage),
 FOREIGN KEY(deployment_id,component) REFERENCES component_snapshots(deployment_id,component)
) STRICT;
CREATE TABLE deployment_steps (
 id INTEGER PRIMARY KEY AUTOINCREMENT, deployment_id TEXT NOT NULL REFERENCES deployments(id),
 component TEXT NOT NULL, name TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','running','succeeded','failed','skipped')),
 intent_id INTEGER UNIQUE REFERENCES operation_intents(id), started_at_ms INTEGER,
 completed_at_ms INTEGER, error TEXT, UNIQUE(deployment_id,component,name),
 FOREIGN KEY(deployment_id,component) REFERENCES component_snapshots(deployment_id,component),
 CHECK(COALESCE((status='pending' AND intent_id IS NULL AND started_at_ms IS NULL AND completed_at_ms IS NULL AND error IS NULL)
    OR (status='skipped' AND intent_id IS NULL AND started_at_ms IS NULL AND completed_at_ms>=0 AND error IS NULL)
    OR (status='running' AND intent_id IS NOT NULL AND started_at_ms>=0 AND completed_at_ms IS NULL AND error IS NULL)
    OR (status='succeeded' AND intent_id IS NOT NULL AND started_at_ms>=0 AND completed_at_ms>=started_at_ms AND error IS NULL)
    OR (status='failed' AND intent_id IS NOT NULL AND started_at_ms>=0 AND completed_at_ms>=started_at_ms AND error IS NOT NULL),0))
) STRICT;
CREATE INDEX steps_deployment ON deployment_steps(deployment_id,id);
";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentComponentSnapshot {
    /// Absent in older history; never backfilled from live configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_snapshot: Option<serde_json::Value>,
    /// Target Release, or the expected current Release when rolling back to absence.
    pub release: ReleaseRef,
    pub expected_current: Option<ReleaseRef>,
    pub target: Option<ReleaseRef>,
    pub execution_order: u32,
}

impl std::fmt::Debug for DeploymentComponentSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeploymentComponentSnapshot")
            .field("release", &self.release)
            .field("execution_order", &self.execution_order)
            .field("target_snapshot_present", &self.target_snapshot.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitWorktree {
    Clean,
    Dirty,
    NotRepository,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentMetadata {
    pub git_branch: Option<String>,
    pub git_revision: Option<String>,
    pub git_worktree: GitWorktree,
    pub operator: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentKind {
    Deploy,
    Rollback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentRecord {
    pub deployment: DeploymentId,
    pub project: ProjectId,
    pub environment: EnvironmentId,
    pub state: DeploymentState,
    pub kind: DeploymentKind,
    pub related_deployment: Option<DeploymentId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub pending_intent_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeploymentQuery {
    pub limit: u32,
    pub offset: u32,
    pub nonterminal_only: bool,
}

impl Default for DeploymentQuery {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
            nonterminal_only: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasePackageRecord {
    pub release: ReleaseRef,
    pub manifest: ReleaseManifest,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationRecord {
    pub sequence: i64,
    pub component: ComponentName,
    pub stage: String,
    /// `Ok(None)` is confirmed absence; an error means observation was unknown.
    pub observed: Result<Option<ReleaseRef>, String>,
    /// Recorded evidence only; never inferred from a Release or Deployment status.
    pub healthy: Option<bool>,
    pub observed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseReceiptRecord {
    pub sequence: i64,
    pub component: ComponentName,
    pub stage: String,
    pub release: ReleaseRef,
    pub recorded_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRecord {
    /// One-based sequence of this query's stable planned-then-legacy ordering.
    pub sequence: u64,
    pub component: ComponentName,
    pub name: String,
    pub status: StepStatus,
    pub intent: Option<IntentId>,
    pub started_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub error: Option<String>,
    /// False denotes an old/unplanned intent, not an invented historical plan.
    pub planned: bool,
}

impl HistoryStore {
    /// Records the immutable selected Component plan before any side effects.
    ///
    /// # Errors
    /// Returns an error for invalid scope/order, an existing snapshot, or prior intents.
    pub fn record_component_snapshots(
        &self,
        deployment: &DeploymentId,
        snapshots: &[DeploymentComponentSnapshot],
    ) -> Result<(), HistoryError> {
        let transaction = self.connection.unchecked_transaction()?;
        let record = require_unstarted(&transaction, deployment)?;
        validate_snapshots(&record, snapshots)?;
        transaction.execute(
            "INSERT INTO deployment_snapshots VALUES (?1,?2)",
            params![
                deployment.to_string(),
                u32::try_from(snapshots.len())
                    .map_err(|_| HistoryError::InvalidMetadata("too many Components"))?
            ],
        )?;
        for snapshot in snapshots {
            transaction.execute(
                "INSERT INTO component_snapshots VALUES (?1,?2,?3,?4)",
                params![
                    deployment.to_string(),
                    snapshot.release.component.as_str(),
                    snapshot.execution_order,
                    encode(snapshot)?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Loads the frozen plan in execution order; old deployments have no snapshots.
    ///
    /// # Errors
    /// Returns an error for corrupt persisted identities/order or database failure.
    pub fn component_snapshots(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Vec<DeploymentComponentSnapshot>, HistoryError> {
        let mut statement = self.connection.prepare("SELECT snapshot,component,execution_order FROM component_snapshots WHERE deployment_id=?1 ORDER BY execution_order")?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })?;
        let mut values = Vec::new();
        for row in rows {
            let (json, component, order) = row?;
            let snapshot: DeploymentComponentSnapshot = decode(&json)?;
            if snapshot.release.component.as_str() != component || snapshot.execution_order != order
            {
                return Err(corrupt(&"Component snapshot index mismatch"));
            }
            values.push(snapshot);
        }
        let count: Option<u32> = self
            .connection
            .query_row(
                "SELECT component_count FROM deployment_snapshots WHERE deployment_id=?1",
                [deployment.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match count {
            None if values.is_empty() => return Ok(values),
            Some(count) if usize::try_from(count).ok() == Some(values.len()) => {}
            _ => return Err(corrupt(&"Component snapshot count mismatch")),
        }
        let record = self
            .deployment(deployment)?
            .ok_or_else(|| corrupt(&"missing snapshot Deployment"))?;
        validate_snapshots(&record, &values)
            .map_err(|_| corrupt(&"invalid Component snapshot scope"))?;
        Ok(values)
    }

    /// Persists sanitized actor and Git context once, before any intents.
    ///
    /// # Errors
    /// Returns an error for invalid revision/context, duplicate data, or prior intents.
    pub fn record_deployment_metadata(
        &self,
        deployment: &DeploymentId,
        metadata: &DeploymentMetadata,
        redactor: &Redactor,
    ) -> Result<(), HistoryError> {
        validate_revision(metadata.git_revision.as_deref())?;
        if metadata.git_worktree == GitWorktree::NotRepository
            && (metadata.git_branch.is_some() || metadata.git_revision.is_some())
        {
            return Err(HistoryError::InvalidMetadata("non-repository Git context"));
        }
        let mut metadata = metadata.clone();
        metadata.git_branch = metadata
            .git_branch
            .map(|value| sanitize_diagnostic(&redactor.redact(&value)));
        metadata.operator = metadata
            .operator
            .map(|value| sanitize_diagnostic(&redactor.redact(&value)));
        let transaction = self.connection.unchecked_transaction()?;
        require_unstarted(&transaction, deployment)?;
        transaction.execute(
            "INSERT INTO deployment_metadata VALUES (?1,?2)",
            params![deployment.to_string(), encode(&metadata)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns optional context; absence means legacy/unrecorded, not clean Git state.
    ///
    /// # Errors
    /// Returns an error for corrupted metadata or database failure.
    pub fn deployment_metadata(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<DeploymentMetadata>, HistoryError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT metadata FROM deployment_metadata WHERE deployment_id=?1",
                [deployment.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| {
            let metadata: DeploymentMetadata = decode(&json)?;
            validate_revision(metadata.git_revision.as_deref())
                .map_err(|_| corrupt(&"invalid Git revision"))?;
            for value in [&metadata.git_branch, &metadata.operator]
                .into_iter()
                .flatten()
            {
                validate_text("metadata", value).map_err(|_| corrupt(&"unsafe metadata"))?;
            }
            if metadata.git_worktree == GitWorktree::NotRepository
                && (metadata.git_branch.is_some() || metadata.git_revision.is_some())
            {
                return Err(corrupt(&"non-repository Git context"));
            }
            Ok(metadata)
        })
        .transpose()
    }

    /// Records the core package identity, manifest and digest without its local path.
    ///
    /// # Errors
    /// Returns an error for invalid/mismatched metadata or duplicate package receipt.
    pub fn record_release_package(
        &self,
        deployment: &DeploymentId,
        release: &ReleaseRef,
        manifest: &ReleaseManifest,
        sha256: &str,
        size: u64,
    ) -> Result<(), HistoryError> {
        validate_package(release, manifest, sha256, size)?;
        let transaction = self.connection.unchecked_transaction()?;
        let record = load_deployment(&transaction, deployment)?
            .ok_or_else(|| HistoryError::NotRunning(deployment.clone()))?;
        if record.state != DeploymentState::Running || record.kind != DeploymentKind::Deploy {
            return Err(HistoryError::NotRunning(deployment.clone()));
        }
        let snapshot = load_snapshot(&transaction, deployment, &release.component)?;
        if snapshot.target.as_ref() != Some(release) || snapshot.release != *release {
            return Err(HistoryError::InvalidMetadata(
                "package differs from frozen target",
            ));
        }
        let existing:Option<(String,String,String,i64)>=transaction.query_row("SELECT release_ref,manifest,sha256,size FROM release_packages WHERE deployment_id=?1 AND component=?2",params![deployment.to_string(),release.component.as_str()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        if let Some((stored_release, stored_manifest, stored_digest, stored_size)) = existing {
            if decode::<ReleaseRef>(&stored_release)? != *release
                || decode::<ReleaseManifest>(&stored_manifest)? != *manifest
                || stored_digest != sha256
                || stored_size != timestamp(size)?
            {
                return Err(HistoryError::InvalidMetadata("conflicting package receipt"));
            }
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute(
            "INSERT INTO release_packages VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                deployment.to_string(),
                release.component.as_str(),
                encode(release)?,
                encode(manifest)?,
                sha256,
                timestamp(size)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads checked package receipts in Component execution order.
    ///
    /// # Errors
    /// Returns an error for corrupt identities or database failure.
    pub fn release_packages(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Vec<ReleasePackageRecord>, HistoryError> {
        let snapshots = self.component_snapshots(deployment)?;
        let mut statement = self.connection.prepare("SELECT p.component,p.release_ref,p.manifest,p.sha256,p.size FROM release_packages p JOIN component_snapshots s USING(deployment_id,component) WHERE p.deployment_id=?1 ORDER BY s.execution_order")?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (component, release, manifest, sha256, size) = row?;
            let release: ReleaseRef = decode(&release)?;
            let manifest = decode(&manifest)?;
            let size = u64::try_from(size).map_err(|_| corrupt(&"invalid package size"))?;
            validate_package(&release, &manifest, &sha256, size)
                .map_err(|_| corrupt(&"invalid package metadata"))?;
            if component != release.component.as_str()
                || !snapshots
                    .iter()
                    .any(|snapshot| snapshot.target.as_ref() == Some(&release))
            {
                return Err(corrupt(&"package snapshot mismatch"));
            }
            Ok(ReleasePackageRecord {
                release,
                manifest,
                sha256,
                size,
            })
        })
        .collect()
    }

    /// Records a Driver's returned Release reference, distinct from current state.
    ///
    /// # Errors
    /// Returns an error for duplicate receipts, invalid time or out-of-scope identity.
    pub fn record_release_receipt(
        &self,
        deployment: &DeploymentId,
        component: &ComponentName,
        stage: &str,
        release: &ReleaseRef,
        at_ms: u64,
    ) -> Result<(), HistoryError> {
        validate_text("stage", stage)?;
        let transaction = self.connection.unchecked_transaction()?;
        let record = load_deployment(&transaction, deployment)?
            .ok_or_else(|| HistoryError::NotRunning(deployment.clone()))?;
        if record.state != DeploymentState::Running {
            return Err(HistoryError::NotRunning(deployment.clone()));
        }
        if at_ms < record.created_at_ms {
            return Err(HistoryError::InvalidMetadata("receipt predates Deployment"));
        }
        let snapshot = load_snapshot(&transaction, deployment, component)?;
        validate_same_scope(&snapshot.release, release)?;
        if snapshot.target.as_ref() != Some(release) {
            return Err(HistoryError::InvalidMetadata(
                "receipt differs from planned target",
            ));
        }
        transaction.execute("INSERT INTO release_receipts (deployment_id,component,stage,release_ref,recorded_at_ms) VALUES (?1,?2,?3,?4,?5)",params![deployment.to_string(),component.as_str(),stage,encode(release)?,timestamp(at_ms)?])?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads Driver receipts in durable sequence order without implying activation.
    ///
    /// # Errors
    /// Returns an error for corrupt receipt identity or database failure.
    pub fn release_receipts(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Vec<ReleaseReceiptRecord>, HistoryError> {
        let snapshots = self.component_snapshots(deployment)?;
        let created_at_ms = self
            .deployment(deployment)?
            .map_or(0, |record| record.created_at_ms);
        let mut statement=self.connection.prepare("SELECT id,component,stage,release_ref,recorded_at_ms FROM release_receipts WHERE deployment_id=?1 ORDER BY id")?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, component, stage, release, at) = row?;
            let recorded_at_ms = u64::try_from(at).map_err(|_| corrupt(&"invalid receipt time"))?;
            if recorded_at_ms < created_at_ms {
                return Err(corrupt(&"receipt predates Deployment"));
            }
            let component = ComponentName::parse(component).map_err(|error| corrupt(&error))?;
            let release: ReleaseRef = decode(&release)?;
            validate_text("stage", &stage).map_err(|_| corrupt(&"invalid receipt stage"))?;
            if component != release.component
                || !snapshots
                    .iter()
                    .any(|snapshot| snapshot.target.as_ref() == Some(&release))
            {
                return Err(corrupt(&"receipt target mismatch"));
            }
            Ok(ReleaseReceiptRecord {
                sequence,
                component,
                stage,
                release,
                recorded_at_ms,
            })
        })
        .collect()
    }

    /// Appends an explicit observation, preserving unknown versus known absence.
    ///
    /// # Errors
    /// Returns an error for out-of-scope refs, impossible health evidence, or invalid time.
    #[allow(clippy::too_many_arguments)]
    pub fn record_observation(
        &self,
        deployment: &DeploymentId,
        component: &ComponentName,
        stage: &str,
        observed: Result<Option<&ReleaseRef>, &str>,
        healthy: Option<bool>,
        at_ms: u64,
        redactor: &Redactor,
    ) -> Result<(), HistoryError> {
        validate_text("stage", stage)?;
        let transaction = self.connection.unchecked_transaction()?;
        let snapshot = load_snapshot(&transaction, deployment, component)?;
        let record = load_deployment(&transaction, deployment)?
            .ok_or_else(|| HistoryError::InvalidMetadata("missing Deployment"))?;
        if at_ms < record.created_at_ms {
            return Err(HistoryError::InvalidMetadata(
                "observation predates Deployment",
            ));
        }
        let (release, error) = match observed {
            Ok(release) => {
                if let Some(release) = release {
                    validate_same_scope(&snapshot.release, release)?;
                }
                (release.map(encode).transpose()?, None)
            }
            Err(error) if healthy.is_none() => {
                (None, Some(sanitize_diagnostic(&redactor.redact(error))))
            }
            Err(_) => {
                return Err(HistoryError::InvalidMetadata(
                    "unknown observation cannot establish health",
                ));
            }
        };
        transaction.execute("INSERT INTO deployment_observations (deployment_id,component,stage,observed_ref,error,healthy,observed_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![deployment.to_string(),component.as_str(),stage,release,error,healthy,timestamp(at_ms)?])?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads observation evidence in append order, never rewriting previous evidence.
    ///
    /// # Errors
    /// Returns an error for invalid persisted context or database failure.
    pub fn observations(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Vec<ObservationRecord>, HistoryError> {
        let snapshots = self.component_snapshots(deployment)?;
        let created_at_ms = self
            .deployment(deployment)?
            .map_or(0, |record| record.created_at_ms);
        let mut statement = self.connection.prepare("SELECT id,component,stage,observed_ref,error,healthy,observed_at_ms FROM deployment_observations WHERE deployment_id=?1 ORDER BY id")?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<bool>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, component, stage, release, error, healthy, at) = row?;
            let observed_at_ms =
                u64::try_from(at).map_err(|_| corrupt(&"negative observation time"))?;
            if observed_at_ms < created_at_ms {
                return Err(corrupt(&"observation predates Deployment"));
            }
            let component = ComponentName::parse(component).map_err(|error| corrupt(&error))?;
            validate_text("stage", &stage).map_err(|_| corrupt(&"invalid observation stage"))?;
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.release.component == component)
                .ok_or_else(|| corrupt(&"missing observation snapshot"))?;
            let release: Option<ReleaseRef> = release.map(|json| decode(&json)).transpose()?;
            if let Some(release) = &release {
                validate_same_scope(&snapshot.release, release)
                    .map_err(|_| corrupt(&"out-of-scope observation"))?;
            }
            let observed = match error {
                Some(error) if release.is_none() && healthy.is_none() => {
                    validate_text("error", &error)
                        .map_err(|_| corrupt(&"unsafe observation error"))?;
                    Err(error)
                }
                Some(_) => return Err(corrupt(&"unknown observation with health or Release")),
                None => Ok(release),
            };
            Ok(ObservationRecord {
                sequence,
                component,
                stage,
                observed,
                healthy,
                observed_at_ms,
            })
        })
        .collect()
    }

    /// Records a Component's named steps once before any intent for that Component.
    ///
    /// # Errors
    /// Returns an error for invalid/duplicate names, unknown selection, or existing work.
    pub fn plan_steps(
        &self,
        deployment: &DeploymentId,
        component: &ComponentName,
        names: &[&str],
    ) -> Result<(), HistoryError> {
        if names.is_empty() || names.len() > 64 {
            return Err(HistoryError::InvalidMetadata(
                "a Component plan requires 1-64 steps",
            ));
        }
        let mut unique = BTreeSet::new();
        for name in names {
            validate_text("step", name)?;
            if !unique.insert(*name) {
                return Err(HistoryError::InvalidMetadata("duplicate step name"));
            }
        }
        let transaction = self.connection.unchecked_transaction()?;
        let record = load_deployment(&transaction, deployment)?
            .ok_or_else(|| HistoryError::InvalidMetadata("missing Deployment"))?;
        if !matches!(
            record.state,
            DeploymentState::Created | DeploymentState::Running
        ) {
            return Err(HistoryError::InvalidMetadata(
                "terminal Deployment step plan",
            ));
        }
        load_snapshot(&transaction, deployment, component)?;
        let occupied: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM operation_intents WHERE deployment_id=?1 AND component=?2 UNION ALL SELECT 1 FROM deployment_steps WHERE deployment_id=?1 AND component=?2)",params![deployment.to_string(),component.as_str()],|row|row.get(0))?;
        if occupied {
            return Err(HistoryError::InvalidMetadata(
                "Component steps already planned or started",
            ));
        }
        for name in names {
            transaction.execute("INSERT INTO deployment_steps (deployment_id,component,name,status) VALUES (?1,?2,?3,'pending')",params![deployment.to_string(),component.as_str(),name])?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Queries planned steps and unmatched legacy intents with explicit timing/state.
    ///
    /// # Errors
    /// Returns an error for corrupt persisted metadata or database failure.
    pub fn steps(&self, deployment: &DeploymentId) -> Result<Vec<StepRecord>, HistoryError> {
        validate_step_links(&self.connection, Some(deployment), None)?;
        let mut statement=self.connection.prepare("SELECT component,name,status,intent_id,started_at_ms,completed_at_ms,error,1 AS planned,id FROM deployment_steps WHERE deployment_id=?1 UNION ALL SELECT component,stage,CASE status WHEN 'pending' THEN 'running' ELSE status END,id,created_at_ms,completed_at_ms,error,0,id FROM operation_intents WHERE deployment_id=?1 AND NOT EXISTS(SELECT 1 FROM deployment_steps WHERE intent_id=operation_intents.id) ORDER BY planned DESC,9")?;
        let rows = statement.query_map([deployment.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })?;
        rows.enumerate()
            .map(|(index, row)| {
                let (component, name, status, intent, start, end, error, planned) = row?;
                validate_text("step", &name).map_err(|_| corrupt(&"invalid step name"))?;
                if let Some(error) = &error {
                    validate_text("error", error)
                        .map_err(|_| corrupt(&"unsafe step diagnostic"))?;
                }
                let status = match status.as_str() {
                    "pending" => StepStatus::Pending,
                    "running" => StepStatus::Running,
                    "succeeded" => StepStatus::Succeeded,
                    "failed" => StepStatus::Failed,
                    "skipped" => StepStatus::Skipped,
                    _ => return Err(corrupt(&"invalid step status")),
                };
                validate_step_state(status, intent, start, end, error.as_deref())?;
                Ok(StepRecord {
                    sequence: u64::try_from(index)
                        .map_err(|_| corrupt(&"step sequence overflow"))?
                        + 1,
                    component: ComponentName::parse(component).map_err(|error| corrupt(&error))?,
                    name,
                    status,
                    intent: intent.map(IntentId),
                    started_at_ms: start
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| corrupt(&"invalid step start"))?,
                    completed_at_ms: end
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| corrupt(&"invalid step end"))?,
                    error,
                    planned,
                })
            })
            .collect()
    }

    /// Finds a Deployment by its globally unique identity without modifying state.
    ///
    /// # Errors
    /// Returns an error for corrupted stored values or database failure.
    pub fn deployment(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<DeploymentRecord>, HistoryError> {
        load_deployment(&self.connection, deployment)
    }

    /// Lists only the requested Project/Environment with deterministic bounded paging.
    ///
    /// # Errors
    /// Returns an error for invalid pagination or corrupted persisted values.
    pub fn deployments(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: DeploymentQuery,
    ) -> Result<Vec<DeploymentRecord>, HistoryError> {
        if !(1..=100).contains(&query.limit) || query.offset > 1_000_000 {
            return Err(HistoryError::InvalidPage);
        }
        let mut statement=self.connection.prepare("SELECT id FROM deployments WHERE project_id=?1 AND environment_id=?2 AND (?3=0 OR state IN ('created','running')) ORDER BY created_at_ms DESC,id DESC LIMIT ?4 OFFSET ?5")?;
        let rows = statement.query_map(
            params![
                project.to_string(),
                environment.to_string(),
                query.nonterminal_only,
                query.limit,
                query.offset
            ],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let id = DeploymentId::from_str(&row?).map_err(|error| corrupt(&error))?;
            self.deployment(&id)?
                .ok_or_else(|| corrupt(&"Deployment disappeared during query"))
        })
        .collect()
    }

    /// Lists unresolved intents even when their Deployment already became terminal.
    /// `nonterminal_only` can additionally restrict results to created/running rows.
    ///
    /// # Errors
    /// Returns an error for invalid pagination or corrupt persisted values.
    pub fn unresolved_deployments(
        &self,
        project: &ProjectId,
        environment: &EnvironmentId,
        query: DeploymentQuery,
    ) -> Result<Vec<DeploymentRecord>, HistoryError> {
        if !(1..=100).contains(&query.limit) || query.offset > 1_000_000 {
            return Err(HistoryError::InvalidPage);
        }
        let mut statement = self.connection.prepare(
            "SELECT id FROM deployments WHERE project_id=?1 AND environment_id=?2
             AND (?3=0 OR state IN ('created','running'))
             AND EXISTS(SELECT 1 FROM operation_intents WHERE deployment_id=deployments.id AND status='pending')
             ORDER BY created_at_ms DESC,id DESC LIMIT ?4 OFFSET ?5",
        )?;
        let rows = statement.query_map(
            params![
                project.to_string(),
                environment.to_string(),
                query.nonterminal_only,
                query.limit,
                query.offset
            ],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let id = DeploymentId::from_str(&row?).map_err(|error| corrupt(&error))?;
            self.deployment(&id)?
                .ok_or_else(|| corrupt(&"Deployment disappeared during query"))
        })
        .collect()
    }
}

pub(super) fn begin_step(
    connection: &Connection,
    deployment: &DeploymentId,
    component: &ComponentName,
    stage: &str,
    intent: IntentId,
    at: u64,
) -> Result<(), HistoryError> {
    validate_selected_component(connection, deployment, component)?;
    let status:Option<String>=connection.query_row("SELECT status FROM deployment_steps WHERE deployment_id=?1 AND component=?2 AND name=?3",params![deployment.to_string(),component.as_str(),stage],|row|row.get(0)).optional()?;
    match status.as_deref() {
        None => {}
        Some("pending") => {
            connection.execute("UPDATE deployment_steps SET status='running',intent_id=?4,started_at_ms=?5 WHERE deployment_id=?1 AND component=?2 AND name=?3",params![deployment.to_string(),component.as_str(),stage,intent.0,timestamp(at)?])?;
        }
        Some(_) => {
            return Err(HistoryError::InvalidMetadata(
                "planned step already started",
            ));
        }
    }
    Ok(())
}

fn validate_step_state(
    status: StepStatus,
    intent: Option<i64>,
    start: Option<i64>,
    end: Option<i64>,
    error: Option<&str>,
) -> Result<(), HistoryError> {
    let started = intent.is_some_and(|id| id > 0) && start.is_some_and(|time| time >= 0);
    let completed = start.zip(end).is_some_and(|(start, end)| end >= start);
    let valid = match status {
        StepStatus::Pending => {
            intent.is_none() && start.is_none() && end.is_none() && error.is_none()
        }
        StepStatus::Skipped => {
            intent.is_none()
                && start.is_none()
                && end.is_some_and(|time| time >= 0)
                && error.is_none()
        }
        StepStatus::Running => started && end.is_none() && error.is_none(),
        StepStatus::Succeeded => started && completed && error.is_none(),
        StepStatus::Failed => started && completed && error.is_some(),
    };
    if valid {
        Ok(())
    } else {
        Err(corrupt(&"inconsistent step status and timestamps"))
    }
}

pub(super) fn validate_selected_component(
    connection: &Connection,
    deployment: &DeploymentId,
    component: &ComponentName,
) -> Result<(), HistoryError> {
    let frozen: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployment_snapshots WHERE deployment_id=?1)",
        [deployment.to_string()],
        |row| row.get(0),
    )?;
    if frozen {
        load_snapshot(connection, deployment, component)?;
    }
    Ok(())
}

pub(super) fn validate_component_result(
    connection: &Connection,
    deployment: &DeploymentId,
    component: &ComponentName,
    result: &ComponentDeploymentResult,
) -> Result<(), HistoryError> {
    let frozen: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployment_snapshots WHERE deployment_id=?1)",
        [deployment.to_string()],
        |row| row.get(0),
    )?;
    if frozen {
        let snapshot = load_snapshot(connection, deployment, component)?;
        if result.attempted_release.as_ref()
            != snapshot.target.as_ref().map(|target| &target.version)
        {
            return Err(HistoryError::InvalidMetadata(
                "Component result attempted Release differs from frozen target",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_step_links(
    connection: &Connection,
    deployment: Option<&DeploymentId>,
    intent: Option<IntentId>,
) -> Result<(), HistoryError> {
    let invalid:bool=connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployment_steps s LEFT JOIN operation_intents i ON s.intent_id=i.id
         WHERE s.intent_id IS NOT NULL AND (?1 IS NULL OR s.deployment_id=?1) AND (?2 IS NULL OR s.intent_id=?2)
         AND (i.id IS NULL OR s.deployment_id!=i.deployment_id OR s.component!=i.component OR s.name!=i.stage
         OR s.status!=CASE i.status WHEN 'pending' THEN 'running' ELSE i.status END
         OR s.started_at_ms IS NOT i.created_at_ms OR s.completed_at_ms IS NOT i.completed_at_ms OR s.error IS NOT i.error))",
        params![deployment.map(ToString::to_string),intent.map(|intent|intent.0)],|row|row.get(0),
    )?;
    if invalid {
        Err(corrupt(&"planned Step and linked intent disagree"))
    } else {
        Ok(())
    }
}

fn require_unstarted(
    connection: &Connection,
    deployment: &DeploymentId,
) -> Result<DeploymentRecord, HistoryError> {
    let record = load_deployment(connection, deployment)?
        .ok_or_else(|| HistoryError::InvalidMetadata("missing Deployment"))?;
    let started: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM operation_intents WHERE deployment_id=?1)",
        [deployment.to_string()],
        |row| row.get(0),
    )?;
    if started
        || !matches!(
            record.state,
            DeploymentState::Created | DeploymentState::Running
        )
    {
        return Err(HistoryError::InvalidMetadata(
            "Deployment has already started or completed",
        ));
    }
    Ok(record)
}

fn load_snapshot(
    connection: &Connection,
    deployment: &DeploymentId,
    component: &ComponentName,
) -> Result<DeploymentComponentSnapshot, HistoryError> {
    let json: Option<String> = connection
        .query_row(
            "SELECT snapshot FROM component_snapshots WHERE deployment_id=?1 AND component=?2",
            params![deployment.to_string(), component.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let snapshot: DeploymentComponentSnapshot = decode(&json.ok_or(
        HistoryError::InvalidMetadata("Component is not in frozen selection"),
    )?)?;
    if snapshot.release.component != *component {
        return Err(corrupt(&"Component snapshot index mismatch"));
    }
    Ok(snapshot)
}

fn validate_snapshots(
    record: &DeploymentRecord,
    snapshots: &[DeploymentComponentSnapshot],
) -> Result<(), HistoryError> {
    if snapshots.is_empty() || snapshots.len() > 256 {
        return Err(HistoryError::InvalidMetadata(
            "a Deployment requires 1-256 Components",
        ));
    }
    let mut components = BTreeSet::new();
    let mut orders = BTreeSet::new();
    for snapshot in snapshots {
        if let Some(target) = &snapshot.target_snapshot {
            let encoded = serde_json::to_string(target)
                .map_err(|_| HistoryError::InvalidMetadata("invalid target snapshot"))?;
            if encoded.len() > 64 * 1024
                || crate::telemetry::detect_sensitive_config(&encoded).is_err()
            {
                return Err(HistoryError::InvalidMetadata(
                    "unsafe or oversized target snapshot",
                ));
            }
        }
        validate_ref(&snapshot.release)?;
        if snapshot.release.project_id != record.project
            || snapshot.release.environment_id != record.environment
        {
            return Err(HistoryError::InvalidMetadata(
                "snapshot belongs to another Project or Environment",
            ));
        }
        if !components.insert(snapshot.release.component.clone())
            || !orders.insert(snapshot.execution_order)
        {
            return Err(HistoryError::InvalidMetadata(
                "duplicate Component or execution order",
            ));
        }
        if snapshot
            .target
            .as_ref()
            .or(snapshot.expected_current.as_ref())
            != Some(&snapshot.release)
            || (record.kind == DeploymentKind::Deploy && snapshot.target.is_none())
        {
            return Err(HistoryError::InvalidMetadata(
                "snapshot identity must match target or rollback current",
            ));
        }
        for reference in [&snapshot.expected_current, &snapshot.target]
            .into_iter()
            .flatten()
        {
            validate_same_scope(&snapshot.release, reference)?;
        }
    }
    if orders.iter().copied().ne(0..u32::try_from(snapshots.len())
        .map_err(|_| HistoryError::InvalidMetadata("too many Components"))?)
    {
        return Err(HistoryError::InvalidMetadata(
            "execution order must be contiguous and zero-based",
        ));
    }
    Ok(())
}

fn validate_ref(release: &ReleaseRef) -> Result<(), HistoryError> {
    crate::drivers::DriverKind::parse(release.driver.as_str())
        .map_err(|_| HistoryError::InvalidMetadata("invalid Driver kind"))?;
    Ok(())
}

fn validate_same_scope(expected: &ReleaseRef, observed: &ReleaseRef) -> Result<(), HistoryError> {
    validate_ref(observed)?;
    if expected.driver != observed.driver
        || expected.project_id != observed.project_id
        || expected.environment_id != observed.environment_id
        || expected.component != observed.component
        || expected.generation != observed.generation
        || expected.destination != observed.destination
        || expected.destination_revision != observed.destination_revision
        || expected.endpoint_fingerprint != observed.endpoint_fingerprint
    {
        return Err(HistoryError::InvalidMetadata(
            "Release reference is outside frozen Component context",
        ));
    }
    Ok(())
}

fn validate_revision(value: Option<&str>) -> Result<(), HistoryError> {
    if value.is_some_and(|value| {
        !(7..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(HistoryError::InvalidMetadata(
            "Git revision must be 7-64 hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_package(
    release: &ReleaseRef,
    manifest: &ReleaseManifest,
    sha256: &str,
    size: u64,
) -> Result<(), HistoryError> {
    validate_ref(release)?;
    validate_revision(manifest.source_revision.as_deref())?;
    if manifest.schema_version != 1
        || manifest.project_id != release.project_id
        || manifest.environment_id != release.environment_id
        || manifest.component != release.component
        || manifest.generation != release.generation
        || manifest.version != release.version
    {
        return Err(HistoryError::InvalidMetadata(
            "manifest differs from Release identity",
        ));
    }
    if size == 0
        || size > i64::MAX as u64
        || sha256.len() != 64
        || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(HistoryError::InvalidMetadata(
            "invalid package size or SHA-256 digest",
        ));
    }
    Ok(())
}

fn encode(value: &impl Serialize) -> Result<String, HistoryError> {
    serde_json::to_string(value)
        .map_err(|_| HistoryError::InvalidMetadata("metadata serialization failed"))
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T, HistoryError> {
    serde_json::from_str(value).map_err(|_| corrupt(&"invalid structured history metadata"))
}

fn load_deployment(
    connection: &Connection,
    deployment: &DeploymentId,
) -> Result<Option<DeploymentRecord>, HistoryError> {
    let row=connection.query_row("SELECT project_id,environment_id,state,kind,related_deployment_id,created_at_ms,updated_at_ms,(SELECT COUNT(*) FROM operation_intents WHERE deployment_id=deployments.id AND status='pending') FROM deployments WHERE id=?1",[deployment.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,i64>(5)?,row.get::<_,i64>(6)?,row.get::<_,i64>(7)?))).optional()?;
    row.map(
        |(project, environment, state, kind, related, created, updated, pending)| {
            let state = match state.as_str() {
                "created" => DeploymentState::Created,
                "running" => DeploymentState::Running,
                "succeeded" => DeploymentState::Succeeded,
                "failed" => DeploymentState::Failed,
                "cancelled" => DeploymentState::Cancelled,
                _ => return Err(corrupt(&"invalid Deployment state")),
            };
            let kind = match kind.as_str() {
                "deploy" => DeploymentKind::Deploy,
                "rollback" => DeploymentKind::Rollback,
                _ => return Err(corrupt(&"invalid Deployment kind")),
            };
            if updated < created
                || created < 0
                || (kind == DeploymentKind::Deploy && related.is_some())
                || (kind == DeploymentKind::Rollback && related.is_none())
            {
                return Err(corrupt(&"invalid Deployment context or timestamps"));
            }
            Ok(DeploymentRecord {
                deployment: deployment.clone(),
                project: ProjectId::from_str(&project).map_err(|error| corrupt(&error))?,
                environment: EnvironmentId::from_str(&environment)
                    .map_err(|error| corrupt(&error))?,
                state,
                kind,
                related_deployment: related
                    .map(|value| DeploymentId::from_str(&value))
                    .transpose()
                    .map_err(|error| corrupt(&error))?,
                created_at_ms: u64::try_from(created)
                    .map_err(|_| corrupt(&"invalid creation time"))?,
                updated_at_ms: u64::try_from(updated)
                    .map_err(|_| corrupt(&"invalid update time"))?,
                pending_intent_count: u64::try_from(pending)
                    .map_err(|_| corrupt(&"invalid pending intent count"))?,
            })
        },
    )
    .transpose()
}

#[cfg(test)]
mod tests;
