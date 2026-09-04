use std::{fmt, ops::Deref};

use thiserror::Error;

use crate::config::HostKeyFingerprint;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn expose_secret(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Default)]
pub struct Redactor {
    values: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Redactor")
            .field("registered_values", &self.values.len())
            .finish_non_exhaustive()
    }
}

impl Redactor {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values = values
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        values.sort_by_key(|right| std::cmp::Reverse(right.len()));
        values.dedup();
        Self { values }
    }

    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        self.values.iter().fold(input.to_owned(), |text, secret| {
            text.replace(secret, "[REDACTED]")
        })
    }

    pub(crate) fn values(&self) -> &[String] {
        &self.values
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CommandArgument {
    value: String,
    sensitive: bool,
}

impl CommandArgument {
    #[must_use]
    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            sensitive: false,
        }
    }

    #[must_use]
    pub fn sensitive(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            sensitive: true,
        }
    }

    #[must_use]
    pub fn expose_for_execution(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for CommandArgument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.sensitive {
            formatter.write_str("[REDACTED]")
        } else {
            self.value.fmt(formatter)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<CommandArgument>,
    pub shell: bool,
}

impl CommandSpec {
    /// Creates a structured command without invoking a Shell.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty program or embedded NUL bytes.
    pub fn structured(
        program: impl Into<String>,
        args: impl IntoIterator<Item = CommandArgument>,
    ) -> Result<Self, SecurityError> {
        let program = program.into();
        let args = args.into_iter().collect::<Vec<_>>();
        if program.is_empty()
            || program.contains('\0')
            || args.iter().any(|argument| argument.value.contains('\0'))
        {
            return Err(SecurityError::InvalidCommand);
        }
        Ok(Self {
            program,
            args,
            shell: false,
        })
    }

    /// Renders a command for a POSIX remote Shell by quoting every argument.
    ///
    /// # Errors
    ///
    /// Returns an error if this is an explicit local Shell script rather than a
    /// structured program and argument list.
    pub fn render_posix(&self) -> Result<String, SecurityError> {
        if self.shell {
            return Err(SecurityError::AlreadyShell);
        }
        Ok(std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(CommandArgument::expose_for_execution))
            .map(quote_posix_argument)
            .collect::<Vec<_>>()
            .join(" "))
    }
}

/// Quotes one argument for a POSIX-compatible Shell.
#[must_use]
pub fn quote_posix_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyPolicy {
    Strict { expected: HostKeyFingerprint },
}

/// Scans Project YAML for secret material and user-specific credential paths.
///
/// # Errors
///
/// Returns the first prohibited field path or private-key payload marker.
pub fn detect_sensitive_config(contents: &str) -> Result<(), SecurityError> {
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(contents).map_err(SecurityError::InvalidYaml)?;
    inspect_yaml(&value, "$")
}

fn inspect_yaml(value: &serde_yaml_ng::Value, path: &str) -> Result<(), SecurityError> {
    match value {
        serde_yaml_ng::Value::Mapping(mapping) => {
            for (key, value) in mapping {
                let key = key.as_str().unwrap_or("<non-string>");
                let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
                if ["password", "token", "secret", "privatekey", "identityfile"]
                    .iter()
                    .any(|prohibited| normalized.contains(prohibited))
                {
                    return Err(SecurityError::SensitiveConfig(format!("{path}.{key}")));
                }
                inspect_yaml(value, &format!("{path}.{key}"))?;
            }
        }
        serde_yaml_ng::Value::Sequence(sequence) => {
            for (index, value) in sequence.iter().enumerate() {
                inspect_yaml(value, &format!("{path}[{index}]"))?;
            }
        }
        serde_yaml_ng::Value::String(value)
            if value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----") =>
        {
            return Err(SecurityError::SensitiveConfig(path.into()));
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum SecurityError {
    #[error("command program is empty or contains an embedded NUL byte")]
    InvalidCommand,
    #[error("explicit Shell commands cannot be rendered as structured remote commands")]
    AlreadyShell,
    #[error("invalid YAML while checking sensitive configuration: {0}")]
    InvalidYaml(serde_yaml_ng::Error),
    #[error("Project configuration contains prohibited sensitive data at {0}")]
    SensitiveConfig(String),
}

impl<T> Deref for Secret<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.expose_secret()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_formats_its_value() {
        let secret = Secret::new("top-secret".to_owned());
        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
    }

    #[test]
    fn redactor_replaces_longest_registered_values() {
        let redactor = Redactor::new(["token".into(), "token-123".into()]);
        assert_eq!(
            redactor.redact("authorization=token-123"),
            "authorization=[REDACTED]"
        );
    }

    #[test]
    fn redactor_debug_never_exposes_registered_secrets() {
        let redactor = Redactor::new(["never-print-this-token".into()]);
        let debug = format!("{redactor:?}");
        assert!(debug.contains("registered_values: 1"));
        assert!(!debug.contains("never-print"));
    }

    #[test]
    fn structured_remote_command_quotes_shell_metacharacters() {
        let command = CommandSpec::structured(
            "systemctl",
            [
                CommandArgument::plain("restart"),
                CommandArgument::plain("api'; rm -rf / #"),
            ],
        )
        .unwrap();
        assert_eq!(
            command.render_posix().unwrap(),
            "'systemctl' 'restart' 'api'\\''; rm -rf / #'"
        );
    }

    #[test]
    fn command_debug_redacts_sensitive_arguments() {
        let command =
            CommandSpec::structured("client", [CommandArgument::sensitive("token-123")]).unwrap();
        let debug = format!("{command:?}");
        assert!(!debug.contains("token-123"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn project_config_rejects_secret_fields_and_private_key_payloads() {
        assert!(matches!(
            detect_sensitive_config("project: demo\napiToken: abc"),
            Err(SecurityError::SensitiveConfig(path)) if path == "$.apiToken"
        ));
        assert!(matches!(
            detect_sensitive_config("project: |\n  -----BEGIN OPENSSH PRIVATE KEY-----\n  material"),
            Err(SecurityError::SensitiveConfig(path)) if path == "$.project"
        ));
    }
}
