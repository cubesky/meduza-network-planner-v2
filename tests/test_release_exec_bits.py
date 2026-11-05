"""Run the real exec-bit helper in throwaway Git repositories; never change the working project's index."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "tools/fix-exec-bits.sh"


@unittest.skipUnless(os.name == "posix" and shutil.which("sh") and shutil.which("git"), "requires POSIX sh and Git")
class ExecutableBits(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="meduza-exec-bits-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "project with spaces"
        self.root.mkdir()
        self.git("init", "--quiet")
        self.git("config", "core.fileMode", "true")
        self.git("config", "core.autocrlf", "false")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True, text=True).stdout

    def file(self, name, content, executable=False):
        path = self.root / name
        path.write_text(content, encoding="utf-8")
        self.git("add", "--", name)
        self.git("update-index", "--chmod=" + ("+x" if executable else "-x"), "--", name)

    def helper(self, *args):
        return subprocess.run(["sh", str(SCRIPT), *args], cwd=self.root, capture_output=True, text=True, timeout=15)

    def test_rust_inner_attributes_are_ordinary_source_files(self):
        for name, header in [("test.rs", "#![cfg(unix)]"), ("spaced.rs", "#! [allow(dead_code)]")]:
            self.file(name, header + "\nfn main() {}\n")
        result = self.helper("--check")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "")

    def test_check_still_reports_missing_shell_and_python_permissions(self):
        self.file("test.rs", "#![cfg(unix)]\n")
        self.file("shell script.sh", "#!/bin/sh\nexit 0\n")
        self.file("python.py", "#! /usr/bin/env python3\n")
        self.file("tab.py", "#!\t/usr/bin/env python3\n")
        self.file("already.sh", "#!/bin/sh\n", executable=True)
        result = self.helper("--check")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(set(result.stdout.splitlines()), {
            "not executable: shell script.sh", "not executable: python.py", "not executable: tab.py",
        })

    def test_fix_changes_only_real_script_modes(self):
        self.file("test.rs", "#![cfg(unix)]\n")
        self.file("run.sh", "#!/bin/sh\n")
        result = self.helper()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout, "+x run.sh\n")
        modes = {line.split("\t", 1)[1]: line.split()[0] for line in self.git("ls-files", "--stage").splitlines()}
        self.assertEqual(modes, {"test.rs": "100644", "run.sh": "100755"})
        self.assertEqual(self.helper("--check").returncode, 0)


if __name__ == "__main__":
    unittest.main()
