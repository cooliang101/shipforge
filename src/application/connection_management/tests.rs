use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use super::*;
use crate::{
    application::{
        DestinationSetupError, DestinationSetupGateway, LocalIdentityCandidate, SetupRootState,
    },
    config::{
        ArtifactSpec, BuildCommand, ComponentSetup, EnvironmentSetup, ProjectSetup, SshCredential,
        TargetSetup,
    },
    domain::{ComponentName, DestinationRevision},
};

#[derive(Debug, Default)]
struct Gateway {
    fail_auth: AtomicBool,
    captures: AtomicUsize,
    auths: AtomicUsize,
    pins: Mutex<Vec<String>>,
    change_during_auth: Mutex<Option<(PathBuf, Vec<u8>)>>,
}

#[async_trait]
impl DestinationSetupGateway for Gateway {
    fn driver_kind(&self) -> DriverKind {
        DriverKind::linux_ssh()
    }
    async fn discover_local_identities(
        &self,
        _: &CancellationToken,
    ) -> Result<Vec<LocalIdentityCandidate>, DestinationSetupError> {
        Ok(Vec::new())
    }
    async fn capture_endpoint_identity(
        &self,
        _: &EndpointProbeRequest,
        _: Duration,
        _: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        self.captures.fetch_add(1, Ordering::Relaxed);
        Ok("SHA256:captured-key".into())
    }
    async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        _: Duration,
        _: Duration,
        _: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        self.auths.fetch_add(1, Ordering::Relaxed);
        assert!(request.credential.downcast_ref::<SshCredential>().is_some());
        assert_eq!(request.remote_root, "/");
        self.pins.lock().unwrap().push(
            request.destination.value["hostKey"]
                .as_str()
                .unwrap()
                .into(),
        );
        if let Some((path, bytes)) = self.change_during_auth.lock().unwrap().take() {
            std::fs::write(path, bytes).unwrap();
        }
        if self.fail_auth.load(Ordering::Relaxed) {
            return Err(DestinationSetupError::operation(
                "authentication",
                "private-secret-path TOKEN",
            ));
        }
        Ok(RemoteSetupCandidates {
            root: SetupRootState::ReadOnlyDirectory,
            services: Vec::new(),
            notices: Vec::new(),
        })
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    service: ConnectionManagementService,
    gateway: Arc<Gateway>,
    key: DestinationKey,
    credential: CredentialHandle,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = ManagementPaths {
            projects: directory.path().join("projects.yaml"),
            destinations: directory.path().join("destinations.yaml"),
            credentials: directory.path().join("credentials.yaml"),
            history: directory.path().join("history.sqlite3"),
        };
        let gateway = Arc::new(Gateway::default());
        let service = ConnectionManagementService::new(
            paths,
            Arc::new(DeploymentSession::default()),
            DestinationSetupService::new(gateway.clone()),
        );
        let mut credentials = CredentialRegistry::new();
        let credential = credentials
            .create(SshCredential::Agent {
                fingerprint: "SHA256:identity".into(),
            })
            .unwrap();
        credentials.save(&service.paths.credentials).unwrap();
        let key = DestinationKey::new();
        let mut destinations = DestinationRegistry::new();
        destinations
            .create(
                key.clone(),
                DestinationSettings::LinuxSsh {
                    host: "old.example".into(),
                    port: 22,
                    user: "operator".into(),
                    credential: credential.clone(),
                    host_key: HostKeyFingerprint::parse("SHA256:old-key").unwrap(),
                },
            )
            .unwrap();
        destinations.save(&service.paths.destinations).unwrap();
        Self {
            directory,
            service,
            gateway,
            key,
            credential,
        }
    }

    fn draft(&self) -> SshConnectionDraft {
        SshConnectionDraft {
            host: "new.example".into(),
            port: 2222,
            user: "deployer".into(),
            credential: ConnectionCredentialDraft::Saved(self.credential.clone()),
        }
    }

    fn project(&self, destination: DestinationKey) -> PathBuf {
        let root = self.directory.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let component = ComponentName::parse("api").unwrap();
        config::initialize(
            &root,
            ProjectSetup {
                project: "demo".into(),
                components: BTreeMap::from([(
                    component.clone(),
                    ComponentSetup {
                        working_directory: None,
                        build: vec![BuildCommand::argv("echo", ["build"])],
                        artifact: ArtifactSpec {
                            path: "dist".into(),
                        },
                    },
                )]),
                environments: BTreeMap::from([(
                    "production".into(),
                    EnvironmentSetup {
                        components: BTreeMap::from([(
                            component,
                            TargetSetup {
                                destination,
                                root: None,
                                service: None,
                                health: None,
                                after: Vec::new(),
                            },
                        )]),
                    },
                )]),
            },
        )
        .unwrap();
        crate::projects::register_initialized_project(&self.service.paths.projects, &root, 1)
            .unwrap();
        root.canonicalize().unwrap()
    }
}

#[tokio::test]
async fn project_removal_changes_only_recents_and_allows_unavailable_directories() {
    let fixture = Fixture::new();
    let root = fixture.project(fixture.key.clone());
    let yaml = std::fs::read(root.join(config::PROJECT_FILE)).unwrap();
    std::fs::write(&fixture.service.paths.history, b"historical sentinel").unwrap();
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    assert_eq!(preview.root(), root);
    fixture.service.remove_project(preview).await.unwrap();
    assert!(
        ProjectRegistry::load(&fixture.service.paths.projects)
            .unwrap()
            .statuses()
            .is_empty()
    );
    assert_eq!(
        std::fs::read(root.join(config::PROJECT_FILE)).unwrap(),
        yaml
    );
    assert_eq!(
        std::fs::read(&fixture.service.paths.history).unwrap(),
        b"historical sentinel"
    );
    crate::projects::register_initialized_project(&fixture.service.paths.projects, &root, 2)
        .unwrap();
    std::fs::rename(&root, root.with_file_name("moved-project")).unwrap();
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    fixture.service.remove_project(preview).await.unwrap();
}

#[tokio::test]
async fn project_removal_rejects_stale_registry_and_busy_session() {
    let fixture = Fixture::new();
    let root = fixture.project(fixture.key.clone());
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    crate::projects::register_initialized_project(&fixture.service.paths.projects, &root, 2)
        .unwrap();
    assert_eq!(
        fixture.service.remove_project(preview).await.unwrap_err(),
        ConnectionManagementError::Stale
    );
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    let result = fixture
        .service
        .session
        .run(fixture.service.remove_project(preview))
        .await
        .unwrap();
    assert_eq!(result.unwrap_err(), ConnectionManagementError::Busy);
    assert_eq!(
        ProjectRegistry::load(&fixture.service.paths.projects)
            .unwrap()
            .statuses()
            .len(),
        1
    );
}

#[tokio::test]
async fn cancelled_project_removal_stops_before_atomic_publish() {
    let fixture = Fixture::new();
    let root = fixture.project(fixture.key.clone());
    let before = std::fs::read(&fixture.service.paths.projects).unwrap();
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert_eq!(
        fixture
            .service
            .remove_project_with_cancellation(preview, &cancellation)
            .await
            .unwrap_err(),
        ConnectionManagementError::Cancelled
    );
    assert_eq!(
        std::fs::read(&fixture.service.paths.projects).unwrap(),
        before
    );
}

#[tokio::test]
async fn unused_destination_removal_never_creates_missing_history_or_removes_credentials() {
    let fixture = Fixture::new();
    let credentials = std::fs::read(&fixture.service.paths.credentials).unwrap();
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    assert!(preview.can_remove());
    assert!(preview.history_missing());
    fixture.service.remove_destination(preview).await.unwrap();
    assert!(fixture.service.list_connections().unwrap().is_empty());
    assert!(!fixture.service.paths.history.exists());
    assert_eq!(
        std::fs::read(&fixture.service.paths.credentials).unwrap(),
        credentials
    );
}

#[tokio::test]
async fn cancelled_destination_removal_stops_before_atomic_publish() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.service.paths.destinations).unwrap();
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert_eq!(
        fixture
            .service
            .remove_destination_with_cancellation(preview, &cancellation)
            .await
            .unwrap_err(),
        ConnectionManagementError::Cancelled
    );
    assert_eq!(
        std::fs::read(&fixture.service.paths.destinations).unwrap(),
        before
    );
}

#[tokio::test]
async fn cancellation_after_successful_removal_does_not_reclassify_or_restore_the_publish() {
    let fixture = Fixture::new();
    let root = fixture.project(fixture.key.clone());
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    let cancellation = CancellationToken::new();
    fixture
        .service
        .remove_project_with_cancellation(preview, &cancellation)
        .await
        .unwrap();
    cancellation.cancel();
    assert!(
        ProjectRegistry::load(&fixture.service.paths.projects)
            .unwrap()
            .statuses()
            .is_empty()
    );

    let fixture = Fixture::new();
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    let cancellation = CancellationToken::new();
    fixture
        .service
        .remove_destination_with_cancellation(preview, &cancellation)
        .await
        .unwrap();
    cancellation.cancel();
    assert!(fixture.service.list_connections().unwrap().is_empty());
}

#[tokio::test]
async fn registered_projects_and_unknown_yaml_block_connection_removal_with_sources() {
    let fixture = Fixture::new();
    let root = fixture.project(fixture.key.clone());
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    assert_eq!(preview.project_references(), std::slice::from_ref(&root));
    assert!(!preview.can_remove());
    assert_eq!(
        fixture
            .service
            .remove_destination(preview)
            .await
            .unwrap_err(),
        ConnectionManagementError::Referenced
    );
    for contents in [Some(b"invalid: [".as_slice()), None] {
        let path = root.join(config::PROJECT_FILE);
        if let Some(bytes) = contents {
            std::fs::write(&path, bytes).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        let preview = fixture
            .service
            .preview_destination_removal(&fixture.key)
            .unwrap();
        assert!(!preview.can_remove());
        assert_eq!(
            preview.blockers()[0].source,
            ManagementSource::ProjectConfig(path)
        );
        assert!(!preview.blockers()[0].reason.contains("invalid: ["));
    }
}

#[tokio::test]
async fn reference_changes_after_preview_are_not_overwritten_or_ignored() {
    let fixture = Fixture::new();
    let root = fixture.project(DestinationKey::new());
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    assert!(preview.can_remove());
    let mut bytes = std::fs::read(root.join(config::PROJECT_FILE)).unwrap();
    bytes.extend_from_slice(b"\n# changed since preview\n");
    std::fs::write(root.join(config::PROJECT_FILE), bytes).unwrap();
    assert_eq!(
        fixture
            .service
            .remove_destination(preview)
            .await
            .unwrap_err(),
        ConnectionManagementError::Stale
    );
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    HistoryStore::open(&fixture.service.paths.history).unwrap();
    assert_eq!(
        fixture
            .service
            .remove_destination(preview)
            .await
            .unwrap_err(),
        ConnectionManagementError::Stale
    );
    assert_eq!(fixture.service.list_connections().unwrap().len(), 1);
}

#[test]
fn corrupt_uninitialized_and_sidecar_only_history_are_unknown_not_absent() {
    let fixture = Fixture::new();
    for bytes in [b"broken sqlite".as_slice(), b"".as_slice()] {
        std::fs::write(&fixture.service.paths.history, bytes).unwrap();
        let preview = fixture
            .service
            .preview_destination_removal(&fixture.key)
            .unwrap();
        assert!(!preview.can_remove());
        assert_eq!(preview.blockers()[0].source, ManagementSource::History);
        assert_eq!(
            std::fs::read(&fixture.service.paths.history).unwrap(),
            bytes
        );
    }
    std::fs::remove_file(&fixture.service.paths.history).unwrap();
    let sidecar = fixture
        .service
        .paths
        .history
        .with_file_name("history.sqlite3-wal");
    std::fs::write(sidecar, b"orphan evidence").unwrap();
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    assert!(!preview.can_remove());
    assert!(!fixture.service.paths.history.exists());
}

#[tokio::test]
async fn edit_requires_explicit_pin_authentication_and_preserves_old_revision() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.service.paths.destinations).unwrap();
    let old = fixture.service.list_connections().unwrap()[0].clone();
    let preview = fixture
        .service
        .preview_edit(&fixture.key, fixture.draft())
        .unwrap();
    let confirmation = fixture
        .service
        .capture_identity(preview, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(confirmation.fingerprint(), "SHA256:captured-key");
    assert_eq!(
        std::fs::read(&fixture.service.paths.destinations).unwrap(),
        before
    );
    assert_eq!(fixture.gateway.auths.load(Ordering::Relaxed), 0);
    let updated = fixture
        .service
        .confirm_and_save(confirmation, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(updated.key, fixture.key);
    assert_eq!(
        updated.current.revision,
        DestinationRevision::INITIAL.checked_next().unwrap()
    );
    assert_eq!(
        fixture.gateway.pins.lock().unwrap().as_slice(),
        &["SHA256:captured-key"]
    );
    let registry = DestinationRegistry::load(&fixture.service.paths.destinations).unwrap();
    assert_eq!(
        registry.resolve_revision(&fixture.key, DestinationRevision::INITIAL),
        Some(&old.current)
    );
}

#[tokio::test]
async fn standalone_create_with_new_identity_creates_no_project_or_history() {
    let fixture = Fixture::new();
    let mut draft = fixture.draft();
    draft.credential = ConnectionCredentialDraft::New(SshCredential::IdentityFile {
        path: fixture.directory.path().join("identity"),
    });
    let preview = fixture.service.preview_create(draft).unwrap();
    let key = preview.key().clone();
    assert!(preview.details().is_none());
    let confirmation = fixture
        .service
        .capture_identity(preview, &CancellationToken::new())
        .await
        .unwrap();
    let added = fixture
        .service
        .confirm_and_save(confirmation, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(added.key, key);
    assert_eq!(added.current.revision, DestinationRevision::INITIAL);
    assert_eq!(
        CredentialRegistry::load(&fixture.service.paths.credentials)
            .unwrap()
            .summaries()
            .len(),
        2
    );
    assert!(!fixture.service.paths.projects.exists());
    assert!(!fixture.service.paths.history.exists());
    assert_eq!(fixture.service.list_connections().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_authentication_is_redacted_and_saves_neither_registry() {
    let fixture = Fixture::new();
    fixture.gateway.fail_auth.store(true, Ordering::Relaxed);
    let before = std::fs::read(&fixture.service.paths.destinations).unwrap();
    let credentials = std::fs::read(&fixture.service.paths.credentials).unwrap();
    let mut draft = fixture.draft();
    draft.credential = ConnectionCredentialDraft::New(SshCredential::Agent {
        fingerprint: "new-identity".into(),
    });
    let preview = fixture.service.preview_create(draft).unwrap();
    let confirmation = fixture
        .service
        .capture_identity(preview, &CancellationToken::new())
        .await
        .unwrap();
    let error = fixture
        .service
        .confirm_and_save(confirmation, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ConnectionManagementError::Setup("pinned authentication")
    );
    assert!(!format!("{error:?}").contains("private-secret"));
    assert_eq!(
        std::fs::read(&fixture.service.paths.destinations).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(&fixture.service.paths.credentials).unwrap(),
        credentials
    );
}

#[tokio::test]
async fn local_change_during_authentication_and_cancellation_prevent_save() {
    let fixture = Fixture::new();
    let changed = b"# external edit\n".to_vec();
    *fixture.gateway.change_during_auth.lock().unwrap() =
        Some((fixture.service.paths.destinations.clone(), changed.clone()));
    let preview = fixture
        .service
        .preview_edit(&fixture.key, fixture.draft())
        .unwrap();
    let confirmation = fixture
        .service
        .capture_identity(preview, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .service
            .confirm_and_save(confirmation, &CancellationToken::new())
            .await
            .unwrap_err(),
        ConnectionManagementError::Stale
    );
    assert_eq!(
        std::fs::read(&fixture.service.paths.destinations).unwrap(),
        changed
    );
    let fixture = Fixture::new();
    let preview = fixture
        .service
        .preview_edit(&fixture.key, fixture.draft())
        .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        fixture
            .service
            .capture_identity(preview, &cancellation)
            .await
            .unwrap_err(),
        ConnectionManagementError::Cancelled
    );
    assert_eq!(fixture.gateway.captures.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn saved_connection_verification_reuses_pin_and_is_read_only() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.service.paths.destinations).unwrap();
    fixture
        .service
        .verify_saved_connection(&fixture.key, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fixture.gateway.captures.load(Ordering::Relaxed), 0);
    assert_eq!(
        fixture.gateway.pins.lock().unwrap().as_slice(),
        &["SHA256:old-key"]
    );
    assert_eq!(
        std::fs::read(&fixture.service.paths.destinations).unwrap(),
        before
    );
}

#[tokio::test]
async fn busy_session_rejects_creation_before_authentication_or_writes() {
    let fixture = Fixture::new();
    let preview = fixture.service.preview_create(fixture.draft()).unwrap();
    let confirmation = fixture
        .service
        .capture_identity(preview, &CancellationToken::new())
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let result = fixture
        .service
        .session
        .run(
            fixture
                .service
                .confirm_and_save(confirmation, &cancellation),
        )
        .await
        .unwrap();
    assert_eq!(result.unwrap_err(), ConnectionManagementError::Busy);
    assert_eq!(fixture.gateway.auths.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.service.list_connections().unwrap().len(), 1);
}

#[test]
fn malformed_and_excessive_registry_inputs_are_not_normalized_into_empty_registries() {
    let fixture = Fixture::new();
    for bytes in [
        b"schemaVersion: 99\nprojects: []".to_vec(),
        vec![b'x'; 1024 * 1024 + 1],
    ] {
        std::fs::write(&fixture.service.paths.projects, bytes).unwrap();
        let preview = fixture
            .service
            .preview_destination_removal(&fixture.key)
            .unwrap();
        assert!(!preview.can_remove());
        assert_eq!(preview.blockers()[0].source, ManagementSource::Projects);
    }
}

#[tokio::test]
async fn historical_reference_blocks_removal_after_project_unregistration() {
    use crate::{
        domain::{
            ComponentGeneration, DeploymentId, DriverCapabilities, EnvironmentId, ProjectId,
            ReleaseVersion,
        },
        drivers::{EndpointFingerprint, ReleaseRef},
        history::DeploymentComponentSnapshot,
    };
    let fixture = Fixture::new();
    let root = fixture.project(fixture.key.clone());
    let preview = fixture.service.preview_project_removal(&root).unwrap();
    fixture.service.remove_project(preview).await.unwrap();
    let store = HistoryStore::open(&fixture.service.paths.history).unwrap();
    let id = DeploymentId::new();
    let release = ReleaseRef {
        driver: DriverKind::linux_ssh(),
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("api").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: ReleaseVersion::parse("v1").unwrap(),
        destination: fixture.key.clone(),
        destination_revision: DestinationRevision::INITIAL,
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        effective_capabilities: DriverCapabilities::default(),
    };
    store
        .create_deployment(&id, &release.project_id, &release.environment_id, 1)
        .unwrap();
    store
        .record_component_snapshots(
            &id,
            &[DeploymentComponentSnapshot {
                target_snapshot: None,
                release: release.clone(),
                target: Some(release),
                expected_current: None,
                execution_order: 0,
            }],
        )
        .unwrap();
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    assert!(preview.project_references().is_empty());
    assert_eq!(preview.history_references().unwrap().deployments, 1);
    assert_eq!(
        fixture
            .service
            .remove_destination(preview)
            .await
            .unwrap_err(),
        ConnectionManagementError::Referenced
    );
}

#[cfg(unix)]
#[test]
fn linked_project_config_is_unknown_without_reading_the_link_target() {
    let fixture = Fixture::new();
    let root = fixture.project(DestinationKey::new());
    let yaml = root.join(config::PROJECT_FILE);
    let target = root.join("protected.yaml");
    std::fs::rename(&yaml, &target).unwrap();
    std::os::unix::fs::symlink(&target, &yaml).unwrap();
    let preview = fixture
        .service
        .preview_destination_removal(&fixture.key)
        .unwrap();
    assert!(!preview.can_remove());
    assert_eq!(
        preview.blockers()[0].source,
        ManagementSource::ProjectConfig(yaml)
    );
    assert!(target.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn new_credential_survives_failed_connection_commit_without_restoring_old_registries() {
    let fixture = Fixture::new();
    let mut draft = fixture.draft();
    draft.credential = ConnectionCredentialDraft::New(SshCredential::Agent {
        fingerprint: "another-identity".into(),
    });
    let preview = fixture.service.preview_create(draft).unwrap();
    let confirmation = fixture
        .service
        .capture_identity(preview, &CancellationToken::new())
        .await
        .unwrap();
    let path = &fixture.service.paths.destinations;
    let before = std::fs::read(path).unwrap();
    let original_permissions = std::fs::metadata(path).unwrap().permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).unwrap();
    let result = fixture
        .service
        .confirm_and_save(confirmation, &CancellationToken::new())
        .await;
    std::fs::set_permissions(path, original_permissions).unwrap();
    assert!(matches!(
        result,
        Err(ConnectionManagementError::CredentialSavedConnectionUnconfirmed { .. })
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(
        CredentialRegistry::load(&fixture.service.paths.credentials)
            .unwrap()
            .summaries()
            .len(),
        2
    );
}
