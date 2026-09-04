use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{domain::DeploymentId, telemetry::Redactor};

#[derive(Debug)]
pub struct RollingLogWriter {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
    retained_files: usize,
}

impl RollingLogWriter {
    /// Opens a bounded log named from the generated Deployment ID.
    ///
    /// # Errors
    ///
    /// Returns an error for zero limits, directory creation, metadata, or file open failures.
    pub fn open(
        directory: &Path,
        deployment: &DeploymentId,
        max_bytes: u64,
        retained_files: usize,
    ) -> Result<Self, RollingLogError> {
        if max_bytes == 0 || retained_files == 0 {
            return Err(RollingLogError::InvalidLimits);
        }
        std::fs::create_dir_all(directory)
            .map_err(|source| RollingLogError::io(directory, source))?;
        let path = directory.join(format!("{deployment}.log"));
        let file = open_append(&path)?;
        let length = file
            .metadata()
            .map_err(|source| RollingLogError::io(&path, source))?
            .len();
        Ok(Self {
            path,
            file: Some(file),
            length,
            max_bytes,
            retained_files,
        })
    }

    /// Redacts and appends one output chunk, rotating exact log files as needed.
    ///
    /// A chunk larger than the configured file size retains its tail, which is
    /// generally the most useful part of build or Driver diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error when rotation or writing fails.
    pub fn append(&mut self, chunk: &str, redactor: &Redactor) -> Result<(), RollingLogError> {
        let redacted = redactor.redact(chunk);
        let bytes = redacted.as_bytes();
        let retained = if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > self.max_bytes {
            let keep = usize::try_from(self.max_bytes)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            &bytes[bytes.len() - keep..]
        } else {
            bytes
        };
        self.append_bytes(retained)
    }

    /// Appends one already-sanitized complete record without splitting or truncating it.
    ///
    /// # Errors
    /// Rejects records larger than a generation and reports rotation or write failures.
    pub fn append_record(&mut self, record: &str) -> Result<(), RollingLogError> {
        if u64::try_from(record.len()).unwrap_or(u64::MAX) > self.max_bytes {
            return Err(RollingLogError::RecordTooLarge);
        }
        self.append_bytes(record.as_bytes())
    }

    fn append_bytes(&mut self, retained: &[u8]) -> Result<(), RollingLogError> {
        let retained_len = u64::try_from(retained.len()).unwrap_or(u64::MAX);
        if self.length > 0 && self.length.saturating_add(retained_len) > self.max_bytes {
            self.rotate()?;
        }
        let file = self.file.as_mut().ok_or(RollingLogError::Closed)?;
        file.write_all(retained)
            .map_err(|source| RollingLogError::io(&self.path, source))?;
        file.flush()
            .map_err(|source| RollingLogError::io(&self.path, source))?;
        self.length = self.length.saturating_add(retained_len);
        Ok(())
    }

    fn rotate(&mut self) -> Result<(), RollingLogError> {
        self.file.take();
        for generation in (1..self.retained_files).rev() {
            let source = generation_path(&self.path, generation);
            if source.exists() {
                let destination = generation_path(&self.path, generation + 1);
                remove_if_exists(&destination)?;
                std::fs::rename(&source, &destination)
                    .map_err(|error| RollingLogError::io(&source, error))?;
            }
        }
        if self.path.exists() {
            let first = generation_path(&self.path, 1);
            remove_if_exists(&first)?;
            std::fs::rename(&self.path, &first)
                .map_err(|error| RollingLogError::io(&self.path, error))?;
        }
        self.file = Some(open_append(&self.path)?);
        self.length = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> Result<File, RollingLogError> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|source| RollingLogError::io(path, source))
}

fn generation_path(path: &Path, generation: usize) -> PathBuf {
    path.with_extension(format!("log.{generation}"))
}

fn remove_if_exists(path: &Path) -> Result<(), RollingLogError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RollingLogError::io(path, source)),
    }
}

#[derive(Debug, Error)]
pub enum RollingLogError {
    #[error("rolling log size and retained-file count must be non-zero")]
    InvalidLimits,
    #[error("complete rolling log record exceeds the generation size limit")]
    RecordTooLarge,
    #[error("rolling log writer is closed")]
    Closed,
    #[error("rolling log I/O failed at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl RollingLogError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_before_writing_and_rotates_within_limits() {
        let directory = tempfile::tempdir().unwrap();
        let deployment = DeploymentId::new();
        let mut writer = RollingLogWriter::open(directory.path(), &deployment, 12, 2).unwrap();
        let redactor = Redactor::new(["TOKEN".into()]);
        writer.append("aTOKENb", &redactor).unwrap();
        writer.append("second-line", &redactor).unwrap();
        writer.append("third", &redactor).unwrap();
        drop(writer);

        let current = directory.path().join(format!("{deployment}.log"));
        let previous = current.with_extension("log.1");
        assert_eq!(std::fs::read_to_string(current).unwrap(), "third");
        assert_eq!(std::fs::read_to_string(previous).unwrap(), "second-line");
        let all = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(!all.contains("TOKEN"));
    }

    #[test]
    fn oversized_chunk_keeps_only_its_tail() {
        let directory = tempfile::tempdir().unwrap();
        let deployment = DeploymentId::new();
        let mut writer = RollingLogWriter::open(directory.path(), &deployment, 4, 1).unwrap();
        writer.append("abcdefgh", &Redactor::default()).unwrap();
        drop(writer);
        let path = directory.path().join(format!("{deployment}.log"));
        assert_eq!(std::fs::read(path).unwrap(), b"efgh");
    }

    #[test]
    fn complete_records_rotate_whole_and_oversized_record_does_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let id = DeploymentId::new();
        let mut writer = RollingLogWriter::open(directory.path(), &id, 12, 2).unwrap();
        writer.append_record("first-line\n").unwrap();
        writer.append_record("second-line\n").unwrap();
        assert!(matches!(
            writer.append_record("too-long-record\n"),
            Err(RollingLogError::RecordTooLarge)
        ));
        drop(writer);
        let path = directory.path().join(format!("{id}.log"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second-line\n");
        assert_eq!(
            std::fs::read_to_string(path.with_extension("log.1")).unwrap(),
            "first-line\n"
        );
    }
}
