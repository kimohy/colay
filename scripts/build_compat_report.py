#!/usr/bin/env python3
"""Build a non-inference Codex compatibility report from captured public metadata."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any


HELP_FILES = (
    "root-help.txt",
    "exec-help.txt",
    "exec-resume-help.txt",
    "app-server-help.txt",
)
OPTION_PATTERN = re.compile(r"(?<![A-Za-z0-9_])--[A-Za-z0-9][A-Za-z0-9-]*")


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace") if path.is_file() else ""


def digest_files(paths: list[Path]) -> str | None:
    files = sorted(path for path in paths if path.is_file())
    if not files:
        return None
    digest = hashlib.sha256()
    for path in files:
        digest.update(path.name.encode())
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def exit_code(capture: Path, name: str) -> int | None:
    value = read_text(capture / f"{name}.exit-code").strip()
    try:
        return int(value)
    except ValueError:
        return None


def option_set(text: str) -> set[str]:
    return set(OPTION_PATTERN.findall(text))


def compare_help(capture: Path, fixture: Path) -> tuple[list[dict[str, Any]], list[str]]:
    changes: list[dict[str, Any]] = []
    broken: list[str] = []
    for name in HELP_FILES:
        code = exit_code(capture, name)
        current = read_text(capture / name)
        baseline = read_text(fixture / name)
        added = sorted(option_set(current) - option_set(baseline))
        removed = sorted(option_set(baseline) - option_set(current))
        changes.append(
            {
                "surface": name,
                "exit_code": code,
                "added_options": added,
                "removed_options": removed,
                "content_changed": bool(baseline) and current != baseline,
            }
        )
        if code not in (None, 0):
            broken.append(f"{name} exited with {code}")
    return changes, broken


def markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Codex compatibility report",
        "",
        f"- Subject: `{report['subject']}` ({report['kind']})",
        f"- Revision: `{report['revision']}`",
        "- Inference executed: `false`",
        f"- Existing support: {', '.join(report['existing_supported_versions']) or 'none'}",
        f"- Risk: `{report['risk']}`",
        "",
        "## Public command and option changes",
        "",
        "| Surface | Exit | Added options | Removed options | Changed |",
        "|---|---:|---|---|---|",
    ]
    for change in report["command_changes"]:
        lines.append(
            "| {surface} | {exit_code} | {added} | {removed} | {changed} |".format(
                surface=change["surface"],
                exit_code=change["exit_code"],
                added=", ".join(change["added_options"]) or "-",
                removed=", ".join(change["removed_options"]) or "-",
                changed=str(change["content_changed"]).lower(),
            )
        )
    lines.extend(
        [
            "",
            "## Protocol contracts",
            "",
            f"- JSONL: {report['jsonl_contract']['status']} — {report['jsonl_contract']['detail']}",
            f"- App Server: {report['app_server_contract']['status']} — {report['app_server_contract']['detail']}",
            "",
            "## Broken contracts",
            "",
        ]
    )
    lines.extend(
        [f"- {value}" for value in report["broken_contracts"]]
        or ["- None detected by the non-inference probe."]
    )
    lines.extend(
        [
            "",
            "## Required action",
            "",
            f"- Adapter change: `{str(report['adapter_change_required']).lower()}`",
            f"- State/config migration: `{str(report['migration_required']).lower()}`",
            f"- Recommendation: {report['recommendation']}",
            "",
            "This report cannot certify JSONL turn behavior because CI deliberately does not start a model turn.",
        ]
    )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--capture", required=True, type=Path)
    parser.add_argument("--matrix", required=True, type=Path)
    parser.add_argument("--fixtures", required=True, type=Path)
    parser.add_argument("--subject", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--kind", choices=("release", "upstream-main"), required=True)
    args = parser.parse_args()

    matrix = json.loads(args.matrix.read_text(encoding="utf-8"))
    supported = [row["version"] for row in matrix.get("versions", [])]
    recommended = supported[0] if supported else ""
    baseline = args.fixtures / recommended
    command_changes, broken = compare_help(args.capture, baseline)
    schema_files = list((args.capture / "schema").rglob("*.json"))
    captured_schema = digest_files(schema_files)
    baseline_schema = digest_files([baseline / "app-server-schema.json"])
    schema_changed = bool(captured_schema and baseline_schema and captured_schema != baseline_schema)
    schema_status = "changed" if schema_changed else "captured" if captured_schema else "unavailable"
    schema_code = exit_code(args.capture, "app-server-schema")
    if schema_code not in (None, 0):
        broken.append(f"stable App Server schema generation exited with {schema_code}")
    if not captured_schema:
        broken.append("stable App Server schema generation did not produce JSON")

    known_subject = args.subject.removeprefix("rust-v") in supported
    option_change = any(
        change["added_options"] or change["removed_options"]
        for change in command_changes
    )
    adapter_change = bool(broken or schema_changed or option_change or not known_subject)
    risk = "high" if broken else "medium" if adapter_change else "low"
    report: dict[str, Any] = {
        "schema_version": "1",
        "subject": args.subject,
        "kind": args.kind,
        "revision": args.revision,
        "inference_executed": False,
        "existing_supported_versions": supported,
        "baseline_version": recommended,
        "command_changes": command_changes,
        "jsonl_contract": {
            "status": "fixture-review-required",
            "detail": "CI runs the N/N-1 parser fixtures but does not sample a provider turn",
        },
        "app_server_contract": {
            "status": schema_status,
            "detail": "stable generated-schema digest was compared with the recommended fixture",
            "captured_sha256": captured_schema,
            "baseline_sha256": baseline_schema,
            "exit_code": schema_code,
        },
        "broken_contracts": broken,
        "adapter_change_required": adapter_change,
        "migration_required": False,
        "risk": risk,
        "recommendation": (
            "keep writable Codex disabled for this subject until a reviewed fixture and matrix row are committed"
            if adapter_change
            else "retain current support after human review of this report"
        ),
    }
    (args.capture / "compatibility-report.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    (args.capture / "compatibility-report.md").write_text(
        markdown(report), encoding="utf-8"
    )


if __name__ == "__main__":
    main()
