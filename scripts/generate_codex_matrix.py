#!/usr/bin/env python3
"""Generate the committed Codex compatibility matrix from version manifests."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def capability(value: bool) -> str:
    return "pass" if value else "fail"


def simple_toml_scalars(path: Path) -> dict[str, Any]:
    """Parse the JSON-compatible scalar subset used by compatibility manifests."""
    values: dict[str, Any] = {}
    for original in path.read_text(encoding="utf-8").splitlines():
        line = original.strip()
        if not line or line.startswith("#") or line.startswith("["):
            continue
        key, separator, encoded = line.partition("=")
        if not separator:
            raise ValueError(f"unsupported manifest line in {path}: {original}")
        values[key.strip()] = json.loads(encoded.strip())
    return values


def build(root: Path) -> dict[str, Any]:
    catalog_path = root / "compatibility/codex-version.toml"
    catalog = simple_toml_scalars(catalog_path)
    tested = list(catalog["tested_versions"])
    recommended = catalog["recommended"]
    ordered = [recommended, *sorted((value for value in tested if value != recommended), reverse=True)]
    rows = []
    for version in ordered:
        manifest_path = root / f"fixtures/codex/versions/{version}/manifest.toml"
        manifest = simple_toml_scalars(manifest_path)
        if manifest["version"] != version:
            raise ValueError(f"manifest version mismatch: {manifest_path}")
        rows.append(
            {
                "version": version,
                "exec": capability(manifest["exec"]),
                "jsonl": capability(manifest["jsonl"]),
                "resume": capability(manifest["resume"]),
                "app_server": capability(manifest["app_server"]),
                "output_schema": capability(manifest["output_schema"]),
                "reasoning_effort": capability(manifest["reasoning_effort"]),
                "usage": capability(manifest["usage_events"]),
                "status": manifest["status"],
            }
        )
    return {
        "schema_version": "1",
        "generated_from": [
            "compatibility/codex-version.toml",
            "fixtures/codex/versions/*/manifest.toml",
        ],
        "versions": rows,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
    )
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = args.root.resolve()
    destination = root / "compatibility/codex-matrix.json"
    generated = build(root)
    if args.check:
        committed = json.loads(destination.read_text(encoding="utf-8"))
        if committed != generated:
            raise SystemExit(
                "compatibility/codex-matrix.json is stale; run scripts/generate_codex_matrix.py"
            )
        return
    destination.write_text(
        json.dumps(generated, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
