use std::{any::Any, collections::BTreeSet, sync::Mutex};

use async_trait::async_trait;

use super::*;
use crate::{
    config::{DestinationSettings, HostKeyFingerprint},
    domain::{
        Capability, ComponentName, ComponentRelease, DeploymentState, DriverCapabilities,
        ReleaseManifest, ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, ComponentPlan, ComponentRequest, CredentialHandle,
        DriverDestinationInput, DriverError, DriverKind, DriverLog, DriverTargetInput, EventSink,
        PreflightReport, PreparedRelease, ReleaseInventory, ReleasePackage, RemoteAuditHistory,
        RetentionPolicy, ValidatedDestinationSettings, ValidatedTargetSettings,
        inventory::{InventoryIssue, InventoryRelease, TemporaryRemnants},
    },
    history::{DeploymentQuery, IntentStatus},
};

#[derive(Debug)]
struct Settings(DriverKind);
impl ValidatedDestinationSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl ValidatedTargetSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
enum DuringRead {
    Nothing,
    RemoveYaml(PathBuf),
    RemoveRegistry(PathBuf),
    EmptyHistory(PathBuf),
    MutateHistory(PathBuf, DeploymentId, ComponentName),
    FailPersistence(PathBuf),
    Cancel(CancellationToken),
}

#[derive(Debug)]
struct FakeDriver {
    reads: Mutex<Vec<InspectionScope>>,
    inventories: Mutex<BTreeMap<ComponentName, Result<ComponentInventory, String>>>,
    during: Mutex<DuringRead>,
}

#[async_trait]
impl DeploymentDriver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::parse("linux-ssh").unwrap()
    }
    fn static_capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::new([Capability::Inventory, Capability::Observe])
    }
    fn validate_destination(
        &self,
        _: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind())))
    }
    fn validate_target(
        &self,
        _: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind())))
    }
    async fn inventory(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        self.reads
            .lock()
            .unwrap()
            .push(scope_from_release(&ReleaseRef {
                driver: self.kind(),
                project_id: context.project_id.clone(),
                environment_id: context.environment_id.clone(),
                component: context.component.clone(),
                generation: context.generation,
                version: version("unused"),
                destination: context.destination.clone(),
                destination_revision: context.destination_revision,
                endpoint_fingerprint: context.endpoint_fingerprint.clone(),
                effective_capabilities: self.static_capabilities(),
            }));
        match std::mem::replace(&mut *self.during.lock().unwrap(), DuringRead::Nothing) {
            DuringRead::Nothing => {}
            DuringRead::RemoveYaml(path) | DuringRead::RemoveRegistry(path) => {
                std::fs::remove_file(path).unwrap();
            }
            DuringRead::MutateHistory(path, deployment, component) => {
                HistoryStore::open(&path)
                    .unwrap()
                    .record_intent(&deployment, &component, "changed", "v2", 20)
                    .unwrap();
            }
            DuringRead::FailPersistence(path) => {
                rusqlite::Connection::open(path).unwrap().execute_batch(
                    "CREATE TRIGGER deny_recovery BEFORE INSERT ON recovery_reports BEGIN SELECT RAISE(ABORT, 'disk full fixture'); END;"
                ).unwrap();
            }
            DuringRead::Cancel(token) => token.cancel(),
            DuringRead::EmptyHistory(path) => std::fs::write(path, []).unwrap(),
        }
        self.inventories
            .lock()
            .unwrap()
            .get(&context.component)
            .cloned()
            .unwrap_or_else(|| Ok(empty_inventory(None)))
            .map_err(|message| DriverError {
                stage: "inventory".into(),
                target: "private.example.test".into(),
                message,
                suggested_action: "private provider data".into(),
            })
    }
    async fn current(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        panic!("inventory contains coherent current evidence")
    }
    async fn preflight(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        panic!("recovery must not preflight/build")
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        _: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        panic!("recovery must not create Release plans")
    }
    async fn prepare(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        panic!("no remote mutation")
    }
    async fn activate(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("no remote mutation")
    }
    async fn rollback(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: Option<&ReleaseRef>,
        _: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("no remote mutation")
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        panic!("no remote log invocation")
    }
    async fn cleanup(
        &self,
        _: &ComponentExecutionContext,
        _: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        panic!("no remote mutation")
    }
}

fn version(text: &str) -> ReleaseVersion {
    ReleaseVersion::parse(text).unwrap()
}
fn component(text: &str) -> ComponentName {
    ComponentName::parse(text).unwrap()
}

fn empty_inventory(current: Option<&str>) -> ComponentInventory {
    ComponentInventory {
        releases: ReleaseInventory {
            releases: vec![],
            issues: vec![],
            current: Ok(current.map(version)),
            notices: vec![],
        },
        audit: RemoteAuditHistory {
            records: vec![],
            notices: vec!["Audit missing".into()],
            incomplete: true,
        },
        remnants: TemporaryRemnants {
            entries: vec![],
            notices: vec![],
            incomplete: false,
        },
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    selection: DeploymentSelection,
    registry_path: PathBuf,
    history_path: PathBuf,
    driver: Arc<FakeDriver>,
    session: Arc<DeploymentSession>,
    service: RecoveryService,
}

impl Fixture {
    fn new(names: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        // Build tools deliberately do not exist: inspection is independent of builds.
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../../../docs/examples/shipforge.yaml")
                .replace("npm", "not-an-installed-build-program"),
        )
        .unwrap();
        let ProjectConfigState::Loaded(config) = crate::config::load(directory.path()).unwrap()
        else {
            panic!()
        };
        let mut registry = DestinationRegistry::new();
        for target in config.environments["production"].components.values() {
            if registry.resolve(&target.destination).is_none() {
                registry
                    .create(
                        target.destination.clone(),
                        DestinationSettings::LinuxSsh {
                            host: "private.example.test".into(),
                            port: 22,
                            user: "private-user".into(),
                            credential: CredentialHandle::new(),
                            host_key: HostKeyFingerprint::parse("SHA256:private-host-key").unwrap(),
                        },
                    )
                    .unwrap();
            }
        }
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let history_path = directory.path().join("history.sqlite3");
        let driver = Arc::new(FakeDriver {
            reads: Mutex::new(vec![]),
            inventories: Mutex::new(BTreeMap::new()),
            during: Mutex::new(DuringRead::Nothing),
        });
        let mut drivers = DriverRegistry::default();
        drivers.register(driver.clone()).unwrap();
        let session = Arc::new(DeploymentSession::default());
        let service = RecoveryService::new(
            Arc::new(drivers),
            history_path.clone(),
            Arc::clone(&session),
        );
        let selection = DeploymentSelection {
            project_root: directory.path().to_owned(),
            config,
            environment: "production".into(),
            components: names.iter().map(|name| component(name)).collect(),
        };
        Self {
            directory,
            selection,
            registry_path,
            history_path,
            driver,
            session,
            service,
        }
    }

    fn source(
        &self,
        previous: Option<&str>,
        stage: Option<&str>,
        completed: bool,
    ) -> (DeploymentId, Vec<DeploymentComponentSnapshot>) {
        let history = HistoryStore::open(&self.history_path).unwrap();
        let id = DeploymentId::new();
        let environment = &self.selection.config.environments["production"];
        let registry = DestinationRegistry::load(&self.registry_path).unwrap();
        history
            .create_deployment(&id, &self.selection.config.project_id, &environment.id, 1)
            .unwrap();
        let snapshots: Vec<_> = self
            .selection
            .components
            .iter()
            .enumerate()
            .map(|(order, name)| {
                let target = &environment.components[name];
                let destination = registry.resolve(&target.destination).unwrap();
                let release = ReleaseRef {
                    driver: self.driver.kind(),
                    project_id: self.selection.config.project_id.clone(),
                    environment_id: environment.id.clone(),
                    component: name.clone(),
                    generation: target.generation,
                    version: version("v2"),
                    destination: target.destination.clone(),
                    destination_revision: destination.revision,
                    endpoint_fingerprint: destination.endpoint_fingerprint.clone(),
                    effective_capabilities: self.driver.static_capabilities(),
                };
                let mut expected = release.clone();
                expected.version = version(previous.unwrap_or("unused"));
                DeploymentComponentSnapshot {
                    release: release.clone(),
                    target: Some(release),
                    expected_current: previous.map(|_| expected),
                    execution_order: u32::try_from(order).unwrap(),
                }
            })
            .collect();
        history.record_component_snapshots(&id, &snapshots).unwrap();
        history
            .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 2)
            .unwrap();
        for snapshot in &snapshots {
            let release = &snapshot.release;
            let manifest = ReleaseManifest::new(
                &ComponentRelease {
                    project_id: release.project_id.clone(),
                    environment_id: release.environment_id.clone(),
                    component: release.component.clone(),
                    generation: release.generation,
                    version: release.version.clone(),
                    destination: release.destination.clone(),
                    destination_revision: release.destination_revision,
                },
                100,
                None,
            );
            history
                .record_release_package(&id, release, &manifest, &"a".repeat(64), 512)
                .unwrap();
            if let Some(stage) = stage {
                let intent = history
                    .record_intent(&id, &release.component, stage, "v2", 3)
                    .unwrap();
                if completed {
                    history
                        .complete_intent(
                            intent,
                            IntentStatus::Succeeded,
                            None,
                            4,
                            &Redactor::default(),
                        )
                        .unwrap();
                }
            }
            let mut inventory = empty_inventory(previous);
            inventory.releases.releases.push(InventoryRelease {
                manifest,
                sha256: "a".repeat(64),
                size: 512,
                extracted: true,
            });
            self.driver
                .inventories
                .lock()
                .unwrap()
                .insert(release.component.clone(), Ok(inventory));
        }
        (id, snapshots)
    }

    async fn inspect(
        &self,
        source: Option<DeploymentId>,
    ) -> Result<RecoveryInspection, RecoveryError> {
        self.service
            .inspect(
                self.selection.clone(),
                source,
                &self.registry_path,
                &CancellationToken::new(),
            )
            .await
    }

    fn current(&self, name: &str, value: Result<Option<ReleaseVersion>, String>) {
        self.driver
            .inventories
            .lock()
            .unwrap()
            .get_mut(&component(name))
            .unwrap()
            .as_mut()
            .unwrap()
            .releases
            .current = value;
    }
}

#[tokio::test]
async fn database_loss_rebuilds_inventory_only_and_reopens_without_fabricated_history() {
    let fixture = Fixture::new(&["frontend"]);
    let inspection = fixture.inspect(None).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    let history = HistoryStore::open(&fixture.history_path).unwrap();
    assert_eq!(
        history.recovery_report(&inspection.report.id).unwrap(),
        Some(inspection.report.clone())
    );
    assert!(
        history
            .deployments(
                &fixture.selection.config.project_id,
                &fixture.selection.config.environments["production"].id,
                DeploymentQuery::default()
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        inspection.report.components[0].alignment,
        CurrentAlignment::Unplanned
    );
    assert_eq!(
        inspection.report.components[0]
            .inventory
            .as_ref()
            .unwrap()
            .releases
            .current,
        Ok(None)
    );
}

#[tokio::test]
async fn every_interrupted_stage_keeps_original_intents_and_never_claims_historical_success() {
    for stage in ["build", "prepare", "activate", "compensate", "rollback"] {
        for completed in [false, true] {
            let fixture = Fixture::new(&["frontend"]);
            let (id, _) = fixture.source(Some("v1"), Some(stage), completed);
            fixture.current("frontend", Ok(Some(version("v2"))));
            let before = HistoryStore::open(&fixture.history_path)
                .unwrap()
                .recovery_basis(&id)
                .unwrap();
            let inspection = fixture.inspect(Some(id.clone())).await.unwrap();
            assert!(
                inspection.persistence_warning.is_none(),
                "{stage}: {:?}",
                inspection.persistence_warning
            );
            assert_eq!(
                inspection.report.components[0].alignment,
                CurrentAlignment::Target
            );
            assert_eq!(
                inspection.report.components[0].package_alignment,
                PackageAlignment::Matches
            );
            let after = HistoryStore::open(&fixture.history_path)
                .unwrap()
                .recovery_basis(&id)
                .unwrap();
            assert_eq!(after.record, before.record);
            assert_eq!(after.intents, before.intents);
            assert_eq!(after.observations, before.observations);
            assert_eq!(after.revision, before.revision);
        }
    }
}

#[tokio::test]
async fn terminal_pending_intent_is_retained_and_mixed_components_are_only_possible_partial_application()
 {
    let fixture = Fixture::new(&["backend", "worker"]);
    let (id, _) = fixture.source(Some("v1"), Some("activate"), false);
    HistoryStore::open(&fixture.history_path)
        .unwrap()
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Failed, 10)
        .unwrap();
    fixture.current("backend", Ok(Some(version("v2"))));
    let inspection = fixture.inspect(Some(id.clone())).await.unwrap();
    assert!(inspection.possibly_partially_applied());
    let history = HistoryStore::open(&fixture.history_path).unwrap();
    assert_eq!(history.pending_intents(&id).unwrap().len(), 2);
    assert_eq!(
        history.deployment(&id).unwrap().unwrap().state,
        DeploymentState::Failed
    );
}

#[tokio::test]
async fn absent_unknown_and_drifted_current_are_not_conflated() {
    let fixture = Fixture::new(&["backend", "frontend", "worker"]);
    let (id, _) = fixture.source(None, None, false);
    fixture.current("backend", Err("observation unavailable".into()));
    fixture.current("worker", Ok(Some(version("external"))));
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert_eq!(
        inspection
            .report
            .components
            .iter()
            .map(|entry| entry.alignment)
            .collect::<Vec<_>>(),
        [
            CurrentAlignment::Unknown,
            CurrentAlignment::Previous,
            CurrentAlignment::Other
        ]
    );
}

#[tokio::test]
async fn archive_digest_mismatch_and_missing_extraction_are_distinct() {
    let fixture = Fixture::new(&["backend", "frontend", "worker"]);
    let (id, _) = fixture.source(Some("v1"), None, false);
    {
        let mut inventories = fixture.driver.inventories.lock().unwrap();
        inventories
            .get_mut(&component("backend"))
            .unwrap()
            .as_mut()
            .unwrap()
            .releases
            .releases[0]
            .sha256 = "b".repeat(64);
        inventories
            .get_mut(&component("frontend"))
            .unwrap()
            .as_mut()
            .unwrap()
            .releases
            .releases[0]
            .extracted = false;
        let worker = inventories
            .get_mut(&component("worker"))
            .unwrap()
            .as_mut()
            .unwrap();
        worker.releases.releases.clear();
        worker.releases.issues.push(InventoryIssue {
            version: Some(version("v2")),
            message: "invalid archive".into(),
        });
    }
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    assert_eq!(
        inspection
            .report
            .components
            .iter()
            .map(|entry| entry.package_alignment)
            .collect::<Vec<_>>(),
        [
            PackageAlignment::Mismatch,
            PackageAlignment::ArchiveOnly,
            PackageAlignment::Unknown
        ]
    );
}

#[tokio::test]
async fn historical_revision_is_used_exactly_after_latest_destination_changes() {
    let fixture = Fixture::new(&["frontend"]);
    let (id, snapshots) = fixture.source(Some("v1"), None, false);
    let mut registry = DestinationRegistry::load(&fixture.registry_path).unwrap();
    let old = registry
        .resolve(&snapshots[0].release.destination)
        .unwrap()
        .clone();
    let mut settings = old.settings.clone();
    let DestinationSettings::LinuxSsh { host, .. } = &mut settings;
    *host = "different-endpoint.example.test".into();
    registry
        .revise(&snapshots[0].release.destination, settings)
        .unwrap();
    registry.save(&fixture.registry_path).unwrap();
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    let reads = fixture.driver.reads.lock().unwrap();
    assert_eq!(reads[0].destination_revision, old.revision);
    assert_eq!(reads[0].endpoint_fingerprint, old.endpoint_fingerprint);
}

#[tokio::test]
async fn missing_yaml_and_corrupt_database_stop_before_any_remote_query() {
    let fixture = Fixture::new(&["frontend"]);
    let path = fixture.directory.path().join("shipforge.yaml");
    let contents = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(fixture.inspect(None).await.is_err());
    assert!(!fixture.history_path.exists());
    std::fs::write(path, contents).unwrap();
    std::fs::write(&fixture.history_path, b"not a database").unwrap();
    assert!(fixture.inspect(None).await.is_err());
    assert!(fixture.driver.reads.lock().unwrap().is_empty());
    assert_eq!(
        std::fs::read(&fixture.history_path).unwrap(),
        b"not a database"
    );
}

#[tokio::test]
async fn changed_generation_is_unknown_without_connecting_to_new_target() {
    let mut fixture = Fixture::new(&["frontend"]);
    let (id, _) = fixture.source(Some("v1"), None, false);
    let path = fixture.directory.path().join("shipforge.yaml");
    let yaml = std::fs::read_to_string(&path)
        .unwrap()
        .replace("generation: 1", "generation: 2");
    std::fs::write(path, yaml).unwrap();
    let ProjectConfigState::Loaded(config) = crate::config::load(fixture.directory.path()).unwrap()
    else {
        panic!()
    };
    fixture.selection.config = config;
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    assert_eq!(
        inspection.report.components[0].alignment,
        CurrentAlignment::Unknown
    );
    assert!(fixture.driver.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn local_history_change_during_observation_rejects_stale_cache_but_retains_facts() {
    let fixture = Fixture::new(&["frontend"]);
    let (id, _) = fixture.source(Some("v1"), None, false);
    *fixture.driver.during.lock().unwrap() = DuringRead::MutateHistory(
        fixture.history_path.clone(),
        id.clone(),
        component("frontend"),
    );
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert!(
        inspection
            .persistence_warning
            .as_ref()
            .unwrap()
            .contains("history changed")
    );
    assert!(inspection.report.components[0].inventory.is_ok());
    assert!(
        HistoryStore::open(&fixture.history_path)
            .unwrap()
            .recovery_report(&inspection.report.id)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn yaml_change_after_first_read_stops_remaining_components_and_cache() {
    let fixture = Fixture::new(&["backend", "worker"]);
    *fixture.driver.during.lock().unwrap() =
        DuringRead::RemoveYaml(fixture.directory.path().join("shipforge.yaml"));
    let inspection = fixture.inspect(None).await.unwrap();
    assert!(
        inspection
            .persistence_warning
            .as_ref()
            .unwrap()
            .contains("configuration changed")
    );
    assert_eq!(fixture.driver.reads.lock().unwrap().len(), 1);
    assert!(inspection.report.components[0].inventory.is_ok());
    assert!(inspection.report.components[1].inventory.is_err());
}

#[tokio::test]
async fn report_write_failure_retains_observations_without_touching_old_history() {
    let fixture = Fixture::new(&["frontend"]);
    let (id, _) = fixture.source(Some("v1"), Some("activate"), false);
    *fixture.driver.during.lock().unwrap() =
        DuringRead::FailPersistence(fixture.history_path.clone());
    let inspection = fixture.inspect(Some(id.clone())).await.unwrap();
    assert!(inspection.persistence_warning.is_some());
    assert!(inspection.report.components[0].inventory.is_ok());
    assert_eq!(
        HistoryStore::open(&fixture.history_path)
            .unwrap()
            .pending_intents(&id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn shared_session_rejects_inspection_before_io_and_releases_on_completion() {
    let fixture = Fixture::new(&["frontend"]);
    fixture
        .session
        .run(async {
            assert!(matches!(
                fixture.inspect(None).await,
                Err(RecoveryError::Busy)
            ));
        })
        .await
        .unwrap();
    assert!(!fixture.history_path.exists());
    assert!(fixture.driver.reads.lock().unwrap().is_empty());
    fixture.inspect(None).await.unwrap();
    assert!(!fixture.session.is_active());
}

#[tokio::test]
async fn cancellation_keeps_remaining_components_unknown_without_remote_mutations() {
    let fixture = Fixture::new(&["backend", "worker"]);
    let cancellation = CancellationToken::new();
    *fixture.driver.during.lock().unwrap() = DuringRead::Cancel(cancellation.clone());
    let inspection = fixture
        .service
        .inspect(
            fixture.selection.clone(),
            None,
            &fixture.registry_path,
            &cancellation,
        )
        .await
        .unwrap();
    assert_eq!(fixture.driver.reads.lock().unwrap().len(), 1);
    assert!(inspection.report.components[1].inventory.is_err());
    assert!(!fixture.session.is_active());
}

#[tokio::test]
async fn unknown_latest_inventory_never_silently_falls_back_to_older_success() {
    let fixture = Fixture::new(&["frontend"]);
    let first = fixture.inspect(None).await.unwrap();
    fixture.driver.inventories.lock().unwrap().insert(
        component("frontend"),
        Err("private-user@private.example.test token=this-must-not-leak".into()),
    );
    let second = fixture.inspect(None).await.unwrap();
    let history = HistoryStore::open(&fixture.history_path).unwrap();
    assert_eq!(
        history
            .latest_recovery_report(&first.report.components[0].scope)
            .unwrap(),
        Some(second.report.clone())
    );
    let text = serde_json::to_string(&second.report).unwrap();
    assert!(!text.contains("private.example.test"));
    assert!(!text.contains("this-must-not-leak"));
}

#[tokio::test]
async fn selecting_component_not_in_frozen_deployment_refuses_instead_of_borrowing_current_config()
{
    let mut fixture = Fixture::new(&["frontend"]);
    let (id, _) = fixture.source(None, None, false);
    fixture.selection.components = BTreeSet::from([component("worker")]);
    assert!(fixture.inspect(Some(id)).await.is_err());
    assert!(fixture.driver.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_historical_destination_never_falls_back_or_connects() {
    let fixture = Fixture::new(&["frontend"]);
    let (id, _) = fixture.source(Some("v1"), None, false);
    DestinationRegistry::new()
        .save(&fixture.registry_path)
        .unwrap();
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    assert!(inspection.report.components[0].inventory.is_err());
    assert!(fixture.driver.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn destination_change_during_read_stops_other_components_and_cache() {
    let fixture = Fixture::new(&["backend", "worker"]);
    *fixture.driver.during.lock().unwrap() =
        DuringRead::RemoveRegistry(fixture.registry_path.clone());
    let inspection = fixture.inspect(None).await.unwrap();
    assert!(
        inspection
            .persistence_warning
            .as_ref()
            .unwrap()
            .contains("configuration changed")
    );
    assert_eq!(fixture.driver.reads.lock().unwrap().len(), 1);
    assert!(inspection.report.components[0].inventory.is_ok());
    assert!(inspection.report.components[1].inventory.is_err());
}

#[tokio::test]
async fn conflicting_exact_target_prepare_audit_prevents_package_match() {
    use crate::drivers::audit::{
        RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPackage, RemoteAuditRecord,
    };
    let fixture = Fixture::new(&["frontend"]);
    let (id, snapshots) = fixture.source(Some("v1"), None, false);
    {
        let mut inventories = fixture.driver.inventories.lock().unwrap();
        let inventory = inventories
            .get_mut(&component("frontend"))
            .unwrap()
            .as_mut()
            .unwrap();
        let archive = &inventory.releases.releases[0];
        inventory.audit.records.push(RemoteAuditRecord {
            schema_version: 1,
            event_id: uuid::Uuid::now_v7(),
            deployment: id.clone(),
            recorded_at_ms: 3,
            release: snapshots[0].release.clone(),
            phase: RemoteAuditPhase::Prepare,
            outcome: RemoteAuditOutcome::Succeeded,
            expected_current: Some(version("v1")),
            target: Some(version("v2")),
            observed: RemoteAuditObserved::Unknown,
            healthy: None,
            package: Some(RemoteAuditPackage {
                manifest: archive.manifest.clone(),
                sha256: "b".repeat(64),
                size: archive.size,
            }),
        });
    }
    let inspection = fixture.inspect(Some(id)).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    assert_eq!(
        inspection.report.components[0].package_alignment,
        PackageAlignment::Mismatch
    );
}

#[tokio::test]
async fn evidence_budget_stops_further_reads_and_marks_omissions_explicitly_unknown() {
    let fixture = Fixture::new(&["backend", "frontend", "worker"]);
    for name in ["backend", "frontend", "worker"] {
        let mut inventory = empty_inventory(None);
        inventory.releases.issues = (0..1024)
            .map(|_| InventoryIssue {
                version: None,
                message: "x".repeat(1024),
            })
            .collect();
        fixture
            .driver
            .inventories
            .lock()
            .unwrap()
            .insert(component(name), Ok(inventory));
    }
    let inspection = fixture.inspect(None).await.unwrap();
    assert!(inspection.persistence_warning.is_none(), "{inspection:#?}");
    assert_eq!(fixture.driver.reads.lock().unwrap().len(), 2);
    assert!(inspection.report.components[0].inventory.is_ok());
    assert!(
        inspection.report.components[1]
            .inventory
            .as_ref()
            .unwrap_err()
            .contains("remaining inspection limit")
    );
    assert!(
        inspection.report.components[2]
            .inventory
            .as_ref()
            .unwrap_err()
            .contains("not inspected")
    );
    assert!(serde_json::to_vec(&inspection.report).unwrap().len() < MAX_EVIDENCE_BYTES);
}

#[tokio::test]
async fn existing_empty_or_uninitialized_history_is_not_silently_rebuilt() {
    for initialized in [false, true] {
        let fixture = Fixture::new(&["frontend"]);
        std::fs::write(&fixture.history_path, []).unwrap();
        if initialized {
            rusqlite::Connection::open(&fixture.history_path).unwrap()
                .execute_batch("CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('preserve');").unwrap();
        }
        let before = std::fs::read(&fixture.history_path).unwrap();
        assert!(fixture.inspect(None).await.is_err());
        assert_eq!(std::fs::read(&fixture.history_path).unwrap(), before);
        assert!(fixture.driver.reads.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn history_truncated_during_observation_is_not_reinitialized_at_persist() {
    for source_backed in [false, true] {
        let fixture = Fixture::new(&["frontend"]);
        let source = source_backed.then(|| fixture.source(None, None, false).0);
        *fixture.driver.during.lock().unwrap() =
            DuringRead::EmptyHistory(fixture.history_path.clone());
        let inspection = fixture.inspect(source).await.unwrap();
        assert!(inspection.persistence_warning.is_some());
        assert!(inspection.report.components[0].inventory.is_ok());
        assert_eq!(std::fs::metadata(&fixture.history_path).unwrap().len(), 0);
    }
}

#[tokio::test]
async fn interrupted_rollback_to_absence_is_target_alignment_not_healthy_success() {
    let fixture = Fixture::new(&["frontend"]);
    let (completed, mut snapshots) = fixture.source(None, None, false);
    let history = HistoryStore::open(&fixture.history_path).unwrap();
    history
        .transition_deployment(
            &completed,
            DeploymentState::Running,
            DeploymentState::Succeeded,
            3,
        )
        .unwrap();
    let rollback = DeploymentId::new();
    let snapshot = &mut snapshots[0];
    snapshot.expected_current.clone_from(&snapshot.target);
    snapshot.target = None;
    history
        .create_rollback_deployment(
            &rollback,
            &completed,
            &snapshot.release.project_id,
            &snapshot.release.environment_id,
            4,
        )
        .unwrap();
    history
        .record_component_snapshots(&rollback, &snapshots)
        .unwrap();
    history
        .transition_deployment(
            &rollback,
            DeploymentState::Created,
            DeploymentState::Running,
            5,
        )
        .unwrap();
    history
        .record_intent(
            &rollback,
            &component("frontend"),
            "rollback",
            "not_deployed",
            6,
        )
        .unwrap();
    let before = history.recovery_basis(&rollback).unwrap();
    let inspection = fixture.inspect(Some(rollback.clone())).await.unwrap();
    assert!(inspection.persistence_warning.is_none());
    assert_eq!(
        inspection.report.components[0].alignment,
        CurrentAlignment::Target
    );
    assert_eq!(
        inspection.report.components[0].package_alignment,
        PackageAlignment::Unplanned
    );
    assert_eq!(history.recovery_basis(&rollback).unwrap(), before);
    assert!(history.observations(&rollback).unwrap().is_empty());
}
