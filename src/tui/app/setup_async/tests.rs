use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::Notify;

use crate::{
    application::{DestinationSetupError, DestinationSetupGateway, SetupRootState},
    config::{ArtifactSpec, BuildCommand, ComponentSetup},
    domain::ComponentName,
    drivers::DriverKind,
    projects::{ComponentCandidate, DiscoveryConfidence, DiscoveryReport},
};

use super::super::{ComponentSetupState, CredentialChoice, DestinationSetupState, SshField};
use super::*;

#[derive(Debug, Default)]
struct ControlledSetup {
    behavior: AtomicUsize,
    started: AtomicUsize,
    cancelled: AtomicBool,
    finished: AtomicBool,
    release: Notify,
    host_calls: AtomicUsize,
    authentication_calls: AtomicUsize,
}

impl ControlledSetup {
    async fn stage(
        &self,
        stage: usize,
        cancellation: &CancellationToken,
    ) -> Result<(), DestinationSetupError> {
        self.started.store(stage, Ordering::SeqCst);
        let behavior = self.behavior.load(Ordering::SeqCst);
        if behavior == stage {
            cancellation.cancelled().await;
            self.cancelled.store(true, Ordering::SeqCst);
            self.release.notified().await;
        } else if behavior == stage + 10 {
            panic!("private-setup-panic-sentinel");
        } else if behavior == stage + 20 {
            return Err(DestinationSetupError::operation(
                "private-stage-sentinel",
                "private-parser-secret-sentinel",
            ));
        } else if behavior == stage + 30 {
            std::future::pending::<()>().await;
        }
        self.finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl DestinationSetupGateway for ControlledSetup {
    fn driver_kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }

    async fn discover_local_identities(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError> {
        self.stage(1, cancellation).await?;
        Ok(vec![LocalIdentityCandidate {
            reference: "SHA256:agent-key".into(),
            label: "controlled Agent".into(),
        }])
    }

    async fn capture_endpoint_identity(
        &self,
        _request: &EndpointProbeRequest,
        _timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        self.host_calls.fetch_add(1, Ordering::SeqCst);
        self.stage(2, cancellation).await?;
        Ok("SHA256:confirmed-test-key".into())
    }

    async fn authenticate_and_probe(
        &self,
        _request: &DestinationSetupRequest,
        _connect_timeout: Duration,
        _command_timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        self.authentication_calls.fetch_add(1, Ordering::SeqCst);
        self.stage(3, cancellation).await?;
        Ok(candidates())
    }
}

fn candidates() -> RemoteSetupCandidates {
    RemoteSetupCandidates {
        root: SetupRootState::Missing,
        services: Vec::new(),
        notices: Vec::new(),
    }
}

fn fixture() -> (
    tempfile::TempDir,
    App,
    NewSshDestinationState,
    Arc<ControlledSetup>,
) {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Arc::new(ControlledSetup::default());
    let component = ComponentName::parse("worker").unwrap();
    let draft = NewSshDestinationState {
        identity_request: None,
        destinations: DestinationSetupState {
            components: ComponentSetupState {
                root: directory.path().to_owned(),
                report: DiscoveryReport {
                    components: vec![ComponentCandidate {
                        name: component.clone(),
                        source: directory.path().to_owned(),
                        confidence: DiscoveryConfidence::Low,
                        setup: ComponentSetup {
                            working_directory: Some(PathBuf::from(".")),
                            build: vec![BuildCommand::argv("cargo", ["build"])],
                            artifact: ArtifactSpec {
                                path: PathBuf::from("target/worker"),
                            },
                        },
                    }],
                    notices: Vec::new(),
                },
                selected: BTreeSet::from([component]),
                cursor: 0,
            },
            destinations: Vec::new(),
            assignments: BTreeMap::new(),
            target_settings: BTreeMap::new(),
            component_cursor: 0,
            destination_cursor: 0,
        },
        connections: Vec::new(),
        connection_cursor: 0,
        host: "test.example.invalid".into(),
        user: "deploy".into(),
        port: "22".into(),
        field: SshField::Credential,
        credentials: vec![CredentialChoice::Agent {
            fingerprint: "SHA256:agent-key".into(),
            label: "fixture Agent".into(),
        }],
        credential_cursor: 0,
        agent_status: String::new(),
    };
    let service = DestinationSetupService::new(gateway.clone());
    let mut app = App::new_with_setup(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
        service,
    )
    .unwrap();
    app.screen = Screen::NewSshDestination(draft.clone());
    (directory, app, draft, gateway)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

async fn wait_started(gateway: &ControlledSetup, stage: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while gateway.started.load(Ordering::SeqCst) != stage {
        assert!(Instant::now() < deadline, "setup worker did not start");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn finish(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.setup_busy() {
        assert!(Instant::now() < deadline, "setup worker did not terminate");
        app.poll_background();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_capture_waits_before_retry_and_ignores_stale_request_results() {
    let (directory, mut app, draft, gateway) = fixture();
    gateway.behavior.store(2, Ordering::SeqCst);
    app.start_host_key_probe(&draft);
    wait_started(&gateway, 2).await;
    let old_id = app.setup_task.as_ref().unwrap().id;
    app.handle_key(key(KeyCode::Esc));
    assert!(app.setup_cancelling());
    assert!(matches!(
        app.screen,
        Screen::HostKeyPending {
            cancellation_requested: true,
            ..
        }
    ));
    for input in [KeyCode::Enter, KeyCode::F(4), KeyCode::Char('q')] {
        assert!(!app.handle_key(key(input)));
    }
    app.start_host_key_probe(&draft);
    assert_eq!(gateway.host_calls.load(Ordering::SeqCst), 1);
    app.poll_background();
    assert!(app.setup_busy());
    gateway.release.notify_one();
    finish(&mut app).await;
    assert!(gateway.cancelled.load(Ordering::SeqCst));
    assert!(matches!(app.screen, Screen::NewSshDestination(_)));
    gateway.behavior.store(0, Ordering::SeqCst);
    app.start_host_key_probe(&draft);
    let new_id = app.setup_task.as_ref().unwrap().id;
    assert_ne!(old_id, new_id);
    app.finish_setup_host_key(old_id, Ok("SHA256:stale-key".into()));
    assert_eq!(app.setup_task.as_ref().unwrap().id, new_id);
    finish(&mut app).await;
    let Screen::HostKeyConfirm { fingerprint, .. } = &app.screen else {
        panic!("expected new confirmation");
    };
    assert_eq!(fingerprint.as_str(), "SHA256:confirmed-test-key");
    assert!(!directory.path().join("destinations.yaml").exists());
    assert!(!directory.path().join("credentials.yaml").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_authentication_success_never_saves_connection_and_control_c_waits() {
    let (directory, mut app, draft, gateway) = fixture();
    gateway.behavior.store(3, Ordering::SeqCst);
    let fingerprint = HostKeyFingerprint::parse("SHA256:confirmed-test-key").unwrap();
    app.screen = Screen::HostKeyConfirm {
        draft: draft.clone(),
        fingerprint: fingerprint.clone(),
    };
    app.start_authentication(&draft, &fingerprint);
    wait_started(&gateway, 3).await;
    app.handle_key(key(KeyCode::F(1)));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.help_open);
    assert!(app.setup_cancelling());
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.help_open);
    assert!(matches!(
        app.screen,
        Screen::SshAuthenticationPending {
            cancellation_requested: true,
            ..
        }
    ));
    gateway.release.notify_one();
    finish(&mut app).await;
    assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));
    assert!(
        app.message
            .as_deref()
            .unwrap()
            .contains("No connection was saved")
    );
    assert!(!directory.path().join("destinations.yaml").exists());
    assert!(!directory.path().join("credentials.yaml").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_shutdown_waits_for_read_only_worker_termination() {
    let (directory, mut app, draft, gateway) = fixture();
    gateway.behavior.store(2, Ordering::SeqCst);
    app.start_host_key_probe(&draft);
    wait_started(&gateway, 2).await;
    let release = gateway.clone();
    let notifier = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        release.release.notify_one();
    });
    app.shutdown();
    notifier.join().unwrap();
    assert!(gateway.cancelled.load(Ordering::SeqCst));
    assert!(gateway.finished.load(Ordering::SeqCst));
    assert!(!app.setup_busy());
    assert!(!directory.path().join("destinations.yaml").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_worker_panic_and_untrusted_error_are_terminal_and_private() {
    for behavior in [12, 22] {
        let (_directory, mut app, draft, gateway) = fixture();
        gateway.behavior.store(behavior, Ordering::SeqCst);
        app.start_host_key_probe(&draft);
        finish(&mut app).await;
        assert!(matches!(app.screen, Screen::NewSshDestination(_)));
        let message = app.message.as_deref().unwrap();
        assert!(message.contains("Host Key capture"));
        assert!(!message.contains("sentinel"));
        assert!(!app.setup_busy());
    }
    for behavior in [13, 23] {
        let (directory, mut app, draft, gateway) = fixture();
        gateway.behavior.store(behavior, Ordering::SeqCst);
        app.start_authentication(
            &draft,
            &HostKeyFingerprint::parse("SHA256:trusted").unwrap(),
        );
        finish(&mut app).await;
        assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));
        assert!(!app.message.as_deref().unwrap().contains("sentinel"));
        assert!(!directory.path().join("destinations.yaml").exists());
    }
}

#[test]
fn stale_identity_event_cannot_modify_another_draft_or_consume_a_new_request() {
    let (_directory, mut app, mut draft, _) = fixture();
    let old_id = Uuid::now_v7();
    let new_id = Uuid::now_v7();
    draft.identity_request = Some(new_id);
    app.screen = Screen::NewSshDestination(draft);
    app.setup_task = Some(SetupTask::fixture(
        new_id,
        SetupKind::Identities,
        CancellationToken::new(),
    ));
    app.finish_setup_identities(
        old_id,
        Ok(vec![LocalIdentityCandidate {
            reference: "SHA256:wrong".into(),
            label: "stale identity".into(),
        }]),
    );
    assert_eq!(app.setup_task.as_ref().unwrap().id, new_id);
    app.screen = Screen::NewSshDestination(NewSshDestinationState {
        identity_request: None,
        ..match app.screen.clone() {
            Screen::NewSshDestination(draft) => draft,
            _ => unreachable!(),
        }
    });
    app.finish_setup_identities(
        new_id,
        Ok(vec![LocalIdentityCandidate {
            reference: "SHA256:wrong".into(),
            label: "other draft".into(),
        }]),
    );
    let Screen::NewSshDestination(draft) = &app.screen else {
        panic!("expected form");
    };
    assert_eq!(draft.credentials.len(), 1);
    assert_eq!(draft.credentials[0].label(), "fixture Agent");
    assert!(!app.setup_busy());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_discovery_is_tracked_cancellable_and_does_not_start_a_capture() {
    let (directory, mut app, _draft, gateway) = fixture();
    gateway.behavior.store(31, Ordering::SeqCst);
    app.start_agent_probe();
    wait_started(&gateway, 1).await;
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::F(4)));
    assert!(app.picker.is_none());
    assert_eq!(gateway.host_calls.load(Ordering::SeqCst), 0);
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    finish(&mut app).await;
    let Screen::NewSshDestination(draft) = &app.screen else {
        panic!("expected identity form");
    };
    assert!(draft.agent_status.contains("cancelled"));
    assert_eq!(draft.credentials.len(), 1);
    assert!(!directory.path().join("credentials.yaml").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_worker_panic_returns_a_terminal_static_status() {
    let (_directory, mut app, _draft, gateway) = fixture();
    gateway.behavior.store(11, Ordering::SeqCst);
    app.start_agent_probe();
    finish(&mut app).await;
    let Screen::NewSshDestination(draft) = &app.screen else {
        panic!("expected form");
    };
    assert!(draft.agent_status.contains("failed or timed out"));
    assert!(!draft.agent_status.contains("sentinel"));
}

#[tokio::test]
async fn unresponsive_identity_provider_has_a_finite_deadline() {
    let gateway = Arc::new(ControlledSetup::default());
    gateway.behavior.store(31, Ordering::SeqCst);
    let service = DestinationSetupService::new(gateway);
    let result = run_with_deadline(
        &service,
        SetupRequest::Identities,
        &CancellationToken::new(),
        Duration::from_millis(10),
    )
    .await;
    assert!(
        matches!(result, Err(message) if message.contains("timed out") && !message.contains("sentinel"))
    );
    assert_eq!(IDENTITY_TIMEOUT, Duration::from_secs(10));
}

#[tokio::test]
async fn setup_cancelled_before_start_does_not_call_the_gateway() {
    let gateway = Arc::new(ControlledSetup::default());
    let service = DestinationSetupService::new(gateway.clone());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = run(&service, SetupRequest::Identities, &cancellation).await;
    assert!(matches!(result, Err(message) if message.contains("cancelled before starting")));
    assert_eq!(gateway.started.load(Ordering::SeqCst), 0);
}

#[test]
fn unavailable_setup_runtime_leaves_form_and_starts_no_operation() {
    let (_directory, mut app, draft, gateway) = fixture();
    app.runtime = None;
    app.start_host_key_probe(&draft);
    assert!(!app.setup_busy());
    assert!(matches!(app.screen, Screen::NewSshDestination(_)));
    assert!(
        app.message
            .as_deref()
            .unwrap()
            .contains("runtime is unavailable")
    );
    assert_eq!(gateway.host_calls.load(Ordering::SeqCst), 0);
    app.start_agent_probe();
    let Screen::NewSshDestination(draft) = &app.screen else {
        panic!("expected form");
    };
    assert!(draft.agent_status.contains("unavailable"));
}

#[test]
fn help_escape_does_not_cancel_planning_but_control_c_does() {
    let (_directory, mut app, draft, _) = fixture();
    // A minimal valid selection is enough: no worker or remote request is started.
    let component = draft.destinations.components.report.components[0].clone();
    let destination = crate::domain::DestinationKey::new();
    let setup = crate::config::ProjectSetup {
        project: "test-project".into(),
        components: BTreeMap::from([(component.name.clone(), component.setup)]),
        environments: BTreeMap::from([(
            "production".into(),
            crate::config::EnvironmentSetup {
                components: BTreeMap::from([(
                    component.name,
                    crate::config::TargetSetup {
                        destination,
                        root: None,
                        service: None,
                        health: None,
                        after: Vec::new(),
                    },
                )]),
            },
        )]),
    };
    let config = crate::config::prepare_initialize(setup)
        .unwrap()
        .config()
        .clone();
    let cancellation = CancellationToken::new();
    app.screen = Screen::DeploymentPlanning {
        request_id: Uuid::now_v7(),
        cancellation: cancellation.clone(),
        selection: super::super::DeploySelectionState {
            root: draft.destinations.components.root,
            selected: config.components.keys().cloned().collect(),
            config,
            environment_cursor: 0,
            component_cursor: 0,
        },
    };
    app.handle_key(key(KeyCode::F(1)));
    app.handle_key(key(KeyCode::Esc));
    assert!(!cancellation.is_cancelled());
    app.handle_key(key(KeyCode::F(1)));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(cancellation.is_cancelled());
    assert!(matches!(app.screen, Screen::DeploymentPlanning { .. }));
}
