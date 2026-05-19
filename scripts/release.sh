#!/usr/bin/env bash
set -euo pipefail

REPO_SLUG="${REPO_SLUG:-xiaotianxt/cx}"
TAP_NAME="${TAP_NAME:-xiaotianxt/tap}"
FORMULA_REF="${FORMULA_REF:-xiaotianxt/tap/cx}"
WORKFLOW="${WORKFLOW:-release.yml}"

RUN_TESTS=1
UPDATE_TAP=1
BREW_VERIFY=1
WATCH_RELEASE=1
BUMP_KIND="patch"
VERSION_OVERRIDE=""

usage() {
  cat <<'USAGE'
Usage: scripts/release.sh [options]

Create a cx release, wait for GitHub Actions to publish the arm64 artifact,
update the Homebrew tap, and verify brew.

Options:
  --bump LEVEL         Bump level when current version is already tagged on
                       another commit. One of: patch, minor, major.
                       Default: patch.
  --version VERSION    Release this exact x.y.z version, updating Cargo files.
  --skip-tests         Do not run cargo test before tagging.
  --skip-tap           Do not update the Homebrew tap formula.
  --skip-brew-verify   Do not run brew update/upgrade/test after tap update.
  --no-watch           Push the tag but do not wait for the release workflow.
  -h, --help           Show this help.

Environment:
  REPO_SLUG            GitHub repo slug. Default: xiaotianxt/cx
  TAP_NAME             Homebrew tap name. Default: xiaotianxt/tap
  FORMULA_REF          Brew formula ref. Default: xiaotianxt/tap/cx
  WORKFLOW             Release workflow file/name. Default: release.yml
USAGE
}

log() {
  printf '==> %s\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

package_version() {
  sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' Cargo.toml | head -1
}

local_tag_commit() {
  git rev-parse -q --verify "refs/tags/${1}^{}" 2>/dev/null || true
}

remote_tag_commit() {
  local tag="$1"
  local sha

  sha="$(git ls-remote --tags origin "refs/tags/${tag}^{}" | awk '{print $1}')"
  if [[ -z "$sha" ]]; then
    sha="$(git ls-remote --tags origin "refs/tags/${tag}" | awk '{print $1}')"
  fi

  printf '%s' "$sha"
}

tag_commit() {
  local tag="$1"
  local sha

  sha="$(local_tag_commit "$tag")"
  if [[ -z "$sha" ]]; then
    sha="$(remote_tag_commit "$tag")"
  fi

  printf '%s' "$sha"
}

cargo_release_version() {
  local level_or_version="$1"

  cargo release "$level_or_version" \
    --execute \
    --no-confirm \
    --no-publish \
    --no-tag \
    --no-push
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bump)
      [[ $# -ge 2 ]] || die "--bump requires patch, minor, or major"
      BUMP_KIND="$2"
      case "$BUMP_KIND" in
        patch|minor|major) ;;
        *) die "--bump must be one of: patch, minor, major" ;;
      esac
      shift
      ;;
    --version)
      [[ $# -ge 2 ]] || die "--version requires a version"
      VERSION_OVERRIDE="$2"
      [[ "$VERSION_OVERRIDE" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "--version must be x.y.z"
      shift
      ;;
    --skip-tests)
      RUN_TESTS=0
      ;;
    --skip-tap)
      UPDATE_TAP=0
      BREW_VERIFY=0
      ;;
    --skip-brew-verify)
      BREW_VERIFY=0
      ;;
    --no-watch)
      WATCH_RELEASE=0
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
  shift
done

need_cmd cargo
need_cmd cargo-release
need_cmd git
need_cmd gh
if [[ "$UPDATE_TAP" -eq 1 || "$BREW_VERIFY" -eq 1 ]]; then
  need_cmd brew
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash changes first"

TAP_DIR=""
FORMULA_PATH=""
if [[ "$UPDATE_TAP" -eq 1 ]]; then
  TAP_DIR="$(brew --repo "$TAP_NAME")"
  FORMULA_PATH="${TAP_DIR}/Formula/cx.rb"
  [[ -z "$(git -C "$TAP_DIR" status --porcelain)" ]] || die "tap working tree is dirty: ${TAP_DIR}"

  log "updating tap checkout ${TAP_NAME}"
  git -C "$TAP_DIR" pull --ff-only
  [[ -z "$(git -C "$TAP_DIR" status --porcelain)" ]] || die "tap working tree is dirty after pull: ${TAP_DIR}"
fi

log "fetching origin/main and tags"
git fetch origin main --tags

HEAD_SHA="$(git rev-parse HEAD)"
ORIGIN_MAIN_SHA="$(git rev-parse origin/main)"
if [[ "$HEAD_SHA" != "$ORIGIN_MAIN_SHA" ]]; then
  if git merge-base --is-ancestor origin/main HEAD; then
    log "current HEAD is ahead of origin/main"
  else
    die "current HEAD is not origin/main and cannot fast-forward it"
  fi
fi

CURRENT_VERSION="$(package_version)"
[[ -n "$CURRENT_VERSION" ]] || die "Cargo.toml version not found"
CURRENT_TAG="v${CURRENT_VERSION}"
CURRENT_TAG_SHA="$(tag_commit "$CURRENT_TAG")"

if [[ -n "$VERSION_OVERRIDE" && "$VERSION_OVERRIDE" != "$CURRENT_VERSION" ]]; then
  TAG_SHA="$(tag_commit "v${VERSION_OVERRIDE}")"
  [[ -z "$TAG_SHA" ]] || die "tag v${VERSION_OVERRIDE} already exists at ${TAG_SHA}; choose a different version"
  log "bumping Cargo version ${CURRENT_VERSION} -> ${VERSION_OVERRIDE} with cargo-release"
  cargo_release_version "$VERSION_OVERRIDE"
elif [[ -n "$CURRENT_TAG_SHA" && "$CURRENT_TAG_SHA" != "$HEAD_SHA" ]]; then
  log "current version ${CURRENT_VERSION} is already tagged; bumping ${BUMP_KIND} with cargo-release"
  cargo_release_version "$BUMP_KIND"
else
  log "using Cargo version ${CURRENT_VERSION}"
fi

[[ -z "$(git status --porcelain -- Cargo.toml Cargo.lock)" ]] || die "cargo-release left uncommitted Cargo version changes"

VERSION="$(package_version)"
[[ -n "$VERSION" ]] || die "Cargo.toml version not found"
TAG="v${VERSION}"
TAG_SHA="$(tag_commit "$TAG")"
HEAD_SHA="$(git rev-parse HEAD)"
if [[ -n "$TAG_SHA" && "$TAG_SHA" != "$HEAD_SHA" ]]; then
  die "tag ${TAG} points to ${TAG_SHA}, not HEAD ${HEAD_SHA}; choose a different version"
fi

if [[ "$RUN_TESTS" -eq 1 ]]; then
  log "running cargo test"
  cargo test
fi

if [[ "$HEAD_SHA" != "$(git rev-parse origin/main)" ]]; then
  log "pushing current HEAD to origin/main"
  git push origin HEAD:main
fi

ASSET_NAME="cx-${TAG}-darwin-arm64.tar.gz"
ASSET_URL="https://github.com/${REPO_SLUG}/releases/download/${TAG}/${ASSET_NAME}"

log "preparing ${TAG}"

if [[ -n "$(local_tag_commit "$TAG")" ]]; then
  log "local tag ${TAG} already exists"
else
  log "creating tag ${TAG}"
  git tag -a "$TAG" -m "$TAG"
fi

REMOTE_TAG_SHA="$(remote_tag_commit "$TAG")"
if [[ -n "$REMOTE_TAG_SHA" ]]; then
  log "remote tag ${TAG} already exists"
else
  log "pushing tag ${TAG}"
  git push origin "$TAG"
fi

if ! gh release view "$TAG" --repo "$REPO_SLUG" >/dev/null 2>&1; then
  [[ "$WATCH_RELEASE" -eq 1 ]] || die "release ${TAG} does not exist yet; rerun without --no-watch"

  log "waiting for release workflow run"
  RUN_ID=""
  for _ in {1..60}; do
    RUN_ID="$(
      gh run list \
        --repo "$REPO_SLUG" \
        --workflow "$WORKFLOW" \
        --branch "$TAG" \
        --limit 1 \
        --json databaseId \
        --jq '.[0].databaseId // empty'
    )"
    [[ -n "$RUN_ID" ]] && break
    sleep 5
  done
  [[ -n "$RUN_ID" ]] || die "release workflow run for ${TAG} was not found"

  gh run watch "$RUN_ID" --repo "$REPO_SLUG" --exit-status
fi

log "reading release asset digest"
ASSET_SHA="$(
  gh release view "$TAG" \
    --repo "$REPO_SLUG" \
    --json assets \
    --jq ".assets[] | select(.name == \"${ASSET_NAME}\") | .digest // empty"
)"
if [[ "$ASSET_SHA" == sha256:* ]]; then
  ASSET_SHA="${ASSET_SHA#sha256:}"
fi

if [[ -z "$ASSET_SHA" ]]; then
  TMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TMP_DIR"' EXIT
  gh release download "$TAG" --repo "$REPO_SLUG" --pattern "$ASSET_NAME" --dir "$TMP_DIR"
  ASSET_SHA="$(shasum -a 256 "${TMP_DIR}/${ASSET_NAME}" | awk '{print $1}')"
fi
[[ -n "$ASSET_SHA" ]] || die "could not determine sha256 for ${ASSET_NAME}"

log "asset sha256 ${ASSET_SHA}"

if [[ "$UPDATE_TAP" -eq 1 ]]; then
  log "updating tap ${TAP_NAME}"
  cat > "$FORMULA_PATH" <<FORMULA
class Cx < Formula
  desc "Fast local Codex launcher, stdin wrapper, and slot manager"
  homepage "https://github.com/${REPO_SLUG}"
  url "${ASSET_URL}"
  sha256 "${ASSET_SHA}"
  license "MIT"
  version "${VERSION}"

  depends_on arch: :arm64

  head do
    url "https://github.com/${REPO_SLUG}.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", "--bin", "cx", "--root", prefix, "."
    else
      bin.install "cx"
    end
    generate_completions_from_executable(bin/"cx", "completions")
  end

  test do
    system "#{bin}/cx", "--help"
  end
end
FORMULA

  if [[ -z "$(git -C "$TAP_DIR" status --porcelain -- Formula/cx.rb)" ]]; then
    log "tap already points to ${VERSION}"
  else
    git -C "$TAP_DIR" diff --check -- Formula/cx.rb
    git -C "$TAP_DIR" add Formula/cx.rb
    git -C "$TAP_DIR" commit -m "cx ${VERSION}"
    git -C "$TAP_DIR" push origin main
  fi
fi

if [[ "$BREW_VERIFY" -eq 1 ]]; then
  log "verifying Homebrew install"
  brew update
  brew upgrade "$FORMULA_REF" || brew reinstall "$FORMULA_REF"
  cx --help >/dev/null
  brew test "$FORMULA_REF"
fi

log "release ${TAG} complete"
