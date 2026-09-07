//! Explicit stdin sudo authentication using the saved connection password.

use super::{RemoteCommandOutput, SshConnectionError};
use crate::{
    config::ProtectedPassword,
    telemetry::{CommandArgument, CommandSpec},
};
use zeroize::{Zeroize, Zeroizing};

const MARKER: &[u8] = b"[shipforge-sudo-password]";

pub(super) struct Request {
    pub command: CommandSpec,
    password: ProtectedPassword,
}

pub(super) fn error() -> SshConnectionError {
    SshConnectionError::Command("Sudo password authentication could not complete; check the saved password and existing sudo policy.".into())
}

impl Request {
    pub fn for_command(
        command: &CommandSpec,
        password: Option<&ProtectedPassword>,
    ) -> Result<Option<Self>, SshConnectionError> {
        if command.shell
            || !matches!(
                command.program.as_str(),
                "sudo" | "/usr/bin/sudo" | "/bin/sudo"
            )
        {
            return Ok(None);
        }
        let args: Vec<_> = command
            .args
            .iter()
            .map(CommandArgument::expose_for_execution)
            .collect();
        if args.first() != Some(&"-S") {
            return Ok(None);
        }
        if args.get(1) != Some(&"--") || args.len() < 3 {
            return Err(SshConnectionError::Command("Use sudo -S -- followed by the executable and its arguments for saved-password authentication.".into()));
        }
        let password = password.ok_or_else(|| SshConnectionError::Command("Sudo -S requires a saved SSH password connection; key and Agent connections have no sudo password.".into()))?;
        let mut command = command.clone();
        command.args.splice(
            1..1,
            [
                CommandArgument::plain("-p"),
                CommandArgument::plain(std::str::from_utf8(MARKER).expect("ASCII prompt")),
            ],
        );
        Ok(Some(Self {
            command,
            password: password.clone(),
        }))
    }

    pub fn response(&self) -> Result<Zeroizing<String>, SshConnectionError> {
        let mut password = self.password.unlock().map_err(|_| error())?;
        if password.contains(['\r', '\n']) {
            return Err(error());
        }
        password.push('\n');
        Ok(password)
    }
}

#[derive(Default)]
pub(super) struct Prompt {
    tail: Vec<u8>,
    sent: bool,
}

impl Prompt {
    pub fn observe(&mut self, bytes: &[u8]) -> bool {
        if self.sent {
            return false;
        }
        for byte in bytes {
            self.tail.push(*byte);
            if self.tail.ends_with(MARKER) {
                self.sent = true;
                self.tail.clear();
                return true;
            }
            if self.tail.len() >= MARKER.len() {
                self.tail.remove(0);
            }
        }
        false
    }
}

// A sudo child or PAM module may echo input, including fragments. Never retain
// this channel's output in history or diagnostics; exit status remains evidence.
pub(super) fn sanitize(
    result: Result<RemoteCommandOutput, SshConnectionError>,
) -> Result<RemoteCommandOutput, SshConnectionError> {
    result
        .map(|mut output| {
            output.stdout.zeroize();
            output.stderr.zeroize();
            output.stdout_truncated = false;
            output.stderr_truncated = false;
            output
        })
        .map_err(|value| match value {
            SshConnectionError::Protocol(_) | SshConnectionError::ExitSignal { .. } => error(),
            other => other,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_sudo_requires_canonical_argv_and_saved_password() {
        let command = |args: &[&str]| {
            CommandSpec::structured(
                "/usr/bin/sudo",
                args.iter().copied().map(CommandArgument::plain),
            )
            .unwrap()
        };
        assert!(
            Request::for_command(&command(&["-n", "true"]), None)
                .unwrap()
                .is_none()
        );
        assert!(Request::for_command(&command(&["-S", "true"]), None).is_err());
        assert!(Request::for_command(&command(&["-S", "--", "true"]), None).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn password_never_enters_command_and_line_breaks_are_rejected() {
        let command =
            CommandSpec::structured("sudo", ["-S", "--", "true"].map(CommandArgument::plain))
                .unwrap();
        let protected = ProtectedPassword::protect("private-sudo-fixture").unwrap();
        let request = Request::for_command(&command, Some(&protected))
            .unwrap()
            .unwrap();
        assert!(
            !request
                .command
                .render_posix()
                .unwrap()
                .contains("private-sudo-fixture")
        );
        assert_eq!(
            request.response().unwrap().as_str(),
            "private-sudo-fixture\n"
        );
        let protected = ProtectedPassword::protect("first\nsecond").unwrap();
        assert!(
            Request::for_command(&command, Some(&protected))
                .unwrap()
                .unwrap()
                .response()
                .is_err()
        );
    }

    #[test]
    fn prompt_handles_split_packets_noise_and_only_one_attempt() {
        let mut prompt = Prompt::default();
        assert!(!prompt.observe(&vec![b'x'; 100_000]));
        assert!(!prompt.observe(&MARKER[..8]));
        assert!(prompt.observe(&MARKER[8..]));
        assert!(!prompt.observe(MARKER));
    }
}
