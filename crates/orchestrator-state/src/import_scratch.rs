use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

use fs2::FileExt as _;

use crate::{
    GlobalStatePaths, StateError, StateResult, ensure_private_directory, ensure_private_file,
    reject_symlink_components,
};

const SCRATCH_DIRECTORY: &str = "import-scratch";
const FINGERPRINT_LOCK: &str = "import.lock";
const OWNER_LOCK: &str = "owner.lock";
const ATTEMPT_PREFIX: &str = "attempt-";

/// Owns the per-source and per-attempt locks for recoverable legacy-import scratch.
pub(crate) struct LegacyImportScratch {
    _fingerprint_lock: File,
    owner_lock: File,
    fingerprint_root: PathBuf,
    attempt: PathBuf,
}

impl LegacyImportScratch {
    pub(crate) fn acquire(paths: &GlobalStatePaths, fingerprint: &str) -> StateResult<Self> {
        validate_fingerprint(fingerprint)?;
        let database_parent = paths.database.parent().ok_or_else(|| {
            StateError::InvalidConfig(format!(
                "global database has no parent: {}",
                paths.database.display()
            ))
        })?;
        ensure_private_directory(database_parent)?;
        let scratch_root = database_parent.join(SCRATCH_DIRECTORY);
        ensure_private_directory(&scratch_root)?;
        validate_exact_child(database_parent, &scratch_root)?;
        let fingerprint_root = scratch_root.join(fingerprint);
        ensure_private_directory(&fingerprint_root)?;
        validate_exact_child(&scratch_root, &fingerprint_root)?;

        let fingerprint_lock_path = fingerprint_root.join(FINGERPRINT_LOCK);
        let fingerprint_lock = open_lock(&fingerprint_lock_path, false)?;
        fingerprint_lock.try_lock_exclusive().map_err(|error| {
            StateError::RollbackGuard(format!(
                "legacy import scratch {fingerprint} is already active: {error}"
            ))
        })?;
        scavenge_stale_attempts(&fingerprint_root)?;

        let attempt = create_attempt_directory(&fingerprint_root)?;
        let owner_lock_path = attempt.join(OWNER_LOCK);
        let owner_lock = open_lock(&owner_lock_path, true)?;
        owner_lock.try_lock_exclusive().map_err(|error| {
            StateError::RollbackGuard(format!(
                "legacy import scratch attempt {} could not be owned: {error}",
                attempt.display()
            ))
        })?;
        Ok(Self {
            _fingerprint_lock: fingerprint_lock,
            owner_lock,
            fingerprint_root,
            attempt,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.attempt
    }
}

impl Drop for LegacyImportScratch {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.owner_lock);
        let _ = remove_owned_attempt(&self.fingerprint_root, &self.attempt);
    }
}

fn validate_fingerprint(fingerprint: &str) -> StateResult<()> {
    if fingerprint.len() != 64
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StateError::InvalidRecord(
            "legacy import scratch fingerprint must be 64 lowercase hexadecimal characters"
                .to_owned(),
        ));
    }
    Ok(())
}

fn open_lock(path: &Path, create_new: bool) -> StateResult<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| StateError::io(path, error))?;
    validate_open_lock(path, &file)?;
    if create_new {
        ensure_private_file(path)?;
    }
    Ok(file)
}

#[cfg(not(windows))]
fn validate_open_lock(path: &Path, file: &File) -> StateResult<()> {
    reject_symlink_components(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| StateError::io(path, error))?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(StateError::ArtifactConflict(path.to_path_buf()))
    }
}

#[cfg(windows)]
fn validate_open_lock(path: &Path, file: &File) -> StateResult<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    let metadata = file
        .metadata()
        .map_err(|error| StateError::io(path, error))?;
    if metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Ok(())
    } else {
        Err(StateError::ArtifactConflict(path.to_path_buf()))
    }
}

fn create_attempt_directory(fingerprint_root: &Path) -> StateResult<PathBuf> {
    for _ in 0..8 {
        let attempt = fingerprint_root.join(format!("{ATTEMPT_PREFIX}{}", uuid::Uuid::now_v7()));
        match fs::create_dir(&attempt) {
            Ok(()) => {
                ensure_private_directory(&attempt)?;
                validate_exact_child(fingerprint_root, &attempt)?;
                return Ok(attempt);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(StateError::io(&attempt, error)),
        }
    }
    Err(StateError::ArtifactConflict(fingerprint_root.to_path_buf()))
}

fn scavenge_stale_attempts(fingerprint_root: &Path) -> StateResult<()> {
    let mut entries = fs::read_dir(fingerprint_root)
        .map_err(|error| StateError::io(fingerprint_root, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| StateError::io(fingerprint_root, error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if name == FINGERPRINT_LOCK {
            continue;
        }
        let name_text = name.to_string_lossy();
        if !valid_attempt_name(&name_text) {
            return Err(StateError::ArtifactConflict(entry.path()));
        }
        let attempt = entry.path();
        validate_exact_child(fingerprint_root, &attempt)?;
        let metadata =
            fs::symlink_metadata(&attempt).map_err(|error| StateError::io(&attempt, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StateError::ArtifactConflict(attempt));
        }
        let owner_lock_path = attempt.join(OWNER_LOCK);
        let owner_lock = match if owner_lock_path.exists() {
            open_lock(&owner_lock_path, false)
        } else {
            open_lock(&owner_lock_path, true)
        } {
            Ok(lock) => lock,
            Err(StateError::Io { source, .. }) if owner_lock_is_active(&source) => continue,
            Err(error) => return Err(error),
        };
        match owner_lock.try_lock_exclusive() {
            Ok(()) => {
                fs2::FileExt::unlock(&owner_lock)
                    .map_err(|error| StateError::io(&owner_lock_path, error))?;
                drop(owner_lock);
                remove_owned_attempt(fingerprint_root, &attempt)?;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock || owner_lock_is_active(&error) => {
            }
            Err(error) => return Err(StateError::io(&owner_lock_path, error)),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn owner_lock_is_active(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(not(windows))]
fn owner_lock_is_active(_error: &std::io::Error) -> bool {
    false
}

fn valid_attempt_name(name: &str) -> bool {
    name.strip_prefix(ATTEMPT_PREFIX)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .is_some()
}

fn remove_owned_attempt(fingerprint_root: &Path, attempt: &Path) -> StateResult<()> {
    if attempt.parent() != Some(fingerprint_root)
        || !attempt
            .file_name()
            .is_some_and(|name| valid_attempt_name(&name.to_string_lossy()))
    {
        return Err(StateError::ArtifactConflict(attempt.to_path_buf()));
    }
    if !attempt.exists() {
        return Ok(());
    }
    validate_exact_child(fingerprint_root, attempt)?;
    validate_scratch_tree(attempt)?;
    fs::remove_dir_all(attempt).map_err(|error| StateError::io(attempt, error))
}

fn validate_scratch_tree(directory: &Path) -> StateResult<()> {
    reject_symlink_components(directory)?;
    let metadata =
        fs::symlink_metadata(directory).map_err(|error| StateError::io(directory, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StateError::ArtifactConflict(directory.to_path_buf()));
    }
    for entry in fs::read_dir(directory).map_err(|error| StateError::io(directory, error))? {
        let entry = entry.map_err(|error| StateError::io(directory, error))?;
        let path = entry.path();
        reject_symlink_components(&path)?;
        let metadata = fs::symlink_metadata(&path).map_err(|error| StateError::io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(StateError::ArtifactConflict(path));
        }
        if metadata.is_dir() {
            validate_scratch_tree(&path)?;
        } else if !metadata.is_file() {
            return Err(StateError::ArtifactConflict(path));
        }
    }
    Ok(())
}

fn validate_exact_child(parent: &Path, child: &Path) -> StateResult<()> {
    reject_symlink_components(parent)?;
    reject_symlink_components(child)?;
    if child.parent() != Some(parent) {
        return Err(StateError::ArtifactConflict(child.to_path_buf()));
    }
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| StateError::io(parent, error))?;
    let canonical_child = fs::canonicalize(child).map_err(|error| StateError::io(child, error))?;
    if canonical_child.parent() != Some(canonical_parent.as_path()) {
        return Err(StateError::ArtifactConflict(child.to_path_buf()));
    }
    Ok(())
}
