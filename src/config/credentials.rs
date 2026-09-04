use std::{
    collections::BTreeMap,
    fmt,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::drivers::CredentialHandle;

const SCHEMA_VERSION: u32 = 1;
const REGISTRY_FILE: &str = "credentials.yaml";

/// Returns the platform-local Credential registry path.
///
/// # Errors
///
/// Returns an error when the user configuration directory is unavailable.
pub fn default_credential_registry_path() -> Result<PathBuf, CredentialRegistryError> {
    crate::adapters::user_config_directory()
        .map(|directory| directory.join(REGISTRY_FILE))
        .map_err(|error| CredentialRegistryError::ConfigurationDirectory(error.to_string()))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SshCredential {
    Agent { fingerprint: String },
    IdentityFile { path: PathBuf },
}

impl fmt::Debug for SshCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent { fingerprint } => formatter
                .debug_struct("Agent")
                .field("fingerprint", fingerprint)
                .finish(),
            Self::IdentityFile { .. } => formatter
                .debug_struct("IdentityFile")
                .field("path", &"[REDACTED]")
                .finish(),
        }
    }
}

impl SshCredential {
    fn validate(&self) -> Result<(), CredentialRegistryError> {
        match self {
            Self::Agent { fingerprint } => {
                if fingerprint.is_empty() || fingerprint.chars().any(char::is_control) {
                    return Err(CredentialRegistryError::Invalid(
                        "SSH Agent fingerprint is empty or contains control characters".into(),
                    ));
                }
            }
            Self::IdentityFile { path } => {
                if !path.is_absolute() {
                    return Err(CredentialRegistryError::Invalid(format!(
                        "IdentityFile path must be absolute: `{}`",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialSummary {
    pub handle: CredentialHandle,
    pub label: String,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRegistry {
    schema_version: u32,
    credentials: BTreeMap<CredentialHandle, SshCredential>,
}

impl CredentialRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            credentials: BTreeMap::new(),
        }
    }

    /// Loads the registry, treating a missing file as empty.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, malformed YAML, unsupported schema, or an
    /// invalid credential record.
    pub fn load(path: &Path) -> Result<Self, CredentialRegistryError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(source) => return Err(CredentialRegistryError::io(path, source)),
        };
        Self::from_yaml(path, &contents)
    }

    pub(crate) fn from_yaml(path: &Path, contents: &str) -> Result<Self, CredentialRegistryError> {
        let registry: Self =
            serde_yaml_ng::from_str(contents).map_err(|source| CredentialRegistryError::Yaml {
                path: path.to_owned(),
                source,
            })?;
        registry.validate()?;
        Ok(registry)
    }

    /// Atomically saves validated credential references.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid data, serialization, directory creation,
    /// or atomic replacement failure.
    pub fn save(&self, path: &Path) -> Result<(), CredentialRegistryError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| CredentialRegistryError::io(parent, source))?;
        }
        let contents =
            serde_yaml_ng::to_string(self).map_err(CredentialRegistryError::Serialize)?;
        let mut file = AtomicWriteFile::open(path)
            .map_err(|source| CredentialRegistryError::io(path, source))?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.commit())
            .map_err(|source| CredentialRegistryError::io(path, source))
    }

    /// Adds a credential under a generated opaque handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the credential fingerprint or path is invalid.
    pub fn create(
        &mut self,
        credential: SshCredential,
    ) -> Result<CredentialHandle, CredentialRegistryError> {
        credential.validate()?;
        let mut handle = CredentialHandle::new();
        while self.credentials.contains_key(&handle) {
            handle = CredentialHandle::new();
        }
        self.credentials.insert(handle.clone(), credential);
        Ok(handle)
    }

    #[must_use]
    pub fn resolve(&self, handle: &CredentialHandle) -> Option<&SshCredential> {
        self.credentials.get(handle)
    }

    /// Removes an unreferenced credential.
    ///
    /// # Errors
    ///
    /// Returns an error when the handle is missing or referenced by a
    /// Destination.
    pub fn remove(
        &mut self,
        handle: &CredentialHandle,
        references: CredentialReferences,
    ) -> Result<SshCredential, CredentialRegistryError> {
        if references.destinations != 0 {
            return Err(CredentialRegistryError::Referenced {
                handle: handle.clone(),
                references,
            });
        }
        self.credentials
            .remove(handle)
            .ok_or_else(|| CredentialRegistryError::Missing(handle.clone()))
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<CredentialSummary> {
        self.credentials
            .iter()
            .map(|(handle, credential)| match credential {
                SshCredential::Agent { fingerprint } => CredentialSummary {
                    handle: handle.clone(),
                    label: format!("SSH Agent · {fingerprint}"),
                    available: true,
                },
                SshCredential::IdentityFile { path } => CredentialSummary {
                    handle: handle.clone(),
                    label: format!(
                        "IdentityFile · {}",
                        path.file_name().map_or_else(
                            || "key".to_owned(),
                            |name| name.to_string_lossy().into_owned()
                        )
                    ),
                    available: path.is_file(),
                },
            })
            .collect()
    }

    fn validate(&self) -> Result<(), CredentialRegistryError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(CredentialRegistryError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        for credential in self.credentials.values() {
            credential.validate()?;
        }
        Ok(())
    }
}

impl Default for CredentialRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CredentialReferences {
    pub destinations: usize,
}

#[derive(Debug, Error)]
pub enum CredentialRegistryError {
    #[error("cannot resolve the Credential registry location: {0}")]
    ConfigurationDirectory(String),
    #[error("invalid Credential registry: {0}")]
    Invalid(String),
    #[error("unsupported Credential registry schemaVersion {0}")]
    UnsupportedSchema(u32),
    #[error("Credential {0:?} does not exist")]
    Missing(CredentialHandle),
    #[error("Credential {handle:?} is referenced by {references:?}")]
    Referenced {
        handle: CredentialHandle,
        references: CredentialReferences,
    },
    #[error("failed to access `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid Credential registry YAML in `{path}`: {source}")]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error("failed to serialize Credential registry: {0}")]
    Serialize(serde_yaml_ng::Error),
}

impl CredentialRegistryError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn generated_handles_round_trip_without_exposing_identity_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("credentials.yaml");
        let identity = directory.path().join("id_ed25519");
        std::fs::write(&identity, "not-a-real-key").unwrap();
        let mut registry = CredentialRegistry::new();
        let handle = registry
            .create(SshCredential::IdentityFile {
                path: identity.clone(),
            })
            .unwrap();
        registry.save(&path).unwrap();

        let loaded = CredentialRegistry::load(&path).unwrap();
        assert_eq!(loaded.resolve(&handle), registry.resolve(&handle));
        assert!(handle.expose_reference().starts_with("cred_"));
        assert!(!format!("{loaded:?}").contains(&identity.display().to_string()));
        assert!(loaded.summaries()[0].available);
    }

    #[test]
    fn rejects_relative_identity_file_and_referenced_removal() {
        let mut registry = CredentialRegistry::new();
        assert!(
            registry
                .create(SshCredential::IdentityFile {
                    path: PathBuf::from(".ssh/id_ed25519")
                })
                .is_err()
        );
        let handle = registry
            .create(SshCredential::Agent {
                fingerprint: "SHA256:test".into(),
            })
            .unwrap();
        assert!(matches!(
            registry.remove(&handle, CredentialReferences { destinations: 1 }),
            Err(CredentialRegistryError::Referenced { .. })
        ));
    }
}
