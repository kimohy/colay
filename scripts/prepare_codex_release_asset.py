#!/usr/bin/env python3
"""Verify and safely prepare the official Codex Linux release package."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import re
import shutil
import tarfile
import tempfile
from pathlib import Path, PurePosixPath


EXPECTED_ASSET = "codex-package-x86_64-unknown-linux-musl.tar.gz"
EXPECTED_BINARY = PurePosixPath("bin/codex")
CHECKSUM_LINE = re.compile(r"^([0-9a-fA-F]{64})[ \t]+\*?(.+?)\s*$")


class ReleaseAssetError(ValueError):
    """The release package failed a trust-boundary validation."""


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _expected_checksum(checksums: Path) -> str:
    matches: list[str] = []
    for line in checksums.read_text(encoding="utf-8").splitlines():
        parsed = CHECKSUM_LINE.fullmatch(line)
        if parsed is not None and parsed.group(2) == EXPECTED_ASSET:
            matches.append(parsed.group(1).lower())
    if len(matches) != 1:
        raise ReleaseAssetError(
            f"expected exactly one checksum entry for {EXPECTED_ASSET}, found {len(matches)}"
        )
    return matches[0]


def _safe_member_path(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if not name or path.is_absolute() or path == PurePosixPath("."):
        raise ReleaseAssetError(f"unsafe archive path: {name!r}")
    if any(part in ("", ".", "..") for part in path.parts):
        raise ReleaseAssetError(f"unsafe archive path: {name!r}")
    return path


def _validated_members(package: tarfile.TarFile) -> list[tuple[tarfile.TarInfo, PurePosixPath]]:
    validated: list[tuple[tarfile.TarInfo, PurePosixPath]] = []
    seen: set[PurePosixPath] = set()
    for member in package.getmembers():
        path = _safe_member_path(member.name)
        if path in seen:
            raise ReleaseAssetError(f"duplicate archive path: {path.as_posix()}")
        seen.add(path)
        if not (member.isdir() or member.isreg()):
            raise ReleaseAssetError(
                f"unsupported archive member at {path.as_posix()}: type {member.type!r}"
            )
        validated.append((member, path))
    return validated


def _extract_regular_members(
    package: tarfile.TarFile,
    members: list[tuple[tarfile.TarInfo, PurePosixPath]],
    destination: Path,
) -> None:
    for member, relative in members:
        target = destination.joinpath(*relative.parts)
        if member.isdir():
            target.mkdir(parents=True, exist_ok=True)
            os.chmod(target, member.mode & 0o777)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        source = package.extractfile(member)
        if source is None:
            raise ReleaseAssetError(f"cannot read archive member: {relative.as_posix()}")
        with source, target.open("xb") as output:
            shutil.copyfileobj(source, output)
        os.chmod(target, member.mode & 0o777)


def prepare_release_asset(
    archive: Path,
    checksums: Path,
    output: Path,
    provenance: Path,
) -> dict[str, str]:
    archive = Path(archive)
    checksums = Path(checksums)
    output = Path(output)
    provenance = Path(provenance)
    if archive.name != EXPECTED_ASSET:
        raise ReleaseAssetError(
            f"unexpected release asset {archive.name!r}; expected {EXPECTED_ASSET!r}"
        )
    if output.exists():
        raise ReleaseAssetError(f"output already exists: {output}")
    expected = _expected_checksum(checksums)
    actual = _sha256(archive)
    if not hmac.compare_digest(expected, actual):
        raise ReleaseAssetError(
            f"checksum mismatch for {EXPECTED_ASSET}: expected {expected}, found {actual}"
        )

    output.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "r:gz") as package:
        members = _validated_members(package)
        binary_member = next(
            (member for member, path in members if path == EXPECTED_BINARY), None
        )
        if binary_member is None or not binary_member.isreg():
            raise ReleaseAssetError(
                f"missing expected executable: {EXPECTED_BINARY.as_posix()}"
            )
        if binary_member.mode & 0o111 == 0:
            raise ReleaseAssetError(
                f"expected executable is not executable: {EXPECTED_BINARY.as_posix()}"
            )

        with tempfile.TemporaryDirectory(
            dir=output.parent, prefix=".codex-release-"
        ) as temporary:
            staged = Path(temporary) / "payload"
            staged.mkdir()
            _extract_regular_members(package, members, staged)
            staged.replace(output)

    binary = output.joinpath(*EXPECTED_BINARY.parts)
    if os.name != "nt" and not os.access(binary, os.X_OK):
        raise ReleaseAssetError(
            f"expected executable is not executable after extraction: {EXPECTED_BINARY.as_posix()}"
        )
    result = {
        "asset_name": EXPECTED_ASSET,
        "asset_sha256": actual,
        "binary_relative_path": EXPECTED_BINARY.as_posix(),
    }
    provenance.parent.mkdir(parents=True, exist_ok=True)
    provenance.write_text(
        json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--checksums", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--provenance", required=True, type=Path)
    args = parser.parse_args()
    try:
        result = prepare_release_asset(
            args.archive, args.checksums, args.output, args.provenance
        )
    except (OSError, tarfile.TarError, ReleaseAssetError) as error:
        parser.exit(1, f"error: {error}\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
