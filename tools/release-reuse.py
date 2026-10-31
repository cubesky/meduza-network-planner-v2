"""Copy unchanged Release assets verbatim and point the new group at the verified copies."""

import argparse
import copy
import hashlib
import json
from pathlib import Path
from urllib.parse import unquote, urlsplit
from urllib.request import urlopen


def asset_name(url):
    parsed = urlsplit(url)
    name = unquote(parsed.path.rsplit("/", 1)[-1])
    if parsed.scheme not in ("https", "http") or not name or name in (".", "..") or "/" in name or "\\" in name:
        raise ValueError(f"invalid Release asset URL: {url}")
    return name


def verify(path, artifact):
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            size += len(chunk)
            digest.update(chunk)
    if size != artifact["size"] or digest.hexdigest() != artifact["sha256"]:
        raise ValueError(f"{path.name}: size or SHA-256 differs from the baseline")


def stage(baseline, matrix, directory, url_base):
    result = copy.deepcopy(baseline)
    packages = {package["name"]: package for package in result["packages"]}
    directory.mkdir(parents=True, exist_ok=True)
    for name, entry in matrix.items():
        if entry["changed"]:
            continue
        package = packages[name]
        if (package["version"], package["fingerprint"]) != (entry["version"], entry["fingerprint"]):
            raise ValueError(f"{name}: reuse matrix does not match the baseline")
        for artifact in package["artifacts"]:
            if artifact["format"] == "oci":
                continue
            filename = asset_name(artifact["url"])
            destination = directory / filename
            if not destination.exists():
                temporary = destination.with_name(filename + ".part")
                try:
                    with urlopen(artifact["url"], timeout=120) as response, temporary.open("wb") as output:
                        size = 0
                        while chunk := response.read(1024 * 1024):
                            size += len(chunk)
                            if size > artifact["size"]:
                                raise ValueError(f"{filename}: download exceeds the baseline size")
                            output.write(chunk)
                    verify(temporary, artifact)
                    temporary.replace(destination)
                finally:
                    temporary.unlink(missing_ok=True)
            verify(destination, artifact)
            artifact["url"] = f"{url_base.rstrip('/')}/{filename}"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--matrix", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--url-base", required=True)
    parser.add_argument("--baseline-out", required=True, type=Path)
    args = parser.parse_args()
    result = stage(json.loads(args.baseline.read_text()), json.loads(args.matrix.read_text()), args.out, args.url_base)
    args.baseline_out.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
