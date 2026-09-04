//! Exact single-version deletion. Inventory and every path mutation are checked
//! independently; transport uncertainty is returned as partial path evidence.

use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    drivers::{
        CleanupCandidate, CleanupPartial, CleanupPathState, CleanupReport,
        ComponentExecutionContext, RetentionPolicy, inventory::ReleaseInventory,
    },
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    AuthenticatedSession, DeploymentMarker, LinuxSshTarget, RemoteCommandOutput, RemotePath,
};

// Including the Driver's 15s connect: 15 + 85 + 2*30 + 15 = 175s,
// below the application 180s budget. A slow inventory fails before mutation.
const CHECK_TIMEOUT: Duration = Duration::from_secs(85);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

// Bash pipefail is required so an incomplete find/head pipeline cannot authorize
// removal. Arguments are separate quoted values, never inserted into this script.
// No namespace/marker/audit/temporary file is created, renamed or repaired.
const SCRIPT: &str = r#"set -eu
export LC_ALL=C
mode=$1 root=$2 version=$3 marker_digest=$4 manifest_digest=$5 digest=$6 size=$7 expected_current=$8
root_id=$9 archives_id=${10} releases_id=${11} archive_stamp=${12} directory_stamp=${13}
fail() { exit 42; }
identity() { stat -L -c '%d:%i' -- "$1"; }
stamp() { stat -c '%d:%i:%s:%y:%z:%h:%f' -- "$1"; }
ancestors() {
  p=$root
  while [ "$p" != / ]; do
    [ ! -L "$p" ] && [ -d "$p" ] || fail
    p=${p%/*}; [ -n "$p" ] || p=/
  done
}
ancestors
initial_root=$(stat -c '%d:%i' -- "$root")
exec 3< "$root"
[ "$(identity /proc/self/fd/3)" = "$initial_root" ] || fail
r=/proc/self/fd/3
for child in archives releases; do
  [ ! -L "$r/$child" ] && [ -d "$r/$child" ] || fail
done
initial_archives=$(stat -c '%d:%i' -- "$r/archives")
initial_releases=$(stat -c '%d:%i' -- "$r/releases")
exec 4< "$r/archives"
exec 5< "$r/releases"
[ "$(identity /proc/self/fd/4)" = "$initial_archives" ] || fail
[ "$(identity /proc/self/fd/5)" = "$initial_releases" ] || fail
if [ "$mode" = inspect ]; then
  root_id=$initial_root archives_id=$initial_archives releases_id=$initial_releases
fi
layout() {
  ancestors
  [ "$(stat -c '%d:%i' -- "$root")" = "$root_id" ] || fail
  [ "$(identity "$r")" = "$root_id" ] || fail
  [ ! -L "$r/archives" ] && [ ! -L "$r/releases" ] || fail
  [ "$(stat -c '%d:%i' -- "$r/archives")" = "$archives_id" ] || fail
  [ "$(stat -c '%d:%i' -- "$r/releases")" = "$releases_id" ] || fail
  [ "$(identity /proc/self/fd/4)" = "$archives_id" ] || fail
  [ "$(identity /proc/self/fd/5)" = "$releases_id" ] || fail
  marker=$r/.shipforge-project.json
  [ ! -L "$marker" ] && [ -f "$marker" ] || fail
  [ "$(stat -c %h -- "$marker")" = 1 ] || fail
  [ "$(stat -c %s -- "$marker")" -le 4096 ] || fail
  before_marker=$(stamp "$marker")
  exec 7< "$marker"
  [ "$(stat -L -c '%d:%i:%s:%y:%z:%h:%f' -- /proc/self/fd/7)" = "$before_marker" ] || fail
  [ "$(sha256sum /proc/self/fd/7)" = "$marker_digest  /proc/self/fd/7" ] || fail
  exec 7<&-
  [ ! -L "$marker" ] && [ "$(stamp "$marker")" = "$before_marker" ] || fail
}
current() {
  if [ -z "$expected_current" ]; then
    [ ! -L "$r/current" ] && [ ! -e "$r/current" ] || fail
  else
    [ -L "$r/current" ] || fail
    [ "$(readlink -- "$r/current")" = "releases/$expected_current" ] || fail
    [ "$expected_current" != "$version" ] || fail
  fi
}
archive=/proc/self/fd/4/$version.tar.gz
directory=/proc/self/fd/5/$version
present() {
  if [ -L "$1" ] || [ -e "$1" ]; then printf 'present\n'; else printf 'absent\n'; fi
}
layout
if [ "$mode" = observe ]; then
  present "$archive"
  present "$directory"
  layout
  exit 0
fi
current
[ ! -L "$archive" ] && [ -f "$archive" ] || fail
[ "$(stat -c %h -- "$archive")" = 1 ] || fail
[ "$(stat -c %s -- "$archive")" = "$size" ] || fail
initial_archive=$(stamp "$archive")
exec 6< "$archive"
[ "$(stat -L -c '%d:%i:%s:%y:%z:%h:%f' -- /proc/self/fd/6)" = "$initial_archive" ] || fail
[ "$(sha256sum /proc/self/fd/6)" = "$digest  /proc/self/fd/6" ] || fail
[ ! -L "$archive" ] && [ "$(stamp "$archive")" = "$initial_archive" ] || fail
initial_directory=absent
if [ -L "$directory" ] || [ -e "$directory" ]; then
  [ ! -L "$directory" ] && [ -d "$directory" ] || fail
  initial_directory=$(stamp "$directory")
  [ ! -L "$directory/manifest.json" ] && [ -f "$directory/manifest.json" ] || fail
  [ "$(stat -c %h -- "$directory/manifest.json")" = 1 ] || fail
  [ "$(stat -c %s -- "$directory/manifest.json")" -le 8192 ] || fail
  before_manifest=$(stamp "$directory/manifest.json")
  exec 8< "$directory/manifest.json"
  [ "$(stat -L -c '%d:%i:%s:%y:%z:%h:%f' -- /proc/self/fd/8)" = "$before_manifest" ] || fail
  [ "$(sha256sum /proc/self/fd/8)" = "$manifest_digest  /proc/self/fd/8" ] || fail
  exec 8<&-
  [ ! -L "$directory/manifest.json" ] && [ -f "$directory/manifest.json" ] || fail
  [ "$(stat -c %h -- "$directory/manifest.json")" = 1 ] || fail
  [ "$(stamp "$directory/manifest.json")" = "$before_manifest" ] || fail
fi
if [ "$mode" = inspect ]; then
  archive_stamp=$initial_archive directory_stamp=$initial_directory
fi
[ "$initial_archive" = "$archive_stamp" ] || fail
[ "$initial_directory" = "$directory_stamp" ] || fail
if [ "$mode" = inspect ]; then
  layout; current
  printf '%s\n' "$root_id" "$archives_id" "$releases_id" "$archive_stamp" "$directory_stamp"
  exit 0
fi
# /proc mountinfo includes same-device bind mounts, unlike rm --one-file-system.
# RemotePath rejects control characters; spaces/backslashes are mountinfo-escaped.
encoded=$(printf '%s' "$root/releases/$version" | sed -e 's/\\/\\134/g' -e 's/ /\\040/g')
while IFS=' ' read -r mount_id parent_id device mount_root mountpoint rest; do
  case "$mountpoint" in "$encoded"|"$encoded"/*) fail;; esac
done < /proc/self/mountinfo
case "$mode" in
  directory)
    [ "$directory_stamp" != absent ] || fail
    # Never follow a payload link, cross a device, or recursively scan without a cap.
    # Preserve terminal LF while counting: command substitution strips it otherwise.
    tree=$(find -P "$directory" -xdev -printf '%D:%y:%s\n' | head -c 65537 && printf x) || fail
    [ "${#tree}" -le 65537 ] || fail
    tree=${tree%x}
    dev=$(stat -c %d -- "$directory")
    printf '%s' "$tree" | awk -F: -v dev="$dev" '
      { if (NF!=3 || $1!=dev || ($2!="d" && $2!="f") || $3!~/^[0-9]+$/) exit 1;
        total+=$3; if (NR>4096 || total>4294967296) exit 1; }' || fail
    [ ! -L "$directory" ] && [ "$(stamp "$directory")" = "$directory_stamp" ] || fail
    layout; current
    [ ! -L "$archive" ] && [ "$(stamp "$archive")" = "$archive_stamp" ] || fail
    rm --recursive --one-file-system --preserve-root=all -- "$directory"
    [ ! -L "$directory" ] && [ ! -e "$directory" ] || fail
    ;;
  archive)
    [ "$directory_stamp" = absent ] || fail
    [ ! -L "$directory" ] && [ ! -e "$directory" ] || fail
    layout; current
    [ ! -L "$archive" ] && [ "$(stamp "$archive")" = "$archive_stamp" ] || fail
    rm -- "$archive"
    [ ! -L "$archive" ] && [ ! -e "$archive" ] || fail
    ;;
  *) fail;;
esac
layout; current
printf 'absent\n'
"#;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RetentionError {
    #[error("cleanup candidate or retention policy does not match this exact Component context")]
    Candidate,
    #[error("cleanup candidate is current, protected, or among the newest retained Releases")]
    Protected,
    #[error("cleanup requires complete, unchanged inventory and matching package evidence")]
    Inventory,
    #[error("cleanup path, marker, file identity, tree or mount layout is unsafe or changed")]
    UnsafePath,
    #[error("cleanup was cancelled before any path mutation")]
    Cancelled,
    #[error("cleanup check exceeded its bounded deadline")]
    Timeout,
    #[error("cleanup could not verify the remote state")]
    Remote,
}

#[derive(Clone, Debug)]
struct Evidence {
    marker_hash: String,
    manifest_hash: String,
}

#[derive(Clone, Debug)]
struct Guard {
    root: String,
    archives: String,
    releases: String,
    archive: String,
    directory: String,
}

#[async_trait]
trait RetentionRemote: Sync {
    async fn inventory(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<ReleaseInventory, RetentionError>;
    async fn evidence(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        candidate: &CleanupCandidate,
        cancellation: &CancellationToken,
    ) -> Result<Evidence, RetentionError>;
    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, RetentionError>;
}

#[async_trait]
impl RetentionRemote for AuthenticatedSession {
    async fn inventory(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<ReleaseInventory, RetentionError> {
        self.release_inventory(target, marker, cancellation)
            .await
            .map_err(|_| RetentionError::Inventory)
    }

    async fn evidence(
        &self,
        target: &LinuxSshTarget,
        marker: &DeploymentMarker,
        candidate: &CleanupCandidate,
        cancellation: &CancellationToken,
    ) -> Result<Evidence, RetentionError> {
        if !self
            .check_deployment_marker(target, marker, cancellation)
            .await
            .map_err(|_| RetentionError::UnsafePath)?
        {
            return Err(RetentionError::UnsafePath);
        }
        let bytes = read_bounded(
            self,
            &format!("{}/.shipforge-project.json", target.root),
            4096,
            cancellation,
        )
        .await?;
        marker
            .validate(&bytes)
            .map_err(|_| RetentionError::UnsafePath)?;
        let marker_hash = byte_digest(&bytes);
        let manifest_hash = if candidate.package.extracted {
            let bytes = read_bounded(
                self,
                &format!(
                    "{}/releases/{}/manifest.json",
                    target.root, candidate.release.version
                ),
                8192,
                cancellation,
            )
            .await?;
            let actual: crate::domain::ReleaseManifest =
                serde_json::from_slice(&bytes).map_err(|_| RetentionError::Inventory)?;
            if actual != candidate.package.manifest {
                return Err(RetentionError::Inventory);
            }
            byte_digest(&bytes)
        } else {
            String::new()
        };
        Ok(Evidence {
            marker_hash,
            manifest_hash,
        })
    }

    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, RetentionError> {
        self.execute(command, COMMAND_TIMEOUT, cancellation)
            .await
            .map_err(|_| RetentionError::Remote)
    }
}

impl AuthenticatedSession {
    /// Deletes only the exact authorized non-current candidate, never selecting others.
    /// Partial or uncertain effects retain independent archive/directory evidence.
    ///
    /// # Errors
    /// Returns an error only before any deletion command could have run.
    pub async fn cleanup_release(
        &self,
        target: &LinuxSshTarget,
        context: &ComponentExecutionContext,
        policy: &RetentionPolicy,
    ) -> Result<CleanupReport, RetentionError> {
        cleanup_with_remote(self, target, context, policy).await
    }
}

async fn read_bounded(
    remote: &impl RetentionRemote,
    path: &str,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, RetentionError> {
    let count = (limit + 1).to_string();
    let command = CommandSpec::structured(
        "timeout",
        ["--kill-after=2s", "10s", "head", "-c", &count, "--", path].map(CommandArgument::plain),
    )
    .map_err(|_| RetentionError::Remote)?;
    let output = remote.command(&command, cancellation).await?;
    if output.exit_status != 0 || output.stdout_truncated || output.stdout.len() > limit {
        return Err(RetentionError::UnsafePath);
    }
    Ok(output.stdout)
}

fn byte_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn validate_candidate(
    context: &ComponentExecutionContext,
    policy: &RetentionPolicy,
) -> Result<(), RetentionError> {
    let release = &policy.candidate.release;
    let package = &policy.candidate.package;
    let manifest = &package.manifest;
    if !(1..=100).contains(&policy.retain_count)
        || release.driver.as_str() != "linux-ssh"
        || release.project_id != context.project_id
        || release.environment_id != context.environment_id
        || release.component != context.component
        || release.generation != context.generation
        || release.destination != context.destination
        || release.destination_revision != context.destination_revision
        || release.endpoint_fingerprint != context.endpoint_fingerprint
        || manifest.schema_version != 1
        || manifest.project_id != release.project_id
        || manifest.environment_id != release.environment_id
        || manifest.component != release.component
        || manifest.generation != release.generation
        || manifest.version != release.version
        || package.size == 0
        || package.size > MAX_ARCHIVE_BYTES
        || package.sha256.len() != 64
        || !package
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RetentionError::Candidate);
    }
    if policy.protected_versions.contains(&release.version)
        || policy.candidate.expected_current.as_ref() == Some(&release.version)
    {
        return Err(RetentionError::Protected);
    }
    Ok(())
}

fn validate_inventory(
    inventory: &ReleaseInventory,
    policy: &RetentionPolicy,
) -> Result<Vec<crate::domain::ReleaseVersion>, RetentionError> {
    if inventory.issues.iter().any(|issue| {
        !inventory.releases.iter().any(|entry| {
            !entry.extracted && issue.version.as_ref() == Some(&entry.manifest.version)
        })
    }) || inventory.releases.is_empty()
        || inventory.releases.len() > 1024
        || inventory
            .releases
            .iter()
            .map(|entry| &entry.manifest.version)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != inventory.releases.len()
        || inventory.current.as_ref() != Ok(&policy.candidate.expected_current)
        || inventory
            .releases
            .iter()
            .filter(|entry| entry.manifest.version == policy.candidate.release.version)
            .collect::<Vec<_>>()
            .as_slice()
            != [&policy.candidate.package]
    {
        return Err(RetentionError::Inventory);
    }
    let mut newest: Vec<_> = inventory.releases.iter().collect();
    newest.sort_by(|left, right| {
        (right.manifest.created_at_unix, &right.manifest.version)
            .cmp(&(left.manifest.created_at_unix, &left.manifest.version))
    });
    if newest
        .iter()
        .take(policy.retain_count)
        .any(|entry| entry.manifest.version == policy.candidate.release.version)
    {
        return Err(RetentionError::Protected);
    }
    Ok(inventory
        .releases
        .iter()
        .filter(|entry| entry.manifest.version != policy.candidate.release.version)
        .map(|entry| entry.manifest.version.clone())
        .collect())
}

async fn cleanup_with_remote(
    remote: &impl RetentionRemote,
    target: &LinuxSshTarget,
    context: &ComponentExecutionContext,
    policy: &RetentionPolicy,
) -> Result<CleanupReport, RetentionError> {
    validate_candidate(context, policy)?;
    RemotePath::parse(target.root.clone()).map_err(|_| RetentionError::UnsafePath)?;
    let cancellation = &context.cancellation;
    let checked = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RetentionError::Cancelled),
        result = tokio::time::timeout(CHECK_TIMEOUT, check(remote, target, context, policy)) => result.map_err(|_| RetentionError::Timeout)??,
    };
    mutate(remote, target, policy, checked, cancellation).await
}

struct Checked {
    evidence: Evidence,
    guard: Guard,
    retained: Vec<crate::domain::ReleaseVersion>,
}

async fn check(
    remote: &impl RetentionRemote,
    target: &LinuxSshTarget,
    context: &ComponentExecutionContext,
    policy: &RetentionPolicy,
) -> Result<Checked, RetentionError> {
    let marker = DeploymentMarker::for_context(context);
    let inventory = remote
        .inventory(target, &marker, &context.cancellation)
        .await?;
    let retained = validate_inventory(&inventory, policy)?;
    let evidence = remote
        .evidence(target, &marker, &policy.candidate, &context.cancellation)
        .await?;
    let command = script_command("inspect", target, policy, &evidence, None)?;
    let output = remote.command(&command, &context.cancellation).await?;
    let guard = parse_guard(output)?;
    if (guard.directory != "absent") != policy.candidate.package.extracted {
        return Err(RetentionError::Inventory);
    }
    Ok(Checked {
        evidence,
        guard,
        retained,
    })
}

fn script_command(
    mode: &str,
    target: &LinuxSshTarget,
    policy: &RetentionPolicy,
    evidence: &Evidence,
    guard: Option<&Guard>,
) -> Result<CommandSpec, RetentionError> {
    let size = policy.candidate.package.size.to_string();
    let current = policy
        .candidate
        .expected_current
        .as_ref()
        .map_or("", crate::domain::ReleaseVersion::as_str);
    let values = [
        "--signal=TERM",
        "--kill-after=2s",
        "25s",
        "bash",
        "-o",
        "pipefail",
        "-c",
        SCRIPT,
        "shipforge-cleanup",
        mode,
        &target.root,
        policy.candidate.release.version.as_str(),
        &evidence.marker_hash,
        &evidence.manifest_hash,
        &policy.candidate.package.sha256,
        &size,
        current,
        guard.map_or("", |guard| &guard.root),
        guard.map_or("", |guard| &guard.archives),
        guard.map_or("", |guard| &guard.releases),
        guard.map_or("", |guard| &guard.archive),
        guard.map_or("", |guard| &guard.directory),
    ];
    CommandSpec::structured("timeout", values.map(CommandArgument::plain))
        .map_err(|_| RetentionError::Remote)
}

fn parse_guard(output: RemoteCommandOutput) -> Result<Guard, RetentionError> {
    if output.exit_status != 0 || output.stdout_truncated || output.stdout.len() > 2048 {
        return Err(RetentionError::UnsafePath);
    }
    let text = String::from_utf8(output.stdout).map_err(|_| RetentionError::Remote)?;
    let fields: Vec<_> = text.lines().collect();
    if fields.len() != 5
        || fields.iter().any(|field| {
            field.is_empty() || field.len() > 256 || field.chars().any(char::is_control)
        })
    {
        return Err(RetentionError::Remote);
    }
    Ok(Guard {
        root: fields[0].into(),
        archives: fields[1].into(),
        releases: fields[2].into(),
        archive: fields[3].into(),
        directory: fields[4].into(),
    })
}

async fn mutate(
    remote: &impl RetentionRemote,
    target: &LinuxSshTarget,
    policy: &RetentionPolicy,
    mut checked: Checked,
    cancellation: &CancellationToken,
) -> Result<CleanupReport, RetentionError> {
    let mut paths = CleanupPartial {
        version: policy.candidate.release.version.clone(),
        archive: CleanupPathState::Present,
        directory: if checked.guard.directory == "absent" {
            CleanupPathState::Absent
        } else {
            CleanupPathState::Present
        },
    };
    let mut attempted = false;
    for mode in ["directory", "archive"] {
        if mode == "directory" && paths.directory == CleanupPathState::Absent {
            continue;
        }
        if cancellation.is_cancelled() {
            if !attempted {
                return Err(RetentionError::Cancelled);
            }
            return Ok(partial_report(paths, checked.retained));
        }
        let command = match script_command(
            mode,
            target,
            policy,
            &checked.evidence,
            Some(&checked.guard),
        ) {
            Ok(command) => command,
            Err(error) if !attempted => return Err(error),
            Err(_) => return Ok(partial_report(paths, checked.retained)),
        };
        attempted = true;
        // From this point the server may have removed entries, even if transport fails.
        // Cancellation is honored between irreversible path operations, not by
        // abandoning an rm that may still run remotely. GNU timeout bounds it.
        let completion = CancellationToken::new();
        let result =
            tokio::time::timeout(COMMAND_TIMEOUT, remote.command(&command, &completion)).await;
        completion.cancel();
        if result.as_ref().is_ok_and(|output| {
            output.as_ref().is_ok_and(|output| {
                output.exit_status == 0 && !output.stdout_truncated && output.stdout == b"absent\n"
            })
        }) {
            if mode == "directory" {
                paths.directory = CleanupPathState::Absent;
                checked.guard.directory = "absent".into();
            } else {
                paths.archive = CleanupPathState::Absent;
            }
        } else {
            if mode == "directory" {
                paths.directory = CleanupPathState::Unknown;
            } else {
                paths.archive = CleanupPathState::Unknown;
            }
            observe_paths(remote, target, policy, &checked, &mut paths).await;
            return Ok(partial_report(paths, checked.retained));
        }
    }
    Ok(CleanupReport {
        removed: vec![paths.version],
        retained: checked.retained,
        ..CleanupReport::default()
    })
}

async fn observe_paths(
    remote: &impl RetentionRemote,
    target: &LinuxSshTarget,
    policy: &RetentionPolicy,
    checked: &Checked,
    paths: &mut CleanupPartial,
) {
    let Ok(command) = script_command(
        "observe",
        target,
        policy,
        &checked.evidence,
        Some(&checked.guard),
    ) else {
        return;
    };
    let token = CancellationToken::new();
    let output = tokio::time::timeout(OBSERVE_TIMEOUT, remote.command(&command, &token)).await;
    token.cancel();
    let Ok(Ok(output)) = output else {
        return;
    };
    if output.exit_status != 0 || output.stdout_truncated {
        return;
    }
    let state = |line: &[u8]| match line {
        b"present" => Some(CleanupPathState::Present),
        b"absent" => Some(CleanupPathState::Absent),
        _ => None,
    };
    let fields: Vec<_> = output.stdout.split(|byte| *byte == b'\n').collect();
    if fields.len() == 3
        && fields[2].is_empty()
        && let (Some(archive), Some(directory)) = (state(fields[0]), state(fields[1]))
    {
        paths.archive = archive;
        paths.directory = directory;
    }
}

fn partial_report(
    paths: CleanupPartial,
    retained: Vec<crate::domain::ReleaseVersion>,
) -> CleanupReport {
    let complete =
        paths.archive == CleanupPathState::Absent && paths.directory == CleanupPathState::Absent;
    CleanupReport {
        removed: if complete { vec![paths.version.clone()] } else { Vec::new() },
        retained,
        partial: if complete { Vec::new() } else { vec![paths] },
        warnings: vec!["Cleanup required additional verification; inspect the recorded per-path results before any retry.".into()],
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
    };

    use crate::{
        domain::{
            ComponentGeneration, ComponentName, ComponentRelease, DestinationKey,
            DestinationRevision, EnvironmentId, ProjectId, ReleaseManifest, ReleaseVersion,
        },
        drivers::{
            CredentialHandle, DriverDestinationInput, DriverKind, DriverTargetInput,
            EndpointFingerprint, ReleaseRef,
            inventory::{InventoryIssue, InventoryRelease},
        },
    };

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Failure {
        Before,
        After,
    }

    struct MockRemote {
        inventory: ReleaseInventory,
        calls: Mutex<Vec<String>>,
        paths: Mutex<(bool, bool)>,
        failure: Option<(&'static str, Failure)>,
        observe_unavailable: bool,
        cancel_after_directory: Option<CancellationToken>,
        unsafe_check: bool,
    }

    fn output(text: &str) -> RemoteCommandOutput {
        RemoteCommandOutput {
            exit_status: 0,
            stdout: text.as_bytes().to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[async_trait]
    impl RetentionRemote for MockRemote {
        async fn inventory(
            &self,
            _: &LinuxSshTarget,
            _: &DeploymentMarker,
            _: &CancellationToken,
        ) -> Result<ReleaseInventory, RetentionError> {
            Ok(self.inventory.clone())
        }
        async fn evidence(
            &self,
            _: &LinuxSshTarget,
            marker: &DeploymentMarker,
            candidate: &CleanupCandidate,
            _: &CancellationToken,
        ) -> Result<Evidence, RetentionError> {
            Ok(Evidence {
                marker_hash: byte_digest(&marker.encode().unwrap()),
                manifest_hash: byte_digest(
                    &serde_json::to_vec(&candidate.package.manifest).unwrap(),
                ),
            })
        }
        async fn command(
            &self,
            command: &CommandSpec,
            cancellation: &CancellationToken,
        ) -> Result<RemoteCommandOutput, RetentionError> {
            let args: Vec<_> = command
                .args
                .iter()
                .map(CommandArgument::expose_for_execution)
                .collect();
            assert_eq!(command.program, "timeout");
            assert_eq!(
                &args[..9],
                &[
                    "--signal=TERM",
                    "--kill-after=2s",
                    "25s",
                    "bash",
                    "-o",
                    "pipefail",
                    "-c",
                    SCRIPT,
                    "shipforge-cleanup"
                ]
            );
            let mode = args[9];
            self.calls.lock().unwrap().push(mode.into());
            let mut paths = self.paths.lock().unwrap();
            if mode == "inspect" {
                if self.unsafe_check {
                    return Err(RetentionError::UnsafePath);
                }
                return Ok(output(if paths.1 {
                    "1:2\n1:3\n1:4\narchive-stamp\ndirectory-stamp\n"
                } else {
                    "1:2\n1:3\n1:4\narchive-stamp\nabsent\n"
                }));
            }
            if mode == "observe" {
                if self.observe_unavailable {
                    return Err(RetentionError::Remote);
                }
                return Ok(output(&format!(
                    "{}\n{}\n",
                    if paths.0 { "present" } else { "absent" },
                    if paths.1 { "present" } else { "absent" }
                )));
            }
            assert!(
                !cancellation.is_cancelled(),
                "started deletion has an independent completion token"
            );
            if self.failure == Some((mode, Failure::Before)) {
                return Err(RetentionError::Remote);
            }
            match mode {
                "directory" => {
                    paths.1 = false;
                    if let Some(token) = &self.cancel_after_directory {
                        token.cancel();
                        assert!(!cancellation.is_cancelled());
                    }
                }
                "archive" => {
                    assert!(!paths.1);
                    assert_eq!(args[21], "absent");
                    paths.0 = false;
                }
                _ => panic!("only one exact candidate's two paths may be deleted"),
            }
            if self.failure == Some((mode, Failure::After)) {
                return Err(RetentionError::Remote);
            }
            Ok(output("absent\n"))
        }
    }

    fn fixture() -> (
        LinuxSshTarget,
        ComponentExecutionContext,
        RetentionPolicy,
        MockRemote,
    ) {
        let target = LinuxSshTarget::validate(&DriverTargetInput {
            value: serde_json::json!({"root":"/srv/app"}),
        })
        .unwrap();
        let destination = super::super::LinuxSshDestination::validate(&DriverDestinationInput { value: serde_json::json!({"host":"127.0.0.1","user":"deploy","port":22,"hostKey":"SHA256:fixture"}) }).unwrap();
        let context = ComponentExecutionContext {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
            credential: CredentialHandle::new(),
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            destination_settings: Arc::new(destination),
            target: Arc::new(target.clone()),
            cancellation: CancellationToken::new(),
        };
        let release = ReleaseRef {
            driver: DriverKind::parse("linux-ssh").unwrap(),
            project_id: context.project_id.clone(),
            environment_id: context.environment_id.clone(),
            component: context.component.clone(),
            generation: context.generation,
            version: ReleaseVersion::parse("z-old").unwrap(),
            destination: context.destination.clone(),
            destination_revision: context.destination_revision,
            endpoint_fingerprint: context.endpoint_fingerprint.clone(),
            effective_capabilities: crate::domain::DriverCapabilities::new([]),
        };
        let package = InventoryRelease {
            manifest: ReleaseManifest::new(
                &ComponentRelease {
                    project_id: release.project_id.clone(),
                    environment_id: release.environment_id.clone(),
                    component: release.component.clone(),
                    generation: release.generation,
                    version: release.version.clone(),
                    destination: release.destination.clone(),
                    destination_revision: release.destination_revision,
                },
                10,
                None,
            ),
            sha256: "b".repeat(64),
            size: 512,
            extracted: true,
        };
        let mut newer = package.clone();
        newer.manifest.version = ReleaseVersion::parse("a-new").unwrap();
        newer.manifest.created_at_unix = 20;
        let current = Some(newer.manifest.version.clone());
        let policy = RetentionPolicy {
            protected_versions: BTreeSet::new(),
            retain_count: 1,
            candidate: CleanupCandidate {
                release,
                package: package.clone(),
                expected_current: current.clone(),
            },
        };
        let remote = MockRemote {
            inventory: ReleaseInventory {
                releases: vec![package, newer],
                issues: Vec::new(),
                current: Ok(current),
                notices: Vec::new(),
            },
            calls: Mutex::new(Vec::new()),
            paths: Mutex::new((true, true)),
            failure: None,
            observe_unavailable: false,
            cancel_after_directory: None,
            unsafe_check: false,
        };
        (target, context, policy, remote)
    }

    #[tokio::test]
    async fn deletes_only_exact_old_candidate_directory_then_archive() {
        let (target, context, policy, remote) = fixture();
        let report = cleanup_with_remote(&remote, &target, &context, &policy)
            .await
            .unwrap();
        assert_eq!(report.removed, vec![policy.candidate.release.version]);
        assert_eq!(
            report.retained,
            vec![ReleaseVersion::parse("a-new").unwrap()]
        );
        assert!(report.partial.is_empty() && report.warnings.is_empty());
        assert_eq!(
            *remote.calls.lock().unwrap(),
            ["inspect", "directory", "archive"]
        );
    }

    #[tokio::test]
    async fn protected_current_and_newest_count_refuse_before_any_path_command() {
        for case in 0..4 {
            let (target, context, mut policy, remote) = fixture();
            match case {
                0 => {
                    policy
                        .protected_versions
                        .insert(policy.candidate.release.version.clone());
                }
                1 => {
                    policy.candidate.expected_current =
                        Some(policy.candidate.release.version.clone());
                }
                2 => policy.retain_count = 2,
                _ => policy.retain_count = 0,
            }
            assert!(
                cleanup_with_remote(&remote, &target, &context, &policy)
                    .await
                    .is_err()
            );
            assert!(remote.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn identity_manifest_digest_current_and_unknown_inventory_never_authorize_deletion() {
        for case in 0..6 {
            let (target, context, mut policy, mut remote) = fixture();
            match case {
                0 => policy.candidate.release.project_id = ProjectId::new(),
                1 => {
                    policy.candidate.package.manifest.generation =
                        ComponentGeneration::INITIAL.checked_next().unwrap();
                }
                2 => policy.candidate.package.sha256 = "c".repeat(64),
                3 => remote.inventory.current = Err("unknown".into()),
                4 => remote.inventory.current = Ok(None),
                _ => remote.inventory.issues.push(InventoryIssue {
                    version: None,
                    message: "invalid entry".into(),
                }),
            }
            assert!(
                cleanup_with_remote(&remote, &target, &context, &policy)
                    .await
                    .is_err()
            );
            assert!(remote.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn unsafe_marker_or_path_check_stops_before_mutation() {
        let (target, context, policy, mut remote) = fixture();
        remote.unsafe_check = true;
        assert_eq!(
            cleanup_with_remote(&remote, &target, &context, &policy)
                .await
                .unwrap_err(),
            RetentionError::UnsafePath
        );
        assert_eq!(*remote.paths.lock().unwrap(), (true, true));
        assert_eq!(*remote.calls.lock().unwrap(), ["inspect"]);
    }

    #[tokio::test]
    async fn archive_only_candidate_is_removed_without_recursive_directory_action() {
        let (target, context, mut policy, mut remote) = fixture();
        policy.candidate.package.extracted = false;
        remote.inventory.releases[0].extracted = false;
        remote.inventory.issues.push(InventoryIssue {
            version: Some(policy.candidate.release.version.clone()),
            message: "archive only".into(),
        });
        *remote.paths.lock().unwrap() = (true, false);
        let report = cleanup_with_remote(&remote, &target, &context, &policy)
            .await
            .unwrap();
        assert_eq!(report.removed, vec![policy.candidate.release.version]);
        assert_eq!(*remote.calls.lock().unwrap(), ["inspect", "archive"]);
    }

    #[tokio::test]
    async fn cancellation_before_delete_has_no_effect_and_after_directory_preserves_absence() {
        let (target, context, policy, mut remote) = fixture();
        context.cancellation.cancel();
        assert_eq!(
            cleanup_with_remote(&remote, &target, &context, &policy)
                .await
                .unwrap_err(),
            RetentionError::Cancelled
        );
        assert!(remote.calls.lock().unwrap().is_empty());
        let mut context = context;
        context.cancellation = CancellationToken::new();
        remote.cancel_after_directory = Some(context.cancellation.clone());
        let report = cleanup_with_remote(&remote, &target, &context, &policy)
            .await
            .unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.partial[0].directory, CleanupPathState::Absent);
        assert_eq!(report.partial[0].archive, CleanupPathState::Present);
        assert_eq!(*remote.calls.lock().unwrap(), ["inspect", "directory"]);
    }

    #[tokio::test]
    async fn interrupted_actions_preserve_each_independently_observed_path_state() {
        for (mode, failure, directory, archive) in [
            (
                "directory",
                Failure::Before,
                CleanupPathState::Present,
                CleanupPathState::Present,
            ),
            (
                "directory",
                Failure::After,
                CleanupPathState::Absent,
                CleanupPathState::Present,
            ),
            (
                "archive",
                Failure::Before,
                CleanupPathState::Absent,
                CleanupPathState::Present,
            ),
        ] {
            let (target, context, policy, mut remote) = fixture();
            remote.failure = Some((mode, failure));
            let report = cleanup_with_remote(&remote, &target, &context, &policy)
                .await
                .unwrap();
            assert!(report.removed.is_empty() && !report.warnings.is_empty());
            assert_eq!(report.partial[0].directory, directory);
            assert_eq!(report.partial[0].archive, archive);
        }
    }

    #[tokio::test]
    async fn lost_archive_ack_is_complete_only_after_both_paths_are_confirmed_absent() {
        let (target, context, policy, mut remote) = fixture();
        remote.failure = Some(("archive", Failure::After));
        let report = cleanup_with_remote(&remote, &target, &context, &policy)
            .await
            .unwrap();
        assert_eq!(report.removed, vec![policy.candidate.release.version]);
        assert!(report.partial.is_empty() && !report.warnings.is_empty());
    }

    #[tokio::test]
    async fn disconnected_observation_keeps_known_directory_absence_and_unknown_archive() {
        let (target, context, policy, mut remote) = fixture();
        remote.failure = Some(("archive", Failure::After));
        remote.observe_unavailable = true;
        let report = cleanup_with_remote(&remote, &target, &context, &policy)
            .await
            .unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.partial[0].directory, CleanupPathState::Absent);
        assert_eq!(report.partial[0].archive, CleanupPathState::Unknown);
    }

    #[test]
    fn destructive_script_pins_paths_checks_mounts_and_has_only_two_exact_unlinks() {
        assert!(SCRIPT.contains("exec 3< \"$root\""));
        assert!(SCRIPT.contains("/proc/self/mountinfo"));
        assert!(SCRIPT.contains("--one-file-system --preserve-root=all -- \"$directory\""));
        assert!(SCRIPT.contains("rm -- \"$archive\""));
        assert_eq!(
            SCRIPT
                .lines()
                .filter(|line| line.trim_start().starts_with("rm "))
                .count(),
            2
        );
        assert!(!SCRIPT.contains("rm --recursive -- \"$root\""));
    }

    // Native GNU/bash proof uses only an owned local TempDir, no SSH, services,
    // privilege changes or mounts. The production script is executed unchanged.
    #[cfg(target_os = "linux")]
    mod native {
        use std::{
            fmt::Write as _,
            fs,
            os::unix::fs::{MetadataExt, PermissionsExt, symlink},
            path::{Path, PathBuf},
        };

        use super::*;

        struct NativeFixture {
            _directory: tempfile::TempDir,
            root: PathBuf,
            target: LinuxSshTarget,
            policy: RetentionPolicy,
            evidence: Evidence,
        }

        impl NativeFixture {
            fn new() -> Self {
                let (_, context, mut policy, _) = fixture();
                let directory = tempfile::tempdir().unwrap();
                let root = directory.path().join("root with 'quoted space");
                for path in [
                    "archives",
                    "releases/z-old",
                    "releases/a-new",
                    "metadata",
                    "temporary",
                ] {
                    fs::create_dir_all(root.join(path)).unwrap();
                }
                let marker = DeploymentMarker::for_context(&context).encode().unwrap();
                let manifest = serde_json::to_vec(&policy.candidate.package.manifest).unwrap();
                let archive = b"owned candidate archive bytes";
                policy.candidate.package.sha256 = byte_digest(archive);
                policy.candidate.package.size = u64::try_from(archive.len()).unwrap();
                fs::write(root.join(".shipforge-project.json"), &marker).unwrap();
                fs::write(root.join("releases/z-old/manifest.json"), &manifest).unwrap();
                fs::write(root.join("releases/z-old/payload"), "old").unwrap();
                fs::write(root.join("archives/z-old.tar.gz"), archive).unwrap();
                fs::write(root.join("releases/a-new/payload"), "current").unwrap();
                fs::write(root.join("archives/a-new.tar.gz"), "retained archive").unwrap();
                fs::write(root.join("metadata/sentinel"), "metadata").unwrap();
                fs::write(root.join("temporary/sentinel"), "temporary").unwrap();
                symlink("releases/a-new", root.join("current")).unwrap();
                let target = LinuxSshTarget::validate(&DriverTargetInput {
                    value: serde_json::json!({"root": root.to_str().unwrap()}),
                })
                .unwrap();
                Self {
                    _directory: directory,
                    root,
                    target,
                    policy,
                    evidence: Evidence {
                        marker_hash: byte_digest(&marker),
                        manifest_hash: byte_digest(&manifest),
                    },
                }
            }

            async fn command(&self, mode: &str, guard: Option<&Guard>) -> RemoteCommandOutput {
                self.command_with_path(mode, guard, None).await
            }

            async fn command_with_path(
                &self,
                mode: &str,
                guard: Option<&Guard>,
                shim_directory: Option<&Path>,
            ) -> RemoteCommandOutput {
                let command =
                    script_command(mode, &self.target, &self.policy, &self.evidence, guard)
                        .unwrap();
                let mut process = tokio::process::Command::new(&command.program);
                process.args(
                    command
                        .args
                        .iter()
                        .map(CommandArgument::expose_for_execution),
                );
                if let Some(shim_directory) = shim_directory {
                    let mut paths = vec![shim_directory.to_owned()];
                    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
                    process.env("PATH", std::env::join_paths(paths).unwrap());
                }
                let result =
                    tokio::time::timeout(COMMAND_TIMEOUT, process.kill_on_drop(true).output())
                        .await
                        .unwrap()
                        .unwrap();
                RemoteCommandOutput {
                    exit_status: u32::try_from(result.status.code().unwrap()).unwrap(),
                    stdout: result.stdout,
                    stderr: result.stderr,
                    stdout_truncated: false,
                    stderr_truncated: false,
                }
            }

            fn assert_preserved(&self) {
                assert_eq!(
                    fs::read_link(self.root.join("current")).unwrap(),
                    PathBuf::from("releases/a-new")
                );
                for (path, bytes) in [
                    ("releases/a-new/payload", "current"),
                    ("archives/a-new.tar.gz", "retained archive"),
                    ("metadata/sentinel", "metadata"),
                    ("temporary/sentinel", "temporary"),
                ] {
                    assert_eq!(fs::read(self.root.join(path)).unwrap(), bytes.as_bytes());
                }
            }
        }

        #[tokio::test]
        async fn gnu_script_deletes_only_the_two_exact_paths_with_fd_pinning() {
            let fixture = NativeFixture::new();
            let mut guard = parse_guard(fixture.command("inspect", None).await).unwrap();
            let removed_directory = fixture.command("directory", Some(&guard)).await;
            assert_eq!(removed_directory.exit_status, 0, "{removed_directory:?}");
            assert!(!fixture.root.join("releases/z-old").exists());
            assert!(fixture.root.join("archives/z-old.tar.gz").exists());
            guard.directory = "absent".into();
            let removed_archive = fixture.command("archive", Some(&guard)).await;
            assert_eq!(removed_archive.exit_status, 0, "{removed_archive:?}");
            assert!(!fixture.root.join("archives/z-old.tar.gz").exists());
            fixture.assert_preserved();
        }

        #[tokio::test]
        async fn gnu_script_rejects_metadata_changed_to_newline_tail_or_embedded_nul() {
            for path in [".shipforge-project.json", "releases/z-old/manifest.json"] {
                for oversized in [false, true] {
                    let fixture = NativeFixture::new();
                    let guard = parse_guard(fixture.command("inspect", None).await).unwrap();
                    let path = fixture.root.join(path);
                    let mut bytes = fs::read(&path).unwrap();
                    if oversized {
                        bytes.extend(std::iter::repeat_n(b'\n', 8193));
                        bytes.extend_from_slice(b"unseen malicious tail");
                    } else {
                        // Bash command substitution used to silently strip this byte.
                        bytes.push(0);
                    }
                    fs::write(path, bytes).unwrap();
                    let refused = fixture.command("directory", Some(&guard)).await;
                    assert_eq!(refused.exit_status, 42, "{refused:?}");
                    assert!(fixture.root.join("releases/z-old/payload").exists());
                    assert!(fixture.root.join("archives/z-old.tar.gz").exists());
                    fixture.assert_preserved();
                }
            }
        }

        #[tokio::test]
        async fn gnu_script_rejects_payload_links_without_following_them() {
            let fixture = NativeFixture::new();
            symlink("../../metadata", fixture.root.join("releases/z-old/link")).unwrap();
            let guard = parse_guard(fixture.command("inspect", None).await).unwrap();
            let refused = fixture.command("directory", Some(&guard)).await;
            assert_eq!(refused.exit_status, 42, "{refused:?}");
            assert!(fixture.root.join("releases/z-old/payload").exists());
            fixture.assert_preserved();
        }

        #[tokio::test]
        async fn gnu_script_rejects_65537_byte_tree_ending_in_lf_even_when_find_exits_zero() {
            let fixture = NativeFixture::new();
            let guard = parse_guard(fixture.command("inspect", None).await).unwrap();
            let shims = fixture.root.parent().unwrap().join("bounded-find-fixture");
            fs::create_dir(&shims).unwrap();
            let dev = fs::metadata(&fixture.root).unwrap().dev();
            let prefix = format!("{dev}:f:");
            // 2047 * 32 + 33 = 65537 bytes; all first rows are otherwise valid,
            // with <4096 entries and zero total size. The unseen suffix is unsafe.
            let line = format!("{prefix}{}\n", "0".repeat(31 - prefix.len()));
            assert_eq!(line.len(), 32);
            let last = format!("{prefix}{}\n", "0".repeat(32 - prefix.len()));
            let mut listing = line.repeat(2047);
            listing.push_str(&last);
            assert_eq!(listing.len(), 65537);
            assert!(listing.ends_with('\n'));
            writeln!(listing, "{dev}:l:0").unwrap();
            fs::write(shims.join("tree-records"), listing).unwrap();
            // Only find is substituted; the unchanged production shell must reject
            // a clipped prefix even if find reports success (no SIGPIPE assumption).
            fs::write(
                shims.join("find"),
                "#!/bin/sh\ncat -- \"$(dirname -- \"$0\")/tree-records\"\nexit 0\n",
            )
            .unwrap();
            fs::set_permissions(shims.join("find"), fs::Permissions::from_mode(0o755)).unwrap();
            let refused = fixture
                .command_with_path("directory", Some(&guard), Some(&shims))
                .await;
            assert_eq!(refused.exit_status, 42, "{refused:?}");
            assert!(fixture.root.join("releases/z-old/payload").exists());
            fixture.assert_preserved();
        }
    }
}
