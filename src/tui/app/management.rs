//! Management navigation is separate from deployment setup and live progress.
//! A single tracked worker owns each request until completion, including cancellation.

mod evidence;
mod gateway;
mod navigation;
#[cfg(test)]
mod navigation_tests;
mod outcome;
mod render;
#[cfg(test)]
mod retired_tests;
#[cfg(test)]
mod rollback_tests;
mod search;
#[cfg(test)]
mod tests;
mod viewport;

use std::{collections::BTreeMap, sync::Arc, thread::JoinHandle, time::Instant};

use tokio_util::sync::CancellationToken;

use crate::{
    application::{
        DeploymentSelection, RollbackCandidates, RollbackPlan, RollbackReport,
        history_query::{DeploymentDetails, HistoryPage},
    },
    domain::{ComponentName, DeploymentId, EnvironmentId},
    drivers::ReleaseRef,
    history::{DeploymentQuery, DeploymentRecord, RecoveryQuery, RecoveryReport},
};

use super::{App, BTreeSet, BackgroundEvent, KeyCode, PathBuf, ProjectConfig, Screen};

pub(super) use gateway::{LocalManagementGateway, ManagementGateway};

const PAGE_SIZE: u32 = 20;
const HISTORICAL_READ_ONLY: &str = "Historical Environment browsing is read-only and local-only; return to a current Environment before remote operations.";
const MISSING_FROZEN_CONTEXT: &str = "Remote actions are unavailable: this record has no frozen Component context. Local history and logs remain readable.";

#[derive(Clone, Debug)]
pub(in crate::tui) struct ManagementScreen {
    scope: Arc<ManagementScope>,
    page: ManagementPage,
    view: viewport::Viewport,
    back: Vec<navigation::ManagementReturn>,
    notice: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ManagementScope {
    root: PathBuf,
    config: ProjectConfig,
    environment: String,
    historical_environment: Option<EnvironmentId>,
}

impl ManagementScope {
    fn environment_id(&self) -> Option<&EnvironmentId> {
        self.historical_environment.as_ref().or_else(|| {
            self.config
                .environments
                .get(&self.environment)
                .map(|environment| &environment.id)
        })
    }

    fn current_name_for(&self, id: &EnvironmentId) -> Option<&str> {
        self.config
            .environments
            .iter()
            .find(|(_, environment)| &environment.id == id)
            .map(|(name, _)| name.as_str())
    }

    fn selection(&self, components: BTreeSet<ComponentName>) -> DeploymentSelection {
        DeploymentSelection {
            project_root: self.root.clone(),
            config: self.config.clone(),
            environment: self.environment.clone(),
            components,
        }
    }

    fn components(&self) -> Vec<ComponentName> {
        self.config
            .environments
            .get(&self.environment)
            .map(|environment| environment.components.keys().cloned().collect())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub(super) enum ManagementPage {
    Home,
    Environments {
        page: Arc<HistoryPage<EnvironmentId>>,
        offset: u32,
        cursor: usize,
    },
    Loading {
        label: &'static str,
        started: Instant,
        cancelling: bool,
    },
    History {
        page: Arc<HistoryPage<DeploymentRecord>>,
        offset: u32,
        cursor: usize,
    },
    Detail(Arc<DeploymentDetails>),
    Reports {
        page: Arc<HistoryPage<RecoveryReport>>,
        offset: u32,
        cursor: usize,
    },
    Report {
        report: Arc<RecoveryReport>,
        warning: Option<String>,
    },
    InspectSelection {
        source: Option<DeploymentId>,
        historical_releases: Vec<ReleaseRef>,
        names: Vec<ComponentName>,
        selected: BTreeSet<ComponentName>,
        cursor: usize,
    },
    RollbackSelection {
        details: Arc<DeploymentDetails>,
        selected: BTreeSet<ComponentName>,
        cursor: usize,
    },
    RollbackTargets {
        candidates: Arc<RollbackCandidates>,
        selected: BTreeSet<ComponentName>,
        options: BTreeMap<ComponentName, usize>,
        cursor: usize,
    },
    RollbackTargetDetail {
        candidates: Arc<RollbackCandidates>,
        cursor: usize,
        option: usize,
    },
    Failed {
        label: &'static str,
        message: String,
        retry: Option<ManagementRequest>,
    },
    RollbackReview(Arc<RollbackPlan>),
    RollbackFinished(Arc<RollbackReport>),
    RollbackFailed {
        request_id: uuid::Uuid,
        message: String,
        progress: crate::tui::live_progress::LiveProgress,
    },
}

#[derive(Clone, Debug)]
pub(super) enum ManagementRequest {
    Environments(RecoveryQuery),
    History(DeploymentQuery),
    Detail(DeploymentId),
    Reports(RecoveryQuery),
    Report(uuid::Uuid),
    Inspect {
        source: Option<DeploymentId>,
        selected: BTreeSet<ComponentName>,
    },
    Candidates {
        source: DeploymentId,
        selected: BTreeSet<ComponentName>,
    },
    Plan {
        source: DeploymentId,
        targets: BTreeMap<ComponentName, Option<ReleaseRef>>,
    },
    Execute(Arc<RollbackPlan>),
}

impl ManagementRequest {
    const fn is_remote(&self) -> bool {
        matches!(
            self,
            Self::Inspect { .. } | Self::Candidates { .. } | Self::Plan { .. } | Self::Execute(_)
        )
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::Environments(_) => "Reading historical Environment IDs (local only)",
            Self::History(_) => "Reading deployment history (local only)",
            Self::Detail(_) => "Reading deployment details (local only)",
            Self::Reports(_) => "Reading saved inspection reports (local only)",
            Self::Report(_) => "Reading an inspection report (local only)",
            Self::Inspect { .. } => "Inspecting remote state (read-only)",
            Self::Candidates { .. } => "Checking local rollback evidence",
            Self::Plan { .. } => "Checking rollback targets (read-only)",
            Self::Execute(_) => "Executing rollback; safe cancellation may require compensation",
        }
    }
}

#[derive(Debug)]
pub(super) struct ManagementTask {
    skip_home_on_success: bool,
    id: uuid::Uuid,
    origin: ManagementScreen,
    cancellation: CancellationToken,
    worker: Option<JoinHandle<()>>,
    execution_progress: Option<crate::tui::live_progress::LiveProgress>,
    request: ManagementRequest,
}

impl ManagementScreen {
    pub(super) fn requires_plain_confirmation(&self) -> bool {
        matches!(self.page, ManagementPage::RollbackReview(_))
    }
}

impl App {
    pub(super) fn management_has_live_progress(&self) -> bool {
        let Some(live) = &self.live_progress else {
            return false;
        };
        let Screen::Management(screen) = &self.screen else {
            return false;
        };
        if !self
            .live_environment
            .as_ref()
            .is_some_and(|(project, environment)| {
                project == &screen.scope.config.project_id
                    && screen.scope.environment_id() == Some(environment)
            })
        {
            return false;
        }
        self.management_task
            .as_ref()
            .and_then(|task| task.execution_progress.as_ref())
            .is_some_and(|progress| live.same_operation(progress))
            || match &screen.page {
                ManagementPage::RollbackFinished(report) => {
                    live.snapshot().deployment.as_ref() == Some(&report.deployment.id)
                }
                ManagementPage::RollbackFailed { progress, .. } => live.same_operation(progress),
                _ => false,
            }
    }

    pub(super) fn open_management(&mut self, root: PathBuf, config: ProjectConfig) {
        self.initialize_management(root, config);
        if let Screen::Management(screen) = &self.screen {
            self.start_management(
                screen.clone(),
                ManagementRequest::History(DeploymentQuery {
                    limit: PAGE_SIZE,
                    ..Default::default()
                }),
            );
            if let Some(task) = &mut self.management_task {
                task.skip_home_on_success = true;
            }
        }
    }

    fn initialize_management(&mut self, root: PathBuf, config: ProjectConfig) {
        self.refresh_destination_labels();
        let Some(environment) = self.preferred_environment(&config) else {
            self.message = Some("This Project has no Environment to manage.".into());
            return;
        };
        self.remember_environment(&config, &environment);
        self.screen = Screen::Management(ManagementScreen {
            scope: Arc::new(ManagementScope {
                root,
                config,
                environment,
                historical_environment: None,
            }),
            page: ManagementPage::Home,
            view: viewport::Viewport::default(),
            back: Vec::new(),
            notice: None,
        });
    }

    pub(super) fn handle_management(&mut self, key: KeyCode, mut screen: ManagementScreen) {
        if self.management_task.is_some() {
            if key == KeyCode::Esc {
                self.cancel_management();
            }
            return;
        }
        if screen.scope.historical_environment.is_some() && matches!(key, KeyCode::Char('i' | 'r'))
        {
            self.message = Some(HISTORICAL_READ_ONLY.into());
            self.screen = Screen::Management(screen);
            return;
        }
        if matches!(
            screen.page,
            ManagementPage::History { .. } | ManagementPage::Reports { .. }
        ) && matches!(key, KeyCode::Char('h' | 'p' | 'a' | 'i'))
        {
            self.handle_management_home(key, screen);
            return;
        }
        if self.handle_management_shortcut(key, &mut screen) {
            return;
        }
        if matches!(screen.page, ManagementPage::Environments { .. }) {
            self.handle_management_environments(key, screen);
            return;
        }
        if let ManagementPage::Detail(details) = &screen.page {
            let details = Arc::clone(details);
            self.handle_management_details(key, screen, &details);
            return;
        }
        let request = match &mut screen.page {
            ManagementPage::Home => return self.handle_management_home(key, screen),
            ManagementPage::History {
                page,
                offset,
                cursor,
            } => history_key(key, page, *offset, cursor),
            ManagementPage::Reports {
                page,
                offset,
                cursor,
            } => report_key(key, page, *offset, cursor),
            ManagementPage::InspectSelection {
                source,
                names,
                selected,
                cursor,
                ..
            } => {
                selection_key(key, names, selected, cursor);
                (key == KeyCode::Enter && !selected.is_empty()).then(|| {
                    ManagementRequest::Inspect {
                        source: source.clone(),
                        selected: selected.clone(),
                    }
                })
            }
            ManagementPage::RollbackSelection {
                details,
                selected,
                cursor,
            } => {
                let names: Vec<_> = details
                    .snapshots
                    .iter()
                    .map(|snapshot| snapshot.release.component.clone())
                    .collect();
                selection_key(key, &names, selected, cursor);
                (key == KeyCode::Enter && !selected.is_empty()).then(|| {
                    ManagementRequest::Candidates {
                        source: details.record.deployment.clone(),
                        selected: selected.clone(),
                    }
                })
            }
            ManagementPage::RollbackTargets {
                candidates,
                selected,
                options,
                cursor,
            } => rollback_key(key, candidates, selected, options, cursor),
            ManagementPage::RollbackReview(plan) if key == KeyCode::Char('c') => {
                Some(ManagementRequest::Execute(Arc::clone(plan)))
            }
            _ => None,
        };
        if let Some(request) = request {
            self.start_management(screen, request);
            return;
        }
        self.navigate_management(key, screen);
    }

    fn handle_management_shortcut(&mut self, key: KeyCode, screen: &mut ManagementScreen) -> bool {
        if key == KeyCode::Char('f')
            && let ManagementPage::Failed {
                retry: Some(request),
                ..
            } = &screen.page
        {
            let request = request.clone();
            screen.go_back();
            self.start_management(screen.clone(), request);
            return true;
        }
        if key == KeyCode::Char('d')
            && let ManagementPage::RollbackTargets {
                candidates,
                options,
                cursor,
                ..
            } = &screen.page
            && let Some(component) = candidates.components.get(*cursor)
        {
            let detail = ManagementPage::RollbackTargetDetail {
                candidates: Arc::clone(candidates),
                cursor: *cursor,
                option: *options.get(&component.component).unwrap_or(&0),
            };
            screen.push_page(detail);
            self.screen = Screen::Management(screen.clone());
            return true;
        }
        false
    }

    fn handle_management_environments(&mut self, key: KeyCode, mut screen: ManagementScreen) {
        let ManagementPage::Environments {
            page,
            offset,
            cursor,
        } = &mut screen.page
        else {
            return;
        };
        move_cursor(key, cursor, page.items.len());
        if key == KeyCode::Enter
            && let Some(environment) = page.items.get(*cursor)
        {
            let environment = environment.clone();
            screen.remember_page();
            Arc::make_mut(&mut screen.scope).historical_environment = Some(environment);
            screen.page = ManagementPage::Home;
            screen.view = viewport::Viewport::default();
            screen.notice = None;
            self.screen = Screen::Management(screen);
            return;
        }
        let offset = match key {
            KeyCode::Char('n') if page.more => Some(offset.saturating_add(PAGE_SIZE)),
            KeyCode::Char('b') if *offset > 0 => Some(offset.saturating_sub(PAGE_SIZE)),
            KeyCode::Char('f') => Some(*offset),
            _ => None,
        };
        if let Some(offset) = offset {
            self.start_management(
                screen,
                ManagementRequest::Environments(RecoveryQuery {
                    limit: PAGE_SIZE,
                    offset,
                }),
            );
        } else {
            self.navigate_management(key, screen);
        }
    }

    fn handle_management_details(
        &mut self,
        key: KeyCode,
        mut screen: ManagementScreen,
        details: &Arc<DeploymentDetails>,
    ) {
        if key == KeyCode::Char('l') {
            let scope = crate::application::history_query::HistoryLogScope {
                project: details.record.project.clone(),
                environment: details.record.environment.clone(),
                deployment: details.record.deployment.clone(),
            };
            self.open_history_logs(scope, screen.scope.root.clone());
            return;
        }
        if details.snapshots.is_empty() && matches!(key, KeyCode::Char('i' | 'r')) {
            self.message = Some(MISSING_FROZEN_CONTEXT.into());
            self.screen = Screen::Management(screen);
            return;
        }
        let request = match key {
            KeyCode::Char('r') => {
                screen.push_page(ManagementPage::RollbackSelection {
                    details: Arc::clone(details),
                    selected: BTreeSet::new(),
                    cursor: 0,
                });
                self.screen = Screen::Management(screen);
                return;
            }
            KeyCode::Char('i') => {
                screen.push_page(ManagementPage::InspectSelection {
                    source: Some(details.record.deployment.clone()),
                    historical_releases: details
                        .snapshots
                        .iter()
                        .map(|snapshot| snapshot.release.clone())
                        .collect(),
                    names: details
                        .snapshots
                        .iter()
                        .map(|snapshot| snapshot.release.component.clone())
                        .collect(),
                    selected: details
                        .snapshots
                        .iter()
                        .map(|snapshot| snapshot.release.component.clone())
                        .collect(),
                    cursor: 0,
                });
                self.screen = Screen::Management(screen);
                return;
            }
            _ => None,
        };
        if let Some(request) = request {
            self.start_management(screen, request);
        } else {
            self.navigate_management(key, screen);
        }
    }

    fn navigate_management(&mut self, key: KeyCode, mut screen: ManagementScreen) {
        if key == KeyCode::Esc {
            if screen.back.is_empty() && matches!(screen.page, ManagementPage::History { .. }) {
                self.show_overview(screen.scope.root.clone(), screen.scope.config.clone());
                return;
            }
            screen.go_back();
        } else {
            screen.view.handle_key(key);
        }
        self.screen = Screen::Management(screen);
    }

    fn handle_management_home(&mut self, key: KeyCode, mut screen: ManagementScreen) {
        let request = match key {
            KeyCode::Char('a') if screen.scope.historical_environment.is_none() => {
                Some(ManagementRequest::Environments(RecoveryQuery {
                    limit: PAGE_SIZE,
                    offset: 0,
                }))
            }
            KeyCode::Char('h') => Some(ManagementRequest::History(DeploymentQuery {
                limit: PAGE_SIZE,
                ..Default::default()
            })),
            KeyCode::Char('p') => Some(ManagementRequest::Reports(RecoveryQuery {
                limit: PAGE_SIZE,
                offset: 0,
            })),
            KeyCode::Char('i') if screen.scope.historical_environment.is_none() => {
                screen.push_page(ManagementPage::InspectSelection {
                    source: None,
                    historical_releases: Vec::new(),
                    names: screen.scope.components(),
                    selected: screen.scope.components().into_iter().collect(),
                    cursor: 0,
                });
                None
            }
            KeyCode::Left | KeyCode::Right if screen.scope.historical_environment.is_none() => {
                let names: Vec<_> = screen.scope.config.environments.keys().cloned().collect();
                let index = names
                    .iter()
                    .position(|name| name == &screen.scope.environment)
                    .unwrap_or(0);
                let next = if key == KeyCode::Left {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(names.len() - 1)
                };
                Arc::make_mut(&mut screen.scope)
                    .environment
                    .clone_from(&names[next]);
                self.remember_environment(&screen.scope.config, &screen.scope.environment);
                screen.view = viewport::Viewport::default();
                screen.notice = None;
                None
            }
            KeyCode::Esc if !screen.back.is_empty() => {
                screen.go_back();
                None
            }
            KeyCode::Esc if screen.scope.historical_environment.is_some() => {
                Arc::make_mut(&mut screen.scope).historical_environment = None;
                screen.view = viewport::Viewport::default();
                None
            }
            KeyCode::Esc => {
                self.show_overview(screen.scope.root.clone(), screen.scope.config.clone());
                return;
            }
            _ => None,
        };
        if let Some(request) = request {
            self.start_management(screen, request);
        } else {
            self.screen = Screen::Management(screen);
        }
    }

    fn start_management(&mut self, origin: ManagementScreen, request: ManagementRequest) {
        if origin.scope.historical_environment.is_some() && request.is_remote() {
            self.message = Some(HISTORICAL_READ_ONLY.into());
            return;
        }
        if self.management_task.is_some()
            || self.connections_task.is_some()
            || self.project_edit_task.is_some()
            || self.deployment_session.is_active()
        {
            self.message = Some("Another operation is still active; wait for its result.".into());
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.management_start_failed(origin, request, "Background runtime is unavailable.");
            return;
        };
        let id = uuid::Uuid::now_v7();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let gateway = Arc::clone(&self.management_gateway);
        let sender = self.background_sender.clone();
        let scope = Arc::clone(&origin.scope);
        let label = request.label();
        let execution_progress = matches!(request, ManagementRequest::Execute(_))
            .then(crate::tui::live_progress::LiveProgress::default);
        let worker_progress = execution_progress.clone();
        let worker_request = request.clone();
        let spawn = std::thread::Builder::new()
            .name("shipforge-management".into())
            .spawn(move || {
                let result = super::catch_worker_failure(|| {
                    runtime.block_on(async {
                        if let Some(progress) = worker_progress.clone() {
                            let events = super::ProgressEvents { progress };
                            gateway
                                .run_with_events(&scope, worker_request, &events, &worker_cancel)
                                .await
                        } else {
                            gateway.run(&scope, worker_request, &worker_cancel).await
                        }
                    })
                });
                if let Some(progress) = &worker_progress {
                    progress.finish();
                }
                let _ = sender.send(BackgroundEvent::Management(id, result));
            });
        match spawn {
            Ok(worker) => {
                if let Some(progress) = &execution_progress {
                    self.live_environment = origin.scope.environment_id().map(|environment| {
                        (origin.scope.config.project_id.clone(), environment.clone())
                    });
                    self.live_progress = Some(progress.clone());
                    self.live_logs = crate::tui::log_view::LogView::default();
                }
                self.invalidate_attention();
                self.screen = Screen::Management(ManagementScreen {
                    scope: Arc::clone(&origin.scope),
                    page: ManagementPage::Loading {
                        label,
                        started: Instant::now(),
                        cancelling: false,
                    },
                    view: viewport::Viewport::default(),
                    back: Vec::new(),
                    notice: None,
                });
                self.management_task = Some(ManagementTask {
                    skip_home_on_success: false,
                    id,
                    origin,
                    cancellation,
                    worker: Some(worker),
                    execution_progress,
                    request,
                });
            }
            Err(_) => {
                self.management_start_failed(
                    origin,
                    request,
                    "Could not start management worker; nothing was started.",
                );
            }
        }
    }

    pub(super) fn cancel_management(&mut self) {
        if let Some(task) = &self.management_task {
            task.cancellation.cancel();
            if let Screen::Management(ManagementScreen {
                page: ManagementPage::Loading { cancelling, .. },
                ..
            }) = &mut self.screen
            {
                *cancelling = true;
            }
        }
    }

    pub(super) fn finish_management(
        &mut self,
        id: uuid::Uuid,
        mut result: Result<ManagementPage, String>,
    ) {
        if self
            .management_task
            .as_ref()
            .is_none_or(|task| task.id != id)
        {
            return;
        }
        let Some(mut task) = self.management_task.take() else {
            return;
        };
        let worker_failed = task
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        if let Some(progress) = &task.execution_progress {
            progress.finish();
        }
        self.poll_live_logs();
        let mut screen = task.origin;
        let cancelled = task.cancellation.is_cancelled();
        let discards_cancelled_result = task.request.discards_cancelled_result();
        if cancelled && discards_cancelled_result {
            screen.notice = Some(if worker_failed {
                "Request cancelled and the worker stopped unexpectedly during cleanup. The previous page is retained; no new query result or rollback preview was accepted."
            } else {
                "Request cancelled. The previous page is retained; no new query result or rollback preview was accepted. Refresh or check again explicitly."
            }.into());
            self.screen = Screen::Management(screen);
            return;
        }
        if worker_failed && discards_cancelled_result {
            result = Err(
                "Management worker stopped unexpectedly. The previous evidence remains unchanged; retry the read or preview."
                    .into(),
            );
        }
        if matches!(task.request, ManagementRequest::Execute(_)) {
            screen.consume_rollback_navigation();
        }
        match result {
            Ok(page) => {
                screen.accept_result(&task.request, page);
                if task.skip_home_on_success {
                    screen.back.clear();
                }
            }
            Err(error) => {
                let message = render::safe_text(&error);
                self.message = Some(message.clone());
                if let Some(progress) = task.execution_progress {
                    // The task ID was checked above. Preserve this execution's
                    // projection; never substitute the source release's history.
                    screen.page = ManagementPage::RollbackFailed {
                        request_id: task.id,
                        message,
                        progress,
                    };
                    screen.view = viewport::Viewport::default();
                } else {
                    screen.show_failure(task.request, message);
                }
            }
        }
        if cancelled {
            screen.notice = Some(
                "Cancellation was requested. Available observations and execution outcomes are retained below; cancellation does not prove that nothing happened."
                    .into(),
            );
        }
        if worker_failed && !discards_cancelled_result {
            let warning = "The worker stopped unexpectedly after reporting this inspection or execution result. Known evidence is retained; inspect durable history before retrying.";
            screen.notice = Some(
                screen
                    .notice
                    .map_or_else(|| warning.into(), |notice| format!("{notice} {warning}")),
            );
        }
        self.screen = Screen::Management(screen);
    }

    fn management_start_failed(
        &mut self,
        mut origin: ManagementScreen,
        request: ManagementRequest,
        message: &str,
    ) {
        self.message = Some(message.into());
        if matches!(request, ManagementRequest::Execute(_)) {
            origin.consume_rollback_navigation();
        }
        origin.show_failure(request, message.into());
        self.screen = Screen::Management(origin);
    }
}

fn history_key(
    key: KeyCode,
    page: &HistoryPage<DeploymentRecord>,
    offset: u32,
    cursor: &mut usize,
) -> Option<ManagementRequest> {
    move_cursor(key, cursor, page.items.len());
    match key {
        KeyCode::Enter => page
            .items
            .get(*cursor)
            .map(|record| ManagementRequest::Detail(record.deployment.clone())),
        KeyCode::Char('n') if page.more => Some(ManagementRequest::History(DeploymentQuery {
            limit: PAGE_SIZE,
            offset: offset.saturating_add(PAGE_SIZE),
            nonterminal_only: false,
        })),
        KeyCode::Char('b') if offset > 0 => Some(ManagementRequest::History(DeploymentQuery {
            limit: PAGE_SIZE,
            offset: offset.saturating_sub(PAGE_SIZE),
            nonterminal_only: false,
        })),
        KeyCode::Char('f') => Some(ManagementRequest::History(DeploymentQuery {
            limit: PAGE_SIZE,
            offset,
            nonterminal_only: false,
        })),
        _ => None,
    }
}

fn report_key(
    key: KeyCode,
    page: &HistoryPage<RecoveryReport>,
    offset: u32,
    cursor: &mut usize,
) -> Option<ManagementRequest> {
    move_cursor(key, cursor, page.items.len());
    match key {
        KeyCode::Enter => page
            .items
            .get(*cursor)
            .map(|report| ManagementRequest::Report(report.id)),
        KeyCode::Char('n') if page.more => Some(ManagementRequest::Reports(RecoveryQuery {
            limit: PAGE_SIZE,
            offset: offset.saturating_add(PAGE_SIZE),
        })),
        KeyCode::Char('b') if offset > 0 => Some(ManagementRequest::Reports(RecoveryQuery {
            limit: PAGE_SIZE,
            offset: offset.saturating_sub(PAGE_SIZE),
        })),
        KeyCode::Char('f') => Some(ManagementRequest::Reports(RecoveryQuery {
            limit: PAGE_SIZE,
            offset,
        })),
        _ => None,
    }
}

fn rollback_key(
    key: KeyCode,
    candidates: &RollbackCandidates,
    selected: &mut BTreeSet<ComponentName>,
    options: &mut BTreeMap<ComponentName, usize>,
    cursor: &mut usize,
) -> Option<ManagementRequest> {
    move_cursor(key, cursor, candidates.components.len());
    if let Some(component) = candidates.components.get(*cursor) {
        let index = options.entry(component.component.clone()).or_default();
        match key {
            KeyCode::Left => *index = index.saturating_sub(1),
            KeyCode::Right => {
                *index = index
                    .saturating_add(1)
                    .min(component.options.len().saturating_sub(1));
            }
            KeyCode::Char(' ') => {
                if selected.contains(&component.component) {
                    selected.remove(&component.component);
                } else if component.unavailable.is_none()
                    && component
                        .options
                        .get(*index)
                        .is_some_and(|option| option.unavailable.is_none())
                {
                    selected.insert(component.component.clone());
                }
            }
            _ => {}
        }
        if component.unavailable.is_some()
            || component
                .options
                .get(*index)
                .is_none_or(|option| option.unavailable.is_some())
        {
            selected.remove(&component.component);
        }
    }
    if key != KeyCode::Enter || selected.is_empty() {
        return None;
    }
    let targets = candidates
        .components
        .iter()
        .filter(|component| selected.contains(&component.component))
        .filter_map(|component| {
            component
                .options
                .get(*options.get(&component.component).unwrap_or(&0))
                .map(|option| (component.component.clone(), option.target.clone()))
        })
        .collect();
    Some(ManagementRequest::Plan {
        source: candidates.source.clone(),
        targets,
    })
}

fn selection_key(
    key: KeyCode,
    names: &[ComponentName],
    selected: &mut BTreeSet<ComponentName>,
    cursor: &mut usize,
) {
    move_cursor(key, cursor, names.len());
    if key == KeyCode::Char(' ')
        && let Some(name) = names.get(*cursor)
        && !selected.remove(name)
    {
        selected.insert(name.clone());
    }
}

fn move_cursor(key: KeyCode, cursor: &mut usize, count: usize) {
    *cursor = match key {
        KeyCode::Up => cursor.saturating_sub(1),
        KeyCode::Down => cursor.saturating_add(1).min(count.saturating_sub(1)),
        KeyCode::PageUp => cursor.saturating_sub(10),
        KeyCode::PageDown => cursor.saturating_add(10).min(count.saturating_sub(1)),
        KeyCode::Home => 0,
        KeyCode::End => count.saturating_sub(1),
        _ => *cursor,
    };
}

impl ManagementScreen {
    pub(super) fn actions(&self) -> Option<super::actions::Actions> {
        use super::actions::Actions;
        use crate::tui::i18n::choose as t;
        let mut actions = match &self.page {
            ManagementPage::Home => Actions::new(
                &[
                    (t("Deployment history", "发布历史"), 'h'),
                    (t("Saved inspections", "已保存的检查报告"), 'p'),
                ],
                0,
                0,
            ),
            ManagementPage::History { page, cursor, .. } => Actions::new(
                &[
                    (t("Refresh history", "刷新历史"), 'f'),
                    (t("Saved inspections", "已保存的检查报告"), 'p'),
                ],
                page.items.len(),
                *cursor,
            ),
            ManagementPage::Reports { page, cursor, .. } => Actions::new(
                &[
                    (t("Refresh reports", "刷新检查报告"), 'f'),
                    (t("Deployment history", "发布历史"), 'h'),
                ],
                page.items.len(),
                *cursor,
            ),
            ManagementPage::Environments { page, cursor, .. } => Actions::new(
                &[(t("Refresh environments", "刷新环境"), 'f')],
                page.items.len(),
                *cursor,
            ),
            ManagementPage::Detail(details) => {
                let mut a = Actions::new(&[(t("View logs", "查看日志"), 'l')], 0, 0);
                if self.scope.historical_environment.is_none() && !details.snapshots.is_empty() {
                    a.items.extend([
                        (t("Restore / rollback", "恢复 / 回退"), KeyCode::Char('r')),
                        (
                            t("Inspect remote state", "检查远端状态"),
                            KeyCode::Char('i'),
                        ),
                    ]);
                }
                a
            }
            ManagementPage::InspectSelection { names, cursor, .. } => {
                Actions::choice(names.len(), *cursor)
            }
            ManagementPage::RollbackSelection {
                details, cursor, ..
            } => Actions::choice(details.snapshots.len(), *cursor),
            ManagementPage::RollbackTargets {
                candidates, cursor, ..
            } => {
                let mut a = Actions::choice(candidates.components.len(), *cursor);
                a.items.extend([
                    (t("Previous version", "上一个版本"), KeyCode::Left),
                    (t("Next version", "下一个版本"), KeyCode::Right),
                    (t("Version details", "版本详情"), KeyCode::Char('d')),
                ]);
                a
            }
            ManagementPage::RollbackReview(_) => {
                Actions::confirmation(t("Confirm rollback", "确认恢复 / 回退"), 'c')
            }
            ManagementPage::Failed { retry: Some(_), .. } => {
                Actions::new(&[(t("Retry", "重试"), 'f')], 0, 0)
            }
            _ => return None,
        };
        if matches!(
            self.page,
            ManagementPage::Home | ManagementPage::History { .. } | ManagementPage::Reports { .. }
        ) && self.scope.historical_environment.is_none()
        {
            actions.items.extend([
                (t("Inspect components", "检查组件"), KeyCode::Char('i')),
                (t("Historical environments", "历史环境"), KeyCode::Char('a')),
            ]);
        }
        let paging = match &self.page {
            ManagementPage::History { page, offset, .. } => Some((page.more, *offset)),
            ManagementPage::Reports { page, offset, .. } => Some((page.more, *offset)),
            ManagementPage::Environments { page, offset, .. } => Some((page.more, *offset)),
            _ => None,
        };
        if let Some((more, offset)) = paging {
            if more {
                actions
                    .items
                    .push((t("Next page", "下一页"), KeyCode::Char('n')));
            }
            if offset > 0 {
                actions
                    .items
                    .push((t("Previous page", "上一页"), KeyCode::Char('b')));
            }
        }
        Some(actions)
    }
}
