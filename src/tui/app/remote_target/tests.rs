use super::*;

mod interaction;

use super::super::{ComponentSetupState, CredentialChoice, NewSshDestinationState, SshField};
use crate::{
    application::{
        DestinationSetupError, DestinationSetupGateway, DestinationSetupRequest,
        DestinationSetupService, EndpointProbeRequest, LocalIdentityCandidate,
    },
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, CredentialRegistry, DestinationRegistry,
        DestinationSettings, HostKeyFingerprint, SshCredential,
    },
    domain::DestinationKey,
    drivers::DriverKind,
    projects::{ComponentCandidate, DiscoveryConfidence, DiscoveryReport},
};
use async_trait::async_trait;
use crossterm::event::KeyModifiers;
use ratatui::{Terminal, backend::TestBackend};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
struct Gateway {
    behavior: AtomicUsize,
    calls: AtomicUsize,
    finished: AtomicBool,
    cancelled: AtomicBool,
    release: Notify,
    paths: Mutex<Vec<String>>,
}

impl Gateway {
    async fn observe(
        &self,
        request: &DestinationSetupRequest,
        cancellation: &CancellationToken,
    ) -> Result<(), DestinationSetupError> {
        self.paths.lock().unwrap().push(request.remote_root.clone());
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            request.destination.value["hostKey"],
            "SHA256:pinned-fixture"
        );
        assert_eq!(request.destination.value["host"], "fixture.invalid");
        assert_eq!(
            request.credential.downcast_ref::<SshCredential>(),
            Some(&SshCredential::Agent {
                fingerprint: "SHA256:fixture-agent".into()
            })
        );
        match self.behavior.load(Ordering::SeqCst) {
            1 => {
                cancellation.cancelled().await;
                self.cancelled.store(true, Ordering::SeqCst);
                self.release.notified().await;
            }
            2 => {
                return Err(DestinationSetupError::operation(
                    "private-stage",
                    "private-protocol-sentinel",
                ));
            }
            3 => panic!("private-panic-sentinel"),
            _ => {}
        }
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl DestinationSetupGateway for Gateway {
    fn driver_kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }
    async fn discover_local_identities(
        &self,
        _: &CancellationToken,
    ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError> {
        panic!("saved target selection cannot discover identities");
    }
    async fn capture_endpoint_identity(
        &self,
        _: &EndpointProbeRequest,
        _: Duration,
        _: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        panic!("saved target selection cannot replace the pin");
    }
    async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        connect: Duration,
        command: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        assert_eq!(connect, Duration::from_secs(10));
        assert_eq!(command, Duration::from_secs(10));
        self.observe(request, cancellation).await?;
        Ok(RemoteSetupCandidates {
            root: SetupRootState::WritableDirectory,
            services: if self.behavior.load(Ordering::SeqCst) == 4 {
                vec!["private-invalid\n.service".into()]
            } else {
                vec!["api.service".into(), "worker.service".into()]
            },
            notices: Vec::new(),
        })
    }
    async fn browse_directories(
        &self,
        request: &DestinationSetupRequest,
        connect: Duration,
        command: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteDirectoryCandidates, DestinationSetupError> {
        assert_eq!(connect, Duration::from_secs(10));
        assert_eq!(command, Duration::from_secs(10));
        self.observe(request, cancellation).await?;
        let children = match request.remote_root.as_str() {
            "/" => vec!["/srv", "/var"],
            "/srv" => vec!["/srv/api", "/srv/worker"],
            _ => Vec::new(),
        };
        Ok(RemoteDirectoryCandidates {
            directory: request.remote_root.clone(),
            directories: children.into_iter().map(str::to_owned).collect(),
        })
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    project: PathBuf,
    app: App,
    gateway: Arc<Gateway>,
    destination_bytes: Vec<u8>,
    credential_bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("demo");
        fs::create_dir(&project).unwrap();
        let mut credentials = CredentialRegistry::new();
        let credential = credentials
            .create(SshCredential::Agent {
                fingerprint: "SHA256:fixture-agent".into(),
            })
            .unwrap();
        credentials
            .save(&directory.path().join("credentials.yaml"))
            .unwrap();
        let key = DestinationKey::new();
        let mut destinations = DestinationRegistry::new();
        destinations
            .create(
                key.clone(),
                DestinationSettings::LinuxSsh {
                    host: "fixture.invalid".into(),
                    port: 22,
                    user: "deploy".into(),
                    credential,
                    host_key: HostKeyFingerprint::parse("SHA256:pinned-fixture").unwrap(),
                },
            )
            .unwrap();
        destinations
            .save(&directory.path().join("destinations.yaml"))
            .unwrap();
        let setup = DestinationSetupState {
            components: ComponentSetupState {
                root: project.clone(),
                report: DiscoveryReport {
                    components: ["backend", "worker"]
                        .into_iter()
                        .map(|value| ComponentCandidate {
                            name: name(value),
                            source: project.clone(),
                            confidence: DiscoveryConfidence::Low,
                            setup: ComponentSetup {
                                working_directory: Some(".".into()),
                                build: vec![BuildCommand::argv(
                                    "never-execute-build-sentinel",
                                    [value],
                                )],
                                artifact: ArtifactSpec {
                                    path: format!("target/{value}").into(),
                                },
                            },
                        })
                        .collect(),
                    notices: Vec::new(),
                },
                selected: BTreeSet::from([name("backend"), name("worker")]),
                cursor: 0,
            },
            destinations: destinations.summaries(),
            assignments: BTreeMap::from([(name("backend"), key.clone()), (name("worker"), key)]),
            target_settings: BTreeMap::from([
                (
                    name("backend"),
                    ComponentTargetSettings {
                        root: Some("/srv/original-api".into()),
                        service: Some("original-api.service".into()),
                    },
                ),
                (
                    name("worker"),
                    ComponentTargetSettings {
                        root: Some("/srv/original-worker".into()),
                        service: Some("original-worker.service".into()),
                    },
                ),
            ]),
            component_cursor: 0,
            destination_cursor: 0,
        };
        let gateway = Arc::new(Gateway::default());
        let mut app = App::new_with_setup(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            &project,
            DestinationSetupService::new(gateway.clone()),
        )
        .unwrap();
        app.screen = Screen::SetupDestinations(setup);
        let destination_bytes = fs::read(&app.destination_registry_path).unwrap();
        let credential_bytes = fs::read(&app.credential_registry_path).unwrap();
        Self {
            directory,
            project,
            app,
            gateway,
            destination_bytes,
            credential_bytes,
        }
    }
    fn open(&mut self) {
        self.app.handle_key(key(KeyCode::Char('e')));
    }
    fn selection(&self) -> &RemoteSetupSelectionState {
        let Screen::RemoteSetupSelection(screen) = &self.app.screen else {
            panic!("expected target choices");
        };
        screen
    }
    fn assert_read_only(&self) {
        assert_eq!(
            fs::read(&self.app.destination_registry_path).unwrap(),
            self.destination_bytes
        );
        assert_eq!(
            fs::read(&self.app.credential_registry_path).unwrap(),
            self.credential_bytes
        );
        assert!(!self.project.join("shipforge.yaml").exists());
        assert!(!self.app.registry_path.exists());
        assert!(!self.directory.path().join("history.sqlite3").exists());
        assert_eq!(fs::read_dir(&self.project).unwrap().count(), 0);
    }
}

fn name(value: &str) -> ComponentName {
    ComponentName::parse(value).unwrap()
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn type_text(app: &mut App, value: &str) {
    app.handle_key(key(KeyCode::Delete));
    for character in value.chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "tracked worker did not reach expected state"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
async fn finish(app: &mut App) {
    wait_for(|| {
        app.poll_background();
        app.remote_target_task.is_none()
    })
    .await;
}
fn rendered(screen: &RemoteSetupSelectionState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rendered_app(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::tui::render(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn browsing_root_is_allowed_but_deployment_requires_a_component_directory() {
    assert!(valid_path("/", true));
    assert!(!valid_path("/", false));
    for path in [
        "relative",
        "/srv/../app",
        "/srv/./app",
        "/srv//app",
        "/srv/app/",
        "/srv/a\n",
        "/srv/\u{202e}app",
    ] {
        assert!(!valid_path(path, true), "{path:?}");
    }
    assert!(valid_path("/srv/project's app", false));
    assert!(valid_path("/srv/应用", false));
}

#[test]
fn manual_service_accepts_a_unit_not_a_shell_command() {
    for unit in ["", "api.service", "worker@blue.service"] {
        assert!(valid_service(unit));
    }
    for unit in [
        "systemctl restart api",
        "api.service;whoami",
        "-x.service",
        "api.service\n",
        "x.socket",
    ] {
        assert!(!valid_service(unit));
    }
}
