//! Canonical remote service intent. Presets populate this same command plan.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Ordered argv commands for one service lifecycle operation.
pub type ServiceAction = Vec<Vec<String>>;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceConfig {
    pub start: ServiceAction,
    pub stop: ServiceAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update: ServiceAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore: ServiceAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<ServiceCheck>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ServiceCheck {
    Command { argv: Vec<String> },
    Systemd { unit: String },
}

impl From<String> for ServiceConfig {
    fn from(unit: String) -> Self {
        Self::systemd(unit)
    }
}

impl From<&str> for ServiceConfig {
    fn from(unit: &str) -> Self {
        Self::systemd(unit)
    }
}

impl fmt::Debug for ServiceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceConfig")
            .field("commands", &"[REDACTED]")
            .field("check", &self.check.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ServiceCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServiceCheck([REDACTED])")
    }
}

impl ServiceConfig {
    /// Generates a preset; validation still occurs at configuration boundaries.
    #[must_use]
    pub fn systemd(unit: impl Into<String>) -> Self {
        let unit = unit.into();
        Self {
            start: vec![vec![
                "systemctl".into(),
                "restart".into(),
                "--".into(),
                unit.clone(),
            ]],
            stop: vec![vec![
                "systemctl".into(),
                "stop".into(),
                "--".into(),
                unit.clone(),
            ]],
            update: Vec::new(),
            restore: Vec::new(),
            check: Some(ServiceCheck::Systemd { unit }),
        }
    }

    #[must_use]
    pub fn update_commands(&self) -> &ServiceAction {
        if self.update.is_empty() {
            &self.start
        } else {
            &self.update
        }
    }

    #[must_use]
    pub fn restore_commands(&self) -> &ServiceAction {
        if self.restore.is_empty() {
            self.update_commands()
        } else {
            &self.restore
        }
    }

    #[must_use]
    pub fn systemd_unit(&self) -> Option<&str> {
        match &self.check {
            Some(ServiceCheck::Systemd { unit }) => Some(unit),
            _ => None,
        }
    }

    /// Only an unchanged preset may be represented by the unit-only editor.
    #[must_use]
    pub fn preset_unit(&self) -> Option<&str> {
        self.systemd_unit()
            .filter(|unit| *self == Self::systemd(*unit))
    }

    /// Validates bounded literal argv, required recovery and optional checks.
    ///
    /// # Errors
    /// Returns a fixed diagnostic without echoing user command text.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.start.is_empty() || self.stop.is_empty() {
            return Err("Service start and stop commands are required.");
        }
        let mut total = 0usize;
        for action in [&self.start, &self.stop, &self.update, &self.restore] {
            if action.len() > 16 {
                return Err("A service action supports at most 16 commands.");
            }
            for command in action {
                validate_argv(command)?;
                total = total.saturating_add(command.iter().map(String::len).sum::<usize>());
            }
        }
        if let Some(check) = &self.check {
            match check {
                ServiceCheck::Command { argv } => {
                    validate_argv(argv)?;
                    total = total.saturating_add(argv.iter().map(String::len).sum::<usize>());
                }
                ServiceCheck::Systemd { unit } if !valid_unit(unit) => {
                    return Err("Use a bounded, complete systemd .service unit name.");
                }
                ServiceCheck::Systemd { .. } => {}
            }
        }
        if total > 16 * 1024 {
            return Err("Service command configuration exceeds 16 KiB.");
        }
        Ok(())
    }
}

pub(crate) fn validate_argv(argv: &[String]) -> Result<(), &'static str> {
    let Some(program) = argv.first() else {
        return Err("A service command needs an executable program.");
    };
    if program.is_empty()
        || program.starts_with('-')
        || program.chars().any(char::is_whitespace)
        || argv.len() > 128
        || argv
            .iter()
            .any(|arg| arg.len() > 4096 || arg.chars().any(|c| c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')))
    {
        return Err(
            "Use bounded executable and literal argument fields, without control characters.",
        );
    }
    let basename = program.rsplit('/').next().unwrap_or(program);
    if argv.iter().skip(1).any(|arg| {
        let name = arg
            .split(['=', ':'])
            .next()
            .unwrap_or(arg)
            .to_ascii_lowercase()
            .replace(['-', '_'], "");
        (arg.starts_with('-') || arg.contains(['=', ':']))
            && [
                "password",
                "passwd",
                "token",
                "secret",
                "apikey",
                "privatekey",
                "credential",
                "authorization",
            ]
            .iter()
            .any(|key| name.contains(key))
    }) {
        return Err(
            "Service commands cannot contain credential arguments; configure credentials on the server.",
        );
    }
    if matches!(basename, "sh" | "bash" | "dash" | "zsh" | "ksh")
        && argv
            .iter()
            .skip(1)
            .any(|arg| arg.starts_with('-') && arg.contains('c'))
    {
        return Err(
            "Shell command strings are not supported; choose a script file and literal arguments.",
        );
    }
    Ok(())
}

pub(crate) fn valid_unit(unit: &str) -> bool {
    unit.len() <= 255
        && unit.len() > ".service".len()
        && unit.ends_with(".service")
        && unit.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@' | b':')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_is_a_regular_command_plan_with_stable_health() {
        let service = ServiceConfig::systemd("api.service");
        service.validate().unwrap();
        assert_eq!(
            service.start[0],
            ["systemctl", "restart", "--", "api.service"]
        );
        assert_eq!(service.update_commands(), &service.start);
        assert_eq!(service.restore_commands(), &service.start);
        assert_eq!(service.preset_unit(), Some("api.service"));
        let yaml = serde_yaml_ng::to_string(&service).unwrap();
        assert_eq!(
            serde_yaml_ng::from_str::<ServiceConfig>(&yaml).unwrap(),
            service
        );
    }

    #[test]
    fn custom_commands_preserve_literal_arguments_and_recovery_overrides() {
        let mut service = ServiceConfig::systemd("api.service");
        service.start = vec![vec!["pm2".into(), "start".into(), "a b.js".into()]];
        service.update = vec![vec!["pm2".into(), "restart".into(), "api".into()]];
        service.check = Some(ServiceCheck::Command {
            argv: vec!["test".into(), "-f".into(), "ready".into()],
        });
        service.validate().unwrap();
        assert_eq!(service.restore_commands(), &service.update);
        assert_eq!(service.systemd_unit(), None);
        assert_eq!(service.preset_unit(), None);
        assert!(!format!("{service:?}").contains("a b.js"));
    }

    #[test]
    fn rejects_missing_recovery_shell_strings_controls_and_excessive_output() {
        let mut service = ServiceConfig::systemd("api.service");
        service.stop.clear();
        assert!(service.validate().is_err());
        for argv in [
            vec![],
            vec!["sh", "-c", "echo unsafe"],
            vec!["pm2 restart"],
            vec!["pm2", "a\nb"],
            vec!["tool", "--api-key", "fixture-secret"],
            vec!["env", "TOKEN=fixture-secret", "tool"],
        ] {
            assert!(
                validate_argv(&argv.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
        for unit in ["api", ".service", "api;echo.service"] {
            assert!(ServiceConfig::systemd(unit).validate().is_err());
        }
        let mut oversized = ServiceConfig::systemd("api.service");
        oversized.start[0].extend(vec!["x".repeat(4096); 5]);
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn rejects_alternate_service_shapes_and_accepts_optional_action_defaults() {
        for yaml in [
            "api.service",
            "{start: pm2, stop: pm2}",
            "{start: [[pm2]], stop: [[pm2]], preset: pm2}",
        ] {
            assert!(serde_yaml_ng::from_str::<ServiceConfig>(yaml).is_err());
        }
        let service: ServiceConfig = serde_yaml_ng::from_str(
            "start: [[node, service.cjs, activate]]\nstop: [[node, service.cjs, stop]]\n",
        )
        .unwrap();
        service.validate().unwrap();
        assert_eq!(service.restore_commands(), &service.start);
        assert_eq!(service.check, None);
    }
}
