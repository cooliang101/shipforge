use std::sync::atomic::Ordering;

use super::super::navigation_tests::ControlledGateway;
use super::*;

#[derive(Debug)]
struct DelayedExecutionResultGateway {
    service: Arc<ServiceGateway>,
    delivery: Arc<ControlledGateway>,
    known: Mutex<Option<Arc<RollbackReport>>>,
    report_delivery_error: bool,
}

#[async_trait(?Send)]
impl ManagementGateway for DelayedExecutionResultGateway {
    async fn run(
        &self,
        _: &ManagementScope,
        _: ManagementRequest,
        _: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        panic!("rollback execution must retain its real event stream");
    }

    async fn run_with_events(
        &self,
        scope: &ManagementScope,
        request: ManagementRequest,
        events: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        assert!(matches!(request, ManagementRequest::Execute(_)));
        let page = self
            .service
            .run_with_events(scope, request.clone(), events, cancellation)
            .await?;
        let ManagementPage::RollbackFinished(report) = &page else {
            panic!("the real rollback must provide its known outcome");
        };
        *self.known.lock().unwrap() = Some(Arc::clone(report));
        // Reuse the bounded controlled worker gate only to delay delivery. The
        // result and events above come from the real service and sealed plan.
        let released = self.delivery.run(scope, request, cancellation).await;
        assert_eq!(released.unwrap_err(), "Result delivery gate released");
        if self.report_delivery_error {
            Err("Result delivery failed after execution; inspect the recorded outcome.".into())
        } else {
            Ok(page)
        }
    }
}

async fn cancelled_execution_preserves_its_outcome_and_consumes_navigation(
    report_delivery_error: bool,
) {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    let delivery = Arc::new(ControlledGateway::new(
        Err("Result delivery gate released".into()),
        true,
    ));
    let gateway = Arc::new(DelayedExecutionResultGateway {
        service: Arc::clone(&fixture.gateway),
        delivery: Arc::clone(&delivery),
        known: Mutex::new(None),
        report_delivery_error,
    });
    fixture.app.management_gateway = gateway.clone();
    press(&mut fixture.app, KeyCode::Char('c'));
    delivery.entered().await; // Actual execution and persistence have finished.
    let known = gateway.known.lock().unwrap().clone().unwrap();
    assert_ne!(known.deployment.id, fixture.details.record.deployment);
    assert_eq!(known.deployment.state, DeploymentState::Succeeded);
    assert_eq!(fixture.driver.0.lock().unwrap().mutations.len(), 1);
    press(&mut fixture.app, KeyCode::Esc);
    delivery.cancelled().await;
    for key in [KeyCode::Char('c'), KeyCode::Char('f'), KeyCode::F(4)] {
        press(&mut fixture.app, key);
        assert!(fixture.app.management_task.is_some());
        assert!(fixture.app.picker.is_none());
    }
    assert!(matches!(
        super::super::tests::screen(&fixture.app).page,
        ManagementPage::Loading {
            cancelling: true,
            ..
        }
    ));
    delivery.release();
    super::super::navigation_tests::finish(&mut fixture.app).await;
    let result = super::super::tests::screen(&fixture.app);
    if report_delivery_error {
        let ManagementPage::RollbackFailed { progress, .. } = &result.page else {
            panic!("delivery error must not restore the consumed review");
        };
        assert_eq!(
            progress.snapshot().deployment.as_ref(),
            Some(&known.deployment.id)
        );
    } else {
        assert_completed(&fixture, &result);
        let ManagementPage::RollbackFinished(report) = &result.page else {
            unreachable!()
        };
        assert_eq!(report.deployment.id, known.deployment.id);
    }
    assert!(
        result
            .notice
            .as_deref()
            .unwrap()
            .contains("Cancellation was requested")
    );
    let calls = fixture.gateway.requests.lock().unwrap().len();
    for key in [KeyCode::Char('c'), KeyCode::Char('f'), KeyCode::F(4)] {
        press(&mut fixture.app, key);
        assert!(fixture.app.management_task.is_none());
        assert!(fixture.app.picker.is_none());
    }
    press(&mut fixture.app, KeyCode::Esc);
    let ManagementPage::Detail(details) = super::super::tests::screen(&fixture.app).page else {
        panic!("only the read-only source details may remain on the return path");
    };
    assert_eq!(details.record.deployment, fixture.details.record.deployment);
    for key in [KeyCode::Char('c'), KeyCode::Char('f'), KeyCode::F(4)] {
        press(&mut fixture.app, key);
        assert!(fixture.app.management_task.is_none());
        assert!(fixture.app.picker.is_none());
    }
    assert_eq!(fixture.gateway.requests.lock().unwrap().len(), calls);
    assert_eq!(delivery.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.driver.0.lock().unwrap().mutations.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_execute_worker_preserves_a_late_known_finished_result_without_old_confirmation()
{
    cancelled_execution_preserves_its_outcome_and_consumes_navigation(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_execute_worker_preserves_a_late_error_identity_without_old_confirmation() {
    cancelled_execution_preserves_its_outcome_and_consumes_navigation(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_rollback_preview_and_target_detail_restore_exact_target_selection() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    press(&mut fixture.app, KeyCode::Esc);
    press(&mut fixture.app, KeyCode::Char(']'));
    let original = super::super::tests::screen(&fixture.app);
    let ManagementPage::RollbackTargets {
        candidates,
        selected,
        options,
        cursor,
    } = &original.page
    else {
        panic!("rejecting preview must restore targets");
    };
    assert_eq!(selected, &BTreeSet::from([name("frontend")]));
    assert_eq!(
        candidates.components[*cursor].options[options[&name("frontend")]]
            .target
            .as_ref(),
        Some(&fixture.target)
    );
    let calls = fixture.gateway.requests.lock().unwrap().len();
    press(&mut fixture.app, KeyCode::Char('d'));
    assert!(matches!(
        super::super::tests::screen(&fixture.app).page,
        ManagementPage::RollbackTargetDetail { .. }
    ));
    for key in [KeyCode::Enter, KeyCode::Char('c'), KeyCode::Char(' ')] {
        press(&mut fixture.app, key);
        assert!(fixture.app.management_task.is_none());
    }
    press(&mut fixture.app, KeyCode::Esc);
    let restored = super::super::tests::screen(&fixture.app);
    let ManagementPage::RollbackTargets {
        selected: restored_selected,
        options: restored_options,
        cursor: restored_cursor,
        ..
    } = &restored.page
    else {
        panic!("target selection must survive details");
    };
    assert_eq!(
        (restored_selected, restored_options, restored_cursor),
        (selected, options, cursor)
    );
    assert_eq!(restored.view.horizontal(), original.view.horizontal());
    assert_eq!(fixture.gateway.requests.lock().unwrap().len(), calls);
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_plan_waits_for_a_real_worker_and_discards_its_late_sealed_preview() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    let ManagementPage::RollbackReview(plan) = super::super::tests::screen(&fixture.app).page
    else {
        panic!("application-produced sealed plan expected");
    };
    press(&mut fixture.app, KeyCode::Esc);
    let gateway = Arc::new(ControlledGateway::new(
        Ok(ManagementPage::RollbackReview(plan)),
        true,
    ));
    fixture.app.management_gateway = gateway.clone();
    press(&mut fixture.app, KeyCode::Enter);
    gateway.entered().await;
    press(&mut fixture.app, KeyCode::F(1));
    fixture
        .app
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    gateway.cancelled().await;
    assert!(fixture.app.management_task.is_some());
    assert!(matches!(
        super::super::tests::screen(&fixture.app).page,
        ManagementPage::Loading {
            cancelling: true,
            ..
        }
    ));
    press(&mut fixture.app, KeyCode::Esc); // Close help, not the running request.
    assert!(fixture.app.management_task.is_some());
    gateway.release();
    super::super::navigation_tests::finish(&mut fixture.app).await;
    let restored = super::super::tests::screen(&fixture.app);
    assert!(matches!(
        restored.page,
        ManagementPage::RollbackTargets { .. }
    ));
    assert!(restored.notice.as_deref().unwrap().contains("cancelled"));
    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(fixture.app.management_task.is_none());
    assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_rollback_consumes_all_old_preview_and_candidate_return_paths() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    assert_completed(&fixture, &super::super::tests::screen(&fixture.app));
    let calls = fixture.gateway.requests.lock().unwrap().len();
    press(&mut fixture.app, KeyCode::Esc);
    let ManagementPage::Detail(details) = super::super::tests::screen(&fixture.app).page else {
        panic!("result must return to read-only source details, not a consumed plan");
    };
    assert_eq!(details.record.deployment, fixture.details.record.deployment);
    for _ in 0..4 {
        press(&mut fixture.app, KeyCode::Char('c'));
        assert!(fixture.app.management_task.is_none());
        let current = super::super::tests::screen(&fixture.app);
        assert!(!matches!(
            current.page,
            ManagementPage::RollbackReview(_) | ManagementPage::RollbackTargets { .. }
        ));
        if current.back.is_empty() {
            break;
        }
        press(&mut fixture.app, KeyCode::Esc);
    }
    assert_eq!(fixture.gateway.requests.lock().unwrap().len(), calls);
    assert_eq!(fixture.driver.0.lock().unwrap().mutations.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_runtime_cannot_leave_a_confirmed_rollback_plan_on_the_return_path() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    fixture.app.runtime = None;
    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(matches!(
        super::super::tests::screen(&fixture.app).page,
        ManagementPage::Failed { retry: None, .. }
    ));
    assert!(fixture.app.management_task.is_none());
    press(&mut fixture.app, KeyCode::Esc);
    assert!(matches!(
        super::super::tests::screen(&fixture.app).page,
        ManagementPage::Detail(_)
    ));
    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(fixture.app.management_task.is_none());
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
}
