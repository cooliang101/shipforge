use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use crate::{
    application::{
        DestinationSetupError, DestinationSetupGateway, DestinationSetupRequest,
        DestinationSetupService, EndpointProbeRequest, LocalIdentityCandidate,
        RemoteSetupCandidates, SetupRootState,
    },
    config::{CredentialRegistry, DestinationRegistry, SshCredential},
    drivers::DriverKind,
};

use super::*;

#[derive(Debug, Default)]
struct Gateway {
    wait_capture: AtomicBool,
    auths: AtomicUsize,
    captures: AtomicUsize,
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
        Ok(vec![LocalIdentityCandidate {
            reference: "SHA256:fake-identity".into(),
            label: "SSH Agent · fixture".into(),
        }])
    }
    async fn capture_endpoint_identity(
        &self,
        _: &EndpointProbeRequest,
        _: std::time::Duration,
        cancellation: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        self.captures.fetch_add(1, Ordering::Relaxed);
        if self.wait_capture.load(Ordering::Relaxed) {
            cancellation.cancelled().await;
            return Err(DestinationSetupError::operation("capture", "cancelled"));
        }
        Ok("SHA256:fixture-host".into())
    }
    async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        _: std::time::Duration,
        _: std::time::Duration,
        _: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        self.auths.fetch_add(1, Ordering::Relaxed);
        assert_eq!(request.destination.value["hostKey"], "SHA256:fixture-host");
        assert!(request.credential.downcast_ref::<SshCredential>().is_some());
        Ok(RemoteSetupCandidates {
            root: SetupRootState::ReadOnlyDirectory,
            services: Vec::new(),
            notices: Vec::new(),
        })
    }
}

fn fixture() -> (tempfile::TempDir, App, Arc<Gateway>) {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Arc::new(Gateway::default());
    let mut app = App::new_with_setup(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
        DestinationSetupService::new(gateway.clone()),
    )
    .unwrap();
    app.home_directory = Some(directory.path().to_owned());
    (directory, app, gateway)
}

fn press(app: &mut App, key: KeyCode) {
    assert!(!app.handle_key(KeyEvent::new(key, KeyModifiers::NONE)));
}

async fn wait(app: &mut App) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            app.poll_background();
            if app.connections_task.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("connection worker completed");
}

async fn form(app: &mut App) {
    app.open_connections();
    wait(app).await;
    press(app, KeyCode::Char('a'));
    wait(app).await;
    let Screen::Connections(ConnectionsScreen {
        page: ConnectionsPage::Form(form),
        ..
    }) = &mut app.screen
    else {
        panic!("form expected: {:?}", app.screen);
    };
    let form = Arc::make_mut(form);
    form.host = "fixture.example".into();
    form.user = "operator".into();
}

fn screen_text(app: &App) -> String {
    let Screen::Connections(screen) = &app.screen else {
        panic!("connection screen expected");
    };
    let mut terminal = Terminal::new(TestBackend::new(100, 35)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(100)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .replace(['│', '─', '┌', '┐', '└', '┘'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_create_edit_verify_and_delete_use_explicit_confirmation_and_saved_revisions() {
    let (directory, mut app, gateway) = fixture();
    form(&mut app).await;
    press(&mut app, KeyCode::Enter);
    wait(&mut app).await;
    assert!(screen_text(&app).contains("Explicit Host Key confirmation"));
    assert!(!directory.path().join("destinations.yaml").exists());
    press(&mut app, KeyCode::Enter);
    assert_eq!(gateway.auths.load(Ordering::Relaxed), 0);
    assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)));
    assert_eq!(gateway.auths.load(Ordering::Relaxed), 0);
    press(&mut app, KeyCode::Char('y'));
    wait(&mut app).await;
    let registry = DestinationRegistry::load(&directory.path().join("destinations.yaml")).unwrap();
    let key = registry.summaries()[0].key.clone();
    assert_eq!(registry.resolve(&key).unwrap().revision.get(), 1);
    assert!(!directory.path().join("projects.yaml").exists());
    assert!(!directory.path().join("history.sqlite3").exists());
    press(&mut app, KeyCode::Char('e'));
    wait(&mut app).await;
    press(&mut app, KeyCode::Enter);
    wait(&mut app).await;
    press(&mut app, KeyCode::Char('y'));
    wait(&mut app).await;
    let registry = DestinationRegistry::load(&directory.path().join("destinations.yaml")).unwrap();
    assert_eq!(registry.resolve(&key).unwrap().revision.get(), 2);
    press(&mut app, KeyCode::Char('v'));
    wait(&mut app).await;
    assert!(screen_text(&app).contains("not a deployment or service-health result"));
    assert_eq!(gateway.auths.load(Ordering::Relaxed), 3);
    press(&mut app, KeyCode::Char('x'));
    wait(&mut app).await;
    assert!(screen_text(&app).contains("No local references found"));
    press(&mut app, KeyCode::Char('c'));
    wait(&mut app).await;
    assert!(
        DestinationRegistry::load(&directory.path().join("destinations.yaml"))
            .unwrap()
            .summaries()
            .is_empty()
    );
    assert!(
        !CredentialRegistry::load(&directory.path().join("credentials.yaml"))
            .unwrap()
            .summaries()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_late_results_do_not_allow_navigation_or_save() {
    let (directory, mut app, gateway) = fixture();
    gateway.wait_capture.store(true, Ordering::Relaxed);
    form(&mut app).await;
    press(&mut app, KeyCode::Enter);
    assert!(app.connections_task.is_some());
    app.finish_connections(
        uuid::Uuid::now_v7(),
        Ok(ConnectionsPage::List {
            items: Arc::new(Vec::new()),
            cursor: 0,
        }),
    );
    assert!(app.connections_task.is_some());
    press(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(
        &app.screen,
        Screen::Connections(ConnectionsScreen {
            page: ConnectionsPage::Loading {
                cancelling: true,
                ..
            },
            ..
        })
    ));
    wait(&mut app).await;
    assert!(matches!(
        &app.screen,
        Screen::Connections(ConnectionsScreen {
            page: ConnectionsPage::Form(_),
            ..
        })
    ));
    assert!(!directory.path().join("destinations.yaml").exists());
    assert_eq!(gateway.auths.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_delete_is_visible_and_confirmation_cannot_bypass_unknown_history() {
    let (directory, mut app, _) = fixture();
    form(&mut app).await;
    press(&mut app, KeyCode::Enter);
    wait(&mut app).await;
    press(&mut app, KeyCode::Char('y'));
    wait(&mut app).await;
    std::fs::write(directory.path().join("history.sqlite3"), b"corrupt fixture").unwrap();
    press(&mut app, KeyCode::Char('x'));
    wait(&mut app).await;
    let text = screen_text(&app);
    assert!(text.contains("UNKNOWN"));
    assert!(text.contains("Removal blocked"));
    press(&mut app, KeyCode::Char('c'));
    assert!(app.connections_task.is_none());
    assert_eq!(
        DestinationRegistry::load(&directory.path().join("destinations.yaml"))
            .unwrap()
            .summaries()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_browser_selects_a_file_without_reading_or_displaying_private_key_contents() {
    let (directory, mut app, _) = fixture();
    let path = directory.path().join("test-identity");
    std::fs::write(&path, "PRIVATE KEY SECRET SENTINEL").unwrap();
    form(&mut app).await;
    press(&mut app, KeyCode::F(3));
    wait(&mut app).await;
    let text = screen_text(&app);
    assert!(!text.contains("PRIVATE KEY SECRET"));
    let Screen::Connections(ConnectionsScreen {
        page:
            ConnectionsPage::Keys {
                directory: listing,
                cursor,
                ..
            },
        ..
    }) = &mut app.screen
    else {
        panic!("key browser expected");
    };
    *cursor = listing
        .entries
        .iter()
        .position(|entry| entry.path.file_name().unwrap() == "test-identity")
        .unwrap();
    press(&mut app, KeyCode::Char('s'));
    wait(&mut app).await;
    let Screen::Connections(ConnectionsScreen {
        page: ConnectionsPage::Form(form),
        ..
    }) = &app.screen
    else {
        panic!("form expected");
    };
    assert!(matches!(
        form.credentials.get(form.credential_cursor),
        Some(CredentialChoice::IdentityFile { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "PRIVATE KEY SECRET SENTINEL"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_unregistration_is_separate_and_does_not_require_available_yaml() {
    let (directory, mut app, _) = fixture();
    let root = directory.path().join("missing-project");
    let registry: crate::projects::ProjectRegistry = serde_yaml_ng::from_value(
        serde_yaml_ng::to_value(serde_json::json!({
            "schemaVersion": 1, "projects": [{"root": root, "lastOpenedUnixMs": 1}]
        }))
        .unwrap(),
    )
    .unwrap();
    registry.save(&app.registry_path).unwrap();
    app.recent = registry.statuses();
    app.preview_recent_removal();
    wait(&mut app).await;
    assert!(screen_text(&app).contains("are NOT deleted"));
    press(&mut app, KeyCode::Char('c'));
    wait(&mut app).await;
    assert!(matches!(app.screen, Screen::Projects));
    assert!(app.recent.is_empty());
    assert!(!root.exists());
    assert!(!directory.path().join("history.sqlite3").exists());
}

#[test]
fn key_directory_and_rendering_are_bounded_and_strip_terminal_controls() {
    assert_eq!(
        render::safe_text("hello\u{1b}[31m\0world"),
        "hello[31mworld"
    );
    let truncated = render::safe_text(&"x".repeat(5000));
    assert!(truncated.ends_with(" [truncated]"));
    assert_eq!(render::safe_text("a\n\t\u{202e}b\u{2066}"), "ab");
}

#[test]
fn narrow_terminal_keeps_last_long_connection_and_key_selected_rows_visible() {
    use crate::{
        config::{DestinationSettings, HostKeyFingerprint},
        drivers::CredentialHandle,
    };
    let mut registry = DestinationRegistry::new();
    for index in 0..30 {
        registry
            .create(
                DestinationKey::new(),
                DestinationSettings::LinuxSsh {
                    host: format!("{}-long.example", "a".repeat(180)),
                    port: 22,
                    user: format!("operator-{index}"),
                    credential: CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:key").unwrap(),
                },
            )
            .unwrap();
    }
    let items = registry
        .summaries()
        .into_iter()
        .map(|summary| ConnectionDetails {
            current: registry.resolve(&summary.key).unwrap().clone(),
            key: summary.key,
        })
        .collect::<Vec<_>>();
    let crate::config::DestinationSettings::LinuxSsh { user, .. } =
        &items.last().unwrap().current.settings;
    let expected = user.clone();
    let mut screen = ConnectionsScreen {
        page: ConnectionsPage::List {
            items: Arc::new(items),
            cursor: 29,
        },
        scroll: 0,
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(text.contains(&format!("> {expected}@")));
    let form = Arc::new(ConnectionForm {
        existing: None,
        host: String::new(),
        user: String::new(),
        port: "22".into(),
        field: SshField::Credential,
        credentials: Vec::new(),
        credential_cursor: 0,
        hosts: Vec::new(),
        host_cursor: 0,
        notices: Vec::new(),
    });
    screen.page = ConnectionsPage::Keys {
        form,
        cursor: 29,
        directory: Arc::new(KeyDirectory {
            path: PathBuf::from("a".repeat(200)),
            entries: (0..30)
                .map(|index| KeyEntry {
                    path: PathBuf::from(format!("key-{index}-{}", "b".repeat(180))),
                    directory: false,
                })
                .collect(),
        }),
    };
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(text.contains("> key-29-"));
}
