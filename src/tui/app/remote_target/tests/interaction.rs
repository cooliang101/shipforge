use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_real_worker_remains_tracked_until_cleanup_and_rejects_late_success() {
    let mut fixture = Fixture::new();
    fixture.open();
    fixture.gateway.behavior.store(1, Ordering::SeqCst);
    fixture.app.handle_key(key(KeyCode::Char('b')));
    wait_for(|| fixture.gateway.calls.load(Ordering::SeqCst) == 1).await;
    fixture.app.finish_remote_target(
        uuid::Uuid::now_v7(),
        Ok(RemoteTargetResult::Browsed(RemoteDirectoryCandidates {
            directory: "/wrong".into(),
            directories: Vec::new(),
        })),
    );
    assert!(fixture.app.remote_target_task.is_some());
    fixture.app.handle_key(key(KeyCode::Esc));
    wait_for(|| fixture.gateway.cancelled.load(Ordering::SeqCst)).await;
    for input in [
        KeyCode::Enter,
        KeyCode::Char('q'),
        KeyCode::F(4),
        KeyCode::Esc,
    ] {
        assert!(!fixture.app.handle_key(key(input)));
    }
    fixture.app.poll_background();
    assert!(fixture.app.remote_target_task.is_some());
    assert!(fixture.app.picker.is_none());
    assert!(matches!(
        fixture.selection().page,
        Page::Loading { cancelling: true }
    ));
    assert!(!fixture.gateway.finished.load(Ordering::SeqCst));
    fixture.gateway.release.notify_one();
    finish(&mut fixture.app).await;
    assert!(fixture.gateway.finished.load(Ordering::SeqCst));
    assert!(
        matches!(&fixture.selection().page, Page::Unavailable { retry_path } if retry_path == "/")
    );
    assert!(fixture.selection().search_choices().is_none());
    assert_eq!(fixture.selection().root, "/srv/original-api");
    assert_eq!(
        fixture.selection().service().as_deref(),
        Some("original-api.service")
    );
    fixture.assert_read_only();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_replaces_cached_nonempty_and_empty_lists_with_recoverable_unknown() {
    for path in ["/srv", "/empty"] {
        let mut fixture = Fixture::new();
        fixture.open();
        let screen = fixture.selection().clone();
        fixture
            .app
            .start_remote_target(screen, worker::Request::Browse(path.into()));
        finish(&mut fixture.app).await;
        assert!(matches!(fixture.selection().page, Page::Directories { .. }));
        fixture.gateway.behavior.store(2, Ordering::SeqCst);
        fixture.app.handle_key(key(KeyCode::Char('f')));
        finish(&mut fixture.app).await;
        assert!(!fixture.app.message.as_deref().unwrap().contains("sentinel"));
        // Even after a new key clears the transient error, unknown is persistent.
        fixture.app.handle_key(key(KeyCode::Down));
        let text = rendered(fixture.selection(), 100, 12);
        assert!(text.contains("contents are unknown"));
        assert!(!text.contains("No child directories"));
        fixture.app.handle_key(key(KeyCode::F(4)));
        assert!(fixture.app.picker.is_none());
        assert!(
            matches!(&fixture.selection().page, Page::Unavailable { retry_path } if retry_path == path)
        );
        fixture.gateway.behavior.store(0, Ordering::SeqCst);
        fixture.app.handle_key(key(KeyCode::Char('f')));
        finish(&mut fixture.app).await;
        assert!(
            matches!(&fixture.selection().page, Page::Directories { candidates, .. } if candidates.directory == path)
        );
        assert_eq!(
            fixture.gateway.paths.lock().unwrap().as_slice(),
            [path, path, path]
        );
        fixture.gateway.behavior.store(2, Ordering::SeqCst);
        fixture.app.handle_key(key(KeyCode::Char('f')));
        finish(&mut fixture.app).await;
        fixture.app.handle_key(key(KeyCode::Char('g')));
        type_text(&mut fixture.app, "/srv");
        fixture.gateway.behavior.store(0, Ordering::SeqCst);
        fixture.app.handle_key(key(KeyCode::Enter));
        finish(&mut fixture.app).await;
        assert!(
            matches!(&fixture.selection().page, Page::Directories { candidates, .. } if candidates.directory == "/srv")
        );
        fixture.app.handle_key(key(KeyCode::Esc));
        assert!(matches!(fixture.selection().page, Page::Services));
        fixture.assert_read_only();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspection_failure_forgets_root_evidence_and_retry_uses_the_current_local_root() {
    let mut fixture = Fixture::new();
    fixture.open();
    fixture.app.handle_key(key(KeyCode::Char('v')));
    finish(&mut fixture.app).await;
    assert_eq!(
        fixture.selection().root_state,
        Some(SetupRootState::WritableDirectory)
    );
    fixture.gateway.behavior.store(3, Ordering::SeqCst);
    fixture.app.handle_key(key(KeyCode::Char('v')));
    finish(&mut fixture.app).await;
    assert_eq!(fixture.selection().root_state, None);
    assert!(matches!(fixture.selection().page, Page::Services));
    assert!(!fixture.app.message.as_deref().unwrap().contains("sentinel"));
    fixture.app.handle_key(key(KeyCode::Char('r')));
    type_text(&mut fixture.app, "/srv/new root");
    fixture.app.handle_key(key(KeyCode::Enter));
    fixture.gateway.behavior.store(0, Ordering::SeqCst);
    fixture.app.handle_key(key(KeyCode::Char('v')));
    finish(&mut fixture.app).await;
    assert_eq!(
        fixture
            .gateway
            .paths
            .lock()
            .unwrap()
            .last()
            .map(String::as_str),
        Some("/srv/new root")
    );
    assert_eq!(
        fixture.selection().root_state,
        Some(SetupRootState::WritableDirectory)
    );
    fixture.assert_read_only();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f4_focus_preserves_real_indices_without_opening_directories_or_applying_service() {
    let mut fixture = Fixture::new();
    fixture.open();
    fixture.app.handle_key(key(KeyCode::Char('b')));
    finish(&mut fixture.app).await;
    fixture.app.handle_key(key(KeyCode::F(4)));
    for character in "var".chars() {
        fixture.app.handle_key(key(KeyCode::Char(character)));
    }
    fixture.app.handle_key(key(KeyCode::Enter));
    assert!(fixture.app.picker.is_none());
    assert!(
        matches!(&fixture.selection().page, Page::Directories { candidates, cursor: 1 } if candidates.directory == "/")
    );
    assert_eq!(fixture.gateway.calls.load(Ordering::SeqCst), 1);
    fixture.app.handle_key(key(KeyCode::Enter));
    finish(&mut fixture.app).await;
    assert!(
        matches!(&fixture.selection().page, Page::Directories { candidates, .. } if candidates.directory == "/var")
    );
    fixture
        .app
        .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(matches!(fixture.selection().page, Page::Directories { .. }));
    fixture.app.handle_key(key(KeyCode::Char('s')));
    assert_eq!(fixture.selection().root, "/var");
    assert_eq!(fixture.selection().root_state, None);
    assert_eq!(fixture.gateway.calls.load(Ordering::SeqCst), 2);
    fixture.app.handle_key(key(KeyCode::Char('v')));
    finish(&mut fixture.app).await;
    fixture.app.handle_key(key(KeyCode::F(4)));
    for character in "worker.service".chars() {
        fixture.app.handle_key(key(KeyCode::Char(character)));
    }
    fixture.app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        fixture.selection().service().as_deref(),
        Some("worker.service")
    );
    assert_eq!(fixture.gateway.calls.load(Ordering::SeqCst), 3);
    assert!(matches!(fixture.selection().page, Page::Services));
    let Origin::Initial(setup) = &fixture.selection().origin else {
        panic!("expected setup");
    };
    assert_eq!(
        setup.target_settings[&name("backend")].root.as_deref(),
        Some("/srv/original-api")
    );
    fixture.app.handle_key(key(KeyCode::Char('r')));
    type_text(&mut fixture.app, "/srv/literal/path");
    fixture.app.handle_key(key(KeyCode::F(4)));
    assert!(fixture.app.picker.is_none());
    assert!(
        matches!(&fixture.selection().page, Page::Text { value, .. } if value == "/srv/literal/path")
    );
    fixture.app.handle_key(key(KeyCode::Esc));
    fixture.app.handle_key(key(KeyCode::Esc));
    let Screen::SetupDestinations(setup) = &fixture.app.screen else {
        panic!("expected setup");
    };
    assert_eq!(
        setup.target_settings[&name("backend")].systemd.as_deref(),
        Some("original-api.service")
    );
    fixture.assert_read_only();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_components_keep_independent_roots_and_services_until_plain_yaml_confirmation() {
    let mut fixture = Fixture::new();
    for (index, root, service) in [
        (0, "/srv/api", "api.service"),
        (1, "/srv/worker", "worker.service"),
    ] {
        if index == 1 {
            fixture.app.handle_key(key(KeyCode::Right));
        }
        fixture.open();
        fixture.app.handle_key(key(KeyCode::Char('r')));
        type_text(&mut fixture.app, root);
        fixture.app.handle_key(key(KeyCode::Enter));
        fixture.app.handle_key(key(KeyCode::Char('m')));
        type_text(&mut fixture.app, service);
        fixture.app.handle_key(key(KeyCode::Enter));
        fixture
            .app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(matches!(
            fixture.app.screen,
            Screen::RemoteSetupSelection(_)
        ));
        fixture.app.handle_key(key(KeyCode::Enter));
        fixture.assert_read_only();
    }
    let Screen::SetupDestinations(setup) = &fixture.app.screen else {
        panic!("expected setup");
    };
    assert_eq!(
        setup.target_settings[&name("backend")].root.as_deref(),
        Some("/srv/api")
    );
    assert_eq!(
        setup.target_settings[&name("backend")].systemd.as_deref(),
        Some("api.service")
    );
    assert_eq!(
        setup.target_settings[&name("worker")].root.as_deref(),
        Some("/srv/worker")
    );
    assert_eq!(
        setup.target_settings[&name("worker")].systemd.as_deref(),
        Some("worker.service")
    );
    fixture.app.handle_key(key(KeyCode::Char('n')));
    assert!(
        matches!(fixture.app.screen, Screen::SetupReview { .. }),
        "{:?}",
        fixture.app.message
    );
    fixture.assert_read_only();
    fixture
        .app
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SHIFT));
    fixture.app.handle_key(key(KeyCode::Enter));
    fixture.assert_read_only();
    fixture.app.handle_key(key(KeyCode::Char('c')));
    let Screen::Overview { config, .. } = &fixture.app.screen else {
        panic!("expected saved overview: {:?}", fixture.app.message);
    };
    let targets = &config.environments["production"].components;
    assert_eq!(targets[&name("backend")].root, "/srv/api");
    assert_eq!(targets[&name("worker")].root, "/srv/worker");
    assert!(fixture.project.join("shipforge.yaml").exists());
    assert_eq!(fixture.gateway.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read(&fixture.app.destination_registry_path).unwrap(),
        fixture.destination_bytes
    );
    assert_eq!(
        fs::read(&fixture.app.credential_registry_path).unwrap(),
        fixture.credential_bytes
    );
}

#[test]
fn long_lists_keep_actual_selected_directory_root_and_observation_visible() {
    let mut fixture = Fixture::new();
    fixture.open();
    let mut screen = fixture.selection().clone();
    screen.systemd_units = (0..100)
        .map(|index| format!("unit-{index:03}.service"))
        .collect();
    screen.cursor = 100;
    screen.root_state = Some(SetupRootState::ReadOnlyDirectory);
    let text = rendered(&screen, 100, 10);
    for expected in [
        "Root: /srv/original-api",
        "read-only when inspected",
        "unit-099.service",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(screen.context_label().contains("[PRODUCTION]"));
    screen.page = Page::Directories {
        candidates: RemoteDirectoryCandidates {
            directory: "/srv/chosen".into(),
            directories: (0..100)
                .map(|index| format!("/srv/chosen/child-{index:03}"))
                .collect(),
        },
        cursor: 99,
    };
    let text = rendered(&screen, 100, 10);
    for expected in [
        "Directory: /srv/chosen",
        "not the highlighted child",
        "child-099",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    for directory in ["/", "/empty"] {
        screen.page = Page::Directories {
            candidates: RemoteDirectoryCandidates {
                directory: directory.into(),
                directories: Vec::new(),
            },
            cursor: 0,
        };
        assert!(!screen.help().contains("Enter"));
        if directory == "/" {
            assert!(!screen.help().contains("s select"));
        }
        assert!(rendered(&screen, 100, 10).contains("No child directories"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_saved_summary_is_rejected_before_any_gateway_call() {
    for change_endpoint in [false, true] {
        let mut fixture = Fixture::new();
        fixture.open();
        if change_endpoint {
            let Screen::RemoteSetupSelection(screen) = &mut fixture.app.screen else {
                unreachable!();
            };
            screen.destination.endpoint = "other.invalid:22".into();
        } else {
            let mut registry =
                DestinationRegistry::load(&fixture.app.destination_registry_path).unwrap();
            let key = fixture.selection().destination.key.clone();
            registry
                .revise(&key, registry.resolve(&key).unwrap().settings.clone())
                .unwrap();
            registry
                .save(&fixture.app.destination_registry_path)
                .unwrap();
        }
        fixture.app.handle_key(key(KeyCode::Char('b')));
        finish(&mut fixture.app).await;
        assert_eq!(fixture.gateway.calls.load(Ordering::SeqCst), 0);
        assert!(matches!(fixture.selection().page, Page::Unavailable { .. }));
        assert!(
            fixture
                .app
                .message
                .as_deref()
                .unwrap()
                .contains("changed or disappeared")
        );
        assert!(!fixture.project.join("shipforge.yaml").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_tracked_target_worker_cleanup() {
    let mut fixture = Fixture::new();
    fixture.open();
    fixture.gateway.behavior.store(1, Ordering::SeqCst);
    fixture.app.handle_key(key(KeyCode::Char('b')));
    wait_for(|| fixture.gateway.calls.load(Ordering::SeqCst) == 1).await;
    let gateway = fixture.gateway.clone();
    let release = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !gateway.cancelled.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!gateway.finished.load(Ordering::SeqCst));
        gateway.release.notify_one();
    });
    fixture.app.shutdown();
    release.join().unwrap();
    assert!(fixture.gateway.finished.load(Ordering::SeqCst));
    assert!(fixture.app.remote_target_task.is_none());
    fixture.assert_read_only();
}

#[test]
fn newly_saved_connection_drops_only_replaced_components_old_target_choices() {
    let mut fixture = Fixture::new();
    let Screen::SetupDestinations(setup) = fixture.app.screen.clone() else {
        unreachable!();
    };
    let draft = NewSshDestinationState {
        identity_request: None,
        destinations: setup,
        connections: Vec::new(),
        connection_cursor: 0,
        host: "new.invalid".into(),
        user: "deploy".into(),
        port: "22".into(),
        field: SshField::Credential,
        credentials: vec![CredentialChoice::Agent {
            fingerprint: "SHA256:new-agent".into(),
            label: "new fixture".into(),
        }],
        credential_cursor: 0,
        agent_status: String::new(),
    };
    let setup = fixture
        .app
        .commit_ssh_destination(
            &draft,
            &HostKeyFingerprint::parse("SHA256:new-host").unwrap(),
        )
        .unwrap();
    assert!(!setup.target_settings.contains_key(&name("backend")));
    assert_eq!(
        setup.target_settings[&name("worker")].root.as_deref(),
        Some("/srv/original-worker")
    );
    assert_eq!(
        setup.target_settings[&name("worker")].systemd.as_deref(),
        Some("original-worker.service")
    );
    fixture.app.open_initial_remote_target(
        setup,
        Some(RemoteSetupCandidates {
            root: SetupRootState::Missing,
            services: Vec::new(),
            notices: Vec::new(),
        }),
    );
    assert_eq!(
        fixture.selection().root,
        default_remote_root("demo", "production", &name("backend"))
    );
    assert_eq!(
        fixture.selection().root_state,
        Some(SetupRootState::Missing)
    );
    assert!(fixture.selection().service().is_none());
    assert!(!fixture.project.join("shipforge.yaml").exists());
}

#[test]
fn full_app_minimum_size_keeps_focused_service_and_directory_with_fixed_context() {
    let mut fixture = Fixture::new();
    fixture.open();
    let mut screen = fixture.selection().clone();
    screen.systemd_units = (0..100)
        .map(|index| format!("unit-{index:03}.service"))
        .collect();
    screen.cursor = 100;
    screen.root_state = Some(SetupRootState::ReadOnlyDirectory);
    screen.notices = vec!["A discovery notice must not hide the selected unit.".into()];
    fixture.app.screen = Screen::RemoteSetupSelection(screen.clone());
    let text = rendered_app(&fixture.app, 80, 10);
    for expected in [
        "[PRODUCTION]",
        "Root: /srv/original-api",
        "Observation: read-only",
        "> unit-099.service",
        "Esc back",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    screen.page = Page::Directories {
        candidates: RemoteDirectoryCandidates {
            directory: "/srv/visible-parent".into(),
            directories: (0..100)
                .map(|index| format!("/srv/visible-parent/child-{index:03}"))
                .collect(),
        },
        cursor: 99,
    };
    fixture.app.screen = Screen::RemoteSetupSelection(screen);
    let text = rendered_app(&fixture.app, 80, 10);
    for expected in [
        "Directory: /srv/visible-parent",
        "not the highlighted child",
        "> /srv/visible-parent/child-099",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    for (width, height) in [(79, 10), (80, 9)] {
        assert!(rendered_app(&fixture.app, width, height).contains("Resize to at least 80x10"));
    }
    fixture.assert_read_only();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_authentication_and_saved_inspection_share_bounded_candidate_validation() {
    let mut fixture = Fixture::new();
    let Screen::SetupDestinations(setup) = fixture.app.screen.clone() else {
        unreachable!();
    };
    let mut invalid = Vec::new();
    for services in [
        vec!["bad\n.service".into()],
        vec!["a.service".into(); 4096],
        vec![String::new()],
    ] {
        invalid.push(RemoteSetupCandidates {
            root: SetupRootState::WritableDirectory,
            services,
            notices: Vec::new(),
        });
    }
    for notices in [
        vec!["note".into(); 257],
        vec!["n".repeat(4097)],
        vec!["private\u{202e}notice".into()],
    ] {
        invalid.push(RemoteSetupCandidates {
            root: SetupRootState::WritableDirectory,
            services: Vec::new(),
            notices,
        });
    }
    for candidates in invalid {
        fixture
            .app
            .open_initial_remote_target(setup.clone(), Some(candidates));
        assert_eq!(fixture.selection().root_state, None);
        assert_eq!(fixture.selection().systemd_units, ["original-api.service"]);
        assert!(fixture.selection().notices.is_empty());
        assert!(
            fixture
                .app
                .message
                .as_deref()
                .unwrap()
                .contains("target observations are unavailable")
        );
        assert!(!rendered_app(&fixture.app, 80, 10).contains("private"));
    }
    fixture.gateway.behavior.store(4, Ordering::SeqCst);
    fixture.app.handle_key(key(KeyCode::Char('v')));
    finish(&mut fixture.app).await;
    assert_eq!(fixture.selection().root_state, None);
    assert_eq!(fixture.selection().systemd_units, ["original-api.service"]);
    assert!(fixture.selection().notices.is_empty());
    assert!(!fixture.app.message.as_deref().unwrap().contains("private"));
    fixture.gateway.behavior.store(0, Ordering::SeqCst);
    fixture.app.handle_key(key(KeyCode::Char('v')));
    finish(&mut fixture.app).await;
    assert_eq!(
        fixture.selection().root_state,
        Some(SetupRootState::WritableDirectory)
    );
    fixture.assert_read_only();
}
