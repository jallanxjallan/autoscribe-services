#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
AUTOSCRIBE_ROOT=${1:-${AUTOSCRIBE_ROOT:-}}
if [ -z "$AUTOSCRIBE_ROOT" ]; then
  echo "usage: $0 /path/to/autoscribe-checkout" >&2
  exit 2
fi
AUTOSCRIBE_ROOT=$(cd "$AUTOSCRIBE_ROOT" && pwd -P)

cd "$ROOT"
cargo build --workspace
BIN="$ROOT/target/debug"
ASC="$AUTOSCRIBE_ROOT/app/bin/asc"

TMP=$(mktemp -d /tmp/autoscribe-dropbox-smoke.XXXXXX)
EXEC_PID=
WORK_PID=
REDIS_PORT=${AUTOSCRIBE_SMOKE_REDIS_PORT:-16379}

cleanup() {
  set +e
  [ -n "$EXEC_PID" ] && kill "$EXEC_PID" 2>/dev/null
  [ -n "$WORK_PID" ] && kill "$WORK_PID" 2>/dev/null
  redis-cli -p "$REDIS_PORT" shutdown nosave >/dev/null 2>&1
  rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$TMP/repos" "$TMP/state" "$TMP/bin" "$TMP/dropbox/incoming"   "$TMP/dropbox/outgoing" "$TMP/extensions" "$TMP/redis"

python3 - <<PY
from pathlib import Path
import secrets
p=Path("$TMP/effect.key")
p.write_text(secrets.token_hex(32)+"\n")
p.chmod(0o600)
PY

cat > "$TMP/services.toml" <<EOF
[paths]
repo_roots = ["$TMP/repos"]
file_roots = []
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
EOF
POLICY="$TMP/services.toml"

cat > "$TMP/bin/rclone" <<'EOF'
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
    (cd "$root" && find . -maxdepth 1 -type f -printf '%P\n' | LC_ALL=C sort)
    ;;
  cat)
    cat "$(map_remote "$1")"
    ;;
  deletefile)
    rm -f "$(map_remote "$1")"
    ;;
  copyto)
    src=$1
    dst=$(map_remote "$2")
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    ;;
  moveto)
    src=$(map_remote "$1")
    dst=$(map_remote "$2")
    mkdir -p "$(dirname "$dst")"
    mv -f "$src" "$dst"
    ;;
  *)
    echo "unsupported fake rclone command: $cmd" >&2
    exit 2
    ;;
esac
EOF
chmod +x "$TMP/bin/rclone"

cat > "$TMP/extensions/smoke_transform.py" <<'PY'
#!/usr/bin/env python3
import sys
text = sys.stdin.read()
sys.stdout.write("PYTHON-SMOKE:" + text.upper())
PY
chmod +x "$TMP/extensions/smoke_transform.py"

python3 - <<PY
import json
from pathlib import Path
Path("$TMP/extensions/registry.json").write_text(json.dumps({
    "smoke-local": "$TMP/extensions/smoke_transform.py"
}))
PY

git init --bare -b main "$TMP/repos/control.git" >/dev/null
git init -b main "$TMP/control-work" >/dev/null
git -C "$TMP/control-work" config user.name Smoke
git -C "$TMP/control-work" config user.email smoke@localhost
mkdir -p "$TMP/control-work/plans" "$TMP/control-work/steps"

cat > "$TMP/control-work/steps/stp_0123456789ABCDEG.json" <<'EOF'
{
  "identity": "stp_0123456789ABCDEG",
  "label": "Local Python Smoke Transform",
  "engine_kind": "script",
  "engine": "local",
  "script": "smoke-local",
  "args": {}
}
EOF

cat > "$TMP/control-work/plans/pln_0123456789ABCDEF.json" <<'EOF'
{
  "identity": "pln_0123456789ABCDEF",
  "title": "Local Python Transport Smoke",
  "description": "Deterministic transport smoke test without an LLM call.",
  "scope": "system",
  "steps": ["stp_0123456789ABCDEG"]
}
EOF

git -C "$TMP/control-work" add .
git -C "$TMP/control-work" commit -m smoke-control >/dev/null
git -C "$TMP/control-work" remote add origin "$TMP/repos/control.git"
git -C "$TMP/control-work" push origin main >/dev/null
CONTROL_SHA=$(git -C "$TMP/control-work" rev-parse HEAD)
"$BIN/srv-control-ingest" --policy "$POLICY"   --repo "$TMP/repos/control.git" --commit "$CONTROL_SHA" >/dev/null

redis-server --port "$REDIS_PORT" --save '' --appendonly no   --dir "$TMP/redis" --daemonize yes
redis-cli -p "$REDIS_PORT" ping | grep -qx PONG

export AUTOSCRIBE_CONTROL_DB="$TMP/state/control.sqlite"
export AUTOSCRIBE_LEDGER_DB="$TMP/state/ledger.sqlite"
export AUTOSCRIBE_REDIS_HOST=127.0.0.1
export AUTOSCRIBE_REDIS_PORT="$REDIS_PORT"
export AUTOSCRIBE_EXTENSION_REGISTRY="$TMP/extensions/registry.json"
export AUTOSCRIBE_EXECUTOR_POLL_SECONDS=0.05
export AUTOSCRIBE_WORKER_POLL_SECONDS=0.05
export AUTOSCRIBE_RCLONE="$TMP/bin/rclone"
export AUTOSCRIBE_FAKE_DROPBOX="$TMP/dropbox"
export AUTOSCRIBE_ASC="$ASC"

python3 "$AUTOSCRIBE_ROOT/app/daemon/executord.py" >"$TMP/executord.log" 2>&1 &
EXEC_PID=$!
python3 "$AUTOSCRIBE_ROOT/app/daemon/workerd.py" >"$TMP/workerd.log" 2>&1 &
WORK_PID=$!

echo '[1/4] Ingress validates client NDJSON, strips content/plan, and preserves opaque baggage'
python3 - <<PY > "$TMP/dropbox/incoming/smoke.ndjson"
import hashlib, json
content = "hello autoscribe"
print(json.dumps({
    "plan": "pln_0123456789ABCDEF",
    "content": content,
    "slug": "smoke-source",
    "body_sha256": hashlib.sha256(content.encode()).hexdigest(),
    "source_path": "Studio/Test/Smoke.md",
    "vault": "Test"
}, separators=(",", ":")))
PY
"$BIN/srv-input" --policy "$POLICY" --once
[ ! -e "$TMP/dropbox/incoming/smoke.ndjson" ]

echo '[2/4] AutoScribe executes the local Python step and produces a pending response'
PENDING=
for _ in $(seq 1 200); do
  PENDING=$("$ASC" export pending)
  [ -n "$PENDING" ] && break
  sleep 0.05
done
if [ -z "$PENDING" ]; then
  echo "response did not become exportable" >&2
  cat "$TMP/executord.log" >&2 || true
  cat "$TMP/workerd.log" >&2 || true
  exit 1
fi

echo '[3/4] Export daemon reattaches baggage and writes one response NDJSON'
"$BIN/srv-export" --policy "$POLICY" --once
python3 - <<PY
import hashlib, json
from pathlib import Path
files = sorted(Path("$TMP/dropbox/outgoing").glob("*.ndjson"))
assert len(files) == 1, files
record = json.loads(files[0].read_text())
assert record["schema"] == "autoscribe.client-response.v1"
assert record["result"] == "PYTHON-SMOKE:HELLO AUTOSCRIBE"
assert record["baggage"] == {
    "slug": "smoke-source",
    "body_sha256": hashlib.sha256(b"hello autoscribe").hexdigest(),
    "source_path": "Studio/Test/Smoke.md",
    "vault": "Test",
}
assert record["call_id"]
PY
[ -z "$("$ASC" export pending)" ]

echo '[4/4] Ingress rejects non-text control characters and leaves the transport for inspection'
python3 - <<PY > "$TMP/dropbox/incoming/reject.ndjson"
import json
print(json.dumps({
    "plan": "pln_0123456789ABCDEF",
    "content": "bad\u0000text",
    "slug": "bad-source"
}))
PY
if "$BIN/srv-input" --policy "$POLICY" --once >/dev/null 2>&1; then
  echo "non-text content was incorrectly accepted" >&2
  exit 1
fi
[ -e "$TMP/dropbox/incoming/reject.ndjson" ]

echo 'AutoScribe Dropbox NDJSON + local Python smoke test: PASS'
