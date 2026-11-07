#!/usr/bin/env bash
# close.sh — campaign `close` node. Marks the issues triage registered
# resolved after verify went green. Missing/stale ids are skipped, not
# fatal — a human can always re-open.
#
# Reads: ${ISSUES:-target/adversarial/issues.txt}
# Exit: 0 always (closing is best-effort bookkeeping).

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

ISSUES="${ISSUES:-$ROOT/target/adversarial/issues.txt}"
[ -f "$ISSUES" ] || { echo "close.sh: no issues file — nothing to close" >&2; exit 0; }

while IFS=$'\t' read -r iid _title; do
    [ -n "$iid" ] || continue
    myco issue resolve "$iid" \
        --resolution "verified green by the peregrin adversarial campaign" \
        >/dev/null 2>&1 \
        && echo "close.sh: resolved $iid" >&2 \
        || echo "close.sh: could not resolve $iid (skipped)" >&2
done < "$ISSUES"
exit 0
