//! A failed atomic replacement does not prove that the new bytes are absent.

use std::path::Path;

use crate::config::{CredentialRegistry, DestinationRegistry};

pub(super) struct Registration<'a> {
    pub credential_path: &'a Path,
    pub destination_path: &'a Path,
    pub original_credentials: &'a CredentialRegistry,
    pub credentials: &'a CredentialRegistry,
    pub original_destinations: &'a DestinationRegistry,
    pub destinations: &'a DestinationRegistry,
    pub created_credential: bool,
}

trait RegistryStorage {
    fn credentials(&self, path: &Path) -> Result<CredentialRegistry, ()>;
    fn destinations(&self, path: &Path) -> Result<DestinationRegistry, ()>;
    fn save_credentials(&mut self, path: &Path, registry: &CredentialRegistry) -> Result<(), ()>;
    fn save_destinations(&mut self, path: &Path, registry: &DestinationRegistry) -> Result<(), ()>;
}

struct LocalStorage;

impl RegistryStorage for LocalStorage {
    fn credentials(&self, path: &Path) -> Result<CredentialRegistry, ()> {
        CredentialRegistry::load(path).map_err(|_| ())
    }

    fn destinations(&self, path: &Path) -> Result<DestinationRegistry, ()> {
        DestinationRegistry::load(path).map_err(|_| ())
    }

    fn save_credentials(&mut self, path: &Path, registry: &CredentialRegistry) -> Result<(), ()> {
        registry.save(path).map_err(|_| ())
    }

    fn save_destinations(&mut self, path: &Path, registry: &DestinationRegistry) -> Result<(), ()> {
        registry.save(path).map_err(|_| ())
    }
}

impl Registration<'_> {
    pub(super) fn persist(&self) -> Result<(), String> {
        self.persist_with(&mut LocalStorage)
    }

    fn persist_with(&self, storage: &mut impl RegistryStorage) -> Result<(), String> {
        if self.created_credential
            && storage
                .save_credentials(self.credential_path, self.credentials)
                .is_err()
        {
            let observation = self.identity_observation(storage);
            return Err(format!(
                "SSH identity save returned an error; its durable result is unconfirmed. {observation} Connection registration was not attempted. Reload Connections and SSH identities before retrying."
            ));
        }
        if storage
            .save_destinations(self.destination_path, self.destinations)
            .is_ok()
        {
            return Ok(());
        }
        let observation = match storage.destinations(self.destination_path) {
            Ok(observed) if observed == *self.destinations => {
                "The new connection is visible on re-read. Its SSH identity was retained; no identity restoration was attempted.".into()
            }
            Ok(observed) if observed == *self.original_destinations => {
                let restoration = self.restore_unreferenced_identity(storage);
                format!("The connection registry is unchanged on re-read. {restoration}")
            }
            Ok(_) => "The connection registry differs from both expected snapshots. SSH identities were retained; no restoration was attempted.".into(),
            Err(()) => "The connection registry could not be re-read. SSH identities were retained; no restoration was attempted.".into(),
        };
        Err(format!(
            "Connection save returned an error; its durable result is unconfirmed. {observation} Reload Connections and SSH identities before retrying; do not assume the connection is absent."
        ))
    }

    fn restore_unreferenced_identity(&self, storage: &mut impl RegistryStorage) -> String {
        if !self.created_credential {
            return "The existing SSH identity registry was not modified.".into();
        }
        // Only restore after both local snapshots still match this attempt.
        // Unknown/new connection state must never cause its identity to vanish.
        if storage.credentials(self.credential_path).ok().as_ref() != Some(self.credentials) {
            return "The identity registry changed or could not be re-read; restoration was not attempted.".into();
        }
        if storage
            .save_credentials(self.credential_path, self.original_credentials)
            .is_ok()
        {
            return "Restoring the original SSH identity registry was acknowledged successfully."
                .into();
        }
        let observation = self.identity_observation(storage);
        format!(
            "Restoring the original SSH identity registry also returned an error; its durable result is unconfirmed. {observation}"
        )
    }

    fn identity_observation(&self, storage: &impl RegistryStorage) -> &'static str {
        match storage.credentials(self.credential_path) {
            Ok(observed) if observed == *self.original_credentials => {
                "The original identity registry is visible on re-read."
            }
            Ok(observed) if observed == *self.credentials => {
                "The new identity is visible on re-read."
            }
            Ok(_) => "The identity registry differs from both expected snapshots.",
            Err(()) => "The identity registry could not be re-read.",
        }
    }
}

#[cfg(test)]
mod tests;
