//! Removed configuration must not make preserved local evidence inaccessible.

use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use crate::{
    domain::{ComponentGeneration, DestinationKey, DestinationRevision, EnvironmentId},
    drivers::{DriverKind, EndpointFingerprint},
    history::{
        CurrentAlignment, HistoryStore, InspectionScope, PackageAlignment, RecoveryComponentReport,
        RollingLogWriter,
    },
    telemetry::Redactor,
};

use super::tests::{fixture, screen};
use super::*;

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

async fn finish(app: &mut App) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            app.poll_background();
            if app.management_task.is_none() && !app.log_workspace_busy() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("local management query must finish");
}

fn rendered(app: &App, width: u16, height: u16) -> String {
    let screen = screen(app);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area(), app))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn report(scope: &ManagementScope, environment: &EnvironmentId) -> RecoveryReport {
    RecoveryReport {
        id: uuid::Uuid::now_v7(),
        related_deployment: None,
        source_revision: None,
        started_at_ms: 10,
        completed_at_ms: 11,
        components: vec![RecoveryComponentReport {
            scope: InspectionScope {
                project: scope.config.project_id.clone(),
                environment: environment.clone(),
                component: ComponentName::parse("frontend").unwrap(),
                generation: ComponentGeneration::INITIAL,
                driver: DriverKind::linux_ssh(),
                destination: DestinationKey::new(),
                destination_revision: DestinationRevision::INITIAL,
                endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            },
            inventory: Err("recorded unknown state".into()),
            alignment: CurrentAlignment::Unknown,
            package_alignment: PackageAlignment::Unknown,
            notices: Vec::new(),
        }],
    }
}

async fn select_historical_environment(app: &mut App, id: &EnvironmentId) {
    press(app, KeyCode::Char('a'));
    finish(app).await;
    let ManagementPage::Environments { page, .. } = screen(app).page else {
        panic!("historical Environment selector expected");
    };
    let index = page.items.iter().position(|item| item == id).unwrap();
    for _ in 0..index {
        press(app, KeyCode::Down);
    }
    press(app, KeyCode::Enter);
    assert_eq!(screen(app).scope.historical_environment.as_ref(), Some(id));
    assert!(matches!(screen(app).page, ManagementPage::Home));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_environment_history_logs_and_reports_survive_same_name_recreation() {
    let (directory, mut app) = fixture();
    let original = screen(&app).scope;
    let old_environment = original.config.environments[&original.environment]
        .id
        .clone();
    let new_environment = EnvironmentId::new();
    let path = directory.path().join("shipforge.yaml");
    let original_yaml = std::fs::read_to_string(&path).unwrap();
    let new_yaml =
        original_yaml.replace(&old_environment.to_string(), &new_environment.to_string());
    std::fs::write(&path, &new_yaml).unwrap();
    let crate::config::ProjectConfigState::Loaded(config) =
        crate::config::load(directory.path()).unwrap()
    else {
        panic!("same name with a new Environment identity must be a valid fixture");
    };
    let store = HistoryStore::open(&directory.path().join("history.sqlite3")).unwrap();
    let old_deployment = DeploymentId::new();
    let new_deployment = DeploymentId::new();
    store
        .create_deployment(
            &old_deployment,
            &original.config.project_id,
            &old_environment,
            1,
        )
        .unwrap();
    store
        .create_deployment(
            &new_deployment,
            &original.config.project_id,
            &new_environment,
            2,
        )
        .unwrap();
    store
        .register_deployment_log(&old_deployment, 1024, 1)
        .unwrap();
    let logs = directory.path().join("logs");
    std::fs::create_dir(&logs).unwrap();
    let mut writer = RollingLogWriter::open(&logs, &old_deployment, 1024, 1).unwrap();
    writer
        .append("retired-environment-log", &Redactor::default())
        .unwrap();
    drop(writer);
    let saved = report(&original, &old_environment);
    store
        .append_recovery_report(&saved, &Redactor::default())
        .unwrap();
    let before = store.deployment(&old_deployment).unwrap();
    // Local evidence must remain usable even when connection setup is unavailable.
    std::fs::write(
        directory.path().join("credentials.yaml"),
        "private-invalid-credential-input",
    )
    .unwrap();
    app.open_management(directory.path().to_owned(), config);
    browse_removed_environment(
        &mut app,
        &new_deployment,
        &old_deployment,
        &old_environment,
        &saved,
    )
    .await;
    assert_eq!(store.deployment(&old_deployment).unwrap(), before);
    assert_eq!(store.recovery_report(&saved.id).unwrap(), Some(saved));
    assert_eq!(std::fs::read_to_string(path).unwrap(), new_yaml);
}

async fn browse_removed_environment(
    app: &mut App,
    new_deployment: &DeploymentId,
    old_deployment: &DeploymentId,
    old_environment: &EnvironmentId,
    saved: &RecoveryReport,
) {
    press(app, KeyCode::Char('h'));
    finish(app).await;
    let ManagementPage::History { page, .. } = screen(app).page else {
        panic!("history expected")
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(&page.items[0].deployment, new_deployment);
    press(app, KeyCode::Esc);
    select_historical_environment(app, old_environment).await;
    assert!(rendered(app, 120, 24).contains(&old_environment.to_string()));
    assert!(!screen(app).help().contains("i inspect"));
    press(app, KeyCode::Char('h'));
    finish(app).await;
    let ManagementPage::History { page, .. } = screen(app).page else {
        panic!("old history expected")
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(&page.items[0].deployment, old_deployment);
    press(app, KeyCode::Enter);
    finish(app).await;
    assert!(matches!(screen(app).page, ManagementPage::Detail(_)));
    for code in [KeyCode::Char('i'), KeyCode::Char('r')] {
        press(app, code);
        assert!(app.management_task.is_none());
        assert!(matches!(screen(app).page, ManagementPage::Detail(_)));
    }
    press(app, KeyCode::Char('l'));
    finish(app).await;
    assert!(matches!(screen(app).page, ManagementPage::Detail(_)));
    let workspace = app.log_workspace.as_ref().expect("historical logs overlay");
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal
        .draw(|frame| workspace.render(frame, frame.area()))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(text.contains("retired-environment-log"));
    assert!(text.contains(&old_deployment.to_string()));
    press(app, KeyCode::Esc); // detail
    press(app, KeyCode::Esc); // historical Home
    press(app, KeyCode::Char('p'));
    finish(app).await;
    press(app, KeyCode::Enter);
    finish(app).await;
    let ManagementPage::Report { report, .. } = screen(app).page else {
        panic!("old report expected")
    };
    assert_eq!(report.as_ref(), saved);
    press(app, KeyCode::Esc); // historical Home
    press(app, KeyCode::Esc); // current Home
    assert!(screen(app).scope.historical_environment.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archived_scope_rejects_remote_requests_before_loading_credentials() {
    let (directory, app) = fixture();
    let mut scope = (*screen(&app).scope).clone();
    scope.historical_environment = Some(EnvironmentId::new());
    let gateway = LocalManagementGateway {
        credentials: directory.path().join("credentials.yaml"),
        destinations: directory.path().join("destinations.yaml"),
        history: directory.path().join("history.sqlite3"),
        session: Arc::clone(&app.deployment_session),
    };
    std::fs::write(&gateway.credentials, "private-invalid-input").unwrap();
    for request in [
        ManagementRequest::Inspect {
            source: None,
            selected: BTreeSet::new(),
        },
        ManagementRequest::Candidates {
            source: DeploymentId::new(),
            selected: BTreeSet::new(),
        },
        ManagementRequest::Plan {
            source: DeploymentId::new(),
            targets: BTreeMap::new(),
        },
    ] {
        let error = gateway
            .run(&scope, request, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("read-only"), "{error}");
        assert!(!error.contains("Credential") && !error.contains("private-invalid"));
    }
    assert!(!gateway.history.exists());
    assert!(!gateway.destinations.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn report_only_historical_environment_has_a_read_only_tui_entry() {
    let (directory, mut app) = fixture();
    let past_environment = EnvironmentId::new();
    let saved = report(&screen(&app).scope, &past_environment);
    let store = HistoryStore::open(&directory.path().join("history.sqlite3")).unwrap();
    store
        .append_recovery_report(&saved, &Redactor::default())
        .unwrap();
    select_historical_environment(&mut app, &past_environment).await;
    press(&mut app, KeyCode::Char('p'));
    finish(&mut app).await;
    let ManagementPage::Reports { page, .. } = screen(&app).page else {
        panic!("report-only Environment must have its saved reports");
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0], saved);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('h'));
    finish(&mut app).await;
    let ManagementPage::History { page, .. } = screen(&app).page else {
        panic!("historical Environment with no deployment history remains browsable");
    };
    assert!(page.items.is_empty() && !page.database_missing);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_environment_selector_is_bounded_and_missing_database_stays_missing() {
    let (directory, mut app) = fixture();
    press(&mut app, KeyCode::Char('a'));
    finish(&mut app).await;
    let ManagementPage::Environments { page, .. } = screen(&app).page else {
        panic!("environment list expected")
    };
    assert!(page.database_missing && page.items.is_empty());
    assert!(!directory.path().join("history.sqlite3").exists());
    press(&mut app, KeyCode::Esc);
    let scope = screen(&app).scope;
    let store = HistoryStore::open(&directory.path().join("history.sqlite3")).unwrap();
    for _ in 0..23 {
        store
            .create_deployment(
                &DeploymentId::new(),
                &scope.config.project_id,
                &EnvironmentId::new(),
                1,
            )
            .unwrap();
    }
    press(&mut app, KeyCode::Char('a'));
    finish(&mut app).await;
    let ManagementPage::Environments { page, .. } = screen(&app).page else {
        panic!("environment list expected")
    };
    assert_eq!(page.items.len(), 20);
    assert!(page.more);
    for _ in 1..20 {
        press(&mut app, KeyCode::Down);
    }
    assert!(rendered(&app, 80, 10).contains(&page.items[19].to_string()));
    press(&mut app, KeyCode::Char('n'));
    finish(&mut app).await;
    let ManagementPage::Environments { page, offset, .. } = screen(&app).page else {
        panic!("next environment page expected")
    };
    assert_eq!(offset, 20);
    assert_eq!(page.items.len(), 3);
    assert!(!page.more);
    press(&mut app, KeyCode::Char('b'));
    finish(&mut app).await;
    let ManagementPage::Environments { offset, .. } = screen(&app).page else {
        panic!("previous environment page expected")
    };
    assert_eq!(offset, 0);
}
