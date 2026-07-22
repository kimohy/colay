# Provider Terminal Error and Lease Recovery Implementation Plan

**Goal:** Finalize confirmed provider terminal failures promptly and make abandoned coordinator/worker authority recoverable within a short bounded interval.

**Architecture:** A provider lifecycle error is an explicit terminal boundary: persist the event, request cancellation, wait for confirmed process-tree termination, and persist the attempt result. Coordinator and worker leases use short TTLs renewed while their owning futures are alive. A hard-killed owner stops renewing, so the existing atomic expiry/takeover transaction recovers authority without PID trust or manual SQL.

**Tech Stack:** Rust 2024, Tokio intervals/select, existing SQLite lease APIs, `orchestrator-test-support` fake provider binary, and Windows process supervision.

## Constraints

- Never release a worker lease when process-tree termination is unconfirmed.
- Preserve exact coordinator/worker ownership and parent-expiry ordering.
- Do not use PID liveness as authority and do not add manual lease deletion.
- Tests must use only the fake provider binary.

## Task 1: Terminal provider error finalization

- [x] Add a fake scenario that delays a terminal error beyond one renewal boundary and remains active until cancelled.
- [x] Add a CLI-layer regression proving terminal attempt persistence, worker renewal, and explicit release ordering.
- [x] Observe the regression produce `Failed` without cancellation before implementation.
- [x] Treat `WorkerEvent::Error` as terminal after its redacted audit event is persisted, then cancel and enter the existing confirmed wait path.

## Task 2: Renewable short leases

- [x] Replace the multi-hour coordinator TTL calculation with a short fixed recovery TTL.
- [x] Renew the coordinator while the coordinated operation future remains active.
- [x] Give worker leases a shorter child TTL and renew them from the active worker event loop.
- [x] Prove renewal keeps a live owner authoritative and simulated heartbeat loss permits atomic takeover after the bounded expiry.

## Task 3: Diagnostics and verification

- [x] Enrich CLI lease conflicts with coordinator renewal/expiry, active child count, and a safe retry time.
- [x] Run focused terminal-error and lease tests, fmt, full Clippy, npm tests, and the full fake-provider workspace suite on Windows.
- [x] Update `WSL-008` only after all finalization and recovery checks pass.
