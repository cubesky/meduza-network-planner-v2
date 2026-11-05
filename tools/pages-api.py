"""Build the static Pages discovery API from published group metadata, without GitHub API calls.

The signed channel pointers/manifests remain the authority for installation. Catalogs are a convenience for
release discovery, downloads, OCI references and SBOMs; they do not replace signature verification.
"""

import argparse
import copy
import hashlib
import json
import re
import shutil
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import unquote, urlsplit
from urllib.request import urlopen


API_VERSION = 1
MAX_METADATA = 16 * 1024 * 1024
CHUNK_BYTES = 500_000
MAX_SITE_BYTES = 1_000_000_000


def label(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,127}", value):
        raise ValueError(f"invalid metadata label: {value!r}")
    return value


def repository(value):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value):
        raise ValueError("repository must be owner/name")
    return {"nameWithOwner": value, "url": f"https://github.com/{value}"}


def base_url(value):
    parsed = urlsplit(value)
    if parsed.scheme not in ("http", "https") or not parsed.netloc or parsed.query or parsed.fragment:
        raise ValueError("base URL must be an HTTP(S) site URL")
    return value.rstrip("/")


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode()


def write(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else encoded(data))


def fetch(base, relative, optional=False):
    # Relative paths are constructed only from validated version/channel labels.
    try:
        with urlopen(f"{base_url(base)}/{relative}", timeout=60) as response:
            result = response.read(MAX_METADATA + 1)
            if len(result) > MAX_METADATA:
                raise ValueError(f"{relative}: metadata exceeds {MAX_METADATA} bytes")
            return result
    except HTTPError as error:
        if optional and error.code == 404:
            return None
        raise


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            size += len(chunk)
            digest.update(chunk)
    return size, digest.hexdigest()


def package_files(group, base):
    """Only content-addressed files named by signed chunk metadata belong to the Pages download tree."""
    prefix = base_url(base) + "/"
    for package in group["packages"]:
        for artifact in package["artifacts"]:
            chunks = artifact.get("chunks", [])
            if not chunks:
                continue
            if artifact["format"] not in ("tar.gz", "tar.zst") or artifact["platform"].startswith("openwrt/"):
                raise ValueError("OpenWrt/OCI artifacts cannot have Pages chunks")
            digest = artifact["sha256"]
            if not re.fullmatch(r"[0-9a-f]{64}", digest):
                raise ValueError("invalid Pages artifact digest")
            root = f"packages/{digest}/"
            filename = asset_name(artifact["url"])
            if not re.fullmatch(r"[A-Za-z0-9._+-]+", filename) or artifact["url"] != prefix + root + filename:
                raise ValueError("Pages package URL does not match its content path")
            remaining = artifact["size"]
            if not remaining or len(chunks) != (remaining + CHUNK_BYTES - 1) // CHUNK_BYTES:
                raise ValueError("invalid Pages chunk count")
            for index, chunk in enumerate(chunks):
                if chunk["url"] != f"{root}{index:06}.part" or chunk["size"] != min(remaining, CHUNK_BYTES) or not re.fullmatch(r"[0-9a-f]{64}", chunk["sha256"]):
                    raise ValueError("invalid Pages chunk path, size or digest")
                remaining -= chunk["size"]
            yield artifact, root + filename


def download_package(base, relative, destination, size, digest):
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(destination.name + ".tmp")
    try:
        for attempt in range(4):
            try:
                with urlopen(f"{base}/{relative}", timeout=60) as response, temporary.open("wb") as output:
                    count = 0
                    checksum = hashlib.sha256()
                    while data := response.read(min(1024 * 1024, size - count + 1)):
                        count += len(data)
                        if count > size:
                            raise ValueError(f"{relative}: exceeds signed package size")
                        checksum.update(data)
                        output.write(data)
                if count != size or checksum.hexdigest() != digest:
                    raise ValueError(f"{relative}: signed package size/SHA-256 mismatch")
                temporary.replace(destination)
                return
            except (OSError, ValueError):
                temporary.unlink(missing_ok=True)
                if attempt == 3:
                    raise
                time.sleep(2 ** attempt)
    finally:
        temporary.unlink(missing_ok=True)


def restore_packages(group, new, out, base):
    for artifact, relative in package_files(group, base):
        destination = out / relative
        if not destination.exists():
            source = new / relative
            if source.is_file() and not source.is_symlink():
                if file_digest(source) != (artifact["size"], artifact["sha256"]):
                    raise ValueError(f"{relative}: package differs from signed manifest")
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
            else:
                download_package(base, relative, destination, artifact["size"], artifact["sha256"])
        if file_digest(destination) != (artifact["size"], artifact["sha256"]):
            raise ValueError(f"{relative}: package differs from signed manifest")
        # Historical parts can be regenerated from the verified full archive, avoiding duplicate downloads.
        with destination.open("rb") as source:
            for chunk in artifact["chunks"]:
                data = source.read(chunk["size"])
                if len(data) != chunk["size"] or sha256(data) != chunk["sha256"]:
                    raise ValueError(f"{relative}: chunk differs from signed manifest")
                path = out / chunk["url"]
                if not path.exists():
                    write(path, data)
                elif file_digest(path) != (chunk["size"], chunk["sha256"]):
                    raise ValueError(f"{path}: immutable chunk differs")


def asset_name(url):
    parsed = urlsplit(url)
    name = unquote(parsed.path.rsplit("/", 1)[-1])
    if parsed.scheme not in ("http", "https") or not name or name in (".", "..") or "/" in name or "\\" in name:
        raise ValueError(f"invalid asset URL: {url}")
    return name


def load_group(raw, version):
    group = json.loads(raw)
    if group.get("groupVersion") != label(version) or group.get("formatVersion") != 1:
        raise ValueError(f"{version}: unexpected group manifest")
    for package in group["packages"]:
        label(package["name"])
        label(package["version"])
    return group


def catalog(group, raw, repo, base):
    version = label(group["groupVersion"])
    info = repository(repo)
    prefix = f"{base_url(base)}/releases/{version}"
    result = copy.deepcopy(group)
    result.update({
        "apiVersion": API_VERSION,
        "repository": info,
        "releaseUrl": f"{info['url']}/releases/tag/{version}",
        "sourceUrl": f"{info['url']}/tree/{label(group['sourceCommit'])}",
        "catalogUrl": f"{prefix}/catalog.json",
        "checksumsUrl": f"{prefix}/checksums.txt",
        "manifest": {"url": f"{prefix}/manifest.json", "signatureUrl": f"{prefix}/manifest.json.sig",
                     "sha256": sha256(raw), "size": len(raw)},
        "inventoryComplete": False,
    })
    assets, images = {}, {}
    for package in result["packages"]:
        package["changed"] = not bool(package.get("reusedFromGroup"))
        for artifact in package["artifacts"]:
            image = artifact.get("oci")
            if image:
                entry = images.setdefault((package["name"], image), {
                    "package": package["name"], "version": package["version"], "reference": image, "platforms": [],
                })
                if artifact["platform"] not in entry["platforms"]:
                    entry["platforms"].append(artifact["platform"])
            if artifact["format"] == "oci":
                continue
            name = asset_name(artifact["url"])
            entry = assets.setdefault(artifact["url"], {
                "name": name, "url": artifact["url"], "package": package["name"], "version": package["version"],
                "kind": "package", "format": artifact["format"], "size": artifact["size"], "sha256": artifact["sha256"], "platforms": [],
            })
            if (entry["size"], entry["sha256"], entry["package"]) != (artifact["size"], artifact["sha256"], package["name"]):
                raise ValueError(f"{name}: inconsistent artifact metadata")
            if artifact["platform"] not in entry["platforms"]:
                entry["platforms"].append(artifact["platform"])
            if artifact.get("chunks"):
                entry["chunks"] = [{**part, "url": f"{base_url(base)}/{part['url']}"} for part in artifact["chunks"]]
    result["assets"] = sorted(assets.values(), key=lambda asset: asset["name"])
    result["images"] = list(images.values())
    return result


def checksums(value):
    return "".join(f"{asset['sha256']}  {asset['name']}\n" for asset in sorted(value["assets"], key=lambda asset: asset["name"])).encode()


def verify_sbom(path, package, version):
    document = json.loads(path.read_bytes())
    component = document.get("metadata", {}).get("component", {})
    if document.get("bomFormat") != "CycloneDX" or (component.get("name"), component.get("version")) != (package, version):
        raise ValueError(f"{path.name}: unexpected SBOM package/version")


def release(site, version, repo, base, artifacts, sboms, root):
    directory = site / "releases" / label(version)
    raw = (directory / "manifest.json").read_bytes()
    group = load_group(raw, version)
    result = catalog(group, raw, repo, base)
    for artifact, relative in package_files(group, base):
        for path, size, digest in [(relative, artifact["size"], artifact["sha256"]), *[(p["url"], p["size"], p["sha256"]) for p in artifact["chunks"]]]:
            if file_digest(site / path) != (size, digest):
                raise ValueError(f"{path}: Pages file differs from signed manifest")
    for asset in result["assets"]:
        if file_digest(artifacts / asset["name"]) != (asset["size"], asset["sha256"]):
            raise ValueError(f"{asset['name']}: artifact differs from the signed manifest")
    for package in result["packages"]:
        name, pkg_version = package["name"], package["version"]
        filename = f"{name}_{pkg_version}.cdx.json"
        path = sboms / filename
        verify_sbom(path, name, pkg_version)
        size, digest = file_digest(path)
        asset = {"name": filename, "url": f"https://github.com/{repo}/releases/download/{version}/{filename}",
                 "package": name, "version": pkg_version, "kind": "sbom", "format": "cyclonedx-json", "size": size, "sha256": digest}
        package["sbom"] = asset
        result["assets"].append(asset)
        metadata = json.loads((root / "packages" / name / "manifest.json").read_bytes())
        package["metadata"] = {key: metadata[key] for key in ("description", "role", "provides", "plannerApi", "systemDependencies") if key in metadata}
    result["assets"].sort(key=lambda asset: asset["name"])
    result["inventoryComplete"] = True
    write(directory / "catalog.json", result)
    write(directory / "checksums.txt", checksums(result))
    return result


def validate_catalog(value, group, raw):
    if value.get("apiVersion") != API_VERSION or value.get("groupVersion") != group["groupVersion"]:
        raise ValueError("catalog does not describe this group")
    if value.get("manifest", {}).get("sha256") != sha256(raw):
        raise ValueError("catalog manifest digest differs from the published manifest")
    if any(value.get(key) != item for key, item in group.items() if key != "packages"):
        raise ValueError("catalog release facts differ from the manifest")
    # Additional discovery fields may not change any signed package facts.
    packages = value.get("packages", [])
    if len(packages) != len(group["packages"]):
        raise ValueError("catalog package set differs from the manifest")
    for package, known in zip(packages, group["packages"]):
        if any(package.get(key) != item for key, item in known.items()):
            raise ValueError("catalog package facts differ from the manifest")


def build(new, out, repo, base):
    base = base_url(base)
    repository(repo)
    if out.exists() and any(out.iterdir()):
        raise ValueError("output directory must be empty")
    out.mkdir(parents=True, exist_ok=True)
    previous = fetch(base, "index.json", optional=True)
    previous = json.loads(previous) if previous is not None else {"groups": [], "channels": []}
    for version in previous["groups"]:
        prefix = f"releases/{label(version)}"
        for filename in ("manifest.json", "manifest.json.sig", "catalog.json"):
            data = fetch(base, f"{prefix}/{filename}", optional=filename == "catalog.json")
            if data is not None:
                write(out / prefix / filename, data)
    for channel in previous["channels"]:
        for suffix in ("json", "json.sig"):
            relative = f"channels/{label(channel)}.{suffix}"
            write(out / relative, fetch(base, relative))
    # Only managed files are merged; published manifests remain byte-for-byte immutable.
    for version_dir in sorted((new / "releases").iterdir()):
        label(version_dir.name)
        for filename in ("manifest.json", "manifest.json.sig", "catalog.json"):
            source = version_dir / filename
            if not source.exists():
                if filename == "catalog.json":
                    continue
                raise ValueError(f"missing {source}")
            dest = out / "releases" / version_dir.name / filename
            if dest.exists() and filename != "manifest.json.sig" and dest.read_bytes() != source.read_bytes():
                raise ValueError(f"immutable release metadata differs: {dest}")
            write(dest, source.read_bytes())
    for pointer in (new / "channels").glob("*.json"):
        label(pointer.stem)
        for path in (pointer, pointer.with_suffix(".json.sig")):
            write(out / "channels" / path.name, path.read_bytes())

    releases = []
    catalogs = {}
    for directory in (out / "releases").iterdir():
        version = label(directory.name)
        raw = (directory / "manifest.json").read_bytes()
        group = load_group(raw, version)
        restore_packages(group, new, out, base)
        path = directory / "catalog.json"
        value = json.loads(path.read_bytes()) if path.exists() else catalog(group, raw, repo, base)
        validate_catalog(value, group, raw)
        write(path, value)
        write(directory / "checksums.txt", checksums(value))
        write(out / f"api/v1/releases/{version}.json", value)
        catalogs[version] = value
        releases.append({key: value[key] for key in ("groupVersion", "sequence", "publishedAt", "sourceCommit", "platforms", "compatibility", "notes", "releaseUrl", "catalogUrl", "manifest", "inventoryComplete") if key in value})
        releases[-1].update({"packageCount": len(group["packages"]),
                             "changedPackages": [p["name"] for p in value["packages"] if p["changed"]],
                             "reusedPackages": [p["name"] for p in value["packages"] if not p["changed"]]})
    releases.sort(key=lambda value: (value["sequence"], value["publishedAt"], value["groupVersion"]), reverse=True)
    channels = {}
    for path in sorted((out / "channels").glob("*.json")):
        raw = path.read_bytes()
        pointer = json.loads(raw)
        channel, version = label(path.stem), label(pointer["groupVersion"])
        relative = f"releases/{version}/manifest.json"
        if pointer.get("formatVersion") != 1 or pointer.get("channel") != channel or pointer.get("manifest") != relative:
            raise ValueError(f"{channel}: invalid channel pointer")
        if version not in catalogs or pointer["manifestSha256"] != catalogs[version]["manifest"]["sha256"]:
            raise ValueError(f"{channel}: channel points at a missing or different manifest")
        if pointer.get("sequence") != catalogs[version]["sequence"]:
            raise ValueError(f"{channel}: pointer sequence differs from the manifest")
        channels[channel] = {**pointer, "url": f"{base}/channels/{channel}.json", "signatureUrl": f"{base}/channels/{channel}.json.sig",
                             "catalogUrl": catalogs[version]["catalogUrl"], "detailsUrl": f"{base}/api/v1/channels/{channel}.json"}
        write(out / f"api/v1/channels/{channel}.json", {**catalogs[version], "channelPointer": channels[channel]})
    index = {"apiVersion": API_VERSION, "repository": repository(repo), "baseUrl": base,
             "generatedAt": datetime.now(timezone.utc).isoformat(),
             "groups": [r["groupVersion"] for r in releases], "channels": sorted(channels),
             "releases": releases, "channelPointers": channels,
             "api": {"index": f"{base}/api/v1/index.json", "releases": f"{base}/api/v1/releases.json", "channels": f"{base}/api/v1/channels.json"}}
    write(out / "api/v1/releases.json", {"apiVersion": API_VERSION, "releases": releases})
    write(out / "api/v1/channels.json", {"apiVersion": API_VERSION, "channels": channels})
    write(out / "api/v1/index.json", index)
    write(out / "index.json", index)
    size = sum(path.stat().st_size for path in out.rglob("*") if path.is_file())
    if size > MAX_SITE_BYTES:
        raise ValueError(f"Pages site is {size} bytes; exceeds the 1 GB published-site limit")
    return index


def reuse_sboms(baseline_path, matrix, out, repo, base):
    repository(repo)
    if all(entry["changed"] for entry in matrix.values()):
        return
    raw = baseline_path.read_bytes()
    baseline = json.loads(raw)
    version = label(baseline["groupVersion"])
    previous = fetch(base, f"releases/{version}/catalog.json", optional=True)
    previous = json.loads(previous) if previous is not None else None
    if previous is not None:
        validate_catalog(previous, baseline, raw)
    known = {p["name"]: p for p in previous["packages"]} if previous else {}
    out.mkdir(parents=True, exist_ok=True)
    for name, entry in matrix.items():
        if entry["changed"]:
            continue
        name, pkg_version = label(name), label(entry["version"])
        origin = label(entry["reusedFromGroup"])
        filename = f"{name}_{pkg_version}.cdx.json"
        asset = known.get(name, {}).get("sbom")
        if asset and (asset["name"], asset["version"]) != (filename, pkg_version):
            raise ValueError(f"{name}: SBOM inventory does not match the reuse matrix")
        url = asset["url"] if asset else f"https://github.com/{repo}/releases/download/{origin}/{filename}"
        if asset_name(url) != filename:
            raise ValueError(f"{name}: unexpected SBOM URL")
        dest = out / filename
        temporary = out / (filename + ".part")
        if not dest.exists():
            try:
                with urlopen(url, timeout=120) as response, temporary.open("wb") as stream:
                    size = 0
                    while chunk := response.read(1024 * 1024):
                        size += len(chunk)
                        if size > (asset["size"] if asset else MAX_METADATA):
                            raise ValueError(f"{filename}: SBOM exceeds expected size")
                        stream.write(chunk)
                verify_sbom(temporary, name, pkg_version)
                if asset and file_digest(temporary) != (asset["size"], asset["sha256"]):
                    raise ValueError(f"{filename}: SBOM size or SHA-256 mismatch")
                temporary.replace(dest)
            finally:
                temporary.unlink(missing_ok=True)
        verify_sbom(dest, name, pkg_version)
        if asset and file_digest(dest) != (asset["size"], asset["sha256"]):
            raise ValueError(f"{filename}: cached SBOM size or SHA-256 mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("release", "build", "reuse-sboms"):
        command = commands.add_parser(name)
        command.add_argument("--repository", required=True)
        command.add_argument("--base-url", required=True)
        if name == "release":
            command.add_argument("--site", required=True, type=Path)
            command.add_argument("--group-version", required=True)
            command.add_argument("--artifacts", required=True, type=Path)
            command.add_argument("--sbom", required=True, type=Path)
            command.add_argument("--repo", type=Path, default=Path("."))
        else:
            command.add_argument("--out", required=True, type=Path)
            if name == "build":
                command.add_argument("--new", required=True, type=Path)
            else:
                command.add_argument("--baseline", required=True, type=Path)
                command.add_argument("--matrix", required=True, type=Path)
    args = parser.parse_args()
    if args.command == "release":
        release(args.site, args.group_version, args.repository, args.base_url, args.artifacts, args.sbom, args.repo)
    elif args.command == "build":
        build(args.new, args.out, args.repository, args.base_url)
    else:
        reuse_sboms(args.baseline, json.loads(args.matrix.read_bytes()), args.out, args.repository, args.base_url)


if __name__ == "__main__":
    main()
