use super::*;
use crate::{
    domain::{
        ComponentGeneration, ComponentName, DeploymentState, DestinationKey, DestinationRevision,
        DriverCapabilities, EnvironmentId, ProjectId,
    },
    drivers::{DriverKind, EndpointFingerprint},
    history::IntentStatus,
    telemetry::Redactor,
};

fn release() -> ReleaseRef {
    ReleaseRef {
        driver: DriverKind::parse("linux-ssh").unwrap(),
        project_id: ProjectId::new(),
        environment_id: EnvironmentId::new(),
        component: ComponentName::parse("api").unwrap(),
        generation: ComponentGeneration::INITIAL,
        version: ReleaseVersion::parse("v3").unwrap(),
        destination: DestinationKey::new(),
        destination_revision: DestinationRevision::INITIAL,
        endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
        effective_capabilities: DriverCapabilities::default(),
    }
}

fn start(store: &HistoryStore, release: &ReleaseRef) -> DeploymentId {
    let id = DeploymentId::new();
    store
        .create_deployment(&id, &release.project_id, &release.environment_id, 1)
        .unwrap();
    let mut previous = release.clone();
    previous.version = ReleaseVersion::parse("v2").unwrap();
    store
        .record_component_snapshots(
            &id,
            &[DeploymentComponentSnapshot {
                release: release.clone(),
                target: Some(release.clone()),
                expected_current: Some(previous),
                execution_order: 0,
            }],
        )
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 2)
        .unwrap();
    id
}

fn package(store: &HistoryStore, id: &DeploymentId, release: &ReleaseRef) {
    store
        .record_release_package(
            id,
            release,
            &ReleaseManifest {
                schema_version: 1,
                project_id: release.project_id.clone(),
                environment_id: release.environment_id.clone(),
                component: release.component.clone(),
                generation: release.generation,
                version: release.version.clone(),
                created_at_unix: 1,
                source_revision: Some("abcdef123".into()),
            },
            &"b".repeat(64),
            123,
        )
        .unwrap();
}

fn observe(
    store: &HistoryStore,
    id: &DeploymentId,
    release: &ReleaseRef,
    healthy: Option<bool>,
    at: u64,
) {
    store
        .record_observation(
            id,
            &release.component,
            "recorded-outcome",
            Ok(Some(release)),
            healthy,
            at,
            &Redactor::default(),
        )
        .unwrap();
}

fn finish(store: &HistoryStore, id: &DeploymentId) {
    store
        .transition_deployment(id, DeploymentState::Running, DeploymentState::Failed, 60)
        .unwrap();
}

#[test]
fn healthy_evidence_uses_insertion_order_preserves_old_endpoints_and_ignores_overall_failure() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let first = start(&store, &release);
    package(&store, &first, &release);
    observe(&store, &first, &release, Some(true), 50);
    finish(&store, &first);
    let mut older_endpoint = release.clone();
    older_endpoint.destination_revision = DestinationRevision::INITIAL.checked_next().unwrap();
    older_endpoint.endpoint_fingerprint = EndpointFingerprint::parse("c".repeat(64)).unwrap();
    older_endpoint.version = ReleaseVersion::parse("v4").unwrap();
    let second = start(&store, &older_endpoint);
    package(&store, &second, &older_endpoint);
    observe(&store, &second, &older_endpoint, Some(true), 3);
    finish(&store, &second);
    let history = store
        .retention_history(&InspectionScope::from(&release))
        .unwrap();
    assert_eq!(history.healthy.len(), 2);
    assert!(history.healthy[0].sequence > history.healthy[1].sequence);
    assert!(history.healthy[0].observed_at_ms < history.healthy[1].observed_at_ms);
    assert_eq!(
        history.healthy[0].observed,
        Ok(Some(older_endpoint.clone()))
    );
    assert!(
        history
            .packages
            .iter()
            .any(|package| package.release == older_endpoint)
    );
    assert!(history.protected_versions.is_empty());
}

#[test]
fn unfinished_or_terminal_pending_work_protects_snapshots_packages_receipts_and_observed_versions()
{
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let id = start(&store, &release);
    package(&store, &id, &release);
    store
        .record_release_receipt(&id, &release.component, "prepare", &release, 3)
        .unwrap();
    let mut drifted = release.clone();
    drifted.version = ReleaseVersion::parse("v7").unwrap();
    observe(&store, &id, &drifted, None, 3);
    let pending = store
        .record_intent(&id, &release.component, "cleanup.release", "v9", 4)
        .unwrap();
    let scope = InspectionScope::from(&release);
    let before = store.recovery_basis(&id).unwrap();
    let active = store.retention_history(&scope).unwrap();
    assert_eq!(store.recovery_basis(&id).unwrap(), before);
    assert_eq!(
        active.protected_versions,
        ["v2", "v3", "v7", "v9"]
            .into_iter()
            .map(|value| ReleaseVersion::parse(value).unwrap())
            .collect()
    );
    assert!(active.healthy.is_empty());
    finish(&store, &id);
    assert_eq!(
        store.retention_history(&scope).unwrap().protected_versions,
        active.protected_versions
    );
    store
        .complete_intent(
            pending,
            IntentStatus::Succeeded,
            None,
            61,
            &Redactor::default(),
        )
        .unwrap();
    assert!(
        store
            .retention_history(&scope)
            .unwrap()
            .protected_versions
            .is_empty()
    );
}

#[test]
fn scope_filter_ignores_other_projects_environments_components_generations_drivers_and_destinations()
 {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let scope = InspectionScope::from(&release);
    for changed in 0..6 {
        let mut other = release.clone();
        match changed {
            0 => other.project_id = ProjectId::new(),
            1 => other.environment_id = EnvironmentId::new(),
            2 => other.component = ComponentName::parse("worker").unwrap(),
            3 => other.generation = ComponentGeneration::INITIAL.checked_next().unwrap(),
            4 => other.driver = DriverKind::parse("future-driver").unwrap(),
            _ => other.destination = DestinationKey::new(),
        }
        let id = start(&store, &other);
        package(&store, &id, &other);
        observe(&store, &id, &other, Some(true), 3);
    }
    assert_eq!(
        store.retention_history(&scope).unwrap(),
        RetentionHistory::default()
    );
}

#[test]
fn missing_legacy_selection_and_unparseable_pending_target_fail_closed() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let scope = InspectionScope::from(&release);
    let legacy = DeploymentId::new();
    store
        .create_deployment(&legacy, &release.project_id, &release.environment_id, 1)
        .unwrap();
    assert!(store.retention_history(&scope).is_err());
    store
        .transition_deployment(
            &legacy,
            DeploymentState::Created,
            DeploymentState::Cancelled,
            2,
        )
        .unwrap();
    let id = start(&store, &release);
    store
        .record_intent(
            &id,
            &release.component,
            "activate",
            "../../not-a-version",
            3,
        )
        .unwrap();
    assert!(store.retention_history(&scope).is_err());
    finish(&store, &id);
    assert!(store.retention_history(&scope).is_err());
}

#[test]
fn rollback_absence_sentinel_is_not_invented_as_a_release() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let source = start(&store, &release);
    finish(&store, &source);
    let id = DeploymentId::new();
    store
        .create_rollback_deployment(
            &id,
            &source,
            &release.project_id,
            &release.environment_id,
            61,
        )
        .unwrap();
    store
        .record_component_snapshots(
            &id,
            &[DeploymentComponentSnapshot {
                release: release.clone(),
                expected_current: Some(release.clone()),
                target: None,
                execution_order: 0,
            }],
        )
        .unwrap();
    store
        .transition_deployment(&id, DeploymentState::Created, DeploymentState::Running, 62)
        .unwrap();
    store
        .record_intent(&id, &release.component, "rollback", "not_deployed", 63)
        .unwrap();
    let history = store
        .retention_history(&InspectionScope::from(&release))
        .unwrap();
    assert_eq!(
        history.protected_versions,
        [release.version].into_iter().collect()
    );
}

#[test]
fn unknown_absent_and_unhealthy_observations_never_become_positive_health_evidence() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let id = start(&store, &release);
    observe(&store, &id, &release, None, 3);
    observe(&store, &id, &release, Some(false), 4);
    store
        .record_observation(
            &id,
            &release.component,
            "unknown",
            Err("unreachable"),
            None,
            5,
            &Redactor::default(),
        )
        .unwrap();
    store
        .record_observation(
            &id,
            &release.component,
            "stopped",
            Ok(None),
            Some(true),
            6,
            &Redactor::default(),
        )
        .unwrap();
    finish(&store, &id);
    assert!(
        store
            .retention_history(&InspectionScope::from(&release))
            .unwrap()
            .healthy
            .is_empty()
    );
}

#[test]
fn malformed_package_observation_and_snapshot_context_fail_closed() {
    for corruption in 0..4 {
        let store = HistoryStore::in_memory().unwrap();
        let release = release();
        let id = start(&store, &release);
        package(&store, &id, &release);
        observe(&store, &id, &release, Some(true), 3);
        finish(&store, &id);
        let query = match corruption {
            0 => "UPDATE release_packages SET sha256=printf('%064d',0),manifest='{}'",
            1 => "UPDATE deployment_observations SET observed_at_ms=0",
            2 => "UPDATE component_snapshots SET snapshot='{}'",
            _ => "PRAGMA ignore_check_constraints=ON; UPDATE deployment_observations SET healthy=2",
        };
        store.connection.execute_batch(query).unwrap();
        assert!(
            store
                .retention_history(&InspectionScope::from(&release))
                .is_err(),
            "case {corruption}"
        );
    }
}

#[test]
fn snapshot_and_shared_budget_fail_closed_on_row_and_byte_limits() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let id = start(&store, &release);
    let oversized = "x".repeat(MAX_BYTES + 1);
    store
        .connection
        .execute(
            "UPDATE component_snapshots SET snapshot=?1 WHERE deployment_id=?2",
            params![oversized, id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.retention_history(&InspectionScope::from(&release)),
        Err(HistoryError::InvalidMetadata(_))
    ));
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE counts(bytes TEXT); WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4097) INSERT INTO counts SELECT 'x' FROM n;").unwrap();
    assert!(
        Budget::default()
            .check(&connection, "length(bytes)", "FROM counts", &[])
            .is_err()
    );
    let mut budget = Budget {
        rows: 4095,
        bytes: 0,
    };
    assert!(
        budget
            .check(
                &connection,
                "length(bytes)",
                "FROM counts WHERE rowid<=2",
                &[]
            )
            .is_err()
    );
}

#[test]
fn actual_observation_query_rejects_more_than_limit_instead_of_returning_a_prefix() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let id = start(&store, &release);
    store.connection.execute(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4097)
         INSERT INTO deployment_observations(deployment_id,component,stage,observed_ref,error,healthy,observed_at_ms)
         SELECT ?1,?2,'current',NULL,NULL,NULL,3 FROM n",params![id.to_string(),release.component.as_str()]).unwrap();
    finish(&store, &id);
    assert!(matches!(
        store.retention_history(&InspectionScope::from(&release)),
        Err(HistoryError::InvalidMetadata(_))
    ));
}

#[test]
fn query_reopens_without_schema_changes_or_rewriting_any_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = HistoryStore::open(&path).unwrap();
    let release = release();
    let id = start(&store, &release);
    package(&store, &id, &release);
    observe(&store, &id, &release, Some(true), 3);
    let before = store.recovery_basis(&id).unwrap();
    let expected = store
        .retention_history(&InspectionScope::from(&release))
        .unwrap();
    assert_eq!(store.recovery_basis(&id).unwrap(), before);
    drop(store);
    let reopened = HistoryStore::open(&path).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 7);
    assert_eq!(
        reopened
            .retention_history(&InspectionScope::from(&release))
            .unwrap(),
        expected
    );
    assert_eq!(reopened.recovery_basis(&id).unwrap(), before);
}

#[test]
fn package_receipt_observation_and_pending_text_are_budgeted_before_decoding() {
    for query in [
        "UPDATE release_packages SET manifest=?1",
        "UPDATE release_receipts SET release_ref=?1",
        "UPDATE deployment_observations SET observed_ref=?1",
        "UPDATE operation_intents SET target=?1",
    ] {
        let store = HistoryStore::in_memory().unwrap();
        let release = release();
        let id = start(&store, &release);
        package(&store, &id, &release);
        store
            .record_release_receipt(&id, &release.component, "prepare", &release, 3)
            .unwrap();
        observe(&store, &id, &release, Some(true), 3);
        store
            .record_intent(&id, &release.component, "cleanup.release", "v9", 4)
            .unwrap();
        store
            .connection
            .execute(query, ["x".repeat(MAX_BYTES + 1)])
            .unwrap();
        assert!(
            matches!(
                store.retention_history(&InspectionScope::from(&release)),
                Err(HistoryError::InvalidMetadata(_))
            ),
            "{query}"
        );
    }
}

#[test]
fn package_receipt_cannot_be_reassigned_to_a_rollback_operation() {
    let store = HistoryStore::in_memory().unwrap();
    let release = release();
    let source = start(&store, &release);
    finish(&store, &source);
    let id = start(&store, &release);
    package(&store, &id, &release);
    finish(&store, &id);
    store
        .connection
        .execute(
            "UPDATE deployments SET kind='rollback',related_deployment_id=?1 WHERE id=?2",
            params![source.to_string(), id.to_string()],
        )
        .unwrap();
    assert!(matches!(
        store.retention_history(&InspectionScope::from(&release)),
        Err(HistoryError::Corrupt(_))
    ));
}
