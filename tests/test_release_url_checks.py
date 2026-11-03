"""Exercise the workflow's real archive/SBOM URL checks against temporary and permanent HTTP failures."""

from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import re
import shutil
import socket
import subprocess
import threading
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"


@contextmanager
def endpoint(statuses):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_HEAD(self):
            requests.append(self.path)
            status = statuses[min(len(requests) - 1, len(statuses) - 1)]
            if status == "reset":
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
                return
            self.send_response(status)
            self.send_header("Content-Length", "1")
            self.end_headers()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/meduza-np-dashboard_0.1.3_linux-amd64.tar.gz", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@unittest.skipUnless(shutil.which("sh") and shutil.which("curl"), "requires POSIX sh and curl")
class ReleaseUrlChecks(unittest.TestCase):
    def setUp(self):
        # Execute the actual loops, including their failure branches, rather than a test copy of the flags.
        self.loops = re.findall(r'^\s*(while IFS= read -r u; do curl .*?; done)$', WORKFLOW.read_text(), re.M)
        self.assertEqual(len(self.loops), 2, "archive and SBOM publication must both have a URL check")

    def check(self, statuses, success, count):
        for loop in self.loops:
            with self.subTest(loop=loop), endpoint(statuses) as (url, requests):
                result = subprocess.run(["sh", "-c", loop], input=url + "\n", text=True,
                                        capture_output=True, timeout=25)
                self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
                self.assertEqual(len(requests), count)
                self.assertTrue(all(path.endswith("_0.1.3_linux-amd64.tar.gz") for path in requests),
                                "reuse keeps the original package version and URL")
                if not success:
                    self.assertIn("404", result.stderr, "the real HTTP error is visible")
                    self.assertIn(url, result.stdout, "the failed URL remains identifiable")
                    self.assertIn("::error::", result.stdout)

    def test_existing_asset_needs_only_one_request(self):
        self.check([200], True, 1)

    def test_just_published_asset_404_is_retried(self):
        self.check([404, 200], True, 2)

    def test_service_failure_is_retried(self):
        self.check([503, 200], True, 2)

    def test_connection_error_is_retried(self):
        self.check(["reset", 200], True, 2)

    def test_permanently_missing_asset_still_blocks_publication(self):
        self.check([404], False, 6)


if __name__ == "__main__":
    unittest.main()
