use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use super::{ConnectionManagementError, ManagementSource, unavailable};

const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct FileSnapshot {
    path: PathBuf,
    source: ManagementSource,
    bytes: Option<Vec<u8>>,
}

impl std::fmt::Debug for FileSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileSnapshot")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl FileSnapshot {
    pub(super) fn read(
        path: &Path,
        source: ManagementSource,
    ) -> Result<Self, ConnectionManagementError> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(unavailable(source, "could not inspect file")),
        };
        let bytes = if let Some(metadata) = metadata {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(unavailable(
                    source,
                    "expected a regular file, not a link or directory",
                ));
            }
            if metadata.len() > MAX_FILE_BYTES {
                return Err(unavailable(source, "file exceeds bounded size limit"));
            }
            let mut bytes = Vec::new();
            File::open(path)
                .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
                .map_err(|_| unavailable(source.clone(), "could not read file"))?;
            if bytes.len() > usize::try_from(MAX_FILE_BYTES).expect("bounded file size fits usize")
            {
                return Err(unavailable(source, "file exceeds bounded size limit"));
            }
            Some(bytes)
        } else {
            None
        };
        Ok(Self {
            path: path.to_owned(),
            source,
            bytes,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn text(&self) -> Result<Option<&str>, ConnectionManagementError> {
        self.bytes
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| unavailable(self.source.clone(), "file is not valid UTF-8"))
    }

    pub(super) fn required_text(&self) -> Result<&str, ConnectionManagementError> {
        self.text()?.ok_or_else(|| {
            unavailable(
                self.source.clone(),
                "file is missing; references are unknown",
            )
        })
    }

    pub(super) fn ensure_unchanged(&self) -> Result<(), ConnectionManagementError> {
        if Self::read(&self.path, self.source.clone())? == *self {
            Ok(())
        } else {
            Err(ConnectionManagementError::Stale)
        }
    }
}
