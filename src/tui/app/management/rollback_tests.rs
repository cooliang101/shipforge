//! Real application plans with an in-memory Driver; no SSH or build execution.

use std::{any::Any, sync::Mutex, time::Duration};

use async_trait::async_trait;
use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use super::*;
use crate::{
    application::{RollbackService, history_query::HistoryQueryService},
    config::{DestinationRegistry, DestinationSettings, HostKeyFingerprint},
    domain::{
        Capability, ComponentRelease, DeploymentState, DriverCapabilities, ReleaseManifest,
        ReleaseVersion,
    },
    drivers::{
        ActivationReceipt, CleanupReport, ComponentExecutionContext, ComponentInventory,
        ComponentPlan, ComponentRequest, CredentialHandle, DeploymentDriver,
        DriverDestinationInput, DriverError, DriverKind, DriverLog, DriverRegistry,
        DriverTargetInput, EventSink, PreflightReport, PreparedRelease, ReleaseInventory,
        ReleasePackage, RemoteAuditHistory, RetentionPolicy, ValidatedDestinationSettings,
        ValidatedTargetSettings,
        inventory::{InventoryRelease, TemporaryRemnants},
    },
    history::{DeploymentComponentSnapshot, HistoryStore},
    telemetry::Redactor,
};

#[derive(Debug)]
struct Settings(DriverKind);

impl ValidatedDestinationSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl ValidatedTargetSettings for Settings {
    fn driver_kind(&self) -> &DriverKind {
        &self.0
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug, Default)]
struct DriverState {
    current: BTreeMap<ComponentName, ReleaseRef>,
    packages: BTreeMap<ComponentName, Vec<InventoryRelease>>,
    mutations: Vec<(ComponentName, Option<ReleaseRef>)>,
}

#[derive(Debug, Default)]
struct MemoryDriver(Mutex<DriverState>);

#[async_trait]
impl DeploymentDriver for MemoryDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }
    fn static_capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::new([
            Capability::Rollback,
            Capability::Observe,
            Capability::Inventory,
            Capability::Cancellation,
        ])
    }
    fn validate_destination(
        &self,
        _: &DriverDestinationInput,
    ) -> Result<Arc<dyn ValidatedDestinationSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind())))
    }
    fn validate_target(
        &self,
        _: &DriverTargetInput,
    ) -> Result<Arc<dyn ValidatedTargetSettings>, DriverError> {
        Ok(Arc::new(Settings(self.kind())))
    }
    async fn preflight(
        &self,
        _: &ComponentExecutionContext,
    ) -> Result<PreflightReport, DriverError> {
        Ok(PreflightReport {
            effective_capabilities: self.static_capabilities(),
            notices: Vec::new(),
        })
    }
    async fn current(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Option<ReleaseRef>, DriverError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .current
            .get(&context.component)
            .cloned())
    }
    async fn inventory(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<ComponentInventory, DriverError> {
        let state = self.0.lock().unwrap();
        Ok(ComponentInventory {
            releases: ReleaseInventory {
                releases: state.packages[&context.component].clone(),
                issues: Vec::new(),
                current: Ok(state
                    .current
                    .get(&context.component)
                    .map(|item| item.version.clone())),
                notices: Vec::new(),
            },
            audit: RemoteAuditHistory::default(),
            remnants: TemporaryRemnants::default(),
        })
    }
    async fn rollback(
        &self,
        _: &DeploymentId,
        context: &ComponentExecutionContext,
        expected: Option<&ReleaseRef>,
        target: Option<&ReleaseRef>,
    ) -> Result<ActivationReceipt, DriverError> {
        let mut state = self.0.lock().unwrap();
        assert_eq!(state.current.get(&context.component), expected);
        state
            .mutations
            .push((context.component.clone(), target.cloned()));
        if let Some(target) = target {
            state
                .current
                .insert(context.component.clone(), target.clone());
        } else {
            state.current.remove(&context.component);
        }
        Ok(ActivationReceipt {
            current: target.cloned(),
            healthy: true,
            warnings: Vec::new(),
        })
    }
    async fn plan(
        &self,
        _: &ComponentExecutionContext,
        _: &ComponentRequest,
    ) -> Result<ComponentPlan, DriverError> {
        panic!("rollback UI must not plan a new build")
    }
    async fn prepare(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ComponentPlan,
        _: &ReleasePackage,
        _: &dyn EventSink,
    ) -> Result<PreparedRelease, DriverError> {
        panic!("rollback UI must not prepare a Release")
    }
    async fn activate(
        &self,
        _: &DeploymentId,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<ActivationReceipt, DriverError> {
        panic!("rollback UI must not call deployment activation")
    }
    async fn logs(
        &self,
        _: &ComponentExecutionContext,
        _: &ReleaseRef,
    ) -> Result<Vec<DriverLog>, DriverError> {
        panic!("rollback UI must not request remote logs")
    }
    async fn cleanup(
        &self,
        _: &ComponentExecutionContext,
        _: &RetentionPolicy,
    ) -> Result<CleanupReport, DriverError> {
        panic!("rollback UI must not clean releases")
    }
}

#[derive(Debug)]
struct ServiceGateway {
    service: RollbackService,
    registry: PathBuf,
    requests: Mutex<Vec<ManagementRequest>>,
}

#[async_trait(?Send)]
impl ManagementGateway for ServiceGateway {
    async fn run(
        &self,
        scope: &ManagementScope,
        request: ManagementRequest,
        cancellation: &CancellationToken,
    ) -> Result<ManagementPage, String> {
        self.requests.lock().unwrap().push(request.clone());
        match request {
            ManagementRequest::Candidates { source, selected } => {
                let candidates = self
                    .service
                    .candidates(
                        scope.selection(selected),
                        source,
                        &self.registry,
                        cancellation,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(ManagementPage::RollbackTargets {
                    candidates: Arc::new(candidates),
                    selected: BTreeSet::new(),
                    options: BTreeMap::new(),
                    cursor: 0,
                })
            }
            ManagementRequest::Plan { source, targets } => {
                let plan = self
                    .service
                    .plan(
                        scope.selection(targets.keys().cloned().collect()),
                        source,
                        targets,
                        &self.registry,
                        cancellation,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(ManagementPage::RollbackReview(Arc::new(plan)))
            }
            ManagementRequest::Execute(plan) => {
                let report = self
                    .service
                    .execute((*plan).clone(), &self.registry, cancellation)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(ManagementPage::RollbackFinished(Arc::new(report)))
            }
            _ => panic!("unexpected request in keyboard rollback flow"),
        }
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    app: App,
    driver: Arc<MemoryDriver>,
    gateway: Arc<ServiceGateway>,
    details: Arc<DeploymentDetails>,
    target: ReleaseRef,
    untouched: ReleaseRef,
    history: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let (directory, mut app) = super::tests::fixture();
        let screen = super::tests::screen(&app);
        let scope = &screen.scope;
        let mut registry = DestinationRegistry::new();
        for target in scope.config.environments[&scope.environment]
            .components
            .values()
        {
            if registry.resolve(&target.destination).is_none() {
                registry
                    .create(
                        target.destination.clone(),
                        DestinationSettings::LinuxSsh {
                            host: "rollback-ui.invalid".into(),
                            user: "deploy".into(),
                            port: 22,
                            credential: CredentialHandle::new(),
                            host_key: HostKeyFingerprint::parse("SHA256:fixture").unwrap(),
                        },
                    )
                    .unwrap();
            }
        }
        let registry_path = directory.path().join("destinations.yaml");
        registry.save(&registry_path).unwrap();
        let history_path = directory.path().join("rollback-history.sqlite3");
        let driver = Arc::new(MemoryDriver::default());
        let history = HistoryStore::open(&history_path).unwrap();
        publish(&history, scope, &registry, &driver, "v1");
        let target = driver.0.lock().unwrap().current[&name("frontend")].clone();
        let source = publish(&history, scope, &registry, &driver, "v2");
        let untouched = driver.0.lock().unwrap().current[&name("backend")].clone();
        drop(history);
        let details = Arc::new(
            HistoryQueryService::new(history_path.clone(), Redactor::default())
                .deployment(
                    &scope.config.project_id,
                    &scope.config.environments[&scope.environment].id,
                    &source,
                )
                .unwrap(),
        );
        assert_eq!(details.snapshots.len(), 2);
        let mut drivers = DriverRegistry::default();
        drivers.register(driver.clone()).unwrap();
        let gateway = Arc::new(ServiceGateway {
            service: RollbackService::new(
                Arc::new(drivers),
                history_path.clone(),
                Arc::clone(&app.deployment_session),
            ),
            registry: registry_path,
            requests: Mutex::new(Vec::new()),
        });
        app.management_gateway = gateway.clone();
        Self {
            directory,
            app,
            driver,
            gateway,
            details,
            target,
            untouched,
            history: history_path,
        }
    }

    async fn open_review(&mut self) {
        let mut screen = super::tests::screen(&self.app);
        screen.page = ManagementPage::Detail(Arc::clone(&self.details));
        self.app.screen = Screen::Management(screen);
        press(&mut self.app, KeyCode::Char('r'));
        let screen = super::tests::screen(&self.app);
        let ManagementPage::RollbackSelection {
            details, selected, ..
        } = &screen.page
        else {
            panic!("rollback Component selector expected")
        };
        assert!(selected.is_empty(), "opening rollback must select nothing");
        let index = details
            .snapshots
            .iter()
            .position(|item| item.release.component == name("frontend"))
            .unwrap();
        for _ in 0..index {
            press(&mut self.app, KeyCode::Down);
        }
        press(&mut self.app, KeyCode::Char(' '));
        press(&mut self.app, KeyCode::Enter);
        finished(&mut self.app).await;
        let screen = super::tests::screen(&self.app);
        let ManagementPage::RollbackTargets {
            candidates,
            selected,
            ..
        } = &screen.page
        else {
            panic!("rollback targets expected: {:?}", self.app.message)
        };
        assert!(selected.is_empty());
        assert_eq!(candidates.components.len(), 1);
        assert_eq!(candidates.components[0].component, name("frontend"));
        let option = candidates.components[0]
            .options
            .iter()
            .position(|item| item.target.as_ref() == Some(&self.target))
            .unwrap();
        for _ in 0..option {
            press(&mut self.app, KeyCode::Right);
        }
        press(&mut self.app, KeyCode::Char(' '));
        press(&mut self.app, KeyCode::Enter);
        finished(&mut self.app).await;
        let screen = super::tests::screen(&self.app);
        let ManagementPage::RollbackReview(plan) = &screen.page else {
            panic!("validated rollback review expected: {:?}", self.app.message)
        };
        assert_eq!(plan.source(), &self.details.record.deployment);
        assert_eq!(plan.entries().len(), 1);
        assert_eq!(plan.entries()[0].component, name("frontend"));
        assert_eq!(plan.entries()[0].target.as_ref(), Some(&self.target));
        assert_eq!(plan.execution_order(), [name("frontend")]);
        assert!(self.driver.0.lock().unwrap().mutations.is_empty());
        let text = rendered(&screen, &self.app);
        assert!(text.contains("CONFIRM ROLLBACK"));
        assert!(text.contains("[PRODUCTION] This rollback changes the selected live services"));
        assert!(screen.context_label().starts_with("[PRODUCTION]"));
        assert!(text.contains("rollback-ui.invalid"));
        assert!(text.contains("frontend"));
        assert!(text.contains("production"));
        assert!(text.contains("v1"));
        assert!(text.contains("v2"));
    }
}

fn publish(
    history: &HistoryStore,
    scope: &ManagementScope,
    registry: &DestinationRegistry,
    driver: &MemoryDriver,
    version: &str,
) -> DeploymentId {
    let id = DeploymentId::new();
    let environment = &scope.config.environments[&scope.environment];
    history
        .create_deployment(&id, &scope.config.project_id, &environment.id, 1)
        .unwrap();
    let snapshots: Vec<_> = ["backend", "frontend"]
        .into_iter()
        .enumerate()
        .map(|(index, component)| {
            let component = name(component);
            let target = &environment.components[&component];
            let destination = registry.resolve(&target.destination).unwrap();
            let release = ReleaseRef {
                driver: driver.kind(),
                project_id: scope.config.project_id.clone(),
                environment_id: environment.id.clone(),
                component: component.clone(),
                generation: target.generation,
                version: ReleaseVersion::parse(version).unwrap(),
                destination: target.destination.clone(),
                destination_revision: destination.revision,
                endpoint_fingerprint: destination.endpoint_fingerprint.clone(),
                effective_capabilities: driver.static_capabilities(),
            };
            DeploymentComponentSnapshot {
                target: Some(release.clone()),
                release,
                expected_current: driver.0.lock().unwrap().current.get(&component).cloned(),
                execution_order: u32::try_from(index).unwrap(),
            }
        })
        .collect();
    history.record_component_snapshots(&id, &snapshots).unwrap();
    history
        .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 2)
        .unwrap();
    for snapshot in snapshots {
        let release = snapshot.release;
        let manifest = ReleaseManifest::new(
            &ComponentRelease {
                project_id: release.project_id.clone(),
                environment_id: release.environment_id.clone(),
                component: release.component.clone(),
                generation: release.generation,
                version: release.version.clone(),
                destination: release.destination.clone(),
                destination_revision: release.destination_revision,
            },
            100,
            None,
        );
        history
            .record_release_package(&id, &release, &manifest, &"a".repeat(64), 512)
            .unwrap();
        history
            .record_observation(
                &id,
                &release.component,
                "preflight",
                Ok(snapshot.expected_current.as_ref()),
                None,
                3,
                &Redactor::default(),
            )
            .unwrap();
        history
            .record_observation(
                &id,
                &release.component,
                "activate-receipt",
                Ok(Some(&release)),
                Some(true),
                4,
                &Redactor::default(),
            )
            .unwrap();
        let mut state = driver.0.lock().unwrap();
        state
            .packages
            .entry(release.component.clone())
            .or_default()
            .push(InventoryRelease {
                manifest,
                sha256: "a".repeat(64),
                size: 512,
                extracted: true,
            });
        state.current.insert(release.component.clone(), release);
    }
    history
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Succeeded, 5)
        .unwrap();
    id
}

fn name(value: &str) -> ComponentName {
    ComponentName::parse(value).unwrap()
}

fn press(app: &mut App, code: KeyCode) {
    assert!(!app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)));
}

async fn finished(app: &mut App) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while app.management_task.is_some() {
            app.poll_background();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("keyboard operation must finish");
}

fn rendered(screen: &ManagementScreen, app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area(), app))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyboard_subset_rollback_requires_plain_confirmation_and_preserves_exact_target() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    let request_count = fixture.gateway.requests.lock().unwrap().len();
    press(&mut fixture.app, KeyCode::Enter);
    for modifiers in [
        KeyModifiers::SHIFT,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL,
        KeyModifiers::SUPER,
    ] {
        assert!(
            !fixture
                .app
                .handle_key(KeyEvent::new(KeyCode::Char('c'), modifiers))
        );
        assert!(
            fixture.app.management_task.is_none(),
            "modified confirmation must not start work: {modifiers:?}"
        );
        assert!(matches!(
            super::tests::screen(&fixture.app).page,
            ManagementPage::RollbackReview(_)
        ));
    }
    assert_eq!(
        fixture.gateway.requests.lock().unwrap().len(),
        request_count
    );
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
    press(&mut fixture.app, KeyCode::Esc);
    assert!(matches!(
        super::tests::screen(&fixture.app).page,
        ManagementPage::Home
    ));
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
    fixture.open_review().await;
    press(&mut fixture.app, KeyCode::Char('c'));
    finished(&mut fixture.app).await;
    let screen = super::tests::screen(&fixture.app);
    assert_completed(&fixture, &screen);
    assert_requests(&fixture);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_scope_cannot_execute_even_a_valid_sealed_rollback_plan() {
    let mut fixture = Fixture::new();
    fixture.open_review().await;
    let mut screen = super::tests::screen(&fixture.app);
    let ManagementPage::RollbackReview(plan) = &screen.page else {
        panic!("real sealed rollback plan expected");
    };
    let plan = Arc::clone(plan);
    Arc::make_mut(&mut screen.scope).historical_environment =
        Some(crate::domain::EnvironmentId::new());
    let scope = Arc::clone(&screen.scope);
    assert!(!screen.help().contains("c confirm"));
    let text = rendered(&screen, &fixture.app);
    assert!(text.contains("read-only"));
    assert!(!text.contains("CONFIRM ROLLBACK"));
    fixture.app.screen = Screen::Management(screen);
    let request_count = fixture.gateway.requests.lock().unwrap().len();
    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(fixture.app.management_task.is_none());
    assert_eq!(
        fixture.gateway.requests.lock().unwrap().len(),
        request_count
    );
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());

    let gateway = LocalManagementGateway {
        credentials: fixture
            .directory
            .path()
            .join("unavailable-credentials.yaml"),
        destinations: fixture
            .directory
            .path()
            .join("unavailable-destinations.yaml"),
        history: fixture.history.clone(),
        session: Arc::clone(&fixture.app.deployment_session),
    };
    std::fs::write(&gateway.credentials, "private-invalid-credential").unwrap();
    let error = gateway
        .run(
            &scope,
            ManagementRequest::Execute(plan),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("read-only"));
    assert!(!error.contains("Credential") && !error.contains("private-invalid"));
    assert!(fixture.driver.0.lock().unwrap().mutations.is_empty());
    assert!(!gateway.destinations.exists());
}

fn assert_completed(fixture: &Fixture, screen: &ManagementScreen) {
    let ManagementPage::RollbackFinished(report) = &screen.page else {
        panic!(
            "successful rollback result expected: {:?}",
            fixture.app.message
        )
    };
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert!(report.failure.is_none() && report.warnings.is_empty());
    assert_eq!(report.deployment.components.len(), 1);
    let state = fixture.driver.0.lock().unwrap();
    assert_eq!(
        state.mutations,
        [(name("frontend"), Some(fixture.target.clone()))]
    );
    assert_eq!(state.current[&name("frontend")], fixture.target);
    assert_eq!(state.current[&name("backend")], fixture.untouched);
    drop(state);
    let history = HistoryStore::open_existing_read_only(&fixture.history).unwrap();
    let record = history.deployment(&report.deployment.id).unwrap().unwrap();
    assert_eq!(
        record.related_deployment,
        Some(fixture.details.record.deployment.clone())
    );
    assert_eq!(
        HistoryQueryService::new(fixture.history.clone(), Redactor::default())
            .deployment(
                &fixture.details.record.project,
                &fixture.details.record.environment,
                &fixture.details.record.deployment,
            )
            .unwrap(),
        *fixture.details,
        "rollback must preserve the original Deployment evidence"
    );
    let text = rendered(screen, &fixture.app);
    assert!(text.contains("Rollback result"));
    assert!(text.contains("Succeeded"));
    assert!(text.contains("frontend"));
    assert!(!text.contains("backend:"));
}

fn assert_requests(fixture: &Fixture) {
    let requests = fixture.gateway.requests.lock().unwrap();
    for request in requests.iter() {
        match request {
            ManagementRequest::Candidates { selected, source } => {
                assert_eq!(selected, &BTreeSet::from([name("frontend")]));
                assert_eq!(source, &fixture.details.record.deployment);
            }
            ManagementRequest::Plan { targets, source } => {
                assert_eq!(
                    targets,
                    &BTreeMap::from([(name("frontend"), Some(fixture.target.clone()))])
                );
                assert_eq!(source, &fixture.details.record.deployment);
            }
            ManagementRequest::Execute(plan) => {
                assert_eq!(plan.entries()[0].target.as_ref(), Some(&fixture.target));
            }
            _ => panic!("unexpected request"),
        }
    }
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(request, ManagementRequest::Execute(_)))
            .count(),
        1
    );
}
