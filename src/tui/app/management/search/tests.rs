use super::super::{
    HistoryPage,
    tests::{fixture, screen},
};
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn historical_environment_search_only_focuses_the_loaded_page_without_adopting_identity() {
    let (directory, mut app) = fixture();
    let first = crate::domain::EnvironmentId::new();
    let second = crate::domain::EnvironmentId::new();
    let mut current = screen(&app);
    let scope = current.scope.clone();
    current.page = ManagementPage::Environments {
        page: Arc::new(HistoryPage {
            items: vec![first, second],
            more: true,
            database_missing: false,
        }),
        offset: 25,
        cursor: 0,
    };
    app.screen = Screen::Management(current);
    press(&mut app, KeyCode::F(4));
    assert!(app.picker.is_some());
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Enter);
    let current = screen(&app);
    assert!(matches!(
        current.page,
        ManagementPage::Environments {
            cursor: 1,
            offset: 25,
            ..
        }
    ));
    assert_eq!(
        current.scope.historical_environment,
        scope.historical_environment
    );
    assert_eq!(current.scope.environment, scope.environment);
    assert!(app.management_task.is_none());
    assert!(!directory.path().join("history.sqlite3").exists());
    let mut current = screen(&app);
    Arc::make_mut(&mut current.scope).historical_environment =
        Some(crate::domain::EnvironmentId::new());
    current.page = ManagementPage::Home;
    app.screen = Screen::Management(current);
    press(&mut app, KeyCode::F(4));
    assert!(app.picker.is_none()); // A historical home must not switch to current YAML environments.
    assert!(app.management_task.is_none());
    assert!(!directory.path().join("history.sqlite3").exists());
}

#[test]
fn inspection_search_preserves_checked_subset_without_starting_inspection() {
    let (directory, mut app) = fixture();
    let names: Vec<_> = ["backend", "worker"]
        .into_iter()
        .map(|name| crate::domain::ComponentName::parse(name).unwrap())
        .collect();
    let selected = std::collections::BTreeSet::from([names[0].clone()]);
    let mut current = screen(&app);
    current.page = ManagementPage::InspectSelection {
        source: None,
        historical_releases: Vec::new(),
        names,
        selected: selected.clone(),
        cursor: 0,
    };
    app.screen = Screen::Management(current);
    press(&mut app, KeyCode::F(4));
    press(&mut app, KeyCode::End);
    press(&mut app, KeyCode::Enter);
    let ManagementPage::InspectSelection {
        selected: after,
        cursor,
        ..
    } = screen(&app).page
    else {
        panic!();
    };
    assert_eq!(after, selected);
    assert_eq!(cursor, 1);
    assert!(app.management_task.is_none());
    assert!(!directory.path().join("history.sqlite3").exists());
}
