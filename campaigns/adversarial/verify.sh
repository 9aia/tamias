#!/usr/bin/env bash
# verify.sh — campaign `verify` node body. Deterministic: lint gate +
# re-run of the attack battery against the (possibly patched) tree.
#
#   1. cargo xtask check    — fmt + clippy -D warnings must stay green
#   2. attack.sh            — the same battery must produce 0 findings
#
# Exit: 0 = green (issues may close), 1 = red (issues reopen → fix),
#       2 = infra error.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

echo "verify.sh: cargo xtask check…" >&2
if ! cargo xtask check; then
    echo "verify.sh: xtask check red" >&2
    exit 1
fi

echo "verify.sh: re-running attack battery…" >&2
bash campaigns/adversarial/attack.sh
rc=$?
[ "$rc" -eq 0 ] && exit 0
[ "$rc" -eq 1 ] && exit 1
exit 2
