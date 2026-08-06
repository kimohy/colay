use std::{
    fs,
    path::{Component, Path, PathBuf},
};

#[cfg(windows)]
use orchestrator_windows_ipc::StateArtifactKind;

use crate::{StateError, StateResult};

/// Creates a directory when necessary and restricts it to the current identity and
/// operating-system administrators.
pub fn ensure_private_directory(path: &Path) -> StateResult<()> {
    reject_symlink_components(path)?;
    fs::create_dir_all(path).map_err(|error| StateError::io(path, error))?;
    reject_symlink_components(path)?;
    set_directory_permissions(path)
}

/// Restricts a file to the current identity and operating-system administrators.
pub fn ensure_private_file(path: &Path) -> StateResult<()> {
    reject_symlink_components(path)?;
    set_file_permissions(path)
}

/// Verifies that an existing input file is private without changing its access policy.
pub fn verify_private_file(path: &Path) -> StateResult<()> {
    reject_symlink_components(path)?;
    let metadata = fs::metadata(path).map_err(|error| StateError::io(path, error))?;
    if !metadata.is_file() {
        return Err(StateError::InvalidRecord(format!(
            "private input is not a regular file: {}",
            path.display()
        )));
    }
    verify_file_permissions(path, &metadata)
}

/// Rejects every existing symbolic-link component in a state path. Missing trailing
/// components are allowed so callers can validate paths before creation.
pub fn reject_symlink_components(path: &Path) -> StateResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(StateError::UnsafeArtifactPath(path.display().to_string()));
            }
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if is_link_like(&metadata) => {
                        return Err(StateError::SymlinkEscape(current));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(StateError::io(&current, error)),
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    // Junctions and other reparse points can redirect ACL operations even when Rust
    // does not classify them as symbolic links.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> StateResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| StateError::io(path, error))
}

#[cfg(windows)]
fn set_directory_permissions(path: &Path) -> StateResult<()> {
    set_windows_permissions(path, StateArtifactKind::Directory)
}

#[cfg(all(not(unix), not(windows)))]
fn set_directory_permissions(path: &Path) -> StateResult<()> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|error| StateError::io(path, error))
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> StateResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| StateError::io(path, error))
}

#[cfg(windows)]
fn set_file_permissions(path: &Path) -> StateResult<()> {
    set_windows_permissions(path, StateArtifactKind::File)
}

#[cfg(all(not(unix), not(windows)))]
fn set_file_permissions(path: &Path) -> StateResult<()> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|error| StateError::io(path, error))
}

#[cfg(unix)]
fn verify_file_permissions(path: &Path, metadata: &fs::Metadata) -> StateResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    if metadata.permissions().mode().trailing_zeros() >= 6 {
        Ok(())
    } else {
        Err(StateError::InvalidRecord(format!(
            "private input is readable or writable by group/other users: {}",
            path.display()
        )))
    }
}

#[cfg(windows)]
fn verify_file_permissions(path: &Path, _metadata: &fs::Metadata) -> StateResult<()> {
    let _acl_guard = windows_acl_guard()?;
    let target = canonical_acl_target(path)?;
    orchestrator_windows_ipc::verify_private_state_artifact(&target, StateArtifactKind::File)
        .map_err(|error| StateError::io(&target, error))
}

#[cfg(all(not(unix), not(windows)))]
fn verify_file_permissions(_path: &Path, _metadata: &fs::Metadata) -> StateResult<()> {
    Ok(())
}

#[cfg(windows)]
fn set_windows_permissions(path: &Path, kind: StateArtifactKind) -> StateResult<()> {
    // Preserve the state-layer lock while the native boundary pins, checks, and, if needed,
    // repairs the target through one retained handle.
    let _acl_guard = windows_acl_guard()?;
    let target = canonical_acl_target(path)?;
    orchestrator_windows_ipc::ensure_private_state_artifact(&target, kind)
        .map_err(|error| StateError::io(&target, error))
}

#[cfg(all(windows, test))]
std::thread_local! {
    static WINDOWS_ACL_GUARD_ACQUISITIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(windows)]
fn windows_acl_guard() -> StateResult<std::sync::MutexGuard<'static, ()>> {
    use std::sync::{Mutex, OnceLock};

    static WINDOWS_ACL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = WINDOWS_ACL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| permission_error("Windows ACL hardening lock was poisoned"))?;
    #[cfg(test)]
    WINDOWS_ACL_GUARD_ACQUISITIONS.with(|count| count.set(count.get() + 1));
    Ok(guard)
}

#[cfg(all(windows, test))]
fn reset_windows_acl_guard_acquisitions_for_test() {
    WINDOWS_ACL_GUARD_ACQUISITIONS.with(|count| count.set(0));
}

#[cfg(all(windows, test))]
fn windows_acl_guard_acquisitions_for_test() -> usize {
    WINDOWS_ACL_GUARD_ACQUISITIONS.with(std::cell::Cell::get)
}

#[cfg(windows)]
fn canonical_acl_target(path: &Path) -> StateResult<PathBuf> {
    reject_symlink_components(path)?;
    let target = fs::canonicalize(path).map_err(|error| StateError::io(path, error))?;
    reject_symlink_components(&target)?;
    let metadata = fs::symlink_metadata(&target).map_err(|error| StateError::io(&target, error))?;
    if is_link_like(&metadata) {
        return Err(StateError::SymlinkEscape(target));
    }
    Ok(target)
}

#[cfg(windows)]
pub fn current_windows_user_sid() -> StateResult<String> {
    orchestrator_windows_ipc::current_process_user_sid()
        .map_err(|error| StateError::io(Path::new("Windows process primary token"), error))
}

#[cfg(windows)]
fn permission_error(message: impl Into<String>) -> StateError {
    StateError::InvalidRecord(format!(
        "Windows permission hardening failed: {}",
        message.into()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_private_permissions_remain_owner_only() -> StateResult<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let directory = temporary.path().join("state");
        ensure_private_directory(&directory)?;
        let file = directory.join("state.json");
        fs::write(&file, b"{}\n").map_err(|error| StateError::io(&file, error))?;
        ensure_private_file(&file)?;
        verify_private_file(&file)?;

        let directory_mode = fs::metadata(&directory)
            .map_err(|error| StateError::io(&directory, error))?
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(&file)
            .map_err(|error| StateError::io(&file, error))?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_wrong_kind_preserves_canonical_path_and_native_stage() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let file = temporary.path().join("state.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;
        let canonical = fs::canonicalize(&file).map_err(|error| StateError::io(&file, error))?;

        let Err(error) = set_windows_permissions(&file, StateArtifactKind::Directory) else {
            panic!("regular file was accepted as a private directory");
        };
        let StateError::Io { path, source } = error else {
            panic!("wrong artifact kind did not preserve the native I/O error");
        };

        assert_eq!(path, canonical);
        assert!(source.to_string().contains("object-kind validation"));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_state_sid_matches_native_process_sid() -> StateResult<()> {
        let native = orchestrator_windows_ipc::current_process_user_sid()
            .map_err(|error| StateError::io("Windows process primary token", error))?;
        assert_eq!(current_windows_user_sid()?, native);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_parent_traversal_is_rejected_before_acl_hardening() {
        let path = Path::new(r"C:\state\..\escape");
        assert!(matches!(
            ensure_private_file(path),
            Err(StateError::UnsafeArtifactPath(rendered)) if rendered == path.display().to_string()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_link_is_rejected_before_acl_hardening() -> StateResult<()> {
        use std::os::windows::fs::symlink_file;

        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let target = temporary.path().join("target.json");
        let link = temporary.path().join("link.json");
        fs::write(&target, b"{}\r\n").map_err(|error| StateError::io(&target, error))?;
        symlink_file(&target, &link).map_err(|error| StateError::io(&link, error))?;

        assert!(matches!(
            ensure_private_file(&link),
            Err(StateError::SymlinkEscape(path)) if path == link
        ));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_file_and_directory_hardening_is_idempotent() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let directory = temporary.path().join("state");
        ensure_private_directory(&directory)?;
        ensure_private_directory(&directory)?;

        let file = directory.join("state.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;
        ensure_private_file(&file)?;
        ensure_private_file(&file)?;
        verify_private_file(&file)
    }

    #[cfg(windows)]
    #[test]
    fn windows_ensure_and_verify_acquire_the_shared_state_acl_gate() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let file = temporary.path().join("state.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;
        ensure_private_file(&file)?;

        reset_windows_acl_guard_acquisitions_for_test();
        ensure_private_file(&file)?;
        assert_eq!(windows_acl_guard_acquisitions_for_test(), 1);
        verify_private_file(&file)?;
        assert_eq!(windows_acl_guard_acquisitions_for_test(), 2);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_public_file_api_repairs_permissive_dacl() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let file = temporary.path().join("permissive.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;
        orchestrator_windows_ipc::test_support::install_permissive_state_artifact_dacl(
            &file,
            StateArtifactKind::File,
        )
        .map_err(|error| StateError::io(&file, error))?;

        assert!(verify_private_file(&file).is_err());
        ensure_private_file(&file)?;
        verify_private_file(&file)
    }

    #[cfg(windows)]
    #[test]
    fn windows_public_file_api_repairs_deny_containing_dacl() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let file = temporary.path().join("deny.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;
        orchestrator_windows_ipc::test_support::install_deny_containing_state_artifact_dacl(
            &file,
            StateArtifactKind::File,
        )
        .map_err(|error| StateError::io(&file, error))?;

        assert!(verify_private_file(&file).is_err());
        ensure_private_file(&file)?;
        verify_private_file(&file)
    }

    #[cfg(windows)]
    #[test]
    fn windows_public_file_api_preserves_native_access_denied_error() -> StateResult<()> {
        let temporary = crate::CanonicalTempDir::new("tempdir")?;
        let file = temporary.path().join("access-denied.json");
        fs::write(&file, b"{}\r\n").map_err(|error| StateError::io(&file, error))?;
        ensure_private_file(&file)?;
        let canonical = fs::canonicalize(&file).map_err(|error| StateError::io(&file, error))?;
        let mut denied = orchestrator_windows_ipc::test_support::deny_state_artifact_acl_access(
            &file,
            StateArtifactKind::File,
        )
        .map_err(|error| StateError::io(&file, error))?;

        let Err(error) = ensure_private_file(&file) else {
            panic!("state hardening unexpectedly opened an ACL-denied artifact");
        };
        let StateError::Io { path, source } = error else {
            panic!("native access denial did not remain a path-aware I/O error");
        };
        assert_eq!(path, canonical);
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(source.to_string().contains("target open"), "{source}");
        assert!(source.to_string().contains("os error 5"), "{source}");
        let mut error: &(dyn std::error::Error + 'static) = &source;
        let native_code_is_preserved = loop {
            if error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error)
                == Some(5)
            {
                break true;
            }
            let Some(next) = error.source() else {
                break false;
            };
            error = next;
        };
        assert!(
            native_code_is_preserved,
            "native access-denied code was absent from the structured error chain"
        );

        denied
            .restore()
            .map_err(|error| StateError::io(&file, error))?;
        verify_private_file(&file)
    }
}
