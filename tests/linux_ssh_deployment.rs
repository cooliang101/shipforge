//! Opt-in tests against two independently pinned disposable Linux containers only.

#[path = "linux_ssh_deployment/connection_stability.rs"]
mod connection_stability;

#[path = "linux_ssh_deployment/management_acceptance.rs"]
mod management_acceptance;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use shipforge::{
    application::{
        DeploymentFailure, DeploymentReport, DeploymentSelection, DeploymentService,
        DeploymentSession, OrchestrationStage, RecoveryInspection, RecoveryService,
        RollbackComponent, RollbackOrchestrator,
    },
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, CredentialRegistry, DestinationRegistry,
        DestinationSettings, EnvironmentSetup, HostKeyFingerprint, ProjectConfig, ProjectSetup,
        SshCredential, TargetSetup, initialize,
    },
    domain::{ComponentName, ComponentOutcome, DeploymentId, DeploymentState, DestinationKey},
    drivers::{
        CleanupCandidate, CleanupPathState, ComponentExecutionContext, DeploymentDriver, DriverLog,
        DriverRegistry, EventSink, ReleaseRef, RetentionPolicy,
        audit::{RemoteAuditOutcome, RemoteAuditPhase},
        inventory::{InventoryRelease, TemporaryRemnantKind},
        linux_ssh::{
            AuthenticatedSession, DeploymentMarker, LinuxSshDestination, LinuxSshDriver,
            LinuxSshTarget, connect_authenticated,
        },
    },
    history::{
        CurrentAlignment, DeploymentQuery, HistoryStore, PackageAlignment, ReleasePackageRecord,
    },
    telemetry::{CommandArgument, CommandSpec, Redactor},
};
use tokio_util::sync::CancellationToken;

struct Fixture {
    scratch: tempfile::TempDir,
    project: PathBuf,
    config: ProjectConfig,
    destinations: DestinationRegistry,
    destinations_path: PathBuf,
    history: PathBuf,
    driver: Arc<LinuxSshDriver>,
    drivers: Arc<DriverRegistry>,
    session: Arc<DeploymentSession>,
    service: DeploymentService,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_setup(project_setup).await
    }

    async fn with_setup(setup: fn(&[DestinationKey]) -> ProjectSetup) -> Self {
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
        let config = initialize(&project, setup(&targets)).unwrap();
        let driver = Arc::new(LinuxSshDriver::new(Arc::new(credentials)));
        let mut registry = DriverRegistry::default();
        registry.register(driver.clone()).unwrap();
        let drivers = Arc::new(registry);
        let history = scratch.path().join("history.sqlite3");
        let service = DeploymentService::new(Arc::clone(&drivers), history.clone());
        let destinations_path = scratch.path().join("destinations.yaml");
        destinations.save(&destinations_path).unwrap();
        let fixture = Self {
            scratch,
            project,
            config,
            destinations,
            destinations_path,
            history,
            driver,
            drivers,
            session: Arc::new(DeploymentSession::default()),
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
            .session
            .run(Box::pin(self.service.execute(
                plan,
                &self.destinations_path,
                &progress,
                &cancellation,
            )))
            .await
            .expect("exclusive fixture Deployment session")
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

#[tokio::test]
#[ignore = "requires two explicitly provisioned disposable Linux OpenSSH endpoints"]
async fn real_linux_exact_retention_partial_and_archive_only_retry() {
    tokio::time::timeout(Duration::from_secs(600), Box::pin(run_real_retention()))
        .await
        .expect("disposable Linux retention acceptance exceeded its ten-minute deadline");
}

#[tokio::test]
#[ignore = "requires two explicitly provisioned disposable Linux OpenSSH endpoints"]
async fn real_linux_automatic_retention_keeps_latest_five() {
    tokio::time::timeout(
        Duration::from_secs(600),
        Box::pin(run_automatic_retention()),
    )
    .await
    .expect("default automatic retention acceptance exceeded its ten-minute deadline");
}

async fn run_automatic_retention() {
    // with_setup independently attests both endpoints before any deployment. This
    // fresh UUID root has no sentinel/remnant writes and no retention overrides.
    let fixture = Fixture::with_setup(retention_setup).await;
    let context = fixture.context("frontend");
    let mut deployments = Vec::new();
    let mut packages: Vec<ReleasePackageRecord> = Vec::new();
    for _ in 0..6 {
        fixture.payload("frontend", "healthy");
        let report = fixture.deploy(&["frontend"], false).await;
        assert_eq!(
            report.deployment.state,
            DeploymentState::Succeeded,
            "{report:#?}"
        );
        assert!(report.failure.is_none() && report.warnings.is_empty());
        assert!(report.compensation_failures.is_empty());
        let history = HistoryStore::open(&fixture.history).unwrap();
        let snapshots = history.component_snapshots(&report.deployment.id).unwrap();
        let receipts = history.release_packages(&report.deployment.id).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(receipts.len(), 1);
        let receipt = receipts.into_iter().next().unwrap();
        assert_eq!(snapshots[0].release, receipt.release);
        assert_eq!(snapshots[0].target.as_ref(), Some(&receipt.release));
        assert_eq!(
            snapshots[0].expected_current.as_ref(),
            packages.last().map(|package| &package.release)
        );
        assert!(
            !packages
                .iter()
                .any(|package| package.release.version == receipt.release.version)
        );
        // The execution-scoped local tar is temporary; these are its real,
        // immutable package receipts, not regenerated archives or invented rows.
        packages.push(receipt);
        deployments.push(report.deployment.id);
    }
    verify_automatic_retention_remote(&fixture, &context, &packages).await;
    verify_automatic_retention_history(&fixture.history, &deployments, &packages);
}

async fn verify_automatic_retention_remote(
    fixture: &Fixture,
    context: &ComponentExecutionContext,
    packages: &[ReleasePackageRecord],
) {
    assert_eq!(packages.len(), 6);
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    let run = target
        .root
        .strip_prefix("/srv/shipforge-acceptance/retention-")
        .unwrap();
    assert!(uuid::Uuid::parse_str(run).is_ok());
    let session = fixture.session("frontend").await;
    assert!(
        session
            .check_deployment_marker(
                target,
                &DeploymentMarker::for_context(context),
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    );
    session.disconnect().await.unwrap();
    // Physical absence, including dangling links, is checked independently of
    // inventory omission. No remote file is created, changed or repaired here.
    let output = fixture
        .remote(
            "frontend",
            "timeout",
            &[
                "--kill-after=2s",
                "10s",
                "sh",
                "-c",
                r#"set -eu
[ ! -L "$1" ] && [ -d "$1" ]
for dir in archives releases metadata temporary; do
  [ ! -L "$1/$dir" ] && [ -d "$1/$dir" ]
done
[ ! -L "$1/archives/$2.tar.gz" ] && [ ! -e "$1/archives/$2.tar.gz" ]
[ ! -L "$1/releases/$2" ] && [ ! -e "$1/releases/$2" ]
[ -L "$1/current" ] && [ "$(readlink -- "$1/current")" = "releases/$3" ]
printf 'first-archive-and-directory-absent\n'
"#,
                "fixture-default-retention-read-only",
                &target.root,
                packages[0].release.version.as_str(),
                packages[5].release.version.as_str(),
            ],
        )
        .await;
    assert_eq!(output, b"first-archive-and-directory-absent\n");
    let inventory = fixture.driver.inventory(context).await.unwrap();
    assert_eq!(
        inventory.releases.current,
        Ok(Some(packages[5].release.version.clone()))
    );
    assert!(inventory.releases.issues.is_empty());
    assert!(!inventory.remnants.incomplete && inventory.remnants.entries.is_empty());
    assert_eq!(inventory.releases.releases.len(), 5);
    for package in &packages[1..] {
        let entry = inventory
            .releases
            .releases
            .iter()
            .find(|entry| entry.manifest.version == package.release.version)
            .unwrap();
        assert!(entry.extracted);
        assert_eq!(entry.manifest, package.manifest);
        assert_eq!(entry.sha256, package.sha256);
        assert_eq!(entry.size, package.size);
    }
    assert_eq!(
        fixture.driver.current(context).await.unwrap(),
        Some(packages[5].release.clone())
    );
}

fn verify_automatic_retention_history(
    path: &std::path::Path,
    deployments: &[DeploymentId],
    packages: &[ReleasePackageRecord],
) {
    assert_eq!(deployments.len(), 6);
    let history = HistoryStore::open(path).unwrap();
    for (deployment, package) in deployments.iter().zip(packages) {
        assert_eq!(
            history.deployment(deployment).unwrap().unwrap().state,
            DeploymentState::Succeeded
        );
        assert!(history.pending_intents(deployment).unwrap().is_empty());
        assert_eq!(
            history.release_packages(deployment).unwrap().as_slice(),
            std::slice::from_ref(package)
        );
    }
    assert!(
        history
            .observations(&deployments[4])
            .unwrap()
            .iter()
            .any(|observation| {
                observation.healthy == Some(true)
                    && observation.observed.as_ref().ok().and_then(Option::as_ref)
                        == Some(&packages[4].release)
            }),
        "the fifth Release has recorded positive health, not inferred inventory health"
    );
    let expected_step = format!("cleanup.{}", packages[0].release.version);
    let steps = history.steps(&deployments[5]).unwrap();
    let cleanup = steps
        .iter()
        .filter(|step| step.name.starts_with("cleanup."))
        .collect::<Vec<_>>();
    assert_eq!(cleanup.len(), 1);
    assert_eq!(cleanup[0].component, component("frontend"));
    assert_eq!(cleanup[0].name, expected_step);
    assert_eq!(cleanup[0].status, shipforge::history::StepStatus::Succeeded);
    assert!(cleanup[0].intent.is_some() && cleanup[0].error.is_none());
    let database =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let total: i64 = database
        .query_row(
            "SELECT count(*) FROM operation_intents WHERE stage LIKE 'cleanup.%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        total, 1,
        "only the first version was selected by the default policy"
    );
    let outcome: (String, String, String, String, Option<String>, Option<i64>) = database.query_row(
        "SELECT component,stage,target,status,error,completed_at_ms FROM operation_intents WHERE deployment_id=?1 AND stage LIKE 'cleanup.%'",
        [deployments[5].to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    ).unwrap();
    assert_eq!(outcome.0, "frontend");
    assert_eq!(outcome.1, expected_step);
    assert_eq!(outcome.2, packages[0].release.version.as_str());
    assert_eq!(outcome.3, "succeeded");
    assert!(outcome.4.is_none() && outcome.5.is_some());
}

async fn run_real_retention() {
    // Independent scope: this case never reads or mutates the lifecycle case's roots.
    let fixture = Fixture::with_setup(retention_setup).await;
    let context = fixture.context("frontend");
    let mut versions = Vec::new();
    for _ in 0..4 {
        fixture.payload("frontend", "healthy");
        let report = fixture.deploy(&["frontend"], false).await;
        assert_eq!(
            report.deployment.state,
            DeploymentState::Succeeded,
            "{report:#?}"
        );
        let snapshots = HistoryStore::open(&fixture.history)
            .unwrap()
            .component_snapshots(&report.deployment.id)
            .unwrap();
        versions.push(snapshots[0].release.clone());
    }
    let root = attest_retention_root(&fixture, &context).await;
    let before = fixture.driver.inventory(&context).await.unwrap();
    assert_eq!(before.releases.releases.len(), 4);
    assert!(before.releases.issues.is_empty());
    assert_eq!(
        before.releases.current,
        Ok(Some(versions[3].version.clone()))
    );
    let package = |release: &ReleaseRef| {
        before
            .releases
            .releases
            .iter()
            .find(|entry| entry.manifest.version == release.version)
            .unwrap()
            .clone()
    };
    let policy = retention_policy(&versions, versions[0].clone(), package(&versions[0]));
    let preserved = retention_snapshot(&fixture, &root, &versions[..2]).await;
    let report = fixture.driver.cleanup(&context, &policy).await.unwrap();
    assert_eq!(report.removed, vec![versions[0].version.clone()]);
    assert!(report.partial.is_empty() && report.warnings.is_empty());
    assert_eq!(
        retention_snapshot(&fixture, &root, &versions[..2]).await,
        preserved
    );
    let after = fixture.driver.inventory(&context).await.unwrap();
    assert_eq!(after.releases.current, before.releases.current);
    assert_eq!(after.releases.releases.len(), 3);
    assert!(
        !after
            .releases
            .releases
            .iter()
            .any(|entry| { entry.manifest.version == versions[0].version })
    );
    let retry = retention_policy(&versions, versions[1].clone(), package(&versions[1]));
    verify_retention_partial_retry(&fixture, &context, &root, &versions, retry).await;
    assert_eq!(
        retention_snapshot(&fixture, &root, &versions[..2]).await,
        preserved,
        "current, retained payloads, marker, metadata and temporary files are unchanged"
    );
}

fn retention_policy(
    versions: &[ReleaseRef],
    release: ReleaseRef,
    package: InventoryRelease,
) -> RetentionPolicy {
    RetentionPolicy {
        protected_versions: versions[2..]
            .iter()
            .map(|item| item.version.clone())
            .collect(),
        retain_count: 2,
        candidate: CleanupCandidate {
            release,
            package,
            expected_current: Some(versions[3].version.clone()),
        },
    }
}

async fn attest_retention_root(fixture: &Fixture, context: &ComponentExecutionContext) -> String {
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    let root = &target.root;
    let run = root
        .strip_prefix("/srv/shipforge-acceptance/retention-")
        .unwrap();
    assert!(uuid::Uuid::parse_str(run).is_ok());
    attest_fixture(
        &fixture.destinations,
        &context.destination,
        &PathBuf::from(required_env("SHIPFORGE_TEST_KEY")),
    )
    .await;
    let session = fixture.session("frontend").await;
    assert!(
        session
            .check_deployment_marker(
                target,
                &DeploymentMarker::for_context(context),
                &CancellationToken::new()
            )
            .await
            .unwrap()
    );
    session.disconnect().await.unwrap();
    fixture
        .remote(
            "frontend",
            "sh",
            &[
                "-c",
                r#"set -eu
[ "$(id -u)" != 0 ]
[ ! -L "$1" ] && [ -d "$1" ]
for dir in archives releases metadata temporary; do
  [ ! -L "$1/$dir" ] && [ -d "$1/$dir" ]
done
[ "$(stat -c %a -- "$1/archives")" = 755 ]
(set -C; printf '%s' retention-sentinel > "$1/temporary/retention-sentinel")
(set -C; printf '%s' metadata-sentinel > "$1/metadata/retention-sentinel")
"#,
                "fixture-retention-attestation",
                root,
            ],
        )
        .await;
    root.clone()
}

async fn retention_snapshot(fixture: &Fixture, root: &str, omitted: &[ReleaseRef]) -> Vec<u8> {
    assert_eq!(omitted.len(), 2);
    fixture.remote("frontend", "timeout", &[
        "--kill-after=2s", "10s", "bash", "-o", "pipefail", "-c", r#"set -eu
find -P "$1" -xdev \( -path "$1/archives/$2.tar.gz" -o -path "$1/releases/$2" -o -path "$1/archives/$3.tar.gz" -o -path "$1/releases/$3" \) -prune -o -printf '%y %p %l\n' | LC_ALL=C sort
find -P "$1" -xdev \( -path "$1/archives/$2.tar.gz" -o -path "$1/releases/$2" -o -path "$1/archives/$3.tar.gz" -o -path "$1/releases/$3" \) -prune -o -type f -exec sha256sum -- {} + | LC_ALL=C sort
"#, "fixture-retention-snapshot", root, omitted[0].version.as_str(), omitted[1].version.as_str(),
    ]).await
}

async fn verify_retention_partial_retry(
    fixture: &Fixture,
    context: &ComponentExecutionContext,
    root: &str,
    versions: &[ReleaseRef],
    mut policy: RetentionPolicy,
) {
    let archives = format!("{root}/archives");
    // deploy is unprivileged; chmod makes the archive unlink fail after successful
    // directory removal. Restore before unwrapping or asserting the Driver result.
    fixture
        .remote("frontend", "chmod", &["0555", "--", &archives])
        .await;
    let outcome = fixture.driver.cleanup(context, &policy).await;
    fixture
        .remote("frontend", "chmod", &["0755", "--", &archives])
        .await;
    let partial = outcome.unwrap();
    assert!(partial.removed.is_empty() && !partial.warnings.is_empty());
    assert_eq!(partial.partial.len(), 1);
    assert_eq!(partial.partial[0].version, versions[1].version);
    assert_eq!(partial.partial[0].directory, CleanupPathState::Absent);
    assert_eq!(partial.partial[0].archive, CleanupPathState::Present);
    let fresh = fixture.driver.inventory(context).await.unwrap();
    assert_eq!(
        fresh.releases.current,
        Ok(Some(versions[3].version.clone()))
    );
    let archive_only = fresh
        .releases
        .releases
        .iter()
        .find(|entry| entry.manifest.version == versions[1].version)
        .unwrap();
    assert!(!archive_only.extracted);
    assert_eq!(archive_only.manifest, policy.candidate.package.manifest);
    assert_eq!(archive_only.sha256, policy.candidate.package.sha256);
    assert_eq!(archive_only.size, policy.candidate.package.size);
    // A new exact candidate is built from fresh observations; never replay an old
    // durable intent or assert the already-removed directory still exists.
    policy.candidate.package = archive_only.clone();
    let completed = fixture.driver.cleanup(context, &policy).await.unwrap();
    assert_eq!(completed.removed, vec![versions[1].version.clone()]);
    assert!(completed.partial.is_empty() && completed.warnings.is_empty());
    let final_inventory = fixture.driver.inventory(context).await.unwrap();
    assert_eq!(
        final_inventory.releases.current,
        Ok(Some(versions[3].version.clone()))
    );
    assert!(final_inventory.releases.issues.is_empty());
    assert_eq!(final_inventory.releases.releases.len(), 2);
    for entry in final_inventory.releases.releases {
        assert!(policy.protected_versions.contains(&entry.manifest.version));
    }
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
    verify_recovery_inspection(&fixture, &baseline.deployment.id).await;
    verify_inventory_and_audit(
        &fixture,
        &baseline.deployment.id,
        &first,
        &failure.deployment.id,
    )
    .await;
}

async fn verify_recovery_inspection(fixture: &Fixture, completed: &DeploymentId) {
    let original = HistoryStore::open(&fixture.history)
        .unwrap()
        .recovery_basis(completed)
        .unwrap();
    let (history_path, interrupted, package) = interrupted_recovery_source(fixture, completed);
    create_recovery_remnants(fixture, &interrupted).await;
    let before_files = recovery_filesystem_snapshot(fixture).await;
    let history = HistoryStore::open(&history_path).unwrap();
    let before_history = history.recovery_basis(&interrupted).unwrap();
    let selection = DeploymentSelection {
        project_root: fixture.project.clone(),
        config: fixture.config.clone(),
        environment: "acceptance".into(),
        components: [component("frontend")].into(),
    };
    let recovery = RecoveryService::new(
        Arc::clone(&fixture.drivers),
        history_path,
        Arc::clone(&fixture.session),
    );
    let inspected = recovery
        .inspect(
            selection.clone(),
            Some(interrupted.clone()),
            &fixture.destinations_path,
            &CancellationToken::new(),
        )
        .await
        .expect("production recovery of an interrupted local source on real Linux");
    assert_recovery_evidence(&inspected, &package, &interrupted, CurrentAlignment::Target);
    assert_eq!(
        inspected.report.components[0].package_alignment,
        PackageAlignment::Matches
    );
    assert_eq!(
        history.recovery_basis(&interrupted).unwrap(),
        before_history
    );
    assert_eq!(history.pending_intents(&interrupted).unwrap().len(), 1);
    assert_eq!(
        history.deployment(&interrupted).unwrap().unwrap().state,
        DeploymentState::Running
    );
    assert_eq!(
        history.recovery_report(&inspected.report.id).unwrap(),
        Some(inspected.report)
    );
    assert_eq!(
        recovery_filesystem_snapshot(fixture).await,
        before_files,
        "inspection must not mutate remote filesystem facts"
    );

    verify_recovery_cache_only(fixture, &package, &interrupted, selection).await;
    assert_eq!(
        recovery_filesystem_snapshot(fixture).await,
        before_files,
        "cache rebuild must not change remote facts or remove remnants"
    );
    assert_eq!(
        HistoryStore::open(&fixture.history)
            .unwrap()
            .recovery_basis(completed)
            .unwrap(),
        original
    );
    assert!(!fixture.session.is_active());
}

async fn verify_recovery_cache_only(
    fixture: &Fixture,
    package: &ReleasePackageRecord,
    interrupted: &DeploymentId,
    selection: DeploymentSelection,
) {
    let fresh_path = fixture.history.with_file_name("rebuilt-history.sqlite3");
    assert!(!fresh_path.exists());
    let service = RecoveryService::new(
        Arc::clone(&fixture.drivers),
        fresh_path.clone(),
        Arc::clone(&fixture.session),
    );
    let inspected = service
        .inspect(
            selection,
            None,
            &fixture.destinations_path,
            &CancellationToken::new(),
        )
        .await
        .expect("production inventory-only cache rebuild on real Linux");
    assert_recovery_evidence(
        &inspected,
        package,
        interrupted,
        CurrentAlignment::Unplanned,
    );
    assert_eq!(
        inspected.report.components[0].package_alignment,
        PackageAlignment::Unplanned
    );
    assert!(inspected.report.related_deployment.is_none());
    let reopened = HistoryStore::open(&fresh_path).unwrap();
    assert_eq!(
        reopened.recovery_report(&inspected.report.id).unwrap(),
        Some(inspected.report)
    );
    assert!(
        reopened
            .deployments(
                &fixture.config.project_id,
                &fixture.config.environments["acceptance"].id,
                DeploymentQuery::default()
            )
            .unwrap()
            .is_empty()
    );
}

fn interrupted_recovery_source(
    fixture: &Fixture,
    completed: &DeploymentId,
) -> (PathBuf, DeploymentId, ReleasePackageRecord) {
    // Copy verified evidence into a separate fixture DB; never rewrite a completed Deployment.
    let original = HistoryStore::open(&fixture.history).unwrap();
    let mut snapshot = original
        .component_snapshots(completed)
        .unwrap()
        .into_iter()
        .find(|snapshot| snapshot.release.component == component("frontend"))
        .unwrap();
    snapshot.execution_order = 0;
    let package = original
        .release_packages(completed)
        .unwrap()
        .into_iter()
        .find(|package| package.release.component == component("frontend"))
        .unwrap();
    assert_eq!(snapshot.target.as_ref(), Some(&package.release));
    let path = fixture
        .history
        .with_file_name("interrupted-history.sqlite3");
    assert!(!path.exists());
    let history = HistoryStore::open(&path).unwrap();
    let interrupted = DeploymentId::new();
    history
        .create_deployment(
            &interrupted,
            &snapshot.release.project_id,
            &snapshot.release.environment_id,
            1,
        )
        .unwrap();
    history
        .record_component_snapshots(&interrupted, &[snapshot])
        .unwrap();
    history
        .plan_steps(&interrupted, &component("frontend"), &["activate"])
        .unwrap();
    history
        .transition_deployment(
            &interrupted,
            DeploymentState::Created,
            DeploymentState::Running,
            2,
        )
        .unwrap();
    history
        .record_release_package(
            &interrupted,
            &package.release,
            &package.manifest,
            &package.sha256,
            package.size,
        )
        .unwrap();
    history
        .record_intent(
            &interrupted,
            &component("frontend"),
            "activate",
            package.release.version.as_str(),
            3,
        )
        .unwrap();
    (path, interrupted, package)
}

async fn create_recovery_remnants(fixture: &Fixture, interrupted: &DeploymentId) {
    let context = fixture.context("frontend");
    let target = context
        .target
        .as_any()
        .downcast_ref::<LinuxSshTarget>()
        .unwrap();
    assert_eq!(target.root, "/srv/shipforge-acceptance/frontend");
    attest_fixture(
        &fixture.destinations,
        &context.destination,
        &PathBuf::from(required_env("SHIPFORGE_TEST_KEY")),
    )
    .await;
    let session = fixture.session("frontend").await;
    assert!(
        session
            .check_deployment_marker(
                target,
                &DeploymentMarker::for_context(&context),
                &CancellationToken::new()
            )
            .await
            .unwrap()
    );
    session.disconnect().await.unwrap();
    // All destinations are inside the existing attested temporary directory. Link
    // targets are deliberately dangling and never followed by either inventory or snapshot.
    fixture
        .remote(
            "frontend",
            "sh",
            &[
                "-c",
                r#"set -eu
test "$1" = /srv/shipforge-acceptance/frontend
test -d "$1" && test ! -L "$1"
test -d "$1/temporary" && test ! -L "$1/temporary"
test -f "$1/.shipforge-project.json" && test ! -L "$1/.shipforge-project.json"
for suffix in .tar.gz .dir .current .rollback-current; do
  test ! -e "$1/temporary/$2$suffix" && test ! -L "$1/temporary/$2$suffix"
done
set -C
printf '%s' fixture-partial-upload > "$1/temporary/$2.tar.gz"
mkdir -- "$1/temporary/$2.dir"
ln -s -- fixture-unpublished-current "$1/temporary/$2.current"
ln -s -- fixture-unpublished-rollback "$1/temporary/$2.rollback-current"
"#,
                "fixture-recovery-remnants",
                &target.root,
                &interrupted.to_string(),
            ],
        )
        .await;
}

async fn recovery_filesystem_snapshot(fixture: &Fixture) -> Vec<Vec<u8>> {
    // atime is intentionally excluded: reads may update it. No-follow metadata
    // (including inode/mtime/ctime/link targets) plus every regular-file digest
    // detects replacement, publication, repair, cleanup, or payload writes.
    let bytes = fixture
        .remote(
            "frontend",
            "sh",
            &[
                "-c",
                r#"set -eu
test "$1" = /srv/shipforge-acceptance/frontend
test -d "$1" && test ! -L "$1"
find -P "$1" -xdev -printf '%P|%y|%i|%m|%U|%G|%s|%T@|%C@|%l\n'
find -P "$1" -xdev -type f -exec sha256sum -- {} +
"#,
                "fixture-recovery-snapshot",
                "/srv/shipforge-acceptance/frontend",
            ],
        )
        .await;
    // Sort locally, without a shell pipeline that could hide a failed remote scan.
    let mut lines: Vec<_> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    lines.sort_unstable();
    lines
}

fn assert_recovery_evidence(
    inspection: &RecoveryInspection,
    package: &ReleasePackageRecord,
    interrupted: &DeploymentId,
    alignment: CurrentAlignment,
) {
    assert!(inspection.persistence_warning.is_none(), "{inspection:#?}");
    assert_eq!(inspection.report.components.len(), 1);
    let component = &inspection.report.components[0];
    assert_eq!(component.scope, (&package.release).into());
    assert_eq!(component.alignment, alignment);
    let inventory = component
        .inventory
        .as_ref()
        .expect("verified real Linux inventory");
    assert_eq!(
        inventory.releases.current,
        Ok(Some(package.release.version.clone()))
    );
    let actual = inventory
        .releases
        .releases
        .iter()
        .find(|entry| entry.manifest.version == package.release.version)
        .unwrap();
    assert_eq!(actual.manifest, package.manifest);
    assert_eq!(actual.sha256, package.sha256);
    assert_eq!(actual.size, package.size);
    assert!(actual.extracted);
    assert!(!inventory.remnants.incomplete, "{inventory:#?}");
    assert_eq!(inventory.remnants.entries.len(), 4);
    for kind in [
        TemporaryRemnantKind::UploadArchive,
        TemporaryRemnantKind::ExtractedDirectory,
        TemporaryRemnantKind::ActivationLink,
        TemporaryRemnantKind::RollbackLink,
    ] {
        assert!(
            inventory
                .remnants
                .entries
                .iter()
                .any(|entry| entry.kind == kind && entry.deployment.as_ref() == Some(interrupted)),
            "{inventory:#?}"
        );
    }
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

fn retention_setup(destinations: &[DestinationKey]) -> ProjectSetup {
    let mut setup = project_setup(destinations);
    setup.project = "retention-acceptance".into();
    setup
        .components
        .retain(|name, _| name == &component("frontend"));
    let environment = setup.environments.get_mut("acceptance").unwrap();
    environment
        .components
        .retain(|name, _| name == &component("frontend"));
    let target = environment
        .components
        .get_mut(&component("frontend"))
        .unwrap();
    target.root = Some(format!(
        "/srv/shipforge-acceptance/retention-{}",
        uuid::Uuid::now_v7()
    ));
    target.health = None;
    setup
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
