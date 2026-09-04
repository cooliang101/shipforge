use std::{fmt, num::NonZeroU64, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ComponentName(String);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ComponentNameError {
    #[error("Component name must contain 1 to 63 characters")]
    Length,
    #[error("Component name must use lowercase letters, digits, or single hyphens")]
    Characters,
}

impl ComponentName {
    /// Parses a canonical Component name.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is empty, too long, or is not lowercase
    /// kebab-case.
    pub fn parse(value: impl Into<String>) -> Result<Self, ComponentNameError> {
        let value = value.into();
        if value.is_empty() || value.len() > 63 {
            return Err(ComponentNameError::Length);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || value.starts_with('-')
            || value.ends_with('-')
            || value.contains("--")
        {
            return Err(ComponentNameError::Characters);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComponentName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ComponentName {
    type Err = ComponentNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ComponentName {
    type Error = ComponentNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ComponentName> for String {
    fn from(value: ComponentName) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ComponentGeneration(NonZeroU64);

impl ComponentGeneration {
    pub const INITIAL: Self = Self(NonZeroU64::MIN);

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_generation_increments_monotonically() {
        assert_eq!(
            ComponentGeneration::INITIAL
                .checked_next()
                .map(ComponentGeneration::get),
            Some(2)
        );
    }
}
