//! User-facing project edits are drafts until an exact service preview is confirmed.

mod forms;
mod gateway;
mod remote;
mod render;
mod search;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use crossterm::event::KeyCode;
use tokio_util::sync::CancellationToken;

use crate::{
    application::project_edit::{ProjectEditDraft, ProjectEditPreview},
    config::{ComponentSetup, EnvironmentSetup, ProjectConfig, TargetSetup},
    domain::ComponentName,
    projects::DiscoveryReport,
};

use super::{App, BackgroundEvent, Screen};

use forms::{ComponentForm, EnvironmentForm, TargetForm, TextEdit};
pub(super) use gateway::{LocalProjectEditGateway, ProjectEditGateway};

#[derive(Clone, Debug)]
pub(in crate::tui) struct ProjectEditScreen {
    root: PathBuf,
    draft: Option<Arc<ProjectEditDraft>>,
    page: ProjectEditPage,
    dirty: bool,
    scroll: u16,
}

#[derive(Clone, Debug)]
pub(super) enum ProjectEditPage {
    Home,
    Loaded(Arc<ProjectEditDraft>),
    Saved(Arc<ProjectConfig>),
    Loading {
        label: &'static str,
        started: Instant,
        cancelling: bool,
    },
    Components {
        cursor: usize,
    },
    Component {
        form: ComponentForm,
        cursor: usize,
    },
    Commands {
        form: ComponentForm,
        cursor: usize,
    },
    Command {
        form: ComponentForm,
        command: usize,
        cursor: usize,
    },
    Environments {
        cursor: usize,
    },
    Environment {
        form: EnvironmentForm,
        cursor: usize,
    },
    Target {
        form: TargetForm,
        cursor: usize,
    },
    Destination {
        form: TargetForm,
        cursor: usize,
    },
    Dependencies {
        form: TargetForm,
        names: Vec<ComponentName>,
        selected: BTreeSet<ComponentName>,
        cursor: usize,
    },
    Discovery {
        report: Arc<DiscoveryReport>,
        cursor: usize,
    },
    Text(TextEdit),
    Delete(DeleteKind),
    Discard,
    Preview(Arc<ProjectEditPreview>),
}

#[derive(Clone, Debug)]
pub(super) enum DeleteKind {
    Component(ComponentName),
    Environment(String),
}

#[derive(Clone, Debug)]
pub(super) enum ProjectEditRequest {
    Load(PathBuf),
    Discover(Arc<ProjectEditDraft>),
    Preview(Arc<ProjectEditDraft>),
    Save(Arc<ProjectEditPreview>),
}

impl ProjectEditRequest {
    const fn label(&self) -> &'static str {
        match self {
            Self::Load(_) => "Reading saved project configuration",
            Self::Discover(_) => {
                "Discovering local Component candidates; no build code is executed"
            }
            Self::Preview(_) => "Validating draft and preparing exact YAML preview",
            Self::Save(_) => "Saving confirmed project YAML; no deployment is performed",
        }
    }
}

#[derive(Debug)]
pub(super) struct ProjectEditTask {
    id: uuid::Uuid,
    origin: ProjectEditScreen,
    cancellation: CancellationToken,
}

impl App {
    pub(super) fn open_project_edit(&mut self, root: PathBuf) {
        let screen = ProjectEditScreen {
            root: root.clone(),
            draft: None,
            page: ProjectEditPage::Home,
            dirty: false,
            scroll: 0,
        };
        self.start_project_edit(screen, ProjectEditRequest::Load(root));
    }

    pub(super) fn handle_project_edit(&mut self, key: KeyCode, mut screen: ProjectEditScreen) {
        if self.project_edit_task.is_some() {
            if key == KeyCode::Esc {
                self.cancel_project_edit();
            }
            return;
        }
        if key == KeyCode::Char('b') && matches!(screen.page, ProjectEditPage::Target { .. }) {
            if let Some(target) =
                super::remote_target::RemoteSetupSelectionState::from_editor(screen.clone())
            {
                self.screen = Screen::RemoteSetupSelection(target);
            } else {
                self.message = Some("Select an available connection before opening target choices. Return to the project editor and reload if it changed.".into());
            }
            return;
        }
        let page = std::mem::replace(&mut screen.page, ProjectEditPage::Home);
        let request = match page {
            ProjectEditPage::Home => self.project_home_key(key, &mut screen),
            ProjectEditPage::Components { cursor } => {
                Self::component_list_key(key, cursor, &mut screen)
            }
            ProjectEditPage::Environments { cursor } => {
                Self::environment_list_key(key, cursor, &mut screen)
            }
            ProjectEditPage::Discovery { report, mut cursor } => {
                move_cursor(key, &mut cursor, report.components.len());
                screen.page = if key == KeyCode::Enter {
                    report.components.get(cursor).map_or(
                        ProjectEditPage::Components { cursor: 0 },
                        |candidate| ProjectEditPage::Component {
                            form: ComponentForm::new(
                                None,
                                candidate.name.to_string(),
                                &candidate.setup,
                            ),
                            cursor: 0,
                        },
                    )
                } else if key == KeyCode::Esc {
                    ProjectEditPage::Components { cursor: 0 }
                } else {
                    ProjectEditPage::Discovery { report, cursor }
                };
                None
            }
            ProjectEditPage::Delete(kind) => {
                Self::delete_draft_key(key, kind, &mut screen);
                None
            }
            ProjectEditPage::Discard => {
                if key == KeyCode::Char('c') {
                    self.leave_project_editor(&screen);
                    return;
                }
                screen.page = if key == KeyCode::Esc {
                    ProjectEditPage::Home
                } else {
                    ProjectEditPage::Discard
                };
                None
            }
            ProjectEditPage::Preview(preview) => {
                if key == KeyCode::Char('c') {
                    screen.page = ProjectEditPage::Preview(Arc::clone(&preview));
                    Some(ProjectEditRequest::Save(preview))
                } else {
                    screen.page = if key == KeyCode::Esc {
                        ProjectEditPage::Home
                    } else {
                        ProjectEditPage::Preview(preview)
                    };
                    scroll_key(key, &mut screen.scroll);
                    None
                }
            }
            page => {
                self.project_form_key(key, page, &mut screen);
                None
            }
        };
        if let Some(request) = request {
            self.start_project_edit(screen, request);
        } else if !matches!(self.screen, Screen::Overview { .. } | Screen::Projects) {
            self.screen = Screen::ProjectEdit(screen);
        }
    }

    fn project_home_key(
        &mut self,
        key: KeyCode,
        screen: &mut ProjectEditScreen,
    ) -> Option<ProjectEditRequest> {
        if key == KeyCode::Esc {
            if screen.dirty {
                screen.page = ProjectEditPage::Discard;
            } else {
                self.leave_project_editor(screen);
            }
            return None;
        }
        let Some(draft) = screen.draft.as_ref() else {
            return (key == KeyCode::Char('r'))
                .then(|| ProjectEditRequest::Load(screen.root.clone()));
        };
        match key {
            KeyCode::Char('n') => {
                screen.page = forms::text_page(
                    forms::TextField::Project,
                    draft.setup.project.clone(),
                    ProjectEditPage::Home,
                );
            }
            KeyCode::Char('c') => screen.page = ProjectEditPage::Components { cursor: 0 },
            KeyCode::Char('e') => screen.page = ProjectEditPage::Environments { cursor: 0 },
            KeyCode::Char('p') => return Some(ProjectEditRequest::Preview(Arc::clone(draft))),
            _ => {}
        }
        None
    }

    fn leave_project_editor(&mut self, screen: &ProjectEditScreen) {
        if let Some(draft) = &screen.draft {
            self.show_overview(screen.root.clone(), draft.original().clone());
        } else {
            self.show_projects();
        }
    }

    fn component_list_key(
        key: KeyCode,
        mut cursor: usize,
        screen: &mut ProjectEditScreen,
    ) -> Option<ProjectEditRequest> {
        let draft = screen.draft.as_ref()?;
        let names = draft.setup.components.keys().cloned().collect::<Vec<_>>();
        move_cursor(key, &mut cursor, names.len());
        screen.page = match key {
            KeyCode::Esc => ProjectEditPage::Home,
            KeyCode::Char('a') => ProjectEditPage::Component {
                form: ComponentForm::blank(),
                cursor: 0,
            },
            KeyCode::Enter => {
                names
                    .get(cursor)
                    .map_or(ProjectEditPage::Components { cursor }, |name| {
                        ProjectEditPage::Component {
                            form: ComponentForm::new(
                                Some(name.clone()),
                                name.to_string(),
                                &draft.setup.components[name],
                            ),
                            cursor: 0,
                        }
                    })
            }
            KeyCode::Char('d') if names.len() > 1 => names
                .get(cursor)
                .map_or(ProjectEditPage::Components { cursor }, |name| {
                    ProjectEditPage::Delete(DeleteKind::Component(name.clone()))
                }),
            KeyCode::Char('f') => {
                screen.page = ProjectEditPage::Components { cursor };
                return Some(ProjectEditRequest::Discover(Arc::clone(draft)));
            }
            _ => ProjectEditPage::Components { cursor },
        };
        None
    }

    fn environment_list_key(
        key: KeyCode,
        mut cursor: usize,
        screen: &mut ProjectEditScreen,
    ) -> Option<ProjectEditRequest> {
        let draft = screen.draft.as_ref()?;
        let names = draft.setup.environments.keys().cloned().collect::<Vec<_>>();
        move_cursor(key, &mut cursor, names.len());
        screen.page = match key {
            KeyCode::Esc => ProjectEditPage::Home,
            KeyCode::Char('a') => ProjectEditPage::Environment {
                form: EnvironmentForm::blank(),
                cursor: 0,
            },
            KeyCode::Enter => {
                names
                    .get(cursor)
                    .map_or(ProjectEditPage::Environments { cursor }, |name| {
                        ProjectEditPage::Environment {
                            form: EnvironmentForm {
                                original: Some(name.clone()),
                                name: name.clone(),
                                targets: draft.setup.environments[name].components.clone(),
                            },
                            cursor: 0,
                        }
                    })
            }
            KeyCode::Char('d') if names.len() > 1 => names
                .get(cursor)
                .map_or(ProjectEditPage::Environments { cursor }, |name| {
                    ProjectEditPage::Delete(DeleteKind::Environment(name.clone()))
                }),
            _ => ProjectEditPage::Environments { cursor },
        };
        None
    }

    fn delete_draft_key(key: KeyCode, kind: DeleteKind, screen: &mut ProjectEditScreen) {
        if key != KeyCode::Char('c') && key != KeyCode::Esc {
            screen.page = ProjectEditPage::Delete(kind);
            return;
        }
        screen.page = match &kind {
            DeleteKind::Component(_) => ProjectEditPage::Components { cursor: 0 },
            DeleteKind::Environment(_) => ProjectEditPage::Environments { cursor: 0 },
        };
        if key == KeyCode::Esc {
            return;
        }
        let Some(draft) = screen.draft.as_mut() else {
            return;
        };
        let draft = Arc::make_mut(draft);
        match kind {
            DeleteKind::Component(name) if draft.setup.components.len() > 1 => {
                draft.setup.components.remove(&name);
                for environment in draft.setup.environments.values_mut() {
                    environment.components.remove(&name);
                    for target in environment.components.values_mut() {
                        target.after.retain(|dependency| dependency != &name);
                    }
                }
            }
            DeleteKind::Environment(name) if draft.setup.environments.len() > 1 => {
                draft.setup.environments.remove(&name);
                draft.environment_renames.retain(|rename| rename.to != name);
            }
            _ => return,
        }
        screen.dirty = true;
    }

    fn start_project_edit(&mut self, origin: ProjectEditScreen, request: ProjectEditRequest) {
        if self.project_edit_task.is_some()
            || self.management_task.is_some()
            || self.connections_task.is_some()
            || self.deployment_session.is_active()
        {
            self.message = Some("Another operation is still active; wait for completion.".into());
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.message = Some("Background runtime is unavailable.".into());
            return;
        };
        let id = uuid::Uuid::now_v7();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let gateway = Arc::clone(&self.project_edit_gateway);
        let sender = self.background_sender.clone();
        let label = request.label();
        let spawned = std::thread::Builder::new()
            .name("shipforge-project-edit".into())
            .spawn(move || {
                let result = super::catch_worker_failure(|| {
                    runtime.block_on(gateway.run(request, &worker_cancel))
                });
                let _ = sender.send(BackgroundEvent::ProjectEdit(id, result));
            });
        match spawned {
            Ok(_) => {
                self.invalidate_attention();
                let mut loading = origin.clone();
                loading.page = ProjectEditPage::Loading {
                    label,
                    started: Instant::now(),
                    cancelling: false,
                };
                loading.scroll = 0;
                self.screen = Screen::ProjectEdit(loading);
                self.project_edit_task = Some(ProjectEditTask {
                    id,
                    origin,
                    cancellation,
                });
            }
            Err(_) => {
                self.message = Some("Could not start editor worker; nothing was saved.".into());
            }
        }
    }

    pub(super) fn cancel_project_edit(&mut self) {
        if let Some(task) = &self.project_edit_task {
            task.cancellation.cancel();
            if let Screen::ProjectEdit(ProjectEditScreen {
                page: ProjectEditPage::Loading { cancelling, .. },
                ..
            }) = &mut self.screen
            {
                *cancelling = true;
            }
        }
    }

    pub(super) fn finish_project_edit(
        &mut self,
        id: uuid::Uuid,
        result: Result<ProjectEditPage, String>,
    ) {
        if self
            .project_edit_task
            .as_ref()
            .is_none_or(|task| task.id != id)
        {
            return;
        }
        let Some(task) = self.project_edit_task.take() else {
            return;
        };
        let mut screen = task.origin;
        match result {
            Ok(ProjectEditPage::Loaded(draft)) => {
                draft.root().clone_into(&mut screen.root);
                screen.draft = Some(draft);
                screen.page = ProjectEditPage::Home;
            }
            Ok(ProjectEditPage::Saved(config)) => {
                self.show_overview(screen.root, (*config).clone());
                self.message =
                    Some("Saved the confirmed shipforge.yaml. No deployment was performed.".into());
                return;
            }
            Ok(page) => {
                screen.page = page;
                screen.scroll = 0;
            }
            Err(error) => self.message = Some(render::safe_text(&error)),
        }
        self.screen = Screen::ProjectEdit(screen);
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
