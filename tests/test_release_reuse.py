import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from io import BytesIO


spec = importlib.util.spec_from_file_location("release_reuse", Path(__file__).resolve().parents[1] / "tools/release-reuse.py")
reuse = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reuse)


class ReleaseReuseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.body = b"unchanged compressed package bytes"
        self.url = "https://github.com/example/repo/releases/download/v1.0.0/wg_1.0.0.tar.gz"
        self.artifact = {"format": "tar.gz", "url": self.url, "size": len(self.body), "sha256": hashlib.sha256(self.body).hexdigest()}
        self.baseline = {"packages": [{"name": "wg", "version": "1.0.0", "fingerprint": "abc", "artifacts": [self.artifact]}]}
        self.matrix = {"wg": {"changed": False, "version": "1.0.0", "fingerprint": "abc", "reusedFromGroup": "v1.0.0"}}

    def stage(self):
        return reuse.stage(self.baseline, self.matrix, self.directory, "https://example.test/releases/v2.0.0")

    def test_copies_exact_bytes_without_changing_version_digest_or_original_baseline(self):
        with patch.object(reuse, "urlopen", return_value=BytesIO(self.body)) as download:
            result = self.stage()
        download.assert_called_once_with(self.url, timeout=120)
        package = result["packages"][0]
        self.assertEqual(package["version"], "1.0.0")
        self.assertEqual(package["artifacts"][0]["sha256"], self.artifact["sha256"])
        self.assertEqual(package["artifacts"][0]["url"], "https://example.test/releases/v2.0.0/wg_1.0.0.tar.gz")
        self.assertEqual((self.directory / "wg_1.0.0.tar.gz").read_bytes(), self.body)
        self.assertEqual(self.artifact["url"], self.url)
        with patch.object(reuse, "urlopen") as download:
            self.stage()
            download.assert_not_called()

    def test_corrupt_and_oversized_downloads_are_never_staged(self):
        for content in (b"x" * len(self.body), self.body + b"extra"):
            with self.subTest(content=content), patch.object(reuse, "urlopen", return_value=BytesIO(content)):
                with self.assertRaises(ValueError):
                    self.stage()
                self.assertEqual(list(self.directory.iterdir()), [])

    def test_existing_corrupt_asset_is_not_overwritten_or_accepted(self):
        (self.directory / "wg_1.0.0.tar.gz").write_bytes(b"wrong")
        with self.assertRaises(ValueError):
            self.stage()

    def test_changed_packages_and_oci_images_do_not_download(self):
        self.matrix["wg"]["changed"] = True
        with patch.object(reuse, "urlopen") as download:
            self.stage()
            self.matrix["wg"]["changed"] = False
            self.artifact["format"] = "oci"
            self.stage()
            download.assert_not_called()

    def test_mismatched_reuse_matrix_is_rejected(self):
        self.matrix["wg"]["fingerprint"] = "different"
        with self.assertRaises(ValueError):
            self.stage()

    def test_unsafe_asset_paths_are_rejected(self):
        for url in ("file:///etc/passwd", "https://example.test/..", "https://example.test/%2foutside", "https://example.test/a%5cb"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                reuse.asset_name(url)


if __name__ == "__main__":
    unittest.main()
