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
        audit::{RemoteAuditOutcome, RemoteAuditPhase},
        linux_ssh::{
            AuthenticatedSession, DeploymentMarker, LinuxSshDestination, LinuxSshDriver,
            LinuxSshTarget, connect_authenticated,
        },
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

    async fn session(&self, name: &str) -> AuthenticatedSession {
        let context = self.context(name);
        let destination = context
            .destination_settings
            .as_any()
            .downcast_ref::<LinuxSshDestination>()
            .unwrap();
        connect_authenticated(
            destination,
            &SshCredential::IdentityFile {
                path: PathBuf::from(required_env("SHIPFORGE_TEST_KEY")),
            },
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }

    async fn remote(&self, name: &str, program: &str, args: &[&str]) -> Vec<u8> {
        let session = self.session(name).await;
        let command =
            CommandSpec::structured(program, args.iter().map(|arg| CommandArgument::plain(*arg)))
                .unwrap();
        let output = session
            .execute(&command, Duration::from_secs(15), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            output.exit_status, 0,
            "disposable fixture mutation failed: {output:?}"
        );
        assert!(!output.stdout_truncated);
        session.disconnect().await.unwrap();
        output.stdout
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
    verify_inventory_and_audit(
        &fixture,
        &baseline.deployment.id,
        &first,
        &failure.deployment.id,
    )
    .await;
}

async fn verify_inventory_and_audit(
    fixture: &Fixture,
    source: &DeploymentId,
    first: &BTreeMap<ComponentName, Option<ReleaseRef>>,
    failed: &DeploymentId,
) {
    for name in ["frontend", "backend"] {
        let context = fixture.context(name);
        let inventory = fixture.driver.inventory(&context).await.unwrap();
        assert!(inventory.releases.issues.is_empty(), "{inventory:#?}");
        assert!(inventory.releases.releases.len() >= 3);
        assert!(!inventory.audit.incomplete, "{inventory:#?}");
        assert!(!inventory.audit.records.is_empty());
        for entry in &inventory.releases.releases {
            assert!(entry.extracted);
            assert_eq!(entry.manifest.component, component(name));
            assert!(inventory.audit.records.iter().any(|record| {
                record.phase == RemoteAuditPhase::Prepare
                    && record.package.as_ref().is_some_and(|package| {
                        package.manifest == entry.manifest
                            && package.sha256 == entry.sha256
                            && package.size == entry.size
                    })
            }));
        }
        if name == "backend" {
            let failure = inventory
                .audit
                .records
                .iter()
                .find(|record| {
                    &record.deployment == failed && record.phase == RemoteAuditPhase::Activate
                })
                .unwrap();
            assert_eq!(failure.outcome, RemoteAuditOutcome::Failed);
            assert_eq!(
                failure.healthy, None,
                "a restored link is not a fresh health check"
            );
        }
        // Historical attribution remains what was recorded, not the caller's new revision.
        let mut revised = context.clone();
        revised.destination_revision = serde_json::from_str("2").unwrap();
        let observed = fixture.driver.inventory(&revised).await.unwrap();
        assert_eq!(observed.audit.records, inventory.audit.records);
        assert!(
            observed
                .audit
                .records
                .iter()
                .all(|record| record.release.destination_revision == context.destination_revision)
        );
    }

    verify_audit_failure_preserves_rollback(fixture, source, first).await;
    verify_audit_loss_and_links(fixture).await;
}

async fn verify_audit_failure_preserves_rollback(
    fixture: &Fixture,
    source: &DeploymentId,
    first: &BTreeMap<ComponentName, Option<ReleaseRef>>,
) {
    let name = "frontend";
    let context = fixture.context(name);
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    assert_eq!(target.root, "/srv/shipforge-acceptance/frontend");
    let audit_file = format!("{}/metadata/deployments.jsonl", target.root);
    // A post-effect audit permission failure must not turn a successful rollback into a failure.
    fixture
        .remote(name, "chmod", &["400", "--", &audit_file])
        .await;
    let expected = fixture.driver.current(&context).await.unwrap().unwrap();
    let desired = first[&component(name)].clone();
    let history = HistoryStore::open(&fixture.history).unwrap();
    let report = RollbackOrchestrator::new(&history, Redactor::default())
        .rollback(
            source,
            vec![RollbackComponent {
                driver: fixture.driver.clone(),
                context: context.clone(),
                expected_current: expected,
                target: desired.clone(),
            }],
            &[component(name)],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        report.deployment.state,
        DeploymentState::Succeeded,
        "{report:#?}"
    );
    assert_eq!(report.warnings.len(), 1, "{report:#?}");
    assert!(report.warnings[0].contains("remote audit was not saved"));
    assert_eq!(fixture.driver.current(&context).await.unwrap(), desired);
    fixture
        .remote(name, "chmod", &["600", "--", &audit_file])
        .await;
}

async fn verify_audit_loss_and_links(fixture: &Fixture) {
    let name = "frontend";
    let context = fixture.context(name);
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    let audit_file = format!("{}/metadata/deployments.jsonl", target.root);
    let before = fixture.driver.inventory(&context).await.unwrap();
    let repeated = before
        .audit
        .records
        .iter()
        .find(|record| record.phase == RemoteAuditPhase::Activate)
        .unwrap();
    fixture
        .remote(
            name,
            "sh",
            &[
                "-c",
                "printf '%s' '{truncated' >> \"$1\"",
                "fixture-audit-tail",
                &audit_file,
            ],
        )
        .await;
    let session = fixture.session(name).await;
    session
        .append_audit(
            target,
            &DeploymentMarker::for_context(&context),
            repeated,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    session.disconnect().await.unwrap();
    let after = fixture.driver.inventory(&context).await.unwrap();
    assert!(after.audit.incomplete);
    assert_eq!(
        after.audit.records, before.audit.records,
        "identical retry deduplicates; torn tail is not evidence"
    );
    assert_eq!(after.releases, before.releases);

    // Missing audit files do not remove independently verified archive inventory.
    for file in ["deployments.jsonl", "releases.jsonl"] {
        let path = format!("{}/metadata/{file}", target.root);
        fixture
            .remote(
                name,
                "mv",
                &["--", &path, &format!("{path}.fixture-backup")],
            )
            .await;
    }
    let missing = fixture.driver.inventory(&context).await.unwrap();
    assert_eq!(missing.releases, before.releases);
    assert!(missing.audit.records.is_empty());
    assert!(missing.audit.incomplete);

    verify_audit_unsafe_paths(fixture, repeated).await;
    verify_partial_inventory(fixture, &before).await;
}

async fn verify_audit_unsafe_paths(
    fixture: &Fixture,
    repeated: &shipforge::drivers::audit::RemoteAuditRecord,
) {
    let name = "frontend";
    let context = fixture.context(name);
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    let audit_file = format!("{}/metadata/deployments.jsonl", target.root);
    // Unsafe audit links cannot redirect a write into another fixture file.
    let sentinel = format!("{}/metadata/sentinel", target.root);
    fixture
        .remote(
            name,
            "sh",
            &[
                "-c",
                "printf '%s' sentinel > \"$1\"",
                "fixture-sentinel",
                &sentinel,
            ],
        )
        .await;
    fixture
        .remote(name, "ln", &["-s", "--", &sentinel, &audit_file])
        .await;
    let session = fixture.session(name).await;
    assert!(
        session
            .append_audit(
                target,
                &DeploymentMarker::for_context(&context),
                repeated,
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    session.disconnect().await.unwrap();
    assert_eq!(
        fixture.remote(name, "cat", &["--", &sentinel]).await,
        b"sentinel"
    );

    fixture
        .remote(
            name,
            "mv",
            &["--", &audit_file, &format!("{audit_file}.fixture-link")],
        )
        .await;
    fixture
        .remote(name, "ln", &["--", &sentinel, &audit_file])
        .await;
    let session = fixture.session(name).await;
    assert!(
        session
            .append_audit(
                target,
                &DeploymentMarker::for_context(&context),
                repeated,
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    session.disconnect().await.unwrap();
    assert_eq!(
        fixture.remote(name, "cat", &["--", &sentinel]).await,
        b"sentinel"
    );
}

async fn verify_partial_inventory(
    fixture: &Fixture,
    before: &shipforge::drivers::ComponentInventory,
) {
    let name = "frontend";
    let context = fixture.context(name);
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    // Filename and archive manifest must agree; links and directory-only remnants are explicit issues.
    let source_archive = format!(
        "{}/archives/{}.tar.gz",
        target.root, before.releases.releases[0].manifest.version
    );
    fixture
        .remote(
            name,
            "cp",
            &[
                "--",
                &source_archive,
                &format!("{}/archives/wrong-manifest.tar.gz", target.root),
            ],
        )
        .await;
    fixture
        .remote(
            name,
            "ln",
            &[
                "-s",
                "--",
                &source_archive,
                &format!("{}/archives/linked.tar.gz", target.root),
            ],
        )
        .await;
    fixture
        .remote(
            name,
            "mkdir",
            &["--", &format!("{}/releases/directory-only", target.root)],
        )
        .await;
    let partial = fixture.driver.inventory(&context).await.unwrap();
    assert_eq!(partial.releases.releases, before.releases.releases);
    for version in ["wrong-manifest", "linked", "directory-only"] {
        assert!(
            partial.releases.issues.iter().any(|issue| issue
                .version
                .as_ref()
                .is_some_and(|found| found.as_str() == version)),
            "{partial:#?}"
        );
    }
    let detached = before
        .releases
        .releases
        .iter()
        .find(|entry| {
            before.releases.current.as_ref().unwrap().as_ref() != Some(&entry.manifest.version)
        })
        .unwrap();
    let directory = format!("{}/releases/{}", target.root, detached.manifest.version);
    let backup = format!("{}/temporary/fixture-extracted-backup", target.root);
    fixture
        .remote(name, "mv", &["--", &directory, &backup])
        .await;
    let archive_only = fixture.driver.inventory(&context).await.unwrap();
    let entry = archive_only
        .releases
        .releases
        .iter()
        .find(|entry| entry.manifest.version == detached.manifest.version)
        .unwrap();
    assert!(!entry.extracted);
    assert_eq!(entry.sha256, detached.sha256);
    assert_eq!(archive_only.releases.current, before.releases.current);
    fixture
        .remote(name, "mv", &["--", &backup, &directory])
        .await;
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
