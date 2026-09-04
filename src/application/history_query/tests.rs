use super::*;
use crate::{
    domain::{
        ComponentDeploymentResult, ComponentGeneration, ComponentName, ComponentOutcome,
        DeploymentState, DestinationKey, DestinationRevision, DriverCapabilities, ReleaseManifest,
        ReleaseVersion,
    },
    drivers::{DriverKind, EndpointFingerprint, ReleaseRef},
    history::{
        CurrentAlignment, DeploymentComponentSnapshot, DeploymentMetadata, GitWorktree,
        InspectionScope, IntentStatus, PackageAlignment, RecoveryComponentReport, RollingLogWriter,
    },
};

struct Fixture {
    directory: tempfile::TempDir,
    service: HistoryQueryService,
    store: HistoryStore,
    id: DeploymentId,
    release: ReleaseRef,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let store = HistoryStore::open(&path).unwrap();
        let id = DeploymentId::new();
        let release = ReleaseRef {
            driver: DriverKind::linux_ssh(),
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse("v1").unwrap(),
            destination: DestinationKey::new(),
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
                    release: release.clone(),
                    expected_current: None,
                    target: Some(release.clone()),
                    execution_order: 0,
                }],
            )
            .unwrap();
        let service = HistoryQueryService::new(path, Redactor::new(["TOKEN".into()]));
        Self {
            directory,
            service,
            store,
            id,
            release,
        }
    }

    fn log(&self, query: HistoricalLogQuery) -> Result<HistoricalLogPage, HistoryQueryError> {
        self.service.log_page(
            &self.release.project_id,
            &self.release.environment_id,
            &self.id,
            query,
        )
    }

    fn register_log(&self, limit: u64, retained: u32) -> PathBuf {
        self.store
            .register_deployment_log(&self.id, limit, retained)
            .unwrap();
        let directory = self.directory.path().join("logs");
        fs::create_dir(&directory).unwrap();
        directory
    }

    fn saved_report(&self, unknown: bool) -> RecoveryReport {
        RecoveryReport {
            id: uuid::Uuid::now_v7(),
            related_deployment: None,
            source_revision: None,
            started_at_ms: 10,
            completed_at_ms: 11,
            components: vec![RecoveryComponentReport {
                scope: InspectionScope::from(&self.release),
                inventory: Err(if unknown {
                    "TOKEN unavailable"
                } else {
                    "earlier unavailable"
                }
                .into()),
                alignment: CurrentAlignment::Unplanned,
                package_alignment: PackageAlignment::Unplanned,
                notices: Vec::new(),
            }],
        }
    }
}

#[test]
fn missing_database_lists_are_explicit_and_never_create_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("absent/history.sqlite3");
    let service = HistoryQueryService::new(path.clone(), Redactor::default());
    let project = ProjectId::new();
    let environment = EnvironmentId::new();
    let environments = service
        .environments(&project, RecoveryQuery::default())
        .unwrap();
    assert!(environments.database_missing && environments.items.is_empty() && !environments.more);
    let page = service
        .deployments(&project, &environment, DeploymentQuery::default())
        .unwrap();
    assert!(page.database_missing && page.items.is_empty() && !page.more);
    assert!(
        service
            .recovery_reports(&project, &environment, RecoveryQuery::default())
            .unwrap()
            .database_missing
    );
    assert_eq!(
        service.deployment(&project, &environment, &DeploymentId::new()),
        Err(HistoryQueryError::DatabaseMissing)
    );
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn historical_environment_pages_ignore_yaml_and_merge_report_only_ids() {
    let fixture = Fixture::new();
    let project = &fixture.release.project_id;
    let recreated: EnvironmentId = "env_00000001".parse().unwrap();
    let report_only: EnvironmentId = "env_00000002".parse().unwrap();
    fixture
        .store
        .create_deployment(&DeploymentId::new(), project, &recreated, 2)
        .unwrap();
    fixture
        .store
        .create_deployment(&DeploymentId::new(), project, &recreated, 3)
        .unwrap();
    let mut report = fixture.saved_report(true);
    report.components[0].scope.environment = report_only.clone();
    fixture
        .store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    let mut duplicate = fixture.saved_report(false);
    duplicate.components[0].scope.environment = recreated.clone();
    fixture
        .store
        .append_recovery_report(&duplicate, &Redactor::default())
        .unwrap();
    let mut foreign = fixture.saved_report(true);
    foreign.components[0].scope.project = ProjectId::new();
    foreign.components[0].scope.environment = "env_00000000".parse().unwrap();
    fixture
        .store
        .append_recovery_report(&foreign, &Redactor::default())
        .unwrap();
    // Missing or invalid current YAML cannot hide a deleted historical identity.
    let yaml = fixture.directory.path().join("shipforge.yaml");
    assert!(!yaml.exists());
    let original = fixture
        .service
        .environments(project, RecoveryQuery::default())
        .unwrap();
    let mut expected = vec![
        fixture.release.environment_id.clone(),
        recreated,
        report_only,
    ];
    expected.sort_by_key(ToString::to_string);
    assert_eq!(original.items, expected);
    assert!(!original.database_missing && !original.more);
    fs::write(&yaml, b"invalid replacement project configuration").unwrap();
    assert_eq!(
        fixture
            .service
            .environments(project, RecoveryQuery::default())
            .unwrap(),
        original
    );
    for (offset, expected) in original.items.iter().enumerate() {
        let page = fixture
            .service
            .environments(
                project,
                RecoveryQuery {
                    limit: 1,
                    offset: u32::try_from(offset).unwrap(),
                },
            )
            .unwrap();
        assert_eq!(page.items.as_slice(), std::slice::from_ref(expected));
        assert_eq!(page.more, offset + 1 < original.items.len());
    }
    assert!(
        fixture
            .service
            .environments(&ProjectId::new(), RecoveryQuery::default())
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn environment_pages_reject_invalid_bounds_and_schemas_without_modifying_files() {
    let fixture = Fixture::new();
    for query in [
        RecoveryQuery {
            limit: 0,
            offset: 0,
        },
        RecoveryQuery {
            limit: 101,
            offset: 0,
        },
        RecoveryQuery {
            limit: 1,
            offset: 1_000_001,
        },
    ] {
        assert_eq!(
            fixture
                .service
                .environments(&fixture.release.project_id, query),
            Err(HistoryQueryError::InvalidPage)
        );
    }
    let directory = tempfile::tempdir().unwrap();
    for version in [0, 5, 6, 8] {
        let path = directory.path().join(format!("history-{version}.sqlite3"));
        let database = rusqlite::Connection::open(&path).unwrap();
        database
            .execute_batch("CREATE TABLE unchanged(value TEXT)")
            .unwrap();
        database
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(database);
        let before = fs::read(&path).unwrap();
        let service = HistoryQueryService::new(path.clone(), Redactor::default());
        assert!(
            service
                .environments(&ProjectId::new(), RecoveryQuery::default())
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

fn prepare_details_fixture(fixture: &Fixture) -> ReleaseManifest {
    let release = &fixture.release;
    fixture
        .store
        .record_deployment_metadata(
            &fixture.id,
            &DeploymentMetadata {
                git_branch: Some("TOKEN-topic".into()),
                git_revision: Some("abcdef123".into()),
                git_worktree: GitWorktree::Dirty,
                operator: Some("TOKEN-user".into()),
            },
            &Redactor::default(),
        )
        .unwrap();
    fixture
        .store
        .plan_steps(&fixture.id, &release.component, &["prepare", "activate"])
        .unwrap();
    fixture
        .store
        .transition_deployment(
            &fixture.id,
            DeploymentState::Created,
            DeploymentState::Running,
            2,
        )
        .unwrap();
    let intent = fixture
        .store
        .record_intent(&fixture.id, &release.component, "prepare", "v1", 3)
        .unwrap();
    let manifest = ReleaseManifest {
        schema_version: 1,
        project_id: release.project_id.clone(),
        environment_id: release.environment_id.clone(),
        component: release.component.clone(),
        generation: release.generation,
        version: release.version.clone(),
        created_at_unix: 3,
        source_revision: Some("abcdef123".into()),
    };
    fixture
        .store
        .record_release_package(&fixture.id, release, &manifest, &"a".repeat(64), 42)
        .unwrap();
    fixture
        .store
        .record_release_receipt(&fixture.id, &release.component, "prepare", release, 4)
        .unwrap();
    fixture
        .store
        .complete_intent(
            intent,
            IntentStatus::Succeeded,
            None,
            4,
            &Redactor::default(),
        )
        .unwrap();
    manifest
}

#[test]
fn details_preserve_all_original_evidence_unknown_and_known_absence() {
    let fixture = Fixture::new();
    let release = &fixture.release;
    let manifest = prepare_details_fixture(&fixture);
    fixture
        .store
        .record_intent(&fixture.id, &release.component, "activate", "v1", 5)
        .unwrap();
    fixture
        .store
        .record_observation(
            &fixture.id,
            &release.component,
            "before",
            Ok(None),
            None,
            5,
            &Redactor::default(),
        )
        .unwrap();
    fixture
        .store
        .record_observation(
            &fixture.id,
            &release.component,
            "after",
            Err("TOKEN unknown"),
            None,
            6,
            &Redactor::default(),
        )
        .unwrap();
    fixture
        .store
        .record_component_result(
            &fixture.id,
            &release.component,
            &ComponentDeploymentResult {
                outcome: ComponentOutcome::Failed,
                attempted_release: Some(release.version.clone()),
                observed_release: None,
            },
            Some("TOKEN failed"),
            &Redactor::default(),
        )
        .unwrap();
    fixture
        .store
        .transition_deployment(
            &fixture.id,
            DeploymentState::Running,
            DeploymentState::Failed,
            7,
        )
        .unwrap();
    let before = fixture.store.recovery_basis(&fixture.id).unwrap();
    let details = fixture
        .service
        .deployment(&release.project_id, &release.environment_id, &fixture.id)
        .unwrap();
    assert_eq!(details.record.pending_intent_count, 1);
    assert_eq!(details.snapshots.len(), 1);
    assert_eq!(details.packages[0].manifest, manifest);
    assert_eq!(details.receipts[0].release, *release);
    assert_eq!(details.pending.len(), 1);
    assert_eq!(details.steps.len(), 2);
    assert_eq!(
        details.results[0].error.as_deref(),
        Some("[REDACTED] failed")
    );
    assert_eq!(details.observations[0].observed, Ok(None));
    assert_eq!(
        details.observations[1].observed,
        Err("[REDACTED] unknown".into())
    );
    assert!(
        details
            .observations
            .iter()
            .all(|value| value.healthy.is_none())
    );
    assert_eq!(
        details.metadata.unwrap().operator.as_deref(),
        Some("[REDACTED]-user")
    );
    assert_eq!(fixture.store.recovery_basis(&fixture.id).unwrap(), before);
    assert_eq!(
        fixture
            .service
            .deployment(&ProjectId::new(), &release.environment_id, &fixture.id),
        Err(HistoryQueryError::NotFound)
    );
}

#[test]
fn saved_reports_are_scoped_paged_and_latest_unknown_does_not_fall_back() {
    let fixture = Fixture::new();
    let first = fixture.saved_report(false);
    fixture
        .store
        .append_recovery_report(&first, &Redactor::default())
        .unwrap();
    let mut second = fixture.saved_report(true);
    second.started_at_ms = 1;
    second.completed_at_ms = 2;
    fixture
        .store
        .append_recovery_report(&second, &Redactor::default())
        .unwrap();
    let project = &fixture.release.project_id;
    let environment = &fixture.release.environment_id;
    let page = fixture
        .service
        .recovery_reports(
            project,
            environment,
            RecoveryQuery {
                limit: 1,
                offset: 0,
            },
        )
        .unwrap();
    assert!(page.more && !page.database_missing);
    assert_eq!(page.items[0].id, second.id);
    assert_eq!(
        page.items[0].components[0].inventory,
        Err("[REDACTED] unavailable".into())
    );
    let next = fixture
        .service
        .recovery_reports(
            project,
            environment,
            RecoveryQuery {
                limit: 1,
                offset: 1,
            },
        )
        .unwrap();
    assert!(!next.more);
    assert_eq!(next.items, [first]);
    assert_eq!(
        fixture
            .service
            .recovery_report(&ProjectId::new(), environment, &second.id),
        Err(HistoryQueryError::NotFound)
    );
    assert_eq!(
        fixture
            .service
            .recovery_report(project, environment, &second.id)
            .unwrap()
            .id,
        second.id
    );
}

#[test]
fn indexed_rotated_logs_are_real_persisted_bytes_redacted_before_paging() {
    let fixture = Fixture::new();
    let directory = fixture.register_log(128, 2);
    let mut writer = RollingLogWriter::open(&directory, &fixture.id, 128, 2).unwrap();
    writer
        .append(&"older".repeat(25), &Redactor::default())
        .unwrap();
    writer
        .append("head TOKEN\n\t尾部\u{1b}[31m\u{0}\r", &Redactor::default())
        .unwrap();
    drop(writer);
    let original = fs::read(log_path(&directory, &fixture.id, 0)).unwrap();
    let first = fixture
        .log(HistoricalLogQuery {
            max_bytes: 9,
            ..HistoricalLogQuery::default()
        })
        .unwrap();
    assert_eq!(first.status, HistoricalLogStatus::Ready);
    assert_eq!(first.available_generations, [0, 1]);
    assert_eq!(first.text, "head [RED");
    let rest = fixture
        .log(HistoricalLogQuery {
            offset: first.next_offset.unwrap(),
            ..HistoricalLogQuery::default()
        })
        .unwrap();
    let combined = format!("{}{}", first.text, rest.text);
    assert_eq!(combined, "head [REDACTED]\n\t尾部[31m");
    assert_eq!(rest.next_offset, None);
    assert!(!combined.contains("TOKEN"));
    assert_eq!(
        fs::read(log_path(&directory, &fixture.id, 0)).unwrap(),
        original
    );
    let older = fixture
        .log(HistoricalLogQuery {
            generation: 1,
            ..HistoricalLogQuery::default()
        })
        .unwrap();
    assert_eq!(older.text, "older".repeat(25));
    assert_eq!(
        fixture
            .log(HistoricalLogQuery {
                generation: 2,
                ..HistoricalLogQuery::default()
            })
            .unwrap()
            .status,
        HistoricalLogStatus::Missing
    );
}

#[test]
fn terminal_control_removal_cannot_reconstruct_a_secret_across_log_pages() {
    let fixture = Fixture::new();
    let directory = fixture.register_log(1024, 1);
    let path = log_path(&directory, &fixture.id, 0);
    let original = "TO\0KEN TO\u{202e}KEN\n\t尾部";
    fs::write(&path, original).unwrap();
    let mut query = HistoricalLogQuery {
        max_bytes: 4,
        ..HistoricalLogQuery::default()
    };
    let mut visible = String::new();
    loop {
        let page = fixture.log(query).unwrap();
        visible.push_str(&page.text);
        let Some(offset) = page.next_offset else {
            break;
        };
        query.offset = offset;
    }
    assert_eq!(visible, "[REDACTED] [REDACTED]\n\t尾部");
    assert!(!visible.contains("TOKEN"));
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn logs_distinguish_missing_index_file_and_invalid_limits() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()).unwrap().status,
        HistoricalLogStatus::NotIndexed
    );
    let directory = fixture.register_log(32, 1);
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()).unwrap().status,
        HistoricalLogStatus::Missing
    );
    let path = log_path(&directory, &fixture.id, 0);
    fs::write(&path, b"valid").unwrap();
    assert_eq!(
        fixture.log(HistoricalLogQuery {
            generation: 2,
            ..HistoricalLogQuery::default()
        }),
        Err(HistoryQueryError::InvalidPage)
    );
    assert_eq!(
        fixture.log(HistoricalLogQuery {
            max_bytes: MAX_LOG_PAGE_BYTES + 1,
            ..HistoricalLogQuery::default()
        }),
        Err(HistoryQueryError::InvalidPage)
    );
    assert_eq!(
        fixture.log(HistoricalLogQuery {
            offset: 99,
            ..HistoricalLogQuery::default()
        }),
        Err(HistoryQueryError::InvalidPage)
    );
    fs::write(path, vec![b'x'; 33]).unwrap();
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()),
        Err(HistoryQueryError::LogLimit)
    );
}

#[test]
fn database_log_index_cannot_authorize_an_arbitrary_file() {
    let fixture = Fixture::new();
    fixture.register_log(1024, 1);
    let outside = fixture.directory.path().join("secret.txt");
    fs::write(&outside, b"SECRET OUTSIDE LOGS").unwrap();
    let connection = rusqlite::Connection::open(&fixture.service.history_path).unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    connection
        .execute(
            "UPDATE deployment_logs SET relative_path=?1",
            [outside.to_str().unwrap()],
        )
        .unwrap();
    let error = fixture.log(HistoricalLogQuery::default()).unwrap_err();
    assert_eq!(error, HistoryQueryError::HistoryUnavailable);
    assert!(!format!("{error:?} {error}").contains("SECRET"));
}

#[test]
fn linked_log_files_and_nonregular_paths_are_rejected() {
    let fixture = Fixture::new();
    let directory = fixture.register_log(1024, 1);
    let outside = fixture.directory.path().join("outside.txt");
    fs::write(&outside, b"outside secret").unwrap();
    let path = log_path(&directory, &fixture.id, 0);
    fs::hard_link(&outside, &path).unwrap();
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()),
        Err(HistoryQueryError::UnsafeLog)
    );
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()),
        Err(HistoryQueryError::UnsafeLog)
    );
}

#[cfg(unix)]
#[test]
fn symbolic_log_files_and_directories_never_expose_external_content() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let directory = fixture.register_log(1024, 1);
    let outside = fixture.directory.path().join("outside.txt");
    fs::write(&outside, b"outside secret").unwrap();
    let path = log_path(&directory, &fixture.id, 0);
    symlink(&outside, &path).unwrap();
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()),
        Err(HistoryQueryError::UnsafeLog)
    );
    fs::remove_file(&path).unwrap();
    fs::remove_dir(&directory).unwrap();
    symlink(fixture.directory.path(), &directory).unwrap();
    assert_eq!(
        fixture.log(HistoricalLogQuery::default()),
        Err(HistoryQueryError::UnsafeLog)
    );
}

#[test]
fn log_pagination_preserves_utf8_and_rejects_interior_offsets() {
    let text = "a中b";
    let page = paginate_log(
        text,
        HistoricalLogQuery {
            max_bytes: 3,
            ..HistoricalLogQuery::default()
        },
        vec![0],
    )
    .unwrap();
    assert_eq!(page.text, "a");
    assert_eq!(page.next_offset, Some(1));
    assert_eq!(
        paginate_log(
            text,
            HistoricalLogQuery {
                offset: 2,
                ..HistoricalLogQuery::default()
            },
            vec![0]
        ),
        Err(HistoryQueryError::InvalidPage)
    );
}

#[test]
fn stored_corruption_is_reported_without_raw_diagnostic_values() {
    let fixture = Fixture::new();
    let connection = rusqlite::Connection::open(&fixture.service.history_path).unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    connection
        .execute("UPDATE deployments SET state='TOKEN'", [])
        .unwrap();
    let error = fixture
        .service
        .deployments(
            &fixture.release.project_id,
            &fixture.release.environment_id,
            DeploymentQuery::default(),
        )
        .unwrap_err();
    assert_eq!(error, HistoryQueryError::HistoryUnavailable);
    assert!(!format!("{error:?}: {error}").contains("TOKEN"));
}
