use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{
        ComponentDeploymentResult, ComponentName, ComponentOutcome, Deployment, DeploymentState,
    },
    drivers::{
        ActivationReceipt, ComponentExecutionContext, DriverError, EventSink, PreparedRelease,
        ReleasePackage, ReleaseRef,
    },
    history::{HistoryError, HistoryStore, IntentStatus},
    telemetry::Redactor,
};

use super::{PlannedComponent, clock::MonotonicClock};

#[derive(Clone, Debug)]
pub struct DeploymentComponent {
    pub planned: PlannedComponent,
    pub package: ReleasePackage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrchestrationStage {
    Prepare,
    Activate,
    Rollback,
    Compensate,
}

impl OrchestrationStage {
    pub(super) const fn intent_name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Activate => "activate",
            Self::Rollback => "rollback",
            Self::Compensate => "compensate",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentFailure {
    Cancelled,
    Driver {
        component: ComponentName,
        stage: OrchestrationStage,
        error: DriverError,
        observed_release: Option<crate::domain::ReleaseVersion>,
    },
    Contract {
        component: ComponentName,
        stage: OrchestrationStage,
        message: String,
        observed_release: Option<crate::domain::ReleaseVersion>,
    },
}

impl DeploymentFailure {
    pub(super) fn diagnostic(&self) -> String {
        match self {
            Self::Cancelled => "Deployment cancelled".into(),
            Self::Driver { error, .. } => error.to_string(),
            Self::Contract { message, .. } => message.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeploymentReport {
    pub deployment: Deployment,
    pub failure: Option<DeploymentFailure>,
    pub compensation_failures: BTreeMap<ComponentName, DriverError>,
}

#[derive(Debug)]
pub struct DeploymentOrchestrator<'a> {
    history: &'a HistoryStore,
    redactor: Redactor,
    clock: MonotonicClock,
}

impl<'a> DeploymentOrchestrator<'a> {
    #[must_use]
    pub fn new(history: &'a HistoryStore, redactor: Redactor) -> Self {
        Self {
            history,
            redactor,
            clock: MonotonicClock::default(),
        }
    }

    /// Runs one already-planned, already-packaged multi-Component Deployment.
    ///
    /// Every selected Component is prepared before activation begins. Activation
    /// follows `activation_order`; a failure compensates confirmed successful
    /// activations in their reverse actual order.
    ///
    /// # Errors
    ///
    /// Returns an error before side effects for invalid input, or when durable
    /// history cannot be written. Driver failures are returned in the report.
    pub async fn deploy(
        &self,
        components: Vec<DeploymentComponent>,
        activation_order: &[ComponentName],
        events: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<DeploymentReport, OrchestrationError> {
        let components = validate_components(components, activation_order)?;
        let mut deployment = self.start_deployment(&components)?;

        if cancellation.is_cancelled() {
            return self
                .finish_failure(deployment, components, DeploymentFailure::Cancelled, &[])
                .await;
        }

        let prepared = match self
            .prepare_all(&deployment, &components, events, cancellation)
            .await?
        {
            Ok(prepared) => prepared,
            Err(failure) => {
                return self
                    .finish_failure(deployment, components, failure, &[])
                    .await;
            }
        };
        let (activated, mut failure) = self
            .activate_all(
                &deployment,
                &components,
                &prepared,
                activation_order,
                cancellation,
            )
            .await?;

        if cancellation.is_cancelled() && failure.is_none() {
            failure = Some(DeploymentFailure::Cancelled);
        }
        if let Some(failure) = failure {
            return self
                .finish_failure(deployment, components, failure, &activated)
                .await;
        }

        for (name, component) in &components {
            let version = component.package.release().version.clone();
            deployment.components.insert(
                name.clone(),
                ComponentDeploymentResult {
                    outcome: ComponentOutcome::Succeeded,
                    attempted_release: Some(version.clone()),
                    observed_release: Some(version),
                },
            );
        }
        self.persist_results(&deployment, None, &BTreeMap::new())?;
        self.history.transition_deployment(
            &deployment.id,
            DeploymentState::Running,
            DeploymentState::Succeeded,
            self.timestamp()?,
        )?;
        deployment.succeed().map_err(OrchestrationError::Domain)?;
        Ok(DeploymentReport {
            deployment,
            failure: None,
            compensation_failures: BTreeMap::new(),
        })
    }

    fn start_deployment(
        &self,
        components: &BTreeMap<ComponentName, DeploymentComponent>,
    ) -> Result<Deployment, OrchestrationError> {
        let Some(first) = components.values().next() else {
            return Err(OrchestrationError::InvalidInput(
                "at least one Component must be selected".into(),
            ));
        };
        let mut deployment = Deployment::new();
        self.history.create_deployment(
            &deployment.id,
            &first.planned.context.project_id,
            &first.planned.context.environment_id,
            self.timestamp()?,
        )?;
        self.history.transition_deployment(
            &deployment.id,
            DeploymentState::Created,
            DeploymentState::Running,
            self.timestamp()?,
        )?;
        deployment.start().map_err(OrchestrationError::Domain)?;
        Ok(deployment)
    }

    async fn prepare_all(
        &self,
        deployment: &Deployment,
        components: &BTreeMap<ComponentName, DeploymentComponent>,
        events: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<
        Result<BTreeMap<ComponentName, PreparedRelease>, DeploymentFailure>,
        OrchestrationError,
    > {
        let mut prepared = BTreeMap::new();
        for (name, component) in components {
            if cancellation.is_cancelled() {
                return Ok(Err(DeploymentFailure::Cancelled));
            }
            match self.prepare(deployment, name, component, events).await? {
                Ok(receipt) => {
                    prepared.insert(name.clone(), receipt);
                }
                Err(error) => {
                    if cancellation.is_cancelled() {
                        return Ok(Err(DeploymentFailure::Cancelled));
                    }
                    return Ok(Err(DeploymentFailure::Driver {
                        component: name.clone(),
                        stage: OrchestrationStage::Prepare,
                        error,
                        observed_release: component
                            .planned
                            .plan
                            .expected_current
                            .as_ref()
                            .map(|release| release.version.clone()),
                    }));
                }
            }
        }
        Ok(Ok(prepared))
    }

    async fn activate_all(
        &self,
        deployment: &Deployment,
        components: &BTreeMap<ComponentName, DeploymentComponent>,
        prepared: &BTreeMap<ComponentName, PreparedRelease>,
        activation_order: &[ComponentName],
        cancellation: &CancellationToken,
    ) -> Result<(Vec<ActivatedComponent>, Option<DeploymentFailure>), OrchestrationError> {
        let mut activated = Vec::new();
        for name in activation_order {
            if cancellation.is_cancelled() {
                return Ok((activated, Some(DeploymentFailure::Cancelled)));
            }
            let component = &components[name];
            let receipt = &prepared[name];
            match self.activate(deployment, name, component, receipt).await? {
                Ok(activation) => {
                    let points_to_candidate = activation.current.as_ref() == Some(&receipt.release);
                    if points_to_candidate {
                        activated.push(ActivatedComponent {
                            name: name.clone(),
                            previous: component.planned.plan.expected_current.clone(),
                            observed: activation.current.clone(),
                        });
                    } else {
                        return Ok((
                            activated,
                            Some(DeploymentFailure::Contract {
                                component: name.clone(),
                                stage: OrchestrationStage::Activate,
                                message: "Driver activation receipt does not point to the candidate Release"
                                    .into(),
                                observed_release: activation.current.map(|release| release.version),
                            }),
                        ));
                    }
                    if !activation.healthy {
                        return Ok((
                            activated,
                            Some(DeploymentFailure::Contract {
                                component: name.clone(),
                                stage: OrchestrationStage::Activate,
                                message: "Driver returned an unhealthy activation receipt".into(),
                                observed_release: Some(receipt.release.version.clone()),
                            }),
                        ));
                    }
                }
                Err(error) => {
                    let observed = component
                        .planned
                        .driver
                        .current(&component.planned.context)
                        .await
                        .ok()
                        .flatten();
                    if observed.as_ref() == Some(&receipt.release) {
                        activated.push(ActivatedComponent {
                            name: name.clone(),
                            previous: component.planned.plan.expected_current.clone(),
                            observed: observed.clone(),
                        });
                    }
                    let failure = if cancellation.is_cancelled() {
                        DeploymentFailure::Cancelled
                    } else {
                        DeploymentFailure::Driver {
                            component: name.clone(),
                            stage: OrchestrationStage::Activate,
                            error,
                            observed_release: observed.map(|release| release.version),
                        }
                    };
                    return Ok((activated, Some(failure)));
                }
            }
        }
        Ok((activated, None))
    }

    async fn prepare(
        &self,
        deployment: &Deployment,
        name: &ComponentName,
        component: &DeploymentComponent,
        events: &dyn EventSink,
    ) -> Result<Result<PreparedRelease, DriverError>, OrchestrationError> {
        let intent = self.history.record_intent(
            &deployment.id,
            name,
            OrchestrationStage::Prepare.intent_name(),
            component.package.release().version.as_str(),
            self.timestamp()?,
        )?;
        let mut result = component
            .planned
            .driver
            .prepare(
                &deployment.id,
                &component.planned.context,
                &component.planned.plan,
                &component.package,
                events,
            )
            .await;
        if result
            .as_ref()
            .is_ok_and(|receipt| !prepared_matches(component, &receipt.release))
        {
            result = Err(contract_driver_error(
                "prepare",
                name,
                "Driver returned a Release identity different from the frozen plan",
            ));
        }
        let outcome = result.as_ref().map(|_| ()).map_err(Clone::clone);
        self.complete_intent(intent, &outcome)?;
        Ok(result)
    }

    async fn activate(
        &self,
        deployment: &Deployment,
        name: &ComponentName,
        component: &DeploymentComponent,
        prepared: &PreparedRelease,
    ) -> Result<Result<ActivationReceipt, DriverError>, OrchestrationError> {
        let intent = self.history.record_intent(
            &deployment.id,
            name,
            OrchestrationStage::Activate.intent_name(),
            prepared.release.version.as_str(),
            self.timestamp()?,
        )?;
        let result = component
            .planned
            .driver
            .activate(
                &deployment.id,
                &component.planned.context,
                &prepared.release,
            )
            .await;
        let outcome = match &result {
            Ok(receipt)
                if receipt.current.as_ref() == Some(&prepared.release) && receipt.healthy =>
            {
                Ok(())
            }
            Ok(_) => Err(contract_driver_error(
                "activate",
                name,
                "Driver activation receipt is unhealthy or does not point to the candidate Release",
            )),
            Err(error) => Err(error.clone()),
        };
        self.complete_intent(intent, &outcome)?;
        Ok(result)
    }

    fn complete_intent(
        &self,
        intent: crate::history::IntentId,
        outcome: &Result<(), DriverError>,
    ) -> Result<(), OrchestrationError> {
        let (status, error) = match &outcome {
            Ok(()) => (IntentStatus::Succeeded, None),
            Err(error) => (IntentStatus::Failed, Some(error.to_string())),
        };
        self.history.complete_intent(
            intent,
            status,
            error.as_deref(),
            self.timestamp()?,
            &self.redactor,
        )?;
        Ok(())
    }

    async fn finish_failure(
        &self,
        mut deployment: Deployment,
        components: BTreeMap<ComponentName, DeploymentComponent>,
        failure: DeploymentFailure,
        activated: &[ActivatedComponent],
    ) -> Result<DeploymentReport, OrchestrationError> {
        let mut component_results = BTreeMap::new();
        let compensation_failures = self
            .compensate(&deployment, &components, activated, &mut component_results)
            .await?;
        deployment.components = component_results;
        let failed_component = failure_component(&failure);
        for (name, component) in &components {
            deployment
                .components
                .entry(name.clone())
                .or_insert_with(|| {
                    let outcome = if failed_component == Some(name) {
                        ComponentOutcome::Failed
                    } else {
                        ComponentOutcome::Cancelled
                    };
                    let observed_release = failure_observed(&failure, name).or_else(|| {
                        component
                            .planned
                            .plan
                            .expected_current
                            .as_ref()
                            .map(|release| release.version.clone())
                    });
                    ComponentDeploymentResult {
                        outcome,
                        attempted_release: Some(component.package.release().version.clone()),
                        observed_release,
                    }
                });
        }
        self.persist_results(
            &deployment,
            Some(&failure.diagnostic()),
            &compensation_failures,
        )?;
        let terminal = if matches!(failure, DeploymentFailure::Cancelled)
            && !deployment
                .components
                .values()
                .any(|result| result.outcome == ComponentOutcome::CompensationFailed)
        {
            DeploymentState::Cancelled
        } else {
            DeploymentState::Failed
        };
        self.history.transition_deployment(
            &deployment.id,
            DeploymentState::Running,
            terminal,
            self.timestamp()?,
        )?;
        match terminal {
            DeploymentState::Cancelled => deployment.cancel(),
            DeploymentState::Failed => deployment.fail(),
            _ => unreachable!(),
        }
        .map_err(OrchestrationError::Domain)?;
        Ok(DeploymentReport {
            deployment,
            failure: Some(failure),
            compensation_failures,
        })
    }

    async fn compensate(
        &self,
        deployment: &Deployment,
        components: &BTreeMap<ComponentName, DeploymentComponent>,
        activated: &[ActivatedComponent],
        results: &mut BTreeMap<ComponentName, ComponentDeploymentResult>,
    ) -> Result<BTreeMap<ComponentName, DriverError>, OrchestrationError> {
        let mut failures = BTreeMap::new();
        for activated in activated.iter().rev() {
            let component = &components[&activated.name];
            let intent = self.history.record_intent(
                &deployment.id,
                &activated.name,
                OrchestrationStage::Compensate.intent_name(),
                component.package.release().version.as_str(),
                self.timestamp()?,
            )?;
            let context = recovery_context(&component.planned.context);
            let rollback = component
                .planned
                .driver
                .rollback(&deployment.id, &context, activated.previous.as_ref())
                .await;
            let intent_outcome = rollback.as_ref().map(|_| ()).map_err(Clone::clone);
            self.complete_intent(intent, &intent_outcome)?;
            let (outcome, observed) = match rollback {
                Ok(receipt) if receipt.current == activated.previous => (
                    ComponentOutcome::Compensated,
                    receipt.current.map(|release| release.version),
                ),
                Ok(receipt) => {
                    failures.insert(
                        activated.name.clone(),
                        contract_driver_error(
                            "compensate",
                            &activated.name,
                            "Driver rollback receipt differs from the pre-activation Release",
                        ),
                    );
                    (
                        ComponentOutcome::CompensationFailed,
                        receipt.current.map(|release| release.version),
                    )
                }
                Err(error) => {
                    failures.insert(activated.name.clone(), error);
                    (
                        ComponentOutcome::CompensationFailed,
                        activated
                            .observed
                            .as_ref()
                            .map(|release| release.version.clone()),
                    )
                }
            };
            results.insert(
                activated.name.clone(),
                ComponentDeploymentResult {
                    outcome,
                    attempted_release: Some(component.package.release().version.clone()),
                    observed_release: observed,
                },
            );
        }
        Ok(failures)
    }

    fn persist_results(
        &self,
        deployment: &Deployment,
        error: Option<&str>,
        compensation_failures: &BTreeMap<ComponentName, DriverError>,
    ) -> Result<(), OrchestrationError> {
        for (component, result) in &deployment.components {
            let compensation_error = compensation_failures
                .get(component)
                .map(ToString::to_string);
            let diagnostic = compensation_error.as_deref().or_else(|| {
                (result.outcome != ComponentOutcome::Succeeded)
                    .then_some(error)
                    .flatten()
            });
            self.history.record_component_result(
                &deployment.id,
                component,
                result,
                diagnostic,
                &self.redactor,
            )?;
        }
        Ok(())
    }

    fn timestamp(&self) -> Result<u64, OrchestrationError> {
        self.clock.timestamp()
    }
}

#[derive(Clone, Debug)]
struct ActivatedComponent {
    name: ComponentName,
    previous: Option<ReleaseRef>,
    observed: Option<ReleaseRef>,
}

fn validate_components(
    components: Vec<DeploymentComponent>,
    activation_order: &[ComponentName],
) -> Result<BTreeMap<ComponentName, DeploymentComponent>, OrchestrationError> {
    let mut mapped: BTreeMap<ComponentName, DeploymentComponent> = BTreeMap::new();
    for component in components {
        let context = &component.planned.context;
        let release = component.package.release();
        if release != &component.planned.plan.release
            || release.component != context.component
            || release.project_id != context.project_id
            || release.environment_id != context.environment_id
        {
            return Err(OrchestrationError::InvalidInput(
                "planned context, plan, and Release package identities must match".into(),
            ));
        }
        if let Some(first) = mapped.values().next()
            && (context.project_id != first.planned.context.project_id
                || context.environment_id != first.planned.context.environment_id)
        {
            return Err(OrchestrationError::InvalidInput(
                "all Components must belong to one Project and Environment".into(),
            ));
        }
        if mapped
            .insert(context.component.clone(), component)
            .is_some()
        {
            return Err(OrchestrationError::InvalidInput(
                "a Component may appear only once".into(),
            ));
        }
    }
    if mapped.is_empty() {
        return Err(OrchestrationError::InvalidInput(
            "at least one Component must be selected".into(),
        ));
    }
    let ordered = activation_order.iter().cloned().collect::<BTreeSet<_>>();
    if ordered.len() != activation_order.len() || ordered != mapped.keys().cloned().collect() {
        return Err(OrchestrationError::InvalidInput(
            "activation order must contain every selected Component exactly once".into(),
        ));
    }
    Ok(mapped)
}

fn recovery_context(context: &ComponentExecutionContext) -> ComponentExecutionContext {
    let mut context = context.clone();
    context.cancellation = CancellationToken::new();
    context
}

fn failure_component(failure: &DeploymentFailure) -> Option<&ComponentName> {
    match failure {
        DeploymentFailure::Driver { component, .. }
        | DeploymentFailure::Contract { component, .. } => Some(component),
        DeploymentFailure::Cancelled => None,
    }
}

fn failure_observed(
    failure: &DeploymentFailure,
    component: &ComponentName,
) -> Option<crate::domain::ReleaseVersion> {
    match failure {
        DeploymentFailure::Driver {
            component: failed,
            observed_release,
            ..
        }
        | DeploymentFailure::Contract {
            component: failed,
            observed_release,
            ..
        } if failed == component => observed_release.clone(),
        _ => None,
    }
}

fn contract_driver_error(stage: &str, component: &ComponentName, message: &str) -> DriverError {
    DriverError {
        stage: stage.into(),
        target: component.to_string(),
        message: message.into(),
        suggested_action: "inspect the Driver implementation before retrying".into(),
    }
}

#[cfg(test)]
mod tests;

fn prepared_matches(component: &DeploymentComponent, release: &ReleaseRef) -> bool {
    let context = &component.planned.context;
    let planned = &component.planned.plan;
    release.driver == *context.target.driver_kind()
        && release.project_id == planned.release.project_id
        && release.environment_id == planned.release.environment_id
        && release.component == planned.release.component
        && release.generation == planned.release.generation
        && release.version == planned.release.version
        && release.destination == planned.release.destination
        && release.destination_revision == planned.release.destination_revision
        && release.endpoint_fingerprint == context.endpoint_fingerprint
        && release.effective_capabilities == planned.effective_capabilities
}

#[derive(Debug, Error)]
pub enum OrchestrationError {
    #[error("invalid Deployment input: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("Deployment state error: {0}")]
    Domain(#[from] crate::domain::DeploymentError),
    #[error("system clock error: {0}")]
    Clock(String),
}
