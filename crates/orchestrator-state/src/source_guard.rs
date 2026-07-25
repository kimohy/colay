use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use same_file::Handle;

use crate::{StateError, StateResult, reject_symlink_components};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComponentKind {
    Directory,
    File,
}

struct GuardedComponent {
    path: PathBuf,
    handle: Handle,
    kind: ComponentKind,
}

/// Owns stable handles for one contained source path and every parent boundary below its root.
pub(crate) struct SourceOpenGuard {
    requested_root: PathBuf,
    requested_path: PathBuf,
    canonical_root: PathBuf,
    canonical_path: PathBuf,
    components: Vec<GuardedComponent>,
}

impl SourceOpenGuard {
    pub(crate) fn open(root: &Path, path: &Path) -> StateResult<Self> {
        if !path.starts_with(root) {
            return Err(StateError::InvalidRecord(format!(
                "legacy source path escapes state root: {}",
                path.display()
            )));
        }
        reject_symlink_components(root)?;
        reject_symlink_components(path)?;
        let canonical_root = fs::canonicalize(root).map_err(|error| StateError::io(root, error))?;
        let canonical_path = fs::canonicalize(path).map_err(|error| StateError::io(path, error))?;
        reject_symlink_components(&canonical_root)?;
        reject_symlink_components(&canonical_path)?;
        if !canonical_path.starts_with(&canonical_root) || canonical_path == canonical_root {
            return Err(StateError::SymlinkEscape(path.to_path_buf()));
        }

        let relative = canonical_path
            .strip_prefix(&canonical_root)
            .map_err(|_| StateError::SymlinkEscape(path.to_path_buf()))?;
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(StateError::InvalidRecord(format!(
                "legacy source path is unsafe: {}",
                path.display()
            )));
        }

        let mut component_paths = vec![(canonical_root.clone(), ComponentKind::Directory)];
        let mut current = canonical_root.clone();
        let relative_components = relative.components().collect::<Vec<_>>();
        for (index, component) in relative_components.iter().enumerate() {
            current.push(component.as_os_str());
            let kind = if index + 1 == relative_components.len() {
                ComponentKind::File
            } else {
                ComponentKind::Directory
            };
            component_paths.push((current.clone(), kind));
        }

        let mut components = Vec::with_capacity(component_paths.len());
        for (component_path, kind) in component_paths {
            components.push(open_component_guard(&component_path, kind)?);
        }
        let guard = Self {
            requested_root: root.to_path_buf(),
            requested_path: path.to_path_buf(),
            canonical_root,
            canonical_path,
            components,
        };
        guard.revalidate()?;
        Ok(guard)
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub(crate) fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub(crate) fn read_all(&self) -> StateResult<Vec<u8>> {
        self.revalidate()?;
        let bytes = self.read_retained()?;
        self.revalidate()?;
        Ok(bytes)
    }

    pub(crate) fn read_retained(&self) -> StateResult<Vec<u8>> {
        let mut file = self
            .final_component()
            .handle
            .as_file()
            .try_clone()
            .map_err(|error| StateError::io(&self.requested_path, error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| StateError::io(&self.requested_path, error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| StateError::io(&self.requested_path, error))?;
        Ok(bytes)
    }

    pub(crate) fn revalidate(&self) -> StateResult<()> {
        reject_symlink_components(&self.requested_root)?;
        reject_symlink_components(&self.requested_path)?;
        let observed_root = fs::canonicalize(&self.requested_root)
            .map_err(|error| StateError::io(&self.requested_root, error))?;
        let observed_path = fs::canonicalize(&self.requested_path)
            .map_err(|error| StateError::io(&self.requested_path, error))?;
        if observed_root != self.canonical_root || observed_path != self.canonical_path {
            return Err(StateError::SymlinkEscape(self.requested_path.clone()));
        }
        for component in &self.components {
            reject_symlink_components(&component.path)?;
            validate_kind(&component.path, component.kind)?;
            let observed = identity_handle(&component.path, component.kind, true)?;
            if observed != component.handle {
                return Err(StateError::SymlinkEscape(self.requested_path.clone()));
            }
        }
        Ok(())
    }

    fn final_component(&self) -> &GuardedComponent {
        // Construction always records the root and at least one final path component.
        &self.components[self.components.len() - 1]
    }
}

fn open_component_guard(path: &Path, kind: ComponentKind) -> StateResult<GuardedComponent> {
    reject_symlink_components(path)?;
    validate_kind(path, kind)?;
    let before = identity_handle(path, kind, true)?;
    run_source_open_hook(SourceOpenHookPhase::BeforeRetainedOpen, path)?;
    let allow_delete_share = source_open_hook_is_set();
    let retained = identity_handle(path, kind, allow_delete_share)?;
    run_source_open_hook(SourceOpenHookPhase::BeforePostOpenCheck, path)?;
    reject_symlink_components(path)?;
    validate_kind(path, kind)?;
    let after = identity_handle(path, kind, true)?;
    if before != retained || retained != after {
        return Err(StateError::SymlinkEscape(path.to_path_buf()));
    }
    Ok(GuardedComponent {
        path: path.to_path_buf(),
        handle: retained,
        kind,
    })
}

fn validate_kind(path: &Path, expected: ComponentKind) -> StateResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| StateError::io(path, error))?;
    let valid = match expected {
        ComponentKind::Directory => metadata.is_dir(),
        ComponentKind::File => metadata.is_file(),
    };
    if metadata.file_type().is_symlink() || !valid {
        return Err(StateError::InvalidRecord(format!(
            "legacy source has an unexpected file type: {}",
            path.display()
        )));
    }
    Ok(())
}

fn identity_handle(
    path: &Path,
    kind: ComponentKind,
    allow_delete_share: bool,
) -> StateResult<Handle> {
    let file = open_identity_file(path, kind, allow_delete_share)
        .map_err(|error| StateError::io(path, error))?;
    Handle::from_file(file).map_err(|error| StateError::io(path, error))
}

#[cfg(not(windows))]
fn open_identity_file(
    path: &Path,
    _kind: ComponentKind,
    _allow_delete_share: bool,
) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(windows)]
fn open_identity_file(
    path: &Path,
    kind: ComponentKind,
    allow_delete_share: bool,
) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    let mut options = OpenOptions::new();
    options.read(true).share_mode(
        FILE_SHARE_READ
            | FILE_SHARE_WRITE
            | if allow_delete_share {
                FILE_SHARE_DELETE
            } else {
                0
            },
    );
    if kind == ComponentKind::Directory {
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceOpenHookPhase {
    BeforeRetainedOpen,
    BeforePostOpenCheck,
}

#[cfg(test)]
type SourceOpenHook = Box<dyn FnMut(SourceOpenHookPhase, &Path) -> StateResult<()> + 'static>;

#[cfg(test)]
thread_local! {
    static SOURCE_OPEN_HOOK: std::cell::RefCell<Option<SourceOpenHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_source_open_hook(
    hook: impl FnMut(SourceOpenHookPhase, &Path) -> StateResult<()> + 'static,
) {
    SOURCE_OPEN_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
pub(crate) fn clear_source_open_hook() {
    SOURCE_OPEN_HOOK.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn source_open_hook_is_set() -> bool {
    SOURCE_OPEN_HOOK.with(|slot| slot.borrow().is_some())
}

#[cfg(not(test))]
fn source_open_hook_is_set() -> bool {
    false
}

#[cfg(test)]
fn run_source_open_hook(phase: SourceOpenHookPhase, path: &Path) -> StateResult<()> {
    SOURCE_OPEN_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(hook) = slot.as_mut() {
            hook(phase, path)?;
        }
        Ok(())
    })
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)]
fn run_source_open_hook(_phase: SourceOpenHookPhase, _path: &Path) -> StateResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use crate::StateError;

    use super::{
        SourceOpenGuard, SourceOpenHookPhase, clear_source_open_hook, set_source_open_hook,
    };

    #[test]
    fn source_parent_aba_is_rejected_and_restored() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("source");
        let parent = root.join("evidence");
        let saved = root.join("evidence-saved");
        let alternate = root.join("evidence-alternate");
        fs::create_dir_all(&parent)?;
        fs::create_dir_all(&alternate)?;
        fs::write(parent.join("artifact.txt"), b"original")?;
        fs::write(alternate.join("artifact.txt"), b"alternate")?;

        let parent_for_hook = parent.clone();
        let saved_for_hook = saved.clone();
        let alternate_for_hook = alternate.clone();
        let phases = Arc::new(Mutex::new(Vec::new()));
        let phases_for_hook = Arc::clone(&phases);
        set_source_open_hook(move |phase, path| {
            if path.file_name() != parent_for_hook.file_name() {
                return Ok(());
            }
            phases_for_hook
                .lock()
                .map_err(|_| StateError::InvalidRecord("source hook lock poisoned".to_owned()))?
                .push(phase);
            match phase {
                SourceOpenHookPhase::BeforeRetainedOpen => {
                    fs::rename(&parent_for_hook, &saved_for_hook)
                        .map_err(|error| StateError::io(&parent_for_hook, error))?;
                    fs::rename(&alternate_for_hook, &parent_for_hook)
                        .map_err(|error| StateError::io(&parent_for_hook, error))?;
                }
                SourceOpenHookPhase::BeforePostOpenCheck => {
                    fs::rename(&parent_for_hook, &alternate_for_hook)
                        .map_err(|error| StateError::io(&parent_for_hook, error))?;
                    fs::rename(&saved_for_hook, &parent_for_hook)
                        .map_err(|error| StateError::io(&parent_for_hook, error))?;
                }
            }
            Ok(())
        });

        let result = SourceOpenGuard::open(&root, &parent.join("artifact.txt"));
        clear_source_open_hook();

        assert!(result.is_err());
        assert_eq!(fs::read(parent.join("artifact.txt"))?, b"original");
        assert_eq!(fs::read(alternate.join("artifact.txt"))?, b"alternate");
        assert_eq!(
            *phases.lock().map_err(|_| "source hook lock poisoned")?,
            vec![
                SourceOpenHookPhase::BeforeRetainedOpen,
                SourceOpenHookPhase::BeforePostOpenCheck,
            ]
        );
        assert!(!saved.exists());
        Ok(())
    }
}
