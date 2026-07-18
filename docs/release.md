# Release compatibility record

This repository versions Colay and its persisted/public contracts independently.

| Component | Current value | Authority |
|---|---:|---|
| Colay | `0.1.0` | workspace `Cargo.toml` |
| Tested Codex | `0.144.4`, `0.144.5` | `compatibility/codex-version.toml` |
| Recommended Codex | `0.144.5` | `compatibility/codex-version.toml` |
| SQLite state schema | `3` | `STATE_SCHEMA_VERSION` and migrations |
| Config schema | `4` | `CONFIG_SCHEMA_VERSION` |
| Checkpoint/handover schema | `1` | domain writers |

## Release notes obligations

Every Colay release must state the supported/recommended Codex versions, state/config migration requirement, known compatibility limitations, and rollback manifest procedure. A production build must use an exact release artifact/revision, never upstream `main`.

Current known limitations:

- The CLI intentionally keeps `codex exec --json` as the default transport; selecting App Server first is an adapter policy rather than a user-facing CLI switch.
- Authoritative checkpoint diffs remain sensitive local source artifacts even though persistence/handover preflight blocks known secret patterns and oversized unscanned files.
- Routing policy can calculate a parallel count, but CLI dispatch currently forces one worker; concurrent read-only fan-out is not implemented.

See [`rollback.md`](rollback.md) for the release manifest, explicit approval, recovery journal, and restart procedure.
