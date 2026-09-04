use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

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

const RECOVERY_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Local diagnostics must not replace known remote outcomes or recovery guidance.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct DeploymentOrchestrator<'a> {
    history: &'a HistoryStore,
    redactor: Redactor,
    clock: MonotonicClock,
    history_warnings: RefCell<Vec<String>>,
    driver_warnings: RefCell<Vec<String>>,
}

impl<'a> DeploymentOrchestrator<'a> {
    pub(super) fn clock(&self) -> &MonotonicClock {
        &self.clock
    }

    #[must_use]
    pub fn new(history: &'a HistoryStore, redactor: Redactor) -> Self {
        Self {
            history,
            redactor,
            clock: MonotonicClock::default(),
            history_warnings: RefCell::new(Vec::new()),
            driver_warnings: RefCell::new(Vec::new()),
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
        let checked = validate_components(components.clone(), activation_order)?;
        let deployment = self.start_deployment(&checked)?;
        let setup = (|| {
            self.history.record_deployment_metadata(
                &deployment.id,
                &crate::history::DeploymentMetadata {
                    git_branch: None,
                    git_revision: None,
                    git_worktree: crate::history::GitWorktree::Unknown,
                    operator: std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok(),
                },
                &self.redactor,
            )?;
            self.snapshot_components(
                &deployment.id,
                activation_order.iter().map(|name| &checked[name].planned),
                false,
            )?;
            Ok::<(), OrchestrationError>(())
        })();
        if let Err(error) = setup {
            return Err(self.before_effect_error(&deployment.id, error));
        }
        self.deploy_started(
            deployment,
            components,
            activation_order,
            events,
            cancellation,
        )
        .await
    }

    pub(super) async fn deploy_started(
        &self,
        mut deployment: Deployment,
        components: Vec<DeploymentComponent>,
        activation_order: &[ComponentName],
        events: &dyn EventSink,
        cancellation: &CancellationToken,
    ) -> Result<DeploymentReport, OrchestrationError> {
        let mut components = validate_components(components, activation_order)?;
        // Planning and execution are separate TUI operations. Drivers must use
        // the execution token, not the token retained by the read-only plan.
        for component in components.values_mut() {
            component.planned.context.cancellation = cancellation.clone();
        }
        for component in components.values() {
            if let Err(error) = self.record_package(&deployment.id, component) {
                return Err(self.before_effect_error(&deployment.id, error));
            }
        }

        if cancellation.is_cancelled() {
            return self
                .finish_failure(
                    deployment,
                    components,
                    DeploymentFailure::Cancelled,
                    &[],
                    events,
                )
                .await;
        }

        let prepared = match self
            .prepare_all(&deployment, &components, events, cancellation)
            .await?
        {
            Ok(prepared) => prepared,
            Err(failure) => {
                return self
                    .finish_failure(deployment, components, failure, &[], events)
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
                events,
            )
            .await?;

        if cancellation.is_cancelled() && failure.is_none() {
            failure = Some(DeploymentFailure::Cancelled);
        }
        if let Some(failure) = failure {
            return self
                .finish_failure(deployment, components, failure, &activated, events)
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
        self.persist_results(&deployment, None, &BTreeMap::new());
        self.remember_history_error(
            self.history
                .transition_deployment(
                    &deployment.id,
                    DeploymentState::Running,
                    DeploymentState::Succeeded,
                    self.timestamp()?,
                )
                .map_err(Into::into),
        );
        deployment.succeed().map_err(OrchestrationError::Domain)?;
        Ok(DeploymentReport {
            deployment,
            failure: None,
            compensation_failures: BTreeMap::new(),
            warnings: self.take_warnings(),
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
        self.start_for_context(&first.planned.context)
    }

    pub(super) fn start_for_context(
        &self,
        context: &ComponentExecutionContext,
    ) -> Result<Deployment, OrchestrationError> {
        self.history_warnings.borrow_mut().clear();
        self.driver_warnings.borrow_mut().clear();
        let mut deployment = Deployment::new();
        self.history.create_deployment(
            &deployment.id,
            &context.project_id,
            &context.environment_id,
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

    pub(super) fn snapshot_components<'b>(
        &self,
        deployment: &crate::domain::DeploymentId,
        components: impl IntoIterator<Item = &'b PlannedComponent>,
        includes_build: bool,
    ) -> Result<(), OrchestrationError> {
        let snapshots = components
            .into_iter()
            .enumerate()
            .map(|(index, planned)| {
                let release = planned_release_ref(planned);
                Ok(crate::history::DeploymentComponentSnapshot {
                    release: release.clone(),
                    expected_current: planned.plan.expected_current.clone(),
                    target: Some(release),
                    execution_order: u32::try_from(index).map_err(|_| {
                        OrchestrationError::InvalidInput("too many selected Components".into())
                    })?,
                })
            })
            .collect::<Result<Vec<_>, OrchestrationError>>()?;
        self.history
            .record_component_snapshots(deployment, &snapshots)?;
        for snapshot in &snapshots {
            let steps: &[&str] = if includes_build {
                &["build-package", "prepare", "activate", "compensate"]
            } else {
                &["prepare", "activate", "compensate"]
            };
            self.history
                .plan_steps(deployment, &snapshot.release.component, steps)?;
        }
        Ok(())
    }

    pub(super) fn record_package(
        &self,
        deployment: &crate::domain::DeploymentId,
        component: &DeploymentComponent,
    ) -> Result<(), OrchestrationError> {
        self.history.record_release_package(
            deployment,
            &planned_release_ref(&component.planned),
            component.package.manifest(),
            component.package.sha256(),
            component.package.size(),
        )?;
        Ok(())
    }

    fn before_effect_error(
        &self,
        deployment: &crate::domain::DeploymentId,
        source: OrchestrationError,
    ) -> OrchestrationError {
        let persistence = self
            .timestamp()
            .and_then(|timestamp| {
                self.history
                    .transition_deployment(
                        deployment,
                        DeploymentState::Running,
                        DeploymentState::Failed,
                        timestamp,
                    )
                    .map_err(Into::into)
            })
            .err()
            .map(|error| error.to_string());
        OrchestrationError::Execution {
            deployment: deployment.clone(),
            source: Box::new(source),
            persistence,
        }
    }

    // These writes follow a remote effect. Retain its in-memory receipt and
    // allow compensation even when auxiliary history cannot be persisted.
    fn remember_history_error(&self, result: Result<(), OrchestrationError>) {
        if let Err(error) = result {
            self.history_warnings.borrow_mut().push(self.redactor.redact(&format!(
                "Local history incomplete: {error}. Inspect history and remote state before retrying."
            )));
        }
    }

    fn observation(
        &self,
        deployment: &crate::domain::DeploymentId,
        component: &ComponentName,
        stage: &str,
        observed: &Result<Option<ReleaseRef>, DriverError>,
        healthy: Option<bool>,
    ) {
        let diagnostic = observed.as_ref().err().map(ToString::to_string);
        let observed = match observed {
            Ok(release) => Ok(release.as_ref()),
            Err(_) => Err(diagnostic.as_deref().unwrap_or("observation failed")),
        };
        self.remember_history_error(self.timestamp().and_then(|timestamp| {
            self.history
                .record_observation(
                    deployment,
                    component,
                    stage,
                    observed,
                    healthy,
                    timestamp,
                    &self.redactor,
                )
                .map_err(Into::into)
        }));
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
                        observed_release: None,
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
        events: &dyn EventSink,
    ) -> Result<(Vec<ActivatedComponent>, Option<DeploymentFailure>), OrchestrationError> {
        let mut activated = Vec::new();
        for name in activation_order {
            if cancellation.is_cancelled() {
                return Ok((activated, Some(DeploymentFailure::Cancelled)));
            }
            let component = &components[name];
            let receipt = &prepared[name];
            events.emit(crate::drivers::DriverLog {
                namespace: "activate.started".into(),
                message: format!("Activating {name} and checking health"),
            });
            let result = match self.activate(deployment, name, component, receipt).await {
                Ok(result) => result,
                Err(error) => {
                    let message = format!(
                        "Local activation intent could not be persisted; no activation attempted for {name}: {error}"
                    );
                    self.remember_history_error(Err(error));
                    return Ok((
                        activated,
                        Some(DeploymentFailure::Contract {
                            component: name.clone(),
                            stage: OrchestrationStage::Activate,
                            message,
                            observed_release: None,
                        }),
                    ));
                }
            };
            match result {
                Ok(activation) => {
                    events.emit(crate::drivers::DriverLog {
                        namespace: "activate.finished".into(),
                        message: format!(
                            "{name}: activation returned; healthy={}",
                            activation.healthy
                        ),
                    });
                    let points_to_candidate = activation.current.as_ref() == Some(&receipt.release);
                    if points_to_candidate {
                        activated.push(ActivatedComponent {
                            name: name.clone(),
                            previous: component.planned.plan.expected_current.clone(),
                            observed: activation.current.clone(),
                        });
                    } else {
                        let failure = self
                            .invalid_activation_receipt(
                                deployment,
                                component,
                                receipt,
                                &mut activated,
                            )
                            .await;
                        return Ok((activated, Some(failure)));
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
                    let observed =
                        observe_after_failure(component, RECOVERY_OBSERVATION_TIMEOUT).await;
                    self.observation(&deployment.id, name, "activate.failure", &observed, None);
                    let failure = activation_failure(
                        component,
                        receipt,
                        error,
                        cancellation,
                        &mut activated,
                        observed,
                    );
                    return Ok((activated, Some(failure)));
                }
            }
            if !self.history_warnings.borrow().is_empty() {
                return Ok((activated, Some(DeploymentFailure::Contract {
                    component: name.clone(),
                    stage: OrchestrationStage::Activate,
                    message: "Local history could not record the activation; stopped forward deployment and requested compensation".into(),
                    observed_release: Some(receipt.release.version.clone()),
                })));
            }
        }
        Ok((activated, None))
    }

    async fn invalid_activation_receipt(
        &self,
        deployment: &Deployment,
        component: &DeploymentComponent,
        receipt: &PreparedRelease,
        activated: &mut Vec<ActivatedComponent>,
    ) -> DeploymentFailure {
        let name = &component.planned.context.component;
        let observed = observe_after_failure(component, RECOVERY_OBSERVATION_TIMEOUT).await;
        self.observation(
            &deployment.id,
            name,
            "activate.invalid_receipt",
            &observed,
            None,
        );
        if observed.as_ref().ok().and_then(Option::as_ref) == Some(&receipt.release) {
            activated.push(ActivatedComponent {
                name: name.clone(),
                previous: component.planned.plan.expected_current.clone(),
                observed: Some(receipt.release.clone()),
            });
        }
        DeploymentFailure::Contract {
            component: name.clone(),
            stage: OrchestrationStage::Activate,
            message: "Driver activation receipt does not point to the candidate Release".into(),
            observed_release: observed.ok().flatten().map(|release| release.version),
        }
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
        if let Ok(receipt) = &result {
            let persisted = self.timestamp().and_then(|timestamp| {
                self.history
                    .record_release_receipt(
                        &deployment.id,
                        name,
                        "prepare",
                        &receipt.release,
                        timestamp,
                    )
                    .map_err(Into::into)
            });
            if let Err(error) = persisted {
                result = Err(contract_driver_error(
                    "history.prepare",
                    name,
                    &format!("prepared Release receipt could not be persisted: {error}"),
                ));
            }
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
        self.remember_driver_warnings(&result);
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
        match &result {
            Ok(receipt) if receipt.current.as_ref() == Some(&prepared.release) => self.observation(
                &deployment.id,
                name,
                "activate.receipt",
                &Ok(receipt.current.clone()),
                Some(receipt.healthy),
            ),
            Ok(_) => self.observation(
                &deployment.id,
                name,
                "activate.receipt",
                &Err(contract_driver_error(
                    "activate",
                    name,
                    "Driver returned an out-of-plan receipt",
                )),
                None,
            ),
            Err(error) => self.observation(
                &deployment.id,
                name,
                "activate.receipt",
                &Err(error.clone()),
                None,
            ),
        }
        self.remember_history_error(self.complete_intent(intent, &outcome));
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
        events: &dyn EventSink,
    ) -> Result<DeploymentReport, OrchestrationError> {
        let mut component_results = BTreeMap::new();
        let compensation_failures = self
            .compensate(
                &deployment,
                &components,
                activated,
                &mut component_results,
                events,
            )
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
                    // A plan is a past observation, never evidence of the state
                    // after a failure. None may mean unknown; keep its diagnostic.
                    let observed_release = failure_observed(&failure, name);
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
        );
        let terminal = if matches!(failure, DeploymentFailure::Cancelled)
            && self.history_warnings.borrow().is_empty()
            && !deployment
                .components
                .values()
                .any(|result| result.outcome == ComponentOutcome::CompensationFailed)
        {
            DeploymentState::Cancelled
        } else {
            DeploymentState::Failed
        };
        self.remember_history_error(
            self.history
                .transition_deployment(
                    &deployment.id,
                    DeploymentState::Running,
                    terminal,
                    self.timestamp()?,
                )
                .map_err(Into::into),
        );
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
            warnings: self.take_warnings(),
        })
    }

    async fn compensate(
        &self,
        deployment: &Deployment,
        components: &BTreeMap<ComponentName, DeploymentComponent>,
        activated: &[ActivatedComponent],
        results: &mut BTreeMap<ComponentName, ComponentDeploymentResult>,
        events: &dyn EventSink,
    ) -> Result<BTreeMap<ComponentName, DriverError>, OrchestrationError> {
        let mut failures = BTreeMap::new();
        for activated in activated.iter().rev() {
            events.emit(crate::drivers::DriverLog {
                namespace: "compensate.started".into(),
                message: format!("Restoring {} to its previous state", activated.name),
            });
            let component = &components[&activated.name];
            let intent = self.history.record_intent(
                &deployment.id,
                &activated.name,
                OrchestrationStage::Compensate.intent_name(),
                activated
                    .previous
                    .as_ref()
                    .map_or("not_deployed", |release| release.version.as_str()),
                self.timestamp()?,
            );
            let intent = match intent {
                Ok(intent) => intent,
                Err(error) => {
                    let (result, error) = self
                        .blocked_compensation(deployment, component, error)
                        .await;
                    results.insert(activated.name.clone(), result);
                    failures.insert(activated.name.clone(), error);
                    continue;
                }
            };
            let context = recovery_context(&component.planned.context);
            let rollback = component
                .planned
                .driver
                .rollback(
                    &deployment.id,
                    &context,
                    activated.observed.as_ref(),
                    activated.previous.as_ref(),
                )
                .await;
            self.remember_driver_warnings(&rollback);
            let rollback = rollback.and_then(|receipt| {
                if receipt.current == activated.previous && receipt.healthy {
                    Ok(receipt)
                } else {
                    Err(contract_driver_error(
                        "compensate",
                        &activated.name,
                        "Driver rollback receipt is unhealthy or differs from the pre-activation Release",
                    ))
                }
            });
            let intent_outcome = rollback.as_ref().map(|_| ()).map_err(Clone::clone);
            self.compensation_observation(&deployment.id, &activated.name, &rollback);
            self.remember_history_error(self.complete_intent(intent, &intent_outcome));
            let (outcome, observed) = match rollback {
                Ok(receipt) => (
                    ComponentOutcome::Compensated,
                    receipt.current.map(|release| release.version),
                ),
                Err(error) => {
                    let observation =
                        observe_after_failure(component, RECOVERY_OBSERVATION_TIMEOUT).await;
                    self.observation(
                        &deployment.id,
                        &activated.name,
                        "compensate.failure",
                        &observation,
                        None,
                    );
                    let (error, observed) = match observation {
                        Ok(observed) => (error, observed),
                        Err(observation) => (unobserved_failure(error, &observation), None),
                    };
                    failures.insert(activated.name.clone(), error);
                    (
                        ComponentOutcome::CompensationFailed,
                        observed.map(|release| release.version),
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

    fn take_warnings(&self) -> Vec<String> {
        let mut warnings = self.history_warnings.take();
        warnings.extend(self.driver_warnings.take());
        warnings
    }

    fn remember_driver_warnings(&self, result: &Result<ActivationReceipt, DriverError>) {
        if let Ok(receipt) = result {
            self.driver_warnings
                .borrow_mut()
                .extend(receipt.warnings.iter().take(16).map(|warning| {
                    self.redactor
                        .redact(&warning.chars().take(2048).collect::<String>())
                }));
        }
    }

    fn compensation_observation(
        &self,
        deployment: &crate::domain::DeploymentId,
        name: &ComponentName,
        rollback: &Result<ActivationReceipt, DriverError>,
    ) {
        match rollback {
            Ok(receipt) => self.observation(
                deployment,
                name,
                "compensate.receipt",
                &Ok(receipt.current.clone()),
                Some(receipt.healthy),
            ),
            Err(error) => self.observation(
                deployment,
                name,
                "compensate.receipt",
                &Err(error.clone()),
                None,
            ),
        }
    }

    async fn blocked_compensation(
        &self,
        deployment: &Deployment,
        component: &DeploymentComponent,
        error: HistoryError,
    ) -> (ComponentDeploymentResult, DriverError) {
        // Journal failure forbids this mutation, not other independently journalable recovery.
        let name = &component.planned.context.component;
        let error = contract_driver_error(
            "compensate",
            name,
            &format!("cannot persist recovery intent: {error}; no recovery mutation attempted"),
        );
        let observed = observe_after_failure(component, RECOVERY_OBSERVATION_TIMEOUT).await;
        self.observation(&deployment.id, name, "compensate.blocked", &observed, None);
        (
            ComponentDeploymentResult {
                outcome: ComponentOutcome::CompensationFailed,
                attempted_release: Some(component.package.release().version.clone()),
                observed_release: observed.ok().flatten().map(|release| release.version),
            },
            error,
        )
    }

    fn persist_results(
        &self,
        deployment: &Deployment,
        error: Option<&str>,
        compensation_failures: &BTreeMap<ComponentName, DriverError>,
    ) {
        for (component, result) in &deployment.components {
            let compensation_error = compensation_failures
                .get(component)
                .map(ToString::to_string);
            let diagnostic = compensation_error.as_deref().or_else(|| {
                (result.outcome != ComponentOutcome::Succeeded)
                    .then_some(error)
                    .flatten()
            });
            self.remember_history_error(
                self.history
                    .record_component_result(
                        &deployment.id,
                        component,
                        result,
                        diagnostic,
                        &self.redactor,
                    )
                    .map_err(Into::into),
            );
        }
    }

    fn timestamp(&self) -> Result<u64, OrchestrationError> {
        self.clock.timestamp()
    }
}

pub(super) fn planned_release_ref(planned: &PlannedComponent) -> ReleaseRef {
    let release = &planned.plan.release;
    ReleaseRef {
        driver: planned.driver.kind(),
        project_id: release.project_id.clone(),
        environment_id: release.environment_id.clone(),
        component: release.component.clone(),
        generation: release.generation,
        version: release.version.clone(),
        destination: release.destination.clone(),
        destination_revision: release.destination_revision,
        endpoint_fingerprint: planned.context.endpoint_fingerprint.clone(),
        effective_capabilities: planned.plan.effective_capabilities.clone(),
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
            || release.generation != context.generation
            || release.destination != context.destination
            || release.destination_revision != context.destination_revision
            || component.planned.driver.kind() != *context.target.driver_kind()
            || context.destination_settings.driver_kind() != context.target.driver_kind()
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

fn activation_failure(
    component: &DeploymentComponent,
    receipt: &PreparedRelease,
    error: DriverError,
    cancellation: &CancellationToken,
    activated: &mut Vec<ActivatedComponent>,
    observed: Result<Option<ReleaseRef>, DriverError>,
) -> DeploymentFailure {
    let name = &component.planned.context.component;
    match observed {
        Ok(observed) => {
            if observed.as_ref() == Some(&receipt.release) {
                activated.push(ActivatedComponent {
                    name: name.clone(),
                    previous: component.planned.plan.expected_current.clone(),
                    observed: observed.clone(),
                });
            }
            if cancellation.is_cancelled()
                && (observed.as_ref() == Some(&receipt.release)
                    || observed == component.planned.plan.expected_current)
            {
                DeploymentFailure::Cancelled
            } else {
                DeploymentFailure::Driver {
                    component: name.clone(),
                    stage: OrchestrationStage::Activate,
                    error,
                    observed_release: observed.map(|release| release.version),
                }
            }
        }
        Err(observation) => DeploymentFailure::Driver {
            component: name.clone(),
            stage: OrchestrationStage::Activate,
            error: unobserved_failure(error, &observation),
            observed_release: None,
        },
    }
}

async fn observe_after_failure(
    component: &DeploymentComponent,
    timeout: Duration,
) -> Result<Option<ReleaseRef>, DriverError> {
    // Cancellation of the user operation must not suppress the read that decides
    // whether an uncertain activation needs compensation. Bound even a stuck Driver.
    let context = recovery_context(&component.planned.context);
    if let Ok(result) =
        tokio::time::timeout(timeout, component.planned.driver.current(&context)).await
    {
        result
    } else {
        context.cancellation.cancel();
        Err(DriverError {
            stage: "observe".into(),
            target: context.component.to_string(),
            message: "post-failure observation timed out".into(),
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
    #[error("Deployment {deployment}: {source}; terminal persistence failure: {persistence:?}")]
    Execution {
        deployment: crate::domain::DeploymentId,
        #[source]
        source: Box<Self>,
        persistence: Option<String>,
    },
    #[error("invalid Deployment input: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("Deployment state error: {0}")]
    Domain(#[from] crate::domain::DeploymentError),
    #[error("system clock error: {0}")]
    Clock(String),
}
