#!/usr/bin/env bash
set -euo pipefail

echo "==> cx install"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: Rust toolchain not found. Install from https://rustup.rs/" >&2
  exit 1
fi

repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_dir"

bin_dir="${BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$bin_dir"

echo "==> building release binary"
cargo build --release

echo "==> installing cx to $bin_dir"
cp target/release/cx "$bin_dir/"

echo ""
echo "installed: $bin_dir/cx"
echo "try: cx status"
