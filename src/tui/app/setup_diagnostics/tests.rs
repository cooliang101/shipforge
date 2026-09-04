use std::{collections::BTreeMap, path::PathBuf};

use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};

use crate::{
    config::{ArtifactSpec, BuildCommand, ComponentSetup, CredentialRegistry, DestinationSummary},
    domain::{ComponentName, DestinationKey, DestinationRevision},
    drivers::DriverKind,
    projects::{ComponentCandidate, DiscoveryConfidence, DiscoveryReport, ProjectRegistry},
    tui::app::{
        App, ComponentSetupState, CredentialChoice, DestinationSetupState, DirectoryBrowser,
        KeyFileBrowser, NewSshDestinationState, Screen, SshField,
    },
};

use super::*;

const SENTINEL: &str = "private-parser-sentinel";

fn fixture() -> (tempfile::TempDir, App, DestinationSetupState) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    let home = directory.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let mut app = App::new(
        directory.path().join("projects.yaml"),
        directory.path().join("destinations.yaml"),
        &root,
    )
    .unwrap();
    // These diagnostics never need SSH Agent discovery, SSH, or an async runtime.
    app.runtime = None;
    app.home_directory = Some(home);
    let name = ComponentName::parse("worker").unwrap();
    let destination = DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
    let setup = DestinationSetupState {
        components: ComponentSetupState {
            root: root.clone(),
            report: DiscoveryReport {
                components: vec![ComponentCandidate {
                    name: name.clone(),
                    source: root,
                    confidence: DiscoveryConfidence::Low,
                    setup: ComponentSetup {
                        working_directory: Some(PathBuf::from(".")),
                        build: vec![BuildCommand::argv("cargo", ["build"])],
                        artifact: ArtifactSpec {
                            path: PathBuf::from("target/worker"),
                        },
                    },
                }],
                notices: Vec::new(),
            },
            selected: [name.clone()].into(),
            cursor: 0,
        },
        destinations: vec![DestinationSummary {
            key: destination.clone(),
            revision: DestinationRevision::INITIAL,
            driver: DriverKind::linux_ssh(),
            endpoint: "deploy@test.example.invalid:22".into(),
        }],
        assignments: BTreeMap::from([(name, destination)]),
        target_settings: BTreeMap::new(),
        component_cursor: 0,
        destination_cursor: 0,
    };
    app.screen = Screen::SetupDestinations(setup.clone());
    (directory, app, setup)
}

fn draft(setup: DestinationSetupState) -> NewSshDestinationState {
    NewSshDestinationState {
        identity_request: None,
        destinations: setup,
        connections: Vec::new(),
        connection_cursor: 0,
        host: "test.example.invalid".into(),
        user: "deploy".into(),
        port: "22".into(),
        field: SshField::Credential,
        credentials: vec![CredentialChoice::Agent {
            fingerprint: "SHA256:test-agent".into(),
            label: "selected identity".into(),
        }],
        credential_cursor: 0,
        agent_status: "SSH Agent: unavailable".into(),
    }
}

fn assert_static_message(app: &App, expected: &str) {
    let message = app.message.as_deref().unwrap();
    assert!(message.contains(expected), "{message}");
    assert!(!message.contains(SENTINEL), "{message}");
}

fn assert_no_setup_writes(app: &App, setup: &DestinationSetupState) {
    assert!(!app.destination_registry_path.exists());
    assert!(!setup.components.root.join("shipforge.yaml").exists());
    assert!(app.setup_task.is_none());
}

#[test]
fn unreadable_local_ssh_candidates_remain_unknown_and_can_retry_without_writes() {
    let (_directory, mut app, setup) = fixture();
    let config = app.home_directory.as_ref().unwrap().join(".ssh/config");
    std::fs::write(&config, [0xff, 0xfe]).unwrap();
    assert!(crate::config::discover_local_ssh(app.home_directory.as_deref().unwrap()).is_err());
    app.open_new_ssh_destination(&setup);
    assert!(matches!(app.screen, Screen::SetupDestinations(_)));
    assert_static_message(&app, "availability is unknown");
    assert_eq!(std::fs::read(&config).unwrap(), [0xff, 0xfe]);
    assert_no_setup_writes(&app, &setup);

    std::fs::write(&config, "Host test\n  HostName test.example.invalid\n").unwrap();
    app.open_new_ssh_destination(&setup);
    let Screen::NewSshDestination(current) = &app.screen else {
        panic!("repaired SSH discovery must reopen the form");
    };
    assert_eq!(current.host, "test.example.invalid");
    assert_no_setup_writes(&app, &setup);
}

#[test]
fn malformed_identity_registry_does_not_become_empty_success_and_retry_is_read_only() {
    let (_directory, mut app, setup) = fixture();
    let invalid = format!("schemaVersion: [{SENTINEL}\n");
    std::fs::write(&app.credential_registry_path, &invalid).unwrap();
    assert!(CredentialRegistry::load(&app.credential_registry_path).is_err());
    app.open_new_ssh_destination(&setup);
    assert!(matches!(app.screen, Screen::SetupDestinations(_)));
    assert_static_message(&app, "availability is unknown");
    assert_eq!(
        std::fs::read_to_string(&app.credential_registry_path).unwrap(),
        invalid
    );
    assert_no_setup_writes(&app, &setup);

    CredentialRegistry::new()
        .save(&app.credential_registry_path)
        .unwrap();
    let repaired = std::fs::read(&app.credential_registry_path).unwrap();
    app.open_new_ssh_destination(&setup);
    assert!(matches!(app.screen, Screen::NewSshDestination(_)));
    assert_eq!(
        std::fs::read(&app.credential_registry_path).unwrap(),
        repaired
    );
    assert_no_setup_writes(&app, &setup);
}

#[test]
fn unsafe_setup_preview_retains_draft_and_does_not_echo_paths_or_write_yaml() {
    let (_directory, mut app, mut setup) = fixture();
    setup.components.report.components[0].setup.artifact.path =
        PathBuf::from(format!("../{SENTINEL}"));
    app.screen = Screen::SetupDestinations(setup.clone());
    app.handle_setup_destinations(KeyCode::Char('n'), &setup);
    let Screen::SetupDestinations(current) = &app.screen else {
        panic!("invalid setup must retain its draft");
    };
    assert_eq!(current.components.report, setup.components.report);
    assert_static_message(&app, "Component paths must stay inside the project");
    assert_no_setup_writes(&app, &setup);

    setup.components.report.components[0].setup.artifact.path = PathBuf::from("dist/worker");
    app.screen = Screen::SetupDestinations(setup.clone());
    app.handle_setup_destinations(KeyCode::Char('n'), &setup);
    assert!(matches!(app.screen, Screen::SetupReview { .. }));
    assert_no_setup_writes(&app, &setup);
}

#[test]
fn public_preview_projection_omits_raw_names_parser_and_io_diagnostics() {
    let component = ComponentName::parse("worker").unwrap();
    let errors = [
        ConfigError::InvalidName {
            field: "Project",
            value: SENTINEL.into(),
        },
        ConfigError::Yaml {
            path: PathBuf::from(SENTINEL),
            source: serde_yaml_ng::from_str::<u64>(SENTINEL).unwrap_err(),
        },
        ConfigError::Read {
            path: PathBuf::from(SENTINEL),
            source: std::io::Error::other(SENTINEL),
        },
        ConfigError::UnsafeRoot {
            environment: SENTINEL.into(),
            component,
            root: SENTINEL.into(),
        },
        ConfigError::Security(crate::telemetry::SecurityError::SensitiveConfig(
            SENTINEL.into(),
        )),
        ConfigError::Security(crate::telemetry::SecurityError::InvalidYaml(
            serde_yaml_ng::from_str::<u64>(SENTINEL).unwrap_err(),
        )),
    ];
    for error in errors {
        let message = configuration_error(&error);
        assert!(!message.contains(SENTINEL));
        assert!(message.contains("No project configuration was written"));
    }
}

#[test]
fn directory_open_failure_retains_previous_page_and_retry_does_not_select_project() {
    let (directory, mut app, setup) = fixture();
    let browser = DirectoryBrowser::open(&setup.components.root).unwrap();
    app.screen = Screen::Browser(browser.clone());
    let missing = directory.path().join(SENTINEL);
    app.open_browser(&missing);
    let Screen::Browser(current) = &app.screen else {
        panic!("browser must remain");
    };
    assert_eq!(current.directory, browser.directory);
    assert_static_message(&app, "previous page was retained");

    std::fs::create_dir(&missing).unwrap();
    app.open_browser(&missing);
    let Screen::Browser(current) = &app.screen else {
        panic!("browser must reopen");
    };
    assert_eq!(current.directory, std::fs::canonicalize(&missing).unwrap());
    assert_no_setup_writes(&app, &setup);
    assert!(!app.registry_path.exists());
}

#[test]
fn key_directory_failures_preserve_identity_and_previous_list_until_retry() {
    let (directory, mut app, setup) = fixture();
    let draft = draft(setup.clone());
    let missing = directory.path().join(SENTINEL);
    app.home_directory = Some(missing.clone());
    app.screen = Screen::NewSshDestination(draft.clone());
    app.open_key_browser(&draft);
    assert!(matches!(app.screen, Screen::NewSshDestination(_)));
    assert_static_message(&app, "F3 to retry");

    let browser = KeyFileBrowser::open(directory.path()).unwrap();
    app.screen = Screen::KeyBrowser {
        draft: draft.clone(),
        browser: browser.clone(),
    };
    app.open_key_browser_directory(&draft, &missing);
    let Screen::KeyBrowser {
        browser: current,
        draft: current_draft,
    } = &app.screen
    else {
        panic!("previous key browser must remain");
    };
    assert_eq!(current.entries, browser.entries);
    assert_eq!(current_draft.credentials[0].label(), "selected identity");
    assert_static_message(&app, "previous list and identity were retained");

    std::fs::create_dir(&missing).unwrap();
    app.open_key_browser_directory(&draft, &missing);
    let Screen::KeyBrowser { browser, .. } = &app.screen else {
        panic!("retry must reopen");
    };
    assert_eq!(browser.directory, std::fs::canonicalize(&missing).unwrap());
    assert_no_setup_writes(&app, &setup);
}

#[test]
fn missing_selected_key_is_rejected_without_changing_identity_or_page() {
    let (_directory, mut app, setup) = fixture();
    let draft = draft(setup.clone());
    let key_path = setup.components.root.join(SENTINEL);
    std::fs::write(&key_path, "not a real key").unwrap();
    let browser = KeyFileBrowser::open(&setup.components.root).unwrap();
    std::fs::remove_file(&key_path).unwrap();
    app.screen = Screen::KeyBrowser {
        draft: draft.clone(),
        browser: browser.clone(),
    };
    app.handle_key_browser(KeyCode::Char('s'), &draft, &browser);
    let Screen::KeyBrowser { draft, .. } = &app.screen else {
        panic!("key selection must remain");
    };
    assert_eq!(draft.credentials[0].label(), "selected identity");
    assert!(app.message.is_some());
    assert!(!app.message.as_deref().unwrap().contains(SENTINEL));
    assert_no_setup_writes(&app, &setup);
}

#[test]
fn recent_registry_failure_retains_disabled_cache_and_repair_refreshes_it() {
    let (_directory, mut app, setup) = fixture();
    std::fs::write(
        setup.components.root.join("shipforge.yaml"),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/examples/shipforge.yaml"
        )),
    )
    .unwrap();
    crate::projects::register_initialized_project(&app.registry_path, &setup.components.root, 42)
        .unwrap();
    app.refresh_recent();
    assert!(!app.recent_unavailable);
    let previous = app.recent.clone();
    assert_eq!(previous.len(), 1);
    assert!(previous[0].available);
    let registry = std::fs::read(&app.registry_path).unwrap();
    std::fs::write(&app.registry_path, format!("schemaVersion: [{SENTINEL}\n")).unwrap();
    assert!(ProjectRegistry::load(&app.registry_path).is_err());

    app.refresh_recent();
    assert!(app.recent_unavailable);
    assert_eq!(app.recent.len(), previous.len());
    assert_eq!(app.recent[0].project, previous[0].project);
    assert!(!app.recent[0].available);
    assert_eq!(app.message.as_deref(), Some(RECENT_REFRESH_FAILED));
    assert_static_message(&app, "Cached entries are retained but disabled");
    app.screen = Screen::Projects;
    app.handle_projects(KeyCode::Enter);
    assert!(matches!(app.screen, Screen::Projects));
    assert_static_message(&app, "reselect its directory to check current state");
    assert!(
        !app.message
            .as_deref()
            .unwrap()
            .contains("shipforge.yaml is unavailable")
    );

    std::fs::write(&app.registry_path, registry).unwrap();
    app.message = Some(RECENT_REFRESH_FAILED.into());
    app.handle_projects(KeyCode::Char('f'));
    assert!(!app.recent_unavailable);
    assert_eq!(app.recent, previous);
    assert!(app.message.is_none());
}

fn rendered(draft: &NewSshDestinationState) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| crate::tui::render_new_ssh_destination(frame, frame.area(), draft))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

#[test]
fn long_identity_list_keeps_keyboard_selection_and_form_fields_visible() {
    let (_directory, _app, setup) = fixture();
    let mut draft = draft(setup);
    draft.credentials = (0..100)
        .map(|index| CredentialChoice::Agent {
            fingerprint: format!("SHA256:test-{index}"),
            label: format!("identity-{index}"),
        })
        .collect();
    draft.credential_cursor = 99;
    assert!(rendered(&draft).contains("> identity-99"));
    for (field, expected) in [
        (SshField::Host, "> Host: test.example.invalid"),
        (SshField::User, "> User: deploy"),
        (SshField::Port, "> Port: 22"),
    ] {
        draft.field = field;
        assert!(rendered(&draft).contains(expected));
    }
}

#[test]
fn empty_identity_view_explains_available_action_and_sanitizes_agent_status() {
    let (_directory, _app, setup) = fixture();
    let mut draft = draft(setup);
    draft.credentials.clear();
    draft.agent_status = "SSH Agent: unavailable\u{1b}[31m\n".into();
    let text = rendered(&draft);
    assert!(text.contains("No identities are available in this form"));
    assert!(text.contains("F3 to choose a key file"));
    assert!(!text.contains('\u{1b}'));
}
