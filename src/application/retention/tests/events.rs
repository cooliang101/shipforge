use super::*;
use crate::{
    application::step_events::tests::Records,
    telemetry::log_record::{
        LogPersistence::{Recorded, Unconfirmed},
        LogStepState::{Failed, Started, Succeeded, Unknown},
    },
};

async fn run(fixture: &Fixture, records: &Records) -> crate::application::DeploymentReport {
    DeploymentOrchestrator::new(&fixture.history, Redactor::default())
        .deploy(
            vec![fixture.component.clone()],
            std::slice::from_ref(&fixture.component.planned.context.component),
            records,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn cleanup_lifecycles_use_exact_durable_version_step_names() {
    let fixture = Fixture::new(Fault::None);
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    for version in ["v1", "v3"] {
        assert_eq!(
            records.states("api", &format!("cleanup.{version}")),
            [(Started, Recorded), (Succeeded, Recorded)]
        );
    }
    assert!(records.states("api", "cleanup").is_empty());
    assert!(records.states("api", "compensate").is_empty());
}

#[tokio::test]
async fn unresolved_cleanup_is_unknown_and_pending_not_a_completed_failure() {
    let fixture = Fixture::new(Fault::Unknown);
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        records.states("api", "cleanup.v1"),
        [(Started, Recorded), (Unknown, Unconfirmed)]
    );
    assert_eq!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(*fixture.driver.calls.lock().unwrap(), [version(1)]);
    assert!(records.states("api", "compensate").is_empty());
}

#[tokio::test]
async fn known_cleanup_with_failed_persistence_preserves_success_and_pending_intent() {
    let fixture = Fixture::new(Fault::ResultWrite);
    let records = Records::default();
    let report = run(&fixture, &records).await;
    assert_eq!(report.deployment.state, DeploymentState::Succeeded);
    assert_eq!(
        records.states("api", "cleanup.v1"),
        [(Started, Recorded), (Succeeded, Unconfirmed)]
    );
    assert_eq!(
        fixture
            .history
            .pending_intents(&report.deployment.id)
            .unwrap()
            .len(),
        1
    );
    assert!(records.states("api", "compensate").is_empty());
}

#[tokio::test]
async fn cleanup_rejected_intent_never_claims_started_and_partial_result_never_claims_success() {
    for fault in [Fault::IntentWrite, Fault::Partial] {
        let rejected = matches!(fault, Fault::IntentWrite);
        let fixture = Fixture::new(fault);
        let records = Records::default();
        let report = run(&fixture, &records).await;
        assert_eq!(report.deployment.state, DeploymentState::Succeeded);
        if rejected {
            assert!(records.states("api", "cleanup.v1").is_empty());
            assert!(fixture.driver.calls.lock().unwrap().is_empty());
        } else {
            assert_eq!(
                records.states("api", "cleanup.v1"),
                [(Started, Recorded), (Failed, Recorded)]
            );
        }
        assert!(records.states("api", "compensate").is_empty());
    }
}
