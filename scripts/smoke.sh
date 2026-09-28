#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

cargo build --workspace

BIN="$ROOT/target/debug"
TMP=$(mktemp -d /tmp/autoscribe-services-smoke.XXXXXX)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/repos" "$TMP/files" "$TMP/state"

python3 - <<PY
from pathlib import Path
import secrets
p=Path("$TMP/effect.key")
p.write_text(secrets.token_hex(32)+"\n")
p.chmod(0o600)
PY

cat > "$TMP/services.toml" <<EOF2
[paths]
repo_roots = ["$TMP/repos"]
file_roots = ["$TMP/files"]
effect_key_file = "$TMP/effect.key"
effects_db = "$TMP/state/effects.sqlite"
control_db = "$TMP/state/control.sqlite"

[limits]
max_record_bytes = 2097152
max_body_bytes = 1048576
max_outputs = 16
max_control_file_bytes = 262144
max_control_records = 10000

[git]
default_branch = "main"
user_name = "AutoScribe Smoke"
user_email = "autoscribe-smoke@localhost"
EOF2

POLICY="$TMP/services.toml"

echo '[1/6] srv-control-ingest validates a Git snapshot and rebuilds SQLite'
git init --bare -b main "$TMP/repos/control.git" >/dev/null
git init -b main "$TMP/control-work" >/dev/null
git -C "$TMP/control-work" config user.name Smoke
git -C "$TMP/control-work" config user.email smoke@localhost
cat > "$TMP/control-work/Role.md" <<'EOF2'
---
title: Smoke Role
identity: rol_0123456789ABCDEF
type: instruction
scope: role
tags: []
---
Write precise smoke-test prose.
EOF2
cat > "$TMP/control-work/Task.md" <<'EOF2'
---
title: Smoke Task
identity: spc_0123456789ABCDEF
type: instruction
scope: task
tags: []
---
Rewrite the supplied text without changing meaning.
EOF2
git -C "$TMP/control-work" add .
git -C "$TMP/control-work" commit -m control-smoke >/dev/null
git -C "$TMP/control-work" remote add origin "$TMP/repos/control.git"
git -C "$TMP/control-work" push origin main >/dev/null
CONTROL_SHA=$(git -C "$TMP/control-work" rev-parse HEAD)
"$BIN/srv-control-ingest" --policy "$POLICY" --repo "$TMP/repos/control.git" --commit "$CONTROL_SHA" --check-only >/dev/null
"$BIN/srv-control-ingest" --policy "$POLICY" --repo "$TMP/repos/control.git" --commit "$CONTROL_SHA" >/dev/null
python3 - <<PY
import sqlite3
con=sqlite3.connect("$TMP/state/control.sqlite")
assert con.execute("select count(*) from control_records").fetchone()[0] == 2
assert con.execute("select value from control_meta where key='source_commit'").fetchone()[0] == "$CONTROL_SHA"
PY

echo '[2/6] srv-input normalizes an allow-listed file'
printf 'Original text\n' > "$TMP/files/input.md"
python3 - <<PY > "$TMP/input-request.ndjson"
import json
print(json.dumps({
  "schema":"autoscribe.input.request.v1",
  "source":{"kind":"file","path":"$TMP/files/input.md"},
  "routing":{"plan_id":"pln_SMOKE"},
  "baggage":{"outputs":[]}
}))
PY
"$BIN/srv-input" --policy "$POLICY" < "$TMP/input-request.ndjson" > "$TMP/canonical-input.ndjson"
python3 - <<PY
import json
r=json.loads(open("$TMP/canonical-input.ndjson").read())
assert r["schema"] == "autoscribe.input.v1"
assert r["content"] == "Original text\n"
assert r["record_id"].startswith("inp_")
PY

echo '[3/6] srv-output signs a repo effect; srv-writeback creates one commit'
python3 - <<PY > "$TMP/repo-response.ndjson"
import json
print(json.dumps({
  "schema":"autoscribe.response.v1",
  "call_id":"01SMOKEREPO",
  "content":"Rewritten text\n",
  "baggage":{"outputs":[{
    "kind":"repo",
    "repo":"$TMP/repos/output.git",
    "path":"Contents/Result.md",
    "branch":"main",
    "create_repo":True
  }]}
}))
PY
"$BIN/srv-output" --policy "$POLICY" < "$TMP/repo-response.ndjson" > "$TMP/repo-effect.ndjson"
"$BIN/srv-writeback" --policy "$POLICY" < "$TMP/repo-effect.ndjson" > "$TMP/repo-receipt.ndjson"
VALUE=$(git --git-dir "$TMP/repos/output.git" show main:Contents/Result.md)
[ "$VALUE" = "Rewritten text" ]
COUNT=$(git --git-dir "$TMP/repos/output.git" rev-list --count main)
[ "$COUNT" = "1" ]

echo '[4/6] writeback retry is idempotent'
"$BIN/srv-writeback" --policy "$POLICY" < "$TMP/repo-effect.ndjson" > "$TMP/repo-receipt-2.ndjson"
cmp "$TMP/repo-receipt.ndjson" "$TMP/repo-receipt-2.ndjson"
COUNT=$(git --git-dir "$TMP/repos/output.git" rev-list --count main)
[ "$COUNT" = "1" ]

echo '[5/6] tampering with an authenticated effect is rejected'
python3 - <<PY
import json
p="$TMP/repo-effect.ndjson"
r=json.loads(open(p).read())
r["effect"]["path"]="Contents/Evil.md"
open("$TMP/tampered-effect.ndjson","w").write(json.dumps(r)+"\n")
PY
if "$BIN/srv-writeback" --policy "$POLICY" < "$TMP/tampered-effect.ndjson" >/dev/null 2>&1; then
  echo 'tampered effect was incorrectly accepted' >&2
  exit 1
fi

echo '[6/6] srv-export atomically applies a signed file effect'
python3 - <<PY > "$TMP/file-response.ndjson"
import json
print(json.dumps({
  "schema":"autoscribe.response.v1",
  "call_id":"01SMOKEFILE",
  "content":"Exported text\n",
  "baggage":{"outputs":[{
    "kind":"file",
    "path":"$TMP/files/export.md",
    "mode":"replace"
  }]}
}))
PY
"$BIN/srv-output" --policy "$POLICY" < "$TMP/file-response.ndjson" > "$TMP/file-effect.ndjson"
"$BIN/srv-export" --policy "$POLICY" < "$TMP/file-effect.ndjson" > "$TMP/file-receipt.ndjson"
[ "$(cat "$TMP/files/export.md")" = "Exported text" ]

echo 'AutoScribe services smoke test: PASS'
