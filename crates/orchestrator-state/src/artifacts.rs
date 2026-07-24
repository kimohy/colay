use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

use fs2::FileExt as _;
use orchestrator_domain::RepoPath;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::{
    StateError, StateResult, WorkspaceStatePaths, ensure_private_directory, ensure_private_file,
    reject_symlink_components,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredArtifact {
    pub relative_path: RepoPath,
    pub sha256: String,
    pub byte_length: u64,
}

#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn open(root: impl Into<PathBuf>) -> StateResult<Self> {
        let root = root.into();
        ensure_private_directory(&root)?;
        let root = fs::canonicalize(&root).map_err(|error| StateError::io(&root, error))?;
        Ok(Self { root })
    }

    /// Opens the artifact namespace rooted at one global workspace. Newly written artifacts use
    /// their ordinary relative paths; read-only legacy imports remain isolated below `imports/`.
    pub fn open_workspace(paths: &WorkspaceStatePaths) -> StateResult<Self> {
        Self::open(&paths.root)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Atomically stores an immutable artifact. Existing identical content is accepted;
    /// existing different content is never overwritten.
    pub fn put(&self, path: RepoPath, contents: &[u8]) -> StateResult<StoredArtifact> {
        let destination = path.join_to(&self.root);
        let parent = destination
            .parent()
            .ok_or_else(|| StateError::UnsafeArtifactPath(destination.display().to_string()))?;
        self.ensure_safe_parent(parent)?;
        ensure_private_directory(parent)?;
        self.ensure_safe_parent(parent)?;

        let digest = hex::encode(Sha256::digest(contents));
        if destination.exists() {
            return Self::verify_existing(path, &destination, &digest, contents.len() as u64);
        }

        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|error| StateError::io(parent, error))?;
        temporary
            .write_all(contents)
            .map_err(|error| StateError::io(temporary.path(), error))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| StateError::io(temporary.path(), error))?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Self::verify_existing(path, &destination, &digest, contents.len() as u64);
            }
            Err(error) => return Err(StateError::io(&destination, error.error)),
        }
        ensure_private_file(&destination)?;
        sync_directory(parent)?;
        Ok(StoredArtifact {
            relative_path: path,
            sha256: digest,
            byte_length: contents.len() as u64,
        })
    }

    pub fn read_verified(&self, artifact: &StoredArtifact) -> StateResult<Vec<u8>> {
        let (path, contents) = self.read_path(&artifact.relative_path)?;
        let digest = hex::encode(Sha256::digest(&contents));
        if digest != artifact.sha256 || contents.len() as u64 != artifact.byte_length {
            return Err(StateError::ArtifactConflict(path));
        }
        Ok(contents)
    }

    /// Reads immutable artifact metadata directly from disk after enforcing containment and
    /// symbolic-link protections. The returned digest can be registered in `SQLite` and later
    /// supplied to [`Self::read_verified`].
    pub fn inspect(&self, relative_path: RepoPath) -> StateResult<StoredArtifact> {
        let (_, contents) = self.read_path(&relative_path)?;
        Ok(StoredArtifact {
            relative_path,
            sha256: hex::encode(Sha256::digest(&contents)),
            byte_length: contents.len() as u64,
        })
    }

    fn verify_existing(
        relative_path: RepoPath,
        destination: &Path,
        digest: &str,
        length: u64,
    ) -> StateResult<StoredArtifact> {
        if fs::symlink_metadata(destination)
            .map_err(|error| StateError::io(destination, error))?
            .file_type()
            .is_symlink()
        {
            return Err(StateError::SymlinkEscape(destination.to_path_buf()));
        }
        let bytes = fs::read(destination).map_err(|error| StateError::io(destination, error))?;
        if bytes.len() as u64 != length || hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(StateError::ArtifactConflict(destination.to_path_buf()));
        }
        Ok(StoredArtifact {
            relative_path,
            sha256: digest.to_owned(),
            byte_length: length,
        })
    }

    fn ensure_safe_parent(&self, parent: &Path) -> StateResult<()> {
        let relative = parent
            .strip_prefix(&self.root)
            .map_err(|_| StateError::UnsafeArtifactPath(parent.display().to_string()))?;
        let mut current = self.root.clone();
        for component in relative.components() {
            current.push(component);
            if current.exists()
                && fs::symlink_metadata(&current)
                    .map_err(|error| StateError::io(&current, error))?
                    .file_type()
                    .is_symlink()
            {
                return Err(StateError::SymlinkEscape(current));
            }
        }
        Ok(())
    }

    fn read_path(&self, relative_path: &RepoPath) -> StateResult<(PathBuf, Vec<u8>)> {
        let path = relative_path.join_to(&self.root);
        self.ensure_safe_parent(path.parent().unwrap_or(&self.root))?;
        if fs::symlink_metadata(&path)
            .map_err(|error| StateError::io(&path, error))?
            .file_type()
            .is_symlink()
        {
            return Err(StateError::SymlinkEscape(path));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|error| StateError::io(&path, error))?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(|error| StateError::io(&path, error))?;
        Ok((path, contents))
    }
}

/// Owns the only writable import directory until both its atomic publication and the matching
/// `SQLite` transaction commit. Dropping an unfinished value removes only that scoped directory.
pub(crate) struct LegacyImportStaging {
    _lock_file: fs::File,
    staging: PathBuf,
    published: PathBuf,
    published_on_disk: bool,
    committed: bool,
}

impl LegacyImportStaging {
    pub(crate) fn acquire(workspace_root: &Path, fingerprint: &str) -> StateResult<Self> {
        validate_fingerprint(fingerprint)?;
        ensure_private_directory(workspace_root)?;
        let imports = workspace_root.join("imports");
        ensure_private_directory(&imports)?;
        reject_symlink_components(&imports)?;
        let lock_path = imports.join(format!("{fingerprint}.lock"));
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| StateError::io(&lock_path, error))?;
        ensure_private_file(&lock_path)?;
        lock_file.try_lock_exclusive().map_err(|error| {
            StateError::RollbackGuard(format!(
                "legacy import {fingerprint} is already active: {error}"
            ))
        })?;
        let staging = imports.join(format!("{fingerprint}.staging"));
        let published = imports.join(fingerprint);
        Ok(Self {
            _lock_file: lock_file,
            staging,
            published,
            published_on_disk: false,
            committed: false,
        })
    }

    pub(crate) fn prepare(&self) -> StateResult<()> {
        remove_orphaned_import_directory(&self.staging)?;
        remove_orphaned_import_directory(&self.published)?;
        fs::create_dir(&self.staging).map_err(|error| StateError::io(&self.staging, error))?;
        ensure_private_directory(&self.staging)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.staging
    }

    pub(crate) fn published_path(&self) -> &Path {
        &self.published
    }

    pub(crate) fn stage_verified_bytes(
        &self,
        relative: &Path,
        bytes: &[u8],
        expected_sha256: &str,
        expected_length: u64,
    ) -> StateResult<()> {
        validate_relative_path(relative)?;
        if bytes.len() as u64 != expected_length
            || hex::encode(Sha256::digest(bytes)) != expected_sha256
        {
            return Err(StateError::ArtifactConflict(relative.to_path_buf()));
        }
        let destination = self.staging.join(relative);
        let parent = destination
            .parent()
            .ok_or_else(|| StateError::UnsafeArtifactPath(destination.display().to_string()))?;
        ensure_private_directory(parent)?;
        reject_symlink_components(parent)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|error| StateError::io(&destination, error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| StateError::io(&destination, error))?;
        Ok(())
    }

    pub(crate) fn publish(&mut self) -> StateResult<()> {
        sync_directory(&self.staging)?;
        fs::rename(&self.staging, &self.published)
            .map_err(|error| StateError::io(&self.published, error))?;
        self.published_on_disk = true;
        if let Some(parent) = self.published.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> PathBuf {
        self.committed = true;
        self.published.clone()
    }
}

fn remove_orphaned_import_directory(path: &Path) -> StateResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| StateError::io(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(StateError::SymlinkEscape(path.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(StateError::ArtifactConflict(path.to_path_buf()));
    }
    fs::remove_dir_all(path).map_err(|error| StateError::io(path, error))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

impl Drop for LegacyImportStaging {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let target = if self.published_on_disk {
            &self.published
        } else {
            &self.staging
        };
        if target.exists() {
            let _ = fs::remove_dir_all(target);
        }
    }
}

fn validate_fingerprint(fingerprint: &str) -> StateResult<()> {
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StateError::InvalidRecord(
            "legacy import fingerprint must be 64 lowercase hexadecimal characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> StateResult<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(StateError::UnsafeArtifactPath(path.display().to_string()));
    }
    Ok(())
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> StateResult<()> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|error| StateError::io(path, error))
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> StateResult<()> {
    let directory = fs::File::open(path).map_err(|error| StateError::io(path, error))?;
    directory
        .sync_all()
        .map_err(|error| StateError::io(path, error))
}

#[cfg(test)]
mod tests {
    use orchestrator_domain::RepoPath;

    use super::ArtifactStore;

    #[test]
    fn artifacts_are_immutable_and_hash_verified() {
        let directory = crate::CanonicalTempDir::new("tempdir")
            .unwrap_or_else(|error| panic!("tempdir: {error}"));
        let store =
            ArtifactStore::open(directory.path()).unwrap_or_else(|error| panic!("store: {error}"));
        let path = RepoPath::try_from("tasks/task-1/diff.patch")
            .unwrap_or_else(|error| panic!("path: {error}"));
        let artifact = store
            .put(path.clone(), b"diff")
            .unwrap_or_else(|error| panic!("put: {error}"));
        assert_eq!(
            store
                .read_verified(&artifact)
                .unwrap_or_else(|error| panic!("read: {error}")),
            b"diff"
        );
        assert!(store.put(path, b"different").is_err());
    }
}
