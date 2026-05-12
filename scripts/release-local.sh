#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
RUN_TESTS=0
REFRESH_SERVICE=0
START_SERVICE=1
INSTALL_CLI=1
SERVICE_ARGS=()

usage() {
  cat <<'USAGE'
Usage: scripts/release-local.sh [options] [-- service install args...]

Build the current worktree in release mode and install a local cx binary. This
does not restart the local cx service unless --service is passed.

Options:
  --bin-dir DIR       Install cx here. Default: ~/.local/bin or $BIN_DIR.
  --test              Run cargo test --all-targets before installing.
  --no-cli            Do not install the cx CLI shim/binary.
  --service           Stop/reinstall/start the local cx service using this build.
  --no-service        Do not stop/reinstall/start the local cx service. Default.
  --no-start          With --service, reinstall launchd plist but do not start.
  -h, --help          Show this help.

Service args:
  Only used with --service. If a launchd plist already exists, its current
  service args are preserved. Args after -- override that and are passed to
  `cx service install`.

Examples:
  scripts/release-local.sh
  scripts/release-local.sh --test
  scripts/release-local.sh --service -- --allow-chat 1032180412 --target default
USAGE
}

log() {
  printf '==> %s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

add_common_tool_paths() {
  local dir
  for dir in \
    "$HOME/.cargo/bin" \
    /opt/homebrew/opt/rustup/bin \
    /usr/local/opt/rustup/bin \
    /opt/homebrew/bin \
    /usr/local/bin; do
    [[ -d "$dir" ]] || continue
    case ":$PATH:" in
      *":$dir:"*) ;;
      *) PATH="$dir:$PATH" ;;
    esac
  done
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

homebrew_managed_bin_dir() {
  case "$1" in
    /opt/homebrew/bin|/usr/local/bin) return 0 ;;
    *) return 1 ;;
  esac
}

service_plist() {
  printf '%s/Library/LaunchAgents/dev.xiaotian.cx.service.plist\n' "$HOME"
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

load_existing_service_args() {
  local plist="$1"

  SERVICE_ARGS=()
  while IFS= read -r -d '' arg; do
    SERVICE_ARGS+=("$arg")
  done < <(read_launchd_service_args "$plist")
}

path_entry_before() {
  local needle="$1"
  local before="$2"
  local entry

  IFS=':' read -r -a path_entries <<<"$PATH"
  for entry in "${path_entries[@]}"; do
    [[ "$entry" == "$needle" ]] && return 0
    [[ "$entry" == "$before" ]] && return 1
  done
  return 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin-dir)
      [[ $# -ge 2 ]] || die "--bin-dir requires a directory"
      BIN_DIR="$2"
      shift
      ;;
    --test)
      RUN_TESTS=1
      ;;
    --no-cli)
      INSTALL_CLI=0
      ;;
    --service)
      REFRESH_SERVICE=1
      ;;
    --no-service)
      REFRESH_SERVICE=0
      ;;
    --no-start)
      START_SERVICE=0
      ;;
    --)
      shift
      SERVICE_ARGS=("$@")
      break
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

add_common_tool_paths
need_cmd cargo
need_cmd install
if [[ "$REFRESH_SERVICE" -eq 1 ]]; then
  need_cmd launchctl
  need_cmd ruby
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN_DIR_PARENT="$(dirname "$BIN_DIR")"
mkdir -p "$BIN_DIR_PARENT"
BIN_DIR="$(cd "$BIN_DIR_PARENT" && pwd)/$(basename "$BIN_DIR")"
if [[ "$INSTALL_CLI" -eq 1 ]] && homebrew_managed_bin_dir "$BIN_DIR"; then
  die "refusing to install into Homebrew-managed bin dir: $BIN_DIR"
fi

if [[ "$RUN_TESTS" -eq 1 ]]; then
  log "running cargo test --all-targets"
  cargo test --all-targets
fi

log "building release binary from current worktree"
cargo build --release

BUILT_BIN="$ROOT/target/release/cx"
LOCAL_BIN="$BIN_DIR/cx"
SERVICE_BIN="$BUILT_BIN"

if [[ "$INSTALL_CLI" -eq 1 ]]; then
  log "installing cx to $LOCAL_BIN"
  mkdir -p "$BIN_DIR"
  install -m 0755 "$BUILT_BIN" "$LOCAL_BIN"
  SERVICE_BIN="$LOCAL_BIN"
fi

if [[ "$REFRESH_SERVICE" -eq 1 ]]; then
  PLIST="$(service_plist)"
  if [[ "${#SERVICE_ARGS[@]}" -eq 0 && -f "$PLIST" ]]; then
    load_existing_service_args "$PLIST"
  fi

  log "stopping existing launchd service if present"
  launchctl bootout "gui/$(id -u)" "$PLIST" >/dev/null 2>&1 || true
  "$SERVICE_BIN" service stop --force --wait-timeout 10 >/dev/null 2>&1 || true

  if [[ "$START_SERVICE" -eq 1 ]]; then
    log "installing and starting launchd service with $SERVICE_BIN"
    "$SERVICE_BIN" service install "${SERVICE_ARGS[@]}" --start
  else
    log "installing launchd service with $SERVICE_BIN"
    "$SERVICE_BIN" service install "${SERVICE_ARGS[@]}"
  fi
fi

if [[ "$INSTALL_CLI" -eq 1 ]]; then
  FOUND_CX="$(command -v cx || true)"
  if [[ "$FOUND_CX" != "$LOCAL_BIN" ]]; then
    warn "PATH currently resolves cx to: ${FOUND_CX:-<not found>}"
    warn "put $BIN_DIR before Homebrew/system bin dirs to use this local build by default"
  elif command -v brew >/dev/null 2>&1 && ! path_entry_before "$BIN_DIR" "/opt/homebrew/bin"; then
    warn "$BIN_DIR is not before /opt/homebrew/bin in PATH; verify shell startup order if Homebrew cx appears later"
  fi
fi

log "local cx ready"
if [[ "$INSTALL_CLI" -eq 1 ]]; then
  printf 'installed: %s\n' "$LOCAL_BIN"
else
  printf 'built: %s\n' "$BUILT_BIN"
fi
