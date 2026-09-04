use std::{fmt, num::NonZeroU64, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DestinationKey(String);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DestinationKeyError {
    #[error("Destination key must be an auto-generated dst_ identifier")]
    Format,
}

impl DestinationKey {
    #[must_use]
    pub fn new() -> Self {
        Self(format!("dst_{}", uuid::Uuid::now_v7().simple()))
    }

    /// Parses a canonical immutable Destination key.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value has the generated `dst_` form.
    pub fn parse(value: impl Into<String>) -> Result<Self, DestinationKeyError> {
        let value = value.into();
        let valid = value.strip_prefix("dst_").is_some_and(|identifier| {
            identifier.len() == 32
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err(DestinationKeyError::Format);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for DestinationKey {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DestinationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for DestinationKey {
    type Err = DestinationKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for DestinationKey {
    type Error = DestinationKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<DestinationKey> for String {
    fn from(value: DestinationKey) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DestinationRevision(NonZeroU64);

impl DestinationRevision {
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
    fn destination_key_accepts_generated_form() {
        let key = DestinationKey::new();
        assert!(key.as_str().starts_with("dst_"));
        assert_eq!(key.as_str().len(), 36);
        assert_eq!(DestinationKey::parse(key.to_string()).unwrap(), key);
    }

    #[test]
    fn destination_key_rejects_names_and_malformed_identifiers() {
        for invalid in [
            "app-server",
            "dst_short",
            "dst_0000000000000000000000000000000g",
        ] {
            assert!(
                DestinationKey::parse(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn destination_revision_is_monotonic() {
        assert_eq!(
            DestinationRevision::INITIAL
                .checked_next()
                .map(DestinationRevision::get),
            Some(2)
        );
    }
}
