use std::{collections::VecDeque, sync::Mutex};

use async_trait::async_trait;

use super::super::{RemoteCommandOutput, SshConnectionError};
use super::*;
use crate::{
    application::package_release,
    config::{ResolvedArtifact, ResolvedArtifactKind},
    domain::{
        ComponentGeneration, ComponentName, ComponentRelease, DestinationKey, DestinationRevision,
        EnvironmentId, ProjectId, ReleaseVersion,
    },
    drivers::DriverTargetInput,
    telemetry::CommandSpec,
};

struct FakeRemote {
    replies: Mutex<VecDeque<String>>,
    commands: Mutex<Vec<String>>,
}

impl FakeRemote {
    fn new(capacity: &str) -> Self {
        Self {
            replies: Mutex::new(VecDeque::from(["/srv\n".into(), capacity.into()])),
            commands: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl PreflightRemote for FakeRemote {
    async fn command(
        &self,
        command: &CommandSpec,
        _: Duration,
        _: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.commands
            .lock()
            .unwrap()
            .push(command.render_posix().unwrap());
        Ok(RemoteCommandOutput {
            exit_status: 0,
            stdout: self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("test reply")
                .into_bytes(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        })
    }
}

fn release() -> ComponentRelease {
    ComponentRelease {
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("api").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: ReleaseVersion::parse("v1").unwrap(),
        destination: DestinationKey::new(),
        destination_revision: DestinationRevision::INITIAL,
    }
}

fn package(directory: &std::path::Path, bytes: usize) -> ReleasePackage {
    let source = directory.join("app");
    std::fs::write(&source, vec![0; bytes]).unwrap();
    package_release(
        &ResolvedArtifact {
            path: source,
            kind: ResolvedArtifactKind::File,
        },
        &release(),
        &directory.join("packages"),
        1,
        None,
        &CancellationToken::new(),
    )
    .unwrap()
}

async fn check(
    remote: &FakeRemote,
    package: &ReleasePackage,
) -> Result<StorageRequirement, SpaceError> {
    let target = LinuxSshTarget::validate(&DriverTargetInput {
        value: serde_json::json!({"root": "/srv/app"}),
    })
    .unwrap();
    check_with_remote(
        remote,
        &target,
        package,
        Duration::from_secs(1),
        &CancellationToken::new(),
    )
    .await
}

#[tokio::test]
async fn checks_actual_archive_and_peak_layout_before_any_write() {
    let directory = tempfile::tempdir().unwrap();
    let package = package(directory.path(), 8193);
    let original = std::fs::read(package.path()).unwrap();
    let remote = FakeRemote::new("1048576:4096:10000:10000\n");
    let required = check(&remote, &package).await.unwrap();
    assert!(required.bytes > 8193 + package.size());
    assert_eq!(required.inodes, 11); // Eight layout objects, one root, payload + manifest.
    assert_eq!(original, std::fs::read(package.path()).unwrap());
    let commands = remote.commands.lock().unwrap();
    assert_eq!(commands.len(), 2);
    assert!(commands[0].starts_with("'sh' '-c' 'path=$1"));
    assert!(commands[1].starts_with("'stat' '--file-system'"));
}

#[tokio::test]
async fn compressed_size_cannot_hide_a_large_extracted_payload() {
    let directory = tempfile::tempdir().unwrap();
    let package = package(directory.path(), 4 * 1024 * 1024);
    assert!(package.size() < 64 * 1024);
    let error = check(&FakeRemote::new("256:4096:10000:10000\n"), &package)
        .await
        .unwrap_err();
    assert!(
        matches!(error, SpaceError::InsufficientBytes { required, available: 1_048_576 }
        if required > 4 * 1024 * 1024)
    );
}

#[tokio::test]
async fn rejects_low_bytes_and_inodes_but_allows_unreported_inode_accounting() {
    let directory = tempfile::tempdir().unwrap();
    let package = package(directory.path(), 1);
    assert!(matches!(
        check(&FakeRemote::new("1:4096:10000:10000"), &package).await,
        Err(SpaceError::InsufficientBytes { .. })
    ));
    assert!(matches!(
        check(&FakeRemote::new("1048576:4096:10000:1"), &package).await,
        Err(SpaceError::InsufficientInodes { .. })
    ));
    assert!(
        check(&FakeRemote::new("1048576:4096:0:0"), &package)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn missing_or_malformed_remote_capacity_never_defaults_to_unlimited() {
    let directory = tempfile::tempdir().unwrap();
    let package = package(directory.path(), 1);
    for capacity in ["", "1024:4096", "1024:0:0:0", "1024:4096:2:3"] {
        assert!(matches!(
            check(&FakeRemote::new(capacity), &package).await,
            Err(SpaceError::Preflight(PreflightError::InvalidOutput(_)))
        ));
    }
}

#[test]
fn rounding_and_metadata_calculations_are_checked_for_overflow() {
    assert_eq!(round_up(4097, 4096).unwrap(), 8192);
    assert_eq!(round_up(0, 4096).unwrap(), 0);
    assert!(round_up(u64::MAX, 4096).is_err());
    assert!(round_up(1, 0).is_err());
    assert!(file_storage(u64::MAX, 1).is_err());
    assert!(add(u64::MAX, 1).is_err());
}

#[test]
fn missing_manifest_and_changed_package_metadata_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut archive = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, "app", &b"x"[..]).unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    let path = directory.path().join("missing-manifest.tar.gz");
    std::fs::write(&path, &bytes).unwrap();
    let package = ReleasePackage::new(
        release(),
        path,
        format!("{:x}", Sha256::digest(&bytes)),
        u64::try_from(bytes.len()).unwrap(),
    );
    assert!(matches!(
        estimate_archive(&package, 4096, 1, &CancellationToken::new()),
        Err(SpaceError::InvalidArchive)
    ));
    std::fs::write(package.path(), b"changed").unwrap();
    assert!(matches!(
        estimate_archive(&package, 4096, 1, &CancellationToken::new()),
        Err(SpaceError::InvalidArchive)
    ));
}

#[test]
fn cancellation_stops_archive_inspection() {
    let directory = tempfile::tempdir().unwrap();
    let package = package(directory.path(), 1);
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        estimate_archive(&package, 4096, 1, &token),
        Err(SpaceError::Cancelled)
    ));
}
