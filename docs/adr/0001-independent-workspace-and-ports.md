# ADR 0001: Independent workspace and provider ports

Status: Accepted

Colay remains an independent Rust workspace. Providers are external official binaries behind a `WorkerAdapter`; Codex wire compatibility is isolated again behind `CodexCompatibilityAdapter`. Domain code cannot import provider wire types or private upstream crates.

This keeps provider upgrades confined to help/schema fixtures and adapters. Upstream patches are permitted only for a minimal integration hook after all official interfaces have been exhausted; routing, quota, handover, and persistence logic can never enter such a patch.
