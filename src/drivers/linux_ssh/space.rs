//! Conservative peak storage estimate from the already generated Release bytes.

use std::{
    fs::File,
    io::{self, Read},
    time::Duration,
};

use flate2::read::MultiGzDecoder;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::drivers::ReleasePackage;

use super::{
    AuthenticatedSession, DeploymentMarker, LinuxSshTarget,
    marker::MAX_MANIFEST_BYTES,
    preflight::{FilesystemCapacity, PreflightError, PreflightRemote, probe_capacity},
};

const MAX_ENTRIES: usize = 100_001; // Core output entries plus the manifest.
const MAX_PATH_BYTES: usize = 4096;
// An inode and its directory entry/index need space too. Reserve four blocks
// per object, independently of its data size; archive and marker hard links do
// not duplicate data, and staging -> releases is a rename, not a second copy.
const METADATA_BLOCKS_PER_OBJECT: u64 = 4;
// temporary, archives, releases, staging directory; archive, marker; two links.
const LAYOUT_OBJECTS: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StorageRequirement {
    pub bytes: u64,
    pub inodes: u64,
}

pub(super) async fn check_release_space(
    session: &AuthenticatedSession,
    target: &LinuxSshTarget,
    package: &ReleasePackage,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<StorageRequirement, SpaceError> {
    check_with_remote(session, target, package, timeout, cancellation).await
}

async fn check_with_remote<R: PreflightRemote>(
    remote: &R,
    target: &LinuxSshTarget,
    package: &ReleasePackage,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<StorageRequirement, SpaceError> {
    let capacity = probe_capacity(remote, target, timeout, cancellation).await?;
    let root_depth = target
        .root
        .split('/')
        .filter(|part| !part.is_empty())
        .count();
    let ancestor_depth = capacity
        .parent
        .split('/')
        .filter(|part| !part.is_empty())
        .count();
    let missing_directories = u64::try_from(root_depth.saturating_sub(ancestor_depth))
        .map_err(|_| SpaceError::Overflow)?;
    let release = package.clone();
    let block_size = capacity.block_size;
    let token = cancellation.clone();
    let requirement = tokio::task::spawn_blocking(move || {
        estimate_archive(&release, block_size, missing_directories, &token)
    })
    .await
    .map_err(|_| SpaceError::ScanFailed)?;
    if cancellation.is_cancelled() {
        return Err(SpaceError::Cancelled);
    }
    let requirement = requirement?;
    ensure_capacity(requirement, &capacity)?;
    Ok(requirement)
}

fn ensure_capacity(
    required: StorageRequirement,
    capacity: &FilesystemCapacity,
) -> Result<(), SpaceError> {
    if required.bytes > capacity.available_bytes {
        return Err(SpaceError::InsufficientBytes {
            required: required.bytes,
            available: capacity.available_bytes,
        });
    }
    if let Some(available) = capacity.available_inodes
        && required.inodes > available
    {
        return Err(SpaceError::InsufficientInodes {
            required: required.inodes,
            available,
        });
    }
    Ok(())
}

fn estimate_archive(
    package: &ReleasePackage,
    block_size: u64,
    missing_directories: u64,
    cancellation: &CancellationToken,
) -> Result<StorageRequirement, SpaceError> {
    if cancellation.is_cancelled() {
        return Err(SpaceError::Cancelled);
    }
    let metadata = std::fs::symlink_metadata(package.path())?;
    if !metadata.is_file() || metadata.len() != package.size() {
        return Err(SpaceError::InvalidArchive);
    }
    let input = HashReader {
        inner: File::open(package.path())?,
        digest: Sha256::new(),
    };
    let reader = CancellableReader {
        inner: MultiGzDecoder::new(input),
        cancellation,
    };
    let mut archive = tar::Archive::new(reader);
    let mut data_bytes = file_storage(package.size(), block_size)?;
    data_bytes = add(data_bytes, round_up(4096, block_size)?)?; // Marker and its hard link.
    let mut inodes = add(LAYOUT_OBJECTS, missing_directories)?;
    let mut manifest_seen = false;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= MAX_ENTRIES {
            return Err(SpaceError::InvalidArchive);
        }
        let mut entry = entry?;
        let path = entry.path_bytes();
        if path.is_empty() || path.len() > MAX_PATH_BYTES {
            return Err(SpaceError::InvalidArchive);
        }
        let is_manifest = path.as_ref() == b"manifest.json";
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(SpaceError::InvalidArchive);
        }
        inodes = add(inodes, 1)?;
        if kind.is_file() {
            data_bytes = add(data_bytes, file_storage(entry.size(), block_size)?)?;
        }
        if is_manifest {
            if manifest_seen || !kind.is_file() || entry.size() > MAX_MANIFEST_BYTES as u64 {
                return Err(SpaceError::InvalidArchive);
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            DeploymentMarker::for_release(package.release())
                .validate_manifest(&bytes, &package.release().version)
                .map_err(|_| SpaceError::InvalidArchive)?;
            manifest_seen = true;
        } else {
            io::copy(&mut entry, &mut io::sink())?;
        }
    }
    let mut reader = archive.into_inner();
    io::copy(&mut reader, &mut io::sink())?; // Verify gzip trailers and hash all bytes.
    let digest = reader.inner.into_inner().digest;
    if !manifest_seen || format!("{:x}", digest.finalize()) != package.sha256() {
        return Err(SpaceError::InvalidArchive);
    }
    let metadata_bytes = inodes
        .checked_mul(METADATA_BLOCKS_PER_OBJECT)
        .and_then(|blocks| blocks.checked_mul(block_size))
        .ok_or(SpaceError::Overflow)?;
    Ok(StorageRequirement {
        bytes: add(data_bytes, metadata_bytes)?,
        inodes,
    })
}

fn round_up(bytes: u64, block_size: u64) -> Result<u64, SpaceError> {
    if block_size == 0 {
        return Err(SpaceError::Overflow);
    }
    let blocks = bytes / block_size + u64::from(!bytes.is_multiple_of(block_size));
    blocks.checked_mul(block_size).ok_or(SpaceError::Overflow)
}

fn file_storage(bytes: u64, block_size: u64) -> Result<u64, SpaceError> {
    let data = round_up(bytes, block_size)?;
    // In addition to object metadata, reserve 64 bytes per allocated data block
    // for extent/block-map growth. This estimate cannot reserve space against
    // concurrent users or replace filesystem/quota errors during the write.
    let mapping = (data / block_size)
        .checked_mul(64)
        .ok_or(SpaceError::Overflow)?;
    add(data, round_up(mapping, block_size)?)
}

fn add(left: u64, right: u64) -> Result<u64, SpaceError> {
    left.checked_add(right).ok_or(SpaceError::Overflow)
}

struct HashReader<R> {
    inner: R,
    digest: Sha256,
}

impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.digest.update(&buffer[..count]);
        Ok(count)
    }
}

struct CancellableReader<'a, R> {
    inner: R,
    cancellation: &'a CancellationToken,
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("Release space inspection cancelled"));
        }
        self.inner.read(buffer)
    }
}

#[derive(Debug, Error)]
pub(super) enum SpaceError {
    #[error(transparent)]
    Preflight(#[from] PreflightError),
    #[error("Release space inspection was cancelled")]
    Cancelled,
    #[error("could not read Release for space inspection: {0}")]
    Io(#[from] io::Error),
    #[error("Release space inspection failed")]
    ScanFailed,
    #[error("Release storage calculation overflowed")]
    Overflow,
    #[error("Release content, metadata, or digest differs from the standard package")]
    InvalidArchive,
    #[error(
        "insufficient remote space: this Release requires at least {required} bytes including extraction and metadata reserve; {available} available"
    )]
    InsufficientBytes { required: u64, available: u64 },
    #[error(
        "insufficient remote inodes: this Release requires at least {required}; {available} available"
    )]
    InsufficientInodes { required: u64, available: u64 },
}

#[cfg(test)]
mod tests;
