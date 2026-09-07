use super::*;

fn target_page(app: &mut App, component: &str) {
    let environment = environment(app, "production");
    let form = TargetForm {
        target: environment.targets[&name(component)].clone(),
        environment,
        component: name(component),
    };
    set_page(app, ProjectEditPage::Target { form, cursor: 0 });
}

fn remote_text(app: &mut App, key: char, value: &str) {
    assert!(matches!(app.screen, Screen::RemoteSetupSelection(_)));
    press(app, KeyCode::Char(key));
    press(app, KeyCode::Delete);
    for character in value.chars() {
        press(app, KeyCode::Char(character));
    }
    press(app, KeyCode::Enter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_target_choices_remain_draft_until_all_forms_and_exact_yaml_are_confirmed() {
    let mut fixture = Fixture::new().await;
    let before = fixture.bytes();
    // Use the real page route from the editor home, not an injected worker result.
    press(&mut fixture.app, KeyCode::Char('e'));
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Down);
    press(&mut fixture.app, KeyCode::Enter);
    let ProjectEditPage::Target { form, .. } = screen(&fixture.app).page else {
        panic!("target");
    };
    assert_eq!(form.component, name("worker"));
    press(&mut fixture.app, KeyCode::Char('b'));
    assert!(matches!(
        fixture.app.screen,
        Screen::RemoteSetupSelection(_)
    ));
    assert!(fixture.app.remote_target_task.is_none());
    remote_text(&mut fixture.app, 'r', "/srv/edited-worker");
    remote_text(&mut fixture.app, 'm', "tasks.service");
    assert_eq!(fixture.bytes(), before);
    fixture
        .app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    assert!(matches!(
        fixture.app.screen,
        Screen::RemoteSetupSelection(_)
    ));
    press(&mut fixture.app, KeyCode::Enter);
    let current = screen(&fixture.app);
    let ProjectEditPage::Target { form, .. } = &current.page else {
        panic!("target");
    };
    assert_eq!(form.target.root.as_deref(), Some("/srv/edited-worker"));
    assert_eq!(
        form.target
            .service
            .as_ref()
            .and_then(crate::config::ServiceConfig::preset_unit),
        Some("tasks.service")
    );
    assert_ne!(
        current.draft.unwrap().setup.environments["production"].components[&name("worker")]
            .root
            .as_deref(),
        Some("/srv/edited-worker")
    );
    for _ in 0..5 {
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter); // Apply Target to Environment form.
    for _ in 0..3 {
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter); // Apply Environment to local project draft.
    press(&mut fixture.app, KeyCode::Esc);
    assert!(matches!(screen(&fixture.app).page, ProjectEditPage::Home));
    press(&mut fixture.app, KeyCode::Char('p'));
    finished(&mut fixture.app).await;
    let ProjectEditPage::Preview(preview) = screen(&fixture.app).page else {
        panic!("preview: {:?}", fixture.app.message);
    };
    assert_eq!(fixture.bytes(), before);
    let production = &preview.config().environments["production"];
    assert_eq!(
        production.components[&name("worker")].root,
        "/srv/edited-worker"
    );
    assert_eq!(
        production.components[&name("worker")]
            .service
            .as_ref()
            .and_then(crate::config::ServiceConfig::preset_unit),
        Some("tasks.service")
    );
    assert_eq!(
        production.components[&name("backend")],
        fixture.original.environments["production"].components[&name("backend")]
    );
    assert_eq!(
        preview.config().environments["staging"],
        fixture.original.environments["staging"]
    );
    press(&mut fixture.app, KeyCode::F(1));
    press(&mut fixture.app, KeyCode::Char('c')); // Help must own the confirmation key.
    assert_eq!(fixture.bytes(), before);
    assert!(fixture.app.project_edit_task.is_none());
    press(&mut fixture.app, KeyCode::Esc);
    fixture
        .app
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(fixture.bytes(), before);
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_eq!(fixture.bytes(), preview.yaml().as_bytes());
    assert!(!fixture.directory.path().join("history.sqlite3").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_project_custom_service_keyboard_edit_saves_only_confirmed_component() {
    let mut fixture = Fixture::new().await;
    let before = fixture.bytes();
    target_page(&mut fixture.app, "worker");
    press(&mut fixture.app, KeyCode::Char('b'));
    press(&mut fixture.app, KeyCode::Char('c'));
    for (stage, phase) in [(0, "activate"), (3, "stop")] {
        press(&mut fixture.app, KeyCode::Home);
        for _ in 0..stage {
            press(&mut fixture.app, KeyCode::Down);
        }
        press(&mut fixture.app, KeyCode::Enter);
        for value in ["node", "service.cjs", phase] {
            press(&mut fixture.app, KeyCode::Char('a'));
            for c in value.chars() {
                press(&mut fixture.app, KeyCode::Char(c));
            }
            press(&mut fixture.app, KeyCode::Enter);
        }
        press(&mut fixture.app, KeyCode::Esc);
        press(&mut fixture.app, KeyCode::Esc);
    }
    press(&mut fixture.app, KeyCode::End);
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Enter);
    assert_eq!(fixture.bytes(), before);
    for _ in 0..5 {
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter);
    for _ in 0..3 {
        press(&mut fixture.app, KeyCode::Down);
    }
    press(&mut fixture.app, KeyCode::Enter);
    press(&mut fixture.app, KeyCode::Esc);
    press(&mut fixture.app, KeyCode::Char('p'));
    finished(&mut fixture.app).await;
    let ProjectEditPage::Preview(preview) = screen(&fixture.app).page else {
        panic!("preview");
    };
    let target = &preview.config().environments["production"].components[&name("worker")];
    assert_eq!(
        target.service.as_ref().unwrap().start[0],
        ["node", "service.cjs", "activate"]
    );
    assert_eq!(target.generation.get(), 2);
    assert_eq!(fixture.bytes(), before);
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    let config::ProjectConfigState::Loaded(saved) = config::load(fixture.directory.path()).unwrap()
    else {
        panic!("saved");
    };
    assert_eq!(&saved, preview.config());
    assert_eq!(
        saved.environments["production"].components[&name("backend")],
        fixture.original.environments["production"].components[&name("backend")]
    );
    assert_eq!(
        saved.environments["staging"],
        fixture.original.environments["staging"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_target_cancel_discards_root_and_service_edits_and_does_not_read_ssh() {
    let mut fixture = Fixture::new().await;
    let before = fixture.bytes();
    target_page(&mut fixture.app, "worker");
    let ProjectEditPage::Target { form: original, .. } = screen(&fixture.app).page else {
        panic!();
    };
    press(&mut fixture.app, KeyCode::Char('b'));
    remote_text(&mut fixture.app, 'r', "/srv/unsaved-worker");
    remote_text(&mut fixture.app, 'm', "unsaved.service");
    press(&mut fixture.app, KeyCode::Esc);
    let ProjectEditPage::Target { form, .. } = screen(&fixture.app).page else {
        panic!();
    };
    assert_eq!(form.target, original.target);
    assert!(fixture.app.remote_target_task.is_none());
    assert_eq!(fixture.bytes(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_choices_preserve_resolved_root_after_project_rename_and_reject_missing_connection()
{
    let mut fixture = Fixture::new().await;
    target_page(&mut fixture.app, "worker");
    let mut current = screen(&fixture.app);
    Arc::make_mut(current.draft.as_mut().unwrap()).setup.project = "renamed".into();
    let ProjectEditPage::Target { form, .. } = &mut current.page else {
        panic!();
    };
    form.target.root = None;
    let (_, _, project, _, root, _) = current.remote_target_context().unwrap();
    assert_eq!(project, "renamed");
    assert_eq!(
        root,
        fixture.original.environments["production"].components[&name("worker")].root
    );
    let ProjectEditPage::Target { form, .. } = &mut current.page else {
        panic!();
    };
    form.target.destination = DestinationKey::new();
    fixture.app.screen = Screen::ProjectEdit(current);
    press(&mut fixture.app, KeyCode::Char('b'));
    assert!(matches!(fixture.app.screen, Screen::ProjectEdit(_)));
    assert!(fixture.app.remote_target_task.is_none());
    assert!(
        fixture
            .app
            .message
            .as_deref()
            .unwrap()
            .contains("available connection")
    );
}
