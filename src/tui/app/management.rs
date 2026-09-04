//! Management navigation is separate from deployment setup and live progress.
//! A single tracked worker owns each request until completion, including cancellation.

mod gateway;
mod render;
#[cfg(test)]
mod retired_tests;
#[cfg(test)]
mod rollback_tests;
mod search;
#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use tokio_util::sync::CancellationToken;

use crate::{
    application::{
        DeploymentSelection, RollbackCandidates, RollbackPlan, RollbackReport,
        history_query::{DeploymentDetails, HistoricalLogPage, HistoricalLogQuery, HistoryPage},
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
    scroll: u16,
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
    Logs {
        details: Arc<DeploymentDetails>,
        page: Arc<HistoricalLogPage>,
        query: HistoricalLogQuery,
        previous_offsets: Vec<u64>,
    },
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
    RollbackReview(Arc<RollbackPlan>),
    RollbackFinished(Arc<RollbackReport>),
}

#[derive(Clone, Debug)]
pub(super) enum ManagementRequest {
    Environments(RecoveryQuery),
    History(DeploymentQuery),
    Detail(DeploymentId),
    Logs {
        details: Arc<DeploymentDetails>,
        query: HistoricalLogQuery,
        previous_offsets: Vec<u64>,
    },
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
            Self::Environments(_)
            | Self::History(_)
            | Self::Detail(_)
            | Self::Reports(_)
            | Self::Report(_) => "Reading local records",
            Self::Logs { .. } => "Reading a bounded local log page",
            Self::Inspect { .. } => "Inspecting remote state (read-only)",
            Self::Candidates { .. } => "Checking local rollback evidence",
            Self::Plan { .. } => "Checking rollback targets (read-only)",
            Self::Execute(_) => "Executing rollback; safe cancellation may require compensation",
        }
    }
}

#[derive(Debug)]
pub(super) struct ManagementTask {
    id: uuid::Uuid,
    origin: ManagementScreen,
    cancellation: CancellationToken,
}

impl ManagementScreen {
    pub(super) fn requires_plain_confirmation(&self) -> bool {
        matches!(self.page, ManagementPage::RollbackReview(_))
    }
}

impl App {
    pub(super) fn open_management(&mut self, root: PathBuf, config: ProjectConfig) {
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
            scroll: 0,
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
            ManagementPage::Logs {
                details,
                page,
                query,
                previous_offsets,
            } => log_key(key, details, page, *query, previous_offsets),
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
            Arc::make_mut(&mut screen.scope).historical_environment = Some(environment.clone());
            screen.page = ManagementPage::Home;
            screen.scroll = 0;
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
            if key == KeyCode::Esc {
                Arc::make_mut(&mut screen.scope).historical_environment = None;
            }
            self.navigate_management(key, screen);
        }
    }

    fn handle_management_details(
        &mut self,
        key: KeyCode,
        mut screen: ManagementScreen,
        details: &Arc<DeploymentDetails>,
    ) {
        if details.snapshots.is_empty() && matches!(key, KeyCode::Char('i' | 'r')) {
            self.message = Some(MISSING_FROZEN_CONTEXT.into());
            self.screen = Screen::Management(screen);
            return;
        }
        let request = match key {
            KeyCode::Char('l') => Some(ManagementRequest::Logs {
                details: Arc::clone(details),
                query: HistoricalLogQuery::default(),
                previous_offsets: Vec::new(),
            }),
            KeyCode::Char('r') => {
                screen.page = ManagementPage::RollbackSelection {
                    details: Arc::clone(details),
                    selected: BTreeSet::new(),
                    cursor: 0,
                };
                screen.scroll = 0;
                self.screen = Screen::Management(screen);
                return;
            }
            KeyCode::Char('i') => {
                screen.page = ManagementPage::InspectSelection {
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
                };
                screen.scroll = 0;
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
            screen.page = match &screen.page {
                ManagementPage::Logs { details, .. } => ManagementPage::Detail(Arc::clone(details)),
                _ => ManagementPage::Home,
            };
            screen.scroll = 0;
        } else {
            scroll_key(key, &mut screen.scroll);
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
                screen.page = ManagementPage::InspectSelection {
                    source: None,
                    historical_releases: Vec::new(),
                    names: screen.scope.components(),
                    selected: screen.scope.components().into_iter().collect(),
                    cursor: 0,
                };
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
                None
            }
            KeyCode::Esc if screen.scope.historical_environment.is_some() => {
                Arc::make_mut(&mut screen.scope).historical_environment = None;
                screen.scroll = 0;
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
            self.message = Some("Background runtime is unavailable.".into());
            return;
        };
        let id = uuid::Uuid::now_v7();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let gateway = Arc::clone(&self.management_gateway);
        let sender = self.background_sender.clone();
        let scope = Arc::clone(&origin.scope);
        let label = request.label();
        let spawn = std::thread::Builder::new()
            .name("shipforge-management".into())
            .spawn(move || {
                let result = super::catch_worker_failure(|| {
                    runtime.block_on(gateway.run(&scope, request, &worker_cancel))
                });
                let _ = sender.send(BackgroundEvent::Management(id, result));
            });
        match spawn {
            Ok(_) => {
                self.invalidate_attention();
                self.screen = Screen::Management(ManagementScreen {
                    scope: Arc::clone(&origin.scope),
                    page: ManagementPage::Loading {
                        label,
                        started: Instant::now(),
                        cancelling: false,
                    },
                    scroll: 0,
                });
                self.management_task = Some(ManagementTask {
                    id,
                    origin,
                    cancellation,
                });
            }
            Err(_) => {
                self.message =
                    Some("Could not start management worker; nothing was started.".into());
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
        result: Result<ManagementPage, String>,
    ) {
        if self
            .management_task
            .as_ref()
            .is_none_or(|task| task.id != id)
        {
            return;
        }
        let Some(task) = self.management_task.take() else {
            return;
        };
        let mut screen = task.origin;
        match result {
            Ok(page) => {
                screen.page = page;
                screen.scroll = 0;
            }
            Err(error) => self.message = Some(render::safe_text(&error)),
        }
        self.screen = Screen::Management(screen);
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

fn log_key(
    key: KeyCode,
    details: &Arc<DeploymentDetails>,
    page: &HistoricalLogPage,
    mut query: HistoricalLogQuery,
    previous: &[u64],
) -> Option<ManagementRequest> {
    let mut previous_offsets = previous.to_vec();
    match key {
        KeyCode::Char('n') => {
            query.offset = page.next_offset?;
            if query.offset <= page.offset || previous_offsets.len() >= 1024 {
                return None;
            }
            previous_offsets.push(page.offset);
        }
        KeyCode::Char('b') => query.offset = previous_offsets.pop()?,
        KeyCode::Left | KeyCode::Right => {
            let index = page
                .available_generations
                .iter()
                .position(|item| *item == query.generation)
                .unwrap_or(0);
            let index = if key == KeyCode::Left {
                index.checked_sub(1)?
            } else {
                index.checked_add(1)?
            };
            query.generation = *page.available_generations.get(index)?;
            query.offset = 0;
            previous_offsets.clear();
        }
        KeyCode::Char('f') => {
            query.offset = 0;
            previous_offsets.clear();
        }
        _ => return None,
    }
    Some(ManagementRequest::Logs {
        details: Arc::clone(details),
        query,
        previous_offsets,
    })
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
        _ => *cursor,
    };
}

fn scroll_key(key: KeyCode, scroll: &mut u16) {
    *scroll = match key {
        KeyCode::Up => scroll.saturating_sub(1),
        KeyCode::Down => scroll.saturating_add(1),
        KeyCode::PageUp => scroll.saturating_sub(10),
        KeyCode::PageDown => scroll.saturating_add(10),
        KeyCode::Home => 0,
        _ => *scroll,
    };
}
