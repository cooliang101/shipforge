//! Exercises the same application services used by the management screens.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use shipforge::{
    application::{
        ConnectionCredentialDraft, ConnectionManagementService, DeploymentSelection,
        ManagementPaths, RecoveryService, RollbackService, SshConnectionDraft,
        history_query::{HistoricalLogQuery, HistoricalLogStatus, HistoryQueryService},
    },
    config::{DestinationRegistry, DestinationSettings, ProjectSetup, SshCredential},
    domain::{DeploymentState, DestinationKey, DestinationRevision},
    history::{CurrentAlignment, DeploymentKind, DeploymentQuery},
    telemetry::Redactor,
};
use tokio_util::sync::CancellationToken;

use super::{Fixture, component, required_env};

#[tokio::test]
#[ignore = "requires two explicitly provisioned disposable Linux OpenSSH endpoints"]
async fn real_linux_management_history_rollback_and_connections() {
    tokio::time::timeout(Duration::from_secs(600), Box::pin(run()))
        .await
        .expect("management acceptance exceeded its ten-minute deadline");
}

async fn run() {
    // Fixture attests both isolated endpoint markers and independent Host Keys
    // before any writes. No user's connection registry or Project is consulted.
    let fixture = Fixture::with_setup(management_setup).await;
    verify_standalone_connection_management(&fixture).await;
    let first = fixture.deploy(&["frontend"], false).await;
    assert_eq!(first.deployment.state, DeploymentState::Succeeded);
    let original = fixture.observed().await;
    assert!(original[&component("backend")].is_none());
    let target = original[&component("frontend")].clone().unwrap();
    fixture.payload("frontend", "healthy");
    let second = fixture.deploy(&["frontend", "backend"], false).await;
    assert_eq!(second.deployment.state, DeploymentState::Succeeded);
    let mut expected_after_rollback = fixture.observed().await;
    assert!(expected_after_rollback[&component("backend")].is_some());
    expected_after_rollback.insert(component("frontend"), Some(target.clone()));

    let history = HistoryQueryService::new(fixture.history.clone(), Redactor::default());
    let project = &fixture.config.project_id;
    let environment = &fixture.config.environments["acceptance"].id;
    let before = history
        .deployment(project, environment, &second.deployment.id)
        .unwrap();
    assert_eq!(before.snapshots.len(), 2);
    assert_eq!(before.packages.len(), 2);
    assert!(!before.steps.is_empty());
    let log = history
        .log_page(
            project,
            environment,
            &second.deployment.id,
            HistoricalLogQuery::default(),
        )
        .unwrap();
    assert_eq!(log.status, HistoricalLogStatus::Ready);
    assert!(!log.text.is_empty());
    assert!(log.text.len() <= 16 * 1024);

    let (selection, result) =
        execute_subset_rollback(&fixture, &second.deployment.id, &target).await;
    let cancellation = CancellationToken::new();
    assert_eq!(
        fixture.observed().await,
        expected_after_rollback,
        "unselected backend must keep the joint deployment's current version"
    );
    let details = history
        .deployment(project, environment, &result.deployment.id)
        .unwrap();
    assert_eq!(details.record.kind, DeploymentKind::Rollback);
    assert_eq!(
        details.record.related_deployment.as_ref(),
        Some(&second.deployment.id)
    );
    assert!(details.observations.iter().any(|observation| {
        observation
            .observed
            .as_ref()
            .is_ok_and(|observed| observed.as_ref() == Some(&target))
    }));
    let recovery = RecoveryService::new(
        Arc::clone(&fixture.drivers),
        fixture.history.clone(),
        Arc::clone(&fixture.session),
    );
    let report = recovery
        .inspect(
            selection,
            Some(result.deployment.id),
            &fixture.destinations_path,
            &cancellation,
        )
        .await
        .unwrap();
    assert!(report.persistence_warning.is_none());
    assert_eq!(report.report.components.len(), 1);
    assert_eq!(
        report.report.components[0].alignment,
        CurrentAlignment::Target
    );
    assert_eq!(
        history
            .deployment(project, environment, &second.deployment.id)
            .unwrap(),
        before,
        "management must not rewrite original outcomes"
    );
    assert_eq!(
        history
            .deployments(project, environment, DeploymentQuery::default())
            .unwrap()
            .items
            .len(),
        3
    );
    assert_eq!(
        history
            .recovery_report(project, environment, &report.report.id)
            .unwrap(),
        report.report
    );
}

async fn execute_subset_rollback(
    fixture: &Fixture,
    source: &shipforge::domain::DeploymentId,
    target: &shipforge::drivers::ReleaseRef,
) -> (DeploymentSelection, shipforge::application::RollbackReport) {
    let selection = DeploymentSelection {
        project_root: fixture.project.clone(),
        config: fixture.config.clone(),
        environment: "acceptance".into(),
        components: [component("frontend")].into(),
    };
    let cancellation = CancellationToken::new();
    let rollback = RollbackService::new(
        Arc::clone(&fixture.drivers),
        fixture.history.clone(),
        Arc::clone(&fixture.session),
    );
    let choices = rollback
        .candidates(
            selection.clone(),
            source.clone(),
            &fixture.destinations_path,
            &cancellation,
        )
        .await
        .unwrap();
    assert_eq!(choices.components.len(), 1);
    assert!(choices.components[0].unavailable.is_none());
    assert!(
        choices.components[0]
            .options
            .iter()
            .any(|option| option.target.as_ref() == Some(target) && option.unavailable.is_none())
    );
    let plan = rollback
        .plan(
            selection.clone(),
            source.clone(),
            BTreeMap::from([(component("frontend"), Some(target.clone()))]),
            &fixture.destinations_path,
            &cancellation,
        )
        .await
        .unwrap();
    assert_eq!(plan.entries().len(), 1);
    assert_eq!(plan.entries()[0].target.as_ref(), Some(target));
    assert_ne!(plan.entries()[0].current.version, target.version);
    let result = rollback
        .execute(plan, &fixture.destinations_path, &cancellation)
        .await
        .unwrap();
    assert_eq!(
        result.deployment.state,
        DeploymentState::Succeeded,
        "{result:#?}"
    );
    assert!(result.failure.is_none() && result.warnings.is_empty());
    (selection, result)
}

fn management_setup(destinations: &[DestinationKey]) -> ProjectSetup {
    let mut setup = super::project_setup(destinations);
    let namespace = format!("management-{}", uuid::Uuid::now_v7());
    for (name, target) in &mut setup.environments.get_mut("acceptance").unwrap().components {
        target.root = Some(format!("/srv/shipforge-acceptance/{namespace}/{name}"));
        target.health = Some(format!("http://127.0.0.1:8080/{namespace}/{name}"));
    }
    setup
}

async fn verify_connection_revision(
    service: &ConnectionManagementService,
    paths: &ManagementPaths,
    first: &shipforge::application::ConnectionDetails,
    draft: SshConnectionDraft,
    cancellation: &CancellationToken,
) {
    let mut changed_identity = draft;
    changed_identity.port = required_env("SHIPFORGE_TEST_PORT_B").parse().unwrap();
    changed_identity.credential =
        ConnectionCredentialDraft::Saved(first.current.resolve().credential);
    let edit = service.preview_edit(&first.key, changed_identity).unwrap();
    let confirmation = service.capture_identity(edit, cancellation).await.unwrap();
    assert_eq!(
        confirmation.fingerprint(),
        required_env("SHIPFORGE_TEST_HOST_KEY_B")
    );
    let revised = service
        .confirm_and_save(confirmation, cancellation)
        .await
        .unwrap();
    assert_eq!(revised.current.revision.get(), 2);
    assert_eq!(revised.key, first.key);
    let registry = DestinationRegistry::load(&paths.destinations).unwrap();
    assert_eq!(
        registry
            .resolve_revision(&first.key, DestinationRevision::INITIAL)
            .unwrap(),
        &first.current
    );
    let DestinationSettings::LinuxSsh { port, host_key, .. } = &revised.current.settings;
    assert_eq!(
        *port,
        required_env("SHIPFORGE_TEST_PORT_B")
            .parse::<u16>()
            .unwrap()
    );
    assert_eq!(host_key.as_str(), required_env("SHIPFORGE_TEST_HOST_KEY_B"));
    service
        .verify_saved_connection(&first.key, cancellation)
        .await
        .unwrap();
}

async fn verify_standalone_connection_management(fixture: &Fixture) {
    let paths = ManagementPaths {
        projects: fixture.scratch.path().join("projects.yaml"),
        destinations: fixture.destinations_path.clone(),
        credentials: fixture.scratch.path().join("credentials.yaml"),
        history: fixture.history.clone(),
    };
    let service = ConnectionManagementService::new(
        paths.clone(),
        Arc::clone(&fixture.session),
        shipforge::bootstrap::destination_setup_service(),
    );
    let cancellation = CancellationToken::new();
    let draft = SshConnectionDraft {
        host: "127.0.0.1".into(),
        port: required_env("SHIPFORGE_TEST_PORT_A").parse().unwrap(),
        user: "deploy".into(),
        credential: ConnectionCredentialDraft::New(SshCredential::IdentityFile {
            path: PathBuf::from(required_env("SHIPFORGE_TEST_KEY")),
        }),
    };
    let preview = service.preview_create(draft.clone()).unwrap();
    let key = preview.key().clone();
    let confirmation = service
        .capture_identity(preview, &cancellation)
        .await
        .unwrap();
    // This exact pin came from the runner's attested disposable server, not TOFU.
    assert_eq!(
        confirmation.fingerprint(),
        required_env("SHIPFORGE_TEST_HOST_KEY_A")
    );
    assert!(
        !paths.credentials.exists(),
        "preview and capture must not save credentials"
    );
    let first = service
        .confirm_and_save(confirmation, &cancellation)
        .await
        .unwrap();
    assert_eq!(first.key, key);
    assert_eq!(first.current.revision.get(), 1);
    service
        .verify_saved_connection(&key, &cancellation)
        .await
        .unwrap();
    verify_connection_revision(&service, &paths, &first, draft, &cancellation).await;
    let removal = service.preview_destination_removal(&key).unwrap();
    assert!(removal.can_remove());
    service.remove_destination(removal).await.unwrap();
    assert!(
        !service
            .list_connections()
            .unwrap()
            .iter()
            .any(|item| item.key == key)
    );
    assert!(
        paths.credentials.is_file(),
        "connection removal keeps credential references"
    );
    assert!(
        !paths.history.exists(),
        "connection management must not create history"
    );
}
