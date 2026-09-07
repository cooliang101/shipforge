use super::*;
use crate::{
    domain::{
        ComponentGeneration, ComponentName, DestinationRevision, DriverCapabilities, EnvironmentId,
        ProjectId, ReleaseManifest, ReleaseVersion,
    },
    drivers::{DriverKind, EndpointFingerprint, ReleaseRef},
    history::{
        CurrentAlignment, DeploymentComponentSnapshot, InspectionScope, PackageAlignment,
        RecoveryComponentReport, RecoveryReport,
    },
    telemetry::Redactor,
};

fn start(store: &HistoryStore) -> (DeploymentId, ReleaseRef) {
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
    let deployment = DeploymentId::new();
    store
        .create_deployment(&deployment, &release.project_id, &release.environment_id, 1)
        .unwrap();
    store
        .record_component_snapshots(
            &deployment,
            &[DeploymentComponentSnapshot {
                target_snapshot: None,
                release: release.clone(),
                target: Some(release.clone()),
                expected_current: None,
                execution_order: 0,
            }],
        )
        .unwrap();
    store
        .transition_deployment(
            &deployment,
            crate::domain::DeploymentState::Created,
            crate::domain::DeploymentState::Running,
            2,
        )
        .unwrap();
    (deployment, release)
}

#[test]
fn counts_historical_scopes_packages_receipts_and_recovery_without_filtering_old_revisions() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, release) = start(&store);
    store
        .record_release_package(
            &id,
            &release,
            &ReleaseManifest {
                schema_version: 1,
                project_id: release.project_id.clone(),
                environment_id: release.environment_id.clone(),
                component: release.component.clone(),
                generation: release.generation,
                version: release.version.clone(),
                created_at_unix: 1,
                source_revision: None,
            },
            &"b".repeat(64),
            123,
        )
        .unwrap();
    store
        .record_release_receipt(&id, &release.component, "prepare", &release, 2)
        .unwrap();
    store
        .record_observation(
            &id,
            &release.component,
            "observed",
            Ok(Some(&release)),
            None,
            3,
            &Redactor::default(),
        )
        .unwrap();
    let mut scope = InspectionScope::from(&release);
    scope.destination_revision = DestinationRevision::INITIAL.checked_next().unwrap();
    scope.endpoint_fingerprint = EndpointFingerprint::parse("c".repeat(64)).unwrap();
    store
        .append_recovery_report(
            &RecoveryReport {
                id: uuid::Uuid::now_v7(),
                related_deployment: None,
                source_revision: None,
                started_at_ms: 3,
                completed_at_ms: 4,
                components: vec![RecoveryComponentReport {
                    scope,
                    inventory: Err("unavailable".into()),
                    alignment: CurrentAlignment::Unknown,
                    package_alignment: PackageAlignment::Unknown,
                    notices: Vec::new(),
                }],
            },
            &Redactor::default(),
        )
        .unwrap();
    let found = store.destination_references(&release.destination).unwrap();
    assert_eq!(
        (found.deployments, found.releases, found.recovery_reports),
        (1, 3, 1)
    );
    let unused = store
        .destination_references(&DestinationKey::new())
        .unwrap();
    assert_eq!(
        (unused.deployments, unused.releases, unused.recovery_reports),
        (0, 0, 0)
    );
    assert_eq!(unused.evidence_fingerprint, found.evidence_fingerprint);
}

#[test]
fn unrelated_new_history_changes_the_preview_fingerprint() {
    let store = HistoryStore::in_memory().unwrap();
    let key = DestinationKey::new();
    let initial = store.destination_references(&key).unwrap();
    start(&store);
    let changed = store.destination_references(&key).unwrap();
    assert_ne!(initial.evidence_fingerprint, changed.evidence_fingerprint);
    assert_eq!(changed.deployments, 0);
}

#[test]
fn absent_or_corrupt_scope_never_means_zero_references() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, release) = start(&store);
    store
        .connection
        .execute(
            "UPDATE component_snapshots SET snapshot='{}' WHERE deployment_id=?1",
            [id.to_string()],
        )
        .unwrap();
    assert!(
        store
            .destination_references(&DestinationKey::new())
            .is_err()
    );
    let store = HistoryStore::in_memory().unwrap();
    store
        .create_deployment(&id, &release.project_id, &release.environment_id, 1)
        .unwrap();
    assert!(
        store
            .destination_references(&DestinationKey::new())
            .is_err()
    );
}

#[test]
fn bounds_are_enforced_before_json_decode_or_unbounded_row_collection() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, _) = start(&store);
    store
        .connection
        .execute(
            "UPDATE component_snapshots SET snapshot=?1 WHERE deployment_id=?2",
            rusqlite::params!["x".repeat(MAX_BYTES + 1), id.to_string()],
        )
        .unwrap();
    assert!(
        store
            .destination_references(&DestinationKey::new())
            .unwrap_err()
            .to_string()
            .contains("byte limits")
    );
    let store = HistoryStore::in_memory().unwrap();
    store.connection.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<4097) INSERT INTO deployment_revisions SELECT printf('%036d',x),1 FROM n;").unwrap();
    assert!(
        store
            .destination_references(&DestinationKey::new())
            .unwrap_err()
            .to_string()
            .contains("row limits")
    );
}

#[test]
fn unscoped_legacy_intent_blocks_even_an_unrelated_destination() {
    let store = HistoryStore::in_memory().unwrap();
    let (id, _) = start(&store);
    store.connection.execute("INSERT INTO operation_intents(deployment_id,component,stage,target,status,created_at_ms) VALUES(?1,'worker','prepare','v1','pending',1)", [id.to_string()]).unwrap();
    assert!(
        store
            .destination_references(&DestinationKey::new())
            .is_err()
    );
}

#[test]
fn recovery_audit_retains_a_different_historical_destination_reference() {
    use crate::drivers::{
        ComponentInventory, ReleaseInventory, RemoteAuditHistory,
        audit::{RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPhase, RemoteAuditRecord},
        inventory::TemporaryRemnants,
    };
    let store = HistoryStore::in_memory().unwrap();
    let (_, release) = start(&store);
    let historical_key = DestinationKey::new();
    let mut historical_release = release.clone();
    historical_release.destination = historical_key.clone();
    let record = RemoteAuditRecord {
        schema_version: 1,
        event_id: uuid::Uuid::now_v7(),
        deployment: DeploymentId::new(),
        recorded_at_ms: 2,
        release: historical_release,
        phase: RemoteAuditPhase::Activate,
        outcome: RemoteAuditOutcome::Failed,
        expected_current: None,
        target: Some(release.version.clone()),
        observed: RemoteAuditObserved::Unknown,
        healthy: None,
        package: None,
    };
    store
        .append_recovery_report(
            &RecoveryReport {
                id: uuid::Uuid::now_v7(),
                related_deployment: None,
                source_revision: None,
                started_at_ms: 3,
                completed_at_ms: 4,
                components: vec![RecoveryComponentReport {
                    scope: InspectionScope::from(&release),
                    inventory: Ok(ComponentInventory {
                        releases: ReleaseInventory {
                            releases: Vec::new(),
                            issues: Vec::new(),
                            current: Err("unknown".into()),
                            notices: Vec::new(),
                        },
                        audit: RemoteAuditHistory {
                            records: vec![record],
                            notices: Vec::new(),
                            incomplete: false,
                        },
                        remnants: TemporaryRemnants::default(),
                    }),
                    alignment: CurrentAlignment::Unplanned,
                    package_alignment: PackageAlignment::Unplanned,
                    notices: Vec::new(),
                }],
            },
            &Redactor::default(),
        )
        .unwrap();
    let references = store.destination_references(&historical_key).unwrap();
    assert_eq!(
        (
            references.deployments,
            references.releases,
            references.recovery_reports
        ),
        (0, 0, 1)
    );
}
