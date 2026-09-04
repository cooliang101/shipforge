//! Execute errors travel through the real background gateway/event route.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::telemetry::log_record::{
    CommandLocation, LogEvent, LogEventKind, LogScope, RecordedCommand,
};

#[derive(Debug)]
struct FailingEventGateway {
    deployment: Option<DeploymentId>,
    calls: AtomicUsize,
}

#[async_trait(?Send)]
impl ManagementGateway for FailingEventGateway {
    async fn run(
        &self,
        _: &ManagementScope,
        _: ManagementRequest,
        _: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        panic!("confirmed rollback must use its structured event route");
    }

    async fn run_with_events(
        &self,
        _: &ManagementScope,
        request: ManagementRequest,
        events: &dyn EventSink,
        _: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        assert!(matches!(request, ManagementRequest::Execute(_)));
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(deployment) = &self.deployment {
            events.emit_record(start_event(deployment));
            events.emit_record(LogEvent {
                namespace: "rollback.command_failed".into(),
                message: "THIS_ROLLBACK_COMMAND_FAILED".into(),
                scope: Some(LogScope {
                    component: name("frontend"),
                    step: "rollback".into(),
                }),
                kind: LogEventKind::FailedCommand {
                    command: RecordedCommand {
                        location: CommandLocation::Remote,
                        index: None,
                        program: "recorded-rollback-tool".into(),
                        args: vec![deployment.to_string(), "literal argument".into()],
                    },
                },
            });
        }
        Err("Rollback execution could not persist its result; inspect known effects before retrying.".into())
    }
}

fn start_event(deployment: &DeploymentId) -> LogEvent {
    LogEvent {
        namespace: "deployment.started".into(),
        message: "Rollback request started".into(),
        scope: None,
        kind: LogEventKind::DeploymentStarted {
            deployment: deployment.clone(),
        },
    }
}

fn log_overlay_text(app: &App) -> String {
    let workspace = app.log_workspace.as_ref().expect("log overlay required");
    let mut terminal = Terminal::new(TestBackend::new(240, 28)).unwrap();
    terminal
        .draw(|frame| workspace.render(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

async fn finish_logs(app: &mut App) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.log_workspace_busy() {
            app.poll_background();
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("local history query must finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_error_keeps_this_requests_logs_and_does_not_hijack_source_history() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    let rollback = DeploymentId::new();
    assert_ne!(rollback, fixture.details.record.deployment);
    let gateway = Arc::new(FailingEventGateway {
        deployment: Some(rollback.clone()),
        calls: AtomicUsize::new(0),
    });
    fixture.app.management_gateway = gateway.clone();
    press(&mut fixture.app, KeyCode::Char('c'));
    let request_id = fixture.app.management_task.as_ref().unwrap().id;
    // A late completion from another request cannot claim this operation's page.
    fixture
        .app
        .finish_management(uuid::Uuid::now_v7(), Err("stale result".into()));
    assert_eq!(fixture.app.management_task.as_ref().unwrap().id, request_id);
    finished(&mut fixture.app).await;
    let failed_screen = super::super::tests::screen(&fixture.app);
    let ManagementPage::RollbackFailed {
        request_id: recorded_request,
        progress,
        ..
    } = &failed_screen.page
    else {
        panic!("execute Err requires its own non-confirming result page");
    };
    assert_eq!(*recorded_request, request_id);
    assert_eq!(progress.snapshot().deployment, Some(rollback.clone()));
    assert!(progress.snapshot().finished);
    assert!(fixture.app.management_has_live_progress());
    let result = rendered(&failed_screen, &fixture.app);
    assert!(result.contains("Rollback did not complete normally"));
    assert!(result.contains(&rollback.to_string()));
    assert!(!result.contains("CONFIRM ROLLBACK"));
    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(fixture.app.management_task.is_none());
    assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    press(&mut fixture.app, KeyCode::Char('l'));
    let logs = log_overlay_text(&fixture.app);
    assert!(logs.contains(&rollback.to_string()));
    assert!(!logs.contains(&fixture.details.record.deployment.to_string()));
    assert!(logs.contains("THIS_ROLLBACK_COMMAND_FAILED"));
    press(&mut fixture.app, KeyCode::Char('y'));
    let copied = fixture.app.take_clipboard_request().unwrap();
    assert!(copied.contains("recorded-rollback-tool"));
    assert!(copied.contains(&rollback.to_string()));
    press(&mut fixture.app, KeyCode::Esc);

    // The source record's normal l entry must keep its own exact historical scope.
    let mut source_screen = failed_screen.clone();
    source_screen.page = ManagementPage::Detail(Arc::clone(&fixture.details));
    fixture.app.screen = Screen::Management(source_screen);
    assert!(!fixture.app.management_has_live_progress());
    press(&mut fixture.app, KeyCode::Char('l'));
    finish_logs(&mut fixture.app).await;
    let source_logs = log_overlay_text(&fixture.app);
    assert!(source_logs.contains(&fixture.details.record.deployment.to_string()));
    assert!(!source_logs.contains(&rollback.to_string()));
    assert!(!source_logs.contains("THIS_ROLLBACK_COMMAND_FAILED"));
    press(&mut fixture.app, KeyCode::Char('v'));
    assert!(
        log_overlay_text(&fixture.app).contains("No live window belongs to this exact deployment")
    );
    press(&mut fixture.app, KeyCode::Esc);

    // A different worker cannot reuse this failure page even with an equal ID.
    fixture.app.screen = Screen::Management(failed_screen);
    let replacement = crate::tui::live_progress::LiveProgress::default();
    replacement.record(start_event(&rollback));
    fixture.app.live_progress = Some(replacement);
    assert!(!fixture.app.management_has_live_progress());
    press(&mut fixture.app, KeyCode::Char('l'));
    assert!(fixture.app.log_workspace.is_none());
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_error_without_an_id_never_substitutes_the_previous_deployment() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    let previous = crate::tui::live_progress::LiveProgress::default();
    previous.record(start_event(&fixture.details.record.deployment));
    fixture.app.live_progress = Some(previous);
    fixture.app.management_gateway = Arc::new(FailingEventGateway {
        deployment: None,
        calls: AtomicUsize::new(0),
    });
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    let screen = super::super::tests::screen(&fixture.app);
    let result = rendered(&screen, &fixture.app);
    assert!(result.contains("supplied no Deployment ID"));
    assert!(!result.contains(&fixture.details.record.deployment.to_string()));
    assert!(fixture.app.management_has_live_progress());
    press(&mut fixture.app, KeyCode::Char('l'));
    assert!(log_overlay_text(&fixture.app).contains("Deployment ID pending"));
    press(&mut fixture.app, KeyCode::Char('h'));
    assert!(!fixture.app.log_workspace_busy());
    assert!(
        !log_overlay_text(&fixture.app).contains(&fixture.details.record.deployment.to_string())
    );
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
}
