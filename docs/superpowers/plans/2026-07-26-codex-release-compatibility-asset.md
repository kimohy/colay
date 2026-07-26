# Codex Stable Compatibility Release Asset Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the scheduled stable compatibility monitor inspect the checksum-verified official Codex Linux x86_64 release package instead of rebuilding an upstream release tag with an incompatible lock file.

**Architecture:** A focused Python helper validates one allowlisted package name, parses its exact SHA-256 entry, rejects unsafe tar members, extracts regular files and directories without `extractall`, and writes asset provenance. The GitHub Actions workflow downloads the exact-tag package and checksum list, invokes the helper, and runs the existing non-inference public probes against `bin/codex`.

**Tech Stack:** Python 3 standard library (`argparse`, `hashlib`, `tarfile`, `unittest`), GitHub Actions YAML, GitHub CLI, existing Rust compatibility tests.

## Global Constraints

- Inspect only `codex-package-x86_64-unknown-linux-musl.tar.gz` selected from a validated `rust-v<semver>` tag.
- Verify the archive against exactly one matching entry in `codex-package_SHA256SUMS` before extraction.
- Reject absolute paths, parent traversal, links, devices, FIFOs, duplicate normalized paths, missing `bin/codex`, and a non-executable binary.
- Record tag, upstream source commit, asset filename, and verified SHA-256 in the capture report.
- Never add provider credentials, inference, usage scraping, unofficial endpoints, or telemetry.
- Keep draft-report publication permissions and behavior unchanged.
- Tests and CI must not invoke real provider inference.

---

### Task 1: Safe release package preparation

**Files:**
- Create: `scripts/prepare_codex_release_asset.py`
- Create: `scripts/test_prepare_codex_release_asset.py`

**Interfaces:**
- Produces: `prepare_release_asset(archive: Path, checksums: Path, output: Path, provenance: Path) -> dict[str, str]`.
- Produces CLI: `python scripts/prepare_codex_release_asset.py --archive PATH --checksums PATH --output DIR --provenance PATH`.
- Provenance JSON fields: `asset_name`, `asset_sha256`, `binary_relative_path`.

- [ ] **Step 1: Write failing valid-package and checksum tests**

Create an in-memory fixture tar with executable `bin/codex`, a matching checksum file, and assert:

```python
result = prepare_release_asset(archive, checksums, output, provenance)
self.assertEqual(result["asset_name"], EXPECTED_ASSET)
self.assertEqual(result["binary_relative_path"], "bin/codex")
self.assertEqual((output / "bin" / "codex").read_bytes(), b"fake-codex")
self.assertTrue(os.access(output / "bin" / "codex", os.X_OK))
```

- [ ] **Step 2: Run the test and confirm RED**

Run: `python -m unittest scripts.test_prepare_codex_release_asset -v`

Expected: import failure because `scripts.prepare_codex_release_asset` does not exist.

- [ ] **Step 3: Implement checksum parsing and safe extraction**

Implement these exact boundaries:

```python
EXPECTED_ASSET = "codex-package-x86_64-unknown-linux-musl.tar.gz"
EXPECTED_BINARY = PurePosixPath("bin/codex")

def prepare_release_asset(
    archive: Path,
    checksums: Path,
    output: Path,
    provenance: Path,
) -> dict[str, str]:
    ...
```

Parse only lowercase or uppercase 64-hex SHA-256 lines with the exact filename. Require one match,
hash before opening the tar, validate every member, extract directories and regular files through
`TarFile.extractfile`, preserve only permission bits, and write sorted indented JSON plus newline.

- [ ] **Step 4: Add failing negative tests one behavior at a time**

Cover missing checksum, duplicate checksum, mismatch, absolute path, `..` traversal, symlink,
duplicate normalized member, missing binary, and non-executable binary. Each test must assert the
specific `ReleaseAssetError` message and leave no accepted provenance.

- [ ] **Step 5: Run focused tests to GREEN**

Run: `python -m unittest scripts.test_prepare_codex_release_asset -v`

Expected: all release-asset tests pass without network access.

- [ ] **Step 6: Commit**

```text
git add scripts/prepare_codex_release_asset.py scripts/test_prepare_codex_release_asset.py
git commit -m "ci: verify Codex release package"
```

### Task 2: Stable compatibility workflow integration

**Files:**
- Modify: `.github/workflows/codex-release-compat.yml`
- Modify: `.github/workflows/ci.yml`
- Modify: `scripts/test_prepare_codex_release_asset.py`
- Modify: `docs/operations.md`

**Interfaces:**
- Consumes: Task 1 CLI and provenance JSON.
- Produces workflow outputs: `asset_name`, `asset_sha256`, and verified binary at `${RUNNER_TEMP}/codex-release/bin/codex`.

- [ ] **Step 1: Write a failing helper CLI contract test**

Invoke the real helper as a subprocess against the fixture archive and assert the process exit,
extracted binary, and parsed provenance:

```python
completed = subprocess.run(
    [sys.executable, str(SCRIPT), "--archive", str(archive), "--checksums", str(checksums),
     "--output", str(output), "--provenance", str(provenance)],
    check=False,
    capture_output=True,
    text=True,
)
self.assertEqual(completed.returncode, 0, completed.stderr)
self.assertEqual(json.loads(provenance.read_text())["binary_relative_path"], "bin/codex")
```

- [ ] **Step 2: Run the CLI contract test and confirm RED**

Run: `python -m unittest scripts.test_prepare_codex_release_asset.ReleaseAssetCliTests -v`

Expected: failure because the helper CLI is not implemented yet.

- [ ] **Step 3: Replace source build with exact release download and preparation**

Keep the exact-tag clone for source commit provenance. Add a download step using separated `gh`
arguments and exact patterns:

```bash
gh release download "$TAG" --repo openai/codex \
  --pattern codex-package-x86_64-unknown-linux-musl.tar.gz \
  --pattern codex-package_SHA256SUMS \
  --dir "$RUNNER_TEMP/codex-release-download"
python scripts/prepare_codex_release_asset.py \
  --archive "$RUNNER_TEMP/codex-release-download/codex-package-x86_64-unknown-linux-musl.tar.gz" \
  --checksums "$RUNNER_TEMP/codex-release-download/codex-package_SHA256SUMS" \
  --output "$RUNNER_TEMP/codex-release" \
  --provenance "$RUNNER_TEMP/codex-release-provenance.json"
```

Update capture to run `${RUNNER_TEMP}/codex-release/bin/codex`. Merge asset provenance with tag,
source commit, and `inference_executed: false` into `report.json` using Python JSON parsing rather
than shell-constructed JSON.

- [ ] **Step 4: Run workflow/helper tests to GREEN**

Run: `python -m unittest discover -s scripts -p "test_*.py" -v`

Expected: all helper API and CLI behavior tests pass. The workflow YAML is configuration wiring;
its behavioral RED is the recorded scheduled failure on run `30189827289`, and its GREEN is Task
3's exact-tag `workflow_dispatch` run rather than a source-text assertion.

- [ ] **Step 5: Document operations and run repository gates**

Document the verified release-package monitor and its fail-closed conditions in `docs/operations.md`.

Run:

```text
python scripts/generate_codex_matrix.py --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
npm test
```

Expected: all commands exit 0; all provider executions remain fake fixtures.

- [ ] **Step 6: Commit**

```text
git add .github/workflows/codex-release-compat.yml .github/workflows/ci.yml scripts/test_prepare_codex_release_asset.py docs/operations.md
git commit -m "ci: inspect verified Codex release artifacts"
```

### Task 3: Remote workflow proof

**Files:**
- No repository file changes expected.

**Interfaces:**
- Consumes: pushed or merged branch containing Tasks 1 and 2.
- Produces: one successful `workflow_dispatch` run and uploaded `codex-compat-rust-v0.145.0` artifact.

- [ ] **Step 1: Confirm remote execution authority**

Do not push, merge, or dispatch against `main` without explicit user authorization. If the branch
is not present remotely, report the exact limitation instead of claiming remote verification.

- [ ] **Step 2: Dispatch exact stable tag when authorized**

Run: `gh workflow run codex-release-compat.yml -R kimohy/colay --ref <remote-branch> -f tag=rust-v0.145.0`

Expected: a scheduled-compatible manual run starts on the implementation commit.

- [ ] **Step 3: Inspect the run**

Run: `gh run watch <run-id> -R kimohy/colay --exit-status`

Expected: inspect job and artifact upload succeed; draft publication stays skipped unless the
repository variable explicitly enables it.
