use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use crate::{
    domain::{
        Capability, ComponentDeploymentResult, ComponentName, ComponentOutcome, Deployment,
        DeploymentId, DeploymentState,
    },
    drivers::{
        ActivationReceipt, ComponentExecutionContext, DeploymentDriver, DriverError, ReleaseRef,
    },
    history::{HistoryStore, IntentStatus},
    telemetry::Redactor,
};

use super::{DeploymentFailure, OrchestrationError, OrchestrationStage, clock::MonotonicClock};

const RECOVERY_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct RollbackComponent {
    pub driver: Arc<dyn DeploymentDriver>,
    pub context: ComponentExecutionContext,
    pub expected_current: ReleaseRef,
    pub target: Option<ReleaseRef>,
}

#[derive(Clone, Debug)]
pub struct RollbackReport {
    pub deployment: Deployment,
    pub failure: Option<DeploymentFailure>,
    pub compensation_failures: BTreeMap<ComponentName, DriverError>,
}

#[derive(Debug)]
pub struct RollbackOrchestrator<'a> {
    history: &'a HistoryStore,
    redactor: Redactor,
    clock: MonotonicClock,
}

impl<'a> RollbackOrchestrator<'a> {
    #[must_use]
    pub fn new(history: &'a HistoryStore, redactor: Redactor) -> Self {
        Self {
            history,
            redactor,
            clock: MonotonicClock::default(),
        }
    }

    /// Runs an explicit Rollback Deployment linked to its source Deployment.
    ///
    /// Components execute in reverse activation order. If a later Component
    /// fails, already changed Components are restored in reverse actual order.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or a durable-history failure. Driver
    /// and contract failures are represented in the returned report.
    pub async fn rollback(
        &self,
        source_deployment: &DeploymentId,
        components: Vec<RollbackComponent>,
        activation_order: &[ComponentName],
        cancellation: &CancellationToken,
    ) -> Result<RollbackReport, OrchestrationError> {
        let mut components = validate_components(components, activation_order)?;
        for component in components.values_mut() {
            component.context.cancellation = cancellation.clone();
        }
        let mut deployment = self.start(source_deployment, &components)?;
        if let Some(failure) = self
            .preflight(&components, activation_order, cancellation)
            .await
        {
            return self
                .finish_failure(deployment, components, failure, &[])
                .await;
        }

        let (applied, mut failure) = self
            .apply(&deployment, &components, activation_order, cancellation)
            .await?;
        if failure.is_none() && cancellation.is_cancelled() {
            failure = Some(DeploymentFailure::Cancelled);
        }
        if let Some(failure) = failure {
            return self
                .finish_failure(deployment, components, failure, &applied)
                .await;
        }

        for component in components.values() {
            let name = component.context.component.clone();
            deployment.components.insert(
                name,
                ComponentDeploymentResult {
                    outcome: ComponentOutcome::Succeeded,
                    attempted_release: target_version(component),
                    observed_release: target_version(component),
                },
            );
        }
        self.persist_results(&deployment, None, &BTreeMap::new())?;
        self.transition_terminal(&mut deployment, DeploymentState::Succeeded)?;
        Ok(RollbackReport {
            deployment,
            failure: None,
            compensation_failures: BTreeMap::new(),
        })
    }

    fn start(
        &self,
        source: &DeploymentId,
        components: &BTreeMap<ComponentName, RollbackComponent>,
    ) -> Result<Deployment, OrchestrationError> {
        let Some(first) = components.values().next() else {
            return Err(OrchestrationError::InvalidInput(
                "at least one Component must be selected".into(),
            ));
        };
        let mut deployment = Deployment::new();
        self.history.create_rollback_deployment(
            &deployment.id,
            source,
            &first.context.project_id,
            &first.context.environment_id,
            self.clock.timestamp()?,
        )?;
        self.history.transition_deployment(
            &deployment.id,
            DeploymentState::Created,
            DeploymentState::Running,
            self.clock.timestamp()?,
        )?;
        deployment.start()?;
        Ok(deployment)
    }

    async fn preflight(
        &self,
        components: &BTreeMap<ComponentName, RollbackComponent>,
        activation_order: &[ComponentName],
        cancellation: &CancellationToken,
    ) -> Option<DeploymentFailure> {
        for name in activation_order.iter().rev() {
            if cancellation.is_cancelled() {
                return Some(DeploymentFailure::Cancelled);
            }
            let component = &components[name];
            match component.driver.current(&component.context).await {
                Ok(observed) if observed.as_ref() == Some(&component.expected_current) => {}
                Ok(observed) => {
                    return Some(DeploymentFailure::Contract {
                        component: name.clone(),
                        stage: OrchestrationStage::Rollback,
                        message: "current Release drifted before Rollback began".into(),
                        observed_release: observed.map(|release| release.version),
                    });
                }
                Err(error) => {
                    return Some(DeploymentFailure::Driver {
                        component: name.clone(),
                        stage: OrchestrationStage::Rollback,
                        error,
                        observed_release: None,
                    });
                }
            }
        }
        None
    }

    async fn apply(
        &self,
        deployment: &Deployment,
        components: &BTreeMap<ComponentName, RollbackComponent>,
        activation_order: &[ComponentName],
        cancellation: &CancellationToken,
    ) -> Result<(Vec<AppliedRollback>, Option<DeploymentFailure>), OrchestrationError> {
        let mut applied = Vec::new();
        for name in activation_order.iter().rev() {
            if cancellation.is_cancelled() {
                return Ok((applied, Some(DeploymentFailure::Cancelled)));
            }
            let component = &components[name];
            let observed = component.driver.current(&component.context).await;
            match observed {
                Ok(current) if current.as_ref() == Some(&component.expected_current) => {}
                Ok(current) => {
                    return Ok((
                        applied,
                        Some(DeploymentFailure::Contract {
                            component: name.clone(),
                            stage: OrchestrationStage::Rollback,
                            message: "current Release drifted immediately before Rollback".into(),
                            observed_release: current.map(|release| release.version),
                        }),
                    ));
                }
                Err(error) => {
                    return Ok((
                        applied,
                        Some(DeploymentFailure::Driver {
                            component: name.clone(),
                            stage: OrchestrationStage::Rollback,
                            error,
                            observed_release: None,
                        }),
                    ));
                }
            }
            let failure = self
                .apply_one(deployment, name, component, cancellation, &mut applied)
                .await?;
            if failure.is_some() {
                return Ok((applied, failure));
            }
        }
        Ok((applied, None))
    }

    async fn apply_one(
        &self,
        deployment: &Deployment,
        name: &ComponentName,
        component: &RollbackComponent,
        cancellation: &CancellationToken,
        applied: &mut Vec<AppliedRollback>,
    ) -> Result<Option<DeploymentFailure>, OrchestrationError> {
        let intent = self.history.record_intent(
            &deployment.id,
            name,
            OrchestrationStage::Rollback.intent_name(),
            component
                .target
                .as_ref()
                .map_or("not_deployed", |release| release.version.as_str()),
            self.clock.timestamp()?,
        )?;
        let context = execution_context(&component.context, cancellation.clone());
        let result = component
            .driver
            .rollback(
                &deployment.id,
                &context,
                Some(&component.expected_current),
                component.target.as_ref(),
            )
            .await;
        let valid = result
            .as_ref()
            .is_ok_and(|receipt| receipt.current == component.target && receipt.healthy);
        self.complete_intent(
            intent,
            result.as_ref().map(|_| ()).map_err(Clone::clone),
            valid,
        )?;
        if valid {
            applied.push(AppliedRollback::new(component));
            return Ok(None);
        }
        let failure = failed_rollback(component, result, cancellation, applied).await;
        Ok(Some(failure))
    }

    fn complete_intent(
        &self,
        intent: crate::history::IntentId,
        driver_result: Result<(), DriverError>,
        valid: bool,
    ) -> Result<(), OrchestrationError> {
        let error = match driver_result {
            Err(error) => Some(error.to_string()),
            Ok(()) if !valid => Some("Driver returned an invalid Rollback receipt".into()),
            Ok(()) => None,
        };
        self.history.complete_intent(
            intent,
            if error.is_some() {
                IntentStatus::Failed
            } else {
                IntentStatus::Succeeded
            },
            error.as_deref(),
            self.clock.timestamp()?,
            &self.redactor,
        )?;
        Ok(())
    }

    async fn finish_failure(
        &self,
        mut deployment: Deployment,
        components: BTreeMap<ComponentName, RollbackComponent>,
        failure: DeploymentFailure,
        applied: &[AppliedRollback],
    ) -> Result<RollbackReport, OrchestrationError> {
        let (mut results, compensation_failures) =
            self.compensate(&deployment, &components, applied).await?;
        let failed_component = failure_component(&failure);
        for (name, component) in &components {
            results.entry(name.clone()).or_insert_with(|| {
                let outcome = if failed_component == Some(name) {
                    ComponentOutcome::Failed
                } else {
                    ComponentOutcome::Cancelled
                };
                // Planned state is not a post-failure observation.
                let observed_release = failure_observed(&failure, name);
                ComponentDeploymentResult {
                    outcome,
                    attempted_release: target_version(component),
                    observed_release,
                }
            });
        }
        deployment.components = results;
        self.persist_results(
            &deployment,
            Some(&failure.diagnostic()),
            &compensation_failures,
        )?;
        let terminal = if matches!(failure, DeploymentFailure::Cancelled)
            && compensation_failures.is_empty()
        {
            DeploymentState::Cancelled
        } else {
            DeploymentState::Failed
        };
        self.transition_terminal(&mut deployment, terminal)?;
        Ok(RollbackReport {
            deployment,
            failure: Some(failure),
            compensation_failures,
        })
    }

    async fn compensate(
        &self,
        deployment: &Deployment,
        components: &BTreeMap<ComponentName, RollbackComponent>,
        applied: &[AppliedRollback],
    ) -> Result<CompensationResult, OrchestrationError> {
        let mut results = BTreeMap::new();
        let mut failures = BTreeMap::new();
        for applied in applied.iter().rev() {
            let component = &components[&applied.name];
            let intent = self.history.record_intent(
                &deployment.id,
                &applied.name,
                OrchestrationStage::Compensate.intent_name(),
                applied.original.version.as_str(),
                self.clock.timestamp()?,
            )?;
            let context = execution_context(&component.context, CancellationToken::new());
            let result = component
                .driver
                .rollback(
                    &deployment.id,
                    &context,
                    component.target.as_ref(),
                    Some(&applied.original),
                )
                .await;
            let valid = result.as_ref().is_ok_and(|receipt| {
                receipt.current.as_ref() == Some(&applied.original) && receipt.healthy
            });
            self.complete_intent(
                intent,
                result.as_ref().map(|_| ()).map_err(Clone::clone),
                valid,
            )?;
            let result = result.and_then(|receipt| {
                if valid {
                    Ok(receipt)
                } else {
                    Err(contract_error(
                        &applied.name,
                        "compensation receipt is unhealthy or differs from original Release",
                    ))
                }
            });
            let (outcome, observed) =
                match result {
                    Ok(receipt) => (
                        ComponentOutcome::Compensated,
                        receipt.current.map(|release| release.version),
                    ),
                    Err(error) => {
                        let (error, observed) =
                            match observe_after_failure(component, RECOVERY_OBSERVATION_TIMEOUT)
                                .await
                            {
                                Ok(observed) => (error, observed.map(|release| release.version)),
                                Err(observation) => (unobserved_failure(error, &observation), None),
                            };
                        failures.insert(applied.name.clone(), error);
                        (ComponentOutcome::CompensationFailed, observed)
                    }
                };
            results.insert(
                applied.name.clone(),
                ComponentDeploymentResult {
                    outcome,
                    attempted_release: target_version(component),
                    observed_release: observed,
                },
            );
        }
        Ok((results, failures))
    }

    fn persist_results(
        &self,
        deployment: &Deployment,
        default_error: Option<&str>,
        compensation_failures: &BTreeMap<ComponentName, DriverError>,
    ) -> Result<(), OrchestrationError> {
        for (name, result) in &deployment.components {
            let specific = compensation_failures.get(name).map(ToString::to_string);
            let error = specific.as_deref().or_else(|| {
                (result.outcome != ComponentOutcome::Succeeded)
                    .then_some(default_error)
                    .flatten()
            });
            self.history.record_component_result(
                &deployment.id,
                name,
                result,
                error,
                &self.redactor,
            )?;
        }
        Ok(())
    }

    fn transition_terminal(
        &self,
        deployment: &mut Deployment,
        terminal: DeploymentState,
    ) -> Result<(), OrchestrationError> {
        self.history.transition_deployment(
            &deployment.id,
            DeploymentState::Running,
            terminal,
            self.clock.timestamp()?,
        )?;
        match terminal {
            DeploymentState::Succeeded => deployment.succeed()?,
            DeploymentState::Failed => deployment.fail()?,
            DeploymentState::Cancelled => deployment.cancel()?,
            DeploymentState::Created | DeploymentState::Running => unreachable!(),
        }
        Ok(())
    }
}

type CompensationResult = (
    BTreeMap<ComponentName, ComponentDeploymentResult>,
    BTreeMap<ComponentName, DriverError>,
);

#[derive(Clone, Debug)]
struct AppliedRollback {
    name: ComponentName,
    original: ReleaseRef,
}

impl AppliedRollback {
    fn new(component: &RollbackComponent) -> Self {
        Self {
            name: component.context.component.clone(),
            original: component.expected_current.clone(),
        }
    }
}

fn validate_components(
    components: Vec<RollbackComponent>,
    activation_order: &[ComponentName],
) -> Result<BTreeMap<ComponentName, RollbackComponent>, OrchestrationError> {
    let mut mapped: BTreeMap<ComponentName, RollbackComponent> = BTreeMap::new();
    for component in components {
        validate_component(&component)?;
        if let Some(first) = mapped.values().next()
            && (component.context.project_id != first.context.project_id
                || component.context.environment_id != first.context.environment_id)
        {
            return Err(OrchestrationError::InvalidInput(
                "all Rollback Components must belong to one Project and Environment".into(),
            ));
        }
        if mapped
            .insert(component.context.component.clone(), component)
            .is_some()
        {
            return Err(OrchestrationError::InvalidInput(
                "a Rollback Component may appear only once".into(),
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
            "activation order must contain every Rollback Component exactly once".into(),
        ));
    }
    Ok(mapped)
}

fn validate_component(component: &RollbackComponent) -> Result<(), OrchestrationError> {
    if component.driver.kind() != *component.context.target.driver_kind()
        || !component
            .driver
            .static_capabilities()
            .contains(Capability::Rollback)
        || !matches_context(&component.context, &component.expected_current)
        || component
            .target
            .as_ref()
            .is_some_and(|target| !matches_context(&component.context, target))
        || component.target.as_ref() == Some(&component.expected_current)
    {
        return Err(OrchestrationError::InvalidInput(
            "Rollback Driver, context, current Release, target, or capability is inconsistent"
                .into(),
        ));
    }
    Ok(())
}

fn matches_context(context: &ComponentExecutionContext, release: &ReleaseRef) -> bool {
    release.driver == *context.target.driver_kind()
        && release.project_id == context.project_id
        && release.environment_id == context.environment_id
        && release.component == context.component
        && release.generation == context.generation
        && release.destination == context.destination
        && release.destination_revision == context.destination_revision
        && release.endpoint_fingerprint == context.endpoint_fingerprint
        && release
            .effective_capabilities
            .contains(Capability::Rollback)
}

fn execution_context(
    context: &ComponentExecutionContext,
    cancellation: CancellationToken,
) -> ComponentExecutionContext {
    let mut context = context.clone();
    context.cancellation = cancellation;
    context
}

async fn failed_rollback(
    component: &RollbackComponent,
    result: Result<ActivationReceipt, DriverError>,
    cancellation: &CancellationToken,
    applied: &mut Vec<AppliedRollback>,
) -> DeploymentFailure {
    let name = &component.context.component;
    let was_error = result.is_err();
    let error = result.err().unwrap_or_else(|| DriverError {
        stage: "rollback".into(),
        target: name.to_string(),
        message: "Driver Rollback receipt is unhealthy or differs from the requested target".into(),
        suggested_action: "inspect the current Release and restore it manually".into(),
    });
    match observe_after_failure(component, RECOVERY_OBSERVATION_TIMEOUT).await {
        Ok(observed) => {
            // Only a successful observation can prove that the target, including
            // not_deployed, was reached. An I/O error must never stand in for None.
            if observed == component.target {
                applied.push(AppliedRollback::new(component));
            }
            if was_error
                && cancellation.is_cancelled()
                && (observed == component.target
                    || observed.as_ref() == Some(&component.expected_current))
            {
                DeploymentFailure::Cancelled
            } else {
                DeploymentFailure::Driver {
                    component: name.clone(),
                    stage: OrchestrationStage::Rollback,
                    error,
                    observed_release: observed.map(|release| release.version),
                }
            }
        }
        Err(observation) => DeploymentFailure::Driver {
            component: name.clone(),
            stage: OrchestrationStage::Rollback,
            error: unobserved_failure(error, &observation),
            observed_release: None,
        },
    }
}

async fn observe_after_failure(
    component: &RollbackComponent,
    timeout: Duration,
) -> Result<Option<ReleaseRef>, DriverError> {
    let context = execution_context(&component.context, CancellationToken::new());
    if let Ok(result) = tokio::time::timeout(timeout, component.driver.current(&context)).await {
        result
    } else {
        context.cancellation.cancel();
        Err(DriverError {
            stage: "observe".into(),
            target: context.component.to_string(),
            message: "post-failure Rollback observation timed out".into(),
            suggested_action: "inspect the remote current Release and service before retrying"
                .into(),
        })
    }
}

fn unobserved_failure(mut original: DriverError, observation: &DriverError) -> DriverError {
    original.message = format!(
        "{}; current state is unknown because post-failure observation failed: {}",
        original.message, observation
    );
    original.suggested_action = format!(
        "{}; inspect the remote current Release and service before retrying; {}",
        original.suggested_action, observation.suggested_action
    );
    original
}

fn target_version(component: &RollbackComponent) -> Option<crate::domain::ReleaseVersion> {
    component
        .target
        .as_ref()
        .map(|release| release.version.clone())
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

fn contract_error(component: &ComponentName, message: &str) -> DriverError {
    DriverError {
        stage: "compensate".into(),
        target: component.to_string(),
        message: message.into(),
        suggested_action: "inspect the current Release and restore it manually".into(),
    }
}

#[cfg(test)]
mod tests;
