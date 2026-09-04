use std::{
    fmt,
    num::NonZeroU8,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh_sftp::{
    client::SftpSession,
    protocol::{FileAttributes, OpenFlags},
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::telemetry::{CommandArgument, CommandSpec};

use super::{AuthenticatedSession, SshConnectionError};

const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REMOTE_PATH_BYTES: usize = 4096;
const MAX_UPLOAD_ATTEMPTS: u8 = 5;
const MAX_REMOTE_ERROR_CHARS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

impl RemotePath {
    /// Validates one normalized absolute Linux file path.
    ///
    /// # Errors
    ///
    /// Returns an error for relative, root, repeated-separator, dot-segment,
    /// control-character, or oversized paths.
    pub fn parse(value: impl Into<String>) -> Result<Self, RemotePathError> {
        let value = value.into();
        if value.len() > MAX_REMOTE_PATH_BYTES {
            return Err(RemotePathError::TooLong);
        }
        if !value.starts_with('/') || value == "/" || value.ends_with('/') {
            return Err(RemotePathError::NotAbsoluteFile);
        }
        if value.chars().any(char::is_control) {
            return Err(RemotePathError::ControlCharacter);
        }
        if value[1..]
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        {
            return Err(RemotePathError::NotNormalized);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadOptions {
    pub max_attempts: NonZeroU8,
    pub attempt_timeout: Duration,
    pub retry_delay: Duration,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            max_attempts: NonZeroU8::new(3).expect("three is non-zero"),
            attempt_timeout: Duration::from_secs(120),
            retry_delay: Duration::from_millis(250),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadProgress {
    pub attempt: u8,
    pub sent: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadReceipt {
    pub remote_path: RemotePath,
    pub bytes: u64,
    pub attempts: u8,
}

impl AuthenticatedSession {
    /// Uploads one immutable Release to a unique remote temporary path.
    ///
    /// The remote file is created with SFTP `EXCLUDE`, never truncated. A
    /// failed attempt is retried only after its owned partial file is removed.
    /// Progress callbacks are monotonic within each numbered attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid local input, remote conflict, cancellation,
    /// timeout, exhausted retries, or failure to remove an owned partial file.
    pub async fn upload_release<F>(
        &self,
        local_path: &Path,
        remote_path: &RemotePath,
        options: UploadOptions,
        cancellation: &CancellationToken,
        progress: F,
    ) -> Result<UploadReceipt, UploadError>
    where
        F: Fn(UploadProgress) + Send + Sync,
    {
        if cancellation.is_cancelled() {
            return Err(UploadError::Cancelled);
        }
        validate_options(options)?;
        let metadata = tokio::fs::symlink_metadata(local_path)
            .await
            .map_err(|source| UploadError::LocalIo {
                path: local_path.to_owned(),
                source,
            })?;
        if !metadata.is_file() {
            return Err(UploadError::NotAFile(local_path.to_owned()));
        }
        let total = metadata.len();
        if total == 0 {
            return Err(UploadError::EmptyRelease(local_path.to_owned()));
        }

        for attempt in 1..=options.max_attempts.get() {
            progress(UploadProgress {
                attempt,
                sent: 0,
                total,
            });
            let created = Arc::new(AtomicBool::new(false));
            let attempt_progress = |sent, total| {
                progress(UploadProgress {
                    attempt,
                    sent,
                    total,
                });
            };
            let operation = upload_once(
                self,
                local_path,
                remote_path,
                total,
                &created,
                cancellation,
                &attempt_progress,
            );
            let outcome = tokio::select! {
                () = cancellation.cancelled() => AttemptOutcome::Cancelled,
                result = tokio::time::timeout(options.attempt_timeout, operation) => {
                    match result {
                        Ok(result) => AttemptOutcome::Finished(result),
                        Err(_) => AttemptOutcome::TimedOut,
                    }
                }
            };
            match outcome {
                AttemptOutcome::Finished(Ok(())) => {
                    return Ok(UploadReceipt {
                        remote_path: remote_path.clone(),
                        bytes: total,
                        attempts: attempt,
                    });
                }
                AttemptOutcome::Finished(Err(AttemptError::Conflict)) => {
                    return Err(UploadError::RemoteConflict(remote_path.clone()));
                }
                AttemptOutcome::Finished(Err(AttemptError::Local(source))) => {
                    cleanup_if_created(self, remote_path, &created).await?;
                    return Err(UploadError::LocalIo {
                        path: local_path.to_owned(),
                        source,
                    });
                }
                AttemptOutcome::Finished(Err(AttemptError::Cancelled))
                | AttemptOutcome::Cancelled => {
                    cleanup_if_created(self, remote_path, &created).await?;
                    return Err(UploadError::Cancelled);
                }
                AttemptOutcome::Finished(Err(AttemptError::Remote(message))) => {
                    cleanup_if_created(self, remote_path, &created).await?;
                    if attempt == options.max_attempts.get() {
                        return Err(UploadError::RetriesExhausted {
                            attempt,
                            message: sanitize_remote_error(&message),
                        });
                    }
                }
                AttemptOutcome::TimedOut => {
                    cleanup_if_created(self, remote_path, &created).await?;
                    if attempt == options.max_attempts.get() {
                        return Err(UploadError::Timeout {
                            attempts: attempt,
                            timeout: options.attempt_timeout,
                        });
                    }
                }
            }
            wait_retry(options.retry_delay, cancellation).await?;
        }
        unreachable!("UploadOptions always contains at least one attempt")
    }

    /// Verifies a remote Release with `sha256sum` over the existing SSH session.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed expected digests, command failure,
    /// malformed or truncated output, timeout, cancellation, or hash mismatch.
    pub async fn verify_remote_sha256(
        &self,
        remote_path: &RemotePath,
        expected: &str,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), UploadError> {
        let expected = normalize_sha256(expected)?;
        let command = CommandSpec::structured(
            "sha256sum",
            [
                CommandArgument::plain("--"),
                CommandArgument::plain(remote_path.as_str()),
            ],
        )
        .map_err(|error| UploadError::HashCommand(error.to_string()))?;
        let output = self
            .execute(&command, timeout, cancellation)
            .await
            .map_err(UploadError::Ssh)?;
        if output.exit_status != 0 {
            return Err(UploadError::HashCommandFailed(output.exit_status));
        }
        if output.stdout_truncated {
            return Err(UploadError::MalformedHashOutput);
        }
        let actual = String::from_utf8(output.stdout)
            .ok()
            .and_then(|stdout| stdout.split_whitespace().next().map(str::to_owned))
            .and_then(|digest| normalize_sha256(&digest).ok())
            .ok_or(UploadError::MalformedHashOutput)?;
        if actual != expected {
            return Err(UploadError::HashMismatch { expected, actual });
        }
        Ok(())
    }
}

enum AttemptOutcome {
    Finished(Result<(), AttemptError>),
    Cancelled,
    TimedOut,
}

enum AttemptError {
    Conflict,
    Cancelled,
    Local(std::io::Error),
    Remote(String),
}

async fn upload_once<F>(
    session: &AuthenticatedSession,
    local_path: &Path,
    remote_path: &RemotePath,
    total: u64,
    created: &AtomicBool,
    cancellation: &CancellationToken,
    progress: &F,
) -> Result<(), AttemptError>
where
    F: Fn(u64, u64) + Send + Sync,
{
    let sftp = open_sftp(session).await.map_err(AttemptError::Remote)?;
    let exists = sftp
        .try_exists(remote_path.as_str())
        .await
        .map_err(|error| AttemptError::Remote(sanitize_remote_error(&error.to_string())))?;
    if exists {
        return Err(AttemptError::Conflict);
    }
    let mut attributes = FileAttributes {
        permissions: Some(0o600),
        ..FileAttributes::default()
    };
    attributes.set_regular(true);
    let mut remote = sftp
        .open_with_flags_and_attributes(
            remote_path.as_str(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            attributes,
        )
        .await
        .map_err(|error| AttemptError::Remote(sanitize_remote_error(&error.to_string())))?;
    created.store(true, Ordering::Release);
    let mut local = tokio::fs::File::open(local_path)
        .await
        .map_err(AttemptError::Local)?;
    let mut buffer = vec![0_u8; UPLOAD_CHUNK_BYTES];
    let mut sent = 0_u64;
    loop {
        if cancellation.is_cancelled() {
            return Err(AttemptError::Cancelled);
        }
        let read = local.read(&mut buffer).await.map_err(AttemptError::Local)?;
        if read == 0 {
            break;
        }
        remote
            .write_all(&buffer[..read])
            .await
            .map_err(|error| AttemptError::Remote(sanitize_remote_error(&error.to_string())))?;
        sent = sent
            .checked_add(read as u64)
            .ok_or_else(|| AttemptError::Remote("uploaded byte count overflowed".into()))?;
        if sent > total {
            return Err(AttemptError::Local(std::io::Error::other(
                "local Release changed size during upload",
            )));
        }
        progress(sent, total);
    }
    if sent != total {
        return Err(AttemptError::Local(std::io::Error::other(
            "local Release changed size during upload",
        )));
    }
    remote
        .sync_all()
        .await
        .map_err(|error| AttemptError::Remote(sanitize_remote_error(&error.to_string())))?;
    remote
        .close()
        .await
        .map_err(|error| AttemptError::Remote(sanitize_remote_error(&error.to_string())))?;
    sftp.close()
        .await
        .map_err(|error| AttemptError::Remote(sanitize_remote_error(&error.to_string())))?;
    created.store(false, Ordering::Release);
    Ok(())
}

async fn open_sftp(session: &AuthenticatedSession) -> Result<SftpSession, String> {
    let channel = session
        .handle
        .channel_open_session()
        .await
        .map_err(|error| sanitize_remote_error(&error.to_string()))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| sanitize_remote_error(&error.to_string()))?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| sanitize_remote_error(&error.to_string()))
}

async fn cleanup_if_created(
    session: &AuthenticatedSession,
    remote_path: &RemotePath,
    created: &AtomicBool,
) -> Result<(), UploadError> {
    if !created.load(Ordering::Acquire) {
        return Ok(());
    }
    let operation = async {
        let sftp = open_sftp(session).await?;
        if sftp
            .try_exists(remote_path.as_str())
            .await
            .map_err(|error| sanitize_remote_error(&error.to_string()))?
        {
            sftp.remove_file(remote_path.as_str())
                .await
                .map_err(|error| sanitize_remote_error(&error.to_string()))?;
        }
        sftp.close()
            .await
            .map_err(|error| sanitize_remote_error(&error.to_string()))
    };
    tokio::time::timeout(CLEANUP_TIMEOUT, operation)
        .await
        .map_err(|_| UploadError::CleanupTimeout(remote_path.clone()))?
        .map_err(|message| UploadError::CleanupFailed {
            path: remote_path.clone(),
            message,
        })
}

async fn wait_retry(delay: Duration, cancellation: &CancellationToken) -> Result<(), UploadError> {
    tokio::select! {
        () = cancellation.cancelled() => Err(UploadError::Cancelled),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

fn normalize_sha256(value: &str) -> Result<String, UploadError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UploadError::InvalidSha256);
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_options(options: UploadOptions) -> Result<(), UploadError> {
    if options.attempt_timeout.is_zero() {
        return Err(UploadError::ZeroTimeout);
    }
    if options.max_attempts.get() > MAX_UPLOAD_ATTEMPTS {
        return Err(UploadError::TooManyAttempts);
    }
    Ok(())
}

pub(super) fn sanitize_remote_error(message: &str) -> String {
    let sanitized = message
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_REMOTE_ERROR_CHARS)
        .collect::<String>();
    if sanitized.trim().is_empty() {
        "remote operation failed".into()
    } else {
        sanitized
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RemotePathError {
    #[error("remote upload path must be a non-root absolute file path")]
    NotAbsoluteFile,
    #[error("remote upload path must not contain empty, `.` or `..` segments")]
    NotNormalized,
    #[error("remote upload path must not contain control characters")]
    ControlCharacter,
    #[error("remote upload path exceeds {MAX_REMOTE_PATH_BYTES} bytes")]
    TooLong,
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("Release upload was cancelled")]
    Cancelled,
    #[error("Release upload attempt timeout must be non-zero")]
    ZeroTimeout,
    #[error("Release upload is limited to {MAX_UPLOAD_ATTEMPTS} attempts")]
    TooManyAttempts,
    #[error("local Release is not a regular file: `{0}`")]
    NotAFile(PathBuf),
    #[error("local Release is empty: `{0}`")]
    EmptyRelease(PathBuf),
    #[error("could not read local Release `{path}`: {source}")]
    LocalIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("remote upload path already exists and will not be overwritten: `{0}`")]
    RemoteConflict(RemotePath),
    #[error("Release upload failed after {attempt} attempts: {message}")]
    RetriesExhausted { attempt: u8, message: String },
    #[error("Release upload timed out after {attempts} attempts of {timeout:?}")]
    Timeout { attempts: u8, timeout: Duration },
    #[error("partial remote Release cleanup timed out: `{0}`")]
    CleanupTimeout(RemotePath),
    #[error("partial remote Release cleanup failed at `{path}`: {message}")]
    CleanupFailed { path: RemotePath, message: String },
    #[error("expected SHA-256 must contain exactly 64 hexadecimal characters")]
    InvalidSha256,
    #[error("could not construct remote hash command: {0}")]
    HashCommand(String),
    #[error("remote sha256sum exited with status {0}")]
    HashCommandFailed(u32),
    #[error("remote sha256sum returned malformed or truncated output")]
    MalformedHashOutput,
    #[error("remote Release SHA-256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error(transparent)]
    Ssh(#[from] SshConnectionError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_requires_normalized_absolute_file() {
        assert!(RemotePath::parse("/srv/app/temp/release.tar.gz").is_ok());
        for invalid in [
            "relative/file",
            "/",
            "/srv//file",
            "/srv/../file",
            "/srv/file/",
            "/srv/file\nname",
        ] {
            assert!(RemotePath::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn sha256_validation_is_exact_and_canonical() {
        let uppercase = "A".repeat(64);
        assert_eq!(normalize_sha256(&uppercase).unwrap(), "a".repeat(64));
        assert!(normalize_sha256(&"a".repeat(63)).is_err());
        assert!(normalize_sha256(&format!("{}g", "a".repeat(63))).is_err());
    }

    #[test]
    fn upload_options_reject_zero_timeout_and_excessive_retries() {
        let default = UploadOptions::default();
        assert!(validate_options(default).is_ok());
        assert!(matches!(
            validate_options(UploadOptions {
                attempt_timeout: Duration::ZERO,
                ..default
            }),
            Err(UploadError::ZeroTimeout)
        ));
        assert!(matches!(
            validate_options(UploadOptions {
                max_attempts: NonZeroU8::new(MAX_UPLOAD_ATTEMPTS + 1).unwrap(),
                ..default
            }),
            Err(UploadError::TooManyAttempts)
        ));
    }

    #[test]
    fn remote_errors_are_bounded_and_single_line() {
        let message = format!("bad\n{}", "x".repeat(MAX_REMOTE_ERROR_CHARS + 100));
        let sanitized = sanitize_remote_error(&message);
        assert!(!sanitized.contains('\n'));
        assert_eq!(sanitized.chars().count(), MAX_REMOTE_ERROR_CHARS);
        assert_eq!(sanitize_remote_error("\r\n"), "remote operation failed");
    }

    #[tokio::test]
    async fn cancelled_retry_delay_returns_immediately() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            wait_retry(Duration::from_secs(60), &cancellation).await,
            Err(UploadError::Cancelled)
        ));
    }
}
