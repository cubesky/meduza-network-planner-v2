#!/bin/sh
# Builds, smoke-tests and pushes one runtime OCI image of a package group (§15.1 step 6, D30), then writes its
# record for `meduza-np-release group --oci` (OUTDIR/PACKAGE.oci.json) and the image's OS package databases for its
# SBOM (OUTDIR/PACKAGE.image-os/). Used by release.yml; runs anywhere with podman + skopeo.
#
#   tools/release-image.sh PACKAGE VERSION OUTDIR
#
# Env:
#   REL            meduza-np-release binary (required)
#   REGISTRY       repository prefix, lowercase, e.g. ghcr.io/owner (required)
#   GROUP_TAG      group version tag added to the image, e.g. v0.3.0 (required)
#   SOURCE_URL     org.opencontainers.image.source label (links the GHCR package to the repository)
#   TLS_VERIFY     false for a local test registry (default true)
#   meduza-np-base-container only:
#   UPDATER_DIR      verified updater archives meduza-np-updater_<UPDATER_VERSION>_<platform-slug>.tar.gz
#   UPDATER_VERSION  the group's updater version (what the image embeds)
#   UPDATE_BASE_URL  default signed update source baked into the image (the project's Pages site)
#
# Idempotent: when IMAGE:VERSION already exists (an earlier run of the same tag), nothing is rebuilt; the pushed index
# is described as it is and only the group tag is (re)applied. A tag is never moved to different content.
set -eu

P=$1 V=$2 OUT=$3
: "${REL:?}" "${REGISTRY:?}" "${GROUP_TAG:?}"
TLS=${TLS_VERIFY:-true}
REPO=$(cd "$(dirname "$0")/.." && pwd)
IMAGE="$REGISTRY/$P"
LOCAL="localhost/mnp-release-$P:$V"
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
mkdir -p "$OUT"

case $P in
meduza-np-base-container) PLATFORMS="linux/amd64 linux/arm64 linux/arm/v7" ;;
meduza-np-manager) PLATFORMS="linux/amd64 linux/arm64" ;;
*) echo "release-image: $P is not a runtime image package" >&2; exit 2 ;;
esac

step() { echo "== $P $V: $*"; }
inspect() { skopeo inspect --raw --tls-verify="$TLS" "docker://$1"; }

if inspect "$IMAGE:$V" >"$W/raw.json" 2>/dev/null; then
	step "$IMAGE:$V already published, reusing it"
else
	labels="--label org.opencontainers.image.version=$V"
	[ -n "${SOURCE_URL:-}" ] && labels="$labels --label org.opencontainers.image.source=$SOURCE_URL"
	plist=$(echo $PLATFORMS | tr ' ' ',')
	podman manifest rm "$LOCAL" >/dev/null 2>&1 || true
	case $P in
	meduza-np-base-container)
		: "${UPDATER_DIR:?}" "${UPDATER_VERSION:?}"
		ctx="$W/ctx"
		cp -r "$REPO/packages/meduza-np-base-container" "$ctx"
		[ -f "$ctx/trust/release-keys.json" ] || {
			echo "release-image: packages/meduza-np-base-container/trust/release-keys.json is missing (public entry of MEDUZA_NP_RELEASE_KEY)" >&2
			exit 1
		}
		for pl in $PLATFORMS; do
			slug=$(echo "$pl" | tr / -)
			# podman's TARGETARCH+TARGETVARIANT: amd64, arm64, armv7
			arch=$(echo "$pl" | cut -d/ -f2-) && arch=$(echo "$arch" | tr -d /)
			a="$UPDATER_DIR/meduza-np-updater_${UPDATER_VERSION}_$slug.tar.gz"
			[ -f "$a" ] || { echo "release-image: $a missing" >&2; exit 1; }
			mkdir -p "$ctx/bin/$arch"
			tar -xzf "$a" -C "$ctx/bin/$arch" --strip-components=2 usr/bin/meduza-np-updater
		done
		step "build $plist (embedded updater $UPDATER_VERSION)"
		# shellcheck disable=SC2086
		podman build --platform "$plist" --manifest "$LOCAL" $labels \
			--label "io.meduza-np.updater.version=$UPDATER_VERSION" \
			--build-arg "UPDATER_VERSION=$UPDATER_VERSION" \
			${UPDATE_BASE_URL:+--build-arg "UPDATE_BASE_URL=$UPDATE_BASE_URL"} "$ctx"
		for pl in $PLATFORMS; do
			step "smoke $pl: embedded updater runs and accepts the trust store"
			# --platform linux/arm/v7 loses the variant when resolving a local manifest list (Podman 5.4)
			variant=$(echo "$pl" | cut -s -d/ -f3)
			podman run --rm --os linux --arch "$(echo "$pl" | cut -d/ -f2)" ${variant:+--variant "$variant"} --entrypoint /usr/lib/meduza-np/image/meduza-np-updater "$LOCAL" \
				trust-check /usr/share/meduza-np/trust/release-keys.json
		done
		;;
	meduza-np-manager)
		step "build $plist"
		# shellcheck disable=SC2086
		podman build --platform "$plist" --manifest "$LOCAL" $labels -f "$REPO/packages/meduza-np-manager/Containerfile" "$REPO"
		step "smoke (native platform): hardened start, health, passkey bootstrap, enrollment, data kept across re-creation"
		IMAGE="$LOCAL" SKIP_BUILD=1 "$REPO/tools/manager-podman-smoke.sh" "$W/smoke"
		;;
	esac
	step "push $IMAGE:$V"
	podman manifest push --all --tls-verify="$TLS" --digestfile "$W/digest" "$LOCAL" "docker://$IMAGE:$V"
	inspect "$IMAGE@$(cat "$W/digest")" >"$W/raw.json"
fi

"$REL" oci-describe --package "$P" --version "$V" --image "$IMAGE" --raw "$W/raw.json" --out "$OUT/$P.oci.json"
index=$(sed -n 's/.*"index": *"\(sha256:[0-9a-f]*\)".*/\1/p' "$OUT/$P.oci.json")

# OS packages of the published image per platform, for its SBOM (`meduza-np-release sbom --image-os`, D28). Taken
# from the pushed index (also when it was reused), copied out of a created, never started container.
os_dir="$OUT/$P.image-os"
rm -rf "$os_dir" && mkdir -p "$os_dir"
for pl in $PLATFORMS; do
	slug=$(echo "$pl" | tr / -)
	variant=$(echo "$pl" | cut -s -d/ -f3)
	step "OS package database of $pl"
	ctr=$(podman create --tls-verify="$TLS" --os linux --arch "$(echo "$pl" | cut -d/ -f2)" ${variant:+--variant "$variant"} \
		--entrypoint /bin/true "$IMAGE@$index")
	podman cp "$ctr:/lib/apk/db/installed" "$os_dir/$slug.installed"
	# /etc/os-release is a link to /usr/lib/os-release on current Alpine; podman cp copies links as links
	podman cp "$ctr:/usr/lib/os-release" "$os_dir/$slug.os-release" 2>/dev/null || podman cp "$ctr:/etc/os-release" "$os_dir/$slug.os-release"
	podman rm "$ctr" >/dev/null
	[ -f "$os_dir/$slug.installed" ] && [ -f "$os_dir/$slug.os-release" ] && [ ! -L "$os_dir/$slug.os-release" ] || {
		echo "release-image: no apk database / os-release in $IMAGE@$index ($pl)" >&2
		exit 1
	}
done
step "tag $GROUP_TAG -> $index"
skopeo copy --all --preserve-digests --src-tls-verify="$TLS" --dest-tls-verify="$TLS" "docker://$IMAGE@$index" "docker://$IMAGE:$GROUP_TAG"
echo "$IMAGE@$index"
