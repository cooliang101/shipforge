use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use tempfile::{TempDir, tempdir};

use super::*;

#[derive(Debug)]
struct FakeGateway {
    calls: AtomicUsize,
    completed: AtomicBool,
    wait_for_cancel: bool,
    panic: bool,
    requests: Mutex<Vec<ManagementRequest>>,
}

impl FakeGateway {
    fn new(wait_for_cancel: bool) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            completed: AtomicBool::new(false),
            wait_for_cancel,
            panic: false,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait(?Send)]
impl ManagementGateway for FakeGateway {
    async fn run(
        &self,
        _: &ManagementScope,
        request: ManagementRequest,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request);
        assert!(!self.panic, "private-secret-panic-payload");
        if self.wait_for_cancel {
            cancellation.cancelled().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.completed.store(true, Ordering::SeqCst);
        Ok(ManagementPage::History {
            page: Arc::new(HistoryPage {
                items: Vec::new(),
                more: false,
                database_missing: true,
            }),
            offset: 0,
            cursor: 0,
        })
    }
}

pub(super) fn fixture() -> (TempDir, App) {
    let directory = tempdir().unwrap();
    std::fs::write(
        directory.path().join("shipforge.yaml"),
        include_str!("../../../../docs/examples/shipforge.yaml"),
    )
    .unwrap();
    let crate::config::ProjectConfigState::Loaded(config) =
        crate::config::load(directory.path()).unwrap()
    else {
        panic!("valid fixture expected");
    };
    let mut app = App::new(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
    )
    .unwrap();
    app.open_management(directory.path().to_owned(), config);
    (directory, app)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn screen(app: &App) -> ManagementScreen {
    let Screen::Management(screen) = &app.screen else {
        panic!("management screen expected");
    };
    screen.clone()
}

async fn finished(app: &mut App) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            app.poll_background();
            if app.management_task.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("management worker should finish");
}

fn render_screen(screen: &ManagementScreen, app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area(), app))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_entry_uses_local_query_without_creating_missing_files() {
    let (directory, mut app) = fixture();
    app.handle_key(key(KeyCode::Char('h')));
    finished(&mut app).await;
    let Screen::Management(ManagementScreen {
        page: ManagementPage::History { page, .. },
        ..
    }) = &app.screen
    else {
        panic!("history page expected");
    };
    assert!(page.database_missing);
    assert!(!directory.path().join("history.sqlite3").exists());
    assert!(!directory.path().join("credentials.yaml").exists());
    assert!(!directory.path().join("destinations.yaml").exists());
    assert!(render_screen(&screen(&app), &app).contains("historical outcomes are unknown"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_credentials_never_echo_input_through_management_errors() {
    for contents in [
        "schemaVersion: 1\ncredentials:\n  cred_test:\n    kind: identity-file\n    path: private-secret-input\n",
        "schemaVersion: 1\ncredentials:\n  cred_test:\n    kind: private-secret-input\n",
    ] {
        let (directory, mut app) = fixture();
        let path = directory.path().join("credentials.yaml");
        std::fs::write(&path, contents).unwrap();
        assert!(crate::config::CredentialRegistry::load(&path).is_err());
        // Local history must remain usable without loading broken credentials.
        app.handle_key(key(KeyCode::Char('h')));
        finished(&mut app).await;
        assert!(app.message.is_none());
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Enter));
        finished(&mut app).await;
        let message = app.message.as_deref().unwrap();
        assert!(message.contains("Credential registry"));
        assert!(!message.contains("private-secret-input"));
        assert!(!message.contains("identity-file"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        assert!(!directory.path().join("history.sqlite3").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn management_is_single_flight_and_esc_waits_for_worker_completion() {
    let (_directory, mut app) = fixture();
    let fake = Arc::new(FakeGateway::new(true));
    app.management_gateway = fake.clone();
    app.handle_key(key(KeyCode::Char('h')));
    for code in [
        KeyCode::Char('h'),
        KeyCode::Char('i'),
        KeyCode::Enter,
        KeyCode::Char('q'),
    ] {
        assert!(!app.handle_key(key(code)));
        assert!(matches!(screen(&app).page, ManagementPage::Loading { .. }));
    }
    app.handle_key(key(KeyCode::Esc));
    assert!(matches!(
        screen(&app).page,
        ManagementPage::Loading {
            cancelling: true,
            ..
        }
    ));
    assert!(app.management_task.is_some());
    finished(&mut app).await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(fake.completed.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_even_before_a_worker_acquires_the_session_permit() {
    let (_directory, mut app) = fixture();
    let fake = Arc::new(FakeGateway::new(true));
    app.management_gateway = fake.clone();
    app.handle_key(key(KeyCode::Char('h')));
    app.shutdown();
    assert!(fake.completed.load(Ordering::SeqCst));
    assert!(app.management_task.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modified_shortcuts_never_confirm_and_ctrl_c_requests_cancellation() {
    let (_directory, mut app) = fixture();
    let fake = Arc::new(FakeGateway::new(true));
    app.management_gateway = fake.clone();
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
    assert!(app.management_task.is_none());
    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(
        screen(&app).page,
        ManagementPage::Loading {
            cancelling: true,
            ..
        }
    ));
    finished(&mut app).await;
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_panic_returns_to_origin_without_exposing_the_panic_payload() {
    let (_directory, mut app) = fixture();
    let mut fake = FakeGateway::new(false);
    fake.panic = true;
    app.management_gateway = Arc::new(fake);
    app.handle_key(key(KeyCode::Char('h')));
    finished(&mut app).await;
    assert!(matches!(screen(&app).page, ManagementPage::Failed { .. }));
    let message = app.message.unwrap();
    assert!(message.contains("stopped unexpectedly"));
    assert!(!message.contains("private-secret"));
}

#[test]
fn stale_results_do_not_replace_current_screen_or_clear_the_active_request() {
    let (_directory, mut app) = fixture();
    let id = uuid::Uuid::now_v7();
    app.management_task = Some(ManagementTask {
        id,
        origin: screen(&app),
        cancellation: CancellationToken::new(),
        execution_progress: None,
        request: ManagementRequest::History(DeploymentQuery::default()),
    });
    app.finish_management(uuid::Uuid::now_v7(), Err("stale failure".into()));
    assert_eq!(app.management_task.as_ref().unwrap().id, id);
    assert!(app.message.is_none());
    app.finish_management(id, Err("current failure".into()));
    assert!(app.management_task.is_none());
    assert_eq!(app.message.as_deref(), Some("current failure"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspection_sends_only_the_explicitly_selected_component_subset() {
    let (_directory, mut app) = fixture();
    let fake = Arc::new(FakeGateway::new(false));
    app.management_gateway = fake.clone();
    app.handle_key(key(KeyCode::Char('i')));
    let ManagementPage::InspectSelection { names, .. } = screen(&app).page else {
        panic!("selection expected");
    };
    assert!(names.len() >= 2);
    app.handle_key(key(KeyCode::Char(' ')));
    app.handle_key(key(KeyCode::Enter));
    finished(&mut app).await;
    let requests = fake.requests.lock().unwrap();
    let ManagementRequest::Inspect { selected, source } = &requests[0] else {
        panic!("inspection request expected");
    };
    assert!(source.is_none());
    assert_eq!(selected.len(), names.len() - 1);
    assert!(!selected.contains(&names[0]));
}

#[test]
fn empty_inspection_selection_never_starts_a_worker() {
    let (_directory, mut app) = fixture();
    app.handle_key(key(KeyCode::Char('i')));
    if let Screen::Management(ManagementScreen {
        page: ManagementPage::InspectSelection { selected, .. },
        ..
    }) = &mut app.screen
    {
        selected.clear();
    }
    app.handle_key(key(KeyCode::Enter));
    assert!(app.management_task.is_none());
    assert!(render_screen(&screen(&app), &app).contains("Select at least one Component"));
    assert!(!screen(&app).help().contains("Enter inspect"));
}

#[test]
fn management_remembers_current_environment_by_id_without_historical_override() {
    let (directory, mut app) = fixture();
    let original = screen(&app).scope;
    let mut config = original.config.clone();
    let mut extra = config.environments[&original.environment].clone();
    extra.id = EnvironmentId::new();
    config.environments.insert("alpha".into(), extra);
    app.remember_environment(&config, &original.environment);
    app.open_management(directory.path().to_owned(), config.clone());
    assert_eq!(screen(&app).scope.environment, original.environment);
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.preferred_environment(&config).as_deref(), Some("alpha"));
    let renamed = config.environments.remove("alpha").unwrap();
    let remembered_id = renamed.id.clone();
    config.environments.insert("z-renamed".into(), renamed);
    app.open_management(directory.path().to_owned(), config.clone());
    assert_eq!(screen(&app).scope.environment, "z-renamed");
    let mut historical = screen(&app);
    Arc::make_mut(&mut historical.scope).historical_environment =
        Some(config.environments[&original.environment].id.clone());
    app.screen = Screen::Management(historical);
    app.handle_key(key(KeyCode::Left));
    assert_eq!(
        app.preferred_environment(&config).as_deref(),
        Some("z-renamed")
    );
    app.handle_key(key(KeyCode::Esc));
    app.open_management(directory.path().to_owned(), config);
    assert_eq!(screen(&app).scope.environment_id(), Some(&remembered_id));
    assert!(screen(&app).scope.historical_environment.is_none());
}

#[test]
fn records_without_frozen_context_offer_only_local_actions() {
    use crate::{
        application::history_query::HistoryQueryService, history::HistoryStore, telemetry::Redactor,
    };
    let (directory, mut app) = fixture();
    let mut current = screen(&app);
    let history = directory.path().join("context-history.sqlite3");
    let store = HistoryStore::open(&history).unwrap();
    let id = DeploymentId::new();
    store
        .create_deployment(
            &id,
            &current.scope.config.project_id,
            current.scope.environment_id().unwrap(),
            1,
        )
        .unwrap();
    let details = HistoryQueryService::new(history, Redactor::default())
        .deployment(
            &current.scope.config.project_id,
            current.scope.environment_id().unwrap(),
            &id,
        )
        .unwrap();
    assert!(details.snapshots.is_empty());
    current.page = ManagementPage::Detail(Arc::new(details));
    app.screen = Screen::Management(current);
    for code in [KeyCode::Char('i'), KeyCode::Char('r')] {
        app.handle_key(key(code));
        assert!(app.management_task.is_none());
        assert!(matches!(screen(&app).page, ManagementPage::Detail(_)));
        assert_eq!(app.message.as_deref(), Some(MISSING_FROZEN_CONTEXT));
    }
    let current = screen(&app);
    assert!(!current.help().contains("r rollback") && !current.help().contains("i inspect"));
    assert!(current.help().contains("l logs"));
    assert!(render_screen(&current, &app).contains("no frozen Component context"));
}

#[test]
fn local_pagination_has_explicit_edges_and_no_implicit_fetch_on_selection() {
    let empty = HistoryPage::<DeploymentRecord> {
        items: Vec::new(),
        more: false,
        database_missing: false,
    };
    let mut cursor = 0;
    assert!(history_key(KeyCode::Enter, &empty, 0, &mut cursor).is_none());
    assert!(history_key(KeyCode::Char('n'), &empty, 0, &mut cursor).is_none());
    assert!(history_key(KeyCode::Char('b'), &empty, 0, &mut cursor).is_none());
    let request = history_key(KeyCode::Char('b'), &empty, PAGE_SIZE, &mut cursor).unwrap();
    assert!(matches!(
        request,
        ManagementRequest::History(DeploymentQuery { offset: 0, .. })
    ));
}

#[test]
fn terminal_controls_are_filtered_and_home_does_not_expose_driver_identifiers() {
    assert_eq!(
        render::safe_text("host\u{1b}[2J\n\u{202e}evil"),
        "host[2Jevil"
    );
    let (_directory, app) = fixture();
    assert!(render_screen(&screen(&app), &app).contains("Opening history never connects"));
    assert!(!render_screen(&screen(&app), &app).contains("linux-ssh"));
}

#[test]
fn unavailable_rollback_options_cannot_be_selected_and_empty_enter_does_nothing() {
    let name = ComponentName::parse("api").unwrap();
    let candidates = RollbackCandidates {
        source: DeploymentId::new(),
        components: vec![crate::application::RollbackComponentCandidates {
            component: name.clone(),
            destination: "old.example revision 1".into(),
            root: "/srv/api".into(),
            options: vec![crate::application::RollbackOption {
                target: None,
                unavailable: Some("absence was never observed".into()),
            }],
            unavailable: None,
        }],
    };
    let mut selected = BTreeSet::new();
    let mut options = BTreeMap::new();
    let mut cursor = 0;
    assert!(
        rollback_key(
            KeyCode::Char(' '),
            &candidates,
            &mut selected,
            &mut options,
            &mut cursor
        )
        .is_none()
    );
    assert!(selected.is_empty());
    assert!(
        rollback_key(
            KeyCode::Enter,
            &candidates,
            &mut selected,
            &mut options,
            &mut cursor
        )
        .is_none()
    );
}
