#!/usr/bin/env bash
# Release glab-feed for macOS (native arch) to GitHub.
#
# Usage:
#   ./scripts/release-macos.sh
#   ./scripts/release-macos.sh --version 0.2.0
#   ./scripts/release-macos.sh --dry-run
#   ./scripts/release-macos.sh --skip-tag   # upload assets only
#   ./scripts/release-macos.sh --skip-screenshot
#   ./scripts/release-macos.sh --skip-changelog
#
# Requires: macOS, git, cargo, gh (authenticated), tar, shasum, vhs, git-cliff
# Optional:
#   GH_REPO     owner/name passed to gh -R (default: repo from the git remote)
#   GIT_REMOTE  remote that receives the tag (default: origin)

set -euo pipefail

GH_REPO="${GH_REPO:-}"
GIT_REMOTE="${GIT_REMOTE:-origin}"
BINARY_NAME="glab-feed"

DRY_RUN=false
ALLOW_DIRTY=false
SKIP_TAG=false
SKIP_SCREENSHOT=false
SKIP_CHANGELOG=false
VERSION=""

usage() {
    sed -n '2,10p' "$0"
    echo ""
    echo "Options:"
    echo "  --version X.Y.Z   Release version (default: from Cargo.toml)"
    echo "  --dry-run         Print actions without tagging or uploading"
    echo "  --allow-dirty     Allow uncommitted changes"
    echo "  --skip-tag        Skip tag creation/push (upload assets only)"
    echo "  --skip-screenshot Skip README screenshot capture"
    echo "  --skip-changelog  Skip changelog generation"
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
        --skip-screenshot) SKIP_SCREENSHOT=true; shift ;;
        --skip-changelog) SKIP_CHANGELOG=true; shift ;;
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

for cmd in cargo git gh tar shasum; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "error: required command not found: $cmd" >&2
        exit 1
    fi
done

if ! $SKIP_SCREENSHOT && ! command -v vhs >/dev/null 2>&1; then
    echo "error: required command not found: vhs (or use --skip-screenshot)" >&2
    exit 1
fi

if ! $SKIP_CHANGELOG && ! command -v git-cliff >/dev/null 2>&1; then
    echo "error: required command not found: git-cliff (or use --skip-changelog)" >&2
    exit 1
fi

if ! gh auth status >/dev/null 2>&1; then
    echo "error: gh is not authenticated" >&2
    echo "Run: gh auth login" >&2
    exit 1
fi

# bash 3.2 errors on "${empty[@]}" under set -u, so append -R only when set.
gh_in_repo() {
    if [[ -n "$GH_REPO" ]]; then
        gh -R "$GH_REPO" "$@"
    else
        gh "$@"
    fi
}

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
CAPTURE_DIR=""
CHANGELOG_DIR=""
PKG_DIR=""
cleanup() {
    rm -f "$RELEASE_NOTES"
    [[ -z "$CAPTURE_DIR" ]] || rm -rf "$CAPTURE_DIR"
    [[ -z "$CHANGELOG_DIR" ]] || rm -rf "$CHANGELOG_DIR"
    [[ -z "$PKG_DIR" ]] || rm -rf "$PKG_DIR"
}
trap cleanup EXIT

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

# ----- Screenshot -----

if ! $SKIP_SCREENSHOT; then
    log "Capturing README screenshot"
    CAPTURE_DIR="$(mktemp -d)"
    cat >"$CAPTURE_DIR/screenshot.tape" <<EOF
Output "$CAPTURE_DIR/demo.gif"
Set Shell "bash"
Set FontSize 13
Set Width 1400
Set Height 780
Set Theme "TokyoNight"
Set Padding 0
Set Margin 0
Set BorderRadius 0
Set WindowBarSize 0
Set FontFamily "FiraCode Nerd Font Mono"
Set TypingSpeed 1ms
Hide
Type "unset NO_COLOR; export TERM=xterm-256color COLORTERM=truecolor; clear; ./$BINARY --demo"
Enter
Show
Sleep 2s
Screenshot "$CAPTURE_DIR/screenshot.png"
Type "q"
Sleep 300ms
EOF
    env -u NO_COLOR TERM=xterm-256color vhs "$CAPTURE_DIR/screenshot.tape"

    if ! cmp -s "$CAPTURE_DIR/screenshot.png" screenshot.png; then
        if $DRY_RUN; then
            log "README screenshot would be updated"
            printf '[dry-run] git add screenshot.png\n'
            printf '[dry-run] git commit -m "docs(release): update screenshot"\n'
            printf '[dry-run] git push %s <current-branch>\n' "$GIT_REMOTE"
        else
            cp "$CAPTURE_DIR/screenshot.png" screenshot.png
            CURRENT_BRANCH="$(git symbolic-ref --quiet --short HEAD)" || {
                echo "error: cannot commit screenshot from a detached HEAD" >&2
                exit 1
            }
            log "Committing updated README screenshot"
            git add screenshot.png
            git commit -m "docs(release): update screenshot"
            log "Pushing screenshot commit to $GIT_REMOTE/$CURRENT_BRANCH"
            git push "$GIT_REMOTE" "$CURRENT_BRANCH"
        fi
    fi
else
    log "Skipping README screenshot (--skip-screenshot)"
fi

# ----- Changelog -----

if ! $SKIP_CHANGELOG; then
    if [[ ! -f CHANGELOG.md ]]; then
        echo "error: CHANGELOG.md not found" >&2
        exit 1
    fi
    if grep -qx "## ${VERSION}" CHANGELOG.md; then
        log "Changelog already contains ${VERSION}"
    else
        log "Updating changelog"
        CHANGELOG_DIR="$(mktemp -d)"
        cp CHANGELOG.md "$CHANGELOG_DIR/CHANGELOG.md"
        git-cliff --unreleased --tag "$VERSION" --prepend "$CHANGELOG_DIR/CHANGELOG.md"
        if ! cmp -s "$CHANGELOG_DIR/CHANGELOG.md" CHANGELOG.md; then
            if $DRY_RUN; then
                log "Changelog would be updated"
                printf '[dry-run] git add CHANGELOG.md\n'
                printf '[dry-run] git commit -m "docs(release): update changelog"\n'
                printf '[dry-run] git push %s <current-branch>\n' "$GIT_REMOTE"
            else
                cp "$CHANGELOG_DIR/CHANGELOG.md" CHANGELOG.md
                CURRENT_BRANCH="$(git symbolic-ref --quiet --short HEAD)" || {
                    echo "error: cannot commit changelog from a detached HEAD" >&2
                    exit 1
                }
                log "Committing updated changelog"
                git add CHANGELOG.md
                git commit -m "docs(release): update changelog"
                log "Pushing changelog commit to $GIT_REMOTE/$CURRENT_BRANCH"
                git push "$GIT_REMOTE" "$CURRENT_BRANCH"
            fi
        fi
    fi
else
    log "Skipping changelog (--skip-changelog)"
fi

CHANGELOG_SECTION=""
if [[ -f CHANGELOG.md ]]; then
    CHANGELOG_SECTION="$(awk -v ver="## ${VERSION}" '
        $0 == ver { found=1; print; next }
        found && /^## / { exit }
        found { print }
    ' CHANGELOG.md)"
fi

{
    if [[ -n "$CHANGELOG_SECTION" ]]; then
        printf '%s\n' "$CHANGELOG_SECTION"
        printf '\n'
    else
        printf '## glab-feed v%s (macOS %s)\n\n' "$VERSION" "$ARCH"
    fi
    cat <<EOF
## Install (macOS ${ARCH})

Extract and place \`${BINARY_NAME}\` on your \`PATH\`:

\`\`\`bash
tar xzf ${ASSET_BASE}.tar.gz
install -m 755 ${BINARY_NAME} /usr/local/bin/
\`\`\`

Requires [\`glab\`](https://gitlab.com/gitlab-org/cli) on \`PATH\` and a GitLab token.
EOF
} >"$RELEASE_NOTES"

# ----- Package -----

log "Packaging ${TARBALL}"
mkdir -p dist
PKG_DIR="$(mktemp -d)"
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
    run git push "$GIT_REMOTE" "$TAG"
else
    log "Skipping tag creation (--skip-tag)"
fi

# ----- GitHub release -----

RELEASE_EXISTS=false
if gh_in_repo release view "$TAG" >/dev/null 2>&1; then
    RELEASE_EXISTS=true
fi

if $RELEASE_EXISTS; then
    log "Release $TAG exists; uploading assets"
    if $DRY_RUN; then
        printf '[dry-run] gh release upload %s' "$TAG"
        if [[ -n "$GH_REPO" ]]; then
            printf ' -R %s' "$GH_REPO"
        fi
        printf ' %s %s --clobber\n' "$TARBALL" "$CHECKSUM"
    else
        gh_in_repo release upload "$TAG" "$TARBALL" "$CHECKSUM" --clobber
    fi
else
    log "Creating GitHub release $TAG"
    if $DRY_RUN; then
        printf '[dry-run] gh release create %s' "$TAG"
        if [[ -n "$GH_REPO" ]]; then
            printf ' -R %s' "$GH_REPO"
        fi
        printf ' --verify-tag --title ... --notes-file ... %s %s\n' "$TARBALL" "$CHECKSUM"
    else
        gh_in_repo release create "$TAG" \
            --verify-tag \
            --title "${BINARY_NAME} v${VERSION}" \
            --notes-file "$RELEASE_NOTES" \
            "$TARBALL" \
            "$CHECKSUM"
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
    if [[ -n "$GH_REPO" ]]; then
        echo "  https://github.com/${GH_REPO}/releases/tag/${TAG}"
    fi
    gh_in_repo release view "$TAG" --json url --jq .url 2>/dev/null \
        || gh_in_repo release view "$TAG" --web 2>/dev/null \
        || true
fi
