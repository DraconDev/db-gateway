#!/usr/bin/env bash
# Clean-host phases — the operator procedure, executed INSIDE the clean host.
#
# This file is the half of the clean-host rehearsal that runs on the host
# under test. scripts/clean-host-rehearsal.sh owns the orchestration; this
# file owns the procedure, so the steps here can be read as the documented
# deployment procedure and checked against docs/deployment.md.
#
# Two modes, matching the two halves of the rehearsal:
#
#   deploy    The documented first deploy (docs/deployment.md steps 1-6):
#             install the release bundle, create the service user, create
#             the data directory, mint the owner credential, install and
#             start the unit. Then the first-deploy assertions (liveness,
#             authenticated read/write, restart survival), a published
#             backup set from the canonical producer, and the off-host leg.
#
#   restore   The disaster-recovery half, on a REPLACEMENT host: provision
#             from the same bundle, restore the off-host artifact onto an
#             empty target (docs/recovery-contract.md §3, Path A), and
#             assert the recovered state.
#
# Inputs, all inside the container:
#   /root/bundle            the release bundle (dg, SHA256SUMS, deploy/dg.service)
#   /usr/local/bin/backup.sh  the canonical backup producer, at the
#                           path the deployment guide installs it at
#   /offhost                the off-host artifact directory (a bind mount
#                           from outside this host, so it survives the loss
#                           of the host itself)
#
# Nothing here reaches outside the container except /offhost. The script
# never prints credential material: the owner key is written straight to a
# mode-0600 file and only its metadata is ever reported.

set -euo pipefail

MODE="${1:-}"
if [ -z "$MODE" ]; then
    echo "[phase] FAIL: no mode given (expected 'deploy' or 'restore')" >&2
    exit 2
fi

BUNDLE="/root/bundle"
OFFHOST="/offhost"
DATA_DIR="/var/lib/dg"
BACKUP_ROOT="/var/lib/dg-backups"
KEY_FILE="/etc/dg/owner.key"
BINARY="/usr/local/bin/dg"
UNIT="/etc/systemd/system/dg.service"
SERVICE_USER="dg"
LISTEN="127.0.0.1:7432"
SERVER="http://${LISTEN}"
DB="deploy_smoke"

# The seeded application data. The recovery contract (§4) requires the drill
# to fix its schema and rows up front and to compare exact rows afterwards --
# counts alone are not a pass.
EXPECTED_AUTHORS='[[1,"octavia"],[2,"ursula"]]'
EXPECTED_BOOKS='[[1,1,"kindred",9.5],[2,2,"earthsea",12.0]]'

log() { echo "[phase] $*"; }
fail() { echo "[phase] FAIL: $*" >&2; exit 1; }

# Run a command as the unprivileged service user.
as_user() {
    if command -v runuser >/dev/null 2>&1; then
        runuser -u "$SERVICE_USER" -- "$@"
    else
        su -s /bin/bash "$SERVICE_USER" -c "$(printf '%q ' "$@")"
    fi
}

# The gateway CLI, always with the owner credential file.
dg_cli() {
    "$BINARY" --json --server "$SERVER" --key-file "$KEY_FILE" "$@"
}

# query_rows <db> <sql> -- the exact rows, as compact JSON.
query_rows() {
    dg_cli query "$1" "$2" \
        | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin).get("rows"), separators=(",",":")))'
}

# assert_rows <label> <expected> <actual>
assert_rows() {
    if [ "$2" != "$3" ]; then
        fail "$1: expected [$2], got [$3]"
    fi
    log "PASS $1 = $3"
}

# assert_eq <label> <expected> <actual>
assert_eq() {
    if [ "$2" != "$3" ]; then
        fail "$1: expected [$2], got [$3]"
    fi
    log "PASS $1 = $3"
}

wait_for_service() {
    local _
    for _ in $(seq 1 60); do
        if systemctl is-active --quiet dg.service; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# ---------------------------------------------------------------------------
# The documented first deploy (docs/deployment.md steps 1-6).
# ---------------------------------------------------------------------------

assert_host_is_clean() {
    log "asserting the host carries nothing of the project"
    [ ! -e "$BINARY" ] || fail "$BINARY already exists: this host is not clean"
    [ ! -e "$DATA_DIR" ] || fail "$DATA_DIR already exists: this host is not clean"
    [ ! -e /etc/dg ] || fail "/etc/dg already exists: this host is not clean"
    if id "$SERVICE_USER" >/dev/null 2>&1; then
        fail "user $SERVICE_USER already exists: this host is not clean"
    fi
    if systemctl list-unit-files dg.service >/dev/null 2>&1 \
        && systemctl list-unit-files dg.service | grep -q '^dg.service'; then
        fail "dg.service is already installed: this host is not clean"
    fi
    log "PASS the host is clean (no binary, no data dir, no service user, no unit)"
}

install_from_bundle() {
    log "step 1: install the release bundle"
    [ -d "$BUNDLE" ] || fail "no bundle at $BUNDLE"
    # The bundle is verified ON the host, exactly as the first-deploy record
    # did on deployment-host: checksum validity does not imply executability, so the
    # checksum is proven here and the mode is set separately.
    ( cd "$BUNDLE" && sha256sum -c SHA256SUMS ) || fail "bundle SHA256SUMS does not verify on this host"
    install -m 0755 -o root -g root "$BUNDLE/dg" "$BINARY" || fail "cannot install the gateway binary"
    "$BINARY" --version || fail "the installed gateway does not run"
    log "PASS bundle verified on the host and the gateway installed at $BINARY"

    log "step 2: create the dedicated unprivileged service user"
    useradd --system --no-create-home --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$SERVICE_USER" \
        || fail "cannot create the service user"
    log "PASS service user $SERVICE_USER created"

    log "step 3: create the data directory (service user, mode 0700)"
    install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DATA_DIR" || fail "cannot create the data directory"
    log "PASS data directory $DATA_DIR created"

    log "step 4: mint the owner credential once, into a mode-0600 file"
    install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" /etc/dg || fail "cannot create /etc/dg"
    as_user "$BINARY" init --data-dir "$DATA_DIR" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["owner_key"])' >"$KEY_FILE" \
        || fail "dg init did not produce an owner key"
    chown "$SERVICE_USER:$SERVICE_USER" "$KEY_FILE"
    chmod 0600 "$KEY_FILE"
    KEY_META="$(stat -c '%a %U:%G %s bytes' "$KEY_FILE")"
    case "$KEY_META" in
        "600 "*) : ;;
        *) fail "the owner key file is not mode 0600: $KEY_META" ;;
    esac
    # The bytes are never printed: only the file's metadata is reported.
    log "PASS owner credential minted into $KEY_FILE ($KEY_META, contents withheld)"

    log "step 5: install the service template with the placeholders substituted"
    [ -f "$BUNDLE/deploy/dg.service" ] \
        || fail "the bundle carries no deploy/dg.service: a clean host cannot be deployed from the bundle alone"
    sed -e "s|{{DG_USER}}|$SERVICE_USER|g" \
        -e "s|{{DG_BINARY}}|$BINARY|g" \
        -e "s|{{DG_DATA_DIR}}|$DATA_DIR|g" \
        -e "s|{{DG_BACKUP_ROOT}}|$BACKUP_ROOT|g" \
        -e "s|{{DG_LISTEN}}|$LISTEN|g" \
        "$BUNDLE/deploy/dg.service" >"$UNIT" || fail "cannot render the service template"
    # The rendered unit is a system configuration file: mode 0644, owned by
    # root. Leaving the mode to the shell's umask is how a unit ends up
    # world-writable -- systemd warns about exactly that and proceeds
    # anyway, which is not a state to ship to a host.
    chown root:root "$UNIT"
    chmod 0644 "$UNIT"
    if grep -q '{{' "$UNIT"; then
        fail "the rendered unit still contains placeholders: $(grep -o '{{[A-Z_]*}}' "$UNIT" | tr '\n' ' ')"
    fi
    systemd-analyze verify "$UNIT" || fail "systemd-analyze verify rejected the rendered unit"
    log "PASS unit rendered and verified (no placeholders left)"

    log "step 6: daemon-reload, enable and start"
    systemctl daemon-reload || fail "daemon-reload failed"
    systemctl enable dg.service >/dev/null || fail "cannot enable dg.service"
    systemctl start dg.service || fail "cannot start dg.service"
    wait_for_service || fail "dg.service did not become active"
    log "PASS dg.service is enabled and active"
}

# ---------------------------------------------------------------------------
# First-deploy assertions.
# ---------------------------------------------------------------------------

assert_first_deploy() {
    log "asserting the deployed service"
    assert_eq "service enabled" "enabled" "$(systemctl is-enabled dg.service)"
    assert_eq "service active" "active" "$(systemctl is-active dg.service)"

    LOCAL_HEALTH="$(curl -fsS "http://${LISTEN}/health/live")" || fail "the liveness endpoint did not answer"
    assert_eq "liveness body" '{"status":"ok"}' "$LOCAL_HEALTH"
    log "PASS unauthenticated liveness answers on loopback"

    log "seeding application data"
    dg_cli create "$DB" >/dev/null || fail "cannot create the test database"
    dg_cli query "$DB" "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL)" >/dev/null \
        || fail "cannot create the authors table"
    dg_cli query "$DB" "CREATE TABLE books (id INTEGER PRIMARY KEY, author_id INTEGER NOT NULL REFERENCES authors(id), title TEXT NOT NULL, price REAL)" >/dev/null \
        || fail "cannot create the books table"
    dg_cli query "$DB" "INSERT INTO authors (id, name) VALUES (1, 'octavia'), (2, 'ursula')" >/dev/null \
        || fail "cannot insert the authors rows"
    dg_cli query "$DB" "INSERT INTO books (id, author_id, title, price) VALUES (1, 1, 'kindred', 9.5), (2, 2, 'earthsea', 12.0)" >/dev/null \
        || fail "cannot insert the books rows"
    assert_rows "seeded authors" "$EXPECTED_AUTHORS" "$(query_rows "$DB" 'SELECT id, name FROM authors ORDER BY id')"
    assert_rows "seeded books" "$EXPECTED_BOOKS" "$(query_rows "$DB" 'SELECT id, author_id, title, price FROM books ORDER BY id')"

    log "asserting restart survival"
    systemctl restart dg.service || fail "restart failed"
    wait_for_service || fail "the service did not come back after a restart"
    assert_rows "authors after restart" "$EXPECTED_AUTHORS" "$(query_rows "$DB" 'SELECT id, name FROM authors ORDER BY id')"
    assert_rows "books after restart" "$EXPECTED_BOOKS" "$(query_rows "$DB" 'SELECT id, author_id, title, price FROM books ORDER BY id')"
    log "PASS a restart loses nothing and duplicates nothing"
}

# ---------------------------------------------------------------------------
# Backup publication and the off-host leg.
# ---------------------------------------------------------------------------

publish_backup() {
    log "publishing a backup set with the canonical producer"
    install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$BACKUP_ROOT" || fail "cannot create the backup root"
    as_user env DG_BIN="$BINARY" bash /usr/local/bin/backup.sh \
        --server "$SERVER" \
        --key-file "$KEY_FILE" \
        --data-dir "$DATA_DIR" \
        --backup-root "$BACKUP_ROOT" \
        --keep 3 || fail "the backup producer failed"
    SET="$(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort -r | head -1)"
    [ -n "$SET" ] || fail "the producer published no backup set"
    log "PASS backup set published: $SET"

    log "verifying the published set"
    "$BINARY" backup verify --backup-root "$BACKUP_ROOT" latest || fail "dg backup verify rejected the published set"
    log "PASS dg backup verify accepted the published set"

    log "performing the off-host leg (the copy that must survive the loss of this host)"
    install -d -m 0700 "$OFFHOST/backup-root"
    cp -r "$BACKUP_ROOT/$SET" "$OFFHOST/backup-root/$SET"
    cp "$KEY_FILE" "$OFFHOST/owner.key"
    chown -R root:root "$OFFHOST"
    chmod 0700 "$OFFHOST/backup-root" "$OFFHOST/backup-root/$SET" "$OFFHOST/owner.key"
    chmod 0600 "$OFFHOST/backup-root/$SET"/* "$OFFHOST/owner.key"
    # The off-host copy must be a complete, verifiable artifact in its own
    # right -- it is the only thing that will exist after this host is gone.
    "$BINARY" backup verify --backup-root "$OFFHOST/backup-root" latest \
        || fail "the off-host copy does not verify"
    OFFHOST_SHA="$(sha256sum "$OFFHOST/backup-root/$SET/manifest.json" | cut -d' ' -f1)"
    log "PASS off-host copy verified (manifest sha256 $OFFHOST_SHA)"
}

# ---------------------------------------------------------------------------
# The disaster-recovery half, on a replacement host.
# ---------------------------------------------------------------------------

restore_from_offhost() {
    log "checking the provisioned target is empty"
    # The empty target was provisioned during installation (documented step
    # 4: a local owner credential was minted against a fresh registry).
    # Path A now supersedes that registry with the restored one, which
    # carries the ORIGINAL credential hash back with it; the locally minted
    # credential is discarded on purpose, exactly as
    # docs/recovery-contract.md §3 steps 2-3 describe.
    [ -f "$DATA_DIR/metadata.sqlite" ] || fail "the target was never provisioned"
    LOCAL_KEY="$(stat -c '%a %U:%G' "$KEY_FILE")"
    log "PASS empty target provisioned (local owner credential $LOCAL_KEY, to be superseded by the restored registry)"

    log "verifying the off-host artifact on this host"
    [ -d "$OFFHOST/backup-root" ] || fail "no off-host artifact to restore from"
    "$BINARY" backup verify --backup-root "$OFFHOST/backup-root" latest \
        || fail "the off-host artifact does not verify on the replacement host"
    SET="$(find "$OFFHOST/backup-root" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort -r | head -1)"
    [ -n "$SET" ] || fail "the off-host artifact holds no backup set"
    log "PASS off-host artifact verifies here (set $SET)"

    log "restoring Path A (the registry travels with the data)"
    # Path A, docs/recovery-contract.md §3: the target server is stopped,
    # the verified registry copy replaces the main registry file, the
    # init-time WAL sidecars are dropped so the restored main file opens
    # clean, and each verified database file is placed into the data
    # directory. No import: the restored registry already holds live rows.
    systemctl stop dg.service >/dev/null 2>&1 || true
    rm -f "$DATA_DIR/metadata.sqlite-wal" "$DATA_DIR/metadata.sqlite-shm"
    cp "$OFFHOST/backup-root/$SET/metadata.sqlite.bak" "$DATA_DIR/metadata.sqlite" \
        || fail "cannot place the restored registry"
    while IFS= read -r db_file; do
        [ -n "$db_file" ] || continue
        cp "$OFFHOST/backup-root/$SET/$db_file" "$DATA_DIR/$db_file" \
            || fail "cannot place the restored database $db_file"
    done < <(python3 - "$OFFHOST/backup-root/$SET/manifest.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    manifest = json.load(stream)
for entry in manifest["databases"]:
    print(entry["file"])
PY
)
    chown -R "$SERVICE_USER:$SERVICE_USER" "$DATA_DIR"
    chmod 0600 "$DATA_DIR"/*.sqlite
    log "PASS restored registry and databases placed into the empty target"

    log "starting the service on the restored state"
    systemctl daemon-reload >/dev/null 2>&1 || true
    systemctl enable dg.service >/dev/null 2>&1 || true
    systemctl start dg.service || fail "the restored host does not start"
    wait_for_service || fail "the restored service did not become active"

    log "installing the owner credential from the operator's copy (the password-manager leg)"
    [ -f "$OFFHOST/owner.key" ] || fail "no owner credential survived the loss"
    install -m 0600 -o "$SERVICE_USER" -g "$SERVICE_USER" "$OFFHOST/owner.key" "$KEY_FILE" \
        || fail "cannot install the owner credential"
    log "PASS owner credential installed from the off-host copy"

    log "asserting the recovered state"
    # The strongest single assertion: the credential minted BEFORE the loss
    # authenticates against the registry restored AFTER it. That can only be
    # true if the registry itself came back, not just the data files.
    assert_eq "pre-loss credential authenticates" "ok" \
        "$(dg_cli list >/dev/null 2>&1 && echo ok || echo denied)"
    DATABASES="$(dg_cli list | python3 -c 'import json,sys; print(",".join(json.load(sys.stdin).get("databases", [])))')"
    assert_eq "recovered database list" "$DB" "$DATABASES"
    assert_rows "recovered authors" "$EXPECTED_AUTHORS" "$(query_rows "$DB" 'SELECT id, name FROM authors ORDER BY id')"
    assert_rows "recovered books" "$EXPECTED_BOOKS" "$(query_rows "$DB" 'SELECT id, author_id, title, price FROM books ORDER BY id')"

    log "checking integrity, foreign keys and protected tables on the restored file"
    python3 - "$DATA_DIR/$DB.sqlite" <<'PY' || fail "the restored database failed the integrity checks"
import sqlite3
import sys

db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
try:
    check, = db.execute("PRAGMA integrity_check").fetchone()
    if check != "ok":
        raise SystemExit(f"integrity_check returned {check!r}")
    bad, = db.execute("SELECT count(*) FROM pragma_foreign_key_check").fetchone()
    if bad:
        raise SystemExit(f"foreign_key_check reported {bad} violation(s)")
    rows = db.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))='_dg_' ORDER BY name"
    ).fetchall()
    smuggled = [name for (name,) in rows if name.lower() != "_dg_replay"]
    if smuggled:
        raise SystemExit(f"protected tables present: {smuggled}")
finally:
    db.close()
print("[phase] PASS integrity ok, no foreign-key violations, no protected tables")
PY

    log "proving the restored copy is independent of the artifact"
    ARTIFACT_SHA_BEFORE="$(sha256sum "$OFFHOST/backup-root/$SET/$DB.sqlite" | cut -d' ' -f1)"
    dg_cli query "$DB" "INSERT INTO authors (id, name) VALUES (3, 'after-the-loss')" >/dev/null \
        || fail "cannot write to the restored database"
    ARTIFACT_SHA_AFTER="$(sha256sum "$OFFHOST/backup-root/$SET/$DB.sqlite" | cut -d' ' -f1)"
    assert_eq "the off-host artifact is untouched by a write to the restored copy" \
        "$ARTIFACT_SHA_BEFORE" "$ARTIFACT_SHA_AFTER"
    assert_rows "the write landed in the restored copy" '[[1,"octavia"],[2,"ursula"],[3,"after-the-loss"]]' \
        "$(query_rows "$DB" 'SELECT id, name FROM authors ORDER BY id')"

    log "proving the negative case: a corrupt artifact publishes nothing"
    head -c 4096 /dev/urandom >/tmp/corrupt.sqlite
    if dg_cli import corrupt_case --from /tmp/corrupt.sqlite >/dev/null 2>&1; then
        fail "a corrupt artifact was accepted: the restore path published a corrupt database"
    fi
    AFTER_IMPORT="$(dg_cli list | python3 -c 'import json,sys; print(",".join(json.load(sys.stdin).get("databases", [])))')"
    assert_eq "the corrupt import published nothing" "$DB" "$AFTER_IMPORT"
    rm -f /tmp/corrupt.sqlite
    log "PASS corrupt input rejected and no database published"
}

case "$MODE" in
    deploy)
        log "=== FIRST DEPLOY on a clean host ==="
        assert_host_is_clean
        install_from_bundle
        assert_first_deploy
        publish_backup
        log "=== FIRST DEPLOY COMPLETE ==="
        ;;
    restore)
        log "=== DISASTER RECOVERY on a replacement host ==="
        assert_host_is_clean
        install_from_bundle
        restore_from_offhost
        log "=== DISASTER RECOVERY COMPLETE ==="
        ;;
    *)
        echo "[phase] FAIL: unknown mode '$MODE' (expected 'deploy' or 'restore')" >&2
        exit 2
        ;;
esac
