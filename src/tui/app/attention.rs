use std::{path::PathBuf, sync::Arc, thread::JoinHandle};

use tokio_util::sync::CancellationToken;

use crate::{application::LocalAttentionSummary, domain::ProjectId};

use super::{App, BackgroundEvent, Screen, catch_worker_failure};

pub(super) trait TuiAttentionGateway: std::fmt::Debug + Send + Sync {
    fn query(
        &self,
        project: Option<&ProjectId>,
        cancellation: &CancellationToken,
    ) -> Result<LocalAttentionSummary, String>;
}

#[derive(Debug)]
pub(super) struct LocalAttentionGateway {
    pub history: PathBuf,
}

impl TuiAttentionGateway for LocalAttentionGateway {
    fn query(
        &self,
        project: Option<&ProjectId>,
        cancellation: &CancellationToken,
    ) -> Result<LocalAttentionSummary, String> {
        if cancellation.is_cancelled() {
            return Err("Local attention check cancelled.".into());
        }
        let result = crate::application::local_attention(&self.history, project, None)
            .map_err(|error| error.to_string());
        if cancellation.is_cancelled() {
            Err("Local attention check cancelled.".into())
        } else {
            result
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AttentionRequest {
    pub id: uuid::Uuid,
    pub project: Option<ProjectId>,
}

#[derive(Debug, Default)]
pub(super) struct AttentionState {
    /// At most one worker runs; opening more projects replaces the queued request.
    pub request: Option<AttentionRequest>,
    pub in_flight: Option<uuid::Uuid>,
    cancellation: Option<CancellationToken>,
    worker: Option<JoinHandle<()>>,
    pub project: Option<ProjectId>,
    pub notice: Option<String>,
}

impl App {
    pub(super) fn refresh_attention(&mut self, project: Option<ProjectId>) {
        self.invalidate_attention();
        if self.attention_suppressed() {
            return;
        }
        self.attention.project.clone_from(&project);
        self.attention.request = Some(AttentionRequest {
            id: uuid::Uuid::now_v7(),
            project,
        });
        self.start_attention_worker();
    }

    pub(super) fn invalidate_attention(&mut self) {
        self.attention.request = None;
        self.attention.notice = None;
        if let Some(cancellation) = &self.attention.cancellation {
            cancellation.cancel();
        }
    }

    fn attention_suppressed(&self) -> bool {
        self.exit_state == super::ExitState::Waiting
            || self.deployment_session.is_active()
            || self.management_task.is_some()
            || self.connections_task.is_some()
            || self.project_edit_task.is_some()
            || self.reinitialize_task.is_some()
            || self.remote_target_task.is_some()
            || matches!(self.screen, Screen::DeploymentRunning { .. })
    }

    fn start_attention_worker(&mut self) {
        if self.attention.in_flight.is_some() || self.attention_suppressed() {
            return;
        }
        let Some(request) = self.attention.request.clone() else {
            return;
        };
        let id = request.id;
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let gateway = Arc::clone(&self.attention_gateway);
        let sender = self.background_sender.clone();
        let spawned = std::thread::Builder::new()
            .name("shipforge-local-attention".into())
            .spawn(move || {
                let result = catch_worker_failure(|| {
                    gateway.query(request.project.as_ref(), &worker_cancellation)
                });
                let _ = sender.send(BackgroundEvent::LocalAttention(request, result));
            });
        if let Ok(worker) = spawned {
            self.attention.in_flight = Some(id);
            self.attention.cancellation = Some(cancellation);
            self.attention.worker = Some(worker);
        } else {
            self.attention.request = None;
            self.attention.notice = Some(unavailable_notice().into());
        }
    }

    pub(super) fn finish_attention(
        &mut self,
        request: &AttentionRequest,
        result: Result<LocalAttentionSummary, String>,
    ) {
        if self.attention.in_flight != Some(request.id) {
            return;
        }
        let cancelled = self
            .attention
            .cancellation
            .take()
            .is_some_and(|cancellation| cancellation.is_cancelled());
        let worker_failed = self
            .attention
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        self.attention.in_flight = None;
        if self.attention.request.as_ref() == Some(request) {
            self.attention.request = None;
            if !cancelled && !self.attention_suppressed() {
                self.attention.notice = attention_notice(if worker_failed {
                    Err("Local attention worker stopped unexpectedly.".into())
                } else {
                    result
                });
            }
        }
        self.start_attention_worker();
    }

    pub(super) fn cancel_attention_for_shutdown(&mut self) {
        self.attention.request = None;
        if let Some(cancellation) = &self.attention.cancellation {
            cancellation.cancel();
        }
    }

    pub(super) fn attention_busy(&self) -> bool {
        self.attention.in_flight.is_some() || self.attention.worker.is_some()
    }

    pub(super) fn show_projects(&mut self) {
        self.screen = Screen::Projects;
        self.refresh_attention(None);
    }

    pub(super) fn show_overview(&mut self, root: PathBuf, config: crate::config::ProjectConfig) {
        self.reset_overview_scroll();
        self.refresh_destination_labels();
        if let Some(environment) = self.preferred_environment(&config) {
            self.remember_environment(&config, &environment);
        }
        let project = config.project_id.clone();
        self.screen = Screen::Overview { root, config };
        self.refresh_attention(Some(project));
    }

    pub fn attention_notice(&self) -> Option<&str> {
        if self.attention_suppressed() {
            return None;
        }
        let visible = match &self.screen {
            Screen::Projects => self.attention.project.is_none(),
            Screen::Overview { config, .. } => {
                self.attention.project.as_ref() == Some(&config.project_id)
            }
            _ => false,
        };
        visible
            .then_some(self.attention.notice.as_deref())
            .flatten()
    }
}

fn attention_notice(result: Result<LocalAttentionSummary, String>) -> Option<String> {
    match result {
        Err(_) => Some(unavailable_notice().into()),
        Ok(summary) if summary.database_missing => {
            Some("本地历史数据库不存在，历史执行情况未知；不会自动连接远端。".into())
        }
        Ok(summary) if !summary.candidates.is_empty() => {
            let qualifier = if summary.more { "至少 " } else { "" };
            Some(format!(
                "发现 {qualifier}{} 条待核实记录（包括未完成意图）；状态尚待确认，不会自动连接远端。",
                summary.candidates.len(),
            ))
        }
        Ok(_) => None,
    }
}

fn unavailable_notice() -> &'static str {
    "本地历史读取失败，待核实记录情况未知；请检查历史数据库，未自动连接远端。"
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::Path,
        sync::{
            Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use async_trait::async_trait;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::{
        application::{DeploymentPlan, DeploymentReport, DeploymentSelection},
        config::{ProjectConfig, ProjectConfigState},
        domain::{ComponentName, DeploymentId, DeploymentState, EnvironmentId},
        drivers::EventSink,
        history::{DeploymentKind, DeploymentRecord, HistoryStore},
        telemetry::log_record::{LogEvent, LogEventKind},
        tui::live_progress::LiveProgress,
    };

    use super::*;

    #[derive(Debug)]
    struct NoRemoteGateway;

    #[async_trait(?Send)]
    impl super::super::TuiDeploymentGateway for NoRemoteGateway {
        async fn plan(
            &self,
            _: DeploymentSelection,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<DeploymentPlan, String> {
            panic!("local attention must not call deployment planning");
        }
        async fn execute(
            &self,
            _: DeploymentPlan,
            _: &dyn EventSink,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<DeploymentReport, String> {
            panic!("local attention must not execute a deployment");
        }
    }

    #[derive(Debug)]
    struct FakeAttentionGateway {
        results: Mutex<VecDeque<Result<LocalAttentionSummary, String>>>,
        queries: Mutex<Vec<Option<ProjectId>>>,
        gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    }

    impl FakeAttentionGateway {
        fn new(results: impl IntoIterator<Item = Result<LocalAttentionSummary, String>>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
                queries: Mutex::new(Vec::new()),
                gate: None,
            }
        }
    }

    impl TuiAttentionGateway for FakeAttentionGateway {
        fn query(
            &self,
            project: Option<&ProjectId>,
            _: &CancellationToken,
        ) -> Result<LocalAttentionSummary, String> {
            self.queries.lock().unwrap().push(project.cloned());
            if let Some(gate) = &self.gate {
                let (open, changed) = &**gate;
                let (open, timeout) = changed
                    .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(3), |open| !*open)
                    .unwrap();
                assert!(
                    *open && !timeout.timed_out(),
                    "test must release attention worker"
                );
            }
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected local query")
        }
    }

    #[derive(Debug, Default)]
    struct ControlledShutdownAttentionGateway {
        started: AtomicBool,
        cancelled: AtomicBool,
        release: Mutex<bool>,
        changed: Condvar,
    }

    impl TuiAttentionGateway for ControlledShutdownAttentionGateway {
        fn query(
            &self,
            _: Option<&ProjectId>,
            cancellation: &CancellationToken,
        ) -> Result<LocalAttentionSummary, String> {
            self.started.store(true, Ordering::SeqCst);
            while !cancellation.is_cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            self.cancelled.store(true, Ordering::SeqCst);
            let (released, timeout) = self
                .changed
                .wait_timeout_while(
                    self.release.lock().unwrap(),
                    Duration::from_secs(3),
                    |released| !*released,
                )
                .unwrap();
            assert!(
                *released && !timeout.timed_out(),
                "test must release the cancelled attention worker"
            );
            Ok(LocalAttentionSummary {
                database_missing: false,
                more: false,
                candidates: Vec::new(),
            })
        }
    }

    fn new_app(root: &Path, gateway: Arc<dyn TuiAttentionGateway>) -> App {
        App::new_with_services(
            root.join("local/projects.yaml"),
            root.join("local/destinations.yaml"),
            root,
            crate::bootstrap::destination_setup_service(),
            Arc::new(NoRemoteGateway),
            gateway,
        )
        .unwrap()
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "attention worker did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_attention(app: &mut App) {
        wait_until(|| {
            app.poll_background();
            app.attention.in_flight.is_none() && app.attention.request.is_none()
        });
    }

    fn summary(count: usize) -> LocalAttentionSummary {
        LocalAttentionSummary {
            database_missing: false,
            more: false,
            candidates: (0..count)
                .map(|_| DeploymentRecord {
                    deployment: DeploymentId::new(),
                    project: ProjectId::new(),
                    environment: EnvironmentId::new(),
                    state: DeploymentState::Running,
                    kind: DeploymentKind::Deploy,
                    related_deployment: None,
                    created_at_ms: 1,
                    updated_at_ms: 2,
                    pending_intent_count: 0,
                })
                .collect(),
        }
    }

    fn config(root: &Path) -> ProjectConfig {
        std::fs::write(
            root.join("shipforge.yaml"),
            include_str!("../../../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let ProjectConfigState::Loaded(config) = crate::config::load(root).unwrap() else {
            panic!("valid fixture")
        };
        config
    }

    #[test]
    fn startup_missing_history_is_unknown_and_does_not_create_local_directories() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(LocalAttentionGateway {
            history: directory.path().join("local/history.sqlite3"),
        });
        let mut app = new_app(directory.path(), gateway);
        wait_attention(&mut app);
        assert!(matches!(app.screen, Screen::Projects));
        assert!(app.attention_notice().unwrap().contains("数据库不存在"));
        assert!(!directory.path().join("local").exists());
        assert!(app.message.is_none());
    }

    #[test]
    fn failed_local_read_is_unknown_not_empty_and_does_not_echo_untrusted_error() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(FakeAttentionGateway::new([Err(
            "SECRET\nraw database failure".into(),
        )]));
        let mut app = new_app(directory.path(), gateway);
        wait_attention(&mut app);
        let notice = app.attention_notice().unwrap();
        assert!(notice.contains("读取失败"));
        assert!(notice.contains("未知"));
        assert!(!notice.contains("SECRET"));
        assert!(!notice.contains("数据库不存在"));
    }

    #[test]
    fn bounded_attention_notice_survives_ordinary_keys_without_claiming_interruption() {
        let directory = tempfile::tempdir().unwrap();
        let mut data = summary(100);
        data.more = true;
        let gateway = Arc::new(FakeAttentionGateway::new([Ok(data)]));
        let mut app = new_app(directory.path(), gateway);
        wait_attention(&mut app);
        let notice = app.attention_notice().unwrap().to_owned();
        assert!(notice.contains("至少 100 条待核实记录"));
        assert!(!notice.contains("已中断"));
        assert!(notice.len() < 512);
        app.message = Some("temporary error".into());
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(app.message.is_none());
        assert_eq!(app.attention_notice(), Some(notice.as_str()));
    }

    #[test]
    fn shutdown_cancels_and_joins_attention_without_starting_a_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(ControlledShutdownAttentionGateway::default());
        let app = new_app(directory.path(), gateway.clone());
        wait_until(|| gateway.started.load(Ordering::SeqCst));

        let (returned_tx, returned_rx) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            let mut app = app;
            app.shutdown();
            returned_tx.send(()).unwrap();
            app
        });
        wait_until(|| gateway.cancelled.load(Ordering::SeqCst));
        assert!(matches!(
            returned_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        *gateway.release.lock().unwrap() = true;
        gateway.changed.notify_all();
        returned_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shutdown must wait until the attention worker exits");
        let app = shutdown.join().unwrap();
        assert!(!app.attention_busy());
        assert!(app.attention.request.is_none());
        assert!(app.exit_ready());
    }

    #[test]
    fn rapid_project_changes_keep_one_worker_and_ignore_stale_requests() {
        let directory = tempfile::tempdir().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut gateway = FakeAttentionGateway::new([Ok(summary(99)), Ok(summary(0))]);
        gateway.gate = Some(Arc::clone(&gate));
        let gateway = Arc::new(gateway);
        let mut app = new_app(directory.path(), gateway.clone());
        wait_until(|| gateway.queries.lock().unwrap().len() == 1);
        let mut project = config(directory.path());
        for _ in 0..20 {
            project.project_id = ProjectId::new();
            app.show_overview(directory.path().to_owned(), project.clone());
        }
        assert_eq!(gateway.queries.lock().unwrap().len(), 1);
        let current = app.attention.request.clone();
        app.finish_attention(
            &AttentionRequest {
                id: uuid::Uuid::now_v7(),
                project: Some(ProjectId::new()),
            },
            Ok(summary(77)),
        );
        assert_eq!(app.attention.request, current);
        assert!(app.attention_notice().is_none());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        wait_attention(&mut app);
        assert_eq!(
            *gateway.queries.lock().unwrap(),
            vec![None, Some(project.project_id)]
        );
        assert!(app.attention_notice().is_none());
    }

    #[test]
    fn late_local_query_does_not_replace_or_classify_a_running_deployment() {
        let directory = tempfile::tempdir().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut gateway = FakeAttentionGateway::new([Ok(summary(1))]);
        gateway.gate = Some(Arc::clone(&gate));
        let gateway = Arc::new(gateway);
        let mut app = new_app(directory.path(), gateway.clone());
        wait_until(|| gateway.queries.lock().unwrap().len() == 1);
        app.screen = Screen::DeploymentRunning {
            root: directory.path().to_owned(),
            config: config(directory.path()),
            cancellation: tokio_util::sync::CancellationToken::new(),
            cancellation_requested: false,
        };
        app.invalidate_attention();
        app.refresh_attention(None);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        wait_attention(&mut app);
        assert!(matches!(app.screen, Screen::DeploymentRunning { .. }));
        assert!(app.attention_notice().is_none());
        assert_eq!(gateway.queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn session_permit_alone_suppresses_detection_before_running_screen() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(FakeAttentionGateway::new([Ok(summary(0))]));
        let mut app = new_app(directory.path(), gateway.clone());
        wait_attention(&mut app);
        let session = Arc::clone(&app.deployment_session);
        session
            .run(async {
                app.refresh_attention(None);
            })
            .await
            .unwrap();
        assert!(app.attention.in_flight.is_none());
        assert_eq!(gateway.queries.lock().unwrap().len(), 1);
    }

    #[test]
    fn deployment_completion_refreshes_attention_and_preserves_live_logs() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(FakeAttentionGateway::new([Ok(summary(0)), Ok(summary(1))]));
        let mut app = new_app(directory.path(), gateway.clone());
        wait_attention(&mut app);
        let project = config(directory.path());
        let request_id = uuid::Uuid::now_v7();
        let cancellation = tokio_util::sync::CancellationToken::new();
        app.screen = Screen::DeploymentRunning {
            root: directory.path().to_owned(),
            config: project.clone(),
            cancellation: cancellation.clone(),
            cancellation_requested: false,
        };
        let progress = LiveProgress::default();
        progress.record(LogEvent {
            namespace: "deploy.stdout".into(),
            message: "running log sentinel".into(),
            scope: None,
            kind: LogEventKind::Output,
        });
        app.live_progress = Some(progress.clone());
        assert!(app.poll_live_logs(), "running page must drain live logs");
        progress.record(LogEvent {
            namespace: "deploy.stdout".into(),
            message: "completion log sentinel".into(),
            scope: None,
            kind: LogEventKind::Output,
        });
        app.deployment_execution_task = Some(super::super::DeploymentTask {
            id: request_id,
            cancellation,
            worker: Some(std::thread::spawn(|| {})),
        });
        app.finish_deployment(request_id, Err("execution failed".into()));
        wait_attention(&mut app);
        assert!(matches!(app.screen, Screen::DeploymentFinished { .. }));
        let retained = app
            .live_logs
            .matching()
            .into_iter()
            .map(|row| row.event.message.as_str())
            .collect::<Vec<_>>();
        assert!(retained.contains(&"running log sentinel"));
        assert!(retained.contains(&"completion log sentinel"));
        app.open_live_logs();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30))
            .expect("log overlay terminal");
        terminal
            .draw(|frame| crate::tui::render(frame, &app))
            .expect("render completed deployment log overlay");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("running log sentinel"));
        assert!(rendered.contains("completion log sentinel"));
        assert_eq!(
            *gateway.queries.lock().unwrap(),
            vec![None, Some(project.project_id)]
        );
        assert!(
            app.attention
                .notice
                .as_ref()
                .unwrap()
                .contains("1 条待核实记录")
        );
    }

    #[test]
    fn valid_project_open_filters_terminal_pending_and_never_changes_history() {
        let directory = tempfile::tempdir().unwrap();
        let project = config(directory.path());
        let environment = &project.environments.values().next().unwrap().id;
        let path = directory.path().join("local/history.sqlite3");
        let history = HistoryStore::open(&path).unwrap();
        let pending = DeploymentId::new();
        history
            .create_deployment(&pending, &project.project_id, environment, 1)
            .unwrap();
        history
            .transition_deployment(
                &pending,
                DeploymentState::Created,
                DeploymentState::Running,
                2,
            )
            .unwrap();
        history
            .record_intent(
                &pending,
                &ComponentName::parse("api").unwrap(),
                "activate",
                "v1",
                3,
            )
            .unwrap();
        history
            .transition_deployment(
                &pending,
                DeploymentState::Running,
                DeploymentState::Failed,
                4,
            )
            .unwrap();
        let running = DeploymentId::new();
        history
            .create_deployment(&running, &project.project_id, environment, 5)
            .unwrap();
        history
            .transition_deployment(
                &running,
                DeploymentState::Created,
                DeploymentState::Running,
                6,
            )
            .unwrap();
        history
            .create_deployment(&DeploymentId::new(), &ProjectId::new(), environment, 7)
            .unwrap();
        let before_pending = history.deployment(&pending).unwrap();
        let before_running = history.deployment(&running).unwrap();
        let gateway = Arc::new(LocalAttentionGateway { history: path });
        let mut app = new_app(directory.path(), gateway);
        wait_attention(&mut app);
        assert!(app.attention_notice().unwrap().contains("3 条待核实记录"));
        app.select_root(directory.path());
        wait_attention(&mut app);
        assert!(matches!(app.screen, Screen::Overview { .. }));
        assert!(app.attention_notice().unwrap().contains("2 条待核实记录"));
        assert_eq!(history.deployment(&pending).unwrap(), before_pending);
        assert_eq!(history.deployment(&running).unwrap(), before_running);
        assert_eq!(history.pending_intents(&pending).unwrap().len(), 1);
    }

    #[test]
    fn missing_or_invalid_project_yaml_does_not_query_a_cached_project_identity() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(FakeAttentionGateway::new([Ok(summary(0))]));
        let mut app = new_app(directory.path(), gateway.clone());
        wait_attention(&mut app);
        app.select_root(directory.path());
        assert!(matches!(app.screen, Screen::SetupComponents(_)));
        std::fs::write(directory.path().join("shipforge.yaml"), "invalid: yaml").unwrap();
        app.select_root(directory.path());
        assert!(matches!(app.screen, Screen::Reinitialize(_)));
        assert!(app.reinitialize_task.is_none());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("shipforge.yaml")).unwrap(),
            "invalid: yaml"
        );
        assert_eq!(*gateway.queries.lock().unwrap(), vec![None]);
        assert!(app.attention.request.is_none());
    }
}
