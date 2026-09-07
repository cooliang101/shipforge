//! User-bound encrypted SSH passwords. No plaintext is serializable.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_PROTECTED_BYTES: usize = 16 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<u8>", into = "Vec<u8>")]
pub struct ProtectedPassword(Vec<u8>);

impl fmt::Debug for ProtectedPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtectedPassword([REDACTED])")
    }
}

impl TryFrom<Vec<u8>> for ProtectedPassword {
    type Error = &'static str;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > MAX_PROTECTED_BYTES {
            return Err("Invalid protected SSH password size");
        }
        Ok(Self(value))
    }
}

impl From<ProtectedPassword> for Vec<u8> {
    fn from(value: ProtectedPassword) -> Self {
        value.0
    }
}

impl ProtectedPassword {
    /// Protects a password for this Windows user before it enters any registry.
    ///
    /// # Errors
    /// Rejects empty/oversized passwords, unsupported platforms or DPAPI failure.
    pub fn protect(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() || value.len() > MAX_PASSWORD_BYTES || value.contains('\0') {
            return Err("Enter an SSH password of 1 to 1024 UTF-8 bytes without NUL");
        }
        #[cfg(windows)]
        {
            windows_dpapi::encrypt_data(value.as_bytes(), windows_dpapi::Scope::User, None)
                .map_err(|_| "Windows could not protect the SSH password")?
                .try_into()
        }
        #[cfg(not(windows))]
        Err("Saved SSH passwords require the Windows client")
    }

    /// Opens a password only at the authenticated transport boundary.
    ///
    /// # Errors
    /// Returns a fixed error for unavailable, corrupt or foreign-user ciphertext.
    pub(crate) fn unlock(&self) -> Result<Zeroizing<String>, &'static str> {
        #[cfg(windows)]
        {
            let bytes = Zeroizing::new(
                windows_dpapi::decrypt_data(&self.0, windows_dpapi::Scope::User, None)
                    .map_err(|_| "Saved SSH password cannot be opened by this Windows user; edit the connection and enter it again")?,
            );
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| "Saved SSH password is invalid; enter it again")?;
            if text.is_empty() || text.len() > MAX_PASSWORD_BYTES || text.contains('\0') {
                return Err("Saved SSH password is invalid; enter it again");
            }
            Ok(Zeroizing::new(text.to_owned()))
        }
        #[cfg(not(windows))]
        Err("Saved SSH passwords require the Windows client")
    }
}

/// Bounded, masked form state, erased on drop and deliberately not serializable.
#[derive(Clone, Default)]
pub(crate) struct PasswordInput(Zeroizing<String>);

impl fmt::Debug for PasswordInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordInput([REDACTED])")
    }
}

impl PasswordInput {
    pub(crate) fn push(&mut self, value: char) {
        if !value.is_control() && self.0.len() + value.len_utf8() <= MAX_PASSWORD_BYTES {
            self.0.push(value);
        }
    }

    pub(crate) fn pop(&mut self) {
        // Replacing the buffer ensures removed bytes are also erased on drop.
        let end = self
            .0
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.0 = Zeroizing::new(self.0[..end].to_owned());
    }

    pub(crate) fn clear(&mut self) {
        self.0 = Zeroizing::new(String::new());
    }

    pub(crate) fn label(&self) -> &'static str {
        if self.0.is_empty() {
            "Password · type password here"
        } else {
            "Password · ******** (hidden)"
        }
    }

    pub(crate) fn masked_value(&self) -> &'static str {
        if self.0.is_empty() {
            "Enter password"
        } else {
            "********"
        }
    }

    pub(crate) fn protect(&self) -> Result<ProtectedPassword, &'static str> {
        ProtectedPassword::protect(&self.0)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip_is_encrypted_and_debug_is_redacted() {
        let password = "test-only 密码 q\"$ spaces";
        let protected = ProtectedPassword::protect(password).unwrap();
        assert!(!format!("{protected:?}").contains(password));
        assert!(
            !protected
                .0
                .windows(password.len())
                .any(|v| v == password.as_bytes())
        );
        let yaml = serde_yaml_ng::to_string(&protected).unwrap();
        assert!(!yaml.contains(password));
        let loaded: ProtectedPassword = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(loaded.unlock().unwrap().as_str(), password);
        let mut corrupt = loaded;
        corrupt.0[0] ^= 255;
        assert!(corrupt.unlock().is_err());
        assert!(ProtectedPassword::protect("").is_err());
        assert!(ProtectedPassword::protect(&"a".repeat(1025)).is_err());
    }

    #[test]
    fn password_input_masks_limits_and_clears_multibyte_characters() {
        let mut input = PasswordInput::default();
        input.push('密');
        input.push('q');
        input.pop();
        assert_eq!(input.protect().unwrap().unlock().unwrap().as_str(), "密");
        assert!(!format!("{input:?}").contains('密'));
        assert!(!input.label().contains('密'));
        input.clear();
        for _ in 0..1025 {
            input.push('x');
        }
        assert_eq!(input.protect().unwrap().unlock().unwrap().len(), 1024);
        input.clear();
        assert!(input.protect().is_err());
    }
}
