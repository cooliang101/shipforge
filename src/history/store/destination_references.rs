//! Bounded, conservative references used only to authorize local connection removal.

use rusqlite::types::ValueRef;
use sha2::{Digest, Sha256};

use super::{DeploymentId, HistoryError, HistoryStore};
use crate::domain::DestinationKey;

const TABLES: &[&str] = &[
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
    "deployment_revisions",
    "recovery_reports",
    "recovery_report_components",
];
const MAX_ROWS: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DestinationReferenceSummary {
    pub deployments: usize,
    pub releases: usize,
    pub recovery_reports: usize,
    /// Digest of all examined local evidence, not a remote state assertion.
    pub evidence_fingerprint: String,
}

impl HistoryStore {
    /// Checks every historical scope before authorizing removal of a Destination.
    /// Old revisions and generations still reference the same connection key.
    /// Reads are bounded to 4096 rows per table and 8 MiB in aggregate.
    ///
    /// # Errors
    /// Fails closed for legacy missing snapshots, invalid evidence, excessive data,
    /// broken references, or database errors. Does not migrate or create a database.
    pub fn destination_references(
        &self,
        key: &DestinationKey,
    ) -> Result<DestinationReferenceSummary, HistoryError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut references = DestinationReferenceSummary {
            evidence_fingerprint: evidence_fingerprint(&transaction)?,
            ..DestinationReferenceSummary::default()
        };
        if transaction
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_some()
        {
            return Err(invalid("history contains broken references"));
        }
        let mut statement = transaction.prepare("SELECT id FROM deployments ORDER BY id")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for id in ids {
            let id: DeploymentId = id
                .parse()
                .map_err(|_| invalid("invalid Deployment identity"))?;
            let snapshots = self.component_snapshots(&id)?;
            if snapshots.is_empty() {
                return Err(invalid(
                    "Deployment has no trustworthy Destination snapshot",
                ));
            }
            if snapshots
                .iter()
                .any(|snapshot| &snapshot.release.destination == key)
            {
                references.deployments += 1;
            }
            self.deployment_metadata(&id)?;
            self.component_results(&id)?;
            for step in self.steps(&id)? {
                if !snapshots
                    .iter()
                    .any(|snapshot| snapshot.release.component == step.component)
                {
                    return Err(invalid(
                        "historical intent has no trustworthy Component scope",
                    ));
                }
            }
            self.deployment_log(&id)?;
            for package in self.release_packages(&id)? {
                references.releases += usize::from(&package.release.destination == key);
            }
            for receipt in self.release_receipts(&id)? {
                references.releases += usize::from(&receipt.release.destination == key);
            }
            for observation in self.observations(&id)? {
                if let Ok(Some(release)) = observation.observed {
                    references.releases += usize::from(&release.destination == key);
                }
            }
        }
        let mut statement = transaction.prepare("SELECT id FROM recovery_reports ORDER BY id")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for id in ids {
            let id = id
                .parse()
                .map_err(|_| invalid("invalid Recovery report identity"))?;
            let report = self
                .recovery_report(&id)?
                .ok_or_else(|| invalid("missing Recovery report"))?;
            if report.components.iter().any(|component| {
                &component.scope.destination == key
                    || component.inventory.as_ref().is_ok_and(|inventory| {
                        inventory
                            .audit
                            .records
                            .iter()
                            .any(|record| &record.release.destination == key)
                    })
            }) {
                references.recovery_reports += 1;
            }
        }
        transaction.commit()?;
        Ok(references)
    }
}

fn evidence_fingerprint(connection: &rusqlite::Connection) -> Result<String, HistoryError> {
    check_budget(connection)?;
    let mut digest = Sha256::new();
    let mut bytes = 0_usize;
    for table in TABLES {
        digest.update(table.as_bytes());
        let mut statement =
            connection.prepare(&format!("SELECT * FROM {table} ORDER BY rowid LIMIT 4097"))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            if count > MAX_ROWS {
                return Err(invalid(
                    "Destination reference scan exceeds bounded row limits",
                ));
            }
            digest.update([0xff]);
            for column in 0..columns {
                let value = row.get_ref(column)?;
                let size = match value {
                    ValueRef::Text(value) | ValueRef::Blob(value) => value.len(),
                    _ => 8,
                };
                bytes = bytes.saturating_add(size);
                if bytes > MAX_BYTES {
                    return Err(invalid(
                        "Destination reference scan exceeds bounded byte limits",
                    ));
                }
                digest.update(size.to_le_bytes());
                match value {
                    ValueRef::Null => digest.update([0]),
                    ValueRef::Integer(value) => {
                        digest.update([1]);
                        digest.update(value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        digest.update([2]);
                        digest.update(value.to_le_bytes());
                    }
                    ValueRef::Text(value) => {
                        digest.update([3]);
                        digest.update(value);
                    }
                    ValueRef::Blob(value) => {
                        digest.update([4]);
                        digest.update(value);
                    }
                }
            }
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn check_budget(connection: &rusqlite::Connection) -> Result<(), HistoryError> {
    let mut bytes = 0_u64;
    for table in TABLES {
        let schema = connection.prepare(&format!("SELECT * FROM {table} LIMIT 0"))?;
        let lengths = schema
            .column_names()
            .into_iter()
            .map(|column| {
                format!(
                    "COALESCE(length(CAST(\"{}\" AS BLOB)),0)",
                    column.replace('"', "\"\"")
                )
            })
            .collect::<Vec<_>>()
            .join("+");
        let mut statement =
            connection.prepare(&format!("SELECT {lengths} FROM {table} LIMIT 4097"))?;
        let mut rows = statement.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            count += 1;
            bytes = bytes.saturating_add(
                u64::try_from(row.get::<_, i64>(0)?)
                    .map_err(|_| invalid("invalid Destination reference byte size"))?,
            );
            if count > MAX_ROWS {
                return Err(invalid(
                    "Destination reference scan exceeds bounded row limits",
                ));
            }
            if bytes > MAX_BYTES as u64 {
                return Err(invalid(
                    "Destination reference scan exceeds bounded byte limits",
                ));
            }
        }
    }
    Ok(())
}

fn invalid(message: &'static str) -> HistoryError {
    HistoryError::InvalidMetadata(message)
}

#[cfg(test)]
mod tests;
