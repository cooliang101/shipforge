use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{ComponentName, DeploymentId, ReleaseVersion};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentState {
    Created,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub state: StepState,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("cannot transition step from {from:?} to {to:?}")]
pub struct StepError {
    pub from: StepState,
    pub to: StepState,
}

impl Step {
    #[must_use]
    pub fn pending(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: StepState::Pending,
        }
    }

    /// Starts a pending step.
    ///
    /// # Errors
    ///
    /// Returns an error unless this step is pending.
    pub fn start(&mut self) -> Result<(), StepError> {
        self.transition(StepState::Running)
    }

    /// Marks a running step as successful.
    ///
    /// # Errors
    ///
    /// Returns an error unless this step is running.
    pub fn succeed(&mut self) -> Result<(), StepError> {
        self.transition(StepState::Succeeded)
    }

    /// Marks a running step as failed.
    ///
    /// # Errors
    ///
    /// Returns an error unless this step is running.
    pub fn fail(&mut self) -> Result<(), StepError> {
        self.transition(StepState::Failed)
    }

    /// Skips a pending step.
    ///
    /// # Errors
    ///
    /// Returns an error unless this step is pending.
    pub fn skip(&mut self) -> Result<(), StepError> {
        self.transition(StepState::Skipped)
    }

    fn transition(&mut self, target: StepState) -> Result<(), StepError> {
        let allowed = matches!(
            (self.state, target),
            (StepState::Pending, StepState::Running | StepState::Skipped)
                | (StepState::Running, StepState::Succeeded | StepState::Failed)
        );
        if !allowed {
            return Err(StepError {
                from: self.state,
                to: target,
            });
        }
        self.state = target;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Compensated,
    CompensationFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentDeploymentResult {
    pub outcome: ComponentOutcome,
    pub attempted_release: Option<ReleaseVersion>,
    pub observed_release: Option<ReleaseVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: DeploymentId,
    pub state: DeploymentState,
    pub steps: Vec<Step>,
    pub components: BTreeMap<ComponentName, ComponentDeploymentResult>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DeploymentError {
    #[error("cannot transition Deployment from {from:?} to {to:?}")]
    InvalidTransition {
        from: DeploymentState,
        to: DeploymentState,
    },
}

impl Deployment {
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: DeploymentId::new(),
            state: DeploymentState::Created,
            steps: Vec::new(),
            components: BTreeMap::new(),
        }
    }

    /// Starts a created Deployment.
    ///
    /// # Errors
    ///
    /// Returns an error unless the Deployment is in `created` state.
    pub fn start(&mut self) -> Result<(), DeploymentError> {
        self.transition(DeploymentState::Running)
    }

    /// Marks a running Deployment as successful.
    ///
    /// # Errors
    ///
    /// Returns an error unless the Deployment is running.
    pub fn succeed(&mut self) -> Result<(), DeploymentError> {
        self.transition(DeploymentState::Succeeded)
    }

    /// Marks a running Deployment as failed.
    ///
    /// # Errors
    ///
    /// Returns an error unless the Deployment is running.
    pub fn fail(&mut self) -> Result<(), DeploymentError> {
        self.transition(DeploymentState::Failed)
    }

    /// Cancels a created or running Deployment.
    ///
    /// # Errors
    ///
    /// Returns an error when the Deployment is already terminal.
    pub fn cancel(&mut self) -> Result<(), DeploymentError> {
        self.transition(DeploymentState::Cancelled)
    }

    fn transition(&mut self, target: DeploymentState) -> Result<(), DeploymentError> {
        let allowed = matches!(
            (self.state, target),
            (
                DeploymentState::Created,
                DeploymentState::Running | DeploymentState::Cancelled
            ) | (
                DeploymentState::Running,
                DeploymentState::Succeeded | DeploymentState::Failed | DeploymentState::Cancelled
            )
        );
        if !allowed {
            return Err(DeploymentError::InvalidTransition {
                from: self.state,
                to: target,
            });
        }
        self.state = target;
        Ok(())
    }
}

impl Default for Deployment {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_allows_every_documented_terminal_state_from_running() {
        for finish in [Deployment::succeed, Deployment::fail, Deployment::cancel] {
            let mut deployment = Deployment::new();
            deployment.start().unwrap();
            finish(&mut deployment).unwrap();
        }
    }

    #[test]
    fn deployment_rejects_restarting_a_terminal_state() {
        let mut deployment = Deployment::new();
        deployment.start().unwrap();
        deployment.succeed().unwrap();
        assert!(matches!(
            deployment.start(),
            Err(DeploymentError::InvalidTransition {
                from: DeploymentState::Succeeded,
                to: DeploymentState::Running
            })
        ));
    }

    #[test]
    fn created_deployment_can_be_cancelled_before_it_starts() {
        let mut deployment = Deployment::new();
        deployment.cancel().unwrap();
        assert_eq!(deployment.state, DeploymentState::Cancelled);
    }

    #[test]
    fn step_allows_only_documented_transitions() {
        let mut successful = Step::pending("build.packaging");
        successful.start().unwrap();
        successful.succeed().unwrap();
        assert_eq!(successful.state, StepState::Succeeded);
        assert!(successful.fail().is_err());

        let mut skipped = Step::pending("linux-ssh.cleanup");
        skipped.skip().unwrap();
        assert_eq!(skipped.state, StepState::Skipped);
        assert!(skipped.start().is_err());
    }
}
