"""Bounded, idempotent GitHub Release publication through the authenticated gh CLI."""

import argparse
import hashlib
import json
from pathlib import Path
import random
import re
import subprocess
import sys
import tempfile
import time


MAX_ATTEMPTS = 6


class PublishError(RuntimeError):
    pass


class GhError(PublishError):
    def __init__(self, message):
        super().__init__(message)
        self.message = message

    def missing(self):
        return self.message.strip().lower() == "release not found" or bool(re.search(r"\bHTTP 404\b", self.message))

    def conflict(self):
        return bool(re.search(r"\bHTTP 409\b", self.message)) or any(value in self.message.lower() for value in (
            "already_exists", "already exists", "already has an asset",
        ))

    def transient(self):
        statuses = re.findall(r"\bHTTP (\d{3})\b", self.message)
        if statuses:
            status = int(statuses[-1])
            return status in (408, 425, 429) or 500 <= status <= 599 or status == 403 and "rate limit" in self.message.lower()
        return self.message.strip().lower() == "eof" or any(value in self.message.lower() for value in (
            "timed out", "timeout", "deadline exceeded", "connection reset", "connection refused", "broken pipe", "unexpected eof", " eof",
            "temporary failure", "no such host", "network is unreachable", "tls handshake",
        ))


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        while data := stream.read(1024 * 1024):
            checksum.update(data)
    return checksum.hexdigest()


class Publisher:
    def __init__(self, repository, tag, *, run=subprocess.run, sleep=time.sleep):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
            raise PublishError("repository must be owner/name")
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,127}", tag):
            raise PublishError("invalid release tag")
        self.repository, self.tag, self.run, self.sleep = repository, tag, run, sleep

    def gh(self, *args):
        try:
            result = self.run(["gh", *args], capture_output=True, text=True, timeout=180)
        except subprocess.TimeoutExpired as error:
            raise GhError("gh request timed out after 180 seconds; remote result is unknown") from error
        if result.returncode:
            raise GhError(result.stderr.strip() or result.stdout.strip() or f"gh exited {result.returncode}")
        return result.stdout

    def pause(self, attempt, error):
        if attempt >= MAX_ATTEMPTS:
            raise PublishError(f"GitHub publication failed after {MAX_ATTEMPTS} attempts: {error}")
        delay = min(2 ** attempt, 32) + random.uniform(0, 1)
        if "rate limit" in str(error).lower() or "HTTP 429" in str(error):
            delay = max(delay, 60)
        print(f"GitHub release attempt {attempt}/{MAX_ATTEMPTS} failed: {str(error)[:1000]}; retrying in {delay:.1f}s", file=sys.stderr, flush=True)
        self.sleep(delay)

    def retry(self, operation, *, allow_missing=False):
        for attempt in range(1, MAX_ATTEMPTS + 1):
            try:
                return operation()
            except GhError as error:
                if not error.transient() and not (allow_missing and error.missing()):
                    raise
                self.pause(attempt, error)

    def view(self):
        result = json.loads(self.gh("release", "view", self.tag, "--repo", self.repository, "--json", "databaseId,isDraft,tagName"))
        if result.get("tagName") != self.tag or not isinstance(result.get("databaseId"), int) or not isinstance(result.get("isDraft"), bool):
            raise PublishError("invalid GitHub release metadata")
        return result

    def ensure(self):
        for attempt in range(1, MAX_ATTEMPTS + 1):
            try:
                return self.view()
            except GhError as error:
                if error.missing():
                    try:
                        self.gh("release", "create", self.tag, "--repo", self.repository, "--verify-tag", "--draft",
                                "--title", self.tag, "--notes", f"Meduza NP package group {self.tag}")
                    except GhError as created:
                        if not created.transient() and not created.conflict():
                            raise
                        error = created
                        # Creation may have succeeded server-side. Reconcile even after the final failed response.
                        try:
                            return self.retry(self.view)
                        except GhError as probe:
                            if not probe.missing() and not probe.transient():
                                raise
                    else:
                        return self.retry(self.view, allow_missing=True)
                elif not error.transient():
                    raise
                self.pause(attempt, error)

    def assets(self, release):
        endpoint = f"repos/{self.repository}/releases/{release['databaseId']}/assets?per_page=100"
        pages = json.loads(self.retry(lambda: self.gh("api", "--paginate", "--slurp", endpoint)))
        if not isinstance(pages, list) or any(not isinstance(page, list) for page in pages):
            raise PublishError("invalid paginated GitHub asset response")
        assets = {}
        for page in pages:
            for asset in page:
                name = asset["name"]
                if name in assets:
                    raise PublishError(f"duplicate immutable asset name: {name}")
                assets[name] = asset
        return assets

    def verify_existing(self, path, asset):
        if asset.get("state") != "uploaded" or asset.get("size") != path.stat().st_size:
            raise PublishError(f"{path.name}: existing asset is incomplete or has a different size; refusing to overwrite it")
        expected = "sha256:" + digest(path)
        if asset.get("digest", "") and str(asset["digest"]).startswith("sha256:"):
            if asset["digest"].lower() != expected:
                raise PublishError(f"{path.name}: existing asset SHA-256 differs; refusing to overwrite it")
        else:
            # Older assets have no API digest. Authenticate draft downloads through gh, then verify the bytes.
            with tempfile.TemporaryDirectory(prefix="meduza-release-verify-") as directory:
                self.retry(lambda: self.gh("release", "download", self.tag, "--repo", self.repository,
                                          "--pattern", path.name, "--dir", directory, "--clobber"))
                if digest(Path(directory) / path.name) != expected.removeprefix("sha256:"):
                    raise PublishError(f"{path.name}: existing asset SHA-256 differs; refusing to overwrite it")

    def upload(self, paths):
        release = self.ensure()
        assets = self.assets(release)
        for path in paths:
            if path.name in assets:
                self.verify_existing(path, assets[path.name])
                print(f"verified existing asset: {path.name}", flush=True)
                continue
            for attempt in range(1, MAX_ATTEMPTS + 1):
                try:
                    self.gh("release", "upload", self.tag, str(path), "--repo", self.repository)
                    print(f"uploaded asset: {path.name}", flush=True)
                    break
                except GhError as error:
                    if not error.transient() and not error.conflict():
                        raise
                    assets = self.assets(release)
                    if path.name in assets:
                        self.verify_existing(path, assets[path.name])
                        print(f"reconciled uploaded asset: {path.name}", flush=True)
                        break
                    self.pause(attempt, error)

    def finalize(self):
        for attempt in range(1, MAX_ATTEMPTS + 1):
            release = self.retry(self.view)
            if not release["isDraft"]:
                return
            try:
                self.gh("release", "edit", self.tag, "--repo", self.repository, "--draft=false")
                return
            except GhError as error:
                if not error.transient():
                    raise
                if not self.retry(self.view)["isDraft"]:
                    return
                self.pause(attempt, error)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("upload", "finalize"))
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--directory", action="append", type=Path, default=[])
    args = parser.parse_args()
    publisher = Publisher(args.repository, args.tag)
    if args.operation == "finalize":
        publisher.finalize()
        return
    paths = []
    for directory in args.directory:
        if not directory.is_dir():
            raise PublishError(f"missing asset directory: {directory}")
        paths.extend(path for path in directory.iterdir() if path.is_file() and path.name.endswith((".tar.gz", ".ipk", ".apk", ".cdx.json")))
    if not paths or len({path.name for path in paths}) != len(paths):
        raise PublishError("no assets selected or duplicate local asset names")
    if any(not re.fullmatch(r"[A-Za-z0-9._+-]+", path.name) for path in paths):
        raise PublishError("invalid generated asset name")
    publisher.upload(sorted(paths))


if __name__ == "__main__":
    try:
        main()
    except (PublishError, OSError, ValueError) as error:
        print(f"::error::{error}", file=sys.stderr)
        raise SystemExit(1)
