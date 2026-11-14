"""Offline tests for the real runtime manifest and archive boundary."""

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from build import OUT, PREFIX, check_archive, read_manifest


class RuntimeBoundaryTests(unittest.TestCase):
    def test_archives_reject_changed_bytes_and_wrong_sizes(self) -> None:
        with tempfile.TemporaryDirectory(dir=OUT) as directory:
            path = Path(directory) / "source.tar.xz"
            path.write_bytes(b"known archive bytes")
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            check_archive(path, path.stat().st_size, digest)
            with self.assertRaisesRegex(ValueError, "size differs"):
                check_archive(path, path.stat().st_size + 1, digest)
            with self.assertRaisesRegex(ValueError, "hash differs"):
                check_archive(path, path.stat().st_size, "0" * 64)

    def test_manifest_rejects_mutable_images_and_parent_paths(self) -> None:
        with tempfile.TemporaryDirectory(dir=OUT) as directory:
            path = Path(directory) / "manifest.json"
            manifest = {
                "format": 1, "prefix": PREFIX,
                "builder_image": "rust@sha256:" + "0" * 64,
                "runtime_image": "debian@sha256:" + "0" * 64,
                "python": {"filename": "source.tar.xz", "bytes": 1, "sha256": "0" * 64},
                "packages": [],
            }
            path.write_text(json.dumps(manifest))
            self.assertEqual(read_manifest(path)["python"]["filename"], "source.tar.xz")
            manifest["builder_image"] = "rust:latest"
            path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "immutable"):
                read_manifest(path)
            manifest["builder_image"] = "rust@sha256:" + "0" * 64
            manifest["python"]["filename"] = "../source.tar.xz"
            path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "basename"):
                read_manifest(path)


if __name__ == "__main__":
    OUT.mkdir(parents=True, exist_ok=True)
    unittest.main()
