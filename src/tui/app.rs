use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    application::{
        DeploymentPlan, DeploymentReport, DeploymentSelection, DeploymentService,
        DeploymentSession, DestinationSetupRequest, DestinationSetupService, EndpointProbeRequest,
        LocalIdentityCandidate, RemoteSetupCandidates, SetupCredential, SetupRootState,
    },
    config::{
        CredentialRegistry, DestinationRegistry, DestinationSettings, DestinationSummary,
        EnvironmentSetup, HostKeyFingerprint, PreparedProjectInitialization, ProjectConfig,
        ProjectSetup, SshCandidate, SshCredential, TargetSetup, default_remote_root,
        discover_local_ssh, prepare_initialize,
    },
    domain::{ComponentName, DestinationKey},
    drivers::{CredentialHandle, DriverDestinationInput, DriverKind, DriverLog, EventSink},
    projects::{
        DiscoveryReport, ProjectRegistry, ProjectRegistryError, ProjectSelection, ProjectStatus,
        discover_components, register_initialized_project, select_project, suggest_project_name,
    },
};

#[derive(Clone, Debug)]
pub(super) enum Screen {
    Projects,
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
        logs: VecDeque<DriverLog>,
    },
    DeploymentFinished {
        root: PathBuf,
        config: ProjectConfig,
        summary: String,
        scroll: u16,
        logs: VecDeque<DriverLog>,
    },
    SetupComponents(ComponentSetupState),
    SetupDestinations(DestinationSetupState),
    NewSshDestination(NewSshDestinationState),
    HostKeyPending {
        draft: NewSshDestinationState,
        cancellation: tokio_util::sync::CancellationToken,
    },
    HostKeyConfirm {
        draft: NewSshDestinationState,
        fingerprint: HostKeyFingerprint,
    },
    SshAuthenticationPending {
        draft: NewSshDestinationState,
        fingerprint: HostKeyFingerprint,
        cancellation: tokio_util::sync::CancellationToken,
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
            Self::Saved { label, .. }
            | Self::Agent { label, .. }
            | Self::IdentityFile { label, .. } => label,
        }
    }

    fn identity(&self) -> String {
        match self {
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
        let destinations =
            DestinationRegistry::load(&self.destinations).map_err(|error| error.to_string())?;
        let credentials = Arc::new(
            CredentialRegistry::load(&self.credentials).map_err(|error| error.to_string())?,
        );
        let drivers = crate::bootstrap::deployment_driver_registry(credentials)
            .map_err(|error| error.to_string())?;
        DeploymentService::new(Arc::new(drivers), self.history.clone())
            .plan(selection, &destinations, cancellation)
            .await
            .map_err(|error| error.to_string())
    }

    async fn execute(
        &self,
        plan: DeploymentPlan,
        events: &dyn EventSink,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<DeploymentReport, String> {
        let credentials = Arc::new(
            CredentialRegistry::load(&self.credentials).map_err(|error| error.to_string())?,
        );
        let drivers = crate::bootstrap::deployment_driver_registry(credentials)
            .map_err(|error| error.to_string())?;
        DeploymentService::new(Arc::new(drivers), self.history.clone())
            .execute(plan, &self.destinations, events, cancellation)
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug)]
pub(super) struct NewSshDestinationState {
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
    AgentIdentities(Result<Vec<LocalIdentityCandidate>, String>),
    HostKey(Result<String, String>),
    Authentication(Result<RemoteSetupCandidates, String>),
    DeploymentPlan(uuid::Uuid, Result<DeploymentPlan, String>),
    DeploymentProgress(DriverLog),
    DeploymentFinished(Result<DeploymentReport, String>),
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
    pub systemd: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct RemoteSetupSelectionState {
    pub destinations: DestinationSetupState,
    pub component: ComponentName,
    pub root: String,
    pub root_state: SetupRootState,
    pub systemd_units: Vec<String>,
    pub cursor: usize,
    pub notices: Vec<String>,
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
}

impl KeyFileBrowser {
    fn open(directory: &Path) -> Result<Self, std::io::Error> {
        let directory = std::fs::canonicalize(directory)?;
        let mut entries = std::fs::read_dir(&directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        entries.sort_by_cached_key(|path| {
            (
                !path.is_dir(),
                path.file_name()
                    .map(|name| name.to_string_lossy().to_lowercase())
                    .unwrap_or_default(),
            )
        });
        Ok(Self {
            directory,
            entries,
            selected: 0,
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
        let mut children = std::fs::read_dir(&directory)
            .map_err(|source| ProjectRegistryError::browser(&directory, source))?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(std::fs::FileType::is_dir)
                    .map(|_| entry.path())
            })
            .collect::<Vec<_>>();
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
    deployment_gateway: Arc<dyn TuiDeploymentGateway>,
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
        Self::new_with_services(
            registry_path,
            destination_registry_path,
            initial_directory,
            setup_service,
            deployment_gateway,
        )
    }

    fn new_with_services(
        registry_path: PathBuf,
        destination_registry_path: PathBuf,
        initial_directory: &Path,
        setup_service: DestinationSetupService,
        deployment_gateway: Arc<dyn TuiDeploymentGateway>,
    ) -> Result<Self, ProjectRegistryError> {
        let initial_directory = std::fs::canonicalize(initial_directory)
            .map_err(|source| ProjectRegistryError::browser(initial_directory, source))?;
        let recent = ProjectRegistry::load(&registry_path)?.statuses();
        let credential_registry_path = destination_registry_path.with_file_name("credentials.yaml");
        let (background_sender, background_receiver) = mpsc::sync_channel(256);
        Ok(Self {
            screen: Screen::Projects,
            recent,
            selected_recent: 0,
            message: None,
            deployment_session: Arc::new(DeploymentSession::default()),
            registry_path,
            destination_registry_path,
            credential_registry_path,
            initial_directory,
            home_directory: crate::adapters::user_home_directory().ok(),
            runtime: tokio::runtime::Handle::try_current().ok(),
            background_sender,
            background_receiver,
            setup_service,
            deployment_gateway,
        })
    }

    pub fn poll_background(&mut self) {
        for _ in 0..256 {
            let Ok(event) = self.background_receiver.try_recv() else {
                break;
            };
            match event {
                BackgroundEvent::AgentIdentities(result) => match &mut self.screen {
                    Screen::NewSshDestination(draft) | Screen::KeyBrowser { draft, .. } => {
                        apply_agent_identities(draft, result);
                    }
                    _ => {}
                },
                BackgroundEvent::HostKey(result) => {
                    let Screen::HostKeyPending { draft, .. } = self.screen.clone() else {
                        continue;
                    };
                    match result {
                        Ok(fingerprint) => match HostKeyFingerprint::parse(fingerprint) {
                            Ok(fingerprint) => {
                                self.screen = Screen::HostKeyConfirm { draft, fingerprint };
                            }
                            Err(error) => {
                                self.screen = Screen::NewSshDestination(draft);
                                self.message = Some(error.to_string());
                            }
                        },
                        Err(error) => {
                            self.screen = Screen::NewSshDestination(draft);
                            self.message = Some(error);
                        }
                    }
                }
                BackgroundEvent::Authentication(result) => {
                    let Screen::SshAuthenticationPending {
                        draft, fingerprint, ..
                    } = self.screen.clone()
                    else {
                        continue;
                    };
                    match result {
                        Ok(candidates) => match self.commit_ssh_destination(&draft, &fingerprint) {
                            Ok(setup) => {
                                let Some(component) = selected_components(&setup)
                                    .get(setup.component_cursor)
                                    .cloned()
                                else {
                                    self.screen = Screen::SetupDestinations(setup);
                                    continue;
                                };
                                let root = default_remote_root(
                                    &suggest_project_name(&setup.components.root),
                                    "production",
                                    &component,
                                );
                                self.screen =
                                    Screen::RemoteSetupSelection(RemoteSetupSelectionState {
                                        destinations: setup,
                                        component,
                                        root,
                                        root_state: candidates.root,
                                        systemd_units: candidates.services,
                                        cursor: 0,
                                        notices: candidates.notices,
                                    });
                            }
                            Err(error) => {
                                self.screen = Screen::HostKeyConfirm { draft, fingerprint };
                                self.message = Some(error);
                            }
                        },
                        Err(error) => {
                            self.screen = Screen::HostKeyConfirm { draft, fingerprint };
                            self.message = Some(error);
                        }
                    }
                }
                BackgroundEvent::DeploymentPlan(completed_id, result) => {
                    self.finish_deployment_plan(completed_id, result);
                }
                BackgroundEvent::DeploymentProgress(log) => {
                    if let Screen::DeploymentRunning { logs, .. } = &mut self.screen {
                        push_bounded_log(logs, log);
                    }
                }
                BackgroundEvent::DeploymentFinished(result) => {
                    self.finish_deployment(result);
                }
            }
        }
    }

    fn finish_deployment_plan(
        &mut self,
        completed_id: uuid::Uuid,
        result: Result<DeploymentPlan, String>,
    ) {
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
        match result {
            Ok(plan) => self.screen = Screen::DeploymentReview { plan, scroll: 0 },
            Err(error) => {
                self.screen = Screen::DeploySelection(selection);
                self.message = Some(error);
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // In raw mode Ctrl+C is an input event, not a process signal. Never
        // let modified shortcuts fall through to plain confirmation keys.
        if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                self.request_deployment_cancellation();
            }
            return false;
        }
        self.message = None;
        match self.screen.clone() {
            Screen::Projects => {
                if key.code == KeyCode::Char('q') {
                    return true;
                }
                self.handle_projects(key.code);
            }
            Screen::Browser(browser) => self.handle_browser(key.code, &browser),
            Screen::SetupComponents(setup) => self.handle_setup_components(key.code, &setup),
            Screen::SetupDestinations(setup) => self.handle_setup_destinations(key.code, &setup),
            Screen::NewSshDestination(draft) => {
                self.handle_new_ssh_destination(key.code, &draft);
            }
            Screen::HostKeyPending {
                draft,
                cancellation,
            } => {
                if key.code == KeyCode::Esc {
                    cancellation.cancel();
                    self.screen = Screen::NewSshDestination(draft);
                }
            }
            Screen::HostKeyConfirm { draft, fingerprint } => {
                self.handle_host_key_confirm(key.code, &draft, &fingerprint);
            }
            Screen::SshAuthenticationPending {
                draft,
                fingerprint,
                cancellation,
            } => {
                if key.code == KeyCode::Esc {
                    cancellation.cancel();
                    self.screen = Screen::HostKeyConfirm { draft, fingerprint };
                }
            }
            Screen::RemoteSetupSelection(selection) => {
                self.handle_remote_setup_selection(key.code, &selection);
            }
            Screen::KeyBrowser { draft, browser } => {
                self.handle_key_browser(key.code, &draft, &browser);
            }
            Screen::SetupReview {
                destinations,
                prepared,
                ..
            } => self.handle_setup_review(key.code, &destinations, &prepared),
            Screen::Overview { root, config } => match key.code {
                KeyCode::Esc => self.screen = Screen::Projects,
                KeyCode::Char('q') => return true,
                KeyCode::Char('d') => self.open_deployment(root, config),
                _ => {}
            },
            Screen::DeploySelection(selection) => {
                self.handle_deploy_selection(key.code, &selection);
            }
            Screen::DeploymentPlanning {
                selection,
                cancellation,
                ..
            } => {
                if key.code == KeyCode::Esc {
                    cancellation.cancel();
                    self.screen = Screen::DeploySelection(selection);
                }
            }
            Screen::DeploymentReview { plan, .. } => {
                self.handle_deployment_review(key.code, plan);
            }
            Screen::DeploymentRunning { .. } => {
                if key.code == KeyCode::Esc {
                    self.request_deployment_cancellation();
                }
            }
            Screen::DeploymentFinished { root, config, .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.screen = Screen::Overview { root, config };
                } else if let Screen::DeploymentFinished { scroll, .. } = &mut self.screen {
                    *scroll = match key.code {
                        KeyCode::Up => scroll.saturating_sub(1),
                        KeyCode::Down => scroll.saturating_add(1),
                        KeyCode::PageUp => scroll.saturating_sub(10),
                        KeyCode::PageDown => scroll.saturating_add(10),
                        _ => *scroll,
                    };
                }
            }
        }
        false
    }

    fn request_deployment_cancellation(&mut self) {
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
        match &self.screen {
            Screen::DeploymentPlanning { cancellation, .. }
            | Screen::HostKeyPending { cancellation, .. }
            | Screen::SshAuthenticationPending { cancellation, .. } => cancellation.cancel(),
            _ => {}
        }
        self.request_deployment_cancellation();
        // The Running screen is set before the worker acquires its session
        // permit, so waiting on is_active alone would introduce an exit race.
        while matches!(self.screen, Screen::DeploymentRunning { .. }) {
            self.poll_background();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn open_deployment(&mut self, root: PathBuf, config: ProjectConfig) {
        let selected = config
            .environments
            .values()
            .next()
            .map(|environment| environment.components.keys().cloned().collect())
            .unwrap_or_default();
        self.screen = Screen::DeploySelection(DeploySelectionState {
            root,
            config,
            environment_cursor: 0,
            component_cursor: 0,
            selected,
        });
    }

    fn handle_deploy_selection(&mut self, key: KeyCode, selection: &DeploySelectionState) {
        match key {
            KeyCode::Left => self.move_environment(selection, false),
            KeyCode::Right => self.move_environment(selection, true),
            KeyCode::Up => self.move_deploy_component(false),
            KeyCode::Down => self.move_deploy_component(true),
            _ => self.handle_deploy_action(key, selection),
        }
    }

    fn move_environment(&mut self, selection: &DeploySelectionState, forward: bool) {
        let count = selection.config.environments.len();
        let cursor = if forward {
            (selection.environment_cursor + 1).min(count.saturating_sub(1))
        } else {
            selection.environment_cursor.saturating_sub(1)
        };
        self.select_environment(cursor);
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
        if let Screen::DeploySelection(selection) = &mut self.screen {
            selection.environment_cursor = cursor;
            selection.component_cursor = 0;
            selection.selected = selection
                .config
                .environments
                .values()
                .nth(cursor)
                .map(|environment| environment.components.keys().cloned().collect())
                .unwrap_or_default();
        }
    }

    fn handle_deploy_action(&mut self, key: KeyCode, selection: &DeploySelectionState) {
        match key {
            KeyCode::Char(' ') => {
                let components = deployment_components(selection);
                if let Some(component) = components.get(selection.component_cursor)
                    && let Screen::DeploySelection(current) = &mut self.screen
                    && !current.selected.remove(component)
                {
                    current.selected.insert(component.clone());
                }
            }
            KeyCode::Enter => self.start_deployment_plan(selection),
            KeyCode::Esc => {
                self.screen = Screen::Overview {
                    root: selection.root.clone(),
                    config: selection.config.clone(),
                };
            }
            _ => {}
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
        let result = spawn_plan_thread(
            request_id,
            runtime,
            Arc::clone(&self.deployment_gateway),
            request,
            cancellation.clone(),
            self.background_sender.clone(),
        );
        if let Err(error) = result {
            self.message = Some(format!("Could not start Deployment check: {error}"));
            return;
        }
        self.screen = Screen::DeploymentPlanning {
            request_id,
            selection: selection.clone(),
            cancellation,
        };
    }

    fn handle_deployment_review(&mut self, key: KeyCode, plan: DeploymentPlan) {
        match key {
            KeyCode::Up => self.adjust_review_scroll(false, 1),
            KeyCode::Down => self.adjust_review_scroll(true, 1),
            KeyCode::PageUp => self.adjust_review_scroll(false, 10),
            KeyCode::PageDown => self.adjust_review_scroll(true, 10),
            KeyCode::Char('c') => self.start_deployment(plan),
            KeyCode::Esc => self.screen = Screen::DeploySelection(selection_state(&plan.selection)),
            _ => {}
        }
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
        let cancellation = tokio_util::sync::CancellationToken::new();
        let result = spawn_execute_thread(
            runtime,
            Arc::clone(&self.deployment_gateway),
            Arc::clone(&self.deployment_session),
            plan,
            cancellation.clone(),
            self.background_sender.clone(),
        );
        if let Err(error) = result {
            self.message = Some(format!("Could not start Deployment: {error}"));
            return;
        }
        self.screen = Screen::DeploymentRunning {
            root,
            config,
            cancellation,
            cancellation_requested: false,
            logs: VecDeque::new(),
        };
    }

    fn finish_deployment(&mut self, result: Result<DeploymentReport, String>) {
        let Screen::DeploymentRunning {
            root, config, logs, ..
        } = self.screen.clone()
        else {
            return;
        };
        let summary = match result {
            Ok(report) => deployment_summary(&report),
            Err(error) => format!("Deployment did not complete: {error}"),
        };
        self.screen = Screen::DeploymentFinished {
            root,
            config,
            summary,
            scroll: 0,
            logs,
        };
    }

    fn handle_setup_components(&mut self, key: KeyCode, setup: &ComponentSetupState) {
        match key {
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
                    Err(error) => self.message = Some(error.to_string()),
                }
            }
            KeyCode::Esc => self.screen = Screen::Projects,
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
                    prepare_initialize(project).map_err(|error| error.to_string())
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
            Err(error) => {
                self.message = Some(error.to_string());
                return;
            }
        };
        let registry = match CredentialRegistry::load(&self.credential_registry_path) {
            Ok(registry) => registry,
            Err(error) => {
                self.message = Some(error.to_string());
                return;
            }
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
        let Some(runtime) = &self.runtime else {
            if let Screen::NewSshDestination(draft) = &mut self.screen {
                draft.agent_status = "SSH Agent: unavailable in this runtime".into();
            }
            return;
        };
        let sender = self.background_sender.clone();
        let setup_service = self.setup_service.clone();
        runtime.spawn(async move {
            let result = setup_service
                .discover_local_identities(&tokio_util::sync::CancellationToken::new())
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(BackgroundEvent::AgentIdentities(result));
        });
    }

    fn handle_new_ssh_destination(&mut self, key: KeyCode, draft: &NewSshDestinationState) {
        match key {
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
            KeyCode::Char('f') => self.open_key_browser(draft),
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
            Err(error) => self.message = Some(format!("Cannot browse SSH keys: {error}")),
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
                    self.message = Some("Select a private-key file, not a directory".into());
                    return;
                };
                let path = match std::fs::canonicalize(path) {
                    Ok(path) => path,
                    Err(error) => {
                        self.message = Some(format!("Cannot select SSH key: {error}"));
                        return;
                    }
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
            Err(error) => self.message = Some(format!("Cannot browse SSH keys: {error}")),
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
        let Some(runtime) = &self.runtime else {
            self.message = Some("SSH probe runtime is unavailable".into());
            return;
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let sender = self.background_sender.clone();
        let setup_service = self.setup_service.clone();
        let request = EndpointProbeRequest {
            driver: DriverKind::linux_ssh(),
            destination: DriverDestinationInput {
                value: serde_json::json!({ "host": draft.host, "port": port }),
            },
        };
        runtime.spawn(async move {
            let result = setup_service
                .capture_endpoint_identity(&request, Duration::from_secs(10), &task_cancellation)
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(BackgroundEvent::HostKey(result));
        });
        self.screen = Screen::HostKeyPending {
            draft: draft.clone(),
            cancellation,
        };
    }

    fn handle_host_key_confirm(
        &mut self,
        key: KeyCode,
        draft: &NewSshDestinationState,
        fingerprint: &HostKeyFingerprint,
    ) {
        match key {
            KeyCode::Char('y') | KeyCode::Enter => self.start_authentication(draft, fingerprint),
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
        let Some(runtime) = &self.runtime else {
            self.message = Some("SSH authentication runtime is unavailable".into());
            return;
        };
        let credential = match self.resolve_draft_credential(draft) {
            Ok(credential) => credential,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        let destination = DriverDestinationInput {
            value: serde_json::json!({
                "host": draft.host,
                "port": draft.port.parse::<u16>().unwrap_or_default(),
                "user": draft.user,
                "hostKey": fingerprint.as_str(),
            }),
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let sender = self.background_sender.clone();
        let setup_service = self.setup_service.clone();
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
        runtime.spawn(async move {
            let result = setup_service
                .authenticate_and_probe(
                    &request,
                    Duration::from_secs(15),
                    Duration::from_secs(10),
                    &task_cancellation,
                )
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(BackgroundEvent::Authentication(result));
        });
        self.screen = Screen::SshAuthenticationPending {
            draft: draft.clone(),
            fingerprint: fingerprint.clone(),
            cancellation,
        };
    }

    fn handle_remote_setup_selection(
        &mut self,
        key: KeyCode,
        selection: &RemoteSetupSelectionState,
    ) {
        match key {
            KeyCode::Up => {
                if let Screen::RemoteSetupSelection(current) = &mut self.screen {
                    current.cursor = current.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Screen::RemoteSetupSelection(current) = &mut self.screen {
                    current.cursor = (current.cursor + 1).min(current.systemd_units.len());
                }
            }
            KeyCode::Enter => {
                let mut destinations = selection.destinations.clone();
                let systemd = selection
                    .cursor
                    .checked_sub(1)
                    .and_then(|index| selection.systemd_units.get(index))
                    .cloned();
                destinations.target_settings.insert(
                    selection.component.clone(),
                    ComponentTargetSettings { systemd },
                );
                self.screen = Screen::SetupDestinations(destinations);
            }
            KeyCode::Esc => {
                self.screen = Screen::SetupDestinations(selection.destinations.clone());
            }
            _ => {}
        }
    }

    fn resolve_draft_credential(
        &self,
        draft: &NewSshDestinationState,
    ) -> Result<SshCredential, String> {
        match draft
            .selected_credential()
            .ok_or_else(|| "Select an SSH identity".to_owned())?
        {
            CredentialChoice::Saved { handle, .. } => {
                CredentialRegistry::load(&self.credential_registry_path)
                    .map_err(|error| error.to_string())?
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
            .map_err(|error| error.to_string())?;
        let mut credentials = original_credentials.clone();
        let (credential, created_credential) = match choice {
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
                    .map_err(|error| error.to_string())?,
                true,
            ),
            CredentialChoice::IdentityFile { path, .. } => (
                credentials
                    .create(SshCredential::IdentityFile { path: path.clone() })
                    .map_err(|error| error.to_string())?,
                true,
            ),
        };
        let mut destinations = DestinationRegistry::load(&self.destination_registry_path)
            .map_err(|error| error.to_string())?;
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
            .map_err(|error| error.to_string())?;

        if created_credential {
            credentials
                .save(&self.credential_registry_path)
                .map_err(|error| error.to_string())?;
        }
        if let Err(error) = destinations.save(&self.destination_registry_path) {
            if created_credential
                && let Err(rollback_error) =
                    original_credentials.save(&self.credential_registry_path)
            {
                return Err(format!(
                    "Destination was not saved: {error}; credential rollback also failed: {rollback_error}"
                ));
            }
            return Err(error.to_string());
        }

        let mut setup = draft.destinations.clone();
        setup.destinations = destinations.summaries();
        setup.destination_cursor = setup
            .destinations
            .iter()
            .position(|destination| destination.key == key)
            .unwrap_or_default();
        if let Some(component) = selected_components(&setup).get(setup.component_cursor) {
            setup.assignments.insert(component.clone(), key);
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
                        if let Err(error) =
                            register_initialized_project(&self.registry_path, &root, now_unix_ms())
                        {
                            self.message = Some(format!(
                                "Configuration saved, but recent-project registration failed: {error}"
                            ));
                        } else {
                            self.refresh_recent();
                        }
                        self.screen = Screen::Overview { root, config };
                    }
                    Err(error) => self.message = Some(error.to_string()),
                }
            }
            KeyCode::Esc => self.screen = Screen::SetupDestinations(destinations.clone()),
            _ => {}
        }
    }

    fn handle_projects(&mut self, key: KeyCode) {
        let item_count = self.recent.len() + 1;
        match key {
            KeyCode::Up => self.selected_recent = self.selected_recent.saturating_sub(1),
            KeyCode::Down => {
                self.selected_recent = (self.selected_recent + 1).min(item_count - 1);
            }
            KeyCode::Enter if self.selected_recent < self.recent.len() => {
                let project = self.recent[self.selected_recent].clone();
                if project.available {
                    self.select_root(&project.project.root);
                } else {
                    self.message =
                        Some("Project directory or shipforge.yaml is unavailable".into());
                }
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                match DirectoryBrowser::open(&self.initial_directory) {
                    Ok(browser) => self.screen = Screen::Browser(browser),
                    Err(error) => self.message = Some(error.to_string()),
                }
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
            KeyCode::Esc => self.screen = Screen::Projects,
            _ => {}
        }
    }

    fn open_browser(&mut self, directory: &Path) {
        match DirectoryBrowser::open(directory) {
            Ok(browser) => self.screen = Screen::Browser(browser),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    fn select_root(&mut self, root: &Path) {
        match select_project(&self.registry_path, root, now_unix_ms()) {
            Ok(ProjectSelection::Existing { root, config }) => {
                self.screen = Screen::Overview { root, config };
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
                Err(error) => self.message = Some(error.to_string()),
            },
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    fn refresh_recent(&mut self) {
        match ProjectRegistry::load(&self.registry_path) {
            Ok(registry) => self.recent = registry.statuses(),
            Err(error) => self.message = Some(error.to_string()),
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
    sender: SyncSender<BackgroundEvent>,
}

impl EventSink for ProgressEvents {
    fn emit(&self, mut event: DriverLog) {
        // Progress is a bounded UI projection, not the durable operation log.
        // A slow renderer must not stall cancellation or remote recovery.
        event.namespace = event.namespace.chars().take(128).collect();
        event.message = event.message.chars().take(4096).collect();
        let _ = self
            .sender
            .try_send(BackgroundEvent::DeploymentProgress(event));
    }
}

fn push_bounded_log(logs: &mut VecDeque<DriverLog>, log: DriverLog) {
    const MAX_LOGS: usize = 500;
    if logs.len() == MAX_LOGS {
        logs.pop_front();
    }
    logs.push_back(log);
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
                let _ = writeln!(summary, "{component} / {stage:?}: {error}");
            }
            DeploymentFailure::Contract {
                component,
                stage,
                message,
                ..
            } => {
                let _ = writeln!(summary, "{component} / {stage:?}: {message}");
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
        let _ = writeln!(summary, "MANUAL RECOVERY REQUIRED — {component}: {error}");
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
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("shipforge-deployment-plan".into())
        .spawn(move || {
            let result =
                catch_worker_failure(|| runtime.block_on(gateway.plan(selection, &cancellation)));
            let _ = sender.send(BackgroundEvent::DeploymentPlan(request_id, result));
        })
        .map(drop)
}

fn spawn_execute_thread(
    runtime: tokio::runtime::Handle,
    gateway: Arc<dyn TuiDeploymentGateway>,
    session: Arc<DeploymentSession>,
    plan: DeploymentPlan,
    cancellation: tokio_util::sync::CancellationToken,
    sender: SyncSender<BackgroundEvent>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("shipforge-deployment-run".into())
        .spawn(move || {
            let events = ProgressEvents {
                sender: sender.clone(),
            };
            let result = catch_worker_failure(|| {
                runtime.block_on(async {
                    session
                        .run(gateway.execute(plan, &events, &cancellation))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result)
                })
            });
            let _ = sender.send(BackgroundEvent::DeploymentFinished(result));
        })
        .map(drop)
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
                    root: None,
                    systemd: setup
                        .target_settings
                        .get(component)
                        .and_then(|settings| settings.systemd.clone()),
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
        assert!(!app.handle_key(key(KeyCode::Enter)));
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::DeploymentReview { .. })
        })
        .await;
        assert!(!gateway.executed.load(Ordering::SeqCst));
        assert!(!app.handle_key(key(KeyCode::Enter)));
        assert!(!gateway.executed.load(Ordering::SeqCst));
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            assert!(!app.handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers)));
            assert!(matches!(app.screen, Screen::DeploymentReview { .. }));
            assert!(!gateway.executed.load(Ordering::SeqCst));
        }
        assert!(!app.handle_key(key(KeyCode::Char('c'))));
        assert!(matches!(app.screen, Screen::DeploymentRunning { .. }));
        assert!(!app.handle_key(key(KeyCode::Char('q'))));
        assert!(matches!(app.screen, Screen::DeploymentRunning { .. }));
        if let Some(cancel_key) = cancel_key {
            assert!(!app.handle_key(cancel_key));
            assert!(matches!(
                app.screen,
                Screen::DeploymentRunning {
                    cancellation_requested: true,
                    ..
                }
            ));
        } else {
            app.shutdown();
            assert!(matches!(app.screen, Screen::DeploymentFinished { .. }));
        }
        wait_for_screen(&mut app, |screen| {
            matches!(screen, Screen::DeploymentFinished { .. })
        })
        .await;
        assert!(gateway.executed.load(Ordering::SeqCst));
        assert!(gateway.cancelled.load(Ordering::SeqCst));
        assert!(!app.deployment_session.is_active());
        assert!(!app.handle_key(key(KeyCode::Enter)));
        assert!(matches!(app.screen, Screen::Overview { .. }));
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
        app.screen = Screen::DeploymentPlanning {
            request_id,
            selection,
            cancellation: tokio_util::sync::CancellationToken::new(),
        };
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
        assert!(summary.contains("inspect current before retrying"));
        assert!(summary.contains("none reported"));
        assert!(summary.contains("WARNING — Deployment log incomplete: disk full"));
    }

    #[test]
    fn progress_flood_is_nonblocking_and_bounds_unicode_payloads() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let events = ProgressEvents { sender };
        for _ in 0..1000 {
            events.emit(DriverLog {
                namespace: "界".repeat(200),
                message: "界".repeat(5000),
            });
        }
        let retained: Vec<_> = receiver.try_iter().collect();
        assert_eq!(retained.len(), 2);
        for event in retained {
            let BackgroundEvent::DeploymentProgress(log) = event else {
                panic!("expected a progress projection");
            };
            assert_eq!(log.namespace.chars().count(), 128);
            assert_eq!(log.message.chars().count(), 4096);
        }
        drop(receiver);
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
    ) -> (TempDir, App, NewSshDestinationState, Arc<AtomicBool>) {
        let directory = tempdir().unwrap();
        std::fs::write(
            directory.path().join("package.json"),
            r#"{"name":"web","scripts":{"build":"vite build"}}"#,
        )
        .unwrap();
        let identity = directory.path().join("id_ed25519");
        std::fs::write(&identity, "test fixture only").unwrap();
        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let service = DestinationSetupService::new(Arc::new(FakeSetupGateway {
            mode,
            cancellation_observed: cancellation_observed.clone(),
        }));
        let mut app = App::new_with_setup(
            directory.path().join("projects.yaml"),
            directory.path().join("destinations.yaml"),
            directory.path(),
            service,
        )
        .unwrap();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));
        let Screen::SetupDestinations(destinations) = app.screen.clone() else {
            panic!("expected Destination setup");
        };
        let draft = NewSshDestinationState {
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
        (directory, app, draft, cancellation_observed)
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

        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
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

        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));
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
                    systemd: Some("web.service".into()),
                },
            );
        }

        app.handle_key(key(KeyCode::Char('n')));
        let Screen::SetupReview { prepared, .. } = &app.screen else {
            panic!("expected setup review");
        };
        assert!(prepared.preview().contains("project:"));
        assert!(prepared.preview().contains(destination_key.as_str()));
        assert!(prepared.preview().contains("systemd: web.service"));
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

        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Char('f')));
        assert!(matches!(app.screen, Screen::KeyBrowser { .. }));
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));
        let Screen::SetupDestinations(destinations) = app.screen.clone() else {
            panic!("expected Destination setup");
        };
        let draft = NewSshDestinationState {
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
        app.screen = Screen::SshAuthenticationPending {
            draft,
            fingerprint,
            cancellation: tokio_util::sync::CancellationToken::new(),
        };
        app.background_sender
            .send(BackgroundEvent::Authentication(Ok(RemoteSetupCandidates {
                root: SetupRootState::Missing,
                services: Vec::new(),
                notices: Vec::new(),
            })))
            .unwrap();
        app.poll_background();

        let Screen::RemoteSetupSelection(selection) = &app.screen else {
            panic!("expected remote setup selection after authentication");
        };
        assert_eq!(selection.root_state, SetupRootState::Missing);
        app.handle_key(key(KeyCode::Enter));
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
        app.screen = Screen::SshAuthenticationPending {
            draft,
            fingerprint,
            cancellation: tokio_util::sync::CancellationToken::new(),
        };
        app.background_sender
            .send(BackgroundEvent::Authentication(Err(
                "SSH server rejected the selected identity".into(),
            )))
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
        app.screen = Screen::KeyBrowser {
            draft,
            browser: KeyFileBrowser::open(directory.path()).unwrap(),
        };
        app.background_sender
            .send(BackgroundEvent::AgentIdentities(Ok(vec![
                LocalIdentityCandidate {
                    reference: "SHA256:agent-key".into(),
                    label: "deploy".into(),
                },
            ])))
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

        app.handle_key(key(KeyCode::Enter));
        allow_background_task_to_run(&mut app).await;
        let Screen::RemoteSetupSelection(selection) = &app.screen else {
            panic!("expected remote setup candidates");
        };
        assert_eq!(selection.root_state, SetupRootState::WritableDirectory);
        assert_eq!(selection.systemd_units, vec!["web.service"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tui_setup_service_failure_returns_to_host_key_confirmation_without_writing() {
        let (directory, mut app, draft, _) =
            app_with_fake_setup(FakeSetupMode::AuthenticationFailure);
        app.screen = Screen::HostKeyConfirm {
            draft,
            fingerprint: HostKeyFingerprint::parse("SHA256:fake-host").unwrap(),
        };
        app.handle_key(key(KeyCode::Enter));
        allow_background_task_to_run(&mut app).await;

        assert!(matches!(app.screen, Screen::HostKeyConfirm { .. }));
        assert!(
            app.message
                .as_deref()
                .is_some_and(|message| message.contains("rejected"))
        );
        assert!(!directory.path().join("destinations.yaml").exists());
        assert!(!directory.path().join("credentials.yaml").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_tui_probe_propagates_to_setup_service_and_ignores_late_result() {
        let (_directory, mut app, draft, cancellation_observed) =
            app_with_fake_setup(FakeSetupMode::WaitForCancellation);
        app.start_host_key_probe(&draft);
        assert!(matches!(app.screen, Screen::HostKeyPending { .. }));
        app.handle_key(key(KeyCode::Esc));
        allow_background_task_to_run(&mut app).await;

        assert!(cancellation_observed.load(Ordering::SeqCst));
        assert!(matches!(app.screen, Screen::NewSshDestination(_)));
    }
}
