use std::{
    collections::VecDeque,
    sync::Mutex,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use crossterm::event::{KeyEvent, KeyModifiers};
use tokio::sync::Notify;

use super::{
    tests::{fixture, screen},
    *,
};

#[derive(Debug)]
pub(super) struct ControlledGateway {
    entered: Notify,
    cancelled: Notify,
    release: Notify,
    wait_for_cancel: bool,
    results: Mutex<VecDeque<Result<ManagementPage, String>>>,
    pub(super) calls: AtomicUsize,
}

impl ControlledGateway {
    pub(super) fn new(result: Result<ManagementPage, String>, wait_for_cancel: bool) -> Self {
        Self {
            entered: Notify::new(),
            cancelled: Notify::new(),
            release: Notify::new(),
            wait_for_cancel,
            results: Mutex::new(VecDeque::from([result])),
            calls: AtomicUsize::new(0),
        }
    }

    pub(super) async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .unwrap();
    }

    pub(super) async fn cancelled(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.cancelled.notified())
            .await
            .unwrap();
    }

    pub(super) fn release(&self) {
        self.release.notify_one();
    }
}

#[async_trait(?Send)]
impl ManagementGateway for ControlledGateway {
    async fn run(
        &self,
        _: &ManagementScope,
        _: ManagementRequest,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            if self.wait_for_cancel {
                cancellation.cancelled().await;
                self.cancelled.notify_one();
            }
            self.release.notified().await;
        })
        .await
        .map_err(|_| "Controlled worker was not released".to_owned())?;
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .expect("one result per request")
    }
}

fn press(app: &mut App, code: KeyCode) {
    assert!(!app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
}

pub(super) async fn finish(app: &mut App) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.management_task.is_some() {
            app.poll_background();
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tracked management worker must finish");
}

fn environments(items: Vec<EnvironmentId>, offset: u32, cursor: usize) -> ManagementPage {
    ManagementPage::Environments {
        page: Arc::new(HistoryPage {
            items,
            more: true,
            database_missing: false,
        }),
        offset,
        cursor,
    }
}

fn report() -> RecoveryReport {
    RecoveryReport {
        id: uuid::Uuid::now_v7(),
        related_deployment: Some(DeploymentId::new()),
        source_revision: None,
        started_at_ms: 1,
        completed_at_ms: 2,
        components: Vec::new(),
    }
}

#[test]
fn historical_home_returns_to_the_exact_id_page_and_view_without_changing_current_environment() {
    let (_directory, mut app) = fixture();
    let original = screen(&app).scope;
    let selected = EnvironmentId::new();
    let mut current = screen(&app);
    current.push_page(environments(
        vec![EnvironmentId::new(), selected.clone()],
        20,
        1,
    ));
    current.view.handle_key(KeyCode::Char(']'));
    let horizontal = current.view.horizontal();
    app.screen = Screen::Management(current);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        screen(&app).scope.historical_environment.as_ref(),
        Some(&selected)
    );
    press(&mut app, KeyCode::Esc);
    let restored = screen(&app);
    assert!(matches!(
        restored.page,
        ManagementPage::Environments {
            offset: 20,
            cursor: 1,
            ..
        }
    ));
    assert_eq!(restored.view.horizontal(), horizontal);
    assert!(restored.scope.historical_environment.is_none());
    assert_eq!(restored.scope.environment, original.environment);
    press(&mut app, KeyCode::Esc);
    assert!(matches!(screen(&app).page, ManagementPage::Home));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_detail_returns_to_the_exact_page_and_selected_deployment_without_another_read() {
    let (directory, mut app) = fixture();
    let current = screen(&app);
    let store =
        crate::history::HistoryStore::open(&directory.path().join("history.sqlite3")).unwrap();
    for timestamp in 0..24 {
        store
            .create_deployment(
                &DeploymentId::new(),
                &current.scope.config.project_id,
                current.scope.environment_id().unwrap(),
                timestamp,
            )
            .unwrap();
    }
    press(&mut app, KeyCode::Char('h'));
    finish(&mut app).await;
    press(&mut app, KeyCode::Char('n'));
    finish(&mut app).await;
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char(']'));
    let before = screen(&app);
    let ManagementPage::History { page, cursor, .. } = &before.page else {
        panic!("history expected")
    };
    let selected = page.items[*cursor].deployment.clone();
    press(&mut app, KeyCode::Enter);
    finish(&mut app).await;
    press(&mut app, KeyCode::Esc);
    assert!(app.management_task.is_none());
    let restored = screen(&app);
    let ManagementPage::History {
        page,
        offset,
        cursor,
    } = &restored.page
    else {
        panic!("history expected")
    };
    assert_eq!((*offset, *cursor), (20, 1));
    assert_eq!(page.items[*cursor].deployment, selected);
    assert_eq!(restored.view.horizontal(), before.view.horizontal());
    assert_eq!(
        restored.back.len(),
        1,
        "pagination must replace, not add return entries"
    );
}

#[test]
fn saved_report_returns_to_its_exact_list_and_selection() {
    let (_directory, mut app) = fixture();
    let selected = Arc::new(report());
    let mut current = screen(&app);
    current.push_page(ManagementPage::Reports {
        page: Arc::new(HistoryPage {
            items: vec![report(), (*selected).clone()],
            more: true,
            database_missing: false,
        }),
        offset: 20,
        cursor: 1,
    });
    current.view.handle_key(KeyCode::Char(']'));
    let horizontal = current.view.horizontal();
    current.push_page(ManagementPage::Report {
        report: Arc::clone(&selected),
        warning: None,
    });
    app.screen = Screen::Management(current);
    press(&mut app, KeyCode::Esc);
    let current = screen(&app);
    let ManagementPage::Reports {
        page,
        offset,
        cursor,
    } = current.page
    else {
        panic!("reports expected")
    };
    assert_eq!((offset, cursor), (20, 1));
    assert_eq!(page.items[cursor].id, selected.id);
    assert_eq!(current.view.horizontal(), horizontal);
}

#[test]
fn collection_refresh_retains_stable_identity_and_clamps_when_it_disappears() {
    let (_directory, app) = fixture();
    let a = EnvironmentId::new();
    let b = EnvironmentId::new();
    let mut current = screen(&app);
    current.push_page(environments(vec![a.clone(), b.clone()], 20, 1));
    current.accept_result(
        &ManagementRequest::Environments(RecoveryQuery {
            offset: 20,
            limit: 20,
        }),
        environments(vec![EnvironmentId::new(), a.clone(), b.clone()], 20, 0),
    );
    assert!(matches!(
        current.page,
        ManagementPage::Environments { cursor: 2, .. }
    ));
    current.accept_result(
        &ManagementRequest::Environments(RecoveryQuery {
            offset: 20,
            limit: 20,
        }),
        environments(vec![a], 20, 0),
    );
    assert!(matches!(
        current.page,
        ManagementPage::Environments { cursor: 0, .. }
    ));
    current.accept_result(
        &ManagementRequest::Environments(RecoveryQuery {
            offset: 40,
            limit: 20,
        }),
        environments(vec![b], 40, 0),
    );
    assert_eq!(current.back.len(), 1);
    assert!(matches!(
        current.page,
        ManagementPage::Environments {
            offset: 40,
            cursor: 0,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_is_persistent_and_only_explicit_retry_replaces_the_old_page() {
    let (_directory, mut app) = fixture();
    let selected = EnvironmentId::new();
    let mut origin = screen(&app);
    origin.push_page(environments(vec![selected.clone()], 20, 0));
    app.screen = Screen::Management(origin);
    let gateway = Arc::new(ControlledGateway::new(
        Err("Read failed; check local storage.".into()),
        false,
    ));
    gateway
        .results
        .lock()
        .unwrap()
        .push_back(Ok(environments(vec![selected.clone()], 20, 0)));
    app.management_gateway = gateway.clone();
    press(&mut app, KeyCode::Char('f'));
    gateway.entered().await;
    gateway.release();
    finish(&mut app).await;
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::F(1));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(
        screen(&app).page,
        ManagementPage::Failed {
            retry: Some(ManagementRequest::Environments(_)),
            ..
        }
    ));
    assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    press(&mut app, KeyCode::Char('f'));
    gateway.entered().await;
    gateway.release();
    finish(&mut app).await;
    assert!(matches!(
        screen(&app).page,
        ManagementPage::Environments {
            offset: 20,
            cursor: 0,
            ..
        }
    ));
    assert!(screen(&app).notice.is_none());
    assert_eq!(screen(&app).back.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_read_waits_for_the_worker_and_keeps_a_persistent_original_page_notice() {
    let (_directory, mut app) = fixture();
    let gateway = Arc::new(ControlledGateway::new(
        Ok(environments(Vec::new(), 0, 0)),
        true,
    ));
    app.management_gateway = gateway.clone();
    press(&mut app, KeyCode::Char('a'));
    gateway.entered().await;
    press(&mut app, KeyCode::Esc);
    gateway.cancelled().await;
    assert!(app.management_task.is_some());
    assert!(matches!(
        screen(&app).page,
        ManagementPage::Loading {
            cancelling: true,
            ..
        }
    ));
    gateway.release();
    finish(&mut app).await;
    press(&mut app, KeyCode::Down);
    assert!(matches!(screen(&app).page, ManagementPage::Home));
    assert!(
        screen(&app)
            .notice
            .as_deref()
            .unwrap()
            .contains("cancelled")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_inspection_retains_its_real_report_and_failed_inspection_does_not_offer_retry() {
    let (_directory, mut app) = fixture();
    press(&mut app, KeyCode::Char('i'));
    let saved = Arc::new(report());
    let gateway = Arc::new(ControlledGateway::new(
        Ok(ManagementPage::Report {
            report: Arc::clone(&saved),
            warning: Some("Observed facts could not be cached".into()),
        }),
        true,
    ));
    app.management_gateway = gateway.clone();
    press(&mut app, KeyCode::Enter);
    gateway.entered().await;
    press(&mut app, KeyCode::Esc);
    gateway.cancelled().await;
    gateway.release();
    finish(&mut app).await;
    assert!(
        matches!(screen(&app).page, ManagementPage::Report { report, warning: Some(_) } if report.id == saved.id)
    );
    assert!(screen(&app).notice.is_some());
    press(&mut app, KeyCode::Esc);
    let gateway = Arc::new(ControlledGateway::new(
        Err("Inspection result is unavailable.".into()),
        false,
    ));
    app.management_gateway = gateway.clone();
    press(&mut app, KeyCode::Enter);
    gateway.entered().await;
    gateway.release();
    finish(&mut app).await;
    assert!(matches!(
        screen(&app).page,
        ManagementPage::Failed { retry: None, .. }
    ));
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    press(&mut app, KeyCode::Esc);
    assert!(matches!(
        screen(&app).page,
        ManagementPage::InspectSelection { .. }
    ));
    assert!(screen(&app).notice.is_some());
}

#[test]
fn return_snapshots_are_nonrecursive_and_bounded() {
    let (_directory, app) = fixture();
    let mut current = screen(&app);
    for _ in 0..32 {
        current.push_page(ManagementPage::Report {
            report: Arc::new(report()),
            warning: None,
        });
    }
    assert_eq!(current.back.len(), 8);
    for _ in 0..9 {
        current.go_back();
    }
    assert!(current.back.is_empty());
    assert!(matches!(current.page, ManagementPage::Home));
}
