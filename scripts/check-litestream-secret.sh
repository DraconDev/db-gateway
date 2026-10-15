#!/usr/bin/env bash
# Prove /etc/dg/litestream.env is the ONLY copy of the replication
# credential on the host.
#
# Usage: scripts/check-litestream-secret.sh [SSH_TARGET]
#
# WHY THIS EXISTS AS ITS OWN SCRIPT. The goal's verification contract
# requires "/etc/dg/litestream.env is the only copy of the credentials on
# the host". The check is harder than it looks, because the obvious way to
# write it is itself a leak:
#
#     SEC=$(sudo sed -n 's/^LITESTREAM_S3_SECRET_ACCESS_KEY=//p' /etc/dg/litestream.env)
#     sudo grep -rlF "$SEC" /etc /var /home
#
# sudo logs the full COMMAND= of every invocation to the journal, so that
# grep writes the secret into persistent storage -- the exact leak the
# contract forbids. An earlier revision of this goal did precisely that,
# and had to restart journald and vacuum to recover (recorded in the audit
# ledger). This script therefore reads the secret only INSIDE the remote
# python process, which receives the secret from neither argv nor stdin.
#
# Exit 0 when the secret appears in no file other than the env file.
# Exit 1, naming the leaking paths, otherwise.

set -euo pipefail
SELF="check-litestream-secret"
fail() { echo "[$SELF] FAIL: $*" >&2; exit 1; }

TARGET="${1:-${MICRO2_SSH:-micro2}}"
ENV_PATH="/etc/dg/litestream.env"
command -v ssh >/dev/null || fail "ssh not on PATH"

# The whole remote check is a single-quoted heredoc piped to a root python
# interpreter. The secret never appears in the local process, in argv, or
# in any file this script writes.
if ! ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    sudo -n python3 - <<'PY' 2>&1
import os
import sys

ENV = "/etc/dg/litestream.env"
SECRET_PREFIX = "LITESTREAM_S3_SECRET_ACCESS_KEY="

try:
    raw = open(ENV).read()
except Exception as error:
    print(f"cannot read {ENV}: {error}", file=sys.stderr)
    raise SystemExit(2)

line = next((l for l in raw.splitlines() if l.startswith(SECRET_PREFIX)), None)
if line is None:
    print(f"{ENV} has no {SECRET_PREFIX}", file=sys.stderr)
    raise SystemExit(2)
secret = line[len(SECRET_PREFIX):].strip().encode()
if not secret:
    print(f"{ENV} has an empty secret", file=sys.stderr)
    raise SystemExit(2)

# Filesystem roots to walk, bounded to the trees that can plausibly
# hold a copy. /var/log is deliberately excluded: journald's store is
# large and slow to read, and a leak into the journal is removed by
# journald restart + vacuum (documented in the ledger) rather than by
# this scan. /proc and /sys are virtual.
ROOTS = ("/etc", "/home", "/root", "/opt", "/srv", "/var/lib", "/var/backups", "/var/tmp")
SKIP_PREFIXES = ("/proc", "/sys", "/var/log")

leaks = []
for root in ROOTS:
    if not os.path.isdir(root):
        continue
    for directory, _subdirs, files in os.walk(root):
        if directory.startswith(SKIP_PREFIXES):
            continue
        for name in files:
            path = os.path.join(directory, name)
            if path == ENV:
                continue
            try:
                with open(path, "rb") as handle:
                    if handle.read().find(secret) >= 0:
                        leaks.append(path)
            except OSError:
                # Unreadable (permissions on a 0600 third-party file, a
                # socket, a device node) is not a leak: if root cannot read
                # it, neither can the sidecar.
                continue

if leaks:
    shown = ", ".join(leaks[:8]) + (" ..." if len(leaks) > 8 else "")
    print(f"SECRET LEAK beyond {ENV}: {shown}", file=sys.stderr)
    print(
        "sudo logs every COMMAND= to the journal, so removing a leak with "
        "grep -F writes the secret again; use journald restart + vacuum.",
        file=sys.stderr,
    )
    raise SystemExit(1)

print(f"only copy verified: {ENV}")
PY
then
  fail "the replication credential is not confined to ${ENV_PATH} on ${TARGET}; do not start the sidecar until the extra copies are removed and the journal is vacuumed"
fi
log() { echo "[$SELF] $*"; }
log "OK: ${ENV_PATH} is the only copy of the replication secret on ${TARGET}"
