//! Typed diagnostics for deployment controls, not a formatter for user log bodies.

use crate::{
    application::{
        ApplicationError, BuildError, DeploymentServiceError, OrchestrationError, PackageError,
    },
    drivers::DriverError,
};

use super::presentation::step_label;

const HISTORY_ERROR: &str = "Local deployment history could not be accessed or updated. Check the application's data directory and inspect history before retrying; do not delete it to bypass this error.";
const EXECUTION_NOTICE: &str = "Remote effects may already exist. Inspect the recorded outcome and current state before retrying; this error does not establish a new deployment outcome.";
const PERSISTENCE_WARNING: &str =
    "Local terminal state could not be saved; inspect history and current state before retrying.";
const LOG_WARNING: &str =
    "The deployment log may be incomplete. Inspect deployment history before retrying.";
const MISSING_CONNECTION_SUPPORT: &str = "This application cannot perform deployment operations for the selected connection type. Choose a supported connection in the TUI.";

// Matches the owned error passed to Result::map_err at the UI boundary.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn deployment_error(error: DeploymentServiceError) -> String {
    service_error(&error)
}

fn service_error(error: &DeploymentServiceError) -> String {
    match error {
        DeploymentServiceError::PersistenceAfterFailure {
            original,
            persistence,
        } => format!(
            "{} {PERSISTENCE_WARNING} Save diagnostic: {}",
            service_error(original),
            service_error(persistence)
        ),
        DeploymentServiceError::Execution {
            deployment,
            source,
            log_diagnostic,
        } => {
            let warning = if log_diagnostic.is_empty() {
                ""
            } else {
                LOG_WARNING
            };
            format!(
                "Deployment {deployment}: {} {EXECUTION_NOTICE} {warning}",
                service_error(source)
            )
        }
        DeploymentServiceError::Log(_) => LOG_WARNING.into(),
        DeploymentServiceError::StalePlan(_) => {
            "The deployment plan is stale. Return to the selection and run its checks again.".into()
        }
        DeploymentServiceError::InvalidSelection(_) => {
            "The deployment selection is invalid. Select the Environment and Components again.".into()
        }
        DeploymentServiceError::MissingDestination(_) => {
            "A selected connection is missing. Open Connections and correct the Component target in the TUI.".into()
        }
        DeploymentServiceError::MissingDriver(_) => MISSING_CONNECTION_SUPPORT.into(),
        DeploymentServiceError::Cancelled => "Deployment was cancelled.".into(),
        DeploymentServiceError::Clock(_) => clock_error().into(),
        DeploymentServiceError::Version(_) => {
            "A Release version could not be generated. Check the system clock and retry the plan.".into()
        }
        DeploymentServiceError::TemporaryDirectory(_) => {
            "A temporary Release directory could not be created. Check local disk space and temporary-directory permissions.".into()
        }
        DeploymentServiceError::Build(error) => build_error(error),
        DeploymentServiceError::Config(_) => {
            "Project or connection configuration could not be loaded, validated or saved. Review the configuration in the TUI and check local file permissions; underlying file contents are not displayed.".into()
        }
        DeploymentServiceError::Package(error) => package_error(error).into(),
        DeploymentServiceError::Application(error) => application_error(error),
        DeploymentServiceError::History(_) => HISTORY_ERROR.into(),
        DeploymentServiceError::Orchestration(error) => orchestration_error(error),
    }
}

fn application_error(error: &ApplicationError) -> String {
    match error {
        ApplicationError::Cancelled => "Deployment planning was cancelled.".into(),
        ApplicationError::MissingDriver(_) => MISSING_CONNECTION_SUPPORT.into(),
        ApplicationError::UnsupportedCapabilities(rejection) => {
            let operations = rejection
                .missing
                .iter()
                .map(|capability| step_label(&format!("{capability:?}")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("The selected connection does not support these required operations: {operations}. Choose a supported connection or revise the deployment in the TUI.")
        }
        ApplicationError::Driver(error) => driver_error(error),
        ApplicationError::Contract(_) => {
            "An operation safety check failed; inspect the selection and recorded/current state before retrying.".into()
        }
    }
}

fn orchestration_error(error: &OrchestrationError) -> String {
    match error {
        OrchestrationError::Execution {
            deployment,
            source,
            persistence,
        } => {
            let warning = if persistence.is_some() {
                PERSISTENCE_WARNING
            } else {
                ""
            };
            format!(
                "Deployment {deployment}: {} {EXECUTION_NOTICE} {warning}",
                orchestration_error(source)
            )
        }
        OrchestrationError::InvalidInput(_) => {
            "Deployment inputs are inconsistent. Return to the selection and run its checks again.".into()
        }
        OrchestrationError::History(_) => HISTORY_ERROR.into(),
        OrchestrationError::Domain(_) => {
            "The deployment state could not be updated consistently. Inspect history and current state before retrying.".into()
        }
        OrchestrationError::Clock(_) => clock_error().into(),
    }
}

fn clock_error() -> &'static str {
    "The system clock could not provide a valid deployment timestamp. Check the local clock before retrying."
}

pub(super) fn driver_error(error: &DriverError) -> String {
    // These public string fields are not guaranteed to be redacted. Only known
    // metadata enter the control diagnostic. A syntactically valid target is
    // not proof it is one of the user's selected Components; the caller can
    // display its independently typed Component alongside this message.
    let stage = error
        .stage
        .strip_prefix("linux-ssh.")
        .unwrap_or(&error.stage);
    let (stage, advice) = match stage {
        "connect" | "authentication" => (
            stage,
            "Check the saved SSH host-key fingerprint, selected identity, server address and connectivity in Connections. Do not bypass host-key verification.",
        ),
        "configuration" | "context" | "target" | "destination" => (
            stage,
            "Review the selected connection and Component target in the TUI.",
        ),
        "space" => (stage, "Check available disk space on the selected server."),
        "health" => (
            stage,
            "Inspect the service and its configured health checks; do not infer service health from the current Release alone.",
        ),
        "preflight" | "marker" | "upload" | "uploading" | "prepare" | "prepare.audit"
        | "activate" | "activate.receipt" | "rollback" | "compensate" | "observe" | "inventory"
        | "audit" | "cleanup" | "logs" => (
            stage,
            "Review connection access, remote permissions and the recorded/current deployment state before retrying.",
        ),
        _ => (
            "linux-ssh",
            "Review the selected connection and the recorded/current deployment state before retrying.",
        ),
    };
    format!(
        "{} failed for the selected target. {advice}",
        step_label(stage)
    )
}

fn build_error(error: &BuildError) -> String {
    match error {
        BuildError::MissingProgram(_) => {
            "A build executable is unavailable. Install the configured program or correct the Component build command in the TUI.".into()
        }
        BuildError::InvalidGitMetadata => "Git returned invalid branch or revision metadata. Check the local repository.".into(),
        BuildError::Path { .. } | BuildError::UnsafeWorkingDirectory(_) => {
            "The build directory cannot be used safely. Select an existing directory inside the Project and check its permissions.".into()
        }
        BuildError::DirtyConfirmationRequired(_) => {
            "The Git worktree has changes. Review and explicitly confirm them in the TUI before building.".into()
        }
        BuildError::GitProcess(_) | BuildError::GitFailed(_) => {
            "The Git check failed. Verify Git is installed and the local repository is readable.".into()
        }
        BuildError::GitCancelled => "The Git check was cancelled.".into(),
        BuildError::GitTimedOut => "The Git check timed out. Inspect the local repository before retrying.".into(),
        BuildError::Cancelled { index, .. } => format!("Build command {} was cancelled.", index.saturating_add(1)),
        BuildError::TimedOut { index, .. } => format!("Build command {} timed out. Review the build command and its logs.", index.saturating_add(1)),
        BuildError::CommandFailed { index, .. } => format!("Build command {} exited unsuccessfully. Review the build command and its logs.", index.saturating_add(1)),
        BuildError::Process(_) => "The build process could not be completed. Check the configured executable, local permissions and build logs.".into(),
    }
}

fn package_error(error: &PackageError) -> &'static str {
    match error {
        PackageError::Cancelled => "Release packaging was cancelled.",
        PackageError::AlreadyExists(_) => {
            "The Release archive already exists. Generate a new version; do not overwrite the existing archive."
        }
        PackageError::OutputInsideArtifact(_) => {
            "The Release output directory is inside the build output. Separate these directories before packaging."
        }
        PackageError::UnsafePath(_)
        | PackageError::NonUtf8Path(_)
        | PackageError::PathTooLong(_) => {
            "The build output contains a path that cannot be archived safely. Use portable UTF-8 paths within the archive path limit."
        }
        PackageError::TooManyEntries(_) => {
            "The build output exceeds the archive entry limit. Reduce the packaged output."
        }
        PackageError::ReservedPath(_) => {
            "The build output uses the reserved manifest.json archive path. Rename that entry before packaging."
        }
        PackageError::SymbolicLink(_) | PackageError::UnsupportedEntry(_) => {
            "The build output contains a symbolic link or unsupported filesystem entry. Package regular files and directories only."
        }
        PackageError::EmptyArtifact(_) => {
            "The configured build output is empty. Check the build command and artifact path."
        }
        PackageError::ArtifactKindChanged(_) | PackageError::ArtifactChanged(_) => {
            "The build output changed during packaging. Stop concurrent writes and build again."
        }
        PackageError::InvalidSourceRevision => {
            "The Release source revision is invalid. Check the local Git repository and rebuild."
        }
        PackageError::Io { .. } | PackageError::Archive(_) => {
            "The Release archive could not be read or written. Check local disk space and file permissions."
        }
        PackageError::Manifest(_) => {
            "The Release manifest could not be created. Check the build output and Project configuration before rebuilding."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapters::{ProcessOutput, ProcessTermination},
        config::ConfigError,
        domain::{Capability, DeploymentId, DriverCapabilities},
        history::HistoryError,
    };

    const SECRET: &str = "private-credential-sentinel";

    #[test]
    fn nested_execution_preserves_ids_and_persistence_and_log_warnings() {
        let outer = DeploymentId::new();
        let inner = DeploymentId::new();
        let error = DeploymentServiceError::Execution {
            deployment: outer.clone(),
            log_diagnostic: SECRET.into(),
            source: Box::new(DeploymentServiceError::PersistenceAfterFailure {
                original: Box::new(DeploymentServiceError::Orchestration(
                    OrchestrationError::Execution {
                        deployment: inner.clone(),
                        source: Box::new(OrchestrationError::History(HistoryError::Corrupt(
                            SECRET.into(),
                        ))),
                        persistence: Some(SECRET.into()),
                    },
                )),
                persistence: Box::new(DeploymentServiceError::History(HistoryError::Corrupt(
                    SECRET.into(),
                ))),
            }),
        };
        let text = deployment_error(error);
        for expected in [
            outer.to_string(),
            inner.to_string(),
            PERSISTENCE_WARNING.into(),
            LOG_WARNING.into(),
            EXECUTION_NOTICE.into(),
        ] {
            assert!(text.contains(&expected), "{text}");
        }
        assert!(!text.contains(SECRET));
        assert!(!text.contains("not started"));
        assert!(!text.contains("succeeded"));
    }

    #[test]
    fn unavailable_operations_never_show_implementation_or_capability_identifiers() {
        let rejection = DriverCapabilities::default()
            .require([Capability::StagedDeployment, Capability::ExplicitActivation])
            .unwrap_err();
        let supported_diagnostic = application_error(&ApplicationError::UnsupportedCapabilities(
            rejection.clone(),
        ));
        assert!(supported_diagnostic.contains("Release preparation"));
        assert!(supported_diagnostic.contains("Release activation"));
        for error in [
            DeploymentServiceError::MissingDriver("linux-ssh".into()),
            DeploymentServiceError::Application(ApplicationError::MissingDriver(
                "linux-ssh".into(),
            )),
            DeploymentServiceError::Application(ApplicationError::UnsupportedCapabilities(
                rejection,
            )),
        ] {
            let text = deployment_error(error);
            for forbidden in [
                "linux-ssh",
                "Driver",
                "Capability",
                "StagedDeployment",
                "ExplicitActivation",
            ] {
                assert!(!text.contains(forbidden), "{text}");
            }
            assert!(text.contains("connection"));
        }
    }

    #[test]
    fn driver_errors_keep_known_stage_and_safe_connection_advice() {
        for (stage, label) in [
            ("linux-ssh.upload", "Uploading"),
            ("connect", "Connecting"),
            ("authentication", "Authenticating"),
        ] {
            let text = deployment_error(DeploymentServiceError::Application(
                ApplicationError::Driver(DriverError {
                    stage: stage.into(),
                    target: "api".into(),
                    message: SECRET.into(),
                    suggested_action: format!("disable checks with {SECRET}"),
                }),
            ));
            assert!(text.starts_with(&format!("{label} failed for the selected target.")));
            assert!(!text.contains(SECRET));
            assert!(!text.contains("linux-ssh"));
            if stage != "linux-ssh.upload" {
                assert!(text.contains("host-key fingerprint"));
                assert!(text.contains("selected identity"));
                assert!(text.contains("Do not bypass"));
            }
        }
        let text = driver_error(&DriverError {
            stage: format!("unknown.{SECRET}"),
            target: format!("/private/{SECRET}\n\u{202e}"),
            message: SECRET.into(),
            suggested_action: SECRET.into(),
        });
        assert!(text.starts_with("Remote operation failed for the selected target."));
        assert!(!text.contains(SECRET));
    }

    #[test]
    fn driver_errors_do_not_trust_syntactically_valid_raw_target_text() {
        assert!(crate::domain::ComponentName::parse(SECRET).is_ok());
        let text = driver_error(&DriverError {
            stage: "connect".into(),
            target: SECRET.into(),
            message: SECRET.into(),
            suggested_action: SECRET.into(),
        });
        assert!(text.starts_with("Connecting failed for the selected target."));
        assert!(!text.contains(SECRET));
    }

    #[test]
    fn parser_database_contract_and_process_bodies_are_not_displayed() {
        let yaml = serde_yaml_ng::from_str::<u64>(&format!("'{SECRET}'")).unwrap_err();
        assert!(yaml.to_string().contains(SECRET));
        let database = rusqlite::Connection::open_in_memory().unwrap();
        database.execute_batch(&format!("CREATE TABLE sample(id INTEGER); CREATE TRIGGER reject BEFORE INSERT ON sample BEGIN SELECT RAISE(ABORT, '{SECRET}'); END;")).unwrap();
        let sqlite = database
            .execute("INSERT INTO sample VALUES(1)", [])
            .unwrap_err();
        assert!(sqlite.to_string().contains(SECRET));
        let json = serde_json::from_str::<u64>(&format!("\"{SECRET}\"")).unwrap_err();
        for error in [
            DeploymentServiceError::Config(ConfigError::Yaml {
                path: SECRET.into(),
                source: yaml,
            }),
            DeploymentServiceError::History(HistoryError::Sqlite(sqlite)),
            DeploymentServiceError::Package(PackageError::Manifest(json)),
            DeploymentServiceError::Application(ApplicationError::Contract(SECRET.into())),
            DeploymentServiceError::Build(BuildError::CommandFailed {
                index: 0,
                output: ProcessOutput {
                    termination: ProcessTermination::Exited,
                    exit_code: Some(1),
                    stdout: SECRET.as_bytes().to_vec(),
                    stderr: SECRET.as_bytes().to_vec(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
            }),
        ] {
            let text = deployment_error(error);
            assert!(!text.contains(SECRET), "{text}");
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn nested_contract_error_does_not_erase_execution_identity_or_claim_no_effects() {
        let id = DeploymentId::new();
        let text = deployment_error(DeploymentServiceError::Execution {
            deployment: id.clone(),
            source: Box::new(DeploymentServiceError::Application(
                ApplicationError::Contract(SECRET.into()),
            )),
            log_diagnostic: String::new(),
        });
        assert!(text.starts_with(&format!("Deployment {id}:")));
        assert!(text.contains("An operation safety check failed"));
        assert!(text.contains(EXECUTION_NOTICE));
        assert!(!text.contains(SECRET));
        assert!(!text.contains("not started"));
        assert!(!text.contains(LOG_WARNING));
    }
}
