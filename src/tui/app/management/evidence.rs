//! Read-only presentation of recorded evidence; never an execution or recovery policy.

use std::fmt::Write as _;

use crate::{
    drivers::{
        ComponentInventory, ReleaseRef,
        audit::{RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPhase, RemoteAuditRecord},
        inventory::{InventoryRelease, TemporaryRemnantKind},
    },
    history::{
        CurrentAlignment, DeploymentDetails, PackageAlignment, RecoveryReport, StepRecord,
        StepStatus,
    },
    tui::presentation::{safe_text, step_label},
};

pub(super) fn deployment_details(details: &DeploymentDetails) -> String {
    let record = &details.record;
    let mut text = format!(
        "Deployment: {}\nRecorded operation / outcome: {:?} / {:?}\nProject: {}\nEnvironment: {}\nCreated: {}\nLast local record update: {} (not proof of execution finish)\nPending intents: {} (not replayed by inspection)\nHistorical running/pending states do not prove that a worker is active now.\n",
        record.deployment,
        record.kind,
        record.state,
        record.project,
        record.environment,
        timestamp(record.created_at_ms),
        timestamp(record.updated_at_ms),
        record.pending_intent_count
    );
    if let Some(source) = &record.related_deployment {
        let _ = writeln!(text, "Related Deployment: {source}");
    }
    if let Some(metadata) = &details.metadata {
        let _ = writeln!(
            text,
            "Branch: {}\nCommit: {}\nWorktree: {:?}\nOperator: {}",
            optional_text(metadata.git_branch.as_deref()),
            optional_text(metadata.git_revision.as_deref()),
            metadata.git_worktree,
            optional_text(metadata.operator.as_deref())
        );
    } else {
        text.push_str("Source and operator metadata: unknown / not recorded\n");
    }
    text.push_str("\nFrozen Components (no current-config substitution)\nThese are planned targets, not proof that activation or health checks succeeded.\n");
    for snapshot in &details.snapshots {
        let release = &snapshot.release;
        let _ = writeln!(
            text,
            "{} · generation {} · destination {} revision {}\n  endpoint fingerprint: {}\n  recorded execution order: {}\n  before: {} -> target: {}",
            release.component,
            release.generation.get(),
            release.destination,
            release.destination_revision.get(),
            fingerprint(release),
            snapshot.execution_order,
            release_label(snapshot.expected_current.as_ref()),
            release_label(snapshot.target.as_ref())
        );
    }
    if details.snapshots.is_empty() {
        text.push_str("Unknown: no frozen Component plan was recorded.\n");
    }
    append_packages(&mut text, details);
    append_receipts(&mut text, details);
    text.push_str("\nComponent results\n");
    for item in &details.results {
        let _ = writeln!(
            text,
            "{}: {:?}\n  attempted version: {}\n  reported version: {}\n  diagnostic: {}",
            item.component,
            item.result.outcome,
            item.result
                .attempted_release
                .as_ref()
                .map_or("none reported (not proof of absence)", |version| version
                    .as_str()),
            item.result
                .observed_release
                .as_ref()
                .map_or("none reported (not proof of absence)", |version| version
                    .as_str()),
            optional_text(item.error.as_deref())
        );
    }
    if details.results.is_empty() {
        text.push_str("No Component results recorded; success or absence cannot be inferred.\n");
    }
    append_execution_details(&mut text, details);
    text
}

fn append_packages(text: &mut String, details: &DeploymentDetails) {
    text.push_str(
        "\nRecorded Release packages\nA package is not an activation or health result.\n",
    );
    for package in &details.packages {
        let _ = writeln!(
            text,
            "{} / {} · {} bytes\n  SHA-256: {}\n  source commit: {}\n  manifest created: {}",
            package.release.component,
            package.release.version,
            package.size,
            safe_text(&package.sha256),
            optional_text(package.manifest.source_revision.as_deref()),
            manifest_timestamp(package.manifest.created_at_unix)
        );
    }
    if details.packages.is_empty() {
        text.push_str("No package evidence recorded; this does not establish remote absence.\n");
    }
}

fn append_receipts(text: &mut String, details: &DeploymentDetails) {
    text.push_str("\nRecorded Release receipts\nPreparation receipts establish only their recorded phase; they do not prove activation or health.\n");
    for receipt in &details.receipts {
        let release = &receipt.release;
        let _ = writeln!(
            text,
            "{} / {} · version {}\n  recorded: {}\n  destination {} revision {} · generation {}\n  endpoint fingerprint: {}",
            receipt.component,
            step_label(&receipt.stage),
            release.version,
            timestamp(receipt.recorded_at_ms),
            release.destination,
            release.destination_revision.get(),
            release.generation.get(),
            fingerprint(release)
        );
    }
    if details.receipts.is_empty() {
        text.push_str("No receipts recorded; preparation or remote absence cannot be inferred.\n");
    }
}

fn append_execution_details(text: &mut String, details: &DeploymentDetails) {
    text.push_str("\nRecorded steps\n");
    for step in &details.steps {
        let _ = writeln!(
            text,
            "{} / {}: {:?} · elapsed {}\n  start: {}\n  completion: {}\n  evidence: {}\n  diagnostic: {}",
            step.component,
            step_label(&step.name),
            step.status,
            step_elapsed(step),
            optional_timestamp(step.started_at_ms),
            optional_timestamp(step.completed_at_ms),
            if step.planned {
                "recorded planned step"
            } else {
                "legacy/unplanned intent; no plan reconstructed"
            },
            optional_text(step.error.as_deref())
        );
    }
    if details.steps.is_empty() {
        text.push_str("No steps recorded; completion or success cannot be inferred.\n");
    }
    text.push_str("\nObserved facts (empty result fields alone do not establish absence)\nThese are historical observations, not fresh service-health checks.\n");
    for observation in &details.observations {
        let current = match &observation.observed {
            Ok(None) => "not_deployed (confirmed absent)".into(),
            Ok(Some(release)) => release.version.to_string(),
            Err(error) => format!("UNKNOWN: {}", safe_text(error)),
        };
        let _ = writeln!(
            text,
            "{} / {}: {current}; {} ({})",
            observation.component,
            step_label(&observation.stage),
            health_label(observation.healthy),
            timestamp(observation.observed_at_ms)
        );
    }
    if details.observations.is_empty() {
        text.push_str("No observations recorded; current version and health remain unknown.\n");
    }
    text.push_str("\nPending intents — inspect, do not replay\nCleanup issues do not undo a known successful deployment. Unknown deletion outcomes remain unresolved.\n");
    for pending in &details.pending {
        let _ = writeln!(
            text,
            "{} / {} · target: {}\n  intent created: {}",
            pending.component,
            step_label(&pending.stage),
            safe_text(&pending.target),
            timestamp(pending.created_at_ms)
        );
    }
    if details.pending.is_empty() {
        text.push_str("No pending intents recorded in this detail snapshot; this does not prove remote health or absence.\n");
    }
}

fn step_elapsed(step: &StepRecord) -> String {
    if let Some((start, end)) = step.started_at_ms.zip(step.completed_at_ms) {
        return end.checked_sub(start).map_or_else(
            || "unknown (recorded completion precedes start)".into(),
            |elapsed| format!("{elapsed}ms"),
        );
    }
    match step.status {
        StepStatus::Pending => "unknown (start not recorded)".into(),
        StepStatus::Running => {
            "unknown (completion not recorded; not proof of activity now)".into()
        }
        StepStatus::Skipped => "not executed (recorded skipped)".into(),
        StepStatus::Succeeded | StepStatus::Failed => "unknown (recorded timing incomplete)".into(),
    }
}

pub(super) fn report_details(report: &RecoveryReport) -> String {
    let mut text = format!(
        "Inspection: {}\nCheck started: {}\nCheck completed: {}\nNo commands replayed, services repaired or original outcomes changed.\nVersion equality does not prove health or operation success.\nThis report describes its observation time, not current service health.\n",
        report.id,
        timestamp(report.started_at_ms),
        timestamp(report.completed_at_ms)
    );
    text.push_str(
        "Project / Environment identities are shown from each recorded Component scope below.\n",
    );
    if let Some(source) = &report.related_deployment {
        let _ = writeln!(
            text,
            "Source Deployment: {source}\nSource history revision: {}",
            report.source_revision.map_or_else(
                || "unknown / not recorded".into(),
                |revision| revision.to_string()
            )
        );
    } else {
        text.push_str(
            "Source: inventory-only inspection; no historical Deployment association recorded.\n",
        );
    }
    for component in &report.components {
        let scope = &component.scope;
        let _ = writeln!(
            text,
            "\n{} · generation {} · destination {} revision {}\n  Project: {}\n  Environment: {}\n  endpoint fingerprint: {}\n  current alignment: {} · package: {}",
            scope.component,
            scope.generation.get(),
            scope.destination,
            scope.destination_revision.get(),
            scope.project,
            scope.environment,
            String::from(scope.endpoint_fingerprint.clone()),
            alignment(component.alignment),
            package_alignment(component.package_alignment)
        );
        match &component.inventory {
            Err(error) => {
                let _ = writeln!(text, "  Inventory UNKNOWN: {}", safe_text(error));
            }
            Ok(inventory) => append_inventory(&mut text, inventory),
        }
        append_notices(&mut text, &component.notices);
    }
    if report.components.is_empty() {
        text.push_str("\nNo Component evidence recorded; no successful inspection or absence is established.\n");
    }
    text
}

fn append_inventory(text: &mut String, inventory: &ComponentInventory) {
    let current = match &inventory.releases.current {
        Ok(Some(version)) => version.to_string(),
        Ok(None) => "not_deployed (confirmed absent)".into(),
        Err(error) => format!("UNKNOWN: {}", safe_text(error)),
    };
    let _ = writeln!(
        text,
        "  Current: {current}\n  Release inventory (not an activation or deletion authorization):"
    );
    for release in &inventory.releases.releases {
        append_inventory_release(text, release);
    }
    if inventory.releases.releases.is_empty() {
        text.push_str("  No Release entries recorded; use current, issues and coverage separately, not as proof of absence.\n");
    }
    for issue in &inventory.releases.issues {
        let _ = writeln!(
            text,
            "  INCOMPLETE {}: {}",
            issue
                .version
                .as_ref()
                .map_or("unknown version", |version| version.as_str()),
            safe_text(&issue.message)
        );
    }
    append_audit(text, inventory);
    append_remnants(text, inventory);
    for notices in [
        &inventory.releases.notices,
        &inventory.audit.notices,
        &inventory.remnants.notices,
    ] {
        append_notices(text, notices);
    }
}

fn append_inventory_release(text: &mut String, release: &InventoryRelease) {
    let _ = writeln!(
        text,
        "  {} · {} bytes · {}\n    SHA-256: {}\n    source: {} · created: {}",
        release.manifest.version,
        release.size,
        if release.extracted {
            "archive + extracted directory"
        } else {
            "archive only; extracted directory missing"
        },
        safe_text(&release.sha256),
        optional_text(release.manifest.source_revision.as_deref()),
        manifest_timestamp(release.manifest.created_at_unix)
    );
}

fn append_audit(text: &mut String, inventory: &ComponentInventory) {
    let _ = writeln!(
        text,
        "  Audit: {} entries; {}\n  Audit is auxiliary phase evidence, not the overall Deployment outcome or fresh health.",
        inventory.audit.records.len(),
        if inventory.audit.incomplete {
            "incomplete / missing; not proof of success"
        } else {
            "complete scan of retained evidence only"
        }
    );
    for record in &inventory.audit.records {
        append_audit_record(text, record);
    }
    if inventory.audit.records.is_empty() {
        text.push_str("  No audit entries recorded; no operation or remote absence is inferred.\n");
    }
}

fn append_audit_record(text: &mut String, record: &RemoteAuditRecord) {
    let phase = match record.phase {
        RemoteAuditPhase::Prepare => "Preparing Release",
        RemoteAuditPhase::Activate => "Activating",
        RemoteAuditPhase::Rollback => "Rolling back",
    };
    let outcome = match record.outcome {
        RemoteAuditOutcome::Succeeded => "phase succeeded",
        RemoteAuditOutcome::Failed => "phase failed",
    };
    let observed = match &record.observed {
        RemoteAuditObserved::Unknown => "UNKNOWN (observation unavailable)".into(),
        RemoteAuditObserved::NotDeployed => {
            "not_deployed (confirmed absent in this audit observation)".into()
        }
        RemoteAuditObserved::Release(version) => version.to_string(),
    };
    let _ = writeln!(
        text,
        "    Event {} · Deployment {}\n      {} / {}: {outcome}; recorded {}\n      audit expected before: {} -> target: {} (intent, not observation)\n      observed: {observed}; {}\n      destination {} revision {} · generation {}\n      endpoint fingerprint: {}",
        record.event_id,
        record.deployment,
        record.release.component,
        phase,
        timestamp(record.recorded_at_ms),
        record
            .expected_current
            .as_ref()
            .map_or("not_deployed", |version| version.as_str()),
        record
            .target
            .as_ref()
            .map_or("not_deployed", |version| version.as_str()),
        health_label(record.healthy),
        record.release.destination,
        record.release.destination_revision.get(),
        record.release.generation.get(),
        fingerprint(&record.release)
    );
    if let Some(package) = &record.package {
        let _ = writeln!(
            text,
            "      package {} · {} bytes · SHA-256: {}",
            package.manifest.version,
            package.size,
            safe_text(&package.sha256)
        );
    } else {
        text.push_str("      Package evidence not recorded in this audit entry.\n");
    }
}

fn append_remnants(text: &mut String, inventory: &ComponentInventory) {
    let _ = writeln!(
        text,
        "  Temporary remnants: {}; {}\n  Identification only: neither ownership nor permission to remove; nothing removed.",
        inventory.remnants.entries.len(),
        if inventory.remnants.incomplete {
            "scan incomplete"
        } else {
            "complete scan"
        }
    );
    for remnant in &inventory.remnants.entries {
        let kind = match remnant.kind {
            TemporaryRemnantKind::UploadArchive => "temporary upload archive",
            TemporaryRemnantKind::ExtractedDirectory => "temporary extracted directory",
            TemporaryRemnantKind::ActivationLink => "temporary activation link",
            TemporaryRemnantKind::RollbackLink => "temporary rollback link",
            TemporaryRemnantKind::MarkerPublication => "temporary identity-marker publication",
        };
        let _ = writeln!(
            text,
            "    {kind} · Deployment: {}",
            remnant.deployment.as_ref().map_or_else(
                || "not encoded (marker identity is not a Deployment ID)".into(),
                ToString::to_string
            )
        );
    }
    if inventory.remnants.entries.is_empty() {
        text.push_str("  No remnant entries recorded; assess scan coverage separately.\n");
    }
}

fn append_notices(text: &mut String, notices: &[String]) {
    for notice in notices {
        let _ = writeln!(text, "  NOTICE: {}", safe_text(notice));
    }
}

fn fingerprint(release: &ReleaseRef) -> String {
    String::from(release.endpoint_fingerprint.clone())
}

const fn health_label(health: Option<bool>) -> &'static str {
    match health {
        Some(true) => "healthy at the recorded observation",
        Some(false) => "unhealthy at the recorded observation",
        None => "health unknown",
    }
}

const fn alignment(value: CurrentAlignment) -> &'static str {
    match value {
        CurrentAlignment::Unplanned => "inventory only",
        CurrentAlignment::Target => "target version",
        CurrentAlignment::Previous => "previous version",
        CurrentAlignment::Other => "other version",
        CurrentAlignment::Unknown => "unknown",
    }
}

const fn package_alignment(value: PackageAlignment) -> &'static str {
    match value {
        PackageAlignment::Unplanned => "inventory only",
        PackageAlignment::Matches => "matches frozen evidence",
        PackageAlignment::ArchiveOnly => "archive only",
        PackageAlignment::Missing => "missing",
        PackageAlignment::Mismatch => "mismatched",
        PackageAlignment::Unknown => "unknown",
    }
}

pub(super) fn timestamp(millis: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).map_or_else(
        |_| "timestamp outside supported range".into(),
        |value| {
            format!(
                "{} {:02}:{:02}:{:02}.{:03} UTC",
                value.date(),
                value.hour(),
                value.minute(),
                value.second(),
                value.millisecond()
            )
        },
    )
}

fn optional_timestamp(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown / not recorded".into(), timestamp)
}

fn manifest_timestamp(seconds: u64) -> String {
    seconds
        .checked_mul(1000)
        .map_or_else(|| "timestamp outside supported range".into(), timestamp)
}

fn optional_text(value: Option<&str>) -> String {
    value.map_or_else(|| "unknown / not recorded".into(), safe_text)
}

pub(super) fn release_label(release: Option<&ReleaseRef>) -> String {
    release.map_or_else(
        || "not_deployed".into(),
        |release| release.version.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{
            ComponentGeneration, ComponentName, DeploymentId, DeploymentState, DestinationKey,
            DestinationRevision, DriverCapabilities, EnvironmentId, ProjectId, ReleaseManifest,
            ReleaseVersion,
        },
        drivers::{
            DriverKind, EndpointFingerprint, ReleaseInventory, RemoteAuditHistory,
            inventory::{TemporaryRemnant, TemporaryRemnants},
        },
        history::{
            DeploymentComponentSnapshot, DeploymentKind, DeploymentRecord, InspectionScope,
            ObservationRecord, RecoveryComponentReport, ReleaseReceiptRecord,
        },
    };

    fn reference(component: &str) -> ReleaseRef {
        ReleaseRef {
            driver: DriverKind::parse("linux-ssh").unwrap(),
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse(component).unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse("v1.2.3").unwrap(),
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            effective_capabilities: DriverCapabilities::new([]),
        }
    }

    fn details() -> DeploymentDetails {
        DeploymentDetails {
            record: DeploymentRecord {
                deployment: DeploymentId::new(),
                project: ProjectId::new(),
                environment: EnvironmentId::new(),
                state: DeploymentState::Running,
                kind: DeploymentKind::Deploy,
                related_deployment: None,
                created_at_ms: 10,
                updated_at_ms: 20,
                pending_intent_count: 0,
            },
            metadata: None,
            snapshots: Vec::new(),
            results: Vec::new(),
            steps: Vec::new(),
            observations: Vec::new(),
            pending: Vec::new(),
            packages: Vec::new(),
            receipts: Vec::new(),
            log: None,
        }
    }

    fn step(status: StepStatus, start: Option<u64>, end: Option<u64>) -> StepRecord {
        StepRecord {
            sequence: 1,
            component: ComponentName::parse("api").unwrap(),
            name: "cleanup.v1.2.3".into(),
            status,
            intent: None,
            started_at_ms: start,
            completed_at_ms: end,
            error: None,
            planned: true,
        }
    }

    fn report() -> RecoveryReport {
        let release = reference("api");
        RecoveryReport {
            id: uuid::Uuid::now_v7(),
            related_deployment: None,
            source_revision: None,
            started_at_ms: 10,
            completed_at_ms: 20,
            components: vec![RecoveryComponentReport {
                scope: InspectionScope::from(&release),
                alignment: CurrentAlignment::Unplanned,
                package_alignment: PackageAlignment::Unplanned,
                notices: Vec::new(),
                inventory: Ok(ComponentInventory {
                    releases: ReleaseInventory {
                        releases: Vec::new(),
                        issues: Vec::new(),
                        current: Err("observation unavailable".into()),
                        notices: Vec::new(),
                    },
                    audit: RemoteAuditHistory::default(),
                    remnants: TemporaryRemnants::default(),
                }),
            }],
        }
    }

    fn audit(release: ReleaseRef, observed: RemoteAuditObserved) -> RemoteAuditRecord {
        let absent = observed == RemoteAuditObserved::NotDeployed;
        RemoteAuditRecord {
            schema_version: 1,
            event_id: uuid::Uuid::now_v7(),
            deployment: DeploymentId::new(),
            recorded_at_ms: 30,
            expected_current: Some(release.version.clone()),
            target: (!absent).then(|| release.version.clone()),
            phase: if absent {
                RemoteAuditPhase::Rollback
            } else {
                RemoteAuditPhase::Activate
            },
            outcome: if absent {
                RemoteAuditOutcome::Succeeded
            } else {
                RemoteAuditOutcome::Failed
            },
            release,
            observed,
            healthy: None,
            package: None,
        }
    }

    #[test]
    fn empty_history_is_unknown_and_created_is_not_started_or_finished() {
        let details = details();
        let before = details.clone();
        let text = deployment_details(&details);
        assert!(text.contains("Created: 1970-01-01 00:00:00.010 UTC"));
        assert!(text.contains("Last local record update:"));
        assert!(text.contains("not proof of execution finish"));
        assert!(text.contains("do not prove that a worker is active now"));
        for category in [
            "No package evidence recorded",
            "No receipts recorded",
            "No Component results recorded",
            "No steps recorded",
            "No observations recorded",
            "No pending intents recorded",
        ] {
            assert!(text.contains(category));
        }
        assert!(!text.contains("confirmed absent"));
        assert_eq!(details, before);
    }

    #[test]
    fn receipts_keep_component_phase_version_and_frozen_endpoint_without_claiming_health() {
        let mut details = details();
        for component in ["api", "worker"] {
            let mut release = reference(component);
            release.project_id.clone_from(&details.record.project);
            release
                .environment_id
                .clone_from(&details.record.environment);
            details.receipts.push(ReleaseReceiptRecord {
                sequence: 1,
                component: release.component.clone(),
                stage: "linux-ssh.prepare".into(),
                release,
                recorded_at_ms: 42,
            });
        }
        let before = details.clone();
        let text = deployment_details(&details);
        for receipt in &details.receipts {
            assert!(text.contains(&format!(
                "{} / Preparing Release · version v1.2.3",
                receipt.component
            )));
            assert!(text.contains(&receipt.release.destination.to_string()));
        }
        assert!(text.contains("1970-01-01 00:00:00.042 UTC"));
        assert!(text.contains("do not prove activation or health"));
        assert!(!text.contains("linux-ssh"));
        assert_eq!(details, before);
    }

    #[test]
    fn steps_distinguish_skipped_unfinished_missing_and_reversed_timing() {
        assert_eq!(
            step_elapsed(&step(StepStatus::Succeeded, Some(10), Some(42))),
            "32ms"
        );
        assert!(
            step_elapsed(&step(StepStatus::Failed, Some(42), Some(10)))
                .contains("completion precedes start")
        );
        assert!(
            step_elapsed(&step(StepStatus::Running, Some(10), None))
                .contains("not proof of activity now")
        );
        assert_eq!(
            step_elapsed(&step(StepStatus::Skipped, None, Some(20))),
            "not executed (recorded skipped)"
        );
        assert!(
            step_elapsed(&step(StepStatus::Succeeded, None, Some(20)))
                .contains("timing incomplete")
        );
        let mut details = details();
        details
            .steps
            .push(step(StepStatus::Failed, Some(42), Some(10)));
        let text = deployment_details(&details);
        assert!(text.contains("Cleanup v1.2.3"));
        assert!(!text.contains("elapsed 0ms"));
    }

    #[test]
    fn planned_absence_unknown_observation_and_observed_absence_remain_distinct() {
        let mut details = details();
        let mut release = reference("api");
        release.project_id.clone_from(&details.record.project);
        release
            .environment_id
            .clone_from(&details.record.environment);
        details.snapshots.push(DeploymentComponentSnapshot {
            release: release.clone(),
            expected_current: Some(release.clone()),
            target: None,
            execution_order: 0,
        });
        details.observations = vec![
            ObservationRecord {
                sequence: 1,
                component: release.component.clone(),
                stage: "before".into(),
                observed: Ok(None),
                healthy: None,
                observed_at_ms: 20,
            },
            ObservationRecord {
                sequence: 2,
                component: release.component,
                stage: "after".into(),
                observed: Err("connection lost".into()),
                healthy: None,
                observed_at_ms: 21,
            },
        ];
        let text = deployment_details(&details);
        assert!(text.contains("target: not_deployed"));
        assert!(text.contains("api / before: not_deployed (confirmed absent); health unknown"));
        assert!(text.contains("api / after: UNKNOWN: connection lost; health unknown"));
        assert_eq!(text.matches("confirmed absent").count(), 1);
    }

    #[test]
    fn report_keeps_explicit_scope_source_revision_and_unknown_empty_inventory() {
        let mut report = report();
        report.related_deployment = Some(DeploymentId::new());
        report.source_revision = Some(17);
        let before = report.clone();
        let text = report_details(&report);
        assert!(text.contains(&format!("Project: {}", report.components[0].scope.project)));
        assert!(text.contains(&format!(
            "Environment: {}",
            report.components[0].scope.environment
        )));
        assert!(text.contains("Source history revision: 17"));
        assert!(text.contains("Current: UNKNOWN: observation unavailable"));
        assert!(text.contains("No Release entries recorded"));
        assert!(text.contains("No audit entries recorded"));
        assert!(text.contains("No remnant entries recorded"));
        assert!(!text.contains("confirmed absent"));
        assert_eq!(report, before);
    }

    #[test]
    fn audit_and_remnants_show_evidence_not_success_repair_or_deletion_authority() {
        let mut report = report();
        let mut release = reference("api");
        release
            .project_id
            .clone_from(&report.components[0].scope.project);
        release
            .environment_id
            .clone_from(&report.components[0].scope.environment);
        let unknown = audit(release.clone(), RemoteAuditObserved::Unknown);
        let absent = audit(release, RemoteAuditObserved::NotDeployed);
        let remnant_deployment = DeploymentId::new();
        let inventory = report.components[0].inventory.as_mut().unwrap();
        inventory.audit.records = vec![unknown, absent];
        inventory.audit.incomplete = true;
        inventory.remnants.entries = vec![
            TemporaryRemnant {
                kind: TemporaryRemnantKind::UploadArchive,
                deployment: Some(remnant_deployment.clone()),
            },
            TemporaryRemnant {
                kind: TemporaryRemnantKind::MarkerPublication,
                deployment: None,
            },
        ];
        inventory.remnants.incomplete = true;
        let before = report.clone();
        let text = report_details(&report);
        assert!(text.contains("Activating: phase failed"));
        assert!(text.contains("Rolling back: phase succeeded"));
        assert!(text.contains("observed: UNKNOWN (observation unavailable); health unknown"));
        assert!(text.contains("confirmed absent in this audit observation"));
        assert!(text.contains("not the overall Deployment outcome"));
        assert!(text.contains("temporary upload archive"));
        assert!(text.contains(&remnant_deployment.to_string()));
        assert!(text.contains("marker identity is not a Deployment ID"));
        assert!(text.contains("neither ownership nor permission to remove"));
        assert!(text.contains("scan incomplete"));
        assert!(!text.contains("linux-ssh"));
        assert_eq!(report, before);
    }

    #[test]
    fn all_inventory_entries_remain_reachable_and_dynamic_diagnostics_are_terminal_safe() {
        let mut report = report();
        let scope = report.components[0].scope.clone();
        report.components[0]
            .notices
            .push("notice\u{1b}\u{202e}END_NOTICE".into());
        let inventory = report.components[0].inventory.as_mut().unwrap();
        for index in 0..100 {
            inventory.releases.releases.push(InventoryRelease {
                manifest: ReleaseManifest {
                    schema_version: 1,
                    project_id: scope.project.clone(),
                    environment_id: scope.environment.clone(),
                    component: scope.component.clone(),
                    generation: scope.generation,
                    version: ReleaseVersion::parse(format!("v{index}.END_RELEASE")).unwrap(),
                    created_at_unix: 1,
                    source_revision: None,
                },
                sha256: "a".repeat(64),
                size: 123,
                extracted: index % 2 == 0,
            });
        }
        let text = report_details(&report);
        assert!(text.contains("v99.END_RELEASE"));
        assert!(text.contains("archive only; extracted directory missing"));
        assert!(text.ends_with("NOTICE: noticeEND_NOTICE\n"));
        assert!(!text.contains(['\u{1b}', '\u{202e}']));
    }

    #[test]
    fn timestamps_use_utc_and_invalid_extremes_do_not_wrap() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(timestamp(86_400_123), "1970-01-02 00:00:00.123 UTC");
        assert_eq!(timestamp(u64::MAX), "timestamp outside supported range");
        assert_eq!(
            manifest_timestamp(u64::MAX),
            "timestamp outside supported range"
        );
        assert_eq!(release_label(None), "not_deployed");
    }
}
