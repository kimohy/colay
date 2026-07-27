# WSL Nightly Real-Provider QA Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clean-install the public Colay nightly in WSL and verify user-facing behavior with available authenticated real providers without modifying the user's existing Colay state.

**Architecture:** Install the npm nightly into a timestamped isolated prefix and use a separate `COLAY_HOME` plus four non-Git workspaces. Keep the existing WSL user home only so installed provider CLIs can use their existing authentication, while recording no credential values.

**Tech Stack:** WSL 2 Ubuntu 24.04, npm, Colay nightly, SQLite, real provider CLIs.

## Global Constraints

- Do not use real provider inference from automated Rust tests or CI.
- Do not print, copy, or mutate provider credential values.
- Do not reuse or migrate the user's existing Colay database.
- Do not run writable task execution or approve worktree creation.
- Stop the isolated daemon and verify process/socket cleanup.

---

### Task 1: Establish the installation and provider baseline

**Files:**
- Create: `.superpowers/sdd/2026-07-28-wsl-nightly-real-provider-qa/probe-env.sh`
- Produce: `/home/kimohy/.cache/colay-real-provider-qa-20260728/evidence/baseline.txt`

- [ ] Run the probe script and record WSL, Node/npm, Colay absence, provider paths, provider versions, and the npm nightly dist-tag.
- [ ] Select only provider CLIs that are installed and return a bounded version response.

### Task 2: Perform an isolated clean nightly install

**Files:**
- Produce: `/home/kimohy/.cache/colay-real-provider-qa-20260728/npm-prefix`
- Produce: `/home/kimohy/.cache/colay-real-provider-qa-20260728/colay-home`

- [ ] Run `npm install --global --prefix <isolated-prefix> @kimohy/colay@nightly`.
- [ ] Verify root/native package versions and `colay --version` match the registry nightly version.
- [ ] Verify the installed command resolves only through the isolated prefix.

### Task 3: Validate first-run lifecycle and compatibility

**Files:**
- Produce: `/home/kimohy/.cache/colay-real-provider-qa-20260728/evidence/lifecycle`

- [ ] From a non-Git workspace, run safe help/version/compatibility/doctor and migration commands against the isolated `COLAY_HOME`.
- [ ] Start the daemon, confirm its executable/version/target and global database schema, then confirm workspace registration does not create repository-local state.

### Task 4: Exercise real-provider user flows

**Files:**
- Produce: `/home/kimohy/.cache/colay-real-provider-qa-20260728/evidence/providers`

- [ ] For every available compatible real provider, submit one bounded non-sensitive read-only prompt that asks for the exact token `COLAY_QA_OK`.
- [ ] Verify requested-provider selection, successful answer persistence, no worktree/task/lease creation, and clear diagnostics for unavailable providers.
- [ ] Exercise a second turn or resume path when the public CLI exposes it without writable approval.

### Task 5: Verify cleanup and record usability findings

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Produce: `/home/kimohy/.cache/colay-real-provider-qa-20260728/evidence/final`

- [ ] Stop the isolated daemon and verify PID/socket removal, SQLite integrity, foreign keys, and zero writable leases/worktrees.
- [ ] Record exact nightly/provider versions, commands, outcomes, user-facing friction, and reproducible defects without credentials or prompt content beyond the fixed QA token.
