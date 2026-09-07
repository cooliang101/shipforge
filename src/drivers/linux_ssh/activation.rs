use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::{ComponentRelease, DeploymentId, ReleaseVersion},
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    AuthenticatedSession, LinuxSshTarget, PreparedRemoteRelease, RemoteCommandOutput,
    SshConnectionError, prepare::ReleasePaths, transfer::sanitize_remote_error,
};

const COMPENSATION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivationOptions {
    pub command_timeout: Duration,
}

impl Default for ActivationOptions {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivatedRemoteRelease {
    root: String,
    release: ComponentRelease,
    previous: Option<ReleaseVersion>,
    service_started: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RolledBackRemoteRelease {
    from: ComponentRelease,
    current: Option<ReleaseVersion>,
    service_updated: bool,
}

impl RolledBackRemoteRelease {
    #[must_use]
    pub fn from(&self) -> &ComponentRelease {
        &self.from
    }

    #[must_use]
    pub fn current(&self) -> Option<&ReleaseVersion> {
        self.current.as_ref()
    }

    #[must_use]
    pub const fn service_updated(&self) -> bool {
        self.service_updated
    }
}

impl ActivatedRemoteRelease {
    #[must_use]
    pub fn release(&self) -> &ComponentRelease {
        &self.release
    }

    #[must_use]
    pub fn previous(&self) -> Option<&ReleaseVersion> {
        self.previous.as_ref()
    }

    #[must_use]
    pub const fn service_started(&self) -> bool {
        self.service_started
    }
}

impl AuthenticatedSession {
    /// Observes the canonical `current` link for one Component.
    ///
    /// A missing link is `None`. Non-links, dangling links, absolute targets,
    /// and targets outside `releases/<version>` are rejected as drift.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, timeout, remote command failure, or
    /// a malformed remote `current` state.
    pub async fn observe_current(
        &self,
        target: &LinuxSshTarget,
        options: ActivationOptions,
        cancellation: &CancellationToken,
    ) -> Result<Option<ReleaseVersion>, ActivateReleaseError> {
        validate_options(options)?;
        self.check_release_layout(target, cancellation).await?;
        observe_with_remote(self, target, options.command_timeout, cancellation).await
    }

    /// Atomically activates a prepared Component Release.
    ///
    /// The operation checks the expected `current` twice, creates the new link
    /// on the same filesystem, atomically renames it over `current`, and then
    /// executes the configured service action. A known post-switch failure
    /// restores the previous link, or the undeployed state for a first deployment.
    /// Unknown service outcomes require manual recovery without competing commands.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid preparation, drift, cancellation, remote
    /// command failure, or failed compensation.
    pub async fn activate_release(
        &self,
        target: &LinuxSshTarget,
        prepared: &PreparedRemoteRelease,
        expected_current: Option<&ReleaseVersion>,
        deployment: &DeploymentId,
        options: ActivationOptions,
        cancellation: &CancellationToken,
    ) -> Result<ActivatedRemoteRelease, ActivateReleaseError> {
        self.check_release_manifest(
            target,
            &super::DeploymentMarker::for_release(prepared.release()),
            &prepared.release().version,
            cancellation,
        )
        .await?;
        activate_with_remote(
            self,
            target,
            prepared,
            expected_current,
            deployment,
            options,
            cancellation,
        )
        .await
    }

    /// Restores the state captured by a successful activation.
    ///
    /// This is used when a later health check fails. Compensation deliberately
    /// uses its own bounded token so cancellation cannot interrupt recovery.
    ///
    /// # Errors
    ///
    /// Returns an error if the active Release drifted or recovery could not be
    /// completed and verified.
    pub async fn compensate_activation(
        &self,
        target: &LinuxSshTarget,
        activation: &ActivatedRemoteRelease,
        deployment: &DeploymentId,
        options: ActivationOptions,
    ) -> Result<(), ActivateReleaseError> {
        validate_options(options)?;
        if activation.root != target.root {
            return Err(ActivateReleaseError::InvalidActivationReceipt);
        }
        self.check_release_layout(target, &CancellationToken::new())
            .await?;
        compensate(
            self,
            target,
            activation,
            deployment,
            options.command_timeout.min(COMPENSATION_TIMEOUT),
        )
        .await
        .map_err(|compensation| ActivateReleaseError::CompensationFailed {
            cause: "a later activation check failed".into(),
            compensation,
            manual_action: manual_action(target, activation),
        })
    }

    /// Rolls an active Component back to an existing historical Release or to
    /// its pre-first-deployment state.
    ///
    /// Cancellation is honored before the operation begins. Once started, the
    /// atomic switch and service update use an independent bounded token. A
    /// caller receiving an error must observe `current`; the application
    /// orchestrator compensates a partially applied explicit rollback.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, current drift, an unsafe or missing
    /// historical Release, a conflicting temporary path, or command failure.
    pub async fn rollback_release(
        &self,
        target: &LinuxSshTarget,
        current: &ComponentRelease,
        desired: Option<&ReleaseVersion>,
        deployment: &DeploymentId,
        options: ActivationOptions,
        cancellation: &CancellationToken,
    ) -> Result<RolledBackRemoteRelease, ActivateReleaseError> {
        self.check_release_layout(target, cancellation).await?;
        rollback_with_remote(
            self,
            target,
            current,
            desired,
            deployment,
            options,
            cancellation,
        )
        .await
    }

    /// Restores a historical Release after an explicit rollback removed `current`.
    ///
    /// # Errors
    /// Returns an error on cancellation, current drift, unsafe history, or a
    /// command failure. The caller must observe and report any partial change.
    pub async fn restore_undeployed_release(
        &self,
        target: &LinuxSshTarget,
        desired: &ComponentRelease,
        deployment: &DeploymentId,
        options: ActivationOptions,
        cancellation: &CancellationToken,
    ) -> Result<(), ActivateReleaseError> {
        self.check_release_layout(target, cancellation).await?;
        restore_undeployed_with_remote(self, target, desired, deployment, options, cancellation)
            .await
    }
}

async fn restore_undeployed_with_remote<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    desired: &ComponentRelease,
    deployment: &DeploymentId,
    options: ActivationOptions,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError> {
    validate_options(options)?;
    if cancellation.is_cancelled() {
        return Err(ActivateReleaseError::Cancelled);
    }
    let recovery = CancellationToken::new();
    let timeout = options.command_timeout.min(COMPENSATION_TIMEOUT);
    verify_current(remote, target, None, timeout, &recovery).await?;
    let activation = ActivatedRemoteRelease {
        root: target.root.clone(),
        release: desired.clone(),
        previous: None,
        service_started: false,
    };
    restore_previous(
        remote,
        target,
        &activation,
        &desired.version,
        None,
        deployment,
        timeout,
        &recovery,
    )
    .await
    .map_err(|cause| ActivateReleaseError::ExplicitRollbackFailed {
        cause,
        manual_action: format!(
            "Inspect {}/current and the service before retrying recovery",
            target.root
        ),
    })
}

#[allow(clippy::too_many_arguments)]
async fn rollback_with_remote<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    current: &ComponentRelease,
    desired: Option<&ReleaseVersion>,
    deployment: &DeploymentId,
    options: ActivationOptions,
    cancellation: &CancellationToken,
) -> Result<RolledBackRemoteRelease, ActivateReleaseError> {
    validate_options(options)?;
    if cancellation.is_cancelled() {
        return Err(ActivateReleaseError::Cancelled);
    }
    if desired == Some(&current.version) {
        return Err(ActivateReleaseError::InvalidRollbackTarget);
    }
    let activation = ActivatedRemoteRelease {
        root: target.root.clone(),
        release: current.clone(),
        previous: desired.cloned(),
        service_started: target.service.is_some(),
    };
    compensate(
        remote,
        target,
        &activation,
        deployment,
        options.command_timeout.min(COMPENSATION_TIMEOUT),
    )
    .await
    .map_err(|cause| ActivateReleaseError::ExplicitRollbackFailed {
        cause,
        manual_action: manual_action(target, &activation),
    })?;
    Ok(RolledBackRemoteRelease {
        from: current.clone(),
        current: desired.cloned(),
        service_updated: target.service.is_some(),
    })
}

#[async_trait]
trait ActivationRemote: Sync {
    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError>;

    async fn predicate(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.command(command, timeout, cancellation).await
    }
}

#[async_trait]
impl ActivationRemote for AuthenticatedSession {
    async fn predicate(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.execute_allowing(command, timeout, cancellation, &[0, 1])
            .await
    }

    async fn command(
        &self,
        command: &CommandSpec,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, SshConnectionError> {
        self.execute(command, timeout, cancellation).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn activate_with_remote<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    prepared: &PreparedRemoteRelease,
    expected_current: Option<&ReleaseVersion>,
    deployment: &DeploymentId,
    options: ActivationOptions,
    cancellation: &CancellationToken,
) -> Result<ActivatedRemoteRelease, ActivateReleaseError> {
    validate_options(options)?;
    if cancellation.is_cancelled() {
        return Err(ActivateReleaseError::Cancelled);
    }
    let paths = preflight_activation(
        remote,
        target,
        prepared,
        expected_current,
        deployment,
        options.command_timeout,
        cancellation,
    )
    .await?;
    let temporary_link = create_activation_link(
        remote,
        target,
        prepared,
        expected_current,
        deployment,
        &paths,
        options.command_timeout,
        cancellation,
    )
    .await?;
    let current_path = format!("{}/current", target.root);
    let switch = run_required(
        remote,
        "switch current Release",
        "mv",
        [
            "--no-target-directory",
            "--",
            &temporary_link,
            &current_path,
        ],
        options.command_timeout,
        cancellation,
    )
    .await;
    if let Err(error) = switch {
        return resolve_uncertain_switch(
            remote,
            target,
            prepared,
            expected_current,
            deployment,
            &temporary_link,
            options.command_timeout,
            error,
        )
        .await;
    }

    let activation = ActivatedRemoteRelease {
        root: target.root.clone(),
        release: prepared.release().clone(),
        previous: expected_current.cloned(),
        service_started: target.service.is_some(),
    };
    finish_activation(
        remote,
        target,
        activation,
        deployment,
        options.command_timeout,
        cancellation,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn preflight_activation<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    prepared: &PreparedRemoteRelease,
    expected_current: Option<&ReleaseVersion>,
    deployment: &DeploymentId,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ReleasePaths, ActivateReleaseError> {
    let paths = validate_prepared(target, prepared, deployment)?;
    require_path(
        remote,
        "verify prepared archive",
        "-f",
        paths.archive.as_str(),
        timeout,
        cancellation,
    )
    .await?;
    reject_symbolic_path(remote, paths.archive.as_str(), timeout, cancellation).await?;
    require_path(
        remote,
        "verify prepared Release directory",
        "-d",
        &paths.final_directory,
        timeout,
        cancellation,
    )
    .await?;
    reject_symbolic_path(remote, &paths.final_directory, timeout, cancellation).await?;
    verify_same_filesystem(
        remote,
        &target.root,
        &paths.temporary_directory,
        timeout,
        cancellation,
    )
    .await?;

    let observed = observe_with_remote(remote, target, timeout, cancellation).await?;
    ensure_expected(expected_current, observed.as_ref())?;
    Ok(paths)
}

#[allow(clippy::too_many_arguments)]
async fn create_activation_link<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    prepared: &PreparedRemoteRelease,
    expected_current: Option<&ReleaseVersion>,
    deployment: &DeploymentId,
    paths: &ReleasePaths,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String, ActivateReleaseError> {
    let temporary_link = format!("{}/{}.current", paths.temporary_directory, deployment);
    if path_exists_any(remote, &temporary_link, timeout, cancellation).await? {
        return Err(ActivateReleaseError::Conflict(temporary_link));
    }
    let link_target = format!("releases/{}", prepared.release().version);
    let create = run_required(
        remote,
        "create activation link",
        "ln",
        ["--symbolic", "--", &link_target, &temporary_link],
        timeout,
        cancellation,
    )
    .await;
    if let Err(error) = create {
        return if matches!(error, ActivateReleaseError::CommandFailed { .. }) {
            Err(error)
        } else {
            combine_cleanup(
                error,
                remove_temporary_link(remote, &temporary_link, timeout).await,
            )
        };
    }

    let observed = match observe_with_remote(remote, target, timeout, cancellation).await {
        Ok(observed) => observed,
        Err(error) => {
            return combine_cleanup(
                error,
                remove_temporary_link(remote, &temporary_link, timeout).await,
            );
        }
    };
    if let Err(error) = ensure_expected(expected_current, observed.as_ref()) {
        return combine_cleanup(
            error,
            remove_temporary_link(remote, &temporary_link, timeout).await,
        );
    }
    Ok(temporary_link)
}

async fn finish_activation<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    activation: ActivatedRemoteRelease,
    deployment: &DeploymentId,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ActivatedRemoteRelease, ActivateReleaseError> {
    if cancellation.is_cancelled() {
        return fail_after_switch(
            remote,
            target,
            &activation,
            deployment,
            timeout,
            "activation was cancelled after the current link changed".into(),
            true,
        )
        .await;
    }

    let completion_token = CancellationToken::new();
    if let Some(service) = &target.service {
        let commands = if activation.previous.is_some() {
            service.update_commands()
        } else {
            &service.start
        };
        let restart = run_service_action(
            remote,
            "activate service",
            commands,
            &format!("{}/releases/{}", target.root, activation.release.version),
            timeout,
            &completion_token,
        )
        .await;
        if let Err(error) = restart {
            if error.service_outcome_unknown() {
                return Err(error);
            }
            return fail_after_switch(
                remote,
                target,
                &activation,
                deployment,
                timeout,
                error.to_string(),
                false,
            )
            .await;
        }
    }

    let observed = observe_with_remote(remote, target, timeout, &completion_token).await;
    match observed {
        Ok(Some(version)) if version == activation.release.version => {
            if cancellation.is_cancelled() {
                fail_after_switch(
                    remote,
                    target,
                    &activation,
                    deployment,
                    timeout,
                    "activation was cancelled after the current link changed".into(),
                    true,
                )
                .await
            } else {
                Ok(activation)
            }
        }
        Ok(actual) => {
            let error = ActivateReleaseError::Drift {
                expected: display_current(Some(&activation.release.version)),
                actual: display_current(actual.as_ref()),
            };
            fail_after_switch(
                remote,
                target,
                &activation,
                deployment,
                timeout,
                error.to_string(),
                false,
            )
            .await
        }
        Err(error) => {
            let cancelled = matches!(error, ActivateReleaseError::Cancelled);
            fail_after_switch(
                remote,
                target,
                &activation,
                deployment,
                timeout,
                error.to_string(),
                cancelled,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_uncertain_switch<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    prepared: &PreparedRemoteRelease,
    expected_current: Option<&ReleaseVersion>,
    deployment: &DeploymentId,
    temporary_link: &str,
    command_timeout: Duration,
    original: ActivateReleaseError,
) -> Result<ActivatedRemoteRelease, ActivateReleaseError> {
    let recovery_timeout = command_timeout.min(COMPENSATION_TIMEOUT);
    let recovery_token = CancellationToken::new();
    let activation = ActivatedRemoteRelease {
        root: target.root.clone(),
        release: prepared.release().clone(),
        previous: expected_current.cloned(),
        service_started: false,
    };
    if let Err(cleanup) = remove_temporary_link(remote, temporary_link, recovery_timeout).await {
        return Err(ActivateReleaseError::CompensationFailed {
            cause: original.to_string(),
            compensation: format!("could not neutralize the activation link: {cleanup}").into(),
            manual_action: manual_action(target, &activation),
        });
    }
    let observed = observe_with_remote(remote, target, recovery_timeout, &recovery_token).await;
    match observed {
        Ok(Some(version)) if version == prepared.release().version => {
            fail_after_switch(
                remote,
                target,
                &activation,
                deployment,
                command_timeout,
                original.to_string(),
                matches!(original, ActivateReleaseError::Cancelled),
            )
            .await
        }
        Ok(actual) if actual.as_ref() == expected_current => Err(original),
        Ok(actual) => Err(ActivateReleaseError::CompensationFailed {
            cause: original.to_string(),
            compensation: format!(
                "could not determine ownership after switch; current is {}",
                display_current(actual.as_ref())
            )
            .into(),
            manual_action: manual_action(target, &activation),
        }),
        Err(error) => Err(ActivateReleaseError::CompensationFailed {
            cause: original.to_string(),
            compensation: error.to_string().into(),
            manual_action: manual_action(target, &activation),
        }),
    }
}

async fn fail_after_switch<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    activation: &ActivatedRemoteRelease,
    deployment: &DeploymentId,
    command_timeout: Duration,
    cause: String,
    cancelled: bool,
) -> Result<ActivatedRemoteRelease, ActivateReleaseError> {
    match compensate(
        remote,
        target,
        activation,
        deployment,
        command_timeout.min(COMPENSATION_TIMEOUT),
    )
    .await
    {
        Ok(()) if cancelled => Err(ActivateReleaseError::CancelledAndCompensated),
        Ok(()) => Err(ActivateReleaseError::ActivationFailedAndCompensated { cause }),
        Err(compensation) => Err(ActivateReleaseError::CompensationFailed {
            cause,
            compensation,
            manual_action: manual_action(target, activation),
        }),
    }
}

async fn compensate<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    activation: &ActivatedRemoteRelease,
    deployment: &DeploymentId,
    timeout: Duration,
) -> Result<(), RecoveryFailure> {
    let cancellation = CancellationToken::new();
    let observed = observe_with_remote(remote, target, timeout, &cancellation)
        .await
        .map_err(|error| error.to_string())?;
    if observed.as_ref() != Some(&activation.release.version) {
        return Err(format!(
            "current drifted before compensation: expected {}, got {}",
            activation.release.version,
            display_current(observed.as_ref())
        )
        .into());
    }

    if let Some(previous) = &activation.previous {
        restore_previous(
            remote,
            target,
            activation,
            previous,
            Some(&activation.release.version),
            deployment,
            timeout,
            &cancellation,
        )
        .await?;
    } else {
        restore_not_deployed(remote, target, activation, timeout, &cancellation).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn restore_previous<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    activation: &ActivatedRemoteRelease,
    previous: &ReleaseVersion,
    expected_current: Option<&ReleaseVersion>,
    deployment: &DeploymentId,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), RecoveryFailure> {
    let paths = ReleasePaths::new(target, &activation.release, deployment);
    let previous_directory = format!("{}/{}", paths.release_parent, previous);
    validate_historical_release(
        remote,
        &previous_directory,
        &activation.release,
        previous,
        timeout,
        cancellation,
    )
    .await?;
    let temporary_link = format!(
        "{}/{}.rollback-current",
        paths.temporary_directory, deployment
    );
    if path_exists_any(remote, &temporary_link, timeout, cancellation)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err(format!("rollback temporary path already exists: `{temporary_link}`").into());
    }
    let link_target = format!("releases/{previous}");
    let create = run_required(
        remote,
        "create rollback link",
        "ln",
        ["--symbolic", "--", &link_target, &temporary_link],
        timeout,
        cancellation,
    )
    .await;
    if let Err(error) = create {
        return Err(cleanup_error(remote, &temporary_link, timeout, error)
            .await
            .into());
    }
    if let Err(error) =
        verify_current(remote, target, expected_current, timeout, cancellation).await
    {
        return Err(cleanup_error(remote, &temporary_link, timeout, error)
            .await
            .into());
    }
    let current_path = format!("{}/current", target.root);
    if let Err(error) = run_required(
        remote,
        "restore previous current Release",
        "mv",
        [
            "--no-target-directory",
            "--",
            &temporary_link,
            &current_path,
        ],
        timeout,
        cancellation,
    )
    .await
    {
        return Err(cleanup_error(remote, &temporary_link, timeout, error)
            .await
            .into());
    }
    verify_current(remote, target, Some(previous), timeout, cancellation)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(service) = &target.service {
        run_service_action(
            remote,
            "restore service",
            service.restore_commands(),
            &format!("{}/releases/{previous}", target.root),
            timeout,
            cancellation,
        )
        .await
        .map_err(RecoveryFailure::from)?;
    }
    Ok(())
}

async fn validate_historical_release<R: ActivationRemote>(
    remote: &R,
    directory: &str,
    identity: &ComponentRelease,
    version: &ReleaseVersion,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    require_path(
        remote,
        "verify previous Release directory",
        "-d",
        directory,
        timeout,
        cancellation,
    )
    .await
    .map_err(|error| error.to_string())?;
    reject_symbolic_path(remote, directory, timeout, cancellation)
        .await
        .map_err(|error| error.to_string())?;
    let manifest = format!("{directory}/manifest.json");
    require_path(
        remote,
        "verify previous Release manifest",
        "-f",
        &manifest,
        timeout,
        cancellation,
    )
    .await
    .map_err(|error| error.to_string())?;
    reject_symbolic_path(remote, &manifest, timeout, cancellation)
        .await
        .map_err(|error| error.to_string())?;
    let output = run(
        remote,
        "read previous Release manifest",
        "head",
        ["-c", "8193", "--", &manifest],
        timeout,
        cancellation,
    )
    .await
    .map_err(|error| error.to_string())?;
    if output.exit_status != 0 || output.stdout_truncated {
        return Err("previous Release manifest could not be read safely".into());
    }
    super::DeploymentMarker::for_release(identity)
        .validate_manifest(&output.stdout, version)
        .map_err(|error| error.to_string())
}

async fn restore_not_deployed<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    activation: &ActivatedRemoteRelease,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), RecoveryFailure> {
    verify_current(
        remote,
        target,
        Some(&activation.release.version),
        timeout,
        cancellation,
    )
    .await
    .map_err(|error| error.to_string())?;
    let current_path = format!("{}/current", target.root);
    run_required(
        remote,
        "remove first-deployment current link",
        "rm",
        ["--", &current_path],
        timeout,
        cancellation,
    )
    .await
    .map_err(|error| error.to_string())?;
    verify_current(remote, target, None, timeout, cancellation)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(service) = &target.service {
        run_service_action(
            remote,
            "stop first-deployment service",
            &service.stop,
            &format!("{}/releases/{}", target.root, activation.release.version),
            timeout,
            cancellation,
        )
        .await
        .map_err(RecoveryFailure::from)?;
    }
    Ok(())
}

async fn cleanup_error<R: ActivationRemote>(
    remote: &R,
    temporary_link: &str,
    timeout: Duration,
    error: ActivateReleaseError,
) -> String {
    match remove_temporary_link(remote, temporary_link, timeout).await {
        Ok(()) => error.to_string(),
        Err(cleanup) => format!("{error}; rollback-link cleanup failed: {cleanup}"),
    }
}

fn validate_options(options: ActivationOptions) -> Result<(), ActivateReleaseError> {
    if options.command_timeout.is_zero() {
        Err(ActivateReleaseError::ZeroCommandTimeout)
    } else {
        Ok(())
    }
}

fn validate_prepared(
    target: &LinuxSshTarget,
    prepared: &PreparedRemoteRelease,
    deployment: &DeploymentId,
) -> Result<ReleasePaths, ActivateReleaseError> {
    let paths = ReleasePaths::new(target, prepared.release(), deployment);
    if prepared.deployment() != deployment
        || prepared.archive_path() != &paths.archive
        || prepared.release_directory() != paths.final_directory
        || prepared.size() == 0
        || prepared.sha256().len() != 64
        || !prepared
            .sha256()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ActivateReleaseError::InvalidPreparedRelease);
    }
    Ok(paths)
}

fn ensure_expected(
    expected: Option<&ReleaseVersion>,
    actual: Option<&ReleaseVersion>,
) -> Result<(), ActivateReleaseError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ActivateReleaseError::Drift {
            expected: display_current(expected),
            actual: display_current(actual),
        })
    }
}

async fn verify_current<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    expected: Option<&ReleaseVersion>,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError> {
    let actual = observe_with_remote(remote, target, timeout, cancellation).await?;
    ensure_expected(expected, actual.as_ref())
}

async fn observe_with_remote<R: ActivationRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Option<ReleaseVersion>, ActivateReleaseError> {
    let current = format!("{}/current", target.root);
    let is_link = test_path(
        remote,
        "inspect current link",
        "-L",
        &current,
        timeout,
        cancellation,
    )
    .await?;
    if !is_link {
        if test_path(
            remote,
            "inspect current path",
            "-e",
            &current,
            timeout,
            cancellation,
        )
        .await?
        {
            return Err(ActivateReleaseError::InvalidCurrent(
                "current exists but is not a symbolic link".into(),
            ));
        }
        return Ok(None);
    }

    let output = run(
        remote,
        "read current link",
        "readlink",
        ["--", &current],
        timeout,
        cancellation,
    )
    .await?;
    if output.exit_status != 0 {
        return Err(ActivateReleaseError::CommandFailed {
            stage: "read current link",
            status: output.exit_status,
        });
    }
    let version = parse_current_target(&output)?;
    let release_directory = format!("{}/releases/{version}", target.root);
    if test_path(
        remote,
        "verify current Release path type",
        "-L",
        &release_directory,
        timeout,
        cancellation,
    )
    .await?
    {
        return Err(ActivateReleaseError::InvalidCurrent(
            "current Release directory must not be a symbolic link".into(),
        ));
    }
    if !test_path(
        remote,
        "verify current Release directory",
        "-d",
        &current,
        timeout,
        cancellation,
    )
    .await?
    {
        return Err(ActivateReleaseError::InvalidCurrent(
            "current is a dangling or non-directory link".into(),
        ));
    }
    Ok(Some(version))
}

fn parse_current_target(
    output: &RemoteCommandOutput,
) -> Result<ReleaseVersion, ActivateReleaseError> {
    if output.stdout_truncated {
        return Err(ActivateReleaseError::InvalidCurrent(
            "readlink output was truncated".into(),
        ));
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| {
        ActivateReleaseError::InvalidCurrent("readlink output was not UTF-8".into())
    })?;
    let target = text.strip_suffix('\n').unwrap_or(text);
    if target.is_empty() || target.contains(['\n', '\r']) || target.chars().any(char::is_control) {
        return Err(ActivateReleaseError::InvalidCurrent(
            "readlink returned a malformed target".into(),
        ));
    }
    let version = target.strip_prefix("releases/").ok_or_else(|| {
        ActivateReleaseError::InvalidCurrent(
            "current must point to relative releases/<version>".into(),
        )
    })?;
    ReleaseVersion::parse(version.to_owned()).map_err(|_| {
        ActivateReleaseError::InvalidCurrent("current contains an invalid Release version".into())
    })
}

async fn verify_same_filesystem<R: ActivationRemote>(
    remote: &R,
    root: &str,
    temporary: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError> {
    let output = run(
        remote,
        "verify activation filesystem",
        "stat",
        ["--dereference", "--format=%d", "--", root, temporary],
        timeout,
        cancellation,
    )
    .await?;
    if output.exit_status != 0 {
        return Err(ActivateReleaseError::CommandFailed {
            stage: "verify activation filesystem",
            status: output.exit_status,
        });
    }
    if output.stdout_truncated {
        return Err(ActivateReleaseError::InvalidFilesystemOutput);
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| ActivateReleaseError::InvalidFilesystemOutput)?;
    let devices = text.lines().collect::<Vec<_>>();
    if devices.len() != 2
        || devices
            .iter()
            .any(|device| device.is_empty() || !device.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(ActivateReleaseError::InvalidFilesystemOutput);
    }
    if devices[0] != devices[1] {
        return Err(ActivateReleaseError::DifferentFilesystems);
    }
    Ok(())
}

async fn require_path<R: ActivationRemote>(
    remote: &R,
    stage: &'static str,
    predicate: &str,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError> {
    if test_path(remote, stage, predicate, path, timeout, cancellation).await? {
        Ok(())
    } else {
        Err(ActivateReleaseError::MissingPreparedPath(path.into()))
    }
}

async fn reject_symbolic_path<R: ActivationRemote>(
    remote: &R,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError> {
    if test_path(
        remote,
        "verify prepared path type",
        "-L",
        path,
        timeout,
        cancellation,
    )
    .await?
    {
        Err(ActivateReleaseError::PreparedPathIsLink(path.into()))
    } else {
        Ok(())
    }
}

async fn path_exists_any<R: ActivationRemote>(
    remote: &R,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<bool, ActivateReleaseError> {
    if test_path(
        remote,
        "inspect temporary link",
        "-L",
        path,
        timeout,
        cancellation,
    )
    .await?
    {
        return Ok(true);
    }
    test_path(
        remote,
        "inspect temporary path",
        "-e",
        path,
        timeout,
        cancellation,
    )
    .await
}

async fn test_path<R: ActivationRemote>(
    remote: &R,
    stage: &'static str,
    predicate: &str,
    path: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<bool, ActivateReleaseError> {
    let command = CommandSpec::structured("test", [predicate, path].map(CommandArgument::plain))
        .map_err(|error| ActivateReleaseError::InvalidCommand(error.to_string()))?;
    let output = remote
        .predicate(&command, timeout, cancellation)
        .await
        .map_err(|error| map_ssh_error(stage, &error))?;
    match output.exit_status {
        0 => Ok(true),
        1 => Ok(false),
        status => Err(ActivateReleaseError::CommandFailed { stage, status }),
    }
}

async fn remove_temporary_link<R: ActivationRemote>(
    remote: &R,
    path: &str,
    timeout: Duration,
) -> Result<(), ActivateReleaseError> {
    run_required(
        remote,
        "cleanup activation link",
        "rm",
        ["-f", "--", path],
        timeout.min(COMPENSATION_TIMEOUT),
        &CancellationToken::new(),
    )
    .await
}

fn combine_cleanup<T>(
    original: ActivateReleaseError,
    cleanup: Result<(), ActivateReleaseError>,
) -> Result<T, ActivateReleaseError> {
    match cleanup {
        Ok(()) => Err(original),
        Err(cleanup) => Err(ActivateReleaseError::CleanupFailed {
            cause: original.to_string(),
            cleanup: cleanup.to_string(),
        }),
    }
}

async fn run_required<R, I, S>(
    remote: &R,
    stage: &'static str,
    program: &str,
    args: I,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError>
where
    R: ActivationRemote,
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let output = run(remote, stage, program, args, timeout, cancellation).await?;
    if output.exit_status == 0 {
        Ok(())
    } else {
        Err(ActivateReleaseError::CommandFailed {
            stage,
            status: output.exit_status,
        })
    }
}

async fn run_service_action<R: ActivationRemote>(
    remote: &R,
    stage: &'static str,
    commands: &crate::config::ServiceAction,
    directory: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<(), ActivateReleaseError> {
    let deadline = tokio::time::Instant::now() + timeout;
    for argv in commands {
        if cancellation.is_cancelled() {
            return Err(ActivateReleaseError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(ActivateReleaseError::RemoteCommand {
                stage,
                message: "Service action timed out; verify its outcome before retrying.".into(),
            });
        }
        let command = super::service_command(argv, directory).map_err(|_| {
            ActivateReleaseError::InvalidCommand("Invalid service command context".into())
        })?;
        let output = remote
            .command(&command, remaining, cancellation)
            .await
            .map_err(|_| ActivateReleaseError::ServiceOutcomeUnknown { stage })?;
        if output.exit_status != 0 {
            return Err(ActivateReleaseError::CommandFailed {
                stage,
                status: output.exit_status,
            });
        }
    }
    Ok(())
}

async fn run<R, I, S>(
    remote: &R,
    stage: &'static str,
    program: &str,
    args: I,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteCommandOutput, ActivateReleaseError>
where
    R: ActivationRemote,
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let command = CommandSpec::structured(
        program,
        args.into_iter()
            .map(|argument| CommandArgument::plain(argument.into())),
    )
    .map_err(|error| ActivateReleaseError::InvalidCommand(error.to_string()))?;
    remote
        .command(&command, timeout, cancellation)
        .await
        .map_err(|error| map_ssh_error(stage, &error))
}

fn map_ssh_error(stage: &'static str, error: &SshConnectionError) -> ActivateReleaseError {
    if matches!(error, SshConnectionError::Cancelled) {
        ActivateReleaseError::Cancelled
    } else {
        ActivateReleaseError::RemoteCommand {
            stage,
            message: sanitize_remote_error(&error.to_string()),
        }
    }
}

fn display_current(version: Option<&ReleaseVersion>) -> String {
    version.map_or_else(|| "not_deployed".into(), ToString::to_string)
}

fn manual_action(target: &LinuxSshTarget, activation: &ActivatedRemoteRelease) -> String {
    match &activation.previous {
        Some(previous) => format!(
            "inspect `{0}/current` and atomically point it to `releases/{previous}`; then verify the configured service",
            target.root
        ),
        None => format!(
            "inspect and remove `{0}/current` only if it still points to `releases/{1}`; then stop the configured service if present",
            target.root, activation.release.version
        ),
    }
}

#[derive(Debug, Error)]
pub enum ActivateReleaseError {
    #[error(
        "{stage} has no confirmed exit status; inspect the remote process before any retry or recovery"
    )]
    ServiceOutcomeUnknown { stage: &'static str },
    #[error(transparent)]
    UnsafeRemote(#[from] super::MarkerError),
    #[error("activation command timeout must be non-zero")]
    ZeroCommandTimeout,
    #[error("Release activation was cancelled before current changed")]
    Cancelled,
    #[error("Release activation was cancelled and the previous state was restored")]
    CancelledAndCompensated,
    #[error("prepared Release does not match the target, Deployment, or package metadata")]
    InvalidPreparedRelease,
    #[error("activation receipt does not belong to this Component root")]
    InvalidActivationReceipt,
    #[error("Rollback target must differ from the current Release")]
    InvalidRollbackTarget,
    #[error("prepared remote path is missing: `{0}`")]
    MissingPreparedPath(String),
    #[error("prepared remote path unexpectedly became a symbolic link: `{0}`")]
    PreparedPathIsLink(String),
    #[error("activation temporary path already exists and will not be overwritten: `{0}`")]
    Conflict(String),
    #[error("current Release drifted: expected {expected}, got {actual}")]
    Drift { expected: String, actual: String },
    #[error("invalid current link: {0}")]
    InvalidCurrent(String),
    #[error("root and temporary directory are on different filesystems")]
    DifferentFilesystems,
    #[error("remote filesystem identity output was malformed")]
    InvalidFilesystemOutput,
    #[error("could not construct a structured remote command: {0}")]
    InvalidCommand(String),
    #[error("{stage} failed before an exit status was received: {message}")]
    RemoteCommand {
        stage: &'static str,
        message: String,
    },
    #[error("{stage} exited with status {status}")]
    CommandFailed { stage: &'static str, status: u32 },
    #[error("activation failed and the previous state was restored: {cause}")]
    ActivationFailedAndCompensated { cause: String },
    #[error(
        "activation failed ({cause}); compensation failed ({compensation}); manual action: {manual_action}"
    )]
    CompensationFailed {
        cause: String,
        compensation: RecoveryFailure,
        manual_action: String,
    },
    #[error("activation failed ({cause}); temporary-link cleanup also failed ({cleanup})")]
    CleanupFailed { cause: String, cleanup: String },
    #[error("explicit Rollback failed ({cause}); manual action: {manual_action}")]
    ExplicitRollbackFailed {
        cause: RecoveryFailure,
        manual_action: String,
    },
}

#[derive(Debug, Error)]
pub enum RecoveryFailure {
    #[error("{0}")]
    Known(String),
    #[error(
        "service command has no confirmed outcome; inspect the process before further recovery"
    )]
    ServiceOutcomeUnknown,
}

impl From<String> for RecoveryFailure {
    fn from(message: String) -> Self {
        Self::Known(message)
    }
}

impl From<ActivateReleaseError> for RecoveryFailure {
    fn from(error: ActivateReleaseError) -> Self {
        if error.service_outcome_unknown() {
            Self::ServiceOutcomeUnknown
        } else {
            Self::Known(error.to_string())
        }
    }
}

impl ActivateReleaseError {
    pub(super) fn service_outcome_unknown(&self) -> bool {
        matches!(
            self,
            Self::ServiceOutcomeUnknown { .. }
                | Self::CompensationFailed {
                    compensation: RecoveryFailure::ServiceOutcomeUnknown,
                    ..
                }
                | Self::ExplicitRollbackFailed {
                    cause: RecoveryFailure::ServiceOutcomeUnknown,
                    ..
                }
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::Mutex,
    };

    use super::*;
    use crate::{
        domain::{
            ComponentGeneration, ComponentName, DestinationKey, DestinationRevision, EnvironmentId,
            ProjectId,
        },
        drivers::{DriverTargetInput, linux_ssh::RemotePath},
    };

    #[derive(Debug)]
    struct FakeState {
        files: BTreeSet<String>,
        manifests: BTreeMap<String, Vec<u8>>,
        directories: BTreeSet<String>,
        links: BTreeMap<String, String>,
        commands: Vec<String>,
        restart_failures: usize,
        service_unknown: bool,
        service_calls: Vec<(String, Vec<String>, String)>,
        cancel_after_move: Option<CancellationToken>,
        drift_on_readlink: Option<usize>,
        readlinks: usize,
        different_filesystems: bool,
    }

    #[derive(Debug)]
    struct FakeRemote {
        state: Mutex<FakeState>,
    }

    impl FakeRemote {
        fn fixture(
            target: &LinuxSshTarget,
            prepared: &PreparedRemoteRelease,
            previous: Option<&ReleaseVersion>,
        ) -> Self {
            let mut directories = BTreeSet::from([
                target.root.clone(),
                format!("{}/temporary", target.root),
                prepared.release_directory().to_owned(),
            ]);
            let current = format!("{}/current", target.root);
            let mut links = BTreeMap::new();
            let mut files = BTreeSet::from([
                prepared.archive_path().as_str().to_owned(),
                format!("{}/manifest.json", prepared.release_directory()),
            ]);
            let mut manifests = BTreeMap::from([(
                format!("{}/manifest.json", prepared.release_directory()),
                serde_json::to_vec(&crate::domain::ReleaseManifest::new(
                    prepared.release(),
                    1,
                    None,
                ))
                .unwrap(),
            )]);
            if let Some(previous) = previous {
                let previous_directory = format!("{}/releases/{previous}", target.root);
                directories.insert(previous_directory.clone());
                files.insert(format!("{previous_directory}/manifest.json"));
                let mut previous_release = prepared.release().clone();
                previous_release.version = previous.clone();
                manifests.insert(
                    format!("{previous_directory}/manifest.json"),
                    serde_json::to_vec(&crate::domain::ReleaseManifest::new(
                        &previous_release,
                        1,
                        None,
                    ))
                    .unwrap(),
                );
                links.insert(current, format!("releases/{previous}"));
            }
            Self {
                state: Mutex::new(FakeState {
                    files,
                    manifests,
                    directories,
                    links,
                    commands: Vec::new(),
                    restart_failures: 0,
                    service_unknown: false,
                    service_calls: Vec::new(),
                    cancel_after_move: None,
                    drift_on_readlink: None,
                    readlinks: 0,
                    different_filesystems: false,
                }),
            }
        }

        fn current(&self, target: &LinuxSshTarget) -> Option<String> {
            self.state
                .lock()
                .unwrap()
                .links
                .get(&format!("{}/current", target.root))
                .cloned()
        }
    }

    #[async_trait]
    impl ActivationRemote for FakeRemote {
        #[allow(clippy::too_many_lines)] // In-memory remote command simulator.
        async fn command(
            &self,
            command: &CommandSpec,
            _timeout: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<RemoteCommandOutput, SshConnectionError> {
            let args = command
                .args
                .iter()
                .map(|argument| argument.expose_for_execution().to_owned())
                .collect::<Vec<_>>();
            let mut state = self.state.lock().unwrap();
            state.commands.push(command.render_posix().unwrap());
            if let Some(directory) = &command.working_directory {
                state.service_calls.push((
                    command.program.clone(),
                    args.clone(),
                    directory.clone(),
                ));
                assert!(state.directories.contains(directory));
                if state.service_unknown {
                    return Err(SshConnectionError::MissingExitStatus);
                }
                if command.program == "pm2" {
                    return Ok(output(0, b""));
                }
                if command.program == "false" {
                    return Ok(output(1, b""));
                }
            }
            if command.program == "systemctl"
                && args.first().map(String::as_str) == Some("restart")
                && state.restart_failures > 0
            {
                state.restart_failures -= 1;
                return Ok(output(1, b""));
            }

            match command.program.as_str() {
                "head" => match state.manifests.get(args.last().unwrap()) {
                    Some(bytes) => Ok(output(0, &bytes[..bytes.len().min(8193)])),
                    None => Ok(output(1, b"")),
                },
                "test" => {
                    let predicate = &args[0];
                    let path = &args[1];
                    let exists = match predicate.as_str() {
                        "-L" => state.links.contains_key(path),
                        "-e" => {
                            state.links.contains_key(path)
                                || state.files.contains(path)
                                || state.directories.contains(path)
                        }
                        "-f" => state.files.contains(path),
                        "-d" => {
                            if let Some(link) = state.links.get(path) {
                                let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
                                state.directories.contains(&format!("{parent}/{link}"))
                            } else {
                                state.directories.contains(path)
                            }
                        }
                        _ => false,
                    };
                    Ok(output(u32::from(!exists), b""))
                }
                "readlink" => {
                    state.readlinks += 1;
                    if state.drift_on_readlink == Some(state.readlinks) {
                        let path = args.last().unwrap().clone();
                        state
                            .directories
                            .insert(format!("{}/releases/operator", target_parent(&path)));
                        state.links.insert(path, "releases/operator".into());
                    }
                    let path = args.last().unwrap();
                    let link = state.links.get(path).cloned().unwrap_or_default();
                    Ok(output(0, format!("{link}\n").as_bytes()))
                }
                "stat" => {
                    if state.different_filesystems {
                        Ok(output(0, b"1\n2\n"))
                    } else {
                        Ok(output(0, b"1\n1\n"))
                    }
                }
                "ln" => {
                    let target = args[2].clone();
                    let path = args[3].clone();
                    if state.links.contains_key(&path)
                        || state.files.contains(&path)
                        || state.directories.contains(&path)
                    {
                        Ok(output(1, b""))
                    } else {
                        state.links.insert(path, target);
                        Ok(output(0, b""))
                    }
                }
                "mv" => {
                    let source = args[2].clone();
                    let destination = args[3].clone();
                    let Some(link) = state.links.remove(&source) else {
                        return Ok(output(1, b""));
                    };
                    state.links.insert(destination, link);
                    if let Some(cancellation) = state.cancel_after_move.take() {
                        cancellation.cancel();
                    }
                    Ok(output(0, b""))
                }
                "rm" => {
                    let path = args.last().unwrap();
                    state.links.remove(path);
                    Ok(output(0, b""))
                }
                "systemctl" => Ok(output(0, b"")),
                _ => Ok(output(127, b"")),
            }
        }
    }

    fn output(status: u32, stdout: &[u8]) -> RemoteCommandOutput {
        RemoteCommandOutput {
            exit_status: status,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn target_parent(current: &str) -> &str {
        current.strip_suffix("/current").unwrap()
    }

    fn release(version: &str) -> ComponentRelease {
        ComponentRelease {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse(version).unwrap(),
            destination: DestinationKey::parse("dst_00000000000000000000000000000001").unwrap(),
            destination_revision: DestinationRevision::INITIAL,
        }
    }

    fn target(systemd: bool) -> LinuxSshTarget {
        LinuxSshTarget::validate(&DriverTargetInput {
            value: serde_json::json!({
                "root": "/srv/shipforge/project/production/api",
                "service": systemd.then(|| crate::config::ServiceConfig::systemd("api.service")),
                "health": null
            }),
        })
        .unwrap()
    }

    fn prepared(target: &LinuxSshTarget, deployment: &DeploymentId) -> PreparedRemoteRelease {
        let release = release("v2");
        let paths = ReleasePaths::new(target, &release, deployment);
        PreparedRemoteRelease::new(
            deployment.clone(),
            release,
            RemotePath::parse(paths.archive.as_str()).unwrap(),
            paths.final_directory,
            "a".repeat(64),
            42,
        )
    }

    fn custom_target() -> LinuxSshTarget {
        let mut target = target(false);
        target.service = Some(crate::config::ServiceConfig {
            start: vec![vec!["pm2".into(), "start".into(), "ecosystem.cjs".into()]],
            update: vec![vec![
                "pm2".into(),
                "startOrReload".into(),
                "ecosystem.cjs".into(),
            ]],
            restore: vec![vec![
                "pm2".into(),
                "startOrRestart".into(),
                "ecosystem.cjs".into(),
            ]],
            stop: vec![vec!["pm2".into(), "delete".into(), "api".into()]],
            check: None,
        });
        target
    }

    #[tokio::test]
    async fn custom_actions_use_distinct_lifecycles_and_actual_version_directories() {
        for first in [true, false] {
            let target = custom_target();
            let deployment = DeploymentId::new();
            let prepared = prepared(&target, &deployment);
            let previous = (!first).then(|| ReleaseVersion::parse("v1").unwrap());
            let remote = FakeRemote::fixture(&target, &prepared, previous.as_ref());
            let activated = activate_with_remote(
                &remote,
                &target,
                &prepared,
                previous.as_ref(),
                &deployment,
                ActivationOptions::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            compensate(
                &remote,
                &target,
                &activated,
                &deployment,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            let state = remote.state.lock().unwrap();
            assert_eq!(state.service_calls.len(), 2);
            assert_eq!(
                state.service_calls[0].1[0],
                if first { "start" } else { "startOrReload" }
            );
            assert!(state.service_calls[0].2.ends_with("/releases/v2"));
            assert_eq!(
                state.service_calls[1].1[0],
                if first { "delete" } else { "startOrRestart" }
            );
            assert!(state.service_calls[1].2.ends_with(if first {
                "/releases/v2"
            } else {
                "/releases/v1"
            }));
        }
    }

    #[tokio::test]
    async fn unknown_service_outcome_never_starts_automatic_recovery() {
        let target = custom_target();
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        remote.state.lock().unwrap().service_unknown = true;
        let error = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.service_outcome_unknown());
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v2"));
        assert_eq!(remote.state.lock().unwrap().service_calls.len(), 1);
        let error = rollback_with_remote(
            &remote,
            &target,
            prepared.release(),
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.service_outcome_unknown());
    }

    #[tokio::test]
    async fn known_custom_failure_stops_remaining_commands_and_compensates_once() {
        let mut target = custom_target();
        target.service.as_mut().unwrap().start = vec![
            vec!["false".into()],
            vec!["pm2".into(), "must-not-run".into()],
        ];
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let remote = FakeRemote::fixture(&target, &prepared, None);
        let error = activate_with_remote(
            &remote,
            &target,
            &prepared,
            None,
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ActivateReleaseError::ActivationFailedAndCompensated { .. }
        ));
        assert_eq!(remote.current(&target), None);
        let state = remote.state.lock().unwrap();
        assert_eq!(state.service_calls.len(), 2);
        assert_eq!(state.service_calls[1].1[0], "delete");
    }

    #[tokio::test]
    async fn observes_only_canonical_non_dangling_current_links() {
        let target = target(false);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        assert_eq!(
            observe_with_remote(
                &remote,
                &target,
                Duration::from_secs(1),
                &CancellationToken::new()
            )
            .await
            .unwrap(),
            Some(previous)
        );

        remote.state.lock().unwrap().links.insert(
            format!("{}/current", target.root),
            "/srv/elsewhere/v1".into(),
        );
        assert!(matches!(
            observe_with_remote(
                &remote,
                &target,
                Duration::from_secs(1),
                &CancellationToken::new()
            )
            .await,
            Err(ActivateReleaseError::InvalidCurrent(_))
        ));

        {
            let mut state = remote.state.lock().unwrap();
            state
                .links
                .insert(format!("{}/current", target.root), "releases/v1".into());
            state.links.insert(
                format!("{}/releases/v1", target.root),
                "/srv/elsewhere/v1".into(),
            );
        }
        assert!(matches!(
            observe_with_remote(
                &remote,
                &target,
                Duration::from_secs(1),
                &CancellationToken::new()
            )
            .await,
            Err(ActivateReleaseError::InvalidCurrent(_))
        ));
    }

    #[tokio::test]
    async fn atomically_activates_and_restarts_systemd() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        let receipt = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(receipt.previous(), Some(&previous));
        assert!(receipt.service_started());
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v2"));
        let commands = &remote.state.lock().unwrap().commands;
        assert!(
            commands
                .iter()
                .any(|command| { command.starts_with("'mv' '--no-target-directory' '--'") })
        );
        assert!(
            commands
                .iter()
                .any(|command| { command == "cd -- '/srv/shipforge/project/production/api/releases/v2' && exec 'systemctl' 'restart' '--' 'api.service'" })
        );
    }

    #[tokio::test]
    async fn drift_before_switch_removes_only_owned_temporary_link() {
        let target = target(false);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        remote.state.lock().unwrap().drift_on_readlink = Some(2);

        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(ActivateReleaseError::Drift { .. })));
        assert_eq!(
            remote.current(&target).as_deref(),
            Some("releases/operator")
        );
        assert!(
            !remote
                .state
                .lock()
                .unwrap()
                .links
                .contains_key(&format!("{}/temporary/{}.current", target.root, deployment))
        );
    }

    #[tokio::test]
    async fn restart_failure_restores_previous_release_and_service() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        remote.state.lock().unwrap().restart_failures = 1;

        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ActivateReleaseError::ActivationFailedAndCompensated { .. })
        ));
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
        let restart_count = remote
            .state
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|command| command.ends_with("exec 'systemctl' 'restart' '--' 'api.service'"))
            .count();
        assert_eq!(restart_count, 2);
    }

    #[tokio::test]
    async fn first_deployment_failure_removes_current_and_stops_service() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let remote = FakeRemote::fixture(&target, &prepared, None);
        remote.state.lock().unwrap().restart_failures = 1;

        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            None,
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ActivateReleaseError::ActivationFailedAndCompensated { .. })
        ));
        assert_eq!(remote.current(&target), None);
        assert!(
            remote
                .state
                .lock()
                .unwrap()
                .commands
                .iter()
                .any(|command| command == "cd -- '/srv/shipforge/project/production/api/releases/v2' && exec 'systemctl' 'stop' '--' 'api.service'")
        );
    }

    #[tokio::test]
    async fn cancellation_after_switch_compensates_with_fresh_token() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        let cancellation = CancellationToken::new();
        remote.state.lock().unwrap().cancel_after_move = Some(cancellation.clone());

        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &cancellation,
        )
        .await;
        assert!(matches!(
            result,
            Err(ActivateReleaseError::CancelledAndCompensated)
        ));
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
    }

    #[tokio::test]
    async fn different_filesystem_stops_before_creating_a_link() {
        let target = target(false);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let remote = FakeRemote::fixture(&target, &prepared, None);
        remote.state.lock().unwrap().different_filesystems = true;
        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            None,
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ActivateReleaseError::DifferentFilesystems)
        ));
        assert!(remote.state.lock().unwrap().links.is_empty());
    }

    #[tokio::test]
    async fn compensation_refuses_to_overwrite_external_drift() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        let receipt = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        {
            let mut state = remote.state.lock().unwrap();
            state
                .directories
                .insert(format!("{}/releases/operator", target.root));
            state.links.insert(
                format!("{}/current", target.root),
                "releases/operator".into(),
            );
        }
        let result = compensate(
            &remote,
            &target,
            &receipt,
            &deployment,
            Duration::from_secs(1),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("drifted before compensation")
        );
        assert_eq!(
            remote.current(&target).as_deref(),
            Some("releases/operator")
        );
    }

    #[tokio::test]
    async fn prepared_receipt_is_bound_to_its_deployment() {
        let target = target(false);
        let prepared_for = DeploymentId::new();
        let other = DeploymentId::new();
        let prepared = prepared(&target, &prepared_for);
        let remote = FakeRemote::fixture(&target, &prepared, None);
        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            None,
            &other,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ActivateReleaseError::InvalidPreparedRelease)
        ));
        assert!(remote.state.lock().unwrap().commands.is_empty());
    }

    #[tokio::test]
    async fn later_health_failure_can_use_the_sealed_compensation_receipt() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let rollback = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        let receipt = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        compensate(
            &remote,
            &target,
            &receipt,
            &rollback,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
    }

    #[tokio::test]
    async fn explicit_rollback_switches_to_historical_release() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let rollback = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let receipt = rollback_with_remote(
            &remote,
            &target,
            prepared.release(),
            Some(&previous),
            &rollback,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(receipt.current(), Some(&previous));
        assert!(receipt.service_updated());
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
    }

    #[tokio::test]
    async fn explicit_rollback_can_restore_not_deployed_state() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let remote = FakeRemote::fixture(&target, &prepared, None);
        let activated = activate_with_remote(
            &remote,
            &target,
            &prepared,
            None,
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let rollback = DeploymentId::new();
        let receipt = rollback_with_remote(
            &remote,
            &target,
            activated.release(),
            None,
            &rollback,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(receipt.current(), None);
        assert_eq!(remote.current(&target), None);
        restore_undeployed_with_remote(
            &remote,
            &target,
            activated.release(),
            &DeploymentId::new(),
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            remote.current(&target),
            Some(format!("releases/{}", activated.release().version))
        );
    }

    #[tokio::test]
    async fn restore_undeployed_rejects_existing_current_and_early_cancellation() {
        let target = target(false);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        assert!(
            restore_undeployed_with_remote(
                &remote,
                &target,
                prepared.release(),
                &DeploymentId::new(),
                ActivationOptions::default(),
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            restore_undeployed_with_remote(
                &remote,
                &target,
                prepared.release(),
                &DeploymentId::new(),
                ActivationOptions::default(),
                &cancelled,
            )
            .await,
            Err(ActivateReleaseError::Cancelled)
        ));
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
    }

    #[tokio::test]
    async fn explicit_rollback_rejects_a_linked_historical_directory() {
        let target = target(false);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        {
            let mut state = remote.state.lock().unwrap();
            let historical = format!("{}/releases/v1", target.root);
            state.directories.remove(&historical);
            state
                .directories
                .insert(format!("{}/releases/v1-real", target.root));
            state.links.insert(historical, "v1-real".into());
        }
        let result = rollback_with_remote(
            &remote,
            &target,
            prepared.release(),
            Some(&previous),
            &DeploymentId::new(),
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ActivateReleaseError::ExplicitRollbackFailed { .. })
        ));
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v2"));
    }

    #[tokio::test]
    async fn failed_compensation_reports_manual_action_without_hiding_actual_current() {
        let target = target(true);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let previous = ReleaseVersion::parse("v1").unwrap();
        let remote = FakeRemote::fixture(&target, &prepared, Some(&previous));
        remote.state.lock().unwrap().restart_failures = 2;
        let result = activate_with_remote(
            &remote,
            &target,
            &prepared,
            Some(&previous),
            &deployment,
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        let Err(ActivateReleaseError::CompensationFailed { manual_action, .. }) = result else {
            panic!("expected failed compensation");
        };
        assert!(manual_action.contains("releases/v1"));
        assert_eq!(remote.current(&target).as_deref(), Some("releases/v1"));
    }

    #[tokio::test]
    async fn historical_manifest_identity_mismatch_stops_before_restoring_a_link() {
        let target = target(false);
        let deployment = DeploymentId::new();
        let prepared = prepared(&target, &deployment);
        let remote = FakeRemote::fixture(&target, &prepared, None);
        let path = format!("{}/manifest.json", prepared.release_directory());
        let mut manifest = crate::domain::ReleaseManifest::new(prepared.release(), 1, None);
        manifest.project_id = ProjectId::new();
        remote
            .state
            .lock()
            .unwrap()
            .manifests
            .insert(path, serde_json::to_vec(&manifest).unwrap());
        let error = restore_undeployed_with_remote(
            &remote,
            &target,
            prepared.release(),
            &DeploymentId::new(),
            ActivationOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("manifest identity"));
        assert_eq!(remote.current(&target), None);
        assert!(
            !remote
                .state
                .lock()
                .unwrap()
                .commands
                .iter()
                .any(|command| command.starts_with("'ln'"))
        );
    }
}
