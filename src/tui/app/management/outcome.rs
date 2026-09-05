//! Safe rollback outcomes; the report's typed state remains the source of truth.

use std::fmt::Write as _;

use crate::{
    application::{DeploymentFailure, OrchestrationStage, RollbackReport},
    domain::{ComponentOutcome, DeploymentState, ReleaseVersion},
    tui::{deployment_error::driver_error, presentation::step_label},
};

pub(super) fn rollback_result(report: &RollbackReport) -> String {
    let mut text = format!(
        "Result: {} · warnings:{} · manual recovery:{}\nRollback Deployment: {}\n",
        result_label(report.deployment.state),
        report.warnings.len(),
        report.compensation_failures.len(),
        report.deployment.id
    );
    write_failure(&mut text, report.failure.as_ref());
    write_warnings(&mut text, report);
    if !report.compensation_failures.is_empty() {
        let _ = writeln!(
            text,
            "MANUAL RECOVERY REQUIRED: {} Component compensation failure(s).",
            report.compensation_failures.len()
        );
        for (name, error) in &report.compensation_failures {
            let _ = writeln!(
                text,
                "Compensation failure: {name}. {}",
                driver_error(error)
            );
        }
    }
    text.push_str("\nComponent results:\n");
    if report.deployment.components.is_empty() {
        text.push_str("No Component outcomes were reported; remote state is unknown.\n");
    }
    for (name, result) in &report.deployment.components {
        let _ = writeln!(
            text,
            "{name}: {}; requested version: {}; reported version: {}",
            outcome_label(&result.outcome),
            version_label(result.attempted_release.as_ref()),
            version_label(result.observed_release.as_ref())
        );
    }
    text.push_str("\nCompensated means changes from this rollback were restored, not that its requested target succeeded. Reported versions are not fresh service-health checks.\nOpen this Deployment's history and logs for recorded steps and observations before retrying.\n");
    text
}

const PERSISTENCE_SUFFIX: &str =
    ": local history persistence failed; inspect durable history before retrying";
const LOG_WARNING: &str = "Rollback log is incomplete; known remote outcomes are retained; inspect durable history before retrying";
const PERSISTENCE_OPERATIONS: [(&str, &str); 6] = [
    (
        "record Rollback intent",
        "Could not record the rollback intent.",
    ),
    (
        "complete Rollback intent",
        "Could not save rollback intent completion.",
    ),
    (
        "record Rollback compensation intent",
        "Could not record the compensation intent.",
    ),
    (
        "complete Rollback compensation intent",
        "Could not save compensation intent completion.",
    ),
    (
        "persist Rollback Component result",
        "Could not save a Component result.",
    ),
    (
        "persist Rollback terminal state",
        "Could not save the rollback terminal state.",
    ),
];
const OBSERVATION_STAGES: [&str; 7] = [
    "rollback-preflight",
    "rollback-before-mutation",
    "rollback-receipt",
    "rollback-after-failure",
    "compensation-before-mutation",
    "compensation-receipt",
    "compensation-after-failure",
];

fn write_warnings(text: &mut String, report: &RollbackReport) {
    if report.warnings.is_empty() {
        return;
    }
    let _ = writeln!(
        text,
        "WARNING: {} reported warning(s); known outcomes are unchanged.",
        report.warnings.len()
    );
    let mut unknown = 0;
    for warning in &report.warnings {
        if let Some(description) = warning_context(warning, report) {
            let _ = writeln!(text, "Persistence warning: {description}");
        } else {
            unknown += 1;
        }
    }
    if unknown > 0 {
        let _ = writeln!(
            text,
            "{unknown} other operation/audit warning(s); raw diagnostics are not displayed."
        );
    }
    text.push_str(
        "Saved history, logs or audit may be incomplete. Inspect history before retrying.\n",
    );
}

fn warning_context(warning: &str, report: &RollbackReport) -> Option<String> {
    // Compare entire controlled messages. Neither a recognized prefix nor a
    // syntactically valid Component name authorizes arbitrary Driver text.
    if warning == LOG_WARNING {
        return Some("The rollback log is incomplete; known remote outcomes are retained.".into());
    }
    for (operation, description) in PERSISTENCE_OPERATIONS {
        if warning == format!("{operation}{PERSISTENCE_SUFFIX}") {
            return Some(description.into());
        }
    }
    for name in report.deployment.components.keys() {
        for stage in OBSERVATION_STAGES {
            if warning == format!("persist {stage} observation for {name}{PERSISTENCE_SUFFIX}") {
                return Some(format!("Could not save {name} / {}.", step_label(stage)));
            }
        }
    }
    None
}

const fn result_label(state: DeploymentState) -> &'static str {
    match state {
        DeploymentState::Succeeded => "Succeeded",
        DeploymentState::Cancelled => "Cancelled",
        DeploymentState::Failed => "Failed",
        DeploymentState::Created => "Not completed (created)",
        DeploymentState::Running => "Not completed (running)",
    }
}

fn write_failure(text: &mut String, failure: Option<&DeploymentFailure>) {
    match failure {
        Some(DeploymentFailure::Cancelled) => {
            text.push_str("Cancellation was requested. Review the reported Component and compensation outcomes; cancellation does not prove that no remote effects occurred.\n");
        }
        Some(DeploymentFailure::Driver {
            component,
            stage,
            error,
            observed_release,
        }) => {
            let _ = writeln!(
                text,
                "Main failure: {component} / {}\n{}\nVersion observed at failure: {}",
                stage_label(*stage),
                driver_error(error),
                version_label(observed_release.as_ref())
            );
        }
        Some(DeploymentFailure::Contract {
            component,
            stage,
            observed_release,
            ..
        }) => {
            let _ = writeln!(
                text,
                "Main failure: {component} / {}\nExecution checks or required evidence did not satisfy the confirmed plan.\nInspect recorded steps and current state before retrying.\nVersion observed at failure: {}",
                stage_label(*stage),
                version_label(observed_release.as_ref())
            );
        }
        None => {}
    }
}

fn stage_label(stage: OrchestrationStage) -> String {
    step_label(match stage {
        OrchestrationStage::Prepare => "prepare",
        OrchestrationStage::Activate => "activate",
        OrchestrationStage::Rollback => "rollback",
        OrchestrationStage::Compensate => "compensate",
    })
}

const fn outcome_label(outcome: &ComponentOutcome) -> &'static str {
    match outcome {
        ComponentOutcome::Succeeded => "Succeeded",
        ComponentOutcome::Failed => "Failed",
        ComponentOutcome::Cancelled => "Cancelled",
        ComponentOutcome::Compensated => "Compensated",
        ComponentOutcome::CompensationFailed => "Compensation failed",
    }
}

fn version_label(version: Option<&ReleaseVersion>) -> &str {
    version.map_or(
        "unknown / not reported (not proof of absence)",
        ReleaseVersion::as_str,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        domain::{ComponentDeploymentResult, ComponentName, Deployment},
        drivers::DriverError,
    };

    const SECRET: &str = "raw-driver-private-secret";

    fn report(state: DeploymentState) -> RollbackReport {
        let mut deployment = Deployment::new();
        deployment.state = state;
        RollbackReport {
            deployment,
            failure: None,
            compensation_failures: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    fn component(report: &mut RollbackReport, name: &str, outcome: ComponentOutcome) {
        report.deployment.components.insert(
            ComponentName::parse(name).unwrap(),
            ComponentDeploymentResult {
                outcome,
                attempted_release: Some(ReleaseVersion::parse("v2").unwrap()),
                observed_release: Some(ReleaseVersion::parse("v1").unwrap()),
            },
        );
    }

    fn malicious_error() -> DriverError {
        DriverError {
            stage: format!("\u{1b}[31m{SECRET}"),
            target: SECRET.into(),
            message: format!("-----BEGIN PRIVATE KEY-----\n{SECRET}"),
            suggested_action: format!("run unsafe-command --token={SECRET}"),
        }
    }

    fn failure(name: &str) -> DeploymentFailure {
        DeploymentFailure::Driver {
            component: ComponentName::parse(name).unwrap(),
            stage: OrchestrationStage::Rollback,
            error: malicious_error(),
            observed_release: None,
        }
    }

    #[test]
    fn success_retains_the_deployment_id_and_component_versions() {
        let mut report = report(DeploymentState::Succeeded);
        component(&mut report, "api", ComponentOutcome::Succeeded);
        let text = rollback_result(&report);
        assert!(text.contains(&report.deployment.id.to_string()));
        assert!(text.contains("Result: Succeeded"));
        assert!(text.contains("api: Succeeded; requested version: v2; reported version: v1"));
        assert!(!text.contains("Main failure:"));
        assert!(!text.contains("WARNING:"));
    }

    #[test]
    fn cancellation_is_not_relabelled_failure_because_a_failure_detail_exists() {
        let mut report = report(DeploymentState::Cancelled);
        report.failure = Some(DeploymentFailure::Cancelled);
        component(&mut report, "api", ComponentOutcome::Cancelled);
        let text = rollback_result(&report);
        assert!(text.contains("Result: Cancelled"));
        assert!(text.contains("Cancellation was requested"));
        assert!(text.contains("does not prove that no remote effects occurred"));
        assert!(!text.contains("Result: Failed"));
        assert!(!text.contains("Rollback failed"));
    }

    #[test]
    fn failed_rollback_retains_main_failure_when_other_components_were_compensated() {
        let mut report = report(DeploymentState::Failed);
        report.failure = Some(failure("api"));
        component(&mut report, "api", ComponentOutcome::Failed);
        component(&mut report, "worker", ComponentOutcome::Compensated);
        let text = rollback_result(&report);
        assert!(text.contains("Result: Failed"));
        assert!(text.contains("Main failure: api / Rolling back"));
        assert!(text.contains("worker: Compensated"));
        assert!(text.contains("not that its requested target succeeded"));
        assert!(!text.contains("MANUAL RECOVERY REQUIRED"));
        assert!(!text.contains(SECRET));
    }

    #[test]
    fn known_driver_stage_adds_safe_advice_without_replacing_trusted_component_context() {
        let mut report = report(DeploymentState::Failed);
        let mut error = malicious_error();
        error.stage = "health".into();
        report.failure = Some(DeploymentFailure::Driver {
            component: ComponentName::parse("trusted-api").unwrap(),
            stage: OrchestrationStage::Rollback,
            error,
            observed_release: None,
        });
        let text = rollback_result(&report);
        assert!(text.contains("Main failure: trusted-api / Rolling back"));
        assert!(text.contains("Health check failed"));
        assert!(text.contains("Inspect the service and its configured health checks"));
        assert!(!text.contains(SECRET));
    }

    #[test]
    fn primary_failure_persistence_warning_and_compensation_error_precede_large_results() {
        let mut report = report(DeploymentState::Failed);
        report.failure = Some(failure("zzz-api"));
        report
            .warnings
            .push(format!("local history persistence failed: {SECRET}"));
        report.compensation_failures.insert(
            ComponentName::parse("zzz-worker").unwrap(),
            malicious_error(),
        );
        for index in 0..300 {
            component(
                &mut report,
                &format!("component-{index:03}"),
                ComponentOutcome::Compensated,
            );
        }
        component(
            &mut report,
            "zzz-worker",
            ComponentOutcome::CompensationFailed,
        );
        let text = rollback_result(&report);
        let details = text.find("Component results:").unwrap();
        for label in [
            "Main failure: zzz-api",
            "WARNING: 1",
            "MANUAL RECOVERY REQUIRED: 1",
            "Compensation failure: zzz-worker",
        ] {
            assert!(text.find(label).unwrap() < details);
        }
        assert!(text.contains("known outcomes are unchanged"));
        assert!(text.contains("zzz-worker: Compensation failed"));
        assert!(!text.contains(SECRET));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn absent_optional_versions_never_claim_undeployed_or_healthy() {
        let mut report = report(DeploymentState::Succeeded);
        report.deployment.components.insert(
            ComponentName::parse("worker").unwrap(),
            ComponentDeploymentResult {
                outcome: ComponentOutcome::Succeeded,
                attempted_release: None,
                observed_release: None,
            },
        );
        let text = rollback_result(&report);
        assert!(text.contains("reported version: unknown / not reported (not proof of absence)"));
        assert!(text.contains("not fresh service-health checks"));
        assert!(!text.contains("not_deployed"));
    }

    #[test]
    fn malicious_contract_text_and_raw_driver_targets_never_enter_the_summary() {
        let mut report = report(DeploymentState::Failed);
        report.failure = Some(DeploymentFailure::Contract {
            component: ComponentName::parse("trusted-api").unwrap(),
            stage: OrchestrationStage::Compensate,
            message: format!("\u{1b}[2J {SECRET}"),
            observed_release: Some(ReleaseVersion::parse("v3").unwrap()),
        });
        report.compensation_failures.insert(
            ComponentName::parse("trusted-worker").unwrap(),
            malicious_error(),
        );
        let text = rollback_result(&report);
        assert!(text.contains("Main failure: trusted-api / Compensating changes"));
        assert!(text.contains("Version observed at failure: v3"));
        assert!(text.contains("Inspect recorded steps and current state before retrying"));
        for forbidden in [SECRET, "unsafe-command", "PRIVATE KEY", "\u{1b}"] {
            assert!(!text.contains(forbidden));
        }
    }

    #[test]
    fn compensation_failure_after_cancellation_uses_the_actual_failed_state() {
        let mut report = report(DeploymentState::Failed);
        report.failure = Some(DeploymentFailure::Cancelled);
        report
            .compensation_failures
            .insert(ComponentName::parse("api").unwrap(), malicious_error());
        let text = rollback_result(&report);
        assert!(text.contains("Result: Failed"));
        assert!(text.contains("Cancellation was requested"));
        assert!(text.contains("MANUAL RECOVERY REQUIRED"));
    }

    #[test]
    fn incomplete_reports_do_not_claim_a_terminal_outcome() {
        for state in [DeploymentState::Created, DeploymentState::Running] {
            let text = rollback_result(&report(state));
            assert!(text.contains("Result: Not completed"));
            assert!(text.contains("No Component outcomes were reported; remote state is unknown"));
        }
    }

    #[test]
    fn complete_controlled_warnings_preserve_the_failed_persistence_operation() {
        let mut report = report(DeploymentState::Failed);
        component(&mut report, "trusted-api", ComponentOutcome::Failed);
        for (operation, expected) in PERSISTENCE_OPERATIONS {
            let warning = format!("{operation}{PERSISTENCE_SUFFIX}");
            assert_eq!(
                warning_context(&warning, &report).as_deref(),
                Some(expected)
            );
            report.warnings.push(warning);
        }
        for stage in OBSERVATION_STAGES {
            let warning =
                format!("persist {stage} observation for trusted-api{PERSISTENCE_SUFFIX}");
            let expected = format!("Could not save trusted-api / {}.", step_label(stage));
            assert_eq!(warning_context(&warning, &report), Some(expected));
            report.warnings.push(warning);
        }
        report.warnings.push(LOG_WARNING.into());
        let text = rollback_result(&report);
        assert!(text.starts_with("Result: Failed · warnings:14 · manual recovery:0\n"));
        for (_, description) in PERSISTENCE_OPERATIONS {
            assert!(text.contains(description));
        }
        assert!(text.contains("The rollback log is incomplete"));
        assert!(!text.contains("other operation/audit warning(s)"));
    }

    #[test]
    fn known_warning_prefixes_never_authorize_raw_suffixes_or_untrusted_component_names() {
        let mut report = report(DeploymentState::Failed);
        component(&mut report, "trusted-api", ComponentOutcome::Failed);
        for warning in [
            format!("record Rollback intent{PERSISTENCE_SUFFIX} {SECRET}"),
            format!("record Rollback intent: {SECRET}{PERSISTENCE_SUFFIX}"),
            format!("persist rollback-preflight observation for {SECRET}{PERSISTENCE_SUFFIX}"),
            format!(
                "persist rollback-preflight observation for trusted-api{PERSISTENCE_SUFFIX}\n{SECRET}"
            ),
            format!("{LOG_WARNING}; {SECRET}"),
        ] {
            assert_eq!(warning_context(&warning, &report), None);
            report.warnings.push(warning);
        }
        let text = rollback_result(&report);
        assert!(text.contains("5 other operation/audit warning(s)"));
        assert!(!text.contains("Could not record the rollback intent"));
        assert!(!text.contains(SECRET));
    }

    #[test]
    fn result_and_main_failure_context_have_short_dedicated_lines() {
        let mut report = report(DeploymentState::Failed);
        report.failure = Some(failure("trusted-api"));
        let text = rollback_result(&report);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], "Result: Failed · warnings:0 · manual recovery:0");
        assert_eq!(lines[2], "Main failure: trusted-api / Rolling back");
        assert!(lines[3].contains("Review the selected connection"));
    }

    #[test]
    fn first_line_keeps_authoritative_state_and_warning_counts_before_failure_details() {
        for state in [
            DeploymentState::Succeeded,
            DeploymentState::Cancelled,
            DeploymentState::Failed,
        ] {
            let mut report = report(state);
            report.failure = Some(failure("trusted-api"));
            report.warnings = vec![LOG_WARNING.into(), SECRET.into()];
            report.compensation_failures.insert(
                ComponentName::parse("trusted-worker").unwrap(),
                malicious_error(),
            );
            let text = rollback_result(&report);
            let expected = format!(
                "Result: {} · warnings:2 · manual recovery:1",
                result_label(state)
            );
            assert_eq!(text.lines().next(), Some(expected.as_str()));
            assert!(expected.chars().count() <= 78);
            assert!(text.contains("Main failure: trusted-api / Rolling back"));
            assert!(!text.contains(SECRET));
        }
    }
}
