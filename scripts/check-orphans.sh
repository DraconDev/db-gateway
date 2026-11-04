#!/usr/bin/env bash
# Fail when a `dg serve` or a temp directory outlived the run that started it.
#
# Why this exists: on 2026-10-22 the maintainer's box was carrying six leaked
# `dg serve` processes -- the oldest 1 day 8 hours old -- every one of them
# reparented to systemd with its data directory already deleted, plus 277 MB
# of scratch directories under /tmp. None of them came from a bug in the
# gateway's own logic; they came from launchers that died without reaping
# their children (a SIGKILLed test binary, a torn-down audit harness) and a
# server that had no reason to notice. `dg serve` now arms PR_SET_PDEATHSIG,
# and this script is the gate that proves the class stays fixed: a future
# change that reintroduces a survivor fails here instead of quietly filling
# the disk and the process table for a week.
#
# Usage:
#   scripts/check-orphans.sh              # scan for live orphans and residue
#   scripts/check-orphans.sh --baseline F # ignore processes/dirs recorded in F
#
# Exit: 0 clean, 1 residue found, 2 usage/environment problem.
#
# The script never kills anything: reporting is the contract, and an operator
# decides. It also never inspects process memory or data directories, only the
# process table and the temp-dir listing.
set -uo pipefail

PROG="check-orphans"
BASELINE=""
KILL_AFTER_SECS="${DG_ORPHAN_MIN_AGE_SECS:-0}"

# Temp roots that only a test/audit/agent run creates. Matched by name prefix,
# never by deleting: this script reports, it does not clean.
RESIDUE_GLOBS=(
  "/tmp/dg-*"
  "/tmp/dgaudit*"
  "/tmp/aud2*"
  "/tmp/steady-*"
)

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --baseline) BASELINE="${2:-}"; shift 2 ;;
    --baseline=*) BASELINE="${1#*=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "$PROG: unknown argument: $1" >&2; exit 2 ;;
  esac
done

fail=0
report() {
  fail=1
  printf '%s: %s\n' "$PROG" "$1" >&2
}

# ---- live servers -----------------------------------------------------------
#
# A `dg serve` whose parent is PID 1 (or a systemd user manager) has been
# reparented: whoever started it is gone and nothing will ever reap it. That
# is the exact shape of the six leaks. A server still parented to a live
# process is that process's business, not ours.
mapfile -t SERVERS < <(pgrep -f '(^|/)dg serve( |$)' 2>/dev/null || true)

orphans=()
for pid in "${SERVERS[@]}"; do
  [ -n "$pid" ] || continue
  ppid="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')"
  [ -n "$ppid" ] || continue
  # 1 = init, and systemd's user manager on this box adopts orphans too.
  parent_cmd="$(ps -o comm= -p "$ppid" 2>/dev/null | tr -d ' ')"
  if [ "$ppid" = "1" ] || [ "$parent_cmd" = "systemd" ]; then
    age_secs="$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ')"
    age_secs="${age_secs:-0}"
    if [ "$age_secs" -ge "$KILL_AFTER_SECS" ] 2>/dev/null; then
      orphans+=("$pid")
      cmd="$(ps -o cmd= -p "$pid" 2>/dev/null)"
      report "orphaned dg serve: pid $pid, ppid $ppid ($parent_cmd), age ${age_secs}s -- $cmd"
    fi
  fi
done

if [ -n "$BASELINE" ] && [ -s "$BASELINE" ]; then
  # A baseline lets the caller run a suite that legitimately leaves one
  # already-known survivor in place; it never suppresses a NEW orphan.
  kept=()
  for pid in "${orphans[@]:-}"; do
    [ -n "$pid" ] || continue
    grep -qx "$pid" "$BASELINE" || kept+=("$pid")
  done
  if [ "${#kept[@]}" -eq 0 ]; then
    fail=0
  fi
fi

# ---- temp residue -----------------------------------------------------------
#
# Directories under /tmp that a run should have removed. Reported with their
# size so an operator can see whether it is 4 KiB or 277 MB before deciding.
shopt -s nullglob
residue=()
for glob in "${RESIDUE_GLOBS[@]}"; do
  for path in $glob; do
    [ -d "$path" ] || continue
    residue+=("$path")
  done
done
shopt -u nullglob

if [ "${#residue[@]}" -gt 0 ]; then
  total="$(du -csh "${residue[@]}" 2>/dev/null | tail -1 | cut -f1)"
  for path in "${residue[@]}"; do
    report "temp residue: $path ($(du -sh "$path" 2>/dev/null | cut -f1))"
  done
  report "temp residue totals $total across ${#residue[@]} director$([ "${#residue[@]}" -eq 1 ] && echo y || echo ies)"
fi

# ---- the gateway's own data-dir leftovers -----------------------------------
#
# `dg init` then a crash leaves `state/dg` or similar behind in the repo. The
# tracked tree must not gain one, so this catches the case where a test ran
# against the working directory instead of a temp dir.
if git rev-parse --git-dir >/dev/null 2>&1; then
  while IFS= read -r stray; do
    [ -n "$stray" ] || continue
    report "untracked gateway state in the working tree: $stray"
  done < <(git status --porcelain --untracked-files=all 2>/dev/null \
            | awk '$1=="??" && ($2 ~ /(^|\/)data\/$/ || $2 ~ /(^|\/)state\/dg/) {print $2}')
fi

if [ "$fail" -eq 0 ]; then
  echo "$PROG: clean (no orphaned dg serve, no temp residue)"
fi
exit "$fail"
