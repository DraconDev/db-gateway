#!/usr/bin/env bash
# Check that the two halves of the snapshot cadence agree.
#
# Usage: scripts/check-unit-cadence.sh [repo-root]
#
# The gateway's backup-health alarm and the daily digest's staleness
# verdict are judged by the SAME number, but the number is written twice:
# once as `--backup-cadence-secs` in deploy/dg.service and once as
# `--cadence-secs` in deploy/dg-backup-digest.service. Nothing compared
# them, so a host that rendered one half with a different value reported
# every database overdue in one half and fresh in the other — behind a
# green deploy gate. The deployment guide calls them "the same number";
# this check is what makes that sentence true.
#
# Read-only. Exit status: 0 when the two agree, 1 otherwise.
#
# The check lives on its own so it can be run against a disposable tree
# (tests/unit_cadence.rs) without the rest of the deploy gate.

set -euo pipefail

ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
SERVE="$ROOT/deploy/dg.service"
DIGEST="$ROOT/deploy/dg-backup-digest.service"

fail() { echo "[check-unit-cadence] FAIL: $*" >&2; exit 1; }

[ -f "$SERVE" ] || fail "missing $SERVE"
[ -f "$DIGEST" ] || fail "missing $DIGEST"

# The extraction tolerates "no match": with `set -o pipefail` a failing
# grep would abort the script before the explicit check below could name
# the flag that is missing.
serve_cadence="$(grep -oE -- '--backup-cadence-secs [0-9]+' "$SERVE" | head -1 | awk '{print $NF}' || true)"
digest_cadence="$(grep -oE -- '(^|[[:space:]])--cadence-secs [0-9]+' "$DIGEST" | head -1 | awk '{print $NF}' || true)"

[ -n "$serve_cadence" ] || fail "dg.service must pass --backup-cadence-secs"
[ -n "$digest_cadence" ] || fail "dg-backup-digest.service must pass --cadence-secs"
[ "$serve_cadence" = "$digest_cadence" ] \
    || fail "the snapshot cadence disagrees: dg.service says ${serve_cadence}s, dg-backup-digest.service says ${digest_cadence}s"

exit 0
