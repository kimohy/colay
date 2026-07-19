use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableValidationContext {
    pub working_directory: PathBuf,
    /// Present only for a bare executable selected from the effective PATH.
    pub search_directory: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedExecutable {
    pub configured: PathBuf,
    pub path: PathBuf,
    pub kind: ExecutableKind,
    pub validation: ExecutableValidationContext,
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
    #[error("could not canonicalize executable resolution path `{path}`: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("persisted executable resolution evidence is invalid: {0}")]
    InvalidEvidence(String),
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

/// Validates persisted process-boundary evidence without consulting the
/// current configuration or PATH.
pub fn validate_resolution_evidence(
    evidence: &ResolvedExecutable,
) -> Result<(), ExecutableResolutionError> {
    if evidence.configured.as_os_str().is_empty() {
        return Err(ExecutableResolutionError::InvalidEvidence(
            "configured executable is empty".to_owned(),
        ));
    }
    if !evidence.path.is_absolute() || !evidence.validation.working_directory.is_absolute() {
        return Err(ExecutableResolutionError::InvalidEvidence(
            "resolved path and working directory must be absolute".to_owned(),
        ));
    }
    let metadata = fs::symlink_metadata(&evidence.path).map_err(|_| {
        ExecutableResolutionError::InvalidEvidence("resolved executable is missing".to_owned())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ExecutableResolutionError::InvalidEvidence(
            "resolved executable must be a non-symlink regular file".to_owned(),
        ));
    }
    #[cfg(unix)]
    if !unix_executable(&metadata) {
        return Err(ExecutableResolutionError::InvalidEvidence(
            "resolved executable is not executable".to_owned(),
        ));
    }
    let canonical_path = canonicalize_resolution_path(evidence.path.clone())?;
    let _canonical_working_directory =
        canonicalize_resolution_path(evidence.validation.working_directory.clone())?;
    if executable_kind(&evidence.path, ExecutablePlatform::current()) != evidence.kind {
        return Err(ExecutableResolutionError::InvalidEvidence(
            "executable kind does not match the resolved path".to_owned(),
        ));
    }

    if is_explicit(&evidence.configured) {
        if evidence.validation.search_directory.is_some() {
            return Err(ExecutableResolutionError::InvalidEvidence(
                "explicit executable unexpectedly records a PATH directory".to_owned(),
            ));
        }
        let configured_path = if evidence.configured.is_absolute() {
            evidence.configured.clone()
        } else {
            evidence
                .validation
                .working_directory
                .join(&evidence.configured)
        };
        if canonicalize_resolution_path(configured_path)? != canonical_path {
            return Err(ExecutableResolutionError::InvalidEvidence(
                "configured executable does not match the resolved path".to_owned(),
            ));
        }
    } else {
        let search_directory = evidence
            .validation
            .search_directory
            .as_ref()
            .ok_or_else(|| {
                ExecutableResolutionError::InvalidEvidence(
                    "bare executable omitted its selected PATH directory".to_owned(),
                )
            })?;
        let canonical_search_directory = canonicalize_resolution_path(search_directory.clone())?;
        if Some(canonical_search_directory.as_path()) != canonical_path.parent() {
            return Err(ExecutableResolutionError::InvalidEvidence(
                "selected PATH directory does not contain the resolved executable".to_owned(),
            ));
        }
        validate_bare_filename(evidence)?;
    }
    Ok(())
}

fn validate_bare_filename(evidence: &ResolvedExecutable) -> Result<(), ExecutableResolutionError> {
    let configured = evidence
        .configured
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let resolved = evidence
        .path
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let matches = if ExecutablePlatform::current() == ExecutablePlatform::Windows {
        if evidence.configured.extension().is_some() {
            configured.eq_ignore_ascii_case(resolved)
        } else {
            evidence
                .path
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(|stem| stem.eq_ignore_ascii_case(configured))
                && evidence
                    .path
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|extension| {
                        matches!(
                            extension.to_ascii_lowercase().as_str(),
                            "exe" | "com" | "cmd" | "bat"
                        )
                    })
        }
    } else {
        configured == resolved
    };
    if matches {
        Ok(())
    } else {
        Err(ExecutableResolutionError::InvalidEvidence(
            "configured bare executable does not match the resolved filename".to_owned(),
        ))
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
        validation: ExecutableValidationContext {
            working_directory: search.working_directory.clone(),
            search_directory: None,
        },
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
                return resolved_bare(configured, path, search);
            }
            continue;
        }

        for extension in &extensions {
            let mut candidate = configured.as_os_str().to_os_string();
            candidate.push(extension);
            if let Some(path) = windows_regular_file(directory, &candidate) {
                return resolved_bare(configured, path, search);
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
            return resolved_bare(configured, candidate, search);
        }
    }
    Err(ExecutableResolutionError::NotFound {
        configured: configured.to_path_buf(),
        search_path: search.path.clone(),
    })
}

fn resolved_bare(
    configured: &Path,
    path: PathBuf,
    search: &ExecutableSearch,
) -> Result<ResolvedExecutable, ExecutableResolutionError> {
    let search_directory = path.parent().map(Path::to_path_buf).ok_or_else(|| {
        ExecutableResolutionError::ExplicitNotFile {
            configured: configured.to_path_buf(),
            resolved: path.clone(),
        }
    })?;
    Ok(ResolvedExecutable {
        configured: configured.to_path_buf(),
        kind: executable_kind(&path, search.platform),
        path,
        validation: ExecutableValidationContext {
            working_directory: search.working_directory.clone(),
            search_directory: Some(search_directory),
        },
    })
}

fn canonicalize_resolution_path(path: PathBuf) -> Result<PathBuf, ExecutableResolutionError> {
    fs::canonicalize(&path)
        .map_err(|source| ExecutableResolutionError::Canonicalize { path, source })
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
