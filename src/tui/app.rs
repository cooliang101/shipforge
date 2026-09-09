mod language;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::JoinHandle,
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod actions;
mod attention;
mod connections;
mod logs;
mod management;
mod navigation;
#[cfg(test)]
mod performance_tests;
mod project_edit;
mod reinitialize;
mod remote_target;
mod search;
mod setup_async;
mod setup_diagnostics;
#[cfg(test)]
mod setup_flow_tests;
mod setup_manual;
mod setup_save;
use attention::{AttentionRequest, AttentionState, LocalAttentionGateway, TuiAttentionGateway};
use remote_target::RemoteSetupSelectionState;

use crate::{
    application::{
        DeploymentPlan, DeploymentReport, DeploymentSelection, DeploymentService,
        DeploymentSession, DestinationSetupRequest, DestinationSetupService, EndpointProbeRequest,
        LocalIdentityCandidate, RemoteSetupCandidates, SetupCredential,
    },
    config::{
        CredentialRegistry, DestinationRegistry, DestinationSettings, DestinationSummary,
        EnvironmentSetup, HostKeyFingerprint, PreparedProjectInitialization, ProjectConfig,
        ProjectSetup, SshCandidate, SshCredential, TargetSetup, default_remote_root,
        discover_local_ssh, prepare_initialize,
    },
    domain::{ComponentName, DestinationKey, EnvironmentId, ProjectId},
    drivers::{CredentialHandle, DriverDestinationInput, DriverKind, DriverLog, EventSink},
    projects::{
        DiscoveryReport, ProjectRegistry, ProjectRegistryError, ProjectSelection, ProjectStatus,
        discover_components, register_initialized_project, select_project, suggest_project_name,
    },
};

#[derive(Clone, Debug)]
pub(super) enum Screen {
    Projects,
    Management(management::ManagementScreen),
    Connections(connections::ConnectionsScreen),
    ProjectEdit(project_edit::ProjectEditScreen),
    Browser(DirectoryBrowser),
    Overview {
        root: PathBuf,
        config: ProjectConfig,
    },
    DeploySelection(DeploySelectionState),
    DeploymentPlanning {
        request_id: uuid::Uuid,
        selection: DeploySelectionState,
        cancellation: tokio_util::sync::CancellationToken,
    },
    DeploymentReview {
        plan: DeploymentPlan,
        scroll: u16,
    },
    DeploymentRunning {
        root: PathBuf,
        config: ProjectConfig,
        cancellation: tokio_util::sync::CancellationToken,
        cancellation_requested: bool,
    },
    DeploymentFinished {
        root: PathBuf,
        config: ProjectConfig,
        summary: String,
        scroll: u16,
    },
    SetupComponents(ComponentSetupState),
    ManualComponent(setup_manual::ManualComponentScreen),
    Reinitialize(reinitialize::ReinitializeScreen),
    SetupDestinations(DestinationSetupState),
    NewSshDestination(NewSshDestinationState),
    HostKeyPending {
        request_id: uuid::Uuid,
        draft: NewSshDestinationState,
        cancellation_requested: bool,
    },
    HostKeyConfirm {
        draft: NewSshDestinationState,
        fingerprint: HostKeyFingerprint,
    },
    SshAuthenticationPending {
        request_id: uuid::Uuid,
        draft: NewSshDestinationState,
        fingerprint: HostKeyFingerprint,
        cancellation_requested: bool,
    },
    RemoteSetupSelection(RemoteSetupSelectionState),
    KeyBrowser {
        draft: NewSshDestinationState,
        browser: KeyFileBrowser,
    },
    SetupReview {
        destinations: DestinationSetupState,
        prepared: PreparedProjectInitialization,
        scroll: u16,
    },
}

#[derive(Clone, Debug)]
pub(super) struct DeploySelectionState {
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub environment_cursor: usize,
    pub component_cursor: usize,
    pub selected: BTreeSet<ComponentName>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SshField {
    Host,
    User,
    Port,
    Credential,
}

#[derive(Clone, Debug)]
pub(super) enum CredentialChoice {
    Password(crate::config::PasswordInput),
    Saved {
        handle: CredentialHandle,
        label: String,
    },
    Agent {
        fingerprint: String,
        label: String,
    },
    IdentityFile {
        path: PathBuf,
        label: String,
    },
}

impl CredentialChoice {
    pub fn label(&self) -> &str {
        match self {
            Self::Password(input) => input.label(),
            Self::Saved { label, .. }
            | Self::Agent { label, .. }
            | Self::IdentityFile { label, .. } => label,
        }
    }

    fn identity(&self) -> String {
        match self {
            Self::Password(_) => "password-input".into(),
            Self::Saved { handle, .. } => handle.expose_reference().into(),
            Self::Agent { fingerprint, .. } => format!("agent:{fingerprint}"),
            Self::IdentityFile { path, .. } => format!("file:{}", path.display()),
        }
    }
}

#[async_trait(?Send)]
trait TuiDeploymentGateway: std::fmt::Debug + Send + Sync {
    async fn plan(
        &self,
        selection: DeploymentSelection,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentPlan, String>;

    async fn execute(
        &self,
        plan: DeploymentPlan,
        events: &dyn EventSink,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentReport, String>;
}

#[derive(Debug)]
struct LocalDeploymentGateway {
    destinations: PathBuf,
    credentials: PathBuf,
    history: PathBuf,
}

#[async_trait(?Send)]
impl TuiDeploymentGateway for LocalDeploymentGateway {
    async fn plan(
        &self,
        selection: DeploymentSelection,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentPlan, String> {
        let destinations = DestinationRegistry::load(&self.destinations).map_err(|_| {
            "Saved connections could not be loaded. Check Connections and local file permissions.".to_owned()
        })?;
        let credentials = Arc::new(CredentialRegistry::load(&self.credentials).map_err(|_| {
            "SSH identities could not be loaded. Check the selected identity in Connections."
                .to_owned()
        })?);
        let drivers = crate::bootstrap::deployment_driver_registry(credentials).map_err(|_| {
            "Deployment support is unavailable in this application build.".to_owned()
        })?;
        DeploymentService::new(Arc::new(drivers), self.history.clone())
            .plan(selection, &destinations, cancellation)
            .await
            .map_err(crate::tui::deployment_error::deployment_error)
    }

    async fn execute(
        &self,
        plan: DeploymentPlan,
        events: &dyn EventSink,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentReport, String> {
        let credentials = Arc::new(CredentialRegistry::load(&self.credentials).map_err(|_| {
            "SSH identities could not be loaded. Check the selected identity in Connections."
                .to_owned()
        })?);
        let drivers = crate::bootstrap::deployment_driver_registry(credentials).map_err(|_| {
            "Deployment support is unavailable in this application build.".to_owned()
        })?;
        DeploymentService::new(Arc::new(drivers), self.history.clone())
            .execute(plan, &self.destinations, events, cancellation)
            .await
            .map_err(crate::tui::deployment_error::deployment_error)
    }
}

#[derive(Clone, Debug)]
pub(super) struct NewSshDestinationState {
    pub identity_request: Option<uuid::Uuid>,
    pub destinations: DestinationSetupState,
    pub connections: Vec<SshCandidate>,
    pub connection_cursor: usize,
    pub host: String,
    pub user: String,
    pub port: String,
    pub field: SshField,
    pub credentials: Vec<CredentialChoice>,
    pub credential_cursor: usize,
    pub agent_status: String,
}

impl NewSshDestinationState {
    fn selected_credential(&self) -> Option<&CredentialChoice> {
        self.credentials.get(self.credential_cursor)
    }

    fn apply_connection(&mut self, index: usize) {
        let Some(candidate) = self.connections.get(index) else {
            return;
        };
        self.connection_cursor = index;
        self.host = candidate
            .hostname
            .clone()
            .unwrap_or_else(|| candidate.host.clone());
        self.user = candidate.user.clone().unwrap_or_default();
        self.port = candidate.port.unwrap_or(22).to_string();
    }
}

#[derive(Debug)]
enum BackgroundEvent {
    Management(uuid::Uuid, Result<management::ManagementPage, String>),
    Connections(uuid::Uuid, Result<connections::ConnectionsPage, String>),
    ProjectEdit(uuid::Uuid, Result<project_edit::ProjectEditPage, String>),
    Reinitialize(uuid::Uuid, Result<reinitialize::ReinitializeResult, String>),
    RemoteTarget(
        uuid::Uuid,
        Result<remote_target::RemoteTargetResult, String>,
    ),
    LocalAttention(
        AttentionRequest,
        Result<crate::application::LocalAttentionSummary, String>,
    ),
    AgentIdentities(uuid::Uuid, Result<Vec<LocalIdentityCandidate>, String>),
    HostKey(uuid::Uuid, Result<String, String>),
    Authentication(uuid::Uuid, Result<RemoteSetupCandidates, String>),
    DeploymentPlan(uuid::Uuid, Result<DeploymentPlan, String>),
    DeploymentFinished(uuid::Uuid, Result<DeploymentReport, String>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tui) struct BackgroundPoll {
    changed: bool,
    frame_boundary: bool,
}

impl BackgroundPoll {
    pub(in crate::tui) fn changed(self) -> bool {
        self.changed
    }

    pub(in crate::tui) fn requires_frame_boundary(self) -> bool {
        self.frame_boundary
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tui) enum ExitState {
    #[default]
    Running,
    Confirm,
    Waiting,
}

#[derive(Debug)]
struct DeploymentTask {
    id: uuid::Uuid,
    cancellation: tokio_util::sync::CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl DeploymentTask {
    fn cancel(&self) {
        self.cancellation.cancel();
    }

    fn join(mut self) -> bool {
        self.worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
    }
}

#[derive(Clone, Debug)]
pub(super) struct ComponentSetupState {
    pub root: PathBuf,
    pub report: DiscoveryReport,
    pub selected: std::collections::BTreeSet<ComponentName>,
    pub cursor: usize,
}

#[derive(Clone, Debug)]
pub(super) struct DestinationSetupState {
    pub components: ComponentSetupState,
    pub destinations: Vec<DestinationSummary>,
    pub assignments: std::collections::BTreeMap<ComponentName, DestinationKey>,
    pub target_settings: std::collections::BTreeMap<ComponentName, ComponentTargetSettings>,
    pub component_cursor: usize,
    pub destination_cursor: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ComponentTargetSettings {
    pub root: Option<String>,
    pub service: Option<crate::config::ServiceConfig>,
}

#[derive(Clone, Debug)]
pub(super) struct DirectoryBrowser {
    pub directory: PathBuf,
    pub children: Vec<PathBuf>,
    pub selected: usize,
}

#[derive(Clone, Debug)]
pub(super) struct KeyFileBrowser {
    pub directory: PathBuf,
    pub entries: Vec<PathBuf>,
    pub selected: usize,
    pub directories: BTreeSet<PathBuf>,
}

impl KeyFileBrowser {
    fn open(directory: &Path) -> Result<Self, std::io::Error> {
        let directory = std::fs::canonicalize(directory)?;
        let mut entries = Vec::new();
        let mut directories = BTreeSet::new();
        for (index, entry) in std::fs::read_dir(&directory)?.take(4097).enumerate() {
            if index == 4096 {
                return Err(std::io::Error::other(
                    "Directory has too many entries to browse safely",
                ));
            }
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.insert(path.clone());
            }
            entries.push(path);
        }
        entries.sort_by_cached_key(|path| {
            (
                !directories.contains(path),
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
                    .unwrap_or_default(),
            )
        });
        Ok(Self {
            directory,
            entries,
            selected: 0,
            directories,
        })
    }

    fn selected_entry(&self) -> Option<&Path> {
        self.entries.get(self.selected).map(PathBuf::as_path)
    }
}

impl DirectoryBrowser {
    fn open(directory: &Path) -> Result<Self, ProjectRegistryError> {
        let directory = std::fs::canonicalize(directory)
            .map_err(|source| ProjectRegistryError::browser(directory, source))?;
        let mut children = Vec::new();
        let entries = std::fs::read_dir(&directory)
            .map_err(|source| ProjectRegistryError::browser(&directory, source))?;
        for (index, entry) in entries.take(4097).enumerate() {
            if index == 4096 {
                return Err(ProjectRegistryError::browser(
                    &directory,
                    std::io::Error::other("Directory has too many entries to browse safely"),
                ));
            }
            let entry =
                entry.map_err(|source| ProjectRegistryError::browser(&directory, source))?;
            let kind = entry
                .file_type()
                .map_err(|source| ProjectRegistryError::browser(&directory, source))?;
            if kind.is_dir() {
                children.push(entry.path());
            }
        }
        children.sort_by_cached_key(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        Ok(Self {
            directory,
            children,
            selected: 0,
        })
    }

    fn selected_child(&self) -> Option<&Path> {
        self.children.get(self.selected).map(PathBuf::as_path)
    }
}

#[derive(Debug)]
pub(super) struct App {
    pub screen: Screen,
    pub recent: Vec<ProjectStatus>,
    pub recent_unavailable: bool,
    pub selected_recent: usize,
    pub message: Option<String>,
    pub deployment_session: Arc<DeploymentSession>,
    registry_path: PathBuf,
    destination_registry_path: PathBuf,
    credential_registry_path: PathBuf,
    initial_directory: PathBuf,
    home_directory: Option<PathBuf>,
    runtime: Option<tokio::runtime::Handle>,
    background_sender: SyncSender<BackgroundEvent>,
    background_receiver: Receiver<BackgroundEvent>,
    setup_service: DestinationSetupService,
    setup_task: Option<setup_async::SetupTask>,
    deployment_plan_task: Option<DeploymentTask>,
    deployment_execution_task: Option<DeploymentTask>,
    deployment_gateway: Arc<dyn TuiDeploymentGateway>,
    attention_gateway: Arc<dyn TuiAttentionGateway>,
    attention: AttentionState,
    management_gateway: Arc<dyn management::ManagementGateway>,
    management_task: Option<management::ManagementTask>,
    connections_task: Option<connections::ConnectionsTask>,
    project_edit_gateway: Arc<dyn project_edit::ProjectEditGateway>,
    project_edit_task: Option<project_edit::ProjectEditTask>,
    reinitialize_task: Option<reinitialize::ReinitializeTask>,
    remote_target_task: Option<remote_target::RemoteTargetTask>,
    navigation: navigation::ProjectNavigation,
    action_cursor: Option<usize>,
    pub picker: Option<crate::tui::picker::Picker>,
    pub language: crate::tui::i18n::Language,
    pub language_menu: Option<crate::tui::i18n::Language>,
    pub help_open: bool,
    pub help_scroll: u16,
    pub live_progress: Option<crate::tui::live_progress::LiveProgress>,
    pub live_logs: crate::tui::log_view::LogView,
    live_environment: Option<(ProjectId, EnvironmentId)>,
    pub log_workspace: Option<logs::LogWorkspace>,
    pending_clipboard: Option<String>,
    exit_state: ExitState,
    project_exit_armed_at: Option<std::time::Instant>,
}

impl App {
    pub fn new(
        registry_path: PathBuf,
        destination_registry_path: PathBuf,
        initial_directory: &Path,
    ) -> Result<Self, ProjectRegistryError> {
        Self::new_with_setup(
            registry_path,
            destination_registry_path,
            initial_directory,
            crate::bootstrap::destination_setup_service(),
        )
    }

    fn new_with_setup(
        registry_path: PathBuf,
        destination_registry_path: PathBuf,
        initial_directory: &Path,
        setup_service: DestinationSetupService,
    ) -> Result<Self, ProjectRegistryError> {
        let credential_path = destination_registry_path.with_file_name("credentials.yaml");
        let deployment_gateway = Arc::new(LocalDeploymentGateway {
            destinations: destination_registry_path.clone(),
            credentials: credential_path,
            history: destination_registry_path.with_file_name("history.sqlite3"),
        });
        let attention_gateway = Arc::new(LocalAttentionGateway {
            history: destination_registry_path.with_file_name("history.sqlite3"),
        });
        Self::new_with_services(
            registry_path,
            destination_registry_path,
            initial_directory,
            setup_service,
            deployment_gateway,
            attention_gateway,
        )
    }

    fn new_with_services(
        registry_path: PathBuf,
        destination_registry_path: PathBuf,
        initial_directory: &Path,
        setup_service: DestinationSetupService,
        deployment_gateway: Arc<dyn TuiDeploymentGateway>,
        attention_gateway: Arc<dyn TuiAttentionGateway>,
    ) -> Result<Self, ProjectRegistryError> {
        let initial_directory = std::fs::canonicalize(initial_directory)
            .map_err(|source| ProjectRegistryError::browser(initial_directory, source))?;
        let (recent, recent_unavailable) = match ProjectRegistry::load(&registry_path) {
            Ok(registry) => (registry.statuses(), false),
            Err(_) => (Vec::new(), true),
        };
        let credential_registry_path = destination_registry_path.with_file_name("credentials.yaml");
        let (background_sender, background_receiver) = mpsc::sync_channel(256);
        let deployment_session = Arc::new(DeploymentSession::default());
        let management_gateway = Arc::new(management::LocalManagementGateway {
            destinations: destination_registry_path.clone(),
            credentials: credential_registry_path.clone(),
            history: destination_registry_path.with_file_name("history.sqlite3"),
            session: Arc::clone(&deployment_session),
        });
        let project_edit_gateway = Arc::new(project_edit::LocalProjectEditGateway {
            destinations: destination_registry_path.clone(),
            session: Arc::clone(&deployment_session),
        });
        let language = crate::tui::i18n::Language::load(&registry_path.with_file_name("ui.json"));
        let mut app = Self {
            language: language.unwrap_or_default(),
            language_menu: None,
            screen: Screen::Projects,
            recent,
            recent_unavailable,
            selected_recent: 0,
            message: recent_unavailable.then(|| "Recent projects could not be loaded. The registry was not changed; f retries, or o opens directory browsing.".into()),
            deployment_session,
            registry_path,
            destination_registry_path,
            credential_registry_path,
            initial_directory,
            home_directory: crate::adapters::user_home_directory().ok(),
            runtime: tokio::runtime::Handle::try_current().ok(),
            background_sender,
            background_receiver,
            setup_service,
            setup_task: None,
            deployment_plan_task: None,
            deployment_execution_task: None,
            deployment_gateway,
            attention_gateway,
            attention: AttentionState::default(),
            management_gateway,
            management_task: None,
            connections_task: None,
            project_edit_gateway,
            project_edit_task: None,
            reinitialize_task: None,
            remote_target_task: None,
            navigation: navigation::ProjectNavigation::default(),
            action_cursor: None,
            picker: None,
            help_open: false,
            help_scroll: 0,
            live_progress: None,
            live_logs: crate::tui::log_view::LogView::default(),
            live_environment: None,
            log_workspace: None,
            pending_clipboard: None,
            exit_state: ExitState::Running,
            project_exit_armed_at: None,
        };
        if language.is_err() {
            app.message = Some(
                "Language preferences could not be read; using English. F6 opens language settings."
                    .into(),
            );
        }
        app.refresh_attention(None);
        Ok(app)
    }

    pub(in crate::tui) fn poll_background(&mut self) -> BackgroundPoll {
        let mut poll = BackgroundPoll::default();
        for _ in 0..256 {
            let Ok(event) = self.background_receiver.try_recv() else {
                break;
            };
            poll.changed = true;
            poll.frame_boundary = true;
            match event {
                BackgroundEvent::Management(id, result) => self.finish_management(id, result),
                BackgroundEvent::Connections(id, result) => self.finish_connections(id, result),
                BackgroundEvent::ProjectEdit(id, result) => self.finish_project_edit(id, result),
                BackgroundEvent::Reinitialize(id, result) => self.finish_reinitialize(id, result),
                BackgroundEvent::RemoteTarget(id, result) => self.finish_remote_target(id, result),
                BackgroundEvent::LocalAttention(request, result) => {
                    self.finish_attention(&request, result);
                }
                BackgroundEvent::AgentIdentities(id, result) => {
                    self.finish_setup_identities(id, result);
                }
                BackgroundEvent::HostKey(id, result) => self.finish_setup_host_key(id, result),
                BackgroundEvent::Authentication(id, result) => {
                    self.finish_setup_authentication(id, result);
                }
                BackgroundEvent::DeploymentPlan(completed_id, result) => {
                    self.finish_deployment_plan(completed_id, result);
                }
                BackgroundEvent::DeploymentFinished(id, result) => {
                    self.finish_deployment(id, result);
                }
            }
        }
        poll.changed |= self.poll_live_logs();
        let log_workspace_changed = self.poll_log_workspace();
        poll.changed |= log_workspace_changed;
        poll.frame_boundary |= log_workspace_changed;
        poll
    }

    fn finish_deployment_plan(
        &mut self,
        completed_id: uuid::Uuid,
        mut result: Result<DeploymentPlan, String>,
    ) {
        if self
            .deployment_plan_task
            .as_ref()
            .is_none_or(|task| task.id != completed_id)
        {
            return;
        }
        let task = self
            .deployment_plan_task
            .take()
            .expect("matched Deployment planning task");
        let cancelled = task.cancellation.is_cancelled();
        if task.join() {
            result = Err(
                "Deployment check worker stopped unexpectedly. No completed plan was accepted."
                    .into(),
            );
        }
        let Screen::DeploymentPlanning {
            request_id,
            selection,
            ..
        } = self.screen.clone()
        else {
            return;
        };
        if completed_id != request_id {
            return;
        }
        self.action_cursor = Some(0);
        if cancelled {
            self.screen = Screen::DeploySelection(selection);
            self.message = Some("Deployment check cancelled. The completed plan was discarded; review the selection before checking again.".into());
            return;
        }
        match result {
            Ok(plan) => self.screen = Screen::DeploymentReview { plan, scroll: 0 },
            Err(error) => {
                self.screen = Screen::DeploySelection(selection);
                self.message = Some(error);
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(self.screen, Screen::Projects)
            || key.code != KeyCode::Esc
            || !key.modifiers.is_empty()
            || self.help_open
            || self.picker.is_some()
            || self.log_workspace.is_some()
            || self.language_menu.is_some()
        {
            self.project_exit_armed_at = None;
        }
        if let Some(should_exit) = self.handle_modal_key(key) {
            return should_exit;
        }
        // In raw mode Ctrl+C is an input event, not a process signal. Never
        // let modified shortcuts fall through to plain confirmation keys.
        if self.guard_key(key) {
            return false;
        }
        if self.handle_search_key(key) {
            self.action_cursor = None;
            return false;
        }
        self.message = None;
        let Some(key) = self.action_key(key) else {
            return false;
        };
        // Dispatch latency-sensitive pages before the fallback clone. Their
        // config/plan may be large, and log navigation must stay allocation-bounded.
        if matches!(self.screen, Screen::DeploymentRunning { .. }) {
            self.handle_deployment_running_key(key.code);
            return false;
        }
        if matches!(self.screen, Screen::DeploymentFinished { .. }) {
            self.handle_deployment_finished_key(key.code);
            return false;
        }
        if matches!(self.screen, Screen::DeploymentReview { .. }) {
            self.handle_deployment_review_key(key.code);
            return false;
        }
        if matches!(self.screen, Screen::Overview { .. }) {
            return self.handle_overview_key(key.code);
        }
        if matches!(self.screen, Screen::DeploySelection(_)) {
            self.handle_deploy_selection_key(key.code);
            return false;
        }
        match self.screen.clone() {
            Screen::Management(screen) => self.handle_management(key.code, screen),
            Screen::Connections(screen) => self.handle_connections(key.code, screen),
            Screen::ProjectEdit(screen) => self.handle_project_edit(key.code, screen),
            Screen::Reinitialize(screen) => self.handle_reinitialize(key.code, screen),
            Screen::Projects => {
                if key.code == KeyCode::Char('q') {
                    return true;
                }
                self.handle_projects(key.code);
            }
            Screen::Browser(browser) => self.handle_browser(key.code, &browser),
            Screen::SetupComponents(setup) => self.handle_setup_components(key.code, &setup),
            Screen::ManualComponent(mut screen) => {
                self.screen = screen
                    .handle_key(key)
                    .map_or_else(|| Screen::ManualComponent(screen), Screen::SetupComponents);
            }
            Screen::SetupDestinations(setup) => self.handle_setup_destinations(key.code, &setup),
            Screen::NewSshDestination(draft) => {
                self.handle_new_ssh_destination(key.code, &draft);
            }
            Screen::HostKeyPending { .. } | Screen::SshAuthenticationPending { .. } => {
                self.cancel_setup_on_escape(key.code);
            }
            Screen::HostKeyConfirm { draft, fingerprint } => {
                self.handle_host_key_confirm(key.code, &draft, &fingerprint);
            }
            Screen::RemoteSetupSelection(selection) => {
                self.handle_remote_setup_selection(key, selection);
            }
            Screen::KeyBrowser { draft, browser } => {
                self.handle_key_browser(key.code, &draft, &browser);
            }
            Screen::SetupReview {
                destinations,
                prepared,
                ..
            } => self.handle_setup_review(key.code, &destinations, &prepared),
            Screen::Overview { .. } | Screen::DeploySelection(_) => {
                unreachable!("project pages are dispatched before cloning")
            }
            Screen::DeploymentPlanning { cancellation, .. } => {
                if key.code == KeyCode::Esc {
                    cancellation.cancel();
                }
            }
            Screen::DeploymentReview { .. }
            | Screen::DeploymentRunning { .. }
            | Screen::DeploymentFinished { .. } => {
                unreachable!("large deployment pages are dispatched before cloning")
            }
        }
        false
    }

    fn handle_deployment_running_key(&mut self, key: KeyCode) {
        if key == KeyCode::Esc {
            self.request_deployment_cancellation();
        } else if key == KeyCode::Char('l') {
            self.open_live_logs();
        }
    }

    fn handle_overview_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Esc => self.show_projects(),
            KeyCode::Char('q') => return true,
            KeyCode::Char('d') => match std::mem::replace(&mut self.screen, Screen::Projects) {
                Screen::Overview { root, config } => self.open_deployment(root, config),
                screen => self.screen = screen,
            },
            KeyCode::Char('m') => {
                if let Screen::Overview { root, config } = &self.screen {
                    self.open_management(root.clone(), config.clone());
                }
            }
            KeyCode::Char('e') => {
                if let Screen::Overview { root, .. } = &self.screen {
                    self.open_project_edit(root.clone());
                }
            }
            KeyCode::Left | KeyCode::Right => {
                self.move_overview_environment(key == KeyCode::Right);
            }
            _ => self.scroll_overview(key),
        }
        false
    }

    fn handle_deploy_selection_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Left | KeyCode::Right => {
                let Some((cursor, count)) = (match &self.screen {
                    Screen::DeploySelection(selection) => Some((
                        selection.environment_cursor,
                        selection.config.environments.len(),
                    )),
                    _ => None,
                }) else {
                    return;
                };
                let cursor = if key == KeyCode::Right {
                    cursor.saturating_add(1).min(count.saturating_sub(1))
                } else {
                    cursor.saturating_sub(1)
                };
                self.select_environment(cursor);
            }
            KeyCode::Up => self.move_deploy_component(false),
            KeyCode::Down => self.move_deploy_component(true),
            KeyCode::Char(' ') => {
                let component = match &self.screen {
                    Screen::DeploySelection(selection) => selection
                        .config
                        .environments
                        .values()
                        .nth(selection.environment_cursor)
                        .and_then(|environment| {
                            environment
                                .components
                                .keys()
                                .nth(selection.component_cursor)
                        })
                        .cloned(),
                    _ => None,
                };
                if let (Some(component), Screen::DeploySelection(selection)) =
                    (component, &mut self.screen)
                    && !selection.selected.remove(&component)
                {
                    selection.selected.insert(component);
                }
            }
            KeyCode::Enter => {
                let selection = match &self.screen {
                    Screen::DeploySelection(selection) => Some(selection.clone()),
                    _ => None,
                };
                if let Some(selection) = selection {
                    self.start_deployment_plan(&selection);
                }
            }
            KeyCode::Esc => match std::mem::replace(&mut self.screen, Screen::Projects) {
                Screen::DeploySelection(selection) => {
                    self.screen = Screen::Overview {
                        root: selection.root,
                        config: selection.config,
                    };
                }
                screen => self.screen = screen,
            },
            _ => {}
        }
    }

    fn handle_deployment_finished_key(&mut self, key: KeyCode) {
        if key == KeyCode::Char('l') {
            self.open_live_logs();
            return;
        }
        if matches!(key, KeyCode::Esc | KeyCode::Enter) {
            match std::mem::replace(&mut self.screen, Screen::Projects) {
                Screen::DeploymentFinished { root, config, .. } => {
                    self.show_overview(root, config);
                }
                screen => self.screen = screen,
            }
            return;
        }
        if let Screen::DeploymentFinished { scroll, .. } = &mut self.screen {
            *scroll = match key {
                KeyCode::Up => scroll.saturating_sub(1),
                KeyCode::Down => scroll.saturating_add(1),
                KeyCode::PageUp => scroll.saturating_sub(10),
                KeyCode::PageDown => scroll.saturating_add(10),
                _ => *scroll,
            };
        }
    }

    fn handle_deployment_review_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up => self.adjust_review_scroll(false, 1),
            KeyCode::Down => self.adjust_review_scroll(true, 1),
            KeyCode::PageUp => self.adjust_review_scroll(false, 10),
            KeyCode::PageDown => self.adjust_review_scroll(true, 10),
            KeyCode::Char('c') => {
                if let Screen::DeploymentReview { plan, .. } = &self.screen {
                    self.start_deployment(plan.clone());
                }
            }
            KeyCode::Esc => {
                if let Screen::DeploymentReview { plan, .. } = &self.screen {
                    self.screen = Screen::DeploySelection(selection_state(&plan.selection));
                    self.action_cursor = Some(0);
                }
            }
            _ => {}
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Option<bool> {
        // An open exit dialog is topmost; otherwise text overlays own ordinary `q` input.
        if self.exit_state != ExitState::Running {
            return self.handle_exit_key(key);
        }
        if self.handle_overlay_key(key) {
            Some(false)
        } else {
            self.handle_exit_key(key)
        }
    }

    fn handle_exit_key(&mut self, key: KeyEvent) -> Option<bool> {
        match self.exit_state {
            ExitState::Confirm => {
                if key.modifiers.is_empty() && matches!(key.code, KeyCode::Esc | KeyCode::Char('r'))
                {
                    self.exit_state = ExitState::Running;
                } else if (key.modifiers.is_empty() && key.code == KeyCode::Char('c'))
                    || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c'))
                {
                    self.request_process_exit();
                }
                return Some(false);
            }
            ExitState::Waiting => {
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    self.request_deployment_cancellation();
                }
                return Some(false);
            }
            ExitState::Running => {}
        }

        if let Some(result) = self.handle_projects_exit(key) {
            return Some(result);
        }

        if key.modifiers.is_empty()
            && key.code == KeyCode::Char('q')
            && !matches!(&self.screen, Screen::NewSshDestination(draft)
                if draft.field == SshField::Credential && matches!(draft.selected_credential(), Some(CredentialChoice::Password(_))))
            && (matches!(self.screen, Screen::Projects | Screen::Overview { .. })
                || self.interactive_work_active())
        {
            if self.interactive_work_active() {
                self.exit_state = ExitState::Confirm;
                return Some(false);
            }
            return Some(true);
        }
        None
    }

    fn interactive_work_active(&self) -> bool {
        self.deployment_plan_task.is_some()
            || self.deployment_execution_task.is_some()
            || self.management_task.is_some()
            || self.connections_task.is_some()
            || self.project_edit_task.is_some()
            || self.reinitialize_task.is_some()
            || self.remote_target_task.is_some()
            || self.setup_task.is_some()
            || self.log_workspace_busy()
            || self.deployment_session.is_active()
    }

    fn tracked_work_active(&self) -> bool {
        self.interactive_work_active() || self.attention_busy()
    }

    pub(in crate::tui) fn needs_periodic_redraw(&self) -> bool {
        self.exit_state == ExitState::Running
            && (matches!(self.screen, Screen::DeploymentRunning { .. })
                || (self.management_task.is_some() && self.management_has_live_progress()))
    }

    pub(in crate::tui) fn exit_state(&self) -> ExitState {
        self.exit_state
    }

    pub(in crate::tui) fn exit_ready(&self) -> bool {
        self.exit_state == ExitState::Waiting && !self.tracked_work_active()
    }

    pub(in crate::tui) fn request_process_exit(&mut self) {
        self.exit_state = ExitState::Waiting;
        self.cancel_attention_for_shutdown();
        self.request_deployment_cancellation();
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        if self.handle_language_key(key) {
            return true;
        }
        if key.code == KeyCode::F(1) && key.modifiers.is_empty() {
            self.help_open = !self.help_open;
            self.help_scroll = 0;
            return true;
        }
        if self.help_open {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                self.request_deployment_cancellation();
            } else if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Esc => self.help_open = false,
                    KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                    KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                    KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                    KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(10),
                    KeyCode::Home => self.help_scroll = 0,
                    _ => {}
                }
            }
            return true;
        }
        if self.picker.is_some() {
            self.handle_search_key(key);
            return true;
        }
        if self.log_workspace.is_some() {
            self.handle_log_key(key);
            return true;
        }
        if key.code == KeyCode::Char('l')
            && key.modifiers.is_empty()
            && self.management_has_live_progress()
        {
            self.open_live_logs();
            return true;
        }
        false
    }

    fn guard_key(&mut self, key: KeyEvent) -> bool {
        if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                self.request_deployment_cancellation();
            }
            return true;
        }
        // Shift remains available for text fields, never for confirmation.
        if !key.modifiers.is_empty() && self.requires_plain_confirmation() {
            return true;
        }
        if self.management_task.is_some()
            || self.connections_task.is_some()
            || self.project_edit_task.is_some()
            || self.reinitialize_task.is_some()
            || self.remote_target_task.is_some()
            || self.setup_task.is_some()
        {
            if key.code == KeyCode::Esc {
                self.request_deployment_cancellation();
            }
            return true;
        }
        false
    }

    fn requires_plain_confirmation(&self) -> bool {
        match &self.screen {
            Screen::SetupReview { .. }
            | Screen::DeploymentReview { .. }
            | Screen::HostKeyConfirm { .. } => true,
            Screen::Management(screen) => screen.requires_plain_confirmation(),
            Screen::Connections(screen) => screen.requires_plain_confirmation(),
            Screen::ProjectEdit(screen) => screen.requires_plain_confirmation(),
            Screen::ManualComponent(screen) => screen.requires_plain_confirmation(),
            Screen::Reinitialize(screen) => screen.requires_plain_confirmation(),
            Screen::RemoteSetupSelection(screen) => screen.requires_plain_confirmation(),
            _ => false,
        }
    }

    fn request_deployment_cancellation(&mut self) {
        self.cancel_log_operation();
        self.cancel_management();
        self.cancel_connections();
        self.cancel_project_edit();
        self.cancel_reinitialize();
        self.cancel_remote_target();
        self.cancel_setup();
        if let Some(task) = &self.deployment_plan_task {
            task.cancel();
        }
        if let Some(task) = &self.deployment_execution_task {
            task.cancel();
        }
        if let Screen::DeploymentPlanning { cancellation, .. } = &self.screen {
            cancellation.cancel();
        }
        if let Screen::DeploymentRunning {
            cancellation,
            cancellation_requested,
            ..
        } = &mut self.screen
        {
            cancellation.cancel();
            *cancellation_requested = true;
        }
    }

    pub fn shutdown(&mut self) {
        self.request_process_exit();
        while self.tracked_work_active() {
            self.poll_background();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn open_deployment(&mut self, root: PathBuf, config: ProjectConfig) {
        self.refresh_destination_labels();
        let environment = self.preferred_environment(&config);
        let environment_cursor = config
            .environments
            .keys()
            .position(|name| Some(name) == environment.as_ref())
            .unwrap_or(0);
        if let Some(environment) = &environment {
            self.remember_environment(&config, environment);
        }
        let selected = environment
            .as_ref()
            .and_then(|name| config.environments.get(name))
            .map(|environment| environment.components.keys().cloned().collect())
            .unwrap_or_default();
        self.screen = Screen::DeploySelection(DeploySelectionState {
            root,
            config,
            environment_cursor,
            component_cursor: 0,
            selected,
        });
        self.action_cursor = Some(0);
    }

    fn move_deploy_component(&mut self, forward: bool) {
        let count = match &self.screen {
            Screen::DeploySelection(selection) => deployment_components(selection).len(),
            _ => return,
        };
        if let Screen::DeploySelection(selection) = &mut self.screen {
            selection.component_cursor = if forward {
                (selection.component_cursor + 1).min(count.saturating_sub(1))
            } else {
                selection.component_cursor.saturating_sub(1)
            };
        }
    }

    fn select_environment(&mut self, cursor: usize) {
        let selected_environment = if let Screen::DeploySelection(selection) = &mut self.screen {
            if selection.environment_cursor == cursor {
                return;
            }
            selection.environment_cursor = cursor;
            selection.component_cursor = 0;
            selection.selected = selection
                .config
                .environments
                .values()
                .nth(cursor)
                .map(|environment| environment.components.keys().cloned().collect())
                .unwrap_or_default();
            selection
                .config
                .environments
                .values()
                .nth(cursor)
                .map(|environment| (selection.config.project_id.clone(), environment.id.clone()))
        } else {
            None
        };
        if let Some((project, environment)) = selected_environment {
            self.remember_environment_ids(project, environment);
        }
    }

    fn start_deployment_plan(&mut self, selection: &DeploySelectionState) {
        if selection.selected.is_empty() {
            self.message = Some("Select at least one Component".into());
            return;
        }
        let environment = environment_names(selection)
            .get(selection.environment_cursor)
            .cloned();
        let Some(environment) = environment else {
            self.message = Some("Project has no deployable Environment".into());
            return;
        };
        self.spawn_deployment_plan(selection, environment);
    }

    fn spawn_deployment_plan(&mut self, selection: &DeploySelectionState, environment: String) {
        let Some(runtime) = self.runtime.clone() else {
            self.message = Some("Deployment runtime is unavailable".into());
            return;
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let request = DeploymentSelection {
            project_root: selection.root.clone(),
            config: selection.config.clone(),
            environment,
            components: selection.selected.clone(),
        };
        let request_id = uuid::Uuid::now_v7();
        let worker = match spawn_plan_thread(
            request_id,
            runtime,
            Arc::clone(&self.deployment_gateway),
            request,
            cancellation.clone(),
            self.background_sender.clone(),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                self.message = Some(format!("Could not start Deployment check: {error}"));
                return;
            }
        };
        self.deployment_plan_task = Some(DeploymentTask {
            id: request_id,
            cancellation: cancellation.clone(),
            worker: Some(worker),
        });
        self.screen = Screen::DeploymentPlanning {
            request_id,
            selection: selection.clone(),
            cancellation,
        };
    }

    fn adjust_review_scroll(&mut self, forward: bool, amount: u16) {
        if let Screen::DeploymentReview { scroll, .. } = &mut self.screen {
            *scroll = if forward {
                scroll.saturating_add(amount)
            } else {
                scroll.saturating_sub(amount)
            };
        }
    }

    fn start_deployment(&mut self, plan: DeploymentPlan) {
        let Some(runtime) = self.runtime.clone() else {
            self.message = Some("Deployment runtime is unavailable".into());
            return;
        };
        let root = plan.selection.project_root.clone();
        let config = plan.selection.config.clone();
        let live_environment = config
            .environments
            .get(&plan.selection.environment)
            .map(|environment| (config.project_id.clone(), environment.id.clone()));
        let progress = crate::tui::live_progress::LiveProgress::default();
        self.remember_environment(&config, &plan.selection.environment);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let request_id = uuid::Uuid::now_v7();
        let worker = match spawn_execute_thread(DeploymentExecutionWorker {
            request_id,
            runtime,
            gateway: Arc::clone(&self.deployment_gateway),
            session: Arc::clone(&self.deployment_session),
            plan,
            cancellation: cancellation.clone(),
            sender: self.background_sender.clone(),
            progress: progress.clone(),
        }) {
            Ok(worker) => worker,
            Err(error) => {
                self.message = Some(format!("Could not start Deployment: {error}"));
                return;
            }
        };
        self.deployment_execution_task = Some(DeploymentTask {
            id: request_id,
            cancellation: cancellation.clone(),
            worker: Some(worker),
        });
        self.live_environment = live_environment;
        self.live_progress = Some(progress);
        self.live_logs = crate::tui::log_view::LogView::default();
        self.screen = Screen::DeploymentRunning {
            root,
            config,
            cancellation,
            cancellation_requested: false,
        };
        self.invalidate_attention();
    }

    fn finish_deployment(
        &mut self,
        completed_id: uuid::Uuid,
        mut result: Result<DeploymentReport, String>,
    ) {
        if self
            .deployment_execution_task
            .as_ref()
            .is_none_or(|task| task.id != completed_id)
        {
            return;
        }
        let task = self
            .deployment_execution_task
            .take()
            .expect("matched Deployment execution task");
        if task.join() {
            match &mut result {
                Ok(report) => report.warnings.push(
                    "Deployment worker cleanup stopped unexpectedly after reporting an outcome; inspect recorded and remote state before retrying."
                        .into(),
                ),
                Err(error) => {
                    *error = "Deployment worker stopped unexpectedly. Remote state may be incomplete; inspect deployment history and remote state before retrying.".into();
                }
            }
        }
        if let Some(progress) = &self.live_progress {
            progress.finish();
        }
        self.poll_live_logs();
        let (root, config) = match std::mem::replace(&mut self.screen, Screen::Projects) {
            Screen::DeploymentRunning { root, config, .. } => (root, config),
            screen => {
                self.screen = screen;
                return;
            }
        };
        let summary = match result {
            Ok(report) => deployment_summary(&report),
            Err(error) => format!("Deployment did not complete: {error}"),
        };
        let project = config.project_id.clone();
        self.screen = Screen::DeploymentFinished {
            root,
            config,
            summary,
            scroll: 0,
        };
        self.refresh_attention(Some(project));
    }

    fn handle_setup_components(&mut self, key: KeyCode, setup: &ComponentSetupState) {
        match key {
            KeyCode::Char('a') => {
                self.screen = Screen::ManualComponent(setup_manual::ManualComponentScreen::new(
                    setup.clone(),
                ));
            }
            KeyCode::Char('r') if setup.report.components.is_empty() => {
                self.select_root(&setup.root);
            }
            KeyCode::Up => {
                if let Screen::SetupComponents(current) = &mut self.screen {
                    current.cursor = current.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Screen::SetupComponents(current) = &mut self.screen {
                    current.cursor =
                        (current.cursor + 1).min(current.report.components.len().saturating_sub(1));
                }
            }
            KeyCode::Char(' ') => {
                if let Some(candidate) = setup.report.components.get(setup.cursor)
                    && let Screen::SetupComponents(current) = &mut self.screen
                    && !current.selected.remove(&candidate.name)
                {
                    current.selected.insert(candidate.name.clone());
                }
            }
            KeyCode::Enter => {
                if setup.selected.is_empty() {
                    self.message = Some("Select at least one Component".into());
                    return;
                }
                match DestinationRegistry::load(&self.destination_registry_path) {
                    Ok(registry) => {
                        self.screen = Screen::SetupDestinations(DestinationSetupState {
                            components: setup.clone(),
                            destinations: registry.summaries(),
                            assignments: std::collections::BTreeMap::new(),
                            target_settings: std::collections::BTreeMap::new(),
                            component_cursor: 0,
                            destination_cursor: 0,
                        });
                    }
                    Err(_) => self.message = Some("Saved connections could not be loaded. Check Connections and local file permissions, then retry.".into()),
                }
            }
            KeyCode::Esc => self.show_projects(),
            _ => {}
        }
    }

    fn handle_setup_destinations(&mut self, key: KeyCode, setup: &DestinationSetupState) {
        let components = setup
            .components
            .report
            .components
            .iter()
            .filter(|candidate| setup.components.selected.contains(&candidate.name))
            .map(|candidate| candidate.name.clone())
            .collect::<Vec<_>>();
        match key {
            KeyCode::Left => {
                if let Screen::SetupDestinations(current) = &mut self.screen {
                    current.component_cursor = current.component_cursor.saturating_sub(1);
                }
            }
            KeyCode::Right => {
                if let Screen::SetupDestinations(current) = &mut self.screen {
                    current.component_cursor =
                        (current.component_cursor + 1).min(components.len().saturating_sub(1));
                }
            }
            KeyCode::Up => {
                if let Screen::SetupDestinations(current) = &mut self.screen {
                    current.destination_cursor = current.destination_cursor.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Screen::SetupDestinations(current) = &mut self.screen {
                    current.destination_cursor = (current.destination_cursor + 1)
                        .min(current.destinations.len().saturating_sub(1));
                }
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                let component = components.get(setup.component_cursor);
                let destination = setup.destinations.get(setup.destination_cursor);
                if let (Some(component), Some(destination)) = (component, destination)
                    && let Screen::SetupDestinations(current) = &mut self.screen
                {
                    current
                        .assignments
                        .insert(component.clone(), destination.key.clone());
                    // A different server must not inherit this Component's previous service/root.
                    if setup.assignments.get(component) != Some(&destination.key) {
                        current.target_settings.remove(component);
                    }
                } else if setup.destinations.is_empty() {
                    self.message = Some("Create an SSH Destination before continuing".into());
                }
            }
            KeyCode::Char('n') => {
                if !components
                    .iter()
                    .all(|component| setup.assignments.contains_key(component))
                {
                    self.message = Some("Assign a Destination to every Component".into());
                    return;
                }
                match prepare_setup(setup, &components).and_then(|project| {
                    prepare_initialize(project)
                        .map_err(|error| setup_diagnostics::configuration_error(&error))
                }) {
                    Ok(prepared) => {
                        self.screen = Screen::SetupReview {
                            destinations: setup.clone(),
                            prepared,
                            scroll: 0,
                        };
                    }
                    Err(error) => self.message = Some(error),
                }
            }
            KeyCode::Char('a') => self.open_new_ssh_destination(setup),
            KeyCode::Char('e') => self.open_initial_remote_target(setup.clone(), None),
            KeyCode::Esc => self.screen = Screen::SetupComponents(setup.components.clone()),
            _ => {}
        }
    }

    fn open_new_ssh_destination(&mut self, destinations: &DestinationSetupState) {
        let local = self
            .home_directory
            .as_deref()
            .map(discover_local_ssh)
            .transpose();
        let local = match local {
            Ok(Some(local)) => local,
            Ok(None) => crate::config::LocalSshDiscovery::default(),
            Err(_) => {
                self.message = Some("Local SSH candidates could not be read; availability is unknown. Check SSH config permissions and encoding, then press a to retry. No connection was changed.".into());
                return;
            }
        };
        let Ok(registry) = CredentialRegistry::load(&self.credential_registry_path) else {
            self.message = Some("Saved SSH identities could not be loaded; availability is unknown. Check the identity registry format and permissions, then press a to retry. No connection was changed.".into());
            return;
        };
        let mut credentials = registry
            .summaries()
            .into_iter()
            .filter(|summary| summary.available)
            .map(|summary| CredentialChoice::Saved {
                handle: summary.handle,
                label: summary.label,
            })
            .collect::<Vec<_>>();
        credentials.extend(local.identity_files.into_iter().map(|path| {
            let label = path.file_name().map_or_else(
                || "IdentityFile".into(),
                |name| format!("IdentityFile · {}", name.to_string_lossy()),
            );
            CredentialChoice::IdentityFile { path, label }
        }));
        deduplicate_credentials(&mut credentials);
        let mut draft = NewSshDestinationState {
            identity_request: None,
            destinations: destinations.clone(),
            connections: local.connections,
            connection_cursor: 0,
            host: String::new(),
            user: String::new(),
            port: "22".into(),
            field: SshField::Host,
            credentials,
            credential_cursor: 0,
            agent_status: "SSH Agent: checking…".into(),
        };
        draft.apply_connection(0);
        self.screen = Screen::NewSshDestination(draft);
        self.start_agent_probe();
    }

    fn start_agent_probe(&mut self) {
        self.launch_setup_identities();
    }

    fn handle_new_ssh_destination(&mut self, key: KeyCode, draft: &NewSshDestinationState) {
        if let Screen::NewSshDestination(current) = &mut self.screen
            && password_key(
                key,
                &mut current.credentials,
                &mut current.credential_cursor,
                &mut current.field,
            )
        {
            return;
        }
        match key {
            KeyCode::F(3) => self.open_key_browser(draft),
            KeyCode::Tab => {
                if let Screen::NewSshDestination(current) = &mut self.screen {
                    current.field = match current.field {
                        SshField::Host => SshField::User,
                        SshField::User => SshField::Port,
                        SshField::Port => SshField::Credential,
                        SshField::Credential => SshField::Host,
                    };
                }
            }
            KeyCode::BackTab => {
                if let Screen::NewSshDestination(current) = &mut self.screen {
                    current.field = match current.field {
                        SshField::Host => SshField::Credential,
                        SshField::User => SshField::Host,
                        SshField::Port => SshField::User,
                        SshField::Credential => SshField::Port,
                    };
                }
            }
            KeyCode::Up if draft.field == SshField::Credential => {
                if let Screen::NewSshDestination(current) = &mut self.screen {
                    current.credential_cursor = current.credential_cursor.saturating_sub(1);
                }
            }
            KeyCode::Down if draft.field == SshField::Credential => {
                if let Screen::NewSshDestination(current) = &mut self.screen {
                    current.credential_cursor = (current.credential_cursor + 1)
                        .min(current.credentials.len().saturating_sub(1));
                }
            }
            KeyCode::F(2) if !draft.connections.is_empty() => {
                if let Screen::NewSshDestination(current) = &mut self.screen {
                    let next = (current.connection_cursor + 1) % current.connections.len();
                    current.apply_connection(next);
                }
            }
            KeyCode::Backspace => {
                if let Screen::NewSshDestination(current) = &mut self.screen {
                    editable_field(current).map(String::pop);
                }
            }
            KeyCode::Char(character)
                if !character.is_control() && draft.field != SshField::Credential =>
            {
                if let Screen::NewSshDestination(current) = &mut self.screen
                    && let Some(value) = editable_field(current)
                {
                    value.push(character);
                }
            }
            KeyCode::Enter => self.start_host_key_probe(draft),
            KeyCode::Esc => self.screen = Screen::SetupDestinations(draft.destinations.clone()),
            _ => {}
        }
    }

    fn open_key_browser(&mut self, draft: &NewSshDestinationState) {
        let Some(home) = &self.home_directory else {
            self.message = Some("User home directory is unavailable".into());
            return;
        };
        let ssh = home.join(".ssh");
        let start = if ssh.is_dir() {
            ssh.as_path()
        } else {
            home.as_path()
        };
        match KeyFileBrowser::open(start) {
            Ok(browser) => {
                self.screen = Screen::KeyBrowser {
                    draft: draft.clone(),
                    browser,
                };
            }
            Err(_) => self.message = Some("SSH key directory could not be opened. Check its path and permissions, then use F3 to retry; the selected identity was not changed.".into()),
        }
    }

    fn handle_key_browser(
        &mut self,
        key: KeyCode,
        draft: &NewSshDestinationState,
        browser: &KeyFileBrowser,
    ) {
        match key {
            KeyCode::Up => {
                if let Screen::KeyBrowser { browser, .. } = &mut self.screen {
                    browser.selected = browser.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Screen::KeyBrowser { browser, .. } = &mut self.screen {
                    browser.selected =
                        (browser.selected + 1).min(browser.entries.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if let Some(path) = browser.selected_entry().filter(|path| path.is_dir()) {
                    self.open_key_browser_directory(draft, path);
                }
            }
            KeyCode::Backspace => {
                if let Some(parent) = browser.directory.parent() {
                    self.open_key_browser_directory(draft, parent);
                }
            }
            KeyCode::Char('s') => {
                let Some(path) = browser.selected_entry().filter(|path| path.is_file()) else {
                    self.message = Some("Selected entry is not an available regular file. Choose an existing private-key file; the selected identity was not changed.".into());
                    return;
                };
                let Ok(path) = std::fs::canonicalize(path) else {
                    self.message = Some("The selected key file could not be resolved. Check that it still exists and is readable, then choose it again; the selected identity was not changed.".into());
                    return;
                };
                let mut draft = draft.clone();
                let label = path.file_name().map_or_else(
                    || "IdentityFile".into(),
                    |name| format!("IdentityFile · {}", name.to_string_lossy()),
                );
                draft
                    .credentials
                    .push(CredentialChoice::IdentityFile { path, label });
                deduplicate_credentials(&mut draft.credentials);
                draft.credential_cursor = draft.credentials.len().saturating_sub(1);
                draft.field = SshField::Credential;
                self.screen = Screen::NewSshDestination(draft);
            }
            KeyCode::Esc => self.screen = Screen::NewSshDestination(draft.clone()),
            _ => {}
        }
    }

    fn open_key_browser_directory(&mut self, draft: &NewSshDestinationState, directory: &Path) {
        match KeyFileBrowser::open(directory) {
            Ok(browser) => {
                self.screen = Screen::KeyBrowser {
                    draft: draft.clone(),
                    browser,
                };
            }
            Err(_) => self.message = Some("SSH key directory could not be opened. Check its path and permissions, then choose a directory again; the previous list and identity were retained.".into()),
        }
    }

    fn start_host_key_probe(&mut self, draft: &NewSshDestinationState) {
        let port = match draft.port.parse::<u16>() {
            Ok(port) if port != 0 => port,
            _ => {
                self.message = Some("SSH port must be between 1 and 65535".into());
                return;
            }
        };
        if draft.host.is_empty()
            || draft.user.is_empty()
            || draft
                .host
                .chars()
                .chain(draft.user.chars())
                .any(char::is_whitespace)
        {
            self.message = Some("SSH host and user are required and cannot contain spaces".into());
            return;
        }
        if draft.selected_credential().is_none() {
            self.message = Some(
                "No SSH identity is available; add an Ed25519/ECDSA key or start SSH Agent".into(),
            );
            return;
        }
        let request = EndpointProbeRequest {
            driver: DriverKind::linux_ssh(),
            destination: DriverDestinationInput {
                value: serde_json::json!({ "host": draft.host, "port": port }),
            },
        };
        self.launch_setup_host_key(draft, request);
    }

    fn handle_host_key_confirm(
        &mut self,
        key: KeyCode,
        draft: &NewSshDestinationState,
        fingerprint: &HostKeyFingerprint,
    ) {
        match key {
            KeyCode::Char('y') => self.start_authentication(draft, fingerprint),
            KeyCode::Char('n') | KeyCode::Esc => {
                self.screen = Screen::NewSshDestination(draft.clone());
            }
            _ => {}
        }
    }

    fn start_authentication(
        &mut self,
        draft: &NewSshDestinationState,
        fingerprint: &HostKeyFingerprint,
    ) {
        let Ok(credential) = self.resolve_draft_credential(draft) else {
            self.message = Some("The selected SSH identity could not be loaded. Return to the form and choose an available identity.".into());
            return;
        };
        let destination = DriverDestinationInput {
            value: serde_json::json!({
                "host": draft.host,
                "port": draft.port.parse::<u16>().unwrap_or_default(),
                "user": draft.user,
                "hostKey": fingerprint.as_str(),
            }),
        };
        let component = selected_components(&draft.destinations)
            .get(draft.destinations.component_cursor)
            .cloned();
        let project = suggest_project_name(&draft.destinations.components.root);
        let Some(component) = component else {
            self.message = Some("No Component is selected for this Destination".into());
            return;
        };
        let request = DestinationSetupRequest {
            driver: DriverKind::linux_ssh(),
            destination,
            credential: SetupCredential::new(credential),
            remote_root: default_remote_root(&project, "production", &component),
        };
        self.launch_setup_authentication(draft, fingerprint, request);
    }

    fn resolve_draft_credential(
        &self,
        draft: &NewSshDestinationState,
    ) -> Result<SshCredential, String> {
        match draft
            .selected_credential()
            .ok_or_else(|| "Select an SSH identity".to_owned())?
        {
            CredentialChoice::Password(input) => input.protect()
                .map(|protected| SshCredential::Password { protected })
                .map_err(str::to_owned),
            CredentialChoice::Saved { handle, .. } => {
                CredentialRegistry::load(&self.credential_registry_path)
                    .map_err(|_| "SSH identities could not be loaded. Check Connections and local file permissions before retrying.".to_owned())?
                    .resolve(handle)
                    .cloned()
                    .ok_or_else(|| "The selected SSH identity no longer exists".into())
            }
            CredentialChoice::Agent { fingerprint, .. } => Ok(SshCredential::Agent {
                fingerprint: fingerprint.clone(),
            }),
            CredentialChoice::IdentityFile { path, .. } => {
                Ok(SshCredential::IdentityFile { path: path.clone() })
            }
        }
    }

    fn commit_ssh_destination(
        &self,
        draft: &NewSshDestinationState,
        fingerprint: &HostKeyFingerprint,
    ) -> Result<DestinationSetupState, String> {
        let choice = draft
            .selected_credential()
            .ok_or_else(|| "Select an SSH identity".to_owned())?;
        let port = draft
            .port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| "SSH port must be between 1 and 65535".to_owned())?;
        let original_credentials = CredentialRegistry::load(&self.credential_registry_path)
            .map_err(|_| {
                "SSH identities could not be loaded. No connection was saved.".to_owned()
            })?;
        let mut credentials = original_credentials.clone();
        let (credential, created_credential) = match choice {
            CredentialChoice::Password(input) => (
                credentials
                    .create(SshCredential::Password {
                        protected: input.protect().map_err(str::to_owned)?,
                    })
                    .map_err(|_| "The password could not be saved".to_owned())?,
                true,
            ),
            CredentialChoice::Saved { handle, .. } => {
                if credentials.resolve(handle).is_none() {
                    return Err("The selected SSH identity no longer exists".into());
                }
                (handle.clone(), false)
            }
            CredentialChoice::Agent { fingerprint, .. } => (
                credentials
                    .create(SshCredential::Agent {
                        fingerprint: fingerprint.clone(),
                    })
                    .map_err(|_| {
                        "The Agent identity is invalid. No identity or connection was saved."
                            .to_owned()
                    })?,
                true,
            ),
            CredentialChoice::IdentityFile { path, .. } => (
                credentials
                    .create(SshCredential::IdentityFile { path: path.clone() })
                    .map_err(|_| {
                        "The key-file identity is invalid. No identity or connection was saved."
                            .to_owned()
                    })?,
                true,
            ),
        };
        let mut destinations =
            DestinationRegistry::load(&self.destination_registry_path).map_err(|_| {
                "Saved connections could not be loaded. No identity or connection was saved."
                    .to_owned()
            })?;
        let original_destinations = destinations.clone();
        let key = DestinationKey::new();
        destinations
            .create(
                key.clone(),
                DestinationSettings::LinuxSsh {
                    host: draft.host.clone(),
                    port,
                    user: draft.user.clone(),
                    credential,
                    host_key: fingerprint.clone(),
                },
            )
            .map_err(|_| {
                "The SSH connection settings are invalid. No identity or connection was saved."
                    .to_owned()
            })?;

        setup_save::Registration {
            credential_path: &self.credential_registry_path,
            destination_path: &self.destination_registry_path,
            original_credentials: &original_credentials,
            credentials: &credentials,
            original_destinations: &original_destinations,
            destinations: &destinations,
            created_credential,
        }
        .persist()?;

        let mut setup = draft.destinations.clone();
        setup.destinations = destinations.summaries();
        setup.destination_cursor = setup
            .destinations
            .iter()
            .position(|destination| destination.key == key)
            .unwrap_or_default();
        if let Some(component) = selected_components(&setup).get(setup.component_cursor) {
            setup.assignments.insert(component.clone(), key);
            setup.target_settings.remove(component);
        }
        Ok(setup)
    }

    fn handle_setup_review(
        &mut self,
        key: KeyCode,
        destinations: &DestinationSetupState,
        prepared: &PreparedProjectInitialization,
    ) {
        match key {
            KeyCode::Up => {
                if let Screen::SetupReview { scroll, .. } = &mut self.screen {
                    *scroll = scroll.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Screen::SetupReview { scroll, .. } = &mut self.screen {
                    *scroll = scroll.saturating_add(1);
                }
            }
            KeyCode::PageUp => {
                if let Screen::SetupReview { scroll, .. } = &mut self.screen {
                    *scroll = scroll.saturating_sub(10);
                }
            }
            KeyCode::PageDown => {
                if let Screen::SetupReview { scroll, .. } = &mut self.screen {
                    *scroll = scroll.saturating_add(10);
                }
            }
            KeyCode::Char('c') => {
                let root = destinations.components.root.clone();
                match prepared.clone().commit(&root) {
                    Ok(config) => {
                        if register_initialized_project(&self.registry_path, &root, now_unix_ms()).is_err()
                        {
                            self.message = Some("Configuration saved, but recent-project registration failed. Open this directory again; check local registry permissions.".into());
                        } else {
                            self.refresh_recent();
                        }
                        self.show_overview(root, config);
                    }
                    Err(_) => self.message = Some("Configuration save was not confirmed. Reopen this directory to check shipforge.yaml before retrying; no deployment was started.".into()),
                }
            }
            KeyCode::Esc => self.screen = Screen::SetupDestinations(destinations.clone()),
            _ => {}
        }
    }

    fn handle_projects(&mut self, key: KeyCode) {
        let item_count = self.recent.len() + 1;
        match key {
            KeyCode::Char('c') => self.open_connections(),
            KeyCode::Char('x') => self.preview_recent_removal(),
            KeyCode::Char('f') => self.refresh_recent(),
            KeyCode::Up => self.selected_recent = self.selected_recent.saturating_sub(1),
            KeyCode::Down => {
                self.selected_recent = (self.selected_recent + 1).min(item_count - 1);
            }
            KeyCode::Enter if self.selected_recent < self.recent.len() => {
                let project = self.recent[self.selected_recent].clone();
                if project.available {
                    self.select_root(&project.project.root);
                } else {
                    self.message = Some("Cached project entry cannot be opened. Press f to refresh, or reselect its directory to check current state.".into());
                }
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                self.open_browser(&self.initial_directory.clone());
            }
            _ => {}
        }
    }

    fn handle_browser(&mut self, key: KeyCode, browser: &DirectoryBrowser) {
        match key {
            KeyCode::Up => {
                if let Screen::Browser(current) = &mut self.screen {
                    current.selected = current.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Screen::Browser(current) = &mut self.screen {
                    current.selected =
                        (current.selected + 1).min(current.children.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => {
                if let Some(child) = browser.selected_child() {
                    self.open_browser(child);
                }
            }
            KeyCode::Backspace => {
                if let Some(parent) = browser.directory.parent() {
                    self.open_browser(parent);
                }
            }
            KeyCode::Char('s') => self.select_root(&browser.directory),
            KeyCode::Esc => self.show_projects(),
            _ => {}
        }
    }

    fn open_browser(&mut self, directory: &Path) {
        match DirectoryBrowser::open(directory) {
            Ok(browser) => self.screen = Screen::Browser(browser),
            Err(_) => self.message = Some("Project directory could not be opened. Check its path and permissions, then choose a directory again; the previous page was retained.".into()),
        }
    }

    fn select_root(&mut self, root: &Path) {
        self.invalidate_attention();
        match select_project(&self.registry_path, root, now_unix_ms()) {
            Ok(ProjectSelection::Existing { root, config }) => {
                if config.schema_version != 2 {
                    self.open_project_edit(root);
                    self.message = Some("Configuration upgrade required. Press p in the editor to preview schema 2, then c to save. Project identities and equivalent service settings are preserved; no deployment is performed.".into());
                    return;
                }
                self.show_overview(root, config);
                self.refresh_recent();
            }
            Ok(ProjectSelection::New { root }) => match discover_components(&root) {
                Ok(report) => {
                    let selected = report
                        .components
                        .iter()
                        .map(|candidate| candidate.name.clone())
                        .collect();
                    self.screen = Screen::SetupComponents(ComponentSetupState {
                        root,
                        report,
                        selected,
                        cursor: 0,
                    });
                }
                Err(_) => {
                    self.screen = Screen::SetupComponents(ComponentSetupState {
                        root,
                        report: DiscoveryReport { components: Vec::new(), notices: vec!["Component discovery failed. Check manifest format, size and permissions; r retries, or a defines a Component manually. No build code was run.".into()] },
                        selected: BTreeSet::new(),
                        cursor: 0,
                    });
                }
            },
            Err(ProjectRegistryError::Config(_)) => self.open_reinitialize(root.to_owned()),
            Err(_) => self.message = Some("Project could not be opened or registered. Check the selected directory and local registry permissions, then retry. Configuration was not changed.".into()),
        }
    }

    fn refresh_recent(&mut self) {
        if let Ok(registry) = ProjectRegistry::load(&self.registry_path) {
            self.recent_unavailable = false;
            self.recent = registry.statuses();
            self.selected_recent = self.selected_recent.min(self.recent.len());
            if self.message.as_deref() == Some(setup_diagnostics::RECENT_REFRESH_FAILED) {
                self.message = None;
            }
        } else {
            self.recent_unavailable = true;
            for status in &mut self.recent {
                status.available = false;
            }
            self.message = Some(setup_diagnostics::RECENT_REFRESH_FAILED.into());
        }
    }
}

fn editable_field(draft: &mut NewSshDestinationState) -> Option<&mut String> {
    match draft.field {
        SshField::Host => Some(&mut draft.host),
        SshField::User => Some(&mut draft.user),
        SshField::Port => Some(&mut draft.port),
        SshField::Credential => None,
    }
}

fn password_key(
    key: KeyCode,
    credentials: &mut Vec<CredentialChoice>,
    cursor: &mut usize,
    field: &mut SshField,
) -> bool {
    if key == KeyCode::F(5) {
        if let Some(index) = credentials
            .iter()
            .position(|item| matches!(item, CredentialChoice::Password(_)))
        {
            *cursor = index;
        } else {
            credentials.push(CredentialChoice::Password(
                crate::config::PasswordInput::default(),
            ));
            *cursor = credentials.len() - 1;
        }
        *field = SshField::Credential;
        return true;
    }
    if *field != SshField::Credential {
        return false;
    }
    let Some(CredentialChoice::Password(input)) = credentials.get_mut(*cursor) else {
        return false;
    };
    match key {
        KeyCode::Char(character) if !character.is_control() => input.push(character),
        KeyCode::Backspace => input.pop(),
        KeyCode::Delete => input.clear(),
        _ => return false,
    }
    true
}

fn deduplicate_credentials(credentials: &mut Vec<CredentialChoice>) {
    let mut identities = std::collections::BTreeSet::new();
    credentials.retain(|credential| identities.insert(credential.identity()));
}

fn apply_agent_identities(
    draft: &mut NewSshDestinationState,
    result: Result<Vec<LocalIdentityCandidate>, String>,
) {
    match result {
        Ok(identities) => {
            let count = identities.len();
            draft
                .credentials
                .extend(identities.into_iter().map(|identity| {
                    let label = if identity.label.is_empty() {
                        format!("SSH Agent · {}", identity.reference)
                    } else {
                        format!("SSH Agent · {} · {}", identity.label, identity.reference)
                    };
                    CredentialChoice::Agent {
                        fingerprint: identity.reference,
                        label,
                    }
                }));
            deduplicate_credentials(&mut draft.credentials);
            draft.agent_status = format!("SSH Agent: {count} identities found");
        }
        Err(error) => draft.agent_status = format!("SSH Agent: {error}"),
    }
}

pub(super) fn environment_names(selection: &DeploySelectionState) -> Vec<String> {
    selection.config.environments.keys().cloned().collect()
}

pub(super) fn deployment_components(selection: &DeploySelectionState) -> Vec<ComponentName> {
    selection
        .config
        .environments
        .values()
        .nth(selection.environment_cursor)
        .map(|environment| environment.components.keys().cloned().collect())
        .unwrap_or_default()
}

fn selection_state(selection: &DeploymentSelection) -> DeploySelectionState {
    let environment_cursor = selection
        .config
        .environments
        .keys()
        .position(|name| name == &selection.environment)
        .unwrap_or_default();
    DeploySelectionState {
        root: selection.project_root.clone(),
        config: selection.config.clone(),
        environment_cursor,
        component_cursor: 0,
        selected: selection.components.clone(),
    }
}

#[derive(Debug)]
struct ProgressEvents {
    progress: crate::tui::live_progress::LiveProgress,
}

impl EventSink for ProgressEvents {
    fn emit(&self, event: DriverLog) {
        self.emit_record(crate::telemetry::log_record::LogEvent {
            namespace: event.namespace,
            message: event.message,
            scope: None,
            kind: crate::telemetry::log_record::LogEventKind::Output,
        });
    }

    fn emit_record(&self, event: crate::telemetry::log_record::LogEvent) {
        self.progress.record(event);
    }
}

fn deployment_summary(report: &DeploymentReport) -> String {
    use crate::application::DeploymentFailure;
    use std::fmt::Write as _;

    let mut summary = format!(
        "Deployment {}: {:?}\n",
        report.deployment.id, report.deployment.state
    );
    if let Some(failure) = &report.failure {
        match failure {
            DeploymentFailure::Cancelled => summary.push_str("Cancellation requested.\n"),
            DeploymentFailure::Driver {
                component,
                stage,
                error,
                ..
            } => {
                let _ = writeln!(
                    summary,
                    "{component} / {stage:?}: {}",
                    crate::tui::deployment_error::driver_error(error)
                );
            }
            DeploymentFailure::Contract {
                component, stage, ..
            } => {
                let _ = writeln!(
                    summary,
                    "{component} / {stage:?}: Operation safety check failed; inspect recorded and current state before retrying."
                );
            }
        }
    }
    for (component, result) in &report.deployment.components {
        let observed = result
            .observed_release
            .as_ref()
            .map_or("none reported", |version| version.as_str());
        let _ = writeln!(
            summary,
            "{component}: {:?}; observed Release: {observed}",
            result.outcome
        );
    }
    for (component, error) in &report.compensation_failures {
        let _ = writeln!(
            summary,
            "MANUAL RECOVERY REQUIRED — {component}: {}",
            crate::tui::deployment_error::driver_error(error)
        );
    }
    for warning in &report.warnings {
        let _ = writeln!(summary, "WARNING — {warning}");
    }
    summary
}

fn catch_worker_failure<T>(operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        Err("Background operation stopped unexpectedly. Remote state may be incomplete; inspect deployment history and remote state before retrying.".into())
    })
}

fn spawn_plan_thread(
    request_id: uuid::Uuid,
    runtime: tokio::runtime::Handle,
    gateway: Arc<dyn TuiDeploymentGateway>,
    selection: DeploymentSelection,
    cancellation: tokio_util::sync::CancellationToken,
    sender: SyncSender<BackgroundEvent>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("shipforge-deployment-plan".into())
        .spawn(move || {
            let result =
                catch_worker_failure(|| runtime.block_on(gateway.plan(selection, &cancellation)));
            let _ = sender.send(BackgroundEvent::DeploymentPlan(request_id, result));
        })
}

struct DeploymentExecutionWorker {
    request_id: uuid::Uuid,
    runtime: tokio::runtime::Handle,
    gateway: Arc<dyn TuiDeploymentGateway>,
    session: Arc<DeploymentSession>,
    plan: DeploymentPlan,
    cancellation: tokio_util::sync::CancellationToken,
    sender: SyncSender<BackgroundEvent>,
    progress: crate::tui::live_progress::LiveProgress,
}

fn spawn_execute_thread(request: DeploymentExecutionWorker) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("shipforge-deployment-run".into())
        .spawn(move || {
            let events = ProgressEvents {
                progress: request.progress,
            };
            let result = catch_worker_failure(|| {
                request.runtime.block_on(async {
                    request
                        .session
                        .run(
                            request
                                .gateway
                                .execute(request.plan, &events, &request.cancellation),
                        )
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                })
            });
            // Freeze operation time at the worker boundary, not when the UI
            // eventually consumes a queued completion notification.
            events.progress.finish();
            let _ = request.sender.send(BackgroundEvent::DeploymentFinished(
                request.request_id,
                result,
            ));
        })
}

fn selected_components(setup: &DestinationSetupState) -> Vec<ComponentName> {
    setup
        .components
        .report
        .components
        .iter()
        .filter(|candidate| setup.components.selected.contains(&candidate.name))
        .map(|candidate| candidate.name.clone())
        .collect()
}

fn prepare_setup(
    setup: &DestinationSetupState,
    components: &[ComponentName],
) -> Result<ProjectSetup, String> {
    let component_setups = setup
        .components
        .report
        .components
        .iter()
        .filter(|candidate| setup.components.selected.contains(&candidate.name))
        .map(|candidate| (candidate.name.clone(), candidate.setup.clone()))
        .collect();
    let targets = components
        .iter()
        .map(|component| {
            let destination = setup
                .assignments
                .get(component)
                .cloned()
                .ok_or_else(|| format!("Component `{component}` has no Destination"))?;
            Ok((
                component.clone(),
                TargetSetup {
                    destination,
                    root: setup
                        .target_settings
                        .get(component)
                        .and_then(|settings| settings.root.clone()),
                    service: setup
                        .target_settings
                        .get(component)
                        .and_then(|settings| settings.service.clone()),
                    health: None,
                    after: Vec::new(),
                },
            ))
        })
        .collect::<Result<_, String>>()?;
    Ok(ProjectSetup {
        project: suggest_project_name(&setup.components.root),
        components: component_setups,
        environments: std::collections::BTreeMap::from([(
            "production".into(),
            EnvironmentSetup {
                components: targets,
            },
        )]),
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    mod planning_cancellation;
    use crate::application::SetupRootState;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use async_trait::async_trait;
    use crossterm::event::KeyModifiers;
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{
        config::{DestinationSettings, HostKeyFingerprint},
        drivers::CredentialHandle,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[derive(Debug, Default)]
    struct FakeDeploymentGateway {
        executed: AtomicBool,
        cancelled: AtomicBool,
    }

    #[async_trait(?Send)]
    impl TuiDeploymentGateway for FakeDeploymentGateway {
        async fn plan(
            &self,
            selection: DeploymentSelection,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<DeploymentPlan, String> {
            Ok(DeploymentPlan {
                activation_order: selection.components.iter().cloned().collect(),
                selection,
                entries: Vec::new(),
                git: crate::application::GitWorktreeState::NotRepository,
                git_metadata: crate::application::GitMetadata::default(),
            })
        }

        async fn execute(
            &self,
            _: DeploymentPlan,
            events: &dyn EventSink,
            cancellation: &tokio_util::sync::CancellationToken,
        ) -> Result<DeploymentReport, String> {
            self.executed.store(true, Ordering::SeqCst);
            events.emit(DriverLog {
                namespace: "test".into(),
                message: "execution started".into(),
            });
            cancellation.cancelled().await;
            self.cancelled.store(true, Ordering::SeqCst);
            let mut deployment = crate::domain::Deployment::new();
            deployment.start().unwrap();
            deployment.cancel().unwrap();
            Ok(DeploymentReport {
                deployment,
                failure: Some(crate::application::DeploymentFailure::Cancelled),
                compensation_failures: std::collections::BTreeMap::new(),
                warnings: Vec::new(),
            })
        }
    }

    async fn wait_for_screen(app: &mut App, predicate: impl Fn(&Screen) -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                app.poll_background();
                if predicate(&app.screen) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("background operation should produce the expected screen");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execution_clock_freezes_before_a_blocked_completion_delivery() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory.path()).unwrap()
        else {
            panic!("fixture configuration");
        };
        let environment = config.environments.keys().next().unwrap().clone();
        let selection = DeploymentSelection {
            project_root: directory.path().to_path_buf(),
            config,
            environment,
            components: BTreeSet::new(),
        };
        let gateway = Arc::new(FakeDeploymentGateway::default());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let plan = gateway.plan(selection, &cancellation).await.unwrap();
        // Rendezvous channel keeps delivery blocked until this test receives.
        let (sender, receiver) = mpsc::sync_channel(0);
        let progress = crate::tui::live_progress::LiveProgress::default();
        cancellation.cancel();
        let request_id = uuid::Uuid::now_v7();
        let worker = spawn_execute_thread(DeploymentExecutionWorker {
            request_id,
            runtime: tokio::runtime::Handle::current(),
            gateway,
            session: Arc::new(DeploymentSession::default()),
            plan,
            cancellation,
            sender,
            progress: progress.clone(),
        })
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !progress.snapshot().finished {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must finish even with UI delivery blocked");
        let elapsed = progress.snapshot().elapsed_ms;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(progress.snapshot().elapsed_ms, elapsed);
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
            BackgroundEvent::DeploymentFinished(completed_id, Ok(_)) if completed_id == request_id
        ));
        worker.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deployment_requires_confirmation_and_waits_for_safe_cancellation() {
        assert_confirmation_and_cancellation(Some(key(KeyCode::Esc))).await;
        assert_confirmation_and_cancellation(Some(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .await;
        assert_confirmation_and_cancellation(None).await;
    }

    async fn assert_confirmation_and_cancellation(cancel_key: Option<KeyEvent>) {
        let process_exit = cancel_key.is_none();
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("shipforge.yaml"),
            include_str!("../../docs/examples/shipforge.yaml"),
        )
        .unwrap();
        let crate::config::ProjectConfigState::Loaded(config) =
            crate::config::load(directory.path()).unwrap()
        else {
            panic!("expected example configuration");
        };
        let gateway = Arc::new(FakeDeploymentGateway::default());
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        app.deployment_gateway = gateway.clone();
        app.screen = Screen::Overview {
            root: directory.path().to_owned(),
            config,
        };
        assert!(!app.handle_key(key(KeyCode::Char('d'))));
        assert!(matches!(app.screen, Screen::DeploySelection(_)));
        assert!(!app.enter_primary());
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::DeploymentReview { .. })
        })
        .await;
        assert!(!gateway.executed.load(Ordering::SeqCst));
        assert_eq!(app.action_cursor, Some(0));
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers)));
            assert!(matches!(app.screen, Screen::DeploymentReview { .. }));
            assert!(!gateway.executed.load(Ordering::SeqCst));
        }
        assert!(!app.handle_key(key(KeyCode::Enter)));
        assert!(matches!(app.screen, Screen::DeploymentRunning { .. }));
        assert!(!app.handle_key(key(KeyCode::Char('q'))));
        assert!(matches!(app.screen, Screen::DeploymentRunning { .. }));
        assert_eq!(app.exit_state(), ExitState::Confirm);
        assert!(!gateway.cancelled.load(Ordering::SeqCst));
        if let Some(cancel_key) = cancel_key {
            assert!(!app.handle_key(key(KeyCode::Esc)));
            assert_eq!(app.exit_state(), ExitState::Running);
            assert!(!app.handle_key(cancel_key));
            assert!(matches!(
                app.screen,
                Screen::DeploymentRunning {
                    cancellation_requested: true,
                    ..
                }
            ));
        } else {
            assert!(!app.handle_key(key(KeyCode::Char('c'))));
            assert_eq!(app.exit_state(), ExitState::Waiting);
            assert!(!app.exit_ready());
            assert!(matches!(
                app.screen,
                Screen::DeploymentRunning {
                    cancellation_requested: true,
                    ..
                }
            ));
            app.shutdown();
            assert!(matches!(app.screen, Screen::DeploymentFinished { .. }));
            assert!(app.exit_ready());
        }
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::DeploymentFinished { .. })
        })
        .await;
        assert!(gateway.executed.load(Ordering::SeqCst));
        assert!(gateway.cancelled.load(Ordering::SeqCst));
        assert!(!app.deployment_session.is_active());
        if process_exit {
            assert_eq!(app.exit_state(), ExitState::Waiting);
        } else {
            assert!(!app.handle_key(key(KeyCode::Esc)));
            assert!(matches!(app.screen, Screen::Overview { .. }));
        }
    }

    #[test]
    fn stale_deployment_check_cannot_replace_a_new_request() {
        let directory = tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        let request_id = uuid::Uuid::now_v7();
        let selection = DeploySelectionState {
            root: directory.path().to_owned(),
            config: ProjectConfig {
                schema_version: 1,
                project_id: crate::domain::ProjectId::new(),
                project: "routing-test".into(),
                components: std::collections::BTreeMap::new(),
                environments: std::collections::BTreeMap::new(),
            },
            environment_cursor: 0,
            component_cursor: 0,
            selected: BTreeSet::new(),
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        app.screen = Screen::DeploymentPlanning {
            request_id,
            selection,
            cancellation: cancellation.clone(),
        };
        app.deployment_plan_task = Some(DeploymentTask {
            id: request_id,
            cancellation,
            worker: Some(std::thread::spawn(|| {})),
        });
        app.background_sender
            .send(BackgroundEvent::DeploymentPlan(
                uuid::Uuid::now_v7(),
                Err("stale failure".into()),
            ))
            .unwrap();
        app.poll_background();
        assert!(matches!(app.screen, Screen::DeploymentPlanning { .. }));
        assert!(app.message.is_none());
        app.background_sender
            .send(BackgroundEvent::DeploymentPlan(
                request_id,
                Err("current failure".into()),
            ))
            .unwrap();
        app.poll_background();
        assert!(matches!(app.screen, Screen::DeploySelection(_)));
        assert_eq!(app.message.as_deref(), Some("current failure"));
    }

    #[test]
    fn worker_panic_returns_a_safe_diagnostic_without_payload() {
        let result = catch_worker_failure::<()>(|| panic!("secret panic payload"));
        let error = result.unwrap_err();
        assert!(error.contains("Remote state may be incomplete"));
        assert!(!error.contains("secret panic payload"));
        assert_eq!(catch_worker_failure(|| Ok(42)), Ok(42));
        assert_eq!(
            catch_worker_failure::<()>(|| Err("failure".into())),
            Err("failure".into())
        );
    }

    #[test]
    fn deployment_summary_preserves_failure_and_manual_recovery_instructions() {
        use crate::{
            application::{DeploymentFailure, OrchestrationStage},
            domain::{ComponentDeploymentResult, ComponentOutcome, Deployment},
            drivers::DriverError,
        };
        let component = ComponentName::parse("worker").unwrap();
        let error = DriverError {
            recovery_blocked: false,
            stage: "compensate".into(),
            target: "worker".into(),
            message: "current changed externally".into(),
            suggested_action: "inspect current before retrying".into(),
        };
        let mut deployment = Deployment::new();
        deployment.start().unwrap();
        deployment.fail().unwrap();
        deployment.components.insert(
            component.clone(),
            ComponentDeploymentResult {
                outcome: ComponentOutcome::CompensationFailed,
                attempted_release: None,
                observed_release: None,
            },
        );
        let report = DeploymentReport {
            deployment,
            failure: Some(DeploymentFailure::Driver {
                component: component.clone(),
                stage: OrchestrationStage::Activate,
                error: error.clone(),
                observed_release: None,
            }),
            compensation_failures: std::collections::BTreeMap::from([(component, error)]),
            warnings: vec!["Deployment log incomplete: disk full".into()],
        };
        let summary = deployment_summary(&report);
        assert!(summary.contains("worker / Activate"));
        assert!(summary.contains("CompensationFailed"));
        assert!(summary.contains("MANUAL RECOVERY REQUIRED"));
        assert!(summary.contains("recorded/current deployment state before retrying"));
        assert!(summary.contains("none reported"));
        assert!(summary.contains("WARNING — Deployment log incomplete: disk full"));
    }

    #[test]
    fn progress_flood_is_nonblocking_and_bounds_unicode_payloads() {
        let progress = crate::tui::live_progress::LiveProgress::default();
        let events = ProgressEvents {
            progress: progress.clone(),
        };
        for _ in 0..1000 {
            events.emit(DriverLog {
                namespace: "build.stdout".into(),
                message: "界".repeat(2000),
            });
        }
        let retained = progress.drain();
        assert!(!retained.rows.is_empty());
        assert!(retained.rows.len() <= 128);
        assert!(retained.snapshot.dropped_rows > 0);
        assert!(
            retained
                .rows
                .iter()
                .all(|row| row.event.message.len() <= 16 * 1024)
        );
        events.emit(DriverLog {
            namespace: "closed".into(),
            message: "ignored".into(),
        });
    }

    #[derive(Clone, Copy, Debug)]
    enum FakeSetupMode {
        Success,
        AuthenticationFailure,
        WaitForCancellation,
    }

    #[derive(Debug)]
    struct FakeSetupGateway {
        mode: FakeSetupMode,
        probe_started: AtomicBool,
        cancellation_observed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl crate::application::DestinationSetupGateway for FakeSetupGateway {
        fn driver_kind(&self) -> DriverKind {
            DriverKind::linux_ssh()
        }

        async fn discover_local_identities(
            &self,
            _cancellation: &tokio_util::sync::CancellationToken,
        ) -> Result<Vec<LocalIdentityCandidate>, crate::application::DestinationSetupError>
        {
            Ok(Vec::new())
        }

        async fn capture_endpoint_identity(
            &self,
            _request: &EndpointProbeRequest,
            _timeout: Duration,
            cancellation: &tokio_util::sync::CancellationToken,
        ) -> Result<String, crate::application::DestinationSetupError> {
            self.probe_started.store(true, Ordering::SeqCst);
            if matches!(self.mode, FakeSetupMode::WaitForCancellation) {
                cancellation.cancelled().await;
                self.cancellation_observed.store(true, Ordering::SeqCst);
                return Err(crate::application::DestinationSetupError::operation(
                    "fake Host Key probe",
                    "cancelled",
                ));
            }
            Ok("SHA256:fake-host".into())
        }

        async fn authenticate_and_probe(
            &self,
            request: &DestinationSetupRequest,
            _connect_timeout: Duration,
            _command_timeout: Duration,
            _cancellation: &tokio_util::sync::CancellationToken,
        ) -> Result<RemoteSetupCandidates, crate::application::DestinationSetupError> {
            assert!(request.credential.downcast_ref::<SshCredential>().is_some());
            if matches!(self.mode, FakeSetupMode::AuthenticationFailure) {
                return Err(crate::application::DestinationSetupError::operation(
                    "fake authentication",
                    "rejected",
                ));
            }
            Ok(RemoteSetupCandidates {
                root: SetupRootState::WritableDirectory,
                services: vec!["web.service".into()],
                notices: Vec::new(),
            })
        }
    }

    fn app_with_fake_setup(
        mode: FakeSetupMode,
    ) -> (TempDir, App, NewSshDestinationState, Arc<FakeSetupGateway>) {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"web","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        let identity = directory.path().join("id_ed25519");
        std::fs::write(&identity, "test fixture only").unwrap();
        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let gateway = Arc::new(FakeSetupGateway {
            mode,
            probe_started: AtomicBool::new(false),
            cancellation_observed: cancellation_observed.clone(),
        });
        let service = DestinationSetupService::new(gateway.clone());
        let mut app = App::new_with_setup(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
            service,
        )
        .unwrap();
        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        app.enter_primary();
        let Screen::SetupDestinations(destinations) = app.screen.clone() else {
            panic!("expected Destination setup");
        };
        let draft = NewSshDestinationState {
            identity_request: None,
            destinations,
            connections: Vec::new(),
            connection_cursor: 0,
            host: "fake.example".into(),
            user: "deploy".into(),
            port: "22".into(),
            field: SshField::Credential,
            credentials: vec![CredentialChoice::IdentityFile {
                path: identity,
                label: "test identity".into(),
            }],
            credential_cursor: 0,
            agent_status: String::new(),
        };
        (directory, app, draft, gateway)
    }

    async fn allow_background_task_to_run(app: &mut App) {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
            app.poll_background();
        }
    }

    #[test]
    fn project_picker_opens_keyboard_directory_browser() {
        let directory = tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();

        app.enter_primary();
        let Screen::Browser(browser) = &app.screen else {
            panic!("expected browser");
        };
        assert_eq!(
            browser.children,
            vec![std::fs::canonicalize(child).unwrap()]
        );
    }

    #[test]
    fn selecting_directory_without_config_starts_new_project_flow() {
        let directory = tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        assert!(matches!(app.screen, Screen::SetupComponents(_)));
        assert!(!directory.path().join("projects.yaml").exists());
    }

    #[test]
    fn escape_returns_from_project_detail_without_quitting() {
        let directory = tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        app.screen = Screen::SetupComponents(ComponentSetupState {
            root: directory.path().to_owned(),
            report: DiscoveryReport::default(),
            selected: std::collections::BTreeSet::new(),
            cursor: 0,
        });
        assert!(!app.handle_key(key(KeyCode::Esc)));
        assert!(matches!(app.screen, Screen::Projects));
    }

    #[test]
    fn discovered_components_can_be_toggled_without_writing_config() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"web","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        let Screen::SetupComponents(setup) = &app.screen else {
            panic!("expected Component setup");
        };
        assert_eq!(setup.selected.len(), 1);

        app.handle_key(key(KeyCode::Char(' ')));
        let Screen::SetupComponents(setup) = &app.screen else {
            panic!("expected Component setup");
        };
        assert!(setup.selected.is_empty());
        assert!(!directory.path().join("shipforge.yaml").exists());
    }

    #[test]
    fn reviews_and_commits_setup_only_after_confirmation() {
        let directory = tempdir().unwrap();
        let destination_path = directory.path().join("destinations.yaml");
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"web","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        let destination_key =
            DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
        let mut registry = DestinationRegistry::new();
        registry
            .create(
                destination_key.clone(),
                DestinationSettings::LinuxSsh {
                    host: "app.example.com".into(),
                    port: 22,
                    user: "deploy".into(),
                    credential: CredentialHandle::new(),
                    host_key: HostKeyFingerprint::parse("SHA256:confirmed").unwrap(),
                },
            )
            .unwrap();
        registry.save(&destination_path).unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            destination_path,
            directory.path(),
        )
        .unwrap();

        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        app.enter_primary();
        app.handle_key(key(KeyCode::Char(' ')));

        let Screen::SetupDestinations(setup) = &app.screen else {
            panic!("expected Destination setup");
        };
        assert_eq!(
            setup.assignments.get(&ComponentName::parse("web").unwrap()),
            Some(&destination_key)
        );
        assert!(!directory.path().join("shipforge.yaml").exists());

        if let Screen::SetupDestinations(setup) = &mut app.screen {
            setup.target_settings.insert(
                ComponentName::parse("web").unwrap(),
                ComponentTargetSettings {
                    root: None,
                    service: Some("web.service".into()),
                },
            );
        }

        app.handle_key(key(KeyCode::Char('n')));
        let Screen::SetupReview { prepared, .. } = &app.screen else {
            panic!("expected setup review");
        };
        assert!(prepared.preview().contains("project:"));
        assert!(prepared.preview().contains(destination_key.as_str()));
        assert!(prepared.preview().contains("schemaVersion: 2"));
        assert!(prepared.preview().contains("unit: web.service"));
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.screen, Screen::SetupDestinations(_)));
        assert!(!directory.path().join("shipforge.yaml").exists());

        app.handle_key(key(KeyCode::Char('n')));
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers));
            assert!(matches!(app.screen, Screen::SetupReview { .. }));
            assert!(!directory.path().join("shipforge.yaml").exists());
        }
        app.handle_key(key(KeyCode::Char('c')));
        assert!(matches!(app.screen, Screen::Overview { .. }));
        assert!(directory.path().join("shipforge.yaml").is_file());
        assert!(directory.path().join("projects.yaml").is_file());
    }

    #[test]
    fn new_ssh_destination_prefills_discovered_values_without_writing() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"web","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        let ssh = directory.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::create_dir(ssh.join("custom")).unwrap();
        std::fs::write(ssh.join("custom").join("deploy_key"), "selected later").unwrap();
        std::fs::write(ssh.join("id_ed25519"), "not-read-by-discovery").unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host production\n  HostName app.example.com\n  User deploy\n  Port 2222\n  IdentityFile ~/.ssh/id_ed25519\n",
        )
        .unwrap();
        let destination_path = directory.path().join("destinations.yaml");
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            destination_path.clone(),
            directory.path(),
        )
        .unwrap();
        app.home_directory = Some(directory.path().to_owned());

        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        app.enter_primary();
        app.handle_key(key(KeyCode::Char('a')));

        let Screen::NewSshDestination(draft) = &app.screen else {
            panic!("expected new SSH Destination form");
        };
        assert_eq!(draft.host, "app.example.com");
        assert_eq!(draft.user, "deploy");
        assert_eq!(draft.port, "2222");
        assert_eq!(draft.credentials.len(), 1);
        assert_eq!(draft.credentials[0].label(), "IdentityFile · id_ed25519");
        assert!(!destination_path.exists());
        assert!(!directory.path().join("credentials.yaml").exists());

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::F(3)));
        assert!(matches!(app.screen, Screen::KeyBrowser { .. }));
        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        let Screen::NewSshDestination(draft) = &app.screen else {
            panic!("expected SSH form after key selection");
        };
        assert!(
            draft
                .credentials
                .iter()
                .any(|credential| credential.label() == "IdentityFile · deploy_key")
        );
        assert!(!destination_path.exists());
    }

    #[test]
    fn successful_authentication_creates_and_assigns_destination() {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"web","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        let identity = directory.path().join("id_ed25519");
        std::fs::write(&identity, "test fixture only").unwrap();
        let destination_path = directory.path().join("destinations.yaml");
        let credential_path = directory.path().join("credentials.yaml");
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            destination_path.clone(),
            directory.path(),
        )
        .unwrap();
        app.enter_primary();
        app.handle_key(key(KeyCode::Char('s')));
        app.enter_primary();
        let Screen::SetupDestinations(destinations) = app.screen.clone() else {
            panic!("expected Destination setup");
        };
        let draft = NewSshDestinationState {
            identity_request: None,
            destinations,
            connections: Vec::new(),
            connection_cursor: 0,
            host: "app.example.com".into(),
            user: "deploy".into(),
            port: "22".into(),
            field: SshField::Credential,
            credentials: vec![CredentialChoice::IdentityFile {
                path: identity.clone(),
                label: "IdentityFile · id_ed25519".into(),
            }],
            credential_cursor: 0,
            agent_status: String::new(),
        };
        let fingerprint = HostKeyFingerprint::parse("SHA256:confirmed-host").unwrap();
        let request_id = uuid::Uuid::now_v7();
        let cancellation = tokio_util::sync::CancellationToken::new();
        app.setup_task = Some(setup_async::SetupTask::fixture(
            request_id,
            setup_async::SetupKind::Authentication,
            cancellation.clone(),
        ));
        app.screen = Screen::SshAuthenticationPending {
            request_id,
            draft,
            fingerprint,
            cancellation_requested: false,
        };
        app.background_sender
            .send(BackgroundEvent::Authentication(
                request_id,
                Ok(RemoteSetupCandidates {
                    root: SetupRootState::Missing,
                    services: Vec::new(),
                    notices: Vec::new(),
                }),
            ))
            .unwrap();
        app.poll_background();

        let Screen::RemoteSetupSelection(selection) = &app.screen else {
            panic!("expected remote setup selection after authentication");
        };
        assert_eq!(selection.root_state, Some(SetupRootState::Missing));
        app.enter_primary();
        let Screen::SetupDestinations(setup) = &app.screen else {
            panic!("expected Destination setup after selecting remote settings");
        };
        assert_eq!(setup.destinations.len(), 1);
        assert_eq!(setup.assignments.len(), 1);
        let destination = DestinationRegistry::load(&destination_path).unwrap();
        let credentials = CredentialRegistry::load(&credential_path).unwrap();
        let key = &setup.destinations[0].key;
        let record = destination.resolve(key).unwrap();
        let DestinationSettings::LinuxSsh { credential, .. } = &record.settings;
        assert_eq!(
            credentials.resolve(credential),
            Some(&SshCredential::IdentityFile { path: identity })
        );
        assert!(!directory.path().join("shipforge.yaml").exists());
    }

    #[test]
    fn failed_authentication_does_not_write_registries() {
        let directory = tempdir().unwrap();
        let draft = NewSshDestinationState {
            identity_request: None,
            destinations: DestinationSetupState {
                components: ComponentSetupState {
                    root: directory.path().to_owned(),
                    report: DiscoveryReport::default(),
                    selected: std::collections::BTreeSet::default(),
                    cursor: 0,
                },
                destinations: Vec::new(),
                assignments: std::collections::BTreeMap::default(),
                target_settings: std::collections::BTreeMap::default(),
                component_cursor: 0,
                destination_cursor: 0,
            },
            connections: Vec::new(),
            connection_cursor: 0,
            host: "app.example.com".into(),
            user: "deploy".into(),
            port: "22".into(),
            field: SshField::Credential,
            credentials: Vec::new(),
            credential_cursor: 0,
            agent_status: String::new(),
        };
        let fingerprint = HostKeyFingerprint::parse("SHA256:confirmed-host").unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        let request_id = uuid::Uuid::now_v7();
        let cancellation = tokio_util::sync::CancellationToken::new();
        app.setup_task = Some(setup_async::SetupTask::fixture(
            request_id,
            setup_async::SetupKind::Authentication,
            cancellation.clone(),
        ));
        app.screen = Screen::SshAuthenticationPending {
            request_id,
            draft,
            fingerprint,
            cancellation_requested: false,
        };
        app.background_sender
            .send(BackgroundEvent::Authentication(
                request_id,
                Err("SSH server rejected the selected identity".into()),
            ))
            .unwrap();

        app.poll_background();

        assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));
        assert!(!directory.path().join("destinations.yaml").exists());
        assert!(!directory.path().join("credentials.yaml").exists());
    }

    #[test]
    fn rejecting_host_key_leaves_registries_unchanged() {
        let directory = tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        let draft = NewSshDestinationState {
            identity_request: None,
            destinations: DestinationSetupState {
                components: ComponentSetupState {
                    root: directory.path().to_owned(),
                    report: DiscoveryReport::default(),
                    selected: std::collections::BTreeSet::default(),
                    cursor: 0,
                },
                destinations: Vec::new(),
                assignments: std::collections::BTreeMap::default(),
                target_settings: std::collections::BTreeMap::default(),
                component_cursor: 0,
                destination_cursor: 0,
            },
            connections: Vec::new(),
            connection_cursor: 0,
            host: "app.example.com".into(),
            user: "deploy".into(),
            port: "22".into(),
            field: SshField::Credential,
            credentials: Vec::new(),
            credential_cursor: 0,
            agent_status: String::new(),
        };
        app.screen = Screen::HostKeyConfirm {
            draft,
            fingerprint: HostKeyFingerprint::parse("SHA256:untrusted").unwrap(),
        };
        app.handle_key(key(KeyCode::Esc));

        assert!(matches!(app.screen, Screen::NewSshDestination(_)));
        assert!(!directory.path().join("destinations.yaml").exists());
        assert!(!directory.path().join("credentials.yaml").exists());
    }

    #[test]
    fn agent_results_are_retained_while_key_browser_is_open() {
        let directory = tempdir().unwrap();
        let mut app = App::new(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
        )
        .unwrap();
        let draft = NewSshDestinationState {
            identity_request: None,
            destinations: DestinationSetupState {
                components: ComponentSetupState {
                    root: directory.path().to_owned(),
                    report: DiscoveryReport::default(),
                    selected: std::collections::BTreeSet::default(),
                    cursor: 0,
                },
                destinations: Vec::new(),
                assignments: std::collections::BTreeMap::default(),
                target_settings: std::collections::BTreeMap::default(),
                component_cursor: 0,
                destination_cursor: 0,
            },
            connections: Vec::new(),
            connection_cursor: 0,
            host: String::new(),
            user: String::new(),
            port: "22".into(),
            field: SshField::Credential,
            credentials: Vec::new(),
            credential_cursor: 0,
            agent_status: "checking".into(),
        };
        let request_id = uuid::Uuid::now_v7();
        let mut draft = draft;
        draft.identity_request = Some(request_id);
        app.setup_task = Some(setup_async::SetupTask::fixture(
            request_id,
            setup_async::SetupKind::Identities,
            tokio_util::sync::CancellationToken::new(),
        ));
        app.screen = Screen::KeyBrowser {
            draft,
            browser: KeyFileBrowser::open(directory.path()).unwrap(),
        };
        app.background_sender
            .send(BackgroundEvent::AgentIdentities(
                request_id,
                Ok(vec![LocalIdentityCandidate {
                    reference: "SHA256:agent-key".into(),
                    label: "deploy".into(),
                }]),
            ))
            .unwrap();

        app.poll_background();

        let Screen::KeyBrowser { draft, .. } = &app.screen else {
            panic!("expected Key browser to remain open");
        };
        assert_eq!(draft.credentials.len(), 1);
        assert!(draft.credentials[0].label().contains("deploy"));
        assert_eq!(draft.agent_status, "SSH Agent: 1 identities found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tui_uses_setup_service_for_host_key_authentication_and_candidates() {
        let (_directory, mut app, draft, _) = app_with_fake_setup(FakeSetupMode::Success);
        app.start_host_key_probe(&draft);
        allow_background_task_to_run(&mut app).await;
        assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));

        app.enter_primary();
        assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));
        app.handle_key(key(KeyCode::Char('y')));
        allow_background_task_to_run(&mut app).await;
        let Screen::RemoteSetupSelection(selection) = &app.screen else {
            panic!("expected remote setup candidates");
        };
        assert_eq!(
            selection.root_state,
            Some(SetupRootState::WritableDirectory)
        );
        assert_eq!(selection.systemd_units, vec!["web.service"]);
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn setup_password_is_masked_and_saved_only_after_successful_confirmed_authentication() {
        for mode in [FakeSetupMode::Success, FakeSetupMode::AuthenticationFailure] {
            let succeeds = matches!(mode, FakeSetupMode::Success);
            let (directory, mut app, draft, _) = app_with_fake_setup(mode);
            app.screen = Screen::NewSshDestination(draft);
            app.handle_key(key(KeyCode::F(5)));
            let password = "setup-only 密码 q$'";
            for character in password.chars() {
                app.handle_key(key(KeyCode::Char(character)));
            }
            assert!(!format!("{:?}", app.screen).contains(password));
            let Screen::NewSshDestination(draft) = &app.screen else {
                panic!("input remains in form")
            };
            let resolved = app.resolve_draft_credential(draft).unwrap();
            let SshCredential::Password { protected } = resolved else {
                panic!("password expected")
            };
            assert_eq!(protected.unlock().unwrap().as_str(), password);
            app.enter_primary();
            allow_background_task_to_run(&mut app).await;
            assert!(!directory.path().join("credentials.yaml").exists());
            app.handle_key(key(KeyCode::Char('y')));
            allow_background_task_to_run(&mut app).await;
            let path = directory.path().join("credentials.yaml");
            assert_eq!(path.exists(), succeeds);
            if succeeds {
                assert!(!std::fs::read_to_string(&path).unwrap().contains(password));
                let registry = CredentialRegistry::load(&path).unwrap();
                let summary = registry.summaries().pop().unwrap();
                let Some(SshCredential::Password { protected }) = registry.resolve(&summary.handle)
                else {
                    panic!("saved password expected")
                };
                assert_eq!(protected.unlock().unwrap().as_str(), password);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tui_setup_service_failure_returns_to_host_key_confirmation_without_writing() {
        let (directory, mut app, draft, _) =
            app_with_fake_setup(FakeSetupMode::AuthenticationFailure);
        app.screen = Screen::HostKeyConfirm {
            draft,
            fingerprint: HostKeyFingerprint::parse("SHA256:fake-host").unwrap(),
        };
        app.handle_key(key(KeyCode::Char('y')));
        allow_background_task_to_run(&mut app).await;

        assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));
        assert!(
            app.message
                .as_deref()
                .is_some_and(|message| message.contains("authentication"))
        );
        assert!(!directory.path().join("destinations.yaml").exists());
        assert!(!directory.path().join("credentials.yaml").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_tui_probe_propagates_to_setup_service_and_ignores_late_result() {
        let (_directory, mut app, draft, gateway) =
            app_with_fake_setup(FakeSetupMode::WaitForCancellation);
        app.start_host_key_probe(&draft);
        assert!(matches!(app.screen, Screen::HostKeyPending { .. }));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !gateway.probe_started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the capture must enter the gateway before testing in-flight cancellation");
        app.handle_key(key(KeyCode::Esc));
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::NewSshDestination(_))
        })
        .await;

        assert!(gateway.cancellation_observed.load(Ordering::SeqCst));
        assert!(matches!(app.screen, Screen::NewSshDestination(_)));
    }
}
