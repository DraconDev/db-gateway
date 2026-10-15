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
# process: the remote side copies the secret line into a mode-0600 file in
# /run (tmpfs, gone on reboot, not one of the scanned roots so it cannot
# match itself), greps with `grep -ralFf PATTERN_FILE` -- the pattern
# travels from the FILE, never argv -- removes the pattern file, and
# prints only the names of any files that matched. Neither the local
# shell, the ssh transport, nor sudo's journal entry ever sees the secret.
#
# HOW IT IS BOUNDED. A byte-for-byte read of every file python-side took
# >5 minutes on micro2 (~6GB, ~380k files under the scanned roots) and
# could not finish inside an operator's timeout. grep with a fixed-string
# pattern file scans the same bytes in about five minutes with no python
# per-file overhead, and short-circuits per file. Exit 0 means no file
# under the scanned roots contains the secret except the env file.
#
# Exit 0 when the secret appears in no file other than the env file.
# Exit 1, naming the leaking paths, otherwise.

set -euo pipefail
SELF="check-litestream-secret"

TARGET="${1:-${MICRO2_SSH:-micro2}}"
ENV_PATH="/etc/dg/litestream.env"

command -v ssh >/dev/null || { echo "[$SELF] ssh not on PATH" >&2; exit 1; }

# The whole remote check is a single-quoted heredoc piped to a root bash
# interpreter on the target. The secret never appears in the local
# process, in argv (so never in sudo's COMMAND= journal line), or in any
# file outside /run on the target.
set +e
OUTPUT="$(ssh -o BatchMode=yes -o ConnectTimeout=15 -T -- "$TARGET" sudo -n bash -s <<'BASH' 2>&1
set -u
ENV="/etc/dg/litestream.env"
PAT="/run/litestream-check.pat.$$"
trap 'rm -f "$PAT"' EXIT
sed -n 's/^LITESTREAM_S3_SECRET_ACCESS_KEY=//p' "$ENV" > "$PAT" || exit 2
chmod 0600 "$PAT"
[ -s "$PAT" ] || { echo "$ENV has no secret line" >&2; exit 2; }
# Scanned roots: the trees that can plausibly hold a hand-copied
# credential. /var/log is excluded on purpose (a journal leak is removed
# by journald restart + vacuum, documented in the ledger, and reading it
# is prohibitively slow); /proc and /sys are virtual; /usr and /nix are
# package/build stores, implausible for a hand-copied credential.
ROOTS="/etc /home /root /opt /srv /var/lib /var/backups /var/tmp"
# Capture the FILTERED list and test IT for emptiness. An earlier
# revision tested PIPESTATUS[0] of the first grep -- but that grep exits
# 0 because the env file itself legitimately matches, so the check cried
# leak while printing no paths. The verdict is: does anything survive
# the env-file (and pattern-file) exclusion?
LEAKS="$(grep -ralFf "$PAT" $ROOTS 2>/dev/null | grep -v "^${ENV}$\|^${PAT}$")"
if [ -n "$LEAKS" ]; then
  echo "SECRET LEAK beyond $ENV: $LEAKS" >&2
  exit 1
fi
echo "only copy verified: $ENV (roots: $ROOTS)"
exit 0
BASH
)"
RC=$?
set -e

if [ "$RC" -ne 0 ] || [ -z "$OUTPUT" ]; then
  echo "[$SELF] FAIL: the replication credential is not confined to ${ENV_PATH} on ${TARGET}" >&2
  echo "[$SELF]       ${OUTPUT:-<no output>}" >&2
  exit 1
fi

echo "[$SELF] ${OUTPUT} (target: ${TARGET})"
