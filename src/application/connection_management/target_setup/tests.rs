use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    application::{
        DeploymentSession, DestinationSetupGateway, DestinationSetupService, EndpointProbeRequest,
        LocalIdentityCandidate, SetupRootState,
    },
    config::{
        CredentialRegistry, DestinationRegistry, DestinationSettings, HostKeyFingerprint,
        SshCredential,
    },
    domain::DestinationKey,
    drivers::DriverKind,
};

use super::super::ManagementPaths;
use super::*;

#[derive(Debug, Default)]
struct Gateway {
    calls: AtomicUsize,
    requests: Mutex<Vec<DestinationSetupRequest>>,
    change: Mutex<Option<(PathBuf, Vec<u8>)>>,
    cancel: Mutex<Option<CancellationToken>>,
    fail: AtomicBool,
    wrong_directory: AtomicBool,
}

impl Gateway {
    fn record(
        &self,
        request: &DestinationSetupRequest,
        connect: Duration,
        command: Duration,
    ) -> Result<(), DestinationSetupError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(connect, SETUP_TIMEOUT);
        assert_eq!(command, SETUP_TIMEOUT);
        self.requests.lock().unwrap().push(request.clone());
        if let Some((path, bytes)) = self.change.lock().unwrap().take() {
            fs::write(path, bytes).unwrap();
        }
        if let Some(cancellation) = self.cancel.lock().unwrap().take() {
            cancellation.cancel();
        }
        if self.fail.load(Ordering::Relaxed) {
            return Err(DestinationSetupError::operation(
                "untrusted-stage",
                "private-parser-or-protocol-sentinel",
            ));
        }
        Ok(())
    }
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
        panic!("saved target operations must not discover identities");
    }
    async fn capture_endpoint_identity(
        &self,
        _: &EndpointProbeRequest,
        _: Duration,
        _: &CancellationToken,
    ) -> Result<String, DestinationSetupError> {
        panic!("saved target operations must not capture or replace the host key");
    }
    async fn authenticate_and_probe(
        &self,
        request: &DestinationSetupRequest,
        connect: Duration,
        command: Duration,
        _: &CancellationToken,
    ) -> Result<RemoteSetupCandidates, DestinationSetupError> {
        self.record(request, connect, command)?;
        Ok(RemoteSetupCandidates {
            root: SetupRootState::ReadOnlyDirectory,
            services: vec!["worker.service".into()],
            notices: Vec::new(),
        })
    }
    async fn browse_directories(
        &self,
        request: &DestinationSetupRequest,
        connect: Duration,
        command: Duration,
        _: &CancellationToken,
    ) -> Result<RemoteDirectoryCandidates, DestinationSetupError> {
        self.record(request, connect, command)?;
        Ok(RemoteDirectoryCandidates {
            directory: if self.wrong_directory.load(Ordering::Relaxed) {
                "/different".into()
            } else {
                request.remote_root.clone()
            },
            directories: vec![format!(
                "{}/child",
                request.remote_root.trim_end_matches('/')
            )],
        })
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    service: ConnectionManagementService,
    selected: ConnectionDetails,
    gateway: Arc<Gateway>,
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
        let mut credentials = CredentialRegistry::new();
        let credential = credentials
            .create(SshCredential::Agent {
                fingerprint: "SHA256:saved-agent".into(),
            })
            .unwrap();
        credentials.save(&paths.credentials).unwrap();
        let mut destinations = DestinationRegistry::new();
        destinations
            .create(
                DestinationKey::new(),
                DestinationSettings::LinuxSsh {
                    host: "saved.example.invalid".into(),
                    port: 2222,
                    user: "deployer".into(),
                    credential,
                    host_key: HostKeyFingerprint::parse("SHA256:strict-saved-pin").unwrap(),
                },
            )
            .unwrap();
        destinations.save(&paths.destinations).unwrap();
        let gateway = Arc::new(Gateway::default());
        let service = ConnectionManagementService::new(
            paths,
            Arc::new(DeploymentSession::default()),
            DestinationSetupService::new(gateway.clone()),
        );
        let selected = service.list_connections().unwrap().remove(0);
        Self {
            directory,
            service,
            selected,
            gateway,
        }
    }
    async fn query(
        &self,
        browse: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), ConnectionManagementError> {
        if browse {
            self.service
                .browse_saved_directories(&self.selected, "/srv/chosen ' directory", cancellation)
                .await
                .map(|_| ())
        } else {
            self.service
                .inspect_saved_target(&self.selected, "/srv/chosen ' directory", cancellation)
                .await
                .map(|_| ())
        }
    }
}

#[tokio::test]
async fn saved_target_reads_use_fresh_exact_pin_credential_and_requested_root_without_writes() {
    let fixture = Fixture::new();
    let destination_bytes = fs::read(&fixture.service.paths.destinations).unwrap();
    let credential_bytes = fs::read(&fixture.service.paths.credentials).unwrap();
    for browse in [false, true] {
        fixture
            .query(browse, &CancellationToken::new())
            .await
            .unwrap();
    }
    assert_eq!(fixture.gateway.calls.load(Ordering::Relaxed), 2);
    for request in fixture.gateway.requests.lock().unwrap().iter() {
        assert_eq!(request.driver, DriverKind::linux_ssh());
        assert_eq!(request.remote_root, "/srv/chosen ' directory");
        assert_eq!(request.destination.value["host"], "saved.example.invalid");
        assert_eq!(request.destination.value["port"], 2222);
        assert_eq!(request.destination.value["user"], "deployer");
        assert_eq!(
            request.destination.value["hostKey"],
            "SHA256:strict-saved-pin"
        );
        assert_eq!(
            request.credential.downcast_ref::<SshCredential>(),
            Some(&SshCredential::Agent {
                fingerprint: "SHA256:saved-agent".into()
            })
        );
    }
    assert_eq!(
        fs::read(&fixture.service.paths.destinations).unwrap(),
        destination_bytes
    );
    assert_eq!(
        fs::read(&fixture.service.paths.credentials).unwrap(),
        credential_bytes
    );
    assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn stale_missing_or_invalid_saved_data_never_contacts_the_gateway() {
    for browse in [false, true] {
        for case in 0..5 {
            let fixture = Fixture::new();
            match case {
                0 => {
                    let mut registry =
                        DestinationRegistry::load(&fixture.service.paths.destinations).unwrap();
                    registry
                        .revise(
                            &fixture.selected.key,
                            fixture.selected.current.settings.clone(),
                        )
                        .unwrap();
                    registry.save(&fixture.service.paths.destinations).unwrap();
                }
                1 => fs::remove_file(&fixture.service.paths.destinations).unwrap(),
                2 => fs::remove_file(&fixture.service.paths.credentials).unwrap(),
                3 => fs::write(
                    &fixture.service.paths.destinations,
                    "invalid: [secret-sentinel",
                )
                .unwrap(),
                _ => fs::write(
                    &fixture.service.paths.credentials,
                    "invalid: [secret-sentinel",
                )
                .unwrap(),
            }
            let error = fixture
                .query(browse, &CancellationToken::new())
                .await
                .unwrap_err();
            if case == 0 {
                assert_eq!(error, ConnectionManagementError::Stale);
            }
            if case == 1 {
                assert_eq!(error, ConnectionManagementError::Missing);
            }
            assert!(!format!("{error:?} {error}").contains("secret-sentinel"));
            assert_eq!(fixture.gateway.calls.load(Ordering::Relaxed), 0);
        }
    }
}

#[tokio::test]
async fn registry_drift_during_read_rejects_observations_without_reverting_external_changes() {
    for browse in [false, true] {
        for credentials in [false, true] {
            let fixture = Fixture::new();
            let path = if credentials {
                &fixture.service.paths.credentials
            } else {
                &fixture.service.paths.destinations
            };
            let mut changed = fs::read(path).unwrap();
            changed.extend_from_slice(b"\n# externally changed during query\n");
            *fixture.gateway.change.lock().unwrap() = Some((path.clone(), changed.clone()));
            assert_eq!(
                fixture.query(browse, &CancellationToken::new()).await,
                Err(ConnectionManagementError::Stale)
            );
            assert_eq!(fs::read(path).unwrap(), changed);
            assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 2);
        }
    }
}

#[tokio::test]
async fn saved_target_cancel_busy_and_unsafe_root_checks_preserve_local_state() {
    let fixture = Fixture::new();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    for browse in [false, true] {
        assert_eq!(
            fixture.query(browse, &cancellation).await,
            Err(ConnectionManagementError::Cancelled)
        );
        fixture
            .service
            .session
            .run(async {
                assert_eq!(
                    fixture.query(browse, &CancellationToken::new()).await,
                    Err(ConnectionManagementError::Busy)
                );
            })
            .await
            .unwrap();
    }
    for root in [
        "",
        "relative",
        "/srv/../other",
        "/srv//other",
        "/srv/",
        "/srv/a\nsecret-sentinel",
    ] {
        assert!(
            fixture
                .service
                .inspect_saved_target(&fixture.selected, root, &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            fixture
                .service
                .browse_saved_directories(&fixture.selected, root, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    assert_eq!(fixture.gateway.calls.load(Ordering::Relaxed), 0);
    assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn gateway_failures_late_cancellation_and_wrong_directory_never_become_valid_candidates() {
    for browse in [false, true] {
        let fixture = Fixture::new();
        fixture.gateway.fail.store(true, Ordering::Relaxed);
        let error = fixture
            .query(browse, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("sentinel"));
        fixture.gateway.fail.store(false, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        *fixture.gateway.cancel.lock().unwrap() = Some(cancellation.clone());
        assert_eq!(
            fixture.query(browse, &cancellation).await,
            Err(ConnectionManagementError::Cancelled)
        );
    }
    let fixture = Fixture::new();
    fixture
        .gateway
        .wrong_directory
        .store(true, Ordering::Relaxed);
    assert_eq!(
        fixture.query(true, &CancellationToken::new()).await,
        Err(ConnectionManagementError::Setup(
            "directory evidence validation"
        ))
    );
}

#[tokio::test]
async fn read_deadline_and_cancellation_signal_gateway_and_allow_bounded_cleanup() {
    for cancel in [false, true] {
        let cancellation = CancellationToken::new();
        let worker = cancellation.child_token();
        if cancel {
            cancellation.cancel();
        }
        let cleaned = Arc::new(AtomicBool::new(false));
        let operation = async {
            worker.cancelled().await;
            cleaned.store(true, Ordering::Relaxed);
            Ok::<_, DestinationSetupError>(())
        };
        let result = wait_read(
            operation,
            &worker,
            &cancellation,
            "test read",
            Duration::from_millis(1),
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err());
        assert!(worker.is_cancelled());
        assert!(cleaned.load(Ordering::Relaxed));
    }
    let cancellation = CancellationToken::new();
    let worker = cancellation.child_token();
    let result = wait_read(
        std::future::pending::<Result<(), DestinationSetupError>>(),
        &worker,
        &cancellation,
        "test read",
        Duration::from_millis(1),
        Duration::from_millis(1),
    )
    .await;
    assert!(result.is_err());
    assert!(worker.is_cancelled());
}
