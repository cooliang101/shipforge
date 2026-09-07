use std::{collections::BTreeMap, fs, time::Duration};

use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use super::*;
use crate::{
    config::{
        self, ArtifactSpec, BuildCommand, ComponentSetup, DestinationRegistry, DestinationSettings,
        EnvironmentSetup, HostKeyFingerprint, ProjectSetup, TargetSetup,
    },
    domain::{ComponentName, DestinationKey},
    drivers::CredentialHandle,
};

struct Fixture {
    directory: tempfile::TempDir,
    original: ProjectConfig,
    app: App,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let destination = DestinationKey::new();
        let mut registry = DestinationRegistry::new();
        registry
            .create(
                destination.clone(),
                DestinationSettings::LinuxSsh {
                    host: "fixture.invalid".into(),
                    port: 22,
                    user: "deploy".into(),
                    credential: CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
                },
            )
            .unwrap();
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let name = ComponentName::parse("backend").unwrap();
        let original = config::initialize(
            directory.path(),
            ProjectSetup {
                project: "demo".into(),
                components: BTreeMap::from([(
                    name.clone(),
                    ComponentSetup {
                        working_directory: None,
                        build: vec![BuildCommand::argv("cargo", ["build"])],
                        artifact: ArtifactSpec {
                            path: "target/output".into(),
                        },
                    },
                )]),
                environments: BTreeMap::from([(
                    "production".into(),
                    EnvironmentSetup {
                        components: BTreeMap::from([(
                            name,
                            TargetSetup {
                                destination,
                                root: None,
                                service: None,
                                health: None,
                                after: Vec::new(),
                            },
                        )]),
                    },
                )]),
            },
        )
        .unwrap();
        let mut yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            &fs::read_to_string(directory.path().join(config::PROJECT_FILE)).unwrap(),
        )
        .unwrap();
        yaml.as_mapping_mut()
            .unwrap()
            .remove(serde_yaml_ng::Value::String("_shipforge".into()));
        fs::write(
            directory.path().join(config::PROJECT_FILE),
            serde_yaml_ng::to_string(&yaml).unwrap(),
        )
        .unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            registry_path,
            directory.path(),
        )
        .unwrap();
        app.open_reinitialize(directory.path().to_owned());
        Self {
            directory,
            original,
            app,
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join(config::PROJECT_FILE)
    }
    fn bytes(&self) -> Vec<u8> {
        fs::read(self.path()).unwrap()
    }
    async fn preview(&mut self) -> Arc<ProjectReinitializePreview> {
        press(&mut self.app, KeyCode::Enter);
        finished(&mut self.app).await;
        let ReinitializePage::Preview(preview) = &screen(&self.app).page else {
            panic!("preview expected: {:?}", self.app.message);
        };
        Arc::clone(preview)
    }
}

fn screen(app: &App) -> &ReinitializeScreen {
    let Screen::Reinitialize(screen) = &app.screen else {
        panic!("reinitialize screen expected");
    };
    screen
}

fn press(app: &mut App, key: KeyCode) {
    assert!(!app.handle_key(KeyEvent::new(key, KeyModifiers::NONE)));
}

async fn finished(app: &mut App) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            app.poll_background();
            if app.reinitialize_task.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("reinitialize worker completes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reinitialization_is_preview_only_until_unmodified_c_saves_exact_ids() {
    let mut fixture = Fixture::new();
    let before = fixture.bytes();
    assert!(fixture.app.reinitialize_task.is_none());
    assert!(matches!(screen(&fixture.app).page, ReinitializePage::Intro));
    press(&mut fixture.app, KeyCode::Char('c'));
    assert_eq!(fixture.bytes(), before);
    let preview = fixture.preview().await;
    assert_ne!(preview.config().project_id, fixture.original.project_id);
    assert_eq!(fixture.bytes(), before);
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SHIFT,
    ] {
        fixture
            .app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers));
        assert!(fixture.app.reinitialize_task.is_none());
        assert_eq!(fixture.bytes(), before);
    }
    press(&mut fixture.app, KeyCode::Enter);
    assert!(fixture.app.reinitialize_task.is_none());
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
    let Screen::Overview { config, .. } = &fixture.app.screen else {
        panic!("saved overview expected");
    };
    assert_eq!(config, preview.config());
    assert!(
        fixture
            .app
            .message
            .as_deref()
            .unwrap()
            .contains("NEW Project")
    );
    assert_eq!(fixture.app.recent.len(), 1);
    assert!(!fixture.directory.path().join("history.sqlite3").exists());
    assert!(!fixture.directory.path().join("credentials.yaml").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escape_discards_preview_or_cancels_tracked_preparation_without_writing() {
    for pending in [false, true] {
        let mut fixture = Fixture::new();
        let before = fixture.bytes();
        if pending {
            press(&mut fixture.app, KeyCode::Enter);
        } else {
            fixture.preview().await;
        }
        press(&mut fixture.app, KeyCode::Esc);
        if pending {
            assert!(fixture.app.reinitialize_task.is_some());
            assert!(matches!(
                screen(&fixture.app).page,
                ReinitializePage::Working {
                    cancelling: true,
                    ..
                }
            ));
            press(&mut fixture.app, KeyCode::Char('q'));
            assert!(fixture.app.reinitialize_task.is_some());
        }
        finished(&mut fixture.app).await;
        if pending {
            assert!(matches!(screen(&fixture.app).page, ReinitializePage::Intro));
        } else {
            assert!(matches!(fixture.app.screen, Screen::Projects));
        }
        assert_eq!(fixture.bytes(), before);
        assert!(!fixture.directory.path().join("projects.yaml").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_source_rejects_confirmation_and_requires_a_new_preview() {
    let mut fixture = Fixture::new();
    fixture.preview().await;
    let replacement = String::from_utf8(fixture.bytes())
        .unwrap()
        .replace("project: demo", "project: changed");
    fs::write(fixture.path(), &replacement).unwrap();
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_eq!(fixture.bytes(), replacement.as_bytes());
    assert!(matches!(screen(&fixture.app).page, ReinitializePage::Intro));
    assert!(fixture.app.message.as_deref().unwrap().contains("changed"));
    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(fixture.app.reinitialize_task.is_none());
    let preview = fixture.preview().await;
    assert_eq!(preview.config().project, "changed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn damaged_human_fields_and_missing_yaml_never_offer_a_save_preview() {
    for missing in [false, true] {
        let mut fixture = Fixture::new();
        if missing {
            fs::remove_file(fixture.path()).unwrap();
        } else {
            fs::write(fixture.path(), "project: [secret-parser-sentinel").unwrap();
        }
        press(&mut fixture.app, KeyCode::Enter);
        finished(&mut fixture.app).await;
        assert!(matches!(screen(&fixture.app).page, ReinitializePage::Intro));
        assert!(
            !fixture
                .app
                .message
                .as_deref()
                .unwrap()
                .contains("secret-parser-sentinel")
        );
        press(&mut fixture.app, KeyCode::Char('c'));
        assert!(fixture.app.reinitialize_task.is_none());
        if missing {
            assert!(!fixture.path().exists());
        } else {
            assert_eq!(fixture.bytes(), b"project: [secret-parser-sentinel");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_failure_preserves_saved_yaml_and_shows_the_new_project() {
    for had_cached_project in [false, true] {
        let mut fixture = Fixture::new();
        let cached = crate::projects::RegisteredProject {
            root: fixture.directory.path().to_owned(),
            last_opened_unix_ms: 42,
        };
        if had_cached_project {
            fixture.app.recent = vec![ProjectStatus {
                project: cached.clone(),
                available: true,
            }];
        }
        let preview = fixture.preview().await;
        let invalid = "invalid: [secret-registry-sentinel";
        fs::write(&fixture.app.registry_path, invalid).unwrap();
        press(&mut fixture.app, KeyCode::Char('c'));
        finished(&mut fixture.app).await;
        assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
        assert!(matches!(fixture.app.screen, Screen::Overview { .. }));
        let message = fixture.app.message.as_deref().unwrap();
        assert!(message.contains("Saved shipforge.yaml"));
        assert!(message.contains("registration or refresh failed"));
        assert!(!message.contains("secret-registry-sentinel"));
        assert!(fixture.app.recent_unavailable);
        assert_eq!(fixture.app.recent.len(), usize::from(had_cached_project));
        if had_cached_project {
            assert_eq!(fixture.app.recent[0].project, cached);
            assert!(!fixture.app.recent[0].available);
        }

        press(&mut fixture.app, KeyCode::Esc);
        assert!(matches!(fixture.app.screen, Screen::Projects));
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| crate::tui::render(frame, &fixture.app))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Recent projects unavailable"));
        if had_cached_project {
            press(&mut fixture.app, KeyCode::Enter);
            assert!(matches!(fixture.app.screen, Screen::Projects));
        }
        press(&mut fixture.app, KeyCode::Char('f'));
        assert!(fixture.app.recent_unavailable);
        assert_eq!(
            fs::read_to_string(&fixture.app.registry_path).unwrap(),
            invalid
        );
        ProjectRegistry::new()
            .save(&fixture.app.registry_path)
            .unwrap();
        press(&mut fixture.app, KeyCode::Char('f'));
        assert!(!fixture.app.recent_unavailable);
        assert!(fixture.app.recent.is_empty());
        assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_gateway_request_never_writes_or_registers() {
    let mut fixture = Fixture::new();
    let preview = fixture.preview().await;
    let before = fixture.bytes();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let service = ProjectReinitializeService::new(
        fixture.app.destination_registry_path.clone(),
        Arc::clone(&fixture.app.deployment_session),
    );
    assert!(
        run_request(
            &service,
            &fixture.app.registry_path,
            ReinitializeRequest::Save(preview),
            &cancellation
        )
        .await
        .is_err()
    );
    assert_eq!(fixture.bytes(), before);
    assert!(!fixture.app.registry_path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_matches_request_id_and_known_save_wins_over_late_cancellation() {
    let mut fixture = Fixture::new();
    let preview = fixture.preview().await;
    let origin = screen(&fixture.app).clone();
    let service = ProjectReinitializeService::new(
        fixture.app.destination_registry_path.clone(),
        Arc::clone(&fixture.app.deployment_session),
    );
    let result = run_request(
        &service,
        &fixture.app.registry_path,
        ReinitializeRequest::Save(Arc::clone(&preview)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let id = uuid::Uuid::now_v7();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    fixture.app.reinitialize_task = Some(ReinitializeTask {
        id,
        origin,
        cancellation,
        worker: Some(std::thread::spawn(|| {
            panic!("private-reinitialize-tail-payload");
        })),
        saving: true,
    });
    fixture
        .app
        .finish_reinitialize(uuid::Uuid::now_v7(), Err("obsolete error".into()));
    assert!(fixture.app.reinitialize_task.is_some());
    fixture.app.finish_reinitialize(id, Ok(result));
    assert!(fixture.app.reinitialize_task.is_none());
    assert!(matches!(fixture.app.screen, Screen::Overview { .. }));
    assert!(
        fixture
            .app
            .message
            .as_deref()
            .unwrap()
            .contains("Saved shipforge.yaml")
    );
    assert!(
        fixture
            .app
            .message
            .as_deref()
            .unwrap()
            .contains("worker cleanup")
    );
    assert!(
        !fixture
            .app
            .message
            .as_deref()
            .unwrap()
            .contains("private-reinitialize-tail-payload")
    );
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_tail_panic_rejects_preview_with_a_static_diagnostic() {
    let mut fixture = Fixture::new();
    let preview = fixture.preview().await;
    let origin = ReinitializeScreen {
        root: fixture.directory.path().to_owned(),
        page: ReinitializePage::Intro,
        scroll: 0,
        horizontal: 0,
        document: None,
    };
    let id = uuid::Uuid::now_v7();
    fixture.app.reinitialize_task = Some(ReinitializeTask {
        id,
        origin,
        cancellation: CancellationToken::new(),
        worker: Some(std::thread::spawn(|| {
            panic!("private-reinitialize-tail-payload");
        })),
        saving: false,
    });
    fixture
        .app
        .finish_reinitialize(id, Ok(ReinitializeResult::Preview(preview)));
    assert!(matches!(screen(&fixture.app).page, ReinitializePage::Intro));
    let message = fixture.app.message.as_deref().unwrap();
    assert!(message.contains("worker stopped"));
    assert!(!message.contains("private-reinitialize-tail-payload"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_error_survives_worker_tail_failure_without_exposing_the_panic() {
    let mut fixture = Fixture::new();
    let origin = screen(&fixture.app).clone();
    let id = uuid::Uuid::now_v7();
    fixture.app.reinitialize_task = Some(ReinitializeTask {
        id,
        origin,
        cancellation: CancellationToken::new(),
        worker: Some(std::thread::spawn(|| {
            panic!("private-reinitialize-save-tail-payload");
        })),
        saving: true,
    });
    fixture.app.finish_reinitialize(
        id,
        Err("Known reinitialization durability outcome; reload required.".into()),
    );
    let message = fixture.app.message.as_deref().unwrap();
    assert!(message.contains("Known reinitialization durability outcome"));
    assert!(message.contains("worker"));
    assert!(!message.contains("private-reinitialize-save-tail-payload"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_warning_and_navigation_remain_visible_when_yaml_is_scrolled() {
    let mut fixture = Fixture::new();
    fixture.preview().await;
    for _ in 0..8 {
        press(&mut fixture.app, KeyCode::PageDown);
    }
    let screen = screen(&fixture.app);
    assert!(screen.requires_plain_confirmation());
    assert!(screen.context_label().contains("demo"));
    assert!(screen.help().contains("c confirm NEW Project"));
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(rendered.contains("NEW Project identity"));
    assert!(rendered.contains("Existing deployments"));
    assert!(!rendered.contains("linux-ssh"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_valid_yaml_argument_remains_fully_visible_and_saves_the_exact_preview() {
    let mut fixture = Fixture::new();
    let argument = format!("{}EXACT-YAML-TAIL", "x".repeat(5000));
    let mut yaml: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&fixture.bytes()).unwrap();
    yaml["components"]["backend"]["build"][0] = serde_yaml_ng::Value::Sequence(vec![
        "cargo".into(),
        "build".into(),
        argument.clone().into(),
    ]);
    fs::write(fixture.path(), serde_yaml_ng::to_string(&yaml).unwrap()).unwrap();
    let before = fixture.bytes();
    let preview = fixture.preview().await;
    assert_eq!(fixture.bytes(), before);
    assert!(preview.yaml().lines().any(|line| line.len() > 4096));
    let full_text = preview_text(&preview);
    assert!(full_text.ends_with(preview.yaml()));
    assert!(full_text.contains(&argument));
    assert!(!full_text.contains("[display truncated]"));

    let mut current = screen(&fixture.app).clone();
    let document = current.document.as_ref().unwrap();
    current.scroll = document
        .text
        .lines()
        .position(|line| line.contains(&argument))
        .unwrap();
    let line = document.text.lines().nth(current.scroll).unwrap();
    let tail = line.find("EXACT-YAML-TAIL").unwrap();
    let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
    current.horizontal = tail;
    terminal
        .draw(|frame| current.render(frame, frame.area()))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        rendered.contains("EXACT-YAML-TAIL"),
        "the long argument tail is reachable in a narrow viewport"
    );
    fixture.app.screen = Screen::Reinitialize(current);
    press(&mut fixture.app, KeyCode::Char('0'));
    assert_eq!(screen(&fixture.app).horizontal, 0);
    press(&mut fixture.app, KeyCode::Right);
    press(&mut fixture.app, KeyCode::Char(']'));
    assert_eq!(screen(&fixture.app).horizontal, 33);
    press(&mut fixture.app, KeyCode::Char('['));
    press(&mut fixture.app, KeyCode::Left);
    assert_eq!(screen(&fixture.app).horizontal, 0);
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_u16_lines_of_valid_yaml_remain_reviewable_and_save_without_truncation() {
    use serde_yaml_ng::{Mapping, Value};
    let mut fixture = Fixture::new();
    let mut yaml: Value = serde_yaml_ng::from_slice(&fixture.bytes()).unwrap();
    let component = yaml["components"]["backend"].clone();
    let target = yaml["environments"]["production"]["components"]["backend"].clone();
    let mut components = Mapping::new();
    let mut targets = Mapping::new();
    for index in 0..5 {
        let name = format!("chunk-{index}");
        let mut component = component.clone();
        let mut argv = vec![Value::String(String::new()); 257];
        argv[0] = "cargo".into();
        component["build"] = Value::Sequence(vec![Value::Sequence(argv); 64]);
        components.insert(name.clone().into(), component);
        let mut target = target.clone();
        if index == 4 {
            target["service"] = serde_yaml_ng::to_value(crate::config::ServiceConfig::systemd(
                "last-evidence.service",
            ))
            .unwrap();
        }
        targets.insert(name.into(), target);
    }
    yaml["components"] = Value::Mapping(components);
    yaml["environments"]["production"]["components"] = Value::Mapping(targets);
    let source = serde_yaml_ng::to_string(&yaml).unwrap();
    assert!(source.len() < 1024 * 1024);
    fs::write(fixture.path(), &source).unwrap();
    let preview = fixture.preview().await;
    assert!(preview.yaml().len() < 1024 * 1024);
    assert!(preview.yaml().lines().count() > usize::from(u16::MAX));
    assert_eq!(fixture.bytes(), source.as_bytes());
    press(&mut fixture.app, KeyCode::End);
    let current = screen(&fixture.app);
    assert!(current.scroll > usize::from(u16::MAX));
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| current.render(frame, frame.area()))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(rendered.contains("last-evidence.service"));
    let last_line = current.scroll;
    press(&mut fixture.app, KeyCode::PageUp);
    assert_eq!(screen(&fixture.app).scroll, last_line - 10);
    press(&mut fixture.app, KeyCode::PageDown);
    assert_eq!(screen(&fixture.app).scroll, last_line);
    press(&mut fixture.app, KeyCode::Home);
    assert_eq!(screen(&fixture.app).scroll, 0);
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
}

#[test]
fn review_window_pans_by_complete_unicode_graphemes_without_u16_offsets() {
    let document = ReviewDocument::new("a\u{0301}项目-tail\nsecond".into());
    assert_eq!(document.window(0, 1, 1), "项目-tail");
    assert_eq!(document.window(0, 3, 1), "-tail");
    assert_eq!(document.window(1, 0, 1), "second");
    assert_eq!(document.window(usize::MAX, 0, 1), "");
}

#[test]
fn reinitialization_worker_panic_reports_only_local_uncertainty_without_payload() {
    let error = catch_local_worker_failure::<()>(|| panic!("secret-worker-payload")).unwrap_err();
    assert!(error.contains("local YAML save outcome is unconfirmed"));
    assert!(error.contains("Reload this directory"));
    assert!(!error.contains("secret-worker-payload"));
    assert!(!error.contains("remote history"));
}
