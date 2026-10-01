#!/usr/bin/env bash
set -euo pipefail

SYNC=${CONTROL_SYNC_BIN:-/opt/autoscribe/services/current/bin/srv-control-sync}

if [[ ! -x "$SYNC" ]]; then
  echo "Control sync binary not executable: $SYNC" >&2
  exit 1
fi

args=(--no-fetch)

if [[ -n "${AUTOSCRIBE_POLICY:-}" ]]; then
  args+=(--policy "$AUTOSCRIBE_POLICY")
fi
if [[ -n "${CONTROL_REPO:-}" ]]; then
  args+=(--repo "$CONTROL_REPO")
fi
if [[ -n "${CONTROL_REF:-}" ]]; then
  args+=(--branch "$CONTROL_REF")
fi
if [[ -n "${AUTOSCRIBE_CONTROL_DB:-}" ]]; then
  args+=(--db "$AUTOSCRIBE_CONTROL_DB")
fi
if [[ -n "${CONTROL_INGEST_BIN:-}" ]]; then
  args+=(--ingest "$CONTROL_INGEST_BIN")
fi

exec "$SYNC" "${args[@]}" "$@"
