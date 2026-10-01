#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "Refusing install from a dirty services worktree" >&2
  exit 1
fi

SHA=$(git rev-parse HEAD)
RELEASE="/opt/autoscribe/services/releases/$SHA"
CURRENT="/opt/autoscribe/services/current"
BINS=(srv-input srv-output srv-control-ingest srv-control-sync srv-writeback srv-export)

cargo build --release --workspace

sudo mkdir -p "$RELEASE/bin"
for bin in "${BINS[@]}"; do
  sudo install -m 0755 "target/release/$bin" "$RELEASE/bin/$bin"
done
sudo install -m 0755 scripts/reset-control-db.sh "$RELEASE/bin/reset-control-db"
sudo ln -sfn "$RELEASE" "$CURRENT.new"
sudo mv -Tf "$CURRENT.new" "$CURRENT"
sudo ln -sfn "$CURRENT/bin/srv-control-sync" /usr/local/bin/control-sync

echo "Installed services commit $SHA"
echo "Current: $CURRENT -> $RELEASE"
echo "Control sync: /usr/local/bin/control-sync"
