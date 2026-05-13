#!/usr/bin/env bash
set -euo pipefail

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
RUN_TESTS=0
INSTALL_CLI=1

usage() {
  cat <<'USAGE'
Usage: scripts/release-local.sh [options]

Build the current worktree in release mode and install a local cx binary.

Options:
  --bin-dir DIR       Install cx here. Default: ~/.local/bin or $BIN_DIR.
  --test              Run cargo test --all-targets before installing.
  --no-cli            Do not install the cx CLI shim/binary.
  -h, --help          Show this help.

Examples:
  scripts/release-local.sh
  scripts/release-local.sh --test
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

homebrew_bin_dir() {
  case "$1" in
    /opt/homebrew/bin|/usr/local/bin) return 0 ;;
    *) return 1 ;;
  esac
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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN_DIR_PARENT="$(dirname "$BIN_DIR")"
mkdir -p "$BIN_DIR_PARENT"
BIN_DIR="$(cd "$BIN_DIR_PARENT" && pwd)/$(basename "$BIN_DIR")"
if [[ "$INSTALL_CLI" -eq 1 ]] && homebrew_bin_dir "$BIN_DIR"; then
  die "refusing to install into Homebrew bin dir: $BIN_DIR"
fi

if [[ "$RUN_TESTS" -eq 1 ]]; then
  log "running cargo test --all-targets"
  cargo test --all-targets
fi

log "building release binary from current worktree"
cargo build --release

BUILT_BIN="$ROOT/target/release/cx"
LOCAL_BIN="$BIN_DIR/cx"

if [[ "$INSTALL_CLI" -eq 1 ]]; then
  log "installing cx to $LOCAL_BIN"
  mkdir -p "$BIN_DIR"
  install -m 0755 "$BUILT_BIN" "$LOCAL_BIN"
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
