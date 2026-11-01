# Pages release API

The release workflow publishes static JSON so release browsers, deployment scripts and tooling can discover
versions, download packages and find images without calling the GitHub REST API. Package bytes remain GitHub
Release downloads; OCI pulls use GHCR. There is no GitHub token requirement for these public discovery endpoints.

Set `PAGES_URL` to this repository's Pages URL, for example
`https://cubesky.github.io/meduza-network-planner-v2`.

| Path | Contents |
| --- | --- |
| `/index.json` | Version/channel names plus release summaries and channel pointers; keeps the legacy `groups: string[]` and `channels: string[]` fields |
| `/api/v1/index.json` | Same complete index, with `apiVersion: 1` and links to other endpoints |
| `/api/v1/releases.json` | Release summaries ordered by publication sequence, newest first |
| `/api/v1/channels.json` | Channel map: current group, sequence, publication/expiry times, manifest digest and signed-pointer links |
| `/api/v1/channels/stable.json` | Full current stable release inventory, including its channel pointer; one request for versions, packages and download/image details |
| `/api/v1/releases/<group>.json` | Full inventory for a particular group |
| `/releases/<group>/catalog.json` | Canonical per-release inventory, also available through the versioned API above |
| `/releases/<group>/checksums.txt` | SHA-256 and filename for package files and known SBOMs |
| `/channels/<channel>.json[.sig]` | Original signed updater channel pointer; unchanged |
| `/releases/<group>/manifest.json[.sig]` | Original signed updater group manifest; unchanged |

Release inventories include:

- Source commit, release/source URLs, notes when supplied by the group, publication/expiry times, supported
  platforms and protocol compatibility ranges.
- Each package's actual version, fingerprint, dependencies/conflicts, changed/reused status and origin group.
  New inventories also carry package role, provided capabilities, API range and system dependencies.
- Direct package download URLs, filenames, sizes, SHA-256 and formats/platforms. Reused packages retain their
  original package version, while their URLs describe the copied files in the new group.
- OCI references pinned by digest and their platforms, for runtime images and package distribution images.
- SBOM download URLs, size and SHA-256, and upstream download-provider metadata (including Mihomo's pinned
  versions/assets). Discovering an already pinned core does not require querying its upstream GitHub release.

For example, get a group's package versions and download URLs:

```sh
curl -fsSL "$PAGES_URL/api/v1/channels/stable.json" > release.json
jq -r '.packages[] | [.name, .version, .changed] | @tsv' release.json
jq -r '.assets[] | select(.kind == "package") | [.package, (.platforms | join(",")), .url, .sha256] | @tsv' release.json
jq -r '.images[] | [.package, .reference, (.platforms | join(","))] | @tsv' release.json
jq -r '.packages[] | select(.name == "meduza-np-base-container") | .sbom.url' release.json
```

Catalogs are discovery data. Installers still verify the original signed pointer and manifest, then verify package
bytes against that manifest. The catalog's `manifest` object supplies the exact URL, signature URL, raw-byte size
and SHA-256. The updater keeps using its existing signed endpoints and does not need to load the larger inventory.

Old releases are retained and receive inventories reconstructed from their existing manifests without rewriting
the signed bytes. These have `inventoryComplete: false`: package/image data is available, while unavailable SBOM
hashes or historical package details are omitted. New releases have complete inventories generated from the actual
verified artifacts and SBOMs. A failed history request, malformed metadata, mismatched channel digest or attempted
manifest overwrite stops deployment instead of replacing the site with incomplete history.

The workflow uses `tools/pages-api.py release` to prepare the new release inventory and `build` to merge the live
Pages history and generate the API. Its `reuse-sboms` command takes URLs/hashes from the baseline Pages catalog;
for legacy releases it uses the known direct Release download URL and checks the SBOM's package/version. No GitHub
API is needed for this reuse. Release creation/upload, the Pages settings preflight and discovering a new unpinned
upstream version still use GitHub's authenticated maintenance APIs where necessary.

These endpoints become available after running the updated release/Pages workflow; committing the source alone
does not change the deployed site.
