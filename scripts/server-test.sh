#!/usr/bin/env bash
set -euo pipefail

cd "$HOME/services"
git fetch origin main
git reset --hard origin/main
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/smoke.sh
