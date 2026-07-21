use std::{
    fmt,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// A repository-relative path that cannot traverse above its repository root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(PathBuf);

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RepoPathError {
    #[error("repository path must not be empty")]
    Empty,
    #[error("repository path must be relative and contain only normal components: {0}")]
    Unsafe(String),
    #[error("repository path contains a NUL byte")]
    Nul,
    #[error("repository path is not valid UTF-8")]
    NonUtf8,
}

impl RepoPath {
    /// Validates and constructs a repository-relative path.
    ///
    /// # Errors
    ///
    /// Returns [`RepoPathError`] for empty, absolute, non-UTF-8, NUL-containing, or
    /// traversal-capable paths.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, RepoPathError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(RepoPathError::Empty);
        }
        let Some(text) = path.to_str() else {
            return Err(RepoPathError::NonUtf8);
        };
        if text.contains('\0') {
            return Err(RepoPathError::Nul);
        }
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(RepoPathError::Unsafe(text.to_owned()));
        }
        Ok(Self(path))
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// Resolves the path lexically below `root`. Callers must additionally perform
    /// component-by-component symlink checks before using the result for writes.
    #[must_use]
    pub fn join_to(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }
}

/// Returns whether two normalized repository-relative paths reserve intersecting trees.
#[must_use]
pub fn repo_paths_overlap(left: &RepoPath, right: &RepoPath) -> bool {
    left.as_path().starts_with(right.as_path()) || right.as_path().starts_with(left.as_path())
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.to_string_lossy())
    }
}

impl TryFrom<&str> for RepoPath {
    type Error = RepoPathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RepoPath {
    type Error = RepoPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<PathBuf> for RepoPath {
    type Error = RepoPathError;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for RepoPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let Some(value) = self.0.to_str() else {
            return Err(serde::ser::Error::custom(RepoPathError::NonUtf8));
        };
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::RepoPath;

    #[test]
    fn accepts_only_relative_normal_components() {
        assert!(RepoPath::try_from("src/lib.rs").is_ok());
        assert!(RepoPath::try_from("../secret").is_err());
        assert!(RepoPath::try_from("./src/lib.rs").is_err());
        assert!(RepoPath::try_from("").is_err());
    }

    #[test]
    fn deserialization_revalidates_path() {
        let result = serde_json::from_str::<RepoPath>(r#""../../escape""#);
        assert!(result.is_err());
    }
}
