#!/usr/bin/env bash
# Publication gate: is this repository safe to make public?
#
# WHY THIS EXISTS. db-gateway was developed as a private repo, and the
# private-ness was doing the work of a control that no tool enforced.
# Two real leaks were found while preparing it for publication:
#
#   * A decoded pi-session transcript (the .md twin of a file the
#     2026-09-21 audit had already untracked) was still in the index.
#     It carried the operator's email address, their home directory
#     path, a public server address and production database names.
#     The earlier audit had swept the .html extension only, so the .md
#     twin walked straight past it.
#   * The managed .gitignore block un-ignores .dracon/data/ so the
#     owner PUBLIC key can be tracked. That re-includes everything else
#     under it, so an age private key (.age) or a plaintext age
#     identity (.agekey) dropped in .dracon/data/keys/ was committable.
#
# "Careful before publishing" is not a property any operator holds at
# the moment they click the visibility button. This script is: it runs
# every check in one command, before the click, and it fails loudly
# rather than listing suggestions.
#
# WHAT IT CHECKS
#
#   BLOCKERS (exit 1). A public repo with any of these is a leak:
#     * credential-shaped material in the tracked tree (delegated to
#       scripts/secret-scan.sh, so the matcher and its false-positive
#       policy stay in exactly one place);
#     * credential-shaped material anywhere in the history -- a secret
#       added and later removed is still in the clone (full mode only);
#     * credential files tracked by extension (.key .pem .age .agekey
#       .env);
#     * session/transcript exhaust tracked, or ever committed;
#     * build or runtime output tracked (target/, dist/, state/,
#       node_modules/, .cargo/target*/);
#     * a tracked file large enough to be a blob rather than source
#       (256 KiB; the largest legitimate tracked file in this repo is
#       184 KiB, so the threshold has headroom and still catches the
#       1.7 MiB transcript).
#
#   ADVISORIES (reported, not failed). These are the things a public
#   reader learns about the operator and the deployment. They are the
#   owner's call, not a leak:
#     * the operator's home directory path in tracked files;
#     * personal (non-role) email addresses;
#     * public IP addresses of the deployment hosts;
#     * cloud account identifiers.
#
# MODES
#
#   scripts/public-readiness.sh            full: tree + history.
#   scripts/public-readiness.sh --tree     tree only.
#
# CI runs --tree: the tracked tree is what every future commit changes,
# so it is the thing a gate can actually keep green. The history checks
# are for the moment before publication, because history cannot be
# un-published -- it can only be rewritten, and rewriting is the
# owner's decision, not a CI step's.
#
# Exit codes: 0 = ready to publish, 1 = a blocker was found,
#             2 = the gate could not run.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO" || exit 2

TREE_ONLY=0
case "${1:-}" in
    "") ;;
    --tree) TREE_ONLY=1 ;;
    *)
        echo "public-readiness: unknown argument '${1}'" >&2
        echo "usage: scripts/public-readiness.sh [--tree]" >&2
        exit 2
        ;;
esac

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo "public-readiness: not a git checkout -- run from the repo" >&2
    exit 2
fi

BLOCKERS=0
ADVISORIES=0

section() {
    printf '\n== %s ==\n' "$1"
}

block() {
    # block <detail>: a blocker is a leak. The detail names the file or
    # the count; it never prints a credential value.
    printf 'BLOCK  %s\n' "$1"
    BLOCKERS=$((BLOCKERS + 1))
}

advise() {
    printf 'ADVICE %s\n' "$1"
    ADVISORIES=$((ADVISORIES + 1))
}

ok() {
    printf 'ok     %s\n' "$1"
}

# ---------------------------------------------------------------- tracked set
mapfile -t TRACKED < <(git ls-files)
if [ "${#TRACKED[@]}" -eq 0 ]; then
    echo "public-readiness: git ls-files returned no files -- run from the repo checkout" >&2
    exit 2
fi

# ------------------------------------------------------------- session exhaust
section "session and transcript exhaust"

# The names the managed .gitignore block un-ignores, and therefore the
# names that reach the index unless something below the block overrides
# them. Checked against the INDEX, not the working tree: a file that is
# on disk but ignored is not published, and a file that is tracked but
# deleted is still published.
SESSION_PATTERN='^(pi-session-|conversation-|rollout-|trajectory-)[^/]*$'
SESSION_HITS="$(git ls-files | grep -E "$SESSION_PATTERN" || true)"
if [ -n "$SESSION_HITS" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] && block "session exhaust is tracked: $f"
    done <<< "$SESSION_HITS"
else
    ok "no session exhaust is tracked"
fi

if [ "$TREE_ONLY" -eq 0 ]; then
    # Ever committed, not merely tracked now: a removed file is still in
    # every clone. --diff-filter=A limits this to the commit that added
    # each path.
    HISTORY_SESSIONS="$(git log --all --diff-filter=A --name-only --pretty=format: \
        -- 'pi-session-*' 'conversation-*' 'rollout-*' 'trajectory-*' 2>/dev/null \
        | sed '/^$/d' | sort -u || true)"
    if [ -n "$HISTORY_SESSIONS" ]; then
        n="$(printf '%s\n' "$HISTORY_SESSIONS" | wc -l | tr -d ' ')"
        block "$n session/transcript path(s) were committed and are still in the history:"
        printf '%s\n' "$HISTORY_SESSIONS" | sed 's/^/         /'
        printf '       The fix is a history rewrite (git-filter-repo), which changes every\n'
        printf '       commit id and needs a force-push to both remotes. That is the\n'
        printf '       owner'"'"'s decision: this gate reports it, it does not perform it.\n'
    else
        ok "no session exhaust was ever committed"
    fi
fi

# ------------------------------------------------------------ credential files
section "credential files by extension"

CRED_EXT_PATTERN='(\.key|\.pem|\.age|\.agekey|\.p12|\.pfx|\.jks|\.keystore)$|(^|/)\.env($|\.)'
CRED_HITS="$(git ls-files | grep -E "$CRED_EXT_PATTERN" || true)"
if [ -n "$CRED_HITS" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] && block "credential-shaped file is tracked: $f"
    done <<< "$CRED_HITS"
else
    ok "no credential file is tracked"
fi

# --------------------------------------------------------- build / runtime output
section "build and runtime output"

BUILD_PATTERN='(^|/)(target|dist|state|node_modules|__pycache__|\.venv)/|(^|/)\.cargo/target[^/]*/'
BUILD_HITS="$(git ls-files | grep -E "$BUILD_PATTERN" || true)"
if [ -n "$BUILD_HITS" ]; then
    n="$(printf '%s\n' "$BUILD_HITS" | wc -l | tr -d ' ')"
    block "$n tracked path(s) are build or runtime output:"
    printf '%s\n' "$BUILD_HITS" | sed 's/^/         /'
else
    ok "no build or runtime output is tracked"
fi

# ------------------------------------------------------------- oversized blobs
section "oversized tracked files"

# 256 KiB. The largest legitimate tracked file in this repo is a 184 KiB
# screenshot, so the threshold has headroom and still catches the 1.7 MiB
# transcript that slipped through the .html-only sweep.
OVERSIZED="$(git ls-files -z | xargs -0 du -k 2>/dev/null | awk '$1 > 256 {print $2" ("int($1/1024)" MiB)"}' | sort -rn || true)"
if [ -n "$OVERSIZED" ]; then
    while IFS= read -r f; do
        [ -n "$f" ] && block "tracked file is larger than 256 KiB: $f"
    done <<< "$OVERSIZED"
else
    ok "no tracked file is larger than 256 KiB"
fi

# ------------------------------------------------------ credential shapes
section "credential-shaped material"

if ! SCAN_OUT="$("$REPO/scripts/secret-scan.sh" 2>&1)"; then
    block "credential-shaped material in the tracked tree (see below)"
    printf '%s\n' "$SCAN_OUT" | sed 's/^/         /'
else
    ok "$SCAN_OUT"
fi

if [ "$TREE_ONLY" -eq 0 ]; then
    # The same matcher, over every commit. A secret that was added and
    # later removed is still in every clone of the repo, so the tree scan
    # alone would report a clean repo that has already leaked.
    TMPHIST="$(mktemp -t public-readiness-hist.XXXXXX)"
    trap 'rm -f "$TMPHIST"' EXIT
    if git log -p --all > "$TMPHIST" 2>/dev/null; then
        if ! HIST_OUT="$("$REPO/scripts/secret-scan.sh" "$TMPHIST" 2>&1)"; then
            block "credential-shaped material in the history (see below)"
            printf '%s\n' "$HIST_OUT" | sed 's/^/         /'
            printf '       The line numbers are into the concatenated diff, not a file.\n'
            printf '       Locate the commit with: git log -p --all | grep -n "<pattern>"\n'
        else
            ok "history is clean of credential-shaped material"
        fi
    else
        printf 'ADVICE could not read the history; the history checks were skipped\n'
        ADVISORIES=$((ADVISORIES + 1))
    fi
fi

# ------------------------------------------------------------ identity advisories
section "identity advisories (the owner's call, not a leak)"

# These are the facts a public reader learns about the operator and the
# deployment. They are reported so the decision to publish them is
# deliberate: the legal-hold argument for keeping a repo private does not
# extend to "the operator's home directory is on the internet".
ADVISORY_OUT="$(python3 - <<'PY'
import re
import subprocess

files = subprocess.run(["git", "ls-files"], capture_output=True, text=True,
                       check=False).stdout.split()

PATTERNS = [
    ("home-path", re.compile(r"/home/[A-Za-z0-9._-]+")),
    ("personal-email", re.compile(r"\b[A-Za-z0-9._%+-]+@(?!example\.|invalid\.|turso\.tech|"
                                  r"tty1\.service|1000\.service)[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")),
    ("public-ip", re.compile(r"\b(?:(?:25[0-5]|2[0-4][0-9]|1?[0-9]{1,2})\.){3}"
                             r"(?:25[0-5]|2[0-4][0-9]|1?[0-9]{1,2})\b")),
    ("cloud-account", re.compile(r"\baccount\s+\d{9,12}\b")),
]

# These hosts are the documented examples; naming them teaches nothing.
PUBLIC_IP_ALLOW = {"0.0.0.0", "127.0.0.1", "127.0.0.2", "10.0.0.1",
                   "10.66.0.1", "192.0.2.1", "255.255.255.255"}

hits = {}
for rel in files:
    try:
        with open(rel, "rb") as fh:
            raw = fh.read()
    except OSError:
        continue
    if b"\0" in raw[:4096]:
        continue
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        continue
    for kind, pattern in PATTERNS:
        for m in pattern.finditer(text):
            value = m.group(0)
            if kind == "public-ip" and value in PUBLIC_IP_ALLOW:
                continue
            if kind == "home-path" and not re.match(r"/home/[A-Za-z0-9._-]+", value):
                continue
            hits.setdefault(kind, {}).setdefault(rel, 0)
            hits[kind][rel] += 1

for kind in ("home-path", "personal-email", "public-ip", "cloud-account"):
    files_hit = hits.get(kind, {})
    if not files_hit:
        continue
    total = sum(files_hit.values())
    print(f"{kind}\t{total}\t{len(files_hit)}")
PY
)"

if [ -n "$ADVISORY_OUT" ]; then
    while IFS=$'\t' read -r kind total files_hit; do
        [ -n "$kind" ] || continue
        advise "$kind appears in $total place(s) across $files_hit tracked file(s)"
    done <<< "$ADVISORY_OUT"
else
    ok "no identity disclosure found in the tracked tree"
fi

# ------------------------------------------------------------------ repo state
section "repository state"

DIRTY="$(git status --porcelain --untracked-files=no)"
if [ -n "$DIRTY" ]; then
    advise "the working tree has uncommitted changes; what you publish is not what was checked"
else
    ok "the working tree is clean"
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
UPSTREAM="$(git rev-parse --abbrev-ref '@{upstream}' 2>/dev/null || echo none)"
if [ "$UPSTREAM" = "none" ]; then
    advise "branch '${BRANCH}' has no upstream; nothing is set up to be pushed"
elif git rev-list --count "${UPSTREAM}..HEAD" >/dev/null 2>&1; then
    AHEAD="$(git rev-list --count "${UPSTREAM}..HEAD" 2>/dev/null || echo 0)"
    if [ "$AHEAD" -gt 0 ]; then
        advise "$AHEAD commit(s) on '${BRANCH}' are not on '${UPSTREAM}' yet"
    else
        ok "'${BRANCH}' is level with '${UPSTREAM}'"
    fi
else
    advise "cannot compare '${BRANCH}' with '${UPSTREAM}'"
fi

printf '\n'
if [ "$BLOCKERS" -gt 0 ]; then
    printf 'public-readiness: NOT READY -- %d blocker(s), %d advisory/advisories\n' \
        "$BLOCKERS" "$ADVISORIES"
    printf 'public-readiness: do not publish until the blockers are cleared.\n'
    exit 1
fi

printf 'public-readiness: READY -- no blockers, %d advisory/advisories\n' "$ADVISORIES"
exit 0
