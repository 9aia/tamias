#!/usr/bin/env bash
# triage.sh — campaign `triage` node. Registers each attack finding in
# tamias's project ledger: `myco issue new` (the bug record) + a matching
# `myco todo add` (the work item). Idempotent — a finding whose title
# already matches an open issue is counted, not refiled.
#
# Reads:  ${FINDINGS:-target/adversarial/findings.ndjson}
# Writes: ${ISSUES:-target/adversarial/issues.txt}  — "I-NNN<TAB>title"
#
# Exit: 0 = all findings registered, 1 = findings file missing/empty
# or a myco verb failed, 2 = myco unavailable.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"   # myco discovers the project/ worktree from the repo root

FINDINGS="${FINDINGS:-$ROOT/target/adversarial/findings.ndjson}"
ISSUES="${ISSUES:-$ROOT/target/adversarial/issues.txt}"

command -v myco >/dev/null || { echo "triage.sh: myco not on PATH" >&2; exit 2; }
[ -s "$FINDINGS" ] || { echo "triage.sh: no findings at $FINDINGS" >&2; exit 1; }

: > "$ISSUES"
filed=0; dup=0; failed=0

field() { # extract "key":"value" from an ndjson line
    sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p" <<< "$2" | head -1
}

while IFS= read -r line; do
    [ -z "$line" ] && continue
    check=$(field check "$line"); sev=$(field severity "$line")
    title=$(field title "$line"); detail=$(field detail "$line")
    [ -n "$title" ] || continue
    case "$sev" in crit|high|med|low) ;; *) sev=med ;; esac

    # dedup — don't refile an already-open issue with the same title
    if myco issue list 2>/dev/null | grep -qF "$title"; then
        echo "triage.sh: dup (open issue exists): $title" >&2
        dup=$((dup + 1))
        continue
    fi

    iid=$(myco issue new "adversarial [$check] $title" \
            --pri "$sev" --label adversarial --label campaign \
            --owner peregrin --text "$detail" 2>/dev/null \
          | grep -oE 'I-[0-9]+' | head -1)
    if [ -z "$iid" ]; then
        echo "triage.sh: myco issue new failed for: $title" >&2
        failed=$((failed + 1)); continue
    fi
    myco todo add "fix $iid: $title" --pri "$sev" --label adversarial \
        --owner peregrin >/dev/null 2>&1 \
        || echo "triage.sh: todo add failed for $iid (continuing)" >&2
    printf '%s\t%s\n' "$iid" "$title" >> "$ISSUES"
    filed=$((filed + 1))
done < "$FINDINGS"

echo "triage.sh: $filed filed, $dup dup, $failed failed → $ISSUES" >&2
[ "$failed" -eq 0 ] && { [ "$filed" -gt 0 ] || [ "$dup" -gt 0 ]; } && exit 0
exit 1
