use std::path::PathBuf;

use thiserror::Error;

/// Returns the current user's absolute home directory.
///
/// # Errors
///
/// Returns an error when the platform home directory is unavailable or not
/// absolute.
pub fn user_home_directory() -> Result<PathBuf, PlatformPathError> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);

    #[cfg(unix)]
    let home = std::env::var_os("HOME").map(PathBuf::from);

    #[cfg(not(any(unix, windows)))]
    let home: Option<PathBuf> = None;

    home.filter(|path| path.is_absolute())
        .ok_or(PlatformPathError::HomeUnavailable)
}

/// Returns `ShipForge`'s platform-local user configuration directory.
///
/// # Errors
///
/// Returns an error when the platform base directory is unavailable or not
/// absolute.
pub fn user_config_directory() -> Result<PathBuf, PlatformPathError> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));

    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
        });

    #[cfg(not(any(unix, windows)))]
    let base: Option<PathBuf> = None;

    let base = base
        .filter(|path| path.is_absolute())
        .ok_or(PlatformPathError::Unavailable)?;
    let application_directory = if cfg!(target_os = "linux") {
        "shipforge"
    } else {
        "ShipForge"
    };
    Ok(base.join(application_directory))
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PlatformPathError {
    #[error("the platform user configuration directory is unavailable or not absolute")]
    Unavailable,
    #[error("the platform user home directory is unavailable or not absolute")]
    HomeUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_user_directory_is_absolute() {
        if let Ok(path) = user_config_directory() {
            assert!(path.is_absolute());
            assert!(matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("ShipForge" | "shipforge")
            ));
        }
    }

    #[test]
    fn configured_home_directory_is_absolute() {
        if let Ok(path) = user_home_directory() {
            assert!(path.is_absolute());
        }
    }
}
