//! Shared, draft-only target selection for first setup and the project editor.

mod render;
mod service_editor;
#[cfg(test)]
mod tests;
mod worker;

use crate::{
    application::{RemoteDirectoryCandidates, RemoteSetupCandidates, SetupRootState},
    config::{DestinationSummary, default_remote_root},
    domain::ComponentName,
    tui::picker::ChoiceSet,
};

use super::{
    App, ComponentTargetSettings, DestinationSetupState, KeyCode, KeyEvent, Screen,
    project_edit::ProjectEditScreen, selected_components, suggest_project_name,
};
pub(super) use worker::{RemoteTargetResult, RemoteTargetTask};

#[derive(Clone, Debug)]
enum Origin {
    Initial(Box<DestinationSetupState>),
    Editor(Box<ProjectEditScreen>),
}

#[derive(Clone, Debug)]
enum Page {
    Services,
    CommandEditor(service_editor::ServiceEditor),
    Directories {
        candidates: RemoteDirectoryCandidates,
        cursor: usize,
    },
    Unavailable {
        retry_path: String,
    },
    Text {
        field: TextField,
        value: String,
        offset: usize,
    },
    Loading {
        cancelling: bool,
    },
}

#[derive(Clone, Copy, Debug)]
enum TextField {
    Root,
    Service,
    Browse,
}

enum Action {
    Stay,
    Back(bool),
    Read(worker::Request),
}

#[derive(Clone, Debug)]
pub(in crate::tui) struct RemoteSetupSelectionState {
    origin: Origin,
    pub(super) component: ComponentName,
    destination: DestinationSummary,
    project: String,
    environment: String,
    pub(super) root: String,
    pub(super) root_state: Option<SetupRootState>,
    pub(super) systemd_units: Vec<String>,
    custom_service: Option<crate::config::ServiceConfig>,
    pub(super) cursor: usize,
    notices: Vec<String>,
    page: Page,
}

impl RemoteSetupSelectionState {
    pub(super) fn from_editor(screen: ProjectEditScreen) -> Option<Self> {
        let (destination, component, project, environment, root, service) =
            screen.remote_target_context()?;
        Some(Self::new(
            Origin::Editor(Box::new(screen)),
            destination,
            component,
            project,
            environment,
            root,
            service,
        ))
    }

    fn new(
        origin: Origin,
        destination: DestinationSummary,
        component: ComponentName,
        project: String,
        environment: String,
        root: String,
        service: Option<crate::config::ServiceConfig>,
    ) -> Self {
        Self {
            origin,
            destination,
            component,
            project,
            environment,
            root,
            root_state: None,
            systemd_units: service
                .as_ref()
                .and_then(crate::config::ServiceConfig::preset_unit)
                .map(str::to_owned)
                .into_iter()
                .collect(),
            custom_service: service.filter(|service| service.preset_unit().is_none()),
            cursor: 0,
            notices: Vec::new(),
            page: Page::Services,
        }
        .with_original_service()
    }

    fn with_original_service(mut self) -> Self {
        self.cursor = if self.custom_service.is_some() {
            self.systemd_units.len() + 1
        } else {
            usize::from(!self.systemd_units.is_empty())
        };
        self
    }

    fn service(&self) -> Option<crate::config::ServiceConfig> {
        if self.cursor == self.systemd_units.len() + 1 {
            return self.custom_service.clone();
        }
        self.cursor
            .checked_sub(1)
            .and_then(|index| self.systemd_units.get(index))
            .cloned()
            .map(crate::config::ServiceConfig::systemd)
    }

    fn adopt_probe(&mut self, candidates: RemoteSetupCandidates) -> bool {
        // Initial authentication and later saved-connection reads share one
        // projection boundary; neither can install unbounded/unrenderable data.
        if !valid_probe_candidates(&candidates) {
            return false;
        }
        let selected = self.service();
        let custom_selected = self.cursor == self.systemd_units.len() + 1;
        self.root_state = Some(candidates.root);
        self.systemd_units = candidates.services;
        self.notices = candidates.notices;
        if custom_selected {
            self.cursor = self.systemd_units.len() + 1;
        } else if let Some(service) =
            selected.and_then(|service| service.preset_unit().map(str::to_owned))
        {
            if !self.systemd_units.contains(&service) {
                self.systemd_units.push(service.clone());
            }
            self.cursor = self
                .systemd_units
                .iter()
                .position(|unit| *unit == service)
                .map_or(0, |index| index + 1);
        } else {
            self.cursor = 0;
        }
        true
    }

    fn set_root(&mut self, root: String) {
        if self.root != root {
            self.root = root;
            self.root_state = None;
            self.notices.clear();
        }
    }

    fn back(self, apply: bool) -> Screen {
        let service = self.service();
        match self.origin {
            Origin::Initial(mut setup) => {
                if apply {
                    setup.target_settings.insert(
                        self.component,
                        ComponentTargetSettings {
                            root: Some(self.root),
                            service,
                        },
                    );
                }
                Screen::SetupDestinations(*setup)
            }
            Origin::Editor(mut screen) => {
                if apply {
                    screen.apply_remote_target(self.root, service);
                }
                Screen::ProjectEdit(*screen)
            }
        }
    }

    pub(in crate::tui) fn requires_plain_confirmation(&self) -> bool {
        !matches!(self.page, Page::Text { .. })
            && !matches!(&self.page, Page::CommandEditor(editor) if editor.is_text())
    }

    pub(super) fn search_choices(&self) -> Option<ChoiceSet> {
        let (title, items, selected, empty_hint) = match &self.page {
            Page::Services => {
                let mut items = vec![(0, "Do not manage a service".into())];
                items.extend(
                    self.systemd_units
                        .iter()
                        .enumerate()
                        .map(|(i, unit)| (i + 1, unit.clone())),
                );
                items.push((
                    self.systemd_units.len() + 1,
                    "Custom remote commands".into(),
                ));
                (
                    "service commands and presets",
                    items,
                    self.cursor,
                    "Use v to inspect services or m to enter a unit; service management is optional.",
                )
            }
            Page::Directories { candidates, cursor } => (
                "remote directories",
                candidates
                    .directories
                    .iter()
                    .enumerate()
                    .map(|(i, path)| (i, path.clone()))
                    .collect(),
                *cursor,
                "No child directories. Esc returns; Backspace visits parent, s selects this directory, g enters another path.",
            ),
            _ => return None,
        };
        Some(ChoiceSet {
            title,
            items,
            selected,
            empty_hint,
        })
    }

    pub(super) fn focus_search_choice(&mut self, index: usize) {
        match &mut self.page {
            Page::Services if index <= self.systemd_units.len() + 1 => self.cursor = index,
            Page::Directories { candidates, cursor } if index < candidates.directories.len() => {
                *cursor = index;
            }
            _ => {}
        }
    }
}

impl App {
    pub(super) fn open_initial_remote_target(
        &mut self,
        setup: DestinationSetupState,
        candidates: Option<RemoteSetupCandidates>,
    ) {
        let Some(component) = selected_components(&setup)
            .get(setup.component_cursor)
            .cloned()
        else {
            return;
        };
        let Some(destination) = setup
            .assignments
            .get(&component)
            .and_then(|key| setup.destinations.iter().find(|item| item.key == *key))
            .cloned()
        else {
            self.message = Some("Assign a connection to this Component first.".into());
            return;
        };
        let project = suggest_project_name(&setup.components.root);
        let settings = setup
            .target_settings
            .get(&component)
            .cloned()
            .unwrap_or_default();
        let root = settings
            .root
            .unwrap_or_else(|| default_remote_root(&project, "production", &component));
        let mut screen = RemoteSetupSelectionState::new(
            Origin::Initial(Box::new(setup)),
            destination,
            component,
            project,
            "production".into(),
            root,
            settings.service,
        );
        if let Some(candidates) = candidates
            && !screen.adopt_probe(candidates)
        {
            self.message = Some("The connection was saved, but target observations are unavailable. Press v to inspect again or enter the root and optional service manually.".into());
        }
        self.screen = Screen::RemoteSetupSelection(screen);
    }

    pub(super) fn handle_remote_setup_selection(
        &mut self,
        key: KeyEvent,
        mut screen: RemoteSetupSelectionState,
    ) {
        if key.code == KeyCode::Enter && !key.modifiers.is_empty() {
            return;
        }
        let action = match screen.page.clone() {
            Page::Services => self.remote_service_key(key.code, &mut screen),
            Page::CommandEditor(mut editor) => {
                match editor.handle(key.code) {
                    service_editor::EditResult::Apply(service) => {
                        screen.custom_service = Some(service);
                        screen.cursor = screen.systemd_units.len() + 1;
                        screen.page = Page::Services;
                    }
                    service_editor::EditResult::Discard => screen.page = Page::Services,
                    service_editor::EditResult::Editing => {
                        screen.page = Page::CommandEditor(editor);
                    }
                }
                Action::Stay
            }
            Page::Directories { candidates, cursor } => {
                self.remote_directory_key(key.code, &mut screen, candidates, cursor)
            }
            Page::Text {
                field,
                value,
                offset,
            } => self.remote_text_key(key.code, &mut screen, field, value, offset),
            Page::Unavailable { retry_path } => {
                match key.code {
                    KeyCode::Esc => screen.page = Page::Services,
                    KeyCode::Char('f') => {
                        self.start_remote_target(screen, worker::Request::Browse(retry_path));
                        return;
                    }
                    KeyCode::Char('g') => {
                        screen.page = Page::Text {
                            field: TextField::Browse,
                            value: retry_path,
                            offset: 0,
                        }
                    }
                    _ => {}
                }
                Action::Stay
            }
            Page::Loading { .. } => Action::Stay,
        };
        match action {
            Action::Stay => self.screen = Screen::RemoteSetupSelection(screen),
            Action::Back(apply) => {
                self.screen = screen.back(apply);
                if apply {
                    self.message = Some("Target choices applied to the draft only. Review and confirm YAML to save; no deployment was performed.".into());
                }
            }
            Action::Read(request) => self.start_remote_target(screen, request),
        }
    }

    fn remote_service_key(
        &mut self,
        key: KeyCode,
        screen: &mut RemoteSetupSelectionState,
    ) -> Action {
        match key {
            KeyCode::Esc => return Action::Back(false),
            KeyCode::Enter => {
                if screen.cursor == screen.systemd_units.len() + 1
                    && screen.custom_service.is_none()
                {
                    screen.page = Page::CommandEditor(service_editor::ServiceEditor::new(None));
                    return Action::Stay;
                }
                if valid_path(&screen.root, false) {
                    return Action::Back(true);
                }
                self.message = Some("Choose an absolute Component directory, not /; no trailing slash, . or .. segments. Press r to edit.".into());
            }
            KeyCode::Up => screen.cursor = screen.cursor.saturating_sub(1),
            KeyCode::Down => {
                screen.cursor = (screen.cursor + 1).min(screen.systemd_units.len() + 1);
            }
            KeyCode::Home => screen.cursor = 0,
            KeyCode::End => screen.cursor = screen.systemd_units.len() + 1,
            KeyCode::PageUp => screen.cursor = screen.cursor.saturating_sub(10),
            KeyCode::PageDown => {
                screen.cursor = (screen.cursor + 10).min(screen.systemd_units.len() + 1);
            }
            KeyCode::Char('r') => {
                screen.page = Page::Text {
                    field: TextField::Root,
                    value: screen.root.clone(),
                    offset: 0,
                }
            }
            KeyCode::Char('m') => {
                screen.page = Page::Text {
                    field: TextField::Service,
                    value: screen
                        .service()
                        .as_ref()
                        .and_then(crate::config::ServiceConfig::preset_unit)
                        .unwrap_or_default()
                        .into(),
                    offset: 0,
                }
            }
            KeyCode::Char('v') => return Action::Read(worker::Request::Inspect),
            KeyCode::Char('c') => {
                screen.page =
                    Page::CommandEditor(service_editor::ServiceEditor::new(screen.service()));
            }
            // Browsing starts at an existing navigation root, not a suggested deployment root.
            KeyCode::Char('b') => return Action::Read(worker::Request::Browse("/".into())),
            _ => {}
        }
        Action::Stay
    }

    fn remote_directory_key(
        &mut self,
        key: KeyCode,
        screen: &mut RemoteSetupSelectionState,
        candidates: RemoteDirectoryCandidates,
        mut cursor: usize,
    ) -> Action {
        match key {
            KeyCode::Esc => screen.page = Page::Services,
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Down => {
                cursor = (cursor + 1).min(candidates.directories.len().saturating_sub(1));
            }
            KeyCode::Home => cursor = 0,
            KeyCode::End => cursor = candidates.directories.len().saturating_sub(1),
            KeyCode::PageUp => cursor = cursor.saturating_sub(10),
            KeyCode::PageDown => {
                cursor = (cursor + 10).min(candidates.directories.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(path) = candidates.directories.get(cursor) {
                    return Action::Read(worker::Request::Browse(path.clone()));
                }
            }
            KeyCode::Backspace => {
                let parent = candidates
                    .directory
                    .rsplit_once('/')
                    .map_or(
                        "/",
                        |(parent, _)| {
                            if parent.is_empty() { "/" } else { parent }
                        },
                    );
                return Action::Read(worker::Request::Browse(parent.into()));
            }
            KeyCode::Char('f') => {
                return Action::Read(worker::Request::Browse(candidates.directory));
            }
            KeyCode::Char('g') => {
                screen.page = Page::Text {
                    field: TextField::Browse,
                    value: candidates.directory.clone(),
                    offset: 0,
                }
            }
            KeyCode::Char('s') => {
                if valid_path(&candidates.directory, false) {
                    screen.set_root(candidates.directory.clone());
                    screen.page = Page::Services;
                } else {
                    self.message = Some(
                        "/ is only for browsing. Select a Component-specific subdirectory.".into(),
                    );
                }
            }
            _ => {}
        }
        if matches!(screen.page, Page::Directories { .. }) {
            screen.page = Page::Directories { candidates, cursor };
        }
        Action::Stay
    }

    fn remote_text_key(
        &mut self,
        key: KeyCode,
        screen: &mut RemoteSetupSelectionState,
        field: TextField,
        mut value: String,
        mut offset: usize,
    ) -> Action {
        match key {
            KeyCode::Esc => screen.page = Page::Services,
            KeyCode::Enter => match field {
                TextField::Root if valid_path(&value, false) => {
                    screen.set_root(value.clone());
                    screen.page = Page::Services;
                }
                TextField::Browse if valid_path(&value, true) => {
                    return Action::Read(worker::Request::Browse(value));
                }
                TextField::Service if valid_service(&value) => {
                    if value.is_empty() { screen.cursor = 0; }
                    else {
                        if !screen.systemd_units.contains(&value) {
                            if screen.systemd_units.len() >= 4096 {
                                self.message = Some("Too many service candidates. Refresh inspection before adding another.".into());
                                return Action::Stay;
                            }
                            screen.systemd_units.push(value.clone());
                        }
                        screen.cursor = screen.systemd_units.iter().position(|unit| *unit == value).map_or(0, |index| index + 1);
                    }
                    screen.page = Page::Services;
                }
                _ => self.message = Some("Invalid value. Use an absolute canonical directory or a plain .service unit (empty disables service management).".into()),
            },
            KeyCode::Backspace => { value.pop(); }
            KeyCode::Delete => value.clear(),
            KeyCode::Left => offset = offset.saturating_sub(20),
            KeyCode::Right => offset = offset.saturating_add(20).min(value.chars().count()),
            KeyCode::Home => offset = 0,
            KeyCode::End => offset = value.chars().count().saturating_sub(20),
            KeyCode::Char(c) if safe_char(c) && value.len() + c.len_utf8() <= 4096 => {
                value.push(c);
                offset = value.chars().count().saturating_sub(20);
            }
            _ => {}
        }
        if matches!(screen.page, Page::Text { .. }) {
            screen.page = Page::Text {
                field,
                value,
                offset,
            };
        }
        Action::Stay
    }
}

fn safe_char(c: char) -> bool {
    !c.is_control()
        && !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn valid_probe_candidates(candidates: &RemoteSetupCandidates) -> bool {
    candidates.services.len() <= 4095
        && candidates
            .services
            .iter()
            .all(|unit| !unit.is_empty() && valid_service(unit))
        && candidates.notices.len() <= 256
        && candidates
            .notices
            .iter()
            .all(|note| note.len() <= 4096 && note.chars().all(safe_char))
}

fn valid_path(path: &str, allow_root: bool) -> bool {
    path.len() <= 4096
        && path.chars().all(safe_char)
        && path.starts_with('/')
        && ((allow_root && path == "/")
            || path
                .split('/')
                .skip(1)
                .all(|part| !part.is_empty() && part != "." && part != ".." && part.len() <= 255))
}

fn valid_service(value: &str) -> bool {
    value.is_empty()
        || (value.len() <= 255
            && value.ends_with(".service")
            && !value.starts_with('-')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@._:-\\".contains(&b)))
}

impl RemoteSetupSelectionState {
    pub(super) fn actions(&self) -> Option<super::actions::Actions> {
        use super::actions::Actions;
        use crate::tui::i18n::choose as t;
        Some(match &self.page {
            Page::Services => Actions::new(
                &[
                    (t("Edit remote directory", "编辑远端目录"), 'r'),
                    (t("Browse remote directories", "浏览远端目录"), 'b'),
                    (t("Edit service commands", "编辑服务命令"), 'c'),
                    (t("Enter systemd unit", "输入 systemd 单元"), 'm'),
                    (t("Inspect remote state", "检查远端状态"), 'v'),
                ],
                self.systemd_units.len() + 2,
                self.cursor,
            ),
            Page::Directories { candidates, cursor } => Actions::new(
                &[
                    (t("Use this directory", "使用当前目录"), 's'),
                    (t("Enter path", "输入路径"), 'g'),
                    (t("Refresh", "刷新"), 'f'),
                ],
                candidates.directories.len(),
                *cursor,
            ),
            Page::CommandEditor(editor) => return editor.actions_menu(),
            _ => return None,
        })
    }
}
