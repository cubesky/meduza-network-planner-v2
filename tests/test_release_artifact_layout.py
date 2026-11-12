"""Exercise release consumers with download-artifact v7's single/multiple-match extraction semantics."""

import fnmatch
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile
import textwrap
import unittest
import zipfile


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"


def extract_artifacts(root, artifacts, pattern, path, merge=False):
    # External action behavior: a single pattern match is flattened regardless of merge-multiple=false.
    # https://github.com/actions/download-artifact/blob/v7/src/download-artifact.ts
    selected = [(name, files) for name, files in artifacts.items() if fnmatch.fnmatchcase(name, pattern)]
    for name, files in selected:
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w") as z:
            for file, content in files.items():
                z.writestr(file, content)
        dest = root / path if merge or len(selected) == 1 else root / path / name
        with zipfile.ZipFile(archive) as z:
            z.extractall(dest)


@unittest.skipUnless(os.name == "posix" and shutil.which("bash"), "requires POSIX find/cp and bash")
class ArtifactLayout(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text(encoding="utf-8")
        temp = tempfile.TemporaryDirectory(prefix="meduza-artifact-layout-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)

    def shell(self, script, **env):
        result = subprocess.run(["bash", "-eu", "-c", script], cwd=self.root,
                                env={**os.environ, **env}, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout.strip()

    def download(self, artifacts, pattern):
        match = re.search(r"with: \{ pattern: " + re.escape(pattern) + r", ([^}]+)\}", self.workflow)
        self.assertIsNotNone(match, pattern)
        options = dict(part.strip().split(": ", 1) for part in match[1].strip().split(","))
        extract_artifacts(self.root, artifacts, pattern, options["path"], options.get("merge-multiple") == "true")

    def image_records(self, packages):
        result = {}
        for package in packages:
            files = {f"{package}.oci.json": json.dumps({"package": package})}
            platforms = ["linux-amd64", "linux-arm64"]
            if package == "meduza-np-base-container":
                platforms.append("linux-arm-v7")
            for slug in platforms:
                files[f"{package}.image-os/{slug}.os-release"] = "ID=alpine\nVERSION_ID=3.24.1\n"
                files[f"{package}.image-os/{slug}.installed"] = "P:busybox\nV:1.37.0-r1\n"
            result[f"oci-{package}"] = files
        return result

    def check_images(self, packages):
        self.download(self.image_records(packages), "oci-*")
        collect = re.search(r"^\s*(mkdir -p oci && find in .*?;)$", self.workflow, re.M)
        self.assertIsNotNone(collect)
        self.shell(collect[1])
        assignment = next(line.strip() for line in self.workflow.splitlines() if 'os=""; jq -e' in line)
        for package in packages:
            self.assertEqual(json.loads((self.root / "oci" / f"{package}.oci.json").read_text())["package"], package)
            args = shlex.split(self.shell('jq() { return 0; }; p="$PACKAGE"; ' + assignment + '; printf "%s" "$os"', PACKAGE=package))
            self.assertEqual(args[0], "--image-os")
            expected = ["linux-amd64", "linux-arm64"] + (["linux-arm-v7"] if package.endswith("base-container") else [])
            for slug in expected:
                for extension in ["os-release", "installed"]:
                    path = self.root / args[1] / f"{slug}.{extension}"
                    self.assertTrue(path.is_file(), str(path))
                    self.assertGreater(path.stat().st_size, 0)

    def test_only_manager_image_rebuilt(self):
        self.check_images(["meduza-np-manager"])

    def test_both_runtime_images_rebuilt(self):
        self.check_images(["meduza-np-manager", "meduza-np-base-container"])

    def test_single_base_image_rebuilt(self):
        self.check_images(["meduza-np-base-container"])

    def test_one_or_many_changed_archive_bundles_are_collected(self):
        match = re.search(r"- name: Collect changed artifacts\n\s+run: \|\n((?:          .*\n)+)", self.workflow)
        self.assertIsNotNone(match)
        script = textwrap.dedent(match[1])
        for count in [1, 3]:
            with self.subTest(count=count):
                shutil.rmtree(self.root / "in", ignore_errors=True)
                shutil.rmtree(self.root / "dist", ignore_errors=True)
                artifacts = {f"dist-rust-target{i}": {f"package{i}.tar.gz": f"bytes{i}", f"package{i}.tar.gz.json": f"metadata{i}"} for i in range(count)}
                self.download(artifacts, "dist-*")
                self.shell(script)
                for files in artifacts.values():
                    for file, content in files.items():
                        self.assertEqual((self.root / "dist" / file).read_text(), content)

    def test_updater_build_input_lookup_works_with_one_or_many_rust_bundles(self):
        lookup = next(line.strip() for line in self.workflow.splitlines() if "built=$(find in " in line)
        filename = "meduza-np-updater_1.0.0_linux-amd64.tar.gz"
        body = b"verified updater archive"
        digest = hashlib.sha256(body).hexdigest()
        for count in [1, 3]:
            with self.subTest(count=count):
                shutil.rmtree(self.root / "in", ignore_errors=True)
                artifacts = {"dist-rust-amd64": {filename: body, filename + ".json": json.dumps({"artifact": {"sha256": digest}})}}
                for i in range(count - 1):
                    artifacts[f"dist-rust-other{i}"] = {f"other{i}.tar.gz": b"other"}
                self.download(artifacts, "dist-rust-*")
                found = self.shell('f="$FILE"; ' + lookup + '; printf "%s" "$built"', FILE=filename)
                self.assertTrue(found, "must use this run's updater rather than fall back to the baseline")
                path = self.root / found
                self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest)
                self.assertEqual(json.loads(Path(str(path) + ".json").read_text())["artifact"]["sha256"], digest)

    def test_single_match_exposes_the_previous_missing_image_os_path(self):
        package = "meduza-np-manager"
        extract_artifacts(self.root, self.image_records([package]), "oci-*", "in", merge=False)
        self.assertTrue((self.root / "in" / f"{package}.image-os/linux-amd64.os-release").is_file())
        self.assertFalse((self.root / "in" / f"oci-{package}" / f"{package}.image-os/linux-amd64.os-release").exists())


if __name__ == "__main__":
    unittest.main()
