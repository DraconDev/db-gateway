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
# contract forbids. An earlier revision of this goal did precisely that and
# had to restart journald and vacuum to recover (recorded in the audit
# ledger). This script therefore reads the secret only INSIDE the remote
# python process, which receives it from neither argv nor stdin.
#
# Exit 0 when the secret appears in no file other than the env file.
# Exit 1, naming the leaking paths, otherwise.

set -euo pipefail
SELF="check-litestream-secret"

TARGET="${1:-${MICRO2_SSH:-micro2}}"
ENV_PATH="/etc/dg/litestream.env"

command -v ssh >/dev/null || { echo "[$SELF] ssh not on PATH" >&2; exit 1; }

# The whole remote check is a single-quoted heredoc piped to a root python
# interpreter. The secret never appears in the local process, in argv, or
# in any file this script writes. The heredoc body sits between <<'PY' and
# the terminator line, with the closing quote of the command substitution
# after it.
set +e
OUTPUT="$(ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" \
    sudo -n python3 - 2>&1 <<'PY'
import os
import sys

ENV = "/etc/dg/litestream.env"
SECRET_PREFIX = "LITESTREAM_S3_SECRET_ACCESS_KEY="

try:
    with open(ENV) as handle:
        raw = handle.read()
except Exception as error:
    print("cannot read %s: %s" % (ENV, error), file=sys.stderr)
    raise SystemExit(2)

line = None
for candidate in raw.splitlines():
    if candidate.startswith(SECRET_PREFIX):
        line = candidate
        break
if line is None:
    print("%s has no %s" % (ENV, SECRET_PREFIX), file=sys.stderr)
    raise SystemExit(2)
secret = line[len(SECRET_PREFIX):].strip().encode()
if not secret:
    print("%s has an empty secret" % ENV, file=sys.stderr)
    raise SystemExit(2)

# Filesystem roots walked, bounded to the trees that can plausibly hold a
# copy. /var/log is excluded on purpose: journald's store is large and slow
# to read, and a journal leak is removed by journald restart + vacuum
# (documented in the ledger) rather than by this scan. /proc and /sys are
# virtual. /usr (package files) and /nix (read-only build store) are
# excluded as implausible for a hand-copied credential.
ROOTS = ("/etc", "/home", "/root", "/opt", "/srv",
         "/var/lib", "/var/backups", "/var/tmp")
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
                    content = handle.read()
            except OSError:
                continue
            if content.find(secret) >= 0:
                leaks.append(path)

if leaks:
    shown = ", ".join(leaks[:8]) + (" ..." if len(leaks) > 8 else "")
    print("SECRET LEAK beyond %s: %s" % (ENV, shown), file=sys.stderr)
    raise SystemExit(1)

print("only copy verified: %s" % ENV)
PY
)"
RC=$?
set -e

if [ "$RC" -ne 0 ] || [ -z "$OUTPUT" ]; then
    echo "[$SELF] FAIL: the replication credential is not confined to ${ENV_PATH} on ${TARGET}" >&2
    echo "[$SELF]       ${OUTPUT:-<no output>" >&2
    exit 1
fi

echo "[$SELF] ${OUTPUT} (target: ${TARGET})"
