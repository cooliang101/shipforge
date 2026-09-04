//! Standalone connection management and recent-Project unregistration.

mod render;
mod worker;

use std::{sync::Arc, time::Instant};

use tokio_util::sync::CancellationToken;

use crate::application::{
    ConnectionDetails, ConnectionManagementService, DestinationRemovalPreview, HostKeyConfirmation,
    ManagementPaths, ProjectRemovalPreview,
};

use super::{
    App, BackgroundEvent, CredentialChoice, DestinationKey, KeyCode, PathBuf, ProjectStatus,
    Screen, SshCandidate, SshField,
};

#[derive(Clone, Debug)]
pub(in crate::tui) struct ConnectionsScreen {
    page: ConnectionsPage,
    scroll: u16,
}

#[derive(Clone, Debug)]
pub(super) enum ConnectionsPage {
    List {
        items: Arc<Vec<ConnectionDetails>>,
        cursor: usize,
    },
    Detail {
        connection: Arc<ConnectionDetails>,
        notice: Option<String>,
    },
    Form(Arc<ConnectionForm>),
    Keys {
        form: Arc<ConnectionForm>,
        directory: Arc<KeyDirectory>,
        cursor: usize,
    },
    HostKey {
        form: Arc<ConnectionForm>,
        confirmation: Arc<HostKeyConfirmation>,
    },
    Remove(Arc<DestinationRemovalPreview>),
    ProjectRemove(Arc<ProjectRemovalPreview>),
    ProjectRemoved(Arc<Vec<ProjectStatus>>),
    Loading {
        label: &'static str,
        started: Instant,
        cancelling: bool,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ConnectionForm {
    existing: Option<ConnectionDetails>,
    host: String,
    user: String,
    port: String,
    field: SshField,
    credentials: Vec<CredentialChoice>,
    credential_cursor: usize,
    hosts: Vec<SshCandidate>,
    host_cursor: usize,
    notices: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct KeyDirectory {
    path: PathBuf,
    entries: Vec<KeyEntry>,
}

#[derive(Clone, Debug)]
struct KeyEntry {
    path: PathBuf,
    directory: bool,
}

#[derive(Clone, Debug)]
enum ConnectionsRequest {
    List,
    Form(Option<DestinationKey>),
    Keys {
        form: Arc<ConnectionForm>,
        path: PathBuf,
    },
    SelectKey {
        form: Arc<ConnectionForm>,
        path: PathBuf,
    },
    Capture(Arc<ConnectionForm>),
    Save(Arc<HostKeyConfirmation>),
    Verify(Arc<ConnectionDetails>),
    RemovePreview(DestinationKey),
    Remove(Arc<DestinationRemovalPreview>),
    ProjectPreview(PathBuf),
    ProjectRemove(Arc<ProjectRemovalPreview>),
}

impl ConnectionsRequest {
    const fn label(&self) -> &'static str {
        match self {
            Self::List | Self::Form(_) | Self::Keys { .. } | Self::SelectKey { .. } => {
                "Reading local connection choices"
            }
            Self::Capture(_) => "Capturing SSH host-key fingerprint; not authenticating yet",
            Self::Save(_) => "Authenticating the confirmed host key, then saving local settings",
            Self::Verify(_) => "Read-only verification using the saved host-key pin",
            Self::RemovePreview(_) => {
                "Checking all registered Projects and local history references"
            }
            Self::Remove(_) => {
                "Rechecking references and removing the local connection registration"
            }
            Self::ProjectPreview(_) => "Reading recent-Project registration",
            Self::ProjectRemove(_) => "Removing only the recent-Project registration",
        }
    }
}

#[derive(Debug)]
pub(super) struct ConnectionsTask {
    id: uuid::Uuid,
    origin: ConnectionsScreen,
    cancellation: CancellationToken,
}

impl ConnectionsScreen {
    pub(super) fn requires_plain_confirmation(&self) -> bool {
        matches!(
            self.page,
            ConnectionsPage::HostKey { .. }
                | ConnectionsPage::Remove(_)
                | ConnectionsPage::ProjectRemove(_)
        )
    }
}

impl App {
    pub(super) fn open_connections(&mut self) {
        self.start_connections(empty_screen(), ConnectionsRequest::List);
    }

    pub(super) fn preview_recent_removal(&mut self) {
        let Some(project) = self.recent.get(self.selected_recent) else {
            self.message = Some("Select a registered Project to remove from recents.".into());
            return;
        };
        let root = project.project.root.clone();
        self.start_connections(empty_screen(), ConnectionsRequest::ProjectPreview(root));
    }

    pub(super) fn handle_connections(&mut self, key: KeyCode, mut screen: ConnectionsScreen) {
        if self.connections_task.is_some() {
            if key == KeyCode::Esc {
                self.cancel_connections();
            }
            return;
        }
        let request = match &mut screen.page {
            ConnectionsPage::List { items, cursor } => {
                move_cursor(key, cursor, items.len());
                match key {
                    KeyCode::Char('a') => Some(ConnectionsRequest::Form(None)),
                    KeyCode::Char('f') => Some(ConnectionsRequest::List),
                    KeyCode::Enter => {
                        if let Some(item) = items.get(*cursor) {
                            screen.page = ConnectionsPage::Detail {
                                connection: Arc::new(item.clone()),
                                notice: None,
                            };
                        }
                        None
                    }
                    KeyCode::Esc => {
                        self.screen = Screen::Projects;
                        return;
                    }
                    _ => None,
                }
            }
            ConnectionsPage::Detail { connection, .. } => match key {
                KeyCode::Char('e') => Some(ConnectionsRequest::Form(Some(connection.key.clone()))),
                KeyCode::Char('v') => Some(ConnectionsRequest::Verify(Arc::clone(connection))),
                KeyCode::Char('x') => {
                    Some(ConnectionsRequest::RemovePreview(connection.key.clone()))
                }
                KeyCode::Esc => Some(ConnectionsRequest::List),
                _ => None,
            },
            ConnectionsPage::Form(form) => {
                let form = Arc::clone(form);
                return self.handle_connection_form(key, screen, form);
            }
            ConnectionsPage::Keys {
                form,
                directory,
                cursor,
            } => {
                let request = keys_request(key, form, directory, cursor);
                if key == KeyCode::Esc {
                    screen.page = ConnectionsPage::Form(Arc::clone(form));
                }
                request
            }
            ConnectionsPage::HostKey { form, confirmation } => match key {
                KeyCode::Char('y') => Some(ConnectionsRequest::Save(Arc::clone(confirmation))),
                KeyCode::Esc | KeyCode::Char('n') => {
                    screen.page = ConnectionsPage::Form(Arc::clone(form));
                    None
                }
                _ => None,
            },
            ConnectionsPage::Remove(preview) => match key {
                KeyCode::Char('c') if preview.can_remove() => {
                    Some(ConnectionsRequest::Remove(Arc::clone(preview)))
                }
                KeyCode::Esc => {
                    screen.page = ConnectionsPage::Detail {
                        connection: Arc::new(preview.details().clone()),
                        notice: None,
                    };
                    None
                }
                _ => None,
            },
            ConnectionsPage::ProjectRemove(preview) => match key {
                KeyCode::Char('c') => Some(ConnectionsRequest::ProjectRemove(Arc::clone(preview))),
                KeyCode::Esc => {
                    self.screen = Screen::Projects;
                    return;
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(request) = request {
            self.start_connections(screen, request);
        } else {
            scroll(key, &mut screen.scroll);
            self.screen = Screen::Connections(screen);
        }
    }

    fn handle_connection_form(
        &mut self,
        key: KeyCode,
        mut screen: ConnectionsScreen,
        mut form: Arc<ConnectionForm>,
    ) {
        let edit = Arc::make_mut(&mut form);
        let request = match key {
            KeyCode::Esc => Some(ConnectionsRequest::List),
            KeyCode::Tab => {
                edit.field = next_field(edit.field);
                None
            }
            KeyCode::Up => {
                edit.credential_cursor = edit.credential_cursor.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                edit.credential_cursor = edit
                    .credential_cursor
                    .saturating_add(1)
                    .min(edit.credentials.len().saturating_sub(1));
                None
            }
            KeyCode::F(2) => {
                next_host(edit);
                None
            }
            KeyCode::F(3) => Some(ConnectionsRequest::Keys {
                form: Arc::clone(&form),
                path: self
                    .home_directory
                    .clone()
                    .unwrap_or_else(|| self.initial_directory.clone()),
            }),
            KeyCode::Enter => Some(ConnectionsRequest::Capture(Arc::clone(&form))),
            KeyCode::Backspace => {
                if let Some(value) = editable(edit) {
                    value.pop();
                }
                None
            }
            KeyCode::Char(character) if !character.is_control() => {
                if let Some(value) = editable(edit)
                    && value.len() < 255
                {
                    value.push(character);
                }
                None
            }
            _ => None,
        };
        screen.page = ConnectionsPage::Form(form);
        if let Some(request) = request {
            self.start_connections(screen, request);
        } else {
            self.screen = Screen::Connections(screen);
        }
    }

    fn start_connections(&mut self, origin: ConnectionsScreen, request: ConnectionsRequest) {
        if self.connections_task.is_some()
            || self.management_task.is_some()
            || self.project_edit_task.is_some()
            || self.deployment_session.is_active()
        {
            self.message =
                Some("Wait for the active operation before managing connections.".into());
            return;
        }
        let Some(runtime) = self.runtime.clone() else {
            self.message = Some("Background runtime is unavailable.".into());
            return;
        };
        let service = self.connection_service();
        let setup = self.setup_service.clone();
        let home = self.home_directory.clone();
        let id = uuid::Uuid::now_v7();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let sender = self.background_sender.clone();
        let label = request.label();
        let spawned = std::thread::Builder::new()
            .name("shipforge-connections".into())
            .spawn(move || {
                let result = super::catch_worker_failure(|| {
                    runtime.block_on(worker::run(
                        &service,
                        &setup,
                        home.as_deref(),
                        request,
                        &worker_cancel,
                    ))
                });
                let _ = sender.send(BackgroundEvent::Connections(id, result));
            });
        match spawned {
            Ok(_) => {
                self.connections_task = Some(ConnectionsTask {
                    id,
                    origin,
                    cancellation,
                });
                self.screen = Screen::Connections(ConnectionsScreen {
                    page: ConnectionsPage::Loading {
                        label,
                        started: Instant::now(),
                        cancelling: false,
                    },
                    scroll: 0,
                });
            }
            Err(_) => {
                self.message =
                    Some("Could not start connection worker; nothing was started.".into());
            }
        }
    }

    pub(super) fn connection_service(&self) -> ConnectionManagementService {
        ConnectionManagementService::new(
            ManagementPaths {
                projects: self.registry_path.clone(),
                destinations: self.destination_registry_path.clone(),
                credentials: self.credential_registry_path.clone(),
                history: self
                    .destination_registry_path
                    .with_file_name("history.sqlite3"),
            },
            Arc::clone(&self.deployment_session),
            self.setup_service.clone(),
        )
    }

    pub(super) fn cancel_connections(&mut self) {
        if let Some(task) = &self.connections_task {
            task.cancellation.cancel();
            if let Screen::Connections(ConnectionsScreen {
                page: ConnectionsPage::Loading { cancelling, .. },
                ..
            }) = &mut self.screen
            {
                *cancelling = true;
            }
        }
    }

    pub(super) fn finish_connections(
        &mut self,
        id: uuid::Uuid,
        result: Result<ConnectionsPage, String>,
    ) {
        if self
            .connections_task
            .as_ref()
            .is_none_or(|task| task.id != id)
        {
            return;
        }
        let Some(task) = self.connections_task.take() else {
            return;
        };
        let mut screen = task.origin;
        match result {
            Ok(ConnectionsPage::ProjectRemoved(projects)) => {
                self.recent.clone_from(&projects);
                self.selected_recent = self.selected_recent.min(self.recent.len());
                self.screen = Screen::Projects;
                self.message = Some("Removed from recents only. Project files and deployment history are unchanged.".into());
                return;
            }
            Ok(page) => {
                screen.page = page;
                screen.scroll = 0;
            }
            Err(error) => self.message = Some(render::safe_text(&error)),
        }
        self.screen = Screen::Connections(screen);
    }
}

fn empty_screen() -> ConnectionsScreen {
    ConnectionsScreen {
        page: ConnectionsPage::List {
            items: Arc::new(Vec::new()),
            cursor: 0,
        },
        scroll: 0,
    }
}

fn keys_request(
    key: KeyCode,
    form: &Arc<ConnectionForm>,
    directory: &KeyDirectory,
    cursor: &mut usize,
) -> Option<ConnectionsRequest> {
    move_cursor(key, cursor, directory.entries.len());
    match key {
        KeyCode::Enter => directory
            .entries
            .get(*cursor)
            .filter(|entry| entry.directory)
            .map(|entry| ConnectionsRequest::Keys {
                form: Arc::clone(form),
                path: entry.path.clone(),
            }),
        KeyCode::Char('s') => directory
            .entries
            .get(*cursor)
            .filter(|entry| !entry.directory)
            .map(|entry| ConnectionsRequest::SelectKey {
                form: Arc::clone(form),
                path: entry.path.clone(),
            }),
        KeyCode::Backspace => directory
            .path
            .parent()
            .map(|parent| ConnectionsRequest::Keys {
                form: Arc::clone(form),
                path: parent.to_owned(),
            }),
        _ => None,
    }
}

fn editable(form: &mut ConnectionForm) -> Option<&mut String> {
    match form.field {
        SshField::Host => Some(&mut form.host),
        SshField::User => Some(&mut form.user),
        SshField::Port => Some(&mut form.port),
        SshField::Credential => None,
    }
}

const fn next_field(field: SshField) -> SshField {
    match field {
        SshField::Host => SshField::User,
        SshField::User => SshField::Port,
        SshField::Port => SshField::Credential,
        SshField::Credential => SshField::Host,
    }
}

fn next_host(form: &mut ConnectionForm) {
    if let Some(host) = form.hosts.get(form.host_cursor) {
        form.host
            .clone_from(host.hostname.as_ref().unwrap_or(&host.host));
        if let Some(user) = &host.user {
            form.user.clone_from(user);
        }
        form.port = host.port.unwrap_or(22).to_string();
        form.host_cursor = (form.host_cursor + 1) % form.hosts.len();
    }
}

fn move_cursor(key: KeyCode, cursor: &mut usize, count: usize) {
    *cursor = match key {
        KeyCode::Up => cursor.saturating_sub(1),
        KeyCode::Down => cursor.saturating_add(1).min(count.saturating_sub(1)),
        _ => *cursor,
    };
}

fn scroll(key: KeyCode, value: &mut u16) {
    *value = match key {
        KeyCode::PageUp => value.saturating_sub(10),
        KeyCode::PageDown => value.saturating_add(10),
        KeyCode::Home => 0,
        _ => *value,
    };
}

#[cfg(test)]
mod tests;
