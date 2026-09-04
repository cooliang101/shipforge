//! Production application-service coverage on the same disposable loopback SSH server.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::Path,
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use shipforge::{
    application::{DeploymentSelection, DeploymentService, GitWorktreeState},
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, CredentialRegistry, DestinationRegistry,
        DestinationSettings, EnvironmentSetup, HostKeyFingerprint, ProjectSetup, SshCredential,
        TargetSetup, initialize,
    },
    domain::{ComponentName, DeploymentState, DestinationKey, ReleaseManifest, ReleaseVersion},
    drivers::{DriverLog, DriverRegistry, EventSink, linux_ssh::LinuxSshDriver},
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

const REMOTE_ROOT: &str = "/srv/shipforge/service-fixture/production/api";

#[derive(Default)]
struct Events {
    events: Mutex<Vec<DriverLog>>,
    history: std::path::PathBuf,
}

impl EventSink for Events {
    fn emit(&self, event: DriverLog) {
        if event.namespace == "build.started" {
            let database = rusqlite::Connection::open(&self.history).unwrap();
            let pending: i64 = database.query_row(
                "SELECT count(*) FROM operation_intents WHERE stage='build-package' AND status='pending'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                pending, 1,
                "build intent must exist before the build starts"
            );
        }
        self.events.lock().unwrap().push(event);
    }
}

pub async fn validate(
    address: std::net::SocketAddr,
    host_fingerprint: &str,
    directory: &Path,
    transfer: &Arc<AsyncMutex<super::TransferState>>,
    commands: &Arc<Mutex<Vec<String>>>,
) {
    let project = directory.join("service-project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("main.rs"),
        "fn main() { let unused = 1; println!(\"fixture\"); }\n",
    )
    .unwrap();
    let mut credentials = CredentialRegistry::new();
    let credential = credentials
        .create(SshCredential::IdentityFile {
            path: directory.join("driver_id_ed25519"),
        })
        .unwrap();
    let destination = DestinationKey::new();
    let mut destinations = DestinationRegistry::new();
    destinations
        .create(
            destination.clone(),
            DestinationSettings::LinuxSsh {
                host: "127.0.0.1".into(),
                port: address.port(),
                user: "deploy".into(),
                credential,
                host_key: HostKeyFingerprint::parse(host_fingerprint).unwrap(),
            },
        )
        .unwrap();
    let destinations_path = directory.join("service-destinations.yaml");
    destinations.save(&destinations_path).unwrap();
    let config = initialize(&project, setup(destination)).unwrap();
    let revision = initialize_git(&project).await;
    let mut registry = DriverRegistry::default();
    registry
        .register(Arc::new(LinuxSshDriver::new(Arc::new(credentials))))
        .unwrap();
    let history = directory.join("service-history/history.sqlite3");
    let service = DeploymentService::new(Arc::new(registry), history.clone());
    let component = ComponentName::parse("api").unwrap();
    let cancellation = CancellationToken::new();
    let plan = service
        .plan(
            DeploymentSelection {
                project_root: project.clone(),
                config,
                environment: "production".into(),
                components: BTreeSet::from([component.clone()]),
            },
            &destinations,
            &cancellation,
        )
        .await
        .unwrap();
    assert_eq!(plan.git, GitWorktreeState::Clean);
    assert_eq!(
        plan.git_metadata.revision.as_deref(),
        Some(revision.as_str())
    );
    assert_eq!(plan.entries.len(), 1);
    assert!(!plan.entries[0].notices.is_empty());
    assert!(
        !project.join("server.bin").exists(),
        "planning must not run rustc"
    );
    assert!(!history.exists(), "planning must not create a Deployment");
    assert!(!transfer.lock().await.directories.contains(REMOTE_ROOT));
    let version = plan.entries[0].release.clone();
    let events = Events {
        history: history.clone(),
        ..Events::default()
    };
    let report = service
        .execute(plan, &destinations_path, &events, &cancellation)
        .await
        .unwrap();
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(report.deployment.components.len(), 1);
    assert!(report.failure.is_none());
    assert!(report.warnings.is_empty());
    assert!(project.join("server.bin").metadata().unwrap().len() > 0);
    let deployment = report.deployment.id.to_string();
    verify_history(&history, &deployment);
    verify_detailed_history(&history, &report.deployment.id, &revision, &component);
    verify_remote(&*transfer.lock().await, &version, &revision, &component);
    assert!(
        commands.lock().unwrap().iter().any(
            |command| command.contains(&format!("{REMOTE_ROOT}/temporary/{deployment}.tar.gz"))
        )
    );
    verify_events(&events);
}

fn verify_events(events: &Events) {
    let events = events.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.namespace == "build.stderr" && event.message.contains("unused"))
    );
    assert!(
        events
            .iter()
            .any(|event| event.namespace == "deployment.finished")
    );
}

fn verify_remote(
    state: &super::TransferState,
    version: &ReleaseVersion,
    revision: &str,
    component: &ComponentName,
) {
    assert_eq!(
        state.links.get(&format!("{REMOTE_ROOT}/current")),
        Some(&format!("releases/{version}"))
    );
    let bytes = &state.files[&format!("{REMOTE_ROOT}/releases/{version}/manifest.json")];
    let manifest: ReleaseManifest = serde_json::from_slice(bytes).unwrap();
    assert_eq!(manifest.source_revision.as_deref(), Some(revision));
    assert_eq!(&manifest.component, component);
    assert!(
        state
            .files
            .contains_key(&format!("{REMOTE_ROOT}/archives/{version}.tar.gz"))
    );
    assert!(
        !state
            .directories
            .iter()
            .any(|path| path.contains("/service-fixture/production/unused"))
    );
}

fn setup(destination: DestinationKey) -> ProjectSetup {
    let api = ComponentName::parse("api").unwrap();
    let unused = ComponentName::parse("unused").unwrap();
    ProjectSetup {
        project: "service-fixture".into(),
        components: BTreeMap::from([
            (
                api.clone(),
                ComponentSetup {
                    working_directory: Some(".".into()),
                    build: vec![BuildCommand::argv("rustc", ["main.rs", "-o", "server.bin"])],
                    artifact: ArtifactSpec {
                        path: "server.bin".into(),
                    },
                },
            ),
            (
                unused.clone(),
                ComponentSetup {
                    working_directory: Some(".".into()),
                    build: vec![BuildCommand::argv(
                        "shipforge-never-build-unselected",
                        std::iter::empty::<String>(),
                    )],
                    artifact: ArtifactSpec {
                        path: "unused.bin".into(),
                    },
                },
            ),
        ]),
        environments: BTreeMap::from([(
            "production".into(),
            EnvironmentSetup {
                components: BTreeMap::from([
                    (
                        api,
                        TargetSetup {
                            destination: destination.clone(),
                            root: Some(REMOTE_ROOT.into()),
                            systemd: None,
                            health: None,
                            after: Vec::new(),
                        },
                    ),
                    (
                        unused,
                        TargetSetup {
                            destination,
                            root: None,
                            systemd: None,
                            health: None,
                            after: Vec::new(),
                        },
                    ),
                ]),
            },
        )]),
    }
}

async fn initialize_git(project: &Path) -> String {
    // Keep fixture-only configuration outside the worktree so the production
    // planner can still verify that the freshly committed Project is clean.
    let isolation = project.parent().unwrap().join("git-fixture");
    std::fs::create_dir(&isolation).unwrap();
    std::fs::create_dir(isolation.join("template")).unwrap();
    let hooks = isolation.join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    std::fs::write(isolation.join("global-config"), []).unwrap();

    fixture_git(project, &isolation, ["init"]).await;
    fixture_git(
        project,
        &isolation,
        [
            OsStr::new("config"),
            OsStr::new("--local"),
            OsStr::new("core.hooksPath"),
            hooks.as_os_str(),
        ],
    )
    .await;
    fixture_git(
        project,
        &isolation,
        ["config", "--local", "core.fsmonitor", "false"],
    )
    .await;
    let default_hooks = project.join(".git/hooks");
    std::fs::create_dir_all(&default_hooks).unwrap();
    let rejected_hook = default_hooks.join("pre-commit");
    std::fs::write(&rejected_hook, "#!/bin/sh\nexit 23\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rejected_hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // A successful commit proves that the default hooks directory was not
    // used; it contains only this test-owned, deliberately failing hook.
    fixture_git(project, &isolation, ["add", "main.rs", "shipforge.yaml"]).await;
    fixture_git(
        project,
        &isolation,
        [
            "-c",
            "user.name=Protocol Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture",
        ],
    )
    .await;
    let output = fixture_git(project, &isolation, ["rev-parse", "HEAD"]).await;
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn fixture_git<I, S>(project: &Path, isolation: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut hooks = OsString::from("core.hooksPath=");
    hooks.push(isolation.join("hooks"));
    let mut template = OsString::from("init.templateDir=");
    template.push(isolation.join("template"));
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-c")
        .arg(hooks)
        .arg("-c")
        .arg(template)
        .args(args)
        .current_dir(project)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", isolation.join("global-config"));
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("each isolated fixture Git command must finish within 10 seconds")
        .expect("spawn isolated fixture Git command");
    assert!(
        output.status.success(),
        "fixture git setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn verify_detailed_history(
    history: &Path,
    deployment: &shipforge::domain::DeploymentId,
    revision: &str,
    component: &ComponentName,
) {
    let store = shipforge::history::HistoryStore::open(history).unwrap();
    let snapshots = store.component_snapshots(deployment).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(&snapshots[0].release.component, component);
    assert!(snapshots[0].expected_current.is_none());
    let metadata = store.deployment_metadata(deployment).unwrap().unwrap();
    assert_eq!(metadata.git_revision.as_deref(), Some(revision));
    assert_eq!(
        metadata.git_worktree,
        shipforge::history::GitWorktree::Clean
    );
    let packages = store.release_packages(deployment).unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(
        packages[0].manifest.source_revision.as_deref(),
        Some(revision)
    );
    assert_eq!(packages[0].release, snapshots[0].release);
    assert!(packages[0].size > 0);
    assert_eq!(packages[0].sha256.len(), 64);
    let receipts = store.release_receipts(deployment).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].release, packages[0].release);
    let observed = store.observations(deployment).unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].observed, Ok(Some(packages[0].release.clone())));
    assert_eq!(observed[0].healthy, Some(true));
    let steps = store.steps(deployment).unwrap();
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[3].status, shipforge::history::StepStatus::Skipped);
}

fn verify_history(history: &Path, deployment: &str) {
    let database = rusqlite::Connection::open(history).unwrap();
    let mut statement = database
        .prepare("SELECT deployment_id,stage,status FROM operation_intents ORDER BY id")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().map(|row| row.1.as_str()).collect::<Vec<_>>(),
        ["build-package", "prepare", "activate"]
    );
    assert!(
        rows.iter()
            .all(|row| row.0 == deployment && row.2 == "succeeded")
    );
    let log: String = database
        .query_row(
            "SELECT relative_path FROM deployment_logs WHERE deployment_id=?1",
            [deployment],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(log, format!("logs/{deployment}.log"));
    let contents = std::fs::read_to_string(history.parent().unwrap().join(log)).unwrap();
    assert!(contents.contains(deployment));
    assert!(contents.contains("[build.stderr]"));
    assert!(contents.contains("unused"));
    assert!(contents.contains("[deployment.finished]"));
}
