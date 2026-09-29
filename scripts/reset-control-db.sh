#!/usr/bin/env bash
set -euo pipefail

CONTROL_REPO=${CONTROL_REPO:-"$HOME/Repos/control.git"}
CONTROL_REF=${CONTROL_REF:-main}
CONTROL_DB=${AUTOSCRIBE_CONTROL_DB:-/var/lib/autoscribe/control.sqlite}
POLICY=${AUTOSCRIBE_POLICY:-/etc/autoscribe/services.toml}
INGEST=${CONTROL_INGEST_BIN:-/opt/autoscribe/services/current/bin/srv-control-ingest}

if [[ ! -d "$CONTROL_REPO" ]]; then
  echo "Control repo not found: $CONTROL_REPO" >&2
  exit 1
fi
if [[ ! -x "$INGEST" ]]; then
  echo "Control ingester not executable: $INGEST" >&2
  exit 1
fi

COMMIT=$(git --git-dir="$CONTROL_REPO" rev-parse "${CONTROL_REF}^{commit}")

echo "Validating Control $COMMIT from $CONTROL_REPO"
"$INGEST" \
  --policy "$POLICY" \
  --repo "$CONTROL_REPO" \
  --commit "$COMMIT" \
  --check-only

echo "Resetting $CONTROL_DB"
rm -f "$CONTROL_DB" "$CONTROL_DB-wal" "$CONTROL_DB-shm"

"$INGEST" \
  --policy "$POLICY" \
  --repo "$CONTROL_REPO" \
  --commit "$COMMIT" \
  --db "$CONTROL_DB"

if command -v sqlite3 >/dev/null 2>&1; then
  FK_ERRORS=$(sqlite3 "$CONTROL_DB" 'PRAGMA foreign_key_check;')
  if [[ -n "$FK_ERRORS" ]]; then
    echo "Foreign-key check failed:" >&2
    printf '%s\n' "$FK_ERRORS" >&2
    exit 1
  fi

  sqlite3 -header -column "$CONTROL_DB" <<'SQL'
SELECT 'instructions' AS table_name, COUNT(*) AS rows FROM instructions
UNION ALL
SELECT 'plans', COUNT(*) FROM plans
UNION ALL
SELECT 'steps', COUNT(*) FROM steps
UNION ALL
SELECT 'step_instructions', COUNT(*) FROM step_instructions;
SQL
fi

echo "Control database rebuilt from $COMMIT"
