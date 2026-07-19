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
    #[error(
        "configured executable `{configured}` has an invalid candidate at `{candidate}`: {reason}"
    )]
    InvalidCandidate {
        configured: PathBuf,
        candidate: PathBuf,
        reason: String,
    },
    #[error(
        "access denied while inspecting configured executable `{configured}` candidate `{candidate}`: {source}"
    )]
    AccessDenied {
        configured: PathBuf,
        candidate: PathBuf,
        #[source]
        source: std::io::Error,
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
    if !evidence.path.is_absolute()
        || !evidence.configured.is_absolute()
            && !evidence.validation.working_directory.is_absolute()
    {
        return Err(ExecutableResolutionError::InvalidEvidence(
            "resolved path and required working directory must be absolute".to_owned(),
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
            let _canonical_working_directory =
                canonicalize_resolution_path(evidence.validation.working_directory.clone())?;
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
        let _canonical_working_directory =
            canonicalize_resolution_path(evidence.validation.working_directory.clone())?;
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
        Err(error) => {
            return Err(candidate_io_failure(configured, resolved, error));
        }
    };
    if !metadata.is_file() {
        return Err(ExecutableResolutionError::InvalidCandidate {
            configured: configured.to_path_buf(),
            candidate: resolved,
            reason: "candidate is not a regular file".to_owned(),
        });
    }
    if search.platform == ExecutablePlatform::Unix && !unix_executable(&metadata) {
        return Err(ExecutableResolutionError::InvalidCandidate {
            configured: configured.to_path_buf(),
            candidate: resolved,
            reason: "regular file is not executable".to_owned(),
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
    let mut invalid = None;

    for directory in &search.path {
        if let Some(extension) = configured_extension {
            let extension = format!(".{extension}");
            if extensions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&extension))
            {
                match windows_regular_file(directory, configured.as_os_str()) {
                    Ok(Some(path)) => return resolved_bare(configured, path, search),
                    Ok(None) => {}
                    Err((candidate, error)) => {
                        retain_candidate_failure(
                            &mut invalid,
                            candidate_io_failure(configured, candidate, error),
                        );
                    }
                }
            }
            continue;
        }

        for extension in &extensions {
            let mut candidate = configured.as_os_str().to_os_string();
            candidate.push(extension);
            match windows_regular_file(directory, &candidate) {
                Ok(Some(path)) => return resolved_bare(configured, path, search),
                Ok(None) => {}
                Err((candidate, error)) => {
                    retain_candidate_failure(
                        &mut invalid,
                        candidate_io_failure(configured, candidate, error),
                    );
                }
            }
        }
    }

    if let Some(invalid) = invalid {
        return Err(invalid);
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
    let mut invalid = None;
    for directory in &search.path {
        let candidate = directory.join(configured);
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() && unix_executable(&metadata) => {
                return resolved_bare(configured, candidate, search);
            }
            Ok(metadata) => {
                invalid.get_or_insert_with(|| ExecutableResolutionError::InvalidCandidate {
                    configured: configured.to_path_buf(),
                    candidate,
                    reason: if metadata.is_file() {
                        "regular file is not executable".to_owned()
                    } else {
                        "candidate is not a regular file".to_owned()
                    },
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let failure = candidate_io_failure(configured, candidate, error);
                retain_candidate_failure(&mut invalid, failure);
            }
        }
    }
    if let Some(invalid) = invalid {
        return Err(invalid);
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

fn candidate_io_failure(
    configured: &Path,
    candidate: PathBuf,
    source: std::io::Error,
) -> ExecutableResolutionError {
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        ExecutableResolutionError::AccessDenied {
            configured: configured.to_path_buf(),
            candidate,
            source,
        }
    } else {
        ExecutableResolutionError::InvalidCandidate {
            configured: configured.to_path_buf(),
            candidate,
            reason: format!("candidate metadata is unavailable: {source}"),
        }
    }
}

fn retain_candidate_failure(
    retained: &mut Option<ExecutableResolutionError>,
    candidate: ExecutableResolutionError,
) {
    let candidate_is_access = matches!(candidate, ExecutableResolutionError::AccessDenied { .. });
    let retained_is_access = retained
        .as_ref()
        .is_some_and(|error| matches!(error, ExecutableResolutionError::AccessDenied { .. }));
    if retained.is_none() || candidate_is_access && !retained_is_access {
        *retained = Some(candidate);
    }
}

fn allowed_windows_extensions(pathext: &[OsString]) -> Vec<String> {
    pathext
        .iter()
        .filter_map(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|extension| matches!(extension.as_str(), ".exe" | ".com" | ".cmd" | ".bat"))
        .collect()
}

fn windows_regular_file(
    directory: &Path,
    filename: &OsStr,
) -> Result<Option<PathBuf>, (PathBuf, std::io::Error)> {
    let expected = filename.to_string_lossy();
    let candidate = directory.join(filename);
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err((candidate, error));
        }
    };
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| (candidate.clone(), error))?;
        if !entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected)
        {
            continue;
        }
        let kind = entry.file_type().map_err(|error| (entry.path(), error))?;
        if !kind.is_file() {
            return Err((
                entry.path(),
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "candidate is not a regular file",
                ),
            ));
        }
        matches.push(entry.path());
    }
    matches.sort();
    Ok(matches.into_iter().next())
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

    #[test]
    fn bare_lookup_reports_invalid_candidate_instead_of_not_found() {
        let fixture = SearchFixture::new();
        fs::create_dir_all(fixture.path("bin/provider"))
            .unwrap_or_else(|error| panic!("create invalid candidate directory: {error}"));
        let search = fixture.search(ExecutablePlatform::Unix, "", ["bin"]);

        let Err(error) = resolve_executable(Path::new("provider"), &search) else {
            panic!("invalid bare candidate unexpectedly resolved");
        };

        assert!(matches!(
            error,
            ExecutableResolutionError::InvalidCandidate { candidate, .. }
                if candidate == fixture.path("bin/provider")
        ));
    }

    #[test]
    fn windows_bare_lookup_reports_invalid_supported_candidate() {
        let fixture = SearchFixture::new();
        fs::create_dir_all(fixture.path("bin/provider.exe"))
            .unwrap_or_else(|error| panic!("create invalid Windows candidate: {error}"));
        let search = fixture.search(ExecutablePlatform::Windows, ".EXE", ["bin"]);

        let Err(error) = resolve_executable(Path::new("provider"), &search) else {
            panic!("invalid Windows candidate unexpectedly resolved");
        };

        assert!(matches!(
            error,
            ExecutableResolutionError::InvalidCandidate { candidate, .. }
                if candidate == fixture.path("bin/provider.exe")
        ));
    }

    #[test]
    fn permission_failure_has_typed_access_denied_diagnostic() {
        let error = candidate_io_failure(
            Path::new("provider"),
            PathBuf::from("restricted/provider"),
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );

        assert!(matches!(
            error,
            ExecutableResolutionError::AccessDenied {
                configured,
                candidate,
                source,
            } if configured == Path::new("provider")
                && candidate == Path::new("restricted/provider")
                && source.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn access_denied_outweighs_an_earlier_invalid_candidate() {
        let mut retained = Some(ExecutableResolutionError::InvalidCandidate {
            configured: PathBuf::from("provider"),
            candidate: PathBuf::from("invalid/provider"),
            reason: "candidate is not a regular file".to_owned(),
        });

        retain_candidate_failure(
            &mut retained,
            candidate_io_failure(
                Path::new("provider"),
                PathBuf::from("restricted/provider"),
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ),
        );

        assert!(matches!(
            retained,
            Some(ExecutableResolutionError::AccessDenied { candidate, .. })
                if candidate == Path::new("restricted/provider")
        ));
    }

    #[test]
    fn later_valid_windows_candidate_wins_after_invalid_candidate() {
        let fixture = SearchFixture::new();
        fs::create_dir_all(fixture.path("first/provider.exe"))
            .unwrap_or_else(|error| panic!("create invalid Windows candidate: {error}"));
        fixture.write_bytes("second/provider.exe", b"MZ");
        let search = fixture.search(ExecutablePlatform::Windows, ".EXE", ["first", "second"]);

        let resolved = resolve_executable(Path::new("provider"), &search)
            .unwrap_or_else(|error| panic!("resolve later valid provider: {error}"));

        assert_eq!(resolved.path, fixture.path("second/provider.exe"));
    }

    #[test]
    fn absolute_persisted_identity_does_not_require_historical_working_directory() {
        let fixture = SearchFixture::new();
        fixture.write_bytes("bin/provider.exe", b"MZ");
        let executable = fixture.path("bin/provider.exe");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
                .unwrap_or_else(|error| panic!("set fixture permissions: {error}"));
        }
        let evidence = ResolvedExecutable {
            configured: executable.clone(),
            path: executable,
            kind: ExecutableKind::Native,
            validation: ExecutableValidationContext {
                working_directory: fixture.path("retired-worktree"),
                search_directory: None,
            },
        };

        validate_resolution_evidence(&evidence)
            .unwrap_or_else(|error| panic!("validate absolute persisted identity: {error}"));
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

        assert!(matches!(
            error,
            ExecutableResolutionError::InvalidCandidate {
                configured,
                candidate,
                ..
            } if configured == Path::new("provider")
                && candidate == fixture.path("bin/provider")
        ));
    }
}
