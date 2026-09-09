use super::*;

fn press(app: &mut App, code: KeyCode) {
    if code == KeyCode::Enter {
        assert!(!app.enter_primary());
        return;
    }
    assert!(!app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
}

fn type_value(app: &mut App, value: &str) {
    press(app, KeyCode::Delete);
    for character in value.chars() {
        press(app, KeyCode::Char(character));
    }
    press(app, KeyCode::Enter);
}

#[test]
fn empty_project_can_complete_manual_setup_without_builds_or_writes_before_yaml_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let destinations_path = directory.path().join("destinations.yaml");
    let mut destinations = DestinationRegistry::new();
    destinations
        .create(
            DestinationKey::new(),
            DestinationSettings::LinuxSsh {
                host: "test.invalid".into(),
                port: 22,
                user: "deploy".into(),
                credential: CredentialHandle::new(),
                host_key: HostKeyFingerprint::parse("SHA256:test-only").unwrap(),
            },
        )
        .unwrap();
    destinations.save(&destinations_path).unwrap();
    let mut app = App::new(
        directory.path().join("projects.yaml"),
        destinations_path,
        directory.path(),
    )
    .unwrap();
    app.select_root(directory.path());
    let Screen::SetupComponents(setup) = &app.screen else {
        panic!();
    };
    assert!(setup.report.components.is_empty());
    press(&mut app, KeyCode::Char('a'));
    assert!(matches!(app.screen, Screen::ManualComponent(_)));
    press(&mut app, KeyCode::Enter);
    type_value(&mut app, "worker");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    type_value(&mut app, "dist/worker");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('a'));
    type_value(&mut app, "shipforge-test-do-not-execute");
    press(&mut app, KeyCode::Char('a'));
    type_value(&mut app, "one literal argument with spaces");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert!(!directory.path().join("shipforge.yaml").exists());
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SHIFT));
    assert!(matches!(app.screen, Screen::ManualComponent(_)));
    press(&mut app, KeyCode::Char('c'));
    let Screen::SetupComponents(setup) = &app.screen else {
        panic!();
    };
    assert_eq!(setup.selected.len(), 1);
    assert_eq!(
        setup.report.components[0].setup.working_directory,
        Some(PathBuf::from("."))
    );
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('n'));
    let Screen::SetupReview { prepared, .. } = &app.screen else {
        panic!("YAML preview: {:?}", app.message);
    };
    let yaml = prepared.preview().to_owned();
    assert!(yaml.contains("shipforge-test-do-not-execute"));
    press(&mut app, KeyCode::F(1));
    press(&mut app, KeyCode::Char('c'));
    assert!(!directory.path().join("shipforge.yaml").exists());
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('c'));
    assert!(matches!(app.screen, Screen::Overview { .. }));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("shipforge.yaml")).unwrap(),
        yaml
    );
    assert!(!directory.path().join("history.sqlite3").exists());
    assert!(!directory.path().join("dist").exists());
}

#[test]
fn invalid_recent_registry_does_not_prevent_opening_tui_or_overwrite_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let registry = directory.path().join("projects.yaml");
    let invalid = "projects: {malformed: secret-parser-sentinel}";
    std::fs::write(&registry, invalid).unwrap();
    let mut app = App::new(
        registry.clone(),
        directory.path().join("destinations.yaml"),
        directory.path(),
    )
    .unwrap();
    assert!(app.recent_unavailable);
    assert!(
        !app.message
            .as_deref()
            .unwrap()
            .contains("secret-parser-sentinel")
    );
    press(&mut app, KeyCode::Down);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| crate::tui::render(frame, &app))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(rendered.contains("Recent projects unavailable"));
    press(&mut app, KeyCode::Char('o'));
    assert!(matches!(app.screen, Screen::Browser(_)));
    assert_eq!(std::fs::read_to_string(&registry).unwrap(), invalid);
    // Recovery is a fresh read of valid evidence, never reconstruction of missing project YAML.
    ProjectRegistry::new().save(&registry).unwrap();
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('f'));
    assert!(!app.recent_unavailable);
    assert!(app.recent.is_empty());
}
