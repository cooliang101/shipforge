use std::{collections::VecDeque, path::PathBuf};

use crate::{
    config::{DestinationSettings, HostKeyFingerprint, SshCredential},
    domain::DestinationKey,
};

use super::*;

#[derive(Clone, Copy, Default)]
enum Failure {
    #[default]
    None,
    BeforePublish,
    AfterPublish,
}

#[derive(Default)]
struct InjectedStorage {
    credential_failures: VecDeque<Failure>,
    destination_failure: Failure,
    credential_writes: usize,
    destination_writes: usize,
    cannot_read_destinations: bool,
    different_credentials: Option<CredentialRegistry>,
}

impl RegistryStorage for InjectedStorage {
    fn credentials(&self, path: &Path) -> Result<CredentialRegistry, ()> {
        if self.destination_writes > 0
            && let Some(registry) = &self.different_credentials
        {
            return Ok(registry.clone());
        }
        CredentialRegistry::load(path).map_err(|_| ())
    }

    fn destinations(&self, path: &Path) -> Result<DestinationRegistry, ()> {
        if self.cannot_read_destinations {
            return Err(());
        }
        DestinationRegistry::load(path).map_err(|_| ())
    }

    fn save_credentials(&mut self, path: &Path, registry: &CredentialRegistry) -> Result<(), ()> {
        self.credential_writes += 1;
        match self.credential_failures.pop_front().unwrap_or_default() {
            Failure::BeforePublish => Err(()),
            Failure::AfterPublish => {
                registry.save(path).unwrap();
                Err(())
            }
            Failure::None => registry.save(path).map_err(|_| ()),
        }
    }

    fn save_destinations(&mut self, path: &Path, registry: &DestinationRegistry) -> Result<(), ()> {
        self.destination_writes += 1;
        match self.destination_failure {
            Failure::BeforePublish => Err(()),
            Failure::AfterPublish => {
                registry.save(path).unwrap();
                Err(())
            }
            Failure::None => registry.save(path).map_err(|_| ()),
        }
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    credential_path: PathBuf,
    destination_path: PathBuf,
    original_credentials: CredentialRegistry,
    credentials: CredentialRegistry,
    original_destinations: DestinationRegistry,
    destinations: DestinationRegistry,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let credential_path = directory.path().join("credentials.yaml");
        let destination_path = directory.path().join("destinations.yaml");
        let original_credentials = CredentialRegistry::new();
        let original_destinations = DestinationRegistry::new();
        let mut credentials = original_credentials.clone();
        let credential = credentials
            .create(SshCredential::Agent {
                fingerprint: "SHA256:test-identity".into(),
            })
            .unwrap();
        let mut destinations = original_destinations.clone();
        destinations
            .create(
                DestinationKey::new(),
                DestinationSettings::LinuxSsh {
                    host: "test.invalid".into(),
                    port: 22,
                    user: "deploy".into(),
                    credential,
                    host_key: HostKeyFingerprint::parse("SHA256:test-host").unwrap(),
                },
            )
            .unwrap();
        Self {
            directory,
            credential_path,
            destination_path,
            original_credentials,
            credentials,
            original_destinations,
            destinations,
        }
    }

    fn registration(&self) -> Registration<'_> {
        Registration {
            credential_path: &self.credential_path,
            destination_path: &self.destination_path,
            original_credentials: &self.original_credentials,
            credentials: &self.credentials,
            original_destinations: &self.original_destinations,
            destinations: &self.destinations,
            created_credential: true,
        }
    }
}

#[test]
fn credential_error_after_actual_publication_does_not_claim_absence_or_write_connection() {
    let fixture = Fixture::new();
    let mut storage = InjectedStorage {
        credential_failures: VecDeque::from([Failure::AfterPublish]),
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("durable result is unconfirmed"));
    assert!(error.contains("new identity is visible on re-read"));
    assert!(error.contains("Connection registration was not attempted"));
    assert_eq!(
        CredentialRegistry::load(&fixture.credential_path).unwrap(),
        fixture.credentials
    );
    assert_eq!(storage.destination_writes, 0);
    assert!(!fixture.destination_path.exists());
}

#[test]
fn credential_error_before_publication_reports_observation_without_claiming_durability() {
    let fixture = Fixture::new();
    let mut storage = InjectedStorage {
        credential_failures: VecDeque::from([Failure::BeforePublish]),
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("original identity registry is visible on re-read"));
    assert!(error.contains("durable result is unconfirmed"));
    assert_eq!(storage.destination_writes, 0);
    assert_eq!(
        std::fs::read_dir(fixture.directory.path()).unwrap().count(),
        0
    );
}

#[test]
fn connection_error_after_actual_publication_keeps_its_identity_and_requires_reload() {
    let fixture = Fixture::new();
    let mut storage = InjectedStorage {
        destination_failure: Failure::AfterPublish,
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("new connection is visible on re-read"));
    assert!(error.contains("durable result is unconfirmed"));
    assert!(error.contains("Reload Connections"));
    assert!(error.contains("no identity restoration was attempted"));
    assert_eq!(storage.credential_writes, 1);
    assert_eq!(
        DestinationRegistry::load(&fixture.destination_path).unwrap(),
        fixture.destinations
    );
    assert_eq!(
        CredentialRegistry::load(&fixture.credential_path).unwrap(),
        fixture.credentials
    );
}

#[test]
fn unchanged_connection_snapshot_allows_acknowledged_identity_restoration_only() {
    let fixture = Fixture::new();
    let mut storage = InjectedStorage {
        destination_failure: Failure::BeforePublish,
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("connection registry is unchanged on re-read"));
    assert!(
        error
            .contains("Restoring the original SSH identity registry was acknowledged successfully")
    );
    assert!(error.contains("Connection save returned an error; its durable result is unconfirmed"));
    assert_eq!(storage.credential_writes, 2);
    assert_eq!(
        CredentialRegistry::load(&fixture.credential_path).unwrap(),
        fixture.original_credentials
    );
}

#[test]
fn identity_restoration_error_after_publication_is_observed_but_not_reported_as_success() {
    let fixture = Fixture::new();
    let mut storage = InjectedStorage {
        credential_failures: VecDeque::from([Failure::None, Failure::AfterPublish]),
        destination_failure: Failure::BeforePublish,
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("Restoring the original SSH identity registry also returned an error"));
    assert!(error.contains("original identity registry is visible on re-read"));
    assert!(!error.contains("acknowledged successfully"));
    assert_eq!(
        CredentialRegistry::load(&fixture.credential_path).unwrap(),
        fixture.original_credentials
    );
}

#[test]
fn unreadable_or_changed_snapshots_never_restore_an_identity_blindly() {
    let fixture = Fixture::new();
    let mut storage = InjectedStorage {
        destination_failure: Failure::BeforePublish,
        cannot_read_destinations: true,
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("connection registry could not be re-read"));
    assert_eq!(storage.credential_writes, 1);
    let mut changed = fixture.credentials.clone();
    changed
        .create(SshCredential::Agent {
            fingerprint: "SHA256:unrelated-identity".into(),
        })
        .unwrap();
    let mut storage = InjectedStorage {
        destination_failure: Failure::BeforePublish,
        different_credentials: Some(changed),
        ..Default::default()
    };
    let error = fixture
        .registration()
        .persist_with(&mut storage)
        .unwrap_err();
    assert!(error.contains("identity registry changed or could not be re-read"));
    assert_eq!(storage.credential_writes, 1);
}

#[test]
fn successful_registration_and_saved_identity_do_not_gain_failure_side_effects() {
    let fixture = Fixture::new();
    fixture.registration().persist().unwrap();
    assert_eq!(
        CredentialRegistry::load(&fixture.credential_path).unwrap(),
        fixture.credentials
    );
    assert_eq!(
        DestinationRegistry::load(&fixture.destination_path).unwrap(),
        fixture.destinations
    );
    let mut registration = fixture.registration();
    registration.created_credential = false;
    let mut storage = InjectedStorage {
        destination_failure: Failure::AfterPublish,
        ..Default::default()
    };
    let error = registration.persist_with(&mut storage).unwrap_err();
    assert!(error.contains("new connection is visible on re-read"));
    assert_eq!(storage.credential_writes, 0);
}
