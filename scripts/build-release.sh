#!/usr/bin/env bash
# Build a release binary that does not embed the builder's home directory.
#
# rustc records absolute paths for panic locations, so a plain `cargo build`
# leaves strings like C:\Users\<name>\... inside the exe. Remapping them to `.`
# keeps the artifact free of machine-specific paths.
set -euo pipefail

cd "$(dirname "$0")/.."

root="$(pwd)"
home_dir="$HOME"
if command -v cygpath >/dev/null 2>&1; then
  root="$(cygpath -w "$root")"
  home_dir="$(cygpath -w "$home_dir")"
fi

# CARGO_ENCODED_RUSTFLAGS splits flags on 0x1f, so paths with spaces are safe.
sep=$'\x1f'
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=${root}=.${sep}--remap-path-prefix=${home_dir}=."

cargo build --release "$@"

target="target/release/codex-switch"
[ -f "$target" ] || target="$target.exe"
echo "built: $target"
