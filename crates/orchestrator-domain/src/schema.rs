use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de};

/// Version attached to independently migratable persisted contracts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(String);

impl SchemaVersion {
    pub const V1: &'static str = "1";
    pub const V3: &'static str = "3";
    pub const V4: &'static str = "4";

    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self(version.into())
    }

    #[must_use]
    pub fn v1() -> Self {
        Self::new(Self::V1)
    }

    #[must_use]
    pub fn state_current() -> Self {
        Self::new(Self::V4)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether this version is one of the explicitly supported contract versions.
    #[must_use]
    pub fn is_supported_by(&self, supported: &[&str]) -> bool {
        supported.contains(&self.0.as_str())
    }
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self::v1()
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for SchemaVersion {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SchemaVersion {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Deserializes a persisted v1 contract discriminator and rejects every
/// unrecognized version before the containing document can be used.
///
/// This is intentionally attached to each independently versioned v1 domain
/// document instead of to [`SchemaVersion`] globally: the state event schema
/// has its own version lifecycle.
///
/// # Errors
///
/// Returns a deserializer error when the value is not schema version `1`.
pub fn deserialize_v1_schema_version<'de, D>(deserializer: D) -> Result<SchemaVersion, D::Error>
where
    D: Deserializer<'de>,
{
    let version = SchemaVersion::deserialize(deserializer)?;
    if version.as_str() == SchemaVersion::V1 {
        Ok(version)
    } else {
        Err(de::Error::custom(format!(
            "unsupported persisted schema version {}; supported versions: [{}]",
            version,
            SchemaVersion::V1
        )))
    }
}
