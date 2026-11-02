#!/bin/sh
# Offline application checks; no installs and no implicit network access.
set -eu
if [ "$#" -ne 1 ]; then
  echo 'usage: validate.sh <application/Cargo.toml>' >&2
  exit 2
fi
skill_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python3 "$skill_dir/scripts/preflight.py" --manifest-path "$1"
cargo fmt --manifest-path "$1" -- --check
cargo check --manifest-path "$1" --all-targets --locked --offline
cargo test --manifest-path "$1" --locked --offline
