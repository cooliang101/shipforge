use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdParseError {
    #[error("expected an ID beginning with {expected_prefix}_")]
    Prefix { expected_prefix: &'static str },
    #[error("ID body must contain 8 to 64 ASCII letters or digits")]
    Body,
}

macro_rules! stable_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7().simple().to_string())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!($prefix, "_{}"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let raw =
                    value
                        .strip_prefix(concat!($prefix, "_"))
                        .ok_or(IdParseError::Prefix {
                            expected_prefix: $prefix,
                        })?;
                if !(8..=64).contains(&raw.len())
                    || !raw.bytes().all(|byte| byte.is_ascii_alphanumeric())
                {
                    return Err(IdParseError::Body);
                }
                Ok(Self(raw.to_owned()))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

stable_id!(ProjectId, "prj");
stable_id!(EnvironmentId, "env");
stable_id!(DeploymentId, "dep");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_round_trip_through_text() {
        let id = ProjectId::new();
        assert_eq!(id.to_string().parse(), Ok(id));
    }

    #[test]
    fn parsing_rejects_the_wrong_domain_prefix() {
        let environment = EnvironmentId::new();
        let result = environment.to_string().parse::<ProjectId>();
        assert!(matches!(
            result,
            Err(IdParseError::Prefix {
                expected_prefix: "prj"
            })
        ));
    }

    #[test]
    fn serialized_ids_keep_their_domain_prefix() {
        let id = DeploymentId::new();
        let serialized = serde_json::to_string(&id).unwrap();
        assert!(serialized.starts_with("\"dep_"));
        assert_eq!(
            serde_json::from_str::<DeploymentId>(&serialized).unwrap(),
            id
        );
    }
}
