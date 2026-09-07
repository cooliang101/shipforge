//! Actual subprocess termination around durable history boundaries, with simulated
//! local-file Driver facts. This does not simulate SSH transport or kill a live
//! deployment process; real Linux read-only inspection is covered separately.

use std::{
    any::Any,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shipforge::{
    application::{DeploymentSelection, DeploymentSession, RecoveryService},
    config::{DestinationRegistry, DestinationSettings, HostKeyFingerprint, ProjectConfigState},
    domain::{
        Capability, ComponentName, ComponentRelease, DeploymentId, DeploymentState,
        DriverCapabilities, ReleaseManifest, ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, ComponentExecutionContext, ComponentInventory,
        ComponentPlan, ComponentRequest, CredentialHandle, DeploymentDriver,
        DriverDestinationInput, DriverError, DriverKind, DriverLog, DriverRegistry,
        DriverTargetInput, EventSink, PreflightReport, PreparedRelease, ReleaseInventory,
        ReleasePackage, ReleaseRef, RemoteAuditHistory, RetentionPolicy,
        ValidatedDestinationSettings, ValidatedTargetSettings,
        inventory::{InventoryRelease, TemporaryRemnants},
    },
    history::{
        CurrentAlignment, DeploymentComponentSnapshot, HistoryStore, IntentStatus, StepStatus,
    },
    telemetry::Redactor,
};
use tokio_util::sync::CancellationToken;

const ROOT_ENV: &str = "SHIPFORGE_CRASH_TEST_ROOT";
const TOKEN_ENV: &str = "SHIPFORGE_CRASH_TEST_TOKEN";
const CRASH_EXIT: i32 = 77;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Boundary {
    BeforeEffect,
    AfterEffect,
    BetweenStages,
}

#[derive(Debug, Serialize, Deserialize)]
struct Scenario {
    token: uuid::Uuid,
    deployment: DeploymentId,
    stage: String,
    boundary: Boundary,
    target: ReleaseRef,
    previous: ReleaseRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SimulatedFacts {
    current: ReleaseVersion,
    archives: Vec<InventoryRelease>,
    build_finished: bool,
}

fn name() -> ComponentName {
    ComponentName::parse("frontend").unwrap()
}

fn archive(release: &ReleaseRef) -> InventoryRelease {
    InventoryRelease {
        manifest: ReleaseManifest::new(
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
        ),
        sha256: "a".repeat(64),
        size: 512,
        extracted: true,
    }
}

fn write_json(path: &Path, value: &impl Serialize) {
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    file.sync_all().unwrap();
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    let bytes = std::fs::read(path).unwrap();
    assert!(bytes.len() < 64 * 1024, "bounded fixture file");
    serde_json::from_slice(&bytes).unwrap()
}

struct Fixture {
    directory: tempfile::TempDir,
    selection: DeploymentSelection,
    scenario: Scenario,
}

impl Fixture {
    fn new(stage: &str, boundary: Boundary) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let ProjectConfigState::Loaded(config) = shipforge::config::load(directory.path()).unwrap()
        else {
            panic!("valid saved fixture configuration")
        };
        let environment = &config.environments["production"];
        let target = &environment.components[&name()];
        let mut registry = DestinationRegistry::new();
        let destination = registry
            .create(
                target.destination.clone(),
                DestinationSettings::LinuxSsh {
                    host: "never-contact.invalid".into(),
                    port: 22,
                    user: "simulated".into(),
                    credential: CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:simulated-only").unwrap(),
                },
            )
            .unwrap()
            .clone();
        registry
            .save(&directory.path().join("destinations.yaml"))
            .unwrap();
        let mut candidate = ReleaseRef {
            driver: DriverKind::parse("linux-ssh").unwrap(),
            project_id: config.project_id.clone(),
            environment_id: environment.id.clone(),
            component: name(),
            generation: target.generation,
            version: ReleaseVersion::parse("v2").unwrap(),
            destination: target.destination.clone(),
            destination_revision: destination.revision,
            endpoint_fingerprint: destination.endpoint_fingerprint,
            effective_capabilities: DriverCapabilities::new([
                Capability::Inventory,
                Capability::Observe,
            ]),
        };
        let mut previous = candidate.clone();
        previous.version = ReleaseVersion::parse("v1").unwrap();
        if stage == "rollback" {
            std::mem::swap(&mut candidate, &mut previous);
        }
        let scenario = Scenario {
            token: uuid::Uuid::now_v7(),
            deployment: DeploymentId::new(),
            stage: stage.into(),
            boundary,
            target: candidate,
            previous,
        };
        write_json(&directory.path().join("scenario.json"), &scenario);
        let selection = DeploymentSelection {
            project_root: directory.path().to_owned(),
            config,
            environment: "production".into(),
            components: [name()].into(),
        };
        Self {
            directory,
            selection,
            scenario,
        }
    }

    async fn terminate_child(&self) {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "child_crash_boundary",
                "--nocapture",
            ])
            .env(ROOT_ENV, self.directory.path())
            .env(TOKEN_ENV, self.scenario.token.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .expect("child test has a bounded lifetime")
            .expect("launch scoped child test executable");
        assert_eq!(
            output.status.code(),
            Some(CRASH_EXIT),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!self.directory.path().join("destructor-ran").exists());
    }
}

#[tokio::test]
async fn subprocess_termination_preserves_intents_and_reopens_simulated_driver_facts() {
    tokio::time::timeout(Duration::from_secs(90), async {
        for stage in ["build", "prepare", "activate", "compensate", "rollback"] {
            for boundary in [
                Boundary::BeforeEffect,
                Boundary::AfterEffect,
                Boundary::BetweenStages,
            ] {
                let fixture = Fixture::new(stage, boundary);
                fixture.terminate_child().await;
                verify_reopened_inspection(&fixture).await;
            }
        }
    })
    .await
    .expect("bounded crash/reopen scenario matrix");
}

async fn verify_reopened_inspection(fixture: &Fixture) {
    let path = fixture.directory.path().join("history.sqlite3");
    let history =
        HistoryStore::open(&path).expect("recover committed SQLite state after abrupt exit");
    let before = history
        .recovery_basis(&fixture.scenario.deployment)
        .unwrap();
    let steps = history.steps(&fixture.scenario.deployment).unwrap();
    assert_eq!(before.record.state, DeploymentState::Running);
    assert_eq!(
        before.snapshots[0].target.as_ref(),
        Some(&fixture.scenario.target)
    );
    assert_eq!(
        before.snapshots[0].expected_current.as_ref(),
        Some(&fixture.scenario.previous)
    );
    let between = fixture.scenario.boundary == Boundary::BetweenStages;
    assert_eq!(before.intents.len(), usize::from(!between));
    assert_eq!(
        steps[0].status,
        if between {
            StepStatus::Succeeded
        } else {
            StepStatus::Running
        }
    );
    let attention = HistoryStore::local_attention(&path, None, None).unwrap();
    assert!(
        attention
            .candidates
            .iter()
            .any(|record| record.deployment == fixture.scenario.deployment)
    );
    let facts_path = fixture.directory.path().join("simulated-remote.json");
    let facts_before = std::fs::read(&facts_path).unwrap();
    let facts: SimulatedFacts = read_json(&facts_path);
    assert_eq!(
        facts.build_finished,
        fixture.scenario.stage != "build"
            || !matches!(fixture.scenario.boundary, Boundary::BeforeEffect)
    );
    let mut drivers = DriverRegistry::default();
    drivers
        .register(Arc::new(FileFactsDriver {
            path: facts_path.clone(),
        }))
        .unwrap();
    let session = Arc::new(DeploymentSession::default());
    let service = RecoveryService::new(Arc::new(drivers), path, Arc::clone(&session));
    let inspection = service
        .inspect(
            fixture.selection.clone(),
            Some(fixture.scenario.deployment.clone()),
            &fixture.directory.path().join("destinations.yaml"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(inspection.persistence_warning.is_none(), "{inspection:#?}");
    let result = &inspection.report.components[0];
    let expected = if facts.current == fixture.scenario.target.version {
        CurrentAlignment::Target
    } else {
        CurrentAlignment::Previous
    };
    assert_eq!(result.alignment, expected);
    let inventory = result.inventory.as_ref().unwrap();
    assert_eq!(inventory.releases.current, Ok(Some(facts.current)));
    assert_eq!(inventory.releases.releases, facts.archives);
    assert_eq!(
        history
            .recovery_basis(&fixture.scenario.deployment)
            .unwrap(),
        before
    );
    assert_eq!(history.steps(&fixture.scenario.deployment).unwrap(), steps);
    assert_eq!(std::fs::read(facts_path).unwrap(), facts_before);
    assert_eq!(
        history.recovery_report(&inspection.report.id).unwrap(),
        Some(inspection.report)
    );
    assert!(!session.is_active());
}

struct DestructorWitness(PathBuf);
impl Drop for DestructorWitness {
    fn drop(&mut self) {
        std::fs::write(&self.0, b"unwound").unwrap();
    }
}

#[test]
#[ignore = "only launched by the parent crash matrix with an explicit temporary fixture token"]
fn child_crash_boundary() {
    let Some(root) = std::env::var_os(ROOT_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(root.is_absolute() && root.is_dir());
    let scenario: Scenario = read_json(&root.join("scenario.json"));
    assert_eq!(
        std::env::var(TOKEN_ENV).unwrap(),
        scenario.token.to_string()
    );
    let _witness = DestructorWitness(root.join("destructor-ran"));
    let history = initialize_history(&root, &scenario);
    let mut facts = initial_facts(&scenario);
    write_json(&root.join("simulated-remote.json"), &facts);
    let intended = if scenario.stage == "compensate" {
        &scenario.previous
    } else {
        &scenario.target
    };
    let intent = history
        .record_intent(
            &scenario.deployment,
            &name(),
            &scenario.stage,
            intended.version.as_str(),
            10,
        )
        .unwrap();
    if scenario.boundary == Boundary::BeforeEffect {
        std::process::exit(CRASH_EXIT);
    }
    simulate_effect(&scenario, &mut facts);
    write_json(&root.join("simulated-remote.json"), &facts);
    if scenario.boundary == Boundary::AfterEffect {
        std::process::exit(CRASH_EXIT);
    }
    if scenario.stage == "build" {
        record_package(&history, &scenario);
    }
    history
        .complete_intent(
            intent,
            IntentStatus::Succeeded,
            None,
            11,
            &Redactor::default(),
        )
        .unwrap();
    // Exit between completed stages: no pending intent, but no terminal state either.
    std::process::exit(CRASH_EXIT);
}

fn initialize_history(root: &Path, scenario: &Scenario) -> HistoryStore {
    let history = HistoryStore::open(&root.join("history.sqlite3")).unwrap();
    let target = &scenario.target;
    if scenario.stage == "rollback" {
        let source = DeploymentId::new();
        history
            .create_deployment(&source, &target.project_id, &target.environment_id, 1)
            .unwrap();
        history
            .transition_deployment(
                &source,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        history
            .transition_deployment(
                &source,
                DeploymentState::Running,
                DeploymentState::Succeeded,
                3,
            )
            .unwrap();
        history
            .create_rollback_deployment(
                &scenario.deployment,
                &source,
                &target.project_id,
                &target.environment_id,
                4,
            )
            .unwrap();
    } else {
        history
            .create_deployment(
                &scenario.deployment,
                &target.project_id,
                &target.environment_id,
                4,
            )
            .unwrap();
    }
    history
        .record_component_snapshots(
            &scenario.deployment,
            &[DeploymentComponentSnapshot {
                target_snapshot: None,
                release: target.clone(),
                expected_current: Some(scenario.previous.clone()),
                target: Some(target.clone()),
                execution_order: 0,
            }],
        )
        .unwrap();
    history
        .plan_steps(&scenario.deployment, &name(), &[scenario.stage.as_str()])
        .unwrap();
    history
        .transition_deployment(
            &scenario.deployment,
            DeploymentState::Created,
            DeploymentState::Running,
            5,
        )
        .unwrap();
    if !matches!(scenario.stage.as_str(), "build" | "rollback") {
        record_package(&history, scenario);
    }
    history
}

fn record_package(history: &HistoryStore, scenario: &Scenario) {
    let package = archive(&scenario.target);
    history
        .record_release_package(
            &scenario.deployment,
            &scenario.target,
            &package.manifest,
            &package.sha256,
            package.size,
        )
        .unwrap();
}

fn initial_facts(scenario: &Scenario) -> SimulatedFacts {
    let archives = if matches!(scenario.stage.as_str(), "build" | "prepare") {
        vec![archive(&scenario.previous)]
    } else {
        vec![archive(&scenario.previous), archive(&scenario.target)]
    };
    let current = if scenario.stage == "compensate" {
        &scenario.target
    } else {
        &scenario.previous
    };
    SimulatedFacts {
        current: current.version.clone(),
        archives,
        build_finished: scenario.stage != "build",
    }
}

fn simulate_effect(scenario: &Scenario, facts: &mut SimulatedFacts) {
    match scenario.stage.as_str() {
        "build" => facts.build_finished = true,
        "prepare" => facts.archives.push(archive(&scenario.target)),
        "activate" | "rollback" => facts.current = scenario.target.version.clone(),
        "compensate" => facts.current = scenario.previous.version.clone(),
        _ => panic!("unknown bounded fixture stage"),
    }
}

#[derive(Debug)]
struct FileFactsDriver {
    path: PathBuf,
}

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

#[async_trait]
impl DeploymentDriver for FileFactsDriver {
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
        _: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        let facts: SimulatedFacts = read_json(&self.path);
        Ok(ComponentInventory {
            releases: ReleaseInventory {
                releases: facts.archives,
                issues: vec![],
                current: Ok(Some(facts.current)),
                notices: vec![],
            },
            audit: RemoteAuditHistory {
                records: vec![],
                notices: vec!["Simulated Driver has no remote audit".into()],
                incomplete: true,
            },
            remnants: TemporaryRemnants::default(),
        })
    }
    async fn current(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        panic!("recovery must use coherent inventory")
    }
    async fn preflight(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        panic!("recovery must not run build/preflight")
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        _: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        panic!("recovery must not plan new effects")
    }
    async fn prepare(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        panic!("read-only recovery must not prepare")
    }
    async fn activate(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("read-only recovery must not activate")
    }
    async fn rollback(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: Option<&ReleaseRef>,
        _: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("read-only recovery must not rollback")
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        panic!("read-only recovery must not fetch service logs")
    }
    async fn cleanup(
        &self,
        _: &ComponentExecutionContext,
        _: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        panic!("read-only recovery must not clean up")
    }
}
