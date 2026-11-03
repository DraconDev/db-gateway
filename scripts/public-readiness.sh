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

# Two different directories, and conflating them is a bug: SELFDIR holds
# the scanner this gate delegates to, ROOT is the repository being
# audited. They are the same in the normal case (`./scripts/
public-readiness.sh` from the repo root) and different when the gate is
# run against a fixture, an unpacked release bundle, or another checkout
# -- which is what the contract test in tests/public_readiness.rs does.
#
# The positional root follows the same convention as
# scripts/check-doc-caps.py and scripts/check-doc-index.py, so every gate
# in this repo can be pointed at a fixture tree.
SELFDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TREE_ONLY=0
ROOT=""
for arg in "$@"; do
    case "$arg" in
        --tree) TREE_ONLY=1 ;;
        -h|--help)
            sed -n '3,80p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*)
            echo "public-readiness: unknown option '$arg'" >&2
            echo "usage: scripts/public-readiness.sh [--tree] [repo-root]" >&2
            exit 2
            ;;
        *)
            if [ -n "$ROOT" ]; then
                echo "public-readiness: more than one repo root given" >&2
                exit 2
            fi
            ROOT="$arg"
            ;;
    esac
done

if [ -z "$ROOT" ]; then
    ROOT="$SELFDIR"
fi
if [ ! -d "$ROOT" ]; then
    echo "public-readiness: no such directory: $ROOT" >&2
    exit 2
fi
cd "$ROOT" || exit 2
ROOT="$(pwd)"

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo "public-readiness: $ROOT is not a git checkout" >&2
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

# The scanner resolves a relative path against the CALLER's directory and
# then its own repository, so a relative name would silently be read from
# the wrong tree. Absolute paths remove the ambiguity.
TRACKED_ABS=()
for f in "${TRACKED[@]}"; do
    TRACKED_ABS+=("$ROOT/$f")
done

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

# The tracked paths are passed explicitly rather than letting the scanner
# list its own repository: this gate can be pointed at a fixture tree or
# another checkout, and a scanner that silently read the wrong repository
# would report that one clean. TRACKED holds absolute paths so a lookup
# cannot fall back to the caller's directory.
#
# Exit status is 0 clean, 1 findings, 2 could not run. Only 2 means "no
# answer", and it must not be reported as clean.
SCAN_OUT="$("$SELFDIR/scripts/secret-scan.sh" "${TRACKED_ABS[@]}" 2>&1)"
SCAN_RC=$?
if [ "$SCAN_RC" -eq 1 ]; then
    block "credential-shaped material in the tracked tree (see below)"
    printf '%s\n' "$SCAN_OUT" | sed 's/^/         /'
elif [ "$SCAN_RC" -ne 0 ]; then
    block "the tree credential scan could not run; the tree is UNKNOWN, not clean"
    printf '%s\n' "$SCAN_OUT" | sed 's/^/         /'
else
    ok "$SCAN_OUT"
fi

if [ "$TREE_ONLY" -eq 0 ]; then
    # Encrypted secret material, checked before the generic scan because
    # it needs a different message. The .gitignore documents the
    # dracon-warden policy -- age-encrypted secrets are "intentionally
    # tracked in git for team collaboration" -- and that policy is sound
    # for a PRIVATE repo and stops being sound the moment the repo is
    # public. The ciphertext is safe against anyone without the age
    # identity, but publishing it puts the payload and the recipient key
    # in every clone forever: the owner's private key is the only thing
    # still standing between the internet and those values, and a key
    # that leaks in 2029 decrypts a commit published in 2026.
    #
    # The needle is assembled from two halves on purpose. Written out, the
    # literal would occur in this file, and `git log -S` would then find
    # THIS script in the history and report itself as a leak -- which is
    # exactly what happened on the first run. The gate's own path is
    # excluded as well, so a future edit that reintroduces the literal
    # cannot make the gate flag itself.
    WARDEN_MARK="DRACON""_SECRET"
    WARDEN_COMMITS="$(git log --all --format='%H' -S"$WARDEN_MARK" \
        -- . ':(exclude)scripts/public-readiness.sh' 2>/dev/null || true)"
    WARDEN_COUNT="$(git log --all -p -- . ':(exclude)scripts/public-readiness.sh' 2>/dev/null \
        | grep -oE "${WARDEN_MARK}:[A-Za-z0-9+/=]+" | sort -u | wc -l | tr -d ' ')"
    if [ -n "$WARDEN_COMMITS" ]; then
        block "$WARDEN_COUNT age-encrypted secret value(s) were committed in $(printf '%s\n' "$WARDEN_COMMITS" | wc -l | tr -d ' ') commit(s)"
        printf '       This is the dracon-warden scheme the managed .gitignore describes as\n'
        printf '       "intentionally tracked in git for team collaboration". That reasoning\n'
        printf '       holds because the repo is private; it does not survive publication.\n'
        printf '       Decide explicitly: rewrite them out of history, or publish them\n'
        printf '       knowing the age identity is the only remaining protection.\n'
        printf '       Commits:\n'
        git log --all --format='         %h %ad %s' --date=short -S"$WARDEN_MARK" \
            -- . ':(exclude)scripts/public-readiness.sh' 2>/dev/null | cut -c1-140
    else
        ok "no age-encrypted secret material in the history"
    fi
fi

if [ "$TREE_ONLY" -eq 0 ]; then
    # The same matcher as the tree scan, over every commit. A secret that
    # was added and later removed is still in every clone, so the tree
    # scan alone would call a repo clean that has already leaked.
    #
    # The matcher lives in scripts/secret-scan.sh and stays there: it has
    # its own tests, and a second copy here would drift. What this block
    # adds is provenance. Scanning a concatenated diff gives a line
    # number into the diff, which tells an operator nothing; writing each
    # commit's added lines to its own file lets the untouched scanner
    # report a file, and this block maps that file back to a commit and a
    # path. The line number is into the lines that commit ADDED, which is
    # the honest thing to say rather than pretending it is a file line.
    HIST_DIR="$(mktemp -d -t public-readiness-hist.XXXXXX)"
    trap 'rm -rf "$HIST_DIR"' EXIT

    # The manifest goes in a FILE, not a shell variable: it carries one
    # line per hunk of 2500 commits, and passing that back as argv is how
    # the first cut of this script hit "Argument list too long" and then
    # reported the history CLEAN because the scan had not run at all. A
    # safety gate whose failure mode is a false clean is worse than no
    # gate, so the scan's own output is what decides "clean" below.
    if ! python3 - "$HIST_DIR" <<'PY'
import re
import subprocess
import sys
from pathlib import Path

out = Path(sys.argv[1])
diff = subprocess.run(
    ["git", "log", "--all", "-p", "--no-color", "--format=commit %H"],
    capture_output=True, text=True, check=False,
).stdout

HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")

commit = ""
path = ""
frag = None          # list of (new-file line number, text) for one hunk
index = 0
manifest = []


def flush():
    global frag, index
    if frag:
        index += 1
        name = f"{index:05d}"
        # One added line per line: the scanner's line number is then an
        # offset into this fragment, and the manifest's `first` turns it
        # back into a line number of the real file at that commit.
        (out / name).write_text("".join(t for _, t in frag), encoding="utf-8")
        manifest.append(f"{name}\t{commit}\t{path}\t{frag[0][0]}")
    frag = None


for line in diff.splitlines(keepends=True):
    if line.startswith("commit "):
        flush()
        commit = line[len("commit "):].strip()
        path = ""
        continue
    if line.startswith("+++ "):
        flush()
        name = line[4:].strip()
        path = "" if name == "/dev/null" else (name[2:] if name[:2] in ("b/", "a/") else name)
        continue
    if line.startswith("--- ") or line.startswith("diff "):
        continue
    m = HUNK.match(line)
    if m:
        flush()
        frag = []
        newno = int(m.group(1))
        continue
    if frag is None:
        continue
    if line.startswith("+"):
        frag.append((newno, line[1:]))
        newno += 1
    elif line.startswith(" "):
        newno += 1
    elif line.startswith("\\"):
        pass
    elif line.startswith("-"):
        pass
    else:
        flush()

flush()
(out / ".manifest").write_text("\n".join(manifest) + ("\n" if manifest else ""),
                                 encoding="utf-8")
PY
    then
        advise "could not rebuild the history; the history scan was skipped"
    else
        FRAG_COUNT="$(wc -l < "$HIST_DIR/.manifest" | tr -d ' ')"
        if [ "$FRAG_COUNT" -eq 0 ]; then
            ok "history is clean of credential-shaped material (no text to scan)"
        else
            # Batched: 800 absolute paths per invocation keeps argv well
            # under the limit on every platform, and the scanner is run
            # once per batch rather than once per fragment.
            : > "$HIST_DIR/.scan"
            SCAN_RAN=1

            # The scanner's contract is 0 = clean, 1 = findings found,
            # 2 = could not run. Only 2 means "no answer"; treating 1 as a
            # failure would turn every real finding into "scan broken".
            scan_batch() {
                "$SELFDIR/scripts/secret-scan.sh" "$@" >> "$HIST_DIR/.scan" 2>&1
                rc=$?
                if [ "$rc" -ne 0 ] && [ "$rc" -ne 1 ]; then
                    SCAN_RAN=0
                fi
            }

            BATCH=()
            while IFS= read -r frag; do
                BATCH+=("$HIST_DIR/$frag")
                if [ "${#BATCH[@]}" -ge 800 ]; then
                    scan_batch "${BATCH[@]}"
                    BATCH=()
                fi
            done < <(cut -f1 "$HIST_DIR/.manifest")
            if [ "${#BATCH[@]}" -gt 0 ]; then
                scan_batch "${BATCH[@]}"
            fi

            if [ "$SCAN_RAN" -eq 0 ]; then
                block "the history credential scan could not run; the history is UNKNOWN, not clean"
                printf '       The scanner failed on at least one batch. Re-run it by hand to find\n'
                printf '       out why before publishing:\n'
                printf '         scripts/secret-scan.sh $(git rev-parse --show-toplevel)/src/main.rs\n'
            else
                FINDINGS="$(python3 - "$HIST_DIR/.scan" "$HIST_DIR/.manifest" <<'PY'
import re
import sys

scan_path, manifest_path = sys.argv[1], sys.argv[2]

# The scanner has two kinds of pattern and only one of them is evidence.
# A shaped credential (an AWS key id, a PEM header, a GitHub token) is a
# credential by its form. generic-secret-assignment is a heuristic: a
# secret-ish NAME assigned a long value. That heuristic pays for itself
# on a single file, but over 2500 commits it also matches a constant
# whose VALUE is an environment-variable name rather than a secret, and
# the fixtures in tests/secret_scan.rs and tests/operations_scripts.rs,
# which exist precisely to exercise the scanner. Calling those blockers
# is how a gate earns a reputation for crying wolf and gets switched
# off, so shaped hits block and heuristic hits advise.
SHAPED = {"aws-access-key-id", "aws-secret-access-key", "github-token",
          "private-key-block"}

# fragment index -> (commit, path, first real line number)
origin = {}
for line in open(manifest_path, encoding="utf-8"):
    parts = line.rstrip("\n").split("\t")
    if len(parts) == 4:
        origin[parts[0]] = (parts[1], parts[2], int(parts[3]))

# (commit, path, kind) -> set of real line numbers
groups = {}
for line in open(scan_path, encoding="utf-8", errors="replace"):
    m = re.match(r"^\s+(\S+?):(\d+):\s+(\S+)\s*$", line.rstrip())
    if not m:
        continue
    frag, offset, kind = m.group(1), int(m.group(2)), m.group(3)
    entry = origin.get(frag.rsplit("/", 1)[-1])
    if entry is None:
        continue
    commit, path, first = entry
    groups.setdefault((commit, path, kind), set()).add(first + offset - 1)

for (commit, path, kind), lines in sorted(groups.items()):
    shown = ", ".join(str(n) for n in sorted(lines)[:4])
    if len(lines) > 4:
        shown += f", +{len(lines) - 4} more"
    tier = "block" if kind in SHAPED else "advise"
    print(f"{tier}\t{commit[:12]}\t{path}\t{kind}\t{shown}")
PY
)"

                if [ -z "$FINDINGS" ]; then
                    ok "history is clean of credential-shaped material"
                else
                    BLOCK_ROWS="$(grep -c '^block' <<< "$FINDINGS" || true)"
                    ADVISE_ROWS="$(grep -c '^advise' <<< "$FINDINGS" || true)"

                    if [ "$BLOCK_ROWS" -gt 0 ]; then
                        block "credential-FORMED material in the history: $BLOCK_ROWS commit/path group(s)"
                        while IFS=$'\t' read -r tier csha cpath kind lines; do
                            [ "$tier" = "block" ] || continue
                            printf '       %s %s:%s  (%s)\n' "$csha" "$cpath" "$lines" "$kind"
                        done <<< "$FINDINGS"
                        printf '       These match a credential shape, not just a secret-ish name.\n'
                        printf '       Inspect with: git show <sha>:<path>\n'
                    else
                        ok "no credential-FORMED material in the history"
                    fi

                    if [ "$ADVISE_ROWS" -gt 0 ]; then
                        advise "$ADVISE_ROWS historical name-based match(es) -- review, usually fixtures"
                        while IFS=$'\t' read -r tier csha cpath kind lines; do
                            [ "$tier" = "advise" ] || continue
                            printf '       %s %s:%s  (%s)\n' "$csha" "$cpath" "$lines" "$kind"
                        done <<< "$FINDINGS"
                        printf '       This repo tests its own secret scanner, so a fixture-bearing test\n'
                        printf '       file is the likeliest innocent explanation. Check before treating\n'
                        printf '       one as a real leak.\n'
                    fi
                    printf '       Values are withheld on purpose. A credential that was ever real must\n'
                    printf '       be rotated even if the commit is later rewritten out of the history.\n'
                fi
            fi
        fi
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

raw = subprocess.run(["git", "ls-files", "-z"], capture_output=True,
                     check=False).stdout
files = [f.decode("utf-8", "replace") for f in raw.split(b"\0") if f]

HOME = re.compile(r"/home/[A-Za-z0-9._-]+")
EMAIL = re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
IP = re.compile(r"\b(?:(?:25[0-5]|2[0-4][0-9]|1?[0-9]{1,2})\.){3}"
                r"(?:25[0-5]|2[0-4][0-9]|1?[0-9]{1,2})\b")
ACCOUNT = re.compile(r"\baccount\s+\d{9,12}\b")

# Documented examples: naming them teaches nothing about the operator.
IP_ALLOW = {"0.0.0.0", "127.0.0.1", "127.0.0.2", "10.0.0.1",
            "10.66.0.1", "192.0.2.1", "255.255.255.255"}
# Role mailboxes and forge system addresses, not a person's address.
EMAIL_ALLOW_DOMAIN = {"github.com", "gitlab.com", "noreply.github.com",
                     # RFC 2606 reserved example domains, used in fixtures.
                     "example.com", "example.net", "example.org"}
EMAIL_ALLOW_LOCAL = {"git", "noreply", "support", "info", "admin",
                     "postmaster", "hostmaster", "webmaster", "security",
                     "users", "example", "test", "nobody"}
# RFC 2606 / systemd-style reserved domains carry no identity.
EMAIL_ALLOW_TLD = {".invalid", ".example", ".test", ".service", ".local"}

# Frozen records are never edited after the fact (docs/README.md), so a
# hit there is a publication decision, not a doc to fix. Everything else
# is a live document, where the fix is to remove the path.
FROZEN = ("docs/evidence/", "docs/verification/")

hits = {}
for rel in files:
    try:
        with open(rel, "rb") as fh:
            blob = fh.read()
    except OSError:
        continue
    if b"\0" in blob[:4096]:
        continue  # binary
    try:
        text = blob.decode("utf-8")
    except UnicodeDecodeError:
        continue
    bucket = "frozen" if rel.startswith(FROZEN) else "live"

    def note(kind, value):
        hits.setdefault(kind, {}).setdefault(bucket, {}).setdefault(rel, 0)
        hits[kind][bucket][rel] += 1

    for m in HOME.finditer(text):
        note("home-path", m.group(0))
    for m in EMAIL.finditer(text):
        value = m.group(0)
        local, _, domain = value.rpartition("@")
        if domain.lower() in EMAIL_ALLOW_DOMAIN:
            continue
        if local.lower() in EMAIL_ALLOW_LOCAL:
            continue
        if any(domain.lower().endswith(tld) for tld in EMAIL_ALLOW_TLD):
            continue
        note("personal-email", value)
    for m in IP.finditer(text):
        if m.group(0) not in IP_ALLOW:
            note("public-ip", m.group(0))
    for _ in ACCOUNT.finditer(text):
        note("cloud-account", "")

for kind in ("home-path", "personal-email", "public-ip", "cloud-account"):
    for bucket in ("live", "frozen"):
        files_hit = hits.get(kind, {}).get(bucket, {})
        if not files_hit:
            continue
        total = sum(files_hit.values())
        print(f"{kind}\t{bucket}\t{total}\t{len(files_hit)}")
PY
)"

if [ -n "$ADVISORY_OUT" ]; then
    while IFS=$'\t' read -r kind bucket total files_hit; do
        [ -n "$kind" ] || continue
        if [ "$bucket" = "live" ]; then
            advise "$kind appears in $total place(s) across $files_hit live doc(s) -- editable, so this one is a choice"
        else
            advise "$kind appears in $total place(s) across $files_hit frozen record(s) -- never edited after the fact, so publishing it is the decision"
        fi
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
