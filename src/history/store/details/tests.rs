use super::super::{IntentStatus, MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4};
use super::*;
use crate::{
    domain::{
        Capability, ComponentGeneration, DestinationKey, DestinationRevision, DriverCapabilities,
        ReleaseVersion,
    },
    drivers::{DriverKind, EndpointFingerprint},
};

fn fixture(store: &HistoryStore) -> (DeploymentId, DeploymentComponentSnapshot) {
    let id = DeploymentId::new();
    let release = ReleaseRef {
        driver: DriverKind::parse("linux-ssh").unwrap(),
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("worker").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: ReleaseVersion::parse("v2").unwrap(),
        destination: DestinationKey::new(),
        destination_revision: DestinationRevision::INITIAL,
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        effective_capabilities: DriverCapabilities::new([
            Capability::Rollback,
            Capability::Observe,
        ]),
    };
    store
        .create_deployment(&id, &release.project_id, &release.environment_id, 1)
        .unwrap();
    let mut previous = release.clone();
    previous.version = ReleaseVersion::parse("v1").unwrap();
    (
        id,
        DeploymentComponentSnapshot {
            target_snapshot: None,
            release: release.clone(),
            expected_current: Some(previous),
            target: Some(release),
            execution_order: 0,
        },
    )
}

fn start(store: &HistoryStore, id: &DeploymentId) {
    store
        .transition_deployment(id, DeploymentState::Created, DeploymentState::Running, 2)
        .unwrap();
}
fn manifest(release: &ReleaseRef) -> ReleaseManifest {
    ReleaseManifest {
        schema_version: 1,
        project_id: release.project_id.clone(),
        environment_id: release.environment_id.clone(),
        component: release.component.clone(),
        generation: release.generation,
        version: release.version.clone(),
        created_at_unix: 42,
        source_revision: Some("abcdef123".into()),
    }
}

#[test]
fn snapshots_are_atomic_immutable_ordered_and_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let (id, first) = fixture(&store);
    let mut second = first.clone();
    second.release.component = ComponentName::parse("api").unwrap();
    second.expected_current = None;
    second.target = Some(second.release.clone());
    second.execution_order = 1;
    store
        .record_component_snapshots(&id, &[second.clone(), first.clone()])
        .unwrap();
    assert!(
        store
            .record_component_snapshots(&id, std::slice::from_ref(&first))
            .is_err()
    );
    drop(store);
    let reopened = HistoryStore::open(&path).unwrap();
    assert_eq!(
        reopened.component_snapshots(&id).unwrap(),
        vec![first.clone(), second]
    );
    start(&reopened, &id);
    reopened
        .record_intent(&id, &first.release.component, "build", "worker", 3)
        .unwrap();
    assert!(reopened.record_component_snapshots(&id, &[first]).is_err());
}

#[test]
fn snapshots_reject_wrong_context_gaps_duplicates_and_partial_writes() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    assert!(store.record_component_snapshots(&id, &[]).is_err());
    for field in 0..10 {
        let mut invalid = snapshot.clone();
        match field {
            0 => invalid.release.project_id = ProjectId::new(),
            1 => invalid.release.environment_id = EnvironmentId::new(),
            2 => {
                invalid.expected_current.as_mut().unwrap().generation =
                    ComponentGeneration::INITIAL.checked_next().unwrap();
            }
            3 => invalid.expected_current.as_mut().unwrap().destination = DestinationKey::new(),
            4 => {
                invalid
                    .expected_current
                    .as_mut()
                    .unwrap()
                    .destination_revision = DestinationRevision::INITIAL.checked_next().unwrap();
            }
            5 => {
                invalid
                    .expected_current
                    .as_mut()
                    .unwrap()
                    .endpoint_fingerprint = EndpointFingerprint::parse("b".repeat(64)).unwrap();
            }
            6 => {
                invalid.expected_current.as_mut().unwrap().driver =
                    DriverKind::parse("different-driver").unwrap();
            }
            7 => invalid.execution_order = 2,
            8 => invalid.target = None,
            _ => {
                invalid.expected_current.as_mut().unwrap().component =
                    ComponentName::parse("other").unwrap();
            }
        }
        assert!(
            store.record_component_snapshots(&id, &[invalid]).is_err(),
            "accepted field {field}"
        );
        assert!(store.component_snapshots(&id).unwrap().is_empty());
    }
    assert!(
        store
            .record_component_snapshots(&id, &[snapshot.clone(), snapshot.clone()])
            .is_err()
    );
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
}

#[test]
fn snapshot_insert_failure_rolls_back_header_and_earlier_components() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, first) = fixture(&store);
    let mut second = first.clone();
    second.release.component = ComponentName::parse("api").unwrap();
    second.expected_current = None;
    second.target = Some(second.release.clone());
    second.execution_order = 1;
    store.connection.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON component_snapshots WHEN NEW.component='api' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(
        store
            .record_component_snapshots(&id, &[first.clone(), second])
            .is_err()
    );
    assert!(store.component_snapshots(&id).unwrap().is_empty());
    store
        .connection
        .execute_batch("DROP TRIGGER fail_second")
        .unwrap();
    store.record_component_snapshots(&id, &[first]).unwrap();
}

#[test]
fn rollback_to_absence_uses_current_as_frozen_identity() {
    let store = HistoryStore::in_memory().unwrap();
    let (source, snapshot) = fixture(&store);
    start(&store, &source);
    store
        .transition_deployment(
            &source,
            DeploymentState::Running,
            DeploymentState::Succeeded,
            3,
        )
        .unwrap();
    let id = DeploymentId::new();
    store
        .create_rollback_deployment(
            &id,
            &source,
            &snapshot.release.project_id,
            &snapshot.release.environment_id,
            4,
        )
        .unwrap();
    let rollback = DeploymentComponentSnapshot {
        target_snapshot: None,
        release: snapshot.release.clone(),
        expected_current: Some(snapshot.release),
        target: None,
        execution_order: 0,
    };
    store
        .record_component_snapshots(&id, std::slice::from_ref(&rollback))
        .unwrap();
    assert_eq!(store.component_snapshots(&id).unwrap(), vec![rollback]);
}

#[test]
fn package_receipts_validate_manifest_digest_and_idempotence_without_paths() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    start(&store, &id);
    let release = &snapshot.release;
    let original = manifest(release);
    let digest = "a".repeat(64);
    for field in 0..9 {
        let mut invalid = original.clone();
        let mut invalid_digest = digest.clone();
        let mut size = 512;
        match field {
            0 => invalid.version = ReleaseVersion::parse("other").unwrap(),
            1 => invalid.generation = ComponentGeneration::INITIAL.checked_next().unwrap(),
            2 => invalid.schema_version = 2,
            3 => invalid.source_revision = Some("TOKEN\n".into()),
            4 => invalid_digest = "z".repeat(64),
            5 => size = 0,
            6 => size = u64::MAX,
            7 => invalid.project_id = ProjectId::new(),
            _ => invalid.source_revision = Some("a".repeat(65)),
        }
        assert!(
            store
                .record_release_package(&id, release, &invalid, &invalid_digest, size)
                .is_err()
        );
        assert!(store.release_packages(&id).unwrap().is_empty());
    }
    store
        .record_release_package(&id, release, &original, &digest, 512)
        .unwrap();
    store
        .record_release_package(&id, release, &original, &digest, 512)
        .unwrap();
    assert!(
        store
            .record_release_package(&id, release, &original, &digest, 513)
            .is_err()
    );
    let records = store.release_packages(&id).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].manifest, original);
    assert_eq!(records[0].release, *release);
    assert_eq!(records[0].size, 512);
    let columns: Vec<String> = store
        .connection
        .prepare("PRAGMA table_info(release_packages)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        columns,
        vec![
            "deployment_id",
            "component",
            "release_ref",
            "manifest",
            "sha256",
            "size"
        ]
    );
}

#[test]
fn observations_preserve_unknown_absence_health_and_redaction_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let (id, snapshot) = fixture(&store);
    let component = &snapshot.release.component;
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    let redactor = Redactor::new(["SECRET".into()]);
    store
        .record_observation(&id, component, "before", Ok(None), None, 2, &redactor)
        .unwrap();
    store
        .record_observation(
            &id,
            component,
            "after",
            Err(&format!("SECRET\n{}", "x".repeat(2048))),
            None,
            3,
            &redactor,
        )
        .unwrap();
    store
        .record_observation(
            &id,
            component,
            "receipt",
            Ok(Some(&snapshot.release)),
            Some(true),
            4,
            &redactor,
        )
        .unwrap();
    store
        .record_observation(
            &id,
            component,
            "stopped",
            Ok(None),
            Some(true),
            5,
            &redactor,
        )
        .unwrap();
    assert!(
        store
            .record_observation(
                &id,
                component,
                "invalid",
                Err("unknown"),
                Some(true),
                6,
                &redactor
            )
            .is_err()
    );
    assert!(
        store
            .record_observation(&id, component, "invalid", Ok(None), None, 0, &redactor)
            .is_err()
    );
    let mut other = snapshot.release.clone();
    other.destination = DestinationKey::new();
    assert!(
        store
            .record_observation(
                &id,
                component,
                "invalid",
                Ok(Some(&other)),
                None,
                6,
                &redactor
            )
            .is_err()
    );
    drop(store);
    let reopened = HistoryStore::open(&path).unwrap();
    let rows = reopened.observations(&id).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].observed, Ok(None));
    assert_eq!(rows[0].healthy, None);
    let sanitized_error = rows[1].observed.as_ref().unwrap_err();
    assert!(sanitized_error.starts_with("[REDACTED] "));
    assert!(sanitized_error.len() <= 1024);
    assert!(!sanitized_error.contains('\n'));
    assert_eq!(rows[2].observed, Ok(Some(snapshot.release)));
    assert_eq!(rows[2].healthy, Some(true));
    assert_eq!(
        (&rows[3].observed, rows[3].healthy),
        (&Ok(None), Some(true))
    );
    assert!(
        rows.windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
}

#[test]
fn prepared_receipt_is_not_a_current_observation_and_requires_frozen_target() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    start(&store, &id);
    assert!(
        store
            .record_release_receipt(
                &id,
                &snapshot.release.component,
                "prepare",
                snapshot.expected_current.as_ref().unwrap(),
                3
            )
            .is_err()
    );
    store
        .record_release_receipt(
            &id,
            &snapshot.release.component,
            "prepare",
            &snapshot.release,
            3,
        )
        .unwrap();
    assert!(
        store
            .record_release_receipt(
                &id,
                &snapshot.release.component,
                "prepare",
                &snapshot.release,
                3
            )
            .is_err()
    );
    assert!(store.observations(&id).unwrap().is_empty());
    assert_eq!(
        store.release_receipts(&id).unwrap()[0].release,
        snapshot.release
    );
}

#[test]
fn metadata_is_sanitized_immutable_and_not_invented_for_legacy_deployments() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    assert_eq!(store.deployment_metadata(&id).unwrap(), None);
    let metadata = DeploymentMetadata {
        git_branch: Some("SECRET\nmain".into()),
        git_revision: Some("abcdef123456".into()),
        git_worktree: GitWorktree::Dirty,
        operator: Some("SECRET\u{1b}operator".into()),
    };
    store
        .record_deployment_metadata(&id, &metadata, &Redactor::new(["SECRET".into()]))
        .unwrap();
    let saved = store.deployment_metadata(&id).unwrap().unwrap();
    assert_eq!(saved.git_branch.as_deref(), Some("[REDACTED] main"));
    assert_eq!(saved.operator.as_deref(), Some("[REDACTED] operator"));
    assert_eq!(saved.git_worktree, GitWorktree::Dirty);
    assert!(
        store
            .record_deployment_metadata(&id, &metadata, &Redactor::default())
            .is_err()
    );
    let (second, _) = fixture(&store);
    start(&store, &second);
    store
        .record_intent(&second, &snapshot.release.component, "build", "worker", 3)
        .unwrap();
    assert!(
        store
            .record_deployment_metadata(&second, &metadata, &Redactor::default())
            .is_err()
    );
}

#[test]
fn step_intents_complete_transactionally_and_pending_steps_are_skipped() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    let component = &snapshot.release.component;
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .plan_steps(
            &id,
            component,
            &["build", "prepare", "activate", "compensate"],
        )
        .unwrap();
    assert!(store.plan_steps(&id, component, &["other"]).is_err());
    assert_eq!(store.steps(&id).unwrap()[0].status, StepStatus::Pending);
    start(&store, &id);
    let build = store
        .record_intent(&id, component, "build", "worker", 3)
        .unwrap();
    assert_eq!(store.steps(&id).unwrap()[0].status, StepStatus::Running);
    assert!(
        store
            .record_intent(&id, component, "build", "worker", 3)
            .is_err()
    );
    assert_eq!(store.pending_intents(&id).unwrap().len(), 1);
    store
        .complete_intent(
            build,
            IntentStatus::Succeeded,
            None,
            4,
            &Redactor::default(),
        )
        .unwrap();
    let prepare = store
        .record_intent(&id, component, "prepare", "worker", 5)
        .unwrap();
    store
        .complete_intent(
            prepare,
            IntentStatus::Failed,
            Some("TOKEN failed"),
            6,
            &Redactor::new(["TOKEN".into()]),
        )
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Failed, 7)
        .unwrap();
    let rows = store.steps(&id).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.status).collect::<Vec<_>>(),
        vec![
            StepStatus::Succeeded,
            StepStatus::Failed,
            StepStatus::Skipped,
            StepStatus::Skipped
        ]
    );
    assert_eq!(rows[0].started_at_ms, Some(3));
    assert_eq!(rows[0].completed_at_ms, Some(4));
    assert_eq!(rows[1].error.as_deref(), Some("[REDACTED] failed"));
    assert_eq!(rows[2].started_at_ms, None);
    assert_eq!(rows[2].completed_at_ms, Some(7));
}

#[test]
fn step_write_failure_rolls_back_intent_and_terminal_update() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    let component = &snapshot.release.component;
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .plan_steps(&id, component, &["build", "activate"])
        .unwrap();
    start(&store, &id);
    store.connection.execute_batch("CREATE TRIGGER fail_step BEFORE UPDATE ON deployment_steps BEGIN SELECT RAISE(ABORT,'injected'); END").unwrap();
    assert!(
        store
            .record_intent(&id, component, "build", "worker", 3)
            .is_err()
    );
    assert!(store.pending_intents(&id).unwrap().is_empty());
    assert!(
        store
            .transition_deployment(&id, DeploymentState::Running, DeploymentState::Failed, 4)
            .is_err()
    );
    assert_eq!(
        store.deployment(&id).unwrap().unwrap().state,
        DeploymentState::Running
    );
    store
        .connection
        .execute_batch("DROP TRIGGER fail_step")
        .unwrap();
    let intent = store
        .record_intent(&id, component, "build", "worker", 3)
        .unwrap();
    store.connection.execute_batch("CREATE TRIGGER fail_step BEFORE UPDATE ON deployment_steps BEGIN SELECT RAISE(ABORT,'injected'); END").unwrap();
    assert!(
        store
            .complete_intent(
                intent,
                IntentStatus::Succeeded,
                None,
                4,
                &Redactor::default()
            )
            .is_err()
    );
    assert_eq!(store.pending_intents(&id).unwrap().len(), 1);
    assert_eq!(store.steps(&id).unwrap()[0].status, StepStatus::Running);
}

#[test]
fn unfinished_running_step_keeps_uncertainty_and_legacy_intents_remain_readable() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .plan_steps(&id, &snapshot.release.component, &["build", "prepare"])
        .unwrap();
    start(&store, &id);
    store
        .record_intent(&id, &snapshot.release.component, "build", "worker", 3)
        .unwrap();
    store
        .record_intent(
            &id,
            &snapshot.release.component,
            "legacy-extra",
            "worker",
            4,
        )
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Failed, 5)
        .unwrap();
    let steps = store.steps(&id).unwrap();
    assert_eq!(steps[0].status, StepStatus::Running);
    assert_eq!(steps[1].status, StepStatus::Skipped);
    assert_eq!(steps[2].status, StepStatus::Running);
    assert!(!steps[2].planned);
    assert_eq!(steps[2].started_at_ms, Some(4));
    assert_eq!(store.pending_intents(&id).unwrap().len(), 2);
}

#[test]
fn deployment_pages_are_scoped_bounded_and_deterministic() {
    let store = HistoryStore::in_memory().unwrap();
    let (first, snapshot) = fixture(&store);
    let project = &snapshot.release.project_id;
    let environment = &snapshot.release.environment_id;
    let second = DeploymentId::from_str("dep_zzzzzzzz").unwrap();
    store
        .create_deployment(&second, project, environment, 1)
        .unwrap();
    let newest = DeploymentId::new();
    store
        .create_deployment(&newest, project, environment, 5)
        .unwrap();
    store
        .create_deployment(&DeploymentId::new(), &ProjectId::new(), environment, 6)
        .unwrap();
    store
        .create_deployment(&DeploymentId::new(), project, &EnvironmentId::new(), 7)
        .unwrap();
    start(&store, &first);
    store
        .transition_deployment(
            &first,
            DeploymentState::Running,
            DeploymentState::Succeeded,
            3,
        )
        .unwrap();
    let query = DeploymentQuery {
        limit: 1,
        offset: 0,
        nonterminal_only: false,
    };
    assert_eq!(
        store.deployments(project, environment, query).unwrap()[0].deployment,
        newest
    );
    assert_eq!(
        store
            .deployments(project, environment, DeploymentQuery { offset: 1, ..query })
            .unwrap()[0]
            .deployment,
        second
    );
    assert_eq!(
        store
            .deployments(project, environment, DeploymentQuery { offset: 2, ..query })
            .unwrap()[0]
            .deployment,
        first
    );
    assert!(
        store
            .deployments(project, environment, DeploymentQuery { offset: 3, ..query })
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .deployments(
                project,
                environment,
                DeploymentQuery {
                    limit: 100,
                    nonterminal_only: true,
                    ..query
                }
            )
            .unwrap()
            .len(),
        2
    );
    for query in [
        DeploymentQuery { limit: 0, ..query },
        DeploymentQuery {
            limit: 101,
            ..query
        },
        DeploymentQuery {
            offset: 1_000_001,
            ..query
        },
    ] {
        assert!(matches!(
            store.deployments(project, environment, query),
            Err(HistoryError::InvalidPage)
        ));
    }
    assert!(store.deployment(&DeploymentId::new()).unwrap().is_none());
}

#[test]
fn all_previous_schema_versions_preserve_legacy_rows_without_inventing_context() {
    for version in 1..=4 {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let connection = Connection::open(&path).unwrap();
        for migration in [MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4]
            .into_iter()
            .take(version)
        {
            connection.execute_batch(migration).unwrap();
        }
        connection
            .pragma_update(None, "user_version", u32::try_from(version).unwrap())
            .unwrap();
        let id = DeploymentId::new();
        let project = ProjectId::new();
        let environment = EnvironmentId::new();
        connection.execute("INSERT INTO deployments(id,project_id,environment_id,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,'running',1,2)",params![id.to_string(),project.to_string(),environment.to_string()]).unwrap();
        connection.execute("INSERT INTO operation_intents(deployment_id,component,stage,target,status,created_at_ms) VALUES(?1,'worker','activate','worker','pending',3)",[id.to_string()]).unwrap();
        if version >= 2 {
            connection.execute("INSERT INTO component_results VALUES(?1,'worker','failed','v2',NULL,'legacy error')",[id.to_string()]).unwrap();
        }
        if version >= 4 {
            connection
                .execute(
                    "INSERT INTO deployment_logs VALUES(?1,?2,1024,3)",
                    params![id.to_string(), format!("logs/{id}.log")],
                )
                .unwrap();
        }
        drop(connection);
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 7);
        assert_eq!(store.pending_intents(&id).unwrap().len(), 1);
        assert_eq!(
            store.deployment(&id).unwrap().unwrap().state,
            DeploymentState::Running
        );
        assert!(store.component_snapshots(&id).unwrap().is_empty());
        assert!(store.observations(&id).unwrap().is_empty());
        assert!(store.release_packages(&id).unwrap().is_empty());
        assert!(store.deployment_metadata(&id).unwrap().is_none());
        let steps = store.steps(&id).unwrap();
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].planned);
        assert_eq!(steps[0].status, StepStatus::Running);
        if version >= 2 {
            assert_eq!(
                store.component_results(&id).unwrap()[0]
                    .result
                    .observed_release,
                None
            );
        }
        if version >= 4 {
            assert!(store.deployment_log(&id).unwrap().is_some());
        }
    }
}

#[test]
fn migration_failure_is_atomic_and_future_schema_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let connection = Connection::open(&path).unwrap();
    for migration in [MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4] {
        connection.execute_batch(migration).unwrap();
    }
    connection.pragma_update(None, "user_version", 4).unwrap();
    connection
        .execute_batch("CREATE TABLE release_packages(collision TEXT)")
        .unwrap();
    drop(connection);
    assert!(HistoryStore::open(&path).is_err());
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        4
    );
    let new_tables:u32=connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('deployment_metadata','deployment_snapshots','component_snapshots')",[],|row|row.get(0)).unwrap();
    assert_eq!(new_tables, 0);
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    assert!(matches!(
        HistoryStore::open(&path),
        Err(HistoryError::UnsupportedSchema(99))
    ));
}

#[test]
fn unresolved_query_includes_terminal_deployments_and_respects_scope_and_paging() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    let project = &snapshot.release.project_id;
    let environment = &snapshot.release.environment_id;
    start(&store, &id);
    let intent = store
        .record_intent(&id, &snapshot.release.component, "activate", "worker", 3)
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Failed, 4)
        .unwrap();
    let query = DeploymentQuery::default();
    let records = store
        .unresolved_deployments(project, environment, query)
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].deployment, id);
    assert_eq!(records[0].pending_intent_count, 1);
    assert_eq!(records[0].state, DeploymentState::Failed);
    assert!(
        store
            .unresolved_deployments(
                project,
                environment,
                DeploymentQuery {
                    nonterminal_only: true,
                    ..query
                }
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .unresolved_deployments(project, &EnvironmentId::new(), query)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .unresolved_deployments(&ProjectId::new(), environment, query)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .unresolved_deployments(project, environment, DeploymentQuery { offset: 1, ..query })
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .unresolved_deployments(project, environment, DeploymentQuery { limit: 0, ..query })
            .is_err()
    );
    store
        .complete_intent(
            intent,
            IntentStatus::Failed,
            Some("reconciled failure"),
            5,
            &Redactor::default(),
        )
        .unwrap();
    assert!(
        store
            .unresolved_deployments(project, environment, query)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.deployment(&id).unwrap().unwrap().pending_intent_count,
        0
    );
}

#[test]
fn frozen_selection_rejects_unselected_intents_and_results_without_partial_writes() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    start(&store, &id);
    let unselected = ComponentName::parse("not-selected").unwrap();
    assert!(
        store
            .record_intent(&id, &unselected, "build", "worker", 3)
            .is_err()
    );
    assert!(store.pending_intents(&id).unwrap().is_empty());
    let result = crate::domain::ComponentDeploymentResult {
        outcome: crate::domain::ComponentOutcome::Failed,
        attempted_release: None,
        observed_release: None,
    };
    assert!(
        store
            .record_component_result(&id, &unselected, &result, None, &Redactor::default())
            .is_err()
    );
    assert!(store.component_results(&id).unwrap().is_empty());
    assert!(
        store
            .record_intent(&id, &snapshot.release.component, "build", "worker", 0)
            .is_err()
    );
}

#[test]
fn damaged_snapshot_indices_and_invalid_json_fail_closed() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE deployment_snapshots SET component_count=2 WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.component_snapshots(&id),
        Err(HistoryError::Corrupt(_))
    ));
    store
        .connection
        .execute(
            "UPDATE deployment_snapshots SET component_count=1 WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE component_snapshots SET snapshot='invalid' WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.component_snapshots(&id),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn missing_step_timestamps_fail_sql_checks_and_legacy_read_validation() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .plan_steps(&id, &snapshot.release.component, &["build"])
        .unwrap();
    start(&store, &id);
    let intent = store
        .record_intent(&id, &snapshot.release.component, "build", "worker", 3)
        .unwrap();
    assert!(
        store
            .connection
            .execute(
                "UPDATE deployment_steps SET status='succeeded' WHERE deployment_id=?1",
                [id.to_string()]
            )
            .is_err()
    );
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE deployment_steps SET status='succeeded' WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(store.steps(&id), Err(HistoryError::Corrupt(_))));
    store
        .connection
        .execute(
            "DELETE FROM deployment_steps WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE operation_intents SET status='succeeded' WHERE id=?1",
            [intent.0],
        )
        .unwrap();
    assert!(matches!(store.steps(&id), Err(HistoryError::Corrupt(_))));
}

#[test]
fn tampered_observation_time_is_not_returned_as_valid_evidence() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .record_observation(
            &id,
            &snapshot.release.component,
            "current",
            Ok(None),
            None,
            2,
            &Redactor::default(),
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE deployment_observations SET observed_at_ms=0 WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.observations(&id),
        Err(HistoryError::Corrupt(_))
    ));
    start(&store, &id);
    store
        .record_release_receipt(
            &id,
            &snapshot.release.component,
            "prepare",
            &snapshot.release,
            2,
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE release_receipts SET recorded_at_ms=0 WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.release_receipts(&id),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn frozen_result_attempt_matches_target_and_read_rejects_tampering() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    start(&store, &id);
    let mut result = ComponentDeploymentResult {
        outcome: crate::domain::ComponentOutcome::Succeeded,
        attempted_release: None,
        observed_release: Some(ReleaseVersion::parse("drifted").unwrap()),
    };
    assert!(
        store
            .record_component_result(
                &id,
                &snapshot.release.component,
                &result,
                None,
                &Redactor::default()
            )
            .is_err()
    );
    result.attempted_release = Some(ReleaseVersion::parse("other").unwrap());
    assert!(
        store
            .record_component_result(
                &id,
                &snapshot.release.component,
                &result,
                None,
                &Redactor::default()
            )
            .is_err()
    );
    result.attempted_release = Some(snapshot.release.version.clone());
    store
        .record_component_result(
            &id,
            &snapshot.release.component,
            &result,
            None,
            &Redactor::default(),
        )
        .unwrap();
    assert_eq!(store.component_results(&id).unwrap()[0].result, result);
    store
        .connection
        .execute(
            "UPDATE component_results SET attempted_release='other' WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.component_results(&id),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn rollback_to_absence_result_requires_absent_attempted_release() {
    let store = HistoryStore::in_memory().unwrap();
    let (source, snapshot) = fixture(&store);
    start(&store, &source);
    store
        .transition_deployment(
            &source,
            DeploymentState::Running,
            DeploymentState::Succeeded,
            3,
        )
        .unwrap();
    let id = DeploymentId::new();
    store
        .create_rollback_deployment(
            &id,
            &source,
            &snapshot.release.project_id,
            &snapshot.release.environment_id,
            4,
        )
        .unwrap();
    let rollback = DeploymentComponentSnapshot {
        target_snapshot: None,
        release: snapshot.release.clone(),
        expected_current: Some(snapshot.release.clone()),
        target: None,
        execution_order: 0,
    };
    store.record_component_snapshots(&id, &[rollback]).unwrap();
    store
        .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 5)
        .unwrap();
    let mut result = ComponentDeploymentResult {
        outcome: crate::domain::ComponentOutcome::Succeeded,
        attempted_release: Some(snapshot.release.version),
        observed_release: None,
    };
    assert!(
        store
            .record_component_result(
                &id,
                &snapshot.release.component,
                &result,
                None,
                &Redactor::default()
            )
            .is_err()
    );
    result.attempted_release = None;
    store
        .record_component_result(
            &id,
            &snapshot.release.component,
            &result,
            None,
            &Redactor::default(),
        )
        .unwrap();
    assert_eq!(store.component_results(&id).unwrap()[0].result, result);
}

#[test]
fn mismatched_step_intent_link_is_rejected_on_read_and_before_completion() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, snapshot) = fixture(&store);
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .plan_steps(&id, &snapshot.release.component, &["build"])
        .unwrap();
    start(&store, &id);
    let build = store
        .record_intent(&id, &snapshot.release.component, "build", "worker", 3)
        .unwrap();
    let other = store
        .record_intent(&id, &snapshot.release.component, "other", "worker", 3)
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE deployment_steps SET intent_id=?2 WHERE deployment_id=?1",
            params![id.to_string(), other.0],
        )
        .unwrap();
    assert!(matches!(store.steps(&id), Err(HistoryError::Corrupt(_))));
    assert!(matches!(
        store.complete_intent(
            other,
            IntentStatus::Succeeded,
            None,
            4,
            &Redactor::default()
        ),
        Err(HistoryError::Corrupt(_))
    ));
    assert_eq!(store.pending_intents(&id).unwrap().len(), 2);
    store
        .connection
        .execute(
            "UPDATE deployment_steps SET intent_id=?2 WHERE deployment_id=?1",
            params![id.to_string(), build.0],
        )
        .unwrap();
    assert_eq!(store.steps(&id).unwrap()[0].intent, Some(build));
    store
        .connection
        .execute(
            "UPDATE deployment_steps SET started_at_ms=4 WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(matches!(store.steps(&id), Err(HistoryError::Corrupt(_))));
}
