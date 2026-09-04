//! Driver-neutral application services and orchestration.

mod artifact;
mod build;
mod clock;
mod connection_management;
mod deployment;
mod deployment_logs;
mod destination_setup;
mod execution_guard;
pub mod history_query;
pub mod local_export;
mod orchestrator;
mod planner;
pub mod project_edit;
pub mod project_reinitialize;
pub mod recovery;
mod recovery_attention;
mod retention;
mod rollback;
mod rollback_service;
mod session;
mod step_events;

pub use crate::drivers::ReleasePackage;
pub use artifact::{PackageError, ReleaseManifest, package_release};
pub use build::{BuildError, BuildReport, GitMetadata, GitWorktreeState, inspect_git, run_build};
pub use connection_management::{
    ConnectionCredentialDraft, ConnectionDetails, ConnectionEditPreview, ConnectionManagementError,
    ConnectionManagementService, DestinationRemovalPreview, HostKeyConfirmation, ManagementBlocker,
    ManagementPaths, ManagementSource, ProjectRemovalPreview, SshConnectionDraft,
};
pub use deployment::{
    DeploymentPlan, DeploymentPlanEntry, DeploymentSelection, DeploymentService,
    DeploymentServiceError,
};
pub use destination_setup::{
    DestinationSetupError, DestinationSetupGateway, DestinationSetupRequest,
    DestinationSetupService, EndpointProbeRequest, LocalIdentityCandidate,
    RemoteDirectoryCandidates, RemoteSetupCandidates, SetupCredential, SetupRootState,
};
pub use orchestrator::{
    DeploymentComponent, DeploymentFailure, DeploymentOrchestrator, DeploymentReport,
    OrchestrationError, OrchestrationStage,
};
pub use planner::{ApplicationError, DeploymentPlanner, PlannedComponent};
pub use recovery::{RecoveryError, RecoveryInspection, RecoveryService};
pub use recovery_attention::{LocalAttentionSummary, local_attention};
pub use rollback::{RollbackComponent, RollbackOrchestrator, RollbackReport};
pub use rollback_service::{
    RollbackCandidates, RollbackComponentCandidates, RollbackOption, RollbackPlan,
    RollbackPreviewEntry, RollbackService, RollbackServiceError,
};
pub use session::{DeploymentSession, SessionBusy};
