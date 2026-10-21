#!/usr/bin/env bash
# Render a deployment unit template with explicit values.
#
# Usage: scripts/unit-render.sh <template|-> NAME=VALUE [NAME=VALUE ...]
#
#   <template>   path to the unit template, or "-" to read it from stdin
#   NAME=VALUE   the value for one {{PLACEHOLDER}}; may be repeated
#
# Prints the rendered unit on stdout. Strict by construction: a
# placeholder that no value covers is a hard error that names it. A
# literal "{{PLACEHOLDER}}" is never left in the output.
#
# Why strict. The drift gate (scripts/host-audit.sh) compares the unit
# installed on a host against the repo template rendered with the
# reference-host values. If a template gains a placeholder and the
# render leaves it literal, the expected hash is computed over text
# that no correctly rendered host can have, so the gate reports a
# permanent false drift on every healthy host -- and a permanently red
# gate cannot find the real drift it exists to detect. The same silent
# substitution would let a deploy gate verify a unit that still
# contains template text. A loud failure at render time is the honest
# outcome: the caller has to say what the host's value for the new
# placeholder is.
#
# The renderer knows nothing about the host layout: the values are the
# caller's to state, so the same renderer serves the drift check (the
# reference-host layout) and the deploy gate (a local test layout).
#
# Run: scripts/unit-render.sh deploy/dg.service DG_USER=dg DG_LISTEN=127.0.0.1:7432

set -euo pipefail

fail() { echo "[unit-render] FAIL: $*" >&2; exit 1; }

[ "$#" -ge 1 ] || fail "usage: unit-render.sh <template|-> NAME=VALUE ..."

template="$1"
shift

if [ "$template" = "-" ]; then
    # Command substitution strips trailing newlines, and the trailing
    # byte matters to a byte-exact comparison, so read it back.
    content="$(cat; printf 'x')"
    content="${content%x}"
    label="<stdin>"
else
    [ -f "$template" ] || fail "no such template: $template"
    content="$(cat "$template"; printf 'x')"
    content="${content%x}"
    label="$template"
fi

for kv in "$@"; do
    case "$kv" in
        [A-Z_][A-Z0-9_]*=*) ;;
        *) fail "expected NAME=VALUE, got: $kv" ;;
    esac
done

# Substitute each named placeholder. Bash parameter expansion, not sed:
# the value may hold any character, and a sed replacement would have to
# escape it.
for kv in "$@"; do
    name="${kv%%=*}"
    value="${kv#*=}"
    content="${content//\{\{$name\}\}/$value}"
done

leftover="$(printf '%s\n' "$content" | grep -o '{{[A-Z_][A-Z0-9_]*}}' | sort -u | tr '\n' ' ' || true)"
[ -z "$leftover" ] || fail "$label has placeholders with no value: $leftover"

printf '%s' "$content"
