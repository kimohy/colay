# Owner-bound workspace registration receipt

## Problem

The merge-triggered Windows CI run for `d743964` failed two `global_doctor` tests because
`colay daemon start` wrote `workspace.register` successfully but did not receive the response
within the existing ten-second IPC response budget. The same code passed both pull-request Windows
runs and a local full workspace run, while historical Windows runs show that `global_doctor` was
already load-sensitive before this provider-boundary change.

The timeout exposes duplicated work rather than an undersized normal latency budget. A fresh daemon
registers and imports its startup workspace before binding IPC. After readiness, the spawning client
unconditionally registers the same workspace again. A legacy inspection captures and validates a
SQLite family, migrates a snapshot, verifies event and document evidence, and hashes referenced
files. On Windows its scratch-path hardening also starts many `whoami.exe` and `icacls.exe`
processes. Raising the response timeout would retain this duplication and normalize a blocked writer.

## Decision

The daemon exposes an optional owner-bound bootstrap receipt in the existing schema-v1
`daemon.ping` data:

```json
{
  "ready": true,
  "owner_pid": 1234,
  "startup_workspace_id": "018f..."
}
```

The value contains only the opaque workspace UUID. It contains no repository, configuration,
state path, source fingerprint, or user content.

The client may reuse this receipt instead of sending `workspace.register` only when all of these
conditions hold:

1. this `connect_or_start` invocation spawned a daemon child;
2. readiness proves that exact child PID is the endpoint owner;
3. the daemon supplies a syntactically valid startup workspace ID; and
4. the child has not been classified as a contender.

The exact child received the same repository and explicit configuration arguments from this client,
and it completed bootstrap registration/import before IPC readiness. Its returned workspace ID is
therefore the result the duplicate registration would have produced.

Every other route keeps the existing registration path: an incumbent daemon, a winning daemon
spawned elsewhere, a reaped contender, a Windows legacy endpoint, an older daemon that omits the
new field, or a new workspace on an existing daemon. New workspace registration continues to finish
durable legacy import and activation enqueue before returning success, so commands cannot race the
import.

## Compatibility and safety

- IPC schema remains version 1. The field is additive.
- An old client ignores the field and performs the existing registration.
- A new client talking to an old daemon sees no receipt and performs the existing registration.
- A malformed workspace ID fails readiness closed instead of silently skipping registration.
- Existing owner-PID and endpoint identity validation remains authoritative.
- Bootstrap import stays before IPC bind. `LegacyImporter::apply` keeps its sealed-plan reinspection.
- SQLite writer ordering, append-only audit data, activation order, and source preservation are
  unchanged.
- The general ten-second IPC response timeout stays unchanged. Any later timeout adjustment requires
  measured evidence after duplicate removal.

## Test evidence

A test-fixtures-only legacy-inspection marker records a fixed phase token without paths or source
data. A cold legacy startup must record exactly two inspections: plan construction and the required
sealed-plan revalidation inside `apply`. Before the fix it records a third inspection from the
duplicate IPC registration.

Unit coverage verifies modern/legacy ping parsing, malformed receipt rejection, exact-owner reuse,
and fallback for incumbent/contender/old peers. Integration coverage verifies:

- cold legacy startup imports once, produces two inspection markers, and preserves the source;
- an existing daemon still imports and activates a second workspace before returning its ID;
- concurrent cold clients cannot reuse another contender's receipt; and
- Windows registration no longer fails under the targeted parallel doctor workload.

Automated tests use fake providers only. Test diagnostics contain monotonic phase/counter evidence,
not paths or credentials.

## Deferred optimization

Windows ACL validation is a separate security boundary. This change does not cache SID/DACL results
or weaken private-path enforcement. After receipt deduplication, retained timings determine whether a
separate verify-before-mutate/native-DACL optimization is justified.
