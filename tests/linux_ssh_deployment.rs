//! Opt-in writes to two independently pinned disposable Linux containers only.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use shipforge::{
    application::{
        DeploymentFailure, DeploymentReport, DeploymentSelection, DeploymentService,
        OrchestrationStage, RollbackComponent, RollbackOrchestrator,
    },
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, CredentialRegistry, DestinationRegistry,
        DestinationSettings, EnvironmentSetup, HostKeyFingerprint, ProjectConfig, ProjectSetup,
        SshCredential, TargetSetup, initialize,
    },
    domain::{ComponentName, ComponentOutcome, DeploymentId, DeploymentState, DestinationKey},
    drivers::{
        ComponentExecutionContext, DeploymentDriver, DriverLog, DriverRegistry, EventSink,
        ReleaseRef,
        linux_ssh::{LinuxSshDestination, LinuxSshDriver, connect_authenticated},
    },
    history::HistoryStore,
    telemetry::{CommandArgument, CommandSpec, Redactor},
};
use tokio_util::sync::CancellationToken;

struct Fixture {
    _scratch: tempfile::TempDir,
    project: PathBuf,
    config: ProjectConfig,
    destinations: DestinationRegistry,
    destinations_path: PathBuf,
    history: PathBuf,
    driver: Arc<LinuxSshDriver>,
    service: DeploymentService,
}

impl Fixture {
    async fn new() -> Self {
        assert_eq!(required_env("SHIPFORGE_LINUX_ACCEPTANCE"), "1");
        let identity = PathBuf::from(required_env("SHIPFORGE_TEST_KEY"));
        assert!(identity.is_absolute() && identity.is_file());
        let scratch = tempfile::tempdir().unwrap();
        let project = scratch.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut credentials = CredentialRegistry::new();
        let handle = credentials
            .create(SshCredential::IdentityFile {
                path: identity.clone(),
            })
            .unwrap();
        let mut destinations = DestinationRegistry::new();
        let mut targets = Vec::new();
        let mut ports = std::collections::BTreeSet::new();
        let mut fingerprints = std::collections::BTreeSet::new();
        for suffix in ["A", "B"] {
            let port = required_env(&format!("SHIPFORGE_TEST_PORT_{suffix}"))
                .parse::<u16>()
                .unwrap();
            assert!(
                port != 0 && ports.insert(port),
                "two distinct disposable endpoints required"
            );
            let key = required_env(&format!("SHIPFORGE_TEST_HOST_KEY_{suffix}"));
            assert!(
                fingerprints.insert(key.clone()),
                "independent disposable Host Keys required"
            );
            let target = DestinationKey::new();
            destinations
                .create(
                    target.clone(),
                    DestinationSettings::LinuxSsh {
                        host: "127.0.0.1".into(),
                        port,
                        user: "deploy".into(),
                        credential: handle.clone(),
                        host_key: HostKeyFingerprint::parse(key).unwrap(),
                    },
                )
                .unwrap();
            attest_fixture(&destinations, &target, &identity).await;
            targets.push(target);
        }
        let config = initialize(&project, project_setup(&targets)).unwrap();
        let driver = Arc::new(LinuxSshDriver::new(Arc::new(credentials)));
        let mut registry = DriverRegistry::default();
        registry.register(driver.clone()).unwrap();
        let history = scratch.path().join("history.sqlite3");
        let service = DeploymentService::new(Arc::new(registry), history.clone());
        let destinations_path = scratch.path().join("destinations.yaml");
        destinations.save(&destinations_path).unwrap();
        let fixture = Self {
            _scratch: scratch,
            project,
            config,
            destinations,
            destinations_path,
            history,
            driver,
            service,
        };
        fixture.payload("frontend", "healthy");
        fixture.payload("backend", "healthy");
        fixture
    }

    fn payload(&self, component: &str, health: &str) {
        let path = self.project.join(component);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("health.txt"), health).unwrap();
        std::fs::write(path.join("version.txt"), uuid::Uuid::now_v7().to_string()).unwrap();
    }

    async fn deploy(&self, selected: &[&str], cancel_on_backend: bool) -> DeploymentReport {
        let cancellation = CancellationToken::new();
        let plan = self
            .service
            .plan(
                DeploymentSelection {
                    project_root: self.project.clone(),
                    config: self.config.clone(),
                    environment: "acceptance".into(),
                    components: selected.iter().map(|name| component(name)).collect(),
                },
                &self.destinations,
                &cancellation,
            )
            .await
            .expect("real Linux read-only plan");
        assert_eq!(plan.entries.len(), selected.len());
        let progress = Progress {
            cancellation: cancellation.clone(),
            cancel_on_backend,
            activation_events: Mutex::new(Vec::new()),
        };
        let report = self
            .service
            .execute(plan, &self.destinations_path, &progress, &cancellation)
            .await
            .expect("real Linux deployment report");
        assert!(report.warnings.is_empty(), "{report:#?}");
        if cancel_on_backend {
            assert!(
                cancellation.is_cancelled(),
                "cancellation hook was not reached"
            );
            let events = progress.activation_events.lock().unwrap();
            assert_eq!(
                events.as_slice(),
                ["frontend.started", "frontend.finished", "backend.started"],
                "cancel only after frontend activation and before backend activation completes"
            );
        }
        report
    }

    fn context(&self, name: &str) -> ComponentExecutionContext {
        let environment = &self.config.environments["acceptance"];
        let target = &environment.components[&component(name)];
        let record = self.destinations.resolve(&target.destination).unwrap();
        let resolved = record.resolve();
        ComponentExecutionContext {
            project_id: self.config.project_id.clone(),
            environment_id: environment.id.clone(),
            component: component(name),
            generation: target.generation,
            destination: target.destination.clone(),
            destination_revision: record.revision,
            credential: resolved.credential,
            endpoint_fingerprint: resolved.endpoint_fingerprint,
            destination_settings: self
                .driver
                .validate_destination(&resolved.settings)
                .unwrap(),
            target: self.driver.validate_target(&target.driver_input()).unwrap(),
            cancellation: CancellationToken::new(),
        }
    }

    async fn observed(&self) -> BTreeMap<ComponentName, Option<ReleaseRef>> {
        let mut releases = BTreeMap::new();
        for name in ["frontend", "backend"] {
            let current = self
                .driver
                .current(&self.context(name))
                .await
                .expect("observe real current and manifest");
            releases.insert(component(name), current);
        }
        releases
    }

    async fn rollback(
        &self,
        source: &DeploymentId,
        targets: &BTreeMap<ComponentName, Option<ReleaseRef>>,
    ) {
        let current = self.observed().await;
        let components = ["frontend", "backend"]
            .iter()
            .map(|name| RollbackComponent {
                driver: self.driver.clone(),
                context: self.context(name),
                expected_current: current[&component(name)].clone().unwrap(),
                target: targets[&component(name)].clone(),
            })
            .collect();
        let history = HistoryStore::open(&self.history).unwrap();
        let report = RollbackOrchestrator::new(&history, Redactor::default())
            .rollback(
                source,
                components,
                &[component("frontend"), component("backend")],
                &CancellationToken::new(),
            )
            .await
            .expect("explicit rollback on real Linux");
        assert_eq!(report.deployment.state, DeploymentState::Succeeded);
        assert!(report.compensation_failures.is_empty());
        assert_eq!(&self.observed().await, targets);
    }
}

struct Progress {
    cancellation: CancellationToken,
    cancel_on_backend: bool,
    activation_events: Mutex<Vec<&'static str>>,
}

impl EventSink for Progress {
    fn emit(&self, event: DriverLog) {
        let activation = match event.namespace.as_str() {
            "activate.started" if event.message.contains("frontend") => Some("frontend.started"),
            "activate.finished" if event.message.contains("frontend") => Some("frontend.finished"),
            "activate.started" if event.message.contains("backend") => Some("backend.started"),
            "activate.finished" if event.message.contains("backend") => Some("backend.finished"),
            _ => None,
        };
        if let Some(activation) = activation {
            self.activation_events.lock().unwrap().push(activation);
        }
        if self.cancel_on_backend
            && event.namespace == "activate.started"
            && event.message.contains("backend")
        {
            self.cancellation.cancel();
        }
        if !event.namespace.starts_with("build.std") {
            eprintln!("{}: {}", event.namespace, event.message);
        }
    }
}

#[tokio::test]
#[ignore = "requires two explicitly provisioned disposable Linux OpenSSH endpoints"]
async fn real_linux_single_joint_rollback_health_failure_and_cancellation() {
    tokio::time::timeout(Duration::from_secs(600), run_real_linux_acceptance())
        .await
        .expect("disposable Linux acceptance exceeded its ten-minute deadline");
}

async fn run_real_linux_acceptance() {
    let fixture = Fixture::new().await;
    assert!(fixture.observed().await.values().all(Option::is_none));
    let single = fixture.deploy(&["frontend"], false).await;
    assert_eq!(
        single.deployment.state,
        DeploymentState::Succeeded,
        "{single:#?}"
    );
    assert_eq!(single.deployment.components.len(), 1);
    let first = fixture.observed().await;
    assert!(first[&component("frontend")].is_some());
    assert!(first[&component("backend")].is_none());

    fixture.payload("frontend", "healthy");
    let joint = fixture.deploy(&["frontend", "backend"], false).await;
    assert_eq!(joint.deployment.state, DeploymentState::Succeeded);
    assert_eq!(joint.deployment.components.len(), 2);
    let joint_releases = fixture.observed().await;
    assert!(joint_releases.values().all(Option::is_some));
    assert_ne!(
        joint_releases[&component("frontend")],
        first[&component("frontend")]
    );
    fixture.rollback(&joint.deployment.id, &first).await;

    let baseline = fixture.deploy(&["frontend", "backend"], false).await;
    assert_eq!(baseline.deployment.state, DeploymentState::Succeeded);
    let healthy = fixture.observed().await;
    fixture.payload("backend", "unhealthy");
    let failure = fixture.deploy(&["frontend", "backend"], false).await;
    assert_eq!(failure.deployment.state, DeploymentState::Failed);
    match failure.failure.as_ref() {
        Some(DeploymentFailure::Driver {
            component: failed_component,
            stage,
            error,
            ..
        }) => {
            assert_eq!(failed_component, &component("backend"));
            assert_eq!(*stage, OrchestrationStage::Activate);
            assert_eq!(error.stage, "health", "{failure:#?}");
            assert!(error.message.contains("HTTP status 503"), "{failure:#?}");
            assert!(
                error.message.contains("activation was compensated"),
                "{failure:#?}"
            );
        }
        other => panic!("expected backend HTTP 503 health failure, got {other:#?}"),
    }
    assert!(failure.compensation_failures.is_empty());
    assert_eq!(
        failure.deployment.components[&component("frontend")].outcome,
        ComponentOutcome::Compensated
    );
    assert_eq!(fixture.observed().await, healthy);

    fixture.payload("backend", "healthy");
    let cancelled = fixture.deploy(&["frontend", "backend"], true).await;
    assert_eq!(cancelled.deployment.state, DeploymentState::Cancelled);
    assert_eq!(cancelled.failure, Some(DeploymentFailure::Cancelled));
    assert_eq!(
        cancelled.deployment.components[&component("frontend")].outcome,
        ComponentOutcome::Compensated
    );
    assert!(cancelled.compensation_failures.is_empty());
    assert_eq!(fixture.observed().await, healthy);
    assert_history(&fixture.history);
}

fn assert_history(path: &std::path::Path) {
    let database = rusqlite::Connection::open(path).unwrap();
    let pending: i64 = database
        .query_row(
            "SELECT count(*) FROM operation_intents WHERE status='pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);
    let unfinished: i64 = database
        .query_row(
            "SELECT count(*) FROM deployments WHERE state IN ('created','running')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unfinished, 0);
    let rollbacks: i64 = database.query_row("SELECT count(*) FROM deployments WHERE kind='rollback' AND related_deployment_id IS NOT NULL", [], |row| row.get(0)).unwrap();
    assert_eq!(rollbacks, 1);
}

fn project_setup(destinations: &[DestinationKey]) -> ProjectSetup {
    let mut builds = BTreeMap::new();
    let mut targets = BTreeMap::new();
    for (index, name) in ["frontend", "backend"].iter().enumerate() {
        builds.insert(
            component(name),
            ComponentSetup {
                working_directory: Some(".".into()),
                build: vec![BuildCommand::argv("rustc", ["--version"])],
                artifact: ArtifactSpec {
                    path: (*name).into(),
                },
            },
        );
        targets.insert(
            component(name),
            TargetSetup {
                destination: destinations[index].clone(),
                root: Some(format!("/srv/shipforge-acceptance/{name}")),
                systemd: None,
                health: Some(format!("http://127.0.0.1:8080/{name}")),
                after: if index == 1 {
                    vec![component("frontend")]
                } else {
                    Vec::new()
                },
            },
        );
    }
    ProjectSetup {
        project: "linux-acceptance".into(),
        components: builds,
        environments: BTreeMap::from([(
            "acceptance".into(),
            EnvironmentSetup {
                components: targets,
            },
        )]),
    }
}

async fn attest_fixture(
    registry: &DestinationRegistry,
    key: &DestinationKey,
    identity: &std::path::Path,
) {
    let destination =
        LinuxSshDestination::validate(&registry.resolve(key).unwrap().resolve().settings).unwrap();
    let token = CancellationToken::new();
    let session = connect_authenticated(
        &destination,
        &SshCredential::IdentityFile {
            path: identity.to_owned(),
        },
        Duration::from_secs(10),
        &token,
    )
    .await
    .expect("authenticate independently pinned disposable Linux");
    let output = session
        .execute(
            &CommandSpec::structured(
                "cat",
                [CommandArgument::plain("/etc/shipforge-disposable-fixture")],
            )
            .unwrap(),
            Duration::from_secs(10),
            &token,
        )
        .await
        .expect("verify disposable fixture before writes");
    assert_eq!(output.exit_status, 0);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "shipforge-m1-disposable-v1"
    );
    session.disconnect().await.unwrap();
}

fn component(name: &str) -> ComponentName {
    ComponentName::parse(name).unwrap()
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set by the disposable fixture runner"))
}
