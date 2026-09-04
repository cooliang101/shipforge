use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    ComponentGeneration, ComponentName, DestinationKey, DestinationRevision, EnvironmentId,
    ProjectId,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReleaseVersion(String);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReleaseVersionError {
    #[error("Release version must contain 1 to 128 characters")]
    Length,
    #[error(
        "Release version may contain only ASCII letters, digits, dots, hyphens, and underscores"
    )]
    Characters,
}

impl ReleaseVersion {
    /// Parses a filesystem-safe Release version.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is empty, too long, or contains a path
    /// separator or another unsupported character.
    pub fn parse(value: impl Into<String>) -> Result<Self, ReleaseVersionError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(ReleaseVersionError::Length);
        }
        if matches!(value.as_str(), "." | "..") {
            return Err(ReleaseVersionError::Characters);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(ReleaseVersionError::Characters);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ReleaseVersion {
    type Err = ReleaseVersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ReleaseVersion {
    type Error = ReleaseVersionError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ReleaseVersion> for String {
    fn from(value: ReleaseVersion) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentRelease {
    pub project_id: ProjectId,
    pub environment_id: EnvironmentId,
    pub component: ComponentName,
    pub generation: ComponentGeneration,
    pub version: ReleaseVersion,
    pub destination: DestinationKey,
    pub destination_revision: DestinationRevision,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_version_rejects_path_segments() {
        assert!(ReleaseVersion::parse("../current").is_err());
        assert!(ReleaseVersion::parse("release/one").is_err());
        assert!(ReleaseVersion::parse(".").is_err());
        assert!(ReleaseVersion::parse("..").is_err());
    }
}
