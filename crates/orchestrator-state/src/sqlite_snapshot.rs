use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
};

use crate::{StateError, StateResult, reject_symlink_components, source_guard::SourceOpenGuard};

const SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

struct GuardedMember {
    suffix: &'static str,
    guard: SourceOpenGuard,
}

#[derive(PartialEq, Eq)]
struct CapturedMember {
    suffix: &'static str,
    bytes: Vec<u8>,
}

/// Owns retained handles for one `SQLite` main database and its exact sidecar set.
pub(crate) struct GuardedSqliteFamily {
    database: PathBuf,
    main: SourceOpenGuard,
    sidecars: Vec<GuardedMember>,
}

impl GuardedSqliteFamily {
    pub(crate) fn open(root: &Path, database: &Path) -> StateResult<Self> {
        let main = SourceOpenGuard::open(root, database)?;
        let suffixes = present_sidecars(database)?;
        let mut sidecars = Vec::with_capacity(suffixes.len());
        for suffix in suffixes {
            sidecars.push(GuardedMember {
                suffix,
                guard: SourceOpenGuard::open(root, &sidecar_path(database, suffix)?)?,
            });
        }
        let family = Self {
            database: database.to_path_buf(),
            main,
            sidecars,
        };
        family.revalidate()?;
        Ok(family)
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        self.main.canonical_root()
    }

    pub(crate) fn canonical_database(&self) -> &Path {
        self.main.canonical_path()
    }

    pub(crate) fn capture(&self, destination_main: &Path) -> StateResult<()> {
        self.revalidate()?;
        run_snapshot_capture_hook(SnapshotCaptureHookPhase::BeforeSnapshotCopy, &self.database)?;
        let first = self.read_pass();
        let restore =
            run_snapshot_capture_hook(SnapshotCaptureHookPhase::AfterSnapshotCopy, &self.database);
        let first = first?;
        restore?;
        self.revalidate()?;
        let second = self.read_pass()?;
        self.revalidate()?;
        if first != second {
            return Err(StateError::RollbackGuard(
                "legacy SQLite source family changed while its retained snapshot was captured"
                    .to_owned(),
            ));
        }
        write_captured_family(destination_main, &first)
    }

    fn revalidate(&self) -> StateResult<()> {
        self.main.revalidate()?;
        for sidecar in &self.sidecars {
            sidecar.guard.revalidate()?;
        }
        let observed = present_sidecars(&self.database)?;
        let expected = self
            .sidecars
            .iter()
            .map(|sidecar| sidecar.suffix)
            .collect::<Vec<_>>();
        if observed != expected {
            return Err(StateError::RollbackGuard(
                "legacy SQLite sidecar set changed while its retained snapshot was captured"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn read_pass(&self) -> StateResult<Vec<CapturedMember>> {
        let mut captured = Vec::with_capacity(self.sidecars.len() + 1);
        captured.push(CapturedMember {
            suffix: "",
            bytes: self.main.read_retained()?,
        });
        for sidecar in &self.sidecars {
            captured.push(CapturedMember {
                suffix: sidecar.suffix,
                bytes: sidecar.guard.read_retained()?,
            });
        }
        Ok(captured)
    }
}

fn present_sidecars(database: &Path) -> StateResult<Vec<&'static str>> {
    let mut present = Vec::new();
    for suffix in SIDECAR_SUFFIXES {
        let path = sidecar_path(database, suffix)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                reject_symlink_components(&path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(StateError::InvalidRecord(format!(
                        "legacy SQLite sidecar has an unexpected file type: {}",
                        path.display()
                    )));
                }
                present.push(suffix);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(StateError::io(&path, error)),
        }
    }
    Ok(present)
}

fn sidecar_path(database: &Path, suffix: &str) -> StateResult<PathBuf> {
    let mut file_name = database
        .file_name()
        .ok_or_else(|| {
            StateError::InvalidRecord("legacy database path has no file name".to_owned())
        })?
        .to_os_string();
    file_name.push(suffix);
    Ok(database.with_file_name(file_name))
}

fn write_captured_family(destination_main: &Path, captured: &[CapturedMember]) -> StateResult<()> {
    for member in captured {
        let path = sidecar_path(destination_main, member.suffix)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| StateError::io(&path, error))?;
        file.write_all(&member.bytes)
            .map_err(|error| StateError::io(&path, error))?;
        file.sync_all()
            .map_err(|error| StateError::io(&path, error))?;
        drop(file);
        verify_captured_file_privacy(&path)?;
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)]
fn verify_captured_file_privacy(_path: &Path) -> StateResult<()> {
    // `LegacyImportScratch` creates this new child below an already-hardened attempt directory.
    // Its protected DACL exposes inheritable file ACEs only to the current identity, SYSTEM, and
    // administrators, so a create-new child is private without another external ACL rewrite.
    Ok(())
}

#[cfg(not(windows))]
fn verify_captured_file_privacy(path: &Path) -> StateResult<()> {
    crate::verify_private_file(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotCaptureHookPhase {
    BeforeSnapshotCopy,
    AfterSnapshotCopy,
}

#[cfg(test)]
type SnapshotCaptureHook =
    Box<dyn FnMut(SnapshotCaptureHookPhase, &Path) -> StateResult<()> + 'static>;

#[cfg(test)]
thread_local! {
    static SNAPSHOT_CAPTURE_HOOK: std::cell::RefCell<Option<SnapshotCaptureHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_snapshot_capture_hook(
    hook: impl FnMut(SnapshotCaptureHookPhase, &Path) -> StateResult<()> + 'static,
) {
    SNAPSHOT_CAPTURE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
pub(crate) fn clear_snapshot_capture_hook() {
    SNAPSHOT_CAPTURE_HOOK.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn run_snapshot_capture_hook(phase: SnapshotCaptureHookPhase, database: &Path) -> StateResult<()> {
    SNAPSHOT_CAPTURE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(phase, database)?;
        }
        Ok(())
    })
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)]
fn run_snapshot_capture_hook(
    _phase: SnapshotCaptureHookPhase,
    _database: &Path,
) -> StateResult<()> {
    Ok(())
}
