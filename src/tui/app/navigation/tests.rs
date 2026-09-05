use std::{collections::BTreeSet, sync::Arc};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use tempfile::{TempDir, tempdir};
use tokio_util::sync::CancellationToken;

use crate::{
    application::{DeploymentPlan, DeploymentSelection, GitMetadata, GitWorktreeState},
    config::{
        DestinationRegistry, DestinationSettings, HostKeyFingerprint, ProjectConfig,
        ProjectConfigState,
    },
    domain::{DestinationKey, EnvironmentId, ProjectId},
    drivers::CredentialHandle,
    telemetry::log_record::{LogEvent, LogEventKind},
};

use super::super::{App, LocalDeploymentGateway, Screen, TuiDeploymentGateway};

fn fixture() -> (TempDir, App, ProjectConfig) {
    let directory = tempdir().unwrap();
    std::fs::write(
        directory.path().join("shipforge.yaml"),
        include_str!("../../../../docs/examples/shipforge.yaml"),
    )
    .unwrap();
    let ProjectConfigState::Loaded(mut config) = crate::config::load(directory.path()).unwrap()
    else {
        panic!("fixture config")
    };
    let mut staging = config.environments["production"].clone();
    staging.id = EnvironmentId::new();
    config.environments.insert("staging".into(), staging);
    let app = App::new(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        directory.path(),
    )
    .unwrap();
    (directory, app, config)
}

fn press(app: &mut App, code: KeyCode) {
    assert!(!app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn draw(app: &App, width: u16, height: u16) -> Vec<String> {
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
        .collect()
}

#[test]
fn environment_choice_survives_view_changes_and_only_same_id_renames() {
    let (directory, mut app, mut config) = fixture();
    app.show_overview(directory.path().into(), config.clone());
    press(&mut app, KeyCode::Right);
    assert_eq!(
        app.preferred_environment(&config).as_deref(),
        Some("staging")
    );
    press(&mut app, KeyCode::Char('d'));
    let Screen::DeploySelection(selection) = &app.screen else {
        panic!("selection")
    };
    assert_eq!(selection.environment_cursor, 1);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('m'));
    assert!(app.context_label().contains("staging"));
    press(&mut app, KeyCode::Esc);

    let renamed = config.environments.remove("staging").unwrap();
    config.environments.insert("qa".into(), renamed);
    assert_eq!(app.preferred_environment(&config).as_deref(), Some("qa"));
    config.environments.get_mut("qa").unwrap().id = EnvironmentId::new();
    assert_eq!(
        app.preferred_environment(&config).as_deref(),
        Some("production")
    );
    app.remember_environment(&config, "qa");
    config.project_id = ProjectId::new();
    assert_eq!(
        app.preferred_environment(&config).as_deref(),
        Some("production")
    );
}

#[test]
fn deployment_environment_choice_is_returned_to_overview_and_management() {
    let (directory, mut app, config) = fixture();
    app.open_deployment(directory.path().into(), config.clone());
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Esc);
    assert!(app.context_label().contains("staging"));
    press(&mut app, KeyCode::Char('m'));
    assert!(app.context_label().contains("staging"));
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('d'));
    let Screen::DeploySelection(selection) = &app.screen else {
        panic!("selection")
    };
    assert_eq!(selection.environment_cursor, 0);
}

#[test]
fn connection_display_is_local_only_and_failed_refresh_drops_stale_endpoint() {
    let (directory, mut app, config) = fixture();
    let key = DestinationKey::new();
    let mut registry = DestinationRegistry::new();
    registry
        .create(
            key.clone(),
            DestinationSettings::LinuxSsh {
                host: "2001:db8::1".into(),
                port: 2222,
                user: "deploy".into(),
                credential: CredentialHandle::new(),
                host_key: HostKeyFingerprint::parse("SHA256:test-key").unwrap(),
            },
        )
        .unwrap();
    registry
        .save(&directory.path().join("destinations.yaml"))
        .unwrap();
    std::fs::write(
        directory.path().join("credentials.yaml"),
        "invalid: [secret must not be read",
    )
    .unwrap();
    let before = std::fs::read(directory.path().join("destinations.yaml")).unwrap();
    app.show_overview(directory.path().into(), config);
    assert!(
        app.destination_label(&key)
            .contains("deploy@[2001:db8::1]:2222")
    );
    assert!(app.destination_label(&key).contains(key.as_str()));
    assert_eq!(
        std::fs::read(directory.path().join("destinations.yaml")).unwrap(),
        before
    );
    assert!(!directory.path().join("history.sqlite3").exists());
    std::fs::write(
        directory.path().join("destinations.yaml"),
        "invalid: [do-not-echo",
    )
    .unwrap();
    app.refresh_destination_labels();
    let label = app.destination_label(&key);
    assert!(label.contains("unknown"));
    assert!(!label.contains("2001:db8"));
    assert!(!label.contains("do-not-echo"));
}

#[tokio::test]
async fn planning_gateway_does_not_echo_malformed_registry_or_credential_contents() {
    let (directory, _app, config) = fixture();
    let gateway = LocalDeploymentGateway {
        destinations: directory.path().join("destinations.yaml"),
        credentials: directory.path().join("credentials.yaml"),
        history: directory.path().join("history.sqlite3"),
    };
    let selection = DeploymentSelection {
        project_root: directory.path().into(),
        config,
        environment: "production".into(),
        components: BTreeSet::new(),
    };
    let secret = "private-input-sentinel";
    std::fs::write(&gateway.destinations, format!("'{secret}'")).unwrap();
    let error = gateway
        .plan(selection.clone(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.contains("Saved connections"));
    assert!(!error.contains(secret));
    DestinationRegistry::new()
        .save(&gateway.destinations)
        .unwrap();
    std::fs::write(&gateway.credentials, format!("'{secret}'")).unwrap();
    let error = gateway
        .plan(selection, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.contains("SSH identities"));
    assert!(!error.contains(secret));
    assert!(!gateway.history.exists());
}

#[test]
fn production_context_and_confirmation_remain_visible_when_plan_is_scrolled() {
    let (directory, mut app, config) = fixture();
    let selection = DeploymentSelection {
        project_root: directory.path().into(),
        config,
        environment: "production".into(),
        components: BTreeSet::new(),
    };
    app.screen = Screen::DeploymentReview {
        plan: DeploymentPlan {
            selection,
            activation_order: vec![],
            entries: vec![],
            git: GitWorktreeState::Clean,
            git_metadata: GitMetadata::default(),
        },
        scroll: 999,
    };
    app.message = Some("Check targets before confirming".into());
    let lines = draw(&app, 80, 10);
    assert!(lines[0].starts_with("[PRODUCTION]"));
    assert!(lines[0].contains("production"));
    assert!(lines[1].contains("Confirm deployment"));
    assert!(lines[8].contains("c confirm"));
    assert!(lines[9].contains("Check targets"));
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::DeploymentReview { .. }));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::DeploySelection(_)));
}

#[test]
fn live_and_finished_headers_keep_scope_and_hide_internal_step_namespace() {
    let (directory, mut app, config) = fixture();
    app.remember_environment(&config, "production");
    assert!(app.live_logs.push(crate::tui::log_view::LogRow {
        sequence: 1,
        elapsed_ms: None,
        event: Arc::new(LogEvent {
            namespace: "linux-ssh.upload".into(),
            message: "payload transferred".into(),
            scope: None,
            kind: LogEventKind::Output,
        }),
    }));
    app.screen = Screen::DeploymentRunning {
        root: directory.path().into(),
        config: config.clone(),
        cancellation: CancellationToken::new(),
        cancellation_requested: false,
    };
    let text = draw(&app, 100, 14).join("\n");
    assert!(text.starts_with("[PRODUCTION]"));
    assert!(text.contains("payload transferred"));
    assert!(!text.contains("linux-ssh"));
    app.screen = Screen::DeploymentFinished {
        root: directory.path().into(),
        config,
        summary: "Completed".into(),
        scroll: 0,
    };
    let text = draw(&app, 100, 14).join("\n");
    assert!(text.contains("production"));
    assert!(!text.contains("linux-ssh"));
}

#[test]
fn empty_component_selection_does_not_advertise_an_executable_check() {
    let (directory, mut app, config) = fixture();
    app.open_deployment(directory.path().into(), config);
    let Screen::DeploySelection(selection) = &mut app.screen else {
        panic!("selection")
    };
    selection.selected.clear();
    let lines = draw(&app, 80, 10);
    assert!(lines[8].contains("required"));
    assert!(!lines[8].contains("Enter check"));
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::DeploySelection(_)));
    assert!(app.message.as_deref().unwrap().contains("Select at least"));
}

#[test]
fn clamped_environment_navigation_does_not_reselect_components() {
    let (directory, mut app, config) = fixture();
    app.open_deployment(directory.path().into(), config);
    let Screen::DeploySelection(selection) = &mut app.screen else {
        panic!("selection")
    };
    selection.selected.clear();
    press(&mut app, KeyCode::Left);
    let Screen::DeploySelection(selection) = &app.screen else {
        panic!("selection")
    };
    assert!(selection.selected.is_empty());
}

#[test]
fn overview_scroll_reaches_targets_without_losing_fixed_environment_context() {
    let (directory, mut app, config) = fixture();
    app.show_overview(directory.path().into(), config);
    let mut found_worker = false;
    for _ in 0..50 {
        let lines = draw(&app, 80, 10);
        assert!(lines[0].starts_with("[PRODUCTION]"));
        assert!(lines[0].contains("production"));
        found_worker |= lines[2..8].join(" ").contains("worker →");
        press(&mut app, KeyCode::Down);
    }
    assert!(found_worker);
    press(&mut app, KeyCode::Home);
    assert_eq!(app.overview_scroll(), 0);
    press(&mut app, KeyCode::PageDown);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.overview_scroll(), 0);
    assert!(app.context_label().contains("staging"));
}

#[tokio::test]
async fn active_session_footer_shows_actual_cancel_action_and_cancellation_state() {
    let (directory, mut app, config) = fixture();
    app.remember_environment(&config, "production");
    app.screen = Screen::DeploymentRunning {
        root: directory.path().into(),
        config,
        cancellation: CancellationToken::new(),
        cancellation_requested: false,
    };
    let session = std::sync::Arc::clone(&app.deployment_session);
    session
        .run(async {
            assert!(app.deployment_session.is_active());
            let lines = draw(&app, 100, 12);
            assert!(lines[10].contains("Esc request safe cancellation"));
            assert!(!lines[10].contains("return to progress"));
            press(&mut app, KeyCode::Esc);
            let lines = draw(&app, 100, 12);
            assert!(lines[10].contains("Safe cancellation requested"));
        })
        .await
        .unwrap();
}
