//! Confirmed, no-clobber exports of a frozen, already-redacted text payload.
//!
//! This module never reads history or source logs. The caller owns redaction and
//! scope labels; the opaque preview owns the exact bytes that will be published.
//! Directory/name evidence is rechecked, not locked. These checks do not provide
//! atomic isolation from another process replacing paths between syscalls.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use tempfile::NamedTempFile;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub const MAX_EXPORT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_PATH_COMPONENTS: usize = 256;
const PUBLISHED_WARNING: &str = "File was published; its final path or crash durability could not be fully verified. Inspect the selected directory before retrying.";

pub struct LocalExportService;

/// Immutable payload and local directory evidence, created without any writes.
pub struct LocalExportPreview {
    directory: DirectorySnapshot,
    path: PathBuf,
    text: String,
}

impl std::fmt::Debug for LocalExportPreview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalExportPreview")
            .field("byte_len", &self.text.len())
            .finish_non_exhaustive()
    }
}

impl LocalExportPreview {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.text.len()
    }
}

/// A successful publication is never rewritten as cancellation or save failure.
pub struct LocalExportOutcome {
    path: PathBuf,
    warning: Option<&'static str>,
}

impl std::fmt::Debug for LocalExportOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalExportOutcome")
            .field("warning", &self.warning)
            .finish_non_exhaustive()
    }
}

impl LocalExportOutcome {
    /// The intended, user-reviewed path; heed `warning` if final verification failed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn warning(&self) -> Option<&'static str> {
        self.warning
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum LocalExportError {
    #[error("export cancelled before publication")]
    Cancelled,
    #[error("choose an existing absolute directory without symbolic links or reparse points")]
    UnsafeDirectory,
    #[error(
        "the export directory or temporary file changed; review a fresh preview; a private temporary file may remain"
    )]
    Changed,
    #[error(
        "the export filename must be a bounded, portable ASCII name ending in .log, .txt or .json"
    )]
    InvalidName,
    #[error("export text exceeds the size limit or contains unsafe control characters")]
    InvalidText,
    #[error("the export destination already exists; choose a different filename or directory")]
    AlreadyExists,
    #[error("the export directory could not be inspected safely")]
    Read,
    #[error("the temporary export could not be written safely; no publication was attempted")]
    Write,
    #[error("export publication is unconfirmed; inspect the selected directory before retrying")]
    Unconfirmed,
}

impl LocalExportService {
    /// Freezes validated text and a generated filename in an existing directory.
    /// Does not create directories, files, registries or database records.
    ///
    /// # Errors
    /// Rejects unsafe/missing directories, invalid text/names and existing destinations.
    pub fn prepare(
        existing_directory: &Path,
        automatic_basename: &str,
        redacted_text: String,
    ) -> Result<LocalExportPreview, LocalExportError> {
        validate_name(automatic_basename)?;
        if redacted_text.len() > MAX_EXPORT_BYTES || redacted_text.chars().any(unsafe_character) {
            return Err(LocalExportError::InvalidText);
        }
        let directory = DirectorySnapshot::read(existing_directory)?;
        let path = directory.canonical.join(automatic_basename);
        ensure_absent(&path)?;
        directory.ensure_unchanged()?;
        Ok(LocalExportPreview {
            directory,
            path,
            text: redacted_text,
        })
    }

    /// Publishes exactly this preview, only after the caller's explicit confirmation.
    /// Temporary files are owner-only (0600) on Unix; Windows inherits directory ACLs.
    /// Cancellation is checked before creating/writing/publishing, never after a known
    /// publication. No final file is removed on error or cancellation.
    ///
    /// # Errors
    /// Returns static cancellation, drift, collision, I/O or unknown-publication errors.
    pub fn confirm(
        preview: LocalExportPreview,
        cancellation: &CancellationToken,
    ) -> Result<LocalExportOutcome, LocalExportError> {
        confirm_observed(preview, cancellation, |_| {})
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublicationStage {
    TemporaryCreated,
    ReadyToPublish,
    Published,
}

fn confirm_observed(
    preview: LocalExportPreview,
    cancellation: &CancellationToken,
    mut observe: impl FnMut(PublicationStage),
) -> Result<LocalExportOutcome, LocalExportError> {
    cancelled(cancellation)?;
    preview.directory.ensure_unchanged()?;
    ensure_absent(&preview.path)?;
    cancelled(cancellation)?;
    let mut temporary = TemporaryExport::create(&preview.directory)?;
    observe(PublicationStage::TemporaryCreated);
    temporary.ensure_named()?;
    for chunk in preview.text.as_bytes().chunks(64 * 1024) {
        cancelled(cancellation)?;
        temporary
            .file_mut()
            .write_all(chunk)
            .map_err(|_| LocalExportError::Write)?;
    }
    temporary
        .file_mut()
        .as_file()
        .sync_all()
        .map_err(|_| LocalExportError::Write)?;
    observe(PublicationStage::ReadyToPublish);
    temporary.ensure_named()?;
    let expected_stamp = ensure_content(temporary.file_mut().as_file_mut(), &preview.text)?;
    ensure_absent(&preview.path)?;
    temporary.ensure_named()?;
    cancelled(cancellation)?;
    let published = temporary.publish(&preview.path)?;
    observe(PublicationStage::Published);
    // A late cancellation never removes a known published result. Failure after
    // publish is a warning, not an invitation to overwrite/delete or retry blindly.
    let verified = preview.directory.ensure_unchanged().is_ok()
        && file_stamp(&published).is_ok_and(|stamp| stamp == expected_stamp)
        && same_named_file(&published, &preview.path).is_ok();
    let synced = verified && sync_directory(&preview.directory.canonical);
    Ok(LocalExportOutcome {
        path: preview.path,
        warning: (!synced).then_some(PUBLISHED_WARNING),
    })
}

fn validate_name(name: &str) -> Result<(), LocalExportError> {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return Err(LocalExportError::InvalidName);
    };
    let reserved = matches!(
        stem.to_ascii_lowercase().as_str(),
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    );
    if name.len() > 128
        || !matches!(extension, "log" | "txt" | "json")
        || !stem
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !stem
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
        || reserved
    {
        return Err(LocalExportError::InvalidName);
    }
    Ok(())
}

fn unsafe_character(value: char) -> bool {
    (value.is_control() && !matches!(value, '\n' | '\t'))
        || matches!(value, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

fn cancelled(cancellation: &CancellationToken) -> Result<(), LocalExportError> {
    if cancellation.is_cancelled() {
        Err(LocalExportError::Cancelled)
    } else {
        Ok(())
    }
}

fn ensure_absent(path: &Path) -> Result<(), LocalExportError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(LocalExportError::AlreadyExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(LocalExportError::Read),
    }
}

struct DirectorySnapshot {
    original: PathBuf,
    canonical: PathBuf,
    original_stamps: Vec<DirectoryStamp>,
    canonical_stamps: Vec<DirectoryStamp>,
}

impl DirectorySnapshot {
    fn read(directory: &Path) -> Result<Self, LocalExportError> {
        let original_stamps = directory_stamps(directory)?;
        let canonical = directory
            .canonicalize()
            .map_err(|_| LocalExportError::Read)?;
        let canonical_stamps = directory_stamps(&canonical)?;
        let result = Self {
            original: directory.to_owned(),
            canonical,
            original_stamps,
            canonical_stamps,
        };
        result.ensure_unchanged()?;
        Ok(result)
    }

    fn ensure_unchanged(&self) -> Result<(), LocalExportError> {
        let current = directory_stamps(&self.original).map_err(|_| LocalExportError::Changed)?;
        let canonical = self
            .original
            .canonicalize()
            .map_err(|_| LocalExportError::Changed)?;
        if current != self.original_stamps
            || canonical != self.canonical
            || directory_stamps(&self.canonical).map_err(|_| LocalExportError::Changed)?
                != self.canonical_stamps
        {
            return Err(LocalExportError::Changed);
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
struct DirectoryStamp {
    identity: (u64, u64),
    mode: u32,
    readonly: bool,
}

fn directory_stamps(path: &Path) -> Result<Vec<DirectoryStamp>, LocalExportError> {
    if !path.is_absolute()
        || path.as_os_str().len() > MAX_PATH_BYTES
        || path.components().count() > MAX_PATH_COMPONENTS
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        || path.to_str().is_none_or(|value| {
            value
                .chars()
                .any(|character| character.is_control() || unsafe_character(character))
        })
    {
        return Err(LocalExportError::UnsafeDirectory);
    }
    #[cfg(windows)]
    if path.components().any(|part| {
        matches!(part, Component::Prefix(prefix) if matches!(
            prefix.kind(), std::path::Prefix::DeviceNS(_) | std::path::Prefix::Verbatim(_)
        ))
    }) {
        return Err(LocalExportError::UnsafeDirectory);
    }
    let ancestors: Vec<_> = path.ancestors().collect();
    ancestors
        .into_iter()
        .rev()
        .map(|part| {
            let file = open_safe(part, true)?;
            let metadata = file.metadata().map_err(|_| LocalExportError::Read)?;
            Ok(DirectoryStamp {
                identity: identity(&file, &metadata)?,
                mode: mode(&metadata),
                readonly: metadata.permissions().readonly(),
            })
        })
        .collect()
}

struct TemporaryExport<'a> {
    file: Option<NamedTempFile>,
    directory: &'a DirectorySnapshot,
}

impl<'a> TemporaryExport<'a> {
    fn create(directory: &'a DirectorySnapshot) -> Result<Self, LocalExportError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix(".shipforge-export-").suffix(".tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o600));
        }
        let file = builder
            .tempfile_in(&directory.canonical)
            .map_err(|_| LocalExportError::Write)?;
        let result = Self {
            file: Some(file),
            directory,
        };
        result.ensure_named()?;
        Ok(result)
    }

    fn file_mut(&mut self) -> &mut NamedTempFile {
        self.file
            .as_mut()
            .expect("temporary file exists before publication")
    }

    fn ensure_named(&self) -> Result<(), LocalExportError> {
        self.directory.ensure_unchanged()?;
        let file = self
            .file
            .as_ref()
            .expect("temporary file exists before publication");
        same_named_file(file.as_file(), file.path())
    }

    fn publish(&mut self, path: &Path) -> Result<File, LocalExportError> {
        let file = self
            .file
            .take()
            .expect("temporary file exists before publication");
        match file.persist_noclobber(path) {
            Ok(published) => Ok(published),
            Err(error) => {
                let kind = error.error.kind();
                self.file = Some(error.file);
                Err(if kind == io::ErrorKind::AlreadyExists {
                    LocalExportError::AlreadyExists
                } else {
                    LocalExportError::Unconfirmed
                })
            }
        }
    }
}

impl Drop for TemporaryExport<'_> {
    fn drop(&mut self) {
        if self.file.is_some() && self.ensure_named().is_err() {
            // A replaced source/parent may now name somebody else's file. Leave
            // the original temporary file alone rather than unlink through drift.
            if let Some(file) = &mut self.file {
                file.disable_cleanup(true);
            }
        }
    }
}

#[derive(PartialEq, Eq)]
struct FileStamp {
    identity: (u64, u64),
    size: u64,
    modified: Option<SystemTime>,
    mode: u32,
    readonly: bool,
}

fn file_stamp(file: &File) -> Result<FileStamp, LocalExportError> {
    let metadata = file.metadata().map_err(|_| LocalExportError::Read)?;
    if !metadata.is_file() || linked(&metadata) {
        return Err(LocalExportError::Changed);
    }
    #[cfg(unix)]
    if mode(&metadata) & 0o077 != 0 {
        return Err(LocalExportError::Changed);
    }
    Ok(FileStamp {
        identity: identity(file, &metadata)?,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        mode: mode(&metadata),
        readonly: metadata.permissions().readonly(),
    })
}

fn same_named_file(file: &File, path: &Path) -> Result<(), LocalExportError> {
    let named = open_safe(path, false).map_err(|_| LocalExportError::Changed)?;
    if file_stamp(file)? == file_stamp(&named)? {
        Ok(())
    } else {
        Err(LocalExportError::Changed)
    }
}

fn ensure_content(file: &mut File, expected: &str) -> Result<FileStamp, LocalExportError> {
    file.rewind().map_err(|_| LocalExportError::Read)?;
    let before = file_stamp(file)?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take(MAX_EXPORT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalExportError::Read)?;
    if bytes != expected.as_bytes() || file_stamp(file)? != before {
        return Err(LocalExportError::Changed);
    }
    Ok(before)
}

fn linked(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn open_safe(path: &Path, directory: bool) -> Result<File, LocalExportError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| LocalExportError::UnsafeDirectory)?;
    if linked(&metadata) || (directory && !metadata.is_dir()) || (!directory && !metadata.is_file())
    {
        return Err(LocalExportError::UnsafeDirectory);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT, plus BACKUP_SEMANTICS for directories.
        options.custom_flags(0x0020_0000 | if directory { 0x0200_0000 } else { 0 });
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x0002_0000 | 0x800); // O_NOFOLLOW | O_NONBLOCK
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100 | 0x4); // O_NOFOLLOW | O_NONBLOCK
    }
    let file = options.open(path).map_err(|_| LocalExportError::Read)?;
    let opened = file.metadata().map_err(|_| LocalExportError::Read)?;
    if linked(&opened) || (directory && !opened.is_dir()) || (!directory && !opened.is_file()) {
        return Err(LocalExportError::UnsafeDirectory);
    }
    Ok(file)
}

fn identity(file: &File, metadata: &Metadata) -> Result<(u64, u64), LocalExportError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = file;
        if metadata.is_file() && metadata.nlink() != 1 {
            return Err(LocalExportError::Changed);
        }
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let info = winapi_util::file::information(file).map_err(|_| LocalExportError::Read)?;
        if metadata.is_file() && info.number_of_links() != 1 {
            return Err(LocalExportError::Changed);
        }
        Ok((info.volume_serial_number(), info.file_index()))
    }
}

fn mode(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn sync_directory(path: &Path) -> bool {
    open_safe(path, true)
        .and_then(|file| file.sync_all().map_err(|_| LocalExportError::Write))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        output: PathBuf,
        source: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            #[cfg(windows)]
            let temporary = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            #[cfg(not(windows))]
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let output = root.join("exports");
            let source = root.join("source.log");
            fs::create_dir(&output).unwrap();
            fs::write(&source, "source remains unchanged").unwrap();
            Self {
                _temporary: temporary,
                root,
                output,
                source,
            }
        }

        fn preview(&self) -> LocalExportPreview {
            LocalExportService::prepare(
                &self.output,
                "shipforge-diagnostic-123.log",
                "日志\n\t[REDACTED] 'quoted'\n".into(),
            )
            .unwrap()
        }

        fn unchanged_source(&self) {
            assert_eq!(fs::read(&self.source).unwrap(), b"source remains unchanged");
        }

        fn output_files(&self) -> Vec<PathBuf> {
            fs::read_dir(&self.output)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect()
        }
    }

    #[test]
    fn prepare_is_read_only_and_confirmation_publishes_exact_frozen_bytes() {
        let fixture = Fixture::new();
        let preview = fixture.preview();
        let text = preview.text().to_owned();
        let path = preview.path().to_owned();
        assert_eq!(preview.byte_len(), text.len());
        assert!(fixture.output_files().is_empty());
        assert!(!format!("{preview:?}").contains("REDACTED"));
        let outcome = LocalExportService::confirm(preview, &CancellationToken::new()).unwrap();
        assert_eq!(outcome.path(), path);
        assert_eq!(fs::read(&path).unwrap(), text.as_bytes());
        assert_eq!(
            fixture.output_files().as_slice(),
            std::slice::from_ref(&path)
        );
        fixture.unchanged_source();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn dropped_preview_and_precancelled_confirmation_create_nothing() {
        let fixture = Fixture::new();
        drop(fixture.preview());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert_eq!(
            LocalExportService::confirm(fixture.preview(), &cancellation).unwrap_err(),
            LocalExportError::Cancelled
        );
        assert!(fixture.output_files().is_empty());
        fixture.unchanged_source();
    }

    #[test]
    fn cancellation_at_each_prepublication_boundary_removes_only_own_temporary() {
        for stop in [
            PublicationStage::TemporaryCreated,
            PublicationStage::ReadyToPublish,
        ] {
            let fixture = Fixture::new();
            let cancellation = CancellationToken::new();
            let result = confirm_observed(fixture.preview(), &cancellation, |stage| {
                if stage == stop {
                    cancellation.cancel();
                }
            });
            assert_eq!(result.unwrap_err(), LocalExportError::Cancelled);
            assert!(fixture.output_files().is_empty());
            fixture.unchanged_source();
        }
    }

    #[test]
    fn late_cancellation_preserves_known_published_result() {
        let fixture = Fixture::new();
        let cancellation = CancellationToken::new();
        let outcome = confirm_observed(fixture.preview(), &cancellation, |stage| {
            if stage == PublicationStage::Published {
                cancellation.cancel();
            }
        })
        .unwrap();
        assert!(cancellation.is_cancelled());
        assert!(outcome.path().is_file());
        assert_eq!(fixture.output_files().len(), 1);
        fixture.unchanged_source();
    }

    #[test]
    fn existing_destinations_at_prepare_confirm_or_publish_are_never_overwritten() {
        let fixture = Fixture::new();
        let path = fixture.output.join("exists.txt");
        fs::write(&path, "keep").unwrap();
        assert_eq!(
            LocalExportService::prepare(&fixture.output, "exists.txt", "replace".into())
                .unwrap_err(),
            LocalExportError::AlreadyExists
        );
        for stage_to_collide in [None, Some(PublicationStage::ReadyToPublish)] {
            let preview = fixture.preview();
            let destination = preview.path().to_owned();
            if stage_to_collide.is_none() {
                fs::write(&destination, "concurrent original").unwrap();
            }
            let result = confirm_observed(preview, &CancellationToken::new(), |stage| {
                if Some(stage) == stage_to_collide {
                    fs::write(&destination, "concurrent original").unwrap();
                }
            });
            assert_eq!(result.unwrap_err(), LocalExportError::AlreadyExists);
            assert_eq!(fs::read(&destination).unwrap(), b"concurrent original");
            fs::remove_file(destination).unwrap();
        }
        assert_eq!(fs::read(path).unwrap(), b"keep");
        assert_eq!(fixture.output_files().len(), 1);
        fixture.unchanged_source();
    }

    #[test]
    fn no_clobber_publication_itself_refuses_a_competing_file() {
        let fixture = Fixture::new();
        let snapshot = DirectorySnapshot::read(&fixture.output).unwrap();
        let destination = fixture.output.join("competing.txt");
        let mut temporary = TemporaryExport::create(&snapshot).unwrap();
        temporary.file_mut().write_all(b"ours").unwrap();
        temporary.file_mut().as_file().sync_all().unwrap();
        fs::write(&destination, "theirs").unwrap();
        assert_eq!(
            temporary.publish(&destination).unwrap_err(),
            LocalExportError::AlreadyExists
        );
        drop(temporary);
        assert_eq!(fs::read(&destination).unwrap(), b"theirs");
        assert_eq!(fixture.output_files(), [destination]);
        fixture.unchanged_source();
    }

    #[test]
    fn invalid_paths_names_text_and_limits_are_rejected_without_side_effects() {
        let fixture = Fixture::new();
        for name in [
            "../export.txt",
            "dir/export.txt",
            "dir\\export.txt",
            "export:stream.txt",
            "export..txt",
            ".txt",
            "con.txt",
            "LPT9.log",
            "space name.txt",
            "你好.txt",
            "export.txt ",
            "export.exe",
            "export\0.txt",
            "export\n.txt",
        ] {
            assert_eq!(
                LocalExportService::prepare(&fixture.output, name, "safe".into()).unwrap_err(),
                LocalExportError::InvalidName
            );
        }
        for text in ["raw\x1b[31m", "nul\0", "return\r", "bidi\u{202e}"] {
            assert_eq!(
                LocalExportService::prepare(&fixture.output, "safe.txt", text.into()).unwrap_err(),
                LocalExportError::InvalidText
            );
        }
        assert_eq!(
            LocalExportService::prepare(
                &fixture.output,
                "safe.txt",
                "x".repeat(MAX_EXPORT_BYTES + 1)
            )
            .unwrap_err(),
            LocalExportError::InvalidText
        );
        let missing = fixture.output.join("missing");
        for path in [
            missing.as_path(),
            fixture.source.as_path(),
            Path::new("relative"),
        ] {
            assert!(LocalExportService::prepare(path, "safe.txt", "safe".into()).is_err());
        }
        assert!(!missing.exists());
        assert!(fixture.output_files().is_empty());
        fixture.unchanged_source();
    }

    #[test]
    fn large_payload_is_not_screen_truncated_and_empty_snapshot_is_supported() {
        let fixture = Fixture::new();
        for (name, text) in [
            ("large.log", "日志\n".repeat(100_000)),
            ("empty.log", String::new()),
        ] {
            let preview = LocalExportService::prepare(&fixture.output, name, text.clone()).unwrap();
            assert_eq!(preview.text(), text);
            let outcome = LocalExportService::confirm(preview, &CancellationToken::new()).unwrap();
            assert_eq!(fs::read(outcome.path()).unwrap(), text.as_bytes());
        }
        fixture.unchanged_source();
    }

    #[test]
    fn directory_replacement_since_preview_is_rejected() {
        let fixture = Fixture::new();
        let preview = fixture.preview();
        fs::rename(&fixture.output, fixture.root.join("old-exports")).unwrap();
        fs::create_dir(&fixture.output).unwrap();
        assert_eq!(
            LocalExportService::confirm(preview, &CancellationToken::new()).unwrap_err(),
            LocalExportError::Changed
        );
        assert!(fixture.output_files().is_empty());
        fixture.unchanged_source();
    }

    #[test]
    fn source_temp_replacement_is_never_published_or_deleted_as_ours() {
        let fixture = Fixture::new();
        let mut replacement = PathBuf::new();
        let result = confirm_observed(fixture.preview(), &CancellationToken::new(), |stage| {
            if stage == PublicationStage::ReadyToPublish {
                let original = fixture.output_files().pop().unwrap();
                fs::rename(&original, fixture.output.join("moved-original.tmp")).unwrap();
                fs::write(&original, "foreign replacement").unwrap();
                replacement = original;
            }
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&replacement).unwrap(), b"foreign replacement");
        assert!(!fixture.output.join("shipforge-diagnostic-123.log").exists());
        fixture.unchanged_source();
    }

    #[test]
    fn changed_source_bytes_cannot_replace_the_frozen_payload() {
        let fixture = Fixture::new();
        let result = confirm_observed(fixture.preview(), &CancellationToken::new(), |stage| {
            if stage == PublicationStage::ReadyToPublish {
                fs::write(fixture.output_files().pop().unwrap(), "modified bytes").unwrap();
            }
        });
        assert_eq!(result.unwrap_err(), LocalExportError::Changed);
        assert!(fixture.output_files().is_empty());
        fixture.unchanged_source();
    }

    #[test]
    fn postpublication_drift_is_a_warning_not_cancellation_or_save_failure() {
        let fixture = Fixture::new();
        let preview = fixture.preview();
        let path = preview.path().to_owned();
        let moved = fixture.output.join("published-elsewhere.txt");
        let outcome = confirm_observed(preview, &CancellationToken::new(), |stage| {
            if stage == PublicationStage::Published {
                fs::rename(&path, &moved).unwrap();
            }
        })
        .unwrap();
        assert_eq!(outcome.path(), path);
        assert_eq!(outcome.warning(), Some(PUBLISHED_WARNING));
        assert!(moved.is_file());
        assert!(!path.exists());
        fixture.unchanged_source();
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_including_ancestors_and_dangling_destination_are_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let link = fixture.root.join("linked-exports");
        symlink(&fixture.output, &link).unwrap();
        fs::create_dir(fixture.output.join("nested")).unwrap();
        for path in [&link, &link.join("nested")] {
            assert_eq!(
                LocalExportService::prepare(path, "safe.txt", "safe".into()).unwrap_err(),
                LocalExportError::UnsafeDirectory
            );
        }
        symlink(
            fixture.output.join("missing"),
            fixture.output.join("safe.txt"),
        )
        .unwrap();
        assert_eq!(
            LocalExportService::prepare(&fixture.output, "safe.txt", "safe".into()).unwrap_err(),
            LocalExportError::AlreadyExists
        );
        fixture.unchanged_source();
    }

    #[cfg(unix)]
    #[test]
    fn parent_replaced_by_symlink_never_redirects_cleanup_or_publication() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let old = fixture.root.join("old-exports");
        let foreign = fixture.root.join("foreign");
        fs::create_dir(&foreign).unwrap();
        let mut foreign_file = PathBuf::new();
        let result = confirm_observed(fixture.preview(), &CancellationToken::new(), |stage| {
            if stage == PublicationStage::ReadyToPublish {
                let temporary = fixture.output_files().pop().unwrap();
                foreign_file = foreign.join(temporary.file_name().unwrap());
                fs::write(&foreign_file, "foreign file must survive").unwrap();
                fs::rename(&fixture.output, &old).unwrap();
                symlink(&foreign, &fixture.output).unwrap();
            }
        });
        assert_eq!(result.unwrap_err(), LocalExportError::Changed);
        assert_eq!(
            fs::read(foreign_file).unwrap(),
            b"foreign file must survive"
        );
        assert_eq!(fs::read_dir(&old).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&foreign).unwrap().count(), 1);
        fixture.unchanged_source();
    }

    #[cfg(unix)]
    #[test]
    fn linked_temporary_source_is_not_followed_or_published() {
        use std::os::unix::fs::symlink;
        for symbolic in [false, true] {
            let fixture = Fixture::new();
            let result = confirm_observed(fixture.preview(), &CancellationToken::new(), |stage| {
                if stage == PublicationStage::ReadyToPublish {
                    let path = fixture.output_files().pop().unwrap();
                    if symbolic {
                        fs::rename(&path, fixture.output.join("original.tmp")).unwrap();
                        symlink(&fixture.source, path).unwrap();
                    } else {
                        fs::hard_link(&path, fixture.output.join("alias.tmp")).unwrap();
                    }
                }
            });
            assert!(result.is_err());
            assert!(!fixture.output.join("shipforge-diagnostic-123.log").exists());
            fixture.unchanged_source();
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_permission_drift_rejects_confirmation() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let preview = fixture.preview();
        let old = fs::metadata(&fixture.output).unwrap().permissions();
        fs::set_permissions(&fixture.output, fs::Permissions::from_mode(0o700)).unwrap();
        if old.mode() & 0o777 == 0o700 {
            fs::set_permissions(&fixture.output, fs::Permissions::from_mode(0o750)).unwrap();
        }
        let result = LocalExportService::confirm(preview, &CancellationToken::new());
        fs::set_permissions(&fixture.output, old).unwrap();
        assert_eq!(result.unwrap_err(), LocalExportError::Changed);
        assert!(fixture.output_files().is_empty());
    }

    #[test]
    fn public_errors_and_debug_values_do_not_include_paths_or_payloads() {
        let fixture = Fixture::new();
        let preview = fixture.preview();
        let debug = format!("{preview:?}");
        assert!(!debug.contains(&fixture.root.to_string_lossy().to_string()));
        assert!(!debug.contains("quoted"));
        for error in [
            LocalExportError::Cancelled,
            LocalExportError::UnsafeDirectory,
            LocalExportError::Changed,
            LocalExportError::InvalidName,
            LocalExportError::InvalidText,
            LocalExportError::AlreadyExists,
            LocalExportError::Read,
            LocalExportError::Write,
            LocalExportError::Unconfirmed,
        ] {
            assert!(
                !error
                    .to_string()
                    .contains(&fixture.root.to_string_lossy().to_string())
            );
        }
    }
}
