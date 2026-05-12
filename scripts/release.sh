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
RESTART_LOCAL_SERVICE=1
BUMP_KIND="patch"
VERSION_OVERRIDE=""

LOCAL_SERVICE_LAUNCHD=0
LOCAL_SERVICE_SHOULD_START=0
LOCAL_SERVICE_REFRESHED=0
LOCAL_SERVICE_STATE=""
LOCAL_SERVICE_ARGS=()

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
  --skip-service-restart
                       Do not stop/reinstall/start the local cx service during
                       Homebrew verification.
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

bump_version() {
  local version="$1"
  local kind="$2"
  local major minor patch

  [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || die "can only auto-bump x.y.z versions, got: ${version}"
  major="${BASH_REMATCH[1]}"
  minor="${BASH_REMATCH[2]}"
  patch="${BASH_REMATCH[3]}"

  case "$kind" in
    major) major=$((major + 1)); minor=0; patch=0 ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    patch) patch=$((patch + 1)) ;;
    *) die "unknown bump level: ${kind}" ;;
  esac

  printf '%s.%s.%s\n' "$major" "$minor" "$patch"
}

set_package_version() {
  local version="$1"
  ruby - "$version" <<'RB'
version = ARGV.fetch(0)

cargo = File.read("Cargo.toml")
cargo.sub!(/^version\s*=\s*"[^"]+"/, %(version = "#{version}")) or abort("Cargo.toml version not found")
File.write("Cargo.toml", cargo)

lock = File.read("Cargo.lock")
lock.sub!(/(\[\[package\]\]\nname = "cx"\nversion = )"[^"]+"/, %(\\1"#{version}")) or abort("Cargo.lock cx package not found")
File.write("Cargo.lock", lock)
RB
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

local_service_plist() {
  printf '%s/Library/LaunchAgents/dev.xiaotian.cx.service.plist\n' "$HOME"
}

local_cx_available() {
  command -v cx >/dev/null 2>&1
}

local_service_state() {
  cx service status --json 2>/dev/null \
    | ruby -rjson -e 'print(JSON.parse(STDIN.read).fetch("state", ""))' 2>/dev/null \
    || true
}

read_launchd_service_args() {
  local plist="$1"
  ruby - "$plist" <<'RB'
require "rexml/document"

plist = ARGV.fetch(0)
doc = REXML::Document.new(File.read(plist))
key = nil
args = nil

doc.elements.each("plist/dict/*") do |element|
  if element.name == "key"
    key = element.text
  elsif key == "ProgramArguments" && element.name == "array"
    args = element.elements.select { |child| child.name == "string" }.map { |child| child.text.to_s }
    break
  end
end

exit 0 if args.nil? || args.length < 3
args.drop(3).each do |arg|
  STDOUT.write(arg)
  STDOUT.write("\0")
end
RB
}

load_launchd_service_args() {
  local plist="$1"

  LOCAL_SERVICE_ARGS=()
  while IFS= read -r -d '' arg; do
    LOCAL_SERVICE_ARGS+=("$arg")
  done < <(read_launchd_service_args "$plist")
}

prepare_local_service_refresh() {
  local plist

  LOCAL_SERVICE_LAUNCHD=0
  LOCAL_SERVICE_SHOULD_START=0
  LOCAL_SERVICE_REFRESHED=0
  LOCAL_SERVICE_STATE=""
  LOCAL_SERVICE_ARGS=()

  [[ "$RESTART_LOCAL_SERVICE" -eq 1 ]] || return

  plist="$(local_service_plist)"
  if [[ -f "$plist" ]]; then
    LOCAL_SERVICE_LAUNCHD=1
    LOCAL_SERVICE_SHOULD_START=1
    load_launchd_service_args "$plist"
  fi

  if local_cx_available; then
    LOCAL_SERVICE_STATE="$(local_service_state)"
    if [[ "$LOCAL_SERVICE_STATE" == "running" || "$LOCAL_SERVICE_STATE" == "stale" ]]; then
      LOCAL_SERVICE_SHOULD_START=1
    fi
  elif [[ "$LOCAL_SERVICE_LAUNCHD" -eq 1 ]]; then
    log "cx is not on PATH; local launchd service will be refreshed after brew install"
  fi
}

refresh_local_service_after_reinstall() {
  [[ "$RESTART_LOCAL_SERVICE" -eq 1 ]] || return
  [[ "$LOCAL_SERVICE_SHOULD_START" -eq 1 ]] || return

  if [[ "$LOCAL_SERVICE_LAUNCHD" -eq 1 ]]; then
    log "reinstalling and starting local launchd service"
    if [[ "${#LOCAL_SERVICE_ARGS[@]}" -gt 0 ]]; then
      cx service install "${LOCAL_SERVICE_ARGS[@]}" --start
    else
      cx service install --start
    fi
  else
    log "starting local cx service"
    cx service start
  fi
  LOCAL_SERVICE_REFRESHED=1
}

restore_local_service_on_exit() {
  local status=$?

  if [[ "$LOCAL_SERVICE_SHOULD_START" -eq 1 && "$LOCAL_SERVICE_REFRESHED" -eq 0 ]]; then
    if [[ "$status" -eq 0 ]]; then
      refresh_local_service_after_reinstall
      status=$?
    else
      log "release failed; attempting to restore local cx service"
      refresh_local_service_after_reinstall || true
    fi
  fi

  exit "$status"
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
    --skip-service-restart)
      RESTART_LOCAL_SERVICE=0
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
need_cmd git
need_cmd gh
need_cmd ruby
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
VERSION="$CURRENT_VERSION"

if [[ -n "$VERSION_OVERRIDE" ]]; then
  VERSION="$VERSION_OVERRIDE"
elif [[ -n "$CURRENT_TAG_SHA" && "$CURRENT_TAG_SHA" != "$HEAD_SHA" ]]; then
  VERSION="$(bump_version "$CURRENT_VERSION" "$BUMP_KIND")"
fi

TAG="v${VERSION}"
if [[ "$VERSION" != "$CURRENT_VERSION" ]]; then
  TAG_SHA="$(tag_commit "$TAG")"
  [[ -z "$TAG_SHA" ]] || die "tag ${TAG} already exists at ${TAG_SHA}; choose a different version"

  log "bumping Cargo version ${CURRENT_VERSION} -> ${VERSION}"
  set_package_version "$VERSION"
fi

TAG_SHA="$(tag_commit "$TAG")"
if [[ -n "$TAG_SHA" && "$TAG_SHA" != "$HEAD_SHA" ]]; then
  die "tag ${TAG} points to ${TAG_SHA}, not HEAD ${HEAD_SHA}; choose a different version"
fi

if [[ "$RUN_TESTS" -eq 1 ]]; then
  log "running cargo test"
  cargo test
fi

if ! git diff --quiet -- Cargo.toml Cargo.lock; then
  log "committing version bump"
  git diff --check -- Cargo.toml Cargo.lock
  git add Cargo.toml Cargo.lock
  git commit -m "chore: bump cx version to ${VERSION}"
  HEAD_SHA="$(git rev-parse HEAD)"
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
  prepare_local_service_refresh
  brew update
  brew upgrade "$FORMULA_REF" || brew reinstall "$FORMULA_REF"
  cx --help >/dev/null
  brew test "$FORMULA_REF"
  if [[ "$LOCAL_SERVICE_SHOULD_START" -eq 1 ]]; then
    trap restore_local_service_on_exit EXIT
    refresh_local_service_after_reinstall
    trap - EXIT
  fi
fi

log "release ${TAG} complete"
