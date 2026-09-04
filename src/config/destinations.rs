use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    domain::{DestinationKey, DestinationRevision},
    drivers::{CredentialHandle, DriverDestinationInput, DriverKind, EndpointFingerprint},
};

const DESTINATION_REGISTRY_FILE: &str = "destinations.yaml";

/// Returns the platform-local Destination registry path.
///
/// # Errors
///
/// Returns an error when the user configuration directory is unavailable.
pub fn default_destination_registry_path() -> Result<PathBuf, DestinationRegistryError> {
    crate::adapters::user_config_directory()
        .map(|directory| directory.join(DESTINATION_REGISTRY_FILE))
        .map_err(|error| DestinationRegistryError::ConfigurationDirectory(error.to_string()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostKeyFingerprint(String);

impl HostKeyFingerprint {
    /// Parses the verified SSH Host Key fingerprint shown to the operator.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty value or control characters.
    pub fn parse(value: impl Into<String>) -> Result<Self, DestinationRegistryError> {
        let value = value.into();
        if value.is_empty() || value.chars().any(char::is_control) {
            Err(DestinationRegistryError::Invalid(
                "SSH Host Key fingerprint is empty or contains control characters".into(),
            ))
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostKeyFingerprint {
    type Error = DestinationRegistryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<HostKeyFingerprint> for String {
    fn from(value: HostKeyFingerprint) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "kebab-case")]
pub enum DestinationSettings {
    LinuxSsh {
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        user: String,
        credential: CredentialHandle,
        host_key: HostKeyFingerprint,
    },
}

const fn default_ssh_port() -> u16 {
    22
}

impl DestinationSettings {
    /// Non-secret display label only; not an endpoint identity or connection URI.
    #[must_use]
    pub fn endpoint_label(&self) -> String {
        match self {
            Self::LinuxSsh {
                host, port, user, ..
            } => {
                if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
                    format!("{user}@[{host}]:{port}")
                } else {
                    format!("{user}@{host}:{port}")
                }
            }
        }
    }

    /// Validates Driver-specific connection fields.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or control-character-containing SSH fields or
    /// for port zero.
    pub fn validate(&self) -> Result<(), DestinationRegistryError> {
        match self {
            Self::LinuxSsh {
                host, port, user, ..
            } => {
                if *port == 0
                    || host.is_empty()
                    || user.is_empty()
                    || host
                        .chars()
                        .chain(user.chars())
                        .any(|character| character.is_control() || character.is_whitespace())
                {
                    return Err(DestinationRegistryError::Invalid(
                        "linux-ssh host/user must be non-empty and port must be non-zero".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn driver_kind(&self) -> DriverKind {
        match self {
            Self::LinuxSsh { .. } => DriverKind::linux_ssh(),
        }
    }

    #[must_use]
    pub fn endpoint_fingerprint(&self) -> EndpointFingerprint {
        let mut hash = Sha256::new();
        match self {
            Self::LinuxSsh {
                host,
                port,
                user,
                host_key,
                ..
            } => {
                hash.update(b"linux-ssh\0");
                hash.update(host.as_bytes());
                hash.update(b"\0");
                hash.update(port.to_be_bytes());
                hash.update(b"\0");
                hash.update(user.as_bytes());
                hash.update(b"\0");
                hash.update(host_key.as_str().as_bytes());
            }
        }
        EndpointFingerprint::from_sha256_hex(format!("{:x}", hash.finalize()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationRevisionRecord {
    pub revision: DestinationRevision,
    pub endpoint_fingerprint: EndpointFingerprint,
    pub settings: DestinationSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationSummary {
    pub key: DestinationKey,
    pub revision: DestinationRevision,
    pub driver: DriverKind,
    pub endpoint: String,
}

#[derive(Clone, Debug)]
pub struct ResolvedDestination {
    pub driver: DriverKind,
    pub credential: CredentialHandle,
    pub endpoint_fingerprint: EndpointFingerprint,
    pub settings: DriverDestinationInput,
}

impl DestinationRevisionRecord {
    /// Separates the credential reference from non-secret Driver connection
    /// settings before constructing an execution context.
    #[must_use]
    pub fn resolve(&self) -> ResolvedDestination {
        match &self.settings {
            DestinationSettings::LinuxSsh {
                host,
                port,
                user,
                credential,
                host_key,
            } => ResolvedDestination {
                driver: DriverKind::linux_ssh(),
                credential: credential.clone(),
                endpoint_fingerprint: self.endpoint_fingerprint.clone(),
                settings: DriverDestinationInput {
                    value: serde_json::json!({
                        "host": host,
                        "port": port,
                        "user": user,
                        "hostKey": host_key.as_str(),
                    }),
                },
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationEntry {
    revisions: Vec<DestinationRevisionRecord>,
}

impl DestinationEntry {
    #[must_use]
    pub fn current(&self) -> Option<&DestinationRevisionRecord> {
        self.revisions.last()
    }

    #[must_use]
    pub fn revision(&self, revision: DestinationRevision) -> Option<&DestinationRevisionRecord> {
        self.revisions
            .iter()
            .find(|record| record.revision == revision)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationRegistry {
    #[serde(default = "registry_schema_version")]
    schema_version: u32,
    #[serde(default)]
    destinations: BTreeMap<DestinationKey, DestinationEntry>,
}

const fn registry_schema_version() -> u32 {
    1
}

impl DestinationRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: registry_schema_version(),
            destinations: BTreeMap::new(),
        }
    }

    /// Loads a user-level registry. A missing file yields an empty registry.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, YAML, schema, revision, or connection-field
    /// failures.
    pub fn load(path: &Path) -> Result<Self, DestinationRegistryError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(source) => {
                return Err(DestinationRegistryError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Self::from_yaml(path, &contents)
    }

    pub(crate) fn from_yaml(path: &Path, contents: &str) -> Result<Self, DestinationRegistryError> {
        let registry: Self =
            serde_yaml_ng::from_str(contents).map_err(|source| DestinationRegistryError::Yaml {
                path: path.to_owned(),
                source,
            })?;
        registry.validate()?;
        Ok(registry)
    }

    /// Atomically saves the user-level registry.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the atomic write fails.
    pub fn save(&self, path: &Path) -> Result<(), DestinationRegistryError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| DestinationRegistryError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let contents =
            serde_yaml_ng::to_string(self).map_err(|source| DestinationRegistryError::Yaml {
                path: path.to_owned(),
                source,
            })?;
        let mut file =
            AtomicWriteFile::open(path).map_err(|source| DestinationRegistryError::Io {
                path: path.to_owned(),
                source,
            })?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.commit())
            .map_err(|source| DestinationRegistryError::Io {
                path: path.to_owned(),
                source,
            })
    }

    /// Creates a Destination at revision 1.
    ///
    /// # Errors
    ///
    /// Returns an error if the key exists or settings are invalid.
    pub fn create(
        &mut self,
        key: DestinationKey,
        settings: DestinationSettings,
    ) -> Result<&DestinationRevisionRecord, DestinationRegistryError> {
        if self.destinations.contains_key(&key) {
            return Err(DestinationRegistryError::Duplicate(key));
        }
        settings.validate()?;
        let record = DestinationRevisionRecord {
            revision: DestinationRevision::INITIAL,
            endpoint_fingerprint: settings.endpoint_fingerprint(),
            settings,
        };
        self.destinations.insert(
            key.clone(),
            DestinationEntry {
                revisions: vec![record],
            },
        );
        self.destinations[&key].current().ok_or_else(|| {
            DestinationRegistryError::Invalid("new Destination has no revision".into())
        })
    }

    /// Appends a new immutable revision to an existing Destination.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is missing, settings are invalid, or the
    /// revision counter is exhausted.
    pub fn revise(
        &mut self,
        key: &DestinationKey,
        settings: DestinationSettings,
    ) -> Result<&DestinationRevisionRecord, DestinationRegistryError> {
        settings.validate()?;
        let entry = self
            .destinations
            .get_mut(key)
            .ok_or_else(|| DestinationRegistryError::Missing(key.clone()))?;
        let revision = entry
            .current()
            .ok_or_else(|| {
                DestinationRegistryError::Invalid(format!("Destination `{key}` has no revisions"))
            })?
            .revision
            .checked_next()
            .ok_or(DestinationRegistryError::RevisionOverflow)?;
        entry.revisions.push(DestinationRevisionRecord {
            revision,
            endpoint_fingerprint: settings.endpoint_fingerprint(),
            settings,
        });
        entry.current().ok_or_else(|| {
            DestinationRegistryError::Invalid(format!("Destination `{key}` has no revisions"))
        })
    }

    #[must_use]
    pub fn resolve(&self, key: &DestinationKey) -> Option<&DestinationRevisionRecord> {
        self.destinations
            .get(key)
            .and_then(DestinationEntry::current)
    }

    /// Resolves an exact immutable historical revision, never the latest fallback.
    #[must_use]
    pub fn resolve_revision(
        &self,
        key: &DestinationKey,
        revision: DestinationRevision,
    ) -> Option<&DestinationRevisionRecord> {
        self.destinations.get(key)?.revision(revision)
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<DestinationSummary> {
        self.destinations
            .iter()
            .filter_map(|(key, entry)| {
                entry.current().map(|record| {
                    let endpoint = record.settings.endpoint_label();
                    DestinationSummary {
                        key: key.clone(),
                        revision: record.revision,
                        driver: record.settings.driver_kind(),
                        endpoint,
                    }
                })
            })
            .collect()
    }

    /// Removes an unreferenced Destination key and all its revisions.
    ///
    /// # Errors
    ///
    /// Returns an error if the key is missing or any Project, Deployment, or
    /// Release still references it.
    pub fn remove(
        &mut self,
        key: &DestinationKey,
        references: DestinationReferences,
    ) -> Result<DestinationEntry, DestinationRegistryError> {
        if !references.is_empty() {
            return Err(DestinationRegistryError::Referenced {
                key: key.clone(),
                references,
            });
        }
        self.destinations
            .remove(key)
            .ok_or_else(|| DestinationRegistryError::Missing(key.clone()))
    }

    fn validate(&self) -> Result<(), DestinationRegistryError> {
        if self.schema_version != registry_schema_version() {
            return Err(DestinationRegistryError::Invalid(format!(
                "unsupported registry schemaVersion {}",
                self.schema_version
            )));
        }
        for (key, entry) in &self.destinations {
            if entry.revisions.is_empty() {
                return Err(DestinationRegistryError::Invalid(format!(
                    "Destination `{key}` has no revisions"
                )));
            }
            let mut expected = DestinationRevision::INITIAL;
            for record in &entry.revisions {
                record.settings.validate()?;
                if record.revision != expected
                    || record.endpoint_fingerprint != record.settings.endpoint_fingerprint()
                {
                    return Err(DestinationRegistryError::Invalid(format!(
                        "Destination `{key}` has inconsistent revision metadata"
                    )));
                }
                expected = expected
                    .checked_next()
                    .ok_or(DestinationRegistryError::RevisionOverflow)?;
            }
        }
        Ok(())
    }
}

impl Default for DestinationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DestinationReferences {
    pub registered_projects: usize,
    pub deployments: usize,
    pub releases: usize,
}

impl DestinationReferences {
    const fn is_empty(self) -> bool {
        self.registered_projects == 0 && self.deployments == 0 && self.releases == 0
    }
}

#[derive(Debug, Error)]
pub enum DestinationRegistryError {
    #[error("cannot resolve the Destination registry location: {0}")]
    ConfigurationDirectory(String),
    #[error("Destination `{0}` already exists; immutable keys cannot be renamed")]
    Duplicate(DestinationKey),
    #[error("Destination `{0}` does not exist")]
    Missing(DestinationKey),
    #[error("Destination revision counter is exhausted")]
    RevisionOverflow,
    #[error("Destination `{key}` is still referenced: {references:?}")]
    Referenced {
        key: DestinationKey,
        references: DestinationReferences,
    },
    #[error("invalid Destination registry: {0}")]
    Invalid(String),
    #[error("failed to access {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("invalid Destination registry YAML in {path}: {source}")]
    Yaml {
        path: std::path::PathBuf,
        source: serde_yaml_ng::Error,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshCandidate {
    pub host: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalSshDiscovery {
    pub connections: Vec<SshCandidate>,
    pub identity_files: Vec<PathBuf>,
}

/// Discovers simple SSH connection and modern identity-file candidates.
///
/// A missing `~/.ssh/config` is treated as an empty configuration. This
/// function never opens a private-key file; it only checks whether candidate
/// paths refer to regular files.
///
/// # Errors
///
/// Returns an error when an existing SSH configuration cannot be read.
pub fn discover_local_ssh(home: &Path) -> Result<LocalSshDiscovery, DestinationRegistryError> {
    if !home.is_absolute() {
        return Err(DestinationRegistryError::Invalid(
            "SSH discovery home directory must be absolute".into(),
        ));
    }
    let config_path = home.join(".ssh").join("config");
    let contents = match std::fs::read_to_string(&config_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(DestinationRegistryError::Io {
                path: config_path,
                source,
            });
        }
    };
    let connections = discover_ssh_candidates(&contents);
    let mut identity_files = connections
        .iter()
        .flat_map(|candidate| &candidate.identity_files)
        .filter_map(|path| expand_home_path(home, path))
        .chain(["id_ed25519", "id_ecdsa"].map(|name| home.join(".ssh").join(name)))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    identity_files.sort();
    identity_files.dedup();
    Ok(LocalSshDiscovery {
        connections,
        identity_files,
    })
}

fn expand_home_path(home: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_owned());
    }
    let text = path.to_str()?;
    if text == "~" {
        Some(home.to_owned())
    } else {
        text.strip_prefix("~/")
            .or_else(|| text.strip_prefix("~\\"))
            .map(|relative| home.join(relative))
    }
}

/// Extracts simple, concrete Host candidates from OpenSSH configuration text.
///
/// The MVP deliberately treats SSH config as a convenience source rather than
/// a connection protocol. Wildcards, negation, `Include`, `Match`, and jump-host
/// semantics are not evaluated; the TUI asks for any value it cannot discover.
#[must_use]
pub fn discover_ssh_candidates(contents: &str) -> Vec<SshCandidate> {
    let mut candidates = Vec::<SshCandidate>::new();
    let mut active = Vec::<usize>::new();

    for raw_line in contents.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let Some((keyword, value)) = split_option(line) else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("host") {
            active.clear();
            for host in value.split_whitespace().filter(|host| concrete_host(host)) {
                active.push(candidates.len());
                candidates.push(SshCandidate {
                    host: host.into(),
                    hostname: None,
                    user: None,
                    port: None,
                    identity_files: Vec::new(),
                });
            }
            continue;
        }
        for index in &active {
            let candidate = &mut candidates[*index];
            if keyword.eq_ignore_ascii_case("hostname") && candidate.hostname.is_none() {
                candidate.hostname = Some(unquote(value).into());
            } else if keyword.eq_ignore_ascii_case("user") && candidate.user.is_none() {
                candidate.user = Some(unquote(value).into());
            } else if keyword.eq_ignore_ascii_case("port") && candidate.port.is_none() {
                candidate.port = value.parse().ok().filter(|port| *port != 0);
            } else if keyword.eq_ignore_ascii_case("identityfile") {
                candidate.identity_files.push(PathBuf::from(unquote(value)));
            }
        }
    }
    candidates.sort_by(|left, right| left.host.cmp(&right.host));
    candidates.dedup_by(|left, right| left.host == right.host);
    candidates
}

fn concrete_host(host: &str) -> bool {
    !host.starts_with('!') && !host.contains('*') && !host.contains('?') && !host.contains('[')
}

fn split_option(line: &str) -> Option<(&str, &str)> {
    line.find(char::is_whitespace)
        .map(|index| (&line[..index], line[index..].trim()))
        .or_else(|| {
            line.split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
        })
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

/// Reports whether the current process can see an SSH Agent endpoint.
#[must_use]
pub fn ssh_agent_available() -> bool {
    std::env::var_os("SSH_AUTH_SOCK").is_some() || std::env::var_os("SSH_AGENT_PIPE").is_some()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn settings(host: &str) -> DestinationSettings {
        DestinationSettings::LinuxSsh {
            host: host.into(),
            port: 22,
            user: "deploy".into(),
            credential: CredentialHandle::new(),
            host_key: HostKeyFingerprint::parse("SHA256:test-host-key").unwrap(),
        }
    }

    #[test]
    fn create_and_revise_preserve_immutable_history() {
        let key = DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
        let mut registry = DestinationRegistry::new();
        let first = registry
            .create(key.clone(), settings("one.example.com"))
            .unwrap();
        let first_fingerprint = first.endpoint_fingerprint.clone();
        let second = registry.revise(&key, settings("two.example.com")).unwrap();
        assert_eq!(second.revision.get(), 2);
        assert_ne!(second.endpoint_fingerprint, first_fingerprint);
        assert_eq!(
            registry
                .resolve_revision(&key, DestinationRevision::INITIAL)
                .unwrap()
                .endpoint_fingerprint,
            first_fingerprint
        );
        assert!(
            registry
                .resolve_revision(&DestinationKey::new(), DestinationRevision::INITIAL)
                .is_none()
        );
        assert!(
            registry
                .resolve_revision(
                    &key,
                    DestinationRevision::INITIAL
                        .checked_next()
                        .unwrap()
                        .checked_next()
                        .unwrap()
                )
                .is_none()
        );
    }

    #[test]
    fn registry_round_trips_atomically() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("destinations.yaml");
        let key = DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
        let mut registry = DestinationRegistry::new();
        registry
            .create(key.clone(), settings("app.example.com"))
            .unwrap();
        registry.save(&path).unwrap();
        let loaded = DestinationRegistry::load(&path).unwrap();
        assert_eq!(loaded, registry);
        assert_eq!(loaded.resolve(&key).unwrap().revision.get(), 1);
    }

    #[test]
    fn referenced_destination_cannot_be_removed() {
        let key = DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
        let mut registry = DestinationRegistry::new();
        registry
            .create(key.clone(), settings("app.example.com"))
            .unwrap();
        assert!(matches!(
            registry.remove(
                &key,
                DestinationReferences {
                    registered_projects: 1,
                    deployments: 0,
                    releases: 0,
                }
            ),
            Err(DestinationRegistryError::Referenced { .. })
        ));
        assert!(registry.resolve(&key).is_some());
    }

    #[test]
    fn debug_output_redacts_credential_handle() {
        let debug = format!("{:?}", settings("app.example.com"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("cred_"));
    }

    #[test]
    fn ssh_config_discovery_returns_only_concrete_selectable_hosts() {
        let candidates = discover_ssh_candidates(
            r#"
Host *
  User fallback
Host app-prod worker-prod
  HostName prod.example.com
  User deploy
  Port 2222
  IdentityFile "~/.ssh/prod key"
Host !blocked *.internal
  User ignored
"#,
        );
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].host, "app-prod");
        assert_eq!(candidates[0].hostname.as_deref(), Some("prod.example.com"));
        assert_eq!(candidates[0].user.as_deref(), Some("deploy"));
        assert_eq!(candidates[0].port, Some(2222));
        assert_eq!(
            candidates[0].identity_files,
            [PathBuf::from("~/.ssh/prod key")]
        );
        assert_eq!(candidates[1].host, "worker-prod");
        assert_eq!(candidates[1].user.as_deref(), Some("deploy"));
    }

    #[test]
    fn local_ssh_discovery_expands_existing_keys_without_reading_them() {
        let directory = tempdir().unwrap();
        let ssh = directory.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("id_ed25519"), "private material is never parsed").unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host production\n  HostName example.com\n  IdentityFile ~/.ssh/id_ed25519\n",
        )
        .unwrap();

        let discovery = discover_local_ssh(directory.path()).unwrap();
        assert_eq!(discovery.connections.len(), 1);
        assert_eq!(discovery.connections[0].host, "production");
        assert_eq!(discovery.identity_files, [ssh.join("id_ed25519")]);
    }

    #[test]
    fn local_ssh_discovery_rejects_relative_home() {
        assert!(matches!(
            discover_local_ssh(Path::new("relative")),
            Err(DestinationRegistryError::Invalid(_))
        ));
    }

    #[test]
    fn serialized_host_key_cannot_bypass_validation() {
        assert!(serde_json::from_str::<HostKeyFingerprint>(r#"""#).is_err());
        assert!(serde_json::from_str::<HostKeyFingerprint>("\"line\\nbreak\"").is_err());
    }

    #[test]
    fn ssh_destination_rejects_whitespace_in_host_or_user() {
        for invalid in [
            DestinationSettings::LinuxSsh {
                host: "bad host".into(),
                port: 22,
                user: "deploy".into(),
                credential: CredentialHandle::new(),
                host_key: HostKeyFingerprint::parse("SHA256:test").unwrap(),
            },
            DestinationSettings::LinuxSsh {
                host: "example.com".into(),
                port: 22,
                user: "bad user".into(),
                credential: CredentialHandle::new(),
                host_key: HostKeyFingerprint::parse("SHA256:test").unwrap(),
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn resolved_driver_settings_exclude_credential_data() {
        let key = DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
        let mut registry = DestinationRegistry::new();
        let revision = registry.create(key, settings("app.example.com")).unwrap();
        let resolved = revision.resolve();
        assert_eq!(resolved.driver.as_str(), DriverKind::LINUX_SSH);
        assert!(resolved.credential.expose_reference().starts_with("cred_"));
        let serialized = resolved.settings.value.to_string();
        assert!(!serialized.contains("credential"));
        assert!(!serialized.contains(resolved.credential.expose_reference()));
    }

    #[test]
    fn summaries_expose_selection_details_without_credentials() {
        let key = DestinationKey::parse("dst_00000000000000000000000000000001").unwrap();
        let mut registry = DestinationRegistry::new();
        registry
            .create(key.clone(), settings("app.example.com"))
            .unwrap();
        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].key, key);
        assert_eq!(summaries[0].endpoint, "deploy@app.example.com:22");
        assert!(!format!("{summaries:?}").contains("cred_"));
    }

    #[test]
    fn endpoint_display_brackets_ipv6_without_changing_identity_or_settings() {
        for (host, expected) in [
            ("example.com", "deploy@example.com:22"),
            ("127.0.0.1", "deploy@127.0.0.1:22"),
            ("2001:db8::1", "deploy@[2001:db8::1]:22"),
            ("[2001:db8::1]", "deploy@[2001:db8::1]:22"),
        ] {
            let settings = settings(host);
            let before = serde_json::to_value(&settings).unwrap();
            let identity = settings.endpoint_fingerprint();
            assert_eq!(settings.endpoint_label(), expected);
            assert_eq!(settings.endpoint_fingerprint(), identity);
            assert_eq!(serde_json::to_value(&settings).unwrap(), before);
            let mut registry = DestinationRegistry::new();
            registry.create(DestinationKey::new(), settings).unwrap();
            assert_eq!(registry.summaries()[0].endpoint, expected);
        }
    }
}
