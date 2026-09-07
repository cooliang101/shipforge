//! Opt-in writes only to an independently pinned, run-scoped WSL systemd fixture.

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf, sync::Arc, time::Duration};

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
        linux_ssh::{
            HealthCheckOptions, LinuxSshDestination, LinuxSshDriver, LinuxSshTarget,
            connect_authenticated,
        },
    },
    history::HistoryStore,
    telemetry::{CommandArgument, CommandSpec, Redactor},
};
use tokio_util::sync::CancellationToken;

struct Fixture {
    _scratch: tempfile::TempDir,
    run_id: String,
    project: PathBuf,
    config: ProjectConfig,
    destinations: DestinationRegistry,
    destinations_path: PathBuf,
    history: PathBuf,
    destination: LinuxSshDestination,
    identity: PathBuf,
    driver: Arc<LinuxSshDriver>,
    service: DeploymentService,
}

impl Fixture {
    async fn new() -> Self {
        assert_eq!(required_env("SHIPFORGE_SYSTEMD_ACCEPTANCE"), "1");
        let run_id = fixture_run_id();
        let port = required_env("SHIPFORGE_TEST_SSH_PORT")
            .parse::<u16>()
            .expect("explicit loopback SSH port");
        assert_ne!(port, 0);
        let identity = PathBuf::from(required_env("SHIPFORGE_TEST_SSH_IDENTITY_FILE"));
        assert!(identity.is_absolute() && identity.is_file());
        let mut credentials = CredentialRegistry::new();
        let credential = credentials
            .create(SshCredential::IdentityFile {
                path: identity.clone(),
            })
            .unwrap();
        let key = DestinationKey::new();
        let mut destinations = DestinationRegistry::new();
        destinations
            .create(
                key.clone(),
                DestinationSettings::LinuxSsh {
                    host: "127.0.0.1".into(),
                    port,
                    user: "root".into(),
                    credential,
                    host_key: HostKeyFingerprint::parse(required_env(
                        "SHIPFORGE_TEST_SSH_HOST_KEY",
                    ))
                    .unwrap(),
                },
            )
            .unwrap();
        let destination =
            LinuxSshDestination::validate(&destinations.resolve(&key).unwrap().resolve().settings)
                .unwrap();
        let marker = remote_text(
            &destination,
            &identity,
            "cat",
            &["--", &format!("{}/marker", remote_base(&run_id))],
        )
        .await;
        assert_eq!(
            marker.trim(),
            format!("shipforge-systemd-v1:{run_id}"),
            "run-specific marker must be verified over pinned SSH before any writes"
        );
        assert_eq!(
            remote_text(&destination, &identity, "id", &["-u"])
                .await
                .trim(),
            "0"
        );
        for name in ["worker", "first"] {
            assert_eq!(
                remote_text(
                    &destination,
                    &identity,
                    "systemctl",
                    &[
                        "show",
                        "--property=LoadState",
                        "--value",
                        "--",
                        &unit_name(&run_id, name),
                    ]
                )
                .await
                .trim(),
                "loaded",
                "run-specific fixture unit must already be loaded before any deployment writes"
            );
        }

        let scratch = tempfile::tempdir().unwrap();
        let project = scratch.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let config = initialize(&project, project_setup(&key, &run_id)).unwrap();
        let driver = Arc::new(LinuxSshDriver::new(Arc::new(credentials)));
        let mut registry = DriverRegistry::default();
        registry.register(driver.clone()).unwrap();
        let history = scratch.path().join("history.sqlite3");
        let service = DeploymentService::new(Arc::new(registry), history.clone());
        let destinations_path = scratch.path().join("destinations.yaml");
        destinations.save(&destinations_path).unwrap();
        Self {
            _scratch: scratch,
            run_id,
            project,
            config,
            destinations,
            destinations_path,
            history,
            destination,
            identity,
            driver,
            service,
        }
    }

    fn payload(&self, name: &str, unstable: bool) -> String {
        assert!(matches!(name, "worker" | "first"));
        let path = self.project.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let mode = if unstable { "unstable" } else { "healthy" };
        let token = format!("{mode}:{}:{name}:{}", self.run_id, uuid::Uuid::now_v7());
        let evidence = format!("{}/evidence/{name}.{mode}", remote_base(&self.run_id));
        // All interpolated values are constants, a validated run ID, or a UUID.
        // Only the dedicated nobody-writable evidence directory is modified.
        let mut script =
            format!("#!/bin/sh\nset -eu\nprintf '%s\\n' '{token}' >> '{evidence}-starts'\n");
        if unstable {
            writeln!(
                script,
                "sleep 3\nprintf '%s\\n' '{token}' >> '{evidence}-exits'\nexit 1"
            )
            .unwrap();
        } else {
            script.push_str("exec sleep 300\n");
        }
        std::fs::write(path.join("worker.sh"), script).unwrap();
        std::fs::write(path.join("version.txt"), &token).unwrap();
        token
    }

    async fn deploy(&self, name: &str) -> DeploymentReport {
        let cancellation = CancellationToken::new();
        let plan = self
            .service
            .plan(
                DeploymentSelection {
                    project_root: self.project.clone(),
                    config: self.config.clone(),
                    environment: "acceptance".into(),
                    components: [component(name)].into_iter().collect(),
                },
                &self.destinations,
                &cancellation,
            )
            .await
            .expect("real systemd read-only deployment plan");
        assert_eq!(plan.entries.len(), 1);
        let report = self
            .service
            .execute(plan, &self.destinations_path, &Progress, &cancellation)
            .await
            .expect("real systemd deployment report");
        assert!(report.warnings.is_empty(), "{report:#?}");
        assert_eq!(report.deployment.components.len(), 1);
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

    async fn current(&self, name: &str) -> Option<ReleaseRef> {
        self.driver
            .current(&self.context(name))
            .await
            .expect("observe actual current link and Release manifest")
    }

    async fn active_state(&self, name: &str) -> String {
        remote_text(
            &self.destination,
            &self.identity,
            "systemctl",
            &[
                "show",
                "--property=ActiveState",
                "--value",
                "--",
                &unit_name(&self.run_id, name),
            ],
        )
        .await
        .trim()
        .to_owned()
    }

    async fn evidence(&self, name: &str, kind: &str) -> Vec<String> {
        assert!(matches!(name, "worker" | "first"));
        assert!(matches!(
            kind,
            "healthy-starts" | "unstable-starts" | "unstable-exits"
        ));
        remote_text(
            &self.destination,
            &self.identity,
            "cat",
            &[
                "--",
                &format!("{}/evidence/{name}.{kind}", remote_base(&self.run_id)),
            ],
        )
        .await
        .lines()
        .map(str::to_owned)
        .collect()
    }

    async fn main_pid(&self, name: &str) -> u64 {
        remote_text(
            &self.destination,
            &self.identity,
            "systemctl",
            &[
                "show",
                "--property=MainPID",
                "--value",
                "--",
                &unit_name(&self.run_id, name),
            ],
        )
        .await
        .trim()
        .parse()
        .expect("systemd MainPID is an unsigned integer")
    }

    async fn assert_running_payload(&self, name: &str, token: &str) {
        // Compensation restarts the old service without a second health window.
        // Independently verify the real process, not just its restored symlink.
        let target = &self.config.environments["acceptance"].components[&component(name)];
        let target = LinuxSshTarget::validate(&target.driver_input()).unwrap();
        let cancellation = CancellationToken::new();
        let session = connect_authenticated(
            &self.destination,
            &SshCredential::IdentityFile {
                path: self.identity.clone(),
            },
            Duration::from_secs(10),
            &cancellation,
        )
        .await
        .expect("authenticate to independently verify the running service");
        let health = session
            .check_health(&target, HealthCheckOptions::default(), &cancellation)
            .await
            .expect("running payload must remain stable for the default ten-second window");
        let systemd = health.systemd.expect("real systemd health report");
        assert_eq!(systemd.unit, unit_name(&self.run_id, name));
        assert!(systemd.observations >= 2);
        session.disconnect().await.unwrap();
        assert_eq!(self.active_state(name).await, "active");
        assert_ne!(self.main_pid(name).await, 0);
        let starts = self.evidence(name, "healthy-starts").await;
        assert_eq!(starts.last().map(String::as_str), Some(token));
    }

    async fn assert_stopped(&self, name: &str) {
        assert_eq!(self.active_state(name).await, "inactive");
        assert_eq!(self.main_pid(name).await, 0);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(self.active_state(name).await, "inactive");
        assert_eq!(
            self.main_pid(name).await,
            0,
            "stopped service must not restart"
        );
    }

    async fn assert_unstable_payload(&self, name: &str, token: &str, restarted: bool) {
        let starts = self.evidence(name, "unstable-starts").await;
        let exits = self.evidence(name, "unstable-exits").await;
        assert!(!starts.is_empty(), "unhealthy payload must actually run");
        assert!(
            !exits.is_empty(),
            "unhealthy payload must actually exit unsuccessfully"
        );
        assert!(starts.iter().chain(&exits).all(|line| line == token));
        if restarted {
            assert!(
                starts.len() >= 2,
                "NRestarts increase must have independent startup evidence"
            );
        }
        eprintln!(
            "{name}: observed {} unhealthy starts and {} deliberate exits",
            starts.len(),
            exits.len()
        );
    }

    async fn rollback(&self, source: &DeploymentId, name: &str, target: Option<ReleaseRef>) {
        let expected_current = self
            .current(name)
            .await
            .expect("rollback source is deployed");
        let history = HistoryStore::open(&self.history).unwrap();
        let report = RollbackOrchestrator::new(&history, Redactor::default())
            .rollback(
                source,
                vec![RollbackComponent {
                    driver: self.driver.clone(),
                    context: self.context(name),
                    expected_current,
                    target: target.clone(),
                }],
                &[component(name)],
                &CancellationToken::new(),
            )
            .await
            .expect("explicit rollback of real systemd service");
        assert_eq!(
            report.deployment.state,
            DeploymentState::Succeeded,
            "{report:#?}"
        );
        assert!(report.failure.is_none());
        assert!(report.compensation_failures.is_empty());
        assert_eq!(self.current(name).await, target);
    }
}

struct Progress;

impl EventSink for Progress {
    fn emit(&self, event: DriverLog) {
        if !event.namespace.starts_with("build.std") {
            eprintln!("{}: {}", event.namespace, event.message);
        }
    }
}

#[tokio::test]
#[ignore = "requires an explicitly provisioned run-scoped WSL systemd/SSH fixture"]
async fn real_systemd_stability_failure_compensation_and_explicit_rollback() {
    tokio::time::timeout(Duration::from_secs(600), run_acceptance())
        .await
        .expect("systemd acceptance exceeded its ten-minute deadline");
}

async fn run_acceptance() {
    assert_eq!(
        HealthCheckOptions::default().stable_for,
        Duration::from_secs(10)
    );
    let fixture = Fixture::new().await;
    for name in ["worker", "first"] {
        assert!(fixture.current(name).await.is_none());
        assert_eq!(fixture.active_state(name).await, "inactive");
    }

    let original_token = fixture.payload("worker", false);
    let original = fixture.deploy("worker").await;
    assert_success(&original, "worker");
    let original_release = fixture.current("worker").await.unwrap();
    assert_eq!(
        original.deployment.components[&component("worker")]
            .observed_release
            .as_ref(),
        Some(&original_release.version)
    );
    fixture
        .assert_running_payload("worker", &original_token)
        .await;
    assert!(fixture.current("first").await.is_none());

    let second_token = fixture.payload("worker", false);
    let second = fixture.deploy("worker").await;
    assert_success(&second, "worker");
    assert_ne!(
        fixture.current("worker").await.as_ref(),
        Some(&original_release)
    );
    fixture
        .assert_running_payload("worker", &second_token)
        .await;
    fixture
        .rollback(
            &second.deployment.id,
            "worker",
            Some(original_release.clone()),
        )
        .await;
    fixture
        .assert_running_payload("worker", &original_token)
        .await;

    let unstable_token = fixture.payload("worker", true);
    let failed_update = fixture.deploy("worker").await;
    let restarted = assert_unstable_failure(&failed_update, "worker", &fixture.run_id);
    assert_eq!(
        fixture.current("worker").await.as_ref(),
        Some(&original_release)
    );
    assert_eq!(
        failed_update.deployment.components[&component("worker")]
            .observed_release
            .as_ref(),
        Some(&original_release.version)
    );
    fixture
        .assert_running_payload("worker", &original_token)
        .await;
    fixture
        .assert_unstable_payload("worker", &unstable_token, restarted)
        .await;

    let first_token = fixture.payload("first", true);
    let failed_first = fixture.deploy("first").await;
    let restarted = assert_unstable_failure(&failed_first, "first", &fixture.run_id);
    assert!(fixture.current("first").await.is_none());
    assert!(
        failed_first.deployment.components[&component("first")]
            .observed_release
            .is_none()
    );
    fixture.assert_stopped("first").await;
    fixture
        .assert_unstable_payload("first", &first_token, restarted)
        .await;
    fixture
        .assert_running_payload("worker", &original_token)
        .await;

    fixture
        .rollback(&original.deployment.id, "worker", None)
        .await;
    fixture.assert_stopped("worker").await;
    assert!(fixture.current("first").await.is_none());
    assert_history(&fixture.history);
}

fn assert_success(report: &DeploymentReport, name: &str) {
    assert_eq!(
        report.deployment.state,
        DeploymentState::Succeeded,
        "{report:#?}"
    );
    assert!(report.failure.is_none());
    assert!(report.compensation_failures.is_empty());
    let result = &report.deployment.components[&component(name)];
    assert_eq!(result.outcome, ComponentOutcome::Succeeded);
    assert!(result.attempted_release.is_some());
    assert_eq!(result.attempted_release, result.observed_release);
}

/// Returns whether the real systemd observation reported an increased restart
/// counter. Seeing the brief inactive interval is also a valid instability.
fn assert_unstable_failure(report: &DeploymentReport, name: &str, run_id: &str) -> bool {
    assert_eq!(
        report.deployment.state,
        DeploymentState::Failed,
        "{report:#?}"
    );
    assert!(report.compensation_failures.is_empty());
    let result = &report.deployment.components[&component(name)];
    assert_eq!(result.outcome, ComponentOutcome::Failed);
    assert!(result.attempted_release.is_some());
    match report.failure.as_ref() {
        Some(DeploymentFailure::Driver {
            component: failed_component,
            stage,
            error,
            ..
        }) => {
            assert_eq!(failed_component, &component(name));
            assert_eq!(*stage, OrchestrationStage::Activate);
            assert_eq!(error.stage, "health", "{report:#?}");
            assert!(
                error
                    .message
                    .contains("health check failed and activation was compensated"),
                "{report:#?}"
            );
            assert!(
                error.message.contains(&format!(
                    "systemd unit `{}` was unstable:",
                    unit_name(run_id, name)
                )),
                "{report:#?}"
            );
            let (baseline, observed) = error
                .message
                .split_once("restart baseline=")
                .and_then(|(_, counters)| counters.split_once(", observed="))
                .map(|(baseline, observed)| {
                    (
                        baseline.parse::<u64>().unwrap(),
                        observed.parse::<u64>().unwrap(),
                    )
                })
                .expect("systemd instability must expose the observed restart counters");
            assert!(
                observed > baseline || error.message.contains("active=false"),
                "{report:#?}"
            );
            eprintln!("{name}: {}", error.message);
            observed > baseline
        }
        other => panic!("expected concrete systemd instability, got {other:#?}"),
    }
}

fn assert_history(path: &std::path::Path) {
    let database = rusqlite::Connection::open(path).unwrap();
    for (query, expected) in [
        (
            "SELECT count(*) FROM operation_intents WHERE status='pending'",
            0,
        ),
        (
            "SELECT count(*) FROM deployments WHERE state IN ('created','running')",
            0,
        ),
        ("SELECT count(*) FROM deployments", 6),
        ("SELECT count(*) FROM deployments WHERE state='failed'", 2),
        (
            "SELECT count(*) FROM deployments WHERE state='succeeded'",
            4,
        ),
        (
            "SELECT count(*) FROM deployments WHERE kind='rollback' AND related_deployment_id IS NOT NULL",
            2,
        ),
    ] {
        let actual: i64 = database.query_row(query, [], |row| row.get(0)).unwrap();
        assert_eq!(actual, expected, "{query}");
    }
}

fn project_setup(destination: &DestinationKey, run_id: &str) -> ProjectSetup {
    let mut components = BTreeMap::new();
    let mut targets = BTreeMap::new();
    for name in ["worker", "first"] {
        components.insert(
            component(name),
            ComponentSetup {
                working_directory: Some(".".into()),
                build: vec![BuildCommand::argv("rustc", ["--version"])],
                artifact: ArtifactSpec { path: name.into() },
            },
        );
        targets.insert(
            component(name),
            TargetSetup {
                destination: destination.clone(),
                root: Some(format!("{}/{name}", remote_base(run_id))),
                service: Some(unit_name(run_id, name).into()),
                health: None,
                after: Vec::new(),
            },
        );
    }
    ProjectSetup {
        project: "systemd-acceptance".into(),
        components,
        environments: BTreeMap::from([(
            "acceptance".into(),
            EnvironmentSetup {
                components: targets,
            },
        )]),
    }
}

async fn remote_text(
    destination: &LinuxSshDestination,
    identity: &std::path::Path,
    program: &str,
    arguments: &[&str],
) -> String {
    let cancellation = CancellationToken::new();
    let session = connect_authenticated(
        destination,
        &SshCredential::IdentityFile {
            path: identity.to_owned(),
        },
        Duration::from_secs(10),
        &cancellation,
    )
    .await
    .expect("authenticate independently pinned disposable systemd fixture");
    let command = CommandSpec::structured(
        program,
        arguments
            .iter()
            .map(|argument| CommandArgument::plain(*argument)),
    )
    .unwrap();
    let output = session
        .execute(&command, Duration::from_secs(10), &cancellation)
        .await
        .expect("read systemd fixture evidence");
    assert_eq!(output.exit_status, 0, "read-only {program} failed");
    assert!(!output.stdout_truncated && !output.stderr_truncated);
    let text = String::from_utf8(output.stdout).unwrap();
    session.disconnect().await.unwrap();
    text
}

fn remote_base(run_id: &str) -> String {
    format!("/var/tmp/shipforge-systemd-{run_id}")
}
fn fixture_run_id() -> String {
    let run_id = required_env("SHIPFORGE_SYSTEMD_RUN_ID");
    assert!(
        run_id.len() == 32
            && run_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "fixture run ID must be 32 lowercase hexadecimal characters"
    );
    run_id
}
fn unit_name(run_id: &str, name: &str) -> String {
    format!("shipforge-{run_id}-{name}.service")
}
fn component(name: &str) -> ComponentName {
    ComponentName::parse(name).unwrap()
}
fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set by the disposable systemd fixture runner"))
}
