use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use tempfile::tempdir;

use crate::tui::app::{App, DirectoryBrowser, Screen};

fn press(app: &mut App, key: KeyCode) {
    assert!(!app.handle_key(KeyEvent::new(key, KeyModifiers::NONE)));
}

#[test]
fn project_search_only_focuses_without_opening_directory_or_creating_storage() {
    let root = tempdir().unwrap();
    let mut app = App::new(
        root.path().join("projects.yaml"),
        root.path().join("destinations.yaml"),
        root.path(),
    )
    .unwrap();
    press(&mut app, KeyCode::F(4));
    assert!(app.picker.is_some());
    for c in "browse".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);
    assert!(app.picker.is_none());
    assert!(matches!(app.screen, Screen::Projects));
    assert!(!root.path().join("projects.yaml").exists());
    assert!(!root.path().join("history.sqlite3").exists());
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::Browser(_)));
}

#[test]
fn component_search_and_cancel_preserve_subset_and_same_environment_identity() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("shipforge.yaml"),
        include_str!("../../../../docs/examples/shipforge.yaml"),
    )
    .unwrap();
    let crate::config::ProjectConfigState::Loaded(config) =
        crate::config::load(root.path()).unwrap()
    else {
        panic!("fixture")
    };
    let mut app = App::new(
        root.path().join("projects.yaml"),
        root.path().join("destinations.yaml"),
        root.path(),
    )
    .unwrap();
    app.open_deployment(root.path().into(), config);
    let Screen::DeploySelection(selection) = &mut app.screen else {
        panic!("selection")
    };
    selection.selected.clear();
    press(&mut app, KeyCode::F(4));
    for c in "environment production".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);
    let Screen::DeploySelection(selection) = &app.screen else {
        panic!("selection")
    };
    assert!(selection.selected.is_empty());
    press(&mut app, KeyCode::F(4));
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Esc);
    let Screen::DeploySelection(selection) = &app.screen else {
        panic!("selection")
    };
    assert!(selection.selected.is_empty());
    assert!(!root.path().join("history.sqlite3").exists());
}

#[test]
fn help_owns_confirmation_keys_and_keeps_full_message_available() {
    let root = tempdir().unwrap();
    let mut app = App::new(
        root.path().join("projects.yaml"),
        root.path().join("destinations.yaml"),
        root.path(),
    )
    .unwrap();
    app.message =
        Some("A long actionable message that must survive opening and closing help".into());
    press(&mut app, KeyCode::F(1));
    assert!(app.help_open);
    press(&mut app, KeyCode::Char('c'));
    assert!(matches!(app.screen, Screen::Projects));
    press(&mut app, KeyCode::Esc);
    assert!(!app.help_open);
    assert!(app.message.as_deref().unwrap().contains("actionable"));
}

#[test]
fn changed_directory_choices_are_rejected_without_focusing_a_different_path() {
    let root = tempdir().unwrap();
    let mut app = App::new(
        root.path().join("projects.yaml"),
        root.path().join("destinations.yaml"),
        root.path(),
    )
    .unwrap();
    app.screen = Screen::Browser(DirectoryBrowser {
        directory: root.path().into(),
        children: vec![root.path().join("first"), root.path().join("second")],
        selected: 0,
    });
    press(&mut app, KeyCode::F(4));
    press(&mut app, KeyCode::Down);
    let Screen::Browser(browser) = &mut app.screen else {
        panic!("browser")
    };
    browser.children.reverse();
    press(&mut app, KeyCode::Enter);
    let Screen::Browser(browser) = &app.screen else {
        panic!("browser")
    };
    assert_eq!(browser.selected, 0);
    assert!(app.message.as_deref().unwrap().contains("Choices changed"));
}

#[test]
fn long_directory_list_keeps_keyboard_focus_visible_and_help_renders_message() {
    let root = tempdir().unwrap();
    let mut app = App::new(
        root.path().join("projects.yaml"),
        root.path().join("destinations.yaml"),
        root.path(),
    )
    .unwrap();
    app.screen = Screen::Browser(DirectoryBrowser {
        directory: root.path().into(),
        children: (0..120)
            .map(|index| root.path().join(format!("directory-{index:03}")))
            .collect(),
        selected: 0,
    });
    for _ in 0..119 {
        press(&mut app, KeyCode::Down);
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| crate::tui::render(frame, &app))
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(text.contains("> directory-119"));
    assert!(text.contains("F1 Help  F4 Find  F6 Language"));
    app.message = Some("Preserve the diagnostic while opening help".into());
    press(&mut app, KeyCode::F(1));
    assert!(app.message.as_deref().unwrap().contains("diagnostic"));
}
