use std::{collections::BTreeSet, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    domain::ComponentRelease,
    drivers::audit::{RemoteAuditHistory, RemoteAuditPhase, RemoteAuditRecord},
    telemetry::{CommandArgument, CommandSpec},
};

use super::{
    AuthenticatedSession, DeploymentMarker, LinuxSshTarget, RemoteCommandOutput, RemotePath,
};

const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FILE_BYTES: usize = 48 * 1024;
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_RECORDS: usize = 128;
const MAX_NOTICES: usize = 32;

// All variable data is passed as separately quoted arguments. The read-only
// descriptor pins the checked inode: writing /proc/self/fd/3 cannot follow a
// subsequent replacement of the final pathname. No write precedes fd checks.
const AUDIT_SCRIPT: &str = r#"set -eu
export LC_ALL=C
root=$1
file=$2
marker=$3
mode=$4
payload=${5-}
case "$file" in releases.jsonl|deployments.jsonl) ;; *) exit 42;; esac
case "$mode" in read|append) ;; *) exit 42;; esac
p=$root
while [ "$p" != / ]; do
  [ ! -L "$p" ] && [ -d "$p" ] || exit 42
  p=${p%/*}
  [ -n "$p" ] || p=/
done
mark=$root/.shipforge-project.json
[ ! -L "$mark" ] && [ -f "$mark" ] || exit 43
[ "$(stat -c %h -- "$mark")" = 1 ] || exit 43
[ "$(head -c 4097 -- "$mark")" = "$marker" ] || exit 43
meta=$root/metadata
[ ! -L "$meta" ] || exit 42
if [ ! -e "$meta" ]; then
  [ "$mode" = append ] || exit 44
  mkdir -m 700 -- "$meta"
fi
[ ! -L "$meta" ] && [ -d "$meta" ] || exit 42
directory_id=$(stat -c '%d:%i' -- "$meta")
cd -- "$meta"
[ "$(pwd -P)" = "$meta" ] || exit 42
[ "$(stat -c '%d:%i' -- .)" = "$directory_id" ] || exit 42
[ ! -L "$file" ] || exit 42
if [ ! -e "$file" ]; then
  [ "$mode" = append ] || exit 44
  (set -C; umask 077; : > "$file")
fi
[ ! -L "$file" ] && [ -f "$file" ] || exit 42
[ "$(stat -c %h -- "$file")" = 1 ] || exit 42
file_id=$(stat -c '%d:%i' -- "$file")
exec 3< "$file"
[ -f /proc/self/fd/3 ] || exit 42
[ "$(stat -L -c %h -- /proc/self/fd/3)" = 1 ] || exit 42
[ "$(stat -L -c '%d:%i' -- /proc/self/fd/3)" = "$file_id" ] || exit 42
[ ! -L "$file" ] && [ "$(stat -c '%d:%i' -- "$file")" = "$file_id" ] || exit 42
if [ "$mode" = read ]; then
  head -c 49153 <&3
else
  printf '\n%s\n' "$payload" | dd of=/proc/self/fd/3 oflag=append conv=notrunc status=none
fi
[ ! -L "$file" ] && [ -f "$file" ] || exit 42
[ "$(stat -c '%d:%i:%h' -- "$file")" = "$file_id:1" ] || exit 42
[ ! -L "$meta" ] && [ "$(stat -c '%d:%i' -- "$meta")" = "$directory_id" ] || exit 42
p=$root
while [ "$p" != / ]; do
  [ ! -L "$p" ] && [ -d "$p" ] || exit 42
  p=${p%/*}
  [ -n "$p" ] || p=/
done
"#;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuditError {
    #[error("remote audit operation was cancelled")]
    Cancelled,
    #[error("remote audit operation exceeded its total deadline")]
    Timeout,
    #[error("remote audit requires the matching existing Deployment Marker")]
    Marker,
    #[error("remote audit path is linked, shared, or not a regular file/directory")]
    UnsafePath,
    #[error("remote audit record is invalid, oversized, or outside the Component scope")]
    InvalidRecord,
    #[error("remote audit operation failed; evidence may be incomplete")]
    Remote,
}

impl AuthenticatedSession {
    /// Appends one bounded audit line without truncation, repair, or a remote lock.
    ///
    /// # Errors
    /// Rejects unsafe paths, a missing/mismatched marker, invalid data, and I/O failure.
    pub async fn append_audit(
        &self,
        target: &LinuxSshTarget,
        expected_marker: &DeploymentMarker,
        record: &RemoteAuditRecord,
        cancellation: &CancellationToken,
    ) -> Result<(), AuditError> {
        bounded(
            cancellation,
            append_with_remote(self, target, expected_marker, record, cancellation),
        )
        .await
    }

    /// Reads a capped audit prefix without creating directories or changing files.
    ///
    /// # Errors
    /// Rejects unsafe paths, a missing/mismatched marker, and interrupted/failed reads.
    pub async fn read_audit(
        &self,
        target: &LinuxSshTarget,
        expected_marker: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<RemoteAuditHistory, AuditError> {
        bounded(
            cancellation,
            read_with_remote(self, target, expected_marker, cancellation),
        )
        .await
    }
}

async fn bounded<T>(
    cancellation: &CancellationToken,
    operation: impl std::future::Future<Output = Result<T, AuditError>>,
) -> Result<T, AuditError> {
    bounded_for(cancellation, AUDIT_TIMEOUT, operation).await
}

async fn bounded_for<T>(
    cancellation: &CancellationToken,
    deadline: Duration,
    operation: impl std::future::Future<Output = Result<T, AuditError>>,
) -> Result<T, AuditError> {
    if cancellation.is_cancelled() {
        return Err(AuditError::Cancelled);
    }
    tokio::select! {
        () = cancellation.cancelled() => Err(AuditError::Cancelled),
        result = tokio::time::timeout(deadline, operation) => result.map_err(|_| AuditError::Timeout)?,
    }
}

#[async_trait]
trait AuditRemote: Sync {
    async fn marker(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, AuditError>;
    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, AuditError>;
}

#[async_trait]
impl AuditRemote for AuthenticatedSession {
    async fn marker(
        &self,
        target: &LinuxSshTarget,
        expected: &DeploymentMarker,
        cancellation: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, AuditError> {
        if !self
            .check_deployment_marker(target, expected, cancellation)
            .await
            .map_err(|_| AuditError::Marker)?
        {
            return Ok(None);
        }
        let path = format!("{}/.shipforge-project.json", target.root);
        let command = CommandSpec::structured(
            "head",
            ["-c", "4097", "--", &path].map(CommandArgument::plain),
        )
        .map_err(|_| AuditError::Marker)?;
        let output = self
            .execute(&command, AUDIT_TIMEOUT, cancellation)
            .await
            .map_err(|_| AuditError::Marker)?;
        if output.exit_status != 0 || output.stdout_truncated {
            return Err(AuditError::Marker);
        }
        expected
            .validate(&output.stdout)
            .map_err(|_| AuditError::Marker)?;
        Ok(Some(output.stdout))
    }

    async fn command(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<RemoteCommandOutput, AuditError> {
        self.execute(command, AUDIT_TIMEOUT, cancellation)
            .await
            .map_err(|_| AuditError::Remote)
    }
}

async fn require_marker(
    remote: &impl AuditRemote,
    target: &LinuxSshTarget,
    marker: &DeploymentMarker,
    cancellation: &CancellationToken,
) -> Result<String, AuditError> {
    RemotePath::parse(target.root.clone()).map_err(|_| AuditError::UnsafePath)?;
    let bytes = remote
        .marker(target, marker, cancellation)
        .await?
        .ok_or(AuditError::Marker)?;
    marker.validate(&bytes).map_err(|_| AuditError::Marker)?;
    let actual = String::from_utf8(bytes).map_err(|_| AuditError::Marker)?;
    Ok(actual.trim_end_matches('\n').into())
}

fn same_scope(record: &RemoteAuditRecord, expected: &DeploymentMarker) -> bool {
    let release = &record.release;
    let marker = DeploymentMarker::for_release(&ComponentRelease {
        project_id: release.project_id.clone(),
        environment_id: release.environment_id.clone(),
        component: release.component.clone(),
        generation: release.generation,
        version: release.version.clone(),
        destination: release.destination.clone(),
        destination_revision: release.destination_revision,
    });
    &marker == expected && release.driver.as_str() == crate::drivers::DriverKind::LINUX_SSH
}

fn audit_command(
    target: &LinuxSshTarget,
    marker: &str,
    file: &str,
    payload: Option<&str>,
) -> Result<CommandSpec, AuditError> {
    CommandSpec::structured(
        "timeout",
        [
            "--kill-after=2",
            "25",
            "sh",
            "-c",
            AUDIT_SCRIPT,
            "shipforge-audit",
            target.root.as_str(),
            file,
            marker,
            if payload.is_some() { "append" } else { "read" },
            payload.unwrap_or(""),
        ]
        .map(CommandArgument::plain),
    )
    .map_err(|_| AuditError::Remote)
}

fn audit_output(output: RemoteCommandOutput) -> Result<Option<Vec<u8>>, AuditError> {
    if output.stdout_truncated || output.stderr_truncated {
        return Err(AuditError::Remote);
    }
    match output.exit_status {
        0 => Ok(Some(output.stdout)),
        42 => Err(AuditError::UnsafePath),
        43 => Err(AuditError::Marker),
        44 => Ok(None),
        124 | 137 => Err(AuditError::Timeout),
        _ => Err(AuditError::Remote),
    }
}

async fn append_with_remote(
    remote: &impl AuditRemote,
    target: &LinuxSshTarget,
    marker: &DeploymentMarker,
    record: &RemoteAuditRecord,
    cancellation: &CancellationToken,
) -> Result<(), AuditError> {
    if !record.is_valid() || !same_scope(record, marker) {
        return Err(AuditError::InvalidRecord);
    }
    let payload = serde_json::to_string(record).map_err(|_| AuditError::InvalidRecord)?;
    if payload.len() > MAX_LINE_BYTES || payload.contains(['\n', '\r']) {
        return Err(AuditError::InvalidRecord);
    }
    let actual_marker = require_marker(remote, target, marker, cancellation).await?;
    let file = if record.phase == RemoteAuditPhase::Prepare {
        "releases.jsonl"
    } else {
        "deployments.jsonl"
    };
    let command = audit_command(target, &actual_marker, file, Some(&payload))?;
    match audit_output(remote.command(&command, cancellation).await?)? {
        Some(output) if output.is_empty() => Ok(()),
        _ => Err(AuditError::Remote),
    }
}

async fn read_with_remote(
    remote: &impl AuditRemote,
    target: &LinuxSshTarget,
    marker: &DeploymentMarker,
    cancellation: &CancellationToken,
) -> Result<RemoteAuditHistory, AuditError> {
    let actual_marker = require_marker(remote, target, marker, cancellation).await?;
    let mut history = RemoteAuditHistory::default();
    let mut conflicts = BTreeSet::new();
    for file in ["releases.jsonl", "deployments.jsonl"] {
        let command = audit_command(target, &actual_marker, file, None)?;
        if let Some(bytes) = audit_output(remote.command(&command, cancellation).await?)? {
            parse_lines(&bytes, file, marker, &mut history, &mut conflicts);
        } else {
            notice(
                &mut history,
                "An audit file is absent; earlier activity is unknown.",
            );
        }
    }
    Ok(history)
}

fn notice(history: &mut RemoteAuditHistory, message: &str) {
    history.incomplete = true;
    if history.notices.len() < MAX_NOTICES {
        history.notices.push(message.into());
    }
}

fn parse_lines(
    bytes: &[u8],
    file: &str,
    marker: &DeploymentMarker,
    history: &mut RemoteAuditHistory,
    conflicts: &mut BTreeSet<uuid::Uuid>,
) {
    let limit = bytes.len().min(MAX_FILE_BYTES);
    if bytes.len() > MAX_FILE_BYTES {
        notice(
            history,
            "Audit byte limit reached; only a bounded prefix is available.",
        );
    }
    for line in bytes[..limit].split_inclusive(|byte| *byte == b'\n') {
        if line.last() != Some(&b'\n') {
            notice(history, "An incomplete trailing audit line was ignored.");
            continue;
        }
        let line = &line[..line.len() - 1];
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES || line.contains(&b'\r') {
            notice(history, "An oversized audit line was ignored.");
            continue;
        }
        let Ok(record) = serde_json::from_slice::<RemoteAuditRecord>(line) else {
            notice(
                history,
                "A malformed or unsupported audit line was ignored.",
            );
            continue;
        };
        if !record.is_valid()
            || !same_scope(&record, marker)
            || (file == "releases.jsonl") != (record.phase == RemoteAuditPhase::Prepare)
        {
            notice(
                history,
                "An invalid or out-of-scope audit line was ignored.",
            );
            continue;
        }
        if conflicts.contains(&record.event_id) {
            continue;
        }
        if let Some(index) = history
            .records
            .iter()
            .position(|prior| prior.event_id == record.event_id)
        {
            if history.records[index] != record {
                history.records.remove(index);
                conflicts.insert(record.event_id);
                notice(history, "Conflicting duplicate audit events were excluded.");
            }
        } else if history.records.len() < MAX_RECORDS {
            history.records.push(record);
        } else {
            notice(
                history,
                "Audit record limit reached; additional records were omitted.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use crate::{
        domain::{
            ComponentGeneration, ComponentName, DeploymentId, DestinationKey, DestinationRevision,
            DriverCapabilities, EnvironmentId, ProjectId, ReleaseManifest, ReleaseVersion,
        },
        drivers::{
            DriverKind, DriverTargetInput, EndpointFingerprint, ReleaseRef,
            audit::{RemoteAuditObserved, RemoteAuditOutcome, RemoteAuditPackage},
        },
    };

    use super::*;

    struct FakeRemote {
        present: bool,
        marker_bytes: Option<Vec<u8>>,
        commands: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<VecDeque<RemoteCommandOutput>>,
    }

    impl FakeRemote {
        fn new(outputs: impl IntoIterator<Item = RemoteCommandOutput>) -> Self {
            Self {
                present: true,
                marker_bytes: None,
                commands: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl AuditRemote for FakeRemote {
        async fn marker(
            &self,
            _: &LinuxSshTarget,
            expected: &DeploymentMarker,
            _: &CancellationToken,
        ) -> Result<Option<Vec<u8>>, AuditError> {
            Ok(self.present.then(|| {
                self.marker_bytes
                    .clone()
                    .unwrap_or_else(|| expected.encode().unwrap())
            }))
        }

        async fn command(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<RemoteCommandOutput, AuditError> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected command"))
        }
    }

    fn output(exit_status: u32, stdout: Vec<u8>) -> RemoteCommandOutput {
        RemoteCommandOutput {
            exit_status,
            stdout,
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn fixture() -> (LinuxSshTarget, DeploymentMarker, RemoteAuditRecord) {
        let release = ComponentRelease {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse("v2").unwrap(),
            destination: DestinationKey::new(),
            destination_revision: DestinationRevision::INITIAL,
        };
        let marker = DeploymentMarker::for_release(&release);
        let reference = ReleaseRef {
            driver: DriverKind::linux_ssh(),
            project_id: release.project_id.clone(),
            environment_id: release.environment_id.clone(),
            component: release.component.clone(),
            generation: release.generation,
            version: release.version.clone(),
            destination: release.destination.clone(),
            destination_revision: release.destination_revision,
            endpoint_fingerprint: EndpointFingerprint::parse("a".repeat(64)).unwrap(),
            effective_capabilities: DriverCapabilities::default(),
        };
        let record = RemoteAuditRecord {
            schema_version: 1,
            event_id: uuid::Uuid::now_v7(),
            deployment: DeploymentId::new(),
            recorded_at_ms: 42,
            release: reference,
            phase: RemoteAuditPhase::Prepare,
            outcome: RemoteAuditOutcome::Succeeded,
            expected_current: Some(ReleaseVersion::parse("v1").unwrap()),
            target: Some(release.version.clone()),
            observed: RemoteAuditObserved::Unknown,
            healthy: None,
            package: Some(RemoteAuditPackage {
                manifest: ReleaseManifest::new(&release, 1, Some("abcdef123".into())),
                sha256: "b".repeat(64),
                size: 123,
            }),
        };
        let target = LinuxSshTarget::validate(&DriverTargetInput {
            value: serde_json::json!({"root":"/srv/project/api"}),
        })
        .unwrap();
        (target, marker, record)
    }

    fn line(record: &RemoteAuditRecord) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(record).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[tokio::test]
    async fn append_requires_existing_matching_marker_before_any_command() {
        let (target, marker, record) = fixture();
        let mut remote = FakeRemote::new([]);
        remote.present = false;
        assert_eq!(
            append_with_remote(
                &remote,
                &target,
                &marker,
                &record,
                &CancellationToken::new()
            )
            .await,
            Err(AuditError::Marker)
        );
        assert!(remote.commands.lock().unwrap().is_empty());
        let mut foreign = record;
        foreign.release.project_id = ProjectId::new();
        assert_eq!(
            append_with_remote(
                &remote,
                &target,
                &marker,
                &foreign,
                &CancellationToken::new()
            )
            .await,
            Err(AuditError::InvalidRecord)
        );
        assert!(remote.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_passes_data_as_arguments_and_only_accepts_empty_success() {
        let (mut target, marker, record) = fixture();
        target.root = "/srv/project with 'quote;$(touch unexpected)/api".into();
        let remote = FakeRemote::new([output(0, Vec::new())]);
        append_with_remote(
            &remote,
            &target,
            &marker,
            &record,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let commands = remote.commands.lock().unwrap();
        let command = &commands[0];
        assert_eq!(command.program, "timeout");
        let args = command
            .args
            .iter()
            .map(CommandArgument::expose_for_execution)
            .collect::<Vec<_>>();
        assert_eq!(&args[..3], &["--kill-after=2", "25", "sh"]);
        assert_eq!(args[4], AUDIT_SCRIPT);
        assert_eq!(args[6], target.root);
        assert_eq!(args[7], "releases.jsonl");
        assert_eq!(args[9], "append");
        assert_eq!(
            serde_json::from_str::<RemoteAuditRecord>(args[10]).unwrap(),
            record
        );
        assert!(!args[10].contains(['\n', '\r']));
    }

    #[tokio::test]
    async fn validated_pretty_marker_bytes_are_guarded_without_requiring_canonical_json() {
        let (target, marker, record) = fixture();
        let mut remote = FakeRemote::new([output(0, Vec::new())]);
        let mut pretty = serde_json::to_string_pretty(&marker).unwrap();
        pretty.push('\n');
        remote.marker_bytes = Some(pretty.as_bytes().to_vec());
        append_with_remote(
            &remote,
            &target,
            &marker,
            &record,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            remote.commands.lock().unwrap()[0].args[8].expose_for_execution(),
            pretty.trim_end_matches('\n')
        );
    }

    #[tokio::test]
    async fn unsafe_remote_paths_and_output_errors_are_not_echoed_or_retried() {
        let (target, marker, record) = fixture();
        for (status, expected) in [
            (42, AuditError::UnsafePath),
            (43, AuditError::Marker),
            (44, AuditError::Remote),
            (17, AuditError::Remote),
        ] {
            let remote = FakeRemote::new([output(status, b"private remote content".to_vec())]);
            let error = append_with_remote(
                &remote,
                &target,
                &marker,
                &record,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("private"));
            assert_eq!(remote.commands.lock().unwrap().len(), 1);
        }
        let mut truncated = output(0, Vec::new());
        truncated.stdout_truncated = true;
        assert_eq!(audit_output(truncated), Err(AuditError::Remote));
    }

    #[tokio::test]
    async fn missing_audits_are_unknown_and_read_mode_never_requests_mutation() {
        let (target, marker, _) = fixture();
        let remote = FakeRemote::new([output(44, Vec::new()), output(44, Vec::new())]);
        let history = read_with_remote(&remote, &target, &marker, &CancellationToken::new())
            .await
            .unwrap();
        assert!(history.records.is_empty());
        assert!(history.incomplete);
        assert_eq!(history.notices.len(), 2);
        for command in remote.commands.lock().unwrap().iter() {
            assert_eq!(command.args[9].expose_for_execution(), "read");
            assert_eq!(command.args[10].expose_for_execution(), "");
        }
    }

    #[tokio::test]
    async fn malformed_schema_and_tail_never_become_facts_or_swallow_next_valid_line() {
        let (target, marker, record) = fixture();
        let mut bad_schema = record.clone();
        bad_schema.schema_version = 2;
        let mut bytes = b"secret invalid unfinished previous tail\n\n".to_vec();
        bytes.extend(line(&bad_schema));
        bytes.extend(line(&record));
        bytes.extend_from_slice(b"{\"truncated\":");
        let remote = FakeRemote::new([output(0, bytes), output(0, Vec::new())]);
        let history = read_with_remote(&remote, &target, &marker, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(history.records, vec![record]);
        assert!(history.incomplete);
        assert_eq!(history.notices.len(), 3);
        assert!(
            history
                .notices
                .iter()
                .all(|notice| !notice.contains("secret"))
        );
    }

    #[test]
    fn exact_duplicates_deduplicate_but_conflicting_ids_remove_all_claims() {
        let (_, marker, record) = fixture();
        let mut history = RemoteAuditHistory::default();
        let mut conflicts = BTreeSet::new();
        let mut bytes = line(&record);
        bytes.extend(line(&record));
        parse_lines(
            &bytes,
            "releases.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        assert_eq!(history.records, vec![record.clone()]);
        assert!(!history.incomplete);
        let mut changed = record.clone();
        changed.recorded_at_ms += 1;
        let mut bytes = line(&changed);
        bytes.extend(line(&record));
        parse_lines(
            &bytes,
            "releases.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        assert!(history.records.is_empty());
        assert!(history.incomplete);
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn audit_preserves_historical_endpoint_revision_but_rejects_scope_and_phase_changes() {
        let (_, marker, mut record) = fixture();
        record.release.destination = DestinationKey::new();
        record.release.destination_revision = DestinationRevision::INITIAL.checked_next().unwrap();
        record.release.endpoint_fingerprint = EndpointFingerprint::parse("c".repeat(64)).unwrap();
        let mut history = RemoteAuditHistory::default();
        let mut conflicts = BTreeSet::new();
        parse_lines(
            &line(&record),
            "releases.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        assert_eq!(history.records, vec![record.clone()]);
        record.event_id = uuid::Uuid::now_v7();
        parse_lines(
            &line(&record),
            "deployments.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        record.release.component = ComponentName::parse("other").unwrap();
        parse_lines(
            &line(&record),
            "releases.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.notices.len(), 2);
    }

    #[test]
    fn byte_line_record_and_notice_limits_are_explicit() {
        let (_, marker, mut record) = fixture();
        let mut history = RemoteAuditHistory::default();
        let mut conflicts = BTreeSet::new();
        let mut oversized = vec![b'x'; MAX_LINE_BYTES + 1];
        oversized.push(b'\n');
        parse_lines(
            &oversized,
            "releases.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        parse_lines(
            &vec![b'\n'; MAX_FILE_BYTES + 1],
            "releases.jsonl",
            &marker,
            &mut history,
            &mut conflicts,
        );
        for _ in 0..MAX_RECORDS + MAX_NOTICES {
            record.event_id = uuid::Uuid::now_v7();
            parse_lines(
                &line(&record),
                "releases.jsonl",
                &marker,
                &mut history,
                &mut conflicts,
            );
        }
        assert!(history.incomplete);
        assert_eq!(history.records.len(), MAX_RECORDS);
        assert_eq!(history.notices.len(), MAX_NOTICES);
        assert!(
            history
                .notices
                .iter()
                .any(|notice| notice.contains("byte limit"))
        );
        assert!(
            history
                .notices
                .iter()
                .any(|notice| notice.contains("record limit"))
        );
    }

    #[tokio::test]
    async fn cancellation_and_total_deadline_bound_the_complete_operation() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert_eq!(
            bounded(
                &cancellation,
                std::future::pending::<Result<(), AuditError>>()
            )
            .await,
            Err(AuditError::Cancelled)
        );
        assert_eq!(
            bounded_for(
                &CancellationToken::new(),
                Duration::from_millis(1),
                std::future::pending::<Result<(), AuditError>>()
            )
            .await,
            Err(AuditError::Timeout)
        );
    }

    #[test]
    fn typed_validation_does_not_promote_failure_or_unknown_to_health() {
        let (_, _, mut record) = fixture();
        assert!(record.is_valid());
        record.healthy = Some(true);
        assert!(!record.is_valid());
        record.healthy = None;
        record.phase = RemoteAuditPhase::Rollback;
        record.package = None;
        record.target = None;
        record.expected_current = Some(record.release.version.clone());
        record.observed = RemoteAuditObserved::NotDeployed;
        assert!(record.is_valid());
        record.outcome = RemoteAuditOutcome::Failed;
        assert!(record.is_valid());
        record.healthy = Some(true);
        assert!(!record.is_valid());
        record.healthy = None;
        record.observed = RemoteAuditObserved::Unknown;
        assert!(record.is_valid());
    }

    #[cfg(target_os = "linux")]
    fn native_command(
        root: &str,
        marker: &DeploymentMarker,
        mode: &str,
        payload: &str,
    ) -> std::process::Output {
        std::process::Command::new("timeout")
            .args([
                "--kill-after=2",
                "25",
                "sh",
                "-c",
                AUDIT_SCRIPT,
                "shipforge-audit",
                root,
                "releases.jsonl",
                std::str::from_utf8(&marker.encode().unwrap()).unwrap(),
                mode,
                payload,
            ])
            .output()
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_script_appends_without_repair_and_rejects_symlinks_and_hardlinks() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("component");
        std::fs::create_dir(&root).unwrap();
        let (_, marker, record) = fixture();
        let root_text = root.to_str().unwrap();
        assert_eq!(
            native_command(root_text, &marker, "append", "{}")
                .status
                .code(),
            Some(43)
        );
        assert!(!root.join("metadata").exists());
        std::fs::write(
            root.join(".shipforge-project.json"),
            marker.encode().unwrap(),
        )
        .unwrap();
        assert_eq!(
            native_command(root_text, &marker, "read", "").status.code(),
            Some(44)
        );
        assert!(!root.join("metadata").exists());
        let payload = serde_json::to_string(&record).unwrap();
        assert!(
            native_command(root_text, &marker, "append", &payload)
                .status
                .success()
        );
        let path = root.join("metadata/releases.jsonl");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("\n{payload}\n").into_bytes()
        );
        std::fs::write(&path, b"truncated").unwrap();
        assert!(
            native_command(root_text, &marker, "append", &payload)
                .status
                .success()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("truncated\n{payload}\n").into_bytes()
        );
        let outside = directory.path().join("outside");
        std::fs::write(&outside, b"untouched").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();
        assert_eq!(
            native_command(root_text, &marker, "append", &payload)
                .status
                .code(),
            Some(42)
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert_eq!(
            native_command(root_text, &marker, "append", &payload)
                .status
                .code(),
            Some(42)
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
    }
}
