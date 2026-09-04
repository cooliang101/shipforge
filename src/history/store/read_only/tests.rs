use super::*;

#[test]
fn read_only_open_never_creates_migrates_or_writes_history() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing/history.sqlite3");
    assert!(matches!(
        HistoryStore::open_existing_read_only(&missing),
        Err(HistoryError::Io { .. })
    ));
    assert!(!missing.parent().unwrap().exists());

    let path = directory.path().join("history.sqlite3");
    let writer = HistoryStore::open(&path).unwrap();
    let project = ProjectId::new();
    let environment = EnvironmentId::new();
    let id = DeploymentId::new();
    writer
        .create_deployment(&id, &project, &environment, 1)
        .unwrap();
    drop(writer);
    let bytes = std::fs::read(&path).unwrap();
    let reader = HistoryStore::open_existing_read_only(&path).unwrap();
    assert!(
        reader
            .create_deployment(&DeploymentId::new(), &project, &environment, 2)
            .is_err()
    );
    let details = reader
        .read_deployment_details(&project, &environment, &id)
        .unwrap()
        .unwrap();
    assert_eq!(details.record.deployment, id);
    assert!(details.metadata.is_none() && details.snapshots.is_empty());
    assert_eq!(
        reader
            .read_environment_page(&project, RecoveryQuery::default())
            .unwrap(),
        (vec![environment], false)
    );
    drop(reader);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

fn environment_report(project: &ProjectId, environment: &EnvironmentId) -> RecoveryReport {
    use crate::{
        domain::{ComponentGeneration, ComponentName, DestinationKey, DestinationRevision},
        drivers::{DriverKind, EndpointFingerprint},
        history::{CurrentAlignment, InspectionScope, PackageAlignment, RecoveryComponentReport},
    };
    RecoveryReport {
        id: uuid::Uuid::now_v7(),
        related_deployment: None,
        source_revision: None,
        started_at_ms: 1,
        completed_at_ms: 2,
        components: vec![RecoveryComponentReport {
            scope: InspectionScope {
                project: project.clone(),
                environment: environment.clone(),
                component: ComponentName::parse("api").unwrap(),
                generation: ComponentGeneration::INITIAL,
                driver: DriverKind::linux_ssh(),
                destination: DestinationKey::new(),
                destination_revision: DestinationRevision::INITIAL,
                endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            },
            inventory: Err("inventory unavailable".into()),
            alignment: CurrentAlignment::Unplanned,
            package_alignment: PackageAlignment::Unplanned,
            notices: Vec::new(),
        }],
    }
}

#[test]
fn historical_environment_index_combines_sources_without_duplicates_or_project_leaks() {
    let store = HistoryStore::in_memory().unwrap();
    let project = ProjectId::new();
    let first: EnvironmentId = "env_00000001".parse().unwrap();
    let second: EnvironmentId = "env_00000002".parse().unwrap();
    for environment in [&second, &first, &first] {
        store
            .create_deployment(&DeploymentId::new(), &project, environment, 1)
            .unwrap();
    }
    let third: EnvironmentId = "env_00000003".parse().unwrap();
    for environment in [&second, &third] {
        store
            .append_recovery_report(
                &environment_report(&project, environment),
                &crate::telemetry::Redactor::default(),
            )
            .unwrap();
    }
    let foreign_project = ProjectId::new();
    store
        .append_recovery_report(
            &environment_report(&foreign_project, &EnvironmentId::new()),
            &crate::telemetry::Redactor::default(),
        )
        .unwrap();
    store
        .create_deployment(
            &DeploymentId::new(),
            &foreign_project,
            &EnvironmentId::new(),
            1,
        )
        .unwrap();
    assert_eq!(
        store
            .read_environment_page(
                &project,
                RecoveryQuery {
                    limit: 2,
                    offset: 0
                }
            )
            .unwrap(),
        (vec![first, second], true)
    );
    assert_eq!(
        store
            .read_environment_page(
                &project,
                RecoveryQuery {
                    limit: 2,
                    offset: 2
                }
            )
            .unwrap(),
        (vec![third], false)
    );
    assert_eq!(
        store
            .read_environment_page(
                &project,
                RecoveryQuery {
                    limit: 1,
                    offset: 1_000_000
                }
            )
            .unwrap(),
        (Vec::new(), false)
    );
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
        assert!(matches!(
            store.read_environment_page(&project, query),
            Err(HistoryError::InvalidPage)
        ));
    }
}

#[test]
fn historical_environment_index_rejects_bad_identity_even_outside_requested_page() {
    for table in ["deployments", "recovery_reports"] {
        for value in [
            "zzz_bad".into(),
            "env_abcdefgh\0".into(),
            "env_éééééééé".into(),
            "x".repeat(1024 * 1024),
        ] {
            let store = HistoryStore::in_memory().unwrap();
            let project = ProjectId::new();
            let environment: EnvironmentId = "env_00000000".parse().unwrap();
            store
                .create_deployment(&DeploymentId::new(), &project, &environment, 1)
                .unwrap();
            let report = environment_report(&project, &environment);
            store
                .append_recovery_report(&report, &crate::telemetry::Redactor::default())
                .unwrap();
            store
                .connection
                .pragma_update(None, "ignore_check_constraints", true)
                .unwrap();
            if table == "deployments" {
                store.connection.execute(
                    "INSERT INTO deployments(id,project_id,environment_id,state,created_at_ms,updated_at_ms)
                     VALUES (?1,?2,?3,'created',1,1)",
                    params![DeploymentId::new().to_string(), project.to_string(), value],
                ).unwrap();
            } else {
                store.connection.execute(
                    "INSERT INTO recovery_reports(id,project_id,environment_id,started_at_ms,completed_at_ms,report)
                     SELECT ?1,project_id,?2,started_at_ms,completed_at_ms,report FROM recovery_reports WHERE id=?3",
                    params![uuid::Uuid::now_v7().to_string(), value, report.id.to_string()],
                ).unwrap();
            }
            for offset in [0, 100] {
                assert!(matches!(
                    store.read_environment_page(&project, RecoveryQuery { limit: 1, offset }),
                    Err(HistoryError::Corrupt(_))
                ));
            }
        }
    }
}

#[test]
fn historical_environment_limit_counts_distinct_ids_and_never_returns_a_truncated_prefix() {
    let store = HistoryStore::in_memory().unwrap();
    let project = ProjectId::new();
    store
        .connection
        .execute(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<4096)
         INSERT INTO deployments(id,project_id,environment_id,state,created_at_ms,updated_at_ms)
         SELECT 'dep_'||printf('%08d',n),?1,'env_'||printf('%08d',n),'created',1,1 FROM seq",
            [project.to_string()],
        )
        .unwrap();
    let page = store
        .read_environment_page(
            &project,
            RecoveryQuery {
                limit: 100,
                offset: 4000,
            },
        )
        .unwrap();
    assert_eq!(page.0.len(), 96);
    assert!(!page.1);
    store
        .create_deployment(
            &DeploymentId::new(),
            &project,
            &"env_00000001".parse().unwrap(),
            1,
        )
        .unwrap();
    assert_eq!(
        store
            .read_environment_page(
                &project,
                RecoveryQuery {
                    limit: 100,
                    offset: 4000
                }
            )
            .unwrap(),
        page
    );
    store
        .create_deployment(
            &DeploymentId::new(),
            &project,
            &"env_00004097".parse().unwrap(),
            1,
        )
        .unwrap();
    assert!(matches!(
        store.read_environment_page(
            &project,
            RecoveryQuery {
                limit: 1,
                offset: 0
            }
        ),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn rejects_old_future_uninitialized_and_non_database_files_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    for version in [0, 1, 2, 3, 4, 5, 7] {
        let path = directory.path().join(format!("schema-{version}.sqlite3"));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE unchanged(value TEXT)")
            .unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        let bytes = std::fs::read(&path).unwrap();
        assert!(HistoryStore::open_existing_read_only(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let invalid = directory.path().join("invalid.sqlite3");
    std::fs::write(&invalid, b"not a database").unwrap();
    assert!(HistoryStore::open_existing_read_only(&invalid).is_err());
    assert_eq!(std::fs::read(invalid).unwrap(), b"not a database");
    assert!(HistoryStore::open_existing_read_only(directory.path()).is_err());
}

#[test]
fn missing_database_with_wal_sidecars_is_not_an_empty_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    for suffix in ["-wal", "-shm"] {
        let sidecar = directory.path().join(format!("history.sqlite3{suffix}"));
        std::fs::write(&sidecar, b"orphaned SQLite evidence").unwrap();
        assert!(matches!(
            HistoryStore::open_existing_read_only(&path),
            Err(HistoryError::Corrupt(_))
        ));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"orphaned SQLite evidence"
        );
        std::fs::remove_file(sidecar).unwrap();
    }
}

#[test]
fn scoped_pages_are_bounded_stable_and_include_terminal_pending_counts() {
    let store = HistoryStore::in_memory().unwrap();
    let project = ProjectId::new();
    let environment = EnvironmentId::new();
    let mut ids = Vec::new();
    for at in 1..=4 {
        let id = DeploymentId::new();
        store
            .create_deployment(&id, &project, &environment, at)
            .unwrap();
        ids.push(id);
    }
    let foreign = DeploymentId::new();
    store
        .create_deployment(&foreign, &ProjectId::new(), &environment, 5)
        .unwrap();
    store
        .transition_deployment(
            &ids[3],
            crate::domain::DeploymentState::Created,
            crate::domain::DeploymentState::Running,
            5,
        )
        .unwrap();
    store
        .record_intent(
            &ids[3],
            &crate::domain::ComponentName::parse("api").unwrap(),
            "activate",
            "v1",
            6,
        )
        .unwrap();
    store
        .transition_deployment(
            &ids[3],
            crate::domain::DeploymentState::Running,
            crate::domain::DeploymentState::Failed,
            7,
        )
        .unwrap();
    let (first, more) = store
        .read_deployment_page(
            &project,
            &environment,
            DeploymentQuery {
                limit: 2,
                ..DeploymentQuery::default()
            },
        )
        .unwrap();
    assert!(more);
    assert_eq!(
        first.iter().map(|row| &row.deployment).collect::<Vec<_>>(),
        [&ids[3], &ids[2]]
    );
    assert_eq!(first[0].pending_intent_count, 1);
    let (second, more) = store
        .read_deployment_page(
            &project,
            &environment,
            DeploymentQuery {
                limit: 2,
                offset: 2,
                nonterminal_only: false,
            },
        )
        .unwrap();
    assert!(!more);
    assert_eq!(
        second.iter().map(|row| &row.deployment).collect::<Vec<_>>(),
        [&ids[1], &ids[0]]
    );
    assert!(
        store
            .read_deployment_details(&project, &environment, &foreign)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .read_deployment_page(
                &project,
                &environment,
                DeploymentQuery {
                    limit: 101,
                    ..DeploymentQuery::default()
                }
            )
            .is_err()
    );
}

#[test]
fn oversized_details_ids_and_pending_rows_fail_closed() {
    let store = HistoryStore::in_memory().unwrap();
    let id = DeploymentId::new();
    let project = ProjectId::new();
    let environment = EnvironmentId::new();
    store
        .create_deployment(&id, &project, &environment, 1)
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO deployment_metadata VALUES (?1,?2)",
            params![id.to_string(), "x".repeat(4 * 1024 * 1024 + 1)],
        )
        .unwrap();
    assert!(
        store
            .read_deployment_details(&project, &environment, &id)
            .is_err()
    );
    store
        .connection
        .execute("DELETE FROM deployment_metadata", [])
        .unwrap();
    store
        .connection
        .execute_batch(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<4097)
        INSERT INTO operation_intents(deployment_id,component,stage,target,status,created_at_ms)
        SELECT (SELECT id FROM deployments),'api','activate','v1','pending',1 FROM seq",
        )
        .unwrap();
    assert!(
        store
            .read_deployment_details(&project, &environment, &id)
            .is_err()
    );
    store.connection.execute(
        "INSERT INTO deployments(id,project_id,environment_id,state,created_at_ms,updated_at_ms)
         VALUES (?1,?2,?3,'created',99,99)",
        params!["x".repeat(4096), project.to_string(), environment.to_string()],
    ).unwrap();
    assert!(
        store
            .read_deployment_page(&project, &environment, DeploymentQuery::default())
            .is_err()
    );
}
