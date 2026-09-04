use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component as PathComponent, Path, PathBuf},
};

use flate2::{Compression, GzBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ResolvedArtifact, ResolvedArtifactKind},
    domain::{
        ComponentGeneration, ComponentName, ComponentRelease, EnvironmentId, ProjectId,
        ReleaseVersion,
    },
    drivers::ReleasePackage,
};

const MANIFEST_PATH: &str = "manifest.json";
const FILE_MODE: u32 = 0o644;
const DIRECTORY_MODE: u32 = 0o755;
const EXECUTABLE_MODE: u32 = 0o755;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub environment_id: EnvironmentId,
    pub component: ComponentName,
    pub generation: ComponentGeneration,
    pub version: ReleaseVersion,
    pub created_at_unix: u64,
    pub source_revision: Option<String>,
}

impl ReleaseManifest {
    #[must_use]
    pub fn new(
        release: &ComponentRelease,
        created_at_unix: u64,
        source_revision: Option<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            project_id: release.project_id.clone(),
            environment_id: release.environment_id.clone(),
            component: release.component.clone(),
            generation: release.generation,
            version: release.version.clone(),
            created_at_unix,
            source_revision,
        }
    }
}

/// Packages one resolved build output into one immutable Release.
///
/// Files are streamed through tar, gzip, and SHA-256 without loading the
/// artifact into memory. The completed temporary file is persisted with
/// no-clobber semantics, so an existing version is never overwritten.
///
/// # Errors
///
/// Returns an error for unsafe archive entries, symbolic links, special files,
/// cancellation, I/O failure, serialization failure, or an existing version.
pub fn package_release(
    artifact: &ResolvedArtifact,
    release: &ComponentRelease,
    output_directory: &Path,
    created_at_unix: u64,
    source_revision: Option<String>,
    cancellation: &CancellationToken,
) -> Result<ReleasePackage, PackageError> {
    if cancellation.is_cancelled() {
        return Err(PackageError::Cancelled);
    }
    validate_source_revision(source_revision.as_deref())?;
    let canonical_artifact = revalidate_artifact(artifact)?;
    let prospective_output = prospective_canonical_path(output_directory)?;
    if canonical_artifact.kind == ResolvedArtifactKind::Directory
        && prospective_output.starts_with(&canonical_artifact.path)
    {
        return Err(PackageError::OutputInsideArtifact(prospective_output));
    }
    fs::create_dir_all(output_directory).map_err(|source| PackageError::Io {
        operation: "create output directory",
        path: output_directory.to_owned(),
        source,
    })?;
    let output_directory = output_directory
        .canonicalize()
        .map_err(|source| PackageError::Io {
            operation: "resolve output directory",
            path: output_directory.to_owned(),
            source,
        })?;
    if canonical_artifact.kind == ResolvedArtifactKind::Directory
        && output_directory.starts_with(&canonical_artifact.path)
    {
        return Err(PackageError::OutputInsideArtifact(output_directory));
    }

    let final_path = output_directory.join(format!("{}.tar.gz", release.version));
    if final_path.exists() {
        return Err(PackageError::AlreadyExists(final_path));
    }
    let entries = collect_entries(&canonical_artifact, cancellation)?;
    if entries.is_empty() {
        return Err(PackageError::EmptyArtifact(canonical_artifact.path));
    }
    let manifest = ReleaseManifest::new(release, created_at_unix, source_revision);
    let mut manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    manifest_bytes.push(b'\n');

    let mut temporary = tempfile::Builder::new()
        .prefix(".shipforge-release-")
        .suffix(".tmp")
        .tempfile_in(&output_directory)
        .map_err(|source| PackageError::Io {
            operation: "create temporary Release",
            path: output_directory.clone(),
            source,
        })?;
    let (sha256, size) = write_archive(
        temporary.as_file_mut(),
        &manifest_bytes,
        &entries,
        created_at_unix,
        cancellation,
    )?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| PackageError::Io {
            operation: "sync temporary Release",
            path: temporary.path().to_owned(),
            source,
        })?;
    if cancellation.is_cancelled() {
        return Err(PackageError::Cancelled);
    }
    temporary.persist_noclobber(&final_path).map_err(|error| {
        if error.error.kind() == io::ErrorKind::AlreadyExists {
            PackageError::AlreadyExists(final_path.clone())
        } else {
            PackageError::Io {
                operation: "persist Release",
                path: final_path.clone(),
                source: error.error,
            }
        }
    })?;

    Ok(ReleasePackage::new(
        release.clone(),
        final_path,
        sha256,
        size,
    ))
}

fn revalidate_artifact(artifact: &ResolvedArtifact) -> Result<ResolvedArtifact, PackageError> {
    let metadata = safe_metadata(&artifact.path)?;
    let actual_kind = if metadata.is_file() {
        if metadata.len() == 0 {
            return Err(PackageError::EmptyArtifact(artifact.path.clone()));
        }
        ResolvedArtifactKind::File
    } else if metadata.is_dir() {
        ResolvedArtifactKind::Directory
    } else {
        return Err(PackageError::UnsupportedEntry(artifact.path.clone()));
    };
    if actual_kind != artifact.kind {
        return Err(PackageError::ArtifactKindChanged(artifact.path.clone()));
    }
    let path = artifact
        .path
        .canonicalize()
        .map_err(|source| PackageError::Io {
            operation: "resolve Artifact",
            path: artifact.path.clone(),
            source,
        })?;
    Ok(ResolvedArtifact {
        path,
        kind: actual_kind,
    })
}

#[derive(Debug)]
struct ArchiveEntry {
    source: PathBuf,
    archive_path: String,
    kind: EntryKind,
    size: u64,
    mode: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

fn collect_entries(
    artifact: &ResolvedArtifact,
    cancellation: &CancellationToken,
) -> Result<Vec<ArchiveEntry>, PackageError> {
    match artifact.kind {
        ResolvedArtifactKind::File => {
            let name = artifact
                .path
                .file_name()
                .ok_or_else(|| PackageError::UnsafePath(artifact.path.clone()))?;
            let archive_path = normalize_archive_path(Path::new(name))?;
            if archive_path == MANIFEST_PATH {
                return Err(PackageError::ReservedPath(artifact.path.clone()));
            }
            let metadata = safe_metadata(&artifact.path)?;
            Ok(vec![ArchiveEntry {
                source: artifact.path.clone(),
                archive_path,
                kind: EntryKind::File,
                size: metadata.len(),
                mode: EXECUTABLE_MODE,
            }])
        }
        ResolvedArtifactKind::Directory => {
            let mut entries = Vec::new();
            collect_directory(&artifact.path, &artifact.path, &mut entries, cancellation)?;
            Ok(entries)
        }
    }
}

fn prospective_canonical_path(path: &Path) -> Result<PathBuf, PackageError> {
    let absolute = std::path::absolute(path).map_err(|source| PackageError::Io {
        operation: "resolve prospective output directory",
        path: path.to_owned(),
        source,
    })?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| PackageError::UnsafePath(absolute.clone()))?;
        missing.push(name.to_owned());
        existing = existing
            .parent()
            .ok_or_else(|| PackageError::UnsafePath(absolute.clone()))?;
    }
    let mut resolved = existing.canonicalize().map_err(|source| PackageError::Io {
        operation: "resolve prospective output ancestor",
        path: existing.to_owned(),
        source,
    })?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn collect_directory(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<ArchiveEntry>,
    cancellation: &CancellationToken,
) -> Result<(), PackageError> {
    if cancellation.is_cancelled() {
        return Err(PackageError::Cancelled);
    }
    let directory_entries = fs::read_dir(directory).map_err(|source| PackageError::Io {
        operation: "read Artifact directory",
        path: directory.to_owned(),
        source,
    })?;
    let mut children = Vec::new();
    for child in directory_entries {
        if cancellation.is_cancelled() {
            return Err(PackageError::Cancelled);
        }
        if entries.len().saturating_add(children.len()) >= MAX_ARCHIVE_ENTRIES {
            return Err(PackageError::TooManyEntries(MAX_ARCHIVE_ENTRIES));
        }
        children.push(child.map_err(|source| PackageError::Io {
            operation: "read Artifact entry",
            path: directory.to_owned(),
            source,
        })?);
    }
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        if cancellation.is_cancelled() {
            return Err(PackageError::Cancelled);
        }
        let source = child.path();
        let relative = source
            .strip_prefix(root)
            .map_err(|_| PackageError::UnsafePath(source.clone()))?;
        let archive_path = normalize_archive_path(relative)?;
        if archive_path == MANIFEST_PATH {
            return Err(PackageError::ReservedPath(source));
        }
        let metadata = safe_metadata(&source)?;
        if metadata.is_dir() {
            entries.push(ArchiveEntry {
                source: source.clone(),
                archive_path,
                kind: EntryKind::Directory,
                size: 0,
                mode: DIRECTORY_MODE,
            });
            collect_directory(root, &source, entries, cancellation)?;
        } else if metadata.is_file() {
            entries.push(ArchiveEntry {
                source,
                archive_path,
                kind: EntryKind::File,
                size: metadata.len(),
                mode: source_file_mode(&metadata),
            });
        } else {
            return Err(PackageError::UnsupportedEntry(source));
        }
    }
    Ok(())
}

fn safe_metadata(path: &Path) -> Result<fs::Metadata, PackageError> {
    let metadata = path.symlink_metadata().map_err(|source| PackageError::Io {
        operation: "inspect Artifact entry",
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(PackageError::SymbolicLink(path.to_owned()));
    }
    Ok(metadata)
}

fn normalize_archive_path(path: &Path) -> Result<String, PackageError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            PathComponent::Normal(value) => parts.push(
                value
                    .to_str()
                    .ok_or_else(|| PackageError::NonUtf8Path(path.to_owned()))?,
            ),
            _ => return Err(PackageError::UnsafePath(path.to_owned())),
        }
    }
    if parts.is_empty() {
        return Err(PackageError::UnsafePath(path.to_owned()));
    }
    let normalized = parts.join("/");
    if normalized.len() > MAX_ARCHIVE_PATH_BYTES {
        return Err(PackageError::PathTooLong(path.to_owned()));
    }
    Ok(normalized)
}

fn validate_source_revision(source_revision: Option<&str>) -> Result<(), PackageError> {
    if source_revision.is_some_and(|revision| {
        !(7..=64).contains(&revision.len())
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(PackageError::InvalidSourceRevision);
    }
    Ok(())
}

fn write_archive(
    output: &mut File,
    manifest: &[u8],
    entries: &[ArchiveEntry],
    mtime: u64,
    cancellation: &CancellationToken,
) -> Result<(String, u64), PackageError> {
    let hashing = HashingWriter::new(output);
    let gzip = GzBuilder::new()
        .mtime(0)
        .write(hashing, Compression::default());
    let mut archive = Builder::new(gzip);
    archive.mode(tar::HeaderMode::Deterministic);

    append_bytes(&mut archive, MANIFEST_PATH, manifest, FILE_MODE, mtime)?;
    for entry in entries {
        if cancellation.is_cancelled() {
            return Err(PackageError::Cancelled);
        }
        append_entry(&mut archive, entry, mtime, cancellation)?;
    }
    let gzip = archive.into_inner().map_err(PackageError::Archive)?;
    let hashing = gzip.finish().map_err(PackageError::Archive)?;
    Ok(hashing.finish())
}

fn append_bytes<W: Write>(
    archive: &mut Builder<W>,
    path: &str,
    bytes: &[u8],
    mode: u32,
    mtime: u64,
) -> Result<(), PackageError> {
    let mut header = regular_header(
        bytes.len().try_into().expect("usize fits in u64"),
        mode,
        mtime,
    );
    archive
        .append_data(&mut header, path, bytes)
        .map_err(PackageError::Archive)
}

fn append_entry<W: Write>(
    archive: &mut Builder<W>,
    entry: &ArchiveEntry,
    mtime: u64,
    cancellation: &CancellationToken,
) -> Result<(), PackageError> {
    match entry.kind {
        EntryKind::Directory => {
            let mut header = base_header(entry.mode, mtime);
            header.set_entry_type(EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            archive
                .append_data(&mut header, &entry.archive_path, io::empty())
                .map_err(PackageError::Archive)
        }
        EntryKind::File => {
            let metadata = safe_metadata(&entry.source)?;
            if !metadata.is_file() || metadata.len() != entry.size {
                return Err(PackageError::ArtifactChanged(entry.source.clone()));
            }
            let file = File::open(&entry.source).map_err(|source| PackageError::Io {
                operation: "open Artifact file",
                path: entry.source.clone(),
                source,
            })?;
            let mut reader = CancellationReader {
                inner: file,
                cancellation,
            };
            let mut header = regular_header(entry.size, entry.mode, mtime);
            archive
                .append_data(&mut header, &entry.archive_path, &mut reader)
                .map_err(|source| {
                    if source.kind() == io::ErrorKind::Interrupted {
                        PackageError::Cancelled
                    } else {
                        PackageError::Archive(source)
                    }
                })
        }
    }
}

fn regular_header(size: u64, mode: u32, mtime: u64) -> Header {
    let mut header = base_header(mode, mtime);
    header.set_entry_type(EntryType::Regular);
    header.set_size(size);
    header.set_cksum();
    header
}

fn base_header(mode: u32, mtime: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    header
        .set_username("")
        .expect("empty tar username is valid");
    header
        .set_groupname("")
        .expect("empty tar group name is valid");
    header
}

#[cfg(unix)]
fn source_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        FILE_MODE
    } else {
        EXECUTABLE_MODE
    }
}

#[cfg(not(unix))]
const fn source_file_mode(_metadata: &fs::Metadata) -> u32 {
    FILE_MODE
}

struct CancellationReader<'a> {
    inner: File,
    cancellation: &'a CancellationToken,
}

impl Read for CancellationReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Release packaging cancelled",
            ));
        }
        self.inner.read(buffer)
    }
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    size: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            size: 0,
        }
    }

    fn finish(self) -> (String, u64) {
        (format!("{:x}", self.hasher.finalize()), self.size)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.size = self
            .size
            .checked_add(written as u64)
            .expect("Release size cannot exceed u64");
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Error)]
pub enum PackageError {
    #[error("Release packaging was cancelled")]
    Cancelled,
    #[error("Release already exists: `{0}`")]
    AlreadyExists(PathBuf),
    #[error("Release output directory is inside the Artifact: `{0}`")]
    OutputInsideArtifact(PathBuf),
    #[error("Artifact path cannot be represented safely in a portable archive: `{0}`")]
    UnsafePath(PathBuf),
    #[error("Artifact path is not valid UTF-8: `{0}`")]
    NonUtf8Path(PathBuf),
    #[error("Artifact path exceeds the {MAX_ARCHIVE_PATH_BYTES}-byte archive limit: `{0}`")]
    PathTooLong(PathBuf),
    #[error("Artifact contains more than {0} entries")]
    TooManyEntries(usize),
    #[error("Artifact uses reserved archive path `manifest.json`: `{0}`")]
    ReservedPath(PathBuf),
    #[error("Artifact contains a symbolic link, which the MVP does not package: `{0}`")]
    SymbolicLink(PathBuf),
    #[error("Artifact contains an unsupported filesystem entry: `{0}`")]
    UnsupportedEntry(PathBuf),
    #[error("Artifact is empty: `{0}`")]
    EmptyArtifact(PathBuf),
    #[error("Artifact type changed after build validation: `{0}`")]
    ArtifactKindChanged(PathBuf),
    #[error("Artifact entry changed while the Release was being created: `{0}`")]
    ArtifactChanged(PathBuf),
    #[error("source revision must be a 7 to 64 character hexadecimal Git object ID")]
    InvalidSourceRevision,
    #[error("could not {operation} at `{path}`: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not create Release manifest: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("could not write Release archive: {0}")]
    Archive(io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DestinationKey, DestinationRevision};
    use flate2::read::GzDecoder;
    use std::collections::BTreeMap;

    fn release(version: &str) -> ComponentRelease {
        ComponentRelease {
            project_id: ProjectId::new(),
            environment_id: EnvironmentId::new(),
            component: ComponentName::parse("api").unwrap(),
            generation: ComponentGeneration::INITIAL,
            version: ReleaseVersion::parse(version).unwrap(),
            destination: DestinationKey::parse("dst_00000000000000000000000000000001").unwrap(),
            destination_revision: DestinationRevision::INITIAL,
        }
    }

    fn archive_entries(path: &Path) -> BTreeMap<String, (u32, Vec<u8>)> {
        let file = File::open(path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().replace('\\', "/");
                let mode = entry.header().mode().unwrap();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (path, (mode, bytes))
            })
            .collect()
    }

    #[test]
    fn packages_directory_once_with_manifest_modes_and_digest() {
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("dist");
        fs::create_dir_all(artifact_path.join("assets")).unwrap();
        fs::write(artifact_path.join("index.html"), "hello").unwrap();
        fs::write(artifact_path.join("assets/app.js"), "js").unwrap();
        let output = project.path().join("out");
        let package = package_release(
            &ResolvedArtifact {
                path: artifact_path,
                kind: ResolvedArtifactKind::Directory,
            },
            &release("v1.2.3"),
            &output,
            1_725_000_000,
            Some("abc1234".into()),
            &CancellationToken::new(),
        )
        .unwrap();

        assert_eq!(package.path().file_name().unwrap(), "v1.2.3.tar.gz");
        assert_eq!(package.sha256().len(), 64);
        assert_eq!(package.size(), fs::metadata(package.path()).unwrap().len());
        assert_eq!(
            package.sha256(),
            format!("{:x}", Sha256::digest(fs::read(package.path()).unwrap()))
        );
        let entries = archive_entries(package.path());
        assert_eq!(
            entries.keys().cloned().collect::<Vec<_>>(),
            ["assets", "assets/app.js", "index.html", "manifest.json"]
        );
        assert_eq!(entries["assets"].0, DIRECTORY_MODE);
        assert_eq!(entries["assets/app.js"].0, FILE_MODE);
        let manifest: ReleaseManifest = serde_json::from_slice(&entries[MANIFEST_PATH].1).unwrap();
        assert_eq!(manifest.version.as_str(), "v1.2.3");
        assert_eq!(manifest.source_revision.as_deref(), Some("abc1234"));
    }

    #[test]
    fn identical_inputs_produce_identical_release_bytes() {
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("server");
        fs::write(&artifact_path, "binary").unwrap();
        let artifact = ResolvedArtifact {
            path: artifact_path,
            kind: ResolvedArtifactKind::File,
        };
        let release_one = release("same");
        let first = package_release(
            &artifact,
            &release_one,
            &project.path().join("one"),
            42,
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        let mut release_two = release_one;
        release_two.destination =
            DestinationKey::parse("dst_00000000000000000000000000000002").unwrap();
        let second = package_release(
            &artifact,
            &release_two,
            &project.path().join("two"),
            42,
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(archive_entries(first.path())["server"].0, EXECUTABLE_MODE);
        assert_eq!(
            fs::read(first.path()).unwrap(),
            fs::read(second.path()).unwrap()
        );
        assert_eq!(first.sha256(), second.sha256());
    }

    #[test]
    fn output_directory_cannot_be_created_inside_artifact() {
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("dist");
        fs::create_dir(&artifact_path).unwrap();
        fs::write(artifact_path.join("index.html"), "hello").unwrap();
        let output = artifact_path.join("generated/releases");
        let error = package_release(
            &ResolvedArtifact {
                path: artifact_path,
                kind: ResolvedArtifactKind::Directory,
            },
            &release("v1"),
            &output,
            1,
            None,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::OutputInsideArtifact(_)));
        assert!(!output.exists());
    }

    #[test]
    fn invalid_source_revision_is_rejected_before_output_creation() {
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("server");
        fs::write(&artifact_path, "binary").unwrap();
        let output = project.path().join("out");
        let error = package_release(
            &ResolvedArtifact {
                path: artifact_path,
                kind: ResolvedArtifactKind::File,
            },
            &release("v1"),
            &output,
            1,
            Some("branch-name".into()),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::InvalidSourceRevision));
        assert!(!output.exists());
    }

    #[test]
    fn existing_release_is_never_overwritten() {
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("server");
        fs::write(&artifact_path, "binary").unwrap();
        let output = project.path().join("out");
        fs::create_dir(&output).unwrap();
        let destination = output.join("v1.tar.gz");
        fs::write(&destination, "keep").unwrap();
        let error = package_release(
            &ResolvedArtifact {
                path: artifact_path,
                kind: ResolvedArtifactKind::File,
            },
            &release("v1"),
            &output,
            1,
            None,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::AlreadyExists(_)));
        assert_eq!(fs::read(destination).unwrap(), b"keep");
    }

    #[test]
    fn cancellation_and_reserved_manifest_leave_no_release() {
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("dist");
        fs::create_dir(&artifact_path).unwrap();
        fs::write(artifact_path.join(MANIFEST_PATH), "collision").unwrap();
        let output = project.path().join("out");
        let error = package_release(
            &ResolvedArtifact {
                path: artifact_path.clone(),
                kind: ResolvedArtifactKind::Directory,
            },
            &release("v1"),
            &output,
            1,
            None,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::ReservedPath(_)));

        fs::remove_file(artifact_path.join(MANIFEST_PATH)).unwrap();
        fs::write(artifact_path.join("file"), "data").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            package_release(
                &ResolvedArtifact {
                    path: artifact_path,
                    kind: ResolvedArtifactKind::Directory,
                },
                &release("v2"),
                &output,
                1,
                None,
                &cancellation,
            ),
            Err(PackageError::Cancelled)
        ));
        assert!(!output.join("v1.tar.gz").exists());
        assert!(!output.join("v2.tar.gz").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_nested_symbolic_links() {
        use std::os::unix::fs::symlink;
        let project = tempfile::tempdir().unwrap();
        let artifact_path = project.path().join("dist");
        fs::create_dir(&artifact_path).unwrap();
        fs::write(project.path().join("secret"), "secret").unwrap();
        symlink(project.path().join("secret"), artifact_path.join("link")).unwrap();
        let error = package_release(
            &ResolvedArtifact {
                path: artifact_path,
                kind: ResolvedArtifactKind::Directory,
            },
            &release("v1"),
            &project.path().join("out"),
            1,
            None,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(error, PackageError::SymbolicLink(_)));
    }
}
