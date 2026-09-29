#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

cargo build --workspace

BIN="$ROOT/target/debug"
TMP=$(mktemp -d /tmp/autoscribe-services-smoke.XXXXXX)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/repos" "$TMP/files" "$TMP/state" "$TMP/bin" \
  "$TMP/dropbox/incoming/smoke-batch" "$TMP/dropbox/outgoing"

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
max_outputs = 1
max_control_file_bytes = 262144
max_control_records = 10000

[git]
default_branch = "main"
user_name = "AutoScribe Smoke"
user_email = "autoscribe-smoke@localhost"
EOF2
POLICY="$TMP/services.toml"

cat > "$TMP/bin/rclone" <<'EOF2'
#!/usr/bin/env bash
set -euo pipefail
: "${AUTOSCRIBE_FAKE_DROPBOX:?}"
map_remote() {
  case "$1" in
    dropbox:biznet) printf '%s\n' "$AUTOSCRIBE_FAKE_DROPBOX" ;;
    dropbox:biznet/*) printf '%s/%s\n' "$AUTOSCRIBE_FAKE_DROPBOX" "${1#dropbox:biznet/}" ;;
    *) echo "unexpected fake rclone remote: $1" >&2; exit 2 ;;
  esac
}
cmd=${1:-}
shift || true
case "$cmd" in
  lsf)
    remote=${!#}
    root=$(map_remote "$remote")
    [ -d "$root" ] || exit 0
    (cd "$root" && find . -type f -printf '%P\n' | LC_ALL=C sort)
    ;;
  cat)
    cat "$(map_remote "$1")"
    ;;
  copyto)
    src=$1
    dst=$(map_remote "$2")
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    ;;
  *)
    echo "unsupported fake rclone command: $cmd" >&2
    exit 2
    ;;
esac
EOF2
chmod +x "$TMP/bin/rclone"
export AUTOSCRIBE_RCLONE="$TMP/bin/rclone"
export AUTOSCRIBE_FAKE_DROPBOX="$TMP/dropbox"

echo '[1/7] Control ingest still validates and rebuilds its SQLite catalogue'
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
git -C "$TMP/control-work" add .
git -C "$TMP/control-work" commit -m control-smoke >/dev/null
git -C "$TMP/control-work" remote add origin "$TMP/repos/control.git"
git -C "$TMP/control-work" push origin main >/dev/null
CONTROL_SHA=$(git -C "$TMP/control-work" rev-parse HEAD)
"$BIN/srv-control-ingest" --policy "$POLICY" --repo "$TMP/repos/control.git" --commit "$CONTROL_SHA" --check-only >/dev/null

echo '[2/7] Repo mode derives plan/source and binds return to the same repo/path'
git init --bare -b main "$TMP/repos/content.git" >/dev/null
git init -b main "$TMP/content-work" >/dev/null
git -C "$TMP/content-work" config user.name Smoke
git -C "$TMP/content-work" config user.email smoke@localhost
cat > "$TMP/content-work/Opening.md" <<'EOF2'
---
identity: psg_SMOKE
---
Original repo text.
EOF2
git -C "$TMP/content-work" add .
git -C "$TMP/content-work" commit -m $'dispatch smoke\n\nPlan: Smoke Plan pln_SMOKE' >/dev/null
git -C "$TMP/content-work" remote add origin "$TMP/repos/content.git"
git -C "$TMP/content-work" push origin main >/dev/null
CONTENT_SHA=$(git -C "$TMP/content-work" rev-parse HEAD)
"$BIN/srv-input" --policy "$POLICY" repo \
  --repo "$TMP/repos/content.git" --commit "$CONTENT_SHA" --branch main \
  > "$TMP/repo-input.ndjson"
python3 - <<PY
import json
r=json.loads(open("$TMP/repo-input.ndjson").read())
assert r["schema"] == "autoscribe.input.v1"
assert r["routing"] == {"plan_id":"pln_SMOKE"}
a=r["baggage"]["autoscribe_return"]
assert a["route"]["kind"] == "repo"
assert a["route"]["repo"] == "$TMP/repos/content.git"
assert a["route"]["path"] == "Opening.md"
assert "outputs" not in r["baggage"]
PY

echo '[3/7] Repo response can only become a same-repo effect and writes back once'
python3 - <<PY > "$TMP/repo-response.ndjson"
import json
r=json.loads(open("$TMP/repo-input.ndjson").read())
print(json.dumps({
  "schema":"autoscribe.response.v1",
  "call_id":"01SMOKEREPO",
  "content":"Rewritten repo text.\n",
  "baggage":r["baggage"],
}))
PY
"$BIN/srv-output" --policy "$POLICY" < "$TMP/repo-response.ndjson" > "$TMP/repo-effect.ndjson"
python3 - <<PY
import json
r=json.loads(open("$TMP/repo-effect.ndjson").read())
assert r["effect"]["kind"] == "repo"
assert r["effect"]["repo"] == "$TMP/repos/content.git"
assert r["effect"]["path"] == "Opening.md"
assert r["effect"]["create_repo"] is False
PY
"$BIN/srv-writeback" --policy "$POLICY" < "$TMP/repo-effect.ndjson" > "$TMP/repo-receipt.ndjson"
[ "$(git --git-dir "$TMP/repos/content.git" show main:Opening.md)" = "Rewritten repo text." ]
COUNT=$(git --git-dir "$TMP/repos/content.git" rev-list --count main)
[ "$COUNT" = "2" ]
"$BIN/srv-writeback" --policy "$POLICY" < "$TMP/repo-effect.ndjson" > "$TMP/repo-receipt-2.ndjson"
cmp "$TMP/repo-receipt.ndjson" "$TMP/repo-receipt-2.ndjson"
[ "$(git --git-dir "$TMP/repos/content.git" rev-list --count main)" = "2" ]

echo '[4/7] Writeback commit does not redispatch because it has no Plan: line'
WRITEBACK_SHA=$(git --git-dir "$TMP/repos/content.git" rev-parse main)
"$BIN/srv-input" --policy "$POLICY" repo \
  --repo "$TMP/repos/content.git" --commit "$WRITEBACK_SHA" --branch main \
  > "$TMP/no-loop.ndjson"
[ ! -s "$TMP/no-loop.ndjson" ]

echo '[5/7] Tampering with the signed return route is rejected before effect creation'
python3 - <<PY > "$TMP/tampered-response.ndjson"
import json
r=json.loads(open("$TMP/repo-response.ndjson").read())
r["baggage"]["autoscribe_return"]["route"]["path"]="Evil.md"
print(json.dumps(r))
PY
if "$BIN/srv-output" --policy "$POLICY" < "$TMP/tampered-response.ndjson" >/dev/null 2>&1; then
  echo 'tampered return route was incorrectly accepted' >&2
  exit 1
fi

echo '[6/7] Direct mode reads a named incoming batch and binds it to matching outgoing batch'
printf 'Original direct text.\n' > "$TMP/dropbox/incoming/smoke-batch/note.txt"
"$BIN/srv-input" --policy "$POLICY" direct --batch smoke-batch --plan pln_SMOKE \
  > "$TMP/direct-input.ndjson"
python3 - <<PY
import json
r=json.loads(open("$TMP/direct-input.ndjson").read())
assert r["source"]["kind"] == "dropbox"
assert r["source"]["batch"] == "smoke-batch"
assert r["routing"] == {"plan_id":"pln_SMOKE"}
a=r["baggage"]["autoscribe_return"]
assert a["route"] == {"kind":"dropbox","batch":"smoke-batch","path":"note.txt"}
PY

echo '[7/7] Direct response exports only to dropbox:biznet/outgoing/<same-batch>'
python3 - <<PY > "$TMP/direct-response.ndjson"
import json
r=json.loads(open("$TMP/direct-input.ndjson").read())
print(json.dumps({
  "schema":"autoscribe.response.v1",
  "call_id":"01SMOKEDIRECT",
  "content":"Rewritten direct text.\n",
  "baggage":r["baggage"],
}))
PY
"$BIN/srv-output" --policy "$POLICY" < "$TMP/direct-response.ndjson" > "$TMP/direct-effect.ndjson"
python3 - <<PY
import json
r=json.loads(open("$TMP/direct-effect.ndjson").read())
assert r["effect"] == {"kind":"dropbox","batch":"smoke-batch","path":"note.txt"}
PY
"$BIN/srv-export" --policy "$POLICY" < "$TMP/direct-effect.ndjson" > "$TMP/direct-receipt.ndjson"
[ "$(cat "$TMP/dropbox/outgoing/smoke-batch/note.txt")" = "Rewritten direct text." ]

echo 'AutoScribe locked-mode services smoke test: PASS'
