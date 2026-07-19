# Codex compatibility contracts

The orchestrator integrates only through public CLI JSONL and stable App Server
stdio messages. The exact tested releases are recorded in
`../codex-version.toml`; an unknown version is never authorized for writable
work solely because its version number is close to a tested release.

Automated fixture capture uses only `--version`, `--help`, and
`app-server generate-json-schema`. JSONL streams are curated, redacted contract
fixtures; CI never starts a model turn and consumes no provider quota. Generated
schemas must omit experimental fields.

The table below is a rendered view of the committed machine-readable
`../codex-matrix.json`. Contract tests fail when that matrix, the version
registry, or a per-version manifest diverges. Regenerate it with
`python scripts/generate_codex_matrix.py`; CI runs the same generator in
`--check` mode.

| Codex version | exec | JSONL | resume | app-server | usage | status |
|---|---|---|---|---|---|---|
| 0.144.6 | pass | pass | pass | pass | pass | supported |
| 0.144.5 | pass | pass | pass | pass | pass | supported |

No N-2 maintenance row is claimed until an actual third release has reviewed
fixtures. Unknown releases remain read-only/disabled according to the startup
guard even if their version is numerically adjacent.
