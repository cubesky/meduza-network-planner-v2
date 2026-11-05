"""Static release discovery, Pages history migration and API-free SBOM reuse."""

import copy
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("pages_api", ROOT / "tools/pages-api.py")
api = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(api)
REPO = "example/meduza"


@contextmanager
def server(files):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            requests.append(self.path)
            body = files.get(self.path.lstrip("/"))
            status = 200 if body is not None else 404
            if isinstance(body, tuple):
                status, body = body
            self.send_response(status)
            self.end_headers()
            self.wfile.write(body or b"missing")

    http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=http.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{http.server_port}", requests
    finally:
        http.shutdown()
        http.server_close()
        thread.join()


def sbom(name, version):
    return api.encoded({"bomFormat": "CycloneDX", "metadata": {"component": {"name": name, "version": version}}})


def fixture(site, tag="v0.10.0", sequence=10, channel="stable", pkg_version="0.9.0"):
    filename = f"meduza-np-wireguard_{pkg_version}_linux-amd64.tar.gz"
    data = b"verified archive"
    group = {
        "formatVersion": 1, "groupVersion": tag, "sequence": sequence, "sourceCommit": "0123456789abcdef",
        "publishedAt": "2026-11-02T00:00:00Z", "expiresAt": "2026-12-02T00:00:00Z", "notes": "Package changes",
        "platforms": ["linux/amd64"], "compatibility": {"plannerApi": {"min": 1, "max": 1}},
        "packages": [
            {"name": "meduza-np-wireguard", "version": pkg_version, "fingerprint": "same", "requires": ["meduza-np-planner"],
             "conflicts": [], "reusedFromGroup": "v0.9.0", "artifacts": [
                 {"platform": "linux/amd64", "format": "tar.gz", "size": len(data), "sha256": api.sha256(data),
                  "url": f"https://github.com/{REPO}/releases/download/{tag}/{filename}", "oci": "ghcr.io/example/wireguard@sha256:123"}]},
            {"name": "meduza-np-base-container", "version": tag.lstrip("v"), "fingerprint": "new", "requires": [], "conflicts": [],
             "upstream": {"versions": [{"tag": "v1.0.0", "assets": [{"url": "https://downloads.example/core.gz", "sha256": "trusted"}]}]},
             "artifacts": [{"platform": "linux/amd64", "format": "oci", "url": "oci://ghcr.io/example/base@sha256:456",
                            "oci": "ghcr.io/example/base@sha256:456", "size": 0, "sha256": "456"}]},
        ],
    }
    raw = api.encoded(group)
    api.write(site / f"releases/{tag}/manifest.json", raw)
    api.write(site / f"releases/{tag}/manifest.json.sig", b"original manifest signature bytes\n")
    pointer = {"formatVersion": 1, "channel": channel, "groupVersion": tag, "sequence": sequence,
               "manifest": f"releases/{tag}/manifest.json", "manifestSha256": api.sha256(raw),
               "publishedAt": group["publishedAt"], "expiresAt": group["expiresAt"]}
    api.write(site / f"channels/{channel}.json", pointer)
    api.write(site / f"channels/{channel}.json.sig", b"original channel signature bytes\n")
    return group, filename, data


def files_under(site):
    return {str(path.relative_to(site)).replace("\\", "/"): path.read_bytes() for path in site.rglob("*") if path.is_file()}


def chunked_fixture(site, base, tag="v0.10.0", sequence=10, body=None):
    group, filename, _ = fixture(site, tag, sequence)
    body = body if body is not None else bytes(i % 251 for i in range(1_000_017))
    artifact = group["packages"][0]["artifacts"][0]
    digest = api.sha256(body)
    relative = f"packages/{digest}/{filename}"
    artifact.update(size=len(body), sha256=digest, url=f"{base}/{relative}", chunks=[])
    api.write(site / relative, body)
    for offset in range(0, len(body), api.CHUNK_BYTES):
        data = body[offset:offset + api.CHUNK_BYTES]
        path = f"packages/{digest}/{len(artifact['chunks']):06}.part"
        artifact["chunks"].append(dict(url=path, size=len(data), sha256=api.sha256(data)))
        api.write(site / path, data)
    raw = api.encoded(group)
    api.write(site / f"releases/{tag}/manifest.json", raw)
    pointer = site / "channels/stable.json"
    value = json.loads(pointer.read_bytes())
    value["manifestSha256"] = api.sha256(raw)
    api.write(pointer, value)
    return group, relative, body


class PagesTests(unittest.TestCase):
    def test_chunk_files_and_full_archive_survive_history_merge_with_content_deduplication(self):
        old = self.root / "old"
        files = {}
        with server(files) as (base, requests):
            previous, relative, body = chunked_fixture(old, base, "v0.9.0", 9)
            api.write(old / "index.json", {"groups": ["v0.9.0"], "channels": ["stable"]})
            files.update(files_under(old))
            group, new_relative, _ = chunked_fixture(self.new, base)
            index = api.build(self.new, self.out, REPO, base)
        self.assertEqual(index["groups"], ["v0.10.0", "v0.9.0"])
        self.assertEqual(relative, new_relative)
        self.assertEqual((self.out / relative).read_bytes(), body)
        self.assertEqual(len(list((self.out / "packages").rglob("*.part"))), 3)
        self.assertEqual(len(list((self.out / "packages").rglob("*.tar.gz"))), 1)
        self.assertFalse(any(path.startswith("/packages/") for path in requests), "new content also satisfies historical artifacts")
        artifact = group["packages"][0]["artifacts"][0]
        self.assertEqual(b"".join((self.out / p["url"]).read_bytes() for p in artifact["chunks"]), body)
        value = json.loads((self.out / "api/v1/releases/v0.10.0.json").read_bytes())
        self.assertTrue(value["assets"][0]["chunks"][0]["url"].startswith(base + "/packages/"))

    def test_history_downloads_full_archive_once_and_rebuilds_verified_chunks(self):
        old = self.root / "old"
        files = {}
        with server(files) as (base, requests):
            group, relative, body = chunked_fixture(old, base, "v0.9.0", 9)
            api.write(old / "index.json", {"groups": ["v0.9.0"], "channels": ["stable"]})
            files.update(files_under(old))
            fixture(self.new)
            api.build(self.new, self.out, REPO, base)
        self.assertEqual([p for p in requests if p.startswith("/packages/")], ["/" + relative])
        self.assertEqual((self.out / relative).read_bytes(), body)
        for part in group["packages"][0]["artifacts"][0]["chunks"]:
            self.assertEqual(api.file_digest(self.out / part["url"]), (part["size"], part["sha256"]))

    def test_corrupt_chunks_and_site_capacity_block_pages_deployment(self):
        with server({}) as (base, _):
            group, _, _ = chunked_fixture(self.new, base)
            group["packages"][0]["artifacts"][0]["chunks"][0]["sha256"] = "0" * 64
            # A full archive alone cannot justify parts that disagree with its signed chunk list.
            with self.assertRaisesRegex(ValueError, "chunk differs"):
                api.restore_packages(group, self.new, self.out, base)
            with patch.object(api, "MAX_SITE_BYTES", 1), self.assertRaisesRegex(ValueError, "published-site limit"):
                api.build(self.new, self.root / "too-large", REPO, base)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="meduza-pages-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.new = self.root / "new"
        self.out = self.root / "site"

    def complete_release(self, base="https://example.github.io/meduza"):
        group, filename, data = fixture(self.new)
        artifacts, sboms, repo = self.root / "dist", self.root / "sbom", self.root / "repo"
        api.write(artifacts / filename, data)
        for package in group["packages"]:
            api.write(sboms / f"{package['name']}_{package['version']}.cdx.json", sbom(package["name"], package["version"]))
            api.write(repo / "packages" / package["name"] / "manifest.json", {"role": "service-adapter", "provides": ["test.capability"]})
        value = api.release(self.new, group["groupVersion"], REPO, base, artifacts, sboms, repo)
        return group, value

    def test_complete_inventory_has_downloads_sboms_images_reuse_and_upstream_data(self):
        group, value = self.complete_release()
        self.assertTrue(value["inventoryComplete"])
        self.assertEqual(len(value["assets"]), 3)
        self.assertEqual(len(value["images"]), 2)
        self.assertFalse(value["packages"][0]["changed"])
        self.assertTrue(value["packages"][1]["changed"])
        self.assertEqual(value["packages"][0]["version"], "0.9.0")
        self.assertEqual(value["packages"][0]["metadata"]["provides"], ["test.capability"])
        self.assertEqual(value["packages"][1]["upstream"], group["packages"][1]["upstream"])
        self.assertEqual(value["manifest"]["sha256"], api.sha256((self.new / "releases/v0.10.0/manifest.json").read_bytes()))
        self.assertIn(".cdx.json", (self.new / "releases/v0.10.0/checksums.txt").read_text())

    def test_first_release_and_latest_channel_need_only_pages(self):
        with server({}) as (base, requests):
            _, value = self.complete_release(base)
            index = api.build(self.new, self.out, REPO, base)
        self.assertEqual(requests, ["/index.json"])
        self.assertEqual(index["groups"], ["v0.10.0"])
        self.assertEqual(index["channels"], ["stable"])
        latest = json.loads((self.out / "api/v1/channels/stable.json").read_bytes())
        self.assertEqual(latest["assets"], value["assets"])
        self.assertEqual(latest["images"], value["images"])
        self.assertEqual(latest["channelPointer"]["manifestSha256"], value["manifest"]["sha256"])
        self.assertEqual(latest["channelPointer"]["signatureUrl"], f"{base}/channels/stable.json.sig")
        self.assertEqual((self.out / "index.json").read_bytes(), (self.out / "api/v1/index.json").read_bytes())

    def test_workflow_cli_generates_a_complete_static_site(self):
        with server({}) as (base, requests):
            self.complete_release(base)
            common = ["--repository", REPO, "--base-url", base]
            for args in [
                ["release", "--site", str(self.new), "--group-version", "v0.10.0", "--artifacts", str(self.root / "dist"),
                 "--sbom", str(self.root / "sbom"), "--repo", str(self.root / "repo"), *common],
                ["build", "--new", str(self.new), "--out", str(self.out), *common],
            ]:
                result = subprocess.run([sys.executable, "-B", str(ROOT / "tools/pages-api.py"), *args], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(requests, ["/index.json"])
        self.assertTrue(json.loads((self.out / "api/v1/releases/v0.10.0.json").read_bytes())["inventoryComplete"])

    def test_legacy_history_and_other_channel_are_preserved_and_backfilled(self):
        old = self.root / "old"
        fixture(old, "v0.9.0", 9, "beta")
        api.write(old / "index.json", {"groups": ["v0.9.0"], "channels": ["beta"]})
        original = files_under(old)
        with server(original) as (base, requests):
            self.complete_release(base)
            index = api.build(self.new, self.out, REPO, base)
        self.assertEqual(index["groups"], ["v0.10.0", "v0.9.0"], "numeric publication sequence, not lexicographic version order")
        self.assertEqual(index["channels"], ["beta", "stable"])
        for path, raw in original.items():
            if path != "index.json":
                self.assertEqual((self.out / path).read_bytes(), raw)
        legacy = json.loads((self.out / "api/v1/releases/v0.9.0.json").read_bytes())
        self.assertFalse(legacy["inventoryComplete"], "old releases do not invent SBOM hashes")
        self.assertEqual(len(legacy["assets"]), 1)
        self.assertIn("/releases/v0.9.0/catalog.json", requests)
        self.assertEqual(json.loads((self.out / "api/v1/channels/beta.json").read_bytes())["groupVersion"], "v0.9.0")

    def test_http_failure_never_erases_previous_history(self):
        fixture(self.new)
        with server({"index.json": (503, b"unavailable")}) as (base, _):
            with self.assertRaises(HTTPError):
                api.build(self.new, self.out, REPO, base)
        self.assertFalse((self.out / "index.json").exists())

    def test_published_manifest_cannot_be_overwritten(self):
        old = self.root / "old"
        fixture(old)
        api.write(old / "index.json", {"groups": ["v0.10.0"], "channels": ["stable"]})
        fixture(self.new, sequence=11)
        with server(files_under(old)) as (base, _):
            with self.assertRaisesRegex(ValueError, "immutable"):
                api.build(self.new, self.out, REPO, base)

    def test_channel_must_match_manifest_digest_and_sequence(self):
        fixture(self.new)
        pointer = self.new / "channels/stable.json"
        original = json.loads(pointer.read_bytes())
        for field, bad in (("manifestSha256", "wrong"), ("sequence", 99)):
            changed = {**original, field: bad}
            api.write(pointer, changed)
            with server({}) as (base, _):
                with self.assertRaisesRegex(ValueError, "manifest|sequence"):
                    api.build(self.new, self.root / field, REPO, base)

    def test_release_inventory_refuses_modified_package_bytes(self):
        self.complete_release()
        next((self.root / "dist").iterdir()).write_bytes(b"corrupt")
        with self.assertRaisesRegex(ValueError, "signed manifest"):
            api.release(self.new, "v0.10.0", REPO, "https://example.test", self.root / "dist", self.root / "sbom", self.root / "repo")

    def test_catalog_must_match_all_signed_release_facts(self):
        group, value = self.complete_release()
        raw = (self.new / "releases/v0.10.0/manifest.json").read_bytes()
        for key, changed in (("sourceCommit", "different"), ("sequence", 1000), ("packages", [])):
            invalid = copy.deepcopy(value)
            invalid[key] = changed
            with self.assertRaises(ValueError):
                api.validate_catalog(invalid, group, raw)

    def test_untrusted_history_labels_cannot_escape_the_site(self):
        fixture(self.new)
        with server({"index.json": api.encoded({"groups": ["../../escape"], "channels": []})}) as (base, requests):
            with self.assertRaisesRegex(ValueError, "label"):
                api.build(self.new, self.out, REPO, base)
        self.assertEqual(requests, ["/index.json"])

    def test_sbom_reuse_uses_catalog_urls_and_verifies_the_download(self):
        group, value = self.complete_release()
        baseline = self.new / "releases/v0.10.0/manifest.json"
        matrix = {"meduza-np-wireguard": {"changed": False, "version": "0.9.0", "reusedFromGroup": "v0.9.0"}}
        expected = value["packages"][0]["sbom"]
        body = sbom("meduza-np-wireguard", "0.9.0")
        with patch.object(api, "fetch", return_value=api.encoded(value)), patch.object(api, "urlopen", return_value=io.BytesIO(body)) as download:
            api.reuse_sboms(baseline, matrix, self.root / "reused", REPO, "https://example.test")
        self.assertEqual(download.call_args.args[0], expected["url"])
        self.assertEqual((self.root / "reused" / expected["name"]).read_bytes(), body)
        with patch.object(api, "fetch", return_value=api.encoded(value)), patch.object(api, "urlopen", return_value=io.BytesIO(body + b" ")):
            with self.assertRaisesRegex(ValueError, "size|SHA-256"):
                api.reuse_sboms(baseline, matrix, self.root / "corrupt", REPO, "https://example.test")
        self.assertFalse((self.root / "corrupt" / expected["name"]).exists())
        self.assertFalse((self.root / "corrupt" / (expected["name"] + ".part")).exists())

    def test_legacy_sbom_reuse_uses_direct_release_download_and_checks_identity(self):
        fixture(self.new)
        baseline = self.new / "releases/v0.10.0/manifest.json"
        matrix = {"meduza-np-wireguard": {"changed": False, "version": "0.9.0", "reusedFromGroup": "v0.9.0"}}
        with patch.object(api, "fetch", return_value=None), patch.object(api, "urlopen", return_value=io.BytesIO(sbom("meduza-np-wireguard", "0.9.0"))) as download:
            api.reuse_sboms(baseline, matrix, self.root / "legacy", REPO, "https://example.test")
        self.assertEqual(download.call_args.args[0], f"https://github.com/{REPO}/releases/download/v0.9.0/meduza-np-wireguard_0.9.0.cdx.json")
        with patch.object(api, "fetch", return_value=None), patch.object(api, "urlopen", return_value=io.BytesIO(sbom("another-package", "0.9.0"))):
            with self.assertRaisesRegex(ValueError, "package/version"):
                api.reuse_sboms(baseline, matrix, self.root / "wrong", REPO, "https://example.test")


if __name__ == "__main__":
    unittest.main()
