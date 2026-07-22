# Daemon startup recovery design

## Problem

`colay daemon start` waits five seconds for an online daemon lease. The spawned daemon probes
configured providers before it acquires that lease. A slow, but healthy, probe therefore makes the
parent report a false timeout. The parent then drops its `Child` handle without terminating or
reaping the process, so that child can acquire the lease later and make a stopped repository appear
online again.

Startup stderr is currently discarded, which also removes the evidence needed to distinguish a slow
probe from an actual startup failure.

## Goals

- Publish owned startup state before provider probing begins.
- Distinguish `booting`, `probing`, `online`, and `failed` startup phases.
- Treat only `online` as a successful `daemon start` result.
- On a bounded startup timeout, terminate the exact spawned process tree and reap the child before
  returning an error.
- Return bounded, redacted startup diagnostics when the child exits or times out.
- Preserve exact lease ownership, repository-local state, schema migration safety, and fake-provider
  test isolation.

## State model

Schema migration 9 adds `phase` and `startup_error` to `daemon_instances`. Existing rows migrate to
`online`, preserving the meaning of schema 8 databases. `startup_error` is nullable and contains only
text passed through the configured redactor.

The public daemon instance record exposes the phase. `DaemonStatus` gains `Booting`, `Probing`, and
`Failed` variants in addition to `Online`, `Stale`, and `Stopped`. A non-expired lease is classified by
its phase; an expired lease remains `Stale` regardless of phase.

Only the lease owner may change its phase. Phase changes are monotonic:

```text
booting -> probing -> online
     \         \-> failed
      \----------> failed
```

`failed` records a redacted diagnostic and is then released. Historical rows retain the failure
evidence while the active status becomes `stopped`.

## Child startup sequence

The foreground server canonicalizes its executable and acquires a `booting` lease immediately after
opening the ready database. A lightweight startup heartbeat renews that lease and observes stop
requests while the process builds redaction/runtime services.

Before provider capability probes, the owner changes the phase to `probing`. It then constructs the
planner, executor, and integration services. Once every service required by the daemon loop is ready,
it changes the phase to `online`, stops the temporary startup heartbeat, and enters the normal daemon
loop using the already-owned lease. The normal loop must not reacquire the lease.

Any setup error is redacted, stored as `failed`, and the lease is released. A stop requested during
startup cancels setup and releases the lease without entering the normal loop.

## Parent supervision

`ensure_started` keeps the spawned `Child` handle and polls fresh database state. `Booting` and
`Probing` are progress, not success. `Online` succeeds. An early child exit fails immediately and
includes bounded redacted stderr.

The production startup deadline covers the bounded sequential provider-probe budget plus a small
setup margin. Tests may select a shorter deadline only when the `test-fixtures` feature is compiled.
At deadline, the parent terminates the process tree, waits for confirmed exit, reads bounded stderr,
and returns an error that includes the last observed phase and diagnostic. Windows uses `taskkill`
with separated arguments for tree termination. Unix starts the daemon in its own process group and
uses `kill` with separated arguments, escalating only after a bounded graceful wait.

No shell interpolation is used.

## Diagnostics

The child stderr pipe is retained by the parent and read only after child exit, so raw startup output
is never persisted. Output is bounded before being passed through the configured redactor. The
durable `startup_error` is written by the child from the same redacted error path.

## Tests

- State tests prove phase mapping, owner-only monotonic transitions, failure diagnostics, and schema-8
  migration compatibility.
- A CLI test uses only `colay-e2e-fake-provider` and delays its safe capability probe beyond the old
  five-second deadline. `daemon start` must succeed without returning a false timeout.
- A CLI test combines the same fake probe with the test-only short parent deadline. It must fail,
  remain stopped after the delayed probe would have completed, and allow a clean subsequent start.
- Existing repeated start/restart/stop lifecycle coverage must continue to pass.

## Out of scope

This change does not invoke real providers, change task execution authority, auto-merge worktrees, or
introduce external telemetry.
