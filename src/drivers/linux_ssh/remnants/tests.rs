use std::{fmt::Write as _, sync::Mutex};

use crate::{
    domain::{
        ComponentGeneration, ComponentName, ComponentRelease, DestinationKey, DestinationRevision,
        EnvironmentId, ProjectId, ReleaseVersion,
    },
    drivers::DriverTargetInput,
};

use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    Malformed,
    Truncated,
    Timeout,
    Changed,
    Hang,
    Cancel,
}

struct FakeRemote {
    root: Result<bool, RemnantsError>,
    nodes: BTreeMap<String, u8>,
    marker_links: u64,
    fault: Fault,
    commands: Mutex<Vec<CommandSpec>>,
}

impl Default for FakeRemote {
    fn default() -> Self {
        Self {
            root: Ok(true),
            nodes: BTreeMap::from([
                ("/srv/app".into(), b'd'),
                ("/srv/app/.shipforge-project.json".into(), b'f'),
                ("/srv/app/temporary".into(), b'd'),
            ]),
            marker_links: 1,
            fault: Fault::None,
            commands: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl RemnantsRemote for FakeRemote {
    async fn root(
        &self,
        _: &LinuxSshTarget,
        _: &DeploymentMarker,
        _: &CancellationToken,
    ) -> Result<bool, RemnantsError> {
        self.root
    }

    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, RemnantsError> {
        if self.fault == Fault::Hang {
            cancellation.cancelled().await;
            return Err(RemnantsError::Cancelled);
        }
        if self.fault == Fault::Cancel {
            cancellation.cancel();
            return Err(RemnantsError::Cancelled);
        }
        let args = command
            .args
            .iter()
            .map(CommandArgument::expose_for_execution)
            .collect::<Vec<_>>();
        let prior = self
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|prior| *prior == command)
            .count();
        self.commands.lock().unwrap().push(command.clone());
        let mut output = RemoteCommandOutput {
            exit_status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        match (command.program.as_str(), args.as_slice()) {
            ("stat", ["--format=%h", "--", "/srv/app/.shipforge-project.json"]) => {
                output.stdout = format!("{}\n", self.marker_links).into_bytes();
            }
            ("test", [flag, path]) => {
                let kind = self.nodes.get(*path);
                let yes = match *flag {
                    "-L" => kind == Some(&b'l'),
                    "-e" => kind.is_some(),
                    "-d" => kind == Some(&b'd'),
                    _ => panic!("unexpected predicate"),
                };
                output.exit_status = u32::from(!yes);
            }
            (
                "timeout",
                [
                    "--signal=TERM",
                    "--kill-after=2s",
                    "15s",
                    "find",
                    path,
                    "-mindepth",
                    "1",
                    "-maxdepth",
                    "1",
                    "-printf",
                    "%f\\0%y\\0%s\\0",
                ],
            ) => {
                let prefix = format!("{path}/");
                for (path, kind) in &self.nodes {
                    if let Some(name) = path
                        .strip_prefix(&prefix)
                        .filter(|name| !name.contains('/'))
                    {
                        output.stdout.extend_from_slice(
                            format!("{name}\0{}\0{}\0", char::from(*kind), 0).as_bytes(),
                        );
                    }
                }
                match self.fault {
                    Fault::Malformed => output.stdout = b"malformed SECRET".to_vec(),
                    Fault::Truncated => output.stdout_truncated = true,
                    Fault::Timeout => output.exit_status = 124,
                    Fault::Changed if prior > 0 => {
                        output.stdout.extend_from_slice(b"changed\0f\x000\0");
                    }
                    _ => {}
                }
            }
            _ => panic!("unexpected remote command: {command:?}"),
        }
        Ok(output)
    }
}

fn fixture() -> (LinuxSshTarget, DeploymentMarker) {
    let release = ComponentRelease {
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("api").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: ReleaseVersion::parse("v1").unwrap(),
        destination: DestinationKey::new(),
        destination_revision: DestinationRevision::INITIAL,
    };
    (
        LinuxSshTarget::validate(&DriverTargetInput {
            value: serde_json::json!({"root":"/srv/app"}),
        })
        .unwrap(),
        DeploymentMarker::for_release(&release),
    )
}

async fn check(remote: &FakeRemote) -> Result<TemporaryRemnants, RemnantsError> {
    let (target, marker) = fixture();
    inspect(
        remote,
        &target,
        &marker,
        &CancellationToken::new(),
        SCAN_TIMEOUT,
    )
    .await
}

#[tokio::test]
async fn recognizes_only_canonical_remnants_without_reading_or_following_them() {
    let mut remote = FakeRemote::default();
    let deployment = DeploymentId::new();
    for (suffix, kind) in [
        ("tar.gz", b'f'),
        ("dir", b'd'),
        ("current", b'l'),
        ("rollback-current", b'l'),
    ] {
        remote
            .nodes
            .insert(format!("/srv/app/temporary/{deployment}.{suffix}"), kind);
    }
    remote.nodes.insert(
        format!(
            "/srv/app/.shipforge-marker-{}.tmp",
            uuid::Uuid::now_v7().simple()
        ),
        b'f',
    );
    let result = check(&remote).await.unwrap();
    assert!(!result.incomplete);
    assert_eq!(result.entries.len(), 5);
    assert!(result.entries.iter().all(TemporaryRemnant::is_valid));
    for entry in &result.entries {
        assert_eq!(
            entry.deployment.as_ref(),
            (entry.kind != TemporaryRemnantKind::MarkerPublication).then_some(&deployment)
        );
    }
    assert!(
        remote
            .commands
            .lock()
            .unwrap()
            .iter()
            .all(|command| matches!(command.program.as_str(), "stat" | "test" | "timeout"))
    );
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(!encoded.contains("/srv"));
    assert_eq!(
        serde_json::from_str::<TemporaryRemnants>(&encoded).unwrap(),
        result
    );
}

#[tokio::test]
async fn missing_or_empty_unmarked_root_is_confirmed_empty_without_listing() {
    let remote = FakeRemote {
        root: Ok(false),
        ..FakeRemote::default()
    };
    assert_eq!(check(&remote).await.unwrap(), TemporaryRemnants::default());
    assert!(remote.commands.lock().unwrap().is_empty());
    let mut marked = FakeRemote::default();
    marked.nodes.remove("/srv/app/temporary");
    assert_eq!(check(&marked).await.unwrap(), TemporaryRemnants::default());
}

#[tokio::test]
async fn refuses_mismatched_root_linked_namespace_and_shared_marker() {
    let remote = FakeRemote {
        root: Err(RemnantsError::UnsafeRoot),
        ..FakeRemote::default()
    };
    assert_eq!(check(&remote).await, Err(RemnantsError::UnsafeRoot));
    assert!(remote.commands.lock().unwrap().is_empty());
    let mut linked = FakeRemote::default();
    linked.nodes.insert("/srv/app/temporary".into(), b'l');
    assert_eq!(check(&linked).await, Err(RemnantsError::UnsafeRoot));
    let shared = FakeRemote {
        marker_links: 2,
        ..FakeRemote::default()
    };
    assert_eq!(check(&shared).await, Err(RemnantsError::SharedMarker));
    assert_eq!(shared.commands.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_names_and_wrong_types_remain_unattributed_without_raw_diagnostics() {
    let mut remote = FakeRemote::default();
    remote.nodes.insert(
        format!("/srv/app/temporary/{}.tar.gz", DeploymentId::new()),
        b'l',
    );
    remote
        .nodes
        .insert("/srv/app/temporary/SECRET\nname.dir".into(), b'd');
    remote
        .nodes
        .insert("/srv/app/.shipforge-marker-not-a-uuid.tmp".into(), b'f');
    let result = check(&remote).await.unwrap();
    assert!(result.incomplete);
    assert!(result.entries.is_empty());
    let encoded = serde_json::to_string(&result).unwrap();
    assert!(!encoded.contains("SECRET") && !encoded.contains("not-a-uuid"));
    assert!(
        !remote
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.program == "readlink" || command.program == "head")
    );
}

#[tokio::test]
async fn incomplete_changed_and_timed_out_lists_never_claim_empty_absence() {
    for (fault, error) in [
        (Fault::Malformed, RemnantsError::Remote),
        (Fault::Truncated, RemnantsError::Limit),
        (Fault::Timeout, RemnantsError::Timeout),
        (Fault::Changed, RemnantsError::Remote),
    ] {
        assert_eq!(
            check(&FakeRemote {
                fault,
                ..FakeRemote::default()
            })
            .await,
            Err(error)
        );
    }
}

#[test]
fn listing_parser_bounds_entries_bytes_names_and_duplicates() {
    assert_eq!(
        parse_listing(&vec![b'x'; MAX_LIST_BYTES + 1]),
        Err(RemnantsError::Limit)
    );
    let mut many = String::new();
    for index in 0..=MAX_ENTRIES {
        write!(&mut many, "{index}\0f\0{}\0", 0).unwrap();
    }
    assert_eq!(parse_listing(many.as_bytes()), Err(RemnantsError::Limit));
    for bytes in [
        &b"a\0f\x000\0a\0f\x000\0"[..],
        b"../escape\0f\x000\0",
        b"a\0f\0bad\0",
        b"a\0f\x000",
    ] {
        assert_eq!(parse_listing(bytes), Err(RemnantsError::Remote));
    }
}

#[tokio::test]
async fn combined_namespace_limit_is_enforced() {
    let mut remote = FakeRemote::default();
    for index in 0..MAX_ENTRIES {
        remote
            .nodes
            .insert(format!("/srv/app/temporary/unrecognized-{index}"), b'f');
    }
    assert_eq!(check(&remote).await, Err(RemnantsError::Limit));
}

#[tokio::test]
async fn cancellation_deadline_and_invalid_paths_fail_without_mutations() {
    let (mut target, marker) = fixture();
    let remote = FakeRemote::default();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        inspect(&remote, &target, &marker, &cancellation, SCAN_TIMEOUT).await,
        Err(RemnantsError::Cancelled)
    );
    target.root = "/srv/../external".into();
    assert_eq!(
        inspect(
            &remote,
            &target,
            &marker,
            &CancellationToken::new(),
            SCAN_TIMEOUT
        )
        .await,
        Err(RemnantsError::UnsafeRoot)
    );
    assert!(remote.commands.lock().unwrap().is_empty());
    assert_eq!(
        check(&FakeRemote {
            fault: Fault::Cancel,
            ..FakeRemote::default()
        })
        .await,
        Err(RemnantsError::Cancelled)
    );
    let (target, marker) = fixture();
    assert_eq!(
        inspect(
            &FakeRemote {
                fault: Fault::Hang,
                ..FakeRemote::default()
            },
            &target,
            &marker,
            &CancellationToken::new(),
            Duration::from_millis(1)
        )
        .await,
        Err(RemnantsError::Timeout)
    );
}

#[test]
fn serialized_remnant_shapes_reject_unknown_fields_and_validate_attribution() {
    let marker = TemporaryRemnant {
        kind: TemporaryRemnantKind::MarkerPublication,
        deployment: None,
    };
    assert!(marker.is_valid());
    assert!(
        !TemporaryRemnant {
            deployment: Some(DeploymentId::new()),
            ..marker
        }
        .is_valid()
    );
    assert!(
        !TemporaryRemnant {
            kind: TemporaryRemnantKind::UploadArchive,
            deployment: None
        }
        .is_valid()
    );
    assert!(
        serde_json::from_str::<TemporaryRemnants>(
            r#"{"entries":[],"notices":[],"incomplete":false,"path":"SECRET"}"#
        )
        .is_err()
    );
}
