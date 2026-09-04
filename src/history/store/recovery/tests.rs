use super::*;
use crate::{
    domain::{DeploymentState, DriverCapabilities, ReleaseVersion},
    drivers::{
        ReleaseInventory, RemoteAuditHistory,
        inventory::{InventoryRelease, TemporaryRemnants},
    },
};

fn report() -> RecoveryReport {
    RecoveryReport {
        id: uuid::Uuid::now_v7(),
        related_deployment: None,
        source_revision: None,
        started_at_ms: 10,
        completed_at_ms: 20,
        components: vec![RecoveryComponentReport {
            scope: InspectionScope {
                project: ProjectId::new(),
                environment: EnvironmentId::new(),
                component: ComponentName::parse("api").unwrap(),
                generation: ComponentGeneration::INITIAL,
                driver: DriverKind::parse("linux-ssh").unwrap(),
                destination: DestinationKey::new(),
                destination_revision: DestinationRevision::INITIAL,
                endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            },
            inventory: Ok(ComponentInventory {
                releases: ReleaseInventory {
                    releases: Vec::new(),
                    issues: Vec::new(),
                    current: Ok(None),
                    notices: Vec::new(),
                },
                audit: RemoteAuditHistory::default(),
                remnants: TemporaryRemnants::default(),
            }),
            alignment: CurrentAlignment::Unplanned,
            package_alignment: PackageAlignment::Unplanned,
            notices: Vec::new(),
        }],
    }
}

fn source(store: &HistoryStore) -> (RecoveryReport, DeploymentComponentSnapshot) {
    let mut report = report();
    let scope = &report.components[0].scope;
    let id = DeploymentId::new();
    store
        .create_deployment(&id, &scope.project, &scope.environment, 1)
        .unwrap();
    let release = ReleaseRef {
        driver: scope.driver.clone(),
        project_id: scope.project.clone(),
        environment_id: scope.environment.clone(),
        component: scope.component.clone(),
        generation: scope.generation,
        version: ReleaseVersion::parse("v2").unwrap(),
        destination: scope.destination.clone(),
        destination_revision: scope.destination_revision,
        endpoint_fingerprint: scope.endpoint_fingerprint.clone(),
        effective_capabilities: DriverCapabilities::default(),
    };
    let mut previous = release.clone();
    previous.version = ReleaseVersion::parse("v1").unwrap();
    let snapshot = DeploymentComponentSnapshot {
        release: release.clone(),
        expected_current: Some(previous),
        target: Some(release),
        execution_order: 0,
    };
    store
        .record_component_snapshots(&id, std::slice::from_ref(&snapshot))
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 2)
        .unwrap();
    report.related_deployment = Some(id.clone());
    report.source_revision = Some(store.recovery_basis(&id).unwrap().revision);
    (report, snapshot)
}

fn archive(scope: &InspectionScope) -> InventoryRelease {
    InventoryRelease {
        manifest: ReleaseManifest {
            schema_version: 1,
            project_id: scope.project.clone(),
            environment_id: scope.environment.clone(),
            component: scope.component.clone(),
            generation: scope.generation,
            version: ReleaseVersion::parse("v2").unwrap(),
            created_at_unix: 1,
            source_revision: Some("abcdef123".into()),
        },
        sha256: "c".repeat(64),
        size: 123,
        extracted: true,
    }
}

#[test]
fn empty_database_rebuilds_only_cache_and_reopens_latest_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let first = report();
    store
        .append_recovery_report(&first, &Redactor::default())
        .unwrap();
    let mut unknown = first.clone();
    unknown.id = uuid::Uuid::now_v7();
    // A backwards wall clock must not resurrect the previous successful cache.
    unknown.started_at_ms = 2;
    unknown.completed_at_ms = 3;
    unknown.components[0].inventory = Err("TOKEN\nconnection lost".into());
    unknown.components[0].alignment = CurrentAlignment::Unknown;
    unknown.components[0].package_alignment = PackageAlignment::Unknown;
    store
        .append_recovery_report(&unknown, &Redactor::new(["TOKEN".into()]))
        .unwrap();
    drop(store);
    let reopened = HistoryStore::open(&path).unwrap();
    let cached = reopened
        .latest_recovery_report(&first.components[0].scope)
        .unwrap()
        .unwrap();
    assert_eq!(cached.id, unknown.id);
    assert_eq!(
        cached.components[0].inventory,
        Err("[REDACTED] connection lost".into())
    );
    assert_eq!(reopened.recovery_report(&first.id).unwrap(), Some(first));
    for table in [
        "deployments",
        "operation_intents",
        "component_results",
        "deployment_logs",
        "deployment_revisions",
    ] {
        assert_eq!(
            reopened
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn reports_do_not_change_source_or_complete_pending_intents_and_cas_rejects_changes() {
    let store = HistoryStore::in_memory().unwrap();
    let (mut report, snapshot) = source(&store);
    let id = report.related_deployment.clone().unwrap();
    let intent = store
        .record_intent(&id, &snapshot.release.component, "activate", "api", 3)
        .unwrap();
    let basis = store.recovery_basis(&id).unwrap();
    assert!(matches!(
        store.append_recovery_report(&report, &Redactor::default()),
        Err(HistoryError::StaleRecoveryBasis)
    ));
    report.source_revision = Some(basis.revision);
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    assert_eq!(store.recovery_basis(&id).unwrap(), basis);
    store
        .complete_intent(
            intent,
            super::super::IntentStatus::Succeeded,
            None,
            4,
            &Redactor::default(),
        )
        .unwrap();
    report.id = uuid::Uuid::now_v7();
    assert!(matches!(
        store.append_recovery_report(&report, &Redactor::default()),
        Err(HistoryError::StaleRecoveryBasis)
    ));
    assert_eq!(
        store
            .recovery_reports(
                &basis.record.project,
                &basis.record.environment,
                RecoveryQuery::default()
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn reports_are_atomic_immutable_and_require_matching_source_scope() {
    let store = HistoryStore::in_memory().unwrap();
    let (report, _) = source(&store);
    let mut invalid = report.clone();
    invalid.components[0].scope.destination_revision =
        DestinationRevision::INITIAL.checked_next().unwrap();
    assert!(
        store
            .append_recovery_report(&invalid, &Redactor::default())
            .is_err()
    );
    store.connection.execute_batch("CREATE TRIGGER reject_recovery_scope BEFORE INSERT ON recovery_report_components BEGIN SELECT RAISE(ABORT,'test'); END;").unwrap();
    assert!(
        store
            .append_recovery_report(&report, &Redactor::default())
            .is_err()
    );
    assert!(store.recovery_report(&report.id).unwrap().is_none());
    store
        .connection
        .execute_batch("DROP TRIGGER reject_recovery_scope;")
        .unwrap();
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    assert!(
        store
            .append_recovery_report(&report, &Redactor::default())
            .is_err()
    );
    assert!(
        store
            .connection
            .execute("DELETE FROM recovery_reports", [])
            .is_err()
    );
    assert!(
        store
            .connection
            .execute("UPDATE recovery_reports SET completed_at_ms=21", [])
            .is_err()
    );
    assert!(
        store
            .connection
            .execute("UPDATE recovery_report_components SET scope='{}'", [])
            .is_err()
    );
}

#[test]
fn package_and_current_alignment_must_follow_actual_evidence() {
    let store = HistoryStore::in_memory().unwrap();
    let (mut report, snapshot) = source(&store);
    let id = report.related_deployment.clone().unwrap();
    let package = archive(&report.components[0].scope);
    store
        .record_release_package(
            &id,
            &snapshot.release,
            &package.manifest,
            &package.sha256,
            package.size,
        )
        .unwrap();
    report.source_revision = Some(store.recovery_basis(&id).unwrap().revision);
    let component = &mut report.components[0];
    component
        .inventory
        .as_mut()
        .unwrap()
        .releases
        .releases
        .push(package);
    component.inventory.as_mut().unwrap().releases.current =
        Ok(Some(snapshot.release.version.clone()));
    component.alignment = CurrentAlignment::Target;
    component.package_alignment = PackageAlignment::Matches;
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    for failure in 0..4 {
        let mut invalid = report.clone();
        invalid.id = uuid::Uuid::now_v7();
        let component = &mut invalid.components[0];
        match failure {
            0 => component.alignment = CurrentAlignment::Previous,
            1 => component.inventory.as_mut().unwrap().releases.releases[0].sha256 = "d".repeat(64),
            2 => component.inventory.as_mut().unwrap().releases.releases[0].extracted = false,
            _ => component.inventory.as_mut().unwrap().releases.current = Err("unknown".into()),
        }
        assert!(
            store
                .append_recovery_report(&invalid, &Redactor::default())
                .is_err()
        );
    }
}

#[test]
fn local_attention_never_creates_history_and_includes_terminal_pending_work() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("not-created/history.sqlite3");
    let attention = HistoryStore::local_attention(&missing, None, None).unwrap();
    assert!(attention.database_missing);
    assert!(attention.candidates.is_empty());
    assert!(!missing.parent().unwrap().exists());
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    assert!(
        !HistoryStore::local_attention(&path, None, None)
            .unwrap()
            .database_missing
    );
    let (report, snapshot) = source(&store);
    let id = report.related_deployment.unwrap();
    store
        .record_intent(&id, &snapshot.release.component, "activate", "api", 3)
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Running, DeploymentState::Failed, 4)
        .unwrap();
    let unfinished = DeploymentId::new();
    let scope = report.components[0].scope.clone();
    store
        .create_deployment(&unfinished, &scope.project, &scope.environment, 5)
        .unwrap();
    let records =
        HistoryStore::local_attention(&path, Some(&scope.project), Some(&scope.environment))
            .unwrap();
    assert_eq!(records.candidates.len(), 2);
    assert_eq!(records.candidates[0].deployment, unfinished);
    assert_eq!(records.candidates[1].state, DeploymentState::Failed);
    assert_eq!(records.candidates[1].pending_intent_count, 1);
    assert!(
        HistoryStore::local_attention(&path, Some(&ProjectId::new()), None)
            .unwrap()
            .candidates
            .is_empty()
    );
    assert_eq!(
        store
            .recovery_candidates(&scope.project, &scope.environment, RecoveryQuery::default())
            .unwrap(),
        records.candidates
    );
}

#[test]
fn attention_and_candidate_pagination_are_bounded_and_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let scope = report().components.remove(0).scope;
    for index in 0..102 {
        store
            .create_deployment(
                &format!("dep_{index:08}").parse().unwrap(),
                &scope.project,
                &scope.environment,
                1,
            )
            .unwrap();
    }
    let attention = HistoryStore::local_attention(&path, None, None).unwrap();
    assert_eq!(attention.candidates.len(), 100);
    assert!(attention.more);
    assert_eq!(
        attention.candidates[0].deployment.to_string(),
        "dep_00000101"
    );
    let page = store
        .recovery_candidates(
            &scope.project,
            &scope.environment,
            RecoveryQuery {
                limit: 2,
                offset: 100,
            },
        )
        .unwrap();
    assert_eq!(page[0].deployment.to_string(), "dep_00000001");
    assert_eq!(page[1].deployment.to_string(), "dep_00000000");
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
        assert!(
            store
                .recovery_candidates(&scope.project, &scope.environment, query)
                .is_err()
        );
        assert!(
            store
                .recovery_reports(&scope.project, &scope.environment, query)
                .is_err()
        );
    }
}

fn legacy(path: &std::path::Path, version: u32) -> (DeploymentId, ProjectId, EnvironmentId) {
    let connection = rusqlite::Connection::open(path).unwrap();
    for migration in [
        super::super::MIGRATION_1,
        super::super::MIGRATION_2,
        super::super::MIGRATION_3,
        super::super::MIGRATION_4,
        super::super::details::MIGRATION_5,
    ]
    .iter()
    .take(version as usize)
    {
        connection.execute_batch(migration).unwrap();
    }
    connection
        .pragma_update(None, "user_version", version)
        .unwrap();
    let id = DeploymentId::new();
    let project = ProjectId::new();
    let environment = EnvironmentId::new();
    connection.execute("INSERT INTO deployments(id,project_id,environment_id,state,created_at_ms,updated_at_ms) VALUES (?1,?2,?3,'running',1,2)",
        rusqlite::params![id.to_string(),project.to_string(),environment.to_string()]).unwrap();
    connection.execute("INSERT INTO operation_intents(deployment_id,component,stage,target,status,created_at_ms) VALUES (?1,'api','activate','api','pending',3)",[id.to_string()]).unwrap();
    (id, project, environment)
}

#[test]
fn all_legacy_versions_are_read_without_migration_then_migrate_without_invention() {
    for version in 1..=5 {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let (id, project, environment) = legacy(&path, version);
        let before = std::fs::read(&path).unwrap();
        let attention = HistoryStore::local_attention(&path, None, None).unwrap();
        assert_eq!(attention.candidates[0].deployment, id);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 7);
        let basis = store.recovery_basis(&id).unwrap();
        assert_eq!(basis.record.project, project);
        assert_eq!(basis.record.environment, environment);
        assert_eq!(basis.revision, 0);
        assert_eq!(basis.intents.len(), 1);
        assert!(basis.snapshots.is_empty());
        assert!(basis.packages.is_empty());
        assert!(basis.observations.is_empty());
        assert!(
            store
                .recovery_reports(&project, &environment, RecoveryQuery::default())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn attention_accepts_current_log_format_schema_without_rewriting_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let id = DeploymentId::new();
    let project = ProjectId::new();
    let environment = EnvironmentId::new();
    store
        .create_deployment(&id, &project, &environment, 1)
        .unwrap();
    store.register_event_log(&id, 1024, 3).unwrap();
    drop(store);
    let before = std::fs::read(&path).unwrap();
    let attention =
        HistoryStore::local_attention(&path, Some(&project), Some(&environment)).unwrap();
    assert_eq!(attention.candidates.len(), 1);
    assert_eq!(attention.candidates[0].deployment, id);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        connection
            .query_row("SELECT format FROM deployment_logs", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "jsonl_v1"
    );
}

#[test]
fn failed_v6_migration_rolls_back_and_future_or_invalid_attention_is_not_empty() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let (id, _, _) = legacy(&path, 5);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE recovery_reports(collision TEXT);")
        .unwrap();
    assert!(HistoryStore::open(&path).is_err());
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        5
    );
    assert!(
        connection
            .prepare("SELECT * FROM deployment_revisions")
            .is_err()
    );
    assert_eq!(
        connection
            .query_row("SELECT id FROM deployments", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        id.to_string()
    );
    connection.pragma_update(None, "user_version", 99).unwrap();
    assert!(matches!(
        HistoryStore::open(&path),
        Err(HistoryError::UnsupportedSchema(99))
    ));
    assert!(matches!(
        HistoryStore::local_attention(&path, None, None),
        Err(HistoryError::UnsupportedSchema(99))
    ));
    connection.pragma_update(None, "user_version", 0).unwrap();
    assert!(HistoryStore::local_attention(&path, None, None).is_err());
}

#[test]
fn query_scopes_use_full_endpoint_identity_and_insertion_order() {
    let store = HistoryStore::in_memory().unwrap();
    let mut first = report();
    store
        .append_recovery_report(&first, &Redactor::default())
        .unwrap();
    let scope = first.components[0].scope.clone();
    let first_id = first.id;
    first.id = uuid::Uuid::now_v7();
    first.started_at_ms = 1;
    first.completed_at_ms = 2;
    store
        .append_recovery_report(&first, &Redactor::default())
        .unwrap();
    let page = store
        .recovery_reports(
            &scope.project,
            &scope.environment,
            RecoveryQuery {
                limit: 1,
                offset: 0,
            },
        )
        .unwrap();
    assert_eq!(page[0].id, first.id);
    assert_eq!(
        store
            .recovery_reports(
                &scope.project,
                &scope.environment,
                RecoveryQuery {
                    limit: 1,
                    offset: 1
                }
            )
            .unwrap()[0]
            .id,
        first_id
    );
    assert!(
        store
            .recovery_reports(
                &ProjectId::new(),
                &scope.environment,
                RecoveryQuery::default()
            )
            .unwrap()
            .is_empty()
    );
    for changed in 0..4 {
        let mut incompatible = scope.clone();
        match changed {
            0 => incompatible.generation = ComponentGeneration::INITIAL.checked_next().unwrap(),
            1 => {
                incompatible.destination_revision =
                    DestinationRevision::INITIAL.checked_next().unwrap();
            }
            2 => incompatible.destination = DestinationKey::new(),
            _ => {
                incompatible.endpoint_fingerprint =
                    EndpointFingerprint::parse("f".repeat(64)).unwrap();
            }
        }
        assert!(
            store
                .latest_recovery_report(&incompatible)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn malformed_payload_indexes_and_oversized_source_are_rejected_on_read() {
    let store = HistoryStore::in_memory().unwrap();
    let report = report();
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    store
        .connection
        .execute_batch("DROP TRIGGER recovery_reports_immutable_update;")
        .unwrap();
    store
        .connection
        .execute("UPDATE recovery_reports SET report='{}'", [])
        .unwrap();
    assert!(store.recovery_report(&report.id).is_err());
    store
        .connection
        .execute(
            "UPDATE recovery_reports SET report=?1,project_id=?2",
            rusqlite::params![encode(&report).unwrap(), ProjectId::new().to_string()],
        )
        .unwrap();
    assert!(store.recovery_report(&report.id).is_err());
    let (source, snapshot) = source(&store);
    let id = source.related_deployment.unwrap();
    store
        .connection
        .execute(
            "UPDATE component_snapshots SET snapshot=?1 WHERE deployment_id=?2",
            rusqlite::params!["x".repeat(MAX_REPORT_BYTES + 1), id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.recovery_basis(&id),
        Err(HistoryError::InvalidMetadata(_))
    ));
    assert_eq!(snapshot.release.component.as_str(), "api");
}

#[test]
fn report_validation_rejects_bounds_duplicates_manifest_mismatch_and_fabricated_alignment() {
    let mut valid = report();
    let package = archive(&valid.components[0].scope);
    valid.components[0]
        .inventory
        .as_mut()
        .unwrap()
        .releases
        .releases
        .push(package);
    valid.validate().unwrap();
    for failure in 0..9 {
        let mut invalid = valid.clone();
        match failure {
            0 => invalid.id = uuid::Uuid::nil(),
            1 => invalid.components.push(invalid.components[0].clone()),
            2 => invalid.components[0].alignment = CurrentAlignment::Target,
            3 => {
                invalid.components[0]
                    .inventory
                    .as_mut()
                    .unwrap()
                    .releases
                    .releases[0]
                    .manifest
                    .project_id = ProjectId::new();
            }
            4 => {
                invalid.components[0]
                    .inventory
                    .as_mut()
                    .unwrap()
                    .releases
                    .releases[0]
                    .sha256 = "not-a-digest".into();
            }
            5 => {
                invalid.components[0]
                    .inventory
                    .as_mut()
                    .unwrap()
                    .releases
                    .releases[0]
                    .manifest
                    .source_revision = Some("secret".into());
            }
            6 => invalid.components[0].notices = vec!["bounded".into(); 33],
            7 => invalid.completed_at_ms = 1,
            _ => {
                let inventory = invalid.components[0].inventory.as_mut().unwrap();
                inventory
                    .releases
                    .releases
                    .push(inventory.releases.releases[0].clone());
            }
        }
        assert!(invalid.validate().is_err(), "case {failure}");
    }
}

#[test]
fn diagnostics_are_redacted_recursively_but_unknown_does_not_become_absence() {
    let mut report = report();
    let component = &mut report.components[0];
    component.notices.push("TOKEN\rstatus".into());
    let inventory = component.inventory.as_mut().unwrap();
    inventory.releases.current = Err("TOKEN\x1b[2J".into());
    inventory.releases.notices.push("TOKEN".into());
    inventory
        .releases
        .issues
        .push(crate::drivers::inventory::InventoryIssue {
            version: None,
            message: "TOKEN".into(),
        });
    inventory.audit.notices.push("TOKEN".into());
    inventory.remnants.notices.push("TOKEN".into());
    report.sanitize(&Redactor::new(["TOKEN".into()]));
    report.validate().unwrap();
    let json = encode(&report).unwrap();
    assert!(!json.contains("TOKEN"));
    assert!(!json.contains("\\u001b"));
    assert!(
        report.components[0]
            .inventory
            .as_ref()
            .unwrap()
            .releases
            .current
            .is_err()
    );
}

#[test]
fn historical_audit_keeps_original_endpoint_and_health_without_current_health_claim() {
    use crate::drivers::audit::{
        RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPhase, RemoteAuditRecord,
    };
    let store = HistoryStore::in_memory().unwrap();
    let (mut report, snapshot) = source(&store);
    report.related_deployment = None;
    report.source_revision = None;
    let mut old_release = snapshot.release.clone();
    old_release.destination = DestinationKey::new();
    old_release.destination_revision = DestinationRevision::INITIAL.checked_next().unwrap();
    old_release.endpoint_fingerprint = EndpointFingerprint::parse("b".repeat(64)).unwrap();
    let audit = RemoteAuditRecord {
        schema_version: 1,
        event_id: uuid::Uuid::now_v7(),
        deployment: DeploymentId::new(),
        recorded_at_ms: 1,
        release: old_release.clone(),
        phase: RemoteAuditPhase::Activate,
        outcome: RemoteAuditOutcome::Succeeded,
        expected_current: None,
        target: Some(old_release.version.clone()),
        observed: RemoteAuditObserved::Release(old_release.version.clone()),
        healthy: Some(true),
        package: None,
    };
    report.components[0]
        .inventory
        .as_mut()
        .unwrap()
        .audit
        .records
        .push(audit);
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    let loaded = store.recovery_report(&report.id).unwrap().unwrap();
    let inventory = loaded.components[0].inventory.as_ref().unwrap();
    assert_eq!(inventory.audit.records[0].release, old_release);
    assert_eq!(inventory.audit.records[0].healthy, Some(true));
    assert_eq!(inventory.releases.current, Ok(None));
    assert_eq!(loaded.components[0].alignment, CurrentAlignment::Unplanned);
    report.components[0]
        .inventory
        .as_mut()
        .unwrap()
        .audit
        .records[0]
        .release
        .generation = ComponentGeneration::INITIAL.checked_next().unwrap();
    assert!(report.validate().is_err());
}

fn populated_source() -> (HistoryStore, DeploymentId, ComponentName) {
    use crate::{
        domain::{ComponentDeploymentResult, ComponentOutcome},
        history::{DeploymentMetadata, GitWorktree, IntentStatus},
    };
    let store = HistoryStore::in_memory().unwrap();
    let (report, snapshot) = source(&store);
    let id = report.related_deployment.unwrap();
    let component = &snapshot.release.component;
    store
        .record_deployment_metadata(
            &id,
            &DeploymentMetadata {
                git_branch: None,
                git_revision: None,
                git_worktree: GitWorktree::Unknown,
                operator: None,
            },
            &Redactor::default(),
        )
        .unwrap();
    store.register_deployment_log(&id, 4096, 1).unwrap();
    store.plan_steps(&id, component, &["prepare"]).unwrap();
    let package = archive(&report.components[0].scope);
    store
        .record_release_package(
            &id,
            &snapshot.release,
            &package.manifest,
            &package.sha256,
            package.size,
        )
        .unwrap();
    let intent = store
        .record_intent(&id, component, "prepare", "api", 3)
        .unwrap();
    store
        .record_observation(
            &id,
            component,
            "preflight",
            Ok(None),
            None,
            3,
            &Redactor::default(),
        )
        .unwrap();
    store
        .record_release_receipt(&id, component, "prepare", &snapshot.release, 4)
        .unwrap();
    store
        .record_component_result(
            &id,
            component,
            &ComponentDeploymentResult {
                outcome: ComponentOutcome::Succeeded,
                attempted_release: Some(snapshot.release.version.clone()),
                observed_release: Some(snapshot.release.version.clone()),
            },
            None,
            &Redactor::default(),
        )
        .unwrap();
    store
        .complete_intent(
            intent,
            IntentStatus::Succeeded,
            None,
            4,
            &Redactor::default(),
        )
        .unwrap();
    (store, id, component.clone())
}

#[test]
fn revision_triggers_cover_every_source_table_and_failed_writes_roll_back_revision() {
    let (store, id, component) = populated_source();
    for (table, column) in [
        ("deployments", "id"),
        ("operation_intents", "deployment_id"),
        ("component_results", "deployment_id"),
        ("deployment_logs", "deployment_id"),
        ("deployment_metadata", "deployment_id"),
        ("deployment_snapshots", "deployment_id"),
        ("component_snapshots", "deployment_id"),
        ("release_packages", "deployment_id"),
        ("deployment_observations", "deployment_id"),
        ("release_receipts", "deployment_id"),
        ("deployment_steps", "deployment_id"),
    ] {
        let revision = store.recovery_basis(&id).unwrap().revision;
        assert_eq!(
            store
                .connection
                .execute(
                    &format!("UPDATE {table} SET {column}={column} WHERE {column}=?1"),
                    [id.to_string()]
                )
                .unwrap(),
            1
        );
        assert!(
            store.recovery_basis(&id).unwrap().revision > revision,
            "{table}"
        );
    }
    let before = store.recovery_basis(&id).unwrap();
    store.connection.execute_batch("CREATE TRIGGER reject_observation BEFORE INSERT ON deployment_observations BEGIN SELECT RAISE(ABORT,'test'); END;").unwrap();
    assert!(
        store
            .record_observation(
                &id,
                &component,
                "after",
                Ok(None),
                None,
                5,
                &Redactor::default()
            )
            .is_err()
    );
    assert_eq!(store.recovery_basis(&id).unwrap(), before);
}

#[test]
fn corrupt_oversized_component_indexes_are_never_loaded_as_valid_cache() {
    let store = HistoryStore::in_memory().unwrap();
    let report = report();
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    store.connection.execute_batch("DROP TRIGGER recovery_components_immutable_update; PRAGMA ignore_check_constraints=ON;").unwrap();
    store
        .connection
        .execute(
            "UPDATE recovery_report_components SET scope=?1",
            ["x".repeat(4097)],
        )
        .unwrap();
    assert!(matches!(
        store.recovery_report(&report.id),
        Err(HistoryError::Corrupt(_))
    ));
}

#[test]
fn reports_and_pages_refuse_excessive_bytes_without_silent_truncation() {
    let store = HistoryStore::in_memory().unwrap();
    let mut large = report();
    large.components[0]
        .inventory
        .as_mut()
        .unwrap()
        .releases
        .issues = vec![
        crate::drivers::inventory::InventoryIssue {
            version: None,
            message: "x".repeat(1024)
        };
        1024
    ];
    large.validate().unwrap();
    let mut oversized = large.clone();
    for name in ["worker", "frontend", "cron"] {
        let mut component = large.components[0].clone();
        component.scope.component = ComponentName::parse(name).unwrap();
        oversized.components.push(component);
    }
    assert!(oversized.validate().is_err());
    let scope = large.components[0].scope.clone();
    for _ in 0..8 {
        large.id = uuid::Uuid::now_v7();
        store
            .append_recovery_report(&large, &Redactor::default())
            .unwrap();
    }
    assert!(
        store
            .recovery_reports(&scope.project, &scope.environment, RecoveryQuery::default())
            .is_err()
    );
    assert_eq!(
        store
            .recovery_reports(
                &scope.project,
                &scope.environment,
                RecoveryQuery {
                    limit: 1,
                    offset: 0
                }
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn attention_rejects_incoherent_deployment_kinds_and_oversized_identities() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let (report, _) = source(&store);
    let id = report.related_deployment.unwrap();
    let scope = &report.components[0].scope;
    for query in [
        "UPDATE deployments SET kind='rollback',related_deployment_id=NULL",
        "UPDATE deployments SET kind='deploy',related_deployment_id=id",
        "UPDATE deployments SET kind='deploy',related_deployment_id=NULL,project_id=printf('%100000s','x')",
    ] {
        store.connection.execute(query, []).unwrap();
        assert!(HistoryStore::local_attention(&path, None, None).is_err());
        assert!(store.recovery_basis(&id).is_err());
    }
    store
        .connection
        .execute(
            "UPDATE deployments SET project_id=?1,kind='rollback'",
            [scope.project.to_string()],
        )
        .unwrap();
    assert!(
        store
            .recovery_candidates(&scope.project, &scope.environment, RecoveryQuery::default())
            .is_err()
    );
}

#[test]
fn paged_and_latest_queries_reject_oversized_report_ids_without_old_cache_fallback() {
    let store = HistoryStore::in_memory().unwrap();
    let mut report = report();
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    report.id = uuid::Uuid::now_v7();
    store
        .append_recovery_report(&report, &Redactor::default())
        .unwrap();
    store
        .connection
        .execute_batch(
            "DROP TRIGGER recovery_reports_immutable_update;
         DROP TRIGGER recovery_components_immutable_update;
         PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;",
        )
        .unwrap();
    let oversized = "x".repeat(100_000);
    store
        .connection
        .execute(
            "UPDATE recovery_reports SET id=?1 WHERE id=?2",
            rusqlite::params![oversized, report.id.to_string()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE recovery_report_components SET report_id=?1 WHERE report_id=?2",
            rusqlite::params![oversized, report.id.to_string()],
        )
        .unwrap();
    let scope = &report.components[0].scope;
    assert!(matches!(
        store.recovery_reports(&scope.project, &scope.environment, RecoveryQuery::default()),
        Err(HistoryError::Corrupt(_))
    ));
    assert!(matches!(
        store.latest_recovery_report(scope),
        Err(HistoryError::Corrupt(_))
    ));
}
