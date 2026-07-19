use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutablePlatform {
    Windows,
    Unix,
}

impl ExecutablePlatform {
    pub(crate) const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableKind {
    Native,
    CommandScript,
}

#[derive(Clone, Debug)]
pub struct ExecutableSearch {
    pub platform: ExecutablePlatform,
    pub path: Vec<PathBuf>,
    pub pathext: Vec<OsString>,
    pub working_directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedExecutable {
    pub configured: PathBuf,
    pub path: PathBuf,
    pub kind: ExecutableKind,
}

#[derive(Debug, Error)]
pub enum ExecutableResolutionError {
    #[error("configured executable `{configured}` does not exist at explicit path `{resolved}`")]
    ExplicitMissing {
        configured: PathBuf,
        resolved: PathBuf,
    },
    #[error("configured executable `{configured}` is not a regular file at `{resolved}`")]
    ExplicitNotFile {
        configured: PathBuf,
        resolved: PathBuf,
    },
    #[error("configured executable `{configured}` is not executable at `{resolved}`")]
    ExplicitNotExecutable {
        configured: PathBuf,
        resolved: PathBuf,
    },
    #[error("configured executable `{configured}` was not found on the effective PATH")]
    NotFound {
        configured: PathBuf,
        search_path: Vec<PathBuf>,
    },
}

pub fn resolve_executable(
    configured: &Path,
    search: &ExecutableSearch,
) -> Result<ResolvedExecutable, ExecutableResolutionError> {
    if is_explicit(configured) {
        return resolve_explicit(configured, search);
    }

    match search.platform {
        ExecutablePlatform::Windows => resolve_windows_bare(configured, search),
        ExecutablePlatform::Unix => resolve_unix_bare(configured, search),
    }
}

fn is_explicit(configured: &Path) -> bool {
    configured.is_absolute()
        || configured
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || configured.components().count() > 1
}

fn resolve_explicit(
    configured: &Path,
    search: &ExecutableSearch,
) -> Result<ResolvedExecutable, ExecutableResolutionError> {
    let resolved = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        search.working_directory.join(configured)
    };
    let metadata = match fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ExecutableResolutionError::ExplicitMissing {
                configured: configured.to_path_buf(),
                resolved,
            });
        }
        Err(_) => {
            return Err(ExecutableResolutionError::ExplicitNotFile {
                configured: configured.to_path_buf(),
                resolved,
            });
        }
    };
    if !metadata.is_file() {
        return Err(ExecutableResolutionError::ExplicitNotFile {
            configured: configured.to_path_buf(),
            resolved,
        });
    }
    if search.platform == ExecutablePlatform::Unix && !unix_executable(&metadata) {
        return Err(ExecutableResolutionError::ExplicitNotExecutable {
            configured: configured.to_path_buf(),
            resolved,
        });
    }
    let kind = executable_kind(&resolved, search.platform);
    Ok(ResolvedExecutable {
        configured: configured.to_path_buf(),
        path: resolved,
        kind,
    })
}

fn resolve_windows_bare(
    configured: &Path,
    search: &ExecutableSearch,
) -> Result<ResolvedExecutable, ExecutableResolutionError> {
    let extensions = allowed_windows_extensions(&search.pathext);
    let configured_extension = configured.extension().and_then(OsStr::to_str);

    for directory in &search.path {
        if let Some(extension) = configured_extension {
            let extension = format!(".{extension}");
            if extensions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&extension))
                && let Some(path) = windows_regular_file(directory, configured.as_os_str())
            {
                return Ok(ResolvedExecutable {
                    configured: configured.to_path_buf(),
                    kind: executable_kind(&path, search.platform),
                    path,
                });
            }
            continue;
        }

        for extension in &extensions {
            let mut candidate = configured.as_os_str().to_os_string();
            candidate.push(extension);
            if let Some(path) = windows_regular_file(directory, &candidate) {
                return Ok(ResolvedExecutable {
                    configured: configured.to_path_buf(),
                    kind: executable_kind(&path, search.platform),
                    path,
                });
            }
        }
    }

    Err(ExecutableResolutionError::NotFound {
        configured: configured.to_path_buf(),
        search_path: search.path.clone(),
    })
}

fn resolve_unix_bare(
    configured: &Path,
    search: &ExecutableSearch,
) -> Result<ResolvedExecutable, ExecutableResolutionError> {
    for directory in &search.path {
        let candidate = directory.join(configured);
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
            && unix_executable(&metadata)
        {
            return Ok(ResolvedExecutable {
                configured: configured.to_path_buf(),
                path: candidate,
                kind: ExecutableKind::Native,
            });
        }
    }
    Err(ExecutableResolutionError::NotFound {
        configured: configured.to_path_buf(),
        search_path: search.path.clone(),
    })
}

fn allowed_windows_extensions(pathext: &[OsString]) -> Vec<String> {
    pathext
        .iter()
        .filter_map(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|extension| matches!(extension.as_str(), ".exe" | ".com" | ".cmd" | ".bat"))
        .collect()
}

fn windows_regular_file(directory: &Path, filename: &OsStr) -> Option<PathBuf> {
    let expected = filename.to_string_lossy();
    let mut matches = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(&expected)
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    matches.sort();
    matches.into_iter().next()
}

fn executable_kind(path: &Path, platform: ExecutablePlatform) -> ExecutableKind {
    if platform == ExecutablePlatform::Windows
        && path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            })
    {
        ExecutableKind::CommandScript
    } else {
        ExecutableKind::Native
    }
}

#[cfg(unix)]
fn unix_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn unix_executable(_metadata: &fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::*;

    struct SearchFixture {
        root: TempDir,
    }

    impl SearchFixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir()
                    .unwrap_or_else(|error| panic!("create resolver fixture: {error}")),
            }
        }

        fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.root.path().join(relative)
        }

        fn write_bytes(&self, relative: impl AsRef<Path>, bytes: &[u8]) {
            let path = self.path(relative);
            let parent = path
                .parent()
                .unwrap_or_else(|| panic!("fixture file has no parent: {}", path.display()));
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create fixture parent: {error}"));
            fs::write(path, bytes)
                .unwrap_or_else(|error| panic!("write fixture executable: {error}"));
        }

        fn search<const N: usize>(
            &self,
            platform: ExecutablePlatform,
            pathext: &str,
            directories: [&str; N],
        ) -> ExecutableSearch {
            ExecutableSearch {
                platform,
                path: directories.map(|directory| self.path(directory)).into(),
                pathext: pathext.split(';').map(OsString::from).collect(),
                working_directory: self.root.path().to_path_buf(),
            }
        }

        fn windows_search(&self) -> ExecutableSearch {
            self.search(ExecutablePlatform::Windows, ".COM;.EXE;.BAT;.CMD", ["bin"])
        }
    }

    #[test]
    fn windows_skips_extensionless_foreign_binary_and_selects_cmd() {
        let fixture = SearchFixture::new();
        fixture.write_bytes("first/codex", b"\x7fELF");
        fixture.write_bytes("second/codex.cmd", b"@echo off\r\n");
        let search = fixture.search(
            ExecutablePlatform::Windows,
            ".COM;.EXE;.BAT;.CMD",
            ["first", "second"],
        );

        let resolved = resolve_executable(Path::new("codex"), &search)
            .unwrap_or_else(|error| panic!("resolve codex: {error}"));
        assert_eq!(resolved.path, fixture.path("second/codex.cmd"));
        assert_eq!(resolved.kind, ExecutableKind::CommandScript);
    }

    #[test]
    fn explicit_missing_path_is_not_replaced_from_path() {
        let fixture = SearchFixture::new();
        fixture.write_bytes("bin/codex.exe", b"MZ");
        let explicit = fixture.path("missing/codex.exe");

        let Err(error) = resolve_executable(&explicit, &fixture.windows_search()) else {
            panic!("explicit missing path unexpectedly resolved");
        };

        assert!(matches!(
            error,
            ExecutableResolutionError::ExplicitMissing { .. }
        ));
    }

    #[test]
    fn windows_filters_pathext_and_preserves_its_order() {
        let fixture = SearchFixture::new();
        fixture.write_bytes("bin/tool.exe", b"MZ");
        fixture.write_bytes("bin/tool.CmD", b"@echo off\r\n");
        fixture.write_bytes("bin/tool.ps1", b"exit 0\r\n");
        let search = fixture.search(ExecutablePlatform::Windows, ".PS1;.cMd;.EXE;.TXT", ["bin"]);

        let resolved = resolve_executable(Path::new("tool"), &search)
            .unwrap_or_else(|error| panic!("resolve tool: {error}"));

        assert_eq!(resolved.path, fixture.path("bin/tool.CmD"));
        assert_eq!(resolved.kind, ExecutableKind::CommandScript);
    }

    #[test]
    fn explicit_relative_path_resolves_from_working_directory() {
        let fixture = SearchFixture::new();
        fixture.write_bytes("tools/provider.exe", b"MZ");
        let search = fixture.windows_search();

        let resolved = resolve_executable(Path::new("tools/provider.exe"), &search)
            .unwrap_or_else(|error| panic!("resolve explicit provider: {error}"));

        assert_eq!(resolved.path, fixture.path("tools/provider.exe"));
        assert_eq!(resolved.kind, ExecutableKind::Native);
    }

    #[cfg(unix)]
    #[test]
    fn unix_requires_an_executable_permission_bit() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = SearchFixture::new();
        fixture.write_bytes("bin/provider", b"#!/bin/sh\nexit 0\n");
        fs::set_permissions(
            fixture.path("bin/provider"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap_or_else(|error| panic!("set fixture permissions: {error}"));
        let search = fixture.search(ExecutablePlatform::Unix, "", ["bin"]);

        let Err(error) = resolve_executable(Path::new("provider"), &search) else {
            panic!("non-executable Unix fixture unexpectedly resolved");
        };

        assert!(matches!(error, ExecutableResolutionError::NotFound { .. }));
    }
}
