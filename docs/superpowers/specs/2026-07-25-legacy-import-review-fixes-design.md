# Legacy Import Review Fixes Design

## Scope

This follow-up closes four review findings in the global legacy repository-state importer. It does not change the public importer interfaces, migration schema, provider behavior, or unrelated state operations.

## Immutable evidence and transform scratch

Inspection continues to create and validate a stable read-only SQLite backup. Apply publishes the independently verified `legacy.db` evidence file unchanged and creates a separate private migrated validation image. After all source chain, document, requirement, artifact, migration, integrity, and foreign-key checks pass, apply copies the validated migrated image to a private temporary `rewrite.db`.

Before transformation, `rewrite.db` must contain the six known schema-13 immutable UPDATE triggers with the exact normalized definitions committed in migration 13. Only these triggers are removed from `rewrite.db`:

- `graph_revisions_immutable_payload`
- `graph_revision_authority_immutable`
- `graph_approvals_no_update`
- `integration_batches_payload_immutable`
- `integration_sources_no_update`
- `integration_approvals_no_update`

Any missing, changed, or unexpected definition fails closed before import. Other triggers remain active. The target connection attaches only `rewrite.db`; deterministic collision remaps, typed JSON rewrites, resealing, and propagated graph/integration hashes therefore mutate scratch data, never either evidence image.

## Target audit gate and mutation order

The target transaction starts before target validation. It validates every target event row in sequence, including row/JSON agreement, predecessor continuity, canonical event hashes, and the workspace `event_log_state` cursor when present. A cursor must identify an exact event-chain prefix; an absent cursor is equivalent to an unexported chain.

Only after this gate succeeds may apply create the pre-import target backup, copy rows, record mappings or ledger data, publish staged files, append an anchor, or commit. A validation error rolls back the transaction and the staging guard removes unpublished files.

## Requirement revision validation

Every migrated source `requirement_revisions` row is validated before scratch creation. The row schema must be supported; all identity fields must parse as their exact domain ID types; the ordinal must be positive; `snapshot_json` must decode as the deny-unknown-fields `RequirementSnapshot`; the stored `complete` bit must equal `RequirementSnapshot::is_complete`; and the stored `snapshot_hash` must equal the provider-neutral canonical SHA-256 of the typed snapshot. This validation occurs during both inspection and apply reinspection.

Requirement snapshots contain no embedded persistence IDs, so collision remapping changes row keys and foreign keys during import but does not reseal valid snapshot JSON.

## Source path containment

Every source database, JSONL, and artifact open or copy rechecks the entire root-to-file path with component-wise symbolic-link/reparse-point rejection. The canonical file path must remain beneath the canonical source root. After opening, the path is canonicalized again and must still resolve to the same contained file before bytes are trusted. File metadata and hashes are taken from the opened handle where possible.

## Regression coverage

Focused tests first demonstrate each failure:

- an approved, non-planning graph with a colliding identity imports and propagates its new proposal hash to graph approval;
- an integration batch with source, approval, and application collisions imports and propagates the new preview hash;
- a corrupt or gapped earlier target event and invalid target export cursor leave no imported rows, ledger, backup, or published files;
- mismatched requirement snapshot hashes and inconsistent row/snapshot completeness are rejected before target mutation;
- a nested symbolic-link or junction artifact path is refused on supported platforms.

The graph and integration tests hash source evidence files before and after apply. They also open the published `legacy.db` and verify that its graph proposal and integration preview retain the original identities and sealed hashes. Existing import/replay tests independently verify the published manifest seal.
