use std::{
    any::Any,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::*;
mod events;
use crate::{
    application::{DeploymentOrchestrator, PlannedComponent},
    domain::{
        ComponentGeneration, ComponentRelease, DeploymentState, DestinationKey,
        DestinationRevision, DriverCapabilities, EnvironmentId, ProjectId, ReleaseManifest,
    },
    drivers::{
        ActivationReceipt, CleanupPartial, CleanupPathState, ComponentExecutionContext,
        ComponentPlan, ComponentRequest, CredentialHandle, DeploymentDriver,
        DriverDestinationInput, DriverError, DriverKind, DriverTargetInput, EndpointFingerprint,
        PreflightReport, PreparedRelease, ReleasePackage, RemoteAuditHistory,
        ValidatedDestinationSettings, ValidatedTargetSettings,
        inventory::{InventoryIssue, ReleaseInventory, TemporaryRemnants},
    },
    history::{DeploymentComponentSnapshot, ObservationRecord, ReleasePackageRecord},
};

fn version(number: u64) -> ReleaseVersion {
    ReleaseVersion::parse(format!("v{number}")).unwrap()
}

fn reference() -> ReleaseRef {
    ReleaseRef {
        driver: DriverKind::parse("retention-test").unwrap(),
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("api").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: version(8),
        destination: DestinationKey::new(),
        destination_revision: DestinationRevision::INITIAL,
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        effective_capabilities: DriverCapabilities::new([
            Capability::StagedDeployment,
            Capability::ExplicitActivation,
            Capability::Rollback,
            Capability::Inventory,
            Capability::Retention,
        ]),
    }
}

fn component_release(reference: &ReleaseRef) -> ComponentRelease {
    ComponentRelease {
        project_id: reference.project_id.clone(),
        environment_id: reference.environment_id.clone(),
        component: reference.component.clone(),
        generation: reference.generation,
        version: reference.version.clone(),
        destination: reference.destination.clone(),
        destination_revision: reference.destination_revision,
    }
}

fn facts(reference: &ReleaseRef) -> (ComponentInventory, RetentionHistory) {
    let mut history = RetentionHistory::default();
    let mut releases = Vec::new();
    for index in 1..=8 {
        let mut release = reference.clone();
        release.version = version(index);
        let manifest = ReleaseManifest::new(&component_release(&release), index, None);
        let package = InventoryRelease {
            manifest: manifest.clone(),
            sha256: "b".repeat(64),
            size: 100,
            extracted: true,
        };
        history.packages.push(ReleasePackageRecord {
            release: release.clone(),
            manifest,
            sha256: package.sha256.clone(),
            size: package.size,
        });
        releases.push(package);
        if index == 2 || index == 8 {
            history.healthy.insert(
                0,
                ObservationRecord {
                    sequence: i64::try_from(index).unwrap(),
                    component: release.component.clone(),
                    stage: "activate.receipt".into(),
                    observed: Ok(Some(release)),
                    healthy: Some(true),
                    observed_at_ms: 9 - index,
                },
            );
        }
    }
    (
        ComponentInventory {
            releases: ReleaseInventory {
                releases,
                issues: Vec::new(),
                current: Ok(Some(version(8))),
                notices: Vec::new(),
            },
            audit: RemoteAuditHistory {
                incomplete: true,
                ..RemoteAuditHistory::default()
            },
            remnants: TemporaryRemnants::default(),
        },
        history,
    )
}

fn candidates(policies: &[RetentionPolicy]) -> Vec<ReleaseVersion> {
    policies
        .iter()
        .map(|policy| policy.candidate.release.version.clone())
        .collect()
}

#[test]
fn latest_five_plus_previous_health_and_in_progress_are_protected() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (inventory, mut history) = facts(&release);
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(1), version(3)]
    );
    history.protected_versions.insert(version(1));
    let policies = plan(&scope, &inventory, &history).unwrap();
    assert_eq!(candidates(&policies), [version(3)]);
    assert!(policies[0].protected_versions.contains(&version(2)));
    assert!(policies[0].protected_versions.contains(&version(8)));
}

#[test]
fn health_uses_persisted_order_not_observation_timestamp() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (inventory, mut history) = facts(&release);
    let mut older = history.healthy[1].clone();
    older.observed.as_mut().unwrap().as_mut().unwrap().version = version(1);
    older.observed_at_ms = u64::MAX;
    history.healthy.push(older);
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(1), version(3)]
    );
}

#[test]
fn another_endpoints_newer_health_cannot_displace_this_endpoints_previous_health() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (inventory, mut history) = facts(&release);
    let mut other = history.healthy[1].clone();
    let reference = other.observed.as_mut().unwrap().as_mut().unwrap();
    reference.version = version(1);
    reference.endpoint_fingerprint = EndpointFingerprint::parse("c".repeat(64)).unwrap();
    history.healthy.insert(0, other);
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(1), version(3)]
    );
    history.healthy[2]
        .observed
        .as_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .destination_revision = DestinationRevision::INITIAL.checked_next().unwrap();
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(1), version(3)],
        "old revisions of the same endpoint still protect previous health"
    );
}

#[test]
fn missing_or_conflicting_history_never_authorizes_deletion() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (inventory, mut history) = facts(&release);
    assert!(plan(&scope, &inventory, &RetentionHistory::default()).is_err());
    history.packages.remove(0);
    assert!(plan(&scope, &inventory, &history).is_err());
    let (_, mut history) = facts(&release);
    history.packages[0].sha256 = "c".repeat(64);
    assert!(plan(&scope, &inventory, &history).is_err());
}

#[test]
fn old_revision_is_retained_and_never_rewritten() {
    let release = reference();
    let mut scope = InspectionScope::from(&release);
    let (inventory, history) = facts(&release);
    scope.destination_revision = scope.destination_revision.checked_next().unwrap();
    assert!(plan(&scope, &inventory, &history).unwrap().is_empty());
    scope.destination_revision = release.destination_revision;
    let mut history = history;
    history.packages[0].release.endpoint_fingerprint =
        EndpointFingerprint::parse("c".repeat(64)).unwrap();
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(3)]
    );
}

#[test]
fn current_unknown_inventory_issues_remnants_or_duplicate_versions_veto_cleanup() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    for case in 0..6 {
        let (mut inventory, history) = facts(&release);
        match case {
            0 => inventory.releases.current = Err("unknown".into()),
            1 => inventory.releases.issues.push(InventoryIssue {
                version: Some(version(1)),
                message: "corrupt".into(),
            }),
            2 => inventory.remnants.incomplete = true,
            3 => inventory
                .releases
                .releases
                .push(inventory.releases.releases[0].clone()),
            4 => inventory.releases.current = Ok(Some(version(99))),
            _ => {
                inventory.releases.releases[0].manifest.component =
                    ComponentName::parse("other").unwrap();
            }
        }
        assert!(plan(&scope, &inventory, &history).is_err(), "case {case}");
    }
}

#[test]
fn known_archive_only_candidate_can_be_replanned_after_partial_deletion() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (mut inventory, history) = facts(&release);
    inventory.releases.releases[0].extracted = false;
    inventory.releases.issues.push(InventoryIssue {
        version: Some(version(1)),
        message: "archive only".into(),
    });
    let policies = plan(&scope, &inventory, &history).unwrap();
    assert_eq!(candidates(&policies), [version(1), version(3)]);
    assert!(!policies[0].candidate.package.extracted);
}

#[test]
fn auxiliary_remote_health_only_adds_protection_and_package_conflicts_veto() {
    use crate::drivers::audit::{
        RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPackage, RemoteAuditPhase,
        RemoteAuditRecord,
    };
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (mut inventory, history) = facts(&release);
    let package = &history.packages[0];
    let record = RemoteAuditRecord {
        schema_version: 1,
        event_id: uuid::Uuid::now_v7(),
        deployment: DeploymentId::new(),
        recorded_at_ms: 1,
        release: package.release.clone(),
        phase: RemoteAuditPhase::Activate,
        outcome: RemoteAuditOutcome::Succeeded,
        expected_current: None,
        target: Some(version(1)),
        observed: RemoteAuditObserved::Release(version(1)),
        healthy: Some(true),
        package: None,
    };
    inventory.audit.records.push(record);
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(3)]
    );
    let record = &mut inventory.audit.records[0];
    record.phase = RemoteAuditPhase::Prepare;
    record.healthy = None;
    record.package = Some(RemoteAuditPackage {
        manifest: package.manifest.clone(),
        sha256: "c".repeat(64),
        size: package.size,
    });
    assert!(record.is_valid());
    assert!(plan(&scope, &inventory, &history).is_err());
}

#[test]
fn timestamp_ties_use_version_only_for_retention_not_health() {
    let release = reference();
    let scope = InspectionScope::from(&release);
    let (mut inventory, mut history) = facts(&release);
    for package in &mut inventory.releases.releases {
        package.manifest.created_at_unix = 1;
    }
    for package in &mut history.packages {
        package.manifest.created_at_unix = 1;
    }
    assert_eq!(
        candidates(&plan(&scope, &inventory, &history).unwrap()),
        [version(1), version(3)]
    );
}

#[test]
fn partial_cleanup_preserves_each_known_path() {
    let report = CleanupReport {
        partial: vec![CleanupPartial {
            version: version(1),
            archive: CleanupPathState::Present,
            directory: CleanupPathState::Absent,
        }],
        ..CleanupReport::default()
    };
    let error = cleanup_outcome(&version(1), &report).unwrap_err();
    assert!(error.contains("archive=Present"));
    assert!(error.contains("directory=Absent"));
    assert!(
        cleanup_outcome(
            &version(1),
            &CleanupReport {
                removed: vec![version(9)],
                ..CleanupReport::default()
            }
        )
        .is_err()
    );
}

#[derive(Debug)]
struct Settings(DriverKind);
impl ValidatedTargetSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl ValidatedDestinationSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Partial,
    Unknown,
    IntentWrite,
    ResultWrite,
    ChangedHistory,
}

#[derive(Debug)]
struct Driver {
    release: ReleaseRef,
    inventory: ComponentInventory,
    history: PathBuf,
    calls: Mutex<Vec<ReleaseVersion>>,
    fault: Fault,
}

impl Driver {
    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.history)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
}

#[async_trait]
impl DeploymentDriver for Driver {
    fn kind(&self) -> DriverKind {
        self.release.driver.clone()
    }
    fn static_capabilities(&self) -> DriverCapabilities {
        self.release.effective_capabilities.clone()
    }
    fn validate_target(
        &self,
        _: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        unreachable!()
    }
    fn validate_destination(
        &self,
        _: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        unreachable!()
    }
    async fn preflight(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        unreachable!()
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        _: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        unreachable!()
    }
    async fn current(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        Ok(Some(self.release.clone()))
    }
    async fn inventory(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        if self.fault == Fault::IntentWrite {
            self.sql("CREATE TRIGGER deny_cleanup BEFORE INSERT ON operation_intents WHEN NEW.stage LIKE 'cleanup.%' BEGIN SELECT RAISE(ABORT,'injected'); END;");
        } else if self.fault == Fault::ChangedHistory {
            self.sql("UPDATE deployment_observations SET healthy=0 WHERE healthy=1;");
        }
        Ok(self.inventory.clone())
    }
    async fn prepare(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        Ok(PreparedRelease {
            release: self.release.clone(),
            already_active: false,
        })
    }
    async fn activate(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        Ok(ActivationReceipt {
            current: Some(self.release.clone()),
            healthy: true,
            warnings: Vec::new(),
        })
    }
    async fn rollback(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: Option<&ReleaseRef>,
        _: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("cleanup must not compensate successful activation")
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        unreachable!()
    }
    async fn cleanup(
        &self,
        _: &ComponentExecutionContext,
        policy: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        let version = policy.candidate.release.version.clone();
        let db = rusqlite::Connection::open(&self.history).unwrap();
        let count: i64 = db.query_row("SELECT count(*) FROM operation_intents WHERE stage=?1 AND target=?2 AND status='pending'",
            [format!("cleanup.{version}"),version.to_string()], |row| row.get(0)).unwrap();
        assert_eq!(count, 1, "durable exact intention precedes cleanup");
        self.calls.lock().unwrap().push(version.clone());
        if self.fault == Fault::ResultWrite {
            self.sql("CREATE TRIGGER deny_cleanup_result BEFORE UPDATE ON operation_intents WHEN OLD.stage LIKE 'cleanup.%' BEGIN SELECT RAISE(ABORT,'injected'); END;");
        }
        if matches!(self.fault, Fault::Partial | Fault::Unknown) {
            return Ok(CleanupReport {
                partial: vec![CleanupPartial {
                    version,
                    archive: if self.fault == Fault::Unknown {
                        CleanupPathState::Unknown
                    } else {
                        CleanupPathState::Present
                    },
                    directory: CleanupPathState::Absent,
                }],
                warnings: vec!["permission failure".into()],
                ..CleanupReport::default()
            });
        }
        Ok(CleanupReport {
            removed: vec![version],
            ..CleanupReport::default()
        })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    history: HistoryStore,
    component: DeploymentComponent,
    driver: Arc<Driver>,
}

impl Fixture {
    fn new(fault: Fault) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let history = HistoryStore::open(&path).unwrap();
        let release = reference();
        let (inventory, evidence) = facts(&release);
        for package in &evidence.packages[..7] {
            seed(&history, package);
        }
        let driver = Arc::new(Driver {
            release: release.clone(),
            inventory,
            history: path,
            calls: Mutex::new(Vec::new()),
            fault,
        });
        let settings = Arc::new(Settings(release.driver.clone()));
        let context = ComponentExecutionContext {
            project_id: release.project_id.clone(),
            environment_id: release.environment_id.clone(),
            component: release.component.clone(),
            generation: release.generation,
            destination: release.destination.clone(),
            destination_revision: release.destination_revision,
            credential: CredentialHandle::new(),
            endpoint_fingerprint: release.endpoint_fingerprint.clone(),
            destination_settings: settings.clone(),
            target: settings,
            cancellation: CancellationToken::new(),
        };
        let package = &evidence.packages[7];
        let component = DeploymentComponent {
            planned: PlannedComponent {
                driver: driver.clone(),
                context,
                notices: Vec::new(),
                plan: ComponentPlan {
                    release: component_release(&release),
                    effective_capabilities: release.effective_capabilities.clone(),
                    expected_current: None,
                    driver_steps: Vec::new(),
                },
            },
            package: ReleasePackage::with_manifest(
                component_release(&release),
                package.manifest.clone(),
                PathBuf::from("unused.tar.gz"),
                package.sha256.clone(),
                package.size,
            ),
        };
        Self {
            _directory: directory,
            history,
            component,
            driver,
        }
    }

    async fn deploy(&self) -> crate::application::DeploymentReport {
        DeploymentOrchestrator::new(&self.history, Redactor::default())
            .deploy(
                vec![self.component.clone()],
                std::slice::from_ref(&self.component.planned.context.component),
                &NoEvents,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    }
}

fn seed(history: &HistoryStore, package: &ReleasePackageRecord) {
    let id = DeploymentId::new();
    let release = &package.release;
    history
        .create_deployment(&id, &release.project_id, &release.environment_id, 1)
        .unwrap();
    history
        .record_component_snapshots(
            &id,
            &[DeploymentComponentSnapshot {
                target_snapshot: None,
                release: release.clone(),
                target: Some(release.clone()),
                expected_current: None,
                execution_order: 0,
            }],
        )
        .unwrap();
    history
        .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 2)
        .unwrap();
    history
        .record_release_package(
            &id,
            release,
            &package.manifest,
            &package.sha256,
            package.size,
        )
        .unwrap();
    if release.version == version(2) {
        history
            .record_observation(
                &id,
                &release.component,
                "activate.receipt",
                Ok(Some(release)),
                Some(true),
                3,
                &Redactor::default(),
            )
            .unwrap();
    }
    history
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Succeeded, 4)
        .unwrap();
}

struct NoEvents;
impl EventSink for NoEvents {
    fn emit(&self, _: DriverLog) {}
}

#[tokio::test]
async fn automatic_cleanup_journals_each_exact_candidate_after_success() {
    let fixture = Fixture::new(Fault::None);
    let report = fixture.deploy().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(
        *fixture.driver.calls.lock().unwrap(),
        [version(1), version(3)]
    );
    let steps = fixture.history.steps(&report.deployment.id).unwrap();
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.name.starts_with("cleanup.")
                && step.status == crate::history::StepStatus::Succeeded)
            .count(),
        2
    );
}

#[tokio::test]
async fn cleanup_partial_failure_preserves_success_and_stops_later_versions() {
    let fixture = Fixture::new(Fault::Partial);
    let report = fixture.deploy().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.failure.is_none());
    assert_eq!(*fixture.driver.calls.lock().unwrap(), [version(1)]);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("archive=Present")
                && warning.contains("directory=Absent"))
    );
}

#[tokio::test]
async fn missing_intent_or_changed_history_prevents_any_deletion() {
    for fault in [Fault::IntentWrite, Fault::ChangedHistory] {
        let fixture = Fixture::new(fault);
        let report = fixture.deploy().await;
        assert_eq!(report.deployment.state, DeploymentState::Succeeded);
        assert!(fixture.driver.calls.lock().unwrap().is_empty());
        assert!(!report.warnings.is_empty());
    }
}

#[tokio::test]
async fn result_write_failure_preserves_known_removal_and_pending_intent() {
    let fixture = Fixture::new(Fault::ResultWrite);
    let report = fixture.deploy().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(*fixture.driver.calls.lock().unwrap(), [version(1)]);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("Ok(())") && warning.contains("could not be saved"))
    );
    assert!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .iter()
            .any(|intent| intent.stage == "cleanup.v1")
    );
}

#[tokio::test]
async fn unknown_cleanup_is_discoverable_after_reopening_successful_deployment() {
    let fixture = Fixture::new(Fault::Unknown);
    let report = fixture.deploy().await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.failure.is_none());
    let reopened = HistoryStore::open(&fixture.driver.history).unwrap();
    let intents = reopened.pending_intents(&report.deployment.id).unwrap();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].stage, "cleanup.v1");
    let observations = reopened.observations(&report.deployment.id).unwrap();
    assert!(observations.iter().any(|observation| {
        observation.stage == "cleanup.v1"
            && observation
                .observed
                .as_ref()
                .unwrap_err()
                .contains("archive=Unknown")
    }));
    let attention = HistoryStore::local_attention(&fixture.driver.history, None, None).unwrap();
    assert!(
        attention
            .candidates
            .iter()
            .any(|candidate| candidate.deployment == report.deployment.id)
    );
    assert_eq!(*fixture.driver.calls.lock().unwrap(), [version(1)]);
}

#[tokio::test]
async fn insufficient_total_budget_does_not_start_an_irreversible_path_command() {
    let fixture = Fixture::new(Fault::None);
    let (_, evidence) = facts(&fixture.driver.release);
    seed(&fixture.history, &evidence.packages[7]);
    let clock = MonotonicClock::default();
    let redactor = Redactor::default();
    let cancellation = CancellationToken::new();
    let run = RetentionRun {
        history: &fixture.history,
        clock: &clock,
        redactor: &redactor,
        events: &NoEvents,
        cancellation: &cancellation,
    };
    let result = run
        .component(
            &DeploymentId::new(),
            &fixture.component,
            tokio::time::Instant::now() + Duration::from_secs(179),
        )
        .await;
    assert!(result.unwrap_err().contains("insufficient time"));
    assert!(fixture.driver.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn precancelled_retention_keeps_every_version() {
    let fixture = Fixture::new(Fault::None);
    let clock = MonotonicClock::default();
    let redactor = Redactor::default();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let run = RetentionRun {
        history: &fixture.history,
        clock: &clock,
        redactor: &redactor,
        events: &NoEvents,
        cancellation: &cancellation,
    };
    let warnings = run
        .run(
            &DeploymentId::new(),
            &BTreeMap::from([(
                fixture.component.planned.context.component.clone(),
                fixture.component.clone(),
            )]),
        )
        .await;
    assert!(!warnings.is_empty());
    assert!(fixture.driver.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn post_journal_budget_or_cancellation_records_a_known_non_start() {
    for cancel in [false, true] {
        let fixture = Fixture::new(Fault::None);
        let release = &fixture.driver.release;
        let (inventory, evidence) = facts(release);
        let policy = plan(&InspectionScope::from(release), &inventory, &evidence)
            .unwrap()
            .remove(0);
        let deployment = DeploymentId::new();
        let clock = MonotonicClock::default();
        let redactor = Redactor::default();
        let cancellation = CancellationToken::new();
        fixture
            .history
            .create_deployment(&deployment, &release.project_id, &release.environment_id, 0)
            .unwrap();
        fixture
            .history
            .transition_deployment(
                &deployment,
                DeploymentState::Created,
                DeploymentState::Running,
                1,
            )
            .unwrap();
        if cancel {
            cancellation.cancel();
        }
        let run = RetentionRun {
            history: &fixture.history,
            clock: &clock,
            redactor: &redactor,
            events: &NoEvents,
            cancellation: &cancellation,
        };
        let deadline = tokio::time::Instant::now()
            + if cancel {
                TOTAL_TIMEOUT
            } else {
                Duration::from_secs(179)
            };
        let result = run
            .delete_candidate(
                &deployment,
                &fixture.component,
                &fixture.component.planned.context,
                &policy,
                deadline,
            )
            .await;
        assert!(result.unwrap_err().contains("cleanup not started"));
        assert!(fixture.driver.calls.lock().unwrap().is_empty());
        assert!(
            fixture
                .history
                .pending_intents(&deployment)
                .unwrap()
                .is_empty()
        );
        let steps = fixture.history.steps(&deployment).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "cleanup.v1");
        assert_eq!(steps[0].status, crate::history::StepStatus::Failed);
        assert!(
            fixture
                .history
                .observations(&deployment)
                .unwrap()
                .is_empty()
        );
    }
}
