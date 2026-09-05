use super::super::Screen;
use super::*;

#[path = "tests/viewport.rs"]
mod viewport;

#[test]
fn export_preview_chunks_keep_unicode_boundaries_and_a_bounded_render_payload() {
    let text = "界".repeat(10_000);
    let mut offset = 0;
    for _ in 0..10 {
        let chunk = preview_chunk(&text, offset);
        assert!(chunk.len() <= PREVIEW_CHUNK_BYTES);
        assert!(text.is_char_boundary(offset));
        let next = preview_offset(&text, offset, KeyCode::Char('n'));
        assert!(next >= offset);
        offset = next;
    }
    assert_eq!(preview_offset(&text, offset, KeyCode::Home), 0);
}

#[test]
fn clipboard_consumes_only_the_selected_frozen_failed_command() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = App::new(
        temp.path().join("projects.yaml"),
        temp.path().join("destinations.yaml"),
        temp.path(),
    )
    .unwrap();
    let mut workspace = LogWorkspace::new(None, temp.path().to_path_buf());
    let command = crate::telemetry::log_record::RecordedCommand {
        location: crate::telemetry::log_record::CommandLocation::Local,
        index: Some(2),
        program: "old-build".into(),
        args: vec!["--token".into(), "[REDACTED]".into()],
    };
    workspace.view.push(LogRow {
        sequence: 1,
        elapsed_ms: Some(5),
        event: Arc::new(LogEvent {
            namespace: "build".into(),
            message: "failed".into(),
            scope: None,
            kind: LogEventKind::FailedCommand { command },
        }),
    });
    app.log_workspace = Some(workspace);
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert!(app.take_clipboard_request().is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let copied = app.take_clipboard_request().unwrap();
    assert!(copied.contains("old-build"));
    assert!(copied.contains("[REDACTED]"));
    assert!(app.take_clipboard_request().is_none());
    assert!(matches!(app.screen, Screen::Projects));
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.log_workspace.is_none());
}

#[test]
fn export_confirmation_requires_an_unmodified_key_and_writes_exact_preview() {
    let temp = temporary_export_directory();
    let mut app = App::new(
        temp.path().join("projects.yaml"),
        temp.path().join("destinations.yaml"),
        temp.path(),
    )
    .unwrap();
    let preview = crate::application::local_export::LocalExportService::prepare(
        temp.path(),
        "test-export.txt",
        "safe exact payload".into(),
    )
    .unwrap();
    let path = preview.path().to_path_buf();
    let mut workspace = LogWorkspace::new(None, temp.path().to_path_buf());
    workspace.mode = Mode::ExportPreview {
        preview,
        offset: 0,
        scroll: 0,
    };
    app.log_workspace = Some(workspace);
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
    ] {
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers));
        assert!(!path.exists());
        assert!(!app.log_workspace_busy());
    }
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert!(app.log_workspace_busy());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.log_workspace_busy() && std::time::Instant::now() < deadline {
        app.poll_background();
        std::thread::yield_now();
    }
    assert!(!app.log_workspace_busy());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "safe exact payload");
}

fn log_test_app(directory: &std::path::Path) -> App {
    App::new(
        directory.join("projects.yaml"),
        directory.join("destinations.yaml"),
        directory,
    )
    .unwrap()
}

fn temporary_export_directory() -> tempfile::TempDir {
    // Windows needs an inspectable workspace ancestor on this sandboxed host;
    // Unix needs its native temp filesystem, not WSL's /mnt/d permission mapping.
    #[cfg(windows)]
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"));
    #[cfg(not(windows))]
    let directory = tempfile::tempdir();
    directory.unwrap()
}

fn render_log_lines(workspace: &LogWorkspace, width: u16, height: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| workspace.render(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|line| line.iter().map(ratatui::buffer::Cell::symbol).collect())
        .collect()
}

fn drain_log_test_worker(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.log_workspace_busy() && std::time::Instant::now() < deadline {
        app.poll_background();
        std::thread::yield_now();
    }
    assert!(
        !app.log_workspace_busy(),
        "the tracked local worker must terminate"
    );
}

#[test]
fn live_overlay_reports_producer_gaps_even_when_its_own_window_omitted_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let progress = crate::tui::live_progress::LiveProgress::default();
    for index in 0..1_000 {
        progress.record(LogEvent {
            namespace: "build.stdout".into(),
            message: format!("bounded row {index}"),
            scope: None,
            kind: LogEventKind::Output,
        });
    }
    let dropped = progress.snapshot().dropped_rows;
    assert!(dropped > 0);
    app.live_progress = Some(progress);
    app.open_live_logs();
    let workspace = app.log_workspace.as_ref().unwrap();
    assert_eq!(workspace.view.omitted(), 0);
    assert_eq!(workspace.progress.as_ref().unwrap().dropped_rows, dropped);
    let rendered = render_log_lines(workspace, 240, 20).join("\n");
    assert!(rendered.contains("omitted 0"));
    assert!(rendered.contains(&format!("producer gaps {dropped}/0/0")));
    app.poll_live_logs();
    assert_eq!(
        app.log_workspace
            .as_ref()
            .unwrap()
            .progress
            .as_ref()
            .unwrap()
            .dropped_rows,
        dropped
    );
}

#[test]
fn missing_or_unindexed_history_never_claims_retained_files_were_loaded() {
    use crate::application::history_query::{HistoricalLogStatus, LogReadPage};

    let directory = tempfile::tempdir().unwrap();
    for status in [
        HistoricalLogStatus::NotIndexed,
        HistoricalLogStatus::Missing,
    ] {
        let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
        workspace.accept(Response::Page {
            page: Box::new(LogReadPage {
                entries: Vec::new(),
                total_matches: 0,
                next_cursor: None,
                coverage: LogCoverage {
                    status,
                    format: None,
                    available_generations: Vec::new(),
                    issues: Vec::new(),
                    matches_are_complete: false,
                },
            }),
            steps: Vec::new(),
        });
        assert!(workspace.view.matching().is_empty());
        assert!(!workspace.coverage.as_ref().unwrap().matches_are_complete);
        assert!(!workspace.status.contains("Loaded retained local files"));
        assert!(
            !workspace.status.is_empty(),
            "missing evidence needs a visible explanation"
        );
        let rendered = render_log_lines(&workspace, 240, 20).join("\n");
        assert!(rendered.contains(&format!("{status:?}")));
        assert!(!rendered.contains("Loaded retained local files"));
        assert!(rendered.contains("complete within retained files: false"));
    }
}

#[test]
fn live_component_choice_filters_the_window_without_starting_a_history_read() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let progress = crate::tui::live_progress::LiveProgress::default();
    for component in ["backend", "worker"] {
        progress.record(LogEvent {
            namespace: "build.stdout".into(),
            message: "same output".into(),
            scope: Some(LogScope {
                component: ComponentName::parse(component).unwrap(),
                step: "build-package".into(),
            }),
            kind: LogEventKind::Step {
                state: crate::telemetry::log_record::LogStepState::Started,
                persistence: crate::telemetry::log_record::LogPersistence::Recorded,
            },
        });
    }
    app.live_progress = Some(progress);
    app.open_live_logs();
    for code in [KeyCode::Char('f'), KeyCode::Down, KeyCode::Enter] {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    let workspace = app.log_workspace.as_ref().unwrap();
    assert!(workspace.live);
    assert!(!app.log_workspace_busy());
    assert_eq!(workspace.view.matching().len(), 1);
    assert_eq!(
        workspace
            .view
            .selected()
            .unwrap()
            .event
            .scope
            .as_ref()
            .unwrap()
            .component
            .as_str(),
        "backend"
    );
    assert!(workspace.status.contains("live window only"));
}

#[test]
fn selected_record_source_distinguishes_identical_stdout_and_stderr_text() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    for (sequence, namespace) in ["build.stdout", "build.stderr"].into_iter().enumerate() {
        workspace.view.push(LogRow {
            sequence: sequence as u64,
            elapsed_ms: Some(10),
            event: Arc::new(LogEvent {
                namespace: namespace.into(),
                message: "identical body".into(),
                scope: None,
                kind: LogEventKind::Output,
            }),
        });
    }
    workspace.view.first();
    workspace.mode = Mode::Detail { scroll: 0 };
    app.log_workspace = Some(workspace);
    assert!(
        render_log_lines(app.log_workspace.as_ref().unwrap(), 100, 20)
            .join("\n")
            .contains("Source: Build output")
    );
    for code in [KeyCode::Esc, KeyCode::Down, KeyCode::Enter] {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert!(
        render_log_lines(app.log_workspace.as_ref().unwrap(), 100, 20)
            .join("\n")
            .contains("Source: Build diagnostic output")
    );
}

#[test]
fn long_failed_argv_detail_is_scrollable_to_its_actual_last_argument() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let mut args: Vec<_> = (0..96)
        .map(|index| format!("arg-{index:03}-{}", "x".repeat(64)))
        .collect();
    args.push("TAIL_RECORDED_ARGV_NOT_CURRENT_YAML".into());
    let command = crate::telemetry::log_record::RecordedCommand {
        location: crate::telemetry::log_record::CommandLocation::Local,
        index: Some(2),
        program: "frozen-build".into(),
        args,
    };
    let serialized = serde_json::to_string_pretty(&command).unwrap();
    assert!(
        serialized
            .find("TAIL_RECORDED_ARGV_NOT_CURRENT_YAML")
            .unwrap()
            > 4096
    );
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    assert!(workspace.view.push(LogRow {
        sequence: 1,
        elapsed_ms: Some(20),
        event: Arc::new(LogEvent {
            namespace: "build.command_failed".into(),
            message: "Recorded failure".into(),
            scope: None,
            kind: LogEventKind::FailedCommand { command },
        }),
    }));
    app.log_workspace = Some(workspace);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        !render_log_lines(app.log_workspace.as_ref().unwrap(), 100, 20)
            .join("\n")
            .contains("TAIL_RECORDED_ARGV_NOT_CURRENT_YAML")
    );
    let mut found_tail = false;
    for _ in 0..40 {
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        let rendered = render_log_lines(app.log_workspace.as_ref().unwrap(), 100, 20).join("\n");
        assert!(!rendered.contains("[display truncated]"));
        if rendered.contains("TAIL_RECORDED_ARGV_NOT_CURRENT_YAML") {
            found_tail = true;
            break;
        }
    }
    assert!(
        found_tail,
        "the full frozen argv must remain reachable beyond 4096 characters"
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(
        app.take_clipboard_request().as_deref(),
        Some(serialized.as_str())
    );
}

#[test]
fn exact_export_preview_preserves_line_breaks_and_scrolls_within_the_chunk() {
    let directory = temporary_export_directory();
    let mut app = log_test_app(directory.path());
    let text = format!(
        "PREVIEW_FIRST_LINE\nPREVIEW_SECOND_LINE\n\n{}PREVIEW_LAST_LINE\n",
        "middle line\n".repeat(100)
    );
    assert!(text.len() < PREVIEW_CHUNK_BYTES);
    let preview = crate::application::local_export::LocalExportService::prepare(
        directory.path(),
        "multiline-export.txt",
        text.clone(),
    )
    .unwrap();
    let path = preview.path().to_path_buf();
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    workspace.mode = Mode::ExportPreview {
        preview,
        offset: 0,
        scroll: 0,
    };
    app.log_workspace = Some(workspace);
    let rendered = render_log_lines(app.log_workspace.as_ref().unwrap(), 100, 24);
    let first = rendered
        .iter()
        .position(|line| line.contains("PREVIEW_FIRST_LINE"))
        .unwrap();
    let second = rendered
        .iter()
        .position(|line| line.contains("PREVIEW_SECOND_LINE"))
        .unwrap();
    assert_eq!(
        second,
        first + 1,
        "LF must be rendered as a real line boundary"
    );
    assert!(!rendered.join("\n").contains("PREVIEW_LAST_LINE"));
    let mut found_tail = false;
    for _ in 0..20 {
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(matches!(
            app.log_workspace.as_ref().unwrap().mode,
            Mode::ExportPreview { offset: 0, .. }
        ));
        if render_log_lines(app.log_workspace.as_ref().unwrap(), 100, 24)
            .join("\n")
            .contains("PREVIEW_LAST_LINE")
        {
            found_tail = true;
            break;
        }
    }
    assert!(
        found_tail,
        "every line of the current chunk must be inspectable before confirmation"
    );
    assert!(!path.exists());
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    drain_log_test_worker(&mut app);
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
}

struct ControlledLogOperation {
    started: std::sync::mpsc::Receiver<()>,
    cancelled: std::sync::mpsc::Receiver<()>,
    release: Option<std::sync::mpsc::Sender<()>>,
    cancellation: tokio_util::sync::CancellationToken,
    completed: Arc<std::sync::atomic::AtomicBool>,
}

impl ControlledLogOperation {
    fn new() -> (LogTask, Self) {
        let (started_tx, started) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let task = LogTask::controlled(cancellation.clone(), move |token| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            started_tx.send(()).unwrap();
            runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), token.cancelled())
                    .await
                    .unwrap();
            });
            cancelled_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            worker_completed.store(true, std::sync::atomic::Ordering::SeqCst);
            Err("Controlled local log operation cancelled; no export was published.".into())
        })
        .unwrap();
        (
            task,
            Self {
                started,
                cancelled,
                release: Some(release),
                cancellation,
                completed,
            },
        )
    }

    fn release(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
    }
}

impl Drop for ControlledLogOperation {
    fn drop(&mut self) {
        // Assertion failures also unblock the finite-lived real worker.
        self.cancellation.cancel();
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[test]
fn busy_escape_cancels_without_closing_or_detaching_the_real_local_worker() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let (task, mut control) = ControlledLogOperation::new();
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    workspace.task = Some(task);
    app.log_workspace = Some(workspace);
    control
        .started
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    control
        .cancelled
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(app.log_workspace_busy());
    assert!(!control.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    assert_eq!(app.exit_state(), super::super::ExitState::Running);
    assert!(app.log_workspace_busy());
    app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
    assert!(app.help_open);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.help_open);
    assert!(app.log_workspace_busy());
    control.release();
    drain_log_test_worker(&mut app);
    assert!(control.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(app.log_workspace.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.log_workspace.is_none());
    assert!(matches!(app.screen, Screen::Projects));
}

#[test]
fn shutdown_waits_for_cancelled_log_worker_to_terminate_before_returning() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = log_test_app(directory.path());
    let (task, mut control) = ControlledLogOperation::new();
    let mut workspace = LogWorkspace::new(None, directory.path().to_path_buf());
    workspace.task = Some(task);
    app.log_workspace = Some(workspace);
    control
        .started
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let (returned_tx, returned_rx) = std::sync::mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        app.shutdown();
        returned_tx.send(()).unwrap();
        app
    });
    control
        .cancelled
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(!control.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(
        returned_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    control.release();
    returned_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let app = shutdown.join().unwrap();
    assert!(control.completed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!app.log_workspace_busy());
}
