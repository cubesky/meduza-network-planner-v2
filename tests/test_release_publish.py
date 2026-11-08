"""Inject GitHub failures into the real publisher without contacting or changing a remote release."""

from collections import Counter
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release_publish", ROOT / "tools/release-publish.py")
api = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(api)


class GitHub:
    def __init__(self, exists=False):
        self.release = {"databaseId": 42, "isDraft": True, "tagName": "v0.2.4"} if exists else None
        self.assets, self.blobs, self.faults, self.calls = {}, {}, {}, []

    def asset(self, name, body):
        self.blobs[name] = body
        self.assets[name] = {"name": name, "state": "uploaded", "size": len(body), "digest": "sha256:" + hashlib.sha256(body).hexdigest()}

    def __call__(self, args, **kwargs):
        assert kwargs == dict(capture_output=True, text=True, timeout=180)
        operation = args[2] if args[1] == "release" else "assets"
        self.calls.append((operation, args))
        fault = self.faults.get(operation, []).pop(0) if self.faults.get(operation) else None
        def response(code=0, stdout="", stderr=""):
            return subprocess.CompletedProcess(args, code, stdout, stderr)
        if not fault or fault[1]:
            if operation == "view":
                result = response(stdout=json.dumps(self.release)) if self.release else response(1, stderr="release not found")
            elif operation == "create":
                assert "--draft" in args and "--verify-tag" in args
                if self.release:
                    result = response(1, stderr="HTTP 422: already_exists")
                else:
                    self.release = {"databaseId": 42, "isDraft": True, "tagName": "v0.2.4"}
                    result = response()
            elif operation == "assets":
                assert "--paginate" in args and "--slurp" in args
                # Multiple pages must be flattened even when the requested name is on the last page.
                values = list(self.assets.values())
                result = response(stdout=json.dumps([values[:1], values[1:]]))
            elif operation == "upload":
                assert "--clobber" not in args
                path = Path(args[4])
                self.asset(path.name, path.read_bytes())
                result = response()
            elif operation == "download":
                name = args[args.index("--pattern") + 1]
                directory = Path(args[args.index("--dir") + 1])
                (directory / name).write_bytes(self.blobs[name])
                result = response()
            elif operation == "edit":
                assert "--draft=false" in args
                self.release["isDraft"] = False
                result = response()
            else:
                raise AssertionError(args)
        if fault:
            if fault[0] == "timeout":
                raise subprocess.TimeoutExpired(args, 180)
            return response(1, stderr=fault[0])
        return result

    def count(self, operation):
        return Counter(op for op, _ in self.calls)[operation]


class ReleasePublishTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="meduza-release-publish-")
        self.addCleanup(self.temp.cleanup)
        self.file = Path(self.temp.name) / "meduza-np-planner_0.2.4_linux-amd64.tar.gz"
        self.file.write_bytes(b"signed package fixture")
        self.sleeps = []

    def publisher(self, remote):
        return api.Publisher("cubesky/meduza-network-planner-v2", "v0.2.4", run=remote, sleep=self.sleeps.append)

    def test_creation_http_500_is_retried_then_uploads(self):
        remote = GitHub()
        remote.faults["create"] = [("HTTP 500 (https://api.github.com/repos/example/releases)", False)]
        self.publisher(remote).upload([self.file])
        self.assertEqual(remote.count("create"), 2)
        self.assertEqual(remote.count("upload"), 1)
        self.assertTrue(remote.release["isDraft"])
        self.assertEqual(len(self.sleeps), 1)

    def test_transient_view_error_never_means_missing_release(self):
        remote = GitHub(True)
        remote.faults["view"] = [("HTTP 503", False)]
        self.publisher(remote).ensure()
        self.assertEqual(remote.count("create"), 0)
        self.assertEqual(len(self.sleeps), 1)

    def test_accepted_creation_with_lost_response_is_not_repeated(self):
        for error in ("HTTP 500", "timeout", "context deadline exceeded", "EOF", "HTTP 422: already_exists"):
            with self.subTest(error=error):
                remote = GitHub()
                remote.faults["create"] = [(error, True)]
                self.publisher(remote).ensure()
                self.assertEqual(remote.count("create"), 1)

    def test_successful_creation_waits_for_release_visibility(self):
        remote = GitHub()
        remote.faults["view"] = [None, ("release not found", False)]
        self.publisher(remote).ensure()
        self.assertEqual(remote.count("create"), 1)
        self.assertEqual(len(self.sleeps), 1)

    def test_permission_failure_is_not_retried_or_mistaken_for_missing(self):
        for error in ("HTTP 401: Bad credentials", "HTTP 403: Resource not accessible by integration"):
            with self.subTest(error=error):
                remote = GitHub(True)
                remote.faults["view"] = [(error, False)]
                with self.assertRaises(api.GhError):
                    self.publisher(remote).ensure()
                self.assertEqual(remote.count("view"), 1)
                self.assertEqual(remote.count("create"), 0)
        self.assertEqual(self.sleeps, [])

    def test_persistent_http_500_exhausts_a_bounded_creation_budget(self):
        remote = GitHub()
        remote.faults["create"] = [("HTTP 500", False)] * api.MAX_ATTEMPTS
        with self.assertRaisesRegex(api.PublishError, "after 6 attempts"):
            self.publisher(remote).ensure()
        self.assertEqual(remote.count("create"), 6)
        self.assertEqual(len(self.sleeps), 5)

    def test_upload_500_retries_only_when_the_asset_is_still_missing(self):
        remote = GitHub(True)
        remote.faults["upload"] = [("HTTP 500", False)]
        self.publisher(remote).upload([self.file])
        self.assertEqual(remote.count("upload"), 2)
        self.assertEqual(len(self.sleeps), 1)

    def test_upload_reply_lost_or_conflict_reconciles_the_existing_asset(self):
        for error in ("HTTP 502", "timeout", "HTTP 422: already_exists", "an asset with the same name already exists"):
            with self.subTest(error=error):
                remote = GitHub(True)
                remote.faults["upload"] = [(error, True)]
                self.publisher(remote).upload([self.file])
                self.assertEqual(remote.count("upload"), 1)

    def test_invalid_creation_request_is_not_retried(self):
        remote = GitHub()
        remote.faults["create"] = [("HTTP 422: invalid release field", False)]
        with self.assertRaises(api.GhError):
            self.publisher(remote).ensure()
        self.assertEqual(remote.count("create"), 1)
        self.assertEqual(self.sleeps, [])

    def test_rate_limit_uses_a_longer_delay_and_recovers(self):
        remote = GitHub(True)
        remote.faults["view"] = [("HTTP 403: secondary rate limit", False)]
        self.publisher(remote).ensure()
        self.assertEqual(remote.count("create"), 0)
        self.assertGreaterEqual(self.sleeps[0], 60)

    def test_existing_assets_on_later_pages_are_verified_and_skipped(self):
        remote = GitHub(True)
        remote.asset("unrelated.apk", b"other asset")
        remote.asset(self.file.name, self.file.read_bytes())
        self.publisher(remote).upload([self.file])
        self.assertEqual(remote.count("upload"), 0)

    def test_existing_corrupt_or_incomplete_assets_are_never_overwritten(self):
        for changed in ("digest", "size", "state"):
            with self.subTest(changed=changed):
                remote = GitHub(True)
                remote.asset(self.file.name, self.file.read_bytes())
                remote.assets[self.file.name][changed] = {"digest": "sha256:" + "0" * 64, "size": 0, "state": "starter"}[changed]
                with self.assertRaisesRegex(api.PublishError, "refusing to overwrite"):
                    self.publisher(remote).upload([self.file])
                self.assertEqual(remote.count("upload"), 0)

    def test_legacy_asset_without_digest_is_downloaded_and_verified(self):
        remote = GitHub(True)
        remote.asset(self.file.name, self.file.read_bytes())
        remote.assets[self.file.name].pop("digest")
        self.publisher(remote).upload([self.file])
        self.assertEqual(remote.count("download"), 1)
        self.assertEqual(remote.count("upload"), 0)

    def test_corrupt_legacy_download_is_not_accepted(self):
        remote = GitHub(True)
        remote.asset(self.file.name, self.file.read_bytes())
        remote.assets[self.file.name]["digest"] = None
        remote.blobs[self.file.name] = b"bad remote bytes"
        with self.assertRaisesRegex(api.PublishError, "SHA-256 differs"):
            self.publisher(remote).upload([self.file])
        self.assertEqual(remote.count("upload"), 0)

    def test_asset_list_retries_temporary_service_errors(self):
        remote = GitHub(True)
        remote.faults["assets"] = [("HTTP 503", False)]
        self.publisher(remote).upload([self.file])
        self.assertEqual(remote.count("assets"), 2)
        self.assertEqual(remote.count("upload"), 1)

    def test_finalize_reconciles_a_lost_successful_reply(self):
        remote = GitHub(True)
        remote.faults["edit"] = [("HTTP 500", True)]
        self.publisher(remote).finalize()
        self.assertFalse(remote.release["isDraft"])
        self.assertEqual(remote.count("edit"), 1)

    def test_finalize_retries_transient_failure_and_skips_published_releases(self):
        remote = GitHub(True)
        remote.faults["edit"] = [("HTTP 500", False)]
        publisher = self.publisher(remote)
        publisher.finalize()
        self.assertEqual(remote.count("edit"), 2)
        publisher.finalize()
        self.assertEqual(remote.count("edit"), 2)

    def test_real_workflow_uses_the_publisher_for_upload_and_finalize(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
        self.assertIn("tools/release-publish.py upload", workflow)
        self.assertIn("tools/release-publish.py finalize", workflow)
        self.assertNotIn("if ! existing=$(gh release view", workflow)


if __name__ == "__main__":
    unittest.main()
