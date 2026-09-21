#!/usr/bin/env bash
# Gateway-native backup: versioned snapshot set without new infrastructure.
#
# Captures every live database via `dg export` (consistent committed
# snapshots) plus a consistent registry copy via Python's sqlite3 online
# backup API (no service stop, no sqlite3 CLI needed), with a checksum
# manifest. Same-disk storage protects against logical loss (bad delete,
# corruption through the API), NOT disk loss: pull the versioned set
# off-host (scp/rsync) for the second leg.
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
    --server) SERVER="$2"; shift 2;;
    --key-file) KEY_FILE="$2"; shift 2;;
    --data-dir) DATA_DIR="$2"; shift 2;;
    --backup-root) BACKUP_ROOT="$2"; shift 2;;
    --keep) KEEP="$2"; shift 2;;
    *) echo "unknown flag: $1" >&2; exit 2;;
  esac
done

DG_BIN="${DG_BIN:-dg}"
DG="$DG_BIN --server $SERVER --key-file $KEY_FILE"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
STAGE="$BACKUP_ROOT/.staging-$TS"
mkdir -p "$BACKUP_ROOT" "$STAGE"
chmod 700 "$BACKUP_ROOT"

VERSION="$($DG --version | awk '{print $2}')"
DBS="$($DG list | python3 -c 'import json,sys; print("\n".join(json.load(sys.stdin)["databases"]))')"
[ -n "$DBS" ] || { echo "refusing empty backup: no live databases listed" >&2; exit 1; }

MANIFEST_ENTRIES=""
for db in $DBS; do
  $DG export "$db" --out "$STAGE/$db.sqlite" >/dev/null
  size="$(stat -c%s "$STAGE/$db.sqlite")"
  sha="$(sha256sum "$STAGE/$db.sqlite" | cut -d' ' -f1)"
  MANIFEST_ENTRIES="$MANIFEST_ENTRIES{\"name\":\"$db\",\"size\":$size,\"sha256\":\"$sha\"},"
done

# Consistent registry copy through the SQLite online backup API: a raw file
# copy of a live WAL-mode database is not a valid backup.
META_SRC="$DATA_DIR/metadata.sqlite"
[ -f "$META_SRC" ] || { echo "missing registry: $META_SRC" >&2; exit 1; }
python3 - "$META_SRC" "$STAGE/metadata.sqlite" <<'EOF'
import sqlite3, sys
src = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
dst = sqlite3.connect(sys.argv[2])
with dst:
    src.backup(dst)
ok, = dst.execute("PRAGMA integrity_check").fetchone()
assert ok == "ok", f"registry backup failed integrity: {ok}"
EOF
meta_size="$(stat -c%s "$STAGE/metadata.sqlite")"
meta_sha="$(sha256sum "$STAGE/metadata.sqlite" | cut -d' ' -f1)"

python3 - "$STAGE/MANIFEST.json" "$TS" "$VERSION" "$meta_size" "$meta_sha" "$MANIFEST_ENTRIES" <<'EOF'
import json, sys
_, path, ts, version, meta_size, meta_sha, entries = sys.argv
dbs = json.loads("[" + entries.rstrip(",") + "]")
manifest = {"timestamp": ts, "dg_version": version,
            "databases": dbs,
            "registry": {"size": int(meta_size), "sha256": meta_sha}}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
print(json.dumps(manifest))
EOF

DEST="$BACKUP_ROOT/$TS"
chmod -R go-rwx "$STAGE"
mv "$STAGE" "$DEST"
chmod 700 "$DEST"

# Prune to KEEP latest versioned sets (staging dirs never match the glob).
kept=0
for old in $(ls -1 "$BACKUP_ROOT" | grep -E '^[0-9]{8}T[0-9]{6}Z$' | sort -r); do
  kept=$((kept + 1))
  if [ "$kept" -gt "$KEEP" ]; then
    rm -rf "$BACKUP_ROOT/$old"
  fi
done
echo "backup set: $DEST (kept latest $KEEP)" >&2
