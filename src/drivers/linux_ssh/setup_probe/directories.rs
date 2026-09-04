//! Strict, bounded, read-only enumeration of one explicitly chosen directory.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{
    application::RemoteDirectoryCandidates,
    telemetry::{CommandArgument, CommandSpec},
};

use super::super::{AuthenticatedSession, RemoteCommandOutput, SshConnectionError};

const MAX_BYTES: usize = 64 * 1024;
const MAX_DIRECTORIES: usize = 512;
const MAX_PATH_BYTES: usize = 4096;

pub(in crate::drivers::linux_ssh) fn validate_browse_path(path: &str) -> Result<(), &'static str> {
    if !path.starts_with('/')
        || path.len() > MAX_PATH_BYTES
        || path.chars().any(|character| {
            character.is_control()
                || matches!(character, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
        })
        || (path != "/" && path.split('/').skip(1).any(|segment| matches!(segment, "" | "." | "..")))
    {
        return Err("choose a canonical absolute directory without control characters");
    }
    Ok(())
}

pub(in crate::drivers::linux_ssh) async fn browse_remote_directories(
    session: &AuthenticatedSession,
    root: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<RemoteDirectoryCandidates, &'static str> {
    validate_browse_path(root)?;
    let (canonical, directory, list) = commands(root)?;
    let operation = async {
        let output = session
            .execute(&canonical, timeout, cancellation)
            .await
            .map_err(|error| command_failure(&error, "directory canonical-path check failed"))?;
        require_canonical(&output, root)?;
        let output = session
            .execute(&directory, timeout, cancellation)
            .await
            .map_err(|error| command_failure(&error, "directory type check failed"))?;
        require_success(&output)?;
        if !output.stdout.is_empty() {
            return Err("directory type check returned unexpected output");
        }
        let output = session
            .execute(&list, timeout, cancellation)
            .await
            .map_err(|error| command_failure(&error, "directory enumeration failed"))?;
        let directories = parse_directory_output(root, &output)?;
        // Resolve again after enumeration; symlink/ancestor substitution must
        // not turn a selected spelling into observations from another path.
        let output = session
            .execute(&canonical, timeout, cancellation)
            .await
            .map_err(|error| command_failure(&error, "directory canonical-path recheck failed"))?;
        require_canonical(&output, root)?;
        Ok(RemoteDirectoryCandidates {
            directory: root.to_owned(),
            directories,
        })
    };
    tokio::select! {
        () = cancellation.cancelled() => Err("directory browsing cancelled"),
        result = tokio::time::timeout(timeout, operation) => result.map_err(|_| "directory browsing timed out")?,
    }
}

fn command_failure(error: &SshConnectionError, fallback: &'static str) -> &'static str {
    // Inner command and outer browsing deadlines can win the same poll. Keep
    // their public categories consistent without exposing transport diagnostics.
    match error {
        SshConnectionError::Timeout { .. } => "directory browsing timed out",
        SshConnectionError::Cancelled => "directory browsing cancelled",
        _ => fallback,
    }
}

fn commands(root: &str) -> Result<(CommandSpec, CommandSpec, CommandSpec), &'static str> {
    validate_browse_path(root)?;
    let command = |program, arguments: &[&str]| {
        CommandSpec::structured(
            program,
            arguments
                .iter()
                .map(|argument| CommandArgument::plain(*argument)),
        )
        .map_err(|_| "could not prepare a safe directory command")
    };
    Ok((
        command("readlink", &["-e", "--", root])?,
        command("test", &["-d", root])?,
        command(
            "find",
            &[
                "-P",
                root,
                "-mindepth",
                "1",
                "-maxdepth",
                "1",
                "-type",
                "d",
                "-printf",
                "%p\\0",
            ],
        )?,
    ))
}

fn require_success(output: &RemoteCommandOutput) -> Result<(), &'static str> {
    if output.exit_status != 0
        || output.stdout_truncated
        || output.stderr_truncated
        || !output.stderr.is_empty()
        || output.stdout.len() > MAX_BYTES
    {
        Err("directory evidence is incomplete or unavailable; no directory list was accepted")
    } else {
        Ok(())
    }
}

fn require_canonical(output: &RemoteCommandOutput, root: &str) -> Result<(), &'static str> {
    require_success(output)?;
    if output.stdout.strip_suffix(b"\n") == Some(root.as_bytes()) {
        Ok(())
    } else {
        Err(
            "selected directory is missing, linked or noncanonical; choose its physical absolute path",
        )
    }
}

fn parse_directory_output(
    root: &str,
    output: &RemoteCommandOutput,
) -> Result<Vec<String>, &'static str> {
    validate_browse_path(root)?;
    require_success(output)?;
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let text =
        std::str::from_utf8(&output.stdout).map_err(|_| "directory names must be valid UTF-8")?;
    if !text.ends_with('\0') {
        return Err("directory evidence has no complete NUL terminator");
    }
    let mut directories = Vec::new();
    for path in text.split_terminator('\0') {
        if directories.len() >= MAX_DIRECTORIES {
            return Err("directory has too many candidates; choose a narrower path");
        }
        validate_browse_path(path)?;
        let Some((parent, name)) = path.rsplit_once('/') else {
            return Err("directory candidate is not an absolute direct child");
        };
        if name.is_empty() || name.len() > 255 || parent != if root == "/" { "" } else { root } {
            return Err(
                "directory candidate is outside the selected directory or is not a direct child",
            );
        }
        directories.push(path.to_owned());
    }
    directories.sort();
    if directories.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("directory evidence contains duplicate candidates");
    }
    Ok(directories)
}

#[cfg(test)]
mod tests;
