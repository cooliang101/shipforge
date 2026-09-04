use super::*;
use crate::telemetry::log_record::{LogPersistence, LogStepState};

fn small_frame(app: &App) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
    terminal
        .draw(|frame| crate::tui::render(frame, app))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(80)
        .map(|line| {
            line.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn press(app: &mut App, key: KeyCode) {
    app.handle_key(KeyEvent::new(key, KeyModifiers::NONE));
}

fn scroll_to(app: &mut App, expected: &str) {
    for _ in 0..300 {
        if small_frame(app).contains(expected) {
            return;
        }
        press(app, KeyCode::Down);
    }
    panic!("80x10 scrolling could not reveal {expected}");
}

#[test]
fn actual_small_app_viewport_can_read_and_scroll_the_complete_selected_record() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    workspace.view.push(LogRow {
        sequence: 1,
        elapsed_ms: Some(10),
        event: Arc::new(LogEvent {
            namespace: "build.stderr".into(),
            scope: None,
            kind: LogEventKind::Output,
            message: format!(
                "DETAIL_BODY_START\n{}DETAIL_BODY_TAIL",
                "safe diagnostic line\n".repeat(40)
            ),
        }),
    });
    app.log_workspace = Some(workspace);
    press(&mut app, KeyCode::Enter);
    assert!(small_frame(&app).contains("Source: Build diagnostic output"));
    scroll_to(&mut app, "DETAIL_BODY_START");
    scroll_to(&mut app, "DETAIL_BODY_TAIL");
    press(&mut app, KeyCode::Esc);
    assert!(app.log_workspace.is_some());
}

#[test]
fn actual_small_app_viewport_keeps_all_progress_steps_reachable() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let progress = crate::tui::live_progress::LiveProgress::default();
    for index in 0..30 {
        let step = if index == 29 {
            "cleanup.zz_TAIL_STEP".into()
        } else {
            format!("cleanup.v{index:02}")
        };
        progress.record(LogEvent {
            namespace: "step.state".into(),
            message: "Recorded result".into(),
            scope: Some(LogScope {
                component: ComponentName::parse("api").unwrap(),
                step,
            }),
            kind: LogEventKind::Step {
                state: LogStepState::Succeeded,
                persistence: LogPersistence::Recorded,
            },
        });
    }
    app.live_progress = Some(progress);
    app.open_live_logs();
    press(&mut app, KeyCode::Char('p'));
    assert!(small_frame(&app).contains("Following"));
    scroll_to(&mut app, "Cleanup zz_TAIL_STEP");
}

#[test]
fn actual_small_export_viewport_reads_payload_and_full_path_without_confirming_from_help() {
    let directory = temporary_export_directory();
    let target = directory
        .path()
        .join("first-directory-".repeat(3))
        .join("second-directory-".repeat(3));
    std::fs::create_dir_all(&target).unwrap();
    let text = format!(
        "PAYLOAD_BEGIN\n{}FIRST_CHUNK_TAIL\n{}\nSECOND_CHUNK_TAIL\n",
        format!("{}\n", "f".repeat(60)).repeat(45),
        "x".repeat(1800)
    );
    assert!(text.find("FIRST_CHUNK_TAIL").unwrap() < PREVIEW_CHUNK_BYTES);
    assert!(text.find("SECOND_CHUNK_TAIL").unwrap() > PREVIEW_CHUNK_BYTES);
    let preview = crate::application::local_export::LocalExportService::prepare(
        &target,
        "viewport-export-path-END_MARKER.txt",
        text,
    )
    .unwrap();
    let path = preview.path().to_path_buf();
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    workspace.mode = Mode::ExportPreview {
        preview,
        offset: 0,
        scroll: 0,
    };
    workspace.status = "CURRENT_EXPORT_STATUS_MARKER".into();
    assert!(workspace.help_text().contains(&format!("{path:?}")));
    let mut app = log_test_app(directory.path());
    app.log_workspace = Some(workspace);
    assert!(small_frame(&app).contains("PAYLOAD_BEGIN"));
    scroll_to(&mut app, "FIRST_CHUNK_TAIL");
    press(&mut app, KeyCode::F(1));
    scroll_to(&mut app, "CURRENT_EXPORT_STATUS_MARKER");
    scroll_to(&mut app, "END_MARKER.txt");
    scroll_to(&mut app, "Only unmodified c saves");
    press(&mut app, KeyCode::Char('c'));
    assert!(!app.log_workspace_busy());
    assert!(!path.exists(), "help must not forward a confirmation key");
    press(&mut app, KeyCode::Esc);
    assert!(matches!(
        app.log_workspace.as_ref().unwrap().mode,
        Mode::ExportPreview { offset: 0, .. }
    ));
    assert!(small_frame(&app).contains("FIRST_CHUNK_TAIL"));
    press(&mut app, KeyCode::Char('n'));
    scroll_to(&mut app, "SECOND_CHUNK_TAIL");
    assert!(!path.exists());
    press(&mut app, KeyCode::Esc);
    assert!(app.log_workspace.is_some());
}

#[test]
fn actual_small_export_directory_viewport_keeps_candidates_visible() {
    let directory = temporary_export_directory();
    std::fs::create_dir(directory.path().join("visible-child")).unwrap();
    let mut app = log_test_app(directory.path());
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    workspace.mode = Mode::ExportDirectory {
        browser: DirectoryBrowser::open(directory.path()).unwrap(),
        payload: "export fixture".into(),
        name: "fixture.txt".into(),
    };
    app.log_workspace = Some(workspace);
    assert!(small_frame(&app).contains("> visible-child"));
    press(&mut app, KeyCode::F(1));
    scroll_to(&mut app, "Automatic filename: fixture.txt");
    press(&mut app, KeyCode::Esc);
    assert!(small_frame(&app).contains("> visible-child"));
}
