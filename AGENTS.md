# Repository guidance

- This is a local-only enterprise orchestrator. Never add identity rotation, quota bypass, usage-page scraping, credential extraction, credit purchasing, unofficial endpoints, or default external telemetry.
- Tests must use `orchestrator-test-support` fake binaries. Do not invoke real `codex`, `claude`, or `gemini` inference from tests or CI.
- Use Rust `Command` with separated executable/args; do not add shell interpolation.
- Keep provider wire types inside provider/compatibility crates. `orchestrator-domain` must remain vendor-neutral and I/O-free.
- Missing usage stays unknown. Never compare raw quota units across providers.
- Writable worker changes occur in isolated worktrees. Reviewers are read-only. Do not auto-merge, push, or delete a worktree.
- Preserve schema versions, append-only audit semantics, redaction, and explicit approval gates when modifying persisted data.
- Required verification: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

