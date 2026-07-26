from __future__ import annotations

import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

from scripts.prepare_codex_release_asset import (
    EXPECTED_ASSET,
    ReleaseAssetError,
    prepare_release_asset,
)


SCRIPT = Path(__file__).with_name("prepare_codex_release_asset.py")


class ReleaseAssetFixture:
    def __init__(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.archive = self.root / EXPECTED_ASSET
        self.checksums = self.root / "codex-package_SHA256SUMS"
        self.output = self.root / "output"
        self.provenance = self.root / "provenance.json"

    def close(self) -> None:
        self.temp.cleanup()

    def write_archive(
        self,
        *,
        binary_name: str = "bin/codex",
        binary_mode: int = 0o755,
        extra_members: list[tarfile.TarInfo] | None = None,
    ) -> None:
        with tarfile.open(self.archive, "w:gz") as package:
            directory = tarfile.TarInfo("bin/")
            directory.type = tarfile.DIRTYPE
            directory.mode = 0o755
            package.addfile(directory)

            binary = tarfile.TarInfo(binary_name)
            binary.mode = binary_mode
            payload = b"fake-codex"
            binary.size = len(payload)
            package.addfile(binary, io.BytesIO(payload))

            for member in extra_members or []:
                payload = b"duplicate" if member.isreg() else None
                if payload is not None:
                    member.size = len(payload)
                    package.addfile(member, io.BytesIO(payload))
                else:
                    package.addfile(member)

    def write_checksums(self, *digests: str) -> str:
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        selected = digests or (digest,)
        self.checksums.write_text(
            "".join(f"{value}  {EXPECTED_ASSET}\n" for value in selected),
            encoding="utf-8",
        )
        return digest


class ReleaseAssetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = ReleaseAssetFixture()

    def tearDown(self) -> None:
        self.fixture.close()

    def assert_rejected(self, message: str) -> None:
        with self.assertRaisesRegex(ReleaseAssetError, message):
            prepare_release_asset(
                self.fixture.archive,
                self.fixture.checksums,
                self.fixture.output,
                self.fixture.provenance,
            )
        self.assertFalse(self.fixture.provenance.exists())

    def test_prepares_checksum_verified_executable_and_provenance(self) -> None:
        self.fixture.write_archive()
        digest = self.fixture.write_checksums()

        result = prepare_release_asset(
            self.fixture.archive,
            self.fixture.checksums,
            self.fixture.output,
            self.fixture.provenance,
        )

        binary = self.fixture.output / "bin" / "codex"
        self.assertEqual(binary.read_bytes(), b"fake-codex")
        self.assertTrue(os.access(binary, os.X_OK))
        self.assertEqual(
            result,
            {
                "asset_name": EXPECTED_ASSET,
                "asset_sha256": digest,
                "binary_relative_path": "bin/codex",
            },
        )
        self.assertEqual(
            json.loads(self.fixture.provenance.read_text(encoding="utf-8")), result
        )

    def test_rejects_missing_checksum_entry(self) -> None:
        self.fixture.write_archive()
        self.fixture.checksums.write_text("", encoding="utf-8")
        self.assert_rejected("exactly one checksum entry")

    def test_rejects_duplicate_checksum_entries(self) -> None:
        self.fixture.write_archive()
        digest = hashlib.sha256(self.fixture.archive.read_bytes()).hexdigest()
        self.fixture.write_checksums(digest, digest.upper())
        self.assert_rejected("exactly one checksum entry")

    def test_rejects_checksum_mismatch(self) -> None:
        self.fixture.write_archive()
        self.fixture.write_checksums("0" * 64)
        self.assert_rejected("checksum mismatch")

    def test_rejects_absolute_archive_path(self) -> None:
        self.fixture.write_archive(binary_name="/bin/codex")
        self.fixture.write_checksums()
        self.assert_rejected("unsafe archive path")

    def test_rejects_parent_archive_path(self) -> None:
        self.fixture.write_archive(binary_name="../bin/codex")
        self.fixture.write_checksums()
        self.assert_rejected("unsafe archive path")

    def test_rejects_links(self) -> None:
        link = tarfile.TarInfo("bin/link")
        link.type = tarfile.SYMTYPE
        link.linkname = "codex"
        self.fixture.write_archive(extra_members=[link])
        self.fixture.write_checksums()
        self.assert_rejected("unsupported archive member")

    def test_rejects_duplicate_normalized_paths(self) -> None:
        duplicate = tarfile.TarInfo("bin/codex")
        duplicate.mode = 0o755
        self.fixture.write_archive(extra_members=[duplicate])
        self.fixture.write_checksums()
        self.assert_rejected("duplicate archive path")

    def test_rejects_missing_expected_binary(self) -> None:
        self.fixture.write_archive(binary_name="bin/not-codex")
        self.fixture.write_checksums()
        self.assert_rejected("missing expected executable")

    def test_rejects_non_executable_binary(self) -> None:
        self.fixture.write_archive(binary_mode=0o644)
        self.fixture.write_checksums()
        self.assert_rejected("is not executable")


class ReleaseAssetCliTests(unittest.TestCase):
    def test_cli_prepares_fixture_package(self) -> None:
        fixture = ReleaseAssetFixture()
        self.addCleanup(fixture.close)
        fixture.write_archive()
        fixture.write_checksums()

        completed = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--archive",
                str(fixture.archive),
                "--checksums",
                str(fixture.checksums),
                "--output",
                str(fixture.output),
                "--provenance",
                str(fixture.provenance),
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            json.loads(fixture.provenance.read_text(encoding="utf-8"))[
                "binary_relative_path"
            ],
            "bin/codex",
        )


if __name__ == "__main__":
    unittest.main()
