use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Database, StateError, StateResult};

const RESERVED_MIGRATION_WORKSPACE_ID: Uuid = Uuid::from_u128(1);
const REPOSITORY_WORKSPACE_TOUCH_INTERVAL: TimeDelta = TimeDelta::minutes(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Git,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Active,
    Detached,
    Archived,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRegistration {
    pub workspace_id: WorkspaceId,
    pub kind: WorkspaceKind,
    pub status: WorkspaceStatus,
    pub canonical_path: PathBuf,
    pub git_common_dir: Option<PathBuf>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

impl WorkspaceId {
    pub(crate) const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for WorkspaceId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Database {
    /// Resolves the durable workspace for a repository directory.
    ///
    /// A pre-workspace database migration may contain one detached reserved workspace without a
    /// path. The first repository resolution adopts that partition atomically so migrated rows
    /// remain reachable. Fresh databases, and every later repository, receive `UUIDv7` identities.
    pub fn resolve_repository_workspace(&self, path: &Path) -> StateResult<WorkspaceRegistration> {
        self.resolve_repository_workspace_at(path, Utc::now())
    }

    fn resolve_repository_workspace_at(
        &self,
        path: &Path,
        now: DateTime<Utc>,
    ) -> StateResult<WorkspaceRegistration> {
        let kind = WorkspaceKind::Directory;
        let identity = WorkspacePathIdentity::resolve(path, kind)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;

        let registration =
            match load_current_by_comparison_key(&transaction, &identity.comparison_key)? {
                Some(existing) => refresh_repository_registration_if_stale(
                    &transaction,
                    existing,
                    &identity.comparison_key,
                    now,
                )?,
                None if reserved_workspace_is_unattached(&transaction)? => {
                    attach_reserved_workspace(&transaction, &identity, now)?
                }
                None => insert_workspace(&transaction, &identity, kind, now)?,
            };
        transaction.commit()?;
        Ok(registration)
    }

    pub fn resolve_workspace(
        &self,
        path: &Path,
        kind: WorkspaceKind,
    ) -> StateResult<WorkspaceRegistration> {
        let identity = WorkspacePathIdentity::resolve(path, kind)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let now = Utc::now();

        let registration =
            match load_current_by_comparison_key(&transaction, &identity.comparison_key)? {
                Some(existing) => {
                    if existing.kind != kind {
                        return Err(StateError::InvalidWorkspacePath {
                            path: identity.canonical_path,
                            reason: "is already registered with a different workspace kind"
                                .to_owned(),
                        });
                    }
                    touch_registration(
                        &transaction,
                        existing.workspace_id,
                        &identity.comparison_key,
                        now,
                    )?;
                    load_workspace_in_connection(&transaction, existing.workspace_id)?.ok_or_else(
                        || {
                            StateError::InvalidRecord(
                                "workspace disappeared during registration".to_owned(),
                            )
                        },
                    )?
                }
                None => insert_workspace(&transaction, &identity, kind, now)?,
            };
        transaction.commit()?;
        Ok(registration)
    }

    pub fn attach_workspace(
        &self,
        workspace_id: WorkspaceId,
        path: &Path,
    ) -> StateResult<WorkspaceRegistration> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let existing =
            load_workspace_in_connection(&transaction, workspace_id)?.ok_or_else(|| {
                StateError::WorkspaceNotFound {
                    workspace_id: workspace_id.to_string(),
                }
            })?;
        let identity = WorkspacePathIdentity::resolve(path, existing.kind)?;
        if let Some(occupant) =
            load_current_by_comparison_key(&transaction, &identity.comparison_key)?
            && occupant.workspace_id != workspace_id
        {
            return Err(StateError::WorkspacePathConflict {
                path: identity.canonical_path,
            });
        }

        let now = Utc::now();
        transaction.execute(
            "UPDATE workspace_paths SET is_current = 0 WHERE workspace_id = ?1 AND is_current = 1",
            [workspace_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO workspace_paths( \
                workspace_id, canonical_path, comparison_key, git_common_dir, is_current, first_seen_at, last_seen_at \
             ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5) \
             ON CONFLICT(workspace_id, comparison_key) DO UPDATE SET \
                canonical_path = excluded.canonical_path, \
                git_common_dir = excluded.git_common_dir, \
                is_current = 1, \
                last_seen_at = excluded.last_seen_at",
            params![
                workspace_id.to_string(),
                identity.canonical_path.to_string_lossy(),
                identity.comparison_key,
                identity.git_common_dir.as_ref().map(|value| value.to_string_lossy()),
                now.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "UPDATE workspaces SET status = 'active', last_seen_at = ?2 WHERE workspace_id = ?1",
            params![workspace_id.to_string(), now.to_rfc3339()],
        )?;
        let registration =
            load_workspace_in_connection(&transaction, workspace_id)?.ok_or_else(|| {
                StateError::InvalidRecord("workspace disappeared while attaching a path".to_owned())
            })?;
        transaction.commit()?;
        Ok(registration)
    }

    pub fn load_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> StateResult<Option<WorkspaceRegistration>> {
        let connection = self.lock()?;
        load_workspace_in_connection(&connection, workspace_id)
    }
}

fn refresh_repository_registration_if_stale(
    transaction: &Transaction<'_>,
    existing: WorkspaceRegistration,
    comparison_key: &str,
    now: DateTime<Utc>,
) -> StateResult<WorkspaceRegistration> {
    if now.signed_duration_since(existing.last_seen_at) < REPOSITORY_WORKSPACE_TOUCH_INTERVAL {
        return Ok(existing);
    }
    touch_registration(transaction, existing.workspace_id, comparison_key, now)?;
    load_workspace_in_connection(transaction, existing.workspace_id)?.ok_or_else(|| {
        StateError::InvalidRecord("workspace disappeared during liveness refresh".to_owned())
    })
}

fn reserved_workspace_is_unattached(connection: &Connection) -> StateResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS( \
                 SELECT 1 FROM workspaces AS w \
                 WHERE w.workspace_id = ?1 \
                   AND NOT EXISTS( \
                       SELECT 1 FROM workspace_paths AS p \
                       WHERE p.workspace_id = w.workspace_id AND p.is_current = 1 \
                   ) \
             )",
            [RESERVED_MIGRATION_WORKSPACE_ID.to_string()],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn attach_reserved_workspace(
    transaction: &Transaction<'_>,
    identity: &WorkspacePathIdentity,
    now: DateTime<Utc>,
) -> StateResult<WorkspaceRegistration> {
    let workspace_id = WorkspaceId(RESERVED_MIGRATION_WORKSPACE_ID);
    let timestamp = now.to_rfc3339();
    transaction.execute(
        "UPDATE workspaces \
         SET kind = 'directory', status = 'active', last_seen_at = ?2 \
         WHERE workspace_id = ?1",
        params![workspace_id.to_string(), timestamp],
    )?;
    transaction.execute(
        "INSERT INTO workspace_paths( \
            workspace_id, canonical_path, comparison_key, git_common_dir, is_current, first_seen_at, last_seen_at \
         ) VALUES (?1, ?2, ?3, NULL, 1, ?4, ?4)",
        params![
            workspace_id.to_string(),
            identity.canonical_path.to_string_lossy(),
            identity.comparison_key,
            timestamp,
        ],
    )?;
    load_workspace_in_connection(transaction, workspace_id)?.ok_or_else(|| {
        StateError::InvalidRecord("reserved workspace disappeared while attaching".to_owned())
    })
}

impl WorkspaceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Directory => "directory",
        }
    }

    fn parse(value: &str) -> StateResult<Self> {
        match value {
            "git" => Ok(Self::Git),
            "directory" => Ok(Self::Directory),
            _ => Err(StateError::InvalidRecord(format!(
                "workspace kind is invalid: {value}"
            ))),
        }
    }
}

impl WorkspaceStatus {
    fn parse(value: &str) -> StateResult<Self> {
        match value {
            "active" => Ok(Self::Active),
            "detached" => Ok(Self::Detached),
            "archived" => Ok(Self::Archived),
            _ => Err(StateError::InvalidRecord(format!(
                "workspace status is invalid: {value}"
            ))),
        }
    }
}

struct WorkspacePathIdentity {
    canonical_path: PathBuf,
    comparison_key: String,
    git_common_dir: Option<PathBuf>,
}

type WorkspaceRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
);

impl WorkspacePathIdentity {
    fn resolve(path: &Path, kind: WorkspaceKind) -> StateResult<Self> {
        let canonical_path = fs::canonicalize(path).map_err(|error| StateError::io(path, error))?;
        if !fs::metadata(&canonical_path)
            .map_err(|error| StateError::io(&canonical_path, error))?
            .is_dir()
        {
            return Err(StateError::InvalidWorkspacePath {
                path: canonical_path,
                reason: "workspace path is not a directory".to_owned(),
            });
        }
        let git_common_dir = match kind {
            WorkspaceKind::Git => Some(git_common_dir(&canonical_path)?),
            WorkspaceKind::Directory => None,
        };
        let comparison_path = git_common_dir.as_deref().unwrap_or(&canonical_path);
        Ok(Self {
            comparison_key: comparison_key(comparison_path),
            canonical_path,
            git_common_dir,
        })
    }
}

fn insert_workspace(
    transaction: &Transaction<'_>,
    identity: &WorkspacePathIdentity,
    kind: WorkspaceKind,
    now: DateTime<Utc>,
) -> StateResult<WorkspaceRegistration> {
    let workspace_id = WorkspaceId(Uuid::now_v7());
    let timestamp = now.to_rfc3339();
    transaction.execute(
        "INSERT INTO workspaces(workspace_id, kind, status, created_at, last_seen_at) \
         VALUES (?1, ?2, 'active', ?3, ?3)",
        params![workspace_id.to_string(), kind.as_str(), timestamp],
    )?;
    transaction.execute(
        "INSERT INTO workspace_paths( \
            workspace_id, canonical_path, comparison_key, git_common_dir, is_current, first_seen_at, last_seen_at \
         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
        params![
            workspace_id.to_string(),
            identity.canonical_path.to_string_lossy(),
            identity.comparison_key,
            identity.git_common_dir.as_ref().map(|value| value.to_string_lossy()),
            timestamp,
        ],
    )?;
    Ok(WorkspaceRegistration {
        workspace_id,
        kind,
        status: WorkspaceStatus::Active,
        canonical_path: identity.canonical_path.clone(),
        git_common_dir: identity.git_common_dir.clone(),
        created_at: now,
        last_seen_at: now,
    })
}

fn touch_registration(
    transaction: &Transaction<'_>,
    workspace_id: WorkspaceId,
    comparison_key: &str,
    now: DateTime<Utc>,
) -> StateResult<()> {
    let timestamp = now.to_rfc3339();
    transaction.execute(
        "UPDATE workspaces SET last_seen_at = ?2 WHERE workspace_id = ?1",
        params![workspace_id.to_string(), timestamp],
    )?;
    transaction.execute(
        "UPDATE workspace_paths SET last_seen_at = ?3 \
         WHERE workspace_id = ?1 AND comparison_key = ?2",
        params![workspace_id.to_string(), comparison_key, timestamp],
    )?;
    Ok(())
}

fn load_current_by_comparison_key(
    connection: &Connection,
    comparison_key: &str,
) -> StateResult<Option<WorkspaceRegistration>> {
    let workspace_id: Option<String> = connection
        .query_row(
            "SELECT workspace_id FROM workspace_paths \
             WHERE comparison_key = ?1 AND is_current = 1",
            [comparison_key],
            |row| row.get(0),
        )
        .optional()?;
    workspace_id
        .map(|value| parse_workspace_id(&value))
        .transpose()?
        .map(|workspace_id| load_workspace_in_connection(connection, workspace_id))
        .transpose()
        .map(Option::flatten)
}

fn load_workspace_in_connection(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> StateResult<Option<WorkspaceRegistration>> {
    let row: Option<WorkspaceRow> = connection
        .query_row(
            "SELECT w.workspace_id, w.kind, w.status, w.created_at, w.last_seen_at, \
                    p.canonical_path, p.git_common_dir, p.first_seen_at, p.last_seen_at \
             FROM workspaces AS w \
             LEFT JOIN workspace_paths AS p ON p.workspace_id = w.workspace_id AND p.is_current = 1 \
             WHERE w.workspace_id = ?1",
            [workspace_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?;
    row.map(parse_registration).transpose()
}

fn parse_registration(row: WorkspaceRow) -> StateResult<WorkspaceRegistration> {
    let (
        workspace_id,
        kind,
        status,
        created_at,
        last_seen_at,
        canonical_path,
        git_common_dir,
        _,
        _,
    ) = row;
    Ok(WorkspaceRegistration {
        workspace_id: parse_workspace_id(&workspace_id)?,
        kind: WorkspaceKind::parse(&kind)?,
        status: WorkspaceStatus::parse(&status)?,
        canonical_path: PathBuf::from(canonical_path),
        git_common_dir: git_common_dir.map(PathBuf::from),
        created_at: parse_timestamp("workspaces.created_at", &created_at)?,
        last_seen_at: parse_timestamp("workspaces.last_seen_at", &last_seen_at)?,
    })
}

fn parse_workspace_id(value: &str) -> StateResult<WorkspaceId> {
    Uuid::parse_str(value)
        .map(WorkspaceId)
        .map_err(|error| StateError::InvalidRecord(format!("workspace_id is invalid: {error}")))
}

fn parse_timestamp(column: &str, value: &str) -> StateResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| StateError::InvalidRecord(format!("{column} is invalid: {error}")))
}

fn comparison_key(path: &Path) -> String {
    let key = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        key.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        key
    }
}

fn git_common_dir(workspace: &Path) -> StateResult<PathBuf> {
    let dot_git = workspace.join(".git");
    let metadata = fs::metadata(&dot_git).map_err(|error| StateError::io(&dot_git, error))?;
    if metadata.is_dir() {
        return fs::canonicalize(&dot_git).map_err(|error| StateError::io(&dot_git, error));
    }
    if !metadata.is_file() {
        return Err(StateError::InvalidWorkspacePath {
            path: dot_git,
            reason: ".git is neither a directory nor a gitdir file".to_owned(),
        });
    }
    let gitdir = fs::read_to_string(&dot_git).map_err(|error| StateError::io(&dot_git, error))?;
    let gitdir =
        gitdir
            .strip_prefix("gitdir: ")
            .ok_or_else(|| StateError::InvalidWorkspacePath {
                path: dot_git.clone(),
                reason: ".git file does not contain a gitdir declaration".to_owned(),
            })?;
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        workspace.join(gitdir)
    };
    let gitdir = fs::canonicalize(&gitdir).map_err(|error| StateError::io(&gitdir, error))?;
    let common = gitdir.join("commondir");
    if !common.exists() {
        return Ok(gitdir);
    }
    let common_dir = fs::read_to_string(&common).map_err(|error| StateError::io(&common, error))?;
    let common_dir = PathBuf::from(common_dir.trim());
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        gitdir.join(common_dir)
    };
    fs::canonicalize(&common_dir).map_err(|error| StateError::io(&common_dir, error))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone as _, Utc};

    use super::{Database, REPOSITORY_WORKSPACE_TOUCH_INTERVAL};

    fn fixture() -> Result<(tempfile::TempDir, Database, std::path::PathBuf), crate::StateError> {
        let root = tempfile::tempdir().map_err(|error| {
            crate::StateError::InvalidRecord(format!("test tempdir failed: {error}"))
        })?;
        let repository = root.path().join("repository");
        std::fs::create_dir_all(&repository)
            .map_err(|error| crate::StateError::io(&repository, error))?;
        let database = Database::open(root.path().join("state.db"))?;
        database.migrate_with_backup(&root.path().join("backups"))?;
        Ok((root, database, repository))
    }

    fn timestamp(minute: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 25, 1, minute, 0)
            .single()
            .unwrap_or_else(Utc::now)
    }

    fn path_last_seen(
        database: &Database,
        workspace_id: super::WorkspaceId,
    ) -> crate::StateResult<chrono::DateTime<Utc>> {
        database.with_connection(|connection| {
            let value: String = connection.query_row(
                "SELECT last_seen_at FROM main.workspace_paths \
                 WHERE workspace_id = ?1 AND is_current = 1",
                [workspace_id.to_string()],
                |row| row.get(0),
            )?;
            super::parse_timestamp("workspace_paths.last_seen_at", &value)
        })
    }

    #[test]
    fn repository_resolution_skips_recent_liveness_touch() -> crate::StateResult<()> {
        let (_root, database, repository) = fixture()?;
        let initial = database.resolve_repository_workspace_at(&repository, timestamp(0))?;
        let recent = database.resolve_repository_workspace_at(
            &repository,
            timestamp(0) + REPOSITORY_WORKSPACE_TOUCH_INTERVAL - TimeDelta::seconds(1),
        )?;

        assert_eq!(recent.last_seen_at, initial.last_seen_at);
        assert_eq!(
            path_last_seen(&database, initial.workspace_id)?,
            initial.last_seen_at
        );
        Ok(())
    }

    #[test]
    fn repository_resolution_refreshes_stale_liveness() -> crate::StateResult<()> {
        let (_root, database, repository) = fixture()?;
        let initial = database.resolve_repository_workspace_at(&repository, timestamp(0))?;
        let refreshed_at = timestamp(0) + REPOSITORY_WORKSPACE_TOUCH_INTERVAL;
        let refreshed = database.resolve_repository_workspace_at(&repository, refreshed_at)?;

        assert_eq!(refreshed.workspace_id, initial.workspace_id);
        assert_eq!(refreshed.last_seen_at, refreshed_at);
        assert_eq!(
            path_last_seen(&database, initial.workspace_id)?,
            refreshed_at
        );
        Ok(())
    }
}
