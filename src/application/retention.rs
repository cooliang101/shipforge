//! Automatic, best-effort retention after all selected activations succeeded.
//! Cleanup is never compensation and cannot turn a healthy deployment into a failure.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use crate::{
    domain::{Capability, ComponentName, DeploymentId, ReleaseVersion},
    drivers::{
        CleanupCandidate, CleanupReport, ComponentInventory, DriverLog, EventSink, ReleaseRef,
        RetentionPolicy, inventory::InventoryRelease,
    },
    history::{HistoryStore, InspectionScope, IntentStatus, RetentionHistory},
    telemetry::Redactor,
};
use tokio_util::sync::CancellationToken;

use super::{
    DeploymentComponent,
    clock::MonotonicClock,
    orchestrator::planned_release_ref,
    step_events::{StepEvents, persistence, step_state},
};
use crate::telemetry::log_record::{LogPersistence, LogStepState};

pub const DEFAULT_RETAIN_COUNT: usize = 5;
const MAX_CANDIDATES: usize = 16;
const MAX_WARNINGS: usize = 32;
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(180);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(180);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);

/// A pure, bounded selection. Unknown evidence never becomes deletion authority.
fn plan(
    scope: &InspectionScope,
    inventory: &ComponentInventory,
    history: &RetentionHistory,
) -> Result<Vec<RetentionPolicy>, &'static str> {
    if inventory.releases.releases.len() > 1024
        || inventory.audit.records.len() > 128
        || inventory.releases.issues.iter().any(|issue| {
            !inventory.releases.releases.iter().any(|entry| {
                !entry.extracted && issue.version.as_ref() == Some(&entry.manifest.version)
            })
        })
        || inventory.remnants.incomplete
        || !inventory.remnants.entries.is_empty()
    {
        return Err("inventory or temporary work is incomplete; retain all versions");
    }
    let current = inventory
        .releases
        .current
        .as_ref()
        .map_err(|_| "current is unknown; retain all versions")?;
    let mut releases = BTreeMap::new();
    for entry in &inventory.releases.releases {
        if !matches_manifest(scope, entry)
            || releases
                .insert(entry.manifest.version.clone(), entry)
                .is_some()
        {
            return Err("inventory identity is inconsistent; retain all versions");
        }
    }
    if current
        .as_ref()
        .is_some_and(|value| !releases.contains_key(value))
    {
        return Err("current has no verified archive; retain all versions");
    }
    let mut protected = history.protected_versions.clone();
    protected.extend(current.iter().cloned());
    protected.extend(previous_healthy(scope, current.as_ref(), history));

    let attributed = attribute_packages(scope, &releases, history, &mut protected)?;

    // Auxiliary audit can only add protection or veto a conflict. Missing audit
    // is not used to claim that no references or healthy versions ever existed.
    for record in &inventory.audit.records {
        if !record.is_valid() || !same_target(scope, &record.release) {
            return Err("remote audit conflicts with this Component; retain all versions");
        }
        if let Some(package) = &record.package
            && let Some(entry) = releases.get(&record.release.version)
            && (entry.manifest != package.manifest
                || entry.sha256 != package.sha256
                || entry.size != package.size)
        {
            return Err("remote package evidence conflicts; retain all versions");
        }
        if record.healthy == Some(true) {
            // Protect the latest recorded healthy version only through authoritative
            // local insertion order; unrepresented remote health remains conservative.
            let represented = history.healthy.iter().any(|observation| {
                observation
                    .observed
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .is_some_and(|release| release == &record.release)
            });
            if !represented {
                protected.insert(record.release.version.clone());
            }
        }
    }
    let mut newest = releases.values().copied().collect::<Vec<_>>();
    newest.sort_by(|left, right| {
        right
            .manifest
            .created_at_unix
            .cmp(&left.manifest.created_at_unix)
            .then_with(|| right.manifest.version.cmp(&left.manifest.version))
    });
    protected.extend(
        newest
            .iter()
            .take(DEFAULT_RETAIN_COUNT)
            .map(|entry| entry.manifest.version.clone()),
    );
    Ok(newest
        .into_iter()
        .rev()
        .filter(|entry| !protected.contains(&entry.manifest.version))
        .filter_map(|entry| {
            attributed
                .get(&entry.manifest.version)
                .map(|release| RetentionPolicy {
                    protected_versions: protected.clone(),
                    retain_count: DEFAULT_RETAIN_COUNT,
                    candidate: CleanupCandidate {
                        release: release.clone(),
                        package: entry.clone(),
                        expected_current: current.clone(),
                    },
                })
        })
        .take(MAX_CANDIDATES)
        .collect())
}

fn previous_healthy(
    scope: &InspectionScope,
    current: Option<&ReleaseVersion>,
    history: &RetentionHistory,
) -> Option<ReleaseVersion> {
    history.healthy.iter().find_map(|observation| {
        if observation.healthy != Some(true) {
            return None;
        }
        observation
            .observed
            .as_ref()
            .ok()?
            .as_ref()
            .filter(|release| {
                same_target(scope, release)
                    && release.endpoint_fingerprint == scope.endpoint_fingerprint
                    && Some(&release.version) != current
            })
            .map(|release| release.version.clone())
    })
}

fn attribute_packages(
    scope: &InspectionScope,
    releases: &BTreeMap<ReleaseVersion, &InventoryRelease>,
    history: &RetentionHistory,
    protected: &mut BTreeSet<ReleaseVersion>,
) -> Result<BTreeMap<ReleaseVersion, ReleaseRef>, &'static str> {
    // Local packages establish original endpoint/revision and digest. A rebuilt
    // inventory cache does not supply historical deletion authority.
    let mut attributed = BTreeMap::new();
    for (version, entry) in releases {
        let matches = history
            .packages
            .iter()
            .filter(|package| {
                same_target(scope, &package.release) && &package.release.version == version
            })
            .collect::<Vec<_>>();
        if matches.is_empty()
            || matches.iter().any(|package| {
                package.manifest != entry.manifest
                    || package.sha256 != entry.sha256
                    || package.size != entry.size
            })
        {
            return Err("archive history is missing or conflicting; retain all versions");
        }
        if let Some(package) = matches
            .iter()
            .find(|package| exact_target(scope, &package.release))
        {
            attributed.insert(version.clone(), package.release.clone());
        } else {
            // A different historical connection must not be rewritten to today's revision.
            protected.insert(version.clone());
        }
    }
    Ok(attributed)
}

fn same_target(scope: &InspectionScope, release: &ReleaseRef) -> bool {
    release.project_id == scope.project
        && release.environment_id == scope.environment
        && release.component == scope.component
        && release.generation == scope.generation
        && release.driver == scope.driver
        && release.destination == scope.destination
}

fn exact_target(scope: &InspectionScope, release: &ReleaseRef) -> bool {
    same_target(scope, release)
        && release.destination_revision == scope.destination_revision
        && release.endpoint_fingerprint == scope.endpoint_fingerprint
}

fn matches_manifest(scope: &InspectionScope, package: &InventoryRelease) -> bool {
    let manifest = &package.manifest;
    manifest.schema_version == 1
        && manifest.project_id == scope.project
        && manifest.environment_id == scope.environment
        && manifest.component == scope.component
        && manifest.generation == scope.generation
        && package.size > 0
        && package.sha256.len() == 64
        && package
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) struct RetentionRun<'a> {
    pub history: &'a HistoryStore,
    pub clock: &'a MonotonicClock,
    pub redactor: &'a Redactor,
    pub events: &'a dyn EventSink,
    pub cancellation: &'a CancellationToken,
}

impl RetentionRun<'_> {
    pub async fn run(
        &self,
        deployment: &DeploymentId,
        components: &BTreeMap<ComponentName, DeploymentComponent>,
    ) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;
        let mut warnings = Vec::new();
        for (name, component) in components {
            if !component
                .planned
                .plan
                .effective_capabilities
                .contains(Capability::Retention)
            {
                continue;
            }
            if self.cancellation.is_cancelled() || tokio::time::Instant::now() >= deadline {
                warnings
                    .push("Version cleanup stopped; successful deployment remains active.".into());
                break;
            }
            if warnings.len() >= MAX_WARNINGS - 1 {
                warnings.push("Further cleanup deferred after the diagnostic limit.".into());
                break;
            }
            if let Err(warning) = self.component(deployment, component, deadline).await {
                let message = self.redactor.redact(&format!(
                    "{name}: {warning}; successful deployment remains active"
                ));
                self.events.emit(DriverLog {
                    namespace: "retention.warning".into(),
                    message: message.clone(),
                });
                warnings.push(message);
                // Persistence failure or unknown remote progress must not permit
                // deletion to continue in another Component under stale evidence.
                break;
            }
        }
        warnings
    }

    async fn component(
        &self,
        deployment: &DeploymentId,
        component: &DeploymentComponent,
        deadline: tokio::time::Instant,
    ) -> Result<(), String> {
        let scope = InspectionScope::from(&planned_release_ref(&component.planned));
        let before = self
            .history
            .retention_history(&scope)
            .map_err(|_| "local protection history unavailable")?;
        // No need for a network inventory while this known scope has too few
        // distinct versions to produce an eligible candidate.
        let versions = before
            .packages
            .iter()
            .map(|entry| &entry.release.version)
            .collect::<std::collections::BTreeSet<_>>();
        if versions.len() <= DEFAULT_RETAIN_COUNT {
            return Ok(());
        }
        let mut context = component.planned.context.clone();
        context.cancellation = self.cancellation.child_token();
        let _cancellation_guard = context.cancellation.clone().drop_guard();
        let inventory = tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + INVENTORY_TIMEOUT),
            component.planned.driver.inventory(&context),
        )
        .await
        .map_err(|_| "inventory timed out; cleanup deferred")?
        .map_err(|_| "inventory unavailable; cleanup deferred")?;
        let policies = plan(&scope, &inventory, &before).map_err(str::to_owned)?;
        for policy in policies {
            if self.cancellation.is_cancelled() || tokio::time::Instant::now() >= deadline {
                context.cancellation.cancel();
                return Err("cleanup cancelled or timed out before the next version".into());
            }
            if deadline.saturating_duration_since(tokio::time::Instant::now()) < CLEANUP_TIMEOUT {
                return Err(
                    "cleanup deferred: insufficient time for another safe deletion boundary".into(),
                );
            }
            let fresh = self
                .history
                .retention_history(&scope)
                .map_err(|_| "local protection history unavailable")?;
            if fresh != before {
                return Err("protection history changed; recheck before cleanup".into());
            }
            self.delete_candidate(deployment, component, &context, &policy, deadline)
                .await?;
        }
        context.cancellation.cancel();
        Ok(())
    }

    fn begin_candidate(
        &self,
        deployment: &DeploymentId,
        context: &crate::drivers::ComponentExecutionContext,
        policy: &RetentionPolicy,
        deadline: tokio::time::Instant,
    ) -> Result<(crate::history::IntentId, StepEvents<'_>), String> {
        let version = &policy.candidate.release.version;
        let intent = self
            .history
            .record_intent(
                deployment,
                &context.component,
                &format!("cleanup.{version}"),
                version.as_str(),
                self.clock
                    .timestamp()
                    .map_err(|_| "cleanup clock unavailable")?,
            )
            .map_err(|_| "cleanup intent could not be persisted; no deletion started")?;
        let step_events = StepEvents::start(
            self.events,
            &context.component,
            &format!("cleanup.{version}"),
        );
        // Database contention can consume the earlier budget check. Recheck after
        // journaling; a known non-start is not an uncertain remote side effect.
        let not_started = if self.cancellation.is_cancelled() || context.cancellation.is_cancelled()
        {
            Some("cleanup not started: cancellation requested")
        } else if deadline.saturating_duration_since(tokio::time::Instant::now()) < CLEANUP_TIMEOUT
        {
            Some("cleanup not started: insufficient time for a safe deletion boundary")
        } else {
            None
        };
        if let Some(diagnostic) = not_started {
            let completed = self
                .clock
                .timestamp()
                .map_err(|_| {
                    "cleanup not started but result time unavailable; intent remains pending"
                        .to_owned()
                })
                .and_then(|timestamp| {
                    self.history
                        .complete_intent(
                            intent,
                            IntentStatus::Failed,
                            Some(diagnostic),
                            timestamp,
                            self.redactor,
                        )
                        .map_err(|_| {
                            format!(
                                "{diagnostic}; outcome could not be saved; intent remains pending"
                            )
                        })
                });
            step_events.finish(LogStepState::Skipped, persistence(completed.is_ok()));
            completed?;
            return Err(diagnostic.into());
        }
        Ok((intent, step_events))
    }

    async fn delete_candidate(
        &self,
        deployment: &DeploymentId,
        component: &DeploymentComponent,
        context: &crate::drivers::ComponentExecutionContext,
        policy: &RetentionPolicy,
        deadline: tokio::time::Instant,
    ) -> Result<(), String> {
        let (intent, step_events) = self.begin_candidate(deployment, context, policy, deadline)?;
        let version = &policy.candidate.release.version;
        step_events.emit(DriverLog {
            namespace: "retention.started".into(),
            message: format!(
                "Removing expired version {version} for {}",
                context.component
            ),
        });
        // Guarded Drivers revalidate the saved YAML/registry before mutation.
        let outcome = tokio::time::timeout(
            CLEANUP_TIMEOUT,
            component
                .planned
                .driver
                .cleanup_with_events(context, policy, &step_events),
        )
        .await;
        let (result, resolved) = match outcome {
            Ok(Ok(report)) => (
                cleanup_outcome(version, &report),
                cleanup_resolved(version, &report),
            ),
            Ok(Err(_)) => (
                Err(format!(
                    "cleanup of {version} was not confirmed; inspect archive and directory before retrying"
                )),
                false,
            ),
            Err(_) => {
                context.cancellation.cancel();
                (
                    Err(format!(
                        "cleanup of {version} timed out; archive and directory state unknown"
                    )),
                    false,
                )
            }
        };
        if !resolved {
            // An auxiliary error observation does not complete the durable intent.
            step_events.finish(LogStepState::Unknown, LogPersistence::Unconfirmed);
            let diagnostic = result
                .as_ref()
                .err()
                .map_or("cleanup outcome unknown", String::as_str);
            self.history
                .record_observation(
                    deployment,
                    &context.component,
                    &format!("cleanup.{version}"),
                    Err(diagnostic),
                    None,
                    self.clock
                        .timestamp()
                        .map_err(|_| "cleanup result time unavailable; intent remains pending")?,
                    self.redactor,
                )
                .map_err(|_| {
                    format!("{diagnostic}; diagnostic could not be saved; intent remains pending")
                })?;
            return Err(format!(
                "{diagnostic}; intent remains pending for inspection"
            ));
        }
        let (status, diagnostic) = match &result {
            Ok(()) => (IntentStatus::Succeeded, None),
            Err(error) => (IntentStatus::Failed, Some(error.as_str())),
        };
        let completed = self.clock.timestamp()
            .map_err(|_| "cleanup completed but result time could not be recorded".to_owned())
            .and_then(|timestamp| self.history.complete_intent(intent, status, diagnostic, timestamp, self.redactor)
                .map_err(|_| format!("cleanup of {version} returned {result:?}, but its local outcome could not be saved; stop and inspect")));
        step_events.finish(step_state(result.is_ok()), persistence(completed.is_ok()));
        completed?;
        result?;
        step_events.emit(DriverLog {
            namespace: "retention.finished".into(),
            message: format!(
                "Removed expired version {version} for {}",
                context.component
            ),
        });
        Ok(())
    }
}

fn cleanup_resolved(version: &ReleaseVersion, report: &CleanupReport) -> bool {
    use crate::drivers::CleanupPathState;
    (report.removed == [version.clone()]
        && !report.retained.contains(version)
        && report.partial.is_empty())
        || (report.removed.is_empty()
            && report.partial.len() == 1
            && report.partial[0].version == *version
            && !report.retained.contains(version)
            && report.partial[0].archive != CleanupPathState::Unknown
            && report.partial[0].directory != CleanupPathState::Unknown)
}

fn cleanup_outcome(version: &ReleaseVersion, report: &CleanupReport) -> Result<(), String> {
    if report.removed == [version.clone()]
        && !report.retained.contains(version)
        && report.partial.is_empty()
    {
        return if report.warnings.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "cleanup of {version} removed both archive and directory, but returned a warning; inspect before continuing"
            ))
        };
    }
    let partial = report
        .partial
        .iter()
        .find(|entry| &entry.version == version);
    match partial {
        Some(entry) => Err(format!(
            "cleanup of {version} incomplete: archive={:?}, directory={:?}; inspect before retrying",
            entry.archive, entry.directory
        )),
        None => Err(format!(
            "cleanup of {version} not confirmed; retained or unknown, inspect before retrying"
        )),
    }
}

#[cfg(test)]
mod tests;
