# Durable Session Daemon Phase 1 Audit

Date: 2026-07-21

## Requirement evidence

| Requirement | Direct evidence |
| --- | --- |
| Durable session round-trip and atomic audit | `sessions::tests::session_creation_and_transition_are_atomic_with_events` |
| Ordered messages, task targeting, and finalization | `sessions::tests::messages_keep_per_session_order_and_task_target`; `sessions::tests::streaming_message_finalizes_once_and_only_in_its_session` |
| Idempotent command replay and one claim winner | `client_commands::tests::identical_idempotency_submission_returns_original`; `client_commands::tests::concurrent_claim_has_one_winner` |
| Conservative stale command recovery | `client_commands::tests::stale_replay_safe_commands_requeue_but_stop_requires_reconciliation` |
| Historical event hashes survive v4 | `migration_contract::v3_event_hash_remains_verifiable_after_v4_migration` |
| One repository daemon owner | `daemon_instances::tests::concurrent_acquisition_has_one_winner`; exact-expiry takeover test in the same module |
| Runtime heartbeat, stop, cancellation, release | Both `orchestrator-daemon` Tokio tests |
| Start/status/stop/restart and child survival | `orchestrator-cli/tests/daemon_lifecycle.rs`; separate temporary-repository manual run observed stopped -> online instance A -> online instance B -> stopped |
| TUI-independent lifetime | CLI process test returns from `start`, queries the independent child, then stops it; post-test process scan found no `colay daemon serve` child |
| No network listener | Source scan for `TcpListener`, `TcpStream`, `UdpSocket`, `hyper`, `axum`, `reqwest`, `tonic`, and `warp` in the daemon boundary returned no matches; direct daemon dependencies are chrono, domain, state, thiserror, tokio, and tokio-util |

## Verification gates

The following commands exited successfully in the Phase 1 worktree:

```text
cargo fmt --all -- --check
cargo test -p orchestrator-domain
cargo test -p orchestrator-state
cargo test -p orchestrator-daemon
cargo test -p colay --features test-fixtures daemon
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

All provider-facing integration coverage used compiled fake providers. No
provider inference or network control plane was started.

## Scope boundary

Phase 1 is complete for durable sessions and daemon lifecycle. Task-DAG
planning, parallel execution, integration approval, recovery UX, and the new
chat-first TUI remain later phases and are not claimed by this audit.
