use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use tempfile::TempDir;

use crate::{
    config::{
        self, ArtifactSpec, BuildCommand, DestinationRegistry, DestinationSettings,
        HostKeyFingerprint, ProjectSetup,
    },
    domain::DestinationKey,
    drivers::CredentialHandle,
};

use super::*;

struct Fixture {
    directory: TempDir,
    original: ProjectConfig,
    app: App,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let destination = DestinationKey::new();
        let mut registry = DestinationRegistry::new();
        for (key, host) in [
            (destination.clone(), "one.invalid"),
            (DestinationKey::new(), "two.invalid"),
        ] {
            registry
                .create(
                    key,
                    DestinationSettings::LinuxSsh {
                        host: host.into(),
                        port: 22,
                        user: "deploy".into(),
                        credential: CredentialHandle::new(),
                        host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
                    },
                )
                .unwrap();
        }
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let component = ComponentSetup {
            working_directory: None,
            build: vec![BuildCommand::argv("cargo", ["build"])],
            artifact: ArtifactSpec {
                path: "target/output".into(),
            },
        };
        let backend = TargetSetup {
            destination: destination.clone(),
            root: None,
            systemd: None,
            health: None,
            after: Vec::new(),
        };
        let worker = TargetSetup {
            after: vec![name("backend")],
            ..backend.clone()
        };
        let setup = ProjectSetup {
            project: "demo".into(),
            components: BTreeMap::from([
                (name("backend"), component.clone()),
                (name("worker"), component),
            ]),
            environments: BTreeMap::from([
                (
                    "production".into(),
                    EnvironmentSetup {
                        components: BTreeMap::from([
                            (name("backend"), backend.clone()),
                            (name("worker"), worker),
                        ]),
                    },
                ),
                (
                    "staging".into(),
                    EnvironmentSetup {
                        components: BTreeMap::from([(name("backend"), backend)]),
                    },
                ),
            ]),
        };
        let original = config::initialize(directory.path(), setup).unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            registry_path,
            directory.path(),
        )
        .unwrap();
        app.open_project_edit(directory.path().to_owned());
        finished(&mut app).await;
        assert!(matches!(screen(&app).page, ProjectEditPage::Home));
        Self {
            directory,
            original,
            app,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.directory.path().join(config::PROJECT_FILE)).unwrap()
    }

    async fn preview(&mut self) -> Arc<ProjectEditPreview> {
        set_page(&mut self.app, ProjectEditPage::Home);
        press(&mut self.app, KeyCode::Char('p'));
        finished(&mut self.app).await;
        let ProjectEditPage::Preview(preview) = screen(&self.app).page else {
            panic!("preview expected: {:?}", self.app.message);
        };
        preview
    }
}

fn name(value: &str) -> ComponentName {
    ComponentName::parse(value).unwrap()
}

fn screen(app: &App) -> ProjectEditScreen {
    let Screen::ProjectEdit(screen) = &app.screen else {
        panic!("editor expected");
    };
    screen.clone()
}

fn set_page(app: &mut App, page: ProjectEditPage) {
    let mut current = screen(app);
    current.page = page;
    app.screen = Screen::ProjectEdit(current);
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn enter_text(app: &mut App, value: &str) {
    assert!(matches!(screen(app).page, ProjectEditPage::Text(_)));
    press(app, KeyCode::Delete);
    for character in value.chars() {
        press(app, KeyCode::Char(character));
    }
    press(app, KeyCode::Enter);
}

fn environment(app: &App, name: &str) -> EnvironmentForm {
    let draft = screen(app).draft.unwrap();
    EnvironmentForm {
        original: Some(name.into()),
        name: name.into(),
        targets: draft.setup.environments[name].components.clone(),
    }
}

async fn finished(app: &mut App) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            app.poll_background();
            if app.project_edit_task.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("editor worker completes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_rename_is_draft_only_until_plain_c_saves_exact_preview() {
    let mut fixture = Fixture::new().await;
    let original_bytes = fixture.bytes();
    press(&mut fixture.app, KeyCode::Char('n'));
    enter_text(&mut fixture.app, "renamed-project");
    assert_eq!(fixture.bytes(), original_bytes);
    let preview = fixture.preview().await;
    assert_eq!(preview.config().project_id, fixture.original.project_id);
    assert_eq!(preview.config().project, "renamed-project");
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SHIFT,
    ] {
        fixture
            .app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers));
        assert!(fixture.app.project_edit_task.is_none());
        assert_eq!(fixture.bytes(), original_bytes);
    }
    press(&mut fixture.app, KeyCode::Enter);
    assert!(fixture.app.project_edit_task.is_none());
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
    let Screen::Overview { config, .. } = &fixture.app.screen else {
        panic!("saved overview expected");
    };
    assert_eq!(config, preview.config());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn argv_editor_preserves_spaces_as_one_argument_and_refuses_shell_commands() {
    let mut fixture = Fixture::new().await;
    let draft = screen(&fixture.app).draft.unwrap();
    let mut form = ComponentForm::new(
        Some(name("backend")),
        "backend".into(),
        &draft.setup.components[&name("backend")],
    );
    form.commands[0].shell = true;
    set_page(
        &mut fixture.app,
        ProjectEditPage::Component {
            form: form.clone(),
            cursor: 4,
        },
    );
    press(&mut fixture.app, KeyCode::Enter);
    assert!(matches!(
        screen(&fixture.app).page,
        ProjectEditPage::Component { .. }
    ));
    assert!(
        fixture
            .app
            .message
            .as_ref()
            .unwrap()
            .contains("Shell strings")
    );
    set_page(
        &mut fixture.app,
        ProjectEditPage::Command {
            form,
            command: 0,
            cursor: 0,
        },
    );
    press(&mut fixture.app, KeyCode::Enter);
    enter_text(&mut fixture.app, "echo");
    press(&mut fixture.app, KeyCode::Char('a'));
    enter_text(&mut fixture.app, "hello world; $(not-a-shell)");
    let ProjectEditPage::Command { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    assert!(!form.commands[0].shell);
    assert_eq!(
        form.commands[0].args,
        ["build", "hello world; $(not-a-shell)"]
    );
    set_page(
        &mut fixture.app,
        ProjectEditPage::Component { form, cursor: 4 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    let preview = fixture.preview().await;
    assert_eq!(
        preview.config().components[&name("backend")].build[0].args[1],
        "hello world; $(not-a-shell)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_appended_argv_or_executable_restores_original_command() {
    let mut fixture = Fixture::new().await;
    let draft = screen(&fixture.app).draft.unwrap();
    let form = ComponentForm::new(
        Some(name("backend")),
        "backend".into(),
        &draft.setup.components[&name("backend")],
    );
    let commands = form.commands.clone();
    set_page(
        &mut fixture.app,
        ProjectEditPage::Command {
            form,
            command: 0,
            cursor: 0,
        },
    );
    press(&mut fixture.app, KeyCode::Char('a'));
    press(&mut fixture.app, KeyCode::Char('x'));
    press(&mut fixture.app, KeyCode::Esc);
    let ProjectEditPage::Command { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(form.commands, commands);
    set_page(
        &mut fixture.app,
        ProjectEditPage::Commands { form, cursor: 0 },
    );
    press(&mut fixture.app, KeyCode::Char('a'));
    press(&mut fixture.app, KeyCode::Char('x'));
    press(&mut fixture.app, KeyCode::Esc);
    let ProjectEditPage::Commands { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(form.commands, commands);
    set_page(
        &mut fixture.app,
        ProjectEditPage::Component { form, cursor: 4 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    let preview = fixture.preview().await;
    assert_eq!(
        preview.config().components[&name("backend")].build,
        commands
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_component_defaults_to_its_own_directory_and_artifact_is_relative_to_it() {
    let mut fixture = Fixture::new().await;
    let mut form = ComponentForm::blank();
    form.name = "frontend".into();
    form.artifact = "dist".into();
    form.commands = vec![BuildCommand::argv("npm", ["run", "build"])];
    set_page(
        &mut fixture.app,
        ProjectEditPage::Component { form, cursor: 4 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    let draft = screen(&fixture.app).draft.unwrap();
    assert!(
        draft.setup.components[&name("frontend")]
            .working_directory
            .is_none()
    );
    let preview = fixture.preview().await;
    assert_eq!(
        preview.config().components[&name("frontend")].working_directory,
        PathBuf::from("frontend")
    );
    assert_eq!(
        preview.config().components[&name("frontend")].artifact.path,
        PathBuf::from("dist")
    );
    assert!(
        forms::TextField::Artifact
            .label()
            .contains("Component working directory")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_component_and_environment_limits_reject_growth_but_allow_existing_edits() {
    let mut fixture = Fixture::new().await;
    let mut current = screen(&fixture.app);
    let draft = Arc::make_mut(current.draft.as_mut().unwrap());
    let component = draft.setup.components[&name("backend")].clone();
    for index in 0..254 {
        draft
            .setup
            .components
            .insert(name(&format!("component-{index}")), component.clone());
    }
    let form = ComponentForm::new(None, "too-many".into(), &component);
    current.page = ProjectEditPage::Component { form, cursor: 4 };
    fixture.app.screen = Screen::ProjectEdit(current);
    press(&mut fixture.app, KeyCode::Enter);
    assert_eq!(
        screen(&fixture.app).draft.unwrap().setup.components.len(),
        256
    );
    assert!(
        fixture
            .app
            .message
            .as_ref()
            .unwrap()
            .contains("256 Components")
    );
    let form = ComponentForm::new(Some(name("backend")), "backend".into(), &component);
    set_page(
        &mut fixture.app,
        ProjectEditPage::Component { form, cursor: 4 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    assert!(matches!(
        screen(&fixture.app).page,
        ProjectEditPage::Components { .. }
    ));
    assert_environment_limit(&mut fixture.app);
}

fn assert_environment_limit(app: &mut App) {
    let mut current = screen(app);
    let draft = Arc::make_mut(current.draft.as_mut().unwrap());
    let targets = draft.setup.environments["staging"].components.clone();
    for index in 0..254 {
        draft.setup.environments.insert(
            format!("env-{index}"),
            EnvironmentSetup {
                components: targets.clone(),
            },
        );
    }
    current.page = ProjectEditPage::Environment {
        form: EnvironmentForm {
            original: None,
            name: "too-many".into(),
            targets: targets.clone(),
        },
        cursor: 257,
    };
    app.screen = Screen::ProjectEdit(current);
    press(app, KeyCode::Enter);
    assert_eq!(screen(app).draft.unwrap().setup.environments.len(), 256);
    assert!(app.message.as_ref().unwrap().contains("256 Environments"));
    set_page(
        app,
        ProjectEditPage::Environment {
            form: EnvironmentForm {
                original: Some("staging".into()),
                name: "staging".into(),
                targets,
            },
            cursor: 257,
        },
    );
    press(app, KeyCode::Enter);
    assert!(matches!(
        screen(app).page,
        ProjectEditPage::Environments { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_target_limit_rejects_growth_and_preserves_original_draft() {
    let mut fixture = Fixture::new().await;
    let mut current = screen(&fixture.app);
    let draft = Arc::make_mut(current.draft.as_mut().unwrap());
    let component = draft.setup.components[&name("backend")].clone();
    for index in 0..254 {
        draft
            .setup
            .components
            .insert(name(&format!("component-{index}")), component.clone());
    }
    let template = draft.setup.environments["staging"].components[&name("backend")].clone();
    let targets = draft
        .setup
        .components
        .keys()
        .map(|name| (name.clone(), template.clone()))
        .collect::<BTreeMap<_, _>>();
    draft.setup.environments.clear();
    for index in 0..16 {
        draft.setup.environments.insert(
            format!("env-{index}"),
            EnvironmentSetup {
                components: targets.clone(),
            },
        );
    }
    current.page = ProjectEditPage::Environment {
        form: EnvironmentForm {
            original: None,
            name: "one-more".into(),
            targets: BTreeMap::from([(name("backend"), template)]),
        },
        cursor: 257,
    };
    fixture.app.screen = Screen::ProjectEdit(current);
    press(&mut fixture.app, KeyCode::Enter);
    assert!(
        fixture
            .app
            .message
            .as_ref()
            .unwrap()
            .contains("4096 Component targets")
    );
    let draft = screen(&fixture.app).draft.unwrap();
    assert_eq!(draft.setup.environments.len(), 16);
    assert_eq!(
        draft
            .setup
            .environments
            .values()
            .map(|environment| environment.components.len())
            .sum::<usize>(),
        4096
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_existing_root_is_described_as_retained_and_preview_preserves_it() {
    let mut fixture = Fixture::new().await;
    let environment = environment(&fixture.app, "production");
    let mut target = environment.targets[&name("backend")].clone();
    target.root = None;
    let form = TargetForm {
        environment,
        component: name("backend"),
        target,
    };
    set_page(
        &mut fixture.app,
        ProjectEditPage::Target { form, cursor: 5 },
    );
    let current = screen(&fixture.app);
    let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
    terminal
        .draw(|frame| current.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(text.contains("retain existing frozen root"));
    press(&mut fixture.app, KeyCode::Enter);
    let ProjectEditPage::Environment { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    set_page(
        &mut fixture.app,
        ProjectEditPage::Environment { form, cursor: 3 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    let preview = fixture.preview().await;
    assert_eq!(
        preview.config().environments["production"].components[&name("backend")].root,
        fixture.original.environments["production"].components[&name("backend")].root
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn component_removal_is_explicit_draft_only_and_prunes_assignments_and_after() {
    let mut fixture = Fixture::new().await;
    let original = fixture.bytes();
    press(&mut fixture.app, KeyCode::Char('c'));
    press(&mut fixture.app, KeyCode::Char('d'));
    press(&mut fixture.app, KeyCode::Enter);
    assert_eq!(
        screen(&fixture.app).draft.unwrap().setup.components.len(),
        2
    );
    press(&mut fixture.app, KeyCode::Char('c'));
    let draft = screen(&fixture.app).draft.unwrap();
    assert!(!draft.setup.components.contains_key(&name("backend")));
    assert!(
        !draft.setup.environments["production"]
            .components
            .contains_key(&name("backend"))
    );
    assert!(
        draft.setup.environments["production"].components[&name("worker")]
            .after
            .is_empty()
    );
    assert_eq!(fixture.bytes(), original);
    press(&mut fixture.app, KeyCode::Char('d'));
    assert!(matches!(
        screen(&fixture.app).page,
        ProjectEditPage::Components { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_environment_rename_chain_preserves_identity_and_root() {
    let mut fixture = Fixture::new().await;
    for (before, after) in [("production", "live"), ("live", "release")] {
        let form = environment(&fixture.app, before);
        set_page(
            &mut fixture.app,
            ProjectEditPage::Environment { form, cursor: 0 },
        );
        press(&mut fixture.app, KeyCode::Enter);
        enter_text(&mut fixture.app, after);
        press(&mut fixture.app, KeyCode::Down);
        press(&mut fixture.app, KeyCode::Down);
        press(&mut fixture.app, KeyCode::Down);
        press(&mut fixture.app, KeyCode::Enter);
    }
    let draft = screen(&fixture.app).draft.unwrap();
    assert_eq!(draft.environment_renames.len(), 1);
    assert_eq!(draft.environment_renames[0].from, "production");
    assert_eq!(draft.environment_renames[0].to, "release");
    let preview = fixture.preview().await;
    assert_eq!(
        preview.config().environments["release"],
        fixture.original.environments["production"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_component_subset_and_saved_destination_are_selected_without_ids() {
    let mut fixture = Fixture::new().await;
    let form = environment(&fixture.app, "production");
    set_page(
        &mut fixture.app,
        ProjectEditPage::Environment { form, cursor: 1 },
    );
    press(&mut fixture.app, KeyCode::Char(' '));
    let ProjectEditPage::Environment { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(
        form.targets.keys().cloned().collect::<Vec<_>>(),
        [name("worker")]
    );
    assert!(form.targets[&name("worker")].after.is_empty());
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Enter);
    let destinations = screen(&fixture.app).draft.unwrap().destinations().to_vec();
    set_destination_cursor(&mut fixture.app, 1);
    press(&mut fixture.app, KeyCode::Enter);
    let ProjectEditPage::Target { mut form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(form.target.destination, destinations[1].key);
    form.target.root = Some("/srv/changed-worker".into());
    form.target.systemd = Some("worker.service".into());
    form.target.health = None;
    set_page(
        &mut fixture.app,
        ProjectEditPage::Target { form, cursor: 5 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    let ProjectEditPage::Environment { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    set_page(
        &mut fixture.app,
        ProjectEditPage::Environment { form, cursor: 3 },
    );
    press(&mut fixture.app, KeyCode::Enter);
    let preview = fixture.preview().await;
    assert_eq!(
        preview.config().environments["production"].components.len(),
        1
    );
    let target = &preview.config().environments["production"].components[&name("worker")];
    assert_eq!(target.destination, destinations[1].key);
    assert_eq!(target.root, "/srv/changed-worker");
    assert_eq!(target.systemd.as_deref(), Some("worker.service"));
    assert!(
        target.generation
            > fixture.original.environments["production"].components[&name("worker")].generation
    );
}

fn set_destination_cursor(app: &mut App, cursor: usize) {
    let ProjectEditPage::Destination { form, .. } = screen(app).page else {
        panic!();
    };
    set_page(app, ProjectEditPage::Destination { form, cursor });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_environment_identity_is_generated_at_preview_and_not_at_save() {
    let mut fixture = Fixture::new().await;
    press(&mut fixture.app, KeyCode::Char('e'));
    press(&mut fixture.app, KeyCode::Char('a'));
    press(&mut fixture.app, KeyCode::Enter);
    enter_text(&mut fixture.app, "testing");
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Char(' '));
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Enter);
    let preview = fixture.preview().await;
    let generated = preview.config().environments["testing"].id.clone();
    assert_ne!(generated, fixture.original.environments["production"].id);
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    let config::ProjectConfigState::Loaded(saved) = config::load(fixture.directory.path()).unwrap()
    else {
        panic!();
    };
    assert_eq!(saved.environments["testing"].id, generated);
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_edit_is_bounded_rejects_terminal_controls_and_escape_discards_only_field() {
    let mut fixture = Fixture::new().await;
    press(&mut fixture.app, KeyCode::Char('n'));
    press(&mut fixture.app, KeyCode::Delete);
    for character in ['a', '\n', '\u{1b}', '\u{202e}', '\u{200b}', 'b'] {
        press(&mut fixture.app, KeyCode::Char(character));
    }
    let ProjectEditPage::Text(edit) = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(edit.value, "ab");
    for _ in 0..4100 {
        press(&mut fixture.app, KeyCode::Char('x'));
    }
    let ProjectEditPage::Text(edit) = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(edit.value.len(), 4096);
    press(&mut fixture.app, KeyCode::Esc);
    assert_eq!(screen(&fixture.app).draft.unwrap().setup.project, "demo");
    assert!(!screen(&fixture.app).dirty);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_save_failure_restores_exact_preview_and_does_not_overwrite_external_change() {
    let mut fixture = Fixture::new().await;
    let preview = fixture.preview().await;
    let path = fixture.directory.path().join(config::PROJECT_FILE);
    let changed = format!("{}\n# external change\n", preview.yaml());
    std::fs::write(&path, &changed).unwrap();
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    let ProjectEditPage::Preview(restored) = screen(&fixture.app).page else {
        panic!();
    };
    assert!(Arc::ptr_eq(&restored, &preview));
    assert_eq!(std::fs::read_to_string(path).unwrap(), changed);
}

#[derive(Debug)]
struct WaitingGateway {
    calls: AtomicUsize,
    completed: AtomicBool,
    requests: Mutex<Vec<ProjectEditRequest>>,
}

impl WaitingGateway {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            completed: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait(?Send)]
impl ProjectEditGateway for WaitingGateway {
    async fn run(
        &self,
        request: ProjectEditRequest,
        cancel: &CancellationToken,
    ) -> Result<ProjectEditPage, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request);
        cancel.cancelled().await;
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        self.completed.store(true, Ordering::SeqCst);
        Err("Editor operation cancelled.".into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_single_flight_cancellation_waits_and_stale_uuid_cannot_replace_screen() {
    let mut fixture = Fixture::new().await;
    let fake = Arc::new(WaitingGateway::new());
    fixture.app.project_edit_gateway = fake.clone();
    press(&mut fixture.app, KeyCode::Char('c'));
    press(&mut fixture.app, KeyCode::Char('f'));
    let id = fixture.app.project_edit_task.as_ref().unwrap().id;
    fixture
        .app
        .finish_project_edit(uuid::Uuid::now_v7(), Ok(ProjectEditPage::Home));
    assert_eq!(fixture.app.project_edit_task.as_ref().unwrap().id, id);
    for key in [KeyCode::Char('p'), KeyCode::Char('c'), KeyCode::Enter] {
        press(&mut fixture.app, key);
    }
    press(&mut fixture.app, KeyCode::Esc);
    assert!(fixture.app.project_edit_task.is_some());
    assert!(matches!(
        screen(&fixture.app).page,
        ProjectEditPage::Loading {
            cancelling: true,
            ..
        }
    ));
    finished(&mut fixture.app).await;
    assert!(matches!(
        screen(&fixture.app).page,
        ProjectEditPage::Components { .. }
    ));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(fake.completed.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_shutdown_waits_even_before_service_session_acquisition() {
    let mut fixture = Fixture::new().await;
    let fake = Arc::new(WaitingGateway::new());
    fixture.app.project_edit_gateway = fake.clone();
    press(&mut fixture.app, KeyCode::Char('p'));
    fixture.app.shutdown();
    assert!(fake.completed.load(Ordering::SeqCst));
    assert!(fixture.app.project_edit_task.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_worker_blocks_editor_even_before_session_acquisition() {
    let mut fixture = Fixture::new().await;
    let fake = Arc::new(WaitingGateway::new());
    fixture.app.project_edit_gateway = fake.clone();
    fixture.app.open_connections();
    assert!(fixture.app.connections_task.is_some());
    fixture
        .app
        .open_project_edit(fixture.directory.path().to_owned());
    assert!(fixture.app.project_edit_task.is_none());
    assert!(matches!(fixture.app.screen, Screen::Connections(_)));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    fixture.app.shutdown();
    assert!(fixture.app.connections_task.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_project_editor_load_never_initializes_a_new_project() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = App::new(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
    )
    .unwrap();
    app.open_project_edit(directory.path().to_owned());
    finished(&mut app).await;
    assert!(screen(&app).draft.is_none());
    assert!(!directory.path().join(config::PROJECT_FILE).exists());
    assert!(!directory.path().join("destinations.yaml").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrow_form_keeps_logical_cursor_visible_without_wrapped_rows() {
    let mut fixture = Fixture::new().await;
    let draft = screen(&fixture.app).draft.unwrap();
    let mut form = ComponentForm::new(
        Some(name("backend")),
        "backend".into(),
        &draft.setup.components[&name("backend")],
    );
    form.working_directory = "nested/".repeat(100);
    set_page(
        &mut fixture.app,
        ProjectEditPage::Component { form, cursor: 4 },
    );
    let current = screen(&fixture.app);
    let mut terminal = Terminal::new(TestBackend::new(38, 5)).unwrap();
    terminal
        .draw(|frame| current.render(frame, frame.area()))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(text.contains("> Apply Component"));
}
