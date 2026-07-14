#!/usr/bin/env bash
# Release glab-feed for macOS (native arch) to GitLab.
#
# Usage:
#   ./scripts/release-macos.sh
#   ./scripts/release-macos.sh --version 0.2.0
#   ./scripts/release-macos.sh --dry-run
#   ./scripts/release-macos.sh --skip-tag   # upload assets only
#
# Requires: macOS, git, cargo, glab (authenticated), tar, shasum

set -euo pipefail

GLAB_HOST="${GLAB_HOST:-git.example.com}"
GLAB_REPO="${GLAB_REPO:-group/glab-feed}"
BINARY_NAME="glab-feed"

DRY_RUN=false
ALLOW_DIRTY=false
SKIP_TAG=false
VERSION=""

usage() {
    sed -n '2,8p' "$0"
    echo ""
    echo "Options:"
    echo "  --version X.Y.Z   Release version (default: from Cargo.toml)"
    echo "  --dry-run         Print actions without tagging or uploading"
    echo "  --allow-dirty     Allow uncommitted changes"
    echo "  --skip-tag        Skip tag creation/push (upload assets only)"
    echo "  -h, --help        Show this help"
}

log() { printf '==> %s\n' "$*"; }
run() {
    if $DRY_RUN; then
        printf '[dry-run] %s\n' "$*"
    else
        "$@"
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            VERSION="${2:?--version requires a value}"
            shift 2
            ;;
        --dry-run) DRY_RUN=true; shift ;;
        --allow-dirty) ALLOW_DIRTY=true; shift ;;
        --skip-tag) SKIP_TAG=true; shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# ----- Preflight -----

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: this script must run on macOS" >&2
    exit 1
fi

for cmd in cargo git glab tar shasum; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "error: required command not found: $cmd" >&2
        exit 1
    fi
done

if ! glab auth status --hostname "$GLAB_HOST" >/dev/null 2>&1; then
    echo "error: glab is not authenticated for $GLAB_HOST" >&2
    echo "Run: glab auth login --hostname $GLAB_HOST" >&2
    exit 1
fi

if ! $ALLOW_DIRTY && [[ -n "$(git status --porcelain)" ]]; then
    echo "error: working tree is not clean (use --allow-dirty to override)" >&2
    exit 1
fi

if [[ -z "$VERSION" ]]; then
    VERSION="$(cargo metadata --no-deps --format-version=1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')"
fi

TAG="v${VERSION}"
CARGO_VERSION="$(cargo metadata --no-deps --format-version=1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')"
if [[ "$VERSION" != "$CARGO_VERSION" ]]; then
    echo "error: --version $VERSION does not match Cargo.toml ($CARGO_VERSION)" >&2
    exit 1
fi

case "$(uname -m)" in
    arm64) ARCH=aarch64 ;;
    x86_64) ARCH=x86_64 ;;
    *)
        echo "error: unsupported macOS architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

ASSET_BASE="${BINARY_NAME}-${VERSION}-darwin-${ARCH}"
TARBALL="dist/${ASSET_BASE}.tar.gz"
CHECKSUM="${TARBALL}.sha256"
RELEASE_NOTES="$(mktemp)"
trap 'rm -f "$RELEASE_NOTES"' EXIT

cat >"$RELEASE_NOTES" <<EOF
## glab-feed v${VERSION} (macOS ${ARCH})

Extract and place \`${BINARY_NAME}\` on your \`PATH\`:

\`\`\`bash
tar xzf ${ASSET_BASE}.tar.gz
install -m 755 ${BINARY_NAME} /usr/local/bin/
\`\`\`

Requires [\`glab\`](https://gitlab.com/gitlab-org/cli) on \`PATH\` and a GitLab token.
EOF

log "Releasing ${TAG} for darwin-${ARCH}"

# ----- Build -----

log "Building release binary"
cargo build --release

BINARY="target/release/${BINARY_NAME}"
if [[ ! -f "$BINARY" ]]; then
    echo "error: binary not found at $BINARY" >&2
    exit 1
fi

if command -v strip >/dev/null 2>&1; then
    log "Stripping binary"
    strip "$BINARY"
fi

# ----- Package -----

log "Packaging ${TARBALL}"
mkdir -p dist
PKG_DIR="$(mktemp -d)"
trap 'rm -f "$RELEASE_NOTES"; rm -rf "$PKG_DIR"' EXIT
cp "$BINARY" "$PKG_DIR/${BINARY_NAME}"
tar czf "$TARBALL" -C "$PKG_DIR" "$BINARY_NAME"
shasum -a 256 "$TARBALL" >"$CHECKSUM"

# ----- Tag -----

if ! $SKIP_TAG; then
    if git rev-parse "$TAG" >/dev/null 2>&1; then
        echo "error: tag $TAG already exists locally (use --skip-tag to upload only)" >&2
        exit 1
    fi
    log "Creating annotated tag $TAG"
    run git tag -a "$TAG" -m "Release ${TAG}"
    log "Pushing tag $TAG"
    run git push origin "$TAG"
else
    log "Skipping tag creation (--skip-tag)"
fi

# ----- GitLab release -----

ASSET_SPEC="${TARBALL}#${ASSET_BASE}.tar.gz#package"
RELEASE_EXISTS=false
if glab release view "$TAG" -R "$GLAB_REPO" >/dev/null 2>&1; then
    RELEASE_EXISTS=true
fi

if $RELEASE_EXISTS; then
    log "Release $TAG exists; uploading asset"
    if $DRY_RUN; then
        printf '[dry-run] glab release upload %s -R %s %s\n' "$TAG" "$GLAB_REPO" "$ASSET_SPEC"
    else
        glab release upload "$TAG" -R "$GLAB_REPO" "$ASSET_SPEC"
    fi
else
    log "Creating GitLab release $TAG"
    if $DRY_RUN; then
        printf '[dry-run] glab release create %s -R %s --name ... --notes-file ... %s\n' \
            "$TAG" "$GLAB_REPO" "$ASSET_SPEC"
    else
        glab release create "$TAG" -R "$GLAB_REPO" \
            --name "${BINARY_NAME} v${VERSION}" \
            --notes-file "$RELEASE_NOTES" \
            "$ASSET_SPEC"
    fi
fi

# ----- Done -----

echo ""
echo "Release artifacts:"
echo "  $TARBALL"
echo "  $CHECKSUM"
if ! $DRY_RUN; then
    echo ""
    echo "Release page:"
    glab release view "$TAG" -R "$GLAB_REPO" --web 2>/dev/null || \
        echo "  https://${GLAB_HOST}/${GLAB_REPO}/-/releases/${TAG}"
fi
