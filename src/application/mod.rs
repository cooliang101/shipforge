//! Driver-neutral application services and orchestration.

mod artifact;
mod build;
mod clock;
mod deployment;
mod deployment_logs;
mod destination_setup;
mod execution_guard;
mod orchestrator;
mod planner;
pub mod recovery;
mod recovery_attention;
mod rollback;
mod session;

pub use crate::drivers::ReleasePackage;
pub use artifact::{PackageError, ReleaseManifest, package_release};
pub use build::{BuildError, BuildReport, GitMetadata, GitWorktreeState, inspect_git, run_build};
pub use deployment::{
    DeploymentPlan, DeploymentPlanEntry, DeploymentSelection, DeploymentService,
    DeploymentServiceError,
};
pub use destination_setup::{
    DestinationSetupError, DestinationSetupGateway, DestinationSetupRequest,
    DestinationSetupService, EndpointProbeRequest, LocalIdentityCandidate, RemoteSetupCandidates,
    SetupCredential, SetupRootState,
};
pub use orchestrator::{
    DeploymentComponent, DeploymentFailure, DeploymentOrchestrator, DeploymentReport,
    OrchestrationError, OrchestrationStage,
};
pub use planner::{ApplicationError, DeploymentPlanner, PlannedComponent};
pub use recovery::{RecoveryError, RecoveryInspection, RecoveryService};
pub use recovery_attention::{LocalAttentionSummary, local_attention};
pub use rollback::{RollbackComponent, RollbackOrchestrator, RollbackReport};
pub use session::{DeploymentSession, SessionBusy};
