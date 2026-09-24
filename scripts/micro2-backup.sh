#!/usr/bin/env bash
# Gateway-native backup: versioned snapshot set without new infrastructure.
#
# Captures every live database via `dg export` (consistent committed
# snapshots) plus a consistent registry copy via Python's sqlite3 online
# backup API (no service stop, no sqlite3 CLI needed), with a checksum
# manifest and per-database checksum sidecars. Same-disk storage protects
# against logical loss (bad delete, corruption through the API), NOT disk
# loss: pull the versioned set off-host (scp/rsync) for the second leg.
#
# Usage: micro2-backup.sh [--server URL] [--key-file PATH] [--data-dir DIR]
#                         [--backup-root DIR] [--keep N]
# Defaults suit the micro2 layout. Run as the service user (dg). Prints the
# manifest only; never key material. Exit nonzero on any failure (set -e);
# a failed run publishes nothing.
set -euo pipefail

SERVER="http://127.0.0.1:7432"
KEY_FILE="/etc/dg/owner.key"
DATA_DIR="/var/lib/dg"
BACKUP_ROOT="/var/lib/dg-backups"
KEEP=3
while [ $# -gt 0 ]; do
  case "$1" in
    --server)
      [ $# -ge 2 ] || { echo "--server requires a value" >&2; exit 2; }
      SERVER="$2"; shift 2;;
    --key-file)
      [ $# -ge 2 ] || { echo "--key-file requires a value" >&2; exit 2; }
      KEY_FILE="$2"; shift 2;;
    --data-dir)
      [ $# -ge 2 ] || { echo "--data-dir requires a value" >&2; exit 2; }
      DATA_DIR="$2"; shift 2;;
    --backup-root)
      [ $# -ge 2 ] || { echo "--backup-root requires a value" >&2; exit 2; }
      BACKUP_ROOT="$2"; shift 2;;
    --keep)
      [ $# -ge 2 ] || { echo "--keep requires a value" >&2; exit 2; }
      KEEP="$2"; shift 2;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

DG_BIN="${DG_BIN:-dg}"
# Keep the executable and every path-bearing argument as distinct array
# elements. In particular, DG_BIN, --key-file, and --data-dir may contain
# spaces without changing the process argv seen by dg.
DG=("$DG_BIN" --server "$SERVER" --key-file "$KEY_FILE")
[[ "$KEEP" =~ ^[1-9][0-9]*$ ]] || {
  echo "--keep must be a positive integer" >&2
  exit 2
}
TS="$(date -u +%Y%m%dT%H%M%SZ)"
# Capture the conservative freshness boundary before listing or exporting any
# database. The API compares durable write markers against this value, not the
# later publication mtime.
STARTED_AT="$(date -u +%s)"
mkdir -p "$BACKUP_ROOT"
chmod 700 "$BACKUP_ROOT"
STAGE="$(mktemp -d "$BACKUP_ROOT/.staging.XXXXXX")"
PUBLISHED=0
cleanup() {
  if [ "$PUBLISHED" -eq 0 ]; then
    rm -rf "$STAGE"
  fi
}
trap cleanup EXIT

VERSION="$("${DG[@]}" --version | awk '{print $2}')"
DB_LIST="$STAGE/.databases.json"
"${DG[@]}" list >"$DB_LIST"
# NUL-delimited output avoids word splitting and preserves any legal name
# (including spaces) as one iteration value. The gateway's name validator is
# narrower, but this operator boundary remains safe for legacy/future names.
python3 - "$DB_LIST" >"$STAGE/.databases" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    payload = json.load(stream)
names = payload.get("databases")
if not isinstance(names, list):
    raise SystemExit("dg list response has no databases array")
for name in names:
    if (
        not isinstance(name, str)
        or not name
        or name in {".", ".."}
        or "/" in name
        or "\n" in name
        or "\r" in name
    ):
        raise SystemExit("dg list returned an unsafe database name")
    sys.stdout.write(name)
    sys.stdout.write("\0")
PY
mapfile -d '' -t DBS <"$STAGE/.databases"
if [ "${#DBS[@]}" -eq 0 ]; then
  echo "refusing empty backup: no live databases listed" >&2
  exit 1
fi

ENTRIES="$STAGE/.entries.jsonl"
: >"$ENTRIES"
for db in "${DBS[@]}"; do
  db_file="$STAGE/$db.sqlite"
  "${DG[@]}" export "$db" --out "$db_file" >/dev/null
  size="$(stat -c%s -- "$db_file")"
  sha="$(sha256sum -- "$db_file" | cut -d' ' -f1)"
  printf '%s\n' "$sha" >"$STAGE/$db.sha256"
  python3 - "$db" "$size" "$sha" "$db.sqlite" >>"$ENTRIES" <<'PY'
import json
import sys

name, size, sha256, filename = sys.argv[1:]
print(json.dumps({
    "name": name,
    "file": filename,
    "size": int(size),
    "sha256": sha256,
}, sort_keys=True))
PY
done

# Consistent registry copy through the SQLite online backup API: a raw file
# copy of a live WAL-mode database is not a valid backup.
META_SRC="$DATA_DIR/metadata.sqlite"
[ -f "$META_SRC" ] || { echo "missing registry: $META_SRC" >&2; exit 1; }
META_FILE="metadata.sqlite.bak"
python3 - "$META_SRC" "$STAGE/$META_FILE" <<'PY'
import sqlite3
import sys

src = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
dst = sqlite3.connect(sys.argv[2])
try:
    src.backup(dst)
    dst.commit()
    ok, = dst.execute("PRAGMA integrity_check").fetchone()
    if ok != "ok":
        raise RuntimeError(f"registry backup failed integrity: {ok}")
finally:
    dst.close()
    src.close()
PY
meta_size="$(stat -c%s -- "$STAGE/$META_FILE")"
meta_sha="$(sha256sum -- "$STAGE/$META_FILE" | cut -d' ' -f1)"
REGISTRY_CAPTURED_AT="$(date -u +%s)"
CAPTURE_COMPLETED_AT="$(date -u +%s)"

python3 - "$STAGE/manifest.json" "$TS" "$STARTED_AT" "$CAPTURE_COMPLETED_AT" "$REGISTRY_CAPTURED_AT" "$VERSION" "$META_FILE" "$meta_size" "$meta_sha" "$ENTRIES" <<'PY'
import datetime
import json
import sys

path, timestamp, started_at, completed_at, registry_captured_at, version, registry_file, registry_size, registry_sha, entries_path = sys.argv[1:]
with open(entries_path, encoding="utf-8") as stream:
    databases = [json.loads(line) for line in stream if line.strip()]
manifest = {
    "created": datetime.datetime.strptime(timestamp, "%Y%m%dT%H%M%SZ").replace(
        tzinfo=datetime.timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "timestamp": timestamp,
    "started_at": int(started_at),
    "completed_at": int(completed_at),
    "registry_captured_at": int(registry_captured_at),
    "verification": "validated_before_publication",
    "tool": "dg export",
    "dg_version": version,
    "databases": databases,
    "registry": {
        "file": registry_file,
        "size": int(registry_size),
        "sha256": registry_sha,
    },
}
with open(path, "w", encoding="utf-8") as stream:
    json.dump(manifest, stream, indent=2, sort_keys=True)
    stream.write("\n")
print(json.dumps(manifest, sort_keys=True))
PY

# Validate the complete private stage before publication: every database and
# registry file must be a standalone SQLite database, and the manifest,
# sidecar, and recomputed bytes must agree.
python3 - "$STAGE" <<'PY'
import hashlib
import json
import sqlite3
import sys
from pathlib import Path

root = Path(sys.argv[1])
with (root / "manifest.json").open(encoding="utf-8") as stream:
    manifest = json.load(stream)

def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()

def check_sqlite(path):
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        result, = connection.execute("PRAGMA integrity_check").fetchone()
        if result != "ok":
            raise RuntimeError(f"integrity_check failed for {path.name}: {result}")
    finally:
        connection.close()

for entry in manifest["databases"]:
    path = root / entry["file"]
    sidecar = root / f'{entry["name"]}.sha256'
    if not path.is_file() or path.stat().st_size != entry["size"]:
        raise RuntimeError(f"database size mismatch: {path.name}")
    actual = digest(path)
    if actual != entry["sha256"] or sidecar.read_text().strip() != actual:
        raise RuntimeError(f"database checksum mismatch: {path.name}")
    check_sqlite(path)

registry = manifest["registry"]
registry_path = root / registry["file"]
if not registry_path.is_file() or registry_path.stat().st_size != registry["size"]:
    raise RuntimeError("registry size mismatch")
if digest(registry_path) != registry["sha256"]:
    raise RuntimeError("registry checksum mismatch")
check_sqlite(registry_path)
PY

rm -f -- "$DB_LIST" "$STAGE/.databases" "$ENTRIES"
DEST="$BACKUP_ROOT/$TS"
chmod -R go-rwx "$STAGE"
# A version timestamp has one-second resolution. Never let a concurrent run
# nest a new stage inside an already-published set or overwrite that set.
if [ -e "$DEST" ]; then
  echo "backup set already exists: $DEST" >&2
  exit 1
fi
mv -T "$STAGE" "$DEST"
PUBLISHED=1
chmod 700 "$DEST"

# Prune to KEEP latest versioned sets (staging dirs never match the glob).
mapfile -t SETS < <(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' \
  | awk '/^[0-9]{8}T[0-9]{6}Z$/' | sort -r)
for ((index = KEEP; index < ${#SETS[@]}; index++)); do
  rm -rf -- "$BACKUP_ROOT/${SETS[$index]}"
done
echo "backup set: $DEST (kept latest $KEEP)" >&2
