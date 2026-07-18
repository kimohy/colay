use std::{fs, io::Read as _, path::Path};

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    AcceptanceEvidence, CommandEvidence, ProviderId, RepoPath, SchemaVersion, TaskId, TestEvidence,
    TestStatus, VerificationCheck, VerificationCheckKind, VerificationId, VerificationResult,
    VerificationStatus,
};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult, GitSnapshot};

const MAX_SECRET_SCAN_BYTES_PER_FILE: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretFinding {
    pub location: String,
    pub kind: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretScanReport {
    pub findings: Vec<SecretFinding>,
    pub truncated_files: Vec<String>,
}

impl SecretScanReport {
    #[must_use]
    pub fn safe_to_persist_or_share(&self) -> bool {
        self.findings.is_empty() && self.truncated_files.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct VerificationInput {
    pub task_id: TaskId,
    pub implementation_provider: ProviderId,
    pub reviewer_provider: Option<ProviderId>,
    pub independent_review_required: bool,
    pub independent_review_passed: bool,
    pub snapshot: GitSnapshot,
    pub worktree_root: std::path::PathBuf,
    pub expected_paths: Vec<RepoPath>,
    pub commands: Vec<CommandEvidence>,
    pub tests: Vec<TestEvidence>,
    pub acceptance_criteria: Vec<AcceptanceEvidence>,
    pub unresolved_todos: Vec<String>,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct VerificationEngine {
    secret_patterns: Vec<(&'static str, Regex)>,
}

impl VerificationEngine {
    pub fn new() -> Result<Self, regex::Error> {
        Ok(Self {
            secret_patterns: vec![
                (
                    "private_key",
                    Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----")?,
                ),
                ("openai_key", Regex::new(r"\bsk-[A-Za-z0-9_-]{20,}\b")?),
                (
                    "anthropic_key",
                    Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b")?,
                ),
                (
                    "github_token",
                    Regex::new(r"\b(?:ghp|github_pat)_[A-Za-z0-9_]{20,}\b")?,
                ),
                ("google_api_key", Regex::new(r"\bAIza[0-9A-Za-z_-]{30,}\b")?),
            ],
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn verify(&self, input: VerificationInput) -> EngineResult<VerificationResult> {
        let out_of_scope_files = input
            .snapshot
            .changed_files
            .iter()
            .filter(|path| !path_is_expected(path, &input.expected_paths))
            .cloned()
            .collect::<Vec<_>>();
        let secret_scan = self.scan_changed_files(
            &input.worktree_root,
            &input.snapshot.changed_files,
            &input.snapshot.diff,
            false,
        )?;
        let secret_scan_inconclusive = !secret_scan.truncated_files.is_empty();
        let command_failure = input.commands.iter().any(|command| {
            command.exit_code != Some(0) || command.timed_out || command.output_truncated
        });
        let test_failure = input
            .tests
            .iter()
            .any(|test| test.status != TestStatus::Passed);
        let acceptance_failure = input
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.status != VerificationStatus::Pass);
        let review_failure = input.independent_review_required
            && (!input.independent_review_passed
                || input.reviewer_provider.is_none()
                || input.reviewer_provider == Some(input.implementation_provider));

        let checks = vec![
            check(
                VerificationCheckKind::GitDiff,
                "authoritative Git snapshot",
                VerificationStatus::Pass,
                Some(format!(
                    "{} changed path(s)",
                    input.snapshot.changed_files.len()
                )),
            ),
            check(
                VerificationCheckKind::Scope,
                "changed-file scope",
                pass_if(out_of_scope_files.is_empty()),
                detail_if(
                    !out_of_scope_files.is_empty(),
                    format!(
                        "{} path(s) are outside the expected scope",
                        out_of_scope_files.len()
                    ),
                ),
            ),
            check(
                VerificationCheckKind::CommandExit,
                "command exit evidence",
                pass_if(!command_failure),
                detail_if(
                    command_failure,
                    "one or more commands failed or were truncated",
                ),
            ),
            check(
                VerificationCheckKind::Test,
                "test evidence",
                pass_if(!test_failure),
                detail_if(test_failure, "one or more tests did not pass"),
            ),
            check(
                VerificationCheckKind::AcceptanceCriterion,
                "acceptance criteria",
                pass_if(!acceptance_failure),
                detail_if(acceptance_failure, "acceptance evidence is incomplete"),
            ),
            check(
                VerificationCheckKind::UnresolvedTodo,
                "unresolved TODO review",
                pass_if(input.unresolved_todos.is_empty()),
                detail_if(
                    !input.unresolved_todos.is_empty(),
                    format!("{} unresolved item(s)", input.unresolved_todos.len()),
                ),
            ),
            check(
                VerificationCheckKind::SecretScan,
                "secret scan",
                secret_scan_status(&secret_scan),
                secret_scan_detail(&secret_scan),
            ),
            check(
                VerificationCheckKind::IndependentReview,
                "independent provider review",
                pass_if(!review_failure),
                detail_if(
                    review_failure,
                    "required independent review is missing or not independent",
                ),
            ),
        ];

        let failed = !out_of_scope_files.is_empty()
            || command_failure
            || test_failure
            || acceptance_failure
            || !input.unresolved_todos.is_empty()
            || !secret_scan.findings.is_empty()
            || review_failure;
        let requires_approval = !out_of_scope_files.is_empty()
            || !secret_scan.findings.is_empty()
            || secret_scan_inconclusive;

        Ok(VerificationResult {
            schema_version: SchemaVersion::v1(),
            verification_id: VerificationId::new(),
            task_id: input.task_id,
            implementation_provider: input.implementation_provider,
            reviewer_provider: input.reviewer_provider,
            status: if failed {
                VerificationStatus::Fail
            } else if secret_scan_inconclusive {
                VerificationStatus::Inconclusive
            } else {
                VerificationStatus::Pass
            },
            checks,
            acceptance_criteria: input.acceptance_criteria,
            changed_files: input.snapshot.changed_files,
            out_of_scope_files,
            unresolved_todos: input.unresolved_todos,
            requires_approval,
            verified_at: input.verified_at,
        })
    }

    /// Scans all persisted diff bytes (including removed/context lines) and changed files
    /// before a checkpoint or review prompt is written. This deliberately blocks secret-removal
    /// diffs from automatic handover: persisting the removed value would still disclose it.
    pub fn preflight_persistence(
        &self,
        root: &Path,
        snapshot: &GitSnapshot,
    ) -> EngineResult<SecretScanReport> {
        self.scan_changed_files(root, &snapshot.changed_files, &snapshot.diff, true)
    }

    fn scan_changed_files(
        &self,
        root: &Path,
        files: &[RepoPath],
        diff: &[u8],
        scan_all_diff_bytes: bool,
    ) -> EngineResult<SecretScanReport> {
        let root = fs::canonicalize(root).map_err(|error| EngineError::io(root, error))?;
        let added_lines;
        let diff_to_scan = if scan_all_diff_bytes {
            diff
        } else {
            added_lines = added_diff_lines(diff);
            &added_lines
        };
        let mut result = SecretScanReport {
            findings: self.scan_bytes("git_diff", diff_to_scan),
            truncated_files: Vec::new(),
        };
        for relative in files {
            let path = relative.join_to(&root);
            if !path.exists() {
                continue;
            }
            ensure_file_below_root(&root, &path)?;
            let metadata = fs::metadata(&path).map_err(|error| EngineError::io(&path, error))?;
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > MAX_SECRET_SCAN_BYTES_PER_FILE {
                result.truncated_files.push(relative.to_string());
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .map_err(|error| EngineError::io(&path, error))?
                .take(MAX_SECRET_SCAN_BYTES_PER_FILE)
                .read_to_end(&mut bytes)
                .map_err(|error| EngineError::io(&path, error))?;
            result
                .findings
                .extend(self.scan_bytes(&relative.to_string(), &bytes));
        }
        result.findings.sort_by(|left, right| {
            (&left.location, &left.kind).cmp(&(&right.location, &right.kind))
        });
        result.findings.dedup();
        result.truncated_files.sort();
        result.truncated_files.dedup();
        Ok(result)
    }

    fn scan_bytes(&self, location: &str, bytes: &[u8]) -> Vec<SecretFinding> {
        let text = String::from_utf8_lossy(bytes);
        self.secret_patterns
            .iter()
            .filter(|(_, pattern)| pattern.is_match(&text))
            .map(|(kind, _)| SecretFinding {
                location: location.to_owned(),
                kind: (*kind).to_owned(),
            })
            .collect()
    }
}

fn added_diff_lines(diff: &[u8]) -> Vec<u8> {
    let mut added = Vec::new();
    for line in diff.split_inclusive(|byte| *byte == b'\n') {
        if line.first() == Some(&b'+') && !line.starts_with(b"+++ ") {
            added.extend_from_slice(&line[1..]);
        }
    }
    added
}

const fn secret_scan_status(scan: &SecretScanReport) -> VerificationStatus {
    if !scan.findings.is_empty() {
        VerificationStatus::Fail
    } else if !scan.truncated_files.is_empty() {
        VerificationStatus::Inconclusive
    } else {
        VerificationStatus::Pass
    }
}

fn secret_scan_detail(scan: &SecretScanReport) -> Option<String> {
    match (scan.findings.len(), scan.truncated_files.len()) {
        (0, 0) => None,
        (findings, 0) => Some(format!("{findings} potential secret(s); values omitted")),
        (0, truncated) => Some(format!(
            "{truncated} file(s) exceeded the secret-scan byte limit; approval required"
        )),
        (findings, truncated) => Some(format!(
            "{findings} potential secret(s); {truncated} file(s) exceeded the scan limit; values omitted"
        )),
    }
}

fn check(
    kind: VerificationCheckKind,
    name: &str,
    status: VerificationStatus,
    detail: Option<String>,
) -> VerificationCheck {
    VerificationCheck {
        kind,
        name: name.to_owned(),
        status,
        detail,
        evidence_paths: Vec::new(),
    }
}

const fn pass_if(passes: bool) -> VerificationStatus {
    if passes {
        VerificationStatus::Pass
    } else {
        VerificationStatus::Fail
    }
}

fn detail_if(condition: bool, detail: impl Into<String>) -> Option<String> {
    condition.then(|| detail.into())
}

fn path_is_expected(path: &RepoPath, expected: &[RepoPath]) -> bool {
    expected
        .iter()
        .any(|prefix| path.as_path().starts_with(prefix.as_path()))
}

fn ensure_file_below_root(root: &Path, path: &Path) -> EngineResult<()> {
    let canonical = fs::canonicalize(path).map_err(|error| EngineError::io(path, error))?;
    if canonical.starts_with(root) {
        Ok(())
    } else {
        Err(EngineError::UnsafePath(canonical))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{ProviderId, RepoPath, TaskId, VerificationStatus};

    use crate::GitSnapshot;

    use super::{
        MAX_SECRET_SCAN_BYTES_PER_FILE, VerificationCheckKind, VerificationEngine,
        VerificationInput,
    };

    #[test]
    fn reports_secret_kind_without_secret_value() -> Result<(), Box<dyn std::error::Error>> {
        let engine = VerificationEngine::new()?;
        let findings = engine.scan_bytes("src/lib.rs", b"token=sk-abcdefghijklmnopqrstuvwxyz1234");
        assert_eq!(findings.len(), 1);
        let json = serde_json::to_string(&findings)?;
        assert!(!json.contains("abcdefghijklmnopqrstuvwxyz"));
        Ok(())
    }

    #[test]
    fn directory_scope_allows_nested_file() -> Result<(), Box<dyn std::error::Error>> {
        assert!(super::path_is_expected(
            &RepoPath::try_from("src/nested/lib.rs")?,
            &[RepoPath::try_from("src")?]
        ));
        Ok(())
    }

    #[test]
    fn deleted_diff_lines_are_not_scanned_as_current_secrets()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let engine = VerificationEngine::new()?;
        let deleted = RepoPath::try_from("deleted.env")?;
        let diff = b"diff --git a/deleted.env b/deleted.env\n--- a/deleted.env\n+++ /dev/null\n@@ -1 +0,0 @@\n-sk-abcdefghijklmnopqrstuvwxyz1234\n";

        let scan = engine.scan_changed_files(root.path(), &[deleted], diff, false)?;

        assert!(scan.findings.is_empty());
        assert!(scan.truncated_files.is_empty());
        Ok(())
    }

    #[test]
    fn persistence_preflight_blocks_secret_even_when_line_is_removed()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let engine = VerificationEngine::new()?;
        let snapshot = GitSnapshot {
            base_revision: "0123456789abcdef".to_owned(),
            head: "0123456789abcdef".to_owned(),
            status_porcelain: Vec::new(),
            diff: b"@@ -1 +0,0 @@\n-sk-abcdefghijklmnopqrstuvwxyz1234\n".to_vec(),
            changed_files: Vec::new(),
        };

        let scan = engine.preflight_persistence(root.path(), &snapshot)?;

        assert!(!scan.safe_to_persist_or_share());
        assert_eq!(scan.findings.len(), 1);
        Ok(())
    }

    #[test]
    fn added_diff_lines_are_still_scanned_for_secrets() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let engine = VerificationEngine::new()?;
        let diff = b"diff --git a/new.env b/new.env\n--- /dev/null\n+++ b/new.env\n@@ -0,0 +1 @@\n+sk-abcdefghijklmnopqrstuvwxyz1234\n";

        let scan = engine.scan_changed_files(root.path(), &[], diff, false)?;

        assert_eq!(scan.findings.len(), 1);
        assert_eq!(scan.findings[0].location, "git_diff");
        assert_eq!(scan.findings[0].kind, "openai_key");
        Ok(())
    }

    #[test]
    fn truncated_large_file_makes_secret_scan_inconclusive_and_requires_approval()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("large.bin");
        let file = std::fs::File::create(&path)?;
        file.set_len(MAX_SECRET_SCAN_BYTES_PER_FILE + 1)?;
        let relative = RepoPath::try_from("large.bin")?;
        let engine = VerificationEngine::new()?;

        let result = engine.verify(VerificationInput {
            task_id: TaskId::new(),
            implementation_provider: ProviderId::Codex,
            reviewer_provider: None,
            independent_review_required: false,
            independent_review_passed: false,
            snapshot: GitSnapshot {
                base_revision: "0123456789abcdef".to_owned(),
                head: "0123456789abcdef".to_owned(),
                status_porcelain: Vec::new(),
                diff: Vec::new(),
                changed_files: vec![relative.clone()],
            },
            worktree_root: root.path().to_path_buf(),
            expected_paths: vec![relative],
            commands: Vec::new(),
            tests: Vec::new(),
            acceptance_criteria: Vec::new(),
            unresolved_todos: Vec::new(),
            verified_at: Utc::now(),
        })?;

        assert_eq!(result.status, VerificationStatus::Inconclusive);
        assert!(result.requires_approval);
        let secret_check = result
            .checks
            .iter()
            .find(|check| check.kind == VerificationCheckKind::SecretScan)
            .ok_or("secret scan check must exist")?;
        assert_eq!(secret_check.status, VerificationStatus::Inconclusive);
        assert!(
            secret_check
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("approval required"))
        );
        Ok(())
    }
}
