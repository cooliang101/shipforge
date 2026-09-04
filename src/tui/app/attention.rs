use std::{path::PathBuf, sync::Arc};

use crate::{application::LocalAttentionSummary, domain::ProjectId};

use super::{App, BackgroundEvent, Screen, catch_worker_failure};

pub(super) trait TuiAttentionGateway: std::fmt::Debug + Send + Sync {
    fn query(&self, project: Option<&ProjectId>) -> Result<LocalAttentionSummary, String>;
}

#[derive(Debug)]
pub(super) struct LocalAttentionGateway {
    pub history: PathBuf,
}

impl TuiAttentionGateway for LocalAttentionGateway {
    fn query(&self, project: Option<&ProjectId>) -> Result<LocalAttentionSummary, String> {
        crate::application::local_attention(&self.history, project, None)
            .map_err(|error| error.to_string())
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
    }

    fn attention_suppressed(&self) -> bool {
        self.deployment_session.is_active()
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
        let gateway = Arc::clone(&self.attention_gateway);
        let sender = self.background_sender.clone();
        let spawned = std::thread::Builder::new()
            .name("shipforge-local-attention".into())
            .spawn(move || {
                let result = catch_worker_failure(|| gateway.query(request.project.as_ref()));
                let _ = sender.send(BackgroundEvent::LocalAttention(request, result));
            });
        if spawned.is_ok() {
            self.attention.in_flight = Some(id);
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
        self.attention.in_flight = None;
        if self.attention.request.as_ref() == Some(request) {
            self.attention.request = None;
            if !self.attention_suppressed() {
                self.attention.notice = attention_notice(result);
            }
        }
        self.start_attention_worker();
    }

    pub(super) fn show_projects(&mut self) {
        self.screen = Screen::Projects;
        self.refresh_attention(None);
    }

    pub(super) fn show_overview(&mut self, root: PathBuf, config: crate::config::ProjectConfig) {
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
        sync::{Condvar, Mutex},
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
        fn query(&self, project: Option<&ProjectId>) -> Result<LocalAttentionSummary, String> {
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
            logs: VecDeque::new(),
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
    fn deployment_completion_refreshes_project_attention_and_keeps_result_screen() {
        let directory = tempfile::tempdir().unwrap();
        let gateway = Arc::new(FakeAttentionGateway::new([Ok(summary(0)), Ok(summary(1))]));
        let mut app = new_app(directory.path(), gateway.clone());
        wait_attention(&mut app);
        let project = config(directory.path());
        app.screen = Screen::DeploymentRunning {
            root: directory.path().to_owned(),
            config: project.clone(),
            cancellation: tokio_util::sync::CancellationToken::new(),
            cancellation_requested: false,
            logs: VecDeque::new(),
        };
        app.finish_deployment(Err("execution failed".into()));
        wait_attention(&mut app);
        assert!(matches!(app.screen, Screen::DeploymentFinished { .. }));
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
        assert!(app.message.is_some());
        assert_eq!(*gateway.queries.lock().unwrap(), vec![None]);
        assert!(app.attention.request.is_none());
    }
}
